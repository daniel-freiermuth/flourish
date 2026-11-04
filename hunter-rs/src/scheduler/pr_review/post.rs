//! Posting approved review rounds to the forge (future.md §8.8, slices 2
//! and 3), on the admin lane's PR review duty right after the reviews
//! were refreshed ([`super::sync_pr_reviews`]): a post spends no tokens,
//! only forge calls, like the rest of that duty. Not on the PR sync, whose
//! passes held engages wait for (`crate::daemon::EngageWaits`).
//!
//! A round goes out as one COMMENT review under the operator's forge
//! login: each issue whose line is in a hunk of the PR's diff
//! ([`place`](super::place)) as an inline comment, the rest in the body.
//! The inline comments' forge ids are then read back from the PR: they
//! are what each issue's thread is found by from then on. Then every
//! thread of an issue the round judged or reported is made to show the
//! issue's newest released judgement
//! ([`Store::pr_review_round_threads`](crate::store::Store::pr_review_round_threads)):
//! answered and resolved when it is addressed or rebutted, answered and
//! unresolved when it is open again, answered where somebody replied, and
//! left alone where it shows that already, or where news came in that the
//! verdict's round was not shown. Each thread is found afresh,
//! by its first comment, on one read of every thread of the PR: one a
//! human deleted is recorded gone and left out, never a reason to fail
//! the rest. Each step is recorded as it lands, so a post that failed
//! partway retries only what is missing, and two posts never say the same
//! thing twice. A step the forge took whose recording failed is held
//! ([`Landed`]) and recorded on the next sync instead of being taken
//! again. A step whose forge call failed may have landed all the same (a
//! timeout, a 5xx after the write), so a retry reads before it takes one
//! again: the review and each answer carry a step mark naming their round
//! ([`forge::step_mark`]), and the review an earlier post sent is looked
//! for on the PR ([`send_review`]), an answer in the thread read anyway,
//! and a resolution is set only where that read shows the thread
//! otherwise ([`settle_thread`]). What either finds is recorded instead.
//! For the same reason each call is marked before it is made: the round
//! whose review or answer it sends may be on the PR from then on, and is
//! never discarded; the resolution it sets out to make is hunter's, so a
//! read that finds the thread so never takes it for a human's reopen.
//!
//! A GitLab post is draft notes of the operator's login published one by
//! one, and a post that failed or was stopped leaves the rest as drafts the
//! operator's next review by hand would publish under their name. So a
//! post marks its review before it creates any, and a marked review's
//! drafts of hunter's (each carries the own-comment mark) are deleted
//! before it posts anything again, by every sync until that succeeds
//! ([`delete_left_drafts`]).

use std::collections::HashMap;

use super::EVENT;
use super::place;
use crate::config::Config;
use crate::forge::{self, InlineComment, PostedReview, ReviewPost, Side};
use crate::store::{
    AdminStore, ApprovedRound, InlineState, IssueVerdict, POST_GIVE_UP_AT, PostableIssue,
    PrReviewRow, Reply, ThreadToSettle, ThreadsRead,
};

/// Post every approved round of an open review ([`ApprovedRound`]),
/// oldest first, after deleting the drafts earlier posts left
/// ([`delete_left_drafts`]). A post that fails counts one attempt; at
/// [`POST_GIVE_UP_AT`] the round falls back to `pending` for a user. One
/// that failed while the forge is unreachable (the network gate's probe)
/// counts nothing: the next sync tries again. Stops at the next round once
/// `stopping` says so. Returns how many rounds were posted.
pub async fn post_pr_reviews(
    store: &AdminStore,
    cfg: &Config,
    stopping: impl Fn() -> bool,
) -> usize {
    match store.pr_reviews_with_drafts_left().await {
        Ok(left) => {
            for review in left.iter().filter(|r| !r.repo_moved()) {
                delete_left_drafts(store, review).await;
            }
        }
        Err(e) => {
            let msg = format!("pr_review post: listing reviews with drafts left: {e}");
            let _ = store.log_event("error", &msg, None, None).await;
        }
    }
    let rounds = match store.approved_pr_review_rounds().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("pr_review post: listing approved rounds: {e}");
            let _ = store.log_event("error", &msg, None, None).await;
            return 0;
        }
    };
    let mut posted = 0;
    for round in rounds {
        if stopping() {
            break;
        }
        if post_round(store, cfg, &round).await {
            posted += 1;
        }
    }
    posted
}

