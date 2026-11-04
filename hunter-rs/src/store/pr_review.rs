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

/// Failed posts of an approved round after which it falls back to
/// `pending`: the forge keeps refusing it (the PR moved, a comment it
/// rejects), and a user should look before it is sent again.
pub const POST_GIVE_UP_AT: i64 = 3;

/// A user's decision on a pending round
/// ([`Store::decide_pr_review_post`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostDecision {
    /// Post it on the next PR sync.
    Approve,
    /// Never post it.
    Discard,
}

/// What [`Store::decide_pr_review_post`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostDecided {
    /// The round moved; it belongs to this review.
    Moved {
        review_id: i64,
    },
    NoSuchRound,
    /// The round takes no decision now, for this reason.
    Refused(String),
}

/// An `approved` round, for the PR sync to post.
#[derive(Debug, Clone)]
pub struct ApprovedRound {
    pub id: i64,
    pub review_id: i64,
    pub base_sha: String,
    pub head_sha: String,
}

/// One issue of a round as a post needs it: with its id, to record the
/// inline comment it became.
#[derive(Debug, Clone)]
pub struct PostableIssue {
    pub id: i64,
    pub kind: String,
    pub blocking: bool,
    pub severity: Option<String>,
    pub file: Option<String>,
    pub line: Option<i64>,
    /// `line` numbers the base (code the PR removes), not the head.
    pub left: bool,
    pub summary: String,
    pub detail: String,
}

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
    /// Post each round with issues without waiting for a user's approval.
    pub auto_post: bool,
    /// The author, when they cannot push and the requester vouched.
    pub vouched_author: Option<&'a str>,
    pub requested_by: &'a str,
}

