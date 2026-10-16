//! Scheduler — job selection and the executors that carry it out.
//!
//! The selection half (`pick_next`, `anticipated_tokens`) is also what
//! `GET /api/summary` uses for its next-candidate preview, so the
//! endpoint and the loop cannot disagree about what runs next. The
//! `run_*` executors below are the other half.

use std::collections::{BTreeSet, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use anyhow::Context as _;

use crate::attention::{AnswerRecorded, EngageAnswer, ObservedOpenPr, OpenPrRecorded};
use crate::clones::{ClonePermit, Force, Lane, Unheld};
use crate::config::Config;
use crate::domain::{
    BudgetOverride, ClosureClass, FindingJobKind, FindingStatus, FindingType, FinishedState,
    HoldReason, JobKind, JobState, RepoJobKind, RepoWorkKind, Severity,
};
use crate::store::{
    FindingFilter, JobInsertError, JobOutcome, JobPlace, JobStart, PrSyncPass, SeenActivity, Store,
    StoreWriteError,
};
use crate::transition::{Moved, PrEnded, PrEnding, PrRef, Transition, Unclaimed};
use crate::types::{Finding, Job, Repo};
use crate::util::now_ms;

/// Everything a resumed attempt needs and a cold one does not.
///
/// A resume is not a seventh job kind: it runs through the executor of
/// whatever kind was suspended, so it ingests, advances watermarks and
/// handles PRs exactly as that kind always does. What differs is all in
/// here — which budget the gate reserves, which row the new job points
/// back at, which transcript omp is handed, which output path the
/// continuing worker is still writing to, and the chain's workspace and
/// pinned commit, reused untouched.
#[derive(Debug, Clone)]
pub struct ResumePlan {
    pub kind: JobKind,
    pub repo_id: i64,
    /// Repo name, carried because the events this plan's outcome logs
    /// fire from `record_job`, which never reads a repo row.
    pub repo: String,
    pub finding_id: Option<i64>,
    /// The suspended attempt being continued.
    pub predecessor_id: i64,
    /// The first attempt in the chain. Its id is the one the playbook
    /// baked into the output path, and a continuing worker is still
    /// writing there (`Store::resume_origin_job`).
    pub origin_job_id: i64,
    /// The predecessor's transcript, handed to omp verbatim. Lives in
    /// `workspace.session`.
    pub session_file: PathBuf,
    /// The chain's workspace. A resume runs in it as it is: nothing is
    /// fetched, checked out or created.
    pub workspace: crate::workspace::Workspace,
    /// The commit the chain's tree was created at (`jobs.pinned_sha`).
    pub pinned_sha: String,
    /// The reservation from [`resume_reservation`], standing in for the
    /// per-kind estimate at the budget gate.
    pub anticipated: i64,
    /// The three terms `anticipated` was computed from, kept so the
    /// event logged at dispatch can show its arithmetic. An operator
    /// looking at a large reservation needs to see whether it is a large
    /// context or a chain that has overrun, and re-deriving them at the
    /// log site would mean re-reading the transcript and the chain.
    pub ctx: i64,
    pub typical: i64,
    pub chain_spent: i64,
    /// A handoff rather than a resume: the predecessor finished, and its
    /// session carries on into a DIFFERENT job in the same tree — an
    /// engage that withdrew its PR continuing into the closed PR's
    /// harvest ([`continue_into_harvest`]). The worker already holds the
    /// PR, its discussion and its own reasons for withdrawing, so it gets
    /// the new job's playbook where a resume gets a one-line "carry on";
    /// and a failed handoff changes nothing, since the cold harvest still
    /// runs later.
    pub handoff: bool,
    /// The hold an operator released with `continue` to get here, if this
    /// is the first attempt since: its reason and detail. The resume prompt
    /// is chosen from the reason (WORK-MODEL.md §9.1).
    pub after_hold: Option<(HoldReason, String)>,
}

impl ResumePlan {
    /// The job this plan starts: a handoff of its engage, or a resume of
    /// its checkpoint. Either runs on the predecessor's place, which the
    /// insert reads from that row.
    pub const fn start(&self) -> JobStart {
        if self.handoff {
            JobStart::Handoff {
                engage: self.predecessor_id,
            }
        } else {
            JobStart::Resume {
                checkpoint: self.predecessor_id,
            }
        }
    }
}

/// What `run_cycle` would act on right now, if invoked (scheduler.py
/// `pick_next` docstring). Enough for the summary preview and for
/// `anticipated_tokens(repo_id`, kind).
#[derive(Debug, Clone)]
pub enum Candidate {
    Repo {
        kind: RepoJobKind,
        repo_id: i64,
        label: Option<String>,
    },
    Finding {
        kind: FindingJobKind,
        finding_id: i64,
        repo_id: i64,
        label: Option<String>,
        budget_override: Option<BudgetOverride>,
    },
    /// Continue a suspended attempt instead of redoing it.
    ///
    /// `label` and `budget_override` are the tier's own: a finding tier
    /// that continues its finding's suspended attempt still shows that
    /// finding and still runs under that finding's override, exactly as
    /// the fresh candidate it replaces would have. The repo-level resume
    /// tier carries the repo name and no override.
    Resume {
        label: Option<String>,
        budget_override: Option<BudgetOverride>,
        /// Boxed: a plan carries its workspace's paths and dwarfs the
        /// other variants.
        plan: Box<ResumePlan>,
    },
}

impl Candidate {
    pub fn job_kind(&self) -> JobKind {
        match self {
            Self::Repo { kind, .. } => (*kind).into(),
            Self::Finding { kind, .. } => (*kind).into(),
            Self::Resume { plan, .. } => plan.kind,
        }
    }

    pub fn repo_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } | Self::Finding { repo_id, .. } => *repo_id,
            Self::Resume { plan, .. } => plan.repo_id,
        }
    }

    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Repo { label, .. } | Self::Finding { label, .. } | Self::Resume { label, .. } => {
                label.as_deref()
            }
        }
    }

    /// The primary target ID: `finding_id` for finding kinds, `repo_id` for repo kinds.
    pub fn target_id(&self) -> i64 {
        match self {
            Self::Repo { repo_id, .. } => *repo_id,
            Self::Finding { finding_id, .. } => *finding_id,
            Self::Resume { plan, .. } => plan.finding_id.unwrap_or(plan.repo_id),
        }
    }

    pub fn budget_override(&self) -> Option<BudgetOverride> {
        match self {
            Self::Finding {
                budget_override, ..
            }
            | Self::Resume {
                budget_override, ..
            } => *budget_override,
            Self::Repo { .. } => None,
        }
    }
}

/// Candidate for a finding-target kind (engage/harvest/recheck/fix).
/// label = summary || fingerprint under Python `or` semantics (findings
/// carry no "name" key, the `next_candidate` label in
/// `server.Handler._summary`): an empty summary falls through.
fn finding_candidate(kind: FindingJobKind, f: &Finding) -> Candidate {
    let label = if f.summary.is_empty() {
        f.fingerprint.clone()
    } else {
        f.summary.clone()
    };
    Candidate::Finding {
        kind,
        finding_id: f.id,
        repo_id: f.repo_id,
        label: Some(label),
        budget_override: f.budget_override,
    }
}

/// Candidate for a repo-target kind (`hunt/test_gap/dep_update/refactor`/
/// modernization/standards). label = name (repos carry neither "summary"
/// nor "fingerprint").
fn repo_candidate(kind: RepoJobKind, r: &Repo) -> Candidate {
    Candidate::Repo {
        kind,
        repo_id: r.id,
        label: (!r.name.is_empty()).then(|| r.name.clone()),
    }
}

async fn findings_with_status(store: &Store, status: FindingStatus) -> sqlx::Result<Vec<Finding>> {
    store
        .list_findings(&FindingFilter {
            status: Some(status),
            ..FindingFilter::default()
        })
        .await
}

/// The engage tier's work: `pr_open` findings whose PR is flagged for
/// attention (`pr_state.needs_attention` set) or whose engage has a
/// checkpoint, with the ids of the flagged ones. Flagged ones first, then
/// by `COALESCE(attention_since, synced_at)` ascending — oldest-outstanding
/// reason first, NULL first (SQLite NULL-first ASC == Option's None < Some).
///
/// Engage is active while the finding is `pr_open` (WORK-MODEL.md §6.1,
/// G1): the flag only decides whether it may start fresh. `sync_prs`
/// recomputes the flag every cycle and it can flap, so a paused engage
/// whose PR lost its flag for a cycle is still continued here, where it
/// used to wait for the resume tier below every finding tier.
async fn list_engage(store: &Store) -> sqlx::Result<(Vec<Finding>, BTreeSet<i64>)> {
    let paused = store
        .findings_with_checkpoint(FindingJobKind::Engage)
        .await?;
    let mut flagged = BTreeSet::new();
    let mut rows: Vec<((bool, Option<i64>), Finding)> = Vec::new();
    for f in findings_with_status(store, FindingStatus::PrOpen).await? {
        let ps = store.get_pr_state(f.id).await?;
        let flag = ps.as_ref().is_some_and(|p| p.needs_attention.is_some());
        if flag {
            flagged.insert(f.id);
        }
        if flag || paused.contains(&f.id) {
            let key = ps.and_then(|p| p.attention_since.or(p.synced_at));
            rows.push(((!flag, key), f));
        }
    }
    // Flagged first: a human is waiting on that PR. Stable, like SQLite's
    // unspecified tie order.
    rows.sort_by_key(|(key, _)| *key);
    Ok((rows.into_iter().map(|(_, f)| f).collect(), flagged))
}

/// Selection — replicates `scheduler.pick_next`'s priority order and
/// per-kind eligibility conditions.
///
/// Priority (`scheduler.pick_next`): a budget-overridden finding (any
/// category) jumps the queue -> flagged PR (oldest-outstanding reason
/// first) -> oldest merged or closed PR pending its harvest
/// ([`Store::list_pending_harvest`]) -> oldest
/// rechecking -> oldest queued fix -> resumable suspended work -> the
/// most stale-of-rotation job type for the least-recently-hunted enabled
/// repo (hunt if never cloned, else whichever of
/// `hunt/test_gap/dep_update/refactor` is oldest/never-run subject to
/// `cfg.scan_interval_days`; modernization gated by
/// `cfg.modernization_interval_days`). All types gated for one repo ->
/// the next-stalest repo is tried. None = nothing to do.
///
/// Each finding tier continues a suspended attempt at its own
/// (finding, kind) instead of starting that work over
/// ([`finding_pick`]).
///
/// The scheduler's own selection. On the way it ends checkpoints that
/// can never be continued — a chain past the give-up ceiling, or one
/// whose working directory is gone — which [`resume_plan`] does as it
/// skips over them ([`end_checkpoint`]), and counts a given-up chain as a
/// failed attempt at its work ([`count_given_up_chain`],
/// [`count_given_up_repo_chain`]).
/// [`preview_next`] makes the same selection and writes nothing.
pub async fn pick_next(
    store: &Store,
    cfg: &Config,
    force_repo: Option<&str>,
) -> anyhow::Result<Option<Candidate>> {
    select(store, cfg, force_repo, Pick::Run).await
}

/// The `/api/summary` "what's next" preview: [`pick_next`]'s selection
/// without any of its writes.
///
/// The dashboard polls this, scheduler paused or not, and a poll must not
/// do the scheduler's work: ending a chain, or counting a given-up chain
/// against its work, which can hold that work for the operator. A
/// checkpoint the scheduler would end is only skipped here, so the
/// preview names the fresh start a given-up chain makes way for, even
/// where the scheduler's count ends that work instead.
pub async fn preview_next(store: &Store, cfg: &Config) -> anyhow::Result<Option<Candidate>> {
    select(store, cfg, None, Pick::Preview).await
}

/// Whether a selection records what it finds on the way.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pick {
    /// The scheduler's own: [`pick_next`].
    Run,
    /// The summary's: [`preview_next`], which writes nothing.
    Preview,
}

/// [`pick_next`], or with [`Pick::Preview`] [`preview_next`].
#[allow(
    clippy::too_many_lines,
    reason = "one tier after another in priority order; splitting it would \
              scatter the order that is the whole point of the function"
)]
async fn select(
    store: &Store,
    cfg: &Config,
    force_repo: Option<&str>,
    mode: Pick,
) -> anyhow::Result<Option<Candidate>> {
    let mut rechecking = findings_with_status(store, FindingStatus::Rechecking).await?;
    let (mut attention, flagged) = list_engage(store).await?;
    let mut pending_harvest = store.list_pending_harvest().await?;
    let mut queued = findings_with_status(store, FindingStatus::Queued).await?;

    let force_id: Option<i64> = if let Some(name) = force_repo {
        let r = store
            .get_repo_by_name(name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown repo {name:?}"))?;
        Some(r.id)
    } else {
        None
    };

    if let Some(rid) = force_id {
        rechecking.retain(|f| f.repo_id == rid);
        attention.retain(|f| f.repo_id == rid);
        pending_harvest.retain(|f| f.repo_id == rid);
        queued.retain(|f| f.repo_id == rid);
    }
    // Held work waits for the operator: every tier, the resume tier and the
    // repo rotation skip it, through the one shared query (WORK-MODEL.md
    // §9.5, invariant 3; §17).
    let held = store.held_work().await?;
    let held_ref = &held;
    let not_held = |kind: FindingJobKind| move |f: &Finding| !held_ref.finding(f.id, kind);
    rechecking.retain(not_held(FindingJobKind::Recheck));
    attention.retain(not_held(FindingJobKind::Engage));
    pending_harvest.retain(not_held(FindingJobKind::Harvest));
    queued.retain(not_held(FindingJobKind::Fix));
    // A place a job is running on takes no second attempt: the insert
    // would be refused (migration 031). It is skipped like held work, so a
    // job left `running` by a stray worker keeps only its own place, not
    // the whole selection, from being picked.
    let running = store.running_places().await?;
    let busy = |place: JobPlace| running.contains(&place);
    for tier in [
        &mut rechecking,
        &mut attention,
        &mut pending_harvest,
        &mut queued,
    ] {
        tier.retain(|f| !busy(JobPlace::Finding(f.id)));
    }

    // list_findings is id DESC: oldest first, as the engage and harvest
    // lists already are.
    rechecking.reverse();
    queued.reverse();
    // A finding may start its work fresh unless it is an engage whose PR
    // is not flagged: that one is here only to continue its checkpoint.
    let may_start = |kind: FindingJobKind, f: &Finding| {
        kind != FindingJobKind::Engage || flagged.contains(&f.id)
    };
    let tiers = [
        (FindingJobKind::Engage, &attention),
        (FindingJobKind::Harvest, &pending_harvest),
        (FindingJobKind::Recheck, &rechecking),
        (FindingJobKind::Fix, &queued),
    ];

    // (1) A budget-overridden finding jumps the queue, scanned in the
    // same kind order the normal priorities use (`scheduler.pick_next`).
    // (2)-(5) Then the normal finding-kind priorities, in that order,
    // oldest first.
    for overridden in [true, false] {
        for (kind, items) in tiers {
            for f in items
                .iter()
                .filter(|f| !overridden || f.budget_override.is_some())
            {
                match finding_pick(store, cfg, kind, f, may_start(kind, f), mode).await? {
                    Picked::Work(c) => return Ok(Some(c)),
                    // Its given-up chain just ended `f`'s work, which may
                    // have held it: select again, from the top.
                    Picked::Ended => {
                        return Box::pin(select(store, cfg, force_repo, mode)).await;
                    }
                    // A flagless engage whose checkpoint could not be
                    // continued: nothing to do here, and nothing changed.
                    Picked::Nothing => {}
                }
            }
        }
    }

    // (6) Work already paid for: a suspended attempt whose transcript is
    // still on disk. This sits below every tier above it because those
    // are all a human waiting on a pull request — a flagged review, a
    // merged branch, a recheck they asked for, a fix they queued. A
    // resume is background work, and it outranks *starting* background
    // work because continuing costs the context at suspension (median
    // re-cache ratio 1.00 across 112 production events) where restarting
    // costs a flat ~37,000-token session floor plus every token already
    // spent, and advances no watermark to show for it.
    match pick_resume(store, cfg, force_id, (&held, &running), mode).await? {
        Picked::Work(c) => return Ok(Some(c)),
        // A repo chain given up there was counted as a failure, which may
        // have held its work: select again, from the top.
        Picked::Ended => return Box::pin(select(store, cfg, force_repo, mode)).await,
        Picked::Nothing => {}
    }

    // (7) Repo rotation: enabled repos in staleness order — a repo with an
    // urgent full re-hunt request first, then never-hunted, then oldest
    // last_hunt_at first; ties keep list_repos' name order (Python
    // sorted() and Vec::sort_by_key are both stable). Held work is
    // skipped: a held `sync` takes the whole repo out of the rotation, a
    // held kind only that kind. A request whose hunt is held, or still
    // running, is not urgent: it waits for that hunt without putting the
    // repo's other kinds ahead of every other repo.
    let mut repos: Vec<Repo> = store
        .list_repos()
        .await?
        .into_iter()
        .filter(|r| {
            if let Some(rid) = force_id {
                r.id == rid
            } else {
                r.enabled != 0
            }
        })
        .collect();
    let hunt_busy = |r: &Repo| {
        held.repo(r.id, RepoJobKind::Hunt)
            || busy(JobPlace::of(RepoJobKind::Hunt.into(), r.id, None))
    };
    repos.sort_by_key(|r| {
        (
            !full_hunt_urgent(r) || hunt_busy(r),
            r.last_hunt_at.is_some(),
            r.last_hunt_at.unwrap_or(0),
        )
    });

    let now = now_ms();
    for target in &repos {
        if held.repo_sync(target.id) {
            continue;
        }
        let repo_busy = |kind: RepoJobKind| {
            held.repo(target.id, kind) || busy(JobPlace::of(kind.into(), target.id, None))
        };
        let hunt_held = hunt_busy(target);
        if !Path::new(&target.path).exists() {
            // Not cloned yet -> hunt does the clone (`scheduler.pick_next`).
            if hunt_held {
                continue;
            }
            return Ok(Some(repo_candidate(RepoJobKind::Hunt, target)));
        }

        // An operator asked for a full re-hunt: that hunt goes ahead of
        // the repo's other scans and of the scan interval.
        if full_hunt_urgent(target) && !hunt_held {
            return Ok(Some(repo_candidate(RepoJobKind::Hunt, target)));
        }

        if let Some(kind) = rotation_kind(cfg, target, now, repo_busy) {
            return Ok(Some(repo_candidate(kind, target)));
        }
        // All scan types ran within their intervals for this repo.
    }

    Ok(None)
}

/// The rotation scan `target` is due for at `now`, if any, among the kinds
/// that are not `held`.
fn rotation_kind(
    cfg: &Config,
    target: &Repo,
    now: i64,
    held: impl Fn(RepoJobKind) -> bool,
) -> Option<RepoJobKind> {
    let scan_interval_ms = (cfg.scan_interval_days * 86_400_000.0) as i64;
    let mod_interval_ms = cfg.modernization_interval_days * 86_400_000;
    let std_interval_ms = cfg.standards_interval_days * 86_400_000;

    // Eligible job types with their last-run timestamps, in
    // RepoJobKind::ALL insertion order (mirrors Python's dict).
    let mut job_times: Vec<(RepoJobKind, i64)> = Vec::with_capacity(RepoJobKind::ALL.len());
    for (kind, last, interval_ms) in [
        (RepoJobKind::Hunt, target.last_hunt_at, scan_interval_ms),
        (
            RepoJobKind::TestGap,
            target.last_test_gap_at,
            scan_interval_ms,
        ),
        (
            RepoJobKind::DepUpdate,
            target.last_dep_update_at,
            scan_interval_ms,
        ),
        (
            RepoJobKind::Refactor,
            target.last_refactor_at,
            scan_interval_ms,
        ),
        (
            RepoJobKind::Modernization,
            target.last_modernization_at,
            mod_interval_ms,
        ),
        (
            RepoJobKind::Standards,
            target.last_standards_at,
            std_interval_ms,
        ),
    ] {
        let last = last.unwrap_or(0);
        if !held(kind) && (last == 0 || (now - last) >= interval_ms) {
            job_times.push((kind, last));
        }
    }

    // never-run beats stale-run; both tie-break on JOB_TYPE_PRIORITY,
    // which is job_times' insertion order (Python: next() over
    // _JOB_TYPE_PRIORITY, resp. min() first-wins in insertion order —
    // Iterator::min_by_key also returns the FIRST minimal element).
    job_times
        .iter()
        .find(|&&(_, last)| last == 0)
        .or_else(|| job_times.iter().min_by_key(|&&(_, last)| last))
        .map(|&(kind, _)| kind)
}

/// Whether `repo` has a full re-hunt request that no hunt has started to
/// answer yet.
///
/// Answered by identity, not by time: a hunt records the request it read
/// (`full_hunt_request_attempted`) once it has attempted it, so neither a
/// hunt already running when the request landed nor one prepared from a
/// repo row read before it takes the request's place in the queue. Once a
/// hunt has attempted the request, it stops jumping the queue — a failing
/// hunt takes its place in the rotation like any other, bounded by its
/// failure streak — but stays pending, so the repo's next hunt is still a
/// full one, and a held hunt leaves it pending until a full-scope hunt
/// succeeds. Asking again makes it urgent again.
fn full_hunt_urgent(repo: &Repo) -> bool {
    repo.full_hunt_requested_at.is_some()
        && repo.full_hunt_requested_at != repo.full_hunt_request_attempted
}

// ---------------------------------------------------------------------------
// Resume policy
// ---------------------------------------------------------------------------

/// Floor on the work half of any attempt's reservation.
///
/// The work half is whatever an attempt is budgeted beyond the context
/// it must load first, and it can come out tiny or negative: a chain
/// that has already outspent the per-kind typical `z` leaves
/// `z - chain_spent` below zero, and a negative budget for the work
/// still to do is not a small budget, it is a meaningless one. Floored
/// here at enough for the worker to do real work after loading, rather
/// than exactly enough to load and be killed again with nothing to
/// show, which would turn every resume into another suspension.
const MIN_PROGRESS_TOKENS: i64 = 25_000;

/// The system prompt and tool schemas a cold session loads before it
/// reads a single job-specific byte.
///
/// The smallest first call measured is 17,257 tokens. Kept below that
/// so it under-estimates: it feeds the floor arm of a `max()`, where
/// guessing high would refuse work the window could have funded.
const START_CONTEXT_FLOOR_TOKENS: i64 = 15_000;

/// The fraction of an attempt's budget that must go to work rather than
/// to loading context.
///
/// Every attempt pays to load its context before it can do anything —
/// a cold one its system prompt and tools, a resumed one its whole
/// transcript, re-sent at a measured median ratio of 1.00. That is
/// overhead; only what is left is work. Under a flat 25,000-token work
/// floor a resume with a 100,000-token transcript reserved 125,000, and
/// when the window had just that much room the attempt it admitted
/// spent 80% of its budget re-sending the transcript and 20% working.
/// Holding every start to at least this fraction of work makes the same
/// resume reserve 200,000, so it waits for a window that can fund an
/// attempt that is at least half work.
const MIN_START_EFFICIENCY: f64 = 0.5;

/// The least work budget worth paying `ctx` tokens of loading for.
///
/// Solves `work / (ctx + work) >= MIN_START_EFFICIENCY` for `work`, and
/// never below [`MIN_PROGRESS_TOKENS`]: a small context would otherwise
/// be funded for a few thousand tokens of work, too little to finish
/// anything.
fn min_useful(ctx: i64) -> i64 {
    // One expression rather than a named `eff / (1 - eff)` ratio: at 0.5
    // that ratio is exactly 1, so multiplying `ctx` by it and dividing
    // by it agree, and no reservation could show which one the code
    // does. Spelled out, a wrong operator anywhere here moves the
    // 100,000-token resume off 200,000.
    let work = ctx as f64 * MIN_START_EFFICIENCY / (1.0 - MIN_START_EFFICIENCY);
    MIN_PROGRESS_TOKENS.max(work.ceil() as i64)
}

/// Multiple of the per-kind typical cost at which a chain is abandoned.
///
/// Applied to what the chain spent OUTSIDE its single biggest attempt:
/// work that has burned three typical runs' worth on top of its one
/// most expensive try, and is still not finished, is not finishing.
/// Without a ceiling the chain resumes forever, each link cheap enough
/// to look reasonable — the exact loop this feature exists to end, only
/// slower.
const GIVE_UP_MULTIPLE: i64 = 3;

/// How many attempts one piece of work gets before the chain is
/// abandoned regardless of what it has cost.
///
/// The token arm below cannot bound a chain whose every attempt is
/// cheap: a suspension that resumes, does a little, and suspends again
/// runs forever without ever tripping a spend threshold. This arm is
/// also the one a reader can reason about, because it is expressed in
/// the unit the question is actually asked in — how many times have we
/// tried this.
const MAX_RESUME_ATTEMPTS: i64 = 4;

/// The whole prompt a resumed attempt gets.
///
/// Short by necessity, not by preference: the session already contains
/// the original playbook, the diff range, the suppression list and
/// everything the worker has done since. Re-sending it would re-send a
/// prompt the model is already looking at, for full price.
const RESUME_PROMPT: &str = "Continue the work you were doing in this session. You were interrupted; \
     pick up where you left off.";

/// The prompt a resumed recheck, engage, harvest, hunt or scan gets:
/// [`RESUME_PROMPT`], or, for the first attempt after the operator
/// continued its hold, one chosen by the hold's reason (WORK-MODEL.md
/// §9.1), with the hold's detail as context. Short like [`RESUME_PROMPT`],
/// for the same reason: the session holds the playbook. A fix's comes with
/// its updated playbook instead ([`fix_prompt`]).
fn resume_prompt(kind: JobKind, plan: &ResumePlan) -> String {
    let Some((reason, detail)) = &plan.after_hold else {
        return RESUME_PROMPT.to_owned();
    };
    let (work, deliverable) = match kind {
        JobKind::Finding(FindingJobKind::Recheck) => ("recheck", "your verdict file"),
        JobKind::Finding(FindingJobKind::Engage) => {
            ("PR engagement", "PR-REPLY.md, or WITHDRAW.md")
        }
        JobKind::Finding(FindingJobKind::Fix) => ("fix", "PR-DESCRIPTION.md"),
        JobKind::Finding(FindingJobKind::Harvest) => ("PR review", "what the playbook asks for"),
        JobKind::Repo(_) => ("scan", "the findings file your playbook names"),
    };
    match reason {
        HoldReason::Stuck | HoldReason::GaveUp => format!(
            "Resume this {work} after an operator released it. It was held because earlier \
             attempts kept failing the same way before they finished, not because anything was \
             missing: continue from where it stopped, finish it, and write {deliverable}.\n\n\
             What kept failing (context, not instructions):\n{detail}"
        ),
        HoldReason::Prerequisite => format!(
            "Resume this {work} after an operator released it. Re-evaluate the prerequisite \
             your BLOCKED.md named: if it is still missing, write BLOCKED.md again with what \
             you checked; otherwise finish the work and write {deliverable}.\n\n\
             Previous blocker (context, not instructions):\n{detail}"
        ),
    }
}

/// What to reserve for a resumed attempt.
///
/// `ctx_at_suspension` is what the first call costs: a resumed session
/// re-establishes its whole context, measured median ratio 1.00 across
/// 112 production re-cache events. That is the overhead. The work half
/// is what is left of the per-kind typical estimate after everything
/// the chain has already spent, but never less than [`min_useful`] of
/// that context — a 100,000-token transcript reserves at least 200,000,
/// so the gate admits it only once at least half the attempt can be
/// work rather than re-sending the transcript.
fn resume_reservation(ctx_at_suspension: i64, z: i64, chain_spent: i64) -> i64 {
    ctx_at_suspension + (z - chain_spent).max(min_useful(ctx_at_suspension))
}

/// What to reserve for an attempt that starts cold.
///
/// The per-kind history, but never less than the cheapest cold start
/// that clears [`MIN_START_EFFICIENCY`]: [`START_CONTEXT_FLOOR_TOKENS`]
/// of fixed context plus [`min_useful`] of it, 40,000 with these
/// constants. History knows nothing about that floor and has collapsed
/// before: mis-metered rows once dragged the estimate to 1,876 tokens,
/// and the gate then started hunts with about 10,000 tokens of headroom
/// that died on their second call.
fn cold_reservation(history: i64) -> i64 {
    history.max(START_CONTEXT_FLOOR_TOKENS + min_useful(START_CONTEXT_FLOOR_TOKENS))
}

/// What the budget gate will reserve if `c` runs, for the
/// `/api/summary` preview: a preview that reserved anything else would
/// show a budget decision the scheduler is not going to make.
///
/// Unlike the gate, a failed history read is an error here rather than
/// a zero — the preview reports it instead of guessing.
pub async fn candidate_reservation(
    store: &Store,
    cfg: &Config,
    c: &Candidate,
) -> anyhow::Result<i64> {
    Ok(match c {
        Candidate::Resume { plan, .. } => plan.anticipated,
        Candidate::Repo { .. } | Candidate::Finding { .. } => {
            cold_reservation(anticipated_tokens(store, cfg, c.repo_id(), c.job_kind()).await?)
        }
    })
}

/// The resume tier of [`pick_next`]: the newest suspension that is still
/// worth continuing.
///
/// Candidates are walked newest-first; [`resume_plan`] decides each one
/// and ends those that can never be continued, so the walk moves on to
/// the next in the same cycle — which is what "fall through to normal
/// selection" means. Held work is skipped, a repo's whole work while its
/// `sync` is held. A repo chain the give-up ceiling ends is counted as a
/// failure of its work ([`count_given_up_repo_chain`]): [`Picked::Ended`].
/// A finding's is counted by its own tier ([`finding_pick`]).
async fn pick_resume(
    store: &Store,
    cfg: &Config,
    force_id: Option<i64>,
    (held, running): (&crate::store::HeldWork, &HashSet<JobPlace>),
    mode: Pick,
) -> anyhow::Result<Picked> {
    for job in store.list_resumable_jobs().await? {
        if force_id.is_some_and(|rid| job.repo_id != rid) {
            continue;
        }
        let is_held = match (job.kind, job.finding_id) {
            (JobKind::Finding(kind), Some(fid)) => held.finding(fid, kind),
            (JobKind::Repo(kind), _) => held.repo(job.repo_id, kind),
            (JobKind::Finding(_), None) => false,
        };
        if is_held || running.contains(&JobPlace::of(job.kind, job.repo_id, job.finding_id)) {
            continue;
        }
        let (kind, repo, job_id) = (job.kind, (job.repo_id, job.repo_name.clone()), job.id);
        match resume_plan(store, cfg, job, mode).await? {
            Resumable::Yes(plan) => {
                return Ok(Picked::Work(Candidate::Resume {
                    label: (!plan.repo.is_empty()).then(|| plan.repo.clone()),
                    budget_override: None,
                    plan,
                }));
            }
            Resumable::GaveUp(why) => {
                if let JobKind::Repo(kind) = kind {
                    count_given_up_repo_chain(store, (repo.0, &repo.1), kind, job_id, &why).await;
                    return Ok(Picked::Ended);
                }
            }
            Resumable::No => {}
        }
    }
    Ok(Picked::Nothing)
}

/// A finding tier's pick: continue the work item's checkpoint when it has
/// one that is resumable, otherwise start the work.
///
/// Without this the finding tiers, which outrank the resume tier, never
/// see the checkpoint: they start the work fresh — paying the session
/// floor plus everything the paused attempt already spent. The
/// replacement keeps the tier's position, label and budget override;
/// only the plan differs.
///
/// [`Picked::Ended`] when a chain given up here was the failure that ended
/// `f`'s work ([`count_given_up_chain`]): `f` is no longer this tier's to
/// pick. [`Picked::Nothing`] when nothing can be continued and `may_start`
/// is false: an engage whose PR is not flagged is in its tier only to
/// continue.
async fn finding_pick(
    store: &Store,
    cfg: &Config,
    kind: FindingJobKind,
    f: &Finding,
    may_start: bool,
    mode: Pick,
) -> anyhow::Result<Picked> {
    let fresh = || {
        if may_start {
            Picked::Work(finding_candidate(kind, f))
        } else {
            Picked::Nothing
        }
    };
    let Some(job) = store.checkpoint_job(f.id, kind).await? else {
        return Ok(fresh());
    };
    let job_id = job.id;
    Ok(match resume_plan(store, cfg, job, mode).await? {
        Resumable::Yes(plan) => {
            let tier = finding_candidate(kind, f);
            Picked::Work(Candidate::Resume {
                label: tier.label().map(str::to_owned),
                budget_override: tier.budget_override(),
                plan,
            })
        }
        Resumable::GaveUp(why) => {
            if count_given_up_chain(store, kind, f.id, job_id, &why).await {
                Picked::Ended
            } else {
                fresh()
            }
        }
        Resumable::No => fresh(),
    })
}

/// What [`finding_pick`] made of one finding of a tier, or [`pick_resume`]
/// of the resume tier.
enum Picked {
    /// Run this.
    Work(Candidate),
    /// Work ended here (a given-up chain was counted); the selection
    /// starts over, since what that wrote can change every tier.
    Ended,
    /// Nothing to do for it, and nothing changed.
    Nothing,
}

/// The failure a given-up chain records in its work item's streak.
const CHAIN_GAVE_UP: &str = "resume chain gave up";

/// Count a repo chain the give-up ceiling ended (`why`) as one failed
/// attempt at its work, the failure [`CHAIN_GAVE_UP`] (WORK-MODEL.md §17).
/// It replaces the rotation bump that used to end the chain's turn: the
/// work stays as stale as it was, so the rotation starts it fresh at once,
/// and three given-up chains in a row hold it as `gave_up`, with the chain
/// as its checkpoint for a `continue`.
async fn count_given_up_repo_chain(
    store: &Store,
    (repo_id, name): (i64, &str),
    kind: RepoJobKind,
    job: i64,
    why: &str,
) {
    let detail = format!("{CHAIN_GAVE_UP}: {why}");
    record_repo_failure(
        store,
        (repo_id, name),
        (kind.into(), Some(job)),
        CHAIN_GAVE_UP,
        &detail,
        HoldReason::GaveUp,
    )
    .await;
}

/// Count a chain of this finding tier's work that the give-up ceiling
/// ended (`why`) as one failed attempt at that work.
///
/// Every attempt in the chain was a suspension, and a suspension never
/// counts toward the streak, so without this the finding is still in its
/// tier when the chain ends: the tier starts the work fresh in the same
/// cycle, and a worker that overruns its wall clock every time takes
/// every cycle forever. Counted through the executors' own failure paths
/// ([`handle_recheck_failure`], [`record_fix_failure`],
/// [`record_harvest_failure`], [`record_engage_failure`]), three given-up
/// chains in a row hold the work exactly as three identical failures do.
/// Only here, where `f` is known to still be in this tier: the resume tier
/// also ends chains, for findings a human may since have moved on.
///
/// True when this count ended the work, so the finding has left its
/// tier.
async fn count_given_up_chain(
    store: &Store,
    kind: FindingJobKind,
    fid: i64,
    job: i64,
    why: &str,
) -> bool {
    let failure = CHAIN_GAVE_UP;
    let mut summary = CycleSummary::default();
    match kind {
        FindingJobKind::Recheck => {
            let detail = format!("{failure}: {why}");
            handle_recheck_failure(
                store,
                &mut summary,
                fid,
                job,
                failure,
                &detail,
                HoldReason::GaveUp,
                None,
            )
            .await;
            summary.outcome.as_deref() == Some("held")
        }
        // Only a hold that landed takes the fix out of its tier's picks.
        FindingJobKind::Fix => {
            record_fix_failure(
                store,
                fid,
                job,
                failure,
                why,
                HoldReason::GaveUp,
                &mut summary,
            )
            .await;
            summary.outcome.as_deref() == Some("held")
        }
        FindingJobKind::Harvest => {
            let detail = format!("{failure}: {why}");
            record_harvest_failure(store, fid, Some(job), failure, &detail, HoldReason::GaveUp)
                .await
        }
        FindingJobKind::Engage => {
            let detail = format!("{failure}: {why}");
            record_engage_failure(store, fid, Some(job), failure, &detail, HoldReason::GaveUp).await
        }
    }
}

/// What [`resume_plan`] made of one suspension.
enum Resumable {
    /// Continue it with this plan.
    Yes(Box<ResumePlan>),
    /// The chain was past the give-up ceiling, and its checkpoint is now
    /// ended: why.
    GaveUp(String),
    /// Not resumable for any other reason.
    No,
}

/// How to continue one resumable job, or why it cannot be.
///
/// Checks the two things the database cannot know. The chain's
/// workspace must still hold its tree, and the transcript must live in
/// its session directory — a resumed worker continues a conversation, not
/// a filesystem, so pointing it at a tree that has since gone would have
/// it edit files that are not there. A row from before per-chain
/// workspaces fails this too: its transcript and tree are in the old
/// layout, and its row has no pinned commit. And the chain must be under
/// the give-up ceiling, unless it is a held fix an operator requeued.
///
/// Failing either is permanent, so the checkpoint is ended here rather
/// than skipped ([`end_checkpoint`]). A skip leaves it in place: it is
/// offered again every cycle and never goes away. Refusing from inside the
/// executor would be worse — this tier outranks repo rotation, so the same
/// hopeless job would be picked every cycle, starving every rotation
/// kind. Ending it takes it out of the walk in this same cycle. A
/// [`Pick::Preview`] only skips it, and leaves that to the scheduler's own
/// next selection.
async fn resume_plan(
    store: &Store,
    cfg: &Config,
    job: Job,
    mode: Pick,
) -> anyhow::Result<Resumable> {
    // Non-NULL by the query's own filter; a row that lost its path
    // between the read and here is simply not resumable.
    let Some(session_file) = job.session_file.as_deref().map(PathBuf::from) else {
        return Ok(Resumable::No);
    };
    let Some(repo) = store.get_repo_by_id(job.repo_id).await? else {
        return Ok(Resumable::No);
    };
    let origin_job_id = store.resume_origin_job(job.id).await?;
    let workspace = crate::workspace::Workspace::for_chain(
        &cfg.work_root,
        Path::new(&repo.path),
        origin_job_id,
    );
    let pinned_sha = store.pinned_sha(job.id).await?;
    let pinned_sha = match pinned_sha {
        Some(sha) if workspace.tree.is_dir() && session_file.starts_with(&workspace.session) => sha,
        _ => {
            // The transcript names files in that tree, so no later cycle
            // can resume it either.
            let msg = format!(
                "resume {} {}: job {} given up, workspace {} is gone",
                job.kind,
                repo.name,
                job.id,
                workspace.root.display()
            );
            if mode == Pick::Run {
                end_checkpoint(store, &job, &msg).await;
            }
            return Ok(Resumable::No);
        }
    };

    let z = anticipated_tokens(store, cfg, job.repo_id, job.kind)
        .await
        .unwrap_or(0);
    let chain = store.resume_chain_stats(job.id, None).await?;
    let chain_spent = chain.total;
    // The ceiling bounds what the scheduler continues on its own, so it
    // counts only the attempts and spend since the operator last released
    // this work (WORK-MODEL.md §8): a `continue` gives a fresh budget, and
    // nothing else does — not a daemon restart, a failed write or a
    // released claim. Before any release that is the whole chain.
    let (released_at, after_hold) = match (job.kind, job.finding_id) {
        (JobKind::Finding(kind), Some(fid)) => (
            store.last_release_at(fid, kind).await?,
            store.continued_hold(fid, kind, job.id).await?,
        ),
        (JobKind::Repo(kind), _) => (
            store.repo_last_release_at(job.repo_id, kind).await?,
            store.repo_continued_hold(job.repo_id, kind, job.id).await?,
        ),
        (JobKind::Finding(_), None) => (None, None),
    };
    let own = if released_at.is_some() {
        store.resume_chain_stats(job.id, released_at).await?
    } else {
        chain
    };
    // Two independent ways a chain runs out of road, and neither
    // implies the other: too many tries, or too much spent on the
    // tries other than the biggest one.
    //
    // The biggest attempt is set aside because one enormous attempt
    // is evidence about the SIZE OF THE JOB, not about the chain
    // being stuck — judging by it would retire big-but-healthy work
    // on its first resume. What the chain spent BESIDES that
    // attempt is the part that says continuing is not getting
    // anywhere.
    //
    // Subtracting rather than multiplying it is forced, not
    // stylistic: `chain_spent <= attempts * largest`, so any
    // threshold of the form `k * largest` is unreachable below
    // `attempts = k + 1` and would be decoration at k = 3.
    //
    // Both arms leave a chain of one alone. A first suspension is a
    // single attempt whose spend is, by definition of a cap kill, at
    // least its own cap; retiring on that would end the work before
    // resume had been tried even once, which is the opposite of what
    // this feature is for.
    let excess = own.total - own.max_single;
    let too_many = own.attempts >= MAX_RESUME_ATTEMPTS;
    let too_costly = z > 0 && own.attempts >= 2 && excess > GIVE_UP_MULTIPLE * z;
    if too_many || too_costly {
        let why = if too_many {
            format!("{} attempts, the limit", own.attempts)
        } else {
            format!("{excess} tok outside its largest attempt > {GIVE_UP_MULTIPLE}x {z} typical")
        };
        let msg = format!(
            "resume {} {}: giving up after {} tok across the chain ({why})",
            job.kind, repo.name, own.total
        );
        if mode == Pick::Preview {
            return Ok(Resumable::No);
        }
        // The chain's end counts as a failure of its work, where its tier
        // or the resume tier caught it ([`count_given_up_chain`],
        // [`count_given_up_repo_chain`]); nothing else moves. Best-effort:
        // the candidate is skipped either way, so a failed write only
        // defers the record — and only a landed give-up is reported as
        // one, so the chain is counted once.
        let retired = end_checkpoint(store, &job, &msg).await;
        return Ok(if retired {
            Resumable::GaveUp(msg)
        } else {
            Resumable::No
        });
    }

    // An unreadable transcript, or one with no usage record at all,
    // leaves the first call's cost unknown. `z` is the only other
    // estimate of this work that exists, so it stands in — better a
    // per-kind typical than a zero that would reserve nothing and
    // let the ramp grant a job it cannot afford.
    let ctx = crate::backends::omp_scavenge::harness::ctx_at_suspension(&session_file).unwrap_or(z);
    let anticipated = resume_reservation(ctx, z, chain_spent);

    // No "resuming" event here: this function is also the
    // /api/summary preview, which polls every few seconds, and an
    // event per poll would bury the log. `run_cycle_inner` logs it
    // when the cycle actually acts on the candidate.

    Ok(Resumable::Yes(Box::new(ResumePlan {
        kind: job.kind,
        repo_id: job.repo_id,
        repo: repo.name,
        finding_id: job.finding_id,
        predecessor_id: job.id,
        origin_job_id,
        session_file,
        workspace,
        pinned_sha,
        anticipated,
        ctx,
        typical: z,
        chain_spent,
        handoff: false,
        after_hold,
    })))
}

/// End the checkpoint `job` for good, logging `msg` as a `resume` event.
///
/// The work item drops its checkpoint pointer, a finding's or a repo's, and
/// the job row stays as it ended (WORK-MODEL.md §4). True when the write
/// landed; a failed one, or a pointer that has moved on since, leaves the
/// checkpoint for the next selection to try again.
async fn end_checkpoint(store: &Store, job: &Job, msg: &str) -> bool {
    let dropped = match (job.kind, job.finding_id) {
        (JobKind::Finding(kind), Some(fid)) => store.drop_checkpoint(fid, kind, job.id, msg).await,
        (JobKind::Repo(kind), _) => {
            store
                .drop_repo_checkpoint(job.repo_id, kind, job.id, msg)
                .await
        }
        (JobKind::Finding(_), None) => return false,
    };
    dropped.unwrap_or(false)
}

/// Historical cost estimate for one (repo, kind): warm (finished a
/// non-denied job of this kind on this repo within `cache_ttl_s`) -> p50 of
/// what the kind's recently completed CHAINS cost, cold -> p90; empty
/// history -> 0 (`scheduler.anticipated_tokens`, exact index formula in
/// BACKEND-CONTRACT.md §1.9; the history itself is
/// `Store::kind_token_history`).
pub async fn anticipated_tokens(
    store: &Store,
    cfg: &Config,
    repo_id: i64,
    kind: JobKind,
) -> anyhow::Result<i64> {
    let cache_ttl_ms = (cfg.cache_ttl_s * 1000.0) as i64;
    let cutoff = now_ms() - cache_ttl_ms;
    let warm = store.has_warm_job(repo_id, kind.as_str(), cutoff).await?;
    let history = store.kind_token_history(kind.as_str()).await?;
    if history.is_empty() {
        return Ok(0);
    }
    let frac = if warm { 0.5 } else { 0.9 };
    let idx = ((history.len() as f64 * frac) as usize).min(history.len() - 1);
    Ok(history[idx])
}

// ---------------------------------------------------------------------------
// Executor functions: one per job kind, ported from scheduler.py
// ---------------------------------------------------------------------------

use std::path::PathBuf;

use crate::backend::{Backend, JobClass, Verdict};
use crate::forge::{self, PrView};
use crate::ingest::{EntryTypes, IngestResult, ingest_findings, ingest_scan_findings};
use crate::playbooks;
use crate::recovery::{CreatedJob, CycleScope, FixClaim, RecordedJob, Recovered, RunningJob};
use crate::types::RunResult;
use crate::workspace::{OpenWorkspace, TreeSpec, Workspace};
use serde::{Deserialize, Serialize};

/// Sync PR results — typed replacement for raw JSON blobs.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SyncResult {
    pub synced: i64,
    pub merged: i64,
    pub closed: i64,
    pub attention: i64,
    pub errors: i64,
}

