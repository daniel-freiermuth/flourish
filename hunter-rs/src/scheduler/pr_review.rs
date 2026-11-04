//! Reviews of pull requests hunter did not open (future.md §8.8): one
//! round per due review ([`run_pr_review`]) on the job lane, and on the
//! admin lane the PR sync's refresh of every open review
//! ([`sync_pr_reviews`]) followed by the posting of approved rounds
//! ([`post_pr_reviews`]).
//!
//! A round reviews `base..head` of the PR's head commit in a detached tree:
//! the first round from the merge-base with the PR's base branch, every
//! later one from the head the last successful round reviewed, so a watched
//! PR is reviewed incrementally as its author pushes. The results are the
//! reviewer's issues, recorded per round (`pr_review_rounds`); a round with
//! issues is posted to the PR once a user approved it, or at once when the
//! review posts automatically.

mod place;
mod post;

pub use post::post_pr_reviews;

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
    let (cap, anticipated) = match budget_gate(
        backend, store, cfg, repo.id, KIND, false, &label, None, None,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let fg = forge::forge_for(repo.forge);
    let pr = match fg.view_pr_meta(&repo.url, n) {
        Ok(pr) => pr,
        Err(e) => {
            let ctx = (cfg, backend, &repo);
            return Ok(prefetch_failed(store, ctx, &review, "PR/MR view", &e).await);
        }
    };
    log_write_failure(
        store
            .observe_pr_review(review_id, &pr.head_sha, &pr.title, review_state(pr.state))
            .await,
        format_args!("{label}: recording the PR's state"),
    );
    if pr.state != PrState::Open {
        return Ok(skipped(format!("PR is {:?} now", pr.state).to_lowercase()));
    }
    if review.last_reviewed_sha.as_deref() == Some(pr.head_sha.as_str()) {
        return Ok(skipped("PR head already reviewed".to_owned()));
    }

    // The permit only now, after every forge read.
    let permit = lane.lock_clone(repo.id).await?;
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
        return Ok(prefetch_failed(store, ctx, &review, "fetch", &tail).await);
    }
    // The head and the base, resolved once under the permit: the admin
    // lane fetches the clone while the reviewer runs.
    let last = review.last_reviewed_sha.clone();
    let pr_ref = forge::pr_head_ref(n);
    let resolved = permit
        .blocking(move |hold| {
            let head = crate::workspace::resolve(hold.clone_dir(), &pr_ref)?;
            // Incremental from the last reviewed head while the PR still
            // descends from it; after a force-push that commit is no longer
            // in the PR, so the round starts over from the merge-base.
            let from_last = last.filter(|last| {
                Unheld::IsAncestor {
                    ancestor: last,
                    rev: &head,
                }
                .run(hold.clone_dir(), 60)
                .0 == 0
            });
            let base = if let Some(last) = from_last {
                last
            } else {
                let upstream = format!("origin/{base_ref}");
                let (rc, out) = hold.git(&["merge-base", &upstream, &head], 60);
                let sha = out.trim();
                if rc != 0 || sha.is_empty() {
                    return None;
                }
                sha.to_owned()
            };
            Some((base, head))
        })
        .await?;
    let Some((base, head)) = resolved else {
        drop(permit);
        let why = "the PR head or its merge-base does not resolve in the clone";
        let ctx = (cfg, backend, &repo);
        return Ok(prefetch_failed(store, ctx, &review, "resolve", &why).await);
    };
    let incremental = review.last_reviewed_sha.as_deref() == Some(base.as_str());

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

    let earlier = if incremental {
        store.chain_pr_review_issues(review_id).await?
    } else {
        Vec::new()
    };
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
        (JobState::Done, Err(why)) => Err(why),
        (state, _) => Err(format!("worker {state}")),
    };
    close_workspace(lane, ws).await;
    let (outcome, failure) = match &verdict {
        Ok(issues) => {
            let recorded = store
                .record_pr_review_round(
                    review_id,
                    Some(job),
                    (&base, &head),
                    PrReviewRound::Ok(issues),
                )
                .await;
            let msg = format!(
                "{label}: reviewed {}..{}: {} issue(s)",
                short(&base),
                short(&head),
                issues.len()
            );
            if let Err(e) = recorded {
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
            let _ = store.log_event(EVENT, &msg, Some(job), None).await;
            ("reviewed", None)
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
/// failure does.
async fn prefetch_failed(
    store: &JobStore,
    (cfg, backend, repo): (&Config, &dyn Backend, &Repo),
    review: &PrReviewRow,
    what: &str,
    e: &impl std::fmt::Display,
) -> CycleSummary {
    if let Some(host) = unreachable_now(cfg, &repo.url, backend).await {
        return offline_denial(store, KIND, None, None, host).await;
    }
    let why = format!("{what} failed: {e}");
    // No job ran, so no range was resolved: the round names what the
    // review knew.
    let head = review.head_sha.as_deref().unwrap_or_default();
    let base = review.last_reviewed_sha.as_deref().unwrap_or_default();
    let on_hold = count_failure(store, review, None, (base, head), &why).await;
    CycleSummary {
        kind: Some(KIND),
        repo: Some(repo.name.clone()),
        pr_number: Some(review.pr_number),
        failure: Some(why),
        outcome: Some(if on_hold { "held" } else { "retry" }.into()),
        ..Default::default()
    }
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
        .record_pr_review_round(review.id, job, range, PrReviewRound::Failed(why))
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

/// The PR sync's part of PR reviews: read every open review's PR and
/// record its head, title and state ([`AdminStore::observe_pr_review`]).
/// A merged or closed PR ends its review. A forge error is logged and
/// counts against nothing: the next pass reads the PR again. Stops at the
/// next PR once `stopping` says so. Returns how many reviews were read.
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
        let fg = forge::forge_for(review.forge);
        let (url, n) = (review.repo_url.clone(), review.pr_number);
        let viewed = tokio::task::spawn_blocking(move || fg.view_pr_meta(&url, n)).await;
        let label = format!("pr_review {} #{n}", review.repo_name);
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
            .observe_pr_review(review.id, &pr.head_sha, &pr.title, state)
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
