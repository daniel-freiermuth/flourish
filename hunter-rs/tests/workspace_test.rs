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
use hunter::clones::ClonePermit;
use hunter::config::Config;
use hunter::domain::{FindingStatus, JobState};
use hunter::scheduler::{Attempt, Candidate, Resumed, pick_next, run_fix, run_hunt, run_test_gap};
use hunter::store::{ChainRef, FindingInsert, Store};
use hunter::types::RunResult;
use hunter::workspace::{self, OpenWorkspace, SESSION_RETENTION_MS, TreeSpec, Workspace};
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
    let mut cfg = support::load_config(dir.path());
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

/// The right to change repo 1's clone.
async fn permit(f: &Fixture) -> ClonePermit<'_> {
    f.store.test_lane().lock_clone(1).await.unwrap()
}

/// [`workspace::create`] of chain `id`'s workspace in repo 1's clone.
async fn create(f: &Fixture, work_root: &Path, id: i64, spec: TreeSpec) -> Result<String, String> {
    let work_root = work_root.to_owned();
    permit(f)
        .await
        .blocking(move |hold| workspace::create(hold, &work_root, id, &spec))
        .await
        .unwrap()
}

/// Chain `origin_id` as the store names it.
async fn chain(f: &Fixture, origin_id: i64) -> ChainRef {
    f.store.chain_ref(origin_id).await.unwrap().unwrap()
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
    let pinned = create(
        &f,
        &cfg.work_root,
        1,
        TreeSpec::Branch {
            name: "fix/relative-root".to_owned(),
            at: "origin/main".to_owned(),
        },
    )
    .await
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

    fn worker_hosts(&self) -> Vec<String> {
        Vec::new()
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
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &cold,
    )
    .await
    .unwrap();
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

    let plan = match pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
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
    let second = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &warm,
    )
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
    create(
        f,
        &f.cfg.work_root,
        id,
        TreeSpec::Detached {
            at: "origin/main".to_owned(),
        },
    )
    .await
    .expect("create the workspace");
    std::fs::write(ws.session.join("session.jsonl"), format!("{USAGE}\n")).unwrap();
    ws
}

/// The sweep keeps a checkpoint's tree, releases a terminal chain's tree
/// but keeps its transcript, and removes the whole workspace once the
/// chain finished more than the retention period ago.
#[tokio::test]
async fn the_sweep_releases_only_finished_chains_and_ages_out_their_sessions() {
    let f = fixture("ws-sweep", "main").await;
    let now = hunter::util::now_ms();
    let ws = made_workspace(&f, 10, "suspended", now - 1_000).await;
    support::point_repo_checkpoint(&f.pool, 1, "hunt", Some(10)).await;

    workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert!(ws.tree.is_dir(), "a checkpoint's tree is its resume");

    // The work item lets go of it: given up, superseded, or the repo's
    // work started over. The row stays `suspended`, as it ended.
    support::point_repo_checkpoint(&f.pool, 1, "hunt", None).await;
    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
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
        &support::admin(&f.store),
        &f.cfg.work_root,
        now + SESSION_RETENTION_MS - 10_000,
    )
    .await
    .unwrap();
    assert!(ws.root.is_dir(), "inside retention the workspace stays");
    let report = workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        now + SESSION_RETENTION_MS,
    )
    .await
    .unwrap();
    assert_eq!(report.workspaces_removed, 1);
    assert!(!ws.root.exists(), "past retention the workspace goes");
    assert!(
        f.clone.join(".git").is_dir(),
        "the clone itself is never touched"
    );
}

/// A link of the chain rooted at `made_workspace`'s job: a row that
/// continues `resumed_from` and owns no workspace of its own.
async fn resumed_link(
    f: &Fixture,
    id: i64,
    resumed_from: i64,
    state: &str,
    finished_at: Option<i64>,
) {
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at, resumed_from) \
         VALUES (?1, 'hunt', 1, ?2, 1000, ?3, ?4)",
    )
    .bind(id)
    .bind(state)
    .bind(finished_at)
    .bind(resumed_from)
    .execute(&f.pool)
    .await
    .unwrap();
}

