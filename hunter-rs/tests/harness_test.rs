#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Harness tests — JSONL ledger parsing and `kill_tree`.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use hunter::backends::omp_scavenge::harness;

mod support;
use support::TempDir;

// ============================================================
// ledger_usage — JSONL parsing
// ============================================================

#[test]
fn test_ledger_usage_sums_tokens() {
    let dir = TempDir::new("lu-basic");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T15:38:24.000Z","message":{"role":"assistant","usage":{"input":200,"output":100,"cacheWrite":50}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file).unwrap();
    assert_eq!(tokens, 525); // (100+50+25) + (200+100+50)
    assert_eq!(calls, 2);
}

#[test]
fn test_ledger_usage_skips_non_assistant() {
    let dir = TempDir::new("lu-role");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"user","usage":{"input":999,"output":999,"cacheWrite":999}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T15:00:01.000Z","message":{"role":"assistant","usage":{"input":10,"output":5,"cacheWrite":0}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let (tokens, calls) = harness::ledger_usage(&file).unwrap();
    // Only the assistant record counts
    assert_eq!(tokens, 15);
    assert_eq!(calls, 1);
}

/// omp appends a record and its newline separately, so the meter can
/// catch the final line half-written. That line is not counted yet, and
/// is not an error.
#[test]
fn test_ledger_usage_tolerates_a_torn_final_line() {
    let dir = TempDir::new("lu-torn");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#,
            "\n",
            r#"{"timestamp":"2026-07-23T15:38:24.000Z","message":{"role":"assis"#,
        ),
    )
    .unwrap();
    assert_eq!(harness::ledger_usage(&file).unwrap(), (175, 1));
}

/// A broken line with records after it is not a write in progress: the
/// file is not what the meter thinks it is, and skipping the line could
/// skip a call.
#[test]
fn test_ledger_usage_rejects_a_malformed_line_mid_file() {
    let dir = TempDir::new("lu-bad");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            "partial{{{json\n",
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#,
            "\n",
        ),
    )
    .unwrap();
    assert!(harness::ledger_usage(&file).is_err());
}

/// Schema drift: a renamed usage field must fail the meter, not count
/// the call at zero tokens — that would disarm the cap and record the
/// run as free.
#[test]
fn test_ledger_usage_rejects_usage_missing_a_token_field() {
    let dir = TempDir::new("lu-drift");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        concat!(
            r#"{"timestamp":"2026-07-23T15:38:23.224Z","message":{"role":"assistant","usage":{"input":100,"output":50,"cache_write":25}}}"#,
            "\n",
        ),
    )
    .unwrap();
    let err = harness::ledger_usage(&file).unwrap_err();
    assert!(format!("{err:#}").contains("cacheWrite"), "got: {err:#}");
}

#[test]
fn test_ledger_usage_missing_file_is_an_error() {
    assert!(
        harness::ledger_usage(&PathBuf::from("/nonexistent/path/does-not-exist.jsonl")).is_err()
    );
}

#[test]
fn test_ledger_usage_empty_file() {
    let dir = TempDir::new("lu-empty");
    let file = dir.join("empty.jsonl");
    fs::write(&file, "").unwrap();
    assert_eq!(harness::ledger_usage(&file).unwrap(), (0, 0));
}

#[test]
fn test_ledger_usage_no_usage_field() {
    let dir = TempDir::new("lu-nousage");
    let file = dir.join("test.jsonl");
    fs::write(
        &file,
        r#"{"timestamp":"2026-07-23T15:00:00.000Z","message":{"role":"assistant","content":"hi"}}"#,
    )
    .unwrap();
    assert_eq!(harness::ledger_usage(&file).unwrap(), (0, 0));
}

// ============================================================
// ledger_dir_usage — "not metered yet" vs "cannot be metered"
// ============================================================

/// A run directory omp has not created yet is the "not metered yet"
/// signal the watchdog waits on, not a metering failure.
#[test]
fn test_ledger_dir_usage_missing_dir_is_not_metered_yet() {
    let dir = TempDir::new("ldu-missing");
    assert!(
        harness::ledger_dir_usage(&dir.join("absent"))
            .unwrap()
            .is_none()
    );
}

