#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_harvest` on a PR closed without merging: why it closed, and what
//! it left open.
//!
//! A closure used to reject its finding on sight. Of the first 16 closed
//! PRs, 15 were engage withdrawals (14 "superseded/obsolete") and one was
//! closed by a human, so that suppressed valid findings. The closed PR now
//! waits `closed` for this review, which classifies the closure into the
//! finding's status and files the follow-ups, exactly as the merged
//! harvest files its own.
//!
//! Same scaffolding as `runner_engage_test`: a real repo with a bare
//! origin, a scripted `gh`, and a backend staging the worker's files.

mod support;

use hunter::config::Config;
use hunter::domain::{BudgetOverride, FindingJobKind, FindingStatus, ForgeName, JobState};
use hunter::scheduler::{Candidate, pick_next, run_harvest};
use hunter::store::{FindingInsert, Store};
use hunter::types::RunResult;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, fresh_store};

const REPO_URL: &str = "https://github.com/acme/widget";
const PR_URL: &str = "https://github.com/acme/widget/pull/7";

/// `gh pr view` of the closed PR, as `view_pr_engage` reads it.
const PR_VIEW_JSON: &str = r#"{"state":"CLOSED","title":"fix the widget","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/widget","headRefOid":"deadbeef"}"#;

/// `gh pr diff` of the closed PR.
const PR_DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old_widget()\n+new_widget()\n";

/// One valid follow-up, so a follow-up that is missing was skipped
/// rather than rejected by ingest.
const FOLLOW_UP: &str = r#"[{"type":"bug","fingerprint":"widget:src/lib.rs:left-open","file":"src/lib.rs","bug_class":"logic","severity":"medium","confidence":0.8,"summary":"left open","detail":"src/lib.rs:1 still has it","evidence_plan":"failing test","introduced_by":"left open by closed PR #7"}]"#;

struct Fixture {
    cfg: Config,
    store: Store,
    db: std::path::PathBuf,
    fid: i64,
    /// Last: fields drop in declaration order, and the directory must
    /// outlive the Store's pool.
    _dir: TempDir,
}

impl Fixture {
    /// A raw read-only look at the database, for columns the Store API has
    /// no reader for.
    async fn scalar(&self, sql: &'static str) -> Option<i64> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(Option<i64>,)> = sqlx::query_as(sql).fetch_optional(&pool).await.unwrap();
        row.and_then(|r| r.0)
    }
}

/// A repo with a finding whose PR #7 closed without merging, unharvested,
/// the finding at `status` — `closed` as sync and a withdrawal leave it
/// (and as migration 016 leaves the closures from before this review).
async fn fixture(label: &str, status: FindingStatus) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "fix/widget");
    let (db, store) = fresh_store(&dir, "harvest").await;
    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    std::fs::rename(&repo.work, Store::repo_dir(&repos_root, rid)).unwrap();
    let (fid, _) = store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "widget:src/lib.rs:old-widget".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "old widget is wrong".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_pr_open(fid, PR_URL).await.unwrap();
    store
        .set_finding_verdict(fid, status, "PR closed without merge")
        .await
        .unwrap();
    store.mark_pr_closed(fid, 7, 1).await.unwrap();

    // Hermetic stubs naming the slots this suite asserts on; the contract
    // test renders the real playbooks.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("harvest-closed.md"),
        "closed: the worktree at {{WORKTREE}} is {{WORKTREE_STATE}}.\n{{PR_DIFF}}\n",
    )
    .unwrap();
    std::fs::write(playbooks.join("harvest.md"), "merged {{WORKTREE}}\n").unwrap();
    let mut cfg = Config::load(dir.path()).expect("load config");
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        db,
        fid,
        _dir: dir,
    }
}

/// `gh` answering `pr diff` with `diff` and every other call with the view.
fn gh(bins: &FakeBins, diff_command: &str) {
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = diff ]; then\n{diff_command}\nexit 0\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
}

fn gh_default(bins: &FakeBins) {
    gh(
        bins,
        &format!("cat <<'__FAKE_EOF__'\n{PR_DIFF}__FAKE_EOF__"),
    );
}

fn close_reason(classification: &str) -> String {
    serde_json::json!({
        "classification": classification,
        "reason": "landed in #9",
        "evidence": "abc1234",
        "holds_while": "the guard at src/z.rs:3 rejects it",
        "depends_on": ["src/z.rs:3"],
    })
    .to_string()
}

/// A worker that leaves `CLOSE-REASON.json` with `body`.
fn classifying(body: String) -> ScriptedBackend {
    ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("CLOSE-REASON.json"), &body).unwrap();
        support::done()
    })
}

