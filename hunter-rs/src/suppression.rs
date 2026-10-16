//! The suppression list a scan is shown, made conditional.
//!
//! A `rejected` or `wontfix` finding tells every later scan of its repo not
//! to report it again. A `rejected` verdict (`wrong`, an invalid recheck)
//! is a claim about the code it was judged against, and code moves: finding
//! 20 was "wrong" only because an unrelated first-bin trim hid the phantom
//! row it reported. So a worker's `rejected` verdict is anchored
//! ([`VerdictAnchor`]): the commit its tree was at, the files the verdict
//! depends on, and the one-line condition that makes it true. `wontfix` is
//! a decision about what the project wants, not a fact about its code, so
//! no code change lapses it.
//!
//! When a scan is prompted, every anchored entry whose files changed since
//! its commit is marked CHANGED. The scan re-checks it and either files it
//! again under the same fingerprint, which reopens the finding
//! ([`reopen_if_changed`], called by ingest), or records that the verdict
//! still holds, which moves its anchor to the scanned commit
//! ([`apply_reconfirmations`]) so the next scan does not pay to re-check it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::clones::Unheld;
use crate::domain::FindingStatus;
use crate::playbooks::{Changed, Suppression};
use crate::store::{FindingInsert, Store, VerdictAnchor};
use crate::transition::{Moved, Transition};
use crate::types::{Finding, Repo};

/// Longest `holds_while` kept: one sentence, shown in every later prompt.
const HOLDS_WHILE_MAX_CHARS: usize = 300;
/// Most files one verdict watches. A verdict that depends on more than this
/// is not one condition, and its prompt line would be a file list.
const MAX_ANCHOR_FILES: usize = 12;
/// One `git diff --name-only` over a handful of paths.
const GIT_DIFF_TIMEOUT_S: u64 = 30;
/// How much of a verdict's reason an anchored entry shows next to its
/// condition: what was decided and why, in one line.
const REASON_HEAD_MAX_CHARS: usize = 200;

/// The anchor a worker's verdict gets: `None` unless the verdict rejects,
/// since only a rejection is a claim about the code that the code can
/// outgrow. A `wontfix` stays unconditional, like an operator's verdict.
///
/// It watches the finding's own file and every path the worker said the
/// verdict depends on (`depends_on`), normalised to repo-relative paths;
/// anything that is not one (absolute, outside the repo, prose) is dropped.
pub fn verdict_anchor(
    status: FindingStatus,
    sha: &str,
    finding: &Finding,
    holds_while: Option<&str>,
    depends_on: &[String],
) -> Option<VerdictAnchor> {
    if status != FindingStatus::Rejected {
        return None;
    }
    let mut files: Vec<String> = Vec::new();
    for raw in finding
        .file
        .as_deref()
        .into_iter()
        .chain(depends_on.iter().map(String::as_str))
    {
        if files.len() == MAX_ANCHOR_FILES {
            break;
        }
        if let Some(path) = repo_path(raw)
            && !files.contains(&path)
        {
            files.push(path);
        }
    }
    let holds_while = holds_while
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(|h| h.chars().take(HOLDS_WHILE_MAX_CHARS).collect());
    Some(VerdictAnchor {
        sha: sha.to_owned(),
        files,
        holds_while,
    })
}

/// A repo-relative path from what a worker or a finding wrote:
/// `src/a.rs:42`, `./src/a.rs`, `` `src/a.rs` ``. `None` for anything that
/// cannot name a path inside the repository.
fn repo_path(raw: &str) -> Option<String> {
    let mut path = raw.trim().trim_matches('`').trim();
    // `:42`, `:42-50`, `:42:7`: a location, not part of the path.
    while let Some((head, tail)) = path.rsplit_once(':') {
        if tail.is_empty() || !tail.chars().all(|c| c.is_ascii_digit() || c == '-') {
            break;
        }
        path = head;
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let outside = path.is_empty()
        || path.starts_with('/')
        || path.contains(char::is_whitespace)
        || path.split('/').any(|part| part == "..");
    (!outside).then(|| path.to_owned())
}
/// `depends_on` as a worker writes it into a JSON verdict file: an array
/// of paths, one comma-separated string, or null. Lenient on purpose: the
/// field is advisory, and a shape slip must not void the verdict it rides
/// on (an unreadable `CLOSE-REASON.json` fails the whole review).
pub fn de_paths<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Paths {
        Many(Vec<String>),
        One(String),
    }
    let paths: Option<Paths> = serde::Deserialize::deserialize(d)?;
    Ok(match paths {
        None => Vec::new(),
        Some(Paths::Many(paths)) => paths,
        Some(Paths::One(paths)) => paths
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect(),
    })
}