/// Delete the drafts of hunter's that posts of `review` left on its PR
/// ([`forge::Forge::delete_own_drafts`]) and clear the review's mark
/// ([`PrReviewRow::drafts_left_at`]). A failure is logged and keeps the
/// mark: the next sync deletes them, and until then the review posts
/// nothing new, which would add a second set.
async fn delete_left_drafts(store: &AdminStore, review: &PrReviewRow) {
    let label = format!("pr_review {} #{}", review.repo_name, review.pr_number);
    let (name, url, n) = (review.forge, review.repo_url.clone(), review.pr_number);
    let deleted = blocking(move || forge::forge_for(name).delete_own_drafts(&url, n)).await;
    let (kind, msg) = match deleted {
        Ok(deleted) => match store.clear_pr_review_drafts(review.id).await {
            Ok(()) if deleted == 0 => return,
            Ok(()) => (
                EVENT,
                format!("{label}: deleted {deleted} draft(s) a failed post left on the PR"),
            ),
            Err(e) => (
                "error",
                format!("{label}: no draft of hunter's left on the PR; recording it: {e}"),
            ),
        },
        Err(e) => (
            "error",
            format!("{label}: deleting the drafts a failed post left on the PR: {e:#}"),
        ),
    };
    let _ = store.log_event(kind, &msg, None, None).await;
}

/// Post one round -- its review unless that is up already, then the read
/// of its inline comments' ids, then its threads' answers -- and record it
/// posted; true when all of it went out. Every outcome is an event.
async fn post_round(store: &AdminStore, cfg: &Config, round: &ApprovedRound) -> bool {
    let review = match store.get_pr_review(round.review_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return false,
        Err(e) => {
            let msg = format!("pr_review post: reading review {}: {e}", round.review_id);
            let _ = store.log_event("error", &msg, None, None).await;
            return false;
        }
    };
    let label = format!("pr_review {} #{}", review.repo_name, review.pr_number);
    // Its number may name another PR at the repo's new URL
    // ([`PrReviewRow::repo_moved`]): nothing goes out until a request binds
    // the review to the URL. A round of an earlier binding than that is
    // about the PR the earlier URL named, and never goes out: the list of
    // approved rounds leaves those out, and one rebound since it was read
    // is left here.
    if review.repo_moved() || review.binding != round.binding {
        return false;
    }
    if let Some(landed) = take_unrecorded(Step::Comments(round.id))
        && let Err(e) = record(store, landed).await
    {
        let msg = format!(
            "{label}: round {}: recording its inline comments: {e}",
            round.id
        );
        let _ = store.log_event("error", &msg, None, None).await;
        return false;
    }
    let mut issues = match store.pr_review_round_issues(round.id).await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("{label}: reading round {}: {e}", round.id);
            let _ = store.log_event("error", &msg, None, None).await;
            return false;
        }
    };
    let total = issues.len();
    let mut review_ref = round.review_ref.clone();
    let mut inline = 0;
    if !issues.is_empty() && review_ref.is_none() {
        let posted = match take_unrecorded(Step::Review(round.id)) {
            Some(Landed::Review { posted, .. }) => posted,
            _ => match send_review(store, cfg, (&review, round, &label), &mut issues).await {
                Some(p) => p,
                None => return false,
            },
        };
        inline += posted.inline.len();
        let posted_ref = posted.review_ref.clone();
        let landed = Landed::Review {
            round: round.id,
            posted,
        };
        if let Err(e) = record(store, landed).await {
            // On the forge already, and held for the next sync to record:
            // the round stays `approved` till then.
            let msg = format!(
                "{label}: posted round {}'s review; recording it failed: {e}",
                round.id
            );
            let _ = store.log_event("error", &msg, None, None).await;
            return true;
        }
        review_ref = Some(posted_ref);
    }
    inline += issues
        .iter()
        .filter(|i| i.inline != InlineState::None)
        .count();
    if let Some(review_ref) = &review_ref
        && let Err(e) = read_back_refs(store, (&review, round, &label), review_ref).await
    {
        return post_failed(store, cfg, (&review, round, &label), &e).await;
    }
    // Read after the inline comments' ids are recorded: the threads the
    // review just opened are among them.
    let threads = match store.pr_review_round_threads(round.id).await {
        Ok(t) => t,
        Err(e) => {
            let msg = format!("{label}: reading round {}'s threads: {e}", round.id);
            let _ = store.log_event("error", &msg, None, None).await;
            return false;
        }
    };
    let mut tally = Tally::default();
    if let Err(e) = settle_threads(store, cfg, (&review, round, &label), &threads, &mut tally).await
    {
        return post_failed(store, cfg, (&review, round, &label), &e).await;
    }
    let msg = format!(
        "{label}: posted round {} ({total} issue(s), {inline} inline; {} thread(s) answered, \
         {} resolved, {} unresolved)",
        round.id, tally.answered, tally.resolved, tally.unresolved
    );
    if let Err(e) = store.record_pr_review_round_posted(round.id).await {
        let msg = format!("{msg}; recording it failed: {e}");
        let _ = store.log_event("error", &msg, None, None).await;
        return true;
    }
    let _ = store.log_event(EVENT, &msg, None, None).await;
    true
}

