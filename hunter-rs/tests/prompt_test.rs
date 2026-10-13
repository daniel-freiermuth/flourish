#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! What the prompt builders tell a worker, rendered from stub templates.
//!
//! `playbook_contract_test` renders the real playbooks, which proves each
//! builder supplies every slot its template names, but not what it puts
//! in them. That is checked here, and stubs are the right tool for it: a
//! template made of nothing but slots renders exactly what the builder
//! supplied.
//!
//! The value that matters most is where the worker works. A job runs in
//! its chain's own tree (`jobs/<origin>/tree`); the clone the repo row
//! names is shared by every job on the repo, so a prompt that sent the
//! worker there would have it read, and edit, a checkout that is not the
//! one its chain pinned.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::FindingType;
use hunter::forge::{GhAuthor, GhComment, GhReview, PrView, Voice};
use hunter::playbooks;
use hunter::types::{Finding, PrState, Repo};
use support::{TempDir, sample_finding, sample_repo};

/// Shared shape of the five repo-level analysis prompt builders.
type AnalysisBuilder = fn(
    &Path,
    &Repo,
    &Path,
    &str,
    &[playbooks::Suppression],
    &[Finding],
    &Path,
    i64,
    &str,
) -> anyhow::Result<String>;

/// One slot per line, named as the builder names it.
const COMMON: &str = "tree={{REPO_PATH}}\nout={{OUT_PATH}}\nrepo={{REPO_NAME}}\n";

/// A project root holding a stub of every template under test, plus the
/// `CODING_STANDARDS.md` the standards builder reads beside it.
fn stub_root(dir: &TempDir) -> PathBuf {
    let root = dir.subdir("hunter");
    let playbooks = dir.subdir("hunter/playbooks");
    for (name, max_slot) in [
        ("test_gap.md", "MAX_GAPS"),
        ("dep_update.md", "MAX_UPDATES"),
        ("refactor.md", "MAX_REFACTORS"),
        ("modernization.md", "MAX_MODERNIZATIONS"),
    ] {
        std::fs::write(
            playbooks.join(name),
            format!("{COMMON}max={{{{{max_slot}}}}}\n"),
        )
        .unwrap();
    }
    std::fs::write(
        playbooks.join("standards.md"),
        format!("{COMMON}max={{{{MAX_FINDINGS}}}}\nstandards={{{{STANDARDS}}}}\n"),
    )
    .unwrap();
    std::fs::write(
        playbooks.join("recheck.md"),
        format!("{COMMON}finding={{{{FINDING_JSON}}}}\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("CODING_STANDARDS.md"),
        "Name things for what they mean.\n",
    )
    .unwrap();
    root
}

/// The value rendered into `slot`, from a `slot=value` line.
fn slot<'a>(prompt: &'a str, slot: &str) -> Option<&'a str> {
    prompt
        .lines()
        .find_map(|l| l.strip_prefix(slot)?.strip_prefix('='))
}

/// A chain's tree and output path, deliberately nowhere near the clone.
fn chain_paths(dir: &TempDir) -> (PathBuf, PathBuf) {
    let chain = dir.join("work_root/jobs/7");
    (chain.join("tree"), chain.join("session/out.json"))
}

#[test]
fn analysis_prompts_send_the_worker_to_the_chains_tree() {
    let dir = TempDir::new("prompt-analysis");
    let root = stub_root(&dir);
    let repo = sample_repo();
    let (tree, out) = chain_paths(&dir);

    for (name, build) in [
        (
            "test_gap",
            playbooks::build_test_gap_prompt as AnalysisBuilder,
        ),
        ("dep_update", playbooks::build_dep_update_prompt),
        ("refactor", playbooks::build_refactor_prompt),
        ("modernization", playbooks::build_modernization_prompt),
        ("standards", playbooks::build_standards_prompt),
    ] {
        let prompt = build(&root, &repo, &tree, "scope", &[], &[], &out, 5, "notes").unwrap();

        assert_eq!(slot(&prompt, "tree"), tree.to_str(), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "out"), out.to_str(), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "repo"), Some("widget"), "{name}: {prompt}");
        assert_eq!(slot(&prompt, "max"), Some("5"), "{name}: {prompt}");
        assert!(
            !prompt.contains(&repo.path),
            "{name} names the shared clone: {prompt}"
        );
    }
}

/// The standards audit is pointless without the document it audits
/// against, so the builder carries it into the prompt itself.
#[test]
fn the_standards_prompt_carries_the_standards_document() {
    let dir = TempDir::new("prompt-standards");
    let root = stub_root(&dir);
    let (tree, out) = chain_paths(&dir);

    let prompt = playbooks::build_standards_prompt(
        &root,
        &sample_repo(),
        &tree,
        "scope",
        &[],
        &[],
        &out,
        5,
        "notes",
    )
    .unwrap();

    assert_eq!(
        slot(&prompt, "standards"),
        Some("Name things for what they mean.")
    );
}

