//! Requested reviews of pull requests hunter did not open (migration 035,
//! future.md §8.8): the operator's requests, the PR sync's view of each
//! PR, and the rounds the job lane ran.
//!
//! Who writes what: the operator requests, watches and retries (on
//! [`Store`]); the admin lane's PR sync records the PR's head, title and end,
//! and whether a post may have left drafts of hunter's on the PR
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

/// The wait after a review's round failed before the next is due
/// ([`PrReviewRow::due_at`]), as long as the daemon's error backoff and
/// doubled for each failure in a row after the first. A failure is often
/// brief (the model backend overloaded, a forge's 5xx), and a round
/// retried at once would spend the rounds up to [`PR_REVIEW_HOLD_AT`] on
/// it back to back.
pub const PR_REVIEW_RETRY_MS: i64 = Duration::from_mins(5).as_millis() as i64;

/// Failed posts of an approved round after which it falls back to
/// `pending`: the forge keeps refusing it (the PR moved, a comment it
/// rejects), and a user should look before it is sent again.
pub const POST_GIVE_UP_AT: i64 = 3;

/// A user's decision on a pending round
/// ([`Store::decide_pr_review_post`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostDecision {
    /// Post it on the PR review duty's next run.
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

/// An `approved` round, for the PR review duty to post.
#[derive(Debug, Clone)]
pub struct ApprovedRound {
    pub id: i64,
    pub review_id: i64,
    /// The review's binding the round ran under: a post goes out only
    /// while it is still the review's ([`PrReviewRow::binding`]).
    pub binding: i64,
    pub base_sha: String,
    pub head_sha: String,
    /// The forge's id of the round's review once it went up: a post that
    /// failed afterwards, reading its inline comments' ids, retries only
    /// the read.
    pub review_ref: Option<String>,
    /// An earlier post sent the round's review (`review_sent_at`) and none
    /// is recorded: the send may have landed though its call failed, so
    /// the review is looked for on the PR before it is sent again.
    pub review_sent: bool,
    /// The round's base is not the merge-base it resolved
    /// (`merge_base_sha`): its LEFT lines number that base, and the PR's
    /// diff, whose old side is the merge-base, has other code at them.
    pub incremental: bool,
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
    /// What is on the forge of its inline comment.
    pub inline: InlineState,
}

/// What hunter knows of an issue's inline comment on the forge
/// ([`PostableIssue::inline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineState {
    /// Not posted inline: not posted yet, or in a review's body.
    None,
    /// On the forge (its review went up, or a post that failed partway
    /// put it there:
    /// [`AdminStore::record_pr_review_comments_posted`](super::AdminStore::record_pr_review_comments_posted)),
    /// but its forge id is not read back yet: the post reads it before the
    /// round counts as posted.
    Unread,
    /// On the forge, its forge id read back, or the comment known gone.
    Read,
}

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
    /// Post each round with issues without waiting for a user's approval.
    pub auto_post: bool,
    /// The owner of the fork the head lives in, as the request read it
    /// (`Some("")` when the forge names none); `None` for a head in the
    /// repo itself ([`crate::forge::PrMeta::fork_owner`]).
    pub head_owner: Option<&'a str>,
    /// What the request decided about running the head's code; `None`
    /// keeps what an earlier request of the review at its URL decided,
    /// about the same fork owner ([`PrReviewRow::decided_about`]).
    pub trust: Option<Trust<'a>>,
    pub requested_by: &'a str,
}

/// A request's decision about running a PR's head: whoever wrote the code
/// it runs -- the author, and the owner of the fork the head lives in --
/// can push to the repo, or a user vouched for them.
#[derive(Debug, Clone, Copy, Default)]
pub struct Trust<'a> {
    /// The author, when they cannot push and the requester vouched.
    pub vouched_author: Option<&'a str>,
    /// The fork's owner, when they are not the author, cannot push, and
    /// the requester vouched: running their repository's code vouches for
    /// whoever can push to it.
    pub vouched_head_owner: Option<&'a str>,
}