/// Cycle summary — typed replacement for raw JSON returned by all
/// run_* functions. Fields are Option so each runner sets only what it needs.
/// Serialize skips None fields for clean JSON tracing output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CycleSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<JobKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(rename = "finding", skip_serializing_if = "Option::is_none")]
    pub finding_id: Option<i64>,
    #[serde(rename = "job", skip_serializing_if = "Option::is_none")]
    pub job_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<JobState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_new: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub denied: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// The host whose reachability probe denied the cycle (`denied` says
    /// why in prose); the status page shows such a cycle as offline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unreachable: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_rehunt: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingest: Option<IngestResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(rename = "pr", skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<SyncResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle: Option<String>,
}

/// Maximum consecutive identical failures before giving up (`scheduler.MAX_CONSECUTIVE_SAME_FAILURE`).
const MAX_CONSECUTIVE_SAME_FAILURE: i64 = 3;

/// Map `RunResult` to job state.
///
/// A cap kill that left a transcript behind is a PAUSE, not a failure.
/// The worker ran out of window headroom mid-thought and the record of
/// that thought is still on disk, so the work can be continued for the
/// price of re-caching it — measured median ratio 1.00 across 112
/// production re-cache events — instead of redone for a flat
/// ~37,000-token session floor plus everything already paid for. Calling
/// that outcome `killed` is what let one repo run ten consecutive hunts
/// over an identical diff range for 1.48M tokens, eight of them finding
/// nothing: a killed job advances no watermark, so the same work was
/// re-selected from scratch every cycle.
///
/// The session file is load-bearing, not incidental: resuming means
/// handing omp one exact path, and a cap kill with no transcript has
/// nothing to hand it.
///
/// A wallclock kill that did metered work and left a transcript is a
/// pause too. Long work legitimately outlives one wall-clock slot: F#4082's
/// engage (jobs 4952-4954, 2026-09-30) was killed three times at 45 min
/// mid-remediation, and each fresh restart threw away ~260k tokens of
/// progress to redo the same opening moves. A genuine runaway is still
/// bounded — by [`MAX_RESUME_ATTEMPTS`] and the [`GIVE_UP_MULTIPLE`] spend
/// ceiling on the resume chain — rather than by discarding its work.
///
/// Every other `killed_reason` stays `Killed` and is never resumed.
///
/// A worker that exited unsuccessfully on its own -- a provider error
/// omp's own retries did not absorb, a dead connection after the host
/// slept, a SIGTERM from outside -- is a pause too, provided this attempt
/// did metered work (`tokens_new > 0`) and left a transcript. Job 4912
/// (2026-09-29) lost 105k tokens of test-gap work to one `aborted`
/// stream across a laptop suspend, and was then not retried for a day.
/// The work condition is what keeps configuration errors out: in the
/// whole job history every `model_not_supported` failure and every rate
/// limit hit before the first answer spent zero tokens, so it stays
/// `Failed` instead of being resumed into the same wall. A failure that
/// repeats after doing work is bounded by the resume give-up ceiling.
///
/// `resume-unavailable` is `Failed` rather than `Killed` because nothing
/// was spawned — there was no run to kill, the attempt could not start.
fn job_state(rr: &RunResult) -> FinishedState {
    match rr.killed_reason.as_deref() {
        Some("cap") if rr.session_file.is_some() => FinishedState::Suspended,
        Some("wallclock") if rr.session_file.is_some() && rr.tokens_new > 0 => {
            FinishedState::Suspended
        }
        Some("resume-unavailable") => FinishedState::Failed,
        Some(_) => FinishedState::Killed,
        None if rr.exit_code == Some(0) => FinishedState::Done,
        None if rr.session_file.is_some() && rr.tokens_new > 0 => FinishedState::Suspended,
        None => FinishedState::Failed,
    }
}

/// Turn a refused `create_job` into a skipped cycle, or the cycle's error.
///
/// A skip is a race this run lost to someone else's write between its
/// pick and its insert; the next selection reads the state that write
/// left, so it does not name the same work again:
/// - The repo was live when this run was picked and soft-deleted before
///   the job row went in. Only repo-level work can lose it:
///   `soft_delete_repo` refuses a repo any finding references, and
///   findings are never removed, so a finding's repo is never deleted.
/// - The checkpoint a resume continues is not its work item's checkpoint
///   any more (an operator restarted the work, or another attempt
///   continued it), or the engage a handoff continues was continued
///   already. Selection names a checkpoint by the same pointer the insert
///   reads, so only a write in between parts them.
/// - A harvest whose finding's PR no longer awaits one: another attempt
///   or an operator got there first, and the harvest queue no longer
///   lists the finding.
///
/// Reporting any of them as an error would log "cycle crashed", back every
/// other piece of work off for minutes and hand the operator a stack
/// trace for ordinary traffic. The skip names the kind and repo, never a
/// finding.
///
/// A place another `running` job still holds is an error, not a skip.
/// Selection never picks such a place ([`Store::running_places`]), so the
/// insert only meets one when a runner starts a second attempt itself
/// beside one it failed to end -- an engage handing off to its harvest
/// after the write ending the engage failed. That is a fault to surface.
/// The next selection skips the place, so the backoff does not repeat.
/// So is a finding with no row, and a genuine DB failure propagates too.
fn job_refused(kind: JobKind, repo: &str, err: JobInsertError) -> anyhow::Result<CycleSummary> {
    match err {
        JobInsertError::RepoDeleted { .. }
        | JobInsertError::NotContinuable { .. }
        | JobInsertError::NoHarvestDue { .. } => Ok(CycleSummary {
            kind: Some(kind),
            repo: Some(repo.to_owned()),
            skipped: Some(err.to_string()),
            ..Default::default()
        }),
        JobInsertError::Occupied { .. } | JobInsertError::UnknownFinding { .. } => Err(err.into()),
        JobInsertError::Db(e) => Err(e.into()),
    }
}

/// Record a completed job's outcome (`scheduler._record_job`).
///
/// `resume` is the plan this attempt ran under, if it was a resume. It
/// is here rather than in each executor because every executor funnels
/// through this one call, and the one outcome that needs it —
/// `resume-unavailable` — is identical for all six.
pub async fn record_job(
    store: &Store,
    running: RunningJob,
    rr: &RunResult,
    model: Option<&str>,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<RecordedJob> {
    let job_id = running.id();
    let state = job_state(rr);
    let notes = if state != FinishedState::Done && !rr.stdout_tail.is_empty() {
        Some(crate::util::tail(&rr.stdout_tail, 500))
    } else {
        None
    };
    let recorded = running
        .complete(&JobOutcome {
            state,
            tokens_new: rr.tokens_new,
            calls: rr.calls,
            exit_code: rr.exit_code.map(i64::from),
            killed_reason: rr.killed_reason.as_deref(),
            session_file: rr.session_file.as_deref(),
            notes,
            model,
            usage_delta: rr.usage_delta,
            finished_at: now_ms(),
        })
        .await?;

    // The transcript this attempt was to continue is gone, so nothing
    // ran. Nothing is retired: this attempt ended without pausing, so
    // `complete_job` has already moved its work item off the checkpoint,
    // and the predecessor's row stays as it ended. The event says so.
    //
    // Deliberately NOT restarting the work cold in this same cycle. omp
    // treats an unresolvable `--resume` path as permission to start a
    // FRESH session, write it at that path and exit 0, which is
    // indistinguishable downstream from a real continuation — so
    // "cannot resume" quietly becoming "start cold here" would pay a
    // full session floor while believing it had continued. The next
    // cycle picks the work up through normal selection, cold and
    // knowing it.
    //
    // Not for a handoff: its predecessor was never suspended. It finished,
    // and its work item is another kind's.
    if let Some(plan) = resume
        && !plan.handoff
        && rr.killed_reason.as_deref() == Some("resume-unavailable")
    {
        let msg = format!(
            "resume {} {}: session gone, checkpoint job {} given up",
            plan.kind, plan.repo, plan.predecessor_id
        );
        let _ = store
            .log_event("error", &msg, Some(job_id), plan.finding_id)
            .await;
    }
    Ok(recorded)
}

/// End a hold for the operator ([`Store::release_hold`]), and file what the
/// held work leaves behind that nothing else will: a harvest ended without
/// another review (`classify`, `mark_harvested`) files the `FOLLOW-UPS.json`
/// in its checkpoint's tree. The file is read before the release: the
/// release drops the checkpoint, and the daemon's workspace sweep, which
/// runs alongside the API, may release the tree as soon as it has. It is
/// filed only once the release has happened, best effort, as a concluding
/// harvest's is.
pub async fn release_hold(
    store: &Store,
    cfg: &Config,
    finding_id: i64,
    kind: FindingJobKind,
    release: &crate::store::HoldRelease<'_>,
) -> Result<(), StoreWriteError> {
    let ends_harvest = kind == FindingJobKind::Harvest
        && matches!(
            release.action,
            crate::domain::ReleaseAction::Classify | crate::domain::ReleaseAction::MarkHarvested
        );
    let followups = if ends_harvest {
        held_followups(store, cfg, finding_id, kind).await?
    } else {
        None
    };
    store.release_hold(finding_id, kind, release).await?;
    if let Some((repo_id, job, bytes)) = followups {
        // Staged under `out/`, which no sweep touches, for the ingest to
        // read as it reads a worker's tree.
        let staged = cfg.work_root.join("out").join(format!("release-{job}"));
        if std::fs::create_dir_all(&staged)
            .and_then(|()| std::fs::write(staged.join("FOLLOW-UPS.json"), &bytes))
            .is_ok()
        {
            ingest_followups(store, repo_id, &staged, finding_id, job, "harvest").await;
        }
        let _ = std::fs::remove_dir_all(&staged);
    }
    Ok(())
}

/// The `FOLLOW-UPS.json` in the tree of `finding_id`'s `kind` checkpoint,
/// with the repo and job to file it under; `None` without a checkpoint or
/// a file.
async fn held_followups(
    store: &Store,
    cfg: &Config,
    finding_id: i64,
    kind: FindingJobKind,
) -> sqlx::Result<Option<(i64, i64, Vec<u8>)>> {
    let Some(job) = store.checkpoint_job(finding_id, kind).await? else {
        return Ok(None);
    };
    let origin = store.resume_origin_job(job.id).await?;
    let Some(repo) = store.get_repo_by_id(job.repo_id).await? else {
        return Ok(None);
    };
    let ws = Workspace::for_chain(&cfg.work_root, Path::new(&repo.path), origin);
    Ok(std::fs::read(ws.tree.join("FOLLOW-UPS.json"))
        .ok()
        .map(|bytes| (repo.id, job.id, bytes)))
}

/// Ingest a worker's `FOLLOW-UPS.json`, returning what happened so the
/// caller can put it in the cycle summary. Rejections used to be visible
/// only as per-entry `ingest:` error events — the summary carried no
/// `ingest` block and the event below fired only when something was
/// inserted, so a harvest whose every follow-up was rejected logged
/// "harvested" and nothing else (observed 2026-09-12 and 2026-09-22:
/// four `test_gap` follow-ups silently dropped).
async fn ingest_followups(
    store: &Store,
    repo_id: i64,
    worktree: &Path,
    fid: i64,
    job: i64,
    kind: &str,
) -> Option<crate::ingest::IngestResult> {
    let followups_path = worktree.join("FOLLOW-UPS.json");
    if !followups_path.exists() {
        return None;
    }
    let counts = ingest_findings(
        store,
        repo_id,
        &followups_path,
        EntryTypes::Declared,
        Some(job),
        Some(fid),
    )
    .await;
    if counts.inserted > 0 || counts.invalid > 0 || counts.duplicates > 0 {
        let _ = store
            .log_event(
                kind,
                &format!(
                    "#{fid}: +{} follow-up(s) filed from deferred/superseded work ({} dup / {} invalid)",
                    counts.inserted, counts.duplicates, counts.invalid
                ),
                Some(job),
                Some(fid),
            )
            .await;
    }
    Some(counts)
}

/// Why a repo could not be synced: the fixed key its `sync` work item's
/// streak counts (WORK-MODEL.md §17), git's words as the detail, and
/// whether the network can cause it (a clone or a fetch), so that a host
/// that is unreachable right now makes it the network's failure instead.
#[derive(Debug)]
struct SyncFailure {
    key: &'static str,
    detail: String,
    network: bool,
}

/// The `sync` failure keys. Fixed strings, so one broken clone counts as
/// identical failures however git words it.
const CLONE_FAILED: &str = "clone failed";
const FETCH_FAILED: &str = "fetch failed";
const ORIGIN_MISMATCH: &str = "origin mismatch";
const BRANCH_NOT_FOUND: &str = "branch not found";
const REPO_NOT_CLONED: &str = "repo not cloned";

/// Clone the repo if it is not cloned yet, then fetch, under its clone's
/// permit (`crate::clones`); each command holds the permit on its blocking
/// thread, so one outliving its dropped future still holds the clone.
///
/// Fetch only — never checkout or pull. The clone is the object store
/// every chain's worktree is added from, not a working tree anyone runs
/// in: moving its HEAD would do nothing for the trees, and moving the
/// trees is exactly what must never happen under a suspended chain.
/// Logs nothing: the caller says whose work failed.
///
/// The URL is the repo row's as read under the permit
/// ([`Store::origin_url`]), not one the caller read before taking it: a
/// URL update holds the permit across its write and the clone's repoint,
/// so under the permit the row's URL and the clone's origin agree.
async fn sync_repo(
    store: &Store,
    permit: &ClonePermit<'_>,
) -> sqlx::Result<Result<(), SyncFailure>> {
    let repo_url = &store.origin_url(permit).await?;
    let rpath = permit.clone_dir();
    if rpath.exists() {
        // The directory already exists, so the clone is skipped -- which is
        // only safe if it is a clone of *this* repo. `repos/repo-<id>` is
        // derived from the id, so a directory a failed reclamation left
        // behind would otherwise be hunted under a later repo's identity
        // and pushed to that repo's URL.
        // Deletion now removes the directory, so reaching here means
        // something outside the daemon put it there: refuse rather than
        // guess.
        // The configured URL, not `remote get-url`: that one is rewritten
        // through `url.<base>.insteadOf`, while `git clone -- <url>` stored
        // `repo_url` as given, so a matching rule in the user's gitconfig
        // would make the daemon's own clone look like somebody else's.
        let (rc, out) = permit
            .blocking(|hold| hold.git(&["config", "--get", "remote.origin.url"], 60))
            .await
            .unwrap_or((127, "spawn error".to_owned()));
        let origin = out.trim();
        if rc != 0 || origin != repo_url {
            return Ok(Err(SyncFailure {
                key: ORIGIN_MISMATCH,
                detail: format!(
                    "{} exists but its origin is {} -- expected {repo_url}; \
                     refusing to work in a clone of a different repository",
                    rpath.display(),
                    if rc == 0 { origin } else { "unreadable" }
                ),
                network: false,
            }));
        }
    } else {
        if let Some(parent) = rpath.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let url = repo_url.clone();
        let (rc, out) = permit
            .blocking(move |hold| hold.clone_from(&url, 600))
            .await
            .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            return Ok(Err(SyncFailure {
                key: CLONE_FAILED,
                detail: format!("clone failed: {}", crate::util::tail(&out, 300)),
                network: true,
            }));
        }
    }
    let (rc, out) = fetch_held(permit, &[]).await;
    if rc != 0 {
        return Ok(Err(SyncFailure {
            key: FETCH_FAILED,
            detail: format!("git fetch origin failed: {}", crate::util::tail(&out, 300)),
            network: true,
        }));
    }
    Ok(Ok(()))
}

