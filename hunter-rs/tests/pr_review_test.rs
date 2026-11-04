#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Reviews of PRs hunter did not open (migration 035, future.md §8.8):
//! when a round is due, where the tier sits, what a round records, how a
//! failing review is held and released, and the PR sync's refresh.
//!
//! Executor tests use the `runner_harvest_test` scaffolding: a real repo
//! with a bare origin (the PR head pushed as `refs/pull/7/head`), a scripted
//! `gh`, and a backend standing in for the reviewer.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hunter::config::Config;
use hunter::domain::{ForgeName, JobKind, PrJobKind, PrReviewState};
use hunter::scheduler::{Candidate, pick_next, run_cycle};
use hunter::store::{
    FindingInsert, HEAD_STABLE_MS, NewPrReview, PR_REVIEW_HOLD_AT, PrReviewRound, Store,
};
use serde_json::json;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";

/// The binding of a review no request bound to a new URL: its first.
const BINDING: i64 = 1;

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
            repo_url: REPO_URL,
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
/// new head that has been stable for `HEAD_STABLE_MS` is, and until then
/// that is the next due time the job lane sleeps toward; a held review
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
    jobs.record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    assert!(due(now).await.is_empty(), "reviewed head: nothing due");

    jobs.observe_pr_review((id, BINDING), "bbb", "t", PrReviewState::Open)
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
    let seen = f
        .store
        .get_pr_review(id)
        .await
        .unwrap()
        .unwrap()
        .head_seen_at;
    let next = |at: i64| {
        let store = f.store.clone();
        async move { store.next_pr_review_due_at(at).await.unwrap() }
    };
    assert_eq!(
        next(hunter::util::now_ms()).await,
        seen.map(|at| at + HEAD_STABLE_MS),
        "the next due time"
    );
    assert_eq!(next(later).await, None, "due already: the pick's");
    let settled = seen.unwrap() + HEAD_STABLE_MS;
    assert_eq!(due(settled).await, vec![id], "due the moment it settles");
    assert_eq!(next(settled).await, None, "due at that moment: the pick's");

    for _ in 0..PR_REVIEW_HOLD_AT {
        jobs.record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "bbb"),
            PrReviewRound::Failed("boom"),
        )
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
        .record_pr_review_round(
            (review, BINDING),
            None,
            ("x", "aaa"),
            PrReviewRound::Ok(&[]),
        )
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

