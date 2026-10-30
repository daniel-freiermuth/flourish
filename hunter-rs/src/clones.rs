//! Who may change a repo's clone, and which chain trees a runner holds.
//!
//! Two things on disk are shared between whoever runs at the same time:
//!
//! - A repo's clone (`repos/repo-<id>`): its refs, its object store and
//!   its worktree registry. A fetch, a `worktree add`, a prune, a
//!   `remote set-url`, removing the clone: each is a read-modify-write of
//!   that state, and two at once corrupt it in ways git does not catch --
//!   a prune unlocks the registration of a `worktree add` still under way,
//!   a clone removed mid-fetch is recreated half-made. Every function
//!   that changes a clone takes a [`ClonePermit`] for it (or, on the
//!   blocking thread it runs git on, the permit's [`CloneHold`]), and the
//!   only way to a permit is [`Lane::lock_clone`], which waits for the
//!   clone's previous holder. So no two changes of one clone overlap, and
//!   code that has no permit cannot call them. A permit names its clone by
//!   a [`CloneRef`], which only the store's own reads of a repo row make
//!   (by repo id, or a chain's
//!   [`ChainStatus::clone`](crate::store::ChainStatus::clone)), so the
//!   repo it locks and the directory it changes cannot disagree. Git runs
//!   only from here: in a held clone or its trees through the hold
//!   ([`CloneHold::git`], [`CloneHold::git_in`], [`CloneHold::clone_from`]),
//!   and without one only as an [`Unheld`] command, none of which changes
//!   a clone. `check-git.sh` refuses `git` spelled anywhere else in `src/`.
//! - A chain's tree (`jobs/<origin>/tree`). The job table says when a
//!   chain is live ([`Store::chain_status`](crate::store::Store::chain_status)),
//!   but a runner still uses its tree after recording its job: it reads
//!   the worker's outputs, and an engage hands the tree to its harvest
//!   before that harvest has a row. A [`TreeLease`] covers that: a runner
//!   holds one from before its tree exists until it has released it, and
//!   nothing releases a leased tree. A lease is taken only through the
//!   permit of the tree's own clone ([`ClonePermit::lease`], for a
//!   [`ChainRef`] the store read from the chain's origin row, refused under
//!   any other clone's permit), so "no lease", seen under that permit,
//!   stays true until the permit is let go. A repo's Renovate scan tree
//!   (`scan/repo-<id>`) is kept the same way while the scan runs
//!   ([`ClonePermit::lease_scan`]), with the clone let go meanwhile.
//!
//! Permits are taken in a [`Lane`]: one line of work (the daemon's cycle,
//! the sweep, an HTTP request), which exists only as `&mut Lane` inside
//! [`Store::lane`]. A permit mutably borrows its lane, which the compiler
//! turns into the lane's rules:
//!
//! - A lane holds at most one permit at a time. A lane asking for a second
//!   clone -- or for the one it holds, which would wait for itself -- does
//!   not compile. Two futures of one lane (`join!`, `select!`) cannot both
//!   take permits either. The only exception is the sweep's
//!   [`Lane::try_lock_every`], one permit per repo row, each taken only if
//!   free: it never waits while holding one, so no wait can be circular,
//!   and no job waits behind it for longer than its own step.
//! - A permit can never be `'static`, whatever the store's lifetime:
//!   `tokio::spawn` and `spawn_blocking` refuse it, so it stays in the lane
//!   that took it.
//!
//! Both live in this process's memory, shared by every clone of the
//! [`Store`] that holds them. After a restart nothing is held, which is
//! right: the process that held it is gone.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::OwnedMutexGuard;

use crate::store::{ChainRef, CloneLockError, CloneRef, Store};

/// The clone locks and tree leases of one store handle.
#[derive(Clone, Default)]
pub struct Clones(Arc<Registry>);

#[derive(Default)]
struct Registry {
    /// One lock per repo, by id, made on first use and kept: a handful
    /// per repo ever added.
    locks: Mutex<HashMap<i64, Arc<Slot>>>,
    /// Live leases, by what they keep: a chain's tree, or a repo's scan
    /// tree.
    leases: Mutex<HashMap<Leased, usize>>,
    /// When each clone's last full fetch (`git fetch origin`) ended, by
    /// which lane and how, as recorded under its permit
    /// ([`ClonePermit::record_fetch`]).
    fetched: Mutex<HashMap<i64, (Instant, LastFetch)>>,
}