/// A worker stopped at the cap with a transcript: what the harness
/// reports as a suspension.
fn suspending() -> ScriptedBackend {
    ScriptedBackend::new(suspended_at)
}

/// What the harness reports for a worker in `tree` stopped at the cap.
fn suspended_at(tree: &std::path::Path) -> RunResult {
    let session = tree.parent().unwrap().join("session").join("session.jsonl");
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(session.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// A closed, unharvested PR whose finding is `closed` is picked for its
/// harvest. A harvested one is not picked again.
#[tokio::test]
async fn a_closed_unharvested_pr_is_picked_once() {
    let f = fixture("harvest-pick", FindingStatus::Closed).await;

    let picked = pick_next(&f.store, &f.cfg, None).await.unwrap();
    match picked {
        Some(Candidate::Finding {
            kind: FindingJobKind::Harvest,
            finding_id,
            ..
        }) => assert_eq!(finding_id, f.fid),
        other => panic!("expected the closed PR's harvest, got {other:?}"),
    }

    f.store.mark_pr_harvested(f.fid, 2).await.unwrap();
    let after = pick_next(&f.store, &f.cfg, None).await.unwrap();
    assert!(
        !matches!(
            after,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                ..
            })
        ),
        "a harvested PR must not be picked again: {after:?}"
    );
}

/// A closure harvested as `abandoned` sends the finding back to `new`, and
/// its re-fix ships as a new PR into the same `pr_state` row. When that PR
/// merges it is due its own harvest: the closed PR's stamp must not carry
/// over and hide it from the queue.
#[tokio::test]
async fn a_refix_of_an_abandoned_pr_is_harvested_when_it_merges() {
    let bins = FakeBins::acquire("harvest-refix");
    gh_default(&bins);
    let f = fixture("harvest-refix", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = run_harvest(
        &f.store,
        &f.cfg,
        &finding,
        &classifying(close_reason("abandoned")),
        None,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("harvested"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::New);

    // The re-fix ships as PR #8, which then merges (as `sync_prs` records it).
    f.store
        .set_finding_pr_open(f.fid, "https://github.com/acme/widget/pull/8")
        .await
        .unwrap();
    f.store
        .set_finding_status(f.fid, FindingStatus::Merged)
        .await
        .unwrap();
    f.store.mark_pr_merged(f.fid, 8, 3).await.unwrap();

    let pending: Vec<i64> = f
        .store
        .list_pending_harvest()
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(pending, [f.fid], "merged PR #8 awaits its own harvest");
}

/// Each classification lands as its status, with the reason and the
/// evidence as the verdict. Only `wrong` and `unwanted` suppress, and only
/// `wrong` is anchored to the commit the harvest judged: `unwanted` is the
/// maintainers' decision, which no code change lapses.
#[tokio::test]
async fn each_classification_lands_as_its_status() {
    let bins = FakeBins::acquire("harvest-classes");
    gh_default(&bins);
    let table = [
        ("superseded", FindingStatus::Superseded),
        ("duplicate", FindingStatus::Superseded),
        ("obsolete", FindingStatus::Superseded),
        ("wrong", FindingStatus::Rejected),
        ("unwanted", FindingStatus::Wontfix),
        ("abandoned", FindingStatus::New),
    ];
    for (class, expected) in table {
        let f = fixture(&format!("harvest-class-{class}"), FindingStatus::Closed).await;
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

        let summary = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying(close_reason(class)),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome.as_deref(),
            Some("harvested"),
            "{class}: {summary:?}"
        );
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.status, expected, "{class}");
        assert_eq!(
            after.verdict_reason.as_deref(),
            Some(format!("{class}: landed in #9 (evidence: abc1234)").as_str()),
            "{class}"
        );
        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        assert!(
            ps.harvested_at.is_some(),
            "{class}: must be marked harvested"
        );
        let anchors = f
            .store
            .suppression_anchors(finding.repo_id, finding.kind.as_str())
            .await
            .unwrap();
        let anchor = anchors.get(&f.fid);
        if after.status == FindingStatus::Rejected {
            let anchor = anchor.unwrap_or_else(|| panic!("{class}: not anchored"));
            let judged = f.store.pinned_sha(summary.job_id.unwrap()).await.unwrap();
            assert_eq!(Some(&anchor.sha), judged.as_ref(), "{class}");
            assert!(
                anchor.files.contains(&"src/z.rs".to_owned()),
                "{class}: {anchor:?}"
            );
            assert_eq!(
                anchor.holds_while.as_deref(),
                Some("the guard at src/z.rs:3 rejects it"),
                "{class}"
            );
        } else {
            assert_eq!(anchor, None, "{class}");
        }
    }
}