/// A due review is picked only for its own repo when a pick is forced to
/// one, and never beside a round of its still running: the review shows
/// that job running, and the jobs list shows it as a review's.
#[tokio::test]
async fn a_review_is_picked_for_its_own_repo_and_never_beside_its_running_round() {
    let f = fixture("prr-busy").await;
    f.store
        .add_repo(
            "gadget",
            "https://github.com/acme/gadget",
            &f.dir.subdir("gadget-repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .unwrap();
    let id = request(&f, "aaa", true).await;
    let scope = support::scope(&f.store);

    let picked = pick_next(&scope, &f.cfg, Some("gadget")).await.unwrap();
    assert!(
        !matches!(picked, Some(Candidate::Pr { .. })),
        "another repo's review: {picked:?}"
    );
    let picked = pick_next(&scope, &f.cfg, Some("widget")).await.unwrap();
    assert!(
        matches!(picked, Some(Candidate::Pr { review_id, .. }) if review_id == id),
        "{picked:?}"
    );

    let created = scope
        .create_pr_review_job(
            hunter::store::JobStart::fresh_pr(PrJobKind::Review, id),
            "base",
            None,
            None,
        )
        .await
        .unwrap();
    let job = created.job.id();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.running_job_id, Some(job));
    let listed = f.store.list_jobs(1).await.unwrap();
    assert_eq!(
        (listed[0].job.id, listed[0].job.kind),
        (job, JobKind::Pr(PrJobKind::Review))
    );
    let picked = pick_next(&scope, &f.cfg, None).await.unwrap();
    assert!(
        !matches!(picked, Some(Candidate::Pr { .. })),
        "its round is running: {picked:?}"
    );
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
        (summary.kind, summary.repo.as_deref(), summary.pr_number),
        (
            Some(JobKind::Pr(PrJobKind::Review)),
            Some("widget"),
            Some(7)
        )
    );
    assert_eq!(
        (summary.state, summary.tokens_new),
        (
            Some(hunter::domain::JobState::Done),
            Some(support::done().tokens_new)
        )
    );
    assert_eq!(
        hunter::daemon::describe_cycle(&summary).1,
        "last: pr_review (widget) -> reviewed"
    );
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
        .observe_pr_review((id, BINDING), &second_head, "t", PrReviewState::Open)
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

/// A round picked for a stable new head finds the PR moved on again (a
/// push seconds ago): it reviews nothing and the newest head waits out its
/// own stability window, so a burst of pushes stays one round.
#[tokio::test]
async fn a_round_whose_head_moved_since_the_pick_waits_for_it_to_settle() {
    let bins = FakeBins::acquire("prr-moved");
    let f = fixture("prr-moved").await;
    gh(&bins, &f, true);
    let first_head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &first_head, "outsider");
    let id = request(&f, &first_head, true).await;
    let backend = reporting();
    run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;

    let second_head = push_to_pr(&f, "MORE.md");
    sqlx::query("UPDATE pr_reviews SET head_sha = ?2, head_seen_at = 0 WHERE id = ?1")
        .bind(id)
        .bind(&second_head)
        .execute(&f.pool().await)
        .await
        .unwrap();
    let third_head = push_to_pr(&f, "EVEN_MORE.md");
    set_view(&f, "OPEN", &third_head, "outsider");

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert!(
        summary
            .skipped
            .as_deref()
            .is_some_and(|s| s.contains("moved")),
        "{summary:?}"
    );
    assert_eq!(
        (summary.kind, summary.repo.as_deref(), summary.pr_number),
        (
            Some(JobKind::Pr(PrJobKind::Review)),
            Some("widget"),
            Some(7)
        ),
        "a skipped round says whose it was"
    );
    assert_eq!(backend.runs().len(), 1, "no reviewer ran");
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        review.last_reviewed_sha.as_deref(),
        Some(first_head.as_str())
    );
    assert_eq!(review.head_sha.as_deref(), Some(third_head.as_str()));
    let now = hunter::util::now_ms();
    assert!(f.store.list_due_pr_reviews(now).await.unwrap().is_empty());
    let settled = now + HEAD_STABLE_MS;
    assert_eq!(
        due_ids(&f.store.list_due_pr_reviews(settled).await.unwrap()),
        vec![id]
    );
}

/// Merging the base branch into the PR keeps the reviewed head an
/// ancestor, but `last..head` would then hold every upstream commit the
/// merge brought in: the round starts over from the new merge-base.
#[tokio::test]
async fn a_round_after_the_base_was_merged_in_reviews_from_the_new_merge_base() {
    let bins = FakeBins::acquire("prr-basemerge");
    let f = fixture("prr-basemerge").await;
    gh(&bins, &f, true);
    let first_head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &first_head, "outsider");
    let id = request(&f, &first_head, true).await;
    let backend = reporting();
    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");

    // Upstream moves on, and the author merges it into the PR.
    git(&f.origin_work, &["checkout", "main"]);
    std::fs::write(f.origin_work.join("UPSTREAM.md"), "upstream\n").unwrap();
    git(&f.origin_work, &["add", "-A"]);
    git(&f.origin_work, &["commit", "-m", "upstream"]);
    git(&f.origin_work, &["push", "origin", "main"]);
    let main = rev(&f.origin_work, "HEAD");
    git(&f.origin_work, &["checkout", "feature"]);
    git(&f.origin_work, &["merge", "--no-edit", "main"]);
    git(
        &f.origin_work,
        &["push", "origin", "HEAD:refs/pull/7/head", "-f"],
    );
    let second_head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &second_head, "outsider");
    support::scope(&f.store)
        .store()
        .observe_pr_review((id, BINDING), &second_head, "t", PrReviewState::Open)
        .await
        .unwrap();
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
        Some(main.as_str())
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
    let summaries = |base: &'static str| async move {
        store
            .chain_pr_review_issues(id, base)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.summary)
            .collect::<Vec<_>>()
    };
    let round = |range: (&'static str, &'static str), issues: Vec<_>| async move {
        store
            .record_pr_review_round((id, BINDING), None, range, PrReviewRound::Ok(&issues))
            .await
            .unwrap();
    };

    round(("base", "aaa"), vec![issue("first")]).await;
    round(("aaa", "bbb"), vec![]).await;
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("bbb", "ccc"),
            PrReviewRound::Failed("worker killed"),
        )
        .await
        .unwrap();
    assert_eq!(
        summaries("bbb").await,
        ["first"],
        "an empty round keeps the chain"
    );
    round(("bbb", "ccc"), vec![issue("third")]).await;
    assert_eq!(summaries("ccc").await, ["first", "third"]);
    assert!(
        summaries("base").await.is_empty(),
        "a round from scratch starts a new chain"
    );

    round(("base", "ddd"), vec![issue("after force-push")]).await;
    assert_eq!(summaries("ddd").await, ["after force-push"]);
}

