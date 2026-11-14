#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `hunter::child`: every process the daemon starts runs in a cgroup of
//! its own inside its unit of work's, and nothing of it outlives the end
//! of its run or unit, the drop of its `Child`, or the owner that stopped
//! waiting; a daemon refuses to start beside what an earlier one left
//! running.
//!
//! Most children here leave a grandchild in a session of its own, as a
//! `setsid` or a double fork does: out of the process group, so a group
//! kill alone never reaches it. Processes are counted by their working
//! directory (`support::processes_in`), which needs no pid of theirs.
//!
//! These tests need a cgroup v2 subtree they may manage, as the daemon
//! does: `just test` and CI run nextest through
//! `scripts/in-delegated-cgroup.sh`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use hunter::child::{self, Child, Leash};
use support::{TempDir, eventually, processes_in};

/// A child that leaves a grandchild in its own session, which writes
/// `escaped` once it runs, then waits for good.
const ESCAPING: &str = "setsid sh -c 'touch escaped; exec sleep 300' & exec sleep 300";

/// Long enough for any process the kernel has been told to kill to be
/// gone; far shorter than the children's own 300 s.
const GONE_WITHIN: Duration = Duration::from_secs(5);

/// What `kill_tree` gives the processes it sent SIGTERM to
/// (`child::TERM_GRACE`).
const GRACE: Duration = Duration::from_secs(10);

/// Where [`owner_role`] runs its child; set only when this test binary
/// is re-executed as that owner.
const OWNER_DIR: &str = "HUNTER_CHILD_TEST_OWNER_DIR";

/// `sh -c <script>` in `dir`, through [`child::command`].
fn start_sh(dir: &Path, script: &str) -> Child {
    let mut cmd = child::command("sh");
    cmd.args(["-c", script])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn in a cgroup of its own")
}

/// [`start_sh`], once its escaped grandchild runs.
fn spawn_in(dir: &Path, script: &str) -> Child {
    let child = start_sh(dir, script);
    assert!(
        eventually(GONE_WITHIN, || dir.join("escaped").exists()),
        "the grandchild never ran"
    );
    child
}

/// The cgroup process `pid` runs in.
fn cgroup_of(pid: u32) -> PathBuf {
    let listed = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = listed
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("cgroup v2");
    Path::new("/sys/fs/cgroup").join(path.trim_start_matches('/'))
}

/// The pid `sh` wrote to `dir/<file>` once it exists.
fn pid_in(dir: &Path, file: &str) -> u32 {
    assert!(eventually(GONE_WITHIN, || {
        std::fs::read_to_string(dir.join(file)).is_ok_and(|s| s.ends_with('\n'))
    }));
    std::fs::read_to_string(dir.join(file))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// Every child runs in a new cgroup of its own, named after the program,
/// in the cgroup of its unit (outside any: `other`), and so does what it
/// starts, however it detaches.
#[test]
fn a_child_and_what_it_starts_run_in_a_cgroup_of_their_own() {
    // Its own cgroup: in one shared with other tests' processes, another
    // process may make a cgroup of the same name once this one's is gone.
    support::private_cgroup("child-cgroup");
    let dir = TempDir::new("child-cgroup");
    let mut child = spawn_in(
        dir.path(),
        "setsid sh -c 'echo $$ > grandchild.tmp; mv grandchild.tmp grandchild; touch escaped; \
         exec sleep 300' & exec sleep 300",
    );
    let root = child::units_root().unwrap();

    let cgroup = cgroup_of(child.id());
    let grandchild = cgroup_of(pid_in(dir.path(), "grandchild"));

    assert_eq!(
        cgroup.parent(),
        Some(root.join("other").as_path()),
        "{}",
        cgroup.display()
    );
    assert!(
        cgroup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("-sh"),
        "{}",
        cgroup.display()
    );
    assert_eq!(grandchild, cgroup, "the setsid grandchild left the cgroup");
    child.kill_tree();
    assert!(!cgroup.exists(), "kill_tree left {}", cgroup.display());
}

/// The cap's kill: the whole tree goes, the escaped grandchild included,
/// and the child is reaped.
#[test]
fn kill_tree_ends_a_grandchild_that_left_its_session() {
    let dir = TempDir::new("child-kill-tree");
    let mut child = spawn_in(dir.path(), ESCAPING);

    child.kill_tree();

    assert!(child.try_wait().unwrap().is_some(), "reaped");
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived kill_tree",
        processes_in(dir.path())
    );
}

/// The kill is SIGTERM first, and the program gets to use it: a worker's
/// git removes its lock files on SIGTERM.
#[test]
fn kill_tree_lets_the_program_handle_its_sigterm() {
    let dir = TempDir::new("child-term");
    let mut child = spawn_in(
        dir.path(),
        "trap 'sleep 1; touch cleaned; exit 0' TERM; touch escaped; while :; do sleep 0.1; done",
    );

    child.kill_tree();

    assert!(dir.join("cleaned").exists(), "the SIGTERM handler ran");
}

/// The grace is for everything in the cgroup, not only for the program: a
/// worker's git, under omp, removes its lock files on SIGTERM. Here the
/// descendant takes a second to clean, the program none.
#[test]
fn kill_tree_lets_a_descendant_finish_its_sigterm_cleanup() {
    let dir = TempDir::new("child-term-descendant");
    std::fs::write(dir.join("lock"), "").unwrap();
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "sleep 1; rm -f lock; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while :; do sleep 0.1; done"#,
    );

    child.kill_tree();

    assert!(
        !dir.join("lock").exists(),
        "the descendant's SIGTERM cleanup was cut short"
    );
}

/// A descendant that ignores SIGTERM costs the program nothing: both get
/// the SIGTERM at once, so the program's handler runs at the start of the
/// grace, not squeezed between a SIGTERM and a SIGKILL at its end. The
/// descendant goes at the deadline.
#[test]
fn kill_tree_runs_the_programs_handler_beside_a_descendant_ignoring_sigterm() {
    let dir = TempDir::new("child-term-stubborn");
    let mut child = spawn_in(
        dir.path(),
        r#"trap 'sleep 0.5; touch cleaned; exit 0' TERM
sh -c 'trap "" TERM; touch escaped; while :; do sleep 0.1; done' &
while :; do sleep 0.1; done"#,
    );

    child.kill_tree();

    assert!(
        dir.join("cleaned").exists(),
        "the program's handler never ran"
    );
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived kill_tree",
        processes_in(dir.path())
    );
}

