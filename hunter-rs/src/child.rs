//! Every process the daemon starts, in one of two kinds.
//!
//! - **Contained** ([`command`]): what runs the code of the repositories
//!   the daemon works on, or code that must not outlive it -- the omp
//!   workers and everything they start, Renovate, the auth broker and
//!   gateway. None of it outlives the daemon, however the daemon dies.
//! - **The daemon's own tools** ([`tool`]): its git, `gh` and `glab`
//!   calls (`crate::util::run_cmd`), run as plain children in a process
//!   group of their own, as before containment existed. They run as the
//!   operator (bwrap's user namespace would show root's files as owned by
//!   nobody, and ssh refuses a config it cannot trust), each bounded by
//!   its caller's timeout, and a graceful stop lets the one in flight
//!   finish: its caller waits for it.
//!
//! A contained child runs as
//! `bwrap --dev-bind / / --unshare-pid --proc /proc --die-with-parent --
//! <hunter> <REAPER_MODE> <program> <args>`. bwrap's own init is PID 1 of
//! a fresh PID namespace, and when that init dies the kernel SIGKILLs
//! every process in the namespace: a `setsid` or a double fork does not
//! get out of it. bwrap dies with its parent (`--die-with-parent`), and
//! its init with bwrap, so when the daemon dies, however it dies,
//! everything contained dies with it. A daemon that starts therefore
//! owns no worker of an earlier one (`crate::recovery`).
//!
//! The namespace's main process is hunter's own binary in reaper mode
//! ([`reap`]), with the program as its child. bwrap exits the moment its
//! main process does, and the namespace dies with it; were that the
//! program, a program that exits would SIGKILL every descendant at once,
//! mid-cleanup (git, under omp, removing its lock files). The reaper
//! adopts the program's orphans (`PR_SET_CHILD_SUBREAPER`), and gives
//! whatever is left, told to stop or after the program exits, a SIGTERM
//! and the time to act on it. It reports the program's status on a pipe
//! of its own the moment the program ends, so [`Child::try_wait`] tells
//! when the program ended, not when its namespace did: a caller's
//! deadline is not spent on what the program left finishing its cleanup.
//!
//! `--die-with-parent` is `PR_SET_PDEATHSIG`, which fires when the
//! *thread* that spawned bwrap exits, not the process (prctl(2)). A tokio
//! blocking-pool thread is retired once idle, and a child spawned from it
//! would be killed mid-run. So [`Command::spawn`] hands every spawn to one
//! thread that lives as long as the process ([`spawner`]); the [`Child`]
//! it returns is waited for and killed from any thread.
//!
//! bwrap arms that signal itself only once its namespace is set up, a few
//! milliseconds into the spawn. So the forked child arms it before it
//! execs bwrap ([`die_with_daemon`]), and exits instead if the daemon
//! already died in between; the setting survives the exec of a bwrap
//! that is not set-user-ID and runs under the daemon's own security
//! label. bwrap is then bound to the daemon from its first instruction.
//! What remains is inside bwrap: the namespace's init, which bwrap
//! clones, arms its own signal against bwrap only after the clone, so a
//! daemon (and with it bwrap) killed in that gap can leave that init and
//! the program it goes on to start running. An exec that changes
//! credentials drops the setting and reopens the whole window: a
//! set-user-ID bwrap, or an LSM profile attached to bwrap's path (such as
//! an apparmor(7) profile for `/usr/bin/bwrap` on Ubuntu; the transition
//! into it clears the setting). bwrap's own arming still holds then.
//!
//! The daemon itself is not run under bwrap: bwrap does not forward
//! SIGTERM, so `systemctl stop` would kill a wrapped daemon outright
//! instead of letting it finish the worker run it is in.
//!
//! A missing bwrap is an error, never a reason to spawn unbound, and so
//! is one that cannot set up its namespaces: the daemon refuses to start
//! unless a probe spawn through it succeeds ([`check`]).

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

/// The hidden first argument that makes the hunter binary a reaper
/// ([`reap`]) instead of the daemon.
pub const REAPER_MODE: &str = "__hunter-reaper";

/// bwrap's options, before the `--` that ends them.
const BWRAP_ARGS: [&str; 7] = [
    "--dev-bind",
    "/",
    "/",
    "--unshare-pid",
    "--proc",
    "/proc",
    "--die-with-parent",
];