/// Only a missing directory means "not metered yet": a path that exists
/// but cannot be listed must fail the meter, not read as zero spend.
#[test]
fn test_ledger_dir_usage_unlistable_dir_is_an_error() {
    let dir = TempDir::new("ldu-notdir");
    let file = dir.join("not-a-dir");
    fs::write(&file, "").unwrap();
    assert!(harness::ledger_dir_usage(&file).is_err());
}

/// omp writes each subagent's transcript in the directory named after the
/// session that spawned it (`<dir>/<stem>.jsonl` +
/// `<dir>/<stem>/<Agent>.jsonl`, and a subagent's own subagents under
/// `<stem>/<Agent>/`). That is spend of the same run, so the meter must
/// count it; the recorded `session_file` stays the top-level transcript a
/// resume continues.
#[test]
fn test_ledger_dir_usage_counts_nested_subagent_transcripts() {
    let dir = TempDir::new("ldu-nested");
    let run = dir.join("run");
    let stem = "2026-11-09T17-41-03-101Z_01a1c166";
    fs::create_dir_all(run.join(stem).join("Recovery")).unwrap();
    let line = |i: i64, o: i64, w: i64| {
        format!(
            "{{\"message\":{{\"role\":\"assistant\",\"usage\":{{\"input\":{i},\"output\":{o},\"cacheWrite\":{w}}}}}}}\n"
        )
    };
    let top = run.join(format!("{stem}.jsonl"));
    fs::write(&top, line(100, 50, 25)).unwrap();
    fs::write(
        run.join(stem).join("Recovery.jsonl"),
        line(1000, 200, 100) + &line(10, 5, 0),
    )
    .unwrap();
    fs::write(
        run.join(stem).join("Recovery").join("Nested.jsonl"),
        line(1, 1, 1),
    )
    .unwrap();
    // Not a ledger: sibling artifacts omp keeps next to subagent transcripts.
    fs::write(run.join(stem).join("Recovery.md"), "notes").unwrap();

    let (session_file, tokens, calls) = harness::ledger_dir_usage(&run).unwrap().unwrap();
    assert_eq!(tokens, 175 + 1300 + 15 + 3);
    assert_eq!(calls, 4);
    assert_eq!(
        session_file, top,
        "the run's own transcript, not a subagent's"
    );
}

/// `<stem>/local/` is the worker's `local://` scratch root, not a
/// transcript directory: whatever `*.jsonl` the agent writes there —
/// findings notes, a copied transcript — is neither a ledger nor this
/// run's spend. Reading it would kill the worker as unmetered on a
/// non-ledger line, or bill a copy as calls that never happened.
#[test]
fn test_ledger_dir_usage_ignores_scratch_jsonl() {
    let dir = TempDir::new("ldu-scratch");
    let run = dir.join("run");
    let stem = "2026-11-09T17-41-03-101Z_01a1c166";
    let local = run.join(stem).join("local");
    fs::create_dir_all(&local).unwrap();
    let assistant =
        r#"{"message":{"role":"assistant","usage":{"input":100,"output":50,"cacheWrite":25}}}"#;
    let top = run.join(format!("{stem}.jsonl"));
    fs::write(&top, format!("{assistant}\n")).unwrap();
    fs::write(
        local.join("notes.jsonl"),
        "{\"file\":\"a.rs\",\"message\":\"unwrap on None\"}\nnot json\n",
    )
    .unwrap();
    fs::write(
        local.join("copy.jsonl"),
        r#"{"message":{"role":"assistant","usage":{"input":100000,"output":0,"cacheWrite":0}}}"#,
    )
    .unwrap();

    assert_eq!(
        harness::ledger_dir_usage(&run).unwrap(),
        Some((top, 175, 1))
    );
}

// ============================================================
// kill_tree — spawn a sleep, verify it dies
// ============================================================

#[test]
fn test_kill_tree_terminates_process() {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("sleep")
            .arg("999")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");

        // Still running
        assert!(child.try_wait().unwrap().is_none());

        hunter::util::kill_tree(&mut child);

        // After kill_tree, wait() should return immediately (cached status)
        let status = child.wait().expect("wait after kill_tree");
        assert!(!status.success(), "process should have been killed");
    }
}

#[test]
fn test_kill_tree_already_exited() {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        let mut child = Command::new("true")
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn true");

        // Wait for it to finish naturally
        child.wait().expect("wait");

        // kill_tree on an already-exited process should not panic
        hunter::util::kill_tree(&mut child);
    }
}
