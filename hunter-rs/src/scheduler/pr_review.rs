//! Reviews of pull requests hunter did not open (future.md §8.8): one
//! round per due review ([`run_pr_review`]) on the job lane, and on the
//! admin lane the PR review duty's refresh of every open review
//! ([`sync_pr_reviews`]) followed by the posting of approved rounds
//! ([`post_pr_reviews`]).
//!
//! A round reviews `base..head` of the PR's head commit in a detached tree:
//! the first round from the merge-base with the PR's base branch, every
//! later one from the head the last round that finished reviewed (one
//! stopped partway keeps what it wrote, but its head is reviewed again),
//! so a watched PR is reviewed incrementally as its author pushes. The
//! results are the reviewer's issues, recorded per round
//! (`pr_review_rounds`); a round with issues is posted to the PR once a
//! user approved it, or at once when the review posts automatically.

mod place;
mod post;

pub use post::post_pr_reviews;

use std::fmt::Write as _;
use std::ops::ControlFlow;

use super::review::{parse_review, tree_state};
use super::{
    BudgetDecision, CycleSummary, Fetcher, WorkOf, budget_gate, close_workspace, fetch_held,
    job_refused, log_write_failure, offline_denial, open_workspace, record_job, runner_failed,
    unmetered_offline, unreachable_now,
};
use crate::backend::{Backend, JobClass};
use crate::clones::{Lane, Unheld};
use crate::config::Config;
use crate::domain::{JobKind, JobState, PrJobKind, PrReviewState};
use crate::forge::{self, PrState};
use crate::playbooks::{self, PrUnderReview};
use crate::recovery::CycleScope;
use crate::store::{AdminStore, JobStore, PR_REVIEW_HOLD_AT, PrReviewRound, PrReviewRow, Store};
use crate::types::{PrReviewIssueOut, Repo};
use crate::workspace::TreeSpec;

/// The event kind of everything a PR review writes to the log.
const EVENT: &str = "pr_review";

const KIND: JobKind = JobKind::Pr(PrJobKind::Review);

/// The PR state a forge read reports, as a review records it.
const fn review_state(state: PrState) -> PrReviewState {
    match state {
        PrState::Open => PrReviewState::Open,
        PrState::Merged => PrReviewState::Merged,
        PrState::Closed => PrReviewState::Closed,
    }
}

