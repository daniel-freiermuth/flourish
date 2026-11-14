//! Every process the daemon starts, in a cgroup (v2) of its own, inside
//! the cgroup of the unit of work it was started for.
//!
//! The guarantee: no process the daemon started runs on once the daemon
//! has stopped on a SIGTERM (or SIGHUP). A process and everything it
//! starts stay in its cgroup, however they detach -- `setsid`, a double
//! fork, a daemonizing tool -- so ending the cgroup ends all of them. (A
//! process that writes itself into another cgroup on purpose, or has
//! systemd start it, is the worker sandbox's to stop.)
//!
//! A daemon killed outright (SIGKILL, a crash) ends nothing itself. Under
//! systemd (`hunter/hunter.service`) the unit's cgroup holds every cgroup
//! here, and systemd ends the whole unit when its main process dies. Run
//! by hand, what it left is the operator's to end, and the next daemon on
//! the same work root refuses to start while any of it runs ([`start`]).
//!
//! **Layout**, below the cgroup the daemon runs in (delegated to it:
//! `Delegate=yes`, `systemd-run --user --scope -p Delegate=yes`, or a
//! directory root made and handed to the operator):
//!
//! - `daemon/`: the daemon itself ([`start`] moves it there, so processes
//!   live in leaves only, as cgroup v2 wants once controllers are on);
//! - one cgroup per unit of work ([`Unit`]): `cycle-<n>/` for a job-lane
//!   cycle (one job, with the git it needs before and after), `admin-<duty>-<n>/`
//!   for one admin duty run, `gateway-<n>/` for the auth broker and
//!   gateway, and `other/` for what runs outside them (startup, the UI,
//!   the usage probes);
//! - inside a unit, one cgroup per process started, `<n>-<program>/`: a
//!   worker run, a git command.
//!
//! A process that never claimed a cgroup (a test, a tool) makes its units
//! in `process-<pid>/` below the one it runs in ([`units_root`]), so that
//! no two processes share the names they make and end.
//!
//! **Ending.** A unit ends when its work does ([`Unit::end`], [`in_unit`]),
//! with everything left in it; the daemon's [`Claimed`] ends the rest when
//! the daemon stops. A worker run's cgroup ends when the run does
//! ([`Child::kill_tree`], the drop of a running [`Child`], [`Leash::cut`]).
//! The daemon's own tools (git, `gh`, `glab`) are not ended per command:
//! what one leaves behind runs on until its unit ends. An end is the
//! process-group kill this replaces, made complete: freeze the cgroup,
//! SIGTERM every process in it, thaw (frozen, nothing forks, so each
//! process there gets exactly one SIGTERM, and a process a SIGTERM
//! handler starts for its cleanup gets none), wait until it is empty for
//! up to [`TERM_GRACE`], then `cgroup.kill`, and remove it.
//!
//! **Which unit.** The job lane and each admin duty run their work in
//! [`in_unit`], a task-local scope; [`spawn_blocking`] carries it onto
//! the blocking thread, and a thread of the daemon's own enters one with
//! [`Unit::enter`]. A spawn outside any unit goes to `other/`.
//!
//! **Placement.** The forked child moves itself into its cgroup before it
//! execs the program (`pre_exec`: one open, write and close of a path
//! prepared beforehand), so the program never runs outside it.
//!
//! **ssh.** Every process started finds a wrapper first on its `PATH`,
//! `<workRoot>/bin/ssh`, that runs the real ssh with `-o
//! ControlPath=none` ([`install_ssh_wrapper`]). So none of the daemon's
//! git uses the operator's ssh connection sharing or starts a shared
//! master, which ending its cgroup would cut under every session
//! multiplexed over it, while `core.sshCommand` and an operator's own
//! `GIT_SSH_COMMAND` keep choosing keys and options.
//!
//! No user namespace: children run as the operator, see root's files as
//! root's, and ssh, signing and set-user-ID programs work as without
//! hunter.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::{CStr, CString, OsStr};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

/// How long an end gives the processes it sent SIGTERM to before it
/// kills what is left.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// How often a wait on a cgroup ([`await_event`]) reads its events again.
const EVENT_TICK: Duration = Duration::from_millis(10);

/// How long an end waits for its freeze to take hold. One that does not
/// (a process stuck in the kernel) delays nothing: the SIGTERMs go out
/// anyway, and the deadline's kill still ends everything.
const FREEZE_WAIT: Duration = Duration::from_secs(1);

/// How long a process's cgroup may still read as populated once the
/// process was reaped and left nothing ([`Cgroup`]'s drop). Waited out in
/// full only when something is really left, which the unit then ends.
const EXIT_SETTLE: Duration = Duration::from_millis(500);

/// How long, after `cgroup.kill`, to wait for a cgroup to empty before
/// leaving it in place (a process stuck in the kernel). A cgroup left
/// behind with members keeps the next daemon from starting ([`start`]).
const KILLED_WAIT: Duration = Duration::from_secs(5);

/// Where the cgroup v2 hierarchy is mounted.
const CGROUP_FS: &str = "/sys/fs/cgroup";

/// The leaf [`start`] moves the daemon into.
const DAEMON_LEAF: &str = "daemon";

/// The unit of what runs outside every [`Unit`].
const OTHER_UNIT: &str = "other";

/// What the cgroup a process without a daemon's claim makes its units in
/// is named by, then its pid ([`units_root`]).
const PROCESS_PREFIX: &str = "process-";

/// What the cgroup of every [`Unit`] is named by.
const UNIT_PREFIXES: [&str; 3] = ["cycle-", "admin-", "gateway-"];