/// A `wrong` the current code proves, with no maintainer having said so,
/// has no quote to give: the playbook tells the worker to leave it empty
/// or out rather than invent one, so either form must land the verdict.
#[tokio::test]
async fn a_wrong_without_a_maintainer_quote_lands() {
    let bins = FakeBins::acquire("harvest-wrong-unquoted");
    gh_default(&bins);
    let bodies = [
        r#"{"classification":"wrong","reason":"there is no bug","evidence":"src/lib.rs:1"}"#,
        r#"{"classification":"wrong","reason":"there is no bug","evidence":"src/lib.rs:1","maintainer_quote":""}"#,
    ];
    for (n, body) in bodies.into_iter().enumerate() {
        let f = fixture(
            &format!("harvest-wrong-unquoted-{n}"),
            FindingStatus::Closed,
        )
        .await;
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

        let summary = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying(body.to_owned()),
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some("harvested"), "{body}");
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.status, FindingStatus::Rejected, "{body}");
        assert_eq!(
            after.verdict_reason.as_deref(),
            Some("wrong: there is no bug (evidence: src/lib.rs:1)"),
            "{body}"
        );
    }
}

/// Follow-ups of a closed PR are filed like the merged harvest's: by the
/// harvest job, against the finding whose PR it reviewed.
#[tokio::test]
async fn follow_ups_are_filed_with_their_provenance() {
    let bins = FakeBins::acquire("harvest-followups");
    gh_default(&bins);
    let f = fixture("harvest-followups", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("CLOSE-REASON.json"), close_reason("superseded")).unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        support::done()
    });

    let summary = run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let job = summary.job_id.unwrap();
    assert_eq!(
        summary.ingest.as_ref().map(|i| i.inserted),
        Some(1),
        "{summary:?}"
    );
    let found_by = f
        .scalar(
            "SELECT found_by_job FROM findings WHERE fingerprint = 'widget:src/lib.rs:left-open'",
        )
        .await;
    assert_eq!(
        found_by,
        Some(job),
        "the follow-up must name the harvest job"
    );
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "harvest"
            && e.finding_id == Some(f.fid)
            && e.job_id == Some(job)
            && e.message.contains("+1 follow-up")),
        "the filing must be logged against the reviewed finding: {events:?}"
    );
}

/// A follow-up that shares a rejected finding's fingerprint is a duplicate,
/// even when that rejection has lapsed (here: its commit cannot be compared
/// with anything, which marks it CHANGED for a scan). Only a hunt or
/// analysis scan reopens a lapsed rejection, because only a scan is shown
/// the suppression list and told to re-check it before filing. The harvest
/// worker never saw it, and its tree may be the PR's head rather than the
/// default branch the verdict was judged against.
#[tokio::test]
async fn a_follow_up_never_reopens_a_rejected_finding() {
    let bins = FakeBins::acquire("harvest-followup-rejected");
    gh_default(&bins);
    let f = fixture("harvest-followup-rejected", FindingStatus::Closed).await;
    let repo_id = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
    let (rejected, _) = f
        .store
        .upsert_finding(
            repo_id,
            &FindingInsert {
                fingerprint: "widget:src/lib.rs:left-open".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Low,
                confidence: 0.5,
                summary: "judged wrong".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    let anchor = hunter::store::VerdictAnchor {
        sha: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        files: vec!["src/lib.rs".to_owned()],
        holds_while: Some("nothing reaches it".to_owned()),
    };
    f.store
        .set_anchored_verdict(
            rejected,
            FindingStatus::Rejected,
            "wrong: unreachable",
            Some(&anchor),
        )
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("CLOSE-REASON.json"), close_reason("superseded")).unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        support::done()
    });

    let summary = run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let ingest = summary.ingest.as_ref().expect("follow-ups ingested");
    assert_eq!(
        (ingest.inserted, ingest.duplicates, ingest.reopened),
        (0, 1, 0)
    );
    let after = f.store.get_finding(rejected).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Rejected);
    assert_eq!(after.summary, "judged wrong");
    assert_eq!(
        f.store.verdict_anchor(rejected).await.unwrap(),
        Some(anchor)
    );
}

/// A review that does not say why the PR closed is a failed harvest: it
/// counts toward the identical-failure streak, and at the limit the PR is
/// given up on with the finding left `closed` — never suppressed.
#[tokio::test]
async fn an_invalid_close_reason_counts_toward_the_streak() {
    let bins = FakeBins::acquire("harvest-invalid");
    gh_default(&bins);
    let f = fixture("harvest-invalid", FindingStatus::Closed).await;

    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying(close_reason("maybe")),
            None,
        )
        .await
        .unwrap();
        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        if attempt < 3 {
            assert_eq!(ps.harvest_attempts, attempt, "{summary:?}");
            assert_eq!(ps.harvested_at, None, "retried, not given up: {summary:?}");
        } else {
            assert!(
                ps.harvested_at.is_some(),
                "given up at the limit: {summary:?}"
            );
        }
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("stuck".into())
        ]
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("gave up after 3")),
        "{events:?}"
    );
}

