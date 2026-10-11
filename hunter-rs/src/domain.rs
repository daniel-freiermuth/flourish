//! Strongly-typed domain enums — the DB stores these as TEXT but Rust
//! code uses typed variants. sqlx maps them transparently via Type
//! derive; serde serializes as the lowercase string for JSON API parity.
//!
//! Adding a variant here is a compile error at every match site — the
//! exhaustive-match guarantee that string comparisons can never provide.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

// ---------------------------------------------------------------------------
// Finding status
// ---------------------------------------------------------------------------

/// Finding lifecycle status. The DB column is TEXT; sqlx maps via
/// `rename_all` = "`snake_case`".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingStatus {
    New,
    Rechecking,
    Queued,
    Fixing,
    PrOpen,
    Merged,
    /// The PR was closed without merging and has not been harvested yet.
    /// Deliberately not suppressed: a closure says nothing on its own about
    /// whether the finding was wrong. Of the first 16 closed PRs, 15 were
    /// withdrawn by engage (14 of those as superseded or obsolete) and only
    /// one was closed by a human, so treating every closure as a rejection
    /// silenced mostly valid findings. The closed-PR harvest decides.
    Closed,
    /// The finding was valid, but its work landed another way (another
    /// commit or PR), duplicated one, or its code is gone. Not suppressed:
    /// the same defect showing up again elsewhere is still worth reporting.
    Superseded,
    Rejected,
    Wontfix,
    Note,
}

impl FindingStatus {
    pub const ALL: [Self; 11] = [
        Self::New,
        Self::Rechecking,
        Self::Queued,
        Self::Fixing,
        Self::PrOpen,
        Self::Merged,
        Self::Closed,
        Self::Superseded,
        Self::Rejected,
        Self::Wontfix,
        Self::Note,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Rechecking => "rechecking",
            Self::Queued => "queued",
            Self::Fixing => "fixing",
            Self::PrOpen => "pr_open",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::Superseded => "superseded",
            Self::Rejected => "rejected",
            Self::Wontfix => "wontfix",
            Self::Note => "note",
        }
    }

    /// Statuses the verdict endpoint accepts. Not `merged`: only the PR sync
    /// sets that, from the forge's state of the finding's PR.
    pub fn is_verdict(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Rejected | Self::Wontfix | Self::Note
        )
    }

    /// Statuses at which a finding asks the operator for a verdict: `new`
    /// (triage), `note` (act on it after all) and `closed` (classify a
    /// closed PR; a harvest still running keeps the operator's verdict,
    /// see `Store::record_closed_harvest`). Everywhere else a job or the
    /// forge owns the finding, and a verdict would race it, so
    /// `/api/verdict` refuses. Work held for the operator is released
    /// through `/api/hold/release`, not by a verdict.
    pub fn awaits_verdict(self) -> bool {
        matches!(self, Self::New | Self::Note | Self::Closed)
    }

    /// Statuses that require a reason when set via verdict.
    pub fn reason_required(self) -> bool {
        matches!(self, Self::Rejected | Self::Wontfix)
    }

    /// Suppressed statuses (feed the suppression corpus). `Closed` and
    /// `Superseded` are left out on purpose: see their variants.
    pub fn is_suppressed(self) -> bool {
        matches!(self, Self::Rejected | Self::Wontfix)
    }
}

impl std::fmt::Display for FindingStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Budget override
// ---------------------------------------------------------------------------

/// A finding's budget override (`findings.budget_override`, TEXT; NULL is
/// "no override"). Either mode runs the finding under the backend's
/// prioritized outlook; `Once` is cleared after the next attempt, `Exempt`
/// stays until cleared by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum BudgetOverride {
    Once,
    Exempt,
}

impl BudgetOverride {
    pub const ALL: [Self; 2] = [Self::Once, Self::Exempt];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Exempt => "exempt",
        }
    }
}

impl std::fmt::Display for BudgetOverride {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BudgetOverride {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| format!("unknown budget override {s:?}"))
    }
}

// ---------------------------------------------------------------------------
// Why a pull request closed without merging
// ---------------------------------------------------------------------------