/// The file in the work root that names the cgroup the daemon claimed
/// ([`start`]) and the claim, until the claim ends cleanly: one left
/// behind means a daemon on this work root died without its stop.
const CLAIMED_FILE: &str = "cgroup";

/// The extended attribute the claimed cgroup carries the claim in
/// ([`claim_identity`]), until the claim ends cleanly. An attribute, not
/// a file: cgroupfs takes no regular files, but takes `user.` attributes
/// from the owner of a cgroup (Linux 5.7; `cgroup.kill` needs 5.14).
const CLAIM_XATTR: &CStr = c"user.hunter.claim";

/// The directory in the work root that holds the `ssh` wrapper every
/// process started finds first on its `PATH` ([`install_ssh_wrapper`]).
const WRAPPER_DIR: &str = "bin";

/// The wrapper's directory, once [`install_ssh_wrapper`] has written it.
static SSH_WRAPPER: Mutex<Option<PathBuf>> = Mutex::new(None);

/// How long [`start`] asks systemd whether its unit is delegated.
const SYSTEMCTL_TIMEOUT_S: u64 = 5;

/// How to give hunter a cgroup to manage; said wherever it has none.
const HOW_TO_DELEGATE: &str = "hunter runs every process it starts in a cgroup of its \
     own, so that none outlives it, and needs a cgroup v2 subtree it may manage: run it \
     under systemd with Delegate=yes (hunter/hunter.service), or by hand as \
     `systemd-run --user --scope --unit=hunter -p Delegate=yes hunter ...`";

/// The cgroup this process makes its units in, once known.
static ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

/// What names the cgroups made here, in order: units and processes.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The lock on `mutex`. Every value guarded here is written whole in one
/// statement, so a panic while it was held cannot have left it half-set.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The cgroup (v2) this process runs in, as a directory.
pub fn own_cgroup() -> io::Result<PathBuf> {
    let listed = fs::read_to_string("/proc/self/cgroup")?;
    let unified = listed
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| io::Error::other(format!("no cgroup v2 hierarchy. {HOW_TO_DELEGATE}")))?;
    let dir = Path::new(CGROUP_FS).join(unified.trim_start_matches('/'));
    // `cgroup.kill` came with Linux 5.14; `cgroup.freeze` before it.
    if !dir.join("cgroup.kill").is_file() {
        return Err(io::Error::other(format!(
            "{} is not a cgroup v2 directory with cgroup.kill (Linux 5.14 or later). \
             {HOW_TO_DELEGATE}",
            dir.display()
        )));
    }
    Ok(dir)
}

/// Make this process's cgroup the daemon's, for the daemon working in
/// `work_root`: the daemon runs this before anything else and does not
/// start unless it succeeds.
///
/// Refuses a cgroup this process may not write to. Refuses, naming each
/// cgroup and its processes and killing nothing, while a process of an
/// earlier run is left in one of the daemon's cgroups -- in this cgroup,
/// or in the one the daemon last claimed for `work_root`, wherever the
/// new daemon starts (another terminal's scope): a starting daemon
/// assumes none of its predecessor's workers runs (`crate::recovery`).
/// That last one counts only while it still carries that claim: a unit
/// of the same name made since (a reused scope name) is someone else's,
/// another daemon's perhaps, and is left alone. Empty leftovers are
/// removed. Then the daemon moves into its leaf, marks this cgroup with
/// its claim and records both in the work root. Whether systemd
/// delegated it is only checked and warned about ([`delegation`]).
///
/// The [`Claimed`] it returns ends every unit left in the cgroup when it
/// is dropped: the daemon holds it until all of its work is done.
pub fn start(work_root: &Path) -> io::Result<Claimed> {
    let claimed = {
        let mut root = lock(&ROOT);
        let dir = own_cgroup()?;
        let claimed = claim(&dir, work_root)?;
        *root = Some(dir);
        claimed
    };
    if let Some(warning) = delegation(&claimed.cgroup).warning() {
        tracing::warn!("{warning}");
    }
    Ok(claimed)
}

/// The daemon's cgroup, claimed by [`start`]. Dropped, it ends every unit
/// still in it (module docs), together: what the daemon's work left, once
/// it is done. Then, unless a unit could not be ended, it takes the claim
/// off the cgroup and out of the work root: a claim left behind is a
/// daemon that died without its stop.
#[derive(Debug)]
#[must_use = "dropping the claim ends every unit at once"]
pub struct Claimed {
    cgroup: PathBuf,
    /// Where the work root records the claim.
    record: PathBuf,
    /// The claim ([`claim_identity`]).
    identity: String,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        let units: Vec<PathBuf> = subdirs(&self.cgroup)
            .into_iter()
            .filter(|dir| {
                dir.file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|name| name != DAEMON_LEAF && is_ours(name))
            })
            .collect();
        end_cgroups(&units);
        if units.iter().any(|unit| unit.exists()) {
            return;
        }
        if read_claim_mark(&self.cgroup).as_deref() == Some(self.identity.as_str()) {
            remove_claim_mark(&self.cgroup);
        }
        if recorded_claim(&self.record).is_some_and(|(_, identity)| identity == self.identity) {
            let _ = fs::remove_file(&self.record);
        }
    }
}