/// A harvest whose tree can never be created is a failed harvest like any
/// other: retried, and at the limit the PR is given up on with the finding
/// left `closed`. Left pending with nothing counted, the harvest tier
/// (above recheck and fix) would take it again every cycle and starve
/// everything below it. The tree fails here because `jobs/` is a file:
/// the harvest fetches its default branch by name first, so a branch
/// origin lacks would fail before any tree is attempted.
#[tokio::test]
async fn a_harvest_whose_tree_can_never_be_made_is_given_up() {
    let bins = FakeBins::acquire("harvest-never-a-tree");
    gh_default(&bins);
    let f = fixture("harvest-never-a-tree", FindingStatus::Closed).await;
    std::fs::write(f.cfg.work_root.join("jobs"), "not a directory").unwrap();
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(&f.store, &f.cfg, &finding, &never, None)
            .await
            .unwrap();
        assert!(
            summary
                .failure
                .as_deref()
                .is_some_and(|r| r.starts_with("workspace not created")),
            "{summary:?}"
        );
        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        if attempt < 3 {
            assert_eq!(ps.harvest_attempts, attempt, "{summary:?}");
            assert_eq!(ps.harvested_at, None, "retried, not given up: {summary:?}");
        } else {
            assert!(
                ps.harvested_at.is_some(),
                "given up at the limit: {summary:?}"
            );
        }
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("stuck".into())
        ]
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
}

/// How many findings carry `fingerprint`.
async fn filed(f: &Fixture, fingerprint: &str) -> usize {
    f.store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap()
        .iter()
        .filter(|x| x.fingerprint == fingerprint)
        .count()
}

/// A worker that leaves `close` as `CLOSE-REASON.json` and the
/// `FOLLOW_UP` entry, filed under `fingerprint`, as `FOLLOW-UPS.json`.
fn classifying_with_follow_up(close: String, fingerprint: &'static str) -> ScriptedBackend {
    let followups = FOLLOW_UP.replace("widget:src/lib.rs:left-open", fingerprint);
    ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("CLOSE-REASON.json"), &close).unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), &followups).unwrap();
        support::done()
    })
}

/// Whether a `harvest` event says the finding's FOLLOW-UPS.json was left
/// unfiled for `why`.
async fn unfiled_because(f: &Fixture, why: &str) -> bool {
    let expected = format!("#{}: FOLLOW-UPS.json not filed: {why}", f.fid);
    f.store
        .recent_events(50)
        .await
        .unwrap()
        .iter()
        .any(|e| e.kind == "harvest" && e.finding_id == Some(f.fid) && e.message == expected)
}

/// A harvest that will be retried is reviewed again from scratch,
/// follow-ups included, and the retry picks its own slugs: filing the
/// failed attempt's follow-ups would file the same open work twice. Only
/// the attempt that lands files them.
#[tokio::test]
async fn a_retried_harvest_files_no_follow_ups() {
    let bins = FakeBins::acquire("harvest-retry-followups");
    gh_default(&bins);
    let f = fixture("harvest-retry-followups", FindingStatus::Closed).await;

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let first = run_harvest(
        &f.store,
        &f.cfg,
        &finding,
        &classifying_with_follow_up(close_reason("maybe"), "widget:src/lib.rs:left-open"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(first.outcome.as_deref(), Some("retry"), "{first:?}");
    assert_eq!(
        filed(&f, "widget:src/lib.rs:left-open").await,
        0,
        "a retried attempt files nothing: {first:?}"
    );
    assert!(
        unfiled_because(&f, "the attempt will be redone").await,
        "the unfiled follow-ups are logged, not silently dropped"
    );

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let second = run_harvest(
        &f.store,
        &f.cfg,
        &finding,
        &classifying_with_follow_up(close_reason("superseded"), "widget:src/lib.rs:still-open"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(second.outcome.as_deref(), Some("harvested"), "{second:?}");
    assert_eq!(filed(&f, "widget:src/lib.rs:still-open").await, 1);
    assert_eq!(
        filed(&f, "widget:src/lib.rs:left-open").await,
        0,
        "the same work must be filed once"
    );
}

/// A harvest given up on is not reviewed again, so its last attempt's
/// follow-ups are the only ones the PR will ever get: they are filed.
#[tokio::test]
async fn a_harvest_given_up_on_files_its_last_follow_ups() {
    let bins = FakeBins::acquire("harvest-stuck-followups");
    gh_default(&bins);
    let f = fixture("harvest-stuck-followups", FindingStatus::Closed).await;

    let mut outcomes = Vec::new();
    for _ in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying_with_follow_up(close_reason("maybe"), "widget:src/lib.rs:left-open"),
            None,
        )
        .await
        .unwrap();
        outcomes.push((
            summary.outcome,
            filed(&f, "widget:src/lib.rs:left-open").await,
        ));
    }

    assert_eq!(
        outcomes,
        [
            (Some("retry".into()), 0),
            (Some("retry".into()), 0),
            (Some("stuck".into()), 1)
        ]
    );
}

/// A suspending worker that has written the `FOLLOW_UP` entry by then.
fn suspending_with_follow_up() -> ScriptedBackend {
    ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        suspended_at(tree)
    })
}

