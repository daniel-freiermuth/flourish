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
use hunter::scheduler::{Attempt, Resumed};
use hunter::store::{FindingInsert, WorkStreak};
use support::{GitRepo, ScriptedBackend, TempDir, fresh_store, git};

const REPO_URL: &str = "https://github.com/acme/widget";
/// Matches the slug `run_fix` derives from the finding summary below and
/// its id, 2 behind the fixture's bystander.
const BRANCH: &str = "fix/a-real-bug-2";

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

    // Filed first and left `new`, so the finding under fix is not id 1: a
    // write aimed at the wrong finding id lands on this one, not on the
    // finding the test then reads back.
    store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "fp-bystander".to_owned(),
                file: "src/other.rs".to_owned(),
                severity: hunter::domain::Severity::Low,
                confidence: 0.5,
                summary: "a bystander".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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

    let summary =
        hunter::scheduler::run_cycle(&mut support::jobs(&f.store), &f.cfg, &backend, None).await;

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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never,
        )
        .await
        .unwrap();
        assert!(
            summary
                .failure
                .as_deref()
                .is_some_and(|r| r.starts_with("workspace not created")),
            "{summary:?}"
        );
        assert_eq!(summary.finding_id, Some(f.fid), "{summary:?}");
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
    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &never,
    )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &worker,
        )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &capped,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"));
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a cap suspension must resume");
    };
    let stale = Resumed::load(&f.store, &plan).await.unwrap();
    f.store
        .set_finding_verdict(
            f.fid,
            hunter::domain::FindingStatus::Rejected,
            "not worth it",
        )
        .await
        .unwrap();

    let never = ScriptedBackend::new(|_| panic!("a rejected finding must not run"));
    let result = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&stale),
        &never,
    )
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
    let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &blocked,
    )
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
    let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
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
    hunter::workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume, not start cold");
    };
    assert_eq!(plan.predecessor_id(), summary.job_id.unwrap());
    assert_eq!(
        plan.after_hold(),
        Some(&(HoldReason::Prerequisite, expected_reason.clone()))
    );
    let denied = ScriptedBackend::noop().denying_above(0);
    let result = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &denied,
    )
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
    let failed = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &provider_failure,
    )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };

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
        hunter::scheduler::run_fix(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Resume(&Resumed::load(&store, &plan).await.unwrap()),
            &crashing,
        )
        .await
    })
    .await;
    assert!(crashed.unwrap_err().is_panic());

    // The restart: a new process, so a new store handle, opened under the
    // daemon's lock with its own startup token.
    let lock = hunter::recovery::DaemonLock::acquire(&f.cfg.work_root, &f.db).unwrap();
    let (_restarted, token) = hunter::store::Store::connect_exclusive(lock).await.unwrap();
    hunter::recovery::reconcile_and_log(token, &f.cfg.work_root)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the interrupted attempt must resume");
    };
    assert_eq!(plan.after_hold(), None, "an ordinary resume");
    let provider_failure = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.exit_code = Some(1);
        result.tokens_new = 0;
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let failed = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &provider_failure,
    )
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
            hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("continued work must resume");
        };
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
            &worker,
        )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &empty,
        )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued held work must resume");
    };
    let worker = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &worker,
    )
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
        hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &unfinished,
        )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &capped,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"));
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a cap suspension must resume");
    };
    let resumed = ScriptedBackend::staged(|_, session| {
        let mut result = support::done();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &resumed,
    )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };
    let jobs_before = f.store.jobs_by_finding(f.fid).await.unwrap().len();
    std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
    let never = ScriptedBackend::new(|_| panic!("must not run beside a stale report"));
    let err = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &never,
    )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the checkpoint must still resume");
    };
    assert_eq!(again.predecessor_id(), plan.predecessor_id());
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

    hunter::workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
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
    hunter::workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
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
        let plan = match hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
        {
            Some(hunter::scheduler::Candidate::Resume { plan, .. }) => Some(*plan),
            _ => None,
        };
        assert_eq!(
            plan.as_ref()
                .map(hunter::scheduler::ResumePlan::predecessor_id),
            summary.job_id,
            "the continued checkpoint: attempt {attempt}"
        );
        let err = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Resume(
                &Resumed::load(&f.store, plan.as_ref().unwrap())
                    .await
                    .unwrap(),
            ),
            &failing,
        )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &worker,
        )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &backend,
        )
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
    let err = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &worker,
    )
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("continued work must resume");
    };
    let picked = Resumed::load(&f.store, &plan).await.unwrap();
    std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Rejected)
        .await
        .unwrap();
    let never = ScriptedBackend::new(|_| panic!("a rejected finding must not run"));
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&picked),
        &never,
    )
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
    let err = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &worker,
    )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &worker,
        )
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
        let summary = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &ScriptedBackend::noop(),
        )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &never,
    )
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
    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &worker,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("requeued"));
    assert_eq!(summary.failure.as_deref(), Some("no commits"));
    assert!(!bins.called_with("gh", "create"), "{:?}", bins.calls());
}

/// The admin lane fetches the clone while a worker runs, so the default
/// branch's remote-tracking ref can move under it. Here upstream rewrites
/// its default branch mid-run and the clone fetches it: the worker made no
/// commits, so nothing ships -- not the dropped upstream commits, which
/// `origin/<default>..HEAD` would now name as the worker's.
#[tokio::test]
async fn a_fix_ships_nothing_when_its_default_branch_was_rewritten_meanwhile() {
    let bins = support::FakeBins::acquire("fix-default-rewritten");
    bins.ok("gh", "https://github.com/acme/widget/pull/43");
    let f = fixture("fix-default-rewritten").await;
    route_push(&f, None);
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let origin = origin.trim().to_owned();
    let clone = f.repo_dir.clone();
    let upstream = TempDir::new("fix-default-rewritten-upstream");
    let checkout = upstream.join("checkout");
    let upstream_dir = upstream.path().to_owned();
    let worker = ScriptedBackend::new(move |tree| {
        let co = checkout.to_string_lossy();
        git(&upstream_dir, &["clone", "-q", &origin, &co]);
        git(&checkout, &["checkout", "-q", "--orphan", "rewritten"]);
        git(&checkout, &["commit", "-qm", "history rewritten upstream"]);
        git(&checkout, &["push", "-q", "--force", "origin", "HEAD:main"]);
        git(&clone, &["fetch", "-q", "origin"]);
        std::fs::write(tree.join("PR-DESCRIPTION.md"), "nothing changed").unwrap();
        support::done()
    });

    let summary = fix(&f, &worker).await;

    assert_eq!(
        summary.failure.as_deref(),
        Some("no commits"),
        "{summary:?}"
    );
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
    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        backend,
    )
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
    support::scope(&f.store)
        .store()
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
        support::scope(&f.store)
            .store()
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
            hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("the unpublished attempt is continued");
        };
        assert_eq!(Some(plan.predecessor_id()), summary.job_id);
        assert_eq!(
            git(&plan.workspace().tree, &["log", "-1", "--format=%s"]).trim(),
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
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the continued fix resumes");
    };
    let failing = ScriptedBackend::failing()
        .needing(&[provider])
        .offline(&[provider]);

    let denied = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &failing,
    )
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

    let summary = hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &blocked,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("held"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        finding.budget_override,
        Some(hunter::domain::BudgetOverride::Once)
    );
}

/// The admin lane's Renovate pass can rewrite a `queued` dep update's
/// target after the job lane picked it and before the fix claims it --
/// the window holds the budget gate. The worker is asked for the target
/// the claimed row names, not the one the pick read: built from the
/// pick, the fix would ship the old upgrade under a finding that names
/// the new one, and the next scan would find the dependency still behind.
#[tokio::test]
async fn a_fix_works_from_the_row_it_claimed_not_the_row_it_picked() {
    let f = fixture("fix-claimed-row").await;
    let dep = |latest: &str| FindingInsert {
        fingerprint: "fp-dep-serde".to_owned(),
        file: "Cargo.toml".to_owned(),
        severity: hunter::domain::Severity::Low,
        confidence: 0.9,
        summary: format!("serde 1.0.0 -> {latest}"),
        ecosystem: Some("cargo".to_owned()),
        package: Some("serde".to_owned()),
        current_version: Some("1.0.0".to_owned()),
        latest_version: Some(latest.to_owned()),
        update_type: Some("minor".to_owned()),
        ..Default::default()
    };
    let repo_id = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
    let (fid, _) = f
        .store
        .upsert_finding(repo_id, &dep("1.5.0"), "dep_update", None)
        .await
        .unwrap();
    f.store
        .set_finding_status(fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    std::fs::write(
        f.cfg.root.join("playbooks").join("apply_improvement.md"),
        "apply {{FINDING_JSON}}\n",
    )
    .unwrap();
    let picked = f.store.get_finding(fid).await.unwrap().unwrap();
    let store = f.store.clone();
    let worker = ScriptedBackend::noop().on_decide(move || {
        let store = store.clone();
        async move {
            assert!(store.refresh_dep_update(fid, &dep("2.0.0")).await.unwrap());
        }
    });

    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&picked),
        &worker,
    )
    .await
    .unwrap();

    let runs = worker.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].prompt.contains("2.0.0"), "{}", runs[0].prompt);
    assert!(!runs[0].prompt.contains("1.5.0"), "{}", runs[0].prompt);
}