/// A process the program orphaned before the kill (here by a subshell
/// that has exited since, so it is no descendant of the program any
/// more) gets the grace for its cleanup too.
#[test]
fn kill_tree_lets_an_orphan_finish_its_sigterm_cleanup() {
    let dir = TempDir::new("child-term-orphan");
    std::fs::write(dir.join("lock"), "").unwrap();
    let mut child = spawn_in(
        dir.path(),
        r#"( sh -c 'trap "sleep 1; rm -f lock; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' & )
touch orphaned
while :; do sleep 0.1; done"#,
    );
    assert!(eventually(GONE_WITHIN, || dir.join("orphaned").exists()));

    child.kill_tree();

    assert!(
        !dir.join("lock").exists(),
        "the orphan's SIGTERM cleanup was cut short"
    );
}

/// A SIGTERM handler may start a process for its cleanup. That process
/// is not sent a SIGTERM of its own: each process in the cgroup gets one,
/// when the kill begins. (The handler resets SIGTERM first: run again by
/// a second SIGTERM it would start a fresh cleanup, which would hide the
/// first one's death.)
#[test]
fn kill_tree_lets_a_handler_start_its_cleanup() {
    let dir = TempDir::new("child-term-cleanup-command");
    let mut child = spawn_in(
        dir.path(),
        r#"trap 'trap - TERM; sh -c "sleep 0.5; touch saved"; exit 0' TERM
touch escaped
while :; do sleep 0.1; done"#,
    );

    child.kill_tree();

    assert!(
        dir.join("saved").exists(),
        "the handler's cleanup was cut short"
    );
}

/// A program that ignores SIGTERM is killed once the grace is over, and
/// not before: the grace is what it was given to clean up in.
#[test]
fn kill_tree_kills_a_program_that_ignores_sigterm_at_the_deadline() {
    let dir = TempDir::new("child-term-ignored");
    let mut child = spawn_in(
        dir.path(),
        "trap '' TERM; touch escaped; while :; do sleep 1; done",
    );
    let (killed, done) = std::sync::mpsc::channel();
    let t0 = Instant::now();

    // On a thread, so a kill_tree that never returns fails this test
    // rather than hanging it.
    std::thread::spawn(move || {
        child.kill_tree();
        let _ = killed.send(());
    });

    assert!(
        done.recv_timeout(GRACE * 3).is_ok(),
        "kill_tree did not return"
    );
    assert!(
        t0.elapsed() + Duration::from_secs(1) >= GRACE,
        "killed after {:?}, inside the grace",
        t0.elapsed()
    );
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived kill_tree",
        processes_in(dir.path())
    );
}

/// A program's end is reported the moment it ends, while what it left
/// still runs: a caller's deadline is not spent on that. `kill_tree` then
/// gives the leftover its SIGTERM and the time for its cleanup, and the
/// status stays the program's.
#[test]
fn the_programs_end_is_reported_at_once_and_its_leftover_gets_the_grace_after() {
    let dir = TempDir::new("child-exit-early-status");
    std::fs::write(dir.join("lock"), "").unwrap();
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "sleep 1; rm -f lock; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while [ ! -e escaped ]; do sleep 0.01; done
exit 3"#,
    );

    let mut status = None;
    assert!(eventually(GONE_WITHIN, || {
        status = child.try_wait().unwrap();
        status.is_some()
    }));

    assert_eq!(status.and_then(|s| s.code()), Some(3));
    assert!(processes_in(dir.path()) > 0, "the leftover still runs");
    let t0 = Instant::now();
    child.kill_tree();
    assert!(
        !dir.join("lock").exists(),
        "the leftover's SIGTERM cleanup was cut short"
    );
    assert!(t0.elapsed() < GRACE, "{:?}", t0.elapsed());
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

/// What a program left that ignores SIGTERM ends at the deadline of the
/// kill that follows the program's exit, and not before.
#[test]
fn what_a_program_left_ignoring_sigterm_ends_with_the_grace() {
    let dir = TempDir::new("child-exit-stubborn");
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "" TERM; touch escaped; while :; do sleep 0.1; done' &
while [ ! -e escaped ]; do sleep 0.01; done
exit 4"#,
    );
    assert_eq!(child.wait().unwrap().code(), Some(4));
    let t0 = Instant::now();

    child.kill_tree();

    assert!(t0.elapsed() + Duration::from_secs(1) >= GRACE);
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the grace",
        processes_in(dir.path())
    );
}

/// Where [`forker_role`] forks; set only when this test binary is
/// re-executed as that forker.
const FORKER_DIR: &str = "HUNTER_CHILD_TEST_FORKER_DIR";

/// The forker [`kill_tree_reaches_a_process_forked_during_the_kill`]
/// re-executes this binary as: it touches `escaped`, then starts
/// `sleep 300` every 2 ms by a raw clone(2). Not fork(3): that blocks
/// every signal around the system call, and a forker frozen in that
/// section takes its SIGTERM only after the clone, leaving the new child
/// to the deadline's kill -- a window `child::terminate` documents, and
/// the process-group kill it replaced had too. A raw clone leaves only
/// what the freeze promises to cover. As an ordinary test it does nothing.
#[test]
fn forker_role() {
    let Some(dir) = std::env::var_os(FORKER_DIR) else {
        return;
    };
    let sleep = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|d| d.join("sleep"))
        .find(|p| p.is_file())
        .expect("sleep on PATH");
    let sleep = std::ffi::CString::new(sleep.into_os_string().into_encoded_bytes()).unwrap();
    let argv = [sleep.as_ptr(), c"300".as_ptr(), std::ptr::null()];
    std::fs::write(Path::new(&dir).join("escaped"), "").unwrap();
    loop {
        // SAFETY: the clone's child only execs, an async-signal-safe
        // system call with arguments made before, and exits if that fails.
        #[allow(unsafe_code)]
        unsafe {
            if nix::libc::syscall(nix::libc::SYS_clone, nix::libc::SIGCHLD, 0, 0, 0, 0) == 0 {
                nix::libc::execv(sleep.as_ptr(), argv.as_ptr());
                nix::libc::_exit(127);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// A process forked while the kill is under way gets a SIGTERM too: the
/// cgroup is frozen while the SIGTERMs go out, so nothing escapes the list
/// by forking. The forker starts one every few milliseconds; one that
/// missed its SIGTERM would hold the kill for the whole grace.
#[test]
#[cfg_attr(
    not(any(target_arch = "x86_64", target_arch = "aarch64")),
    ignore = "the forker's raw clone(2) passes its arguments in the order of x86_64 and \
              aarch64 only"
)]
fn kill_tree_reaches_a_process_forked_during_the_kill() {
    for round in 0..10 {
        let dir = TempDir::new(&format!("child-forking-{round}"));
        let mut cmd = child::command(std::env::current_exe().unwrap());
        cmd.args(["--exact", "forker_role", "--nocapture"])
            .env(FORKER_DIR, dir.path())
            .current_dir(dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().unwrap();
        assert!(
            eventually(GONE_WITHIN, || dir.join("escaped").exists()),
            "round {round}: the forker never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
        let t0 = Instant::now();

        child.kill_tree();

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "round {round}: kill_tree took {:?}",
            t0.elapsed()
        );
    }
}

/// A kill right after the spawn, before the program could set anything
/// up, ends it at once.
#[test]
fn kill_tree_right_after_the_spawn_ends_it_at_once() {
    for round in 0..5 {
        let dir = TempDir::new(&format!("child-early-{round}"));
        let mut cmd = child::command("sleep");
        cmd.arg("300").current_dir(dir.path());
        let mut child = cmd.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(round));
        let t0 = Instant::now();

        child.kill_tree();

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "round {round}: kill_tree took {:?}",
            t0.elapsed()
        );
        assert_eq!(processes_in(dir.path()), 0, "round {round}");
    }
}

