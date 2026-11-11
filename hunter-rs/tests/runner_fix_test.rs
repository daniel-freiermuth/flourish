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
use hunter::domain::{FindingJobKind, ForgeName, HoldReason, JobKind, JobState, ReleaseAction};
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
    // No test reaches a forge. A push or `ls-remote` to the repo's URL that
    // a test does not route ([`route_push`]) lands on a path that does not
    // exist, failing at once and offline, as a refusing forge would. Keyed
    // on the scheme alone, so a test's own route for the URL is the longer
    // match and wins.
    git(
        &repo_dir,
        &[
            "config",
            &format!("url.{}/.insteadOf", dir.path().join("no-forge").display()),
            "https://",
        ],
    );

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
                REPO_URL,
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
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100,\"cacheWrite\":0}}}\n",
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
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100,\"cacheWrite\":0}}}\n",
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
    // Review is off here, so the failed run's commits ship, and the forge
    // (unrouted in this fixture) refuses the push. The run did not finish,
    // so it is not kept as finished work: a failure below the limit clears
    // the checkpoint (WORK-MODEL.md §6.3).
    assert!(
        failed
            .failure
            .as_deref()
            .is_some_and(|m| m.starts_with("push failed")),
        "{failed:?}"
    );
    assert_eq!(checkpoint(&f).await, None);
    // It counts as a refused publish, not under the forge's text, which
    // differs on every try and would reset the streak each time.
    assert_eq!(
        fix_streak(&f).await.last_failure.as_deref(),
        Some("publish refused")
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
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":500,\"output\":50,\"cacheWrite\":0}}}\n",
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
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100,\"cacheWrite\":0}}}\n",
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

/// A fix held as `gave_up` (its chain went far over its budget) resumes
/// told why it was held, not that it kept failing the same way: nothing
/// failed, the scheduler only stopped continuing it.
#[tokio::test]
async fn a_fix_held_as_gave_up_resumes_told_why() {
    let f = fixture("fix-gave-up-resume").await;
    let unfinished = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("candidate.txt"), "partial fix\n").unwrap();
        git(tree, &["add", "candidate.txt"]);
        git(tree, &["commit", "-m", "partial candidate"]);
        let session = tree.parent().unwrap().join("session/session.jsonl");
        std::fs::write(
            &session,
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100,\"cacheWrite\":0}}}\n",
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
    // The scheduler's over-budget hold, on the attempt that left a
    // transcript: what `hold_over_budget` writes.
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "UPDATE work_holds SET reason = 'gave_up', \
         detail = 'resume chain over budget: 900000 tok across the chain' \
         WHERE released_at IS NULL",
    )
    .execute(&pool)
    .await
    .unwrap();

    let prompt = resumed_hold_prompt(&f).await;

    assert!(
        prompt.contains("give-up ceiling")
            && prompt.contains("Why it was held")
            && prompt.contains("resume chain over budget: 900000 tok"),
        "{prompt}"
    );
    assert!(!prompt.contains("kept failing the same way"), "{prompt}");
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
            "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100,\"cacheWrite\":0}}}\n",
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
        ("simplification", "apply_simplification.md", "simplify"),
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
            REPO_URL,
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

/// Send `run_fix`'s push, which targets the repo's URL as stored, to
/// `target`: the clone's `origin` when `None`, as the shipping tests above
/// do.
fn route_push(f: &Fixture, target: Option<&str>) {
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let target = target.unwrap_or(origin.trim());
    git(
        &f.repo_dir,
        &["config", &format!("url.{target}.insteadOf"), REPO_URL],
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
    assert_eq!(streak.last_failure.as_deref(), Some("publish refused"));
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
    // The push goes to the repo's URL; land it on the local bare origin
    // instead.
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.trim()),
            GL_REPO_URL,
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
        let nowhere = f.cfg.work_root.join("no-such-remote.git");
        git(
            &f.repo_dir,
            &[
                "config",
                &format!("url.{}.insteadOf", nowhere.display()),
                &url,
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
    /// The fixer as [`Act::Fix`], then leaves what its test runs would: an
    /// uncommitted edit to the tracked `FIX.md`, untracked files and an
    /// ignored one ([`leave_untidy`]).
    FixUntidy,
    /// The fixer as [`Act::Fix`], then leaves the untracked `.coverage` its
    /// test run wrote.
    FixCovered,
    /// The fixer as [`Act::Fix`], but commits its description (`git add
    /// -A`), then updates it uncommitted.
    FixTrackedDescription,
    /// The fixer as [`Act::Fix`], then leaves the tracked `FIX.md`
    /// rewritten, as a build rewrites a stale `Cargo.lock`.
    FixRelocked,
    /// The fixer as [`Act::Fix`], then checks out the submodule `lib`
    /// ([`vendor_lib`]) and leaves it dirty, as building in it would: at
    /// its older commit, with an edit to its tracked `a` and an untracked
    /// `out.o`.
    FixSubmoduled,
    /// The fixer as [`Act::Fix`], then marks the submodule `lib` active in
    /// the clone's shared config, as a sibling tree's `submodule init`
    /// does, without checking it out here, and leaves an untracked
    /// `NOTES.md`.
    FixSubmoduleElsewhere,
    /// The fixer as [`Act::Fix`], then leaves `HEAD` on a branch of its
    /// own (`my-fix`) at its commit, and a `.coverage` behind.
    FixOnOwnBranch,
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
    /// A reviewer that finishes without writing any review file.
    ReviewSilent,
    /// A reviewer that writes `.0` and is then killed at its cap, having
    /// spent all of it.
    ReviewKilled(&'static str),
    /// A reviewer that writes `.0`, then exits non-zero having spent 1000.
    ReviewCrashed(&'static str),
    /// A reviewer that commits in the tree, then writes a clean review.
    CommittingReview,
    /// A reviewer that commits in the tree, edits the tracked `FIX.md` and
    /// leaves a test artifact behind, having written `.0` as its review.
    TamperingReview(&'static str),
    /// A reviewer whose test run updates `.coverage` when there is one, and
    /// writes it afresh when `.0`, then writes a clean review.
    CoverageReview(bool),
    /// A reviewer whose build leaves an untracked `out.o` in the submodule
    /// `lib`, and that also moves `lib` to its older commit when `.0`,
    /// having written `.1` as its review.
    SubmoduleReview(bool, &'static str),
    /// A reviewer that edits the tracked `FIX.md` and leaves an untracked
    /// `.coverage`, committing nothing, then writes a clean review.
    EditingReview,
    /// A reviewer whose test run rewrites the tracked `FIX.md` as the
    /// fixer's build did ([`Act::FixRelocked`]), then writes a clean
    /// review.
    RelockingReview,
    /// A reviewer that leaves the tree detached at the commit it was
    /// shown, having written `.0` as its review.
    DetachingReview(&'static str),
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
    /// A reviewer that commits in the tree, leaves a test artifact behind
    /// and keeps the index locked, so undoing its changes cannot check the
    /// fix branch out again, having written `.0`.
    LockingReview(&'static str),
    /// A reviewer that leaves `HEAD` detached at the commit it was shown and
    /// keeps the index locked, so undoing cannot check the fix branch out
    /// again, having written `.0`.
    StrandingReview(&'static str),
    /// A reviewer that leaves `scratch/notes.txt` in a `scratch/` it made
    /// read-only, which undoing cannot delete, having written `.0`.
    ScratchingReview(&'static str),
    /// A reviewer that makes the worktree's index unreadable, so git can
    /// report no status once it is done, having written `.0`.
    UnreadingReview(&'static str),
    /// The fixer as [`Act::Fix`], then leaves `scratch/notes.txt` in a
    /// `scratch/` it made read-only, which cleaning cannot delete.
    FixScratched,
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
    /// Killed at its wall-clock limit after metered work: suspended, as
    /// a cap kill, but not for its grant.
    Wallclock,
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
        Ending::Wallclock => {
            result.exit_code = None;
            result.killed_reason = Some("wallclock".to_owned());
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
    /// Starts squashing and ends `.0` half way.
    Ends(Ending),
    /// Leaves a rebase in progress, and the daemon dies with it.
    DaemonDies,
    /// Leaves a rebase in progress, and is killed at its cap.
    KilledMidRebase,
    /// Squashes it, and also edits the description.
    EditsDescription,
    /// Squashes it, and leaves an untracked file behind.
    LeavesFile,
    /// Squashes it with a change of its own, and keeps the index locked,
    /// so its rewrite cannot be undone.
    LockedChange,
    /// Squashes it with a change of its own, keeps the index locked, and
    /// the daemon dies with it.
    LockedDaemonDies,
}

/// A curation run `how`, from the stub prompt `curate <base> <reviewed>`.
fn curate_as(
    tree: &std::path::Path,
    prompt: &str,
    how: Curation,
    result: hunter::types::RunResult,
) -> hunter::types::RunResult {
    let base = prompt.split_whitespace().nth(1).expect("a curation prompt");
    if !matches!(how, Curation::DaemonDies | Curation::KilledMidRebase) {
        git(tree, &["reset", "--soft", base]);
    }
    match how {
        Curation::Squash => {
            git(tree, &["commit", "-m", "fix: the real bug, curated"]);
            result
        }
        Curation::ChangeTree | Curation::LockedChange | Curation::LockedDaemonDies => {
            std::fs::write(tree.join("EXTRA.md"), "more\n").unwrap();
            git(tree, &["add", "EXTRA.md"]);
            git(tree, &["commit", "-m", "fix: the real bug, curated"]);
            if !matches!(how, Curation::ChangeTree) {
                std::fs::write(index_lock(tree), "").unwrap();
            }
            if matches!(how, Curation::LockedDaemonDies) {
                panic!("the daemon died mid-curation");
            }
            result
        }
        Curation::Killed => ended(result, Ending::Capped),
        Curation::Ends(ending) => ended(result, ending),
        Curation::EditsDescription | Curation::LeavesFile => {
            git(tree, &["commit", "-m", "fix: the real bug, curated"]);
            if matches!(how, Curation::EditsDescription) {
                std::fs::write(tree.join("PR-DESCRIPTION.md"), "the curator's body").unwrap();
            } else {
                std::fs::write(tree.join("curation.log"), "notes\n").unwrap();
            }
            result
        }
        Curation::DaemonDies | Curation::KilledMidRebase => {
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
            if matches!(how, Curation::KilledMidRebase) {
                return ended(result, Ending::Capped);
            }
            panic!("the daemon died mid-curation");
        }
    }
}

/// A reviewer run `act` that touches the tree, if at all, without
/// committing a file of its own. A review it writes is written with the
/// tree's `git status --porcelain --ignored` as the reviewer found it
/// ([`seen`]).
fn review_as(run: &support::Run, act: Act) -> hunter::types::RunResult {
    let tree = run.tree.as_path();
    let found = git(tree, &["status", "--porcelain", "--ignored"]);
    let reviewed = |review: &str| {
        let out = out_path(&run.prompt);
        std::fs::write(out.with_extension("seen"), &found).unwrap();
        if let Ok(description) = std::fs::read(tree.join("PR-DESCRIPTION.md")) {
            std::fs::write(out.with_extension("description"), description).unwrap();
        }
        std::fs::write(out, review).unwrap();
        support::done()
    };
    match act {
        Act::Review(review) => reviewed(review),
        Act::ReviewKilled(review) => {
            let mut result = ended(reviewed(review), Ending::Capped);
            result.tokens_new = run.cap_tokens.expect("a cap to be killed at");
            result
        }
        Act::ReviewCrashed(review) => {
            let mut result = reviewed(review);
            result.exit_code = Some(1);
            result
        }
        Act::SweepingReview => {
            git(tree, &["add", "-A"]);
            git(tree, &["commit", "-m", "review: everything"]);
            reviewed(CLEAN)
        }
        Act::DetachingReview(review) => {
            git(tree, &["checkout", "--detach"]);
            reviewed(review)
        }
        Act::ReviewUnreached => ended(support::done(), Ending::Failed),
        Act::ReviewSilent => support::done(),
        Act::StrandingReview(review) => {
            git(tree, &["checkout", "--detach"]);
            std::fs::write(index_lock(tree), "").unwrap();
            reviewed(review)
        }
        Act::ScratchingReview(review) => {
            leave_scratch(tree);
            reviewed(review)
        }
        Act::UnreadingReview(review) => {
            use std::os::unix::fs::PermissionsExt;
            let index = git(tree, &["rev-parse", "--git-path", "index"]);
            let index = tree.join(index.trim());
            std::fs::set_permissions(index, std::fs::Permissions::from_mode(0o000)).unwrap();
            reviewed(review)
        }
        Act::CoverageReview(creates) => {
            let coverage = tree.join(".coverage");
            if coverage.exists() {
                let had = std::fs::read_to_string(&coverage).unwrap();
                std::fs::write(&coverage, format!("{had}the reviewer's run\n")).unwrap();
            } else if creates {
                std::fs::write(&coverage, "the reviewer's run\n").unwrap();
            }
            reviewed(CLEAN)
        }
        Act::SubmoduleReview(moves, review) => {
            let lib = tree.join(LIB);
            std::fs::write(lib.join("out.o"), "the reviewer's build\n").unwrap();
            if moves {
                git(&lib, &["checkout", "-q", "HEAD~1"]);
            }
            reviewed(review)
        }
        Act::EditingReview => {
            std::fs::write(tree.join("FIX.md"), "the reviewer's edit\n").unwrap();
            std::fs::write(tree.join(".coverage"), "the reviewer's run\n").unwrap();
            reviewed(CLEAN)
        }
        Act::RelockingReview => {
            std::fs::write(tree.join("FIX.md"), RELOCKED).unwrap();
            reviewed(CLEAN)
        }
        _ => unreachable!("not a reviewer that commits nothing of its own"),
    }
}

/// What the reviewer of `run` found `git status --porcelain --ignored` to
/// list.
fn seen(run: &support::Run) -> String {
    std::fs::read_to_string(out_path(&run.prompt).with_extension("seen")).unwrap()
}

/// Make `tree`'s `scratch/` writable, or read-only.
fn scratch_writable(tree: &std::path::Path, writable: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if writable { 0o755 } else { 0o555 };
    std::fs::set_permissions(tree.join("scratch"), std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Leave `scratch/notes.txt` in `tree`, in a `scratch/` made read-only.
fn leave_scratch(tree: &std::path::Path) {
    std::fs::create_dir(tree.join("scratch")).unwrap();
    std::fs::write(tree.join("scratch/notes.txt"), "scratch\n").unwrap();
    scratch_writable(tree, false);
}

/// A backend that runs `acts`, one per run, against the tree.
fn reviewed_worker(acts: Vec<Act>) -> ScriptedBackend {
    let next = std::sync::atomic::AtomicUsize::new(0);
    ScriptedBackend::seeing(move |run| {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        run_act(run, i, acts[i])
    })
}

/// [`reviewed_worker`], with the daemon's stop latch set once run `stop`
/// (zero-based) has ended: the shutdown arrives while that run is in
/// flight.
fn reviewed_worker_stopping(
    acts: Vec<Act>,
    (stop, latch): (usize, tokio::sync::watch::Sender<bool>),
) -> ScriptedBackend {
    let next = std::sync::atomic::AtomicUsize::new(0);
    ScriptedBackend::seeing(move |run| {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let result = run_act(run, i, acts[i]);
        if i == stop {
            latch.send_replace(true);
        }
        result
    })
}

/// Run `act`, the `i`th run of a reviewed fix, against `run`'s tree.
fn run_act(run: &support::Run, i: usize, act: Act) -> hunter::types::RunResult {
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
    match act {
        Act::Fix => fix(),
        act @ (Act::FixUntidy
        | Act::FixCovered
        | Act::FixRelocked
        | Act::FixScratched
        | Act::FixSubmoduled
        | Act::FixSubmoduleElsewhere
        | Act::FixOnOwnBranch
        | Act::FixTrackedDescription) => {
            let result = fix();
            leave_behind(tree, act);
            result
        }
        Act::FixThen(ending) => ended(fix(), ending),
        Act::Continue => continued(),
        Act::FixMore => {
            commit("MORE.md", "fix: more of it");
            continued()
        }
        Act::ContinueThen(ending) => ended(continued(), ending),
        act @ (Act::Review(_)
        | Act::ReviewKilled(_)
        | Act::ReviewCrashed(_)
        | Act::SweepingReview
        | Act::DetachingReview(_)
        | Act::ReviewUnreached
        | Act::ReviewSilent
        | Act::CoverageReview(_)
        | Act::SubmoduleReview(..)
        | Act::EditingReview
        | Act::RelockingReview
        | Act::StrandingReview(_)
        | Act::ScratchingReview(_)
        | Act::UnreadingReview(_)) => review_as(run, act),
        Act::TamperingReview(review) | Act::LockingReview(review) => {
            commit("REVIEWER.md", "review: a commit of the reviewer's");
            std::fs::write(tree.join("artifact.log"), "test output\n").unwrap();
            if matches!(act, Act::TamperingReview(_)) {
                std::fs::write(tree.join("FIX.md"), "the reviewer's edit\n").unwrap();
            }
            if matches!(act, Act::LockingReview(_)) {
                std::fs::write(index_lock(tree), "").unwrap();
            }
            std::fs::write(out_path(&run.prompt), review).unwrap();
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
        Act::Curate(how) => curate_as(tree, &run.prompt, how, continued()),
        Act::AnswerSuspended => {
            commit("HALF.md", "fix: half of the answer");
            ended(continued(), Ending::Capped)
        }
    }
}

/// What a build rewrites the tracked `FIX.md` to, as `cargo test` without
/// `--locked` rewrites a stale `Cargo.lock` ([`Act::FixRelocked`]).
const RELOCKED: &str = "rewritten by the build\n";

/// What the fixer run `act` leaves in `tree` once it committed its fix.
fn leave_behind(tree: &std::path::Path, act: Act) {
    match act {
        Act::FixUntidy => {
            std::fs::write(tree.join("FIX.md"), "touched by the test run\n").unwrap();
            leave_untidy(tree);
        }
        Act::FixCovered => std::fs::write(tree.join(".coverage"), "the fixer's run\n").unwrap(),
        Act::FixRelocked => std::fs::write(tree.join("FIX.md"), RELOCKED).unwrap(),
        Act::FixScratched => leave_scratch(tree),
        Act::FixTrackedDescription => {
            git(tree, &["add", "-A"]);
            git(tree, &["commit", "-m", "docs: describe the fix"]);
            std::fs::write(tree.join("PR-DESCRIPTION.md"), "the updated body").unwrap();
        }
        Act::FixOnOwnBranch => {
            git(tree, &["switch", "-q", "-c", "my-fix"]);
            std::fs::write(tree.join(".coverage"), "the fixer's run\n").unwrap();
        }
        Act::FixSubmoduleElsewhere => {
            let url = git(
                tree,
                &[
                    "config",
                    "-f",
                    ".gitmodules",
                    &format!("submodule.{LIB}.url"),
                ],
            );
            git(
                tree,
                &["config", &format!("submodule.{LIB}.url"), url.trim()],
            );
            git(
                tree,
                &["config", &format!("submodule.{LIB}.active"), "true"],
            );
            // Something to clean, so the clean runs at all.
            std::fs::write(tree.join("NOTES.md"), "the fixer's notes\n").unwrap();
        }
        Act::FixSubmoduled => {
            let init = [
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ];
            git(tree, &init);
            let lib = tree.join(LIB);
            git(&lib, &["checkout", "-q", "HEAD~1"]);
            std::fs::write(lib.join("a"), "the fixer's edit\n").unwrap();
            std::fs::write(lib.join("out.o"), "the fixer's build\n").unwrap();
        }
        _ => unreachable!("not a fixer that leaves something behind"),
    }
}

/// What [`Act::FixUntidy`] leaves in `tree` besides its edit: untracked
/// `NOTES.md`, `fixtures/case.txt` and `out-01.log` to `out-10.log`, and
/// `target/cache.bin`, which the clone's `info/exclude` ignores.
fn leave_untidy(tree: &std::path::Path) {
    std::fs::write(tree.join("NOTES.md"), "the fixer's notes\n").unwrap();
    std::fs::create_dir(tree.join("fixtures")).unwrap();
    std::fs::write(tree.join("fixtures/case.txt"), "the fixer's case\n").unwrap();
    for n in 1..=10 {
        std::fs::write(tree.join(format!("out-{n:02}.log")), "test output\n").unwrap();
    }
    let exclude = git(tree, &["rev-parse", "--git-path", "info/exclude"]);
    let exclude = tree.join(exclude.trim());
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, "/target/\n").unwrap();
    std::fs::create_dir(tree.join("target")).unwrap();
    std::fs::write(tree.join("target/cache.bin"), "build cache\n").unwrap();
}

/// The index lock of the worktree `tree`.
fn index_lock(tree: &std::path::Path) -> std::path::PathBuf {
    tree.join(git(tree, &["rev-parse", "--git-path", "index.lock"]).trim())
}

/// The submodule [`vendor_lib`] puts on the fixture's `main`.
const LIB: &str = "lib";

/// Vendor a repository of two commits, each writing its `a`, as the
/// submodule [`LIB`] of the fixture's `main`, at the newer one, and publish
/// it: every fix tree has it to check out.
fn vendor_lib(f: &Fixture) {
    let lib = f.cfg.work_root.with_file_name("lib-origin");
    std::fs::create_dir(&lib).unwrap();
    git(&lib, &["init", "-q"]);
    for text in ["one", "two"] {
        std::fs::write(lib.join("a"), format!("{text}\n")).unwrap();
        git(&lib, &["add", "a"]);
        git(&lib, &["commit", "-q", "-m", text]);
    }
    let url = lib.to_string_lossy();
    git(
        &f.repo_dir,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &url,
            LIB,
        ],
    );
    git(&f.repo_dir, &["commit", "-q", "-m", "chore: vendor lib"]);
    git(&f.repo_dir, &["push", "-q", "origin", "main"]);
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

/// The pending resume of the fixture's fix after an operator released a
/// hold on its checkpoint: for an attempt kept only to publish, the one
/// continuation whose worker runs (any other publishes without a run).
async fn released_resume(f: &Fixture) -> hunter::scheduler::ResumePlan {
    let held = support::scope(&f.store)
        .store()
        .hold_work(
            f.fid,
            FindingJobKind::Fix,
            HoldReason::Stuck,
            "push failed: HTTP 500",
            checkpoint(f).await,
        )
        .await
        .unwrap();
    assert!(held);
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    pending_resume(f).await
}

/// The fixture's fix is neither continued nor started fresh: it waits.
async fn assert_waits(f: &Fixture) {
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
        "neither continued nor redone before the wait: {picked:?}"
    );
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
    assert!(
        reviews.iter().any(|e| e
            .message
            .ends_with("review round 1: 2 issues (1 blocking); back to the fixer")),
        "{events:?}"
    );
}

/// Non-blocking issues left after the last round hold the fix as a
/// blocking one does: a review still finding something after its last
/// round is not converging, so nothing ships on the hope the rest is
/// harmless.
#[tokio::test]
async fn non_blocking_issues_left_after_the_last_round_hold_the_fix() {
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

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    assert_eq!(worker.runs().len(), 4, "no answer after the last round");
    assert_eq!(bins.calls_to("gh").len(), 0, "no PR");
    let hold = held(&f).await.expect("held");
    assert_eq!(hold.reason, HoldReason::Prerequisite);
    assert!(
        hold.detail.contains("round 2 (the last of 2 rounds)")
            && hold
                .detail
                .contains("- **the test misses the empty input** (`tests/lib.rs:3`)"),
        "{}",
        hold.detail
    );
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
    assert!(
        report.contains("## Follow-ups from review")
            && report.contains("the other feeds parse dates the same broken way"),
        "the report lists the follow-ups so far: {report}"
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
            .contains("pre-PR review left issues unanswered"),
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

/// A review hold's issues go to the fixer once: when the fixer answering
/// them is held on its own report, the next `continue` resumes it on that
/// report, not on the review's issues again.
#[tokio::test]
async fn a_continued_review_hold_sends_its_issues_once() {
    let (f, _worker, _bins) = held_by_review("fix-review-sent-once").await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(&f).await;
    let answering = reviewed_worker(vec![Act::Report(
        "BLOCKED.md",
        "needs the vendor's spec",
        None,
    )]);

    let summary = resume(&f, &plan, &answering).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let prompt = &answering.runs()[0].prompt;
    assert!(
        prompt.contains("the sibling parser still breaks"),
        "{prompt}"
    );
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
    let prompt = &resumed.runs()[0].prompt;
    assert!(
        prompt.contains("needs the vendor's spec")
            && !prompt.contains("the sibling parser still breaks"),
        "{prompt}"
    );
}

/// A `review-held.json` an earlier daemon wrote (`report` and `issues`,
/// no `sent`) is pending only for a `continue` of its own hold, as that
/// daemon read it: left behind once its issues were answered, it does not
/// send them again on a later hold's `continue`.
#[tokio::test]
async fn a_legacy_review_hold_file_sends_its_issues_only_on_its_own_hold() {
    let legacy = |report: &str| {
        serde_json::json!({
            "report": report,
            "issues": [{"title": "a legacy issue", "blocking": true}],
        })
        .to_string()
    };
    let (f, worker, _bins) = held_by_review("fix-review-legacy-own").await;
    let root = worker.runs()[0].tree.parent().unwrap().to_owned();
    let detail = held(&f).await.expect("held").detail;
    std::fs::write(root.join("review-held.json"), legacy(&detail)).unwrap();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(&f).await;
    let answering = reviewed_worker(vec![Act::Report(
        "BLOCKED.md",
        "needs the vendor's spec",
        None,
    )]);

    let summary = resume(&f, &plan, &answering).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let prompt = &answering.runs()[0].prompt;
    assert!(prompt.contains("a legacy issue"), "its own hold: {prompt}");

    // As the earlier daemon left it after the fixer answered: the file
    // stays, naming the review's hold, while a later hold is continued.
    std::fs::write(root.join("review-held.json"), legacy(&detail)).unwrap();
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
    let prompt = &resumed.runs()[0].prompt;
    assert!(
        prompt.contains("needs the vendor's spec") && !prompt.contains("a legacy issue"),
        "{prompt}"
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
/// each one (the playbook has it rewrite its file after every issue):
/// they go back to the fixer.
#[tokio::test]
async fn a_cut_off_reviewers_issues_go_back_to_the_fixer() {
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
}

/// A grant the review spends before it is through suspends the job: no
/// review cut short ships, and none holds the fix. The issues a reviewer
/// killed at its cap wrote are answered by the resume, under a fresh
/// grant, without first resuming the finished fixer for nothing, and the
/// change ships once a review runs clean.
#[tokio::test]
async fn a_spent_grant_suspends_the_review_and_the_resume_answers_its_issues() {
    for (review, summary_of) in [
        (BLOCKING, "the sibling parser still breaks"),
        (NOT_BLOCKING, "the test misses the empty input"),
    ] {
        let bins = support::FakeBins::acquire("fix-review-capped");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-capped", 3).await;
        let worker =
            reviewed_worker(vec![Act::Fix, Act::ReviewKilled(review)]).granting(Some(10_000));

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
        assert_eq!(
            worker.runs().len(),
            2,
            "{summary_of}: no answer without budget"
        );
        assert!(
            bins.calls_to("gh").is_empty(),
            "{summary_of}: nothing ships"
        );
        assert!(held(&f).await.is_none(), "{summary_of}: nothing is held");
        let fixer = worker.runs()[0].session.clone();
        let plan = pending_resume(&f).await;
        let resumed = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

        let summary = resume(&f, &plan, &resumed).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
        let runs = resumed.runs();
        assert_eq!(runs.len(), 2, "{summary_of}: the answer, then a review");
        assert!(
            runs[0].prompt.starts_with("answer") && runs[0].prompt.contains(summary_of),
            "{}",
            runs[0].prompt
        );
        assert!(
            runs[0]
                .resume_from
                .as_ref()
                .is_some_and(|from| from.starts_with(&fixer)),
            "{summary_of}: the answer continues the fixer's transcript: {:?}",
            runs[0].resume_from
        );
        assert!(runs[1].prompt.starts_with("review"), "{}", runs[1].prompt);
        assert_eq!(shipped_subject(&f), "fix: the sibling parser too");
        assert_eq!(pr_body(&bins), "the body\n\n## Review\naddressed");
        drop(bins);
    }
}

/// Only the attempt that continues a review's suspension skips its fixer:
/// an answer that attempt has cut off at its cap is suspended as any
/// fixer's is, and the next resume continues that fixer before the review
/// goes on.
#[tokio::test]
async fn only_the_resume_of_a_review_suspension_skips_the_fixer() {
    let bins = support::FakeBins::acquire("fix-review-pause-once");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-pause-once", 3).await;
    let worker =
        reviewed_worker(vec![Act::Fix, Act::ReviewKilled(BLOCKING)]).granting(Some(10_000));
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    let plan = pending_resume(&f).await;
    let answering = reviewed_worker(vec![Act::AnswerSuspended]);
    let summary = resume(&f, &plan, &answering).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert!(answering.runs()[0].prompt.starts_with("answer"));
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 2, "the fixer continued, then a review");
    assert!(
        !runs[0].prompt.starts_with("answer") && !runs[0].prompt.starts_with("review"),
        "{}",
        runs[0].prompt
    );
    assert!(runs[1].prompt.starts_with("review"), "{}", runs[1].prompt);
}

/// A review suspended mid-round keeps the PR title and the round's
/// follow-ups: the resume answers the owed issue, and what ships is
/// titled by the commit the fixer finished on before round 1, not the
/// answer the attempt resumed on, and lists the follow-up.
#[tokio::test]
async fn a_suspended_review_keeps_its_title_and_follow_ups() {
    let bins = support::FakeBins::acquire("fix-review-suspend-keeps");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-suspend-keeps", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::ReviewKilled(ANSWER_AND_FOLLOW_UP),
    ])
    .granting(Some(10_000));
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(pr_title(&bins), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(
        body.contains("the other feeds parse dates the same broken way"),
        "{body}"
    );
}

/// A curation owed (the grant ran out on the curator, or the daemon died
/// in it) keeps the PR title and the review's follow-ups through the
/// attempt that curates: when that attempt's push fails and the fixer
/// commits more on the released retry, the review runs again from them,
/// and what ships is still titled by the commit the fixer first finished
/// on and lists the follow-up.
#[tokio::test]
async fn an_owed_curation_keeps_the_title_and_follow_ups() {
    use futures_util::FutureExt;
    for (label, curation) in [
        ("fix-curate-owed-keeps-grant", Curation::Killed),
    ] {
        let bins = support::FakeBins::acquire(label);
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let mut f = fixture(label).await;
        review_on(&mut f, 7);
        f.cfg.review_curate = true;
        f.cfg.network_gate = true;
        let nowhere = f.cfg.work_root.join("no-such-remote.git");
        route_push(&f, Some(&nowhere.to_string_lossy()));
        let host = "github.com";
        let worker = reviewed_worker(vec![
            Act::Fix,
            Act::Review(ANSWER_AND_FOLLOW_UP),
            Act::Answer,
            Act::Review(CLEAN),
            Act::Curate(curation),
        ])
        .granting(Some(10_000));
        let first = std::panic::AssertUnwindSafe(fix(&f, &worker))
            .catch_unwind()
            .await;
        if matches!(curation, Curation::DaemonDies) {
            assert!(first.is_err(), "{label}: the daemon died");
            support::jobs(&f.store)
                .settle_unsettled(&f.cfg.work_root)
                .await
                .unwrap();
        } else {
            let summary = first.expect("no panic");
            assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{label}");
        }
        let plan = pending_resume(&f).await;
        let curating = reviewed_worker(vec![Act::Curate(Curation::Squash)]).offline(&[host]);
        let summary = resume(&f, &plan, &curating).await;
        assert_eq!(
            summary.unreachable.as_deref(),
            Some(host),
            "{label}: {summary:?}"
        );
        let key = format!("url.{}.insteadOf", nowhere.display());
        git(&f.repo_dir, &["config", "--unset-all", &key]);
        route_push(&f, None);
        f.cfg.network_gate = false;
        let plan = released_resume(&f).await;
        let retry = reviewed_worker(vec![
            Act::FixMore,
            Act::Review(CLEAN),
            Act::Curate(Curation::Squash),
        ]);

        let summary = resume(&f, &plan, &retry).await;

        assert_eq!(
            summary.outcome.as_deref(),
            Some("pr_open"),
            "{label}: {summary:?}"
        );
        assert_eq!(retry.runs().len(), 3, "{label}: reviewed and curated again");
        assert_eq!(pr_title(&bins), "fix: the real bug", "{label}");
        let body = pr_body(&bins);
        assert!(
            body.contains("the other feeds parse dates the same broken way"),
            "{label}: {body}"
        );
        drop(bins);
    }
}

/// Owed issues get the same grant check as a round's own: when an
/// operator's `continue` runs the fixer first and that run spends the
/// whole grant, the job is suspended again with the issues still owed,
/// neither held nor shipped as unresolved, and the next resume answers
/// them.
#[tokio::test]
async fn owed_issues_with_no_grant_left_are_owed_again() {
    let bins = support::FakeBins::acquire("fix-review-owed-spent");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-owed-spent", 3).await;
    let worker =
        reviewed_worker(vec![Act::Fix, Act::ReviewKilled(BLOCKING)]).granting(Some(10_000));
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    let plan = released_resume(&f).await;
    let continued = reviewed_worker(vec![Act::Continue]).granting(Some(1_000));

    let summary = resume(&f, &plan, &continued).await;

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(continued.runs().len(), 1, "the fixer, then no answer");
    assert!(held(&f).await.is_none(), "nothing is held");
    assert!(bins.calls_to("gh").is_empty(), "nothing ships");
    let plan = pending_resume(&f).await;
    let answering = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &answering).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = answering.runs();
    assert_eq!(runs.len(), 2, "the owed answer, then a review");
    assert!(
        runs[0].prompt.contains("the sibling parser still breaks"),
        "{}",
        runs[0].prompt
    );
}

/// A grant spent before a round could run, or by a reviewer killed at its
/// cap before it wrote any issue, suspends the job with that round not
/// had: nothing ships unreviewed, and the resume reviews it, running no
/// fixer first.
#[tokio::test]
async fn a_review_round_the_grant_cut_short_is_had_again_by_the_resume() {
    for (case, acts, grant, why) in [
        (
            "spent by the fixer",
            vec![Act::Fix],
            1_000,
            "no budget left to review it",
        ),
        (
            "spent by the reviewer",
            vec![Act::Fix, Act::ReviewKilled(CLEAN)],
            10_000,
            "the reviewer was killed (cap)",
        ),
    ] {
        let bins = support::FakeBins::acquire("fix-review-unreviewed");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-unreviewed", 3).await;
        let ran = acts.len();
        let worker = reviewed_worker(acts).granting(Some(grant));

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{case}");
        assert_eq!(worker.runs().len(), ran, "{case}");
        assert!(bins.calls_to("gh").is_empty(), "{case}: nothing ships");
        let suspended = format!(
            "#{} review round 1: {why}; suspended to go on under a fresh grant",
            f.fid
        );
        let events = f.store.recent_events(20).await.unwrap();
        assert!(
            events.iter().any(|e| e.message == suspended),
            "{case}: {events:?}"
        );
        let fixer = worker.runs()[0].session.clone();
        let plan = pending_resume(&f).await;
        let resumed = reviewed_worker(vec![Act::Review(CLEAN)]);

        let summary = resume(&f, &plan, &resumed).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{case}");
        let runs = resumed.runs();
        assert_eq!(runs.len(), 1, "{case}: only the review");
        assert_eq!(
            runs[0].session,
            review_dir(&fixer, summary.job_id, 1),
            "{case}: round 1 is had again"
        );
        assert_eq!(pr_body(&bins), "the body", "{case}");
        drop(bins);
    }
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
    .granting(Some(10_000))
    .using(0.25);

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
    assert_eq!(job.usage_delta, Some(1.0), "the window every run used");
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
/// ended (a retry an operator released runs its fixer): one that failed
/// before its first token changed nothing the review covered, and throwing
/// the fix away for it would count a failure against a finished, reviewed
/// fix.
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
    let plan = released_resume(&f).await;
    let retry = reviewed_worker(vec![Act::ContinueThen(Ending::Failed)]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(retry.runs().len(), 1, "no second review");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    assert_eq!(pr_body(&bins), "the body");
    assert_eq!(fix_streak(&f).await.streak, 0, "nothing counted");
}

/// A reviewed fix whose push the forge refuses while it answers (a 5xx, an
/// auth backend failing during an incident) is not redone: its job is
/// failed, the attempt is kept as the checkpoint, and it waits
/// `PUBLISH_RETRY_MS` before the retry, which only publishes. Refusals
/// whose text differs (git names the remote, the forge its request id)
/// count as one failure, so an outage of varying errors is bounded too.
#[tokio::test]
async fn a_fix_the_forge_refused_waits_then_publishes_without_redoing_it() {
    let bins = support::FakeBins::acquire("fix-forge-refused");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-forge-refused").await;
    review_on(&mut f, 3);
    let refuse_at = |name: &str| {
        let nowhere = f.cfg.work_root.join(name);
        route_push(&f, Some(&nowhere.to_string_lossy()));
        format!("url.{}.insteadOf", nowhere.display())
    };
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    let age_jobs = async || {
        sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
            .bind(hunter::scheduler::PUBLISH_RETRY_MS)
            .execute(&pool)
            .await
            .unwrap();
    };
    let job_row = async |id: Option<i64>| {
        sqlx::query_as::<_, (String, Option<String>)>("SELECT state, notes FROM jobs WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    let first_route = refuse_at("refusing-1.git");
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]);
    let first = fix(&f, &worker).await;

    assert_eq!(first.outcome.as_deref(), Some("requeued"), "{first:?}");
    let (state, notes) = job_row(first.job_id).await;
    assert_eq!(state, "failed", "nothing shipped: not done");
    assert!(notes.is_some_and(|n| n.starts_with("push failed: ")));
    let streak = fix_streak(&f).await;
    assert_eq!(
        (streak.streak, streak.last_failure.as_deref()),
        (1, Some("publish refused"))
    );
    assert_waits(&f).await;

    age_jobs().await;
    git(&f.repo_dir, &["config", "--unset-all", &first_route]);
    let second_route = refuse_at("refusing-2.git");
    let plan = pending_resume(&f).await;
    assert_eq!(
        Some(plan.predecessor_id()),
        first.job_id,
        "the refused attempt"
    );
    assert_eq!(plan.anticipated(), 0, "the budget gate reserves nothing");
    // The retry runs no worker: refused again, it is still kept, counted
    // as the same failure, and waits.
    let idle = reviewed_worker(vec![]);
    let second = resume(&f, &plan, &idle).await;
    assert!(idle.runs().is_empty(), "nothing for a worker to do");
    assert_eq!(second.outcome.as_deref(), Some("requeued"), "{second:?}");
    assert_eq!(
        fix_streak(&f).await.streak,
        2,
        "a different refusal, the same failure"
    );
    let (state, notes) = job_row(second.job_id).await;
    assert_eq!(state, "failed");
    assert!(notes.is_some_and(|n| n.starts_with("push failed: ")));
    assert_waits(&f).await;

    // The second refusal in a row waits twice as long.
    age_jobs().await;
    assert_waits(&f).await;
    age_jobs().await;
    git(&f.repo_dir, &["config", "--unset-all", &second_route]);
    route_push(&f, None);
    let plan = pending_resume(&f).await;
    assert_eq!(Some(plan.predecessor_id()), second.job_id);
    let retry = reviewed_worker(vec![]);
    let shipped = resume(&f, &plan, &retry).await;

    assert_eq!(shipped.outcome.as_deref(), Some("pr_open"), "{shipped:?}");
    assert!(
        retry.runs().is_empty(),
        "published without redoing, reviewing or even resuming the fix"
    );
    assert_eq!(worker.runs().len(), 2, "one fixer and one reviewer, once");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    assert_eq!(job_row(shipped.job_id).await.0, "done");
}

/// An operator's `continue` of a fix held on refused pushes lifts the
/// wait: the operator is the one who knows the forge is back, and the
/// kept attempt is continued at once, given what the forge kept answering
/// and told to change nothing unless that is about the work itself, so one
/// that changes nothing ships on its saved verdict without another review
/// rather than being told to "finish" it, which could void that verdict.
#[tokio::test]
async fn continuing_a_held_refused_fix_does_not_wait() {
    let bins = support::FakeBins::acquire("fix-refused-released");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-refused-released").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let scope = support::scope(&f.store);
    let held = scope
        .store()
        .hold_work(
            f.fid,
            FindingJobKind::Fix,
            HoldReason::Stuck,
            "push failed",
            refused.job_id,
        )
        .await
        .unwrap();
    assert!(held);

    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    // Released the very millisecond the refusal ended: still since then.
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "UPDATE work_holds SET released_at = (SELECT finished_at FROM jobs WHERE id = ?1)
         WHERE finding_id = ?2",
    )
    .bind(refused.job_id)
    .bind(f.fid)
    .execute(&pool)
    .await
    .unwrap();

    let plan = pending_resume(&f).await;
    assert_eq!(Some(plan.predecessor_id()), refused.job_id);
    git(
        &f.repo_dir,
        &[
            "config",
            "--unset-all",
            &format!("url.{}.insteadOf", nowhere.display()),
        ],
    );
    route_push(&f, None);
    let retry = reviewed_worker(vec![Act::Continue]);
    let shipped = resume(&f, &plan, &retry).await;
    assert_eq!(shipped.outcome.as_deref(), Some("pr_open"), "{shipped:?}");
    let runs = retry.runs();
    assert_eq!(runs.len(), 1, "not reviewed again");
    let prompt = &runs[0].prompt;
    assert!(prompt.contains("Otherwise change nothing"), "{prompt}");
    assert!(prompt.contains("push failed"), "{prompt}");
}

/// A release lifts the wait of the refusal it continued, not of the ones
/// after it: a continued fix the forge refuses again waits as any refusal
/// does.
#[tokio::test]
async fn a_refusal_after_a_release_waits() {
    let bins = support::FakeBins::acquire("fix-refused-after-release");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-refused-after-release").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    let held = support::scope(&f.store)
        .store()
        .hold_work(
            f.fid,
            FindingJobKind::Fix,
            HoldReason::Stuck,
            "push failed",
            refused.job_id,
        )
        .await
        .unwrap();
    assert!(held);
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(&f).await;

    let again = resume(&f, &plan, &reviewed_worker(vec![Act::Continue])).await;

    assert_eq!(again.outcome.as_deref(), Some("requeued"), "{again:?}");
    assert_waits(&f).await;
}

/// A released retry of a kept fix whose own run fails before its first
/// token (a rate limit) still ships the saved verdict, and when the forge
/// refuses that again the finished work stays kept: its job is failed with
/// the refusal as its notes and is the next retry's checkpoint, not lost
/// to a cold redo because its run did not end `done`.
#[tokio::test]
async fn a_released_fix_retry_whose_run_failed_stays_kept_when_refused_again() {
    let _bins = support::FakeBins::acquire("fix-released-refused-again");
    let mut f = fixture("fix-released-refused-again").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let plan = released_resume(&f).await;
    let failing = reviewed_worker(vec![Act::ContinueThen(Ending::Failed)]);

    let again = resume(&f, &plan, &failing).await;

    assert_eq!(again.outcome.as_deref(), Some("requeued"), "{again:?}");
    assert_eq!(failing.runs().len(), 1, "the released fixer ran");
    assert_eq!(checkpoint(&f).await, again.job_id, "still kept");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    let (state, notes): (String, Option<String>) =
        sqlx::query_as("SELECT state, notes FROM jobs WHERE id = ?1")
            .bind(again.job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(state, "failed");
    assert!(notes.is_some_and(|n| n.starts_with("push failed: ")));
}

/// A kept fix whose retry costs no model tokens (no worker, a saved
/// verdict) needs only the forge: it publishes while the model budget is
/// spent and the model provider is unreachable, instead of waiting for a
/// window's reset or the provider to come back. It goes ahead of work the
/// tiers rank higher (an older queued fix), which those would deny and end
/// the cycle on.
#[tokio::test]
async fn a_kept_fix_publishes_past_a_spent_budget_and_an_unreachable_provider() {
    let bins = support::FakeBins::acquire("fix-kept-free");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-kept-free").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    route_push(&f, None);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    // The fixture's bystander, filed first, is the older queued fix.
    f.store
        .set_finding_status(f.fid - 1, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    f.cfg.network_gate = true;
    let provider = "api.provider.example";
    let spent = reviewed_worker(vec![])
        .needing(&[provider])
        .offline(&[provider])
        .denying_above(-1);

    let summary =
        hunter::scheduler::run_cycle(&mut support::jobs(&f.store), &f.cfg, &spent, None).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert!(spent.runs().is_empty(), "no worker");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
}

/// A kept fix keeps its rank behind an older queued fix, whether its
/// retry costs nothing or runs its fixer (released by an operator): a
/// free retry is only the cycle's fallback when the pick is denied, so a
/// kept fix whose own forge is down blocks no work that can run.
#[tokio::test]
async fn a_kept_fix_keeps_its_rank() {
    for released in [false, true] {
        let label = format!("fix-kept-rank-{released}");
        let _bins = support::FakeBins::acquire(&label);
        let mut f = fixture(&label).await;
        review_on(&mut f, 3);
        let nowhere = f.cfg.work_root.join("refusing.git");
        route_push(&f, Some(&nowhere.to_string_lossy()));
        let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
        assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
        let plan = if released {
            released_resume(&f).await
        } else {
            let pool = sqlx::SqlitePool::connect_with(
                sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db),
            )
            .await
            .unwrap();
            sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
                .bind(24 * 60 * 60_000_i64)
                .execute(&pool)
                .await
                .unwrap();
            pending_resume(&f).await
        };
        assert_eq!(plan.free(), !released, "{label}");
        // Only a free retry is the cycle's fallback.
        let offered = hunter::scheduler::pick_free(&support::scope(&f.store), &f.cfg, None, &[])
            .await
            .unwrap();
        assert_eq!(offered.is_some(), !released, "{label}: {offered:?}");
        // The fixture's bystander, filed first, is the older queued fix.
        let older = f.fid - 1;
        f.store
            .set_finding_status(older, hunter::domain::FindingStatus::Queued)
            .await
            .unwrap();

        let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();

        assert!(
            matches!(
                picked,
                Some(hunter::scheduler::Candidate::Finding { finding_id, .. })
                    if finding_id == older
            ),
            "{label}: the older queued fix first: {picked:?}"
        );
    }
}

/// The cycle's fallback offers each free retry in turn: one whose own
/// forge is down is passed over (`tried`), and the next is offered, so it
/// stops no other free retry from publishing.
#[tokio::test]
async fn the_free_fallback_offers_each_kept_fix_in_turn() {
    let _bins = support::FakeBins::acquire("fix-free-each");
    let mut f = fixture("fix-free-each").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    // The fixture's bystander, filed first, is the older queued fix.
    let older = f.fid - 1;
    f.store
        .set_finding_status(older, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    for fid in [older, f.fid] {
        let finding = f.store.get_finding(fid).await.unwrap().unwrap();
        let refused = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]),
        )
        .await
        .unwrap();
        assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    }
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    let free = async |tried: &[i64]| {
        hunter::scheduler::pick_free(&support::scope(&f.store), &f.cfg, None, tried)
            .await
            .unwrap()
            .map(|c| c.target_id())
    };

    assert_eq!(free(&[]).await, Some(older), "the older first");
    assert_eq!(free(&[older]).await, Some(f.fid), "then the next");
    assert_eq!(free(&[older, f.fid]).await, None);
}

/// In a cycle, a free retry whose own forge is down does not stop the
/// next one: with the budget spent, the cycle publishes the newer kept fix
/// (on GitHub) past the older one (on a GitLab that is down), whether that
/// older one is the cycle's pick, denied by its forge, or is passed over
/// after a fresh fix with a `once` override, ranked above both, was denied
/// by the budget.
#[tokio::test]
async fn a_cycle_publishes_past_a_free_retry_whose_forge_is_down() {
    for overridden_first in [false, true] {
        publishes_past_a_down_forge(overridden_first).await;
    }
}

async fn publishes_past_a_down_forge(overridden_first: bool) {
    let label = format!("fix-free-forge-down-{overridden_first}");
    let bins = support::FakeBins::acquire(&label);
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture(&label).await;
    review_on(&mut f, 3);
    let gitlab = bystander_on_gitlab(&f).await;
    let older = f.fid - 1;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    f.store
        .set_finding_status(older, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let nowhere = f.cfg.work_root.join("refusing.git");
    let refusing = nowhere.to_string_lossy().into_owned();
    route_push(&f, Some(&refusing));
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{refusing}.insteadOf"),
            gitlab,
            "--add",
        ],
    );
    for fid in [older, f.fid] {
        let finding = f.store.get_finding(fid).await.unwrap().unwrap();
        let refused = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]),
        )
        .await
        .unwrap();
        assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    }
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    let key = format!("url.{refusing}.insteadOf");
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    route_push(&f, None);
    f.cfg.network_gate = true;
    if overridden_first {
        queue_overridden_fix(&f).await;
    }
    let spent = reviewed_worker(vec![])
        .offline(&["gitlab.example.com"])
        .denying_above(-1);

    let summary =
        hunter::scheduler::run_cycle(&mut support::jobs(&f.store), &f.cfg, &spent, None).await;

    assert_eq!(
        summary.outcome.as_deref(),
        Some("pr_open"),
        "{label}: {summary:?}"
    );
    assert_eq!(summary.finding_id, Some(f.fid), "{label}: {summary:?}");
    drop(bins);
}

/// The GitLab URL of a second repo on another forge, sharing the
/// fixture's clone, which now owns the fixture's bystander.
async fn bystander_on_gitlab(f: &Fixture) -> &'static str {
    let gitlab = "git@gitlab.example.com:acme/other.git";
    let repos_root = f.repo_dir.parent().unwrap().to_path_buf();
    let rid = f
        .store
        .add_repo("other", gitlab, &repos_root, "main", ForgeName::Gitlab)
        .await
        .unwrap();
    std::os::unix::fs::symlink(
        &f.repo_dir,
        hunter::store::Store::repo_dir(&repos_root, rid),
    )
    .unwrap();
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE findings SET repo_id = ?1 WHERE id = ?2")
        .bind(rid)
        .bind(f.fid - 1)
        .execute(&pool)
        .await
        .unwrap();
    gitlab
}

/// File a fresh fix in the fixture's repo, queued and with a `once`
/// override: ranked above every other finding's work.
async fn queue_overridden_fix(f: &Fixture) {
    let repo_id = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
    let (first, _) = f
        .store
        .upsert_finding(
            repo_id,
            &FindingInsert {
                fingerprint: "fp-overridden".to_owned(),
                file: "src/third.rs".to_owned(),
                severity: hunter::domain::Severity::Low,
                confidence: 0.5,
                summary: "prioritised".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    f.store
        .set_finding_status(first, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    f.store
        .set_budget_override(first, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();
}

/// A denied or idle cycle that ran no free retry says when one could run
/// (`kept_retry_at`), so the job lane does not sleep past it. Two kept
/// fixes wait out their refusals, the bystander's ending a minute sooner,
/// and a fresh fix with a `once` override is the pick:
/// - `waiting`, the budget spent: when the bystander's wait ends, also
///   when the cycle is forced onto their repo;
/// - `held`, the bystander held: when the fixture's own wait ends;
/// - `forge down`, the budget spent and the bystander's wait over, free,
///   but on a GitLab that is down: when an offline cycle looks again,
///   sooner than the fixture's wait ends;
/// - `idle`, no fresh fix and every repo disabled: nothing to pick, and
///   when the bystander's wait ends.
#[tokio::test]
async fn a_cycle_says_when_a_kept_fix_can_retry() {
    for case in ["waiting", "held", "forge down", "idle"] {
        let label = format!("fix-kept-retry-at-{}", case.replace(' ', "-"));
        let bins = support::FakeBins::acquire(&label);
        let mut f = fixture(&label).await;
        review_on(&mut f, 3);
        let aged = if case == "forge down" {
            bystander_on_gitlab(&f).await;
            24 * 60 * 60_000
        } else {
            60_000
        };
        let ends = refuse_both(&f, aged).await;
        if case == "held" {
            hold_bystander(&f).await;
        }
        if case == "idle" {
            disable_repos(&f).await;
        } else {
            queue_overridden_fix(&f).await;
        }
        f.cfg.network_gate = true;
        let spent = reviewed_worker(vec![]).offline(&["gitlab.example.com"]);
        let spent = if case == "idle" {
            spent
        } else {
            spent.denying_above(-1)
        };
        let force = (case == "waiting").then_some("widget");

        let before = hunter::util::now_ms();
        let summary =
            hunter::scheduler::run_cycle(&mut support::jobs(&f.store), &f.cfg, &spent, force).await;
        let after = hunter::util::now_ms();

        assert_eq!(
            summary.denied.is_some(),
            case != "idle",
            "{case}: {summary:?}"
        );
        assert_eq!(summary.unreachable, None, "{case}: {summary:?}");
        let at = summary.kept_retry_at;
        match case {
            "waiting" | "idle" => assert_eq!(at, Some(ends[0]), "{case}: {summary:?}"),
            "held" => assert_eq!(at, Some(ends[1]), "{case}: {summary:?}"),
            _ => {
                let three_min = 3 * 60_000;
                assert!(ends[1] > after + three_min);
                assert!(
                    at.is_some_and(|t| (before + three_min..=after + three_min).contains(&t)),
                    "{case}: {summary:?}"
                );
            }
        }
        drop(bins);
    }
}

/// Hold the fixture's bystander's fix on its checkpoint.
async fn hold_bystander(f: &Fixture) {
    let older = f.fid - 1;
    let checkpoint = f
        .store
        .checkpoint_job(older, FindingJobKind::Fix)
        .await
        .unwrap()
        .map(|j| j.id);
    let held = support::scope(&f.store)
        .store()
        .hold_work(
            older,
            FindingJobKind::Fix,
            HoldReason::Stuck,
            "held",
            checkpoint,
        )
        .await
        .unwrap();
    assert!(held);
}

/// Take every repo out of the rotation.
async fn disable_repos(f: &Fixture) {
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE repos SET enabled = 0")
        .execute(&pool)
        .await
        .unwrap();
}

/// Queue the fixture's bystander and have the forge refuse both fixes,
/// the bystander's ended `aged` ms ago and the fixture's just now: when
/// each one's wait ends, the bystander's first.
async fn refuse_both(f: &Fixture, aged: i64) -> [i64; 2] {
    let older = f.fid - 1;
    f.store
        .set_finding_status(older, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(f, Some(&nowhere.to_string_lossy()));
    // The bystander's repo may be on GitLab (`bystander_on_gitlab`).
    let refusing = format!("url.{}.insteadOf", nowhere.display());
    git(
        &f.repo_dir,
        &[
            "config",
            &refusing,
            "git@gitlab.example.com:acme/other.git",
            "--add",
        ],
    );
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    let mut ends = [0; 2];
    for (end, (fid, aged)) in ends.iter_mut().zip([(older, aged), (f.fid, 0)]) {
        let finding = f.store.get_finding(fid).await.unwrap().unwrap();
        let refused = hunter::scheduler::run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]),
        )
        .await
        .unwrap();
        assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
        (*end,) = sqlx::query_as::<_, (i64,)>(
            "UPDATE jobs SET finished_at = finished_at - ?1 WHERE id = ?2
             RETURNING finished_at + ?3",
        )
        .bind(aged)
        .bind(refused.job_id)
        .bind(hunter::scheduler::PUBLISH_RETRY_MS)
        .fetch_one(&pool)
        .await
        .unwrap();
    }
    let key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    ends
}

/// The status page previews a kept fix whose retry costs no model tokens
/// as the scheduler runs it: allowed, even with the budget spent, since
/// the budget gate does not ask about it, and in place of the older queued
/// fix ranked above it, which the spent budget denies.
#[tokio::test]
async fn the_summary_previews_a_free_retry_as_allowed_with_the_budget_spent() {
    use tower::util::ServiceExt;

    let _bins = support::FakeBins::acquire("fix-kept-summary");
    let mut f = fixture("fix-kept-summary").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    support::seed_session(&pool).await;
    // The fixture's bystander, filed first, is the older queued fix.
    f.store
        .set_finding_status(f.fid - 1, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let state = hunter::server::AppState {
        store: std::sync::Arc::new(f.store.clone()),
        config: std::sync::Arc::new(f.cfg.clone()),
        // Denies every reservation, 0 included: a spent window.
        backend: std::sync::Arc::new(hunter::backend::NullBackend),
        repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        scheduler: {
            // Overdrive on: the fallback it previews runs prioritized.
            let scheduler = hunter::server::SchedulerHandle::default();
            scheduler
                .overdrive
                .store(true, std::sync::atomic::Ordering::SeqCst);
            scheduler
        },
    };

    let response = hunter::server::router(state)
        .oneshot(
            axum::http::Request::builder()
                .header(axum::http::header::COOKIE, support::TEST_COOKIE)
                .uri("/api/summary")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let summary: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let next = &summary["next_candidate"];
    assert_eq!(next["id"], f.fid, "{next}");
    assert_eq!(next["budget_state"], "allowed", "{next}");
    assert_eq!(next["is_prioritized"], true, "{next}");
}

/// A publish the forge refuses while the model provider is down is still
/// the forge's refusal: the push talks to the forge alone. It is counted
/// and waits like any refusal, not taken for an offline attempt, which
/// would count nothing, skip the wait, and give the finished work up once
/// the chain's spare ran out.
#[tokio::test]
async fn a_refusal_while_the_provider_is_down_counts_as_a_refusal() {
    let _bins = support::FakeBins::acquire("fix-refused-provider-down");
    let mut f = fixture("fix-refused-provider-down").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    f.cfg.network_gate = true;
    let provider = "api.provider.example";
    let down = reviewed_worker(vec![])
        .needing(&[provider])
        .offline(&[provider]);

    let again =
        hunter::scheduler::run_cycle(&mut support::jobs(&f.store), &f.cfg, &down, None).await;

    assert_eq!(again.outcome.as_deref(), Some("requeued"), "{again:?}");
    assert_eq!(again.unreachable, None, "{again:?}");
    assert_eq!(fix_streak(&f).await.streak, 2, "counted as a refusal");
    assert_waits(&f).await;
}

/// The wait before a refused fix is retried counts from the refusal, not
/// from when the worker finished: a forge that hangs the push for minutes
/// before refusing it would otherwise have used up the wait already.
#[tokio::test]
async fn a_slow_refusal_waits_from_when_it_came() {
    let _bins = support::FakeBins::acquire("fix-refused-slowly");
    let mut f = fixture("fix-refused-slowly").await;
    review_on(&mut f, 3);
    route_push(&f, None);
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    let hook = std::path::Path::new(origin.trim()).join("hooks/pre-receive");
    std::fs::write(
        &hook,
        "#!/bin/sh\nsleep 2\necho 'Internal Server Error' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let worker = reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)]);

    let refused = fix(&f, &worker).await;
    let returned = hunter::util::now_ms();

    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    let (started, finished): (i64, i64) =
        sqlx::query_as("SELECT started_at, finished_at FROM jobs WHERE id = ?1")
            .bind(refused.job_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        finished - started >= 2_000,
        "ended at the refusal: {started}..{finished}"
    );
    assert!(finished <= returned);
}

/// A refused fix is continued however long the chain that finished it
/// was, and so is one whose retry then could not reach the forge: either
/// way its work is done, so the resume ceiling, which retires chains that
/// are not finishing, does not throw it away and start it cold. The
/// failure streak bounds its retries instead.
#[tokio::test]
async fn a_refused_fix_at_the_resume_ceiling_is_still_continued() {
    let _bins = support::FakeBins::acquire("fix-refused-ceiling");
    let mut f = fixture("fix-refused-ceiling").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let capped = fix(&f, &reviewed_worker(vec![Act::FixThen(Ending::Capped)])).await;
    assert_eq!(capped.outcome.as_deref(), Some("suspended"), "{capped:?}");
    for _ in 0..2 {
        let plan = pending_resume(&f).await;
        let again = resume(
            &f,
            &plan,
            &reviewed_worker(vec![Act::ContinueThen(Ending::Capped)]),
        )
        .await;
        assert_eq!(again.outcome.as_deref(), Some("suspended"), "{again:?}");
    }
    let plan = pending_resume(&f).await;
    let finished = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);
    let refused = resume(&f, &plan, &finished).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    // Every link spent (four attempts, the ceiling), and the wait is over.
    sqlx::query("UPDATE jobs SET tokens_new = 1000, finished_at = finished_at - ?1")
        .bind(hunter::scheduler::PUBLISH_RETRY_MS)
        .execute(&pool)
        .await
        .unwrap();
    let plan = pending_resume(&f).await;
    assert_eq!(Some(plan.predecessor_id()), refused.job_id);

    // Retries that find the forge unreachable count nothing, so only the
    // spare's own bound ends them: kept, and continued, until the chain
    // holds six attempts kept to publish (the refusal and five offline),
    // as many as the refusals that hold the work.
    let host = "github.com";
    let mut plan = plan;
    for kept in 2..=6 {
        f.cfg.network_gate = true;
        let offline = resume(
            &f,
            &plan,
            &reviewed_worker(vec![Act::Continue]).offline(&[host]),
        )
        .await;
        assert_eq!(offline.unreachable.as_deref(), Some(host), "{offline:?}");
        f.cfg.network_gate = false;
        sqlx::query("UPDATE jobs SET tokens_new = 1000")
            .execute(&pool)
            .await
            .unwrap();
        let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        match picked {
            Some(hunter::scheduler::Candidate::Resume { plan: next, .. }) if kept < 6 => {
                assert_eq!(Some(next.predecessor_id()), offline.job_id);
                plan = *next;
            }
            Some(hunter::scheduler::Candidate::Finding { .. }) if kept == 6 => {}
            other => panic!("{kept} kept: {other:?}"),
        }
    }
}

/// A kept fix whose retries keep finding the forge unreachable (a flapping
/// network) counts nothing in its streak, and its retries run no worker,
/// so they never add up to the resume ceiling: once the chain holds
/// `MAX_PUBLISH_REFUSALS` attempts kept to publish, it is given up
/// outright instead of being retried forever.
#[tokio::test]
async fn a_fix_kept_offline_is_given_up_when_its_spare_ends() {
    let _bins = support::FakeBins::acquire("fix-offline-spare");
    let mut f = fixture("fix-offline-spare").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(hunter::scheduler::PUBLISH_RETRY_MS)
        .execute(&pool)
        .await
        .unwrap();
    let mut plan = pending_resume(&f).await;
    let host = "github.com";
    for kept in 2..=6 {
        f.cfg.network_gate = true;
        let offline = resume(&f, &plan, &reviewed_worker(vec![]).offline(&[host])).await;
        assert_eq!(offline.unreachable.as_deref(), Some(host), "{offline:?}");
        f.cfg.network_gate = false;
        let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        match picked {
            Some(hunter::scheduler::Candidate::Resume { plan: next, .. }) if kept < 6 => {
                plan = *next;
            }
            Some(hunter::scheduler::Candidate::Finding { .. }) if kept == 6 => {}
            other => panic!("{kept} kept: {other:?}"),
        }
    }
    assert_eq!(checkpoint(&f).await, None, "the chain is given up");
}

/// The spare counts attempts kept to publish since the operator's last
/// release, as the ceiling and the streak do: a refused fix the operator
/// continued is still continued at the ceiling after four more refusals,
/// where counting the chain's whole life (two before the release, four
/// after) would end the spare and redo the finished work cold.
#[tokio::test]
async fn a_continued_refused_fix_gets_the_spare_again() {
    let _bins = support::FakeBins::acquire("fix-refused-continued");
    let mut f = fixture("fix-refused-continued").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    // Every link spent, and every wait over.
    let age = async || {
        sqlx::query("UPDATE jobs SET tokens_new = 1000, finished_at = finished_at - ?1")
            .bind(24 * 60 * 60_000_i64)
            .execute(&pool)
            .await
            .unwrap();
    };
    let first = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(first.outcome.as_deref(), Some("requeued"), "{first:?}");
    age().await;
    let plan = pending_resume(&f).await;
    let mut last = resume(&f, &plan, &reviewed_worker(vec![Act::Continue])).await;
    assert_eq!(last.outcome.as_deref(), Some("requeued"), "{last:?}");
    let scope = support::scope(&f.store);
    assert!(
        scope
            .store()
            .hold_work(
                f.fid,
                FindingJobKind::Fix,
                HoldReason::Stuck,
                "publish refused",
                last.job_id
            )
            .await
            .unwrap()
    );
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;

    for _ in 0..4 {
        let plan = pending_resume(&f).await;
        assert_eq!(Some(plan.predecessor_id()), last.job_id);
        last = resume(&f, &plan, &reviewed_worker(vec![Act::Continue])).await;
        assert_eq!(last.outcome.as_deref(), Some("requeued"), "{last:?}");
        age().await;
    }

    let plan = pending_resume(&f).await;
    assert_eq!(Some(plan.predecessor_id()), last.job_id, "still spared");
}

/// A once override is not spent by a fix the forge refused: the attempt
/// is kept to publish, and the retry that publishes is the one the
/// override is for. A review round cut off while the forge was unreachable
/// is not marked as kept to publish: its work is not done.
#[tokio::test]
async fn a_refused_fix_keeps_its_once_override() {
    let _bins = support::FakeBins::acquire("fix-refused-once");
    let mut f = fixture("fix-refused-once").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();

    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;

    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.budget_override,
        Some(hunter::domain::BudgetOverride::Once)
    );
}

/// A refused fix whose review ended without a verdict of its own (a
/// reviewer that wrote no review: the fix ships with a note, and is
/// otherwise reviewed again) still ships on the retry that only publishes,
/// with the same note, and without any worker run: the verdict it shipped
/// is kept with it.
#[tokio::test]
async fn a_refused_fix_after_a_failed_review_ships_on_its_retry() {
    let bins = support::FakeBins::acquire("fix-refused-failed-review");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-refused-failed-review").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));

    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::ReviewSilent])).await;

    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    route_push(&f, None);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    let plan = pending_resume(&f).await;
    let retry = reviewed_worker(vec![]);

    let shipped = resume(&f, &plan, &retry).await;

    assert_eq!(shipped.outcome.as_deref(), Some("pr_open"), "{shipped:?}");
    assert!(retry.runs().is_empty(), "not resumed or reviewed again");
    assert!(
        pr_body(&bins).starts_with("the body\n\n"),
        "{}",
        pr_body(&bins)
    );
}

/// A refused fix whose verdict pinned a commit (a reviewer committed in
/// the tree and kept the index locked, so its commit could not be undone:
/// the commit it was shown ships, not the tree) keeps no verdict, so its
/// retry is reviewed again. No fixer is resumed for it, but the review
/// costs, so the budget gate reserves for it as for any resume. The retry
/// puts the tree back to the reviewed commit first, so neither that review
/// nor the publish sees the reviewer's commit.
#[tokio::test]
async fn a_refused_fix_with_a_pinned_verdict_is_reserved_for_and_reviewed_again() {
    let bins = support::FakeBins::acquire("fix-refused-pinned");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-refused-pinned").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let worker = reviewed_worker(vec![Act::Fix, Act::LockingReview(NOT_BLOCKING)]);
    let refused = fix(&f, &worker).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    // The operator clears the lock that kept the commit from being undone.
    let tree = worker.runs()[0].tree.clone();
    std::fs::remove_file(index_lock(&tree)).unwrap();
    let key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    route_push(&f, None);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    let plan = pending_resume(&f).await;
    assert!(plan.anticipated() > 0, "the review is reserved for");
    let retry = reviewed_worker(vec![Act::Review(CLEAN)]);

    let shipped = resume(&f, &plan, &retry).await;

    assert_eq!(shipped.outcome.as_deref(), Some("pr_open"), "{shipped:?}");
    assert_eq!(retry.runs().len(), 1, "a reviewer, and no fixer");
    assert_eq!(
        shipped_commits(&f),
        ["fix: the real bug"],
        "the reviewer's commit never ships"
    );
}

/// Only a saved verdict that covers the kept tree spares its retry the
/// review, and so the reservation: one left from a tree that has changed
/// since (a curation that pinned the reviewed commit leaves the clean
/// review's verdict behind) is no verdict, and the retry is reserved for.
#[tokio::test]
async fn a_saved_verdict_for_another_tree_does_not_spare_the_reservation() {
    let _bins = support::FakeBins::acquire("fix-refused-stale-verdict");
    let mut f = fixture("fix-refused-stale-verdict").await;
    review_on(&mut f, 3);
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();
    let plan = pending_resume(&f).await;
    assert_eq!(plan.anticipated(), 0, "its verdict covers the kept tree");
    std::fs::write(
        plan.workspace().root.join("review-ending.json"),
        r#"{"note":null,"title":null,"tree":{"head":"0123456789abcdef0123456789abcdef01234567","branch":null,"status":null,"description":null}}"#,
    )
    .unwrap();

    let plan = pending_resume(&f).await;

    assert!(plan.anticipated() > 0, "a stale verdict covers nothing");
}

/// Refused publishes hold the fix only at the sixth in a row, not the
/// third as other failures do: with the waits doubling from ten minutes,
/// a forge incident of a few hours is ridden out without an operator. A
/// refusal kept to publish concludes nothing and keeps a one-shot
/// override; the hold concludes the attempt and spends it.
#[tokio::test]
async fn a_refused_fix_is_held_only_at_the_sixth_refusal() {
    for (before, holds) in [(4, false), (5, true)] {
        let label = format!("fix-refused-{before}");
        let _bins = support::FakeBins::acquire(&label);
        let mut f = fixture(&label).await;
        review_on(&mut f, 3);
        let nowhere = f.cfg.work_root.join("refusing.git");
        route_push(&f, Some(&nowhere.to_string_lossy()));
        for _ in 0..before {
            support::scope(&f.store)
                .store()
                .record_work_failure(f.fid, FindingJobKind::Fix, "publish refused")
                .await
                .unwrap();
        }
        f.store
            .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
            .await
            .unwrap();

        let refused = fix(&f, &reviewed_worker(vec![Act::Fix, Act::Review(CLEAN)])).await;

        let outcome = if holds { "held" } else { "requeued" };
        assert_eq!(refused.outcome.as_deref(), Some(outcome), "{refused:?}");
        assert_eq!(held(&f).await.is_some(), holds, "{before} before");
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let left = (!holds).then_some(hunter::domain::BudgetOverride::Once);
        assert_eq!(after.budget_override, left, "{before} before");
    }
}

/// Shutdown lets the run a cycle is in finish, and the unit's stop
/// timeout covers one run, so the review starts no further run once the
/// daemon is stopping. The job is suspended on the fixer's transcript,
/// nothing ships, and the resume reviews it, without resuming the
/// finished fixer first.
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
    let resumed = reviewed_worker(vec![Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 1, "only the review: {runs:?}");
    assert!(runs[0].prompt.starts_with("review"), "{}", runs[0].prompt);
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

/// A reviewer that changed the tree (a commit of its own, an edit to a
/// tracked file, a test artifact left behind) has its changes undone: the
/// fix branch is checked out again at the commit it was shown and the
/// artifact removed. Blocking issues it had verified and written still
/// stand, as a killed reviewer's do: answered while there are rounds left,
/// else holding the fix with the tree back where the reviewer found it.
/// Its commit never ships.
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

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0, "no PR");
    let hold = held(&f).await.expect("held");
    assert!(
        hold.detail.contains("the test misses the empty input"),
        "{}",
        hold.detail
    );
    let tree = &worker.runs()[0].tree;
    assert_eq!(
        git(tree, &["status", "--porcelain", "--ignored"]),
        "?? PR-DESCRIPTION.md\n",
        "nothing of the reviewer's is left"
    );
    assert_eq!(
        std::fs::read_to_string(tree.join("FIX.md")).unwrap(),
        "fix: the real bug\n"
    );
    assert_eq!(
        git(tree, &["log", "-1", "--format=%s"]).trim(),
        "fix: the real bug"
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
    let worker = reviewed_worker(vec![Act::Fix, Act::DetachingReview(CLEAN)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_subject(&f), "fix: the real bug");
    let body = pr_body(&bins);
    assert!(
        body.contains("Review round 1 failed: the reviewer changed the worktree"),
        "{body}"
    );
}

/// The fixer answers a reviewer that detached `HEAD` on the fix branch,
/// checked out again: its answer is a commit of the branch.
#[tokio::test]
async fn a_reviewer_that_detaches_head_has_the_fixer_answer_on_the_branch() {
    let f = reviewed_fixture("fix-review-detached-answer", 3).await;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::DetachingReview(BLOCKING),
        Act::AnswerSuspended,
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(
        summary.state,
        Some(hunter::domain::JobState::Suspended),
        "{summary:?}"
    );
    let tree = &worker.runs()[0].tree;
    assert_eq!(
        Some(git(tree, &["branch", "--show-current"]).trim()),
        summary.branch.as_deref()
    );
    assert_eq!(
        git(tree, &["log", "-1", "--format=%s"]).trim(),
        "fix: half of the answer"
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
/// hold the fix for the operator.
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

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 14, "the fix, then 7 reviews and 6 answers");
    assert_eq!(
        runs[13].session,
        review_dir(&runs[0].session, summary.job_id, 7)
    );
    assert_eq!(bins.calls_to("gh").len(), 0, "no PR");
    let hold = held(&f).await.expect("held");
    assert!(
        hold.detail.contains("round 7 (the last of 7 rounds)"),
        "{}",
        hold.detail
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
    let events = f.store.recent_events(30).await.unwrap();
    let said = |text: &str| {
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.ends_with(text))
    };
    assert!(
        said("review round 1: 1 issue (1 blocking), 1 follow-up; back to the fixer"),
        "{events:?}"
    );
    assert!(
        said("review round 2: no issues to answer, 2 follow-ups; shipping"),
        "{events:?}"
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
            &reviewed_worker(Vec::new()),
        )
        .await
        .unwrap();
        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{restart}");
    }
    let plan = pending_resume(&f).await;

    let summary = resume(&f, &plan, &reviewed_worker(vec![Act::Review(CLEAN)])).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(pr_body(&bins), "the body");
}

/// Run the fixture's fix cold with `worker`, which sets the stop latch
/// itself ([`reviewed_worker_stopping`]).
async fn fix_until_stopped(
    f: &Fixture,
    acts: Vec<Act>,
    stop: usize,
) -> (hunter::scheduler::CycleSummary, ScriptedBackend) {
    let (latch, stopping) = tokio::sync::watch::channel(false);
    let worker = reviewed_worker_stopping(acts, (stop, latch));
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
    (summary, worker)
}

/// A shutdown that arrives while a reviewer runs stops the job before the
/// fixer is sent its issues, and keeps them: the resume answers them
/// first, without another review of that round.
#[tokio::test]
async fn a_stop_before_the_answer_keeps_the_rounds_issues_for_the_resume() {
    let bins = support::FakeBins::acquire("fix-review-stop-owed");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-stop-owed", 3).await;

    let (summary, worker) = fix_until_stopped(&f, vec![Act::Fix, Act::Review(BLOCKING)], 1).await;

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2, "no answer after the stop");
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 2, "the answer, then the next round");
    assert!(
        runs[0].prompt.starts_with("answer") && runs[0].prompt.contains("the sibling parser"),
        "{}",
        runs[0].prompt
    );
    assert_eq!(
        runs[1].session,
        review_dir(&worker.runs()[0].session, summary.job_id, 2),
        "the review goes on with round 2"
    );
}

/// A shutdown that arrives while the last reviewer runs stops the job
/// before its curation instead of shipping it uncurated; the resume
/// curates and ships without another review.
#[tokio::test]
async fn a_stop_before_the_curation_suspends_and_the_resume_curates() {
    let bins = support::FakeBins::acquire("fix-review-stop-curate");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-stop-curate", 7).await;
    f.cfg.review_curate = true;
    let acts = vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ];

    let (summary, worker) = fix_until_stopped(&f, acts, 3).await;

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(worker.runs().len(), 4, "no curation after the stop");
    assert!(bins.calls_to("gh").is_empty(), "nothing ships uncurated");
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Curate(Curation::Squash)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(resumed.runs().len(), 1, "only the curation");
    assert_eq!(shipped_commits(&f), ["fix: the real bug, curated"]);
}

/// A suspension because the grant ran out between the review's runs costs
/// the chain no resume attempt, as a shutdown stop does not: a fix whose
/// fixer was already cut off at its cap three times, and that is then
/// owed only its curation, is resumed and ships, where a counted fourth
/// attempt would give the chain up and throw the reviewed fix away.
#[tokio::test]
async fn a_spent_grant_between_runs_costs_the_chain_no_resume_attempt() {
    let bins = support::FakeBins::acquire("fix-review-grant-uncounted");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-grant-uncounted", 7).await;
    f.cfg.review_curate = true;
    let summary = fix(&f, &reviewed_worker(vec![Act::FixThen(Ending::Capped)])).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    for attempt in 2..=3 {
        let plan = pending_resume(&f).await;
        let capped = reviewed_worker(vec![Act::ContinueThen(Ending::Capped)]);
        let summary = resume(&f, &plan, &capped).await;
        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{attempt}");
    }
    let plan = pending_resume(&f).await;
    let reviewed = reviewed_worker(vec![
        Act::Continue,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
    ])
    .granting(Some(4_000));
    let summary = resume(&f, &plan, &reviewed).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(reviewed.runs().len(), 4, "suspended before the curation");

    let plan = pending_resume(&f).await;
    let curating = reviewed_worker(vec![Act::Curate(Curation::Squash)]);
    let summary = resume(&f, &plan, &curating).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_commits(&f), ["fix: the real bug, curated"]);
}

/// A review that keeps running out of grant is bounded all the same: its
/// grant suspensions are no attempt, but they count as runs for the cost
/// arm of the give-up ceiling, so the chain is held for the operator with
/// its work kept, instead of being resumed for ever, a full grant each.
#[tokio::test]
async fn a_review_that_keeps_running_out_of_grant_is_held_for_its_cost() {
    let bins = support::FakeBins::acquire("fix-review-grant-endless");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-grant-endless", 3).await;
    let first = reviewed_worker(vec![Act::Fix, Act::ReviewKilled(CLEAN)]).granting(Some(10_000));
    let summary = fix(&f, &first).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");

    let mut resumes = 0;
    while held(&f).await.is_none() {
        assert!(
            resumes < 20,
            "still resumed after {resumes} cut-off reviews"
        );
        let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        let Some(hunter::scheduler::Candidate::Resume { plan, .. }) = picked else {
            break;
        };
        let capped = reviewed_worker(vec![Act::ReviewKilled(CLEAN)]).granting(Some(10_000));
        let summary = resume(&f, &plan, &capped).await;
        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
        resumes += 1;
    }

    let hold = held(&f).await.expect("held for its cost");
    assert_eq!(hold.reason, HoldReason::GaveUp, "{hold:?}");
    assert!(hold.detail.contains("over budget"), "{}", hold.detail);
    assert!(checkpoint(&f).await.is_some(), "its work is kept");
    assert!(bins.calls_to("gh").is_empty(), "nothing ships unreviewed");
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
            assert!(
                body.contains("The fixer could not answer them: its answer exited 1."),
                "{body}"
            );
        }
        drop(bins);
    }
}

/// An answer that fails without doing work while the forge is unreachable
/// is the network's failure, handled as the fix's other offline failures
/// are: nothing is published or counted, the attempt is kept, and the retry
/// reviews that round (here round 2) again before anything ships, not
/// round 1.
#[tokio::test]
async fn an_answer_that_fails_offline_is_kept_and_its_round_reviewed_again() {
    let bins = support::FakeBins::acquire("fix-review-answer-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-answer-offline", 7).await;
    f.cfg.network_gate = true;
    let host = "github.com";
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(BLOCKING),
        Act::AnswerFails,
    ])
    .offline(&[host]);

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
        Act::FixMore,
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
        review_dir(&runs[0].session, summary.job_id, 2),
        "round 2 again"
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
    let kept = f
        .store
        .checkpoint_job(f.fid, FindingJobKind::Fix)
        .await
        .unwrap()
        .expect("kept to review again");
    assert!(
        !kept
            .notes
            .as_deref()
            .is_some_and(|n| n.starts_with("unpublished while offline")),
        "cut off in review, not kept only to publish: {:?}",
        kept.notes
    );
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

/// A round cut off while a host is unreachable, whose reviewer committed
/// and kept the index locked so its commit could not be undone, keeps the
/// attempt with the tree as the reviewer left it, and the tree it was shown
/// recorded: the retry puts the tree back to it before any worker runs.
/// While the lock is in the way no worker runs, and the attempt fails as
/// `reviewed tree not restored`; once it is gone, the fixer resumes on the
/// reviewed commit and the reviewer's commit never ships.
#[tokio::test]
async fn a_round_cut_off_offline_by_an_undoable_reviewer_resumes_on_the_reviewed_commit() {
    let bins = support::FakeBins::acquire("fix-review-locked-offline");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-review-locked-offline", 7).await;
    f.cfg.network_gate = true;
    let host = "api.provider.example";
    let worker = reviewed_worker(vec![Act::Fix, Act::LockingReview(NOT_BLOCKING)])
        .needing(&[host])
        .offline(&[host]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0, "nothing ships");
    assert!(held(&f).await.is_none(), "nothing is held");
    let tree = worker.runs()[0].tree.clone();
    assert_eq!(
        git(&tree, &["log", "-1", "--format=%s"]).trim(),
        "review: a commit of the reviewer's",
        "the reviewer's commit could not be undone"
    );
    let reviewed = git(&tree, &["rev-parse", "HEAD^"]).trim().to_owned();
    f.cfg.network_gate = false;
    let retry = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);
    let plan = pending_resume(&f).await;

    let blocked = resume(&f, &plan, &retry).await;

    assert!(
        blocked
            .failure
            .as_deref()
            .is_some_and(|r| r.starts_with("reviewed tree not restored")),
        "{blocked:?}"
    );
    assert!(
        retry.runs().is_empty(),
        "no worker runs on the reviewer's leftovers"
    );
    std::fs::remove_file(index_lock(&tree)).unwrap();
    let plan = pending_resume(&f).await;

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(retry.runs().len(), 2, "the fixer, then the round again");
    assert_eq!(
        shipped_commits(&f),
        ["fix: the real bug"],
        "the reviewer's commit never ships"
    );
    let events = f.store.recent_events(30).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "review"
            && e.message.contains(&format!(
                "the tree a reviewer left was put back to the reviewed commit {reviewed}"
            ))),
        "{events:?}"
    );
}