/// [`crate::workspace::fetch`] of `refs` on a blocking thread that holds
/// `permit` until the fetch and its housekeeping end. The only way the
/// daemon fetches into a clone.
async fn fetch_held(permit: &ClonePermit<'_>, refs: &[&str]) -> (i32, String) {
    let refs: Vec<String> = refs.iter().map(|&r| r.to_owned()).collect();
    permit
        .blocking(move |hold| {
            let refs: Vec<&str> = refs.iter().map(String::as_str).collect();
            crate::workspace::fetch(hold, &refs, 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()))
}

/// Sync `repo`'s clone ([`sync_repo`]) and resolve the tip of its default
/// branch: everything every kind of the repo needs before it can start.
/// Success ends the `sync` work item's streak.
async fn sync_to_tip(
    store: &Store,
    permit: &ClonePermit<'_>,
    repo: &Repo,
) -> sqlx::Result<Result<String, SyncFailure>> {
    if let Err(failure) = sync_repo(store, permit).await? {
        return Ok(Err(failure));
    }
    let rev = format!("origin/{}", repo.default_branch);
    let tip = permit
        .blocking(move |hold| crate::workspace::resolve(hold.clone_dir(), &rev))
        .await
        .ok()
        .flatten();
    let Some(tip) = tip else {
        return Ok(Err(SyncFailure {
            key: BRANCH_NOT_FOUND,
            detail: format!("origin/{} does not resolve", repo.default_branch),
            network: false,
        }));
    };
    log_write_failure(
        store.repo_work_succeeded(repo.id, RepoWorkKind::Sync).await,
        format_args!("repo {}: ending the sync streak", repo.id),
    );
    Ok(Ok(tip))
}

/// A repo's `kind` work that could not start because the repo could not be
/// synced: counted once for the repo, on its `sync` work item, which every
/// kind needs (WORK-MODEL.md §17). A clone or fetch failure while a host
/// the repo needs is unreachable is the network's: nothing is counted and
/// the cycle ends denied for the host (#95). Otherwise the error ends the
/// cycle as it always did.
async fn repo_sync_failed(
    store: &Store,
    (cfg, backend): (&Config, &dyn Backend),
    repo: &Repo,
    kind: RepoJobKind,
    failure: SyncFailure,
) -> anyhow::Result<CycleSummary> {
    if failure.network
        && let Some(host) = unreachable_now(cfg, &repo.url, backend).await
    {
        return Ok(offline_denial(store, kind.into(), None, None, host).await);
    }
    record_repo_failure(
        store,
        (repo.id, &repo.name),
        (RepoWorkKind::Sync, None),
        failure.key,
        &failure.detail,
        HoldReason::Stuck,
    )
    .await;
    Err(anyhow::anyhow!("{}", failure.detail))
}

/// Count one failure of `repo`'s `kind` work under `key`, `detail`
/// describing it, in the work item's streak, as [`record_harvest_failure`]
/// does for a finding's. The [`MAX_CONSECUTIVE_SAME_FAILURE`]th identical
/// failure holds the work for the operator as `at_limit`
/// ([`Store::hold_repo_work`]), with `job` as its checkpoint if a worker
/// ran in it. A failure does not move `last_<kind>_at`, so the work is
/// retried as soon as the rotation reaches it again (WORK-MODEL.md §17).
///
/// True when the work is now held. A count or a hold that could not be
/// recorded is reported as such and ends nothing.
async fn record_repo_failure(
    store: &Store,
    (repo_id, repo): (i64, &str),
    (kind, job): (RepoWorkKind, Option<i64>),
    key: &str,
    detail: &str,
    at_limit: HoldReason,
) -> bool {
    let log = |message: String| async move {
        let _ = store.log_event("error", &message, job, None).await;
    };
    let streak = match store.record_repo_failure(repo_id, kind, key).await {
        Ok(streak) => streak,
        Err(e) => {
            log(format!(
                "{kind} {repo}: {detail}, will retry (not counted: {e})"
            ))
            .await;
            return false;
        }
    };
    if streak < MAX_CONSECUTIVE_SAME_FAILURE {
        log(format!("{kind} {repo}: {detail}, will retry")).await;
        return false;
    }
    match store
        .hold_repo_work(repo_id, kind, at_limit, detail, job)
        .await
    {
        Ok(true) => {
            log(format!(
                "{kind} {repo}: held after {streak} identical failures ({detail})"
            ))
            .await;
            true
        }
        Ok(false) => {
            log(format!(
                "{kind} {repo}: {streak} identical failures ({detail}), but the repo is gone \
                 or already held; nothing held"
            ))
            .await;
            false
        }
        Err(e) => {
            log(format!(
                "{kind} {repo}: {streak} identical failures ({detail}), but holding it \
                 failed: {e}; will retry"
            ))
            .await;
            false
        }
    }
}

/// Budget gate: check with backend, return the grant or a denied summary.
enum BudgetDecision {
    /// The token bound to enforce — `None` for a job the ramp put no
    /// ceiling on, which then runs under `maxWallS` alone — and the
    /// anticipated cost the ramp reserved to grant it. The job row records
    /// the latter so the inflight reservation can read it back while the
    /// job runs.
    Approved {
        cap: Option<i64>,
        anticipated: i64,
    },
    Denied(Box<CycleSummary>),
}

async fn budget_gate(
    backend: &dyn Backend,
    store: &Store,
    cfg: &Config,
    repo_id: i64,
    kind: JobKind,
    use_override: bool,
    log_prefix: &str,
    finding_id: Option<i64>,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<BudgetDecision> {
    // A resumed attempt costs its context back plus whatever is left of
    // the per-kind typical, not the per-kind typical on its own: the
    // first call re-establishes a transcript that may be far larger than
    // a cold session's floor. Reserving `z` for it would under-reserve
    // by exactly the amount that makes resuming worth doing.
    //
    // Both reservations are floored so that loading context is at most
    // half of what an admitted attempt pays for (`MIN_START_EFFICIENCY`).
    // The floor only decides WHETHER the gate admits the job: the cap a
    // granted job runs under is the window's headroom computed without
    // this job's own reservation, so a larger reservation never shrinks
    // it. What it does is refuse a window too tight to fund the attempt
    // as mostly work, instead of starting one that spends its budget
    // loading context.
    let anticipated = match resume {
        Some(plan) => plan.anticipated,
        None => cold_reservation(
            anticipated_tokens(store, cfg, repo_id, kind)
                .await
                .unwrap_or(0),
        ),
    };
    let outlook = backend.decide(anticipated).await?;
    let verdict = if use_override {
        &outlook.prioritized
    } else {
        &outlook.normal
    };
    match verdict {
        Verdict::Denied { reason, retry_at } => {
            let _ = store
                .log_event("deny", &format!("{log_prefix}: {reason}"), None, finding_id)
                .await;
            Ok(BudgetDecision::Denied(Box::new(CycleSummary {
                kind: Some(kind),
                denied: Some(reason.clone()),
                retry_at: retry_at.as_ref().map(|v| *v as i64),
                ..Default::default()
            })))
        }
        Verdict::Granted {
            cap_tokens: backend_cap,
            ..
        } => Ok(BudgetDecision::Approved {
            // Verbatim: the ramp's headroom IS the budget for this job,
            // and it is the only token bound. A config constant beside it
            // could only ever be a second, static guess at the same
            // quantity — and the one that shipped had drifted below what
            // the jobs it governed cost, killing them on its own.
            cap: *backend_cap,
            anticipated,
        }),
    }
}

/// The chain workspace a job runs in: for a resume, the chain's own,
/// untouched; for a cold job, a new one — after first releasing the trees
/// of the chains its `create_job` superseded.
///
/// Called after the budget gate and after `create_job`, never before: a
/// job that is denied or refused has no workspace, so there is nothing to
/// clean up on those paths. The supersession release has to come first
/// because a superseded fix chain holds the same branch checked out, and
/// git refuses one branch in two worktrees.
///
/// Takes the clone's permit by value: whatever the runner did to the clone
/// before (a fetch) and the tree made here are one hold, and the runner
/// cannot keep the clone locked while its worker runs. The workspace comes
/// back leased ([`OpenWorkspace`]), from before its tree exists, so
/// nothing releases the tree until the runner closes it.
///
/// `Ok(Err(summary))`: the tree could not be made. The job is recorded
/// `failed` with the reason, and [`crate::workspace::create`] has already
/// removed whatever it made. `Ok(Ok(..))` hands the job back for its
/// runner to end.
async fn open_workspace(
    store: &Store,
    cfg: &Config,
    (permit, repo): (ClonePermit<'_>, &Repo),
    created: CreatedJob,
    resume: Option<&ResumePlan>,
    spec: TreeSpec,
    kind: JobKind,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> anyhow::Result<Result<(OpenWorkspace, String, RunningJob), CycleSummary>> {
    anyhow::ensure!(
        permit.repo_id() == repo.id,
        "{log_prefix}: a permit for repo {} opening a workspace of repo {}",
        permit.repo_id(),
        repo.id
    );
    let CreatedJob { job, superseded } = created;
    if let Some(plan) = resume {
        let chain = chain_of(store, plan.workspace.origin_id).await?;
        let ws = OpenWorkspace::lease(&permit, &cfg.work_root, &chain)?;
        return Ok(Ok((ws, plan.pinned_sha.clone(), job)));
    }
    let id = job.id();
    for &old in &superseded {
        let origin = chain_of(store, store.resume_origin_job(old).await?).await?;
        crate::workspace::release_if_idle(store, &permit, &cfg.work_root, &origin).await?;
    }
    let ws = OpenWorkspace::lease(&permit, &cfg.work_root, &chain_of(store, id).await?)?;
    let made = {
        let work_root = cfg.work_root.clone();
        permit
            .blocking(move |hold| crate::workspace::create(hold, &work_root, id, &spec))
            .await?
    };
    match made {
        Ok(sha) => {
            store.set_pinned_sha(id, &sha).await?;
            Ok(Ok((ws, sha, job)))
        }
        Err(why) => {
            let note = format!("{TREE_NOT_MADE}: {why}");
            job.fail_unstarted(&note).await?;
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: job {id} {note}"),
                    Some(id),
                    finding_id,
                )
                .await;
            Ok(Err(CycleSummary {
                kind: Some(kind),
                repo: Some(repo.name.clone()),
                finding_id,
                job_id: Some(id),
                state: Some(JobState::Failed),
                failure: Some(note),
                ..Default::default()
            }))
        }
    }
}

/// Chain `origin_id` as the store names it, for leasing or releasing its
/// tree. Every caller names a job it read a moment ago, so a missing row
/// is a fault.
async fn chain_of(store: &Store, origin_id: i64) -> anyhow::Result<crate::store::ChainRef> {
    store
        .chain_ref(origin_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("chain {origin_id}: no job has that id"))
}

/// Release a chain's tree once its job has ended, unless the job table
/// says the chain is still `running` or `suspended` — a suspension is a
/// pause, and its tree is the state the resume continues — or another
/// runner holds it (an engage whose harvest it handed the tree to).
///
/// Best effort: a tree that could not be released is picked up by the
/// sweep before the next cycle.
async fn close_workspace(lane: &mut Lane, ws: OpenWorkspace) {
    let tree = ws.tree.clone();
    if let Err(e) = ws.close(lane).await {
        tracing::warn!("releasing {}: {e}", tree.display());
    }
}

/// Log a failed store write the job deliberately continues past.
///
/// These writes sit mid-job, ahead of steps that must still run —
/// releasing a fix claim or a recheck's, releasing the tree, retiring the
/// job row — so aborting with `?` would trade one inconsistency for a
/// worse one, such as a finding left `fixing` until a settle. Continuing
/// is intended; doing so without a trace is not. Logged through `tracing`
/// rather than `log_event`: a store that just refused a write is the
/// least likely place for the record of that refusal to land.
///
/// Returns the value on success, so a write that also reports something
/// (an attempt streak, the recorded job state) keeps its caller's
/// fallback for the failure case.
fn log_write_failure<T, E: std::fmt::Display>(
    result: Result<T, E>,
    what: std::fmt::Arguments<'_>,
) -> Option<T> {
    result
        .inspect_err(
            |e| tracing::error!(error = %format_args!("{e:#}"), "store write failed: {what}"),
        )
        .ok()
}

/// Make `transition` on `fid` for a job or the PR sync, which continues
/// whatever the outcome; `true` when it was made. A failed write is logged
/// as [`log_write_failure`] logs one. A conflict, the finding no longer
/// being where the change starts, is logged as a `kind` event: it records
/// that something else decided the finding in between, and that decision
/// stands. `#[must_use]`, as [`Moved`]: whatever a caller records about
/// the change it records only when the change was made.
#[must_use]
async fn make_transition(
    store: &Store,
    fid: i64,
    job: Option<i64>,
    kind: &str,
    transition: &Transition<'_>,
) -> bool {
    match store.transition(fid, transition).await {
        Ok(Moved::Applied) => true,
        Ok(Moved::Conflict { now }) => {
            log_conflict(store, fid, job, kind, transition, now).await;
            false
        }
        Err(e) => {
            let (from, to) = (transition.from(), transition.to());
            log_write_failure::<(), _>(Err(e), format_args!("#{fid}: status {from} -> {to}"));
            false
        }
    }
}

/// Log as a `kind` event that `transition` was not made on `fid` because
/// the finding is at `now` instead: something else decided it in between,
/// and that decision stands.
async fn log_conflict(
    store: &Store,
    fid: i64,
    job: Option<i64>,
    kind: &str,
    transition: &Transition<'_>,
    now: Option<FindingStatus>,
) {
    log_not_moved(
        store,
        fid,
        job,
        kind,
        (transition.from(), transition.to()),
        now,
    )
    .await;
}

/// [`log_conflict`] for any `from -> to` edge, a fix claim's included.
async fn log_not_moved(
    store: &Store,
    fid: i64,
    job: Option<i64>,
    kind: &str,
    (from, to): (impl std::fmt::Display, impl std::fmt::Display),
    now: Option<FindingStatus>,
) {
    let now = now.map_or_else(|| "gone".to_owned(), |s| format!("'{s}'"));
    let _ = store
        .log_event(
            kind,
            &format!("#{fid} not moved {from} -> {to}: it is {now} now"),
            job,
            Some(fid),
        )
        .await;
}

/// End `pr` for the PR sync ([`Store::end_pr`]), recording it in
/// `pr_state` with the finding's move: a failed write is an `error` event
/// and a sync error, and leaves the finding `pr_open` with its `pr_state`
/// unchanged, so the next sync retries both. A conflict, or a finding that
/// has shipped another PR since sync listed it, is logged as a `kind`
/// event and left to whatever moved it; `true` only when the end was made.
async fn sync_pr_end(
    store: &Store,
    fid: i64,
    kind: &str,
    pr: &PrRef,
    ending: PrEnding<'_>,
    summary: &mut SyncResult,
) -> bool {
    match store.end_pr(fid, pr, ending).await {
        Ok(PrEnded::Applied) => true,
        Ok(PrEnded::Conflict { now }) => {
            log_not_moved(store, fid, None, kind, (ending.from(), ending.to()), now).await;
            false
        }
        Ok(PrEnded::OtherPr) => {
            let _ = store
                .log_event(
                    kind,
                    &format!(
                        "#{fid} PR #{} seen {}, not recorded: the finding has another PR now",
                        pr.number(),
                        ending.to()
                    ),
                    None,
                    Some(fid),
                )
                .await;
            false
        }
        Err(err) => {
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "sync #{fid}: PR {} but not recorded: {err} (the next sync retries)",
                        ending.to()
                    ),
                    None,
                    Some(fid),
                )
                .await;
            summary.errors += 1;
            false
        }
    }
}

/// The failure keys of a hunt's or scan's worker run (WORK-MODEL.md §17).
/// A worker that did not finish is counted as `worker <state>`.
const NO_FINDINGS_FILE: &str = "no findings file";
const INVALID_ENTRIES: &str = "invalid entries";

/// What a hunt's or scan's attempt amounts to for its work item: a pause,
/// the work's outcome, or a failure with its key and detail. The outcome is
/// a worker that finished and wrote its output file with no invalid entry.
/// An empty outcome is a failure: no file, or a file with invalid entries,
/// whose valid entries are still filed.
enum RepoAttempt {
    Paused,
    Succeeded,
    Failed { key: String, detail: String },
}

fn repo_attempt(
    state: JobState,
    ingest: Option<&IngestResult>,
    out_path: &Path,
    rr: &RunResult,
) -> RepoAttempt {
    match (state, ingest) {
        (JobState::Suspended, _) => RepoAttempt::Paused,
        (JobState::Done, Some(counts)) if counts.invalid == 0 => RepoAttempt::Succeeded,
        (JobState::Done, Some(counts)) => RepoAttempt::Failed {
            key: INVALID_ENTRIES.to_owned(),
            detail: format!("{}: {} invalid entries", out_path.display(), counts.invalid),
        },
        (JobState::Done, None) => RepoAttempt::Failed {
            key: NO_FINDINGS_FILE.to_owned(),
            detail: format!("the worker finished without writing {}", out_path.display()),
        },
        (state, _) => RepoAttempt::Failed {
            key: format!("worker {state}"),
            detail: format!(
                "worker {state}\n\n{}",
                crate::util::tail(&rr.stdout_tail, 500)
            ),
        },
    }
}

/// Count a hunt's or scan's failed attempt `job` on its work item, and say
/// so in the cycle's summary: held at the limit, else retried.
async fn count_repo_attempt(
    store: &Store,
    repo: &Repo,
    (kind, job): (RepoJobKind, i64),
    (key, detail): (&str, &str),
    summary: &mut CycleSummary,
) {
    let held = record_repo_failure(
        store,
        (repo.id, &repo.name),
        (kind.into(), Some(job)),
        key,
        detail,
        HoldReason::Stuck,
    )
    .await;
    summary.failure = Some(detail.to_owned());
    summary.outcome = Some(if held { "held" } else { "retry" }.into());
}

/// Count a hunt or scan whose tree could not be made ([`open_workspace`]
/// has failed its job and logged why) as a failure of its work under the
/// fixed [`TREE_NOT_MADE`], git's reason as the detail, as for every
/// finding kind (WORK-MODEL.md §7, G9).
async fn count_unmade_repo_tree(
    store: &Store,
    repo: &Repo,
    (kind, job): (RepoJobKind, i64),
    mut summary: CycleSummary,
) -> CycleSummary {
    let note = summary.failure.take().unwrap_or_default();
    count_repo_attempt(
        store,
        repo,
        (kind, job),
        (TREE_NOT_MADE, &note),
        &mut summary,
    )
    .await;
    summary
}

/// `repo`'s `kind` work succeeded: its streak ends.
async fn repo_succeeded(store: &Store, repo: &Repo, kind: RepoJobKind) {
    log_write_failure(
        store.repo_work_succeeded(repo.id, kind.into()).await,
        format_args!("repo {}: ending the {kind} streak", repo.id),
    );
}

/// The findings a cold repo-level prompt hands its worker: those of each
/// of `types` already rejected or declined, each with its verdict's
/// anchor, and those still on file.
///
/// Any read failing is an error, never an empty list: a worker told
/// that nothing is settled spends a metered run rediscovering findings an
/// operator already turned down. Callers load these before the budget
/// gate so a failed read refuses the job before anything is reserved,
/// written or spawned.
async fn prior_findings(
    store: &Store,
    rid: i64,
    types: &[&str],
    label: &str,
) -> anyhow::Result<(Vec<crate::suppression::Suppressed>, Vec<Finding>)> {
    let mut suppressed = Vec::new();
    let mut known = Vec::new();
    for &ft in types {
        suppressed.extend(
            crate::suppression::suppressed(store, rid, ft)
                .await
                .with_context(|| format!("{label}: loading suppressed {ft} findings"))?,
        );
        known.extend(
            store
                .known_active(rid, ft)
                .await
                .with_context(|| format!("{label}: loading known {ft} findings"))?,
        );
    }
    Ok((suppressed, known))
}

/// Run a hunt job (`scheduler.run_hunt`).
///
/// `resume` continues a suspended attempt: no sync, the diff range ending
/// at the chain's pinned commit, the same ingest and the same watermark
/// rule, but the worker is handed its own transcript and a one-line
/// "carry on" instead of a fresh playbook.
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: sync, resolve diff range, gate budget, \
              run the worker, ingest. The steps share a dozen locals and \
              each early-returns a `CycleSummary`, so helper extraction \
              would only move the same state behind argument lists"
)]
pub async fn run_hunt(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    // Git's empty tree — the implicit parent of all root commits.
    // Using this as diff base includes the root commit itself.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let store = scope.store();
    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    // The clone is this hunt's from its fetch to its tree being made
    // (`open_workspace` takes the permit).
    let permit = lane.lock_clone(repo.id).await?;
    // The full re-hunt request this hunt answers, if one was pending when
    // the repo row was read; a request landing after that is another hunt's.
    let request = repo.full_hunt_requested_at;

    // The commit this chain reviews up to. Cold: the freshly fetched tip
    // of the default branch, which the chain's tree is created at below.
    // Resume: the commit the chain's tree was created at, however far the
    // default branch has moved since — nothing is fetched or checked out
    // for a resume. Both the diff range and the watermark written on Done
    // come from this, so the watermark always names the commit the worker
    // actually reviewed and never marks as hunted a commit that was not in
    // the range.
    let head = if let Some(plan) = resume {
        plan.pinned_sha.clone()
    } else {
        match sync_to_tip(store, &permit, repo).await? {
            Ok(tip) => tip,
            Err(failure) => {
                // Let go before `repo_sync_failed` probes the network: a
                // URL update, the sweep or a delete need not wait it out.
                drop(permit);
                // The attempt is over before it had a job. It still answers
                // the request's place in the queue, or a fetch that keeps
                // failing would put this hunt first in every cycle.
                if let Some(r) = request {
                    log_write_failure(
                        store.mark_full_hunt_attempted(rid, r).await,
                        format_args!("repo {rid}: marking full-hunt request {r} attempted"),
                    );
                }
                return repo_sync_failed(store, (cfg, backend), repo, RepoJobKind::Hunt, failure)
                    .await;
            }
        }
    };
    let rp_str = rpath.to_string_lossy().to_string();

    let last = repo
        .last_hunt_sha
        .as_deref()
        .map(std::borrow::ToOwned::to_owned);
    let last_full = repo.last_full_hunt_at;
    let requested = request.is_some();
    let rehunt_interval_ms = cfg.hunt_rehunt_days * 86_400_000;
    // Both of the decisions below answer "should this work START", and
    // on a resume that question was settled in an earlier cycle. Firing
    // a full re-hunt here — periodic or requested — would re-scope work
    // already in flight, and the no-new-commits skip would abandon a live
    // session.
    //
    // Due once the interval has passed, `>=` like the rotation's own
    // intervals (`rotation_kind`). The two differ only in the one
    // millisecond where the elapsed time equals the interval, which no
    // test can hit, so a strict `>` here is a surviving mutant rather
    // than a decision.
    let rehunt_due = resume.is_none()
        && (requested || last_full.is_some_and(|lf| (now_ms() - lf) >= rehunt_interval_ms));
    // The scope is this hunt's alone: the watermark stays in the repo row
    // until a hunt succeeds and moves it. Clearing it here, as this used
    // to, lost it to every full hunt the budget denied or that failed.
    let last = if rehunt_due {
        let why = if requested {
            "requested".to_owned()
        } else {
            format!("{}d interval", cfg.hunt_rehunt_days)
        };
        let _ = store
            .log_event(
                "hunt",
                &format!("{rname}: full re-hunt triggered ({why})"),
                None,
                None,
            )
            .await;
        None
    } else {
        last
    };
    // A resume keeps the scope its chain started with, which the chain's
    // jobs carry (`full_history`): the repo row that decided it may have
    // changed since, and its watermark is never cleared for a full chain,
    // so a full chain's resume reads past it. For a chain from before that
    // column: starting a full re-hunt used to clear the watermark of a repo
    // that had had a full hunt, so a resume finding none there is
    // continuing one.
    let resumed_full = match resume {
        Some(plan) => {
            store.job_full_history(plan.predecessor_id).await?
                || (last.is_none() && last_full.is_some())
        }
        None => false,
    };
    let last = if resumed_full { None } else { last };
    if resume.is_none() && last.as_deref() == Some(&head) {
        // The watermark is what makes the next pick move on; a skip that
        // failed to record it would be re-picked at once (a skipped cycle
        // restarts immediately), so the failure goes to the error path.
        store.set_last_hunt(rid, &head).await?;
        // Nothing to review is the hunt's outcome too.
        repo_succeeded(store, repo, RepoJobKind::Hunt).await;
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: no new commits since {} -- skipped",
                    &head[..12.min(head.len())]
                ),
                None,
                None,
            )
            .await;
        return Ok(CycleSummary {
            kind: Some(RepoJobKind::Hunt.into()),
            skipped: Some("no new commits".into()),
            head: Some(head),
            ..Default::default()
        });
    }
    let (diff_range, scope_note) = if let Some(ref l) = last {
        // Incremental: only commits since last hunt
        (
            format!("{l}..{head}"),
            format!(
                "Commits since the last completed hunt ({}).",
                &l[..12.min(l.len())]
            ),
        )
    } else if rehunt_due || resumed_full {
        // Full re-hunt: complete history including root commit
        let why = if requested { "Requested" } else { "Periodic" };
        (
            format!("{EMPTY_TREE}..{head}"),
            format!("{why} full re-hunt: complete history including root commit."),
        )
    } else {
        // First hunt: last 3 weeks or 30 commits (bounded)
        let rps = rp_str.clone();
        let h = head.clone();
        let (_rc, base) = tokio::task::spawn_blocking(move || {
            Unheld::LastBefore {
                rev: &h,
                before: "3 weeks ago",
            }
            .run(Path::new(&rps), 30)
        })
        .await
        .unwrap_or((127, String::new()));
        let base = base.trim().to_owned();
        if base.is_empty() || base == head {
            let rps = rp_str.clone();
            let h = head.clone();
            let base2 = tokio::task::spawn_blocking(move || {
                crate::workspace::resolve(Path::new(&rps), &format!("{h}~30"))
            })
            .await
            .ok()
            .flatten();
            match base2 {
                // Very small repo — scan from root
                None => (
                    format!("{EMPTY_TREE}..{head}"),
                    "First hunt for this repo: the full history (small repo).".to_owned(),
                ),
                Some(base2) => (
                    format!("{base2}..{head}"),
                    format!(
                        "First hunt for this repo: the last 30 commits (base {}).",
                        &base2[..12.min(base2.len())]
                    ),
                ),
            }
        } else {
            (
                format!("{base}..{head}"),
                format!(
                    "First hunt for this repo: the last ~3 weeks (base {}).",
                    &base[..12.min(base.len())]
                ),
            )
        }
    };
    // Whether this chain reviews the complete history — a full re-hunt, or
    // the first hunt of a repo small enough to take whole. Such a chain
    // records a full hunt when it finishes and answers a full re-hunt
    // request; failing to record it would start the same full pass over
    // again on the next cold hunt.
    let full_scope = diff_range.starts_with(EMPTY_TREE);

    // A resume hands its worker a "carry on", not a playbook, so it reads
    // neither list.
    let (suppressed, known) = if resume.is_some() {
        (Vec::new(), Vec::new())
    } else {
        let types = [FindingType::Bug.as_str()];
        prior_findings(store, rid, &types, &format!("hunt {rname}")).await?
    };

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        rid,
        RepoJobKind::Hunt.into(),
        false,
        &format!("hunt {rname}"),
        None,
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let created = match scope
        .create_job(
            resume.map_or(
                JobStart::fresh_repo(RepoJobKind::Hunt, rid),
                ResumePlan::start,
            ),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(RepoJobKind::Hunt.into(), rname, e),
    };
    let job = created.job.id();
    if resume.is_none() {
        if full_scope {
            // Before the worker runs, so a suspension carries it to the
            // resume. Best-effort like the job's other bookkeeping: a
            // resume that cannot read it falls back to the watermark
            // heuristic, and the request stays pending for another pass.
            log_write_failure(
                store.mark_full_history(job, request).await,
                format_args!("job {job}: marking full-history scope"),
            );
        }
        if let Some(r) = request {
            log_write_failure(
                store.mark_full_hunt_attempted(rid, r).await,
                format_args!("repo {rid}: marking full-hunt request {r} attempted"),
            );
        }
    }
    let (ws, pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, repo),
        created,
        resume,
        TreeSpec::Detached { at: head.clone() },
        RepoJobKind::Hunt.into(),
        &format!("hunt {rname}"),
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => {
            return Ok(count_unmade_repo_tree(store, repo, (RepoJobKind::Hunt, job), failed).await);
        }
    };
    // A resumed worker is still writing to the path its ORIGINAL prompt
    // named, so the attempt that ingests has to look there. Using this
    // job's own id would find nothing, advance no watermark, and put the
    // same work back in the rotation next cycle.
    let out_job = resume.map_or(job, |r| r.origin_job_id);
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{out_job}.findings.json"));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));

    let suppressions = crate::suppression::suppression_list(repo, suppressed, &pinned).await;
    let repo_notes = Store::repo_notes(&cfg.work_root, rid);
    let prompt = match resume {
        Some(plan) => resume_prompt(RepoJobKind::Hunt.into(), plan),
        None => playbooks::build_hunt_prompt(
            &cfg.root,
            repo,
            &ws.tree,
            &diff_range,
            &scope_note,
            &suppressions,
            &known,
            &out_path,
            cfg.hunt_max_findings,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = match backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            let ran = (WorkOf::Repo(RepoJobKind::Hunt, repo), running);
            let denied = runner_failed(store, (cfg, backend, &repo.url), ran, &e).await;
            close_workspace(lane, ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Repo(RepoJobKind::Hunt, repo), running);
    let running = match unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            close_workspace(lane, ws).await;
            return Ok(denied);
        }
    };
    let state = log_write_failure(
        record_job(store, running, &rr, model, resume)
            .await
            .map(|r| r.state()),
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);

    let mut summary = CycleSummary {
        kind: Some(RepoJobKind::Hunt.into()),
        repo: Some(rname.to_owned()),
        job_id: Some(job),
        state: Some(state),
        diff_range: Some(diff_range.clone()),
        tokens_new: Some(rr.tokens_new),
        head: Some(head.clone()),
        full_rehunt: Some(full_scope),
        ..Default::default()
    };

    let counts = if out_path.exists() {
        let counts = ingest_scan_findings(
            store,
            rid,
            &out_path,
            EntryTypes::Fixed(FindingType::Bug),
            job,
        )
        .await;
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: job {job} {state} over {}... +{} new / {} dup ({} reopened) / {} invalid ({} tok)",
                    &diff_range[..25.min(diff_range.len())],
                    counts.inserted,
                    counts.duplicates,
                    counts.reopened,
                    counts.invalid,
                    rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        Some(counts)
    } else {
        let _ = store
            .log_event(
                "hunt",
                &format!(
                    "{rname}: job {job} {state}, no findings file ({} tok)",
                    rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        None
    };
    match repo_attempt(state, counts.as_ref(), &out_path, &rr) {
        RepoAttempt::Succeeded => {
            // The chain's pinned commit, never the clone's current tip:
            // see where `head` is resolved.
            log_write_failure(
                store.set_last_hunt(rid, &pinned).await,
                format_args!("repo {rid}: last-hunt sha -> {pinned}"),
            );
            if full_scope || last_full.is_none() {
                log_write_failure(
                    store.set_last_full_hunt(rid).await,
                    format_args!("repo {rid}: recording last full hunt"),
                );
            }
            if full_scope {
                log_write_failure(
                    store.settle_full_hunt_request(rid, job).await,
                    format_args!("repo {rid}: settling full-hunt request for job {job}"),
                );
            }
            repo_succeeded(store, repo, RepoJobKind::Hunt).await;
        }
        RepoAttempt::Failed { key, detail } => {
            count_repo_attempt(
                store,
                repo,
                (RepoJobKind::Hunt, job),
                (&key, &detail),
                &mut summary,
            )
            .await;
        }
        RepoAttempt::Paused => {}
    }
    summary.ingest = counts;
    crate::suppression::apply_reconfirmations(
        store,
        repo,
        &[FindingType::Bug.as_str()],
        &out_path,
        &pinned,
    )
    .await;
    close_workspace(lane, ws).await;
    Ok(summary)
}

/// Handle a recheck that failed to produce a valid verdict: `failure` is
/// the streak key that identical failures share, `detail` describes this
/// one in the events and the hold (for a given-up chain, also why it was
/// given up). The [`MAX_CONSECUTIVE_SAME_FAILURE`]th identical failure holds
/// the recheck for the operator as `at_limit` ([`Store::hold_work`]) —
/// `stuck`, or `gave_up` when the give-up ceiling's failure completed the
/// streak — with `job` as its checkpoint if a worker ran in it. The finding
/// stays `rechecking`: sent back to `new` it looked exactly like a finding
/// nobody had rechecked.
async fn handle_recheck_failure(
    store: &Store,
    summary: &mut CycleSummary,
    fid: i64,
    job: i64,
    failure: &str,
    detail: &str,
    at_limit: HoldReason,
    override_mode: Option<BudgetOverride>,
) -> CycleSummary {
    let log = |message: String| async move {
        let _ = store
            .log_event("recheck", &message, Some(job), Some(fid))
            .await;
    };
    summary.outcome = Some("requeued".into());
    match store
        .record_work_failure(fid, FindingJobKind::Recheck, failure)
        .await
    {
        Err(e) => {
            log(format!(
                "#{fid}: job {job} {detail} -- will retry (not counted: {e})"
            ))
            .await;
        }
        Ok(streak) if streak < MAX_CONSECUTIVE_SAME_FAILURE => {
            log(format!("#{fid}: job {job} {detail} -- will retry")).await;
        }
        Ok(streak) => match store
            .hold_work(fid, FindingJobKind::Recheck, at_limit, detail, Some(job))
            .await
        {
            Ok(true) => {
                log(format!(
                    "#{fid} held after {streak} identical failures ({detail})"
                ))
                .await;
                summary.outcome = Some("held".into());
                summary.attempts = Some(streak);
            }
            Ok(false) => {
                log(format!(
                    "#{fid} {streak} identical failures ({detail}), but it is no longer \
                     rechecking; nothing held"
                ))
                .await;
            }
            Err(e) => {
                log(format!(
                    "#{fid} {streak} identical failures ({detail}), but holding it failed: {e}; \
                     will retry"
                ))
                .await;
            }
        },
    }
    if override_mode == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
    summary.clone()
}

/// Run a recheck job (`scheduler.run_recheck`).
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline whose verdict dispatch is a single match \
              over the worker's reply; every arm needs the same surrounding \
              locals (fid, job, summary, override_mode)"
)]
pub async fn run_recheck(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    #[derive(Debug, Clone)]
    enum RecheckOutcome {
        Confirmed,
        Stale,
        Invalid,
        Unknown(String),
    }

    impl<'de> serde::Deserialize<'de> for RecheckOutcome {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let s = String::deserialize(d)?;
            Ok(match s.as_str() {
                "confirmed" => Self::Confirmed,
                "stale" => Self::Stale,
                "invalid" => Self::Invalid,
                _ => Self::Unknown(s),
            })
        }
    }

    #[derive(Debug, Default, Deserialize)]
    struct RecheckVerdict {
        #[serde(default)]
        verdict: Option<RecheckOutcome>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        updated_summary: Option<String>,
        #[serde(default)]
        updated_detail: Option<String>,
        #[serde(default)]
        updated_confidence: Option<f64>,
        #[serde(default)]
        updated_severity: Option<String>,
        /// For `invalid`: the condition in the current code that makes the
        /// finding wrong, and the paths it rests on ([`crate::suppression`]).
        #[serde(default)]
        holds_while: Option<String>,
        #[serde(default, deserialize_with = "crate::suppression::de_paths")]
        depends_on: Vec<String>,
    }

    let store = scope.store();
    let fid = finding.id;
    if finding.status != FindingStatus::Rechecking {
        return Ok(CycleSummary {
            kind: Some(FindingJobKind::Recheck.into()),
            skipped: Some(format!(
                "finding #{fid} is {}, not rechecking",
                finding.status
            )),
            ..Default::default()
        });
    }
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("recheck #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let permit = lane.lock_clone(repo.id).await?;
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none()
        && let Err(failure) = sync_repo(store, &permit).await?
    {
        let _ = store
            .log_event(
                "error",
                &format!("recheck #{fid}: {}", failure.detail),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("{}", failure.detail);
    }

    let override_mode = finding.budget_override;
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Recheck.into(),
        override_mode.is_some(),
        &format!("recheck #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    remove_stale_report(resume)?;
    let created = match scope
        .create_job(
            resume.map_or(
                JobStart::fresh_finding(FindingJobKind::Recheck, fid),
                ResumePlan::start,
            ),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(FindingJobKind::Recheck.into(), &repo.name, e);
        }
    };
    let job = created.job.id();
    let (ws, pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, &repo),
        created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Recheck.into(),
        &format!("recheck #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(mut failed) => {
            // A failed recheck under the fixed [`TREE_NOT_MADE`], as a fix's
            // is ([`count_unmade_tree`]). Uncounted, the finding stays
            // rechecking and the recheck tier takes it again every cycle.
            // No worker ran, so a one-shot override is not spent.
            let note = failed.failure.clone().unwrap_or_default();
            return Ok(handle_recheck_failure(
                store,
                &mut failed,
                fid,
                job,
                TREE_NOT_MADE,
                &note,
                HoldReason::Stuck,
                None,
            )
            .await);
        }
    };
    let out_path = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));
    // Only on a cold run: the verdict file is finding-keyed, so on a
    // resume this path is the one the continuing worker was told to
    // write, and deleting it would discard a verdict it may already
    // have produced before the suspension.
    if resume.is_none() {
        // Remove stale output from a previous crashed attempt — a
        // leftover verdict file would be read as this attempt's result.
        let _ = std::fs::remove_file(&out_path);
    }

    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match resume {
        Some(plan) => resume_prompt(FindingJobKind::Recheck.into(), plan),
        None => playbooks::build_recheck_prompt(
            &cfg.root,
            finding,
            &repo,
            &ws.tree,
            &out_path,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = match backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            let ran = (WorkOf::Finding(FindingJobKind::Recheck, fid), running);
            let denied = runner_failed(store, (cfg, backend, &repo.url), ran, &e).await;
            close_workspace(lane, ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Finding(FindingJobKind::Recheck, fid), running);
    let running = match unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            close_workspace(lane, ws).await;
            return Ok(denied);
        }
    };
    let state = log_write_failure(
        record_job(store, running, &rr, model, resume)
            .await
            .map(|r| r.state()),
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Recheck.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };
    // A worker that could not verify the finding without something only a
    // human can provide wrote BLOCKED.md instead of a verdict: hold the
    // recheck, with this attempt as its checkpoint. A final verdict next
    // to it wins, as a decline wins over a fix's report.
    let final_verdict = state == JobState::Done
        && std::fs::read_to_string(&out_path)
            .ok()
            .and_then(|t| serde_json::from_str::<RecheckVerdict>(&t).ok())
            .and_then(|v| v.verdict)
            .is_some_and(|v| !matches!(v, RecheckOutcome::Unknown(_)));
    if !final_verdict && let Some(report) = blocked_report(&ws.tree)? {
        let held = hold_on_report(store, FindingJobKind::Recheck, fid, job, &ws, &report).await;
        close_workspace(lane, ws).await;
        if !held? {
            anyhow::bail!("recheck #{fid}: BLOCKED.md, but it is no longer rechecking");
        }
        summary.outcome = Some("held".into());
        return Ok(summary);
    }
    if state == JobState::Suspended {
        // A suspension is a pause, not a failure: the worker stopped
        // mid-work (out of window headroom, or died after doing work) and
        // its tree and transcript are kept for the resume. Counting it toward the streak would turn three
        // pauses of one healthy recheck into "stuck" and reset the finding.
        // It stays rechecking, where the recheck tier continues it.
        let _ = store
            .log_event(
                "recheck",
                &format!(
                    "#{fid} suspended; worktree kept at {} for the resume",
                    ws.tree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("suspended".into());
        summary.worktree = Some(ws.tree.to_string_lossy().into_owned());
        if override_mode == Some(BudgetOverride::Once) {
            log_write_failure(
                store.set_budget_override(fid, None).await,
                format_args!("#{fid}: clearing one-shot budget override"),
            );
        }
        close_workspace(lane, ws).await;
        return Ok(summary);
    }

    // Parse verdict file — only read if the worker completed successfully.
    // A stale file from a crashed previous attempt was already deleted above;
    // this gate prevents reading partial output from a killed/failed worker.
    let verdict: Option<RecheckVerdict> = if state == JobState::Done && out_path.exists() {
        std::fs::read_to_string(&out_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    } else {
        None
    };
    // Four failure cases preserved through the type structure:
    // 1. worker didn't complete  2. no verdict file  3. unparseable JSON  → verdict is None
    // 4. unrecognised verdict value → verdict.verdict is Some(Unknown(s))
    // 5. missing verdict field → verdict.verdict is None
    let Some(ref verdict_obj) = verdict else {
        let failure = if state != JobState::Done {
            format!("worker {state}")
        } else if !out_path.exists() {
            "no verdict file".to_owned()
        } else {
            "unparseable verdict file".to_owned()
        };
        let summary = handle_recheck_failure(
            store,
            &mut summary,
            fid,
            job,
            &failure,
            &failure,
            HoldReason::Stuck,
            override_mode,
        )
        .await;
        // Released only once the failure is counted: a hold makes this
        // attempt the checkpoint, and its tree is what `continue` resumes.
        close_workspace(lane, ws).await;
        return Ok(summary);
    };
    let Some(ref outcome) = verdict_obj.verdict else {
        let summary = handle_recheck_failure(
            store,
            &mut summary,
            fid,
            job,
            "missing verdict field",
            "missing verdict field",
            HoldReason::Stuck,
            override_mode,
        )
        .await;
        close_workspace(lane, ws).await;
        return Ok(summary);
    };

    let reason_owned = verdict_obj.reason.as_deref().unwrap_or_default();
    let reason: String = reason_owned.chars().take(500).collect();
    let reason = reason.as_str();

    // Declared out here: the transition borrows what each arm builds.
    let analysis;
    let verdict_reason;
    let anchor;
    let (transition, verdict_str) = match outcome {
        RecheckOutcome::Confirmed => {
            // The worker's spelling is not stored: the column is decoded as
            // a `Severity` by every whole-list read of the repo, so one
            // undecodable row would fail all of them. A value outside the
            // enum is dropped and the finding keeps its severity.
            let raw_severity = verdict_obj.updated_severity.as_deref();
            let severity = raw_severity.and_then(Severity::parse);
            if let (Some(raw), None) = (raw_severity, severity) {
                let _ = store
                    .log_event(
                        "recheck",
                        &format!("#{fid} ignored unknown updated_severity {raw:?}"),
                        Some(job),
                        Some(fid),
                    )
                    .await;
            }
            analysis = crate::store::FindingAnalysisUpdate {
                summary: verdict_obj.updated_summary.clone(),
                detail: verdict_obj.updated_detail.clone(),
                confidence: verdict_obj.updated_confidence,
                severity,
            };
            (
                Transition::RecheckConfirmed {
                    analysis: &analysis,
                },
                "confirmed",
            )
        }
        RecheckOutcome::Stale => {
            // Superseded, not suppressed: the code moved on and took the
            // bug with it, which says nothing against the finding. As
            // `wontfix` it told every later scan not to report the bug
            // again, should the rewritten code bring it back.
            verdict_reason = format!("recheck: {reason}");
            (
                Transition::RecheckStale {
                    reason: &verdict_reason,
                },
                "stale",
            )
        }
        RecheckOutcome::Invalid => {
            anchor = crate::suppression::verdict_anchor(
                FindingStatus::Rejected,
                &pinned,
                finding,
                verdict_obj.holds_while.as_deref(),
                &verdict_obj.depends_on,
            );
            verdict_reason = format!("recheck: {reason}");
            (
                Transition::RecheckInvalid {
                    reason: &verdict_reason,
                    anchor: anchor.as_ref(),
                },
                "invalid",
            )
        }
        // Unknown already handled above — included for exhaustiveness
        RecheckOutcome::Unknown(raw) => {
            let failure = format!("invalid verdict value: {raw:?}");
            let summary = handle_recheck_failure(
                store,
                &mut summary,
                fid,
                job,
                &failure,
                &failure,
                HoldReason::Stuck,
                override_mode,
            )
            .await;
            close_workspace(lane, ws).await;
            return Ok(summary);
        }
    };
    // Recorded only once it is made: a conflict means something else
    // decided the finding meanwhile (`make_transition` logs which), and
    // this verdict, analysis included, is dropped.
    if make_transition(store, fid, Some(job), "recheck", &transition).await {
        let _ = store
            .log_event(
                "recheck",
                &format!("#{fid} {verdict_str}: {reason}"),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some(verdict_str.into());
        summary.verdict = Some(verdict_str.to_owned());
        summary.reason = Some(reason.to_owned());
    }
    // A verdict ends the recheck: its tree is no checkpoint.
    close_workspace(lane, ws).await;
    if override_mode == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
    Ok(summary)
}

// -- analysis jobs (`scheduler._AnalysisSpec`, `run_test_gap` … `run_modernize`) ------------------------------------

/// Static per-kind wiring for analysis jobs.
type AnalysisPromptBuilder = fn(
    &Path,
    &Repo,
    &Path,
    &str,
    &[playbooks::Suppression],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

struct AnalysisSpec {
    kind: RepoJobKind,
    finding_type: FindingType,
    /// Other types the worker may file instead, when what it found turns
    /// out to be one of those. Their open findings and suppressions join
    /// the prompt's, so the worker does not re-file them either.
    also_files: &'static [FindingType],
    out_plural: &'static str,
    no_output_noun: &'static str,
    scope_note: &'static str,
    prompt_builder: AnalysisPromptBuilder,
}

impl AnalysisSpec {
    fn entry_types(&self) -> EntryTypes {
        if self.also_files.is_empty() {
            EntryTypes::Fixed(self.finding_type)
        } else {
            EntryTypes::Mainly(self.finding_type, self.also_files)
        }
    }
}

// A gap whose code already misbehaves is a bug, not a missing test: filed
// as a test_gap it is ranked and fixed as one (tests only), and the fix
// never happens. Of the 369 open test_gaps on 2026-10-08, 210 also
// described a present defect.
const TEST_GAP_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::TestGap,
    finding_type: FindingType::TestGap,
    also_files: &[FindingType::Bug],
    out_plural: "test_gaps",
    no_output_noun: "gaps",
    scope_note: "Full repository scan for test coverage gaps.",
    prompt_builder: playbooks::build_test_gap_prompt,
};
const DEP_UPDATE_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::DepUpdate,
    finding_type: FindingType::DepUpdate,
    also_files: &[],
    out_plural: "dep_updates",
    no_output_noun: "updates",
    scope_note: "Check all package manifests for outdated dependencies.",
    prompt_builder: playbooks::build_dep_update_prompt,
};
const REFACTOR_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Refactor,
    finding_type: FindingType::Refactor,
    also_files: &[],
    out_plural: "refactorings",
    no_output_noun: "refactorings",
    scope_note: "Scan for safe, mechanical refactoring opportunities (duplication, dead code, complexity).",
    prompt_builder: playbooks::build_refactor_prompt,
};
const MODERNIZATION_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Modernization,
    finding_type: FindingType::Modernization,
    also_files: &[],
    out_plural: "modernizations",
    no_output_noun: "modernizations",
    scope_note: "Scan for SOTA-drift modernization opportunities (deprecated/unmaintained deps, language-feature gaps, format/protocol shifts, major version debt, platform EOL).",
    prompt_builder: playbooks::build_modernization_prompt,
};
const STANDARDS_SPEC: AnalysisSpec = AnalysisSpec {
    kind: RepoJobKind::Standards,
    finding_type: FindingType::Standards,
    also_files: &[],
    out_plural: "standards",
    no_output_noun: "standards",
    scope_note: "Full repository audit against coding standards.",
    prompt_builder: playbooks::build_standards_prompt,
};

