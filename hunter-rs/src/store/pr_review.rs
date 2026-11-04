//! Requested reviews of pull requests hunter did not open (migration 035,
//! future.md §8.8): the operator's requests, the PR sync's view of each
//! PR, and the rounds the job lane ran.
//!
//! Who writes what: the operator requests, watches and retries (on
//! [`Store`]); the admin lane's PR sync records the PR's head, title and end
//! ([`AdminStore`](super::AdminStore)); the job lane records rounds
//! ([`JobStore`](super::JobStore)). The columns each writes are disjoint
//! but for `failures`/`held`, which a round counts and the operator clears.

use std::collections::HashSet;
use std::time::Duration;

use super::{ImmediateTx, Store};
use crate::domain::{ForgeName, PrReviewState};
use crate::forge;
use crate::types::{PrReviewIssueOut, PrReviewRoundOut};
use crate::util::now_ms;

/// How long a watched PR's head must stay unchanged before it is reviewed
/// again: a burst of pushes ("fix typo", "address review") is one round.
pub const HEAD_STABLE_MS: i64 = Duration::from_mins(10).as_millis() as i64;

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
    /// The repo's URL the PR was read at and the author's push access
    /// asked against: what the review is bound to.
    pub repo_url: &'a str,
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
#[allow(
    clippy::struct_excessive_bools,
    reason = "the table's independent flags, read as stored; an enum would invent states"
)]
pub struct PrReviewRow {
    pub id: i64,
    pub repo_id: i64,
    pub repo_name: String,
    /// The repo's URL now: what every forge call of the review goes to.
    pub repo_url: String,
    /// The repo's URL the review was requested under (migration 035's
    /// `pr_reviews.repo_url`): the one its trust decision was made against
    /// ([`Self::repo_moved`]).
    pub bound_url: String,
    /// The review's binding to `bound_url` (migration 035's
    /// `pr_reviews.binding`): 1 for its first request, one more for each
    /// request that bound it to a new URL. Only rounds of this binding are
    /// the review's ([`Store::request_pr_review`]).
    pub binding: i64,
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
    /// A request (the first, or one that reopened the review) waits for a
    /// finished round ([`PrReviewRow::due_at`]).
    pub round_requested: bool,
    pub vouched_author: Option<String>,
    pub requested_by: String,
    pub requested_at: i64,
    pub failures: i64,
    pub last_failure: Option<String>,
    pub held: bool,
    /// The `running` `pr_review` job of this review, if any.
    pub running_job_id: Option<i64>,
}

impl PrReviewRow {
    /// Whether the repo's URL is no longer the one the review was
    /// requested under: the request decided whether the PR's author can
    /// push (or a user vouched for them) against that URL, and its PR
    /// number named a PR there. At another URL the number may name another
    /// PR, by an author nobody decided about, so a moved review does
    /// nothing -- no round, no read of the PR, no post -- until a request
    /// binds it to the new URL, deciding afresh.
    pub fn repo_moved(&self) -> bool {
        !self.bound_to(&self.repo_url)
    }

    /// Whether the review was requested under `url`, the repo's URL as a
    /// caller read it ([`Self::repo_moved`]).
    pub fn bound_to(&self, url: &str) -> bool {
        self.bound_url == url
    }

    /// When the review's next round is due (epoch ms), the one definition
    /// both the pick ([`Store::list_due_pr_reviews`]) and the job lane's
    /// sleep ([`Store::next_pr_review_due_at`]) read; `None` while nothing
    /// makes one due. Only an open review not held, at the URL it was
    /// requested under ([`Self::repo_moved`]), with a known head, has
    /// a round due: at once while a request waits for one and the head is
    /// not the reviewed one, whatever `watch` says (a request is the
    /// operator asking for a round now); and, watched, once a head other
    /// than the reviewed one has stayed put for [`HEAD_STABLE_MS`].
    pub fn due_at(&self) -> Option<i64> {
        if self.state != PrReviewState::Open || self.held || self.repo_moved() {
            return None;
        }
        let head = self.head_sha.as_deref()?;
        let reviewed = self.last_reviewed_sha.as_deref() == Some(head);
        if self.round_requested && !reviewed {
            return Some(0);
        }
        if !self.watch || reviewed {
            return None;
        }
        self.head_seen_at.map(|at| at + HEAD_STABLE_MS)
    }
}

