//! Render worker prompts from playbook templates (playbooks/*.md).
//! Port of hunter/playbooks.py. Templates use {{`SLOT_NAME`}} placeholders
//! substituted by `render`, which scans only the template -- user content
//! is inserted verbatim and never reinterpreted.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::Path;

use crate::forge::{CodeLocation, GhAuthor, OWN_MARK, PrView, Voice};
use crate::types::{Finding, PrState, Repo};

/// `PLAYBOOK_DIR` = <`project_root>/playbooks` (types.py `PLAYBOOK_DIR`).
pub fn playbook_dir(root: &Path) -> std::path::PathBuf {
    root.join("playbooks")
}

/// Substitute `{{KEY}}` slots in a template, in a single pass.
///
/// Single-pass matters twice. Substituted values are emitted verbatim and
/// never rescanned, so a value containing `{{` can neither be mistaken for
/// an unfilled slot nor be expanded by a later substitution — template
/// injection through PR bodies, repo notes or finding text is structurally
/// impossible rather than escaped away. And because only the TEMPLATE is
/// scanned, an unknown key is a real playbook typo: a half-rendered prompt
/// never reaches a worker.
pub fn render<S: BuildHasher>(template: &str, slots: &HashMap<&str, String, S>) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        out.push_str(&rest[..open]);
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            let snippet: String = rest[open..].chars().take(60).collect();
            bail!("unterminated placeholder in playbook: {snippet}");
        };
        let key = &after[..close];
        let Some(value) = slots.get(key) else {
            bail!("unfilled placeholder in playbook: {{{{{key}}}}}");
        };
        out.push_str(value);
        rest = &after[close + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One entry of the suppression list a scan is shown
/// ([`crate::suppression::suppression_list`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suppression {
    pub fingerprint: String,
    /// The verdict's condition (`holds while ...`) when the worker stated
    /// one, else its recorded reason.
    pub reason: String,
    /// Whether the code the verdict depends on changed since it was given.
    /// `None`: unchanged, or the verdict is not anchored to a commit.
    pub changed: Option<Changed>,
}

/// How an anchored verdict's code changed since its commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changed {
    /// These watched files differ between the verdict's commit and the
    /// scanned one.
    Files { since: String, files: Vec<String> },
    /// The verdict's commit could not be compared with the scanned one.
    Unknown { since: String },
}