/// What a post did in its threads, for its event.
#[derive(Debug, Default)]
struct Tally {
    answered: usize,
    resolved: usize,
    unresolved: usize,
}

/// One step of a post, by what it touches on the forge: the key of a step
/// that landed but is not recorded yet ([`Landed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Step {
    /// Round `.0`'s review.
    Review(i64),
    /// The inline comments a failed post of round `.0` put up
    /// ([`forge::PartlyPosted`]).
    Comments(i64),
    /// The answer in issue `.0`'s thread for round `.1`.
    Answer(i64, i64),
    /// Resolving or unresolving issue `.0`'s thread.
    Resolution(i64),
}

/// A step the forge took, with what recording it writes.
#[derive(Debug)]
enum Landed {
    Review {
        round: i64,
        posted: PostedReview,
    },
    Comments {
        round: i64,
        inline: Vec<i64>,
    },
    Answer {
        issue: i64,
        round: i64,
        verdict: IssueVerdict,
        answers_reply_at: Option<i64>,
    },
    Resolution {
        issue: i64,
        resolved: bool,
    },
}

impl Landed {
    fn step(&self) -> Step {
        match *self {
            Self::Review { round, .. } => Step::Review(round),
            Self::Comments { round, .. } => Step::Comments(round),
            Self::Answer { issue, round, .. } => Step::Answer(issue, round),
            Self::Resolution { issue, .. } => Step::Resolution(issue),
        }
    }
}

/// Steps that landed on the forge but whose recording failed: the next
/// sync records each ([`take_unrecorded`]) rather than taking it again.
/// Held in memory only: a recording that fails is the database's trouble,
/// and after a restart in that window the retry's read finds the step on
/// the PR by its mark instead ([`send_review`], [`settle_thread`]).
static UNRECORDED: std::sync::LazyLock<std::sync::Mutex<HashMap<Step, Landed>>> =
    std::sync::LazyLock::new(Default::default);

fn unrecorded() -> std::sync::MutexGuard<'static, HashMap<Step, Landed>> {
    UNRECORDED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The step `step` that landed earlier without being recorded, if any:
/// for the caller to [`record`] instead of taking it again.
fn take_unrecorded(step: Step) -> Option<Landed> {
    unrecorded().remove(&step)
}

/// Record `landed`, a step the forge took. When that fails it is held in
/// [`UNRECORDED`], so the next sync records it rather than taking it again.
async fn record(store: &AdminStore, landed: Landed) -> anyhow::Result<()> {
    let done = match &landed {
        Landed::Review { round, posted } => {
            store
                .record_pr_review_review_posted(*round, &posted.review_ref, &posted.inline)
                .await
        }
        Landed::Comments { round, inline } => {
            store.record_pr_review_comments_posted(*round, inline).await
        }
        Landed::Answer {
            issue,
            round,
            verdict,
            answers_reply_at,
        } => {
            store
                .record_pr_review_answered((*issue, *round), *verdict, *answers_reply_at)
                .await
        }
        Landed::Resolution { issue, resolved } => {
            store
                .record_pr_review_thread_resolution(*issue, *resolved)
                .await
        }
    };
    if let Err(e) = done {
        unrecorded().insert(landed.step(), landed);
        return Err(e.into());
    }
    Ok(())
}

