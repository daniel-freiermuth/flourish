//! Settling `running` jobs and `fixing` claims that nothing will finish,
//! and the types that say who may.
//!
//! Two owners can lose a job: the process that ran it, which died, and
//! the runner that created it, which returned without recording an
//! outcome. Each has exactly one place that settles what it lost, and the
//! types make the two impossible to mix up:
//!
//! - A dead process's rows are settled by [`reconcile_and_log`], which
//!   consumes the store's one [`Unreconciled`] token. The daemon gets that
//!   token only with a [`DaemonLock`] on the database
//!   ([`Store::connect_exclusive`]), so no second process can reconcile
//!   under a live one. It cannot run twice on a store, and it
//!   cannot run once a job exists: creating a job takes a [`CycleScope`],
//!   a scope is opened only through the [`JobAuthority`], and the only way
//!   to that is through a [`Recovered`], which only consuming that same
//!   token makes. (Tests may take the token of a store they just created
//!   without a lock, and skip the reconcile through
//!   `Unreconciled::assume_clean`, both behind the `test-support` feature;
//!   the daemon's build has no such way.)
//! - The daemon runs two lanes side by side, and [`Recovered::into_lanes`]
//!   hands each its own authority: the job lane the [`JobAuthority`],
//!   which opens one [`CycleScope`] at a time and is the only way to
//!   create a job or claim a fix, and the admin lane the
//!   [`AdminAuthority`], which is the only way to sync PRs. Neither can
//!   do the other's work, and since the split consumes the store's one
//!   [`Recovered`], the daemon has one of each.
//! - A live process's rows are owned by values. [`CycleScope::create_job`]
//!   returns the job as a [`RunningJob`], and [`CycleScope::claim_fix`] the
//!   `fixing` claim as a [`FixClaim`]. Neither can be copied or made
//!   outside this module, and each is the only way to end what it owns:
//!   [`RunningJob::complete`] / [`RunningJob::fail_unstarted`] for the job,
//!   [`FixClaim::release`] / [`FixClaim::open_pr`] / [`FixClaim::conclude`]
//!   for the claim, each consuming the value. So no code ends a job or
//!   moves a claimed finding on that it does not hold, and a claim, once
//!   released, has no holder left to release it again under a later
//!   claim of the same finding.
//! - A value dropped without being ended -- its runner returned early, a
//!   write failed, a future was cancelled -- has no holder left, so its
//!   `Drop` owes the row to the store's ledger. [`CycleScope::settle`]
//!   and [`JobAuthority::settle_unsettled`] settle what is owed: what was
//!   dropped, and nothing that is still held, however many scopes are
//!   live at once. Dropped is not stopped -- a worker on a blocking
//!   thread outlives its cancelled future -- so an owed job whose tree a
//!   process still works in, and an owed claim whose finding a `running`
//!   job works on, stay owed until that work is gone.
//!
//! The store's settling writes take proof of one of the two: the
//! [`Unreconciled`] token for "every `fixing` claim", a [`StrandedJob`] or
//! [`StrandedClaim`] for one row. Only this module makes those, from the
//! token or from the ledger of dropped values, so no other code can kill
//! a live job or release a live claim through them.
//!
//! The tokens and values hold a [`Store`] handle, not a borrow of one, so
//! they can move into a spawned task. A store holds its [`DaemonLock`]
//! until its last clone is dropped, so every token, and every job made
//! through one, lives inside the lock. A store opened with plain
//! `Store::connect` (the `hunter user` commands) has no token at all.

use std::collections::BTreeSet;
use std::fs::File;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::domain::{ClosureClass, FindingStatus, JobState};
use crate::store::{JobInsertError, JobOutcome, JobStart, PrSyncPass, Store, VerdictAnchor};
use crate::transition::Moved;
use crate::types::{Finding, Job};
use crate::workspace::{Workspace, tree_in_use};

const UNRECONCILED: u8 = 0;
const RECONCILING: u8 = 1;
const RECOVERED: u8 = 2;

/// Where one store handle is in startup recovery, and what its failed
/// settles still owe. The store holds one (shared by its clones) but
/// cannot look inside: every field and method is private to this module,
/// so the tokens below are the only way to move it.
#[derive(Clone, Default)]
pub struct Recovery {
    /// [`UNRECONCILED`] until [`take_unreconciled`] hands out the one
    /// token, then [`RECONCILING`], then [`RECOVERED`].
    state: Arc<AtomicU8>,
    /// What dropped [`RunningJob`]s and [`FixClaim`]s, and failed settles,
    /// still owe; settled by [`settle_owed`].
    unsettled: Arc<Mutex<Owned>>,
    /// The lock this handle was opened under ([`Store::connect_exclusive`]),
    /// held until the last clone is dropped.
    _lock: Option<Arc<DaemonLock>>,
}

