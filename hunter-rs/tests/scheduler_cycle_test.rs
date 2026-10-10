#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_cycle`'s own outcomes, apart from the jobs it dispatches: a cycle
//! with nothing to do reports idle, and a cycle that fails reports the
//! error instead of propagating it, so the daemon loop that calls it
//! keeps running. Both leave an event, which is how the UI tells an idle
//! daemon from a crashing one.

mod support;

use hunter::backend::NullBackend;
use hunter::config::Config;
use hunter::scheduler::run_cycle;
use hunter::store::Store;
use support::TempDir;

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
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    cfg.min_free_disk_bytes = 0;
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

#[tokio::test]
async fn a_cycle_with_no_work_is_idle_and_logs_it() {
    let (_dir, store, cfg) = fixture("cycle-idle").await;

    let summary = run_cycle(&store, &cfg, &NullBackend, None).await;

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

    let summary = run_cycle(&store, &cfg, &NullBackend, Some("nosuch")).await;

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
