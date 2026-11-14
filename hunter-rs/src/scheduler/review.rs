//! The pre-PR review of a finished fix (WORK-MODEL.md §5.7).
//!
//! A fixer that finished with commits and a `PR-DESCRIPTION.md` is not
//! shipped at once. A fresh reviewer session, told to distrust the change,
//! reads the whole `<pinned>..HEAD` and writes its issues to a file outside
//! the tree. Its issues go back to the fixer, resumed in its own session,
//! which fixes or rebuts each; then the whole change is reviewed again,
//! until a review raises none or `review.maxRounds` reviews have run. Issues
//! outside the change's scope (`follow-up`s) are never answered: they ship
//! in the PR body for later work. Replayed over 88 shipped fixes,
//! such a reviewer named most of the bugs later hunts traced back to
//! hunter's own PRs, and flagged few clean ones.
//!
//! All of it runs inside the fix job: the finding stays `fixing` under the
//! same claim, and the job row records every run's spend under the one
//! budget grant. The reviewer never holds the fix hostage: a review that
//! cannot be had ships the fix as before, saying so in the PR body.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{BLOCKED_REPORT, FixStart, decline_report, fix_event, job_state};
use crate::backend::{Backend, JobClass};
use crate::clones::{Lane, Unheld};
use crate::config::Config;
use crate::domain::FinishedState;
use crate::playbooks;
use crate::recovery::CycleScope;
use crate::store::{ReportedIssue, Store};
use crate::types::{JobPhase, RunResult};
use crate::workspace::Workspace;

/// The `killed_reason` of a fix job the daemon's shutdown stopped between
/// two of its runs: a pause on the fixer's transcript (`job_state`).
pub(super) const STOPPED: &str = "shutdown";

/// The `killed_reason` of a fix job the review suspended between two of
/// its runs because its grant was spent ([`Rounds::spent`],
/// [`Rounds::curation_owed`]): a pause on the fixer's transcript, as a
/// cap is (`job_state`), but not a resume attempt, as a shutdown stop is
/// not (`Store::resume_chain_stats`): the work is done up to there, and
/// the resume only carries on with it.
pub(super) const GRANT: &str = "grant";

/// In the workspace root: how far an unfinished review loop got
/// ([`Progress`]). A resumed attempt reviews from the round after the
/// last one sent back, so a fixer suspended while answering a review does
/// not restart the count. Reset to round 0 once the loop ends, keeping the
/// title and the follow-ups for any later review of the chain.
const PROGRESS_FILE: &str = "review-progress.json";

/// In the workspace root: the verdict a review loop reached ([`Saved`]),
/// for the tree it reviewed. An attempt that finds the tree exactly so (an
/// offline push retried) ships on it without another review; any other
/// tree is reviewed again.
const ENDING_FILE: &str = "review-ending.json";

/// In the workspace root, while a curation runs ([`curate`]): the tree
/// it started from ([`ReviewedTree`]). Left behind only when the daemon
/// died during the curation; the next attempt puts the tree back to it
/// before anything reads the tree ([`recover_curation`]).
const CURATING_FILE: &str = "review-curating.json";

/// In the workspace root: the tree an agent was shown whose changes could
/// not be undone, and which agent that was ([`Restore`]): a failed round's
/// reviewer, or a curator, rejected or cut off by a restart
/// ([`recover_curation`]). Recorded before the attempt goes on, whatever
/// it then amounts to (a hold, a retry once a host is reachable again, a
/// pinned commit whose publish failed), so every later attempt of the
/// chain puts the tree back to it before any worker runs in it
/// ([`restore_reviewed`]).
const RESTORE_FILE: &str = "review-restore.json";

/// A reviewed tree to put back to ([`undo`]), and the fixer's description:
/// the one a curation started from ([`CURATING_FILE`]), or the one an
/// agent was shown and could not be undone from ([`Restore`]).
#[derive(Debug, Serialize, Deserialize)]
struct ReviewedTree {
    before: TreeState,
    description: Vec<u8>,
}

/// Which agent changed a tree that is put back to the one it was shown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum LeftBy {
    /// A failed review round's reviewer: the fixer's session never saw
    /// its changes.
    #[default]
    Reviewer,
    /// A curator, which ran in the fixer's session: its curation is owed
    /// again once its changes are undone ([`owe_curation`]).
    Curator,
}

/// What [`RESTORE_FILE`] holds: the tree to put back to, and whose
/// changes that undoes.
#[derive(Debug, Serialize, Deserialize)]
struct Restore {
    #[serde(flatten)]
    tree: ReviewedTree,
    #[serde(default)]
    by: LeftBy,
}

/// What the review loop leaves for the job's record and its PR.
pub(super) struct Reviewed {
    /// Every run of the job, summed: tokens, calls and usage. The
    /// exit, kill and output tail are those of the fixer's last run, which
    /// decides the job's state ([`STOPPED`] when shutdown stopped the loop);
    /// the session file stays the fixer's.
    pub rr: RunResult,
    /// What to ship with, once the tree may ship: the loop ended in this
    /// attempt, a saved verdict covers the tree as it is, or the review is
    /// off. `None` otherwise -- the fixer did not finish (a pause, a
    /// failure, half an answer to a review), left nothing to ship, or the
    /// daemon stopped the loop -- and then nothing ships.
    pub ending: Option<Ending>,
    /// The fixer's answer to a review failed without doing work while this
    /// host is unreachable: the network's failure, not the fix's. The
    /// attempt is kept unpublished for a retry (`FixOutcome::Offline`).
    pub offline: Option<String>,
}

/// How the review loop ended: what `ship_pr` ships.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Ending {
    /// Appended to the PR body: unresolved issues, or why the review
    /// failed. `None` for a clean review, or no review.
    pub note: Option<String>,
    /// The commit to ship in place of `HEAD`: set when a reviewer moved
    /// `HEAD`, so what ships is what was reviewed, not the reviewer's
    /// commits.
    pub head: Option<String>,
    /// The PR title: the subject of the commit the fixer finished on
    /// before the first review round, so commits answering the review do
    /// not retitle the fix. `None`: the shipped commit's subject.
    pub title: Option<String>,
}

/// A verdict the review reached, kept for the tree it covers. Only a
/// reviewer's verdict is kept (clean, or issues left after the last
/// round): a review that failed, or issues the fixer never got to answer,
/// are had again on the next attempt.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    note: Option<String>,
    title: Option<String>,
    tree: TreeState,
}

/// How far an unfinished review loop got ([`PROGRESS_FILE`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Progress {
    /// The last round whose issues went back to the fixer, or are owed
    /// to it ([`Owed::Answer`]).
    rounds: u32,
    /// [`Ending::title`], taken before round 1.
    title: Option<String>,
    /// The follow-ups the rounds so far raised ([`merge_follow_ups`]).
    #[serde(default)]
    follow_ups: Vec<ReportedIssue>,
    /// The part of the fix a suspended job still owes ([`Rounds::pause`]),
    /// which its resume carries out first; `None` when the resume goes on
    /// with the next review round.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owed: Option<Owed>,
    /// The job the review suspended between its runs, its fixer's last one
    /// finished ([`Rounds::pause`]): the attempt that continues it has no
    /// fixer run to continue, and goes straight back to the loop
    /// ([`suspended_between_runs`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    paused_by: Option<i64>,
}

/// What a job suspended between its runs still owes, besides reviewing
/// again ([`Progress::owed`]).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Owed {
    /// The fixer's answer to round [`Progress::rounds`]'s issues.
    Answer(Vec<ReportedIssue>),
    /// The curation of a review that is through, its verdict saved for the
    /// tree ([`ENDING_FILE`]).
    Curation,
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
        /// The tree before the reviewer ran, when it could not be put back
        /// to it: its commit is the one that ships, and the one every later
        /// attempt puts the tree back to before a worker runs
        /// ([`RESTORE_FILE`]).
        left: Option<ReviewedTree>,
        issues: Vec<ReportedIssue>,
    },
}