/// A resume never changes the attempt it continues: that row stays
/// `suspended` for good, superseded by its successor. So once the
/// successor finishes, the chain is finished, and its tree is released
/// by the executor that recorded the outcome.
#[tokio::test]
async fn a_resumed_chain_that_finished_releases_its_tree() {
    let f = fixture("ws-resumed-done", "main").await;

    let (_, _, cold, _, origin_id) = suspend_then_resume_a_hunt(&f).await;

    let (origin_state, _, _) = job_state(&f.pool, origin_id).await;
    assert_eq!(
        origin_state, "suspended",
        "the resumed attempt is untouched"
    );
    let status = f.store.chain_status(origin_id).await.unwrap();
    assert_eq!((status.attempts, status.live), (2, 0), "{status:?}");
    assert!(
        !cold.tree.exists(),
        "a finished resumed chain's tree is released"
    );
    assert!(
        !git(&f.clone, &["worktree", "list", "--porcelain"])
            .contains(cold.tree.to_string_lossy().as_ref()),
        "and unregistered from the clone"
    );
    assert!(
        cold.session.join("session.jsonl").is_file(),
        "the transcript outlives the tree"
    );
}

/// The sweep reads a multi-attempt chain as a whole: kept while its newest
/// attempt runs, released once that attempt finished, and aged out by the
/// newest attempt's finish, not the origin's.
#[tokio::test]
async fn the_sweep_ages_a_resumed_chain_by_its_newest_attempt() {
    let f = fixture("ws-sweep-resumed", "main").await;
    let now = hunter::util::now_ms();
    let origin_finished = now - SESSION_RETENTION_MS - 60_000;
    let ws = made_workspace(&f, 10, "suspended", origin_finished).await;
    resumed_link(&f, 11, 10, "running", None).await;

    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 0);
    assert!(ws.tree.is_dir(), "a chain whose resume runs keeps its tree");

    sqlx::query("UPDATE jobs SET state = 'done', finished_at = ?1 WHERE id = 11")
        .bind(now)
        .execute(&f.pool)
        .await
        .unwrap();
    let status = f.store.chain_status(10).await.unwrap();
    assert_eq!(status.live, 0, "{status:?}");
    assert_eq!(
        status.last_finished_at,
        Some(now),
        "the newest attempt's finish"
    );

    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 1);
    assert!(
        !ws.tree.exists(),
        "a finished resumed chain's tree is released"
    );
    assert!(
        ws.root.is_dir(),
        "the origin's old finish does not age out a chain that just ended"
    );

    let report = workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        now + SESSION_RETENTION_MS + 1,
    )
    .await
    .unwrap();
    assert_eq!(report.workspaces_removed, 1);
    assert!(!ws.root.exists(), "past retention the workspace goes");
}

/// A resume never rewrites the link it continues, so every link of a
/// chain given up after several attempts reads `suspended`. Only the
/// pointer says whether one is still a checkpoint: once the give-up drops
/// it, the chain is over, and its tree is released.
#[tokio::test]
async fn a_chain_given_up_after_several_attempts_releases_its_tree() {
    let f = fixture("ws-giveup", "main").await;
    let now = hunter::util::now_ms();
    let ws = made_workspace(&f, 10, "suspended", now - 4_000).await;
    resumed_link(&f, 11, 10, "suspended", Some(now - 3_000)).await;
    resumed_link(&f, 12, 11, "suspended", Some(now - 2_000)).await;
    resumed_link(&f, 13, 12, "suspended", Some(now - 1_000)).await;
    support::point_repo_checkpoint(&f.pool, 1, "hunt", Some(13)).await;

    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 0);
    assert!(ws.tree.is_dir(), "the newest suspension is still resumable");

    support::point_repo_checkpoint(&f.pool, 1, "hunt", None).await;
    let status = f.store.chain_status(10).await.unwrap();
    assert_eq!((status.attempts, status.live), (4, 0), "{status:?}");
    assert!(
        workspace::release_if_idle(
            &f.store,
            &permit(&f).await,
            &f.cfg.work_root,
            &chain(&f, 10).await
        )
        .await
        .unwrap(),
        "a given-up chain is released"
    );
    assert!(!ws.tree.exists());
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

    workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
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

