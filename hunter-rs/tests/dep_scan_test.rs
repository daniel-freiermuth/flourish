#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `dep_scan::scan_repo` — the Renovate gate.
//!
//! The invariant a review round installed here: an unsuccessful exit means
//! "unavailable", *whatever* the run printed. The original gate only bailed
//! when the output was **also** empty, so a run that died halfway but had
//! already logged a `packageFiles with updates` line was treated as
//! authoritative — and a truncated lookup silently became the repo's full
//! set of dependency findings.
//!
//! The other half of the contract is the `Some(vec![])` / `None`
//! distinction: `None` tells the caller to fall back to the model, which
//! costs a whole AI run, so "renovate ran and found nothing to update" must
//! never collapse into it.

mod support;

use std::path::Path;

use hunter::dep_scan::{github_token, scan_repo};
use hunter::domain::{ForgeName, Severity};
use support::{FakeBins, TempDir};

const REPO: &str = "acme/widget";

/// One line of Renovate's `LOG_FORMAT=json` debug stream, carrying a single
/// npm update. This is the shape `parse_renovate_output` mines.
const UPDATE_LINE: &str = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"^1.2.0","datasource":"npm","updates":[{"newVersion":"1.3.0","updateType":"minor"}]}]}]}}"#;

/// The same shape, but every dep is already current: `updates` is empty.
const NO_UPDATE_LINE: &str = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;

/// Chatter Renovate emits around the useful lines.
const CHATTER: &str = r#"{"name":"renovate","level":30,"msg":"Repository started"}"#;

/// Assert the fake `renovate` was actually reached with its argv, so a
/// `None` in the test below is a decision about the *result* and not the
/// scanner quietly failing to spawn anything.
fn assert_renovate_invoked(bins: &FakeBins) {
    let calls = bins.calls_to("renovate");
    assert_eq!(
        calls.len(),
        1,
        "expected exactly one renovate call: {calls:?}"
    );
    assert_eq!(
        calls[0],
        vec![
            "renovate".to_owned(),
            "--platform=local".to_owned(),
            "--dry-run=lookup".to_owned(),
        ],
    );
}

#[test]
fn success_with_candidates_parses_them() {
    let bins = FakeBins::acquire("depscan-ok");
    let work = TempDir::new("depscan-ok-repo");
    bins.ok("renovate", &format!("{CHATTER}\n{UPDATE_LINE}"));

    let got = scan_repo(work.path(), REPO, None, 30)
        .expect("renovate succeeded, so Some")
        .candidates;

    assert_renovate_invoked(&bins);
    assert_eq!(got.len(), 1, "one dep, one update: {got:?}");
    let c = &got[0];
    assert_eq!(c.package, "left-pad");
    assert_eq!(c.file, "package.json");
    assert_eq!(c.ecosystem, "npm");
    // Without a resolved version, the range stands in for it.
    assert_eq!(c.current_version, "1.2.0");
    assert_eq!(c.latest_version, "1.3.0");
    assert_eq!(c.update_type, "minor");
    assert_eq!(c.severity, Severity::Medium);
    // Where the dependency stands, never where it could go: a later release
    // must land on this same finding.
    assert_eq!(c.fingerprint, "acme/widget:dep:left-pad-non-major@1.2.0");
}

/// `Some(vec![])` is a real answer: renovate ran, everything is current.
/// Folding it into `None` would send the caller to the model for a repo it
/// has already been told has nothing to do.
#[test]
fn success_with_no_updates_is_empty_not_none() {
    let bins = FakeBins::acquire("depscan-empty");
    let work = TempDir::new("depscan-empty-repo");
    bins.ok("renovate", &format!("{CHATTER}\n{NO_UPDATE_LINE}"));

    let got = scan_repo(work.path(), REPO, None, 30);

    assert_renovate_invoked(&bins);
    assert!(
        matches!(&got, Some(v) if v.candidates.is_empty()),
        "up-to-date repo must be Some(empty), not a model fallback: {got:?}"
    );
}

