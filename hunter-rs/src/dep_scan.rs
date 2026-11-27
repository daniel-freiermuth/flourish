//! Static dependency scanner using Renovate's local platform.
//!
//! Zero AI tokens — Renovate handles ecosystem detection, registry
//! queries, semver classification, and advisory lookup. Falls back
//! to None when Renovate isn't installed or fails.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::domain::{ForgeName, Severity};
use crate::util::{drain_pipe, join_pipes, run_cmd};

/// One update candidate, matching the `dep_update` finding schema.
///
/// One candidate is one Renovate branch: the unit Renovate itself would
/// open a PR for. A branch carries every dependency that has to move
/// together (a monorepo group, the same tool pinned in a manifest and in
/// CI), and a dependency with both a non-breaking and a major update
/// available lands in two branches, so the easy bump is never held
/// hostage by the migration.
#[derive(Debug, Clone)]
pub struct DepCandidate {
    pub fingerprint: String,
    pub file: String,
    pub ecosystem: String,
    pub package: String,
    pub current_version: String,
    pub latest_version: String,
    pub update_type: String,
    pub severity: Severity,
    pub confidence: f64,
    pub summary: String,
    pub detail: String,
    /// The fingerprint up to where the versions begin
    /// (`{repo}:dep:{branch}@`): the same Renovate branch in this repo,
    /// whatever its members stand at. A queued finding with this prefix is
    /// the same unit even after a member was bumped by hand.
    pub unit: String,
    /// What the branch moves, one entry per package and class: a group's
    /// type is its strongest member's, so this is the only place a member's
    /// own class survives.
    pub moves: Vec<DepMove>,
}

/// One package a candidate moves, and whether that package's own update
/// is a major one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepMove {
    pub package: String,
    pub major: bool,
}

/// What one Renovate scan of a repo found.
#[derive(Debug, Clone, Default)]
pub struct DepScan {
    pub candidates: Vec<DepCandidate>,
    /// Dependencies Renovate found but did not check: skipped (e.g.
    /// `github-token-required`) or whose lookup failed. Their updates may
    /// still be open, so the scan is no evidence against them.
    pub unchecked: Vec<String>,
}

/// The only variables Renovate's child sees, besides the sanitised `PATH`,
/// the two logging switches and the GitHub token from [`github_token`]:
/// what a Node process needs to find its home, a temp dir and a proxy.
/// Everything else -- `GH_TOKEN`, cloud credentials, whatever the daemon
/// was started with -- stays behind.
const FORWARDED_ENV: &[&str] = &[
    "HOME",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
];

/// Where Renovate reads the token for its github.com lookups (Actions,
/// GitHub tags and releases) when the platform is not GitHub itself.
/// Without one those deps are skipped as `github-token-required` and the
/// run still exits 0.
const GITHUB_TOKEN_ENV: &str = "GITHUB_COM_TOKEN";

/// Presets layered under the repo's own Renovate config, if it has one.
/// `config:recommended` is what nearly every Renovate-managed repo
/// extends; what matters here is its grouping (`group:monorepos`,
/// `group:recommended`), which puts packages that are released and must
/// be upgraded together on one branch, and its workarounds (e.g.
/// `@types/node` follows Node's LTS line). Without it every member of a
/// monorepo became its own finding.
const RENOVATE_PRESETS: &str = r#"["config:recommended"]"#;

/// The token Renovate's github.com lookups get for a repo on `forge`.
///
/// Only GitHub-hosted repos get one. `configured` (config.json
/// `renovate.githubToken`) wins; otherwise the operator's `gh` login for
/// github.com. `None` when neither is available -- the scan still runs,
/// without the GitHub-hosted deps.
pub fn github_token(forge: ForgeName, configured: Option<&str>) -> Option<String> {
    if forge != ForgeName::Github {
        return None;
    }
    if let Some(token) = configured {
        return Some(token.to_owned());
    }
    let (rc, out) = run_cmd(&["gh", "auth", "token", "--hostname", "github.com"], 10);
    let token = out.trim();
    // `run_cmd` merges stderr into the output: anything beyond one bare
    // token is a message, not a token. Never log `out`.
    if rc != 0 || token.is_empty() || token.contains(char::is_whitespace) {
        tracing::warn!(
            "dep_scan: no gh token for github.com (rc={rc}); GitHub-hosted deps are skipped"
        );
        return None;
    }
    Some(token.to_owned())
}

