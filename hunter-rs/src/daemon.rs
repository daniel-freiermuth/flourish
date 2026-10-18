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
//! - The admin lane runs its duties ([`ADMIN_DUTIES`]): PR sync, reaping
//!   deleted repos, fetch-ahead, the Renovate dependency scans, the
//!   workspace sweep. Each is its own task on its own timer
//!   ([`spawn_admin_lane`]), so a slow one -- a Renovate scan, the fetch
//!   of a large repo -- delays none of the others, and PR sync above all
//!   records a merge within its interval whatever else the lane is doing.
//!   Each run is its own task too ([`supervise_duty`]): a run that panics
//!   is logged and the duty runs again at its next tick. Only this lane
//!   holds the [`AdminAuthority`], which its duties take, so only it
//!   syncs PRs and runs them. None of its work spends tokens, so none of
//!   it waits for a job any more, and a pause, which stops new jobs, does
//!   not stop it.
//!
//! What the lanes and duties touch at once is guarded where it lives, not
//! by running them apart: a finding's status moves only by guarded
//! transitions, a PR's `pr_state` columns have one writer per group, a
//! clone changes only under its permit, and a tree a runner still uses is
//! leased. The lanes meet through the database: a duty that made work
//! wakes the job lane, which picks from what the duty recorded. Where the
//! database may be behind the forge, the job lane waits for a PR sync
//! pass ([`PrSyncPasses`]): before its first pick, and after a cycle that
//! met a PR the forge has ended and the database has not.
//!
//! # Where the work queue is, and why there is no queue type
//!
//! There is no in-memory queue of work items, and no pool of workers that
//! drains one. The scheduler does without them for now; the end of this
//! section says when one becomes worth building. The queue is the
//! database: a work item is a finding × a kind, active while the finding
//! is in that kind's tier, with its streak, hold and checkpoint
//! (WORK-MODEL.md §6), or a repo's scan, due by its `last_*_at`. What to
//! do next is read from those rows each time:
//!
//! - The job lane's dispatcher is [`crate::scheduler::pick_next`], one
//!   priority ladder over every token-spending work item. One job runs at
//!   a time, and the types make it so: a cycle borrows the one
//!   `&mut JobAuthority`. That bound fits while tokens are the scarce
//!   resource; several workers need the changes listed below.
//! - Each admin duty reads its own due set (the `pr_open` findings, the
//!   flagged repos, the clones not fetched lately, the repos whose
//!   dependencies are due, the finished trees) at every run and works
//!   through it. A duty is its own worker; between runs it sleeps until
//!   its timer or its wake.
//! - The lanes hand work to each other through the same rows: a duty that
//!   made work wakes the job lane ([`DutyRun::made_work`]), which then
//!   picks it. When Renovate has no answer, the admin lane hands the
//!   repo's dependency scan to the job lane's AI scan as a row
//!   (`dep_scan_fallbacks`), not as a message.
//!
//! So the state survives a crash or a restart without a queue to rebuild,
//! and the guards that keep two writers apart (guarded transitions, one
//! running job per place, clone permits, tree leases) sit on those rows,
//! where every reader sees them.
//!
//! Two limits of the current design, and why they stand:
//!
//! - **A duty's pass is serial inside the duty.** The Renovate duty scans
//!   its due repos one after another, fetch-ahead fetches one clone after
//!   another. A slow scan delays only the scans after it. The duties do
//!   not wait for each other, and none of them waits for a job. (Before
//!   the duties were split, the whole admin lane was one serial pass with
//!   a 5-minute sleep, so a Renovate scan held up PR sync. That is the
//!   problem [`spawn_admin_lane`] fixes; do not reintroduce a shared
//!   pass.)
//! - **The job lane still does a job's preparation and wrap-up itself.**
//!   The fetch, the clone, the worktree, ingest, push and PR creation run
//!   inside its cycle, around the worker. They spend no tokens, but each
//!   has to finish before the next job starts. Fetch-ahead keeps the
//!   fetch short. Whether the rest is worth moving to the admin lane is
//!   to be measured first: the job lane reports each of these phases
//!   (`job_activity`, [`crate::types::JobPhase`]).
//!
//! A queue of typed work items with a pool of job workers becomes worth
//! building when either of these is wanted:
//!
//! - **Several job workers.** Then the budget's grant and its reservation
//!   must become one step, because today two workers could both pass the
//!   gate against the same headroom. Each chain also needs its own build
//!   cache, because the clone's shared `target/` serializes builds
//!   ([`crate::workspace::Workspace::build_cache`]). The UI needs every
//!   running job, not one. The database already refuses two running jobs
//!   on one place (migration 031), so two workers cannot corrupt state.
//!   They would only waste a pick.
//! - **Preparation and wrap-up off the job lane.** Then a job becomes
//!   three steps handed between the lanes: prepare, run, conclude. The
//!   hand-offs would have to be persisted, for example a fix ready to
//!   ship, so that a crash between steps loses nothing.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

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
const UI_DRAIN: Duration = Duration::from_secs(5);
/// How often each admin duty runs when nothing wakes it sooner.
pub const ADMIN_INTERVAL: Duration = Duration::from_mins(5);
/// How long fetch-ahead leaves a clone alone after its last fetch,
/// whichever lane made it and whether it succeeded or not
/// ([`crate::scheduler::fetch_ahead`]).
pub const FETCH_AHEAD_MIN_AGE: Duration = Duration::from_mins(30);