/// Rejected/wontfix findings as a suppression corpus block, opened by the
/// rule for reading it. `reconfirmed` is where the scan records the
/// CHANGED verdicts it re-checked and found still holding
/// ([`crate::suppression::apply_reconfirmations`]).
pub fn suppressions_block(suppressions: &[Suppression], reconfirmed: &Path) -> String {
    if suppressions.is_empty() {
        return "(none yet)".to_owned();
    }
    let rule = format!(
        "\
Each entry was decided against the code as it was then.
- An entry marked CHANGED was judged at a commit whose code it depends on
  has changed since. Re-check its claim against the current code.
  - If the claim is true now, file it again with the SAME fingerprint: that
    reopens the finding. Say in its detail what changed since the verdict.
  - If the verdict still holds, record that in {}, a JSON array of
    {{\"fingerprint\": \"...\", \"holds_while\": \"the condition as it stands now\"}}
    entries. It moves the verdict forward to this commit, so the next scan
    does not re-check it.
- Any other entry: do not re-file it or a variant of it.",
        reconfirmed.display()
    );
    let entries = suppressions.iter().map(|s| {
        let mark = match &s.changed {
            None => String::new(),
            Some(Changed::Files { since, files }) => {
                format!(" [CHANGED since {since}: {}]", files.join(", "))
            }
            Some(Changed::Unknown { since }) => {
                format!(" [CHANGED: the verdict's commit {since} is not in this history]")
            }
        };
        format!("- {} -- {}{mark}", s.fingerprint, s.reason)
    });
    std::iter::once(rule)
        .chain(std::iter::once(String::new()))
        .chain(entries)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Active findings listed for novelty comparison.
pub fn known_block(known: &[Finding]) -> String {
    if known.is_empty() {
        return "(none yet)".to_owned();
    }
    known
        .iter()
        .map(|k| {
            let fp = &k.fingerprint;
            let summary = &k.summary;
            format!("- {fp} [{}] -- {summary}", k.status)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Prompt-relevant field keys (`playbooks._FINDING_PROMPT_KEYS`).
const FINDING_PROMPT_KEYS: &[&str] = &[
    "type",
    "fingerprint",
    "file",
    "symbol",
    "line",
    "bug_class",
    "severity",
    "confidence",
    "summary",
    "detail",
    "evidence_plan",
    "introduced_by",
    // dep_update fields
    "ecosystem",
    "package",
    "current_version",
    "latest_version",
    "update_type",
    "security_advisory",
    // test_gap fields
    "missing_tests",
    "test_file",
    // refactor fields
    "smell_type",
    "suggested_refactor",
    // modernization fields
    "modernization_class",
    "current_approach",
    "proposed_approach",
    // standards fields
    "standard_section",
];

/// Prompt-relevant fields from a finding, dropping None values.
pub fn finding_subset(f: &Finding) -> serde_json::Value {
    let full = serde_json::to_value(f).unwrap_or_default();
    let Some(map) = full.as_object() else {
        return serde_json::Value::Object(serde_json::Map::new());
    };
    let mut out = serde_json::Map::new();
    for &key in FINDING_PROMPT_KEYS {
        if let Some(v) = map.get(key)
            && !v.is_null()
        {
            out.insert(key.to_owned(), v.clone());
        }
    }
    serde_json::Value::Object(out)
}

// -- Private helpers ---------------------------------------------------------
/// Read a playbook template file.
fn read_template(root: &Path, name: &str) -> Result<String> {
    let path = playbook_dir(root).join(name);
    std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read playbook template {}", path.display()))
}

/// Substitute a default when the repo has no notes.
///
/// Despite the name it once had, this does *not* escape anything: the
/// single-pass `render` never re-examines substituted text (see the
/// module header), so escaping at call sites is unnecessary — and
/// reintroducing it here would corrupt any note legitimately containing
/// braces.
fn notes_or_default(repo_notes: &str, default: &str) -> String {
    if repo_notes.is_empty() {
        default.to_owned()
    } else {
        repo_notes.to_owned()
    }
}

/// Chronological comments + reviews by `voice`; oldest dropped past ~cap
/// chars. Port of playbooks.py _`feedback_blocks`, split by [`Voice`] so
/// bots' reviews reach the worker under their own heading and guidance.
///
/// Hunter's own comments ([`Voice::Own`]) run with the maintainers', as
/// the conversation they answer, labelled as hunter's and without their
/// mark. An inline comment names the file and line it was left on.
fn feedback_blocks(pr: &PrView, voice: Voice, cap: usize) -> String {
    let mut items: Vec<(String, String, String)> = Vec::new();
    let shown = |v: Voice| v == voice || (voice == Voice::Maintainer && v == Voice::Own);
    let who = |author: Option<&GhAuthor>, v: Voice| {
        let login = author.map_or("?", |a| a.login.as_str());
        if v == Voice::Own {
            format!("{login} (hunter's own earlier reply)")
        } else {
            login.to_owned()
        }
    };
    let unmarked = |body: &str| body.trim().trim_end_matches(OWN_MARK).trim_end().to_owned();

    for c in pr.comments.iter().filter(|c| shown(c.voice)) {
        let ts = c.created_at.clone();
        let mut header = who(c.author.as_ref(), c.voice);
        if let Some(loc) = &c.location {
            header = format!("{header} on {}", location_label(loc));
        }
        items.push((ts, header, unmarked(&c.body)));
    }

    for r in pr.reviews.iter().filter(|r| shown(r.voice)) {
        let state = &r.state;
        let raw_body = unmarked(&r.body);
        let body = if !state.is_empty() && state != "COMMENTED" {
            format!("[review: {state}] {raw_body}").trim().to_owned()
        } else {
            raw_body
        };
        if !body.is_empty() {
            let ts = r.submitted_at.clone();
            items.push((ts, who(r.author.as_ref(), r.voice), body));
        }
    }

    items.sort();
    let mut blocks: Vec<String> = items
        .iter()
        .map(|(ts, who, body)| format!("### {who} at {ts}\n{body}"))
        .collect();

    let mut dropped = 0usize;
    while blocks.len() > 1 && blocks.iter().map(|b| b.len() + 2).sum::<usize>() > cap {
        blocks.remove(0);
        dropped += 1;
    }
    if dropped > 0 {
        blocks.insert(0, format!("({dropped} older item(s) elided)"));
    }

    if blocks.is_empty() {
        match voice {
            Voice::Maintainer | Voice::Own => "(no comments or reviews)".to_owned(),
            Voice::Bot => "(no bot reviews)".to_owned(),
        }
    } else {
        blocks.join("\n\n")
    }
}

/// `path:line`, marked when the line number refers to an earlier commit
/// of the PR: the code it was left on has since changed.
fn location_label(loc: &CodeLocation) -> String {
    let mut label = match loc.line {
        Some(line) => format!("{}:{line}", loc.path),
        None => loc.path.clone(),
    };
    if loc.outdated {
        label.push_str(" (outdated: line as of an earlier commit)");
    }
    label
}

/// Status check rollup as bullet list.
fn checks_lines(pr: &PrView) -> String {
    let rollup = &pr.status_check_rollup;
    if rollup.is_empty() {
        return "(no checks reported)".to_owned();
    }
    rollup
        .iter()
        .take(30)
        .map(|c| {
            let name = c.name.as_deref().or(c.context.as_deref()).unwrap_or("?");
            let conclusion = c
                .conclusion
                .as_deref()
                .or(c.state.as_deref())
                .unwrap_or("PENDING");
            format!("- {name}: {conclusion}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// JSON-encode finding subset for template injection.
fn finding_json(f: &Finding) -> String {
    let subset = finding_subset(f);
    serde_json::to_string_pretty(&subset).unwrap_or_else(|_| "{}".to_owned())
}

// -- Build functions ---------------------------------------------------------

/// Each build_*_prompt reads the corresponding .md template, builds slots,
/// and calls `render()`. Signatures use typed structs instead of JSON blobs.
///
/// `tree` (`{{REPO_PATH}}`) is the job chain's own worktree, not
/// `repo.path`: the clone is fetch-only and its working files are never
/// updated, so a worker told to read it would review stale code.
pub fn build_hunt_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    diff_range: &str,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(
        repo_notes,
        "(No notes yet \u{2014} consider adding conventions/architecture/gotchas \
         as you discover them)",
    );
    let template = read_template(root, "hunt.md")?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", tree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DIFF_RANGE", diff_range.to_owned());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert(
        "SUPPRESSIONS",
        suppressions_block(
            suppressions,
            &crate::suppression::reconfirmed_path(out_path),
        ),
    );
    slots.insert("KNOWN_FINDINGS", known_block(known));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("MAX_FINDINGS", max_findings.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

/// How a fix worker classifies a decline (`NOT-A-BUG.md` / `DECLINED.md`),
/// spliced into every playbook that offers one so the instructions match
/// the parser (`scheduler::parse_decline`). The words are
/// [`crate::domain::ClosureClass`]'s, and so are the statuses they land as.
pub const DECLINE_CLASSIFICATION: &str = "\
The FIRST line of the decline file must be `Classification: <word>`, with
exactly one of these words, lowercase. The explanation and its evidence
follow from the second line on.
- `superseded`: the finding was valid, and its fix or change has landed
  since by another way (a commit, a PR, a larger change). Name the commit.
- `duplicate`: another finding or PR already covers the same change. Name it.
- `obsolete`: the code the finding is about no longer exists, or its
  premise no longer applies, for reasons unrelated to this finding.
- `wrong`: the finding's premise is false: there is no bug, the gap does
  not exist, or the change is not an improvement. Only when evidence from
  the current code SHOWS it, never because proof was hard to get.
- `unwanted`: the problem is real, but the project has decided against
  this kind of change. Cite where it decided (its docs, CI configuration,
  repository notes, a maintainer's words), not your own preference.
`wrong` and `unwanted` suppress: every later scan of this repository is
told not to report the finding again. The others do not. When in doubt
between a word that suppresses and one that does not, pick the one that
does not: a wrongly suppressed finding is never reported again. A decline
file whose first line is not a valid classification sends the finding
back to the operator's triage.

For `wrong`, follow the classification line directly with two more lines,
then the explanation:

    Holds while: <the condition>
    Depends on: <path>, <path>";

/// What a rejecting verdict states about itself, spliced into every
/// playbook that can give one (fix declines, the closed-PR harvest,
/// recheck) so that all three describe the same two fields the scheduler
/// anchors the verdict with (`crate::suppression`).
pub const VERDICT_CONDITION: &str = "\
A verdict that the finding is wrong (`wrong`, `invalid`) is a claim about
the code as it is now, and every later scan of this repository is told to
trust it until that code changes. So state what it rests on:
- holds while: ONE sentence naming the condition in the current code
  that makes the verdict true, specific enough that a later reader can
  check it, e.g. \"get_data_in_window drops the first time bin
  (src/plot_area.rs:141)\". Not a restatement of the verdict.
- depends on: the repository paths that condition lives in. When any of
  them changes, later scans are told to re-check the verdict instead of
  trusting it. Name the file the condition is in, even when it is not
  the finding's own file: that is the change that would make the finding
  true again.";

/// The decline instructions a fix playbook gets (`{{DECLINE_CLASSIFICATION}}`).
fn decline_instructions() -> String {
    format!("{DECLINE_CLASSIFICATION}\n\n{VERDICT_CONDITION}")
}

pub fn build_fix_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "fix.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    slots.insert("DECLINE_CLASSIFICATION", decline_instructions());
    render(&template, &slots)
}

pub fn build_apply_improvement_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "apply_improvement.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    slots.insert("DECLINE_CLASSIFICATION", decline_instructions());
    render(&template, &slots)
}

pub fn build_apply_modernization_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "apply_modernization.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("REPO_NOTES", notes);
    slots.insert("DECLINE_CLASSIFICATION", decline_instructions());
    render(&template, &slots)
}

pub fn build_engage_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    pr: &PrView,
    ps: &PrState,
    repo_notes: &str,
) -> Result<String> {
    let _ = finding; // engage template does not embed the finding JSON
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let attention = ps
        .needs_attention
        .as_deref()
        .unwrap_or("(none recorded)")
        .to_owned();
    let template = read_template(root, "engage.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("BRANCH", branch.to_owned());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DEFAULT_BRANCH", repo.default_branch.clone());
    slots.insert("PR_TITLE", pr.title.clone());
    slots.insert(
        "PR_BODY",
        if pr.body.is_empty() {
            "(no description)".to_owned()
        } else {
            pr.body.clone()
        },
    );
    slots.insert("FEEDBACK", feedback_blocks(pr, Voice::Maintainer, 8000));
    slots.insert("BOT_FEEDBACK", feedback_blocks(pr, Voice::Bot, 6000));
    slots.insert("CHECKS", checks_lines(pr));
    slots.insert("ATTENTION", attention);
    slots.insert(
        "PR_NUMBER",
        ps.pr_number
            .map_or_else(|| "?".to_owned(), |n| n.to_string()),
    );
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_harvest_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    branch: &str,
    repo: &Repo,
    pr: &PrView,
    pr_number: i64,
    repo_notes: &str,
) -> Result<String> {
    let _ = branch; // harvest template does not use branch directly
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "harvest.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DEFAULT_BRANCH", repo.default_branch.clone());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("PR_NUMBER", pr_number.to_string());
    slots.insert("PR_TITLE", pr.title.clone());
    slots.insert(
        "PR_BODY",
        if pr.body.is_empty() {
            "(no description)".to_owned()
        } else {
            pr.body.clone()
        },
    );
    slots.insert("FEEDBACK", feedback_blocks(pr, Voice::Maintainer, 8000));
    slots.insert("BOT_FEEDBACK", feedback_blocks(pr, Voice::Bot, 6000));
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

/// What the closed-PR harvest's tree holds (`{{WORKTREE_STATE}}`).
///
/// The worker has to know whether the files in front of it are the PR's
/// or the default branch's, or it will read one as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedTree {
    /// A fresh tree detached at the default branch: the cold harvest.
    DefaultBranch,
    /// The withdrawing engage's own tree, still at the PR's head: the
    /// harvest that continues it in the same session.
    PrHead,
}

/// How much of a closed PR's diff one prompt carries, in characters.
///
/// About 10,000 tokens. The two hand-run closed harvests cost 23,000 and
/// 40,000 tokens in total, so an uncapped diff could easily cost more than
/// the review of it; a PR larger than this is summarised by its title,
/// body and discussion anyway, and the worker can read the rest with git.
pub const PR_DIFF_CAP_CHARS: usize = 40_000;

/// The PR's diff for `{{PR_DIFF}}` as a whole fenced block.
///
/// The fence is one backtick longer than the longest backtick run in the
/// diff (and at least three): a diff of a markdown file carries its own
/// ``` lines, and a fixed fence would end the block at the first of them,
/// leaving the rest of the diff to read as prompt text.
fn fenced_diff(diff: &str) -> String {
    let body = capped_diff(diff);
    let longest = body.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    let newline = if body.ends_with('\n') { "" } else { "\n" };
    format!("{fence}diff\n{body}{newline}{fence}")
}

/// The PR's diff, cut at [`PR_DIFF_CAP_CHARS`] with a marker saying so,
/// so a truncated diff is never mistaken for the whole.
fn capped_diff(diff: &str) -> String {
    if diff.trim().is_empty() {
        return "(empty diff)".to_owned();
    }
    match diff.char_indices().nth(PR_DIFF_CAP_CHARS) {
        None => diff.to_owned(),
        Some((cut, _)) => {
            let rest = diff[cut..].chars().count();
            format!(
                "{}\n[diff truncated here: {rest} more characters not shown]",
                &diff[..cut]
            )
        }
    }
}

/// `base` is the default-branch commit the harvest judges against,
/// resolved right after its fetch (`jobs.base_sha`). The worker is named
/// it by its sha, not `origin/<default>`: that ref belongs to the shared
/// clone, which the admin lane fetches while the worker runs.
pub fn build_harvest_closed_prompt(
    root: &Path,
    finding: &Finding,
    worktree: &Path,
    repo: &Repo,
    pr: &PrView,
    pr_number: i64,
    pr_diff: &str,
    tree: ClosedTree,
    base: &str,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let db = &repo.default_branch;
    let state = match tree {
        ClosedTree::DefaultBranch => format!(
            "checked out at {db}'s HEAD ({base}), which does NOT contain this PR's changes; \
             the PR's diff is below"
        ),
        ClosedTree::PrHead => format!(
            "checked out at the PR's head branch; {db} is commit {base}; the PR's diff is below"
        ),
    };
    let template = read_template(root, "harvest-closed.md")?;
    let mut slots = HashMap::new();
    slots.insert("WORKTREE", worktree.display().to_string());
    slots.insert("WORKTREE_STATE", state);
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("DEFAULT_BRANCH", db.clone());
    slots.insert("BASE", base.to_owned());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("PR_NUMBER", pr_number.to_string());
    slots.insert("PR_TITLE", pr.title.clone());
    slots.insert(
        "PR_BODY",
        if pr.body.is_empty() {
            "(no description)".to_owned()
        } else {
            pr.body.clone()
        },
    );
    slots.insert("PR_DIFF", fenced_diff(pr_diff));
    slots.insert("FEEDBACK", feedback_blocks(pr, Voice::Maintainer, 8000));
    slots.insert("BOT_FEEDBACK", feedback_blocks(pr, Voice::Bot, 6000));
    slots.insert("REPO_NOTES", notes);
    slots.insert("VERDICT_CONDITION", VERDICT_CONDITION.to_owned());
    render(&template, &slots)
}

pub fn build_recheck_prompt(
    root: &Path,
    finding: &Finding,
    repo: &Repo,
    tree: &Path,
    out_path: &Path,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "recheck.md")?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", tree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("FINDING_JSON", finding_json(finding));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("REPO_NOTES", notes);
    slots.insert("VERDICT_CONDITION", VERDICT_CONDITION.to_owned());
    render(&template, &slots)
}

/// Analysis-job prompt builders (`test_gap`, `dep_update`, refactor, modernization,
/// standards) share a common signature via the `AnalysisSpec` pattern.
fn build_analysis_prompt(
    root: &Path,
    template_name: &str,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_items: i64,
    repo_notes: &str,
    known_slot: &str,
    max_slot: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, template_name)?;
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", tree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert(
        "SUPPRESSIONS",
        suppressions_block(
            suppressions,
            &crate::suppression::reconfirmed_path(out_path),
        ),
    );
    slots.insert(known_slot, known_block(known));
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert(max_slot, max_items.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

pub fn build_test_gap_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "test_gap.md",
        repo,
        tree,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_GAPS",
        "MAX_GAPS",
    )
}

pub fn build_dep_update_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "dep_update.md",
        repo,
        tree,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_UPDATES",
        "MAX_UPDATES",
    )
}

pub fn build_refactor_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "refactor.md",
        repo,
        tree,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_REFACTORS",
        "MAX_REFACTORS",
    )
}