/// A clone's last full fetch ([`ClonePermit::last_fetch_within`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastFetch {
    pub by: FetchedBy,
    pub outcome: FetchOutcome,
}

/// Which lane made a fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchedBy {
    JobLane,
    AdminLane,
}

/// How a fetch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Succeeded,
    Failed,
}

/// What a [`TreeLease`] keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Leased {
    /// Chain `origin_id`'s tree, `jobs/<origin>/tree`.
    Chain(i64),
    /// Repo `repo_id`'s Renovate scan tree, `scan/repo-<id>`.
    Scan(i64),
}

/// A chain's tree was to be leased under the permit of a clone it was not
/// added from. The sweep looks for a chain's leases under its own clone's
/// permit only, so such a lease would not keep the tree.
#[derive(Debug, thiserror::Error)]
#[error(
    "chain {origin_id} belongs to repo {chain_repo}, not to the held clone of repo {held_repo}"
)]
pub struct ForeignChain {
    pub origin_id: i64,
    pub chain_repo: i64,
    pub held_repo: i64,
}

/// A Renovate scan of this repo already has its scan tree.
#[derive(Debug, thiserror::Error)]
#[error("a scan of repo {0} is already running in its scan tree")]
pub struct ScanRunning(pub i64);

/// One repo's lock.
#[derive(Default)]
struct Slot {
    mutex: Arc<tokio::sync::Mutex<()>>,
    /// Lanes waiting for the lock right now.
    waiting: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Nothing a holder does can leave either map inconsistent.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

tokio::task_local! {
    /// Set while a task runs inside a [`Lane`]: a second lane opened there
    /// could wait for the first's permit, which would never be let go.
    static IN_LANE: ();
}

/// One line of work that takes clone permits: the cycle, the sweep, an
/// HTTP request. Made only by [`Store::lane`] and lent only as `&mut`, so
/// a permit it gives ([`Lane::lock_clone`]) borrows it exclusively: the
/// lane can take no other permit until that one is dropped.
pub struct Lane {
    store: Store,
}

/// A lane was opened inside another one of the same task, where it could
/// wait for a permit its own task holds and never gets back.
#[derive(Debug, thiserror::Error)]
#[error("a clone lane was opened inside another lane of the same task")]
pub struct NestedLane;

impl Store {
    /// Run `f` as one [`Lane`]: the only way to take clone permits. The
    /// lane is lent to `f` and dropped when it returns, so no permit taken
    /// in it can be kept, spawned or returned out of it.
    ///
    /// Refused inside another lane of the same task ([`NestedLane`]): the
    /// one rule the borrow cannot carry, since a second lane borrows
    /// nothing of the first. Work that needs a lane takes the caller's
    /// `&mut Lane` instead. A task spawned from inside a lane is a task of
    /// its own and opens its own.
    pub async fn lane<R>(&self, f: impl AsyncFnOnce(&mut Lane) -> R) -> Result<R, NestedLane> {
        if IN_LANE.try_with(|()| ()).is_ok() {
            return Err(NestedLane);
        }
        let mut lane = Lane {
            store: self.clone(),
        };
        Ok(IN_LANE.scope((), f(&mut lane)).await)
    }

    /// A lane for a test fixture that calls runners directly, outside any
    /// [`Store::lane`]: leaked, so its permits are `'static` and the
    /// compile-time bounds above do not hold for them. Only in a
    /// `test-support` build, like `Unreconciled::assume_clean`; the
    /// daemon's build has no lane but [`Store::lane`]'s.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_lane(&self) -> &'static mut Lane {
        Box::leak(Box::new(Lane {
            store: self.clone(),
        }))
    }

    /// How many lanes are waiting for repo `repo_id`'s clone right now,
    /// so a test can tell that one is, instead of guessing from time.
    #[cfg(any(test, feature = "test-support"))]
    pub fn clone_waiters(&self, repo_id: i64) -> usize {
        lock(&self.clones().0.locks)
            .get(&repo_id)
            .map_or(0, |slot| slot.waiting.load(Ordering::SeqCst))
    }
}

/// Counts a lane among a slot's waiters for as long as it waits, also
/// when the wait is dropped.
struct Waiting<'s>(&'s AtomicUsize);