// ---------------------------------------------------------------------------
// The pre-PR review (WORK-MODEL.md §5.7)
// ---------------------------------------------------------------------------

/// A review with one blocking issue and one that is not.
/// The reviewer lists the non-blocking one first: blocking ones go first.
const BLOCKING: &str = r#"[
  {"kind": "introduced", "blocking": false, "file": "src/lib.rs", "line": 9,
   "severity": "low", "confidence": 0.5, "summary": "a minor edge case",
   "detail": "parse_a accepts a trailing comma"},
  {"kind": "incomplete", "blocking": true, "file": "src/lib.rs", "line": 7,
   "severity": "high", "confidence": 0.9,
   "summary": "the sibling parser still breaks",
   "detail": "parse_b has the same off-by-one"}
]"#;

/// A review with issues, none of them blocking.
const NOT_BLOCKING: &str = r#"[
  {"kind": "weak-proof", "blocking": false, "file": "tests/lib.rs", "line": 3,
   "severity": "low", "confidence": 0.7, "summary": "the test misses the empty input",
   "detail": "parse(\"\") is never exercised"}
]"#;

/// A review with no issue at all.
const CLEAN: &str = "[]";

/// A review whose only issue is outside the change's scope.
const FOLLOW_UP_ONLY: &str = r#"[
  {"kind": "follow-up", "blocking": false, "file": "src/feed.rs", "line": 12,
   "severity": "medium", "confidence": 0.8,
   "summary": "the other feeds parse dates the same broken way",
   "detail": "feed_b::parse_date has its own copy"}
]"#;

/// A review with an issue to answer and one follow-up.
const ANSWER_AND_FOLLOW_UP: &str = r#"[
  {"kind": "follow-up", "blocking": true, "file": "src/feed.rs", "line": 12,
   "severity": "high", "confidence": 0.8,
   "summary": "the other feeds parse dates the same broken way",
   "detail": "first wording"},
  {"kind": "incomplete", "blocking": true, "file": "src/lib.rs", "line": 7,
   "severity": "high", "confidence": 0.9,
   "summary": "the sibling parser still breaks",
   "detail": "parse_b has the same off-by-one"}
]"#;

/// The first round's follow-up again, reworded, and a new one.
const MORE_FOLLOW_UPS: &str = r#"[
  {"kind": "follow-up", "blocking": false, "file": "src/feed.rs", "line": 12,
   "severity": "medium", "confidence": 0.8,
   "summary": "the other feeds parse dates the same broken way",
   "detail": "latest wording"},
  {"kind": "follow-up", "blocking": false, "file": "src/cache.rs", "line": 40,
   "severity": "low", "confidence": 0.7,
   "summary": "the cache never expires stale entries",
   "detail": "no TTL anywhere"}
]"#;

