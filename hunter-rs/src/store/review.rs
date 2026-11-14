//! The review model every review shares (migration 040): a review of one
//! subject (a PR hunter did not open, a fix, an engage's reply), its rounds
//! over `base..head`, the issues each round reported, what each later
//! round said of earlier ones (`review_verdicts`), and what the other side
//! answered (`review_answers`). What a reviewer reports is the same for
//! every subject: both reviewers write the issue schema of their playbook
//! (`review.md`), and judge earlier issues by id. A subject's own state
//! extends these rows in tables of its own ([`super::pr_review`] for a PR
//! review's).
//!
//! A fix's review is the `fix` subject: one review per fix chain, owned by
//! the finding and the chain's first job, created by the first round the
//! job lane records of it ([`JobStore::record_fix_review_round`]). Its
//! rounds, issues, verdicts and the fixer's answers
//! ([`JobStore::record_fix_review_answers`]) are what the fix's review
//! loop goes by: which issues are still open ([`Store::fix_review`]), and
//! what the PR body says of the review.
//!
//! [`JobStore::record_fix_review_round`]: super::JobStore::record_fix_review_round
//! [`JobStore::record_fix_review_answers`]: super::JobStore::record_fix_review_answers

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

/// What a round said about one earlier issue (`review_verdicts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueVerdict {
    /// The new commits fix it.
    Addressed,
    /// The other side argued it is no issue, and the round checked the
    /// argument against the code.
    RebuttalAccepted,
    /// It still stands.
    Open,
}

impl IssueVerdict {
    /// The word the reviewer writes and the table keeps.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Addressed => "addressed",
            Self::RebuttalAccepted => "rebuttal-accepted",
            Self::Open => "open",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "addressed" => Some(Self::Addressed),
            "rebuttal-accepted" => Some(Self::RebuttalAccepted),
            "open" => Some(Self::Open),
            _ => None,
        }
    }
}

/// A round's verdict on earlier issue `issue_id`, with its evidence. The
/// last three fields are what a PR review's round was shown of the issue's
/// thread on the PR; `None` for a review without threads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub issue_id: i64,
    pub verdict: IssueVerdict,
    pub note: String,
    /// The reply the issue's thread ended in as the round was shown it,
    /// somebody's other than hunter's (epoch ms): the verdict is posted
    /// there as hunter's answer to it, even when it changes nothing.
    pub answers_reply_at: Option<i64>,
    /// The issue's `reopened_at` as the round read it
    /// ([`EarlierIssue::reopened_at`](super::EarlierIssue::reopened_at)):
    /// the verdict counts only while no later reopen is recorded.
    pub seen_reopened_at: Option<i64>,
    /// The newest reply by somebody other than hunter in the thread as the
    /// round was shown it
    /// ([`EarlierIssue::newest_reply_at`](super::EarlierIssue::newest_reply_at),
    /// epoch ms): a newer one leaves the issue awaiting judgment, and its
    /// thread is not answered or resolved with this verdict.
    pub seen_reply_at: Option<i64>,
}

/// Record in `tx` round `round_id`'s verdict `v` on an earlier issue of
/// its review. A verdict on an issue of another review, or of this very
/// round, is dropped: the reviewer was never shown it. So is a second
/// verdict of the round on the same issue. A subject's own filter goes
/// first (a PR review's binding).
pub(super) async fn insert_verdict(
    tx: &mut SqliteConnection,
    round_id: i64,
    v: &Verdict,
) -> sqlx::Result<()> {
    let word = v.verdict.as_str();
    sqlx::query!(
        "INSERT INTO review_verdicts \
             (round_id, issue_id, verdict, note, answers_reply_at, seen_reopened_at, \
              seen_reply_at) \
         SELECT ?1, i.id, ?3, ?4, ?5, ?6, ?7 FROM review_issues i \
         JOIN review_rounds r ON r.id = i.round_id \
         WHERE i.id = ?2 AND r.id != ?1 \
           AND r.review_id = (SELECT review_id FROM review_rounds WHERE id = ?1) \
         ON CONFLICT DO NOTHING",
        round_id,
        v.issue_id,
        word,
        v.note,
        v.answers_reply_at,
        v.seen_reopened_at,
        v.seen_reply_at
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// What the other side answered to an issue (`review_answers.answer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AnswerKind {
    /// Fixed, in the answer's commit.
    Addressed,
    /// Argued to be no issue, with the answer's evidence.
    Rebutted,
}

impl AnswerKind {
    /// The word the fixer writes and the table keeps.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Addressed => "addressed",
            Self::Rebutted => "rebutted",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "addressed" => Some(Self::Addressed),
            "rebutted" => Some(Self::Rebutted),
            _ => None,
        }
    }
}

