#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_fix` — worktree setup.
//!
//! The branch a fix job works on is named after the finding, so it is the
//! same name on every attempt. Any attempt that ends without deleting the
//! branch (the reclaim path only fires when the worktree *directory* is
//! still there) leaves `git worktree add -b` failing with "a branch named
//! ... already exists" — on that cycle and on every cycle after it, since
//! nothing about waiting changes it. Observed 2026-09-15: five
//! consecutive cycles two minutes apart, all for finding 3304, ended only
//! by restarting the daemon.

mod support;

use hunter::config::Config;
use hunter::domain::{FindingJobKind, ForgeName, HoldReason, ReleaseAction};
use hunter::store::{FindingInsert, WorkStreak};
use support::{GitRepo, ScriptedBackend, TempDir, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// Matches the slug `run_fix` derives from the finding summary below.
const BRANCH: &str = "fix/a-real-bug-1";

struct Fixture {
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    repo_dir: std::path::PathBuf,
    db: std::path::PathBuf,
    /// Last, so the directory outlives the Store's SQLite pool. See
    /// `runner_engage_test` for why, and for what it does not fix.
    _dir: TempDir,
}

/// A repo with a queued bug finding — the state `run_fix` expects.
async fn fixture(label: &str) -> Fixture {
    fixture_on(label, REPO_URL, ForgeName::Github).await
}

/// [`fixture`] for a repo registered under `url` on `forge`.
async fn fixture_on(label: &str, url: &str, forge: ForgeName) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "some-other-branch");
    let (db, store) = fresh_store(&dir, "fix").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo("widget", url, &repos_root, &repo.default_branch, forge)
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();

    let (fid, _) = store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "fp-fix-1".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "a real bug".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store
        .set_finding_status(fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();

    // Hermetic stub: the scripted worker ignores the prompt, so all the
    // template has to do is render.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        _dir: dir,
        cfg,
        store,
        fid,
        repo_dir,
        db,
    }
}

/// The regression: a leftover branch from an abandoned attempt, with no
/// worktree registered for it, must not wedge every later attempt.
#[tokio::test]
async fn leftover_branch_without_a_worktree_does_not_wedge_the_fix() {
    let f = fixture("fix-leftover-branch").await;
    git(&f.repo_dir, &["branch", BRANCH, "main"]);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Declining needs no forge: the run ends at NOT-A-BUG.md, so what is
    // under test is the worktree setup that precedes it.
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .expect("a leftover branch is reclaimable, not a permanent failure");

    assert_eq!(summary.branch.as_deref(), Some(BRANCH));
    assert_eq!(
        summary.outcome.as_deref(),
        Some("rejected"),
        "the worker ran and its verdict was recorded: {summary:?}"
    );
}

/// The same branch, still checked out by a registration whose tree is
/// gone and which git left locked: what a daemon killed during
/// `git worktree add` leaves behind. `prune` skips a locked worktree and
/// `branch -D` refuses a checked-out branch, so without unlocking it every
/// later attempt failed (observed 2026-09-29: 400 cycles, finding 3949).
#[tokio::test]
async fn leftover_branch_held_by_a_killed_worktree_add_does_not_wedge_the_fix() {
    let f = fixture("fix-locked-leftover").await;
    let tree = f.cfg.work_root.join("jobs").join("999").join("tree");
    let t = tree.to_string_lossy();
    git(&f.repo_dir, &["worktree", "add", "-b", BRANCH, &t, "main"]);
    git(
        &f.repo_dir,
        &["worktree", "lock", "--reason", "initializing", &t],
    );
    std::fs::remove_dir_all(tree.parent().unwrap()).unwrap();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(
        summary.outcome.as_deref(),
        Some("rejected"),
        "the worker ran and its verdict was recorded: {summary:?}"
    );
}

/// The same run with no leftover branch, so the test above is known to be
/// asserting on a difference rather than on a path that always works.
#[tokio::test]
async fn clean_repo_runs_the_fix() {
    let f = fixture("fix-clean").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code");
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("rejected"));
}

/// A cycle hands a queued finding to the fix runner: the same fix as
/// above, picked and dispatched by `run_cycle` rather than called direct.
#[tokio::test]
async fn a_cycle_runs_the_queued_fix() {
    let f = fixture("fix-cycle").await;
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code");

    let summary = hunter::scheduler::run_cycle(&f.store, &f.cfg, &backend, None).await;

    assert_eq!(summary.finding_id, Some(f.fid), "{summary:?}");
    assert_eq!(summary.outcome.as_deref(), Some("rejected"), "{summary:?}");
}

/// A fix whose tree can never be created -- here the repo names a
/// default branch origin does not have -- is a failed fix attempt like any
/// other: it stays `queued` for the retry and, after
/// `MAX_CONSECUTIVE_SAME_FAILURE` (3) of them in a row, is held as stuck
/// (`blocked`). Left `queued` with nothing counted, `pick_next` (oldest queued
/// first) would re-pick it every cycle and no other queued fix would run.
#[tokio::test]
async fn a_tree_that_can_never_be_made_ends_held_as_stuck() {
    use hunter::domain::FindingStatus;

    let f = fixture("fix-never-a-tree").await;
    let rid = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
    f.store
        .update_repo(
            rid,
            &hunter::store::RepoUpdate {
                default_branch: Some("no-such-branch".to_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(
            finding.status,
            FindingStatus::Queued,
            "before attempt {attempt}"
        );
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, None)
            .await
            .unwrap();
        assert!(
            summary
                .failure
                .as_deref()
                .is_some_and(|r| r.starts_with("workspace not created")),
            "{summary:?}"
        );
        outcomes.push(summary.outcome.unwrap_or_default());
    }

    assert_eq!(outcomes, ["requeued", "requeued", "held"]);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        finding.status,
        FindingStatus::Queued,
        "a hold keeps the status"
    );
    let hold = held(&f).await.expect("held at the limit");
    assert_eq!(hold.reason, HoldReason::Stuck);
    assert!(
        hold.detail.starts_with("workspace not created") && hold.detail.contains("no-such-branch"),
        "{hold:?}"
    );
    // No worker ever ran, so there is no checkpoint to continue: the
    // operator can restart it, not continue it.
    assert_eq!(hold.checkpoint_job_id, None);
    assert!(
        !hold.actions().contains(&ReleaseAction::Continue),
        "{hold:?}"
    );
    assert!(hold.actions().contains(&ReleaseAction::Restart), "{hold:?}");
}