fn claim(root: &Path, work_root: &Path) -> io::Result<Claimed> {
    use nix::unistd::{AccessFlags, access};
    for target in [root.to_owned(), root.join("cgroup.procs")] {
        access(&target, AccessFlags::W_OK).map_err(|e| {
            io::Error::other(format!(
                "cannot manage the cgroup {}: {} is not writable ({e}). {HOW_TO_DELEGATE}",
                root.display(),
                target.display()
            ))
        })?;
    }
    fs::create_dir_all(work_root)?;
    let record = work_root.join(CLAIMED_FILE);
    let identity = claim_identity(work_root)?;
    // This cgroup, and the one the last daemon on this work root claimed,
    // wherever that is (once, if they are the same), while it still
    // carries that claim: one that does not (a scope name reused since)
    // is someone else's, and left alone.
    let earlier = recorded_claim(&record).and_then(|(cgroup, claim)| {
        read_claim_mark(&cgroup)
            .is_some_and(|mark| mark == claim)
            .then_some(cgroup)
    });
    let roots: BTreeSet<PathBuf> = std::iter::once(root.to_owned())
        .chain(earlier.clone())
        .collect();
    let (busy, empty) = leftovers(&roots);
    if !busy.is_empty() {
        return Err(io::Error::other(format!(
            "processes of an earlier run still run in the daemon's cgroups. It does not end \
             what it did not start: end them (e.g. `echo 1 > <cgroup>/cgroup.kill`, or stop \
             the unit they run in) and start again. {}",
            busy.join("; ")
        )));
    }
    // Only now that nothing was found running: a refusal touches nothing.
    for dir in empty {
        remove_cgroup(&dir)?;
    }
    // The dead daemon's claim is done with (this cgroup's is marked anew
    // below).
    if let Some(earlier) = &earlier {
        remove_claim_mark(earlier);
    }
    let leaf = root.join(DAEMON_LEAF);
    // Made here, or left by an earlier run; a failure to make it shows in
    // the move.
    let _ = fs::create_dir(&leaf);
    fs::write(leaf.join("cgroup.procs"), "0").map_err(|e| {
        io::Error::other(format!(
            "cannot move the daemon into {}: {e}. {HOW_TO_DELEGATE}",
            leaf.display()
        ))
    })?;
    write_claim_mark(root, &identity).map_err(|e| {
        io::Error::other(format!(
            "cannot mark the cgroup {} with the daemon's claim: {e}. {HOW_TO_DELEGATE}",
            root.display()
        ))
    })?;
    let staged = work_root.join(format!("{CLAIMED_FILE}.new"));
    fs::write(&staged, format!("{}\n{identity}\n", root.display()))?;
    fs::rename(&staged, &record)?;
    Ok(Claimed {
        cgroup: root.to_owned(),
        record,
        identity,
    })
}

/// The claim of this process on a cgroup for `work_root`: its pid, its
/// start time (field 22 of `/proc/self/stat`, which a reused pid does not
/// share) and the canonical work root.
fn claim_identity(work_root: &Path) -> io::Result<String> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    let started = stat
        .rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .ok_or_else(|| io::Error::other("no start time in /proc/self/stat"))?;
    Ok(format!(
        "{} {started} {}",
        std::process::id(),
        fs::canonicalize(work_root)?.display()
    ))
}

/// The cgroup and the claim the work root's record at `record` names.
/// Not checked for being empty: an empty path carries no mark, and no
/// mark is empty, so such a record matches no cgroup's claim.
fn recorded_claim(record: &Path) -> Option<(PathBuf, String)> {
    let recorded = fs::read_to_string(record).ok()?;
    let (cgroup, identity) = recorded.split_once('\n')?;
    Some((
        PathBuf::from(cgroup),
        identity.trim_end_matches('\n').to_owned(),
    ))
}