/// The operator's `renovate`, and the `PATH` to run it with.
///
/// Why not `npx renovate`, which is what this used to run: npx resolves a
/// package from the *current directory's* `node_modules/.bin` first, and the
/// current directory is the checkout being scanned. A repo that commits
/// `node_modules/.bin/renovate` therefore had its own program executed by
/// the daemon, with the daemon's environment. And when no Renovate was
/// installed at all, npx fetched whatever the registry served as latest.
///
/// So: only absolute `PATH` entries are searched (a relative one like
/// `node_modules/.bin` would resolve against the checkout), any entry or
/// binary that lives inside the checkout is skipped, and nothing is ever
/// downloaded -- no installed Renovate means `None`, and the caller falls
/// back to the model. The same filtered `PATH` is handed to the child, so
/// Renovate's own `#!/usr/bin/env node` cannot be pointed into the repo
/// either.
fn resolve_renovate(repo_path: &Path) -> Option<(PathBuf, OsString)> {
    let repo = repo_path.canonicalize().ok()?;
    let inside_repo = |p: &Path| p.canonicalize().is_ok_and(|real| real.starts_with(&repo));
    let path = std::env::var_os("PATH")?;
    let dirs: Vec<PathBuf> = std::env::split_paths(&path)
        .filter(|d| d.is_absolute() && !inside_repo(d))
        .collect();
    let binary = dirs
        .iter()
        .map(|d| d.join("renovate"))
        .find(|candidate| is_executable_file(candidate) && !inside_repo(candidate))?;
    let clean_path = std::env::join_paths(&dirs).ok()?;
    Some((binary, clean_path))
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Run Renovate in local dry-run mode and return update candidates.
/// `github_token` is exported as [`GITHUB_TOKEN_ENV`].
/// Returns None when Renovate has no answer for the repo -- it failed (not
/// installed, timeout, non-zero exit) or looked up no dependency at all
/// (none found, or all skipped, e.g. an unsupported ecosystem or GitHub
/// deps without a token). The caller then falls back to the AI-based
/// analysis job. No candidates means every dependency looked up is current.
pub fn scan_repo(
    repo_path: &Path,
    repo_name: &str,
    github_token: Option<&str>,
    timeout_s: u64,
) -> Option<DepScan> {
    let Some((renovate, clean_path)) = resolve_renovate(repo_path) else {
        tracing::debug!("dep_scan: no installed renovate usable for {repo_name}");
        return None;
    };
    // renovate is a node script: the timeout below is enforced against
    // the whole tree (`Child::kill_tree`).
    let mut cmd = crate::child::command(&renovate);
    cmd.args(["--platform=local", "--dry-run=lookup"])
        .current_dir(repo_path)
        .env_clear()
        .env("PATH", clean_path)
        .env("LOG_FORMAT", "json")
        .env("LOG_LEVEL", "debug")
        .env("RENOVATE_EXTENDS", RENOVATE_PRESETS);
    for key in FORWARDED_ENV {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    if let Some(token) = github_token {
        cmd.env(GITHUB_TOKEN_ENV, token);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("dep_scan: renovate not available for {repo_name}: {e}");
            return None;
        }
    };

    // Renovate is extremely chatty at debug level; drain both pipes from a
    // reader thread each or it wedges on a full pipe buffer mid-lookup.
    let so = drain_pipe(child.take_stdout());
    let se = drain_pipe(child.take_stderr());

    let status = match child.wait_timeout(Duration::from_secs(timeout_s)) {
        Ok(Some(status)) => status,
        Ok(None) => {
            child.kill_tree();
            let _ = join_pipes(so, se);
            tracing::warn!("dep_scan: renovate timeout for {repo_name}");
            return None;
        }
        Err(_) => {
            child.kill_tree();
            let _ = join_pipes(so, se);
            return None;
        }
    };
    // renovate is a node script that spawns helpers: the direct child can
    // exit while a descendant still holds the pipes, which would make this
    // read unbounded (see util::run_cmd).
    child.kill_tree();
    let output = join_pipes(so, se);
    // A non-zero exit means the lookup is untrustworthy even when it logged
    // plenty: fall back to the AI job.
    if !status.success() {
        tracing::warn!(
            "dep_scan: renovate failed (rc={:?}) for {repo_name}",
            status.code()
        );
        return None;
    }
    let (scan, looked_up) = parse_renovate_output(&output, repo_name);
    if looked_up == 0 {
        tracing::info!("dep_scan: renovate looked up no dependencies for {repo_name}");
        return None;
    }
    tracing::info!(
        "dep_scan: {repo_name} — {} update candidates from renovate \
         ({looked_up} dependencies looked up, {} unchecked)",
        scan.candidates.len(),
        scan.unchecked.len()
    );
    Some(scan)
}

/// One dependency move on a Renovate branch.
#[derive(Debug)]
struct Member {
    package: String,
    datasource: String,
    current: String,
    new_version: String,
    update_type: String,
    vulnerable: bool,
    files: Vec<String>,
}

/// A Renovate branch key and the moves filed under it.
type Branch = (String, Vec<Member>);

/// What the scan found, and how many dependencies Renovate actually
/// looked up (those without a `skipReason`).
fn parse_renovate_output(output: &str, repo_name: &str) -> (DepScan, usize) {
    // In first-seen order, so candidates come out in Renovate's order.
    let mut branches: Vec<Branch> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();
    let mut looked_up = 0;

    for line in output.lines() {
        let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(config_obj) = obj.get("config").and_then(serde_json::Value::as_object) else {
            continue;
        };
        for (manager, files) in config_obj {
            let Some(files_arr) = files.as_array() else {
                continue;
            };
            for pf in files_arr {
                let Some(pf_obj) = pf.as_object() else {
                    continue;
                };
                let pkg_file = pf_obj
                    .get("packageFile")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let Some(deps) = pf_obj.get("deps").and_then(|v| v.as_array()) else {
                    continue;
                };
                looked_up += deps
                    .iter()
                    .filter(|d| d.get("skipReason").is_none())
                    .count();
                for dep in deps {
                    collect_dep_updates(dep, manager, pkg_file, &mut branches);
                    if let Some(name) = unchecked_dep(dep)
                        && !unchecked.iter().any(|u| u == name)
                    {
                        unchecked.push(name.to_owned());
                    }
                }
            }
        }
    }
    let candidates = branches
        .iter()
        .map(|(key, members)| branch_candidate(repo_name, key, members))
        .collect();
    (
        DepScan {
            candidates,
            unchecked,
        },
        looked_up,
    )
}

/// The dependency's name when Renovate did not check it: it was skipped
/// (`skipReason`) or its lookup failed (`warnings`).
fn unchecked_dep(dep: &serde_json::Value) -> Option<&str> {
    let skipped = dep.get("skipReason").is_some();
    let failed = dep
        .get("warnings")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|w| !w.is_empty());
    if !(skipped || failed) {
        return None;
    }
    dep.get("depName")
        .or_else(|| dep.get("packageName"))
        .and_then(serde_json::Value::as_str)
}

