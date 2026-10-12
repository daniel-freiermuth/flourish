//! Scheduler — job selection and the executors that carry it out.
//!
//! The selection half (`pick_next`, `anticipated_tokens`) is also what
//! `GET /api/summary` uses for its next-candidate preview, so the
//! endpoint and the loop cannot disagree about what runs next. The
//! `run_*` executors below are the other half.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Context as _;

use crate::config::Config;
use crate::domain::{
    BudgetOverride, ClosureClass, FindingJobKind, FindingStatus, FindingType, HoldReason, JobKind,
    JobState, RepoJobKind, Severity,
};
use crate::store::{CreatedJob, FindingFilter, JobOutcome, Store, StoreWriteError, SyncPrData};
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
/// skips over them ([`end_checkpoint`]), and counts a finding tier's
/// given-up chain as a failed attempt ([`count_given_up_chain`]).
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
/// do the scheduler's work: ending a chain, bumping a rotation
/// timestamp, or counting a given-up chain against its finding, which can
/// send the finding to the inbox, hold it for the operator or give its PR up. A
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
    // Held work waits for the operator: every tier skips it, through the
    // one shared query (WORK-MODEL.md §9.5, invariant 3).
    let held = store.held_work().await?;
    let held_ref = &held;
    let not_held = |kind: FindingJobKind| move |f: &Finding| !held_ref.contains(&(f.id, kind));
    rechecking.retain(not_held(FindingJobKind::Recheck));
    attention.retain(not_held(FindingJobKind::Engage));
    pending_harvest.retain(not_held(FindingJobKind::Harvest));
    queued.retain(not_held(FindingJobKind::Fix));

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
    if let Some(c) = pick_resume(store, cfg, force_id, &held, mode).await? {
        return Ok(Some(c));
    }

    // (7) Repo rotation: enabled repos in staleness order — a repo with an
    // urgent full re-hunt request first, then never-hunted, then oldest
    // last_hunt_at first; ties keep list_repos' name order (Python
    // sorted() and Vec::sort_by_key are both stable).
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
    repos.sort_by_key(|r| {
        (
            !full_hunt_urgent(r),
            r.last_hunt_at.is_some(),
            r.last_hunt_at.unwrap_or(0),
        )
    });

    let now = now_ms();
    for target in &repos {
        if !Path::new(&target.path).exists() {
            // Not cloned yet -> hunt does the clone (`scheduler.pick_next`).
            return Ok(Some(repo_candidate(RepoJobKind::Hunt, target)));
        }

        // An operator asked for a full re-hunt: that hunt goes ahead of
        // the repo's other scans and of the scan interval.
        if full_hunt_urgent(target) {
            return Ok(Some(repo_candidate(RepoJobKind::Hunt, target)));
        }

        if let Some(kind) = rotation_kind(cfg, target, now) {
            return Ok(Some(repo_candidate(kind, target)));
        }
        // All scan types ran within their intervals for this repo.
    }

    Ok(None)
}