/// The PR body line saying a round's reviewer changed the worktree.
fn undone_note(round: u32) -> String {
    format!(
        "The reviewer of round {round} changed the worktree; its changes were undone, and its \
         issues stand."
    )
}

/// What the loop's runs share: where they run and what bounds them.
struct Rounds<'a> {
    scope: &'a CycleScope<'a>,
    cfg: &'a Config,
    backend: &'a dyn Backend,
    start: &'a FixStart,
    /// [`Ending::title`], kept across attempts and holds.
    title: Option<String>,
}

impl Rounds<'_> {
    fn store(&self) -> &Store {
        self.scope.store()
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
    /// ([`Self::suspend`]). `owed`: the round reviewed last, and its
    /// issues the fixer has not been sent yet.
    async fn stopped(
        &self,
        job: &mut Reviewed,
        owed: (u32, &[ReportedIssue]),
        follow_ups: &[ReportedIssue],
    ) -> bool {
        if !self.scope.stopping() {
            return false;
        }
        let stop = "the daemon is stopping; suspended";
        self.suspend(job, owed, follow_ups, (STOPPED, stop)).await
    }

    /// Suspend the job when its grant is spent before the review is
    /// through (`why`): before a round could run, by the reviewer it
    /// killed, or before the fixer could answer a round ([`Self::suspend`],
    /// as a cap kill). The resume, under a fresh grant, carries on where
    /// this one stopped: a review cut short is never shipped as one.
    async fn spent(
        &self,
        job: &mut Reviewed,
        owed: (u32, &[ReportedIssue]),
        follow_ups: &[ReportedIssue],
        why: &str,
    ) -> bool {
        let stop = format!("{why}; suspended to go on under a fresh grant");
        self.suspend(job, owed, follow_ups, (GRANT, &stop)).await
    }

    /// Suspend the job between two of its runs, the fixer's last one
    /// finished, as `reason` (its `killed_reason`, which `job_state` makes
    /// a suspension), logging `stop`. `owed` is the round reviewed last
    /// and its issues the fixer has not been sent: the progress records
    /// them, so the resume sends them before it reviews again, and names
    /// this job as paused, so its resume runs no fixer before that
    /// ([`suspended_between_runs`]). Only with a transcript to resume;
    /// without one, `false`, and the loop goes on as if there were no stop.
    async fn suspend(
        &self,
        job: &mut Reviewed,
        (reviewed, issues): (u32, &[ReportedIssue]),
        follow_ups: &[ReportedIssue],
        (reason, stop): (&str, &str),
    ) -> bool {
        let progress = Progress {
            rounds: reviewed,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
            owed: (!issues.is_empty()).then(|| Owed::Answer(issues.to_vec())),
            ..Progress::default()
        };
        let round = if issues.is_empty() {
            reviewed + 1
        } else {
            reviewed
        };
        let logged = format!("review round {round}: {stop}");
        self.pause(job, progress, (reason, &logged)).await
    }

    /// Suspend the job between two of its runs, the fixer's last one
    /// finished, as `reason` (its `killed_reason`, which `job_state` makes
    /// a suspension), logging `stop`: `progress` is recorded, naming this
    /// job as paused, so its resume runs no fixer and goes straight back
    /// to where this one stopped ([`suspended_between_runs`]). Only with a
    /// transcript to resume; without one, `false`, and nothing is written.
    async fn pause(
        &self,
        job: &mut Reviewed,
        progress: Progress,
        (reason, stop): (&str, &str),
    ) -> bool {
        if job.rr.session_file.is_none() {
            return false;
        }
        let progress = Progress {
            paused_by: Some(self.start.job),
            ..progress
        };
        write_progress(self.start, &progress);
        job.rr.killed_reason = Some(reason.to_owned());
        job.rr.exit_code = None;
        let fid = self.start.finding.id;
        let message = format!("#{fid} {stop}");
        let _ = fix_event(self.store(), "review", fid, self.start.job, message).await;
        true
    }

    /// Suspend the job before its curation, the review through and its
    /// verdict (`ending`) saved for the tree, so its resume curates and
    /// ships without another review ([`Progress::curate`]). The daemon's
    /// shutdown suspends it as `shutdown`, a spent grant (`why`) as `cap`.
    async fn curation_owed(
        &self,
        job: &mut Reviewed,
        ending: &Ending,
        follow_ups: &[ReportedIssue],
        why: &str,
    ) -> bool {
        let reason = if self.scope.stopping() {
            STOPPED
        } else {
            GRANT
        };
        if job.rr.session_file.is_none() {
            return false;
        }
        save_ending(self.start, ending).await;
        let progress = Progress {
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
            owed: Some(Owed::Curation),
            ..Progress::default()
        };
        let stop = format!("curation: {why}; suspended to curate under a fresh grant");
        self.pause(job, progress, (reason, &stop)).await
    }
}

/// What becomes of a round's issues when no answer is coming
/// ([`Rounds::unanswered`]).
enum Unanswered {
    /// An answer is coming: send them back to the fixer.
    No,
    /// The fix is held for the operator ([`hold`]).
    Held,
    /// They ship unresolved; `keep` saves the verdict for the tree.
    Ship(Ending, bool),
    /// A run did no work while this host is unreachable: retry later.
    Offline(String),
    /// The grant is spent: the job is suspended, and its resume answers
    /// them ([`Rounds::spent`]).
    Suspended,
}