// ---------------------------------------------------------------------------
// The cap the job is granted
// ---------------------------------------------------------------------------

/// The granted cap is the backend's headroom, unaltered.
///
/// It used to be `min(config cap, headroom)`, the config number being a
/// hand-picked constant (`fix.capNewTokens`, default 150 000). Such a
/// constant drifts below what the jobs it governs actually cost and then
/// kills work the ramp had already found room for, while the ramp's own
/// headroom — computed from live window state — bounds the same spend
/// correctly. Two bounds, one of them blind.
#[tokio::test]
async fn granted_cap_is_the_backend_headroom_verbatim() {
    let f = fixture("fix-cap-verbatim").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Deliberately above every cap the config used to impose, so a
    // surviving min() shows up as the config number instead.
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code")
            .granting(Some(777_000));
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let jobs = f.store.list_jobs(10).await.unwrap();
    assert_eq!(
        jobs[0].job.cap_tokens,
        Some(777_000),
        "the job must run under the headroom the backend granted"
    );
}

/// A backend that grants no ceiling leaves the job with no token bound:
/// NULL in `jobs.cap_tokens`, `maxWallS` the only remaining stop
/// condition. The old fallback turned "the ramp sees no reason to bound
/// this" into "bound it at the config cap".
#[tokio::test]
async fn backend_without_a_ceiling_leaves_the_job_unbounded() {
    let f = fixture("fix-cap-none").await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code")
            .granting(None);
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let jobs = f.store.list_jobs(10).await.unwrap();
    assert_eq!(
        jobs[0].job.cap_tokens, None,
        "no headroom bound means no token bound, not the config's"
    );
}

/// A fix whose tree cannot be made goes back to `queued`, not stranded
/// in `fixing` until the next daemon restart.
#[tokio::test]
async fn fix_whose_tree_cannot_be_made_returns_the_finding_to_queued() {
    let f = fixture("fix-no-tree").await;
    // The first job's workspace is already taken, so creating it is refused.
    let taken = f.cfg.work_root.join("jobs").join("1").join("session");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("other.jsonl"), "someone else's\n").unwrap();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, None)
        .await
        .unwrap();
    assert_eq!(summary.state, Some(hunter::domain::JobState::Failed));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
}

/// The shipping path: a finished fix is pushed and its draft PR recorded,
/// both for a new PR and for the "already exists" recovery.
#[tokio::test]
async fn a_finished_fix_ships_and_records_its_draft_pr() {
    const URL: &str = "https://github.com/acme/widget/pull/42";
    for case in ["created", "exists"] {
        let bins = support::FakeBins::acquire("fix-ship");
        let f = fixture("fix-ship").await;
        // The commits to ship are counted against origin's default branch;
        // a local `main` must not stand in for it.
        git(&f.repo_dir, &["checkout", "--detach"]);
        git(&f.repo_dir, &["branch", "-D", "main"]);
        // Push to the fixture's bare origin instead of the forge.
        let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
        git(
            &f.repo_dir,
            &[
                "config",
                &format!("url.{}.insteadOf", origin.trim()),
                "git@github.com:acme/widget.git",
            ],
        );
        if case == "created" {
            bins.ok("gh", URL);
        } else {
            bins.fail("gh", 1, &format!("a pull request already exists: {URL}"));
        }
        let worker = ScriptedBackend::new(|tree| {
            std::fs::write(tree.join("fix.txt"), "fixed\n").unwrap();
            git(tree, &["add", "fix.txt"]);
            git(tree, &["commit", "-m", "fix it"]);
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the fix").unwrap();
            support::done()
        });
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
            .await
            .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{case}");
        assert_eq!(
            summary.kind,
            Some(hunter::domain::FindingJobKind::Fix.into()),
            "{case}"
        );
        assert_eq!(summary.finding_id, Some(f.fid), "{case}");
        assert_eq!(
            summary.tokens_new,
            Some(support::done().tokens_new),
            "{case}"
        );
        assert_eq!(summary.pr_url.as_deref(), Some(URL), "{case}");
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(finding.status.as_str(), "pr_open", "{case}");
        assert_eq!(finding.pr_url.as_deref(), Some(URL), "{case}");
        drop(bins);
    }
}

/// A verdict that lands after `pick_next` read the finding wins: the stale
/// fix creates no job, so it neither overwrites the operator's rejection
/// with `fixing` nor continues the checkpoint, which stays resumable if the
/// operator changes their mind.
#[tokio::test]
async fn verdict_after_selection_stops_the_fix_before_its_job() {
    let f = fixture("fix-stale-selection").await;
    let capped = ScriptedBackend::staged(|tree, _| {
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let mut result = support::done();
        result.exit_code = None;
        result.killed_reason = Some("cap".to_owned());
        result.session_file = Some(session.to_string_lossy().into_owned());
        result
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &capped, None)
        .await
        .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"));
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a cap suspension must resume");
    };
    let stale = f.store.get_finding(f.fid).await.unwrap().unwrap();
    f.store
        .set_finding_verdict(
            f.fid,
            hunter::domain::FindingStatus::Rejected,
            "not worth it",
        )
        .await
        .unwrap();

    let never = ScriptedBackend::new(|_| panic!("a rejected finding must not run"));
    let result = hunter::scheduler::run_fix(&f.store, &f.cfg, &stale, &never, Some(&plan))
        .await
        .unwrap();
    assert!(result.skipped.is_some());
    assert!(result.job_id.is_none());
    assert_eq!(
        result.kind,
        Some(hunter::domain::FindingJobKind::Fix.into()),
        "the cycle status line names the skipped kind"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "rejected");
    assert_eq!(finding.verdict_reason.as_deref(), Some("not worth it"));

    // The verdict took the finding out of the fix's tier, which ends the
    // work item: queued again, the fix starts over.
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let picked = hunter::scheduler::pick_next(&f.store, &f.cfg, None)
        .await
        .unwrap();
    assert!(
        matches!(
            picked,
            Some(hunter::scheduler::Candidate::Finding { finding_id, .. }) if finding_id == f.fid
        ),
        "{picked:?}"
    );
    let job = summary.job_id.unwrap();
    let row = f.store.jobs_by_finding(f.fid).await.unwrap();
    let row = row.iter().find(|j| j.id == job).unwrap();
    assert_eq!(
        row.state,
        hunter::domain::JobState::Suspended,
        "left as it ended"
    );
}