impl Recovery {
    /// The recovery state of a handle opened under `lock`.
    pub(crate) fn exclusive(lock: DaemonLock) -> Self {
        Self {
            _lock: Some(Arc::new(lock)),
            ..Self::default()
        }
    }
}

/// This process's exclusive hold on a work root and a database: proof
/// that no other daemon runs on either. Taken before the store is opened
/// ([`Store::connect_exclusive`]), which is the only way the daemon gets
/// its startup token.
///
/// Two locks, because the two paths are configured apart (`workRoot`,
/// `dbPath`): two daemons with their own work roots on one database would
/// each reconcile the other's live jobs, and two with their own databases
/// in one work root would sweep each other's trees.
pub struct DaemonLock {
    _work_root: File,
    _db: File,
    db_path: PathBuf,
}

impl DaemonLock {
    /// Lock `<work_root>/hunter.lock` and `<db_path>-daemon.lock`, or fail
    /// naming the one another process holds.
    pub fn acquire(work_root: &Path, db_path: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(work_root)?;
        let work_root_lock = lock_file(&work_root.join("hunter.lock"))?;
        let mut db_lock = db_path.as_os_str().to_owned();
        db_lock.push("-daemon.lock");
        let db = lock_file(Path::new(&db_lock))?;
        Ok(Self {
            _work_root: work_root_lock,
            _db: db,
            db_path: db_path.to_owned(),
        })
    }

    /// The database this lock is for.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
}

/// Open `path` and take an exclusive lock on it, held while the file is.
fn lock_file(path: &Path) -> anyhow::Result<File> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| anyhow::anyhow!("opening lock {}: {e}", path.display()))?;
    file.try_lock().map_err(|_| {
        anyhow::anyhow!(
            "another hunter process is already running (lock: {})\n  \
             check: ps aux | grep '[h]unter --root'\n  \
             or:    systemctl --user status hunter.service",
            path.display()
        )
    })?;
    Ok(file)
}

/// The store's one [`Unreconciled`], `None` once it was handed out:
/// [`Store::connect_exclusive`], and `Store::take_unreconciled` in tests.
pub(crate) fn take_unreconciled(store: &Store) -> Option<Unreconciled> {
    store
        .recovery()
        .state
        .compare_exchange(
            UNRECONCILED,
            RECONCILING,
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .ok()
        .map(|_| Unreconciled {
            store: store.clone(),
        })
}

/// `Store::recovered`: the proof, once the store's token was spent. Only
/// in a `test-support` build.
#[cfg(feature = "test-support")]
pub(crate) fn recovered(store: &Store) -> Option<Recovered> {
    (store.recovery().state.load(Ordering::SeqCst) == RECOVERED).then(|| Recovered {
        store: store.clone(),
    })
}

/// The right to settle a store's jobs as a dead process's. One per
/// [`Store`] handle opened under a [`DaemonLock`]
/// ([`Store::connect_exclusive`]), consumed by [`reconcile_and_log`].
pub struct Unreconciled {
    store: Store,
}

impl Unreconciled {
    /// Spend the token: the store is recovered from here on.
    fn into_recovered(self) -> Recovered {
        self.store
            .recovery()
            .state
            .store(RECOVERED, Ordering::SeqCst);
        Recovered { store: self.store }
    }

    /// Declare, without looking, that no dead process left anything in
    /// this store: for a store this process just created, as the tests'
    /// fixtures are. Only in a `test-support` build, so the daemon can
    /// reach a [`Recovered`] through [`reconcile_and_log`] alone.
    #[cfg(feature = "test-support")]
    pub fn assume_clean(self) -> Recovered {
        self.into_recovered()
    }
}

/// Proof that `store`'s dead-process rows are settled, so jobs may be
/// created in it. Made only from the store's [`Unreconciled`] token, by
/// [`reconcile_and_log`], and neither `Clone` nor `Copy`: the daemon holds
/// exactly one, and spends it on its two lanes ([`Self::into_lanes`]).
/// Tests can also read it back from a store (`Store::recovered`,
/// `test-support` only).
pub struct Recovered {
    store: Store,
}

impl Recovered {
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Hand the proof to the daemon's two lanes, each its own part of it.
    pub fn into_lanes(self) -> (JobAuthority, AdminAuthority) {
        (
            JobAuthority {
                store: self.store.clone(),
                status: crate::activity::JobStatus::default(),
            },
            AdminAuthority { store: self.store },
        )
    }
}

/// The job lane's authority: the only way to a [`CycleScope`], and so to
/// creating jobs and claiming fixes. Neither `Clone` nor `Copy`, and a
/// scope borrows it mutably, so one lane holds it and opens one scope at
/// a time. Two cycles at once do not compile.
pub struct JobAuthority {
    store: Store,
    /// Where the lane's cycles report their phases
    /// (`crate::activity::job_phase`); nowhere until [`Self::report_to`].
    status: crate::activity::JobStatus,
}

impl JobAuthority {
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Report this lane's cycle phases to `status` (the status page's).
    pub fn report_to(&mut self, status: crate::activity::JobStatus) {
        self.status = status;
    }

    /// Where this lane's cycles report their phases.
    pub fn status(&self) -> crate::activity::JobStatus {
        self.status.clone()
    }

    /// Open the cycle's scope. It borrows this authority until it is
    /// settled or dropped, so no second scope opens beside it.
    pub fn scope(&mut self) -> CycleScope<'_> {
        CycleScope {
            store: self.store.clone(),
            _jobs: PhantomData,
        }
    }

    /// A scope for a test fixture that calls runners directly, bound to no
    /// authority: the one-at-a-time rule does not hold for it. Only in a
    /// `test-support` build, like `Store::test_lane`.
    #[cfg(feature = "test-support")]
    pub fn into_test_scope(self) -> CycleScope<'static> {
        CycleScope {
            store: self.store,
            _jobs: PhantomData,
        }
    }
}