/// A runner still uses its tree after its job is recorded: it reads the
/// worker's outputs, and an engage hands the tree to its harvest before
/// the harvest has a row. The job table then says the chain is finished,
/// so only the runner's lease keeps the sweep off the tree.
#[tokio::test]
async fn the_sweep_keeps_a_finished_chains_tree_while_a_runner_leases_it() {
    let f = fixture("ws-leased", "main").await;
    let now = hunter::util::now_ms();
    let ws = made_workspace(&f, 10, "done", now - 1_000).await;
    let lease = OpenWorkspace::lease(
        &permit(&f).await,
        &f.cfg.work_root,
        &chain(&f, ws.origin_id).await,
    )
    .unwrap();

    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 0);
    assert!(ws.tree.is_dir(), "a leased tree is kept");

    drop(lease);
    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
        .await
        .unwrap();
    assert_eq!(report.trees_released, 1, "and released once the lease ends");
    assert!(!ws.tree.exists());
}

/// A release made on another chain's behalf -- a fresh job releasing the
/// trees of the chains it superseded -- holds no lease of its own, so any
/// lease on the tree keeps it: a runner still reading its outputs.
#[tokio::test]
async fn a_release_with_no_lease_of_its_own_keeps_a_leased_tree() {
    let f = fixture("ws-release-leased", "main").await;
    let ws = made_workspace(&f, 10, "done", hunter::util::now_ms() - 1_000).await;
    let permit = permit(&f).await;
    let chain = chain(&f, ws.origin_id).await;
    let lease = OpenWorkspace::lease(&permit, &f.cfg.work_root, &chain).unwrap();

    let released = workspace::release_if_idle(&f.store, &permit, &f.cfg.work_root, &chain)
        .await
        .unwrap();
    assert!(!released && ws.tree.is_dir(), "a leased tree is kept");

    drop(lease);
    let released = workspace::release_if_idle(&f.store, &permit, &f.cfg.work_root, &chain)
        .await
        .unwrap();
    assert!(released && !ws.tree.exists(), "and released once unleased");
}

/// A Renovate scan interrupted before its repo was deleted leaves a scan
/// tree no listed repo's pass removes. The sweep removes it anyway, as a
/// tree whose clone only git can name.
#[tokio::test]
async fn the_sweep_removes_the_scan_tree_of_a_repo_no_longer_listed() {
    let f = fixture("ws-scan-orphan", "main").await;
    let orphan = f.cfg.work_root.join("scan").join("repo-99");
    std::fs::create_dir_all(orphan.join("half-written")).unwrap();

    workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
    .await
    .unwrap();
    assert!(!orphan.exists(), "the leftover scan tree is removed");
}

/// The pack files in `clone`'s object store.
fn packs(clone: &Path) -> usize {
    std::fs::read_dir(clone.join(".git/objects/pack")).map_or(0, |dir| {
        dir.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count()
    })
}

