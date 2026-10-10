//! Per-chain workspaces: where a job runs, and when that place goes away.
//!
//! A chain is a cold job plus every attempt that resumes it. Each chain
//! gets one directory, `<work_root>/jobs/<origin_id>/`, keyed by the id of
//! the chain's FIRST job:
//!
//! - `tree/` is a git worktree of the repo's clone, created once at one
//!   commit (recorded as `jobs.pinned_sha`) and never moved by the daemon.
//! - `session/` is the omp `--session-dir`: the chain's transcript, which a
//!   resumed attempt continues in place.
//!
//! Two properties follow from the keying, and the rest of the daemon relies
//! on both. Every attempt of a chain sees the same files, because a resumed
//! worker continues a conversation full of absolute paths into them. And no
//! two chains share a directory, so nothing one job does to "its" tree can
//! touch another job's — the failure the finding-keyed `wt/<x><fid>` trees
//! had, where a cold attempt reclaimed a suspended job's tree by name.
//!
//! The clone under `repos/repo-<id>` is only an object store: it is
//! fetched, never checked out, so no job can move HEAD under another.
//!
//! Every removal here is gated on the job table. [`release_if_idle`] and
//! [`sweep`] read the chain's state from the database before touching a
//! tree, and a chain with a `running` attempt, or a `suspended` one
//! nothing has resumed yet, is never touched. The one exception is
//! [`create`] cleaning up a workspace it made itself a moment earlier for
//! a job that has not run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::domain::{FindingJobKind, JobKind, JobState};
use crate::store::Store;

/// How long a finished chain's transcript is kept.
///
/// The tree goes as soon as the chain is terminal; the transcript is only
/// read to debug a job noticed days later, which is what this bounds. Also
/// the age at which a leftover `sessions/<run-dir>` from the per-run scheme
/// goes.
pub const SESSION_RETENTION_MS: i64 = 30 * 86_400_000;

/// Seconds allowed for a local git plumbing command.
const GIT_TIMEOUT_S: u64 = 60;

/// One chain's workspace. Plain paths; nothing here exists until
/// [`create`] makes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// The chain's first job, whose id names the directory.
    pub origin_id: i64,
    /// `<work_root>/jobs/<origin_id>`.
    pub root: PathBuf,
    /// The git worktree the worker runs in.
    pub tree: PathBuf,
    /// The omp session directory.
    pub session: PathBuf,
    /// The repo clone `tree` is a worktree of.
    pub clone: PathBuf,
}

impl Workspace {
    pub fn for_chain(work_root: &Path, clone: &Path, origin_id: i64) -> Self {
        let root = jobs_root(work_root).join(origin_id.to_string());
        Self {
            origin_id,
            tree: root.join("tree"),
            session: root.join("session"),
            root,
            clone: clone.to_owned(),
        }
    }

    /// The per-repo cargo build cache a worker compiles into.
    ///
    /// The clone is fetch-only for checkouts, and this directory is the one
    /// deliberate exception. Every chain starts from a fresh tree, so a
    /// tree-local `target/` would mean a full cold compile inside the job's
    /// wall-clock limit and up to a gigabyte per live chain; the clone's
    /// own `target/` is already gitignored and warm from before trees were
    /// per-chain. Sharing it is safe because the scheduler runs one job at
    /// a time and cargo locks the directory anyway.
    pub fn build_cache(&self) -> PathBuf {
        self.clone.join("target")
    }
}

/// `<work_root>/jobs`: the parent of every chain workspace.
pub fn jobs_root(work_root: &Path) -> PathBuf {
    work_root.join("jobs")
}