/// A Renovate that answered within the timeout is not a timeout because a
/// helper it left is still finishing its SIGTERM cleanup: the scan ended
/// when Renovate did, not when its namespace does.
#[test]
fn a_renovate_that_ended_in_time_is_not_timed_out_by_what_it_left() {
    let bins = FakeBins::acquire("depscan-leftover");
    let work = TempDir::new("depscan-leftover-repo");
    bins.script(
        "renovate",
        &format!(
            "sh -c 'trap \"sleep 2; exit 0\" TERM; touch trapping; while :; do sleep 0.1; done' &\n\
             while [ ! -e trapping ]; do sleep 0.01; done\n\
             printf '%s\\n' '{CHATTER}' '{UPDATE_LINE}'\nexit 0"
        ),
    );

    let got = scan_repo(work.path(), REPO, None, 1);

    assert_renovate_invoked(&bins);
    assert_eq!(got.map(|scan| scan.candidates.len()), Some(1));
}

/// Scan a repo whose Renovate run printed `line` and exited 0.
fn scan_printing(label: &str, line: &str) -> Option<hunter::dep_scan::DepScan> {
    let bins = FakeBins::acquire(label);
    let work = TempDir::new(label);
    bins.ok("renovate", &format!("{CHATTER}\n{line}"));
    let got = scan_repo(work.path(), REPO, None, 30);
    assert_renovate_invoked(&bins);
    got
}

/// Renovate exits 0 and reports `{}` for a repo it finds no dependency in
/// (observed with 44.96.2). That is not "everything is current":
/// Renovate has no answer, so the caller must fall back to the model.
#[test]
fn no_dependencies_found_is_unavailable() {
    let got = scan_printing(
        "depscan-no-deps",
        r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{}}"#,
    );
    assert!(got.is_none(), "{got:?}");
}

/// Deps Renovate could not look up carry a `skipReason`; a repo made only
/// of those has had nothing checked.
#[test]
fn only_skipped_dependencies_is_unavailable() {
    let got = scan_printing(
        "depscan-all-skipped",
        r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"github-actions":[{"packageFile":".github/workflows/ci.yml","deps":[{"depName":"actions/checkout","currentValue":"v4","datasource":"github-tags","skipReason":"github-token-required","updates":[]}]}]}}"#,
    );
    assert!(got.is_none(), "{got:?}");
}

/// The subtle one: a failed lookup that still printed a parseable
/// `packageFiles with updates` line. The gate has to ask "did it
/// succeed?", not "did it produce output?" — otherwise a half-finished
/// scan is recorded as an authoritative empty result.
#[test]
fn failure_with_output_is_unavailable() {
    let bins = FakeBins::acquire("depscan-fail-loud");
    let work = TempDir::new("depscan-fail-loud-repo");
    // Output on *both* pipes, and the stdout half is exactly the payload
    // the happy-path test above turns into a candidate.
    bins.script(
        "renovate",
        &format!(
            "cat <<'__RENO__'\n{CHATTER}\n{UPDATE_LINE}\n__RENO__\n\
             echo 'FATAL: registry lookup aborted' >&2\nexit 1"
        ),
    );

    let got = scan_repo(work.path(), REPO, None, 30);

    assert_renovate_invoked(&bins);
    assert!(
        got.is_none(),
        "a non-zero exit is untrustworthy however much it logged: {got:?}"
    );
}

/// Same, with nothing on either pipe — the case the old gate did catch.
/// Kept so a fix that swaps one branch for the other is still caught.
#[test]
fn failure_without_output_is_unavailable() {
    let bins = FakeBins::acquire("depscan-fail-quiet");
    let work = TempDir::new("depscan-fail-quiet-repo");
    bins.script("renovate", "exit 3");

    let got = scan_repo(work.path(), REPO, None, 30);

    assert_renovate_invoked(&bins);
    assert!(got.is_none(), "silent failure is unavailable too: {got:?}");
}

/// No `renovate` on `PATH` at all: the scanner reports unavailable rather than
/// unwrapping the spawn error.
#[test]
fn missing_binary_is_unavailable() {
    // `isolate()` drops the real PATH: `FakeBins` normally appends it, so
    // a developer's own renovate would answer and this would pass vacuously.
    let bins = FakeBins::acquire("depscan-missing");
    bins.isolate();
    let work = TempDir::new("depscan-missing-repo");

    let got = scan_repo(work.path(), REPO, None, 30);

    assert!(got.is_none(), "no renovate means fall back to the model");
    assert!(
        bins.calls().is_empty(),
        "nothing should have run: {:?}",
        bins.calls()
    );
}

