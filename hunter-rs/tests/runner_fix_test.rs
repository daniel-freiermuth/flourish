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
use hunter::domain::ForgeName;
use hunter::store::FindingInsert;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, fresh_store, git};

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

/// [`fixture`] for a repo registered at `url` on `forge`.
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
    let mut cfg = Config::load(dir.path()).expect("load config");
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

// ---------------------------------------------------------------------------
// Shipping into an MR that already exists
// ---------------------------------------------------------------------------

const GL_REPO_URL: &str = "https://gitlab.com/acme/widget";
const GL_MR_URL: &str = "https://gitlab.com/acme/widget/-/merge_requests/12";

/// A worker that ships: one commit on the fix branch and a PR description.
fn shipping_worker() -> ScriptedBackend {
    ScriptedBackend::new(|cwd| {
        std::fs::write(cwd.join("FIX.md"), "fixed\n").unwrap();
        git(cwd, &["add", "FIX.md"]);
        git(cwd, &["commit", "-m", "fix: a real bug"]);
        std::fs::write(cwd.join("PR-DESCRIPTION.md"), "the fix\n").unwrap();
        support::done()
    })
}

/// A re-run of a fix whose MR is still open from an earlier attempt: the
/// push lands, `glab mr create` is refused because the branch already has
/// an open MR, and that MR is the fix's MR. GitLab names it only by
/// reference (`!12`, from `MergeRequest#conflicting_mr_message`), never by
/// URL, so the finding must end `pr_open` on that MR — the same recovery
/// a `gh` "already exists" error gets. Requeueing instead fails the same
/// way on every attempt until the streak rejects the finding as stuck,
/// with its MR still open.
#[tokio::test]
async fn gitlab_mr_that_already_exists_is_recovered() {
    let bins = FakeBins::acquire("fix-gl-mr-exists");
    bins.script(
        "glab",
        &format!(
            "case \"$1 $2\" in\n\
             'mr create') echo 'POST https://gitlab.com/api/v4/projects/acme%2Fwidget/merge_requests: 409 {{message: [Another open merge request already exists for this source branch: !12]}}' >&2; exit 1;;\n\
             'mr view') echo '{{\"iid\":12,\"state\":\"opened\",\"source_branch\":\"{BRANCH}\",\"web_url\":\"{GL_MR_URL}\"}}'; exit 0;;\n\
             esac\n\
             exit 1"
        ),
    );
    let f = fixture_on("fix-gl-mr-exists", GL_REPO_URL, ForgeName::Gitlab).await;
    // `run_fix` pushes to the forge's SSH URL; point it at the local origin.
    let origin = git(&f.repo_dir, &["remote", "get-url", "origin"]);
    git(
        &f.repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", origin.trim()),
            "git@gitlab.com:acme/widget.git",
        ],
    );

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &shipping_worker(), None)
        .await
        .unwrap();

    assert_eq!(
        summary.outcome.as_deref(),
        Some("pr_open"),
        "the existing MR must be adopted, not retried: {summary:?}"
    );
    assert_eq!(summary.pr_url.as_deref(), Some(GL_MR_URL));
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, hunter::domain::FindingStatus::PrOpen);
    assert_eq!(after.pr_url.as_deref(), Some(GL_MR_URL));
    assert_eq!(
        after.fix_attempts, 0,
        "a recovered MR is not a failed attempt"
    );
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

    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the checkpoint must still resume");
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
}