/// Resume the suspended harvest `pick_next` offers with `worker`.
async fn resume_harvest(f: &Fixture, worker: &ScriptedBackend) -> hunter::scheduler::CycleSummary {
    let plan = match pick_next(&f.store, &f.cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended harvest to be resumed, got {other:?}"),
    };
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    run_harvest(&f.store, &f.cfg, &finding, worker, Some(&plan))
        .await
        .unwrap()
}

/// A suspension files nothing: its tree keeps FOLLOW-UPS.json, and the
/// resume that lands files it. A resume that fails is retried cold, which
/// files the same open work under its own slug, so the suspended run's
/// follow-ups must not have been filed either.
#[tokio::test]
async fn a_suspended_harvest_files_its_follow_ups_once() {
    let bins = FakeBins::acquire("harvest-suspend-followups");
    gh_default(&bins);
    for resume_lands in [true, false] {
        let f = fixture("harvest-suspend-followups", FindingStatus::Closed).await;
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let suspended = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &suspending_with_follow_up(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(suspended.outcome.as_deref(), Some("suspended"));
        assert_eq!(
            filed(&f, "widget:src/lib.rs:left-open").await,
            0,
            "a suspension files nothing: {suspended:?}"
        );
        assert!(
            unfiled_because(&f, "kept for the resume").await,
            "the unfiled follow-ups are logged, not silently dropped"
        );

        if resume_lands {
            let resumed = resume_harvest(&f, &classifying(close_reason("superseded"))).await;
            assert_eq!(resumed.outcome.as_deref(), Some("harvested"), "{resumed:?}");
            assert_eq!(
                filed(&f, "widget:src/lib.rs:left-open").await,
                1,
                "the landed resume files what the suspension left: {resumed:?}"
            );
            continue;
        }
        let resumed = resume_harvest(&f, &classifying(close_reason("maybe"))).await;
        assert_eq!(resumed.outcome.as_deref(), Some("retry"), "{resumed:?}");
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let cold = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying_with_follow_up(close_reason("superseded"), "widget:src/lib.rs:still-open"),
            None,
        )
        .await
        .unwrap();
        assert_eq!(cold.outcome.as_deref(), Some("harvested"), "{cold:?}");
        assert_eq!(
            (
                filed(&f, "widget:src/lib.rs:left-open").await,
                filed(&f, "widget:src/lib.rs:still-open").await
            ),
            (0, 1),
            "the same work must be filed once"
        );
    }
}

/// A classification the database refuses to record leaves the finding
/// `closed` and pending for the next cycle. It is not the worker's
/// failure, so it records no attempt toward the streak; once the database
/// takes writes again, the next run lands the classification.
#[tokio::test]
async fn a_classification_that_cannot_be_recorded_is_retried() {
    let bins = FakeBins::acquire("harvest-record-fails");
    gh_default(&bins);
    let f = fixture("harvest-record-fails", FindingStatus::Closed).await;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_harvest_stamp BEFORE UPDATE OF harvested_at ON pr_state \
         BEGIN SELECT RAISE(ABORT, 'injected harvested_at failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = run_harvest(
        &f.store,
        &f.cfg,
        &finding,
        &classifying(close_reason("superseded")),
        None,
    )
    .await
    .unwrap();

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    let pending = f.store.list_pending_harvest().await.unwrap();
    assert_eq!(
        (
            after.status,
            ps.harvested_at,
            pending.iter().any(|p| p.id == f.fid)
        ),
        (FindingStatus::Closed, None, true),
        "(status, harvested_at, pending): nothing landed and the finding waits for the next cycle"
    );
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("PR closed without merge")
    );
    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert_eq!(ps.harvest_attempts, 0, "not counted toward the streak");
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("injected harvested_at failure")),
        "{events:?}"
    );

    sqlx::query("DROP TRIGGER no_harvest_stamp")
        .execute(&pool)
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = run_harvest(
        &f.store,
        &f.cfg,
        &finding,
        &classifying(close_reason("superseded")),
        None,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("harvested"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Superseded);
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some());
}

