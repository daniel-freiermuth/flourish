#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_recheck` — how a worker's reply moves a `rechecking` finding.
//!
//! The recheck is the only consumer of the operator's `POST /api/recheck`,
//! and its failure path is the subtle part: a worker that produced no
//! usable verdict leaves the finding `rechecking` (so the scheduler picks
//! it again) until the same failure repeats `MAX_CONSECUTIVE_SAME_FAILURE`
//! times, when the recheck is held for a human. Reading the wrong file
//! — one left by a crashed attempt, or one a killed worker half-wrote —
//! would silently reject a real bug, so those guards are pinned here too.
//!
//! The verdict-specific cases that already have a home stay there:
//! `scheduler_resume_test` covers suspensions, `updated_severity`
//! parsing, and which of stale/invalid reaches the suppression list.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hunter::config::Config;
use hunter::domain::{BudgetOverride, FindingJobKind, FindingStatus};
use hunter::scheduler::{CycleSummary, run_recheck};
use hunter::store::{FindingInsert, Store, WorkStreak};
use hunter::types::{Finding, RunResult};
use sqlx::SqlitePool;
use support::{GitRepo, ScriptedBackend, TempDir};

struct Fixture {
    cfg: Config,
    store: Store,
    pool: SqlitePool,
    fid: i64,
    /// Last, so the directory outlives the Store's SQLite pool. See
    /// `runner_engage_test` for why.
    _dir: TempDir,
}

impl Fixture {
    /// Where `run_recheck` tells the worker to write its verdict.
    fn out(&self) -> PathBuf {
        self.cfg
            .work_root
            .join("out")
            .join(format!("recheck{}.json", self.fid))
    }

    async fn finding(&self) -> Finding {
        self.store.get_finding(self.fid).await.unwrap().unwrap()
    }

    async fn recheck(&self, backend: &ScriptedBackend) -> CycleSummary {
        let finding = self.finding().await;
        run_recheck(
            &support::scope(&self.store),
            &self.cfg,
            &finding,
            backend,
            None,
        )
        .await
        .unwrap()
    }

    async fn streak(&self) -> WorkStreak {
        self.store
            .work_streak(self.fid, FindingJobKind::Recheck)
            .await
            .unwrap()
    }

    async fn job_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
}

/// A real clone registered as repo 1 with one medium bug finding in
/// `status`, and a stub recheck playbook that only has to render.
async fn fixture(label: &str, status: FindingStatus) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    let (db, pool) = support::fresh_pool(&dir, "recheck").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(repo.work.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    let store = Store::connect(&db).await.unwrap();
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "alpha:README.md:seed:1".to_owned(),
                file: "README.md".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.5,
                summary: "a real bug".to_owned(),
                detail: Some("README.md:1 is wrong".to_owned()),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_status(fid, status).await.unwrap();

    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("recheck.md"),
        "recheck {{REPO_PATH}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        pool,
        fid,
        _dir: dir,
    }
}

/// A worker that writes `body` to `out` (when given) and then ends as `rr`.
fn worker(out: PathBuf, body: Option<String>, rr: RunResult) -> ScriptedBackend {
    ScriptedBackend::new(move |_| {
        if let Some(body) = &body {
            std::fs::write(&out, body).unwrap();
        }
        rr.clone()
    })
}

/// A worker that exits 0 after writing `verdict` as its verdict file.
fn replying(out: PathBuf, verdict: &serde_json::Value) -> ScriptedBackend {
    worker(out, Some(verdict.to_string()), support::done())
}

/// A worker that exited non-zero on its own, having done no work.
fn exited_failed() -> RunResult {
    RunResult {
        exit_code: Some(1),
        ..support::done()
    }
}

/// A worker the harness killed for a reason that is never resumed.
fn killed() -> RunResult {
    RunResult {
        exit_code: None,
        killed_reason: Some("wallclock".to_owned()),
        ..support::done()
    }
}

