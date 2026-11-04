//! Domain and API response types — THE parity contract with the Python
//! server (see API-CONTRACT.md). JSON key names and presence/null semantics
//! are load-bearing: the Svelte UI (`hunter/ui-svelte/`) consumes these,
//! and `lib/validate.ts` structurally checks what its components
//! dereference without a `?.`. That is a weaker contract than the zod
//! schema this comment used to describe: it rejects what would blank the
//! dashboard, and lets everything else through.
//!
//! The UI's TypeScript types are generated from these structs by ts-rs
//! (`#[derive(TS)]`; `#[ts(export)]` on each type the UI fetches, whose
//! dependencies follow) into `hunter/ui-svelte/src/lib/generated/`.
//! A serde attribute that changes presence needs its ts-rs twin: ts-rs
//! ignores `skip_serializing_if` without `default`, so "key absent" also
//! carries `#[ts(optional)]`.
//!
//! Conventions:
//! - SQLite INTEGER -> i64, REAL -> f64, TEXT -> String; nullable -> Option.
//! - `enabled` stays i64 (Python serves 0/1 ints, not booleans).
//! - "key absent" (not null) is modeled as Option + `skip_serializing_if`;
//!   "present but possibly null" as plain Option; "absent OR null" as
//!   Option<Option<T>> (None = absent, Some(None) = null).

use crate::domain::{
    BudgetOverride, BugClass, FindingJobKind, FindingStatus, FindingType, ForgeName, HoldReason,
    JobKind, JobState, PrReviewState, ReleaseAction, RepoWorkKind, Severity,
};
use serde::Serialize;
use ts_rs::TS;

/// Budget gate result: allowed or denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum BudgetState {
    Allowed,
    Denied,
}

// ---------------------------------------------------------------------------
// Row types (verbatim table columns; explicit column lists in store.rs —
// never SELECT *: the live DB carries residual columns absent from dev.db).
// ---------------------------------------------------------------------------

/// repos row, all 18 columns (10 base + 6 migration-added last_*_at +
/// the two full re-hunt request columns of migration 020).
/// Served verbatim by /api/repos; /api/summary uses `RepoBrief` instead.
#[derive(Debug, Clone, Serialize)]
pub struct Repo {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub path: String,
    pub forge: ForgeName,
    pub default_branch: String,
    pub last_hunt_sha: Option<String>,
    pub last_hunt_at: Option<i64>,
    pub enabled: i64,
    pub added_at: i64,
    pub last_full_hunt_at: Option<i64>,
    pub last_test_gap_at: Option<i64>,
    pub last_dep_update_at: Option<i64>,
    pub last_refactor_at: Option<i64>,
    pub last_modernization_at: Option<i64>,
    pub last_standards_at: Option<i64>,
    /// A pending operator request for a full re-hunt (migration 020).
    pub full_hunt_requested_at: Option<i64>,
    /// The request a hunt last started to answer (migration 020).
    pub full_hunt_request_attempted: Option<i64>,
}

/// The repo object inside /api/summary: the 10 base columns plus
/// `full_hunt_requested_at`, which the Repos page shows as a queued
/// re-hunt, and its work held for the operator or failing (WORK-MODEL.md
/// §17). The client does NOT require exactly these keys -- `isKeyedList`
/// checks `id` is a number and the components read `url`, `added_at` and
/// `name` off every entry, so extra keys pass and a missing one of those
/// four blanks the page (validate.ts:83-96).
#[derive(Debug, Clone, Serialize, TS)]
pub struct RepoBrief {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub path: String,
    pub forge: String,
    pub default_branch: String,
    pub last_hunt_sha: Option<String>,
    pub last_hunt_at: Option<i64>,
    pub enabled: i64,
    pub added_at: i64,
    pub full_hunt_requested_at: Option<i64>,
    /// The repo's work held for the operator, at most one per kind, oldest
    /// first. Always present, [] when none.
    pub holds: Vec<RepoHoldOut>,
    /// The repo's work that is failing without a hold yet. Always
    /// present, [] when none.
    pub failing: Vec<RepoStreakOut>,
}