impl<'s> Waiting<'s> {
    fn new(n: &'s AtomicUsize) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Lane {
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Wait for, then take, the right to change repo `repo_id`'s clone, at
    /// the directory its row names. A row flagged deleted still names one:
    /// it stays until its clone is removed.
    pub async fn lock_clone(&mut self, repo_id: i64) -> Result<ClonePermit<'_>, CloneLockError> {
        let clone = self.store.clone_ref(repo_id).await?;
        Ok(acquire(&self.store, clone).await)
    }

    /// Wait for, then take, the right to change `clone`. The clone stays
    /// held until the permit is dropped and every blocking command it ran
    /// ([`ClonePermit::blocking`]) has ended; a lane that locks it again
    /// meanwhile waits for that command like any other.
    pub(crate) async fn lock(&mut self, clone: &CloneRef) -> ClonePermit<'_> {
        acquire(&self.store, clone.clone()).await
    }

    /// Every clone's permit -- every repo row's, deleted or not -- for a
    /// change to a clone that only git can name, if every one is free now;
    /// `None`, holding none, if any is taken. The only way a lane holds
    /// more than one, and it never waits while holding: waiting for one
    /// clone while holding the others would keep a job on any of those
    /// waiting behind an admin duty's fetch of an unrelated repo. The
    /// caller tries again on its next run.
    pub(crate) async fn try_lock_every(&mut self) -> sqlx::Result<Option<EveryClone<'_>>> {
        let mut held = Vec::new();
        for repo_id in self.store.clone_ids().await? {
            match self.store.clone_ref(repo_id).await {
                Ok(clone) => match try_acquire(&self.store, clone) {
                    Some(permit) => held.push(permit),
                    None => return Ok(None),
                },
                // Gone since it was listed: its clone went with it.
                Err(CloneLockError::NoRepo(_)) => {}
                Err(CloneLockError::Db(e)) => return Err(e),
            }
        }
        Ok(Some(EveryClone { held }))
    }
}

/// Wait for `clone`'s lock and make its permit. Private: the lifetime is
/// whatever the caller says, so only [`Lane`]'s methods, which tie it to
/// their `&mut self`, may call it.
///
/// A lock another lane holds is waited for as the job lane's
/// [`JobPhase::WaitingForClone`](crate::types::JobPhase::WaitingForClone),
/// so the status page names the wait instead of showing nothing while an
/// admin duty finishes with the clone. Off the job lane nothing is
/// reported; a free lock is taken before the status page could show it.
async fn acquire<'l>(store: &Store, clone: CloneRef) -> ClonePermit<'l> {
    let (clones, slot) = slot_of(store, &clone);
    let guard = {
        let _waiting = Waiting::new(&slot.waiting);
        let _phase = crate::activity::job_phase(crate::types::JobPhase::WaitingForClone);
        slot.mutex.clone().lock_owned().await
    };
    permit_of(clones, clone, guard)
}

/// `clone`'s permit if its lock is free now, without waiting. Private for
/// the reason [`acquire`] is.
fn try_acquire<'l>(store: &Store, clone: CloneRef) -> Option<ClonePermit<'l>> {
    let (clones, slot) = slot_of(store, &clone);
    let guard = slot.mutex.clone().try_lock_owned().ok()?;
    Some(permit_of(clones, clone, guard))
}

fn slot_of(store: &Store, clone: &CloneRef) -> (Clones, Arc<Slot>) {
    let clones = store.clones().clone();
    let slot = lock(&clones.0.locks)
        .entry(clone.repo_id())
        .or_default()
        .clone();
    (clones, slot)
}

fn permit_of<'l>(clones: Clones, clone: CloneRef, guard: OwnedMutexGuard<()>) -> ClonePermit<'l> {
    ClonePermit {
        hold: CloneHold(Arc::new(Held {
            clone,
            command: Mutex::new(()),
            _guard: guard,
        })),
        clones,
        _lane: PhantomData,
    }
}

impl Clones {
    /// How many leases chain `origin_id`'s tree has.
    pub(crate) fn chain_leases(&self, origin_id: i64) -> usize {
        self.count(Leased::Chain(origin_id))
    }

    /// Whether a running scan holds repo `repo_id`'s scan tree.
    pub(crate) fn scan_leased(&self, repo_id: i64) -> bool {
        self.count(Leased::Scan(repo_id)) > 0
    }

