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
use hunter::domain::{
    BudgetOverride, FindingJobKind, FindingStatus, ForgeName, JobState, ReleaseAction,
};
use hunter::scheduler::{Attempt, Candidate, Resumed, pick_next, run_harvest};
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

    /// A raw write, for states the Store API never produces.
    async fn exec(&self, sql: &'static str) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query(sql).execute(&pool).await.unwrap();
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
    let mut cfg = support::load_config(dir.path());
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

    let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    match picked {
        Some(Candidate::Finding {
            kind: FindingJobKind::Harvest,
            finding_id,
            ..
        }) => assert_eq!(finding_id, f.fid),
        other => panic!("expected the closed PR's harvest, got {other:?}"),
    }

    support::scope(&f.store)
        .store()
        .mark_pr_harvested(f.fid, 2)
        .await
        .unwrap();
    let after = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
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
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &classifying(close_reason("abandoned")),
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
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying(close_reason(class)),
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
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying(body.to_owned()),
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

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
/// counts toward the identical-failure streak, and at the limit the
/// harvest is held for the operator with the finding left `closed` —
/// never suppressed, and never stamped harvested unreviewed.
#[tokio::test]
async fn an_invalid_close_reason_counts_toward_the_streak() {
    let bins = FakeBins::acquire("harvest-invalid");
    gh_default(&bins);
    let f = fixture("harvest-invalid", FindingStatus::Closed).await;

    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying(close_reason("maybe")),
        )
        .await
        .unwrap();
        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        if attempt < 3 {
            assert_eq!(harvest_streak(&f).await, attempt, "{summary:?}");
            assert_eq!(ps.harvested_at, None, "retried, not given up: {summary:?}");
        } else {
            assert_eq!(ps.harvested_at, None, "held, not stamped: {summary:?}");
        }
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("held".into())
        ]
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_held(&f, ReleaseAction::Classify).await;
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("held after 3")),
        "{events:?}"
    );
}

/// The fixture's harvest is held as `stuck` and skipped by the harvest
/// tier, and `last` is how a closed PR (`classify`) or a merged one
/// (`mark_harvested`) can be ended by hand.
async fn assert_held(f: &Fixture, last: ReleaseAction) {
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
        .await
        .expect("held at the limit");
    assert_eq!(hold.reason, hunter::domain::HoldReason::Stuck);
    assert_eq!(hold.actions().last(), Some(&last), "{hold:?}");
    let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(
            picked,
            Some(Candidate::Finding { kind: FindingJobKind::Harvest, finding_id, .. })
                if finding_id == f.fid
        ),
        "held work is skipped: {picked:?}"
    );
}