/// A reviewer whose changes to the tree cannot be undone fails its round,
/// and the fix ships unreviewed with a note, unless the reviewer had
/// written a blocking issue, or any issue in the last round: those never
/// ship, and hold the fix, whose report names the commit the tree is put
/// back to on `continue` ([`continue_locked`]).
#[tokio::test]
async fn a_blocking_issue_from_a_reviewer_that_could_not_be_undone_holds_the_fix() {
    for (review, max_rounds, outcome) in [
        (BLOCKING, 7, "held"),
        (NOT_BLOCKING, 7, "pr_open"),
        (NOT_BLOCKING, 1, "held"),
    ] {
        let bins = support::FakeBins::acquire("fix-review-undoable");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-undoable", max_rounds).await;
        let worker = reviewed_worker(vec![Act::Fix, Act::LockingReview(review)]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{summary:?}");
        if outcome == "held" {
            assert_eq!(bins.calls_to("gh").len(), 0, "a held fix never ships");
            let report = held(&f).await.expect("held").detail;
            assert!(report.contains("could not be undone"), "{report}");
            let issue = if review == BLOCKING {
                "the sibling parser still breaks"
            } else {
                "the test misses the empty input"
            };
            assert!(report.contains(issue), "{report}");
            let tree = worker.runs()[0].tree.clone();
            let reviewed = git(&tree, &["rev-parse", "HEAD^"]);
            assert!(
                report.contains(&format!(
                    "put back to the reviewed commit `{}`",
                    reviewed.trim()
                )),
                "{report}"
            );
            continue_locked(&f, &tree, issue).await;
        } else {
            let body = pr_body(&bins);
            assert!(body.contains("could not be undone"), "{body}");
            assert!(body.contains("the test misses the empty input"), "{body}");
        }
        drop(bins);
    }
}

/// The operator's `continue` of a fix held on a reviewer that committed
/// and keeps the index locked ([`Act::LockingReview`]) in `tree`: its
/// commit cannot be undone, so every attempt fails without running the
/// fixer, counted under `reviewed tree not restored` with what is in the
/// way, and the third holds the fix `stuck`. Once the operator removes the
/// lock and continues that hold, the next attempt puts the tree back and
/// resumes the fixer on the held `issue`, which no fixer has run on yet,
/// not on the stuck hold's failure, and the fix ships without the
/// reviewer's commit.
async fn continue_locked(f: &Fixture, tree: &std::path::Path, issue: &str) {
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    let branch = git(tree, &["branch", "--show-current"]).trim().to_owned();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let resumed = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);
    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let plan = pending_resume(f).await;

        let summary = resume(f, &plan, &resumed).await;

        assert!(
            summary
                .failure
                .as_deref()
                .is_some_and(|r| r.starts_with("reviewed tree not restored")),
            "{summary:?}"
        );
        assert_eq!(
            (
                summary.kind,
                summary.finding_id,
                summary.state,
                summary.branch.as_deref()
            ),
            (
                Some(JobKind::Finding(FindingJobKind::Fix)),
                Some(f.fid),
                Some(JobState::Failed),
                Some(branch.as_str())
            ),
            "{summary:?}"
        );
        let (state, notes): (String, String) =
            sqlx::query_as("SELECT state, notes FROM jobs WHERE id = ?1")
                .bind(summary.job_id.expect("the attempt's job"))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, "failed", "attempt {attempt}");
        assert!(notes.starts_with("reviewed tree not restored"), "{notes}");
        assert!(
            resumed.runs().is_empty(),
            "no worker runs on the reviewer's leftovers"
        );
        let streak = fix_streak(f).await;
        assert_eq!(
            (streak.streak, streak.last_failure.as_deref()),
            (
                attempt % 3,
                (attempt < 3).then_some("reviewed tree not restored")
            ),
            "attempt {attempt}"
        );
        outcomes.push(summary.outcome.unwrap_or_default());
    }
    assert_eq!(outcomes, ["requeued", "requeued", "held"]);
    let hold = held(f).await.expect("held stuck at the limit");
    assert_eq!(hold.reason, HoldReason::Stuck);
    let lock = index_lock(tree);
    let locked = format!(
        "checkout failed: fatal: Unable to create '{}'",
        lock.display()
    );
    for named in ["reviewed tree not restored", &locked, "HEAD is at "] {
        assert!(hold.detail.contains(named), "{named}: {hold:?}");
    }
    std::fs::remove_file(&lock).unwrap();
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(f).await;

    let summary = resume(f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = resumed.runs();
    assert_eq!(runs.len(), 2, "the fixer, then a clean review");
    assert!(
        runs[0].prompt.contains(issue),
        "the held issues go to the fixer: {}",
        runs[0].prompt
    );
    assert_eq!(shipped_commits(f), ["fix: the real bug"]);
}