    fn count(&self, what: Leased) -> usize {
        lock(&self.0.leases).get(&what).copied().unwrap_or(0)
    }

    fn take(&self, what: Leased) -> TreeLease {
        *lock(&self.0.leases).entry(what).or_default() += 1;
        TreeLease {
            clones: self.clone(),
            what,
        }
    }
}

/// The right to change one repo's clone, as the lane that took it holds
/// it: proof that no one else is. Made only by [`Lane::lock_clone`].
///
/// Neither `Clone` nor `'static`: it mutably borrows the [`Lane`] it came
/// from, which exists only inside [`Store::lane`], so the lane takes no
/// other permit while it lives, and `tokio::spawn` and `spawn_blocking`
/// refuse it. A git command that changes the clone runs through
/// [`Self::blocking`], which keeps the clone held until the command ends.
#[must_use = "a clone permit holds the clone only while it is kept"]
pub struct ClonePermit<'l> {
    hold: CloneHold,
    /// The store's leases, which [`Self::lease`] adds to.
    clones: Clones,
    _lane: PhantomData<&'l mut Lane>,
}

/// The clone held on a blocking thread, for the git commands a
/// [`ClonePermit`] allows. Handed out only by reference, to the closure of
/// [`ClonePermit::blocking`], so no async code can keep the clone held
/// past its permit and then wait on itself, nor run git in it outside
/// such a closure.
pub struct CloneHold(Arc<Held>);

struct Held {
    clone: CloneRef,
    /// Held by whichever closure of [`ClonePermit::blocking`] runs right
    /// now: commands under one permit run one after another, also when
    /// the future of an earlier one was dropped while it ran on.
    command: Mutex<()>,
    _guard: OwnedMutexGuard<()>,
}

impl ClonePermit<'_> {
    pub fn repo_id(&self) -> i64 {
        self.hold.repo_id()
    }

    /// The clone this permit is for. Git runs in it only through
    /// [`Self::blocking`].
    pub fn clone_dir(&self) -> &Path {
        self.hold.clone_dir()
    }

    /// Which clone this permit is for, to lock it again once let go.
    pub fn clone_ref(&self) -> &CloneRef {
        self.hold.clone_ref()
    }

    /// Run `f` on a blocking thread that holds the clone until `f` returns,
    /// even if this future is dropped first. One `f` of this permit runs at
    /// a time: a second waits for the first, joined or dropped alike.
    pub async fn blocking<R, F>(&self, f: F) -> Result<R, tokio::task::JoinError>
    where
        F: FnOnce(&CloneHold) -> R + Send + 'static,
        R: Send + 'static,
    {
        let hold = CloneHold(Arc::clone(&self.hold.0));
        tokio::task::spawn_blocking(move || {
            let _one = lock(&hold.0.command);
            f(&hold)
        })
        .await
    }

    /// `Ok` when `chain`'s tree is a tree of this permit's clone: the
    /// clone the sweep locks to look at that chain, since a [`ChainRef`]
    /// names the repo the store read from the chain's origin row.
    pub fn holds_chain(&self, chain: &ChainRef) -> Result<(), ForeignChain> {
        if chain.repo_id() == self.repo_id() {
            Ok(())
        } else {
            Err(ForeignChain {
                origin_id: chain.origin_id(),
                chain_repo: chain.repo_id(),
                held_repo: self.repo_id(),
            })
        }
    }

    /// Take a lease on `chain`'s tree, which must be a tree of this
    /// permit's clone ([`Self::holds_chain`]). The only way to a chain's
    /// [`TreeLease`]: the sweep and [`crate::workspace::release_if_idle`]
    /// look for leases under the permit of the chain's own clone, so a
    /// lease taken under no permit, or under another clone's, could appear
    /// right after they looked.
    pub fn lease(&self, chain: &ChainRef) -> Result<TreeLease, ForeignChain> {
        self.holds_chain(chain)?;
        Ok(self.clones.take(Leased::Chain(chain.origin_id())))
    }

    /// Take the lease on this repo's scan tree, for a Renovate scan: one
    /// scan per repo at a time, since the repo has one scan tree. Taken
    /// under the permit, so the sweep, which removes scan leftovers under
    /// the same permit, sees it.
    pub fn lease_scan(&self) -> Result<TreeLease, ScanRunning> {
        let repo_id = self.repo_id();
        if self.clones.scan_leased(repo_id) {
            return Err(ScanRunning(repo_id));
        }
        Ok(self.clones.take(Leased::Scan(repo_id)))
    }

    /// Record that a full fetch of this clone ended just now, by whom and
    /// how. Under the permit, so no other fetch of the clone ran between
    /// it and this.
    pub(crate) fn record_fetch(&self, last: LastFetch) {
        lock(&self.clones.0.fetched).insert(self.repo_id(), (Instant::now(), last));
    }

    /// This clone's last full fetch in this process, if it ended less than
    /// `age` ago. `None` after a restart.
    pub fn last_fetch_within(&self, age: Duration) -> Option<LastFetch> {
        lock(&self.clones.0.fetched)
            .get(&self.repo_id())
            .filter(|(at, _)| at.elapsed() < age)
            .map(|&(_, last)| last)
    }
}