/// A cwd that does not exist fails at spawn too — the scanner must not
/// treat that as an authoritative "nothing to update".
#[test]
fn unspawnable_scan_is_unavailable() {
    let bins = FakeBins::acquire("depscan-nocwd");
    let work = TempDir::new("depscan-nocwd-repo");
    bins.ok("renovate", UPDATE_LINE);
    let missing = work.join("no-such-checkout");

    let got = scan_repo(&missing, REPO, None, 30);

    assert!(got.is_none(), "spawn failure is unavailable: {got:?}");
}

/// Renovate's stream is not pure JSON — it interleaves plain text, and a
/// killed or buffered writer can leave a half-written line. Unparseable
/// lines are skipped individually; one of them must not discard the
/// findings on the lines around it.
#[test]
fn malformed_lines_do_not_discard_good_ones() {
    let bins = FakeBins::acquire("depscan-malformed");
    let work = TempDir::new("depscan-malformed-repo");
    let truncated = r#"{"name":"renovate","config":{"npm":[{"packageFile":"pack"#;
    let second = r#"{"name":"renovate","config":{"cargo":[{"packageFile":"Cargo.toml","deps":[{"depName":"serde","currentValue":"1.0.1","datasource":"crate","updates":[{"newVersion":"2.0.0","updateType":"major"}]}]}]}}"#;
    bins.ok(
        "renovate",
        &format!(
            "not json at all\n{UPDATE_LINE}\n{truncated}\n\
             {{\"config\": \"a string, not an object\"}}\n{second}"
        ),
    );

    let got = scan_repo(work.path(), REPO, None, 30)
        .expect("renovate succeeded, so Some")
        .candidates;

    assert_renovate_invoked(&bins);
    let packages: Vec<&str> = got.iter().map(|c| c.package.as_str()).collect();
    assert_eq!(
        packages,
        vec!["left-pad", "serde"],
        "a bad line must skip only itself: {got:?}"
    );
    // The line after the truncated one is the one at risk, so pin it whole.
    let serde = &got[1];
    assert_eq!(serde.ecosystem, "crate");
    assert_eq!(serde.update_type, "major");
    assert_eq!(serde.severity, Severity::High);
}

/// The scanner runs Renovate *in the repo*, since `--platform=local`
/// reads whatever is in the cwd. A scan pointed at the wrong directory
/// would report another checkout's dependencies under this repo's name.
#[test]
fn renovate_runs_inside_the_repo_checkout() {
    let bins = FakeBins::acquire("depscan-cwd");
    let work = TempDir::new("depscan-cwd-repo");
    let checkout = work.subdir("checkout");
    let marker = work.join("cwd.txt");
    bins.script(
        "renovate",
        &format!(
            "pwd > '{}'\necho '{NO_UPDATE_LINE}'",
            marker.to_string_lossy()
        ),
    );

    let got = scan_repo(&checkout, REPO, None, 30);

    assert!(got.is_some(), "clean exit is Some: {got:?}");
    let recorded = std::fs::read_to_string(&marker).expect("fake renovate recorded its cwd");
    assert_eq!(
        Path::new(recorded.trim()).canonicalize().ok(),
        checkout.canonicalize().ok(),
        "renovate ran in {recorded:?}, not the checkout"
    );
}

/// One update line with a chosen `updateType` and vulnerability flag.
fn update_line(update_type: &str, vulnerability: bool) -> String {
    let vuln = if vulnerability {
        r#","isVulnerabilityAlert":true"#
    } else {
        ""
    };
    format!(
        r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"npm":[{{"packageFile":"package.json","deps":[{{"depName":"left-pad","currentValue":"1.0.0","datasource":"npm","updates":[{{"newVersion":"2.0.0","updateType":"{update_type}"{vuln}}}]}}]}}]}}}}"#
    )
}

fn single_candidate(
    label: &str,
    update_type: &str,
    vulnerability: bool,
) -> hunter::dep_scan::DepCandidate {
    let bins = FakeBins::acquire(label);
    let work = TempDir::new(label);
    bins.ok("renovate", &update_line(update_type, vulnerability));
    let got = scan_repo(work.path(), REPO, None, 30)
        .expect("renovate succeeded")
        .candidates;
    assert_eq!(got.len(), 1, "expected one candidate: {got:?}");
    got.into_iter().next().unwrap_or_else(|| unreachable!())
}

