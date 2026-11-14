//! The pre-PR review of a finished fix (WORK-MODEL.md §5.7).
//!
//! A fixer that finished with commits and a `PR-DESCRIPTION.md` is not
//! shipped at once. A fresh reviewer session, told to distrust the change,
//! reads the whole `<pinned>..HEAD` and writes its issues to a file outside
//! the tree. Its issues go back to the fixer, resumed in its own session,
//! which fixes or rebuts each, answering each by its id in a file of its
//! own ([`ANSWERS_FILE`]); then the whole change is reviewed again, the
//! issues still open shown with the fixer's answers and judged by id, as a
//! PR review's earlier issues are ([`parse_verdicts`]), until a review
//! raises none and leaves none open or `review.maxRounds` reviews have run.
//! Issues outside the change's scope (`follow-up`s) are never answered:
//! they ship in the PR body for later work. Replayed over 88 shipped
//! fixes, such a reviewer named most of the bugs later hunts traced back
//! to hunter's own PRs, and flagged few clean ones.
//!
//! All of it runs inside the fix job: the finding stays `fixing` under the
//! same claim, and the job row records every run's spend under the one
//! budget grant. The reviewer never holds the fix hostage: a review that
//! cannot be had ships the fix as before, saying so in the PR body. Every
//! round, its issues and verdicts, and the fixer's answers are recorded in
//! the chain's `fix` review ([`record_round`], [`record_answers`]), which
//! the finding's detail shows. Which issues are open, and so what goes back
//! to the fixer and what holds the fix, is read from there
//! ([`Rounds::open`]); the PR body's account of the review is rendered from
//! it too ([`review_summary`]).
//!
//! The fix job moves through its steps ([`Step`]): the fixer implements
//! the fix, each review round is had, the fixer answers what a round left
//! open, the curator rewrites the reviewed history, and the fix is
//! published. Where the job is, and what the steps carry from one attempt
//! to the next, is one record on its work item ([`StepRecord`]), written
//! as each step completes and with the attempt's end: whatever stops an
//! attempt, the next one runs the step it stopped at.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{BLOCKED_REPORT, FixStart, Outage, decline_report, fix_event, job_state};
use crate::backend::{AUTH_UNAVAILABLE, Backend, JobClass};
use crate::clones::{Lane, Unheld};
use crate::config::Config;
use crate::domain::{FinishedState, HoldReason};
use crate::playbooks::{self, ReviewOutputs};
use crate::recovery::CycleScope;
use crate::store::{
    AnswerKind, ChainIssue, FixReview, FixReviewRound, IssueAnswer, IssueVerdict, JobStore,
    ReportedIssue, Store, Verdict,
};
use crate::types::{JobPhase, RunResult};
use crate::workspace::Workspace;

/// The `killed_reason` of a fix job the daemon's shutdown stopped between
/// two of its runs: a pause on the fixer's transcript (`job_state`), and
/// no resume attempt (`Store::resume_chain_stats`).
pub(super) const STOPPED: &str = "shutdown";

/// The `killed_reason` of a fix job whose grant ran out between two of its
/// runs ([`Rounds::spent`]): the job's cap killed it, as it kills a run,
/// and its resume carries on under a fresh grant.
const SPENT: &str = "cap";

/// In the workspace root: the fixer's answers to the issues it was sent
/// (`review_followup.md`), one per issue id, written as it goes. Read into
/// the chain's review ([`record_answers`]) once the fixer's run ends, and
/// by the next attempt when the run that wrote it was cut short; removed
/// once read, and before the fixer is sent issues, so no answer is read
/// twice for another round.
const ANSWERS_FILE: &str = "review-answers.json";

/// Where a fix job is: the part of the fix its next run is ([`StepRecord`]).
/// Each part that completes moves the job on to the next before the next
/// runs; a stop of any kind (a cap, the wall clock, the daemon's shutdown
/// or death, a host out of reach, a grant spent) leaves it where it was,
/// and the attempt that continues it runs that part.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub(super) enum Step {
    /// The fixer: its playbook on a cold start, its transcript continued on
    /// a resume, the prompt for the hold an operator released after one.
    #[default]
    Implement,
    /// Review round `.0` of the pass: a fresh reviewer, and nothing else.
    /// The pass's own count, which bounds it at `review.maxRounds`; people
    /// are shown the chain's count ([`shown_round`]).
    Review(u32),
    /// The fixer's answer to the issues the review left open after round
    /// `.0` of the pass: 0 for those a hold held, after which a new pass
    /// starts at its round 1.
    Answer(u32),
    /// The curator, the review being through.
    Curate,
    /// The push and the PR, on the review's verdict: no model runs.
    Publish,
}

impl std::fmt::Display for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Implement => f.write_str("implement"),
            Self::Review(round) => write!(f, "review {round}"),
            Self::Answer(round) => write!(f, "answer {round}"),
            Self::Curate => f.write_str("curate"),
            Self::Publish => f.write_str("publish"),
        }
    }
}

/// A step as its record keeps it: `implement`, `review N`, `answer N`,
/// `curate` or `publish`, which is also the job's `ended_step`.
impl From<Step> for String {
    fn from(step: Step) -> Self {
        step.to_string()
    }
}

impl TryFrom<String> for Step {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        let round = |n: &str| n.parse::<u32>().map_err(|e| format!("step {text:?}: {e}"));
        match (text.as_str(), text.split_once(' ')) {
            ("implement", _) => Ok(Self::Implement),
            ("curate", _) => Ok(Self::Curate),
            ("publish", _) => Ok(Self::Publish),
            (_, Some(("review", n))) => round(n).map(Self::Review),
            (_, Some(("answer", n))) => round(n).map(Self::Answer),
            _ => Err(format!("no such step: {text:?}")),
        }
    }
}

/// What a fix job keeps from one attempt to the next, on its work item
/// (`finding_work.step`, JSON): the step its next attempt runs, and what
/// the steps carry over. Which issues are open is not here: the chain's
/// review has them ([`Rounds::open`]). Written as each step completes,
/// before the next one runs ([`Rounds::go`]), and with the attempt's end,
/// in the transaction that records it (`Store::complete_job`, a hold, an
/// attempt kept to publish); gone with the checkpoint, when the next
/// attempt starts cold. None is a cold start: [`Step::Implement`], nothing
/// kept.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct StepRecord {
    step: Step,
    /// The follow-ups the rounds so far raised ([`merge_follow_ups`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    follow_ups: Vec<ReportedIssue>,
    /// The verdict the review reached, and the tree it covers: what
    /// [`Step::Curate`] curates and [`Step::Publish`] ships, as long as the
    /// tree is exactly so.
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<Saved>,
    /// The tree an agent (a reviewer, or a curator) was shown, and which
    /// it may have changed: set before it runs, and cleared once its
    /// changes are undone or accepted. Left set (the daemon died while it
    /// ran, or its changes could not be undone), every later attempt puts
    /// the tree back to it before anything runs ([`put_back`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    rewrite: Option<ReviewedTree>,
    /// The round a hold stopped after ([`Rounds::hold`]), as people are
    /// shown it (numbered among the chain's rounds, [`shown_round`]), while
    /// the job is at [`Step::Answer`] 0 on the issues it held: what the
    /// events and a hold made again name them by. Cleared once the job
    /// moves on.
    #[serde(skip_serializing_if = "Option::is_none")]
    held_round: Option<u32>,
    /// Set before the fixer first runs on the issues of a hold an operator
    /// released ([`Rounds::release`]): from then on a report in the tree
    /// is the fixer's, not the hold's ([`Self::holding`]). Cleared with
    /// `held_round`, and by a hold made again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    released: bool,
    /// Set by [`Self::reopen`], an operator's `continue` of a publish the
    /// forge kept refusing: the fixer the reopened implementation runs is
    /// told only to publish (`scheduler::publish_only_prompt`), however
    /// many attempts it takes to start. Cleared once it has started
    /// ([`Self::fixer_started`]), or the fix starts over: the `continue`
    /// of a later hold is told what that hold was.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    reopened: bool,
}

impl StepRecord {
    /// The record `json` holds, as `finding_work.step` keeps it; `None` is
    /// a cold start.
    pub(super) fn read(json: Option<&str>) -> Result<Self, String> {
        json.map_or_else(
            || Ok(Self::default()),
            |json| serde_json::from_str(json).map_err(|e| e.to_string()),
        )
    }

    /// The step the next attempt runs.
    pub(super) const fn step(&self) -> Step {
        self.step
    }

    /// The record as `finding_work.step` keeps it. Every field serializes,
    /// so the fallback, a record no different from none, is never written.
    pub(super) fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// Whether the fix is past its implementation: the fixer finished it,
    /// and its review, curation or publish is what remains.
    pub(super) fn implemented(&self) -> bool {
        self.step != Step::Implement
    }

    /// Whether the record is where the review's own hold leaves it
    /// ([`Rounds::hold`]): at [`Step::Answer`] 0, with the round it stopped
    /// at, and no fixer run on its issues since an operator released it
    /// ([`Self::released`]). Only there is a report in the tree the
    /// review's.
    fn holding(&self) -> bool {
        self.step == Step::Answer(0) && self.held_round.is_some() && !self.released
    }

    /// Whether the implementation is one an operator's `continue` of a
    /// refused publish reopened ([`Self::reopen`]), not yet finished: its
    /// fixer is to publish, changing only what the forge's answer names.
    pub(super) const fn reopened(&self) -> bool {
        self.reopened
    }

    /// Start the fix over at [`Step::Implement`], as a report that
    /// concludes it does, keeping the follow-ups and any tree
    /// still to put back, and no verdict, nor a reopened publish. The
    /// review's open issues are the chain's, not the record's: they carry
    /// over to the new pass.
    fn restart(&mut self) {
        self.step = Step::Implement;
        self.verdict = None;
        self.reopened = false;
        self.held_round = None;
        self.released = false;
    }

    /// Take the fix back to [`Step::Implement`] for the fixer to run again,
    /// as an operator's `continue` of a publish the forge kept refusing
    /// does (`run_fix`), keeping everything else: the follow-ups, and the
    /// verdict, which still ships the tree when the fixer leaves it as it
    /// was ([`implemented`]). Not one a curation is still owed on (its
    /// rewrite could not be undone, and the commit it pinned was refused):
    /// that covers the uncurated tree and would ship it uncurated, so the
    /// fix is reviewed and curated again. The record says it was
    /// reopened ([`Self::reopened`]) until its fixer starts, so an attempt
    /// that fails before then leaves the next one to tell the fixer the
    /// same.
    pub(super) fn reopen(&mut self) {
        if self.step == Step::Curate {
            self.verdict = None;
        }
        self.step = Step::Implement;
        self.reopened = true;
    }

    /// The implementation's fixer has started (`run_fix`): a reopened one
    /// was told to publish, and its attempt's end records the reopening
    /// done ([`Self::reopened`]).
    pub(super) fn fixer_started(&mut self) {
        self.reopened = false;
    }
}

/// The step record `start`'s attempt begins at. A cold start begins
/// afresh, at [`Step::Implement`], and records so, so nothing a dropped
/// earlier attempt of the work recorded outlives it. A resume continues
/// the work item's ([`Store::fix_step`]); one that cannot be read is
/// logged, and the fix starts over at [`Step::Implement`], on the fixer's
/// transcript.
pub(super) async fn begin(
    store: &JobStore,
    start: &FixStart,
    resumed: bool,
) -> anyhow::Result<StepRecord> {
    let fid = start.finding.id;
    if !resumed {
        let fresh = StepRecord::default();
        store.set_fix_step(fid, &fresh.json()).await?;
        return Ok(fresh);
    }
    let json = store.fix_step(fid).await?;
    Ok(match StepRecord::read(json.as_deref()) {
        Ok(record) => record,
        Err(why) => {
            let message =
                format!("#{fid} the fix's step record could not be read ({why}); implementing");
            let _ = fix_event(store, "fix", fid, start.job, message).await;
            StepRecord::default()
        }
    })
}

/// Whether the next attempt of a fix at `record`, in `ws`, has only to
/// publish, running no model: it is at [`Step::Publish`], no tree is to be
/// put back, and its verdict covers the tree exactly as it is.
pub(super) async fn publishes_as_reviewed(record: &StepRecord, ws: &Workspace) -> bool {
    let Some(saved) = record.verdict.as_ref() else {
        return false;
    };
    record.step == Step::Publish
        && record.rewrite.is_none()
        && covers(saved, &tree_state(&ws.tree).await)
}

/// A reviewed tree to put back to ([`undo`]), and the fixer's description:
/// the one a reviewer or a curator was shown ([`StepRecord::rewrite`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewedTree {
    before: TreeState,
    description: Vec<u8>,
}

/// What the review loop leaves for the job's record and its PR.
pub(super) struct Reviewed {
    /// Every run of the job, summed: tokens, calls and usage. The
    /// exit, kill and output tail are those of the fixer's last run, which
    /// decides the job's state ([`STOPPED`] when shutdown stopped the loop);
    /// the session file stays the fixer's.
    pub rr: RunResult,
    /// What to ship with, once the tree may ship: the review reached its
    /// verdict, a verdict covers the tree as it is, or the review is off.
    /// `None` otherwise -- the fixer did not finish (a pause, a failure,
    /// half an answer to a review), left nothing to ship, or the job was
    /// suspended -- and then nothing ships.
    pub ending: Option<Ending>,
    /// A run cut short by an outage ([`Outage`]): one that did no work
    /// while a host is unreachable or workers get no credentials, or one
    /// refused its credentials. The machine's failure, not the fix's: the
    /// attempt is kept for a retry of its step (`FixOutcome::Offline`), or,
    /// when it ran nothing at all, closed as never started (`run_fix`).
    pub offline: Option<Outage>,
    /// The attempt fails, shipping nothing, for this reason: the review
    /// could not tell which issues are open, so it can neither ship past
    /// them nor hold on them, or could not record the fixer's answers, so
    /// the next reviewer could not judge them (`FixOutcome::Failed`,
    /// counted in the finding's failure streak like any other failure).
    /// The attempt is kept as the checkpoint with its end
    /// ([`Self::kept`]), the record at the step the retry runs again, in
    /// the same tree and review: a streak that holds the fix holds it there.
    pub failure: Option<String>,
    /// Where the job is as the attempt ends: what its end records for the
    /// next attempt ([`Self::step`]).
    pub record: StepRecord,
    /// A report in the tree that concludes the attempt with the record as
    /// it is: the review's own hold ([`Rounds::hold`]), whether written in
    /// this attempt or found unrecorded at its start. Any other report
    /// starts the fix over ([`Self::concluded_on_report`]).
    held: bool,
}

impl Reviewed {
    /// The step record the attempt ended with, as the store keeps it.
    pub(super) fn step(&self) -> String {
        self.record.json()
    }

    /// The attempt ends about to publish: its run finished, and it ships
    /// ([`Self::ending`]). Its end keeps it as the checkpoint, at its step
    /// record (`JobOutcome::kept`), so a daemon that dies while it pushes
    /// or opens the PR leaves the next attempt that step, not the fix to
    /// redo: at [`Step::Publish`], the publish, running no model; with a
    /// commit pinned in place of `HEAD` (a reviewer's or a curator's
    /// changes that could not be undone), the tree put back and that
    /// review or curation again ([`finish`], [`curation`]).
    pub(super) fn publishing(&self) -> bool {
        self.ending.is_some() && job_state(&self.rr) == FinishedState::Done
    }

