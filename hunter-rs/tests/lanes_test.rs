#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The daemon's two lanes: the admin lane's work does not wait for a job.
//!
//! PR sync, reaping and the workspace sweep used to run inside the cycle,
//! before its pick, so a 45-minute fix held every merged PR, every review
//! comment and every finished tree until it ended. They are the admin
//! lane's now (`daemon::admin_pass`), and run while the job lane's worker
//! does.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use hunter::activity::AdminStatus;
use hunter::config::Config;
use hunter::domain::{FindingStatus, JobState};
use hunter::store::{FindingInsert, Store};
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir};

/// How long a step may take before the test fails instead of hanging.
const BOUND: Duration = Duration::from_secs(20);

/// Two repos. `alpha` is enabled, with a real clone of a local origin,
/// and its `test_gap` is due, so a cycle runs it. `widget` is GitHub's
/// and disabled, so the rotation passes it by; one of its findings has an
/// open PR, which only sync reads (a clone is never needed for that).
async fn fixture(label: &str) -> (TempDir, Store, Config, i64) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "lanes").await;
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, 1000, ?3, ?3, ?3, ?3), \
                (2, 'widget', 'https://github.com/acme/widget', ?4, 'github', 'main', 0, 1000, \
                 ?3, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(future)
    .bind(dir.path().join("repo-2").to_string_lossy().to_string())
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
    let (fid, _) = store
        .upsert_finding(
            2,
            &FindingInsert {
                fingerprint: "fp-open-pr".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "a bug with a PR".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store
        .set_finding_pr_open(fid, "https://github.com/acme/widget/pull/7")
        .await
        .unwrap();
    (dir, store, cfg, fid)
}

/// While the job lane's worker runs, an admin pass syncs the open PR to
/// its merge, sweeps, and returns; the job is still running when it does,
/// and finishes once its worker does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_admin_pass_runs_while_a_job_runs() {
    let bins = FakeBins::acquire("lanes");
    bins.ok(
        "gh",
        r#"{"state":"MERGED","mergedAt":"2026-01-02T03:04:05Z","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let (_dir, store, cfg, fid) = fixture("lanes").await;
    let started = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let (worker_started, worker_release) = (started.clone(), release.clone());
    // Bounded: a failed assertion below never releases it, and an
    // unbounded wait would keep the test binary alive.
    let backend = ScriptedBackend::new(move |_| {
        worker_started.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + BOUND;
        while !worker_release.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        support::done()
    });

    let mut jobs = support::jobs(&store);
    let job_cfg = cfg.clone();
    let cycle = tokio::spawn(async move {
        hunter::scheduler::run_cycle(&mut jobs, &job_cfg, &backend, None).await
    });
    let waited = tokio::time::timeout(BOUND, async {
        while !started.load(Ordering::SeqCst) && !cycle.is_finished() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if waited.is_err() || !started.load(Ordering::SeqCst) {
        let summary = if cycle.is_finished() {
            format!("{:?}", cycle.await)
        } else {
            "still running".to_owned()
        };
        panic!("the job lane's worker never started; the cycle: {summary}");
    }
    let running = store
        .current_job()
        .await
        .unwrap()
        .expect("a job is running");

    let status = AdminStatus::default();
    let notes_lock = tokio::sync::Mutex::new(());
    let (_keep, running_on) = tokio::sync::watch::channel(false);
    let pass = tokio::time::timeout(
        BOUND,
        hunter::daemon::admin_pass(
            &support::admin(&store),
            &cfg,
            &notes_lock,
            &status,
            &running_on,
        ),
    )
    .await
    .expect("the admin pass waited for the running job");

    let sync = pass.sync.expect("the open PR was synced");
    assert_eq!((sync.merged, sync.errors), (1, 0), "{sync:?}");
    assert!(sync.made_work(), "a merge is work for the job lane");
    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Merged);
    assert_eq!(
        status.current(),
        None,
        "no task shows once the pass is over"
    );
    assert_eq!(prsync_events(&store).await, 1, "the merge is in the log");
    assert_eq!(
        store.current_job().await.unwrap().map(|j| j.id),
        Some(running.id),
        "the job was still running when the pass returned"
    );

    release.store(true, Ordering::SeqCst);
    let summary = tokio::time::timeout(BOUND, cycle)
        .await
        .expect("the cycle never finished")
        .unwrap();
    assert_eq!(summary.job_id, Some(running.id), "{summary:?}");
    assert_ne!(summary.state, Some(JobState::Running), "{summary:?}");
    assert!(store.current_job().await.unwrap().is_none());
}

/// How many `prsync` events the log holds.
async fn prsync_events(store: &Store) -> usize {
    store
        .recent_events(50)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == "prsync")
        .count()
}

/// A pass that only re-reads an open PR nobody touched makes no work for
/// the job lane and leaves the log alone: it runs every five minutes.
#[tokio::test]
async fn a_pass_that_changes_nothing_makes_no_work_and_logs_nothing() {
    let bins = FakeBins::acquire("lanes-quiet");
    bins.ok(
        "gh",
        r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let (_dir, store, cfg, fid) = fixture("lanes-quiet").await;

    let (_keep, running_on) = tokio::sync::watch::channel(false);
    let pass = hunter::daemon::admin_pass(
        &support::admin(&store),
        &cfg,
        &tokio::sync::Mutex::new(()),
        &AdminStatus::default(),
        &running_on,
    )
    .await;

    let sync = pass.sync.expect("the open PR was synced");
    assert_eq!((sync.synced, sync.errors), (1, 0), "{sync:?}");
    assert!(!sync.made_work(), "{sync:?}");
    assert_eq!(prsync_events(&store).await, 0);
    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::PrOpen);
}

/// Each change a sync can make is work for the job lane on its own; what
/// a sync merely saw, or failed at, is not.
#[test]
fn a_sync_made_work_when_it_ended_flagged_or_requeued_something() {
    use hunter::scheduler::SyncResult;
    let seen = SyncResult {
        synced: 3,
        attention: 2,
        errors: 1,
        ..SyncResult::default()
    };
    assert!(!seen.made_work(), "{seen:?}");
    for (what, made) in [
        (
            "merged",
            SyncResult {
                merged: 1,
                ..SyncResult::default()
            },
        ),
        (
            "closed",
            SyncResult {
                closed: 1,
                ..SyncResult::default()
            },
        ),
        (
            "raised",
            SyncResult {
                raised: 1,
                ..SyncResult::default()
            },
        ),
        (
            "requeued",
            SyncResult {
                requeued: 1,
                ..SyncResult::default()
            },
        ),
    ] {
        assert!(made.made_work(), "{what}: {made:?}");
    }
}

/// The status page's admin row names a task only while it runs: from
/// `begin` until its guard is dropped, however the task returned.
#[test]
fn an_admin_task_shows_only_while_its_guard_lives() {
    use hunter::types::AdminTask;
    let status = AdminStatus::default();
    let reader = status.clone();
    assert_eq!(reader.current(), None);
    let guard = status.begin(AdminTask::Sweep);
    assert_eq!(reader.current().map(|a| a.task), Some(AdminTask::Sweep));
    drop(guard);
    assert_eq!(reader.current(), None);
}

/// The admin lane fetches every enabled clone ahead of its jobs: a commit
/// pushed to `alpha`'s origin is in its clone's `origin/main` after one
/// pass, with no job involved. A repo with no clone is left to its first
/// hunt, enabled or not: fetch-ahead never clones.
#[tokio::test]
async fn fetch_ahead_brings_every_enabled_clone_up_to_its_origin() {
    let (dir, store, _cfg, _fid) = fixture("lanes-fetch-ahead").await;
    store
        .update_repo(
            2,
            &hunter::store::RepoUpdate {
                enabled: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    let widget = store.get_repo_by_id(2).await.unwrap().unwrap();
    let pusher = dir.path().join("pusher");
    support::git(
        dir.path(),
        &["clone", "-q", &alpha.url, &pusher.to_string_lossy()],
    );
    support::git(
        &pusher,
        &["commit", "--allow-empty", "-qm", "upstream moved"],
    );
    support::git(&pusher, &["push", "-q", "origin", "HEAD:main"]);
    let upstream = support::git(&pusher, &["rev-parse", "HEAD"]);
    let clone = std::path::Path::new(&alpha.path);
    assert_ne!(
        support::git(clone, &["rev-parse", "origin/main"]),
        upstream,
        "fixture: the clone starts behind its origin"
    );

    let (_keep, running_on) = tokio::sync::watch::channel(false);
    let done = hunter::scheduler::fetch_ahead(&store, &running_on).await;

    assert_eq!(
        done,
        hunter::scheduler::FetchAhead {
            fetched: 1,
            failed: 0
        }
    );
    assert_eq!(support::git(clone, &["rev-parse", "origin/main"]), upstream);
    assert!(
        !std::path::Path::new(&widget.path).exists(),
        "fetch-ahead cloned a repo"
    );
}