/// Make `clone`'s housekeeping due: a threshold of one, and thousands of
/// loose objects. Returns the clone's pack count before any runs.
fn housekeeping_due(f: &Fixture) -> usize {
    git(&f.clone, &["config", "gc.auto", "1"]);
    let loose = f.dir.subdir("loose");
    let paths: Vec<String> = (0..3000)
        .map(|i| {
            let p = loose.join(i.to_string());
            std::fs::write(&p, format!("loose {i}\n")).unwrap();
            p.to_string_lossy().into_owned()
        })
        .collect();
    let mut hashed = std::process::Command::new("git")
        .args([
            "-C",
            &f.clone.to_string_lossy(),
            "hash-object",
            "-w",
            "--stdin-paths",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(hashed.stdin.as_mut().unwrap(), paths.join("\n").as_bytes()).unwrap();
    drop(hashed.stdin.take());
    assert!(hashed.wait().unwrap().success());
    packs(&f.clone)
}

/// Git ends a fetch by starting its housekeeping detached: a repack that
/// outlives the fetch, and so the permit, and would run under the next
/// holder's `worktree add` or `branch -D`. The daemon's fetch runs it
/// inside the hold instead: when the fetch returns, the repack it was due
/// is done, and nothing goes on changing the clone after it.
#[tokio::test]
async fn a_fetch_finishes_the_clones_housekeeping_before_it_returns() {
    let f = fixture("ws-fetch-maintenance", "main").await;
    let before = housekeeping_due(&f);

    let (rc, out) = permit(&f)
        .await
        .blocking(|hold| workspace::fetch(hold, &[], 60))
        .await
        .unwrap();

    assert_eq!(rc, 0, "{out}");
    let at_return = packs(&f.clone);
    assert!(at_return > before, "the fetch's housekeeping never ran");
    // Detached, the repack is still writing when the fetch returns: its
    // last pack appears after that, under whoever holds the clone next.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        packs(&f.clone),
        at_return,
        "the clone's housekeeping went on after the fetch returned"
    );
}

/// A worker's commit in its tree starts the same detached housekeeping in
/// the clone, and a fix or a resume starts its worker without fetching
/// first. So startup turns automatic maintenance off in every clone of
/// the daemon's own on disk, also in one no fetch has reached since: after
/// it, a commit in a tree leaves the clone as it found it.
#[tokio::test]
async fn after_startup_a_commit_in_a_tree_leaves_its_clone_alone() {
    let f = fixture("ws-quiet-clones", "main").await;
    let before = housekeeping_due(&f);
    let tree = f.cfg.work_root.join("jobs").join("77").join("tree");
    let t = tree.to_string_lossy().into_owned();
    git(
        &f.clone,
        &["worktree", "add", "--detach", &t, "origin/main"],
    );

    // The fixture's clone is `<dir>/repos/repo-1`: the daemon's own clone
    // for a work root of `<dir>`.
    workspace::quiet_clones(f.store.test_lane(), f.dir.path())
        .await
        .unwrap();
    git(
        &tree,
        &[
            "-c",
            "user.name=worker",
            "-c",
            "user.email=worker@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "work",
        ],
    );

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(
        packs(&f.clone),
        before,
        "the commit started housekeeping that went on in the clone"
    );
}

/// A repo row may name the operator's own checkout instead of a clone
/// under `<work_root>/repos`. Its config is the operator's: startup turns
/// nothing off there.
#[tokio::test]
async fn startup_leaves_the_config_of_an_operators_checkout_alone() {
    let f = fixture("ws-quiet-foreign", "main").await;
    // `f.cfg.work_root` is `<dir>/work_root`, so `<dir>/repos/repo-1` is
    // not under its `repos/`: to this daemon, an operator's checkout.
    workspace::quiet_clones(f.store.test_lane(), &f.cfg.work_root)
        .await
        .unwrap();

    let set = std::process::Command::new("git")
        .args(["config", "--local", "--get", "maintenance.auto"])
        .current_dir(&f.clone)
        .output()
        .unwrap();
    assert_eq!(
        set.status.code(),
        Some(1),
        "startup wrote maintenance.auto into the operator's checkout: {}",
        String::from_utf8_lossy(&set.stdout)
    );
}

/// A clone made after startup is never reached by `quiet_clones`, and a
/// fix starts its worker without fetching: the clone has automatic
/// maintenance off from the moment it is made.
#[tokio::test]
async fn a_new_clone_starts_with_automatic_maintenance_off() {
    let f = fixture("ws-clone-config", "main").await;
    let fresh = f.dir.subdir("repos").join("repo-2");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(f.repo.origin.to_string_lossy().to_string())
    .bind(fresh.to_string_lossy().to_string())
    .execute(&f.pool)
    .await
    .unwrap();
    let url = f.repo.origin.to_string_lossy().into_owned();

    let (rc, out) = f
        .store
        .test_lane()
        .lock_clone(2)
        .await
        .unwrap()
        .blocking(move |hold| hold.clone_from(&url, 60))
        .await
        .unwrap();

    assert_eq!(rc, 0, "{out}");
    assert_eq!(
        git(&fresh, &["config", "--local", "--get", "maintenance.auto"]).trim(),
        "false"
    );
}

/// A fetch writes no config: a clone whose config cannot be rewritten
/// (a `config.lock` a killed `git config` left behind) still fetches.
#[tokio::test]
async fn a_fetch_works_in_a_clone_whose_config_is_locked() {
    let f = fixture("ws-fetch-config-lock", "main").await;
    std::fs::write(f.clone.join(".git").join("config.lock"), "").unwrap();

    let (rc, out) = permit(&f)
        .await
        .blocking(|hold| workspace::fetch(hold, &[], 60))
        .await
        .unwrap();

    assert_eq!(rc, 0, "the fetch failed on the locked config: {out}");
}

/// Under one repo's permit, git runs only in trees of that repo's clone. A
/// chain's tree path names no clone, so a tree of another clone would be
/// changed under a permit that does not cover it.
#[tokio::test]
async fn a_hold_runs_git_only_in_trees_of_its_own_clone() {
    let f = fixture("ws-git-in-foreign", "main").await;
    let stranger = f.dir.subdir("stranger");
    git(&stranger, &["init", "-q"]);
    std::fs::write(stranger.join("README.md"), "not repo 1\n").unwrap();
    git(&stranger, &["add", "-A"]);
    git(&stranger, &["commit", "-q", "-m", "seed"]);
    let foreign = f.dir.join("foreign-tree");
    let own = f.dir.join("own-tree");
    let (ft, ot) = (
        foreign.to_string_lossy().into_owned(),
        own.to_string_lossy().into_owned(),
    );
    git(
        &stranger,
        &["worktree", "add", "-q", "--detach", &ft, "HEAD"],
    );
    git(
        &f.clone,
        &["worktree", "add", "-q", "--detach", &ot, "origin/main"],
    );

    let (foreign_rc, own_rc) = permit(&f)
        .await
        .blocking(move |hold| {
            (
                hold.git_in(&foreign, &["checkout", "-q", "-B", "intruder"], 30)
                    .0,
                hold.git_in(&own, &["checkout", "-q", "-B", "mine"], 30).0,
            )
        })
        .await
        .unwrap();

    assert_ne!(foreign_rc, 0, "git ran in a tree of another clone");
    assert!(
        !git(&stranger, &["branch", "--list", "intruder"]).contains("intruder"),
        "the other clone got a branch under repo 1's permit"
    );
    assert_eq!(own_rc, 0, "a tree of the held clone is refused");
    assert!(git(&f.clone, &["branch", "--list", "mine"]).contains("mine"));
}

/// A `worktree add` under way looks exactly like the killed one above: a
/// registration locked `initializing` whose directory is not there yet.
/// The sweep's prune would unlock and drop it under the add. So the sweep
/// changes a clone only under its permit, and waits while another holder
/// (the runner making that tree) has it.
#[tokio::test]
async fn the_sweep_waits_for_the_clones_holder_before_pruning() {
    let f = fixture("ws-permit", "main").await;
    let adding = f.cfg.work_root.join("jobs").join("42").join("tree");
    let a = adding.to_string_lossy().into_owned();
    git(
        &f.clone,
        &["worktree", "add", "-b", "adding-branch", &a, "origin/main"],
    );
    git(
        &f.clone,
        &["worktree", "lock", "--reason", "initializing", &a],
    );
    std::fs::remove_dir_all(adding.parent().unwrap()).unwrap();
    let registered = || git(&f.clone, &["worktree", "list", "--porcelain"]).contains(a.as_str());

    let held = permit(&f).await;
    let sweep = tokio::spawn({
        let (store, root) = (f.store.clone(), f.cfg.work_root.clone());
        async move { workspace::sweep(&support::admin(&store), &root, hunter::util::now_ms()).await }
    });
    support::until_waiting(&f.store, 1, 1).await;
    assert!(!sweep.is_finished(), "the sweep waits for the clone");
    assert!(registered(), "the add under way keeps its registration");

    drop(held);
    sweep.await.unwrap().unwrap();
    assert!(
        !registered(),
        "once the clone is free the stale registration goes"
    );
}

/// A repo flagged deleted keeps its clone until it is reaped, and its
/// clone has a permit like any other: a Renovate scan of it, or the
/// reaper removing it, may hold that permit right now. The sweep holds it
/// too before it touches that repo's scan tree, so it waits for the
/// holder instead of removing the tree as a leftover under it.
#[tokio::test]
async fn the_sweep_waits_for_a_deleted_repos_clone_before_its_scan_tree() {
    let f = fixture("ws-deleted-scan", "main").await;
    let gone = f.dir.subdir("repos").join("repo-2");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, deleted_at) \
         VALUES (2, 'gone', 'https://example.com/gone.git', ?1, 'github', 'main', 1, 1000, 2000)",
    )
    .bind(gone.to_string_lossy().to_string())
    .execute(&f.pool)
    .await
    .unwrap();
    let scan = f.cfg.work_root.join("scan").join("repo-2");
    std::fs::create_dir_all(scan.join("package.json.d")).unwrap();

    let held = f.store.test_lane().lock_clone(2).await.unwrap();
    let sweep = tokio::spawn({
        let (store, root) = (f.store.clone(), f.cfg.work_root.clone());
        async move { workspace::sweep(&support::admin(&store), &root, hunter::util::now_ms()).await }
    });
    support::until_waiting(&f.store, 2, 1).await;
    assert!(scan.is_dir(), "the scan tree stays while its clone is held");

    drop(held);
    sweep.await.unwrap().unwrap();
    assert!(!scan.exists(), "once the clone is free the leftover goes");
}