/// Record the inline comments a failed post of `round` already put on the
/// PR ([`forge::PartlyPosted`]), so its retry leaves them out. A recording
/// that fails is logged and held: the next sync records them first.
async fn keep_partly_posted(
    store: &AdminStore,
    (label, round): (&str, &ApprovedRound),
    e: &anyhow::Error,
) {
    let Some(partly) = e.downcast_ref::<forge::PartlyPosted>() else {
        return;
    };
    let landed = Landed::Comments {
        round: round.id,
        inline: partly.inline.clone(),
    };
    if let Err(db) = record(store, landed).await {
        let msg = format!(
            "{label}: round {}: {partly}; recording them failed: {db}",
            round.id
        );
        let _ = store.log_event("error", &msg, None, None).await;
    }
}

/// `call` on a blocking thread: every forge call is a subprocess.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(call)
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("forge task: {e}")))
}

/// The step mark of round `round_id`'s review ([`forge::step_mark`]).
fn review_step(round_id: i64) -> String {
    format!("review of round {round_id}")
}

/// The step mark of round `round_id`'s answer in an issue's thread.
fn answer_step(round_id: i64) -> String {
    format!("answer of round {round_id}")
}

/// Round `round`'s review on the PR: posted now ([`post_marked`]), unless
/// an earlier post sent it ([`ApprovedRound::review_sent`]) and it is
/// there. A send's call can fail after the forge took the review (a
/// timeout, a 5xx after the write), so a review sent before is looked for
/// first, by its step mark ([`forge::Forge::find_review`]), and taken as
/// posted when found. Inline comments the read finds without their review
/// (a GitLab post that failed between its notes) are recorded, as those a
/// failed post reports are ([`keep_partly_posted`]), and the post leaves
/// them out. `None` when nothing went up: a failed read fails the post,
/// which is counted, as a failed send is.
async fn send_review(
    store: &AdminStore,
    cfg: &Config,
    (review, round, label): (&PrReviewRow, &ApprovedRound, &str),
    issues: &mut [PostableIssue],
) -> Option<PostedReview> {
    if round.review_sent {
        let wanted: Vec<(i64, String)> = issues
            .iter()
            .filter(|i| i.inline == InlineState::None)
            .map(|i| (i.id, comment_text(i)))
            .collect();
        let (name, url, n, step) = (
            review.forge,
            review.repo_url.clone(),
            review.pr_number,
            review_step(round.id),
        );
        let found = blocking(move || {
            let wanted: Vec<(i64, &str)> = wanted.iter().map(|(i, t)| (*i, t.as_str())).collect();
            forge::forge_for(name).find_review(&url, n, &step, &wanted)
        })
        .await;
        let found = match found {
            Ok(found) => found,
            Err(e) => {
                let e = e.context("looking for the review an earlier post sent");
                post_failed(store, cfg, (review, round, label), &e).await;
                return None;
            }
        };
        if let Some(review_ref) = found.review_ref {
            let msg = format!(
                "{label}: round {}: the review an earlier post sent is on the PR; not sent again",
                round.id
            );
            let _ = store.log_event(EVENT, &msg, None, None).await;
            return Some(PostedReview {
                review_ref,
                inline: found.inline,
            });
        }
        if !found.inline.is_empty() {
            let landed = Landed::Comments {
                round: round.id,
                inline: found.inline.clone(),
            };
            if let Err(e) = record(store, landed).await {
                let msg = format!(
                    "{label}: round {}: recording the inline comments an earlier post left: {e}",
                    round.id
                );
                let _ = store.log_event("error", &msg, None, None).await;
                return None;
            }
            for i in issues.iter_mut().filter(|i| found.inline.contains(&i.id)) {
                i.inline = InlineState::Unread;
            }
        }
    }
    post_marked(store, cfg, (review, round, label), issues.to_vec()).await
}