/// Run one round of review `review_id` (see the module docs).
///
/// The forge is read before the clone's permit is taken, as every
/// executor does: the permit holds the repo's other work. The round's own
/// read of the PR is recorded ([`JobStore::observe_pr_review`]) before
/// anything else, so a PR that ended or moved since the pick is not picked
/// again on the strength of what the sync last saw.
///
/// A round that fails while the hosts its work needs are reachable counts
/// toward the review's hold ([`PR_REVIEW_HOLD_AT`]); a budget denial or an
/// unreachable host ends the cycle denied and counts nothing.
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: gate budget, read the forge, fetch, run \
              the reviewer, record the round"
)]
pub(super) async fn run_pr_review(
    scope: &CycleScope<'_>,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    review_id: i64,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let Some(review) = store.get_pr_review(review_id).await? else {
        anyhow::bail!("PR review {review_id} not found");
    };
    let Some(repo) = store.get_repo_by_id(review.repo_id).await? else {
        anyhow::bail!("repo {} not found", review.repo_id);
    };
    let n = review.pr_number;
    let label = format!("pr_review {} #{n}", repo.name);
    let skipped = |why: String| CycleSummary {
        kind: Some(KIND),
        repo: Some(repo.name.clone()),
        pr_number: Some(n),
        skipped: Some(why),
        ..Default::default()
    };
    // At another URL than the review was requested under, PR #n may be
    // another PR, by an author nobody decided about
    // ([`PrReviewRow::repo_moved`]); the URL the round would work at is
    // the repo's as read here. Not due, so the pick never chose it; a URL
    // changed since the pick ends here, before any budget or forge call.
    if !review.bound_to(&repo.url) {
        return Ok(skipped(
            "the repo's URL changed since the review was requested; a request binds it anew"
                .to_owned(),
        ));
    }
    // A PR hunter opened after the request is hunter's own: the review
    // ends there, before any budget or forge call.
    if store.is_hunter_pr(repo.id, n).await? {
        let ended = store.end_own_pr_review(review_id).await;
        return Ok(skipped(ended_own(store, &label, ended).await));
    }
    let (cap, anticipated) = match budget_gate(
        backend, store, cfg, repo.id, KIND, false, &label, None, None,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let fg = forge::forge_for(repo.forge);
    // On a blocking thread, as the PR review duty reads it: the view is a forge
    // subprocess, which would hold a runtime worker for its whole timeout.
    // Its start is when the round read the PR: a request made before it
    // is this round's to answer (taken below).
    let read_at = crate::util::now_ms();
    let (url, kind) = (repo.url.clone(), repo.forge);
    let viewed = tokio::task::spawn_blocking(move || forge::forge_for(kind).view_pr_meta(&url, n))
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("forge task: {e}")));
    let pr = match viewed {
        Ok(pr) => pr,
        Err(e) => {
            let ctx = (cfg, backend, &repo);
            return prefetch_failed(store, ctx, &review, "PR/MR view", &e).await;
        }
    };
    log_write_failure(
        store
            .observe_pr_review(
                (review_id, review.binding),
                (&pr.head_sha, pr.fork_owner()),
                &pr.title,
                review_state(pr.state),
            )
            .await,
        format_args!("{label}: recording the PR's state"),
    );
    if pr.state != PrState::Open {
        return Ok(skipped(format!("PR is {:?} now", pr.state).to_lowercase()));
    }
    // A round runs the code of the repository the head lives in: only a
    // fork whose owner the request decided about. One that changed hands
    // since (recorded above, so the review is due for nothing) waits for a
    // request to decide about its new owner.
    if !review.decided_about(pr.fork_owner()) {
        return Ok(skipped(fork_changed_hands(&pr)));
    }
    // A fresh GitLab MR can have no head commit yet: the review has no
    // known head (the read above recorded none) and is due for nothing
    // until a read finds one.
    if pr.head_sha.is_empty() {
        return Ok(skipped("the forge reports no head commit yet".to_owned()));
    }
    if review.last_reviewed_sha.as_deref() == Some(pr.head_sha.as_str()) {
        return Ok(skipped("PR head already reviewed".to_owned()));
    }
    // The PR moved since the pick, which went by the head the sync last
    // saw: the new head waits out its stability window like any other
    // (the read above restarted it), so a burst of pushes is still one
    // round. A round a request asked for waits for no stability and runs
    // at the next pick.
    if review.round_requested_at.is_none()
        && review.head_sha.as_deref() != Some(pr.head_sha.as_str())
    {
        return Ok(skipped(
            "PR head moved since the pick; waiting for it to settle".to_owned(),
        ));
    }
    // The round reviews the PR's head now, which answers the request made
    // before the read; one made since waits, whatever head it read: it may
    // be for news in a thread the round's read below misses.
    log_write_failure(
        store
            .take_pr_review_request((review_id, review.binding), &pr.head_sha, read_at)
            .await,
        format_args!("{label}: taking the request"),
    );

    // The permit only now, after every forge read.
    let permit = lane.lock_clone(repo.id).await?;
    // A repo no job has cloned yet (one added disabled, whose scans never
    // ran) is cloned first, as a scan's sync would: the fetch below needs
    // a clone to fetch into.
    if !permit.clone_dir().exists()
        && let Err(failure) = super::sync_repo(store, &permit, Fetcher::Job).await?
    {
        drop(permit);
        let ctx = (cfg, backend, &repo);
        return prefetch_failed(store, ctx, &review, "clone", &failure.detail).await;
    }
    let refspec = fg.pr_head_refspec(n);
    let base_ref = if pr.base_ref.is_empty() {
        review.base_ref.clone()
    } else {
        pr.base_ref.clone()
    };
    let (rc, out) = fetch_held(&permit, &[&refspec, &base_ref], Fetcher::Job).await;
    if rc != 0 {
        drop(permit);
        let tail = crate::util::tail(&out, 300);
        let ctx = (cfg, backend, &repo);
        return prefetch_failed(store, ctx, &review, "fetch", &tail).await;
    }
    // The head and the base, resolved once under the permit: the admin
    // lane fetches the clone while the reviewer runs.
    let last = review.last_reviewed_sha.clone();
    let pr_ref = forge::pr_head_ref(n);
    let resolved = permit
        .blocking(move |hold| {
            let head = crate::workspace::resolve(hold.clone_dir(), &pr_ref)?;
            let upstream = format!("origin/{base_ref}");
            let (rc, out) = hold.git(&["merge-base", &upstream, &head], 60);
            if rc != 0 {
                return None;
            }
            let merge_base = out.trim();
            let descends = |ancestor: &str, rev: &str| {
                Unheld::IsAncestor { ancestor, rev }
                    .run(hold.clone_dir(), 60)
                    .0
                    == 0
            };
            // Incremental from the last reviewed head while the PR still
            // descends from it; after a force-push that commit is no longer
            // in the PR, so the round starts over from the merge-base. So
            // it does once the base branch was merged into the PR: the
            // merge-base moved past the last head, and `last..head` would
            // hold every upstream commit the merge brought in.
            let from_last = last.filter(|last| descends(last, &head) && descends(merge_base, last));
            let base = from_last.unwrap_or_else(|| merge_base.to_owned());
            // Recorded with the round: whether its base is the merge-base
            // decides where its LEFT issues can go ([`post`]).
            Some((base, head, merge_base.to_owned()))
        })
        .await?;
    let Some((base, head, merge_base)) = resolved else {
        drop(permit);
        let why = "the PR head or its merge-base does not resolve in the clone";
        let ctx = (cfg, backend, &repo);
        return prefetch_failed(store, ctx, &review, "resolve", &why).await;
    };
    // The round is the head's the forge reported, the one recorded above:
    // the forge updates the PR's head ref only some time after a push, and
    // a fetch of a ref that still names another commit fetched the wrong
    // head. Reviewing it would record that commit as reviewed; the fetch
    // failed instead, and is tried again after the error backoff.
    if head != pr.head_sha {
        drop(permit);
        let why = format!(
            "the PR's head ref is at {}, the forge reports {}",
            short(&head),
            short(&pr.head_sha)
        );
        let ctx = (cfg, backend, &repo);
        return prefetch_failed(store, ctx, &review, "fetch", &why).await;
    }

    let created = match scope
        .create_pr_review_job(
            crate::store::JobStart::fresh_pr(PrJobKind::Review, review_id),
            &base,
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(KIND, &repo.name, e),
    };
    let job = created.job.id();
    let (ws, _pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, &repo),
        created,
        None,
        TreeSpec::Detached { at: head.clone() },
        KIND,
        &label,
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(mut failed) => {
            let why = failed.failure.clone().unwrap_or_default();
            let on_hold = count_failure(store, &review, Some(job), (&base, &head), &why).await;
            failed.outcome = Some(if on_hold { "held" } else { "retry" }.into());
            return Ok(failed);
        }
    };

    // What the chain this round continues reported: none for a round from
    // scratch. A round stopped partway left its base the chain's end, so
    // the round reviewing its commits again is shown what it wrote.
    let earlier = store.chain_pr_review_issues(review_id, &base).await?;
    let out = ws.root.join("pr-review.json");
    let _ = std::fs::remove_file(&out);
    let prompt = playbooks::build_pr_review_prompt(
        &cfg.root,
        &repo,
        &Store::repo_notes(&cfg.work_root, repo.id),
        &ws.tree,
        &base,
        PrUnderReview {
            number: n,
            title: &pr.title,
            body: &pr.body,
            author: &pr.author,
        },
        &out,
        &serde_json::to_string_pretty(&earlier)?,
    );
    let prompt = match prompt {
        Ok(p) => p,
        Err(e) => {
            let why = format!("no reviewer prompt: {e}");
            log_write_failure(
                running.fail_unstarted(&why).await,
                format_args!("job {job}: failing unstarted"),
            );
            close_workspace(lane, ws).await;
            count_failure(store, &review, Some(job), (&base, &head), &why).await;
            return Err(e);
        }
    };
    let before = tree_state(&ws.tree).await;
    let model = cfg.model_for("fix");
    let work = WorkOf::Pr {
        review: &review,
        range: (&base, &head),
    };
    let rr = match backend
        .run(&ws, &prompt, cap, cfg.fix_max_wall_s, JobClass::Fix, None)
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            let denied = runner_failed(store, (cfg, backend, &repo.url), (work, running), &e).await;
            close_workspace(lane, ws).await;
            return denied.ok_or(e);
        }
    };
    let after = tree_state(&ws.tree).await;
    let running =
        match unmetered_offline(store, cfg, backend, &repo.url, (work, running), &rr).await {
            ControlFlow::Continue(running) => running,
            ControlFlow::Break(denied) => {
                close_workspace(lane, ws).await;
                return Ok(denied);
            }
        };
    let state = log_write_failure(
        record_job(store, running, &rr, model, None)
            .await
            .map(|r| r.state()),
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);
    // The tree is the reviewer's own and is released below, so a reviewer
    // that changed it needs no undo; what it reported on a tree it changed
    // is still about the PR's commits, but an empty report is no verdict.
    let changed = !before.readable() || before != after;
    let verdict = match (state, parse_review(&out)) {
        (JobState::Done, Ok((issues, dropped))) if dropped > 0 && issues.is_empty() => {
            Err(format!("{dropped} review entries had no summary"))
        }
        (JobState::Done, Ok((issues, _))) if changed && issues.is_empty() => {
            Err("the reviewer changed the worktree".to_owned())
        }
        (JobState::Done, Ok((issues, _))) => Ok(issues.iter().map(issue_out).collect::<Vec<_>>()),
        // A reviewer stopped by its cap or the wall clock still verified
        // what it wrote: the playbook has it rewrite the file after every
        // issue, as the pre-PR reviewer's does.
        (JobState::Suspended, Ok((issues, _))) if !issues.is_empty() => {
            Ok(issues.iter().map(issue_out).collect::<Vec<_>>())
        }
        (JobState::Done, Err(why)) => Err(why),
        (state, _) => Err(format!("worker {state}")),
    };
    close_workspace(lane, ws).await;
    let mut posts_approved = false;
    let (outcome, failure) = match &verdict {
        Ok(issues) => {
            // Only a reviewer that finished reviewed `base..head`: one
            // stopped partway keeps what it wrote, and the next round
            // reviews the same commits again.
            let stop = (state == JobState::Suspended)
                .then(|| format!("worker {state} before it finished"));
            let round = match &stop {
                None => PrReviewRound::Ok(issues),
                Some(why) => PrReviewRound::Partial { issues, why },
            };
            let recorded = store
                .record_pr_review_round(
                    (review_id, review.binding),
                    Some(job),
                    (&base, &head),
                    Some(&merge_base),
                    round,
                )
                .await;
            let range = format!("{}..{}", short(&base), short(&head));
            let mut msg = match &stop {
                None => format!("{label}: reviewed {range}: {} issue(s)", issues.len()),
                Some(why) => format!(
                    "{label}: stopped partway through {range} ({why}): {} issue(s) kept",
                    issues.len()
                ),
            };
            let streak = match recorded {
                Ok(streak) => streak,
                Err(e) => {
                    let _ = store
                        .log_event(
                            "error",
                            &format!("{msg}; recording failed: {e}"),
                            Some(job),
                            None,
                        )
                        .await;
                    return Err(e.into());
                }
            };
            let on_hold = streak >= PR_REVIEW_HOLD_AT;
            if stop.is_some() {
                let _ = if on_hold {
                    write!(msg, "; held after {streak} unfinished rounds in a row")
                } else {
                    write!(msg, "; {streak} unfinished in a row, reviewed again next")
                };
            }
            let _ = store.log_event(EVENT, &msg, Some(job), None).await;
            // An auto-posted round goes up on the PR review duty's next
            // run; the daemon wakes it rather than leaving it for the tick.
            posts_approved = store
                .approved_pr_review_rounds()
                .await
                .is_ok_and(|r| r.iter().any(|r| r.review_id == review_id));
            match stop {
                None => ("reviewed", None),
                Some(why) => (if on_hold { "held" } else { "retry" }, Some(why)),
            }
        }
        Err(why) => {
            let on_hold = count_failure(store, &review, Some(job), (&base, &head), why).await;
            (if on_hold { "held" } else { "retry" }, Some(why.clone()))
        }
    };
    Ok(CycleSummary {
        kind: Some(KIND),
        repo: Some(repo.name.clone()),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        pr_number: Some(n),
        diff_range: Some(format!("{base}..{head}")),
        failure,
        outcome: Some(outcome.into()),
        posts_approved,
        ..Default::default()
    })
}

