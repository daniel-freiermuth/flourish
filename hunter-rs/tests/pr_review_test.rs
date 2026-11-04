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
    FindingInsert, HEAD_STABLE_MS, NewPrReview, PR_REVIEW_HOLD_AT, PrReviewRound, Store,
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

const ISSUES: &str = r#"[{"kind":"introduced","blocking":true,"file":"CHANGE.md","line":1,"severity":"high","confidence":0.9,"summary":"breaks the widget","detail":"CHANGE.md:1"},{"kind":"follow-up","blocking":false,"summary":"old debt"}]"#;

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
        summary: summary.into(),
        detail: String::new(),
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