    /// The attempt's end keeps it as the checkpoint, at its step record
    /// (`JobOutcome::kept`): it is about to publish ([`Self::publishing`]),
    /// or its review failed short of a verdict ([`Self::failure`]), and
    /// its retry runs that step again rather than the fix from the start.
    pub(super) fn kept(&self) -> bool {
        self.publishing() || self.failure.is_some()
    }

    /// The attempt concludes on a report (`BLOCKED.md`, a decline): unless
    /// it is the review's own hold, the fix starts over at
    /// [`Step::Implement`], keeping the follow-ups. However
    /// the report came to be (the fixer's first run, an answer, an answer
    /// cut off at its cap), the operator's `continue` of its hold is
    /// reviewed in a new pass from its round 1. The chain's issues still
    /// open carry over: that round is shown them, and one it leaves open
    /// goes back to the fixer. Its rounds go on counting the chain's
    /// rounds before the report ([`shown_round`]).
    pub(super) fn concluded_on_report(&mut self) {
        if !self.held {
            self.record.restart();
        }
    }
}

/// How the review loop ended: what `ship_pr` ships.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Ending {
    /// Appended to the PR body: unresolved issues, or why the review
    /// failed. `None` for a clean review, or no review.
    pub note: Option<String>,
    /// The commit to ship in place of `HEAD`: set when a reviewer or a
    /// curator changed the tree and it could not be put back, so what
    /// ships is what was reviewed, not that agent's commits.
    pub head: Option<String>,
}

/// The verdict the review reached ([`StepRecord::verdict`]): the PR body's
/// note, for the tree it covers.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    note: Option<String>,
    tree: TreeState,
}

/// Whether `saved` covers the tree as it is `now`: its commit, branch,
/// uncommitted state and description are exactly so.
fn covers(saved: &Saved, now: &TreeState) -> bool {
    saved.tree.head.is_some() && saved.tree == *now
}

/// One review round's verdict.
enum RoundVerdict {
    /// No issue to answer: none, or only follow-ups.
    Clean,
    /// The reviewer's issues, as it reported them, blocking ones first.
    /// `undone`: the reviewer also changed the worktree, and that was
    /// undone ([`undo`]), so the tree is the one it reviewed and its issues
    /// stand.
    Issues {
        issues: Vec<ReportedIssue>,
        undone: bool,
    },
    /// No usable review this round. `issues`: entries the reviewer wrote before it changed a tree that could not be put back;
    /// they ship unresolved.
    Failed {
        why: String,
        /// The commit the reviewer was shown, when its changes could not
        /// be undone: the one that ships, and the one every later attempt
        /// puts the tree back to before anything runs
        /// ([`StepRecord::rewrite`]).
        head: Option<String>,
        issues: Vec<ReportedIssue>,
    },
}

/// The PR body line saying the reviewer of round `shown` changed the
/// worktree.
fn undone_note(shown: u32) -> String {
    format!(
        "The reviewer of round {shown} changed the worktree; its changes were undone, and its \
         issues stand."
    )
}

/// The number people are shown for the round that runs after the chain's
/// `kept` rounds (`ok` or `partial`, [`FixReview::rounds`]): the PR body,
/// the hold reports and the events count every round the chain kept, so a
/// round's number matches its issues' ([`ChainIssue::round`]) and the
/// review summary's count. The loop's own round ([`Step::Review`]), which
/// starts over at 1 with every pass, only bounds a pass at
/// `review.maxRounds`.
fn shown_round(kept: usize) -> u32 {
    u32::try_from(kept).unwrap_or(u32::MAX).saturating_add(1)
}

/// What the loop's runs share: where they run and what bounds them.
struct Rounds<'a> {
    scope: &'a CycleScope<'a>,
    cfg: &'a Config,
    backend: &'a dyn Backend,
    start: &'a FixStart,
}

impl Rounds<'_> {
    fn store(&self) -> &JobStore {
        self.scope.store()
    }

    /// Record `job`'s step record as where its work item continues from
    /// (`Store::set_fix_step`).
    async fn save(&self, job: &Reviewed) -> sqlx::Result<()> {
        self.store()
            .set_fix_step(self.start.finding.id, &job.record.json())
            .await
    }

    /// Move `job` on to `step`, recorded before its next run starts, so an
    /// attempt the daemon's death cuts off resumes there. A write that
    /// fails is logged: the attempt goes on, and its end records where it
    /// got to.
    async fn go(&self, job: &mut Reviewed, step: Step) {
        job.record.step = step;
        if step != Step::Answer(0) {
            job.record.held_round = None;
            job.record.released = false;
        }
        if let Err(e) = self.save(job).await {
            let fid = self.start.finding.id;
            tracing::warn!("fix #{fid}: recording step {step}: {e}");
        }
    }

    /// Record, before the fixer runs on the issues a hold held (at
    /// [`Step::Answer`] 0), that the hold was released
    /// ([`StepRecord::released`]): a report the daemon's death leaves
    /// from that run is then the fixer's. Unlike [`Self::go`]'s, a write
    /// that fails stops the fixer: should the daemon die in its run, the
    /// record would take its report for the hold's. `Err` says why; the
    /// record is left holding, for the attempt to fail kept at this step
    /// ([`Reviewed::failure`]) and its retry to release the hold again.
    async fn release(&self, job: &mut Reviewed) -> Result<(), String> {
        if !job.record.holding() {
            return Ok(());
        }
        job.record.released = true;
        self.save(job).await.map_err(|e| {
            job.record.released = false;
            format!("the released hold could not be recorded: {e}")
        })
    }

    /// The chain's review as the store has it ([`Store::fix_review`]), or
    /// why it could not be read.
    async fn review(&self) -> Result<FixReview, String> {
        self.store()
            .fix_review(chain(self.start))
            .await
            .map_err(|e| format!("the review's record could not be read: {e}"))
    }

    /// The issues the fixer owes an answer: those of the chain's review no
    /// round settled ([`FixReview::open`]), blocking ones first. What goes
    /// back to the fixer, and what a hold holds on, is always this set:
    /// the round's new issues, and earlier ones a round judged `open` or
    /// did not judge at all.
    async fn open(&self) -> Result<Vec<ChainIssue>, String> {
        Ok(self.review().await?.open())
    }

    /// The tokens and the wall time the next run may have, or why there
    /// are none left: what is left of the job's grant after `spent`, and
    /// a whole `fix.maxWallS` of its own. The loop as a whole has no wall
    /// limit: `review.maxRounds` and the grant bound it, and one run's
    /// limit bounds how long a stop waits.
    fn allowance(&self, spent: &RunResult) -> Result<(Option<i64>, i64), &'static str> {
        Ok((remaining(self.start.cap, spent)?, self.cfg.fix_max_wall_s))
    }

    /// Stop the job for the daemon's shutdown, before another run starts
    /// ([`Self::suspend`]); `what` names the step for the event.
    async fn stopped(&self, job: &mut Reviewed, what: &str) -> bool {
        if !self.scope.stopping() {
            return false;
        }
        let stop = format!("{what}: the daemon is stopping; suspended");
        self.suspend(job, STOPPED, &stop).await
    }

    /// Suspend the job when its grant is spent before the review is
    /// through (`why`): before a run could start, or by a reviewer it
    /// killed ([`Self::suspend`], as a cap kill). The resume, under a fresh
    /// grant, runs the step this one stopped at: a review cut short is
    /// never shipped as one. `what` names the step for the event.
    async fn spent(&self, job: &mut Reviewed, what: &str, why: &str) -> bool {
        let stop = format!("{what}: {why}; suspended to go on under a fresh grant");
        self.suspend(job, SPENT, &stop).await
    }

    /// Suspend the job between two of its runs, the fixer's last one
    /// finished, as `reason` (its `killed_reason`, which `job_state` makes
    /// a suspension), logging `stop`. The job's record stays at the step it
    /// is at, and its resume runs that step. Only with a transcript to
    /// resume; without one, `false`, and the loop goes on as if there were
    /// no stop.
    async fn suspend(&self, job: &mut Reviewed, reason: &str, stop: &str) -> bool {
        if job.rr.session_file.is_none() {
            return false;
        }
        job.rr.killed_reason = Some(reason.to_owned());
        job.rr.exit_code = None;
        let fid = self.start.finding.id;
        let message = format!("#{fid} {stop}");
        let _ = fix_event(self.store(), "review", fid, self.start.job, message).await;
        true
    }

    /// Suspend the job before its curation (`why`), the review through and
    /// its verdict recorded, so its resume curates and ships without
    /// another review. The daemon's shutdown suspends it as `shutdown`, a
    /// spent grant as `cap`.
    async fn curation_owed(&self, job: &mut Reviewed, why: &str) -> bool {
        let reason = if self.scope.stopping() {
            STOPPED
        } else {
            SPENT
        };
        let stop = format!("curation: {why}; suspended to curate under a fresh grant");
        self.suspend(job, reason, &stop).await
    }

    /// Suspend the job before its curation, whose run `outage` refused its
    /// credentials, as [`AUTH_UNAVAILABLE`]: as [`Self::curation_owed`], so
    /// the resume curates and ships without another review. The caller
    /// keeps the attempt for the outage (`Reviewed::offline`), so the cycle
    /// ends with its denial as for a review round.
    async fn curation_refused(&self, job: &mut Reviewed, outage: &Outage) -> bool {
        let stop = format!("curation: {outage}; suspended to curate once it is back");
        self.suspend(job, AUTH_UNAVAILABLE, &stop).await
    }

    /// Hold the fix for the operator after review round `shown` (as people
    /// are shown it) on its unanswered `issues` (`stop` says why no answer
    /// comes): the report goes where a worker's would (`BLOCKED.md`, which
    /// `conclude_fix` turns into a `prerequisite` hold with this attempt as
    /// its checkpoint), and the job moves on to answering them once the
    /// operator continues ([`Step::Answer`] 0), after which the review goes
    /// on with a new pass of up to `review.maxRounds` rounds, with the
    /// follow-ups so far. A tree to
    /// put back ([`StepRecord::rewrite`]: a reviewer's changes that could
    /// not be undone) is put back before the fixer resumes; the report
    /// says so. `false` when the report could not be written: the fix
    /// ships with the issues unresolved instead.
    async fn hold(&self, job: &mut Reviewed, shown: u32, issues: &[Raised], stop: &str) -> bool {
        let start = self.start;
        let max = self.cfg.review_max_rounds;
        let mut report = format!(
            "# Held by the pre-PR review\n\nThe automated pre-PR review stopped after round \
             {shown} ({stop}) with issues unresolved. Continue to have the fixer \
             answer them; the review then goes on with a new pass of up to {max} rounds.\n\n"
        );
        let restoring = job.record.rewrite.as_ref();
        if let Some(commit) = restoring.and_then(|tree| tree.before.head.as_deref()) {
            let _ = write!(
                report,
                "The reviewer's changes to the tree could not be undone. Before the fixer \
                 resumes, the fix branch is put back to the reviewed commit `{commit}` and what \
                 the reviewer added is removed; while that fails, the fixer does not run (the \
                 attempt fails as `{TREE_NOT_RESTORED}`), and `{}` needs cleaning up by hand.\n\n",
                start.ws.tree.display()
            );
        }
        report.push_str("## Remaining review issues\n");
        raised_list(&mut report, issues);
        let follow_ups = &job.record.follow_ups;
        if !follow_ups.is_empty() {
            let _ = write!(report, "\n\n{}", follow_ups_note(follow_ups));
        }
        if let Err(e) = std::fs::write(start.ws.tree.join(BLOCKED_REPORT), &report) {
            tracing::warn!("fix #{}: holding on the review: {e}", start.finding.id);
            return false;
        }
        job.held = true;
        job.record.verdict = None;
        job.record.held_round = Some(shown);
        job.record.released = false;
        self.go(job, Step::Answer(0)).await;
        true
    }
}

/// What becomes of a round's issues when no answer is coming
/// ([`Rounds::unanswered`]).
enum Unanswered {
    /// An answer is coming: send them back to the fixer.
    No,
    /// The fix is held for the operator ([`Rounds::hold`]).
    Held,
    /// They ship unresolved.
    Ship(Ending),
    /// A run did no work for an outage ([`Outage`]): retry later.
    Offline(Outage),
    /// The grant is spent: the job is suspended, and its resume answers
    /// them ([`Rounds::spent`]).
    Suspended,
}

impl Rounds<'_> {
    /// Whether a run of round `shown`'s `what` (`review`, `answer`) that
    /// did no work did so for an outage, not the fix: `run` was refused its
    /// credentials ([`Outage::of_run`]), or a host the work needs is
    /// unreachable or workers get no credentials now ([`Outage::now`]).
    /// Handled as the fix's other offline failures are
    /// (`FixOutcome::Offline`). The job's record stays at its step, so the
    /// retry has it again.
    async fn offline(&self, shown: u32, what: &str, run: Option<&RunResult>) -> Option<Outage> {
        let start = self.start;
        let outage = if let Some(outage) = run.and_then(Outage::of_run) {
            outage
        } else {
            Outage::now(self.cfg, &start.repo.url, self.backend).await?
        };
        let message = format!("{outage}; kept to {what} round {shown} again");
        log_round(self.store(), start, shown, &message).await;
        Some(outage)
    }

    /// What a failed round amounts to (`why`), its reviewer's run `run`
    /// when it had one. A reviewer refused its credentials, or any while a
    /// host the work needs is unreachable, failed for an outage
    /// ([`Self::offline`]): one that cut a review off mid-way is no more
    /// the fix's than one that kept it from starting. Otherwise the
    /// round decides as one that failed before it ran
    /// ([`Self::unreviewed`]), on the issues open before it (`earlier`,
    /// which a failed round settles none of) and those its reviewer wrote
    /// before it failed (`issues`), numbered as the round is shown
    /// (`shown`). A reviewer whose changes could not be
    /// undone (`head`, the commit it was shown) leaves the tree to put
    /// back in the record, however the round ends: no later worker builds
    /// on what it left.
    async fn failed_round(
        &self,
        job: &mut Reviewed,
        (round, shown): (u32, u32),
        (why, head, issues): (&str, Option<String>, &[ReportedIssue]),
        (earlier, run): (&[ChainIssue], Option<&RunResult>),
        also: &str,
    ) -> Unanswered {
        if let Some(outage) = self.offline(shown, "review", run).await {
            return Unanswered::Offline(outage);
        }
        let open: Vec<Raised> = numbered(earlier)
            .into_iter()
            .chain(issues.iter().map(|i| (shown, i.clone())))
            .collect();
        self.unreviewed(job, (round, shown), (why, head), &open, also)
            .await
    }

    /// What round `round` of the pass (`shown`, as people are shown it),
    /// which reached no verdict (`why`), amounts to on
    /// the review's `open` issues: the earlier rounds' still open, and any
    /// its reviewer wrote. They decide as an answered round's would
    /// ([`Self::holds`]): in the pass's last round any of them holds the fix for
    /// the operator, before it a blocking one does. Otherwise the fix
    /// ships, saying why it was not reviewed and naming the issues it
    /// leaves unresolved: a failing reviewer never holds it hostage, but
    /// neither does it wave through what the review already found.
    /// `head`: the commit a reviewer whose changes could not be undone was
    /// shown, which ships in place of `HEAD`.
    async fn unreviewed(
        &self,
        job: &mut Reviewed,
        (round, shown): (u32, u32),
        (why, head): (&str, Option<String>),
        open: &[Raised],
        also: &str,
    ) -> Unanswered {
        let start = self.start;
        if self.holds(round, open) && self.hold(job, shown, open, why).await {
            let message = format!(
                "failed: {why}; {}{also}; held for the operator",
                tally(open.iter().map(|(_, i)| i))
            );
            log_round(self.store(), start, shown, &message).await;
            return Unanswered::Held;
        }
        let message = format!(
            "failed: {why}; {}{also}; shipping",
            tally(open.iter().map(|(_, i)| i))
        );
        log_round(self.store(), start, shown, &message).await;
        Unanswered::Ship(failed(shown, why, head, open))
    }

    /// Whether the unanswered `issues` after round `round` of the pass
    /// hold the fix for the operator instead of shipping: any of them in
    /// the pass's last round (a review still finding something after
    /// `review.maxRounds` rounds is not converging), else only a blocking
    /// one.
    fn holds(&self, round: u32, issues: &[Raised]) -> bool {
        let last = round >= self.cfg.review_max_rounds;
        (last && !issues.is_empty()) || issues.iter().any(|(_, i)| i.blocking)
    }

    /// Whether the review's open `issues` after round `round` of the pass
    /// (`shown`, as people are shown it) get no
    /// answer, and what then: none comes in the pass's last round, and any issue
    /// still open holds the fix for the operator: a review that still finds
    /// something after `review.maxRounds` rounds is not converging, and its
    /// change is not shipped on the hope that the rest is harmless. When
    /// the grant is spent (a reviewer killed at its cap) and leaves nothing
    /// to answer them with, the job is suspended at answering them
    /// ([`Rounds::spent`]); only a job with no transcript to resume holds
    /// the fix for a blocking issue and ships the rest unresolved.
    /// `raised` is the round's tally for the event.
    async fn unanswered(
        &self,
        job: &mut Reviewed,
        (round, shown): (u32, u32),
        (issues, undone): (&[ChainIssue], bool),
        raised: &str,
    ) -> Unanswered {
        let (start, max) = (self.start, self.cfg.review_max_rounds);
        let why = if round >= max {
            None
        } else {
            match self.allowance(&job.rr) {
                Ok(_) => return Unanswered::No,
                Err(why) => Some(why),
            }
        };
        let stop = why.map_or_else(
            || format!("the last of this pass's {max} rounds"),
            |why| format!("{why} to answer them"),
        );
        if why.is_some() {
            self.go(job, Step::Answer(round)).await;
            let what = format!("review round {shown}");
            if self.spent(job, &what, &format!("{raised}, {stop}")).await {
                return Unanswered::Suspended;
            }
        }
        let issues = numbered(issues);
        if self.holds(round, &issues) && self.hold(job, shown, &issues, &stop).await {
            let message = format!("{raised}, {stop}; held for the operator");
            log_round(self.store(), start, shown, &message).await;
            return Unanswered::Held;
        }
        let message = format!("{raised}, {stop}; shipping with them");
        log_round(self.store(), start, shown, &message).await;
        let mut ending = unresolved(&issues, why);
        if undone && let Some(note) = &mut ending.note {
            let _ = write!(note, "\n\n{}", undone_note(shown));
        }
        Unanswered::Ship(ending)
    }
}