/// Stage an implemented fix plus durable reports and its metered transcript,
/// whose worker then writes `BLOCKED.md`.
async fn held_fixture() -> (
    Fixture,
    ScriptedBackend,
    hunter::scheduler::CycleSummary,
    String,
) {
    let f = fixture("fix-held-checkpoint").await;
    let reason = format!(
        "Missing supported compiler\n{}",
        "Build prerequisite detail\n".repeat(40)
    );
    let expected_reason = reason.clone();
    let blocked = ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("candidate.txt"), "implemented fix\n").unwrap();
        git(tree, &["add", "candidate.txt"]);
        git(tree, &["commit", "-m", "implemented candidate"]);
        std::fs::write(tree.join("BLOCKED.md"), &reason).unwrap();
        std::fs::write(
            tree.join("PR-DESCRIPTION.md"),
            "verified candidate; awaiting prerequisite",
        )
        .unwrap();
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let mut result = support::done();
        result.session_file = Some(session.to_string_lossy().into_owned());
        result
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &blocked, None)
        .await
        .unwrap();
    (f, blocked, summary, expected_reason)
}

/// A worker's `BLOCKED.md` holds the fix for the operator as a missing
/// prerequisite. Not suppressed, not retried on its own, and the status
/// stays `queued`: the hold is the work item's, not the finding's. The
/// attempt is the checkpoint, the report the hold's detail, and the tree
/// holds no stale `BLOCKED.md`; a sweep keeps the committed work.
#[tokio::test]
async fn a_blocked_report_holds_the_fix_and_keeps_its_checkpoint() {
    let (f, blocked, summary, expected_reason) = held_fixture().await;
    assert_eq!(summary.outcome.as_deref(), Some("held"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
    let events = f.store.recent_events(20).await.unwrap();
    assert!(
        events.iter().any(|e| e.message.starts_with(&format!(
            "#{} held: prerequisite; checkpoint retained at",
            f.fid
        ))),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "hold" && e.finding_id == Some(f.fid)),
        "{events:?}"
    );
    let hold = held(&f).await.expect("held");
    assert_eq!(hold.reason, HoldReason::Prerequisite);
    assert_eq!(hold.detail, expected_reason);
    assert_eq!(hold.checkpoint_job_id, summary.job_id);
    assert_eq!(finding.verdict_reason, None, "the report is not copied");
    let tree = blocked.runs()[0].tree.clone();
    assert_eq!(
        std::fs::read_to_string(tree.join("candidate.txt")).unwrap(),
        "implemented fix\n"
    );
    assert!(!tree.join("BLOCKED.md").exists());
    assert!(tree.join("PR-DESCRIPTION.md").is_file());
    assert!(f.store.suppressions(1, "bug").await.unwrap().is_empty());
    let picked = hunter::scheduler::pick_next(&f.store, &f.cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(
            picked,
            Some(
                hunter::scheduler::Candidate::Resume { .. }
                    | hunter::scheduler::Candidate::Finding { .. }
            )
        ),
        "held work is skipped: {picked:?}"
    );
    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(tree.is_dir(), "a held checkpoint must survive a sweep");
}

/// `continue` resumes the checkpoint with the prerequisite in its prompt.
/// A budget denial leaves the checkpoint, and a `BLOCKED.md` still in the
/// tree is the old report, removed before it could read as this attempt's
/// outcome. After that the chain is ordinary (WORK-MODEL.md §8): an attempt
/// that fails is a counted failure, not held again on the spot.
#[tokio::test]
async fn a_continued_fix_resumes_its_checkpoint_and_is_ordinary_again() {
    let (f, blocked, summary, expected_reason) = held_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume, not start cold");
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
    assert_eq!(
        plan.after_hold,
        Some((HoldReason::Prerequisite, expected_reason.clone()))
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let denied = ScriptedBackend::noop().denying_above(0);
    let result = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &denied, Some(&plan))
        .await
        .unwrap();
    assert!(result.denied.is_some());
    assert_eq!(
        checkpoint(&f).await,
        summary.job_id,
        "budget denial must leave the checkpoint"
    );
    // A report left in the tree by a daemon that died between recording it
    // and deleting it is the old one, not this attempt's outcome.
    std::fs::write(tree.join("BLOCKED.md"), &expected_reason).unwrap();

    let provider_failure = ScriptedBackend::staged(|tree, session| {
        assert!(
            !tree.join("BLOCKED.md").exists(),
            "the stale report must not read as this attempt's outcome"
        );
        let mut result = support::done();
        result.exit_code = Some(1);
        result.tokens_new = 0;
        result.stdout_tail = "provider access denied".to_owned();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let failed =
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &provider_failure, Some(&plan))
            .await
            .unwrap();
    let prompt = &provider_failure.runs()[0].prompt;
    assert!(
        prompt.contains("Re-evaluate the prerequisite")
            && prompt.contains("Missing supported compiler"),
        "{prompt}"
    );
    assert_eq!(failed.outcome.as_deref(), Some("requeued"), "{failed:?}");
    assert!(held(&f).await.is_none(), "not held again on the spot");
    assert_eq!(fix_streak(&f).await.streak, 1);
    assert_eq!(
        checkpoint(&f).await,
        None,
        "a failure below the limit clears the checkpoint (WORK-MODEL.md §6.3)"
    );
}

/// A daemon that dies during the attempt a `continue` started leaves an
/// orphan that startup reconciliation suspends. Resuming it is an ordinary
/// resume: the transcript already holds the post-release prompt, and the
/// chain is bounded by the ceiling and the streak, not held again.
#[tokio::test]
async fn an_interrupted_continue_resumes_as_an_ordinary_resume() {
    let (f, _, _, _) = held_fixture().await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    // The process dies after the worker has metered work of its own.
    let crashing = ScriptedBackend::staged(|_, session| {
        let session = session.unwrap();
        let mut ledger = std::fs::read_to_string(session).unwrap();
        ledger.push_str(
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":500,\"output\":50}}}\n",
        );
        std::fs::write(session, ledger).unwrap();
        panic!("daemon killed mid-attempt");
    });
    let (store, cfg) = (f.store.clone(), f.cfg.clone());
    let crashed = tokio::spawn(async move {
        hunter::scheduler::run_fix(&store, &cfg, &finding, &crashing, Some(&plan)).await
    })
    .await;
    assert!(crashed.unwrap_err().is_panic());

    hunter::daemon::reconcile_and_log(&f.store, &f.cfg.work_root)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the interrupted attempt must resume");
    };
    assert_eq!(plan.after_hold, None, "an ordinary resume");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let provider_failure = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.exit_code = Some(1);
        result.tokens_new = 0;
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let failed =
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &provider_failure, Some(&plan))
            .await
            .unwrap();
    assert_eq!(failed.outcome.as_deref(), Some("requeued"));
    assert_eq!(
        provider_failure.runs()[0].prompt,
        "Continue the work you were doing in this session. You were interrupted; \
         pick up where you left off."
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
    assert!(held(&f).await.is_none());
}

/// The fixture finding's fix streak.
async fn fix_streak(f: &Fixture) -> WorkStreak {
    f.store
        .work_streak(f.fid, FindingJobKind::Fix)
        .await
        .unwrap()
}

/// The fixture finding's active fix hold.
async fn held(f: &Fixture) -> Option<hunter::store::WorkHold> {
    support::hold_of(&f.store, f.fid, FindingJobKind::Fix).await
}

/// The fixture finding's fix checkpoint.
async fn checkpoint(f: &Fixture) -> Option<i64> {
    f.store
        .checkpoint_job(f.fid, FindingJobKind::Fix)
        .await
        .unwrap()
        .map(|j| j.id)
}

/// A continued fix that concludes ends like any other attempt: one that
/// finished without a PR description is a counted failure, one that
/// declined the finding takes the decline's status.
#[tokio::test]
async fn a_continued_fix_that_concludes_ends_ordinarily() {
    for (case, expected) in [("finished", "requeued"), ("declined", "rejected")] {
        let (f, _blocked, _, _) = held_fixture().await;
        support::release(
            &f.store,
            f.fid,
            FindingJobKind::Fix,
            ReleaseAction::Continue,
        )
        .await;
        let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
            hunter::scheduler::pick_next(&f.store, &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("continued work must resume");
        };
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let worker = ScriptedBackend::staged(move |tree, session| {
            let mut result = support::done();
            if case == "finished" {
                std::fs::remove_file(tree.join("PR-DESCRIPTION.md")).unwrap();
            } else {
                std::fs::write(
                    tree.join("NOT-A-BUG.md"),
                    "Classification: wrong\nintended behaviour",
                )
                .unwrap();
                result.exit_code = Some(1);
            }
            result.session_file = session.map(|path| path.to_string_lossy().into_owned());
            result
        });
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, Some(&plan))
            .await
            .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some(expected), "{case}");
        if case == "finished" {
            assert_eq!(summary.failure.as_deref(), Some("no PR-DESCRIPTION.md"));
        }
        assert!(held(&f).await.is_none(), "{case}");
    }
}

