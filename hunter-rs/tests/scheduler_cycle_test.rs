#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_cycle`'s own outcomes, apart from the jobs it dispatches: a cycle
//! with nothing to do reports idle, and a cycle that fails reports the
//! error instead of propagating it, so the daemon loop that calls it
//! keeps running. Both leave an event, which is how the UI tells an idle
//! daemon from a crashing one. And a cycle leaves no job of its own
//! `running`, whichever way its runner returned.

mod support;

use hunter::backend::NullBackend;
use hunter::config::Config;
use hunter::domain::JobState;
use hunter::scheduler::run_cycle;
use hunter::store::Store;
use hunter::util::now_ms;
use support::{GitRepo, ScriptedBackend, TempDir};

/// A writable store on an empty schema, and a config whose disk gate
/// cannot deny (a real statvfs of the test machine's disk would make the
/// outcome depend on its free space).
async fn fixture(label: &str) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "cycle").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/r.git', '/nonexistent/alpha', 'github', \
                 'main', 0, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.min_free_disk_bytes = 0;
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

#[tokio::test]
async fn a_cycle_with_no_work_is_idle_and_logs_it() {
    let (_dir, store, cfg) = fixture("cycle-idle").await;

    let summary = run_cycle(&support::recovered(&store), &cfg, &NullBackend, None).await;

    assert_eq!(summary.kind, None);
    assert_eq!(summary.job_id, None);
    assert_eq!(summary.error, None);
    assert_eq!(
        summary.idle.as_deref(),
        Some("no queued findings, no enabled repos")
    );
    let event = store
        .last_cycle_event()
        .await
        .unwrap()
        .expect("cycle event");
    assert_eq!(event.message, "idle: nothing to do");
}