/// The claim the cgroup `dir` carries ([`CLAIM_XATTR`]), if any.
fn read_claim_mark(dir: &Path) -> Option<String> {
    let path = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut value = vec![0_u8; 4096];
    // SAFETY: both names are NUL-terminated strings that outlive the
    // call, and the buffer is as long as the size passed with it.
    #[allow(unsafe_code)]
    let len = unsafe {
        nix::libc::getxattr(
            path.as_ptr(),
            CLAIM_XATTR.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    value.truncate(usize::try_from(len).ok()?);
    String::from_utf8(value).ok()
}

/// Mark the cgroup `dir` with the claim `identity` ([`CLAIM_XATTR`]).
fn write_claim_mark(dir: &Path, identity: &str) -> io::Result<()> {
    let path = CString::new(dir.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: both names are NUL-terminated strings that outlive the
    // call, and the value is as long as the size passed with it.
    #[allow(unsafe_code)]
    let rc = unsafe {
        nix::libc::setxattr(
            path.as_ptr(),
            CLAIM_XATTR.as_ptr(),
            identity.as_ptr().cast(),
            identity.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Take the claim off the cgroup `dir` ([`CLAIM_XATTR`]).
fn remove_claim_mark(dir: &Path) {
    if let Ok(path) = CString::new(dir.as_os_str().as_bytes()) {
        // SAFETY: both names are NUL-terminated strings that outlive the
        // call.
        #[allow(unsafe_code)]
        unsafe {
            nix::libc::removexattr(path.as_ptr(), CLAIM_XATTR.as_ptr());
        }
    }
}

/// The daemon's cgroups directly below each of `roots`: those a process
/// other than this one is in, each named with its processes, and the
/// empty ones but the daemon's leaf, which are leftovers to remove.
fn leftovers(roots: &BTreeSet<PathBuf>) -> (Vec<String>, Vec<PathBuf>) {
    let me = std::process::id();
    let (mut busy, mut empty) = (Vec::new(), Vec::new());
    for dir in roots.iter().flat_map(|root| subdirs(root)) {
        let Some(name) = dir.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !is_ours(name) {
            continue;
        }
        let others: Vec<u32> = members(&dir).into_iter().filter(|&p| p != me).collect();
        if !others.is_empty() {
            busy.push(format!("{}: {}", dir.display(), describe(&others)));
        } else if name != DAEMON_LEAF {
            empty.push(dir);
        }
    }
    (busy, empty)
}

/// Whether a cgroup named `name`, below the daemon's, is one it makes.
fn is_ours(name: &str) -> bool {
    name == DAEMON_LEAF || name == OTHER_UNIT || UNIT_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Whether systemd delegated a cgroup ([`delegation`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delegated {
    /// Its systemd unit has `Delegate=yes`.
    Yes,
    /// Its systemd unit, named, does not delegate it.
    No(String),
    /// It is no systemd unit's cgroup (a directory root made and handed
    /// over), so systemd cannot say.
    NotAUnit,
    /// It is named like a unit's, but no systemd manager answers for it.
    Unknown,
}

impl Delegated {
    /// What to warn about, when systemd has not promised to leave the
    /// subtree to the daemon. Never a reason to refuse: a directory root
    /// made and handed over is delegated too, without a unit.
    pub fn warning(&self) -> Option<String> {
        match self {
            Self::Yes => None,
            Self::No(unit) => Some(format!(
                "systemd unit {unit} does not delegate its cgroup (Delegate=yes): systemd may \
                 move or remove the cgroups hunter makes in it. {HOW_TO_DELEGATE}"
            )),
            Self::NotAUnit => Some(format!(
                "the daemon's cgroup is no systemd unit's: hunter cannot tell whether it was \
                 delegated. {HOW_TO_DELEGATE}"
            )),
            Self::Unknown => Some(format!(
                "no systemd unit answers for the daemon's cgroup (or systemctl is \
                 unavailable): hunter cannot tell whether it was delegated. {HOW_TO_DELEGATE}"
            )),
        }
    }
}

/// Ask systemd whether the unit `cgroup` belongs to delegates it: the
/// user manager, then the system's, each `systemctl show -p Delegate -p
/// ControlGroup <unit>` (the unit named by the cgroup's last component,
/// `.service` or `.scope`). Only a manager that answers, and names this
/// very cgroup as the unit's, counts.
pub fn delegation(cgroup: &Path) -> Delegated {
    let Some(unit) = cgroup.file_name().and_then(OsStr::to_str).filter(|u| {
        Path::new(u)
            .extension()
            .is_some_and(|e| e == "service" || e == "scope")
    }) else {
        return Delegated::NotAUnit;
    };
    let path = cgroup
        .strip_prefix(CGROUP_FS)
        .map_or_default(|p| Path::new("/").join(p));
    ["--user", "--system"]
        .into_iter()
        .find_map(|manager| {
            let (rc, said) = crate::util::run_cmd(
                &[
                    "systemctl",
                    manager,
                    "show",
                    "-p",
                    "Delegate",
                    "-p",
                    "ControlGroup",
                    unit,
                ],
                SYSTEMCTL_TIMEOUT_S,
            );
            let ours = rc == 0
                && said
                    .lines()
                    .any(|l| l.strip_prefix("ControlGroup=").map(Path::new) == Some(&path));
            ours.then(|| {
                if said.lines().any(|l| l == "Delegate=yes") {
                    Delegated::Yes
                } else {
                    Delegated::No(unit.to_owned())
                }
            })
        })
        .unwrap_or(Delegated::Unknown)
}

/// The cgroup units are made in: the one [`start`] claimed, or else (no
/// daemon: a test, a tool) `process-<pid>` below the one this process
/// runs in, made now and kept for the life of the process.
///
/// Every name below the root is then this process's alone, and never
/// made twice (numbers count on): a cgroup it ended and removed is never
/// a new one of another process by the same name. Sharing a parent,
/// processes did trip over each other -- one's late end or removal of a
/// cgroup it had already ended hit another's new one of the same name,
/// which then failed its next spawn (seen as parallel tests failing).
pub fn units_root() -> io::Result<PathBuf> {
    let mut root = lock(&ROOT);
    if let Some(dir) = &*root {
        return Ok(dir.clone());
    }
    let dir = own_cgroup()?.join(format!("{PROCESS_PREFIX}{}", std::process::id()));
    // Made now, or left by an earlier process of the same pid; a failure
    // shows, explained, where the first cgroup is made in it.
    let _ = fs::create_dir(&dir);
    *root = Some(dir.clone());
    Ok(dir)
}

/// A new, empty cgroup below `parent` named `name(n)`, `n` counting on
/// (so this process never makes a name twice) and skipping names taken:
/// an earlier run's leftover, or one made by hand.
fn make_cgroup(parent: &Path, name: impl Fn(u64) -> String) -> io::Result<PathBuf> {
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = parent.join(name(n));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "cannot make a cgroup in {}: {e}. {HOW_TO_DELEGATE}",
                        parent.display()
                    ),
                ));
            }
        }
    }
}

/// `pids` as an operator reads them: each with its command line.
fn describe(pids: &[u32]) -> String {
    pids.iter()
        .map(|pid| {
            let cmdline = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let cmdline: String = String::from_utf8_lossy(&cmdline)
                .replace('\0', " ")
                .trim()
                .chars()
                .take(120)
                .collect();
            format!("pid {pid} ({cmdline})")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every process in the cgroup `dir` and its descendants (a process may
/// make cgroups below its own).
fn members(dir: &Path) -> Vec<u32> {
    let mut pids: Vec<u32> = fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect();
    for sub in subdirs(dir) {
        pids.extend(members(&sub));
    }
    pids
}

/// The cgroups directly below `dir`.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir).map_or_else(
        |_| Vec::new(),
        |entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        },
    )
}

/// Remove the empty cgroup `dir`, the cgroups below it first.
fn remove_cgroup(dir: &Path) -> io::Result<()> {
    for sub in subdirs(dir) {
        remove_cgroup(&sub)?;
    }
    fs::remove_dir(dir)
}

/// The value of `key` in the cgroup's `cgroup.events` is 1. A cgroup that
/// is gone reads as neither populated nor frozen.
fn event(dir: &Path, key: &str) -> bool {
    fs::read_to_string(dir.join("cgroup.events")).is_ok_and(|events| {
        events
            .lines()
            .any(|line| line.strip_prefix(key).is_some_and(|v| v.trim() == "1"))
    })
}

/// Whether any process is in the cgroup `dir` or below it.
fn populated(dir: &Path) -> bool {
    event(dir, "populated")
}

/// SIGKILL every process in the cgroup `dir` and below it, at once.
fn kill_cgroup(dir: &Path) {
    let _ = fs::write(dir.join("cgroup.kill"), "1");
}

/// Wait until the cgroup `dir`'s `key` event is `on`, at most `within`;
/// whether it is.
fn await_event(dir: &Path, key: &str, on: bool, within: Duration) -> bool {
    let until = Instant::now() + within;
    loop {
        if event(dir, key) == on {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(EVENT_TICK);
    }
}

/// SIGTERM, once, every process in the cgroup `dir`: frozen meanwhile, so
/// none can fork, and a process forked during the pass is in the list.
///
/// One window stays, as it did for the process-group kill this replaces:
/// fork(3) blocks every signal around its clone(2), and a process frozen
/// inside that section gets its SIGTERM only after the clone, when it
/// unblocks. The child it made then is in no list and has no SIGTERM
/// pending (a child inherits none); only the deadline's `cgroup.kill`
/// ends it.
fn terminate(dir: &Path) {
    if !populated(dir) {
        return;
    }
    let frozen = fs::write(dir.join("cgroup.freeze"), "1").is_ok();
    if frozen {
        await_event(dir, "frozen", true, FREEZE_WAIT);
    }
    for pid in members(dir) {
        kill_process(pid, Signal::SIGTERM);
    }
    if frozen {
        let _ = fs::write(dir.join("cgroup.freeze"), "0");
    }
}

/// End the cgroups `dirs` together (module docs): one SIGTERM to each
/// process, up to [`TERM_GRACE`] for all of them to empty, then SIGKILL
/// to what is left; then remove them.
fn end_cgroups(dirs: &[PathBuf]) {
    for dir in dirs {
        terminate(dir);
    }
    let deadline = Instant::now() + TERM_GRACE;
    for dir in dirs {
        let left = deadline.saturating_duration_since(Instant::now());
        // A zombie is no member: nothing needs reaping first.
        if !await_event(dir, "populated", false, left) {
            kill_cgroup(dir);
            await_event(dir, "populated", false, KILLED_WAIT);
        }
    }
    for dir in dirs {
        if let Err(e) = remove_cgroup(dir) {
            tracing::warn!("cgroup {} left in place: {e}", dir.display());
        }
    }
}

/// A unit of work: one cgroup, below the daemon's, that holds every
/// process started for the work (module docs). Ended by [`Self::end`];
/// once the last handle is dropped without that, whatever is left in it
/// is killed outright.
#[derive(Clone)]
pub struct Unit(Arc<UnitDir>);

struct UnitDir(PathBuf);

tokio::task_local! {
    static UNIT: Unit;
}

thread_local! {
    static ENTERED: RefCell<Option<Unit>> = const { RefCell::new(None) };
}

impl Unit {
    /// A new unit, its cgroup named `<label>-<n>` (`label` one of
    /// [`UNIT_PREFIXES`] or starting with it).
    pub fn new(label: &str) -> io::Result<Self> {
        Ok(Self(Arc::new(UnitDir(make_cgroup(&units_root()?, |n| {
            format!("{label}-{n}")
        })?))))
    }

    /// The name of the unit's cgroup.
    pub fn name(&self) -> String {
        self.0
            .0
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned())
    }

    /// Start every process this thread starts in this unit, until the
    /// guard drops.
    #[must_use = "the thread leaves the unit when the guard drops"]
    pub fn enter(&self) -> Entered {
        Entered(ENTERED.with(|e| e.borrow_mut().replace(self.clone())))
    }

    /// End everything in the unit and remove its cgroup. Blocking: up to
    /// [`TERM_GRACE`] and a little more.
    pub fn end(&self) {
        end_cgroups(std::slice::from_ref(&self.0.0));
    }
}

impl Drop for UnitDir {
    fn drop(&mut self) {
        kill_cgroup(&self.0);
        await_event(&self.0, "populated", false, KILLED_WAIT);
        let _ = remove_cgroup(&self.0);
    }
}

/// A thread's stay in a [`Unit`] ([`Unit::enter`]); the drop restores the
/// unit it was in before.
pub struct Entered(Option<Unit>);

impl Drop for Entered {
    fn drop(&mut self) {
        let before = self.0.take();
        ENTERED.with(|e| *e.borrow_mut() = before);
    }
}

/// The unit a process started here goes into: the thread's, else the
/// task's.
fn current() -> Option<Unit> {
    ENTERED
        .with(|e| e.borrow().clone())
        .or_else(|| UNIT.try_with(Unit::clone).ok())
}

/// The name of the unit a process started here goes into, if any: for
/// the log, which ties a job to its cycle's cgroup.
pub fn current_unit() -> Option<String> {
    current().map(|unit| unit.name())
}

/// Run `work` in a new unit named after `label` (a task-local scope),
/// then end the unit, with whatever its processes left. The unit is made
/// now, not when the future is first polled; one that cannot be made is
/// logged, and `work` runs outside any.
pub fn in_unit<F: Future>(label: &str, work: F) -> impl Future<Output = F::Output> + use<F> {
    let unit = Unit::new(label)
        .inspect_err(|e| tracing::warn!("no cgroup for {label}: {e}"))
        .ok();
    async move {
        let Some(unit) = unit else {
            return work.await;
        };
        let out = UNIT.scope(unit.clone(), work).await;
        #[allow(clippy::disallowed_methods, reason = "ends the unit itself")]
        let _ = tokio::task::spawn_blocking(move || unit.end()).await;
        out
    }
}

/// [`tokio::task::spawn_blocking`], with `f` in the caller's unit: the
/// only way the daemon runs blocking work, so that what it starts there
/// goes where the work belongs (`clippy.toml` refuses the plain one).
pub fn spawn_blocking<F, R>(f: F) -> tokio::task::JoinHandle<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let unit = current();
    #[allow(clippy::disallowed_methods, reason = "the one wrapper")]
    tokio::task::spawn_blocking(move || {
        let _in = unit.as_ref().map(Unit::enter);
        f()
    })
}

/// Write `<work_root>/bin/ssh`, which every process started from now on
/// finds first on its `PATH`: `exec <ssh> -o ControlPath=none "$@"`,
/// `<ssh>` the first `ssh` on the daemon's `PATH` but this one (module
/// docs). An `-o` on ssh's command line outranks every ssh config, so no
/// git of the daemon's or its workers' shares an ssh connection, whatever
/// else picks its ssh options: `core.sshCommand`, or the operator's own
/// `GIT_SSH_COMMAND`, passed on untouched, keep their keys and options. An
/// ssh named by an absolute path there bypasses it. With no ssh on the
/// `PATH` there is nothing to wrap: logged, and nothing changes.
pub fn install_ssh_wrapper(work_root: &Path) {
    let dir = work_root.join(WRAPPER_DIR);
    match write_ssh_wrapper(&dir) {
        Ok(Some(real)) => {
            tracing::info!(
                "processes started find {}/ssh, {} without connection sharing",
                dir.display(),
                real.display()
            );
            *lock(&SSH_WRAPPER) = Some(dir);
        }
        Ok(None) => tracing::warn!("no ssh on PATH: no ssh wrapper written"),
        Err(e) => tracing::warn!("cannot write the ssh wrapper in {}: {e}", dir.display()),
    }
}

/// [`install_ssh_wrapper`]'s work: the ssh it wraps, if there is one.
fn write_ssh_wrapper(dir: &Path) -> io::Result<Option<PathBuf>> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(dir)?;
    let own = dir.canonicalize()?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let Some(real) = std::env::split_paths(&path)
        .filter(|entry| entry.canonicalize().ok() != Some(own.clone()))
        .map(|entry| entry.join("ssh"))
        .find(|ssh| nix::unistd::access(ssh, nix::unistd::AccessFlags::X_OK).is_ok())
    else {
        return Ok(None);
    };
    let quoted = real.to_string_lossy().replace('\'', r"'\''");
    let staged = dir.join("ssh.new");
    fs::write(
        &staged,
        format!("#!/bin/sh\nexec '{quoted}' -o ControlPath=none \"$@\"\n"),
    )?;
    fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    fs::rename(&staged, dir.join("ssh"))?;
    Ok(Some(real))
}

/// A command to start: configured as a [`std::process::Command`] is --
/// arguments, environment, working directory, stdio -- and started in a
/// cgroup of its own by [`Self::spawn`]. Not a `Deref` to one, whose
/// `spawn` would start it outside.
pub struct Command {
    cmd: std::process::Command,
    /// What its cgroup is named by: the program's file name.
    label: String,
    /// Whether the environment was cleared ([`Self::env_clear`]): its
    /// `PATH` is then only the one it was given.
    cleared: bool,
    /// Whether the program gets SIGTERM when its spawner ends
    /// ([`Self::die_with_spawner`]).
    dies_with_spawner: bool,
}

/// `program`, to be started in a cgroup of its own, in the current unit,
/// and a process group of its own (a Ctrl-C to the daemon's terminal is
/// the daemon's, which lets its children finish).
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let label: String = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(32)
        .collect();
    let mut cmd = std::process::Command::new(program);
    cmd.process_group(0);
    Command {
        cmd,
        label,
        cleared: false,
        dies_with_spawner: false,
    }
}

impl Command {
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.arg(arg);
        self
    }

    pub fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        self.cmd.args(args);
        self
    }

    pub fn env(&mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> &mut Self {
        self.cmd.env(key, value);
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.cmd.env_clear();
        self.cleared = true;
        self
    }

    /// Have the kernel send the program SIGTERM when the thread that
    /// spawns it ends (`PR_SET_PDEATHSIG`): damage limitation for a
    /// daemon killed outright (SIGKILL, a crash, the OOM killer), which
    /// ends nothing itself. Under systemd the unit's kill ends everything
    /// anyway; run by hand, this at least stops the programs that would
    /// otherwise go on spending or holding ports. No guarantee: only the
    /// program gets it, not what it started (a fork clears it), which
    /// stays in its cgroup for the operator to end.
    ///
    /// The spawning *thread*, not the process: the signal comes when that
    /// thread exits, so spawn only from a thread that lives as long as the
    /// program, waiting for it; one that returns first SIGTERMs the
    /// program there and then. A daemon already gone when the program
    /// starts (between the fork and the `prctl`) fails the spawn.
    pub fn die_with_spawner(&mut self) -> &mut Self {
        self.dies_with_spawner = true;
        self
    }

    pub fn current_dir(&mut self, dir: impl AsRef<Path>) -> &mut Self {
        self.cmd.current_dir(dir);
        self
    }

    pub fn stdin(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.cmd.stdin(cfg);
        self
    }

    pub fn stdout(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.cmd.stdout(cfg);
        self
    }

    pub fn stderr(&mut self, cfg: impl Into<Stdio>) -> &mut Self {
        self.cmd.stderr(cfg);
        self
    }

    /// Start the command in a new cgroup in the current unit.
    pub fn spawn(self) -> io::Result<Child> {
        self.start(None)
    }

    /// [`Self::spawn`], with the child tied to `leash`: [`Leash::cut`]
    /// kills it.
    pub fn spawn_leashed(self, leash: &Leash) -> io::Result<Child> {
        self.start(Some(leash))
    }

    fn start(mut self, leash: Option<&Leash>) -> io::Result<Child> {
        // The ssh wrapper first on the `PATH` the program gets: the one
        // the caller gave it, else (unless it cleared the environment)
        // the daemon's own. Set here, after the caller's environment.
        if let Some(wrapper) = lock(&SSH_WRAPPER).clone() {
            let given = self
                .cmd
                .get_envs()
                .find(|(key, _)| *key == "PATH")
                .map(|(_, value)| value.map(OsStr::to_owned));
            let path = match given {
                Some(path) => path,
                None if self.cleared => None,
                None => std::env::var_os("PATH"),
            };
            let entries = std::iter::once(wrapper).chain(
                path.iter()
                    .flat_map(|path| std::env::split_paths(path).collect::<Vec<_>>()),
            );
            if let Ok(joined) = std::env::join_paths(entries) {
                self.cmd.env("PATH", joined);
            }
        }
        let unit = if let Some(unit) = current() {
            unit.0.0.clone()
        } else {
            let other = units_root()?.join(OTHER_UNIT);
            // Made by the first spawn outside a unit, or an earlier run.
            let _ = fs::create_dir(&other);
            other
        };
        let cgroup = Cgroup {
            // The number first: a program's name may not start a cgroup's.
            dir: make_cgroup(&unit, |n| format!("{n}-{}", self.label))?,
        };
        let procs = CString::new(cgroup.dir.join("cgroup.procs").as_os_str().as_bytes())
            .map_err(io::Error::other)?;
        let spawner = self.dies_with_spawner.then(nix::unistd::getpid);
        // SAFETY: `enter` and `die_with` run between fork and exec, where
        // only async-signal-safe calls are allowed: `enter` makes three,
        // open(2), write(2) and close(2), on a path made before the fork,
        // `die_with` two, prctl(2) and getppid(2), and neither allocates.
        #[allow(unsafe_code)]
        unsafe {
            self.cmd.pre_exec(move || {
                enter(&procs)?;
                spawner.map_or(Ok(()), die_with)
            });
        }
        // On an error `cgroup` is dropped, and removed, unused.
        let inner = self.cmd.spawn()?;
        let mut child = Child {
            inner,
            cgroup: Some(cgroup),
            leash: None,
        };
        if let Some(leash) = leash {
            leash.attach(&mut child);
        }
        Ok(child)
    }
}

