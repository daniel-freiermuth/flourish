#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_worker` end to end — metering, the unmetered verdict, output drain.
//!
//! `harness_test.rs` covers the pieces (`ledger_usage`, `kill_tree`) in
//! isolation. Nothing covered the function that
//! wires them together, which is where the accounting invariant lives:
//!
//! > a run whose token spend could not be measured must never be recorded
//! > as costing zero.
//!
//! `tokens_new = 0` means "unknown", never "free". A review round found
//! `run_worker` dropping a ledger-less worker through as a clean success:
//! the budget then under-counts the window *and* the false zero drags the
//! `anticipated_tokens` percentiles down, so the next job is sized against
//! a spend that never happened. `killed_reason` is the flag that stops it —
//! `scheduler::job_state` maps `Some(_)` to `JobState::Killed`, and only
//! `None` + exit 0 to `JobState::Done`.
//!
//! The seam: `cfg.omp_bin` is looked up on `PATH` (`FakeBins`). Session
//! directories need no sandboxing — the caller hands `run_worker` a
//! workspace under a scratch directory, never the operator's
//! `~/.omp/agent/sessions`. `$OMP_HOME` is still redirected so that a
//! worker which ignores `--session-dir` writes into the scratch tree
//! instead of the developer's real one.

mod support;

use std::fmt::Write as _;
use std::path::PathBuf;

use hunter::backends::omp_scavenge::harness;
use hunter::config::Config;
use hunter::types::RunResult;
use hunter::workspace::Workspace;
use support::{FakeBins, TempDir};

const PROMPT: &str = "hunt the bug";

/// Pipe capacity on Linux is 64 KiB; a worker writing more than that
/// blocks until someone reads, which is what `drain_tail` exists to do.
const PIPE_BUFFER: usize = 64 * 1024;

/// `run_worker` keeps this many bytes of worker output.
const TAIL_BYTES: usize = 2000;

/// Characters per `emit_sh` chunk.
const CHUNK_CHARS: usize = 1024;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A scripted `omp` on `PATH`, a sandboxed `$OMP_HOME`, and a chain
/// workspace whose tree the worker runs in.
///
/// Field order is drop order: `OMP_HOME` is restored first, then `PATH`,
/// then the scratch directory goes away. There is no lock: environment
/// mutation is safe only because nextest runs each test in its own
/// process, which `FakeBins::acquire` asserts before anything is touched.
/// Under plain `cargo test` that assertion fails rather than racing.
struct Fixture {
    _home: support::EnvGuard,
    bins: FakeBins,
    _dir: TempDir,
    ws: Workspace,
    cfg: Config,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let bins = FakeBins::acquire(label); // asserts per-test process isolation
        let dir = TempDir::new(label);
        let omp_home = dir.subdir("omp-home");
        std::fs::create_dir_all(omp_home.join("agent/sessions")).expect("create sessions dir");
        // Scoped through FakeBins: acquiring one is what checked that this
        // test owns its process, which is what licenses mutating the
        // environment at all.
        let home = bins.env("OMP_HOME", &omp_home);

        // No config.json under a scratch root → every default, including
        // `ompBin = "omp"`. Only the meter tick is overridden: the 2 s
        // production poll would make each of these tests a 2 s test.
        let mut cfg = support::load_config(dir.path());
        cfg.poll_s = 0.02;
        let ws = Workspace::for_chain(&cfg.work_root, &dir.join("clone"), 1);
        std::fs::create_dir_all(&ws.tree).expect("create the chain's tree");

        Self {
            _home: home,
            bins,
            _dir: dir,
            ws,
            cfg,
        }
    }

    fn run(&self, cap_tokens: Option<i64>, max_wall_s: i64) -> RunResult {
        harness::run_worker(
            &self.cfg, &self.ws, PROMPT, cap_tokens, max_wall_s, None, None,
        )
    }

    /// Continue `session_file` instead of starting cold.
    fn resume(&self, session_file: &std::path::Path) -> RunResult {
        harness::run_worker(
            &self.cfg,
            &self.ws,
            PROMPT,
            Some(1_000_000),
            30,
            None,
            Some(session_file),
        )
    }
}

