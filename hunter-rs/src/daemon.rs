//! Daemon — UI server, usage prober, and the scheduler's two lanes, one
//! process. Port of server.py daemon/_`compute_sleep_s`/
//! _`usage_prober_loop`/_`describe_cycle`; the lockfile
//! (`_acquire_lockfile`) and the startup reconcile are [`crate::recovery`].
//!
//! The scheduler runs as two lanes side by side, each with its own part of
//! the startup proof ([`crate::recovery::Recovered::into_lanes`]):
//!
//! - The job lane runs cycles ([`crate::scheduler::run_cycle`]): pick the
//!   next work, run its job. Only it holds the
//!   [`JobAuthority`](crate::recovery::JobAuthority), so only it creates
//!   jobs and claims fixes, one cycle at a time.
//! - The admin lane runs [`admin_pass`] on a timer: PR sync, reaping
//!   deleted repos, the workspace sweep. Only it holds the
//!   [`AdminAuthority`], so only it syncs PRs. None of its work spends
//!   tokens, so none of it waits for a job any more, and a pause, which
//!   stops new jobs, does not stop it.
//!
//! What the two touch at once is guarded where it lives, not by running
//! them apart: a finding's status moves only by guarded transitions, a
//! PR's `pr_state` columns have one writer per group, a clone changes only
//! under its permit, and a tree a runner still uses is leased. The lanes
//! meet through the database: a sync pass that made work wakes the job
//! lane, which picks from what the sync recorded.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Notify, watch};

use crate::activity::AdminStatus;
use crate::backend::Backend;
use crate::config::Config;
use crate::domain::JobState;
use crate::recovery::AdminAuthority;
use crate::scheduler::{CycleSummary, DepScans, FetchAhead, SyncResult};
use crate::server::{AppState, router};
use crate::store::Store;
use crate::types::AdminTask;

const USAGE_PROBE_TICK_S: u64 = 60;
/// How long to let in-flight HTTP responses finish once the scheduler has
/// stopped. Bounded so a wedged client cannot keep the daemon alive past
/// the unit's `TimeoutStopSec`, which would turn a graceful stop into a
/// SIGKILL.
const UI_DRAIN: std::time::Duration = std::time::Duration::from_secs(5);
/// How often the admin lane runs a pass when nothing wakes it sooner.
pub const ADMIN_INTERVAL: std::time::Duration = std::time::Duration::from_mins(5);

/// Install a signal handler now; `None` (logged) if the OS refuses.
fn install_signal(kind: SignalKind, name: &str) -> Option<Signal> {
    signal(kind)
        .inspect_err(|e| tracing::error!("cannot install {name} handler: {e}"))
        .ok()
}