/// Identical failures are retried until the streak limit, then the work
/// is held as stuck with its tree, rather than rejected.
#[tokio::test]
async fn repeated_identical_fix_failures_are_held_at_the_limit() {
    let f = fixture("fix-stuck").await;
    let empty = ScriptedBackend::noop();
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &empty, None)
            .await
            .unwrap();
        assert_eq!(summary.failure.as_deref(), Some("no commits"));
        outcomes.push(summary.outcome.unwrap());
    }
    assert_eq!(outcomes, ["requeued", "requeued", "held"]);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
    let tree = empty.runs()[2].tree.clone();
    assert!(
        !tree.join("BLOCKED.md").exists(),
        "the report lives on the hold"
    );
    let hold = held(&f).await.expect("held at the limit");
    assert_eq!(hold.reason, HoldReason::Stuck);
    assert!(hold.detail.starts_with("no commits"), "{hold:?}");
    assert_eq!(
        fix_streak(&f).await,
        WorkStreak::default(),
        "a hold ends the streak"
    );
}

/// The prompt a held fix resumes with: continue it, resume its checkpoint
/// once, and return what the worker was told.
async fn resumed_hold_prompt(f: &Fixture) -> String {
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued held work must resume");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let worker = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, Some(&plan))
        .await
        .unwrap();
    worker.runs()[0].prompt.clone()
}

/// A held fix resumes against what held it, chosen by the hold's reason
/// rather than its text. A worker's `BLOCKED.md` names a prerequisite to
/// re-evaluate. A failure streak names none, only what kept failing, so
/// its resume asks the worker to finish: told to re-evaluate a
/// prerequisite and recreate `BLOCKED.md`, it could hold the fix again
/// over nothing, without any progress.
#[tokio::test]
async fn a_held_fix_resumes_against_what_held_it() {
    let f = fixture("fix-streak-resume").await;
    let unfinished = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("candidate.txt"), "partial fix\n").unwrap();
        git(tree, &["add", "candidate.txt"]);
        git(tree, &["commit", "-m", "partial candidate"]);
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let mut result = support::done();
        result.session_file = Some(session.to_string_lossy().into_owned());
        result
    });
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &unfinished, None)
            .await
            .unwrap();
    }
    assert_eq!(held(&f).await.map(|h| h.reason), Some(HoldReason::Stuck));
    let streak = resumed_hold_prompt(&f).await;
    let (reported, _, _, _) = held_fixture().await;
    let report = resumed_hold_prompt(&reported).await;

    assert!(
        streak.contains("kept failing the same way") && streak.contains("no PR-DESCRIPTION.md"),
        "{streak}"
    );
    assert!(
        !streak.contains("prerequisite") && !streak.contains("BLOCKED.md"),
        "{streak}"
    );
    assert!(report.contains("Missing supported compiler"), "{report}");
    assert!(
        report.contains("Re-evaluate the prerequisite") && report.contains("BLOCKED.md"),
        "{report}"
    );
}

/// A resume of a chain that was never blocked (a cap suspension) is the
/// plain continuation, and ending without an outcome is a plain requeue.
#[tokio::test]
async fn fix_resume_without_a_blocker_runs_the_plain_continuation() {
    let f = fixture("fix-cap-resume").await;
    let capped = ScriptedBackend::staged(|tree, _| {
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
        )
        .unwrap();
        let mut result = support::done();
        result.exit_code = None;
        result.killed_reason = Some("cap".to_owned());
        result.session_file = Some(session.to_string_lossy().into_owned());
        result
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &capped, None)
        .await
        .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"));
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a cap suspension must resume");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let resumed = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &resumed, Some(&plan))
        .await
        .unwrap();
    assert_eq!(resumed.runs().len(), 1);
    assert_eq!(summary.outcome.as_deref(), Some("requeued"));
}