/// The fixture finding's harvest streak.
async fn harvest_streak(f: &Fixture) -> i64 {
    f.store
        .work_streak(f.fid, FindingJobKind::Harvest)
        .await
        .unwrap()
        .streak
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
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &classifying_with_follow_up(close_reason("maybe"), "widget:src/lib.rs:left-open"),
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
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &classifying_with_follow_up(close_reason("superseded"), "widget:src/lib.rs:still-open"),
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

/// A held harvest files no follow-ups: the operator's `continue` or
/// `restart` reviews the PR again, and that review files the open work.
/// Before holds, a harvest given up on was never reviewed again, so its
/// last attempt's follow-ups were filed.
#[tokio::test]
async fn a_held_harvest_files_no_follow_ups() {
    let bins = FakeBins::acquire("harvest-stuck-followups");
    gh_default(&bins);
    let f = fixture("harvest-stuck-followups", FindingStatus::Closed).await;

    let mut outcomes = Vec::new();
    for _ in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying_with_follow_up(close_reason("maybe"), "widget:src/lib.rs:left-open"),
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
            (Some("held".into()), 0)
        ]
    );
    assert!(
        unfiled_because(&f, "the attempt will be redone").await,
        "the unfiled follow-ups are logged, not silently dropped"
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
    let plan = match pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended harvest to be resumed, got {other:?}"),
    };
    run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        worker,
    )
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
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &suspending_with_follow_up(),
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
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying_with_follow_up(close_reason("superseded"), "widget:src/lib.rs:still-open"),
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
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &classifying(close_reason("superseded")),
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
    assert_eq!(harvest_streak(&f).await, 0, "not counted toward the streak");
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
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &classifying(close_reason("superseded")),
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
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &classifying(close_reason("superseded")),
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
            let summary = run_harvest(
                &support::scope(&f.store),
                f.store.test_lane(),
                &f.cfg,
                Attempt::Cold(&finding),
                &worker,
            )
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
    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
        let err = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never_runs,
        )
        .await
        .expect_err("a failed diff fails the harvest");
        assert!(err.to_string().contains("diff failed"), "{err}");

        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        let still_pending = matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                finding_id,
                ..
            }) if finding_id == f.fid
        );
        if attempt < 3 {
            assert_eq!(harvest_streak(&f).await, attempt, "attempt {attempt}");
            assert_eq!(ps.harvested_at, None, "retried, not given up");
            assert!(still_pending, "attempt {attempt}: {picked:?}");
        } else {
            assert_eq!(ps.harvested_at, None, "held, not stamped");
            assert!(!still_pending, "a held PR must not be picked: {picked:?}");
        }
    }

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("held after 3")),
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
        let err = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never_runs,
        )
        .await
        .expect_err("a failed view fails the harvest");
        assert!(err.to_string().contains("view failed"), "{err}");

        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        let still_pending = matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                finding_id,
                ..
            }) if finding_id == f.fid
        );
        if attempt < 3 {
            assert_eq!(harvest_streak(&f).await, attempt, "attempt {attempt}");
            assert_eq!(ps.harvested_at, None, "retried, not given up");
            assert!(still_pending, "attempt {attempt}: {picked:?}");
        } else {
            assert_eq!(ps.harvested_at, None, "held, not stamped");
            assert!(!still_pending, "a held PR must not be picked: {picked:?}");
        }
    }

    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("held after 3")),
        "{events:?}"
    );
}

/// The same for a clone whose default branch will not fetch, which comes
/// before the view: here the clone is gone, as migration 007 leaves a repo
/// whose directory was not moved. Uncounted, the harvest is picked on
/// every cycle and the hunt that would re-clone never runs. (#62)
#[tokio::test]
async fn a_failing_fetch_counts_toward_the_streak() {
    let bins = FakeBins::acquire("harvest-fetch-fails");
    gh_default(&bins);
    let f = fixture("harvest-fetch-fails", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let repo = f.store.list_repos().await.unwrap().remove(0);
    std::fs::remove_dir_all(&repo.path).unwrap();
    let never_runs = ScriptedBackend::new(|_| panic!("no worker without a fetched clone"));

    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never_runs,
        )
        .await
        .expect_err("a failed fetch fails the harvest");
        assert!(err.to_string().contains("fetch failed"), "{err}");

        let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
        let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
        let still_pending = matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Harvest,
                finding_id,
                ..
            }) if finding_id == f.fid
        );
        if attempt < 3 {
            assert_eq!(harvest_streak(&f).await, attempt, "attempt {attempt}");
            assert_eq!(ps.harvested_at, None, "retried, not given up");
            assert!(still_pending, "attempt {attempt}: {picked:?}");
        } else {
            assert_eq!(ps.harvested_at, None, "held, not stamped");
            assert!(!still_pending, "a held PR must not be picked: {picked:?}");
        }
    }

    assert_held(&f, ReleaseAction::MarkHarvested).await;
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("held after 3")),
        "{events:?}"
    );
}

/// A harvest whose tree can never be created is a failed harvest like any
/// other: retried, and at the limit held for the operator with the finding
/// left `closed`. Left pending with nothing counted, the harvest tier
/// (above recheck and fix) would take it again every cycle and starve
/// everything below it. The tree fails here because `jobs/` is a file:
/// the harvest fetches its default branch by name first, so a branch
/// origin lacks would fail before any tree is attempted. No worker ran,
/// so the hold has no checkpoint to continue.
#[tokio::test]
async fn a_harvest_whose_tree_can_never_be_made_is_held() {
    let bins = FakeBins::acquire("harvest-never-a-tree");
    gh_default(&bins);
    let f = fixture("harvest-never-a-tree", FindingStatus::Closed).await;
    std::fs::write(f.cfg.work_root.join("jobs"), "not a directory").unwrap();
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let mut outcomes = Vec::new();
    for attempt in 1..=3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never,
        )
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
            assert_eq!(harvest_streak(&f).await, attempt, "{summary:?}");
        }
        assert_eq!(ps.harvested_at, None, "never stamped: {summary:?}");
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("held".into())
        ]
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_held(&f, ReleaseAction::Classify).await;
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
        .await
        .unwrap();
    assert_eq!(hold.checkpoint_job_id, None);
}