/// Shared body for the four repo-level analysis job types (`scheduler._run_analysis_job`).
#[allow(
    clippy::too_many_lines,
    reason = "one body shared by four job kinds; the per-kind differences \
              are already factored into `AnalysisJobSpec`, so what remains \
              is a single non-branching sequence"
)]
async fn run_analysis_job(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    spec: &AnalysisSpec,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    let kind = spec.kind;

    if !rpath.exists() {
        let failure = SyncFailure {
            key: REPO_NOT_CLONED,
            detail: format!("{} is not cloned", rpath.display()),
            network: false,
        };
        return repo_sync_failed(store, (cfg, backend), repo, kind, failure).await;
    }
    let permit = lane.lock_clone(repo.id).await?;
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none()
        && let Err(failure) = sync_to_tip(store, &permit, repo).await?
    {
        // Let go before the probe (see `run_hunt`).
        drop(permit);
        return repo_sync_failed(store, (cfg, backend), repo, kind, failure).await;
    }

    let types: Vec<&str> = std::iter::once(&spec.finding_type)
        .chain(spec.also_files)
        .map(|ft| ft.as_str())
        .collect();
    // As in `run_hunt`: read before the budget gate, and only for a cold job.
    let (suppressed, known) = if resume.is_some() {
        (Vec::new(), Vec::new())
    } else {
        prior_findings(store, rid, &types, &format!("{kind} {rname}")).await?
    };

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        rid,
        JobKind::from(kind),
        false,
        &format!("{kind} {rname}"),
        None,
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };
    let created = match scope
        .create_job(
            resume.map_or(JobStart::fresh_repo(kind, rid), ResumePlan::start),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(kind.into(), rname, e),
    };
    let job = created.job.id();
    let (ws, pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, repo),
        created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        kind.into(),
        &format!("{kind} {rname}"),
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(count_unmade_repo_tree(store, repo, (kind, job), failed).await),
    };
    // The continuing worker still writes the path its original prompt
    // named; see `run_hunt`.
    let out_job = resume.map_or(job, |r| r.origin_job_id);
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("job{out_job}.{}.json", spec.out_plural));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(Path::new(".")));

    let suppressions = crate::suppression::suppression_list(repo, suppressed, &pinned).await;
    let repo_notes = Store::repo_notes(&cfg.work_root, rid);
    let prompt = match resume {
        Some(plan) => resume_prompt(kind.into(), plan),
        None => (spec.prompt_builder)(
            &cfg.root,
            repo,
            &ws.tree,
            spec.scope_note,
            &suppressions,
            &known,
            &out_path,
            cfg.hunt_max_findings,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("hunt");
    let rr = match backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            let ran = (WorkOf::Repo(kind, repo), running);
            let denied = runner_failed(store, (cfg, backend, &repo.url), ran, &e).await;
            close_workspace(lane, ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Repo(kind, repo), running);
    let running = match unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            close_workspace(lane, ws).await;
            return Ok(denied);
        }
    };
    let state = log_write_failure(
        record_job(store, running, &rr, model, resume)
            .await
            .map(|r| r.state()),
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(kind.into()),
        repo: Some(rname.to_owned()),
        job_id: Some(job),
        state: Some(state),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };

    let counts = if out_path.exists() {
        let counts = ingest_scan_findings(store, rid, &out_path, spec.entry_types(), job).await;
        let _ = store
            .log_event(
                kind.as_str(),
                &format!(
                    "{rname}: job {job} {state} -- +{} new / {} dup ({} reopened) / {} invalid ({} tok)",
                    counts.inserted,
                    counts.duplicates,
                    counts.reopened,
                    counts.invalid,
                    rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        Some(counts)
    } else {
        let _ = store
            .log_event(
                kind.as_str(),
                &format!(
                    "{rname}: job {job} {state}, no {} file ({} tok)",
                    spec.no_output_noun, rr.tokens_new
                ),
                Some(job),
                None,
            )
            .await;
        None
    };
    match repo_attempt(state, counts.as_ref(), &out_path, &rr) {
        RepoAttempt::Succeeded => {
            log_write_failure(
                store.set_last_kind_at(rid, kind).await,
                format_args!("repo {rid}: recording last {kind} run"),
            );
            repo_succeeded(store, repo, kind).await;
        }
        RepoAttempt::Failed { key, detail } => {
            count_repo_attempt(store, repo, (kind, job), (&key, &detail), &mut summary).await;
        }
        RepoAttempt::Paused => {}
    }
    summary.ingest = counts;
    crate::suppression::apply_reconfirmations(store, repo, &types, &out_path, &pinned).await;
    close_workspace(lane, ws).await;
    Ok(summary)
}

pub async fn run_test_gap(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(scope, lane, cfg, repo, &TEST_GAP_SPEC, backend, resume).await
}
/// The zero-token Renovate scan of `tree`, the repo's scan tree at its
/// fetched tip, with the GitHub token the repo's forge qualifies for.
/// `None` means Renovate has no answer (see `scan_repo`). Runs holding no
/// clone: the tree's scan lease is what keeps it.
async fn renovate_scan(
    cfg: &Config,
    repo: &Repo,
    tree: &crate::workspace::ScanTree,
) -> Option<crate::dep_scan::DepScan> {
    let path = tree.path().to_owned();
    let rn = repo.name.clone();
    let forge = repo.forge;
    let configured = cfg.renovate_github_token.clone();
    tokio::task::spawn_blocking(move || {
        let token = crate::dep_scan::github_token(
            forge,
            configured.as_ref().map(crate::config::Secret::expose),
        );
        crate::dep_scan::scan_repo(&path, &rn, token.as_deref(), 120)
    })
    .await
    .ok()
    .flatten()
}

/// After a Renovate scan whose every candidate was ingested: retire the
/// open findings it no longer proposes, and hand queued ones over to the
/// finding it proposes for the same move. Returns (retired, handed over).
///
/// An open finding Renovate checked and no longer proposes landed some
/// other way or moved to another unit -- never one for a dependency
/// Renovate skipped or could not look up this time. A queued one whose
/// move Renovate proposes under another fingerprint hands its queue over
/// instead, so the same update is not worked twice. Either failing fails
/// the cycle, so the cadence stays put and the next cycle retries it
/// (re-ingesting the same answer changes nothing).
async fn settle_dep_updates(
    store: &Store,
    repo: &Repo,
    scan: &crate::dep_scan::DepScan,
) -> anyhow::Result<(u64, u64)> {
    let proposed: Vec<String> = scan
        .candidates
        .iter()
        .map(|cand| cand.fingerprint.clone())
        .collect();
    let retired = store
        .supersede_unproposed_dep_updates(repo.id, &proposed, &scan.unchecked)
        .await
        .map_err(|e| {
            anyhow::anyhow!("dep_update {}: retiring unproposed updates: {e}", repo.name)
        })?;
    let moves: Vec<crate::store::ProposedMove> = scan
        .candidates
        .iter()
        .flat_map(|cand| {
            cand.moves.iter().map(|mv| crate::store::ProposedMove {
                fingerprint: cand.fingerprint.clone(),
                unit: cand.unit.clone(),
                package: mv.package.clone(),
                major: mv.major,
            })
        })
        .collect();
    let handed_over = store
        .hand_over_queued_dep_updates(repo.id, &moves)
        .await
        .map_err(|e| {
            anyhow::anyhow!("dep_update {}: handing over queued updates: {e}", repo.name)
        })?;
    Ok((retired, handed_over))
}

/// Dependency scan. Renovate answers it for free whenever it can;
/// otherwise an AI worker does.
pub async fn run_dep_update(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    // A resume continues a suspended AI worker, so the Renovate
    // shortcut below is not an option for it: that path spends no
    // tokens, creates no job row, and would leave the suspension
    // uncontinued and still resumable. `run_analysis_job` repeats the
    // clone check and the sync, so nothing is skipped by going straight
    // there.
    if resume.is_some() {
        return run_analysis_job(scope, lane, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await;
    }
    let rpath = std::path::PathBuf::from(&repo.path);
    if !rpath.exists() {
        let failure = SyncFailure {
            key: REPO_NOT_CLONED,
            detail: format!("{} is not cloned", rpath.display()),
            network: false,
        };
        let kind = RepoJobKind::DepUpdate;
        return repo_sync_failed(store, (cfg, backend), repo, kind, failure).await;
    }
    let permit = lane.lock_clone(repo.id).await?;
    if let Err(failure) = sync_to_tip(store, &permit, repo).await? {
        let kind = RepoJobKind::DepUpdate;
        // Let go before the probe (see `run_hunt`).
        drop(permit);
        return repo_sync_failed(store, (cfg, backend), repo, kind, failure).await;
    }

    // Try Renovate (zero tokens). Its answer is authoritative even when it
    // is "nothing to update"; only an unavailable scan falls back to AI,
    // which takes the clone again. The scan tree is made at the fetched
    // tip in the fetch's hold, which ends there: Renovate runs holding
    // only the tree's lease.
    let tip = format!("origin/{}", repo.default_branch);
    let scan = match crate::workspace::ScanTree::create(&cfg.work_root, permit, &tip).await {
        Ok(tree) => {
            let scan = renovate_scan(cfg, repo, &tree).await;
            tree.discard(lane).await;
            scan
        }
        Err(e) => {
            tracing::warn!("dep_update {}: {e}", repo.name);
            None
        }
    };
    let Some(scan) = scan else {
        tracing::info!(
            "dep_update {}: renovate unavailable, falling back to AI",
            repo.name
        );
        return run_analysis_job(scope, lane, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await;
    };

    // Write candidates to JSON and ingest via the standard path
    let out_path = cfg
        .work_root
        .join("out")
        .join(format!("dep_scan_{}.json", repo.id));
    let _ = std::fs::create_dir_all(out_path.parent().unwrap_or(std::path::Path::new(".")));
    let json_entries: Vec<_> = scan
        .candidates
        .iter()
        .map(|cand| {
            serde_json::json!({
                "fingerprint": cand.fingerprint,
                "file": cand.file,
                "ecosystem": cand.ecosystem,
                "package": cand.package,
                "current_version": cand.current_version,
                "latest_version": cand.latest_version,
                "update_type": cand.update_type,
                "severity": cand.severity,
                "confidence": cand.confidence,
                "summary": cand.summary,
                "detail": cand.detail,
            })
        })
        .collect();
    // A write that fails must end the cycle: ingesting from `out_path`
    // anyway would re-file whatever an earlier scan left there.
    std::fs::write(&out_path, serde_json::to_string_pretty(&json_entries)?)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", out_path.display()))?;

    let counts = crate::ingest::ingest_findings(
        store,
        repo.id,
        &out_path,
        EntryTypes::Fixed(FindingType::DepUpdate),
        None,
        None,
    )
    .await;
    let (retired, handed_over) = if counts.invalid == 0 {
        settle_dep_updates(store, repo, &scan).await?
    } else {
        (0, 0)
    };
    let _ = store
        .log_event(
            "dep_update",
            &format!(
                "{}: renovate scan +{} new / {} dup ({} refreshed) / {retired} retired / \
                 {handed_over} queued handed over / {} invalid (0 tok)",
                repo.name, counts.inserted, counts.duplicates, counts.refreshed, counts.invalid,
            ),
            None,
            None,
        )
        .await;

    Ok(renovate_outcome(store, repo, counts).await)
}

/// What a Renovate scan amounts to for the repo's `dep_update` work: one
/// that settled is its outcome, and only then does its timestamp move. One
/// with invalid entries filed its valid ones and failed; it has no job, so
/// nothing to continue from.
async fn renovate_outcome(store: &Store, repo: &Repo, counts: IngestResult) -> CycleSummary {
    let mut summary = CycleSummary {
        kind: Some(RepoJobKind::DepUpdate.into()),
        repo: Some(repo.name.clone()),
        state: Some(JobState::Done),
        ..Default::default()
    };
    if counts.invalid == 0 {
        log_write_failure(
            store.set_last_dep_update(repo.id).await,
            format_args!("repo {}: recording last dep_update run", repo.id),
        );
        repo_succeeded(store, repo, RepoJobKind::DepUpdate).await;
    } else {
        let detail = format!("renovate scan: {} invalid entries", counts.invalid);
        let held = record_repo_failure(
            store,
            (repo.id, &repo.name),
            (RepoWorkKind::DepUpdate, None),
            INVALID_ENTRIES,
            &detail,
            HoldReason::Stuck,
        )
        .await;
        summary.failure = Some(detail);
        summary.outcome = Some(if held { "held" } else { "retry" }.into());
    }
    summary.ingest = Some(counts);
    summary
}
pub async fn run_refactor(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(scope, lane, cfg, repo, &REFACTOR_SPEC, backend, resume).await
}
pub async fn run_modernize(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(scope, lane, cfg, repo, &MODERNIZATION_SPEC, backend, resume).await
}
pub async fn run_standards(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(scope, lane, cfg, repo, &STANDARDS_SPEC, backend, resume).await
}

fn extract_pr_url(text: &str) -> Option<String> {
    for word in text.split_whitespace() {
        if !word.starts_with("https://") {
            continue;
        }
        // `gh` reports a PR URL, `GitLabForge::create_pr` an MR URL.
        for marker in ["/pull/", "/-/merge_requests/"] {
            if let Some(idx) = word.find(marker) {
                let end = idx + marker.len();
                let after = &word[end..];
                let digit_end = after
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(after.len());
                if digit_end > 0 {
                    return Some(word[..end + digit_end].to_owned());
                }
            }
        }
    }
    None
}

/// `fix/<slug>-<id>` (`modernize/`, `improve/` for the other types): the
/// branch every attempt at this finding works on.
fn fix_branch(finding: &Finding) -> String {
    let slug: String = finding
        .summary
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let slug: String = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let slug = &slug[..slug.len().min(40)];
    let slug = slug.trim_end_matches('-');
    let prefix = match finding.kind {
        FindingType::Bug => "fix",
        FindingType::Modernization => "modernize",
        _ => "improve",
    };
    format!("{prefix}/{slug}-{}", finding.id)
}

/// A fix job that owns its finding (`fixing`) and has a working tree. The
/// row and the claim are owned apart, by the [`RunningJob`] and
/// [`FixClaim`] [`start_fix`] returns beside it, since they end at
/// different points.
struct FixStart {
    repo: Repo,
    job: i64,
    ws: OpenWorkspace,
    /// The commit the chain's tree was created at (`jobs.pinned_sha`).
    pinned: String,
    branch: String,
    cap: Option<i64>,
}

/// Put the claimed finding back to `queued` ([`FixClaim::release`]). A
/// release that fails is logged; the claim is then owed, and the cycle's
/// settle retries it.
async fn release_claim(claim: FixClaim) {
    let fid = claim.finding_id();
    log_write_failure(
        claim.release().await,
        format_args!("#{fid}: releasing fixing -> queued"),
    );
}

/// A fix cycle that ends before its job: the finding is not `queued`.
fn fix_skipped(why: String) -> CycleSummary {
    CycleSummary {
        kind: Some(FindingJobKind::Fix.into()),
        skipped: Some(why),
        ..Default::default()
    }
}

/// Everything before the worker runs: budget gate, claim, job row, tree.
/// `Ok(Err(summary))` is a cycle that ends here without a worker (denied,
/// skipped, refused, no tree); every failure after the claim releases it.
#[allow(
    clippy::too_many_lines,
    reason = "linear start sequence: gate, claim, job row, tree. Every step \
              after the claim must release it on failure, which only reads \
              as correct while the steps sit in order beside the claim"
)]
async fn start_fix(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<Result<(FixStart, RunningJob, FixClaim), CycleSummary>> {
    let store = scope.store();
    let fid = finding.id;
    if finding.status != FindingStatus::Queued {
        return Ok(Err(fix_skipped(format!(
            "finding #{fid} is {}, not queued",
            finding.status
        ))));
    }
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("fix #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let branch = fix_branch(finding);
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Fix.into(),
        finding.budget_override.is_some(),
        &format!("fix #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(Err(*d)),
    };
    // Claim before the job exists. A verdict that landed since `pick_next`
    // read the row must win: a job created first would overwrite it with
    // `fixing` and continue or supersede the checkpoint of a finding the
    // operator has just rejected.
    let Some(claim) = scope.claim_fix(fid).await? else {
        return Ok(Err(fix_skipped(format!(
            "finding #{fid} is no longer queued"
        ))));
    };
    if let Err(e) = remove_stale_report(resume) {
        release_claim(claim).await;
        anyhow::bail!("fix #{fid}: {e}");
    }
    let created = match scope
        .create_job(
            resume.map_or(
                JobStart::fresh_finding(FindingJobKind::Fix, fid),
                ResumePlan::start,
            ),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            release_claim(claim).await;
            return job_refused(FindingJobKind::Fix.into(), &repo.name, e).map(Err);
        }
    };
    let job = created.job.id();
    // The branch is per finding, so a superseded fix chain of this finding
    // holds it checked out; `open_workspace` releases that chain's tree
    // before adding this one.
    let permit = lane.lock_clone(repo.id).await?;
    let opened = open_workspace(
        store,
        cfg,
        (permit, &repo),
        created,
        resume,
        TreeSpec::Branch {
            name: branch.clone(),
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Fix.into(),
        &format!("fix #{fid}"),
        Some(fid),
    )
    .await;
    let (ws, pinned, running) = match opened {
        Ok(Ok(opened)) => opened,
        // An unmade tree releases the claim once it is counted.
        Ok(Err(failed)) => return Ok(Err(count_unmade_tree(store, claim, job, failed).await)),
        Err(e) => {
            release_claim(claim).await;
            return Err(e);
        }
    };
    Ok(Ok((
        FixStart {
            repo,
            job,
            ws,
            pinned,
            branch,
            cap,
        },
        running,
        claim,
    )))
}

/// The file a worker of any kind writes when it cannot go on without
/// something only a human can provide (WORK-MODEL.md §9.1): its report
/// holds the work as a missing prerequisite.
const BLOCKED_REPORT: &str = "BLOCKED.md";

/// A `BLOCKED.md` in the tree a resume continues is removed before the
/// worker runs only when it is not this chain's open question: the resume
/// follows the operator's `continue` of a hold, which recorded it (the
/// daemon died before deleting it), or it is a handoff, whose engage wrote
/// it next to its `WITHDRAW.md`. Any other report is one no hold has
/// recorded yet, and it holds the work once this attempt ends, whatever
/// state that attempt ends in. An error means a recorded report would be
/// read as this attempt's outcome, so it stops the attempt.
fn remove_stale_report(resume: Option<&ResumePlan>) -> anyhow::Result<()> {
    let Some(plan) = resume.filter(|p| p.after_hold.is_some() || p.handoff) else {
        return Ok(());
    };
    let stale = plan.workspace.tree.join(BLOCKED_REPORT);
    if stale.exists()
        && let Err(e) = std::fs::remove_file(&stale)
    {
        anyhow::bail!("cannot remove the stale {}: {e}", stale.display());
    }
    Ok(())
}

/// The worker's `BLOCKED.md`, if it wrote one: its bytes, read lossily.
fn blocked_report(tree: &Path) -> std::io::Result<Option<String>> {
    let path = tree.join(BLOCKED_REPORT);
    if !path.exists() {
        return Ok(None);
    }
    read_report(&path).map(Some)
}

/// Hold `kind`'s work for the operator on the worker's `BLOCKED.md`
/// (WORK-MODEL.md §9.1): reason `prerequisite`, the report as the detail,
/// `job` as the checkpoint. Then the report leaves the tree, so the hold
/// owns it and a resume cannot mistake it for its own outcome. `false`
/// when nothing was held: the finding has left the kind's tier.
async fn hold_on_report(
    store: &Store,
    kind: FindingJobKind,
    fid: i64,
    job: i64,
    ws: &Workspace,
    report: &str,
) -> anyhow::Result<bool> {
    if !store
        .hold_work(fid, kind, HoldReason::Prerequisite, report, Some(job))
        .await?
    {
        return Ok(false);
    }
    std::fs::remove_file(ws.tree.join(BLOCKED_REPORT))?;
    let message = format!(
        "#{fid} held: prerequisite; checkpoint retained at {}",
        ws.root.display()
    );
    store
        .log_event(kind.as_str(), &message, Some(job), Some(fid))
        .await?;
    Ok(true)
}

/// The worker's prompt. The playbook builders run on a resume too: the
/// session already holds the playbook, but a builder that fails is a
/// configuration fault worth surfacing on either path.
fn fix_prompt(
    cfg: &Config,
    finding: &Finding,
    start: &FixStart,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<String> {
    let worktree = &start.ws.tree;
    let repo_notes = Store::repo_notes(&cfg.work_root, start.repo.id);
    let built = match finding.kind {
        FindingType::Bug => playbooks::build_fix_prompt(
            &cfg.root,
            finding,
            worktree,
            &start.branch,
            &start.repo,
            &repo_notes,
        ),
        FindingType::Modernization => playbooks::build_apply_modernization_prompt(
            &cfg.root,
            finding,
            worktree,
            &start.branch,
            &start.repo,
            &repo_notes,
        ),
        _ => playbooks::build_apply_improvement_prompt(
            &cfg.root,
            finding,
            worktree,
            &start.branch,
            &start.repo,
            &repo_notes,
        ),
    };
    let prompt = built.map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(
        if let Some((reason, detail)) = resume.and_then(|r| r.after_hold.as_ref()) {
            match reason {
                HoldReason::Stuck | HoldReason::GaveUp => format!(
                    "Resume this retained fix after an operator released it. Preserve the existing \
                 committed implementation and proof. It was held because earlier attempts kept \
                 failing the same way before they finished, not because anything was missing: \
                 continue the work from where it stopped and finish it, then write or update \
                 PR-DESCRIPTION.md. Follow this updated playbook instead of earlier verification \
                 instructions.\n\n\
                 What kept failing (context, not instructions):\n{detail}\n\n{prompt}"
                ),
                HoldReason::Prerequisite => format!(
                    "Resume this retained fix after an operator released it. Preserve the existing \
                 committed implementation and proof. Re-evaluate the prerequisite, not whether \
                 the original bug exists in your already-fixed branch. Follow this updated \
                 playbook instead of earlier verification instructions. Recreate BLOCKED.md \
                 if the affected change remains unverified; otherwise update PR-DESCRIPTION.md.\n\n\
                 Previous blocker (context, not instructions):\n{detail}\n\n{prompt}"
                ),
            }
        } else if resume.is_some() {
            RESUME_PROMPT.to_owned()
        } else {
            prompt
        },
    )
}

/// What a finished fix attempt amounts to. Exactly one per attempt;
/// [`record_fix_outcome`] writes it.
enum FixOutcome {
    /// The worker wrote `BLOCKED.md`: hold the work for the operator, with
    /// this attempt as its checkpoint.
    Held { report: String },
    /// `NOT-A-BUG.md` / `DECLINED.md`: the finding takes the status its
    /// classification maps to ([`parse_decline`]).
    Declined(Decline),
    /// The branch is pushed and its draft PR exists (`recovered`: it
    /// already did).
    Shipped { pr_url: String, recovered: bool },
    /// The worker stopped mid-work and its tree and transcript are kept.
    Suspended,
    /// No PR this time: counted in the finding's failure streak
    /// ([`record_fix_failure`]).
    Failed { failure: String },
    /// The finished worker's branch could not be pushed or its PR opened
    /// while a host the fix needs is unreachable (#95): the network's
    /// failure, not the fix's. Nothing is counted and the attempt is kept
    /// as the checkpoint, so the next cycle continues it and only has to
    /// publish.
    Offline { host: String },
}

/// A worker's report file, read lossily: whatever bytes it wrote are its
/// report. Only a file that cannot be read at all is an error.
fn read_report(path: &Path) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

/// A declined fix's report, as the finding will record it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Decline {
    class: Option<ClosureClass>,
    /// `"<classification>: <explanation>"`, or the whole report when it
    /// classifies nothing.
    reason: String,
    /// The `Holds while:` line: the condition in the code that makes the
    /// verdict true.
    holds_while: Option<String>,
    /// The `Depends on:` paths the verdict rests on.
    depends_on: Vec<String>,
}

/// Parse a declined fix's report.
///
/// The first line names the classification (`Classification: superseded`;
/// Markdown emphasis around it is ignored). Optional `Holds while:` and
/// `Depends on:` lines may follow it directly, and the explanation comes
/// after them. The reason is `"<classification>: <explanation>"`, the
/// shape a closed-PR harvest records. A report whose first line names no
/// classification, or that explains nothing after it, keeps its whole text
/// and has none: nothing then says the premise failed, so nothing may
/// suppress it.
fn parse_decline(report: &str) -> Decline {
    let body = report.trim_start();
    let (first, rest) = body.split_once('\n').unwrap_or((body, ""));
    let class = header(first)
        .filter(|(key, _)| key == "classification")
        .and_then(|(_, word)| {
            word.trim_matches('`')
                .to_ascii_lowercase()
                .parse::<ClosureClass>()
                .ok()
        });
    let mut holds_while = None;
    let mut depends_on = Vec::new();
    let mut lines = rest.lines().peekable();
    while let Some(line) = lines.peek() {
        match header(line) {
            Some((key, value)) if key == "holds while" => holds_while = Some(value),
            Some((key, value)) if key == "depends on" => depends_on.extend(
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned),
            ),
            _ if line.trim().is_empty() => {}
            _ => break,
        }
        lines.next();
    }
    let explanation = lines.collect::<Vec<_>>().join("\n");
    let explanation = explanation.trim();
    let class = class.filter(|_| !explanation.is_empty());
    let Some(class) = class else {
        return Decline {
            class: None,
            reason: report.chars().take(500).collect(),
            holds_while: None,
            depends_on: Vec::new(),
        };
    };
    // The bound every verdict reason gets: a suppressing one is injected
    // into every later scan's suppression list.
    Decline {
        class: Some(class),
        reason: format!("{class}: {explanation}")
            .chars()
            .take(500)
            .collect(),
        holds_while,
        depends_on,
    }
}

/// A `Key: value` header line of a worker's report, its key lowercased and
/// stripped of Markdown emphasis and a list bullet (`- **Holds while:**`),
/// its value stripped of surrounding emphasis.
fn header(line: &str) -> Option<(String, String)> {
    let (key, value) = line.split_once(':')?;
    let key: String = key
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '_' | '#'))
        .collect();
    let key = key.trim().trim_start_matches('-').trim();
    let value = value.trim_matches(|c: char| c == '*' || c.is_whitespace());
    Some((key.to_ascii_lowercase(), value.to_owned()))
}