/// What one run of a reviewed fix does, in call order.
#[derive(Clone, Copy)]
enum Act {
    /// The fixer: commits the fix, describes it, and leaves a transcript.
    Fix,
    /// The fixer commits the fix and describes it, then ends `.0`.
    FixThen(Ending),
    /// The fixer continuing its transcript, with nothing left to do.
    Continue,
    /// The fixer continuing its transcript and committing more work.
    FixMore,
    /// The fixer continuing its transcript, then ending `.0` at once.
    ContinueThen(Ending),
    /// A reviewer writing `.0` as its review.
    Review(&'static str),
    /// A reviewer that writes `.0` and is then killed at its cap, having
    /// spent all of it.
    ReviewKilled(&'static str),
    /// A reviewer that writes `.0`, then exits non-zero having spent 1000.
    ReviewCrashed(&'static str),
    /// A reviewer that commits in the tree, then writes a clean review.
    CommittingReview,
    /// A reviewer that commits in the tree and leaves a test artifact
    /// behind, having written `.0` as its review.
    TamperingReview(&'static str),
    /// A reviewer that leaves the tree detached at the commit it was
    /// shown, then writes a clean review.
    DetachingReview,
    /// A reviewer that commits everything in the tree, the fixer's
    /// untracked description included (`git add -A`), then writes a clean
    /// review.
    SweepingReview,
    /// The fixer answering a review: one more fix commit and a Review
    /// section in the description.
    Answer,
    /// The fixer answering a review with report `.0` holding `.1`, ending
    /// as `.2` when given.
    Report(&'static str, &'static str, Option<Ending>),
    /// The fixer cap-killed mid-answer, after committing part of it.
    AnswerSuspended,
    /// The fixer's answer exits non-zero before doing any work: a provider
    /// error, or no network.
    AnswerFails,
    /// The fixer as [`Act::Fix`], then leaves `HEAD` detached at its commit.
    FixDetached,
    /// A reviewer that exits non-zero before its first call: no provider.
    ReviewUnreached,
    /// A reviewer that leaves an untracked file git lists quoted, which
    /// undoing its changes does not remove, having written `.0`.
    LitteringReview(&'static str),
    /// The fixer resumed to curate its reviewed history, as `.0` says.
    Curate(Curation),
}

/// How a scripted fixer run that does not finish ends.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// Killed at the cap after metered work: suspended.
    Capped,
    /// Exited non-zero before any metered work: failed.
    Failed,
}

/// `result` ended as `ending`.
fn ended(mut result: hunter::types::RunResult, ending: Ending) -> hunter::types::RunResult {
    match ending {
        Ending::Capped => {
            result.exit_code = None;
            result.killed_reason = Some("cap".to_owned());
        }
        Ending::Failed => {
            result.exit_code = Some(1);
            result.tokens_new = 0;
            result.calls = 0;
        }
    }
    result
}

/// The file a stub reviewer prompt names for its issues
/// (`review {{BASE}} into {{OUT_PATH}}`).
fn out_path(prompt: &str) -> std::path::PathBuf {
    let (_, out) = prompt
        .trim()
        .split_once(" into ")
        .expect("a reviewer prompt");
    std::path::PathBuf::from(out)
}

/// A worker run that wrote report `name` holding `text`, ending as `result`
/// or, given one, as `ending`.
fn report(
    tree: &std::path::Path,
    name: &str,
    text: &str,
    ending: Option<Ending>,
    result: hunter::types::RunResult,
) -> hunter::types::RunResult {
    std::fs::write(tree.join(name), text).unwrap();
    match ending {
        Some(ending) => ended(result, ending),
        None => result,
    }
}

/// How a scripted curation goes.
#[derive(Clone, Copy, Debug)]
enum Curation {
    /// Squashes the branch into one commit of the same tree.
    Squash,
    /// Squashes it, and slips in a change of its own.
    ChangeTree,
    /// Starts squashing and is killed at its cap half way.
    Killed,
    /// Leaves a rebase in progress, and the daemon dies with it.
    DaemonDies,
}

/// A curation run `how`, from the stub prompt `curate <base> <reviewed>`.
fn curate_as(
    tree: &std::path::Path,
    prompt: &str,
    how: Curation,
    result: hunter::types::RunResult,
) -> hunter::types::RunResult {
    let base = prompt.split_whitespace().nth(1).expect("a curation prompt");
    if !matches!(how, Curation::DaemonDies) {
        git(tree, &["reset", "--soft", base]);
    }
    match how {
        Curation::Squash => {
            git(tree, &["commit", "-m", "fix: the real bug, curated"]);
            result
        }
        Curation::ChangeTree => {
            std::fs::write(tree.join("EXTRA.md"), "more\n").unwrap();
            git(tree, &["add", "EXTRA.md"]);
            git(tree, &["commit", "-m", "fix: the real bug, curated"]);
            result
        }
        Curation::Killed => ended(result, Ending::Capped),
        Curation::DaemonDies => {
            // `--exec false` stops the rebase after its first pick.
            let stopped = std::process::Command::new("git")
                .args([
                    "-C",
                    &tree.to_string_lossy(),
                    "rebase",
                    "--exec",
                    "false",
                    base,
                ])
                .output()
                .unwrap();
            assert!(
                !stopped.status.success(),
                "the rebase must be left in progress"
            );
            panic!("the daemon died mid-curation");
        }
    }
}

/// A backend that runs `acts`, one per run, against the tree.
fn reviewed_worker(acts: Vec<Act>) -> ScriptedBackend {
    let next = std::sync::atomic::AtomicUsize::new(0);
    ScriptedBackend::seeing(move |run| {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let tree = run.tree.as_path();
        let commit = |name: &str, message: &str| {
            std::fs::write(tree.join(name), format!("{message}\n")).unwrap();
            git(tree, &["add", name]);
            git(tree, &["commit", "-m", message]);
        };
        let fix = || {
            commit("FIX.md", "fix: the real bug");
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
            support::done_with_transcript(tree)
        };
        let continued = || {
            let mut result = support::done();
            result.session_file = run
                .resume_from
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned());
            result
        };
        match acts[i] {
            Act::Fix => fix(),
            Act::FixThen(ending) => ended(fix(), ending),
            Act::Continue => continued(),
            Act::FixMore => {
                commit("MORE.md", "fix: more of it");
                continued()
            }
            Act::ContinueThen(ending) => ended(continued(), ending),
            Act::Review(review) => {
                std::fs::write(out_path(&run.prompt), review).unwrap();
                support::done()
            }
            Act::ReviewKilled(review) => {
                std::fs::write(out_path(&run.prompt), review).unwrap();
                let mut result = ended(support::done(), Ending::Capped);
                result.tokens_new = run.cap_tokens.expect("a cap to be killed at");
                result
            }
            Act::ReviewCrashed(review) => {
                std::fs::write(out_path(&run.prompt), review).unwrap();
                let mut result = support::done();
                result.exit_code = Some(1);
                result
            }
            Act::TamperingReview(review) => {
                commit("REVIEWER.md", "review: a commit of the reviewer's");
                std::fs::write(tree.join("artifact.log"), "test output\n").unwrap();
                std::fs::write(out_path(&run.prompt), review).unwrap();
                support::done()
            }
            Act::SweepingReview => {
                git(tree, &["add", "-A"]);
                git(tree, &["commit", "-m", "review: everything"]);
                std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
                support::done()
            }
            Act::DetachingReview => {
                git(tree, &["checkout", "--detach"]);
                std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
                support::done()
            }
            Act::CommittingReview => {
                commit("REVIEWER.md", "review: a commit of the reviewer's");
                std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
                support::done()
            }
            Act::Answer => {
                commit(&format!("ANSWER-{i}.md"), "fix: the sibling parser too");
                let description = tree.join("PR-DESCRIPTION.md");
                let body = std::fs::read_to_string(&description).unwrap();
                std::fs::write(&description, format!("{body}\n\n## Review\naddressed")).unwrap();
                continued()
            }
            Act::Report(name, text, end) => report(tree, name, text, end, continued()),
            Act::AnswerFails => ended(continued(), Ending::Failed),
            Act::FixDetached => {
                let result = fix();
                git(tree, &["checkout", "--detach"]);
                result
            }
            Act::ReviewUnreached => ended(support::done(), Ending::Failed),
            Act::Curate(how) => curate_as(tree, &run.prompt, how, continued()),
            Act::LitteringReview(review) => {
                std::fs::write(tree.join("scratch \"notes\".txt"), "scratch\n").unwrap();
                std::fs::write(out_path(&run.prompt), review).unwrap();
                support::done()
            }
            Act::AnswerSuspended => {
                commit("HALF.md", "fix: half of the answer");
                ended(continued(), Ending::Capped)
            }
        }
    })
}

/// Turn the review on for `rounds` rounds, with stub reviewer playbooks.
fn review_on(f: &mut Fixture, rounds: u32) {
    f.cfg.review_max_rounds = rounds;
    let playbooks = f.cfg.root.join("playbooks");
    std::fs::write(
        playbooks.join("review.md"),
        "review {{BASE}} into {{OUT_PATH}}\n",
    )
    .unwrap();
    std::fs::write(playbooks.join("review_followup.md"), "answer {{ISSUES}}\n").unwrap();
    std::fs::write(
        playbooks.join("curate.md"),
        "curate {{BASE}} {{REVIEWED}}\n",
    )
    .unwrap();
    // Off unless a test is about it: a scripted worker answers every run.
    f.cfg.review_curate = false;
}

/// [`fixture`] with the review on for `rounds` rounds and the push routed
/// to the bare origin.
async fn reviewed_fixture(label: &str, rounds: u32) -> Fixture {
    let mut f = fixture(label).await;
    review_on(&mut f, rounds);
    route_push(&f, None);
    f
}

/// The subject of the commit the fix branch has on origin.
fn shipped_subject(f: &Fixture) -> String {
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    git(
        std::path::Path::new(origin.trim()),
        &["log", "-1", "--format=%s", BRANCH],
    )
    .trim()
    .to_owned()
}

/// The body of the one PR the fix opened.
fn pr_body(bins: &support::FakeBins) -> String {
    let creates = bins.calls_to("gh");
    assert_eq!(creates.len(), 1, "{creates:?}");
    let call = &creates[0];
    let i = call.iter().position(|a| a == "--body").unwrap();
    call[i + 1].clone()
}

/// The title of the one PR the fix opened.
fn pr_title(bins: &support::FakeBins) -> String {
    let creates = bins.calls_to("gh");
    let call = &creates[0];
    let i = call.iter().position(|a| a == "--title").unwrap();
    call[i + 1].clone()
}

/// The reviewer session directory of `job`'s round `round`, beside the
/// fixer's session directory `fixer`.
fn review_dir(fixer: &std::path::Path, job: Option<i64>, round: u32) -> std::path::PathBuf {
    fixer
        .parent()
        .unwrap()
        .join(format!("review-{}-{round}", job.unwrap()))
}

/// The pending resume of the fixture's fix.
async fn pending_resume(f: &Fixture) -> hunter::scheduler::ResumePlan {
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the fix must be resumable");
    };
    *plan
}

/// Run the fixture's fix as a resume of `plan`.
async fn resume(
    f: &Fixture,
    plan: &hunter::scheduler::ResumePlan,
    backend: &ScriptedBackend,
) -> hunter::scheduler::CycleSummary {
    hunter::scheduler::run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, plan).await.unwrap()),
        backend,
    )
    .await
    .unwrap()
}

/// A clean review ships the fix as it stands. The reviewer is a fresh
/// session in a session directory of its own, so its transcript is
/// neither a continuation of the fixer's nor metered as part of it.
#[tokio::test]
async fn a_clean_review_ships_and_the_reviewer_ran_fresh_in_its_own_session() {
    let bins = support::FakeBins::acquire("fix-review-clean");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-clean", 3).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 2, "one fixer run, one review");
    let (fixer, reviewer) = (&runs[0], &runs[1]);
    assert_eq!(reviewer.resume_from, None, "the reviewer starts fresh");
    assert_eq!(reviewer.tree, fixer.tree, "it reviews the fixer's tree");
    assert_eq!(
        reviewer.session,
        review_dir(&fixer.session, summary.job_id, 1)
    );
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    assert_eq!(pr_body(&bins), "the body", "a clean review adds nothing");
}

/// A blocking issue goes back to the fixer: its own session resumed, with
/// the issue in the prompt. Its answer is reviewed again, and once that
/// review is clean, the answered fix ships.
#[tokio::test]
async fn a_blocking_issue_resumes_the_fixer_and_the_answer_ships_once_clean() {
    let bins = support::FakeBins::acquire("fix-review-answer");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-answer", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, review, answer, review");
    let transcript = runs[0].session.join("session.jsonl");
    let answer = &runs[2];
    assert_eq!(answer.resume_from.as_deref(), Some(transcript.as_path()));
    assert_eq!(answer.session, runs[0].session);
    assert!(
        answer.prompt.contains("the sibling parser still breaks"),
        "{}",
        answer.prompt
    );
    let (blocking, minor) = (
        answer.prompt.find("the sibling parser still breaks"),
        answer.prompt.find("a minor edge case"),
    );
    assert!(
        minor.is_some() && blocking < minor,
        "every issue goes back, blocking first: {}",
        answer.prompt
    );
    assert_eq!(runs[3].resume_from, None);
    assert_eq!(
        runs[3].session,
        review_dir(&runs[0].session, summary.job_id, 2)
    );
    assert_eq!(shipped_subject(&f), "fix: the sibling parser too");
    assert_eq!(
        pr_title(&bins),
        "fix: the real bug",
        "an answer to the review does not retitle the fix"
    );
    assert_eq!(pr_body(&bins), "the body\n\n## Review\naddressed");
    let events = f.store.recent_events(20).await.unwrap();
    let reviews: Vec<_> = events.iter().filter(|e| e.kind == "review").collect();
    assert_eq!(reviews.len(), 2, "{events:?}");
}