impl Rounds<'_> {
    /// Whether a run of round `round` that did no work did so because a
    /// host the work needs is unreachable ([`super::unreachable_now`]):
    /// the network's failure, handled as the fix's other offline failures
    /// are (`FixOutcome::Offline`). The progress is rewound so the retry
    /// reviews that round again, keeping the title and `follow_ups`.
    async fn offline(&self, round: u32, follow_ups: &[ReportedIssue]) -> Option<String> {
        let start = self.start;
        let host = super::unreachable_now(self.cfg, &start.repo.url, self.backend).await?;
        let again = Progress {
            rounds: round.saturating_sub(1),
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
            ..Progress::default()
        };
        write_progress(start, &again);
        let message = format!("{host} unreachable; kept to review round {round} again");
        log_round(self.store(), start, round, &message).await;
        Some(host)
    }

    /// What a failed round amounts to (`why`). A reviewer whose changes
    /// could not be undone (`left`) has the tree it was shown recorded
    /// first ([`record_restore`]), however the round ends: no later worker
    /// builds on what it left. With a host the work needs unreachable,
    /// whatever the reviewer did, the network's failure
    /// ([`Self::offline`]): an outage that cut a review off mid-way is no
    /// more the fix's than one that kept it from starting.
    /// Otherwise the fix ships, saying why it was not reviewed: the
    /// reviewer never holds it hostage. But issues the reviewer wrote
    /// before the round failed hold the fix as an answered round's would
    /// ([`Self::holds`]).
    async fn failed_round(
        &self,
        round: u32,
        (why, left, issues): (&str, Option<ReviewedTree>, &[ReportedIssue]),
        follow_ups: &[ReportedIssue],
        also: &str,
    ) -> Unanswered {
        let start = self.start;
        let head = left.and_then(|tree| {
            let head = tree.before.head.clone();
            record_restore(
                start,
                &Restore {
                    tree,
                    by: LeftBy::Reviewer,
                },
            );
            head
        });
        if let Some(host) = self.offline(round, follow_ups).await {
            return Unanswered::Offline(host);
        }
        let progress = Progress {
            rounds: 0,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
            ..Progress::default()
        };
        let restoring = head.as_deref();
        if self.holds(round, issues) && hold(start, round, issues, (&progress, why, restoring)) {
            let message = format!(
                "failed: {why}; {}{also}; held for the operator",
                tally(issues)
            );
            log_round(self.store(), start, round, &message).await;
            return Unanswered::Held;
        }
        let message = format!("failed: {why}; {}{also}; shipping", tally(issues));
        log_round(self.store(), start, round, &message).await;
        Unanswered::Ship(failed(round, why, head, issues), false)
    }

    /// Whether round `round`'s unanswered `issues` hold the fix for the
    /// operator instead of shipping: any of them in the last round (a
    /// review still finding something after `review.maxRounds` rounds is
    /// not converging), else only a blocking one.
    fn holds(&self, round: u32, issues: &[ReportedIssue]) -> bool {
        let last = round >= self.cfg.review_max_rounds;
        (last && !issues.is_empty()) || issues.iter().any(|i| i.blocking)
    }

    /// Whether round `round`'s `issues` get no answer, and what then: none
    /// comes in the last round, and any issue it left holds the fix for
    /// the operator: a review that still finds something after
    /// `review.maxRounds` rounds is not converging, and its change is not
    /// shipped on the hope that the rest is harmless. When the grant is
    /// spent (a reviewer killed at its cap) and leaves nothing to answer
    /// them with, the job is suspended for its resume to answer them
    /// ([`Rounds::spent`]); only a job with no transcript to resume holds
    /// the fix for a blocking issue and ships the rest unresolved. `raised`
    /// is the round's tally for the event.
    async fn unanswered(
        &self,
        job: &mut Reviewed,
        round: u32,
        (issues, undone): (&[ReportedIssue], bool),
        follow_ups: &[ReportedIssue],
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
            || format!("the last of {max} rounds"),
            |why| format!("{why} to answer them"),
        );
        let owed = (round, issues);
        let suspended = format!("{raised}, {stop}");
        if why.is_some() && self.spent(job, owed, follow_ups, &suspended).await {
            return Unanswered::Suspended;
        }
        let progress = Progress {
            rounds: 0,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
            ..Progress::default()
        };
        if self.holds(round, issues) && hold(start, round, issues, (&progress, &stop, None)) {
            let message = format!("{raised}, {stop}; held for the operator");
            log_round(self.store(), start, round, &message).await;
            return Unanswered::Held;
        }
        let message = format!("{raised}, {stop}; shipping with them");
        log_round(self.store(), start, round, &message).await;
        let mut ending = unresolved(round, issues, why);
        if undone && let Some(note) = &mut ending.note {
            let _ = write!(note, "\n\n{}", undone_note(round));
        }
        Unanswered::Ship(ending, why.is_none())
    }
}

/// Review the fixer's finished work in `start`'s tree, and send every
/// issue the reviewer raises back to it, until a review raises none or
/// `review.maxRounds` reviews have run. Whether an issue is `blocking` only
/// orders it and marks it for the human; every one is answered but a
/// follow-up, which is collected across rounds and shipped in the PR body
/// ([`follow_ups_note`]). `fixer` is the attempt's first run; the result
/// sums it with every run after it.
///
/// A saved verdict that covers the tree exactly as it is ships it, however
/// this attempt's fixer run ended: a retried offline push whose resume
/// failed before doing anything changed nothing that was reviewed.
/// Otherwise nothing is reviewed unless the fixer would ship as it stands
/// ([`ready_to_ship`]). A fixer that is suspended or writes a report while
/// answering a review ends the loop there: the job is recorded on its
/// state and `conclude_fix` reads its report, as for any fix. So does the
/// daemon's shutdown, checked before every run ([`Rounds::stopped`]), and a
/// grant spent before the review is through ([`Rounds::spent`]): either
/// suspends the job, and its resume carries on with the issues a round
/// left unanswered, or else with the round that did not run to its end.
pub(super) async fn review_fix(
    scope: &CycleScope<'_>,
    lane: &mut Lane,
    cfg: &Config,
    backend: &dyn Backend,
    start: &FixStart,
    fixer: RunResult,
) -> Reviewed {
    let mut job = Reviewed {
        rr: fixer,
        ending: None,
        offline: None,
    };
    // Off, or a saved verdict that covers the tree: no review to run.
    job.ending = if cfg.review_max_rounds == 0 {
        Some(Ending::default())
    } else {
        saved_ending(start).await
    };
    if let Some(ending) = job.ending.take() {
        let rounds = (scope, cfg, backend, start);
        return ship_saved(rounds, lane, job, ending).await;
    }
    if !ready_to_ship(start, &job.rr).await {
        return job;
    }
    let root = &start.ws.root;
    let progress = read_json::<Progress>(&root.join(PROGRESS_FILE)).unwrap_or_default();
    let title = match progress.title {
        Some(title) => Some(title),
        None => head_subject(&start.ws.tree).await,
    };
    let rounds = Rounds {
        scope,
        cfg,
        backend,
        start,
        title: title.clone(),
    };
    // The round reviewed last, and its issues the fixer is owed an answer
    // to: a suspended attempt's, sent before anything is reviewed again.
    let mut round = progress.rounds;
    let mut owed = match progress.owed {
        Some(Owed::Answer(issues)) => issues,
        Some(Owed::Curation) | None => Vec::new(),
    };
    let mut follow_ups = progress.follow_ups;
    let (mut ending, keep) = loop {
        let (issues, raised) = if owed.is_empty() {
            round += 1;
            match next_round(&rounds, lane, &mut job, round, &mut follow_ups).await {
                Next::Answer { issues, raised } => (issues, raised),
                Next::Ship(ending, keep) => break (ending, keep),
                Next::End => return job,
            }
        } else {
            let issues = std::mem::take(&mut owed);
            let raised = format!("{} owed since the job was suspended", tally(&issues));
            // Owed issues get the grant check a round's own get
            // (`Rounds::unanswered`): a fixer run before them (an
            // operator's `continue`) may have spent it all.
            if let Err(why) = rounds.allowance(&job.rr) {
                let stop = format!("{raised}, {why} to answer them");
                if rounds
                    .spent(&mut job, (round, &issues), &follow_ups, &stop)
                    .await
                {
                    return job;
                }
            }
            (issues, raised)
        };
        if rounds
            .stopped(&mut job, (round, &issues), &follow_ups)
            .await
        {
            return job;
        }
        let message = format!("{raised}; back to the fixer");
        log_round(rounds.store(), start, round, &message).await;
        let progress = Progress {
            rounds: round,
            title: title.clone(),
            follow_ups: follow_ups.clone(),
            ..Progress::default()
        };
        match send_back(&rounds, &mut job, &progress, &issues).await {
            Answered::ReviewAgain => {}
            Answered::Conclude | Answered::Held => return job,
            Answered::Offline(host) => {
                job.offline = Some(host);
                return job;
            }
            Answered::Ship(ending) => break (ending, false),
        }
    };
    ending.title = title;
    if finish(&rounds, lane, &mut job, &mut ending, follow_ups, keep).await {
        job.ending = Some(ending);
    }
    job
}

/// Ship on `ending`, a saved verdict that covers the tree, curating first
/// when the job this attempt continues was suspended with its curation
/// owed ([`Progress::curate`]). The mark is taken here: a later attempt
/// curates only if this one is suspended again.
async fn ship_saved(
    (scope, cfg, backend, start): (&CycleScope<'_>, &Config, &dyn Backend, &FixStart),
    lane: &mut Lane,
    mut job: Reviewed,
    mut ending: Ending,
) -> Reviewed {
    let progress = read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE)).unwrap_or_default();
    if !matches!(progress.owed, Some(Owed::Curation)) {
        job.ending = Some(ending);
        return job;
    }
    let follow_ups = progress.follow_ups;
    let taken = Progress {
        title: progress.title.clone(),
        follow_ups: follow_ups.clone(),
        ..Progress::default()
    };
    write_progress(start, &taken);
    let rounds = Rounds {
        scope,
        cfg,
        backend,
        start,
        title: progress.title,
    };
    match curate(&rounds, lane, &mut job, &mut ending, &follow_ups).await {
        Curated::Accepted => save_ending(start, &ending).await,
        Curated::Not => {}
        Curated::Suspended => return job,
    }
    job.ending = Some(ending);
    job
}