/// Post round `round`'s `issues` ([`post_review`]) with its review marked
/// as possibly leaving drafts on the PR ([`PrReviewRow::drafts_left_at`])
/// from before the post creates any until it published them all, and the
/// round's review marked sent ([`ApprovedRound::review_sent`]) before the
/// call that may land it, unmarked when the forge refused it outright
/// ([`forge::NothingPosted`]): a round the forge refuses for good can
/// then be discarded once it falls back to `pending`. `None` when nothing
/// was posted: drafts an earlier post left are still on the PR (deleting
/// them failed, as logged), and a new post would leave a second set; or
/// the post failed. Either is counted as a failed post ([`post_failed`]),
/// so a round whose drafts cannot be deleted falls back to `pending` like
/// any round that cannot post; a failed post's drafts are deleted.
async fn post_marked(
    store: &AdminStore,
    cfg: &Config,
    (review, round, label): (&PrReviewRow, &ApprovedRound, &str),
    issues: Vec<PostableIssue>,
) -> Option<PostedReview> {
    if review.drafts_left_at.is_some() {
        let e = anyhow::anyhow!(
            "drafts an earlier post left on the PR are not deleted yet, and posting \
             now would leave a second set"
        );
        post_failed(store, cfg, (review, round, label), &e).await;
        return None;
    }
    if let Err(e) = store.mark_pr_review_drafts(review.id).await {
        let msg = format!("{label}: round {}: marking its post: {e}", round.id);
        let _ = store.log_event("error", &msg, None, None).await;
        return None;
    }
    if let Err(e) = store.mark_pr_review_sent(round.id).await {
        let msg = format!("{label}: round {}: marking its review sent: {e}", round.id);
        let _ = store.log_event("error", &msg, None, None).await;
        return None;
    }
    match post_review(review, round, issues).await {
        Ok(posted) => {
            if let Err(e) = store.clear_pr_review_drafts(review.id).await {
                // The next sync finds none of hunter's left and clears it.
                let msg = format!("{label}: round {}: unmarking its post: {e}", round.id);
                let _ = store.log_event("error", &msg, None, None).await;
            }
            Some(posted)
        }
        Err(e) => {
            delete_left_drafts(store, review).await;
            keep_partly_posted(store, (label, round), &e).await;
            if e.downcast_ref::<forge::NothingPosted>().is_some()
                && let Err(db) = store.unmark_pr_review_sent(round.id).await
            {
                let msg = format!(
                    "{label}: round {}: unmarking its review sent: {db}",
                    round.id
                );
                let _ = store.log_event("error", &msg, None, None).await;
            }
            post_failed(store, cfg, (review, round, label), &e).await;
            None
        }
    }
}

/// Post round `round`'s `issues` as one review, but for those a post that
/// failed partway already put up inline: the header still counts them.
/// The body carries the review's step mark ([`review_step`]), what
/// [`send_review`] finds it by after a send whose call failed.
async fn post_review(
    review: &PrReviewRow,
    round: &ApprovedRound,
    mut issues: Vec<PostableIssue>,
) -> anyhow::Result<PostedReview> {
    // The issues' lines number the head the round read. Once the PR moved
    // on, its diff is another one, and a line there is other code: the
    // whole round goes into the body. `head_sha` is what the sync just
    // read.
    let current = review.head_sha.as_deref() == Some(round.head_sha.as_str());
    let header = header(round, issues.len());
    let mark = forge::step_mark(&review_step(round.id));
    issues.retain(|i| i.inline == InlineState::None);
    let (fg, url, n) = (
        forge::forge_for(review.forge),
        review.repo_url.clone(),
        review.pr_number,
    );
    // A round that went all into the body is posted against the head the
    // PR has now: after a force-push the round's own head is no commit of
    // the PR any more, and GitHub refuses a review naming it.
    let commit_id = match (&review.head_sha, current) {
        (Some(now), false) => now.clone(),
        _ => round.head_sha.clone(),
    };
    // A round whose base is not the merge-base numbers its LEFT lines by
    // that base; the PR's diff numbers its old side from the merge-base,
    // so they go into the body.
    let left_inline = !round.incremental;
    blocking(move || {
        let placeable = issues
            .iter()
            .any(|i| i.file.is_some() && i.line.is_some() && (left_inline || !i.left));
        // A diff the forge will not serve (GitHub refuses one past its size
        // limits) leaves every issue for the body rather than failing the
        // post for good.
        let diff = if current && placeable {
            fg.pr_diff(&url, n)
                .inspect_err(|e| tracing::warn!("PR #{n}: no diff to place issues on: {e:#}"))
                .ok()
        } else {
            None
        };
        let post = compose(
            (commit_id, header, mark),
            &issues,
            diff.as_deref(),
            left_inline,
        );
        fg.post_review(&url, n, &post)
    })
    .await
}

