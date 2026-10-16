#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Suppressions are conditional: a worker's suppressing verdict is anchored
//! to the commit it was judged at and the files it rests on, and a scan is
//! told to re-check the verdict once those files change.
//!
//! The motivating case is finding 20 (fleascope-monitor-rs): "the phantom
//! row corrupts the first bin" was wrong only because an unrelated
//! first-bin trim hid the row. Its suppression said nothing about that
//! trim, so removing it would have brought the bug back with every later
//! scan told not to report it.

mod support;

use std::path::{Path, PathBuf};

use hunter::config::Config;
use hunter::domain::FindingStatus;
use hunter::scheduler::{Attempt, run_fix, run_hunt};
use hunter::store::{FindingInsert, Store, VerdictAnchor};
use hunter::suppression::reconfirmed_path;
use hunter::types::Repo;
use support::{GitRepo, ScriptedBackend, TempDir, done, git};

struct Fixture {
    cfg: Config,
    store: Store,
    pusher: PathBuf,
    /// Last, so the directory outlives the SQLite handles above.
    _dir: TempDir,
}

/// Repo 1 with `src/lib.rs` and `src/plot.rs` on origin/main, cloned, and
/// stub fix and hunt playbooks; the hunt's prompt is its suppression list.
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    std::fs::create_dir_all(repo.work.join("src")).unwrap();
    std::fs::write(repo.work.join("src/lib.rs"), "pub fn parse() {}\n").unwrap();
    std::fs::write(repo.work.join("src/plot.rs"), "// drops the first bin\n").unwrap();
    git(&repo.work, &["add", "-A"]);
    git(&repo.work, &["commit", "-m", "code"]);
    git(&repo.work, &["push", "origin", "main"]);
    let pusher = dir.join("pusher");
    git(
        dir.path(),
        &[
            "clone",
            repo.origin.to_string_lossy().as_ref(),
            pusher.to_string_lossy().as_ref(),
        ],
    );
    let clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();

    let (path, pool) = support::fresh_pool(&dir, "suppression").await;
    let now = hunter::util::now_ms();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, \
         last_test_gap_at, last_dep_update_at, last_refactor_at, last_modernization_at, \
         last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let store = Store::connect(&path).await.unwrap();

    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    std::fs::write(
        playbooks.join("hunt.md"),
        "{{SUPPRESSIONS}}\n-> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        pusher,
        _dir: dir,
    }
}

impl Fixture {
    async fn repo(&self) -> Repo {
        self.store.get_repo_by_id(1).await.unwrap().unwrap()
    }

    /// A queued bug finding on `src/lib.rs`.
    async fn queued_bug(&self, fingerprint: &str) -> i64 {
        let (fid, _) = self
            .store
            .upsert_finding(
                1,
                &FindingInsert {
                    fingerprint: fingerprint.to_owned(),
                    file: "src/lib.rs:1".to_owned(),
                    severity: hunter::domain::Severity::Medium,
                    confidence: 0.9,
                    summary: "a phantom row corrupts the first bin".to_owned(),
                    ..Default::default()
                },
                "bug",
                None,
            )
            .await
            .unwrap();
        self.store
            .set_finding_status(fid, FindingStatus::Queued)
            .await
            .unwrap();
        fid
    }

    /// Commit `content` to `file` on origin/main.
    fn push(&self, file: &str, content: &str) {
        git(&self.pusher, &["pull", "--ff-only", "origin", "main"]);
        let path = self.pusher.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        git(&self.pusher, &["add", "-A"]);
        git(&self.pusher, &["commit", "-m", &format!("touch {file}")]);
        git(&self.pusher, &["push", "origin", "main"]);
    }

    /// Run a hunt and return the suppression list its prompt carried.
    async fn hunt_suppressions(&self) -> String {
        self.hunt_reporting("[]", None).await.0
    }

