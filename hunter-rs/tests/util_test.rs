#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `util::run_cmd*` end to end: the daemon's own tools (git, `gh`,
//! `glab`), run as plain children (`hunter::child::tool`).

use std::path::Path;
use std::time::{Duration, Instant};

use hunter::util::{run_cmd, run_cmd_in};

/// The daemon's own tools run as the operator, not in a user namespace
/// that shows root's files as owned by nobody: ssh refuses a config it
/// cannot trust that way, and every fetch and push over ssh would fail.
#[test]
fn own_tools_see_files_as_their_owners() {
    let (rc, out) = run_cmd(&["stat", "-c", "%u", "/"], 30);

    assert_eq!((rc, out.trim()), (0, "0"), "/ is root's");
}

#[test]
fn test_run_cmd_outlives_a_full_pipe_buffer() {
    // 300 KB per stream, far past the 64 KiB pipe buffer. Read only
    // after the wait, the child blocks on write, never exits, and this
    // comes back as (124, "timeout after 30s") instead.
    let (rc, out) = run_cmd(
        &[
            "sh",
            "-c",
            "yes abcdefghij | head -c 300000; yes klmnopqrst | head -c 300000 >&2",
        ],
        30,
    );
    assert_eq!(rc, 0);
    assert_eq!(out.len(), 600_000);
}

/// `read_to_string` would abort on the first invalid byte and leave the
/// buffer EMPTY -- losing the whole stream and stopping the drain.
#[test]
fn test_run_cmd_survives_invalid_utf8() {
    let (rc, out) = run_cmd(&["sh", "-c", "printf 'before\\377after'"], 30);
    assert_eq!(rc, 0);
    assert!(
        out.contains("before"),
        "lost output before the bad byte: {out:?}"
    );
    assert!(
        out.contains("after"),
        "stopped draining at the bad byte: {out:?}"
    );
    assert!(
        out.contains('\u{fffd}'),
        "bad byte should decode lossily: {out:?}"
    );
}

/// The working directory is the whole point of `run_cmd_in` (and of
/// `forge`'s wrapper of it). Deliberately not the test's own cwd: a call
/// that dropped the directory would still pass against a path the process
/// is in anyway.
#[test]
fn test_command_runs_in_the_requested_directory() {
    let (rc, out) = run_cmd_in(&["sh", "-c", "pwd -P"], Some(Path::new("/")), 30);
    assert_eq!(rc, 0);
    assert_eq!(out.trim_end(), "/");
}

/// A timeout must bound the call even when the child leaves a
/// descendant holding the pipes.
///
/// `sh -c "sleep 30 & ..."` models `npx` → `node` → tool: the shell
/// exits, the grandchild inherits stdout and stderr and keeps them
/// open. Killing only the direct child leaves those pipes unclosed, so
/// `join_pipes` blocks on `read_to_end` forever and `run_cmd` never
/// returns — the timeout it was given is unenforceable.
#[test]
fn test_timeout_holds_when_a_grandchild_inherits_the_pipes() {
    let t0 = Instant::now();
    let (rc, _out) = run_cmd(&["sh", "-c", "sleep 30 & sleep 30"], 1);
    let elapsed = t0.elapsed();

    assert_eq!(rc, 124, "expected the timeout exit code");
    assert!(
        elapsed < Duration::from_secs(20),
        "run_cmd hung past its 1s timeout: {elapsed:?}"
    );
}

/// A descendant that ignores SIGTERM must still not outlive the
/// deadline. `kill_tree` sends SIGTERM and must not return while it
/// lives: returning as soon as the *direct* child exits leaves a
/// trapping grandchild holding the pipes, which is the same hang the
/// group kill exists to prevent.
#[test]
fn test_timeout_holds_against_a_sigterm_ignoring_grandchild() {
    let t0 = Instant::now();
    let (rc, _out) = run_cmd(
        &["sh", "-c", "sh -c 'trap \"\" TERM; sleep 30' & sleep 30"],
        1,
    );
    let elapsed = t0.elapsed();

    assert_eq!(rc, 124, "expected the timeout exit code");
    assert!(
        elapsed < Duration::from_secs(20),
        "run_cmd hung past its 1s timeout: {elapsed:?}"
    );
}

/// A child that exits *promptly* while leaving a background descendant
/// must not block the call. This is the success path, where no timeout
/// is in force at all: `try_wait` reporting the direct child gone says
/// nothing about the pipes, which the descendant still holds open, so
/// `read_to_end` never returns and the caller hangs with no deadline to
/// rescue it. The timeout tests cannot catch this — they keep the
/// direct child alive until the deadline.
#[test]
fn test_prompt_exit_with_a_lingering_descendant_does_not_block() {
    let t0 = Instant::now();
    // The shell exits at once; the backgrounded sleep inherits stdout
    // and stderr and would hold them for 30s.
    let (rc, _out) = run_cmd(&["sh", "-c", "sleep 30 & exit 0"], 60);
    let elapsed = t0.elapsed();

    assert_eq!(rc, 0, "the direct child exited cleanly");
    assert!(
        elapsed < Duration::from_secs(10),
        "run_cmd blocked on a descendant holding the pipes: {elapsed:?}"
    );
}
