//! Data access against the live hunter.db (WAL mode).
//!
//! # Conventions
//! - Every query uses an EXPLICIT column list — never `SELECT *`.
//! - Mutation methods use **specific, purpose-named methods** with
//!   `sqlx::query!` macros — fully compile-time checked SQL, no
//!   `QueryBuilder` for mutations.
//! - One method, one unit of work: a handler never opens a transaction of
//!   its own or spans one across two `Store` calls. Most methods are a
//!   single statement and autocommit. A method whose statements must land
//!   together owns its transaction internally -- `add_repo` (the insert and
//!   the id-derived clone path), `soft_delete_repo` (the findings/jobs
//!   refusal checks and the flag, under `BEGIN IMMEDIATE` so the checks
//!   cannot go stale before the write), `record_open_pr` and
//!   `mark_pr_engaged` (a `pr_state` write and the attention settled from
//!   it, `crate::attention`), `create_job` (the insert
//!   and the end of the checkpoint it supersedes, with that event),
//!   `complete_job` (the outcome and the work item's checkpoint), and
//!   `record_closed_harvest` (a closed PR's classification and its harvest
//!   stamp). Those transactions close real races; do not unwind them to make a method look like the others.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::attention::{AnswerRecorded, EngageAnswer, ObservedOpenPr, OpenPrRecorded};
use crate::auth::CurrentUser;
use crate::clones::{ClonePermit, Clones};
use crate::domain::{
    BudgetOverride, BugClass, ClosureClass, FindingJobKind, FindingStatus, FindingType,
    FinishedState, ForgeName, HoldReason, JobKind, JobState, ReleaseAction, RepoJobKind,
    RepoWorkKind, Severity,
};
use crate::recovery::{
    DaemonLock, JobAuthority, Owner, RecordedJob, Recovered, Recovery, StrandedClaim, StrandedJob,
    Unreconciled,
};
use crate::transition::{
    Moved, PrEnded, PrEnding, PrRef, Record, ReleaseVerdict, Sources, Transition, Unclaimed,
    decline_status,
};
use crate::types::{
    Event, Finding, Job, JobListEntry, PrState, Repo, SchedulerState, StatsByFinding, StatsByKind,
    StatsTotals,
};
use crate::util::now_ms;

mod pr_state;

pub use pr_state::{PrRead, PrSyncPass};

/// A `pr_state` row as the store read it ([`Store::get_pr_state`]).
///
/// Only the store makes one, and it is read-only: its fields are read
/// through [`Deref`](std::ops::Deref), never set. So whatever is built
/// from one -- an engage's decline
/// ([`Declined::as_read`](crate::attention::Declined::as_read)) -- names
/// only what a write recorded, never a [`PrState`] put together elsewhere.
#[derive(Debug, Clone)]
pub struct RecordedPrState(PrState);

impl RecordedPrState {
    /// The row, for a caller that hands it on (the API's `/api/finding`).
    pub fn into_inner(self) -> PrState {
        self.0
    }
}

impl std::ops::Deref for RecordedPrState {
    type Target = PrState;

    fn deref(&self) -> &PrState {
        &self.0
    }
}

/// The newest PR feedback (epoch ms) an engage's worker was shown: the
/// mark its answer raises the engagement watermark to.
///
/// Made only from the forge's view of the PR ([`Self::of`]), or read back
/// from the job that recorded it ([`Store::seen_activity`]), so no answer
/// can raise the watermark past feedback that was actually shown. The
/// watermark only ever rises: a mark taken from anywhere else -- a clock,
/// a constant -- would hide every comment posted before it, for good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeenActivity(i64);

impl SeenActivity {
    /// The newest screened feedback on `pr` (`attention::latest_activity_ms`).
    pub fn of(pr: &crate::forge::PrView) -> Self {
        Self(crate::attention::latest_activity_ms(pr))
    }

    /// A mark a test chose: only in a `test-support` build.
    #[cfg(feature = "test-support")]
    pub const fn fixture(at_ms: i64) -> Self {
        Self(at_ms)
    }
}

/// A write transaction that took SQLite's write lock when it began
/// (`BEGIN IMMEDIATE`), not at its first write as sqlx's plain `begin()`
/// does. Nothing else can commit between two of its statements, so a
/// check it reads still holds when it writes: every guarded write in this
/// module (a status checked, then moved; a row read, then settled) is
/// correct only inside one. The only way to make one is [`Self::begin`],
/// private to this module, and the helpers that rely on the lock take
/// this type, not a `sqlx::Transaction`: handing them a deferred
/// transaction does not compile.
struct ImmediateTx(sqlx::Transaction<'static, sqlx::Sqlite>);

impl ImmediateTx {
    async fn begin(pool: &SqlitePool) -> sqlx::Result<Self> {
        pool.begin_with("BEGIN IMMEDIATE").await.map(Self)
    }

    async fn commit(self) -> sqlx::Result<()> {
        self.0.commit().await
    }
}

impl std::ops::Deref for ImmediateTx {
    type Target = sqlx::SqliteConnection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ImmediateTx {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// The verdict a `dep_update` gets when a Renovate scan stops proposing it
/// ([`Store::supersede_unproposed_dep_updates`]). Also the marker that lets
/// a later scan proposing it again bring it back
/// ([`Store::refresh_dep_update`]); a finding superseded for any other
/// reason stays retired.
pub const DEP_UNPROPOSED_REASON: &str = "no longer proposed by the dependency scan";

/// One package move a dependency scan proposes: the finding that makes it
/// (`fingerprint`) and its unit (`unit`, that fingerprint up to where the
/// versions begin), the package, and whether that package's own update is
/// a major one. A group's `update_type` is its strongest member's, so the
/// class has to come from here, not from the finding.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProposedMove {
    pub fingerprint: String,
    pub unit: String,
    pub package: String,
    pub major: bool,
}

/// Write-path errors: a domain refusal (HTTP 400 with the exact message)
/// vs an underlying DB error (HTTP 500).
#[derive(Debug, thiserror::Error)]
pub enum StoreWriteError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The repos whose `dep_update` is the job lane's: Renovate had no answer
/// for them since their last `dep_update` run, so the AI scan is due
/// ([`Store::dep_update_fallbacks`]). Every other repo's `dep_update` is
/// the admin lane's Renovate scan (`scheduler::dependency_scans`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DepScanFallbacks(HashSet<i64>);

impl DepScanFallbacks {
    /// Whether `repo_id`'s AI `dep_update` scan is due.
    pub fn pending(&self, repo_id: i64) -> bool {
        self.0.contains(&repo_id)
    }
}

/// Why [`Store::upsert_finding`] wrote no finding.
#[derive(Debug, thiserror::Error)]
pub enum FindingInsertError {
    /// The repo was soft-deleted while the work that found this ran. Only
    /// work with no job row can get here -- a repo with a job or a finding
    /// cannot be deleted at all -- which today is the zero-token Renovate
    /// scan. A finding inserted anyway would be invisible (every read
    /// skips a deleted repo) and would hold the repo's row in place
    /// (`findings.repo_id` has no cascade), so the reaper's delete of it
    /// would fail on every pass.
    #[error("repo {repo_id} is deleted -- no finding can be filed in it")]
    RepoDeleted { repo_id: i64 },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why [`CycleScope::create_job`](crate::recovery::CycleScope::create_job)
/// wrote no job row.
#[derive(Debug, thiserror::Error)]
pub enum JobInsertError {
    /// The repo was soft-deleted after the run was picked: ordinary
    /// operator traffic, the cycle is skipped.
    #[error("repo {repo_id} is deleted -- cannot start a {kind} job")]
    RepoDeleted { repo_id: i64, kind: JobKind },
    /// Another job is `running` on the same finding, or for repo-level
    /// work on the same (repo, kind) (migration 031). Selection skips a
    /// place a job is running on ([`Store::running_places`]), so this is a
    /// fault to surface, not a race to absorb: the holder is named.
    #[error("{place}: job {holder} is still running -- cannot start a {kind} job beside it")]
    Occupied {
        place: JobPlace,
        holder: i64,
        kind: JobKind,
    },
    /// The attempt a resume or handoff names can no longer be continued:
    /// a resume's is not its work item's checkpoint any more (an operator
    /// restarted the work, or another attempt continued it), a handoff's
    /// engage is no engage or was already continued. Selection names only
    /// a current checkpoint, by the same pointer this insert reads, so
    /// meeting one means the work moved on between the pick and the
    /// insert: the cycle is skipped, and the next selection reads where
    /// the work is now.
    #[error("{start} can no longer be continued")]
    NotContinuable { start: JobStart },
    /// Fresh finding work names a finding with no row. Findings are never
    /// removed, so this is a fault, not a race.
    #[error("finding {finding_id} does not exist -- cannot start a {kind} job")]
    UnknownFinding {
        finding_id: i64,
        kind: FindingJobKind,
    },
    /// A harvest of a finding whose PR no longer awaits one: it was
    /// harvested since the pick, or the finding moved off `merged` and a
    /// closed PR's `closed`. The queue a harvest is picked from
    /// ([`Store::list_pending_harvest`]) no longer lists it, so losing
    /// that race is a skip.
    #[error("finding {finding_id} awaits no harvest -- cannot start one")]
    NoHarvestDue { finding_id: i64 },
    /// An engage of a finding that is no longer `pr_open`: the admin lane's
    /// PR sync recorded the PR's merge or close between the pick and the
    /// insert, which spans forge calls and a fetch. The engage queue no
    /// longer lists the finding, so losing that race is a skip.
    #[error("finding {finding_id} has no open PR -- cannot start an engage")]
    NoEngageDue { finding_id: i64 },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// How a job starts, which decides where it runs.
///
/// Fresh work names its target; a resume or a handoff names the attempt
/// it continues, and its place is read from that row inside the insert,
/// so a successor can never run anywhere but where its chain does: on
/// its predecessor's finding, or for repo-level work its (repo, kind).
/// Together with migration 031's indexes that gives an attempt at most
/// one live successor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStart {
    /// New work, superseding any checkpoint of the same work.
    Fresh(FreshWork),
    /// Continue `checkpoint`, which must still be its work item's
    /// checkpoint (`finding_work`/`repo_work.checkpoint_job_id`): same
    /// kind, repo and finding.
    Resume { checkpoint: i64 },
    /// Continue the finished engage `engage` into its finding's harvest,
    /// in the same tree. Only once: an engage with a successor is refused.
    Handoff { engage: i64 },
}

/// What fresh work is for: a finding-level kind on one finding, or a
/// repo-level kind on one repo. Nothing else can be named, so a job row
/// always has a finding exactly when its kind is a finding's. A finding's
/// work runs in the finding's own repo, read from its row inside the
/// insert, so no job can pair a finding with another repo's clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshWork {
    Finding {
        kind: FindingJobKind,
        finding_id: i64,
    },
    Repo {
        kind: RepoJobKind,
        repo_id: i64,
    },
}

impl JobStart {
    /// Fresh `kind` work on finding `finding_id`.
    pub const fn fresh_finding(kind: FindingJobKind, finding_id: i64) -> Self {
        Self::Fresh(FreshWork::Finding { kind, finding_id })
    }

    /// Fresh `kind` work on repo `repo_id`.
    pub const fn fresh_repo(kind: RepoJobKind, repo_id: i64) -> Self {
        Self::Fresh(FreshWork::Repo { kind, repo_id })
    }
}

impl std::fmt::Display for JobStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fresh(FreshWork::Finding { kind, finding_id }) => {
                write!(f, "fresh {kind} of #{finding_id}")
            }
            Self::Fresh(FreshWork::Repo { kind, repo_id }) => {
                write!(f, "fresh {kind} of repo {repo_id}")
            }
            Self::Resume { checkpoint } => write!(f, "resume of job {checkpoint}"),
            Self::Handoff { engage } => write!(f, "handoff of engage job {engage}"),
        }
    }
}

/// What one `running` job holds: its finding, or for repo-level work its
/// (repo, kind). Migration 031's indexes allow one `running` job per place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobPlace {
    Finding(i64),
    RepoWork { repo_id: i64, kind: JobKind },
}

impl JobPlace {
    /// The place migration 031's indexes give a job row of `kind` in repo
    /// `repo_id`, on `finding_id` if any. Keyed on the finding, as the
    /// indexes are: [`FreshWork`] and the rows a successor copies its
    /// place from make every row the daemon writes agree with its kind.
    pub fn of(kind: JobKind, repo_id: i64, finding_id: Option<i64>) -> Self {
        finding_id.map_or(Self::RepoWork { repo_id, kind }, Self::Finding)
    }
}

impl std::fmt::Display for JobPlace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Finding(fid) => write!(f, "#{fid}"),
            Self::RepoWork { repo_id, kind } => write!(f, "repo {repo_id} {kind}"),
        }
    }
}

/// How far [`Store::resume_chain_stats`] will walk a resume chain.
///
/// Purely a termination guarantee for a graph that should not need one:
/// far beyond any chain a give-up ceiling would let form, so it never
/// truncates a real answer, and small enough that a cyclic row from a
/// hand-repaired database costs a bounded query instead of a hung one.
const RESUME_CHAIN_MAX_DEPTH: i64 = 64;

/// What a resume chain has cost, across every attempt in it.
///
/// The three numbers are read in one walk because they answer one
/// question — has this work run out of road? — and a ceiling that read
/// them separately could see three different chains if a successor row
/// landed between the queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeChainStats {
    /// Every token the chain has spent.
    pub total: i64,
    /// How many attempts the chain contains, this one included.
    pub attempts: i64,
    /// The costliest single attempt in it. 0 when nothing in the chain
    /// has a metered cost yet.
    pub max_single: i64,
}

/// The identical-failure streak of one work item (`finding_work`): how
/// many attempts in a row failed, and the key they failed with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkStreak {
    pub streak: i64,
    pub last_failure: Option<String>,
}

/// Every work item held for the operator, from the one query that answers
/// "is this held?" ([`Store::held_work`]): finding work by `(finding,
/// kind)`, repo work by `(repo, kind)`, `sync` included.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldWork {
    pub findings: BTreeSet<(i64, FindingJobKind)>,
    pub repos: BTreeSet<(i64, RepoWorkKind)>,
}

impl HeldWork {
    /// Whether finding `fid`'s `kind` work is held.
    pub fn finding(&self, fid: i64, kind: FindingJobKind) -> bool {
        self.findings.contains(&(fid, kind))
    }

    /// Whether repo `rid`'s `sync` is held: then every kind of the repo
    /// waits for the operator (WORK-MODEL.md §17).
    pub fn repo_sync(&self, rid: i64) -> bool {
        self.repos.contains(&(rid, RepoWorkKind::Sync))
    }

    /// Whether repo `rid`'s `kind` work waits for the operator: its own
    /// hold, or the repo's `sync` hold.
    pub fn repo(&self, rid: i64, kind: RepoJobKind) -> bool {
        self.repo_sync(rid) || self.repos.contains(&(rid, kind.into()))
    }
}

/// One active hold on a repo's work (`work_holds` with a `repo_id`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoHold {
    pub id: i64,
    pub repo_id: i64,
    pub kind: RepoWorkKind,
    pub reason: HoldReason,
    pub detail: String,
    pub held_at: i64,
    /// The work item's checkpoint: `continue` needs one.
    pub checkpoint_job_id: Option<i64>,
}

impl RepoHold {
    /// The actions that can release this hold, in [`ReleaseAction::ALL`]'s
    /// order.
    pub fn actions(&self) -> Vec<ReleaseAction> {
        ReleaseAction::ALL
            .into_iter()
            .filter(|a| {
                a.applies_to_repo(self.kind)
                    && (*a != ReleaseAction::Continue || self.checkpoint_job_id.is_some())
            })
            .collect()
    }
}

/// A repo work item failing without a hold yet: its streak so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoStreak {
    pub repo_id: i64,
    pub kind: RepoWorkKind,
    pub streak: i64,
    pub last_failure: String,
}

/// One active hold (`work_holds`), with what its release depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkHold {
    pub id: i64,
    pub finding_id: i64,
    pub kind: FindingJobKind,
    pub reason: HoldReason,
    pub detail: String,
    pub held_at: i64,
    /// The work item's checkpoint: `continue` needs one.
    pub checkpoint_job_id: Option<i64>,
    /// The finding's status: a harvest's release depends on how its PR
    /// ended.
    pub status: FindingStatus,
    /// An engage hold's PR has had activity since it was held. It stays
    /// held: whether that activity resolves it is the operator's call
    /// (WORK-MODEL.md §9.4).
    pub new_activity: bool,
}

impl WorkHold {
    /// The actions that can release this hold, in [`ReleaseAction::ALL`]'s
    /// order.
    pub fn actions(&self) -> Vec<ReleaseAction> {
        ReleaseAction::ALL
            .into_iter()
            .filter(|a| {
                a.applies_to(self.kind)
                    && action_fits(*a, self.status, self.checkpoint_job_id.is_some())
            })
            .collect()
    }
}

/// Whether `action` fits the held work as it stands: `continue` needs a
/// checkpoint, and a harvest is classified only for a closed PR and marked
/// harvested only for a merged one.
fn action_fits(action: ReleaseAction, status: FindingStatus, has_checkpoint: bool) -> bool {
    match action {
        ReleaseAction::Continue => has_checkpoint,
        ReleaseAction::Classify => status == FindingStatus::Closed,
        ReleaseAction::MarkHarvested => status == FindingStatus::Merged,
        _ => true,
    }
}

/// An operator's release of a hold ([`Store::release_hold`]).
#[derive(Debug, Clone, Copy)]
pub struct HoldRelease<'a> {
    pub action: ReleaseAction,
    /// The verdict's reason: required by `reject`, `wontfix` and `classify`.
    pub reason: Option<&'a str>,
    /// How a closed PR ended: required by `classify`.
    pub classification: Option<ClosureClass>,
    pub by: &'a CurrentUser,
}

/// A job row just inserted, and the checkpoints its insert superseded.
/// The store's half of [`CycleScope::create_job`](crate::recovery::CycleScope::create_job),
/// which turns `id` into the [`RunningJob`](crate::recovery::RunningJob) that
/// owns the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InsertedJob {
    pub(crate) id: i64,
    /// Ids of the checkpoints this fresh job superseded, oldest first.
    pub(crate) superseded: Vec<i64>,
}

/// Which clone: a repo's id and its directory, as one repo row names them.
///
/// The lock is keyed by the id and the git commands run in the directory,
/// so the two must come from the same row. Only this module makes one,
/// from a row it read ([`Lane::lock_clone`](crate::clones::Lane::lock_clone), [`ChainStatus::clone`]):
/// neither a caller's id and path nor a [`Repo`] put together elsewhere
/// can name a clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneRef {
    repo_id: i64,
    dir: PathBuf,
}

impl CloneRef {
    pub const fn repo_id(&self) -> i64 {
        self.repo_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Which chain, and which repo's clone its tree was added from: the
/// repo of the chain's origin job row, as [`Store::chain_ref`] read it.
///
/// A chain's tree is leased under its clone's permit
/// ([`ClonePermit::lease`](crate::clones::ClonePermit::lease)), and that
/// is the clone the sweep locks to look for the lease
/// ([`ChainStatus::clone`], from the same row). Only this module makes
/// one, so a caller cannot pair a chain with another repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainRef {
    origin_id: i64,
    repo_id: i64,
}

impl ChainRef {
    pub const fn origin_id(&self) -> i64 {
        self.origin_id
    }

    pub const fn repo_id(&self) -> i64 {
        self.repo_id
    }
}

/// Why [`Lane::lock_clone`](crate::clones::Lane::lock_clone) gave no permit.
#[derive(Debug, thiserror::Error)]
pub enum CloneLockError {
    /// No row, live or flagged deleted, has this id.
    #[error("no repo {0} -- it has no clone to lock")]
    NoRepo(i64),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Whether a chain's working tree is still needed; see
/// [`Store::chain_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStatus {
    /// Rows found in the chain. 0 means no job owns the workspace at all.
    pub attempts: i64,
    /// Attempts that are `running`, or `suspended` and not yet continued.
    /// Nonzero means the tree is in use or will be resumed, and must not
    /// be touched.
    pub live: i64,
    /// When the chain's latest attempt finished, if any has.
    pub last_finished_at: Option<i64>,
    /// The clone the chain's tree was added from, if its repo still exists.
    pub clone: Option<CloneRef>,
}

/// [`ChainStatus`] as the query reads it: the clone's two halves come
/// from one repo row, and only become a [`CloneRef`] together.
struct ChainRow {
    attempts: i64,
    live: i64,
    last_finished_at: Option<i64>,
    repo_id: Option<i64>,
    clone: Option<String>,
}

impl From<ChainRow> for ChainStatus {
    fn from(row: ChainRow) -> Self {
        Self {
            attempts: row.attempts,
            live: row.live,
            last_finished_at: row.last_finished_at,
            clone: row.repo_id.zip(row.clone).map(|(repo_id, dir)| CloneRef {
                repo_id,
                dir: PathBuf::from(dir),
            }),
        }
    }
}

/// A running or suspended job, as the workspace sweep needs to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveJob {
    pub id: i64,
    pub kind: JobKind,
    pub finding_id: Option<i64>,
    pub session_file: Option<String>,
}

/// A job that recorded a session file, as the legacy session sweep ages it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef {
    pub session_file: String,
    pub state: JobState,
    pub finished_at: Option<i64>,
}

/// Everything [`RunningJob::complete`](crate::recovery::RunningJob::complete) writes when a job finishes.
///
/// Named fields rather than positional arguments: the outcome carries
/// four optional strings and three integers whose order the compiler
/// cannot check, and a swapped `tokens_new`/`calls` corrupts the ledger
/// the per-kind token budget is estimated from.
#[derive(Debug, Clone, Copy)]
pub struct JobOutcome<'a> {
    /// How the attempt ended. Never `running`: a job is `running` only
    /// from its creation in an owning scope until its outcome is recorded.
    pub state: FinishedState,
    pub tokens_new: i64,
    pub calls: i64,
    pub exit_code: Option<i64>,
    pub killed_reason: Option<&'a str>,
    pub session_file: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub model: Option<&'a str>,
    pub usage_delta: Option<f64>,
    pub finished_at: i64,
}

/// Allowed /api/repo update fields, post-coercion (WRITES contract §6).
#[derive(Debug, Default)]
pub struct RepoUpdate {
    pub enabled: Option<i64>,
    pub url: Option<String>,
    pub default_branch: Option<String>,
    pub forge: Option<ForgeName>,
    pub name: Option<String>,
}

/// Typed insert for `upsert_finding` — compile-time field safety instead of
/// runtime `.get()` on raw JSON.
#[derive(Debug, Clone, Default)]
pub struct FindingInsert {
    pub fingerprint: String,
    pub file: String,
    pub symbol: Option<String>,
    pub line: Option<i64>,
    pub severity: Severity,
    pub confidence: f64,
    pub summary: String,
    pub detail: Option<String>,
    pub bug_class: Option<BugClass>,
    pub evidence_plan: Option<String>,
    pub introduced_by: Option<String>,
    pub ecosystem: Option<String>,
    pub package: Option<String>,
    pub current_version: Option<String>,
    pub latest_version: Option<String>,
    pub update_type: Option<String>,
    pub security_advisory: Option<String>,
    pub missing_tests: Option<String>,
    pub test_file: Option<String>,
    pub smell_type: Option<String>,
    pub suggested_refactor: Option<String>,
    pub modernization_class: Option<String>,
    pub current_approach: Option<String>,
    pub proposed_approach: Option<String>,
    pub standard_section: Option<String>,
}

/// A confirmed recheck's revised analysis, written with its
/// `rechecking → new` move ([`crate::transition::Transition::RecheckConfirmed`]).
#[derive(Debug, Clone, Default)]
pub struct FindingAnalysisUpdate {
    pub summary: Option<String>,
    pub detail: Option<String>,
    pub confidence: Option<f64>,
    pub severity: Option<Severity>,
}

