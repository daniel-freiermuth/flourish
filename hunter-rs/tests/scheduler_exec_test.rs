#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Round-3 scheduler executor + store method tests. Each test seeds its own
//! fresh copy of dev.db (schema-complete, zero rows) through a plain sqlx pool.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingJobKind, FindingType, ForgeName, RepoJobKind};
use hunter::store::{JobOutcome, JobStart, Store};
use hunter::transition::Moved;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use support::{GitRepo, ScriptedBackend, TempDir, fresh_pool};

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the pool and any `Store` opened on `path` and SQLite's handles are
/// closed before the files go. Returning only the path would leak the
/// whole directory the moment an assertion panicked.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched-exec");
    let (path, pool) = fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

async fn rw_store(path: &Path) -> Store {
    Store::connect(path).await.unwrap()
}

async fn seed_repo(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_findings(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status,
             created_at, updated_at)
        VALUES
            (1, 'bug', 1, 'fp1', 'low',    0.9, 'one',   'new',     1000, 1000),
            (2, 'bug', 1, 'fp2', 'medium', 0.8, 'two',   'queued',  1000, 1000),
            (3, 'bug', 1, 'fp3', 'high',   0.7, 'three', 'fixing',  1000, 1000),
            (4, 'bug', 1, 'fp4', 'low',    0.6, 'four',  'queued',  1000, 1000);
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

// -- 1. _record_job maps RunResult fields correctly --------------------------

#[tokio::test]
async fn record_job_done_state() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let rr = RunResult {
        exit_code: Some(0),
        killed_reason: None,
        tokens_new: 42_000,
        calls: 15,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 120.5,
        stdout_tail: "all good".to_owned(),
        usage_delta: Some(0.05),
    };
    let state = hunter::scheduler::record_job(&store, job, &rr, Some("claude-4"), None)
        .await
        .unwrap()
        .state();
    assert_eq!(state, hunter::domain::JobState::Done);

    // Verify written fields
    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Done);
    assert_eq!(j.job.tokens_new, Some(42_000));
    assert_eq!(j.job.calls, Some(15));
    assert_eq!(j.job.exit_code, Some(0));
    assert!(j.job.killed_reason.is_none());
    assert_eq!(j.job.session_file.as_deref(), Some("/tmp/sess.jsonl"));
    assert_eq!(j.job.model.as_deref(), Some("claude-4"));
    assert!(j.job.finished_at.is_some());
    // notes should be None for "done" state
    assert!(j.job.notes.is_none());
}

#[tokio::test]
async fn record_job_failed_state() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Fix, 1),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let rr = RunResult {
        exit_code: Some(1),
        killed_reason: None,
        tokens_new: 30_000,
        calls: 10,
        session_file: None,
        duration_s: 60.0,
        stdout_tail: "error: something broke\npanic at line 42".to_owned(),
        usage_delta: None,
    };
    let state = hunter::scheduler::record_job(&store, job, &rr, None, None)
        .await
        .unwrap()
        .state();
    assert_eq!(state, hunter::domain::JobState::Failed);

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Failed);
    assert_eq!(j.job.exit_code, Some(1));
    // notes should contain stdout_tail (up to 500 chars)
    assert!(j.job.notes.is_some());
    assert!(j.job.notes.as_ref().unwrap().contains("panic at line 42"));
}

/// A cap kill that left NO transcript is still `killed`. The session
/// file is what makes a suspension resumable — there is one exact path
/// to hand omp, or there is nothing to continue — so a cap kill without
/// one must not be parked as suspended work that can never be picked up.
#[tokio::test]
async fn record_job_cap_kill_without_a_session_stays_killed() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            Some(50_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 50_000,
        calls: 20,
        session_file: None,
        duration_s: 300.0,
        stdout_tail: "exceeded cap".to_owned(),
        usage_delta: Some(0.1),
    };
    let state = hunter::scheduler::record_job(&store, job, &rr, Some("claude-4"), None)
        .await
        .unwrap()
        .state();
    assert_eq!(state, hunter::domain::JobState::Killed);

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Killed);
    assert_eq!(j.job.killed_reason.as_deref(), Some("cap"));
    assert!(j.job.notes.is_some()); // killed => notes from stdout_tail
}

/// A cap kill that DID leave a transcript is a pause, not a failure.
///
/// The work stopped because the window ran out of headroom, and the
/// reasoning that got it that far is still on disk. Recording it as
/// `killed` is what let one repo run ten consecutive hunts over an
/// identical diff range for 1.48M tokens: a killed job advances no
/// watermark, so the same work was re-selected from scratch every cycle.
#[tokio::test]
async fn record_job_cap_kill_with_a_session_suspends() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            Some(50_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 50_000,
        calls: 20,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 300.0,
        stdout_tail: "exceeded cap".to_owned(),
        usage_delta: Some(0.1),
    };
    let state = hunter::scheduler::record_job(&store, job, &rr, Some("claude-4"), None)
        .await
        .unwrap()
        .state();
    assert_eq!(state, hunter::domain::JobState::Suspended);

    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Suspended);
    assert_eq!(
        store.list_resumable_jobs().await.unwrap().len(),
        1,
        "a suspension with a transcript must be offered back to the scheduler"
    );
}