/// One answer to an issue: whether it is fixed or argued away, how or
/// why, and the commit that fixes it. As the reviewer is shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IssueAnswer {
    pub answer: AnswerKind,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// What a later round said of an issue ([`Verdict`], as kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueJudged {
    pub round_id: i64,
    pub verdict: IssueVerdict,
    pub note: String,
}

/// An issue of a fix chain's review, with everything said of it since.
#[derive(Debug, Clone)]
pub struct ChainIssue {
    pub id: i64,
    /// The round that reported it.
    pub round_id: i64,
    pub issue: ReportedIssue,
    /// The fixer's answers, oldest first.
    pub answers: Vec<IssueAnswer>,
    /// Later rounds' verdicts on it, oldest first.
    pub verdicts: Vec<IssueJudged>,
}

impl ChainIssue {
    /// How a later round settled it (`addressed` or `rebuttal-accepted`),
    /// if one did. A settled issue is never shown to a reviewer again, so
    /// the first such verdict is the last word on it.
    pub fn settled(&self) -> Option<IssueVerdict> {
        self.verdicts
            .iter()
            .map(|v| v.verdict)
            .find(|v| *v != IssueVerdict::Open)
    }

    /// Whether the fixer still owes it: an issue to answer (no follow-up)
    /// that no round settled. A round that left it unjudged settled
    /// nothing: silence is no verdict.
    pub fn is_open(&self) -> bool {
        !self.issue.is_follow_up() && self.settled().is_none()
    }

    /// The fixer's newest answer.
    pub fn answer(&self) -> Option<&IssueAnswer> {
        self.answers.last()
    }

    /// Why the newest round that judged it still found it open, when the
    /// newest verdict on it is `open`.
    pub fn still_open(&self) -> Option<&str> {
        self.verdicts
            .last()
            .filter(|v| v.verdict == IssueVerdict::Open)
            .map(|v| v.note.as_str())
    }
}

/// A fix chain's review as its loop and the PR body read it
/// ([`Store::fix_review`]).
#[derive(Debug, Clone, Default)]
pub struct FixReview {
    /// The ids of its kept rounds (`ok` or `partial`), oldest first.
    pub rounds: Vec<i64>,
    /// Every issue its rounds reported, follow-ups among them: by round,
    /// oldest first, blocking ones first within a round.
    pub issues: Vec<ChainIssue>,
}

impl FixReview {
    /// The issues the fixer still owes an answer ([`ChainIssue::is_open`]),
    /// blocking ones first, otherwise oldest first.
    pub fn open(&self) -> Vec<ChainIssue> {
        let mut open: Vec<ChainIssue> = self
            .issues
            .iter()
            .filter(|i| i.is_open())
            .cloned()
            .collect();
        open.sort_by_key(|i| !i.issue.blocking);
        open
    }
}

