//! Small shared utilities (port of hunter/util.py where still needed).

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::thread::JoinHandle;
use std::time::Duration;

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
/// For the daemon's own tools (git, `gh`, `glab`): it runs `argv` as one
/// ([`crate::child::tool`]), not contained.
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
    let mut cmd = crate::child::tool(prog);
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
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (127, e.to_string()),
    };
    // Fed from a thread, like the pipes are drained: a child that answers
    // before reading all of its input would otherwise block us on a full
    // stdin pipe while we never get to read its output. Dropping the
    // handle at the end is the EOF the child waits for.
    if let (Some(input), Some(mut stdin)) = (input, child.take_stdin()) {
        let input = input.to_owned();
        std::thread::spawn(move || {
            use std::io::Write as _;
            // A child that exits without reading is its own answer.
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    // Readers start now, not after the wait: see `drain_pipe`. Every exit
    // path below reaps the child first, so the joins see a closed pipe.
    let so = drain_pipe(child.take_stdout());
    let se = drain_pipe(child.take_stderr());
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
                child.kill_tree();
                return (status.code().unwrap_or(-1), join_pipes(so, se));
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    child.kill_tree();
                    let out = join_pipes(so, se);
                    return (124, format!("timeout after {timeout_s}s\n{out}"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                child.kill_tree();
                let out = join_pipes(so, se);
                return (127, format!("{e}\n{out}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