/// A stale report in the tree that cannot be removed stops the resume
/// before any successor exists (running on would let it read as this
/// attempt's outcome), and gives the finding back to `queued` with the
/// checkpoint still its resumable tip once the obstacle is cleared.
#[tokio::test]
async fn an_unremovable_stale_report_stops_the_resume_before_its_job() {
    let (f, blocked, _, _) = held_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };
    let jobs_before = f.store.jobs_by_finding(f.fid).await.unwrap().len();
    std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("must not run beside a stale report"));
    let err = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, Some(&plan))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot remove the stale "),
        "{err}"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
    assert_eq!(
        f.store.jobs_by_finding(f.fid).await.unwrap().len(),
        jobs_before,
        "no successor was created"
    );

    std::fs::remove_dir(tree.join("BLOCKED.md")).unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan: again, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the checkpoint must still resume");
    };
    assert_eq!(again.predecessor_id, plan.predecessor_id);
}

/// Rejecting a held fix ends its work item: the release records the
/// verdict, the finding leaves the tier, which drops the checkpoint, and
/// the next sweep releases its tree instead of holding it forever. The job
/// row stays as it ended.
#[tokio::test]
async fn rejecting_a_held_fix_lets_the_sweep_release_its_checkpoint() {
    let (f, blocked, summary, _) = held_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    let job = summary.job_id.unwrap();

    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(tree.is_dir(), "a held checkpoint is kept");

    let by = support::operator(&f.store).await;
    f.store
        .release_hold(
            f.fid,
            FindingJobKind::Fix,
            &hunter::store::HoldRelease {
                action: ReleaseAction::Reject,
                reason: Some("not worth it"),
                classification: None,
                by: &by,
            },
        )
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        (finding.status.as_str(), finding.verdict_reason.as_deref()),
        ("rejected", Some("not worth it"))
    );
    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(!tree.exists(), "the rejected checkpoint's tree is released");
    let jobs = f.store.jobs_by_finding(f.fid).await.unwrap();
    let held_job = jobs.iter().find(|j| j.id == job).unwrap();
    assert_eq!(held_job.state, hunter::domain::JobState::Done);
    assert_eq!(checkpoint(&f).await, None);
    assert!(held(&f).await.is_none());
}

/// A fix whose backend fails before any worker runs is a failed fix
/// (WORK-MODEL.md §7): its job is closed `failed` as unstarted instead of
/// left `running`, the failure counts in the streak under the fixed
/// `runner error`, and the third in a row holds the fix. Before, nothing
/// was counted and the cycle failed, the same fix picked again five
/// minutes later, forever. No worker ran, so nothing replaced the
/// checkpoint the fix was continuing (§6.3): every attempt continues it,
/// and the hold keeps it, so the operator can still `continue`.
#[tokio::test]
async fn a_fix_whose_backend_keeps_failing_is_held() {
    let (f, _blocked, summary, _) = held_fixture().await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let failing = ScriptedBackend::failing();
    for attempt in 1..=3 {
        let plan = match hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
        {
            Some(hunter::scheduler::Candidate::Resume { plan, .. }) => Some(*plan),
            _ => None,
        };
        assert_eq!(
            plan.as_ref().map(|p| p.predecessor_id),
            summary.job_id,
            "the continued checkpoint: attempt {attempt}"
        );
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &failing, plan.as_ref())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("backend failed"), "{err}");
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.status.as_str(), "queued");
        if attempt < 3 {
            let streak = fix_streak(&f).await;
            assert_eq!(
                (streak.streak, streak.last_failure.as_deref()),
                (attempt, Some("runner error"))
            );
        }
    }

    let hold = held(&f).await.expect("held at the limit");
    assert_eq!(hold.reason, HoldReason::Stuck);
    assert!(hold.detail.contains("backend failed"), "{hold:?}");
    assert_eq!(hold.checkpoint_job_id, summary.job_id);
    assert!(hold.actions().contains(&ReleaseAction::Continue));
    let jobs = f.store.jobs_by_finding(f.fid).await.unwrap();
    assert!(
        jobs.iter()
            .all(|j| j.state != hunter::domain::JobState::Running),
        "no job is left running: {jobs:?}"
    );
}

/// Whatever bytes a worker writes into its report are the report: a
/// `BLOCKED.md` or `NOT-A-BUG.md` that is not valid UTF-8 still blocks or
/// declines, its text read lossily.
#[tokio::test]
async fn a_report_that_is_not_utf8_still_counts() {
    for (file, expected, status) in [
        ("BLOCKED.md", "held", "queued"),
        ("NOT-A-BUG.md", "new", "new"),
    ] {
        let f = fixture("fix-non-utf8").await;
        let worker = ScriptedBackend::new(move |tree| {
            std::fs::write(tree.join(file), b"needs a rig \xff\xfe").unwrap();
            support::done()
        });
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
            .await
            .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some(expected), "{file}");
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(finding.status.as_str(), status, "{file}");
        let report = if file == "BLOCKED.md" {
            held(&f).await.map(|h| h.detail)
        } else {
            finding.verdict_reason
        };
        assert_eq!(
            report.as_deref(),
            Some("needs a rig \u{fffd}\u{fffd}"),
            "{file}"
        );
    }
}