/// A round stopped partway is in the chain from its base: what it wrote
/// counts, and the round that reviews the same commits again continues
/// the chain from that base, keeping what came before it. Only a finished
/// round moves the reviewed head and ends the streak.
#[tokio::test]
async fn a_partial_round_keeps_the_chain_at_its_base() {
    let f = fixture("prr-chain-partial").await;
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
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("base", "aaa"),
            PrReviewRound::Ok(&[issue("first")]),
        )
        .await
        .unwrap();
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "bbb"),
            PrReviewRound::Partial {
                issues: &[issue("stopped")],
                why: "worker suspended before it finished",
            },
        )
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (review.last_reviewed_sha.as_deref(), review.failures),
        (Some("aaa"), 1)
    );
    store
        .record_pr_review_round((id, BINDING), None, ("aaa", "bbb"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    store
        .record_pr_review_round((id, BINDING), None, ("bbb", "ccc"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (review.last_reviewed_sha.as_deref(), review.failures),
        (Some("ccc"), 0)
    );
    let earlier: Vec<String> = store
        .chain_pr_review_issues(id, "ccc")
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.summary)
        .collect();
    assert_eq!(
        earlier,
        ["first", "stopped"],
        "one chain from the merge-base"
    );
}

/// A request that binds a review to the repo's new URL starts it afresh:
/// its rounds so far were about the PR at the earlier URL, so nothing they
/// reviewed counts at the new one, whose first round reviews the PR from
/// its merge-base. A round and a read of the earlier PR picked before the
/// rebind (a request can land while a round runs) move nothing of it.
#[tokio::test]
async fn a_rebound_review_starts_afresh() {
    let f = fixture("prr-rebound").await;
    let id = request(&f, "aaa", true).await;
    let jobs = support::scope(&f.store);
    let store = jobs.store();
    store
        .record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();

    let moved = "https://github.com/someone-else/widget";
    f.store
        .request_pr_review(&NewPrReview {
            repo_id: f.rid,
            pr_number: 7,
            repo_url: moved,
            title: "make the widget faster",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: "bbb",
            watch: true,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (
            review.binding,
            review.last_reviewed_sha.as_deref(),
            review.round_requested
        ),
        (BINDING + 1, None, true),
        "reviewed afresh, from the merge-base"
    );

    store
        .record_pr_review_round((id, BINDING), None, ("aaa", "ccc"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "ccc"),
            PrReviewRound::Failed("boom"),
        )
        .await
        .unwrap();
    store
        .observe_pr_review(
            (id, BINDING),
            "ccc",
            "the earlier PR",
            PrReviewState::Merged,
        )
        .await
        .unwrap();
    let after = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (
            after.state,
            after.head_sha.as_deref(),
            after.last_reviewed_sha.as_deref(),
            after.round_requested,
            after.failures
        ),
        (PrReviewState::Open, Some("bbb"), None, true, 0)
    );
}

/// A round that fails before its job exists (here the PR read) ends the
/// cycle with its error, counted toward the hold, as an engage's or a
/// harvest's does: the daemon waits its error backoff, not the 15 minutes
/// of a cycle that did nothing.
#[tokio::test]
async fn a_round_failing_before_its_job_ends_the_cycle_with_its_error() {
    let bins = FakeBins::acquire("prr-prefetch");
    let f = fixture("prr-prefetch").await;
    let id = request(&f, "head", true).await;
    bins.script("gh", "echo 'HTTP 502: Bad Gateway' >&2; exit 1");

    let summary = run_cycle(
        &mut support::jobs(&f.store),
        &f.cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;
    assert!(
        summary
            .error
            .as_deref()
            .is_some_and(|e| e.contains("PR/MR view failed")),
        "{summary:?}"
    );
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.failures, 1);
    assert_eq!(
        hunter::daemon::compute_sleep_s(&f.store, &summary).await,
        5.0 * 60.0
    );
}

/// The forge updates a PR's head ref only some time after a push: a round
/// whose fetched ref still names another commit than the head the forge
/// reported fetched the wrong head. It reviews nothing and records no head
/// as reviewed; it fails as a failed fetch does, so the daemon waits its
/// error backoff instead of looping, and once the ref caught up the head
/// the forge reported is the one reviewed.
#[tokio::test]
async fn a_head_ref_behind_the_forge_reviews_nothing() {
    let bins = FakeBins::acquire("prr-lagging-ref");
    let f = fixture("prr-lagging-ref").await;
    gh(&bins, &f, true);
    let fetched = rev(&f.origin_work, "HEAD");
    std::fs::write(f.origin_work.join("MORE.md"), "more\n").unwrap();
    git(&f.origin_work, &["add", "-A"]);
    git(&f.origin_work, &["commit", "-m", "more"]);
    let pushed = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &pushed, "outsider");
    let id = request(&f, &pushed, true).await;
    let backend = reporting();

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    let lag = format!("the PR's head ref is at {}", &fetched[..12]);
    assert!(
        summary.error.as_deref().is_some_and(|e| e.contains(&lag)),
        "{summary:?}"
    );
    assert!(backend.runs().is_empty(), "no reviewer ran");
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.last_reviewed_sha, None);
    assert!(review.round_requested, "the request still waits");
    assert_eq!(review.failures, 1);
    assert_eq!(
        hunter::daemon::compute_sleep_s(&f.store, &summary).await,
        5.0 * 60.0
    );

    git(
        &f.origin_work,
        &["push", "origin", "HEAD:refs/pull/7/head", "-f"],
    );
    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.last_reviewed_sha, Some(pushed));
}