/// What [`Store::request_pr_review`] recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requested {
    pub id: i64,
    /// Whether the request waits for a round: no round reviewed the head
    /// it read yet. `false` for a review that is not open (a merged or own
    /// one at its URL), and for one whose head a round reviewed.
    pub asks_round: bool,
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
    pub auto_post: bool,
    pub state: PrReviewState,
    pub head_sha: Option<String>,
    pub head_seen_at: Option<i64>,
    pub last_reviewed_sha: Option<String>,
    /// When the request that waits for a finished round was made (epoch
    /// ms; [`Store::request_pr_review`]: the first, or any later one),
    /// and the head it read; `None` while none waits
    /// ([`PrReviewRow::due_at`]).
    pub round_requested_at: Option<i64>,
    pub requested_head_sha: Option<String>,
    pub vouched_author: Option<String>,
    /// The owner of the fork the head lives in, as the newest read of the
    /// PR reported it (`Some("")`: the forge named none); `None` for a
    /// head in the repo itself.
    pub head_owner: Option<String>,
    /// The fork owner the request decided about ([`Self::decided_about`]).
    pub decided_head_owner: Option<String>,
    /// The fork owner, when they are not the author, cannot push and a
    /// user vouched for them.
    pub vouched_head_owner: Option<String>,
    pub requested_by: String,
    pub requested_at: i64,
    /// Consecutive rounds that did not finish, the newest one's reason,
    /// and when it ended (epoch ms; [`Self::retry_at`]).
    pub failures: i64,
    pub last_failure: Option<String>,
    pub failed_at: Option<i64>,
    pub held: bool,
    /// When a post began that may have left drafts of hunter's on the PR
    /// (epoch ms; migration 036): the PR review duty deletes them
    /// ([`forge::Forge::delete_own_drafts`]) before the review posts
    /// anything again.
    pub drafts_left_at: Option<i64>,
    /// The `running` `pr_review` job of this review, if any.
    pub running_job_id: Option<i64>,
}

impl PrReviewRow {
    /// Whether the repo's URL no longer names the repository the review
    /// was requested under: the request decided whether the PR's author
    /// can push (or a user vouched for them) against that URL, and its PR
    /// number named a PR there. At another URL the number may name another
    /// PR, by an author nobody decided about, so a moved review does
    /// nothing -- no round, no read of the PR, no post -- until a request
    /// binds it to the new URL, deciding afresh. An edit of the URL's form
    /// alone ([`crate::forge::same_repo`]) moves nothing.
    pub fn repo_moved(&self) -> bool {
        !self.bound_to(&self.repo_url)
    }

    /// Whether the review was requested under `url`, the repo's URL as a
    /// caller read it, or another form of it ([`Self::repo_moved`]).
    pub fn bound_to(&self, url: &str) -> bool {
        crate::forge::same_repo(&self.bound_url, url)
    }

    /// Whether the review's request decided about running code from a
    /// head in `fork_owner`'s fork (`None`: a head in the repo itself), as
    /// a read of the PR reports it ([`crate::forge::PrMeta::fork_owner`]):
    /// a round runs the code of the repository the head lives in, so it
    /// runs only what somebody decided about. A head in the repo itself was
    /// pushed by somebody who can push; a fork's owner, and whoever they
    /// let push to it, was decided about by the request -- they can push,
    /// are the author, or a user vouched for them. Logins compare as the
    /// forges do, ignoring case.
    pub fn decided_about(&self, fork_owner: Option<&str>) -> bool {
        match (self.decided_head_owner.as_deref(), fork_owner) {
            (None, None) => true,
            (Some(decided), Some(owner)) => decided.eq_ignore_ascii_case(owner),
            _ => false,
        }
    }

    /// Whether the fork the head lives in changed hands since the request
    /// decided about its owner, as the newest read reported it
    /// ([`Self::decided_about`]): such a review does nothing -- no round
    /// runs the new owner's code -- until a request decides about them.
    pub fn fork_changed_hands(&self) -> bool {
        !self.decided_about(self.head_owner.as_deref())
    }

    /// When the review's next round is due (epoch ms), the one definition
    /// both the pick ([`Store::list_due_pr_reviews`]) and the job lane's
    /// sleep ([`Store::next_pr_review_due_at`]) read; `None` while nothing
    /// makes one due. Only an open review not held, at the URL it was
    /// requested under ([`Self::repo_moved`]), whose fork did not change
    /// hands ([`Self::fork_changed_hands`]), with a known head, has a round
    /// due: at once while a request waits for one
    /// ([`Self::round_requested_at`]) and the head is not the reviewed one,
    /// whatever `watch` says (a request is the operator asking for a round
    /// now); and, watched, once a head other than the reviewed one has
    /// stayed put for [`HEAD_STABLE_MS`].
    pub fn due_at(&self) -> Option<i64> {
        let due = self.wanted_at()?;
        Some(self.retry_at().map_or(due, |retry| due.max(retry)))
    }

    /// When a request or the PR wants the review's next round
    /// ([`Self::due_at`], before any backoff).
    fn wanted_at(&self) -> Option<i64> {
        if self.state != PrReviewState::Open
            || self.held
            || self.repo_moved()
            || self.fork_changed_hands()
        {
            return None;
        }
        let head = self.head_sha.as_deref()?;
        let reviewed = self.last_reviewed_sha.as_deref() == Some(head);
        if self.round_requested_at.is_some() && !reviewed {
            return Some(0);
        }
        if !self.watch || reviewed {
            return None;
        }
        self.head_seen_at.map(|at| at + HEAD_STABLE_MS)
    }