/// Shell that writes an omp-style session ledger, one assistant record per
/// `(input, output, cacheWrite)` triple.
///
/// Target resolution mirrors what omp actually does (measured against
/// v18.2.6): with `--resume=<path>` the records are appended to that file
/// and `--session-dir` does not move them; without one they go into the
/// private session directory this run was handed. Resolving both from
/// argv is the point — a worker that ignored the flags would write where
/// nothing looks.
fn ledger_sh(records: &[(i64, i64, i64)]) -> String {
    // fd 0 must be /dev/null. Inherited, an open pipe makes the real omp
    // announce "Reading prompt from piped stdin (waiting for EOF)" and
    // block before the session exists, so the worker writes no ledger
    // and dies unmetered with its wall-clock slot spent. Asserted on the
    // spawned process rather than by reading the builder, because what
    // matters is the descriptor the child actually gets.
    let mut sh = "test \"$(readlink /proc/$$/fd/0)\" = /dev/null || exit 93\n\
                  SDIR=\"\"\nRESUME=\"\"\nfor a in \"$@\"; do\n  case \"$a\" in \
                  --session-dir=*) SDIR=\"${a#--session-dir=}\";; \
                  --resume=*) RESUME=\"${a#--resume=}\";; esac\ndone\n\
                  mkdir -p \"$SDIR\"\nLEDGER=\"${RESUME:-$SDIR/session-01.jsonl}\"\n\
                  TS=$(date -u '+%Y-%m-%dT%H:%M:%S.000Z')\n"
        .to_owned();
    for &(input, output, cache_write) in records {
        let record = serde_json::json!({
            "timestamp": "@TS@",
            "message": {
                "role": "assistant",
                "usage": {
                    "input": input,
                    "output": output,
                    "cacheWrite": cache_write,
                },
            },
        })
        .to_string()
        .replace("@TS@", "%s");
        let _ = writeln!(sh, "printf '{record}\\n' \"$TS\" >> \"$LEDGER\"");
    }
    sh
}

/// Shell emitting `chunks` repeats of `CHUNK_CHARS` copies of `ch` on
/// stdout (`fd` 1) or stderr (`fd` 2). A shell loop over `printf`, so
/// nothing depends on `dd` or a `head` that understands `-c`.
fn emit_sh(ch: char, chunks: usize, fd: u8) -> String {
    let chunk: String = std::iter::repeat_n(ch, CHUNK_CHARS).collect();
    let redirect = if fd == 2 { " >&2" } else { "" };
    format!(
        "i=0\nwhile [ $i -lt {chunks} ]; do printf '%s' '{chunk}'{redirect}; i=$((i+1)); done\n"
    )
}

// ---------------------------------------------------------------------------
// Metering
// ---------------------------------------------------------------------------

/// The happy path, so every "was flagged" test below is known to be
/// asserting on a difference rather than on a harness that always flags.
#[test]
fn metered_worker_sums_the_ledger_and_is_not_flagged() {
    let fx = Fixture::new("metered");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}printf 'worker-done\\n'\nexit 0",
            ledger = ledger_sh(&[(1000, 200, 50), (3000, 400, 350)])
        ),
    );

    let res = harness::run_worker(
        &fx.cfg,
        &fx.ws,
        PROMPT,
        Some(1_000_000),
        30,
        Some("test-model"),
        None,
    );

    assert_eq!(res.killed_reason, None, "a metered run carries no flag");
    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.tokens_new, 5000, "(1000+200+50) + (3000+400+350)");
    assert_eq!(res.calls, 2);
    assert!(
        res.session_file
            .as_deref()
            .is_some_and(|f| f.ends_with("session-01.jsonl")),
        "ledger path recorded, got {:?}",
        res.session_file
    );
    assert!(res.stdout_tail.contains("worker-done"));

    // The worker is spawned with the prompt, the requested model, and its
    // chain's session directory — the flag that stops omp's `autoResume`
    // from continuing this cwd's previous session.
    let calls = fx.bins.calls_to("omp");
    let argv = calls.first().expect("omp was invoked");
    assert_eq!(calls.len(), 1);
    assert_eq!(argv[..3], ["omp", "-p", PROMPT]);
    assert!(
        argv.contains(&"--model=test-model".to_owned()),
        "got {argv:?}"
    );
    let sdir = argv
        .iter()
        .find_map(|a| a.strip_prefix("--session-dir="))
        .expect("worker handed a session dir");
    assert_eq!(
        std::path::Path::new(sdir),
        fx.ws.session,
        "worker transcripts belong in the chain's session directory, not in \
         the operator's own omp session tree"
    );
}