/// The admin lane's authority: the only way to begin a PR sync pass, and
/// what the admin lane's other duties take (`crate::daemon`): the reaper,
/// fetch-ahead, the Renovate scans and their request for the AI scan, the
/// sweep. Neither `Clone` nor `Copy`: the admin lane holds it, sharing it
/// among its duties. It creates no job and claims no fix; those take the
/// [`JobAuthority`].
pub struct AdminAuthority {
    store: Store,
}

impl AdminAuthority {
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Begin the store's PR sync pass, or `None` while another is live
    /// ([`PrSyncPass`]). Sync needs this proof so that only the daemon's
    /// store, the one held under its [`DaemonLock`], can sync.
    pub fn begin_pr_sync(&self) -> Option<PrSyncPass> {
        PrSyncPass::begin(self)
    }

    /// Record that Renovate had no answer for `repo_id` just now, which
    /// hands the repo's `dep_update` to the job lane's AI scan
    /// ([`Store::dep_update_fallbacks`]). Only the admin lane's Renovate
    /// scan decides that, so only its authority records it.
    pub async fn request_dep_update_fallback(&self, repo_id: i64) -> sqlx::Result<()> {
        self.store.request_dep_update_fallback(repo_id).await
    }
}

/// A `running` job nothing will finish: found with the startup token in
/// hand, or created through a scope being settled. The store's settling
/// writes for a job take one ([`Store::suspend_orphan`],
/// [`Store::orphan_job`]), and only this module makes one.
pub struct StrandedJob(i64);

impl StrandedJob {
    pub const fn id(&self) -> i64 {
        self.0
    }
}

/// A `fixing` claim its owner can no longer release: taken through a
/// scope being settled. [`Store::requeue_stranded_claim`] takes one, and
/// only this module makes one.
pub struct StrandedClaim(i64);

impl StrandedClaim {
    pub const fn finding_id(&self) -> i64 {
        self.0
    }
}

/// Why a `running` row has no runner left to finish it.
#[derive(Clone, Copy)]
enum Orphaned {
    /// The process that ran it is gone; found at startup.
    PriorProcess,
    /// Its runner returned without recording an outcome; found when the
    /// scope it was created through was settled.
    Abandoned,
}

impl Orphaned {
    /// How the event log names a job settled this way.
    const fn label(self) -> &'static str {
        match self {
            Self::PriorProcess => "orphaned",
            Self::Abandoned => "abandoned",
        }
    }

    const fn cause(self) -> &'static str {
        match self {
            Self::PriorProcess => "prior process died mid-job",
            Self::Abandoned => "its runner returned without recording an outcome",
        }
    }
}

/// What one settling pass did, as counts the caller can report.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Settled {
    /// Jobs that were `running` and are now `suspended` or `killed`.
    pub jobs: usize,
    /// Findings put back from `fixing` to `queued`.
    pub requeued: usize,
    /// Owed jobs and claims this pass left owed, for a later one: a job
    /// whose tree a live process still works in (its worker outlived the
    /// dropped value), a claim whose finding a `running` job works on.
    pub deferred: usize,
}