/// How long [`Child::kill_tree`] gives the processes it sent SIGTERM to
/// before it sends SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// How often [`Child::kill_tree`] looks again: whether the group is empty
/// yet, and for a process its SIGTERM has not reached.
const KILL_TICK: Duration = Duration::from_millis(50);

/// The absolute path of `bwrap`, looked up on `PATH` once per process.
///
/// Once, because the lookup has to see the daemon's own `PATH`: a worker
/// runs with a cleared environment, and tests narrow `PATH` to a
/// directory of fakes.
pub fn bwrap() -> io::Result<&'static Path> {
    static BWRAP: LazyLock<Option<PathBuf>> = LazyLock::new(|| on_path(OsStr::new("bwrap")));
    BWRAP.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bwrap (bubblewrap) not found on PATH: every process hunter \
                 starts runs in its own PID namespace, so that none outlives \
                 the daemon, and hunter starts none without it",
        )
    })
}

/// Where nextest names the `hunter` binary built for the test run.
#[cfg(any(test, feature = "test-support"))]
const TEST_REAPER_ENV: &str = "NEXTEST_BIN_EXE_hunter";

/// The executable [`command`] runs as the reaper, opened once and held
/// open for the life of the process: the running hunter binary.
///
/// Every child execs it through that descriptor (`/proc/self/fd/<n>`,
/// [`pass_reaper`]), not by path, so it is always the file this process
/// started from. By path, a binary removed or rebuilt while the daemon
/// runs (`cargo clean`, a build of another commit into `target/release`)
/// would fail every later spawn with bwrap's exit 1, or an older hunter's
/// exit 2 for an argument it does not know, either read as the program's
/// own exit code.
///
/// A test binary is not hunter's. In a test build the reaper is the
/// `hunter` binary built for the run, whose path nextest, the only runner
/// these tests support, gives every integration test as
/// [`TEST_REAPER_ENV`] (cargo's `CARGO_BIN_EXE_hunter`, at run time).
/// Unit tests get none, so nothing spawned through this module runs from
/// one.
fn reaper() -> io::Result<&'static File> {
    static REAPER: LazyLock<Result<File, String>> = LazyLock::new(|| {
        #[cfg(not(any(test, feature = "test-support")))]
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        #[cfg(any(test, feature = "test-support"))]
        let exe = std::env::var_os(TEST_REAPER_ENV)
            .map(PathBuf::from)
            .ok_or_else(|| {
                format!("{TEST_REAPER_ENV} unset: spawn only from integration tests under nextest")
            })?;
        if !is_executable_file(&exe) {
            return Err(format!("{} is not an executable file", exe.display()));
        }
        File::open(&exe).map_err(|e| format!("{}: {e}", exe.display()))
    });
    REAPER.as_ref().map_err(|e| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("hunter's own binary, which every child runs under: {e}"),
        )
    })
}

/// Hand `reaper` to the child `cmd` starts, and only to it ([`pass_fd`]).
/// Returns the path the child execs it by. The reaper closes it before it
/// starts the program ([`reap`]).
fn pass_reaper(cmd: &mut std::process::Command, reaper: &'static File) -> String {
    pass_fd(cmd, reaper);
    format!("{REAPER_FD_PREFIX}{}", reaper.as_raw_fd())
}

/// Hand the descriptor `fd` to the child `cmd` starts, and only to it: it
/// stays close-on-exec in this process, and the forked child clears that
/// before it execs bwrap, which passes it on. `cmd` keeps `fd` open until
/// it is dropped, after its spawn.
fn pass_fd(cmd: &mut std::process::Command, fd: impl AsFd + Send + Sync + 'static) {
    let keep = move || {
        nix::fcntl::fcntl(
            &fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::empty()),
        )?;
        Ok(())
    };
    // SAFETY: `keep` runs between fork and exec, where only
    // async-signal-safe calls are allowed: it makes one, fcntl(2), and
    // allocates nothing.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(keep);
    }
}

/// What the reaper's path starts with: it is exec'd by its descriptor.
const REAPER_FD_PREFIX: &str = "/proc/self/fd/";