impl Unanswered {
    /// Where the loop goes when no answer is coming: the fix ships
    /// ([`Flow::Finish`]), or the attempt ends on its state (held,
    /// suspended, or kept for a retry while a host is unreachable).
    fn ended(self, job: &mut Reviewed) -> Flow {
        match self {
            Self::Ship(ending) => Flow::Finish(ending),
            Self::Offline(outage) => {
                job.offline = Some(outage);
                Flow::End
            }
            Self::Held | Self::No | Self::Suspended => Flow::End,
        }
    }
}

/// Where a step leaves the attempt.
enum Flow {
    /// On to the step the job's record now names, in this attempt.
    On,
    /// On to [`Step::Answer`], with the review's open issues after the
    /// round just had, their tally, and that round's number as people are
    /// shown it ([`shown_round`]).
    Answer(Vec<ChainIssue>, String, u32),
    /// The review reached this ending: the fix ships ([`finish`]).
    Finish(Ending),
    /// The attempt ends on its state, shipping [`Reviewed::ending`] if it
    /// is set.
    End,
}

/// Run the fix job from the step its record names (`record`) until it
/// ships, is held, or stops: the review of the fixer's finished work, the
/// fixer's answers to it, the curation and the publish ([`Step`]). Each
/// review round's issues go back to the fixer, until a review raises none
/// and leaves none open, or `review.maxRounds` reviews have run. Whether an
/// issue is `blocking` only orders it and marks it for the human; every
/// one is answered but a follow-up, which is collected across rounds and
/// shipped in the PR body ([`follow_ups_note`]). `fixer` is the
/// attempt's fixer run, when it is at [`Step::Implement`] (`run_fix`), and
/// otherwise stands in for none, on the fixer's transcript; the result
/// sums it with every run after it. `after_hold` is the hold an operator
/// released to get here: an attempt that starts at [`Step::Answer`] tells
/// the fixer so.
///
/// Answers a fixer left before the attempt (one cut short mid-answer, or
/// one whose answers the store could not take) are recorded first
/// ([`record_answers`]); answers that still cannot be recorded fail the
/// attempt, kept at its step ([`Reviewed::failure`]). A report in the
/// tree the daemon died before recording ends the attempt at once as it
/// would have ended had it lived: the review's own hold
/// ([`StepRecord::holding`]) holds the fix as it is, a fixer's from an
/// answer concludes it as any fixer's report does, and one a reviewer or
/// a curator left is gone, its changes put back first ([`put_back`]). A review
/// switched off (`review.maxRounds` 0) while the fix was under review
/// runs no more reviewer: an answer owed is still had, then the fix ships
/// as it stands, with the review's summary ([`finish`]).
/// A fixer that is suspended or writes a report while answering a review
/// ends the attempt there: the job is recorded on its state and
/// `conclude_fix` reads its report, as for any fix. So does the daemon's
/// shutdown, checked before every run ([`Rounds::stopped`]), and a grant
/// spent before the review is through ([`Rounds::spent`]): either
/// suspends the job at its step.
pub(super) async fn review_fix(
    scope: &CycleScope<'_>,
    lane: &mut Lane,
    (cfg, backend): (&Config, &dyn Backend),
    start: &FixStart,
    (fixer, record): (RunResult, StepRecord),
    after_hold: Option<&(HoldReason, String)>,
) -> Reviewed {
    let answered = record_answers(scope.store(), start).await;
    let rounds = Rounds {
        scope,
        cfg,
        backend,
        start,
    };
    let mut job = Reviewed {
        rr: fixer,
        ending: None,
        offline: None,
        failure: None,
        record,
        held: false,
    };
    // Answers the store cannot take would be lost to the next reviewer:
    // the attempt fails, kept at its step, and its retry records them.
    if let Err(why) = answered {
        job.failure = Some(why);
        return job;
    }
    if has_report(start) {
        match job.record.step {
            // Only the review's own hold leaves its report at the answer
            // it holds: the daemon died before it was recorded.
            _ if job.record.holding() => {
                job.held = true;
                return job;
            }
            // A fixer's report, written while it answered a round: it
            // concludes the attempt as it would have had the daemon lived
            // ([`Reviewed::concluded_on_report`]).
            Step::Answer(_) => return job,
            // One a reviewer or a curator left went with the rest of its
            // changes, put back before the attempt began ([`put_back`]).
            Step::Review(_) | Step::Curate | Step::Implement | Step::Publish => {}
        }
    }
    let mut after_hold = after_hold.filter(|_| matches!(job.record.step, Step::Answer(_)));
    let mut owed = None;
    loop {
        let flow = match job.record.step {
            Step::Implement => implemented(&rounds, &mut job).await,
            // The review switched off (`review.maxRounds` 0) while the
            // fix was under review: no reviewer runs, and the fix ships as
            // it stands, with what the review came to.
            Step::Review(_) if cfg.review_max_rounds == 0 => Flow::Finish(Ending::default()),
            Step::Review(round) => next_round(&rounds, lane, &mut job, round).await,
            Step::Answer(round) => {
                let released = after_hold.take();
                answer(&rounds, &mut job, (round, owed.take()), released).await
            }
            Step::Curate => curation(&rounds, lane, &mut job).await,
            Step::Publish => publish(&rounds, &mut job).await,
        };
        let flow = match flow {
            Flow::Finish(ending) => finish(&rounds, lane, &mut job, ending).await,
            flow => flow,
        };
        match flow {
            Flow::On => {}
            Flow::Answer(issues, raised, from) => owed = Some((issues, raised, from)),
            Flow::End | Flow::Finish(_) => return job,
        }
    }
}

/// Whether `start`'s tree holds a report: `BLOCKED.md`, or a decline.
fn has_report(start: &FixStart) -> bool {
    let tree = &start.ws.tree;
    tree.join(BLOCKED_REPORT).exists() || tree.join(decline_report(start.finding.kind)).exists()
}

/// Remove from `tree` every report a worker may leave: `BLOCKED.md` and
/// both declines.
fn remove_reports(tree: &Path) {
    for report in [BLOCKED_REPORT, "NOT-A-BUG.md", "DECLINED.md"] {
        let _ = std::fs::remove_file(tree.join(report));
    }
}

/// Where the fixer's run leaves the fix ([`Step::Implement`]). A saved
/// verdict that still covers the tree (the fixer an operator's `continue`
/// of a refused publish ran changed nothing, however its run ended) moves
/// it on to [`Step::Publish`], which ships on it. Otherwise a fix that
/// would ship as it stands ([`ready_to_ship`]) goes on: with the review
/// off, to [`Step::Publish`] on an empty verdict for the tree, which a
/// retry of its publish ships; with the review on, to review round 1.
/// Anything else ends the attempt, for `conclude_fix` to decide: a
/// finished fixer that wrote a report or left nothing to ship is never
/// kept to publish.
async fn implemented(rounds: &Rounds<'_>, job: &mut Reviewed) -> Flow {
    let start = rounds.start;
    let now = tree_state(&start.ws.tree).await;
    if job
        .record
        .verdict
        .as_ref()
        .is_some_and(|saved| covers(saved, &now))
    {
        rounds.go(job, Step::Publish).await;
        return Flow::On;
    }
    job.record.verdict = None;
    let ready = ready_to_ship(start, &job.rr).await;
    if rounds.cfg.review_max_rounds == 0 {
        // Unreviewed, it ships as it stands, as it always has; a fixer
        // that finished with nothing to ship is `conclude_fix`'s failure.
        if ready || job_state(&job.rr) != FinishedState::Done {
            job.ending = Some(Ending::default());
        }
        if ready {
            job.record.verdict = Some(Saved {
                note: None,
                tree: now,
            });
            rounds.go(job, Step::Publish).await;
        }
        return Flow::End;
    }
    if !ready {
        return Flow::End;
    }
    rounds.go(job, Step::Review(1)).await;
    Flow::On
}

/// Run review round `round` of the pass ([`Step::Review`]), unless the
/// daemon is stopping or the grant is spent, either of which suspends the
/// job, as does a reviewer killed at its cap before it wrote an issue
/// ([`Rounds::spent`]): the resume has the round again. The reviewer is
/// shown the issues earlier rounds left open, with the fixer's answers,
/// and judges them. Then what the round amounts to: the fix ships once the
/// review has no issue left open ([`Rounds::open`]), new or earlier;
/// otherwise every open one goes back to the fixer ([`Rounds::unanswered`]).
/// Events and notes name the round as people are shown it
/// ([`shown_round`]). A round that cannot be recorded fails the attempt,
/// kept at this step ([`Reviewed::failure`]): the loop reads what is open
/// from the record, so its issues would reach no fixer. The retry runs the
/// round again.
async fn next_round(rounds: &Rounds<'_>, lane: &mut Lane, job: &mut Reviewed, round: u32) -> Flow {
    let start = rounds.start;
    let Some((earlier, settled, shown)) = before_round(rounds, job).await else {
        return Flow::End;
    };
    let what = format!("review round {shown}");
    if rounds.stopped(job, &what).await {
        return Flow::End;
    }
    let (cap, wall) = match rounds.allowance(&job.rr) {
        Ok(allowance) => allowance,
        Err(why) => {
            let why = format!("{why} to review it");
            if rounds.spent(job, &what, &why).await {
                return Flow::End;
            }
            let open = numbered(&earlier);
            let unreviewed = rounds.unreviewed(job, (round, shown), (&why, None), &open, "");
            return unreviewed.await.ended(job);
        }
    };
    let (run, head, verdict, raised_follow_ups) =
        review_round(rounds, lane, job, shown, (cap, wall), (&earlier, &settled)).await;
    if let Some(run) = &run {
        add_spend(&mut job.rr, run);
    }
    let reviewed = (run.as_ref(), head.as_deref());
    let recorded = record_round(
        rounds,
        shown,
        reviewed,
        &verdict,
        (&raised_follow_ups, &earlier),
    )
    .await;
    if let Err(why) = recorded {
        let message = format!("{why}; kept to review it again");
        log_round(rounds.store(), start, shown, &message).await;
        job.failure = Some(why);
        return Flow::End;
    }
    let capped = run
        .as_ref()
        .is_some_and(|run| run.killed_reason.as_deref() == Some("cap"));
    let also = follow_up_tally(raised_follow_ups.len());
    merge_follow_ups(&mut job.record.follow_ups, raised_follow_ups);
    let (written, undone) = match verdict {
        // The grant ran out on a reviewer that wrote nothing: no review
        // was had, and the resume has it again.
        RoundVerdict::Failed {
            why, head: None, ..
        } if capped && rounds.spent(job, &what, &why).await => {
            return Flow::End;
        }
        RoundVerdict::Failed { why, head, issues } => {
            let failure = (why.as_str(), head, issues.as_slice());
            let context = (earlier.as_slice(), run.as_ref());
            let failed = rounds
                .failed_round(job, (round, shown), failure, context, &also)
                .await;
            return failed.ended(job);
        }
        RoundVerdict::Clean => (Vec::new(), false),
        RoundVerdict::Issues { issues, undone } => (issues, undone),
    };
    // The round is recorded; what it left open is read back from there.
    // Unread, the round's own verdicts on earlier issues are unknown, and
    // all of them count as still open.
    let issues = match rounds.open().await {
        Ok(issues) => issues,
        Err(why) => {
            let failure = (why.as_str(), None, written.as_slice());
            let context = (earlier.as_slice(), run.as_ref());
            let failed = rounds
                .failed_round(job, (round, shown), failure, context, &also)
                .await;
            return failed.ended(job);
        }
    };
    if issues.is_empty() {
        let message = format!("no issues to answer{also}; shipping");
        log_round(rounds.store(), start, shown, &message).await;
        return Flow::Finish(Ending::default());
    }
    let raised = format!("{}{also}", raised(&issues, &earlier, undone));
    match rounds
        .unanswered(job, (round, shown), (&issues, undone), &raised)
        .await
    {
        Unanswered::No => {
            rounds.go(job, Step::Answer(round)).await;
            Flow::Answer(issues, raised, shown)
        }
        unanswered => unanswered.ended(job),
    }
}

/// What a round is run on: the review's open and settled issues, and the
/// round's number as people are shown it ([`shown_round`]). What earlier
/// rounds left open decides a round that reaches no verdict as much as one
/// that does. Unread, the review cannot tell whether shipping would pass a
/// blocking issue by, nor hold on one: `None`, the attempt failed
/// ([`Reviewed::failure`]), shipping nothing, and its retry has the round
/// again.
async fn before_round(
    rounds: &Rounds<'_>,
    job: &mut Reviewed,
) -> Option<(Vec<ChainIssue>, Vec<ChainIssue>, u32)> {
    match rounds.review().await {
        Ok(review) => Some((
            review.open(),
            review.settled(),
            shown_round(review.rounds.len()),
        )),
        Err(why) => {
            let (start, fid) = (rounds.start, rounds.start.finding.id);
            let message = format!("#{fid} review: {why}; nothing ships");
            let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
            job.failure = Some(why);
            None
        }
    }
}