/// The triage table: `updateType` and `isVulnerabilityAlert` decide the
/// severity and confidence every dep finding is ranked by.
///
/// Added after mutation testing showed the parsing tests pinned only THAT
/// a candidate comes back, not how it is graded — six mutants that
/// rewrote this table survived, including one that downgrades a security
/// advisory to `low`.
#[test]
fn update_type_and_vulnerability_decide_severity() {
    for (ut, vuln, severity) in [
        ("major", false, Severity::High),
        ("major", true, Severity::High),
        ("minor", false, Severity::Medium),
        ("minor", true, Severity::Medium),
        // Neither major nor minor: the advisory flag is the only thing
        // that lifts it off the floor.
        ("patch", false, Severity::Low),
        ("patch", true, Severity::High),
        ("replacement", false, Severity::Low),
        ("replacement", true, Severity::High),
    ] {
        let c = single_candidate(&format!("sev-{ut}-{vuln}"), ut, vuln);
        assert_eq!(
            c.severity, severity,
            "updateType={ut} isVulnerabilityAlert={vuln} should be {severity}"
        );
    }
}

/// Confidence is graded separately from severity: a patch is the safest
/// bump even though it is the lowest severity.
#[test]
fn update_type_decides_confidence() {
    for (ut, confidence) in [
        ("patch", 0.95),
        ("minor", 0.85),
        ("major", 0.7),
        ("replacement", 0.7),
    ] {
        let c = single_candidate(&format!("conf-{ut}"), ut, false);
        assert!(
            (c.confidence - confidence).abs() < f64::EPSILON,
            "updateType={ut} should be {confidence}, got {}",
            c.confidence
        );
    }
}

/// Renovate's housekeeping update types are all reported as `patch`.
/// Leaving them unnormalised would give each its own severity and
/// confidence bucket, and none of them match the table above.
#[test]
fn housekeeping_update_types_normalise_to_patch() {
    for ut in [
        "pin",
        "pinDigest",
        "digest",
        "lockFileMaintenance",
        "lockfileUpdate",
    ] {
        let c = single_candidate(&format!("norm-{ut}"), ut, false);
        assert_eq!(c.update_type, "patch", "{ut} should normalise to patch");
        assert_eq!(c.severity, Severity::Low);
        assert!((c.confidence - 0.95).abs() < f64::EPSILON);
    }
}