/// A declined fix lands as its classification's status, the way a closed
/// PR's does, and only `wrong` and `unwanted` reach the suppression list
/// every later scan is shown. Before, every decline was `rejected`: of 38
/// rejected findings on 2026-10-09, 14 were already fixed at HEAD and 6
/// declined by policy, all telling later scans not to report them again.
/// A first line that classifies nothing, or a classification with no
/// explanation, goes back to triage with the whole report.
#[tokio::test]
async fn a_decline_lands_as_its_classification() {
    let cases = [
        (
            "Classification: superseded\nfixed by abc1234",
            "superseded",
            "superseded: fixed by abc1234",
            false,
        ),
        (
            "Classification: duplicate\nsee #7",
            "superseded",
            "duplicate: see #7",
            false,
        ),
        (
            "Classification: obsolete\ncode gone",
            "superseded",
            "obsolete: code gone",
            false,
        ),
        (
            "**Classification:** `Wrong`\n\n# NOT A BUG\nintended",
            "rejected",
            "wrong: # NOT A BUG\nintended",
            true,
        ),
        (
            "Classification: unwanted\nCI pins Node 24",
            "wontfix",
            "unwanted: CI pins Node 24",
            true,
        ),
        ("# NOT A BUG\nmisread", "new", "# NOT A BUG\nmisread", false),
        (
            "Classification: sideways\nhm",
            "new",
            "Classification: sideways\nhm",
            false,
        ),
        (
            "Classification: wrong\n  \n",
            "new",
            "Classification: wrong\n  \n",
            false,
        ),
    ];
    for (report, status, reason, suppressed) in cases {
        let f = fixture("fix-decline-class").await;
        let backend = ScriptedBackend::writing("NOT-A-BUG.md", report);
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &backend, None)
            .await
            .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some(status), "{report:?}");
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.status.as_str(), status, "{report:?}");
        assert_eq!(after.verdict_reason.as_deref(), Some(reason), "{report:?}");
        let listed = f
            .store
            .suppressions(finding.repo_id, "bug")
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == f.fid);
        assert_eq!(listed, suppressed, "{report:?}");
    }
}

/// A report that cannot be read at all is an error, but it does not leave
/// the finding stranded at `fixing`.
#[tokio::test]
async fn an_unreadable_report_returns_the_finding_to_queued() {
    let f = fixture("fix-unreadable-report").await;
    let worker = ScriptedBackend::new(|tree| {
        std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
        support::done()
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let err = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot read the worker's report"),
        "{err}"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
}

/// A verdict given after `pick_next` wins over a stale report too: the
/// resume is skipped at the claim, before the report is looked at, and
/// the finding keeps the operator's status.
#[tokio::test]
async fn a_verdict_after_selection_wins_over_a_stale_report() {
    let (f, blocked, _, _) = held_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };
    let picked = f.store.get_finding(f.fid).await.unwrap().unwrap();
    std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Rejected)
        .await
        .unwrap();
    let never = ScriptedBackend::new(|_| panic!("a rejected finding must not run"));
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &picked, &never, Some(&plan))
        .await
        .unwrap();
    assert!(summary.skipped.is_some(), "{summary:?}");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "rejected");
}

/// Recording a hold can fail (a busy database); the claim would then
/// leave the finding at `fixing`, where nothing would pick it up again
/// before a restart. The fix puts it back to `queued` instead.
#[tokio::test]
async fn a_hold_that_cannot_be_recorded_returns_the_finding_to_queued() {
    let f = fixture("fix-block-not-recorded").await;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::raw_sql(
        "CREATE TRIGGER refuse_hold BEFORE INSERT ON work_holds \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let worker = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "needs a rig").unwrap();
        support::done()
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let err = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("injected"), "{err}");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "queued");
}

/// Each kind gets its own branch prefix and playbook, and every one of
/// them hands the worker the decline classification the scheduler parses:
/// a playbook without it leaves every decline unclassified, back at `new`.
#[tokio::test]
async fn each_fix_kind_gets_its_branch_and_playbook_with_the_decline_classes() {
    for (kind, template, prefix) in [
        ("bug", "fix.md", "fix"),
        ("refactor", "apply_improvement.md", "improve"),
        ("modernization", "apply_modernization.md", "modernize"),
    ] {
        let f = fixture("fix-kinds").await;
        std::fs::write(
            f.cfg.root.join("playbooks").join(template),
            format!("{prefix} {{{{WORKTREE}}}}\n{{{{DECLINE_CLASSIFICATION}}}}\n"),
        )
        .unwrap();
        let repo_id = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
        let (fid, _) = f
            .store
            .upsert_finding(
                repo_id,
                &FindingInsert {
                    fingerprint: format!("fp-{kind}-1"),
                    file: "src/lib.rs".to_owned(),
                    severity: hunter::domain::Severity::Medium,
                    confidence: 0.9,
                    summary: "Move to the new API".to_owned(),
                    ..Default::default()
                },
                kind,
                None,
            )
            .await
            .unwrap();
        f.store
            .set_finding_status(fid, hunter::domain::FindingStatus::Queued)
            .await
            .unwrap();
        let finding = f.store.get_finding(fid).await.unwrap().unwrap();
        let worker = ScriptedBackend::noop();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
            .await
            .unwrap();
        assert_eq!(
            summary.branch.as_deref(),
            Some(format!("{prefix}/move-to-the-new-api-{fid}").as_str()),
            "{kind}"
        );
        let prompt = &worker.runs()[0].prompt;
        assert!(
            prompt.starts_with(&format!("{prefix} ")),
            "{kind}: {prompt}"
        );
        assert!(
            prompt.contains(hunter::playbooks::DECLINE_CLASSIFICATION),
            "{kind}: {prompt}"
        );
    }
}

/// A one-shot budget override is spent by the attempt it bought; an
/// exempt one stays.
#[tokio::test]
async fn a_one_shot_override_is_spent_by_its_fix_attempt() {
    for (mode, after) in [
        (hunter::domain::BudgetOverride::Once, None),
        (
            hunter::domain::BudgetOverride::Exempt,
            Some(hunter::domain::BudgetOverride::Exempt),
        ),
    ] {
        let f = fixture("fix-override").await;
        f.store
            .set_budget_override(f.fid, Some(mode))
            .await
            .unwrap();
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary =
            hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &ScriptedBackend::noop(), None)
                .await
                .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{mode:?}");
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(finding.budget_override, after, "{mode:?}");
    }
}

/// A finding that left `queued` before its fix started ends the cycle as
/// skipped, without a job.
#[tokio::test]
async fn a_fix_for_a_finding_no_longer_queued_is_skipped() {
    let f = fixture("fix-not-queued").await;
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::New)
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("a finding that is not queued must not run"));
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, None)
        .await
        .unwrap();
    assert_eq!(
        summary.kind,
        Some(hunter::domain::FindingJobKind::Fix.into())
    );
    assert_eq!(
        summary.skipped,
        Some(format!("finding #{} is new, not queued", f.fid))
    );
    assert_eq!(summary.job_id, None);
}