/// File each of `dep`'s updates under its Renovate branch.
fn collect_dep_updates(
    dep: &serde_json::Value,
    manager: &str,
    pkg_file: &str,
    branches: &mut Vec<Branch>,
) {
    let str_of = |v: &serde_json::Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| v.get(*k).and_then(serde_json::Value::as_str))
            .map(str::to_owned)
    };
    let Some(package) = str_of(dep, &["depName", "packageName"]) else {
        return;
    };
    // The resolved version (lockfile, digest-pinned tag) before the
    // manifest's range: `^24.0.0` and `^24.13.3` are both 24.13.3 when
    // that is what is installed, and the fingerprint must agree.
    // A digest-pinned dep stands at its digest too: a digest-only update
    // keeps the tag, and without the digest every later bump would share
    // the merged finding's fingerprint and never be filed.
    let with_digest = |v: String, src: &serde_json::Value, key: &str| match str_of(src, &[key]) {
        Some(d) => format!("{v}@{d}"),
        None => v,
    };
    let current = with_digest(
        str_of(dep, &["currentVersion", "currentValue"]).map_or_else(
            || "?".to_owned(),
            |c| {
                c.trim_start_matches(|c: char| "^~>=<".contains(c))
                    .to_owned()
            },
        ),
        dep,
        "currentDigest",
    );
    let datasource = str_of(dep, &["datasource"]).unwrap_or_else(|| manager.to_owned());
    let Some(updates) = dep.get("updates").and_then(|v| v.as_array()) else {
        return;
    };
    for u in updates {
        let new_version = with_digest(
            str_of(u, &["newVersion", "newValue"]).unwrap_or_else(|| "?".to_owned()),
            u,
            "newDigest",
        );
        // Normalise BEFORE grading. Renovate's housekeeping types mean
        // "this is a patch", and the record says so — grading them off the
        // raw value gave two findings both labelled `patch` different
        // confidences, with pin/digest scored 0.7, the same as a major bump.
        let update_type = normalize_update_type(
            u.get("updateType")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
        )
        .to_owned();
        let key = match u.get("branchName").and_then(|v| v.as_str()) {
            Some(b) => b.strip_prefix("renovate/").unwrap_or(b).to_owned(),
            // Renovate names every branch; without one, keep at least the
            // split between a non-breaking and a major update.
            None if update_type == "major" => format!("{package}-major"),
            None => format!("{package}-non-major"),
        };
        let vulnerable = u
            .get("isVulnerabilityAlert")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let idx = branches
            .iter()
            .position(|(k, _)| *k == key)
            .unwrap_or_else(|| {
                branches.push((key, Vec::new()));
                branches.len() - 1
            });
        let members = &mut branches[idx].1;
        // The same move in another manifest (a workspace package, a second
        // workflow) is one member touching several files.
        let same = members.iter_mut().find(|m| {
            m.package == package
                && m.datasource == datasource
                && m.current == current
                && m.new_version == new_version
        });
        match same {
            Some(m) => {
                if !m.files.iter().any(|f| f == pkg_file) {
                    m.files.push(pkg_file.to_owned());
                }
                m.vulnerable |= vulnerable;
            }
            None => members.push(Member {
                package: package.clone(),
                datasource: datasource.clone(),
                current: current.clone(),
                new_version,
                update_type,
                vulnerable,
                files: vec![pkg_file.to_owned()],
            }),
        }
    }
}