/// Non-blocking issues left after the last round ship with the fix, in a
/// section of the PR body that hands them to the human.
#[tokio::test]
async fn non_blocking_issues_left_after_the_last_round_ship_in_the_pr_body() {
    let bins = support::FakeBins::acquire("fix-review-exhausted");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-exhausted", 2).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(NOT_BLOCKING),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 4, "no answer after the last round");
    let body = pr_body(&bins);
    assert!(
        body.starts_with("the body\n\n## Review\naddressed\n\n"),
        "{body}"
    );
    assert!(body.contains("## Unresolved review issues"), "{body}");
    assert!(
        body.contains("- **the test misses the empty input** (`tests/lib.rs:3`)"),
        "{body}"
    );
    assert!(!body.contains("[blocking]"), "{body}");
}

/// A fix whose review stopped with a blocking issue unanswered, at the
/// last of two rounds: held for the operator.
async fn held_by_review(label: &str) -> (Fixture, ScriptedBackend, support::FakeBins) {
    let bins = support::FakeBins::acquire(label);
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture(label, 2).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(ANSWER_AND_FOLLOW_UP),
        Act::Answer,
        Act::Review(BLOCKING),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    (f, worker, bins)
}

/// A blocking issue still open after the last round does not ship: the
/// fix is held for the operator as a prerequisite, its tree and the
/// fixer's transcript kept as the checkpoint, and the hold's report names
/// the review stop and lists the remaining issues, blocking first.
#[tokio::test]
async fn a_blocking_issue_left_by_the_last_round_holds_the_fix() {
    let (f, worker, bins) = held_by_review("fix-review-held").await;

    assert_eq!(worker.runs().len(), 4, "no answer after the last round");
    assert_eq!(bins.calls_to("gh").len(), 0, "no PR");
    let hold = held(&f).await.expect("held");
    assert_eq!(hold.reason, HoldReason::Prerequisite);
    assert!(
        checkpoint(&f).await.is_some(),
        "the attempt is the checkpoint"
    );
    let report = &hold.detail;
    assert!(report.contains("Held by the pre-PR review"), "{report}");
    assert!(
        report.contains("round 2 (the last of 2 rounds)"),
        "{report}"
    );
    assert!(
        report.contains("- [blocking] **the sibling parser still breaks** (`src/lib.rs:7`)"),
        "{report}"
    );
    assert!(
        report.contains("parse_b has the same off-by-one"),
        "{report}"
    );
    assert!(
        report.find("the sibling parser") < report.find("a minor edge case"),
        "blocking first: {report}"
    );
    assert!(!worker.runs()[0].tree.join("BLOCKED.md").exists());
    let events = f.store.recent_events(30).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.contains("held for the operator")),
        "{events:?}"
    );
}

/// The operator's `continue` resumes the fixer's own transcript on the
/// issues the review left, and the review runs again from round 1: a clean
/// review then ships the fix.
#[tokio::test]
async fn a_continued_review_hold_answers_the_issues_and_is_reviewed_again() {
    let (f, worker, bins) = held_by_review("fix-review-continued").await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 2, "the answer, then one review");
    let transcript = worker.runs()[0].session.join("session.jsonl");
    assert_eq!(runs[0].resume_from.as_deref(), Some(transcript.as_path()));
    assert!(
        runs[0].prompt.contains("the sibling parser still breaks"),
        "{}",
        runs[0].prompt
    );
    assert!(
        runs[0]
            .prompt
            .contains("pre-PR review left blocking issues"),
        "{}",
        runs[0].prompt
    );
    assert_eq!(
        runs[1].session,
        review_dir(&runs[0].session, summary.job_id, 1)
    );
    let body = pr_body(&bins);
    assert!(
        body.starts_with("the body\n\n## Review\naddressed\n\n## Review\naddressed\n\n"),
        "{body}"
    );
    assert!(
        body.contains("## Follow-ups from review")
            && body.contains("the other feeds parse dates the same broken way"),
        "follow-ups found before the hold still ship: {body}"
    );
    assert_eq!(
        pr_title(&bins),
        "fix: the real bug",
        "the hold keeps the title the fix had before round 1"
    );
}

/// A reviewer that changes the tree has not reviewed it: the fix ships
/// with a note saying so, and what ships is the commit the fixer left,
/// not the reviewer's.
#[tokio::test]
async fn a_reviewer_that_moves_head_fails_its_review_and_its_commit_does_not_ship() {
    let bins = support::FakeBins::acquire("fix-review-moved");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-moved", 3).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::CommittingReview]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2);
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(
        body.contains("Review round 1 failed: the reviewer changed the worktree"),
        "{body}"
    );
    let title = bins.calls_to("gh")[0]
        .iter()
        .skip_while(|a| *a != "--title")
        .nth(1)
        .cloned();
    assert_eq!(title.as_deref(), Some("fix: the real bug"));
}

/// A reviewer that rewrites or deletes the description, which becomes the
/// PR body, has changed what ships: its round fails like one that
/// commits, and the fixer's description is put back, so the fixer's text
/// ships and a deleted one is no failure of the fix.
#[tokio::test]
async fn a_reviewer_that_touches_the_description_fails_its_review_and_it_is_put_back() {
    for deletes in [false, true] {
        let bins = support::FakeBins::acquire("fix-review-description");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-description", 3).await;
        let next = std::sync::atomic::AtomicUsize::new(0);
        let worker = ScriptedBackend::seeing(move |run| {
            let tree = run.tree.as_path();
            let description = tree.join("PR-DESCRIPTION.md");
            if next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
                git(tree, &["add", "FIX.md"]);
                git(tree, &["commit", "-m", "fix: the real bug"]);
                std::fs::write(&description, "the body").unwrap();
                return support::done_with_transcript(tree);
            }
            if deletes {
                std::fs::remove_file(&description).unwrap();
            } else {
                std::fs::write(&description, "the reviewer's body").unwrap();
            }
            std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
            support::done()
        });

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{deletes}");
        let body = pr_body(&bins);
        assert!(body.starts_with("the body\n\n"), "{deletes}: {body}");
        assert!(
            body.contains("Review round 1 failed: the reviewer changed the worktree"),
            "{deletes}: {body}"
        );
        drop(bins);
    }
}

/// A reviewer cut off after writing blocking issues has still verified
/// each one (the playbook has it rewrite its file after every issue).
/// They go back to the fixer while the job's grant has something left to
/// answer them with; a reviewer killed at its cap spent all of it, so a
/// blocking issue among them holds the fix for the operator instead.
#[tokio::test]
async fn a_cut_off_reviewers_issues_go_back_only_while_the_grant_can_answer_them() {
    let bins = support::FakeBins::acquire("fix-review-crashed");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-crashed", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::ReviewCrashed(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ])
    .granting(Some(10_000));

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, cut-off review, answer, review");
    assert!(
        runs[2].prompt.contains("the sibling parser still breaks"),
        "{}",
        runs[2].prompt
    );
    assert_eq!(shipped_subject(&f), "fix: the sibling parser too");
    drop(bins);

    let bins = support::FakeBins::acquire("fix-review-capped");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-capped", 3).await;
    let worker =
        reviewed_worker(vec![Act::Fix, Act::ReviewKilled(BLOCKING)]).granting(Some(10_000));

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2, "no answer without budget");
    assert_eq!(
        bins.calls_to("gh").len(),
        0,
        "a blocking issue does not ship"
    );
    let report = held(&f).await.expect("held").detail;
    assert!(
        report.contains("(no budget left to answer them)"),
        "{report}"
    );
    assert!(
        report.contains("the sibling parser still breaks"),
        "{report}"
    );
}

