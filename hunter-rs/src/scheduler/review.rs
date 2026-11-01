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
use crate::store::Store;
use crate::types::{JobPhase, RunResult};
use crate::workspace::Workspace;

/// The `killed_reason` of a fix job the daemon's shutdown stopped between
/// two of its runs: a pause on the fixer's transcript (`job_state`).
pub(super) const STOPPED: &str = "shutdown";

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
/// it started from ([`Curating`]). Left behind only when the daemon died
/// during the curation; the next attempt puts the tree back to it before
/// anything reads the tree ([`recover_curation`]).
const CURATING_FILE: &str = "review-curating.json";

/// The reviewed tree a curation started from, and the fixer's description.
#[derive(Debug, Serialize, Deserialize)]
struct Curating {
    before: TreeState,
    description: Vec<u8>,
}

/// What the review loop leaves for the job's record and its PR.
pub(super) struct Reviewed {
    /// Every run of the job, summed: tokens, calls, usage and time. The
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
    /// The last round whose issues went back to the fixer.
    rounds: u32,
    /// [`Ending::title`], taken before round 1.
    title: Option<String>,
    /// The follow-ups the rounds so far raised ([`merge_follow_ups`]).
    #[serde(default)]
    follow_ups: Vec<serde_json::Value>,
}

/// One review round's verdict.
enum RoundVerdict {
    /// No issue to answer: none, or only follow-ups.
    Clean,
    /// The reviewer's issues, verbatim, blocking ones first. `undone`: the
    /// reviewer also changed the worktree, and that was undone ([`undo`]),
    /// so the tree is the one it reviewed and its issues stand.
    Issues {
        issues: Vec<serde_json::Value>,
        undone: bool,
    },
    /// No usable review this round. `issues`: entries the reviewer wrote before it changed a tree that could not be put back;
    /// they ship unresolved.
    Failed {
        why: String,
        /// `HEAD` before the reviewer ran, when the tree could not be put
        /// back to it: the commit that ships.
        head: Option<String>,
        issues: Vec<serde_json::Value>,
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

    /// Stop the job for the daemon's shutdown, before another run starts:
    /// the job is suspended on the fixer's transcript, and its resume
    /// reviews again. Only with a transcript to resume; without one, the
    /// loop goes on.
    async fn stopped(&self, job: &mut Reviewed, round: u32) -> bool {
        if !self.scope.stopping() || job.rr.session_file.is_none() {
            return false;
        }
        job.rr.killed_reason = Some(STOPPED.to_owned());
        job.rr.exit_code = None;
        log_round(
            self.store(),
            self.start,
            round,
            "the daemon is stopping; suspended",
        )
        .await;
        true
    }
}

/// What becomes of a round's issues when no answer is coming
/// ([`Rounds::unanswered`]).
enum Unanswered {
    /// An answer is coming: send them back to the fixer.
    No,
    /// A blocking one holds the fix for the operator ([`hold`]).
    Held,
    /// They ship unresolved; `keep` saves the verdict for the tree.
    Ship(Ending, bool),
    /// A run did no work while this host is unreachable: retry later.
    Offline(String),
}

impl Rounds<'_> {
    /// Whether a run of round `round` that did no work did so because a
    /// host the work needs is unreachable ([`super::unreachable_now`]):
    /// the network's failure, handled as the fix's other offline failures
    /// are (`FixOutcome::Offline`). The progress is rewound so the retry
    /// reviews that round again, keeping the title and `follow_ups`.
    async fn offline(&self, round: u32, follow_ups: &[serde_json::Value]) -> Option<String> {
        let start = self.start;
        let host = super::unreachable_now(self.cfg, &start.repo.url, self.backend).await?;
        let again = Progress {
            rounds: round - 1,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
        };
        write_progress(start, &again);
        let message = format!("{host} unreachable; kept to review round {round} again");
        log_round(self.store(), start, round, &message).await;
        Some(host)
    }