/// The retry path of an unrecordable classification still spends a
/// `once` override, as every other end of an attempt does, and keeps an
/// `exempt` one, which lasts until a human clears it.
#[tokio::test]
async fn a_classification_that_cannot_be_recorded_spends_only_a_once_override() {
    let bins = FakeBins::acquire("harvest-record-fails-override");
    gh_default(&bins);
    for (mode, left) in [
        (BudgetOverride::Once, None),
        (BudgetOverride::Exempt, Some(BudgetOverride::Exempt)),
    ] {
        let f = fixture(
            &format!("harvest-record-fails-{mode}"),
            FindingStatus::Closed,
        )
        .await;
        f.store
            .set_budget_override(f.fid, Some(mode))
            .await
            .unwrap();
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db),
        )
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER no_harvest_stamp BEFORE UPDATE OF harvested_at ON pr_state \
             BEGIN SELECT RAISE(ABORT, 'injected harvested_at failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &f.store,
            &f.cfg,
            &finding,
            &classifying(close_reason("superseded")),
            None,
        )
        .await
        .unwrap();

        assert_eq!(
            summary.outcome.as_deref(),
            Some("retry"),
            "{mode}: {summary:?}"
        );
        let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
        assert_eq!(after.budget_override, left, "{mode} override");
    }
}

/// Every other end of a harvest attempt spends a `once` override too — a
/// suspension, a close reason it cannot use, a landed review — and keeps
/// an `exempt` one.
#[tokio::test]
async fn every_end_of_a_harvest_attempt_spends_only_a_once_override() {
    let bins = FakeBins::acquire("harvest-override-ends");
    gh_default(&bins);
    for end in ["suspended", "retry", "harvested"] {
        for (mode, left) in [
            (BudgetOverride::Once, None),
            (BudgetOverride::Exempt, Some(BudgetOverride::Exempt)),
        ] {
            let f = fixture(
                &format!("harvest-override-{end}-{mode}"),
                FindingStatus::Closed,
            )
            .await;
            f.store
                .set_budget_override(f.fid, Some(mode))
                .await
                .unwrap();

            let worker = match end {
                "suspended" => suspending(),
                "retry" => classifying(close_reason("maybe")),
                _ => classifying(close_reason("superseded")),
            };
            let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
            let summary = run_harvest(&f.store, &f.cfg, &finding, &worker, None)
                .await
                .unwrap();

            assert_eq!(summary.outcome.as_deref(), Some(end), "{end} {mode}");
            let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
            assert_eq!(after.budget_override, left, "{end} {mode} override");
        }
    }
}

/// A verdict a human sets while the harvest worker runs is kept: the
/// classification only replaces `closed`. The PR was still reviewed, so
/// the harvest stamp lands and it is not picked again, and an event says
/// which verdict was kept over which classification.
#[tokio::test(flavor = "multi_thread")]
async fn a_verdict_set_while_the_harvest_ran_is_kept() {
    let bins = FakeBins::acquire("harvest-human-meanwhile");
    gh_default(&bins);
    let f = fixture("harvest-human-meanwhile", FindingStatus::Closed).await;
    let (db, fid) = (f.db.clone(), f.fid);
    let backend = ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("CLOSE-REASON.json"), close_reason("superseded")).unwrap();
        // The human's verdict, through the API's own write, mid-run.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                Store::connect(&db)
                    .await
                    .unwrap()
                    .set_finding_verdict(fid, FindingStatus::Wontfix, "not worth it")
                    .await
                    .unwrap();
            });
        });
        support::done()
    });

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("harvested"), "{summary:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Wontfix);
    assert_eq!(after.verdict_reason.as_deref(), Some("not worth it"));
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some(), "the PR was reviewed");
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.finding_id == Some(f.fid)
            && e.message
                .contains("kept the human's verdict wontfix; closure classified as superseded")),
        "{events:?}"
    );
}