/// The regression a review round found: the worker exited 0, promptly, and
/// left no ledger. Zero tokens here is "unknown", and recording it as a
/// clean success both under-counts the window and feeds a false zero into
/// the `anticipated_tokens` percentiles.
#[test]
fn worker_without_a_ledger_is_flagged_unmetered_despite_exit_zero() {
    let fx = Fixture::new("unmetered");
    fx.bins.script("omp", "printf 'nothing metered\\n'\nexit 0");

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("unmetered"),
        "a run nobody could meter is not a clean success"
    );
    // Exit 0 with no flag is the only shape `scheduler::job_state` calls
    // Done; the flag above is what keeps this run out of that bucket.
    assert_eq!(res.exit_code, Some(0), "the worker really did exit cleanly");
    assert_eq!(res.tokens_new, 0);
    assert_eq!(res.calls, 0);
    assert_eq!(res.session_file, None);
}

// ---------------------------------------------------------------------------
// Kill reasons
// ---------------------------------------------------------------------------

/// "unmetered" is the weakest of the three reasons: it only says the spend
/// is unknown. `wallclock` says *why* the worker died and is what the
/// operator needs to see, so the later unmetered check must not overwrite
/// it — and this is the one combination where both fire, since a `cap`
/// kill can only happen once a ledger has been found.
#[test]
fn an_existing_kill_reason_survives_the_unmetered_check() {
    let fx = Fixture::new("wallclock-wins");
    fx.bins.script("omp", "sleep 10");

    let res = fx.run(Some(1_000_000), 1);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("wallclock"),
        "the specific reason must not be clobbered by the generic one"
    );
    assert_eq!(
        res.session_file, None,
        "no ledger — so the unmetered branch really was reached"
    );
    assert!(
        res.duration_s < 5.0,
        "killed at the wall limit, not after the worker's own sleep: {}s",
        res.duration_s
    );
}

/// The cap watchdog, end to end: the ledger crosses the threshold while
/// the worker is still running and the process group is killed.
#[test]
fn cap_kill_fires_while_the_worker_is_still_running() {
    let fx = Fixture::new("cap-kill");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}sleep 10",
            ledger = ledger_sh(&[(400_000, 50_000, 50_000)])
        ),
    );

    let res = fx.run(Some(1_000), 30);

    assert_eq!(res.killed_reason.as_deref(), Some("cap"));
    assert_eq!(res.tokens_new, 500_000, "metered before the kill");
    assert_ne!(res.exit_code, Some(0), "a capped worker did not succeed");
    assert!(
        res.duration_s < 5.0,
        "killed on crossing the cap, not after the worker's own sleep: {}s",
        res.duration_s
    );
}

/// No cap means no token bound — not a very large one.
///
/// The budget ramp decides how much headroom a job gets, and it can
/// legitimately decide there is no ceiling to impose. There used to be a
/// config constant underneath it (`hunt.capNewTokens` / `fix.capNewTokens`)
/// that the scheduler substituted whenever the ramp granted no ceiling, so
/// "unbounded" silently meant "bounded at whatever someone typed into
/// config.json" — a number that had drifted below what the jobs it
/// governed cost, and killed them on its own.
///
/// The same ledger as the cap test above, an order of magnitude past any
/// threshold it could plausibly be compared with. What stops this worker
/// is `max_wall_s`, which is the point: dropping the token bound does not
/// leave a runaway worker with nothing to stop it.
#[test]
fn no_cap_means_no_token_bound_only_the_wall_clock() {
    let fx = Fixture::new("cap-none");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}sleep 10",
            ledger = ledger_sh(&[(400_000, 50_000, 50_000)])
        ),
    );

    let res = fx.run(None, 1);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("wallclock"),
        "500 000 tokens under no token bound is not a cap kill"
    );
    assert_eq!(res.tokens_new, 500_000, "still metered, just not bounded");
    assert!(
        res.duration_s < 5.0,
        "the wall limit still stops it, well before the worker's own \
         sleep: {}s",
        res.duration_s
    );
}