/// A child that already exited, and left nothing, is only reaped.
#[test]
fn kill_tree_on_an_exited_child_reaps_it() {
    let mut child = child::command("true").spawn().unwrap();
    assert!(eventually(GONE_WITHIN, || child
        .try_wait()
        .unwrap()
        .is_some()));

    child.kill_tree();

    assert!(child.wait().unwrap().success());
}

/// A child dropped running (its owner returned early or unwound) is
/// killed, its escaped grandchild with it, and its cgroup removed.
#[test]
fn dropping_a_running_child_kills_it() {
    support::private_cgroup("child-drop");
    let dir = TempDir::new("child-drop");
    let child = spawn_in(dir.path(), ESCAPING);
    let cgroup = cgroup_of(child.id());

    drop(child);

    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the dropped child",
        processes_in(dir.path())
    );
    assert!(!cgroup.exists(), "{} left", cgroup.display());
}

/// A leftover that traps SIGTERM to remove `lock`, detached from its
/// command, the way an ssh master or a helper daemon is: a command of the
/// daemon's own leaves it running and exits 0.
const LEAVES_A_HELPER: &str = "setsid sh -c 'trap \"rm -f lock; exit 0\" TERM; touch escaped; \
                               while :; do sleep 0.1; done' & \
                               while [ ! -e escaped ]; do sleep 0.01; done; exit 0";

/// The daemon's own commands are not ended one by one: what a git
/// command leaves running (an ssh `ControlPersist` master, a helper
/// daemon) runs on beside its unit's other work, and goes, with its
/// SIGTERM and grace, when the unit ends.
#[test]
fn what_a_finished_command_left_runs_until_its_unit_ends() {
    support::private_cgroup("child-own-tool-leftover");
    let dir = TempDir::new("child-own-tool-leftover");
    std::fs::write(dir.join("lock"), "").unwrap();
    let unit = child::Unit::new("cycle").unwrap();
    let cgroup = own_unit_dir(&unit);
    let (rc, _) = {
        let _in = unit.enter();
        hunter::util::run_cmd_in(&["sh", "-c", LEAVES_A_HELPER], Some(dir.path()), 30)
    };

    assert_eq!(rc, 0);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        processes_in(dir.path()) > 0,
        "the command's leftover was ended with it"
    );
    unit.end();
    assert!(
        !dir.join("lock").exists(),
        "the unit's end gave no SIGTERM grace"
    );
    assert_eq!(processes_in(dir.path()), 0);
    assert!(!cgroup.exists(), "{} left", cgroup.display());
}

/// The cgroup of `unit`.
fn own_unit_dir(unit: &child::Unit) -> PathBuf {
    child::units_root().unwrap().join(unit.name())
}

/// Dropped after its program ended, a child leaves what the program left
/// to its unit, like a command of the daemon's own.
#[test]
fn dropping_a_child_whose_program_ended_leaves_what_it_left_to_its_unit() {
    let dir = TempDir::new("child-drop-leftover");
    let unit = child::Unit::new("cycle").unwrap();
    let _in = unit.enter();
    let mut child = spawn_in(dir.path(), "setsid sleep 300 & touch escaped; exit 0");
    assert!(child.wait().unwrap().success());

    drop(child);

    assert!(
        processes_in(dir.path()) > 0,
        "the leftover was ended with the child"
    );
    unit.end();
    assert_eq!(processes_in(dir.path()), 0, "the unit's end left it");
}

/// A unit whose last handle is dropped without its end (its work
/// unwound) kills what is left in it.
#[test]
fn a_unit_dropped_unended_kills_what_is_left_in_it() {
    support::private_cgroup("child-unit-dropped");
    let dir = TempDir::new("child-unit-dropped");
    let unit = child::Unit::new("admin-test").unwrap();
    let cgroup = own_unit_dir(&unit);
    {
        let _in = unit.enter();
        let (rc, _) =
            hunter::util::run_cmd_in(&["sh", "-c", LEAVES_A_HELPER], Some(dir.path()), 30);
        assert_eq!(rc, 0);
    }

    drop(unit);

    assert_eq!(
        processes_in(dir.path()),
        0,
        "the dropped unit left its processes"
    );
    assert!(!cgroup.exists(), "{} left", cgroup.display());
}