/// A fix held on a reviewer whose commit could not be undone resumes on
/// the commit the reviewer was shown: the operator's `continue` puts the
/// fix branch back to it before the fixer runs, so the reviewer's commit
/// never ships. Whether a blocking issue held it, or any issue in the last
/// round. Only once: a later attempt of the chain keeps what the fixer
/// committed since.
#[tokio::test]
async fn a_continued_hold_on_an_undoable_reviewer_resumes_on_the_reviewed_commit() {
    for (review, max_rounds) in [(BLOCKING, 7), (NOT_BLOCKING, 1)] {
        let bins = support::FakeBins::acquire("fix-review-locked");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-locked", max_rounds).await;
        let worker = reviewed_worker(vec![Act::Fix, Act::LockingReview(review)]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
        let tree = worker.runs()[0].tree.clone();
        assert_eq!(
            git(&tree, &["log", "-1", "--format=%s"]).trim(),
            "review: a commit of the reviewer's",
            "the reviewer's commit could not be undone"
        );
        let reviewed = git(&tree, &["rev-parse", "HEAD^"]).trim().to_owned();
        let report = held(&f).await.expect("held").detail;
        assert!(
            report.contains(&format!("put back to the reviewed commit `{reviewed}`")),
            "{report}"
        );
        // The operator clears the lock the report quotes, and continues.
        std::fs::remove_file(index_lock(&tree)).unwrap();
        support::release(
            &f.store,
            f.fid,
            FindingJobKind::Fix,
            ReleaseAction::Continue,
        )
        .await;
        let plan = pending_resume(&f).await;
        let resumed = reviewed_worker(vec![
            Act::AnswerSuspended,
            Act::Continue,
            Act::Review(CLEAN),
        ]);

        let summary = resume(&f, &plan, &resumed).await;

        assert_eq!(
            summary.state,
            Some(hunter::domain::JobState::Suspended),
            "{summary:?}"
        );
        let plan = pending_resume(&f).await;
        let summary = resume(&f, &plan, &resumed).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
        assert_eq!(
            shipped_commits(&f),
            ["fix: half of the answer", "fix: the real bug"],
            "the fixer built on the reviewed commit, and its answer was kept"
        );
        let events = f.store.recent_events(30).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "review"
                && e.message
                    .contains(&format!("put back to the reviewed commit {reviewed}"))),
            "{events:?}"
        );
        drop(bins);
    }
}