impl RepoBrief {
    pub fn new(r: &Repo, work: RepoWorkOut) -> Self {
        Self {
            id: r.id,
            name: r.name.clone(),
            url: r.url.clone(),
            path: r.path.clone(),
            forge: r.forge.to_string(),
            default_branch: r.default_branch.clone(),
            last_hunt_sha: r.last_hunt_sha.clone(),
            last_hunt_at: r.last_hunt_at,
            enabled: r.enabled,
            added_at: r.added_at,
            full_hunt_requested_at: r.full_hunt_requested_at,
            holds: work.holds,
            failing: work.failing,
        }
    }
}

/// One repo of GET /api/repos: the row, plus its held and failing work.
#[derive(Debug, Clone, Serialize)]
pub struct RepoOut {
    #[serde(flatten)]
    pub repo: Repo,
    #[serde(flatten)]
    pub work: RepoWorkOut,
}

/// A repo's work held for the operator and failing without a hold yet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RepoWorkOut {
    pub holds: Vec<RepoHoldOut>,
    pub failing: Vec<RepoStreakOut>,
}

/// One active hold on a repo's work.
#[derive(Debug, Clone, Serialize, TS)]
pub struct RepoHoldOut {
    /// A scan's job kind, or `sync`: then every kind of the repo waits.
    pub kind: RepoWorkKind,
    pub reason: HoldReason,
    /// The failure and its output.
    pub detail: String,
    /// Epoch ms.
    pub held_at: i64,
    /// What `POST /api/hold/release` accepts for it right now: `continue`
    /// only for a kind with a checkpoint, `restart` always.
    pub actions: Vec<ReleaseAction>,
}

/// A repo work item failing without a hold yet: how many identical
/// failures in a row, and the key they share.
#[derive(Debug, Clone, Serialize, TS)]
pub struct RepoStreakOut {
    pub kind: RepoWorkKind,
    pub streak: i64,
    pub last_failure: String,
}

/// findings row, all 35 columns (38 from migrations/001 + `standard_section`
/// from migrations/004, less the four streak columns migration 023 moved to
/// `finding_work`). `missing_tests` is JSON-encoded TEXT served as a plain
/// string — do NOT parse it.
#[derive(Debug, Clone, Serialize, TS)]
pub struct Finding {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: FindingType,
    pub repo_id: i64,
    pub fingerprint: String,
    pub file: Option<String>,
    pub symbol: Option<String>,
    pub line: Option<i64>,
    pub severity: Severity,
    pub confidence: f64,
    pub summary: String,
    pub detail: Option<String>,
    pub status: FindingStatus,
    pub pr_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub bug_class: Option<BugClass>,
    pub evidence_plan: Option<String>,
    pub introduced_by: Option<String>,
    pub rung_achieved: Option<i64>,
    pub verdict_reason: Option<String>,
    pub budget_override: Option<BudgetOverride>,
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

impl Finding {
    /// Computed `category` key (API-CONTRACT.md §4). Every `FindingType`
    /// variant maps to a value, so this always returns `Some`; the key is
    /// dropped only in legacy mode (`unified` != "1", server.rs:357).
    /// The inner `None` — known type whose source column is null — is
    /// serialized as null, matching Python, which emits the key with the
    /// column's value.
    pub fn category(&self) -> Option<Option<String>> {
        match self.kind {
            FindingType::Bug => Some(self.bug_class.map(|b| b.to_string())),
            FindingType::DepUpdate => Some(self.update_type.clone()),
            FindingType::TestGap => Some(Some("coverage".to_owned())),
            FindingType::Refactor => Some(self.smell_type.clone()),
            FindingType::Modernization => Some(self.modernization_class.clone()),
            FindingType::Standards => Some(self.standard_section.clone()),
        }
    }
}

/// One row of GET /api/findings: the finding columns flattened, plus the
/// computed/embedded keys.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct FindingOut {
    #[serde(flatten)]
    pub finding: Finding,
    /// Absent only in legacy mode (`unified` != "1", server.rs:357); null
    /// when the source column is null.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub category: Option<Option<String>>,
    /// Always present, ascending event-id order, [] when empty.
    pub timeline: Vec<Event>,
    /// Present ONLY for status == "`pr_open`" rows that have a `pr_state` row;
    /// value is `pr_state.needs_attention` (string or null).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub needs_attention: Option<Option<String>>,
    /// The finding's work held for the operator (WORK-MODEL.md §9), at
    /// most one per kind, oldest first. Always present, [] when none.
    pub holds: Vec<HoldOut>,
    /// The finding whose engage/harvest job filed this one as a follow-up;
    /// null for everything a hunt or analysis job found. Always present.
    pub follow_up_of: Option<i64>,
    /// Findings filed as follow-ups by jobs working on this one, ascending
    /// id. Always present, [] when none.
    pub follow_ups: Vec<i64>,
}

