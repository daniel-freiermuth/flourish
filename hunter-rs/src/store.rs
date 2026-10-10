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
//!   cannot go stale before the write), `sync_pr_open` (the PR upsert and
//!   the dependent `attention_since` update), `create_job` (the insert
//!   and the retirement of the suspension it supersedes, with that
//!   retirement's event), and `record_closed_harvest` (a closed PR's
//!   classification and its harvest stamp). Those transactions close real
//!   races; do not unwind them to make a method look like the others.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};

use crate::auth::CurrentUser;
use crate::domain::{
    BudgetOverride, BugClass, FindingJobKind, FindingStatus, FindingType, ForgeName, JobKind,
    JobState, RepoJobKind, Severity,
};
use crate::types::{
    Event, Finding, Job, JobListEntry, PrState, Repo, SchedulerState, StatsByFinding, StatsByKind,
    StatsTotals,
};
use crate::util::now_ms;

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

/// A job row just inserted, and the suspensions its insert retired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedJob {
    pub id: i64,
    /// Ids of the suspended jobs this fresh job superseded, oldest first.
    pub superseded: Vec<i64>,
}

/// Whether a chain's working tree is still needed; see
/// [`Store::chain_status`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStatus {
    /// Rows found in the chain. 0 means no job owns the workspace at all.
    pub attempts: i64,
    /// Attempts that are `running`, or `suspended` with nothing continuing
    /// them yet. Nonzero means the tree is in use or will be resumed, and
    /// must not be touched.
    pub live: i64,
    /// When the chain's latest attempt finished, if any has.
    pub last_finished_at: Option<i64>,
    /// The clone the chain's tree was added from, if its repo still exists.
    pub clone: Option<String>,
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

/// Everything [`Store::complete_job`] writes when a job finishes.
///
/// Named fields rather than positional arguments: the outcome carries
/// four optional strings and three integers whose order the compiler
/// cannot check, and a swapped `tokens_new`/`calls` corrupts the ledger
/// the per-kind token budget is estimated from.
#[derive(Debug, Clone, Copy)]
pub struct JobOutcome<'a> {
    pub state: JobState,
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

/// All required fields for syncing an open PR's state.  The only
/// conditional parts — `attention_since` and `clear_addressed` — are
/// handled by two extra compile-time checked queries inside
/// `sync_pr_open`, not dynamic SQL.
#[derive(Debug)]
pub struct SyncPrData {
    pub pr_number: i64,
    pub state: String,
    pub mergeable: String,
    pub checks: Option<String>,
    pub head_ref: String,
    pub head_sha: String,
    pub last_activity_at: i64,
    pub last_engaged_activity_at: i64,
    pub needs_attention: Option<String>,
    pub attention_fingerprint: Option<String>,
    pub synced_at: i64,
    /// None = don't touch; Some(None) = set NULL; Some(Some(v)) = set to v.
    pub attention_since: Option<Option<i64>>,
    /// When true, set `addressed_fingerprint` = NULL, `addressed_head_sha` = NULL.
    pub clear_addressed: bool,
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

/// Typed update for finding analysis fields after worker recheck.
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

/// Move one job to a terminal `state` on `ex`; see
/// [`Store::retire_suspended_job`] for why `killed_reason` is optional.
async fn retire_job<'e, E: sqlx::SqliteExecutor<'e>>(
    ex: E,
    job_id: i64,
    state: JobState,
    killed_reason: Option<&str>,
    notes: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE jobs SET state = ?1, killed_reason = COALESCE(?2, killed_reason), \
         notes = ?3 WHERE id = ?4",
        state,
        killed_reason,
        notes,
        job_id
    )
    .execute(ex)
    .await?;
    Ok(())
}

impl Store {
    // Private — all SQL goes through typed store methods.
    // No code outside this module can access the raw pool (field is not pub).

    // ── counting helpers (daemon sleep logic) ─────────────────────────