/// How a round ended, as [`JobStore::record_pr_review_round`] records it.
///
/// [`JobStore::record_pr_review_round`]: super::JobStore::record_pr_review_round
#[derive(Debug, Clone, Copy)]
pub enum PrReviewRound<'a> {
    /// The reviewer finished `base..head` and reported these issues.
    Ok(&'a [PrReviewIssueOut]),
    /// The reviewer was stopped before it finished, for reason `why`,
    /// after it wrote these issues: they are kept as an `Ok` round's are,
    /// but `base..head` is not reviewed.
    Partial {
        issues: &'a [PrReviewIssueOut],
        why: &'a str,
    },
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
                   r.name AS "repo_name!", r.url AS "repo_url!", p.repo_url AS "bound_url!",
                   p.binding AS "binding!: i64",
                   r.forge AS "forge!: ForgeName",
                   p.pr_number AS "pr_number!: i64", p.title, p.author, p.base_ref,
                   p.cross_repo AS "cross_repo!: bool", p.watch AS "watch!: bool",
                   p.state AS "state!: PrReviewState",
                   p.head_sha, p.head_seen_at, p.last_reviewed_sha,
                   p.round_requested AS "round_requested!: bool", p.vouched_author,
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
            bound_url: r.bound_url,
            binding: r.binding,
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
            round_requested: r.round_requested,
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
    /// Record the operator's request for a review of `new.pr_number`, read
    /// open on the forge just now at `new.repo_url`, or, when one exists,
    /// set its `watch` (and, turning it on, clear its failures as
    /// [`Self::set_pr_review_watch`] does). A `closed` review follows its
    /// PR again: the PR was reopened, so the review is too, at the head
    /// and title `new` read (a changed head restarting the stability wait)
    /// and without the failures of its old life, and the request waits
    /// for a round as a new review's does, watched or not, unless a round
    /// already reviewed that head. A review requested under another URL
    /// ([`PrReviewRow::repo_moved`]) is bound to `new.repo_url` the same
    /// way, whatever its state, and takes what the request read and
    /// decided there -- author, base, fork, the vouch, who asked -- since
    /// its number now names the PR at this URL: `merged`, `closed` and
    /// `own` were facts about the PR at the earlier URL, so the review is
    /// `open` again (the request read the PR here open, and refuses one
    /// hunter opened). Its binding (`binding`) goes up by one, which starts
    /// the review afresh: every round so far was about the PR at the
    /// earlier URL, so none of them counts again -- the next round, which
    /// the request waits for, reviews the PR from its merge-base and is
    /// told none of their issues. Nothing else of an existing review at
    /// its own URL changes: its
    /// author is the PR's, so the vouch given for them stands, and
    /// `merged` and `own` are final there (a merged PR is never open again,
    /// and hunter's own PRs are refused before this). A new review's first
    /// round is due at once (`round_requested`, which
    /// [`PrReviewRow::due_at`] exempts from the stability wait). An empty
    /// `new.head_sha` (a fresh GitLab MR has no head commit yet) is
    /// recorded as no known head, which makes no round due. Returns the
    /// review's id.
    pub async fn request_pr_review(&self, new: &NewPrReview<'_>) -> sqlx::Result<i64> {
        let now = now_ms();
        sqlx::query_scalar!(
            r#"
            INSERT INTO pr_reviews (repo_id, pr_number, title, author, base_ref, cross_repo,
                                    watch, head_sha, head_seen_at, vouched_author,
                                    requested_by, requested_at, repo_url)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULLIF(?8, ''), ?9, ?10, ?11, ?9, ?12)
            ON CONFLICT (repo_id, pr_number) DO UPDATE SET
                watch = excluded.watch,
                failures = CASE WHEN excluded.watch OR state = 'closed'
                                     OR repo_url IS NOT excluded.repo_url
                                THEN 0 ELSE failures END,
                held = CASE WHEN excluded.watch OR state = 'closed'
                                 OR repo_url IS NOT excluded.repo_url
                            THEN 0 ELSE held END,
                title = CASE WHEN state = 'closed' OR repo_url IS NOT excluded.repo_url
                             THEN excluded.title ELSE title END,
                head_seen_at = CASE WHEN (state = 'closed' OR repo_url IS NOT excluded.repo_url)
                                         AND head_sha IS NOT excluded.head_sha
                                    THEN excluded.head_seen_at ELSE head_seen_at END,
                head_sha = CASE WHEN state = 'closed' OR repo_url IS NOT excluded.repo_url
                                THEN excluded.head_sha ELSE head_sha END,
                round_requested = CASE WHEN repo_url IS NOT excluded.repo_url THEN 1
                                       WHEN state = 'closed'
                                       THEN excluded.head_sha IS NOT last_reviewed_sha
                                       ELSE round_requested END,
                last_reviewed_sha = CASE WHEN repo_url IS NOT excluded.repo_url
                                         THEN NULL ELSE last_reviewed_sha END,
                binding = CASE WHEN repo_url IS NOT excluded.repo_url
                               THEN binding + 1 ELSE binding END,
                author = CASE WHEN repo_url IS NOT excluded.repo_url
                              THEN excluded.author ELSE author END,
                base_ref = CASE WHEN repo_url IS NOT excluded.repo_url
                                THEN excluded.base_ref ELSE base_ref END,
                cross_repo = CASE WHEN repo_url IS NOT excluded.repo_url
                                  THEN excluded.cross_repo ELSE cross_repo END,
                vouched_author = CASE WHEN repo_url IS NOT excluded.repo_url
                                      THEN excluded.vouched_author ELSE vouched_author END,
                requested_by = CASE WHEN repo_url IS NOT excluded.repo_url
                                    THEN excluded.requested_by ELSE requested_by END,
                requested_at = CASE WHEN repo_url IS NOT excluded.repo_url
                                    THEN excluded.requested_at ELSE requested_at END,
                repo_url = excluded.repo_url,
                state = CASE WHEN state = 'closed' OR repo_url IS NOT excluded.repo_url
                             THEN 'open' ELSE state END
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
            new.repo_url,
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
        Ok(self.hunter_pr_numbers(repo_id).await?.contains(&pr_number))
    }

    /// Numbers of every PR hunter opened in repo `repo_id`: each finding's
    /// PR as shipping it recorded it (`pr_url`, written as the PR is
    /// opened) and as the PR sync last saw it (`pr_state.pr_number`). The
    /// URL is the record that exists from the moment the PR does; the sync
    /// fills the number in only on its next pass.
    pub async fn hunter_pr_numbers(&self, repo_id: i64) -> sqlx::Result<HashSet<i64>> {
        let rows = sqlx::query!(
            r#"SELECT r.forge AS "forge!: ForgeName", f.pr_url, p.pr_number
               FROM findings f
               JOIN repos r ON r.id = f.repo_id
               LEFT JOIN pr_state p ON p.finding_id = f.id
               WHERE f.repo_id = ?1 AND (f.pr_url IS NOT NULL OR p.pr_number IS NOT NULL)"#,
            repo_id
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .flat_map(|r| {
                let shipped = r
                    .pr_url
                    .as_deref()
                    .and_then(|url| forge::forge_for(r.forge).parse_pr_url(url))
                    .map(|(_, n)| n);
                shipped.into_iter().chain(r.pr_number)
            })
            .collect())
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

    /// Reviews still following an open PR, for the PR review duty.
    pub async fn open_pr_reviews(&self) -> sqlx::Result<Vec<PrReviewRow>> {
        let rows = select_reviews!(" AND p.state = 'open' ORDER BY p.id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|r| review_row!(r)).collect())
    }

    /// Reviews whose next round is due at `now` ([`PrReviewRow::due_at`]).
    /// A disabled repo's reviews are due too: disabling stops a repo's
    /// scans, as it never stopped its findings' work, and a review is work
    /// a human asked for. Oldest request first.
    pub async fn list_due_pr_reviews(&self, now: i64) -> sqlx::Result<Vec<PrReviewRow>> {
        let rows = select_reviews!(" AND p.state = 'open' ORDER BY p.requested_at, p.id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| review_row!(r))
            .filter(|r| r.due_at().is_some_and(|at| at <= now))
            .collect())
    }

    /// When the next review not due at `now` becomes due by the clock
    /// alone (epoch ms; [`PrReviewRow::due_at`]): a watched head settling,
    /// a reply going quiet. Nothing else wakes the job lane for it, so its
    /// idle sleep ends there. `None` when no review is waiting for a time.
    pub async fn next_pr_review_due_at(&self, now: i64) -> sqlx::Result<Option<i64>> {
        Ok(self
            .open_pr_reviews()
            .await?
            .iter()
            .filter_map(PrReviewRow::due_at)
            .filter(|&at| at > now)
            .min())
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

    /// Issues of review `id`'s incremental chain that a round from `base`
    /// continues: every kept round since the newest one that reviewed from
    /// scratch (the first round, or one after a force-push), as each later
    /// round's base is the head its predecessor reviewed. That is what the
    /// round is told was already reported: a round that found nothing new
    /// does not make the chain forget what came before it. A `partial`
    /// round is in the chain from its base, but reviewed nothing up to its
    /// head: the chain goes on from its base, which the next round reviews
    /// from again. A round from anywhere else starts a new chain, and is
    /// told of none.
    /// Only rounds of the review's binding are in a chain: an earlier
    /// binding's are about another PR ([`Self::request_pr_review`]).
    pub async fn chain_pr_review_issues(
        &self,
        id: i64,
        base: &str,
    ) -> sqlx::Result<Vec<PrReviewIssueOut>> {
        let rounds = sqlx::query!(
            r#"
            SELECT id AS "id!: i64", base_sha, head_sha, verdict = 'ok' AS "finished!: bool"
            FROM pr_review_rounds
            WHERE review_id = ?1 AND verdict != 'failed'
              AND binding = (SELECT binding FROM pr_reviews WHERE id = ?1)
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
            prev_head = Some(if r.finished { &r.head_sha } else { &r.base_sha });
        }
        if prev_head != Some(base) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query!(
            r#"
            SELECT i.round_id AS "round_id!: i64", i.kind, i.blocking AS "blocking!: bool",
                   i.severity, i.confidence, i.file, i.line, i.summary, i.detail
            FROM pr_review_issues i JOIN pr_review_rounds r ON r.id = i.round_id
            WHERE r.review_id = ?1
              AND r.binding = (SELECT binding FROM pr_reviews WHERE id = ?1)
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
    /// restarts the stability wait; an empty one, a fresh GitLab MR's,
    /// is no known head), title and state. Only an open review
    /// is touched, the only kind the sync and a round read: a `closed` one
    /// follows its PR again once a request finds it reopened
    /// ([`Self::request_pr_review`]), and `merged` and `own` are final. A
    /// read made under another binding than the review's now (`binding`,
    /// as the reader read the review) was of another PR, and changes
    /// nothing. The admin lane's write
    /// ([`AdminStore::observe_pr_review`](super::AdminStore::observe_pr_review)).
    pub(super) async fn observe_pr_review(
        &self,
        (id, binding): (i64, i64),
        head_sha: &str,
        title: &str,
        state: PrReviewState,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_reviews SET \
                 head_seen_at = CASE WHEN head_sha IS NULLIF(?2, '') THEN head_seen_at \
                                     ELSE ?5 END, \
                 head_sha = NULLIF(?2, ''), title = ?3, state = ?4 \
             WHERE id = ?1 AND state = 'open' AND binding = ?6",
            id,
            head_sha,
            title,
            state,
            now,
            binding
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// End open review `id` for good as `own`: its PR turned out to be one
    /// hunter opened ([`Self::is_hunter_pr`]), which only its own tiers
    /// work on. A request refuses such a PR, but one hunter opened after
    /// the request is found only by a later read; from then on no lane
    /// touches the review (each acts only on open ones). Returns whether
    /// the review was open. Both lanes' write, as each finds it
    /// ([`AdminStore::end_own_pr_review`](super::AdminStore::end_own_pr_review),
    /// [`JobStore::end_own_pr_review`](super::JobStore::end_own_pr_review)).
    pub(super) async fn end_own_pr_review(&self, id: i64) -> sqlx::Result<bool> {
        let r = sqlx::query!(
            "UPDATE pr_reviews SET state = 'own' WHERE id = ?1 AND state = 'open'",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() > 0)
    }

    /// Record one round of review `id` over `base..head`, by job `job_id`
    /// when one ran, in one transaction with its issues and the review's
    /// streak. The round ran under the review's `binding` as its pick read
    /// it; once a request bound the review to a new URL since, the round
    /// was about the PR at the earlier one, and is kept as history only: it
    /// moves nothing of the review below. A finished round (`Ok`) moves
    /// `last_reviewed_sha` to `head`,
    /// answers the request a round waited for (`round_requested`) and ends
    /// the streak: only a reviewer that finished reviewed
    /// `base..head`. A `Partial` one keeps its issues, as a finished one
    /// does, but counts toward the hold as a failure does and leaves
    /// `last_reviewed_sha` where it was, so the next round reviews the same
    /// commits again; a reviewer whose budget never lets it finish them is
    /// held rather than run forever. A failure counts one and holds the
    /// review at [`PR_REVIEW_HOLD_AT`]. Returns the streak after this
    /// round. The job lane's write
    /// ([`JobStore::record_pr_review_round`](super::JobStore::record_pr_review_round)).
    pub(super) async fn record_pr_review_round(
        &self,
        (id, binding): (i64, i64),
        job_id: Option<i64>,
        (base, head): (&str, &str),
        round: PrReviewRound<'_>,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (verdict, failure) = match round {
            PrReviewRound::Ok(_) => ("ok", None),
            PrReviewRound::Partial { why, .. } => ("partial", Some(why)),
            PrReviewRound::Failed(why) => ("failed", Some(why)),
        };
        let round_id = sqlx::query_scalar!(
            r#"INSERT INTO pr_review_rounds
                   (review_id, job_id, base_sha, head_sha, verdict, failure, finished_at,
                    binding)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
               RETURNING id AS "id!: i64""#,
            id,
            job_id,
            base,
            head,
            verdict,
            failure,
            now,
            binding
        )
        .fetch_one(&mut *tx)
        .await?;
        if let PrReviewRound::Ok(issues) | PrReviewRound::Partial { issues, .. } = round {
            record_issues(&mut tx, round_id, issues).await?;
        }
        let streak = match round {
            PrReviewRound::Ok(_) => {
                sqlx::query!(
                    "UPDATE pr_reviews SET last_reviewed_sha = ?2, round_requested = 0, \
                         failures = 0, last_failure = NULL WHERE id = ?1 AND binding = ?3",
                    id,
                    head,
                    binding
                )
                .execute(&mut *tx)
                .await?;
                0
            }
            PrReviewRound::Partial { why, .. } | PrReviewRound::Failed(why) => sqlx::query_scalar!(
                r#"UPDATE pr_reviews SET failures = failures + 1, last_failure = ?2,
                           held = (failures + 1 >= ?3)
                       WHERE id = ?1 AND binding = ?4
                       RETURNING failures AS "failures!: i64""#,
                id,
                why,
                PR_REVIEW_HOLD_AT,
                binding
            )
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0),
        };
        tx.commit().await?;
        Ok(streak)
    }
}

/// Record the `issues` kept round `round_id` reported.
async fn record_issues(
    tx: &mut sqlx::SqliteConnection,
    round_id: i64,
    issues: &[PrReviewIssueOut],
) -> sqlx::Result<()> {
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
    Ok(())
}
