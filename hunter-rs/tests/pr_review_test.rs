#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Reviews of PRs hunter did not open (migration 034, future.md §8.8):
//! when a round is due, where the tier sits, what a round records, how a
//! failing review is held and released, the request's trust check, and the
//! PR sync's refresh.
//!
//! Executor tests use the `runner_harvest_test` scaffolding: a real repo
//! with a bare origin (the PR head pushed as `refs/pull/7/head`), a scripted
//! `gh`, and a backend standing in for the reviewer.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::config::Config;
use hunter::domain::{ForgeName, PrJobKind, PrReviewState};
use hunter::scheduler::{Candidate, pick_next, run_cycle};
use hunter::server::{AppState, router};
use hunter::store::{
    FindingInsert, HEAD_STABLE_MS, NewPrReview, PR_REVIEW_HOLD_AT, PostDecided, PostDecision,
    PrReviewRound, Store,
};
use serde_json::{Value, json};
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, fresh_store, git};
use tower::util::ServiceExt;

const REPO_URL: &str = "https://github.com/acme/widget";

struct Fixture {
    cfg: Config,
    store: Store,
    db: PathBuf,
    rid: i64,
    /// The repo's working clone (the daemon's clone of `origin`).
    origin_work: PathBuf,
    /// Last: fields drop in declaration order, and the directory must
    /// outlive the Store's pool.
    dir: TempDir,
}

impl Fixture {
    /// A writable side connection, for columns no Store method writes.
    async fn pool(&self) -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db))
            .await
            .unwrap()
    }
}

/// Repo "widget" (GitHub, `REPO_URL`) with a real clone whose origin
/// carries `main` and branch `feature` one commit ahead, and a hermetic
/// `pr_review.md` naming the slots these tests assert on.
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    let (db, store) = fresh_store(&dir, "hunter").await;
    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    std::fs::rename(&repo.work, Store::repo_dir(&repos_root, rid)).unwrap();
    // A second clone to author the PR's commits from.
    let origin_work = dir.join("author");
    git(
        dir.path(),
        &[
            "clone",
            repo.origin.to_string_lossy().as_ref(),
            origin_work.to_string_lossy().as_ref(),
        ],
    );
    git(&origin_work, &["checkout", "feature"]);
    git(
        &origin_work,
        &["push", "origin", "feature:refs/pull/7/head"],
    );
    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("pr_review.md"),
        "base={{BASE}}\nearlier={{EARLIER_ISSUES}}\nout={{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        db,
        rid,
        origin_work,
        dir,
    }
}

fn rev(cwd: &Path, rev: &str) -> String {
    git(cwd, &["rev-parse", rev]).trim().to_owned()
}

/// Push one more commit to the PR's head; its sha.
fn push_to_pr(f: &Fixture, file: &str) -> String {
    std::fs::write(f.origin_work.join(file), "more\n").unwrap();
    git(&f.origin_work, &["add", "-A"]);
    git(&f.origin_work, &["commit", "-m", file]);
    git(
        &f.origin_work,
        &["push", "origin", "HEAD:refs/pull/7/head", "-f"],
    );
    rev(&f.origin_work, "HEAD")
}

/// `gh` answering `pr view` from `view.json` in the fixture (rewritten by
/// [`set_view`]) and the push-permission lookup with `push`.
fn gh(bins: &FakeBins, f: &Fixture, push: bool) {
    let view = f.dir.join("view.json");
    bins.script(
        "gh",
        &format!(
            "if [ \"$1\" = api ]; then echo {push}; exit 0; fi\ncat '{}'\nexit 0",
            view.display()
        ),
    );
}

fn set_view(f: &Fixture, state: &str, head: &str, author: &str) {
    let view = json!({
        "state": state,
        "headRefOid": head,
        "headRefName": "feature",
        "baseRefName": "main",
        "author": { "login": author },
        "title": "make the widget faster",
        "body": "it is faster now",
        "isCrossRepository": true,
    });
    std::fs::write(f.dir.join("view.json"), view.to_string()).unwrap();
}