/// One active hold on a finding's work, as `GET /api/findings` shows it.
#[derive(Debug, Clone, Serialize, TS)]
pub struct HoldOut {
    pub kind: FindingJobKind,
    pub reason: HoldReason,
    /// The worker's report, or the failure and its output.
    pub detail: String,
    /// Epoch ms.
    pub held_at: i64,
    /// What `POST /api/hold/release` accepts for it right now. `continue`
    /// only while the work has a checkpoint to continue.
    pub actions: Vec<ReleaseAction>,
    /// An engage hold's PR has had activity since it was held, which may be
    /// what it waits for. It does not release the hold.
    pub new_activity: bool,
}

/// jobs row (17 columns) + `repo_name` from the JOIN. The two finding_*
/// keys exist ONLY on /`api/summary.current_job` when the job has a
/// finding — absent (not null) everywhere else.
#[derive(Debug, Clone, Serialize, TS)]
pub struct Job {
    pub id: i64,
    pub kind: JobKind,
    pub repo_id: i64,
    pub finding_id: Option<i64>,
    pub state: JobState,
    pub pid: Option<i64>,
    pub session_file: Option<String>,
    pub cap_tokens: Option<i64>,
    pub tokens_new: Option<i64>,
    pub calls: Option<i64>,
    pub exit_code: Option<i64>,
    pub killed_reason: Option<String>,
    pub notes: Option<String>,
    pub model: Option<String>,
    pub usage_delta: Option<f64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub repo_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub finding_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub finding_fingerprint: Option<String>,
}

/// A job as /api/jobs reports it: the row plus what it produced.
///
/// Separate from [`Job`] because "what this job created" is not a column
/// on `jobs` — it is a reverse lookup over `findings.found_by_job`, and
/// only the jobs list needs it. Keeping it out of `Job` leaves the three
/// other job queries selecting exactly the columns they have.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct JobListEntry {
    #[serde(flatten)]
    pub job: Job,
    /// Findings this job brought into existence.
    ///
    /// Non-empty chiefly for the ingesting jobs (hunts, analysis), which
    /// have no `finding_id` of their own, and for engage/harvest jobs that
    /// filed `FOLLOW-UPS.json` — those carry both: the finding they worked
    /// on and the follow-ups it produced. Always present, so a client
    /// never distinguishes "produced nothing" from "not reported".
    pub produced_finding_ids: Vec<i64>,
}

/// events row.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct Event {
    pub id: i64,
    pub at: i64,
    pub kind: String,
    pub message: String,
    pub job_id: Option<i64>,
    pub finding_id: Option<i64>,
    /// The account that caused it through the API; `None` for the
    /// scheduler's own events and for anything older than accounts.
    pub username: Option<String>,
}

/// `pr_state` row, all 16 columns the code creates (the live DB may carry
/// residual columns — explicit column list excludes them).
#[derive(Debug, Clone, Serialize, TS)]
pub struct PrState {
    pub finding_id: i64,
    pub pr_number: Option<i64>,
    pub state: Option<String>,
    pub mergeable: Option<String>,
    pub checks: Option<String>,
    pub head_ref: Option<String>,
    pub last_activity_at: Option<i64>,
    pub last_engaged_activity_at: Option<i64>,
    pub needs_attention: Option<String>,
    pub attention_since: Option<i64>,
    pub attention_fingerprint: Option<String>,
    pub addressed_fingerprint: Option<String>,
    pub head_sha: Option<String>,
    pub addressed_head_sha: Option<String>,
    pub synced_at: Option<i64>,
    pub harvested_at: Option<i64>,
}