/// The admin lane's duties, each its own task ([`spawn_admin_lane`]).
pub const ADMIN_DUTIES: [AdminTask; 5] = [
    AdminTask::PrSync,
    AdminTask::Reap,
    AdminTask::FetchAhead,
    AdminTask::DependencyScan,
    AdminTask::Sweep,
];

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
/// run is retried on the next.
async fn sweep_workspaces(admin: &AdminAuthority, work_root: &Path) {
    let swept = crate::workspace::sweep(admin, work_root, crate::util::now_ms()).await;
    match swept {
        Ok(r) if r != crate::workspace::SweepReport::default() => {
            tracing::info!("workspace sweep: {r:?}");
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("workspace sweep failed: {e}"),
    }
}

/// What one run of an admin duty did ([`admin_duty`]).
#[derive(Debug)]
pub enum DutyRun {
    /// The PR sync's counts; `None` when another sync pass was live.
    Synced(Option<SyncResult>),
    /// How many deleted repos' directories were removed.
    Reaped(usize),
    Fetched(FetchAhead),
    Scanned(DepScans),
    Swept,
}

impl DutyRun {
    /// Whether this run gave the job lane work it did not have: a PR's end
    /// or flag ([`SyncResult::made_work`]), or a repo whose AI dependency
    /// scan is now due.
    pub fn made_work(&self) -> bool {
        match self {
            Self::Synced(sync) => sync.as_ref().is_some_and(SyncResult::made_work),
            Self::Scanned(scans) => scans.fallbacks > 0,
            Self::Reaped(_) | Self::Fetched(_) | Self::Swept => false,
        }
    }
}

/// Run admin duty `task` once, shown on the status page while it runs
/// (`status`). None of it creates a job; that takes the job lane's
/// authority, which this does not have.
///
/// Once `shutdown` is set, a sync stops at the next PR, and fetch-ahead
/// and the dependency scans at the next repo: the daemon waits for every
/// duty's run in progress, and a run over many PRs or repos is many
/// round trips.
pub async fn admin_duty(
    admin: &AdminAuthority,
    cfg: &Config,
    notes_lock: &tokio::sync::Mutex<()>,
    status: &AdminStatus,
    task: AdminTask,
    shutdown: &watch::Receiver<bool>,
) -> DutyRun {
    let _shown = status.begin(task);
    match task {
        AdminTask::PrSync => DutyRun::Synced(sync_open_prs(admin, cfg, shutdown).await),
        AdminTask::Reap => DutyRun::Reaped(
            crate::server::reap_deleted_repos(admin, &cfg.work_root, notes_lock).await,
        ),
        AdminTask::FetchAhead => DutyRun::Fetched(
            crate::scheduler::fetch_ahead(admin, FETCH_AHEAD_MIN_AGE, shutdown).await,
        ),
        AdminTask::DependencyScan => {
            DutyRun::Scanned(crate::scheduler::dependency_scans(admin, cfg, shutdown).await)
        }
        AdminTask::Sweep => {
            sweep_workspaces(admin, &cfg.work_root).await;
            DutyRun::Swept
        }
    }
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
        // Only this duty syncs, one run at a time; refused all the same
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

/// The PR sync passes the admin lane has begun and finished, so the job
/// lane can wait until the forge's state of every open PR is recorded
/// before it picks from it: once at startup, when the database may be
/// hours behind the forge, and after a cycle that found a PR already
/// ended there ([`CycleSummary::awaits_pr_sync`]). Picked from a
/// `pr_open` row the sync has not yet caught up with, an engage would
/// run a whole worker session on a merged PR.
#[derive(Clone, Default)]
pub struct PrSyncPasses(Arc<watch::Sender<SyncCount>>);

#[derive(Clone, Copy, Default)]
struct SyncCount {
    begun: u64,
    finished: u64,
    /// Whether the pass that finished last read every open PR
    /// ([`SyncResult::complete`]).
    last_complete: bool,
}

/// A PR sync pass in progress: counted finished when dropped, so a pass
/// that panics still releases a job lane waiting for it. Complete only
/// once [`Self::completed`] says so; a pass that panicked or stopped for
/// shutdown before reading every PR is not.
struct SyncPassCount<'a> {
    passes: &'a PrSyncPasses,
    complete: bool,
}

impl SyncPassCount<'_> {
    /// Record what the pass's run returned.
    fn completed(&mut self, run: &DutyRun) {
        self.complete = matches!(run, DutyRun::Synced(Some(s)) if s.complete);
    }
}