/// Start `/bin/sh -c 'exit 0'` through [`command`], as every child is
/// started, and require it to exit 0. The daemon runs this before
/// anything else, and does not start otherwise.
///
/// bwrap on `PATH` is not enough. Where it may not create its namespaces
/// (unprivileged user namespaces disallowed), every contained spawn still
/// succeeds and bwrap exits 1, which callers read as the program's own
/// exit 1: a failed worker, a Renovate with no answer. This also opens
/// the hunter binary every contained child runs under ([`reaper`]) at
/// startup.
pub fn check() -> io::Result<()> {
    use std::io::Read as _;
    let mut cmd = command("/bin/sh");
    cmd.args(["-c", "exit 0"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let mut said = Vec::new();
    if let Some(mut stderr) = child.take_stderr() {
        let _ = stderr.read_to_end(&mut said);
    }
    let status = child.wait()?;
    if status.success() {
        return Ok(());
    }
    Err(io::Error::other(format!(
        "bwrap cannot start a process here ({status}: {}). Every worker \
         hunter starts runs in a PID namespace of its own, so that none \
         outlives the daemon. Most likely unprivileged user namespaces are \
         not allowed: AppArmor's kernel.apparmor_restrict_unprivileged_userns \
         (Ubuntu), kernel.unprivileged_userns_clone=0, a container that \
         forbids them, or systemd's RestrictNamespaces=",
        String::from_utf8_lossy(&said).trim()
    )))
}

/// A command to start, contained ([`command`]) or one of the daemon's own
/// tools ([`tool`]). Configured as a [`std::process::Command`] is --
/// arguments, environment, working directory, stdio -- and started with
/// [`Self::spawn`]. Not a `Deref` to one: a chained `.arg(..).spawn()`
/// would then reach the plain command's `spawn`, from whatever thread
/// called it.
pub struct Command {
    cmd: std::process::Command,
    /// Why this command cannot run: bwrap or the program not found. Told
    /// at [`Self::spawn`], where a missing program always surfaced.
    unspawnable: Option<io::Error>,
    contained: bool,
    /// A contained command's status pipe, the end the [`Child`] reads the
    /// program's status from. The end the reaper writes it to is held by
    /// `cmd` ([`pass_fd`]), so this process closes it with `cmd`, once
    /// spawned ([`reap`]).
    status: Option<File>,
}

/// A contained command: `program` under bwrap and the reaper, in its own
/// process group (whose leader is bwrap: see [`Child::kill_tree`]).
///
/// A bare `program` is looked up on the daemon's `PATH` now, as spawning
/// it directly would have: not found is then the spawn's error, as it was,
/// rather than an exit status from bwrap. A path is passed on as it is.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    let resolved = bwrap().and_then(|bwrap| {
        let (read, write) = nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC)?;
        // Read on every poll ([`Child::try_wait`]), which must not wait
        // for the program to end.
        nix::fcntl::fcntl(
            &read,
            nix::fcntl::FcntlArg::F_SETFL(nix::fcntl::OFlag::O_NONBLOCK),
        )?;
        Ok((bwrap, reaper()?, resolve(program)?, (read, write)))
    });
    let (mut cmd, unspawnable, status) = match resolved {
        Ok((bwrap, reaper, program, (read, write))) => {
            let mut cmd = std::process::Command::new(bwrap);
            let reaper = pass_reaper(&mut cmd, reaper);
            let write_fd = write.as_raw_fd();
            pass_fd(&mut cmd, write);
            cmd.args(BWRAP_ARGS)
                .arg("--")
                .arg(reaper)
                .arg(REAPER_MODE)
                .arg(write_fd.to_string())
                .arg(program);
            die_with_daemon(&mut cmd);
            (cmd, None, Some(File::from(read)))
        }
        Err(e) => (std::process::Command::new(program), Some(e), None),
    };
    cmd.process_group(0);
    Command {
        cmd,
        unspawnable,
        contained: true,
        status,
    }
}

/// One of the daemon's own tools: `program` as a plain child, in a process
/// group of its own so that [`Child::kill_tree`] reaches all of it.
pub fn tool(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = std::process::Command::new(program);
    cmd.process_group(0);
    Command {
        cmd,
        unspawnable: None,
        contained: false,
        status: None,
    }
}

/// Whether this reaper was told to stop (SIGTERM).
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Whether this reaper has sent its namespace the SIGTERM ([`terminate_rest`]).
static TERMINATED: AtomicBool = AtomicBool::new(false);

/// SIGTERM, once, every other process of the reaper's namespace:
/// `kill(-1)` from inside a PID namespace reaches exactly its processes but
/// the init and the caller, and a process being forked meanwhile is
/// signalled too. Only as pid 2, bwrap's program in a namespace of its
/// own: anywhere else `kill(-1)` would reach every process of the user's.
/// Async-signal-safe.
fn terminate_rest() {
    if nix::unistd::getpid().as_raw() == 2 && !TERMINATED.swap(true, Ordering::SeqCst) {
        let _ = signal::kill(Pid::from_raw(-1), Signal::SIGTERM);
    }
}