/// A round the budget denies ends the cycle denied and counts nothing: no
/// round is recorded, the review is never held for it, and it stays due.
#[tokio::test]
async fn a_budget_denied_round_counts_nothing() {
    let bins = FakeBins::acquire("prr-denied");
    let f = fixture("prr-denied").await;
    gh(&bins, &f, true);
    let head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &head, "outsider");
    let id = request(&f, &head, true).await;
    let denying = ScriptedBackend::new(|_| panic!("a denied round runs nothing")).denying_above(0);

    for _ in 0..PR_REVIEW_HOLD_AT {
        let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &denying, None).await;
        assert_eq!(
            summary.kind.map(|k| k.to_string()).as_deref(),
            Some("pr_review"),
            "{summary:?}"
        );
        assert!(summary.denied.is_some(), "{summary:?}");
    }
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!((review.failures, review.held), (0, false), "{review:?}");
    assert!(f.store.list_pr_review_rounds().await.unwrap().is_empty());
    let due = f
        .store
        .list_due_pr_reviews(hunter::util::now_ms())
        .await
        .unwrap();
    assert_eq!(due_ids(&due), vec![id]);
}

/// A round whose PR read fails while the forge cannot be reached is the
/// machine's failure, not the review's: past the network gate, the forge
/// went down, and the cycle ends denied for the host with nothing counted
/// (the same failure with the forge reachable counts: see
/// `a_round_failing_before_its_job_ends_the_cycle_with_its_error`).
#[tokio::test]
async fn a_round_failing_while_the_forge_is_unreachable_counts_nothing() {
    let bins = FakeBins::acquire("prr-offline");
    bins.script("gh", "echo 'connection reset' >&2; exit 1");
    let mut f = fixture("prr-offline").await;
    f.cfg.network_gate = true;
    let id = request(&f, "aaa", true).await;
    let down = "github.com";
    let backend = ScriptedBackend::new(|_| panic!("no reviewer without the PR"))
        .offline_once_dispatched(&[down]);

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.unreachable.as_deref(), Some(down), "{summary:?}");
    assert!(summary.error.is_none(), "{summary:?}");
    assert!(
        !bins.calls_to("gh").is_empty(),
        "past the gate: the PR read was tried"
    );
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(review.failures, 0, "{review:?}");
    assert!(f.store.list_pr_review_rounds().await.unwrap().is_empty());
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