    /// When a review whose last rounds failed may run the next: the
    /// newest failure's end plus [`PR_REVIEW_RETRY_MS`], doubled for each
    /// failure in a row after the first. `None` while no failure counts:
    /// a finished round, a request, Retry and turning watch on clear them.
    pub fn retry_at(&self) -> Option<i64> {
        let failed_at = self.failed_at.filter(|_| self.failures > 0)?;
        let doublings = u32::try_from((self.failures - 1).clamp(0, 16)).unwrap_or(0);
        Some(failed_at.saturating_add(PR_REVIEW_RETRY_MS.saturating_mul(1 << doublings)))
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
                   p.auto_post AS "auto_post!: bool",
                   p.state AS "state!: PrReviewState",
                   p.head_sha, p.head_seen_at, p.last_reviewed_sha,
                   p.round_requested_at, p.requested_head_sha, p.vouched_author,
                   p.head_owner, p.decided_head_owner, p.vouched_head_owner,
                   p.requested_by, p.requested_at AS "requested_at!: i64",
                   p.failures AS "failures!: i64", p.last_failure, p.failed_at,
                   p.held AS "held!: bool", p.drafts_left_at,
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
            auto_post: r.auto_post,
            state: r.state,
            head_sha: r.head_sha,
            head_seen_at: r.head_seen_at,
            last_reviewed_sha: r.last_reviewed_sha,
            round_requested_at: r.round_requested_at,
            requested_head_sha: r.requested_head_sha,
            vouched_author: r.vouched_author,
            head_owner: r.head_owner,
            decided_head_owner: r.decided_head_owner,
            vouched_head_owner: r.vouched_head_owner,
            requested_by: r.requested_by,
            requested_at: r.requested_at,
            failures: r.failures,
            last_failure: r.last_failure,
            failed_at: r.failed_at,
            held: r.held,
            drafts_left_at: r.drafts_left_at,
            running_job_id: r.running_job_id,
        }
    }};
}