extern "C" fn on_sigterm(_: nix::libc::c_int) {
    STOPPING.store(true, Ordering::SeqCst);
    terminate_rest();
}

/// The reaper: run `argv` (the status descriptor, the program, then its
/// arguments) with the program as its child, and reap every child, the
/// program's orphans included, then exit with the program's status: its
/// exit code, or 128 + the signal that killed it, as bwrap reports one.
/// The hunter binary runs this when its first argument is
/// [`REAPER_MODE`], with `exe` the path it was exec'd by; it is the main
/// process of every contained child's namespace ([`command`]).
///
/// The program gets the reaper's environment, working directory, file
/// descriptors and process group unchanged, and default signal handling.
///
/// The moment the program ends, its status goes to the daemon on the
/// status descriptor, one byte, before anything is done about what it
/// left: the daemon learns that the program ended, not only when the
/// namespace does ([`Child::try_wait`]).
///
/// On SIGTERM the reaper sends every other process of the namespace a
/// SIGTERM, once ([`terminate_rest`]), and waits for all of them to exit,
/// however long: the daemon's own deadline ends the namespace
/// ([`Child::kill_tree`]). When the program exits without one, whatever
/// it left running gets the same SIGTERM and up to [`TERM_GRACE`] to exit:
/// then an `alarm(2)` set at the program's end kills the reaper (SIGALRM,
/// at its default), and the namespace with it.
///
/// It exits with `_exit`, running no exit handlers: it has nothing to
/// flush, and a coverage-instrumented build would otherwise write its
/// profile into the program's working directory.
pub fn reap(exe: &OsStr, argv: &[OsString]) -> ! {
    let code = match run_reaper(exe, argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("hunter reaper: {e}");
            127
        }
    };
    // SAFETY: `_exit` ends the process at once; nothing runs after it,
    // and nothing of this process needs to.
    #[allow(unsafe_code)]
    unsafe {
        nix::libc::_exit(code)
    }
}