/// A review can show the fix is wrong at its root. A fixer that answers
/// with a report concludes as the report says, and nothing ships.
#[tokio::test]
async fn a_report_written_in_answer_to_a_review_concludes_without_a_pr() {
    for (report, contents, outcome) in [
        (
            "NOT-A-BUG.md",
            "Classification: superseded\nfixed by abc1234",
            "superseded",
        ),
        ("BLOCKED.md", "needs the vendor's spec", "held"),
    ] {
        let bins = support::FakeBins::acquire("fix-review-report");
        let f = reviewed_fixture("fix-review-report", 3).await;
        let worker = reviewed_worker(vec![
            Act::Fix,
            Act::Review(BLOCKING),
            Act::Report(report, contents, None),
        ]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{report}");
        assert_eq!(worker.runs().len(), 3, "{report}: no review after it");
        assert!(bins.calls_to("gh").is_empty(), "{report}: no PR");
        drop(bins);
    }
}

/// `review.maxRounds: 0` ships what the fixer left, unreviewed.
#[tokio::test]
async fn no_review_rounds_ships_without_a_reviewer() {
    let bins = support::FakeBins::acquire("fix-review-off");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-off", 0).await;
    let worker = reviewed_worker(vec![Act::Fix]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 1);
    assert_eq!(pr_body(&bins), "the body");
}

/// The job is one grant and one row: every later run is bounded by what
/// the earlier ones left of the grant, and the row records them all.
#[tokio::test]
async fn the_job_records_every_run_and_each_run_gets_what_is_left_of_the_grant() {
    let bins = support::FakeBins::acquire("fix-review-spend");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-spend", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ])
    .granting(Some(10_000));

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let caps: Vec<_> = worker.runs().iter().map(|r| r.cap_tokens).collect();
    assert_eq!(
        caps,
        [Some(10_000), Some(9_000), Some(8_000), Some(7_000)],
        "each run's cap is the grant less what the job spent before it"
    );
    let per_run = support::done();
    assert_eq!(summary.tokens_new, Some(4 * per_run.tokens_new));
    let job = f
        .store
        .list_jobs(10)
        .await
        .unwrap()
        .into_iter()
        .find(|j| Some(j.job.id) == summary.job_id)
        .unwrap()
        .job;
    assert_eq!(job.tokens_new, Some(4 * per_run.tokens_new));
    assert_eq!(job.calls, Some(4 * per_run.calls));
    let transcript = worker.runs()[0].session.join("session.jsonl");
    assert_eq!(
        job.session_file.as_deref(),
        Some(transcript.to_string_lossy().as_ref()),
        "the job's transcript is the fixer's"
    );
}

/// A fixer that did not finish has not been reviewed, so it ships
/// nothing, even when it left commits and a description behind: not a
/// first run that stopped after describing its fix, and not half an
/// answer to a review, however often its resume stops again.
#[tokio::test]
async fn a_fixer_that_did_not_finish_ships_nothing_while_the_review_is_on() {
    for (ending, outcome) in [(Ending::Capped, "suspended"), (Ending::Failed, "requeued")] {
        let bins = support::FakeBins::acquire("fix-review-unfinished");
        // A PR could be opened: only the gate keeps one from it.
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-unfinished", 3).await;
        let worker = reviewed_worker(vec![Act::FixThen(ending)]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{ending:?}");
        assert_eq!(worker.runs().len(), 1, "{ending:?}: no review");
        assert!(bins.calls_to("gh").is_empty(), "{ending:?}: no PR");
        drop(bins);
    }

    let bins = support::FakeBins::acquire("fix-review-half-answer");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-half-answer", 3).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(BLOCKING), Act::AnswerSuspended]);
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    for (ending, outcome) in [(Ending::Capped, "suspended"), (Ending::Failed, "requeued")] {
        let plan = pending_resume(&f).await;
        let again = reviewed_worker(vec![Act::ContinueThen(ending)]);
        let summary = resume(&f, &plan, &again).await;
        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{ending:?}");
        assert_eq!(again.runs().len(), 1, "{ending:?}: no review");
    }
    assert!(bins.calls_to("gh").is_empty(), "half an answer never ships");
}

/// A fixer suspended while answering a review ships nothing: half an
/// answer is not a fix. Its resume finishes the answer, and the review
/// goes on at the next round instead of starting the count over.
#[tokio::test]
async fn a_fixer_suspended_mid_answer_ships_nothing_and_its_resume_reviews_the_next_round() {
    let bins = support::FakeBins::acquire("fix-review-suspended");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-suspended", 2).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(BLOCKING), Act::AnswerSuspended]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert!(bins.calls_to("gh").is_empty(), "nothing ships");
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);
    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 2, "the resume, then round 2 only");
    assert_eq!(
        runs[1].session,
        review_dir(&runs[0].session, summary.job_id, 2)
    );
    assert_eq!(shipped_subject(&f), "fix: half of the answer");
    assert_eq!(pr_body(&bins), "the body");
    assert_eq!(
        pr_title(&bins),
        "fix: the real bug",
        "titled by the fix, across attempts"
    );
}

/// A fix that could not publish offline is continued, and its fixer may
/// commit more before the push is retried. What the review concluded
/// covers the tree it reviewed, not the one that ships now: the new tree
/// is reviewed, and the commit a reviewer moved `HEAD` away from does not
/// drop the newer commits. The PR keeps the title the fix had before its
/// first review and the follow-ups the first loop found.
#[tokio::test]
async fn work_after_an_unpublished_review_is_reviewed_again_and_ships_whole() {
    let bins = support::FakeBins::acquire("fix-review-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-review-offline").await;
    review_on(&mut f, 3);
    f.cfg.network_gate = true;
    // The push goes nowhere while the forge is down.
    let nowhere = f.cfg.work_root.join("no-such-remote.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let host = "github.com";
    let worker =
        reviewed_worker(vec![Act::Fix, Act::TamperingReview(FOLLOW_UP_ONLY)]).offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0);
    let nowhere_key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &nowhere_key]);
    route_push(&f, None);
    f.cfg.network_gate = false;
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::FixMore, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(resumed.runs().len(), 2, "the new work is reviewed");
    assert_eq!(shipped_subject(&f), "fix: more of it");
    let body = pr_body(&bins);
    assert!(body.starts_with("the body\n\n"), "{body}");
    assert!(
        !body.contains("failed"),
        "the earlier failure is moot: {body}"
    );
    assert!(
        body.contains("the other feeds parse dates the same broken way"),
        "the first loop's follow-ups are kept: {body}"
    );
    assert_eq!(
        pr_title(&bins),
        "fix: the real bug",
        "titled by the fix it had before its first review, not the newer commit"
    );
}

/// An already-reviewed fix that could not publish offline ships on its
/// saved verdict when the push is retried, however the retry's fixer run
/// ended: one that failed before its first token changed nothing the
/// review covered, and throwing the fix away for it would count a failure
/// against a finished, reviewed fix.
#[tokio::test]
async fn a_reviewed_fix_ships_on_its_saved_verdict_when_the_retry_run_fails() {
    let bins = support::FakeBins::acquire("fix-review-saved");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-review-saved").await;
    review_on(&mut f, 3);
    f.cfg.network_gate = true;
    let nowhere = f.cfg.work_root.join("no-such-remote.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let host = "github.com";
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]).offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    let nowhere_key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &nowhere_key]);
    route_push(&f, None);
    f.cfg.network_gate = false;
    let plan = pending_resume(&f).await;
    let retry = reviewed_worker(vec![Act::ContinueThen(Ending::Failed)]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(retry.runs().len(), 1, "no second review");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    assert_eq!(pr_body(&bins), "the body");
    assert_eq!(fix_streak(&f).await.streak, 0, "nothing counted");
}

/// Shutdown lets the run a cycle is in finish, and the unit's stop
/// timeout covers one run, so the review starts no further run once the
/// daemon is stopping. The job is suspended on the fixer's transcript,
/// nothing ships, and the resume reviews it.
#[tokio::test]
async fn a_stopping_daemon_starts_no_review_run_and_the_resume_reviews() {
    let bins = support::FakeBins::acquire("fix-review-shutdown");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-shutdown", 3).await;
    let worker = reviewed_worker(vec![Act::Fix]);
    let (_stop, stopping) = tokio::sync::watch::channel(true);
    let mut jobs = support::jobs(&f.store);
    jobs.stop_on(stopping);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    let summary = hunter::scheduler::run_fix(
        &jobs.into_test_scope(),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &worker,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(
        summary.state,
        Some(hunter::domain::JobState::Suspended),
        "{summary:?}"
    );
    assert_eq!(worker.runs().len(), 1, "no reviewer after the stop");
    assert_eq!(bins.calls_to("gh").len(), 0, "nothing ships");
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(resumed.runs().len(), 2, "the resume, then its review");
    assert_eq!(pr_body(&bins), "the body");
}

/// The review loop as a whole has no wall-clock limit: each reviewer and
/// each answer gets a whole `fix.maxWallS` of its own, however long the
/// runs before it took, so a slow review is still answered. One run's
/// limit is what bounds how long a stop waits.
#[tokio::test]
async fn every_review_run_gets_the_full_per_run_wall_limit() {
    let bins = support::FakeBins::acquire("fix-review-wall");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-wall", 3).await;
    f.cfg.fix_max_wall_s = 1;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker = ScriptedBackend::seeing(move |run| {
        let tree = run.tree.as_path();
        match next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => {
                std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
                git(tree, &["add", "FIX.md"]);
                git(tree, &["commit", "-m", "fix: the real bug"]);
                std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
                support::done_with_transcript(tree)
            }
            // A review that outlasts the whole per-run limit.
            1 => {
                std::thread::sleep(std::time::Duration::from_millis(1100));
                std::fs::write(out_path(&run.prompt), BLOCKING).unwrap();
                support::done()
            }
            2 => {
                std::fs::write(tree.join("ANSWER.md"), "answered\n").unwrap();
                git(tree, &["add", "ANSWER.md"]);
                git(tree, &["commit", "-m", "fix: the sibling parser too"]);
                let mut result = support::done();
                result.session_file = run
                    .resume_from
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned());
                result
            }
            _ => {
                std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
                support::done()
            }
        }
    });

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, slow review, answer, review");
    let walls: Vec<_> = runs.iter().map(|r| r.max_wall_s).collect();
    assert_eq!(walls, [1, 1, 1, 1], "each run gets the whole per-run limit");
    assert_eq!(shipped_subject(&f), "fix: the sibling parser too");
}