/// How a round of a fix's review ended, as
/// [`JobStore::record_fix_review_round`](super::JobStore::record_fix_review_round)
/// records it: the round's verdict, and the issues and verdicts on earlier
/// issues of a kept one.
#[derive(Debug, Clone, Copy)]
pub enum FixReviewRound<'a> {
    /// The reviewer finished `base..head`, reporting these issues (none:
    /// a clean round) and judging these earlier ones.
    Ok {
        issues: &'a [ReportedIssue],
        verdicts: &'a [Verdict],
    },
    /// The reviewer was stopped (killed, crashed, its tree not put back)
    /// after it wrote these issues and verdicts, which the fix goes on
    /// with; `why` it did not finish.
    Partial {
        why: &'a str,
        issues: &'a [ReportedIssue],
        verdicts: &'a [Verdict],
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
    /// follow-ups among them, and its verdicts on earlier issues
    /// ([`insert_verdict`]). A fix's range always starts at the commit
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
        let (verdict, failure, issues, verdicts) = match round {
            FixReviewRound::Ok { issues, verdicts } => ("ok", None, issues, verdicts),
            FixReviewRound::Partial {
                why,
                issues,
                verdicts,
            } => ("partial", Some(why), issues, verdicts),
            FixReviewRound::Failed(why) => ("failed", Some(why), &[][..], &[][..]),
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
        for v in verdicts {
            insert_verdict(&mut tx, round_id, v).await?;
        }
        tx.commit().await?;
        Ok(round_id)
    }

    /// Record `answers`, which job `job_id`'s fixer gave to issues of the
    /// review of `finding_id`'s fix chain that job `origin_job_id` began
    /// (issue id, answer), as answers to the review's newest round: the
    /// one whose issues it was sent. An answer to an issue of another
    /// review is dropped; one to an issue the fixer already answered for
    /// that round replaces it, as a fixer resumed mid-answer rewrites the
    /// whole file. Nothing is recorded before the review has a round.
    /// Returns how many were recorded. The job lane's write
    /// ([`JobStore::record_fix_review_answers`](super::JobStore::record_fix_review_answers)).
    pub(super) async fn record_fix_review_answers(
        &self,
        (finding_id, origin_job_id): (i64, i64),
        job_id: i64,
        answers: &[(i64, IssueAnswer)],
    ) -> sqlx::Result<usize> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let newest = sqlx::query!(
            r#"SELECT r.id AS "round_id!: i64", r.review_id AS "review_id!: i64"
               FROM review_rounds r JOIN reviews rv ON rv.id = r.review_id
               WHERE rv.subject = 'fix' AND rv.finding_id = ?1 AND rv.origin_job_id = ?2
               ORDER BY r.id DESC LIMIT 1"#,
            finding_id,
            origin_job_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(newest) = newest else {
            return Ok(0);
        };
        let (author, at) = (format!("job {job_id}"), now_ms());
        let mut recorded = 0;
        for (issue_id, a) in answers {
            sqlx::query!(
                "DELETE FROM review_answers \
                 WHERE issue_id = ?1 AND round_id = ?2 AND author_kind = 'fixer'",
                issue_id,
                newest.round_id
            )
            .execute(&mut *tx)
            .await?;
            let word = a.answer.as_str();
            let inserted = sqlx::query!(
                "INSERT INTO review_answers \
                     (issue_id, round_id, author_kind, author, answer, text, commit_sha, at) \
                 SELECT i.id, ?2, 'fixer', ?3, ?4, ?5, ?6, ?7 FROM review_issues i \
                 JOIN review_rounds r ON r.id = i.round_id \
                 WHERE i.id = ?1 AND r.review_id = ?8",
                issue_id,
                newest.round_id,
                author,
                word,
                a.text,
                a.commit,
                at,
                newest.review_id
            )
            .execute(&mut *tx)
            .await?;
            recorded += inserted.rows_affected() as usize;
        }
        tx.commit().await?;
        Ok(recorded)
    }

    /// The review of `finding_id`'s fix chain that job `origin_job_id`
    /// began, as its loop goes by it: its kept rounds, and every issue they
    /// reported with the fixer's answers and later rounds' verdicts. Empty
    /// before its first round is recorded.
    pub async fn fix_review(
        &self,
        (finding_id, origin_job_id): (i64, i64),
    ) -> sqlx::Result<FixReview> {
        let rounds = sqlx::query_scalar!(
            r#"SELECT r.id AS "id!: i64"
               FROM review_rounds r JOIN reviews rv ON rv.id = r.review_id
               WHERE rv.subject = 'fix' AND rv.finding_id = ?1 AND rv.origin_job_id = ?2
                 AND r.verdict != 'failed'
               ORDER BY r.id"#,
            finding_id,
            origin_job_id
        )
        .fetch_all(&self.pool)
        .await?;
        let issues = sqlx::query!(
            r#"SELECT i.id AS "id!: i64", i.round_id AS "round_id!: i64", i.kind,
                      i.blocking AS "blocking!: bool", i.severity, i.confidence, i.file,
                      i.line, i.side, i.summary, i.detail
               FROM review_issues i JOIN review_rounds r ON r.id = i.round_id
               JOIN reviews rv ON rv.id = r.review_id
               WHERE rv.subject = 'fix' AND rv.finding_id = ?1 AND rv.origin_job_id = ?2
               ORDER BY i.round_id, i.blocking DESC, i.id"#,
            finding_id,
            origin_job_id
        )
        .fetch_all(&self.pool)
        .await?;
        let answers = sqlx::query!(
            r#"SELECT a.issue_id AS "issue_id!: i64", a.answer, a.text, a.commit_sha
               FROM review_answers a JOIN review_issues i ON i.id = a.issue_id
               JOIN review_rounds r ON r.id = i.round_id
               JOIN reviews rv ON rv.id = r.review_id
               WHERE rv.subject = 'fix' AND rv.finding_id = ?1 AND rv.origin_job_id = ?2
                 AND a.author_kind = 'fixer'
               ORDER BY a.round_id, a.id"#,
            finding_id,
            origin_job_id
        )
        .fetch_all(&self.pool)
        .await?;
        let verdicts = sqlx::query!(
            r#"SELECT v.round_id AS "round_id!: i64", v.issue_id AS "issue_id!: i64",
                      v.verdict, v.note
               FROM review_verdicts v JOIN review_rounds r ON r.id = v.round_id
               JOIN reviews rv ON rv.id = r.review_id
               WHERE rv.subject = 'fix' AND rv.finding_id = ?1 AND rv.origin_job_id = ?2
               ORDER BY v.round_id"#,
            finding_id,
            origin_job_id
        )
        .fetch_all(&self.pool)
        .await?;
        let mut issues: Vec<ChainIssue> = issues
            .into_iter()
            .map(|i| ChainIssue {
                id: i.id,
                round_id: i.round_id,
                issue: ReportedIssue {
                    kind: i.kind,
                    blocking: i.blocking,
                    severity: i.severity,
                    confidence: i.confidence,
                    file: i.file,
                    line: i.line,
                    side: i.side,
                    summary: i.summary,
                    detail: i.detail,
                },
                answers: Vec::new(),
                verdicts: Vec::new(),
            })
            .collect();
        let at: std::collections::HashMap<i64, usize> =
            issues.iter().enumerate().map(|(n, i)| (i.id, n)).collect();
        for a in answers {
            let issue = at.get(&a.issue_id).and_then(|&n| issues.get_mut(n));
            if let (Some(issue), Some(answer)) = (issue, AnswerKind::parse(&a.answer)) {
                issue.answers.push(IssueAnswer {
                    answer,
                    text: a.text,
                    commit: a.commit_sha,
                });
            }
        }
        for v in verdicts {
            let issue = at.get(&v.issue_id).and_then(|&n| issues.get_mut(n));
            if let (Some(issue), Some(verdict)) = (issue, IssueVerdict::parse(&v.verdict)) {
                issue.verdicts.push(IssueJudged {
                    round_id: v.round_id,
                    verdict,
                    note: v.note,
                });
            }
        }
        Ok(FixReview { rounds, issues })
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