/// A reviewer stopped by its token cap keeps the issues it already wrote:
/// the playbook has it rewrite the file after each one, so they are
/// verified. But it did not finish the range, so the round is `partial`:
/// the head is not reviewed, the round counts toward the hold, and the
/// next round reviews the same commits again, shown what the stopped one
/// reported.
#[tokio::test]
async fn a_reviewer_stopped_by_its_cap_keeps_the_issues_it_wrote() {
    let bins = FakeBins::acquire("prr-capped");
    let f = fixture("prr-capped").await;
    gh(&bins, &f, true);
    let head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &head, "outsider");
    let id = request(&f, &head, true).await;
    let backend = ScriptedBackend::seeing(|run| {
        std::fs::write(run.tree.parent().unwrap().join("pr-review.json"), ISSUES).unwrap();
        hunter::types::RunResult {
            exit_code: None,
            killed_reason: Some("cap".to_owned()),
            ..support::done_with_transcript(&run.tree)
        }
    });

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!((review.failures, review.last_reviewed_sha), (1, None));
    let stopped = f.store.list_pr_review_rounds().await.unwrap().remove(0).1;
    assert_eq!(
        (stopped.verdict.as_str(), stopped.issues.len()),
        ("partial", 2)
    );
    assert!(
        stopped
            .failure
            .as_deref()
            .unwrap()
            .contains("before it finished"),
        "{stopped:?}"
    );

    let next = reporting();
    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &next, None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");
    let finished = f.store.list_pr_review_rounds().await.unwrap().remove(0).1;
    assert_eq!(
        (finished.base_sha.as_str(), finished.head_sha.as_str()),
        (stopped.base_sha.as_str(), stopped.head_sha.as_str()),
        "the same commits again"
    );
    let prompt = &next.runs()[0].prompt;
    assert!(prompt.contains("breaks the widget"), "{prompt}");
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (review.failures, review.last_reviewed_sha.as_deref()),
        (0, Some(head.as_str()))
    );
}

/// A review in a repo no job has cloned yet (added disabled, never
/// scanned) clones it first instead of failing at the fetch.
#[tokio::test]
async fn a_review_in_a_repo_never_cloned_clones_it_first() {
    let bins = FakeBins::acquire("prr-clone");
    let f = fixture("prr-clone").await;
    gh(&bins, &f, true);
    // The repo's URL resolves to the fixture's origin for this process's
    // git only.
    let origin = git(&f.origin_work, &["remote", "get-url", "origin"]);
    let gitconfig = f.dir.join("gitconfig");
    std::fs::write(
        &gitconfig,
        format!("[url \"{}\"]\n\tinsteadOf = {REPO_URL}\n", origin.trim()),
    )
    .unwrap();
    let _global = bins.env("GIT_CONFIG_GLOBAL", &gitconfig);
    let clone = Store::repo_dir(&f.dir.path().join("repos"), f.rid);
    std::fs::remove_dir_all(&clone).unwrap();
    let head = rev(&f.origin_work, "HEAD");
    set_view(&f, "OPEN", &head, "outsider");
    request(&f, &head, true).await;

    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &reporting(), None).await;
    assert_eq!(summary.outcome.as_deref(), Some("reviewed"), "{summary:?}");
    assert!(clone.join(".git").exists());
}

/// Each forge's PR head is fetched from where the forge publishes it into
/// hunter's own ref, which a round resolves the head by; never into a
/// branch of the clone.
#[tokio::test]
async fn a_prs_head_is_fetched_into_hunters_own_ref_never_a_branch() {
    use hunter::forge::Forge as _;
    let f = fixture("prr-refspec").await;
    let head = rev(&f.origin_work, "HEAD");
    git(
        &f.origin_work,
        &["push", "origin", "HEAD:refs/merge-requests/7/head"],
    );
    let clone = Store::repo_dir(&f.dir.join("repos"), f.rid);
    let branches = || {
        git(
            &clone,
            &["for-each-ref", "--format=%(refname)", "refs/heads"],
        )
    };
    let before = branches();
    let local = hunter::forge::pr_head_ref(7);

    for refspec in [
        hunter::forge::GitHubForge.pr_head_refspec(7),
        hunter::forge::GitLabForge.pr_head_refspec(7),
    ] {
        git(&clone, &["fetch", "origin", &refspec]);
        assert_eq!(rev(&clone, &local), head, "{refspec}");
        git(&clone, &["update-ref", "-d", &local]);
    }
    assert_eq!(branches(), before, "never a branch of the clone");
}