/// A repo can be reaped while its Renovate scan runs: the scan holds only
/// its scan lease, not the clone, so the reaper gets the permit and the
/// repo row goes. Its scan tree is then one no listed repo's pass covers.
/// The sweep keeps it while the scan's lease lives, and removes a leftover
/// no scan leases beside it.
#[tokio::test]
async fn the_sweep_keeps_the_live_scan_tree_of_a_reaped_repo() {
    let f = fixture("ws-reaped-scan", "main").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'reaped', 'https://example.com/reaped.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(
        f.dir
            .subdir("repos")
            .join("repo-2")
            .to_string_lossy()
            .to_string(),
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let scan = f.cfg.work_root.join("scan").join("repo-2");
    let leftover = f.cfg.work_root.join("scan").join("repo-99");
    std::fs::create_dir_all(scan.join("package.json.d")).unwrap();
    std::fs::create_dir_all(leftover.join("half-written")).unwrap();
    let lease = f
        .store
        .test_lane()
        .lock_clone(2)
        .await
        .unwrap()
        .lease_scan()
        .unwrap();
    sqlx::query("DELETE FROM repos WHERE id = 2")
        .execute(&f.pool)
        .await
        .unwrap();
    let sweep = async || {
        workspace::sweep(
            &support::admin(&f.store),
            &f.cfg.work_root,
            hunter::util::now_ms(),
        )
        .await
        .unwrap();
    };

    sweep().await;
    assert!(scan.is_dir(), "the sweep removed a live scan tree");
    assert!(!leftover.exists(), "a scan tree no scan leases is removed");

    drop(lease);
    sweep().await;
    assert!(!scan.exists(), "once the scan ends its tree goes");
}