/// An unknown `force_repo` fails `pick_next`; the cycle turns that into
/// an error summary and a "cycle crashed" event rather than an `Err`.
#[tokio::test]
async fn a_crashing_cycle_returns_the_error_and_logs_it() {
    let (_dir, store, cfg) = fixture("cycle-crash").await;

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &NullBackend,
        Some("nosuch"),
    )
    .await;

    assert_eq!(summary.error.as_deref(), Some(r#"unknown repo "nosuch""#));
    assert_eq!(summary.kind, None);
    assert_eq!(summary.idle, None);
    let events = store.recent_events(10).await.unwrap();
    let crash = events
        .iter()
        .find(|e| e.kind == "error")
        .expect("error event");
    assert!(
        crash.message.starts_with("cycle crashed: ") && crash.message.contains("nosuch"),
        "{}",
        crash.message
    );
    assert!(
        events.iter().all(|e| e.kind != "cycle"),
        "a crash is not an idle cycle: {events:?}"
    );
}

/// A cycle whose hunt is due on repo 1 and returns between creating its
/// job and recording the outcome: its playbook is missing, so the prompt
/// fails after the job row and its tree exist. `seed` runs on the
/// database (at the returned path) before the store opens it.
async fn abandoning_hunt(
    label: &str,
    seed: &'static str,
) -> (TempDir, std::path::PathBuf, Store, Config) {
    let dir = TempDir::new(label);
    let origin = GitRepo::with_branch(&dir, "feature");
    let clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&origin.work, &clone).unwrap();
    let (path, pool) = support::fresh_pool(&dir, "cycle").await;
    // Repo 1: never hunted, every scan just run, so a hunt is what is due.
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, \
         last_test_gap_at, last_dep_update_at, last_refactor_at, last_modernization_at, \
         last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(origin.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(now_ms())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(seed).execute(&pool).await.unwrap();
    pool.close().await;
    let store = Store::connect(&path).await.unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.min_free_disk_bytes = 0;
    (dir, path, store, cfg)
}

/// A runner that returns between creating its job and recording the
/// outcome does not leave the row `running`: that row would show as the
/// current job, reserve budget and pin its tree until the daemon
/// restarted. The cycle settles every job it created and only those, so
/// a row that was already `running` when it began is left to whoever
/// owns it.
#[tokio::test]
async fn a_cycle_settles_the_jobs_its_runner_abandoned_and_no_others() {
    let (_dir, _, store, cfg) = abandoning_hunt(
        "cycle-abandoned",
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/b.git', '/nonexistent/beta', 'github', \
                 'main', 0, 1000); \
         INSERT INTO jobs (id, kind, repo_id, state, started_at) \
         VALUES (50, 'test_gap', 2, 'running', 1000);",
    )
    .await;

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;

    assert!(summary.error.is_some(), "the prompt failed: {summary:?}");
    let jobs = store.list_jobs(10).await.unwrap();
    let hunt = &jobs
        .iter()
        .find(|j| j.job.id != 50)
        .expect("the cycle created its hunt job")
        .job;
    assert_eq!(hunt.state, JobState::Killed, "{hunt:?}");
    assert_eq!(hunt.killed_reason.as_deref(), Some("orphaned"), "{hunt:?}");
    assert_eq!(
        hunt.notes.as_deref(),
        Some("abandoned -- its runner returned without recording an outcome"),
        "the row says it was the runner, not a dead process: {hunt:?}"
    );
    assert!(hunt.finished_at.is_some(), "{hunt:?}");
    let current = store.current_job().await.unwrap().map(|j| j.id);
    assert_eq!(
        current,
        Some(50),
        "a job the cycle did not create is not its to settle"
    );
}

/// A cycle that cannot settle what its runner abandoned says so as its
/// error, ahead of the runner's own, which is what puts the scheduler
/// state at `error` and the loop into its backoff instead of reading
/// "idle". What it could not settle is owed, not dropped: the next cycle
/// settles it before anything else, starts no work while that still
/// fails, and settles it once the store takes the writes again.
#[tokio::test]
async fn a_failed_settle_is_the_cycles_error_and_is_retried_before_the_next() {
    let (_dir, path, store, cfg) = abandoning_hunt(
        "cycle-settle-fails",
        "CREATE TRIGGER no_settle BEFORE UPDATE OF killed_reason ON jobs \
         WHEN NEW.killed_reason = 'orphaned' BEGIN SELECT RAISE(ABORT, 'settle refused'); END;",
    )
    .await;
    let recovered = support::recovered(&store);
    let backend = ScriptedBackend::noop();
    let cycle = || run_cycle(&recovered, &cfg, &backend, None);

    let failed = cycle().await;

    let error = failed.error.as_deref().unwrap_or_default();
    assert!(
        error.starts_with("settling the cycle's abandoned work failed, no new work starts ")
            && error.contains("settle refused"),
        "{failed:?}"
    );
    let (state, _) = hunter::daemon::describe_cycle(&failed);
    assert_eq!(state, "error");
    let events = store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.starts_with("settling the cycle's")),
        "{events:?}"
    );
    let abandoned = store
        .current_job()
        .await
        .unwrap()
        .expect("still running")
        .id;

    let still_failing = cycle().await;

    let error = still_failing.error.as_deref().unwrap_or_default();
    assert!(
        error.starts_with("settling an earlier cycle's abandoned work failed, no new work starts "),
        "{still_failing:?}"
    );
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1, "no work starts while a settle is owed");

    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    sqlx::raw_sql("DROP TRIGGER no_settle")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    cycle().await;

    let jobs = store.list_jobs(10).await.unwrap();
    let first = &jobs.iter().find(|j| j.job.id == abandoned).unwrap().job;
    assert_eq!(first.state, JobState::Killed, "{first:?}");
    assert_eq!(
        first.notes.as_deref(),
        Some("abandoned -- its runner returned without recording an outcome"),
        "{first:?}"
    );
    assert_eq!(store.current_job().await.unwrap().map(|j| j.id), None);
}