/// Record review round `shown` of the chain (as people are shown it)
/// ([`JobStore::record_fix_review_round`]):
/// over `<pinned>..head`, the commit the reviewer was shown, by this
/// attempt's job, with the spend of its reviewer's `run`, the issues it
/// wrote, the `follow_ups` it raised and its verdicts on the `earlier`
/// issues it was shown ([`read_verdicts`]; a failed round, or one whose
/// reviewer's changes were undone, records none).
/// A round whose reviewer finished is `ok`, whatever its `verdict`'s
/// issues; one whose reviewer was stopped, or whose round failed, after it
/// wrote issues the fix goes on with is `partial`; any other, follow-ups
/// alone included, is `failed`.
/// The loop reads what is open from this record: `Err` says why the round
/// could not be written. A round whose commit git could not say (only one
/// that failed before its reviewer ran, with nothing written) is not
/// recorded, logged.
async fn record_round(
    rounds: &Rounds<'_>,
    shown: u32,
    (run, head): (Option<&RunResult>, Option<&str>),
    verdict: &RoundVerdict,
    (follow_ups, earlier): (&[ReportedIssue], &[ChainIssue]),
) -> Result<(), String> {
    let start = rounds.start;
    let stopped = run
        .filter(|rr| rr.killed_reason.is_some() || rr.exit_code != Some(0))
        .map(|rr| format!("the reviewer {}", run_failure(rr)));
    let (written, failure) = match verdict {
        RoundVerdict::Clean => (&[][..], None),
        RoundVerdict::Issues { issues, .. } => (issues.as_slice(), stopped.as_deref()),
        RoundVerdict::Failed { why, issues, .. } => (issues.as_slice(), Some(why.as_str())),
    };
    // A reviewer whose changes had to be undone judged a tree that does not
    // ship: its verdicts settle nothing, and earlier issues stay open. Its
    // own issues still stand.
    let verdicts = match verdict {
        RoundVerdict::Failed { .. } | RoundVerdict::Issues { undone: true, .. } => Vec::new(),
        RoundVerdict::Clean | RoundVerdict::Issues { undone: false, .. } => {
            read_verdicts(rounds, shown, earlier).await
        }
    };
    // Follow-ups alone keep no failed round: they reach the PR body from
    // the step record (`merge_follow_ups`), not from this one.
    let issues: Vec<ReportedIssue> = written.iter().chain(follow_ups).cloned().collect();
    let recorded = match failure {
        None => FixReviewRound::Ok {
            issues: &issues,
            verdicts: &verdicts,
        },
        Some(why) if !written.is_empty() => FixReviewRound::Partial {
            why,
            issues: &issues,
            verdicts: &verdicts,
        },
        Some(why) => FixReviewRound::Failed(why),
    };
    let Some(head) = head else {
        tracing::warn!(
            "#{} review round {shown}: not recorded, its commit is unknown",
            start.finding.id
        );
        return Ok(());
    };
    rounds
        .scope
        .store()
        .record_fix_review_round(
            chain(start),
            start.job,
            (&start.pinned, head),
            run.map(|rr| rr.tokens_new),
            recorded,
        )
        .await
        .map_err(|e| format!("the round could not be recorded: {e}"))?;
    // Answers still waiting in the file (their record failed) were to the
    // rounds before this one, which was reviewed without them: kept, they
    // would be recorded as answers to this round.
    let _ = std::fs::remove_file(start.ws.root.join(ANSWERS_FILE));
    Ok(())
}

/// The review chain `start`'s fix belongs to: its finding, and the job
/// that began it.
fn chain(start: &FixStart) -> (i64, i64) {
    (start.finding.id, start.ws.origin_id)
}

/// Where the reviewer of round `round` of the chain (as people are shown
/// it, [`shown_round`]) writes its verdicts on earlier issues: beside its
/// issues file ([`review_round`]), outside the tree.
fn verdicts_path(start: &FixStart, round: u32) -> PathBuf {
    start
        .ws
        .review_session(start.job, round)
        .with_extension("verdicts.json")
}

/// The verdicts of round `shown` of the chain (as people are shown it) on
/// the `earlier` issues it was shown
/// ([`parse_verdicts`]). A file that cannot be read gives none, logged:
/// every earlier issue then stays open, and goes back to the fixer.
async fn read_verdicts(rounds: &Rounds<'_>, shown: u32, earlier: &[ChainIssue]) -> Vec<Verdict> {
    if earlier.is_empty() {
        return Vec::new();
    }
    match parse_verdicts(&verdicts_path(rounds.start, shown), earlier) {
        Ok(verdicts) => verdicts,
        Err(why) => {
            let message = format!("no verdicts on the earlier issues ({why}); they stay open");
            log_round(rounds.store(), rounds.start, shown, &message).await;
            Vec::new()
        }
    }
}

/// The review reached `ending`: the review's summary ([`review_summary`])
/// and the follow-ups go into the PR body, and the
/// verdict is recorded for the tree as it is ([`StepRecord::verdict`]),
/// for the curation ([`Step::Curate`]) and the publish that follow; with
/// the review switched off, for the publish alone, as a fix implemented
/// with the review off is not curated. The tree is cleaned first, as for a
/// reviewer ([`clean_tree`]): only the commits ship, and what a fixer's
/// failed answer left uncommitted is no part of the tree the verdict
/// covers, nor something a rejected curation's [`undo`] removes from under
/// it. One
/// that ships a commit a reviewer was shown in place of `HEAD`, the tree
/// as that reviewer left it, covers no tree: it ships in this attempt, and
/// any later one puts the tree back and reviews it again in a new pass. A
/// review whose record cannot be read ships without its summary, logged.
async fn finish(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    mut ending: Ending,
) -> Flow {
    let start = rounds.start;
    match rounds.review().await {
        Ok(review) => with_summary(&mut ending, review_summary(&review)),
        Err(why) => tracing::warn!("fix #{}: no review summary: {why}", start.finding.id),
    }
    with_follow_ups(&mut ending, &job.record.follow_ups);
    if ending.head.is_some() {
        job.record.verdict = None;
        rounds.go(job, Step::Review(1)).await;
        job.ending = Some(ending);
        return Flow::End;
    }
    let fid = start.finding.id;
    let cleaned = match clean_tree(lane, start).await {
        Ok(cleaned) if cleaned.is_empty() => None,
        Ok(cleaned) => Some(format!(
            "#{fid} the tree was cleaned before shipping: {}",
            cleaned_note(&cleaned)
        )),
        Err(why) => Some(format!("#{fid} {why}; it ships as it is")),
    };
    if let Some(message) = cleaned {
        let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
    }
    let tree = tree_state(&start.ws.tree).await;
    job.record.verdict = Some(Saved {
        note: ending.note,
        tree,
    });
    let next = if rounds.cfg.review_max_rounds == 0 {
        Step::Publish
    } else {
        Step::Curate
    };
    rounds.go(job, next).await;
    Flow::On
}

/// Curate the reviewed fix ([`Step::Curate`], [`curate`]), then publish
/// it: on the curated tree, whose verdict is the review's, or on the
/// reviewed one. A rewrite that could not be undone ships the reviewed
/// commit in place of `HEAD` in this attempt, and leaves the record at the
/// curation, the curator's tree to put back: any later attempt puts the
/// tree back and curates again. A job with no verdict (a record an
/// operator wrote), or one whose verdict no longer covers the tree (changed
/// by hand while the job waited to be curated), is reviewed again
/// ([`review_again`]).
async fn curation(rounds: &Rounds<'_>, lane: &mut Lane, job: &mut Reviewed) -> Flow {
    let now = tree_state(&rounds.start.ws.tree).await;
    let Some(saved) = job
        .record
        .verdict
        .clone()
        .filter(|saved| covers(saved, &now))
    else {
        return review_again(rounds, job).await;
    };
    let mut ending = Ending {
        note: saved.note,
        head: None,
    };
    match curate(rounds, lane, job, &mut ending).await {
        Curated::Accepted => {
            let tree = tree_state(&rounds.start.ws.tree).await;
            job.record.verdict = Some(Saved {
                note: ending.note,
                tree,
            });
        }
        Curated::Not if ending.head.is_some() => {
            if let Err(e) = rounds.save(job).await {
                let fid = rounds.start.finding.id;
                tracing::warn!("fix #{fid}: recording the curator's tree to put back: {e}");
            }
            job.ending = Some(ending);
            return Flow::End;
        }
        Curated::Not => {}
        Curated::Suspended => return Flow::End,
        Curated::Refused(outage) => {
            if rounds.curation_refused(job, &outage).await {
                job.offline = Some(outage);
                return Flow::End;
            }
        }
    }
    rounds.go(job, Step::Publish).await;
    Flow::On
}

/// Publish the fix ([`Step::Publish`]) on its verdict, when that covers
/// the tree exactly as it is: nothing runs. Any other tree (changed since
/// the verdict) is reviewed again ([`review_again`]); with the review off,
/// that ships it as it stands, with what the review came to ([`finish`]).
async fn publish(rounds: &Rounds<'_>, job: &mut Reviewed) -> Flow {
    let now = tree_state(&rounds.start.ws.tree).await;
    match &job.record.verdict {
        Some(saved) if covers(saved, &now) => {
            job.ending = Some(Ending {
                note: saved.note.clone(),
                head: None,
            });
            Flow::End
        }
        _ => review_again(rounds, job).await,
    }
}

/// The tree has no verdict that covers it: it is reviewed again in a new
/// pass, with the follow-ups so far, when it would
/// ship as it stands ([`ready_to_ship`]); otherwise the attempt ends on
/// its fixer's state.
async fn review_again(rounds: &Rounds<'_>, job: &mut Reviewed) -> Flow {
    job.record.verdict = None;
    if !ready_to_ship(rounds.start, &job.rr).await {
        return Flow::End;
    }
    rounds.go(job, Step::Review(1)).await;
    Flow::On
}

/// Have the fixer curate its reviewed history before it ships
/// (`curate.md`): its own session, resumed once more under what is left of
/// the grant, rewrites `<pinned>..HEAD` into a few logical commits of the
/// same tree. The rewrite is accepted only when the run finished, `HEAD`
/// moved, the new tree is byte-identical to the reviewed one
/// (`Unheld::SameTree`), the branch, uncommitted state and description are
/// as they were, and the new range is not empty and still descends from
/// the pinned commit. Otherwise the tree is put back to the reviewed
/// commit ([`undo`]) and the original commits ship. Curation never holds,
/// fails or counts against the fix, and is skipped when it is off, there
/// is one commit, or there is no transcript to resume. The daemon's
/// shutdown, a grant with nothing left, or a curator killed at its cap
/// (its rewrite undone) suspend the job at its curation instead
/// ([`Rounds::curation_owed`]): a fix is not shipped uncurated because the
/// budget ran out. Nor for a curator refused its credentials
/// ([`Curated::Refused`]), which never ran. When the tree cannot be put
/// back, `ending` pins the reviewed commit, so that is what ships.
async fn curate(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    ending: &mut Ending,
) -> Curated {
    let Some(due) = curation_due(rounds, job).await else {
        return Curated::Not;
    };
    let stop = if rounds.scope.stopping() {
        Some("the daemon is stopping")
    } else {
        rounds.allowance(&job.rr).err()
    };
    if let Some(why) = stop
        && rounds.curation_owed(job, why).await
    {
        return Curated::Suspended;
    }
    curated_head(rounds, lane, job, ending, due).await
}

/// How a curation went.
enum Curated {
    /// The curated history ships.
    Accepted,
    /// Skipped or rejected: the original commits ship, or the reviewed
    /// commit `ending` now pins when a rejected rewrite could not be undone.
    Not,
    /// The job was suspended at its curation.
    Suspended,
    /// The curator was refused its credentials and never ran, the tree as
    /// it was: the curation is owed again once the outage is over.
    Refused(Outage),
}

/// Whether a curation is due, and what it starts from: the commit count
/// and the fixer's transcript. Not when curation is off, for a single
/// commit, or with no transcript to resume (logged).
async fn curation_due(rounds: &Rounds<'_>, job: &Reviewed) -> Option<(usize, String)> {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let commits = commit_count(start)
        .await
        .filter(|n| cfg.review_curate && *n > 1)?;
    let Some(session) = job.rr.session_file.clone() else {
        let fid = start.finding.id;
        let message = format!(
            "#{fid} curation: skipped (no transcript to resume); shipping the {commits} \
             commits as they are"
        );
        let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
        return None;
    };
    Some((commits, session))
}

/// [`curate`]'s work. While it runs, the job's record holds the reviewed
/// tree to put back ([`StepRecord::rewrite`]), so a daemon that dies in it
/// leaves the next attempt what to put back before it curates again; a
/// tree the curator's rejected rewrite could not be undone from stays
/// there, for every later attempt to put back.
async fn curated_head(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    ending: &mut Ending,
    (commits, session): (usize, String),
) -> Curated {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let fid = start.finding.id;
    let note = |message: String| async move {
        let message = format!("#{fid} curation: {message}");
        let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
    };
    let tree = &start.ws.tree;
    let curating = ReviewedTree {
        before: tree_state(tree).await,
        description: std::fs::read(tree.join(DESCRIPTION)).unwrap_or_default(),
    };
    let (Some(reviewed), Ok((cap, wall))) =
        (curating.before.head.clone(), rounds.allowance(&job.rr))
    else {
        return Curated::Not;
    };
    let Ok(prompt) =
        playbooks::build_curate_prompt(&cfg.root, tree, &start.pinned, &start.branch, &reviewed)
    else {
        return Curated::Not;
    };
    job.record.rewrite = Some(curating.clone());
    if let Err(e) = rounds.save(job).await {
        job.record.rewrite = None;
        note(format!("skipped (cannot record the reviewed tree: {e})")).await;
        return Curated::Not;
    }
    let run = rounds
        .backend
        .run(
            &start.ws,
            &prompt,
            cap,
            wall,
            JobClass::Fix,
            Some(Path::new(&session)),
        )
        .await;
    let after = tree_state(tree).await;
    let capped = run
        .as_ref()
        .is_ok_and(|rr| rr.killed_reason.as_deref() == Some("cap"));
    let refused = run.as_ref().ok().and_then(Outage::of_run);
    let verdict = match &run {
        Err(e) => Err(format!("it could not run: {e}")),
        Ok(rr) => {
            add_spend(&mut job.rr, rr);
            if job_state(rr) == FinishedState::Done {
                curated(start, &curating.before, &after, &reviewed).await
            } else {
                Err(format!("the curation {}", run_failure(rr)))
            }
        }
    };
    match verdict {
        Ok(curated) => {
            job.record.rewrite = None;
            note(format!("{commits} commits rewritten into {curated}")).await;
            Curated::Accepted
        }
        Err(why) => {
            let undone = undo(lane, start, &curating.before, &curating.description).await;
            if undone.is_ok() {
                job.record.rewrite = None;
            }
            // A curator the grant ran out on curated nothing: the resume
            // has it again, once its rewrite is undone. One refused its
            // credentials never ran: its caller decides what the outage
            // makes of the attempt ([`Curated::Refused`]).
            if let Some(outage) = refused.filter(|_| undone.is_ok()) {
                return Curated::Refused(outage);
            }
            if capped && undone.is_ok() && rounds.curation_owed(job, &why).await {
                return Curated::Suspended;
            }
            if undone.is_err() {
                ending.head = Some(reviewed);
            }
            let left = undone
                .err()
                .map_or_else(String::new, |e| format!(" (not undone: {e})"));
            note(format!(
                "rejected: {why}{left}; shipping the original {commits} commits"
            ))
            .await;
            Curated::Not
        }
    }
}