/// Filters for GET /api/findings (all optional, combined with AND).
#[derive(Debug, Default)]
pub struct FindingFilter {
    pub status: Option<FindingStatus>,
    pub repo_id: Option<i64>,
    pub kind: Option<FindingType>,
    /// Minimum severity rank (`Severity::rank`); expands to "at or above".
    pub min_severity_rank: Option<i64>,
}

/// Tail-truncation limit for repo notes (`Store._MAX_NOTES_CHARS`).
const MAX_NOTES_CHARS: usize = 4000;
const NOTES_TRUNCATION_PREFIX: &str = "...(older notes truncated)...\n";

/// (YYYY-MM-DD, HH:MM) in UTC for repo-note timestamps. Python uses
/// `datetime.now()` (LOCAL time) here; deriving the local offset without a
/// time crate isn't worth it, so we store UTC — an accepted, documented
/// deviation (notes are informational free text, never parsed).
fn utc_date_time() -> (String, String) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    (
        format!("{y:04}-{m:02}-{d:02}"),
        format!("{:02}:{:02}", tod / 3_600, (tod % 3_600) / 60),
    )
}

/// Proleptic-Gregorian date from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`, exact for the full i64-day range we can encounter).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// What a suppressing verdict was judged against (`verdict_anchors`): the
/// commit the worker's tree was at, the files the verdict depends on, and
/// the worker's one-line statement of the condition that makes it true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerdictAnchor {
    pub sha: String,
    /// Repo-relative paths.
    pub files: Vec<String>,
    pub holds_while: Option<String>,
}

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    /// This handle's startup recovery and the ledger of settles it still
    /// owes. Opaque here: only `crate::recovery` reads or moves it, so no
    /// code outside that module can mint a token, mark the store
    /// recovered or drop owed work. Shared by clones, which are the same
    /// handle.
    recovery: Recovery,
    /// Whether a [`PrSyncPass`] is live on this handle: set by its
    /// `begin`, cleared by its drop. Shared by clones, which are the same
    /// handle.
    pr_sync: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Who may change each repo clone, and which chain trees runners
    /// hold (`crate::clones`). Shared by clones of the handle, so the HTTP
    /// handlers and the scheduler wait for each other.
    clones: Clones,
}

/// INSERT one event row on `ex` — the pool, or an open transaction when
/// the event must land with the change it records.
async fn insert_event<'e, E: sqlx::SqliteExecutor<'e>>(
    ex: E,
    kind: &str,
    message: &str,
    job_id: Option<i64>,
    finding_id: Option<i64>,
    user_id: Option<i64>,
) -> sqlx::Result<()> {
    let at = now_ms();
    sqlx::query!(
        "INSERT INTO events (at, kind, message, job_id, finding_id, user_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        at,
        kind,
        message,
        job_id,
        finding_id,
        user_id
    )
    .execute(ex)
    .await?;
    Ok(())
}

/// Where finding `finding_id` is now (`None`: there is no such finding),
/// for the [`Moved::Conflict`] of a guarded write that did not land.
async fn status_now<'e, E: sqlx::SqliteExecutor<'e>>(
    ex: E,
    finding_id: i64,
) -> sqlx::Result<Option<FindingStatus>> {
    sqlx::query_scalar!(
        r#"SELECT status AS "status!: FindingStatus" FROM findings WHERE id = ?1"#,
        finding_id
    )
    .fetch_optional(ex)
    .await
}

/// Move the checkpoint of the work item `job_id` belongs to: to `job_id`
/// when `paused`, otherwise off it. An attempt that ended without pausing
/// has consumed whatever its chain continued from, so nothing is left to
/// resume. A finding's work only while the finding is in that kind's tier
/// (`work_tiers`): work that has left it keeps no checkpoint. A repo-level
/// job's work item is `(repo, kind)` (`repo_work`), active while the repo
/// exists.
async fn point_checkpoint(tx: &mut ImmediateTx, job_id: i64, paused: bool) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO finding_work (finding_id, kind, checkpoint_job_id) \
         SELECT j.finding_id, j.kind, CASE WHEN ?2 THEN j.id END \
         FROM jobs j \
         JOIN findings f ON f.id = j.finding_id \
         JOIN work_tiers t ON t.kind = j.kind AND t.status = f.status \
         WHERE j.id = ?1 \
         ON CONFLICT (finding_id, kind) DO UPDATE SET \
             checkpoint_job_id = excluded.checkpoint_job_id",
        job_id,
        paused
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "INSERT INTO repo_work (repo_id, kind, checkpoint_job_id) \
         SELECT j.repo_id, j.kind, CASE WHEN ?2 THEN j.id END \
         FROM jobs j \
         WHERE j.id = ?1 AND j.finding_id IS NULL \
           AND j.kind IN ('hunt', 'test_gap', 'dep_update', 'refactor', 'modernization', \
                          'standards') \
         ON CONFLICT (repo_id, kind) DO UPDATE SET \
             checkpoint_job_id = excluded.checkpoint_job_id",
        job_id,
        paused
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The verdict a release writes, if its action is one, with its reason.
/// Refused when it lacks its reason, or `classify` its classification.
fn release_verdict(
    action: ReleaseAction,
    reason: Option<&str>,
    classification: Option<ClosureClass>,
) -> Result<Option<(ReleaseVerdict, String)>, StoreWriteError> {
    let reason = reason.map(str::trim).filter(|r| !r.is_empty());
    let refuse = |msg: String| Err(StoreWriteError::Refused(msg));
    Ok(match action {
        ReleaseAction::Reject | ReleaseAction::Wontfix => {
            let Some(reason) = reason else {
                return refuse(format!("{action} needs a reason"));
            };
            let verdict = if action == ReleaseAction::Reject {
                ReleaseVerdict::Rejected
            } else {
                ReleaseVerdict::Wontfix
            };
            Some((verdict, reason.chars().take(500).collect()))
        }
        ReleaseAction::Classify => {
            let (Some(class), Some(reason)) = (classification, reason) else {
                return refuse("classify needs a classification and a reason".to_owned());
            };
            Some((
                ReleaseVerdict::Classified(class),
                format!("{class}: {reason}").chars().take(500).collect(),
            ))
        }
        _ => None,
    })
}

/// Write a release's verdict inside [`Store::release_hold`]'s transaction,
/// with its `verdict` event as the operator's. `from` is the finding's
/// status read with the hold in that transaction. The verdict moves the
/// finding out of the tier, which ends the work item (migration 024's
/// trigger); the hold is already released, so it is not released again as
/// `finding-left-tier`. A `classify` also stamps the closed PR harvested,
/// as its harvest would have.
async fn write_release_verdict(
    tx: &mut ImmediateTx,
    finding_id: i64,
    from: FindingStatus,
    verdict: ReleaseVerdict,
    text: &str,
    user_id: i64,
    now: i64,
) -> Result<(), StoreWriteError> {
    let Ok(from) = Unclaimed::try_from(from) else {
        return Err(StoreWriteError::Refused(format!(
            "#{finding_id} is claimed by a fix; a release cannot move it"
        )));
    };
    let release = Transition::HoldReleased {
        from,
        verdict,
        reason: text,
    };
    if let Moved::Conflict { now } = Store::apply_transition(tx, finding_id, &release).await? {
        // `from` was read in this transaction, so this is a store fault.
        return Err(StoreWriteError::Refused(format!(
            "#{finding_id} moved from {from} to {now:?} inside its release"
        )));
    }
    let to = release.to();
    if matches!(verdict, ReleaseVerdict::Classified(_)) {
        pr_state::stamp_harvested(&mut **tx, finding_id, now).await?;
    }
    let message = format!("finding {finding_id} -> {to}: {text}");
    insert_event(
        &mut **tx,
        "verdict",
        &message,
        None,
        Some(finding_id),
        Some(user_id),
    )
    .await?;
    Ok(())
}

/// The effect of releasing a hold with `action` on the work item and the
/// PR, inside [`Store::release_hold`]'s transaction: `restart` drops the
/// checkpoint, `mark_harvested` stamps the PR harvested, `dismiss` marks
/// its current attention handled as a no-op engage would and drops the
/// engage's checkpoint, since the finding stays in the engage tier and no
/// trigger ends its work.
async fn apply_release(
    tx: &mut ImmediateTx,
    finding_id: i64,
    kind: FindingJobKind,
    action: ReleaseAction,
    now: i64,
) -> sqlx::Result<()> {
    match action {
        ReleaseAction::Restart => {
            sqlx::query!(
                "UPDATE finding_work SET checkpoint_job_id = NULL \
                 WHERE finding_id = ?1 AND kind = ?2",
                finding_id,
                kind
            )
            .execute(&mut **tx)
            .await?;
        }
        ReleaseAction::MarkHarvested => {
            pr_state::stamp_harvested(&mut **tx, finding_id, now).await?;
        }
        ReleaseAction::Dismiss => {
            // Dismissing ends the engage, as a no-op engage would: nothing
            // may resume it and act on the PR the operator dismissed.
            sqlx::query!(
                "UPDATE finding_work SET checkpoint_job_id = NULL, streak = 0, last_failure = NULL \
                 WHERE finding_id = ?1 AND kind = ?2",
                finding_id,
                kind
            )
            .execute(&mut **tx)
            .await?;
            pr_state::record_dismiss(tx, finding_id, now).await?;
        }
        // `continue` keeps the checkpoint; a verdict is written by the
        // caller, with its event.
        ReleaseAction::Continue
        | ReleaseAction::Reject
        | ReleaseAction::Wontfix
        | ReleaseAction::Classify => {}
    }
    Ok(())
}

impl Store {
    // Private — all SQL goes through typed store methods.
    // No code outside this module can access the raw pool (field is not pub).

    // ── counting helpers (daemon sleep logic) ─────────────────────────

