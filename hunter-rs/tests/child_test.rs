#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `hunter::child`: no process the daemon starts outlives it, nor the
//! kill that was meant to stop it, nor the owner that stopped waiting.
//!
//! The children here leave a grandchild in a session of its own, as a
//! `setsid` or a double fork does: out of the process group, so a group
//! kill alone never reaches it. Processes are found by their working
//! directory (`support::processes_in`), since one in a PID namespace has
//! no pid that means anything out here.

mod support;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use hunter::child::{self, Child, Leash};
use support::{TempDir, eventually, processes_in};

/// A child that leaves a grandchild in its own session, which writes
/// `escaped` once it runs, then waits for good.
const ESCAPING: &str = "setsid sh -c 'touch escaped; exec sleep 300' & exec sleep 300";

/// Long enough for any process the kernel has been told to kill to be
/// gone; far shorter than the children's own 300 s.
const GONE_WITHIN: Duration = Duration::from_secs(5);

/// What a program's leftovers get to exit in once it ended (the reaper's
/// grace, `child::TERM_GRACE`).
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
    cmd.spawn().expect("spawn through bwrap")
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

/// The owner [`a_killed_owner_takes_its_children_with_it`] re-executes
/// this binary as: it starts a child and SIGKILLs itself, so nothing of
/// it runs to clean up. As an ordinary test it does nothing.
#[test]
fn owner_role() {
    let Some(dir) = std::env::var_os(OWNER_DIR) else {
        return;
    };
    let _child = spawn_in(Path::new(&dir), ESCAPING);
    nix::sys::signal::raise(nix::sys::signal::Signal::SIGKILL).unwrap();
}

/// The daemon's guarantee: an owner that dies however it dies, SIGKILL
/// included, leaves nothing it started running, not even a grandchild
/// that left its process group.
#[test]
fn a_killed_owner_takes_its_children_with_it() {
    use std::os::unix::process::ExitStatusExt as _;
    let dir = TempDir::new("child-owner");

    let owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "owner_role", "--nocapture"])
        .env(OWNER_DIR, dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();

    assert_eq!(owner.signal(), Some(9), "the owner was killed: {owner:?}");
    assert!(dir.join("escaped").exists(), "the owner's child had run");
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived their owner",
        processes_in(dir.path())
    );
}

/// The cap's kill: the whole tree goes, the escaped grandchild included,
/// and bwrap is reaped.
#[test]
fn kill_tree_ends_a_child_that_escaped_its_group() {
    let dir = TempDir::new("child-kill-tree");
    let mut child = spawn_in(dir.path(), ESCAPING);

    child.kill_tree();

    assert!(child.try_wait().unwrap().is_some(), "bwrap reaped");
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived kill_tree",
        processes_in(dir.path())
    );
}

/// The kill is SIGTERM first, and the program gets to use it: a worker's
/// git removes its lock files on SIGTERM. Signalling bwrap with it would
/// kill bwrap outright and the whole namespace with it, the handler
/// included.
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

/// The grace is for everything the program started, not only for the
/// program: a worker's git, under omp, removes its lock files on SIGTERM.
/// The program exiting first would end its namespace, and the descendant
/// cleaning up in it, at once. Here the descendant takes a second to
/// clean, the program none.
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
/// that has exited since) gets the grace for its cleanup too, though the
/// program dies on its SIGTERM at once.
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
/// is not sent a SIGTERM of its own: each process in the namespace gets
/// one, when the kill begins.
#[test]
fn kill_tree_lets_a_handler_start_its_cleanup() {
    let dir = TempDir::new("child-term-cleanup-command");
    let mut child = spawn_in(
        dir.path(),
        r#"trap 'sh -c "sleep 0.5; touch saved"; exit 0' TERM
touch escaped
while :; do sleep 0.1; done"#,
    );

    child.kill_tree();

    assert!(
        dir.join("saved").exists(),
        "the handler's cleanup was cut short"
    );
}