/// A reviewer's issue entry as recorded: the playbook's schema, read
/// leniently (a missing optional field is null, a missing kind is the
/// reviewer's default `introduced`).
fn issue_out(v: &serde_json::Value) -> PrReviewIssueOut {
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    PrReviewIssueOut {
        kind: s("kind").unwrap_or_else(|| "introduced".to_owned()),
        blocking: v.get("blocking").and_then(serde_json::Value::as_bool) == Some(true),
        severity: s("severity"),
        confidence: v.get("confidence").and_then(serde_json::Value::as_f64),
        file: s("file"),
        line: v.get("line").and_then(serde_json::Value::as_i64),
        // The store keeps only RIGHT or LEFT; anything else is no side.
        side: s("side")
            .map(|side| side.to_ascii_uppercase())
            .filter(|side| side == "RIGHT" || side == "LEFT"),
        summary: s("summary").unwrap_or_default(),
        detail: s("detail").unwrap_or_default(),
        posted_inline: false,
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// A round that failed before its job existed (forge view, fetch, an
/// unresolvable head): counted unless a host the work needs is unreachable
/// now, which ends the cycle denied instead, as every executor's prefetch
/// failure does. A counted one ends the cycle with the error, as an
/// engage's or a harvest's does, so the daemon waits its error backoff: a
/// summary saying nothing would read as an idle cycle, and idle the lane.
async fn prefetch_failed(
    store: &JobStore,
    (cfg, backend, repo): (&Config, &dyn Backend, &Repo),
    review: &PrReviewRow,
    what: &str,
    e: &impl std::fmt::Display,
) -> anyhow::Result<CycleSummary> {
    if let Some(host) = unreachable_now(cfg, &repo.url, backend).await {
        return Ok(offline_denial(store, KIND, None, None, host).await);
    }
    let why = format!("{what} failed: {e}");
    // No job ran, so no range was resolved: the round names what the
    // review knew.
    let head = review.head_sha.as_deref().unwrap_or_default();
    let base = review.last_reviewed_sha.as_deref().unwrap_or_default();
    count_failure(store, review, None, (base, head), &why).await;
    anyhow::bail!("pr_review {} #{}: {why}", repo.name, review.pr_number)
}

/// Record a failed round of `review` and log it; true when the review is
/// now held ([`PR_REVIEW_HOLD_AT`]). A failed write is logged and holds
/// nothing.
pub(super) async fn count_failure(
    store: &JobStore,
    review: &PrReviewRow,
    job: Option<i64>,
    range: (&str, &str),
    why: &str,
) -> bool {
    let label = format!("pr_review {} #{}", review.repo_name, review.pr_number);
    match store
        .record_pr_review_round(
            (review.id, review.binding),
            job,
            range,
            None,
            PrReviewRound::Failed(why),
        )
        .await
    {
        Ok(streak) => {
            let held = streak >= PR_REVIEW_HOLD_AT;
            let msg = if held {
                format!("{label}: round failed ({why}); held after {streak} failures in a row")
            } else {
                format!("{label}: round failed ({why}); {streak} in a row")
            };
            let _ = store.log_event(EVENT, &msg, job, None).await;
            held
        }
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!("{label}: recording a failed round ({why}): {e}"),
                    job,
                    None,
                )
                .await;
            false
        }
    }
}