/// The rotation scan `target` is due for at `now`, if any.
fn rotation_kind(cfg: &Config, target: &Repo, now: i64) -> Option<RepoJobKind> {
    let scan_interval_ms = (cfg.scan_interval_days * 86_400_000.0) as i64;
    let mod_interval_ms = cfg.modernization_interval_days * 86_400_000;

    // Eligible job types with their last-run timestamps, in
    // RepoJobKind::ALL insertion order (mirrors Python's dict).
    let mut job_times: Vec<(RepoJobKind, i64)> = Vec::with_capacity(RepoJobKind::ALL.len());
    for (kind, last) in [
        (RepoJobKind::Hunt, target.last_hunt_at),
        (RepoJobKind::TestGap, target.last_test_gap_at),
        (RepoJobKind::DepUpdate, target.last_dep_update_at),
        (RepoJobKind::Refactor, target.last_refactor_at),
    ] {
        let last = last.unwrap_or(0);
        if last == 0 || (now - last) >= scan_interval_ms {
            job_times.push((kind, last));
        }
    }
    let last_modernization = target.last_modernization_at.unwrap_or(0);
    if last_modernization == 0 || (now - last_modernization) >= mod_interval_ms {
        job_times.push((RepoJobKind::Modernization, last_modernization));
    }
    let last_standards = target.last_standards_at.unwrap_or(0);
    let std_interval_ms = cfg.standards_interval_days * 86_400_000;
    if last_standards == 0 || (now - last_standards) >= std_interval_ms {
        job_times.push((RepoJobKind::Standards, last_standards));
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
/// hunt is retried after the scan interval, not back to back — but stays
/// pending, so the repo's next hunt is still a full one. Asking again
/// makes it urgent again.
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

/// The prompt a resumed recheck, engage or harvest gets: [`RESUME_PROMPT`],
/// or, for the first attempt after the operator continued its hold, one
/// chosen by the hold's reason (WORK-MODEL.md §9.1), with the hold's detail
/// as context. Short like [`RESUME_PROMPT`], for the same reason: the
/// session holds the playbook. A fix's comes with its updated playbook
/// instead ([`fix_prompt`]).
fn resume_prompt(kind: FindingJobKind, plan: &ResumePlan) -> String {
    let Some((reason, detail)) = &plan.after_hold else {
        return RESUME_PROMPT.to_owned();
    };
    let (work, deliverable) = match kind {
        FindingJobKind::Recheck => ("recheck", "your verdict file"),
        FindingJobKind::Engage => ("PR engagement", "PR-REPLY.md, or WITHDRAW.md"),
        FindingJobKind::Fix => ("fix", "PR-DESCRIPTION.md"),
        FindingJobKind::Harvest => ("PR review", "what the playbook asks for"),
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
/// worth continuing, or `None`.
///
/// Candidates are walked newest-first; [`resume_plan`] decides each one
/// and ends those that can never be continued, so the walk moves on to
/// the next in the same cycle — which is what "fall through to normal
/// selection" means.
async fn pick_resume(
    store: &Store,
    cfg: &Config,
    force_id: Option<i64>,
    held: &BTreeSet<(i64, FindingJobKind)>,
    mode: Pick,
) -> anyhow::Result<Option<Candidate>> {
    for job in store.list_resumable_jobs().await? {
        if force_id.is_some_and(|rid| job.repo_id != rid) {
            continue;
        }
        if let (Some(fid), Some(kind)) = (job.finding_id, job.kind.as_finding())
            && held.contains(&(fid, kind))
        {
            continue;
        }
        if let Resumable::Yes(plan) = resume_plan(store, cfg, job, mode).await? {
            return Ok(Some(Candidate::Resume {
                label: (!plan.repo.is_empty()).then(|| plan.repo.clone()),
                budget_override: None,
                plan,
            }));
        }
    }
    Ok(None)
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

/// What [`finding_pick`] made of one finding of a tier.
enum Picked {
    /// Run this.
    Work(Candidate),
    /// Its work ended here (a given-up chain was counted); the selection
    /// starts over, since what that wrote can change every tier.
    Ended,
    /// Nothing to do for it, and nothing changed.
    Nothing,
}

/// The failure a given-up chain records in its finding's streak.
const CHAIN_GAVE_UP: &str = "resume chain gave up";

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
        _ => (None, None),
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
        // The chain's turn is over. Every attempt in it was a suspension,
        // which the cycle's starvation bump leaves alone, so a rotation
        // kind's timestamp is still as old as before the chain started:
        // rotation would pick the same work fresh in this very selection
        // and repeat the whole chain, while everything else waited. Hunts
        // included: a hunt's watermark stays where it was, so the next
        // hunt reviews the same commits after the scan interval.
        //
        // Bumped before the checkpoint is dropped: once dropped, the chain
        // is no longer resumable and nothing would retry a bump that
        // failed, whereas a drop that fails after a bump leaves the
        // checkpoint and the next selection runs both writes again.
        if let JobKind::Repo(kind) = job.kind {
            let _ = store.set_last_kind_at(job.repo_id, kind).await;
        }
        // Best-effort: the candidate is skipped either way, so a failed
        // write only defers the record — and only a landed give-up is
        // reported as one, so the chain is counted once.
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
use crate::forge::{self, CheckConclusion, GhCheckRun, Mergeable, PrView, ReviewDecision};
use crate::ingest::{EntryTypes, IngestResult, ingest_findings, ingest_scan_findings};
use crate::playbooks;
use crate::types::RunResult;
use crate::workspace::{TreeSpec, Workspace};
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
fn job_state(rr: &RunResult) -> JobState {
    match rr.killed_reason.as_deref() {
        Some("cap") if rr.session_file.is_some() => JobState::Suspended,
        Some("wallclock") if rr.session_file.is_some() && rr.tokens_new > 0 => JobState::Suspended,
        Some("resume-unavailable") => JobState::Failed,
        Some(_) => JobState::Killed,
        None if rr.exit_code == Some(0) => JobState::Done,
        None if rr.session_file.is_some() && rr.tokens_new > 0 => JobState::Suspended,
        None => JobState::Failed,
    }
}

/// Turn a refused `create_job` into a skipped cycle.
///
/// The repo was live when this run was picked and soft-deleted before the
/// job row went in. Losing that race is ordinary operator traffic, not a
/// fault: reporting it as an error would log "cycle crashed" and hand the
/// operator a stack trace for having deleted a repo. A genuine DB failure
/// still propagates.
fn job_refused(
    kind: JobKind,
    repo: Option<&str>,
    finding_id: Option<i64>,
    err: StoreWriteError,
) -> anyhow::Result<CycleSummary> {
    match err {
        StoreWriteError::Refused(msg) => Ok(CycleSummary {
            kind: Some(kind),
            repo: repo.map(ToOwned::to_owned),
            finding_id,
            skipped: Some(msg),
            ..Default::default()
        }),
        StoreWriteError::Db(e) => Err(e.into()),
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
    job_id: i64,
    rr: &RunResult,
    model: Option<&str>,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<JobState> {
    let state = job_state(rr);
    let notes = if state != JobState::Done && !rr.stdout_tail.is_empty() {
        Some(crate::util::tail(&rr.stdout_tail, 500))
    } else {
        None
    };
    store
        .complete_job(
            job_id,
            &JobOutcome {
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
            },
        )
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
    Ok(state)
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

/// Blocking git command wrapper for use inside `spawn_blocking`.
fn run_cmd_sync(argv: &[&str], timeout_s: u64) -> (i32, String) {
    crate::util::run_cmd(argv, timeout_s)
}

/// Clone the repo if it is not cloned yet, then fetch.
///
/// Fetch only — never checkout or pull. The clone is the object store
/// every chain's worktree is added from, not a working tree anyone runs
/// in: moving its HEAD would do nothing for the trees, and moving the
/// trees is exactly what must never happen under a suspended chain.
/// Returns Ok(()) on success, Err with an error summary on failure.
async fn sync_repo(
    store: &Store,
    repo_url: &str,
    rpath: &Path,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> Result<(), String> {
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
        let rp = rpath.to_string_lossy().to_string();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &["git", "-C", &rp, "config", "--get", "remote.origin.url"],
                60,
            )
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        let origin = out.trim();
        if rc != 0 || origin != repo_url {
            let msg = format!(
                "{} exists but its origin is {} -- expected {repo_url}; \
                 refusing to work in a clone of a different repository",
                rpath.display(),
                if rc == 0 { origin } else { "unreadable" }
            );
            let _ = store
                .log_event("error", &format!("{log_prefix}: {msg}"), None, finding_id)
                .await;
            return Err(msg);
        }
    } else {
        if let Some(parent) = rpath.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let url = repo_url.to_owned();
        let rp = rpath.to_owned();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            // `--` before the operands: even a URL that slipped past
            // validation cannot be read as an option here.
            run_cmd_sync(&["git", "clone", "--", &url, &rp.to_string_lossy()], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: clone failed: {tail}"),
                    None,
                    finding_id,
                )
                .await;
            return Err(format!("clone failed: {tail}"));
        }
    }
    let rps = rpath.to_string_lossy().to_string();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(&["git", "-C", &rps, "fetch", "origin"], 600)
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        let tail = crate::util::tail(&out, 300);
        let _ = store
            .log_event(
                "error",
                &format!("{log_prefix}: git fetch origin failed: {tail}"),
                None,
                finding_id,
            )
            .await;
        return Err(format!("git fetch origin failed: {tail}"));
    }
    Ok(())
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
/// `Ok(Err(summary))`: the tree could not be made. The job is recorded
/// `failed` with the reason, and [`crate::workspace::create`] has already
/// removed whatever it made.
async fn open_workspace(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    created: &CreatedJob,
    resume: Option<&ResumePlan>,
    spec: TreeSpec,
    kind: JobKind,
    log_prefix: &str,
    finding_id: Option<i64>,
) -> anyhow::Result<Result<(Workspace, String), CycleSummary>> {
    if let Some(plan) = resume {
        return Ok(Ok((plan.workspace.clone(), plan.pinned_sha.clone())));
    }
    let clone = Path::new(&repo.path);
    for &old in &created.superseded {
        let origin = store.resume_origin_job(old).await?;
        let stale = Workspace::for_chain(&cfg.work_root, clone, origin);
        crate::workspace::release_if_idle(store, &stale).await?;
    }
    let ws = Workspace::for_chain(&cfg.work_root, clone, created.id);
    let made = {
        let ws = ws.clone();
        tokio::task::spawn_blocking(move || crate::workspace::create(&ws, &spec)).await?
    };
    match made {
        Ok(sha) => {
            store.set_pinned_sha(created.id, &sha).await?;
            Ok(Ok((ws, sha)))
        }
        Err(why) => {
            let note = format!("{TREE_NOT_MADE}: {why}");
            store.fail_unstarted_job(created.id, &note).await?;
            let _ = store
                .log_event(
                    "error",
                    &format!("{log_prefix}: job {} {note}", created.id),
                    Some(created.id),
                    finding_id,
                )
                .await;
            Ok(Err(CycleSummary {
                kind: Some(kind),
                repo: Some(repo.name.clone()),
                finding_id,
                job_id: Some(created.id),
                state: Some(JobState::Failed),
                failure: Some(note),
                ..Default::default()
            }))
        }
    }
}

/// Release a chain's tree once its job has ended, unless the job table
/// says the chain is still `running` or `suspended` — a suspension is a
/// pause, and its tree is the state the resume continues.
///
/// Best effort: a tree that could not be released is picked up by the
/// sweep before the next cycle.
async fn close_workspace(store: &Store, ws: &Workspace) {
    if let Err(e) = crate::workspace::release_if_idle(store, ws).await {
        tracing::warn!("releasing {}: {e}", ws.tree.display());
    }
}

/// Log a failed store write the job deliberately continues past.
///
/// These writes sit mid-job, ahead of steps that must still run —
/// `finalize_in_progress`, releasing the tree, retiring the job row — so
/// aborting with `?` would trade one inconsistency for a worse one, such
/// as a finding stranded in `fixing` until the next restart. Continuing
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

/// Fetch `repo`'s clone and return the tip of its default branch: where a
/// cold hunt's chain is pinned.
async fn fetch_tip(store: &Store, repo: &Repo) -> anyhow::Result<String> {
    let rname = &repo.name;
    let db = &repo.default_branch;
    let rpath = PathBuf::from(&repo.path);
    sync_repo(store, &repo.url, &rpath, &format!("hunt {rname}"), None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let rev = format!("origin/{db}");
    let tip = tokio::task::spawn_blocking(move || crate::workspace::resolve(&rpath, &rev)).await?;
    let Some(tip) = tip else {
        let _ = store
            .log_event(
                "error",
                &format!("hunt {rname}: origin/{db} does not resolve"),
                None,
                None,
            )
            .await;
        anyhow::bail!("origin/{db} does not resolve");
    };
    Ok(tip)
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
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    // Git's empty tree — the implicit parent of all root commits.
    // Using this as diff base includes the root commit itself.
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
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
        match fetch_tip(store, repo).await {
            Ok(tip) => tip,
            Err(e) => {
                // The attempt is over before it had a job. It still answers
                // the request's place in the queue, or a fetch that keeps
                // failing would put this hunt first in every cycle.
                if let Some(r) = request {
                    log_write_failure(
                        store.mark_full_hunt_attempted(rid, r).await,
                        format_args!("repo {rid}: marking full-hunt request {r} attempted"),
                    );
                }
                return Err(e);
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
    let last = if rehunt_due {
        log_write_failure(
            store.clear_last_hunt_sha(rid).await,
            format_args!("repo {rid}: clearing last-hunt sha for the full re-hunt"),
        );
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
    // changed since. For a chain from before that column: starting a full
    // re-hunt is the only thing that clears the watermark of a repo that
    // has had a full hunt, so a resume finding none there is continuing
    // one.
    let resumed_full = match resume {
        Some(plan) => {
            store.job_full_history(plan.predecessor_id).await?
                || (last.is_none() && last_full.is_some())
        }
        None => false,
    };
    if resume.is_none() && last.as_deref() == Some(&head) {
        // The watermark is what makes the next pick move on; a skip that
        // failed to record it would be re-picked at once (a skipped cycle
        // restarts immediately), so the failure goes to the error path.
        store.set_last_hunt(rid, &head).await?;
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
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &rps,
                    "rev-list",
                    "-1",
                    "--before=3 weeks ago",
                    &h,
                ],
                30,
            )
        })
        .await
        .unwrap_or((127, String::new()));
        let base = base.trim().to_owned();
        if base.is_empty() || base == head {
            let rps = rp_str.clone();
            let h = head.clone();
            let (rc2, base2) = tokio::task::spawn_blocking(move || {
                run_cmd_sync(&["git", "-C", &rps, "rev-parse", &format!("{h}~30")], 30)
            })
            .await
            .unwrap_or((127, String::new()));
            let base2 = base2.trim().to_owned();
            if rc2 != 0 || base2.is_empty() {
                // Very small repo — scan from root
                (
                    format!("{EMPTY_TREE}..{head}"),
                    "First hunt for this repo: the full history (small repo).".to_owned(),
                )
            } else {
                (
                    format!("{base2}..{head}"),
                    format!(
                        "First hunt for this repo: the last 30 commits (base {}).",
                        &base2[..12.min(base2.len())]
                    ),
                )
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

    let created = match store
        .create_job(
            RepoJobKind::Hunt.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(RepoJobKind::Hunt.into(), Some(rname), None, e),
    };
    let job = created.id;
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
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        repo,
        &created,
        resume,
        TreeSpec::Detached { at: head.clone() },
        RepoJobKind::Hunt.into(),
        &format!("hunt {rname}"),
        None,
    )
    .await?
    {
        Ok(opened) => opened,
        Err(failed) => return Ok(failed),
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
        Some(_) => RESUME_PROMPT.to_owned(),
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
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = log_write_failure(
        record_job(store, job, &rr, model, resume).await,
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

    if out_path.exists() {
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
        if state == JobState::Done && counts.invalid == 0 {
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
        }
        summary.ingest = Some(counts);
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
    }
    crate::suppression::apply_reconfirmations(
        store,
        repo,
        &[FindingType::Bug.as_str()],
        &out_path,
        &pinned,
    )
    .await;
    close_workspace(store, &ws).await;
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
    store: &Store,
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
    let rpath = PathBuf::from(&repo.path);
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none() {
        sync_repo(
            store,
            &repo.url,
            &rpath,
            &format!("recheck #{fid}"),
            Some(fid),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
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
    let created = match store
        .create_job(
            FindingJobKind::Recheck.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Recheck.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let job = created.id;
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
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
        Some(plan) => resume_prompt(FindingJobKind::Recheck, plan),
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
            let ran = (FindingJobKind::Recheck, fid, job);
            let denied = runner_failed(store, (cfg, backend, &repo.url), ran, &e).await;
            close_workspace(store, &ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (FindingJobKind::Recheck, fid, job);
    if let Some(denied) = unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        close_workspace(store, &ws).await;
        return Ok(denied);
    }
    let state = log_write_failure(
        record_job(store, job, &rr, model, resume).await,
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
        close_workspace(store, &ws).await;
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
        close_workspace(store, &ws).await;
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
        close_workspace(store, &ws).await;
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
        close_workspace(store, &ws).await;
        return Ok(summary);
    };

    let reason_owned = verdict_obj.reason.as_deref().unwrap_or_default();
    let reason: String = reason_owned.chars().take(500).collect();
    let reason = reason.as_str();

    let verdict_str = match outcome {
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
            let update = crate::store::FindingAnalysisUpdate {
                summary: verdict_obj.updated_summary.clone(),
                detail: verdict_obj.updated_detail.clone(),
                confidence: verdict_obj.updated_confidence,
                severity,
            };
            log_write_failure(
                store.update_finding_analysis(fid, &update).await,
                format_args!("#{fid}: recording recheck analysis"),
            );
            log_write_failure(
                store.set_finding_status(fid, FindingStatus::New).await,
                format_args!("#{fid}: status -> new"),
            );
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} confirmed: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("confirmed".into());
            "confirmed"
        }
        RecheckOutcome::Stale => {
            // Superseded, not suppressed: the code moved on and took the
            // bug with it, which says nothing against the finding. As
            // `wontfix` it told every later scan not to report the bug
            // again, should the rewritten code bring it back.
            log_write_failure(
                store
                    .set_finding_verdict(
                        fid,
                        FindingStatus::Superseded,
                        &format!("recheck: {reason}"),
                    )
                    .await,
                format_args!("#{fid}: verdict -> superseded"),
            );
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} stale: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("stale".into());
            "stale"
        }
        RecheckOutcome::Invalid => {
            let anchor = crate::suppression::verdict_anchor(
                FindingStatus::Rejected,
                &pinned,
                finding,
                verdict_obj.holds_while.as_deref(),
                &verdict_obj.depends_on,
            );
            log_write_failure(
                store
                    .set_anchored_verdict(
                        fid,
                        FindingStatus::Rejected,
                        &format!("recheck: {reason}"),
                        anchor.as_ref(),
                    )
                    .await,
                format_args!("#{fid}: verdict -> rejected"),
            );
            let _ = store
                .log_event(
                    "recheck",
                    &format!("#{fid} invalid: {reason}"),
                    Some(job),
                    Some(fid),
                )
                .await;
            summary.outcome = Some("invalid".into());
            "invalid"
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
            close_workspace(store, &ws).await;
            return Ok(summary);
        }
    };
    // A verdict ends the recheck: its tree is no checkpoint.
    close_workspace(store, &ws).await;
    summary.verdict = Some(verdict_str.to_owned());
    summary.reason = Some(reason.to_owned());
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
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    spec: &AnalysisSpec,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let rid = repo.id;
    let rname = &repo.name;
    let rpath = PathBuf::from(&repo.path);
    let kind = spec.kind;

    if !rpath.exists() {
        let _ = store
            .log_event(
                "error",
                &format!("{kind} {rname}: repo not cloned"),
                None,
                None,
            )
            .await;
        anyhow::bail!("repo not cloned");
    }
    // Never on a resume: a resumed chain continues in its own tree, and
    // nothing is fetched for it.
    if resume.is_none() {
        sync_repo(store, &repo.url, &rpath, &format!("{kind} {rname}"), None)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
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
    let created = match store
        .create_job(
            kind.into(),
            rid,
            None,
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => return job_refused(kind.into(), Some(rname), None, e),
    };
    let job = created.id;
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        repo,
        &created,
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
        Err(failed) => return Ok(failed),
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
        Some(_) => RESUME_PROMPT.to_owned(),
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
    let rr = backend
        .run(
            &ws,
            &prompt,
            cap,
            cfg.hunt_max_wall_s,
            JobClass::Hunt,
            resume.map(|r| r.session_file.as_path()),
        )
        .await?;
    let state = log_write_failure(
        record_job(store, job, &rr, model, resume).await,
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

    if out_path.exists() {
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
        if state == JobState::Done && counts.invalid == 0 {
            // Update last_{kind}_at timestamp
            log_write_failure(
                store.set_last_kind_at(rid, kind).await,
                format_args!("repo {rid}: recording last {kind} run"),
            );
        }
        summary.ingest = Some(counts);
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
    }
    crate::suppression::apply_reconfirmations(store, repo, &types, &out_path, &pinned).await;
    close_workspace(store, &ws).await;
    Ok(summary)
}

pub async fn run_test_gap(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &TEST_GAP_SPEC, backend, resume).await
}
/// The zero-token Renovate scan of the repo at its fetched tip, with the
/// GitHub token the repo's forge qualifies for. `None` means Renovate has
/// no answer (see `scan_repo`).
async fn renovate_scan(
    cfg: &Config,
    repo: &Repo,
    rpath: &std::path::Path,
) -> Option<crate::dep_scan::DepScan> {
    let work_root = cfg.work_root.clone();
    let rp = rpath.to_owned();
    let rn = repo.name.clone();
    let rid = repo.id;
    let tip = format!("origin/{}", repo.default_branch);
    let forge = repo.forge;
    let configured = cfg.renovate_github_token.clone();
    tokio::task::spawn_blocking(move || {
        // Renovate reads manifests, and the fetch-only clone's own files
        // are never updated, so it scans a throwaway tree at the fetched tip.
        let scan = match crate::workspace::ScanTree::create(&work_root, &rp, rid, &tip) {
            Ok(scan) => scan,
            Err(e) => {
                tracing::warn!("dep_update {rn}: {e}");
                return None;
            }
        };
        let token = crate::dep_scan::github_token(
            forge,
            configured.as_ref().map(crate::config::Secret::expose),
        );
        crate::dep_scan::scan_repo(scan.path(), &rn, token.as_deref(), 120)
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
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    // A resume continues a suspended AI worker, so the Renovate
    // shortcut below is not an option for it: that path spends no
    // tokens, creates no job row, and would leave the suspension
    // uncontinued and still resumable. `run_analysis_job` repeats the
    // clone check and the sync, so nothing is skipped by going straight
    // there.
    if resume.is_some() {
        return run_analysis_job(store, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await;
    }
    let rpath = std::path::PathBuf::from(&repo.path);
    if !rpath.exists() {
        let _ = store
            .log_event(
                "error",
                &format!("dep_update {}: repo not cloned", repo.name),
                None,
                None,
            )
            .await;
        anyhow::bail!("repo not cloned");
    }

    sync_repo(store, &repo.url, &rpath, "dep_update", None)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Try Renovate (zero tokens). Its answer is authoritative even when it
    // is "nothing to update"; only an unavailable scan falls back to AI.
    let Some(scan) = renovate_scan(cfg, repo, &rpath).await else {
        tracing::info!(
            "dep_update {}: renovate unavailable, falling back to AI",
            repo.name
        );
        return run_analysis_job(store, cfg, repo, &DEP_UPDATE_SPEC, backend, resume).await;
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

    // Only advance timestamp after successful ingestion (no invalid entries)
    if counts.invalid == 0 {
        log_write_failure(
            store.set_last_dep_update(repo.id).await,
            format_args!("repo {}: recording last dep_update run", repo.id),
        );
    }

    Ok(CycleSummary {
        kind: Some(RepoJobKind::DepUpdate.into()),
        repo: Some(repo.name.clone()),
        state: Some(JobState::Done),
        ingest: Some(counts),
        ..Default::default()
    })
}
pub async fn run_refactor(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &REFACTOR_SPEC, backend, resume).await
}
pub async fn run_modernize(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &MODERNIZATION_SPEC, backend, resume).await
}
pub async fn run_standards(
    store: &Store,
    cfg: &Config,
    repo: &Repo,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    run_analysis_job(store, cfg, repo, &STANDARDS_SPEC, backend, resume).await
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

/// A fix job that owns its finding (`fixing`) and has a working tree.
struct FixStart {
    repo: Repo,
    job: i64,
    ws: Workspace,
    /// The commit the chain's tree was created at (`jobs.pinned_sha`).
    pinned: String,
    branch: String,
    cap: Option<i64>,
}

/// Put the finding back to `queued` after a step between the claim and the
/// worker failed; [`Store::finalize_in_progress`] only does so while the
/// fix still owns it.
async fn release_claim(store: &Store, fid: i64) {
    log_write_failure(
        store
            .finalize_in_progress(fid, FindingStatus::Fixing, FindingStatus::Queued)
            .await,
        format_args!("#{fid}: finalizing fixing -> queued"),
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
async fn start_fix(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<Result<FixStart, CycleSummary>> {
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
    if !store
        .claim_in_progress(fid, FindingStatus::Queued, FindingStatus::Fixing)
        .await?
    {
        return Ok(Err(fix_skipped(format!(
            "finding #{fid} is no longer queued"
        ))));
    }
    if let Err(e) = remove_stale_report(resume) {
        release_claim(store, fid).await;
        anyhow::bail!("fix #{fid}: {e}");
    }
    let created = match store
        .create_job(
            FindingJobKind::Fix.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            release_claim(store, fid).await;
            return job_refused(FindingJobKind::Fix.into(), Some(&repo.name), Some(fid), e)
                .map(Err);
        }
    };
    // The branch is per finding, so a superseded fix chain of this finding
    // holds it checked out; `open_workspace` releases that chain's tree
    // before adding this one.
    let opened = open_workspace(
        store,
        cfg,
        &repo,
        &created,
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
    // A tree that could not be made releases the claim once it is counted.
    if opened.is_err() {
        release_claim(store, fid).await;
    }
    let (ws, pinned) = match opened? {
        Ok(opened) => opened,
        Err(failed) => return Ok(Err(count_unmade_tree(store, fid, created.id, failed).await)),
    };
    Ok(Ok(FixStart {
        repo,
        job: created.id,
        ws,
        pinned,
        branch,
        cap,
    }))
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
async fn fix_prompt(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    start: &FixStart,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<String> {
    let fid = finding.id;
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
    let prompt = match built {
        Ok(p) => p,
        Err(e) => {
            release_claim(store, fid).await;
            anyhow::bail!("{e}");
        }
    };
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
/// part of deciding (a push or a PR that fails is a failed attempt).
async fn conclude_fix(
    store: &Store,
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
    // strand the finding at `fixing`.
    let report = match report_file.map(|f| read_report(f)).transpose() {
        Ok(report) => report,
        Err(e) => {
            release_claim(store, fid).await;
            anyhow::bail!("fix #{fid}: cannot read the worker's report: {e}");
        }
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

/// The failure a finding's work records when the backend could not run its
/// worker at all (`Backend::run` returned an error). The error follows it
/// in the detail; it is not the key, which must stay the same for one
/// outage to count as identical failures.
const RUNNER_ERROR: &str = "runner error";

/// Count a finding's work whose backend failed before any worker ran
/// (WORK-MODEL.md §7). The job is closed `failed` as unstarted rather than
/// left `running`, and the failure goes through the kind's own streak, so
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
    (kind, fid, job): (FindingJobKind, i64, i64),
    err: &anyhow::Error,
) -> Option<CycleSummary> {
    if let Some(host) = unreachable_now(cfg, repo_url, backend).await {
        log_write_failure(
            store
                .fail_unstarted_job(
                    job,
                    &format!("offline: {host} unreachable; {RUNNER_ERROR}: {err}"),
                )
                .await,
            format_args!("job {job}: failing unstarted"),
        );
        return Some(offline_denial(store, kind.into(), Some(fid), Some(job), host).await);
    }
    let detail = format!("{RUNNER_ERROR}: {err}");
    log_write_failure(
        store.fail_unstarted_job(job, &detail).await,
        format_args!("job {job}: failing unstarted"),
    );
    let mut summary = CycleSummary::default();
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
    fid: i64,
    job: i64,
    mut summary: CycleSummary,
) -> CycleSummary {
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
    release_claim(store, fid).await;
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
    let wts = worktree.to_string_lossy().to_string();
    let db2 = db.clone();
    let (rc, commits) = tokio::task::spawn_blocking(move || {
        let (rc, out) = run_cmd_sync(
            &[
                "git",
                "-C",
                &wts,
                "log",
                &format!("origin/{db2}..HEAD"),
                "--oneline",
            ],
            30,
        );
        if rc != 0 {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &wts,
                    "log",
                    &format!("{db2}..HEAD"),
                    "--oneline",
                ],
                30,
            )
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
    let wts = worktree.to_string_lossy().to_string();
    let (prc, pout) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &["git", "-C", &wts, "push", "--force", &push_url, "HEAD"],
            600,
        )
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
    let wts = worktree.to_string_lossy().to_string();
    let (_trc, title) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(&["git", "-C", &wts, "log", "-1", "--format=%s"], 30)
    })
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
    fid: i64,
    start: &FixStart,
    report: &str,
    summary: &mut CycleSummary,
) -> anyhow::Result<()> {
    let held = hold_on_report(
        store,
        FindingJobKind::Fix,
        fid,
        start.job,
        &start.ws,
        report,
    )
    .await;
    release_claim(store, fid).await;
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
    let status = class.map_or(FindingStatus::New, ClosureClass::status);
    let anchor = crate::suppression::verdict_anchor(
        status,
        &start.pinned,
        finding,
        holds_while.as_deref(),
        &depends_on,
    );
    log_write_failure(
        store
            .set_anchored_verdict(fid, status, reason, anchor.as_ref())
            .await,
        format_args!("#{fid}: verdict -> {status}"),
    );
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
    summary.outcome = Some(status.as_str().into());
    release_claim(store, fid).await;
}

/// Write a fix outcome: the finding's status, the event, and the summary.
/// A one-shot budget override is spent by every outcome but a `BLOCKED.md`
/// hold, a decline, and an attempt that could not publish offline (none of
/// them concluded the work's attempt; a streak's hold did, and spends it);
/// any outcome that leaves the finding `fixing` returns it to `queued`.
async fn record_fix_outcome(
    store: &Store,
    finding: &Finding,
    start: &FixStart,
    outcome: FixOutcome,
    stdout_tail: &str,
    summary: &mut CycleSummary,
) -> anyhow::Result<()> {
    let fid = finding.id;
    let job = start.job;
    let tail = crate::util::tail(stdout_tail, 300);
    match outcome {
        FixOutcome::Held { report } => {
            return hold_prerequisite(store, fid, start, &report, summary).await;
        }
        FixOutcome::Declined(decline) => {
            record_declined_fix(store, finding, start, decline, summary).await;
            return Ok(());
        }
        FixOutcome::Shipped { pr_url, recovered } => {
            log_write_failure(
                store.set_finding_pr_open(fid, &pr_url).await,
                format_args!("#{fid}: status -> pr_open ({pr_url})"),
            );
            let message = if recovered {
                format!("#{fid} recovered existing PR: {pr_url}")
            } else {
                format!("#{fid} draft PR: {pr_url}")
            };
            let _ = fix_event(store, "ship", fid, job, message).await;
            summary.outcome = Some("pr_open".into());
            summary.pr_url = Some(pr_url);
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
        }
        FixOutcome::Failed { failure } => {
            record_fix_failure(store, fid, job, &failure, tail, HoldReason::Stuck, summary).await;
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
            release_claim(store, fid).await;
            return Ok(());
        }
    }
    if finding.budget_override == Some(BudgetOverride::Once) {
        log_write_failure(
            store.set_budget_override(fid, None).await,
            format_args!("#{fid}: clearing one-shot budget override"),
        );
    }
    release_claim(store, fid).await;
    Ok(())
}

/// Run a fix job (`scheduler.run_fix`): start it ([`start_fix`]), run the
/// worker, then decide ([`conclude_fix`]) and record
/// ([`record_fix_outcome`]) what the attempt amounts to.
pub async fn run_fix(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let fid = finding.id;
    let start = match start_fix(store, cfg, finding, backend, resume).await? {
        Ok(start) => start,
        Err(ended) => return Ok(ended),
    };
    let prompt = fix_prompt(store, cfg, finding, &start, resume).await?;
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
            let ran = (FindingJobKind::Fix, fid, start.job);
            let denied = runner_failed(store, (cfg, backend, &start.repo.url), ran, &e).await;
            release_claim(store, fid).await;
            close_workspace(store, &start.ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (FindingJobKind::Fix, fid, start.job);
    if let Some(denied) = unmetered_offline(store, cfg, backend, &start.repo.url, ran, &rr).await {
        release_claim(store, fid).await;
        close_workspace(store, &start.ws).await;
        return Ok(denied);
    }
    let state = log_write_failure(
        record_job(store, start.job, &rr, model, resume).await,
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
    let outcome = conclude_fix(store, (cfg, backend), finding, &start, state).await?;
    record_fix_outcome(
        store,
        finding,
        &start,
        outcome,
        &rr.stdout_tail,
        &mut summary,
    )
    .await?;
    close_workspace(store, &start.ws).await;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// sync_prs helpers (`scheduler._iso_ms` … `scheduler._attention_fingerprint`)
// ---------------------------------------------------------------------------

/// ISO-8601 / RFC-3339 timestamp -> epoch ms (0 when absent/unparseable).
/// GitHub emits `2024-01-15T12:30:45Z`; GitLab may use `+00:00` suffix.
/// Handles both, plus the offset-less variant (treated as UTC).
#[allow(clippy::many_single_char_names)]
fn iso_ms(ts: &str) -> i64 {
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

/// Max of createdAt from comments + submittedAt from reviews -> epoch ms.
///
/// Over screened feedback only (`forge::screen_feedback`): a comment from
/// someone who cannot push, and is not a configured bot, never raises
/// `new_comments`, so it cannot start an engage run.
fn latest_activity_ms(pr: &PrView) -> i64 {
    let comment_stamps = pr.comments.iter().map(|c| iso_ms(&c.created_at));
    let review_stamps = pr.reviews.iter().map(|r| iso_ms(&r.submitted_at));
    comment_stamps.chain(review_stamps).max().unwrap_or(0)
}

/// (human summary, `any_failing`, sorted-unique failing check names).
fn checks_summary(rollup: &[GhCheckRun]) -> (Option<String>, bool, Vec<String>) {
    if rollup.is_empty() {
        return (None, false, Vec::new());
    }
    let named: Vec<(&str, CheckConclusion)> = rollup
        .iter()
        .map(|c| {
            let name = c.name.as_deref().or(c.context.as_deref()).unwrap_or("?");
            let raw = c.conclusion.as_deref().or(c.state.as_deref()).unwrap_or("");
            (name, CheckConclusion::parse(raw))
        })
        .collect();

    let mut failing_set: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut fail_count = 0usize;
    let mut pass_count = 0usize;
    for &(name, concl) in &named {
        if concl.is_failing() {
            failing_set.insert(name);
            fail_count += 1;
        } else if concl.is_passing() {
            pass_count += 1;
        }
    }
    let pending = named.len() - fail_count - pass_count;
    let failing_names: Vec<String> = failing_set.iter().map(|s| (*s).to_owned()).collect();

    let mut parts = vec![format!("{pass_count} pass")];
    if !failing_names.is_empty() {
        parts.push(format!("{fail_count} fail"));
    }
    if pending > 0 {
        parts.push(format!("{pending} pending"));
    }
    (
        Some(parts.join(" / ")),
        !failing_names.is_empty(),
        failing_names,
    )
}

/// Fingerprint of static (non-comment) attention reasons.
/// `review:CHANGES_REQUESTED` | mergeable:CONFLICTING | checks:name1,name2
/// None when nothing static is wrong.
fn attention_fingerprint(pr: &PrView, failing_names: &[String]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if pr.review_decision == ReviewDecision::ChangesRequested {
        parts.push("review:CHANGES_REQUESTED".to_owned());
    }
    if pr.mergeable == Mergeable::Conflicting {
        parts.push("mergeable:CONFLICTING".to_owned());
    }
    if !failing_names.is_empty() {
        parts.push(format!("checks:{}", failing_names.join(",")));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("|"))
    }
}

/// Sync PRs for all `pr_open` findings (`scheduler.sync_prs`).
#[allow(
    clippy::too_many_lines,
    reason = "one pass per open PR with a flat decision table over PR \
              state (merged, closed, review comments, checks); the arms \
              read as a table only while they sit together"
)]
pub async fn sync_prs(store: &Store, cfg: &Config) -> SyncResult {
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
                log_write_failure(
                    store.set_finding_status(fid, FindingStatus::Queued).await,
                    format_args!("#{fid}: status -> queued"),
                );
                let _ = store.log_event(
                    "fix", &format!("#{fid} requeued: pr_open with no pr_url (prior fix failed before PR creation)"),
                    None, Some(fid),
                ).await;
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
        let Some((_slug, pr_number)) = fg.parse_pr_url(url) else {
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
                log_write_failure(
                    store.set_finding_status(fid, FindingStatus::Merged).await,
                    format_args!("#{fid}: status -> merged"),
                );
                log_write_failure(
                    store.mark_pr_merged(fid, pr_number, now_ms()).await,
                    format_args!("#{fid}: recording PR #{pr_number} merged"),
                );
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
                // Record the closure first, and only then leave pr_open:
                // sync_prs only revisits pr_open findings, so a finding set
                // Closed while pr_state still reads OPEN would never be
                // looked at again. Kept pr_open, the next sync retries both.
                if let Err(err) = store.mark_pr_closed(fid, pr_number, now_ms()).await {
                    let _ = store
                        .log_event(
                            "error",
                            &format!(
                                "sync #{fid}: PR closed but not recorded: {err} \
                                 (the next sync retries)"
                            ),
                            None,
                            Some(fid),
                        )
                        .await;
                    summary.errors += 1;
                    continue;
                }
                log_write_failure(
                    store
                        .set_finding_verdict(
                            fid,
                            FindingStatus::Closed,
                            "PR closed without merge; awaiting harvest",
                        )
                        .await,
                    format_args!("#{fid}: verdict -> closed"),
                );
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

        // -- Open PR: full attention-flagging logic (open-PR branch of `scheduler.sync_prs`) --

        let prev = store.get_pr_state(fid).await.ok().flatten();
        let last_activity = latest_activity_ms(&pr);
        let (checks, failing, failing_names) = checks_summary(&pr.status_check_rollup);

        // Baseline the engaged watermark on first sync so we don't
        // flag our own PR-creation chatter.
        let engaged: i64 = if let Some(e) = prev.as_ref().and_then(|p| p.last_engaged_activity_at) {
            e
        } else {
            let updated_ms = iso_ms(&pr.updated_at);
            std::cmp::max(updated_ms, last_activity)
        };

        // Suppression: don't re-flag a static reason identical to the
        // one an engage cycle already declined (same fingerprint AND
        // same head_sha — a push resets suppression even if the static
        // snapshot looks identical).
        let fp = attention_fingerprint(&pr, &failing_names);
        let addressed_fp = prev
            .as_ref()
            .and_then(|p| p.addressed_fingerprint.as_deref());
        let addressed_sha = prev.as_ref().and_then(|p| p.addressed_head_sha.as_deref());
        let head_sha = &pr.head_sha;
        let suppressed = fp.is_some()
            && fp.as_deref() == addressed_fp
            && addressed_sha.is_some()
            && Some(head_sha.as_str()) == addressed_sha;

        let mut reasons: Vec<&str> = Vec::new();
        if last_activity > engaged {
            reasons.push("new_comments");
        }
        if !suppressed {
            if pr.review_decision == ReviewDecision::ChangesRequested {
                reasons.push("changes_requested");
            }
            if pr.mergeable == Mergeable::Conflicting {
                reasons.push("conflict");
            }
            if failing {
                reasons.push("checks_failing");
            }
        }
        let attention: Option<String> = if reasons.is_empty() {
            None
        } else {
            Some(reasons.join(","))
        };

        // attention_since fairness: only touch when the reason changes.
        let prev_attention = prev.as_ref().and_then(|p| p.needs_attention.as_deref());
        let mut attention_since_val: Option<Option<i64>> = None;
        if attention.as_deref() != prev_attention {
            attention_since_val = if attention.is_some() {
                Some(Some(now_ms()))
            } else {
                Some(None)
            };
        }
        let mut addr_fp: Option<Option<String>> = None;
        if addressed_fp.is_some()
            && (fp.as_deref() != addressed_fp || Some(head_sha.as_str()) != addressed_sha)
        {
            addr_fp = Some(None);
        }
        let clear_addressed = addr_fp.is_some();
        let data = SyncPrData {
            pr_number,
            state: pr.state.as_str().into(),
            mergeable: pr.mergeable.as_str().into(),
            checks,
            head_ref: pr.head_ref.clone(),
            head_sha: head_sha.clone(),
            last_activity_at: last_activity,
            last_engaged_activity_at: engaged,
            needs_attention: attention.clone(),
            attention_fingerprint: fp,
            synced_at: now_ms(),
            attention_since: attention_since_val,
            clear_addressed,
        };
        log_write_failure(
            store.sync_pr_open(fid, &data).await,
            format_args!("#{fid}: recording synced PR state"),
        );

        if attention.is_some() && attention.as_deref() != prev_attention {
            let _ = store
                .log_event(
                    "engage",
                    &format!(
                        "#{fid} PR #{pr_number} needs attention: {}",
                        attention.as_deref().unwrap_or("")
                    ),
                    None,
                    Some(fid),
                )
                .await;
        }
        summary.synced += 1;
        if attention.is_some() {
            summary.attention += 1;
        }
    }
    summary
}

/// The repo and PR an engage works on: its `pr_state` row, branch and
/// number, with the PR head fetched into the clone for a cold attempt.
/// Every way to lack one is logged and an error: sync records them first.
async fn engage_target(
    store: &Store,
    finding: &Finding,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<(Repo, crate::types::PrState, String, i64)> {
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
    let pr_number = if let Some(n) = ps.pr_number {
        n
    } else {
        let fg = forge::forge_for(repo.forge);
        if let Some((_slug, num)) = finding.pr_url.as_deref().and_then(|u| fg.parse_pr_url(u)) {
            tracing::warn!(
                finding_id = fid,
                pr_number = num,
                "self-healed missing pr_number from pr_url"
            );
            log_write_failure(
                store.set_pr_number(fid, num).await,
                format_args!("#{fid}: self-healed pr_number -> {num}"),
            );
            num
        } else {
            let _ = store
                .log_event(
                    "error",
                    &format!("engage #{fid}: no pr_number and no parseable pr_url"),
                    None,
                    Some(fid),
                )
                .await;
            anyhow::bail!("no pr_number");
        }
    };
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
    // Fetch the PR head into the clone for a cold attempt; the tree is
    // added from it once the job exists. Never on a resume: the chain
    // continues in its own tree, at the head it was created at.
    if resume.is_none() {
        let rps = repo.path.clone();
        let hr = head_ref.clone();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &rps, "fetch", "origin", &hr], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
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
    Ok((repo, ps, head_ref, pr_number))
}

/// An engage job with its PR read and a tree at the PR head.
struct EngageStart {
    repo: Repo,
    ps: crate::types::PrState,
    head_ref: String,
    pr_number: i64,
    /// The PR as the forge shows it, for the playbook: `None` on a resume,
    /// which sends no playbook and so does not ask the forge.
    pr: Option<PrView>,
    job: i64,
    ws: Workspace,
    pinned: String,
    cap: Option<i64>,
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

/// Everything before the worker runs: target, fetch, budget gate, PR view,
/// job row, tree. `Ok(Err(summary))` is a cycle that ends here.
async fn start_engage(
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<Result<EngageStart, CycleSummary>> {
    let fid = finding.id;
    let (repo, ps, head_ref, pr_number) = match engage_target(store, finding, resume).await {
        Ok(target) => target,
        Err(e) => {
            if let Some(denied) =
                engage_prefetch_failed(store, (cfg, backend), finding, &e.to_string()).await
            {
                return Ok(Err(denied));
            }
            return Err(e);
        }
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

    let pr = match engage_view(store, (cfg, backend), finding, &repo, pr_number, resume).await? {
        Ok(pr) => pr,
        Err(denied) => return Ok(Err(denied)),
    };

    let created = match store
        .create_job(
            FindingJobKind::Engage.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Engage.into(),
                Some(&repo.name),
                Some(fid),
                e,
            )
            .map(Err);
        }
    };
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
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
            return Ok(Err(count_unmade_engage_tree(
                store, fid, created.id, failed,
            )
            .await));
        }
    };
    Ok(Ok(EngageStart {
        repo,
        ps,
        head_ref,
        pr_number,
        pr,
        job: created.id,
        ws,
        pinned,
        cap,
    }))
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
    let (repo, job, pr_number) = (&start.repo, start.job, start.pr_number);
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
    let mut closed_on_forge = false;
    if let Err(err) = store.mark_pr_closed(fid, pr_number, now_ms()).await {
        // The harvest picks its playbook from pr_state: with the row still
        // OPEN it would run the merged-PR review, never ask why the PR
        // closed, and still mark the finding harvested. So no handoff, and
        // the finding stays pr_open rather than going Closed: Closed with
        // an OPEN pr_state is a state nothing else produces, and pr_open is
        // what sync_prs revisits. The PR is closed on the forge, so the
        // next cycle's sync takes its Closed branch, records both, and the
        // cold harvest follows with the right playbook.
        tracing::warn!(finding = fid, pr = pr_number, error = %err, "mark_pr_closed failed");
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
    } else if let Err(err) = store
        .set_finding_verdict(fid, FindingStatus::Closed, &reason_short)
        .await
    {
        // pr_state reads CLOSED but the finding is still pr_open, so no
        // handoff: the harvest would take pr_open for a human's verdict,
        // keep it, and mark the PR harvested, and the finding the next
        // sync sets `closed` would never be harvested. sync_prs revisits
        // pr_open findings, takes its Closed branch (mark_pr_closed is an
        // UPSERT, so the row already reading CLOSED is fine), sets the
        // finding `closed`, and the cold harvest follows.
        tracing::warn!(finding = fid, pr = pr_number, error = %err, "closed verdict failed");
        let _ = store
            .log_event(
                "error",
                &format!(
                    "#{fid} PR #{pr_number} withdrawn but the finding was not set closed: \
                     {err} (the next PR sync sets it)"
                ),
                Some(job),
                Some(fid),
            )
            .await;
    } else {
        closed_on_forge = forge_closed;
    }
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

/// Push the worker's commits to the PR branch and post its `PR-REPLY.md`:
/// `(pushed, replied)`, or why the engage is incomplete. A worker that did
/// not finish publishes nothing.
async fn push_and_reply(start: &EngageStart, state: JobState) -> Result<(bool, bool), String> {
    if state != JobState::Done {
        return Err(format!("worker {state}"));
    }
    let (repo, worktree, head_ref) = (&start.repo, &start.ws.tree, &start.head_ref);
    let fg = forge::forge_for(repo.forge);
    let wts = worktree.to_string_lossy().to_string();
    let hr = head_ref.clone();
    let (rc, commits) = tokio::task::spawn_blocking(move || {
        run_cmd_sync(
            &[
                "git",
                "-C",
                &wts,
                "log",
                &format!("origin/{hr}..HEAD"),
                "--oneline",
            ],
            30,
        )
    })
    .await
    .unwrap_or((127, String::new()));
    let mut pushed = false;
    if rc == 0 && !commits.trim().is_empty() {
        let push_url = fg.ssh_url(&repo.url);
        let wts = worktree.to_string_lossy().to_string();
        let hr = head_ref.clone();
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
        let lease = format!("--force-with-lease={hr}:{}", start.pinned);
        let (prc, pout) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(
                &[
                    "git",
                    "-C",
                    &wts,
                    "push",
                    &lease,
                    &push_url,
                    &format!("HEAD:{hr}"),
                ],
                600,
            )
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if prc != 0 {
            let tail = crate::util::tail(&pout, 300);
            return Err(format!("push failed: {tail}"));
        }
        pushed = true;
    }
    let reply = worktree.join("PR-REPLY.md");
    let mut replied = false;
    if reply.exists() {
        let body = std::fs::read_to_string(&reply).unwrap_or_default();
        if let Err(e) = fg.comment_pr(&repo.url, start.pr_number, &body) {
            return Err(format!(
                "PR comment failed: {}",
                crate::util::tail(&e.to_string(), 300)
            ));
        }
        replied = true;
    }
    Ok((pushed, replied))
}

/// Record a finished engage: what it published, or why it is incomplete.
/// Either way a one-shot budget override is spent.
async fn record_engage(
    store: &Store,
    (net, finding): ((&Config, &dyn Backend), &Finding),
    start: &EngageStart,
    state: JobState,
    published: Result<(bool, bool), String>,
    stdout_tail: &str,
    summary: &mut CycleSummary,
) {
    let (fid, job, ps, pr_number) = (finding.id, start.job, &start.ps, start.pr_number);
    let tail = crate::util::tail(stdout_tail, 300);
    match published {
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
            if state == JobState::Done {
                log_write_failure(
                    store.fail_job(job, fail.as_str()).await,
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
        Ok((pushed, replied)) => {
            let engaged_mark = if replied {
                now_ms() + 3_000
            } else {
                ps.last_activity_at.unwrap_or_else(now_ms)
            };
            let addressed_fp = if pushed {
                None
            } else {
                ps.attention_fingerprint.as_deref()
            };
            let addressed_sha = if pushed { None } else { ps.head_sha.as_deref() };
            log_write_failure(
                store
                    .mark_pr_engaged(fid, engaged_mark, now_ms(), addressed_fp, addressed_sha)
                    .await,
                format_args!("#{fid}: marking PR engaged"),
            );
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
            |plan| resume_prompt(FindingJobKind::Engage, plan),
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
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let fid = finding.id;
    remove_stale_report(resume)?;
    let start = match start_engage(store, cfg, finding, backend, resume).await? {
        Ok(start) => start,
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
            let ran = (FindingJobKind::Engage, fid, start.job);
            let denied = runner_failed(store, (cfg, backend, &start.repo.url), ran, &e).await;
            close_workspace(store, &start.ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (FindingJobKind::Engage, fid, start.job);
    if let Some(denied) = unmetered_offline(store, cfg, backend, &start.repo.url, ran, &rr).await {
        close_workspace(store, &start.ws).await;
        return Ok(denied);
    }
    let state = log_write_failure(
        record_job(store, start.job, &rr, model, resume).await,
        format_args!("job {}: recording outcome", start.job),
    )
    .unwrap_or(JobState::Failed);
    let mut summary = CycleSummary {
        kind: Some(FindingJobKind::Engage.into()),
        finding_id: Some(fid),
        job_id: Some(start.job),
        state: Some(state),
        pr_number: Some(start.pr_number),
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
        engage_blocked(store, fid, &start, &report, &mut summary).await?;
        false
    } else {
        let published = push_and_reply(&start, state).await;
        record_engage(
            store,
            ((cfg, backend), finding),
            &start,
            state,
            published,
            &rr.stdout_tail,
            &mut summary,
        )
        .await;
        false
    };
    if closed_on_forge {
        continue_into_harvest(
            store,
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
    close_workspace(store, &start.ws).await;
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
        close_workspace(store, &start.ws).await;
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
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    repo: &Repo,
    fid: i64,
    engage_job: i64,
    ws: &Workspace,
    pinned: &str,
    session_file: Option<&str>,
) {
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
    if let Err(e) = run_harvest(store, cfg, &finding, backend, Some(&plan)).await {
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
    store: &Store,
    fid: i64,
    (job, state): (i64, JobState),
    pr_number: i64,
    ws: &Workspace,
    report: &str,
) -> anyhow::Result<CycleSummary> {
    let held = hold_on_report(store, FindingJobKind::Harvest, fid, job, ws, report).await;
    close_workspace(store, ws).await;
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
    store: &Store,
    cfg: &Config,
    finding: &Finding,
    backend: &dyn Backend,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
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
                store.set_pr_number(fid, num).await,
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
    let rpath = PathBuf::from(&repo.path);
    // A handoff starts a new job, so it gets what a cold one gets from
    // here on: a fresh default branch to compare against, the diff and
    // the full playbook. Only its tree and transcript are inherited.
    let fresh = resume.is_none_or(|p| p.handoff);
    let handoff = resume.is_some_and(|p| p.handoff);
    // Fetch the default branch into the clone for a fresh attempt; a cold
    // tree is added at it once the job exists, and a handoff's worker
    // reads it as `origin/<default>`. Never on a resume: the chain
    // continues in its own tree.
    if fresh {
        let db = repo.default_branch.clone();
        let rps = rpath.to_string_lossy().to_string();
        let (rc, out) = tokio::task::spawn_blocking(move || {
            run_cmd_sync(&["git", "-C", &rps, "fetch", "origin", &db], 600)
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            let tail = crate::util::tail(&out, 300);
            let what = format!("fetch {}", repo.default_branch);
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

    let created = match store
        .create_job(
            FindingJobKind::Harvest.into(),
            repo.id,
            Some(fid),
            cap,
            JobState::Running,
            Some(anticipated),
            resume.map(|r| r.predecessor_id),
        )
        .await
    {
        Ok(j) => j,
        Err(e) => {
            return job_refused(
                FindingJobKind::Harvest.into(),
                Some(&repo.name),
                Some(fid),
                e,
            );
        }
    };
    let job = created.id;
    let (ws, pinned) = match open_workspace(
        store,
        cfg,
        &repo,
        &created,
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
            |plan| resume_prompt(FindingJobKind::Harvest, plan),
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
                    store
                        .fail_unstarted_job(job, &format!("{RUNNER_ERROR}: {e}"))
                        .await,
                    format_args!("job {job}: failing unstarted"),
                );
                None
            } else {
                let ran = (FindingJobKind::Harvest, fid, job);
                runner_failed(store, (cfg, backend, &repo.url), ran, &e).await
            };
            close_workspace(store, &ws).await;
            return denied.ok_or(e);
        }
    };
    let ran = (FindingJobKind::Harvest, fid, job);
    if let Some(denied) = unmetered_offline(store, cfg, backend, &repo.url, ran, &rr).await {
        close_workspace(store, &ws).await;
        return Ok(denied);
    }
    let state = log_write_failure(
        record_job(store, job, &rr, model, resume).await,
        format_args!("job {job}: recording outcome"),
    )
    .unwrap_or(JobState::Failed);

    if let Some(report) = blocked_report(&worktree)? {
        return harvest_blocked(store, fid, (job, state), pr_number, &ws, &report).await;
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
    close_workspace(store, &ws).await;

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
        let status = verdict.class.status();
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
            .record_closed_harvest(fid, status, &verdict.reason, anchor.as_ref(), now_ms())
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

/// A finding's worker that failed without doing any metered work, while a
/// host its work needs is unreachable: it never got to the work, so its
/// failure is not counted. Its job is closed `failed` as unstarted, which
/// leaves the checkpoint it was continuing in place, and the cycle ends as
/// [`offline_denial`]. `None` for any other run: one that did metered work
/// failed at the work, and is counted as before.
async fn unmetered_offline(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    repo_url: &str,
    (kind, fid, job): (FindingJobKind, i64, i64),
    rr: &RunResult,
) -> Option<CycleSummary> {
    if !matches!(job_state(rr), JobState::Failed | JobState::Killed) || rr.tokens_new > 0 {
        return None;
    }
    let host = unreachable_now(cfg, repo_url, backend).await?;
    log_write_failure(
        store
            .fail_unstarted_job(
                job,
                &format!("offline: {host} unreachable; the worker did no metered work"),
            )
            .await,
        format_args!("job {job}: failing unstarted"),
    );
    Some(offline_denial(store, kind.into(), Some(fid), Some(job), host).await)
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
pub async fn run_cycle(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> CycleSummary {
    // Catch-all
    match run_cycle_inner(store, cfg, backend, force_repo).await {
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
    }
}

/// Whether a rotation scan's attempt ran to an end — done, killed or
/// failed — and so took that scan's turn. (One that crashed is an `Err`
/// from its runner, which the cycle bumps on its own.)
///
/// Whether it succeeded does not matter here: a clean success has its
/// runner advance `last_{kind}_at` to now already, so bumping it again
/// changes nothing. Suspended and denied attempts did not end: the first
/// is continued by the resume tier, the second never started.
fn attempt_ended(result: &CycleSummary) -> bool {
    matches!(
        result.state,
        Some(JobState::Done | JobState::Killed | JobState::Failed)
    )
}

/// Run the picked candidate through its kind's executor.
///
/// Exhaustive dispatch on Candidate variant and sub-kind.
async fn dispatch(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    candidate: &Candidate,
) -> anyhow::Result<CycleSummary> {
    match candidate {
        Candidate::Finding {
            kind, finding_id, ..
        } => run_finding_job(store, cfg, backend, *kind, *finding_id, None).await,
        Candidate::Repo { kind, repo_id, .. } => {
            run_repo_job(store, cfg, backend, *kind, *repo_id, None).await
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
                    run_repo_job(store, cfg, backend, kind, plan.repo_id, resume).await
                }
                JobKind::Finding(kind) => {
                    // A finding-kind job always recorded its target, so a
                    // row without one is corrupt rather than merely odd.
                    let fid = plan
                        .finding_id
                        .ok_or_else(|| anyhow::anyhow!("resume of a {kind} job with no finding"))?;
                    run_finding_job(store, cfg, backend, kind, fid, resume).await
                }
            }
        }
    }
}

async fn run_repo_job(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    kind: RepoJobKind,
    repo_id: i64,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let repo = store
        .get_repo_by_id(repo_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("repo {repo_id} not found"))?;
    match kind {
        RepoJobKind::Hunt => run_hunt(store, cfg, &repo, backend, resume).await,
        RepoJobKind::TestGap => run_test_gap(store, cfg, &repo, backend, resume).await,
        RepoJobKind::DepUpdate => run_dep_update(store, cfg, &repo, backend, resume).await,
        RepoJobKind::Refactor => run_refactor(store, cfg, &repo, backend, resume).await,
        RepoJobKind::Modernization => run_modernize(store, cfg, &repo, backend, resume).await,
        RepoJobKind::Standards => run_standards(store, cfg, &repo, backend, resume).await,
    }
}

async fn run_finding_job(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    kind: FindingJobKind,
    finding_id: i64,
    resume: Option<&ResumePlan>,
) -> anyhow::Result<CycleSummary> {
    let finding = store
        .get_finding(finding_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("finding {finding_id} not found"))?;
    match kind {
        FindingJobKind::Engage => run_engage(store, cfg, &finding, backend, resume).await,
        FindingJobKind::Harvest => run_harvest(store, cfg, &finding, backend, resume).await,
        FindingJobKind::Recheck => run_recheck(store, cfg, &finding, backend, resume).await,
        FindingJobKind::Fix => run_fix(store, cfg, &finding, backend, resume).await,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the scheduler's priority ladder: each tier is an ordered \
              `if` that must be read against the tiers above and below it, \
              which is exactly what extracting them into helpers destroys"
)]
async fn run_cycle_inner(
    store: &Store,
    cfg: &Config,
    backend: &dyn Backend,
    force_repo: Option<&str>,
) -> anyhow::Result<CycleSummary> {
    // (0) Cheap PR sync
    let has_pr_open = !store
        .list_findings(&FindingFilter {
            status: Some(FindingStatus::PrOpen),
            ..FindingFilter::default()
        })
        .await?
        .is_empty();
    let sync = if has_pr_open {
        Some(sync_prs(store, cfg).await)
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

    let dispatched = dispatch(store, cfg, backend, &candidate).await;
    // Starvation prevention for rotation kinds: a scan whose attempt ended
    // has its `last_{kind}_at` bumped, so it is retried after its interval
    // instead of being re-picked at once. The runners only advance that
    // timestamp (and, for hunt, the `last_hunt_sha` watermark) on a clean
    // success — done, an output file, no invalid entry — so without this
    // an attempt that keeps failing would be the stalest work in the
    // rotation forever: re-run every cycle, and every other repo's scans
    // starved behind it. The bump records the attempt, not success; the
    // hunt watermark stays where it was, so the retry reviews the same
    // commits again.
    //
    // Suspended is deliberately absent. A cap kill is a pause, and
    // re-selecting that work is no longer this bump's job: the resume
    // tier claims it by id at a priority above rotation. Were it bumped
    // here it would be counted as a turn taken, while the work itself had
    // not finished. A chain that never finishes is retired by the give-up
    // ceiling, which bumps the timestamp itself (`resume_plan`): that is
    // where the chain's turn ends.
    //
    // A resumed attempt counts exactly like a fresh one. When it ends
    // killed or failed the chain is over and the resume tier has nothing
    // left to claim, so without the bump rotation would start the same
    // work fresh.
    let rotation = match candidate.job_kind() {
        JobKind::Repo(kind) => Some((candidate.repo_id(), kind)),
        JobKind::Finding(_) => None,
    };
    let mut result = match dispatched {
        Ok(result) => result,
        Err(e) => {
            if let Some((repo_id, kind)) = rotation {
                log_write_failure(
                    store.set_last_kind_at(repo_id, kind).await,
                    format_args!("repo {repo_id}: recording last {kind} run"),
                );
            }
            return Err(e);
        }
    };
    if let Some((repo_id, kind)) = rotation
        && attempt_ended(&result)
    {
        log_write_failure(
            store.set_last_kind_at(repo_id, kind).await,
            format_args!("repo {repo_id}: recording last {kind} run"),
        );
    }

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