/// A wallclock kill that did work and left a transcript is a pause.
///
/// Long work outlives one wall-clock slot; restarting it from scratch
/// repays everything already done (F#4082: three 45-min kills, ~785k
/// tokens, no progress kept). Runaways are bounded by the resume ceiling.
#[tokio::test]
async fn record_job_wallclock_kill_with_work_and_a_session_suspends() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    // Two kinds: a second attempt at the same work would replace the
    // first one's checkpoint, however it ended.
    let mut jobs = Vec::new();
    for kind in [RepoJobKind::Hunt, RepoJobKind::TestGap] {
        jobs.push(
            support::scope(&store)
                .create_job(JobStart::fresh_repo(kind, 1), Some(50_000), None)
                .await
                .unwrap()
                .job,
        );
    }
    let rr = |tokens_new| RunResult {
        exit_code: None,
        killed_reason: Some("wallclock".to_owned()),
        tokens_new,
        calls: 20,
        session_file: Some("/tmp/sess.jsonl".to_owned()),
        duration_s: 1800.0,
        stdout_tail: "ran out of wall clock".to_owned(),
        usage_delta: Some(0.1),
    };
    let (idle_job, worked_job) = (jobs.pop().unwrap(), jobs.pop().unwrap());
    let worked_id = worked_job.id();
    let worked = hunter::scheduler::record_job(&store, worked_job, &rr(50_000), None, None)
        .await
        .unwrap()
        .state();
    assert_eq!(worked, hunter::domain::JobState::Suspended);
    // Nothing metered means nothing to continue: resuming would re-run
    // whatever stalled before the first answer.
    let idle = hunter::scheduler::record_job(&store, idle_job, &rr(0), None, None)
        .await
        .unwrap()
        .state();
    assert_eq!(idle, hunter::domain::JobState::Killed);
    let resumable: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(resumable, vec![worked_id]);
}

// -- 2. create_job + update_job round-trip -----------------------------------

#[tokio::test]
async fn create_and_update_job_round_trip() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Create job
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Fix, 1),
            Some(150_000),
            None,
        )
        .await
        .unwrap()
        .job;
    assert!(job.id() > 0);

    // Verify initial state
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Running);
    assert_eq!(jobs[0].job.kind, FindingJobKind::Fix.into());
    assert_eq!(jobs[0].job.repo_id, 1);
    assert_eq!(jobs[0].job.cap_tokens, Some(150_000));

    // Update with completion
    job.complete(&JobOutcome {
        state: hunter::domain::FinishedState::Done,
        tokens_new: 80_000,
        calls: 25,
        exit_code: Some(0),
        killed_reason: None,
        session_file: Some("/tmp/s.json"),
        notes: Some("completed"),
        model: Some("claude-4"),
        usage_delta: Some(0.03),
        finished_at: 2000,
    })
    .await
    .unwrap();

    let jobs = store.list_jobs(10).await.unwrap();
    let j = &jobs[0];
    assert_eq!(j.job.state, hunter::domain::JobState::Done);
    assert_eq!(j.job.tokens_new, Some(80_000));
    assert_eq!(j.job.calls, Some(25));
    assert_eq!(j.job.exit_code, Some(0));
    assert_eq!(j.job.notes.as_deref(), Some("completed"));
    assert_eq!(j.job.model.as_deref(), Some("claude-4"));
    assert_eq!(j.job.finished_at, Some(2000));
}

// -- 3. startup reconcile, cycle scopes, and the stranded-fix requeue --------

/// At startup every `running` job is the dead process's: the fix job is
/// killed as orphaned and the claim it held goes back to `queued`.
#[tokio::test]
async fn reconcile_kills_the_orphaned_fix_and_requeues_its_finding() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    // The prior process: finding #3 is at "fixing" (seeded); its job is
    // "running" when the process dies.
    let prior = rw_store(&path).await;
    let job_id = support::scope(&prior)
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Fix, 3),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job
        .disown();
    drop(prior);
    let store = rw_store(&path).await;

    let (_, settled) = hunter::recovery::reconcile_and_log(
        store.take_unreconciled().unwrap(),
        &dir.subdir("work_root"),
    )
    .await
    .unwrap();

    assert_eq!(
        settled,
        hunter::recovery::Settled {
            jobs: 1,
            requeued: 1,
            deferred: 0,
        }
    );
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);
    let jobs = store.list_jobs(10).await.unwrap();
    let j = jobs.iter().find(|j| j.job.id == job_id).unwrap();
    assert_eq!(j.job.state, hunter::domain::JobState::Killed);
    assert_eq!(j.job.killed_reason.as_deref(), Some("orphaned"));
    assert_eq!(
        j.job.notes.as_deref(),
        Some("orphaned -- prior process died mid-job")
    );
    assert!(j.job.finished_at.is_some());
    let events = store.recent_events(20).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message == "#3 stuck 'fixing' -> 'queued' -- prior process died mid-job"),
        "{events:?}"
    );
}

/// The daemon's startup token comes only with the lock on its database,
/// and that lock is the database's, not the work root's: a second daemon
/// on the same database is refused even with a work root of its own, so
/// it cannot reconcile the first one's live jobs. The store holds the
/// lock while any handle of it lives, and is recovered once the token is
/// spent.
#[tokio::test]
async fn the_startup_token_comes_with_the_database_lock() {
    use hunter::recovery::DaemonLock;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let lock = DaemonLock::acquire(&dir.subdir("work_root"), &path).unwrap();
    let (store, token) = Store::connect_exclusive(lock).await.unwrap();

    let second = DaemonLock::acquire(&dir.subdir("other_root"), &path);
    assert!(second.is_err(), "a second daemon on the same database");
    assert!(
        DaemonLock::acquire(&dir.subdir("work_root"), &dir.join("other.db")).is_err(),
        "a second daemon in the same work root"
    );
    assert!(store.recovered().is_none(), "the token is out, not spent");
    let (recovered, _) = hunter::recovery::reconcile_and_log(token, &dir.subdir("work_root"))
        .await
        .unwrap();
    assert!(store.recovered().is_some());

    drop(store);
    assert!(
        DaemonLock::acquire(&dir.subdir("other_root"), &path).is_err(),
        "held while a handle -- here the proof of recovery -- lives"
    );
    drop(recovered);
    DaemonLock::acquire(&dir.subdir("other_root"), &path).expect("released with the last handle");
}