/// The change is its commits: before a reviewer runs, the daemon discards
/// the fixer's uncommitted changes to tracked files and deletes its
/// untracked files, so the reviewer is shown the commits alone. Ignored
/// files (a build cache) stay, as does the description, the PR body. The
/// round's event names what went, ten paths at most. A clean verdict on
/// that tree ships the fix with its body intact.
#[tokio::test]
async fn the_reviewer_is_shown_the_commits_alone() {
    let bins = support::FakeBins::acquire("fix-review-cleaned");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-cleaned", 7).await;
    let worker = reviewed_worker(vec![Act::FixUntidy, Act::Review(CLEAN)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        seen(&worker.runs()[1]),
        "?? PR-DESCRIPTION.md\n!! target/\n",
        "the reviewer finds the description and the ignored cache, nothing else"
    );
    let events = f.store.recent_events(30).await.unwrap();
    let cleaned = "review round 1: the tree was cleaned for the reviewer: uncommitted \
                   changes discarded: `FIX.md`; untracked files deleted: `NOTES.md`, \
                   `fixtures/`, `out-01.log`, `out-02.log`, `out-03.log`, `out-04.log`, \
                   `out-05.log`, `out-06.log`, `out-07.log` and 3 more";
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.ends_with(cleaned)),
        "{events:?}"
    );
    assert_eq!(pr_body(&bins), "the body", "the description is the body");
    assert_eq!(shipped_commits(&f), ["fix: the real bug"]);
}

