//! Worker runner — spawns headless omp, meters its ledger, kills at cap.
//!
//! Port of `hunter/backends/omp_scavenge/harness.py`. The cap design (exp1b):
//! never trust harness cooperation. The worker's session JSONL is written
//! live under its chain's session directory; we watch it and SIGTERM the
//! process group at the token threshold.
//!
//! Every run is handed an explicit `--session-dir`: its chain's
//! `session/` (see `crate::workspace`), which the caller owns and which is
//! empty for a cold run. That is not tidiness: omp's `autoResume` setting
//! (global config, on by default for this operator) makes a bare `omp -p`
//! continue the newest session for the same cwd whenever no session flag
//! or session directory is passed. A worker that resumes re-caches the
//! whole prior transcript on its first call — the observed cost was
//! 508 709 `cacheWrite` tokens on call #1 for a repo whose session had
//! accumulated 290 calls since 2026-09-06, which trips any cap before the
//! worker does a single useful thing. Passing an explicit, empty session
//! directory is what makes each job start cold.
//!
//! The exception is a deliberate continuation: `run_worker` can be handed
//! the session file of a job that ran out of window headroom mid-flight
//! and continue *that* transcript by naming its path. That is a different
//! mechanism from `autoResume` and the two must not be confused — see the
//! `--resume` argument in `run_worker` for what was measured.

use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde::Deserialize;

use crate::config::Config;
use crate::types::RunResult;
use crate::util::kill_tree;
use crate::workspace::Workspace;

// ---------------------------------------------------------------------------
// Ledger metering (`harness.ledger_usage`, `harness._ledger_dir_usage`)
// ---------------------------------------------------------------------------

/// One line of omp's session JSONL, as far as metering reads it.
///
/// Typed so that a change to omp's schema is an error rather than a
/// silent zero: a meter that reads a renamed or restringified usage
/// field as 0 keeps counting calls at no cost, which disarms the cap and
/// records the run as free. Unknown fields are ignored; records without
/// a `message` (session headers, model changes, …) are not calls.
#[derive(Deserialize)]
struct LedgerRecord {
    #[serde(default)]
    message: Option<LedgerMessage>,
}

/// Only an assistant message is a call; every other role is skipped
/// without looking at its body.
#[derive(Deserialize)]
#[serde(tag = "role")]
enum LedgerMessage {
    #[serde(rename = "assistant")]
    Assistant {
        #[serde(default)]
        usage: Option<LedgerUsage>,
    },
    #[serde(other)]
    Other,
}

/// The "new" token counts of one call. All required: a record that
/// lacks one is unmeterable, not free.
#[derive(Deserialize)]
struct LedgerUsage {
    input: i64,
    output: i64,
    #[serde(rename = "cacheWrite")]
    cache_write: i64,
}

/// Sum "new" tokens (input + output + cacheWrite) and call count from a
/// session JSONL file.
///
/// The one thing tolerated is a torn final line — omp is mid-write and
/// the newline has not landed yet. Everything else is an error: a file
/// that cannot be opened or read, a line that is not JSON anywhere
/// before the end, or an assistant usage record missing a token field.
/// Reporting 0 for any of those would tell the cap watchdog the worker
/// is spending nothing.
pub fn ledger_usage(session_file: &Path) -> anyhow::Result<(i64, i64)> {
    let file = fs::File::open(session_file)
        .with_context(|| format!("opening ledger {}", session_file.display()))?;
    let mut reader = BufReader::new(file);
    let mut tokens: i64 = 0;
    let mut calls: i64 = 0;
    let mut line: Vec<u8> = Vec::new();

    for lineno in 1.. {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .with_context(|| format!("reading ledger {}", session_file.display()))?;
        if n == 0 {
            break;
        }
        let terminated = line.last() == Some(&b'\n');
        if line.trim_ascii().is_empty() {
            continue;
        }
        let rec: LedgerRecord = match serde_json::from_slice(&line) {
            Ok(r) => r,
            Err(_) if !terminated => break, // torn final line mid-write
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("ledger {} line {lineno}", session_file.display()));
            }
        };
        if let Some(LedgerMessage::Assistant { usage: Some(u) }) = rec.message {
            calls += 1;
            tokens += u.input + u.output + u.cache_write;
        }
    }
    Ok((tokens, calls))
}