/// A program that exits leaves what it started a SIGTERM and the grace
/// to act on it, not the namespace's SIGKILL at once; the child's status
/// is still the program's.
#[test]
fn a_program_exiting_gives_what_it_left_its_sigterm_cleanup() {
    let dir = TempDir::new("child-exit-cleanup");
    std::fs::write(dir.join("lock"), "").unwrap();
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "sleep 0.5; rm -f lock; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while [ ! -e escaped ]; do sleep 0.01; done
exit 3"#,
    );

    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(3));
    assert!(
        !dir.join("lock").exists(),
        "the leftover's SIGTERM cleanup was cut short"
    );
}

/// `try_wait` reports the program's status the moment the program ends,
/// while what it left still runs in the namespace (and bwrap with it);
/// `kill_tree` then still gives that leftover its cleanup.
#[test]
fn try_wait_reports_the_program_ending_before_what_it_left() {
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
    assert!(
        dir.join("lock").exists(),
        "reported only once the leftover had finished"
    );
    child.kill_tree();
    assert!(
        !dir.join("lock").exists(),
        "the leftover's SIGTERM cleanup was cut short"
    );
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

/// The reaper broadcasts its SIGTERM once. A leftover whose handler
/// started a cleanup command after the broadcast at the program's exit
/// does not have that command killed by a second one when `kill_tree`
/// then tells the reaper to stop. (The handler resets SIGTERM first: run
/// again by a second broadcast it would start a fresh cleanup, which
/// would hide the first one's death.)
#[test]
fn kill_tree_after_the_program_exited_does_not_signal_the_namespace_again() {
    let dir = TempDir::new("child-term-once");
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "trap - TERM; sh -c \"sleep 1; touch saved\" & touch cleaning; wait; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while [ ! -e escaped ]; do sleep 0.01; done
exit 3"#,
    );
    assert!(
        eventually(GONE_WITHIN, || {
            matches!(child.try_wait(), Ok(Some(_))) && dir.join("cleaning").exists()
        }),
        "the leftover never started its cleanup"
    );

    child.kill_tree();

    assert!(
        dir.join("saved").exists(),
        "the cleanup command was killed by a second SIGTERM"
    );
}

/// An orphan the reaper adopted that dies of a real-time signal is reaped
/// like any other: the reaper goes on, and so does the program.
#[test]
fn an_orphan_dying_of_a_real_time_signal_does_not_end_the_program() {
    let dir = TempDir::new("child-rt-orphan");
    let mut child = start_sh(
        dir.path(),
        r#"(sh -c 'echo $$ > orphan.tmp; mv orphan.tmp orphan; exec sleep 300' &)
while [ ! -e orphan ]; do sleep 0.01; done
pid=$(cat orphan)
kill -40 "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 0.01; done
touch done
exit 7"#,
    );

    assert_eq!(child.wait().unwrap().code(), Some(7));
    assert!(dir.join("done").exists(), "the program was cut short");
}

/// A program that dies of a real-time signal reports 128 + its number, as
/// for any other signal.
#[test]
fn a_program_dying_of_a_real_time_signal_reports_it() {
    let dir = TempDir::new("child-rt-program");
    let mut child = start_sh(dir.path(), "kill -40 $$");

    let mut status = None;
    assert!(eventually(GONE_WITHIN, || {
        status = child.try_wait().unwrap();
        status.is_some()
    }));

    assert_eq!(status.and_then(|s| s.code()), Some(168));
    assert_eq!(child.wait().unwrap().code(), Some(168));
}

/// What a program left that ignores SIGTERM ends with the grace: `wait`
/// returns, with the program's status, about [`GRACE`] after the program
/// ended, and the leftover is gone.
#[test]
fn what_a_program_left_ignoring_sigterm_ends_with_the_grace() {
    let dir = TempDir::new("child-term-ignored");
    let mut child = spawn_in(
        dir.path(),
        r#"sh -c 'trap "" TERM; touch escaped; while :; do sleep 0.1; done' &
while [ ! -e escaped ]; do sleep 0.01; done
exit 4"#,
    );
    let group = child.id();

    let (done, waited) = std::sync::mpsc::channel();
    let started = std::time::Instant::now();
    std::thread::spawn(move || {
        let _ = done.send(child.wait().map(|s| s.code()));
    });
    let status = waited.recv_timeout(GRACE + GONE_WITHIN);
    if status.is_err() {
        // Not left running past the test: `group` is a live child's.
        let _ = std::process::Command::new("kill")
            .args(["-KILL", "--", &format!("-{group}")])
            .status();
    }

    assert_eq!(
        status.expect("the wait never ended").unwrap(),
        Some(4),
        "the program's status"
    );
    assert!(started.elapsed() + Duration::from_secs(1) >= GRACE);
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the grace",
        processes_in(dir.path())
    );
}