impl CloneHold {
    pub fn repo_id(&self) -> i64 {
        self.0.clone.repo_id()
    }

    /// The clone this hold is for: where its trees go and what is checked
    /// for being there. Git runs in it only through [`Self::git`].
    pub fn clone_dir(&self) -> &Path {
        self.0.clone.dir()
    }

    /// Which clone this hold is for, to lock it again once let go.
    pub fn clone_ref(&self) -> &CloneRef {
        &self.0.clone
    }

    /// `git -C <clone> <args>`, in the held clone.
    ///
    /// With [`Self::git_in`] and [`Self::clone_from`] the only way the
    /// daemon runs a git command that may change a clone: `git` is spelled
    /// in this module alone (`check-git.sh`), and every other command it
    /// runs is an [`Unheld`] one, which changes none.
    pub fn git(&self, args: &[&str], timeout_s: u64) -> (i32, String) {
        run_git(self.clone_dir(), args, timeout_s)
    }

    /// `git -C <tree> <args>`, in a tree of the held clone: a worktree
    /// shares the clone's refs, objects and registry, so a command that
    /// writes there changes the clone.
    ///
    /// Refused (rc 128, nothing run) unless `tree` is a worktree of this
    /// clone ([`Self::owns_tree`]). A chain's tree path does not depend on
    /// its clone, so no type can tie the two; under one repo's permit,
    /// a command in another repo's tree would change a clone someone else
    /// may hold.
    pub fn git_in(&self, tree: &Path, args: &[&str], timeout_s: u64) -> (i32, String) {
        if !self.owns_tree(tree) {
            return (
                128,
                format!(
                    "{} is not a tree of the held clone {}",
                    tree.display(),
                    self.clone_dir().display()
                ),
            );
        }
        run_git(tree, args, timeout_s)
    }

    /// Whether `tree` is a worktree (or the checkout) of this clone.
    pub fn owns_tree(&self, tree: &Path) -> bool {
        clone_of_tree(tree).is_some_and(|clone| self.is_clone(&clone))
    }

    /// Whether `dir`, already resolved by the filesystem, is this clone.
    /// Compared resolved because a repo row's path may run through a
    /// symlink, while git names a common dir by its real path.
    pub fn is_clone(&self, dir: &Path) -> bool {
        std::fs::canonicalize(self.clone_dir()).is_ok_and(|held| held == dir)
    }

    /// `git clone -- <url> <clone>`: make the held clone, with automatic
    /// maintenance off in its config from the start, so a worker's commit
    /// in one of its trees starts no detached housekeeping
    /// (`crate::workspace::fetch` says why).
    pub fn clone_from(&self, url: &str, timeout_s: u64) -> (i32, String) {
        let dir = self.clone_dir().to_string_lossy();
        // `--` before the operands: even a URL that slipped past
        // validation cannot be read as an option here.
        crate::util::run_cmd(
            &[
                "git",
                "clone",
                "--config",
                "maintenance.auto=false",
                "--",
                url,
                &dir,
            ],
            timeout_s,
        )
    }
}

/// The clone `tree` is a worktree (or the checkout) of, resolved by the
/// filesystem: the parent of its git common dir. `None` when `tree` is no
/// git tree.
pub fn clone_of_tree(tree: &Path) -> Option<PathBuf> {
    let (rc, out) = Unheld::CommonDir.run(tree, 30);
    if rc != 0 {
        return None;
    }
    let common = PathBuf::from(out.trim());
    std::fs::canonicalize(common.parent()?).ok()
}