/// Stage an implemented fix plus durable reports and its metered transcript.
async fn blocked_fixture() -> (
    Fixture,
    ScriptedBackend,
    hunter::scheduler::CycleSummary,
    String,
) {
    let f = fixture("fix-blocked-checkpoint").await;
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

/// Blocked is not suppressed or automatically retried, and a sweep preserves
/// its committed implementation and uncommitted verification reports. The
/// report moves to the job row, so the tree holds no stale `BLOCKED.md`.
#[tokio::test]
async fn blocked_fix_keeps_checkpoint_and_requeues_without_losing_commits() {
    let (f, blocked, summary, expected_reason) = blocked_fixture().await;
    assert_eq!(summary.outcome.as_deref(), Some("blocked"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "blocked");
    let events = f.store.recent_events(20).await.unwrap();
    assert!(
        events.iter().any(|e| e
            .message
            .starts_with(&format!("#{} blocked; checkpoint retained at", f.fid))),
        "{events:?}"
    );
    assert_eq!(shown_blocker(&f).await, Some(expected_reason.clone()));
    assert_eq!(finding.verdict_reason, None, "the report is not copied");
    let tree = blocked.runs()[0].tree.clone();
    assert_eq!(
        std::fs::read_to_string(tree.join("candidate.txt")).unwrap(),
        "implemented fix\n"
    );
    assert!(!tree.join("BLOCKED.md").exists());
    assert_eq!(
        f.store
            .job_blocker(summary.job_id.unwrap())
            .await
            .unwrap()
            .as_deref(),
        Some(expected_reason.as_str())
    );
    assert!(tree.join("PR-DESCRIPTION.md").is_file());
    assert!(f.store.suppressions(1, "bug").await.unwrap().is_empty());
    assert!(!matches!(
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap(),
        Some(hunter::scheduler::Candidate::Resume { .. })
    ));
    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(tree.is_dir(), "blocked checkpoint must survive a sweep");
}

/// Requeue continues the original commits. Neither a budget denial nor a
/// provider failure can erase the checkpoint or turn it into a rejection.
#[tokio::test]
async fn requeued_blocked_fix_preserves_work_across_provider_failures() {
    let (f, blocked, summary, expected_reason) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    let original_head = git(&tree, &["rev-parse", "HEAD"]);
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued blocked work must resume, not start cold");
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let denied = ScriptedBackend::noop().denying_above(0);
    let result = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &denied, Some(&plan))
        .await
        .unwrap();
    assert!(result.denied.is_some());
    assert_eq!(
        blocker_of(&f, Some(plan.predecessor_id)).await,
        Some(expected_reason.clone()),
        "budget denial must leave the blocker untouched"
    );
    // A report left in the tree by a daemon that died between recording it
    // and deleting it is the old blocker, not this attempt's outcome.
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
    assert_eq!(failed.outcome.as_deref(), Some("blocked"));
    assert_eq!(
        blocker_of(&f, failed.job_id).await,
        Some(expected_reason.clone())
    );
    assert_eq!(shown_blocker(&f).await, Some(expected_reason.clone()));
    assert_eq!(git(&tree, &["rev-parse", "HEAD"]), original_head);
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a failed provider handoff must preserve the requeued checkpoint");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    let unblocked = ScriptedBackend::staged(move |tree, session| {
        assert!(
            session.is_some(),
            "unblocked worker needs the paid-for transcript"
        );
        assert_eq!(git(tree, &["rev-parse", "HEAD"]), original_head);
        assert_eq!(
            std::fs::read_to_string(tree.join("candidate.txt")).unwrap(),
            "implemented fix\n"
        );
        assert!(
            !tree.join("BLOCKED.md").exists(),
            "stale blocker must not override a new result"
        );
        // A new prerequisite is a new block, not a rejection or cold retry.
        std::fs::write(tree.join("BLOCKED.md"), "External peer still missing").unwrap();
        let mut result = support::done();
        result.session_file = session.map(|path| path.to_string_lossy().into_owned());
        result
    });
    let result = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &unblocked, Some(&plan))
        .await
        .unwrap();
    assert_eq!(result.outcome.as_deref(), Some("blocked"));
    assert_eq!(
        shown_blocker(&f).await.as_deref(),
        Some("External peer still missing")
    );
    assert!(tree.is_dir());
}