async fn request(f: &Fixture, head: &str, watch: bool) -> i64 {
    f.store
        .request_pr_review(&NewPrReview {
            repo_id: f.rid,
            pr_number: 7,
            title: "make the widget faster",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: head,
            watch,
            auto_post: false,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap()
}

fn due_ids(rows: &[hunter::store::PrReviewRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

/// The first round is due at once; afterwards only a watched review with a
/// new head that has been stable for `HEAD_STABLE_MS` is; a held review
/// never is until it is retried.
#[tokio::test]
async fn due_reviews_follow_watch_head_stability_and_hold() {
    let f = fixture("prr-due").await;
    let now = hunter::util::now_ms();
    let id = request(&f, "aaa", false).await;
    let due = |at: i64| {
        let store = f.store.clone();
        async move { due_ids(&store.list_due_pr_reviews(at).await.unwrap()) }
    };
    assert_eq!(
        due(now).await,
        vec![id],
        "the first round waits for nothing"
    );

    let scope = support::scope(&f.store);
    let jobs = scope.store();
    jobs.record_pr_review_round(id, None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    assert!(due(now).await.is_empty(), "reviewed head: nothing due");

    jobs.observe_pr_review(id, "bbb", "t", PrReviewState::Open)
        .await
        .unwrap();
    let later = hunter::util::now_ms() + HEAD_STABLE_MS;
    assert!(
        due(later).await.is_empty(),
        "unwatched: a new head is not reviewed"
    );

    assert!(f.store.set_pr_review_watch(id, true).await.unwrap());
    assert!(
        due(hunter::util::now_ms()).await.is_empty(),
        "watched, but the new head has just been seen"
    );
    assert_eq!(due(later).await, vec![id], "stable for the window: due");

    for _ in 0..PR_REVIEW_HOLD_AT {
        jobs.record_pr_review_round(id, None, ("aaa", "bbb"), PrReviewRound::Failed("boom"))
            .await
            .unwrap();
    }
    let held = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(held.held, "{held:?}");
    assert_eq!(held.last_failure.as_deref(), Some("boom"));
    assert!(due(later).await.is_empty(), "held: never due");

    assert!(f.store.retry_pr_review(id).await.unwrap());
    let retried = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(!retried.held && retried.failures == 0, "{retried:?}");
    assert_eq!(due(later).await, vec![id]);
}

/// A finding of the fixture's repo with hunter's PR #`n` open.
async fn pr_finding(f: &Fixture, fingerprint: &str, n: i64) -> i64 {
    let (fid, _) = f
        .store
        .upsert_finding(
            f.rid,
            &FindingInsert {
                fingerprint: fingerprint.to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: fingerprint.to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    let url = format!("https://github.com/acme/widget/pull/{n}");
    f.store.set_finding_pr_open(fid, &url).await.unwrap();
    fid
}

/// A due review ranks below a flagged PR's engage and above a pending
/// harvest.
#[tokio::test]
async fn a_due_review_ranks_between_engage_and_harvest() {
    let f = fixture("prr-tier").await;
    let engage = pr_finding(&f, "w:a:engage", 1).await;
    let harvest = pr_finding(&f, "w:b:harvest", 2).await;
    f.store
        .set_finding_verdict(harvest, hunter::domain::FindingStatus::Closed, "closed")
        .await
        .unwrap();
    f.store.mark_pr_closed(harvest, 2, 1).await.unwrap();
    let pool = f.pool().await;
    sqlx::query("DELETE FROM pr_state WHERE finding_id = ?1")
        .bind(engage)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, needs_attention, \
         attention_since, synced_at) VALUES (?1, 1, 'OPEN', 'review_comments', 1, 5000)",
    )
    .bind(engage)
    .execute(&pool)
    .await
    .unwrap();
    let review = request(&f, "aaa", true).await;

    let pick = || async {
        pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    };
    match pick().await {
        Some(Candidate::Finding {
            kind, finding_id, ..
        }) => {
            assert_eq!(kind, hunter::domain::FindingJobKind::Engage);
            assert_eq!(finding_id, engage);
        }
        other => panic!("expected the flagged engage first, got {other:?}"),
    }

    sqlx::query("UPDATE pr_state SET needs_attention = NULL WHERE finding_id = ?1")
        .bind(engage)
        .execute(&pool)
        .await
        .unwrap();
    match pick().await {
        Some(Candidate::Pr {
            kind,
            review_id,
            label,
            ..
        }) => {
            assert_eq!(kind, PrJobKind::Review);
            assert_eq!(review_id, review);
            assert_eq!(label.as_deref(), Some("PR #7 make the widget faster"));
        }
        other => panic!("expected the review before the harvest, got {other:?}"),
    }

    let preview = hunter::scheduler::preview_next(&f.store, &f.cfg)
        .await
        .unwrap();
    assert!(
        matches!(preview, Some(Candidate::Pr { .. })),
        "the preview shows it too: {preview:?}"
    );

    f.store.set_pr_review_watch(review, false).await.unwrap();
    let jobs = support::scope(&f.store);
    jobs.store()
        .record_pr_review_round(review, None, ("x", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    match pick().await {
        Some(Candidate::Finding {
            kind, finding_id, ..
        }) => {
            assert_eq!(kind, hunter::domain::FindingJobKind::Harvest);
            assert_eq!(finding_id, harvest);
        }
        other => panic!("expected the harvest once the review is done, got {other:?}"),
    }
}

const ISSUES: &str = r#"[{"kind":"introduced","blocking":true,"file":"CHANGE.md","line":1,"severity":"high","confidence":0.9,"summary":"breaks the widget","detail":"CHANGE.md:1"},{"kind":"follow-up","blocking":false,"file":"OLD.md","line":3,"side":"left","summary":"old debt"}]"#;

/// A reviewer that reports `ISSUES` where the playbook names its output.
fn reporting() -> ScriptedBackend {
    ScriptedBackend::seeing(|run| {
        std::fs::write(run.tree.parent().unwrap().join("pr-review.json"), ISSUES).unwrap();
        support::done()
    })
}

/// A first round reviews from the merge-base with the base branch and
/// records the reviewer's issues; once the author pushes again, the next
/// round reviews only from the head the first one reviewed, and is shown
/// what it reported.
#[tokio::test]
async fn rounds_review_from_the_merge_base_then_incrementally() {
    let bins = FakeBins::acquire("prr-rounds");
    let f = fixture("prr-rounds").await;
    gh(&bins, &f, true);
    let first_head = rev(&f.origin_work, "HEAD");
    let main = rev(&f.origin_work, "origin/main");
    set_view(&f, "OPEN", &first_head, "outsider");
    let id = request(&f, &first_head, true).await;
    let backend = reporting();

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");
    let job = summary.job_id.unwrap();
    assert_eq!(
        f.store.base_sha(job).await.unwrap().as_deref(),
        Some(main.as_str())
    );
    assert!(backend.runs()[0].prompt.contains(&format!("base={main}")));
    assert!(backend.runs()[0].prompt.contains("earlier=[]"));
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        review.last_reviewed_sha.as_deref(),
        Some(first_head.as_str())
    );
    let rounds = f.store.list_pr_review_rounds().await.unwrap();
    assert_eq!(rounds.len(), 1);
    let (_, round) = &rounds[0];
    assert_eq!((round.verdict.as_str(), round.job_id), ("ok", Some(job)));
    assert_eq!(round.issues.len(), 2);
    assert!(round.issues[0].blocking && round.issues[0].summary == "breaks the widget");
    // The side as the forge spells it; none given reads as the head's.
    let sides: Vec<_> = round.issues.iter().map(|i| i.side.as_deref()).collect();
    assert_eq!(sides, [None, Some("LEFT")]);
    assert_eq!(round.post_state.as_deref(), Some("pending"));

    let second_head = push_to_pr(&f, "MORE.md");
    set_view(&f, "OPEN", &second_head, "outsider");
    support::scope(&f.store)
        .store()
        .observe_pr_review(id, &second_head, "t", PrReviewState::Open)
        .await
        .unwrap();
    // The new head has been stable for the whole window.
    sqlx::query("UPDATE pr_reviews SET head_seen_at = 0 WHERE id = ?1")
        .bind(id)
        .execute(&f.pool().await)
        .await
        .unwrap();

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");
    let job = summary.job_id.unwrap();
    assert_eq!(
        f.store.base_sha(job).await.unwrap().as_deref(),
        Some(first_head.as_str())
    );
    let prompt = &backend.runs()[1].prompt;
    assert!(prompt.contains(&format!("base={first_head}")), "{prompt}");
    assert!(
        prompt.contains("breaks the widget"),
        "earlier issues shown: {prompt}"
    );
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        review.last_reviewed_sha.as_deref(),
        Some(second_head.as_str())
    );
}

/// What an incremental round is told was already reported spans the whole
/// chain since the last review from scratch: a round that found nothing new
/// does not drop the issues before it, and a round that started over (after
/// a force-push its base is no earlier head) starts a new chain.
#[tokio::test]
async fn earlier_issues_span_the_incremental_chain() {
    let f = fixture("prr-chain").await;
    let id = request(&f, "aaa", true).await;
    let issue = |summary: &str| hunter::types::PrReviewIssueOut {
        kind: "introduced".into(),
        blocking: true,
        severity: None,
        confidence: None,
        file: None,
        line: None,
        side: None,
        summary: summary.into(),
        detail: String::new(),
        posted_inline: false,
    };
    let jobs = support::scope(&f.store);
    let store = jobs.store();
    let summaries = || async {
        store
            .chain_pr_review_issues(id)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.summary)
            .collect::<Vec<_>>()
    };
    let round = |range: (&'static str, &'static str), issues: Vec<_>| async move {
        store
            .record_pr_review_round(id, None, range, PrReviewRound::Ok(&issues))
            .await
            .unwrap();
    };

    round(("base", "aaa"), vec![issue("first")]).await;
    round(("aaa", "bbb"), vec![]).await;
    store
        .record_pr_review_round(
            id,
            None,
            ("bbb", "ccc"),
            PrReviewRound::Failed("worker killed"),
        )
        .await
        .unwrap();
    assert_eq!(
        summaries().await,
        ["first"],
        "an empty round keeps the chain"
    );
    round(("bbb", "ccc"), vec![issue("third")]).await;
    assert_eq!(summaries().await, ["first", "third"]);

    round(("base", "ddd"), vec![issue("after force-push")]).await;
    assert_eq!(summaries().await, ["after force-push"]);
}

/// A reviewer that leaves no review fails its round; the third failure in
/// a row holds the review, which is then not picked until retried.
#[tokio::test]
async fn three_failed_rounds_hold_the_review_until_retried() {
    let bins = FakeBins::acquire("prr-hold");
    let f = fixture("prr-hold").await;
    gh(&bins, &f, true);
    let head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &head, "outsider");
    let id = request(&f, &head, true).await;

    for n in 1..=PR_REVIEW_HOLD_AT {
        let summary = run_cycle(
            &mut support::jobs(&f.store),
            &f.cfg,
            &ScriptedBackend::noop(),
            None,
        )
        .await;
        assert_eq!(
            summary.kind.map(|k| k.to_string()).as_deref(),
            Some("pr_review"),
            "{summary:?}"
        );
        let expected = if n == PR_REVIEW_HOLD_AT {
            "held"
        } else {
            "retry"
        };
        assert_eq!(summary.outcome.as_deref(), Some(expected), "{summary:?}");
    }
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(review.held, "{review:?}");
    assert_eq!(review.failures, PR_REVIEW_HOLD_AT);
    assert!(review.last_failure.unwrap().contains("no review file"));
    assert!(review.last_reviewed_sha.is_none());

    let pick = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(!matches!(pick, Some(Candidate::Pr { .. })), "{pick:?}");

    f.store.retry_pr_review(id).await.unwrap();
    let pick = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(
        matches!(pick, Some(Candidate::Pr { review_id, .. }) if review_id == id),
        "{pick:?}"
    );
}

/// The PR sync records a merged PR's end: the review is over and no round
/// is due any more.
#[tokio::test]
async fn the_sync_ends_a_review_whose_pr_merged() {
    let bins = FakeBins::acquire("prr-sync");
    let f = fixture("prr-sync").await;
    gh(&bins, &f, true);
    let id = request(&f, "aaa", true).await;
    set_view(&f, "MERGED", "bbb", "outsider");

    let admin = support::admin(&f.store);
    let read = hunter::scheduler::sync_pr_reviews(admin.store(), || false).await;

    assert_eq!(read, 1);
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.state, PrReviewState::Merged);
    assert_eq!(review.head_sha.as_deref(), Some("bbb"));
    assert!(
        f.store
            .list_due_pr_reviews(hunter::util::now_ms())
            .await
            .unwrap()
            .is_empty()
    );
}

fn app_state(f: &Fixture) -> AppState {
    AppState {
        store: Arc::new(f.store.clone()),
        config: Arc::new(f.cfg.clone()),
        backend: Arc::new(hunter::backend::NullBackend),
        repo_notes: Arc::new(tokio::sync::Mutex::new(())),
        scheduler: hunter::server::SchedulerHandle::default(),
    }
}

async fn post(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
    let cookie = support::auth_cookie(&state.store).await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header(axum::http::header::COOKIE, cookie.as_str())
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// An author who cannot push needs the requester's vouch: without it the
/// request answers 409 `needs_vouch` and records nothing; with it the
/// review is recorded with the vouched author. A trusted author needs none.
#[tokio::test]
async fn requesting_a_review_of_an_untrusted_author_needs_a_vouch() {
    let bins = FakeBins::acquire("prr-vouch");
    let f = fixture("prr-vouch").await;
    gh(&bins, &f, false);
    set_view(&f, "OPEN", "aaa", "outsider");
    let state = app_state(&f);
    let url = "https://github.com/acme/widget/pull/7";

    let (status, body) = post(
        &state,
        "/api/pr-review",
        json!({ "repo_id": f.rid, "pr": url, "watch": true }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["needs_vouch"], json!(true));
    assert_eq!(body["author"], json!("outsider"));
    assert_eq!(body["cross_repo"], json!(true));
    assert!(f.store.list_pr_reviews().await.unwrap().is_empty());

    let (status, body) = post(
        &state,
        "/api/pr-review",
        json!({ "repo_id": f.rid, "pr": url, "watch": true, "vouch": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["id"].as_i64().unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.vouched_author.as_deref(), Some("outsider"));
    assert_eq!(review.requested_by, "tester");
    assert_eq!(review.head_sha.as_deref(), Some("aaa"));

    // A trusted author: PR #8 by someone who can push.
    gh(&bins, &f, true);
    set_view(&f, "OPEN", "ccc", "maintainer");
    let (status, body) = post(
        &state,
        "/api/pr-review",
        json!({ "repo_id": f.rid, "pr": "8", "watch": false }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let review = f
        .store
        .get_pr_review(body["id"].as_i64().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(review.vouched_author, None);
    assert!(!review.watch);
}

/// A PR hunter opened for a finding is its own work, never reviewed.
#[tokio::test]
async fn hunters_own_pr_cannot_be_reviewed() {
    let f = fixture("prr-own").await;
    let fid = pr_finding(&f, "w:own", 7).await;
    f.store.mark_pr_closed(fid, 7, 1).await.unwrap();
    let state = app_state(&f);

    let (status, body) = post(
        &state,
        "/api/pr-review",
        json!({ "repo_id": f.rid, "pr": 7, "watch": true }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body.get("needs_vouch"), None);
    assert!(f.store.list_pr_reviews().await.unwrap().is_empty());
}

// -- Posting (slice 2) -------------------------------------------------------

/// The PR's diff as the forge returns it: `CHANGE.md` added with 2 lines.
const PR_DIFF: &str = "\
diff --git a/CHANGE.md b/CHANGE.md
new file mode 100644
--- /dev/null
+++ b/CHANGE.md
@@ -0,0 +1,2 @@
+change
+more
";

fn issue(summary: &str, file: Option<&str>, line: Option<i64>) -> hunter::types::PrReviewIssueOut {
    hunter::types::PrReviewIssueOut {
        kind: "introduced".into(),
        blocking: file.is_some(),
        severity: Some("high".into()),
        confidence: None,
        file: file.map(str::to_owned),
        line,
        side: None,
        summary: summary.into(),
        detail: format!("why {summary}"),
        posted_inline: false,
    }
}

/// Record an ok round of review `id` over `base..head` with `issues`; the
/// round's id.
async fn record_round(
    f: &Fixture,
    id: i64,
    range: (&str, &str),
    issues: &[hunter::types::PrReviewIssueOut],
) -> i64 {
    let jobs = support::scope(&f.store);
    jobs.store()
        .record_pr_review_round(id, None, range, PrReviewRound::Ok(issues))
        .await
        .unwrap();
    newest_round(f).await.id
}

async fn newest_round(f: &Fixture) -> hunter::types::PrReviewRoundOut {
    f.store.list_pr_review_rounds().await.unwrap().remove(0).1
}

/// `gh` for posting: `pr diff` answers [`PR_DIFF`]; the repo was renamed,
/// so the review POST only goes through at its current name (GitHub
/// answers a POST to the old one with a 307 that gh does not follow), and
/// it keeps its input in `posted.json` and answers review 99 (or fails,
/// with `refuse`); review 99's comments list the one on `CHANGE.md:1`.
fn gh_posting(bins: &FakeBins, f: &Fixture, refuse: bool) {
    let diff = f.dir.join("pr.diff");
    std::fs::write(&diff, PR_DIFF).unwrap();
    let posted = f.dir.join("posted.json");
    let review = if refuse {
        "echo 'HTTP 422: Unprocessable Entity' >&2; exit 1".to_owned()
    } else {
        format!("cat > '{}'; echo '{{\"id\":99}}'", posted.display())
    };
    bins.script(
        "gh",
        &format!(
            "case \"$*\" in\n\
             'pr diff'*) cat '{}';;\n\
             'api repos/acme/widget --jq .full_name') echo acme/widget-renamed;;\n\
             *'repos/acme/widget-renamed/pulls/7/reviews --input -'*) {review};;\n\
             *'pulls/7/comments'*'pull_request_review_id == 99'*) echo '{{\"node_id\":\"C1\",\"path\":\"CHANGE.md\",\"line\":1}}';;\n\
             *) echo \"unexpected gh $*\" >&2; exit 1;;\n\
             esac",
            diff.display()
        ),
    );
}

/// What the scripted forge was sent for review posts.
fn review_posts(bins: &FakeBins) -> usize {
    bins.calls_to("gh")
        .iter()
        .filter(|c| c.iter().any(|a| a.ends_with("/reviews")))
        .count()
}

/// A round waits for approval unless the review posts automatically; an
/// approved round goes out as one COMMENT review at the head it read, with
/// inline comments only for issues on a line of the PR's diff and the rest
/// in the marked body, and is then recorded as posted.
#[tokio::test]
async fn an_approved_round_posts_placeable_issues_inline_and_the_rest_in_the_body() {
    let bins = FakeBins::acquire("prr-post");
    let f = fixture("prr-post").await;
    gh_posting(&bins, &f, false);
    let head = "a".repeat(40);
    let id = request(&f, &head, true).await;
    let round = record_round(
        &f,
        id,
        ("b".repeat(40).as_str(), head.as_str()),
        &[
            issue("breaks the widget", Some("CHANGE.md"), Some(1)),
            issue("far away", Some("CHANGE.md"), Some(50)),
            issue("old debt", None, None),
        ],
    )
    .await;
    assert_eq!(
        newest_round(&f).await.post_state.as_deref(),
        Some("pending")
    );

    let admin = support::admin(&f.store);
    let posted = hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!((posted, review_posts(&bins)), (0, 0), "pending waits");

    let decided = f
        .store
        .decide_pr_review_post(round, PostDecision::Approve)
        .await
        .unwrap();
    assert_eq!(decided, PostDecided::Moved { review_id: id });
    let posted = hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!((posted, review_posts(&bins)), (1, 1));

    let sent: Value =
        serde_json::from_str(&std::fs::read_to_string(f.dir.join("posted.json")).unwrap()).unwrap();
    assert_eq!(sent["commit_id"], json!(head));
    assert_eq!(sent["event"], json!("COMMENT"));
    assert_eq!(
        sent["comments"],
        json!([{
            "path": "CHANGE.md",
            "line": 1,
            "side": "RIGHT",
            "body": "**high · introduced** · blocking: breaks the widget\n\nwhy breaks the widget",
        }])
    );
    let body = sent["body"].as_str().unwrap();
    assert!(
        body.starts_with(
            "**Automated review by hunter** (round of `bbbbbbb..aaaaaaa`): 3 issue(s)."
        ),
        "{body}"
    );
    assert!(
        body.contains("`CHANGE.md:50`") && body.contains("far away"),
        "{body}"
    );
    assert!(body.contains("old debt"), "{body}");
    assert!(!body.contains("breaks the widget"), "inline only: {body}");
    assert!(body.ends_with(hunter::forge::OWN_MARK), "{body}");

    let round = newest_round(&f).await;
    assert_eq!(round.post_state.as_deref(), Some("posted"));
    assert!(round.posted_at.is_some() && round.post_error.is_none());
    let inline: Vec<_> = round
        .issues
        .iter()
        .map(|i| (i.summary.as_str(), i.posted_inline))
        .collect();
    assert_eq!(
        inline,
        [
            ("breaks the widget", true),
            ("far away", false),
            ("old debt", false)
        ]
    );
    // Posted is final: the next sync sends nothing.
    hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!(review_posts(&bins), 1);
}

/// A round whose head the PR has moved past is posted with every issue in
/// the body: its lines number a diff the PR no longer has.
#[tokio::test]
async fn a_round_the_pr_moved_past_is_posted_in_the_body() {
    let bins = FakeBins::acquire("prr-moved");
    let f = fixture("prr-moved").await;
    gh_posting(&bins, &f, false);
    let id = request(&f, "newer", true).await;
    f.store.set_pr_review_auto_post(id, true).await.unwrap();
    record_round(
        &f,
        id,
        ("base", "older"),
        &[issue("breaks the widget", Some("CHANGE.md"), Some(1))],
    )
    .await;

    let admin = support::admin(&f.store);
    hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;

    let sent: Value =
        serde_json::from_str(&std::fs::read_to_string(f.dir.join("posted.json")).unwrap()).unwrap();
    assert_eq!(sent["commit_id"], json!("older"));
    assert_eq!(sent["comments"], json!([]));
    assert!(sent["body"].as_str().unwrap().contains("breaks the widget"));
    assert!(
        !bins
            .calls_to("gh")
            .iter()
            .any(|c| c.get(1).is_some_and(|a| a == "pr")),
        "no diff is read for a round that cannot go inline"
    );
}

/// A post the forge refuses stays approved with its error and is tried
/// again on the next sync; the third failure hands it back to a user as
/// pending, error kept. Approving it again clears the error.
#[tokio::test]
async fn three_refused_posts_return_the_round_to_pending() {
    let bins = FakeBins::acquire("prr-refused");
    let f = fixture("prr-refused").await;
    gh_posting(&bins, &f, true);
    let id = request(&f, "head", true).await;
    f.store.set_pr_review_auto_post(id, true).await.unwrap();
    let round = record_round(&f, id, ("base", "head"), &[issue("x", None, None)]).await;
    assert_eq!(
        newest_round(&f).await.post_state.as_deref(),
        Some("approved"),
        "auto-post approves at once"
    );

    let admin = support::admin(&f.store);
    for attempt in 1..=3 {
        let posted = hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
        assert_eq!(posted, 0);
        let r = newest_round(&f).await;
        let want = if attempt < 3 { "approved" } else { "pending" };
        assert_eq!(r.post_state.as_deref(), Some(want), "attempt {attempt}");
        assert!(r.post_error.as_deref().unwrap().contains("422"), "{r:?}");
    }
    // Pending now: the sync leaves it alone.
    hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!(review_posts(&bins), 3);

    f.store
        .decide_pr_review_post(round, PostDecision::Approve)
        .await
        .unwrap();
    let r = newest_round(&f).await;
    assert_eq!(
        (r.post_state.as_deref(), r.post_error),
        (Some("approved"), None)
    );
}

/// Only a round with issues has anything to post: one without issues and a
/// failed one have no post state at all.
#[tokio::test]
async fn only_a_round_with_issues_is_ever_posted() {
    let f = fixture("prr-nothing").await;
    let id = request(&f, "head", true).await;
    f.store.set_pr_review_auto_post(id, true).await.unwrap();

    record_round(&f, id, ("base", "head"), &[]).await;
    assert_eq!(newest_round(&f).await.post_state, None);
    support::scope(&f.store)
        .store()
        .record_pr_review_round(id, None, ("head", "next"), PrReviewRound::Failed("killed"))
        .await
        .unwrap();
    assert_eq!(newest_round(&f).await.post_state, None);
    assert!(
        f.store
            .approved_pr_review_rounds()
            .await
            .unwrap()
            .is_empty()
    );
}

/// The API moves only a pending round: approving or discarding anything
/// else is a 409, an unknown round a 404. The auto-post toggle is the
/// review's.
#[tokio::test]
async fn post_and_discard_move_only_pending_rounds() {
    let f = fixture("prr-api-post").await;
    let state = app_state(&f);
    let id = request(&f, "head", true).await;
    let first = record_round(&f, id, ("base", "head"), &[issue("x", None, None)]).await;
    let second = record_round(&f, id, ("head", "next"), &[issue("y", None, None)]).await;
    let empty = record_round(&f, id, ("next", "last"), &[]).await;

    let (status, body) = post(&state, "/api/pr-review/post", json!({ "round_id": first })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = post(&state, "/api/pr-review/post", json!({ "round_id": first })).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = post(
        &state,
        "/api/pr-review/discard",
        json!({ "round_id": first }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "an approved round is not discarded"
    );

    let (status, _) = post(
        &state,
        "/api/pr-review/discard",
        json!({ "round_id": second }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post(&state, "/api/pr-review/post", json!({ "round_id": second })).await;
    assert_eq!(status, StatusCode::CONFLICT, "discarded is final");

    let (status, _) = post(&state, "/api/pr-review/post", json!({ "round_id": empty })).await;
    assert_eq!(status, StatusCode::CONFLICT, "nothing to post");
    let (status, _) = post(&state, "/api/pr-review/post", json!({ "round_id": 9999 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let post_states: Vec<_> = f
        .store
        .list_pr_review_rounds()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, r)| (r.id, r.post_state))
        .collect();
    assert_eq!(
        post_states,
        [
            (empty, None),
            (second, Some("discarded".to_owned())),
            (first, Some("approved".to_owned()))
        ]
    );

    let (status, _) = post(
        &state,
        "/api/pr-review/auto-post",
        json!({ "id": id, "auto_post": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(f.store.get_pr_review(id).await.unwrap().unwrap().auto_post);
    let third = record_round(&f, id, ("last", "more"), &[issue("z", None, None)]).await;
    assert_eq!(newest_round(&f).await.id, third);
    assert_eq!(
        newest_round(&f).await.post_state.as_deref(),
        Some("approved")
    );
}

/// Approving is refused once the PR ended: only an open PR's rounds are
/// posted, so the round would wait forever.
#[tokio::test]
async fn a_round_of_an_ended_pr_is_not_approved() {
    let f = fixture("prr-ended").await;
    let id = request(&f, "head", true).await;
    let round = record_round(&f, id, ("base", "head"), &[issue("x", None, None)]).await;
    support::admin(&f.store)
        .store()
        .observe_pr_review(id, "head", "t", PrReviewState::Merged)
        .await
        .unwrap();

    let decided = f
        .store
        .decide_pr_review_post(round, PostDecision::Approve)
        .await
        .unwrap();
    assert!(matches!(decided, PostDecided::Refused(_)), "{decided:?}");
    let decided = f
        .store
        .decide_pr_review_post(round, PostDecision::Discard)
        .await
        .unwrap();
    assert_eq!(decided, PostDecided::Moved { review_id: id });
}

/// A GitLab review on its own repo: `gitlab.com/acme/widget`, MR !7.
async fn gitlab_review(f: &Fixture, head: &str) -> i64 {
    let repos_root = f.dir.subdir("gl-repos");
    let rid = f
        .store
        .add_repo(
            "gl-widget",
            "https://gitlab.com/acme/widget",
            &repos_root,
            "main",
            ForgeName::Gitlab,
        )
        .await
        .unwrap();
    f.store
        .request_pr_review(&NewPrReview {
            repo_id: rid,
            pr_number: 7,
            title: "t",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: head,
            watch: true,
            auto_post: true,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap()
}

/// `glab` for posting: the MR's `diff_refs` name `mr_head`, its diffs are
/// [`PR_DIFF`]'s, each draft note's input is appended to `drafts.jsonl`
/// and answered with the next id, and publishing succeeds.
fn glab_posting(bins: &FakeBins, f: &Fixture, mr_head: &str) {
    let drafts = f.dir.join("drafts.jsonl");
    let counter = f.dir.join("draft-id");
    let mr = json!({
        "diff_refs": { "base_sha": "B", "start_sha": "S", "head_sha": mr_head }
    });
    let diffs = json!([{
        "old_path": "CHANGE.md",
        "new_path": "CHANGE.md",
        "new_file": true,
        "diff": "@@ -0,0 +1,2 @@\n+change\n+more\n",
    }]);
    bins.script(
        "glab",
        &format!(
            "case \"$2\" in\n\
             */draft_notes/bulk_publish) echo '{{}}';;\n\
             */draft_notes) cat >> '{drafts}'; echo >> '{drafts}'; \
                n=$(( $(cat '{counter}' 2>/dev/null || echo 0) + 1 )); echo $n > '{counter}'; \
                echo \"{{\\\"id\\\":$n}}\";;\n\
             */diffs*) printf '%s\\n' '{diffs}';;\n\
             */merge_requests/7) printf '%s\\n' '{mr}';;\n\
             *) echo \"unexpected glab $*\" >&2; exit 1;;\n\
             esac",
            drafts = drafts.display(),
            counter = counter.display(),
        ),
    );
}

fn drafts(f: &Fixture) -> Vec<Value> {
    std::fs::read_to_string(f.dir.join("drafts.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// GitLab: each placeable issue becomes a positioned draft note on the
/// MR's current diff, the body a plain one, and the drafts are published
/// together.
#[tokio::test]
async fn gitlab_posts_draft_notes_then_publishes_them() {
    let bins = FakeBins::acquire("prr-gl");
    let f = fixture("prr-gl").await;
    glab_posting(&bins, &f, "head");
    let id = gitlab_review(&f, "head").await;
    record_round(
        &f,
        id,
        ("base", "head"),
        &[
            issue("breaks the widget", Some("CHANGE.md"), Some(2)),
            issue("old debt", None, None),
        ],
    )
    .await;

    let admin = support::admin(&f.store);
    let posted = hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!(posted, 1);

    let notes = drafts(&f);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(
        notes[0]["position"],
        json!({
            "position_type": "text",
            "base_sha": "B",
            "start_sha": "S",
            "head_sha": "head",
            "old_path": "CHANGE.md",
            "new_path": "CHANGE.md",
            "new_line": 2,
        })
    );
    assert!(notes[1].get("position").is_none());
    let body = notes[1]["note"].as_str().unwrap();
    assert!(
        body.contains("old debt") && body.ends_with(hunter::forge::OWN_MARK),
        "{body}"
    );
    let last = bins.calls_to("glab").pop().unwrap();
    assert!(last[2].ends_with("/draft_notes/bulk_publish"), "{last:?}");
    let round = newest_round(&f).await;
    assert_eq!(round.post_state.as_deref(), Some("posted"));
    assert!(round.issues.iter().any(|i| i.posted_inline));
}

/// GitLab positions name the MR's current version: when that is not the
/// head the round read, every issue goes into the body.
#[tokio::test]
async fn gitlab_posts_only_the_body_when_the_mr_moved_on() {
    let bins = FakeBins::acquire("prr-gl-moved");
    let f = fixture("prr-gl-moved").await;
    glab_posting(&bins, &f, "newer");
    let id = gitlab_review(&f, "head").await;
    record_round(
        &f,
        id,
        ("base", "head"),
        &[issue("breaks the widget", Some("CHANGE.md"), Some(2))],
    )
    .await;

    let admin = support::admin(&f.store);
    let posted = hunter::scheduler::post_pr_reviews(admin.store(), &f.cfg, || false).await;
    assert_eq!(posted, 1);

    let notes = drafts(&f);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].get("position").is_none());
    let body = notes[0]["note"].as_str().unwrap();
    assert!(
        body.contains("`CHANGE.md:2`") && body.contains("breaks the widget"),
        "{body}"
    );
    assert!(!newest_round(&f).await.issues[0].posted_inline);
}