/// A settle takes what was dropped unended and nothing that is still
/// held: the cycle's abandoned jobs are killed, and its claims -- one
/// whose fix was abandoned, one whose release never landed -- go back to
/// `queued`. A job abandoned on a finding whose claim another fix holds
/// does not release that claim. Another owner's job and claim, held
/// throughout, are untouched.
#[tokio::test]
async fn a_settle_takes_only_what_was_dropped_unended() {
    use hunter::domain::FindingStatus::{Fixing, Queued};
    use hunter::domain::JobState::{Killed, Running};
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) VALUES (5, 'bug', 1, 'fp5', 'low', 0.5, 'five', \
         'queued', 1000, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let scope = support::scope(&store);
    let job = |kind: FindingJobKind, fid| {
        scope.create_job(JobStart::fresh_finding(kind, fid), Some(100_000), None)
    };
    // The cycle's fix of #2 claimed it and abandoned its job.
    let abandoned_claim = scope.claim_fix(2).await.unwrap().unwrap();
    let abandoned_fix = job(FindingJobKind::Fix, 2).await.unwrap().job;
    let abandoned_fix_id = abandoned_fix.id();
    // Another fix of #3 (seeded `fixing`) holds its claim and has not
    // created its job yet; the cycle abandons an engage on #3.
    let abandoned_engage = job(FindingJobKind::Engage, 3).await.unwrap().job;
    let abandoned_engage_id = abandoned_engage.id();
    // Another owner claims #4 and runs its fix, holding both.
    let live_claim = scope.claim_fix(4).await.unwrap().unwrap();
    let live = job(FindingJobKind::Fix, 4).await.unwrap().job;
    let live_id = live.id();
    // The cycle's fix of #5 returned with its claim unreleased (the
    // release write failed after its job was recorded).
    let unreleased = scope.claim_fix(5).await.unwrap().unwrap();
    drop((abandoned_claim, abandoned_fix, abandoned_engage, unreleased));

    let settled = scope.settle(&dir.subdir("work_root")).await.unwrap();

    assert_eq!(
        settled,
        hunter::recovery::Settled {
            jobs: 2,
            requeued: 2,
            deferred: 0,
        }
    );
    for (fid, expected) in [(2, Queued), (5, Queued), (3, Fixing), (4, Fixing)] {
        let status = store.get_finding(fid).await.unwrap().unwrap().status;
        assert_eq!(status, expected, "finding #{fid}");
    }
    let jobs = store.list_jobs(10).await.unwrap();
    let state = |id| jobs.iter().find(|j| j.job.id == id).unwrap().job.state;
    assert_eq!(
        (
            state(abandoned_fix_id),
            state(abandoned_engage_id),
            state(live_id)
        ),
        (Killed, Killed, Running)
    );
    live_claim.disown();
    live.disown();
}

/// A claim, once released, is gone: when another fix claims the same
/// finding before the first fix's cycle settles, that settle leaves the
/// new claim alone. A release is not remembered as something to release
/// again.
#[tokio::test]
async fn a_released_claim_is_not_released_again_under_a_later_claim() {
    use hunter::domain::FindingStatus::Fixing;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let first = support::scope(&store);
    let second = support::scope(&store);
    let claim = first.claim_fix(2).await.unwrap().unwrap();
    assert!(claim.release().await.unwrap());
    let later = second.claim_fix(2).await.unwrap().unwrap();

    let settled = first.settle(&dir.subdir("work_root")).await.unwrap();

    assert_eq!(settled, hunter::recovery::Settled::default());
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Fixing);
    later.disown();
}

/// `FixClaim::disown`, the fixtures' stand-in for a claim a live fix still
/// holds, leaves it in no ledger: a settle after it keeps the finding
/// `fixing`. The tests that model a live holder this way rely on it.
#[tokio::test]
async fn a_disowned_claim_is_owed_to_no_settle() {
    use hunter::domain::FindingStatus::Fixing;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let scope = support::scope(&store);
    scope.claim_fix(2).await.unwrap().unwrap().disown();

    let settled = scope.settle(&dir.subdir("work_root")).await.unwrap();

    assert_eq!(settled, hunter::recovery::Settled::default());
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Fixing);
}

/// A claim a settle released is struck from what is owed: when another
/// fix then claims the same finding, the next settle leaves that claim
/// alone. A settled claim is not remembered as something to release
/// again.
#[tokio::test]
async fn a_settled_claim_is_not_released_again_under_a_later_claim() {
    use hunter::domain::FindingStatus::Fixing;
    use hunter::recovery::Settled;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let recovered = support::recovered(&store);
    let work_root = dir.subdir("work_root");
    drop(support::scope(&store).claim_fix(2).await.unwrap().unwrap());
    let released = recovered.settle_unsettled(&work_root).await.unwrap();
    assert_eq!(released.requeued, 1);
    let later = support::scope(&store).claim_fix(2).await.unwrap().unwrap();

    let settled = recovered.settle_unsettled(&work_root).await.unwrap();

    assert_eq!(settled, Settled::default());
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Fixing);
    later.disown();
}