/// Where review round `round` leaves the loop ([`next_round`]).
enum Next {
    /// Its issues go back to the fixer; `raised` is their tally.
    Answer {
        issues: Vec<ReportedIssue>,
        raised: String,
    },
    /// The fix ships; `keep` saves the verdict for the tree.
    Ship(Ending, bool),
    /// The job ends on its state: held, suspended, or kept while a host
    /// is unreachable.
    End,
}

/// Run review round `round`, unless the daemon is stopping or the grant
/// is spent, either of which suspends the job, as does a reviewer killed
/// at its cap before it wrote an issue ([`Rounds::spent`]): the resume
/// has the round again. Then what its verdict amounts to.
async fn next_round(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    round: u32,
    follow_ups: &mut Vec<ReportedIssue>,
) -> Next {
    let start = rounds.start;
    let before = (round - 1, &[][..]);
    if rounds.stopped(job, before, follow_ups).await {
        return Next::End;
    }
    let (cap, wall) = match rounds.allowance(&job.rr) {
        Ok(allowance) => allowance,
        Err(why) => {
            let why = format!("{why} to review it");
            if rounds.spent(job, before, follow_ups, &why).await {
                return Next::End;
            }
            return Next::Ship(failed(round, &why, None, &[]), false);
        }
    };
    let (run, verdict, raised_follow_ups) = review_round(rounds, lane, round, cap, wall).await;
    let capped = run
        .as_ref()
        .is_some_and(|run| run.killed_reason.as_deref() == Some("cap"));
    if let Some(run) = &run {
        add_spend(&mut job.rr, run);
    }
    let also = follow_up_tally(raised_follow_ups.len());
    merge_follow_ups(follow_ups, raised_follow_ups);
    let (issues, undone) = match verdict {
        // The grant ran out on a reviewer that wrote nothing: no review
        // was had, and the resume has it again.
        RoundVerdict::Failed { why, left, .. }
            if capped && left.is_none() && rounds.spent(job, before, follow_ups, &why).await =>
        {
            return Next::End;
        }
        RoundVerdict::Failed { why, left, issues } => {
            let failure = (why.as_str(), left, issues.as_slice());
            return match rounds.failed_round(round, failure, follow_ups, &also).await {
                Unanswered::Ship(ending, keep) => Next::Ship(ending, keep),
                Unanswered::Offline(host) => {
                    job.offline = Some(host);
                    Next::End
                }
                Unanswered::Held | Unanswered::No | Unanswered::Suspended => Next::End,
            };
        }
        RoundVerdict::Clean => {
            let message = format!("no issues to answer{also}; shipping");
            log_round(rounds.store(), start, round, &message).await;
            return Next::Ship(Ending::default(), true);
        }
        RoundVerdict::Issues { issues, undone } => (issues, undone),
    };
    let raised = format!("{}{also}", raised(&issues, undone));
    match rounds
        .unanswered(job, round, (&issues, undone), follow_ups, &raised)
        .await
    {
        Unanswered::No => Next::Answer { issues, raised },
        Unanswered::Held | Unanswered::Offline(_) | Unanswered::Suspended => Next::End,
        Unanswered::Ship(ending, keep) => Next::Ship(ending, keep),
    }
}

/// End a loop that ships: the follow-ups go into the PR body, the loop's
/// files are ended (the verdict saved for the reviewed tree when `keep`),
/// and the history is curated ([`curate`]); an accepted curation saves the
/// verdict again, for the curated tree. Saving before the curation is what
/// lets an attempt that recovers from a curation the daemon died in ship
/// the reviewed tree without another review. `false` when the job was
/// suspended before its curation instead: nothing ships in this attempt.
async fn finish(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    ending: &mut Ending,
    follow_ups: Vec<ReportedIssue>,
    keep: bool,
) -> bool {
    let start = rounds.start;
    with_follow_ups(ending, &follow_ups);
    end_loop(start, ending, follow_ups.clone(), keep).await;
    match curate(rounds, lane, job, ending, &follow_ups).await {
        Curated::Accepted => end_loop(start, ending, follow_ups, keep).await,
        Curated::Not => {}
        Curated::Suspended => return false,
    }
    true
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
/// is one commit, there is no transcript to resume, or `ending` already
/// pins a commit (a reviewer moved `HEAD`). The daemon's shutdown, a grant
/// with nothing left, or a curator killed at its cap (its rewrite undone)
/// suspend the job instead, its curation owed ([`Rounds::curation_owed`]):
/// a fix is not shipped uncurated because the budget ran out. When the
/// tree cannot be put back, `ending` pins the reviewed commit, so that is
/// what ships.
async fn curate(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    ending: &mut Ending,
    follow_ups: &[ReportedIssue],
) -> Curated {
    if ending.head.is_some() {
        return Curated::Not;
    }
    let Some(due) = curation_due(rounds, job).await else {
        return Curated::Not;
    };
    let stop = if rounds.scope.stopping() {
        Some("the daemon is stopping")
    } else {
        rounds.allowance(&job.rr).err()
    };
    if let Some(why) = stop
        && rounds.curation_owed(job, ending, follow_ups, why).await
    {
        return Curated::Suspended;
    }
    curated_head(rounds, lane, job, (ending, follow_ups), due).await
}

/// How a curation went.
enum Curated {
    /// The curated history ships.
    Accepted,
    /// Skipped or rejected: the original commits ship, or the reviewed
    /// commit `ending` now pins when a rejected rewrite could not be undone.
    Not,
    /// The job was suspended with its curation owed.
    Suspended,
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

/// [`curate`]'s work. While it runs, [`CURATING_FILE`] records the
/// reviewed tree, so a daemon that dies in it leaves the next attempt
/// what to put back ([`recover_curation`]).
async fn curated_head(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut Reviewed,
    (ending, follow_ups): (&mut Ending, &[ReportedIssue]),
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
    let marker = start.ws.root.join(CURATING_FILE);
    let recorded = serde_json::to_vec(&curating)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&marker, bytes));
    if let Err(e) = recorded {
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
    let outcome = match verdict {
        Ok(curated) => {
            note(format!("{commits} commits rewritten into {curated}")).await;
            Curated::Accepted
        }
        Err(why) => {
            let undone = undo_or_record(lane, start, curating).await;
            let _ = std::fs::remove_file(&marker);
            // A curator the grant ran out on curated nothing: the resume
            // has it again, once its rewrite is undone.
            if capped && undone.is_ok() && rounds.curation_owed(job, ending, follow_ups, &why).await
            {
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
    };
    let _ = std::fs::remove_file(&marker);
    outcome
}

/// Put the tree back to the reviewed commit when the daemon died during a
/// curation ([`CURATING_FILE`] left behind): [`undo`] to the reviewed tree
/// and description. Run first in a later attempt of the chain, before
/// anything reads the tree. The curation is owed again ([`owe_curation`]):
/// the attempt runs no fixer, and curates on the verdict saved for the
/// reviewed tree before it ships, as any part of the fix a stop cut short
/// is had again. A tree that could not be put back is recorded for
/// [`restore_reviewed`] ([`undo_or_record`]), which runs next: no worker
/// runs on it until it is.
pub(super) async fn recover_curation(lane: &mut Lane, store: &Store, start: &FixStart) {
    let marker = start.ws.root.join(CURATING_FILE);
    let Some(curating) = read_json::<ReviewedTree>(&marker) else {
        return;
    };
    let undone = undo_or_record(lane, start, curating).await;
    let outcome = match &undone {
        Ok(()) => "the reviewed commit restored".to_owned(),
        Err(e) => format!("restoring the reviewed commit failed: {e}"),
    };
    owe_curation(start);
    let _ = std::fs::remove_file(&marker);
    let fid = start.finding.id;
    let message =
        format!("#{fid} curation: interrupted by a restart; {outcome}; to be curated again");
    let _ = fix_event(store, "review", fid, start.job, message).await;
}

/// Record that `start`'s fix owes its curation again ([`Owed::Curation`]),
/// its review through and its verdict saved for the reviewed tree: a
/// curation cut short by a restart, or whose changes had to be put back.
fn owe_curation(start: &FixStart) {
    let progress = read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE)).unwrap_or_default();
    write_progress(
        start,
        &Progress {
            title: progress.title,
            follow_ups: progress.follow_ups,
            owed: Some(Owed::Curation),
            ..Progress::default()
        },
    );
}

/// Whether `start`'s fix owes only its curation ([`Owed::Curation`]): its
/// review is through, so the attempt runs no fixer, whatever the hold an
/// operator released, and goes straight to the curation.
pub(super) fn owes_curation(start: &FixStart) -> bool {
    read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE))
        .is_some_and(|progress| matches!(progress.owed, Some(Owed::Curation)))
}