    pub async fn count_queued(&self) -> sqlx::Result<i64> {
        let status = FindingStatus::Queued;
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM findings WHERE status = ?1"#,
            status
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

    /// Clear `last_hunt_sha` (triggers a full re-hunt on next cycle).
    pub async fn clear_last_hunt_sha(&self, repo_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE repos SET last_hunt_sha = NULL WHERE id = ?1",
            repo_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

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
        Ok(Self { pool })
    }

    /// Open the live DB read-write. Runs embedded migrations on connect —
    /// the binary carries its own schema, so deploying a new binary
    /// automatically migrates the DB.
    pub async fn connect(db_path: &Path) -> anyhow::Result<Self> {
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
        Ok(Self { pool })
    }

    // -- writes (API-CONTRACT-WRITES.md; commit-per-method) -------------------

    /// SELECT all 39 columns FROM findings WHERE id = ? (embedded row in
    /// verdict/recheck/unqueue/override success bodies).
    pub async fn get_finding(&self, id: i64) -> sqlx::Result<Option<Finding>> {
        sqlx::query_as!(
            Finding,
            r#"
            SELECT id, type AS "kind: FindingType", repo_id, fingerprint, file, symbol, line,
                   severity AS "severity: Severity", confidence, summary, detail, status AS "status: FindingStatus", pr_url,
                   created_at, updated_at, bug_class AS "bug_class: BugClass", evidence_plan,
                   introduced_by, rung_achieved, verdict_reason,
                   budget_override AS "budget_override: BudgetOverride", fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
                   current_version, latest_version, update_type,
                   security_advisory, missing_tests, test_file, smell_type,
                   suggested_refactor, modernization_class, current_approach,
                   proposed_approach, standard_section
            FROM findings
            WHERE id = ?1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// Plain finding status transition (new, queued, rechecking, merged, fixing).
    pub async fn set_finding_status(
        &self,
        finding_id: i64,
        status: FindingStatus,
    ) -> sqlx::Result<()> {
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

    /// Status transition with a verdict reason (rejected, wontfix). Any
    /// anchor an earlier verdict left is removed in the same transaction:
    /// it described that verdict, not this one.
    pub async fn set_finding_verdict(
        &self,
        finding_id: i64,
        status: FindingStatus,
        reason: &str,
    ) -> sqlx::Result<()> {
        self.set_anchored_verdict(finding_id, status, reason, None)
            .await
    }

    /// [`Self::set_finding_verdict`] for a worker's verdict, recording what
    /// it was judged against (`anchor`) in the same transaction, or
    /// removing the previous verdict's anchor when there is none.
    pub async fn set_anchored_verdict(
        &self,
        finding_id: i64,
        status: FindingStatus,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
    ) -> sqlx::Result<()> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        tx.commit().await?;
        Ok(())
    }

    /// [`Self::set_finding_verdict`], applied only while the finding is
    /// still at `from` (the status the caller just checked). `false` when
    /// something else moved it in between: the operator's verdict loses to
    /// a job that claimed the finding meanwhile. An operator's verdict has
    /// no tree to be anchored to, so it removes any anchor an earlier one
    /// left.
    pub async fn set_verdict_if(
        &self,
        finding_id: i64,
        from: FindingStatus,
        status: FindingStatus,
        reason: &str,
    ) -> sqlx::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let set = sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
             WHERE id = ?4 AND status = ?5",
            status,
            reason,
            now,
            finding_id,
            from
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if set {
            Self::write_anchor(&mut tx, finding_id, None).await?;
        }
        tx.commit().await?;
        Ok(set)
    }

    /// Replace `finding_id`'s verdict anchor with `anchor`, or remove it.
    async fn write_anchor(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
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

    /// Reopen a suppressed finding that a scan filed again: back to `new`
    /// with the scan's analysis, its verdict and anchor gone. Only from a
    /// suppressed status, so a finding something else moved meanwhile
    /// keeps that. The caller decides whether the verdict had lapsed
    /// ([`crate::suppression::reopen_if_changed`]). `true` when reopened.
    pub async fn reopen_suppressed(
        &self,
        finding_id: i64,
        row: &FindingInsert,
    ) -> sqlx::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let new = FindingStatus::New;
        let s1 = FindingStatus::Rejected;
        let s2 = FindingStatus::Wontfix;
        let symbol = row.symbol.as_deref();
        let detail = row.detail.as_deref();
        let evidence_plan = row.evidence_plan.as_deref();
        let reopened = sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = NULL, symbol = ?2, line = ?3, \
             severity = ?4, confidence = ?5, summary = ?6, detail = ?7, evidence_plan = ?8, \
             updated_at = ?9 \
             WHERE id = ?10 AND status IN (?11, ?12)",
            new,
            symbol,
            row.line,
            row.severity,
            row.confidence,
            row.summary,
            detail,
            evidence_plan,
            now,
            finding_id,
            s1,
            s2
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if reopened {
            Self::write_anchor(&mut tx, finding_id, None).await?;
        }
        tx.commit().await?;
        Ok(reopened)
    }

    /// Retain a fix checkpoint without treating its blocker as a rejection.
    /// Both rows change atomically; a mismatched finding or non-fix job is an
    /// error and leaves them untouched. Spend, transcript, notes, and the
    /// original kill reason survive. The report is stored once, as the job's
    /// `blocker`, which every resume of this chain inherits
    /// ([`Self::create_job`]) and reads ([`Self::job_blocker`]), and the API
    /// shows ([`Self::held_fix_blockers`]).
    ///
    /// An attempt whose worker never ran (`Backend::run` failed) has no
    /// transcript of its own; it takes over its predecessor's, which a
    /// successor row makes unresumable. Otherwise neither link of the chain
    /// would be resumable and the next requeue would start the fix cold.
    pub async fn block_fix_job(
        &self,
        finding_id: i64,
        job_id: i64,
        reason: &str,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        let suspended = JobState::Suspended;
        let fix = JobKind::Finding(FindingJobKind::Fix);
        let blocked = FindingStatus::Blocked;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let job = sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, blocker = ?2, \
             finished_at = COALESCE(finished_at, ?3), \
             session_file = COALESCE(session_file, \
                 (SELECT p.session_file FROM jobs p WHERE p.id = jobs.resumed_from)) \
             WHERE id = ?4 AND finding_id = ?5 AND kind = ?6",
            suspended,
            reason,
            now,
            job_id,
            finding_id,
            fix
        )
        .execute(&mut *tx)
        .await?;
        if job.rows_affected() == 0 {
            return Err(sqlx::Error::RowNotFound);
        }
        let finding = sqlx::query!(
            "UPDATE findings SET status = ?1, updated_at = ?2, \
             fix_attempts = 0, last_fix_failure = NULL WHERE id = ?3",
            blocked,
            now,
            finding_id
        )
        .execute(&mut *tx)
        .await?;
        if finding.rows_affected() == 0 {
            return Err(sqlx::Error::RowNotFound);
        }
        tx.commit().await
    }

    /// Set finding status to `pr_open` with PR URL.
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
                   budget_override AS "budget_override: BudgetOverride", fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
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

    pub async fn get_pr_state(&self, finding_id: i64) -> sqlx::Result<Option<PrState>> {
        sqlx::query_as!(
            PrState,
            r#"
            SELECT finding_id, pr_number, state, mergeable, checks, head_ref,
                   last_activity_at, last_engaged_activity_at, needs_attention,
                   attention_since, attention_fingerprint,
                   addressed_fingerprint, head_sha, addressed_head_sha,
                   synced_at, harvested_at, harvest_attempts,
                   last_harvest_failure
            FROM pr_state
            WHERE finding_id = ?1
            "#,
            finding_id
        )
        .fetch_optional(&self.pool)
        .await
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

    /// The worker report each `blocked` finding's held fix checkpoint is
    /// blocked on (`jobs.blocker`, newest chain link without a successor):
    /// `finding_id` -> report. The job row is the only store of that
    /// report; this is how the API shows it.
    pub async fn held_fix_blockers(&self) -> sqlx::Result<BTreeMap<i64, String>> {
        let blocked = FindingStatus::Blocked;
        let suspended = JobState::Suspended;
        let fix = JobKind::Finding(FindingJobKind::Fix);
        let rows = sqlx::query!(
            r#"
            SELECT j.finding_id AS "finding_id!: i64", j.blocker AS "blocker!: String"
            FROM jobs j
            JOIN findings f ON f.id = j.finding_id
            WHERE f.status = ?1 AND j.state = ?2 AND j.kind = ?3
              AND j.blocker IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
            ORDER BY j.id
            "#,
            blocked,
            suspended,
            fix
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.finding_id, r.blocker))
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

    /// Suspended jobs that could actually be picked up again, newest first.
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
    /// Four filters make "could be picked up again" true rather than
    /// merely claimed:
    /// - the repo must still be live, since a soft-deleted repo's clone
    ///   is being reaped and `create_job` would refuse the successor;
    /// - `session_file` must be present, because resume means handing
    ///   omp that exact session path. A suspended job with no path has
    ///   nothing to continue and would silently become a fresh run —
    ///   the implicit-resume behaviour that once re-cached an unrelated
    ///   290-call transcript for 508,709 tokens on a single call;
    /// - finding jobs require an actionable status for that kind. A blocked
    ///   fix retains its checkpoint but waits for an operator to queue it;
    ///   any other suspension this filter hides is retired by
    ///   [`Self::retire_stranded_suspensions`];
    /// - nothing may already continue it. A successor row IS the record
    ///   that this suspension has been picked up, which is why no
    ///   `resumed` state exists: the link carries the fact, and a state
    ///   flag beside it would be a second copy of the same truth, free
    ///   to disagree. Without this clause one transcript would be
    ///   resumed once per cycle forever, every attempt re-caching the
    ///   same context — the loop this feature exists to end, rebuilt
    ///   one level up.
    pub async fn list_resumable_jobs(&self) -> sqlx::Result<Vec<Job>> {
        let suspended = JobState::Suspended;
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
            WHERE j.state = ?1
              AND r.deleted_at IS NULL
              AND j.session_file IS NOT NULL
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
              AND (j.kind NOT IN (?2, ?4, ?6, ?8) OR EXISTS (
                  SELECT 1 FROM findings f WHERE f.id = j.finding_id AND (
                      (j.kind = ?2 AND f.status = ?3)
                      OR (j.kind = ?4 AND f.status = ?5)
                      OR (j.kind = ?6 AND f.status = ?7)
                      OR (j.kind = ?8 AND f.status IN (?9, ?10))
                  )
              ))
            ORDER BY j.id DESC
            "#,
            suspended,
            fix,
            queued,
            recheck,
            rechecking,
            engage,
            pr_open,
            harvest,
            merged,
            closed
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Retire suspended finding jobs whose finding has moved on, so
    /// [`Self::list_resumable_jobs`] will never offer them again: anything
    /// not at its kind's actionable status, except a fix at `blocked` (held
    /// for the operator) or `fixing`. `fixing` with no successor yet is
    /// `run_fix` between its claim and `create_job`, or a daemon that died
    /// there; startup sweeps before reconciliation requeues the finding, so
    /// retiring it then would throw away the checkpoint being resumed.
    /// Returns the retired ids.
    ///
    /// Without this such a row stayed `suspended` forever — no resume
    /// reaches it, so neither the give-up ceiling nor the `workdir-gone`
    /// check ever runs — and the sweep, which keeps every suspended chain,
    /// held its whole tree on disk (a blocked fix the operator rejected).
    /// The actionable mapping must match [`Self::list_resumable_jobs`];
    /// `every_finding_suspension_is_resumable_held_or_retired` pins the two
    /// together.
    pub async fn retire_stranded_suspensions(&self) -> sqlx::Result<Vec<i64>> {
        let suspended = JobState::Suspended;
        let fix = JobKind::Finding(FindingJobKind::Fix);
        let queued = FindingStatus::Queued;
        let recheck = JobKind::Finding(FindingJobKind::Recheck);
        let rechecking = FindingStatus::Rechecking;
        let engage = JobKind::Finding(FindingJobKind::Engage);
        let pr_open = FindingStatus::PrOpen;
        let harvest = JobKind::Finding(FindingJobKind::Harvest);
        let merged = FindingStatus::Merged;
        let closed = FindingStatus::Closed;
        let blocked = FindingStatus::Blocked;
        let fixing = FindingStatus::Fixing;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let stranded = sqlx::query!(
            r#"
            SELECT j.id AS "id!", j.kind AS "kind!: JobKind", j.finding_id AS "finding_id!",
                   f.status AS "status!: FindingStatus", r.name AS "repo_name!"
            FROM jobs j
            JOIN findings f ON f.id = j.finding_id
            JOIN repos r ON r.id = j.repo_id
            WHERE j.state = ?1
              AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
              AND NOT (
                  (j.kind = ?2 AND f.status IN (?3, ?11, ?12))
                  OR (j.kind = ?4 AND f.status = ?5)
                  OR (j.kind = ?6 AND f.status = ?7)
                  OR (j.kind = ?8 AND f.status IN (?9, ?10))
              )
            ORDER BY j.id
            "#,
            suspended,
            fix,
            queued,
            recheck,
            rechecking,
            engage,
            pr_open,
            harvest,
            merged,
            closed,
            blocked,
            fixing
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut retired = Vec::with_capacity(stranded.len());
        for row in stranded {
            let msg = format!(
                "resume {} {}: job {} retired, finding #{} is {}",
                row.kind, row.repo_name, row.id, row.finding_id, row.status
            );
            retire_job(
                &mut *tx,
                row.id,
                JobState::Killed,
                Some("finding-moved"),
                &msg,
            )
            .await?;
            insert_event(
                &mut *tx,
                "resume",
                &msg,
                Some(row.id),
                Some(row.finding_id),
                None,
            )
            .await?;
            retired.push(row.id);
        }
        tx.commit().await?;
        Ok(retired)
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
    pub async fn resume_chain_stats(&self, job_id: i64) -> sqlx::Result<ResumeChainStats> {
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
            "#,
            job_id,
            RESUME_CHAIN_MAX_DEPTH
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

    /// Take a suspended attempt out of the resumable pool for good.
    ///
    /// The ways a suspension ends without being continued: the
    /// scheduler's give-up ceiling retires it `failed` / `give-up`; a
    /// suspension whose working directory is gone is retired `killed` /
    /// `workdir-gone`; fresh work at the same key retires it `killed` /
    /// `superseded` ([`Self::create_job`]); a finding that moved on retires
    /// it `killed` / `finding-moved` ([`Self::retire_stranded_suspensions`]);
    /// and a resume that found the transcript gone retires it `killed`.
    /// All are terminal states, so [`Self::list_resumable_jobs`] stops
    /// offering the row.
    ///
    /// `killed_reason` is `Option` so the last case can leave the
    /// original reason alone: that attempt really was killed for `cap`,
    /// and overwriting that with the successor's problem would lose the
    /// only record of why the work stopped. `notes` carries the new fact
    /// instead.
    pub async fn retire_suspended_job(
        &self,
        job_id: i64,
        state: JobState,
        killed_reason: Option<&str>,
        notes: &str,
    ) -> sqlx::Result<()> {
        retire_job(&self.pool, job_id, state, killed_reason, notes).await
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

    /// INSERT INTO jobs, returns new id. state starts as "running" (the
    /// scheduler immediately overwrites to the caller's desired state).
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
    /// `resumed_from` names the suspended attempt this job continues, or
    /// `None` for work starting fresh. It is written here and never
    /// updated, so a row can only ever point at an id that already
    /// existed — which is what keeps the chain acyclic.
    ///
    /// Fresh work supersedes a suspension of the same work: a suspended
    /// job with no successor, of this kind and for this finding (finding
    /// kinds) or this repo (hunt and the analysis kinds), is retired
    /// `killed` / `superseded` in the same transaction. Whatever path
    /// started the work over, the fresh attempt now owns it, so the
    /// suspension can never be resumed. Left `suspended`, it would stay in
    /// the table forever. Doing it here rather than in selection is what
    /// covers every path that creates a job.
    ///
    /// The retired ids are returned because their chains' working trees
    /// must be released before the fresh job builds its own: a fix chain
    /// holds its branch checked out, and git refuses to check one branch
    /// out in two worktrees.
    ///
    /// A resumed attempt copies `pinned_sha` and `blocker` from the attempt
    /// it continues, here in the INSERT, so every link of a chain names the
    /// commit its shared tree was created at and the prerequisite, if any,
    /// it was blocked on.
    pub async fn create_job(
        &self,
        kind: JobKind,
        repo_id: i64,
        finding_id: Option<i64>,
        cap_tokens: Option<i64>,
        state: JobState,
        estimated_tokens: Option<i64>,
        resumed_from: Option<i64>,
    ) -> Result<CreatedJob, StoreWriteError> {
        let now = now_ms();
        // IMMEDIATE so the supersession below reads the suspensions under
        // the same write lock that inserts their replacement.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query!(
            "INSERT INTO jobs \
             (kind, repo_id, finding_id, cap_tokens, state, started_at, estimated_tokens, \
              resumed_from, pinned_sha, blocker, full_history, full_hunt_request) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, \
                    (SELECT p.pinned_sha FROM jobs p WHERE p.id = ?8), \
                    (SELECT p.blocker FROM jobs p WHERE p.id = ?8), \
                    COALESCE((SELECT p.full_history FROM jobs p WHERE p.id = ?8), 0), \
                    (SELECT p.full_hunt_request FROM jobs p WHERE p.id = ?8) \
             WHERE EXISTS (SELECT 1 FROM repos WHERE id = ?2 AND deleted_at IS NULL)",
            kind,
            repo_id,
            finding_id,
            cap_tokens,
            state,
            now,
            estimated_tokens,
            resumed_from
        )
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(StoreWriteError::Refused(format!(
                "repo {repo_id} is deleted -- cannot start a {kind} job"
            )));
        }
        let id = result.last_insert_rowid();
        let mut retired = Vec::new();

        if resumed_from.is_none() {
            let suspended = JobState::Suspended;
            let by_finding = kind.is_finding();
            let superseded = sqlx::query!(
                r#"
                SELECT j.id AS "id!", r.name AS "repo_name!"
                FROM jobs j
                JOIN repos r ON r.id = j.repo_id
                WHERE j.state = ?1
                  AND j.kind = ?2
                  AND j.id != ?3
                  AND CASE WHEN ?4 THEN j.finding_id = ?5 ELSE j.repo_id = ?6 END
                  AND NOT EXISTS (SELECT 1 FROM jobs s WHERE s.resumed_from = j.id)
                ORDER BY j.id
                "#,
                suspended,
                kind,
                id,
                by_finding,
                finding_id,
                repo_id
            )
            .fetch_all(&mut *tx)
            .await?;
            for old in superseded {
                let msg = format!(
                    "resume {kind} {}: job {} superseded by fresh job {id}",
                    old.repo_name, old.id
                );
                retire_job(&mut *tx, old.id, JobState::Killed, Some("superseded"), &msg).await?;
                insert_event(&mut *tx, "resume", &msg, Some(old.id), finding_id, None).await?;
                retired.push(old.id);
            }
        }

        tx.commit().await?;
        Ok(CreatedJob {
            id,
            superseded: retired,
        })
    }

    /// Record a job that never ran: `failed`, finished now, with `notes`
    /// saying why. `tokens_new` stays NULL rather than 0 — nothing was
    /// spawned, so there is no spend to know, and a 0 would be read back by
    /// the per-kind estimate as a job that cost nothing.
    pub async fn fail_unstarted_job(&self, job_id: i64, notes: &str) -> sqlx::Result<()> {
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
    /// migration 015; resumed attempts get theirs from [`Self::create_job`].
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

    /// The prerequisite report `job_id`'s fix chain is blocked on: set by
    /// [`Self::block_fix_job`], inherited by every resume of the chain.
    pub async fn job_blocker(&self, job_id: i64) -> sqlx::Result<Option<String>> {
        Ok(
            sqlx::query_scalar!("SELECT blocker FROM jobs WHERE id = ?1", job_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten(),
        )
    }

    /// Whether the chain rooted at `origin_id` still needs its working
    /// tree, and how long ago it last did anything.
    ///
    /// Walks successors from the origin, because a workspace is keyed by
    /// its chain's FIRST job and every later attempt links back to it.
    /// Depth-capped for the same reason [`Self::resume_chain_stats`] is.
    ///
    /// A `suspended` attempt that a later row continues is not live. It
    /// stays `suspended` once resumed — the successor is the record that
    /// it was picked up, see [`Self::list_resumable_jobs`] — and is never
    /// offered again, so counting it would hold every resumed chain's
    /// tree, and a fix chain's branch, for good.
    pub async fn chain_status(&self, origin_id: i64) -> sqlx::Result<ChainStatus> {
        let running = JobState::Running;
        let suspended = JobState::Suspended;
        sqlx::query_as!(
            ChainStatus,
            r#"
            WITH RECURSIVE chain(id, depth) AS (
                SELECT id, 0 FROM jobs WHERE id = ?1
                UNION
                SELECT j.id, c.depth + 1
                FROM jobs j, chain c
                WHERE c.depth < ?2 AND j.resumed_from = c.id
            )
            SELECT COUNT(*) AS "attempts!: i64",
                   COALESCE(SUM(a.state = ?3 OR (a.state = ?4 AND NOT EXISTS
                       (SELECT 1 FROM jobs s WHERE s.resumed_from = a.id))), 0) AS "live!: i64",
                   MAX(a.finished_at) AS "last_finished_at?: i64",
                   (SELECT r.path FROM jobs o JOIN repos r ON r.id = o.repo_id
                    WHERE o.id = ?1) AS "clone?: String"
            FROM jobs a WHERE a.id IN (SELECT id FROM chain)
            "#,
            origin_id,
            RESUME_CHAIN_MAX_DEPTH,
            running,
            suspended
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Every running or suspended job, with what it could still be
    /// using on disk. Read by the workspace sweep so that no directory a
    /// live job refers to is ever removed.
    pub async fn live_jobs(&self) -> sqlx::Result<Vec<LiveJob>> {
        let running = JobState::Running;
        let suspended = JobState::Suspended;
        sqlx::query_as!(
            LiveJob,
            r#"
            SELECT id AS "id!", kind AS "kind!: JobKind", finding_id, session_file
            FROM jobs WHERE state IN (?1, ?2)
            "#,
            running,
            suspended
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
    pub async fn complete_job(&self, job_id: i64, outcome: &JobOutcome<'_>) -> sqlx::Result<()> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record an orphaned running job as `suspended`: the prior process
    /// died while its worker had done metered work, and the transcript is
    /// on disk for the resume tier to continue. `killed_reason` stays
    /// `orphaned` so the row still says how the attempt ended.
    pub async fn suspend_orphan(
        &self,
        job_id: i64,
        session_file: &str,
        tokens_new: i64,
        notes: &str,
        finished_at: i64,
    ) -> sqlx::Result<()> {
        let suspended = JobState::Suspended;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, killed_reason = 'orphaned', \
             session_file = ?2, tokens_new = ?3, notes = ?4, finished_at = ?5 \
             WHERE id = ?6",
            suspended,
            session_file,
            tokens_new,
            notes,
            finished_at,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark an orphaned running job as killed during reconciliation.
    pub async fn orphan_job(&self, job_id: i64, notes: &str, finished_at: i64) -> sqlx::Result<()> {
        let killed = JobState::Killed;
        sqlx::query!(
            "UPDATE jobs SET state = ?1, pid = NULL, killed_reason = 'orphaned', \
             notes = ?2, finished_at = ?3 WHERE id = ?4",
            killed,
            notes,
            finished_at,
            job_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Downgrade an already-recorded job to failed (e.g. post-run push failure).
    pub async fn fail_job(&self, job_id: i64, notes: &str) -> sqlx::Result<()> {
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
                   budget_override AS "budget_override: BudgetOverride", fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
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
    /// Everything except the verdicts that mean forget this. A blocked fix,
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
                   budget_override AS "budget_override: BudgetOverride", fix_attempts, last_fix_failure,
                   recheck_attempts, last_recheck_failure, ecosystem, package,
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
    /// Deliberately *not* `INSERT OR IGNORE`: the lookup and the INSERT are
    /// separate statements, so two callers racing on the same fingerprint
    /// would have the second fail the UNIQUE constraint rather than be
    /// ignored. Only the single scheduler loop ingests, so no second caller
    /// exists to open that window.
    /// Insert a finding, or report the existing one with this fingerprint.
    ///
    /// `found_by_job` is stored only on a genuine insert: a duplicate
    /// belongs to the job that first turned it up, not the latest one to
    /// rediscover it.
    pub async fn upsert_finding(
        &self,
        repo_id: i64,
        row: &FindingInsert,
        finding_type: &str,
        found_by_job: Option<i64>,
    ) -> sqlx::Result<(i64, bool)> {
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
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27, ?28, ?29, ?30, ?31)",
            finding_type, repo_id, fingerprint, file, symbol, line, bug_class,
            severity, confidence, summary, detail, evidence_plan, introduced_by,
            ecosystem, package, current_version, latest_version, update_type, security_advisory,
            missing_tests_ref, test_file, smell_type, suggested_refactor,
            modernization_class, current_approach, proposed_approach, standard_section,
            new_status, now, now, found_by_job
        )
        .execute(&self.pool)
        .await?;
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
    /// landed. `true` when anything changed.
    pub async fn refresh_dep_update(
        &self,
        finding_id: i64,
        row: &FindingInsert,
    ) -> sqlx::Result<bool> {
        let now = now_ms();
        let new = FindingStatus::New;
        let queued = FindingStatus::Queued;
        let superseded = FindingStatus::Superseded;
        let unproposed = DEP_UNPROPOSED_REASON;
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
        // SQLite evaluates every SET expression against the old row, so the
        // two CASEs both see the status before this update.
        let done = sqlx::query!(
            "UPDATE findings SET file = ?1, severity = ?2, confidence = ?3, summary = ?4, \
             detail = ?5, ecosystem = ?6, package = ?7, current_version = ?8, \
             latest_version = ?9, update_type = ?10, security_advisory = ?11, \
             status = CASE WHEN status = ?12 THEN ?13 ELSE status END, \
             verdict_reason = CASE WHEN status = ?12 THEN NULL ELSE verdict_reason END, \
             updated_at = ?14 \
             WHERE id = ?15 AND type = 'dep_update' \
               AND (status IN (?13, ?16) OR (status = ?12 AND verdict_reason = ?17)) \
               AND (status = ?12 OR file IS NOT ?1 OR severity IS NOT ?2 \
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
            superseded,
            new,
            now,
            finding_id,
            queued,
            unproposed
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Retire the repo's `new` `dep_update` findings that a Renovate scan
    /// no longer proposes: the update landed some other way, or Renovate
    /// now files it under another unit (a different group, the next major).
    /// `proposed` is every fingerprint the scan produced; `unchecked` the
    /// dependencies it skipped or failed to look up, whose findings stay.
    /// A group finding is matched by its group name, not its members.
    /// Anything a worker or the operator has taken up is left alone.
    /// Returns how many were retired.
    pub async fn supersede_unproposed_dep_updates(
        &self,
        repo_id: i64,
        proposed: &[String],
        unchecked: &[String],
    ) -> sqlx::Result<u64> {
        let now = now_ms();
        let new = FindingStatus::New;
        let superseded = FindingStatus::Superseded;
        let unproposed = DEP_UNPROPOSED_REASON;
        let proposed = serde_json::to_string(proposed).unwrap_or_else(|_| "[]".to_owned());
        let unchecked = serde_json::to_string(unchecked).unwrap_or_else(|_| "[]".to_owned());
        let done = sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
             WHERE repo_id = ?4 AND type = 'dep_update' AND status = ?5 \
               AND fingerprint NOT IN (SELECT value FROM json_each(?6)) \
               AND package NOT IN (SELECT value FROM json_each(?7))",
            superseded,
            unproposed,
            now,
            repo_id,
            new,
            proposed,
            unchecked
        )
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected())
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
    /// match is left as it is. `moves` is every move of every finding the
    /// scan produced. Returns how many were handed over.
    pub async fn hand_over_queued_dep_updates(
        &self,
        repo_id: i64,
        moves: &[ProposedMove],
    ) -> sqlx::Result<u64> {
        let now = now_ms();
        let new = FindingStatus::New;
        let queued = FindingStatus::Queued;
        let superseded = FindingStatus::Superseded;
        let exempt = BudgetOverride::Exempt;
        let moves = serde_json::to_string(moves).unwrap_or_else(|_| "[]".to_owned());
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
            let reason = format!("replaced by #{} from the dependency scan", pair.to_id);
            let from = sqlx::query!(
                "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
                 WHERE id = ?4 AND status = ?5",
                superseded,
                reason,
                now,
                pair.from_id,
                queued
            )
            .execute(&mut *tx)
            .await?;
            if from.rows_affected() == 0 {
                continue;
            }
            // Several queued findings can hand over to one group: keep the
            // strongest override any of them (or the group) carries. `once`
            // is cleared after the next attempt, `exempt` only by hand, so a
            // `once` that arrived first must not displace a later `exempt`.
            sqlx::query!(
                "UPDATE findings SET status = ?1, updated_at = ?2, \
                 budget_override = CASE \
                     WHEN budget_override = ?6 \
                       OR (SELECT budget_override FROM findings WHERE id = ?3) = ?6 \
                     THEN ?6 \
                     ELSE COALESCE(budget_override, \
                         (SELECT budget_override FROM findings WHERE id = ?3)) \
                 END \
                 WHERE id = ?4 AND status IN (?5, ?1)",
                queued,
                now,
                pair.from_id,
                pair.to_id,
                new,
                exempt
            )
            .execute(&mut *tx)
            .await?;
            handed += 1;
        }
        tx.commit().await?;
        Ok(handed)
    }

    /// Mark a PR as merged (UPSERT).
    ///
    /// `pr_state` holds one row per finding, not per PR, and a finding can
    /// ship more than one PR: a closure harvested as `abandoned` sends it
    /// back to `new`, and its next fix opens a fresh PR on the same row.
    /// The harvest bookkeeping (`harvested_at`, `harvest_attempts`,
    /// `last_harvest_failure`) describes the PR it was recorded for, so a
    /// different `pr_number` clears it; otherwise the new PR inherits the
    /// old one's stamp and `list_pending_harvest` never selects it. The
    /// same number keeps it, so re-syncing a PR never un-harvests it. A
    /// stored NULL is an unknown number, not another PR (`set_pr_number`
    /// fills it in for the PR already being worked), hence `<>` and not
    /// `IS NOT`. Every `pr_state` upsert repeats these three CASEs:
    /// `query!` takes one literal, so the SQL cannot be shared.
    pub async fn mark_pr_merged(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, needs_attention, synced_at) \
             VALUES (?1, ?2, 'MERGED', NULL, ?3) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, state = 'MERGED', \
             needs_attention = NULL, synced_at = excluded.synced_at, \
             harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.harvested_at END, \
             harvest_attempts = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN 0 ELSE pr_state.harvest_attempts END, \
             last_harvest_failure = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.last_harvest_failure END",
            finding_id,
            pr_number,
            synced_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a PR as closed (UPSERT). A new `pr_number` clears the harvest
    /// bookkeeping, as in [`Self::mark_pr_merged`].
    pub async fn mark_pr_closed(
        &self,
        finding_id: i64,
        pr_number: i64,
        synced_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, needs_attention, synced_at) \
             VALUES (?1, ?2, 'CLOSED', NULL, ?3) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, state = 'CLOSED', \
             needs_attention = NULL, synced_at = excluded.synced_at, \
             harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.harvested_at END, \
             harvest_attempts = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN 0 ELSE pr_state.harvest_attempts END, \
             last_harvest_failure = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.last_harvest_failure END",
            finding_id,
            pr_number,
            synced_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Full sync of an open PR.  The main UPSERT sets all always-present
    /// columns; two conditional queries handle `attention_since` and
    /// clearing addressed state — three compile-time checked queries
    /// instead of one dynamic one. A new `pr_number` clears the harvest
    /// bookkeeping, as in [`Self::mark_pr_merged`].
    ///
    /// All three share one transaction because the conditional UPDATEs are
    /// decided from the state the UPSERT writes: `attention_since` is only
    /// stamped on the sync where the attention reason *changes*, so if the
    /// UPSERT committed the new `attention_fingerprint` and the UPDATE then
    /// failed, every later sync would see an unchanged reason and never
    /// stamp it — leaving `scheduler::list_attention` ordering and the displayed
    /// attention age permanently wrong.  `clear_addressed` has the same
    /// shape.  A plain (deferred) `begin` suffices: the first statement is
    /// a write, so the lock is taken before anything is read back.
    pub async fn sync_pr_open(&self, finding_id: i64, d: &SyncPrData) -> sqlx::Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number, state, mergeable, checks, \
             head_ref, head_sha, last_activity_at, last_engaged_activity_at, \
             needs_attention, attention_fingerprint, synced_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
             ON CONFLICT(finding_id) DO UPDATE SET \
             pr_number = excluded.pr_number, \
             state = excluded.state, \
             mergeable = excluded.mergeable, \
             checks = excluded.checks, \
             head_ref = excluded.head_ref, \
             head_sha = excluded.head_sha, \
             last_activity_at = excluded.last_activity_at, \
             last_engaged_activity_at = excluded.last_engaged_activity_at, \
             needs_attention = excluded.needs_attention, \
             attention_fingerprint = excluded.attention_fingerprint, \
             synced_at = excluded.synced_at, \
             harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.harvested_at END, \
             harvest_attempts = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN 0 ELSE pr_state.harvest_attempts END, \
             last_harvest_failure = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.last_harvest_failure END",
            finding_id,
            d.pr_number,
            d.state,
            d.mergeable,
            d.checks,
            d.head_ref,
            d.head_sha,
            d.last_activity_at,
            d.last_engaged_activity_at,
            d.needs_attention,
            d.attention_fingerprint,
            d.synced_at
        )
        .execute(&mut *tx)
        .await?;
        // Conditional: set attention_since when the reason changed.
        if let Some(since) = d.attention_since {
            sqlx::query!(
                "UPDATE pr_state SET attention_since = ?1 WHERE finding_id = ?2",
                since,
                finding_id
            )
            .execute(&mut *tx)
            .await?;
        }
        // Conditional: clear addressed state when the static snapshot changed.
        if d.clear_addressed {
            sqlx::query!(
                "UPDATE pr_state SET addressed_fingerprint = NULL, \
                 addressed_head_sha = NULL WHERE finding_id = ?1",
                finding_id
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Update engagement watermark and addressed state after a successful
    /// engage cycle.  `addressed_fingerprint` / `addressed_head_sha` are
    /// nullable: pass None to clear (pushed) or Some to set (replied-only).
    pub async fn mark_pr_engaged(
        &self,
        finding_id: i64,
        last_engaged_activity_at: i64,
        synced_at: i64,
        addressed_fingerprint: Option<&str>,
        addressed_head_sha: Option<&str>,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET last_engaged_activity_at = ?1, synced_at = ?2, \
             addressed_fingerprint = ?3, addressed_head_sha = ?4 \
             WHERE finding_id = ?5",
            last_engaged_activity_at,
            synced_at,
            addressed_fingerprint,
            addressed_head_sha,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set a flagged PR's attention aside without engaging it: the engage
    /// watermark moves to its latest activity and its static reasons count
    /// as addressed at its current head, as an engage that did nothing
    /// leaves them ([`Self::mark_pr_engaged`]), and the flag clears now
    /// rather than at the next sync. New activity or a push raises it again.
    pub async fn set_attention_aside(&self, finding_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_state SET last_engaged_activity_at = COALESCE(last_activity_at, ?1), \
             addressed_fingerprint = attention_fingerprint, addressed_head_sha = head_sha, \
             needs_attention = NULL, attention_since = NULL \
             WHERE finding_id = ?2",
            now,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a merged PR as harvested.
    pub async fn mark_pr_harvested(&self, finding_id: i64, harvested_at: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET harvested_at = ?1 WHERE finding_id = ?2",
            harvested_at,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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
        status: FindingStatus,
        reason: &str,
        anchor: Option<&VerdictAnchor>,
        harvested_at: i64,
    ) -> sqlx::Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let now = now_ms();
        let classified = sqlx::query!(
            "UPDATE findings SET status = ?1, verdict_reason = ?2, updated_at = ?3 \
             WHERE id = ?4 AND status = 'closed'",
            status,
            reason,
            now,
            finding_id
        )
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        if classified {
            Self::write_anchor(&mut tx, finding_id, anchor).await?;
        }
        sqlx::query!(
            "UPDATE pr_state SET harvested_at = ?1 WHERE finding_id = ?2",
            harvested_at,
            finding_id
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(classified)
    }

    /// Self-heal a missing `pr_number` (UPSERT — row may not exist yet).
    /// A new `pr_number` clears the harvest bookkeeping, as in
    /// [`Self::mark_pr_merged`]; filling in a NULL one does not.
    pub async fn set_pr_number(&self, finding_id: i64, pr_number: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "INSERT INTO pr_state (finding_id, pr_number) VALUES (?1, ?2) \
             ON CONFLICT(finding_id) DO UPDATE SET pr_number = excluded.pr_number, \
             harvested_at = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.harvested_at END, \
             harvest_attempts = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN 0 ELSE pr_state.harvest_attempts END, \
             last_harvest_failure = CASE WHEN pr_state.pr_number <> excluded.pr_number \
                 THEN NULL ELSE pr_state.last_harvest_failure END",
            finding_id,
            pr_number
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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
                   f.budget_override AS "budget_override: BudgetOverride", f.fix_attempts, f.last_fix_failure,
                   f.recheck_attempts, f.last_recheck_failure, f.ecosystem, f.package,
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

    /// UPDATE finding's analysis fields (summary, detail, etc.) after a
    /// worker recheck.  Uses COALESCE to skip NULL fields.
    pub async fn update_finding_analysis(
        &self,
        finding_id: i64,
        fields: &FindingAnalysisUpdate,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        let summary = fields.summary.as_deref();
        let detail = fields.detail.as_deref();
        let severity = fields.severity;
        sqlx::query!(
            "UPDATE findings SET \
             updated_at = ?1, \
             summary = COALESCE(?2, summary), \
             detail = COALESCE(?3, detail), \
             confidence = COALESCE(?4, confidence), \
             severity = COALESCE(?5, severity) \
             WHERE id = ?6",
            now,
            summary,
            detail,
            fields.confidence,
            severity,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Recover orphaned jobs (state=running) and findings (status=fixing)
    /// from a crashed prior process.
    pub async fn reconcile_orphaned_jobs(&self) -> sqlx::Result<(Vec<Finding>, Vec<Job>)> {
        // Findings stuck at 'fixing' -> queued
        let stuck_findings = self
            .list_findings(&FindingFilter {
                status: Some(FindingStatus::Fixing),
                ..FindingFilter::default()
            })
            .await?;
        for f in &stuck_findings {
            self.set_finding_status(f.id, FindingStatus::Queued).await?;
        }
        // Jobs stuck at 'running' -> killed/orphaned
        let mut orphaned_jobs = Vec::new();
        for j in self.list_running_jobs().await? {
            let now = now_ms();
            self.orphan_job(
                j.id,
                "reconciled at cycle startup -- prior process died mid-job",
                now,
            )
            .await?;
            orphaned_jobs.push(j);
        }
        Ok((stuck_findings, orphaned_jobs))
    }

    /// Record a fix attempt fingerprint for retry-limiting.
    pub async fn record_fix_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        // Streak tracking: same failure -> increment; different -> reset to 1
        let finding = self.get_finding(finding_id).await?;
        let (prev_failure, prev_attempts) = match &finding {
            Some(f) => (f.last_fix_failure.as_deref(), f.fix_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE findings SET fix_attempts = ?1, last_fix_failure = ?2, updated_at = ?3 WHERE id = ?4",
            attempts, failure_fingerprint, now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_fix_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET fix_attempts = 0, last_fix_failure = NULL, updated_at = ?1 WHERE id = ?2",
            now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn record_recheck_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let finding = self.get_finding(finding_id).await?;
        let (prev_failure, prev_attempts) = match &finding {
            Some(f) => (f.last_recheck_failure.as_deref(), f.recheck_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE findings SET recheck_attempts = ?1, last_recheck_failure = ?2, updated_at = ?3 WHERE id = ?4",
            attempts, failure_fingerprint, now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_recheck_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE findings SET recheck_attempts = 0, last_recheck_failure = NULL, updated_at = ?1 WHERE id = ?2",
            now, finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
    pub async fn record_harvest_attempt(
        &self,
        finding_id: i64,
        failure_fingerprint: &str,
    ) -> sqlx::Result<i64> {
        // harvest_attempts are stored in pr_state, not findings
        let ps = self.get_pr_state(finding_id).await?;
        let (prev_failure, prev_attempts) = match &ps {
            Some(p) => (p.last_harvest_failure.as_deref(), p.harvest_attempts),
            None => (None, 0),
        };
        let attempts = if Some(failure_fingerprint) == prev_failure {
            prev_attempts + 1
        } else {
            1
        };
        sqlx::query!(
            "UPDATE pr_state SET harvest_attempts = ?1, last_harvest_failure = ?2 \
             WHERE finding_id = ?3",
            attempts,
            failure_fingerprint,
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(attempts)
    }
    pub async fn clear_harvest_attempts(&self, finding_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_state SET harvest_attempts = 0, last_harvest_failure = NULL \
             WHERE finding_id = ?1",
            finding_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Move a finding from `from` to `to` only if it is still at `from`;
    /// `false` when something else (an operator verdict) changed it since
    /// the caller read it. Undo with [`Self::finalize_in_progress`].
    pub async fn claim_in_progress(
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
    /// If finding is still at `expected_status`, reset to `fallback`. One
    /// guarded UPDATE, so a verdict landing between a read and a write here
    /// cannot be overwritten.
    pub async fn finalize_in_progress(
        &self,
        finding_id: i64,
        expected_status: FindingStatus,
        fallback: FindingStatus,
    ) -> sqlx::Result<()> {
        self.claim_in_progress(finding_id, expected_status, fallback)
            .await
            .map(drop)
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
