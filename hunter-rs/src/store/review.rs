//! The review model every review shares (migration 040): a review of one
//! subject (a PR hunter did not open, a fix, an engage's reply), its rounds
//! over `base..head`, the issues each round reported and what each later
//! round said of earlier ones. What a reviewer reports is the same for
//! every subject: both reviewers write the issue schema of their playbook
//! (`review.md`). A subject's own state extends these rows in tables of
//! its own ([`super::pr_review`] for a PR review's).
//!
//! A fix's review is the `fix` subject: one review per fix chain, owned by
//! the finding and the chain's first job, created by the first round the
//! job lane records of it ([`JobStore::record_fix_review_round`]). The
//! rounds are a record: the fix's review loop does not read them back.
//!
//! [`JobStore::record_fix_review_round`]: super::JobStore::record_fix_review_round

use serde::{Deserialize, Serialize};
use sqlx::SqliteConnection;

use super::{ImmediateTx, Store};
use crate::types::{FixReviewRoundOut, ReviewIssueOut};
use crate::util::now_ms;

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
/// latter two), when, and what its reviewer spent (`tokens_new`, when
/// counted). Returns the round's id.
pub(super) async fn insert_round(
    tx: &mut SqliteConnection,
    review_id: i64,
    job_id: Option<i64>,
    (base, head, merge_base): (&str, &str, Option<&str>),
    (verdict, failure): (&str, Option<&str>),
    (finished_at, tokens_new): (i64, Option<i64>),
) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"INSERT INTO review_rounds
               (review_id, job_id, base_sha, head_sha, merge_base_sha, verdict, failure,
                finished_at, tokens_new)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
           RETURNING id AS "id!: i64""#,
        review_id,
        job_id,
        base,
        head,
        merge_base,
        verdict,
        failure,
        finished_at,
        tokens_new
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

/// How a round of a fix's review ended, as
/// [`JobStore::record_fix_review_round`](super::JobStore::record_fix_review_round)
/// records it: the round's verdict, and the issues of a kept one.
#[derive(Debug, Clone, Copy)]
pub enum FixReviewRound<'a> {
    /// The reviewer finished `base..head`, reporting these issues (none:
    /// a clean round).
    Ok(&'a [ReportedIssue]),
    /// The reviewer was stopped (killed, crashed, its tree not put back)
    /// after it wrote these issues, which the fix goes on with; `why` it
    /// did not finish.
    Partial {
        why: &'a str,
        issues: &'a [ReportedIssue],
    },
    /// No review was had this round, for this reason.
    Failed(&'a str),
}

impl Store {
    /// Record one round of the review of `finding_id`'s fix chain, the
    /// chain that job `origin_job_id` began, in one transaction: the
    /// chain's review, the first time a round of it is recorded; the
    /// round, run by job `job_id` over `base..head` with its reviewer's
    /// spend (`tokens_new`); and the issues a kept round reported,
    /// follow-ups among them. A fix's range always starts at the commit
    /// its tree was made at, the change's merge-base, so every round
    /// reviews the whole change. Returns the round's id. The job lane's
    /// write
    /// ([`JobStore::record_fix_review_round`](super::JobStore::record_fix_review_round)).
    pub(super) async fn record_fix_review_round(
        &self,
        (finding_id, origin_job_id): (i64, i64),
        job_id: i64,
        (base, head): (&str, &str),
        tokens_new: Option<i64>,
        round: FixReviewRound<'_>,
    ) -> sqlx::Result<i64> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        sqlx::query!(
            "INSERT INTO reviews (subject, finding_id, origin_job_id) VALUES ('fix', ?1, ?2) \
             ON CONFLICT (origin_job_id) DO NOTHING",
            finding_id,
            origin_job_id
        )
        .execute(&mut *tx)
        .await?;
        let review_id = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM reviews
               WHERE subject = 'fix' AND finding_id = ?1 AND origin_job_id = ?2"#,
            finding_id,
            origin_job_id
        )
        .fetch_one(&mut *tx)
        .await?;
        let (verdict, failure, issues) = match round {
            FixReviewRound::Ok(issues) => ("ok", None, issues),
            FixReviewRound::Partial { why, issues } => ("partial", Some(why), issues),
            FixReviewRound::Failed(why) => ("failed", Some(why), &[][..]),
        };
        let round_id = insert_round(
            &mut tx,
            review_id,
            Some(job_id),
            (base, head, Some(base)),
            (verdict, failure),
            (now_ms(), tokens_new),
        )
        .await?;
        for issue in issues {
            insert_issue(&mut tx, round_id, issue).await?;
        }
        tx.commit().await?;
        Ok(round_id)
    }

    /// Every round of the reviews of `finding_id`'s fixes, newest first,
    /// each with its issues, blocking ones first.
    pub async fn fix_review_rounds(&self, finding_id: i64) -> sqlx::Result<Vec<FixReviewRoundOut>> {
        let rounds = sqlx::query!(
            r#"
            SELECT r.id AS "id!: i64", rv.origin_job_id AS "origin_job_id!: i64", r.job_id,
                   r.base_sha, r.head_sha, r.verdict, r.failure,
                   r.finished_at AS "finished_at!: i64", r.tokens_new
            FROM review_rounds r JOIN reviews rv ON rv.id = r.review_id
            WHERE rv.subject = 'fix' AND rv.finding_id = ?1
            ORDER BY r.finished_at DESC, r.id DESC
            "#,
            finding_id
        )
        .fetch_all(&self.pool)
        .await?;
        let issues = sqlx::query!(
            r#"
            SELECT i.id AS "id!: i64", i.round_id AS "round_id!: i64", i.kind,
                   i.blocking AS "blocking!: bool", i.severity, i.confidence, i.file, i.line,
                   i.side, i.summary, i.detail
            FROM review_issues i JOIN review_rounds r ON r.id = i.round_id
            JOIN reviews rv ON rv.id = r.review_id
            WHERE rv.subject = 'fix' AND rv.finding_id = ?1
            ORDER BY i.round_id, i.blocking DESC, i.id
            "#,
            finding_id
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_round: std::collections::HashMap<i64, Vec<ReviewIssueOut>> =
            std::collections::HashMap::new();
        for i in issues {
            by_round
                .entry(i.round_id)
                .or_default()
                .push(ReviewIssueOut {
                    id: i.id,
                    kind: i.kind,
                    blocking: i.blocking,
                    severity: i.severity,
                    confidence: i.confidence,
                    file: i.file,
                    line: i.line,
                    side: i.side,
                    summary: i.summary,
                    detail: i.detail,
                });
        }
        Ok(rounds
            .into_iter()
            .map(|r| FixReviewRoundOut {
                id: r.id,
                origin_job_id: r.origin_job_id,
                job_id: r.job_id,
                base_sha: r.base_sha,
                head_sha: r.head_sha,
                verdict: r.verdict,
                failure: r.failure,
                finished_at: r.finished_at,
                tokens_new: r.tokens_new,
                issues: by_round.remove(&r.id).unwrap_or_default(),
            })
            .collect())
    }
}