/// Log the end of review `label` as one of hunter's own PRs, `ended` being
/// the lane's write of it ([`JobStore::end_own_pr_review`],
/// [`AdminStore::end_own_pr_review`]); returns why it ended.
async fn ended_own(store: &Store, label: &str, ended: sqlx::Result<bool>) -> String {
    let why = "the PR is one hunter opened: it is engaged, not reviewed".to_owned();
    match ended {
        Ok(true) => {
            let msg = format!("{label}: review ended: {why}");
            let _ = store.log_event(EVENT, &msg, None, None).await;
        }
        Ok(false) => {}
        Err(e) => {
            let msg = format!("{label}: ending the review ({why}): {e}");
            let _ = store.log_event("error", &msg, None, None).await;
        }
    }
    why
}

/// Why a review whose fork changed hands since its request does nothing
/// ([`PrReviewRow::fork_changed_hands`]): `pr`'s head is in a repository
/// whose owner nobody decided about running the code of.
fn fork_changed_hands(pr: &forge::PrMeta) -> String {
    let fork = match pr.fork_owner() {
        Some("") => "a fork the forge names no owner of".to_owned(),
        Some(owner) => format!("{owner}'s fork"),
        None => "the repo itself".to_owned(),
    };
    format!(
        "the PR's head is in {fork} now, whose code nobody decided about running; \
         a request decides"
    )
}