/// Async work in a unit ([`child::in_unit`]) starts what its blocking
/// tasks start in that unit, and the unit ends with the work: what the
/// work left is gone once `in_unit` returns.
#[tokio::test]
async fn work_in_a_unit_starts_its_processes_there_and_ends_them_with_it() {
    let dir = TempDir::new("child-in-unit");
    std::fs::write(dir.join("lock"), "").unwrap();
    let work = dir.path().to_owned();

    let rc = child::in_unit("cycle", async move {
        child::spawn_blocking(move || {
            hunter::util::run_cmd_in(
                &[
                    "sh",
                    "-c",
                    &format!("cat /proc/self/cgroup > cg; {LEAVES_A_HELPER}"),
                ],
                Some(&work),
                30,
            )
            .0
        })
        .await
        .unwrap()
    })
    .await;

    assert_eq!(rc, 0);
    let cg = std::fs::read_to_string(dir.join("cg")).unwrap();
    assert!(cg.contains("/cycle-"), "started outside the unit: {cg}");
    assert_eq!(
        processes_in(dir.path()),
        0,
        "the unit's end left its leftover"
    );
    assert!(
        !dir.join("lock").exists(),
        "the leftover got no SIGTERM grace"
    );
}

/// Every process started finds the ssh wrapper first on its `PATH`, so
/// none of the daemon's git uses the operator's ssh master or becomes
/// one, which the end of its unit would cut under every session sharing
/// it.
#[test]
fn a_commands_git_runs_ssh_without_connection_sharing() {
    let bins = support::FakeBins::acquire("child-ssh");
    let _own = bins.unset_env("GIT_SSH_COMMAND");
    let _own_ssh = bins.unset_env("GIT_SSH");

    let argv = ssh_argv(&bins);

    assert!(has(&argv, ["-o", "ControlPath=none"]), "{argv:?}");
}

/// An ssh command chosen in git's config (`core.sshCommand`, here with a
/// key of its own) keeps its options, and still shares no connection:
/// the wrapper is what its `ssh` finds.
#[test]
fn a_configured_ssh_command_keeps_its_key_and_shares_no_connection() {
    let bins = support::FakeBins::acquire("child-ssh-config");
    let _own = bins.unset_env("GIT_SSH_COMMAND");
    let _own_ssh = bins.unset_env("GIT_SSH");
    let home = TempDir::new("child-ssh-config-home");
    std::fs::write(
        home.join(".gitconfig"),
        "[core]\n\tsshCommand = ssh -i /tmp/x\n",
    )
    .unwrap();
    let _home = bins.env("HOME", home.path());

    let argv = ssh_argv(&bins);

    assert!(has(&argv, ["-i", "/tmp/x"]), "{argv:?}");
    assert!(has(&argv, ["-o", "ControlPath=none"]), "{argv:?}");
}

/// An operator who exported their own `GIT_SSH_COMMAND` for the daemon
/// keeps it, untouched; its `ssh` is the wrapper as well.
#[test]
fn an_operators_git_ssh_command_is_passed_on() {
    let bins = support::FakeBins::acquire("child-ssh-operator");
    let _own = bins.env("GIT_SSH_COMMAND", Path::new("ssh -o BatchMode=yes"));

    let argv = ssh_argv(&bins);

    assert!(has(&argv, ["-o", "BatchMode=yes"]), "{argv:?}");
    assert!(has(&argv, ["-o", "ControlPath=none"]), "{argv:?}");
}

/// Whether `pair` comes, in that order, somewhere in `argv`.
fn has(argv: &[String], pair: [&str; 2]) -> bool {
    argv.windows(2).any(|w| w == pair)
}

/// The argv `git ls-remote` over ssh ran ssh with, through a fake `ssh`
/// that records it and fails (no network), with the ssh wrapper written
/// over it.
fn ssh_argv(bins: &support::FakeBins) -> Vec<String> {
    bins.script("ssh", "exit 1");
    let work = TempDir::new("child-ssh-work");
    child::install_ssh_wrapper(work.path());
    let _ = hunter::util::run_cmd(&["git", "ls-remote", "git@example.org:o/r.git"], 30);
    let calls = bins.calls_to("ssh");
    assert_eq!(calls.len(), 1, "git ran ssh once: {calls:?}");
    calls[0].clone()
}

/// Cutting the leash kills the child there and then, escaped grandchild
/// included, though another thread owns it.
#[test]
fn cutting_the_leash_kills_the_child() {
    let dir = TempDir::new("child-leash");
    let leash = Leash::default();
    let mut cmd = child::command("sh");
    cmd.args(["-c", ESCAPING]).current_dir(dir.path());
    let mut child = cmd.spawn_leashed(&leash).unwrap();
    assert!(eventually(GONE_WITHIN, || dir.join("escaped").exists()));

    assert!(leash.cut(), "there was a child to kill");

    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the cut",
        processes_in(dir.path())
    );
    assert!(child.wait().is_ok(), "the owner still reaps it");
}

/// A leash cut before its child was spawned (the owner stopped waiting
/// first) kills the child as soon as it is.
#[test]
fn a_child_on_a_cut_leash_is_killed_at_once() {
    let dir = TempDir::new("child-cut-first");
    let leash = Leash::default();
    assert!(!leash.cut(), "nothing leashed yet");
    let mut cmd = child::command("sleep");
    cmd.arg("300").current_dir(dir.path());

    let mut child = cmd.spawn_leashed(&leash).unwrap();

    assert!(
        eventually(GONE_WITHIN, || child.try_wait().unwrap().is_some()),
        "a child on a cut leash ran on"
    );
    assert_eq!(processes_in(dir.path()), 0);
}

/// A leash has nothing to kill once its child's program ended and left
/// nothing (the cgroup is still there, empty, once the kernel has taken
/// the reaped program out of it), nor once the child's cgroup is removed.
#[test]
fn a_leash_on_an_ended_child_kills_nothing() {
    let leash = Leash::default();
    let mut child = child::command("true").spawn_leashed(&leash).unwrap();
    assert!(child.wait().unwrap().success());

    assert!(
        eventually(GONE_WITHIN, || !leash.cut()),
        "the cut keeps finding something in an empty cgroup"
    );
    child.kill_tree();
    assert!(!leash.cut(), "the cut found something in a removed cgroup");
}