/// A post-run downgrade (`fail_job`, an engage whose publish failed)
/// revises the job whose outcome it was handed, and no other.
#[tokio::test]
async fn a_downgrade_fails_the_recorded_job_and_no_other() {
    use hunter::domain::{FinishedState, JobState};
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;
    let scope = support::scope(&store);
    let mut recorded = Vec::new();
    for _ in 0..2 {
        let job = scope
            .create_job(JobStart::fresh_repo(RepoJobKind::Hunt, 1), None, None)
            .await
            .unwrap()
            .job;
        let outcome = JobOutcome {
            state: FinishedState::Done,
            tokens_new: 1_000,
            calls: 1,
            exit_code: Some(0),
            killed_reason: None,
            session_file: None,
            notes: None,
            model: None,
            usage_delta: None,
            finished_at: 2000,
        };
        recorded.push(job.complete(&outcome).await.unwrap());
    }

    store.fail_job(&recorded[1], "push failed").await.unwrap();

    let jobs = store.list_jobs(10).await.unwrap();
    let row = |id: i64| {
        let j = &jobs.iter().find(|j| j.job.id == id).unwrap().job;
        (j.state, j.notes.clone())
    };
    assert_eq!(row(recorded[0].id()), (JobState::Done, None));
    assert_eq!(
        row(recorded[1].id()),
        (JobState::Failed, Some("push failed".to_owned()))
    );
}

/// A declined fix's verdict is written only while its claim still holds
/// the finding. One that left `fixing` meanwhile keeps the verdict it was
/// given there, its anchor included: the anchor goes with the status
/// write, not past it. The conclusion reports where the finding went.
#[tokio::test]
async fn a_conclusion_leaves_a_verdict_written_past_the_claim() {
    use hunter::domain::FindingStatus::Rejected;
    use hunter::store::VerdictAnchor;
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let claim = support::scope(&store).claim_fix(2).await.unwrap().unwrap();
    let operator = VerdictAnchor {
        sha: "a".repeat(40),
        files: vec!["src/a.rs".to_owned()],
        holds_while: None,
    };
    store
        .set_anchored_verdict(2, Rejected, "operator: wrong", Some(&operator))
        .await
        .unwrap();
    let worker = VerdictAnchor {
        sha: "b".repeat(40),
        files: vec!["src/b.rs".to_owned()],
        holds_while: Some("b stays as it is".to_owned()),
    };

    let concluded = claim
        .conclude(
            Some(hunter::domain::ClosureClass::Unwanted),
            "worker: not worth it",
            Some(&worker),
        )
        .await;

    assert_eq!(
        concluded.ok(),
        Some(Moved::Conflict {
            now: Some(Rejected)
        })
    );
    let finding = store.get_finding(2).await.unwrap().unwrap();
    assert_eq!(
        (finding.status, finding.verdict_reason.as_deref()),
        (Rejected, Some("operator: wrong"))
    );
    assert_eq!(store.verdict_anchor(2).await.unwrap(), Some(operator));
}

/// A shipped fix moves its finding to `pr_open` only while its claim still
/// holds it. One that left `fixing` meanwhile stays where it went, without
/// the PR's URL, and the claim reports where that is.
#[tokio::test]
async fn an_opened_pr_leaves_a_finding_moved_past_the_claim() {
    use hunter::domain::FindingStatus::Wontfix;
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let claim = support::scope(&store).claim_fix(2).await.unwrap().unwrap();
    store
        .set_finding_verdict(2, Wontfix, "operator: not worth it")
        .await
        .unwrap();

    let opened = claim.open_pr("https://github.com/o/r/pull/7").await;

    assert_eq!(opened.ok(), Some(Moved::Conflict { now: Some(Wontfix) }));
    let finding = store.get_finding(2).await.unwrap().unwrap();
    assert_eq!((finding.status, finding.pr_url), (Wontfix, None));
}

/// A dropped claim is released only when no `running` job works on the
/// finding: an owed claim on which a fix still runs is kept, so a settle
/// cannot take a claim from a fix that is running. Kept, not forgotten:
/// once that job has ended, the next settle releases it.
#[tokio::test]
async fn a_settled_claim_spares_a_finding_a_running_job_works_on() {
    use hunter::domain::FindingStatus::{Fixing, Queued};
    use hunter::recovery::Settled;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let scope = support::scope(&store);
    drop(scope.claim_fix(2).await.unwrap().unwrap());
    let running = scope
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Fix, 2),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let recovered = support::recovered(&store);
    let work_root = dir.subdir("work_root");

    let spared = recovered.settle_unsettled(&work_root).await.unwrap();

    assert_eq!(
        spared,
        Settled {
            deferred: 1,
            ..Settled::default()
        }
    );
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Fixing, "job {} still runs on it", running.id());

    running
        .complete(&JobOutcome {
            state: hunter::domain::FinishedState::Failed,
            tokens_new: 0,
            calls: 0,
            exit_code: Some(1),
            killed_reason: None,
            session_file: None,
            notes: None,
            model: None,
            usage_delta: None,
            finished_at: 2000,
        })
        .await
        .unwrap();
    let released = recovered.settle_unsettled(&work_root).await.unwrap();

    assert_eq!(
        released,
        Settled {
            requeued: 1,
            ..Settled::default()
        }
    );
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Queued);
}