/// `scheduler_state` single row (id = 1).
#[derive(Debug, Clone, Serialize, TS)]
pub struct SchedulerState {
    pub id: i64,
    pub state: String,
    pub detail: String,
    pub next_wake_at: Option<i64>,
    pub updated_at: i64,
}

// ---------------------------------------------------------------------------
// Composite responses
// ---------------------------------------------------------------------------

/// GET /api/finding?id=N
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct FindingDetail {
    pub jobs: Vec<Job>,
    pub pr_state: Option<PrState>,
}

/// /`api/summary.next_candidate` -- the job the scheduler would pick next,
/// shown in the UI so a human can see what the budget gate is holding back.
#[derive(Debug, Clone, Serialize, TS)]
pub struct NextCandidate {
    pub kind: JobKind,
    pub id: i64,
    pub label: Option<String>,
    pub is_finding: bool,
    pub is_prioritized: bool,
    pub budget_state: BudgetState,
    pub budget_reason: String,
    /// Epoch ms; always null when allowed.
    pub budget_retry_at: Option<f64>,
}

/// /`api/summary.activity_status` — tagged union on "kind". The client
/// requires `kind` to be a string and the fields the matching arm reads
/// to be present (`hasActivityFields`, validate.ts:68-80); an unknown
/// kind is accepted and falls through to the default arm, which
/// `validate.test.ts` pins with `"hibernating"`. So adding a variant does
/// not break the dashboard -- but `StatusPage`'s branch chain has no
/// fallback, so the new state renders an empty activity card until the
/// UI gains an arm for it. A rename looks the same from the client: the
/// old arm stops firing and the new name falls through to that empty
/// card. Only dropping a field an existing arm reads is louder: the
/// validator rejects the whole summary.
#[derive(Debug, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityStatus {
    Running {
        job: Box<Job>,
    },
    /// The job lane is inside a cycle with no `running` job: picking,
    /// preparing a job (fetch, clone, worktree) or finishing one
    /// (push, PR, ingest).
    Working,
    Error {
        detail: String,
    },
    /// The last cycle was denied because a host its work needs is
    /// unreachable; `detail` names it (`daemon::describe_cycle`).
    Offline {
        detail: String,
    },
    Paused {
        candidate: NextCandidate,
    },
    Ready {
        candidate: NextCandidate,
    },
    Idle,
    WarmingUp,
}

/// GET /api/summary — polled by the UI every 5s and structurally checked
/// by `isSummary` (`hunter/ui-svelte/src/lib/validate.ts`): only the fields
/// a component dereferences without `?.`. A violation there blanks the
/// dashboard; anything else missing or renamed degrades silently, so this
/// shape is held by the Rust tests, not by the client.
#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct Summary {
    pub backend_status_html: String,
    /// All `FindingStatus` keys, zero-filled.
    pub counts: std::collections::BTreeMap<String, i64>,
    /// Only observed types; may be {}.
    pub type_counts: std::collections::BTreeMap<String, i64>,
    pub repos: Vec<RepoBrief>,
    pub last_cycle: Option<Event>,
    /// Whether the job lane is inside a cycle.
    pub cycle_running: bool,
    pub scheduler_paused: bool,
    pub scheduler_overdrive: bool,
    pub current_job: Option<Job>,
    pub next_candidate: Option<NextCandidate>,
    pub scheduler_state: Option<SchedulerState>,
    pub activity_status: ActivityStatus,
    /// The admin lane's duties running now, beside the job lane and each
    /// other, by task; empty between their runs.
    pub admin_activity: Vec<AdminActivity>,
    /// The job lane's current phase outside its worker -- waiting for a
    /// clone an admin duty holds, cloning, fetching, the tree, publishing,
    /// ingesting; `null` otherwise.
    pub job_activity: Option<JobActivity>,
}