/// Once its child is dropped, a leash lets go of the child's cgroup: what
/// the program left there is its unit's to end, with its SIGTERM and
/// grace, not the leash's to SIGKILL.
#[test]
fn a_leash_lets_go_of_a_dropped_childs_cgroup() {
    let dir = TempDir::new("child-leash-dropped");
    let unit = child::Unit::new("cycle").unwrap();
    let _in = unit.enter();
    let leash = Leash::default();
    let mut cmd = child::command("sh");
    cmd.args(["-c", "setsid sleep 300 & touch escaped; exit 0"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn_leashed(&leash).unwrap();
    assert!(eventually(GONE_WITHIN, || dir.join("escaped").exists()));
    assert!(child.wait().unwrap().success());

    drop(child);

    assert!(!leash.cut(), "the leash killed what the dropped child left");
    assert!(
        processes_in(dir.path()) > 0,
        "the leftover was ended before its unit"
    );
    unit.end();
    assert_eq!(processes_in(dir.path()), 0, "the unit's end left it");
}

/// A command whose environment was cleared runs with the `PATH` its
/// caller gave it, which is none: only the ssh wrapper goes in front,
/// none of the daemon's own `PATH` behind it.
#[test]
fn a_cleared_environment_gets_no_path_but_the_ssh_wrappers() {
    let work = TempDir::new("child-cleared-work");
    child::install_ssh_wrapper(work.path());
    let mut cmd = child::command("/bin/sh");
    cmd.env_clear()
        .args(["-c", "printf %s \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().unwrap();
    let mut path = String::new();
    std::io::Read::read_to_string(&mut child.take_stdout().unwrap(), &mut path).unwrap();
    assert!(child.wait().unwrap().success());

    assert_eq!(Path::new(&path), work.join("bin"));
}

/// [`child::current_unit`] names the cgroup of the unit a process started
/// here goes into (the log ties a job to it by that name); outside any
/// unit, none.
#[test]
fn current_unit_names_the_cgroup_a_process_starts_in() {
    assert_eq!(child::current_unit(), None, "outside any unit");
    let dir = TempDir::new("child-current-unit");
    let unit = child::Unit::new("cycle").unwrap();
    let _in = unit.enter();

    let mut child = start_sh(dir.path(), "sleep 300");

    let named = child::current_unit().expect("in a unit");
    let started_in = cgroup_of(child.id());
    assert_eq!(
        started_in.parent().and_then(Path::file_name),
        Some(std::ffi::OsStr::new(&named)),
        "{} is not in {named}",
        started_in.display()
    );
    child.kill_tree();
    unit.end();
}

/// Where [`spawner_role`] runs its child; set only when this test binary
/// is re-executed as that spawner.
const SPAWNER_DIR: &str = "HUNTER_CHILD_TEST_SPAWNER_DIR";

/// Set to "1" for [`spawner_role`] to start its child with
/// [`child::Command::die_with_spawner`].
const SPAWNER_TIES: &str = "HUNTER_CHILD_TEST_SPAWNER_TIES";

/// A program that touches `termed` and exits on SIGTERM.
const RECORDS_TERM: &str =
    "trap 'touch termed; exit 0' TERM; touch escaped; while :; do sleep 0.05; done";

/// The spawner [`spawned_children_die_with_their_spawner_only_if_told`]
/// re-executes this binary as: it starts [`RECORDS_TERM`], then SIGKILLs
/// itself, as a daemon killed outright. As an ordinary test it does
/// nothing.
#[test]
fn spawner_role() {
    let Some(dir) = std::env::var_os(SPAWNER_DIR) else {
        return;
    };
    let mut cmd = child::command("sh");
    cmd.args(["-c", RECORDS_TERM])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if std::env::var_os(SPAWNER_TIES).is_some_and(|v| v == "1") {
        cmd.die_with_spawner();
    }
    let _child = cmd.spawn().unwrap();
    assert!(eventually(GONE_WITHIN, || Path::new(&dir)
        .join("escaped")
        .exists()));
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGKILL).unwrap();
}

/// [`spawner_role`] run in `dir`, its child tied to it or not, once it has
/// killed itself; the spawner's `Child`, whose `kill_tree` ends what is
/// left.
fn killed_spawner(dir: &Path, ties: bool) -> Child {
    let mut cmd = child::command(std::env::current_exe().unwrap());
    cmd.args(["--exact", "spawner_role", "--nocapture"])
        .env(SPAWNER_DIR, dir)
        .env(SPAWNER_TIES, if ties { "1" } else { "0" })
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut spawner = cmd.spawn().unwrap();
    let status = spawner.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9),
        "the spawner was killed: {status:?}"
    );
    assert!(dir.join("escaped").exists(), "its child ran");
    spawner
}

/// A child started with [`child::Command::die_with_spawner`] gets SIGTERM
/// when its spawner is killed outright, and ends; one started without it
/// runs on, left to its cgroup.
#[test]
fn spawned_children_die_with_their_spawner_only_if_told() {
    // Each spawner's cgroup is ended before the asserts, so a failure
    // leaves nothing running.
    let tied = TempDir::new("child-spawner-tied");
    let mut spawner = killed_spawner(tied.path(), true);
    let termed = eventually(GONE_WITHIN, || tied.join("termed").exists());
    let ended = eventually(GONE_WITHIN, || processes_in(tied.path()) == 0);
    spawner.kill_tree();
    assert!(termed, "the tied child got no SIGTERM");
    assert!(ended, "the tied child ran on");

    let untied = TempDir::new("child-spawner-untied");
    let mut spawner = killed_spawner(untied.path(), false);
    std::thread::sleep(Duration::from_millis(500));
    let (termed, running) = (untied.join("termed").exists(), processes_in(untied.path()));
    spawner.kill_tree();
    assert!(!termed, "the untied child was ended");
    assert!(running > 0, "the untied child is gone");
    assert_eq!(processes_in(untied.path()), 0);
}

/// A process may make cgroups below its child's own (it owns them) and
/// move into one. It is still the child's: `kill_tree` sends it the
/// SIGTERM with the rest and the grace, and removes what was made below
/// the child's cgroup with it.
#[test]
fn kill_tree_ends_what_moved_into_a_cgroup_below_the_childs() {
    support::private_cgroup("child-nested");
    let dir = TempDir::new("child-nested");
    let mut child = spawn_in(
        dir.path(),
        r#"c=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
mkdir "$c/inner"
sh -c 'echo 0 > '"$c"'/inner/cgroup.procs; trap "touch cleaned; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while :; do sleep 0.1; done"#,
    );
    let cgroup = cgroup_of(child.id());
    assert!(
        members_of(&cgroup.join("inner")) > 0,
        "nothing moved below the child's cgroup"
    );
    let t0 = Instant::now();

    child.kill_tree();

    assert!(
        dir.join("cleaned").exists(),
        "the moved process got no SIGTERM"
    );
    assert!(t0.elapsed() < GRACE, "{:?}", t0.elapsed());
    assert!(!cgroup.exists(), "{} left", cgroup.display());
}

/// How many processes are in the cgroup `dir` itself.
fn members_of(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// A process's cgroup takes the next name no cgroup has yet: one an
/// earlier run left, or one made by hand, is skipped.
#[test]
fn a_childs_cgroup_skips_names_already_taken() {
    support::private_cgroup("child-taken");
    let root = child::units_root().unwrap();
    std::fs::create_dir_all(root.join("other/0-sleep")).unwrap();
    let mut cmd = child::command("sleep");
    cmd.arg("300");

    let mut child = cmd.spawn().unwrap();

    let cgroup = cgroup_of(child.id());
    child.kill_tree();
    std::fs::remove_dir(root.join("other/0-sleep")).unwrap();
    assert_eq!(cgroup, root.join("other/1-sleep"));
}

/// A process without a daemon's claim makes its cgroups in one of its
/// own, named by its pid, below the one it runs in: no other process
/// shares the names it makes and ends.
#[test]
fn a_process_without_a_claim_makes_its_cgroups_in_its_own() {
    let own = support::private_cgroup("child-process-root");

    let root = child::units_root().unwrap();

    assert_eq!(root, own.join(format!("process-{}", std::process::id())));
    let unit = child::Unit::new("cycle").unwrap();
    assert!(root.join(unit.name()).is_dir(), "{}", unit.name());
}

/// A program not on `PATH` fails to spawn, and leaves no cgroup behind.
#[test]
fn a_program_not_on_path_does_not_spawn() {
    support::private_cgroup("child-not-on-path");

    let spawned = child::command("hunter-no-such-program").spawn();

    let err = spawned.err().expect("no child");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    let left: Vec<_> = std::fs::read_dir(child::units_root().unwrap().join("other"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_type().unwrap().is_dir())
        .map(|e| e.path())
        .collect();
    assert!(left.is_empty(), "left {left:?}");
}

/// Without a cgroup it may make cgroups in, a spawn fails, and says how
/// to give hunter one, rather than starting the program outside.
#[test]
fn a_spawn_without_a_cgroup_to_manage_fails_clearly() {
    use std::os::unix::fs::PermissionsExt as _;
    let own = support::private_cgroup("child-no-cgroup");
    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o555)).unwrap();

    let spawned = child::command("true").spawn();

    std::fs::set_permissions(&own, std::fs::Permissions::from_mode(0o755)).unwrap();
    let err = spawned.err().expect("no child");
    assert!(err.to_string().contains("Delegate=yes"), "{err}");
}

/// Where [`owner_role`] keeps its work root.
const OWNER_WORK: &str = "HUNTER_CHILD_TEST_OWNER_WORK";

/// The owner [`a_killed_owner_leaves_its_children_and_the_next_start_refuses`]
/// re-executes this binary as: a daemon that claims its cgroup, starts a
/// child and SIGKILLs itself, so nothing of it runs to clean up. As an
/// ordinary test it does nothing.
#[test]
fn owner_role() {
    let (Some(dir), Some(work)) = (std::env::var_os(OWNER_DIR), std::env::var_os(OWNER_WORK))
    else {
        return;
    };
    let _claimed = child::start(Path::new(&work)).unwrap();
    let _child = spawn_in(Path::new(&dir), ESCAPING);
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGKILL).unwrap();
}