/// A missing `omp` is a failure, not a free run: exit 127 keeps it out of
/// `JobState::Done` even though `killed_reason` is None and no tokens were
/// counted.
#[test]
fn missing_omp_binary_is_a_failure_not_a_free_run() {
    let fx = Fixture::new("no-omp");
    // Nothing scripted, and the real PATH dropped: otherwise a developer
    // with omp installed spawns an actual worker from this test.
    fx.bins.isolate();

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(res.exit_code, Some(127));
    assert_ne!(res.exit_code, Some(0), "never a clean success");
    assert_eq!(res.tokens_new, 0);
    assert!(
        res.stdout_tail.contains("spawn error"),
        "got {:?}",
        res.stdout_tail
    );
}

// ---------------------------------------------------------------------------
// Output capture
// ---------------------------------------------------------------------------

/// Both pipes have to be drained *while* the worker runs. A worker that
/// writes past the 64 KiB pipe buffer blocks in `write()` until someone
/// reads, and a blocked worker never exits and never writes another ledger
/// line — the cap watchdog goes blind and the run dies on the wall limit.
///
/// The script fills stdout first and only then writes stderr, so seeing
/// the stderr marker proves the stdout pipe was being drained too.
#[test]
fn oversized_output_on_both_pipes_does_not_deadlock() {
    let fx = Fixture::new("big-output");
    let chunks = PIPE_BUFFER * 2 / CHUNK_CHARS;
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}{out}printf 'STDOUT-END\\n'\n{err}printf 'STDERR-END\\n' >&2\nexit 0",
            ledger = ledger_sh(&[(4000, 200, 42)]),
            out = emit_sh('a', chunks, 1),
            err = emit_sh('b', chunks, 2),
        ),
    );

    let res = fx.run(Some(1_000_000), 5);

    assert_eq!(
        res.killed_reason, None,
        "a deadlocked worker dies on the wall limit instead"
    );
    assert_eq!(res.exit_code, Some(0));
    assert_eq!(res.tokens_new, 4242, "still metered through all that noise");
    assert!(
        res.stdout_tail.ends_with("STDERR-END\n"),
        "both pipes drained to EOF; tail ended {:?}",
        &res.stdout_tail[res.stdout_tail.len().saturating_sub(40)..]
    );
    assert!(
        res.stdout_tail.len() <= TAIL_BYTES,
        "{} bytes kept of a {}-byte worker",
        res.stdout_tail.len(),
        chunks * CHUNK_CHARS * 2
    );
}

/// A worker that emits bytes that are not UTF-8 (a truncated build log, a
/// binary blob echoed by mistake) still has usable output: the bad bytes
/// become replacement characters and everything around them survives.
/// Dropping the capture would throw away the only diagnostic a killed run
/// leaves behind.
#[test]
fn invalid_utf8_output_is_kept_lossily() {
    let fx = Fixture::new("bad-utf8");
    fx.bins.script(
        "omp",
        "printf 'START-'\nprintf '\\377\\376\\375\\300'\nprintf -- '-END\\n'\nexit 0",
    );

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(res.exit_code, Some(0));
    assert!(
        res.stdout_tail.starts_with("START-"),
        "output before the bad bytes survives: {:?}",
        res.stdout_tail
    );
    assert!(
        res.stdout_tail.ends_with("-END\n"),
        "output after the bad bytes survives: {:?}",
        res.stdout_tail
    );
    assert!(
        res.stdout_tail.contains('\u{FFFD}'),
        "bad bytes replaced, not dropped: {:?}",
        res.stdout_tail
    );
}

