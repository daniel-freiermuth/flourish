//! Small shared utilities (port of hunter/util.py where still needed).

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

/// Epoch milliseconds (types.py `now_ms`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Read a child pipe to EOF on its own thread. Both pipes have to be
/// drained while the child runs — a child that fills a pipe buffer blocks
/// forever if we only read after it exits (util.py got this for free from
/// `communicate()`).
///
/// Bytes, not `String`: `read_to_string` aborts on the first invalid UTF-8
/// sequence AND leaves the buffer empty, so one stray byte from a git diff
/// or a worker would discard the whole stream and stop draining it —
/// reinstating the deadlock this exists to prevent.
pub fn drain_pipe<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    })
}

/// Join a pair of pipe readers, stdout first — the merged output util.py
/// produced with `stderr=STDOUT`. Decoded lossily once, after both streams
/// reach EOF. A reader that panicked contributes nothing.
pub fn join_pipes(so: JoinHandle<Vec<u8>>, se: JoinHandle<Vec<u8>>) -> String {
    let mut bytes = so.join().unwrap_or_default();
    bytes.extend_from_slice(&se.join().unwrap_or_default());
    String::from_utf8_lossy(&bytes).into_owned()
}

fn killpg<T: Into<Option<Signal>>>(pgid: u32, sig: T) -> bool {
    signal::killpg(Pid::from_raw(pgid as i32), sig).is_ok()
}

/// Is any process still in the group? The null signal performs the
/// permission and existence checks without delivering anything.
fn group_alive(pgid: u32) -> bool {
    killpg(pgid, None)
}