/// How many commits `start`'s branch has on top of its pinned commit, if
/// git can say.
async fn commit_count(start: &FixStart) -> Option<usize> {
    let range = format!("{}..HEAD", start.pinned);
    let tree = start.ws.tree.clone();
    let (rc, commits) =
        crate::child::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
            .await
            .ok()?;
    (rc == 0).then(|| commits.lines().filter(|l| !l.trim().is_empty()).count())
}

/// Whether a curation that ended the tree as `after` (from `before`, at
/// the `reviewed` commit) may ship: the commit count it left, or why not.
async fn curated(
    start: &FixStart,
    before: &TreeState,
    after: &TreeState,
    reviewed: &str,
) -> Result<usize, String> {
    if after.head == before.head {
        return Err("the history was left as it was".to_owned());
    }
    if after.branch != before.branch
        || after.status != before.status
        || after.description != before.description
    {
        return Err("the worktree changed besides its history".to_owned());
    }
    let (tree, reviewed, pinned) = (
        start.ws.tree.clone(),
        reviewed.to_owned(),
        start.pinned.clone(),
    );
    let (same, descends) = crate::child::spawn_blocking(move || {
        let same = Unheld::SameTree {
            from: &reviewed,
            to: "HEAD",
        }
        .run(&tree, 60);
        let descends = Unheld::IsAncestor {
            ancestor: &pinned,
            rev: "HEAD",
        }
        .run(&tree, 30);
        (same.0 == 0, descends.0 == 0)
    })
    .await
    .map_err(|e| e.to_string())?;
    if !same {
        return Err("the curated tree differs from the reviewed one".to_owned());
    }
    if !descends {
        return Err("the curated history does not descend from the pinned commit".to_owned());
    }
    match commit_count(start).await {
        Some(0) | None => Err("no commits are left on the branch".to_owned()),
        Some(n) => Ok(n),
    }
}

/// Put the review's `summary` ([`review_summary`]) first in `ending`'s PR
/// body note, unless it is empty.
fn with_summary(ending: &mut Ending, summary: String) {
    if summary.is_empty() {
        return;
    }
    ending.note = Some(match ending.note.take() {
        Some(note) => format!("{summary}\n\n{note}"),
        None => summary,
    });
}

/// Append the follow-ups every round raised to `ending`'s PR body note.
fn with_follow_ups(ending: &mut Ending, follow_ups: &[ReportedIssue]) {
    if follow_ups.is_empty() {
        return;
    }
    let section = follow_ups_note(follow_ups);
    ending.note = Some(match ending.note.take() {
        Some(note) => format!("{note}\n\n{section}"),
        None => section,
    });
}

/// Why an attempt could not put its tree back to the one an agent whose
/// changes could not be undone was shown ([`put_back`]): the failure its
/// streak counts.
pub(super) const TREE_NOT_RESTORED: &str = "reviewed tree not restored";

/// Put `start`'s tree back to the one a reviewer or a curator was shown
/// ([`undo`]) when the job's `record` says it may have been left as that
/// agent changed it ([`StepRecord::rewrite`]): the daemon died while the
/// agent ran, or its changes could not be undone, whatever the attempt
/// then amounted to (a hold, a retry kept while a host was unreachable, a
/// pinned commit whose publish failed). Run first in every attempt of the
/// chain, before anything reads the tree, so no worker builds on what an
/// agent left and none of it ships. Done once: the record then says so, so
/// a later attempt keeps the fixer's own work. The step the record names
/// is the one to run then: a curator's tree put back is curated again. An
/// error says what still differs, or that the record could not say the
/// tree was put back (a later attempt would put the fixer's new commits
/// back); no worker may run in the tree then, and the record keeps the
/// tree to put back.
///
/// A `BLOCKED.md` in the tree at the review's own hold (the record at
/// [`Step::Answer`] 0 with [`StepRecord::held_round`] and no fixer run
/// since its release, as only [`Rounds::hold`] leaves it) is that hold's
/// report, which no hold has
/// recorded yet: the daemon died between [`Rounds::hold`] writing it and
/// `conclude_fix` recording it (once recorded it leaves the tree, and no
/// worker runs before this). [`undo`] removes reports as a reviewer's, so
/// it is put back whatever undo did: it holds the fix once this attempt
/// ends. Anywhere else a report is the agent's that changed the tree, and
/// goes with its changes.
pub(super) async fn put_back(
    lane: &mut Lane,
    store: &JobStore,
    start: &FixStart,
    record: &mut StepRecord,
) -> Result<(), String> {
    let Some(shown) = &record.rewrite else {
        return Ok(());
    };
    let commit = shown.before.head.clone().unwrap_or_default();
    let unrecorded = start.ws.tree.join(BLOCKED_REPORT);
    let report = record
        .holding()
        .then(|| std::fs::read(&unrecorded).ok())
        .flatten();
    let undone = undo(lane, start, &shown.before, &shown.description).await;
    if let Some(report) = report {
        std::fs::write(&unrecorded, report)
            .map_err(|e| format!("keeping the review's unrecorded {BLOCKED_REPORT}: {e}"))?;
    }
    undone.map_err(|e| format!("putting the tree back to the reviewed commit {commit}: {e}"))?;
    let shown = record.rewrite.take();
    let fid = start.finding.id;
    if let Err(e) = store.set_fix_step(fid, &record.json()).await {
        record.rewrite = shown;
        return Err(format!("recording the tree as put back to {commit}: {e}"));
    }
    let agent = if record.step == Step::Curate {
        "a curator"
    } else {
        "a reviewer"
    };
    let message =
        format!("#{fid} the tree {agent} left was put back to the reviewed commit {commit}");
    let _ = fix_event(store, "review", fid, start.job, message).await;
    Ok(())
}

/// The fixer's answer to the issues the review left open after round
/// `round` of the pass ([`Step::Answer`]): `owed`, the round's own as the
/// attempt just reviewed them, with their tally and the round's number as
/// people are shown it, or, for an attempt that starts here, the review's
/// open issues as they are now ([`Rounds::open`]), from the round
/// [`answered_round`] names. A record that cannot be read, or that has
/// none open, ends the attempt ([`Reviewed::failure`]) kept at this step,
/// a hold still releasable, so the retry answers what is owed: a job
/// reaches this step only on open issues, and nothing but a review round
/// settles one, so none open is a record gone wrong, not a review that
/// can go on. The daemon's shutdown suspends the job here first
/// ([`Rounds::stopped`]); `after_hold`, the hold an operator released for
/// this attempt, is what the fixer is told first ([`answer_review`]).
async fn answer(
    rounds: &Rounds<'_>,
    job: &mut Reviewed,
    (round, owed): (u32, Option<(Vec<ChainIssue>, String, u32)>),
    after_hold: Option<&(HoldReason, String)>,
) -> Flow {
    let start = rounds.start;
    let (issues, raised, from) = if let Some(owed) = owed {
        owed
    } else {
        let read = rounds.review().await.and_then(|review| {
            let open = review.open();
            if open.is_empty() {
                return Err("the review has none open".to_owned());
            }
            Ok((open, answered_round(job, review.rounds.len())))
        });
        let (issues, from) = match read {
            Ok(read) => read,
            Err(why) => {
                let fid = start.finding.id;
                let message = format!("#{fid} review: the issues the fixer owes an answer: {why}");
                let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
                job.failure = Some(why);
                return Flow::End;
            }
        };
        let tallied = tally(issues.iter().map(|i| &i.issue));
        let raised = if after_hold.is_some() {
            format!("{tallied} an operator released")
        } else {
            format!("{tallied} owed since the job was suspended")
        };
        (issues, raised, from)
    };
    if rounds.stopped(job, &format!("review round {from}")).await {
        return Flow::End;
    }
    let message = format!("{raised}; back to the fixer");
    log_round(rounds.store(), start, from, &message).await;
    send_back(rounds, job, (round, from), &issues, after_hold).await
}

/// The round, as people are shown it ([`shown_round`]), whose issues the
/// job answers on the chain's `kept` rounds: the round a hold stopped
/// after ([`StepRecord::held_round`], set only at [`Step::Answer`] 0, on
/// the issues it held), or else the newest kept round.
fn answered_round(job: &Reviewed, kept: usize) -> u32 {
    job.record
        .held_round
        .unwrap_or_else(|| shown_round(kept) - 1)
}

/// Send the review's open `issues` after round `round` of the pass (`from`,
/// as people are shown it) back to the fixer
/// ([`answer_review`]), and record the answers it wrote
/// ([`record_answers`]) whatever its run came to: an issue it did not
/// answer stays unanswered. A fixer that finished its answer moves the job
/// on to the next round; one that paused, wrote a report or broke its
/// branch ends the attempt on its state, for `conclude_fix` to decide,
/// the job at this step. Answers the store could not take end the attempt
/// ([`Reviewed::failure`]): at the next round after a finished answer, as
/// its reviewer would judge the issues without them and its round would
/// drop them; at this step after a failed one ([`no_answer`]), as what
/// ships unresolved would lack them. The retry records them first. A
/// released hold the store could not record runs no fixer
/// ([`Rounds::release`]): the attempt fails kept at this step, and the
/// retry releases it again.
async fn send_back(
    rounds: &Rounds<'_>,
    job: &mut Reviewed,
    (round, from): (u32, u32),
    issues: &[ChainIssue],
    after_hold: Option<&(HoldReason, String)>,
) -> Flow {
    let start = rounds.start;
    let fid = start.finding.id;
    if let Err(why) = rounds.release(job).await {
        job.failure = Some(why);
        return Flow::End;
    }
    let fx = answer_review(rounds, issues, &job.rr, after_hold).await;
    let recorded = record_answers(rounds.store(), start).await;
    let fx = match fx {
        Ok(fx) => fx,
        Err(why) => {
            let message = format!("#{fid} the fixer could not answer round {from}: {why}");
            let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
            return no_answer(rounds, job, (round, from, issues), (&why, None), recorded).await;
        }
    };
    match job_state(&fx) {
        FinishedState::Done => {
            take_run(&mut job.rr, &fx);
            // A report (or a broken branch) ends the attempt:
            // `conclude_fix` decides.
            if !ready_to_ship(start, &job.rr).await {
                return Flow::End;
            }
            rounds.go(job, Step::Review(round + 1)).await;
            if let Err(why) = recorded {
                job.failure = Some(why);
                return Flow::End;
            }
            Flow::On
        }
        // Half an answer: nothing ships, and the resume answers again.
        FinishedState::Suspended => {
            take_run(&mut job.rr, &fx);
            Flow::End
        }
        FinishedState::Failed | FinishedState::Killed => {
            add_spend(&mut job.rr, &fx);
            let why = format!("its answer {}", run_failure(&fx));
            let message = format!("#{fid} the fixer's answer to round {from} failed: {why}");
            let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
            no_answer(
                rounds,
                job,
                (round, from, issues),
                (&why, Some(&fx)),
                recorded,
            )
            .await
        }
    }
}

/// Where the job goes when the fixer's answer to the open `issues` after
/// round `round` of the pass (`from`, as people are shown it) could not
/// start or failed without doing work (`why`; `run`, when it ran).
///
/// A report the failed answer left in the tree (`BLOCKED.md`, or a
/// decline) ends the attempt on it, as a finished answer's does:
/// `conclude_fix` reads it ([`Reviewed::concluded_on_report`]). Neither
/// a hold of the review's own nor the clean before shipping ([`finish`])
/// may overwrite or remove it.
///
/// Answers the fixer wrote that the store could not take (`recorded`)
/// end the attempt kept at this step ([`Reviewed::failure`]): neither a
/// hold nor a ship may go on without them, and the retry records them
/// first. A report still wins: it ends the attempt anyway.
///
/// When the answer was refused its credentials, or a host the work needs
/// is unreachable, it is an outage's failure ([`Rounds::offline`]),
/// handled as the fix's own offline failures are: nothing is counted or
/// published, the attempt is kept as the checkpoint, and the retry has the
/// answer again. Otherwise a blocking
/// issue never ships: it holds the fix for the operator, as one left by
/// the last round does. Only non-blocking ones ship unresolved, and only
/// in the loop: issues a hold held (round 0) are held again, blocking or
/// not, as the hold that released them was made on them all. And only
/// the tree the newest round reviewed ships so: when `HEAD` has moved on
/// from that round's head (commits of a half answer a resume answers
/// again, or of the failed run itself), no round reviewed it, and the
/// attempt fails kept at its step ([`Reviewed::failure`]); the retry asks
/// for the answer again.
async fn no_answer(
    rounds: &Rounds<'_>,
    job: &mut Reviewed,
    (round, from, issues): (u32, u32, &[ChainIssue]),
    (why, run): (&str, Option<&RunResult>),
    recorded: Result<(), String>,
) -> Flow {
    let start = rounds.start;
    if has_report(start) {
        return Flow::End;
    }
    if let Err(unrecorded) = recorded {
        job.failure = Some(unrecorded);
        return Flow::End;
    }
    if let Some(outage) = rounds.offline(from, "answer", run).await {
        job.offline = Some(outage);
        return Flow::End;
    }
    let issues = numbered(issues);
    if round == 0 || issues.iter().any(|(_, i)| i.blocking) {
        let stop = format!("the fixer could not answer them: {why}");
        if rounds.hold(job, from, &issues, &stop).await {
            let tally = tally(issues.iter().map(|(_, i)| i));
            let message = format!("{tally}, {stop}; held for the operator");
            log_round(rounds.store(), start, from, &message).await;
            return Flow::End;
        }
    }
    if let Err(moved) = reviewed_head(rounds).await {
        job.failure = Some(format!(
            "the fixer could not answer round {from}: {why}, and {moved}"
        ));
        return Flow::End;
    }
    Flow::Finish(unresolved(&issues, Some(why)))
}

/// Whether the tree's `HEAD` is the commit the review's newest round was
/// shown: `Err` says why it cannot be told so.
async fn reviewed_head(rounds: &Rounds<'_>) -> Result<(), String> {
    let reviewed = rounds.review().await?.head;
    let head = tree_state(&rounds.start.ws.tree).await.head;
    match (reviewed, head) {
        (Some(reviewed), Some(head)) if reviewed == head => Ok(()),
        (_, None) => Err("its HEAD could not be read".to_owned()),
        (None, _) => Err("no round reviewed its tree".to_owned()),
        (Some(_), Some(head)) => Err(format!("{head} was not reviewed")),
    }
}