/// A daemon that dies mid-resume must not lose the blocker: once startup
/// reconciliation suspends the orphan, the next resume inherits it from the
/// job row, and an attempt that fails without resolving the prerequisite
/// holds the finding with the original report again.
#[tokio::test]
async fn interrupted_blocked_resume_keeps_the_report_for_the_next_resume() {
    let (f, blocked, _, expected_reason) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued blocked work must resume");
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
    assert_eq!(failed.outcome.as_deref(), Some("blocked"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "blocked");
    assert_eq!(shown_blocker(&f).await, Some(expected_reason.clone()));
    assert_eq!(
        blocker_of(&f, failed.job_id).await,
        Some(expected_reason.clone())
    );
    assert!(
        !tree.join("BLOCKED.md").exists(),
        "the restored report lives on the job row"
    );
}

/// The blocker recorded on `job`'s row.
async fn blocker_of(f: &Fixture, job: Option<i64>) -> Option<String> {
    f.store.job_blocker(job.unwrap()).await.unwrap()
}

/// The report the API shows on the fixture finding's Blocked card.
async fn shown_blocker(f: &Fixture) -> Option<String> {
    f.store.held_fix_blockers().await.unwrap().remove(&f.fid)
}

/// The inherited blocker is restored only for an attempt that ended with
/// no outcome. A worker that finished (here without a PR description), or
/// one that declined the finding even while failing, concluded something,
/// and the old blocker must not override it.
#[tokio::test]
async fn a_blocked_resume_that_concludes_is_not_blocked_again() {
    for (case, expected) in [("finished", "requeued"), ("declined", "rejected")] {
        let (f, _blocked, _, _) = blocked_fixture().await;
        f.store
            .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
            .await
            .unwrap();
        let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
            hunter::scheduler::pick_next(&f.store, &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("requeued blocked work must resume");
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
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_ne!(finding.status.as_str(), "blocked", "{case}");
    }
}

/// Identical failures are retried until the streak limit, then the work
/// is held as blocked with its tree, rather than rejected.
#[tokio::test]
async fn repeated_identical_fix_failures_become_blocked_at_the_limit() {
    let f = fixture("fix-stuck").await;
    let empty = ScriptedBackend::noop();
    let mut outcomes = Vec::new();
    let mut last_job = None;
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &empty, None)
            .await
            .unwrap();
        assert_eq!(summary.failure.as_deref(), Some("no commits"));
        outcomes.push(summary.outcome.unwrap());
        last_job = summary.job_id;
    }
    assert_eq!(outcomes, ["requeued", "requeued", "blocked"]);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "blocked");
    let tree = empty.runs()[2].tree.clone();
    assert!(
        !tree.join("BLOCKED.md").exists(),
        "the report lives on the job row"
    );
    let shown = shown_blocker(&f).await;
    assert!(
        shown
            .as_deref()
            .is_some_and(|r| r.contains("3 consecutive fix attempts hit the same failure"))
    );
    assert_eq!(f.store.job_blocker(last_job.unwrap()).await.unwrap(), shown);
}

/// The prompt a requeued held fix resumes with: requeue it, resume its
/// checkpoint once, and return what the worker was told.
async fn resumed_hold_prompt(f: &Fixture) -> String {
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued held work must resume");
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

/// A held fix resumes against what held it. A worker's `BLOCKED.md` names
/// a prerequisite to re-evaluate. A failure streak names none, only what
/// kept failing, so its resume asks the worker to finish: told to
/// re-evaluate a prerequisite and recreate `BLOCKED.md`, it could hold
/// the fix again over nothing, without any progress.
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
    let streak = resumed_hold_prompt(&f).await;
    let (reported, _, _, _) = blocked_fixture().await;
    let report = resumed_hold_prompt(&reported).await;

    assert!(
        streak.contains("3 consecutive fix attempts hit the same failure: no PR-DESCRIPTION.md"),
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
/// attempt's outcome), and holds the checkpoint blocked again: still the
/// chain's resumable tip once the operator clears the obstacle.
#[tokio::test]
async fn unremovable_stale_report_holds_the_checkpoint_blocked() {
    let (f, blocked, _, _) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued blocked work must resume");
    };
    let jobs_before = f.store.jobs_by_finding(f.fid).await.unwrap().len();
    std::fs::create_dir(tree.join("BLOCKED.md")).unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("must not run beside a stale report"));
    let summary = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &never, Some(&plan))
        .await
        .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("blocked"), "{summary:?}");
    assert_eq!(
        summary.kind,
        Some(hunter::domain::FindingJobKind::Fix.into())
    );
    assert_eq!(summary.finding_id, Some(f.fid));
    assert_eq!(summary.state, Some(hunter::domain::JobState::Suspended));
    assert!(
        summary
            .failure
            .as_deref()
            .is_some_and(|m| m.starts_with("cannot remove the stale ")),
        "{summary:?}"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.status.as_str(), "blocked");
    assert_eq!(
        f.store.jobs_by_finding(f.fid).await.unwrap().len(),
        jobs_before,
        "no successor was created"
    );

    std::fs::remove_dir(tree.join("BLOCKED.md")).unwrap();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan: again, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the held checkpoint must still resume");
    };
    assert_eq!(again.predecessor_id, plan.predecessor_id);
}