/// Read back the forge ids of round `round`'s inline comments that have
/// none yet, from its review `review_ref`, and record them: an issue's
/// thread is found by its comment's id from then on. They are asked for
/// in the round's order of its issues, the one its post created them in
/// ([`compose`]), which tells two comments of one text apart
/// ([`forge::Forge::inline_comment_refs`]). One the read does not find is
/// recorded gone (somebody deleted it) and logged. A failed read fails the
/// post, which reads again on its retry: the review is up, and is never
/// posted twice.
async fn read_back_refs(
    store: &AdminStore,
    (review, round, label): (&PrReviewRow, &ApprovedRound, &str),
    review_ref: &str,
) -> anyhow::Result<()> {
    let unread: Vec<(i64, String)> = store
        .pr_review_round_issues(round.id)
        .await?
        .iter()
        .filter(|i| i.inline == InlineState::Unread)
        .map(|i| (i.id, comment_text(i)))
        .collect();
    if unread.is_empty() {
        return Ok(());
    }
    let (name, url, n) = (review.forge, review.repo_url.clone(), review.pr_number);
    let (wanted, review_ref) = (unread.clone(), review_ref.to_owned());
    let found = blocking(move || {
        let wanted: Vec<(i64, &str)> = wanted.iter().map(|(i, t)| (*i, t.as_str())).collect();
        forge::forge_for(name).inline_comment_refs(&url, n, &review_ref, &wanted)
    })
    .await?;
    let gone: Vec<i64> = unread
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| !found.iter().any(|(f, _)| f == id))
        .collect();
    store
        .record_pr_review_thread_refs(round.id, &found, &gone)
        .await?;
    for issue in gone {
        let msg = format!(
            "{label}: round {}: the inline comment of issue {issue} is gone from the PR",
            round.id
        );
        let _ = store.log_event(EVENT, &msg, None, None).await;
    }
    Ok(())
}

/// Make each of `threads` show its judgement ([`settle_thread`]). The
/// threads with something to do are found on one read of every thread of
/// the PR, by their first comment: one the read does not hold is gone (a
/// human deleted it), which is recorded and logged, and the post goes on
/// without it. A thread holding news its verdict's round was not shown
/// ([`ThreadToSettle::saw`]: by the rule that makes a round due) is left
/// as it is, and what the read found there is recorded
/// ([`AdminStore::observe_pr_review_threads`]): the news awaits judgment
/// from then on, shown on the review, and the round that judges it answers
/// the thread -- on a watched review the round the news makes due, on an
/// unwatched one, where news alone makes none, the round a request asks
/// for ([`crate::store::Store::request_pr_review`]). A
/// failed read or step fails the post, whose retry takes only the steps
/// still to do.
async fn settle_threads(
    store: &AdminStore,
    cfg: &Config,
    (review, round, label): (&PrReviewRow, &ApprovedRound, &str),
    threads: &[ThreadToSettle],
    tally: &mut Tally,
) -> anyhow::Result<()> {
    let due: Vec<&ThreadToSettle> = threads.iter().filter(|t| !t.steps().is_empty()).collect();
    if due.is_empty() {
        return Ok(());
    }
    let (name, url, n) = (review.forge, review.repo_url.clone(), review.pr_number);
    let (refs, author, bots) = (
        due.iter().map(|t| t.thread_ref.clone()).collect::<Vec<_>>(),
        review.author.clone(),
        cfg.review_bots.clone(),
    );
    let read_at = crate::util::now_ms();
    let read = blocking(move || {
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        forge::forge_for(name).review_threads(&url, n, &refs, &author, &bots)
    })
    .await?;
    let thread_of = |t: &ThreadToSettle| read.iter().find(|r| r.first_ref == t.thread_ref);
    let gone: Vec<i64> = due
        .iter()
        .filter(|t| thread_of(t).is_none())
        .map(|t| t.issue_id)
        .collect();
    if !gone.is_empty() {
        store.record_pr_review_threads_gone(&gone).await?;
        for issue in gone {
            let msg = format!(
                "{label}: round {}: the thread of issue {issue} is gone from the PR; not answered",
                round.id
            );
            let _ = store.log_event(EVENT, &msg, None, None).await;
        }
    }
    let mut news = ThreadsRead::default();
    for t in due {
        let Some(found) = thread_of(t) else {
            continue;
        };
        let seen = super::thread_seen(t.issue_id, found.clone());
        if !t.saw(&seen) {
            let msg = format!(
                "{label}: round {}: issue {} has news in its thread the verdict did not see; \
                 left for the round that judges it",
                round.id, t.issue_id
            );
            let _ = store.log_event(EVENT, &msg, None, None).await;
            news.seen.push(seen);
            continue;
        }
        settle_thread(store, review, (t, found), tally).await?;
    }
    if !news.seen.is_empty() {
        store.observe_pr_review_threads(&news, read_at).await?;
    }
    Ok(())
}

