#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The daemon's two lanes: the admin lane's work does not wait for a job,
//! and its duties do not wait for each other.
//!
//! PR sync, reaping and the workspace sweep used to run inside the cycle,
//! before its pick, so a 45-minute fix held every merged PR, every review
//! comment and every finished tree until it ended. They are the admin
//! lane's duties now (`daemon::spawn_admin_lane`), each its own task, and
//! run while the job lane's worker does.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hunter::activity::AdminStatus;
use hunter::config::Config;
use hunter::daemon::{ADMIN_DUTIES, DutyRun, DutyTimer};
use hunter::domain::{FindingStatus, JobState};
use hunter::store::{FindingInsert, Store};
use hunter::types::AdminTask;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir};

/// How long a step may take before the test fails instead of hanging.
const BOUND: Duration = Duration::from_secs(20);

/// Two repos. `alpha` is enabled, with a real clone of a local origin,
/// and its `test_gap` is due, so a cycle runs it. `widget` is GitHub's
/// and disabled, so the rotation passes it by; one of its findings has an
/// open PR, which only sync reads (a clone is never needed for that).
async fn fixture(label: &str) -> (TempDir, Store, Config, i64) {
    fixture_with(label, false).await
}

/// [`fixture`], with `alpha`'s dependency scan due when `deps_due`; then
/// `alpha` is GitLab's, so that scan asks `gh` for no token.
async fn fixture_with(label: &str, deps_due: bool) -> (TempDir, Store, Config, i64) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "lanes").await;
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let future = hunter::util::now_ms() + 999_999;
    let (alpha_deps, alpha_forge) = if deps_due {
        (0, "gitlab")
    } else {
        (future, "github")
    };
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, ?6, 'main', 1, 1000, ?3, 1000, ?5, ?3, ?3, ?3), \
                (2, 'widget', 'https://github.com/acme/widget', ?4, 'github', 'main', 0, 1000, \
                 ?3, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(future)
    .bind(dir.path().join("repo-2").to_string_lossy().to_string())
    .bind(alpha_deps)
    .bind(alpha_forge)
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

/// Run every admin duty once, one after another, as the daemon's tasks
/// would each run it, and return the PR sync's counts.
async fn every_duty_once(
    store: &Store,
    cfg: &Config,
    status: &AdminStatus,
) -> Option<hunter::scheduler::SyncResult> {
    let admin = support::admin(store);
    let notes_lock = tokio::sync::Mutex::new(());
    let (_keep, running_on) = tokio::sync::watch::channel(false);
    let mut sync = None;
    for task in ADMIN_DUTIES {
        let run =
            hunter::daemon::admin_duty(&admin, cfg, &notes_lock, status, task, &running_on).await;
        if let DutyRun::Synced(s) = run {
            sync = s;
        }
    }
    sync
}

/// While the job lane's worker runs, the admin duties sync the open PR to
/// its merge, sweep, and return; the job is still running when they do,
/// and finishes once its worker does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admin_duties_run_while_a_job_runs() {
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
    let sync = tokio::time::timeout(BOUND, every_duty_once(&store, &cfg, &status))
        .await
        .expect("an admin duty waited for the running job")
        .expect("the open PR was synced");

    assert_eq!((sync.merged, sync.errors), (1, 0), "{sync:?}");
    assert!(sync.made_work(), "a merge is work for the job lane");
    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Merged);
    assert_eq!(status.current(), [], "no duty shows once its run is over");
    assert_eq!(prsync_events(&store).await, 1, "the merge is in the log");
    assert_eq!(
        store.current_job().await.unwrap().map(|j| j.id),
        Some(running.id),
        "the job was still running when the duties returned"
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

/// A sync that only re-reads an open PR nobody touched makes no work for
/// the job lane and leaves the log alone: it runs every five minutes.
#[tokio::test]
async fn a_sync_that_changes_nothing_makes_no_work_and_logs_nothing() {
    let bins = FakeBins::acquire("lanes-quiet");
    bins.ok(
        "gh",
        r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let (_dir, store, cfg, fid) = fixture("lanes-quiet").await;

    let sync = every_duty_once(&store, &cfg, &AdminStatus::default())
        .await
        .expect("the open PR was synced");

    assert_eq!((sync.synced, sync.errors), (1, 0), "{sync:?}");
    assert!(!sync.made_work(), "{sync:?}");
    assert_eq!(prsync_events(&store).await, 0);
    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::PrOpen);
}

/// A PR sync asked for while a Renovate scan runs reads the PR at once:
/// each admin duty is its own task, so the scan -- up to two minutes per
/// repo -- holds up no merge. Run as one pass, the sync would wait for
/// the scan to end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pr_sync_does_not_wait_for_a_dependency_scan() {
    let bins = FakeBins::acquire("lanes-apart");
    let open = r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    let merged = r#"{"state":"MERGED","mergedAt":"2026-01-02T03:04:05Z","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    bins.ok("gh", open);
    let (dir, store, cfg, fid) = fixture_with("lanes-apart", true).await;
    let scanning = dir.path().join("renovate-started");
    let release = dir.path().join("renovate-release");
    bins.script(
        "renovate",
        &format!(
            // Bounded, so a failing assertion below cannot leave it waiting.
            "touch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done\nexit 1",
            scanning.display(),
            release.display()
        ),
    );

    let scheduler = hunter::server::SchedulerHandle::default();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let duties = hunter::daemon::spawn_admin_lane(
        hunter::daemon::AdminLane {
            admin: support::admin(&store),
            cfg: cfg.clone(),
            notes_lock: Arc::default(),
            status: scheduler.admin.clone(),
            pr_sync_wake: scheduler.pr_sync_wake.clone(),
            pr_sync_passes: hunter::daemon::PrSyncPasses::default(),
            job_wake: scheduler.wake.clone(),
        },
        &stopping,
    );
    tokio::time::timeout(BOUND, async {
        while !scanning.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the dependency scan never started");

    // The PR merges while Renovate runs, and the operator asks for a sync.
    bins.ok("gh", merged);
    scheduler.pr_sync_wake.notify_one();
    let synced = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let f = store.get_finding(fid).await.unwrap().unwrap();
            if f.status == FindingStatus::Merged {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let scan_still_running = scheduler
        .admin
        .current()
        .iter()
        .any(|a| a.task == AdminTask::DependencyScan);

    std::fs::write(&release, "").unwrap();
    stop.send_replace(true);
    for duty in duties {
        tokio::time::timeout(BOUND, duty)
            .await
            .expect("an admin duty did not stop")
            .unwrap();
    }
    assert!(synced.is_ok(), "the PR sync waited for the dependency scan");
    assert!(
        scan_still_running,
        "fixture: the scan ended before the sync, so it proves nothing"
    );
}

/// The admin lane, started as the daemon starts it, with `passes` counting
/// its PR syncs; stopped by sending `true` on the returned sender.
fn start_admin_lane(
    store: &Store,
    cfg: &Config,
    passes: &hunter::daemon::PrSyncPasses,
) -> (
    hunter::server::SchedulerHandle,
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
    Vec<tokio::task::JoinHandle<()>>,
) {
    let scheduler = hunter::server::SchedulerHandle::default();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let duties = hunter::daemon::spawn_admin_lane(
        hunter::daemon::AdminLane {
            admin: support::admin(store),
            cfg: cfg.clone(),
            notes_lock: Arc::default(),
            status: scheduler.admin.clone(),
            pr_sync_wake: scheduler.pr_sync_wake.clone(),
            pr_sync_passes: passes.clone(),
            job_wake: scheduler.wake.clone(),
        },
        &stopping,
    );
    (scheduler, stop, stopping, duties)
}

async fn stop_admin_lane(
    stop: &tokio::sync::watch::Sender<bool>,
    duties: Vec<tokio::task::JoinHandle<()>>,
) {
    stop.send_replace(true);
    for duty in duties {
        tokio::time::timeout(BOUND, duty)
            .await
            .expect("an admin duty did not stop")
            .unwrap();
    }
}

/// The job lane's first pick waits for the first PR sync: a PR merged
/// while the daemon was down is recorded before anything is picked, so no
/// engage is started from the `pr_open` row the database still has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_pick_waits_for_a_sync_to_record_what_the_forge_did() {
    let bins = FakeBins::acquire("lanes-first-sync");
    let merged = r#"{"state":"MERGED","mergedAt":"2026-01-02T03:04:05Z","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    bins.ok("gh", merged);
    let (_dir, store, cfg, fid) = fixture("lanes-first-sync").await;
    let passes = hunter::daemon::PrSyncPasses::default();
    let (_scheduler, stop, stopping, duties) = start_admin_lane(&store, &cfg, &passes);

    tokio::time::timeout(BOUND, passes.first(&stopping))
        .await
        .expect("the first PR sync never finished");
    let status = store.get_finding(fid).await.unwrap().unwrap().status;

    stop_admin_lane(&stop, duties).await;
    assert_eq!(status, FindingStatus::Merged);
}

/// After a cycle that met a PR the forge has ended, the job lane picks
/// again only once a sync pass begun after that cycle has recorded it;
/// picking sooner would pick the same engage from the same stale row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pick_after_an_ended_pr_waits_for_a_sync_begun_after_it() {
    let bins = FakeBins::acquire("lanes-next-sync");
    let open = r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    let merged = r#"{"state":"MERGED","mergedAt":"2026-01-02T03:04:05Z","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    bins.ok("gh", open);
    let (_dir, store, cfg, fid) = fixture("lanes-next-sync").await;
    let passes = hunter::daemon::PrSyncPasses::default();
    let (scheduler, stop, stopping, duties) = start_admin_lane(&store, &cfg, &passes);
    tokio::time::timeout(BOUND, passes.first(&stopping))
        .await
        .expect("the first PR sync never finished");
    let before = store.get_finding(fid).await.unwrap().unwrap().status;

    // The PR merges; the admin lane's next tick is minutes away.
    bins.ok("gh", merged);
    let synced = tokio::time::timeout(BOUND, passes.next(&scheduler.pr_sync_wake, &stopping))
        .await
        .expect("the woken PR sync never finished");
    let after = store.get_finding(fid).await.unwrap().unwrap().status;

    stop_admin_lane(&stop, duties).await;
    assert_eq!(before, FindingStatus::PrOpen);
    assert_eq!(after, FindingStatus::Merged);
    assert!(synced, "a pass finished");
}

/// A wait for the next pass that shutdown ends says no pass finished: the
/// job lane then counts nothing against the engage it waited for.
#[tokio::test]
async fn a_wait_for_the_next_sync_ended_by_shutdown_saw_no_pass() {
    let passes = hunter::daemon::PrSyncPasses::default();
    let wake = tokio::sync::Notify::new();
    let (stop, stopping) = tokio::sync::watch::channel(false);
    stop.send_replace(true);
    let synced = tokio::time::timeout(BOUND, passes.next(&wake, &stopping))
        .await
        .expect("shutdown did not end the wait");
    assert!(!synced);
}

/// A pass that stopped for shutdown before reading every open PR is no
/// pass for the job lane's wait: the PR it waited for may be one the pass
/// never reached, and counting it would charge that engage a failure for
/// a sync that never looked. The wait here has its own latch, so only
/// the lane's shutdown cuts the pass short.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_pass_cut_short_by_shutdown_is_not_a_finished_pass() {
    let bins = FakeBins::acquire("lanes-cut-short");
    let (dir, store, cfg, fid) = fixture("lanes-cut-short").await;
    let rid = store.get_finding(fid).await.unwrap().unwrap().repo_id;
    let extra = hunter::scheduler::PR_SYNC_CONCURRENCY;
    for n in 0..extra {
        let (id, _) = store
            .upsert_finding(
                rid,
                &FindingInsert {
                    fingerprint: format!("fp-cut-{n}"),
                    file: "src/lib.rs".to_owned(),
                    summary: "another PR".to_owned(),
                    ..Default::default()
                },
                "bug",
                None,
            )
            .await
            .unwrap();
        store
            .set_finding_pr_open(
                id,
                &format!("https://github.com/acme/widget/pull/{}", 20 + n),
            )
            .await
            .unwrap();
    }
    let started = dir.path().join("started");
    let release = dir.path().join("release");
    std::fs::create_dir_all(&started).unwrap();
    std::fs::write(&release, "").unwrap();
    let open = r#"{"state":"OPEN","mergeable":"MERGEABLE","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;
    bins.script(
        "gh",
        &format!(
            "touch '{started}/'$$\n\
             i=0\n\
             while [ ! -e '{release}' ] && [ $i -lt 500 ]; do sleep 0.02; i=$((i+1)); done\n\
             cat <<'__PR_EOF__'\n{open}\n__PR_EOF__\nexit 0",
            started = started.display(),
            release = release.display(),
        ),
    );
    let passes = hunter::daemon::PrSyncPasses::default();
    let (scheduler, stop, stopping, duties) = start_admin_lane(&store, &cfg, &passes);
    tokio::time::timeout(BOUND, passes.first(&stopping))
        .await
        .expect("the first PR sync never finished");

    // The next pass holds its first reads until released; one PR is left
    // for when a read ends.
    std::fs::remove_file(&release).unwrap();
    for e in std::fs::read_dir(&started).unwrap() {
        std::fs::remove_file(e.unwrap().path()).unwrap();
    }
    let (_never, not_stopping) = tokio::sync::watch::channel(false);
    let wake = scheduler.pr_sync_wake.clone();
    let waited = {
        let passes = passes.clone();
        tokio::spawn(async move { passes.next(&wake, &not_stopping).await })
    };
    let live = tokio::time::timeout(BOUND, async {
        while std::fs::read_dir(&started).unwrap().count() < extra {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    stop.send_replace(true);
    std::fs::write(&release, "").unwrap();
    let synced = tokio::time::timeout(BOUND, waited)
        .await
        .expect("the cut-short pass never finished")
        .unwrap();

    stop_admin_lane(&stop, duties).await;
    assert!(live.is_ok(), "fixture: the pass never had its reads out");
    assert_eq!(
        std::fs::read_dir(&started).unwrap().count(),
        extra,
        "fixture: the pass read every PR, so it was not cut short"
    );
    assert!(!synced, "a pass cut short by shutdown counted as a sync");
}

/// A duty whose run panics runs again at its next tick, and the panic is
/// in the log: one bad run cannot end PR sync, or any duty, for the rest
/// of the daemon's life while the job lane goes on.
#[tokio::test]
async fn a_duty_whose_run_panics_runs_again_and_the_panic_is_logged() {
    let dir = TempDir::new("lanes-panic");
    let (_db, store) = support::fresh_store(&dir, "lanes-panic").await;
    let runs = Arc::new(AtomicUsize::new(0));
    let run = {
        let runs = Arc::clone(&runs);
        move || {
            let runs = Arc::clone(&runs);
            async move {
                assert_ne!(runs.fetch_add(1, Ordering::SeqCst), 0, "a bad first run");
                DutyRun::Swept
            }
        }
    };
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let duty = tokio::spawn(hunter::daemon::supervise_duty(
        store.clone(),
        AdminTask::Sweep,
        DutyTimer {
            interval: Duration::from_millis(20),
            wake: None,
        },
        Arc::new(tokio::sync::Notify::new()),
        stopping,
        run,
    ));

    let ran_again = tokio::time::timeout(BOUND, async {
        while runs.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    stop.send_replace(true);
    let ended = tokio::time::timeout(BOUND, duty)
        .await
        .expect("the duty did not stop");

    assert!(ended.is_ok(), "the duty's own task died with its run");
    assert!(ran_again.is_ok(), "the duty never ran again");
    let logged = store.recent_events(50).await.unwrap();
    assert!(logged.iter().any(|e| e.kind == "error"), "{logged:?}");
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

/// A duty's run is work for the job lane when its sync made some, or its
/// dependency scan handed a repo to the AI scan; reaping, fetching and
/// sweeping never are.
#[test]
fn a_duty_made_work_only_through_a_sync_or_a_scan_fallback() {
    use hunter::scheduler::{DepScans, FetchAhead, SyncResult};
    let merged = SyncResult {
        merged: 1,
        ..SyncResult::default()
    };
    for (run, made) in [
        (DutyRun::Synced(None), false),
        (DutyRun::Synced(Some(SyncResult::default())), false),
        (DutyRun::Synced(Some(merged)), true),
        (
            DutyRun::Scanned(DepScans {
                scanned: 3,
                failed: 1,
                ..DepScans::default()
            }),
            false,
        ),
        (
            DutyRun::Scanned(DepScans {
                fallbacks: 1,
                ..DepScans::default()
            }),
            true,
        ),
        (DutyRun::Reaped(2), false),
        (
            DutyRun::Fetched(FetchAhead {
                fetched: 4,
                ..FetchAhead::default()
            }),
            false,
        ),
        (DutyRun::Swept, false),
    ] {
        assert_eq!(run.made_work(), made, "{run:?}");
    }
}

/// A run that made work wakes the job lane; one that made none leaves it
/// asleep.
#[tokio::test]
async fn a_duty_wakes_the_job_lane_only_after_a_run_that_made_work() {
    let dir = TempDir::new("lanes-wake");
    let (_db, store) = support::fresh_store(&dir, "lanes-wake").await;
    let job_wake = Arc::new(tokio::sync::Notify::new());
    let supervise = |made: bool| {
        let (stop, stopping) = tokio::sync::watch::channel(false);
        let run = move || async move {
            DutyRun::Scanned(hunter::scheduler::DepScans {
                fallbacks: usize::from(made),
                ..Default::default()
            })
        };
        let duty = tokio::spawn(hunter::daemon::supervise_duty(
            store.clone(),
            AdminTask::DependencyScan,
            DutyTimer {
                interval: BOUND,
                wake: None,
            },
            Arc::clone(&job_wake),
            stopping,
            run,
        ));
        (stop, duty)
    };
    let short = Duration::from_millis(300);

    let (stop, duty) = supervise(false);
    let woken = tokio::time::timeout(short, job_wake.notified()).await;
    stop.send_replace(true);
    duty.await.unwrap();
    assert!(woken.is_err(), "a run that made no work woke the job lane");

    let (stop, duty) = supervise(true);
    let woken = tokio::time::timeout(BOUND, job_wake.notified()).await;
    stop.send_replace(true);
    duty.await.unwrap();
    assert!(
        woken.is_ok(),
        "a run that made work left the job lane asleep"
    );
}

/// The status page's admin row names each duty only while it runs, from
/// `begin` until its guard is dropped, however the duty returned -- also
/// while others run beside it and end first.
#[test]
fn an_admin_duty_shows_only_while_its_guard_lives() {
    let status = AdminStatus::default();
    let reader = status.clone();
    let tasks = || reader.current().iter().map(|a| a.task).collect::<Vec<_>>();
    assert_eq!(tasks(), []);
    let sweep = status.begin(AdminTask::Sweep);
    let sync = status.begin(AdminTask::PrSync);
    assert_eq!(tasks(), [AdminTask::PrSync, AdminTask::Sweep]);
    drop(sweep);
    assert_eq!(tasks(), [AdminTask::PrSync], "the sync still runs");
    drop(sync);
    assert_eq!(tasks(), []);
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
    let admin = support::admin(&store);
    let hour = Duration::from_hours(1);
    let done = hunter::scheduler::fetch_ahead(&admin, hour, &running_on).await;

    assert_eq!(
        done,
        hunter::scheduler::FetchAhead {
            fetched: 1,
            fresh: 0,
            failed: 0
        }
    );
    assert_eq!(support::git(clone, &["rev-parse", "origin/main"]), upstream);
    assert!(
        !std::path::Path::new(&widget.path).exists(),
        "fetch-ahead cloned a repo"
    );
}

/// Fetch-ahead leaves a clone fetched in full less than its `min_age` ago
/// alone, and fetches it again once that has passed: a fetch holds the
/// clone, which a job starting on the repo waits for, so fetching every
/// clone on every run would cost the job lane for nothing.
#[tokio::test]
async fn fetch_ahead_leaves_a_recently_fetched_clone_alone() {
    let (dir, store, _cfg, _fid) = fixture("lanes-fetch-fresh").await;
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    let clone = std::path::Path::new(&alpha.path);
    let (_keep, running_on) = tokio::sync::watch::channel(false);
    let admin = support::admin(&store);
    let fetch = |min_age| hunter::scheduler::fetch_ahead(&admin, min_age, &running_on);
    let hour = Duration::from_hours(1);
    assert_eq!(
        fetch(hour).await.fetched,
        1,
        "never fetched in this process"
    );

    let pusher = dir.path().join("pusher");
    support::git(
        dir.path(),
        &["clone", "-q", &alpha.url, &pusher.to_string_lossy()],
    );
    support::git(&pusher, &["commit", "--allow-empty", "-qm", "moved again"]);
    support::git(&pusher, &["push", "-q", "origin", "HEAD:main"]);
    let upstream = support::git(&pusher, &["rev-parse", "HEAD"]);

    let again = fetch(hour).await;
    assert_eq!((again.fetched, again.fresh), (0, 1), "{again:?}");
    assert_ne!(
        support::git(clone, &["rev-parse", "origin/main"]),
        upstream,
        "a fresh clone was fetched"
    );

    let stale = fetch(Duration::ZERO).await;
    assert_eq!((stale.fetched, stale.fresh), (1, 0), "{stale:?}");
    assert_eq!(support::git(clone, &["rev-parse", "origin/main"]), upstream);
}