fn run_reaper(exe: &OsStr, argv: &[OsString]) -> io::Result<i32> {
    use nix::errno::Errno;
    let [status_fd, program, rest @ ..] = argv else {
        return Err(io::Error::other("no status descriptor and program to run"));
    };
    let status_fd: std::os::fd::RawFd = status_fd
        .to_str()
        .and_then(|fd| fd.parse().ok())
        .ok_or_else(|| io::Error::other("the status descriptor is not a number"))?;
    // SAFETY: the daemon passed this descriptor to the reaper alone
    // ([`command`]); nothing else in this process uses it.
    #[allow(unsafe_code)]
    let mut report = unsafe { <File as std::os::fd::FromRawFd>::from_raw_fd(status_fd) };
    // Not the program's: closed at its exec.
    nix::fcntl::fcntl(
        &report,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )?;
    // Exec'd by the descriptor the daemon passed ([`pass_reaper`]): the
    // program does not get it.
    if let Some(fd) = exe
        .to_str()
        .and_then(|exe| exe.strip_prefix(REAPER_FD_PREFIX))
        .and_then(|fd| fd.parse::<std::os::fd::RawFd>().ok())
    {
        nix::unistd::close(fd)?;
    }
    nix::sys::prctl::set_child_subreaper(true)?;
    // Held until the program runs: a SIGTERM that comes sooner is
    // delivered then, so its broadcast reaches the program.
    let term = signal::SigSet::from(Signal::SIGTERM);
    term.thread_block()?;
    let on_term = signal::SigAction::new(
        signal::SigHandler::Handler(on_sigterm),
        signal::SaFlags::empty(),
        signal::SigSet::empty(),
    );
    // SAFETY: the handler makes only async-signal-safe calls: an atomic
    // store and swap, getpid and kill.
    #[allow(unsafe_code)]
    unsafe {
        signal::sigaction(Signal::SIGTERM, &on_term)?;
    }
    // At its default, so that the grace's alarm ends the reaper, and not
    // blocked, whatever the daemon's spawning thread had.
    let default = signal::SigAction::new(
        signal::SigHandler::SigDfl,
        signal::SaFlags::empty(),
        signal::SigSet::empty(),
    );
    // SAFETY: restores the default disposition; no handler runs.
    #[allow(unsafe_code)]
    unsafe {
        signal::sigaction(Signal::SIGALRM, &default)?;
    }
    signal::SigSet::from(Signal::SIGALRM).thread_unblock()?;
    let mut spawn = std::process::Command::new(program);
    spawn.args(rest);
    // The program starts with SIGTERM at its default and not blocked,
    // whatever the reaper holds: the fork inherits both. A SIGTERM sent
    // to the fork before this is pending and kills it here, before the
    // program runs.
    let default_term = move || {
        let default = signal::SigAction::new(
            signal::SigHandler::SigDfl,
            signal::SaFlags::empty(),
            signal::SigSet::empty(),
        );
        // SAFETY: sigaction is async-signal-safe.
        #[allow(unsafe_code)]
        unsafe {
            signal::sigaction(Signal::SIGTERM, &default)?;
        }
        term.thread_unblock()?;
        Ok(())
    };
    // SAFETY: `default_term` runs between fork and exec, where only
    // async-signal-safe calls are allowed: sigaction and pthread_sigmask,
    // no allocation.
    #[allow(unsafe_code)]
    unsafe {
        spawn.pre_exec(default_term);
    }
    let spawned = spawn
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", program.to_string_lossy())));
    term.thread_unblock()?;
    let program = spawned?.id() as i32;
    let mut ended = None;
    loop {
        // libc's waitpid, not nix's: nix fails a status it cannot decode
        // -- a death by a real-time signal -- after the kernel has reaped
        // the child.
        let mut status = 0;
        // SAFETY: waitpid writes the status into `status`, which outlives
        // the call.
        #[allow(unsafe_code)]
        let reaped = unsafe { nix::libc::waitpid(-1, &raw mut status, 0) };
        match reaped {
            -1 => match Errno::last() {
                Errno::EINTR => {}
                Errno::ECHILD => return Ok(ended.unwrap_or(127)),
                e => return Err(e.into()),
            },
            pid if pid == program && ended.is_none() => {
                let Some(code) = exit_code(status) else {
                    continue;
                };
                ended = Some(code);
                // A code is 0..=255 (128 + a signal number included). Lost
                // only if the daemon is gone, which ends the namespace
                // anyway.
                let _ = std::io::Write::write_all(&mut report, &[code as u8]);
                // Read only after a wait returned: a SIGTERM sent before
                // the program's end was delivered, its handler run, on the
                // way out of the wait that saw the program end.
                if !STOPPING.load(Ordering::SeqCst) {
                    terminate_rest();
                    nix::unistd::alarm::set(
                        u32::try_from(TERM_GRACE.as_secs()).unwrap_or(u32::MAX),
                    );
                }
            }
            _ => {}
        }
    }
}

/// The exit code a wait `status` reports, as bwrap reports one: the
/// code of an exit, 128 + the signal of a death by signal (real-time ones
/// included); `None` for a status that is not an end.
fn exit_code(status: nix::libc::c_int) -> Option<i32> {
    use nix::libc::{WEXITSTATUS, WIFEXITED, WIFSIGNALED, WTERMSIG};
    if WIFEXITED(status) {
        Some(WEXITSTATUS(status))
    } else if WIFSIGNALED(status) {
        Some(128 + WTERMSIG(status))
    } else {
        None
    }
}

/// Arm `PR_SET_PDEATHSIG` (SIGKILL) in the forked child, before it execs
/// `cmd`, then check the daemon is still its parent: one that died
/// between the fork and the arming signals nobody, so the child refuses
/// to exec at all (the standard race check). The parent this arms against
/// is the forking thread, which is the [`spawner`].
fn die_with_daemon(cmd: &mut std::process::Command) {
    let daemon = nix::unistd::getpid();
    let arm = move || {
        nix::sys::prctl::set_pdeathsig(Signal::SIGKILL)?;
        if nix::unistd::getppid() != daemon {
            return Err(io::Error::from_raw_os_error(
                nix::errno::Errno::ESRCH as i32,
            ));
        }
        Ok(())
    };
    // SAFETY: `arm` runs between fork and exec, where only
    // async-signal-safe calls are allowed. It makes two system calls,
    // prctl and getppid, touches no lock and allocates nothing: `Errno`
    // converts to `io::Error` by its raw code.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(arm);
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

    /// The program starts with no environment but what [`Self::env`]
    /// gives it. bwrap needs none.
    pub fn env_clear(&mut self) -> &mut Self {
        self.cmd.env_clear();
        self
    }

    /// bwrap starts the program in the same directory.
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

    /// Start the command: a contained one on the [`spawner`] thread, a
    /// tool on this one (it has no parent-death signal to bind to a
    /// thread).
    pub fn spawn(self) -> io::Result<Child> {
        self.start(None)
    }

    /// [`Self::spawn`], with the child tied to `leash`: [`Leash::cut`]
    /// kills it.
    pub fn spawn_leashed(self, leash: &Leash) -> io::Result<Child> {
        self.start(Some(leash))
    }

    fn start(mut self, leash: Option<&Leash>) -> io::Result<Child> {
        if let Some(e) = self.unspawnable {
            return Err(e);
        }
        let inner = if self.contained {
            let gone = || io::Error::other("the process spawner thread is gone");
            let (reply, answer) = mpsc::sync_channel(1);
            spawner()?.send((self.cmd, reply)).map_err(|_| gone())?;
            answer.recv().map_err(|_| gone())??
        } else {
            self.cmd.spawn()?
        };
        let mut child = Child {
            inner,
            leash: None,
            contained: self.contained,
            status: self.status,
            program: None,
        };
        if let Some(leash) = leash {
            leash.attach(&mut child);
        }
        Ok(child)
    }
}