/// A legacy tree of a clone no repo row names is one no permit covers: the
/// sweep removes the directory but never runs git in that clone, so the
/// clone's registration is only ever dropped by a prune of a clone someone
/// holds. Before, the sweep took every listed repo's permit as leave to
/// run `worktree remove` in whatever clone the tree named.
#[tokio::test]
async fn the_sweep_changes_no_clone_it_holds_no_permit_for() {
    let f = fixture("ws-foreign-clone", "main").await;
    let stranger = f.dir.subdir("stranger");
    git(&stranger, &["init", "-q"]);
    std::fs::write(stranger.join("README.md"), "not a repo of ours\n").unwrap();
    git(&stranger, &["add", "-A"]);
    git(&stranger, &["commit", "-q", "-m", "seed"]);
    let tree = f.cfg.work_root.join("wt").join("x9");
    let t = tree.to_string_lossy().into_owned();
    git(
        &stranger,
        &["worktree", "add", "-q", "--detach", &t, "HEAD"],
    );
    let registered = || git(&stranger, &["worktree", "list", "--porcelain"]).contains(t.as_str());
    assert!(registered());

    workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
    .await
    .unwrap();

    assert!(!tree.exists(), "the legacy tree's directory goes");
    assert!(registered(), "the stranger's registration is not touched");
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
    let first = run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending,
    )
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
    let fresh = run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &declining,
    )
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
        ("suspended", Some("cap")),
        "the superseded checkpoint's row is left as it ended"
    );
    assert!(
        !old_tree.exists(),
        "the superseded chain's tree was released"
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
    support::point_checkpoint(&f.pool, 5, "engage", 30).await;

    let report = workspace::sweep(
        &support::admin(&f.store),
        &f.cfg.work_root,
        hunter::util::now_ms(),
    )
    .await
    .unwrap();

    assert!(!leftover.exists(), "an unreferenced leftover is removed");
    assert!(!registered.exists(), "an unreferenced worktree is removed");
    assert!(
        !git(&f.clone, &["worktree", "list", "--porcelain"]).contains("/wt/h9"),
        "and unregistered from its clone"
    );
    assert!(in_use.is_dir(), "a checkpoint's legacy tree is kept");
    assert_eq!(
        (report.legacy_trees_removed, report.legacy_trees_kept),
        (2, 1)
    );
}