    /// Queued fixes the scheduler can work on: held ones stay `queued` but
    /// wait for the operator, and counting them would keep the daemon
    /// cycling without sleep for nothing.
    pub async fn count_queued(&self) -> sqlx::Result<i64> {
        let status = FindingStatus::Queued;
        let fix = FindingJobKind::Fix;
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM findings f WHERE f.status = ?1
               AND NOT EXISTS (SELECT 1 FROM work_holds h WHERE h.finding_id = f.id
                               AND h.kind = ?2 AND h.released_at IS NULL)"#,
            status,
            fix
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn count_enabled_repos(&self) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM repos WHERE enabled = 1 AND deleted_at IS NULL"#
        )
        .fetch_one(&self.pool)
        .await
    }

    // ── repo timestamp helpers (scheduler) ────────────────────────────

    /// Set `last_dep_update_at` to now.
    pub async fn set_last_dep_update(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_dep_update_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set `last_standards_at` to now.
    pub async fn set_last_standards_at(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_standards_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Advance a rotation-kind's `last_{kind}_at` timestamp.
    /// Uses per-kind compile-time checked queries (no `QueryBuilder`).
    pub async fn set_last_kind_at(&self, repo_id: i64, kind: RepoJobKind) -> sqlx::Result<()> {
        let now = now_ms();
        match kind {
            RepoJobKind::Hunt => {
                sqlx::query!(
                    "UPDATE repos SET last_hunt_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::TestGap => {
                sqlx::query!(
                    "UPDATE repos SET last_test_gap_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::DepUpdate => {
                sqlx::query!(
                    "UPDATE repos SET last_dep_update_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Refactor => {
                sqlx::query!(
                    "UPDATE repos SET last_refactor_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Modernization => {
                sqlx::query!(
                    "UPDATE repos SET last_modernization_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
            RepoJobKind::Standards => {
                sqlx::query!(
                    "UPDATE repos SET last_standards_at = ?1 WHERE id = ?2",
                    now,
                    repo_id
                )
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }
}

/// The embedded migration chain.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

impl Store {
    /// Open the DB strictly read-only -- physically unable to write, so a
    /// caller that must not mutate cannot. Used by the tests to assert a
    /// read path touches nothing.
    pub async fn connect_read_only(db_path: &Path) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .read_only(true)
            .create_if_missing(false);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        Ok(Self::new(pool, Recovery::default()))
    }

    /// Open the live DB read-write. Runs embedded migrations on connect —
    /// the binary carries its own schema, so deploying a new binary
    /// automatically migrates the DB.
    ///
    /// The handle has no startup token: [`Recovered`], and so any job,
    /// is out of its reach. The daemon connects through
    /// [`Self::connect_exclusive`] instead.
    pub async fn connect(db_path: &Path) -> anyhow::Result<Self> {
        Ok(Self::new(
            Self::open_rw(db_path).await?,
            Recovery::default(),
        ))
    }

    /// [`Self::connect`] for the daemon: the database `lock` holds, with
    /// its one startup token ([`crate::recovery::reconcile_and_log`]).
    ///
    /// The store keeps `lock` until its last clone is dropped, so the
    /// lock outlives every token and every job this handle can make: no
    /// second daemon can reconcile this database while they exist.
    pub async fn connect_exclusive(lock: DaemonLock) -> anyhow::Result<(Self, Unreconciled)> {
        let pool = Self::open_rw(lock.db_path()).await?;
        let store = Self::new(pool, Recovery::exclusive(lock));
        let token = crate::recovery::take_unreconciled(&store)
            .ok_or_else(|| anyhow::anyhow!("a store just connected has no startup token"))?;
        Ok((store, token))
    }

    async fn open_rw(db_path: &Path) -> anyhow::Result<SqlitePool> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            // Must be set here rather than by migration 001's `PRAGMA
            // journal_mode = WAL`: sqlx runs each migration inside a
            // transaction and SQLite refuses to change journal mode there,
            // so on a database that is not already WAL that migration
            // fails outright. As a connect option it runs outside any
            // transaction, and 001's pragma is then the no-op it has to be.
            // 001 cannot simply drop the pragma: it is already applied
            // everywhere, and sqlx rejects a migration whose text changed.
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        MIGRATOR.run(&pool).await?;
        Ok(pool)
    }

    fn new(pool: SqlitePool, recovery: Recovery) -> Self {
        Self {
            pool,
            recovery,
            pr_sync: std::sync::Arc::default(),
            clones: Clones::default(),
        }
    }

    /// This handle's clone locks and tree leases.
    pub(crate) const fn clones(&self) -> &Clones {
        &self.clones
    }

    /// Which clone repo `repo_id`'s row names, for
    /// [`Lane::lock_clone`](crate::clones::Lane::lock_clone). A row flagged deleted still names one: it
    /// stays until its clone is removed.
    pub(crate) async fn clone_ref(&self, repo_id: i64) -> Result<CloneRef, CloneLockError> {
        let path = sqlx::query_scalar!("SELECT path FROM repos WHERE id = ?1", repo_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(CloneLockError::NoRepo(repo_id))?;
        Ok(CloneRef {
            repo_id,
            dir: PathBuf::from(path),
        })
    }

    /// Every repo row's id, deleted or not, in id order: the clones on
    /// disk. A row flagged deleted keeps its clone until it is reaped, so
    /// whatever changes every clone (the sweep) must hold its permit too.
    pub(crate) async fn clone_ids(&self) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar!(r#"SELECT id AS "id!" FROM repos ORDER BY id"#)
            .fetch_all(&self.pool)
            .await
    }

    /// The URL `permit`'s repo row names now. Read under the permit, it is
    /// the clone's origin too: a URL update holds the permit across its
    /// write and the clone's repoint (`server::update_repo`).
    pub async fn origin_url(&self, permit: &ClonePermit<'_>) -> sqlx::Result<String> {
        sqlx::query_scalar!("SELECT url FROM repos WHERE id = ?1", permit.repo_id())
            .fetch_one(&self.pool)
            .await
    }

    /// Chain `origin_id` and the repo its tree belongs to, read from its
    /// origin job row. `None` when no job has that id.
    pub async fn chain_ref(&self, origin_id: i64) -> sqlx::Result<Option<ChainRef>> {
        let repo_id = sqlx::query_scalar!("SELECT repo_id FROM jobs WHERE id = ?1", origin_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(repo_id.map(|repo_id| ChainRef { origin_id, repo_id }))
    }

    /// This handle's recovery state, for `crate::recovery` alone: every
    /// method on it is private to that module.
    pub(crate) const fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// The startup token of a store a test just made, which no daemon
    /// lock guards; `None` once it was handed out. Only in a
    /// `test-support` build: the daemon's token comes with its lock
    /// ([`Self::connect_exclusive`]).
    #[cfg(feature = "test-support")]
    pub fn take_unreconciled(&self) -> Option<Unreconciled> {
        crate::recovery::take_unreconciled(self)
    }

    /// Proof that this store is recovered, once its [`Unreconciled`]
    /// token was spent on it. Only in a `test-support` build: the daemon's
    /// one [`Recovered`](crate::recovery::Recovered) is the one `recovery::reconcile_and_log` returns,
    /// and the right to create jobs is handed on from there, never read
    /// back from a store handle.
    #[cfg(feature = "test-support")]
    pub fn recovered(&self) -> Option<crate::recovery::Recovered> {
        crate::recovery::recovered(self)
    }

    // -- writes (API-CONTRACT-WRITES.md; commit-per-method) -------------------

    /// SELECT all 39 columns FROM findings WHERE id = ? (embedded row in
    /// verdict/recheck/unqueue/override success bodies).
    pub async fn get_finding(&self, id: i64) -> sqlx::Result<Option<Finding>> {
        Self::read_finding(&self.pool, id).await
    }

    /// [`Self::get_finding`], on `executor`: the pool, or a transaction
    /// that reads the row it has just written.
    async fn read_finding<'e>(
        executor: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
        id: i64,
    ) -> sqlx::Result<Option<Finding>> {
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override AS "budget_override: BudgetOverride", ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE id = ?1
            "#,
            id
        )
        .fetch_optional(executor)
        .await
    }

    /// Set a finding's status whatever it was: a fixture write for tests
    /// that start from a given status. Only in a `test-support` build: the
    /// daemon moves a finding only through [`Self::transition`] or a fix
    /// claim's holder.
    ///
    /// Never to `fixing`: that claim has an owner that releases it if its
    /// fix does not, which only [`CycleScope::claim_fix`](crate::recovery::CycleScope::claim_fix) records.
    #[cfg(feature = "test-support")]
    pub async fn set_finding_status(
        &self,
        finding_id: i64,
        status: FindingStatus,
    ) -> Result<(), StoreWriteError> {
        if status == FindingStatus::Fixing {
            return Err(StoreWriteError::Refused(format!(
                "finding #{finding_id}: a fix claim needs an owner (CycleScope::claim_fix)"
            )));
        }
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET status = ?1, updated_at = ?2 WHERE id = ?3",
            status,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// [`Self::set_anchored_verdict`] without an anchor: a fixture write.
    #[cfg(feature = "test-support")]
    pub async fn set_finding_verdict(
        &self,
        finding_id: i64,
        status: FindingStatus,
        reason: &str,
    ) -> sqlx::Result<()> {
        self.set_anchored_verdict(finding_id, status, reason, None)
            .await
    }

    /// A verdict whatever the status was, recording `anchor` (or removing
    /// the previous verdict's): a fixture write, only in a `test-support`
    /// build, as [`Self::set_finding_status`].
    #[cfg(feature = "test-support")]
    pub async fn set_anchored_verdict(
        &self,
        finding_id: i64,
        status: FindingStatus,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 WHERE id = ?4",
            status,
            reason,
            now,
            finding_id
        )
        .execute(&mut *tx)
        .await?;
        Self::write_anchor(&mut tx, finding_id, anchor).await?;
        tx.commit().await
    }

    /// Make `transition` on `finding_id`, only while the finding is still
    /// at one of the transition's sources. One transaction: the status, and
    /// what the transition records with it (a verdict's reason and anchor,
    /// an analysis), land together or not at all. [`Moved::Conflict`] names
    /// where the finding is instead. A PR's end is [`Self::end_pr`].
    pub async fn transition(
        &self,
        finding_id: i64,
        transition: &Transition<'_>,
    ) -> sqlx::Result<Moved> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let moved = Self::apply_transition(&mut tx, finding_id, transition).await?;
        tx.commit().await?;
        Ok(moved)
    }

    /// [`Self::transition`] inside a writer's own transaction, for an edge
    /// that lands with other writes (a hold's release, a harvest's stamp,
    /// the other half of a dep-update handover).
    async fn apply_transition(
        tx: &mut ImmediateTx,
        finding_id: i64,
        transition: &Transition<'_>,
    ) -> sqlx::Result<Moved> {
        let (from, to, record) = (transition.from(), transition.to(), transition.record());
        if Self::apply_edge(tx, finding_id, from, to, record, now_ms()).await? {
            return Ok(Moved::Applied);
        }
        let now = status_now(&mut **tx, finding_id).await?;
        Ok(Moved::Conflict { now })
    }

    /// End `finding_id`'s PR `pr` ([`PrEnding`]): the finding's move and
    /// the end in `pr_state`, in one write, only while the finding is still
    /// `pr_open` with that PR (`pr_state::end`).
    pub async fn end_pr(
        &self,
        finding_id: i64,
        pr: &PrRef,
        ending: PrEnding<'_>,
    ) -> sqlx::Result<PrEnded> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let ended = pr_state::end(&mut tx, finding_id, pr, ending).await?;
        tx.commit().await?;
        Ok(ended)
    }

    /// Take over the PR sync's record of the closure an engage's
    /// withdrawal made, the sync having recorded it first: the worker's
    /// `reason` replaces the sync's. `true` when it was that closure
    /// (`pr_state::adopt_closure`).
    pub async fn adopt_closure(
        &self,
        finding_id: i64,
        pr: &PrRef,
        reason: &str,
    ) -> sqlx::Result<bool> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let adopted = pr_state::adopt_closure(&mut tx, finding_id, pr, reason).await?;
        tx.commit().await?;
        Ok(adopted)
    }

    /// Move `finding_id` from one of `from` to `to`, writing `record` with
    /// it; `true` when the finding was at one of `from` and moved.
    #[allow(
        clippy::too_many_lines,
        reason = "one guarded UPDATE per kind of record: `query!` takes one literal each, so \
                  the arms cannot share their SQL"
    )]
    async fn apply_edge(
        tx: &mut ImmediateTx,
        finding_id: i64,
        from: Sources,
        to: Unclaimed,
        record: Record<'_>,
        now: i64,
    ) -> sqlx::Result<bool> {
        let (from_a, from_b) = from.pair();
        let to = FindingStatus::from(to);
        let applied = match record {
            Record::StatusOnly => {
                sqlx::query!(
                    "UPDATE findings SET status = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND status IN (?4, ?5)",
                    to,
                    now,
                    finding_id,
                    from_a,
                    from_b
                )
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0
            }
            Record::Analysis(fields) => {
                let summary = fields.summary.as_deref();
                let detail = fields.detail.as_deref();
                let severity = fields.severity;
                sqlx::query!(
                    "UPDATE findings SET status = ?1, updated_at = ?2, \
                     summary = COALESCE(?3, summary), \
                     detail = COALESCE(?4, detail), \
                     confidence = COALESCE(?5, confidence), \
                     severity = COALESCE(?6, severity) \
                     WHERE id = ?7 AND status IN (?8, ?9)",
                    to,
                    now,
                    summary,
                    detail,
                    fields.confidence,
                    severity,
                    finding_id,
                    from_a,
                    from_b
                )
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0
            }
            Record::Rescan(row) => {
                let symbol = row.symbol.as_deref();
                let detail = row.detail.as_deref();
                let evidence_plan = row.evidence_plan.as_deref();
                let set = sqlx::query!(
                    "UPDATE findings SET status = ?1, verdict_reason = NULL, symbol = ?2, \
                     line = ?3, severity = ?4, confidence = ?5, summary = ?6, detail = ?7, \
                     evidence_plan = ?8, updated_at = ?9 \
                     WHERE id = ?10 AND status IN (?11, ?12)",
                    to,
                    symbol,
                    row.line,
                    row.severity,
                    row.confidence,
                    row.summary,
                    detail,
                    evidence_plan,
                    now,
                    finding_id,
                    from_a,
                    from_b
                )
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0;
                if set {
                    Self::write_anchor(tx, finding_id, None).await?;
                }
                set
            }
            Record::Verdict { reason, anchor } => {
                let set = sqlx::query!(
                    "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
                     WHERE id = ?4 AND status IN (?5, ?6)",
                    to,
                    reason,
                    now,
                    finding_id,
                    from_a,
                    from_b
                )
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0;
                if set {
                    Self::write_anchor(tx, finding_id, anchor).await?;
                }
                set
            }
            Record::InheritOverride { from_id } => {
                let exempt = BudgetOverride::Exempt;
                sqlx::query!(
                    "UPDATE findings SET status = ?1, updated_at = ?2, \
                     budget_override = CASE \
                         WHEN budget_override = ?3 \
                           OR (SELECT budget_override FROM findings WHERE id = ?4) = ?3 \
                         THEN ?3 \
                         ELSE COALESCE(budget_override, \
                             (SELECT budget_override FROM findings WHERE id = ?4)) \
                     END \
                     WHERE id = ?5 AND status IN (?6, ?7)",
                    to,
                    now,
                    exempt,
                    from_id,
                    finding_id,
                    from_a,
                    from_b
                )
                .execute(&mut **tx)
                .await?
                .rows_affected()
                    > 0
            }
        };
        Ok(applied)
    }

    /// The verdict of a fix the worker declined, its classification's
    /// status ([`decline_status`]) with the anchor, applied only while the
    /// finding is still `fixing`; [`Moved::Conflict`] names where it is
    /// instead, and nothing is written.
    /// Takes an [`Owner`], which only `crate::recovery` makes, inside
    /// [`FixClaim::conclude`](crate::recovery::FixClaim::conclude): only the
    /// claim's holder moves a finding out of `fixing`.
    pub(crate) async fn conclude_fixing(
        &self,
        _owner: &Owner,
        finding_id: i64,
        class: Option<ClosureClass>,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
    ) -> sqlx::Result<Moved> {
        let (now, fixing) = (now_ms(), FindingStatus::Fixing);
        let status = FindingStatus::from(decline_status(class));
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let set = sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
             WHERE id = ?4 AND status = ?5",
            status,
            reason,
            now,
            finding_id,
            fixing
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        let moved = if set {
            Self::write_anchor(&mut tx, finding_id, anchor).await?;
            Moved::Applied
        } else {
            Moved::Conflict {
                now: status_now(&mut *tx, finding_id).await?,
            }
        };
        tx.commit().await?;
        Ok(moved)
    }

    /// Replace `finding_id`'s verdict anchor with `anchor`, or remove it.
    async fn write_anchor(
        tx: &mut ImmediateTx,
        finding_id: i64,
        anchor: Option<&VerdictAnchor>,
    ) -> sqlx::Result<()> {
        let Some(anchor) = anchor else {
            sqlx::query!(
                "DELETE FROM verdict_anchors WHERE finding_id = ?1",
                finding_id
            )
            .execute(&mut **tx)
            .await?;
            return Ok(());
        };
        let files = serde_json::to_string(&anchor.files).unwrap_or_else(|_| "[]".to_owned());
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO verdict_anchors (finding_id, sha, files, holds_while, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(finding_id) DO UPDATE SET sha = excluded.sha, files = excluded.files, \
             holds_while = excluded.holds_while, created_at = excluded.created_at",
            finding_id,
            anchor.sha,
            files,
            anchor.holds_while,
            now
        )
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// The anchors of `repo_id`'s suppressed findings of `finding_type`,
    /// keyed by finding id: the counterpart of [`Self::suppressions`].
    pub async fn suppression_anchors(
        &self,
        repo_id: i64,
        finding_type: &str,
    ) -> sqlx::Result<BTreeMap<i64, VerdictAnchor>> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        let rows = sqlx::query!(
            "SELECT a.finding_id AS \"finding_id!\", a.sha, a.files, a.holds_while \
             FROM verdict_anchors a JOIN findings f ON f.id = a.finding_id \
             WHERE f.repo_id = ?1 AND f.type = ?2 AND f.status IN (?3, ?4)",
            repo_id,
            finding_type,
            s1,
            s2
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let files = serde_json::from_str(&r.files).unwrap_or_default();
                (
                    r.finding_id,
                    VerdictAnchor {
                        sha: r.sha,
                        files,
                        holds_while: r.holds_while,
                    },
                )
            })
            .collect())
    }

    /// `finding_id`'s verdict anchor, if its verdict has one.
    pub async fn verdict_anchor(&self, finding_id: i64) -> sqlx::Result<Option<VerdictAnchor>> {
        let row = sqlx::query!(
            "SELECT sha, files, holds_while FROM verdict_anchors WHERE finding_id = ?1",
            finding_id
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| VerdictAnchor {
            sha: r.sha,
            files: serde_json::from_str(&r.files).unwrap_or_default(),
            holds_while: r.holds_while,
        }))
    }

    /// A scan re-checked an anchored verdict whose code had changed and
    /// found it still holds: move the anchor to `sha`, the commit it was
    /// re-checked at, so the next scan does not re-check it again, and
    /// replace its condition when the scan restated it. Nothing moves once
    /// the finding is no longer suppressed.
    pub async fn reconfirm_anchor(
        &self,
        finding_id: i64,
        sha: &str,
        holds_while: Option<&str>,
    ) -> sqlx::Result<()> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        let now = now_ms();
        sqlx::query!(
            "UPDATE verdict_anchors SET sha = ?1, holds_while = COALESCE(?2, holds_while), \
             created_at = ?3 \
             WHERE finding_id = ?4 \
               AND finding_id IN (SELECT id FROM findings WHERE status IN (?5, ?6))",
            sha,
            holds_while,
            now,
            finding_id,
            s1,
            s2
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set finding status to `pr_open` with PR URL, whatever it was: a
    /// fixture write for tests that start from an open PR. Only in a
    /// `test-support` build: the daemon moves a finding to `pr_open` only
    /// through [`FixClaim::open_pr`](crate::recovery::FixClaim::open_pr).
    #[cfg(feature = "test-support")]
    pub async fn set_finding_pr_open(&self, finding_id: i64, pr_url: &str) -> sqlx::Result<()> {
        let now = now_ms();
        let pr_open = FindingStatus::PrOpen;
        sqlx::query!(
            "UPDATE findings SET status = ?1, pr_url = ?2, updated_at = ?3 WHERE id = ?4",
            pr_open,
            pr_url,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `fixing` → `pr_open` with `pr_url`, only while the finding is still
    /// `fixing`; [`Moved::Conflict`] names where it is instead, and nothing
    /// is written. Takes an [`Owner`], which only `crate::recovery` makes,
    /// inside [`FixClaim::open_pr`](crate::recovery::FixClaim::open_pr).
    pub(crate) async fn open_pr_from_fixing(
        &self,
        _owner: &Owner,
        finding_id: i64,
        pr_url: &str,
    ) -> sqlx::Result<Moved> {
        let (now, pr_open, fixing) = (now_ms(), FindingStatus::PrOpen, FindingStatus::Fixing);
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let set = sqlx::query!(
            "UPDATE findings SET status = ?1, pr_url = ?2, updated_at = ?3 \
             WHERE id = ?4 AND status = ?5",
            pr_open,
            pr_url,
            now,
            finding_id,
            fixing
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        let moved = if set {
            Moved::Applied
        } else {
            Moved::Conflict {
                now: status_now(&mut *tx, finding_id).await?,
            }
        };
        tx.commit().await?;
        Ok(moved)
    }

    /// UPDATE findings SET `budget_override` = ?, `updated_at` = now WHERE id = ?.
    /// `None` clears the override.
    pub async fn set_budget_override(
        &self,
        finding_id: i64,
        mode: Option<BudgetOverride>,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET budget_override = ?1, updated_at = ?2 WHERE id = ?3",
            mode,
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE findings SET `budget_override` = NULL, `updated_at` = now
    /// WHERE `budget_override` IS NOT NULL; returns rows affected.
    pub async fn clear_all_overrides(&self) -> sqlx::Result<i64> {
        let now = now_ms();
        let result = sqlx::query!(
            "UPDATE findings SET budget_override = NULL, updated_at = ?1 \
             WHERE budget_override IS NOT NULL",
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() as i64)
    }

    /// Directory a repo is cloned into: `<repos_dir>/repo-<id>`.
    ///
    /// Derived from the id, never from the name. A name is whatever the
    /// operator typed, and a filesystem is not a string store: names
    /// differing only in case collide on NTFS and APFS, `CON` and `NUL`
    /// are reserved on Windows, trailing dots are silently stripped
    /// there, and anything past 255 bytes is `ENAMETOOLONG` — which would
    /// surface at hunt time, long after the repo was accepted. Every
    /// charset rule that could be written here is a denylist against an
    /// open-ended set, so the name is display-only and the id owns the
    /// path. `NOTES.md` has always been keyed this way.
    pub fn repo_dir(repos_dir: &Path, repo_id: i64) -> PathBuf {
        repos_dir.join(format!("repo-{repo_id}"))
    }

    /// Where a repo's notes live: `<work_root>/notes/repo-<id>.md`.
    ///
    /// Outside the clone, and deliberately so. Notes used to be written
    /// to `repos/repo-<id>/NOTES.md`, which put them inside the working
    /// tree of a real git checkout, with two consequences.
    ///
    /// The file showed up as untracked in the clone, so any worker doing
    /// a broad `git add` would commit the operator's private notes into
    /// a pull request.
    ///
    /// And writing a note created `repos/repo-<id>/` as a side effect.
    /// `sync_repo` treats an existing path as an already-cloned repo and
    /// checks its `origin`; a directory holding only notes has no origin
    /// to read, so it refused to work there — permanently, since nothing
    /// removes it. Adding a note to a repo before its first cycle was
    /// enough to make that repo uncloneable for good.
    ///
    /// Keyed by id for the same reason as the clone directory: the name
    /// is whatever the operator typed.
    pub fn notes_path(work_root: &Path, repo_id: i64) -> PathBuf {
        work_root.join("notes").join(format!("repo-{repo_id}.md"))
    }

    /// INSERT INTO repos (name, url, path, forge, `default_branch`, `added_at`);
    /// returns new id. `path` is `<repos_dir>/repo-<id>`, so it can only be
    /// written once the id exists — both statements share a transaction to
    /// rule out a row whose path never got filled in.
    pub async fn add_repo(
        &self,
        name: &str,
        url: &str,
        repos_dir: &Path,
        default_branch: &str,
        forge: ForgeName,
    ) -> sqlx::Result<i64> {
        let added_at = now_ms();
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query!(
            "INSERT INTO repos (name, url, path, forge, default_branch, added_at) \
             VALUES (?1, ?2, '', ?3, ?4, ?5)",
            name,
            url,
            forge,
            default_branch,
            added_at
        )
        .execute(&mut *tx)
        .await?;
        let id = result.last_insert_rowid();
        let path = Self::repo_dir(repos_dir, id).to_string_lossy().into_owned();
        sqlx::query!("UPDATE repos SET path = ?1 WHERE id = ?2", path, id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(id)
    }

    /// UPDATE repos — each field is set only when non-NULL, else the
    /// existing value is preserved via COALESCE.
    pub async fn update_repo(&self, id: i64, fields: &RepoUpdate) -> sqlx::Result<()> {
        if fields.enabled.is_none()
            && fields.url.is_none()
            && fields.default_branch.is_none()
            && fields.forge.is_none()
            && fields.name.is_none()
        {
            return Ok(());
        }
        let url = fields.url.as_deref();
        let branch = fields.default_branch.as_deref();
        let forge = fields.forge.map(super::domain::ForgeName::as_str);
        let name = fields.name.as_deref();
        sqlx::query!(
            "UPDATE repos SET \
             enabled = COALESCE(?1, enabled), \
             url = COALESCE(?2, url), \
             default_branch = COALESCE(?3, default_branch), \
             forge = COALESCE(?4, forge), \
             name = COALESCE(?5, name) \
             WHERE id = ?6",
            fields.enabled,
            url,
            branch,
            forge,
            name,
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Refuses (exact Python message: "repo <id> has <n> finding(s) and
    /// <m> job(s) -- cannot delete without losing history; pause it
    /// instead") when findings or jobs reference the repo; else flags the
    /// row `deleted_at = now`, which removes it from every read path.
    ///
    /// Phase one of two. The row deliberately survives its own deletion:
    /// it is the only record that `repos/repo-<id>` is still on disk, and
    /// removing a large clone is not instant. [`reap_deleted_repos`]
    /// removes the directory and drops the row only once that succeeded,
    /// so an interrupted or refused removal leaves a flagged row the next
    /// pass retries instead of a directory nothing owns -- and `sync_repo`
    /// treats any directory at that path as an existing clone.
    ///
    /// The count checks and the flag share one transaction, so a job or
    /// finding created concurrently cannot slip in between them.
    pub async fn soft_delete_repo(&self, id: i64) -> Result<(), StoreWriteError> {
        // BEGIN IMMEDIATE rather than sqlx's plain `begin()`: a deferred
        // transaction takes no write lock until its first write, so the
        // counts below would be read outside it and a job inserted
        // concurrently could land between the check and the flag. Taking
        // the lock up front makes the check and the flag see one state.
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let findings = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM findings WHERE repo_id = ?1"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        let jobs = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM jobs WHERE repo_id = ?1"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if findings > 0 || jobs > 0 {
            return Err(StoreWriteError::Refused(format!(
                "repo {id} has {findings} finding(s) and {jobs} job(s) -- \
                 cannot delete without losing history; pause it instead"
            )));
        }
        // Its work items and holds go with it (WORK-MODEL.md §17): a repo
        // with no jobs can still have a failing or held `sync`.
        sqlx::query!("DELETE FROM repo_work WHERE repo_id = ?1", id)
            .execute(&mut *tx)
            .await?;
        sqlx::query!("DELETE FROM work_holds WHERE repo_id = ?1", id)
            .execute(&mut *tx)
            .await?;
        // The name is released here, not at reap time: `repos.name` is
        // UNIQUE, so a flagged row would otherwise keep rejecting the name
        // of a repo the operator has already been told is gone. Suffixing
        // with the id keeps it unique without a table rebuild to drop the
        // constraint, and the row is invisible to every read path anyway --
        // only the reaper looks at it, and only by id.
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos
             SET deleted_at = ?1, name = name || ' (deleted #' || id || ')'
             WHERE id = ?2 AND deleted_at IS NULL",
            now,
            id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Is this id a repo that was deleted but not yet reclaimed?
    ///
    /// Only the delete endpoint asks, so that a retry of a request whose
    /// response was lost is still a success rather than a 404.
    pub async fn repo_is_deleted(&self, id: i64) -> sqlx::Result<bool> {
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM repos
               WHERE id = ?1 AND deleted_at IS NOT NULL"#,
            id
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Ids of repos awaiting reclamation, oldest first.
    pub async fn deleted_repo_ids(&self) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM repos
               WHERE deleted_at IS NOT NULL ORDER BY deleted_at"#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Drop a flagged row once its directory and notes are gone. The row is
    /// what records that `repos/repo-<id>` and the notes file are still on
    /// disk -- the reaper only finds leftovers through it -- so dropping it
    /// first would orphan them for good. It does not free the id:
    /// `repos.id` is AUTOINCREMENT (migration 009), so SQLite never reissues
    /// it.
    pub async fn forget_deleted_repo(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "DELETE FROM repos WHERE id = ?1 AND deleted_at IS NOT NULL",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// INSERT INTO events (at = now, kind, message, `job_id`, `finding_id`).
    pub async fn log_event(
        &self,
        kind: &str,
        message: &str,
        job_id: Option<i64>,
        finding_id: Option<i64>,
    ) -> sqlx::Result<()> {
        insert_event(&self.pool, kind, message, job_id, finding_id, None).await
    }

    /// [`Store::log_event`] for something a person did through the API,
    /// recorded as theirs.
    pub async fn log_user_event(
        &self,
        user: &CurrentUser,
        kind: &str,
        message: &str,
        finding_id: Option<i64>,
    ) -> sqlx::Result<()> {
        insert_event(&self.pool, kind, message, None, finding_id, Some(user.id)).await
    }

    // -- users / sessions --------------------------------------------------------

    /// Create an account. A taken username (compared case-insensitively)
    /// is refused.
    pub async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<i64, StoreWriteError> {
        let now = now_ms();
        let inserted = sqlx::query_scalar!(
            r#"INSERT INTO users (username, password_hash, created_at)
               VALUES (?1, ?2, ?3)
               ON CONFLICT(username) DO NOTHING
               RETURNING id AS "id!: i64""#,
            username,
            password_hash,
            now
        )
        .fetch_optional(&self.pool)
        .await?;
        inserted
            .ok_or_else(|| StoreWriteError::Refused(format!("user {username:?} already exists")))
    }

    /// Replace an account's password and end its sessions, so a password
    /// changed because it leaked also logs out whoever used it. `false`
    /// when no such account exists.
    pub async fn set_password(&self, username: &str, password_hash: &str) -> sqlx::Result<bool> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let id = sqlx::query_scalar!(
            r#"UPDATE users SET password_hash = ?2 WHERE username = ?1
               RETURNING id AS "id!: i64""#,
            username,
            password_hash
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        sqlx::query!("DELETE FROM sessions WHERE user_id = ?1", id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Disable an account and end its sessions. The row stays, so events
    /// it authored keep their author. `false` when no such account exists.
    pub async fn disable_user(&self, username: &str) -> sqlx::Result<bool> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let now = now_ms();
        let id = sqlx::query_scalar!(
            r#"UPDATE users SET disabled_at = COALESCE(disabled_at, ?2) WHERE username = ?1
               RETURNING id AS "id!: i64""#,
            username,
            now
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(id) = id else {
            return Ok(false);
        };
        sqlx::query!("DELETE FROM sessions WHERE user_id = ?1", id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// `(user, password hash)` for an account that may log in; `None` for
    /// an unknown or disabled name.
    pub async fn login_candidate(
        &self,
        username: &str,
    ) -> sqlx::Result<Option<(CurrentUser, String)>> {
        let row = sqlx::query!(
            r#"SELECT id AS "id!: i64", username, password_hash
               FROM users WHERE username = ?1 AND disabled_at IS NULL"#,
            username
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            (
                CurrentUser {
                    id: r.id,
                    username: r.username,
                },
                r.password_hash,
            )
        }))
    }

    /// Record a session for `user`, valid until `expires_at` (epoch ms).
    /// Sessions that have already expired are cleared out on the way,
    /// since nothing else ever deletes them.
    pub async fn create_session(
        &self,
        user: &CurrentUser,
        token_hash: &[u8],
        expires_at: i64,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query!("DELETE FROM sessions WHERE expires_at <= ?1", now)
            .execute(&mut *tx)
            .await?;
        sqlx::query!(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4)",
            token_hash,
            user.id,
            now,
            expires_at
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// The account behind a session token hash, if the session exists, has
    /// not expired at `now` (epoch ms), and its account is not disabled.
    pub async fn session_user(
        &self,
        token_hash: &[u8],
        now: i64,
    ) -> sqlx::Result<Option<CurrentUser>> {
        let row = sqlx::query!(
            r#"SELECT u.id AS "id!: i64", u.username
               FROM sessions s JOIN users u ON u.id = s.user_id
               WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND u.disabled_at IS NULL"#,
            token_hash,
            now
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| CurrentUser {
            id: r.id,
            username: r.username,
        }))
    }

    /// End one session (logout). Unknown tokens are not an error.
    pub async fn delete_session(&self, token_hash: &[u8]) -> sqlx::Result<()> {
        sqlx::query!("DELETE FROM sessions WHERE token_hash = ?1", token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Append to [`Store::notes_path`] (creating dir +
    /// header "# Notes: {`repo_name}\n\nLast` updated: YYYY-MM-DD\n\n" on
    /// first write), entry "## {category}\n" (when Some) +
    /// "- [{YYYY-MM-DD HH:MM}] {note}\n\n" (UTC — Python wrote local
    /// time; accepted deviation, see `utc_date_time`). Returns the
    /// bounded re-read (same truncation as `repo_notes`).
    pub fn append_repo_note(
        work_root: &Path,
        repo_id: i64,
        repo_name: &str,
        note: &str,
        category: Option<&str>,
    ) -> std::io::Result<String> {
        use std::fmt::Write;
        let path = Self::notes_path(work_root, repo_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let (date, hhmm) = utc_date_time();
        let mut entry = String::new();
        if !path.exists() {
            // "Last updated" is written once at creation, never refreshed
            // (`Store.append_repo_note`).
            let _ = write!(entry, "# Notes: {repo_name}\n\nLast updated: {date}\n\n");
        }
        if let Some(category) = category {
            let _ = writeln!(entry, "## {category}");
        }
        let _ = write!(entry, "- [{date} {hhmm}] {note}\n\n");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(entry.as_bytes())?;
        Ok(Self::repo_notes(work_root, repo_id))
    }

    // -- repos ---------------------------------------------------------------

    /// SELECT <cols> FROM repos ORDER BY name (BINARY collation).
    pub async fn list_repos(&self) -> sqlx::Result<Vec<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at, full_hunt_requested_at,
                   full_hunt_request_attempted
            FROM repos
            WHERE deleted_at IS NULL
            ORDER BY name
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_repo_by_id(&self, id: i64) -> sqlx::Result<Option<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at, full_hunt_requested_at,
                   full_hunt_request_attempted
            FROM repos
            WHERE id = ?1 AND deleted_at IS NULL
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn get_repo_by_name(&self, name: &str) -> sqlx::Result<Option<Repo>> {
        sqlx::query_as!(
            Repo,
            r#"
            SELECT id, name, url, path, forge AS "forge: ForgeName", default_branch, last_hunt_sha,
                   last_hunt_at, enabled, added_at, last_full_hunt_at,
                   last_test_gap_at, last_dep_update_at, last_refactor_at,
                   last_modernization_at, last_standards_at, full_hunt_requested_at,
                   full_hunt_request_attempted
            FROM repos
            WHERE name = ?1 AND deleted_at IS NULL
            "#,
            name
        )
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn finding_exists(&self, id: i64) -> sqlx::Result<bool> {
        sqlx::query_scalar!(r#"SELECT 1 AS "one!: i64" FROM findings WHERE id = ?1"#, id)
            .fetch_optional(&self.pool)
            .await
            .map(|row| row.is_some())
    }

    /// NOT in the DB: reads [`Store::notes_path`], "" when missing,
    /// tail-truncated to 4000 chars with the
    /// "...(older notes truncated)...\n" prefix (`Store.repo_notes`).
    pub fn repo_notes(work_root: &Path, repo_id: i64) -> String {
        let path = Self::notes_path(work_root, repo_id);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return String::new();
        };
        let total = text.chars().count();
        if total <= MAX_NOTES_CHARS {
            return text;
        }
        // Python slices by character (text[-4000:]); mirror that, not bytes.
        let start = text
            .char_indices()
            .nth(total - MAX_NOTES_CHARS)
            .map_or(0, |(i, _)| i);
        let mut out = String::with_capacity(NOTES_TRUNCATION_PREFIX.len() + text.len() - start);
        out.push_str(NOTES_TRUNCATION_PREFIX);
        out.push_str(&text[start..]);
        out
    }

    // -- findings ------------------------------------------------------------

    /// ORDER BY id DESC, no LIMIT. Severity filter via rank CASE expression.
    pub async fn list_findings(&self, filter: &FindingFilter) -> sqlx::Result<Vec<Finding>> {
        let status = filter.status.map(|s| s.as_str().to_owned());
        let status = status.as_deref();
        let kind = filter.kind.map(super::domain::FindingType::as_str);
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override AS "budget_override: BudgetOverride", ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE (?1 IS NULL OR status = ?1)
              AND (?2 IS NULL OR repo_id = ?2)
              AND (?3 IS NULL OR type = ?3)
              AND (?4 IS NULL
                   OR CASE severity
                        WHEN 'high' THEN 3
                        WHEN 'medium' THEN 2
                        ELSE 1
                      END >= ?4)
            ORDER BY id DESC
            "#,
            status,
            filter.repo_id,
            kind,
            filter.min_severity_rank
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Events grouped by `finding_id`, ascending id order within each group.
    /// Timelines for the findings being returned, keyed by finding id.
    ///
    /// Scoped to `ids` rather than reading the whole table: `events` grows
    /// for the life of the service, and this runs on every
    /// `GET /api/findings`, which the UI polls. Unscoped, a filter
    /// matching nothing still paid for the entire history. Python bound
    /// the id list too (`Store.events_by_finding(fids)`).
    ///
    /// The ids go in as a JSON array joined through `json_each`, because
    /// `query_as!` cannot bind a variable-length `IN` list and this keeps
    /// the query compile-time checked.
    pub async fn events_by_finding(&self, ids: &[i64]) -> sqlx::Result<BTreeMap<i64, Vec<Event>>> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let ids_json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
        let rows = sqlx::query_as!(
            Event,
            r#"
            SELECT e.id AS "id!", e.at AS "at!", e.kind AS "kind!", e.message AS "message!",
                   e.job_id, e.finding_id, u.username AS "username?"
            FROM events e
            LEFT JOIN users u ON u.id = e.user_id
            JOIN json_each(?1) ids ON ids.value = e.finding_id
            "#,
            ids_json
        )
        .fetch_all(&self.pool)
        .await?;
        let mut grouped: BTreeMap<i64, Vec<Event>> = BTreeMap::new();
        for event in rows {
            if let Some(fid) = event.finding_id {
                grouped.entry(fid).or_default().push(event);
            }
        }
        // Ascending id per finding, as the contract requires. Sorted here
        // rather than by `ORDER BY`: the index join yields rows grouped by
        // finding, so ordering in SQL costs a temp B-tree over every row
        // returned, while these per-finding runs are short.
        for events in grouped.values_mut() {
            events.sort_unstable_by_key(|e| e.id);
        }
        Ok(grouped)
    }

    /// `(source, follow_up)` pairs touching `ids` on either side: findings
    /// filed by a job that was working on another finding (engage and
    /// harvest `FOLLOW-UPS.json`), paired with that finding.
    ///
    /// Derived rather than stored: the follow-up's `found_by_job` names the
    /// job, and the job's `finding_id` names the source. Ingesting jobs
    /// (hunts, analysis) have no `finding_id`, so their findings have no
    /// source and never appear here. Scoped to `ids` for the same reason as
    /// [`Self::events_by_finding`]; ordered by follow-up id so each source
    /// lists its follow-ups oldest first.
    pub async fn follow_up_pairs(&self, ids: &[i64]) -> sqlx::Result<Vec<(i64, i64)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(ids).unwrap_or_else(|_| "[]".to_owned());
        let rows = sqlx::query!(
            r#"
            SELECT j.finding_id AS "source!: i64", f.id AS "follow_up!: i64"
            FROM findings f
            JOIN jobs j ON j.id = f.found_by_job
            WHERE f.found_by_job IS NOT NULL
              AND j.finding_id IS NOT NULL
              AND (f.id IN (SELECT value FROM json_each(?1))
                   OR j.finding_id IN (SELECT value FROM json_each(?1)))
            ORDER BY f.id
            "#,
            ids_json
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| (r.source, r.follow_up)).collect())
    }

    pub async fn get_pr_state(&self, finding_id: i64) -> sqlx::Result<Option<RecordedPrState>> {
        sqlx::query_as!(
            PrState,
            r#"
            SELECT finding_id, pr_number, state, mergeable, checks, head_ref,
                   last_activity_at, last_engaged_activity_at, needs_attention,
                   attention_since, attention_fingerprint,
                   addressed_fingerprint, head_sha, addressed_head_sha,
                   synced_at, harvested_at
            FROM pr_state
            WHERE finding_id = ?1
            "#,
            finding_id
        )
        .fetch_optional(&self.pool)
        .await
        .map(|row| row.map(RecordedPrState))
    }

    /// `pr_state` rows for all `pr_open` findings in one query:
    /// `finding_id` -> `needs_attention` (row presence matters, value may be null).
    pub async fn pr_attention(&self) -> sqlx::Result<BTreeMap<i64, Option<String>>> {
        let pr_open = FindingStatus::PrOpen;
        let rows = sqlx::query!(
            r#"
            SELECT p.finding_id AS "finding_id!: i64", p.needs_attention
            FROM pr_state p
            JOIN findings f ON f.id = p.finding_id
            WHERE f.status = ?1
            "#,
            pr_open
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.finding_id, r.needs_attention))
            .collect())
    }

    /// All `FindingStatus` keys zero-filled, then GROUP BY status counts.
    pub async fn status_counts(&self) -> sqlx::Result<BTreeMap<String, i64>> {
        let mut counts: BTreeMap<String, i64> = FindingStatus::ALL
            .iter()
            .map(|s| (s.as_str().to_owned(), 0))
            .collect();
        let rows = sqlx::query!(
            r#"
            SELECT status, COUNT(*) AS "n!: i64"
            FROM findings
            GROUP BY status
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        for row in rows {
            counts.insert(row.status, row.n);
        }
        Ok(counts)
    }

    /// GROUP BY type — only observed types, possibly empty.
    pub async fn type_counts(&self) -> sqlx::Result<BTreeMap<String, i64>> {
        let rows = sqlx::query!(
            r#"
            SELECT type AS "kind!: String", COUNT(*) AS "n!: i64"
            FROM findings
            GROUP BY type
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| (r.kind, r.n)).collect())
    }

    // -- jobs ----------------------------------------------------------------

    /// jobs JOIN repos, ORDER BY j.id DESC LIMIT ?. finding_* keys stay None.
    /// Recent jobs, each carrying the findings it produced.
    ///
    /// `produced_finding_ids` comes from one correlated subquery rather
    /// than a query per job: this feeds a 50-row table that polls, so an
    /// N+1 here would be 50 extra round trips every few seconds.
    /// `group_concat` returns NULL for a job that produced nothing,
    /// which is the common case (every fix, recheck and engage job), so
    /// the empty vector is the normal result and not an error.
    pub async fn list_jobs(&self, limit: i64) -> sqlx::Result<Vec<JobListEntry>> {
        let rows = sqlx::query!(
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   (SELECT group_concat(f.id) FROM findings f WHERE f.found_by_job = j.id)
                       AS "produced?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            ORDER BY j.id DESC
            LIMIT ?1
            "#,
            limit
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| JobListEntry {
                produced_finding_ids: r
                    .produced
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|s| s.parse().ok())
                    .collect(),
                job: Job {
                    id: r.id,
                    kind: r.kind,
                    repo_id: r.repo_id,
                    finding_id: r.finding_id,
                    state: r.state,
                    pid: r.pid,
                    session_file: r.session_file,
                    cap_tokens: r.cap_tokens,
                    tokens_new: r.tokens_new,
                    calls: r.calls,
                    exit_code: r.exit_code,
                    killed_reason: r.killed_reason,
                    notes: r.notes,
                    model: r.model,
                    usage_delta: r.usage_delta,
                    started_at: r.started_at,
                    finished_at: r.finished_at,
                    repo_name: r.repo_name,
                    finding_summary: None,
                    finding_fingerprint: None,
                },
            })
            .collect())
    }

    /// Complete history for one finding, ORDER BY j.id DESC, no limit.
    pub async fn jobs_by_finding(&self, finding_id: i64) -> sqlx::Result<Vec<Job>> {
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.finding_id = ?1
            ORDER BY j.id DESC
            "#,
            finding_id
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Newest running job with `finding_summary/finding_fingerprint` populated
    /// via correlated subqueries (NULL -> absent key, see `types::Job`).
    pub async fn current_job(&self) -> sqlx::Result<Option<Job>> {
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   (SELECT f.summary FROM findings f WHERE f.id = j.finding_id)
                       AS "finding_summary?: String",
                   (SELECT f.fingerprint FROM findings f WHERE f.id = j.finding_id)
                       AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.state = ?1
            ORDER BY j.id DESC
            LIMIT 1
            "#,
            running
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Running jobs only, filtered in SQL — reconcile must not drag the whole
    /// job history into memory. Same join and newest-first ordering as
    /// `list_jobs`; finding_* keys stay None.
    pub async fn list_running_jobs(&self) -> sqlx::Result<Vec<Job>> {
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE j.state = ?1
            ORDER BY j.id DESC
            "#,
            running
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Checkpoints that could actually be picked up again, newest first.
    ///
    /// Newest first because a suspended transcript loses value as its
    /// repo moves on: the freshest one was reasoning about a tree closest
    /// to today's HEAD, so continuing it redoes the least. That also
    /// matches every other job read here, so the scheduler's candidate
    /// list runs in the same direction as the UI's job feed. The
    /// starvation this ordering permits is the intended outcome, not a
    /// defect — a suspension nothing has come back to for many cycles is
    /// stale work, and the scheduler's give-up ceiling ends it.
    ///
    /// A checkpoint is a work item's `checkpoint_job_id`: a repo's
    /// (`repo_work`) or a finding's (`finding_work`). Three more filters
    /// make "could be picked up again" true rather than merely claimed:
    /// - the repo must still be live, since a soft-deleted repo's clone
    ///   is being reaped and `create_job` would refuse the successor;
    /// - `session_file` must be present, because resume means handing
    ///   omp that exact session path. A checkpoint with no path has
    ///   nothing to continue and would silently become a fresh run —
    ///   the implicit-resume behaviour that once re-cached an unrelated
    ///   290-call transcript for 508,709 tokens on a single call;
    /// - no attempt continuing it still runs, and a finding's work resumes
    ///   only at the status its kind starts from: a fix at `queued`, not
    ///   `fixing` (claimed). Held work is still listed: whether it is held
    ///   is the one shared query, [`Self::held_work`], which the scheduler
    ///   applies to this list as to every tier.
    pub async fn list_resumable_jobs(&self) -> sqlx::Result<Vec<Job>> {
        let running = JobState::Running;
        let fix = JobKind::Finding(FindingJobKind::Fix);
        let queued = FindingStatus::Queued;
        let recheck = JobKind::Finding(FindingJobKind::Recheck);
        let rechecking = FindingStatus::Rechecking;
        let engage = JobKind::Finding(FindingJobKind::Engage);
        let pr_open = FindingStatus::PrOpen;
        let harvest = JobKind::Finding(FindingJobKind::Harvest);
        let merged = FindingStatus::Merged;
        let closed = FindingStatus::Closed;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM jobs j
            JOIN repos r ON r.id = j.repo_id
            WHERE r.deleted_at IS NULL
              AND j.session_file IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id AND s.state = ?10)
              AND (EXISTS (
                  SELECT 1 FROM repo_work w
                  WHERE w.checkpoint_job_id = j.id AND w.repo_id = j.repo_id AND w.kind = j.kind
                ) OR EXISTS (
                  SELECT 1 FROM finding_work w JOIN findings f ON f.id = w.finding_id
                  WHERE w.checkpoint_job_id = j.id AND w.kind = j.kind AND (
                      (j.kind = ?1 AND f.status = ?2)
                      OR (j.kind = ?3 AND f.status = ?4)
                      OR (j.kind = ?5 AND f.status = ?6)
                      OR (j.kind = ?7 AND f.status IN (?8, ?9))
                  )
              ))
            ORDER BY j.id DESC
            "#,
            fix,
            queued,
            recheck,
            rechecking,
            engage,
            pr_open,
            harvest,
            merged,
            closed,
            running
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The job `finding_id`'s `kind` work continues from, if it has a
    /// checkpoint that can be continued: one with a transcript, in a live
    /// repo, that no running attempt continues yet (see
    /// [`Self::list_resumable_jobs`]).
    pub async fn checkpoint_job(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
    ) -> sqlx::Result<Option<Job>> {
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM finding_work w
            JOIN jobs j ON j.id = w.checkpoint_job_id
            JOIN repos r ON r.id = j.repo_id
            WHERE w.finding_id = ?1 AND w.kind = ?2
              AND r.deleted_at IS NULL
              AND j.session_file IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id AND s.state = ?3)
            "#,
            finding_id,
            kind,
            running
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Make the finished attempt `job_id` its work item's checkpoint, when
    /// it has a transcript to continue: an attempt whose work is done but
    /// could not be published while the network was down (#95), which the
    /// next cycle continues to publish instead of redoing the work. Without
    /// a transcript the pointer is left as it is.
    pub async fn keep_attempt_as_checkpoint(&self, job_id: i64) -> sqlx::Result<()> {
        let resumable = sqlx::query_scalar!(
            r#"SELECT session_file IS NOT NULL AS "resumable!: bool" FROM jobs WHERE id = ?1"#,
            job_id
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(false);
        if resumable {
            let mut tx = ImmediateTx::begin(&self.pool).await?;
            point_checkpoint(&mut tx, job_id, true).await?;
            tx.commit().await?;
        }
        Ok(())
    }

    /// The findings whose `kind` work has a checkpoint.
    pub async fn findings_with_checkpoint(
        &self,
        kind: FindingJobKind,
    ) -> sqlx::Result<BTreeSet<i64>> {
        Ok(sqlx::query_scalar!(
            r#"SELECT finding_id AS "finding_id!: i64" FROM finding_work
               WHERE kind = ?1 AND checkpoint_job_id IS NOT NULL"#,
            kind
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect())
    }

    /// Give up `finding_id`'s `kind` checkpoint at `job_id`, logging `msg`
    /// as a `resume` event in the same transaction. The job row is left as
    /// it ended. A pointer that has moved on since is left alone: `false`.
    /// The job lane's alone, through [`JobStore`].
    async fn drop_checkpoint(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        job_id: i64,
        msg: &str,
    ) -> sqlx::Result<bool> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let dropped = sqlx::query!(
            "UPDATE finding_work SET checkpoint_job_id = NULL \
             WHERE finding_id = ?1 AND kind = ?2 AND checkpoint_job_id = ?3",
            finding_id,
            kind,
            job_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if dropped {
            insert_event(
                &mut *tx,
                "resume",
                msg,
                Some(job_id),
                Some(finding_id),
                None,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(dropped)
    }

    /// The job `repo_id`'s `kind` work continues from, if it has a
    /// checkpoint that can be continued (see [`Self::list_resumable_jobs`]).
    pub async fn repo_checkpoint_job(
        &self,
        repo_id: i64,
        kind: RepoJobKind,
    ) -> sqlx::Result<Option<Job>> {
        let kind = JobKind::Repo(kind);
        let running = JobState::Running;
        sqlx::query_as!(
            Job,
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!",
                   j.finding_id, j.state AS "state!: JobState", j.pid, j.session_file,
                   j.cap_tokens, j.tokens_new, j.calls, j.exit_code,
                   j.killed_reason, j.notes, j.model, j.usage_delta,
                   j.started_at, j.finished_at, r.name AS "repo_name!",
                   NULL AS "finding_summary?: String",
                   NULL AS "finding_fingerprint?: String"
            FROM repo_work w
            JOIN jobs j ON j.id = w.checkpoint_job_id
            JOIN repos r ON r.id = j.repo_id
            WHERE w.repo_id = ?1 AND w.kind = ?2
              AND r.deleted_at IS NULL
              AND j.session_file IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id AND s.state = ?3)
            "#,
            repo_id,
            kind,
            running
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Give up `repo_id`'s `kind` checkpoint at `job_id`, logging `msg` as a
    /// `resume` event in the same transaction, as [`Self::drop_checkpoint`]
    /// does for a finding's work. The job row is left as it ended. The job
    /// lane's alone, through [`JobStore`].
    async fn drop_repo_checkpoint(
        &self,
        repo_id: i64,
        kind: RepoJobKind,
        job_id: i64,
        msg: &str,
    ) -> sqlx::Result<bool> {
        let kind = JobKind::Repo(kind);
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let dropped = sqlx::query!(
            "UPDATE repo_work SET checkpoint_job_id = NULL \
             WHERE repo_id = ?1 AND kind = ?2 AND checkpoint_job_id = ?3",
            repo_id,
            kind,
            job_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if dropped {
            insert_event(&mut *tx, "resume", msg, Some(job_id), None, None).await?;
        }
        tx.commit().await?;
        Ok(dropped)
    }

    /// What the chain containing `job_id` has cost, how many attempts
    /// it took, and what its most expensive attempt cost.
    ///
    /// A resumed attempt is its own row, so `tokens_new` alone answers
    /// "what did this attempt cost" and never "what has this work
    /// cost". The second question is the one a give-up ceiling has to
    /// ask, so it is answered by walking the link.
    ///
    /// `attempts` and `max_single` ride along because the ceiling
    /// cannot be applied without them: a chain of one has not been
    /// continued even once, and one enormous attempt is evidence about
    /// the job's size rather than about the chain being stuck.
    ///
    /// `attempts` counts the initial attempt always, a cap-killed one
    /// always, and any other resumed one only when it has positive
    /// metered spend: a handoff that spent nothing or recorded no spend
    /// (a provider failure before the first response) was not a try at
    /// the work. Cap kills count regardless because they re-suspend
    /// automatically, so the count must bound them on its own.
    ///
    /// Walks BOTH ways — to predecessors via `resumed_from` and to
    /// successors via the rows that name this one — because `job_id`
    /// can be any link, not just the newest.
    ///
    /// Only links between jobs of `job_id`'s own kind count. A
    /// withdrawal's handoff creates a harvest `resumed_from` the engage,
    /// so the link crosses kinds there; the engage's spend and attempt
    /// are its own work, and counting them would bring the harvest to
    /// its give-up ceiling for work it never did. The workspace a chain
    /// shares still crosses kinds: that is [`Self::resume_origin_job`].
    ///
    /// The depth cap is the termination guarantee. `UNION` de-duplicates
    /// against rows already produced, but cannot stop a cycle once
    /// `depth` is carried: each revisit arrives with a larger depth and
    /// is therefore a genuinely new row, so the recursion would run
    /// forever. The write path cannot produce a cycle — `resumed_from`
    /// is set once at INSERT to an id that already exists, so ids
    /// strictly decrease along the link — but this read must not hang on
    /// a database that did not come from that write path (a hand repair,
    /// a partial restore). [`RESUME_CHAIN_MAX_DEPTH`] sits far above any
    /// chain length worth resuming, so the cap costs a healthy chain
    /// nothing.
    ///
    /// Carrying `depth` is what forces the aggregate to go through `id
    /// IN (SELECT ...)` rather than a join: a node reachable by several
    /// paths appears once per depth, and joining on it would count that
    /// job's tokens once per appearance.
    ///
    /// `since` limits the aggregate to attempts started after that instant:
    /// the give-up ceiling counts only what the scheduler did after the
    /// operator last released the work (WORK-MODEL.md §8).
    pub async fn resume_chain_stats(
        &self,
        job_id: i64,
        since: Option<i64>,
    ) -> sqlx::Result<ResumeChainStats> {
        sqlx::query_as!(
            ResumeChainStats,
            r#"
            WITH RECURSIVE chain(id, resumed_from, kind, depth) AS (
                SELECT id, resumed_from, kind, 0 FROM jobs WHERE id = ?1
                UNION
                SELECT j.id, j.resumed_from, j.kind, c.depth + 1
                FROM jobs j, chain c
                WHERE c.depth < ?2
                  AND j.kind = c.kind
                  AND (j.id = c.resumed_from OR j.resumed_from = c.id)
            )
            SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64",
                   COALESCE(SUM(resumed_from IS NULL OR killed_reason = 'cap' OR COALESCE(tokens_new, 0) > 0), 0) AS "attempts!: i64",
                   COALESCE(MAX(tokens_new), 0) AS "max_single!: i64"
            FROM jobs WHERE id IN (SELECT id FROM chain)
              AND (?3 IS NULL OR started_at > ?3)
            "#,
            job_id,
            RESUME_CHAIN_MAX_DEPTH,
            since
        )
        .fetch_one(&self.pool)
        .await
    }

    /// The first attempt in `job_id`'s chain — the one that ran from a
    /// real playbook prompt.
    ///
    /// A resumed worker is continuing a transcript, and that transcript
    /// names an output file: the hunt playbook bakes
    /// `<work_root>/out/job<N>.findings.json` into the prompt, the
    /// analysis playbooks bake `job<N>.<plural>.json`. `N` is the id of
    /// the job whose prompt was written, so every later attempt in the
    /// chain writes to the FIRST attempt's path. An executor that
    /// ingested its own id's path would find nothing, advance no
    /// watermark, and re-select the same work next cycle — the loop
    /// resume exists to end, rebuilt one level up.
    ///
    /// One hop back is not enough: a resume of a resume still writes the
    /// original's path. So this walks `resumed_from` to the top.
    ///
    /// Unlike [`Self::resume_chain_stats`] this crosses kinds on purpose:
    /// a harvest handed off from a withdrawing engage continues the
    /// engage's transcript in the engage's tree, so its workspace is the
    /// engage chain's.
    ///
    /// `MIN(id)` is exact rather than a heuristic: `resumed_from` is
    /// written once at INSERT naming a row that already exists, so ids
    /// strictly decrease along the link and the ancestor walk's smallest
    /// id is its root. Depth-capped for the same reason
    /// [`Self::resume_chain_stats`] is — a hand-repaired database can
    /// carry a cycle the write path cannot produce. Falls back to
    /// `job_id` when the row is gone (retention prunes oldest-first, and
    /// `ON DELETE SET NULL` means a pruned root leaves the successor
    /// looking like a root, which is the honest answer).
    pub async fn resume_origin_job(&self, job_id: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"
            WITH RECURSIVE ancestry(id, resumed_from, depth) AS (
                SELECT id, resumed_from, 0 FROM jobs WHERE id = ?1
                UNION
                SELECT j.id, j.resumed_from, a.depth + 1
                FROM jobs j, ancestry a
                WHERE a.depth < ?2
                  AND j.id = a.resumed_from
            )
            SELECT COALESCE(MIN(id), ?1) AS "origin!: i64" FROM ancestry
            "#,
            job_id,
            RESUME_CHAIN_MAX_DEPTH
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Tokens recorded by every strict ancestor of `job_id`, of any kind.
    ///
    /// That is everything already metered out of the transcript directory
    /// `job_id` shares with its chain: the walk is the one
    /// [`Self::resume_origin_job`] makes to find that directory, so a
    /// harvest handed off from an engage counts the engage's spend here,
    /// where [`Self::resume_chain_stats`] (same kind only, for the give-up
    /// ceiling) does not.
    pub async fn ancestor_tokens(&self, job_id: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"
            WITH RECURSIVE ancestry(id, resumed_from, depth) AS (
                SELECT id, resumed_from, 0 FROM jobs WHERE id = ?1
                UNION
                SELECT j.id, j.resumed_from, a.depth + 1
                FROM jobs j, ancestry a
                WHERE a.depth < ?2
                  AND j.id = a.resumed_from
            )
            SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64"
            FROM jobs WHERE id IN (SELECT id FROM ancestry WHERE id <> ?1)
            "#,
            job_id,
            RESUME_CHAIN_MAX_DEPTH
        )
        .fetch_one(&self.pool)
        .await
    }

    // -- events / scheduler ----------------------------------------------------

    /// ORDER BY id DESC LIMIT ?.
    pub async fn recent_events(&self, limit: i64) -> sqlx::Result<Vec<Event>> {
        sqlx::query_as!(
            Event,
            r#"
            SELECT e.id, e.at, e.kind, e.message, e.job_id, e.finding_id,
                   u.username AS "username?"
            FROM events e
            LEFT JOIN users u ON u.id = e.user_id
            ORDER BY e.id DESC
            LIMIT ?1
            "#,
            limit
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Newest event with kind = 'cycle' (accepted deviation from Python's
    /// "within the last 500 events" window — strictly more correct).
    pub async fn last_cycle_event(&self) -> sqlx::Result<Option<Event>> {
        sqlx::query_as!(
            Event,
            r#"
            SELECT id, at, kind, message, job_id, finding_id,
                   NULL AS "username?: String"
            FROM events
            WHERE kind = 'cycle'
            ORDER BY id DESC
            LIMIT 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// SELECT ... FROM `scheduler_state` WHERE id = 1.
    pub async fn scheduler_state(&self) -> sqlx::Result<Option<SchedulerState>> {
        sqlx::query_as!(
            SchedulerState,
            r#"
            SELECT id, state, detail, next_wake_at, updated_at
            FROM scheduler_state
            WHERE id = 1
            "#
        )
        .fetch_optional(&self.pool)
        .await
    }

    // -- scheduler store methods ----------------------------------------------

    /// INSERT INTO jobs as `running`, returns the new id. Only a
    /// [`FinishedState`] moves the row on ([`Self::complete_job`]), so a
    /// job is never `running` without the owner that created it.
    ///
    /// Refuses when the repo has been soft-deleted. The `WHERE EXISTS` is
    /// part of the insert rather than a preceding SELECT because the
    /// scheduler reads its repo row long before it gets here: a delete
    /// landing in between would pass any check-then-insert, and the
    /// foreign key cannot catch it either — a flagged row is still
    /// physically present. The job that slips through is worse than a lost
    /// cycle: `forget_deleted_repo` is a plain DELETE, so a single
    /// referencing job wedges the repo permanently half-deleted —
    /// invisible to every read path, unreapable, still accruing work.
    ///
    /// `cap_tokens` is the backend's headroom for this job, and `None`
    /// means it has no token bound at all — `maxWallS` is then the only
    /// limit the worker runs under.
    ///
    /// `estimated_tokens` is what the budget ramp reserved before it
    /// granted the job, not the job's cap. The inflight reservation is
    /// read back from that column, so a job that omits it is invisible
    /// to the budget for as long as it runs.
    ///
    /// `start` says what the job continues, if anything ([`JobStart`]).
    /// A resume or handoff records its predecessor as `resumed_from` and
    /// runs on the predecessor's place, both read from that row here; a
    /// predecessor that can no longer be continued is refused as
    /// [`JobInsertError::NotContinuable`]. `resumed_from` is written here
    /// and never updated, so a row can only ever point at an id that
    /// already existed — which is what keeps the chain acyclic.
    ///
    /// Fresh work supersedes a checkpoint of the same work, in the same
    /// transaction: the work item's checkpoint pointer is cleared, for a
    /// finding kind (`finding_work`) and a repo kind (`repo_work`) alike,
    /// and the job it named is left as it ended. Whatever path started the
    /// work over, the fresh attempt now owns it, so the old checkpoint can
    /// never be resumed. Doing it here rather than in selection is what
    /// covers every path that creates a job.
    ///
    /// The retired ids are returned because their chains' working trees
    /// must be released before the fresh job builds its own: a fix chain
    /// holds its branch checked out, and git refuses to check one branch
    /// out in two worktrees.
    ///
    /// A resumed attempt copies `pinned_sha`, `base_sha` and
    /// `seen_activity_at` from the attempt it continues, here in the
    /// INSERT, so every link of a chain names the commit its shared tree
    /// was created at, the default-branch commit a harvest chain judges
    /// against, and the newest PR feedback an engage chain's worker was
    /// shown.
    ///
    /// At most one job is `running` per finding, and per (repo, kind) for
    /// repo-level work (migration 031's indexes). An insert that would be
    /// a second is refused as [`JobInsertError::Occupied`], naming the job
    /// that holds the place.
    ///
    /// A harvest, however it starts, goes in only while its finding's PR
    /// still awaits one ([`JobInsertError::NoHarvestDue`]), and an engage
    /// only while its finding is `pr_open` ([`JobInsertError::NoEngageDue`]).
    ///
    /// Takes an [`Owner`], which only `crate::recovery` makes, inside
    /// [`CycleScope::create_job`](crate::recovery::CycleScope::create_job): every job is recorded in the scope that
    /// settles it.
    pub(crate) async fn insert_job(
        &self,
        _owner: &Owner,
        start: JobStart,
        cap_tokens: Option<i64>,
        estimated_tokens: Option<i64>,
    ) -> Result<InsertedJob, JobInsertError> {
        // IMMEDIATE so the supersession below reads the suspensions under
        // the same write lock that inserts their replacement, and the
        // predecessor is read under the lock that inserts its successor.
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (kind, repo_id, finding_id, resumed_from) = Self::start_place(&mut tx, start).await?;
        match (kind, finding_id) {
            (JobKind::Finding(FindingJobKind::Harvest), Some(fid)) => {
                Self::ensure_harvest_due(&mut tx, fid).await?;
            }
            (JobKind::Finding(FindingJobKind::Engage), Some(fid)) => {
                Self::ensure_engage_due(&mut tx, fid).await?;
            }
            _ => {}
        }
        let id = Self::insert_running_row(
            &mut tx,
            (kind, repo_id, finding_id),
            (cap_tokens, estimated_tokens),
            resumed_from,
        )
        .await?;
        let mut retired = Vec::new();

        if resumed_from.is_none()
            && let (Some(fid), Some(work)) = (finding_id, kind.as_finding())
        {
            // A finding's work continues from its checkpoint pointer, so
            // fresh work drops the pointer; the row it named stays as it
            // ended.
            let old = sqlx::query!(
                r#"
                SELECT w.checkpoint_job_id AS "id!: i64", r.name AS "repo_name!"
                FROM finding_work w
                JOIN jobs j ON j.id = w.checkpoint_job_id
                JOIN repos r ON r.id = j.repo_id
                WHERE w.finding_id = ?1 AND w.kind = ?2
                "#,
                fid,
                work
            )
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(old) = old {
                let msg = format!(
                    "resume {kind} {}: checkpoint job {} superseded by fresh job {id}",
                    old.repo_name, old.id
                );
                sqlx::query!(
                    "UPDATE finding_work SET checkpoint_job_id = NULL \
                     WHERE finding_id = ?1 AND kind = ?2",
                    fid,
                    work
                )
                .execute(&mut *tx)
                .await?;
                insert_event(&mut *tx, "resume", &msg, Some(old.id), Some(fid), None).await?;
                retired.push(old.id);
            }
        } else if resumed_from.is_none() && finding_id.is_none() && kind.is_repo() {
            let old = sqlx::query!(
                r#"
                SELECT w.checkpoint_job_id AS "id!: i64", r.name AS "repo_name!"
                FROM repo_work w
                JOIN jobs j ON j.id = w.checkpoint_job_id
                JOIN repos r ON r.id = j.repo_id
                WHERE w.repo_id = ?1 AND w.kind = ?2
                "#,
                repo_id,
                kind
            )
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(old) = old {
                let msg = format!(
                    "resume {kind} {}: checkpoint job {} superseded by fresh job {id}",
                    old.repo_name, old.id
                );
                sqlx::query!(
                    "UPDATE repo_work SET checkpoint_job_id = NULL \
                     WHERE repo_id = ?1 AND kind = ?2",
                    repo_id,
                    kind
                )
                .execute(&mut *tx)
                .await?;
                insert_event(&mut *tx, "resume", &msg, Some(old.id), None, None).await?;
                retired.push(old.id);
            }
        }

        tx.commit().await?;
        Ok(InsertedJob {
            id,
            superseded: retired,
        })
    }

    /// Where `start` runs: (kind, repo, finding, `resumed_from`). Fresh
    /// repo work names its repo; fresh finding work runs in its finding's
    /// repo, read here. A resume runs where its checkpoint ran, and only
    /// while that job is still its work item's checkpoint; a handoff is
    /// its engage's finding's harvest, and only while that engage has no
    /// successor. Read in the insert's transaction, so neither the
    /// finding nor the predecessor can move between this read and the
    /// INSERT.
    async fn start_place(
        tx: &mut ImmediateTx,
        start: JobStart,
    ) -> Result<(JobKind, i64, Option<i64>, Option<i64>), JobInsertError> {
        match start {
            JobStart::Fresh(FreshWork::Repo { kind, repo_id }) => {
                Ok((kind.into(), repo_id, None, None))
            }
            JobStart::Fresh(FreshWork::Finding { kind, finding_id }) => {
                let repo_id = sqlx::query_scalar!(
                    r#"SELECT repo_id AS "repo_id!: i64" FROM findings WHERE id = ?1"#,
                    finding_id
                )
                .fetch_optional(&mut **tx)
                .await?
                .ok_or(JobInsertError::UnknownFinding { finding_id, kind })?;
                Ok((kind.into(), repo_id, Some(finding_id), None))
            }
            JobStart::Resume { checkpoint } => {
                let row = sqlx::query!(
                    r#"
                    SELECT j.kind AS "kind!: JobKind", j.repo_id AS "repo_id!: i64", j.finding_id
                    FROM jobs j
                    WHERE j.id = ?1
                      AND (EXISTS (SELECT 1 FROM finding_work w
                                   WHERE w.checkpoint_job_id = j.id
                                     AND w.finding_id = j.finding_id AND w.kind = j.kind)
                        OR (j.finding_id IS NULL
                            AND EXISTS (SELECT 1 FROM repo_work w
                                        WHERE w.checkpoint_job_id = j.id
                                          AND w.repo_id = j.repo_id AND w.kind = j.kind)))
                    "#,
                    checkpoint
                )
                .fetch_optional(&mut **tx)
                .await?;
                let row = row.ok_or(JobInsertError::NotContinuable { start })?;
                Ok((row.kind, row.repo_id, row.finding_id, Some(checkpoint)))
            }
            JobStart::Handoff { engage } => {
                let engage_kind: JobKind = FindingJobKind::Engage.into();
                let row = sqlx::query!(
                    r#"
                    SELECT j.repo_id AS "repo_id!: i64", j.finding_id AS "finding_id!: i64"
                    FROM jobs j
                    WHERE j.id = ?1 AND j.kind = ?2 AND j.finding_id IS NOT NULL
                      AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
                    "#,
                    engage,
                    engage_kind
                )
                .fetch_optional(&mut **tx)
                .await?;
                let row = row.ok_or(JobInsertError::NotContinuable { start })?;
                Ok((
                    FindingJobKind::Harvest.into(),
                    row.repo_id,
                    Some(row.finding_id),
                    Some(engage),
                ))
            }
        }
    }

    /// Refuse a harvest of `finding_id` unless its PR still awaits one, by
    /// [`Self::list_pending_harvest`]'s rule, read under the insert's write
    /// lock. A harvest is picked, handed over or resumed some forge calls
    /// before its row goes in, so without this a second attempt -- another
    /// picker's, or a cold harvest that finished before an engage's handoff
    /// arrived -- would harvest the PR again after the first had stamped it.
    async fn ensure_harvest_due(
        tx: &mut ImmediateTx,
        finding_id: i64,
    ) -> Result<(), JobInsertError> {
        let merged = FindingStatus::Merged;
        let closed = FindingStatus::Closed;
        let due = sqlx::query_scalar!(
            r#"
            SELECT 1 AS "due!: i64"
            FROM findings f
            JOIN pr_state p ON p.finding_id = f.id
            WHERE f.id = ?1
              AND p.harvested_at IS NULL
              AND (f.status = ?2 OR (p.state = 'CLOSED' AND f.status = ?3))
            "#,
            finding_id,
            merged,
            closed
        )
        .fetch_optional(&mut **tx)
        .await?;
        due.map(|_| ())
            .ok_or(JobInsertError::NoHarvestDue { finding_id })
    }

    /// Refuse an engage of `finding_id` unless the finding is `pr_open`,
    /// read under the insert's write lock. PR sync runs on the admin lane,
    /// beside the job lane, and an engage is picked some forge calls and a
    /// fetch before its row goes in: without this, a sync that recorded the
    /// PR's merge or close in between would leave a worker answering a PR
    /// that has ended. A resume is already refused once the finding leaves
    /// `pr_open` (its checkpoint is cleared), so this matters for fresh
    /// engages; it is checked for every start all the same.
    async fn ensure_engage_due(
        tx: &mut ImmediateTx,
        finding_id: i64,
    ) -> Result<(), JobInsertError> {
        let pr_open = FindingStatus::PrOpen;
        let due = sqlx::query_scalar!(
            r#"SELECT 1 AS "due!: i64" FROM findings WHERE id = ?1 AND status = ?2"#,
            finding_id,
            pr_open
        )
        .fetch_optional(&mut **tx)
        .await?;
        due.map(|_| ())
            .ok_or(JobInsertError::NoEngageDue { finding_id })
    }

    /// [`Self::insert_job`]'s INSERT: the `running` row, its chain's pins
    /// copied from `resumed_from`. Refused for a deleted repo, and for a
    /// place another `running` job holds.
    async fn insert_running_row(
        tx: &mut ImmediateTx,
        (kind, repo_id, finding_id): (JobKind, i64, Option<i64>),
        (cap_tokens, estimated_tokens): (Option<i64>, Option<i64>),
        resumed_from: Option<i64>,
    ) -> Result<i64, JobInsertError> {
        let now = now_ms();
        let result = sqlx::query!(
            "INSERT INTO jobs \
             (kind, repo_id, finding_id, cap_tokens, state, started_at, estimated_tokens, \
              resumed_from, pinned_sha, base_sha, seen_activity_at, full_history, \
              full_hunt_request) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, \
                    (SELECT p.pinned_sha FROM jobs p WHERE p.id = ?8), \
                    (SELECT p.base_sha FROM jobs p WHERE p.id = ?8), \
                    (SELECT p.seen_activity_at FROM jobs p WHERE p.id = ?8), \
                    COALESCE((SELECT p.full_history FROM jobs p WHERE p.id = ?8), 0), \
                    (SELECT p.full_hunt_request FROM jobs p WHERE p.id = ?8) \
             WHERE EXISTS (SELECT 1 FROM repos WHERE id = ?2 AND deleted_at IS NULL)",
            kind,
            repo_id,
            finding_id,
            cap_tokens,
            JobState::Running,
            now,
            estimated_tokens,
            resumed_from
        )
        .execute(&mut **tx)
        .await;
        match result {
            Ok(r) if r.rows_affected() == 0 => Err(JobInsertError::RepoDeleted { repo_id, kind }),
            Ok(r) => Ok(r.last_insert_rowid()),
            Err(e) => Err(Self::insert_refused(tx, e, (kind, repo_id, finding_id)).await),
        }
    }

    /// What a failed job INSERT means: a unique-index violation is the
    /// place being held (migration 031), named by the `running` job read in
    /// the same transaction, so it is the row the index saw.
    async fn insert_refused(
        tx: &mut ImmediateTx,
        err: sqlx::Error,
        (kind, repo_id, finding_id): (JobKind, i64, Option<i64>),
    ) -> JobInsertError {
        let sqlx::Error::Database(db) = &err else {
            return err.into();
        };
        if !db.is_unique_violation() {
            return err.into();
        }
        let place = JobPlace::of(kind, repo_id, finding_id);
        match Self::running_holder(tx, place).await {
            Ok(holder) => JobInsertError::Occupied {
                place,
                holder,
                kind,
            },
            Err(e) => e.into(),
        }
    }

    /// The `running` job holding `place`.
    async fn running_holder(tx: &mut ImmediateTx, place: JobPlace) -> sqlx::Result<i64> {
        let running = JobState::Running;
        match place {
            JobPlace::Finding(fid) => {
                sqlx::query_scalar!(
                    r#"SELECT id AS "id!: i64" FROM jobs WHERE state = ?1 AND finding_id = ?2"#,
                    running,
                    fid
                )
                .fetch_one(&mut **tx)
                .await
            }
            JobPlace::RepoWork { repo_id, kind } => {
                sqlx::query_scalar!(
                    r#"SELECT id AS "id!: i64" FROM jobs
                       WHERE state = ?1 AND finding_id IS NULL AND repo_id = ?2 AND kind = ?3"#,
                    running,
                    repo_id,
                    kind
                )
                .fetch_one(&mut **tx)
                .await
            }
        }
    }

    /// Record a job that never ran: `failed`, finished now, with `notes`
    /// saying why. `tokens_new` stays NULL rather than 0 — nothing was
    /// spawned, so there is no spend to know, and a 0 would be read back by
    /// the per-kind estimate as a job that cost nothing. Takes an
    /// [`Owner`], which only `crate::recovery` makes, inside
    /// [`RunningJob::fail_unstarted`](crate::recovery::RunningJob::fail_unstarted):
    /// only the job's owner ends it.
    pub(crate) async fn fail_unstarted_job(
        &self,
        _owner: &Owner,
        job_id: i64,
        notes: &str,
    ) -> sqlx::Result<()> {
        let failed = JobState::Failed;
        let now = now_ms();
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, notes = ?2, finished_at = ?3 WHERE id = ?4",
            failed,
            notes,
            now,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record the commit a cold job's working tree was created at. See
    /// migration 015; resumed attempts get theirs from [`CycleScope::create_job`](crate::recovery::CycleScope::create_job).
    pub async fn set_pinned_sha(&self, job_id: i64, sha: &str) -> sqlx::Result<()> {
        sqlx::query!("UPDATE jobs SET pinned_sha = ?1 WHERE id = ?2", sha, job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The commit `job_id`'s chain tree was created at, if one was recorded.
    pub async fn pinned_sha(&self, job_id: i64) -> sqlx::Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT pinned_sha FROM jobs WHERE id = ?1", job_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    /// Record the default-branch commit a harvest judges against
    /// (migration 033), resolved right after its own fetch. A resumed
    /// attempt inherits it ([`CycleScope::create_job`](crate::recovery::CycleScope::create_job)).
    pub async fn set_base_sha(&self, job_id: i64, sha: &str) -> sqlx::Result<()> {
        sqlx::query!("UPDATE jobs SET base_sha = ?1 WHERE id = ?2", sha, job_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The default-branch commit `job_id`'s harvest chain judges against,
    /// if one was recorded ([`Self::set_base_sha`]).
    pub async fn base_sha(&self, job_id: i64) -> sqlx::Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT base_sha FROM jobs WHERE id = ?1", job_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    /// Record the newest PR feedback a cold engage attempt's worker was
    /// shown. Resumes inherit it ([`CycleScope::create_job`](crate::recovery::CycleScope::create_job)) instead.
    pub async fn set_seen_activity(&self, job_id: i64, seen: SeenActivity) -> sqlx::Result<()> {
        let at_ms = seen.0;
        sqlx::query!(
            "UPDATE jobs SET seen_activity_at = ?1 WHERE id = ?2",
            at_ms,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The newest PR feedback `job_id`'s engage chain was shown, if its
    /// first attempt recorded it ([`Self::set_seen_activity`]).
    pub async fn seen_activity(&self, job_id: i64) -> sqlx::Result<Option<SeenActivity>> {
        Ok(
            sqlx::query_scalar!("SELECT seen_activity_at FROM jobs WHERE id = ?1", job_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten()
                .map(SeenActivity),
        )
    }
    /// Whether the chain rooted at `origin_id` still needs its working
    /// tree, and how long ago it last did anything.
    ///
    /// Walks successors from the origin, because a workspace is keyed by
    /// its chain's FIRST job and every later attempt links back to it.
    /// Depth-capped for the same reason [`Self::resume_chain_stats`] is.
    ///
    /// An attempt is live while it is `running`, or while it is a
    /// checkpoint: a work item's `checkpoint_job_id`, a finding's or a
    /// repo's. A resume inserts its successor and never updates the attempt
    /// it continues, so every link but the newest of a resumed chain stays
    /// `suspended` for good; counting those would keep the tree of every
    /// chain that was ever resumed forever. A `suspended` row that no work
    /// item points at is history: its work was given up, superseded, or
    /// left its tier.
    pub async fn chain_status(&self, origin_id: i64) -> sqlx::Result<ChainStatus> {
        let running = JobState::Running;
        sqlx::query_as!(
            ChainRow,
            r#"
            WITH RECURSIVE chain(id, depth) AS (
                SELECT id, 0 FROM jobs WHERE id = ?1
                UNION
                SELECT j.id, c.depth + 1
                FROM jobs j, chain c
                WHERE c.depth < ?2 AND j.resumed_from = c.id
            )
            SELECT COUNT(*) AS "attempts!: i64",
                   COALESCE(SUM(state = ?3
                       OR EXISTS (SELECT 1 FROM finding_work w
                                  WHERE w.checkpoint_job_id = jobs.id)
                       OR EXISTS (SELECT 1 FROM repo_work w
                                  WHERE w.checkpoint_job_id = jobs.id)), 0)
                       AS "live!: i64",
                   MAX(finished_at) AS "last_finished_at?: i64",
                   (SELECT r.id FROM jobs o JOIN repos r ON r.id = o.repo_id
                    WHERE o.id = ?1) AS "repo_id?: i64",
                   (SELECT r.path FROM jobs o JOIN repos r ON r.id = o.repo_id
                    WHERE o.id = ?1) AS "clone?: String"
            FROM jobs WHERE id IN (SELECT id FROM chain)
            "#,
            origin_id,
            RESUME_CHAIN_MAX_DEPTH,
            running
        )
        .fetch_one(&self.pool)
        .await
        .map(ChainStatus::from)
    }

    /// Every running job and every checkpoint (see [`Self::chain_status`]),
    /// with what it could still be using on disk. Read by the workspace
    /// sweep so that no directory a live job refers to is ever removed.
    pub async fn live_jobs(&self) -> sqlx::Result<Vec<LiveJob>> {
        let running = JobState::Running;
        sqlx::query_as!(
            LiveJob,
            r#"
            SELECT id AS "id!", kind AS "kind!: JobKind", finding_id, session_file
            FROM jobs
            WHERE state = ?1
               OR EXISTS (SELECT 1 FROM finding_work w WHERE w.checkpoint_job_id = jobs.id)
               OR EXISTS (SELECT 1 FROM repo_work w WHERE w.checkpoint_job_id = jobs.id)
            "#,
            running
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Jobs whose `session_file` lies under `dir` (a prefix compare, not
    /// LIKE: `_` and `%` in a path would be wildcards), with the state and
    /// finish time the legacy session sweep ages them by.
    pub async fn jobs_with_session_under(&self, dir: &str) -> sqlx::Result<Vec<SessionRef>> {
        sqlx::query_as!(
            SessionRef,
            r#"
            SELECT session_file AS "session_file!", state AS "state!: JobState", finished_at
            FROM jobs
            WHERE session_file IS NOT NULL
              AND substr(session_file, 1, length(?1)) = ?1
            "#,
            dir
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Record a completed job (all outcome fields set at once).
    ///
    /// This also moves its work item's checkpoint, in
    /// the same transaction: to this attempt when it ended paused, and off
    /// whatever it was when it did not ([`point_checkpoint`]). Takes an
    /// [`Owner`], which only `crate::recovery` makes, inside
    /// [`RunningJob::complete`](crate::recovery::RunningJob::complete): only
    /// the job's owner records how it ended.
    pub(crate) async fn complete_job(
        &self,
        _owner: &Owner,
        job_id: i64,
        outcome: &JobOutcome<'_>,
    ) -> sqlx::Result<()> {
        let JobOutcome {
            state,
            tokens_new,
            calls,
            exit_code,
            killed_reason,
            session_file,
            notes,
            model,
            usage_delta,
            finished_at,
        } = *outcome;
        let suspended = state == FinishedState::Suspended;
        let state = JobState::from(state);
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, tokens_new = ?2, calls = ?3, \
             exit_code = ?4, killed_reason = ?5, session_file = ?6, notes = ?7, \
             model = ?8, usage_delta = ?9, finished_at = ?10 \
             WHERE id = ?11",
            state,
            tokens_new,
            calls,
            exit_code,
            killed_reason,
            session_file,
            notes,
            model,
            usage_delta,
            finished_at,
            job_id
        )
        .execute(&mut *tx)
        .await?;
        point_checkpoint(&mut tx, job_id, suspended).await?;
        tx.commit().await
    }

    /// Record an orphaned running job as `suspended`: the prior process
    /// died while its worker had done metered work, and the transcript is
    /// on disk for the resume tier to continue. `killed_reason` stays
    /// `orphaned` so the row still says how the attempt ended. Its work
    /// item now continues from it. Only for a job nothing will finish
    /// ([`StrandedJob`], which only `crate::recovery` makes), and only
    /// while it is still `running`: `false`, and nothing written, when it
    /// no longer is.
    pub async fn suspend_orphan(
        &self,
        job: &StrandedJob,
        session_file: &str,
        tokens_new: i64,
        notes: &str,
        finished_at: i64,
    ) -> sqlx::Result<bool> {
        let job_id = job.id();
        let (suspended, running) = (JobState::Suspended, JobState::Running);
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let updated = sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, killed_reason = 'orphaned', \
             session_file = ?2, tokens_new = ?3, notes = ?4, finished_at = ?5 \
             WHERE id = ?6 AND state = ?7",
            suspended,
            session_file,
            tokens_new,
            notes,
            finished_at,
            job_id,
            running
        )
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 0 {
            return Ok(false);
        }
        point_checkpoint(&mut tx, job_id, true).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Mark an orphaned running job as killed. Only for a job nothing will
    /// finish ([`StrandedJob`], which only `crate::recovery` makes), and
    /// only while it is still `running`: `false`, and nothing written,
    /// when it no longer is.
    pub async fn orphan_job(
        &self,
        job: &StrandedJob,
        notes: &str,
        finished_at: i64,
    ) -> sqlx::Result<bool> {
        let job_id = job.id();
        let (killed, running) = (JobState::Killed, JobState::Running);
        let updated = sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, killed_reason = 'orphaned', \
             notes = ?2, finished_at = ?3 WHERE id = ?4 AND state = ?5",
            killed,
            notes,
            finished_at,
            job_id,
            running
        )
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated > 0)
    }

    /// Downgrade an already-recorded job to failed (e.g. post-run push
    /// failure). Takes the [`RecordedJob`] its outcome was recorded as, so
    /// it cannot end a job that is still `running`.
    pub async fn fail_job(&self, job: &RecordedJob, notes: &str) -> sqlx::Result<()> {
        let job_id = job.id();
        let failed = JobState::Failed;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, notes = ?2 WHERE id = ?3",
            failed,
            notes,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE repos SET `last_hunt_sha` = ?, `last_hunt_at` = now WHERE id = ?.
    pub async fn set_last_hunt(&self, repo_id: i64, sha: &str) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_hunt_sha = ?1, last_hunt_at = ?2 WHERE id = ?3",
            sha,
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// UPDATE repos SET `last_full_hunt_at` = now WHERE id = ?.
    pub async fn set_last_full_hunt(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE repos SET last_full_hunt_at = ?1 WHERE id = ?2",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record an operator's request for a full re-hunt of `repo_id`, as of
    /// now. A pending request is replaced, so asking again while a chain
    /// is under way queues another full pass rather than being absorbed
    /// by the one in flight. False if the repo is gone.
    ///
    /// The value identifies the request — hunts record which one they
    /// accepted and attempted, and compare by equality — so it must never
    /// repeat: it is the time of the request, or one past the last value
    /// this repo issued or a hunt attempted if that is not already later.
    /// Two requests in the same millisecond would otherwise share a value,
    /// and a hunt that read the first would count the second as answered.
    /// One statement, so concurrent requests serialize on the row.
    pub async fn request_full_hunt(&self, repo_id: i64) -> sqlx::Result<bool> {
        let now = now_ms();
        let result = sqlx::query!(
            "UPDATE repos SET full_hunt_requested_at = MAX( \
                 ?1, \
                 COALESCE(full_hunt_requested_at, 0) + 1, \
                 COALESCE(full_hunt_request_attempted, 0) + 1) \
             WHERE id = ?2 AND deleted_at IS NULL",
            now,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Record that a hunt has started to answer `request` (a
    /// `full_hunt_requested_at` value), so that request no longer jumps
    /// the rotation.
    pub async fn mark_full_hunt_attempted(&self, repo_id: i64, request: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE repos SET full_hunt_request_attempted = ?1 WHERE id = ?2",
            request,
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Settle the pending full re-hunt request with the full-history hunt
    /// chain that `job_id` belongs to, which has just finished — but only
    /// if it is the request that chain accepted. One made since asked for
    /// a pass over a tree this chain had already pinned, and stays pending.
    pub async fn settle_full_hunt_request(&self, repo_id: i64, job_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE repos SET full_hunt_requested_at = NULL \
             WHERE id = ?1 \
               AND full_hunt_requested_at = \
                   (SELECT full_hunt_request FROM jobs WHERE id = ?2)",
            repo_id,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that hunt job `job_id` starts a chain over the repo's
    /// complete history, answering `request` if one was pending; every
    /// resume of the chain inherits both.
    pub async fn mark_full_history(&self, job_id: i64, request: Option<i64>) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE jobs SET full_history = 1, full_hunt_request = ?1 WHERE id = ?2",
            request,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether job `job_id`'s chain reviews the complete history.
    pub async fn job_full_history(&self, job_id: i64) -> sqlx::Result<bool> {
        Ok(sqlx::query_scalar!(
            r#"SELECT full_history AS "full_history: bool" FROM jobs WHERE id = ?1"#,
            job_id
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(false))
    }

    /// Targeted query for `anticipated_tokens`: is there a warm (non-denied,
    /// finished within cutoff) job for this exact (repo, kind)?
    pub async fn has_warm_job(
        &self,
        repo_id: i64,
        kind: &str,
        cutoff_ms: i64,
    ) -> sqlx::Result<bool> {
        let row = sqlx::query_scalar!(
            r#"SELECT 1 AS "x!: i64" FROM jobs
               WHERE repo_id = ?1 AND kind = ?2 AND finished_at > ?3
               AND state != 'denied' LIMIT 1"#,
            repo_id,
            kind,
            cutoff_ms
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Targeted query for `anticipated_tokens`: what this kind's most
    /// recently COMPLETED work cost, one sample per completed chain,
    /// sorted ascending.
    ///
    /// A killed job's `tokens_new` is not a measurement of what the work
    /// costs — it is whatever bound killed it, so killed rows cluster at the
    /// cap and the percentile ends up describing the estimator's own
    /// failures. Live data: 64 of 259 hunt jobs were killed, and counting
    /// them put the cold p90 at 204,173, within 2% of the 200,000 cap that
    /// did the killing, against 73,724 for the 189 that finished.
    ///
    /// The sample is the CHAIN's total, not the `done` row's own
    /// `tokens_new`. Since resume exists, a continued attempt only meters
    /// what that attempt added — the harness subtracts the transcript's
    /// pre-spawn baseline — so the row that finally reaches `done` records
    /// the tail of the work, and the attempts that paid for the rest stay
    /// `suspended` and never enter this history at all. Live: engage 4375
    /// finished having recorded 38,193 for a chain that cost 142,256, its
    /// suspended predecessor 4374 holding the other 104,063. Reading the
    /// row alone biases the estimate down exactly where the work was
    /// hardest, and this estimate is what funds the next cold start, sizes
    /// a resume's reservation and sets the give-up ceiling.
    ///
    /// Filtering to `resumed_from IS NULL` — counting only chain roots —
    /// looks like the same fix and is the opposite of it: a resumed
    /// chain's root is the attempt that was suspended, so it is never
    /// `done`, and the filter would drop every chain that had to be
    /// continued. The expensive work would leave the history entirely.
    ///
    /// Walking ancestors only is enough, and exact: resume is offered for
    /// `suspended` attempts alone, so a `done` row is the last link of its
    /// chain and no two `done` rows share one. That makes "the 20 most
    /// recent `done` rows" already "the 20 most recent completed chains".
    /// Depth-capped like [`Self::resume_chain_stats`], for the same reason,
    /// and like it follows only links within `kind`: a harvest handed off
    /// from a withdrawing engage is `resumed_from` that engage, whose cost
    /// is engage work and would otherwise inflate the harvest's estimate.
    ///
    /// `RECENT_COMPLETED_WINDOW` is the other half, and it bounds how far
    /// back the estimate can be dragged. An estimator that reads all
    /// history forever is hostage to every accounting bug the ledger has
    /// ever had, and to repos that have since changed size: 130 of the 189
    /// completed hunts here are recorded under 3,000 tokens, all of them
    /// from before the metering repair, when a worker's first call alone
    /// writes a ~37,000-token prompt cache. Those rows cannot be corrected
    /// and will never leave the table, so the p50 they produced was 1,876
    /// against 43,901 over the recent window. A window ages a bad era out
    /// on its own, which no filter written against one known defect does.
    ///
    /// Most recent by `id` DESC, then sorted ascending, because the two
    /// orderings answer different questions: the first picks WHICH chains
    /// count, the second is what makes a percentile index meaningful.
    ///
    /// Below `MIN_COMPLETED_SAMPLES` the unfiltered history comes back
    /// instead, because the alternative is worse than a biased estimate:
    /// with no completed history the estimate is 0, and an anticipated of 0
    /// makes the budget gate reserve nothing and wave through work it cannot
    /// fund. 3 is the smallest count for which a percentile index picks
    /// anything other than an endpoint.
    pub async fn kind_token_history(&self, kind: &str) -> sqlx::Result<Vec<i64>> {
        const MIN_COMPLETED_SAMPLES: usize = 3;
        const RECENT_COMPLETED_WINDOW: i64 = 20;
        let done = sqlx::query_scalar!(
            r#"
            WITH RECURSIVE recent(head) AS (
                SELECT id FROM jobs
                WHERE kind = ?1 AND state = 'done' AND tokens_new IS NOT NULL
                ORDER BY id DESC LIMIT ?2
            ),
            chain(head, id, resumed_from, depth) AS (
                SELECT r.head, j.id, j.resumed_from, 0
                  FROM recent r JOIN jobs j ON j.id = r.head
                UNION
                SELECT c.head, j.id, j.resumed_from, c.depth + 1
                  FROM jobs j, chain c
                 WHERE c.depth < ?3 AND j.id = c.resumed_from AND j.kind = ?1
            )
            SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64" FROM (
                SELECT DISTINCT c.head AS head, c.id AS id, j.tokens_new AS tokens_new
                  FROM chain c JOIN jobs j ON j.id = c.id
            )
            GROUP BY head
            ORDER BY 1 ASC
            "#,
            kind,
            RECENT_COMPLETED_WINDOW,
            RESUME_CHAIN_MAX_DEPTH
        )
        .fetch_all(&self.pool)
        .await?;
        if done.len() >= MIN_COMPLETED_SAMPLES {
            return Ok(done);
        }
        let rows = sqlx::query_scalar!(
            r#"SELECT tokens_new AS "tokens_new!: i64" FROM jobs
               WHERE kind = ?1 AND tokens_new IS NOT NULL
               ORDER BY tokens_new ASC"#,
            kind
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// UPSERT INTO `scheduler_state` (id=1, state, detail, `next_wake_at`, `updated_at`).
    pub async fn set_scheduler_state(
        &self,
        state: &str,
        detail: &str,
        next_wake_at: Option<i64>,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO scheduler_state (id, state, detail, next_wake_at, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4) \
             ON CONFLICT(id) DO UPDATE SET state=excluded.state, detail=excluded.detail, \
             next_wake_at=excluded.next_wake_at, updated_at=excluded.updated_at",
            state,
            detail,
            next_wake_at,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Findings with suppressed statuses for a repo+type (from `FindingStatus::is_suppressed`).
    pub async fn suppressions(
        &self,
        repo_id: i64,
        finding_type: &str,
    ) -> sqlx::Result<Vec<Finding>> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override AS "budget_override: BudgetOverride", ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE repo_id = ?1 AND type = ?2
              AND status IN (?3, ?4)
            ORDER BY id
            "#,
            repo_id,
            finding_type,
            s1,
            s2
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Non-suppressed findings for a repo+type (novelty comparison).
    ///
    /// Everything except the verdicts that mean forget this. A held fix,
    /// merged work, or a noted finding remains knowledge a hunt should not
    /// rediscover as novel; "known" does not mean "still in flight".
    pub async fn known_active(
        &self,
        repo_id: i64,
        finding_type: &str,
    ) -> sqlx::Result<Vec<Finding>> {
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override AS "budget_override: BudgetOverride", ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE repo_id = ?1 AND type = ?2
              AND status NOT IN (?3, ?4)
            ORDER BY id
            "#,
            repo_id,
            finding_type,
            s1,
            s2
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Insert a finding unless one with the same (type, fingerprint) already
    /// exists. Returns (id, `was_inserted`).
    ///
    /// Both lanes file findings -- the job lane its workers', the admin lane
    /// Renovate's -- so two writers can race on one fingerprint. The INSERT
    /// yields to the UNIQUE index (`ON CONFLICT DO NOTHING`) and the row
    /// that won is read back, so the loser reports a duplicate rather than
    /// failing the constraint.
    ///
    /// `found_by_job` is stored only on a genuine insert: a duplicate
    /// belongs to the job that first turned it up, not the latest one to
    /// rediscover it.
    ///
    /// Refused for a soft-deleted repo ([`FindingInsertError::RepoDeleted`]),
    /// read by the INSERT itself, as `insert_job` refuses one: the delete
    /// checks that the repo has no findings, and this keeps that true.
    pub async fn upsert_finding(
        &self,
        repo_id: i64,
        row: &FindingInsert,
        finding_type: &str,
        found_by_job: Option<i64>,
    ) -> Result<(i64, bool), FindingInsertError> {
        // Check if already exists
        let fingerprint = &row.fingerprint;
        let existing = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM findings WHERE type = ?1 AND fingerprint = ?2"#,
            finding_type,
            fingerprint
        )
        .fetch_optional(&self.pool)
        .await?;
        if let Some(id) = existing {
            return Ok((id, false));
        }
        let now = now_ms();
        let file = &row.file;
        let symbol = row.symbol.as_deref();
        let line = row.line;
        let bug_class = row.bug_class;
        let severity = row.severity;
        let confidence = row.confidence;
        let summary = &row.summary;
        let detail = row.detail.as_deref();
        let evidence_plan = row.evidence_plan.as_deref();
        let introduced_by = row.introduced_by.as_deref();
        let ecosystem = row.ecosystem.as_deref();
        let package = row.package.as_deref();
        let current_version = row.current_version.as_deref();
        let latest_version = row.latest_version.as_deref();
        let update_type = row.update_type.as_deref();
        let security_advisory = row.security_advisory.as_deref();
        let missing_tests_ref = row.missing_tests.as_deref();
        let test_file = row.test_file.as_deref();
        let smell_type = row.smell_type.as_deref();
        let suggested_refactor = row.suggested_refactor.as_deref();
        let modernization_class = row.modernization_class.as_deref();
        let current_approach = row.current_approach.as_deref();
        let proposed_approach = row.proposed_approach.as_deref();
        let standard_section = row.standard_section.as_deref();
        let new_status = FindingStatus::New;
        let result = sqlx::query!(
            "INSERT INTO findings (type, repo_id, fingerprint, file, symbol, line, bug_class, \
             severity, confidence, summary, detail, evidence_plan, introduced_by, \
             ecosystem, package, current_version, latest_version, update_type, security_advisory, \
             missing_tests, test_file, smell_type, suggested_refactor, \
             modernization_class, current_approach, proposed_approach, standard_section, \
             status, created_at, updated_at, found_by_job) \
             SELECT ?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27, ?28, ?29, ?30, ?31 \
             WHERE EXISTS (SELECT 1 FROM repos WHERE id = ?2 AND deleted_at IS NULL) \
             ON CONFLICT (type, fingerprint) DO NOTHING",
            finding_type, repo_id, fingerprint, file, symbol, line, bug_class,
            severity, confidence, summary, detail, evidence_plan, introduced_by,
            ecosystem, package, current_version, latest_version, update_type, security_advisory,
            missing_tests_ref, test_file, smell_type, suggested_refactor,
            modernization_class, current_approach, proposed_approach, standard_section,
            new_status, now, now, found_by_job
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            // Another writer filed it since the lookup above, or the repo
            // is deleted.
            let raced = sqlx::query_scalar!(
                r#"SELECT id AS "id!: i64" FROM findings WHERE type = ?1 AND fingerprint = ?2"#,
                finding_type,
                fingerprint
            )
            .fetch_optional(&self.pool)
            .await?;
            return match raced {
                Some(id) => Ok((id, false)),
                None => Err(FindingInsertError::RepoDeleted { repo_id }),
            };
        }
        Ok((result.last_insert_rowid(), true))
    }

    /// Bring an open `dep_update` up to date with the scan that just
    /// reported it again. Its fingerprint names the update unit and where
    /// the dependencies stand today, not the version they can move to, so a
    /// newer upstream release arrives as the same finding with a new target:
    /// this rewrites the target and the description instead of leaving the
    /// old one behind. Only `new` and `queued` (no worker has started on
    /// it), and a finding [`Self::supersede_unproposed_dep_updates`] retired,
    /// which comes back as `new`: the scan proposing it again means it never
    /// landed. Not a `queued` one whose fix has a checkpoint or is held: a
    /// worker is part way to the old target, and resuming it against a rewritten one
    /// would mix the two (WORK-MODEL.md G2). `true` when anything changed.
    /// The revival's edge cannot conflict, since its status is read in this
    /// transaction: a conflict is a store fault, and is refused.
    pub async fn refresh_dep_update(
        &self,
        finding_id: i64,
        row: &FindingInsert,
    ) -> Result<bool, StoreWriteError> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let open = sqlx::query!(
            r#"SELECT f.status AS "status!: FindingStatus", f.verdict_reason
               FROM findings f
               WHERE f.id = ?1 AND f.type = 'dep_update'
                 AND NOT EXISTS (SELECT 1 FROM finding_work w WHERE w.finding_id = f.id
                     AND w.kind = 'fix' AND w.checkpoint_job_id IS NOT NULL)
                 AND NOT EXISTS (SELECT 1 FROM work_holds h WHERE h.finding_id = f.id
                     AND h.kind = 'fix' AND h.released_at IS NULL)"#,
            finding_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(open) = open else {
            return Ok(false);
        };
        let revived = open.status == FindingStatus::Superseded
            && open.verdict_reason.as_deref() == Some(DEP_UNPROPOSED_REASON);
        let revive = Transition::Reproposed(ScanEdge(()));
        if revived
            && let Moved::Conflict { now } =
                Self::apply_transition(&mut tx, finding_id, &revive).await?
        {
            return Err(StoreWriteError::Refused(format!(
                "#{finding_id} moved from {} to {now:?} inside its refresh",
                revive.from()
            )));
        }
        let now = now_ms();
        let new = FindingStatus::New;
        let queued = FindingStatus::Queued;
        let file = &row.file;
        let severity = row.severity;
        let confidence = row.confidence;
        let summary = &row.summary;
        let detail = row.detail.as_deref();
        let ecosystem = row.ecosystem.as_deref();
        let package = row.package.as_deref();
        let current_version = row.current_version.as_deref();
        let latest_version = row.latest_version.as_deref();
        let update_type = row.update_type.as_deref();
        let security_advisory = row.security_advisory.as_deref();
        let rewritten = sqlx::query!(
            "UPDATE findings SET file = ?1, severity = ?2, confidence = ?3, summary = ?4, \
             detail = ?5, ecosystem = ?6, package = ?7, current_version = ?8, \
             latest_version = ?9, update_type = ?10, security_advisory = ?11, \
             updated_at = ?12 \
             WHERE id = ?13 AND status IN (?14, ?15) \
               AND (?16 OR file IS NOT ?1 OR severity IS NOT ?2 \
                    OR confidence IS NOT ?3 OR summary IS NOT ?4 OR detail IS NOT ?5 \
                    OR ecosystem IS NOT ?6 OR package IS NOT ?7 \
                    OR current_version IS NOT ?8 OR latest_version IS NOT ?9 \
                    OR update_type IS NOT ?10 OR security_advisory IS NOT ?11)",
            file,
            severity,
            confidence,
            summary,
            detail,
            ecosystem,
            package,
            current_version,
            latest_version,
            update_type,
            security_advisory,
            now,
            finding_id,
            new,
            queued,
            revived
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(revived || rewritten)
    }

    /// Retire the repo's `new` `dep_update` findings that a Renovate scan
    /// no longer proposes: the update landed some other way, or Renovate
    /// now files it under another unit (a different group, the next major).
    /// `proposed` is every fingerprint the scan produced; `unchecked` the
    /// dependencies it skipped or failed to look up, whose findings stay.
    /// A group finding is matched by its group name, not its members.
    /// Anything a worker or the operator has taken up is left alone.
    /// Returns how many were retired. Each edge's status is read in this
    /// transaction, so a conflict is a store fault: refused, and nothing is
    /// retired.
    ///
    /// Only the admin lane's Renovate scan decides what Renovate no longer
    /// proposes, so only [`AdminStore`] exposes this.
    async fn supersede_unproposed_dep_updates(
        &self,
        repo_id: i64,
        proposed: &[String],
        unchecked: &[String],
    ) -> Result<u64, StoreWriteError> {
        let new = FindingStatus::New;
        let proposed = serde_json::to_string(proposed).unwrap_or_else(|_| "[]".to_owned());
        let unchecked = serde_json::to_string(unchecked).unwrap_or_else(|_| "[]".to_owned());
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let unproposed = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM findings
               WHERE repo_id = ?1 AND type = 'dep_update' AND status = ?2
                 AND fingerprint NOT IN (SELECT value FROM json_each(?3))
                 AND package NOT IN (SELECT value FROM json_each(?4))"#,
            repo_id,
            new,
            proposed,
            unchecked
        )
        .fetch_all(&mut *tx)
        .await?;
        let retire = Transition::Unproposed {
            reason: DEP_UNPROPOSED_REASON,
            scan: ScanEdge(()),
        };
        for &id in &unproposed {
            if let Moved::Conflict { now } = Self::apply_transition(&mut tx, id, &retire).await? {
                return Err(StoreWriteError::Refused(format!(
                    "#{id} moved from {} to {now:?} inside its retirement",
                    retire.from()
                )));
            }
        }
        tx.commit().await?;
        Ok(unproposed.len() as u64)
    }

    /// Move the queue onto the scan's own findings: a `queued` `dep_update`
    /// the scan no longer proposes, but whose move it proposes in another
    /// finding, is superseded, and that finding is queued in its place
    /// (with the queued one's budget override).
    ///
    /// The same move is the same package in the same class -- major or
    /// not -- judged by that package's own update in `moves`, never by
    /// the group's type: a group making `node` major and `@types/node`
    /// minor is `major`, but a queued major `@types/node` is not its move.
    /// Or it is the same unit: a queued finding whose fingerprint starts
    /// with a proposed one's `unit` is that Renovate branch from before a
    /// member moved. This is how a queued group is matched, since its
    /// `package` names the group, not a member.
    /// This is what the fingerprints alone cannot see: the AI fallback's
    /// names a unit differently from Renovate's branch, a finding filed
    /// before branch grouping keyed on versions, and one whose installed
    /// version moved has a new fingerprint. Without it the queued one and
    /// the proposal would both be worked. A queued finding with no such
    /// match is left as it is, and so is one whose fix has a checkpoint or
    /// is held: handing it over would drop the work a paused fix has done for a
    /// fresh fix of the scan's finding (WORK-MODEL.md G2). `moves` is every
    /// move of every finding the scan produced. Returns how many were
    /// handed over. Only the admin lane's Renovate scan proposes moves, so
    /// only [`AdminStore`] exposes this.
    async fn hand_over_queued_dep_updates(
        &self,
        repo_id: i64,
        moves: &[ProposedMove],
    ) -> Result<u64, StoreWriteError> {
        let new = FindingStatus::New;
        let queued = FindingStatus::Queued;
        let moves = serde_json::to_string(moves).unwrap_or_else(|_| "[]".to_owned());
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        // JSON booleans extract as 1/0, the same as the comparison on q.
        let pairs = sqlx::query!(
            r#"SELECT q.id AS "from_id!: i64", MIN(p.id) AS "to_id!: i64"
               FROM findings q
               JOIN json_each(?4) m
                 ON (json_extract(m.value, '$.package') = q.package
                     AND json_extract(m.value, '$.major')
                         = (COALESCE(q.update_type, '') = 'major'))
                 OR substr(q.fingerprint, 1, length(json_extract(m.value, '$.unit')))
                    = json_extract(m.value, '$.unit')
               JOIN findings p
                 ON p.repo_id = q.repo_id AND p.type = 'dep_update' AND p.status = ?3
                AND p.fingerprint = json_extract(m.value, '$.fingerprint')
               WHERE q.repo_id = ?1 AND q.type = 'dep_update' AND q.status = ?2
                 AND NOT EXISTS (SELECT 1 FROM finding_work w WHERE w.finding_id = q.id
                     AND w.kind = 'fix' AND w.checkpoint_job_id IS NOT NULL)
                 AND NOT EXISTS (SELECT 1 FROM work_holds h WHERE h.finding_id = q.id
                     AND h.kind = 'fix' AND h.released_at IS NULL)
                 AND q.fingerprint NOT IN
                     (SELECT json_extract(value, '$.fingerprint') FROM json_each(?4))
               GROUP BY q.id"#,
            repo_id,
            queued,
            new,
            moves
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut handed = 0;
        for pair in pairs {
            // Both read in this transaction: the queued finding, and the
            // scan's finding at `new` or queued by an earlier pair. Either
            // edge conflicting is a store fault, and committing past it
            // would leave a queue place given up with nobody taking it:
            // the whole handover is refused, rolled back with `tx`.
            let reason = format!("replaced by #{} from the dependency scan", pair.to_id);
            let gives = Transition::HandedOver {
                reason: &reason,
                scan: ScanEdge(()),
            };
            let takes = Transition::TakesOver {
                from_id: pair.from_id,
                scan: ScanEdge(()),
            };
            for (id, edge) in [(pair.from_id, &gives), (pair.to_id, &takes)] {
                if let Moved::Conflict { now } = Self::apply_transition(&mut tx, id, edge).await? {
                    return Err(StoreWriteError::Refused(format!(
                        "#{id} moved from {} to {now:?} inside its dep-update handover",
                        edge.from()
                    )));
                }
            }
            handed += 1;
        }
        tx.commit().await?;
        Ok(handed)
    }

    /// Record PR `pr_number` merged whatever the finding's status: a
    /// fixture write, only in a `test-support` build. The daemon records
    /// it with the finding's move ([`Self::end_pr`], [`PrEnding::Merged`]).
    #[cfg(feature = "test-support")]
    pub async fn mark_pr_merged(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let end = crate::transition::PrEnd::Merged;
        pr_state::record_end_unchecked(&mut tx, finding_id, end, pr_number, synced_at).await?;
        tx.commit().await
    }

    /// [`Self::mark_pr_merged`] for a closed PR: a fixture write
    /// ([`Self::end_pr`], [`PrEnding::Closed`], in the daemon).
    #[cfg(feature = "test-support")]
    pub async fn mark_pr_closed(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let end = crate::transition::PrEnd::Closed;
        pr_state::record_end_unchecked(&mut tx, finding_id, end, pr_number, synced_at).await?;
        tx.commit().await
    }

    /// Record what sync observed on an open PR and settle its attention, in
    /// one [`ImmediateTx`]: nothing is written once the finding has left
    /// `pr_open` ([`OpenPrRecorded::NotPrOpen`]) or moved on to another PR
    /// ([`OpenPrRecorded::OtherPr`]). See `pr_state::record_open` for why
    /// each check is there.
    pub async fn record_open_pr(
        &self,
        finding_id: i64,
        pr: &ObservedOpenPr<'_>,
    ) -> sqlx::Result<OpenPrRecorded> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let recorded = pr_state::record_open(&mut tx, finding_id, pr).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// Record an engage's answer and settle the PR's attention from it, in
    /// one transaction, while the finding is still `pr_open` with the PR
    /// answered ([`AnswerRecorded`]). Writes the answered columns only
    /// (`crate::attention`); the watermark only ever rises.
    pub async fn mark_pr_engaged(
        &self,
        finding_id: i64,
        answer: &EngageAnswer<'_>,
    ) -> sqlx::Result<AnswerRecorded> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let recorded = pr_state::record_answer(&mut tx, finding_id, answer, now_ms()).await?;
        tx.commit().await?;
        Ok(recorded)
    }

    /// Mark a merged PR as harvested.
    pub async fn mark_pr_harvested(&self, finding_id: i64, harvested_at: i64) -> sqlx::Result<()> {
        pr_state::stamp_harvested(&self.pool, finding_id, harvested_at).await
    }

    /// Record a closed PR's harvest: the finding's classification and the
    /// harvest stamp, in one transaction.
    ///
    /// Both or neither, because each write alone takes the finding out of
    /// `list_pending_harvest` in a different way. The verdict moves it off
    /// `closed` and the stamp sets `harvested_at`; with only the verdict
    /// landed, the row matches neither arm of the queue (not `merged`, no
    /// longer `closed`) while its harvest never completed, so it would be
    /// dropped for good instead of retried. `BEGIN IMMEDIATE` like the
    /// other write transactions here.
    ///
    /// The classification only replaces `closed`, the status that means
    /// "awaiting this harvest". A human may have set another verdict
    /// while the worker ran, or set one before it started (a `merged`
    /// verdict puts the finding back in the queue's merged arm, and the
    /// harvest still reviews the closure it finds in `pr_state`); either
    /// way the human's decision stands. The stamp lands regardless, since
    /// the PR was reviewed. Returns whether the classification applied.
    /// The verdict's `anchor` (or the removal of an earlier one) lands with
    /// the classification, never without it.
    pub async fn record_closed_harvest(
        &self,
        finding_id: i64,
        class: ClosureClass,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
        harvested_at: i64,
    ) -> sqlx::Result<bool> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let classify = Transition::Classified {
            class,
            reason,
            anchor,
        };
        let classified =
            Self::apply_transition(&mut tx, finding_id, &classify).await? == Moved::Applied;
        pr_state::stamp_harvested(&mut *tx, finding_id, harvested_at).await?;
        tx.commit().await?;
        Ok(classified)
    }

    /// Self-heal a missing `pr_number`; fill-only (`pr_state::fill_pr_number`).
    pub async fn fill_pr_number(&self, finding_id: i64, pr_number: i64) -> sqlx::Result<()> {
        pr_state::fill_pr_number(&self.pool, finding_id, pr_number).await
    }

    /// Findings whose pull request awaits its one-time harvest: merged, or
    /// closed without merging, and not harvested yet. Oldest sync first
    /// (NULL first), then id DESC — the order the merged-only selection had
    /// when it walked `list_findings` and sorted stably by `synced_at`.
    ///
    /// A closed PR qualifies only while its finding is `closed`, which the
    /// verdict API cannot set, so the status means exactly "awaiting this
    /// harvest". Any other status is a human's decision taken after the PR
    /// closed (re-queued, re-triaged, settled, or `rejected` outright),
    /// which the harvest's classification must not overwrite. Closures
    /// from before the closed-PR harvest, which left their finding
    /// `rejected`, were moved to `closed` once by migration 016.
    pub async fn list_pending_harvest(&self) -> sqlx::Result<Vec<Finding>> {
        let merged = FindingStatus::Merged;
        let closed = FindingStatus::Closed;
        sqlx::query_as!(
            Finding,
            r#"
            SELECT f.id, f.type AS "kind: FindingType", f.repo_id, f.fingerprint, f.file,
                   f.symbol, f.line, f.severity AS "severity: Severity", f.confidence,
                   f.summary, f.detail, f.status AS "status: FindingStatus", f.pr_url,
                   f.created_at, f.updated_at, f.bug_class AS "bug_class: BugClass",
                   f.evidence_plan, f.introduced_by, f.rung_achieved, f.verdict_reason,
                   f.budget_override AS "budget_override: BudgetOverride", f.ecosystem, f.package,
                   f.current_version, f.latest_version, f.update_type,
                   f.security_advisory, f.missing_tests, f.test_file, f.smell_type,
                   f.suggested_refactor, f.modernization_class, f.current_approach,
                   f.proposed_approach, f.standard_section
            FROM findings f
            JOIN pr_state p ON p.finding_id = f.id
            WHERE p.harvested_at IS NULL
              AND (f.status = ?1 OR (p.state = 'CLOSED' AND f.status = ?2))
            ORDER BY p.synced_at ASC, f.id DESC
            "#,
            merged,
            closed
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Put every `fixing` finding no `running` job works on back to
    /// `queued`: at startup, every claim is a dead process's. Takes the
    /// startup token, so it cannot run beside a live fix that has claimed
    /// its finding but not yet created its job. Returns the ids.
    pub async fn requeue_stranded_fixes(&self, _startup: &Unreconciled) -> sqlx::Result<Vec<i64>> {
        self.requeue_unworked_fixing(None).await
    }

    /// Put `claim`'s finding back to `queued` if it is still `fixing` and
    /// no `running` job works on it: its owner, a settled scope, can no
    /// longer release it. `true` when it was put back.
    pub async fn requeue_stranded_claim(&self, claim: &StrandedClaim) -> sqlx::Result<bool> {
        Ok(!self
            .requeue_unworked_fixing(Some(claim.finding_id()))
            .await?
            .is_empty())
    }

    /// `fixing → queued` for findings no `running` job works on; `only`
    /// limits it to one finding. One guarded UPDATE, so a finding a live
    /// job works on, or one a verdict moved on meanwhile, is left alone.
    async fn requeue_unworked_fixing(&self, only: Option<i64>) -> sqlx::Result<Vec<i64>> {
        let (fixing, queued, running) = (
            FindingStatus::Fixing,
            FindingStatus::Queued,
            JobState::Running,
        );
        let now = now_ms();
        let rows = sqlx::query!(
            r#"UPDATE findings SET status = ?1, updated_at = ?2
               WHERE status = ?3 AND (?5 IS NULL OR id = ?5) AND NOT EXISTS
                   (SELECT 1 FROM jobs j WHERE j.finding_id = findings.id AND j.state = ?4)
               RETURNING id AS "id!""#,
            queued,
            now,
            fixing,
            running,
            only
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.id).collect())
    }

    /// The identical-failure streak of `finding_id`'s `kind` work: zero and
    /// no key while the work item has no row.
    pub async fn work_streak(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
    ) -> sqlx::Result<WorkStreak> {
        Ok(sqlx::query_as!(
            WorkStreak,
            r#"SELECT streak AS "streak!: i64", last_failure FROM finding_work
               WHERE finding_id = ?1 AND kind = ?2"#,
            finding_id,
            kind
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default())
    }

    /// Count one failed attempt of `finding_id`'s `kind` work under `key`
    /// and return the streak it makes: one more than before when the last
    /// failure had the same key, else 1. One statement, so two writers
    /// cannot both read the old streak and lose a count.
    ///
    /// `None`, and nothing counted, when the finding is no longer in the
    /// kind's tier, or a harvest's PR is harvested already: that work is
    /// over, and leaving the tier ended its streak (migration 024). PR sync
    /// runs beside the job lane and can end an engage's PR while its worker
    /// runs; counted, that failure would start the streak of the next PR's
    /// engage, which `hold_work` refuses for the same reason.
    ///
    /// The checkpoint is left alone (WORK-MODEL.md §6.3): an attempt that
    /// ran has moved it in `complete_job` already, and a failure without a
    /// new attempt (a fetch, a PR view, a runner that would not start) did
    /// no work that could replace the paused one, so throwing it away would
    /// lose that work to what is most often a network that is not back yet.
    ///
    /// The job lane's alone, through [`JobStore`].
    async fn record_work_failure(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        key: &str,
    ) -> sqlx::Result<Option<i64>> {
        let harvest = FindingJobKind::Harvest;
        sqlx::query_scalar!(
            r#"INSERT INTO finding_work (finding_id, kind, streak, last_failure)
               SELECT ?1, ?2, 1, ?3
               WHERE EXISTS (SELECT 1 FROM findings f
                             JOIN work_tiers t ON t.status = f.status AND t.kind = ?2
                             WHERE f.id = ?1)
                 AND (?2 <> ?4 OR NOT EXISTS (SELECT 1 FROM pr_state p
                      WHERE p.finding_id = ?1 AND p.harvested_at IS NOT NULL))
               ON CONFLICT(finding_id, kind) DO UPDATE SET
                 streak = CASE WHEN finding_work.last_failure IS excluded.last_failure
                     THEN finding_work.streak + 1 ELSE 1 END,
                 last_failure = excluded.last_failure
               RETURNING streak AS "streak!: i64""#,
            finding_id,
            kind,
            key,
            harvest
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// End `finding_id`'s `kind` streak: the work concluded without
    /// leaving its tier, as an engage does (leaving the tier ends it on its
    /// own, migration 024's trigger).
    pub async fn clear_work_streak(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE finding_work SET streak = 0, last_failure = NULL \
             WHERE finding_id = ?1 AND kind = ?2",
            finding_id,
            kind
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -- holds (WORK-MODEL.md §9, §17) -----------------------------------------

    /// Every work item held for the operator, a finding's or a repo's: the
    /// one answer to "is this held?", which every finding tier, the resume
    /// tier, the repo rotation and the summary preview all read (§9.5,
    /// invariant 3). One statement, so the two halves cannot come from two
    /// different moments.
    pub async fn held_work(&self) -> sqlx::Result<HeldWork> {
        let rows = sqlx::query!(
            r#"SELECT finding_id, repo_id, kind AS "kind!: String"
               FROM work_holds WHERE released_at IS NULL"#
        )
        .fetch_all(&self.pool)
        .await?;
        let mut held = HeldWork::default();
        let bad = |e: String| sqlx::Error::Decode(e.into());
        for r in rows {
            match (r.finding_id, r.repo_id) {
                (Some(fid), _) => {
                    held.findings.insert((fid, r.kind.parse().map_err(bad)?));
                }
                (None, Some(rid)) => {
                    held.repos.insert((rid, r.kind.parse().map_err(bad)?));
                }
                // The schema's CHECK rules this out.
                (None, None) => {}
            }
        }
        Ok(held)
    }

    /// The place every `running` job holds ([`JobPlace`]): what selection
    /// skips, since a second attempt there is refused at insert.
    pub async fn running_places(&self) -> sqlx::Result<HashSet<JobPlace>> {
        let running = JobState::Running;
        let rows = sqlx::query!(
            r#"SELECT kind AS "kind!: JobKind", repo_id, finding_id
               FROM jobs WHERE state = ?1"#,
            running
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| JobPlace::of(r.kind, r.repo_id, r.finding_id))
            .collect())
    }

    /// Every active hold with the facts its release depends on, oldest
    /// first: for the API's `holds`.
    pub async fn active_holds(&self) -> sqlx::Result<Vec<WorkHold>> {
        sqlx::query_as!(
            WorkHold,
            r#"
            SELECT h.id AS "id!: i64", h.finding_id AS "finding_id!: i64",
                   h.kind AS "kind!: FindingJobKind", h.reason AS "reason!: HoldReason",
                   h.detail, h.held_at AS "held_at!: i64",
                   w.checkpoint_job_id,
                   f.status AS "status!: FindingStatus",
                   (h.kind = 'engage' AND COALESCE(p.last_activity_at, 0) > h.held_at)
                       AS "new_activity!: bool"
            FROM work_holds h
            JOIN findings f ON f.id = h.finding_id
            LEFT JOIN finding_work w ON w.finding_id = h.finding_id AND w.kind = h.kind
            LEFT JOIN pr_state p ON p.finding_id = h.finding_id
            WHERE h.released_at IS NULL
            ORDER BY h.held_at, h.id
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Hold `finding_id`'s `kind` work for the operator (§9), in one
    /// transaction with its `hold` event. The streak ends, and the work
    /// item's checkpoint becomes `job` if a worker ran in it (it has a
    /// transcript). Without one (no job, or a tree never made, G9) the
    /// checkpoint the work item already had stays: the work done before it
    /// is still there to continue, and with none the operator can only
    /// restart.
    ///
    /// Only while the finding is in that kind's tier (invariant 2): work
    /// that has left it is over, and `false` says nothing was held. `false`
    /// too when the work item is already held, which the partial unique
    /// index enforces. The job lane's alone, through [`JobStore`].
    async fn hold_work(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        reason: HoldReason,
        detail: &str,
        job: Option<i64>,
    ) -> sqlx::Result<bool> {
        let now = now_ms();
        let harvest = FindingJobKind::Harvest;
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let held = sqlx::query!(
            "INSERT INTO work_holds (finding_id, kind, reason, detail, held_at) \
             SELECT ?1, ?2, ?3, ?4, ?5 \
             WHERE EXISTS (SELECT 1 FROM findings f \
                           JOIN work_tiers t ON t.status = f.status AND t.kind = ?2 \
                           WHERE f.id = ?1) \
               AND (?2 <> ?6 OR NOT EXISTS (SELECT 1 FROM pr_state p \
                    WHERE p.finding_id = ?1 AND p.harvested_at IS NOT NULL)) \
             ON CONFLICT (finding_id, kind) WHERE released_at IS NULL DO NOTHING",
            finding_id,
            kind,
            reason,
            detail,
            now,
            harvest
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !held {
            return Ok(false);
        }
        sqlx::query!(
            "INSERT INTO finding_work (finding_id, kind, streak, last_failure, checkpoint_job_id) \
             VALUES (?1, ?2, 0, NULL, \
                     (SELECT id FROM jobs WHERE id = ?3 AND session_file IS NOT NULL)) \
             ON CONFLICT (finding_id, kind) DO UPDATE SET \
                 streak = 0, last_failure = NULL, \
                 checkpoint_job_id = COALESCE(excluded.checkpoint_job_id, \
                                              finding_work.checkpoint_job_id)",
            finding_id,
            kind,
            job
        )
        .execute(&mut *tx)
        .await?;
        let first_line: String = detail
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        let message = format!("#{finding_id} {kind} held ({reason}): {first_line}");
        insert_event(&mut *tx, "hold", &message, job, Some(finding_id), None).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// When `finding_id`'s `kind` work was last released, if ever. The give-up
    /// ceiling counts only what the scheduler did after that (§8).
    pub async fn last_release_at(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
    ) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar!(
            r#"SELECT MAX(released_at) AS "at?: i64" FROM work_holds
               WHERE finding_id = ?1 AND kind = ?2"#,
            finding_id,
            kind
        )
        .fetch_one(&self.pool)
        .await
    }

    /// The hold an operator released with `continue` after checkpoint `job`
    /// was made, if there is one: the first attempt resuming `job` is the
    /// one the operator asked for, and its prompt says why the work was
    /// held. A later checkpoint was made after the release, so its resume
    /// is an ordinary one.
    pub async fn continued_hold(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        job: i64,
    ) -> sqlx::Result<Option<(HoldReason, String)>> {
        let continue_ = ReleaseAction::Continue;
        let row = sqlx::query!(
            r#"SELECT h.reason AS "reason!: HoldReason", h.detail
               FROM work_holds h
               WHERE h.finding_id = ?1 AND h.kind = ?2 AND h.release_action = ?3
                 AND h.released_at >= COALESCE(
                     (SELECT finished_at FROM jobs WHERE id = ?4), 0)
               ORDER BY h.released_at DESC, h.id DESC
               LIMIT 1"#,
            finding_id,
            kind,
            continue_,
            job
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.reason, r.detail)))
    }

    /// An operator's release of `finding_id`'s `kind` hold (§9.3), in one
    /// transaction: the hold row records who, when and how, the action
    /// takes effect, and the `release` event (plus `verdict` for a verdict)
    /// is logged as theirs.
    ///
    /// Refused when there is no active hold, when `action` does not apply
    /// to the kind or to how the PR ended, when `continue` has no
    /// checkpoint to continue, or when a verdict lacks its reason or a
    /// classification its class.
    pub async fn release_hold(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        release: &HoldRelease<'_>,
    ) -> Result<(), StoreWriteError> {
        let HoldRelease {
            action,
            reason,
            classification,
            by,
        } = *release;
        let refuse = |msg: String| Err(StoreWriteError::Refused(msg));
        if !action.applies_to(kind) {
            return refuse(format!("a {kind} hold cannot be released with {action}"));
        }
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let hold = sqlx::query!(
            r#"SELECT h.id AS "id!: i64", w.checkpoint_job_id,
                      f.status AS "status!: FindingStatus"
               FROM work_holds h
               JOIN findings f ON f.id = h.finding_id
               LEFT JOIN finding_work w ON w.finding_id = h.finding_id AND w.kind = h.kind
               WHERE h.finding_id = ?1 AND h.kind = ?2 AND h.released_at IS NULL"#,
            finding_id,
            kind
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(hold) = hold else {
            return refuse(format!("#{finding_id} has no active {kind} hold"));
        };
        if !action_fits(action, hold.status, hold.checkpoint_job_id.is_some()) {
            return refuse(format!(
                "#{finding_id}'s {kind} hold cannot be released with {action} \
                 (finding {}, {})",
                hold.status,
                if hold.checkpoint_job_id.is_some() {
                    "with a checkpoint"
                } else {
                    "no checkpoint"
                }
            ));
        }
        let verdict = release_verdict(action, reason, classification)?;

        let released_by = by.username.as_str();
        sqlx::query!(
            "UPDATE work_holds SET released_at = ?1, released_by = ?2, release_action = ?3 \
             WHERE id = ?4",
            now,
            released_by,
            action,
            hold.id
        )
        .execute(&mut *tx)
        .await?;
        let message = format!("#{finding_id} {kind} hold released: {action}");
        insert_event(
            &mut *tx,
            "release",
            &message,
            None,
            Some(finding_id),
            Some(by.id),
        )
        .await?;
        apply_release(&mut tx, finding_id, kind, action, now).await?;
        if let Some((verdict, text)) = verdict {
            write_release_verdict(&mut tx, finding_id, hold.status, verdict, &text, by.id, now)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // -- repo work (WORK-MODEL.md §17) ----------------------------------------

    /// The identical-failure streak of `repo_id`'s `kind` work: zero and no
    /// key while the work item has no row.
    pub async fn repo_work_streak(
        &self,
        repo_id: i64,
        kind: RepoWorkKind,
    ) -> sqlx::Result<WorkStreak> {
        Ok(sqlx::query_as!(
            WorkStreak,
            r#"SELECT streak AS "streak!: i64", last_failure FROM repo_work
               WHERE repo_id = ?1 AND kind = ?2"#,
            repo_id,
            kind
        )
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or_default())
    }

    /// Count one failure of `repo_id`'s `kind` work under `key` and return
    /// the streak it makes, as [`Self::record_work_failure`] does for a
    /// finding's. The checkpoint is left alone: an attempt that ran has
    /// moved it in `complete_job` already, and a failure without a new
    /// attempt did no work that could replace it.
    pub async fn record_repo_failure(
        &self,
        repo_id: i64,
        kind: RepoWorkKind,
        key: &str,
    ) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            r#"INSERT INTO repo_work (repo_id, kind, streak, last_failure)
               VALUES (?1, ?2, 1, ?3)
               ON CONFLICT(repo_id, kind) DO UPDATE SET
                 streak = CASE WHEN repo_work.last_failure IS excluded.last_failure
                     THEN repo_work.streak + 1 ELSE 1 END,
                 last_failure = excluded.last_failure
               RETURNING streak AS "streak!: i64""#,
            repo_id,
            kind,
            key
        )
        .fetch_one(&self.pool)
        .await
    }

    /// `repo_id`'s `kind` work succeeded (§6.3): its streak ends and its
    /// checkpoint is cleared.
    pub async fn repo_work_succeeded(&self, repo_id: i64, kind: RepoWorkKind) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE repo_work SET streak = 0, last_failure = NULL, checkpoint_job_id = NULL \
             WHERE repo_id = ?1 AND kind = ?2",
            repo_id,
            kind
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that Renovate had no answer for `repo_id` just now, so its
    /// AI `dep_update` scan is due on the job lane (migration 032). Only
    /// the admin lane's Renovate scan decides that, so only [`AdminStore`]
    /// exposes this: the job lane cannot request its own AI scan.
    async fn request_dep_update_fallback(&self, repo_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO dep_scan_fallbacks (repo_id, requested_at) VALUES (?1, ?2) \
             ON CONFLICT (repo_id) DO UPDATE SET requested_at = excluded.requested_at",
            repo_id,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The repos whose AI `dep_update` scan is due: Renovate had no answer
    /// for them after their last `dep_update` run ([`DepScanFallbacks`]).
    pub async fn dep_update_fallbacks(&self) -> sqlx::Result<DepScanFallbacks> {
        let rows = sqlx::query!(
            r#"SELECT f.repo_id AS "repo_id!: i64"
               FROM dep_scan_fallbacks f JOIN repos r ON r.id = f.repo_id
               WHERE f.requested_at > COALESCE(r.last_dep_update_at, 0)"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(DepScanFallbacks(
            rows.into_iter().map(|r| r.repo_id).collect(),
        ))
    }

    /// Every live repo's work item that is failing and not yet held: its
    /// streak and the key it fails with, by repo then kind.
    pub async fn repo_streaks(&self) -> sqlx::Result<Vec<RepoStreak>> {
        sqlx::query_as!(
            RepoStreak,
            r#"SELECT w.repo_id AS "repo_id!: i64", w.kind AS "kind!: RepoWorkKind",
                      w.streak AS "streak!: i64", w.last_failure AS "last_failure!: String"
               FROM repo_work w
               JOIN repos r ON r.id = w.repo_id
               WHERE r.deleted_at IS NULL AND w.streak > 0 AND w.last_failure IS NOT NULL
               ORDER BY w.repo_id, w.kind"#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Every active hold on a live repo's work, with its checkpoint, oldest
    /// first: for the API's repo `holds` and the needs-you list.
    pub async fn active_repo_holds(&self) -> sqlx::Result<Vec<RepoHold>> {
        sqlx::query_as!(
            RepoHold,
            r#"
            SELECT h.id AS "id!: i64", h.repo_id AS "repo_id!: i64",
                   h.kind AS "kind!: RepoWorkKind", h.reason AS "reason!: HoldReason",
                   h.detail, h.held_at AS "held_at!: i64",
                   w.checkpoint_job_id
            FROM work_holds h
            JOIN repos r ON r.id = h.repo_id
            LEFT JOIN repo_work w ON w.repo_id = h.repo_id AND w.kind = h.kind
            WHERE h.released_at IS NULL AND r.deleted_at IS NULL
            ORDER BY h.held_at, h.id
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Hold `repo_id`'s `kind` work for the operator, in one transaction
    /// with its `hold` event, as [`Self::hold_work`] does for a finding's:
    /// the streak ends, and the checkpoint becomes `job` if a worker ran in
    /// it, else stays what it was. `false` when nothing was held: the repo
    /// is gone, or the work item is already held.
    pub async fn hold_repo_work(
        &self,
        repo_id: i64,
        kind: RepoWorkKind,
        reason: HoldReason,
        detail: &str,
        job: Option<i64>,
    ) -> sqlx::Result<bool> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let held = sqlx::query!(
            "INSERT INTO work_holds (repo_id, kind, reason, detail, held_at) \
             SELECT ?1, ?2, ?3, ?4, ?5 \
             WHERE EXISTS (SELECT 1 FROM repos WHERE id = ?1 AND deleted_at IS NULL) \
             ON CONFLICT (repo_id, kind) WHERE released_at IS NULL DO NOTHING",
            repo_id,
            kind,
            reason,
            detail,
            now
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !held {
            return Ok(false);
        }
        sqlx::query!(
            "INSERT INTO repo_work (repo_id, kind, streak, last_failure, checkpoint_job_id) \
             VALUES (?1, ?2, 0, NULL, \
                     (SELECT id FROM jobs WHERE id = ?3 AND session_file IS NOT NULL)) \
             ON CONFLICT (repo_id, kind) DO UPDATE SET \
                 streak = 0, last_failure = NULL, \
                 checkpoint_job_id = COALESCE(excluded.checkpoint_job_id, \
                                              repo_work.checkpoint_job_id)",
            repo_id,
            kind,
            job
        )
        .execute(&mut *tx)
        .await?;
        let first_line: String = detail
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(200)
            .collect();
        let name = sqlx::query_scalar!("SELECT name FROM repos WHERE id = ?1", repo_id)
            .fetch_one(&mut *tx)
            .await?;
        let message = format!("{name} {kind} held ({reason}): {first_line}");
        insert_event(&mut *tx, "hold", &message, job, None, None).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// When `repo_id`'s `kind` work was last released, if ever: the give-up
    /// ceiling counts only what the scheduler did after that (§8).
    pub async fn repo_last_release_at(
        &self,
        repo_id: i64,
        kind: RepoJobKind,
    ) -> sqlx::Result<Option<i64>> {
        let kind = RepoWorkKind::from(kind);
        sqlx::query_scalar!(
            r#"SELECT MAX(released_at) AS "at?: i64" FROM work_holds
               WHERE repo_id = ?1 AND kind = ?2"#,
            repo_id,
            kind
        )
        .fetch_one(&self.pool)
        .await
    }

    /// The hold an operator released with `continue` after checkpoint `job`
    /// was made, if there is one, as [`Self::continued_hold`] answers for a
    /// finding's work.
    pub async fn repo_continued_hold(
        &self,
        repo_id: i64,
        kind: RepoJobKind,
        job: i64,
    ) -> sqlx::Result<Option<(HoldReason, String)>> {
        let kind = RepoWorkKind::from(kind);
        let continue_ = ReleaseAction::Continue;
        let row = sqlx::query!(
            r#"SELECT h.reason AS "reason!: HoldReason", h.detail
               FROM work_holds h
               WHERE h.repo_id = ?1 AND h.kind = ?2 AND h.release_action = ?3
                 AND h.released_at >= COALESCE(
                     (SELECT finished_at FROM jobs WHERE id = ?4), 0)
               ORDER BY h.released_at DESC, h.id DESC
               LIMIT 1"#,
            repo_id,
            kind,
            continue_,
            job
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| (r.reason, r.detail)))
    }

    /// An operator's release of `repo_id`'s `kind` hold, in one transaction:
    /// the hold row records who, when and how, the streak starts from zero
    /// (§16), `restart` drops the checkpoint, and the `release` event is
    /// logged as theirs. Refused when there is no active hold, when
    /// `action` does not apply to the kind, or when `continue` has no
    /// checkpoint to continue.
    pub async fn release_repo_hold(
        &self,
        repo_id: i64,
        kind: RepoWorkKind,
        action: ReleaseAction,
        by: &CurrentUser,
    ) -> Result<(), StoreWriteError> {
        let refuse = |msg: String| Err(StoreWriteError::Refused(msg));
        if !action.applies_to_repo(kind) {
            return refuse(format!(
                "a repo's {kind} hold cannot be released with {action}"
            ));
        }
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let hold = sqlx::query!(
            r#"SELECT h.id AS "id!: i64", w.checkpoint_job_id, r.name
               FROM work_holds h
               JOIN repos r ON r.id = h.repo_id
               LEFT JOIN repo_work w ON w.repo_id = h.repo_id AND w.kind = h.kind
               WHERE h.repo_id = ?1 AND h.kind = ?2 AND h.released_at IS NULL
                 AND r.deleted_at IS NULL"#,
            repo_id,
            kind
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(hold) = hold else {
            return refuse(format!("repo {repo_id} has no active {kind} hold"));
        };
        if action == ReleaseAction::Continue && hold.checkpoint_job_id.is_none() {
            return refuse(format!(
                "{}'s {kind} hold has no checkpoint to continue",
                hold.name
            ));
        }
        let released_by = by.username.as_str();
        sqlx::query!(
            "UPDATE work_holds SET released_at = ?1, released_by = ?2, release_action = ?3 \
             WHERE id = ?4",
            now,
            released_by,
            action,
            hold.id
        )
        .execute(&mut *tx)
        .await?;
        let restart = action == ReleaseAction::Restart;
        sqlx::query!(
            "UPDATE repo_work SET streak = 0, last_failure = NULL, \
                 checkpoint_job_id = CASE WHEN ?3 THEN NULL ELSE checkpoint_job_id END \
             WHERE repo_id = ?1 AND kind = ?2",
            repo_id,
            kind,
            restart
        )
        .execute(&mut *tx)
        .await?;
        let message = format!("{} {kind} hold released: {action}", hold.name);
        insert_event(&mut *tx, "release", &message, None, None, Some(by.id)).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The guarded UPDATE behind a fix claim's release
    /// ([`FixClaim::release`](crate::recovery::FixClaim::release)).
    async fn move_if_at(
        &self,
        finding_id: i64,
        from: FindingStatus,
        to: FindingStatus,
    ) -> sqlx::Result<bool> {
        let now = now_ms();
        let claimed = sqlx::query!(
            "UPDATE findings SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = ?4",
            to,
            now,
            finding_id,
            from
        )
        .execute(&self.pool)
        .await?;
        Ok(claimed.rows_affected() > 0)
    }

    /// Claim `finding_id` for a fix (`queued` → `fixing`) and read the row
    /// as the claim took it, in one transaction; `None` when it is no
    /// longer `queued`. Takes an [`Owner`], which only `crate::recovery`
    /// makes, inside [`CycleScope::claim_fix`](crate::recovery::CycleScope::claim_fix):
    /// every claim is a [`FixClaim`](crate::recovery::FixClaim) that releases it.
    ///
    /// The row is read here, not at the pick, because a `queued` row can
    /// still change between the two: the admin lane's Renovate pass
    /// rewrites a `queued` dep update's target ([`Self::refresh_dep_update`]).
    /// Once `fixing`, nothing rewrites it, so the fix works from this row
    /// throughout.
    pub(crate) async fn claim_fixing(
        &self,
        _owner: &Owner,
        finding_id: i64,
    ) -> sqlx::Result<Option<Finding>> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (queued, fixing, now) = (FindingStatus::Queued, FindingStatus::Fixing, now_ms());
        let claimed = sqlx::query!(
            "UPDATE findings SET status = ?1, updated_at = ?2 WHERE id = ?3 AND status = ?4",
            fixing,
            now,
            finding_id,
            queued
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if !claimed {
            return Ok(None);
        }
        let row = Self::read_finding(&mut *tx, finding_id).await?;
        tx.commit().await?;
        Ok(row)
    }

    /// Release a fix claim (`fixing` → `queued`) if the finding is still
    /// `fixing`. Takes an [`Owner`], made only inside
    /// [`FixClaim::release`](crate::recovery::FixClaim::release).
    pub(crate) async fn release_fixing(
        &self,
        _owner: &Owner,
        finding_id: i64,
    ) -> sqlx::Result<bool> {
        self.move_if_at(finding_id, FindingStatus::Fixing, FindingStatus::Queued)
            .await
    }

    // -- stats -----------------------------------------------------------------

    pub async fn stats_totals(&self) -> sqlx::Result<StatsTotals> {
        sqlx::query_as!(
            StatsTotals,
            r#"
            SELECT COUNT(*) AS "jobs!: i64",
                   SUM(tokens_new) AS "total_tokens: i64",
                   SUM(calls) AS "total_calls: i64",
                   SUM(usage_delta) AS "total_usage_delta: f64",
                   SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END) AS "done: i64",
                   SUM(CASE WHEN state = 'denied' THEN 1 ELSE 0 END) AS "denied: i64"
            FROM jobs
            "#
        )
        .fetch_one(&self.pool)
        .await
    }

    pub async fn stats_by_kind(&self) -> sqlx::Result<Vec<StatsByKind>> {
        sqlx::query_as!(
            StatsByKind,
            r#"
            SELECT kind AS "kind!: JobKind", COUNT(*) AS "jobs!: i64",
                   SUM(CASE WHEN state = 'done' THEN 1 ELSE 0 END) AS "done: i64",
                   SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END) AS "failed: i64",
                   SUM(CASE WHEN state = 'killed' THEN 1 ELSE 0 END) AS "killed: i64",
                   SUM(CASE WHEN state = 'denied' THEN 1 ELSE 0 END) AS "denied: i64",
                   SUM(tokens_new) AS "total_tokens: i64",
                   SUM(calls) AS "total_calls: i64",
                   AVG(tokens_new) AS "avg_tokens: f64",
                   SUM(usage_delta) AS "total_usage_delta: f64",
                   GROUP_CONCAT(DISTINCT model) AS "models: String"
            FROM jobs
            GROUP BY kind
            ORDER BY kind
            "#
        )
        .fetch_all(&self.pool)
        .await
    }

    pub async fn stats_by_finding(&self) -> sqlx::Result<Vec<StatsByFinding>> {
        // ORDER BY repeats the SUM expression: the Python source orders by the
        // `total_tokens` alias, but our alias carries a sqlx type override, so
        // the bare name would not resolve. Identical semantics (NULLs last on
        // DESC in SQLite).
        sqlx::query_as!(
            StatsByFinding,
            r#"
            SELECT j.finding_id AS "finding_id!: i64",
                   f.fingerprint AS "fingerprint!", f.status AS "status!",
                   f.severity AS "severity!", COUNT(*) AS "jobs!: i64",
                   SUM(j.tokens_new) AS "total_tokens: i64",
                   SUM(j.calls) AS "total_calls: i64",
                   SUM(j.usage_delta) AS "total_usage_delta: f64"
            FROM jobs j
            JOIN findings f ON f.id = j.finding_id
            WHERE j.finding_id IS NOT NULL
            GROUP BY j.finding_id
            ORDER BY SUM(j.tokens_new) DESC
            "#
        )
        .fetch_all(&self.pool)
        .await
    }
}

/// What only the store's dependency-scan writes can make: the finding
/// transitions that carry one ([`Transition::Unproposed`],
/// [`Transition::HandedOver`], [`Transition::TakesOver`] and
/// [`Transition::Reproposed`]) cannot be built anywhere else, so
/// [`Store::transition`] cannot be handed one. The first three are made
/// only through [`AdminStore`], Renovate's; the last by the guarded
/// `dep_update` refresh both lanes' ingest runs.
#[derive(Debug, Clone, Copy)]
pub struct ScanEdge(());

/// The store as the job lane writes it: everything a [`Store`] does,
/// through `Deref`, and the writes only the job lane makes. Those end a
/// chain's checkpoint and count or hold a finding's work, which only the
/// lane's own selection and runners may decide.
///
/// Made only from the [`JobAuthority`] (by its
/// [`CycleScope`](crate::recovery::CycleScope)), so an admin duty or a
/// handler, holding a plain [`Store`], has none of these writes to call.
pub struct JobStore(Store);

impl JobStore {
    pub(crate) fn of(_jobs: &JobAuthority, store: Store) -> Self {
        Self(store)
    }

    /// Give up `finding_id`'s `kind` checkpoint at `job_id`, with its
    /// `resume` event; `false` when the pointer has moved on since.
    pub async fn drop_checkpoint(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        job_id: i64,
        msg: &str,
    ) -> sqlx::Result<bool> {
        self.0.drop_checkpoint(finding_id, kind, job_id, msg).await
    }

    /// Give up `repo_id`'s `kind` checkpoint at `job_id`, as
    /// [`Self::drop_checkpoint`] does for a finding's work.
    pub async fn drop_repo_checkpoint(
        &self,
        repo_id: i64,
        kind: RepoJobKind,
        job_id: i64,
        msg: &str,
    ) -> sqlx::Result<bool> {
        self.0
            .drop_repo_checkpoint(repo_id, kind, job_id, msg)
            .await
    }

    /// Count a failure of `finding_id`'s `kind` work under `key`: the
    /// streak it makes, or `None` when the finding has left the kind's
    /// tier and nothing was counted (WORK-MODEL.md §7).
    pub async fn record_work_failure(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        key: &str,
    ) -> sqlx::Result<Option<i64>> {
        self.0.record_work_failure(finding_id, kind, key).await
    }

    /// Hold `finding_id`'s `kind` work for the operator (WORK-MODEL.md
    /// §9); `false` when the finding has left the kind's tier or the work
    /// is already held.
    pub async fn hold_work(
        &self,
        finding_id: i64,
        kind: FindingJobKind,
        reason: HoldReason,
        detail: &str,
        job: Option<i64>,
    ) -> sqlx::Result<bool> {
        self.0
            .hold_work(finding_id, kind, reason, detail, job)
            .await
    }
}

impl std::ops::Deref for JobStore {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.0
    }
}

/// The store as the admin lane writes it: everything a [`Store`] does,
/// through `Deref`, and the writes only the admin lane's Renovate scan
/// makes. Only that scan decides what Renovate proposes, retires or hands
/// over, and when the job lane's AI scan is wanted instead.
///
/// Made only with the startup proof ([`Recovered`]), by
/// [`Recovered::into_lanes`] for the [`AdminAuthority`], which only the
/// admin lane holds.
pub struct AdminStore(Store);

impl AdminStore {
    pub(crate) fn of(_recovered: &Recovered, store: Store) -> Self {
        Self(store)
    }

    /// Retire the repo's `new` `dep_update` findings Renovate no longer
    /// proposes; see the private `Store` method of the same name.
    pub async fn supersede_unproposed_dep_updates(
        &self,
        repo_id: i64,
        proposed: &[String],
        unchecked: &[String],
    ) -> Result<u64, StoreWriteError> {
        self.0
            .supersede_unproposed_dep_updates(repo_id, proposed, unchecked)
            .await
    }

    /// Hand each queued `dep_update` in `moves` over to the scan's finding
    /// for the same move; see the private `Store` method of the same name.
    pub async fn hand_over_queued_dep_updates(
        &self,
        repo_id: i64,
        moves: &[ProposedMove],
    ) -> Result<u64, StoreWriteError> {
        self.0.hand_over_queued_dep_updates(repo_id, moves).await
    }

    /// Ask for the job lane's AI dependency scan of `repo_id`: Renovate had
    /// no answer (migration 032).
    pub async fn request_dep_update_fallback(&self, repo_id: i64) -> sqlx::Result<()> {
        self.0.request_dep_update_fallback(repo_id).await
    }
}

impl std::ops::Deref for AdminStore {
    type Target = Store;

    fn deref(&self) -> &Store {
        &self.0
    }
}

/// `SpendLedger` over the same pool (BACKEND-CONTRACT.md §1.6 — exact SQL
/// there; Python's `ThreadLocalLedger` dissolves under the pool).
#[async_trait::async_trait]
impl crate::backend::SpendLedger for Store {
    async fn running_estimate(&self) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            // Per job this is its `estimated_tokens` — what the ramp
            // reserved when it granted the job — and not its
            // `cap_tokens`. The cap is a kill threshold, not a
            // reservation, and a job may carry no finite cap at all;
            // summing caps would then reserve nothing for a job that is
            // very much spending.
            //
            // Rows written before `estimated_tokens` existed have NULL
            // there and fall back to their cap, which is what the sum
            // meant for them at the time. The trailing 0 keeps a row
            // with neither from turning the whole sum NULL.
            r#"SELECT COALESCE(SUM(COALESCE(estimated_tokens, cap_tokens, 0)), 0) AS "total!: i64"
               FROM jobs WHERE state = 'running'"#
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            // `tokens_new IS NOT NULL` does not change the sum — SUM skips
            // NULLs — but it is what makes the partial `jobs_finished_at`
            // index eligible. Without it SQLite full-scans `jobs` on the
            // endpoint the UI polls every 5s.
            r#"SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64"
               FROM jobs
               WHERE state != 'running'
                 AND finished_at > ?1
                 AND tokens_new IS NOT NULL"#,
            ts_ms
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn finished_between(&self, start_ms: i64, end_ms: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar!(
            // Same partial-index predicate as `finished_since`.
            r#"SELECT COALESCE(SUM(tokens_new), 0) AS "total!: i64"
               FROM jobs
               WHERE state != 'running'
                 AND finished_at > ?1
                 AND finished_at <= ?2
                 AND tokens_new IS NOT NULL"#,
            start_ms,
            end_ms
        )
        .fetch_one(&self.pool)
        .await
    }

    async fn log_window_observation(
        &self,
        limit_id: &str,
        used_fraction: Option<f64>,
        status: Option<&str>,
        resets_at: Option<i64>,
        age_s: f64,
    ) -> sqlx::Result<()> {
        let observed_at = now_ms();
        // int(age_s): truncation toward zero, matching Python (`Store.log_window_observation`).
        let source_age_s = age_s as i64;
        sqlx::query!(
            "INSERT INTO window_log \
             (observed_at, limit_id, used_fraction, status, resets_at, source_age_s) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            observed_at,
            limit_id,
            used_fraction,
            status,
            resets_at,
            source_age_s
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn last_window_observation(
        &self,
        limit_id: &str,
        resets_at: i64,
    ) -> sqlx::Result<Option<(i64, f64)>> {
        let lo = resets_at - 5_000;
        let hi = resets_at + 5_000;
        let row = sqlx::query!(
            r#"SELECT observed_at AS "observed_at!: i64",
                      used_fraction AS "used_fraction: f64"
               FROM window_log
               WHERE limit_id = ?1 AND resets_at BETWEEN ?2 AND ?3
               ORDER BY observed_at DESC
               LIMIT 1"#,
            limit_id,
            lo,
            hi
        )
        .fetch_optional(&self.pool)
        .await?;
        // None when no row OR the newest row's used_fraction is NULL
        // (`Store.last_window_observation`).
        Ok(row.and_then(|r| r.used_fraction.map(|f| (r.observed_at, f))))
    }

    async fn record_calibration_sample(
        &self,
        limit_id: &str,
        window_resets_at: Option<i64>,
        used_fraction_delta: f64,
        hunter_tokens: i64,
    ) -> sqlx::Result<()> {
        let observed_at = now_ms();
        sqlx::query!(
            "INSERT INTO calibration_samples \
             (observed_at, limit_id, window_resets_at, used_fraction_delta, hunter_tokens) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            observed_at,
            limit_id,
            window_resets_at,
            used_fraction_delta,
            hunter_tokens
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn estimate_capacity(&self, limit_id: &str) -> sqlx::Result<Option<f64>> {
        // Newest completed cycles considered (store.py sample_limit=200).
        const SAMPLE_LIMIT: i64 = 200;
        // Per-model-class / unknown lids have no known period. A calendar
        // month binds NULL: the SQL derives each cycle's own start.
        let Some(period) = crate::backends::omp_scavenge::LlmProvider::window_period(limit_id)
        else {
            return Ok(None);
        };
        let period_ms = period.fixed_ms();
        let now = now_ms();
        // One statement, not one per cycle. Previously this fetched up to
        // SAMPLE_LIMIT cycles and then ran a SUM over `jobs` for each of
        // them; with 200 cycles that is 200 sequential round trips, and
        // `status_html` plus `decide` call this up to six times per
        // GET /api/summary — the endpoint the UI polls every 5s.
        //
        // The cycle set still dedupes resets_at into 10 s buckets and
        // takes the newest SAMPLE_LIMIT, and the per-cycle spend is still
        // a half-open (start, resets] window; only the number of
        // statements changes.
        let best = sqlx::query_scalar!(
            r#"
            WITH cycles AS (
                SELECT MIN(resets_at) AS resets
                FROM window_log
                WHERE limit_id = ?1 AND resets_at < ?2
                GROUP BY CAST(resets_at / 10000 AS INT)
                ORDER BY CAST(resets_at / 10000 AS INT) DESC
                LIMIT ?3
            )
            SELECT COALESCE(MAX(spent), 0) AS "best!: i64"
            FROM (
                SELECT (
                    SELECT COALESCE(SUM(j.tokens_new), 0)
                    FROM jobs j
                    WHERE j.state NOT IN ('denied', 'running')
                      AND j.tokens_new IS NOT NULL
                      AND j.finished_at > CASE
                          WHEN ?4 IS NULL THEN CAST(strftime(
                              '%s', c.resets / 1000, 'unixepoch',
                              'start of month', '-1 month'
                          ) AS INTEGER) * 1000
                          ELSE c.resets - ?4
                      END
                      AND j.finished_at <= c.resets
                ) AS spent
                FROM cycles c
                WHERE c.resets IS NOT NULL
            )
            "#,
            limit_id,
            now,
            SAMPLE_LIMIT,
            period_ms
        )
        .fetch_one(&self.pool)
        .await?;
        Ok((best > 0).then_some(best as f64))
    }
}