/// A spawn request: the command, and where to send what spawning it gave.
type Spawn = (
    std::process::Command,
    mpsc::SyncSender<io::Result<std::process::Child>>,
);

/// The one thread every child is spawned from. It is never retired: its
/// receiver lives as long as the sender in the static, which is never
/// dropped, so the thread exits only with the process -- the parent
/// death bwrap's `--die-with-parent` waits for.
fn spawner() -> io::Result<&'static mpsc::Sender<Spawn>> {
    static SPAWNER: LazyLock<Result<mpsc::Sender<Spawn>, String>> = LazyLock::new(|| {
        let (requests, incoming) = mpsc::channel::<Spawn>();
        std::thread::Builder::new()
            .name("hunter-spawner".to_owned())
            .spawn(move || {
                for (mut cmd, reply) in incoming {
                    // A requester that stopped waiting leaves a child no
                    // one would reap or stop.
                    if let Err(mpsc::SendError(Ok(mut orphan))) = reply.send(cmd.spawn()) {
                        let _ = orphan.kill();
                        let _ = orphan.wait();
                    }
                }
            })
            .map(|_| requests)
            .map_err(|e| e.to_string())
    });
    SPAWNER
        .as_ref()
        .map_err(|e| io::Error::other(format!("cannot start the process spawner thread: {e}")))
}

/// A process started by [`Command::spawn`]: for a contained command
/// bwrap, with the program in its PID namespace; for a tool the program.
/// Either leads a process group of its own.
///
/// Dropped before it was reaped, its group is killed (SIGKILL) and it is
/// reaped: whatever returns early or unwinds past a child cannot leave it
/// running.
pub struct Child {
    inner: std::process::Child,
    leash: Option<Leash>,
    contained: bool,
    /// A contained child's status pipe, which its reaper writes the
    /// program's status to the moment the program ends ([`reap`]).
    status: Option<File>,
    /// That status, once read.
    program: Option<ExitStatus>,
}

impl Child {
    /// The pid of bwrap or of the tool, which is also the process group's id.
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