    /// What a failed round amounts to (`why`). With a host the work needs
    /// unreachable, whatever the reviewer did, the network's failure
    /// ([`Self::offline`]): an outage that cut a review off mid-way is no
    /// more the fix's than one that kept it from starting.
    /// Otherwise the fix ships, saying why it was not reviewed: the
    /// reviewer never holds it hostage. But a blocking issue the reviewer
    /// wrote before the round failed never ships: it holds the fix.
    async fn failed_round(
        &self,
        round: u32,
        (why, head, issues): (&str, Option<String>, &[serde_json::Value]),
        follow_ups: &[serde_json::Value],
        also: &str,
    ) -> Unanswered {
        let start = self.start;
        if let Some(host) = self.offline(round, follow_ups).await {
            return Unanswered::Offline(host);
        }
        let progress = Progress {
            rounds: 0,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
        };
        if issues.iter().any(is_blocking) && hold(start, round, issues, &progress, why) {
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

    /// Whether round `round`'s `issues` get no answer, and what then: none
    /// comes in the last round, or when the grant is spent (a reviewer
    /// killed at its cap) and leaves nothing to answer them with. A
    /// blocking issue then holds the fix for the operator; the rest ship
    /// unresolved. `raised` is the round's tally for the event.
    async fn unanswered(
        &self,
        spent: &RunResult,
        round: u32,
        (issues, undone): (&[serde_json::Value], bool),
        follow_ups: &[serde_json::Value],
        raised: &str,
    ) -> Unanswered {
        let (start, max) = (self.start, self.cfg.review_max_rounds);
        let why = if round >= max {
            None
        } else {
            match self.allowance(spent) {
                Ok(_) => return Unanswered::No,
                Err(why) => Some(why),
            }
        };
        let stop = why.map_or_else(
            || format!("the last of {max} rounds"),
            |why| format!("{why} to answer them"),
        );
        let progress = Progress {
            rounds: 0,
            title: self.title.clone(),
            follow_ups: follow_ups.to_vec(),
        };
        if issues.iter().any(is_blocking) && hold(start, round, issues, &progress, &stop) {
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
/// daemon's shutdown, checked before every run ([`Rounds::stopped`]).
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
    if job.ending.is_some() || !ready_to_ship(start, &job.rr).await {
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
    let mut round = progress.rounds + 1;
    let mut follow_ups = progress.follow_ups;
    let (mut ending, keep) = loop {
        if rounds.stopped(&mut job, round).await {
            return job;
        }
        let (cap, wall) = match rounds.allowance(&job.rr) {
            Ok(allowance) => allowance,
            Err(why) => break (failed(round, why, None, &[]), false),
        };
        let (run, verdict, raised_follow_ups) = review_round(&rounds, lane, round, cap, wall).await;
        if let Some(run) = &run {
            add_spend(&mut job.rr, run);
        }
        let also = follow_up_tally(raised_follow_ups.len());
        merge_follow_ups(&mut follow_ups, raised_follow_ups);
        let store = rounds.store();
        let (issues, undone) = match verdict {
            RoundVerdict::Failed { why, head, issues } => {
                let failure = (why.as_str(), head, issues.as_slice());
                match rounds
                    .failed_round(round, failure, &follow_ups, &also)
                    .await
                {
                    Unanswered::Ship(ending, keep) => break (ending, keep),
                    Unanswered::Offline(host) => job.offline = Some(host),
                    Unanswered::Held | Unanswered::No => {}
                }
                return job;
            }
            RoundVerdict::Clean => {
                let message = format!("no issues to answer{also}; shipping");
                log_round(store, start, round, &message).await;
                break (Ending::default(), true);
            }
            RoundVerdict::Issues { issues, undone } => (issues, undone),
        };
        let raised = format!("{}{also}", raised(&issues, undone));
        match rounds
            .unanswered(&job.rr, round, (&issues, undone), &follow_ups, &raised)
            .await
        {
            Unanswered::No => {}
            Unanswered::Held | Unanswered::Offline(_) => return job,
            Unanswered::Ship(ending, keep) => break (ending, keep),
        }
        if rounds.stopped(&mut job, round).await {
            return job;
        }
        let message = format!("{raised}; back to the fixer");
        log_round(store, start, round, &message).await;
        let progress = Progress {
            rounds: round,
            title: title.clone(),
            follow_ups: follow_ups.clone(),
        };
        match send_back(&rounds, &mut job, &progress, &issues).await {
            Answered::ReviewAgain => round += 1,
            Answered::Conclude | Answered::Held => return job,
            Answered::Offline(host) => {
                job.offline = Some(host);
                return job;
            }
            Answered::Ship(ending) => break (ending, false),
        }
    };
    ending.title = title;
    finish(&rounds, lane, &mut job.rr, &mut ending, follow_ups, keep).await;
    job.ending = Some(ending);
    job
}

/// End a loop that ships: the follow-ups go into the PR body, the loop's
/// files are ended (the verdict saved for the reviewed tree when `keep`),
/// and the history is curated ([`curate`]); an accepted curation saves the
/// verdict again, for the curated tree. Saving before the curation is what
/// lets an attempt that recovers from a curation the daemon died in ship
/// the reviewed tree without another review.
async fn finish(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut RunResult,
    ending: &mut Ending,
    follow_ups: Vec<serde_json::Value>,
    keep: bool,
) {
    let start = rounds.start;
    with_follow_ups(ending, &follow_ups);
    end_loop(start, ending, follow_ups.clone(), keep).await;
    if curate(rounds, lane, job, ending).await {
        end_loop(start, ending, follow_ups, keep).await;
    }
}

/// Have the fixer curate its reviewed history before it ships
/// (`curate.md`): its own session, resumed once more under what is left of
/// the grant, rewrites `<pinned>..HEAD` into a few logical commits of the
/// same tree. The rewrite is accepted only when the run finished, `HEAD`
/// moved, the new tree is byte-identical to the reviewed one
/// (`Unheld::SameTree`), the branch, uncommitted state and description are
/// as they were, and the new range is not empty and still descends from
/// the pinned commit. Otherwise the tree is put back to the reviewed
/// commit ([`undo`]) and the original commits ship. Curation is cosmetic:
/// it never holds, fails or counts against the fix, and is skipped when
/// it is off, there is one commit, the daemon is stopping (the fix ships
/// as reviewed rather than paying another run), nothing is left of the
/// grant, or `ending` already pins a commit (a reviewer moved `HEAD`).
/// When the tree cannot be put back, `ending` pins the reviewed commit,
/// so that is what ships. `true` when the curated history was accepted.
async fn curate(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    job: &mut RunResult,
    ending: &mut Ending,
) -> bool {
    if ending.head.is_some() {
        return false;
    }
    match curated_head(rounds, lane, job).await {
        Curated::Accepted => true,
        Curated::Not(pin) => {
            ending.head = pin;
            false
        }
    }
}

/// How a curation went.
enum Curated {
    /// The curated history ships.
    Accepted,
    /// Skipped or rejected; the commit to pin when a rejected curation's
    /// tree could not be put back.
    Not(Option<String>),
}

/// [`curate`]'s work. While it runs, [`CURATING_FILE`] records the
/// reviewed tree, so a daemon that dies in it leaves the next attempt
/// what to put back ([`recover_curation`]).
async fn curated_head(rounds: &Rounds<'_>, lane: &mut Lane, job: &mut RunResult) -> Curated {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let fid = start.finding.id;
    let note = |message: String| async move {
        let message = format!("#{fid} curation: {message}");
        let _ = fix_event(rounds.store(), "review", fid, start.job, message).await;
    };
    let Some(commits) = commit_count(start)
        .await
        .filter(|n| cfg.review_curate && *n > 1)
    else {
        return Curated::Not(None);
    };
    let skipped = if rounds.scope.stopping() {
        Some("the daemon is stopping".to_owned())
    } else {
        rounds.allowance(job).err().map(str::to_owned)
    };
    let (Some(session), None) = (job.session_file.clone(), &skipped) else {
        let why = skipped.unwrap_or_else(|| "no transcript to resume".to_owned());
        note(format!(
            "skipped ({why}); shipping the {commits} commits as they are"
        ))
        .await;
        return Curated::Not(None);
    };
    let tree = &start.ws.tree;
    let curating = Curating {
        before: tree_state(tree).await,
        description: std::fs::read(tree.join(DESCRIPTION)).unwrap_or_default(),
    };
    let (Some(reviewed), Ok((cap, wall))) = (curating.before.head.clone(), rounds.allowance(job))
    else {
        return Curated::Not(None);
    };
    let Ok(prompt) =
        playbooks::build_curate_prompt(&cfg.root, tree, &start.pinned, &start.branch, &reviewed)
    else {
        return Curated::Not(None);
    };
    let marker = start.ws.root.join(CURATING_FILE);
    let recorded = serde_json::to_vec(&curating)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&marker, bytes));
    if let Err(e) = recorded {
        note(format!("skipped (cannot record the reviewed tree: {e})")).await;
        return Curated::Not(None);
    }
    let Curating {
        before,
        description,
    } = curating;
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
    let verdict = match &run {
        Err(e) => Err(format!("it could not run: {e}")),
        Ok(rr) => {
            add_spend(job, rr);
            if job_state(rr) == FinishedState::Done {
                curated(start, &before, &after, &reviewed).await
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
            let undone = undo(lane, start, &before, &after, &description).await;
            let pin = undone.as_ref().err().map(|_| reviewed);
            let left = undone
                .err()
                .map_or_else(String::new, |e| format!(" (not undone: {e})"));
            note(format!(
                "rejected: {why}{left}; shipping the original {commits} commits"
            ))
            .await;
            Curated::Not(pin)
        }
    };
    let _ = std::fs::remove_file(&marker);
    outcome
}

/// Put the tree back to the reviewed commit when the daemon died during a
/// curation ([`CURATING_FILE`] left behind): abort a rebase, cherry-pick,
/// merge or `am` the curator left in progress, then [`undo`] to the
/// reviewed tree and description, under the clone's permit. Run first in a
/// later attempt of the chain, before anything reads the tree; the verdict
/// saved for the reviewed tree then ships it without another review.
/// `true` when there was a curation to recover from.
pub(super) async fn recover_curation(lane: &mut Lane, store: &Store, start: &FixStart) -> bool {
    let marker = start.ws.root.join(CURATING_FILE);
    let Some(Curating {
        before,
        description,
    }) = read_json::<Curating>(&marker)
    else {
        return false;
    };
    let tree = start.ws.tree.clone();
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
    let now = tree_state(&tree).await;
    let outcome = match undo(lane, start, &before, &now, &description).await {
        Ok(()) => "the reviewed commit restored".to_owned(),
        Err(e) => format!("restoring the reviewed commit failed: {e}"),
    };
    let _ = std::fs::remove_file(&marker);
    let fid = start.finding.id;
    let message = format!("#{fid} curation: interrupted by a restart; {outcome}");
    let _ = fix_event(store, "review", fid, start.job, message).await;
    true
}

/// What a fixer resumed after [`recover_curation`] is told: its interrupted
/// curation was undone, and there is nothing left to do.
pub(super) const CURATION_UNDONE_PROMPT: &str = "Your rewrite of this branch's history was \
     interrupted and has been undone: the branch is back at the reviewed commit, which ships as \
     it is. Do not change the history, the files or PR-DESCRIPTION.md. Stop now.";

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
fn with_follow_ups(ending: &mut Ending, follow_ups: &[serde_json::Value]) {
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
    let saved = read_json::<Saved>(&start.ws.root.join(ENDING_FILE))?;
    let covers = saved.tree.head.is_some() && saved.tree == tree_state(&start.ws.tree).await;
    covers.then_some(Ending {
        note: saved.note,
        head: None,
        title: saved.title,
    })
}

/// End the loop's files. Its progress is reset to round 0, keeping the
/// title and the `follow_ups`: a later attempt that finds the tree changed
/// (an offline push retried after the fixer went on) is reviewed from
/// round 1 again, under the same title and with them. `ending` is saved
/// for the tree as it is when `keep` (a reviewer's own verdict).
async fn end_loop(
    start: &FixStart,
    ending: &Ending,
    follow_ups: Vec<serde_json::Value>,
    keep: bool,
) {
    let root = &start.ws.root;
    let progress = Progress {
        rounds: 0,
        title: ending.title.clone(),
        follow_ups,
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
    if !keep {
        return;
    }
    let saved = Saved {
        note: ending.note.clone(),
        title: ending.title.clone(),
        tree: tree_state(&start.ws.tree).await,
    };
    let written = serde_json::to_vec(&saved)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(root.join(ENDING_FILE), bytes));
    if let Err(e) = written {
        tracing::warn!("fix #{}: recording the review's end: {e}", start.finding.id);
    }
}

/// In the workspace root: the review stop a hold was made for ([`Held`]),
/// so the operator's `continue` resumes the fixer on its issues.
const HELD_FILE: &str = "review-held.json";

/// Blocking issues a review left unanswered, held for the operator.
#[derive(Debug, Serialize, Deserialize)]
struct Held {
    /// The hold's report, as `BLOCKED.md` gave it: what the resume plan's
    /// `after_hold` detail is when the hold is this one.
    report: String,
    issues: Vec<serde_json::Value>,
}

/// Hold the fix for the operator on review round `round`'s unanswered
/// `issues` (`stop` says why no answer comes): the report goes where a
/// worker's would (`BLOCKED.md`, which `conclude_fix` turns into a
/// `prerequisite` hold with this attempt as its checkpoint), and the
/// issues to [`HELD_FILE`] for the resume. `progress` (round 0, the title
/// and the follow-ups so far) replaces the loop's progress, so a continued
/// fix is reviewed from round 1 under the same title and keeps them. `false` when the report could not be
/// written: the fix ships with the issues unresolved instead.
fn hold(
    start: &FixStart,
    round: u32,
    issues: &[serde_json::Value],
    progress: &Progress,
    stop: &str,
) -> bool {
    let follow_ups = &progress.follow_ups;
    let root = &start.ws.root;
    let mut report = format!(
        "# Held by the pre-PR review\n\nThe automated pre-PR review stopped at round \
         {round} ({stop}) with blocking issues unresolved. Continue to have the fixer \
         answer them; the review then runs again from round 1.\n\n## Remaining review \
         issues\n"
    );
    issue_list(&mut report, issues);
    if !follow_ups.is_empty() {
        let _ = write!(report, "\n\n{}", follow_ups_note(follow_ups));
    }
    let held = Held {
        report: report.clone(),
        issues: issues.to_vec(),
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

/// The prompt a fix resumed after the operator continued a review hold
/// ([`hold`]) gets, or `None` when `detail` is not that hold's report:
/// the follow-up playbook with the issues the review left, so the fixer
/// answers them before the review runs again.
pub(super) fn held_resume_prompt(
    cfg: &Config,
    start: &FixStart,
    detail: &str,
) -> Option<anyhow::Result<String>> {
    let held = read_json::<Held>(&start.ws.root.join(HELD_FILE))?;
    if held.report != detail {
        return None;
    }
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
             left blocking issues unanswered. Preserve the committed work; answer the \
             issues below.\n\n{prompt}"
        )
    }))
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
    issues: &[serde_json::Value],
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
    issues: &[serde_json::Value],
    why: &str,
) -> Answered {
    let (start, round) = (rounds.start, progress.rounds);
    if let Some(host) = rounds.offline(round, &progress.follow_ups).await {
        return Answered::Offline(host);
    }
    if issues.iter().any(is_blocking) {
        let kept = Progress {
            rounds: 0,
            title: progress.title.clone(),
            follow_ups: progress.follow_ups.clone(),
        };
        let stop = format!("the fixer could not answer them: {why}");
        if hold(start, round, issues, &kept, &stop) {
            let message = format!("{}, {stop}; held for the operator", tally(issues));
            log_round(rounds.store(), start, round, &message).await;
            return Answered::Held;
        }
    }
    Answered::Ship(unresolved(round, issues, Some(why)))
}

/// A fix that concludes on a report (`BLOCKED.md`, a decline) starts any
/// later review from round 1, keeping the title and follow-ups: the
/// operator's `continue` of a hold is a fresh start, however the report
/// came to be (an answer, a suspended one, the attempt that finished it,
/// or the review's own hold). Called from the one place every report is
/// read, `conclude_fix`. Nothing to do without a review in progress.
pub(super) fn restart_rounds(start: &FixStart) {
    if let Some(progress) = read_json::<Progress>(&start.ws.root.join(PROGRESS_FILE))
        && progress.rounds != 0
    {
        write_progress(
            start,
            &Progress {
                rounds: 0,
                ..progress
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
        0 if !start.ws.tree.join("PR-DESCRIPTION.md").exists() => "no PR-DESCRIPTION.md".to_owned(),
        _ => "the review could not list the fix's commits (git failed)".to_owned(),
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
    let subject = out.trim();
    (rc == 0 && !subject.is_empty()).then(|| subject.to_owned())
}

/// Run review round `round` in `start`'s tree under `cap` tokens and
/// `wall` seconds: a fresh session in a directory of its own
/// ([`Workspace::review_session`], one per job and round, so it is never a
/// transcript to continue and is metered apart from the fixer's), writing
/// its issues outside the tree. The run, if one happened, and its verdict.
///
/// A reviewer that changed the tree has its changes undone ([`undo`]).
/// Its written issues still stand, as a killed reviewer's do; a verdict
/// of none does not, since it was reached on a tree it changed.
async fn review_round(
    rounds: &Rounds<'_>,
    lane: &mut Lane,
    round: u32,
    cap: Option<i64>,
    wall: i64,
) -> (Option<RunResult>, RoundVerdict, Vec<serde_json::Value>) {
    let (cfg, start) = (rounds.cfg, rounds.start);
    let _phase = crate::activity::job_phase(JobPhase::Reviewing);
    let fail = |why: String| RoundVerdict::Failed {
        why,
        head: None,
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
    let before = tree_state(&start.ws.tree).await;
    // A detached `HEAD` (no branch) is a state like any other; only a
    // commit or status git could not report leaves nothing to compare.
    if before.head.is_none() || before.status.is_none() {
        // Nothing to hold the reviewer's tree against, or to put back to:
        // no review this round, and nothing in the tree is touched.
        let why = "the worktree's state could not be read (git failed)".to_owned();
        return (None, fail(why), Vec::new());
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
    let (written, follow_ups) = match parse_review(&out) {
        Ok((entries, dropped)) => {
            let (follow_ups, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(is_follow_up);
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
        const CHANGED: &str = "the reviewer changed the worktree";
        let issues = written.unwrap_or_default();
        let verdict = match undo(lane, start, &before, &after, &description).await {
            Ok(()) if !issues.is_empty() => RoundVerdict::Issues {
                issues,
                undone: true,
            },
            Ok(()) => fail(CHANGED.to_owned()),
            // Not the tree it reviewed, and not one an answer may build
            // on: what ships is the commit it was shown.
            Err(why) => RoundVerdict::Failed {
                why: format!("{CHANGED}, and it could not be undone: {why}"),
                head: before.head,
                issues,
            },
        };
        return (Some(rr), verdict, follow_ups);
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

/// The fixer's description of its change: the PR body.
const DESCRIPTION: &str = "PR-DESCRIPTION.md";

/// Put `start`'s tree back as it was `before` a reviewer changed it to
/// `after`: remove the reports and the untracked paths it added; when its
/// commit, branch or tracked files still differ, check the fix branch out
/// again at the reviewed commit, discarding the tracked changes
/// (`checkout --force -B`, under the clone's permit); then write the
/// fixer's `description` back if it differs. Last, because a reviewer
/// that committed or staged the untracked description has it removed by
/// that checkout. Untracked files that were there before stay, as do
/// ignored ones. An error says what still differs.
async fn undo(
    lane: &mut Lane,
    start: &FixStart,
    before: &TreeState,
    after: &TreeState,
    description: &[u8],
) -> Result<(), String> {
    let tree = &start.ws.tree;
    for report in [BLOCKED_REPORT, "NOT-A-BUG.md", "DECLINED.md"] {
        let _ = std::fs::remove_file(tree.join(report));
    }
    for path in added_untracked(before, after) {
        let path = tree.join(path);
        let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
    }
    let restore = || -> Result<(), String> {
        std::fs::write(tree.join(DESCRIPTION), description)
            .map_err(|e| format!("restoring {DESCRIPTION}: {e}"))
    };
    if tree_state(tree).await.description != before.description {
        restore()?;
    }
    if tree_state(tree).await == *before {
        return Ok(());
    }
    let head = before.head.clone().ok_or("no commit to go back to")?;
    let permit = lane
        .lock_clone(start.repo.id)
        .await
        .map_err(|e| format!("locking the clone: {e}"))?;
    // Back onto the fix branch, or detached as the fixer left it.
    let mut args = vec!["checkout".to_owned(), "--force".to_owned()];
    if before.branch.is_some() {
        args.extend(["-B".to_owned(), start.branch.clone()]);
    } else {
        args.push("--detach".to_owned());
    }
    args.push(head);
    let at = tree.clone();
    let (rc, out) = permit
        .blocking(move |hold| {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            hold.git_in(&at, &args, 60)
        })
        .await
        .map_err(|e| e.to_string())?;
    drop(permit);
    if rc != 0 {
        return Err(format!("checkout failed: {}", crate::util::tail(&out, 300)));
    }
    if tree_state(tree).await.description != before.description {
        restore()?;
    }
    if tree_state(tree).await == *before {
        Ok(())
    } else {
        Err("the tree still differs".to_owned())
    }
}

/// The untracked paths `git status --porcelain` lists `after` and not
/// `before`, relative to the tree. A quoted (escaped) path or one that
/// leaves the tree is skipped: it is not removed.
fn added_untracked<'a>(before: &TreeState, after: &'a TreeState) -> Vec<&'a str> {
    // Not knowing what was there is not knowing that nothing was.
    let Some(had) = before.status.as_deref() else {
        return Vec::new();
    };
    let had: Vec<&str> = had.lines().collect();
    after
        .status
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|line| !had.contains(line))
        .filter_map(|line| line.strip_prefix("?? "))
        .filter(|path| {
            !path.starts_with('"')
                && Path::new(path)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
        })
        .collect()
}

/// Resume the fixer's session on the review's `issues`, under
/// what is left of the job's grant after `job`.
async fn answer_review(
    rounds: &Rounds<'_>,
    issues: &[serde_json::Value],
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
/// description that becomes the PR body (untracked, so `status` shows it
/// the same whatever it says).
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct TreeState {
    head: Option<String>,
    /// The ref `HEAD` points at (`refs/heads/<branch>`); `None` detached.
    branch: Option<String>,
    status: Option<String>,
    description: Option<Vec<u8>>,
}

async fn tree_state(tree: &Path) -> TreeState {
    let tree: PathBuf = tree.to_owned();
    tokio::task::spawn_blocking(move || {
        let ok = |(rc, out): (i32, String)| (rc == 0).then(|| out.trim().to_owned());
        TreeState {
            head: ok(Unheld::Commit { rev: "HEAD" }.run(&tree, 30)),
            branch: ok(Unheld::HeadRef.run(&tree, 30)),
            status: ok(Unheld::Status.run(&tree, 60)),
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
fn parse_review(out: &Path) -> Result<(Vec<serde_json::Value>, usize), String> {
    let bytes = std::fs::read(out).map_err(|e| format!("no review file: {e}"))?;
    let entries: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
        .map_err(|e| format!("the review file is not an array: {e}"))?;
    let total = entries.len();
    let mut issues: Vec<serde_json::Value> = entries
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
            issue
        })
        .collect();
    let dropped = total - issues.len();
    issues.sort_by_key(|entry| !is_blocking(entry));
    Ok((issues, dropped))
}

/// Whether the reviewer marked `issue` blocking.
fn is_blocking(issue: &serde_json::Value) -> bool {
    issue.get("blocking").and_then(serde_json::Value::as_bool) == Some(true)
}

/// [`tally`], and whether the reviewer's changes to the tree were undone.
fn raised(issues: &[serde_json::Value], undone: bool) -> String {
    if undone {
        format!(
            "{} (the reviewer changed the worktree; undone)",
            tally(issues)
        )
    } else {
        tally(issues)
    }
}

/// Whether the reviewer judged `issue` outside the change's scope: a real
/// defect it did not introduce or worsen, or a generalization beyond the
/// finding. Reported to the human, never answered by the fixer.
fn is_follow_up(issue: &serde_json::Value) -> bool {
    issue.get("kind").and_then(serde_json::Value::as_str) == Some("follow-up")
}

/// Add a round's `raised` follow-ups to `all`: one per summary, the latest
/// round's wording winning.
fn merge_follow_ups(all: &mut Vec<serde_json::Value>, raised: Vec<serde_json::Value>) {
    let summary = |v: &serde_json::Value| v.get("summary").cloned();
    for issue in raised {
        all.retain(|kept| summary(kept) != summary(&issue));
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
fn tally(issues: &[serde_json::Value]) -> String {
    let blocking = issues.iter().filter(|i| is_blocking(i)).count();
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
    job.duration_s += run.duration_s;
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
fn failed(round: u32, why: &str, head: Option<String>, issues: &[serde_json::Value]) -> Ending {
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
fn unresolved(round: u32, issues: &[serde_json::Value], unanswered: Option<&str>) -> Ending {
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
fn unresolved_note(round: u32, issues: &[serde_json::Value]) -> String {
    let mut note = format!(
        "## Unresolved review issues\n\nAutomated pre-PR review round {round} raised these \
         issues, and the fix ships without resolving them:\n"
    );
    issue_list(&mut note, issues);
    note
}

/// The PR body section with the follow-ups every round raised.
fn follow_ups_note(follow_ups: &[serde_json::Value]) -> String {
    let mut note = "## Follow-ups from review\n\nThe automated pre-PR review found these \
                    defects outside this change's scope. They are not addressed here:\n"
        .to_owned();
    issue_list(&mut note, follow_ups);
    note
}

/// `issues` as a Markdown list on `note`: blocking ones marked, each with
/// its place and detail.
fn issue_list(note: &mut String, issues: &[serde_json::Value]) {
    let field = |issue: &serde_json::Value, key: &str| match issue.get(key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    for issue in issues {
        let place = match (field(issue, "file"), field(issue, "line")) {
            (file, _) if file.is_empty() => String::new(),
            (file, line) if line.is_empty() || line == "0" => format!(" (`{file}`)"),
            (file, line) => format!(" (`{file}:{line}`)"),
        };
        let mark = if is_blocking(issue) {
            "[blocking] "
        } else {
            ""
        };
        let _ = write!(note, "\n- {mark}**{}**{place}", field(issue, "summary"));
        for line in field(issue, "detail").lines() {
            note.push_str("\n  ");
            note.push_str(line);
        }
    }
}