/// Move the calling process into the cgroup whose `cgroup.procs` is
/// `procs` ("0" is the writer itself). Async-signal-safe: open(2),
/// write(2) and close(2) on a path made before the fork; nix passes a
/// `CStr` through as it is, and an `Errno` converts by its raw code. The
/// descriptor is closed before the exec, so it needs no `O_CLOEXEC`.
fn enter(procs: &CStr) -> io::Result<()> {
    let fd = nix::fcntl::open(
        procs,
        nix::fcntl::OFlag::O_WRONLY,
        nix::sys::stat::Mode::empty(),
    )?;
    nix::unistd::write(&fd, b"0")?;
    Ok(())
}

/// SIGTERM the calling process when the thread that forked it ends
/// ([`Command::die_with_spawner`]); an error if the process `spawner`
/// is no longer its parent, having died before the `prctl` took hold.
/// Async-signal-safe: prctl(2) and getppid(2) only.
fn die_with(spawner: nix::unistd::Pid) -> io::Result<()> {
    nix::sys::prctl::set_pdeathsig(Signal::SIGTERM)?;
    if nix::unistd::getppid() != spawner {
        return Err(io::Error::from(nix::errno::Errno::ESRCH));
    }
    Ok(())
}

/// A process's cgroup, removed when dropped if it is empty by then; one
/// with something left in it is its unit's to end.
struct Cgroup {
    dir: PathBuf,
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        // A process just reaped may still count as a member for a moment:
        // the kernel takes an exited task out of its cgroup only once it
        // has switched out for the last time.
        if await_event(&self.dir, "populated", false, EXIT_SETTLE) {
            let _ = remove_cgroup(&self.dir);
        }
    }
}