    /// The program's status: for a contained child as soon as its reaper
    /// reported the program ended, though what it left may still run in
    /// the namespace (and bwrap with it) -- [`Self::kill_tree`] or
    /// [`Self::wait`] end that; otherwise once the child is reaped. A
    /// contained child that ended without a report (bwrap failed, or was
    /// killed before the program ended) gives bwrap's own status.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let leader = self.try_wait_leader()?;
        Ok(self.program_ended().or(leader))
    }

    /// Wait until the child is reaped -- for a contained child bwrap, so
    /// the namespace is gone -- and give [`Self::try_wait`]'s status. Blocks
    /// a [`Leash::cut`] until then, so call it only on a child that is
    /// exiting: one killed already, or whose program has ended.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut tether = self.leash.as_ref().map(Leash::lock);
        let status = self.inner.wait();
        if let (Some(tether), Ok(_)) = (&mut tether, &status) {
            tether.pid = None;
        }
        drop(tether);
        let leader = status?;
        Ok(self.program_ended().unwrap_or(leader))
    }

    /// [`std::process::Child::try_wait`] on the group's leader. Reaps under
    /// the leash's lock, so a [`Leash::cut`] never signals a pid this has
    /// already released.
    fn try_wait_leader(&mut self) -> io::Result<Option<ExitStatus>> {
        let mut tether = self.leash.as_ref().map(Leash::lock);
        let status = self.inner.try_wait();
        if let (Some(tether), Ok(Some(_))) = (&mut tether, &status) {
            tether.pid = None;
        }
        status
    }

    /// The program's status from the status pipe, once the reaper wrote it.
    fn program_ended(&mut self) -> Option<ExitStatus> {
        if self.program.is_none()
            && let Some(pipe) = &mut self.status
        {
            let mut code = [0u8; 1];
            if matches!(std::io::Read::read(pipe, &mut code), Ok(1)) {
                use std::os::unix::process::ExitStatusExt as _;
                // The wait status of an exit with that code.
                self.program = Some(ExitStatus::from_raw(i32::from(code[0]) << 8));
            }
        }
        self.program
    }

    /// Stop the program and everything it started, then reap the child:
    /// SIGTERM, up to [`TERM_GRACE`] for the whole group to exit, then
    /// SIGKILL to the group.
    ///
    /// A tool gets the SIGTERM as a group: every process in it, at once.
    ///
    /// A contained child gets it through its reaper only, which sends
    /// every other process of its namespace a SIGTERM of its own and then
    /// waits for all of them ([`reap`]), so a descendant's cleanup (git,
    /// under omp, removing its lock files) is not cut short by the program
    /// finishing its own sooner. bwrap is never sent SIGTERM: it has no
    /// handler and would take the namespace down at once. A reaper not
    /// started yet is looked for again on every tick; one that has not
    /// installed its handler yet dies of the signal, and the namespace
    /// with it.
    ///
    /// The wait is for the whole group, not the direct child: a
    /// descendant still alive holds the inherited stdout and stderr, so
    /// a caller reading them to EOF would block past the deadline this
    /// exists to enforce.
    pub fn kill_tree(&mut self) {
        let pgid = self.id();
        // The reaper to signal, while there is one to look for. Without
        // `/proc/<pid>/task/<tid>/children` (a kernel built without
        // `CONFIG_PROC_CHILDREN`) there is none to find, and the whole
        // group gets the SIGTERM, bwrap included: no grace, but stopped.
        let mut look_for_reaper = self.contained
            && std::fs::metadata(format!("/proc/{pgid}/task/{pgid}/children")).is_ok();
        if !look_for_reaper {
            killpg(pgid, Signal::SIGTERM);
        }
        let deadline = Instant::now() + TERM_GRACE;
        loop {
            if look_for_reaper && let Some(reaper) = reaper_under(pgid) {
                kill_process(reaper, Signal::SIGTERM);
                look_for_reaper = false;
            }
            // Reap first: a zombie leader still counts as a group member,
            // so an unreaped one would make the probe below claim the
            // group is alive forever.
            let _ = self.try_wait_leader();
            if !group_alive(pgid) {
                break;
            }
            if Instant::now() >= deadline {
                killpg(pgid, Signal::SIGKILL);
                break;
            }
            std::thread::sleep(KILL_TICK);
        }
        let _ = self.wait();
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // The whole group, not only its leader: a bwrap killed while still
        // setting up may not have armed its namespace's init yet, which
        // would then run on; and a tool's descendants are not its leader.
        // Not once reaped: the group's id may be free again.
        if matches!(self.try_wait_leader(), Ok(None)) {
            kill(self.id());
        }
        let _ = self.wait();
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
    /// The leashed child's pid, until it is reaped.
    pid: Option<u32>,
}

impl Leash {
    fn lock(&self) -> std::sync::MutexGuard<'_, Tether> {
        // Two plain fields, each written in one statement: a panic while
        // the lock was held cannot have left them inconsistent.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tie a just-spawned `child` (unreaped) to this leash; killed at
    /// once if the leash is already cut.
    fn attach(&self, child: &mut Child) {
        let mut tether = self.lock();
        if tether.cut {
            kill(child.id());
        } else {
            tether.pid = Some(child.id());
        }
        child.leash = Some(self.clone());
    }

    /// SIGKILL the leashed child, if it has not been reaped, and any
    /// child leashed from now on; whether there was one to kill. The
    /// child's own thread still reaps it.
    pub fn cut(&self) -> bool {
        let mut tether = self.lock();
        tether.cut = true;
        tether.pid.take().inspect(|&pid| kill(pid)).is_some()
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

/// `program` as bwrap is to exec it: a bare name looked up on `PATH`, a
/// path as it is.
fn resolve(program: &OsStr) -> io::Result<OsString> {
    if program.as_bytes().contains(&b'/') {
        return Ok(program.to_owned());
    }
    on_path(program)
        .map(PathBuf::into_os_string)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{}: not found on PATH", program.to_string_lossy()),
            )
        })
}