/// The context a session had reached when it stopped: the LAST usage
/// record's `input + cacheRead + cacheWrite`, skipping records that
/// report no context at all.
///
/// Same file and same records [`ledger_usage`] walks, different
/// question. `ledger_usage` sums what a run ADDED, deliberately
/// excluding `cacheRead` because a cache hit is not new spend. Resuming
/// asks the opposite: the first call of a resumed session re-establishes
/// the whole context, so what it will cost is the context size at the
/// moment of suspension — cache reads included. Measured across 112
/// production re-cache events the re-cache / prior-context ratio had
/// median 1.00 and p10 1.00, so the last record's total is the estimate,
/// not a lower bound to pad.
///
/// The LAST record rather than the largest: context grows monotonically
/// within a session, and the final call is the one the resumed session
/// picks up from.
///
/// A record whose context is zero is a call that was killed, not a
/// measurement. When the watchdog SIGTERMs a worker at its cap, omp
/// writes a `session_exit` record and then closes the in-flight call as
/// an assistant message with `stopReason: "aborted"` and all-zero usage
/// — so for every cap suspension that aborted record is the last one.
/// Taking it would report a context of 0 and reserve a resume only the
/// progress floor, while its first call re-caches the whole transcript.
/// No real call runs with an empty context, so the zero rule covers any
/// other such record too, not just the aborted one.
///
/// `None` when the file is unreadable or holds no usage record with a
/// context — the caller has a documented fallback, and inventing a
/// number here would hide a missing transcript inside a
/// plausible-looking estimate.
pub fn ctx_at_suspension(session_file: &Path) -> Option<i64> {
    let file = fs::File::open(session_file).ok()?;
    let reader = BufReader::new(file);
    let mut last: Option<i64> = None;

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue; // partial trailing line mid-write
        };
        let Some(msg) = rec.get("message").and_then(|v| v.as_object()) else {
            continue;
        };
        if msg.get("role").and_then(|v| v.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(u) = msg.get("usage").and_then(|v| v.as_object()) {
            let field = |name: &str| u.get(name).and_then(serde_json::Value::as_i64).unwrap_or(0);
            let ctx = field("input") + field("cacheRead") + field("cacheWrite");
            if ctx > 0 {
                last = Some(ctx);
            }
        }
    }
    last
}