/// A process started by [`Command::spawn`], in its cgroup.
///
/// Its status ([`Self::try_wait`], [`Self::wait`]) is the program's own,
/// the moment it ends; what it started may still run, in its cgroup,
/// until [`Self::kill_tree`] or the end of its unit. Dropped while the
/// program runs, everything in its cgroup is killed (SIGKILL): whatever
/// returns early or unwinds past a child cannot leave it running.
pub struct Child {
    inner: std::process::Child,
    /// Until [`Self::kill_tree`] or the drop.
    cgroup: Option<Cgroup>,
    leash: Option<Leash>,
}

impl Child {
    /// The program's pid, for the log.
    pub fn id(&self) -> u32 {
        self.inner.id()
    }

    pub const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin.take()
    }

    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout.take()
    }

    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr.take()
    }

    /// The program's status, if it has ended.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner.try_wait()
    }

    /// Wait for the program to end; its status. What it left keeps running
    /// until [`Self::kill_tree`] or the end of its unit.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait()
    }

    /// End the program and everything in its cgroup, then reap it and
    /// remove the cgroup (module docs: one SIGTERM each, up to
    /// [`TERM_GRACE`], then SIGKILL). On a program that has ended, this is
    /// how what it left gets its SIGTERM and grace; on an empty cgroup it
    /// only reaps.
    ///
    /// The wait is for the whole cgroup, not the program: a descendant
    /// still alive holds the inherited stdout and stderr, so a caller
    /// reading them to EOF would block past the deadline this enforces.
    pub fn kill_tree(&mut self) {
        if let Some(cgroup) = &self.cgroup {
            end_cgroups(std::slice::from_ref(&cgroup.dir));
        }
        let _ = self.inner.wait();
        self.release();
    }

    /// Let go of the cgroup, and the leash's hold on it first: a cut never
    /// writes to a cgroup that is gone, or to a new one of the same name.
    fn release(&mut self) {
        if let Some(leash) = &self.leash {
            lock(&leash.0).cgroup = None;
        }
        self.cgroup = None;
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        if let Some(cgroup) = &self.cgroup
            && matches!(self.inner.try_wait(), Ok(None))
        {
            kill_cgroup(&cgroup.dir);
            let _ = self.inner.wait();
            await_event(&cgroup.dir, "populated", false, KILLED_WAIT);
        }
        self.release();
    }
}