/// `try_wait` on a program still running answers at once, without
/// waiting for its status.
#[test]
fn try_wait_on_a_running_program_answers_at_once() {
    let dir = TempDir::new("child-try-wait-running");
    let mut child = start_sh(dir.path(), "sleep 3");

    let asked = std::time::Instant::now();
    let status = child.try_wait().unwrap();

    assert!(
        asked.elapsed() < Duration::from_secs(1),
        "{:?}",
        asked.elapsed()
    );
    assert!(status.is_none(), "{status:?}");
    child.kill_tree();
}

/// One of the daemon's own tools stops as a process group: every member
/// gets the SIGTERM and its grace, not only the leader.
#[test]
fn a_tools_kill_tree_gives_its_group_the_grace() {
    let dir = TempDir::new("child-tool-term");
    let mut cmd = child::tool("sh");
    cmd.args([
        "-c",
        r#"sh -c 'trap "sleep 0.5; touch cleaned; exit 0" TERM; touch escaped; while :; do sleep 0.1; done' &
while :; do sleep 0.1; done"#,
    ])
    .current_dir(dir.path());
    let mut child = cmd.spawn().unwrap();
    assert!(eventually(GONE_WITHIN, || dir.join("escaped").exists()));

    child.kill_tree();

    assert!(
        dir.join("cleaned").exists(),
        "the member's cleanup was cut short"
    );
    assert_eq!(processes_in(dir.path()), 0);
}

/// Not killed, a program that exits takes what it left running with it,
/// and its exit status is the child's, as without the reaper.
#[test]
fn a_program_exiting_on_its_own_takes_its_leftovers_with_it() {
    let dir = TempDir::new("child-exit");
    let mut child = spawn_in(dir.path(), "sleep 300 & touch escaped; exit 7");

    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(7));
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the program",
        processes_in(dir.path())
    );
}

/// A program killed by a signal exits the child with 128 + the signal,
/// as bwrap reports it.
#[test]
fn a_program_killed_by_a_signal_exits_with_128_plus_it() {
    let mut cmd = child::command("sh");
    cmd.args(["-c", "kill -KILL $$"]);

    let status = cmd.spawn().unwrap().wait().unwrap();

    assert_eq!(status.code(), Some(137));
}

/// `sh -c 'exit <code>'` through the module; the child's exit code.
fn exit_code_of(code: i32) -> Option<i32> {
    let mut cmd = child::command("sh");
    cmd.args(["-c", &format!("exit {code}")]);
    cmd.spawn().unwrap().wait().unwrap().code()
}

/// The binary every child runs under is the one this process started
/// from, whatever happens to its file since: removed (`cargo clean`) or
/// replaced (a build of another commit). By path, every later spawn would
/// exit 1 (bwrap finds nothing to exec) or with whatever the new file
/// does, read as the program's own exit code.
#[test]
fn children_run_under_the_binary_as_it_was_opened() {
    let bins = support::FakeBins::acquire("child-binary-replaced");
    let dir = TempDir::new("child-binary-replaced");
    let original = std::env::var_os("NEXTEST_BIN_EXE_hunter").unwrap();
    let copy = dir.join("hunter");
    std::fs::copy(&original, &copy).unwrap();
    let _reaper = bins.env("NEXTEST_BIN_EXE_hunter", &copy);
    assert_eq!(exit_code_of(0), Some(0));

    std::fs::remove_file(&copy).unwrap();
    let after_removal = exit_code_of(5);
    executable(&copy, "#!/bin/sh\nexit 2\n");
    let after_replacement = exit_code_of(5);

    assert_eq!(after_removal, Some(5), "after the binary was removed");
    assert_eq!(after_replacement, Some(5), "after the binary was replaced");
}