/// A dropped job is not a stopped worker: a worker runs on a blocking
/// thread that cancelling its runner's future does not stop. While a
/// process still works in the job's tree, the settle leaves the job
/// `running` and owed, holding its reservation; once the tree is free,
/// the next settle ends it.
#[tokio::test]
async fn a_job_dropped_while_its_tree_is_in_use_stays_owed() {
    use hunter::domain::JobState::{Killed, Running};
    use hunter::recovery::Settled;
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let work_root = dir.subdir("work_root");
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let id = job.id();
    let tree = hunter::workspace::Workspace::for_chain(&work_root, Path::new(""), id).tree;
    std::fs::create_dir_all(&tree).unwrap();
    // The worker its runner's dropped future left behind.
    let mut worker = std::process::Command::new("sleep")
        .arg("60")
        .current_dir(&tree)
        .spawn()
        .unwrap();
    drop(job);
    let recovered = support::recovered(&store);
    let state = |store: Store| async move {
        let jobs = store.list_jobs(10).await.unwrap();
        jobs.into_iter().find(|j| j.job.id == id).unwrap().job.state
    };

    let while_working = recovered.settle_unsettled(&work_root).await;
    let state_while_working = state(store.clone()).await;
    worker.kill().unwrap();
    worker.wait().unwrap();
    let after = recovered.settle_unsettled(&work_root).await.unwrap();

    assert_eq!(
        while_working.unwrap(),
        Settled {
            deferred: 1,
            ..Settled::default()
        }
    );
    assert_eq!(state_while_working, Running);
    assert_eq!(
        after,
        Settled {
            jobs: 1,
            ..Settled::default()
        }
    );
    assert_eq!(state(store).await, Killed);
}

/// A job dropped unended -- its runner returned early, its future was
/// cancelled, a panic unwound through it -- is owed: the next settle of
/// the store's ledger kills it.
#[tokio::test]
async fn a_job_dropped_unended_is_owed_to_the_next_settle() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let job = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            Some(100_000),
            None,
        )
        .await
        .unwrap()
        .job;
    let id = job.id();
    drop(job);

    let settled = support::recovered(&store)
        .settle_unsettled(&dir.subdir("work_root"))
        .await
        .unwrap();

    assert_eq!(settled.jobs, 1);
    let jobs = store.list_jobs(10).await.unwrap();
    let row = &jobs.iter().find(|j| j.job.id == id).unwrap().job;
    assert_eq!(row.state, hunter::domain::JobState::Killed);
    assert_eq!(
        row.notes.as_deref(),
        Some("abandoned -- its runner returned without recording an outcome")
    );
}

/// `fixing` is taken only through `claim_fix`, and left only through the
/// `FixClaim` it returns: no status write reaches it otherwise (no
/// `Transition` names it), and even the fixture write refuses it, so no
/// test builds a claim that nothing would release.
#[tokio::test]
async fn a_fix_claim_without_an_owner_is_refused() {
    use hunter::domain::FindingStatus::{Fixing, Queued};
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    let set = store.set_finding_status(2, Fixing).await;

    assert!(set.is_err(), "{set:?}");
    let status = store.get_finding(2).await.unwrap().unwrap().status;
    assert_eq!(status, Queued);
}

// -- 4. ingest_findings: valid inserted, duplicates skipped, invalid counted -

#[tokio::test]
async fn ingest_findings_valid_dup_invalid() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    // Write a temporary findings JSON
    let tmp = dir.join("findings.json");
    let content = serde_json::json!([
        {
            "fingerprint": "fp-ingest-1",
            "bug_class": "logic",
            "severity": "high",
            "confidence": 0.95,
            "summary": "A real bug",
            "file": "src/main.rs",
        },
        {
            "fingerprint": "fp-ingest-2",
            "bug_class": "boundary",
            "severity": "medium",
            "confidence": 0.5,
            "summary": "Another bug",
        },
        "not an object at all",
        {
            "fingerprint": "fp-ingest-3",
            "bug_class": "INVALID_CLASS",
            "severity": "low",
            "confidence": 0.3,
            "summary": "Bad bug class",
        },
    ]);
    std::fs::write(&tmp, content.to_string()).unwrap();

    let result = hunter::ingest::ingest_findings(
        &store,
        1,
        &tmp,
        hunter::ingest::EntryTypes::Fixed(FindingType::Bug),
        None,
        None,
    )
    .await;
    assert_eq!(result.inserted, 2);
    assert_eq!(result.duplicates, 0);
    assert_eq!(result.invalid, 2); // string + bad bug_class

    // Ingest again: same entries -> duplicates
    let result2 = hunter::ingest::ingest_findings(
        &store,
        1,
        &tmp,
        hunter::ingest::EntryTypes::Fixed(FindingType::Bug),
        None,
        None,
    )
    .await;
    assert_eq!(result2.inserted, 0);
    assert_eq!(result2.duplicates, 2);
}

#[tokio::test]
async fn ingest_findings_test_gap_type() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let tmp = dir.join("test-gaps.json");
    let content = serde_json::json!([
        {
            "fingerprint": "tg-1",
            "severity": "medium",
            "confidence": 0.8,
            "summary": "Missing tests for module A",
            "missing_tests": ["test_a", "test_b"],
            "test_file": "tests/test_a.rs",
        },
        {
            "fingerprint": "tg-2",
            "severity": "low",
            "confidence": 0.5,
            "summary": "Missing tests but wrong shape",
            "missing_tests": "not a list",
            "test_file": "tests/test_b.rs",
        },
    ]);
    std::fs::write(&tmp, content.to_string()).unwrap();

    let result = hunter::ingest::ingest_findings(
        &store,
        1,
        &tmp,
        hunter::ingest::EntryTypes::Fixed(FindingType::TestGap),
        None,
        None,
    )
    .await;
    assert_eq!(result.inserted, 1);
    assert_eq!(result.invalid, 1); // missing_tests not a list
}

// -- 5. Store::transition ----------------------------------------------------