/// Issue #20: the fixer's test run leaves an untracked artifact
/// (`.coverage`), which the reviewer's targeted test run writes again. The
/// artifact is deleted before the review, so the reviewer's run finds
/// none. One that writes it afresh has only added a file to the tree it
/// was shown, which changes nothing it reviewed: the file is deleted
/// again, the round's event names it, and the clean verdict ships the fix
/// as reviewed, as it does when the run writes nothing.
#[tokio::test]
async fn a_fixers_artifact_is_gone_before_the_reviewers_test_run() {
    for creates in [false, true] {
        let bins = support::FakeBins::acquire("fix-review-coverage");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-coverage", 7).await;
        let worker = reviewed_worker(vec![Act::FixCovered, Act::CoverageReview(creates)]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
        assert_eq!(seen(&worker.runs()[1]), "?? PR-DESCRIPTION.md\n");
        assert_eq!(pr_body(&bins), "the body", "{creates}: shipped as reviewed");
        let events = f.store.recent_events(30).await.unwrap();
        let removed = "review round 1: the reviewer's uncommitted changes were reverted, and its \
                       verdict stands: untracked files deleted: `.coverage`";
        assert_eq!(
            events
                .iter()
                .any(|e| e.kind == "review" && e.message.ends_with(removed)),
            creates,
            "{events:?}"
        );
        drop(bins);
    }
}

/// A fixer may leave `HEAD` on a branch of its own: what ships is the
/// commit. Undoing a reviewer that only wrote a `.coverage` checks that
/// branch out again, as the reviewer was shown it, and its verdict stands.
#[tokio::test]
async fn a_reviewer_on_the_fixers_own_branch_is_undone_onto_it() {
    let bins = support::FakeBins::acquire("fix-review-own-branch");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-own-branch", 7).await;
    let worker = reviewed_worker(vec![Act::FixOnOwnBranch, Act::CoverageReview(true)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(pr_body(&bins), "the body", "shipped as reviewed");
    assert_eq!(shipped_commits(&f), ["fix: the real bug"]);
    let events = f.store.recent_events(30).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.contains("its verdict stands")),
        "{events:?}"
    );
}

/// Issue #23: a fixer that committed its description and then updated it
/// uncommitted, as `review_followup.md` asks after every round, leaves it
/// tracked and changed (` M PR-DESCRIPTION.md`). That is the description's
/// own status line as much as an untracked one's: the clean keeps it, the
/// reviewer sees the updated description, and every round, the answer's
/// update included, is reviewed and ships with the updated body.
#[tokio::test]
async fn a_committed_then_updated_description_is_reviewed_every_round() {
    let bins = support::FakeBins::acquire("fix-review-tracked-description");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-tracked-description", 3).await;
    let worker = reviewed_worker(vec![
        Act::FixTrackedDescription,
        Act::Review(BLOCKING),
        Act::Answer,
        // Leaves an edit and a file behind: undone, with the tracked,
        // edited description as the restored tree's own line.
        Act::EditingReview,
    ]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 4, "fix, review, answer, review: {summary:?}");
    let described = |run: &support::Run| {
        std::fs::read_to_string(out_path(&run.prompt).with_extension("description")).unwrap()
    };
    assert_eq!(seen(&runs[1]), " M PR-DESCRIPTION.md\n");
    assert_eq!(described(&runs[1]), "the updated body");
    assert_eq!(seen(&runs[3]), " M PR-DESCRIPTION.md\n");
    let answered = "the updated body\n\n## Review\naddressed";
    assert_eq!(described(&runs[3]), answered);
    assert_eq!(pr_body(&bins), answered, "shipped as reviewed");
    let events = f.store.recent_events(30).await.unwrap();
    let reviews: Vec<_> = events.iter().filter(|e| e.kind == "review").collect();
    assert_eq!(reviews.len(), 3, "two rounds and the revert: {events:?}");
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("could not be cleaned")),
        "{events:?}"
    );
    assert!(
        reviews
            .iter()
            .any(|e| e.message.contains("its verdict stands")),
        "{events:?}"
    );
}