    /// Run a hunt whose worker files `findings` and, when given, records
    /// `reconfirmed` in the reconfirmation file its prompt named; return
    /// the suppression list the prompt carried, and how many of the filed
    /// findings reopened a suppressed one.
    async fn hunt_reporting(&self, findings: &str, reconfirmed: Option<&str>) -> (String, i64) {
        let out_dir = self.cfg.work_root.join("out");
        let findings = findings.to_owned();
        let reconfirmed = reconfirmed.map(str::to_owned);
        let worker = ScriptedBackend::new(move |tree| {
            let out = findings_path(&out_dir, tree);
            std::fs::write(&out, &findings).unwrap();
            if let Some(body) = &reconfirmed {
                std::fs::write(reconfirmed_path(&out), body).unwrap();
            }
            done()
        });
        self.hunt_with(&worker).await
    }

    /// Run a hunt with `worker`; return what [`Self::hunt_reporting`] does.
    async fn hunt_with(&self, worker: &ScriptedBackend) -> (String, i64) {
        let summary = run_hunt(
            &support::scope(&self.store),
            self.store.test_lane(),
            &self.cfg,
            Attempt::Cold(&self.repo().await),
            worker,
        )
        .await
        .unwrap();
        assert!(summary.skipped.is_none(), "{summary:?}");
        let prompt = worker.runs()[0].prompt.clone();
        let reopened = summary.ingest.as_ref().map_or(0, |i| i.reopened);
        (prompt.split("\n-> ").next().unwrap().to_owned(), reopened)
    }

    fn origin_tip(&self) -> String {
        git(&self.pusher, &["fetch", "origin"]);
        git(&self.pusher, &["rev-parse", "origin/main"])
            .trim()
            .to_owned()
    }

    /// The suppression line for `fingerprint`.
    fn line<'a>(list: &'a str, fingerprint: &str) -> &'a str {
        list.lines()
            .find(|l| l.starts_with(&format!("- {fingerprint} -- ")))
            .unwrap_or_else(|| panic!("no suppression for {fingerprint} in:\n{list}"))
    }
}

/// `<work_root>/out/job<origin>.findings.json` for the chain whose tree is
/// `<work_root>/jobs/<origin>/tree`.
fn findings_path(out_dir: &Path, tree: &Path) -> PathBuf {
    let origin = tree
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    out_dir.join(format!("job{origin}.findings.json"))
}

/// What a hunt files for the phantom finding: the same fingerprint, so it
/// lands on the suppressed row, with a new analysis.
const REFILED: &str = r#"[{"fingerprint":"alpha:src/lib.rs:phantom","file":"src/lib.rs","symbol":"parse","bug_class":"logic","severity":"high","confidence":0.9,"summary":"the phantom row is drawn again","detail":"src/plot.rs no longer drops the first bin, so the (0,0) row is plotted","evidence_plan":"plot a fresh buffer"}]"#;

/// Decline the phantom finding as `wrong`, resting on `src/plot.rs`;
/// return the commit it was judged at.
async fn decline_phantom(f: &Fixture, fid: i64) -> String {
    let judged_at = f.origin_tip();
    let decline = ScriptedBackend::writing(
        "NOT-A-BUG.md",
        "Classification: wrong\n\
         \n\
         **Holds while:** the plot drops the first bin (`src/plot.rs:1`)\n\
         Depends on: `src/plot.rs:1`, ../outside.rs\n\
         \n\
         Evidence: the row never reaches the output.",
    );
    let finding = f.store.get_finding(fid).await.unwrap().unwrap();
    run_fix(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &decline,
    )
    .await
    .unwrap();
    judged_at
}

const PHANTOM_LINE: &str = "- alpha:src/lib.rs:phantom -- rejected: wrong: Evidence: the row never \
                            reaches the output.; holds while the plot drops the first bin \
                            (`src/plot.rs:1`)";