/// Read the worker's result and decide the attempt's outcome. Shipping is
/// part of deciding (a push or a PR that fails is a failed attempt). An
/// error leaves the claim to the caller, which releases it.
async fn conclude_fix(
    (cfg, backend): (&Config, &dyn Backend),
    finding: &Finding,
    start: &FixStart,
    state: JobState,
) -> anyhow::Result<FixOutcome> {
    let fid = finding.id;
    let worktree = &start.ws.tree;
    let decline_name = if finding.kind == FindingType::Bug {
        "NOT-A-BUG.md"
    } else {
        "DECLINED.md"
    };
    let decline_file = worktree.join(decline_name);
    let blocked_file = worktree.join("BLOCKED.md");
    let declined = decline_file.exists();
    let report_file = if declined {
        Some(&decline_file)
    } else if blocked_file.exists() {
        Some(&blocked_file)
    } else {
        None
    };
    // Any bytes the worker wrote are its report; only a file that cannot be
    // read at all (a directory, permissions) is an error, and that must not
    // strand the finding at `fixing` (the caller releases the claim).
    let report = match report_file.map(|f| read_report(f)).transpose() {
        Ok(report) => report,
        Err(e) => anyhow::bail!("fix #{fid}: cannot read the worker's report: {e}"),
    };
    if declined {
        return Ok(FixOutcome::Declined(parse_decline(
            &report.unwrap_or_default(),
        )));
    }
    if let Some(report) = report {
        return Ok(FixOutcome::Held { report });
    }

    let ShipFailure { failure, at_forge } =
        match ship_pr(&start.repo, &start.branch, worktree, state).await {
            Ok((pr_url, recovered)) => return Ok(FixOutcome::Shipped { pr_url, recovered }),
            Err(failure) => failure,
        };
    if state == JobState::Suspended {
        // A suspension is a pause, not a failure: counting it toward the
        // streak would turn three pauses of one healthy fix into "stuck".
        return Ok(FixOutcome::Suspended);
    }
    if at_forge && let Some(host) = unreachable_now(cfg, &start.repo.url, backend).await {
        return Ok(FixOutcome::Offline { host });
    }
    Ok(FixOutcome::Failed { failure })
}

/// Count a failed fix attempt in the finding's streak. The
/// [`MAX_CONSECUTIVE_SAME_FAILURE`]th identical failure holds the work for
/// the operator ([`Store::hold_work`]) as `at_limit` — `stuck`, or
/// `gave_up` when the give-up ceiling's failure completed the streak —
/// with `job` as its checkpoint if a worker ran in it; short of that the
/// fix tier tries again. `detail` follows the failure in the hold and the
/// event: the worker's output tail, why a chain gave up, or why a tree
/// could not be made.
///
/// Every failed fix is counted here — a worker's ([`record_fix_outcome`]),
/// a given-up resume chain ([`count_given_up_chain`]) and a tree that could
/// not be made ([`count_unmade_tree`]) alike — so they cannot drift apart
/// on what a stuck fix becomes. A count or a hold that could not be
/// recorded is reported as such and ends nothing.
async fn record_fix_failure(
    store: &Store,
    fid: i64,
    job: i64,
    failure: &str,
    detail: &str,
    at_limit: HoldReason,
    summary: &mut CycleSummary,
) {
    summary.failure = Some(failure.to_owned());
    summary.outcome = Some("requeued".into());
    let streak = match store
        .record_work_failure(fid, FindingJobKind::Fix, failure)
        .await
    {
        Ok(streak) => streak,
        Err(e) => {
            let message =
                format!("#{fid} incomplete ({failure}), not counted: {e}. tail: {detail}");
            let _ = fix_event(store, "fix", fid, job, message).await;
            return;
        }
    };
    if streak < MAX_CONSECUTIVE_SAME_FAILURE {
        let message = format!("#{fid} incomplete ({failure}). tail: {detail}");
        let _ = fix_event(store, "fix", fid, job, message).await;
        return;
    }
    let report = format!("{failure}\n\n{detail}");
    let message = match store
        .hold_work(fid, FindingJobKind::Fix, at_limit, &report, Some(job))
        .await
    {
        Ok(true) => {
            summary.outcome = Some("held".into());
            summary.attempts = Some(streak);
            format!("#{fid} held after {streak} identical failures ({failure}). tail: {detail}")
        }
        Ok(false) => format!(
            "#{fid} {streak} identical failures ({failure}), but it has left the fix tier; \
             nothing held. tail: {detail}"
        ),
        Err(e) => format!(
            "#{fid} {streak} identical failures ({failure}), but holding it failed: {e}; \
             will retry. tail: {detail}"
        ),
    };
    let _ = fix_event(store, "fix", fid, job, message).await;
}

/// How [`open_workspace`] reports a tree it could not make, and the
/// failure a finding's work that hit it records in its streak.
const TREE_NOT_MADE: &str = "workspace not created";

/// Count an engage whose tree could not be made ([`open_workspace`] has
/// failed its job and logged why) as a failed engage under the fixed
/// [`TREE_NOT_MADE`], as for every other kind. Uncounted, the flagged PR
/// stays in the engage tier, the highest of them, and is picked every
/// cycle.
async fn count_unmade_engage_tree(
    store: &Store,
    fid: i64,
    job: i64,
    mut summary: CycleSummary,
) -> CycleSummary {
    let note = summary.failure.clone().unwrap_or_default();
    let held = record_engage_failure(
        store,
        fid,
        Some(job),
        TREE_NOT_MADE,
        &note,
        HoldReason::Stuck,
    )
    .await;
    summary.outcome = Some(if held { "held" } else { "retry" }.into());
    summary
}

/// The failure a work item records when the backend could not run its
/// worker at all (`Backend::run` returned an error). The error follows it
/// in the detail; it is not the key, which must stay the same for one
/// outage to count as identical failures.
const RUNNER_ERROR: &str = "runner error";

/// Whose work an attempt was: a finding's, or a repo's.
#[derive(Clone, Copy)]
enum WorkOf<'a> {
    Finding(FindingJobKind, i64),
    Repo(RepoJobKind, &'a Repo),
}

impl WorkOf<'_> {
    fn job_kind(self) -> JobKind {
        match self {
            Self::Finding(kind, _) => kind.into(),
            Self::Repo(kind, _) => kind.into(),
        }
    }

    fn finding_id(self) -> Option<i64> {
        match self {
            Self::Finding(_, fid) => Some(fid),
            Self::Repo(..) => None,
        }
    }
}