/// The descriptor the reaper is exec'd by is not the program's: the
/// reaper closes it before it starts the program.
#[test]
fn the_program_does_not_inherit_the_reapers_binary() {
    let hunter =
        std::fs::canonicalize(std::env::var_os("NEXTEST_BIN_EXE_hunter").unwrap()).unwrap();
    let mut cmd = child::command("sh");
    // `|| true`: the descriptor the glob read the directory with is gone
    // by the time the loop reaches it.
    cmd.args([
        "-c",
        "for fd in /proc/$$/fd/*; do readlink \"$fd\" || true; done",
    ])
    .stdout(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    let mut listed = String::new();
    std::io::Read::read_to_string(&mut child.take_stdout().unwrap(), &mut listed).unwrap();

    assert!(child.wait().unwrap().success());
    assert!(
        !listed.lines().any(|l| Path::new(l) == hunter),
        "the program holds {}: {listed}",
        hunter.display()
    );
}

/// The program runs `orphan` in a subshell that exits at once, so the
/// orphan is the reaper's to reap, waits until the reaper has reaped it
/// (a zombie still answers `kill -0`), then writes `done` and exits 7.
fn outlives_an_orphan(dir: &Path, orphan: &str) -> std::process::ExitStatus {
    let mut cmd = child::command("sh");
    cmd.args([
        "-c",
        &format!(
            "( sh -c '{orphan}' & echo $! > orphan )
while [ ! -s orphan ]; do sleep 0.01; done
while kill -0 \"$(cat orphan)\" 2>/dev/null; do sleep 0.01; done
touch done; exit 7"
        ),
    ])
    .current_dir(dir);
    cmd.spawn().unwrap().wait().unwrap()
}

/// An orphan dying of a signal is not the program dying of it: the reaper
/// goes on, and the child's status is the program's own.
#[test]
fn an_orphan_killed_by_a_signal_is_not_the_program() {
    let dir = TempDir::new("child-orphan-killed");

    let status = outlives_an_orphan(dir.path(), "kill -KILL $$");

    assert_eq!(status.code(), Some(7));
    assert!(dir.join("done").exists(), "the program was cut short");
}

/// Likewise an orphan exiting with a code of its own.
#[test]
fn an_orphan_exiting_is_not_the_program() {
    let dir = TempDir::new("child-orphan-exits");

    let status = outlives_an_orphan(dir.path(), "exit 3");

    assert_eq!(status.code(), Some(7));
    assert!(dir.join("done").exists(), "the program was cut short");
}

/// A kill that comes while the child is still starting -- bwrap setting
/// up, the reaper not yet running its program, the program not yet
/// exec'd -- ends it at once, not at the deadline. Each kill below lands
/// at another point of the start; any of them that missed would take
/// the whole 10 s grace.
#[test]
fn kill_tree_ends_a_child_still_starting_without_the_grace() {
    for round in 0..20 {
        let dir = TempDir::new(&format!("child-early-{round}"));
        let mut cmd = child::command("sleep");
        cmd.arg("300").current_dir(dir.path());
        let mut child = cmd.spawn().unwrap();
        std::thread::sleep(Duration::from_millis(round));
        let t0 = std::time::Instant::now();

        child.kill_tree();

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "round {round}: kill_tree took {:?}",
            t0.elapsed()
        );
        assert!(
            eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
            "round {round}: {} process(es) outlived kill_tree",
            processes_in(dir.path())
        );
    }
}

/// A process forked while the kill is under way -- after the SIGTERM has
/// passed its parent -- gets one too, rather than outliving the program
/// until the deadline. The program forks one every few milliseconds, so
/// the kill meets a fork in progress.
#[test]
fn kill_tree_reaches_a_process_forked_during_the_kill() {
    for round in 0..10 {
        let dir = TempDir::new(&format!("child-forking-{round}"));
        let mut child = spawn_in(
            dir.path(),
            "touch escaped; while :; do sleep 300 & sleep 0.002; done",
        );
        std::thread::sleep(Duration::from_millis(100));
        let t0 = std::time::Instant::now();

        child.kill_tree();

        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "round {round}: kill_tree took {:?}",
            t0.elapsed()
        );
    }
}