/// Every way a worker can end without a usable verdict is a retry, not a
/// verdict: the finding stays `rechecking` for the scheduler to pick again,
/// and the failure is recorded under its own name so the streak can tell
/// a repeat from a new problem.
#[tokio::test]
async fn each_failure_is_a_retry_recorded_under_its_own_name() {
    let cases: [(&str, Option<&str>, RunResult, &str); 6] = [
        ("failed", None, exited_failed(), "worker failed"),
        ("killed", None, killed(), "worker killed"),
        ("no-file", None, support::done(), "no verdict file"),
        (
            "garbage",
            Some("{not json"),
            support::done(),
            "unparseable verdict file",
        ),
        (
            "no-key",
            Some(r#"{"reason":"forgot the verdict"}"#),
            support::done(),
            "missing verdict field",
        ),
        (
            "unknown",
            Some(r#"{"verdict":"maybe","reason":"unsure"}"#),
            support::done(),
            "invalid verdict value: \"maybe\"",
        ),
    ];
    for (label, body, rr, failure) in cases {
        let f = fixture(&format!("recheck-fail-{label}"), FindingStatus::Rechecking).await;
        let backend = worker(f.out(), body.map(ToOwned::to_owned), rr);

        let summary = f.recheck(&backend).await;

        assert_eq!(
            summary.outcome.as_deref(),
            Some("requeued"),
            "{label}: {summary:?}"
        );
        assert_eq!(summary.verdict, None, "{label}");
        let after = f.finding().await;
        assert_eq!(after.status, FindingStatus::Rechecking, "{label}");
        assert_eq!(f.streak().await.streak, 1, "{label}");
        assert_eq!(
            f.streak().await.last_failure.as_deref(),
            Some(failure),
            "{label}"
        );
        assert_eq!(after.verdict_reason, None, "{label}");
    }
}

/// The third identical failure in a row holds the recheck for a human as
/// `stuck`, with what kept failing. The finding stays `rechecking`: sent
/// back to `new` (as it was until the work model), it looked exactly like
/// a fresh finding nobody had rechecked. The streak ends with the hold.
#[tokio::test]
async fn the_third_identical_failure_holds_the_recheck() {
    let f = fixture("recheck-stuck", FindingStatus::Rechecking).await;
    let silent = ScriptedBackend::noop();

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        outcomes.push(f.recheck(&silent).await.outcome);
        if outcomes.len() < 3 {
            assert_eq!(f.finding().await.status, FindingStatus::Rechecking);
        }
    }

    assert_eq!(
        outcomes,
        [
            Some("requeued".into()),
            Some("requeued".into()),
            Some("held".into())
        ]
    );
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::Rechecking);
    assert_eq!(f.streak().await, WorkStreak::default());
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Recheck)
        .await
        .expect("held at the limit");
    assert_eq!(hold.reason, hunter::domain::HoldReason::Stuck);
    assert_eq!(hold.detail, "no verdict file");
    assert_eq!(
        hold.actions(),
        [
            hunter::domain::ReleaseAction::Restart,
            hunter::domain::ReleaseAction::Reject,
            hunter::domain::ReleaseAction::Wontfix
        ],
        "no transcript to continue"
    );
    assert!(
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
            .is_none_or(|c| c.job_kind() != FindingJobKind::Recheck.into()),
        "the recheck tier skips held work"
    );
}

/// A held recheck released with `restart` is the recheck tier's again,
/// started fresh with a full streak; `reject` ends it with the verdict.
#[tokio::test]
async fn a_held_recheck_is_released_by_the_operator() {
    use hunter::domain::ReleaseAction;
    let f = fixture("recheck-release", FindingStatus::Rechecking).await;
    let silent = ScriptedBackend::noop();
    for _ in 0..3 {
        f.recheck(&silent).await;
    }
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Recheck,
        ReleaseAction::Restart,
    )
    .await;
    let picked = hunter::scheduler::pick_next(&f.store, &f.cfg, None)
        .await
        .unwrap();
    assert!(
        matches!(
            picked,
            Some(hunter::scheduler::Candidate::Finding {
                kind: FindingJobKind::Recheck,
                ..
            })
        ),
        "{picked:?}"
    );
    for _ in 0..3 {
        f.recheck(&silent).await;
    }
    let by = support::operator(&f.store).await;
    f.store
        .release_hold(
            f.fid,
            FindingJobKind::Recheck,
            &hunter::store::HoldRelease {
                action: ReleaseAction::Reject,
                reason: Some("cannot be verified"),
                classification: None,
                by: &by,
            },
        )
        .await
        .unwrap();
    assert_eq!(f.finding().await.status, FindingStatus::Rejected);
}

/// Only IDENTICAL failures make a streak: two kinds alternating never
/// give up, because each one is a different problem seen once.
#[tokio::test]
async fn alternating_failures_never_accumulate_a_streak() {
    let f = fixture("recheck-alternating", FindingStatus::Rechecking).await;
    let silent = ScriptedBackend::noop();
    let garbled = worker(f.out(), Some("{not json".to_owned()), support::done());

    for _ in 0..3 {
        assert_eq!(
            f.recheck(&silent).await.outcome.as_deref(),
            Some("requeued")
        );
        assert_eq!(
            f.recheck(&garbled).await.outcome.as_deref(),
            Some("requeued")
        );
    }

    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::Rechecking);
    assert_eq!(f.streak().await.streak, 1);
    assert_eq!(
        f.streak().await.last_failure.as_deref(),
        Some("unparseable verdict file")
    );
}