/// A PR description without commits ships nothing: there is no change to
/// open a PR for.
#[tokio::test]
async fn a_description_without_commits_ships_nothing() {
    let bins = support::FakeBins::acquire("fix-no-commits");
    bins.ok("gh", "https://github.com/acme/widget/pull/43");
    let f = fixture("fix-no-commits").await;
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.trim()),
            "git@github.com:acme/widget.git",
        ],
    );
    let worker = ScriptedBackend::writing("PR-DESCRIPTION.md", "nothing changed");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &worker, None)
        .await
        .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("requeued"));
    assert_eq!(summary.failure.as_deref(), Some("no commits"));
    assert!(!bins.called_with("gh", "create"), "{:?}", bins.calls());
}

/// Send `run_fix`'s push, which targets the forge's SSH URL, to `target`:
/// the clone's `origin` when `None`, as the shipping tests above do.
fn route_push(f: &Fixture, target: Option<&str>) {
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let target = target.unwrap_or(origin.trim());
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{target}.insteadOf"),
            "git@github.com:acme/widget.git",
        ],
    );
}

/// A worker that commits a change and, if `describe`, writes the PR body.
fn committing(describe: bool) -> ScriptedBackend {
    ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
        git(tree, &["add", "-A"]);
        git(tree, &["commit", "-m", "fix: the real bug"]);
        if describe {
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
        }
        support::done()
    })
}

/// Run one fix attempt of the fixture's finding, as the store has it now.
async fn fix(f: &Fixture, backend: &ScriptedBackend) -> hunter::scheduler::CycleSummary {
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, backend, None)
        .await
        .unwrap()
}

/// What a shipped fix hands the forge: the branch commit reaches origin,
/// and the draft PR is opened from that branch onto the default branch,
/// titled by the last commit and described by `PR-DESCRIPTION.md`. A
/// shipped fix also ends any failure streak.
#[tokio::test]
async fn a_shipped_fix_reaches_origin_and_opens_its_pr_from_the_branch() {
    let bins = support::FakeBins::acquire("fix-ship-argv");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = fixture("fix-ship-argv").await;
    route_push(&f, None);
    f.store
        .record_work_failure(f.fid, FindingJobKind::Fix, "no commits")
        .await
        .unwrap();

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        fix_streak(&f).await,
        WorkStreak::default(),
        "a shipped fix ends the streak"
    );
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let pushed = git(
        std::path::Path::new(origin.trim()),
        &["log", "-1", "--format=%s", BRANCH],
    );
    assert_eq!(
        pushed.trim(),
        "fix: the real bug",
        "the branch reached origin"
    );
    let creates = bins.calls_to("gh");
    assert_eq!(creates.len(), 1, "{creates:?}");
    let call = &creates[0];
    assert_eq!(&call[1..4], ["pr", "create", "--draft"], "{call:?}");
    let after_flag = |flag: &str| {
        let i = call.iter().position(|a| a == flag).unwrap();
        call[i + 1].clone()
    };
    assert_eq!(after_flag("--head"), BRANCH);
    assert_eq!(after_flag("--base"), "main");
    assert_eq!(
        after_flag("--title"),
        "fix: the real bug",
        "last commit subject"
    );
    assert_eq!(after_flag("--body"), "the body");
}

/// A PR-create failure with no PR to recover requeues the finding and
/// counts toward the streak.
#[tokio::test]
async fn a_failed_pr_create_requeues() {
    let bins = support::FakeBins::acquire("fix-pr-fails");
    bins.fail("gh", 1, "HTTP 422: Validation Failed");
    let f = fixture("fix-pr-fails").await;
    route_push(&f, None);

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("PR create failed"), "{failure}");
    assert!(failure.contains("Validation Failed"), "{failure}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, hunter::domain::FindingStatus::Queued);
    assert_eq!(after.pr_url, None);
    let streak = fix_streak(&f).await;
    assert_eq!(streak.streak, 1);
    assert_eq!(streak.last_failure.as_deref(), Some(failure.as_str()));
}

/// A push that fails requeues the finding, and no PR is attempted for a
/// branch the forge does not have.
#[tokio::test]
async fn a_failed_push_requeues_without_creating_a_pr() {
    let bins = support::FakeBins::acquire("fix-push-fails");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = fixture("fix-push-fails").await;
    let nowhere = f.cfg.work_root.join("no-such-remote.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let failure = summary.failure.unwrap();
    assert!(failure.starts_with("push failed"), "{failure}");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, hunter::domain::FindingStatus::Queued);
}

/// Work that cannot be shipped is requeued, named by what is missing: a
/// worker that committed but wrote no PR description, and one that was
/// killed without leaving a transcript to resume.
#[tokio::test]
async fn unshippable_work_is_requeued_with_the_reason() {
    let killed = ScriptedBackend::new(|_| hunter::types::RunResult {
        exit_code: None,
        killed_reason: Some("wallclock".to_owned()),
        ..support::done()
    });
    for (label, backend, reason) in [
        (
            "fix-no-description",
            committing(false),
            "no PR-DESCRIPTION.md",
        ),
        ("fix-worker-killed", killed, "worker killed"),
    ] {
        let f = fixture(label).await;

        let summary = fix(&f, &backend).await;

        assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
        assert_eq!(summary.failure.as_deref(), Some(reason), "{label}");
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            hunter::domain::FindingStatus::Queued,
            "{label}"
        );
        assert_eq!(
            fix_streak(&f).await.last_failure.as_deref(),
            Some(reason),
            "{label}"
        );
    }
}

/// A different failure restarts the streak, so alternating failures are
/// retried rather than held as blocked at the limit.
#[tokio::test]
async fn a_different_failure_restarts_the_streak() {
    let f = fixture("fix-streak-reset").await;
    for _ in 0..2 {
        f.store
            .record_work_failure(f.fid, FindingJobKind::Fix, "no commits")
            .await
            .unwrap();
    }

    let summary = fix(&f, &committing(false)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, hunter::domain::FindingStatus::Queued);
    assert_eq!(fix_streak(&f).await.streak, 1);
}

// ---------------------------------------------------------------------------
// Recovering a PR that already exists
// ---------------------------------------------------------------------------

const GL_REPO_URL: &str = "https://gitlab.com/group/widget";
const GL_MR_URL: &str = "https://gitlab.com/group/widget/-/merge_requests/42";