/// The cold review runs in a tree at the default branch, which does not
/// hold the PR's changes: the prompt must say so, name that commit, and
/// carry the diff.
#[tokio::test]
async fn the_prompt_carries_the_diff_and_what_the_tree_holds() {
    let bins = FakeBins::acquire("harvest-prompt");
    gh_default(&bins);
    let f = fixture("harvest-prompt", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = classifying(close_reason("superseded"));

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    let runs = backend.runs();
    assert_eq!(runs.len(), 1);
    let prompt = &runs[0].prompt;
    assert!(prompt.contains(PR_DIFF), "{prompt}");
    let job = summary.job_id.unwrap();
    let base = f
        .store
        .base_sha(job)
        .await
        .unwrap()
        .expect("a recorded base");
    assert_eq!(
        Some(&base),
        f.store.pinned_sha(job).await.unwrap().as_ref(),
        "a cold tree is created at the commit the harvest judges against"
    );
    assert!(
        prompt.contains(&format!(
            "checked out at main's HEAD ({base}), which does NOT contain this PR's changes"
        )),
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

    run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
        let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap();
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

    let picked = pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
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

    run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    let f = fixture("harvest-merged-no-diff", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::noop();

    run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
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
    let cold = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending,
    )
    .await
    .unwrap();
    assert_eq!(cold.state, Some(JobState::Suspended), "{cold:?}");
    assert_eq!(diff_fetches(&bins), 1, "the cold review carries the diff");

    let plan = match pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended harvest to be resumed, got {other:?}"),
    };
    let backend = classifying(close_reason("superseded"));
    run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &backend,
    )
    .await
    .unwrap();

    let runs = backend.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].prompt,
        "Continue the work you were doing in this session. You were interrupted; \
         pick up where you left off."
    );
    assert_eq!(runs[0].resume_from.as_deref(), Some(plan.session_file()));
    assert_eq!(diff_fetches(&bins), 1, "{:?}", bins.calls_to("gh"));
}

/// A resumed harvest judges against the commit its chain's first attempt
/// recorded, not `origin/main` after the worker: the admin lane fetches the
/// clone while a worker runs, so the ref may by then name a commit nobody
/// judged, and a verdict anchored there hides the changes in between from
/// suppression. The resume's own row carries the chain's base.
#[tokio::test]
async fn a_resumed_harvest_keeps_its_chains_base_while_origin_moves() {
    let bins = FakeBins::acquire("harvest-resume-base");
    gh_default(&bins);
    let f = fixture("harvest-resume-base", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let cold = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending(),
    )
    .await
    .unwrap();
    assert_eq!(cold.state, Some(JobState::Suspended), "{cold:?}");
    let base = f
        .store
        .base_sha(cold.job_id.unwrap())
        .await
        .unwrap()
        .expect("a cold harvest records its base");

    let plan = match pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended harvest to be resumed, got {other:?}"),
    };
    let wrong = close_reason("wrong");
    let backend = ScriptedBackend::new(move |tree| {
        // A fetch landing while the worker runs: `origin/main` moves to a
        // commit on top of the one the chain judges.
        let later = support::git(
            tree,
            &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "later"],
        );
        support::git(
            tree,
            &["update-ref", "refs/remotes/origin/main", later.trim()],
        );
        std::fs::write(tree.join("CLOSE-REASON.json"), &wrong).unwrap();
        support::done()
    });
    let resumed = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(resumed.outcome.as_deref(), Some("harvested"), "{resumed:?}");
    assert_eq!(
        f.store.base_sha(resumed.job_id.unwrap()).await.unwrap(),
        Some(base.clone()),
        "the resume's row carries the chain's base"
    );
    let anchors = f
        .store
        .suppression_anchors(finding.repo_id, finding.kind.as_str())
        .await
        .unwrap();
    let anchor = anchors.get(&f.fid).expect("a rejected verdict is anchored");
    assert_eq!(
        anchor.sha, base,
        "anchored at the chain's base, not the moved ref"
    );
}

