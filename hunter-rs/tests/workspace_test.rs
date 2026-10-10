#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Per-chain workspaces: where a job runs, and what may remove it.
//!
//! Every job chain (a cold job plus the attempts that resume it) runs in
//! `<work_root>/jobs/<origin>/{tree,session}`. These tests pin the rules
//! that make that safe, against real git in scratch directories:
//! a resume runs in its origin's tree and session untouched; nothing
//! removes the tree of a running or suspended chain; a hunt's watermark is
//! the commit its chain was pinned at; a fresh fix can take over a
//! suspended fix's branch; the previous layout's leftovers go unless a
//! live job still uses them; and a cold job whose tree cannot be made
//! fails without leaving anything behind.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hunter::backend::{Backend, JobClass, Outlook, Verdict};
use hunter::config::Config;
use hunter::domain::{FindingStatus, JobState};
use hunter::scheduler::{Candidate, pick_next, run_fix, run_hunt, run_test_gap};
use hunter::store::{FindingInsert, Store};
use hunter::types::RunResult;
use hunter::workspace::{self, SESSION_RETENTION_MS, TreeSpec, Workspace};
use sqlx::SqlitePool;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, git};

/// One usage record in omp's session format.
const USAGE: &str = r#"{"message":{"role":"assistant","usage":{"input":1000,"output":100,"cacheRead":0,"cacheWrite":500}}}"#;

struct Fixture {
    cfg: Config,
    pool: SqlitePool,
    store: Store,
    repo: GitRepo,
    /// The clone registered as repo 1 (`repos/repo-1`).
    clone: PathBuf,
    /// Last, so the directory outlives the SQLite handles above.
    dir: TempDir,
}

/// A real clone of a bare origin, registered as repo 1, with stub
/// playbooks that render with the slots each builder supplies.
async fn fixture(label: &str, default_branch: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let (path, pool) = support::fresh_pool(&dir, "ws").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', ?3, 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(default_branch)
    .execute(&pool)
    .await
    .unwrap();
    let store = Store::connect(&path).await.unwrap();

    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("hunt.md"),
        "hunt {{REPO_PATH}} {{DIFF_RANGE}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    std::fs::write(
        playbooks.join("test_gap.md"),
        "gaps {{REPO_PATH}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        pool,
        store,
        repo,
        clone,
        dir,
    }
}