/// The PR sync's part of PR reviews: read every open review's PR and
/// record its head, the owner of the fork it lives in, title and state
/// ([`AdminStore::observe_pr_review`]). A fork that changed hands since the
/// request stops the review until a request decides about its new owner,
/// logged once ([`PrReviewRow::fork_changed_hands`]). A merged or closed PR
/// ends its review, and so does a PR that turned
/// out to be one hunter opened ([`AdminStore::end_own_pr_review`]),
/// unread. A forge error is logged and counts against nothing: the next
/// pass reads the PR again. Stops at the next PR once `stopping` says so.
/// Returns how many reviews were read.
pub async fn sync_pr_reviews(store: &AdminStore, stopping: impl Fn() -> bool) -> usize {
    let reviews = match store.open_pr_reviews().await {
        Ok(r) => r,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!("pr_review sync: listing reviews: {e}"),
                    None,
                    None,
                )
                .await;
            return 0;
        }
    };
    let mut read = 0;
    for review in reviews {
        if stopping() {
            break;
        }
        if review.repo_moved() {
            continue;
        }
        let (url, n) = (review.repo_url.clone(), review.pr_number);
        let label = format!("pr_review {} #{n}", review.repo_name);
        match store.is_hunter_pr(review.repo_id, n).await {
            Ok(false) => {}
            Ok(true) => {
                let ended = store.end_own_pr_review(review.id).await;
                ended_own(store, &label, ended).await;
                continue;
            }
            Err(e) => {
                let msg = format!("{label}: sync: telling whether hunter opened the PR: {e}");
                let _ = store.log_event("error", &msg, None, None).await;
                continue;
            }
        }
        let fg = forge::forge_for(review.forge);
        let viewed = tokio::task::spawn_blocking(move || fg.view_pr_meta(&url, n)).await;
        let pr = match viewed {
            Ok(Ok(pr)) => pr,
            Ok(Err(e)) => {
                let _ = store
                    .log_event("error", &format!("{label}: sync: {e}"), None, None)
                    .await;
                continue;
            }
            Err(e) => {
                tracing::warn!("{label}: sync: {e}");
                continue;
            }
        };
        read += 1;
        let state = review_state(pr.state);
        if let Err(e) = store
            .observe_pr_review(
                (review.id, review.binding),
                (&pr.head_sha, pr.fork_owner()),
                &pr.title,
                state,
            )
            .await
        {
            let _ = store
                .log_event(
                    "error",
                    &format!("{label}: sync: recording: {e}"),
                    None,
                    None,
                )
                .await;
            continue;
        }
        if !review.fork_changed_hands() && !review.decided_about(pr.fork_owner()) {
            let msg = format!("{label}: {}", fork_changed_hands(&pr));
            let _ = store.log_event(EVENT, &msg, None, None).await;
        }
        if state != PrReviewState::Open {
            let _ = store
                .log_event(
                    EVENT,
                    &format!("{label}: PR {}", state.as_str()),
                    None,
                    None,
                )
                .await;
        }
    }
    read
}