/// A transition is made only from its source status. A finding something
/// else moved on meanwhile keeps its status and its verdict, and the
/// conflict names where it is.
#[tokio::test]
async fn a_transition_applies_only_from_its_source_status() {
    use hunter::domain::FindingStatus::{PrOpen, Rechecking};
    use hunter::transition::{Moved, Transition};
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Finding #1 is "new".
    let requested = store.transition(1, &Transition::RecheckRequested).await;
    assert_eq!(requested.unwrap(), Moved::Applied);
    assert_eq!(
        store.get_finding(1).await.unwrap().unwrap().status,
        Rechecking
    );

    store
        .set_finding_verdict(2, PrOpen, "operator's reason")
        .await
        .unwrap();
    let stale = Transition::RecheckStale {
        reason: "recheck: code moved",
    };
    let moved = store.transition(2, &stale).await.unwrap();
    assert_eq!(moved, Moved::Conflict { now: Some(PrOpen) });
    let f2 = store.get_finding(2).await.unwrap().unwrap();
    assert_eq!(f2.status, PrOpen);
    assert_eq!(f2.verdict_reason.as_deref(), Some("operator's reason"));

    let gone = store.transition(99, &Transition::Unqueued).await.unwrap();
    assert_eq!(gone, Moved::Conflict { now: None });
}

/// An operator's verdict replaces the verdict before it entirely: one
/// given without a reason leaves no reason behind, so a closed PR the
/// operator queues again no longer reads as "awaiting harvest".
#[tokio::test]
async fn a_verdict_without_a_reason_clears_the_earlier_one() {
    use hunter::domain::FindingStatus::{Closed, Queued};
    use hunter::transition::{Moved, OperatorVerdict, Transition};
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    store
        .set_finding_verdict(1, Closed, "PR closed without merge; awaiting harvest")
        .await
        .unwrap();

    let requeue = OperatorVerdict::new(Closed, Queued, None).unwrap();
    let moved = store.transition(1, &Transition::Verdict(requeue)).await;

    assert_eq!(moved.unwrap(), Moved::Applied);
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!((f.status, f.verdict_reason), (Queued, None));
}

// -- 6. upsert_finding + suppressions + known_active -------------------------

#[tokio::test]
async fn upsert_finding_and_queries() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    let row = hunter::store::FindingInsert {
        fingerprint: "my-fp".into(),
        file: "lib.rs".into(),
        severity: hunter::domain::Severity::High,
        confidence: 0.9,
        summary: "test finding".into(),
        bug_class: Some(hunter::domain::BugClass::Logic),
        ..Default::default()
    };
    let (id, inserted) = store.upsert_finding(1, &row, "bug", None).await.unwrap();
    assert!(inserted);
    assert!(id > 0);

    // Duplicate -> not inserted
    let (id2, inserted2) = store.upsert_finding(1, &row, "bug", None).await.unwrap();
    assert!(!inserted2);
    assert_eq!(id, id2);

    // The finding is 'new' -> in known_active, not in suppressions
    let active = store.known_active(1, "bug").await.unwrap();
    assert_eq!(active.len(), 1);
    let suppressed = store.suppressions(1, "bug").await.unwrap();
    assert_eq!(suppressed.len(), 0);

    // Mark as rejected -> in suppressions
    store
        .set_finding_verdict(id, hunter::domain::FindingStatus::Rejected, "test")
        .await
        .unwrap();
    let active2 = store.known_active(1, "bug").await.unwrap();
    assert_eq!(active2.len(), 0);
    let suppressed2 = store.suppressions(1, "bug").await.unwrap();
    assert_eq!(suppressed2.len(), 1);
}

// -- 7. record/clear failure streaks -----------------------------------------

#[tokio::test]
async fn work_failure_streak_tracking() {
    use hunter::domain::FindingJobKind::{Fix, Harvest};
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    let streak = |kind| {
        let store = store.clone();
        async move { store.work_streak(1, kind).await.unwrap() }
    };

    // Same failure twice
    assert_eq!(
        store
            .record_work_failure(1, Fix, "push failed")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        streak(Fix).await.last_failure.as_deref(),
        Some("push failed")
    );
    assert_eq!(
        store
            .record_work_failure(1, Fix, "push failed")
            .await
            .unwrap(),
        2
    );
    assert_eq!(streak(Fix).await.streak, 2);

    // Each kind keeps its own streak
    assert_eq!(
        store
            .record_work_failure(1, Harvest, "push failed")
            .await
            .unwrap(),
        1
    );
    assert_eq!(streak(Fix).await.streak, 2);

    // Different failure resets streak
    assert_eq!(
        store
            .record_work_failure(1, Fix, "PR create failed")
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        streak(Fix).await.last_failure.as_deref(),
        Some("PR create failed")
    );
}

// -- 3b. reconcile_and_log: an orphan that did work is kept for resume -------

/// A daemon killed mid-job leaves the row `running`. When the chain's
/// session directory holds a transcript with metered work, the job is
/// suspended with that transcript so the resume tier continues it; a job
/// that died before its first call (the 2026-09-29 fix jobs killed inside
/// `git worktree add`) is still reconciled `killed`.
#[tokio::test]
async fn reconcile_suspends_an_orphan_that_did_work_and_kills_one_that_did_not() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let prior = rw_store(&path).await;
    let work_root = dir.subdir("work_root");
    let mut ids = Vec::new();
    // Two kinds: one repo runs one job per kind at a time.
    for kind in [
        hunter::domain::RepoJobKind::TestGap,
        hunter::domain::RepoJobKind::Refactor,
    ] {
        let id = support::scope(&prior)
            .create_job(JobStart::fresh_repo(kind, 1), Some(100_000), None)
            .await
            .unwrap()
            .job
            .disown();
        std::fs::create_dir_all(work_root.join("jobs").join(id.to_string()).join("session"))
            .unwrap();
        ids.push(id);
    }
    let (worked, empty) = (ids[0], ids[1]);
    let transcript = work_root
        .join("jobs")
        .join(worked.to_string())
        .join("session")
        .join("2026-09-29T21-39-39-038Z_x.jsonl");
    std::fs::write(
        &transcript,
        r#"{"message":{"role":"assistant","usage":{"input":1000,"output":100,"cacheRead":0,"cacheWrite":500}}}
"#,
    )
    .unwrap();

    let store = rw_store(&path).await;
    hunter::recovery::reconcile_and_log(store.take_unreconciled().unwrap(), &work_root)
        .await
        .unwrap();

    let jobs = store.list_jobs(10).await.unwrap();
    let row = |id: i64| jobs.iter().find(|j| j.job.id == id).unwrap().job.clone();
    let w = row(worked);
    assert_eq!(w.state, hunter::domain::JobState::Suspended);
    assert_eq!(w.session_file.as_deref(), transcript.to_str());
    assert_eq!(w.tokens_new, Some(1600));
    assert_eq!(row(empty).state, hunter::domain::JobState::Killed);
    let resumable: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(resumable, vec![worked]);
}