/// What `glab mr create` (1.119) prints when GitLab answers 409 because
/// the branch already has an open MR. Captured against a stub API, host
/// rewritten. It carries no MR web URL — only `!42` — and glab wraps it
/// at a fixed width, which splits "already exists" for some project
/// paths.
const GLAB_MR_EXISTS: &str = "\nCreating draft merge request for fix/a-real-bug-1 into main in group/widget\n\nFailed to create merge request. Created recovery file: /tmp/glab/recover/group/widget/mr.json\nRun the command again with the '--recover' option to retry.\n          \n   ERROR  \n          \n  Post https://gitlab.com/api/v4/projects/group%2Fwidget/merge_requests: 409 {message: [Another open merge request already\n  exists for this source branch: !42]}.";

/// A retried fix whose MR is already open on GitLab adopts that MR, as a
/// GitHub retry adopts its PR. Failing instead repeats the identical
/// "PR create failed" on every retry until the streak rejects a finding
/// whose fix is sitting in an open MR.
#[tokio::test]
async fn gitlab_retry_adopts_the_mr_that_already_exists() {
    let bins = support::FakeBins::acquire("fix-gl-exists");
    let f = fixture_on("fix-gl-exists", GL_REPO_URL, ForgeName::Gitlab).await;
    // The push goes to git@gitlab.com:group/widget.git; land it on the
    // local bare origin instead.
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.trim()),
            "git@gitlab.com:group/widget.git",
        ],
    );
    bins.script(
        "glab",
        &format!(
            "case \"$1 $2\" in\n\"mr create\") cat >&2 <<'__FAKE_EOF__'\n{GLAB_MR_EXISTS}\n__FAKE_EOF__\nexit 1;;\n\"mr list\") echo '{GL_MR_URL}'; exit 0;;\nesac\nexit 1"
        ),
    );

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(summary.pr_url.as_deref(), Some(GL_MR_URL));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status, hunter::domain::FindingStatus::PrOpen);
    assert_eq!(finding.pr_url.as_deref(), Some(GL_MR_URL));
}

/// A finished fix whose push fails while the forge cannot be reached is
/// the network's failure, not the fix's: nothing is counted, the attempt
/// becomes the checkpoint, so its commits and transcript are kept and the
/// next cycle continues it to publish, and the cycle ends denied for the
/// host (#95). Counted, a lid closed right after the worker finished cost
/// the whole metered run and one step toward a hold. The same failure with
/// the forge reachable is the fix's, and counts.
#[tokio::test]
async fn a_failed_push_counts_only_while_the_forge_is_reachable() {
    let host = support::FORGE_HOST;
    for counted in [false, true] {
        let url = format!("https://{host}/acme/widget");
        let mut f = fixture_on("fix-push-offline", &url, ForgeName::Github).await;
        f.cfg.network_gate = true;
        // The push goes nowhere, and fails at once, without a network.
        let push_url = hunter::forge::forge_for(ForgeName::Github).ssh_url(&url);
        let nowhere = f.cfg.work_root.join("no-such-remote.git");
        git(
            &f.repo_dir,
            &[
                "config",
                &format!("url.{}.insteadOf", nowhere.display()),
                &push_url,
            ],
        );
        let down: &[&str] = if counted { &[] } else { &[host] };
        let finished = ScriptedBackend::new(|tree| {
            std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
            git(tree, &["add", "-A"]);
            git(tree, &["commit", "-m", "fix: the real bug"]);
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
            support::done_with_transcript(tree)
        })
        .offline(down);

        let summary = fix(&f, &finished).await;

        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.status, hunter::domain::FindingStatus::Queued);
        if counted {
            assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
            assert_eq!(
                fix_streak(&f).await.streak,
                1,
                "the forge answered: counted"
            );
            continue;
        }
        assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
        assert!(summary.retry_at.is_some(), "{summary:?}");
        assert_eq!(fix_streak(&f).await.streak, 0);
        let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
            hunter::scheduler::pick_next(&f.store, &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("the unpublished attempt is continued");
        };
        assert_eq!(Some(plan.predecessor_id), summary.job_id);
        assert_eq!(
            git(&plan.workspace.tree, &["log", "-1", "--format=%s"]).trim(),
            "fix: the real bug",
            "its commits are kept"
        );
    }
}

/// A backend that cannot start the worker while the provider is
/// unreachable is the network's failure, not the fix's: nothing is
/// counted, the job is closed as never started, the checkpoint the fix was
/// continuing stays, and the cycle ends denied for the host, as the
/// network gate's own denial does (#95).
#[tokio::test]
async fn a_runner_error_while_the_provider_is_unreachable_counts_nothing() {
    let (mut f, _blocked, summary, _) = held_fixture().await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let forge = support::FORGE_HOST;
    let provider = support::PROVIDER_HOST;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE repos SET url = ?1")
        .bind(format!("https://{forge}/acme/widget"))
        .execute(&pool)
        .await
        .unwrap();
    f.cfg.network_gate = true;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the continued fix resumes");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let failing = ScriptedBackend::failing()
        .needing(&[provider])
        .offline(&[provider]);

    let denied = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &failing, Some(&plan))
        .await
        .expect("the network's failure ends the cycle denied, not failed");

    assert_eq!(denied.unreachable.as_deref(), Some(provider));
    assert_eq!(
        denied.finding_id,
        Some(f.fid),
        "the denial names its finding"
    );
    assert!(denied.retry_at.is_some(), "{denied:?}");
    assert_eq!(fix_streak(&f).await.streak, 0);
    assert_eq!(held(&f).await, None, "nothing counted, nothing held");
    assert_eq!(
        f.store
            .checkpoint_job(f.fid, FindingJobKind::Fix)
            .await
            .unwrap()
            .map(|j| j.id),
        summary.job_id
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status.as_str(), "queued");
}

/// A one-shot budget override survives a hold on the worker's report, as
/// for every kind: the hold concluded nothing, and the operator's
/// `continue` is the attempt the override was bought for.
#[tokio::test]
async fn a_blocked_fix_keeps_its_one_shot_override() {
    let f = fixture("fix-blocked-override").await;
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let blocked = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "needs a device").unwrap();
        support::done()
    });

    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &blocked, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("held"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        finding.budget_override,
        Some(hunter::domain::BudgetOverride::Once)
    );
}
