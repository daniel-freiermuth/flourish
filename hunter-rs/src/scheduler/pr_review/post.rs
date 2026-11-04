//! Posting approved review rounds to the forge (future.md §8.8, slice 2),
//! on the admin lane's PR sync right after the reviews were refreshed
//! ([`super::sync_pr_reviews`]): a post spends no tokens, only forge calls,
//! like the rest of that duty.
//!
//! A round goes out as one COMMENT review under the operator's forge
//! login: each issue whose line is in a hunk of the PR's diff
//! ([`place`](super::place)) as an inline comment, the rest in the body.
//! The inline comments' forge ids are then read back from the PR: they
//! are what each issue's thread is found by from then on. Each step is
//! recorded as it lands, so a post that failed partway retries only what
//! is missing.
//!
//! A GitLab post is draft notes of the operator's login published one by
//! one, and a post that failed or was stopped leaves the rest as drafts the
//! operator's next review by hand would publish under their name. So a
//! post marks its review before it creates any, and a marked review's
//! drafts of hunter's (each carries the own-comment mark) are deleted
//! before it posts anything again, by every sync until that succeeds
//! ([`delete_left_drafts`]).

use super::EVENT;
use super::place;
use crate::config::Config;
use crate::forge::{self, InlineComment, PostedReview, ReviewPost, Side};
use crate::store::{
    AdminStore, ApprovedRound, InlineState, POST_GIVE_UP_AT, PostableIssue, PrReviewRow,
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
/// of its inline comments' ids -- and record it posted; true when all of
/// it went out. Every outcome is an event.
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
    let mut issues = match store.pr_review_round_issues(round.id).await {
        Ok(i) => i,
        Err(e) => {
            let msg = format!("{label}: reading round {}'s issues: {e}", round.id);
            let _ = store.log_event("error", &msg, None, None).await;
            return false;
        }
    };
    let total = issues.len();
    let mut review_ref = round.review_ref.clone();
    let mut inline = 0;
    if review_ref.is_none() {
        let Some(review_post) =
            send_review(store, cfg, (&review, round, &label), &mut issues).await
        else {
            return false;
        };
        inline += review_post.inline.len();
        if let Err(e) = store
            .record_pr_review_review_posted(round.id, &review_post.review_ref, &review_post.inline)
            .await
        {
            // On the forge already: posting it again would repeat it.
            let msg = format!(
                "{label}: posted round {}'s review; recording it failed: {e}",
                round.id
            );
            let _ = store.log_event("error", &msg, None, None).await;
            return true;
        }
        review_ref = Some(review_post.review_ref);
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
    let msg = format!(
        "{label}: posted round {} ({total} issue(s), {inline} inline)",
        round.id
    );
    if let Err(e) = store.record_pr_review_round_posted(round.id).await {
        let msg = format!("{msg}; recording it failed: {e}");
        let _ = store.log_event("error", &msg, None, None).await;
        return true;
    }
    let _ = store.log_event(EVENT, &msg, None, None).await;
    true
}

/// Record the inline comments a failed post of `round` already put on the
/// PR ([`forge::PartlyPosted`]), so its retry leaves them out. A recording
/// that fails is logged: the retry then posts them a second time.
async fn keep_partly_posted(
    store: &AdminStore,
    (label, round): (&str, &ApprovedRound),
    e: &anyhow::Error,
) {
    let Some(partly) = e.downcast_ref::<forge::PartlyPosted>() else {
        return;
    };
    if let Err(db) = store
        .record_pr_review_comments_posted(round.id, &partly.inline)
        .await
    {
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
            if let Err(e) = store
                .record_pr_review_comments_posted(round.id, &found.inline)
                .await
            {
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