/// A child dropped unreaped (its owner returned early or unwound) is
/// killed, its escaped grandchild with it.
#[test]
fn dropping_a_running_child_kills_it() {
    let dir = TempDir::new("child-drop");
    let child = spawn_in(dir.path(), ESCAPING);

    drop(child);

    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the dropped child",
        processes_in(dir.path())
    );
}

/// `--die-with-parent` fires when the spawning *thread* exits, and the
/// child is armed against that thread before it execs bwrap. Spawns are
/// made on one thread that lives as long as the process, so a child
/// spawned for a thread that exits right after asking keeps running.
#[test]
fn a_child_outlives_the_thread_that_asked_for_it() {
    let mut child = std::thread::spawn(|| {
        let mut cmd = child::command("sleep");
        cmd.arg("300");
        cmd.spawn().unwrap()
    })
    .join()
    .unwrap();

    std::thread::sleep(Duration::from_millis(500));

    assert!(
        child.try_wait().unwrap().is_none(),
        "killed when the thread that asked for it exited"
    );
    child.kill_tree();
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
/// first) kills the child as soon as it is: while bwrap is still setting
/// up, so the namespace's init it already cloned goes too, not only
/// bwrap.
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
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the cut",
        processes_in(dir.path())
    );
}

/// A child dropped the moment it is spawned, while bwrap still sets up,
/// takes the namespace's init with it too.
#[test]
fn dropping_a_child_mid_setup_leaves_nothing_running() {
    let dir = TempDir::new("child-drop-early");
    let mut cmd = child::command("sleep");
    cmd.arg("300").current_dir(dir.path());

    drop(cmd.spawn().unwrap());

    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived the dropped child",
        processes_in(dir.path())
    );
}

/// The startup probe passes where bwrap can set up its namespaces.
#[test]
fn the_probe_passes_with_a_working_bwrap() {
    child::check().unwrap();
}

/// Where `bwrap` is set when [`a_bwrap_that_cannot_start_anything_is_refused`]
/// re-executes this binary to probe it.
const PROBE_ROLE: &str = "HUNTER_CHILD_TEST_PROBE";

/// The probe [`a_bwrap_that_cannot_start_anything_is_refused`] runs in a
/// fresh process, whose one lookup of `bwrap` finds the fake. Prints the
/// refusal. As an ordinary test it does nothing.
#[test]
fn probe_role() {
    if std::env::var_os(PROBE_ROLE).is_none() {
        return;
    }
    let refused = child::check().expect_err("the probe passed");
    println!("refused: {refused}");
}

/// A bwrap that cannot create its namespaces (user namespaces restricted
/// by apparmor(7), a container) still starts, and exits 1 for every program.
/// The probe refuses it, with what bwrap said and the likely causes,
/// rather than letting every spawn's exit 1 pass for the program's.
#[test]
fn a_bwrap_that_cannot_start_anything_is_refused() {
    let fakes = TempDir::new("child-probe-bwrap");
    executable(
        &fakes.join("bwrap"),
        "#!/bin/sh\necho 'bwrap: setting up uid map: Permission denied' >&2\nexit 1\n",
    );
    let path = format!(
        "{}:{}",
        fakes.path().display(),
        std::env::var("PATH").unwrap()
    );

    let probe = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "probe_role", "--nocapture"])
        .env(PROBE_ROLE, "1")
        .env("PATH", path)
        .output()
        .unwrap();

    let said = String::from_utf8_lossy(&probe.stdout);
    assert!(probe.status.success(), "{probe:?}");
    assert!(
        said.contains("refused: bwrap cannot start a process here")
            && said.contains("setting up uid map: Permission denied")
            && said.contains("unprivileged user namespaces"),
        "{said}"
    );
}