/// The tail is a byte window over a byte stream, so its left edge lands
/// mid-character whenever the worker speaks anything but ASCII — here the
/// 2000-byte cut falls inside a 3-byte `€`. Slicing there panics, and a
/// panic in the harness loses the whole run, not just its output.
#[test]
fn output_cut_mid_multibyte_char_is_not_a_panic() {
    let fx = Fixture::new("mid-char");
    // One chunk of '€' is 3072 bytes, so the 2000-byte window cuts into
    // it — and 2000 - "END\n" = 1996 is not a multiple of 3, so the cut
    // lands on a continuation byte rather than a character start.
    fx.bins.script(
        "omp",
        &format!(
            "{euros}printf 'END\\n'\nexit 0",
            euros = emit_sh('\u{20AC}', 1, 1)
        ),
    );

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(res.exit_code, Some(0));
    assert!(res.stdout_tail.ends_with("END\n"));
    assert!(
        res.stdout_tail.len() <= TAIL_BYTES,
        "kept {} bytes",
        res.stdout_tail.len()
    );
    assert!(
        res.stdout_tail
            .trim_end_matches("END\n")
            .chars()
            .all(|c| c == '€'),
        "the boundary snap must not mangle the characters it keeps: {:?}",
        res.stdout_tail.chars().take(8).collect::<String>()
    );
}

/// The sibling of `worker_without_a_ledger_is_flagged_unmetered_despite_exit_zero`:
/// a worker that never exits and never produces a ledger.
///
/// This is the branch the cap exists for — a worker burning tokens that
/// nothing can measure. It was untestable while the grace period was a
/// hardcoded 120 s constant; `sessionGraceS` makes it injectable, exactly
/// as `maxWallS` already is for the wall-clock branch in the same loop.
/// Shortening the interval is the only honest option here: the sleep is
/// `std::thread::sleep` on a blocking thread, so `tokio::time::pause()`
/// has no effect on it.
#[test]
fn hanging_worker_without_a_ledger_is_killed_as_unmetered() {
    let mut fx = Fixture::new("unmetered-hang");
    fx.cfg.session_grace_s = 1;
    // Sleeps well past the grace period and writes no ledger.
    fx.bins.script("omp", "sleep 30");

    let started = std::time::Instant::now();
    // max_wall_s is far larger, so a pass cannot be the wall-clock branch.
    let res = fx.run(Some(1_000_000), 300);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("unmetered"),
        "a worker that never produced a ledger must be killed, not left running"
    );
    assert_eq!(res.tokens_new, 0);
    assert!(
        started.elapsed().as_secs() < 20,
        "must be killed at the grace period, not left to run: {:?}",
        started.elapsed()
    );
}

/// A worker that exits while leaving a descendant behind must not hang the
/// harness.
///
/// This is the normal-exit path: the loop breaks the moment `try_wait`
/// reports `omp` gone, so no deadline is in force any more — `max_wall_s`
/// has already been left behind. The reader threads drain to EOF, and a
/// descendant that inherited stdout and stderr holds those pipes open, so
/// the join never returns. `omp` spawning subagents, tools and git makes
/// this the most likely place in the daemon to meet one.
#[test]
fn worker_leaving_a_descendant_behind_does_not_hang_the_harness() {
    let fx = Fixture::new("lingering");
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}sleep 30 &\nprintf 'worker-done\\n'\nexit 0",
            ledger = ledger_sh(&[(1000, 200, 50)])
        ),
    );

    let t0 = std::time::Instant::now();
    let res = harness::run_worker(&fx.cfg, &fx.ws, PROMPT, Some(1_000_000), 30, None, None);
    let elapsed = t0.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "run_worker blocked on a descendant holding the pipes: {elapsed:?}"
    );
    assert_eq!(res.exit_code, Some(0), "the worker itself exited cleanly");
    assert!(
        res.stdout_tail.contains("worker-done"),
        "output captured before the group was reaped: {:?}",
        res.stdout_tail
    );
}

// ---------------------------------------------------------------------------
// Build cache
// ---------------------------------------------------------------------------