/// How many commits `start`'s branch has on top of its pinned commit, if
/// git can say.
async fn commit_count(start: &FixStart) -> Option<usize> {
    let range = format!("{}..HEAD", start.pinned);
    let tree = start.ws.tree.clone();
    let (rc, commits) =
        tokio::task::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
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
    let (same, descends) = tokio::task::spawn_blocking(move || {
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

/// The saved verdict ([`ENDING_FILE`]), if it covers `start`'s tree
/// exactly as it is now.
async fn saved_ending(start: &FixStart) -> Option<Ending> {
    let saved = covering_verdict(&start.ws).await?;
    Some(Ending {
        note: saved.note,
        head: None,
        title: saved.title,
    })
}

/// The verdict saved in `ws` ([`ENDING_FILE`]), if it covers its tree
/// exactly as it is now. A file left from a verdict whose tree has since
/// changed (a rejected curation that could not be undone, which pins the
/// reviewed commit instead) covers nothing.
async fn covering_verdict(ws: &Workspace) -> Option<Saved> {
    let saved = read_json::<Saved>(&ws.root.join(ENDING_FILE))?;
    let now = tree_state(&ws.tree).await;
    // A verdict saved by an earlier daemon holds its status trimmed at
    // both ends, which dropped the first line's leading blank: compare
    // the statuses as that trim leaves them.
    let status = |t: &TreeState| t.status.as_deref().map(str::trim_start).map(str::to_owned);
    let covers = saved.tree.head.is_some()
        && (
            &saved.tree.head,
            &saved.tree.branch,
            &saved.tree.description,
        ) == (&now.head, &now.branch, &now.description)
        && status(&saved.tree) == status(&now);
    covers.then_some(saved)
}

/// End the loop's files. Its progress is reset to round 0, keeping the
/// title and the `follow_ups`: a later attempt that finds the tree changed
/// (an offline push retried after the fixer went on) is reviewed from
/// round 1 again, under the same title and with them. `ending` is saved
/// for the tree as it is when `keep` (a reviewer's own verdict).
async fn end_loop(start: &FixStart, ending: &Ending, follow_ups: Vec<ReportedIssue>, keep: bool) {
    let root = &start.ws.root;
    let progress = Progress {
        rounds: 0,
        title: ending.title.clone(),
        follow_ups,
        ..Progress::default()
    };
    let reset = serde_json::to_vec(&progress)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(root.join(PROGRESS_FILE), bytes));
    if let Err(e) = reset {
        tracing::warn!(
            "fix #{}: resetting the review's progress: {e}",
            start.finding.id
        );
    }
    let _ = std::fs::remove_file(root.join(ENDING_FILE));
    let _ = std::fs::remove_file(root.join(HELD_FILE));
    if keep {
        save_ending(start, ending).await;
    }
}

/// Save `ending` ([`ENDING_FILE`]) for `start`'s tree as it is now.
async fn save_ending(start: &FixStart, ending: &Ending) {
    let saved = Saved {
        note: ending.note.clone(),
        title: ending.title.clone(),
        tree: tree_state(&start.ws.tree).await,
    };
    let written = serde_json::to_vec(&saved)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(start.ws.root.join(ENDING_FILE), bytes));
    if let Err(e) = written {
        tracing::warn!("fix #{}: recording the review's end: {e}", start.finding.id);
    }
}

/// Keep `ending`, which the forge refused to publish, for the retry of
/// the kept attempt: whatever the review loop decided about saving it (a
/// failed round or unanswered issues ship with a note and are otherwise
/// reviewed again), the retry only has to publish the same verdict, on the
/// same tree, however its own run ends. Anything the fixer changes since
/// is reviewed afresh, as for any saved verdict. One that ships a
/// reviewer-pinned commit in place of `HEAD` is not kept, and is reviewed
/// again: a verdict is saved for `HEAD` only.
pub(super) async fn keep_for_publish(start: &FixStart, ending: &Ending) {
    if ending.head.is_none() {
        save_ending(start, ending).await;
    }
}

/// Whether `ws` holds a saved verdict that covers its tree as it is now
/// ([`covering_verdict`]): a kept fix's retry then ships on it without a
/// review.
pub(super) async fn has_kept_verdict(ws: &Workspace) -> bool {
    covering_verdict(ws).await.is_some()
}

/// Whether the next attempt in `ws` will curate: a curation is owed
/// ([`Owed::Curation`]), or will be once a curation the daemon died in is
/// recovered ([`CURATING_FILE`]) or a curator's tree is put back
/// ([`RESTORE_FILE`], [`LeftBy::Curator`]). Such an attempt runs a model,
/// so its continuation is never free.
pub(super) fn curation_pending(ws: &Workspace) -> bool {
    let owed = read_json::<Progress>(&ws.root.join(PROGRESS_FILE))
        .is_some_and(|progress| matches!(progress.owed, Some(Owed::Curation)));
    let restoring = read_json::<Restore>(&ws.root.join(RESTORE_FILE))
        .is_some_and(|restore| restore.by == LeftBy::Curator);
    owed || restoring || ws.root.join(CURATING_FILE).exists()
}

/// In the workspace root: the review stop a hold was made for ([`Held`]),
/// so the operator's `continue` resumes the fixer on its issues.
const HELD_FILE: &str = "review-held.json";

/// Blocking issues a review left unanswered, held for the operator. They
/// are pending until a fixer has run on them ([`mark_held_sent`]): every
/// attempt that follows an operator's `continue` while they are pending
/// resumes the fixer on them ([`held_resume_prompt`]), whichever hold that
/// `continue` released. A hold made since on an attempt no fixer ran in
/// (`stuck` on a tree that could not be put back, say) stands between the
/// review's hold and the fixer, but the issues are still the work left.
#[derive(Debug, Serialize, Deserialize)]
struct Held {
    issues: Vec<ReportedIssue>,
    /// A fixer has run on `issues`: later continues of the chain resume it
    /// as any other hold's do. Every file this daemon writes has it; one
    /// without it is a legacy file ([`Held::pending`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sent: Option<bool>,
    /// A legacy file's hold report, as its `BLOCKED.md` gave it: read to
    /// decide whether its issues are pending, never written.
    #[serde(default, skip_serializing)]
    report: Option<String>,
}

impl Held {
    /// Whether `issues` are pending for an attempt resumed after the
    /// operator continued a hold whose detail is `detail`. A file with
    /// `sent` says so itself. A legacy one (an earlier daemon's, without
    /// `sent`) never recorded it: it stayed behind after its issues were
    /// answered, and was used only on a `continue` of its own hold, whose
    /// detail is its `report`; so it still is, and only then.
    fn pending(&self, detail: &str) -> bool {
        match self.sent {
            Some(sent) => !sent,
            None => self.report.as_deref() == Some(detail),
        }
    }
}

/// Hold the fix for the operator on review round `round`'s unanswered
/// `issues` (`stop` says why no answer comes): the report goes where a
/// worker's would (`BLOCKED.md`, which `conclude_fix` turns into a
/// `prerequisite` hold with this attempt as its checkpoint), and the
/// issues to [`HELD_FILE`] for the resume. `progress` (round 0, the title
/// and the follow-ups so far) replaces the loop's progress, so a continued
/// fix is reviewed from round 1 under the same title and keeps them.
/// `restoring`: the commit the round's reviewer was shown, when its changes
/// could not be undone, which every later attempt puts the tree back to
/// before the fixer resumes ([`RESTORE_FILE`]); the report says so.
/// `false` when the report could not be written: the fix ships with the
/// issues unresolved instead.
fn hold(
    start: &FixStart,
    round: u32,
    issues: &[ReportedIssue],
    (progress, stop, restoring): (&Progress, &str, Option<&str>),
) -> bool {
    let follow_ups = &progress.follow_ups;
    let root = &start.ws.root;
    let mut report = format!(
        "# Held by the pre-PR review\n\nThe automated pre-PR review stopped at round \
         {round} ({stop}) with issues unresolved. Continue to have the fixer \
         answer them; the review then runs again from round 1.\n\n"
    );
    if let Some(commit) = restoring {
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
    issue_list(&mut report, issues);
    if !follow_ups.is_empty() {
        let _ = write!(report, "\n\n{}", follow_ups_note(follow_ups));
    }
    let held = Held {
        issues: issues.to_vec(),
        sent: Some(false),
        report: None,
    };
    let written = serde_json::to_vec(&held)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(root.join(HELD_FILE), bytes))
        .and_then(|()| std::fs::write(start.ws.tree.join(BLOCKED_REPORT), &report));
    if let Err(e) = written {
        tracing::warn!("fix #{}: holding on the review: {e}", start.finding.id);
        let _ = std::fs::remove_file(root.join(HELD_FILE));
        return false;
    }
    // The continued fix is reviewed from round 1 again, under the same
    // title and with the follow-ups found so far.
    let kept = serde_json::to_vec(progress)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(root.join(PROGRESS_FILE), bytes));
    if let Err(e) = kept {
        tracing::warn!(
            "fix #{}: keeping the review's progress: {e}",
            start.finding.id
        );
    }
    let _ = std::fs::remove_file(root.join(ENDING_FILE));
    true
}

