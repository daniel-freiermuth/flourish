#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Every playbook renders with the slots its builder supplies.
//!
//! `render` errors on a `{{KEY}}` the caller did not supply, which is
//! the right behaviour — a half-rendered prompt must never reach a
//! worker. But it turned a typo from a logged warning into a hard
//! failure that aborts the job and wastes the cycle, and the only thing
//! standing between the two is that twelve templates in
//! `hunter/playbooks/` agree with twelve builders in `src/playbooks.rs`.
//! A review flagged this as the highest-risk item of its batch and
//! verified it by *reading* the templates. This checks it.
//!
//! Deliberately NOT hermetic: it renders the real playbooks, because a
//! stub template would agree with its builder by construction and
//! assert nothing. That makes it a contract test between two
//! directories of the repo, like `hunter/tests/test_schema_parity.py`.
//! cargo-mutants copies only the crate, so `.cargo/mutants.toml`
//! excludes this binary rather than have every mutant run fail its
//! baseline.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::FindingType;
use hunter::forge::{GhAuthor, GhCheckRun, GhComment, GhReview, PrView, Voice};
use hunter::playbooks;
use hunter::types::{Finding, PrState};
use support::{sample_finding, sample_repo};

/// The real playbook directory, beside the crate.
fn root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate has a parent")
        .join("hunter");
    assert!(
        root.join("playbooks").is_dir(),
        "expected the real playbooks at {}; this test renders them on purpose",
        root.join("playbooks").display()
    );
    root
}

fn out_path() -> PathBuf {
    PathBuf::from("/tmp/hunter-playbook-contract/out.json")
}

/// Every prompt must be non-empty and substantial.
///
/// Note what is deliberately NOT asserted: that the output contains no
/// `{{`. An unrendered template slot is already impossible — `render`
/// returns `Err` for a key the builder did not supply, which is what
/// the expectations below rely on. Braces in the OUTPUT are ordinary
/// content: `build_standards_prompt` injects the whole of
/// `CODING_STANDARDS.md`, and that document documents the placeholder
/// syntax, so it contains `{{` legitimately.
///
/// Asserting on braces here would repeat the bug the single-pass
/// renderer fixed — conflating "the template has a hole" with "a value
/// contains a brace". Confirmed by experiment: under the previous
/// renderer, which scanned the substituted text, this repo could not
/// build a standards prompt at all.
fn assert_usable(name: &str, prompt: &str) {
    assert!(!prompt.trim().is_empty(), "{name}: rendered empty");
    assert!(
        prompt.len() > 200,
        "{name}: rendered suspiciously short ({} bytes) — a slot filled \
         with an empty value still costs a worker a window: {prompt}",
        prompt.len()
    );
}

/// Shared shape of the two "apply a proposed change" builders.
type ApplyBuilder =
    fn(&Path, &Finding, &Path, &str, &hunter::types::Repo, &str) -> anyhow::Result<String>;

/// Shared shape of the five repo-level analysis prompt builders.
type AnalysisBuilder = fn(
    &Path,
    &hunter::types::Repo,
    &Path,
    &str,
    &[playbooks::Suppression],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

/// One suppression of each shape a scan can be shown.
fn sample_suppression() -> playbooks::Suppression {
    playbooks::Suppression {
        fingerprint: "widget:src/lib.rs:parse:logic".to_owned(),
        reason: "holds while parse rejects empty input (src/lib.rs:12)".to_owned(),
        changed: Some(playbooks::Changed::Files {
            since: "abc123".to_owned(),
            files: vec!["src/lib.rs".to_owned()],
        }),
    }
}

fn analysis_cases() -> Vec<(&'static str, FindingType)> {
    vec![
        ("test_gap", FindingType::TestGap),
        ("dep_update", FindingType::DepUpdate),
        ("refactor", FindingType::Refactor),
        ("modernization", FindingType::Modernization),
        ("standards", FindingType::Standards),
    ]
}

#[test]
fn hunt_prompt_renders() {
    let repo = sample_repo();
    let known = [sample_finding(FindingType::Bug)];
    let suppressed = [sample_suppression()];
    let prompt = playbooks::build_hunt_prompt(
        &root(),
        &repo,
        Path::new("/tmp/tree"),
        "abc123..def456",
        "a scope note",
        &suppressed,
        &known,
        &out_path(),
        8,
        "repo notes",
    )
    .expect("hunt.md must render with the slots build_hunt_prompt supplies");
    assert_usable("hunt", &prompt);
}

#[test]
fn analysis_prompts_render() {
    let repo = sample_repo();
    let root = root();
    let out = out_path();
    for (name, kind) in analysis_cases() {
        let known = [sample_finding(kind)];
        let suppressed = [sample_suppression()];
        let build: AnalysisBuilder = match kind {
            FindingType::TestGap => playbooks::build_test_gap_prompt,
            FindingType::DepUpdate => playbooks::build_dep_update_prompt,
            FindingType::Refactor => playbooks::build_refactor_prompt,
            FindingType::Modernization => playbooks::build_modernization_prompt,
            FindingType::Standards => playbooks::build_standards_prompt,
            other @ FindingType::Bug => panic!("unhandled analysis kind {other:?}"),
        };
        let prompt = build(
            &root,
            &repo,
            Path::new("/tmp/tree"),
            "a scope note",
            &suppressed,
            &known,
            &out,
            8,
            "repo notes",
        )
        .unwrap_or_else(|e| panic!("{name}.md must render with its builder's slots: {e}"));
        assert_usable(name, &prompt);
    }
}

#[test]
fn fix_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt = playbooks::build_fix_prompt(
        &root(),
        &finding,
        Path::new("/tmp/worktree"),
        "fix/branch",
        &repo,
        "repo notes",
    )
    .expect("fix.md must render");
    assert_usable("fix", &prompt);
}