/// A closed PR whose diff will not load fails before any job exists, so
/// the attempt must still count toward the streak: otherwise the PR stays
/// pending for good and, as the oldest one, is picked on every cycle. The
/// error text differs per attempt, as a real outage's does, and the
/// streak must accumulate regardless.
#[tokio::test]
async fn a_failing_pr_diff_counts_toward_the_streak() {
    let bins = FakeBins::acquire("harvest-diff-fails");
    gh(&bins, "echo \"HTTP 502 at $(date +%s%N)\" >&2\nexit 1");
    let f = fixture("harvest-diff-fails", FindingStatus::Closed).await;
    let never_runs = ScriptedBackend::new(|_| panic!("no worker without the PR's diff"));

    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = run_harvest(&f.store, &f.cfg, &finding, &never_runs, None)
            .await
            .expect_err("a failed diff fails the harvest");
        assert!(err.to_string().contains("diff failed"), "{err}");

        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        let picked = pick_next(&f.store, &f.cfg, None).await.unwrap();
        let still_pending = matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                finding_id,
                ..
            }) if finding_id == f.fid
        );
        if attempt < 3 {
            assert_eq!(ps.harvest_attempts, attempt, "attempt {attempt}");
            assert_eq!(ps.harvested_at, None, "retried, not given up");
            assert!(still_pending, "attempt {attempt}: {picked:?}");
        } else {
            assert!(ps.harvested_at.is_some(), "given up at the limit");
            assert!(
                !still_pending,
                "a given-up PR must not be picked: {picked:?}"
            );
        }
    }

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("gave up after 3")),
        "{events:?}"
    );
}

/// The same for a PR whose view will not load, which comes before the
/// diff and is shared with the merged harvest: a merged PR is the case
/// here, as the view is all it fetches.
#[tokio::test]
async fn a_failing_pr_view_counts_toward_the_streak() {
    let bins = FakeBins::acquire("harvest-view-fails");
    bins.script("gh", "echo \"HTTP 502 at $(date +%s%N)\" >&2\nexit 1");
    let f = fixture("harvest-view-fails", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let never_runs = ScriptedBackend::new(|_| panic!("no worker without the PR's view"));

    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = run_harvest(&f.store, &f.cfg, &finding, &never_runs, None)
            .await
            .expect_err("a failed view fails the harvest");
        assert!(err.to_string().contains("view failed"), "{err}");

        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        let picked = pick_next(&f.store, &f.cfg, None).await.unwrap();
        let still_pending = matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                finding_id,
                ..
            }) if finding_id == f.fid
        );
        if attempt < 3 {
            assert_eq!(ps.harvest_attempts, attempt, "attempt {attempt}");
            assert_eq!(ps.harvested_at, None, "retried, not given up");
            assert!(still_pending, "attempt {attempt}: {picked:?}");
        } else {
            assert!(ps.harvested_at.is_some(), "given up at the limit");
            assert!(
                !still_pending,
                "a given-up PR must not be picked: {picked:?}"
            );
        }
    }

    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("gave up after 3")),
        "{events:?}"
    );
}

/// The cold review runs in a tree at the default branch, which does not
/// hold the PR's changes: the prompt must say so and carry the diff.
#[tokio::test]
async fn the_prompt_carries_the_diff_and_what_the_tree_holds() {
    let bins = FakeBins::acquire("harvest-prompt");
    gh_default(&bins);
    let f = fixture("harvest-prompt", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = classifying(close_reason("superseded"));

    run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let runs = backend.runs();
    assert_eq!(runs.len(), 1);
    let prompt = &runs[0].prompt;
    assert!(prompt.contains(PR_DIFF), "{prompt}");
    assert!(
        prompt.contains("checked out at main's HEAD, which does NOT contain this PR's changes"),
        "{prompt}"
    );
    assert_eq!(runs[0].resume_from, None);
}

/// A diff past the cap is cut, and says it was: a worker must not take a
/// truncated diff for the whole PR.
#[tokio::test]
async fn a_huge_diff_is_cut_with_a_marker() {
    let bins = FakeBins::acquire("harvest-huge-diff");
    // 60,000 lines of "+x\n": 180,000 characters.
    gh(&bins, "yes '+x' | head -n 60000");
    let f = fixture("harvest-huge-diff", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = classifying(close_reason("superseded"));

    run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let prompt = &backend.runs()[0].prompt;
    assert!(
        prompt.contains("[diff truncated here: 140000 more characters not shown]"),
        "{}",
        &prompt[prompt.len().saturating_sub(300)..]
    );
    assert!(prompt.len() < 41_000, "{} bytes", prompt.len());
}

/// A closed PR whose finding a human moved on after the closure — back to
/// `queued` for another fix, or settled as `wontfix` — is not harvested:
/// the harvest's classification would overwrite that decision. Only
/// `closed` (the steady state) is picked.
#[tokio::test]
async fn a_closed_pr_whose_finding_a_human_moved_on_is_not_picked() {
    for status in [FindingStatus::Wontfix, FindingStatus::Queued] {
        let f = fixture(&format!("harvest-moved-on-{status}"), status).await;
        let picked = pick_next(&f.store, &f.cfg, None).await.unwrap();
        assert!(
            !matches!(
                picked,
                Some(Candidate::Finding {
                    kind: FindingJobKind::Harvest,
                    ..
                })
            ),
            "{status}: must not be harvested: {picked:?}"
        );
    }
}

/// A human who rejects the finding after its PR closed has decided it: the
/// harvest must not pick it up and overwrite that verdict with its own
/// classification. Every closure before this review also left `rejected`,
/// so the two cannot be told apart by status; migration 016 moved those to
/// `closed` once, and only `closed` is picked from then on.
#[tokio::test]
async fn a_finding_rejected_after_its_pr_closed_is_not_picked() {
    let f = fixture("harvest-rejected-later", FindingStatus::Closed).await;
    f.store
        .set_finding_verdict(f.fid, FindingStatus::Rejected, "wrong after all")
        .await
        .unwrap();

    let picked = pick_next(&f.store, &f.cfg, None).await.unwrap();
    assert!(
        !matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                ..
            })
        ),
        "a human's rejection must not be harvested: {picked:?}"
    );
}