/// The whole chain on a real repo. A declined fix's `wrong` verdict is
/// anchored to the commit the fix's tree was at, watching the finding's
/// file and the ones the verdict depends on, and every later scan is shown
/// its status, reason and condition. Filing the finding again while that
/// code is unchanged is a duplicate: the verdict stands. Once a depended-on
/// file changes, the scan is told so, and filing it again reopens it with
/// the new analysis. Before, the re-file was counted as a duplicate and
/// dropped, so the CHANGED mark could never lead anywhere.
#[tokio::test]
async fn a_finding_filed_again_after_its_verdicts_code_changed_is_reopened() {
    let f = fixture("supp-reopened").await;
    let fid = f.queued_bug("alpha:src/lib.rs:phantom").await;
    let judged_at = decline_phantom(&f, fid).await;

    let after = f.store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Rejected);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("wrong: Evidence: the row never reaches the output.")
    );
    let anchors = f.store.suppression_anchors(1, "bug").await.unwrap();
    assert_eq!(
        anchors.get(&fid),
        Some(&VerdictAnchor {
            sha: judged_at.clone(),
            files: vec!["src/lib.rs".to_owned(), "src/plot.rs".to_owned()],
            holds_while: Some("the plot drops the first bin (`src/plot.rs:1`)".to_owned()),
        }),
        "the path outside the repo is dropped"
    );

    let (list, count) = f.hunt_reporting(REFILED, None).await;
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:phantom"),
        PHANTOM_LINE
    );
    assert_eq!(count, 0, "nothing reopened");
    let unchanged = f.store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(
        unchanged.status,
        FindingStatus::Rejected,
        "filed again while the code it rests on is unchanged: still a duplicate"
    );

    f.push("NOTES.md", "unrelated\n");
    let list = f.hunt_suppressions().await;
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:phantom"),
        PHANTOM_LINE,
        "a change elsewhere leaves the verdict standing"
    );

    f.push("src/plot.rs", "// draws every bin\n");
    let (list, count) = f.hunt_reporting(REFILED, None).await;
    assert_eq!(count, 1, "the scan reports the reopen");
    let since: String = judged_at.chars().take(12).collect();
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:phantom"),
        format!("{PHANTOM_LINE} [CHANGED since {since}: src/plot.rs]"),
    );
    let reopened = f.store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(reopened.status, FindingStatus::New);
    assert_eq!(reopened.verdict_reason, None);
    assert_eq!(reopened.summary, "the phantom row is drawn again");
    assert_eq!(
        reopened.detail.as_deref(),
        Some("src/plot.rs no longer drops the first bin, so the (0,0) row is plotted")
    );
    assert_eq!(reopened.severity, hunter::domain::Severity::High);
    assert!(f.store.verdict_anchor(fid).await.unwrap().is_none());
    assert!(
        f.store.suppressions(1, "bug").await.unwrap().is_empty(),
        "the reopened finding is no longer suppressed"
    );
}

/// A scan that re-checks a CHANGED verdict and finds it still holds says so
/// in its reconfirmation file: the anchor moves to the scanned commit,
/// taking the restated condition, and the next scan sees the entry
/// unmarked instead of paying to re-check it on every run. An entry that
/// is not an anchored suppression of this repo is ignored.
#[tokio::test]
async fn a_verdict_reconfirmed_by_a_scan_is_not_rechecked_again() {
    let f = fixture("supp-reconfirmed").await;
    let fid = f.queued_bug("alpha:src/lib.rs:phantom").await;
    decline_phantom(&f, fid).await;
    f.push("src/plot.rs", "// still drops the first bin, reworded\n");
    let rechecked_at = f.origin_tip();

    let reconfirm = r#"[
        {"fingerprint": "alpha:src/lib.rs:phantom",
         "holds_while": "the plot still drops the first bin (src/plot.rs:1)"},
        {"fingerprint": "alpha:src/lib.rs:nothing-of-the-sort"}
    ]"#;
    let (list, _) = f.hunt_reporting("[]", Some(reconfirm)).await;
    assert!(
        Fixture::line(&list, "alpha:src/lib.rs:phantom").contains("[CHANGED since"),
        "{list}"
    );
    assert_eq!(
        f.store.verdict_anchor(fid).await.unwrap(),
        Some(VerdictAnchor {
            sha: rechecked_at,
            files: vec!["src/lib.rs".to_owned(), "src/plot.rs".to_owned()],
            holds_while: Some("the plot still drops the first bin (src/plot.rs:1)".to_owned()),
        })
    );

    f.push("NOTES.md", "unrelated\n");
    let list = f.hunt_suppressions().await;
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:phantom"),
        "- alpha:src/lib.rs:phantom -- rejected: wrong: Evidence: the row never reaches the \
         output.; holds while the plot still drops the first bin (src/plot.rs:1)"
    );
    let still = f.store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(still.status, FindingStatus::Rejected);
}