/// A worker compiles into its repo clone's `target/`, not into its tree.
///
/// Every chain starts from a fresh tree, so cargo's default `target/`
/// beside the sources would be empty every time: a full cold compile
/// inside the job's wall-clock limit, and up to a gigabyte of disk per
/// live chain. The clone's own `target/` is already warm and gitignored.
/// Unset first because the test runner may itself have been started with
/// a `CARGO_TARGET_DIR`, which the harness deliberately passes through.
#[test]
fn a_worker_builds_into_its_clones_shared_cargo_target() {
    let fx = Fixture::new("cargo-target");
    let _unset = fx.bins.unset_env("CARGO_TARGET_DIR");
    let seen = fx.ws.tree.join("cargo-target-dir");
    fx.bins.script(
        "omp",
        &format!(
            "printf '%s' \"$CARGO_TARGET_DIR\" > '{seen}'\n{ledger}exit 0",
            seen = seen.display(),
            ledger = ledger_sh(&[(1000, 200, 50)])
        ),
    );

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(res.killed_reason, None, "{:?}", res.stdout_tail);
    assert_eq!(
        std::fs::read_to_string(&seen).expect("the worker recorded its environment"),
        fx.ws.clone.join("target").to_string_lossy(),
    );
}

// ---------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------

/// Without a resume source nothing changes: a private, empty session
/// directory and no `--resume`.
///
/// This is the branch every job took before resume existed, and it is the
/// one that costs a whole session floor if it silently stops being cold.
/// The absent flag is the assertion: `--resume` pointing anywhere at all
/// hands the worker a transcript to re-cache.
#[test]
fn a_run_without_a_resume_source_starts_cold() {
    let fx = Fixture::new("resume-absent");
    fx.bins.script(
        "omp",
        &format!("{ledger}exit 0", ledger = ledger_sh(&[(1000, 200, 50)])),
    );

    let res = fx.run(Some(1_000_000), 30);

    assert_eq!(res.killed_reason, None);
    assert_eq!(res.tokens_new, 1250);
    let calls = fx.bins.calls_to("omp");
    let argv = calls.first().expect("omp was invoked");
    assert!(
        !argv.iter().any(|a| a.starts_with("--resume")),
        "a cold run must name no session to continue, got {argv:?}"
    );
    let sdir = argv
        .iter()
        .find_map(|a| a.strip_prefix("--session-dir="))
        .expect("worker handed a session dir");
    assert_eq!(std::path::Path::new(sdir), fx.ws.session);
}

/// A resume names the transcript to continue, by path, and runs in the
/// directory that holds it.
///
/// Both flags, and both pointing at the predecessor: omp writes the
/// continued session at the `--resume` path and `--session-dir` does not
/// move it, so a run directory anywhere else would simply be empty and the
/// run would be killed as unmetered. Naming the path is also what
/// distinguishes this from `autoResume`, which continues whatever session
/// is newest for the cwd.
#[test]
fn a_resume_source_is_continued_by_explicit_path() {
    let fx = Fixture::new("resume-argv");
    fx.bins.script(
        "omp",
        &format!("{ledger}exit 0", ledger = ledger_sh(&[(1000, 200, 50)])),
    );

    let first = fx.run(Some(1_000_000), 30);
    let ledger = PathBuf::from(first.session_file.expect("first run wrote a ledger"));
    let second = fx.resume(&ledger);

    assert_eq!(second.killed_reason, None, "{:?}", second.stdout_tail);
    let calls = fx.bins.calls_to("omp");
    let argv = calls.get(1).expect("omp was invoked twice");
    assert!(
        argv.contains(&format!("--resume={}", ledger.display())),
        "got {argv:?}"
    );
    assert!(
        argv.contains(&format!(
            "--session-dir={}",
            ledger.parent().expect("ledger has a parent").display()
        )),
        "the resumed run belongs in the directory holding that transcript, \
         got {argv:?}"
    );
    assert_eq!(
        second.session_file.as_deref(),
        Some(ledger.to_string_lossy().as_ref()),
        "a resumed attempt continues one transcript, it does not open another"
    );
}