/// A pass whose legacy trees a live job all keeps has nothing to remove,
/// so it takes no clone's permit for them: while it waits for a clone a
/// runner holds, every other clone stays free. Before, any pass with a
/// kept tree took every clone's permit in id order, and waiting for one
/// held all those before it.
#[tokio::test]
async fn a_sweep_that_keeps_every_legacy_tree_holds_no_clone_while_it_waits() {
    let f = fixture("ws-legacy-kept", "main").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/beta.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(
        f.dir
            .subdir("repos")
            .join("repo-2")
            .to_string_lossy()
            .to_string(),
    )
    .execute(&f.pool)
    .await
    .unwrap();
    let in_use = f.cfg.work_root.join("wt").join("e5");
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
    support::point_checkpoint(&f.pool, 5, "engage", 30).await;

    let held = f.store.test_lane().lock_clone(2).await.unwrap();
    let sweep = tokio::spawn({
        let (store, root) = (f.store.clone(), f.cfg.work_root.clone());
        async move { workspace::sweep(&support::admin(&store), &root, hunter::util::now_ms()).await }
    });
    support::until_waiting(&f.store, 2, 1).await;
    let other = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        f.store.test_lane().lock_clone(1),
    )
    .await;
    assert!(
        other.is_ok_and(|p| p.is_ok()),
        "repo 1's clone was held while the sweep waited for repo 2's"
    );

    drop(held);
    let report = sweep.await.unwrap().unwrap();
    assert!(in_use.is_dir(), "the checkpoint's legacy tree is kept");
    assert_eq!(report.legacy_trees_kept, 1);
}

// -- invariant 6: a tree that cannot be made ---------------------------------