/// One assistant call worth 1,600 new tokens, in omp's ledger format.
const CALL_1600: &str = r#"{"message":{"role":"assistant","usage":{"input":1000,"output":100,"cacheRead":0,"cacheWrite":500}}}"#;

/// A harvest handed off from an engage continues the engage's transcript
/// in the engage's session directory, so the ledger there holds the
/// engage's recorded spend too. Only what the harvest added is its own:
/// none, and it is killed as before; one more call, and it is suspended
/// with exactly that call's tokens.
#[tokio::test]
async fn reconcile_counts_only_the_orphans_own_work_in_a_handed_off_transcript() {
    for (extra_calls, expected) in [(0, None), (1, Some(1600))] {
        let (dir, path, pool) = fresh_db().await;
        seed_repo(&pool).await;
        seed_findings(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO jobs (id, kind, repo_id, finding_id, state, tokens_new, started_at, finished_at) \
             VALUES (10, 'engage', 1, 2, 'done', 1600, 1000, 2000); \
             INSERT INTO jobs (id, kind, repo_id, finding_id, state, started_at, resumed_from) \
             VALUES (11, 'harvest', 1, 2, 'running', 3000, 10);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = rw_store(&path).await;
        let work_root = dir.subdir("work_root");
        let session = work_root.join("jobs").join("10").join("session");
        std::fs::create_dir_all(&session).unwrap();
        let lines = vec![CALL_1600; 1 + extra_calls].join("\n") + "\n";
        std::fs::write(session.join("s.jsonl"), lines).unwrap();

        hunter::recovery::reconcile_and_log(store.take_unreconciled().unwrap(), &work_root)
            .await
            .unwrap();

        let jobs = store.list_jobs(10).await.unwrap();
        let harvest = &jobs.iter().find(|j| j.job.id == 11).unwrap().job;
        match expected {
            None => assert_eq!(harvest.state, hunter::domain::JobState::Killed),
            Some(tokens) => {
                assert_eq!(harvest.state, hunter::domain::JobState::Suspended);
                assert_eq!(harvest.tokens_new, Some(tokens));
            }
        }
    }
}

/// A worker that outlived its daemon (one run by hand and killed) is
/// still in its tree. Resuming that chain would put a second worker in
/// the same tree and transcript, so the orphan is killed, not suspended.
#[tokio::test]
async fn reconcile_does_not_suspend_an_orphan_whose_worker_is_still_running() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at) VALUES (5, 'test_gap', 1, 'running', 1000);",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let work_root = dir.subdir("work_root");
    let chain = work_root.join("jobs").join("5");
    std::fs::create_dir_all(chain.join("session")).unwrap();
    std::fs::create_dir_all(chain.join("tree")).unwrap();
    std::fs::write(
        chain.join("session").join("s.jsonl"),
        format!("{CALL_1600}\n"),
    )
    .unwrap();
    let mut worker = std::process::Command::new("sleep")
        .arg("30")
        .current_dir(chain.join("tree"))
        .spawn()
        .unwrap();

    let result =
        hunter::recovery::reconcile_and_log(store.take_unreconciled().unwrap(), &work_root).await;
    worker.kill().unwrap();
    worker.wait().unwrap();
    result.unwrap();

    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Killed);
}

/// A suspension ends a job like a kill or a failure does: the cycle did
/// work, so with fixes queued the daemon drains at once rather than
/// sleeping the 15-minute idle interval, and `scheduler_state` names the
/// job instead of reporting no outcome. A wallclock overrun or a death
/// after doing work used to end `killed`/`failed` and is now `suspended`.
#[tokio::test]
async fn a_suspended_job_drains_the_queue_like_a_killed_one() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;
    // The contrast: with the same fixes queued, a cycle that ran no job
    // still sleeps the idle interval. That fall-through is where a
    // suspended job used to land.
    let no_job = hunter::scheduler::CycleSummary::default();
    let idle_s = hunter::daemon::compute_sleep_s(&store, &no_job).await;
    assert!(
        (idle_s - 15.0 * 60.0).abs() < f64::EPSILON,
        "no job slept {idle_s}s"
    );
    for state in [
        hunter::domain::JobState::Killed,
        hunter::domain::JobState::Suspended,
    ] {
        let summary = hunter::scheduler::CycleSummary {
            kind: Some(RepoJobKind::TestGap.into()),
            repo: Some("alpha".into()),
            state: Some(state),
            ..Default::default()
        };
        let sleep_s = hunter::daemon::compute_sleep_s(&store, &summary).await;
        assert!(sleep_s.abs() < f64::EPSILON, "{state:?} slept {sleep_s}s");
        assert_eq!(
            hunter::daemon::describe_cycle(&summary),
            (
                "idle".to_owned(),
                format!("last: test_gap (alpha) {}", state.as_str())
            )
        );
    }
}