/// A diff with three- and four-backtick fence lines (a markdown file) stays
/// one block: the fence is longer than any backtick run in it, so the
/// block closes only after the whole diff.
#[tokio::test]
async fn a_diff_containing_fences_stays_one_block() {
    const MD_DIFF: &str =
        "diff --git a/README.md b/README.md\n+```rust\n+let x = 1;\n+```\n+````\n+nested\n+````\n";
    let bins = FakeBins::acquire("harvest-fenced-diff");
    gh(
        &bins,
        &format!("cat <<'__FAKE_EOF__'\n{MD_DIFF}__FAKE_EOF__"),
    );
    let f = fixture("harvest-fenced-diff", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = classifying(close_reason("superseded"));

    run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let prompt = &backend.runs()[0].prompt;
    assert!(
        prompt.contains(&format!("\n`````diff\n{MD_DIFF}`````\n")),
        "{prompt}"
    );
}

/// How many times `gh pr diff` ran.
fn diff_fetches(bins: &FakeBins) -> usize {
    bins.calls_to("gh")
        .iter()
        .filter(|c| {
            c.get(1).map(String::as_str) == Some("pr")
                && c.get(2).map(String::as_str) == Some("diff")
        })
        .count()
}

/// A merged PR's changes are on the default branch the tree is made
/// from, so its harvest has no use for the diff. Fetching it anyway
/// costs a forge call per harvest and, for a large PR, a failure that
/// would fail a review which never needed it.
#[tokio::test]
async fn a_merged_harvest_fetches_no_diff() {
    let bins = FakeBins::acquire("harvest-merged-no-diff");
    gh_default(&bins);
    let f = fixture("harvest-merged-no-diff", FindingStatus::PrOpen).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::noop();

    run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    let runs = backend.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].prompt.starts_with("merged "), "{}", runs[0].prompt);
    assert_eq!(diff_fetches(&bins), 0, "{:?}", bins.calls_to("gh"));
}

/// A suspended closed-PR review picks up in its own transcript, which
/// already holds the playbook and the diff: the resume is told only to
/// carry on. Re-sending the playbook would restart the review on top of
/// the half done one, and re-fetching the diff is a forge call whose
/// result nobody reads.
#[tokio::test]
async fn a_resumed_closed_harvest_gets_neither_the_playbook_nor_the_diff_again() {
    let bins = FakeBins::acquire("harvest-closed-resume");
    gh_default(&bins);
    let f = fixture("harvest-closed-resume", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let suspending = suspending();
    let cold = run_harvest(&f.store, &f.cfg, &finding, &suspending, None)
        .await
        .unwrap();
    assert_eq!(cold.state, Some(JobState::Suspended), "{cold:?}");
    assert_eq!(diff_fetches(&bins), 1, "the cold review carries the diff");

    let plan = match pick_next(&f.store, &f.cfg, None).await.unwrap() {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended harvest to be resumed, got {other:?}"),
    };
    assert!(!plan.handoff);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = classifying(close_reason("superseded"));
    run_harvest(&f.store, &f.cfg, &finding, &backend, Some(&plan))
        .await
        .unwrap();

    let runs = backend.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].prompt,
        "Continue the work you were doing in this session. You were interrupted; \
         pick up where you left off."
    );
    assert_eq!(
        runs[0].resume_from.as_deref(),
        Some(plan.session_file.as_path())
    );
    assert_eq!(diff_fetches(&bins), 1, "{:?}", bins.calls_to("gh"));
}