/// Ties a [`Child`] to an owner that may stop waiting for it.
///
/// A worker runs on a blocking thread, which the future waiting for it
/// does not stop when it is dropped. Cutting the leash from that future's
/// drop ([`Self::cut_on_drop`]) kills the worker there and then, so no
/// worker runs on with no one left to record it.
#[derive(Clone, Default)]
pub struct Leash(Arc<Mutex<Tether>>);

#[derive(Default)]
struct Tether {
    cut: bool,
    /// The leashed child's cgroup, until the child lets go of it.
    cgroup: Option<PathBuf>,
}

impl Leash {
    /// Tie a just-spawned `child` to this leash; killed at once if the
    /// leash is already cut.
    fn attach(&self, child: &mut Child) {
        let mut tether = lock(&self.0);
        if let Some(cgroup) = &child.cgroup {
            if tether.cut {
                kill_cgroup(&cgroup.dir);
            } else {
                tether.cgroup = Some(cgroup.dir.clone());
            }
        }
        child.leash = Some(self.clone());
    }

    /// SIGKILL everything in the leashed child's cgroup, and any child
    /// leashed from now on; whether there was anything to kill. The
    /// child's own thread still reaps it.
    pub fn cut(&self) -> bool {
        let mut tether = lock(&self.0);
        tether.cut = true;
        tether
            .cgroup
            .as_deref()
            .filter(|dir| populated(dir))
            .inspect(|dir| kill_cgroup(dir))
            .is_some()
    }