/// Point the fixture's repo at a forge on `host` and turn the network gate
/// on, so failures are re-probed (#95 layer 2). Whether `host` answers is
/// up to the test backend (`ScriptedBackend::offline`).
async fn served_at(f: &mut Fixture, host: &str) {
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE repos SET url = ?1")
        .bind(format!("https://{host}/acme/widget"))
        .execute(&pool)
        .await
        .unwrap();
    f.cfg.network_gate = true;
}

/// A PR view that fails while the forge cannot be reached is the
/// machine's failure, not the harvest's: nothing is counted, and the cycle
/// ends denied for the host, to be retried, as the network gate's own
/// denial does (#95). The same failure with the forge reachable is the
/// harvest's, and counts.
#[tokio::test]
async fn a_failed_pr_view_counts_only_while_the_forge_is_reachable() {
    let bins = FakeBins::acquire("harvest-view-offline");
    bins.script("gh", "echo 'connection reset' >&2\nexit 1");
    let host = support::FORGE_HOST;
    for counted in [false, true] {
        let mut f = fixture("harvest-view-offline", FindingStatus::Closed).await;
        served_at(&mut f, host).await;
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let down: &[&str] = if counted { &[] } else { &[host] };
        let never = ScriptedBackend::new(|_| panic!("no worker without the PR")).offline(down);

        let result = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never,
        )
        .await;

        if counted {
            assert!(result.is_err(), "{result:?}");
            assert_eq!(harvest_streak(&f).await, 1, "the forge answered: counted");
            continue;
        }
        let summary = result.unwrap();
        assert_eq!(
            (
                summary.denied.as_deref(),
                summary.unreachable.as_deref(),
                summary.finding_id
            ),
            (
                Some(format!("offline: {host} unreachable").as_str()),
                Some(host),
                Some(f.fid)
            )
        );
        assert!(summary.retry_at.is_some(), "{summary:?}");
        assert_eq!(
            harvest_streak(&f).await,
            0,
            "the network's failure counts nothing"
        );
    }
}

/// A resumed harvest whose worker fails without doing any metered work
/// while its provider cannot be reached never got to the work: nothing is
/// counted, its job is closed as unstarted, and the paused attempt it was
/// continuing stays the checkpoint, where it used to be counted as a
/// failed harvest (#95).
#[tokio::test]
async fn a_worker_that_never_reached_its_provider_keeps_the_checkpoint() {
    let bins = FakeBins::acquire("harvest-worker-offline");
    gh_default(&bins);
    let mut f = fixture("harvest-worker-offline", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let paused = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending(),
    )
    .await
    .unwrap();
    let provider = support::PROVIDER_HOST;
    served_at(&mut f, support::FORGE_HOST).await;
    let plan = match pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("the paused harvest resumes, got {other:?}"),
    };
    let unmetered = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        tokens_new: 0,
        session_file: None,
        ..support::done()
    })
    .needing(&[provider])
    .offline(&[provider]);

    // The view fails here before the resume skips it (step 6), the worker
    // after; either way it is the provider that is down.
    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &unmetered,
    )
    .await
    .unwrap();

    assert_eq!(summary.unreachable.as_deref(), Some(provider));
    assert_eq!(harvest_streak(&f).await, 0);
    let checkpoint = f
        .store
        .checkpoint_job(f.fid, FindingJobKind::Harvest)
        .await
        .unwrap()
        .map(|j| j.id);
    assert_eq!(
        checkpoint, paused.job_id,
        "the paused attempt is still the checkpoint"
    );
    assert!(
        f.store
            .jobs_by_finding(f.fid)
            .await
            .unwrap()
            .iter()
            .all(|j| j.state != JobState::Running),
        "no job is left running"
    );
}