/// A GitLab MR reads `locked` while it is being merged: it has not ended,
/// so the sync keeps its review open (and a merge that then fails leaves
/// it followed), and the sync after the merge records it merged.
#[tokio::test]
async fn the_sync_keeps_a_gitlab_mr_being_merged_open() {
    let bins = FakeBins::acquire("prr-sync-locked");
    let f = fixture("prr-sync-locked").await;
    let mr = f.dir.join("mr.json");
    bins.script(
        "glab",
        &format!(
            "case \"$2\" in\n\
             */merge_requests/7) cat '{}';;\n\
             *) echo \"unexpected glab $*\" >&2; exit 1;;\n\
             esac",
            mr.display()
        ),
    );
    let rid = f
        .store
        .add_repo(
            "gl-widget",
            "https://gitlab.com/acme/widget",
            &f.dir.subdir("gl-repos"),
            "main",
            ForgeName::Gitlab,
        )
        .await
        .unwrap();
    let id = f
        .store
        .request_pr_review(&NewPrReview {
            repo_id: rid,
            pr_number: 7,
            repo_url: "https://gitlab.com/acme/widget",
            title: "t",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: "aaa",
            watch: true,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap();
    let set_state = |state: &str| {
        let view = json!({ "state": state, "sha": "aaa", "title": "t", "source_branch": "feature",
                           "target_branch": "main", "author": { "username": "outsider" } });
        std::fs::write(&mr, view.to_string()).unwrap();
    };
    let admin = support::admin(&f.store);
    let state = || async { f.store.get_pr_review(id).await.unwrap().unwrap().state };

    set_state("locked");
    hunter::scheduler::sync_pr_reviews(admin.store(), || false).await;
    assert_eq!(
        state().await,
        PrReviewState::Open,
        "mid-merge is not an end"
    );
    set_state("merged");
    hunter::scheduler::sync_pr_reviews(admin.store(), || false).await;
    assert_eq!(state().await, PrReviewState::Merged);
}

/// A watched round comes due by the clock alone, which nothing else wakes
/// the job lane for. A PR review refresh that records a new head brings a round
/// forward, which is work for the job lane (it planned its sleep without
/// it), and the lane's idle sleep then ends as the head has been stable
/// for the window. A refresh that changes nothing is no work, and neither is
/// one whose newer head puts the round off rather than forward.
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "the idle sleep is an exact constant, not a computed value"
)]
async fn a_round_due_by_the_clock_wakes_and_bounds_the_job_lane() {
    let bins = FakeBins::acquire("prr-due-wake");
    let f = fixture("prr-due-wake").await;
    gh(&bins, &f, true);
    set_view(&f, "OPEN", "aaa", "outsider");
    let id = request(&f, "aaa", true).await;
    support::scope(&f.store)
        .store()
        .record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let admin = support::admin(&f.store);
    let notes_lock = tokio::sync::Mutex::new(());
    let status = hunter::activity::AdminStatus::default();
    let (_keep, running) = tokio::sync::watch::channel(false);
    let sync = || {
        hunter::daemon::admin_duty(
            &admin,
            &f.cfg,
            &notes_lock,
            &status,
            hunter::types::AdminTask::PrReviews,
            &running,
        )
    };
    let idle = hunter::scheduler::CycleSummary::default();
    let sleep = || hunter::daemon::compute_sleep_s(&f.store, &idle);

    assert!(!sync().await.made_work(), "the reviewed head again");
    assert_eq!(sleep().await, 15.0 * 60.0, "nothing to wait for");

    set_view(&f, "OPEN", "bbb", "outsider");
    assert!(
        sync().await.made_work(),
        "a new head brings a round forward"
    );
    let window = HEAD_STABLE_MS as f64 / 1000.0;
    let slept = sleep().await;
    assert!(slept <= window && slept > window - 60.0, "{slept}");
    assert!(!sync().await.made_work(), "the same new head again");
    std::thread::sleep(std::time::Duration::from_millis(2));
    set_view(&f, "OPEN", "ccc", "outsider");
    assert!(!sync().await.made_work(), "a newer head puts the round off");
}