/// A verdict file left behind by a crashed attempt is deleted before the
/// worker runs: a worker that then writes nothing is "no verdict file",
/// never the old `invalid` read as this attempt's answer.
#[tokio::test]
async fn a_verdict_left_by_an_earlier_attempt_is_never_read() {
    let f = fixture("recheck-stale-output", FindingStatus::Rechecking).await;
    std::fs::create_dir_all(f.out().parent().unwrap()).unwrap();
    std::fs::write(
        f.out(),
        r#"{"verdict":"invalid","reason":"from a crashed attempt"}"#,
    )
    .unwrap();
    let existed_at_start = Arc::new(AtomicBool::new(true));
    let existed = existed_at_start.clone();
    let out = f.out();
    let backend = ScriptedBackend::new(move |_| {
        existed.store(out.exists(), Ordering::SeqCst);
        support::done()
    });

    let summary = f.recheck(&backend).await;

    assert_eq!(backend.runs().len(), 1);
    assert!(
        !existed_at_start.load(Ordering::SeqCst),
        "deleted before the run"
    );
    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::Rechecking);
    assert_eq!(
        f.streak().await.last_failure.as_deref(),
        Some("no verdict file")
    );
}

/// A killed worker's verdict file is not read even when it is complete
/// and valid: only a worker that finished is trusted to have finished
/// writing it.
#[tokio::test]
async fn a_killed_workers_verdict_is_ignored() {
    let f = fixture("recheck-killed-verdict", FindingStatus::Rechecking).await;
    let body = serde_json::json!({ "verdict": "invalid", "reason": "looked wrong" });
    let backend = worker(f.out(), Some(body.to_string()), killed());

    let summary = f.recheck(&backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("requeued"), "{summary:?}");
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::Rechecking);
    assert_eq!(
        f.streak().await.last_failure.as_deref(),
        Some("worker killed")
    );
    assert_eq!(after.verdict_reason, None);
}

/// A confirmation rewrites the analysis with what the worker found, sends
/// the finding back to the inbox, and forgets the failure streak earlier
/// attempts built up.
#[tokio::test]
async fn a_confirmation_rewrites_the_analysis_and_clears_the_streak() {
    let f = fixture("recheck-confirmed", FindingStatus::Rechecking).await;
    f.recheck(&ScriptedBackend::noop()).await;
    assert_eq!(f.streak().await.streak, 1);

    let backend = replying(
        f.out(),
        &serde_json::json!({
            "verdict": "confirmed",
            "reason": "still reachable",
            "updated_summary": "a sharper summary",
            "updated_detail": "README.md:2 is where it happens",
            "updated_confidence": 0.95,
        }),
    );
    let summary = f.recheck(&backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("confirmed"), "{summary:?}");
    assert_eq!(summary.verdict.as_deref(), Some("confirmed"));
    assert_eq!(summary.reason.as_deref(), Some("still reachable"));
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::New);
    assert_eq!(after.summary, "a sharper summary");
    assert_eq!(
        after.detail.as_deref(),
        Some("README.md:2 is where it happens")
    );
    assert!(
        (after.confidence - 0.95).abs() < 1e-9,
        "{}",
        after.confidence
    );
    assert_eq!(after.severity, hunter::domain::Severity::Medium);
    assert_eq!(f.streak().await.streak, 0);
    assert_eq!(f.streak().await.last_failure, None);
}

/// A confirmation that names no updates keeps the analysis the finding had.
#[tokio::test]
async fn a_bare_confirmation_keeps_the_existing_analysis() {
    let f = fixture("recheck-confirmed-bare", FindingStatus::Rechecking).await;
    let backend = replying(f.out(), &serde_json::json!({ "verdict": "confirmed" }));

    let summary = f.recheck(&backend).await;

    assert_eq!(summary.outcome.as_deref(), Some("confirmed"), "{summary:?}");
    assert_eq!(summary.reason.as_deref(), Some(""));
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::New);
    assert_eq!(after.summary, "a real bug");
    assert_eq!(after.detail.as_deref(), Some("README.md:1 is wrong"));
    assert!(
        (after.confidence - 0.5).abs() < 1e-9,
        "{}",
        after.confidence
    );
}