/// Make thread `t`, `found` on the forge, show its judgement
/// ([`ThreadToSettle::steps`]): answer it, then resolve or unresolve it,
/// recording each step as it lands. A step that landed on an earlier post
/// but was not recorded is recorded now, not taken again; so is one the
/// read of the thread shows taken already -- an answer of this round's
/// (by its step mark, [`answer_step`]), the resolution wanted -- whose
/// forge call failed after the forge acted on it (a timeout, a 5xx after
/// the write). Since a call may act however it ends, each is marked
/// before it is made: an answer marks its round as possibly on the PR
/// (never to be discarded), and a resolution marks what hunter sets out
/// to make the thread show, so a read that finds it so -- one overlapping
/// the call, or the next after a call or a recording that failed --
/// takes it for hunter's step, never for a human reopening the thread
/// ([`AdminStore::mark_pr_review_resolving`]). A failed step
/// fails the post, whose retry takes only the steps still to do: hunter's
/// login can push to every repo it tracks, so the forge never refuses it
/// a thread's resolution for want of access.
async fn settle_thread(
    store: &AdminStore,
    review: &PrReviewRow,
    (t, found): (&ThreadToSettle, &forge::ReviewThread),
    tally: &mut Tally,
) -> anyhow::Result<()> {
    let steps = t.steps();
    let (name, url, n) = (review.forge, review.repo_url.clone(), review.pr_number);
    if let Some(reply) = steps.reply {
        let landed = if let Some(landed) = take_unrecorded(Step::Answer(t.issue_id, t.round_id)) {
            landed
        } else {
            let step = answer_step(t.round_id);
            let answered = found
                .replies
                .iter()
                .any(|r| r.own && r.step.as_deref() == Some(step.as_str()));
            if !answered {
                store.mark_pr_review_answering(t.round_id).await?;
                let text = format!("{}\n\n{}", answer_text(reply, t), forge::step_mark(&step));
                let (url, reply_to) = (url.clone(), found.reply_to.clone());
                blocking(move || forge::forge_for(name).reply_to_thread(&url, n, &reply_to, &text))
                    .await?;
            }
            Landed::Answer {
                issue: t.issue_id,
                round: t.round_id,
                verdict: t.verdict,
                answers_reply_at: t.answers_reply_at,
            }
        };
        record(store, landed).await?;
        tally.answered += 1;
    }
    if let Some(resolved) = steps.resolve {
        // The read is the forge's state, a resolution that landed on an
        // earlier post unrecorded included: the step is taken only when
        // the thread shows otherwise, and recorded either way.
        let _ = take_unrecorded(Step::Resolution(t.issue_id));
        if found.resolved != resolved {
            store.mark_pr_review_resolving(t.issue_id, resolved).await?;
            let thread_id = found.thread_id.clone();
            blocking(move || {
                forge::forge_for(name).set_thread_resolved(&url, n, &thread_id, resolved)
            })
            .await?;
        }
        let landed = Landed::Resolution {
            issue: t.issue_id,
            resolved,
        };
        record(store, landed).await?;
        if resolved {
            tally.resolved += 1;
        } else {
            tally.unresolved += 1;
        }
    }
    Ok(())
}

/// Hunter's answer in thread `t`: its round's verdict and note, the head
/// it reviewed for an addressed issue.
fn answer_text(reply: Reply, t: &ThreadToSettle) -> String {
    let (note, head) = (t.note.trim(), t.head_sha.as_str());
    let lead = match reply {
        Reply::Settled if t.verdict == IssueVerdict::RebuttalAccepted => {
            "Rebuttal accepted".to_owned()
        }
        Reply::Settled => format!("Addressed in `{}`", head.get(..7).unwrap_or(head)),
        Reply::StillOpen => "Still open".to_owned(),
        Reply::OpenAgain => format!("Open again at `{}`", head.get(..7).unwrap_or(head)),
    };
    if note.is_empty() {
        format!("{lead}.")
    } else {
        format!("{lead}: {note}")
    }
}