/// Record the answers the fixer left in [`ANSWERS_FILE`] in the chain's
/// review ([`JobStore::record_fix_review_answers`]), as answers to its
/// newest round, then remove the file. Called once a fixer's run that was
/// sent issues ends, and at the start of every attempt's review, for a
/// fixer cut short mid-answer. A file that is missing or not a list
/// records nothing; an entry it cannot read is dropped, never taking the
/// others with it ([`parse_answers`]), and so is an answer to an issue the
/// fixer was not sent ([`sent_answers`]). What is recorded, and what could
/// not be, is logged. Answers that could not be recorded stay in the file
/// for the next call (the next attempt's review), which records them
/// again: a record replaces the fixer's answers to the round, so one
/// already made is not doubled. The next round recorded drops them
/// ([`record_round`]), so an attempt about to review fails on the error,
/// which says why they could not be recorded ([`Reviewed::failure`]).
async fn record_answers(store: &JobStore, start: &FixStart) -> Result<(), String> {
    let path = start.ws.root.join(ANSWERS_FILE);
    if !path.exists() {
        return Ok(());
    }
    let fid = start.finding.id;
    let (message, recorded) = match parse_answers(&path) {
        Err(why) => {
            let _ = std::fs::remove_file(&path);
            (
                format!("#{fid} the fixer's answers were not read: {why}"),
                Ok(()),
            )
        }
        Ok(answers) => match sent_answers(store, start, answers).await {
            Err(e) => {
                let why = format!("the fixer's answers could not be recorded: {e}");
                (format!("#{fid} {why}"), Err(why))
            }
            Ok((answers, dropped)) => match store
                .record_fix_review_answers(chain(start), start.job, &answers)
                .await
            {
                Ok(n) => {
                    let _ = std::fs::remove_file(&path);
                    let mut message = format!("#{fid} the fixer answered {n} review issue(s)");
                    if !dropped.is_empty() {
                        let ids: Vec<String> = dropped.iter().map(i64::to_string).collect();
                        let _ = write!(
                            message,
                            "; answers to issues it was not sent dropped: {}",
                            ids.join(", ")
                        );
                    }
                    (message, Ok(()))
                }
                Err(e) => {
                    let why = format!("the fixer's answers could not be recorded: {e}");
                    (format!("#{fid} {why}"), Err(why))
                }
            },
        },
    };
    let _ = fix_event(store, "review", fid, start.job, message).await;
    recorded
}

/// `answers` split into those to issues the fixer was sent, the chain's
/// open issues ([`FixReview::open`]: unsettled, no follow-up), and the
/// ids of the others, which are dropped: an answer to a settled issue or
/// a follow-up would read as one no reviewer judged, contradicting the
/// verdict that settled it. As [`parse_verdicts`] keeps a reviewer to the
/// issues it was shown.
async fn sent_answers(
    store: &JobStore,
    start: &FixStart,
    answers: Vec<(i64, IssueAnswer)>,
) -> sqlx::Result<(Vec<(i64, IssueAnswer)>, Vec<i64>)> {
    let open: HashSet<i64> = store
        .fix_review(chain(start))
        .await?
        .open()
        .iter()
        .map(|i| i.id)
        .collect();
    let (sent, other): (Vec<_>, Vec<_>) =
        answers.into_iter().partition(|(id, _)| open.contains(id));
    Ok((sent, other.into_iter().map(|(id, _)| id).collect()))
}

/// Why a fixer that finished is not shipped when no review ran
/// ([`ready_to_ship`] was false and no report concludes it): what
/// `ship_pr` would say for nothing to ship, or that its commits could not
/// be listed. Nothing unreviewed ships while the review is on.
pub(super) async fn unshippable(start: &FixStart) -> String {
    let range = format!("{}..HEAD", start.pinned);
    let tree = start.ws.tree.clone();
    let (rc, commits) =
        crate::child::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
            .await
            .unwrap_or((127, String::new()));
    match rc {
        0 if commits.trim().is_empty() => "no commits".to_owned(),
        0 if !start.ws.tree.join(DESCRIPTION).exists() => format!("no {DESCRIPTION}"),
        _ => "the review could not list the change's commits (git failed)".to_owned(),
    }
}

/// Whether the fixer's run left a fix `conclude_fix` would ship: it
/// finished, wrote no report, committed on top of the pinned commit, and
/// described the change.
async fn ready_to_ship(start: &FixStart, rr: &RunResult) -> bool {
    let tree = &start.ws.tree;
    if job_state(rr) != FinishedState::Done
        || !tree.join("PR-DESCRIPTION.md").exists()
        || tree.join(BLOCKED_REPORT).exists()
        || tree.join(decline_report(start.finding.kind)).exists()
    {
        return false;
    }
    let range = format!("{}..HEAD", start.pinned);
    let tree = tree.clone();
    let (rc, commits) =
        crate::child::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
            .await
            .unwrap_or((127, String::new()));
    rc == 0 && !commits.trim().is_empty()
}

/// Remove the files round `round`'s reviewer writes (its issues and its
/// verdicts, [`verdicts_path`]) before it runs, so that only what it
/// writes is read back; the issues file's path.
fn fresh_outputs(start: &FixStart, round: u32) -> PathBuf {
    let out = start
        .ws
        .review_session(start.job, round)
        .with_extension("json");
    for stale in [&out, &verdicts_path(start, round)] {
        let _ = std::fs::remove_file(stale);
    }
    out
}

/// Run review round `label` of the chain (as people are shown it,
/// [`shown_round`]) in `start`'s tree under `cap` tokens and
/// `wall` seconds: a fresh session in a directory of its own
/// ([`Workspace::review_session`], one per job and chain round, so it is
/// never a transcript to continue, even for a second pass in the same
/// job, and is metered apart from the fixer's), writing its issues outside
/// the tree, and its verdicts on the `earlier` issues still open beside
/// them ([`verdicts_path`]); the `settled` ones it is shown, not to raise
/// them again. Both files are removed before it runs, so a reviewer that
/// writes none is never read as having written what an earlier run left.
/// The run, if one happened, the commit the reviewer was shown, if git
/// could say, its verdict and its follow-ups.
///
/// The tree is cleaned first ([`clean_tree`]): the reviewer is shown the
/// commits alone, and its verdict is about them. While it runs, `job`'s
/// record holds that tree to put back ([`StepRecord::rewrite`]), so a
/// daemon that dies in it leaves the next attempt what to put back. One
/// that changed the tree has its changes undone ([`undo`]); one whose
/// changes could not be undone leaves the tree it was shown in the record.
/// One that left the commits alone
/// ([`uncommitted_only`]: a test run's artifacts, a lock file or generated
/// source its build rewrote) changed nothing it reviewed: its changes are
/// reverted, logged in the round's event, and its verdict stands. One that
/// changed more keeps its written issues, as a killed reviewer does; a
/// verdict of none does not, since it was reached on a tree it changed.
async fn review_round(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    label: u32,
    (cap, wall): (Option<i64>, i64),
    (earlier, settled): (&[ChainIssue], &[ChainIssue]),
) -> (
    Option<RunResult>,
    Option<String>,
    RoundVerdict,
    Vec<ReportedIssue>,
) {
    let start = rounds.start;
    let _phase = crate::activity::job_phase(JobPhase::Reviewing);
    let fail = |why: String| RoundVerdict::Failed {
        why,
        head: None,
        issues: Vec::new(),
    };
    let session = start.ws.review_session(start.job, label);
    let out = fresh_outputs(start, label);
    // The fixer's description, put back if the reviewer touches it: it is
    // the PR body, and untracked, so no commit protects it.
    let description = std::fs::read(start.ws.tree.join(DESCRIPTION)).unwrap_or_default();
    let prompt = match reviewer_prompt(rounds, label, &description, (earlier, settled)) {
        Ok(prompt) => prompt,
        Err(e) => {
            let head = tree_state(&start.ws.tree).await.head;
            let why = format!("no reviewer prompt: {e}");
            return (None, head, fail(why), Vec::new());
        }
    };
    // The change is the commits: the reviewer is shown them alone, and
    // whatever it leaves besides is its own. What the clean cannot remove
    // stays, part of the tree the reviewer is held to, as before there
    // was a clean: a leftover never costs the fix its review.
    match clean_tree(lane, start).await {
        Ok(cleaned) if cleaned.is_empty() => {}
        Ok(cleaned) => {
            let message = format!(
                "the tree was cleaned for the reviewer: {}",
                cleaned_note(&cleaned)
            );
            log_round(rounds.store(), start, label, &message).await;
        }
        Err(why) => {
            let message = format!("{why}; the reviewer is shown it as it is");
            log_round(rounds.store(), start, label, &message).await;
        }
    }
    let before = tree_state(&start.ws.tree).await;
    // A detached `HEAD` (no branch) is a state like any other; only a
    // commit or status git could not report leaves nothing to compare.
    if !before.readable() {
        // Nothing to hold the reviewer's tree against, or to put back to:
        // no review this round.
        return (None, before.head, fail(UNREADABLE.to_owned()), Vec::new());
    }
    let head = before.head.clone();
    let shown = ReviewedTree {
        before,
        description,
    };
    job.record.rewrite = Some(shown.clone());
    if let Err(e) = rounds.save(job).await {
        let fid = start.finding.id;
        tracing::warn!("fix #{fid} review round {label}: recording the tree to put back: {e}");
    }
    let ws = Workspace {
        session,
        ..(*start.ws).clone()
    };
    let run = rounds
        .backend
        .run(&ws, &prompt, cap, wall, JobClass::Fix, None)
        .await;
    let after = tree_state(&start.ws.tree).await;
    job.record.rewrite = None;
    let rr = match run {
        Ok(rr) => rr,
        Err(e) => {
            let why = format!("the reviewer could not run: {e}");
            return (None, head, fail(why), Vec::new());
        }
    };
    let finished = rr.killed_reason.is_none() && rr.exit_code == Some(0);
    // Follow-ups stand however the round ends, and are never answered: the
    // verdict is the rest's.
    let (mut written, follow_ups) = match parse_review(&out) {
        Ok((entries, dropped)) => {
            let (follow_ups, rest): (Vec<_>, Vec<_>) =
                entries.into_iter().partition(ReportedIssue::is_follow_up);
            // Entries that were not issues may have meant one: no clean
            // verdict rests on them.
            if dropped > 0 && rest.is_empty() {
                let why = format!("{dropped} review entries had no summary");
                (Err(why), follow_ups)
            } else {
                (Ok(rest), follow_ups)
            }
        }
        Err(why) => (Err(why), Vec::new()),
    };
    if shown.before != after
        && let Some(verdict) =
            undo_reviewer(rounds, lane, label, &shown, &after, &mut written).await
    {
        if matches!(verdict, RoundVerdict::Failed { head: Some(_), .. }) {
            job.record.rewrite = Some(shown);
        }
        return (Some(rr), head, verdict, follow_ups);
    }
    let verdict = match written {
        Ok(issues) if issues.is_empty() && finished => RoundVerdict::Clean,
        // A reviewer killed mid-review still verified what it wrote: the
        // playbook has it rewrite the file after every issue.
        Ok(issues) if !issues.is_empty() => RoundVerdict::Issues {
            issues,
            undone: false,
        },
        Err(why) if finished => fail(why),
        // Ended early with no issue written: no verdict.
        Ok(_) | Err(_) => fail(format!("the reviewer {}", run_failure(&rr))),
    };
    (Some(rr), head, verdict, follow_ups)
}

/// Round `round`'s reviewer prompt (`review.md`): the change and the
/// fixer's `description`, and every earlier issue of the chain with the
/// fixer's answers: those still open (`earlier`) for it to judge into
/// [`verdicts_path`], and the `settled` ones with the verdict that settled
/// them, not to be judged or raised again ([`earlier_json`]).
fn reviewer_prompt(
    rounds: &Rounds<'_>,
    round: u32,
    description: &[u8],
    (earlier, settled): (&[ChainIssue], &[ChainIssue]),
) -> anyhow::Result<String> {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let out = start
        .ws
        .review_session(start.job, round)
        .with_extension("json");
    let verdicts = verdicts_path(start, round);
    let shown = if earlier.is_empty() && settled.is_empty() {
        None
    } else {
        Some(earlier_json(earlier, settled)?)
    };
    playbooks::build_review_prompt(
        &cfg.root,
        &start.finding,
        &start.ws.tree,
        &start.pinned,
        &String::from_utf8_lossy(description),
        ReviewOutputs {
            issues: &out,
            earlier: &verdicts,
        },
        shown.as_deref(),
        &start.repo,
        &Store::repo_notes(&cfg.work_root, start.repo.id),
    )
}

/// An issue of the chain's review as the fixer and the next reviewer are
/// shown it: the reviewer's entry, the id that an answer and a verdict
/// name it by, the fixer's newest answer, and why the newest round that
/// judged it found it still open, unless that answer came after it: no
/// reviewer read it yet, so the note judged an earlier one. The reviewer
/// is also shown its `status` (`open`, or the verdict that settled it, as
/// a PR review's earlier issues are) and, for a settled one, that
/// verdict's `note`.
#[derive(Serialize)]
struct ShownIssue<'a> {
    id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    #[serde(flatten)]
    issue: &'a ReportedIssue,
    #[serde(skip_serializing_if = "Option::is_none")]
    answer: Option<&'a IssueAnswer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    still_open: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<&'a str>,
}

impl<'a> ShownIssue<'a> {
    /// `issue` as the fixer is shown it: no status, all of them open.
    fn of(issue: &'a ChainIssue) -> Self {
        Self {
            id: issue.id,
            status: None,
            issue: &issue.issue,
            answer: issue.answer(),
            still_open: issue
                .unjudged_answer()
                .is_none()
                .then(|| issue.still_open())
                .flatten(),
            note: None,
        }
    }

    /// `issue` as the next reviewer is shown it: with its status, and the
    /// note of the verdict that settled it, if one did.
    fn judged(issue: &'a ChainIssue) -> Self {
        let settled = issue.settled_by();
        Self {
            status: Some(settled.map_or(IssueVerdict::Open, |v| v.verdict).as_str()),
            note: settled.map(|v| v.note.as_str()),
            ..Self::of(issue)
        }
    }
}

/// Every earlier issue of the chain as the next reviewer is shown it
/// ([`ShownIssue::judged`]), one JSON document: the `open` ones, to judge,
/// then the `settled` ones, not to raise again.
fn earlier_json(open: &[ChainIssue], settled: &[ChainIssue]) -> serde_json::Result<String> {
    let shown: Vec<ShownIssue<'_>> = open.iter().chain(settled).map(ShownIssue::judged).collect();
    serde_json::to_string_pretty(&shown)
}

/// `issues` as the fixer is shown them ([`ShownIssue::of`]), one JSON
/// document: the reviewer's and the fixer's words stay inside its strings,
/// where they cannot end the block or pass for prompt text.
fn issues_json(issues: &[ChainIssue]) -> serde_json::Result<String> {
    let shown: Vec<ShownIssue<'_>> = issues.iter().map(ShownIssue::of).collect();
    serde_json::to_string_pretty(&shown)
}

/// Undo the changes of round `label`'s reviewer (as people are shown the
/// round), which left `after` of the
/// tree it was `shown` ([`undo`]), and give the round's verdict on the
/// issues it wrote (`written`, taken when the verdict is given here).
/// `None` when it left the commits alone ([`uncommitted_only`]): its
/// changes are reverted, the round's event names them, and its verdict
/// stands. When it changed more, its written issues stand, undone, and a
/// verdict of none fails the round. One that could not be undone fails the
/// round, with the commit it was shown, which ships, and its written
/// issues.
async fn undo_reviewer(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    label: u32,
    shown: &ReviewedTree,
    after: &TreeState,
    written: &mut Result<Vec<ReportedIssue>, String>,
) -> Option<RoundVerdict> {
    const CHANGED: &str = "the reviewer changed the worktree";
    let start = rounds.start;
    // Read before the undo reverts them.
    let left = uncommitted_only(&start.ws.tree, &shown.before, after).await;
    let undone = undo(lane, start, &shown.before, &shown.description).await;
    let mut issues = || written.as_mut().map_or_default(std::mem::take);
    match (undone, left) {
        (Ok(()), Some(left)) => {
            let message = format!(
                "the reviewer's uncommitted changes were reverted, and its verdict stands: {}",
                cleaned_note(&left)
            );
            log_round(rounds.store(), start, label, &message).await;
            None
        }
        (Ok(()), None) => {
            let issues = issues();
            Some(if issues.is_empty() {
                RoundVerdict::Failed {
                    why: CHANGED.to_owned(),
                    head: None,
                    issues,
                }
            } else {
                RoundVerdict::Issues {
                    issues,
                    undone: true,
                }
            })
        }
        // Not the tree it reviewed, and not one an answer may build on:
        // what ships is the commit it was shown, and every later attempt
        // puts the tree back to it before anything runs.
        (Err(why), _) => Some(RoundVerdict::Failed {
            why: format!("{CHANGED}, and it could not be undone: {why}"),
            head: shown.before.head.clone(),
            issues: issues(),
        }),
    }
}