// -- unreadable prior findings refuse a repo job -----------------------------

/// A repo with a real clone whose only finding of `finding_type` cannot be
/// decoded: its severity is not one the enum knows. Both whole-list reads a
/// cold prompt is built from fail on such a row. The playbooks exist, so
/// nothing but that read stands between the job and its worker.
async fn repo_with_an_undecodable_finding(
    dir: &TempDir,
    finding_type: &str,
) -> (PathBuf, Store, hunter::types::Repo, hunter::config::Config) {
    let repo = GitRepo::with_branch(dir, "feature");
    let (path, pool) = fresh_pool(dir, "hunter").await;
    let store = rw_store(&path).await;
    let repos_root = dir.subdir("repos");
    let rid = store
        .add_repo(
            "widget",
            &repo.origin.to_string_lossy(),
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    std::fs::rename(&repo.work, Store::repo_dir(&repos_root, rid)).unwrap();
    sqlx::query(
        "INSERT INTO findings
            (type, repo_id, fingerprint, severity, confidence, summary, status,
             created_at, updated_at)
         VALUES (?1, ?2, 'fp-bad', 'HIGH', 0.9, 'turned down', 'rejected', 1000, 1000)",
    )
    .bind(finding_type)
    .bind(rid)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let playbooks = dir.subdir("playbooks");
    for name in ["hunt.md", "refactor.md", "test_gap.md"] {
        std::fs::write(playbooks.join(name), "work in {{REPO_PATH}}\n").unwrap();
    }
    let row = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    (path, store, row, cfg)
}

/// A hunt whose suppression list cannot be read is refused, not run blind.
///
/// Reading that failure as "nothing was ever rejected" would launch a
/// metered worker told to rediscover what an operator already turned
/// down. The refusal comes before the budget gate: no job row, no worker.
#[tokio::test]
async fn a_hunt_whose_suppressions_cannot_be_read_is_refused() {
    let dir = TempDir::new("hunt-bad-suppressions");
    let (_path, store, row, cfg) =
        repo_with_an_undecodable_finding(&dir, FindingType::Bug.as_str()).await;
    let backend = ScriptedBackend::noop();

    let err = hunter::scheduler::run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        &row,
        &backend,
        None,
    )
    .await
    .expect_err("an unreadable suppression list must refuse the hunt");

    assert!(
        format!("{err:#}").contains("hunt widget: loading suppressed bug findings"),
        "the error must say which read failed for which job: {err:#}"
    );
    assert!(
        backend.runs().is_empty(),
        "no worker may have been launched"
    );
    assert!(
        store.list_jobs(10).await.unwrap().is_empty(),
        "no job row may have been written"
    );
}

/// The same refusal for the shared repo-level analysis body, through each
/// runner that wraps it: the error names the runner's own kind, so each
/// runs its own spec.
#[tokio::test]
async fn an_analysis_job_whose_suppressions_cannot_be_read_is_refused() {
    for kind in [
        RepoJobKind::Refactor,
        RepoJobKind::Modernization,
        RepoJobKind::Standards,
    ] {
        let dir = TempDir::new("analysis-bad-suppressions");
        let (_path, store, row, cfg) = repo_with_an_undecodable_finding(&dir, kind.as_str()).await;
        let backend = ScriptedBackend::noop();
        let scope = support::scope(&store);

        let result = match kind {
            RepoJobKind::Refactor => {
                hunter::scheduler::run_refactor(
                    &scope,
                    store.test_lane(),
                    &cfg,
                    &row,
                    &backend,
                    None,
                )
                .await
            }
            RepoJobKind::Modernization => {
                hunter::scheduler::run_modernize(
                    &scope,
                    store.test_lane(),
                    &cfg,
                    &row,
                    &backend,
                    None,
                )
                .await
            }
            _ => {
                hunter::scheduler::run_standards(
                    &scope,
                    store.test_lane(),
                    &cfg,
                    &row,
                    &backend,
                    None,
                )
                .await
            }
        };
        let err = result.expect_err("an unreadable suppression list must refuse the job");

        let expected = format!("{kind} widget: loading suppressed {kind} findings");
        assert!(
            format!("{err:#}").contains(&expected),
            "the error must say which read failed for which job: {err:#}"
        );
        assert!(
            backend.runs().is_empty(),
            "no worker may have been launched"
        );
        assert!(
            store.list_jobs(10).await.unwrap().is_empty(),
            "no job row may have been written"
        );
    }
}

/// A `test_gap` job also hands its worker the repo's bug findings, since it
/// may file a bug instead; an unreadable bug list refuses it just the same.
#[tokio::test]
async fn a_test_gap_whose_bug_suppressions_cannot_be_read_is_refused() {
    let dir = TempDir::new("test-gap-bad-bug-suppressions");
    let (_path, store, row, cfg) =
        repo_with_an_undecodable_finding(&dir, FindingType::Bug.as_str()).await;
    let backend = ScriptedBackend::noop();

    let err = hunter::scheduler::run_test_gap(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        &row,
        &backend,
        None,
    )
    .await
    .expect_err("an unreadable bug suppression list must refuse the job");

    assert!(
        format!("{err:#}").contains("test_gap widget: loading suppressed bug findings"),
        "the error must say which read failed for which job: {err:#}"
    );
    assert!(
        backend.runs().is_empty(),
        "no worker may have been launched"
    );
    assert!(
        store.list_jobs(10).await.unwrap().is_empty(),
        "no job row may have been written"
    );
}