/// Meter a worker's whole private session directory: every `*.jsonl` omp
/// wrote for this run, summed. One file is the norm; summing rather than
/// picking one means an extra transcript (a nested session) is counted as
/// spend instead of silently discounted. That includes subagents: omp
/// keeps a session's artifacts in the directory named after its
/// transcript (`<stem>.jsonl` → `<stem>/`) and writes each subagent's
/// transcript there (`<stem>/<Agent>.jsonl`, whose own subagents go in
/// `<stem>/<Agent>/`), and that spend is this run's.
///
/// Only such paired directories are walked. Everything else under
/// `<stem>/` — notably `local/`, the worker's `local://` scratch root —
/// holds whatever the agent chose to write, and a `*.jsonl` there is
/// neither a ledger nor spend.
///
/// `Ok(None)` until the directory exists and holds at least one ledger —
/// that is the "not metered yet" signal `run_worker` waits on, and kills
/// on. Any ledger that cannot be metered ([`ledger_usage`]) or a
/// directory that cannot be listed is an error: a partial sum would read
/// as spend that never happened being free.
pub fn ledger_dir_usage(run_dir: &Path) -> anyhow::Result<Option<(PathBuf, i64, i64)>> {
    let entries = match fs::read_dir(run_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(e).with_context(|| format!("listing {}", run_dir.display()));
        }
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("listing {}", run_dir.display()))?;
        let p = entry.path();
        // `DirEntry::file_type` does not follow symlinks, so a link cannot
        // send the walk outside the run directory or around a cycle.
        let is_dir = entry
            .file_type()
            .with_context(|| format!("listing {}", run_dir.display()))?
            .is_dir();
        if is_dir {
            subdirs.push(p);
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            files.push(p);
        }
    }
    if files.is_empty() {
        return Ok(None);
    }
    // omp names sessions by creation instant, so sorting by name puts the
    // run's first — the one worth recording as `session_file`.
    files.sort();
    let (mut tokens, mut calls) = (0, 0);
    for f in &files {
        let (t, c) = ledger_usage(f)?;
        tokens += t;
        calls += c;
    }
    // Nested transcripts are subagents' spend: summed, but never the
    // `session_file`. A directory is walked only when the transcript it
    // belongs to sits next to it.
    for d in subdirs {
        let mut transcript = d.as_os_str().to_owned();
        transcript.push(".jsonl");
        if files.binary_search(&PathBuf::from(transcript)).is_err() {
            continue;
        }
        if let Some((_, t, c)) = ledger_dir_usage(&d)? {
            tokens += t;
            calls += c;
        }
    }
    Ok(Some((files.swap_remove(0), tokens, calls)))
}

// ---------------------------------------------------------------------------
// Worker output capture
// ---------------------------------------------------------------------------

/// Bytes of worker output kept for `RunResult::stdout_tail`.
const TAIL_BYTES: usize = 2000;

/// Drain a worker pipe on its own thread, keeping only the last
/// `TAIL_BYTES`. The pipe has to be read for as long as the worker runs: a
/// full pipe buffer blocks the worker, and a blocked worker stops writing
/// the ledger the cap watchdog reads. Everything before the tail is
/// dropped as it arrives — a chatty worker emits megabytes.
fn drain_tail<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        if let Some(mut p) = pipe {
            let mut chunk = [0u8; 8192];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > TAIL_BYTES {
                            tail.drain(..tail.len() - TAIL_BYTES);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        tail
    })
}

/// Last `TAIL_BYTES` of `s`, snapped forward to a `char` boundary.
fn tail_str(s: &str) -> &str {
    crate::util::tail(s, TAIL_BYTES)
}

/// First line of a file, or empty on any IO error.
///
/// Only ever compared against itself: omp writes a `{"type":"title"…}`
/// header as a session's opening record and never rewrites it, so that
/// line is a cheap identity for "still the same session".
fn first_line(path: &Path) -> Vec<u8> {
    let Ok(file) = fs::File::open(path) else {
        return Vec::new();
    };
    let mut line = Vec::new();
    let _ = BufReader::new(file).read_until(b'\n', &mut line);
    line
}