/// The fixer's description of its change: the PR body.
const DESCRIPTION: &str = "PR-DESCRIPTION.md";

/// Whether `line`, of `git status --porcelain`, is the fixer's description's
/// own: untracked (`??`), or tracked and changed since its commit (` M`),
/// as a fixer that committed it and then updated it leaves it. Either way
/// it belongs in the tree, its bytes are kept across every cleaning, and
/// it is the PR body.
fn is_description_status(line: &str) -> bool {
    matches!(line.split_at_checked(3), Some(("?? " | " M ", DESCRIPTION)))
}

/// `git clean` of every untracked file and directory that is not ignored,
/// a nested repository's among them.
const CLEAN: [&str; 2] = ["clean", "-ffdq"];

/// Every submodule this tree checked out, recursively, put back to the
/// commit its parent records for it (`$sha1`), its uncommitted changes
/// discarded and its untracked files deleted ([`CLEAN`]). `foreach` visits
/// only checked-out submodules, so one this tree never checked out stays
/// so, even when the clone's shared config marks it active (a sibling
/// tree initialized it): `submodule update` would clone it here. No fetch:
/// the commit is the one it was checked out at, or one the worker
/// committed in it.
const SUBMODULE_RESET: [&str; 5] = [
    "submodule",
    "foreach",
    "--quiet",
    "--recursive",
    "git checkout --force --quiet \"$sha1\" && git clean -ffdq",
];

/// The git commands that clean a tree once `first` has put its tracked
/// files back to a commit: [`CLEAN`], then the same for every submodule
/// ([`SUBMODULE_RESET`]). `reset`, `checkout` and `clean` leave a
/// submodule's own tree alone, and `status` lists a submodule at another
/// commit, or with changes or untracked files of its own, as a change of
/// the tree. One checked out at the recorded commit stays, as a fetched
/// dependency would.
fn cleaning(first: Vec<String>) -> Vec<Vec<String>> {
    let owned = |op: &[&str]| op.iter().map(|&arg| arg.to_owned()).collect();
    vec![first, owned(&CLEAN), owned(&SUBMODULE_RESET)]
}

/// How many of the paths a cleaning removed its event names.
const CLEANED_SHOWN: usize = 10;

/// Why a round failed with no state of the tree to hold a reviewer to.
const UNREADABLE: &str = "the worktree's state could not be read (git failed)";

/// Clean `start`'s tree for a reviewer: discard the uncommitted changes to
/// tracked files (`reset --hard`) and delete the untracked files that are
/// not ignored (`clean`), in every checked-out submodule as well
/// ([`cleaning`]), under the clone's permit. Only the commits are the
/// change, so that is all the reviewer is shown, and whatever it then
/// leaves is its own. The fixer's description stays (written back, as the
/// clean removes it and the reset a staged one), as do ignored files
/// (build caches, dependencies). What went, as `git status --porcelain`
/// listed it; an error says why the round cannot be reviewed.
async fn clean_tree(lane: &mut Lane, start: &FixStart) -> Result<Vec<String>, String> {
    let tree = &start.ws.tree;
    let listed = uncommitted(tree).await?;
    if listed.is_empty() {
        return Ok(listed);
    }
    let description = std::fs::read(tree.join(DESCRIPTION)).ok();
    let reset = vec![
        "reset".to_owned(),
        "--hard".to_owned(),
        "--quiet".to_owned(),
    ];
    let mut failed = git_ops(lane, start, cleaning(reset)).await;
    if let Err(e) = write_description(tree, description.as_deref()) {
        failed.push(e);
    } else {
        let left = uncommitted(tree).await?;
        if left.is_empty() {
            return Ok(listed);
        }
        failed.push(format!("left in the tree: {}", quoted(&left)));
    }
    Err(format!(
        "the worktree could not be cleaned for the reviewer: {}",
        failed.join("; ")
    ))
}

/// What `git status --porcelain` lists in `tree` besides the fixer's
/// description: uncommitted changes and untracked files that are not
/// ignored. [`UNREADABLE`] when git cannot say.
async fn uncommitted(tree: &Path) -> Result<Vec<String>, String> {
    let tree = tree.to_owned();
    let (rc, out) = crate::child::spawn_blocking(move || Unheld::Status.run(&tree, 60))
        .await
        .unwrap_or((127, String::new()));
    if rc != 0 {
        return Err(UNREADABLE.to_owned());
    }
    Ok(leftovers(&out).map(str::to_owned).collect())
}

/// What a reviewer left uncommitted in `before`, the clean tree it was
/// shown, as `git status --porcelain` lists it (edits to tracked files,
/// untracked files, a submodule's changes as the submodule itself), when
/// that is all that tells `after` from it: the same commit, branch and
/// description. Its verdict is about the commits, which it left alone,
/// and [`undo`] reverts the rest. `None` when it changed more, or git
/// cannot say.
async fn uncommitted_only(
    tree: &Path,
    before: &TreeState,
    after: &TreeState,
) -> Option<Vec<String>> {
    let kept = (&after.head, &after.branch, &after.description)
        == (&before.head, &before.branch, &before.description);
    if !kept {
        return None;
    }
    uncommitted(tree).await.ok()
}

/// The lines of `git status --porcelain` output but the description's
/// ([`is_description_status`]).
fn leftovers(status: &str) -> impl Iterator<Item = &str> {
    status
        .lines()
        .filter(|line| !line.is_empty() && !is_description_status(line))
}

/// `lines`, each in backticks, comma-separated.
fn quoted<S: AsRef<str>>(lines: &[S]) -> String {
    let quoted: Vec<String> = lines.iter().map(|l| format!("`{}`", l.as_ref())).collect();
    quoted.join(", ")
}

/// What a cleaning removed (`cleaned`, `git status --porcelain` lines),
/// for its event: the tracked files whose changes were discarded and the
/// untracked paths deleted, at most [`CLEANED_SHOWN`] named.
fn cleaned_note(cleaned: &[String]) -> String {
    let (untracked, tracked): (Vec<&String>, Vec<&String>) =
        cleaned.iter().partition(|line| line.starts_with("??"));
    let mut shown = CLEANED_SHOWN;
    let mut part = |what: &str, lines: &[&String]| {
        let named: Vec<&str> = lines
            .iter()
            .take(shown)
            .map(|line| line.get(3..).unwrap_or(line))
            .collect();
        shown -= named.len();
        let more = match lines.len() - named.len() {
            0 => String::new(),
            more => format!(" and {more} more"),
        };
        (!lines.is_empty()).then(|| format!("{what} {}{more}", quoted(&named)))
    };
    let parts: Vec<String> = [
        part("uncommitted changes discarded:", &tracked),
        part("untracked files deleted:", &untracked),
    ]
    .into_iter()
    .flatten()
    .collect();
    parts.join("; ")
}

/// Run the git commands `ops` in `start`'s tree, every one of them, under
/// the clone's permit: what failed, each with the first line git said.
async fn git_ops(lane: &mut Lane, start: &FixStart, ops: Vec<Vec<String>>) -> Vec<String> {
    let permit = match lane.lock_clone(start.repo.id).await {
        Ok(permit) => permit,
        Err(e) => return vec![format!("locking the clone: {e}")],
    };
    let at = start.ws.tree.clone();
    let failed = permit
        .blocking(move |hold| {
            let failed = ops.iter().filter_map(|op| {
                let args: Vec<&str> = op.iter().map(String::as_str).collect();
                let (rc, out) = hold.git_in(&at, &args, 60);
                let said = out.lines().find(|line| !line.trim().is_empty());
                let what = args.first().copied().unwrap_or_default();
                (rc != 0).then(|| format!("{what} failed: {}", said.unwrap_or_default()))
            });
            failed.collect()
        })
        .await;
    failed.unwrap_or_else(|e| vec![e.to_string()])
}

/// Write the fixer's `description` back into `tree`, unless there was none.
fn write_description(tree: &Path, description: Option<&[u8]>) -> Result<(), String> {
    let Some(description) = description else {
        return Ok(());
    };
    std::fs::write(tree.join(DESCRIPTION), description)
        .map_err(|e| format!("restoring {DESCRIPTION}: {e}"))
}

/// Put `start`'s tree back to `before`, the clean tree a reviewer was
/// shown ([`clean_tree`]): abort any rebase, cherry-pick, merge or `am` an
/// agent killed or cut short left in progress (`git status` does not show
/// one, and it would block the next agent's own), remove the reports,
/// check the branch `HEAD` was
/// on then out again at the reviewed commit, discarding every uncommitted
/// change (`checkout --force -B`, or `--detach` as the fixer left it; the
/// fixer may work on a branch of its own, and what ships is the commit),
/// delete the
/// untracked files that are not ignored, and do the same in every
/// checked-out submodule ([`cleaning`]), all under the clone's permit,
/// then write the fixer's `description` back (the checkout removes it when
/// it was committed or staged). Ignored files stay. Undone or not is
/// judged by the tree ([`unrestored`]) once the description is back; an
/// error says what git could not do and what is still not back.
async fn undo(
    lane: &mut Lane,
    start: &FixStart,
    before: &TreeState,
    description: &[u8],
) -> Result<(), String> {
    let tree = &start.ws.tree;
    if let Ok(permit) = lane.lock_clone(start.repo.id).await {
        let at = tree.clone();
        let _ = permit
            .blocking(move |hold| {
                for op in ["rebase", "cherry-pick", "merge", "am"] {
                    // Nothing in progress is fine.
                    let _ = hold.git_in(&at, &[op, "--abort"], 60);
                }
            })
            .await;
    }
    remove_reports(tree);
    let head = before.head.clone().ok_or("no commit to go back to")?;
    let mut checkout = vec!["checkout".to_owned(), "--force".to_owned()];
    match before.branch.as_deref() {
        Some(branch) => {
            let name = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            checkout.extend(["-B".to_owned(), name.to_owned()]);
        }
        None => checkout.push("--detach".to_owned()),
    }
    checkout.push(head);
    let mut failed = git_ops(lane, start, cleaning(checkout)).await;
    let description = before.description.as_ref().map(|_| description);
    write_description(tree, description).map_err(|e| {
        failed.push(e);
        failed.join("; ")
    })?;
    let Some(left) = unrestored(before, &tree_state(tree).await) else {
        return Ok(());
    };
    failed.push(left);
    Err(failed.join("; "))
}

/// What keeps `now` from being `before`, the tree a reviewer was shown,
/// put back, or `None` when nothing does: the same commit and branch, and
/// nothing uncommitted or untracked but ignored files, the fixer's
/// description (written back, so as it was) and what `before` already
/// listed (a leftover the clean could not remove). Named for the operator
/// who has to clear it.
fn unrestored(before: &TreeState, now: &TreeState) -> Option<String> {
    let mut differs = Vec::new();
    if now.head != before.head {
        let head = now.head.as_deref().unwrap_or("unreadable");
        differs.push(format!("HEAD is at {head}"));
    }
    if now.branch != before.branch {
        let branch = now.branch.as_deref().unwrap_or("none (detached)");
        differs.push(format!("HEAD's branch is {branch}"));
    }
    match now.status.as_deref() {
        None => differs.push("git could not report the status".to_owned()),
        Some(status) => {
            let shown: Vec<&str> = before
                .status
                .as_deref()
                .map_or_else(Vec::new, |s| leftovers(s).collect());
            let left: Vec<&str> = leftovers(status)
                .filter(|line| !shown.contains(line))
                .collect();
            if !left.is_empty() {
                differs.push(format!("left in the tree: {}", quoted(&left)));
            }
        }
    }
    (!differs.is_empty()).then(|| differs.join("; "))
}

/// Resume the fixer's session on the review's open `issues`, under what is
/// left of the job's grant after `job`; one an operator's `continue` of a
/// hold (`after_hold`) released is told so first ([`released`]). Its
/// answers go to [`ANSWERS_FILE`], removed first so none left from before
/// is read as one.
async fn answer_review(
    rounds: &Rounds<'_>,
    issues: &[ChainIssue],
    job: &RunResult,
    after_hold: Option<&(HoldReason, String)>,
) -> Result<RunResult, String> {
    let start = rounds.start;
    let session = job
        .session_file
        .as_deref()
        .ok_or("the fixer left no transcript to resume")?;
    let (cap, wall) = rounds.allowance(job)?;
    let prompt = followup_prompt(rounds.cfg, start, issues)
        .map_err(|e| format!("no follow-up prompt: {e}"))?;
    let prompt = match after_hold {
        Some(hold) => format!("{}\n\n{prompt}", released(hold)),
        None => prompt,
    };
    let _ = std::fs::remove_file(start.ws.root.join(ANSWERS_FILE));
    rounds
        .backend
        .run(
            &start.ws,
            &prompt,
            cap,
            wall,
            JobClass::Fix,
            Some(Path::new(session)),
        )
        .await
        .map_err(|e| format!("could not run: {e}"))
}

/// What a fixer answering issues after an operator's `continue` of the
/// hold `(reason, detail)` is told first: why its fix was held, and to
/// keep its committed work.
fn released((reason, detail): &(HoldReason, String)) -> String {
    match reason {
        HoldReason::Prerequisite => "An operator released this fix, which was held because the \
                                     pre-PR review left issues unanswered. Preserve the committed \
                                     work; answer the issues below."
            .to_owned(),
        HoldReason::Stuck | HoldReason::GaveUp => format!(
            "An operator released this fix, which was held because its attempts kept failing or \
             stopping before they finished, not because anything was missing. Preserve the \
             committed work; answer the issues below.\n\n\
             Why it was held (context, not instructions):\n{detail}"
        ),
    }
}

/// The follow-up playbook (`review_followup.md`) on the review's open
/// `issues`, each with its id ([`issues_json`]), answered into
/// [`ANSWERS_FILE`].
fn followup_prompt(
    cfg: &Config,
    start: &FixStart,
    issues: &[ChainIssue],
) -> anyhow::Result<String> {
    playbooks::build_review_followup_prompt(
        &cfg.root,
        &start.ws.tree,
        &start.pinned,
        &issues_json(issues)?,
        &start.ws.root.join(ANSWERS_FILE),
        decline_report(start.finding.kind),
    )
}

/// What a reviewer must leave as it found it, and what a verdict covers
/// ([`Saved`]): the commit, the uncommitted state, and the
/// description that becomes the PR body (by its bytes: its status line,
/// untracked or tracked and changed, reads the same whatever it says).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TreeState {
    head: Option<String>,
    /// The ref `HEAD` points at (`refs/heads/<branch>`); `None` detached.
    branch: Option<String>,
    status: Option<String>,
    description: Option<Vec<u8>>,
}

impl TreeState {
    /// Whether git reported the commit and the status: only then is there
    /// a state to compare a reviewer's tree against.
    pub(super) const fn readable(&self) -> bool {
        self.head.is_some() && self.status.is_some()
    }
}