/// The next delivery of `sig`; never, if its handler failed to install.
async fn next_signal(sig: Option<Signal>) {
    match sig {
        Some(mut sig) => {
            sig.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// Release finished chains' trees and reclaim old workspaces and the
/// previous layout's leftovers (`workspace::sweep`). Safe beside a running
/// job: the sweep releases no tree a live job or a runner's lease keeps,
/// and changes a clone only under its permit. Best effort, since a failed
/// pass is retried on the next.
async fn sweep_workspaces(store: &Store, work_root: &Path) {
    let swept = store
        .lane(async |lane| crate::workspace::sweep(lane, work_root, crate::util::now_ms()).await)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    match swept {
        Ok(r) if r != crate::workspace::SweepReport::default() => {
            tracing::info!("workspace sweep: {r:?}");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("workspace sweep failed: {e}"),
    }
}

/// What one [`admin_pass`] did.
#[derive(Debug, Default)]
pub struct AdminPass {
    /// The PR sync's counts; `None` when another pass was syncing.
    pub sync: Option<SyncResult>,
    /// The fetch-ahead's counts; `None` when the pass stopped before it.
    pub fetched: Option<FetchAhead>,
    /// The dependency scans' counts; `None` when the pass stopped before
    /// them.
    pub dependencies: Option<DepScans>,
}

impl AdminPass {
    /// Whether this pass gave the job lane work it did not have: a PR's
    /// end or flag ([`SyncResult::made_work`]), or a repo whose AI
    /// dependency scan is now due.
    pub fn made_work(&self) -> bool {
        self.sync.as_ref().is_some_and(SyncResult::made_work)
            || self.dependencies.is_some_and(|d| d.fallbacks > 0)
    }
}

/// One pass of the admin lane: sync every open PR, remove what deleted
/// repos left, fetch every enabled clone ahead of its jobs, run the
/// zero-token Renovate scan of every repo whose dependencies are due,
/// sweep the workspaces. Each task shows on the status page while it
/// runs (`status`). None of it creates a job; that takes the job lane's
/// authority, which this does not have.
///
/// Once `shutdown` is set the pass stops at the next step, and the sync at
/// the next PR: the daemon waits for the pass, and a pass over many open
/// PRs is many forge round trips.
pub async fn admin_pass(
    admin: &AdminAuthority,
    cfg: &Config,
    notes_lock: &tokio::sync::Mutex<()>,
    status: &AdminStatus,
    shutdown: &watch::Receiver<bool>,
) -> AdminPass {
    let store = admin.store();
    let stopping = || *shutdown.borrow();
    let mut pass = AdminPass::default();
    {
        let _task = status.begin(AdminTask::PrSync);
        pass.sync = sync_open_prs(admin, cfg, shutdown).await;
    }
    if stopping() {
        return pass;
    }
    {
        let _task = status.begin(AdminTask::Reap);
        crate::server::reap_deleted_repos(store, &cfg.work_root, notes_lock).await;
    }
    if stopping() {
        return pass;
    }
    {
        let _task = status.begin(AdminTask::FetchAhead);
        pass.fetched = Some(crate::scheduler::fetch_ahead(store, shutdown).await);
    }
    if stopping() {
        return pass;
    }
    {
        let _task = status.begin(AdminTask::DependencyScan);
        pass.dependencies = Some(crate::scheduler::dependency_scans(store, cfg, shutdown).await);
    }
    if stopping() {
        return pass;
    }
    {
        let _task = status.begin(AdminTask::Sweep);
        sweep_workspaces(store, &cfg.work_root).await;
    }
    pass
}

/// [`crate::scheduler::sync_prs`], logged as a `prsync` event when it made
/// work for the job lane. Its failures are already events of their own,
/// one per PR (`sync #<id>: ...`).
async fn sync_open_prs(
    admin: &AdminAuthority,
    cfg: &Config,
    shutdown: &watch::Receiver<bool>,
) -> Option<SyncResult> {
    let Some(sync_pass) = admin.begin_pr_sync() else {
        // Only this lane syncs, one pass at a time; refused all the same
        // rather than read a PR twice at once.
        tracing::warn!("PR sync skipped: another sync pass is running");
        return None;
    };
    let s = crate::scheduler::sync_prs(&sync_pass.until(shutdown.clone()), cfg).await;
    let line = format!(
        "prsync {}s/{}m/{}c/{}a/{}r/{}q/{}e",
        s.synced, s.merged, s.closed, s.attention, s.raised, s.requeued, s.errors
    );
    tracing::debug!("{line}");
    if s.made_work() {
        let _ = admin.store().log_event("prsync", &line, None, None).await;
    }
    Some(s)
}

/// What the admin lane shares with the rest of the daemon.
struct AdminLane {
    admin: AdminAuthority,
    cfg: Config,
    notes_lock: Arc<tokio::sync::Mutex<()>>,
    status: AdminStatus,
    /// Its own wake: a trigger.
    wake: Arc<Notify>,
    /// The job lane's: rung when a pass made work for it.
    job_wake: Arc<Notify>,
}

/// Run admin passes every [`ADMIN_INTERVAL`], or sooner when woken, until
/// shutdown. Pausing the scheduler does not stop them: a pause stops new
/// jobs, which spend tokens, while this lane spends none, and a paused
/// daemon still records merges, closes and review comments and sweeps
/// finished trees. A pass stops early at shutdown ([`admin_pass`]).
async fn run_admin_lane(lane: AdminLane, mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        let pass = admin_pass(
            &lane.admin,
            &lane.cfg,
            &lane.notes_lock,
            &lane.status,
            &shutdown,
        )
        .await;
        if pass.made_work() {
            lane.job_wake.notify_one();
        }
        tokio::select! {
            () = tokio::time::sleep(ADMIN_INTERVAL) => {},
            () = lane.wake.notified() => {},
            _ = shutdown.changed() => {},
        }
    }
}

/// Turn off automatic maintenance in every clone of the daemon's own on
/// disk (`workspace::quiet_clones`). Startup only, before the first job;
/// best effort, since a fetch runs with it off regardless.
async fn quiet_clones(store: &Store, work_root: &Path) {
    let quieted = store
        .lane(async |lane| crate::workspace::quiet_clones(lane, work_root).await)
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r.map_err(anyhow::Error::from));
    if let Err(e) = quieted {
        tracing::warn!("turning off automatic maintenance in the clones failed: {e}");
    }
}

/// (`state_label`, detail) for `scheduler_state` (`server._describe_cycle`).
pub fn describe_cycle(summary: &CycleSummary) -> (String, String) {
    use std::fmt::Write;
    if let Some(e) = summary.error.as_deref() {
        return ("error".into(), e.chars().take(200).collect());
    }
    if let Some(v) = summary.idle.as_deref() {
        return ("idle".into(), v.to_owned());
    }
    if let Some(v) = summary.skipped.as_deref() {
        return ("idle".into(), v.to_owned());
    }
    // Before `denied`, which it also sets: offline is its own state, so
    // the status page can say which host is missing rather than "denied".
    if let Some(host) = summary.unreachable.as_deref() {
        return ("offline".into(), format!("{host} unreachable"));
    }
    if let Some(d) = summary.denied.as_deref() {
        return ("denied".into(), format!("last: {d}"));
    }
    let state = summary.state;
    let kind = summary.kind.map_or("", super::domain::JobKind::as_str);
    if matches!(
        state,
        Some(JobState::Done | JobState::Killed | JobState::Failed | JobState::Suspended)
    ) && !kind.is_empty()
    {
        let target = if let Some(fid) = summary.finding_id {
            format!("#{fid}")
        } else if let Some(repo) = summary.repo.as_deref() {
            format!("({repo})")
        } else {
            "(?)".into()
        };
        let outcome = summary.outcome.as_deref();
        let mut bits = format!("{kind} {target}");
        if let Some(o) = outcome {
            let _ = write!(bits, " -> {o}");
        } else if !matches!(state, Some(JobState::Done)) {
            let _ = write!(
                bits,
                " {}",
                state.map_or("", super::domain::JobState::as_str)
            );
        }
        return ("idle".into(), format!("last: {bits}"));
    }
    ("idle".into(), "cycle produced no actionable outcome".into())
}

/// How long the daemon should sleep after a cycle (`server._compute_sleep_s`).
pub async fn compute_sleep_s(store: &Store, summary: &CycleSummary) -> f64 {
    let sleep_s: f64;
    if summary.error.is_some() {
        sleep_s = 5.0 * 60.0;
    } else if {
        matches!(
            summary.state,
            Some(JobState::Done | JobState::Killed | JobState::Failed | JobState::Suspended)
        )
    } {
        let queued = store.count_queued().await.unwrap_or(0);
        let inserted = summary.ingest.as_ref().map_or(0, |i| i.inserted);
        let enabled = store.count_enabled_repos().await.unwrap_or(0);
        sleep_s = if queued > 0 {
            0.0
        } else if inserted > 0 {
            5.0
        } else if enabled > 0 {
            60.0
        } else {
            15.0 * 60.0
        };
    } else if summary.denied.is_some() {
        if let Some(retry_at) = summary.retry_at.map(|v| v as f64) {
            if retry_at > 0.0 {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();
                let until = retry_at / 1000.0 - now;
                sleep_s = (until + 30.0).clamp(60.0, 3600.0);
            } else {
                sleep_s = 30.0 * 60.0;
            }
        } else {
            sleep_s = 30.0 * 60.0;
        }
    } else if summary.skipped.is_some() {
        // The pick did no work: it was stale (the finding left `queued` or
        // `rechecking`, its repo was deleted) or had nothing to do (a hunt
        // with no new commits). Every skip changes what the next pick sees,
        // so start the next cycle now instead of idling a slot away.
        sleep_s = 0.0;
    } else {
        sleep_s = 15.0 * 60.0;
    }
    // While a PR is open, never sleep past the admin lane's interval
    // (budget backoff excepted, which only the budget can lift). The admin
    // lane wakes this one when a sync makes new work, but not for work that
    // was already there: an engage whose last attempt failed and is due a
    // retry, or a PR still flagged from an earlier pass.
    if summary.denied.is_none() && has_open_pr(store).await {
        return sleep_s.min(ADMIN_INTERVAL.as_secs_f64());
    }
    sleep_s
}

/// Whether any finding has an open PR; `false` when that cannot be read,
/// which only leaves the sleep as it was.
async fn has_open_pr(store: &Store) -> bool {
    store
        .list_findings(&crate::store::FindingFilter {
            status: Some(crate::domain::FindingStatus::PrOpen),
            ..crate::store::FindingFilter::default()
        })
        .await
        .is_ok_and(|open| !open.is_empty())
}

/// When a loop that sleeps `sleep_s` from `now_ms` wakes, in epoch ms:
/// the `next_wake_at` the status page counts down to.
pub fn next_wake_ms(now_ms: i64, sleep_s: f64) -> i64 {
    now_ms + (sleep_s * 1000.0) as i64
}

/// Run the daemon: axum serve, usage prober, the job lane and the admin
/// lane.
#[allow(
    clippy::too_many_lines,
    reason = "process entry point: every line is wiring (lockfile, store, \
              router, listener, prober, shutdown) whose order is the \
              startup contract"
)]
pub async fn run_daemon(cfg: Config) -> anyhow::Result<()> {
    // The lock first, then the store under it: the daemon's startup
    // token comes only with the lock, and the store holds the lock until
    // its last clone is gone. Before anything else reads the job table:
    // every `running` row and every `fixing` claim is the dead prior
    // process's. The lanes need the `Recovered` the token becomes, so
    // this runs exactly once and before any job exists. The sweep below
    // then releases the trees of the chains this ended.
    let lock = crate::recovery::DaemonLock::acquire(&cfg.work_root, &cfg.db_path)?;
    let (store, unreconciled) = Store::connect_exclusive(lock).await?;
    let store = Arc::new(store);
    let (recovered, settled) =
        crate::recovery::reconcile_and_log(unreconciled, &cfg.work_root).await?;
    tracing::info!("startup reconcile: {settled:?}");
    let (mut jobs, admin) = recovered.into_lanes();
    // One mutex for the whole process, shared by the HTTP handlers and
    // the reaper passes below: it exists to keep a note append from
    // recreating a file that reclamation is removing, which only works
    // if both sides take the same lock.
    let repo_notes = Arc::new(tokio::sync::Mutex::new(()));
    // Finish any deletion interrupted by a crash or a locked directory.
    // A flagged repo is already invisible, but its row is what records
    // that `repos/repo-<id>` is still on disk, so it may only be dropped
    // once those files are gone -- which is what this does.
    crate::server::migrate_repo_notes(&cfg.work_root);
    let reaped = crate::server::reap_deleted_repos(&store, &cfg.work_root, &repo_notes).await;
    if reaped > 0 {
        tracing::info!("reclaimed {reaped} repo director(ies) left by earlier deletions");
    }
    quiet_clones(&store, &cfg.work_root).await;
    sweep_workspaces(&store, &cfg.work_root).await;
    anyhow::ensure!(
        cfg.backend_type == "omp-scavenge",
        "unknown backend_type: {:?}",
        cfg.backend_type
    );
    // The overdrive flag is the handle's, shared with the backend wrapper
    // that reads it on every decision.
    let scheduler = crate::server::SchedulerHandle::default();
    jobs.report_to(scheduler.job.clone());
    let provider_backend: Arc<dyn Backend> =
        Arc::new(crate::backends::omp_scavenge::OmpScavengeBackend {
            cfg: cfg.clone(),
            ledger: store.clone() as Arc<dyn crate::backend::SpendLedger>,
            agent_db: crate::backends::omp_scavenge::default_agent_db(),
            prober: Arc::new(crate::backend::CmdProber),
        });
    let backend: Arc<dyn Backend> = Arc::new(crate::backend::OverdriveBackend::new(
        provider_backend,
        scheduler.overdrive.clone(),
    ));

    let cycle_running = scheduler.running.clone();
    let scheduler_paused = scheduler.paused.clone();
    let wake = scheduler.wake.clone();

    let state = AppState {
        store: store.clone(),
        config: Arc::new(cfg.clone()),
        backend: backend.clone(),
        repo_notes: repo_notes.clone(),
        scheduler: scheduler.clone(),
    };

    // Loopback unless `serve.host` opts in to more. Widening the listener
    // does not widen who is served: the `Host` check still admits only
    // loopback names and `serve.allowedHosts` (`server::host_guard`).
    let addr = std::net::SocketAddr::new(cfg.serve_host, cfg.serve_port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("daemon started: ui http://{addr}/ -- job and admin lanes live");

    // One shutdown source for the whole daemon, created before the loop.
    // A signal future built inside the loop's select! cannot replay a
    // delivery that predates it, so a signal arriving while the cycle body
    // ran would be dropped on the floor; latching it here means the next
    // wait observes it. Both SIGTERM (systemctl stop) and SIGINT stop the
    // daemon, matching the single stop event in server.py.
    //
    // Both handlers are installed here, synchronously, not inside the
    // task: a spawned task may not have run yet when the UI below starts
    // answering, and until a handler is installed SIGTERM keeps its default
    // action and kills the process outright instead of stopping it cleanly.
    // (`ctrl_c()` is no substitute: it installs only when first polled.)
    let sigterm = install_signal(SignalKind::terminate(), "SIGTERM");
    let sigint = install_signal(SignalKind::interrupt(), "SIGINT");
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let signal_handle = tokio::spawn(async move {
        let name = tokio::select! {
            () = next_signal(sigint) => "SIGINT",
            () = next_signal(sigterm) => "SIGTERM",
        };
        tracing::info!("received {name}: finishing the lanes' current work, then stopping");
        shutdown_tx.send_replace(true);
        // Hold the sender forever: dropping it would make `changed()`
        // resolve instantly with an error on every iteration, spinning
        // the scheduler loop.
        std::future::pending::<()>().await;
    });

    // UI server task, stopped by its own handle rather than by the
    // signal. A private `ctrl_c()` made the two halves answer the same
    // signal differently — SIGINT stopped the HTTP server at once while
    // the scheduler ran on for the rest of the cycle (up to the fix cap,
    // 45 minutes), leaving a live process serving nothing. Waiting on
    // the shared shutdown latch instead does not fix that: the latch is
    // set when the signal arrives, so the UI still stops immediately and
    // the process spends the rest of the cycle refusing HTTP.
    //
    // The operator wants the opposite — a stopping daemon that still
    // answers "what is it doing?" until it has actually stopped. So this
    // is notified after both scheduler lanes exit, and nothing else
    // resolves it.
    let ui_stop = Arc::new(Notify::new());
    let ui_stop_rx = ui_stop.clone();
    let ui_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state))
            .with_graceful_shutdown(async move { ui_stop_rx.notified().await })
            .await;
    });

    // Usage prober task
    let prober_backend = backend.clone();
    let prober_handle = tokio::spawn(async move {
        // Immediate probe on startup (Python does keep_fresh() first, then sleeps)
        match prober_backend.keep_fresh().await {
            Ok(true) => tracing::debug!("usage probe: startup refresh"),
            Ok(false) => {}
            Err(e) => tracing::warn!("usage probe startup error: {e}"),
        }
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(USAGE_PROBE_TICK_S)).await;
            match prober_backend.keep_fresh().await {
                Ok(true) => tracing::debug!("usage probe: refreshed"),
                Ok(false) => {}
                Err(e) => tracing::warn!("usage probe error: {e}"),
            }
        }
    });

    // Admin lane: its own task, beside the job lane below.
    let admin_handle = tokio::spawn(run_admin_lane(
        AdminLane {
            admin,
            cfg: cfg.clone(),
            notes_lock: repo_notes.clone(),
            status: scheduler.admin.clone(),
            wake: scheduler.admin_wake.clone(),
            job_wake: wake.clone(),
        },
        shutdown_rx.clone(),
    ));

    // Job lane
    let mut stop = false;
    while !stop {
        // Not atomic with the `cycle_running.store(true)` below, on
        // purpose. A pause stored between the two (a window of a few
        // instructions, no await in it) lets one cycle start after the
        // handler has answered `{"paused": true}`. That outcome looks
        // exactly like a pause arriving just after the cycle started,
        // which the feature already allows: a running cycle always
        // finishes, and the summary shows paused with the cycle still
        // running. A lock shared with the handler would close a window
        // nobody can see.
        if scheduler_paused.load(Ordering::SeqCst) {
            let _ = store
                .set_scheduler_state("paused", "scheduler paused by operator", None)
                .await;
            // The shared shutdown latch, not a private `ctrl_c()`: that saw
            // only SIGINT, so `systemctl stop` (SIGTERM) on a paused daemon
            // went unanswered until systemd's stop timeout killed it.
            tokio::select! {
                () = wake.notified() => {},
                _ = shutdown_rx.changed() => {},
            }
            stop = *shutdown_rx.borrow();
            continue;
        }

        cycle_running.store(true, Ordering::SeqCst);
        let summary = crate::scheduler::run_cycle(&mut jobs, &cfg, &*backend, None).await;
        cycle_running.store(false, Ordering::SeqCst);

        let sleep_s = compute_sleep_s(&store, &summary).await;
        let (state_label, detail) = describe_cycle(&summary);
        let next_wake = next_wake_ms(crate::util::now_ms(), sleep_s);
        let _ = store
            .set_scheduler_state(&state_label, &detail, Some(next_wake))
            .await;
        let summary_str = serde_json::to_string(&summary).unwrap_or_default();
        let summary_head: String = summary_str.chars().take(200).collect();
        tracing::info!("cycle: {summary_head} -> sleep {sleep_s:.0}s");

        // Sleep, wake on shutdown or notify
        tokio::select! {
            () = tokio::time::sleep(std::time::Duration::from_secs_f64(sleep_s)) => {},
            () = wake.notified() => {
                tracing::debug!("job lane woken by notify");
            },
            _ = shutdown_rx.changed() => {},
        }
        // Whichever branch won the race: the flag is latched, so consult it
        // rather than inferring shutdown from the branch that fired.
        stop = *shutdown_rx.borrow();
    }

    // The admin lane saw the same latch; let its pass in progress finish.
    if let Err(e) = admin_handle.await {
        tracing::error!("admin lane: {e}");
    }
    signal_handle.abort();
    prober_handle.abort();
    // Only now: both lanes are done, so the UI has nothing left to report.
    // Notified rather than aborted, so in-flight responses finish.
    ui_stop.notify_waiters();
    let _ = tokio::time::timeout(UI_DRAIN, ui_handle).await;
    tracing::info!("daemon stopped");
    Ok(())
}