impl Drop for SyncPassCount<'_> {
    fn drop(&mut self) {
        let complete = self.complete;
        self.passes.0.send_modify(|c| {
            c.finished += 1;
            c.last_complete = complete;
        });
    }
}

impl PrSyncPasses {
    fn begin(&self) -> SyncPassCount<'_> {
        self.0.send_modify(|c| c.begun += 1);
        SyncPassCount {
            passes: self,
            complete: false,
        }
    }

    /// Wait until a first pass has finished, or for shutdown.
    pub async fn first(&self, shutdown: &watch::Receiver<bool>) {
        self.finished_after(0, shutdown).await;
    }

    /// Wake the PR sync (`wake`) and wait until a pass begun after this
    /// call has finished, or for shutdown: whether such a pass finished
    /// having read every open PR. A pass already running when this is
    /// called may have read a PR before it ended, so it does not count;
    /// nor does one that panicked or stopped for shutdown before reaching
    /// every PR, since it may never have read the one waited for.
    pub async fn next(&self, wake: &Notify, shutdown: &watch::Receiver<bool>) -> bool {
        let begun = self.0.borrow().begun;
        wake.notify_one();
        self.finished_after(begun, shutdown).await
    }

    /// Whether a pass past the first `passes` finished before shutdown,
    /// having read every open PR.
    async fn finished_after(&self, passes: u64, shutdown: &watch::Receiver<bool>) -> bool {
        let mut counts = self.0.subscribe();
        let mut shutdown = shutdown.clone();
        tokio::select! {
            biased;
            c = counts.wait_for(|c| c.finished > passes) => c.is_ok_and(|c| c.last_complete),
            _ = shutdown.wait_for(|stop| *stop) => false,
        }
    }
}

/// What the admin lane's duties share.
pub struct AdminLane {
    pub admin: AdminAuthority,
    pub cfg: Config,
    pub notes_lock: Arc<tokio::sync::Mutex<()>>,
    pub status: AdminStatus,
    /// The PR sync duty's wake (`SchedulerHandle::pr_sync_wake`): a
    /// trigger.
    pub pr_sync_wake: Arc<Notify>,
    /// The reaper duty's wake (`SchedulerHandle::reap_wake`): a repo was
    /// deleted.
    pub reap_wake: Arc<Notify>,
    /// Counts the PR sync duty's passes, for the job lane to wait on.
    pub pr_sync_passes: PrSyncPasses,
    /// The job lane's wake: rung when a duty's run made work for it.
    pub job_wake: Arc<Notify>,
}

/// Start every admin duty ([`ADMIN_DUTIES`]) as its own task, each run
/// every [`ADMIN_INTERVAL`] ([`supervise_duty`]); PR sync also whenever
/// `pr_sync_wake` is notified, the reaper whenever `reap_wake` is. Pausing
/// the scheduler does not stop them: a
/// pause stops new jobs, which spend tokens, while these spend none, and a
/// paused daemon still records merges, closes and review comments and
/// sweeps finished trees. The tasks end at shutdown, once their run in
/// progress has ([`admin_duty`]).
///
/// One task per duty, never one shared pass: a pass ran PR sync, reaping,
/// fetch-ahead, the Renovate scans and the sweep in turn, so a merge or a
/// review comment waited for every Renovate scan due. See the module docs
/// for why the lanes have no queue type.
pub fn spawn_admin_lane(lane: AdminLane, shutdown: &watch::Receiver<bool>) -> Vec<JoinHandle<()>> {
    let lane = Arc::new(lane);
    ADMIN_DUTIES
        .into_iter()
        .map(|task| {
            let timer = DutyTimer {
                interval: ADMIN_INTERVAL,
                wake: match task {
                    AdminTask::PrSync => Some(Arc::clone(&lane.pr_sync_wake)),
                    AdminTask::Reap => Some(Arc::clone(&lane.reap_wake)),
                    _ => None,
                },
            };
            let run = {
                let (lane, shutdown) = (Arc::clone(&lane), shutdown.clone());
                move || {
                    let (lane, shutdown) = (Arc::clone(&lane), shutdown.clone());
                    async move {
                        let mut pass =
                            (task == AdminTask::PrSync).then(|| lane.pr_sync_passes.begin());
                        let run = admin_duty(
                            &lane.admin,
                            &lane.cfg,
                            &lane.notes_lock,
                            &lane.status,
                            task,
                            &shutdown,
                        )
                        .await;
                        if let Some(pass) = &mut pass {
                            pass.completed(&run);
                        }
                        run
                    }
                }
            };
            tokio::spawn(supervise_duty(
                Store::clone(lane.admin.store()),
                task,
                timer,
                Arc::clone(&lane.job_wake),
                shutdown.clone(),
                run,
            ))
        })
        .collect()
}

