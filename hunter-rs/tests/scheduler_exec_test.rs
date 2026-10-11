#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Round-3 scheduler executor + store method tests. Each test seeds its own
//! fresh copy of dev.db (schema-complete, zero rows) through a plain sqlx pool.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingJobKind, FindingType, ForgeName, RepoJobKind};
use hunter::store::{JobOutcome, Store};
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
             created_at, updated_at, fix_attempts, recheck_attempts)
        VALUES
            (1, 'bug', 1, 'fp1', 'low',    0.9, 'one',   'new',     1000, 1000, 0, 0),
            (2, 'bug', 1, 'fp2', 'medium', 0.8, 'two',   'queued',  1000, 1000, 0, 0),
            (3, 'bug', 1, 'fp3', 'high',   0.7, 'three', 'fixing',  1000, 1000, 0, 0),
            (4, 'bug', 1, 'fp4', 'low',    0.6, 'four',  'queued',  1000, 1000, 0, 0);
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
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
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
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
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
    let store = rw_store(&path).await;
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            None,
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
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
    let state = hunter::scheduler::record_job(&store, job_id, &rr, None, None)
        .await
        .unwrap();
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
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(50_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
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
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
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
    let job_id = store
        .create_job(
            RepoJobKind::Hunt.into(),
            1,
            None,
            Some(50_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
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
    let state = hunter::scheduler::record_job(&store, job_id, &rr, Some("claude-4"), None)
        .await
        .unwrap();
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
    let mut ids = Vec::new();
    for _ in 0..2 {
        ids.push(
            store
                .create_job(
                    RepoJobKind::Hunt.into(),
                    1,
                    None,
                    Some(50_000),
                    hunter::domain::JobState::Running,
                    None,
                    None,
                )
                .await
                .unwrap()
                .id,
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
    let worked = hunter::scheduler::record_job(&store, ids[0], &rr(50_000), None, None)
        .await
        .unwrap();
    assert_eq!(worked, hunter::domain::JobState::Suspended);
    // Nothing metered means nothing to continue: resuming would re-run
    // whatever stalled before the first answer.
    let idle = hunter::scheduler::record_job(&store, ids[1], &rr(0), None, None)
        .await
        .unwrap();
    assert_eq!(idle, hunter::domain::JobState::Killed);
    let resumable: Vec<i64> = store
        .list_resumable_jobs()
        .await
        .unwrap()
        .iter()
        .map(|j| j.id)
        .collect();
    assert_eq!(resumable, vec![ids[0]]);
}

// -- 2. create_job + update_job round-trip -----------------------------------

#[tokio::test]
async fn create_and_update_job_round_trip() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    let store = rw_store(&path).await;

    // Create job
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            None,
            Some(150_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;
    assert!(job_id > 0);

    // Verify initial state
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Running);
    assert_eq!(jobs[0].job.kind, FindingJobKind::Fix.into());
    assert_eq!(jobs[0].job.repo_id, 1);
    assert_eq!(jobs[0].job.cap_tokens, Some(150_000));

    // Update with completion
    store
        .complete_job(
            job_id,
            &JobOutcome {
                state: hunter::domain::JobState::Done,
                tokens_new: 80_000,
                calls: 25,
                exit_code: Some(0),
                killed_reason: None,
                session_file: Some("/tmp/s.json"),
                notes: Some("completed"),
                model: Some("claude-4"),
                usage_delta: Some(0.03),
                finished_at: 2000,
            },
        )
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

// -- 3. reconcile_orphaned_jobs ----------------------------------------------

#[tokio::test]
async fn reconcile_orphaned_jobs() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Create a "running" job (orphaned)
    let job_id = store
        .create_job(
            FindingJobKind::Fix.into(),
            1,
            Some(3),
            Some(100_000),
            hunter::domain::JobState::Running,
            None,
            None,
        )
        .await
        .unwrap()
        .id;

    // Finding #3 is at "fixing" (seeded above), job at "running"
    let (stuck_findings, orphaned_jobs) = store.reconcile_orphaned_jobs().await.unwrap();

    // Finding #3 should have been moved from "fixing" to "queued"
    assert_eq!(stuck_findings.len(), 1);
    assert_eq!(stuck_findings[0].id, 3);
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);

    // Job should have been marked "killed"
    assert_eq!(orphaned_jobs.len(), 1);
    assert_eq!(orphaned_jobs[0].id, job_id);
    let jobs = store.list_jobs(10).await.unwrap();
    let j = jobs.iter().find(|j| j.job.id == job_id).unwrap();
    assert_eq!(j.job.state, hunter::domain::JobState::Killed);
    assert_eq!(j.job.killed_reason.as_deref(), Some("orphaned"));
    assert!(j.job.notes.as_deref().unwrap().contains("reconciled"));
    assert!(j.job.finished_at.is_some());
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

// -- 5. finalize_in_progress ------------------------------------------------

#[tokio::test]
async fn finalize_in_progress_resets_when_unchanged() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Finding #3 is "fixing"
    store
        .finalize_in_progress(
            3,
            hunter::domain::FindingStatus::Fixing,
            hunter::domain::FindingStatus::Queued,
        )
        .await
        .unwrap();
    let f = store.get_finding(3).await.unwrap().unwrap();
    assert_eq!(f.status, hunter::domain::FindingStatus::Queued);

    // If status is different, no-op
    store
        .set_finding_status(2, hunter::domain::FindingStatus::Fixing)
        .await
        .unwrap();
    store
        .set_finding_status(2, hunter::domain::FindingStatus::PrOpen)
        .await
        .unwrap(); // changed away
    store
        .finalize_in_progress(
            2,
            hunter::domain::FindingStatus::Fixing,
            hunter::domain::FindingStatus::Queued,
        )
        .await
        .unwrap();
    let f2 = store.get_finding(2).await.unwrap().unwrap();
    assert_eq!(f2.status, hunter::domain::FindingStatus::PrOpen); // untouched
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

// -- 7. record/clear attempt tracking ----------------------------------------

#[tokio::test]
async fn fix_attempt_streak_tracking() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool).await;
    seed_findings(&pool).await;
    let store = rw_store(&path).await;

    // Record same failure 3 times
    store.record_fix_attempt(1, "push failed").await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 1);
    assert_eq!(f.last_fix_failure.as_deref(), Some("push failed"));

    store.record_fix_attempt(1, "push failed").await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 2);

    // Different failure resets streak
    store
        .record_fix_attempt(1, "PR create failed")
        .await
        .unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 1);
    assert_eq!(f.last_fix_failure.as_deref(), Some("PR create failed"));

    // Clear resets everything
    store.clear_fix_attempts(1).await.unwrap();
    let f = store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(f.fix_attempts, 0);
    assert!(f.last_fix_failure.is_none());
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
    let store = rw_store(&path).await;
    let work_root = dir.subdir("work_root");
    let mut ids = Vec::new();
    for _ in 0..2 {
        let id = store
            .create_job(
                hunter::domain::RepoJobKind::TestGap.into(),
                1,
                None,
                Some(100_000),
                hunter::domain::JobState::Running,
                None,
                None,
            )
            .await
            .unwrap()
            .id;
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

    hunter::daemon::reconcile_and_log(&store, &work_root)
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

        hunter::daemon::reconcile_and_log(&store, &work_root)
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

    let result = hunter::daemon::reconcile_and_log(&store, &work_root).await;
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
             created_at, updated_at, fix_attempts, recheck_attempts)
         VALUES (?1, ?2, 'fp-bad', 'HIGH', 0.9, 'turned down', 'rejected', 1000, 1000, 0, 0)",
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

    let err = hunter::scheduler::run_hunt(&store, &cfg, &row, &backend, None)
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

/// The same refusal for the shared repo-level analysis body.
#[tokio::test]
async fn an_analysis_job_whose_suppressions_cannot_be_read_is_refused() {
    let dir = TempDir::new("refactor-bad-suppressions");
    let (_path, store, row, cfg) =
        repo_with_an_undecodable_finding(&dir, RepoJobKind::Refactor.as_str()).await;
    let backend = ScriptedBackend::noop();

    let err = hunter::scheduler::run_refactor(&store, &cfg, &row, &backend, None)
        .await
        .expect_err("an unreadable suppression list must refuse the job");

    assert!(
        format!("{err:#}").contains("refactor widget: loading suppressed refactor findings"),
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

/// A `test_gap` job also hands its worker the repo's bug findings, since it
/// may file a bug instead; an unreadable bug list refuses it just the same.
#[tokio::test]
async fn a_test_gap_whose_bug_suppressions_cannot_be_read_is_refused() {
    let dir = TempDir::new("test-gap-bad-bug-suppressions");
    let (_path, store, row, cfg) =
        repo_with_an_undecodable_finding(&dir, FindingType::Bug.as_str()).await;
    let backend = ScriptedBackend::noop();

    let err = hunter::scheduler::run_test_gap(&store, &cfg, &row, &backend, None)
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