/// One duty of the admin lane (`crate::daemon`), which runs beside the
/// job lane and the other duties, each on its own timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AdminTask {
    /// Reading every open PR from its forge (`scheduler::sync_prs`).
    PrSync,
    /// Reading every open PR review's PR from its forge and posting the
    /// approved rounds (`scheduler::sync_pr_reviews`,
    /// `scheduler::post_pr_reviews`).
    PrReviews,
    /// Removing the clones of deleted repos (`server::reap_deleted_repos`).
    Reap,
    /// Fetching enabled repos' clones ahead of their jobs
    /// (`scheduler::fetch_ahead`).
    FetchAhead,
    /// The zero-token Renovate dependency scan of every repo whose
    /// `dep_update` is due (`scheduler::dependency_scans`).
    DependencyScan,
    /// Releasing finished chains' trees (`workspace::sweep`).
    Sweep,
}

/// One entry of /`api/summary.admin_activity`: an admin duty running now,
/// since when (epoch ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
pub struct AdminActivity {
    pub task: AdminTask,
    pub started_at: i64,
}

/// A phase of the job lane beside its main worker, reported by the helpers
/// that do it (`crate::activity::job_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum JobPhase {
    /// Waiting for a repo's clone that something else holds (an admin
    /// duty, such as fetch-ahead, a dependency scan or the sweep, or a
    /// handler's repo URL update): reported only while the wait lasts.
    WaitingForClone,
    /// Cloning a repo for its first job.
    Cloning,
    /// `git fetch` into a repo's clone.
    Fetching,
    /// Adding the job's worktree.
    PreparingTree,
    /// A fresh reviewer reading a finished fix before it ships
    /// (`scheduler::review_fix`).
    Reviewing,
    /// Pushing and opening or answering a PR, or withdrawing one.
    Publishing,
    /// Filing a worker's findings.
    Ingesting,
}

/// /`api/summary.job_activity`: the job lane's current phase, since when
/// (epoch ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
pub struct JobActivity {
    pub phase: JobPhase,
    pub started_at: i64,
}

// ---------------------------------------------------------------------------
// Stats (GET /api/stats) — SQL aggregate null semantics preserved:
// SUM over zero rows is null, COUNT is 0.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, TS)]
pub struct StatsTotals {
    pub jobs: i64,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub total_usage_delta: Option<f64>,
    pub done: Option<i64>,
    pub denied: Option<i64>,
}

#[derive(Debug, Serialize, TS)]
pub struct StatsByKind {
    pub kind: JobKind,
    pub jobs: i64,
    pub done: Option<i64>,
    pub failed: Option<i64>,
    pub killed: Option<i64>,
    pub denied: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub avg_tokens: Option<f64>,
    pub total_usage_delta: Option<f64>,
    /// Comma-joined distinct model strings (`GROUP_CONCAT`), e.g. "opus,sonnet".
    pub models: Option<String>,
}

#[derive(Debug, Serialize, TS)]
pub struct StatsByFinding {
    pub finding_id: i64,
    pub fingerprint: String,
    pub status: String,
    pub severity: String,
    pub jobs: i64,
    pub total_tokens: Option<i64>,
    pub total_calls: Option<i64>,
    pub total_usage_delta: Option<f64>,
}

#[derive(Debug, Serialize, TS)]
#[ts(export)]
pub struct Stats {
    pub totals: StatsTotals,
    pub by_kind: Vec<StatsByKind>,
    pub by_finding: Vec<StatsByFinding>,
}

/// How far back GET /api/stats looks (`?horizon=`): jobs started within
/// the last 24 hours, 7, 30 or 90 days, or every job ever (`all`, also
/// what an absent or empty parameter means).
#[derive(Debug, Clone, Copy, PartialEq, Eq, TS)]
#[ts(export)]
pub enum StatsHorizon {
    #[ts(rename = "24h")]
    Day,
    #[ts(rename = "7d")]
    Week,
    #[ts(rename = "30d")]
    Month,
    #[ts(rename = "90d")]
    Quarter,
    #[ts(rename = "all")]
    All,
}

