//! The review model every review shares (migration 040): a review of one
//! subject (a PR hunter did not open, a fix, an engage's reply), its rounds
//! over `base..head`, the issues each round reported and what each later
//! round said of earlier ones. What a reviewer reports is the same for
//! every subject: both reviewers write the issue schema of their playbook
//! (`review.md`). A subject's own state extends these rows in tables of
//! its own ([`super::pr_review`] for a PR review's).

use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

/// An issue as the reviewer reported it: a pre-PR round's, which goes back
/// to the fixer and is kept in the fix's review files, or a PR review
/// round's ([`PrReviewRound::Ok`](super::PrReviewRound::Ok)).
///
/// It is written as the playbooks' JSON object, with the fields the
/// reviewer left out still left out, and read leniently (its `From`
/// `serde_json::Value`), so the files of an earlier daemon, which kept the
/// reviewer's entries as they were, still read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(from = "serde_json::Value")]
pub struct ReportedIssue {
    pub kind: String,
    pub blocking: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    /// `RIGHT` or `LEFT`, in any case; anything else is the reviewer not
    /// saying.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

impl ReportedIssue {
    /// Whether the reviewer judged the issue outside the change's scope: a
    /// real defect it did not introduce or worsen, or a generalization
    /// beyond the finding. A fix's review reports it to the human, and
    /// never sends it to the fixer.
    pub fn is_follow_up(&self) -> bool {
        self.kind == "follow-up"
    }
}

/// A reviewer's issue entry: the playbook's schema, read leniently. A
/// missing or mistyped optional field is none, a missing kind is the
/// reviewer's default `introduced`, a `blocking` that is not `true` is not
/// blocking, and fields outside the schema are dropped. Which entries are
/// issues at all, and a `blocking` the reviewer got wrong, are for the
/// reader of its file to decide (`parse_review`).
impl From<serde_json::Value> for ReportedIssue {
    fn from(v: serde_json::Value) -> Self {
        let s = |k: &str| {
            v.get(k)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Self {
            kind: s("kind").unwrap_or_else(|| "introduced".to_owned()),
            blocking: v.get("blocking").and_then(serde_json::Value::as_bool) == Some(true),
            severity: s("severity"),
            confidence: v.get("confidence").and_then(serde_json::Value::as_f64),
            file: s("file"),
            line: v.get("line").and_then(serde_json::Value::as_i64),
            side: s("side"),
            summary: s("summary").unwrap_or_default(),
            detail: s("detail").unwrap_or_default(),
        }
    }
}

/// Record in `tx` one round of review `review_id` over `base..head`, by
/// job `job_id` when one ran, with the merge-base it resolved
/// (`merge_base`; `None` for a round that failed before resolving it),
/// how it ended (`verdict`: `ok`, `partial` or `failed`, with why for the
/// latter two) and when. Returns the round's id.
pub(super) async fn insert_round(
    tx: &mut SqliteConnection,
    review_id: i64,
    job_id: Option<i64>,
    (base, head, merge_base): (&str, &str, Option<&str>),
    (verdict, failure): (&str, Option<&str>),
    finished_at: i64,
) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"INSERT INTO review_rounds
               (review_id, job_id, base_sha, head_sha, merge_base_sha, verdict, failure,
                finished_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
           RETURNING id AS "id!: i64""#,
        review_id,
        job_id,
        base,
        head,
        merge_base,
        verdict,
        failure,
        finished_at
    )
    .fetch_one(&mut *tx)
    .await
}

/// Record in `tx` `issue`, which round `round_id` reported. Returns its id.
pub(super) async fn insert_issue(
    tx: &mut SqliteConnection,
    round_id: i64,
    issue: &ReportedIssue,
) -> sqlx::Result<i64> {
    // The column takes only the two words a forge knows; anything else is
    // the reviewer not saying.
    let side = issue
        .side
        .as_deref()
        .map(str::to_ascii_uppercase)
        .filter(|s| s == "RIGHT" || s == "LEFT");
    sqlx::query_scalar!(
        r#"INSERT INTO review_issues
               (round_id, kind, blocking, severity, confidence, file, line, side, summary,
                detail)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
           RETURNING id AS "id!: i64""#,
        round_id,
        issue.kind,
        issue.blocking,
        issue.severity,
        issue.confidence,
        issue.file,
        issue.line,
        side,
        issue.summary,
        issue.detail
    )
    .fetch_one(&mut *tx)
    .await
}