/// A `pr_reviews` row with its repo's name, URL and forge.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the table's independent flags, read as stored; an enum would invent states"
)]
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
    pub auto_post: bool,
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
                   p.auto_post AS "auto_post!: bool",
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
            auto_post: r.auto_post,
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
    /// when one exists, set its `watch` and `auto_post` (and, turning watch
    /// on, clear its failures as [`Self::set_pr_review_watch`] does).
    /// Nothing else of an existing review changes: a request never reopens
    /// an ended one or re-vouches. The first round is due at once
    /// (`last_reviewed_sha` is NULL, which [`Self::list_due_pr_reviews`]
    /// exempts from the stability wait). Returns the review's id.
    pub async fn request_pr_review(&self, new: &NewPrReview<'_>) -> sqlx::Result<i64> {
        let now = now_ms();
        sqlx::query_scalar!(
            r#"
            INSERT INTO pr_reviews (repo_id, pr_number, title, author, base_ref, cross_repo,
                                    watch, head_sha, head_seen_at, vouched_author,
                                    requested_by, requested_at, auto_post)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?9, ?12)
            ON CONFLICT (repo_id, pr_number) DO UPDATE SET
                watch = excluded.watch,
                auto_post = excluded.auto_post,
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
            new.auto_post,
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

    /// Set review `id`'s `auto_post`. Only rounds recorded from now on
    /// follow it: a pending round still waits for a user. `false` when
    /// there is no such review.
    pub async fn set_pr_review_auto_post(&self, id: i64, auto_post: bool) -> sqlx::Result<bool> {
        let r = sqlx::query!(
            "UPDATE pr_reviews SET auto_post = ?2 WHERE id = ?1",
            id,
            auto_post
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// A user's decision on round `round_id` of a review: only a `pending`
    /// round takes one. Approving clears the newest post error and the
    /// attempt count, so a round that fell back after failed posts is
    /// tried afresh; it is refused once the PR ended, since only an open
    /// review's rounds are posted. Discarding is final.
    pub async fn decide_pr_review_post(
        &self,
        round_id: i64,
        decision: PostDecision,
    ) -> sqlx::Result<PostDecided> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let row = sqlx::query!(
            r#"SELECT r.post_state, p.state AS "state!: PrReviewState", p.id AS "review_id!: i64"
               FROM pr_review_rounds r
               JOIN pr_reviews p ON p.id = r.review_id
               JOIN repos rp ON rp.id = p.repo_id
               WHERE r.id = ?1 AND rp.deleted_at IS NULL"#,
            round_id
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(PostDecided::NoSuchRound);
        };
        if row.post_state.as_deref() != Some("pending") {
            let now = row.post_state.as_deref().unwrap_or("nothing to post");
            return Ok(PostDecided::Refused(format!("round {round_id} is {now}")));
        }
        match decision {
            PostDecision::Approve if row.state != PrReviewState::Open => {
                return Ok(PostDecided::Refused(format!(
                    "the PR is {}; only an open PR's rounds are posted",
                    row.state.as_str()
                )));
            }
            PostDecision::Approve => {
                sqlx::query!(
                    "UPDATE pr_review_rounds SET post_state = 'approved', post_error = NULL, \
                         post_attempts = 0 WHERE id = ?1",
                    round_id
                )
                .execute(&mut *tx)
                .await?;
            }
            PostDecision::Discard => {
                sqlx::query!(
                    "UPDATE pr_review_rounds SET post_state = 'discarded' WHERE id = ?1",
                    round_id
                )
                .execute(&mut *tx)
                .await?;
            }
        }
        tx.commit().await?;
        Ok(PostDecided::Moved {
            review_id: row.review_id,
        })
    }

    /// Every `approved` round of an open review, oldest first: what the
    /// PR sync posts.
    pub async fn approved_pr_review_rounds(&self) -> sqlx::Result<Vec<ApprovedRound>> {
        let rows = sqlx::query!(
            r#"SELECT r.id AS "id!: i64", r.review_id AS "review_id!: i64", r.base_sha,
                      r.head_sha
               FROM pr_review_rounds r
               JOIN pr_reviews p ON p.id = r.review_id
               JOIN repos rp ON rp.id = p.repo_id
               WHERE r.post_state = 'approved' AND p.state = 'open'
                 AND rp.deleted_at IS NULL
               ORDER BY r.finished_at, r.id"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ApprovedRound {
                id: r.id,
                review_id: r.review_id,
                base_sha: r.base_sha,
                head_sha: r.head_sha,
            })
            .collect())
    }

    /// Round `round_id`'s issues as a post needs them, blocking first.
    pub async fn pr_review_round_issues(&self, round_id: i64) -> sqlx::Result<Vec<PostableIssue>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!: i64", kind, blocking AS "blocking!: bool", severity, file,
                      line, side, summary, detail
               FROM pr_review_issues WHERE round_id = ?1
               ORDER BY blocking DESC, id"#,
            round_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|i| PostableIssue {
                id: i.id,
                kind: i.kind,
                blocking: i.blocking,
                severity: i.severity,
                file: i.file,
                line: i.line,
                left: i.side.as_deref() == Some("LEFT"),
                summary: i.summary,
                detail: i.detail,
            })
            .collect())
    }

    /// Record round `round_id` as posted, with the forge's id of the
    /// review and of each issue's inline comment. Only an `approved` round
    /// moves. The admin lane's write
    /// ([`AdminStore::record_pr_review_posted`](super::AdminStore::record_pr_review_posted)).
    pub(super) async fn record_pr_review_posted(
        &self,
        round_id: i64,
        review_ref: &str,
        thread_refs: &[(i64, String)],
    ) -> sqlx::Result<()> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        sqlx::query!(
            "UPDATE pr_review_rounds SET post_state = 'posted', posted_at = ?2, \
                 forge_review_ref = ?3, post_error = NULL \
             WHERE id = ?1 AND post_state = 'approved'",
            round_id,
            now,
            review_ref
        )
        .execute(&mut *tx)
        .await?;
        for (issue, thread) in thread_refs {
            sqlx::query!(
                "UPDATE pr_review_issues SET thread_ref = ?3 WHERE id = ?1 AND round_id = ?2",
                issue,
                round_id,
                thread
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    }

    /// Count one failed post of the `approved` round `round_id`, keeping
    /// `why`; at [`POST_GIVE_UP_AT`] failures the round falls back to
    /// `pending`, for a user to decide. Returns the attempts counted, `0`
    /// when the round was not `approved`. The admin lane's write
    /// ([`AdminStore::record_pr_review_post_failure`](super::AdminStore::record_pr_review_post_failure)).
    pub(super) async fn record_pr_review_post_failure(
        &self,
        round_id: i64,
        why: &str,
    ) -> sqlx::Result<i64> {
        let attempts = sqlx::query_scalar!(
            r#"UPDATE pr_review_rounds SET post_attempts = post_attempts + 1, post_error = ?2,
                   post_state = CASE WHEN post_attempts + 1 >= ?3 THEN 'pending'
                                     ELSE post_state END
               WHERE id = ?1 AND post_state = 'approved'
               RETURNING post_attempts AS "attempts!: i64""#,
            round_id,
            why,
            POST_GIVE_UP_AT
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(attempts.unwrap_or(0))
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
                   head_sha, verdict, failure, finished_at AS "finished_at!: i64",
                   post_state, post_error, posted_at
            FROM pr_review_rounds
            ORDER BY finished_at DESC, id DESC
            "#
        )
        .fetch_all(&self.pool)
        .await?;
        let issues = sqlx::query!(
            r#"
            SELECT round_id AS "round_id!: i64", kind, blocking AS "blocking!: bool",
                   severity, confidence, file, line, side, summary, detail,
                   thread_ref IS NOT NULL AS "posted_inline!: bool"
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
                    side: i.side,
                    summary: i.summary,
                    detail: i.detail,
                    posted_inline: i.posted_inline,
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
                        post_state: r.post_state,
                        post_error: r.post_error,
                        posted_at: r.posted_at,
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
                   i.severity, i.confidence, i.file, i.line, i.side, i.summary, i.detail,
                   i.thread_ref IS NOT NULL AS "posted_inline!: bool"
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
                side: i.side,
                summary: i.summary,
                detail: i.detail,
                posted_inline: i.posted_inline,
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
    /// [`PR_REVIEW_HOLD_AT`]. A success with issues is to be posted: it
    /// starts `approved` when the review posts automatically, `pending`
    /// otherwise; any other round has nothing to post. Returns the streak
    /// after this round. The job lane's write
    /// ([`JobStore::record_pr_review_round`](super::JobStore::record_pr_review_round)).
    pub(super) async fn record_pr_review_round(
        &self,
        id: i64,
        job_id: Option<i64>,
        (base, head): (&str, &str),
        round: PrReviewRound<'_>,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (verdict, failure, has_issues) = match round {
            PrReviewRound::Ok(issues) => ("ok", None, !issues.is_empty()),
            PrReviewRound::Failed(why) => ("failed", Some(why), false),
        };
        let round_id = sqlx::query_scalar!(
            r#"INSERT INTO pr_review_rounds
                   (review_id, job_id, base_sha, head_sha, verdict, failure, finished_at,
                    post_state)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                       CASE WHEN ?8 THEN
                           CASE WHEN (SELECT auto_post FROM pr_reviews WHERE id = ?1)
                                THEN 'approved' ELSE 'pending' END
                       END)
               RETURNING id AS "id!: i64""#,
            id,
            job_id,
            base,
            head,
            verdict,
            failure,
            now,
            has_issues
        )
        .fetch_one(&mut *tx)
        .await?;
        let streak = match round {
            PrReviewRound::Ok(issues) => {
                for i in issues {
                    // The column takes only the two words a forge knows;
                    // anything else is the reviewer not saying.
                    let side = i
                        .side
                        .as_deref()
                        .map(str::to_ascii_uppercase)
                        .filter(|s| s == "RIGHT" || s == "LEFT");
                    sqlx::query!(
                        "INSERT INTO pr_review_issues \
                             (round_id, kind, blocking, severity, confidence, file, line, \
                              side, summary, detail) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        round_id,
                        i.kind,
                        i.blocking,
                        i.severity,
                        i.confidence,
                        i.file,
                        i.line,
                        side,
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
