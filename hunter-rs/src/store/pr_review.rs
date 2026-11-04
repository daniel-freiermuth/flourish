//! Requested reviews of pull requests hunter did not open (migration 034,
//! future.md §8.8): the operator's requests, the PR sync's view of each
//! PR, and the rounds the job lane ran.
//!
//! Who writes what: the operator requests, watches and retries (on
//! [`Store`]); the admin lane's PR sync records the PR's head, title and end
//! ([`AdminStore`](super::AdminStore)); the job lane records rounds
//! ([`JobStore`](super::JobStore)). The columns each writes are disjoint
//! but for `failures`/`held`, which a round counts and the operator clears.

use super::{ImmediateTx, Store};
use crate::domain::{ForgeName, PrReviewState};
use crate::types::{PrReviewIssueOut, PrReviewRoundOut};
use crate::util::now_ms;

/// How long a watched PR's head must stay unchanged before it is reviewed
/// again: a burst of pushes ("fix typo", "address review") is one round.
pub const HEAD_STABLE_MS: i64 = 10 * 60 * 1000;

/// Consecutive failed rounds after which a review is held for the
/// operator: something about the PR (or the forge) keeps failing, and
/// retrying every cycle would only spend budget on it.
pub const PR_REVIEW_HOLD_AT: i64 = 3;

/// What a request records about the PR, read from the forge at request
/// time.
#[derive(Debug, Clone)]
pub struct NewPrReview<'a> {
    pub repo_id: i64,
    pub pr_number: i64,
    pub title: &'a str,
    pub author: &'a str,
    pub base_ref: &'a str,
    pub cross_repo: bool,
    pub head_sha: &'a str,
    pub watch: bool,
    /// The author, when they cannot push and the requester vouched.
    pub vouched_author: Option<&'a str>,
    pub requested_by: &'a str,
}

/// A `pr_reviews` row with its repo's name, URL and forge.
#[derive(Debug, Clone)]
pub struct PrReviewRow {
    pub id: i64,
    pub repo_id: i64,
    pub repo_name: String,
    pub repo_url: String,
    pub forge: ForgeName,
    pub pr_number: i64,
    pub title: String,
    pub author: String,
    pub base_ref: String,
    pub cross_repo: bool,
    pub watch: bool,
    pub state: PrReviewState,
    pub head_sha: Option<String>,
    pub head_seen_at: Option<i64>,
    pub last_reviewed_sha: Option<String>,
    pub vouched_author: Option<String>,
    pub requested_by: String,
    pub requested_at: i64,
    pub failures: i64,
    pub last_failure: Option<String>,
    pub held: bool,
    /// The `running` `pr_review` job of this review, if any.
    pub running_job_id: Option<i64>,
}

