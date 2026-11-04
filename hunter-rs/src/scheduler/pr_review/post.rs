//! Posting approved review rounds to the forge (future.md §8.8, slice 2),
//! on the admin lane's PR sync right after the reviews were refreshed
//! ([`super::sync_pr_reviews`]): a post spends no tokens, only forge calls,
//! like the rest of that duty.
//!
//! A round goes out as one COMMENT review under the operator's forge
//! login: each issue whose line is in a hunk of the PR's diff
//! ([`place`](super::place)) as an inline comment, the rest in the body.

use super::EVENT;
use super::place;
use crate::config::Config;
use crate::forge::{self, InlineComment, PostedReview, ReviewPost, Side};
use crate::store::{AdminStore, ApprovedRound, POST_GIVE_UP_AT, PostableIssue};

/// Post every approved round of an open review ([`ApprovedRound`]),
/// oldest first. A post that fails counts one attempt; at
/// [`POST_GIVE_UP_AT`] the round falls back to `pending` for a user. One
/// that failed while the forge is unreachable (the network gate's probe)
/// counts nothing: the next sync tries again. Stops at the next round once
/// `stopping` says so. Returns how many rounds were posted.
pub async fn post_pr_reviews(
    store: &AdminStore,
    cfg: &Config,
    stopping: impl Fn() -> bool,
) -> usize {
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

/// Post one round; true when it went out. Every outcome is an event.
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
    let issues = match store.pr_review_round_issues(round.id).await {
        Ok(i) => i,
        Err(e) => {
            let msg = format!("{label}: reading round {}'s issues: {e}", round.id);
            let _ = store.log_event("error", &msg, None, None).await;
            return false;
        }
    };
    // The issues' lines number the head the round read. Once the PR moved
    // on, its diff is another one, and a line there is other code: the
    // whole round goes into the body. `head_sha` is what the sync just
    // read.
    let current = review.head_sha.as_deref() == Some(round.head_sha.as_str());
    let header = header(round, issues.len());
    let (fg, url, n) = (
        forge::forge_for(review.forge),
        review.repo_url.clone(),
        review.pr_number,
    );
    let commit_id = round.head_sha.clone();
    let total = issues.len();
    let sent = tokio::task::spawn_blocking(move || {
        let placeable = issues.iter().any(|i| i.file.is_some() && i.line.is_some());
        let diff = if current && placeable {
            Some(fg.pr_diff(&url, n)?)
        } else {
            None
        };
        let post = compose(commit_id, header, &issues, diff.as_deref());
        fg.post_review(&url, n, &post)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("posting task: {e}")));
    match sent {
        Ok(PostedReview {
            review_ref,
            thread_refs,
        }) => {
            let msg = format!(
                "{label}: posted round {} ({total} issue(s), {} inline)",
                round.id,
                thread_refs.len()
            );
            if let Err(e) = store
                .record_pr_review_posted(round.id, &review_ref, &thread_refs)
                .await
            {
                // On the forge already: posting it again would repeat it.
                let msg = format!("{msg}; recording it failed: {e}");
                let _ = store.log_event("error", &msg, None, None).await;
                return true;
            }
            let _ = store.log_event(EVENT, &msg, None, None).await;
            true
        }
        Err(e) => {
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
                    "{label}: posting round {} failed ({why}); back to pending after \
                     {attempts} attempts",
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
    }
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

/// How one issue reads on the forge, inline or in the body.
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
    text
}

/// The review for `issues`: inline where `diff` has the line in a hunk,
/// in the body otherwise (and all of them in the body without a diff).
fn compose(
    commit_id: String,
    header: String,
    issues: &[PostableIssue],
    diff: Option<&str>,
) -> ReviewPost {
    let mut post = ReviewPost {
        commit_id,
        body: header,
        comments: Vec::new(),
    };
    for issue in issues {
        let side = if issue.left { Side::Left } else { Side::Right };
        let text = comment_text(issue);
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