/// A reviewer that changed the tree (a commit of its own, a test artifact
/// left behind) has its changes undone: the fix branch is checked out
/// again at the commit it was shown and the artifact removed. Blocking
/// issues it had verified and written still stand, as a killed
/// reviewer's do: answered while there are rounds left, else shipped
/// unresolved, saying the reviewer's changes were undone. Its commit never
/// ships.
#[tokio::test]
async fn a_reviewer_that_changed_the_tree_is_undone_and_its_issues_stand() {
    let bins = support::FakeBins::acquire("fix-review-tampered");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-tampered", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::TamperingReview(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, tampering review, answer, review");
    assert!(
        runs[2].prompt.contains("the sibling parser still breaks"),
        "{}",
        runs[2].prompt
    );
    assert!(!runs[0].tree.join("artifact.log").exists());
    assert!(!runs[0].tree.join("REVIEWER.md").exists());
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let shipped = git(
        std::path::Path::new(origin.trim()),
        &["log", "--format=%s", BRANCH],
    );
    assert!(shipped.contains("fix: the sibling parser too"), "{shipped}");
    assert!(!shipped.contains("a commit of the reviewer's"), "{shipped}");
    drop(bins);

    let bins = support::FakeBins::acquire("fix-review-tampered-last");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-tampered-last", 1).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::TamperingReview(NOT_BLOCKING)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(body.contains("## Unresolved review issues"), "{body}");
    assert!(body.contains("the test misses the empty input"), "{body}");
    assert!(
        body.contains("The reviewer of round 1 changed the worktree; its changes were undone"),
        "{body}"
    );
}

/// A reviewer that leaves the tree detached at the very commit it was
/// shown has changed the tree all the same: the branch is checked out
/// again, the round fails, and the fix still ships to its branch.
#[tokio::test]
async fn a_reviewer_that_detaches_head_fails_its_round_and_the_fix_still_ships() {
    let bins = support::FakeBins::acquire("fix-review-detached");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-detached", 3).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::DetachingReview]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(
        body.contains("Review round 1 failed: the reviewer changed the worktree"),
        "{body}"
    );
}

/// Every issue goes back to the fixer, blocking or not, and a round is
/// clean only when the reviewer raises none: a review whose issues are all
/// non-blocking is answered and reviewed again before the fix ships.
#[tokio::test]
async fn a_review_with_only_non_blocking_issues_is_answered_before_shipping() {
    let bins = support::FakeBins::acquire("fix-review-minor");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-minor", 7).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(NOT_BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, review, answer, review");
    assert!(
        runs[2].prompt.contains("the test misses the empty input"),
        "{}",
        runs[2].prompt
    );
    assert_eq!(shipped_subject(&f), "fix: the sibling parser too");
    assert_eq!(pr_body(&bins), "the body\n\n## Review\naddressed");
}

/// By default the review runs up to seven rounds: a reviewer that keeps
/// finding issues is answered six times, and the seventh round's issues
/// ship unresolved.
#[tokio::test]
async fn by_default_a_fix_is_reviewed_up_to_seven_rounds() {
    let bins = support::FakeBins::acquire("fix-review-seven");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-review-seven").await;
    let rounds = Config::load(&f.cfg.root).unwrap().review_max_rounds;
    review_on(&mut f, rounds);
    route_push(&f, None);
    let mut acts = vec![Act::Fix];
    for _ in 0..6 {
        acts.extend([Act::Review(NOT_BLOCKING), Act::Answer]);
    }
    acts.push(Act::Review(NOT_BLOCKING));
    let worker = reviewed_worker(acts);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 14, "the fix, then 7 reviews and 6 answers");
    assert_eq!(
        runs[13].session,
        review_dir(&runs[0].session, summary.job_id, 7)
    );
    let body = pr_body(&bins);
    assert!(
        body.contains("Automated pre-PR review round 7 raised these issues"),
        "{body}"
    );
}

/// A follow-up is a real defect outside the change's scope: reported to
/// the human, never answered. A round whose only issues are follow-ups is
/// clean, and they ship in a PR body section of their own.
#[tokio::test]
async fn a_round_with_only_follow_ups_is_clean_and_ships_them_in_the_pr_body() {
    let bins = support::FakeBins::acquire("fix-review-follow-up");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-follow-up", 7).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(FOLLOW_UP_ONLY)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2, "no answer to a follow-up");
    let body = pr_body(&bins);
    assert!(
        body.starts_with("the body\n\n## Follow-ups from review\n"),
        "{body}"
    );
    assert!(
        body.contains("- **the other feeds parse dates the same broken way** (`src/feed.rs:12`)"),
        "{body}"
    );
    assert!(!body.contains("Unresolved review issues"), "{body}");
}

/// Only the issues in scope go back to the fixer. Follow-ups are kept
/// across rounds, one per summary in its latest wording, and all of them
/// ship in the final PR body.
#[tokio::test]
async fn follow_ups_skip_the_fixer_and_every_rounds_reach_the_pr_body() {
    let bins = support::FakeBins::acquire("fix-review-follow-ups");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-follow-ups", 7).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(ANSWER_AND_FOLLOW_UP),
        Act::Answer,
        Act::Review(MORE_FOLLOW_UPS),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, review, answer, review");
    let answer = &runs[2].prompt;
    assert!(
        answer.contains("the sibling parser still breaks"),
        "{answer}"
    );
    assert!(!answer.contains("the other feeds"), "{answer}");
    let body = pr_body(&bins);
    assert!(body.contains("## Follow-ups from review"), "{body}");
    assert_eq!(
        body.matches("the other feeds parse dates the same broken way")
            .count(),
        1,
        "{body}"
    );
    assert!(body.contains("latest wording"), "{body}");
    assert!(!body.contains("first wording"), "{body}");
    assert!(
        body.contains("the cache never expires stale entries"),
        "{body}"
    );
}

/// A reviewer that commits the fixer's untracked description along with
/// everything else has it removed again by the checkout that undoes its
/// commit; the description is written back after that checkout, so the
/// finished fix still ships with the fixer's text.
#[tokio::test]
async fn a_reviewer_that_commits_the_description_is_undone_and_the_fix_ships() {
    let bins = support::FakeBins::acquire("fix-review-swept");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-swept", 3).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::SweepingReview]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(body.starts_with("the body\n\n"), "{body}");
    assert!(
        body.contains("Review round 1 failed: the reviewer changed the worktree"),
        "{body}"
    );
}

/// When the tree's state cannot be read before a review (here `git
/// status` fails on an unreadable index), nothing can be held against
/// it: the round fails without a reviewer, nothing in the tree is
/// removed, and the fix ships with its description.
#[tokio::test]
async fn an_unreadable_tree_state_fails_the_round_without_touching_the_tree() {
    use std::os::unix::fs::PermissionsExt;
    let bins = support::FakeBins::acquire("fix-review-unreadable");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-unreadable", 3).await;
    let index = |tree: &std::path::Path| {
        let path = git(
            tree,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"],
        );
        std::path::PathBuf::from(path.trim())
    };
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker = ScriptedBackend::seeing(move |run| {
        let tree = run.tree.as_path();
        if next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
            git(tree, &["add", "FIX.md"]);
            git(tree, &["commit", "-m", "fix: the real bug"]);
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
            let index = index(tree);
            std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o000)).unwrap();
            return support::done_with_transcript(tree);
        }
        // A failure that has passed by the time the reviewer is done.
        let index = index(tree);
        std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(out_path(&run.prompt), CLEAN).unwrap();
        support::done()
    });

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        worker.runs().len(),
        1,
        "no reviewer without a state to hold it to"
    );
    let body = pr_body(&bins);
    assert!(body.starts_with("the body\n\n"), "{body}");
    assert!(
        body.contains("the worktree's state could not be read"),
        "{body}"
    );
}