/// The stored reason is capped at 500 characters — characters, not
/// bytes, so a multibyte reason is cut on a boundary instead of panicking
/// or storing half a code point.
#[tokio::test]
async fn a_long_multibyte_reason_is_capped_at_500_characters() {
    let f = fixture("recheck-long-reason", FindingStatus::Rechecking).await;
    let reason = "é".repeat(600);
    let backend = replying(
        f.out(),
        &serde_json::json!({ "verdict": "invalid", "reason": reason }),
    );

    let summary = f.recheck(&backend).await;

    let capped = "é".repeat(500);
    assert_eq!(summary.reason.as_deref(), Some(capped.as_str()));
    let after = f.finding().await;
    assert_eq!(after.status, FindingStatus::Rejected);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some(format!("recheck: {capped}").as_str())
    );
}

/// A finding that left `rechecking` between selection and the run (the
/// operator triaged it) is skipped without starting a job.
#[tokio::test]
async fn a_finding_no_longer_rechecking_is_skipped_without_a_job() {
    let f = fixture("recheck-precondition", FindingStatus::New).await;
    let backend = ScriptedBackend::noop();

    let summary = f.recheck(&backend).await;

    assert_eq!(
        summary.skipped.as_deref(),
        Some(format!("finding #{} is new, not rechecking", f.fid).as_str())
    );
    assert!(backend.runs().is_empty());
    assert_eq!(f.job_count().await, 0);
    assert_eq!(f.finding().await.status, FindingStatus::New);
}

/// A one-shot budget override is spent by the attempt it bought, whether
/// that attempt produced a verdict or failed; a standing exemption is not.
#[tokio::test]
async fn a_once_override_is_spent_by_any_finished_attempt() {
    let f = fixture("recheck-override", FindingStatus::Rechecking).await;
    let set = |mode| f.store.set_budget_override(f.fid, Some(mode));

    set(BudgetOverride::Once).await.unwrap();
    f.recheck(&ScriptedBackend::noop()).await;
    assert_eq!(f.finding().await.budget_override, None, "after a failure");

    set(BudgetOverride::Exempt).await.unwrap();
    f.recheck(&ScriptedBackend::noop()).await;
    assert_eq!(
        f.finding().await.budget_override,
        Some(BudgetOverride::Exempt),
        "an exemption outlives a failure"
    );

    set(BudgetOverride::Once).await.unwrap();
    let backend = replying(f.out(), &serde_json::json!({ "verdict": "confirmed" }));
    assert_eq!(
        f.recheck(&backend).await.outcome.as_deref(),
        Some("confirmed")
    );
    assert_eq!(f.finding().await.budget_override, None, "after a verdict");
}

/// A recheck held for repeated failures keeps the tree of the attempt it
/// holds, so the operator's `continue` resumes it. The tree used to be
/// released before the failure was counted: the hold offered `continue`,
/// and `continue` found no tree and silently started a fresh recheck.
#[tokio::test]
async fn a_stuck_recheck_keeps_its_tree_for_the_continue() {
    let f = fixture("recheck-stuck-tree", FindingStatus::Rechecking).await;
    // Finished, with a transcript, but no verdict file.
    let silent = ScriptedBackend::new(support::done_with_transcript);
    for _ in 0..3 {
        f.recheck(&silent).await;
    }
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Recheck)
        .await
        .expect("held at the limit");
    assert!(hold.checkpoint_job_id.is_some(), "{hold:?}");
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Recheck,
        hunter::domain::ReleaseAction::Continue,
    )
    .await;

    let picked = hunter::scheduler::pick_next(&f.store, &f.cfg, None)
        .await
        .unwrap();

    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) = picked else {
        panic!("continue resumes the held attempt, got {picked:?}");
    };
    assert_eq!(Some(plan.predecessor_id), hold.checkpoint_job_id);
    assert!(plan.workspace.tree.is_dir());
}

/// A recheck worker that fails without doing any metered work while its
/// provider cannot be reached never got to the finding: nothing is
/// counted and the cycle ends denied for the host (#95). One that did
/// metered work failed at the recheck, offline or not, and counts.
#[tokio::test]
async fn an_unmetered_failure_counts_only_while_the_provider_is_reachable() {
    let provider = support::PROVIDER_HOST;
    for (tokens_new, counted) in [(0, false), (5_000, true)] {
        let mut f = fixture("recheck-worker-offline", FindingStatus::Rechecking).await;
        f.cfg.network_gate = true;
        let failing = ScriptedBackend::new(move |_| RunResult {
            exit_code: Some(1),
            tokens_new,
            ..support::done()
        })
        .needing(&[provider])
        .offline(&[provider]);

        let summary = f.recheck(&failing).await;

        let streak = f
            .store
            .work_streak(f.fid, FindingJobKind::Recheck)
            .await
            .unwrap()
            .streak;
        assert_eq!(
            streak,
            i64::from(counted),
            "{tokens_new} tokens: {summary:?}"
        );
        assert_eq!(
            summary.unreachable.as_deref(),
            (!counted).then_some(provider),
            "{summary:?}"
        );
        assert_eq!(f.finding().await.status, FindingStatus::Rechecking);
    }
}