/// Whether `anchor`'s files changed between its commit and `at`, in the
/// clone at `clone`. Blocking: runs git.
///
/// A commit the clone cannot diff from (rewritten history, a PR head that
/// was never fetched or since pruned) is reported as [`Changed::Unknown`]:
/// nothing vouches for the verdict any more, so the scan re-checks it.
fn changed_since(clone: &Path, anchor: &VerdictAnchor, at: &str) -> Option<Changed> {
    if anchor.files.is_empty() {
        return None;
    }
    let (rc, out) = Unheld::ChangedPaths {
        from: &anchor.sha,
        to: at,
        paths: &anchor.files,
    }
    .run(clone, GIT_DIFF_TIMEOUT_S);
    let since: String = anchor.sha.chars().take(12).collect();
    if rc != 0 {
        return Some(Changed::Unknown { since });
    }
    let files: Vec<String> = out
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    (!files.is_empty()).then_some(Changed::Files { since, files })
}

/// A suppressed finding and the anchor of its verdict, if it has one.
pub type Suppressed = (Finding, Option<VerdictAnchor>);

/// `repo_id`'s suppressed findings of `finding_type`, each with its anchor.
pub async fn suppressed(
    store: &Store,
    repo_id: i64,
    finding_type: &str,
) -> sqlx::Result<Vec<Suppressed>> {
    let mut anchors = store.suppression_anchors(repo_id, finding_type).await?;
    Ok(store
        .suppressions(repo_id, finding_type)
        .await?
        .into_iter()
        .map(|f| {
            let anchor = anchors.remove(&f.id);
            (f, anchor)
        })
        .collect())
}

