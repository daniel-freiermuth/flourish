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
//! Every git command here that changes a clone runs under that clone's
//! [`ClonePermit`] (`crate::clones`).
//!
//! Every removal here is gated on the job table and on the runners'
//! [`TreeLease`]s. [`release_if_idle`] and [`sweep`] read the chain's
//! state from the database before touching a tree, and a chain with a
//! `running` attempt or a checkpoint (a finding work item's
//! `checkpoint_job_id`, or a repo-level `suspended` attempt), or a tree a
//! runner still holds, is never touched. The one exception is [`create`]
//! cleaning up a workspace it made itself a moment earlier for a job that
//! has not run.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::clones::{CloneHold, ClonePermit, ForeignChain, Lane, TreeLease, Unheld};
use crate::domain::{FindingJobKind, JobKind, JobState};
use crate::recovery::AdminAuthority;
use crate::store::{ChainRef, ChainStatus, CloneLockError, CloneRef, Store};

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

    /// Chain `origin_id`'s workspace in `clone`: how every function that
    /// changes a tree names it, from the permit or hold it changes it
    /// under, so the tree they change and the clone they hold are one by
    /// construction.
    pub fn in_clone(work_root: &Path, clone: &CloneRef, origin_id: i64) -> Self {
        Self::for_chain(work_root, clone.dir(), origin_id)
    }

    /// The per-repo cargo build cache a worker compiles into.
    ///
    /// The clone is fetch-only for checkouts, and this directory is the one
    /// deliberate exception. Every chain starts from a fresh tree, so a
    /// tree-local `target/` would mean a full cold compile inside the job's
    /// wall-clock limit and up to a gigabyte per live chain; the clone's
    /// own `target/` is already gitignored and warm from before trees were
    /// per-chain. Sharing it is safe because the job lane runs one job at
    /// a time (`recovery::JobAuthority`) and cargo locks the directory anyway.
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
fn prune_worktrees(permit: &CloneHold) {
    let (rc, out) = permit.git(&["worktree", "list", "--porcelain", "-z"], GIT_TIMEOUT_S);
    if rc == 0 {
        for path in missing_worktrees(&out) {
            permit.git(&["worktree", "unlock", path], GIT_TIMEOUT_S);
        }
    }
    permit.git(&["worktree", "prune"], GIT_TIMEOUT_S);
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

/// `git fetch origin <refs>` into the held clone, then the housekeeping
/// git would have run after it, both on the caller's blocking thread and
/// so both inside the hold.
///
/// Git ends a fetch -- and a commit, which workers make in their trees of
/// this clone -- by starting `git maintenance run --auto --detach`: a
/// process of its own that repacks, packs refs, expires reflogs and
/// prunes worktrees in the clone after the command that started it has
/// returned, so after the permit is let go. Under the next holder that
/// takes `packed-refs.lock` from a `branch -D`, or prunes the
/// registration of a `worktree add` still under way. So the fetch runs
/// with automatic maintenance off for that one command
/// (`-c maintenance.auto=false`, which its children inherit), and runs the
/// housekeeping here instead, attached. `--auto` keeps git's own
/// thresholds: most fetches find nothing worth doing.
///
/// Per command, not written to the clone's config: a fetch must not fail
/// because the config cannot be rewritten (a stale `config.lock` left by a
/// killed `git config`), and a repo row may name the operator's own
/// checkout, whose config is not ours to change. Worker commits, which
/// pass no such flag, are covered in the daemon's own clones by their
/// config ([`quiet_clones`], [`CloneHold::clone_from`]).
pub fn fetch(hold: &CloneHold, refs: &[&str], timeout_s: u64) -> (i32, String) {
    let mut args = vec!["-c", "maintenance.auto=false", "fetch", "origin"];
    args.extend_from_slice(refs);
    let fetched = hold.git(&args, timeout_s);
    if fetched.0 == 0 {
        let (rc, out) = hold.git(
            &[
                "-c",
                "maintenance.autoDetach=false",
                "-c",
                "gc.autoDetach=false",
                "maintenance",
                "run",
                "--auto",
            ],
            timeout_s,
        );
        if rc != 0 {
            // The fetch itself succeeded; housekeeping is retried by the
            // next one.
            tracing::warn!(
                "maintenance in {} failed: {}",
                hold.clone_dir().display(),
                crate::util::tail(&out, 300)
            );
        }
    }
    fetched
}

/// Turn off git's automatic maintenance in the held clone:
/// `maintenance.auto`, written to the clone's own config, which its
/// worktrees share, so a worker's commit in its tree starts none either.
/// See [`fetch`] for why.
fn disable_auto_maintenance(hold: &CloneHold) -> (i32, String) {
    hold.git(&["config", "maintenance.auto", "false"], GIT_TIMEOUT_S)
}

/// [`disable_auto_maintenance`] in every clone of the daemon's own on
/// disk, each under its permit. Runs once at daemon startup, before any
/// job.
///
/// A clone made from now on gets the setting when it is made
/// ([`CloneHold::clone_from`]). But a fix, and every resume, makes or
/// reuses a tree without fetching: in a clone that predates the setting,
/// a worker's commit would still start detached maintenance under the
/// next holder.
///
/// Only the daemon's own clones (`<work_root>/repos/repo-<id>`): a repo
/// row whose path names the operator's own checkout (kept by migration
/// 007) keeps the operator's config. Best effort per clone, as the sweep
/// is: a clone whose config cannot be written is logged.
pub async fn quiet_clones(lane: &mut Lane, work_root: &Path) -> sqlx::Result<()> {
    let repos_dir = work_root.join("repos");
    for repo_id in lane.store().clone_ids().await? {
        let permit = match lane.lock_clone(repo_id).await {
            Ok(permit) => permit,
            // Gone since it was listed: its clone went with it.
            Err(CloneLockError::NoRepo(_)) => continue,
            Err(CloneLockError::Db(e)) => return Err(e),
        };
        // Ours when the row's path is `<work_root>/repos/repo-<id>`, as
        // the filesystem resolves both. A clone not made yet fails to
        // resolve too: the sync that makes it sets the config.
        let ours = std::fs::canonicalize(Store::repo_dir(&repos_dir, repo_id))
            .is_ok_and(|own| std::fs::canonicalize(permit.clone_dir()).is_ok_and(|dir| dir == own));
        if !ours {
            continue;
        }
        let (rc, out) = permit
            .blocking(disable_auto_maintenance)
            .await
            .unwrap_or((127, "spawn error".to_owned()));
        if rc != 0 {
            tracing::warn!(
                "{}: disabling automatic maintenance: {}",
                permit.clone_dir().display(),
                crate::util::tail(&out, 300)
            );
        }
    }
    Ok(())
}

/// The commit `rev` names in `clone` (or one of its trees), or `None`.
pub fn resolve(clone: &Path, rev: &str) -> Option<String> {
    let (rc, out) = Unheld::Commit { rev }.run(clone, GIT_TIMEOUT_S);
    let sha = out.trim();
    (rc == 0 && !sha.is_empty()).then(|| sha.to_owned())
}

/// Make chain `origin_id`'s workspace in the held clone, and return the
/// commit its tree is at. The workspace is the hold's
/// ([`Workspace::in_clone`]): no caller names the clone a tree goes in.
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
pub fn create(
    permit: &CloneHold,
    work_root: &Path,
    origin_id: i64,
    spec: &TreeSpec,
) -> Result<String, String> {
    let ws = Workspace::in_clone(work_root, permit.clone_ref(), origin_id);
    if ws.root.exists() {
        return Err(format!("workspace {} already exists", ws.root.display()));
    }
    if let Err(e) = std::fs::create_dir_all(&ws.session) {
        let _ = std::fs::remove_dir_all(&ws.root);
        return Err(format!("cannot create {}: {e}", ws.session.display()));
    }
    let made = add_tree(permit, &ws.tree, spec);
    if made.is_err() {
        release_tree(Some(permit), &ws.tree);
        let _ = std::fs::remove_dir_all(&ws.root);
    }
    made
}

fn add_tree(permit: &CloneHold, tree: &Path, spec: &TreeSpec) -> Result<String, String> {
    let t = tree.to_string_lossy();
    let failed = |what: &str, out: &str| format!("{what} failed: {}", crate::util::tail(out, 300));
    let commit = |rev: &str| {
        resolve(permit.clone_dir(), rev)
            .ok_or_else(|| format!("{rev} does not resolve to a commit"))
    };
    let git = |args: &[&str]| permit.git(args, GIT_TIMEOUT_S);
    match spec {
        TreeSpec::Detached { at } => {
            let at = commit(at)?;
            let (rc, out) = git(&["worktree", "add", "--detach", &t, &at]);
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            Ok(at)
        }
        TreeSpec::Branch { name, at } => {
            let at = commit(at)?;
            let add = || git(&["worktree", "add", "-b", name, &t, &at]);
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
            let _ = std::fs::remove_dir_all(tree);
            prune_worktrees(permit);
            git(&["branch", "-D", name]);
            let (rc, out) = add();
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            Ok(at)
        }
        TreeSpec::PrHead { head_ref } => {
            let remote = format!("origin/{head_ref}");
            let sha = commit(&remote)?;
            let (rc, out) = git(&["worktree", "add", "--detach", &t, &sha]);
            if rc != 0 {
                return Err(failed("worktree add", &out));
            }
            // Best effort, as it always was: a worker left on a detached
            // HEAD at the same commit still works, and the push names its
            // refspec (`HEAD:<head_ref>`) explicitly. `origin/<head_ref>`
            // rather than the sha so the local branch tracks the remote.
            permit.git_in(tree, &["checkout", "-B", head_ref, &remote], GIT_TIMEOUT_S);
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
/// either way. `clone` is `None` for a chain whose repo is gone: there is
/// no registration left to remove, only the directory.
fn release_tree(clone: Option<&CloneHold>, tree: &Path) {
    // Git only when the clone is really there: `git -C <missing>` fails,
    // and nothing would be left to prune.
    let clone = clone.filter(|p| p.clone_dir().is_dir());
    if tree.exists() {
        if let Some(p) = clone {
            let t = tree.to_string_lossy();
            p.git(&["worktree", "remove", "--force", &t], GIT_TIMEOUT_S);
        }
        if tree.exists() {
            let _ = std::fs::remove_dir_all(tree);
        }
    }
    if let Some(p) = clone {
        prune_worktrees(p);
    }
}

/// Release `chain`'s tree in the permit's clone if, according to the
/// database, no attempt of the chain is `running` or a checkpoint awaiting
/// resume (see [`Store::chain_status`]), and no runner holds a lease on
/// it. Returns whether it released.
///
/// The executors call this for every chain a fresh job superseded, and
/// [`OpenWorkspace::close`] the same check with the runner's own lease
/// discounted, after recording a job's outcome; both are the moment a
/// chain becomes terminal. Reading the state back rather than trusting the
/// caller's local copy is what keeps a suspended chain's tree safe from
/// every caller, including ones written later; the leases are what keep a
/// tree safe between a job being recorded and its runner being done with
/// it. A chain of another clone is refused ([`ForeignChain`]): its leases
/// are taken under its own clone's permit, not this one.
pub async fn release_if_idle(
    store: &Store,
    permit: &ClonePermit<'_>,
    work_root: &Path,
    chain: &ChainRef,
) -> anyhow::Result<bool> {
    permit.holds_chain(chain)?;
    let leased = store.clones().chain_leases(chain.origin_id()) > 0;
    let ws = Workspace::in_clone(work_root, permit.clone_ref(), chain.origin_id());
    release_unless_live(store, permit, &ws, leased).await
}

/// [`release_if_idle`] once whether another runner leases the tree is
/// known.
async fn release_unless_live(
    store: &Store,
    permit: &ClonePermit<'_>,
    ws: &Workspace,
    leased: bool,
) -> anyhow::Result<bool> {
    if leased || store.chain_status(ws.origin_id).await?.live > 0 {
        return Ok(false);
    }
    let tree = ws.tree.clone();
    permit
        .blocking(move |hold| release_tree(Some(hold), &tree))
        .await?;
    Ok(true)
}

/// A chain's workspace as the runner of one of its attempts holds it: the
/// paths, which repo's clone the tree is in, and the runner's
/// [`TreeLease`], taken before the tree exists. Ended by [`Self::close`]
/// once the runner is done with the tree, or by being dropped, which ends
/// the lease and leaves the tree to the sweep.
pub struct OpenWorkspace {
    ws: Workspace,
    clone: CloneRef,
    lease: TreeLease,
}

impl OpenWorkspace {
    /// Hold `chain`'s tree in the permit's clone ([`ClonePermit::lease`],
    /// which refuses a chain of another clone). Its paths are the
    /// permit's ([`Workspace::in_clone`]), so the lease, the tree and the
    /// clone [`Self::close`] locks again are one.
    pub fn lease(
        permit: &ClonePermit<'_>,
        work_root: &Path,
        chain: &ChainRef,
    ) -> Result<Self, ForeignChain> {
        Ok(Self {
            lease: permit.lease(chain)?,
            clone: permit.clone_ref().clone(),
            ws: Workspace::in_clone(work_root, permit.clone_ref(), chain.origin_id()),
        })
    }

    /// Release the tree unless the chain is still live or another runner
    /// holds it ([`release_if_idle`], this runner's own lease discounted),
    /// taking the clone's permit for it in the runner's `lane`. Consumes
    /// the workspace: once closed, its tree may be gone, so nothing can
    /// reach it through this value again, nor close it twice.
    pub async fn close(self, lane: &mut Lane) -> anyhow::Result<bool> {
        let store = lane.store().clone();
        let permit = lane.lock(&self.clone).await;
        release_unless_live(&store, &permit, &self.ws, self.lease.shared()).await
    }
}

impl std::ops::Deref for OpenWorkspace {
    type Target = Workspace;

    fn deref(&self) -> &Workspace {
        &self.ws
    }
}

// ---------------------------------------------------------------------------
// Renovate scan tree
// ---------------------------------------------------------------------------

/// A throwaway tree for the zero-token Renovate scan of `dep_update`.
///
/// Renovate reads the manifests of a working tree, and the clone's own
/// working tree is never updated now that the clone is fetch-only. The scan
/// creates no job row, so it has no chain and no workspace; it gets this
/// tree at `<work_root>/scan/repo-<id>` instead.
///
/// The tree is made and removed under the clone's permit, and kept in
/// between by the repo's scan lease ([`ClonePermit::lease_scan`]) rather
/// than by the permit: Renovate's local dry-run lookup only reads the
/// tree, and its lookups go to registries and the forge, so the clone
/// stays free for others while they run. The lease, taken under the
/// permit, refuses a second scan of the repo, which would share the path,
/// and keeps the sweep, which removes scan leftovers under the same
/// permit, off a live tree.
///
/// Removed by [`Self::discard`]. One dropped instead (a cancelled cycle)
/// ends its lease and leaves the tree as a leftover, which the next scan
/// of the repo or the sweep removes.
pub struct ScanTree {
    path: PathBuf,
    clone: CloneRef,
    _lease: TreeLease,
}

impl ScanTree {
    /// Make the permit's repo's scan tree at `at`, and let the clone go:
    /// the permit is consumed once the tree exists.
    pub async fn create(
        work_root: &Path,
        permit: ClonePermit<'_>,
        at: &str,
    ) -> Result<Self, String> {
        let lease = permit.lease_scan().map_err(|e| e.to_string())?;
        let path = scan_tree_path(work_root, permit.repo_id());
        let (p, at) = (path.clone(), at.to_owned());
        permit
            .blocking(move |hold| make_scan_tree(hold, &p, &at))
            .await
            .map_err(|e| format!("scan worktree add: {e}"))??;
        Ok(Self {
            path,
            clone: permit.clone_ref().clone(),
            _lease: lease,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the tree, taking the clone's permit again in `lane`. The
    /// lease ends after the removal, still under the permit.
    pub async fn discard(self, lane: &mut Lane) {
        let Self {
            path,
            clone,
            _lease: lease,
        } = self;
        let permit = lane.lock(&clone).await;
        let p = path.clone();
        if let Err(e) = permit
            .blocking(move |hold| discard_scan_tree(hold, &p))
            .await
        {
            tracing::warn!("removing scan tree {}: {e}", path.display());
        }
        drop(lease);
        drop(permit);
    }
}

/// Make the scan tree at `path` in the held clone, first removing any
/// leftover of an interrupted scan: same owner, same purpose, and the
/// caller's lease says no scan is using it.
fn make_scan_tree(hold: &CloneHold, path: &Path, at: &str) -> Result<(), String> {
    discard_scan_tree(hold, path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let p = path.to_string_lossy();
    let (rc, out) = hold.git(&["worktree", "add", "--detach", &p, at], GIT_TIMEOUT_S);
    if rc != 0 {
        discard_scan_tree(hold, path);
        return Err(format!(
            "scan worktree add failed: {}",
            crate::util::tail(&out, 300)
        ));
    }
    Ok(())
}

/// `<work_root>/scan/repo-<id>`: the one scan tree repo `repo_id` has.
fn scan_tree_path(work_root: &Path, repo_id: i64) -> PathBuf {
    scan_root(work_root).join(format!("repo-{repo_id}"))
}

fn discard_scan_tree(permit: &CloneHold, path: &Path) {
    if path.exists() {
        let p = path.to_string_lossy();
        permit.git(&["worktree", "remove", "--force", &p], GIT_TIMEOUT_S);
        let _ = std::fs::remove_dir_all(path);
    }
    if permit.clone_dir().is_dir() {
        prune_worktrees(permit);
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
/// disk. Runs at daemon startup and as the admin lane's sweep duty
/// (`daemon::admin_duty`), beside whatever job is running.
///
/// Nothing is removed without first reading from the database that no
/// `running` job or checkpoint needs it, and no tree a runner leases. A
/// finding's checkpoint goes when the finding leaves its kind's tier
/// (migration 024's trigger), so a checkpoint nothing will ever resume
/// does not hold its tree forever. Every change of a clone is made under
/// that clone's [`ClonePermit`]:
///
/// (a) `<work_root>/jobs/<id>/`: a chain with no live attempt and no lease
///     has its tree released (normally the executor already did; this
///     catches a job that crashed between recording its outcome and
///     releasing). Once the chain's latest attempt finished more than
///     [`SESSION_RETENTION_MS`] ago the whole workspace, transcript
///     included, goes. A workspace no job row owns is aged by its
///     directory's mtime.
/// (b) `<work_root>/wt/*`, the finding-keyed trees of the previous layout:
///     removed, unless a live job's session file lies inside one or the
///     old per-kind naming (`f`/`e`/`h` + finding id) says a live job ran
///     there. Those are kept and logged once. The owning clone of such a
///     tree is only known to git, so this step holds every repo's permit.
/// (c) `<work_root>/sessions/<run-dir>`, the per-run transcripts of the
///     previous layout: removed once older than [`SESSION_RETENTION_MS`],
///     aged by the finish time of the jobs that recorded a transcript in
///     it, else by its mtime. Never one a live job recorded.
/// (d) `git worktree prune` on every repo clone, so a tree removed by hand
///     or by a killed `worktree add` does not keep its branch checked out.
///     Every clone means every repo row's, including one flagged deleted:
///     its clone stays on disk until it is reaped.
/// (e) `<work_root>/scan/*`: leftovers of an interrupted Renovate scan. A
///     live scan holds its repo's permit, so one is never removed.
///
/// Kept, always, whatever the job table says: the clones under
/// `<work_root>/repos/`, including each clone's `target/`, which is the
/// per-repo cargo build cache every worker compiles into (see
/// [`Workspace::build_cache`]); `<work_root>/out/`; and a chain's
/// `session/` while its workspace is inside retention.
///
/// The admin lane's alone: it takes the [`AdminAuthority`], and runs in a
/// lane of its own.
pub async fn sweep(
    admin: &AdminAuthority,
    work_root: &Path,
    now_ms: i64,
) -> anyhow::Result<SweepReport> {
    admin
        .store()
        .lane(async |lane| sweep_in(lane, work_root, now_ms).await)
        .await?
}

/// [`sweep`] in its lane.
async fn sweep_in(lane: &mut Lane, work_root: &Path, now_ms: i64) -> anyhow::Result<SweepReport> {
    let mut report = SweepReport::default();
    sweep_chains(lane, work_root, now_ms, &mut report).await?;
    sweep_legacy_trees(lane, work_root, &mut report).await?;
    sweep_legacy_sessions(lane.store(), work_root, now_ms, &mut report).await?;
    prune_clones_and_scans(lane, work_root).await?;
    Ok(report)
}

/// Step (a) of [`sweep`].
async fn sweep_chains(
    lane: &mut Lane,
    work_root: &Path,
    now_ms: i64,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let store = lane.store().clone();
    let in_use = |status: &ChainStatus, origin_id| {
        status.live > 0 || store.clones().chain_leases(origin_id) > 0
    };
    for (origin_id, root) in numbered_dirs(&jobs_root(work_root)) {
        let status = store.chain_status(origin_id).await?;
        if in_use(&status, origin_id) {
            continue;
        }
        let permit = match &status.clone {
            Some(clone) => Some(lane.lock(clone).await),
            None => None,
        };
        // Again under the permit: the first look only named the clone.
        let status = store.chain_status(origin_id).await?;
        if in_use(&status, origin_id) {
            continue;
        }
        let ws = Workspace::for_chain(
            work_root,
            permit.as_ref().map_or(Path::new(""), |p| p.clone_dir()),
            origin_id,
        );
        // A workspace no row owns, or whose rows never finished, is aged by
        // its directory instead.
        let last = status.last_finished_at.or_else(|| mtime_ms(&root));
        let expired = last.is_some_and(|t| now_ms - t > SESSION_RETENTION_MS);
        let had_tree = ws.tree.exists();
        let release = move |hold: Option<&CloneHold>| {
            release_tree(hold, &ws.tree);
            if expired {
                let _ = std::fs::remove_dir_all(&ws.root);
            }
        };
        match &permit {
            Some(p) => p.blocking(move |hold| release(Some(hold))).await?,
            None => tokio::task::spawn_blocking(move || release(None)).await?,
        }
        report.trees_released += usize::from(had_tree);
        report.workspaces_removed += usize::from(expired && !root.exists());
    }
    Ok(())
}

/// Step (b) of [`sweep`]. Every clone's permit is taken only when there is
/// a tree to remove, not for one a live job keeps: a pass that removes
/// nothing waits for no runner.
async fn sweep_legacy_trees(
    lane: &mut Lane,
    work_root: &Path,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let wt_root = work_root.join("wt");
    let entries = child_dirs(&wt_root);
    if entries.is_empty() {
        let _ = std::fs::remove_dir(&wt_root);
        return Ok(());
    }
    let live = lane.store().live_jobs().await?;
    let mut removable = Vec::new();
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
        } else {
            removable.push(entry);
        }
    }
    if removable.is_empty() {
        return Ok(());
    }
    let every = lane.lock_every().await?;
    for entry in removable {
        let path = entry.clone();
        every
            .blocking(move |held| remove_legacy_tree(held, &path))
            .await?;
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

/// Steps (d) and (e) of [`sweep`], one clone at a time under its permit.
/// A scan tree of a repo no longer listed is removed with every clone
/// held, as its clone is only known to git. A scan tree a running scan
/// leases is kept: the lease is taken under the same permit.
async fn prune_clones_and_scans(lane: &mut Lane, work_root: &Path) -> anyhow::Result<()> {
    let store = lane.store().clone();
    let mut scans: BTreeSet<PathBuf> = child_dirs(&scan_root(work_root)).into_iter().collect();
    for repo_id in store.clone_ids().await? {
        let permit = match lane.lock_clone(repo_id).await {
            Ok(permit) => permit,
            // Gone since it was listed: its clone went with it.
            Err(CloneLockError::NoRepo(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        let scan = scan_tree_path(work_root, repo_id);
        scans.remove(&scan);
        let leased = store.clones().scan_leased(repo_id);
        permit
            .blocking(move |hold| {
                if !leased {
                    discard_scan_tree(hold, &scan);
                }
                if hold.clone_dir().is_dir() {
                    prune_worktrees(hold);
                }
            })
            .await?;
    }
    scans.retain(|scan| {
        scan_tree_repo(scan).is_none_or(|repo_id| !store.clones().scan_leased(repo_id))
    });
    if !scans.is_empty() {
        let every = lane.lock_every().await?;
        every
            .blocking(move |held| {
                for scan in &scans {
                    remove_legacy_tree(held, scan);
                }
            })
            .await?;
    }
    Ok(())
}

/// The repo id a `scan/repo-<id>` directory is named for.
fn scan_tree_repo(scan: &Path) -> Option<i64> {
    scan.file_name()?
        .to_str()?
        .strip_prefix("repo-")?
        .parse()
        .ok()
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
/// clone, let git unregister it there if `held` holds that clone, and
/// delete whatever is left -- including a half-removed leftover with no
/// `.git` entry at all.
///
/// The clone could be any repo's, so the caller holds every one it knows.
/// One it does not hold (a clone no repo row names) is never changed: the
/// directory still goes, and its registration is left for git to drop as
/// prunable from a clone whose permit someone holds.
fn remove_legacy_tree(held: &[CloneHold], path: &Path) {
    let p = path.to_string_lossy();
    if path.join(".git").exists()
        && let Some(clone) = crate::clones::clone_of_tree(path)
        && let Some(hold) = held.iter().find(|h| h.is_clone(&clone))
    {
        hold.git(&["worktree", "remove", "--force", &p], GIT_TIMEOUT_S);
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