/// A daemon killed outright ends nothing (systemd's unit kill does, run as
/// a service): what it started runs on in its cgroup. The next daemon in
/// the same cgroup does not start beside it, and says which cgroup and
/// which processes; once they are ended, it starts, and removes the
/// emptied cgroup.
#[test]
fn a_killed_owner_leaves_its_children_and_the_next_start_refuses() {
    use std::os::unix::process::ExitStatusExt as _;
    let cgroup = support::private_cgroup("child-owner");
    let dir = TempDir::new("child-owner");
    let work = TempDir::new("child-owner-work");

    let owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "owner_role", "--nocapture"])
        .env(OWNER_DIR, dir.path())
        .env(OWNER_WORK, work.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(owner.signal(), Some(9), "the owner was killed: {owner:?}");
    assert_eq!(processes_in(dir.path()), 2, "the child and its grandchild");
    let left = cgroup.join("other");

    let refused = child::start(work.path()).expect_err("started beside the leftover");

    std::fs::write(left.join("cgroup.kill"), "1").unwrap();
    let said = refused.to_string();
    assert!(said.contains(&left.display().to_string()), "{said}");
    assert!(said.contains("sleep 300"), "{said}");
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "the leftover outlived its cgroup.kill"
    );
    let _claimed = child::start(work.path()).unwrap();
    assert!(!left.exists(), "the emptied leftover was not removed");
    assert_eq!(
        cgroup_of(std::process::id()),
        cgroup.join("daemon"),
        "the daemon moved into its leaf"
    );
}

/// A daemon started from somewhere else -- another terminal's scope --
/// still finds what the last daemon on the same work root left in the
/// cgroup it claimed, which the work root records: it does not start
/// beside it, so recovery's "no worker of the dead process runs" holds.
#[test]
fn a_daemon_started_elsewhere_still_finds_what_the_last_one_left() {
    let shared = child::own_cgroup().unwrap();
    let first = support::private_cgroup("child-remembered-a");
    let work = TempDir::new("child-remembered-work");
    let dir = TempDir::new("child-remembered");
    let claimed = child::start(work.path()).unwrap();
    let (rc, _) = hunter::util::run_cmd_in(&["sh", "-c", LEAVES_A_HELPER], Some(dir.path()), 30);
    assert_eq!(rc, 0);
    // The first daemon is killed: its claim ends nothing.
    std::mem::forget(claimed);
    let second = shared.join(format!("test-child-remembered-b-{}", std::process::id()));
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("cgroup.procs"), "0").unwrap();
    let left = first.join("other");

    let refused = child::start(work.path()).expect_err("started beside the leftover");

    std::fs::write(left.join("cgroup.kill"), "1").unwrap();
    let said = refused.to_string();
    assert!(said.contains(&left.display().to_string()), "{said}");
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "the leftover outlived its cgroup.kill"
    );
    let _claimed = child::start(work.path()).unwrap();
    assert!(!left.exists(), "the emptied leftover was not removed");
    let recorded = std::fs::read_to_string(work.join("cgroup")).unwrap();
    assert_eq!(
        recorded.lines().next(),
        Some(second.display().to_string().as_str()),
        "the work root records the cgroup claimed last"
    );
}

