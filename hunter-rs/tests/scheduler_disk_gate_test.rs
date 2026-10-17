#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The disk gate: a cycle whose `work_root` is short of space starts no
//! job.
//!
//! On 2026-09-29 the disk filled and every fix job failed at checkout,
//! and a restart during one of those checkouts left a locked worktree that
//! wedged the fix queue. The gate denies the cycle before anything is
//! picked, and the work is still there once space is back.

mod support;

use hunter::config::Config;
use hunter::scheduler::{disk_denial, pick_next, run_cycle};
use hunter::store::Store;
use support::{GitRepo, ScriptedBackend, TempDir};

/// An enabled repo with a real clone, whose `test_gap` is due: a cycle
/// with room would run it.
async fn fixture(label: &str) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "disk").await;
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, 1000, ?3, ?3, ?3, ?3)",
    )
    .bind("https://github.com/acme/widget")
    .bind(clone.to_string_lossy().to_string())
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    std::fs::write(
        dir.subdir("playbooks").join("test_gap.md"),
        "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

/// A short disk denies the cycle before its pick: no job row, no worker,
/// and the work still waiting. (PR sync is the admin lane's, which the
/// disk gate does not stop: review feedback need not wait for space.)
#[tokio::test]
async fn a_cycle_short_of_disk_is_denied_before_any_job_starts() {
    let (_dir, store, mut cfg) = fixture("disk-gate").await;
    // More than any filesystem has free.
    cfg.min_free_disk_bytes = u64::MAX;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start on a full disk"));

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;
    let after = hunter::util::now_ms();

    let denied = summary.denied.as_deref().unwrap_or_default();
    assert!(denied.starts_with("disk:"), "{summary:?}");
    assert_eq!(summary.job_id, None, "{summary:?}");
    let five_min = 5 * 60_000;
    assert!(
        summary
            .retry_at
            .is_some_and(|t| (before + five_min..=after + five_min).contains(&t)),
        "a disk denial is looked at again in five minutes: {summary:?}"
    );
    assert!(
        store.list_jobs(100).await.unwrap().is_empty(),
        "no job row is created"
    );
    assert!(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap()
            .is_some(),
        "the work is still waiting"
    );
}

/// The threshold is 1 GiB: one byte less denies, exactly 1 GiB runs.
#[test]
fn the_default_threshold_is_one_gib_inclusive() {
    let dir = TempDir::new("disk-threshold");
    let cfg = support::load_config(dir.path());
    let gib: u64 = 1 << 30;

    assert!(disk_denial(&cfg, gib - 1).is_some());
    assert_eq!(disk_denial(&cfg, gib), None);
}