/// A reviewer that edits a tracked file and adds an untracked one but
/// commits nothing has left the commits it reviewed alone: both are
/// reverted, the round's event names them, and its clean verdict ships the
/// fix as reviewed. One that commits fails its round
/// (`a_reviewer_that_moves_head_fails_its_review_and_its_commit_does_not_ship`).
#[tokio::test]
async fn a_reviewer_that_edits_a_tracked_file_keeps_its_verdict() {
    let bins = support::FakeBins::acquire("fix-review-edited");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-edited", 7).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::EditingReview]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(pr_body(&bins), "the body", "shipped as reviewed");
    let events = f.store.recent_events(30).await.unwrap();
    let reverted = "review round 1: the reviewer's uncommitted changes were reverted, and its \
                    verdict stands: uncommitted changes discarded: `FIX.md`; untracked files \
                    deleted: `.coverage`";
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.ends_with(reverted)),
        "{events:?}"
    );
}

/// Issue #22: the fixer's build leaves a tracked file rewritten (a stale
/// `Cargo.lock`), the clean step resets it, and the reviewer's test run
/// rewrites it again. The reviewer left the commits alone: the file is
/// reverted after the review, the round's event names it, and the clean
/// verdict ships the fix as reviewed.
#[tokio::test]
async fn a_lock_file_the_reviewers_test_run_rewrites_keeps_its_verdict() {
    let bins = support::FakeBins::acquire("fix-review-relocked");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-relocked", 7).await;
    let worker = reviewed_worker(vec![Act::FixRelocked, Act::RelockingReview]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(seen(&worker.runs()[1]), "?? PR-DESCRIPTION.md\n");
    assert_eq!(pr_body(&bins), "the body", "shipped as reviewed");
    let events = f.store.recent_events(30).await.unwrap();
    let reverted = "review round 1: the reviewer's uncommitted changes were reverted, and its \
                    verdict stands: uncommitted changes discarded: `FIX.md`";
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.ends_with(reverted)),
        "{events:?}"
    );
    assert_eq!(shipped_commits(&f), ["fix: the real bug"]);
}