fn run_git(dir: &Path, args: &[&str], timeout_s: u64) -> (i32, String) {
    let dir = dir.to_string_lossy();
    let mut command = Vec::with_capacity(args.len() + 3);
    command.extend(["git", "-C", &dir]);
    command.extend_from_slice(args);
    crate::util::run_cmd(&command, timeout_s)
}

/// A git command that changes no clone -- not its refs, objects, config or
/// worktree registry -- and so runs without a [`ClonePermit`], in a clone
/// or one of its trees. Each is spelled out here, so a command that would
/// change a clone cannot be run this way: it needs a [`CloneHold`].
#[derive(Debug, Clone, Copy)]
pub enum Unheld<'a> {
    /// `log <range> --oneline`.
    Commits { range: &'a str },
    /// `log -1 --format=%s <rev>`: the subject of the commit `rev` names.
    Subject { rev: &'a str },
    /// `symbolic-ref --quiet HEAD`: the ref `HEAD` points at; rc 1 when
    /// it is detached.
    HeadRef,
    /// `diff --quiet --no-ext-diff <from> <to>`: rc 0 when the two
    /// commits' trees are identical, 1 when they differ.
    SameTree { from: &'a str, to: &'a str },
    /// `merge-base --is-ancestor <ancestor> <rev>`: rc 0 when `rev`
    /// descends from `ancestor` (or is it), 1 when not.
    IsAncestor { ancestor: &'a str, rev: &'a str },
    /// `--no-optional-locks status --porcelain`: the tree's uncommitted
    /// changes and untracked files, one per line; empty when it is clean.
    /// Without the flag `status` refreshes and rewrites the tree's index.
    Status,
    /// `push <force> <url> <refspec>`. It changes the remote only: a push
    /// to a URL updates no remote-tracking ref.
    Push {
        force: Force<'a>,
        url: &'a str,
        refspec: &'a str,
    },
    /// `-c core.quotepath=off diff --name-only --no-renames <from> <to> --
    /// <paths>`. `--no-renames`: rename detection is the one thing that
    /// warns on stderr while succeeding, and the command's two streams are
    /// merged. A watched file renamed away still shows, as its old path.
    ChangedPaths {
        from: &'a str,
        to: &'a str,
        paths: &'a [String],
    },
    /// `ls-files --error-unmatch -- <path>`: 0 when tracked, 1 when not.
    IsTracked { path: &'a str },
    /// `rev-parse --path-format=absolute --git-common-dir`: the `.git` of
    /// the clone a tree belongs to.
    CommonDir,
    /// `rev-parse --verify --quiet <rev>^{commit}`: the commit `rev` names.
    Commit { rev: &'a str },
    /// `rev-list -1 --before=<before> <rev>`: the newest commit reachable
    /// from `rev` that is older than `before`.
    LastBefore { rev: &'a str, before: &'a str },
}

/// How an [`Unheld::Push`] may replace the remote branch.
#[derive(Debug, Clone, Copy)]
pub enum Force<'a> {
    /// `--force`.
    Always,
    /// `--force-with-lease=<lease>`, a `<ref>:<sha>` expectation.
    Lease(&'a str),
}

impl Unheld<'_> {
    /// Run in `dir`, a clone or one of its trees, holding nothing.
    pub fn run(&self, dir: &Path, timeout_s: u64) -> (i32, String) {
        let (lease, spec);
        let mut args: Vec<&str> = Vec::new();
        match *self {
            Self::Commits { range } => args.extend(["log", range, "--oneline"]),
            Self::Subject { rev } => args.extend(["log", "-1", "--format=%s", rev]),
            Self::HeadRef => args.extend(["symbolic-ref", "--quiet", "HEAD"]),
            Self::SameTree { from, to } => {
                args.extend(["diff", "--quiet", "--no-ext-diff", from, to]);
            }
            Self::IsAncestor { ancestor, rev } => {
                args.extend(["merge-base", "--is-ancestor", ancestor, rev]);
            }
            Self::Status => args.extend(["--no-optional-locks", "status", "--porcelain"]),
            Self::Push {
                force,
                url,
                refspec,
            } => {
                args.push("push");
                match force {
                    Force::Always => args.push("--force"),
                    Force::Lease(l) => {
                        lease = format!("--force-with-lease={l}");
                        args.push(&lease);
                    }
                }
                args.extend([url, refspec]);
            }
            Self::ChangedPaths { from, to, paths } => {
                args.extend([
                    "-c",
                    "core.quotepath=off",
                    "diff",
                    "--name-only",
                    "--no-renames",
                    from,
                    to,
                    "--",
                ]);
                args.extend(paths.iter().map(String::as_str));
            }
            Self::IsTracked { path } => args.extend(["ls-files", "--error-unmatch", "--", path]),
            Self::CommonDir => {
                args.extend(["rev-parse", "--path-format=absolute", "--git-common-dir"]);
            }
            Self::Commit { rev } => {
                spec = format!("{rev}^{{commit}}");
                args.extend(["rev-parse", "--verify", "--quiet", &spec]);
            }
            Self::LastBefore { rev, before } => {
                spec = format!("--before={before}");
                args.extend(["rev-list", "-1", &spec, rev]);
            }
        }
        run_git(dir, &args, timeout_s)
    }
}

/// Every clone's [`ClonePermit`], from [`Lane::lock_every`].
pub(crate) struct EveryClone<'l> {
    held: Vec<ClonePermit<'l>>,
}

impl EveryClone<'_> {
    /// Run `f` on a blocking thread holding every clone until it returns,
    /// even if this future is dropped first, and after every command
    /// already running under one of the permits has ended.
    pub(crate) async fn blocking<R, F>(&self, f: F) -> Result<R, tokio::task::JoinError>
    where
        F: FnOnce(&[CloneHold]) -> R + Send + 'static,
        R: Send + 'static,
    {
        let holds: Vec<CloneHold> = self
            .held
            .iter()
            .map(|p| CloneHold(Arc::clone(&p.hold.0)))
            .collect();
        tokio::task::spawn_blocking(move || {
            // In id order, like the permits themselves.
            let _all: Vec<_> = holds.iter().map(|h| lock(&h.0.command)).collect();
            f(&holds)
        })
        .await
    }
}