/// The first executable `name` in a `PATH` directory.
fn on_path(name: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

/// A regular file with an execute bit.
pub fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// SIGKILL a child's process group (`pgid`): for a contained child bwrap,
/// its namespace's init and the program. Signalling bwrap alone misses an
/// init it has not armed yet, a few milliseconds into the spawn. Call it
/// only on an unreaped leader, whose group id cannot have been reused.
fn kill(pgid: u32) {
    killpg(pgid, Signal::SIGKILL);
}

/// Send `sig` (`None`: the null signal) to the process group `pgid`;
/// whether it was sent. Never to a group [`target`] refuses.
fn killpg<T: Into<Option<Signal>>>(pgid: u32, sig: T) -> bool {
    target(pgid).is_some_and(|pgid| signal::killpg(pgid, sig).is_ok())
}

/// Send `sig` (`None`: the null signal) to the one process `pid`;
/// whether it was sent. Never to a process [`target`] refuses.
fn kill_process<T: Into<Option<Signal>>>(pid: u32, sig: T) -> bool {
    target(pid).is_some_and(|pid| signal::kill(pid, sig).is_ok())
}

/// `id` as a process or process group to signal, refusing 0 (to
/// `kill(2)` the caller's own group), 1 (init; as a group `kill(-1)`,
/// every process the daemon's user may signal) and an id beyond `pid_t`
/// (negative as one: a group). A child's id is none of these, so a wrong
/// one comes from a bug, which then signals nothing rather than the
/// operator's whole session.
fn target(id: u32) -> Option<Pid> {
    match i32::try_from(id) {
        Ok(id) if id > 1 => Some(Pid::from_raw(id)),
        _ => {
            tracing::debug!("refused to signal process (group) {id}");
            None
        }
    }
}

/// Is any process still in the group? The null signal performs the
/// permission and existence checks without delivering anything.
fn group_alive(pgid: u32) -> bool {
    killpg(pgid, None)
}

/// The reaper under `bwrap`, once there is one: the child of bwrap's
/// namespace init (bwrap's child), which has no other.
fn reaper_under(bwrap: u32) -> Option<u32> {
    children(bwrap).into_iter().flat_map(children).next()
}

/// The children of `pid`, listed by each of its threads: a child is
/// listed only under the thread that forked it. A process or thread gone
/// meanwhile lists none.
fn children(pid: u32) -> Vec<u32> {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|task| std::fs::read_to_string(task.path().join("children")).ok())
        .flat_map(|list| {
            list.split_whitespace()
                .filter_map(|child| child.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// What [`die_with_daemon`] arms survives the exec of a program that
    /// is not set-user-ID, so the program is bound to its spawning thread
    /// from its first instruction. Checked on a plain `sleep`, not bwrap,
    /// which would arm itself: spawned from a thread that exits once the
    /// spawn returns, after the exec, it must die then. Unarmed, it sleeps
    /// on.
    #[test]
    fn a_spawn_is_armed_to_die_with_its_spawning_thread() {
        let mut child = std::thread::spawn(|| {
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("300");
            die_with_daemon(&mut cmd);
            cmd.spawn().unwrap()
        })
        .join()
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut exited = child.try_wait().unwrap();
        while exited.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            exited = child.try_wait().unwrap();
        }
        if exited.is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }

        assert!(
            exited.is_some(),
            "the spawn was not armed: it outlived its thread"
        );
    }

    /// No signal goes to group or process 0 (the daemon's own group), 1
    /// (as a group, `kill(-1)`: every process of the operator's) or an id
    /// that is negative as a `pid_t`, whatever asks. Probed with the null
    /// signal, which delivers nothing: a broken guard reads as sent.
    #[test]
    fn no_signal_goes_to_the_daemons_group_or_every_process() {
        let beyond_pid_t = u32::try_from(i32::MAX).unwrap() + 2;
        for id in [0, 1, beyond_pid_t, u32::MAX] {
            assert!(!killpg(id, None), "a signal to group {id}");
            assert!(!kill_process(id, None), "a signal to process {id}");
        }
        let own = nix::unistd::getpgrp().as_raw().unsigned_abs();
        assert!(killpg(own, None), "group {own} is this test's own");
        let me = std::process::id();
        assert!(kill_process(me, None), "process {me} is this test");
    }
}