/// Why a finding's work ended without a merge: the closed-PR harvest's
/// verdict on a PR closed unmerged (`CLOSE-REASON.json`'s
/// `classification`), and a fix worker's on a finding it declined (the
/// first line of `NOT-A-BUG.md` / `DECLINED.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClosureClass {
    Superseded,
    Duplicate,
    Obsolete,
    Wrong,
    Unwanted,
    Abandoned,
}

impl ClosureClass {
    pub const ALL: [Self; 6] = [
        Self::Superseded,
        Self::Duplicate,
        Self::Obsolete,
        Self::Wrong,
        Self::Unwanted,
        Self::Abandoned,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::Duplicate => "duplicate",
            Self::Obsolete => "obsolete",
            Self::Wrong => "wrong",
            Self::Unwanted => "unwanted",
            Self::Abandoned => "abandoned",
        }
    }

    /// The status the finding takes once its PR's closure is classified.
    ///
    /// Only `wrong` and `unwanted` suppress, because only they say
    /// something about the finding: its premise failed, or the maintainers
    /// do not want that kind of change. Work that landed another way, or
    /// whose code is gone, says the finding was right. `abandoned` says
    /// nothing at all, so the finding goes back to triage.
    pub fn status(self) -> FindingStatus {
        match self {
            Self::Superseded | Self::Duplicate | Self::Obsolete => FindingStatus::Superseded,
            Self::Wrong => FindingStatus::Rejected,
            Self::Unwanted => FindingStatus::Wontfix,
            Self::Abandoned => FindingStatus::New,
        }
    }
}

impl std::fmt::Display for ClosureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ClosureClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| format!("unknown classification {s:?}"))
    }
}

// ---------------------------------------------------------------------------
// Holds: work stopped for the operator (WORK-MODEL.md §9)
// ---------------------------------------------------------------------------

/// Why a work item is held (`work_holds.reason`). The resume prompt after a
/// `continue` is chosen from this, never from the detail's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum HoldReason {
    /// The worker reported a missing prerequisite (`BLOCKED.md`).
    Prerequisite,
    /// The same failure `MAX_CONSECUTIVE_SAME_FAILURE` times in a row.
    Stuck,
    /// The give-up ceiling ended the chain, and that completed the streak.
    GaveUp,
}

impl HoldReason {
    pub const ALL: [Self; 3] = [Self::Prerequisite, Self::Stuck, Self::GaveUp];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prerequisite => "prerequisite",
            Self::Stuck => "stuck",
            Self::GaveUp => "gave_up",
        }
    }
}

impl std::fmt::Display for HoldReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an operator ends a hold (`work_holds.release_action`, §9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum ReleaseAction {
    /// Resume from the checkpoint; the ceiling counts from now.
    Continue,
    /// Drop the checkpoint and start the work fresh.
    Restart,
    /// Reject the finding (a verdict, with a reason).
    Reject,
    /// Mark the finding won't-fix (a verdict, with a reason).
    Wontfix,
    /// Classify a closed PR by hand, as its harvest would have.
    Classify,
    /// Mark a merged PR harvested without a review.
    MarkHarvested,
    /// Mark the PR's current attention handled, as a no-op engage would.
    Dismiss,
}

impl ReleaseAction {
    pub const ALL: [Self; 7] = [
        Self::Continue,
        Self::Restart,
        Self::Reject,
        Self::Wontfix,
        Self::Classify,
        Self::MarkHarvested,
        Self::Dismiss,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Restart => "restart",
            Self::Reject => "reject",
            Self::Wontfix => "wontfix",
            Self::Classify => "classify",
            Self::MarkHarvested => "mark_harvested",
            Self::Dismiss => "dismiss",
        }
    }

    /// Whether this action can release a hold of `kind`'s work (§9.3). A
    /// harvest's `classify` and `mark_harvested` also depend on how its PR
    /// ended, which only the finding knows: see `Store::release_hold`.
    pub fn applies_to(self, kind: FindingJobKind) -> bool {
        match self {
            Self::Continue | Self::Restart => true,
            Self::Reject | Self::Wontfix => {
                matches!(kind, FindingJobKind::Recheck | FindingJobKind::Fix)
            }
            Self::Classify | Self::MarkHarvested => kind == FindingJobKind::Harvest,
            Self::Dismiss => kind == FindingJobKind::Engage,
        }
    }
}

impl std::fmt::Display for ReleaseAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ReleaseAction {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|a| a.as_str() == s)
            .ok_or_else(|| format!("unknown release action {s:?}"))
    }
}