/// Recover what a crashed prior process left: every `running` job, and
/// every `fixing` finding no job is working on.
///
/// A job still `running` here outlived the process that ran it. When its
/// chain's session directory holds a transcript with metered work from
/// this attempt, the job is recorded `suspended`, so the resume tier
/// continues it instead of the work being lost; the same rule
/// `scheduler::job_state` applies to a worker that died on its own. The
/// rest are marked killed -- among them every job that died before its
/// worker made a call, such as the fix jobs killed inside
/// `git worktree add` on 2026-09-29.
///
/// Consuming `token` is what keeps this to startup: no job can exist in
/// the store before it ([`Recovered`]), and it cannot run again after.
/// On an error the token is spent and the store stays unrecovered; the
/// daemon does not start.
pub async fn reconcile_and_log(
    token: Unreconciled,
    work_root: &Path,
) -> anyhow::Result<(Recovered, Settled)> {
    let store = &token.store;
    let jobs = store.list_running_jobs().await?;
    let jobs = settle_jobs(store, work_root, &jobs, Orphaned::PriorProcess).await?;
    let requeued = store.requeue_stranded_fixes(&token).await?;
    log_requeued(store, &requeued, Orphaned::PriorProcess).await;
    let settled = Settled {
        jobs,
        requeued: requeued.len(),
        deferred: 0,
    };
    Ok((token.into_recovered(), settled))
}

/// What dropped values owe, or what failed settles left owing
/// ([`Recovery`]'s ledger).
#[derive(Default, Clone)]
struct Owned {
    jobs: BTreeSet<i64>,
    /// [`FixClaim`]s dropped while still `fixing`.
    claims: BTreeSet<OwedClaim>,
}

impl Owned {
    /// Drop what `settled` holds: those rows were settled.
    fn forget(&mut self, settled: &Self) {
        self.jobs.retain(|id| !settled.jobs.contains(id));
        self.claims.retain(|c| !settled.claims.contains(c));
    }
}

/// One dropped [`FixClaim`]: its finding, and which claim of it. The
/// finding alone does not name the debt. A settle can release a claim
/// and the finding be claimed again, and that claim dropped, before the
/// settle strikes what it settled; keyed by finding, the later claim's
/// debt would be struck with the earlier one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OwedClaim {
    finding_id: i64,
    claim: u64,
}

/// The identity of the next [`FixClaim`] this process makes.
static NEXT_CLAIM: AtomicU64 = AtomicU64::new(0);

/// Proof, for the store's owner-only writes, that the caller holds what
/// it writes: a [`CycleScope`] creating a job or taking a claim, a
/// [`RunningJob`] recording its outcome, a [`FixClaim`] moving its
/// finding on. Only this module makes one, and only inside those, so no
/// job or `fixing` claim can exist without a value that ends it, and no
/// code can end one it does not hold.
pub struct Owner(());

/// The authority jobs are created and fixes claimed through, for one
/// cycle. Opened only by [`JobAuthority::scope`], which it borrows
/// mutably, so nothing is created before the startup reconcile, nothing
/// outside the job lane, and no two cycles run at once. What it creates
/// is owned by the values it returns, not by the scope:
/// [`CycleScope::settle`] settles what was dropped unended. Its runners
/// take clone permits in the cycle's [`Lane`](crate::clones::Lane), which
/// they are lent beside it: a permit borrows the lane, not the scope, so
/// a runner holding one can still create its job.
pub struct CycleScope<'a> {
    store: Store,
    _jobs: PhantomData<&'a mut JobAuthority>,
}

impl CycleScope<'_> {
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Create a job, owned by the [`RunningJob`] returned. The INSERT and
    /// its rules (deleted repos refused, one `running` job per finding and
    /// per repo-level (repo, kind), checkpoints superseded, a resume or
    /// handoff running on its predecessor's place and inheriting its
    /// chain's pins) are `Store::insert_job`'s. A job is always created
    /// `running`, and leaves that state only through its [`RunningJob`]
    /// or, once that is dropped, a settle.
    pub async fn create_job(
        &self,
        start: JobStart,
        cap_tokens: Option<i64>,
        estimated_tokens: Option<i64>,
    ) -> Result<CreatedJob, JobInsertError> {
        let inserted = self
            .store
            .insert_job(&Owner(()), start, cap_tokens, estimated_tokens)
            .await?;
        Ok(CreatedJob {
            job: RunningJob {
                store: self.store.clone(),
                id: inserted.id,
                ended: false,
            },
            superseded: inserted.superseded,
        })
    }

    /// Claim `finding_id` for a fix (`queued` → `fixing`), held by the
    /// [`FixClaim`] returned, which carries the row as the claim took it
    /// ([`FixClaim::finding`]). `None` when the finding is no longer
    /// `queued`.
    pub async fn claim_fix(&self, finding_id: i64) -> sqlx::Result<Option<FixClaim>> {
        let claimed = self.store.claim_fixing(&Owner(()), finding_id).await?;
        Ok(claimed.map(|finding| FixClaim {
            store: self.store.clone(),
            finding: Box::new(finding),
            claim: NEXT_CLAIM.fetch_add(1, Ordering::Relaxed),
            ended: false,
        }))
    }

    /// Settle what is owed once this cycle's runners have returned (which
    /// consuming the scope guarantees: they borrowed it): every
    /// [`RunningJob`] and [`FixClaim`] dropped unended, by this cycle or an
    /// earlier one whose settle failed.
    pub async fn settle(self, work_root: &Path) -> anyhow::Result<Settled> {
        settle_owed(&self.store, work_root).await
    }
}