/// A runner's hold on a tree: nothing releases the tree while a lease on
/// it is live. A chain's tree ([`ClonePermit::lease`]) is kept from
/// [`crate::workspace::release_if_idle`] and the sweep; a repo's scan tree
/// ([`ClonePermit::lease_scan`]) from the sweep and from a second scan.
/// Dropped, it ends.
#[must_use = "a tree lease protects the tree only while it is held"]
pub struct TreeLease {
    clones: Clones,
    what: Leased,
}

impl TreeLease {
    /// Whether anyone but this lease holds the tree.
    pub fn shared(&self) -> bool {
        self.clones.count(self.what) > 1
    }
}

impl Drop for TreeLease {
    fn drop(&mut self) {
        let mut leases = lock(&self.clones.0.leases);
        if let Some(n) = leases.get_mut(&self.what) {
            *n -= 1;
            if *n == 0 {
                leases.remove(&self.what);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::activity::{JobStatus, reporting_job_phases};
    use crate::types::JobPhase;

    /// A store over a fresh copy of the schema-only `dev.db` with repo 1.
    async fn store(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("clones-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("hunter.db");
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dev.db"),
            &db,
        )
        .unwrap();
        let pool =
            sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
                .await
                .unwrap();
        sqlx::raw_sql(
            "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
             VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', \
             'main', 1, 1000);",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
        Store::connect(&db).await.unwrap()
    }

    /// While the job lane waits for a clone another lane holds, its status
    /// says so, and only for as long as it waits: a job queued behind an
    /// admin duty's fetch is visible as that, not as nothing.
    #[tokio::test]
    async fn the_job_lane_shows_its_wait_for_a_held_clone() {
        let store = store("wait-shown").await;
        let held = store.test_lane().lock_clone(1).await.unwrap();
        let status = JobStatus::default();
        let waiter = tokio::spawn(reporting_job_phases(status.clone(), {
            let store = store.clone();
            async move {
                drop(store.test_lane().lock_clone(1).await.unwrap());
            }
        }));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while store.clone_waiters(1) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let waiting = status.current().map(|a| a.phase);

        drop(held);
        waiter.await.unwrap();

        assert_eq!(waiting, Some(JobPhase::WaitingForClone));
        assert_eq!(status.current(), None);
    }
}