impl Store {
    /// Record the operator's request for a review of `new.pr_number`, read
    /// open on the forge just now at `new.repo_url`, or, when one exists,
    /// set its `watch` and `auto_post` (and, turning watch on, clear its
    /// failures as [`Self::set_pr_review_watch`] does). A request asks for
    /// a round at the head the PR has now: an `open` or `closed` review
    /// takes the head, fork owner and title `new` read (a changed head
    /// restarting the stability wait), and the request waits for a round
    /// as a new review's does, watched or not, without the failures and
    /// the hold of the rounds before, unless a round already reviewed that
    /// head. The request waiting is recorded with its time and the head it
    /// read ([`PrReviewRow::round_requested_at`]), which tell the rounds
    /// that answer it ([`Self::record_pr_review_round`]) from one that read
    /// the PR before it; a request that asks for nothing answers one still
    /// waiting, the PR being at a reviewed head. A `closed` review follows its PR again: the PR
    /// was reopened, so the review is too, without the failures of its old
    /// life. A review requested under another URL
    /// ([`PrReviewRow::repo_moved`]: another repository, not another form
    /// of the same one's URL) is bound to `new.repo_url` the same
    /// way, whatever its state, and takes what the request read and
    /// decided there -- author, base, fork, the trust decision, who asked
    /// -- since its number now names the PR at this URL: `merged` and
    /// `closed` were facts about the PR at the earlier URL, so the review
    /// is `open` again (the request read the PR here open; an `own` review
    /// never gets here, the request refusing any PR of the repo hunter
    /// opened). Its binding (`binding`) goes up by one, which starts
    /// the review afresh: every round so far was about the PR at the
    /// earlier URL, so none of them counts again -- none is posted (those
    /// with nothing on a PR yet are discarded), its issues are neither
    /// judged nor read again, and the next round, which the request waits
    /// for, reviews the PR from its merge-base. A request's trust decision
    /// (`new.trust`) replaces the review's; one without (a review at its
    /// URL whose fork has the owner decided about) keeps it, and a rebind
    /// without one decides nobody, so no round runs. Nothing else of an
    /// existing review at its own URL changes: its author is the PR's, and
    /// `merged` and `own` are final there (a merged PR is never open
    /// again, and hunter's own PRs are refused before this). A new
    /// review's first round is due at once (a waiting request, which
    /// [`PrReviewRow::due_at`] exempts from the stability wait). An empty
    /// `new.head_sha` (a fresh GitLab MR has no head commit yet) is
    /// recorded as no known head, which makes no round due until a read
    /// finds one; the request waits for that head's round.
    pub async fn request_pr_review(&self, new: &NewPrReview<'_>) -> sqlx::Result<Requested> {
        let now = now_ms();
        let trust = new.trust.unwrap_or_default();
        let decided = new.trust.is_some();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        // Bound to another repository: an edit of the URL's form alone is
        // no move ([`PrReviewRow::repo_moved`]).
        let moved = sqlx::query_scalar!(
            "SELECT repo_url FROM pr_reviews WHERE repo_id = ?1 AND pr_number = ?2",
            new.repo_id,
            new.pr_number
        )
        .fetch_optional(&mut *tx)
        .await?
        .is_some_and(|bound| !crate::forge::same_repo(&bound, new.repo_url));
        let id = sqlx::query_scalar!(
            r#"
            INSERT INTO pr_reviews (repo_id, pr_number, title, author, base_ref, cross_repo,
                                    watch, head_sha, head_seen_at, vouched_author,
                                    requested_by, requested_at, auto_post, repo_url,
                                    head_owner, decided_head_owner, vouched_head_owner)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULLIF(?8, ''), ?9, ?10, ?11, ?9, ?12, ?13,
                    ?14, CASE WHEN ?15 THEN ?14 END, ?16)
            ON CONFLICT (repo_id, pr_number) DO UPDATE SET
                watch = excluded.watch,
                auto_post = excluded.auto_post,
                failures = CASE WHEN excluded.watch OR state = 'closed' OR ?17
                                THEN 0 ELSE failures END,
                held = CASE WHEN excluded.watch OR state = 'closed' OR ?17
                            THEN 0 ELSE held END,
                title = CASE WHEN state IN ('open', 'closed') OR ?17
                             THEN excluded.title ELSE title END,
                head_seen_at = CASE WHEN (state IN ('open', 'closed') OR ?17)
                                         AND head_sha IS NOT excluded.head_sha
                                    THEN excluded.head_seen_at ELSE head_seen_at END,
                head_sha = CASE WHEN state IN ('open', 'closed') OR ?17
                                THEN excluded.head_sha ELSE head_sha END,
                head_owner = CASE WHEN state IN ('open', 'closed') OR ?17
                                  THEN excluded.head_owner ELSE head_owner END,
                last_reviewed_sha = CASE WHEN ?17 THEN NULL ELSE last_reviewed_sha END,
                binding = CASE WHEN ?17 THEN binding + 1 ELSE binding END,
                author = CASE WHEN ?17 THEN excluded.author ELSE author END,
                base_ref = CASE WHEN ?17 THEN excluded.base_ref ELSE base_ref END,
                cross_repo = CASE WHEN ?17 THEN excluded.cross_repo ELSE cross_repo END,
                vouched_author = CASE WHEN ?15 OR ?17
                                      THEN excluded.vouched_author ELSE vouched_author END,
                decided_head_owner = CASE WHEN ?15 OR ?17
                                          THEN excluded.decided_head_owner
                                          ELSE decided_head_owner END,
                vouched_head_owner = CASE WHEN ?15 OR ?17
                                          THEN excluded.vouched_head_owner
                                          ELSE vouched_head_owner END,
                requested_by = CASE WHEN ?17 THEN excluded.requested_by ELSE requested_by END,
                requested_at = CASE WHEN ?17 THEN excluded.requested_at ELSE requested_at END,
                repo_url = excluded.repo_url,
                state = CASE WHEN state = 'closed' OR ?17 THEN 'open' ELSE state END
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
            trust.vouched_author,
            new.requested_by,
            new.auto_post,
            new.repo_url,
            new.head_owner,
            decided,
            trust.vouched_head_owner,
            moved,
        )
        .fetch_one(&mut *tx)
        .await?;
        // An earlier binding's rounds still to post are about another PR:
        // never posted, here or anywhere. One part of which landed stays as
        // it is, what is on that PR being said; posting skips it all the
        // same ([`Self::approved_pr_review_rounds`]).
        sqlx::query!(
            "UPDATE pr_review_rounds SET post_state = 'discarded' \
             WHERE review_id = ?1 AND post_state IN ('pending', 'approved') \
               AND landed_at IS NULL \
               AND binding != (SELECT binding FROM pr_reviews WHERE id = ?1)",
            id
        )
        .execute(&mut *tx)
        .await?;
        let asks_round = record_request(&mut tx, id, now).await?;
        tx.commit().await?;
        Ok(Requested { id, asks_round })
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
    /// review's rounds are posted, and for a round of an earlier binding
    /// than the review's ([`PrReviewRow::binding`]), which is about another
    /// PR. Discarding is final, and refused once any part of the round
    /// reached the PR (`landed_at`, unmarked when the forge refused its
    /// review outright, [`Self::unmark_pr_review_sent`]): what is on the PR
    /// stays said, and only posting the rest makes the round whole.
    pub async fn decide_pr_review_post(
        &self,
        round_id: i64,
        decision: PostDecision,
    ) -> sqlx::Result<PostDecided> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let row = sqlx::query!(
            r#"SELECT r.post_state, r.landed_at IS NOT NULL AS "landed!: bool",
                      r.binding = p.binding AS "current!: bool",
                      p.state AS "state!: PrReviewState", p.id AS "review_id!: i64"
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
                    "the review is {}; only an open review's rounds are posted",
                    row.state.as_str()
                )));
            }
            PostDecision::Approve if !row.current => {
                return Ok(PostDecided::Refused(format!(
                    "round {round_id} is about the PR at the URL the review was bound to \
                     before; it is never posted"
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
            PostDecision::Discard if row.landed => {
                return Ok(PostDecided::Refused(format!(
                    "round {round_id} may be partly on the PR already; approve it to post the rest"
                )));
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
    /// PR review duty posts. Only rounds of the review's binding: an earlier
    /// one's are about another PR ([`Self::request_pr_review`]).
    pub async fn approved_pr_review_rounds(&self) -> sqlx::Result<Vec<ApprovedRound>> {
        let rows = sqlx::query!(
            r#"SELECT r.id AS "id!: i64", r.review_id AS "review_id!: i64",
                      r.binding AS "binding!: i64", r.base_sha,
                      r.head_sha, r.forge_review_ref,
                      r.review_sent_at IS NOT NULL AS "review_sent!: bool",
                      r.base_sha IS NOT COALESCE(r.merge_base_sha, r.base_sha)
                        AS "incremental!: bool"
               FROM pr_review_rounds r
               JOIN pr_reviews p ON p.id = r.review_id
               JOIN repos rp ON rp.id = p.repo_id
               WHERE r.post_state = 'approved' AND p.state = 'open'
                 AND r.binding = p.binding AND rp.deleted_at IS NULL
               ORDER BY r.finished_at, r.id"#
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ApprovedRound {
                id: r.id,
                review_id: r.review_id,
                binding: r.binding,
                base_sha: r.base_sha,
                head_sha: r.head_sha,
                review_ref: r.forge_review_ref,
                review_sent: r.review_sent,
                incremental: r.incremental,
            })
            .collect())
    }

    /// Round `round_id`'s issues as a post needs them, blocking first.
    pub async fn pr_review_round_issues(&self, round_id: i64) -> sqlx::Result<Vec<PostableIssue>> {
        let rows = sqlx::query!(
            r#"SELECT id AS "id!: i64", kind, blocking AS "blocking!: bool", severity, file,
                      line, side, summary, detail, inline AS "posted_inline!: bool",
                      inline AND thread_ref IS NULL AND thread_gone_at IS NULL
                        AS "unread_ref!: bool"
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
                inline: match (i.posted_inline, i.unread_ref) {
                    (false, _) => InlineState::None,
                    (true, true) => InlineState::Unread,
                    (true, false) => InlineState::Read,
                },
            })
            .collect())
    }

    /// Record that round `round_id`'s review is on the forge, with the
    /// forge's id of it and the issues it put `inline`. The round stays
    /// `approved` until its inline comments are read back too
    /// ([`Self::record_pr_review_round_posted`]), so a later failure
    /// retries only that. The admin lane's write
    /// ([`AdminStore::record_pr_review_review_posted`](super::AdminStore::record_pr_review_review_posted)).
    pub(super) async fn record_pr_review_review_posted(
        &self,
        round_id: i64,
        review_ref: &str,
        inline: &[i64],
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        sqlx::query!(
            "UPDATE pr_review_rounds SET forge_review_ref = ?2 \
             WHERE id = ?1 AND post_state = 'approved'",
            round_id,
            review_ref
        )
        .execute(&mut *tx)
        .await?;
        record_inline(&mut tx, round_id, inline).await?;
        mark_landed(&mut tx, round_id).await?;
        tx.commit().await
    }

    /// Record that the inline comments of round `round_id`'s issues
    /// `inline` (never none) are on the forge though its review is not: a
    /// GitLab post failed after publishing them, and the round landed. The
    /// retry leaves them out ([`PostableIssue::inline`]) rather than
    /// posting them again. The admin lane's write
    /// ([`AdminStore::record_pr_review_comments_posted`](super::AdminStore::record_pr_review_comments_posted)).
    pub(super) async fn record_pr_review_comments_posted(
        &self,
        round_id: i64,
        inline: &[i64],
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        record_inline(&mut tx, round_id, inline).await?;
        mark_landed(&mut tx, round_id).await?;
        tx.commit().await
    }

    /// Record what reading back the inline comments of round `round_id`
    /// found: the forge's id of each comment `found` (by issue), and the
    /// issues whose comment a read of every comment did not list, `gone`.
    /// The admin lane's write
    /// ([`AdminStore::record_pr_review_thread_refs`](super::AdminStore::record_pr_review_thread_refs)).
    pub(super) async fn record_pr_review_thread_refs(
        &self,
        round_id: i64,
        found: &[(i64, String)],
        gone: &[i64],
    ) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        for (issue, thread) in found {
            sqlx::query!(
                "UPDATE pr_review_issues SET thread_ref = ?3 WHERE id = ?1 AND round_id = ?2",
                issue,
                round_id,
                thread
            )
            .execute(&mut *tx)
            .await?;
        }
        mark_gone(&mut tx, gone).await?;
        tx.commit().await
    }

    /// Record the `approved` round `round_id` as posted: its review is on
    /// the forge and its inline comments' ids are read back. The admin
    /// lane's write
    /// ([`AdminStore::record_pr_review_round_posted`](super::AdminStore::record_pr_review_round_posted)).
    pub(super) async fn record_pr_review_round_posted(&self, round_id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_review_rounds SET post_state = 'posted', posted_at = ?2, \
                 post_error = NULL \
             WHERE id = ?1 AND post_state = 'approved'",
            round_id,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Record that a post of review `id` may leave drafts of hunter's on
    /// the PR from now on: set before the post creates any, so a failure
    /// or a daemon stopped mid-post leaves the mark behind with them. The
    /// admin lane's write
    /// ([`AdminStore::mark_pr_review_drafts`](super::AdminStore::mark_pr_review_drafts)).
    pub(super) async fn mark_pr_review_drafts(&self, id: i64) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_reviews SET drafts_left_at = COALESCE(drafts_left_at, ?2) WHERE id = ?1",
            id,
            now
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that a post is sending round `round_id`'s review: set before
    /// the forge call, which can fail after the forge took the review (a
    /// timeout, a 5xx after the write), so the next post looks for it on
    /// the PR before sending it again ([`ApprovedRound::review_sent`]),
    /// and the round may have landed from now on: it can no longer be
    /// discarded ([`Self::decide_pr_review_post`]), unless the forge
    /// refuses the call outright ([`Self::unmark_pr_review_sent`]). The
    /// admin lane's write
    /// ([`AdminStore::mark_pr_review_sent`](super::AdminStore::mark_pr_review_sent)).
    pub(super) async fn mark_pr_review_sent(&self, round_id: i64) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_review_rounds SET review_sent_at = ?2 WHERE id = ?1",
            round_id,
            now
        )
        .execute(&mut *tx)
        .await?;
        mark_landed(&mut tx, round_id).await?;
        tx.commit().await
    }

    /// Record that the forge refused round `round_id`'s review outright,
    /// putting none of it on the PR ([`crate::forge::NothingPosted`]):
    /// undo what [`Self::mark_pr_review_sent`] marked before the call. The
    /// next post sends the review without looking for it, and the round
    /// has not landed -- a user may discard it again once it is pending
    /// -- unless an inline comment of it is on the PR, published by an
    /// earlier GitLab post that failed partway. Nothing else of a round
    /// goes out before its review: its answers in threads follow it. An
    /// earlier send whose call failed left nothing either, since this one
    /// was made only after the look for what it left found no review
    /// (see `send_review` in the PR review duty). The admin lane's write
    /// ([`AdminStore::unmark_pr_review_sent`](super::AdminStore::unmark_pr_review_sent)).
    pub(super) async fn unmark_pr_review_sent(&self, round_id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_review_rounds SET review_sent_at = NULL, \
                 landed_at = CASE WHEN EXISTS (SELECT 1 FROM pr_review_issues \
                                               WHERE round_id = ?1 AND inline) \
                                  THEN landed_at END \
             WHERE id = ?1",
            round_id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record that a post is sending an answer giving round `round_id`'s
    /// verdict in a thread: set before the forge call, which can fail
    /// after the forge took the answer, so the round may have landed from
    /// now on, as a sent review makes it ([`Self::mark_pr_review_sent`]).
    /// The admin lane's write
    /// ([`AdminStore::mark_pr_review_answering`](super::AdminStore::mark_pr_review_answering)).
    pub(super) async fn mark_pr_review_answering(&self, round_id: i64) -> sqlx::Result<()> {
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        mark_landed(&mut tx, round_id).await?;
        tx.commit().await
    }

    /// Record that no draft of hunter's is left on review `id`'s PR: its
    /// post published them all, or a sweep deleted every one. The admin
    /// lane's write
    /// ([`AdminStore::clear_pr_review_drafts`](super::AdminStore::clear_pr_review_drafts)).
    pub(super) async fn clear_pr_review_drafts(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_reviews SET drafts_left_at = NULL WHERE id = ?1",
            id
        )
        .execute(&self.pool)
        .await?;
        Ok(())
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

    /// Reviews whose posts may have left drafts of hunter's on the PR
    /// ([`PrReviewRow::drafts_left_at`]), whatever their state: an ended
    /// PR's drafts are the operator's to publish all the same.
    pub async fn pr_reviews_with_drafts_left(&self) -> sqlx::Result<Vec<PrReviewRow>> {
        let rows = select_reviews!(" AND p.drafts_left_at IS NOT NULL ORDER BY p.id")
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
                   head_sha, verdict, failure, finished_at AS "finished_at!: i64",
                   post_state, post_error, posted_at, landed_at IS NOT NULL AS "landed!: bool"
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
                   inline AS "posted_inline!: bool"
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
                        landed: r.landed,
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
                   i.severity, i.confidence, i.file, i.line, i.side, i.summary, i.detail,
                   i.inline AS "posted_inline!: bool"
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
                side: i.side,
                summary: i.summary,
                detail: i.detail,
                posted_inline: i.posted_inline,
            })
            .collect())
    }

    /// The PR sync's view of review `id`'s PR: its head (a changed head
    /// restarts the stability wait; an empty one, a fresh GitLab MR's,
    /// is no known head), the owner of the fork it lives in (`None`: the
    /// repo itself; one the request did not decide about stops the review,
    /// [`PrReviewRow::fork_changed_hands`]), title and state. Only an open
    /// review is touched, the only kind the sync and a round read: a
    /// `closed` one follows its PR again once a request finds it reopened
    /// ([`Self::request_pr_review`]), and `merged` and `own` are final. A
    /// read made under another binding than the review's now (`binding`,
    /// as the reader read the review) was of another PR, and changes
    /// nothing. A head read is no request: what a request waits for stays
    /// as it is. The admin lane's write
    /// ([`AdminStore::observe_pr_review`](super::AdminStore::observe_pr_review)).
    pub(super) async fn observe_pr_review(
        &self,
        (id, binding): (i64, i64),
        (head_sha, head_owner): (&str, Option<&str>),
        title: &str,
        state: PrReviewState,
    ) -> sqlx::Result<()> {
        let now = now_ms();
        sqlx::query!(
            "UPDATE pr_reviews SET \
                 head_seen_at = CASE WHEN head_sha IS NULLIF(?2, '') THEN head_seen_at \
                                     ELSE ?5 END, \
                 head_sha = NULLIF(?2, ''), head_owner = ?7, title = ?3, state = ?4 \
             WHERE id = ?1 AND state = 'open' AND binding = ?6",
            id,
            head_sha,
            title,
            state,
            now,
            binding,
            head_owner
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
    /// when one ran, with the merge-base it resolved (`merge_base`; `None`
    /// for a round that failed before resolving it), in one transaction
    /// with its issues and the review's streak. The round ran under the review's `binding` as its pick read
    /// it; once a request bound the review to a new URL since, the round
    /// was about the PR at the earlier one, and is kept as history only: it
    /// moves nothing of the review below. A finished round (`Ok`) moves
    /// `last_reviewed_sha` to `head`, answers the request waiting
    /// ([`PrReviewRow::round_requested_at`]) when it is for `head` -- one
    /// the round took as it read the PR ([`Self::take_pr_review_request`]),
    /// or one made since that read the head the round reviewed -- and ends
    /// the streak: only a reviewer that finished reviewed `base..head`. A
    /// request made while the round ran, for a newer head, waits for that
    /// head's round; a head the PR sync read while the round ran is no
    /// request, and makes nothing wait. A `Partial` one keeps its issues, as
    /// a finished one does, but counts toward the hold as a failure does and leaves
    /// `last_reviewed_sha` where it was, so the next round reviews the same
    /// commits again; a reviewer whose budget never lets it finish them is
    /// held rather than run forever. A failure counts one and holds the
    /// review at [`PR_REVIEW_HOLD_AT`]. A kept round with issues is to be
    /// posted: it starts `approved` when the review posts automatically,
    /// `pending` otherwise, and `discarded` when it is of an earlier
    /// binding, which is never posted; any other round has nothing to
    /// post. Returns
    /// the streak after this round. The job lane's write
    /// ([`JobStore::record_pr_review_round`](super::JobStore::record_pr_review_round)).
    pub(super) async fn record_pr_review_round(
        &self,
        (id, binding): (i64, i64),
        job_id: Option<i64>,
        (base, head): (&str, &str),
        merge_base: Option<&str>,
        round: PrReviewRound<'_>,
    ) -> sqlx::Result<i64> {
        let now = now_ms();
        let mut tx = ImmediateTx::begin(&self.pool).await?;
        let (verdict, failure, has_issues) = match round {
            PrReviewRound::Ok(issues) => ("ok", None, !issues.is_empty()),
            PrReviewRound::Partial { issues, why } => ("partial", Some(why), !issues.is_empty()),
            PrReviewRound::Failed(why) => ("failed", Some(why), false),
        };
        let round_id = sqlx::query_scalar!(
            r#"INSERT INTO pr_review_rounds
                   (review_id, job_id, base_sha, head_sha, verdict, failure, finished_at,
                    post_state, merge_base_sha, binding)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7,
                       CASE WHEN ?8 THEN
                           CASE WHEN (SELECT binding FROM pr_reviews WHERE id = ?1) != ?10
                                THEN 'discarded'
                                WHEN (SELECT auto_post FROM pr_reviews WHERE id = ?1)
                                THEN 'approved' ELSE 'pending' END
                       END, ?9, ?10)
               RETURNING id AS "id!: i64""#,
            id,
            job_id,
            base,
            head,
            verdict,
            failure,
            now,
            has_issues,
            merge_base,
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
                    "UPDATE pr_reviews SET last_reviewed_sha = ?2, \
                         round_requested_at = CASE WHEN requested_head_sha IS ?2 \
                                                   THEN NULL ELSE round_requested_at END, \
                         requested_head_sha = CASE WHEN requested_head_sha IS ?2 \
                                                   THEN NULL ELSE requested_head_sha END, \
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
                           failed_at = ?5, held = (failures + 1 >= ?3)
                       WHERE id = ?1 AND binding = ?4
                       RETURNING failures AS "failures!: i64""#,
                id,
                why,
                PR_REVIEW_HOLD_AT,
                binding,
                now
            )
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(0),
        };
        tx.commit().await?;
        Ok(streak)
    }

    /// The round of review `id` (under `binding`, as its pick read it)
    /// that began reading the PR at `read_at` (epoch ms) and found `head`
    /// takes the request made before that read: it is a request for
    /// `head` from now on, which the round answers by reviewing it
    /// ([`Self::record_pr_review_round`]). What the round reviews is what
    /// the PR had when the request was made or later. A request made since
    /// the read began read the PR no earlier than the round did, and keeps
    /// the head it read: the round answers it only by reviewing that head.
    /// The job lane's write
    /// ([`JobStore::take_pr_review_request`](super::JobStore::take_pr_review_request)).
    pub(super) async fn take_pr_review_request(
        &self,
        (id, binding): (i64, i64),
        head: &str,
        read_at: i64,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            "UPDATE pr_reviews SET requested_head_sha = ?3 \
             WHERE id = ?1 AND binding = ?2 AND round_requested_at < ?4",
            id,
            binding,
            head,
            read_at
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

/// Record what the request made at `now` of open review `id`, as it now
/// stands, waits for ([`Store::request_pr_review`]): a round, unless one
/// reviewed its head. A request asking for one waits with the head it
/// read, without the failures and the hold of the rounds before; one
/// asking for none answers any request still waiting. A review that is not
/// open (merged or own at its URL) is over, and its request asks for
/// nothing. Returns whether it asks for a round.
async fn record_request(tx: &mut sqlx::SqliteConnection, id: i64, now: i64) -> sqlx::Result<bool> {
    let asks = sqlx::query_scalar!(
        r#"SELECT (p.head_sha IS NULL OR p.head_sha IS NOT p.last_reviewed_sha) AS "asks!: bool"
           FROM pr_reviews p WHERE p.id = ?1 AND p.state = 'open'"#,
        id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(asks) = asks else {
        return Ok(false);
    };
    sqlx::query!(
        "UPDATE pr_reviews SET round_requested_at = CASE WHEN ?2 THEN ?3 END, \
             requested_head_sha = CASE WHEN ?2 THEN head_sha END, \
             failures = CASE WHEN ?2 THEN 0 ELSE failures END, \
             held = CASE WHEN ?2 THEN 0 ELSE held END \
         WHERE id = ?1",
        id,
        asks,
        now
    )
    .execute(&mut *tx)
    .await?;
    Ok(asks)
}

/// Record the `issues` kept round `round_id` reported.
async fn record_issues(
    tx: &mut sqlx::SqliteConnection,
    round_id: i64,
    issues: &[PrReviewIssueOut],
) -> sqlx::Result<()> {
    for i in issues {
        // The column takes only the two words a forge knows; anything
        // else is the reviewer not saying.
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
    Ok(())
}

/// Mark the issues `inline` of round `round_id` as posted inline.
async fn record_inline(
    tx: &mut sqlx::SqliteConnection,
    round_id: i64,
    inline: &[i64],
) -> sqlx::Result<()> {
    for issue in inline {
        sqlx::query!(
            "UPDATE pr_review_issues SET inline = 1 WHERE id = ?1 AND round_id = ?2",
            issue,
            round_id
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Mark the threads of issues `gone` as gone from the PR, from now on.
async fn mark_gone(tx: &mut sqlx::SqliteConnection, gone: &[i64]) -> sqlx::Result<()> {
    let now = now_ms();
    for issue in gone {
        sqlx::query!(
            "UPDATE pr_review_issues SET thread_gone_at = COALESCE(thread_gone_at, ?2) \
             WHERE id = ?1",
            issue,
            now
        )
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Mark round `round_id` landed: part of it is on the PR, so it can no
/// longer be discarded.
async fn mark_landed(tx: &mut sqlx::SqliteConnection, round_id: i64) -> sqlx::Result<()> {
    let now = now_ms();
    sqlx::query!(
        "UPDATE pr_review_rounds SET landed_at = COALESCE(landed_at, ?2) WHERE id = ?1",
        round_id,
        now
    )
    .execute(&mut *tx)
    .await?;
    Ok(())
}