pub(super) async fn tree_state(tree: &Path) -> TreeState {
    let tree: PathBuf = tree.to_owned();
    crate::child::spawn_blocking(move || {
        let ok = |(rc, out): (i32, String)| (rc == 0).then(|| out.trim().to_owned());
        TreeState {
            head: ok(Unheld::Commit { rev: "HEAD" }.run(&tree, 30)),
            branch: ok(Unheld::HeadRef.run(&tree, 30)),
            // Only the end is trimmed: a porcelain line's first column is
            // its index state, a blank for an unstaged edit (` M path`).
            status: {
                let (rc, out) = Unheld::Status.run(&tree, 60);
                (rc == 0).then(|| out.trim_end().to_owned())
            },
            description: std::fs::read(tree.join("PR-DESCRIPTION.md"))
                .ok()
                .map(|bytes| Sha256::digest(bytes).to_vec()),
        }
    })
    .await
    .unwrap_or(TreeState {
        head: None,
        branch: None,
        status: None,
        description: None,
    })
}

/// The reviewer's issues, blocking ones first and otherwise in its order,
/// and how many entries were not issues; or why its file is not a review
/// (not a JSON array). Each entry stands on its own: an object with a
/// string `summary` is an issue, one whose `blocking` is not a boolean is
/// taken as blocking (a slip in severity must not let a defect ship), and
/// anything else is counted and dropped, never taking the others with it.
pub(super) fn parse_review(out: &Path) -> Result<(Vec<ReportedIssue>, usize), String> {
    let bytes = std::fs::read(out).map_err(|e| format!("no review file: {e}"))?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("the review file is not an array: {e}"))?;
    let total = entries.len();
    let mut issues: Vec<ReportedIssue> = entries
        .into_iter()
        .filter(|entry| {
            entry
                .get("summary")
                .is_some_and(serde_json::Value::is_string)
        })
        .map(|mut issue| {
            if !issue
                .get("blocking")
                .is_some_and(serde_json::Value::is_boolean)
            {
                issue["blocking"] = serde_json::Value::Bool(true);
            }
            ReportedIssue::from(issue)
        })
        .collect();
    let dropped = total - issues.len();
    issues.sort_by_key(|issue| !issue.blocking);
    Ok((issues, dropped))
}

/// The longest verdict note kept: one sentence of evidence, not an essay
/// (a PR review's is posted on somebody's PR).
const NOTE_CAP_CHARS: usize = 600;

/// An earlier issue a reviewer is shown and judges by its id
/// ([`parse_verdicts`]): a fix review's ([`ChainIssue`]) or a PR review's.
pub(super) trait Judged {
    /// What the verdict names it by.
    fn id(&self) -> i64;
    /// Whether the other side argued it is no issue: only then can a
    /// rebuttal be accepted.
    fn rebutted(&self) -> bool;
    /// The reviewer's verdict on it, with its `note`, and what the round
    /// was shown of it.
    fn verdict(&self, verdict: IssueVerdict, note: &str) -> Verdict;
}

/// A fix review's open issue: the fixer rebutted it when its newest
/// answer says so. A verdict on it carries nothing of a thread.
impl Judged for ChainIssue {
    fn id(&self) -> i64 {
        self.id
    }

    fn rebutted(&self) -> bool {
        self.answer()
            .is_some_and(|a| a.answer == AnswerKind::Rebutted)
    }

    fn verdict(&self, verdict: IssueVerdict, note: &str) -> Verdict {
        Verdict {
            issue_id: self.id,
            verdict,
            note: note.to_owned(),
            answers_reply_at: None,
            seen_reopened_at: None,
            seen_reply_at: None,
        }
    }
}

/// The reviewer's verdicts on `earlier` from the file at `path`, read
/// leniently: an entry naming no issue it was shown, or with a verdict
/// word it does not know, is no verdict, and an issue named twice keeps
/// its first. Silence is never a verdict: an issue the file does not name
/// stays as it was. A rebuttal can only be accepted where the other side
/// argued one ([`Judged::rebutted`]); claimed anywhere else it is no
/// verdict either. A note is trimmed and kept to [`NOTE_CAP_CHARS`].
/// `Err` when there is no usable file at all.
pub(super) fn parse_verdicts<J: Judged>(
    path: &Path,
    earlier: &[J],
) -> Result<Vec<Verdict>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("no verdict file: {e}"))?;
    let entries: Vec<serde_json::Value> =
        serde_json::from_str(&text).map_err(|e| format!("unparseable verdict file: {e}"))?;
    let mut named: HashSet<i64> = HashSet::new();
    let mut verdicts = Vec::new();
    for entry in &entries {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(issue) = earlier.iter().find(|i| i.id() == id) else {
            continue;
        };
        let Some(verdict) = entry
            .get("verdict")
            .and_then(serde_json::Value::as_str)
            .and_then(IssueVerdict::parse)
        else {
            continue;
        };
        if verdict == IssueVerdict::RebuttalAccepted && !issue.rebutted() {
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
        let note: String = note.chars().take(NOTE_CAP_CHARS).collect();
        verdicts.push(issue.verdict(verdict, &note));
    }
    Ok(verdicts)
}

/// The longest answer text kept: it goes into every later round's prompt,
/// and one pasted log must not crowd out the rest.
const ANSWER_CAP_CHARS: usize = 2000;

/// The fixer's answers from [`ANSWERS_FILE`] at `path`, as (issue id,
/// answer), read leniently as the reviewer's files are: an entry without
/// an integer `id` or with an answer word other than `addressed` or
/// `rebutted` is dropped, never taking the others with it, and an issue
/// named twice keeps its first. `text` is trimmed and kept to
/// [`ANSWER_CAP_CHARS`]; an empty `commit` is none. Which ids the fixer was
/// sent is for [`sent_answers`] to check. `Err` when the file is not a JSON
/// array.
fn parse_answers(path: &Path) -> Result<Vec<(i64, IssueAnswer)>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("no answers file: {e}"))?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("the answers file is not an array: {e}"))?;
    let text = |entry: &serde_json::Value, key: &str| {
        entry
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map_or_else(String::new, |s| s.trim().to_owned())
    };
    let mut named: HashSet<i64> = HashSet::new();
    let mut answers = Vec::new();
    for entry in &entries {
        let Some(id) = entry.get("id").and_then(serde_json::Value::as_i64) else {
            continue;
        };
        let Some(answer) = entry
            .get("answer")
            .and_then(serde_json::Value::as_str)
            .and_then(AnswerKind::parse)
        else {
            continue;
        };
        if !named.insert(id) {
            continue;
        }
        let commit = text(entry, "commit");
        answers.push((
            id,
            IssueAnswer {
                answer,
                text: text(entry, "text").chars().take(ANSWER_CAP_CHARS).collect(),
                commit: (!commit.is_empty()).then_some(commit),
            },
        ));
    }
    Ok(answers)
}

/// The review's open `issues` after a round, for its event: their
/// [`tally`], how many of them were open before it (`earlier`), and
/// whether the reviewer's changes to the tree were undone.
fn raised(issues: &[ChainIssue], earlier: &[ChainIssue], undone: bool) -> String {
    let mut raised = tally(issues.iter().map(|i| &i.issue));
    let left = issues
        .iter()
        .filter(|i| earlier.iter().any(|e| e.id == i.id))
        .count();
    if left > 0 {
        let _ = write!(raised, ", {left} of them still open from earlier rounds");
    }
    if undone {
        raised.push_str(" (the reviewer changed the worktree; undone)");
    }
    raised
}

/// An issue the review leaves open, with the number of the round that
/// raised it.
type Raised = (u32, ReportedIssue);

/// The reviewer's entries of `issues`, each with the round that raised it
/// ([`ChainIssue::round`]).
fn numbered(issues: &[ChainIssue]) -> Vec<Raised> {
    issues.iter().map(|i| (i.round, i.issue.clone())).collect()
}

/// Add a round's `raised` follow-ups to `all`: one per summary, the latest
/// round's wording winning.
fn merge_follow_ups(all: &mut Vec<ReportedIssue>, raised: Vec<ReportedIssue>) {
    for issue in raised {
        all.retain(|kept| kept.summary != issue.summary);
        all.push(issue);
    }
}

/// `", N follow-ups"` for a round's event, or nothing.
fn follow_up_tally(count: usize) -> String {
    match count {
        0 => String::new(),
        1 => ", 1 follow-up".to_owned(),
        n => format!(", {n} follow-ups"),
    }
}

/// `"N issues (M blocking)"`, for events.
fn tally<'a>(issues: impl IntoIterator<Item = &'a ReportedIssue>) -> String {
    let (count, blocking) = issues
        .into_iter()
        .fold((0, 0), |(n, b), i| (n + 1, b + usize::from(i.blocking)));
    let s = if count == 1 { "" } else { "s" };
    format!("{count} issue{s} ({blocking} blocking)")
}

/// What is left of the job's grant after `spent`: `None` for a job with no
/// token bound, an error when nothing is left.
fn remaining(cap: Option<i64>, spent: &RunResult) -> Result<Option<i64>, &'static str> {
    match cap {
        None => Ok(None),
        Some(cap) if cap > spent.tokens_new => Ok(Some(cap - spent.tokens_new)),
        Some(_) => Err("no budget left"),
    }
}

/// Add `run`'s spend to the job's.
fn add_spend(job: &mut RunResult, run: &RunResult) {
    job.tokens_new += run.tokens_new;
    job.calls += run.calls;
    job.usage_delta = match (job.usage_delta, run.usage_delta) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
}

/// Add a fixer run's spend to the job's, and make its end the job's.
fn take_run(job: &mut RunResult, run: &RunResult) {
    add_spend(job, run);
    job.exit_code = run.exit_code;
    job.killed_reason.clone_from(&run.killed_reason);
    job.stdout_tail.clone_from(&run.stdout_tail);
}

/// How a run that did not finish ended, for a note or an event.
fn run_failure(rr: &RunResult) -> String {
    match (&rr.killed_reason, rr.exit_code) {
        (Some(reason), _) => format!("was killed ({reason})"),
        (None, Some(code)) => format!("exited {code}"),
        (None, None) => "ended without an exit code".to_owned(),
    }
}

/// Log `outcome` as the event of review round `shown`, as people are
/// shown it ([`shown_round`]).
async fn log_round(store: &Store, start: &FixStart, shown: u32, outcome: &str) {
    let fid = start.finding.id;
    let message = format!("#{fid} review round {shown}: {outcome}");
    let _ = fix_event(store, "review", fid, start.job, message).await;
}

/// A review whose round `shown` failed: the fix ships, and its PR says why
/// it was not reviewed, followed by the `issues` it leaves open: earlier
/// rounds' and any the failed round's reviewer had written.
fn failed(shown: u32, why: &str, head: Option<String>, issues: &[Raised]) -> Ending {
    let mut note = format!(
        "## Pre-PR review\n\nReview round {shown} failed: {why}. This change ships \
         without a completed automated review."
    );
    if !issues.is_empty() {
        let _ = write!(note, "\n\n{}", unresolved_note(issues));
    }
    Ending {
        note: Some(note),
        head,
    }
}

/// Ship the review's open `issues` unresolved, with why the fixer did not
/// answer them when it was meant to.
fn unresolved(issues: &[Raised], unanswered: Option<&str>) -> Ending {
    let mut note = unresolved_note(issues);
    if let Some(why) = unanswered {
        let _ = write!(note, "\n\nThe fixer could not answer them: {why}.");
    }
    Ending {
        note: Some(note),
        head: None,
    }
}

/// The PR body section that hands the review's open issues to the human,
/// each with the round that raised it, blocking ones first and marked so.
fn unresolved_note(issues: &[Raised]) -> String {
    let mut note = "## Unresolved review issues\n\nThe automated pre-PR review raised these \
                    issues, and the fix ships without resolving them:\n"
        .to_owned();
    raised_list(&mut note, issues);
    note
}

/// The PR body section with the follow-ups every round raised.
fn follow_ups_note(follow_ups: &[ReportedIssue]) -> String {
    let mut note = "## Follow-ups from review\n\nThe automated pre-PR review found these \
                    defects outside this change's scope. They are not addressed here:\n"
        .to_owned();
    issue_list(&mut note, follow_ups);
    note
}

/// The PR body section that says what the pre-PR review came to, rendered
/// from the chain's review: how many rounds read the change, how many
/// issues they raised for the fixer, how many of those it addressed, had a
/// rebuttal accepted, or left unresolved, and every one of them by name
/// under its outcome. An unresolved issue whose newest answer no round
/// judged (the review switched off after the fixer answered it, or the
/// rounds after it were silent on it) is shown with that answer, unjudged
/// ([`ChainIssue::unjudged_answer`]). Follow-ups ship in a
/// section of their own and are not counted here. Empty when no round
/// raised an issue to
/// answer: a clean review adds nothing to the body. The one place the
/// review's record reaches the PR: rendering the whole back-and-forth
/// instead (each issue's answers and verdicts, round by round, all in
/// `review`) changes this function alone.
fn review_summary(review: &FixReview) -> String {
    let raised: Vec<&ChainIssue> = review
        .issues
        .iter()
        .filter(|i| !i.issue.is_follow_up())
        .collect();
    if raised.is_empty() {
        return String::new();
    }
    let settled =
        |verdict: IssueVerdict| raised.iter().filter(move |i| i.settled() == Some(verdict));
    let addressed = settled(IssueVerdict::Addressed).count();
    let rebutted = settled(IssueVerdict::RebuttalAccepted).count();
    let unresolved = raised.len() - addressed - rebutted;
    let (rounds, issues) = (review.rounds.len(), raised.len());
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let mut note = format!(
        "## Review\n\nAn automated reviewer read this change in {rounds} round{} before \
         its PR was opened, and raised {issues} issue{}: {addressed} addressed, {rebutted} \
         rebutted, {unresolved} unresolved.\n",
        plural(rounds),
        plural(issues),
    );
    for (word, verdict) in [
        ("Addressed", Some(IssueVerdict::Addressed)),
        ("Rebutted", Some(IssueVerdict::RebuttalAccepted)),
        ("Unresolved", None),
    ] {
        let named = raised.iter().filter(|i| match verdict {
            Some(v) => i.settled() == Some(v),
            None => !matches!(
                i.settled(),
                Some(IssueVerdict::Addressed | IssueVerdict::RebuttalAccepted)
            ),
        });
        for issue in named {
            let _ = write!(note, "\n- {word}: **{}**", issue.issue.summary);
            if let Some(answer) = issue.unjudged_answer() {
                let kind = answer.answer.as_str();
                let _ = write!(note, " (the fixer's answer, not reviewed: {kind})");
                for line in answer.text.lines() {
                    note.push_str("\n  ");
                    note.push_str(line);
                }
            }
        }
    }
    note.trim_end().to_owned()
}

/// `issues` as a Markdown list on `note`: blocking ones marked, each with
/// its place and detail.
fn issue_list(note: &mut String, issues: &[ReportedIssue]) {
    for issue in issues {
        list_item(note, "", issue);
    }
}

/// [`issue_list`] for issues the review leaves open, each led by the
/// round that raised it.
fn raised_list(note: &mut String, issues: &[Raised]) {
    for (round, issue) in issues {
        list_item(note, &format!("Round {round}: "), issue);
    }
}

/// One entry of [`issue_list`], after `lead`.
fn list_item(note: &mut String, lead: &str, issue: &ReportedIssue) {
    let place = match (issue.file.as_deref(), issue.line) {
        (None | Some(""), _) => String::new(),
        (Some(file), None | Some(0)) => format!(" (`{file}`)"),
        (Some(file), Some(line)) => format!(" (`{file}:{line}`)"),
    };
    let mark = if issue.blocking { "[blocking] " } else { "" };
    let _ = write!(note, "\n- {lead}{mark}**{}**{place}", issue.summary);
    for line in issue.detail.lines() {
        note.push_str("\n  ");
        note.push_str(line);
    }
}