/// Why an attempt could not put its tree back to the one an agent whose
/// changes could not be undone was shown ([`restore_reviewed`]): the
/// failure its streak counts.
pub(super) const TREE_NOT_RESTORED: &str = "reviewed tree not restored";

/// Record `left`, the tree an agent (a failed round's reviewer, or a
/// curator) was shown and could not be undone from, for
/// [`restore_reviewed`] ([`RESTORE_FILE`]). A failure is logged: the tree
/// then stays as the agent left it.
fn record_restore(start: &FixStart, left: &Restore) {
    let written = serde_json::to_vec(left)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(start.ws.root.join(RESTORE_FILE), bytes));
    if let Err(e) = written {
        tracing::warn!(
            "fix #{}: recording the reviewed tree to put back: {e}",
            start.finding.id
        );
    }
}

/// [`undo`] a curator's changes back to `reviewed`; when that fails, the
/// tree is recorded as the curator's for [`restore_reviewed`]
/// ([`record_restore`]), so no later worker runs on, and no later review
/// ships, what the curator left.
async fn undo_or_record(
    lane: &mut Lane,
    start: &FixStart,
    reviewed: ReviewedTree,
) -> Result<(), String> {
    let undone = undo(lane, start, &reviewed.before, &reviewed.description).await;
    if undone.is_err() {
        let left = Restore {
            tree: reviewed,
            by: LeftBy::Curator,
        };
        record_restore(start, &left);
    }
    undone
}

/// Put `start`'s tree back to the one a failed round's reviewer or a
/// curator was shown ([`undo`]) when it was left as that agent changed it
/// ([`RESTORE_FILE`]), whatever the attempt then amounted to: a hold, a
/// retry kept while a host was unreachable, a pinned commit whose publish
/// failed. Run first in every later attempt of the chain, before anything
/// reads the tree, so no worker builds on a reviewer's commit or leftovers
/// and none of them ships. Done once: the record then goes, so a later
/// attempt keeps the fixer's own work. A curator's changes put back leave
/// its curation owed again ([`owe_curation`]). An error says what still
/// differs; no worker may run in the tree then.
///
/// A `BLOCKED.md` in the tree is a review hold's own report, which no hold
/// has recorded yet: the daemon died between [`hold`] writing it and
/// `conclude_fix` recording it (once recorded it leaves the tree, and no
/// worker runs before this). [`undo`] removes reports as a reviewer's, so
/// it is put back whatever undo did: it holds the fix once this attempt
/// ends, as any report no hold has recorded does.
pub(super) async fn restore_reviewed(
    lane: &mut Lane,
    store: &Store,
    start: &FixStart,
) -> Result<(), String> {
    let path = start.ws.root.join(RESTORE_FILE);
    let Some(Restore { tree: reviewed, by }) = read_json::<Restore>(&path) else {
        return Ok(());
    };
    let commit = reviewed.before.head.as_deref().unwrap_or_default();
    let unrecorded = start.ws.tree.join(BLOCKED_REPORT);
    let report = std::fs::read(&unrecorded).ok();
    let undone = undo(lane, start, &reviewed.before, &reviewed.description).await;
    if let Some(report) = report {
        std::fs::write(&unrecorded, report)
            .map_err(|e| format!("keeping the review's unrecorded {BLOCKED_REPORT}: {e}"))?;
    }
    undone.map_err(|e| format!("putting the tree back to the reviewed commit {commit}: {e}"))?;
    std::fs::remove_file(&path)
        .map_err(|e| format!("recording the tree as put back to {commit}: {e}"))?;
    let fid = start.finding.id;
    let agent = match by {
        LeftBy::Reviewer => "a reviewer",
        LeftBy::Curator => {
            owe_curation(start);
            "a curator"
        }
    };
    let message =
        format!("#{fid} the tree {agent} left was put back to the reviewed commit {commit}");
    let _ = fix_event(store, "review", fid, start.job, message).await;
    Ok(())
}

/// The prompt a fix resumed after an operator's `continue` of a hold whose
/// detail is `detail` gets while a review hold's issues are pending
/// ([`Held::pending`]), or `None` when none are: the follow-up playbook
/// with the issues the review left, so the fixer answers them before the
/// review runs again. Once a fixer has run on it, [`mark_held_sent`] says
/// so.
pub(super) fn held_resume_prompt(
    cfg: &Config,
    start: &FixStart,
    detail: &str,
) -> Option<anyhow::Result<String>> {
    let held =
        read_json::<Held>(&start.ws.root.join(HELD_FILE)).filter(|held| held.pending(detail))?;
    let built = serde_json::to_string_pretty(&held.issues)
        .map_err(anyhow::Error::from)
        .and_then(|issues| {
            playbooks::build_review_followup_prompt(
                &cfg.root,
                &start.ws.tree,
                &start.pinned,
                &issues,
                decline_report(start.finding.kind),
            )
        });
    Some(built.map(|prompt| {
        format!(
            "An operator released this fix, which was held because the pre-PR review \
             left issues unanswered. Preserve the committed work; answer the \
             issues below.\n\n{prompt}"
        )
    }))
}