#[test]
fn the_recheck_prompt_sends_the_worker_to_the_chains_tree() {
    let dir = TempDir::new("prompt-recheck");
    let root = stub_root(&dir);
    let repo = sample_repo();
    let (tree, out) = chain_paths(&dir);
    let finding = sample_finding(FindingType::Bug);

    let prompt =
        playbooks::build_recheck_prompt(&root, &finding, &repo, &tree, &out, "notes").unwrap();

    assert_eq!(slot(&prompt, "tree"), tree.to_str(), "{prompt}");
    assert_eq!(slot(&prompt, "out"), out.to_str(), "{prompt}");
    assert!(
        prompt.contains(&finding.fingerprint),
        "the finding to recheck: {prompt}"
    );
    assert!(
        !prompt.contains(&repo.path),
        "names the shared clone: {prompt}"
    );
}

/// The text of `<<name ... name>>` in a stub-rendered prompt.
fn section<'a>(prompt: &'a str, name: &str) -> &'a str {
    let open = format!("<<{name}\n");
    let start = prompt.find(&open).map(|i| i + open.len()).unwrap();
    let len = prompt[start..].find(&format!("\n{name}>>")).unwrap();
    &prompt[start..start + len]
}

/// A screened PR thread: one maintainer comment and review, one bot
/// comment, and hunter's own reply posted under the maintainer's login.
fn screened_thread() -> PrView {
    let author = |login: &str| {
        Some(GhAuthor {
            login: login.to_owned(),
        })
    };
    PrView {
        comments: vec![
            GhComment {
                id: String::new(),
                author: author("lead"),
                body: "please add a test".to_owned(),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                voice: Voice::Maintainer,
            },
            GhComment {
                id: String::new(),
                author: author("lead"),
                body: hunter::forge::mark_own("added the test"),
                created_at: "2026-01-01T12:00:00Z".to_owned(),
                voice: Voice::Own,
            },
            GhComment {
                id: String::new(),
                author: author("coderabbitai"),
                body: "rename everything".to_owned(),
                created_at: "2026-01-02T00:00:00Z".to_owned(),
                voice: Voice::Bot,
            },
        ],
        reviews: vec![GhReview {
            id: String::new(),
            author: author("lead"),
            body: "and document it".to_owned(),
            submitted_at: "2026-01-03T00:00:00Z".to_owned(),
            state: "COMMENTED".to_owned(),
            voice: Voice::Maintainer,
        }],
        ..PrView::default()
    }
}

/// Bots' reviews reach the worker under their own heading, where the
/// playbooks tell it to verify rather than obey them, and never mixed in
/// with what maintainers asked for -- in every prompt that carries a
/// PR's discussion. Hunter's own replies stay in the maintainers' thread
/// they answer, labelled as its own and without the mark that tells them.
#[test]
fn pr_prompts_keep_bot_reviews_apart_from_maintainer_feedback() {
    let dir = TempDir::new("prompt-voices");
    let root = dir.subdir("hunter");
    let playbooks_dir = dir.subdir("hunter/playbooks");
    let stub = "<<maint\n{{FEEDBACK}}\nmaint>>\n<<bots\n{{BOT_FEEDBACK}}\nbots>>\n";
    for name in ["engage.md", "harvest.md", "harvest-closed.md"] {
        std::fs::write(playbooks_dir.join(name), stub).unwrap();
    }
    let repo = sample_repo();
    let finding = sample_finding(FindingType::Bug);
    let tree = Path::new("/tmp/tree");
    let pr = screened_thread();
    let ps = PrState {
        finding_id: finding.id,
        pr_number: Some(7),
        state: Some("open".to_owned()),
        mergeable: None,
        checks: None,
        head_ref: Some("fix/x".to_owned()),
        last_activity_at: None,
        last_engaged_activity_at: None,
        needs_attention: Some("new_comments".to_owned()),
        attention_since: None,
        attention_fingerprint: None,
        addressed_fingerprint: None,
        head_sha: None,
        addressed_head_sha: None,
        synced_at: None,
        harvested_at: None,
    };

    for (name, prompt) in [
        (
            "engage",
            playbooks::build_engage_prompt(&root, &finding, tree, "fix/x", &repo, &pr, &ps, "")
                .unwrap(),
        ),
        (
            "harvest",
            playbooks::build_harvest_prompt(&root, &finding, tree, "fix/x", &repo, &pr, 7, "")
                .unwrap(),
        ),
        (
            "harvest-closed",
            playbooks::build_harvest_closed_prompt(
                &root,
                &finding,
                tree,
                &repo,
                &pr,
                7,
                "",
                playbooks::ClosedTree::DefaultBranch,
                "",
            )
            .unwrap(),
        ),
    ] {
        let maint = section(&prompt, "maint");
        let bots = section(&prompt, "bots");
        assert!(maint.contains("please add a test"), "{name}: {prompt}");
        assert!(maint.contains("and document it"), "{name}: {prompt}");
        assert!(!maint.contains("rename everything"), "{name}: {prompt}");
        assert!(
            maint.contains(
                "### lead (hunter's own earlier reply) at 2026-01-01T12:00:00Z\nadded the test"
            ),
            "{name}: {prompt}"
        );
        assert!(
            !prompt.contains(hunter::forge::OWN_MARK),
            "{name}: {prompt}"
        );
        assert!(bots.contains("### coderabbitai"), "{name}: {prompt}");
        assert!(bots.contains("rename everything"), "{name}: {prompt}");
        assert!(!bots.contains("lead"), "{name}: {prompt}");
    }
}