/// SIGTERM the child's process group, wait up to 10 s, then SIGKILL.
///
/// Signalling the group rather than the pid is what makes a timeout
/// enforceable. Killing only the direct child leaves any descendant it
/// spawned (`npx` → `node` → the tool itself) holding the inherited stdout
/// and stderr, so the pipes never reach EOF and `join_pipes` blocks
/// forever — the process we gave up waiting for outlives the deadline that
/// was supposed to bound it. Callers must spawn with `process_group(0)`.
pub fn kill_tree(child: &mut Child) {
    let pgid = child.id();
    if !killpg(pgid, Signal::SIGTERM) {
        let _ = child.wait();
        return; // already gone
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // Reap first: a zombie direct child still counts as a group
        // member, so an unreaped one would make the probe below claim
        // the group is alive forever.
        let _ = child.try_wait();
        // Signal 0 tests the whole group, which is the condition that
        // matters. Waiting on the direct child instead declares success
        // while a descendant that ignored SIGTERM is still holding the
        // inherited pipes — the caller then blocks in `join_pipes` and
        // the deadline this exists to enforce is lost.
        if !group_alive(pgid) {
            break; // no members left
        }
        if Instant::now() >= deadline {
            killpg(pgid, Signal::SIGKILL);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.wait();
}

/// Last `max_bytes` of `s`, snapped forward to a char boundary.
///
/// Command output and worker logs are arbitrary UTF-8, so indexing a tail
/// by byte offset panics whenever the cut lands inside a codepoint. The
/// result is always a suffix and never longer than `max_bytes`.
pub fn tail(s: &str, max_bytes: usize) -> &str {
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Run a command, merged stdout+stderr, never fails (`util.run_cmd`):
/// timeout -> (124, msg + whatever was captured); spawn error -> (127, msg).
/// Blocking — call via `spawn_blocking` from async contexts.
pub fn run_cmd(argv: &[&str], timeout_s: u64) -> (i32, String) {
    run_cmd_in(argv, None, timeout_s)
}

/// [`run_cmd`], optionally in a working directory.
///
/// The single spawn-and-reap implementation. Every copy of this loop has
/// to reap the whole process group on all three exit paths, and the
/// copies did not: the fix landed four times, one of them two rounds
/// late. There is one body now, so there is one place to get it right.
pub fn run_cmd_in(argv: &[&str], cwd: Option<&Path>, timeout_s: u64) -> (i32, String) {
    run_cmd_fed(argv, cwd, None, timeout_s)
}

/// [`run_cmd`] with `input` written to the command's stdin, for a request
/// body too structured for flags (`gh api --input -`).
pub fn run_cmd_input(argv: &[&str], input: &str, timeout_s: u64) -> (i32, String) {
    run_cmd_fed(argv, None, Some(input), timeout_s)
}

/// The one body behind every `run_cmd*` (see [`run_cmd_in`]).
fn run_cmd_fed(
    argv: &[&str],
    cwd: Option<&Path>,
    input: Option<&str>,
    timeout_s: u64,
) -> (i32, String) {
    let Some((prog, rest)) = argv.split_first() else {
        return (127, "empty argv".to_owned());
    };
    let mut cmd = Command::new(prog);
    cmd.args(rest)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (127, e.to_string()),
    };
    // Fed from a thread, like the pipes are drained: a child that answers
    // before reading all of its input would otherwise block us on a full
    // stdin pipe while we never get to read its output. Dropping the
    // handle at the end is the EOF the child waits for.
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let input = input.to_owned();
        std::thread::spawn(move || {
            use std::io::Write as _;
            // A child that exits without reading is its own answer.
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    // Readers start now, not after the wait: see `drain_pipe`. Every exit
    // path below reaps the child first, so the joins see a closed pipe.
    let so = drain_pipe(child.stdout.take());
    let se = drain_pipe(child.stderr.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // The direct child exiting says nothing about the pipes: a
                // descendant it left behind still holds them, and
                // `join_pipes` reads to EOF. On this path no deadline is
                // in force, so that wait would be unbounded. Killing the
                // group first is what guarantees EOF; it costs nothing
                // when there is no descendant, since the group is then
                // already empty.
                kill_tree(&mut child);
                return (status.code().unwrap_or(-1), join_pipes(so, se));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_tree(&mut child);
                    let out = join_pipes(so, se);
                    return (124, format!("timeout after {timeout_s}s\n{out}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                kill_tree(&mut child);
                let out = join_pipes(so, se);
                return (127, format!("{e}\n{out}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // -- tail -------------------------------------------------------------
    //
    // `tail` truncates command output and worker logs at ~20 call sites,
    // all of them on bytes that came from a subprocess. Indexing the tail
    // by byte offset panicked whenever the cut landed inside a codepoint,
    // which a diff containing one non-ASCII character is enough to trigger.

    #[test]
    fn test_tail_shorter_than_limit_is_whole() {
        assert_eq!(tail("abc", 10), "abc");
        assert_eq!(tail("", 10), "");
    }

    #[test]
    fn test_tail_exactly_limit_is_whole() {
        assert_eq!(tail("abcdef", 6), "abcdef");
    }

    #[test]
    fn test_tail_longer_than_limit_keeps_the_end() {
        let s = "abcdefghij";
        let got = tail(s, 3);
        // The end is the interesting part of a log: a prefix would show the
        // startup banner and drop the error.
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert_eq!(got, "hij");
    }

    /// The panic this function exists to prevent. `€` is 3 bytes, the limit
    /// is 7, so the raw cut at `len - 7` lands one byte into a codepoint --
    /// a limit divisible by the char width would pass even with byte
    /// slicing restored.
    #[test]
    fn test_tail_does_not_split_a_codepoint() {
        let s = "€€€€€"; // 15 bytes, boundaries at 0/3/6/9/12/15
        assert_eq!(s.len(), 15);
        let got = tail(s, 7);
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert!(got.len() <= 7, "{} bytes exceeds the limit", got.len());
        // Snapped forward from byte 8 to byte 9, so two whole chars.
        assert_eq!(got, "€€");
    }

    /// Same cut, but with the split codepoint at the very front of what
    /// survives and ASCII behind it -- the shape a truncated stack trace has.
    #[test]
    fn test_tail_snaps_forward_past_a_partial_char() {
        let s = "aaa€bbb"; // 9 bytes: cut at 9-5=4 is inside the `€`
        let got = tail(s, 5);
        assert!(s.ends_with(got), "{got:?} is not a suffix of {s:?}");
        assert!(got.len() <= 5, "{} bytes exceeds the limit", got.len());
        assert_eq!(got, "bbb");
    }

    #[test]
    fn test_tail_zero_limit_is_empty() {
        assert_eq!(tail("abcdef", 0), "");
        assert_eq!(tail("€", 0), "");
        assert_eq!(tail("", 0), "");
    }
}

#[cfg(test)]
mod process_group_tests {
    use super::*;

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
        let t0 = std::time::Instant::now();
        let (rc, _out) = run_cmd(&["sh", "-c", "sleep 30 & sleep 30"], 1);
        let elapsed = t0.elapsed();

        assert_eq!(rc, 124, "expected the timeout exit code");
        assert!(
            elapsed < Duration::from_secs(20),
            "run_cmd hung past its 1s timeout: {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod kill_tree_tests {
    use super::*;

    /// A descendant that ignores SIGTERM must still not outlive the
    /// deadline. `kill_tree` sends SIGTERM to the group and then has to
    /// escalate; returning as soon as the *direct* child exits leaves a
    /// trapping grandchild holding the pipes, which is the same hang the
    /// group kill exists to prevent.
    #[test]
    fn test_timeout_holds_against_a_sigterm_ignoring_grandchild() {
        let t0 = std::time::Instant::now();
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
}

#[cfg(test)]
mod exit_path_tests {
    use super::*;

    /// A child that exits *promptly* while leaving a background descendant
    /// must not block the call. This is the success path, where no timeout
    /// is in force at all: `try_wait` reporting the direct child gone says
    /// nothing about the pipes, which the descendant still holds open, so
    /// `read_to_end` never returns and the caller hangs with no deadline to
    /// rescue it. The timeout tests cannot catch this — they keep the
    /// direct child alive until the deadline.
    #[test]
    fn test_prompt_exit_with_a_lingering_descendant_does_not_block() {
        let t0 = std::time::Instant::now();
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
}