/// When an admin duty runs: every `interval`, and at once when `wake` is
/// notified.
pub struct DutyTimer {
    pub interval: Duration,
    pub wake: Option<Arc<Notify>>,
}

/// Run admin duty `task` by `run` until shutdown: once now, then on every
/// tick of `timer`, waking `job_wake` after a run that made work for the
/// job lane.
///
/// Each run is a task of its own, so a run that panics ends that run
/// only: it is logged as an `error` event, and the duty runs again at its
/// next tick. Run inline, one panic would end the duty for good while the
/// job lane went on, with nothing on the status page to say so -- no PR
/// sync, no merge recorded, no tree swept until a restart.
pub async fn supervise_duty<F, Fut>(
    store: Store,
    task: AdminTask,
    timer: DutyTimer,
    job_wake: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
    run: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = DutyRun> + Send + 'static,
{
    while !*shutdown.borrow() {
        match tokio::spawn(run()).await {
            Ok(done) => {
                if done.made_work() {
                    job_wake.notify_one();
                }
            }
            Err(e) => {
                let message = format!(
                    "admin duty {task:?} failed: {e}; it runs again in {}s",
                    timer.interval.as_secs()
                );
                tracing::error!("{message}");
                let _ = store.log_event("error", &message, None, None).await;
            }
        }
        let woken = async {
            match &timer.wake {
                Some(wake) => wake.notified().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            () = tokio::time::sleep(timer.interval) => {},
            () = woken => {},
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
    let reaped = crate::server::reap_deleted_repos(&admin, &cfg.work_root, &repo_notes).await;
    if reaped > 0 {
        tracing::info!("reclaimed {reaped} repo director(ies) left by earlier deletions");
    }
    quiet_clones(&store, &cfg.work_root).await;
    sweep_workspaces(&admin, &cfg.work_root).await;
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

    // Admin lane: its duties, each its own task, beside the job lane below.
    let pr_sync_passes = PrSyncPasses::default();
    let admin_duties = spawn_admin_lane(
        AdminLane {
            admin,
            cfg: cfg.clone(),
            notes_lock: repo_notes.clone(),
            status: scheduler.admin.clone(),
            pr_sync_wake: scheduler.pr_sync_wake.clone(),
            reap_wake: scheduler.reap_wake.clone(),
            pr_sync_passes: pr_sync_passes.clone(),
            job_wake: wake.clone(),
        },
        &shutdown_rx,
    );

    // Job lane. Not before the first PR sync: what the forge did while
    // the daemon was down -- a merge, a close -- is not in the database
    // until that pass records it.
    pr_sync_passes.first(&shutdown_rx).await;
    let mut stop = *shutdown_rx.borrow();
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

        // The cycle met a PR the forge has ended and the database has not:
        // picking again before a sync records it would pick the same work.
        // A pass that read every open PR and still could not record it
        // counts against the engage, so a sync that keeps failing on one PR
        // holds that engage instead of keeping the lane on it. Woken by
        // shutdown instead, or by a pass that panicked or stopped for
        // shutdown before reading every PR, nothing says the PR was read,
        // so there is nothing to count.
        if summary.awaits_pr_sync
            && pr_sync_passes
                .next(&scheduler.pr_sync_wake, &shutdown_rx)
                .await
        {
            crate::scheduler::count_unrecorded_pr_end(&jobs.scope(), &summary).await;
        }
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

    // The admin duties saw the same latch; let their runs in progress
    // finish.
    for duty in admin_duties {
        if let Err(e) = duty.await {
            tracing::error!("admin duty: {e}");
        }
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