/// Record that a fixer ran on the prompt [`held_resume_prompt`] gave: the
/// review hold's issues are no longer pending. Unrecorded, a later
/// `continue` sends them again.
pub(super) fn mark_held_sent(start: &FixStart) {
    let path = start.ws.root.join(HELD_FILE);
    let Some(mut held) = read_json::<Held>(&path) else {
        return;
    };
    held.sent = Some(true);
    let written = serde_json::to_vec(&held)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&path, bytes));
    if let Err(e) = written {
        tracing::warn!(
            "fix #{}: recording the held issues as sent: {e}",
            start.finding.id
        );
    }
}

/// Where the loop goes once the fixer has been sent a round's issues.
enum Answered {
    /// The fixer finished its answer: review the whole change again.
    ReviewAgain,
    /// The fixer paused, wrote a report or broke its branch: the job ends
    /// on its state, and `conclude_fix` decides.
    Conclude,
    /// No answer came, and a blocking issue among them holds the fix for
    /// the operator ([`hold`]).
    Held,
    /// No answer came while this host is unreachable: retry later.
    Offline(String),
    /// No answer came: ship with the issues unresolved.
    Ship(Ending),
}

/// Send round `progress.rounds`'s `issues` back to the fixer,
/// recording the progress first so a resumed attempt continues after it.
async fn send_back(
    rounds: &Rounds<'_>,
    job: &mut Reviewed,
    progress: &Progress,
    issues: &[ReportedIssue],
) -> Answered {
    let start = rounds.start;
    let (fid, round) = (start.finding.id, progress.rounds);
    let written = serde_json::to_vec(progress)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(start.ws.root.join(PROGRESS_FILE), bytes));
    if let Err(e) = written {
        tracing::warn!("fix #{fid}: recording review round {round}: {e}");
    }
    let fx = match answer_review(rounds, issues, &job.rr).await {
        Ok(fx) => fx,
        Err(why) => {
            let message = format!("#{fid} the fixer could not answer round {round}: {why}");
            let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
            return no_answer(rounds, progress, issues, &why).await;
        }
    };
    match job_state(&fx) {
        FinishedState::Done => {
            take_run(&mut job.rr, &fx);
            if ready_to_ship(start, &job.rr).await {
                return Answered::ReviewAgain;
            }
            // A report (or a broken branch) ends the loop: `conclude_fix`
            // decides, and restarts the rounds on a report.
            Answered::Conclude
        }
        // Half an answer: `ending` stays `None`, so nothing ships.
        FinishedState::Suspended => {
            take_run(&mut job.rr, &fx);
            Answered::Conclude
        }
        FinishedState::Failed | FinishedState::Killed => {
            add_spend(&mut job.rr, &fx);
            let why = format!("its answer {}", run_failure(&fx));
            let message = format!("#{fid} the fixer's answer to round {round} failed: {why}");
            let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
            no_answer(rounds, progress, issues, &why).await
        }
    }
}

/// Where the loop goes when the fixer's answer to round `progress.rounds`'s
/// `issues` could not start or failed without doing work (`why`).
///
/// While a host the work needs is unreachable it is the network's failure
/// ([`super::unreachable_now`]), handled as the fix's own offline failures
/// are: nothing is counted or published, the attempt is kept as the
/// checkpoint, and the retry reviews that round again. Otherwise a
/// blocking issue never ships: it holds the fix for the operator, as one
/// left by the last round does. Only non-blocking ones ship unresolved.
async fn no_answer(
    rounds: &Rounds<'_>,
    progress: &Progress,
    issues: &[ReportedIssue],
    why: &str,
) -> Answered {
    let (start, round) = (rounds.start, progress.rounds);
    if let Some(host) = rounds.offline(round, &progress.follow_ups).await {
        return Answered::Offline(host);
    }
    if issues.iter().any(|i| i.blocking) {
        let kept = Progress {
            rounds: 0,
            title: progress.title.clone(),
            follow_ups: progress.follow_ups.clone(),
            ..Progress::default()
        };
        let stop = format!("the fixer could not answer them: {why}");
        if hold(start, round, issues, (&kept, &stop, None)) {
            let message = format!("{}, {stop}; held for the operator", tally(issues));
            log_round(rounds.store(), start, round, &message).await;
            return Answered::Held;
        }
    }
    Answered::Ship(unresolved(round, issues, Some(why)))
}

/// A fix that concludes on a report (`BLOCKED.md`, a decline) starts any
/// later review from round 1, keeping the title and follow-ups and nothing
/// else (no issues owed from before the report, no stop): the
/// operator's `continue` of a hold is a fresh start, however the report
/// came to be (an answer, a suspended one, the attempt that finished it,
/// or the review's own hold). Called from the one place every report is
/// read, `conclude_fix`. Nothing to do without a review in progress.
pub(super) fn restart_rounds(start: &FixStart) {
    if let Some(progress) = read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE)) {
        write_progress(
            start,
            &Progress {
                title: progress.title,
                follow_ups: progress.follow_ups,
                ..Progress::default()
            },
        );
    }
}

/// Write `progress` to [`PROGRESS_FILE`]; a failure is logged.
fn write_progress(start: &FixStart, progress: &Progress) {
    let written = serde_json::to_vec(progress)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(start.ws.root.join(PROGRESS_FILE), bytes));
    if let Err(e) = written {
        tracing::warn!(
            "fix #{}: recording the review's progress: {e}",
            start.finding.id
        );
    }
}

/// Whether `job`, the attempt `start` continues, was suspended by the
/// review between its runs, its fixer's last one finished
/// ([`Rounds::suspend`]): the resume then runs no fixer, which would only
/// re-send the transcript to be told to carry on, and goes straight back
/// to the review loop. Only that job's own continuation does: an attempt
/// that continues a later one runs its fixer.
pub(super) fn suspended_between_runs(start: &FixStart, job: i64) -> bool {
    read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE))
        .is_some_and(|progress| progress.paused_by == Some(job))
}

/// Why a fixer that finished is not shipped when no review ran
/// ([`ready_to_ship`] was false and no report concludes it): what
/// `ship_pr` would say for nothing to ship, or that its commits could not
/// be listed. Nothing unreviewed ships while the review is on.
pub(super) async fn unshippable(start: &FixStart) -> String {
    let range = format!("{}..HEAD", start.pinned);
    let tree = start.ws.tree.clone();
    let (rc, commits) =
        tokio::task::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
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
        tokio::task::spawn_blocking(move || Unheld::Commits { range: &range }.run(&tree, 30))
            .await
            .unwrap_or((127, String::new()));
    rc == 0 && !commits.trim().is_empty()
}

/// The subject of `tree`'s `HEAD`, if it has one.
async fn head_subject(tree: &Path) -> Option<String> {
    let tree = tree.to_owned();
    let (rc, out) =
        tokio::task::spawn_blocking(move || Unheld::Subject { rev: "HEAD" }.run(&tree, 30))
            .await
            .ok()?;
    (rc == 0).then(|| out.trim().to_owned())
}