/// Count a failed post of `round` (see [`post_pr_reviews`]); always false,
/// for the caller to return.
async fn post_failed(
    store: &AdminStore,
    cfg: &Config,
    (review, round, label): (&PrReviewRow, &ApprovedRound, &str),
    e: &anyhow::Error,
) -> bool {
    if let Some(host) = unreachable_forge(cfg, &review.repo_url).await {
        tracing::info!(
            "{label}: posting round {} failed with {host} unreachable: {e}",
            round.id
        );
        return false;
    }
    let why = format!("{e:#}");
    let why = crate::util::tail(&why, 500);
    let msg = match store.record_pr_review_post_failure(round.id, why).await {
        Ok(attempts) if attempts >= POST_GIVE_UP_AT => format!(
            "{label}: posting round {} failed ({why}); back to pending after {attempts} attempts",
            round.id
        ),
        Ok(attempts) => format!(
            "{label}: posting round {} failed ({why}); attempt {attempts}",
            round.id
        ),
        Err(db) => format!(
            "{label}: posting round {} failed ({why}); recording it: {db}",
            round.id
        ),
    };
    let _ = store.log_event("error", &msg, None, None).await;
    false
}

/// The forge host of the repo at `url`, when the network gate is on and
/// its probe cannot reach it: a failed post is then the machine's, not the
/// round's.
async fn unreachable_forge(cfg: &Config, url: &str) -> Option<String> {
    if !cfg.network_gate {
        return None;
    }
    let hosts: Vec<String> = forge::web_host(url).into_iter().collect();
    crate::reach::first_unreachable(&hosts).await
}

/// The first line of every posted review.
fn header(round: &ApprovedRound, issues: usize) -> String {
    let short = |sha: &str| sha.get(..7).unwrap_or(sha).to_owned();
    format!(
        "**Automated review by hunter** (round of `{}..{}`): {issues} issue(s).",
        short(&round.base_sha),
        short(&round.head_sha)
    )
}

/// The longest issue text posted, in characters: half of what the forge
/// takes in one comment ([`forge::REVIEW_TEXT_MAX_CHARS`]), which leaves
/// room for the marks a comment carries and, in a review's body, for its
/// header and other issues. The reviewer's summary and detail come
/// uncapped, and one comment past the forge's limit would fail the post
/// on every attempt.
const ISSUE_TEXT_MAX_CHARS: usize = forge::REVIEW_TEXT_MAX_CHARS / 2;

/// How one issue reads on the forge, inline or in the body: cut at
/// [`ISSUE_TEXT_MAX_CHARS`], saying so.
fn comment_text(issue: &PostableIssue) -> String {
    let label = issue
        .severity
        .as_deref()
        .map_or_else(|| issue.kind.clone(), |s| format!("{s} · {}", issue.kind));
    let blocking = if issue.blocking { " · blocking" } else { "" };
    let mut text = format!("**{label}**{blocking}: {}", issue.summary);
    if !issue.detail.is_empty() {
        text.push_str("\n\n");
        text.push_str(&issue.detail);
    }
    match text.char_indices().nth(ISSUE_TEXT_MAX_CHARS) {
        Some((at, _)) => format!("{} [cut]", &text[..at]),
        None => text,
    }
}

/// The review for `issues`, with `header` and the step mark `mark` in its
/// body: inline where `diff` has the line in a hunk, in the body otherwise
/// (and all of them in the body without a diff, the LEFT ones without
/// `left_inline`).
fn compose(
    (commit_id, header, mark): (String, String, String),
    issues: &[PostableIssue],
    diff: Option<&str>,
    left_inline: bool,
) -> ReviewPost {
    let mut post = ReviewPost {
        commit_id,
        header,
        unplaced: Vec::new(),
        mark,
        comments: Vec::new(),
    };
    for issue in issues {
        let side = if issue.left { Side::Left } else { Side::Right };
        let text = comment_text(issue);
        let diff = diff.filter(|_| left_inline || !issue.left);
        let located = match (diff, issue.file.as_deref(), issue.line) {
            (Some(diff), Some(file), Some(line)) => {
                place::place(diff, file, line, side).map(|at| (line, at))
            }
            _ => None,
        };
        if let Some((line, at)) = located {
            post.comments.push(InlineComment {
                issue_id: issue.id,
                path: at.new_path,
                old_path: at.old_path,
                line,
                side,
                old_line: at.old_line,
                new_line: at.new_line,
                body: text,
            });
            continue;
        }
        let location = issue.file.as_deref().map(|file| match issue.line {
            Some(line) if issue.left => format!("{file}:{line} (base)"),
            Some(line) => format!("{file}:{line}"),
            None => file.to_owned(),
        });
        post.push_unplaced(location.as_deref(), &text);
    }
    post
}