/// Git changes directory to the clone before adding a worktree. A relative
/// project root must not relocate that tree away from the worker's cwd.
#[tokio::test]
async fn relative_project_root_creates_tree_at_worker_cwd() {
    let _bins = FakeBins::acquire("ws-relative-root");
    let f = fixture("ws-relative-root", "main").await;
    let project = f.dir.subdir("hunter");
    // nextest gives this test its own process, so the cwd cannot race others.
    std::env::set_current_dir(f.dir.path()).unwrap();
    let cfg = Config::load(Path::new("hunter")).unwrap();
    let ws = Workspace::for_chain(&cfg.work_root, &f.clone, 1);
    let pinned = workspace::create(
        &ws,
        &TreeSpec::Branch {
            name: "fix/relative-root".to_owned(),
            at: "origin/main".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(git(&ws.tree, &["rev-parse", "HEAD"]).trim(), pinned);
    assert_eq!(
        std::fs::read_to_string(project.join("data/jobs/1/tree/README.md")).unwrap(),
        "seed\n",
    );
}

async fn job_state(pool: &SqlitePool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, killed_reason, notes FROM jobs WHERE id = ?1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Push one more commit to origin from a scratch clone and return it: the
/// default branch moving while a chain is suspended.
fn advance_origin(dir: &TempDir, origin: &Path) -> String {
    let pusher = dir.join("pusher");
    git(
        dir.path(),
        &[
            "clone",
            origin.to_string_lossy().as_ref(),
            pusher.to_string_lossy().as_ref(),
        ],
    );
    std::fs::write(pusher.join("LATER.md"), "later\n").unwrap();
    git(&pusher, &["add", "-A"]);
    git(&pusher, &["commit", "-m", "later"]);
    git(&pusher, &["push", "origin", "main"]);
    git(&pusher, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// What one run was handed.
#[derive(Debug, Clone)]
struct Seen {
    tree: PathBuf,
    session: PathBuf,
    resume_from: Option<PathBuf>,
    /// The tree's HEAD while the worker ran.
    head: String,
    prompt: String,
}

type Stage = Box<dyn Fn(&Workspace, Option<&Path>) -> RunResult + Send + Sync>;

/// A backend that records every run and stages the worker's output with
/// a closure over the full workspace — `ScriptedBackend` only sees the
/// tree, and these tests are about the session directory too.
struct StagedBackend {
    seen: Mutex<Vec<Seen>>,
    stage: Stage,
}

impl StagedBackend {
    fn new(stage: impl Fn(&Workspace, Option<&Path>) -> RunResult + Send + Sync + 'static) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            stage: Box::new(stage),
        }
    }

    fn only_run(&self) -> Seen {
        let seen = self.seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one run, got {seen:?}");
        seen[0].clone()
    }
}

#[async_trait::async_trait]
impl Backend for StagedBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let granted = Verdict::Granted {
            cap_tokens: None,
            reason: "test: always granted".to_owned(),
        };
        Ok(Outlook {
            normal: granted.clone(),
            prioritized: granted,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    async fn run(
        &self,
        ws: &Workspace,
        prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        self.seen.lock().unwrap().push(Seen {
            prompt: prompt.to_owned(),
            tree: ws.tree.clone(),
            session: ws.session.clone(),
            resume_from: resume_from.map(Path::to_path_buf),
            head: git(&ws.tree, &["rev-parse", "HEAD"]).trim().to_owned(),
        });
        Ok((self.stage)(ws, resume_from))
    }
}

/// A worker stopped at the cap with its transcript in the chain's
/// session directory: what the harness reports as a suspension.
fn suspended_at_cap(session: &Path) -> RunResult {
    let file = session.join("session.jsonl");
    std::fs::write(&file, format!("{USAGE}\n")).unwrap();
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(file.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

fn one_finding() -> String {
    serde_json::json!([{
        "fingerprint": "alpha:README.md:seed:1",
        "type": "bug",
        "file": "README.md",
        "line": 1,
        "bug_class": "boundary",
        "severity": "high",
        "confidence": 0.9,
        "summary": "off-by-one in the seed",
        "detail": "found across a suspension",
        "evidence_plan": "failing test first"
    }])
    .to_string()
}

/// A cold hunt that is suspended, the default branch then moving and
/// being fetched into the clone (another job's sync), and the resume that
/// finishes the chain. Returns the pinned commit, the commit origin moved
/// to, and the two runs.
async fn suspend_then_resume_a_hunt(f: &Fixture) -> (String, String, Seen, Seen, i64) {
    let pinned = git(&f.clone, &["rev-parse", "origin/main"])
        .trim()
        .to_owned();
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();

    let cold = StagedBackend::new(|ws, _| suspended_at_cap(&ws.session));
    let first = run_hunt(&f.store, &f.cfg, &row, &cold, None).await.unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    let origin_id = first.job_id.unwrap();

    let moved = advance_origin(&f.dir, &f.repo.origin);
    git(&f.clone, &["fetch", "origin"]);
    // Any fetch from here on fails, so the resume succeeding is proof it
    // fetched nothing.
    git(
        &f.clone,
        &["remote", "set-url", "origin", "/nonexistent/origin.git"],
    );

    let plan = match pick_next(&f.store, &f.cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended hunt to be resumed, got {other:?}"),
    };
    let out = f
        .cfg
        .work_root
        .join("out")
        .join(format!("job{origin_id}.findings.json"));
    let warm = StagedBackend::new(move |_ws, resume_from| {
        std::fs::write(&out, one_finding()).unwrap();
        let file = resume_from.expect("a resume names its transcript");
        let mut text = std::fs::read_to_string(file).unwrap();
        text.push_str(USAGE);
        text.push('\n');
        std::fs::write(file, text).unwrap();
        RunResult {
            exit_code: Some(0),
            killed_reason: None,
            tokens_new: 1_600,
            calls: 1,
            session_file: Some(file.to_string_lossy().into_owned()),
            duration_s: 1.0,
            stdout_tail: String::new(),
            usage_delta: None,
        }
    });
    let second = run_hunt(&f.store, &f.cfg, &row, &warm, Some(&plan))
        .await
        .unwrap();
    assert_eq!(second.state, Some(JobState::Done), "{second:?}");
    (pinned, moved, cold.only_run(), warm.only_run(), origin_id)
}

// -- invariant 1: a resume reuses its chain's workspace untouched ------------

/// A resumed attempt runs in the same tree and session directory as the
/// attempt it continues, at the same commit, and nothing is fetched or
/// checked out for it — even after the default branch moved and was
/// fetched into the clone in between.
#[tokio::test]
async fn a_resume_runs_in_its_origins_tree_and_session_untouched() {
    let f = fixture("ws-resume", "main").await;

    let (pinned, moved, cold, warm, origin_id) = suspend_then_resume_a_hunt(&f).await;

    let root = f.cfg.work_root.join("jobs").join(origin_id.to_string());
    assert_eq!(cold.tree, root.join("tree"));
    assert_eq!(cold.session, root.join("session"));
    assert_eq!(warm.tree, cold.tree, "the resume runs in its origin's tree");
    assert_eq!(warm.session, cold.session, "and continues its session dir");
    assert_eq!(
        warm.resume_from,
        Some(root.join("session").join("session.jsonl"))
    );
    assert_ne!(pinned, moved);
    assert_eq!(cold.head, pinned);
    assert_eq!(
        warm.head, pinned,
        "the tree must not move under a suspended chain"
    );
}

// -- invariant 3: the hunt watermark is the pinned commit --------------------

/// A chain that finishes after the default branch moved marks as hunted
/// exactly the commit it was pinned at. Recording the clone's tip instead
/// would mark the commits fetched in between as reviewed when they were
/// never in the diff range.
#[tokio::test]
async fn the_hunt_watermark_is_the_pinned_commit_after_the_branch_moved() {
    let f = fixture("ws-watermark", "main").await;

    let (pinned, moved, _, _, _) = suspend_then_resume_a_hunt(&f).await;

    let after = f.store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_ne!(after.last_hunt_sha.as_deref(), Some(moved.as_str()));
    assert_eq!(after.last_hunt_sha.as_deref(), Some(pinned.as_str()));
}

// -- invariant 2: only a finished chain loses its tree -----------------------

/// A chain workspace made from the clone, for job `id`.
async fn made_workspace(f: &Fixture, id: i64, state: &str, finished_at: i64) -> Workspace {
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (?1, 'hunt', 1, ?2, 1000, ?3)",
    )
    .bind(id)
    .bind(state)
    .bind(finished_at)
    .execute(&f.pool)
    .await
    .unwrap();
    let ws = Workspace::for_chain(&f.cfg.work_root, &f.clone, id);
    workspace::create(
        &ws,
        &TreeSpec::Detached {
            at: "origin/main".to_owned(),
        },
    )
    .expect("create the workspace");
    std::fs::write(ws.session.join("session.jsonl"), format!("{USAGE}\n")).unwrap();
    ws
}

/// The sweep keeps a suspended chain's tree, releases a terminal chain's
/// tree but keeps its transcript, and removes the whole workspace once the
/// chain finished more than the retention period ago.
#[tokio::test]
async fn the_sweep_releases_only_finished_chains_and_ages_out_their_sessions() {
    let f = fixture("ws-sweep", "main").await;
    let now = hunter::util::now_ms();
    let ws = made_workspace(&f, 10, "suspended", now - 1_000).await;

    workspace::sweep(&f.store, &f.cfg.work_root, now)
        .await
        .unwrap();
    assert!(ws.tree.is_dir(), "a suspended chain's tree is its resume");

    sqlx::query("UPDATE jobs SET state = 'killed' WHERE id = 10")
        .execute(&f.pool)
        .await
        .unwrap();
    let report = workspace::sweep(&f.store, &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 1);
    assert!(!ws.tree.exists(), "a finished chain's tree is released");
    assert!(
        !git(&f.clone, &["worktree", "list", "--porcelain"])
            .contains(ws.tree.to_string_lossy().as_ref()),
        "and unregistered from the clone"
    );
    assert!(
        ws.session.join("session.jsonl").is_file(),
        "the transcript outlives the tree"
    );

    workspace::sweep(
        &f.store,
        &f.cfg.work_root,
        now + SESSION_RETENTION_MS - 10_000,
    )
    .await
    .unwrap();
    assert!(ws.root.is_dir(), "inside retention the workspace stays");
    let report = workspace::sweep(&f.store, &f.cfg.work_root, now + SESSION_RETENTION_MS)
        .await
        .unwrap();
    assert_eq!(report.workspaces_removed, 1);
    assert!(!ws.root.exists(), "past retention the workspace goes");
    assert!(
        f.clone.join(".git").is_dir(),
        "the clone itself is never touched"
    );
}

/// A chain that was resumed and then finished is as finished as one that
/// never was: its tree is released when the last attempt ends, and the
/// workspace ages out with the rest. The attempt the resume continued
/// will never be offered again — its successor is the record that it was
/// picked up — so it must not keep the chain's tree alive.
#[tokio::test]
async fn a_resumed_chain_that_finished_releases_its_tree_and_ages_out() {
    let f = fixture("ws-resumed-release", "main").await;

    let (_, _, cold, _, origin_id) = suspend_then_resume_a_hunt(&f).await;

    assert!(!cold.tree.exists(), "the finished chain's tree is released");
    assert!(
        !git(&f.clone, &["worktree", "list", "--porcelain"])
            .contains(cold.tree.to_string_lossy().as_ref()),
        "and unregistered from the clone"
    );

    let root = f.cfg.work_root.join("jobs").join(origin_id.to_string());
    let later = hunter::util::now_ms() + SESSION_RETENTION_MS + 1;
    let report = workspace::sweep(&f.store, &f.cfg.work_root, later)
        .await
        .unwrap();
    assert_eq!(report.workspaces_removed, 1);
    assert!(!root.exists(), "past retention the workspace goes");
}

/// A daemon killed during `git worktree add` leaves the registration
/// locked (`initializing`) with its directory gone. Plain prune skips a
/// locked worktree, so the sweep must unlock it first -- but only one
/// whose directory is really gone: a locked tree that exists is kept.
#[tokio::test]
async fn the_sweep_prunes_a_locked_registration_whose_tree_is_gone() {
    let f = fixture("ws-locked-stale", "main").await;
    let gone = f.cfg.work_root.join("jobs").join("41").join("tree");
    let kept = f.dir.join("kept-tree");
    for (path, branch) in [(&gone, "stale-branch"), (&kept, "kept-branch")] {
        let p = path.to_string_lossy();
        git(
            &f.clone,
            &["worktree", "add", "-b", branch, &p, "origin/main"],
        );
        git(
            &f.clone,
            &["worktree", "lock", "--reason", "initializing", &p],
        );
    }
    std::fs::remove_dir_all(gone.parent().unwrap()).unwrap();

    workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();

    let listed = git(&f.clone, &["worktree", "list", "--porcelain"]);
    assert!(
        !listed.contains(gone.to_string_lossy().as_ref()),
        "the stale registration is pruned: {listed}"
    );
    git(&f.clone, &["branch", "-D", "stale-branch"]);
    let kept_record = listed
        .split("\n\n")
        .find(|r| r.contains(kept.to_string_lossy().as_ref()))
        .unwrap_or_else(|| panic!("a tree that still exists stays registered: {listed}"));
    assert!(
        kept_record.contains("locked initializing"),
        "and keeps its lock: {kept_record}"
    );
}

// -- invariant 4: a fresh fix takes over a suspended fix's branch ------------

/// A queued bug finding on repo 1, the state `run_fix` expects.
async fn queued_finding(store: &Store) -> i64 {
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "fp-fix-1".to_owned(),
                file: "README.md".to_owned(),
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
        .set_finding_status(fid, FindingStatus::Queued)
        .await
        .unwrap();
    fid
}

/// Both fix chains of one finding use the same branch name, and git
/// refuses one branch in two worktrees. The fresh fix still gets its
/// tree, because the superseded chain's tree is released first.
#[tokio::test]
async fn a_fresh_fix_superseding_a_suspended_fix_takes_over_its_branch() {
    let f = fixture("ws-fix-supersede", "main").await;
    let fid = queued_finding(&f.store).await;
    let finding = f.store.get_finding(fid).await.unwrap().unwrap();

    let suspending =
        ScriptedBackend::new(|tree| suspended_at_cap(&tree.parent().unwrap().join("session")));
    let first = run_fix(&f.store, &f.cfg, &finding, &suspending, None)
        .await
        .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    let old = first.job_id.unwrap();
    let old_tree = f
        .cfg
        .work_root
        .join("jobs")
        .join(old.to_string())
        .join("tree");
    assert!(old_tree.is_dir());

    let finding = f.store.get_finding(fid).await.unwrap().unwrap();
    let declining =
        ScriptedBackend::writing("NOT-A-BUG.md", "Classification: wrong\nmisread the code");
    let fresh = run_fix(&f.store, &f.cfg, &finding, &declining, None)
        .await
        .unwrap();

    assert_eq!(
        fresh.outcome.as_deref(),
        Some("rejected"),
        "the fresh fix got a tree on the shared branch and ran: {fresh:?}"
    );
    let (state, reason, _) = job_state(&f.pool, old).await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("killed", Some("superseded"))
    );
    assert!(
        !old_tree.exists(),
        "the superseded chain's tree was released"
    );
}

/// A resumed fix that then fails ends its chain, so its tree — and the
/// per-finding branch checked out in it — is released, and the next fresh
/// fix of the finding can check the branch out again. Nothing supersedes
/// that chain: its suspension already has a successor.
#[tokio::test]
async fn a_fresh_fix_after_a_failed_resume_takes_over_its_branch() {
    let f = fixture("ws-fix-after-resume", "main").await;
    let fid = queued_finding(&f.store).await;
    let finding = f.store.get_finding(fid).await.unwrap().unwrap();

    let suspending =
        ScriptedBackend::new(|tree| suspended_at_cap(&tree.parent().unwrap().join("session")));
    let first = run_fix(&f.store, &f.cfg, &finding, &suspending, None)
        .await
        .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    let old_tree = f
        .cfg
        .work_root
        .join("jobs")
        .join(first.job_id.unwrap().to_string())
        .join("tree");

    let plan = match pick_next(&f.store, &f.cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended fix to be resumed, got {other:?}"),
    };
    let finding = f.store.get_finding(fid).await.unwrap().unwrap();
    let failing = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        killed_reason: None,
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 1.0,
        stdout_tail: "crashed".to_owned(),
        usage_delta: None,
    });
    let resumed = run_fix(&f.store, &f.cfg, &finding, &failing, Some(&plan))
        .await
        .unwrap();
    assert_eq!(resumed.state, Some(JobState::Failed), "{resumed:?}");
    assert!(!old_tree.exists(), "the ended chain's tree was released");

    let finding = f.store.get_finding(fid).await.unwrap().unwrap();
    let declining = ScriptedBackend::writing("NOT-A-BUG.md", "misread the code");
    let fresh = run_fix(&f.store, &f.cfg, &finding, &declining, None)
        .await
        .unwrap();
    assert_eq!(
        fresh.outcome.as_deref(),
        Some("rejected"),
        "the fresh fix got a tree on the shared branch and ran: {fresh:?}"
    );
}

// -- invariant 5: the previous layout's leftovers ----------------------------

/// Legacy `wt/` trees go unless a running or suspended job still uses one:
/// a half-removed leftover with no `.git`, and a real worktree still
/// registered in the clone, are both removed; the tree a suspended engage
/// of finding 5 ran in (`wt/e5`) is kept.
#[tokio::test]
async fn the_sweep_removes_legacy_trees_no_live_job_uses() {
    let f = fixture("ws-legacy", "main").await;
    let wt = f.cfg.work_root.join("wt");
    let leftover = wt.join("f57");
    std::fs::create_dir_all(leftover.join("src")).unwrap();
    std::fs::write(leftover.join("src").join("lib.rs"), "half removed\n").unwrap();
    let registered = wt.join("h9");
    git(
        &f.clone,
        &[
            "worktree",
            "add",
            "--detach",
            registered.to_string_lossy().as_ref(),
            "origin/main",
        ],
    );
    let in_use = wt.join("e5");
    std::fs::create_dir_all(&in_use).unwrap();
    sqlx::query(
        "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) \
         VALUES (5, 'bug', 1, 'fp5', 'high', 0.9, 'bug 5', 'pr_open', 1000, 1000)",
    )
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state, session_file, started_at) \
         VALUES (30, 'engage', 1, 5, 'suspended', '/legacy/sessions/x/s.jsonl', 1000)",
    )
    .execute(&f.pool)
    .await
    .unwrap();

    let report = workspace::sweep(&f.store, &f.cfg.work_root, hunter::util::now_ms())
        .await
        .unwrap();

    assert!(!leftover.exists(), "an unreferenced leftover is removed");
    assert!(!registered.exists(), "an unreferenced worktree is removed");
    assert!(
        !git(&f.clone, &["worktree", "list", "--porcelain"]).contains("/wt/h9"),
        "and unregistered from its clone"
    );
    assert!(in_use.is_dir(), "a suspended job's legacy tree is kept");
    assert_eq!(
        (report.legacy_trees_removed, report.legacy_trees_kept),
        (2, 1)
    );
}

// -- invariant 6: a tree that cannot be made ---------------------------------

/// A cold job whose tree cannot be created is recorded `failed` with the
/// reason, and leaves no workspace directory behind. Here the repo names a
/// default branch origin does not have, so there is no commit to pin.
#[tokio::test]
async fn a_cold_job_whose_tree_cannot_be_made_fails_and_leaves_nothing() {
    let f = fixture("ws-no-tree", "no-such-branch").await;
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let summary = run_test_gap(&f.store, &f.cfg, &row, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.state, Some(JobState::Failed), "{summary:?}");
    let job = summary
        .job_id
        .expect("the job row exists and records the failure");
    let (state, _, notes) = job_state(&f.pool, job).await;
    assert_eq!(state, "failed");
    assert!(
        notes
            .as_deref()
            .is_some_and(|n| n.contains("workspace not created")),
        "{notes:?}"
    );
    assert!(
        !f.cfg.work_root.join("jobs").join(job.to_string()).exists(),
        "no half-made workspace may be left"
    );
}

/// Creating a workspace that already exists is refused, and the existing
/// directory is left exactly as it was. A job id is brand new when its
/// workspace is made, so a directory already there belongs to something
/// else — and a cold worker started against a session directory that
/// holds a transcript would be continued by omp's `autoResume`.
#[tokio::test]
async fn creating_over_an_existing_workspace_is_refused() {
    let f = fixture("ws-exists", "main").await;
    let ws = Workspace::for_chain(&f.cfg.work_root, &f.clone, 7);
    std::fs::create_dir_all(&ws.session).unwrap();
    std::fs::write(ws.session.join("other.jsonl"), "someone else's\n").unwrap();

    let made = workspace::create(
        &ws,
        &TreeSpec::Detached {
            at: "origin/main".to_owned(),
        },
    );

    assert!(made.is_err(), "{made:?}");
    assert!(!ws.tree.exists());
    assert_eq!(
        std::fs::read_to_string(ws.session.join("other.jsonl")).unwrap(),
        "someone else's\n"
    );
}

// -- the clone is fetch-only -------------------------------------------------

/// A cold job fetches the clone and builds its tree at the fetched tip,
/// but never checks out or pulls in the clone itself: the clone's HEAD is
/// not anyone's working tree any more, and the worker is pointed at its
/// chain's tree, not at the clone.
#[tokio::test]
async fn a_cold_job_fetches_the_clone_but_never_moves_its_checkout() {
    let f = fixture("ws-fetch-only", "main").await;
    let before = git(&f.clone, &["rev-parse", "HEAD"]).trim().to_owned();
    let moved = advance_origin(&f.dir, &f.repo.origin);
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();
    let backend = StagedBackend::new(|_, _| RunResult {
        exit_code: Some(0),
        killed_reason: None,
        tokens_new: 1_000,
        calls: 1,
        session_file: None,
        duration_s: 1.0,
        stdout_tail: String::new(),
        usage_delta: None,
    });

    run_hunt(&f.store, &f.cfg, &row, &backend, None)
        .await
        .unwrap();

    let seen = backend.only_run();
    assert_eq!(seen.head, moved, "the tree is built at the fetched tip");
    assert_eq!(
        git(&f.clone, &["rev-parse", "HEAD"]).trim(),
        before,
        "the clone's own checkout never moves"
    );
    assert!(
        seen.prompt.contains(seen.tree.to_string_lossy().as_ref()),
        "the playbook names the chain's tree, not the stale clone: {}",
        seen.prompt
    );
}

// -- fix: a suspension is not a failure --------------------------------------

/// A fix whose worker concluded before the cap stopped it — here it left
/// its verdict — is retired rather than left `suspended`: the finding is
/// no longer queued, so nothing could ever resume it, and a suspension
/// kept around would pin its tree and be offered every cycle. Every
/// status a decline can land at counts, not only `rejected`.
#[tokio::test]
async fn a_suspended_fix_whose_work_concluded_is_retired_and_released() {
    for (report, outcome) in [
        ("Classification: wrong\nmisread the code", "rejected"),
        ("Classification: superseded\nfixed by abc1234", "superseded"),
        ("Classification: unwanted\nCI pins Node 24", "wontfix"),
        ("misread the code", "new"),
    ] {
        let f = fixture("ws-fix-concluded", "main").await;
        let fid = queued_finding(&f.store).await;
        let finding = f.store.get_finding(fid).await.unwrap().unwrap();
        let concluded = ScriptedBackend::new(move |tree| {
            std::fs::write(tree.join("NOT-A-BUG.md"), report).unwrap();
            suspended_at_cap(&tree.parent().unwrap().join("session"))
        });

        let summary = run_fix(&f.store, &f.cfg, &finding, &concluded, None)
            .await
            .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some(outcome));
        let job = summary.job_id.unwrap();
        assert_eq!(job_state(&f.pool, job).await.0, "killed", "{outcome}");
        assert!(
            f.store.list_resumable_jobs().await.unwrap().is_empty(),
            "{outcome}"
        );
        assert!(
            !f.cfg
                .work_root
                .join("jobs")
                .join(job.to_string())
                .join("tree")
                .exists(),
            "{outcome}"
        );
    }
}