pub fn build_modernization_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    build_analysis_prompt(
        root,
        "modernization.md",
        repo,
        tree,
        scope_note,
        suppressions,
        known,
        out_path,
        max_findings,
        repo_notes,
        "KNOWN_MODERNIZATIONS",
        "MAX_MODERNIZATIONS",
    )
}

pub fn build_standards_prompt(
    root: &Path,
    repo: &Repo,
    tree: &Path,
    scope_note: &str,
    suppressions: &[Suppression],
    known: &[Finding],
    out_path: &Path,
    max_findings: i64,
    repo_notes: &str,
) -> Result<String> {
    let notes = notes_or_default(repo_notes, "(No notes yet)");
    let template = read_template(root, "standards.md")?;
    // CODING_STANDARDS.md is the entire point of this job type — refuse to
    // build a prompt without it rather than waste tokens on a blind audit.
    let standards_path = root.parent().unwrap_or(root).join("CODING_STANDARDS.md");
    let standards_content = std::fs::read_to_string(&standards_path).with_context(|| {
        format!(
            "CODING_STANDARDS.md not found at {}",
            standards_path.display()
        )
    })?;
    if standards_content.is_empty() {
        bail!(
            "CODING_STANDARDS.md is empty at {}",
            standards_path.display()
        );
    }
    let mut slots = HashMap::new();
    slots.insert("REPO_PATH", tree.display().to_string());
    slots.insert("REPO_NAME", repo.name.clone());
    slots.insert("SCOPE_NOTE", scope_note.to_owned());
    slots.insert(
        "SUPPRESSIONS",
        suppressions_block(
            suppressions,
            &crate::suppression::reconfirmed_path(out_path),
        ),
    );
    slots.insert("KNOWN_FINDINGS", known_block(known));
    slots.insert("STANDARDS", standards_content.clone());
    slots.insert("OUT_PATH", out_path.display().to_string());
    slots.insert("MAX_FINDINGS", max_findings.to_string());
    slots.insert("REPO_NOTES", notes);
    render(&template, &slots)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::render;
    use std::collections::HashMap;

    fn slots(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[test]
    fn substitutes_every_occurrence() {
        let out = render("{{A}}-{{B}}-{{A}}", &slots(&[("A", "1"), ("B", "2")])).unwrap();
        assert_eq!(out, "1-2-1");
    }

    /// The whole point of scanning only the template: a PR body or repo note
    /// containing braces must not look like an unfilled slot.
    #[test]
    fn value_containing_braces_is_not_an_unfilled_placeholder() {
        let out = render(
            "body:\n{{PR_BODY}}",
            &slots(&[("PR_BODY", "use {{ mustache }} like this")]),
        )
        .unwrap();
        assert_eq!(out, "body:\nuse {{ mustache }} like this");
    }

    /// A value must never be rescanned -- otherwise content could inject a
    /// placeholder that a later substitution expands.
    #[test]
    fn value_naming_another_slot_is_not_expanded() {
        let out = render(
            "{{PR_BODY}} {{SECRET}}",
            &slots(&[("PR_BODY", "{{SECRET}}"), ("SECRET", "swordfish")]),
        )
        .unwrap();
        assert_eq!(out, "{{SECRET}} swordfish");
    }

    #[test]
    fn unknown_template_key_is_rejected() {
        let err = render("hello {{TYPO}}", &slots(&[("NAME", "x")])).unwrap_err();
        assert!(err.to_string().contains("TYPO"), "{err}");
    }

    #[test]
    fn unterminated_placeholder_is_rejected() {
        let err = render("hello {{NAME", &slots(&[("NAME", "x")])).unwrap_err();
        assert!(err.to_string().contains("unterminated"), "{err}");
    }

    #[test]
    fn multibyte_around_placeholders_is_preserved() {
        let out = render("→{{A}}←", &slots(&[("A", "café")])).unwrap();
        assert_eq!(out, "→café←");
    }
}