/// A paused harvest is continued without asking the forge for the PR: the
/// view only feeds the playbook, which a resume does not send. A laptop
/// that wakes before its network is back used to fail that view, and the
/// failure dropped the checkpoint and with it the partial review.
#[tokio::test]
async fn a_paused_harvest_continues_without_the_forge() {
    let bins = FakeBins::acquire("harvest-resume-offline");
    gh_default(&bins);
    let f = fixture("harvest-resume-offline", FindingStatus::Closed).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let paused = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &suspending(),
    )
    .await
    .unwrap();
    assert_eq!(paused.outcome.as_deref(), Some("suspended"));
    bins.script("gh", "echo 'dial tcp: no route to host' >&2\nexit 1");

    let resumed = resume_harvest(&f, &suspending()).await;

    assert_eq!(resumed.outcome.as_deref(), Some("suspended"), "{resumed:?}");
    let checkpoint = f
        .store
        .checkpoint_job(f.fid, FindingJobKind::Harvest)
        .await
        .unwrap()
        .map(|j| j.id);
    assert_eq!(checkpoint, resumed.job_id);
    assert_eq!(harvest_streak(&f).await, 0);
}

/// A held harvest's follow-ups are filed when the operator ends it without
/// another review (`classify` on a closed PR, `mark_harvested` on a merged
/// one): no later attempt will file them, and the tree they are in is
/// released with the checkpoint. Before, they were lost with the tree.
/// `continue` and `restart` file nothing: the review they run files its
/// own, and filing the held attempt's too would file the same open work
/// twice.
#[tokio::test]
async fn a_held_harvest_ended_by_the_operator_files_its_follow_ups() {
    let bins = FakeBins::acquire("harvest-held-released");
    gh_default(&bins);
    for (action, filed_after) in [
        (ReleaseAction::Classify, 1),
        (ReleaseAction::Continue, 0),
        (ReleaseAction::Restart, 0),
    ] {
        let f = fixture(&format!("harvest-held-{action}"), FindingStatus::Closed).await;
        let unclassifiable = ScriptedBackend::new(|tree| {
            std::fs::write(tree.join("CLOSE-REASON.json"), close_reason("maybe")).unwrap();
            std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
            support::done_with_transcript(tree)
        });
        for _ in 0..3 {
            let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
            run_harvest(
                &support::scope(&f.store),
                f.store.test_lane(),
                &f.cfg,
                Attempt::Cold(&finding),
                &unclassifiable,
            )
            .await
            .unwrap();
        }
        assert!(
            support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
                .await
                .is_some_and(|h| h.checkpoint_job_id.is_some())
        );
        assert_eq!(filed(&f, "widget:src/lib.rs:left-open").await, 0);

        let by = support::operator(&f.store).await;
        let classify = action == ReleaseAction::Classify;
        hunter::scheduler::release_hold(
            &f.store,
            &f.cfg,
            f.fid,
            FindingJobKind::Harvest,
            &hunter::store::HoldRelease {
                action,
                reason: classify.then_some("superseded by #9"),
                classification: classify.then_some(hunter::domain::ClosureClass::Superseded),
                by: &by,
            },
        )
        .await
        .unwrap();

        assert_eq!(
            filed(&f, "widget:src/lib.rs:left-open").await,
            filed_after,
            "{action}"
        );
    }
}

/// A default-branch fetch that fails while the forge cannot be reached is
/// the network's failure: nothing is counted, and the cycle ends denied
/// for the host (#95). Counted, an hour offline held every pending
/// harvest in turn.
#[tokio::test]
async fn a_failing_fetch_while_the_forge_is_unreachable_counts_nothing() {
    let bins = FakeBins::acquire("harvest-fetch-offline");
    gh_default(&bins);
    let mut f = fixture("harvest-fetch-offline", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let repo = f.store.list_repos().await.unwrap().remove(0);
    std::fs::remove_dir_all(&repo.path).unwrap();
    let down = support::FORGE_HOST;
    served_at(&mut f, down).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never_runs =
        ScriptedBackend::new(|_| panic!("no worker without a fetched clone")).offline(&[down]);

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &never_runs,
    )
    .await
    .unwrap();

    assert_eq!(summary.unreachable.as_deref(), Some(down));
    assert_eq!(harvest_streak(&f).await, 0);
}