// -- harvest keeps a suspended tree ------------------------------------------

/// Minimal `gh pr view --json` payload that `view_pr_engage` can parse.
const PR_VIEW_JSON: &str = r#"{"state":"MERGED","mergeable":"MERGEABLE","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"feature","headRefOid":"deadbeef"}"#;

/// A suspended harvest keeps its tree for the resume. The harvest used
/// to remove its worktree after every run, suspended or not, so no
/// harvest suspension could ever be continued.
#[tokio::test]
async fn a_suspended_harvest_keeps_its_tree() {
    let bins = FakeBins::acquire("ws-harvest");
    bins.ok("gh", PR_VIEW_JSON);
    let f = fixture("ws-harvest", "main").await;
    sqlx::query("UPDATE repos SET url = 'https://github.com/acme/widget' WHERE id = 1")
        .execute(&f.pool)
        .await
        .unwrap();
    let fid = queued_finding(&f.store).await;
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at) \
         VALUES (?1, 7, 'MERGED', 1)",
    )
    .bind(fid)
    .execute(&f.pool)
    .await
    .unwrap();
    std::fs::write(
        f.dir.join("playbooks").join("harvest.md"),
        "harvest {{WORKTREE}}\n",
    )
    .unwrap();
    let finding = f.store.get_finding(fid).await.unwrap().unwrap();
    let suspending =
        ScriptedBackend::new(|tree| suspended_at_cap(&tree.parent().unwrap().join("session")));

    let summary = hunter::scheduler::run_harvest(&f.store, &f.cfg, &finding, &suspending, None)
        .await
        .unwrap();

    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    let tree = f
        .cfg
        .work_root
        .join("jobs")
        .join(summary.job_id.unwrap().to_string())
        .join("tree");
    assert!(tree.is_dir(), "the suspended harvest's tree is its resume");
}