/// Once its child is reaped, a leash has nothing left to kill: its pid
/// may belong to another process by then. Reaped by either wait.
#[test]
fn a_reaped_child_is_off_the_leash() {
    let polled = Leash::default();
    let mut child = child::command("true").spawn_leashed(&polled).unwrap();
    assert!(eventually(GONE_WITHIN, || child
        .try_wait()
        .unwrap()
        .is_some()));
    assert!(!polled.cut(), "try_wait released the pid");

    let waited = Leash::default();
    let mut child = child::command("true").spawn_leashed(&waited).unwrap();
    child.wait().unwrap();
    assert!(!waited.cut(), "wait released the pid");
}

/// A child that already exited is only reaped.
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

/// A program that ignores SIGTERM is killed (SIGKILL) once the grace is
/// over, and not before: the grace is what it was given to clean up in.
#[test]
fn kill_tree_kills_a_program_that_ignores_sigterm() {
    let dir = TempDir::new("child-term-ignored");
    let mut child = spawn_in(
        dir.path(),
        "trap '' TERM; touch escaped; while :; do sleep 1; done",
    );
    let (killed, done) = std::sync::mpsc::channel();
    let t0 = std::time::Instant::now();

    // On a thread, so a kill_tree that never returns fails this test
    // rather than hanging it.
    std::thread::spawn(move || {
        child.kill_tree();
        let _ = killed.send(());
    });

    assert!(
        done.recv_timeout(Duration::from_secs(30)).is_ok(),
        "kill_tree did not return"
    );
    assert!(
        t0.elapsed() >= Duration::from_secs(9),
        "SIGKILLed after {:?}, inside the grace",
        t0.elapsed()
    );
    assert!(
        eventually(GONE_WITHIN, || processes_in(dir.path()) == 0),
        "{} process(es) outlived kill_tree",
        processes_in(dir.path())
    );
}

/// A program not on `PATH` fails to spawn, as it did before bwrap: not a
/// bwrap that starts and exits 1.
#[test]
fn a_program_not_on_path_does_not_spawn() {
    let spawned = child::command("hunter-no-such-program").spawn();

    let err = spawned.err().expect("no child");
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound, "{err}");
    assert!(
        err.to_string()
            .contains("hunter-no-such-program: not found on PATH"),
        "{err}"
    );
}

/// A program named by a path is not looked up on `PATH`: a relative one
/// runs from the working directory, as it would have without bwrap.
#[test]
fn a_relative_program_path_runs_from_the_working_directory() {
    let dir = TempDir::new("child-relative");
    executable(&dir.join("prog"), "#!/bin/sh\ntouch ran\n");
    let mut cmd = child::command("./prog");
    cmd.current_dir(dir.path());

    let status = cmd.spawn().unwrap().wait().unwrap();

    assert!(status.success(), "{status:?}");
    assert!(dir.join("ran").exists());
}

/// A bare name runs the first executable file of that name on `PATH`,
/// passing over a file without an execute bit and a directory.
#[test]
fn a_bare_program_runs_the_first_executable_on_path() {
    let bins = support::FakeBins::acquire("child-path");
    let (plain, dir, exe) = (
        TempDir::new("child-path-plain"),
        TempDir::new("child-path-dir"),
        TempDir::new("child-path-exe"),
    );
    std::fs::write(plain.join("hunter-probe"), "#!/bin/sh\nexit 3\n").unwrap();
    std::fs::create_dir(dir.join("hunter-probe")).unwrap();
    // A builtin: the narrowed `PATH` holds no `touch`.
    executable(&exe.join("hunter-probe"), "#!/bin/sh\n: > ran\n");
    let path = format!(
        "{}:{}:{}",
        plain.path().display(),
        dir.path().display(),
        exe.path().display()
    );
    let _path = bins.env("PATH", Path::new(&path));
    let work = TempDir::new("child-path-work");
    let mut cmd = child::command("hunter-probe");
    cmd.current_dir(work.path());

    let status = cmd.spawn().unwrap().wait().unwrap();

    assert!(status.success(), "{status:?}");
    assert!(work.join("ran").exists());
}

/// Write `path` as an executable script.
fn executable(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