/// Rank for picking the update type that speaks for a whole branch.
fn type_rank(update_type: &str) -> u8 {
    match update_type {
        "major" => 2,
        "minor" => 1,
        _ => 0,
    }
}

/// Distinct values in first-seen order.
fn distinct<'a>(values: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for v in values {
        if !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// The finding for one Renovate branch.
///
/// The fingerprint is the branch plus where its members stand today, never
/// the version they could move to: a newer upstream release refreshes the
/// open finding (same fingerprint) instead of filing another, while a
/// merge or a manual bump moves `current` and so starts a new one.
/// "Where they stand" is every occurrence (package, manifest) with its
/// version, not the set of versions: after a partial upgrade (1, 1, 2 ->
/// 1, 2, 2) the set is unchanged, and the rest would match the merged
/// finding as a duplicate and never be filed.
fn branch_candidate(repo_name: &str, key: &str, members: &[Member]) -> DepCandidate {
    // The first member of the highest rank speaks for the branch.
    let lead = members
        .iter()
        .rev()
        .max_by_key(|m| type_rank(&m.update_type))
        .unwrap_or_else(|| unreachable!("a branch exists only with a member"));
    let update_type = lead.update_type.as_str();
    // The highest grade any member earns on its own: a security patch keeps
    // its `high` next to an ordinary minor bump in the same group.
    let severity = members
        .iter()
        .map(member_severity)
        .max_by_key(|s| match s {
            Severity::Low => 0,
            Severity::Medium => 1,
            Severity::High => 2,
        })
        .unwrap_or_else(|| unreachable!("a branch exists only with a member"));
    let confidence = match update_type {
        "patch" => 0.95,
        "minor" => 0.85,
        _ => 0.7,
    };

    let currents = distinct(members.iter().map(|m| m.current.as_str()));
    let latest = distinct(members.iter().map(|m| m.new_version.as_str())).join(", ");
    let packages = distinct(members.iter().map(|m| m.package.as_str()));
    let ecosystems = distinct(members.iter().map(|m| m.datasource.as_str()));
    let files = distinct(
        members
            .iter()
            .flat_map(|m| m.files.iter().map(String::as_str)),
    );
    let current_version = currents.join(", ");
    // One package (the branch key already names it): its versions, one per
    // manifest. Several: each as `package=version`.
    let mut standing: Vec<String> = members
        .iter()
        .flat_map(|m| {
            let at = if packages.len() == 1 {
                m.current.clone()
            } else {
                format!("{}={}", m.package, m.current)
            };
            std::iter::repeat_n(at, m.files.len())
        })
        .collect();
    standing.sort_unstable();
    let unit = format!("{repo_name}:dep:{key}@");
    let fingerprint = format!("{unit}{}", standing.join("+"));

    let (package, summary) = match packages.as_slice() {
        [one] => (
            (*one).to_owned(),
            format!("{one}: {update_type} update {current_version} \u{2192} {latest}"),
        ),
        many => (
            key.to_owned(),
            format!(
                "{key}: {update_type} update of {} packages that move together ({})",
                many.len(),
                many.join(", ")
            ),
        ),
    };
    let mut detail =
        format!("Renovate branch renovate/{key}: one change moves all of these together.");
    for m in members {
        let vuln = if m.vulnerable { " [security]" } else { "" };
        let _ = write!(
            detail,
            "\n- {} ({}): {} \u{2192} {} [{}]{vuln} in {}",
            m.package,
            m.datasource,
            m.current,
            m.new_version,
            m.update_type,
            m.files.join(", ")
        );
    }

    let mut moves: Vec<DepMove> = Vec::new();
    for m in members {
        let mv = DepMove {
            package: m.package.clone(),
            major: m.update_type == "major",
        };
        if !moves.contains(&mv) {
            moves.push(mv);
        }
    }

    DepCandidate {
        fingerprint,
        file: files.join(", "),
        ecosystem: ecosystems.join("+"),
        package,
        current_version,
        latest_version: latest,
        update_type: update_type.to_owned(),
        severity,
        confidence,
        summary,
        detail,
        unit,
        moves,
    }
}

/// How one move is graded on its own.
fn member_severity(m: &Member) -> Severity {
    match m.update_type.as_str() {
        "major" => Severity::High,
        "minor" => Severity::Medium,
        _ if m.vulnerable => Severity::High,
        _ => Severity::Low,
    }
}

fn normalize_update_type(ut: &str) -> &str {
    match ut {
        "pin" | "pinDigest" | "digest" | "lockFileMaintenance" | "lockfileUpdate" => "patch",
        _ => ut,
    }
}