/// A daemon started elsewhere on the same work root, after one that died
/// left only idle cgroups, removes them and takes the dead daemon's claim
/// off its cgroup: nothing there is its any more.
#[test]
fn a_start_elsewhere_takes_a_dead_claim_off_its_idle_cgroup() {
    let shared = child::own_cgroup().unwrap();
    let old = support::private_cgroup("child-dead-claim-a");
    let work = TempDir::new("child-dead-claim-work");
    // The first daemon is killed, leaving an idle unit.
    std::mem::forget(child::start(work.path()).unwrap());
    let unit = child::Unit::new("cycle").unwrap();
    let left = old.join(unit.name());
    std::mem::forget(unit);
    assert!(
        claim_mark(&old).is_some(),
        "the old cgroup carries the claim"
    );
    let new = shared.join(format!("test-child-dead-claim-b-{}", std::process::id()));
    std::fs::create_dir(&new).unwrap();
    std::fs::write(new.join("cgroup.procs"), "0").unwrap();

    let _claimed = child::start(work.path()).unwrap();

    assert!(!left.exists(), "the idle leftover was not removed");
    assert_eq!(
        claim_mark(&old),
        None,
        "the old cgroup still carries the claim"
    );
}

/// The claim the cgroup `dir` carries (`user.hunter.claim`), if any.
fn claim_mark(dir: &Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).unwrap();
    let mut value = vec![0_u8; 4096];
    // SAFETY: both names are NUL-terminated strings that outlive the
    // call, and the buffer is as long as the size passed with it.
    #[allow(unsafe_code)]
    let len = unsafe {
        nix::libc::getxattr(
            path.as_ptr(),
            c"user.hunter.claim".as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    value.truncate(usize::try_from(len).ok()?);
    Some(String::from_utf8(value).unwrap())
}

/// A daemon that stops cleanly takes its claim back: off its cgroup and
/// out of the work root. A claim left behind is a daemon that died
/// without its stop.
#[test]
fn a_claim_ended_cleanly_leaves_no_record_of_itself() {
    let cgroup = support::private_cgroup("child-clean-stop");
    let work = TempDir::new("child-clean-stop-work");
    let claimed = child::start(work.path()).unwrap();
    let mark = claim_mark(&cgroup).expect("the cgroup carries the claim");
    let recorded = std::fs::read_to_string(work.join("cgroup")).unwrap();
    assert_eq!(
        recorded,
        format!("{}\n{mark}\n", cgroup.display()),
        "the work root records the cgroup and the claim"
    );
    let canonical = std::fs::canonicalize(work.path()).unwrap();
    assert!(
        mark.starts_with(&format!("{} ", std::process::id())),
        "{mark}"
    );
    assert!(
        mark.ends_with(&format!(" {}", canonical.display())),
        "{mark}"
    );

    drop(claimed);

    assert!(
        !work.join("cgroup").exists(),
        "the work root still records the claim"
    );
    assert_eq!(claim_mark(&cgroup), None, "the cgroup still carries it");
}

/// The cgroup the work root records was claimed since by a daemon on
/// another work root (a scope name reused after a clean stop, or after
/// the unit went): what runs there is that daemon's live work, not an
/// earlier run's. A daemon started elsewhere on the first work root
/// leaves it alone, and starts.
#[test]
fn a_recorded_cgroup_claimed_since_for_another_work_root_is_left_alone() {
    let reused = support::private_cgroup("child-reclaimed");
    let (first, other) = (
        TempDir::new("child-reclaimed-first"),
        TempDir::new("child-reclaimed-other"),
    );
    // A daemon on the first work root, gone without taking its record
    // back; then one on the other work root in the same cgroup, at work.
    // (Each claim moves this process into the cgroup's `daemon/`; back
    // out for the next.)
    std::mem::forget(child::start(first.path()).unwrap());
    std::fs::write(reused.join("cgroup.procs"), "0").unwrap();
    let _other = child::start(other.path()).unwrap();
    let dir = TempDir::new("child-reclaimed-work");
    let unit = child::Unit::new("cycle").unwrap();
    let mut work = {
        let _in = unit.enter();
        start_sh(dir.path(), "touch escaped; sleep 300")
    };
    assert!(eventually(GONE_WITHIN, || dir.join("escaped").exists()));
    let elsewhere = child::own_cgroup()
        .unwrap()
        .join(format!("test-child-elsewhere-{}", std::process::id()));
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("cgroup.procs"), "0").unwrap();
    let started = child::start(first.path());

    let running = processes_in(dir.path());
    let mark = claim_mark(&reused);
    work.kill_tree();
    unit.end();
    let _claimed = started.expect("refused over another work root's live work");
    assert!(running > 0, "the other daemon's work was ended");
    assert!(
        mark.is_some_and(|m| m.ends_with(&format!(
            " {}",
            std::fs::canonicalize(other.path()).unwrap().display()
        ))),
        "the other daemon's claim was taken off its cgroup"
    );
}

/// The daemon's claim, dropped when the daemon is done, ends every unit
/// still in its cgroup, with its SIGTERM and grace: what runs outside the
/// job and admin units (`other`) included.
#[test]
fn the_daemons_claim_ends_what_its_work_left_when_dropped() {
    support::private_cgroup("child-claim");
    let work = TempDir::new("child-claim-work");
    let (outside, inside) = (
        TempDir::new("child-claim-other"),
        TempDir::new("child-claim-unit"),
    );
    for dir in [&outside, &inside] {
        std::fs::write(dir.join("lock"), "").unwrap();
    }
    let claimed = child::start(work.path()).unwrap();
    let unit = child::Unit::new("cycle").unwrap();
    for (dir, unit) in [(&outside, None), (&inside, Some(&unit))] {
        let _in = unit.map(child::Unit::enter);
        let (rc, _) =
            hunter::util::run_cmd_in(&["sh", "-c", LEAVES_A_HELPER], Some(dir.path()), 30);
        assert_eq!(rc, 0);
    }

    drop(claimed);

    for dir in [&outside, &inside] {
        assert!(
            !dir.join("lock").exists(),
            "no SIGTERM grace in {}",
            dir.path().display()
        );
        assert_eq!(processes_in(dir.path()), 0);
    }
}

