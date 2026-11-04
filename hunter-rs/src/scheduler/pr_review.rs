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
//!
//! A later round also judges the issues earlier rounds of its chain
//! reported, against the new commits and the replies in the threads their
//! inline comments opened, which it reads first ([`read_threads`]). An
//! issue it finds addressed, or rebutted by a reply it checked, is answered
//! and its thread resolved when the round is posted.

mod place;
mod post;

pub use post::post_pr_reviews;

use std::collections::HashSet;
use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::path::Path;

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
use crate::playbooks::{self, PrUnderReview, ReviewOutputs};
use crate::recovery::CycleScope;
use crate::store::{
    AdminStore, EarlierIssue, IssueVerdict, JobStore, PR_REVIEW_HOLD_AT, PrReviewRound,
    PrReviewRow, ReportedIssue, Store, ThreadSeen, ThreadsRead, Verdict,
};
use crate::types::{PrReviewReplyOut, Repo};
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
    // Before the permit, as every forge read: the threads of every issue
    // posted so far (a review with none reads nothing).
    read_threads(store, cfg, &review, &pr.author).await;

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

    // Incremental or after a force-push, the round judges what is still
    // open ([`JobStore::earlier_pr_review_issues`]).
    let earlier = store.earlier_pr_review_issues(review_id).await?;
    let out = ws.root.join("pr-review.json");
    let earlier_out = ws.root.join("pr-review-earlier.json");
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&earlier_out);
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
        ReviewOutputs {
            issues: &out,
            earlier: &earlier_out,
        },
        &earlier,
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
    // that changed it needs no undo. What it wrote on a tree it changed is
    // still about the PR's commits, as a stopped reviewer's is; only its
    // silence is no verdict (below).
    let changed = !before.readable() || before != after;
    let stopped = state == JobState::Suspended;
    let verdict = match (state, parse_review(&out)) {
        (JobState::Done, Ok((issues, dropped))) if dropped > 0 && issues.is_empty() => {
            Err(format!("{dropped} review entries had no summary"))
        }
        // A reviewer stopped by its cap or the wall clock still verified
        // what it wrote: the playbook has it rewrite each file after every
        // entry, as the pre-PR reviewer's does.
        (JobState::Done | JobState::Suspended, Ok((issues, _))) => {
            Ok(issues.iter().map(issue_out).collect::<Vec<_>>())
        }
        (JobState::Done, Err(why)) => Err(why),
        (state, _) => Err(format!("worker {state}")),
    };
    // Verdicts only beside a usable review: a failed round records none.
    let judged = match (&verdict, earlier.is_empty()) {
        (Ok(_), false) => Some(parse_verdicts(&earlier_out, &earlier)),
        _ => None,
    };
    // A round that gave nothing -- no issue, no verdict -- says the PR is
    // clean only when the reviewer finished on the tree it was shown. One
    // stopped before it wrote anything gave nothing; one that changed the
    // tree may have changed what it should have reported. Either fails.
    // An issue or a verdict it wrote stands either way: a push round that
    // only confirms fixes reports no issue.
    let gave = |issues: &[ReportedIssue]| {
        !issues.is_empty() || matches!(&judged, Some(Ok(v)) if !v.is_empty())
    };
    let verdict = match verdict {
        Ok(issues) if stopped && !gave(&issues) => Err(format!("worker {state}")),
        Ok(issues) if changed && !gave(&issues) => {
            Err("the reviewer changed the worktree and reported nothing".to_owned())
        }
        verdict => verdict,
    };
    close_workspace(lane, ws).await;
    let mut posts_approved = false;
    let (outcome, failure) = match &verdict {
        Ok(issues) => {
            let (verdicts, unread) = match judged {
                Some(Ok(v)) => (v, None),
                Some(Err(why)) => (Vec::new(), Some(why)),
                None => (Vec::new(), None),
            };
            // Only a reviewer that finished reviewed `base..head`: one
            // stopped partway keeps what it wrote, and the next round
            // reviews the same commits again.
            let stop = (state == JobState::Suspended)
                .then(|| format!("worker {state} before it finished"));
            let round = match &stop {
                None => PrReviewRound::Ok {
                    issues,
                    verdicts: &verdicts,
                    read_at,
                },
                Some(why) => PrReviewRound::Partial {
                    issues,
                    verdicts: &verdicts,
                    why,
                },
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
            if !earlier.is_empty() {
                msg.push_str(&judgement_summary(earlier.len(), &verdicts));
            }
            if let Some(why) = unread {
                let _ = write!(msg, "; earlier issues left as they were: {why}");
            }
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
fn issue_out(v: &serde_json::Value) -> ReportedIssue {
    let s = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    ReportedIssue {
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

/// The longest verdict note kept: one sentence of evidence, not an essay
/// to post on somebody's PR.
const NOTE_CAP_CHARS: usize = 600;

/// The longest reply kept: every reply of every earlier issue goes into
/// the next round's prompt, and one pasted log must not crowd out the rest.
const REPLY_CAP_CHARS: usize = 4000;

/// `body` cut at [`REPLY_CAP_CHARS`], saying so.
fn capped_reply(body: String) -> String {
    match body.char_indices().nth(REPLY_CAP_CHARS) {
        Some((at, _)) => format!("{} [reply cut]", &body[..at]),
        None => body,
    }
}

/// The reviewer's verdicts on `earlier` from the file at `path`, read
/// leniently: an entry naming no issue it was shown, or with a verdict
/// word it does not know, is no verdict, and an issue named twice keeps
/// its first. Silence is never a verdict: an issue the file does not name
/// stays as it was. A rebuttal can only be accepted where somebody other
/// than hunter replied in the issue's thread; claimed anywhere else it is
/// no verdict either. `Err` when there is no usable file at all.
fn parse_verdicts(path: &Path, earlier: &[EarlierIssue]) -> Result<Vec<Verdict>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("no verdict file: {e}"))?;
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|e| format!("unparseable verdict file: {e}"))?;
    let mut named: HashSet<i64> = HashSet::new();
    let mut verdicts = Vec::new();
    for entry in &entries {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(issue) = earlier.iter().find(|i| i.id == id) else {
            continue;
        };
        let Some(verdict) = entry
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .and_then(IssueVerdict::parse)
        else {
            continue;
        };
        let argued = issue.replies.iter().any(|r| !r.own);
        if verdict == IssueVerdict::RebuttalAccepted && !argued {
            continue;
        }
        if !named.insert(id) {
            continue;
        }
        let note = entry
            .get("note")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim();
        verdicts.push(Verdict {
            issue_id: id,
            verdict,
            note: note.chars().take(NOTE_CAP_CHARS).collect(),
            seen_reopened_at: issue.reopened_at,
        });
    }
    Ok(verdicts)
}

/// How a round judged the `shown` earlier issues, for its event.
fn judgement_summary(shown: usize, verdicts: &[Verdict]) -> String {
    let count = |v: IssueVerdict| verdicts.iter().filter(|x| x.verdict == v).count();
    format!(
        "; of {shown} earlier: {} addressed, {} rebutted, {} still open, {} not judged",
        count(IssueVerdict::Addressed),
        count(IssueVerdict::RebuttalAccepted),
        count(IssueVerdict::Open),
        shown - verdicts.len()
    )
}

/// Read the threads of every issue of review `review` posted inline
/// ([`Store::posted_pr_review_issues`]: settled ones and older chains'
/// too, so one a human reopened is seen) and record their replies,
/// screened with `pr_author` trusted, and any a human reopened
/// ([`JobStore::observe_pr_review_threads`]). The round judges them next.
/// A failure is logged and costs only the replies: the round goes on with
/// what the last read saw.
async fn read_threads(store: &JobStore, cfg: &Config, review: &PrReviewRow, pr_author: &str) {
    let label = format!("pr_review {} #{}", review.repo_name, review.pr_number);
    let posted = match store.posted_pr_review_issues(review.id).await {
        Ok(posted) => posted,
        Err(e) => {
            let msg = format!("{label}: reading the posted issues: {e}");
            let _ = store.log_event("error", &msg, None, None).await;
            return;
        }
    };
    if posted.is_empty() {
        return;
    }
    let read_at = crate::util::now_ms();
    // On a blocking thread: the read is a run of forge subprocesses.
    let (kind, url, n, author, bots) = (
        review.forge,
        review.repo_url.clone(),
        review.pr_number,
        pr_author.to_owned(),
        cfg.review_bots.clone(),
    );
    let refs: Vec<String> = posted.iter().map(|(_, r)| r.clone()).collect();
    let threads = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
        forge::forge_for(kind).review_threads(&url, n, &refs, &author, &bots)
    })
    .await
    .unwrap_or_else(|e| Err(anyhow::anyhow!("forge task: {e}")));
    let threads = match threads {
        Ok(t) => t,
        Err(e) => {
            let msg = format!(
                "{label}: reading the review's threads failed, judging without replies: {e:#}"
            );
            let _ = store.log_event(EVENT, &msg, None, None).await;
            return;
        }
    };
    let seen = seen_threads(&posted, threads);
    match store.observe_pr_review_threads(&seen, read_at).await {
        Ok(0) => {}
        Ok(reopened) => {
            let msg = format!("{label}: {reopened} resolved thread(s) reopened on the PR");
            let _ = store.log_event(EVENT, &msg, None, None).await;
        }
        Err(e) => {
            let msg = format!("{label}: recording the review's threads: {e}");
            let _ = store.log_event("error", &msg, None, None).await;
        }
    }
}

/// What a read of the threads hunter's issues `posted` (issue id, the
/// inline comment's forge id) opened found, as the store records it:
/// replies capped and dated, and the issues whose thread the read, which
/// lists every thread of the PR, did not hold. A thread no posted issue
/// opened is dropped.
fn seen_threads(posted: &[(i64, String)], threads: Vec<forge::ReviewThread>) -> ThreadsRead {
    let gone = posted
        .iter()
        .filter(|(_, r)| !threads.iter().any(|t| t.first_ref == *r))
        .map(|(id, _)| *id)
        .collect();
    let seen = threads
        .into_iter()
        .filter_map(|t| {
            let (issue_id, _) = posted.iter().find(|(_, r)| *r == t.first_ref)?;
            Some(ThreadSeen {
                issue_id: *issue_id,
                resolved: t.resolved,
                replies: t
                    .replies
                    .into_iter()
                    .map(|r| PrReviewReplyOut {
                        author: r.author,
                        body: capped_reply(r.body),
                        own: r.own,
                    })
                    .collect(),
                hidden: i64::try_from(t.hidden).unwrap_or(i64::MAX),
            })
        })
        .collect();
    ThreadsRead { seen, gone }
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