/// The suppression list for a scan of `repo` at commit `at`, from its
/// [`suppressed`] findings `entries`: every one with its status and
/// reason, its anchor's condition when it has one, and whether the code
/// its verdict depends on has changed since.
pub async fn suppression_list(repo: &Repo, entries: Vec<Suppressed>, at: &str) -> Vec<Suppression> {
    let clone = PathBuf::from(&repo.path);
    let at = at.to_owned();
    tokio::task::spawn_blocking(move || {
        entries
            .into_iter()
            .map(|(f, anchor)| {
                let changed = anchor.as_ref().and_then(|a| changed_since(&clone, a, &at));
                let reason = f
                    .verdict_reason
                    .unwrap_or_else(|| "(no reason recorded)".to_owned());
                let reason = match anchor.and_then(|a| a.holds_while) {
                    Some(condition) => {
                        let head: String = reason
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(REASON_HEAD_MAX_CHARS)
                            .collect();
                        format!("{}: {head}; holds while {condition}", f.status)
                    }
                    None => format!("{}: {reason}", f.status),
                };
                Suppression {
                    fingerprint: f.fingerprint,
                    reason,
                    changed,
                }
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Reopen suppressed finding `fid` that a scan of repo `repo_id` (job
/// `job`) just filed again, if its verdict lapsed: anchored, and the code
/// it rests on changed between its commit and the commit the job scanned.
/// That is the CHANGED entry the scan was told to re-check and file if its
/// claim is true now. Any other duplicate stays a duplicate. `true` when
/// reopened.
pub async fn reopen_if_changed(
    store: &Store,
    repo_id: i64,
    fid: i64,
    job: Option<i64>,
    row: &FindingInsert,
) -> sqlx::Result<bool> {
    let Some(job) = job else { return Ok(false) };
    let Some(finding) = store.get_finding(fid).await? else {
        return Ok(false);
    };
    if !finding.status.is_suppressed() {
        return Ok(false);
    }
    let Some(anchor) = store.verdict_anchor(fid).await? else {
        return Ok(false);
    };
    let Some(at) = store.pinned_sha(job).await? else {
        return Ok(false);
    };
    let Some(repo) = store.get_repo_by_id(repo_id).await? else {
        return Ok(false);
    };
    let clone = PathBuf::from(&repo.path);
    let lapsed = tokio::task::spawn_blocking(move || changed_since(&clone, &anchor, &at))
        .await
        .ok()
        .flatten()
        .is_some();
    if !lapsed {
        return Ok(false);
    }
    // Only from a suppressed status, so a finding something else moved
    // since the read above keeps that.
    let reopened = Transition::Reopened { scan: row };
    Ok(matches!(
        store.transition(fid, &reopened).await?,
        Moved::Applied
    ))
}

/// Where a scan writing its findings to `out_path` records the CHANGED
/// verdicts it re-checked and found still holding.
pub fn reconfirmed_path(out_path: &Path) -> PathBuf {
    out_path.with_extension("reconfirmed.json")
}

/// One entry of a scan's reconfirmation file.
#[derive(serde::Deserialize)]
struct Reconfirmed {
    fingerprint: String,
    #[serde(default)]
    holds_while: Option<String>,
}

/// Move the anchor of every verdict the scan at commit `at` re-checked and
/// found still holding ([`reconfirmed_path`] of its `out_path`) to `at`,
/// taking its restated condition. Only the repo's own anchored
/// suppressions of the scanned `types` move; anything else in the file is
/// ignored. No file reconfirms nothing. One that cannot be read or parsed
/// reconfirms nothing either, and says so: otherwise every CHANGED entry
/// would be re-checked again next scan with no trace of why.
pub async fn apply_reconfirmations(
    store: &Store,
    repo: &Repo,
    types: &[&str],
    out_path: &Path,
    at: &str,
) {
    let path = reconfirmed_path(out_path);
    let parsed = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => Err(e.to_string()),
        Ok(text) => serde_json::from_str::<Vec<Reconfirmed>>(&text).map_err(|e| e.to_string()),
    };
    let entries = match parsed {
        Ok(entries) => entries,
        Err(e) => {
            let _ = store
                .log_event(
                    "error",
                    &format!(
                        "{}: reconfirmation file {} unusable, nothing reconfirmed: {e}",
                        repo.name,
                        path.display()
                    ),
                    None,
                    None,
                )
                .await;
            return;
        }
    };
    let mut by_fingerprint: BTreeMap<String, i64> = BTreeMap::new();
    for ft in types {
        let anchored = store
            .suppression_anchors(repo.id, ft)
            .await
            .unwrap_or_default();
        for f in store.suppressions(repo.id, ft).await.unwrap_or_default() {
            if anchored.contains_key(&f.id) {
                by_fingerprint.insert(f.fingerprint, f.id);
            }
        }
    }
    for entry in entries {
        let Some(&fid) = by_fingerprint.get(&entry.fingerprint) else {
            continue;
        };
        let holds_while: Option<String> = entry
            .holds_while
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| h.chars().take(HOLDS_WHILE_MAX_CHARS).collect());
        if store
            .reconfirm_anchor(fid, at, holds_while.as_deref())
            .await
            .is_ok()
        {
            let _ = store
                .log_event(
                    "verdict",
                    &format!(
                        "finding {fid} [{}] verdict re-checked and still holds at {}",
                        entry.fingerprint,
                        at.chars().take(12).collect::<String>()
                    ),
                    None,
                    Some(fid),
                )
                .await;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::repo_path;

    #[test]
    fn a_location_and_decoration_are_not_part_of_the_path() {
        for (raw, path) in [
            ("src/a.rs", Some("src/a.rs")),
            ("src/a.rs:42", Some("src/a.rs")),
            ("src/a.rs:42-50", Some("src/a.rs")),
            ("src/a.rs:42:7", Some("src/a.rs")),
            ("`./src/a.rs`", Some("src/a.rs")),
            ("C:x", Some("C:x")),
            ("/etc/passwd", None),
            ("../outside.rs", None),
            ("src/../../x", None),
            ("the parser module", None),
            ("", None),
        ] {
            assert_eq!(repo_path(raw).as_deref(), path, "{raw:?}");
        }
    }
}