/// Run review round `round` in `start`'s tree under `cap` tokens and
/// `wall` seconds: a fresh session in a directory of its own
/// ([`Workspace::review_session`], one per job and round, so it is never a
/// transcript to continue and is metered apart from the fixer's), writing
/// its issues outside the tree. The run, if one happened, and its verdict.
///
/// The tree is cleaned first ([`clean_tree`]): the reviewer is shown the
/// commits alone, and its verdict is about them. One that changed the tree
/// has its changes undone ([`undo`]). One that left the commits alone
/// ([`uncommitted_only`]: a test run's artifacts, a lock file or generated
/// source its build rewrote) changed nothing it reviewed: its changes are
/// reverted, logged in the round's event, and its verdict stands. One that
/// changed more keeps its written issues, as a killed reviewer does; a
/// verdict of none does not, since it was reached on a tree it changed.
async fn review_round(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    round: u32,
    cap: Option<i64>,
    wall: i64,
) -> (Option<RunResult>, RoundVerdict, Vec<ReportedIssue>) {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let _phase = crate::activity::job_phase(JobPhase::Reviewing);
    let fail = |why: String| RoundVerdict::Failed {
        why,
        left: None,
        issues: Vec::new(),
    };
    let session = start.ws.review_session(start.job, round);
    let out = session.with_extension("json");
    // The fixer's description, put back if the reviewer touches it: it is
    // the PR body, and untracked, so no commit protects it.
    let description = std::fs::read(start.ws.tree.join(DESCRIPTION)).unwrap_or_default();
    let prompt = match playbooks::build_review_prompt(
        &cfg.root,
        &start.finding,
        &start.ws.tree,
        &start.pinned,
        &String::from_utf8_lossy(&description),
        &out,
        &start.repo,
        &Store::repo_notes(&cfg.work_root, start.repo.id),
    ) {
        Ok(prompt) => prompt,
        Err(e) => return (None, fail(format!("no reviewer prompt: {e}")), Vec::new()),
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
            log_round(rounds.store(), start, round, &message).await;
        }
        Err(why) => {
            let message = format!("{why}; the reviewer is shown it as it is");
            log_round(rounds.store(), start, round, &message).await;
        }
    }
    let before = tree_state(&start.ws.tree).await;
    // A detached `HEAD` (no branch) is a state like any other; only a
    // commit or status git could not report leaves nothing to compare.
    if !before.readable() {
        // Nothing to hold the reviewer's tree against, or to put back to:
        // no review this round.
        return (None, fail(UNREADABLE.to_owned()), Vec::new());
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
    let rr = match run {
        Ok(rr) => rr,
        Err(e) => {
            let why = format!("the reviewer could not run: {e}");
            return (None, fail(why), Vec::new());
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
    if before != after {
        let shown = ReviewedTree {
            before,
            description,
        };
        if let Some(verdict) = undo_reviewer(rounds, lane, round, shown, &after, &mut written).await
        {
            return (Some(rr), verdict, follow_ups);
        }
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
    (Some(rr), verdict, follow_ups)
}

/// Undo the changes of round `round`'s reviewer, which left `after` of the
/// tree it was `shown` ([`undo`]), and give the round's verdict on the
/// issues it wrote (`written`, taken when the verdict is given here).
/// `None` when it left the commits alone ([`uncommitted_only`]): its
/// changes are reverted, the round's event names them, and its verdict
/// stands. When it changed more, its written issues stand, undone, and a
/// verdict of none fails the round. One that could not be undone fails the
/// round, with the tree it was shown to put back and its written issues.
async fn undo_reviewer(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    round: u32,
    shown: ReviewedTree,
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
            log_round(rounds.store(), start, round, &message).await;
            None
        }
        (Ok(()), None) => {
            let issues = issues();
            Some(if issues.is_empty() {
                RoundVerdict::Failed {
                    why: CHANGED.to_owned(),
                    left: None,
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
        // what ships is the commit it was shown, and a hold puts the tree
        // back to it before the fixer resumes.
        (Err(why), _) => Some(RoundVerdict::Failed {
            why: format!("{CHANGED}, and it could not be undone: {why}"),
            left: Some(shown),
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
    let (rc, out) = tokio::task::spawn_blocking(move || Unheld::Status.run(&tree, 60))
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
    for report in [BLOCKED_REPORT, "NOT-A-BUG.md", "DECLINED.md"] {
        let _ = std::fs::remove_file(tree.join(report));
    }
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

/// Resume the fixer's session on the review's `issues`, under
/// what is left of the job's grant after `job`.
async fn answer_review(
    rounds: &Rounds<'_>,
    issues: &[ReportedIssue],
    job: &RunResult,
) -> Result<RunResult, String> {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let session = job
        .session_file
        .as_deref()
        .ok_or("the fixer left no transcript to resume")?;
    let (cap, wall) = rounds.allowance(job)?;
    let issues = serde_json::to_string_pretty(issues).map_err(|e| e.to_string())?;
    let prompt = playbooks::build_review_followup_prompt(
        &cfg.root,
        &start.ws.tree,
        &start.pinned,
        &issues,
        decline_report(start.finding.kind),
    )
    .map_err(|e| format!("no follow-up prompt: {e}"))?;
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

/// What a reviewer must leave as it found it, and what a saved verdict
/// covers ([`Saved`]): the commit, the uncommitted state, and the
/// description that becomes the PR body (by its bytes: its status line,
/// untracked or tracked and changed, reads the same whatever it says).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    tokio::task::spawn_blocking(move || {
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

/// [`tally`], and whether the reviewer's changes to the tree were undone.
fn raised(issues: &[ReportedIssue], undone: bool) -> String {
    if undone {
        format!(
            "{} (the reviewer changed the worktree; undone)",
            tally(issues)
        )
    } else {
        tally(issues)
    }
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
fn tally(issues: &[ReportedIssue]) -> String {
    let blocking = issues.iter().filter(|i| i.blocking).count();
    let s = if issues.len() == 1 { "" } else { "s" };
    format!("{} issue{s} ({blocking} blocking)", issues.len())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
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

async fn log_round(store: &Store, start: &FixStart, round: u32, outcome: &str) {
    let fid = start.finding.id;
    let message = format!("#{fid} review round {round}: {outcome}");
    let _ = fix_event(store, "review", fid, start.job, message).await;
}

/// A review that failed: the fix ships, and its PR says why it was not
/// reviewed, followed by any `issues` the reviewer had written.
fn failed(round: u32, why: &str, head: Option<String>, issues: &[ReportedIssue]) -> Ending {
    let mut note = format!(
        "## Pre-PR review\n\nReview round {round} failed: {why}. This change ships \
         without a completed automated review."
    );
    if !issues.is_empty() {
        let _ = write!(note, "\n\n{}", unresolved_note(round, issues));
    }
    Ending {
        note: Some(note),
        head,
        title: None,
    }
}

/// Ship round `round`'s `issues` unresolved, with why the fixer
/// did not answer them when it was meant to.
fn unresolved(round: u32, issues: &[ReportedIssue], unanswered: Option<&str>) -> Ending {
    let mut note = unresolved_note(round, issues);
    if let Some(why) = unanswered {
        let _ = write!(note, "\n\nThe fixer could not answer them: {why}.");
    }
    Ending {
        note: Some(note),
        head: None,
        title: None,
    }
}

/// The PR body section that hands a round's issues to the human, blocking
/// ones first and marked so.
fn unresolved_note(round: u32, issues: &[ReportedIssue]) -> String {
    let mut note = format!(
        "## Unresolved review issues\n\nAutomated pre-PR review round {round} raised these \
         issues, and the fix ships without resolving them:\n"
    );
    issue_list(&mut note, issues);
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

/// `issues` as a Markdown list on `note`: blocking ones marked, each with
/// its place and detail.
fn issue_list(note: &mut String, issues: &[ReportedIssue]) {
    for issue in issues {
        let place = match (issue.file.as_deref(), issue.line) {
            (None | Some(""), _) => String::new(),
            (Some(file), None | Some(0)) => format!(" (`{file}`)"),
            (Some(file), Some(line)) => format!(" (`{file}:{line}`)"),
        };
        let mark = if issue.blocking { "[blocking] " } else { "" };
        let _ = write!(note, "\n- {mark}**{}**{place}", issue.summary);
        for line in issue.detail.lines() {
            note.push_str("\n  ");
            note.push_str(line);
        }
    }
}