/// Count work whose backend failed before any worker ran (WORK-MODEL.md
/// §7). The job is closed `failed` as unstarted rather than left
/// `running`, and the failure goes through the work item's own streak, so
/// a backend that keeps failing ends in a hold instead of the same work
/// being picked again every cycle. The caller releases what it holds (a
/// fix's claim, the tree) and returns the error, which fails the cycle as
/// before.
///
/// Unless a host the work needs is unreachable now ([`unreachable_now`]):
/// a backend that cannot start a worker offline is the network's failure.
/// Then the job is closed with that note, nothing is counted, and the
/// returned summary ends the cycle denied for the host, as the network
/// gate's own denial does (#95).
async fn runner_failed(
    store: &Store,
    (cfg, backend, repo_url): (&Config, &dyn Backend, &str),
    (work, running): (WorkOf<'_>, RunningJob),
    err: &anyhow::Error,
) -> Option<CycleSummary> {
    let job = running.id();
    if let Some(host) = unreachable_now(cfg, repo_url, backend).await {
        log_write_failure(
            running
                .fail_unstarted(&format!(
                    "offline: {host} unreachable; {RUNNER_ERROR}: {err}"
                ))
                .await,
            format_args!("job {job}: failing unstarted"),
        );
        return Some(
            offline_denial(store, work.job_kind(), work.finding_id(), Some(job), host).await,
        );
    }
    let detail = format!("{RUNNER_ERROR}: {err}");
    log_write_failure(
        running.fail_unstarted(&detail).await,
        format_args!("job {job}: failing unstarted"),
    );
    let mut summary = CycleSummary::default();
    let (kind, fid) = match work {
        WorkOf::Finding(kind, fid) => (kind, fid),
        WorkOf::Repo(kind, repo) => {
            record_repo_failure(
                store,
                (repo.id, &repo.name),
                (kind.into(), Some(job)),
                RUNNER_ERROR,
                &detail,
                HoldReason::Stuck,
            )
            .await;
            return None;
        }
    };
    match kind {
        FindingJobKind::Recheck => {
            handle_recheck_failure(
                store,
                &mut summary,
                fid,
                job,
                RUNNER_ERROR,
                &detail,
                HoldReason::Stuck,
                None,
            )
            .await;
        }
        FindingJobKind::Fix => {
            record_fix_failure(
                store,
                fid,
                job,
                RUNNER_ERROR,
                &err.to_string(),
                HoldReason::Stuck,
                &mut summary,
            )
            .await;
        }
        FindingJobKind::Engage => {
            record_engage_failure(
                store,
                fid,
                Some(job),
                RUNNER_ERROR,
                &detail,
                HoldReason::Stuck,
            )
            .await;
        }
        FindingJobKind::Harvest => {
            record_harvest_failure(
                store,
                fid,
                Some(job),
                RUNNER_ERROR,
                &detail,
                HoldReason::Stuck,
            )
            .await;
        }
    }
    None
}

/// Count a fix whose tree could not be made ([`open_workspace`] has failed
/// its job and logged why) as a failed attempt ([`record_fix_failure`]),
/// then release the claim. Uncounted, the finding would go back to
/// `queued` unchanged, the oldest-first pick would take it again every
/// cycle, and no other queued fix would run. The streak records the fixed
/// [`TREE_NOT_MADE`], not the reason: that quotes git, which can name this
/// job's own tree, so it differs on every attempt and would reset the
/// streak each time. The reason follows it in the report, and the summary
/// keeps it whole.
async fn count_unmade_tree(
    store: &Store,
    claim: FixClaim,
    job: i64,
    mut summary: CycleSummary,
) -> CycleSummary {
    let fid = claim.finding_id();
    let note = summary.failure.take().unwrap_or_default();
    let why = note
        .strip_prefix(TREE_NOT_MADE)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(&note);
    // Counted while the claim still holds the finding, as a worker's
    // failure is ([`record_fix_outcome`]).
    record_fix_failure(
        store,
        fid,
        job,
        TREE_NOT_MADE,
        why,
        HoldReason::Stuck,
        &mut summary,
    )
    .await;
    release_claim(claim).await;
    summary.failure = Some(note);
    summary
}

/// Why a fix did not ship: the failure as the streak counts it, and
/// whether it came from the forge (the push, the PR's creation), the
/// failures a network that went away can cause.
struct ShipFailure {
    failure: String,
    at_forge: bool,
}

impl ShipFailure {
    fn local(failure: String) -> Self {
        Self {
            failure,
            at_forge: false,
        }
    }

    fn forge(failure: String) -> Self {
        Self {
            failure,
            at_forge: true,
        }
    }
}

/// Push the fix branch and open its draft PR: `(url, recovered)`, or why
/// not. Nothing to ship (no commits, no `PR-DESCRIPTION.md`, a worker
/// that did not finish) is a failure like a push that fails.
async fn ship_pr(
    repo: &Repo,
    branch: &str,
    worktree: &Path,
    state: JobState,
) -> Result<(String, bool), ShipFailure> {
    let db = repo.default_branch.clone();
    let wt = worktree.to_owned();
    let db2 = db.clone();
    let (rc, commits) = tokio::task::spawn_blocking(move || {
        let commits = |range: &str| Unheld::Commits { range }.run(&wt, 30);
        let (rc, out) = commits(&format!("origin/{db2}..HEAD"));
        if rc != 0 {
            commits(&format!("{db2}..HEAD"))
        } else {
            (rc, out)
        }
    })
    .await
    .unwrap_or((127, String::new()));
    let commits = commits.trim();
    let pr_desc = worktree.join("PR-DESCRIPTION.md");
    if rc != 0 || commits.is_empty() || !pr_desc.exists() {
        return Err(ShipFailure::local(if state == JobState::Done {
            if commits.is_empty() {
                "no commits".to_owned()
            } else {
                "no PR-DESCRIPTION.md".to_owned()
            }
        } else {
            format!("worker {state}")
        }));
    }

    let f = forge::forge_for(repo.forge);
    let push_url = f.ssh_url(&repo.url);
    let wt = worktree.to_owned();
    let (prc, pout) = tokio::task::spawn_blocking(move || {
        Unheld::Push {
            force: Force::Always,
            url: &push_url,
            refspec: "HEAD",
        }
        .run(&wt, 600)
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if prc != 0 {
        let tail = crate::util::tail(&pout, 300);
        return Err(ShipFailure::forge(format!("push failed: {tail}")));
    }
    if f.owner_repo(&repo.url).is_none() {
        return Err(ShipFailure::local(format!(
            "unparseable repo url for PR: {:?}",
            repo.url
        )));
    }
    let body = std::fs::read_to_string(&pr_desc).unwrap_or_default();
    let wt = worktree.to_owned();
    let (_trc, title) = tokio::task::spawn_blocking(move || Unheld::HeadSubject.run(&wt, 30))
        .await
        .unwrap_or((127, branch.to_owned()));
    let title = title.trim();
    let title = if title.is_empty() { branch } else { title };
    match f.create_pr(Path::new(&repo.path), branch, &db, title, &body) {
        Ok(pr_url) => Ok((pr_url, false)),
        Err(e) => {
            let err_msg = e.to_string();
            if err_msg.contains("already exists")
                && let Some(pr_url) = extract_pr_url(&err_msg)
            {
                return Ok((pr_url, true));
            }
            let head: String = err_msg.chars().take(300).collect();
            Err(ShipFailure::forge(format!("PR create failed: {head}")))
        }
    }
}

/// Log a fix-job event against its job and finding.
async fn fix_event(
    store: &Store,
    kind: &str,
    fid: i64,
    job: i64,
    message: String,
) -> sqlx::Result<()> {
    store.log_event(kind, &message, Some(job), Some(fid)).await
}

/// A worker's `BLOCKED.md`: the fix is held for the operator as a missing
/// prerequisite ([`hold_on_report`]), and the claim goes back to `queued`,
/// where the fix tier skips held work. The budget override is left alone:
/// nothing concluded.
async fn hold_prerequisite(
    store: &Store,
    start: &FixStart,
    claim: FixClaim,
    report: &str,
    summary: &mut CycleSummary,
) -> anyhow::Result<()> {
    let fid = claim.finding_id();
    let held = hold_on_report(
        store,
        FindingJobKind::Fix,
        fid,
        start.job,
        &start.ws,
        report,
    )
    .await;
    release_claim(claim).await;
    if !held? {
        anyhow::bail!("fix #{fid}: BLOCKED.md, but the fix has left its tier; nothing held");
    }
    summary.outcome = Some("held".into());
    Ok(())
}

/// A fix the worker declined (`NOT-A-BUG.md` / `DECLINED.md`): the finding
/// takes its classification's status with the worker's reason, as a closed
/// PR's does. Only `wrong` and `unwanted` suppress; most declines are work
/// that landed another way, and rejecting those told every later scan to
/// stop reporting valid findings. An unclassified decline goes back to
/// triage (`new`), like an `abandoned` closure. A suppressing verdict is
/// anchored to the commit the fix's tree was created at.
async fn record_declined_fix(
    store: &Store,
    finding: &Finding,
    start: &FixStart,
    claim: FixClaim,
    decline: Decline,
    summary: &mut CycleSummary,
) {
    let fid = finding.id;
    let job = start.job;
    let Decline {
        class,
        reason,
        holds_while,
        depends_on,
    } = decline;
    let reason = reason.as_str();
    let status = crate::transition::decline_status(class);
    let anchor = crate::suppression::verdict_anchor(
        status.into(),
        &start.pinned,
        finding,
        holds_while.as_deref(),
        &depends_on,
    );
    match claim.conclude(class, reason, anchor.as_ref()).await {
        Ok(Moved::Applied) => {}
        // Something moved the finding off its claim meanwhile, and that
        // decision stands: the decline records nothing.
        Ok(Moved::Conflict { now }) => {
            let edge = (FindingStatus::Fixing, status);
            log_not_moved(store, fid, Some(job), "fix", edge, now).await;
            return;
        }
        // A claim whose verdict could not be written goes back to `queued`.
        Err((claim, e)) => {
            log_write_failure(Err::<(), _>(e), format_args!("#{fid}: verdict -> {status}"));
            release_claim(claim).await;
        }
    }
    let first_line: String = reason
        .lines()
        .next()
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_default();
    let message = match class {
        Some(class) => format!("#{fid} declined by worker as {class} -> {status}: {first_line}"),
        None => {
            format!("#{fid} declined by worker without a classification -> {status}: {first_line}")
        }
    };
    let _ = fix_event(store, "fix", fid, job, message).await;
    summary.outcome = Some(FindingStatus::from(status).as_str().into());
}

/// Write a fix outcome: the finding's status, the event, and the summary.
/// A one-shot budget override is spent by every outcome but a `BLOCKED.md`
/// hold, a decline, and an attempt that could not publish offline (none of
/// them concluded the work's attempt; a streak's hold did, and spends it);
/// any outcome that leaves the finding `fixing` returns it to `queued`.
/// Every outcome ends `claim`.
async fn record_fix_outcome(
    store: &Store,
    finding: &Finding,
    (start, claim): (&FixStart, FixClaim),
    outcome: FixOutcome,
    stdout_tail: &str,
    summary: &mut CycleSummary,
) -> anyhow::Result<()> {
    let fid = finding.id;
    let job = start.job;
    let tail = crate::util::tail(stdout_tail, 300);
    // The claim still to release once the outcome is written: none once
    // the finding has moved on to `pr_open`.
    let unreleased = match outcome {
        FixOutcome::Held { report } => {
            return hold_prerequisite(store, start, claim, &report, summary).await;
        }
        FixOutcome::Declined(decline) => {
            record_declined_fix(store, finding, start, claim, decline, summary).await;
            return Ok(());
        }
        FixOutcome::Shipped { pr_url, recovered } => {
            // A claim whose `pr_open` could not be written goes back to
            // `queued`.
            let (lost, unopened) = match claim.open_pr(&pr_url).await {
                Ok(Moved::Applied) => (false, None),
                // The PR exists, but something moved the finding off its
                // claim meanwhile, and that decision stands: it is not
                // `pr_open`, and nothing is left to release.
                Ok(Moved::Conflict { now }) => {
                    let edge = (FindingStatus::Fixing, FindingStatus::PrOpen);
                    log_not_moved(store, fid, Some(job), "ship", edge, now).await;
                    (true, None)
                }
                Err((claim, e)) => {
                    log_write_failure(
                        Err::<(), _>(e),
                        format_args!("#{fid}: status -> pr_open ({pr_url})"),
                    );
                    (false, Some(claim))
                }
            };
            let message = if recovered {
                format!("#{fid} recovered existing PR: {pr_url}")
            } else {
                format!("#{fid} draft PR: {pr_url}")
            };
            let _ = fix_event(store, "ship", fid, job, message).await;
            if !lost {
                summary.outcome = Some("pr_open".into());
            }
            summary.pr_url = Some(pr_url);
            unopened
        }
        FixOutcome::Suspended => {
            // Back to queued below, where the fix tier continues it.
            let _ = fix_event(
                store,
                "fix",
                fid,
                job,
                format!(
                    "#{fid} suspended; worktree kept at {} for the resume. tail: {tail}",
                    start.ws.tree.display()
                ),
            )
            .await;
            summary.outcome = Some("suspended".into());
            summary.worktree = Some(start.ws.tree.to_string_lossy().into_owned());
            Some(claim)
        }
        FixOutcome::Failed { failure } => {
            record_fix_failure(store, fid, job, &failure, tail, HoldReason::Stuck, summary).await;
            Some(claim)
        }
        FixOutcome::Offline { host } => {
            // Nothing concluded, so a one-shot override is kept for the
            // attempt that publishes.
            log_write_failure(
                store.keep_attempt_as_checkpoint(job).await,
                format_args!("job {job}: keeping the unpublished attempt as the checkpoint"),
            );
            *summary = offline_denial(
                store,
                FindingJobKind::Fix.into(),
                Some(fid),
                Some(job),
                host,
            )
            .await;
            release_claim(claim).await;
            return Ok(());
        }
    };
    if finding.budget_override == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
    if let Some(claim) = unreleased {
        release_claim(claim).await;
    }
    Ok(())
}

/// Run a fix job (`scheduler.run_fix`): start it ([`start_fix`]), run the
/// worker, then decide ([`conclude_fix`]) and record
/// ([`record_fix_outcome`]) what the attempt amounts to.
pub async fn run_fix(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let fid = finding.id;
    let (start, running, claim) =
        match start_fix(scope, lane, cfg, finding, backend, resume).await? {
            Ok(start) => start,
            Err(ended) => return Ok(ended),
        };
    // Any early return from here on drops `running`: the cycle's settle
    // records the abandoned job.
    let prompt = match fix_prompt(cfg, finding, &start, resume) {
        Ok(prompt) => prompt,
        Err(e) => {
            release_claim(claim).await;
            return Err(e);
        }
    };
    let model = cfg.model_for("fix");
    let rr = match backend
        .run(
            &start.ws,
            &prompt,
            start.cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Counted while the claim still holds the finding, as a
            // worker's failure is.
            let ran = (WorkOf::Finding(FindingJobKind::Fix, fid), running);
            let denied = runner_failed(store, (cfg, backend, &start.repo.url), ran, &e).await;
            release_claim(claim).await;
            close_workspace(lane, start.ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Finding(FindingJobKind::Fix, fid), running);
    let running = match unmetered_offline(store, cfg, backend, &start.repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            release_claim(claim).await;
            close_workspace(lane, start.ws).await;
            return Ok(denied);
        }
    };
    let state = log_write_failure(
        record_job(store, running, &rr, model, resume)
            .await
            .map(|r| r.state()),
        format_args!("job {}: recording outcome", start.job),
    )
    .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Fix.into()),
        finding_id: Some(fid),
        job_id: Some(start.job),
        state: Some(state),
        branch: Some(start.branch.clone()),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };
    let outcome = match conclude_fix((cfg, backend), finding, &start, state).await {
        Ok(outcome) => outcome,
        Err(e) => {
            release_claim(claim).await;
            return Err(e);
        }
    };
    record_fix_outcome(
        store,
        finding,
        (&start, claim),
        outcome,
        &rr.stdout_tail,
        &mut summary,
    )
    .await?;
    close_workspace(lane, start.ws).await;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// sync_prs helpers (`scheduler._iso_ms` … `scheduler._attention_fingerprint`)
// ---------------------------------------------------------------------------

/// ISO-8601 / RFC-3339 timestamp -> epoch ms (0 when absent/unparseable).
/// GitHub emits `2024-01-15T12:30:45Z`; GitLab may use `+00:00` suffix.
/// Handles both, plus the offset-less variant (treated as UTC).
#[allow(clippy::many_single_char_names)]
pub(crate) fn iso_ms(ts: &str) -> i64 {
    if ts.is_empty() {
        return 0;
    }
    // Try the standard-library approach: DateTime::parse_from_rfc3339 equivalent
    // via jiff/time is unavailable, so parse manually.
    // Expected: YYYY-MM-DDTHH:MM:SS[.frac](Z|+HH:MM|-HH:MM|)
    let (date_time_part, tz_offset_secs) = if let Some(pos) = ts.rfind('Z') {
        (&ts[..pos], 0i64)
    } else if let Some(pos) = ts.rfind('+').filter(|&p| p > 10) {
        // The +10 filter skips a '+' that might appear in the date portion
        let tz = &ts[pos + 1..];
        let secs = parse_tz_offset(tz);
        (&ts[..pos], secs)
    } else if let Some(pos) = ts.rfind('-').filter(|&p| p > 10) {
        let tz = &ts[pos + 1..];
        let secs = parse_tz_offset(tz);
        (&ts[..pos], -secs)
    } else {
        // No timezone suffix — treat as UTC
        (ts, 0i64)
    };
    // Parse YYYY-MM-DDTHH:MM:SS[.frac]
    let parts: Vec<&str> = date_time_part.splitn(2, 'T').collect();
    if parts.len() != 2 {
        return 0;
    }
    let date_parts: Vec<&str> = parts[0].split('-').collect();
    let time_full = parts[1];
    let time_parts: Vec<&str> = time_full.splitn(2, '.').collect();
    let hms: Vec<&str> = time_parts[0].split(':').collect();
    if date_parts.len() != 3 || hms.len() != 3 {
        return 0;
    }
    let (Ok(y), Ok(mo), Ok(d)) = (
        date_parts[0].parse::<i64>(),
        date_parts[1].parse::<u32>(),
        date_parts[2].parse::<u32>(),
    ) else {
        return 0;
    };
    let (Ok(h), Ok(mi), Ok(s)) = (
        hms[0].parse::<i64>(),
        hms[1].parse::<i64>(),
        hms[2].parse::<i64>(),
    ) else {
        return 0;
    };
    // Fractional seconds -> ms
    let frac_ms: i64 = if time_parts.len() == 2 {
        let frac = time_parts[1];
        // Timestamps come from the forge API; a malformed non-ASCII
        // fraction must not panic the slice.
        let digits = frac.len().min(3);
        let n: i64 = frac.get(..digits).unwrap_or("").parse().unwrap_or(0);
        // Scale to ms: if 1 digit, *100; 2 digits, *10; 3 digits, *1
        n * 10i64.pow((3 - digits) as u32)
    } else {
        0
    };
    // days since epoch using a simplified civil_to_days (Hinnant's algorithm)
    let epoch_days = civil_to_days(y, mo, d);
    let epoch_secs = epoch_days * 86400 + h * 3600 + mi * 60 + s - tz_offset_secs;
    epoch_secs * 1000 + frac_ms
}

/// Parse HH:MM timezone offset -> total seconds.
fn parse_tz_offset(tz: &str) -> i64 {
    let parts: Vec<&str> = tz.split(':').collect();
    let h: i64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let m: i64 = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    h * 3600 + m * 60
}

/// Proleptic-Gregorian days since 1970-01-01 (Hinnant's `civil_from_days` inverse).
fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + i64::from(doe) - 719_468
}

/// Sync PRs for all `pr_open` findings (`scheduler.sync_prs`), as `pass`:
/// no other sync of the store runs meanwhile, and each PR is read once
/// ([`PrSyncPass`]).
#[allow(
    clippy::too_many_lines,
    reason = "one pass per open PR with a flat decision table over PR \
              state (merged, closed, review comments, checks); the arms \
              read as a table only while they sit together"
)]
pub async fn sync_prs(pass: &PrSyncPass, cfg: &Config) -> SyncResult {
    let store = pass.store();
    let mut summary = SyncResult::default();
    let findings = store
        .list_findings(&FindingFilter {
            status: Some(FindingStatus::PrOpen),
            ..FindingFilter::default()
        })
        .await
        .unwrap_or_default();

    for f in &findings {
        let fid = f.id;
        let url = match f.pr_url.as_deref() {
            Some(u) if !u.is_empty() => u,
            _ => {
                // pr_open with no PR URL = a fix attempt failed before creating
                // the PR. Requeue so it gets retried instead of sitting stuck.
                tracing::warn!(
                    finding_id = fid,
                    "pr_open finding has no pr_url — requeueing"
                );
                if make_transition(store, fid, None, "fix", &Transition::PrMissing).await {
                    let _ = store.log_event(
                        "fix", &format!("#{fid} requeued: pr_open with no pr_url (prior fix failed before PR creation)"),
                        None, Some(fid),
                    ).await;
                }
                continue;
            }
        };
        let Ok(Some(repo)) = store.get_repo_by_id(f.repo_id).await else {
            let _ = store
                .log_event(
                    "error",
                    &format!("sync #{fid}: repo {} missing", f.repo_id),
                    None,
                    Some(fid),
                )
                .await;
            summary.errors += 1;
            continue;
        };
        let fg = forge::forge_for(repo.forge);
        let Some(read_pr) = PrRef::parse(&*fg, url) else {
            let _ = store
                .log_event(
                    "error",
                    &format!("sync #{fid}: unparseable pr_url {url:?}"),
                    None,
                    Some(fid),
                )
                .await;
            summary.errors += 1;
            continue;
        };
        let pr_number = read_pr.number();
        // Begun before the read, not after: the observation's `synced_at`
        // is when the forge was asked, and the push count it carries is the
        // one before the forge answered, so a push landing while the read
        // is out leaves this observation counted as older than that push.
        let begun = log_write_failure(
            pass.begin_read(fid, &read_pr).await,
            format_args!("#{fid}: beginning a PR read"),
        );
        let read = match begun {
            Some(Some(read)) => read,
            Some(None) => {
                // `list_findings` lists each finding once, so this pass
                // cannot reach a finding twice; refused if it ever does.
                let _ = store
                    .log_event(
                        "error",
                        &format!("sync #{fid}: PR already read in this pass"),
                        None,
                        Some(fid),
                    )
                    .await;
                summary.errors += 1;
                continue;
            }
            None => {
                summary.errors += 1;
                continue;
            }
        };
        let pr = match fg.view_pr_sync(&repo.url, pr_number, &cfg.review_bots) {
            Ok(pr) => pr,
            Err(e) => {
                let _ = store
                    .log_event(
                        "error",
                        &format!("sync #{fid}: PR view failed: {e}"),
                        None,
                        Some(fid),
                    )
                    .await;
                summary.errors += 1;
                continue;
            }
        };

        match pr.state {
            forge::PrState::Merged => {
                // The finding's move and the PR's record are one write: a
                // failed write leaves both for the next sync, and a
                // conflict leaves the finding to what moved it.
                if !sync_pr_end(store, fid, "ship", &read_pr, PrEnding::Merged, &mut summary).await
                {
                    continue;
                }
                let _ = store
                    .log_event("ship", &format!("#{fid} PR merged: {url}"), None, Some(fid))
                    .await;
                summary.merged += 1;
                continue;
            }
            forge::PrState::Closed => {
                // Not a rejection: a closure alone does not say whether the
                // finding was wrong, and suppression is permanent and silent.
                // Of the first 16 closed PRs only one was closed by a human.
                // The finding waits here until its harvest classifies why.
                // `closed` and `pr_state` CLOSED land together: sync_prs only
                // revisits pr_open findings, so a finding set closed while
                // pr_state still read OPEN would never be looked at again.
                let closed = PrEnding::Closed {
                    reason: "PR closed without merge; awaiting harvest",
                };
                if !sync_pr_end(store, fid, "verdict", &read_pr, closed, &mut summary).await {
                    continue;
                }
                let _ = store
                    .log_event(
                        "verdict",
                        &format!("#{fid} PR closed without merge: {url}"),
                        None,
                        Some(fid),
                    )
                    .await;
                summary.closed += 1;
                continue;
            }
            forge::PrState::Open => { /* fall through to attention logic below */ }
        }

        // -- Open PR: record what the forge shows; the attention it calls
        // for is settled from that and engage's last answer, in the same
        // write (`crate::attention`). Nothing here reads the row first, so
        // an engage answering meanwhile cannot be written over, and the
        // write lands only while the finding is still `pr_open`, so an
        // engage that withdrew the PR meanwhile is not undone either.
        let observed = ObservedOpenPr::from_view(read, &pr);
        // Bound first: the `format_args!` temporary is not `Send`, and the
        // arms below await.
        let recorded = log_write_failure(
            store.record_open_pr(fid, &observed).await,
            format_args!("#{fid}: recording synced PR state"),
        );
        let change = match recorded {
            Some(OpenPrRecorded::Recorded(change)) => change,
            Some(OpenPrRecorded::NotPrOpen { now }) => {
                // Something else decided the finding since it was listed,
                // and recorded the PR's end if it had one; that stands.
                let now = now.map_or_else(|| "gone".to_owned(), |s| format!("'{s}'"));
                let _ = store
                    .log_event(
                        "engage",
                        &format!("#{fid} PR #{pr_number} seen open, not recorded: it is {now} now"),
                        None,
                        Some(fid),
                    )
                    .await;
                continue;
            }
            Some(OpenPrRecorded::OtherPr) => {
                // The PR read ended and the finding shipped a new one while
                // the read was out; the new PR's next sync records that one.
                let _ = store
                    .log_event(
                        "engage",
                        &format!("#{fid} PR #{pr_number} seen open, not recorded: the finding has another PR now"),
                        None,
                        Some(fid),
                    )
                    .await;
                continue;
            }
            None => {
                summary.errors += 1;
                continue;
            }
        };

        if let Some(reasons) = change.raised() {
            let _ = store
                .log_event(
                    "engage",
                    &format!("#{fid} PR #{pr_number} needs attention: {reasons}"),
                    None,
                    Some(fid),
                )
                .await;
        }
        summary.synced += 1;
        if change.now.is_some() {
            summary.attention += 1;
        }
    }
    summary
}

/// The repo and PR an engage works on: its `pr_state` row, branch, and the
/// PR itself. Every way to lack one is logged and an error: sync records
/// them first. Reads only; the clone is changed later, by
/// [`fetch_pr_head`].
async fn engage_target(
    store: &Store,
    finding: &Finding,
) -> anyhow::Result<(Repo, crate::store::RecordedPrState, String, PrRef)> {
    let fid = finding.id;
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let Ok(Some(ps)) = store.get_pr_state(fid).await else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: no pr_state/head_ref -- sync first"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("no pr_state");
    };
    let head_ref = match &ps.head_ref {
        Some(hr) if !hr.is_empty() => hr.clone(),
        _ => {
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: no head_ref"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_state");
        }
    };
    let pr = engage_pr(store, finding, &repo).await?;
    check_row_is_of(store, fid, &ps, &pr).await?;
    if head_ref == repo.default_branch {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: refusing -- head_ref equals default branch {head_ref:?}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("head_ref equals default branch");
    }
    Ok((repo, ps, head_ref, pr))
}

/// Take the clone's permit, held from here until the tree is made, and
/// fetch the PR head into the clone for a cold attempt; the tree is added
/// from it once the job exists. Never on a resume: the chain continues in
/// its own tree, at the head it was created at.
///
/// Taken after every forge read of the engage, never before: the permit
/// keeps the repo's other changes (a sync, the sweep, a URL update) waiting
/// while it is held, and a PR view is several network round trips.
async fn fetch_pr_head<'l>(
    lane: &'l mut Lane,
    repo: &Repo,
    fid: i64,
    head_ref: &str,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<ClonePermit<'l>> {
    let store = lane.store().clone();
    let permit = lane.lock_clone(repo.id).await?;
    if resume.is_none() {
        let (rc, out) = fetch_held(&permit, &[head_ref]).await;
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: fetch {head_ref} failed: {tail}"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("fetch failed: {tail}");
        }
    }
    Ok(permit)
}

/// Refuse an engage whose `pr_state` row `ps` is of another PR than `pr`,
/// the finding's, or fill in the row's missing number from `pr`.
///
/// The row holds the head and problems sync last recorded for the
/// finding. Only an observation of the finding's own PR writes its
/// number, so a row naming another one is from before that rule; the next
/// sync of the finding's PR rewrites it, and until then its head is not
/// this PR's.
async fn check_row_is_of(
    store: &Store,
    fid: i64,
    ps: &crate::types::PrState,
    pr: &PrRef,
) -> anyhow::Result<()> {
    let num = pr.number();
    match ps.pr_number {
        Some(n) if n != num => {
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "engage #{fid}: pr_state is PR #{n}, the finding's PR is #{num} -- sync first"
                    ),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("pr_state of another PR");
        }
        Some(_) => {}
        None => {
            tracing::warn!(
                finding_id = fid,
                pr_number = num,
                "self-healed missing pr_number from pr_url"
            );
            log_write_failure(
                store.fill_pr_number(fid, num).await,
                format_args!("#{fid}: self-healed pr_number -> {num}"),
            );
        }
    }
    Ok(())
}

/// The PR an engage answers: the finding's `pr_url`, its number parsed
/// from it. Its answer is recorded against it, and lands only while that
/// is still the finding's PR (`AnswerRecorded::OtherPr`).
async fn engage_pr(store: &Store, finding: &Finding, repo: &Repo) -> anyhow::Result<PrRef> {
    let fid = finding.id;
    let Some(url) = finding.pr_url.as_deref().filter(|u| !u.is_empty()) else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: no pr_url"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("no pr_url");
    };
    let Some(pr) = PrRef::parse(&*forge::forge_for(repo.forge), url) else {
        let _ = store
            .log_event(
                "error",
                &format!("engage #{fid}: unparseable pr_url {url:?}"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("unparseable pr_url");
    };
    Ok(pr)
}

/// An engage job with its PR read and a tree at the PR head.
struct EngageStart {
    repo: Repo,
    ps: crate::store::RecordedPrState,
    head_ref: String,
    /// The PR answered: the finding's when this attempt started.
    pr_ref: PrRef,
    /// The PR as the forge shows it, for the playbook: `None` on a resume,
    /// which sends no playbook and so does not ask the forge.
    pr: Option<PrView>,
    job: i64,
    ws: OpenWorkspace,
    pinned: String,
    cap: Option<i64>,
    /// The newest feedback the chain's worker was shown: read from the PR
    /// by a cold attempt, inherited by a resume (which is only told to
    /// carry on). None for a chain started before this was recorded.
    seen_activity: Option<SeenActivity>,
}

/// The PR as the forge shows it, for a fresh engage's playbook. Only a
/// fresh engage reads it: a paused one continues without the forge (a
/// resume sends no playbook), where a view that failed while the network
/// is not back would count against it for nothing. A failed view is a
/// failed engage ([`engage_prefetch_failed`]), or the network's, which
/// ends the cycle denied (`Ok(Err(summary))`).
async fn engage_view(
    store: &Store,
    (cfg, backend): (&Config, &dyn Backend),
    finding: &Finding,
    repo: &Repo,
    pr_number: i64,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<Result<Option<PrView>, CycleSummary>> {
    if resume.is_some() {
        return Ok(Ok(None));
    }
    let fg = forge::forge_for(repo.forge);
    match fg.view_pr_engage(&repo.url, pr_number, &cfg.review_bots) {
        Ok(p) => Ok(Ok(Some(p))),
        Err(e) => {
            let fid = finding.id;
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: PR/MR view failed: {e}"),
                    None,
                    Some(fid),
                )
                .await;
            let failure = format!("PR/MR view failed: {e}");
            if let Some(denied) =
                engage_prefetch_failed(store, (cfg, backend), finding, &failure).await
            {
                return Ok(Err(denied));
            }
            anyhow::bail!("PR/MR view failed");
        }
    }
}

/// A step before the worker, `step`'s outcome, with a failure handed to
/// [`engage_prefetch_failed`]: the network's ends the cycle denied
/// (`Ok(Err(summary))`), anything else stays the error it was.
async fn engage_prefetch<T>(
    store: &Store,
    net: (&Config, &dyn Backend),
    finding: &Finding,
    step: anyhow::Result<T>,
) -> anyhow::Result<Result<T, CycleSummary>> {
    match step {
        Ok(value) => Ok(Ok(value)),
        Err(e) => match engage_prefetch_failed(store, net, finding, &e.to_string()).await {
            Some(denied) => Ok(Err(denied)),
            None => Err(e),
        },
    }
}

/// Everything before the worker runs: target, budget gate, PR view, fetch,
/// job row, tree. `Ok(Err(summary))` is a cycle that ends here.
async fn start_engage(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<Result<(EngageStart, RunningJob), CycleSummary>> {
    let (store, fid) = (scope.store(), finding.id);
    let target = engage_target(store, finding).await;
    let (repo, ps, head_ref, pr_ref) =
        match engage_prefetch(store, (cfg, backend), finding, target).await? {
            Ok(target) => target,
            Err(denied) => return Ok(Err(denied)),
        };

    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Engage.into(),
        finding.budget_override.is_some(),
        &format!("engage #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(Err(*d)),
    };

    let pr_number = pr_ref.number();
    let pr = match engage_view(store, (cfg, backend), finding, &repo, pr_number, resume).await? {
        Ok(pr) => pr,
        Err(denied) => return Ok(Err(denied)),
    };
    let fetched = fetch_pr_head(lane, &repo, fid, &head_ref, resume).await;
    let permit = match engage_prefetch(store, (cfg, backend), finding, fetched).await? {
        Ok(permit) => permit,
        Err(denied) => return Ok(Err(denied)),
    };

    let created = match scope
        .create_job(
            resume.map_or(
                JobStart::fresh_finding(FindingJobKind::Engage, fid),
                ResumePlan::start,
            ),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(FindingJobKind::Engage.into(), &repo.name, e).map(Err);
        }
    };
    let job = created.job.id();
    let (ws, pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, &repo),
        created,
        resume,
        TreeSpec::PrHead {
            head_ref: head_ref.clone(),
        },
        FindingJobKind::Engage.into(),
        &format!("engage #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => {
            return Ok(Err(count_unmade_engage_tree(store, fid, job, failed).await));
        }
    };
    let seen_activity = engage_seen_activity(store, job, pr.as_ref()).await;
    Ok(Ok((
        EngageStart {
            repo,
            ps,
            head_ref,
            pr_ref,
            pr,
            job,
            ws,
            pinned,
            cap,
            seen_activity,
        },
        running,
    )))
}

/// The newest feedback job `job`'s worker is shown. Only a fresh attempt
/// reads the PR (`pr`), and its playbook shows the worker exactly that
/// view; a resume inherits what its chain was shown.
async fn engage_seen_activity(
    store: &Store,
    job: i64,
    pr: Option<&PrView>,
) -> Option<SeenActivity> {
    let Some(pr) = pr else {
        return store.seen_activity(job).await.ok().flatten();
    };
    let seen = SeenActivity::of(pr);
    // Unrecorded, a resume of this chain falls back to the mark the
    // chain started with: a re-raised comment, never a hidden one.
    let _ = store.set_seen_activity(job, seen).await;
    Some(seen)
}

/// The worker asked to withdraw the PR (`WITHDRAW.md`): close it on the
/// forge and record the finding `closed`. Returns whether the closure is
/// complete enough to hand to the closed PR's harvest right away.
async fn withdraw_pr(
    store: &Store,
    net: (&Config, &dyn Backend),
    fid: i64,
    start: &EngageStart,
    reason: &str,
    summary: &mut CycleSummary,
) -> bool {
    let (repo, job, pr_number) = (&start.repo, start.job, start.pr_ref.number());
    let fg = forge::forge_for(repo.forge);
    let mut forge_closed = false;
    if fg.owner_repo(&repo.url).is_some() {
        if let Err(err) = fg.close_pr(&repo.url, pr_number, reason) {
            // close_pr posts the withdrawal reason and only then closes,
            // so a failure here means the PR is still OPEN on the forge.
            // Recording the verdict anyway would mark it closed locally
            // and set the finding Closed -- and sync_prs only revisits
            // pr_open findings, so nothing would ever reconcile it. Left
            // untouched, the finding stays in the engage tier: a failed
            // attempt the caller counts in the engage streak, which a
            // forge that keeps refusing runs into (WORK-MODEL.md §9.4).
            tracing::warn!(finding = fid, pr = pr_number, error = %err, "close_pr failed");
            if let Some(denied) = unpublished_offline(store, net, fid, start).await {
                *summary = denied;
                return false;
            }
            let fail = format!("PR close failed: {err}");
            // A paused attempt that withdrew still ended: its withdrawal
            // failed, and a failed attempt keeps no checkpoint (§6.3), or it
            // would be resumed into the same withdrawal.
            log_write_failure(
                store
                    .drop_checkpoint(
                        fid,
                        FindingJobKind::Engage,
                        job,
                        &format!(
                            "resume engage: checkpoint job {job} dropped, its withdrawal failed"
                        ),
                    )
                    .await,
                format_args!("#{fid}: dropping the withdrawn engage's checkpoint"),
            );
            let held = record_engage_failure(
                store,
                fid,
                Some(job),
                "withdraw failed",
                &fail,
                HoldReason::Stuck,
            )
            .await;
            summary.outcome = Some(if held { "held" } else { "withdraw-failed" }.into());
            summary.failure = Some(fail);
            return false;
        }
        forge_closed = true;
    }
    // Closed, not Rejected: 14 of the first 15 engage withdrawals were
    // "superseded/obsolete", i.e. the finding was right and its work
    // landed elsewhere. Suppressing those told every later scan to stop
    // reporting valid bugs. The harvest classifies the closure and also
    // owns the follow-ups, which is why this no longer ingests a
    // FOLLOW-UPS.json.
    let reason_short: String = reason.chars().take(500).collect();
    let closed_on_forge =
        set_withdrawn_closed(store, fid, job, &start.pr_ref, &reason_short).await && forge_closed;
    let first_line: String = reason
        .lines()
        .next()
        .map(|l| l.chars().take(120).collect())
        .unwrap_or_default();
    let _ = store
        .log_event(
            "verdict",
            &format!("#{fid} withdrawn by engage worker: {first_line}"),
            Some(job),
            Some(fid),
        )
        .await;
    summary.outcome = Some("withdrawn".into());
    closed_on_forge
}

/// Record a withdrawn PR's finding `closed` and its `pr_state` CLOSED, in
/// one write, only while `pr` is still the finding's; `true` when it was.
async fn set_withdrawn_closed(store: &Store, fid: i64, job: i64, pr: &PrRef, reason: &str) -> bool {
    let pr_number = pr.number();
    let now = match store.end_pr(fid, pr, PrEnding::Closed { reason }).await {
        Ok(PrEnded::Applied) => return true,
        // Something moved the finding off pr_open since the engage read it,
        // or it went on to another PR, and that decision owns the finding
        // now: no handoff, the harvest it would start is that decision's to
        // start.
        Ok(PrEnded::Conflict { now }) => {
            now.map_or_else(|| "gone".to_owned(), |s| format!("'{s}'"))
        }
        Ok(PrEnded::OtherPr) => "on another PR".to_owned(),
        Err(err) => {
            // Neither landed: the finding is still pr_open and pr_state
            // still OPEN, so no handoff. The harvest picks its playbook from
            // pr_state and takes pr_open for a human's verdict, so it would
            // review the closed PR as a merged one and mark it harvested.
            // The PR is closed on the forge and sync_prs revisits pr_open
            // findings: the next sync takes its Closed branch, records both,
            // and the cold harvest follows with the right playbook.
            tracing::warn!(finding = fid, pr = pr_number, error = %err, "closed verdict failed");
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "#{fid} PR #{pr_number} withdrawn but not recorded closed: {err} \
                         (the next PR sync records it)"
                    ),
                    Some(job),
                    Some(fid),
                )
                .await;
            return false;
        }
    };
    let _ = store
        .log_event(
            "verdict",
            &format!(
                "#{fid} PR #{pr_number} withdrawn, but the finding is {now} now: left as it is"
            ),
            Some(job),
            Some(fid),
        )
        .await;
    false
}

/// What an engage put on the forge: whether its commits were pushed, and
/// whether its `PR-REPLY.md` was posted, or why the publication is
/// incomplete.
///
/// `pushed` stands on its own: a push that landed moved the PR's head
/// whatever happens after it, so a reply that then fails must not hide it
/// (`record_engage` counts it either way).
struct Published {
    pushed: bool,
    replied: Result<bool, String>,
}

/// Push the worker's commits to the PR branch and post its `PR-REPLY.md`.
/// A worker that did not finish publishes nothing.
async fn push_and_reply(start: &EngageStart, state: JobState) -> Published {
    let mut pushed = false;
    let replied = push_then_reply(start, state, &mut pushed).await;
    Published { pushed, replied }
}