// -- the previous layout's transcripts ---------------------------------------

/// Stamp a directory's mtime `age_ms` into the past.
fn age_dir(dir: &Path, age_ms: i64) {
    let when = std::time::SystemTime::now()
        - std::time::Duration::from_millis(u64::try_from(age_ms).unwrap());
    std::fs::File::open(dir)
        .and_then(|d| d.set_modified(when))
        .unwrap();
}

/// Legacy per-run transcript directories go once past retention — aged by
/// the finish time of the job that recorded a transcript there, else by
/// the directory's mtime — and never while a live job recorded one there.
#[tokio::test]
async fn the_sweep_ages_out_legacy_session_dirs_no_live_job_recorded() {
    let f = fixture("ws-legacy-sessions", "main").await;
    let now = hunter::util::now_ms();
    let old = SESSION_RETENTION_MS + 86_400_000;
    let sessions = f.cfg.work_root.join("sessions");
    let dir_of = |name: &str| {
        let d = sessions.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("s.jsonl"), format!("{USAGE}\n")).unwrap();
        d
    };
    let stale = dir_of("stale");
    let recent = dir_of("recent");
    let live = dir_of("live");
    let finished_long_ago = dir_of("finished-long-ago");
    for d in [&stale, &live] {
        age_dir(d, old);
    }
    for (id, state, finished, dir) in [
        (40, "suspended", now - old, &live),
        (41, "done", now - old, &finished_long_ago),
    ] {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, session_file, started_at, \
             finished_at) VALUES (?1, 'hunt', 1, ?2, ?3, 1000, ?4)",
        )
        .bind(id)
        .bind(state)
        .bind(dir.join("s.jsonl").to_string_lossy().to_string())
        .bind(finished)
        .execute(&f.pool)
        .await
        .unwrap();
    }

    let report = workspace::sweep(&f.store, &f.cfg.work_root, now)
        .await
        .unwrap();

    assert!(!stale.exists(), "unreferenced and old by mtime");
    assert!(
        !finished_long_ago.exists(),
        "aged by its job's finish time, whatever the mtime says"
    );
    assert!(recent.exists(), "inside retention");
    assert!(
        live.exists(),
        "a suspended job's transcript is never removed"
    );
    assert_eq!(report.legacy_sessions_removed, 2);
}