impl StatsHorizon {
    pub const ALL: [Self; 5] = [Self::Day, Self::Week, Self::Month, Self::Quarter, Self::All];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
            Self::Quarter => "90d",
            Self::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|h| h.as_str() == s)
    }

    /// The epoch-ms cutoff for a window ending at `now_ms`: jobs dated at
    /// or after it are in. None for `all`, which has no cutoff.
    pub fn since(self, now_ms: i64) -> Option<i64> {
        const DAY_MS: i64 = 24 * 60 * 60 * 1000;
        let days = match self {
            Self::Day => 1,
            Self::Week => 7,
            Self::Month => 30,
            Self::Quarter => 90,
            Self::All => return None,
        };
        Some(now_ms - days * DAY_MS)
    }
}

// ---------------------------------------------------------------------------
// PR reviews (migration 035; future.md §8.8)
// ---------------------------------------------------------------------------

/// One issue a review round reported, as the reviewer wrote it, with what
/// later rounds and the PR's thread made of it.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the issue's independent flags, as the UI reads them"
)]
pub struct PrReviewIssueOut {
    pub id: i64,
    pub kind: String,
    pub blocking: bool,
    pub severity: Option<String>,
    pub confidence: Option<f64>,
    pub file: Option<String>,
    pub line: Option<i64>,
    /// `"RIGHT"` when `line` numbers the PR's head, `"LEFT"` the base (code
    /// the PR removes); null when the reviewer did not say, read as RIGHT.
    pub side: Option<String>,
    pub summary: String,
    pub detail: String,
    /// Whether the issue went out as an inline comment on the PR's diff
    /// (rather than in the posted review's body).
    pub posted_inline: bool,
    /// `open`, `addressed` (a later round found the new commits fix it),
    /// `rebutted` (a later round accepted a reply's rebuttal) or `reopened`
    /// (a human reopened the thread hunter had resolved, and no round
    /// judged it since).
    pub status: String,
    /// Whether hunter answered and resolved the issue's thread on the PR.
    pub resolved_on_forge: bool,
    /// The newest round's note on this issue, when a round judged it.
    pub verdict_note: Option<String>,
    /// The replies in the issue's thread as the newest read saw them,
    /// screened to the authors a review trusts; hunter's own flagged `own`.
    pub replies: Vec<PrReviewReplyOut>,
    /// Replies the screen dropped: counted, never shown.
    pub hidden_replies: i64,
    /// Whether its thread holds news no round judged yet: a reply newer
    /// than any its newest verdict was shown, or a reopen. A watched
    /// review's next round judges it once the news is quiet.
    pub awaits_judgment: bool,
}

/// One reply in the thread of a posted issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, TS)]
#[ts(export)]
pub struct PrReviewReplyOut {
    /// The author's login; `hunter` for one of hunter's own replies.
    pub author: String,
    pub body: String,
    /// Whether it is hunter's own reply (it carried the own-comment mark
    /// and came from hunter's login or a pusher). The only way to tell
    /// one apart: a person's login may be `hunter` too.
    pub own: bool,
    /// When it was written (epoch ms, as the forge dates it); 0 for a
    /// reply read before reply times were kept.
    #[serde(default)]
    pub at: i64,
}

/// One review attempt of the range `base_sha..head_sha`.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct PrReviewRoundOut {
    pub id: i64,
    pub job_id: Option<i64>,
    pub base_sha: String,
    /// `base_sha` too for a round that reviewed no new commits: one that
    /// ran for replies and reopens alone.
    pub head_sha: String,
    /// `ok` (the reviewer finished the range, which counts as reviewed),
    /// `partial` (it was stopped partway: its issues and verdicts count,
    /// but the range is reviewed again) or `failed` (nothing counts).
    pub verdict: String,
    /// Why the round failed, or stopped partway.
    pub failure: Option<String>,
    pub finished_at: i64,
    /// Null when there is nothing to post (failed, or no issues and no
    /// verdict that changes what a thread shows); otherwise `pending`
    /// (awaiting a user), `approved` (posted on the PR review duty's next
    /// run), `posted` or `discarded`.
    pub post_state: Option<String>,
    /// The newest failed post's reason; kept after the round fell back to
    /// `pending`, cleared once it is approved again.
    pub post_error: Option<String>,
    pub posted_at: Option<i64>,
    /// Part of the round's post is, or may be, on the PR already (a post
    /// sent some of it and failed): it can only be approved again, never
    /// discarded.
    pub landed: bool,
    /// Earlier issues this round judged addressed or rebutted whose inline
    /// thread its post answers and resolves, unless a newer judgement of
    /// the issue stands by then.
    pub resolutions: i64,
    /// Earlier issues this round judged still open whose thread waits for
    /// an answer: somebody replied, or hunter's last answer there settled
    /// the issue. Its post answers each one ("Still open", "Open again")
    /// and unresolves a thread hunter had resolved.
    pub answers: i64,
    pub issues: Vec<PrReviewIssueOut>,
}