/// The attribution invariant: a resumed attempt is billed for what IT
/// added, never for the transcript it inherited.
///
/// `ledger_dir_usage` sums the whole run directory, and a resumed worker
/// appends to the predecessor's file in that same directory — so without a
/// pre-spawn baseline every link of a resume chain re-bills its own
/// history. That would double-count the window, and because `tokens_new`
/// feeds the `anticipated_tokens` percentiles it would also inflate the
/// estimate for every later job of the same kind.
#[test]
fn a_resumed_run_meters_only_what_this_attempt_added() {
    let fx = Fixture::new("resume-metering");
    fx.bins.script(
        "omp",
        &format!("{ledger}exit 0", ledger = ledger_sh(&[(1000, 200, 50)])),
    );
    let first = fx.run(Some(1_000_000), 30);
    assert_eq!(first.tokens_new, 1250);
    let ledger = PathBuf::from(first.session_file.expect("first run wrote a ledger"));

    // The resumed attempt appends two calls to that same file.
    fx.bins.script(
        "omp",
        &format!(
            "{ledger}exit 0",
            ledger = ledger_sh(&[(2000, 300, 100), (400, 50, 10)])
        ),
    );
    let second = fx.resume(&ledger);

    assert_eq!(
        second.tokens_new, 2860,
        "(2000+300+100) + (400+50+10); the predecessor's 1250 is already \
         billed to its own job row"
    );
    assert_eq!(second.calls, 2, "two new calls, not the transcript's three");
    // The inherited history is still on disk — the subtraction is an
    // attribution rule, not a truncation of the transcript.
    let (total_tokens, total_calls) = harness::ledger_usage(&ledger);
    assert_eq!((total_tokens, total_calls), (4110, 3));
}

/// A resume source that is gone is refused, not quietly restarted.
///
/// omp does not help here: an unresolvable `--resume` path makes it start
/// a fresh session, write it at that path, and exit 0. From the outside
/// that is indistinguishable from a successful continuation, so the daemon
/// would pay a full session floor and redo the work while believing it had
/// resumed. Whether to start clean is the scheduler's call, so the harness
/// hands the decision back instead of making it invisibly.
#[test]
fn a_missing_resume_source_is_refused_rather_than_restarted() {
    let fx = Fixture::new("resume-missing");
    fx.bins.script("omp", "printf 'should not run\\n'\nexit 0");
    let gone = fx.ws.session.join("session-01.jsonl");

    let res = fx.resume(&gone);

    assert_eq!(
        res.killed_reason.as_deref(),
        Some("resume-unavailable"),
        "the caller asked for a continuation and must be told it cannot happen"
    );
    assert_eq!(res.exit_code, None, "nothing was spawned");
    assert_eq!(res.tokens_new, 0);
    assert!(
        fx.bins.calls_to("omp").is_empty(),
        "a refused resume must not spend a session floor finding out"
    );
}

/// A resume that silently did not take is reported.
///
/// The source existed, so the pre-check passed, but omp began a new
/// session over it anyway — a corrupt or unparseable transcript does this.
/// Exit code and session path are identical to a real continuation; the
/// opening record is not, because a continuation appends and leaves it
/// alone.
#[test]
fn a_resume_that_did_not_take_is_reported() {
    let fx = Fixture::new("resume-lost");
    fx.bins.script(
        "omp",
        &format!("{ledger}exit 0", ledger = ledger_sh(&[(1000, 200, 50)])),
    );
    let first = fx.run(Some(1_000_000), 30);
    let ledger = PathBuf::from(first.session_file.expect("first run wrote a ledger"));

    // Truncate-and-write is exactly what omp does when it decides the
    // named session is not resumable.
    fx.bins.script(
        "omp",
        &format!(
            "RESUME=\"\"\nfor a in \"$@\"; do case \"$a\" in \
             --resume=*) RESUME=\"${{a#--resume=}}\";; esac\ndone\n\
             printf '%s\\n' '{record}' > \"$RESUME\"\nexit 0",
            record = r#"{"message":{"role":"assistant","usage":{"input":900,"output":100,"cacheWrite":0}}}"#
        ),
    );
    let second = fx.resume(&ledger);

    assert!(
        second.stdout_tail.starts_with("[resume-lost]"),
        "a continuation that did not happen must not read as one, got {:?}",
        second.stdout_tail
    );
    assert_eq!(second.tokens_new, 1000, "still metered, just not continued");
}