/// A stop the daemon chose costs the work no resume attempt: a fix stopped
/// between its review runs by one restart after another is still resumed,
/// past `MAX_RESUME_ATTEMPTS`, and ships once a review runs clean.
#[tokio::test]
async fn shutdown_stops_cost_the_fix_no_resume_attempt() {
    let bins = support::FakeBins::acquire("fix-review-restarts");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-restarts", 3).await;
    let stopping = |f: &Fixture| {
        let (stop, latch) = tokio::sync::watch::channel(true);
        std::mem::forget(stop);
        let mut jobs = support::jobs(&f.store);
        jobs.stop_on(latch);
        jobs.into_test_scope()
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(
        &stopping(&f),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &reviewed_worker(vec![Act::Fix]),
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    for restart in 1..=5 {
        let plan = pending_resume(&f).await;
        let summary = hunter::scheduler::run_fix(
            &stopping(&f),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
            &reviewed_worker(vec![Act::Continue]),
        )
        .await
        .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{restart}");
    }
    let plan = pending_resume(&f).await;

    let summary = resume(
        &f,
        &plan,
        &reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]),
    )
    .await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(pr_body(&bins), "the body");
}

/// An answer that fails without doing work while the forge is reachable
/// is the fixer's failure, not the network's: a blocking issue still open
/// never ships and holds the fix for the operator, as one the last round
/// leaves does; non-blocking ones ship unresolved.
#[tokio::test]
async fn a_failed_answer_holds_a_blocking_issue_and_ships_only_non_blocking_ones() {
    for (review, outcome) in [(BLOCKING, "held"), (NOT_BLOCKING, "pr_open")] {
        let bins = support::FakeBins::acquire("fix-review-answer-fails");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-answer-fails", 7).await;
        let worker = reviewed_worker(vec![Act::Fix, Act::Review(review), Act::AnswerFails]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{summary:?}");
        assert_eq!(worker.runs().len(), 3, "{outcome}");
        if outcome == "held" {
            assert_eq!(bins.calls_to("gh").len(), 0, "a blocking issue never ships");
            let report = held(&f).await.expect("held").detail;
            assert!(
                report.contains("the fixer could not answer them"),
                "{report}"
            );
            assert!(
                report.contains("the sibling parser still breaks"),
                "{report}"
            );
        } else {
            let body = pr_body(&bins);
            assert!(body.contains("## Unresolved review issues"), "{body}");
            assert!(body.contains("The fixer could not answer them"), "{body}");
        }
        drop(bins);
    }
}

/// An answer that fails without doing work while the forge is unreachable
/// is the network's failure, handled as the fix's other offline failures
/// are: nothing is published or counted, the attempt is kept, and the retry
/// reviews that round again before anything ships.
#[tokio::test]
async fn an_answer_that_fails_offline_is_kept_and_its_round_reviewed_again() {
    let bins = support::FakeBins::acquire("fix-review-answer-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-answer-offline", 7).await;
    f.cfg.network_gate = true;
    let host = "github.com";
    let worker =
        reviewed_worker(vec![Act::Fix, Act::Review(BLOCKING), Act::AnswerFails]).offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0, "nothing ships");
    assert!(held(&f).await.is_none(), "nothing is held");
    assert_eq!(fix_streak(&f).await.streak, 0, "nothing is counted");
    f.cfg.network_gate = false;
    let plan = pending_resume(&f).await;
    let retry = reviewed_worker(vec![
        Act::Continue,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = retry.runs();
    assert_eq!(
        runs.len(),
        4,
        "the resume, the round again, its answer, a review"
    );
    assert_eq!(
        runs[1].session,
        review_dir(&runs[0].session, summary.job_id, 1)
    );
    assert!(runs[2].prompt.contains("the sibling parser still breaks"));
}

/// A reviewer that fails while a host is unreachable, before its first
/// call or cut off mid-review, failed for the network: handled as the
/// fix's other offline failures are. Nothing ships unreviewed; the retry reviews the
/// round again.
#[tokio::test]
async fn a_reviewer_that_fails_offline_is_kept_and_its_round_reviewed_again() {
    // Before its first call, and cut off after doing work.
    for reviewer in [Act::ReviewUnreached, Act::ReviewCrashed(CLEAN)] {
        a_reviewer_fails_offline(reviewer).await;
    }
}

async fn a_reviewer_fails_offline(reviewer: Act) {
    let bins = support::FakeBins::acquire("fix-review-reviewer-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-reviewer-offline", 7).await;
    f.cfg.network_gate = true;
    let host = "api.provider.example";
    let worker = reviewed_worker(vec![Act::Fix, reviewer])
        .needing(&[host])
        .offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0, "nothing ships unreviewed");
    assert_eq!(fix_streak(&f).await.streak, 0, "nothing is counted");
    f.cfg.network_gate = false;
    let plan = pending_resume(&f).await;
    let retry = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = retry.runs();
    assert_eq!(
        runs[1].session,
        review_dir(&runs[0].session, summary.job_id, 1)
    );
    assert_eq!(
        pr_body(&bins),
        "the body",
        "reviewed this time, with no failure note"
    );
}

/// A reviewer whose changes to the tree cannot be undone fails its round,
/// and the fix ships unreviewed with a note, unless the reviewer had
/// written a blocking issue: that never ships, and holds the fix.
#[tokio::test]
async fn a_blocking_issue_from_a_reviewer_that_could_not_be_undone_holds_the_fix() {
    for (review, outcome) in [(BLOCKING, "held"), (NOT_BLOCKING, "pr_open")] {
        let bins = support::FakeBins::acquire("fix-review-littered");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-littered", 7).await;
        let worker = reviewed_worker(vec![Act::Fix, Act::LitteringReview(review)]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{summary:?}");
        if outcome == "held" {
            assert_eq!(bins.calls_to("gh").len(), 0, "a blocking issue never ships");
            let report = held(&f).await.expect("held").detail;
            assert!(report.contains("could not be undone"), "{report}");
            assert!(
                report.contains("the sibling parser still breaks"),
                "{report}"
            );
        } else {
            let body = pr_body(&bins);
            assert!(body.contains("could not be undone"), "{body}");
            assert!(body.contains("the test misses the empty input"), "{body}");
        }
        drop(bins);
    }
}

/// A fixer that leaves `HEAD` detached is reviewed like any other: a
/// detached `HEAD` is a state the reviewer is held to, not a git failure.
#[tokio::test]
async fn a_fixer_on_a_detached_head_is_still_reviewed() {
    let bins = support::FakeBins::acquire("fix-review-fixer-detached");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-fixer-detached", 7).await;
    let worker = reviewed_worker(vec![Act::FixDetached, Act::Review(CLEAN)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2, "the reviewer ran");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    assert_eq!(pr_body(&bins), "the body");
}

/// The operator's `continue` of the fixture's held fix: the review runs
/// from round 1 again, the issue it raises is answered, and the fix ships.
async fn continue_reviews_from_round_1(f: &Fixture) {
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(f).await;
    let resumed = reviewed_worker(vec![
        Act::Continue,
        Act::Review(BLOCKING),
        Act::FixMore,
        Act::Review(CLEAN),
    ]);

    let summary = resume(f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 4, "a full review: issues answered, then clean");
    assert_eq!(
        runs[1].session,
        review_dir(&runs[0].session, summary.job_id, 1)
    );
}

/// A fixer hold during the review is a fresh start on the operator's
/// `continue`, as a review hold is: the review runs from round 1 again,
/// not from the round it stopped at. However the `BLOCKED.md` came about:
/// written by an answer, by an answer cut off at its cap, or by the attempt
/// that finished a suspended answer.
#[tokio::test]
async fn a_fixer_hold_during_the_review_restarts_the_rounds_on_continue() {
    let suspended_then_blocked = vec![
        vec![
            Act::Fix,
            Act::Review(BLOCKING),
            Act::Answer,
            Act::Review(BLOCKING),
            Act::AnswerSuspended,
        ],
        vec![Act::Report("BLOCKED.md", "needs the vendor's spec", None)],
    ];
    for attempts in [
        vec![vec![
            Act::Fix,
            Act::Review(BLOCKING),
            Act::Answer,
            Act::Review(BLOCKING),
            Act::Report("BLOCKED.md", "needs the vendor's spec", None),
        ]],
        vec![vec![
            Act::Fix,
            Act::Review(BLOCKING),
            Act::Answer,
            Act::Review(BLOCKING),
            Act::Report(
                "BLOCKED.md",
                "needs the vendor's spec",
                Some(Ending::Capped),
            ),
        ]],
        suspended_then_blocked,
    ] {
        let bins = support::FakeBins::acquire("fix-review-fixer-blocked");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-fixer-blocked", 3).await;
        let mut summary = fix(&f, &reviewed_worker(attempts[0].clone())).await;
        for later in &attempts[1..] {
            let plan = pending_resume(&f).await;
            summary = resume(&f, &plan, &reviewed_worker(later.clone())).await;
        }
        assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
        continue_reviews_from_round_1(&f).await;
        drop(bins);
    }
}

/// While the review is on, a finished fix ships only through it. When the
/// check that it is ready to review cannot list its commits (git failed),
/// no review runs and nothing is pushed: the attempt fails, even if git
/// answers again by the time a push would have run.
#[tokio::test]
async fn a_fix_the_review_could_not_check_is_not_pushed_unreviewed() {
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout)
        .unwrap()
        .trim()
        .to_owned();
    let bins = support::FakeBins::acquire("fix-review-git-fails");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-git-fails", 7).await;
    let marker = f.cfg.work_root.join("fail-next-log");
    // `git -C <tree> log <range> --oneline` fails once, after the fixer
    // armed it; every other git call is the real one.
    bins.script(
        "git",
        &format!(
            "if [ \"$3\" = log ] && [ \"$5\" = --oneline ] && [ -e '{m}' ]; then \
             rm -f '{m}'; exit 1; fi\nexec '{real_git}' \"$@\"",
            m = marker.display()
        ),
    );
    let armed = marker.clone();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let worker = ScriptedBackend::seeing(move |run| {
        let tree = run.tree.as_path();
        assert_eq!(
            next.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            0,
            "no reviewer"
        );
        std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
        git(tree, &["add", "FIX.md"]);
        git(tree, &["commit", "-m", "fix: the real bug"]);
        std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
        std::fs::write(&armed, "").unwrap();
        support::done_with_transcript(tree)
    });

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    assert!(
        summary
            .failure
            .as_deref()
            .is_some_and(|f| f.contains("could not list")),
        "{summary:?}"
    );
    assert!(
        !bins.called_with("gh", "create"),
        "nothing is pushed unreviewed"
    );
}

/// Each entry of the review file stands on its own. A malformed one (no
/// summary) is dropped without taking the others with it, and one whose
/// `blocking` is not a boolean is taken as blocking: the reviewer's issues
/// still go back to the fixer, and its follow-ups still ship.
#[tokio::test]
async fn a_malformed_review_entry_does_not_discard_the_others() {
    const MIXED: &str = r#"[
      {"kind": "incomplete", "blocking": "true", "file": "src/lib.rs", "line": 7,
       "summary": "the sibling parser still breaks", "detail": "parse_b too"},
      {"kind": "follow-up", "file": "src/feed.rs",
       "summary": "the other feeds parse dates the same broken way"},
      {"title": "no summary here"}
    ]"#;
    let bins = support::FakeBins::acquire("fix-review-malformed");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-malformed", 7).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(MIXED),
        Act::Answer,
        Act::Review(CLEAN),
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, review, answer, review");
    assert!(
        runs[2].prompt.contains("the sibling parser still breaks"),
        "{}",
        runs[2].prompt
    );
    assert!(
        !runs[2].prompt.contains("the other feeds"),
        "{}",
        runs[2].prompt
    );
    let body = pr_body(&bins);
    assert!(!body.contains("failed"), "{body}");
    assert!(
        body.contains("the other feeds parse dates the same broken way"),
        "{body}"
    );
}

/// The fixture's fix as reviewed with one answer, then curated `how`:
/// four review runs, then the curation, with curation on.
async fn curated(label: &str, how: Curation) -> (Fixture, ScriptedBackend, support::FakeBins) {
    let bins = support::FakeBins::acquire(label);
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture(label, 7).await;
    f.cfg.review_curate = true;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(how),
    ]);
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    (f, worker, bins)
}