/// Whether a live process has its working directory inside `tree`.
///
/// A worker runs with its chain's tree as cwd, and so does everything it
/// spawns. Under the systemd unit nothing survives the daemon
/// (`KillMode=mixed` SIGKILLs the rest of the cgroup once the main
/// process is gone), but a daemon run by hand and killed leaves its
/// worker running in its own process group. Continuing that chain would
/// put a second worker in the same tree and transcript, so the caller
/// treats a tree in use as not resumable. Processes whose cwd cannot be
/// read (another user's) are not ours and are skipped.
pub fn tree_in_use(tree: &Path) -> bool {
    let Ok(tree) = tree.canonicalize() else {
        return false;
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs
        .flatten()
        .any(|p| std::fs::read_link(p.path().join("cwd")).is_ok_and(|cwd| cwd.starts_with(&tree)))
}

/// What a cold chain's tree starts as. Revisions are resolved to a commit
/// in the clone when the tree is made; that commit is the chain's
/// `pinned_sha`.
#[derive(Debug, Clone)]
pub enum TreeSpec {
    /// Detached at a revision: hunt, recheck, harvest and the analysis kinds.
    Detached { at: String },
    /// A new local branch at a revision: fix. The branch name is per
    /// finding, so an older chain's leftover branch may still exist.
    Branch { name: String, at: String },
    /// A pull request's head, already fetched into `origin/<head_ref>`,
    /// checked out as the local `head_ref`: engage.
    PrHead { head_ref: String },
}

fn git(argv: &[&str]) -> (i32, String) {
    crate::util::run_cmd(argv, GIT_TIMEOUT_S)
}

/// `git worktree prune`, after unlocking every registration whose
/// directory is gone.
///
/// Plain prune skips a locked worktree, and git locks each new one with
/// reason `initializing` for the length of `worktree add`. A daemon killed
/// mid-add (observed 2026-09-29: a restart during a checkout slowed by a
/// full disk) leaves a registration that no prune removes and that keeps
/// its branch checked out, so `branch -D` refuses the branch and every
/// later `worktree add -b` of it fails with "a branch named ... already
/// exists" -- 400 failed fix jobs, one per cycle, until unlocked by hand.
///
/// Unlocking is safe because a lock only protects a directory that may
/// come back (removable media), and nothing does that to a clone here:
/// every worktree of one is a tree this daemon made under `work_root`.
/// A registration whose directory still exists is never touched. One that
/// is gone but was never locked is unlocked too, which git refuses
/// harmlessly; telling the two apart would buy nothing, since prune
/// removes both.
fn prune_worktrees(clone: &str) {
    let (rc, out) = git(&["git", "-C", clone, "worktree", "list", "--porcelain", "-z"]);
    if rc == 0 {
        for path in missing_worktrees(&out) {
            git(&["git", "-C", clone, "worktree", "unlock", path]);
        }
    }
    git(&["git", "-C", clone, "worktree", "prune"]);
}

/// Paths in `git worktree list --porcelain -z` output whose directory does
/// not exist. Every field is NUL-terminated, and each record's path is its
/// `worktree <path>` field.
fn missing_worktrees(porcelain: &str) -> impl Iterator<Item = &str> {
    porcelain
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree "))
        .filter(|p| !Path::new(p).exists())
}

/// The commit `rev` names in `clone`, or `None`.
pub fn resolve(clone: &Path, rev: &str) -> Option<String> {
    let c = clone.to_string_lossy();
    let spec = format!("{rev}^{{commit}}");
    let (rc, out) = git(&["git", "-C", &c, "rev-parse", "--verify", "--quiet", &spec]);
    let sha = out.trim();
    (rc == 0 && !sha.is_empty()).then(|| sha.to_owned())
}

/// Make a cold chain's workspace and return the commit its tree is at.
///
/// Refuses a workspace that already exists rather than reusing or clearing
/// it: `origin_id` is a job row inserted a moment ago, so a directory
/// already there belongs to something else — at best a stale leftover the
/// sweep will judge, at worst a live chain — and a cold worker started
/// against a session directory that already holds a transcript would be
/// continued by omp's `autoResume` instead of starting clean.
///
/// On any failure after the directory was made, everything it made is
/// removed before returning, so a job whose tree could not be built leaves
/// nothing behind.
pub fn create(ws: &Workspace, spec: &TreeSpec) -> Result<String, String> {
    if ws.root.exists() {
        return Err(format!("workspace {} already exists", ws.root.display()));
    }
    if let Err(e) = std::fs::create_dir_all(&ws.session) {
        let _ = std::fs::remove_dir_all(&ws.root);
        return Err(format!("cannot create {}: {e}", ws.session.display()));
    }
    let made = add_tree(ws, spec);
    if made.is_err() {
        release_tree(ws);
        let _ = std::fs::remove_dir_all(&ws.root);
    }
    made
}

fn add_tree(ws: &Workspace, spec: &TreeSpec) -> Result<String, String> {
    let c = ws.clone.to_string_lossy();
    let t = ws.tree.to_string_lossy();
    let failed = |what: &str, out: &str| format!("{what} failed: {}", crate::util::tail(out, 300));
    let commit = |rev: &str| {
        resolve(&ws.clone, rev).ok_or_else(|| format!("{rev} does not resolve to a commit"))
    };
    match spec {
        TreeSpec::Detached { at } => {
            let at = commit(at)?;
            let (rc, out) = git(&["git", "-C", &c, "worktree", "add", "--detach", &t, &at]);
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            Ok(at)
        }
        TreeSpec::Branch { name, at } => {
            let at = commit(at)?;
            let add = || git(&["git", "-C", &c, "worktree", "add", "-b", name, &t, &at]);
            let (rc, _) = add();
            if rc == 0 {
                return Ok(at);
            }
            // A finished chain of this finding can leave its branch behind
            // with its tree gone, and `worktree add -b` then fails with "a
            // branch named ... already exists" on every later attempt -- a
            // retry loop no amount of waiting resolves (observed
            // 2026-09-15, broken only by a restart). The branch belongs to
            // this finding, and any chain still using it was superseded
            // and had its tree released before this one was created, so
            // reclaiming the name is safe: a finding whose branch reached a
            // PR is `pr_open` and never re-enters `fix`. A tree half-made
            // by the failed attempt is removed first, or the retry's add
            // would fail on the directory instead.
            let _ = std::fs::remove_dir_all(&ws.tree);
            prune_worktrees(&c);
            git(&["git", "-C", &c, "branch", "-D", name]);
            let (rc, out) = add();
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            Ok(at)
        }
        TreeSpec::PrHead { head_ref } => {
            let remote = format!("origin/{head_ref}");
            let sha = commit(&remote)?;
            let (rc, out) = git(&["git", "-C", &c, "worktree", "add", "--detach", &t, &sha]);
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            // Best effort, as it always was: a worker left on a detached
            // HEAD at the same commit still works, and the push names its
            // refspec (`HEAD:<head_ref>`) explicitly. `origin/<head_ref>`
            // rather than the sha so the local branch tracks the remote.
            git(&["git", "-C", &t, "checkout", "-B", head_ref, &remote]);
            Ok(sha)
        }
    }
}

/// Remove a chain's tree from its clone. Never touches `session/`.
///
/// Private on purpose: this does no state check, so the only ways to reach
/// it are [`release_if_idle`] and [`sweep`], which do, and [`create`]
/// undoing its own work. A missing tree is not an error; a tree git will
/// not remove (a clone that is gone, a corrupt registration) is deleted
/// outright and its registration pruned, since the chain is finished
/// either way.
fn release_tree(ws: &Workspace) {
    // Git only when the clone is really there: `git -C ""` runs in the
    // daemon's own cwd, which may be some unrelated repository whose
    // worktrees are not ours to prune.
    let clone = ws.clone.is_dir().then(|| ws.clone.to_string_lossy());
    if ws.tree.exists() {
        if let Some(c) = &clone {
            let t = ws.tree.to_string_lossy();
            git(&["git", "-C", c, "worktree", "remove", "--force", &t]);
        }
        if ws.tree.exists() {
            let _ = std::fs::remove_dir_all(&ws.tree);
        }
    }
    if let Some(c) = &clone {
        prune_worktrees(c);
    }
}

/// Release a chain's tree if, according to the job table, no attempt of the
/// chain is `running` or awaiting resume ([`Store::chain_status`]'s `live`).
/// Returns whether it released.
///
/// The executors call this after recording a job's outcome, and for every
/// chain a fresh job superseded; both are the moment a chain becomes
/// terminal. Reading the state back rather than trusting the caller's
/// local copy is what keeps a suspended chain's tree safe from every
/// caller, including ones written later.
pub async fn release_if_idle(store: &Store, ws: &Workspace) -> anyhow::Result<bool> {
    let status = store.chain_status(ws.origin_id).await?;
    if status.live > 0 {
        return Ok(false);
    }
    let ws = ws.clone();
    tokio::task::spawn_blocking(move || release_tree(&ws)).await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Renovate scan tree
// ---------------------------------------------------------------------------

/// A throwaway tree for the zero-token Renovate scan of `dep_update`.
///
/// Renovate reads the manifests of a working tree, and the clone's own
/// working tree is never updated now that the clone is fetch-only. The scan
/// creates no job row, so it has no chain and no workspace; it gets this
/// tree at `<work_root>/scan/repo-<id>` instead, made and removed within
/// the one call. Nothing in the job table ever refers to it, and a leftover
/// from a crash mid-scan is removed by [`sweep`], which only runs between
/// cycles, when no scan can be in progress.
pub struct ScanTree {
    path: PathBuf,
    clone: PathBuf,
}

impl ScanTree {
    pub fn create(work_root: &Path, clone: &Path, repo_id: i64, at: &str) -> Result<Self, String> {
        let path = scan_root(work_root).join(format!("repo-{repo_id}"));
        let tree = Self {
            path,
            clone: clone.to_owned(),
        };
        // A leftover from an interrupted scan of this repo: same owner,
        // same purpose, and no job refers to it.
        tree.discard();
        if let Some(parent) = tree.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let c = tree.clone.to_string_lossy();
        let p = tree.path.to_string_lossy();
        let (rc, out) = git(&["git", "-C", &c, "worktree", "add", "--detach", &p, at]);
        if rc != 0 {
            tree.discard();
            return Err(format!(
                "scan worktree add failed: {}",
                crate::util::tail(&out, 300)
            ));
        }
        Ok(tree)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn discard(&self) {
        let c = self.clone.to_string_lossy();
        if self.path.exists() {
            let p = self.path.to_string_lossy();
            git(&["git", "-C", &c, "worktree", "remove", "--force", &p]);
            let _ = std::fs::remove_dir_all(&self.path);
        }
        if self.clone.is_dir() {
            prune_worktrees(&c);
        }
    }
}

impl Drop for ScanTree {
    fn drop(&mut self) {
        self.discard();
    }
}

fn scan_root(work_root: &Path) -> PathBuf {
    work_root.join("scan")
}

// ---------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------

/// What one [`sweep`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub trees_released: usize,
    pub workspaces_removed: usize,
    pub legacy_trees_removed: usize,
    pub legacy_trees_kept: usize,
    pub legacy_sessions_removed: usize,
}

/// Reclaim what finished chains, and the layouts before this one, left on
/// disk. Runs at daemon startup and before every cycle, never during one.
///
/// Nothing is removed without first reading from the job table that no
/// `running` or `suspended` job needs it. First, suspensions whose finding
/// has moved on are retired ([`Store::retire_stranded_suspensions`]), so a
/// checkpoint nothing will ever resume does not hold its tree forever:
///
/// (a) `<work_root>/jobs/<id>/`: a chain with no live attempt has its tree
///     released (normally the executor already did; this catches a job
///     that crashed between recording its outcome and releasing). Once the
///     chain's latest attempt finished more than [`SESSION_RETENTION_MS`]
///     ago the whole workspace, transcript included, goes. A workspace no
///     job row owns is aged by its directory's mtime.
/// (b) `<work_root>/wt/*`, the finding-keyed trees of the previous layout:
///     removed, unless a live job's session file lies inside one or the
///     old per-kind naming (`f`/`e`/`h` + finding id) says a live job ran
///     there. Those are kept and logged once.
/// (c) `<work_root>/sessions/<run-dir>`, the per-run transcripts of the
///     previous layout: removed once older than [`SESSION_RETENTION_MS`],
///     aged by the finish time of the jobs that recorded a transcript in
///     it, else by its mtime. Never one a live job recorded.
/// (d) `git worktree prune` on every repo clone, so a tree removed by hand
///     or by a killed `worktree add` does not keep its branch checked out.
/// (e) `<work_root>/scan/*`: leftovers of an interrupted Renovate scan.
///
/// Kept, always, whatever the job table says: the clones under
/// `<work_root>/repos/`, including each clone's `target/`, which is the
/// per-repo cargo build cache every worker compiles into (see
/// [`Workspace::build_cache`]); `<work_root>/out/`; and a chain's
/// `session/` while its workspace is inside retention.
pub async fn sweep(store: &Store, work_root: &Path, now_ms: i64) -> anyhow::Result<SweepReport> {
    let mut report = SweepReport::default();
    store.retire_stranded_suspensions().await?;
    sweep_chains(store, work_root, now_ms, &mut report).await?;
    sweep_legacy_trees(store, work_root, &mut report).await?;
    sweep_legacy_sessions(store, work_root, now_ms, &mut report).await?;
    prune_clones_and_scans(store, work_root).await?;
    Ok(report)
}

/// Step (a) of [`sweep`].
async fn sweep_chains(
    store: &Store,
    work_root: &Path,
    now_ms: i64,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    for (origin_id, root) in numbered_dirs(&jobs_root(work_root)) {
        let status = store.chain_status(origin_id).await?;
        if status.live > 0 {
            continue;
        }
        let clone = status
            .clone
            .as_deref()
            .map_or_else(PathBuf::new, PathBuf::from);
        let ws = Workspace::for_chain(work_root, &clone, origin_id);
        // A workspace no row owns, or whose rows never finished, is aged by
        // its directory instead.
        let last = status.last_finished_at.or_else(|| mtime_ms(&root));
        let expired = last.is_some_and(|t| now_ms - t > SESSION_RETENTION_MS);
        let had_tree = ws.tree.exists();
        tokio::task::spawn_blocking(move || {
            release_tree(&ws);
            if expired {
                let _ = std::fs::remove_dir_all(&ws.root);
            }
        })
        .await?;
        report.trees_released += usize::from(had_tree);
        report.workspaces_removed += usize::from(expired && !root.exists());
    }
    Ok(())
}

/// Step (b) of [`sweep`].
async fn sweep_legacy_trees(
    store: &Store,
    work_root: &Path,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let wt_root = work_root.join("wt");
    let entries = child_dirs(&wt_root);
    if entries.is_empty() {
        let _ = std::fs::remove_dir(&wt_root);
        return Ok(());
    }
    let live = store.live_jobs().await?;
    for entry in entries {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let referenced = live.iter().any(|j| {
            j.session_file
                .as_deref()
                .is_some_and(|sf| Path::new(sf).starts_with(&entry))
                || legacy_wt_name(j.kind, j.finding_id).as_deref() == Some(name.as_str())
        });
        if referenced {
            report.legacy_trees_kept += 1;
            log_once(&entry, || {
                tracing::info!(
                    "sweep: keeping legacy tree {} -- a running or suspended job still uses it",
                    entry.display()
                );
            });
            continue;
        }
        let path = entry.clone();
        tokio::task::spawn_blocking(move || remove_legacy_tree(&path)).await?;
        report.legacy_trees_removed += usize::from(!entry.exists());
    }
    let _ = std::fs::remove_dir(&wt_root);
    Ok(())
}

/// Step (c) of [`sweep`].
async fn sweep_legacy_sessions(
    store: &Store,
    work_root: &Path,
    now_ms: i64,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let sessions_root = work_root.join("sessions");
    let dirs = child_dirs(&sessions_root);
    if dirs.is_empty() {
        let _ = std::fs::remove_dir(&sessions_root);
        return Ok(());
    }
    let prefix = format!("{}/", sessions_root.display());
    let mut refs: BTreeMap<String, Vec<(JobState, Option<i64>)>> = BTreeMap::new();
    for r in store.jobs_with_session_under(&prefix).await? {
        let Some(run_dir) = r.session_file[prefix.len()..].split('/').next() else {
            continue;
        };
        refs.entry(run_dir.to_owned())
            .or_default()
            .push((r.state, r.finished_at));
    }
    for dir in dirs {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let jobs = refs.get(&name).map_or(&[][..], Vec::as_slice);
        if jobs
            .iter()
            .any(|(s, _)| matches!(s, JobState::Running | JobState::Suspended))
        {
            continue;
        }
        let aged = jobs
            .iter()
            .filter_map(|(_, f)| *f)
            .max()
            .or_else(|| mtime_ms(&dir));
        if aged.is_some_and(|t| now_ms - t > SESSION_RETENTION_MS) {
            let d = dir.clone();
            tokio::task::spawn_blocking(move || {
                let _ = std::fs::remove_dir_all(&d);
            })
            .await?;
            report.legacy_sessions_removed += usize::from(!dir.exists());
        }
    }
    let _ = std::fs::remove_dir(&sessions_root);
    Ok(())
}

/// Steps (d) and (e) of [`sweep`].
async fn prune_clones_and_scans(store: &Store, work_root: &Path) -> anyhow::Result<()> {
    let clones: Vec<PathBuf> = store
        .list_repos()
        .await?
        .into_iter()
        .map(|r| PathBuf::from(r.path))
        .filter(|p| p.is_dir())
        .collect();
    let scans = child_dirs(&scan_root(work_root));
    tokio::task::spawn_blocking(move || {
        for scan in &scans {
            remove_legacy_tree(scan);
        }
        for clone in &clones {
            prune_worktrees(&clone.to_string_lossy());
        }
    })
    .await?;
    Ok(())
}

/// The directory the previous layout ran a finding kind in, by name.
fn legacy_wt_name(kind: JobKind, finding_id: Option<i64>) -> Option<String> {
    let prefix = match kind {
        JobKind::Finding(FindingJobKind::Fix) => 'f',
        JobKind::Finding(FindingJobKind::Engage) => 'e',
        JobKind::Finding(FindingJobKind::Harvest) => 'h',
        _ => return None,
    };
    finding_id.map(|fid| format!("{prefix}{fid}"))
}

/// Remove a worktree whose owning clone is not known: ask it for its
/// clone, let git unregister it, and delete whatever is left — including a
/// half-removed leftover with no `.git` entry at all.
fn remove_legacy_tree(path: &Path) {
    let p = path.to_string_lossy();
    if path.join(".git").exists() {
        let (rc, out) = git(&[
            "git",
            "-C",
            &p,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ]);
        let common = PathBuf::from(out.trim());
        if rc == 0
            && let Some(clone) = common.parent()
        {
            git(&[
                "git",
                "-C",
                &clone.to_string_lossy(),
                "worktree",
                "remove",
                "--force",
                &p,
            ]);
        }
    }
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
}

fn child_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// Workspace directories, by chain id. Anything not named by a job id is
/// not a workspace and is left alone.
fn numbered_dirs(dir: &Path) -> Vec<(i64, PathBuf)> {
    child_dirs(dir)
        .into_iter()
        .filter_map(|p| {
            let id = p.file_name()?.to_str()?.parse::<i64>().ok()?;
            Some((id, p))
        })
        .collect()
}

fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let ms = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    i64::try_from(ms).ok()
}

/// Log a kept legacy tree once per daemon lifetime, not once per cycle.
fn log_once(path: &Path, log: impl FnOnce()) {
    static SEEN: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
    if let Ok(mut seen) = SEEN.lock()
        && seen.insert(path.to_owned())
    {
        log();
    }
}