/// How a round ended, as [`JobStore::record_pr_review_round`] records it.
///
/// [`JobStore::record_pr_review_round`]: super::JobStore::record_pr_review_round
#[derive(Debug, Clone, Copy)]
pub enum PrReviewRound<'a> {
    /// The reviewer reported these issues for `base..head`.
    Ok(&'a [PrReviewIssueOut]),
    /// No usable review came out of the attempt, for this reason.
    Failed(&'a str),
}

/// Raw `pr_reviews` columns as one SELECT returns them; every reader goes
/// through this list so a new column is added in one place.
macro_rules! select_reviews {
    ($tail:literal $(, $arg:expr)* $(,)?) => {
        sqlx::query!(
            r#"
            SELECT p.id AS "id!: i64", p.repo_id AS "repo_id!: i64",
                   r.name AS "repo_name!", r.url AS "repo_url!",
                   r.forge AS "forge!: ForgeName",
                   p.pr_number AS "pr_number!: i64", p.title, p.author, p.base_ref,
                   p.cross_repo AS "cross_repo!: bool", p.watch AS "watch!: bool",
                   p.state AS "state!: PrReviewState",
                   p.head_sha, p.head_seen_at, p.last_reviewed_sha, p.vouched_author,
                   p.requested_by, p.requested_at AS "requested_at!: i64",
                   p.failures AS "failures!: i64", p.last_failure,
                   p.held AS "held!: bool",
                   (SELECT j.id FROM jobs j
                    WHERE j.pr_review_id = p.id AND j.state = 'running') AS "running_job_id: i64"
            FROM pr_reviews p
            JOIN repos r ON r.id = p.repo_id
            WHERE r.deleted_at IS NULL
            "# + $tail
            $(, $arg)*
        )
    };
}

/// Map a [`select_reviews!`] row to a [`PrReviewRow`].
macro_rules! review_row {
    ($r:expr) => {{
        let r = $r;
        PrReviewRow {
            id: r.id,
            repo_id: r.repo_id,
            repo_name: r.repo_name,
            repo_url: r.repo_url,
            forge: r.forge,
            pr_number: r.pr_number,
            title: r.title,
            author: r.author,
            base_ref: r.base_ref,
            cross_repo: r.cross_repo,
            watch: r.watch,
            state: r.state,
            head_sha: r.head_sha,
            head_seen_at: r.head_seen_at,
            last_reviewed_sha: r.last_reviewed_sha,
            vouched_author: r.vouched_author,
            requested_by: r.requested_by,
            requested_at: r.requested_at,
            failures: r.failures,
            last_failure: r.last_failure,
            held: r.held,
            running_job_id: r.running_job_id,
        }
    }};
}

impl Store {
    /// Record the operator's request for a review of `new.pr_number`, or,
    /// when one exists, set its `watch` (and, turning it on, clear its
    /// failures as [`Self::set_pr_review_watch`] does). Nothing else of an
    /// existing review changes: a request never reopens an ended one or
    /// re-vouches. The first round is due at once (`last_reviewed_sha` is
    /// NULL, which [`Self::list_due_pr_reviews`] exempts from the
    /// stability wait). Returns the review's id.
    pub async fn request_pr_review(&self, new: &NewPrReview<'_>) -> sqlx::Result<i64> {
        let now = now_ms();
        sqlx::query_scalar!(
            r#"
            INSERT INTO pr_reviews (repo_id, pr_number, title, author, base_ref, cross_repo,
                                    watch, head_sha, head_seen_at, vouched_author,
                                    requested_by, requested_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?9)
            ON CONFLICT (repo_id, pr_number) DO UPDATE SET
                watch = excluded.watch,
                failures = CASE WHEN excluded.watch THEN 0 ELSE failures END,
                held = CASE WHEN excluded.watch THEN 0 ELSE held END
            RETURNING id AS "id!: i64"
            "#,
            new.repo_id,
            new.pr_number,
            new.title,
            new.author,
            new.base_ref,
            new.cross_repo,
            new.watch,
            new.head_sha,
            now,
            new.vouched_author,
            new.requested_by,
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Set review `id`'s `watch`. Turning it on also clears the failure
    /// streak and the hold: it is the operator saying "keep at it".
    /// `false` when there is no such review.
    pub async fn set_pr_review_watch(&self, id: i64, watch: bool) -> sqlx::Result<bool> {
        let r = sqlx::query!(
            "UPDATE pr_reviews SET watch = ?2, \
                 failures = CASE WHEN ?2 THEN 0 ELSE failures END, \
                 held = CASE WHEN ?2 THEN 0 ELSE held END \
             WHERE id = ?1",
            id,
            watch
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Clear review `id`'s failure streak and hold, so its next round is
    /// due again. `false` when there is no such review.
    pub async fn retry_pr_review(&self, id: i64) -> sqlx::Result<bool> {
        let r = sqlx::query!(
            "UPDATE pr_reviews SET failures = 0, held = 0 WHERE id = ?1",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Whether PR `pr_number` of repo `repo_id` is one hunter opened for a
    /// finding: those are hunter's own work, engaged and harvested by their
    /// own tiers, never reviewed as somebody else's change.
    pub async fn is_hunter_pr(&self, repo_id: i64, pr_number: i64) -> sqlx::Result<bool> {
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM pr_state p
               JOIN findings f ON f.id = p.finding_id
               WHERE f.repo_id = ?1 AND p.pr_number = ?2"#,
            repo_id,
            pr_number
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(n > 0)
    }

    /// Numbers of every PR hunter opened in repo `repo_id`
    /// ([`Self::is_hunter_pr`] for a whole list).
    pub async fn hunter_pr_numbers(&self, repo_id: i64) -> sqlx::Result<Vec<i64>> {
        sqlx::query_scalar!(
            r#"SELECT p.pr_number AS "n!: i64" FROM pr_state p
               JOIN findings f ON f.id = p.finding_id
               WHERE f.repo_id = ?1 AND p.pr_number IS NOT NULL"#,
            repo_id
        )
        .fetch_all(&self.pool)
        .await
    }

    /// The id of the review of PR `pr_number` of repo `repo_id`, if one
    /// was requested.
    pub async fn find_pr_review(&self, repo_id: i64, pr_number: i64) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM pr_reviews WHERE repo_id = ?1 AND pr_number = ?2"#,
            repo_id,
            pr_number
        )
        .fetch_optional(&self.pool)
        .await
    }

    /// One review by id, if its repo is not deleted.
    pub async fn get_pr_review(&self, id: i64) -> sqlx::Result<Option<PrReviewRow>> {
        let row = select_reviews!(" AND p.id = ?1", id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| review_row!(r)))
    }

    /// Every review, newest request first.
    pub async fn list_pr_reviews(&self) -> sqlx::Result<Vec<PrReviewRow>> {
        let rows = select_reviews!(" ORDER BY p.requested_at DESC, p.id DESC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| review_row!(r)).collect())
    }

    /// Reviews still following an open PR, for the PR sync.
    pub async fn open_pr_reviews(&self) -> sqlx::Result<Vec<PrReviewRow>> {
        let rows = select_reviews!(" AND p.state = 'open' ORDER BY p.id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| review_row!(r)).collect())
    }

    /// Reviews whose next round is due at `now`: open, not held, with a
    /// known head, and either never reviewed yet, or watched with a head
    /// that differs from the last reviewed one and has stayed put for
    /// [`HEAD_STABLE_MS`]. A disabled repo's reviews are due too: disabling
    /// stops a repo's scans, as it never stopped its findings' work, and a
    /// review is work a human asked for. Oldest request first.
    pub async fn list_due_pr_reviews(&self, now: i64) -> sqlx::Result<Vec<PrReviewRow>> {
        let stable_before = now - HEAD_STABLE_MS;
        let rows = select_reviews!(
            " AND p.state = 'open' AND p.held = 0
              AND p.head_sha IS NOT NULL
              AND (p.last_reviewed_sha IS NULL
                   OR (p.watch = 1 AND p.head_sha != p.last_reviewed_sha
                       AND p.head_seen_at <= ?1))
              ORDER BY p.requested_at, p.id",
            stable_before
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| review_row!(r)).collect())
    }

    /// Every round of every review, newest first, with its issues.
    /// Paired with the review's id.
    pub async fn list_pr_review_rounds(&self) -> sqlx::Result<Vec<(i64, PrReviewRoundOut)>> {
        let rounds = sqlx::query!(
            r#"
            SELECT id AS "id!: i64", review_id AS "review_id!: i64", job_id, base_sha,
                   head_sha, verdict, failure, finished_at AS "finished_at!: i64"
            FROM pr_review_rounds
            ORDER BY finished_at DESC, id DESC
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        let issues = sqlx::query!(
            r#"
            SELECT round_id AS "round_id!: i64", kind, blocking AS "blocking!: bool",
                   severity, confidence, file, line, summary, detail
            FROM pr_review_issues
            ORDER BY round_id, blocking DESC, id
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        let mut by_round: std::collections::HashMap<i64, Vec<PrReviewIssueOut>> =
            std::collections::HashMap::new();
        for i in issues {
            by_round
                .entry(i.round_id)
                .or_default()
                .push(PrReviewIssueOut {
                    kind: i.kind,
                    blocking: i.blocking,
                    severity: i.severity,
                    confidence: i.confidence,
                    file: i.file,
                    line: i.line,
                    summary: i.summary,
                    detail: i.detail,
                });
        }
        Ok(rounds
            .into_iter()
            .map(|r| {
                (
                    r.review_id,
                    PrReviewRoundOut {
                        id: r.id,
                        job_id: r.job_id,
                        base_sha: r.base_sha,
                        head_sha: r.head_sha,
                        verdict: r.verdict,
                        failure: r.failure,
                        finished_at: r.finished_at,
                        issues: by_round.remove(&r.id).unwrap_or_default(),
                    },
                )
            })
            .collect())
    }

    /// Issues of review `id`'s current incremental chain: every successful
    /// round since the newest one that reviewed from scratch (the first
    /// round, or one after a force-push), as each later round's base is
    /// the head its predecessor reviewed. That is what the next
    /// incremental round is told was already reported: a round that found
    /// nothing new does not make the chain forget what came before it.
    pub async fn chain_pr_review_issues(&self, id: i64) -> sqlx::Result<Vec<PrReviewIssueOut>> {
        let rounds = sqlx::query!(
            r#"
            SELECT id AS "id!: i64", base_sha, head_sha FROM pr_review_rounds
            WHERE review_id = ?1 AND verdict = 'ok'
            ORDER BY finished_at, id
            "#,
            id
        )
        .fetch_all(&self.pool)
        .await?;
        let mut chain: Vec<i64> = Vec::new();
        let mut prev_head: Option<&str> = None;
        for r in &rounds {
            if prev_head != Some(r.base_sha.as_str()) {
                chain.clear();
            }
            chain.push(r.id);
            prev_head = Some(&r.head_sha);
        }
        let rows = sqlx::query!(
            r#"
            SELECT i.round_id AS "round_id!: i64", i.kind, i.blocking AS "blocking!: bool",
                   i.severity, i.confidence, i.file, i.line, i.summary, i.detail
            FROM pr_review_issues i JOIN pr_review_rounds r ON r.id = i.round_id
            WHERE r.review_id = ?1
            ORDER BY r.finished_at, r.id, i.id
            "#,
            id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .filter(|i| chain.contains(&i.round_id))
            .map(|i| PrReviewIssueOut {
                kind: i.kind,
                blocking: i.blocking,
                severity: i.severity,
                confidence: i.confidence,
                file: i.file,
                line: i.line,
                summary: i.summary,
                detail: i.detail,
            })
            .collect())
    }

    /// The PR sync's view of review `id`'s PR: its head (a changed head
    /// restarts the stability wait), title and state. Only an open review
    /// is touched: `merged`/`closed` are terminal. The admin lane's write
    /// ([`AdminStore::observe_pr_review`](super::AdminStore::observe_pr_review)).
    pub(super) async fn observe_pr_review(
        &self,
        id: i64,
        head_sha: &str,
        title: &str,
        state: PrReviewState,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_reviews SET \
                 head_seen_at = CASE WHEN head_sha IS ?2 THEN head_seen_at ELSE ?5 END, \
                 head_sha = ?2, title = ?3, state = ?4 \
             WHERE id = ?1 AND state = 'open'",
            id,
            head_sha,
            title,
            state,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record one round of review `id` over `base..head`, by job `job_id`
    /// when one ran, in one transaction with its issues and the review's
    /// streak. A success moves `last_reviewed_sha` to `head` and ends the
    /// streak; a failure counts one and holds the review at
    /// [`PR_REVIEW_HOLD_AT`]. Returns the streak after this round. The job
    /// lane's write ([`JobStore::record_pr_review_round`](super::JobStore::record_pr_review_round)).
    pub(super) async fn record_pr_review_round(
        &self,
        id: i64,
        job_id: Option<i64>,
        (base, head): (&str, &str),
        round: PrReviewRound<'_>,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (verdict, failure) = match round {
            PrReviewRound::Ok(_) => ("ok", None),
            PrReviewRound::Failed(why) => ("failed", Some(why)),
        };
        let round_id = sqlx::query_scalar!(
            r#"INSERT INTO pr_review_rounds
                   (review_id, job_id, base_sha, head_sha, verdict, failure, finished_at)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
               RETURNING id AS "id!: i64""#,
            id,
            job_id,
            base,
            head,
            verdict,
            failure,
            now
        )
        .fetch_one(&mut *tx)
        .await?;
        let streak = match round {
            PrReviewRound::Ok(issues) => {
                for i in issues {
                    sqlx::query!(
                        "INSERT INTO pr_review_issues \
                             (round_id, kind, blocking, severity, confidence, file, line, \
                              summary, detail) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        round_id,
                        i.kind,
                        i.blocking,
                        i.severity,
                        i.confidence,
                        i.file,
                        i.line,
                        i.summary,
                        i.detail
                    )
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query!(
                    "UPDATE pr_reviews SET last_reviewed_sha = ?2, failures = 0, \
                         last_failure = NULL WHERE id = ?1",
                    id,
                    head
                )
                .execute(&mut *tx)
                .await?;
                0
            }
            PrReviewRound::Failed(why) => sqlx::query_scalar!(
                r#"UPDATE pr_reviews SET failures = failures + 1, last_failure = ?2,
                       held = (failures + 1 >= ?3)
                   WHERE id = ?1
                   RETURNING failures AS "failures!: i64""#,
                id,
                why,
                PR_REVIEW_HOLD_AT
            )
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0),
        };
        tx.commit().await?;
        Ok(streak)
    }
}