#[test]
fn improvement_and_modernization_apply_prompts_render() {
    let repo = sample_repo();
    for (name, kind, build) in [
        (
            "apply_improvement",
            FindingType::Refactor,
            playbooks::build_apply_improvement_prompt as ApplyBuilder,
        ),
        (
            "apply_modernization",
            FindingType::Modernization,
            playbooks::build_apply_modernization_prompt,
        ),
    ] {
        let finding = sample_finding(kind);
        let prompt = build(
            &root(),
            &finding,
            Path::new("/tmp/worktree"),
            "fix/branch",
            &repo,
            "repo notes",
        )
        .unwrap_or_else(|e| panic!("{name}.md must render: {e}"));
        assert_usable(name, &prompt);
    }
}

#[test]
fn recheck_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt = playbooks::build_recheck_prompt(
        &root(),
        &finding,
        &repo,
        Path::new("/tmp/tree"),
        &out_path(),
        "repo notes",
    )
    .expect("recheck.md must render");
    assert_usable("recheck", &prompt);
}

/// A `PrView` with every list populated, so `FEEDBACK`, `BOT_FEEDBACK`
/// and `CHECKS` render real content rather than their empty-case
/// placeholders — an empty slot value hides a template that was never
/// given anything to say.
fn sample_pr_view() -> PrView {
    PrView {
        head_ref: "fix/branch".to_owned(),
        head_sha: "deadbeef".to_owned(),
        updated_at: "2026-01-02T03:04:05Z".to_owned(),
        title: "Fix the thing".to_owned(),
        body: "This fixes the thing.".to_owned(),
        status_check_rollup: vec![GhCheckRun {
            name: Some("build".to_owned()),
            context: None,
            conclusion: Some("FAILURE".to_owned()),
            state: None,
        }],
        comments: vec![
            GhComment {
                id: String::new(),
                author: Some(GhAuthor {
                    login: "reviewer".to_owned(),
                }),
                body: "please rename this".to_owned(),
                created_at: "2026-01-02T03:00:00Z".to_owned(),
                voice: Voice::Maintainer,
            },
            GhComment {
                id: String::new(),
                author: Some(GhAuthor {
                    login: "coderabbitai".to_owned(),
                }),
                body: "consider extracting a helper".to_owned(),
                created_at: "2026-01-02T03:02:00Z".to_owned(),
                voice: Voice::Bot,
            },
        ],
        reviews: vec![GhReview {
            id: String::new(),
            author: Some(GhAuthor {
                login: "reviewer".to_owned(),
            }),
            body: "needs a test".to_owned(),
            submitted_at: "2026-01-02T03:01:00Z".to_owned(),
            state: "CHANGES_REQUESTED".to_owned(),
            voice: Voice::Maintainer,
        }],
        ..PrView::default()
    }
}

/// `PrState` has no `Default` in production and should not grow one just
/// for tests, so every column is spelled out.
fn sample_pr_state() -> PrState {
    PrState {
        finding_id: 42,
        pr_number: Some(7),
        state: Some("open".to_owned()),
        mergeable: Some("conflicting".to_owned()),
        checks: Some("failing".to_owned()),
        head_ref: Some("fix/branch".to_owned()),
        last_activity_at: Some(1),
        last_engaged_activity_at: Some(1),
        needs_attention: Some("new_comments,changes_requested".to_owned()),
        attention_since: Some(1),
        attention_fingerprint: Some("att-fp".to_owned()),
        addressed_fingerprint: Some("addr-fp".to_owned()),
        head_sha: Some("deadbeef".to_owned()),
        addressed_head_sha: Some("cafebabe".to_owned()),
        synced_at: Some(1),
        harvested_at: None,
    }
}