/// A merged PR's held harvest files its follow-ups when the operator marks
/// it harvested, as a closed one's does on `classify`: no review follows
/// that would file them.
#[tokio::test]
async fn a_held_merged_harvest_marked_harvested_files_its_follow_ups() {
    let bins = FakeBins::acquire("harvest-held-merged");
    gh_default(&bins);
    let f = fixture("harvest-held-merged", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    // Killed after metered work, with a transcript: a counted failure whose
    // attempt can be continued, so the hold keeps it as the checkpoint.
    let killed = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        RunResult {
            exit_code: None,
            killed_reason: Some("unmetered".to_owned()),
            ..support::done_with_transcript(tree)
        }
    });
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &killed,
        )
        .await
        .unwrap();
    }
    assert!(
        support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
            .await
            .is_some_and(|h| h.checkpoint_job_id.is_some())
    );
    assert_eq!(filed(&f, "widget:src/lib.rs:left-open").await, 0);

    let by = support::operator(&f.store).await;
    hunter::scheduler::release_hold(
        &f.store,
        &f.cfg,
        f.fid,
        FindingJobKind::Harvest,
        &hunter::store::HoldRelease {
            action: ReleaseAction::MarkHarvested,
            reason: None,
            classification: None,
            by: &by,
        },
    )
    .await
    .unwrap();

    assert_eq!(filed(&f, "widget:src/lib.rs:left-open").await, 1);
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some());
}

/// A harvest whose backend cannot run its worker is a failed harvest
/// (WORK-MODEL.md §7): counted under the fixed `runner error`, and held at
/// the limit, the PR left unharvested.
#[tokio::test]
async fn a_harvest_whose_backend_keeps_failing_is_held() {
    let bins = FakeBins::acquire("harvest-backend-fails");
    gh_default(&bins);
    let f = fixture("harvest-backend-fails", FindingStatus::Closed).await;
    let failing = ScriptedBackend::failing();
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = run_harvest(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &failing,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("backend failed"), "{err}");
    }
    assert_held(&f, ReleaseAction::Classify).await;
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
}

/// A closed PR's harvest that cannot classify the closure without a human
/// writes `BLOCKED.md` (WORK-MODEL.md §9.1): the harvest is held as a
/// missing prerequisite on the report, the PR stays unharvested and the
/// finding `closed`, and no follow-ups are filed. A hold concludes
/// nothing, so a `once` override is kept for the continue, as for every
/// kind.
#[tokio::test]
async fn a_harvest_that_cannot_classify_is_held_on_its_report() {
    let bins = FakeBins::acquire("harvest-blocked");
    gh_default(&bins);
    let f = fixture("harvest-blocked", FindingStatus::Closed).await;
    f.store
        .set_budget_override(f.fid, Some(BudgetOverride::Once))
        .await
        .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "closed without a word").unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        support::done_with_transcript(tree)
    });

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        (
            summary.kind,
            summary.finding_id,
            summary.state,
            summary.pr_number,
            summary.outcome.as_deref()
        ),
        (
            Some(FindingJobKind::Harvest.into()),
            Some(f.fid),
            Some(JobState::Done),
            Some(7),
            Some("held")
        )
    );
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
        .await
        .expect("held on the report");
    assert_eq!(
        (hold.reason, hold.detail.as_str(), hold.checkpoint_job_id),
        (
            hunter::domain::HoldReason::Prerequisite,
            "closed without a word",
            summary.job_id
        )
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(after.budget_override, Some(BudgetOverride::Once));
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert_eq!(
        filed(&f, "widget:src/lib.rs:left-open").await,
        0,
        "nothing filed"
    );
}