// ---------------------------------------------------------------------------
// Job state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Done,
    Failed,
    Killed,
    /// Paused, not finished: the attempt stopped with its transcript
    /// intact, so the work can be continued in a new job rather than
    /// started over. Either the worker hit the token bound, or it died
    /// after doing metered work (a provider error, a dead connection, the
    /// daemon going down under it), or a wallclock kill after doing
    /// metered work. Only how the attempt ended: whether the work
    /// continues from it is the work item's checkpoint pointer
    /// (`finding_work.checkpoint_job_id`) for a finding's work, and for
    /// repo-level work whether nothing continues it yet. Resume chains are
    /// bounded by the give-up ceiling. See `scheduler::job_state`.
    Suspended,
    /// Never written by this daemon: the budget gate refuses before a job
    /// row exists. Kept so the Python daemon's `denied` rows still decode,
    /// and the stats still count them.
    Denied,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Suspended => "suspended",
            Self::Denied => "denied",
        }
    }
}

impl std::fmt::Display for JobState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Severity (replaces the existing types::Severity with sqlx + serde)
// ---------------------------------------------------------------------------

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    sqlx::Type,
    TS,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum Severity {
    Low,
    /// What ingest assumes when a worker names none.
    #[default]
    Medium,
    High,
}

impl Severity {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }

    pub fn rank(self) -> i64 {
        match self {
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Finding type (bug, dep_update, test_gap, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum FindingType {
    Bug,
    DepUpdate,
    TestGap,
    Refactor,
    Modernization,
    Standards,
}

impl FindingType {
    pub const ALL: [Self; 6] = [
        Self::Bug,
        Self::DepUpdate,
        Self::TestGap,
        Self::Refactor,
        Self::Modernization,
        Self::Standards,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::DepUpdate => "dep_update",
            Self::TestGap => "test_gap",
            Self::Refactor => "refactor",
            Self::Modernization => "modernization",
            Self::Standards => "standards",
        }
    }
}

impl std::fmt::Display for FindingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FindingType {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bug" => Ok(Self::Bug),
            "dep_update" => Ok(Self::DepUpdate),
            "test_gap" => Ok(Self::TestGap),
            "refactor" => Ok(Self::Refactor),
            "modernization" => Ok(Self::Modernization),
            "standards" => Ok(Self::Standards),
            _ => Err(format!("unknown finding type: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Repo job kinds (hunt + analysis scans) — Rust-only type narrowing;
// DB/JSON traffic keeps using the flat `JobKind` enum below.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TS)]
#[ts(rename_all = "snake_case")]
pub enum RepoJobKind {
    Hunt,
    TestGap,
    DepUpdate,
    Refactor,
    Modernization,
    Standards,
}

impl RepoJobKind {
    pub const ALL: [Self; 6] = [
        Self::Hunt,
        Self::TestGap,
        Self::DepUpdate,
        Self::Refactor,
        Self::Modernization,
        Self::Standards,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hunt => "hunt",
            Self::TestGap => "test_gap",
            Self::DepUpdate => "dep_update",
            Self::Refactor => "refactor",
            Self::Modernization => "modernization",
            Self::Standards => "standards",
        }
    }
}
impl std::fmt::Display for RepoJobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RepoJobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "hunt" => Ok(Self::Hunt),
            "test_gap" => Ok(Self::TestGap),
            "dep_update" => Ok(Self::DepUpdate),
            "refactor" => Ok(Self::Refactor),
            "modernization" => Ok(Self::Modernization),
            "standards" => Ok(Self::Standards),
            _ => Err(format!("unknown repo job kind: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Finding job kinds (fix + PR lifecycle).
// ---------------------------------------------------------------------------

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, sqlx::Type, TS,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum FindingJobKind {
    Fix,
    Engage,
    Harvest,
    Recheck,
}

impl FindingJobKind {
    pub const ALL: [Self; 4] = [Self::Fix, Self::Engage, Self::Harvest, Self::Recheck];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fix => "fix",
            Self::Engage => "engage",
            Self::Harvest => "harvest",
            Self::Recheck => "recheck",
        }
    }
}

impl std::fmt::Display for FindingJobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for FindingJobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "fix" => Ok(Self::Fix),
            "engage" => Ok(Self::Engage),
            "harvest" => Ok(Self::Harvest),
            "recheck" => Ok(Self::Recheck),
            _ => Err(format!("unknown finding job kind: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Job kind — composed of the two sub-enums. Serializes to flat strings
// ("hunt", "fix", etc.) for DB/JSON compatibility. All parsing and
// serialization delegates to the sub-enums — adding a variant there
// automatically extends JobKind with zero changes here.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, TS)]
#[ts(untagged)]
pub enum JobKind {
    Repo(RepoJobKind),
    Finding(FindingJobKind),
}

impl JobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Repo(r) => r.as_str(),
            Self::Finding(f) => f.as_str(),
        }
    }

    pub fn is_repo(self) -> bool {
        matches!(self, Self::Repo(_))
    }

    pub fn is_finding(self) -> bool {
        matches!(self, Self::Finding(_))
    }

    pub fn as_repo(self) -> Option<RepoJobKind> {
        match self {
            Self::Repo(r) => Some(r),
            Self::Finding(_) => None,
        }
    }

    pub fn as_finding(self) -> Option<FindingJobKind> {
        match self {
            Self::Finding(f) => Some(f),
            Self::Repo(_) => None,
        }
    }
}

impl From<RepoJobKind> for JobKind {
    fn from(k: RepoJobKind) -> Self {
        Self::Repo(k)
    }
}

impl From<FindingJobKind> for JobKind {
    fn from(k: FindingJobKind) -> Self {
        Self::Finding(k)
    }
}

impl std::fmt::Display for JobKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for JobKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<RepoJobKind>()
            .map(Self::Repo)
            .or_else(|_| s.parse::<FindingJobKind>().map(Self::Finding))
            .map_err(|_| format!("unknown job kind: {s:?}"))
    }
}

impl Serialize for JobKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for JobKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

impl sqlx::Type<sqlx::Sqlite> for JobKind {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <str as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <str as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

impl sqlx::Encode<'_, sqlx::Sqlite> for JobKind {
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        self.as_str().encode_by_ref(buf)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for JobKind {
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        let s = <&str as sqlx::Decode<'r, sqlx::Sqlite>>::decode(value)?;
        s.parse()
            .map_err(|e: String| sqlx::error::BoxDynError::from(e))
    }
}

// ---------------------------------------------------------------------------
// Bug class (kebab-case: boundary, error-path, race, …)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type, TS)]
pub enum BugClass {
    #[serde(rename = "boundary")]
    #[sqlx(rename = "boundary")]
    Boundary,
    #[serde(rename = "error-path")]
    #[sqlx(rename = "error-path")]
    ErrorPath,
    #[serde(rename = "race")]
    #[sqlx(rename = "race")]
    Race,
    #[serde(rename = "contract-drift")]
    #[sqlx(rename = "contract-drift")]
    ContractDrift,
    #[serde(rename = "leak")]
    #[sqlx(rename = "leak")]
    Leak,
    #[serde(rename = "logic")]
    #[sqlx(rename = "logic")]
    Logic,
}

impl BugClass {
    pub const ALL: [Self; 6] = [
        Self::Boundary,
        Self::ErrorPath,
        Self::Race,
        Self::ContractDrift,
        Self::Leak,
        Self::Logic,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boundary => "boundary",
            Self::ErrorPath => "error-path",
            Self::Race => "race",
            Self::ContractDrift => "contract-drift",
            Self::Leak => "leak",
            Self::Logic => "logic",
        }
    }
}

impl std::fmt::Display for BugClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for BugClass {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "boundary" => Ok(Self::Boundary),
            "error-path" => Ok(Self::ErrorPath),
            "race" => Ok(Self::Race),
            "contract-drift" => Ok(Self::ContractDrift),
            "leak" => Ok(Self::Leak),
            "logic" => Ok(Self::Logic),
            _ => Err(format!("unknown bug class: {s:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Forge name (github, gitlab)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(rename_all = "lowercase")]
pub enum ForgeName {
    Github,
    Gitlab,
}

impl ForgeName {
    pub const ALL: [Self; 2] = [Self::Github, Self::Gitlab];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }
}

impl std::fmt::Display for ForgeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ForgeName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "github" => Ok(Self::Github),
            "gitlab" => Ok(Self::Gitlab),
            _ => Err(format!("unknown forge name: {s:?}")),
        }
    }
}