/// A resume that cannot happen, reported rather than silently downgraded.
///
/// The caller asked to continue one specific transcript. If that
/// transcript is gone, spawning a cold worker here would be
/// indistinguishable from a successful resume anywhere downstream — same
/// exit code, same session path — while paying a fresh session floor and
/// redoing the work the resume existed to avoid. Whether to start clean
/// is the scheduler's decision, so the scheduler is the one told.
fn resume_unavailable(session_file: &Path, why: &str) -> RunResult {
    tracing::warn!("harness: cannot resume {}: {why}", session_file.display());
    RunResult {
        exit_code: None,
        killed_reason: Some("resume-unavailable".to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: format!("cannot resume {}: {why}", session_file.display()),
        usage_delta: None,
    }
}

// ---------------------------------------------------------------------------
// Worker execution (`harness.run_worker`)
// ---------------------------------------------------------------------------

/// Spawn `omp -p`, meter its JSONL session ledger, kill at the cap.
///
/// `cap_tokens` is the ramp's headroom for this run. `None` is a run with
/// no token bound — the ramp found no ceiling to impose — and `max_wall_s`
/// is then the only thing that stops a worker that will not stop itself.
///
/// `ws` is the chain's workspace: the worker runs in `ws.tree` and omp
/// writes its transcript into `ws.session`, which the harness meters.
///
/// `resume_from` is the session file of an earlier attempt to continue,
/// inside `ws.session`. `None` starts cold in that (empty) directory.
/// `Some` continues that transcript in place and meters only what this
/// attempt adds to it; a source that is no longer on disk is refused
/// outright (`killed_reason = "resume-unavailable"`) rather than quietly
/// downgraded to a cold run.
///
/// Blocking — call via `spawn_blocking` from async contexts.
#[allow(
    clippy::too_many_lines,
    reason = "spawn, meter and reap are one state machine over a single \
              child process; splitting it would hand the pid, the ledger \
              cursor and the deadline across function boundaries where a \
              missed step leaks a worker"
)]
pub fn run_worker(
    cfg: &Config,
    ws: &Workspace,
    prompt: &str,
    cap_tokens: Option<i64>,
    max_wall_s: i64,
    model: Option<&str>,
    resume_from: Option<&Path>,
) -> RunResult {
    let t0 = Instant::now();
    let cwd = ws.tree.as_path();
    let run_dir = ws.session.clone();
    // omp writes a continued session at the `--resume` path and
    // `--session-dir` does not move it (see the argument below), so the
    // source must still be there for the continuation to be the one meant.
    let mut resume_head: Option<Vec<u8>> = None;
    if let Some(sf) = resume_from {
        if !run_dir.is_dir() {
            return resume_unavailable(sf, "session directory is gone");
        }
        // Empty counts as gone: omp treats an unreadable resume source
        // as "start fresh here", which is the failure this refuses.
        if fs::metadata(sf).map_or(0, |m| m.len()) == 0 {
            return resume_unavailable(sf, "session file is missing or empty");
        }
        resume_head = Some(first_line(sf));
    }
    // Attribution baseline. A resumed worker appends to the predecessor's
    // ledger and `ledger_dir_usage` sums the whole directory, so every
    // token already in it was paid for by the predecessor's job row.
    // Subtracting the pre-spawn totals is what keeps this attempt's
    // `tokens_new` this attempt's — without it a resumed chain re-bills
    // its own history on every link.
    //
    // The resume source's own share is kept apart: a lost resume overwrites
    // that one file and nothing else (see `resume_lost` below).
    let (base_tokens, base_calls, source_tokens, source_calls) = if let Some(sf) = resume_from {
        let metered = ledger_dir_usage(&run_dir).and_then(|found| {
            let (t, c) = found.map_or((0, 0), |(_, t, c)| (t, c));
            // Only part of the baseline when the directory walk saw it.
            let (st, sc) = if sf.starts_with(&run_dir) {
                ledger_usage(sf)?
            } else {
                (0, 0)
            };
            Ok((t, c, st, sc))
        });
        match metered {
            Ok(found) => found,
            // No baseline means this attempt's spend cannot be told from
            // the predecessor's; running it would bill one or the other
            // wrongly.
            Err(e) => {
                return RunResult {
                    exit_code: Some(127),
                    killed_reason: Some("unmetered".to_owned()),
                    tokens_new: 0,
                    calls: 0,
                    session_file: None,
                    duration_s: 0.0,
                    stdout_tail: format!("resume baseline unmeterable: {e:#}"),
                    usage_delta: None,
                };
            }
        }
    } else {
        (0, 0, 0, 0)
    };
    if let Err(e) = fs::create_dir_all(&run_dir) {
        // Without a private directory the worker would fall back to omp's
        // cwd-keyed session and resume it. Refusing is cheaper than the
        // half-megatoken first call that follows.
        return RunResult {
            exit_code: Some(127),
            killed_reason: Some("unmetered".to_owned()),
            tokens_new: 0,
            calls: 0,
            session_file: None,
            duration_s: 0.0,
            stdout_tail: format!("session dir {} unusable: {e}", run_dir.display()),
            usage_delta: None,
        };
    }

    // Build command: [omp_bin, "-p", prompt] + optional flags
    let mut cmd = Command::new(&cfg.omp_bin);
    cmd.arg("-p").arg(prompt);
    cmd.arg(format!("--session-dir={}", run_dir.display()));
    if let Some(sf) = resume_from {
        // Measured against omp v18.2.6 on 2026-09-24, because the obvious
        // reading of these two flags is that they conflict and they do
        // not: `--resume=<path>` and `--session-dir=<dir>` coexist, and
        // under `-p` the named session continues non-interactively. Proof
        // from the probe: the resumed run's transcript kept the first
        // exchange byte for byte and chained its new records onto the old
        // file's last one, while the same prompt in a fresh session
        // directory answered "there is no earlier reply in this
        // conversation". This is emphatically not omp's `autoResume`,
        // which continues "the newest session for this cwd" — naming the
        // path is what makes the continuation the one we meant.
        //
        // `--resume` also decides where the transcript is written: omp
        // appends to the named file and `--session-dir` does not override
        // that. An unresolvable path is not an error — omp starts a fresh
        // session, writes it at that path, and exits 0 with no diagnostic
        // — which is why the source is checked before this spawn instead
        // of trusted after it.
        //
        // Cost shape, same probe: the cold call wrote 22 976 `cacheWrite`
        // and read 0; the resumed call wrote 28 and read 22 976. Inside
        // the prompt-cache TTL a resume re-caches essentially nothing.
        // Past it the re-cache costs the context size at suspension (112
        // production re-cache events, median ratio 1.00) — still bounded
        // by the transcript, never by redoing the work that produced it.
        cmd.arg(format!("--resume={}", sf.display()));
    }
    if let Some(m) = model {
        cmd.arg(format!("--model={m}"));
    }
    if let Some(smol) = &cfg.model_smol {
        cmd.arg(format!("--smol={smol}"));
    }

    // Spawn in its own process group (for group-kill).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Build a minimal env so the worker doesn't inherit the full daemon env
    // (especially ANTHROPIC_ API keys/tokens).
    let allowlist: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SSH_AUTH_SOCK",
    ];
    let allowed_prefixes: &[&str] = &["OMP_", "XDG_"];
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if allowlist.contains(&k.as_str()) || allowed_prefixes.iter().any(|p| k.starts_with(p)) {
            cmd.env(&k, &v);
        }
    }
    // Every chain builds in a fresh tree, so a tree-local `target/` would
    // start every chain with a full cold compile inside its wall-clock
    // limit, and cost up to a gigabyte of disk per live chain. Point cargo
    // at the clone's own warm `target/` instead (`Workspace::build_cache`).
    // Set for every repo: an unused variable costs a non-Rust worker
    // nothing, and detecting the language would be a second guess at what
    // the worker will build. An operator who exported their own for the
    // daemon keeps it — it is not in the allowlist above, so it is passed
    // through here rather than inherited.
    match std::env::var_os("CARGO_TARGET_DIR") {
        Some(own) => cmd.env("CARGO_TARGET_DIR", own),
        None => cmd.env("CARGO_TARGET_DIR", ws.build_cache()),
    };
    // Explicitly null, never inherited: omp reads a piped stdin as extra
    // prompt text and blocks on EOF before it initialises the session --
    // "Reading prompt from piped stdin (waiting for EOF)". A worker that
    // does that writes no ledger at all, so it is killed as `unmetered`
    // after the grace period, having spent its whole wall-clock slot
    // doing nothing.
    //
    // Today the daemon happens to have /dev/null on fd 0, because the
    // unit sets no StandardInput and systemd's default is null. That is
    // an inherited accident, not a decision: run the daemon from a
    // supervisor or a wrapper that pipes stdin and every worker hangs.
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(e) => {
            return RunResult {
                exit_code: Some(127),
                killed_reason: None,
                tokens_new: 0,
                calls: 0,
                session_file: None,
                duration_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
                stdout_tail: format!("spawn error: {e}"),
                usage_delta: None,
            };
        }
    };

    // Readers start here and run until the pipes close: see `drain_tail`.
    let so = drain_tail(proc.stdout.take());
    let se = drain_tail(proc.stderr.take());

    let mut session: Option<PathBuf> = None;
    let mut tokens: i64 = 0;
    let mut calls: i64 = 0;
    let mut killed: Option<String> = None;

    loop {
        let exited = matches!(proc.try_wait(), Ok(Some(_)));
        match ledger_dir_usage(&run_dir) {
            Ok(Some((path, t, c))) => {
                session = Some(path);
                // Clamped because the subtraction has one way to go
                // negative: the transcript shrinking, i.e. being replaced
                // rather than appended to. A negative running total would
                // sit below every cap and disarm the watchdog outright.
                tokens = (t - base_tokens).max(0);
                calls = (c - base_calls).max(0);
            }
            Ok(None) => {}
            // A ledger the meter cannot read is a worker the cap cannot
            // see. Stop it as unmetered and keep the last good total.
            Err(e) => {
                tracing::warn!(
                    "harness: cannot meter {}: {e:#} — killing worker rather \
                     than running it unmetered",
                    run_dir.display()
                );
                killed = Some("unmetered".to_owned());
                kill_tree(&mut proc);
                break;
            }
        }
        if exited {
            // Reap the group even though the worker left of its own
            // accord. `exited` is about the direct child; a descendant it
            // spawned still holds the inherited stdout and stderr, and the
            // joins below read to EOF. This path has already left
            // `max_wall_s` behind, so nothing would interrupt that wait.
            // No-op when the group is already empty, which is the norm.
            kill_tree(&mut proc);
            break;
        }
        if cap_tokens.is_some_and(|cap| tokens >= cap) {
            killed = Some("cap".to_owned());
            kill_tree(&mut proc);
            break;
        }
        // On a resume the directory already holds the predecessor's
        // transcript, so "a ledger exists" says nothing about this
        // attempt; the equivalent signal is "this attempt has recorded no
        // call yet".
        let unmetered = if resume_from.is_some() {
            calls == 0
        } else {
            session.is_none()
        };
        if unmetered && t0.elapsed().as_secs() >= cfg.session_grace_s {
            tracing::warn!(
                "harness: no session ledger under {} after {}s — \
                 killing worker rather than running it unmetered",
                run_dir.display(),
                cfg.session_grace_s
            );
            killed = Some("unmetered".to_owned());
            kill_tree(&mut proc);
            break;
        }
        if t0.elapsed().as_secs() as i64 >= max_wall_s {
            killed = Some("wallclock".to_owned());
            kill_tree(&mut proc);
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(cfg.poll_s));
    }

    let exit_code = proc.wait().ok().and_then(|s| s.code());

    // Did the resume take? omp answers in the one place it costs nothing
    // to look: a fresh session written at the `--resume` path replaces the
    // file's opening record, where a real continuation appends and leaves
    // it untouched. Nothing in the exit code distinguishes the two, and
    // missing it means believing work was continued that was in fact
    // redone from scratch at full price.
    let mut resume_lost = false;
    if let (Some(sf), Some(head)) = (resume_from, &resume_head) {
        resume_lost = first_line(sf) != *head;
        if resume_lost {
            tracing::warn!(
                "harness: --resume {} did not continue that session — omp \
                 started a fresh one at the same path, so this attempt \
                 redid the work it was meant to continue",
                sf.display()
            );
        }
    }
    // A lost resume took the predecessor's transcript with it, so that
    // file's share of the baseline no longer describes anything on disk:
    // what is there now was written by this attempt. The rest of the
    // baseline still is on disk — omp keeps a session's subagent
    // transcripts in `<stem>/` and a fresh session at the same path
    // reuses that directory without clearing it — and stays the
    // predecessor's.
    let (base_tokens, base_calls) = if resume_lost {
        (base_tokens - source_tokens, base_calls - source_calls)
    } else {
        (base_tokens, base_calls)
    };

    // Final ledger read after exit. A metering error here keeps the last
    // good running total and marks the run unmetered, unless a more
    // specific reason already did.
    match ledger_dir_usage(&run_dir) {
        Ok(Some((path, t, c))) => {
            session = Some(path);
            tokens = (t - base_tokens).max(0);
            calls = (c - base_calls).max(0);
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                "harness: cannot meter {} after exit: {e:#} — keeping the last \
                 running total ({tokens} tok) and marking the run unmetered",
                run_dir.display()
            );
            if killed.is_none() {
                killed = Some("unmetered".to_owned());
            }
        }
    }

    // Exited before the grace period with no ledger: `tokens_new = 0` here
    // means "unknown", never "free". Recording it Done would assert the
    // worker cost nothing, which also drags the anticipated_tokens
    // percentiles down. An existing reason (cap, wallclock) already marks
    // the run as not-Done and is more specific, so it wins.
    let unmetered = if resume_from.is_some() {
        calls == 0
    } else {
        session.is_none()
    };
    if unmetered {
        if killed.is_none() {
            killed = Some("unmetered".to_owned());
        }
        tracing::warn!(
            "harness: worker exited after {:.1}s with no session ledger under {} — \
             token spend unmetered",
            t0.elapsed().as_secs_f64(),
            run_dir.display()
        );
    }

    // Capture output tail. Every path above reaps the process group, so
    // no writer survives to hold a pipe open and the joins are short. That
    // is a precondition of this code, not an observation about it: drop
    // the reap on any one path and these joins become unbounded.
    let mut bytes = so.join().unwrap_or_default();
    bytes.extend_from_slice(&se.join().unwrap_or_default());
    let text = String::from_utf8_lossy(&bytes);
    // The marker rides on the output tail because that is what a killed
    // run leaves an operator to read; it is worth the few bytes over
    // `TAIL_BYTES` that it costs.
    let stdout_tail = if resume_lost {
        format!("[resume-lost] {}", tail_str(&text))
    } else {
        tail_str(&text).to_owned()
    };

    RunResult {
        exit_code,
        killed_reason: killed,
        tokens_new: tokens,
        calls,
        session_file: session.map(|p| p.to_string_lossy().into_owned()),
        duration_s: (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        stdout_tail,
        usage_delta: None, // set by facade.rs run()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tail_str_snaps_to_char_boundary() {
        // 3-byte chars: the raw byte cut at len-TAIL_BYTES lands mid-char
        // (3000 - 2000 = 1000, and 1000 % 3 != 0), which would panic.
        let s = "€".repeat(1000);
        let tail = tail_str(&s);
        assert_eq!(tail.len(), 1998);
        assert!(s.ends_with(tail));
        assert!(tail.chars().all(|c| c == '€'));
    }

    #[test]
    fn test_tail_str_keeps_short_input_whole() {
        assert_eq!(tail_str("héllo"), "héllo");
    }

    #[test]
    fn test_drain_tail_retains_only_the_tail() {
        let mut src = vec![b'a'; TAIL_BYTES * 3];
        src.extend_from_slice(b"END");
        let out = drain_tail(Some(std::io::Cursor::new(src)))
            .join()
            .unwrap_or_default();
        assert_eq!(out.len(), TAIL_BYTES);
        assert!(out.ends_with(b"END"));
    }
}