/// A cold job whose tree cannot be created is recorded `failed` with the
/// reason, leaves no workspace directory behind, and is counted on its
/// work item under the fixed key. Here `jobs/` is a file, so no chain
/// directory can be made under it.
#[tokio::test]
async fn a_cold_job_whose_tree_cannot_be_made_fails_and_leaves_nothing() {
    let f = fixture("ws-no-tree", "main").await;
    std::fs::create_dir_all(&f.cfg.work_root).unwrap();
    std::fs::write(f.cfg.work_root.join("jobs"), "not a directory").unwrap();
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let summary = run_test_gap(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &backend,
    )
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
        !f.cfg.work_root.join("jobs").join(job.to_string()).is_dir(),
        "no half-made workspace may be left"
    );
    let streak = f
        .store
        .repo_work_streak(1, hunter::domain::RepoWorkKind::TestGap)
        .await
        .unwrap();
    assert_eq!(
        (streak.streak, streak.last_failure.as_deref()),
        (1, Some("workspace not created"))
    );
    assert_eq!(summary.outcome.as_deref(), Some("retry"));
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

    let made = create(
        &f,
        &f.cfg.work_root,
        7,
        TreeSpec::Detached {
            at: "origin/main".to_owned(),
        },
    )
    .await;

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

    run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &backend,
    )
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
/// its verdict — keeps no checkpoint: the finding left the fix's tier, so
/// nothing could ever resume it, and a checkpoint kept around would pin
/// its tree and be offered every cycle. The job row stays `suspended`, as
/// the attempt ended. Every status a decline can land at counts, not only
/// `rejected`.
#[tokio::test]
async fn a_suspended_fix_whose_work_concluded_keeps_no_checkpoint_and_is_released() {
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

        let summary = run_fix(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &concluded,
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some(outcome));
        let job = summary.job_id.unwrap();
        assert_eq!(job_state(&f.pool, job).await.0, "suspended", "{outcome}");
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
    f.store
        .set_finding_status(fid, hunter::domain::FindingStatus::Merged)
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

    let summary = hunter::scheduler::run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending,
    )
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

    let report = workspace::sweep(&support::admin(&f.store), &f.cfg.work_root, now)
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
    // Fails after recording, so Renovate has no answer; what matters here
    // is where it was run.
    bins.script(
        "renovate",
        &format!(
            "pwd > '{}'\nls > '{}'\nexit 1",
            seen_cwd.display(),
            seen_ls.display()
        ),
    );
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();

    hunter::scheduler::scan_dependencies(
        f.store.test_lane(),
        &support::admin(&f.store),
        &f.cfg,
        &row,
    )
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

/// Renovate's lookups go to the network, so the scan holds the clone only
/// to make and remove its tree: while Renovate runs, another lane gets
/// the clone at once, and the sweep, under that same permit, keeps the
/// live scan tree.
#[tokio::test]
async fn renovate_runs_holding_no_clone_and_the_sweep_keeps_its_tree() {
    let bins = FakeBins::acquire("ws-renovate-free");
    let f = fixture("ws-renovate-free", "main").await;
    let started = f.dir.join("renovate-started");
    let release = f.dir.join("renovate-release");
    bins.script(
        "renovate",
        &format!(
            // Bounded, so a failing assertion below cannot leave it waiting.
            "touch '{}'\ni=0\nwhile [ ! -e '{}' ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\nexit 1",
            started.display(),
            release.display()
        ),
    );
    let row = f.store.get_repo_by_id(1).await.unwrap().unwrap();
    let scan = f.cfg.work_root.join("scan").join("repo-1");

    let admin = support::admin(&f.store);
    let runner = hunter::scheduler::scan_dependencies(f.store.test_lane(), &admin, &f.cfg, &row);
    let meanwhile = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !started.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("renovate started");
        let free = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            f.store.test_lane().lock_clone(1),
        )
        .await;
        assert!(
            free.is_ok_and(|p| p.is_ok()),
            "the clone is held while Renovate runs"
        );
        workspace::sweep(
            &support::admin(&f.store),
            &f.cfg.work_root,
            hunter::util::now_ms(),
        )
        .await
        .unwrap();
        assert!(scan.is_dir(), "the sweep removed a live scan tree");
        std::fs::write(&release, "").unwrap();
    };
    let (ran, ()) = tokio::join!(runner, meanwhile);
    ran.unwrap();
    assert!(!scan.exists(), "the scan tree does not outlive the scan");
}