/// A job [`CycleScope::create_job`] just inserted: the value that owns it,
/// and the checkpoints its insert superseded.
pub struct CreatedJob {
    pub job: RunningJob,
    /// Ids of the checkpoints this fresh job superseded, oldest first.
    pub superseded: Vec<i64>,
}

/// A `running` job, and the duty to record how it ended. The only way to
/// end the row ([`Self::complete`], [`Self::fail_unstarted`]), each
/// consuming it; made only by [`CycleScope::create_job`]. Dropped unended
/// -- its runner returned early, or the write that would have ended it
/// failed -- it owes the job to the store's ledger, and the first settle
/// after no process works in its tree any more suspends or kills it.
#[must_use = "a running job is ended by complete or fail_unstarted; dropped, it is settled as abandoned"]
pub struct RunningJob {
    store: Store,
    id: i64,
    ended: bool,
}

impl RunningJob {
    pub const fn id(&self) -> i64 {
        self.id
    }

    /// Record how the attempt ended (`Store::complete_job`).
    pub async fn complete(mut self, outcome: &JobOutcome<'_>) -> sqlx::Result<RecordedJob> {
        self.store
            .complete_job(&Owner(()), self.id, outcome)
            .await?;
        self.ended = true;
        Ok(RecordedJob {
            id: self.id,
            state: outcome.state.into(),
        })
    }

    /// Record a job whose worker never ran (`Store::fail_unstarted_job`).
    pub async fn fail_unstarted(mut self, notes: &str) -> sqlx::Result<()> {
        self.store
            .fail_unstarted_job(&Owner(()), self.id, notes)
            .await?;
        self.ended = true;
        Ok(())
    }

    /// Give the job up without ending or owing it: for a test fixture's
    /// row, which stands for a job a live owner still runs. Only in a
    /// `test-support` build, so the daemon cannot lose a job this way.
    #[cfg(feature = "test-support")]
    pub fn disown(mut self) -> i64 {
        self.ended = true;
        self.id
    }
}

impl Drop for RunningJob {
    fn drop(&mut self) {
        if !self.ended {
            lock(&self.store.recovery().unsettled).jobs.insert(self.id);
        }
    }
}

/// A job whose outcome is recorded ([`RunningJob::complete`]): proof for
/// the one write that revises it afterwards (`Store::fail_job`), which so
/// can never end a job that is still `running`.
#[derive(Debug, Clone, Copy)]
pub struct RecordedJob {
    id: i64,
    state: JobState,
}

impl RecordedJob {
    pub const fn id(&self) -> i64 {
        self.id
    }

    /// The state the outcome was recorded with.
    pub const fn state(&self) -> JobState {
        self.state
    }
}

/// A finding claimed for a fix (`fixing`), and the duty to move it on.
/// The only way to move a claimed finding out of `fixing`
/// ([`Self::release`], [`Self::open_pr`], [`Self::conclude`]), each
/// consuming it; made only by [`CycleScope::claim_fix`], and the store
/// refuses `fixing` on every other path. Dropped while still holding the
/// claim -- its fix returned early, or the release failed -- it owes the
/// finding to the store's ledger, and the next settle puts it back to
/// `queued`. Once consumed it is gone, so a later claim of the same
/// finding has a holder of its own that nothing else can release.
#[must_use = "a fix claim is moved on by release, open_pr or conclude; dropped, it is released by the next settle"]
pub struct FixClaim {
    store: Store,
    /// The row as the claim took it; boxed, so a claim handed back in an
    /// error stays small.
    finding: Box<Finding>,
    /// Which claim of the finding this is ([`OwedClaim`]).
    claim: u64,
    ended: bool,
}

impl FixClaim {
    pub const fn finding_id(&self) -> i64 {
        self.finding.id
    }

    /// The finding as the claim took it, read in the claim's own write.
    /// Until then a `queued` row can still change -- the admin lane's
    /// Renovate pass rewrites a `queued` dep update's target -- so the row
    /// the pick read can be older than the one now held. Once `fixing`,
    /// nothing rewrites it: the fix works from this row.
    pub fn finding(&self) -> &Finding {
        &self.finding
    }