/// A recheck whose backend cannot run its worker is a failed recheck
/// (WORK-MODEL.md §7): counted under the fixed `runner error`, its job
/// closed instead of left `running`, and held at the limit. Uncounted, the
/// recheck tier took it again every cycle.
#[tokio::test]
async fn a_recheck_whose_backend_keeps_failing_is_held() {
    let f = fixture("recheck-backend-fails", FindingStatus::Rechecking).await;
    let failing = ScriptedBackend::failing();
    for _ in 0..3 {
        let finding = f.finding().await;
        let err = run_recheck(&support::scope(&f.store), &f.cfg, &finding, &failing, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("backend failed"), "{err}");
    }
    let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Recheck)
        .await
        .expect("held at the limit");
    assert!(hold.detail.starts_with("runner error"), "{hold:?}");
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE state = 'running'")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(running, 0, "no job is left running");
}

/// A recheck that cannot reach a verdict without a human writes
/// `BLOCKED.md` (WORK-MODEL.md §9.1): the recheck is held as a missing
/// prerequisite with the report as its detail and this attempt as its
/// checkpoint, the finding stays `rechecking`, and the report leaves the
/// tree, so a continue cannot read it as its own. A final verdict written
/// next to it wins.
#[tokio::test]
async fn a_recheck_that_needs_a_human_is_held_on_its_report() {
    for verdict in [None, Some(serde_json::json!({ "verdict": "confirmed" }))] {
        let f = fixture("recheck-blocked", FindingStatus::Rechecking).await;
        f.store
            .set_budget_override(f.fid, Some(BudgetOverride::Once))
            .await
            .unwrap();
        let out = f.out();
        let body = verdict.as_ref().map(ToString::to_string);
        let backend = ScriptedBackend::new(move |tree| {
            std::fs::write(tree.join("BLOCKED.md"), "needs the staging database").unwrap();
            if let Some(body) = &body {
                std::fs::write(&out, body).unwrap();
            }
            support::done_with_transcript(tree)
        });

        let summary = f.recheck(&backend).await;

        let hold = support::hold_of(&f.store, f.fid, FindingJobKind::Recheck).await;
        if verdict.is_some() {
            assert_eq!(hold, None, "the verdict wins");
            assert_eq!(summary.outcome.as_deref(), Some("confirmed"));
            continue;
        }
        assert_eq!(summary.outcome.as_deref(), Some("held"));
        let hold = hold.expect("held on the report");
        assert_eq!(
            (hold.reason, hold.detail.as_str(), hold.checkpoint_job_id),
            (
                hunter::domain::HoldReason::Prerequisite,
                "needs the staging database",
                summary.job_id
            )
        );
        assert_eq!(f.finding().await.status, FindingStatus::Rechecking);
        assert_eq!(
            f.finding().await.budget_override,
            Some(BudgetOverride::Once),
            "a hold concluded nothing: the override is for the continue"
        );
        let tree = backend.runs()[0].tree.clone();
        assert!(
            !tree.join("BLOCKED.md").exists(),
            "the hold owns the report"
        );
    }
}

/// The operator's `continue` on a recheck held on its report resumes the
/// checkpoint with a prompt chosen by the hold's reason, the report as
/// context (WORK-MODEL.md §9.1): the worker re-evaluates the prerequisite
/// instead of being told it was merely interrupted.
#[tokio::test]
async fn a_continued_recheck_hold_resumes_with_its_reason() {
    let f = fixture("recheck-continue", FindingStatus::Rechecking).await;
    let blocked = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "needs the staging database").unwrap();
        support::done_with_transcript(tree)
    });
    f.recheck(&blocked).await;
    support::release(
        &f.store,
        f.fid,
        FindingJobKind::Recheck,
        hunter::domain::ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&f.store, &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a continued hold resumes its checkpoint");
    };
    let resumed = replying(f.out(), &serde_json::json!({ "verdict": "confirmed" }));

    let finding = f.finding().await;
    run_recheck(
        &support::scope(&f.store),
        &f.cfg,
        &finding,
        &resumed,
        Some(&plan),
    )
    .await
    .unwrap();

    let prompt = &resumed.runs()[0].prompt;
    assert!(prompt.contains("Re-evaluate the prerequisite"), "{prompt}");
    assert!(prompt.contains("needs the staging database"), "{prompt}");
}