#[test]
fn engage_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt = playbooks::build_engage_prompt(
        &root(),
        &finding,
        Path::new("/tmp/worktree"),
        "fix/branch",
        &repo,
        &sample_pr_view(),
        &sample_pr_state(),
        "repo notes",
    )
    .expect("engage.md must render with the slots build_engage_prompt supplies");
    assert_usable("engage", &prompt);
}

#[test]
fn harvest_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let prompt = playbooks::build_harvest_prompt(
        &root(),
        &finding,
        Path::new("/tmp/worktree"),
        "fix/branch",
        &repo,
        &sample_pr_view(),
        7,
        "repo notes",
    )
    .expect("harvest.md must render with the slots build_harvest_prompt supplies");
    assert_usable("harvest", &prompt);
}

/// Rendered for both trees the closed-PR harvest runs in: the builder
/// writes a different `{{WORKTREE_STATE}}` for each, and both reach a
/// worker.
#[test]
fn harvest_closed_prompt_renders() {
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    for tree in [
        playbooks::ClosedTree::DefaultBranch,
        playbooks::ClosedTree::PrHead,
    ] {
        let prompt = playbooks::build_harvest_closed_prompt(
            &root(),
            &finding,
            Path::new("/tmp/worktree"),
            &repo,
            &sample_pr_view(),
            7,
            "diff --git a/x b/x\n",
            tree,
            "repo notes",
        )
        .expect(
            "harvest-closed.md must render with the slots build_harvest_closed_prompt supplies",
        );
        assert_usable("harvest-closed", &prompt);
    }
}

/// The harvest playbooks are the ones whose workers INVENT findings
/// of any type, and `ingest` rejects an entry that omits (or misshapes) a
/// required field for its type — the follow-up is dropped, not queued,
/// and only a truncated `ingest:` error event survives. Seven such losses
/// are on record: four `test_gap` entries without a usable `missing_tests`
/// (2026-09-12, 2026-09-22) and three `bug` entries with no `bug_class`
/// (2026-09-18, 2026-09-22), each because the playbook spelled out the
/// shapes for some types and not others — engage (which offered them on
/// withdrawal until the harvest took that over) did not spell out any, it
/// pointed at a "same schema as `apply_improvement.md`" that does not
/// exist. The instructions and the validator have to agree field by field.
///
/// Scope is derived, not listed: every playbook that offers
/// `FOLLOW-UPS.json` is held to this.
#[test]
fn followup_playbooks_name_every_required_field_they_ask_workers_to_emit() {
    let dir = root().join("playbooks");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read playbooks").flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        // The template, not a rendered prompt: rendering splices the
        // source finding's own JSON in, which mentions every column and
        // would make these assertions pass whatever the instructions say.
        let template = std::fs::read_to_string(&path).expect("read playbook");
        if !template.contains("FOLLOW-UPS.json") {
            continue;
        }
        checked += 1;
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // The exhaustive list, not a hand-written one: a type added to
        // `FindingType` without matching playbook instructions is exactly
        // the failure this test exists to catch.
        for ft in FindingType::ALL {
            for field in hunter::ingest::type_required_fields(ft) {
                assert!(
                    template.contains(field),
                    "{name} offers {ft} follow-ups but never names its required field {field:?}"
                );
            }
        }
        // `bug` has no entry in `type_required_fields`: ingest validates
        // `bug_class` and its enum separately. Workers that had to guess
        // invented "missing-guard", "stale-selector", "unchecked-index".
        assert!(
            template.contains("bug_class"),
            "{name} offers bug follow-ups but never names bug_class"
        );
        for bc in hunter::domain::BugClass::ALL {
            assert!(
                template.contains(&bc.to_string()),
                "{name} must name the valid bug_class {bc}"
            );
        }
    }
    assert!(
        checked >= 2,
        "expected harvest and harvest-closed to offer FOLLOW-UPS.json, found {checked}"
    );
}

/// Guards against a playbook being added without a builder, or renamed
/// out from under one — either of which only shows up as a job failing
/// at runtime.
#[test]
fn every_playbook_is_covered_by_this_test() {
    let dir = root().join("playbooks");
    let mut found: Vec<String> = std::fs::read_dir(&dir)
        .expect("read playbooks")
        .filter_map(std::result::Result::ok)
        .filter_map(|e| {
            let p = e.path();
            (p.extension()? == "md").then(|| p.file_stem()?.to_str().map(str::to_owned))?
        })
        .collect();
    found.sort();

    let mut exercised: Vec<String> = [
        "hunt",
        "fix",
        "apply_improvement",
        "apply_modernization",
        "recheck",
        "engage",
        "harvest",
        "harvest-closed",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .chain(analysis_cases().into_iter().map(|(n, _)| n.to_owned()))
    .collect();
    exercised.sort();

    assert_eq!(
        found, exercised,
        "playbooks on disk and playbooks exercised here have diverged — \
         a new template needs a case above, or it ships unrendered"
    );
}