/// The PR reviews are their own admin duty, beside the PR sync and never
/// in its pass: a sync pass, which held engages wait for and whose end
/// wakes the job lane for a PR's end, finishes while a review's forge read
/// still hangs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_sync_pass_does_not_wait_for_the_reviews() {
    let bins = FakeBins::acquire("prr-apart");
    let f = fixture("prr-apart").await;
    request(&f, "aaa", true).await;
    let (reading, release) = (f.dir.join("gh-reading"), f.dir.join("gh-release"));
    bins.script(
        "gh",
        &format!(
            // Bounded, so a failing assertion below cannot leave it waiting.
            "touch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\nexit 1",
            reading.display(),
            release.display()
        ),
    );
    let scheduler = hunter::server::SchedulerHandle::default();
    let passes = hunter::daemon::PrSyncPasses::default();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let duties = hunter::daemon::spawn_admin_lane(
        hunter::daemon::AdminLane {
            admin: support::admin(&f.store),
            cfg: f.cfg.clone(),
            notes_lock: Arc::default(),
            status: scheduler.admin.clone(),
            pr_sync_wake: scheduler.pr_sync_wake.clone(),
            pr_review_wake: scheduler.pr_review_wake.clone(),
            reap_wake: scheduler.reap_wake.clone(),
            pr_sync_passes: passes.clone(),
            job_wake: scheduler.wake.clone(),
        },
        &stopping,
    );
    let bound = std::time::Duration::from_secs(10);
    let tick = std::time::Duration::from_millis(20);
    let read_began = tokio::time::timeout(bound, async {
        while !reading.exists() {
            tokio::time::sleep(tick).await;
        }
    })
    .await;
    let synced = tokio::time::timeout(bound, async {
        while passes.now().last_complete == 0 {
            tokio::time::sleep(tick).await;
        }
    })
    .await;
    let still_reading = scheduler
        .admin
        .current()
        .iter()
        .any(|a| a.task == hunter::types::AdminTask::PrReviews);

    std::fs::write(&release, "").unwrap();
    stop.send_replace(true);
    for duty in duties {
        tokio::time::timeout(std::time::Duration::from_secs(30), duty)
            .await
            .expect("an admin duty did not stop")
            .unwrap();
    }
    assert!(read_began.is_ok(), "fixture: the review was never read");
    assert!(
        synced.is_ok(),
        "the PR sync pass waited for the review's read"
    );
    assert!(
        still_reading,
        "fixture: the read ended before the pass did, so it proves nothing"
    );
}

/// The admin lane runs the PR review duty and wakes it on
/// `SchedulerHandle::pr_review_wake` (an approved round, `/api/cycle`):
/// a woken duty reads the open review's PR at once instead of at its
/// next tick, minutes away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_woken_pr_review_duty_refreshes_an_open_review_at_once() {
    let bins = FakeBins::acquire("prr-wake");
    let f = fixture("prr-wake").await;
    gh(&bins, &f, true);
    let id = request(&f, "aaa", true).await;
    // Read by the duty's first run, which the lane makes as it starts:
    // once it is recorded, that run is over and the next is a tick away.
    set_view(&f, "OPEN", "bbb", "outsider");
    let scheduler = hunter::server::SchedulerHandle::default();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let duties = hunter::daemon::spawn_admin_lane(
        hunter::daemon::AdminLane {
            admin: support::admin(&f.store),
            cfg: f.cfg.clone(),
            notes_lock: Arc::default(),
            status: scheduler.admin.clone(),
            pr_sync_wake: scheduler.pr_sync_wake.clone(),
            pr_review_wake: scheduler.pr_review_wake.clone(),
            reap_wake: scheduler.reap_wake.clone(),
            pr_sync_passes: hunter::daemon::PrSyncPasses::default(),
            job_wake: scheduler.wake.clone(),
        },
        &stopping,
    );
    let head_becomes = |head: &'static str| {
        let store = f.store.clone();
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    let review = store.get_pr_review(id).await.unwrap().unwrap();
                    if review.head_sha.as_deref() == Some(head) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .is_ok()
        }
    };
    let first_read = head_becomes("bbb").await;

    set_view(&f, "OPEN", "ccc", "outsider");
    scheduler.pr_review_wake.notify_one();
    let woken_read = head_becomes("ccc").await;

    stop.send_replace(true);
    for duty in duties {
        tokio::time::timeout(std::time::Duration::from_secs(30), duty)
            .await
            .expect("an admin duty did not stop")
            .unwrap();
    }
    assert!(first_read, "the PR review duty's first run never ran");
    assert!(
        woken_read,
        "the woken PR review duty did not run before its tick"
    );
}

/// The PR review refresh records a merged PR's end: the review is over and
/// no round is due any more.
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