/// A reconfirmation file the scan wrote in the wrong shape, or one that
/// cannot be read at all, moves nothing and says so in the event log:
/// otherwise every CHANGED entry is re-checked again next scan with no
/// trace of why. A scan that wrote none (it had nothing to reconfirm) logs
/// nothing.
#[tokio::test]
async fn an_unusable_reconfirmation_file_is_logged_and_a_missing_one_is_not() {
    let f = fixture("supp-reconfirm-unusable").await;
    let fid = f.queued_bug("alpha:src/lib.rs:phantom").await;
    let judged_at = decline_phantom(&f, fid).await;
    f.push("src/plot.rs", "// reworded\n");
    let unusable = |events: &[hunter::types::Event]| {
        events
            .iter()
            .filter(|e| e.kind == "error" && e.message.contains("reconfirmation file"))
            .count()
    };

    f.hunt_suppressions().await;
    let events = f.store.recent_events(100).await.unwrap();
    assert_eq!(unusable(&events), 0, "no file is not an error: {events:?}");

    f.push("NOTES.md", "unrelated\n");
    f.hunt_reporting("[]", Some(r#"{"fingerprint": "alpha:src/lib.rs:phantom"}"#))
        .await;
    let events = f.store.recent_events(100).await.unwrap();
    assert_eq!(unusable(&events), 1, "not an array: {events:?}");

    f.push("NOTES.md", "unrelated again\n");
    let out_dir = f.cfg.work_root.join("out");
    let unreadable = ScriptedBackend::new(move |tree| {
        let out = findings_path(&out_dir, tree);
        std::fs::write(&out, "[]").unwrap();
        std::fs::create_dir_all(reconfirmed_path(&out)).unwrap();
        done()
    });
    f.hunt_with(&unreadable).await;
    let events = f.store.recent_events(100).await.unwrap();
    assert_eq!(unusable(&events), 2, "a directory: {events:?}");

    assert_eq!(
        f.store.verdict_anchor(fid).await.unwrap().map(|a| a.sha),
        Some(judged_at),
        "nothing moved"
    );
}

/// A verdict whose commit the clone cannot compare with (rewritten
/// history) is marked changed: nothing vouches for it any more. A verdict
/// with no anchor (a human's, a `wontfix`, or one from before anchors)
/// stays as it was, its status and reason shown unconditionally.
#[tokio::test]
async fn a_verdict_nothing_vouches_for_is_rechecked_and_an_unanchored_one_stands() {
    let f = fixture("supp-unknown").await;
    let orphaned = f.queued_bug("alpha:src/lib.rs:lost").await;
    let gone = "0123456789abcdef0123456789abcdef01234567";
    f.store
        .set_anchored_verdict(
            orphaned,
            FindingStatus::Rejected,
            "wrong: fine as it is",
            Some(&VerdictAnchor {
                sha: gone.to_owned(),
                files: vec!["src/lib.rs".to_owned()],
                holds_while: None,
            }),
        )
        .await
        .unwrap();
    let human = f.queued_bug("alpha:src/lib.rs:human").await;
    f.store
        .set_finding_verdict(human, FindingStatus::Wontfix, "not worth it")
        .await
        .unwrap();

    let list = f.hunt_suppressions().await;
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:lost"),
        "- alpha:src/lib.rs:lost -- rejected: wrong: fine as it is \
         [CHANGED: the verdict's commit 0123456789ab is not in this history]"
    );
    assert_eq!(
        Fixture::line(&list, "alpha:src/lib.rs:human"),
        "- alpha:src/lib.rs:human -- wontfix: not worth it"
    );
}