/// A `renovate` the scanned repo ships must never run.
///
/// This is what `npx renovate` did: npx resolves from the current
/// directory's `node_modules/.bin` first, and the current directory is the
/// checkout. The repo copy here is also reachable through a relative
/// `PATH` entry and an absolute one pointing into the checkout -- the two
/// other ways the checkout's copy could win -- and the installed one sits
/// behind both, so a scanner that honoured either would run the repo's.
#[test]
fn a_renovate_shipped_by_the_scanned_repo_never_runs() {
    let bins = FakeBins::acquire("depscan-hijack");
    let work = TempDir::new("depscan-hijack-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("renovate");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bins.ok("renovate", NO_UPDATE_LINE);
    let installed = std::env::var("PATH").unwrap();
    let hostile = format!("node_modules/.bin:{}:{installed}", repo_bin.display());
    let _path = bins.env("PATH", Path::new(&hostile));

    let got = scan_repo(work.path(), REPO, None, 30);

    assert!(
        !marker.exists(),
        "the scanned repo's own renovate ran -- repo content executed as the daemon"
    );
    assert_renovate_invoked(&bins);
    assert!(
        matches!(&got, Some(v) if v.candidates.is_empty()),
        "{got:?}"
    );
}

/// When the only `renovate` available is the repo's, there is no scan --
/// and certainly no download: the caller falls back to the model.
#[test]
fn a_repo_shipped_renovate_is_not_a_fallback() {
    let bins = FakeBins::acquire("depscan-only-repo");
    bins.isolate();
    let work = TempDir::new("depscan-only-repo-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("renovate");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path = bins.env("PATH", &repo_bin);

    let got = scan_repo(work.path(), REPO, None, 30);

    assert!(!marker.exists(), "the repo's renovate ran");
    assert!(
        got.is_none(),
        "no installed renovate means fall back: {got:?}"
    );
}

/// Renovate runs with a scrubbed environment: a token the daemon holds is
/// not inherited, while what a Node process needs to start still is.
#[test]
fn renovate_does_not_inherit_the_daemons_secrets() {
    let bins = FakeBins::acquire("depscan-env");
    let work = TempDir::new("depscan-env-repo");
    let seen = work.join("env.txt");
    bins.script(
        "renovate",
        &format!("env > '{}'\necho '{NO_UPDATE_LINE}'", seen.display()),
    );
    let _token = bins.env("GH_TOKEN", Path::new("s3cret-token"));
    let _home = bins.env("HOME", work.path());

    let got = scan_repo(work.path(), REPO, None, 30);

    assert!(got.is_some(), "{got:?}");
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(
        !env.contains("s3cret-token"),
        "GH_TOKEN leaked into renovate:\n{env}"
    );
    assert!(
        env.lines().any(|l| l.starts_with("HOME=")),
        "HOME must be forwarded:\n{env}"
    );
    assert!(env.contains("LOG_FORMAT=json"), "{env}");
    // Grouping (monorepos, one tool in several files) comes from this preset.
    assert!(
        env.lines()
            .any(|l| l == r#"RENOVATE_EXTENDS=["config:recommended"]"#),
        "{env}"
    );
}

/// The `PATH` Renovate itself runs with cannot reach into the checkout.
///
/// The installed `renovate` is a Node script that starts with
/// `#!/usr/bin/env node`, so the child looks `node` up on its own `PATH`,
/// from inside the checkout. A relative entry like `node_modules/.bin`
/// would find a `node` the repo ships. Here the fake renovate calls `node`
/// the same way, and the repo ships a `node` that leaves a marker.
#[test]
fn renovates_own_path_lookups_cannot_reach_the_checkout() {
    let bins = FakeBins::acquire("depscan-child-path");
    let work = TempDir::new("depscan-child-path-repo");
    let marker = work.join("HIJACKED");
    let repo_bin = work.subdir("node_modules/.bin");
    std::fs::create_dir_all(&repo_bin).unwrap();
    let evil = repo_bin.join("node");
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    bins.script(
        "renovate",
        &format!("node --version >/dev/null 2>&1\necho '{NO_UPDATE_LINE}'"),
    );
    let installed = std::env::var("PATH").unwrap();
    let hostile = format!("node_modules/.bin:{installed}");
    let _path = bins.env("PATH", Path::new(&hostile));

    let got = scan_repo(work.path(), REPO, None, 30);

    assert!(got.is_some(), "{got:?}");
    assert!(
        !marker.exists(),
        "renovate's child PATH resolved into the checkout and ran the repo's node"
    );
}

/// The token handed to `scan_repo` is the one Renovate's github.com lookups
/// read. Without it every Action and GitHub-tagged dep is skipped as
/// `github-token-required` while the run still exits 0.
#[test]
fn the_github_token_reaches_renovate() {
    let bins = FakeBins::acquire("depscan-gh-token");
    let work = TempDir::new("depscan-gh-token-repo");
    let seen = work.join("env.txt");
    bins.script(
        "renovate",
        &format!("env > '{}'\necho '{NO_UPDATE_LINE}'", seen.display()),
    );

    let got = scan_repo(work.path(), REPO, Some("ghp_scan"), 30);

    assert!(got.is_some(), "{got:?}");
    let env = std::fs::read_to_string(&seen).unwrap();
    assert!(
        env.lines().any(|l| l == "GITHUB_COM_TOKEN=ghp_scan"),
        "token not exported to renovate:\n{env}"
    );
}

/// A configured token wins over the `gh` login, and `gh` is not even asked.
#[test]
fn configured_token_takes_precedence_over_gh() {
    let bins = FakeBins::acquire("depscan-token-cfg");
    bins.ok("gh", "gho_from_gh");

    let got = github_token(ForgeName::Github, Some("ghp_configured"));

    assert_eq!(got.as_deref(), Some("ghp_configured"));
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
}

/// Without a configured token, a GitHub repo gets the `gh` login for
/// github.com.
#[test]
fn github_repo_falls_back_to_the_gh_login() {
    let bins = FakeBins::acquire("depscan-token-gh");
    bins.ok("gh", "gho_from_gh");

    let got = github_token(ForgeName::Github, None);

    assert_eq!(got.as_deref(), Some("gho_from_gh"));
    assert_eq!(
        bins.calls_to("gh"),
        vec![vec!["gh", "auth", "token", "--hostname", "github.com"]]
    );
}

/// Repos not hosted on GitHub get no token, configured or not.
#[test]
fn non_github_repo_gets_no_token() {
    let bins = FakeBins::acquire("depscan-token-gitlab");
    bins.ok("gh", "gho_from_gh");

    assert_eq!(
        github_token(ForgeName::Gitlab, Some("ghp_configured")),
        None
    );
    assert_eq!(github_token(ForgeName::Gitlab, None), None);
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
}

/// A logged-out `gh` exits non-zero with a message; that message must not
/// be handed to Renovate as a token.
#[test]
fn failed_gh_login_is_no_token() {
    let bins = FakeBins::acquire("depscan-token-nogh");
    bins.fail("gh", 1, "no oauth token found for github.com");

    assert_eq!(github_token(ForgeName::Github, None), None);
}

/// A failed `gh` is no token even when what it printed looks like one:
/// the exit status decides, not the shape of the output.
#[test]
fn failed_gh_exit_is_no_token_whatever_it_printed() {
    let bins = FakeBins::acquire("depscan-token-gh-fail-tokenish");
    bins.script("gh", "echo gho_stale\nexit 1");

    assert_eq!(github_token(ForgeName::Github, None), None);
}

/// `gh` succeeding but adding a notice (stderr is merged in) is not a
/// token: handing Renovate the whole text would fail every lookup.
#[test]
fn gh_output_beyond_one_token_is_no_token() {
    let bins = FakeBins::acquire("depscan-token-gh-notice");
    bins.script(
        "gh",
        "echo gho_from_gh\necho 'A new release of gh is available' >&2\nexit 0",
    );

    assert_eq!(github_token(ForgeName::Github, None), None);
}

/// Renovate's lookup line for [`candidates_follow_renovates_branches`].
fn branches_fixture() -> String {
    let dep = |name: &str, current: &str, updates: serde_json::Value| {
        serde_json::json!({
            "depName": name, "currentValue": format!("^{current}"),
            "currentVersion": current, "datasource": "npm", "updates": updates
        })
    };
    let up = |new: &str, ut: &str, branch: Option<&str>| {
        let mut u = serde_json::json!({"newVersion": new, "updateType": ut});
        if let Some(b) = branch {
            u["branchName"] = serde_json::json!(format!("renovate/{b}"));
        }
        u
    };
    let mut root_deps = serde_json::json!([
        dep(
            "@typescript-eslint/parser",
            "8.64.0",
            serde_json::json!([up("8.71.1", "minor", Some("typescript-eslint-monorepo"))])
        ),
        dep(
            "typescript-eslint",
            "8.64.0",
            serde_json::json!([up("8.71.1", "minor", Some("typescript-eslint-monorepo"))])
        ),
        dep(
            "typescript",
            "6.0.3",
            serde_json::json!([up("7.0.2", "major", Some("typescript-7.x"))])
        ),
        dep(
            "pnpm",
            "11.3.0",
            serde_json::json!([
                up("11.28.5", "minor", Some("pnpm-11.x")),
                up("12.10.1", "major", Some("pnpm-12.x")),
            ])
        ),
        // A group's members need not share an update type; listed weakest
        // first, so the group's type is not simply its first member's.
        dep(
            "js-sys",
            "0.3.103",
            serde_json::json!([up("0.3.106", "patch", Some("wasm-monorepo"))])
        ),
        dep(
            "wasm-bindgen",
            "0.2.126",
            serde_json::json!([up("0.3.0", "minor", Some("wasm-monorepo"))])
        ),
        dep(
            "@types/node",
            "24.13.3",
            serde_json::json!([up("24.19.1", "minor", Some("node"))])
        ),
        dep(
            "node",
            "24.0.0",
            serde_json::json!([up("26.0.0", "major", Some("node"))])
        ),
        // No branch name: the non-breaking and the major update stay apart.
        dep(
            "left-pad",
            "1.2.0",
            serde_json::json!([up("1.3.0", "minor", None), up("2.0.0", "major", None)])
        ),
    ]);
    let mut lib_typescript = dep(
        "typescript",
        "6.0.3",
        serde_json::json!([up("7.0.2", "major", Some("typescript-7.x"))]),
    );
    lib_typescript["updates"][0]["isVulnerabilityAlert"] = serde_json::json!(true);
    // js-sys: a security patch grouped with an ordinary minor bump.
    assert_eq!(root_deps[4]["depName"], "js-sys");
    root_deps[4]["updates"][0]["isVulnerabilityAlert"] = serde_json::json!(true);
    serde_json::json!({
        "name": "renovate", "level": 20, "msg": "packageFiles with updates",
        "config": {"npm": [
            {"packageFile": "package.json", "deps": root_deps},
            {"packageFile": "lib/package.json", "deps": [lib_typescript]},
        ]}
    })
    .to_string()
}

/// One candidate per Renovate branch, i.e. per change Renovate would open a
/// PR for: packages released together are one finding, the same move in
/// several manifests is one member of it, and a package with both a
/// non-breaking and a major update available is two findings. The
/// strongest update type speaks for a group, and `detail` lists every
/// member: it is what the fix worker moves.
#[test]
fn candidates_follow_renovates_branches() {
    let line = branches_fixture();

    let got = scan_printing("depscan-branches", &line)
        .expect("renovate succeeded")
        .candidates;

    let shape: Vec<(&str, &str, &str, &str, &str)> = got
        .iter()
        .map(|c| {
            (
                c.fingerprint.as_str(),
                c.package.as_str(),
                c.update_type.as_str(),
                c.file.as_str(),
                c.detail.as_str(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (
                "acme/widget:dep:typescript-eslint-monorepo@@typescript-eslint/parser=8.64.0+typescript-eslint=8.64.0",
                "typescript-eslint-monorepo",
                "minor",
                "package.json",
                "Renovate branch renovate/typescript-eslint-monorepo: one change moves all of these together.\n\
                 - @typescript-eslint/parser (npm): 8.64.0 \u{2192} 8.71.1 [minor] in package.json\n\
                 - typescript-eslint (npm): 8.64.0 \u{2192} 8.71.1 [minor] in package.json",
            ),
            (
                "acme/widget:dep:typescript-7.x@6.0.3+6.0.3",
                "typescript",
                "major",
                "package.json, lib/package.json",
                // One move in two manifests is one member, and the advisory
                // on the second copy is not lost.
                "Renovate branch renovate/typescript-7.x: one change moves all of these together.\n\
                 - typescript (npm): 6.0.3 \u{2192} 7.0.2 [major] [security] in package.json, lib/package.json",
            ),
            (
                "acme/widget:dep:pnpm-11.x@11.3.0",
                "pnpm",
                "minor",
                "package.json",
                "Renovate branch renovate/pnpm-11.x: one change moves all of these together.\n\
                 - pnpm (npm): 11.3.0 \u{2192} 11.28.5 [minor] in package.json",
            ),
            (
                "acme/widget:dep:pnpm-12.x@11.3.0",
                "pnpm",
                "major",
                "package.json",
                "Renovate branch renovate/pnpm-12.x: one change moves all of these together.\n\
                 - pnpm (npm): 11.3.0 \u{2192} 12.10.1 [major] in package.json",
            ),
            (
                "acme/widget:dep:wasm-monorepo@js-sys=0.3.103+wasm-bindgen=0.2.126",
                "wasm-monorepo",
                "minor",
                "package.json",
                "Renovate branch renovate/wasm-monorepo: one change moves all of these together.\n\
                 - js-sys (npm): 0.3.103 \u{2192} 0.3.106 [patch] [security] in package.json\n\
                 - wasm-bindgen (npm): 0.2.126 \u{2192} 0.3.0 [minor] in package.json",
            ),
            (
                "acme/widget:dep:node@@types/node=24.13.3+node=24.0.0",
                "node",
                "major",
                "package.json",
                "Renovate branch renovate/node: one change moves all of these together.\n\
                 - @types/node (npm): 24.13.3 \u{2192} 24.19.1 [minor] in package.json\n\
                 - node (npm): 24.0.0 \u{2192} 26.0.0 [major] in package.json",
            ),
            (
                "acme/widget:dep:left-pad-non-major@1.2.0",
                "left-pad",
                "minor",
                "package.json",
                "Renovate branch renovate/left-pad-non-major: one change moves all of these together.\n\
                 - left-pad (npm): 1.2.0 \u{2192} 1.3.0 [minor] in package.json",
            ),
            (
                "acme/widget:dep:left-pad-major@1.2.0",
                "left-pad",
                "major",
                "package.json",
                "Renovate branch renovate/left-pad-major: one change moves all of these together.\n\
                 - left-pad (npm): 1.2.0 \u{2192} 2.0.0 [major] in package.json",
            ),
        ]
    );
    assert_eq!(got[0].latest_version, "8.71.1");
}

/// A group is graded as high as its highest member would be on its own:
/// the wasm group's security patch keeps it `high` despite the group's
/// `minor` type, so a severity filter still finds the patch.
#[test]
fn groups_are_graded_by_their_highest_member() {
    let got = scan_printing("depscan-group-severity", &branches_fixture())
        .expect("renovate succeeded")
        .candidates;

    let severities: Vec<Severity> = got.iter().map(|c| c.severity).collect();
    assert_eq!(
        severities,
        vec![
            Severity::Medium,
            Severity::High,
            Severity::Medium,
            Severity::High,
            Severity::High,
            Severity::High,
            Severity::Medium,
            Severity::High,
        ]
    );
}

/// The fingerprint is where every member stands, not which versions occur:
/// after a partial upgrade the same set of versions remains, and the rest
/// of the update would otherwise match the merged finding as a duplicate
/// and never be filed again.
#[test]
fn a_partial_upgrade_changes_the_fingerprint() {
    let group = |a: &str, b: &str, c: &str| {
        let dep = |name: &str, current: &str| {
            serde_json::json!({
                "depName": name, "currentVersion": current, "datasource": "npm",
                "updates": [{"newVersion": "3.0.0", "updateType": "major", "branchName": "renovate/kit-monorepo"}]
            })
        };
        let line = serde_json::json!({
            "name": "renovate", "level": 20, "msg": "packageFiles with updates",
            "config": {"npm": [{"packageFile": "package.json",
                "deps": [dep("@kit/a", a), dep("@kit/b", b), dep("@kit/c", c)]}]}
        })
        .to_string();
        let got = scan_printing(&format!("depscan-partial-{a}{b}{c}"), &line)
            .expect("renovate succeeded")
            .candidates;
        assert_eq!(got.len(), 1, "{got:?}");
        got[0].fingerprint.clone()
    };

    let before = group("1.0.0", "1.0.0", "2.0.0");
    let after = group("2.0.0", "1.0.0", "2.0.0");

    assert_ne!(before, after);
}

/// Dependencies Renovate found but did not check -- skipped, or a failed
/// lookup -- are reported, so nothing concludes their updates are done.
#[test]
fn skipped_and_failed_lookups_are_reported_unchecked() {
    let line = serde_json::json!({
        "name": "renovate", "level": 20, "msg": "packageFiles with updates",
        "config": {"npm": [{"packageFile": "package.json", "deps": [
            {"depName": "left-pad", "currentVersion": "1.3.0", "datasource": "npm", "updates": []},
            {"depName": "private-pkg", "currentVersion": "1.0.0", "datasource": "npm",
             "warnings": [{"topic": "private-pkg", "message": "Failed to look up npm package private-pkg"}],
             "updates": []}
        ]}], "github-actions": [{"packageFile": ".github/workflows/ci.yml", "deps": [
            {"depName": "actions/checkout", "currentValue": "v4", "datasource": "github-tags",
             "skipReason": "github-token-required"}
        ]}]}
    })
    .to_string();

    let got = scan_printing("depscan-unchecked", &line).expect("renovate succeeded");

    assert!(got.candidates.is_empty(), "{got:?}");
    let mut unchecked = got.unchecked;
    unchecked.sort();
    assert_eq!(unchecked, vec!["actions/checkout", "private-pkg"]);
}
