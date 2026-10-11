#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_test_gap` — a test-gap scan that may file bugs.
//!
//! The scan is told about the repo's bugs as well as its gaps, so it does
//! not re-file what a hunt already found or a human already rejected, and
//! what it files as a bug lands as a bug.

mod support;

use hunter::domain::{FindingStatus, FindingType, ForgeName, Severity};
use hunter::store::FindingInsert;
use support::{GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

/// A registered, cloned repo with a stub `test_gap.md` that renders just
/// the suppression and known-findings blocks.
async fn setup() -> (
    TempDir,
    hunter::store::Store,
    hunter::config::Config,
    hunter::types::Repo,
) {
    let dir = TempDir::new("testgap-bugs");
    let origin_repo = GitRepo::with_branch(&dir, "unused");
    let origin = origin_repo.origin.to_string_lossy().into_owned();
    let (_db, store) = fresh_store(&dir, "testgap").await;
    let rid = store
        .add_repo(
            "widget",
            &origin,
            &dir.subdir("repos"),
            &origin_repo.default_branch,
            ForgeName::Gitlab,
        )
        .await
        .unwrap();
    let repo = store.get_repo_by_id(rid).await.unwrap().unwrap();
    git(dir.path(), &["clone", &origin, &repo.path]);
    std::fs::write(
        dir.subdir("playbooks").join("test_gap.md"),
        "SUPPRESSIONS:\n{{SUPPRESSIONS}}\nKNOWN:\n{{KNOWN_GAPS}}\nOUT {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");

    (dir, store, cfg, repo)
}

/// File a finding of `kind`; with `rejected`, reject it for that reason.
async fn seed(
    store: &hunter::store::Store,
    repo_id: i64,
    fingerprint: &str,
    kind: &str,
    rejected: Option<&str>,
) {
    let row = FindingInsert {
        fingerprint: fingerprint.to_owned(),
        file: "src/lib.rs".to_owned(),
        severity: Severity::Medium,
        confidence: 0.9,
        summary: format!("summary of {fingerprint}"),
        ..Default::default()
    };
    let (id, _) = store
        .upsert_finding(repo_id, &row, kind, None)
        .await
        .unwrap();
    if let Some(reason) = rejected {
        store
            .set_finding_verdict(id, FindingStatus::Rejected, reason)
            .await
            .unwrap();
    }
}

/// The type the finding with `fingerprint` was filed as.
async fn kind_of(store: &hunter::store::Store, fingerprint: &str) -> Option<FindingType> {
    store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.fingerprint == fingerprint)
        .map(|f| f.kind)
}

#[tokio::test]
async fn a_test_gap_scan_sees_the_repos_bugs_and_may_file_one() {
    let (_dir, store, cfg, repo) = setup().await;

    seed(
        &store,
        repo.id,
        "widget:src/lib.rs:open:boundary",
        "bug",
        None,
    )
    .await;
    seed(
        &store,
        repo.id,
        "widget:src/lib.rs:intended:logic",
        "bug",
        Some("intended behavior"),
    )
    .await;
    seed(
        &store,
        repo.id,
        "widget:src/lib.rs:refactor:dup",
        "refactor",
        None,
    )
    .await;

    let out = cfg.work_root.join("out").join("job1.test_gaps.json");
    let backend = ScriptedBackend::new({
        let out = out.clone();
        move |_| {
            let entries = serde_json::json!([
                {
                    "fingerprint": "widget:src/lib.rs:parse:test-gap", "file": "src/lib.rs",
                    "severity": "medium", "confidence": 0.8, "summary": "parse is untested",
                    "missing_tests": ["error path: empty input"], "test_file": "tests/parse.rs"
                },
                {
                    "type": "bug", "fingerprint": "widget:src/lib.rs:last:boundary",
                    "file": "src/lib.rs", "bug_class": "boundary", "severity": "high",
                    "confidence": 0.9, "summary": "drops the last element"
                }
            ]);
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            std::fs::write(&out, entries.to_string()).unwrap();
            done()
        }
    });

    let summary = hunter::scheduler::run_test_gap(&store, &cfg, &repo, &backend, None)
        .await
        .unwrap();

    let prompt = &backend.runs()[0].prompt;
    let (suppressions, known) = prompt
        .split_once("KNOWN:")
        .expect("the stub template renders");
    assert!(
        suppressions.contains("widget:src/lib.rs:intended:logic -- rejected: intended behavior"),
        "a rejected bug is a suppression: {prompt}"
    );
    assert!(
        known.contains("widget:src/lib.rs:open:boundary"),
        "an open bug is already tracked: {prompt}"
    );
    assert!(
        !prompt.contains("refactor:dup"),
        "other types stay out: {prompt}"
    );
    assert_eq!(
        summary.ingest.as_ref().map(|i| (i.inserted, i.invalid)),
        Some((2, 0)),
        "{summary:?}"
    );
    assert_eq!(
        kind_of(&store, "widget:src/lib.rs:parse:test-gap").await,
        Some(FindingType::TestGap)
    );
    assert_eq!(
        kind_of(&store, "widget:src/lib.rs:last:boundary").await,
        Some(FindingType::Bug)
    );
}