/// Rejecting a blocked fix ends its checkpoint: the next sweep retires the
/// suspension and releases its tree, instead of holding both forever.
#[tokio::test]
async fn rejecting_a_blocked_fix_lets_the_sweep_release_its_checkpoint() {
    let (f, blocked, summary, _) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    let job = summary.job_id.unwrap();

    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(tree.is_dir(), "a blocked checkpoint is held");

    f.store
        .set_finding_verdict(
            f.fid,
            hunter::domain::FindingStatus::Rejected,
            "not worth it",
        )
        .await
        .unwrap();
    hunter::workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();
    assert!(!tree.exists(), "the rejected checkpoint's tree is released");
    let jobs = f.store.jobs_by_finding(f.fid).await.unwrap();
    let retired = jobs.iter().find(|j| j.id == job).unwrap();
    assert_eq!(retired.state, hunter::domain::JobState::Killed);
    assert_eq!(retired.killed_reason.as_deref(), Some("finding-moved"));
}

/// A blocked resume whose backend fails before any worker runs leaves no
/// transcript of its own. The checkpoint must still resume on the next
/// requeue, continuing the predecessor's transcript, not start the fix cold.
#[tokio::test]
async fn a_blocked_resume_whose_backend_fails_stays_resumable() {
    let (f, blocked, summary, expected_reason) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    let transcript = tree.parent().unwrap().join("session/session.jsonl");
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued blocked work must resume");
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let failing = ScriptedBackend::failing();
    let err = hunter::scheduler::run_fix(&f.store, &f.cfg, &finding, &failing, Some(&plan))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("backend failed"), "{err}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status.as_str(), "blocked");
    assert_eq!(shown_blocker(&f).await, Some(expected_reason));

    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the checkpoint must still resume after a failed backend");
    };
    assert_ne!(
        plan.predecessor_id,
        summary.job_id.unwrap(),
        "the failed attempt is the link"
    );
    assert_eq!(plan.session_file, transcript);
    assert_eq!(
        git(&tree, &["log", "-1", "--format=%s"]).trim(),
        "implemented candidate"
    );
}

/// Whatever bytes a worker writes into its report are the report: a
/// `BLOCKED.md` or `NOT-A-BUG.md` that is not valid UTF-8 still blocks or
/// declines, its text read lossily.
#[tokio::test]
async fn a_report_that_is_not_utf8_still_counts() {
    for (file, expected, status) in [
        ("BLOCKED.md", "blocked", "blocked"),
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
            f.store.job_blocker(summary.job_id.unwrap()).await.unwrap()
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

/// A verdict given after `pick_next` wins over the stale-report hold too:
/// the resume is skipped at the claim, and the finding keeps the
/// operator's status instead of being forced back to `blocked`.
#[tokio::test]
async fn a_verdict_after_selection_wins_over_the_stale_report_hold() {
    let (f, blocked, _, _) = blocked_fixture().await;
    let tree = blocked.runs()[0].tree.clone();
    f.store
        .set_finding_status(f.fid, hunter::domain::FindingStatus::Queued)
        .await
        .unwrap();
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("requeued blocked work must resume");
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

/// Recording a block can fail (a busy database); the transaction then
/// leaves the finding at `fixing`, where nothing would pick it up again
/// before a restart. The fix puts it back to `queued` instead.
#[tokio::test]
async fn a_block_that_cannot_be_recorded_returns_the_finding_to_queued() {
    let f = fixture("fix-block-not-recorded").await;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::raw_sql(
        "CREATE TRIGGER refuse_block BEFORE UPDATE OF blocker ON jobs \
         WHEN NEW.blocker IS NOT NULL BEGIN SELECT RAISE(ABORT, 'injected'); END;",
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
        .record_fix_attempt(f.fid, "no commits")
        .await
        .unwrap();

    let summary = fix(&f, &committing(true)).await;

    assert_eq!(summary.outcome.as_deref(), Some("pr_open"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.fix_attempts, 0, "a shipped fix ends the streak");
    assert_eq!(after.last_fix_failure, None);
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
    assert_eq!(after.fix_attempts, 1);
    assert_eq!(after.last_fix_failure.as_deref(), Some(failure.as_str()));
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
        assert_eq!(after.last_fix_failure.as_deref(), Some(reason), "{label}");
    }
}

/// A different failure restarts the streak, so alternating failures are
/// retried rather than held as blocked at the limit.
#[tokio::test]
async fn a_different_failure_restarts_the_streak() {
    let f = fixture("fix-streak-reset").await;
    for _ in 0..2 {
        f.store
            .record_fix_attempt(f.fid, "no commits")
            .await
            .unwrap();
    }

    let summary = fix(&f, &committing(false)).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, hunter::domain::FindingStatus::Queued);
    assert_eq!(after.fix_attempts, 1);
}