    /// A guard that cuts this leash when dropped.
    #[must_use = "the leash is cut when the guard is dropped"]
    pub fn cut_on_drop(&self) -> CutOnDrop {
        CutOnDrop(self.clone())
    }
}

/// Cuts its [`Leash`] when dropped ([`Leash::cut_on_drop`]).
pub struct CutOnDrop(Leash);

impl Drop for CutOnDrop {
    fn drop(&mut self) {
        if self.0.cut() {
            tracing::warn!("killed a worker no one was waiting for any more");
        }
    }
}

/// Send `sig` (`None`: the null signal) to the one process `pid`;
/// whether it was sent. Never to a process [`target`] refuses.
fn kill_process<T: Into<Option<Signal>>>(pid: u32, sig: T) -> bool {
    target(pid).is_some_and(|pid| signal::kill(pid, sig).is_ok())
}

/// `id` as a process to signal, refusing 0 (to `kill(2)` the caller's
/// own process group), 1 (init) and an id beyond `pid_t` (negative as
/// one: a process group, and -1 every process the daemon's user may
/// signal). No process in a child's cgroup has such an id, so one comes
/// from a bug, which then signals nothing rather than the operator's
/// whole session.
fn target(id: u32) -> Option<Pid> {
    match i32::try_from(id) {
        Ok(id) if id > 1 => Some(Pid::from_raw(id)),
        _ => {
            tracing::debug!("refused to signal process {id}");
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// No signal goes to process 0 (the daemon's own group), 1 (init), or
    /// an id that is negative as a `pid_t` (a group; -1: every process of
    /// the operator's), whatever asks. Probed with the null signal, which
    /// delivers nothing: a broken guard reads as sent.
    #[test]
    fn no_signal_goes_to_the_daemons_group_or_every_process() {
        let beyond_pid_t = u32::try_from(i32::MAX).unwrap() + 2;
        for id in [0, 1, beyond_pid_t, u32::MAX] {
            assert!(!kill_process(id, None), "a signal to process {id}");
        }
        let me = std::process::id();
        assert!(kill_process(me, None), "process {me} is this test");
    }

    /// The guard itself, at its edges: a signal to init would fail with
    /// EPERM anyway, so only `target` shows that 1 is refused.
    #[test]
    fn the_signal_target_starts_at_pid_2() {
        assert_eq!(target(0), None);
        assert_eq!(target(1), None);
        assert_eq!(target(2), Some(Pid::from_raw(2)));
        assert_eq!(target(u32::try_from(i32::MAX).unwrap() + 1), None);
    }

    /// The daemon's cgroups are told from anything else below its own by
    /// name.
    #[test]
    fn the_daemons_cgroups_are_known_by_name() {
        for ours in ["daemon", "other", "cycle-3", "admin-prsync-12", "gateway-0"] {
            assert!(is_ours(ours), "{ours}");
        }
        for theirs in ["test-x-1", "foreign", "cycles", "init.scope"] {
            assert!(!is_ours(theirs), "{theirs}");
        }
    }
}