/// GET /api/pr-reviews: one requested review, rounds newest first.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the review's independent flags, as the UI reads them"
)]
pub struct PrReviewOut {
    pub id: i64,
    pub repo_id: i64,
    pub repo_name: String,
    pub pr_number: i64,
    pub pr_url: String,
    pub title: String,
    pub author: String,
    pub cross_repo: bool,
    pub watch: bool,
    /// Whether a new round with issues is posted without waiting for a
    /// user's approval.
    pub auto_post: bool,
    pub state: PrReviewState,
    pub head_sha: Option<String>,
    pub last_reviewed_sha: Option<String>,
    pub vouched_author: Option<String>,
    /// The owner of the fork the PR's head lives in, as the newest read
    /// reported it: a round runs their repository's code. Null for a head
    /// in the repo itself; empty when the forge names no owner.
    pub head_owner: Option<String>,
    /// The fork's owner, when they are not the author, cannot push and a
    /// user vouched for running code from their fork.
    pub vouched_head_owner: Option<String>,
    pub requested_by: String,
    pub requested_at: i64,
    pub failures: i64,
    pub last_failure: Option<String>,
    pub held: bool,
    /// Whether the repo's URL changed since the review was requested: the
    /// review does nothing until a request binds it to the new URL,
    /// deciding about the author afresh.
    pub repo_moved: bool,
    /// Whether the fork the head lives in changed hands since the request
    /// decided about its owner (`head_owner` is somebody else): the review
    /// runs no round until a request decides about the new owner.
    pub fork_changed_hands: bool,
    pub running_job_id: Option<i64>,
    /// When the newest news in hunter's threads that no round judged yet
    /// arrived (epoch ms): a trusted reply newer than any its issue's
    /// newest verdict was shown, or a reopen; null when there is none
    /// ([`PrReviewIssueOut::awaits_judgment`] marks the issues).
    pub awaiting_since: Option<i64>,
    /// When the review's next round is due (epoch ms;
    /// [`crate::store::PrReviewRow::due_at`]): what the scheduler picks by,
    /// so a review shows as waiting for a round exactly when it is. Null
    /// while nothing makes one due: the review is not open, held, its repo
    /// moved or its fork changed hands, or -- unwatched -- no request waits
    /// for a round.
    pub due_at: Option<i64>,
    pub rounds: Vec<PrReviewRoundOut>,
}

/// GET /api/repo/open-prs: one open PR of a repo, with whether hunter
/// opened it (not reviewable) and the review already requested for it.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct OpenPrOut {
    pub number: i64,
    pub title: String,
    pub author: String,
    pub is_draft: bool,
    pub cross_repo: bool,
    pub url: String,
    pub hunter_own: bool,
    pub review_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Worker result (types.py RunResult; consumed by scheduler _record_job)
// ---------------------------------------------------------------------------

/// Outcome of one worker run (harness.py `run_worker`).
#[derive(Debug, Clone)]
pub struct RunResult {
    pub exit_code: Option<i32>,
    /// None | "cap" | "wallclock" | "unmetered" | "resume-unavailable";
    /// what each maps to is `scheduler::job_state`.
    pub killed_reason: Option<String>,
    pub tokens_new: i64,
    pub calls: i64,
    pub session_file: Option<String>,
    pub duration_s: f64,
    pub stdout_tail: String,
    pub usage_delta: Option<f64>,
}