// -- Renovate scans a tree at the fetched tip --------------------------------

/// The zero-token Renovate scan reads the manifests of a throwaway tree at
/// the freshly fetched default branch, not the clone's stale checkout, and
/// the tree is gone once the scan is.
#[tokio::test]
async fn renovate_scans_a_tree_at_the_fetched_tip() {
    let bins = FakeBins::acquire("ws-renovate");
    let f = fixture("ws-renovate", "main").await;
    advance_origin(&f.dir, &f.repo.origin);
    let seen_cwd = f.dir.join("renovate-cwd");
    let seen_ls = f.dir.join("renovate-ls");
    // Fails after recording, so the scan yields nothing and the AI
    // fallback runs; what matters here is where Renovate was run.
    bins.script(
        "renovate",
        &format!(
            "pwd > '{}'\nls > '{}'\nexit 1",
            seen_cwd.display(),
            seen_ls.display()
        ),
    );
    std::fs::write(
        f.dir.join("playbooks").join("dep_update.md"),
        "deps {{REPO_PATH}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();

    hunter::scheduler::run_dep_update(&f.store, &f.cfg, &row, &ScriptedBackend::noop(), None)
        .await
        .unwrap();

    let scan = f.cfg.work_root.join("scan").join("repo-1");
    let cwd = std::fs::read_to_string(&seen_cwd).expect("renovate ran");
    assert_eq!(
        Path::new(cwd.trim()),
        scan.canonicalize().unwrap_or(scan.clone())
    );
    assert!(
        std::fs::read_to_string(&seen_ls)
            .unwrap()
            .lines()
            .any(|l| l == "LATER.md"),
        "the scan sees the fetched tip, which only origin has"
    );
    assert!(!scan.exists(), "the scan tree does not outlive the scan");
}