/// Issue #21: a fixer that builds in a submodule leaves it dirty: at
/// another commit than the tree records, with an edit to a tracked file
/// and an untracked one. `reset` and `clean` do not enter a submodule, so
/// the clean step also puts it back to the recorded commit and cleans it:
/// the reviewer is shown the commits alone, and its clean verdict ships
/// the fix as reviewed.
#[tokio::test]
async fn a_fixers_dirty_submodule_is_cleaned_for_the_reviewer() {
    let bins = support::FakeBins::acquire("fix-review-submodule");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-submodule", 7).await;
    vendor_lib(&f);
    let worker = reviewed_worker(vec![Act::FixSubmoduled, Act::Review(CLEAN)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(worker.runs().len(), 2, "a reviewer ran");
    assert_eq!(seen(&worker.runs()[1]), "?? PR-DESCRIPTION.md\n");
    let events = f.store.recent_events(30).await.unwrap();
    let cleaned = "review round 1: the tree was cleaned for the reviewer: uncommitted \
                   changes discarded: `lib`";
    assert!(
        events
            .iter()
            .any(|e| e.kind == "review" && e.message.ends_with(cleaned)),
        "{events:?}"
    );
    assert_eq!(pr_body(&bins), "the body", "shipped as reviewed");
    assert_eq!(shipped_commits(&f), ["fix: the real bug"]);
}

/// A submodule the clone's shared config marks active (a sibling tree
/// initialized it) but this tree never checked out stays unchecked-out:
/// cleaning the tree for the reviewer clones nothing into it. The tree is
/// kept (the last round's issue holds the fix) to look at.
#[tokio::test]
async fn a_submodule_this_tree_never_checked_out_is_not_cloned_by_the_clean() {
    let bins = support::FakeBins::acquire("fix-review-submodule-elsewhere");
    // A clone the clean may make would succeed, as over the network: only
    // not trying keeps the submodule unchecked-out.
    let env = |key, value| bins.env(key, std::path::Path::new(value));
    let _count = env("GIT_CONFIG_COUNT", "1");
    let _key = env("GIT_CONFIG_KEY_0", "protocol.file.allow");
    let _value = env("GIT_CONFIG_VALUE_0", "always");
    let f = reviewed_fixture("fix-review-submodule-elsewhere", 1).await;
    vendor_lib(&f);
    let worker = reviewed_worker(vec![Act::FixSubmoduleElsewhere, Act::Review(NOT_BLOCKING)]);

    let summary = fix(&f, &worker).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let runs = worker.runs();
    assert_eq!(runs.len(), 2, "a reviewer ran");
    assert_eq!(seen(&runs[1]), "?? PR-DESCRIPTION.md\n");
    let lib = std::fs::read_dir(runs[0].tree.join(LIB))
        .expect("the tree is kept")
        .count();
    assert_eq!(lib, 0, "the submodule stays unchecked-out");
}

/// A reviewer whose build leaves an untracked file in a submodule, or that
/// also moves the submodule to another commit, left the commits it
/// reviewed alone: the submodule is cleaned and put back to the recorded
/// commit, and its verdict stands. Its blocking issues go to the fixer,
/// and the next round's reviewer finds the submodule as recorded.
#[tokio::test]
async fn a_reviewers_changes_to_a_submodule_are_undone() {
    for moves in [false, true] {
        let bins = support::FakeBins::acquire("fix-review-submodule-undone");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-submodule-undone", 7).await;
        vendor_lib(&f);
        let acts = if moves {
            vec![
                Act::FixSubmoduled,
                Act::SubmoduleReview(true, BLOCKING),
                Act::Answer,
                Act::Review(CLEAN),
            ]
        } else {
            vec![Act::FixSubmoduled, Act::SubmoduleReview(false, CLEAN)]
        };
        let worker = reviewed_worker(acts);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
        let runs = worker.runs();
        assert_eq!(runs.len(), if moves { 4 } else { 2 }, "{moves}");
        if moves {
            assert_eq!(seen(&runs[3]), "?? PR-DESCRIPTION.md\n");
        }
        let events = f.store.recent_events(30).await.unwrap();
        let said = |what: &str| {
            events
                .iter()
                .any(|e| e.kind == "review" && e.message.contains(what))
        };
        assert!(
            said(
                "review round 1: the reviewer's uncommitted changes were reverted, and its \
                 verdict stands: uncommitted changes discarded: `lib`"
            ),
            "{moves}: {events:?}"
        );
        assert!(
            !said("the reviewer changed the worktree"),
            "{moves}: {events:?}"
        );
        let body = pr_body(&bins);
        assert!(!body.contains("failed"), "{moves}: {body}");
        drop(bins);
    }
}

/// A fixer leaving a file the tree cannot be cleaned of (`scratch/notes.txt`
/// in a read-only `scratch/`) still has its fix reviewed: the round's event
/// says what is in the way, the reviewer is shown the tree with it, and
/// that leftover is part of the tree it is held to. A leftover never costs
/// the fix its review.
#[tokio::test]
async fn a_tree_that_cannot_be_cleaned_is_reviewed_as_it_is() {
    // A reviewer that leaves changes of its own too: undoing them leaves
    // the tree it was shown, the leftover with it, and its verdict stands.
    for edits in [false, true] {
        let bins = support::FakeBins::acquire("fix-review-uncleaned");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-uncleaned", 7).await;
        let reviewer = if edits {
            Act::EditingReview
        } else {
            Act::Review(CLEAN)
        };
        let worker = reviewed_worker(vec![Act::FixScratched, reviewer]);

        let summary = fix(&f, &worker).await;
        // Before anything can panic: a read-only directory outlives the
        // fixture's cleanup.
        let tree = &worker.runs()[0].tree;
        if tree.join("scratch").exists() {
            scratch_writable(tree, true);
        }

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
        assert_eq!(worker.runs().len(), 2, "the fix is reviewed");
        assert!(seen(&worker.runs()[1]).contains("?? scratch/"));
        assert_eq!(
            pr_body(&bins),
            "the body",
            "edits {edits}: shipped as reviewed"
        );
        let events = f.store.recent_events(30).await.unwrap();
        assert!(
            events.iter().any(|e| e.kind == "review"
                && e.message.contains(
                    "the worktree could not be cleaned for the reviewer: clean failed: warning: \
                     failed to remove scratch/notes.txt"
                )
                && e.message.contains("left in the tree: `?? scratch/`")
                && e.message.ends_with("the reviewer is shown it as it is")),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .any(|e| e.message.contains("its verdict stands")),
            edits,
            "{events:?}"
        );
        assert_eq!(shipped_commits(&f), ["fix: the real bug"]);
        drop(bins);
    }
}

/// What keeps a reviewer's changes from being undone is named in the hold
/// its blocking issue makes: the branch it left (`HEAD` detached, with the
/// index locked so the fix branch cannot be checked out again), a file it
/// left that cannot be deleted, with what git said, or a status git can no
/// longer report.
#[tokio::test]
async fn what_keeps_a_reviewer_from_being_undone_is_named() {
    for (reviewer, named) in [
        (
            Act::StrandingReview(BLOCKING),
            &["HEAD's branch is none (detached)"][..],
        ),
        (
            Act::ScratchingReview(BLOCKING),
            &[
                "clean failed: warning: failed to remove scratch/notes.txt",
                "left in the tree: `?? scratch/`",
            ],
        ),
        (
            Act::UnreadingReview(BLOCKING),
            &["git could not report the status"],
        ),
    ] {
        let bins = support::FakeBins::acquire("fix-review-named");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-named", 7).await;
        let worker = reviewed_worker(vec![Act::Fix, reviewer]);

        let summary = fix(&f, &worker).await;
        // Before anything can panic: a read-only directory outlives the
        // fixture's cleanup.
        let tree = &worker.runs()[0].tree;
        if tree.join("scratch").exists() {
            scratch_writable(tree, true);
        }

        assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
        let report = held(&f).await.expect("held").detail;
        assert!(report.contains("could not be undone"), "{report}");
        for named in named {
            assert!(report.contains(named), "{named}: {report}");
        }
        drop(bins);
    }
}

/// A daemon that dies between the review's hold and its recording leaves
/// the hold's report in the tree, which the resumed attempt keeps (the
/// report holds the work once the attempt ends) while it puts the tree
/// back to the reviewed commit: the report is the review's, not one the
/// reviewer left. So the fix is held on it, and does not ship on a fresh
/// review with the issues never shown to anyone; the operator's `continue`
/// then resumes the fixer on them.
#[tokio::test]
async fn a_review_hold_the_daemon_died_before_recording_survives_the_tree_put_back() {
    let bins = support::FakeBins::acquire("fix-review-locked-died");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-locked-died", 1).await;
    let worker = reviewed_worker(vec![Act::Fix, Act::LockingReview(NOT_BLOCKING)]);
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let tree = worker.runs()[0].tree.clone();
    let reviewed = git(&tree, &["rev-parse", "HEAD^"]).trim().to_owned();
    let report = held(&f).await.expect("held").detail;
    // As if the daemon had died before `conclude_fix` recorded the hold:
    // no hold, the checkpoint the attempt left, and its report in the
    // tree. The index lock is gone with the reviewer that held it.
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("DELETE FROM work_holds WHERE finding_id = ?1")
        .bind(f.fid)
        .execute(&pool)
        .await
        .unwrap();
    std::fs::write(tree.join("BLOCKED.md"), &report).unwrap();
    std::fs::remove_file(index_lock(&tree)).unwrap();
    let plan = pending_resume(&f).await;
    assert_eq!(plan.after_hold(), None, "no operator released anything");
    let resumed = reviewed_worker(vec![Act::Continue, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    assert_eq!(bins.calls_to("gh").len(), 0, "a held fix never ships");
    let hold = held(&f).await.expect("held on the review's report");
    assert_eq!(
        (hold.reason, hold.detail.as_str()),
        (HoldReason::Prerequisite, report.as_str())
    );
    assert_eq!(
        git(&tree, &["rev-parse", "HEAD"]).trim(),
        reviewed,
        "the tree was put back all the same"
    );
    let issue = "the test misses the empty input";
    let runs = resumed.runs();
    assert_eq!(runs.len(), 1, "no review of a held fix");
    assert!(
        !runs[0].prompt.contains(issue),
        "the issues go to the operator first: {}",
        runs[0].prompt
    );
    // The operator's `continue` then has the fixer answer them.
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Fix,
        ReleaseAction::Continue,
    )
    .await;
    let plan = pending_resume(&f).await;
    let answering = reviewed_worker(vec![Act::Answer, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &answering).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let prompt = &answering.runs()[0].prompt;
    assert!(prompt.contains(issue), "{prompt}");
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

/// A report concludes a fix owing issues from before it as any other:
/// nothing owed carries over, and the operator's `continue` reviews from
/// round 1, with no answer to the old issues.
#[tokio::test]
async fn a_report_drops_the_issues_owed_before_it() {
    let bins = support::FakeBins::acquire("fix-review-owed-blocked");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let f = reviewed_fixture("fix-review-owed-blocked", 3).await;
    let worker =
        reviewed_worker(vec![Act::Fix, Act::ReviewKilled(BLOCKING)]).granting(Some(10_000));
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    let plan = released_resume(&f).await;
    let blocked = reviewed_worker(vec![Act::Report(
        "BLOCKED.md",
        "needs the vendor's spec",
        None,
    )]);
    let summary = resume(&f, &plan, &blocked).await;
    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");

    continue_reviews_from_round_1(&f).await;
}

/// A curator killed at its cap in the middle of a rebase leaves none in
/// progress once its tree is put back: the owed curation's own rebase
/// would fail on it.
#[tokio::test]
async fn a_curator_killed_mid_rebase_leaves_no_rebase_behind() {
    let bins = support::FakeBins::acquire("fix-curate-killed-rebase");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-curate-killed-rebase", 7).await;
    f.cfg.review_curate = true;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(Curation::KilledMidRebase),
    ]);
    let summary = fix(&f, &worker).await;
    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    let tree = worker.runs()[0].tree.clone();
    let rebase = git(&tree, &["rev-parse", "--git-path", "rebase-merge"]);
    let rebase = tree.join(rebase.trim());
    assert!(!rebase.exists(), "{} left behind", rebase.display());
    let plan = pending_resume(&f).await;
    let resumed = reviewed_worker(vec![Act::Curate(Curation::Squash)]);

    let summary = resume(&f, &plan, &resumed).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(shipped_commits(&f), ["fix: the real bug, curated"]);
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
      {"kind": "follow-up", "blocking": false, "summary": "no place given"},
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
    assert!(
        body.contains(
            "- [blocking] **the other feeds parse dates the same broken way** (`src/feed.rs`)"
        ),
        "no line, so only its file: {body}"
    );
    assert!(
        body.contains("- **no place given**\n"),
        "no place at all: {body}"
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

/// A curation that changes the tree, or anything besides the history (the
/// description, an untracked file), or a curator that ends early for any
/// reason but its grant (a failed run, a wall-clock kill), is thrown away:
/// the branch goes back to the reviewed commit and the original commits
/// ship. Only a grant that runs out owes the curation. It costs the fix
/// nothing else.
#[tokio::test]
async fn a_curation_that_changes_the_tree_ships_the_original_commits() {
    for how in [
        Curation::ChangeTree,
        Curation::EditsDescription,
        Curation::LeavesFile,
        Curation::Ends(Ending::Failed),
        Curation::Ends(Ending::Wallclock),
    ] {
        let (f, worker, bins) = curated("fix-curate-rejected", how).await;

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
        assert_eq!(
            pr_body(&bins),
            "the body\n\n## Review\naddressed",
            "{how:?}"
        );
        assert!(
            !worker.runs()[0].tree.join("curation.log").exists(),
            "{how:?}"
        );
        drop(bins);
    }
}

/// A grant that runs out before the curation, or on the curator (its
/// rewrite undone), suspends the fix instead of shipping it uncurated. The
/// resume, without a fixer run or another review, curates and ships.
#[tokio::test]
async fn a_curation_the_grant_cut_short_is_had_by_the_resume() {
    for (case, curation, grant) in [
        ("spent before the curation", None, 4_000),
        ("spent by the curator", Some(Curation::Killed), 10_000),
    ] {
        let bins = support::FakeBins::acquire("fix-curate-spent");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let mut f = reviewed_fixture("fix-curate-spent", 7).await;
        f.cfg.review_curate = true;
        let mut acts = vec![
            Act::Fix,
            Act::Review(BLOCKING),
            Act::Answer,
            Act::Review(CLEAN),
        ];
        acts.extend(curation.map(Act::Curate));
        let ran = acts.len();
        let worker = reviewed_worker(acts).granting(Some(grant));

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{case}");
        assert_eq!(worker.runs().len(), ran, "{case}");
        assert!(bins.calls_to("gh").is_empty(), "{case}: nothing ships");
        let plan = pending_resume(&f).await;
        let resumed = reviewed_worker(vec![Act::Curate(Curation::Squash)]);

        let summary = resume(&f, &plan, &resumed).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{case}");
        let runs = resumed.runs();
        assert_eq!(runs.len(), 1, "{case}: only the curation");
        assert!(runs[0].prompt.starts_with("curate"), "{case}");
        assert_eq!(
            shipped_commits(&f),
            ["fix: the real bug, curated"],
            "{case}"
        );
        assert_eq!(pr_title(&bins), "fix: the real bug", "{case}");
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
/// on the retry without another review or curation, or any run at all:
/// the saved verdict is for the tree as curated.
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
    let retry = reviewed_worker(vec![]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert!(retry.runs().is_empty(), "no run, review or curation again");
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

/// The attempt after a curation whose rewrite could not be undone fails
/// as `reviewed tree not restored` without running a worker while what
/// keeps the tree from being put back is there; once it is gone, the
/// tree goes back to the reviewed commit and the original commits ship.
/// The runs of that attempt.
async fn an_unrestorable_curation_is_put_back_first(
    f: &Fixture,
    tree: &std::path::Path,
    attempt: &[Act],
) -> Vec<support::Run> {
    let plan = pending_resume(f).await;
    let retry = reviewed_worker(attempt.to_vec());

    let blocked = resume(f, &plan, &retry).await;

    assert!(
        blocked
            .failure
            .as_deref()
            .is_some_and(|r| r.starts_with("reviewed tree not restored")),
        "{blocked:?}"
    );
    assert!(
        retry.runs().is_empty(),
        "no worker runs on the curator's rewrite"
    );
    std::fs::remove_file(index_lock(tree)).unwrap();
    let plan = pending_resume(f).await;

    let summary = resume(f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(
        shipped_commits(f),
        ["fix: the sibling parser too", "fix: the real bug"],
        "the curator's rewrite never ships"
    );
    retry.runs()
}

/// A rejected curation whose rewrite cannot be undone pins the reviewed
/// commit for its ship; when that publish fails, the retry puts the tree
/// back to it before anything runs, and neither reviews nor ships the
/// rewrite.
#[tokio::test]
async fn a_rejected_curation_that_could_not_be_undone_is_put_back_on_the_retry() {
    let bins = support::FakeBins::acquire("fix-curate-locked");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-curate-locked").await;
    review_on(&mut f, 7);
    f.cfg.review_curate = true;
    let nowhere = f.cfg.work_root.join("refusing.git");
    route_push(&f, Some(&nowhere.to_string_lossy()));
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(Curation::LockedChange),
    ]);
    let refused = fix(&f, &worker).await;
    assert_eq!(refused.outcome.as_deref(), Some("requeued"), "{refused:?}");
    let tree = worker.runs()[0].tree.clone();
    let key = format!("url.{}.insteadOf", nowhere.display());
    git(&f.repo_dir, &["config", "--unset-all", &key]);
    route_push(&f, None);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE jobs SET finished_at = finished_at - ?1")
        .bind(24 * 60 * 60_000_i64)
        .execute(&pool)
        .await
        .unwrap();

    let runs = an_unrestorable_curation_is_put_back_first(&f, &tree, &[Act::Continue]).await;
    assert!(runs.is_empty(), "the pinned commit's publish is retried");
}

/// A curation the daemon died in whose rewrite cannot be undone after the
/// restart is not resumed on as undone: no fixer runs on the rewrite until
/// the tree is back at the reviewed commit, and the one that then resumes
/// is told its rewrite was undone, not to pick it back up.
#[tokio::test]
async fn a_curation_the_daemon_died_in_that_could_not_be_undone_is_put_back_first() {
    use futures_util::FutureExt;
    let bins = support::FakeBins::acquire("fix-curate-died-locked");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = reviewed_fixture("fix-curate-died-locked", 7).await;
    f.cfg.review_curate = true;
    let worker = reviewed_worker(vec![
        Act::Fix,
        Act::Review(BLOCKING),
        Act::Answer,
        Act::Review(CLEAN),
        Act::Curate(Curation::LockedDaemonDies),
    ]);
    let died = std::panic::AssertUnwindSafe(fix(&f, &worker))
        .catch_unwind()
        .await;
    assert!(died.is_err(), "the daemon died");
    let tree = worker.runs()[0].tree.clone();
    support::jobs(&f.store)
        .settle_unsettled(&f.cfg.work_root)
        .await
        .unwrap();

    let runs = an_unrestorable_curation_is_put_back_first(&f, &tree, &[Act::Continue]).await;
    assert_eq!(runs.len(), 1, "no new review, no new curation");
    assert!(
        runs[0].prompt.contains("has been undone"),
        "{}",
        runs[0].prompt
    );
    let events = f.store.recent_events(40).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("the tree a curator left was put back")),
        "{events:?}"
    );
}

/// A saved verdict covers the tree it was reached on and nothing else: a
/// fixer that commits more on a released retry of an offline push is
/// reviewed again.
#[tokio::test]
async fn a_saved_verdict_does_not_cover_a_tree_that_changed_since() {
    let bins = support::FakeBins::acquire("fix-review-saved-stale");
    bins.ok("gh", "https://github.com/acme/widget/pull/42");
    let mut f = fixture("fix-review-saved-stale").await;
    review_on(&mut f, 7);
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
    let plan = released_resume(&f).await;
    assert!(plan.anticipated() > 0, "a run that resumes is reserved for");
    let retry = reviewed_worker(vec![Act::FixMore, Act::Review(CLEAN)]);

    let summary = resume(&f, &plan, &retry).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    assert_eq!(retry.runs().len(), 2, "the changed tree is reviewed again");
}

/// A verdict saved by a daemon from before this change still covers its
/// unchanged tree: a fix kept only to publish across the upgrade ships on
/// it, with no run, as it would have before. That daemon saved the tree
/// as its commit, branch, `git status` (trimmed at both ends, so a first
/// line's leading blank was lost, as for a tracked, edited description)
/// and description digest.
#[tokio::test]
async fn a_verdict_saved_by_an_earlier_daemon_still_covers_its_tree() {
    for (label, fixer, status) in [
        ("fix-review-saved-legacy", Act::Fix, None),
        (
            "fix-review-saved-legacy-tracked",
            Act::FixTrackedDescription,
            Some(" M PR-DESCRIPTION.md"),
        ),
    ] {
        let bins = support::FakeBins::acquire(label);
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let mut f = fixture(label).await;
        review_on(&mut f, 7);
        f.cfg.network_gate = true;
        let nowhere = f.cfg.work_root.join("no-such-remote.git");
        route_push(&f, Some(&nowhere.to_string_lossy()));
        let host = "github.com";
        let worker = reviewed_worker(vec![fixer, Act::Review(CLEAN)]).offline(&[host]);
        let summary = fix(&f, &worker).await;
        assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
        let nowhere_key = format!("url.{}.insteadOf", nowhere.display());
        git(&f.repo_dir, &["config", "--unset-all", &nowhere_key]);
        route_push(&f, None);
        f.cfg.network_gate = false;
        let plan = pending_resume(&f).await;
        // As the earlier daemon wrote it: those four fields, nothing else,
        // the status trimmed at both ends.
        let path = plan.workspace().root.join("review-ending.json");
        let mut saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let tree = &saved["tree"];
        let mut legacy: serde_json::Map<String, serde_json::Value> =
            ["head", "branch", "status", "description"]
                .into_iter()
                .map(|key| (key.to_owned(), tree[key].clone()))
                .collect();
        assert!(legacy["head"].is_string(), "{saved}");
        let kept = legacy["status"].as_str().expect("a status").to_owned();
        if let Some(status) = status {
            assert_eq!(kept, status, "the status this case is about");
        }
        legacy.insert("status".to_owned(), kept.trim().into());
        saved["tree"] = legacy.into();
        std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let retry = reviewed_worker(vec![]);

        let summary = resume(&f, &plan, &retry).await;

        assert_eq!(
            summary.outcome.as_deref(),
            Some("pr_open"),
            "{label}: {summary:?}"
        );
        assert!(retry.runs().is_empty(), "{label}: no run, no review again");
        drop(bins);
    }
}

/// While the review is on, a finished fixer with nothing to review fails
/// its attempt with what it lacks, as a fix with nothing to ship does,
/// and no reviewer runs. Its job is failed with that as its notes:
/// `done` is for an attempt that reached its outcome.
#[tokio::test]
async fn a_fix_with_nothing_to_review_fails_with_what_it_lacks() {
    for (commits, describe, failure) in [
        (false, true, "no commits"),
        (true, false, "no PR-DESCRIPTION.md"),
    ] {
        let bins = support::FakeBins::acquire("fix-review-nothing");
        let f = reviewed_fixture("fix-review-nothing", 7).await;
        let worker = ScriptedBackend::new(move |tree| {
            if commits {
                std::fs::write(tree.join("FIX.md"), "fixed\n").unwrap();
                git(tree, &["add", "FIX.md"]);
                git(tree, &["commit", "-m", "fix: the real bug"]);
            }
            if describe {
                std::fs::write(tree.join("PR-DESCRIPTION.md"), "the body").unwrap();
            }
            support::done_with_transcript(tree)
        });

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.failure.as_deref(), Some(failure), "{summary:?}");
        assert_eq!(worker.runs().len(), 1, "{failure}: no reviewer");
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db),
        )
        .await
        .unwrap();
        let job: (String, Option<String>) =
            sqlx::query_as("SELECT state, notes FROM jobs WHERE id = ?1")
                .bind(summary.job_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(job, ("failed".to_owned(), Some(failure.to_owned())));
        drop(bins);
    }
}

/// A round with no review to read fails, and the PR says why: what the
/// reviewer left (no file, nothing but entries without a summary) when it
/// finished, how it ended when it did not.
#[tokio::test]
async fn a_round_without_a_readable_review_ships_saying_why() {
    for (reviewer, why) in [
        (Act::ReviewSilent, "Review round 1 failed: no review file"),
        (
            Act::ReviewUnreached,
            "Review round 1 failed: the reviewer exited 1.",
        ),
        (
            Act::Review(r#"[{"title": "no summary"}]"#),
            "Review round 1 failed: 1 review entries had no summary.",
        ),
    ] {
        let bins = support::FakeBins::acquire("fix-review-unreadable-file");
        bins.ok("gh", "https://github.com/acme/widget/pull/42");
        let f = reviewed_fixture("fix-review-unreadable-file", 7).await;
        let worker = reviewed_worker(vec![Act::Fix, reviewer]);

        let summary = fix(&f, &worker).await;

        assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{why}");
        let body = pr_body(&bins);
        assert!(body.contains(why), "{why}: {body}");
        drop(bins);
    }
}