/// A `BLOCKED.md` holds the harvest whatever state its attempt ended in: a
/// merged PR's review that asked for a human and then lost its connection
/// is held, not resumed with the report deleted as stale and the PR
/// stamped harvested without the review it asked a human about.
#[tokio::test]
async fn a_suspended_harvest_that_wrote_blocked_is_held() {
    let bins = FakeBins::acquire("harvest-blocked-suspended");
    gh_default(&bins);
    let f = fixture("harvest-blocked-suspended", FindingStatus::Merged).await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "the diff will not load").unwrap();
        suspended_at(tree)
    });

    let summary = run_harvest(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Harvest)
        .await
        .expect("held on the report");
    assert_eq!(
        (hold.reason, hold.detail.as_str(), hold.checkpoint_job_id),
        (
            hunter::domain::HoldReason::Prerequisite,
            "the diff will not load",
            summary.job_id
        )
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None, "not stamped harvested");
}

/// `gh pr view` calls made for PR `number`.
fn views_of(bins: &FakeBins, number: &str) -> usize {
    bins.calls_to("gh")
        .iter()
        .filter(|c| {
            c.get(1).map(String::as_str) == Some("pr")
                && c.get(2).map(String::as_str) == Some("view")
                && c.iter().any(|a| a == number)
        })
        .count()
}

/// A `pr_state` row that lost its number is repaired from the finding's
/// PR URL, and the harvest reviews the PR that number names.
#[tokio::test]
async fn a_missing_pr_number_is_recovered_from_the_pr_url() {
    let bins = FakeBins::acquire("harvest-self-heal");
    gh_default(&bins);
    let f = fixture("harvest-self-heal", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    f.store
        .set_finding_pr_open(f.fid, "https://github.com/acme/widget/pull/42")
        .await
        .unwrap();
    f.store
        .set_finding_status(f.fid, FindingStatus::Merged)
        .await
        .unwrap();
    f.exec("UPDATE pr_state SET pr_number = NULL").await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::noop();

    let summary = run_harvest(&f.store, &f.cfg, &finding, &backend, None)
        .await
        .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("harvested"), "{summary:?}");
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.pr_number, Some(42), "the row is repaired from the URL");
    assert!(ps.harvested_at.is_some(), "the recovered PR is harvested");
    assert_eq!(views_of(&bins, "42"), 1, "{:?}", bins.calls_to("gh"));
    assert_eq!(backend.runs().len(), 1);
}

/// With no number and no URL to recover one from, the harvest refuses
/// before any forge call, job or worker, and says why in the event log.
#[tokio::test]
async fn a_missing_pr_number_without_a_parseable_url_bails() {
    let bins = FakeBins::acquire("harvest-no-pr-number");
    gh_default(&bins);
    let f = fixture("harvest-no-pr-number", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    f.exec("UPDATE pr_state SET pr_number = NULL").await;
    f.exec("UPDATE findings SET pr_url = 'not a pr url'").await;
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let never_runs = ScriptedBackend::new(|_| panic!("no worker without a PR number"));

    let err = run_harvest(&f.store, &f.cfg, &finding, &never_runs, None)
        .await
        .expect_err("a harvest with no PR number must refuse");

    assert_eq!(err.to_string(), "no pr_number");
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("no parseable pr_url")),
        "{events:?}"
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM jobs").await, Some(0));
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!((ps.pr_number, ps.harvested_at), (None, None));
}

/// A harvest the budget denies ends before the forge and the tree: no
/// job, no view, no worktree, nothing counted against it, and the PR
/// stays pending for a window with room.
#[tokio::test]
async fn a_denied_harvest_leaves_the_pr_pending() {
    let bins = FakeBins::acquire("harvest-denied");
    gh_default(&bins);
    let f = fixture("harvest-denied", FindingStatus::Merged).await;
    f.store.mark_pr_merged(f.fid, 7, 2).await.unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let denying = ScriptedBackend::new(|_| panic!("no worker once denied")).denying_above(0);

    let summary = run_harvest(&f.store, &f.cfg, &finding, &denying, None)
        .await
        .unwrap();

    assert!(summary.denied.is_some(), "{summary:?}");
    assert_eq!(f.scalar("SELECT COUNT(*) FROM jobs").await, Some(0));
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    let trees: Vec<_> = std::fs::read_dir(&f.cfg.work_root)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    assert!(trees.is_empty(), "no worktree may be left: {trees:?}");
    assert_eq!(harvest_streak(&f).await, 0);
    let pending: Vec<i64> = f
        .store
        .list_pending_harvest()
        .await
        .unwrap()
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(pending, [f.fid]);
}