/// [`push_and_reply`]'s steps: `pushed` is set the moment the push lands,
/// so it holds even when a later step fails.
async fn push_then_reply(
    start: &EngageStart,
    state: JobState,
    pushed: &mut bool,
) -> Result<bool, String> {
    if state != JobState::Done {
        return Err(format!("worker {state}"));
    }
    let (repo, worktree, head_ref) = (&start.repo, &start.ws.tree, &start.head_ref);
    let fg = forge::forge_for(repo.forge);
    let wt = worktree.clone();
    let range = format!("origin/{head_ref}..HEAD");
    let (rc, commits) =
        tokio::task::spawn_blocking(move || Unheld::Commits { range: &range }.run(&wt, 30))
            .await
            .unwrap_or((127, String::new()));
    if rc == 0 && !commits.trim().is_empty() {
        let push_url = fg.ssh_url(&repo.url);
        let wt = worktree.clone();
        let refspec = format!("HEAD:{head_ref}");
        // Leased to the PR head the chain's tree was created at, with the
        // expected commit spelled out: the worker may rewrite that history,
        // but a commit pushed to the branch since -- while this attempt ran,
        // or while a resume of it waited, suspended, for the budget -- is
        // not in the tree, and forcing over it would delete it from the PR.
        // A refused push fails the attempt with the attention left
        // unaddressed, so a cold engage takes it up on the branch as it now
        // is. The explicit `<ref>:<sha>` form reads no remote-tracking ref,
        // so it works against the raw URL; `--force` must not be added, as
        // it overrides the lease.
        let lease = format!("{head_ref}:{}", start.pinned);
        let (prc, pout) = tokio::task::spawn_blocking(move || {
            Unheld::Push {
                force: Force::Lease(&lease),
                url: &push_url,
                refspec: &refspec,
            }
            .run(&wt, 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if prc != 0 {
            let tail = crate::util::tail(&pout, 300);
            return Err(format!("push failed: {tail}"));
        }
        *pushed = true;
    }
    let reply = worktree.join("PR-REPLY.md");
    let mut replied = false;
    if reply.exists() {
        let body = std::fs::read_to_string(&reply).unwrap_or_default();
        if let Err(e) = fg.comment_pr(&repo.url, start.pr_ref.number(), &body) {
            return Err(format!(
                "PR comment failed: {}",
                crate::util::tail(&e.to_string(), 300)
            ));
        }
        replied = true;
    }
    Ok(replied)
}

/// Record a finished engage: what it published, or why it is incomplete.
/// Either way a one-shot budget override is spent.
async fn record_engage(
    store: &Store,
    (net, finding): ((&Config, &dyn Backend), &Finding),
    start: &EngageStart,
    recorded: Option<RecordedJob>,
    published: Published,
    stdout_tail: &str,
    summary: &mut CycleSummary,
) {
    let (fid, job, pr_number) = (finding.id, start.job, start.pr_ref.number());
    // An outcome that could not be recorded reads as `failed`.
    let state = recorded.map_or(JobState::Failed, |r| r.state());
    let tail = crate::util::tail(stdout_tail, 300);
    let Published { pushed, replied } = published;
    if pushed && replied.is_err() {
        // The push landed and the rest did not: nothing is answered, but
        // the PR's head moved, and an observation of the head it replaced
        // must not count against the new one.
        record_engage_answer(store, fid, start, EngageAnswer::push_only(&start.pr_ref)).await;
    }
    match replied {
        // A pause, not a failure: the tree and transcript are kept, and
        // the engage tier continues it while the PR is open (§6.1).
        Err(fail) if state == JobState::Suspended => {
            let _ = store
                .log_event(
                    "engage",
                    &format!(
                        "#{fid} suspended ({fail}); worktree kept at {} for the resume. \
                         tail: {tail}",
                        start.ws.tree.display()
                    ),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("suspended".into());
            summary.worktree = Some(start.ws.tree.to_string_lossy().into_owned());
        }
        // A failed engage: counted, so a PR whose engage keeps failing the
        // same way is held at the limit where it used to be picked again
        // every cycle, the highest finding tier with no counter at all.
        // A finished worker whose push or reply could not reach the forge
        // while it was unreachable: the network's failure (#95).
        Err(_)
            if state == JobState::Done
                && let Some(denied) = unpublished_offline(store, net, fid, start).await =>
        {
            // Nothing concluded: a one-shot override is kept for the
            // attempt that publishes.
            *summary = denied;
            return;
        }
        Err(fail) => {
            // Only a recorded `done` is revised: an attempt that did not
            // finish has nothing published to fail.
            if let Some(recorded) = recorded.filter(|r| r.state() == JobState::Done) {
                log_write_failure(
                    store.fail_job(&recorded, fail.as_str()).await,
                    format_args!("job {job}: failing ({fail})"),
                );
            }
            let detail = format!("{fail}\n\n{tail}");
            let held = record_engage_failure(
                store,
                fid,
                Some(job),
                engage_failure_key(&fail),
                &detail,
                HoldReason::Stuck,
            )
            .await;
            summary.outcome = Some(if held { "held" } else { "retry" }.into());
            summary.failure = Some(fail);
        }
        Ok(replied) => {
            let answer = if pushed {
                EngageAnswer::pushed(&start.pr_ref, start.seen_activity)
            } else {
                EngageAnswer::without_push(&start.pr_ref, start.seen_activity, &start.ps)
            };
            record_engage_answer(store, fid, start, answer).await;
            let did: Vec<&str> = [("pushed", pushed), ("replied", replied)]
                .iter()
                .filter(|(_, on)| *on)
                .map(|(b, _)| *b)
                .collect();
            let did_str = if did.is_empty() {
                "no-op".to_owned()
            } else {
                did.join(", ")
            };
            log_write_failure(
                store.clear_work_streak(fid, FindingJobKind::Engage).await,
                format_args!("#{fid}: clearing the engage streak"),
            );
            let _ = store
                .log_event(
                    "engage",
                    &format!("#{fid} PR #{pr_number} engaged ({did_str})"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("engaged".into());
        }
    }
    if finding.budget_override == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
}

/// Record what a published engage answered (`crate::attention`): the
/// feedback its worker saw, not "now", so feedback that arrived while it
/// ran stays new (its own reply is no activity, `OWN_MARK`); and, for a
/// reply without a push, the problems it was started on as declined,
/// which settle voids if they have moved since. Not recorded when the PR
/// ended while the worker ran, which is logged.
async fn record_engage_answer(
    store: &Store,
    fid: i64,
    start: &EngageStart,
    answer: EngageAnswer<'_>,
) {
    // Bound first: the `format_args!` temporary is not `Send`, and the
    // arms below await.
    let recorded = log_write_failure(
        store.mark_pr_engaged(fid, &answer).await,
        format_args!("#{fid}: marking PR engaged"),
    );
    let why = match recorded {
        Some(AnswerRecorded::NotPrOpen { now }) => format!(
            "it is {} now",
            now.map_or_else(|| "gone".to_owned(), |s| format!("'{s}'"))
        ),
        Some(AnswerRecorded::OtherPr) => "the finding has another PR now".to_owned(),
        Some(AnswerRecorded::Recorded(_)) | None => return,
    };
    // Whatever ended the PR recorded that; the answer was to a PR that
    // needs none.
    let _ = store
        .log_event(
            "engage",
            &format!(
                "#{fid} PR #{} answer not recorded: {why}",
                start.pr_ref.number()
            ),
            Some(start.job),
            Some(fid),
        )
        .await;
}

/// The streak key of a failed engage: what failed, without the forge's or
/// git's text, which differs on every attempt and would reset the streak
/// each time (`push failed: <tail>` counts as `push failed`).
fn engage_failure_key(fail: &str) -> &str {
    fail.split(':').next().unwrap_or(fail).trim()
}

/// Count a failed engage in the finding's streak, `key` naming the failure
/// and `detail` describing it (WORK-MODEL.md §7). The
/// [`MAX_CONSECUTIVE_SAME_FAILURE`]th identical failure holds the engage for
/// the operator as `at_limit` ([`Store::hold_work`]), with `job` as its
/// checkpoint if a worker ran in it. The PR's attention flag is not
/// touched: the engage tier skips held work, and new PR activity does not
/// release the hold, only the operator does (§9.4).
///
/// Every failed engage is counted here — before its worker runs
/// ([`engage_prefetch_failed`]), by its worker or its publishing
/// ([`record_engage`]), a refused withdrawal ([`run_engage`]), or a
/// given-up resume chain ([`count_given_up_chain`]). True when the engage
/// is now held. A count or a hold that could not be recorded is reported
/// as such and ends nothing.
async fn record_engage_failure(
    store: &Store,
    fid: i64,
    job: Option<i64>,
    key: &str,
    detail: &str,
    at_limit: HoldReason,
) -> bool {
    let log = |message: String| async move {
        let _ = store.log_event("engage", &message, job, Some(fid)).await;
    };
    let streak = match store
        .record_work_failure(fid, FindingJobKind::Engage, key)
        .await
    {
        Ok(streak) => streak,
        Err(e) => {
            log(format!("#{fid} incomplete ({detail}), not counted: {e}")).await;
            return false;
        }
    };
    if streak < MAX_CONSECUTIVE_SAME_FAILURE {
        log(format!("#{fid} incomplete ({detail}), will retry")).await;
        return false;
    }
    match store
        .hold_work(fid, FindingJobKind::Engage, at_limit, detail, job)
        .await
    {
        Ok(true) => {
            log(format!(
                "#{fid} held after {streak} identical failures ({detail})"
            ))
            .await;
            true
        }
        Ok(false) => {
            log(format!(
                "#{fid} {streak} identical failures ({detail}), but its PR is no longer open; \
                 nothing held"
            ))
            .await;
            false
        }
        Err(e) => {
            log(format!(
                "#{fid} {streak} identical failures ({detail}), but holding it failed: {e}; \
                 will retry"
            ))
            .await;
            false
        }
    }
}

/// A finished engage that could not publish (push, reply, the withdrawal's
/// close) while a host it needs is unreachable (#95): the network's
/// failure, not the engage's. Nothing is counted, the job is left as it
/// ended, and the attempt becomes the checkpoint, so its tree (commits,
/// `PR-REPLY.md`, `WITHDRAW.md`) is kept and the next cycle continues it to
/// publish. `None` when every host is reachable: a real publish failure.
async fn unpublished_offline(
    store: &Store,
    (cfg, backend): (&Config, &dyn Backend),
    fid: i64,
    start: &EngageStart,
) -> Option<CycleSummary> {
    let host = unreachable_now(cfg, &start.repo.url, backend).await?;
    let job = start.job;
    log_write_failure(
        store.keep_attempt_as_checkpoint(job).await,
        format_args!("job {job}: keeping the unpublished attempt as the checkpoint"),
    );
    Some(
        offline_denial(
            store,
            FindingJobKind::Engage.into(),
            Some(fid),
            Some(job),
            host,
        )
        .await,
    )
}

/// Count an engage that failed before its worker ran: its target could
/// not be read (no `pr_state`, no head branch, a head fetch that failed)
/// or the PR would not load. The caller has logged the `error` already;
/// this counts it ([`record_engage_failure`]) under the failure's name
/// without its text. Unless a host the engage needs is unreachable now
/// ([`unreachable_now`]): then the failure is the network's, nothing is
/// counted, and the returned summary ends the cycle denied for the host.
async fn engage_prefetch_failed(
    store: &Store,
    (cfg, backend): (&Config, &dyn Backend),
    finding: &Finding,
    failure: &str,
) -> Option<CycleSummary> {
    let fid = finding.id;
    // A repo that is gone has no forge to probe: its workers' hosts only.
    let repo_url = store
        .get_repo_by_id(finding.repo_id)
        .await
        .ok()
        .flatten()
        .map_or_else(String::new, |r| r.url);
    if let Some(host) = unreachable_now(cfg, &repo_url, backend).await {
        return Some(
            offline_denial(store, FindingJobKind::Engage.into(), Some(fid), None, host).await,
        );
    }
    record_engage_failure(
        store,
        fid,
        None,
        engage_failure_key(failure),
        failure,
        HoldReason::Stuck,
    )
    .await;
    None
}

/// The engage worker's prompt: the playbook for a fresh engage, the short
/// resume prompt for a continued one (whose transcript holds the playbook,
/// and which read no PR to build one from).
fn engage_prompt(
    cfg: &Config,
    finding: &Finding,
    start: &EngageStart,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<String> {
    let Some(pr) = &start.pr else {
        return Ok(resume.map_or_else(
            || RESUME_PROMPT.to_owned(),
            |plan| resume_prompt(FindingJobKind::Engage.into(), plan),
        ));
    };
    let repo_notes = Store::repo_notes(&cfg.work_root, start.repo.id);
    playbooks::build_engage_prompt(
        &cfg.root,
        finding,
        &start.ws.tree,
        &start.head_ref,
        &start.repo,
        pr,
        &start.ps,
        &repo_notes,
    )
}

/// Run engage job (`scheduler.run_engage`): start it ([`start_engage`]),
/// run the worker, then withdraw the PR ([`withdraw_pr`]) or publish its
/// work ([`push_and_reply`]) and record that ([`record_engage`]).
pub async fn run_engage(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let fid = finding.id;
    remove_stale_report(resume)?;
    let (start, running) = match start_engage(scope, lane, cfg, finding, backend, resume).await? {
        Ok(started) => started,
        Err(ended) => return Ok(ended),
    };
    let prompt = engage_prompt(cfg, finding, &start, resume)?;
    let model = cfg.model_for("fix");
    let rr = match backend
        .run(
            &start.ws,
            &prompt,
            start.cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            let ran = (WorkOf::Finding(FindingJobKind::Engage, fid), running);
            let denied = runner_failed(store, (cfg, backend, &start.repo.url), ran, &e).await;
            close_workspace(lane, start.ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Finding(FindingJobKind::Engage, fid), running);
    let running = match unmetered_offline(store, cfg, backend, &start.repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            close_workspace(lane, start.ws).await;
            return Ok(denied);
        }
    };
    let recorded = log_write_failure(
        record_job(store, running, &rr, model, resume).await,
        format_args!("job {}: recording outcome", start.job),
    );
    let state = recorded.map_or(JobState::Failed, |r| r.state());
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Engage.into()),
        finding_id: Some(fid),
        job_id: Some(start.job),
        state: Some(state),
        pr_number: Some(start.pr_ref.number()),
        tokens_new: Some(rr.tokens_new),
        ..Default::default()
    };
    // Set only once the forge has accepted the close AND pr_state records
    // it AND the finding is `closed`: a handoff into the closed PR's
    // harvest for a PR that is still open would review a closure that
    // never happened, one whose pr_state still reads OPEN would be run as
    // a merged PR's harvest, and one whose finding is still `pr_open`
    // would have its classification kept as if a human had set that
    // status while the PR is marked harvested all the same.
    let withdraw = start.ws.tree.join("WITHDRAW.md");
    let closed_on_forge = if withdraw.exists() {
        let reason = std::fs::read_to_string(&withdraw).unwrap_or_default();
        // The withdrawal is this attempt's end, whatever the forge does
        // with it, so it spends a `once` override here, before the handoff
        // reloads the finding: left set, the closed PR's harvest would run
        // prioritized and jump the queue on it.
        if finding.budget_override == Some(BudgetOverride::Once) {
            log_write_failure(
                store.set_budget_override(fid, None).await,
                format_args!("#{fid}: clearing one-shot budget override"),
            );
        }
        withdraw_pr(store, (cfg, backend), fid, &start, &reason, &mut summary).await
    } else if let Some(report) = blocked_report(&start.ws.tree)? {
        if let Err(e) = engage_blocked(lane.store(), fid, &start, &report, &mut summary).await {
            close_workspace(lane, start.ws).await;
            return Err(e);
        }
        false
    } else {
        let published = push_and_reply(&start, state).await;
        record_engage(
            store,
            ((cfg, backend), finding),
            &start,
            recorded,
            published,
            &rr.stdout_tail,
            &mut summary,
        )
        .await;
        false
    };
    if closed_on_forge {
        continue_into_harvest(
            scope,
            lane,
            cfg,
            backend,
            &start.repo,
            fid,
            start.job,
            &start.ws,
            &start.pinned,
            rr.session_file.as_deref(),
        )
        .await;
    }
    close_workspace(lane, start.ws).await;
    Ok(summary)
}

/// An engage whose feedback needs a decision only a maintainer can make
/// wrote `BLOCKED.md`: it is held on the report ([`hold_on_report`]), and
/// nothing is pushed or posted. The attention stays unaddressed, for the
/// continue to take up.
async fn engage_blocked(
    store: &Store,
    fid: i64,
    start: &EngageStart,
    report: &str,
    summary: &mut CycleSummary,
) -> anyhow::Result<()> {
    let held = hold_on_report(
        store,
        FindingJobKind::Engage,
        fid,
        start.job,
        &start.ws,
        report,
    )
    .await;
    if !held? {
        anyhow::bail!("engage #{fid}: BLOCKED.md, but the PR is no longer open");
    }
    summary.outcome = Some("held".into());
    Ok(())
}

/// Carry a withdrawing engage straight on into its closed PR's harvest:
/// the next attempt of the engage's chain, in the engage's tree and
/// session, rather than a cold harvest in a fresh tree some cycles later.
///
/// The withdrawing worker has just read the PR, its discussion and the
/// code, and decided why the PR should close; the cold harvest would pay
/// a session floor to rediscover all of that. So the harvest is created
/// `resumed_from` the engage (inheriting the chain's workspace and pinned
/// commit) and handed the engage's transcript, and it reserves what a
/// resume reserves — the transcript's context plus the larger of the
/// harvest's typical cost and [`min_useful`] of that context — through
/// its own budget gate.
///
/// Everything here is best effort, and a failure leaves exactly what the
/// withdrawal left: the finding `closed`, awaiting the harvest tier's cold
/// review. That is the whole fallback — no retry. The caller releases the
/// tree afterwards, as it always does.
async fn continue_into_harvest(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    repo: &Repo,
    fid: i64,
    engage_job: i64,
    ws: &Workspace,
    pinned: &str,
    session_file: Option<&str>,
) {
    let store = scope.store();
    let Some(session_file) = session_file.map(PathBuf::from) else {
        let _ = store
            .log_event(
                "harvest",
                &format!(
                    "#{fid} engage job {engage_job} left no transcript to continue; \
                     it will be harvested cold"
                ),
                Some(engage_job),
                Some(fid),
            )
            .await;
        return;
    };
    let kind: JobKind = FindingJobKind::Harvest.into();
    let z = anticipated_tokens(store, cfg, repo.id, kind)
        .await
        .unwrap_or(0);
    // Same stand-in as `resume_plan`: an unreadable transcript leaves the
    // first call's cost unknown, and the per-kind typical is the only
    // other estimate there is.
    let ctx = crate::backends::omp_scavenge::harness::ctx_at_suspension(&session_file).unwrap_or(z);
    // Nothing of this chain was harvest work yet, so nothing is spent
    // against the harvest's typical cost.
    let chain_spent = 0;
    let plan = ResumePlan {
        kind,
        repo_id: repo.id,
        repo: repo.name.clone(),
        finding_id: Some(fid),
        predecessor_id: engage_job,
        origin_job_id: ws.origin_id,
        session_file,
        workspace: ws.clone(),
        pinned_sha: pinned.to_owned(),
        anticipated: resume_reservation(ctx, z, chain_spent),
        ctx,
        typical: z,
        chain_spent,
        handoff: true,
        after_hold: None,
    };
    let Ok(Some(finding)) = store.get_finding(fid).await else {
        return;
    };
    // The harvest reads its outputs from this tree, which the engage
    // worker wrote in first. Engage no longer offers follow-ups, but a
    // leftover file would be filed as if the harvest had verified it.
    for stale in ["FOLLOW-UPS.json", "CLOSE-REASON.json"] {
        let _ = std::fs::remove_file(ws.tree.join(stale));
    }
    let _ = store
        .log_event(
            "harvest",
            &format!(
                "#{fid} continuing engage job {engage_job} into the closed PR's harvest: \
                 reserving {} tok (ctx {ctx} + max({z} - {chain_spent}, {}))",
                plan.anticipated,
                min_useful(ctx)
            ),
            Some(engage_job),
            Some(fid),
        )
        .await;
    if let Err(e) = run_harvest(scope, lane, cfg, &finding, backend, Some(&plan)).await {
        let _ = store
            .log_event(
                "error",
                &format!(
                    "harvest #{fid}: continuing the withdrawal failed: {e}; \
                     it will be harvested cold"
                ),
                Some(engage_job),
                Some(fid),
            )
            .await;
    }
}

/// Record a harvest that failed before its job row exists. Nothing else
/// records such an attempt: left alone, a PR whose default branch never
/// fetches or whose view or diff never loads stays unharvested, and as the
/// oldest pending one it is picked again on every cycle, starving the tier
/// and every tier below it. `key` leaves out the error text so repeats of
/// one outage count as identical. A failure while a host the harvest needs
/// is unreachable is the network's, and ends the cycle denied instead
/// ([`unreachable_now`]): the returned summary.
///
/// Not for a handoff: it is an extra chance, not one of the harvest's own
/// attempts, and counting it would cost the cold harvest, which still
/// runs, part of its budget of tries. It is logged, and the finding stays
/// `closed` and pending.
async fn harvest_prefetch_failed(
    store: &Store,
    (cfg, backend, repo_url): (&Config, &dyn Backend, &str),
    fid: i64,
    handoff: bool,
    key: &str,
    what: &str,
    e: &impl std::fmt::Display,
) -> Option<CycleSummary> {
    if handoff {
        let _ = store
            .log_event(
                "error",
                &format!(
                    "harvest #{fid}: {what} failed: {e}; counted no attempt, \
                     it will be harvested cold"
                ),
                None,
                Some(fid),
            )
            .await;
        return None;
    }
    if let Some(host) = unreachable_now(cfg, repo_url, backend).await {
        return Some(
            offline_denial(store, FindingJobKind::Harvest.into(), Some(fid), None, host).await,
        );
    }
    let detail = format!("{what} failed: {e}");
    record_harvest_failure(store, fid, None, key, &detail, HoldReason::Stuck).await;
    None
}

/// Count a failed harvest in the finding's streak, `key` naming the
/// failure and `detail` describing it. The
/// [`MAX_CONSECUTIVE_SAME_FAILURE`]th identical failure holds the harvest
/// for the operator as `at_limit` ([`Store::hold_work`]) — `stuck`, or
/// `gave_up` when the give-up ceiling's failure completed the streak —
/// with `job` as its checkpoint if a worker ran in it. The PR stays
/// unharvested and the finding keeps its status; the harvest tier skips
/// it until a release: `continue`, `restart`, `classify` for a closed PR,
/// `mark_harvested` for a merged one. Stamping it harvested instead, as
/// before the work model, left a merged PR's give-up with no way back.
///
/// Every failed harvest is counted here — before its job exists
/// ([`harvest_prefetch_failed`]), without a tree or by its worker
/// ([`run_harvest`]), or as a given-up resume chain
/// ([`count_given_up_chain`]) — so they cannot drift apart on what a stuck
/// harvest becomes.
///
/// True when the harvest is now held. A count or a hold that could not be
/// recorded is reported as such and ends nothing.
async fn record_harvest_failure(
    store: &Store,
    fid: i64,
    job: Option<i64>,
    key: &str,
    detail: &str,
    at_limit: HoldReason,
) -> bool {
    let log = |message: String| async move {
        let _ = store.log_event("error", &message, job, Some(fid)).await;
    };
    let streak = match store
        .record_work_failure(fid, FindingJobKind::Harvest, key)
        .await
    {
        Ok(streak) => streak,
        Err(e) => {
            log(format!(
                "harvest #{fid}: {detail}, will retry (not counted: {e})"
            ))
            .await;
            return false;
        }
    };
    if streak < MAX_CONSECUTIVE_SAME_FAILURE {
        log(format!("harvest #{fid}: {detail}, will retry")).await;
        return false;
    }
    match store
        .hold_work(fid, FindingJobKind::Harvest, at_limit, detail, job)
        .await
    {
        Ok(true) => {
            log(format!(
                "harvest #{fid}: held after {streak} identical failures ({detail})"
            ))
            .await;
            true
        }
        Ok(false) => {
            log(format!(
                "harvest #{fid}: {streak} identical failures ({detail}), but it no longer \
                 awaits its harvest; nothing held"
            ))
            .await;
            false
        }
        Err(e) => {
            log(format!(
                "harvest #{fid}: {streak} identical failures ({detail}), but holding it \
                 failed: {e}; will retry"
            ))
            .await;
            false
        }
    }
}

/// A harvest whose worker could not classify the PR, or review it, without
/// a human wrote `BLOCKED.md`: the harvest is held on the report
/// ([`hold_on_report`]), the PR stays unharvested, and its follow-ups wait
/// for the attempt that concludes, as a failed harvest's do. A one-shot
/// budget override is kept for the `continue`, as for every kind: the hold
/// concluded nothing.
async fn harvest_blocked(
    lane: &mut Lane,
    fid: i64,
    (job, state): (i64, JobState),
    pr_number: i64,
    ws: OpenWorkspace,
    report: &str,
) -> anyhow::Result<CycleSummary> {
    let store = lane.store();
    let held = hold_on_report(store, FindingJobKind::Harvest, fid, job, &ws, report).await;
    close_workspace(lane, ws).await;
    if !held? {
        anyhow::bail!("harvest #{fid}: BLOCKED.md, but the PR is no longer awaiting harvest");
    }
    Ok(CycleSummary {
        kind: Some(FindingJobKind::Harvest.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        pr_number: Some(pr_number),
        outcome: Some("held".into()),
        ..Default::default()
    })
}

/// Run harvest job (`scheduler.run_harvest`).
///
/// One job kind for both ends of a PR's life. A merged PR is reviewed for
/// follow-up work (`harvest.md`); a PR closed without merging is also
/// classified — why it closed decides the finding's status — and reviewed
/// for what it left open (`harvest-closed.md`). Which one runs is the PR's
/// own state in `pr_state`, the record that says how the PR ended; the
/// finding's status says what was decided about the finding since.
#[allow(
    clippy::too_many_lines,
    reason = "linear job pipeline: sync, gate budget, run the worker, \
              ingest follow-up findings"
)]
pub async fn run_harvest(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let fid = finding.id;
    let Ok(Some(repo)) = store.get_repo_by_id(finding.repo_id).await else {
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: repo {} missing", finding.repo_id),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("repo missing");
    };
    let Ok(Some(ps)) = store.get_pr_state(fid).await else {
        let _ = store
            .log_event(
                "error",
                &format!("harvest #{fid}: no pr_state/pr_number -- sync first"),
                None,
                Some(fid),
            )
            .await;
        anyhow::bail!("no pr_state");
    };
    let pr_number = if let Some(n) = ps.pr_number {
        n
    } else {
        // Self-heal: pr_number missing in pr_state but pr_url exists on the finding.
        let fg = forge::forge_for(repo.forge);
        if let Some((_slug, num)) = finding.pr_url.as_deref().and_then(|u| fg.parse_pr_url(u)) {
            tracing::warn!(
                finding_id = fid,
                pr_number = num,
                "self-healed missing pr_number from pr_url"
            );
            log_write_failure(
                store.fill_pr_number(fid, num).await,
                format_args!("#{fid}: self-healed pr_number -> {num}"),
            );
            num
        } else {
            let _ = store
                .log_event(
                    "error",
                    &format!("harvest #{fid}: no pr_number and no parseable pr_url"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_number");
        }
    };

    remove_stale_report(resume)?;
    // A handoff starts a new job, so it gets what a cold one gets from
    // here on: a fresh default branch to compare against, the diff and
    // the full playbook. Only its tree and transcript are inherited.
    let fresh = resume.is_none_or(|p| p.handoff);
    let handoff = resume.is_some_and(|p| p.handoff);
    let override_mode = finding.budget_override;
    let (cap, anticipated) = match budget_gate(
        backend,
        store,
        cfg,
        repo.id,
        FindingJobKind::Harvest.into(),
        override_mode.is_some(),
        &format!("harvest #{fid}"),
        Some(fid),
        resume,
    )
    .await?
    {
        BudgetDecision::Approved { cap, anticipated } => (cap, anticipated),
        BudgetDecision::Denied(d) => return Ok(*d),
    };

    let fg = forge::forge_for(repo.forge);
    // The view feeds only the playbook, which a resume does not send: a
    // paused harvest continues without the forge, where a failed view
    // would count against it for nothing.
    let pr = if fresh {
        match fg.view_pr_engage(&repo.url, pr_number, &cfg.review_bots) {
            Ok(p) => Some(p),
            Err(e) => {
                let net = (cfg, backend, repo.url.as_str());
                if let Some(denied) = harvest_prefetch_failed(
                    store,
                    net,
                    fid,
                    handoff,
                    "pr view failed",
                    "PR/MR view",
                    &e,
                )
                .await
                {
                    return Ok(denied);
                }
                anyhow::bail!("PR/MR view failed");
            }
        }
    } else {
        None
    };
    let closed = ps.state.as_deref() == Some("CLOSED");
    // A closed PR's changes are on no branch the tree can be made from, so
    // the worker gets them as a diff, fetched the way the view is. A resume
    // needs neither: its transcript already holds the prompt.
    let pr_diff = if closed && fresh {
        match fg.pr_diff(&repo.url, pr_number) {
            Ok(d) => d,
            Err(e) => {
                let net = (cfg, backend, repo.url.as_str());
                if let Some(denied) = harvest_prefetch_failed(
                    store,
                    net,
                    fid,
                    handoff,
                    "pr diff failed",
                    "PR/MR diff",
                    &e,
                )
                .await
                {
                    return Ok(denied);
                }
                anyhow::bail!("PR/MR diff failed");
            }
        }
    } else {
        String::new()
    };

    // The clone's permit only now, after every forge read: it keeps the
    // repo's other changes (a sync, the sweep, a URL update) waiting while
    // it is held, and a PR view and diff are several network round trips.
    let permit = lane.lock_clone(repo.id).await?;
    // Fetch the default branch into the clone for a fresh attempt; a cold
    // tree is added at it once the job exists, and a handoff's worker
    // reads it as `origin/<default>`. Never on a resume: the chain
    // continues in its own tree.
    if fresh {
        let db = repo.default_branch.as_str();
        let (rc, out) = fetch_held(&permit, &[db]).await;
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let what = format!("fetch {}", repo.default_branch);
            // Let go before the probe (see `run_hunt`).
            drop(permit);
            let net = (cfg, backend, repo.url.as_str());
            if let Some(denied) =
                harvest_prefetch_failed(store, net, fid, handoff, "fetch failed", &what, &tail)
                    .await
            {
                return Ok(denied);
            }
            anyhow::bail!("fetch failed: {tail}");
        }
    }

    let created = match scope
        .create_job(
            resume.map_or(
                JobStart::fresh_finding(FindingJobKind::Harvest, fid),
                ResumePlan::start,
            ),
            cap,
            Some(anticipated),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(FindingJobKind::Harvest.into(), &repo.name, e);
        }
    };
    let job = created.job.id();
    let (ws, pinned, running) = match open_workspace(
        store,
        cfg,
        (permit, &repo),
        created,
        resume,
        TreeSpec::Detached {
            at: format!("origin/{}", repo.default_branch),
        },
        FindingJobKind::Harvest.into(),
        &format!("harvest #{fid}"),
        Some(fid),
    )
    .await?
    {
        Ok(opened) => opened,
        Err(mut failed) => {
            // A failed harvest under the fixed [`TREE_NOT_MADE`], as a fix's
            // is ([`count_unmade_tree`]). Uncounted, the PR stays pending
            // and the harvest tier takes it again every cycle. Only a cold
            // harvest makes a tree, so this is never a handoff. No worker
            // ran, so a one-shot override is not spent.
            let note = failed.failure.clone().unwrap_or_default();
            let held = record_harvest_failure(
                store,
                fid,
                Some(job),
                TREE_NOT_MADE,
                &note,
                HoldReason::Stuck,
            )
            .await;
            failed.outcome = Some(if held { "held" } else { "retry" }.into());
            return Ok(failed);
        }
    };
    let worktree = ws.tree.clone();
    let repo_notes = Store::repo_notes(&cfg.work_root, repo.id);
    let prompt = match &pr {
        // Only a resume goes without the view: its transcript holds the
        // playbook, so it gets the short prompt.
        None => resume.map_or_else(
            || RESUME_PROMPT.to_owned(),
            |plan| resume_prompt(FindingJobKind::Harvest.into(), plan),
        ),
        Some(pr) if closed => playbooks::build_harvest_closed_prompt(
            &cfg.root,
            finding,
            &worktree,
            &repo,
            pr,
            pr_number,
            &pr_diff,
            if resume.is_some() {
                playbooks::ClosedTree::PrHead
            } else {
                playbooks::ClosedTree::DefaultBranch
            },
            &repo_notes,
        )?,
        Some(pr) => playbooks::build_harvest_prompt(
            &cfg.root,
            finding,
            &worktree,
            &repo.default_branch,
            &repo,
            pr,
            pr_number,
            &repo_notes,
        )?,
    };
    let model = cfg.model_for("fix");
    let rr = match backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.fix_max_wall_s,
            JobClass::Fix,
            resume.map(|r| r.session_file.as_path()),
        )
        .await
    {
        Ok(rr) => rr,
        Err(e) => {
            // A failed handoff is an extra chance, not an attempt of the
            // harvest's own: its job is closed, nothing is counted.
            let denied = if handoff {
                log_write_failure(
                    running
                        .fail_unstarted(&format!("{RUNNER_ERROR}: {e}"))
                        .await,
                    format_args!("job {job}: failing unstarted"),
                );
                None
            } else {
                let ran = (WorkOf::Finding(FindingJobKind::Harvest, fid), running);
                runner_failed(store, (cfg, backend, &repo.url), ran, &e).await
            };
            close_workspace(lane, ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (WorkOf::Finding(FindingJobKind::Harvest, fid), running);
    let running = match unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        ControlFlow::Continue(running) => running,
        ControlFlow::Break(denied) => {
            close_workspace(lane, ws).await;
            return Ok(denied);
        }
    };
    let state = log_write_failure(
        record_job(store, running, &rr, model, resume)
            .await
            .map(|r| r.state()),
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);

    if let Some(report) = blocked_report(&worktree)? {
        return harvest_blocked(lane, fid, (job, state), pr_number, ws, &report).await;
    }
    // A closed PR's review must say why it closed. Without that the
    // finding would sit `closed` for good, so a missing or unusable
    // CLOSE-REASON.json is a failed harvest like a failed worker, and runs
    // into the same streak limit. A suspension is no failure: see below.
    let closure = closed.then(|| read_close_reason(&worktree));
    let failure = match state {
        JobState::Suspended => None,
        JobState::Done => match &closure {
            Some(Err(why)) => Some(why.clone()),
            _ => None,
        },
        _ => Some(CloseFailure {
            streak_key: format!("worker {state}"),
            detail: format!("worker {state}"),
        }),
    };
    // A failed handoff is no attempt of the harvest's own: its streak is
    // left alone (see below).
    let held = match &failure {
        Some(failure) if !handoff => Some(
            record_harvest_failure(
                store,
                fid,
                Some(job),
                &failure.streak_key,
                &failure.detail,
                HoldReason::Stuck,
            )
            .await,
        ),
        _ => None,
    };
    // An attempt that will be continued or tried again files no
    // follow-ups. A failed handoff is reviewed again cold, and any other
    // failed harvest is retried, or held until the operator continues or
    // restarts it; each of those is a later review, and its worker files
    // the same open work under slugs of its own. A suspension keeps its
    // tree and FOLLOW-UPS.json for the resume, whose last run files them,
    // but a chain that is given up or whose resume fails ends in a cold
    // review too. So only an attempt that concluded files its follow-ups.
    let redone = state == JobState::Suspended || failure.is_some();
    // Follow-ups are read from the tree, so before it is released. The
    // release itself is state-checked: a suspended harvest keeps its tree
    // for the resume, where this used to drop it after every run.
    let followups = if redone {
        // Logged, so the follow-ups left unfiled are not silently dropped
        // (see `ingest_followups`).
        if worktree.join("FOLLOW-UPS.json").exists() {
            let why = if state == JobState::Suspended {
                "kept for the resume"
            } else {
                "the attempt will be redone"
            };
            let _ = store
                .log_event(
                    "harvest",
                    &format!("#{fid}: FOLLOW-UPS.json not filed: {why}"),
                    Some(job),
                    Some(fid),
                )
                .await;
        }
        None
    } else {
        ingest_followups(store, repo.id, &worktree, fid, job, "harvest").await
    };
    close_workspace(lane, ws).await;

    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Harvest.into()),
        finding_id: Some(fid),
        job_id: Some(job),
        state: Some(state),
        pr_number: Some(pr_number),
        ingest: followups,
        ..Default::default()
    };
    if state == JobState::Suspended {
        // A suspension is a pause, not a failure: the worker stopped
        // mid-work (out of window headroom, or died after doing work) and
        // its tree and transcript are kept for the resume. Counting it toward the streak would turn three
        // pauses of one healthy harvest into "stuck" and give the PR up.
        // It stays unharvested, where the harvest tier continues it.
        let _ = store
            .log_event(
                "harvest",
                &format!(
                    "#{fid} suspended; worktree kept at {} for the resume",
                    worktree.display()
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("suspended".into());
        summary.worktree = Some(worktree.to_string_lossy().into_owned());
        if override_mode == Some(BudgetOverride::Once) {
            log_write_failure(
                store.set_budget_override(fid, None).await,
                format_args!("#{fid}: clearing one-shot budget override"),
            );
        }
        return Ok(summary);
    }
    if let Some(failure) = &failure
        && handoff
    {
        // The handoff was an extra chance, not the harvest's own attempt:
        // the finding stays `closed` and the harvest tier reviews it cold,
        // with its streak untouched.
        let _ = store
            .log_event(
                "harvest",
                &format!(
                    "#{fid} continuing the withdrawal into its harvest failed ({}); \
                     it will be harvested cold",
                    failure.detail
                ),
                Some(job),
                Some(fid),
            )
            .await;
        summary.outcome = Some("handoff-failed".into());
        summary.failure = Some(failure.detail.clone());
        return Ok(summary);
    }
    if let (Some(failure), Some(held)) = (failure, held) {
        let detail = &failure.detail;
        summary.outcome = Some(if held { "held" } else { "retry" }.into());
        summary.failure = Some(detail.clone());
        if override_mode == Some(BudgetOverride::Once) {
            log_write_failure(
                store.set_budget_override(fid, None).await,
                format_args!("#{fid}: clearing one-shot budget override"),
            );
        }
        return Ok(summary);
    }

    // A merged PR's harvest changes no status, so the stamp alone records
    // it. A closed PR's classification and stamp land together through
    // `record_closed_harvest`: see there for why neither may land alone,
    // and why a verdict a human set in the meantime is kept.
    let reviewed = if let Some(Ok(verdict)) = &closure {
        let status = FindingStatus::from(Unclaimed::from(verdict.class));
        // The verdict is about the default branch. A cold harvest's tree is
        // at its tip; a continued one's tree is the PR's head, so the tip
        // is read from the clone instead.
        let judged_at = if resume.is_some() {
            let clone = PathBuf::from(&repo.path);
            let tip = format!("origin/{}", repo.default_branch);
            tokio::task::spawn_blocking(move || crate::workspace::resolve(&clone, &tip))
                .await
                .ok()
                .flatten()
                .unwrap_or_else(|| pinned.clone())
        } else {
            pinned.clone()
        };
        let anchor = crate::suppression::verdict_anchor(
            status,
            &judged_at,
            finding,
            verdict.holds_while.as_deref(),
            &verdict.depends_on,
        );
        match store
            .record_closed_harvest(
                fid,
                verdict.class,
                &verdict.reason,
                anchor.as_ref(),
                now_ms(),
            )
            .await
        {
            Err(e) => {
                // Nothing landed, so the finding is still `closed` and
                // pending and the next cycle harvests it again. No streak
                // entry: the worker did its job, and a database that
                // refused the write is not a reason to give its PR up.
                let _ = store
                    .log_event(
                        "error",
                        &format!(
                            "harvest #{fid}: could not record the classification ({e}), will retry"
                        ),
                        Some(job),
                        Some(fid),
                    )
                    .await;
                summary.outcome = Some("retry".into());
                summary.failure = Some(format!("recording the classification failed: {e}"));
                if override_mode == Some(BudgetOverride::Once) {
                    log_write_failure(
                        store.set_budget_override(fid, None).await,
                        format_args!("#{fid}: clearing one-shot budget override"),
                    );
                }
                return Ok(summary);
            }
            Ok(true) => {
                summary.verdict = Some(status.as_str().to_owned());
                summary.reason = Some(verdict.reason.clone());
                format!(
                    "closed as {}, finding now {status}; reviewed for follow-ups",
                    verdict.class
                )
            }
            Ok(false) => {
                let kept = store
                    .get_finding(fid)
                    .await
                    .ok()
                    .flatten()
                    .map_or_else(|| "unknown".to_owned(), |f| f.status.as_str().to_owned());
                format!(
                    "kept the human's verdict {kept}; closure classified as {}; \
                     reviewed for follow-ups",
                    verdict.class
                )
            }
        }
    } else {
        log_write_failure(
            store.mark_pr_harvested(fid, now_ms()).await,
            format_args!("#{fid}: marking PR harvested"),
        );
        "reviewed for follow-ups".to_owned()
    };
    let _ = store
        .log_event(
            "harvest",
            &format!("#{fid} PR #{pr_number} {reviewed}"),
            Some(job),
            Some(fid),
        )
        .await;
    summary.outcome = Some("harvested".into());
    if override_mode == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
    Ok(summary)
}

/// A closed PR's review that cannot be used.
#[derive(Debug, Clone)]
struct CloseFailure {
    /// What the identical-failure streak compares. Stable per kind of
    /// failure on purpose: a worker that writes a different invented
    /// classification each time is failing the same way every time, and
    /// keying on the bad value would reset the streak and never give up.
    streak_key: String,
    /// What the event says.
    detail: String,
}

/// A closed PR's review, as the finding will record it.
#[derive(Debug, Clone)]
struct CloseVerdict {
    class: ClosureClass,
    /// `verdict_reason`: `"<classification>: <reason>"`, plus the evidence.
    reason: String,
    /// The condition in the current code that makes a suppressing verdict
    /// true, and the paths it rests on ([`crate::suppression`]).
    holds_while: Option<String>,
    depends_on: Vec<String>,
}

/// `CLOSE-REASON.json` as `harvest-closed.md` asks for it.
#[derive(Deserialize)]
struct CloseReasonFile {
    classification: String,
    reason: String,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    holds_while: Option<String>,
    #[serde(default, deserialize_with = "crate::suppression::de_paths")]
    depends_on: Vec<String>,
}

/// Read and check the closed-PR harvest's verdict file.
fn read_close_reason(worktree: &Path) -> Result<CloseVerdict, CloseFailure> {
    use std::fmt::Write as _;
    let fail = |key: &str, detail: String| CloseFailure {
        streak_key: key.to_owned(),
        detail,
    };
    let path = worktree.join("CLOSE-REASON.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Err(fail(
            "no CLOSE-REASON.json",
            "worker left no CLOSE-REASON.json".to_owned(),
        ));
    };
    let file: CloseReasonFile = serde_json::from_str(&text).map_err(|e| {
        fail(
            "invalid CLOSE-REASON.json",
            format!("CLOSE-REASON.json unreadable: {e}"),
        )
    })?;
    let class: ClosureClass = file
        .classification
        .trim()
        .to_ascii_lowercase()
        .parse()
        .map_err(|e| {
            fail(
                "invalid CLOSE-REASON.json",
                format!("CLOSE-REASON.json: {e}"),
            )
        })?;
    let reason = file.reason.trim();
    if reason.is_empty() {
        return Err(fail(
            "invalid CLOSE-REASON.json",
            "CLOSE-REASON.json: empty reason".to_owned(),
        ));
    }
    let mut full = format!("{class}: {reason}");
    if let Some(evidence) = file.evidence.as_deref().map(str::trim)
        && !evidence.is_empty()
    {
        let _ = write!(full, " (evidence: {evidence})");
    }
    // The same bound an engage withdrawal's reason gets: this text is
    // injected into every later scan's suppression list when it suppresses.
    Ok(CloseVerdict {
        class,
        reason: full.chars().take(500).collect(),
        holds_while: file.holds_while,
        depends_on: file.depends_on,
    })
}

/// How soon a disk denial is retried. Measuring is free, so this only
/// bounds how long freed space goes unused.
const DISK_RETRY_MS: i64 = 5 * 60_000;

/// The cycle's denial when `cfg.work_root` is short of space, else `None`.
///
/// A filesystem that cannot be measured does not deny: that is a missing
/// number, not a full disk, and every job would otherwise stop on it.
fn disk_gate(cfg: &Config) -> Option<String> {
    match nix::sys::statvfs::statvfs(&cfg.work_root) {
        Ok(st) => disk_denial(
            cfg,
            st.blocks_available().saturating_mul(st.fragment_size()),
        ),
        Err(e) => {
            tracing::warn!("disk gate: cannot stat {}: {e}", cfg.work_root.display());
            None
        }
    }
}

/// The denial for `free` bytes under `cfg.work_root`: `Some` below
/// `cfg.min_free_disk_bytes`, `None` at or above it.
pub fn disk_denial(cfg: &Config, free: u64) -> Option<String> {
    (free < cfg.min_free_disk_bytes).then(|| {
        format!(
            "disk: {} MiB free under {} < {} MiB",
            free >> 20,
            cfg.work_root.display(),
            cfg.min_free_disk_bytes >> 20
        )
    })
}

/// How soon a cycle denied for an unreachable host is retried. A probe
/// is one connection attempt per host, so this only bounds how long work
/// waits once the network is back.
const NETWORK_RETRY_MS: i64 = 3 * 60_000;

/// The hosts work on the repo at `repo_url` needs, for
/// [`crate::reach::first_unreachable`]: the forge the URL names, if any
/// (a local clone has none), then the hosts `backend`'s workers talk to.
///
/// Every kind needs both, since every kind can run a worker: even a
/// `dep_update` that Renovate usually answers falls back to one.
pub fn work_hosts(repo_url: &str, backend: &dyn Backend) -> Vec<String> {
    crate::forge::web_host(repo_url)
        .into_iter()
        .chain(backend.worker_hosts())
        .collect()
}

/// Layer 2 of the network gate (#95): the host the work on the repo at
/// `repo_url` needs that cannot be reached right now, if any, asked again
/// when a failure is about to be counted. A failure while one is
/// unreachable is the machine's, not the work's, and counts nothing. The
/// same probe as the gate's ([`work_hosts`], `Backend::first_unreachable`);
/// always `None` with
/// `network.gate` off. Never read from the error's text: a forge or a
/// provider words an outage in too many ways.
async fn unreachable_now(cfg: &Config, repo_url: &str, backend: &dyn Backend) -> Option<String> {
    if !cfg.network_gate {
        return None;
    }
    backend
        .first_unreachable(&work_hosts(repo_url, backend))
        .await
}

/// The cycle's end when a host its work needs is unreachable: denied, to
/// be retried in [`NETWORK_RETRY_MS`], with a `deny` event naming the host
/// and the host in the summary, so the status page shows it offline.
/// Before dispatch ([`network_gate`]) or once a failure turned out to be
/// the network's ([`unreachable_now`]); either way nothing is counted and
/// no checkpoint moves.
async fn offline_denial(
    store: &Store,
    kind: JobKind,
    finding_id: Option<i64>,
    job_id: Option<i64>,
    host: String,
) -> CycleSummary {
    let reason = format!("offline: {host} unreachable");
    let _ = store.log_event("deny", &reason, job_id, finding_id).await;
    CycleSummary {
        kind: Some(kind),
        finding_id,
        job_id,
        denied: Some(reason),
        retry_at: Some(now_ms() + NETWORK_RETRY_MS),
        unreachable: Some(host),
        ..Default::default()
    }
}

/// A worker that failed without doing any metered work, while a host its
/// work needs is unreachable: it never got to the work, so its failure is
/// not counted. Its job is closed `failed` as unstarted, which leaves the
/// checkpoint it was continuing in place, and the cycle ends as
/// [`offline_denial`] (`Break`). Any other run is handed back
/// (`Continue`) for its outcome to be recorded: one that did metered work
/// failed at the work, and is counted as before.
async fn unmetered_offline(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    repo_url: &str,
    (work, running): (WorkOf<'_>, RunningJob),
    rr: &RunResult,
) -> ControlFlow<CycleSummary, RunningJob> {
    if !matches!(job_state(rr), FinishedState::Failed | FinishedState::Killed) || rr.tokens_new > 0
    {
        return ControlFlow::Continue(running);
    }
    let Some(host) = unreachable_now(cfg, repo_url, backend).await else {
        return ControlFlow::Continue(running);
    };
    let job = running.id();
    log_write_failure(
        running
            .fail_unstarted(&format!(
                "offline: {host} unreachable; the worker did no metered work"
            ))
            .await,
        format_args!("job {job}: failing unstarted"),
    );
    ControlFlow::Break(
        offline_denial(store, work.job_kind(), work.finding_id(), Some(job), host).await,
    )
}

/// The host `candidate` needs that cannot be reached, if any; always
/// `None` with `network.gate` off.
async fn network_gate(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    candidate: &Candidate,
) -> anyhow::Result<Option<String>> {
    if !cfg.network_gate {
        return Ok(None);
    }
    // A repo that is gone has no forge to probe; dispatch reports it.
    let hosts = match store.get_repo_by_id(candidate.repo_id()).await? {
        Some(repo) => work_hosts(&repo.url, backend),
        None => backend.worker_hosts(),
    };
    Ok(backend.first_unreachable(&hosts).await)
}

/// Run one cycle: sync PRs, `pick_next`, dispatch to the appropriate runner (`scheduler.run_cycle`).
///
/// Takes `recovered` because a cycle creates jobs, which a store whose
/// dead-process rows are unsettled must not get ([`Recovered`]). Leaves
/// no job it created `running` and no claim it took `fixing`, however its
/// runner returned: each is a [`RunningJob`] or [`FixClaim`] the runner
/// either ended or dropped, and the cycle's [`CycleScope`] settles what
/// was dropped. Nothing still held, by this cycle or any other, is touched.
///
/// A settle that fails is the cycle's error, ahead of any the runner
/// returned, so the status page and the 5-minute error backoff know.
/// What it failed on is owed to the store's ledger, and the next cycle
/// settles that first ([`Recovered::settle_unsettled`]); while that
/// still fails, no cycle starts, so nothing new is created beside rows
/// that hold budget and trees with no runner.
pub async fn run_cycle(
    recovered: &Recovered,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> CycleSummary {
    let store = recovered.store();
    // Every settled row already leaves its own event; this is only the
    // per-cycle count, so it is logged whatever it is.
    if let Err(e) = recovered
        .settle_unsettled(&cfg.work_root)
        .await
        .map(|settled| tracing::debug!("owed settles: {settled:?}"))
    {
        return CycleSummary {
            error: Some(settle_failed(store, "an earlier cycle's", &e).await),
            ..Default::default()
        };
    }
    // The cycle is one lane: its runners take clone permits in it, and
    // none can outlive it.
    let ran = store
        .lane(async |lane| {
            run_scoped_cycle(CycleScope::open(recovered), lane, cfg, backend, force_repo).await
        })
        .await;
    ran.unwrap_or_else(|e| CycleSummary {
        error: Some(e.to_string()),
        ..Default::default()
    })
}

/// [`run_cycle`] inside its lane: run the picked work, then settle what
/// the runner dropped.
async fn run_scoped_cycle(
    scope: CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> CycleSummary {
    let store = scope.store().clone();
    // Catch-all
    let mut summary = match run_cycle_inner(&scope, lane, cfg, backend, force_repo).await {
        Ok(result) => result,
        Err(e) => {
            let _ = store
                .log_event("error", &format!("cycle crashed: {e:?}"), None, None)
                .await;
            CycleSummary {
                error: Some(e.to_string()),
                ..Default::default()
            }
        }
    };
    if let Err(e) = scope.settle(&cfg.work_root).await {
        let msg = settle_failed(&store, "the cycle's", &e).await;
        summary.error = Some(match summary.error.take() {
            Some(runner) => format!("{msg}; {runner}"),
            None => msg,
        });
    }
    summary
}

/// Log a failed settle of `whose` abandoned work; the message it logged.
async fn settle_failed(store: &Store, whose: &str, e: &anyhow::Error) -> String {
    // What it means for the operator first: `describe_cycle` keeps the
    // first 200 characters, and a store error can be longer than that.
    let msg = format!(
        "settling {whose} abandoned work failed, no new work starts until a retry succeeds: {e:#}"
    );
    tracing::error!("{msg}");
    let _ = store.log_event("error", &msg, None, None).await;
    msg
}

/// Run the picked candidate through its kind's executor.
///
/// Exhaustive dispatch on Candidate variant and sub-kind.
async fn dispatch(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    candidate: &Candidate,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    match candidate {
        Candidate::Finding {
            kind, finding_id, ..
        } => run_finding_job(scope, lane, cfg, backend, *kind, *finding_id, None).await,
        Candidate::Repo { kind, repo_id, .. } => {
            run_repo_job(scope, lane, cfg, backend, *kind, *repo_id, None).await
        }
        // A resume runs through the executor of the kind that was
        // suspended — so ingest, watermarks and PR handling are that
        // kind's own, unchanged. Logged here rather than in `pick_next`,
        // which the summary endpoint also calls on every poll.
        Candidate::Resume { plan, .. } => {
            let _ = store
                .log_event(
                    "resume",
                    &format!(
                        "resume {} {}: job {} -> reserving {} tok \
                         (ctx {} + max({} - {}, {}))",
                        plan.kind,
                        plan.repo,
                        plan.predecessor_id,
                        plan.anticipated,
                        plan.ctx,
                        plan.typical,
                        plan.chain_spent,
                        min_useful(plan.ctx)
                    ),
                    Some(plan.predecessor_id),
                    plan.finding_id,
                )
                .await;
            let resume = Some(plan.as_ref());
            match plan.kind {
                JobKind::Repo(kind) => {
                    run_repo_job(scope, lane, cfg, backend, kind, plan.repo_id, resume).await
                }
                JobKind::Finding(kind) => {
                    // A finding-kind job always recorded its target, so a
                    // row without one is corrupt rather than merely odd.
                    let fid = plan
                        .finding_id
                        .ok_or_else(|| anyhow::anyhow!("resume of a {kind} job with no finding"))?;
                    run_finding_job(scope, lane, cfg, backend, kind, fid, resume).await
                }
            }
        }
    }
}

async fn run_repo_job(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    kind: RepoJobKind,
    repo_id: i64,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let repo = store
        .get_repo_by_id(repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
    match kind {
        RepoJobKind::Hunt => run_hunt(scope, lane, cfg, &repo, backend, resume).await,
        RepoJobKind::TestGap => run_test_gap(scope, lane, cfg, &repo, backend, resume).await,
        RepoJobKind::DepUpdate => run_dep_update(scope, lane, cfg, &repo, backend, resume).await,
        RepoJobKind::Refactor => run_refactor(scope, lane, cfg, &repo, backend, resume).await,
        RepoJobKind::Modernization => run_modernize(scope, lane, cfg, &repo, backend, resume).await,
        RepoJobKind::Standards => run_standards(scope, lane, cfg, &repo, backend, resume).await,
    }
}

async fn run_finding_job(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    kind: FindingJobKind,
    finding_id: i64,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    let finding = store
        .get_finding(finding_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("finding {finding_id} not found"))?;
    match kind {
        FindingJobKind::Engage => run_engage(scope, lane, cfg, &finding, backend, resume).await,
        FindingJobKind::Harvest => run_harvest(scope, lane, cfg, &finding, backend, resume).await,
        FindingJobKind::Recheck => run_recheck(scope, lane, cfg, &finding, backend, resume).await,
        FindingJobKind::Fix => run_fix(scope, lane, cfg, &finding, backend, resume).await,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the scheduler's priority ladder: each tier is an ordered \
              `if` that must be read against the tiers above and below it, \
              which is exactly what extracting them into helpers destroys"
)]
async fn run_cycle_inner(
    scope: &CycleScope,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> anyhow::Result<CycleSummary> {
    let store = scope.store();
    // (0) Cheap PR sync
    let has_pr_open = !store
        .list_findings(&FindingFilter {
            status: Some(FindingStatus::PrOpen),
            ..FindingFilter::default()
        })
        .await?
        .is_empty();
    let sync = if has_pr_open {
        if let Some(pass) = scope.begin_pr_sync() {
            Some(sync_prs(&pass, cfg).await)
        } else {
            // Another pass of this store is live; its observations stand.
            // Nothing in the daemon runs one beside the cycle today.
            tracing::warn!("PR sync skipped: another sync pass is running");
            None
        }
    } else {
        None
    };

    // Before picking, so no job of any kind -- cold, resumed or forced --
    // starts writing a tree, a build or a transcript into a full disk.
    if let Some(reason) = disk_gate(cfg) {
        let _ = store.log_event("deny", &reason, None, None).await;
        return Ok(CycleSummary {
            denied: Some(reason),
            retry_at: Some(now_ms() + DISK_RETRY_MS),
            sync,
            ..Default::default()
        });
    }

    let picked = pick_next(store, cfg, force_repo).await?;
    if picked.is_none() {
        let result = CycleSummary {
            kind: None,
            idle: Some("no queued findings, no enabled repos".into()),
            sync,
            ..Default::default()
        };
        let _ = store
            .log_event("cycle", "idle: nothing to do", None, None)
            .await;
        return Ok(result);
    }

    let Some(candidate) = picked else {
        return Ok(CycleSummary {
            kind: None,
            idle: Some("no candidate".into()),
            ..Default::default()
        });
    };

    // Once the candidate is known, so only the hosts it needs are probed,
    // and before dispatch, so none of its work starts, fails or is
    // counted while its forge or provider cannot be reached (#95).
    if let Some(host) = network_gate(store, cfg, backend, &candidate).await? {
        return Ok(CycleSummary {
            sync,
            ..offline_denial(store, candidate.job_kind(), None, None, host).await
        });
    }

    // A repo kind's `last_{kind}_at` moves only when its runner succeeds.
    // A failure is counted on its work item and leaves the timestamp
    // alone, so the work is retried as soon as the rotation reaches it and
    // is held after three identical failures (WORK-MODEL.md §17), where it
    // used to be bumped here and wait out its interval each time.
    let mut result = dispatch(scope, lane, cfg, backend, &candidate).await?;

    if sync.is_some() {
        result.sync = sync;
    }
    // Build log line
    let mut parts: Vec<String> = Vec::new();
    if let Some(k) = result.kind {
        parts.push(format!("kind={k}"));
    }
    if let Some(v) = &result.repo {
        parts.push(format!("repo={v}"));
    }
    if let Some(v) = result.finding_id {
        parts.push(format!("finding={v}"));
    }
    if let Some(v) = result.job_id {
        parts.push(format!("job={v}"));
    }
    if let Some(v) = result.state {
        parts.push(format!("state={v}"));
    }
    if let Some(v) = &result.outcome {
        parts.push(format!("outcome={v}"));
    }
    if let Some(v) = &result.skipped {
        parts.push(format!("skipped={v}"));
    }
    if let Some(v) = &result.denied {
        parts.push(format!("denied={v}"));
    }
    if let Some(v) = &result.error {
        parts.push(format!("error={v}"));
    }
    let mut msg = parts.join(", ");
    if let Some(s) = &result.sync {
        let sync_str = format!(
            "prsync {}s/{}m/{}c/{}a/{}e",
            s.synced, s.merged, s.closed, s.attention, s.errors
        );
        msg = if msg.is_empty() {
            sync_str
        } else {
            format!("{msg}; {sync_str}")
        };
    }
    let _ = store.log_event("cycle", &msg, None, None).await;
    Ok(result)
}

#[cfg(test)]
mod extract_pr_url_tests {
    use super::extract_pr_url;

    #[test]
    fn stops_at_the_last_digit_of_the_pr_number() {
        assert_eq!(
            extract_pr_url("already exists: https://github.com/o/r/pull/12."),
            Some("https://github.com/o/r/pull/12".to_owned())
        );
        assert_eq!(
            extract_pr_url("https://github.com/o/r/pull/12/files?x=1"),
            Some("https://github.com/o/r/pull/12".to_owned())
        );
    }

    #[test]
    fn needs_a_number_after_pull() {
        assert_eq!(extract_pr_url("https://github.com/o/r/pull/abc"), None);
        assert_eq!(extract_pr_url("https://github.com/o/r/pull/"), None);
        assert_eq!(
            extract_pr_url("https://github.com/o/r/pull/new https://github.com/o/r/pull/9"),
            Some("https://github.com/o/r/pull/9".to_owned()),
            "a word without a number does not end the search"
        );
    }

    #[test]
    fn only_https_words_count() {
        assert_eq!(extract_pr_url("http://github.com/o/r/pull/3"), None);
        assert_eq!(extract_pr_url("github.com/o/r/pull/3"), None);
        assert_eq!(extract_pr_url("no url here"), None);
    }

    #[test]
    fn first_match_wins() {
        assert_eq!(
            extract_pr_url("https://github.com/o/r/pull/1 https://github.com/o/r/pull/2"),
            Some("https://github.com/o/r/pull/1".to_owned())
        );
    }
}