/// The commits the fix branch has on origin beyond `main`, newest first.
fn shipped_commits(f: &Fixture) -> Vec<String> {
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let range = format!("main..{BRANCH}");
    git(
        std::path::Path::new(origin.trim()),
        &["log", "--format=%s", &range],
    )
    .lines()
    .map(str::to_owned)
    .collect()
}

/// A review that ends in a ship hands the fixer its own session once more
/// to curate the step-by-step history. A rewrite whose tree is exactly the
/// reviewed one ships in place of the original commits, under the title
/// the fix had before its first review.
#[tokio::test]
async fn a_curated_history_of_the_reviewed_tree_ships() {
    let (f, worker, bins) = curated("fix-curate-ok", Curation::Squash).await;

    let runs = worker.runs();
    assert_eq!(runs.len(), 5, "the review, then one curation");
    let transcript = runs[0].session.join("session.jsonl");
    assert_eq!(runs[4].resume_from.as_deref(), Some(transcript.as_path()));
    assert_eq!(shipped_commits(&f), ["fix: the real bug, curated"]);
    assert_eq!(pr_title(&bins), "fix: the real bug");
    let events = f.store.recent_events(30).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("2 commits rewritten into 1")),
        "{events:?}"
    );
}

/// A curation that changes the tree, or does not finish, is thrown away:
/// the branch goes back to the reviewed commit and the original commits
/// ship. It costs the fix nothing else.
#[tokio::test]
async fn a_curation_that_changes_the_tree_or_fails_ships_the_original_commits() {
    for how in [Curation::ChangeTree, Curation::Killed] {
        let (f, _, bins) = curated("fix-curate-rejected", how).await;

        assert_eq!(
            shipped_commits(&f),
            ["fix: the sibling parser too", "fix: the real bug"],
            "{how:?}"
        );
        let events = f.store.recent_events(30).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("curation: rejected")),
            "{how:?}: {events:?}"
        );
        assert_eq!(fix_streak(&f).await.streak, 0, "{how:?}");
        drop(bins);
    }
}

/// Nothing to curate in a single commit, and nothing is curated with
/// `review.curate` off.
#[tokio::test]
async fn curation_is_skipped_for_one_commit_or_when_off() {
    for (curate, acts, runs) in [
        (true, vec![Act::Fix, Act::Review(CLEAN)], 2),
        (
            false,
            vec![
                Act::Fix,
                Act::Review(BLOCKING),
                Act::Answer,
                Act::Review(CLEAN),
            ],
            4,
        ),
    ] {
        let bins = support::FakeBins::acquire("fix-curate-skipped");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let mut f = reviewed_fixture("fix-curate-skipped", 7).await;
        f.cfg.review_curate = curate;
        let worker = reviewed_worker(acts);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{curate}");
        assert_eq!(worker.runs().len(), runs, "{curate}: no curation run");
        drop(bins);
    }
}

/// A curated fix that could not publish offline ships its curated history
/// on the retry without another review or curation: the saved verdict is
/// for the tree as curated.
#[tokio::test]
async fn a_curated_fix_retried_after_an_offline_push_ships_on_its_saved_verdict() {
    let bins = support::FakeBins::acquire("fix-curate-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-curate-offline").await;
    review_on(&mut f, 7);
    f.cfg.review_curate = true;
    f.cfg.network_gate = true;
    let nowhere = f.cfg.work_root.join("no-such-remote.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let host = "github.com";
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(Curation::Squash),
    ])
    .offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    let nowhere_key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &nowhere_key]);
    route_push(&f, None);
    f.cfg.network_gate = false;
    let plan = pending_resume(&f).await;
    let retry = reviewed_worker(vec![Act::Continue]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(retry.runs().len(), 1, "no review, no curation again");
    assert_eq!(shipped_commits(&f), ["fix: the real bug, curated"]);
}

/// A daemon that dies during a curation leaves the branch half rewritten,
/// mid-rebase. The next attempt puts it back to the reviewed commit before
/// anything reads the tree, tells the resumed fixer there is nothing left
/// to do, and ships the original commits on the verdict saved for the
/// reviewed tree, without another review.
#[tokio::test]
async fn a_curation_the_daemon_died_in_is_undone_and_the_reviewed_commits_ship() {
    use futures_util::FutureExt;
    let bins = support::FakeBins::acquire("fix-curate-died");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-curate-died", 7).await;
    f.cfg.review_curate = true;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(Curation::DaemonDies),
    ]);
    let died = std::panic::AssertUnwindSafe(fix(&f, &worker))
        .catch_unwind()
        .await;
    assert!(died.is_err(), "the daemon died");
    let tree = worker.runs()[0].tree.clone();
    assert!(
        git(&tree, &["status"]).contains("rebase in progress"),
        "the curation left a rebase behind"
    );
    // What the next start's settle does with the job it left running.
    support::jobs(&f.store)
        .settle_unsettled(&f.cfg.work_root)
        .await
        .unwrap();
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Continue]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 1, "no new review, no new curation");
    assert!(
        runs[0].prompt.contains("has been undone"),
        "{}",
        runs[0].prompt
    );
    assert_eq!(
        shipped_commits(&f),
        ["fix: the sibling parser too", "fix: the real bug"]
    );
    assert_eq!(pr_body(&bins), "the body\n\n## Review\naddressed");
    let events = f.store.recent_events(40).await.unwrap();
    assert!(
        events.iter().any(|e| e
            .message
            .contains("interrupted by a restart; the reviewed commit restored")),
        "{events:?}"
    );
}