/// Another process in the daemon's leaf is another daemon's, or what one
/// left: the daemon does not start beside it.
#[test]
fn start_refuses_beside_another_process_in_the_daemons_leaf() {
    let cgroup = support::private_cgroup("child-other-daemon");
    let work = TempDir::new("child-other-daemon-work");
    let leaf = cgroup.join("daemon");
    std::fs::create_dir(&leaf).unwrap();
    let mut other = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    std::fs::write(leaf.join("cgroup.procs"), other.id().to_string()).unwrap();

    let refused = child::start(work.path()).map(drop);

    other.kill().unwrap();
    other.wait().unwrap();
    let said = refused
        .expect_err("started beside another daemon")
        .to_string();
    assert!(said.contains(&format!("pid {}", other.id())), "{said}");
}

/// A refused start removes nothing, not even an empty cgroup beside the
/// busy one it refuses for: it decides on all of them first.
#[test]
fn a_refused_start_removes_nothing() {
    let cgroup = support::private_cgroup("child-refused-intact");
    let work = TempDir::new("child-refused-intact-work");
    let (empty, busy) = (cgroup.join("cycle-0"), cgroup.join("cycle-1"));
    std::fs::create_dir(&empty).unwrap();
    std::fs::create_dir(&busy).unwrap();
    let mut other = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    std::fs::write(busy.join("cgroup.procs"), other.id().to_string()).unwrap();

    let refused = child::start(work.path()).map(drop);

    other.kill().unwrap();
    other.wait().unwrap();
    assert!(refused.is_err(), "started beside a busy leftover");
    assert!(
        empty.is_dir(),
        "the refused start removed {}",
        empty.display()
    );
}

/// A cgroup this process may not manage is refused at startup, with how
/// to give hunter one.
#[test]
fn start_refuses_a_cgroup_it_may_not_manage() {
    use std::os::unix::fs::PermissionsExt as _;
    let cgroup = support::private_cgroup("child-unmanaged");
    let work = TempDir::new("child-unmanaged-work");
    let procs = cgroup.join("cgroup.procs");
    std::fs::set_permissions(&procs, std::fs::Permissions::from_mode(0o444)).unwrap();

    let refused = child::start(work.path()).map(drop);

    std::fs::set_permissions(&procs, std::fs::Permissions::from_mode(0o644)).unwrap();
    let said = refused.expect_err("started unmanaged").to_string();
    assert!(said.contains("not writable"), "{said}");
    assert!(said.contains("Delegate=yes"), "{said}");
}

/// A fake `systemctl` that answers `--user` with `user` and `--system`
/// with `system`, each a shell snippet; the verdict for `cgroup`.
fn delegation_with(label: &str, cgroup: &str, user: &str, system: &str) -> child::Delegated {
    let bins = support::FakeBins::acquire(label);
    bins.script(
        "systemctl",
        &format!("case \"$1\" in\n--user) {user} ;;\n--system) {system} ;;\nesac"),
    );
    child::delegation(Path::new(cgroup))
}

/// The cgroup the systemctl answers below are about.
const UNIT_CGROUP: &str = "/sys/fs/cgroup/user.slice/app.slice/hunter.service";

/// What a manager prints for the unit of [`UNIT_CGROUP`], `Delegate=` as
/// given.
fn shows(delegate: &str) -> String {
    format!("printf 'Delegate={delegate}\\nControlGroup=/user.slice/app.slice/hunter.service\\n'")
}

/// A cgroup not named like a systemd unit's (a directory root made and
/// handed over) is no unit's: systemd is not asked.
#[test]
fn a_cgroup_not_named_like_a_unit_is_no_units() {
    let bins = support::FakeBins::acquire("delegation-not-a-unit");
    bins.script("systemctl", "exit 1");

    let verdict = child::delegation(Path::new("/sys/fs/cgroup/hunter"));

    assert_eq!(verdict, child::Delegated::NotAUnit);
    assert_eq!(bins.calls_to("systemctl"), Vec::<Vec<String>>::new());
}

/// The user manager's answer decides, for a service and a scope alike.
#[test]
fn the_user_managers_delegate_decides() {
    let yes = delegation_with("delegation-yes", UNIT_CGROUP, &shows("yes"), "exit 1");
    let scope = UNIT_CGROUP.replace(".service", ".scope");
    let no = delegation_with(
        "delegation-no",
        &scope,
        &shows("no").replace(".service", ".scope"),
        "exit 1",
    );

    assert_eq!(yes, child::Delegated::Yes);
    assert_eq!(no, child::Delegated::No("hunter.scope".to_owned()));
}

/// A manager that names another cgroup as the unit's (a unit of the same
/// name elsewhere) does not answer for this one: the system manager's
/// answer counts then.
#[test]
fn a_manager_answering_for_another_cgroup_does_not_count() {
    let elsewhere = "printf 'Delegate=no\\nControlGroup=/elsewhere/hunter.service\\n'";

    let verdict = delegation_with("delegation-system", UNIT_CGROUP, elsewhere, &shows("yes"));

    assert_eq!(verdict, child::Delegated::Yes);
}

/// A manager whose `systemctl` fails does not answer, whatever it
/// printed.
#[test]
fn a_failing_systemctl_does_not_answer() {
    let failing = format!("{}; exit 1", shows("yes"));

    let verdict = delegation_with("delegation-failing", UNIT_CGROUP, &failing, &shows("no"));

    assert_eq!(verdict, child::Delegated::No("hunter.service".to_owned()));
}

/// With no manager answering for it, delegation is unknown.
#[test]
fn no_manager_answering_leaves_delegation_unknown() {
    let verdict = delegation_with("delegation-unknown", UNIT_CGROUP, "exit 1", "exit 1");

    assert_eq!(verdict, child::Delegated::Unknown);
}

/// Only a delegated unit goes without a warning.
#[test]
fn every_verdict_but_yes_warns() {
    assert_eq!(child::Delegated::Yes.warning(), None);
    for verdict in [
        child::Delegated::No("hunter.service".to_owned()),
        child::Delegated::NotAUnit,
        child::Delegated::Unknown,
    ] {
        let warning = verdict.warning().expect("a warning");
        assert!(warning.contains("Delegate=yes"), "{warning}");
    }
    assert!(
        child::Delegated::No("hunter.service".to_owned())
            .warning()
            .unwrap()
            .contains("hunter.service")
    );
}