    /// Put the finding back to `queued` if it is still `fixing`; `false`
    /// when it no longer was. On an error the claim is dropped held, so
    /// the next settle retries the release.
    pub async fn release(mut self) -> sqlx::Result<bool> {
        let released = self
            .store
            .release_fixing(&Owner(()), self.finding.id)
            .await?;
        self.ended = true;
        Ok(released)
    }

    /// `fixing` → `pr_open` with `pr_url`, if the finding is still
    /// `fixing`. [`Moved::Conflict`]: it had left `fixing`, so nothing was
    /// written and nothing is left to release; the claim ends either way.
    /// On an error the claim is handed back, still held.
    pub async fn open_pr(mut self, pr_url: &str) -> Result<Moved, (Self, sqlx::Error)> {
        match self
            .store
            .open_pr_from_fixing(&Owner(()), self.finding.id, pr_url)
            .await
        {
            Ok(moved) => {
                self.ended = true;
                Ok(moved)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// `fixing` → the status the decline's classification gives (`None`,
    /// unclassified: back to triage, as an `abandoned` closure), with
    /// `reason` and its `anchor`, if the finding is still `fixing`. A
    /// classification is the only thing a decline can say, so no other
    /// exit from `fixing` can be written here. [`Moved::Conflict`] as for
    /// [`Self::open_pr`]. On an error the claim is handed back, still held.
    pub async fn conclude(
        mut self,
        class: Option<ClosureClass>,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
    ) -> Result<Moved, (Self, sqlx::Error)> {
        match self
            .store
            .conclude_fixing(&Owner(()), self.finding.id, class, reason, anchor)
            .await
        {
            Ok(moved) => {
                self.ended = true;
                Ok(moved)
            }
            Err(e) => Err((self, e)),
        }
    }

    /// Give the claim up without releasing or owing it: for a test
    /// fixture's claim, which stands for one a live fix still holds. Only
    /// in a `test-support` build.
    #[cfg(feature = "test-support")]
    pub fn disown(mut self) {
        self.ended = true;
    }
}

impl Drop for FixClaim {
    fn drop(&mut self) {
        if !self.ended {
            lock(&self.store.recovery().unsettled)
                .claims
                .insert(OwedClaim {
                    finding_id: self.finding.id,
                    claim: self.claim,
                });
        }
    }
}

impl JobAuthority {
    /// Settle what is owed (as [`CycleScope::settle`]) before anything new is
    /// created: what earlier cycles dropped unended, or failed to settle.
    pub async fn settle_unsettled(&self, work_root: &Path) -> anyhow::Result<Settled> {
        settle_owed(&self.store, work_root).await
    }
}

/// Settle every [`RunningJob`] and [`FixClaim`] dropped unended: each has
/// no holder left, so nothing else will end it. Safe to repeat and to run
/// beside live owners: a settle acts only on rows still `running` or
/// `fixing`, and an owed claim's finding stays `fixing` until it is
/// released, so no other holder can have claimed it meanwhile. What is
/// still held is in no ledger and untouched. A dropped value is not proof
/// that its work stopped: a worker runs on a blocking thread that
/// dropping its future does not stop. So a job whose tree a process
/// still works in, and a claim whose finding a `running` job still works
/// on, stay owed for a later settle rather than being ended under that
/// work. On an error, or if this future is dropped mid-settle, what it
/// was settling stays owed.
async fn settle_owed(store: &Store, work_root: &Path) -> anyhow::Result<Settled> {
    let owed = owed_now(store);
    let (settled, deferred) = settle_owned(store, &owed, work_root).await?;
    forget_settled(store, owed, &deferred);
    Ok(settled)
}

/// What the ledger owes at this moment: the batch one settle works on.
fn owed_now(store: &Store) -> Owned {
    lock(&store.recovery().unsettled).clone()
}

/// Strike from the ledger what a settle of `owed` settled: all of it but
/// `deferred`. What was dropped into the ledger while that settle ran is
/// not in `owed`, so it stays owed.
fn forget_settled(store: &Store, mut owed: Owned, deferred: &Owned) {
    owed.forget(deferred);
    lock(&store.recovery().unsettled).forget(&owed);
}

fn lock(owned: &Mutex<Owned>) -> std::sync::MutexGuard<'_, Owned> {
    // The guarded sets are only inserted into, extended or pruned of
    // settled ids, so a panic while one was held cannot have left them
    // inconsistent.
    owned.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Settle `owned`; returns the counts and what is still worked on, which
/// stays owed.
async fn settle_owned(
    store: &Store,
    owned: &Owned,
    work_root: &Path,
) -> anyhow::Result<(Settled, Owned)> {
    let mut deferred = Owned::default();
    let mut stranded = Vec::new();
    for j in store.list_running_jobs().await? {
        if !owned.jobs.contains(&j.id) {
            continue;
        }
        if tree_in_use(&chain_workspace(store, work_root, &j).await?.tree) {
            deferred.jobs.insert(j.id);
        } else {
            stranded.push(j);
        }
    }
    let jobs = settle_jobs(store, work_root, &stranded, Orphaned::Abandoned).await?;
    let mut requeued = Vec::new();
    for &owed_claim in &owned.claims {
        let fid = owed_claim.finding_id;
        if store.requeue_stranded_claim(&StrandedClaim(fid)).await? {
            requeued.push(fid);
        } else if store
            .get_finding(fid)
            .await?
            .is_some_and(|f| f.status == FindingStatus::Fixing)
        {
            // Spared because a `running` job works on it: released once
            // that job has ended, not forgotten.
            deferred.claims.insert(owed_claim);
        }
    }
    log_requeued(store, &requeued, Orphaned::Abandoned).await;
    let settled = Settled {
        jobs,
        requeued: requeued.len(),
        deferred: deferred.jobs.len() + deferred.claims.len(),
    };
    Ok((settled, deferred))
}

/// Suspend or kill each of `jobs`, which nothing will finish (`why`), if
/// it is still `running`. Returns how many it settled.
async fn settle_jobs(
    store: &Store,
    work_root: &Path,
    jobs: &[Job],
    why: Orphaned,
) -> anyhow::Result<usize> {
    let (label, cause) = (why.label(), why.cause());
    let mut settled = 0;
    for j in jobs {
        let now = crate::util::now_ms();
        let stranded = StrandedJob(j.id);
        if let Some((session_file, tokens)) = orphan_work(store, work_root, j).await? {
            let note = format!("{label} -- {cause}; transcript kept for the resume");
            if !store
                .suspend_orphan(&stranded, &session_file, tokens, &note, now)
                .await?
            {
                continue;
            }
            let _ = store
                .log_event(
                    "resume",
                    &format!(
                        "{label} {} job #{} suspended for resume ({tokens} tok of work in {})",
                        j.kind, j.id, session_file
                    ),
                    Some(j.id),
                    j.finding_id,
                )
                .await;
        } else {
            if !store
                .orphan_job(&stranded, &format!("{label} -- {cause}"), now)
                .await?
            {
                continue;
            }
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "{label} {} job #{} (finding {:?}) -- {cause}",
                        j.kind, j.id, j.finding_id
                    ),
                    Some(j.id),
                    j.finding_id,
                )
                .await;
        }
        settled += 1;
    }
    Ok(settled)
}

/// One event per finding put back from `fixing` to `queued`.
async fn log_requeued(store: &Store, requeued: &[i64], why: Orphaned) {
    for fid in requeued {
        let _ = store
            .log_event(
                "error",
                &format!("#{fid} stuck 'fixing' -> 'queued' -- {}", why.cause()),
                None,
                Some(*fid),
            )
            .await;
    }
}

/// The workspace job `j` works in: its chain's, named by the chain's
/// first job.
async fn chain_workspace(store: &Store, work_root: &Path, j: &Job) -> anyhow::Result<Workspace> {
    let origin = store.resume_origin_job(j.id).await?;
    Ok(Workspace::for_chain(work_root, Path::new(""), origin))
}

/// `(session_file, tokens)` of the work orphaned job `j` did itself,
/// when it did any and nothing is still running in its tree.
///
/// The chain's session directory is shared by every attempt, so the
/// ledger there also holds what earlier attempts already recorded --
/// across kinds, when an engage handed its session to a harvest. That is
/// subtracted, as the harness meters a resume against its base.
async fn orphan_work(
    store: &Store,
    work_root: &Path,
    j: &Job,
) -> anyhow::Result<Option<(String, i64)>> {
    let ws = chain_workspace(store, work_root, j).await?;
    let Some((file, tokens, _)) =
        crate::backends::omp_scavenge::harness::ledger_dir_usage(&ws.session)
    else {
        return Ok(None);
    };
    let own = tokens - store.ancestor_tokens(j.id).await?;
    if own <= 0 || tree_in_use(&ws.tree) {
        return Ok(None);
    }
    Ok(Some((file.to_string_lossy().into_owned(), own)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A store over a fresh copy of the schema-only `dev.db`, its startup
    /// token spent as "nothing to reconcile", with repo 1 and finding 2
    /// (`queued`) seeded; and a work root beside it.
    async fn fixture(name: &str) -> (PathBuf, Recovered) {
        let dir = std::env::temp_dir().join(format!("recovery-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("work_root")).unwrap();
        let db = dir.join("hunter.db");
        std::fs::copy(Path::new(env!("CARGO_MANIFEST_DIR")).join("dev.db"), &db).unwrap();
        let pool =
            sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
                .await
                .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
             VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', \
             'main', 1, 1000); \
             INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at) \
             VALUES (2, 'bug', 1, 'fp2', 'medium', 0.8, 'two', 'queued', 1000, 1000);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        let store = Store::connect(&db).await.unwrap();
        let recovered = take_unreconciled(&store).unwrap().into_recovered();
        (dir, recovered)
    }

    /// What is dropped while a settle runs is owed until a settle takes
    /// it: a job, and a claim even when it is a later claim of a finding
    /// that settle has just released. The settle strikes what it settled,
    /// by claim, not by finding, and nothing else. Otherwise the later
    /// claim's debt is struck with the earlier one and the finding stays
    /// `fixing` until a restart; the job stays `running`, holding its
    /// reservation.
    #[tokio::test]
    async fn what_is_dropped_while_a_settle_runs_stays_owed() {
        use crate::domain::RepoJobKind;
        let (dir, recovered) = fixture("drop-during-settle").await;
        let mut jobs = recovered.into_lanes().0;
        let store = &jobs.store().clone();
        let work_root = dir.join("work_root");
        let scope = jobs.scope();
        drop(scope.claim_fix(2).await.unwrap().unwrap());

        // A settle takes the ledger and releases that claim ...
        let owed = owed_now(store);
        let (settled, deferred) = settle_owned(store, &owed, &work_root).await.unwrap();
        assert_eq!(settled.requeued, 1);
        // ... another fix claims the finding again and drops its claim
        // unended, and a hunt is created and dropped unended ...
        drop(scope.claim_fix(2).await.unwrap().unwrap());
        let hunt = scope
            .create_job(JobStart::fresh_repo(RepoJobKind::Hunt, 1), None, None)
            .await
            .unwrap()
            .job;
        let hunt_id = hunt.id();
        drop(hunt);
        // ... and only then does the settle strike what it settled.
        forget_settled(store, owed, &deferred);

        let next = jobs.settle_unsettled(&work_root).await.unwrap();

        assert_eq!(
            next,
            Settled {
                jobs: 1,
                requeued: 1,
                deferred: 0
            }
        );
        let status = store.get_finding(2).await.unwrap().unwrap().status;
        assert_eq!(status, FindingStatus::Queued);
        let jobs = store.list_jobs(10).await.unwrap();
        let hunt = &jobs.iter().find(|j| j.job.id == hunt_id).unwrap().job;
        assert_eq!(hunt.state, JobState::Killed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A settle writes only jobs that are still `running` when it writes
    /// them. A job that was ended between the settle's read and its write
    /// keeps the outcome it was ended with, whether the settle would have
    /// suspended it (its transcript holds work) or killed it, and is not
    /// counted.
    #[tokio::test]
    async fn a_settle_leaves_a_job_ended_since_it_read_it() {
        use crate::domain::RepoJobKind;
        let (dir, recovered) = fixture("ended-since-read").await;
        let mut jobs = recovered.into_lanes().0;
        let store = &jobs.store().clone();
        let work_root = dir.join("work_root");
        let scope = jobs.scope();
        let create =
            |kind: RepoJobKind| scope.create_job(JobStart::fresh_repo(kind, 1), None, None);
        let with_work = create(RepoJobKind::Hunt).await.unwrap().job;
        let without = create(RepoJobKind::TestGap).await.unwrap().job;
        let session = Workspace::for_chain(&work_root, Path::new(""), with_work.id()).session;
        std::fs::create_dir_all(&session).unwrap();
        std::fs::write(
            session.join("s.jsonl"),
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let read = store.list_running_jobs().await.unwrap();
        assert_eq!(read.len(), 2);
        let ids = (with_work.id(), without.id());
        with_work
            .fail_unstarted("ended by its owner")
            .await
            .unwrap();
        without.fail_unstarted("ended by its owner").await.unwrap();

        let settled = settle_jobs(store, &work_root, &read, Orphaned::Abandoned)
            .await
            .unwrap();

        assert_eq!(settled, 0);
        let jobs = store.list_jobs(10).await.unwrap();
        for id in [ids.0, ids.1] {
            let job = &jobs.iter().find(|j| j.job.id == id).unwrap().job;
            assert_eq!(
                (job.state, job.notes.as_deref()),
                (JobState::Failed, Some("ended by its owner")),
                "job #{id}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
