#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_dep_update` — what the scheduler does with Renovate's answer.
//!
//! `dep_scan::scan_repo` distinguishes "every dependency looked up is
//! current" (`Some(vec![])`) from "Renovate has no answer" (`None`: failed,
//! or looked nothing up). The runner has to keep that distinction: only
//! `None` may fall back to the model, because the fallback is a whole AI
//! run — and it must, or a repo Renovate cannot read is never analysed.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hunter::domain::{ForgeName, JobKind, JobState, RepoJobKind};
use hunter::scheduler::CycleSummary;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

/// One cycle of `run_dep_update` over a real clone, with the installed
/// Renovate replaced by one printing `renovate_output` and exiting 0.
/// Returns the summary, whether the model ran, and whether the repo's
/// dep-update cadence advanced.
async fn dep_update_cycle(label: &str, renovate_output: &str) -> (CycleSummary, bool, bool) {
    let c = cycle_with(label, renovate_output, None).await;
    (c.result.expect("dep_update cycle"), c.model_ran, c.advanced)
}

struct Cycle {
    result: anyhow::Result<CycleSummary>,
    model_ran: bool,
    advanced: bool,
    findings: i64,
}

/// A registered, cloned repo whose installed Renovate is a fake, ready for
/// one or more `run_dep_update` cycles against the same store.
struct Rig {
    bins: FakeBins,
    _dir: TempDir,
    store: hunter::store::Store,
    pool: sqlx::SqlitePool,
    cfg: hunter::config::Config,
    repo: hunter::types::Repo,
    backend: ScriptedBackend,
    model_ran: Arc<AtomicBool>,
}

impl Rig {
    async fn new(label: &str) -> Self {
        let bins = FakeBins::acquire(label);
        let dir = TempDir::new(&format!("{label}-root"));
        let origin_repo = GitRepo::with_branch(&dir, "unused");
        let origin = origin_repo.origin.to_string_lossy().into_owned();
        let (db, store) = fresh_store(&dir, "depupd").await;
        let repos_root = dir.subdir("repos");
        // GitLab-hosted, so no `gh` token lookup: the test is about the answer.
        let rid = store
            .add_repo(
                "widget",
                &origin,
                &repos_root,
                &origin_repo.default_branch,
                ForgeName::Gitlab,
            )
            .await
            .unwrap();
        let repo = store.get_repo_by_id(rid).await.unwrap().unwrap();
        // `run_dep_update` only works in an existing clone.
        git(dir.path(), &["clone", &origin, &repo.path]);
        // Hermetic stub for the fallback's prompt; the scripted worker ignores it.
        std::fs::write(
            dir.subdir("playbooks").join("dep_update.md"),
            "dep update\n",
        )
        .unwrap();
        let mut cfg = support::load_config(dir.path());
        cfg.work_root = dir.subdir("work_root");
        let model_ran = Arc::new(AtomicBool::new(false));
        let backend = ScriptedBackend::new({
            let model_ran = Arc::clone(&model_ran);
            move |_| {
                model_ran.store(true, Ordering::SeqCst);
                done()
            }
        });
        let pool =
            sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&db))
                .await
                .unwrap();
        Self {
            bins,
            _dir: dir,
            store,
            pool,
            cfg,
            repo,
            backend,
            model_ran,
        }
    }

    /// One cycle in which Renovate prints `renovate_output` and exits 0.
    async fn scan(&self, renovate_output: &str) -> anyhow::Result<CycleSummary> {
        let before = self.bins.calls_to("renovate").len();
        self.bins.ok("renovate", renovate_output);
        let result = hunter::scheduler::run_dep_update(
            &support::scope(&self.store),
            &self.cfg,
            &self.repo,
            &self.backend,
            None,
        )
        .await;
        assert_eq!(
            self.bins.calls_to("renovate").len(),
            before + 1,
            "{:?}",
            self.bins.calls()
        );
        result
    }

    /// File an open `dep_update` for `package` in repo `repo_id`.
    async fn seed_dep(&self, fingerprint: &str, package: &str, repo_id: i64) -> i64 {
        self.seed_update(fingerprint, package, "minor", repo_id)
            .await
    }

    /// File an open `dep_update` of `update_type` for `package`.
    async fn seed_update(
        &self,
        fingerprint: &str,
        package: &str,
        update_type: &str,
        repo_id: i64,
    ) -> i64 {
        self.store
            .upsert_finding(
                repo_id,
                &hunter::store::FindingInsert {
                    fingerprint: fingerprint.to_owned(),
                    file: "package.json".to_owned(),
                    severity: hunter::domain::Severity::Low,
                    confidence: 0.9,
                    summary: format!("{package} update"),
                    package: Some(package.to_owned()),
                    update_type: Some(update_type.to_owned()),
                    ..Default::default()
                },
                "dep_update",
                None,
            )
            .await
            .unwrap()
            .0
    }

    /// `(status, verdict_reason, budget_override)` of the finding.
    async fn state_of(&self, fingerprint: &str) -> (String, Option<String>, Option<String>) {
        sqlx::query_as(
            "SELECT status, verdict_reason, budget_override FROM findings \
             WHERE fingerprint = ?1",
        )
        .bind(fingerprint)
        .fetch_one(&self.pool)
        .await
        .unwrap()
    }

    async fn id_of(&self, fingerprint: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM findings WHERE fingerprint = ?1")
            .bind(fingerprint)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn set_status(&self, fingerprint: &str, status: &str, reason: Option<&str>) {
        sqlx::query("UPDATE findings SET status = ?1, verdict_reason = ?2 WHERE fingerprint = ?3")
            .bind(status)
            .bind(reason)
            .bind(fingerprint)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    async fn status_of(&self, fingerprint: &str) -> String {
        sqlx::query_scalar("SELECT status FROM findings WHERE fingerprint = ?1")
            .bind(fingerprint)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// `(fingerprint, status, latest_version)` of every finding, by id.
    async fn findings(&self) -> Vec<(String, String, Option<String>)> {
        sqlx::query_as(
            "SELECT fingerprint, status, latest_version FROM findings \
             WHERE repo_id = ?1 ORDER BY id",
        )
        .bind(self.repo.id)
        .fetch_all(&self.pool)
        .await
        .unwrap()
    }
}

/// [`dep_update_cycle`], optionally with a read-only result file an
/// earlier scan left behind (`stale`), so the new result cannot be written.
async fn cycle_with(label: &str, renovate_output: &str, stale: Option<&str>) -> Cycle {
    let rig = Rig::new(label).await;
    if let Some(stale) = stale {
        use std::os::unix::fs::PermissionsExt;
        let old = rig
            .cfg
            .work_root
            .join("out")
            .join(format!("dep_scan_{}.json", rig.repo.id));
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, stale).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o444)).unwrap();
    }

    let result = rig.scan(renovate_output).await;

    let after = rig
        .store
        .get_repo_by_id(rig.repo.id)
        .await
        .unwrap()
        .unwrap();
    Cycle {
        result,
        model_ran: rig.model_ran.load(Ordering::SeqCst),
        advanced: after.last_dep_update_at.is_some(),
        findings: i64::try_from(rig.findings().await.len()).unwrap(),
    }
}

#[tokio::test]
async fn up_to_date_repo_does_not_fall_back_to_the_model() {
    let current = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;

    let (summary, model_ran, advanced) = dep_update_cycle("depupd-current", current).await;

    assert!(
        !model_ran,
        "renovate answered 'nothing to update', yet the model ran"
    );
    assert_eq!(summary.state, Some(JobState::Done));
    assert_eq!(
        summary.ingest.as_ref().map(|i| (i.inserted, i.invalid)),
        Some((0, 0)),
        "{summary:?}"
    );
    assert!(
        advanced,
        "the cadence must advance, or the repo is rescanned every cycle"
    );
}

/// What Renovate 44.96.2 prints for a repo it finds no dependency in.
#[tokio::test]
async fn repo_renovate_cannot_read_falls_back_to_the_model() {
    let no_deps = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{}}"#;

    let (summary, model_ran, _) = dep_update_cycle("depupd-nodeps", no_deps).await;

    assert!(model_ran, "renovate had no answer, so the model must run");
    assert!(summary.job_id.is_some(), "{summary:?}");
}

/// Renovate's updates become `dep_update` findings, and the cycle summary
/// (the daemon's per-cycle log line and the manual-trigger response) says
/// which job ran for which repo and what it ingested.
#[tokio::test]
async fn updates_are_ingested_and_reported() {
    let update = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"^1.2.0","datasource":"npm","updates":[{"newVersion":"1.3.0","updateType":"minor"}]}]}]}}"#;

    let (summary, model_ran, advanced) = dep_update_cycle("depupd-update", update).await;

    assert!(!model_ran, "renovate answered, so no model run");
    assert_eq!(summary.kind, Some(JobKind::Repo(RepoJobKind::DepUpdate)));
    assert_eq!(summary.repo.as_deref(), Some("widget"));
    assert_eq!(
        summary
            .ingest
            .as_ref()
            .map(|i| (i.inserted, i.duplicates, i.invalid)),
        Some((1, 0, 0)),
        "{summary:?}"
    );
    assert!(advanced);
}

/// When the result file cannot be written, the cycle fails instead of
/// ingesting whatever an earlier scan left at that path: here a stale
/// candidate would be re-filed and the cadence advanced, for a repo whose
/// scan just said everything is current.
#[tokio::test]
async fn unwritable_result_file_fails_the_cycle() {
    let current = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"left-pad","currentValue":"1.3.0","datasource":"npm","updates":[]}]}]}}"#;
    let stale = r#"[{"fingerprint":"widget:npm:left-pad:1.2.0→left-pad 1.3.0","file":"package.json","ecosystem":"npm","package":"left-pad","current_version":"1.2.0","latest_version":"1.3.0","update_type":"minor","severity":"medium","confidence":0.85,"summary":"left-pad: minor update","detail":"stale"}]"#;

    let c = cycle_with("depupd-stale", current, Some(stale)).await;

    assert!(c.result.is_err(), "{:?}", c.result);
    assert_eq!(c.findings, 0, "the stale result was ingested");
    assert!(!c.advanced, "a failed cycle must not advance the cadence");
    assert!(!c.model_ran);
}

/// Renovate's lookup output for `left-pad` at `current`, offering `next`
/// on its non-major branch (`None`: up to date).
fn left_pad(current: &str, next: Option<&str>) -> String {
    let updates = next.map_or_else(String::new, |n| {
        format!(
            r#"{{"newVersion":"{n}","updateType":"minor","branchName":"renovate/left-pad-1.x"}}"#
        )
    });
    format!(
        r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"npm":[{{"packageFile":"package.json","deps":[{{"depName":"left-pad","currentValue":"^1.0.0","currentVersion":"{current}","datasource":"npm","updates":[{updates}]}}]}}]}}}}"#
    )
}

/// An open update is one finding however many releases pass before anyone
/// acts on it: each scan moves its target instead of filing another. It
/// retires once Renovate stops proposing it, comes back if Renovate
/// proposes it again, and a new one starts only when the dependency itself
/// moved.
#[tokio::test]
async fn an_open_update_follows_upstream_until_the_dependency_moves() {
    let rig = Rig::new("depupd-lifecycle").await;
    let fp = "widget:dep:left-pad-1.x@1.2.0";

    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "new".into(), Some("1.3.0".into()))]
    );

    let s = rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();
    assert_eq!(
        s.ingest.as_ref().map(|i| (i.inserted, i.refreshed)),
        Some((0, 1)),
        "{s:?}"
    );
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "new".into(), Some("1.4.0".into()))],
        "a newer release moves the open finding's target"
    );
    let s = rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();
    assert_eq!(
        s.ingest.as_ref().map(|i| (i.inserted, i.refreshed)),
        Some((0, 0)),
        "the same answer again changes nothing: {s:?}"
    );

    rig.scan(&left_pad("1.2.0", None)).await.unwrap();
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "superseded".into(), Some("1.4.0".into()))],
        "no longer proposed: retired"
    );

    rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "new".into(), Some("1.4.0".into()))],
        "proposed again: it never landed, so it is open again"
    );

    rig.scan(&left_pad("1.4.0", Some("1.5.0"))).await.unwrap();
    assert_eq!(
        rig.findings().await,
        vec![
            (fp.into(), "superseded".into(), Some("1.4.0".into())),
            (
                "widget:dep:left-pad-1.x@1.4.0".into(),
                "new".into(),
                Some("1.5.0".into())
            ),
        ],
        "the dependency moved: the old update is done, the next one is new"
    );
}

/// Once a worker or the operator has taken an update up, the scan neither
/// rewrites what it is working towards nor retires it.
#[tokio::test]
async fn an_update_in_flight_is_left_alone() {
    let rig = Rig::new("depupd-in-flight").await;
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    sqlx::query("UPDATE findings SET status = 'pr_open'")
        .execute(&rig.pool)
        .await
        .unwrap();

    rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();
    rig.scan(&left_pad("1.2.0", None)).await.unwrap();

    assert_eq!(
        rig.findings().await,
        vec![(
            "widget:dep:left-pad-1.x@1.2.0".into(),
            "pr_open".into(),
            Some("1.3.0".into())
        )]
    );
}

/// A dependency Renovate skipped or failed to look up this time was not
/// checked, so its open finding is no evidence of anything and stays open.
#[tokio::test]
async fn an_update_renovate_could_not_check_stays_open() {
    let rig = Rig::new("depupd-unchecked").await;
    let checkout = |skipped: bool| {
        let dep = if skipped {
            r#"{"depName":"actions/checkout","currentValue":"v4","datasource":"github-tags","skipReason":"github-token-required"}"#.to_owned()
        } else {
            r#"{"depName":"actions/checkout","currentValue":"v4","currentVersion":"v4.4.0","datasource":"github-tags","updates":[{"newVersion":"v7.0.1","updateType":"major","branchName":"renovate/actions-checkout-7.x"}]}"#.to_owned()
        };
        format!(
            r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"github-actions":[{{"packageFile":".github/workflows/ci.yml","deps":[{dep}]}}],"npm":[{{"packageFile":"package.json","deps":[{{"depName":"left-pad","currentVersion":"1.3.0","datasource":"npm","updates":[]}}]}}]}}}}"#
        )
    };

    rig.scan(&checkout(false)).await.unwrap();
    rig.scan(&checkout(true)).await.unwrap();

    assert_eq!(
        rig.findings().await,
        vec![(
            "widget:dep:actions-checkout-7.x@v4.4.0".into(),
            "new".into(),
            Some("v7.0.1".into())
        )]
    );
}

/// A newer target that could not be saved fails the cycle like a failed
/// insert: the cadence stays put so the next cycle retries, nothing is
/// retired on the strength of an answer that was not recorded, and the
/// scan is counted as `invalid entries` on the repo's `dep_update` work.
#[tokio::test]
async fn a_failed_refresh_fails_the_cycle() {
    let rig = Rig::new("depupd-refresh-fails").await;
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    // Open, and not proposed by the failing scan below: a sweep would
    // retire it.
    rig.seed_dep("widget:dep:other-1.x@1.0.0", "other", rig.repo.id)
        .await;
    sqlx::query("UPDATE repos SET last_dep_update_at = NULL")
        .execute(&rig.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_refresh BEFORE UPDATE OF latest_version ON findings \
         BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&rig.pool)
    .await
    .unwrap();

    let s = rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();

    assert_eq!(
        s.ingest.as_ref().map(|i| (i.refreshed, i.invalid)),
        Some((0, 1)),
        "{s:?}"
    );
    assert_eq!(s.outcome.as_deref(), Some("retry"), "{s:?}");
    let streak = rig
        .store
        .repo_work_streak(rig.repo.id, hunter::domain::RepoWorkKind::DepUpdate)
        .await
        .unwrap();
    assert_eq!(
        (streak.streak, streak.last_failure.as_deref()),
        (1, Some("invalid entries"))
    );
    let after = rig
        .store
        .get_repo_by_id(rig.repo.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.last_dep_update_at, None,
        "the cadence must not advance"
    );
    assert_eq!(
        rig.status_of("widget:dep:other-1.x@1.0.0").await,
        "new",
        "nothing is retired on an answer that was not recorded"
    );
}

/// `queued` means the operator wants it fixed, and no worker has started:
/// it takes the newer target like `new` does, but a scan never retires it.
#[tokio::test]
async fn a_queued_update_takes_the_newer_target_and_is_never_retired() {
    let rig = Rig::new("depupd-queued").await;
    let fp = "widget:dep:left-pad-1.x@1.2.0";
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    rig.set_status(fp, "queued", None).await;

    rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "queued".into(), Some("1.4.0".into()))]
    );

    rig.scan(&left_pad("1.2.0", None)).await.unwrap();
    assert_eq!(rig.status_of(fp).await, "queued");
}

/// Only what the scan itself retired comes back when proposed again. A
/// finding superseded for another reason (the harvest saw its work land
/// in another PR) stays retired, and is not rewritten either.
#[tokio::test]
async fn an_update_superseded_for_another_reason_stays_retired() {
    let rig = Rig::new("depupd-superseded-elsewhere").await;
    let fp = "widget:dep:left-pad-1.x@1.2.0";
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    rig.set_status(fp, "superseded", Some("harvest: duplicate of #12"))
        .await;

    let s = rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();

    assert_eq!(s.ingest.as_ref().map(|i| i.refreshed), Some(0), "{s:?}");
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "superseded".into(), Some("1.3.0".into()))]
    );
}

/// A scan speaks for its own repo's dependency updates only.
#[tokio::test]
async fn a_scan_retires_nothing_outside_its_repos_dep_updates() {
    let rig = Rig::new("depupd-scope").await;
    let other_repo = rig
        .store
        .add_repo(
            "gadget",
            "git@example.com:acme/gadget.git",
            &rig.cfg.work_root.join("repos"),
            "main",
            ForgeName::Gitlab,
        )
        .await
        .unwrap();
    rig.seed_dep("gadget:dep:left-pad-1.x@1.0.0", "left-pad", other_repo)
        .await;
    rig.store
        .upsert_finding(
            rig.repo.id,
            &hunter::store::FindingInsert {
                fingerprint: "widget:src/lib.rs:parse:test-gap".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Low,
                confidence: 0.9,
                summary: "untested".to_owned(),
                package: Some("left-pad".to_owned()),
                ..Default::default()
            },
            "test_gap",
            None,
        )
        .await
        .unwrap();

    rig.scan(&left_pad("1.3.0", None)).await.unwrap();

    assert_eq!(rig.status_of("gadget:dep:left-pad-1.x@1.0.0").await, "new");
    assert_eq!(
        rig.status_of("widget:src/lib.rs:parse:test-gap").await,
        "new"
    );
}

/// A cleanup that fails fails the cycle: the cadence stays put, so the next
/// cycle retries instead of leaving retired updates open for a whole scan
/// interval.
#[tokio::test]
async fn a_failed_cleanup_fails_the_cycle() {
    let rig = Rig::new("depupd-sweep-fails").await;
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    sqlx::query("UPDATE repos SET last_dep_update_at = NULL")
        .execute(&rig.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_sweep BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'superseded' BEGIN SELECT RAISE(ABORT, 'refused'); END",
    )
    .execute(&rig.pool)
    .await
    .unwrap();

    let result = rig.scan(&left_pad("1.2.0", None)).await;

    assert!(result.is_err(), "{result:?}");
    let after = rig
        .store
        .get_repo_by_id(rig.repo.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.last_dep_update_at, None,
        "the cadence must not advance"
    );
}

/// A queued update the scan files under another fingerprint -- the AI
/// fallback names units differently, an older finding keyed on versions --
/// hands its queue (and budget override) to the scan's finding for the same
/// move, so the update is worked once. Not the same move (the other class),
/// nothing to hand over to, or already in a PR: left as it is.
#[tokio::test]
async fn a_queued_update_hands_its_queue_to_the_scans_finding_for_the_same_move() {
    let rig = Rig::new("depupd-handover").await;
    let id = rig.repo.id;
    let fallback = "widget:dep:left-pad-non-major@1.2.0";
    let major = "widget:dep:left-pad-major@1.2.0";
    let in_pr = "widget:dep:left-pad:1.2.0\u{2192}1.2.9";
    rig.seed_update(fallback, "left-pad", "minor", id).await;
    rig.seed_update(major, "left-pad", "major", id).await;
    rig.seed_update(in_pr, "left-pad", "patch", id).await;
    rig.set_status(fallback, "queued", None).await;
    rig.set_status(major, "queued", None).await;
    rig.set_status(in_pr, "pr_open", None).await;
    sqlx::query("UPDATE findings SET budget_override = 'exempt' WHERE fingerprint = ?1")
        .bind(fallback)
        .execute(&rig.pool)
        .await
        .unwrap();

    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();

    let renovate = "widget:dep:left-pad-1.x@1.2.0";
    let reason = format!(
        "replaced by #{} from the dependency scan",
        rig.id_of(renovate).await
    );
    assert_eq!(
        rig.state_of(fallback).await,
        ("superseded".into(), Some(reason), Some("exempt".into()))
    );
    assert_eq!(
        rig.state_of(renovate).await,
        ("queued".into(), None, Some("exempt".into())),
        "the queue and its budget override move to the scan's finding"
    );
    assert_eq!(
        rig.status_of(major).await,
        "queued",
        "a major is not the same move"
    );
    assert_eq!(
        rig.status_of(in_pr).await,
        "pr_open",
        "a PR in flight is left alone"
    );
}

/// The scan's finding may be a group: a queued update for one of its
/// members is the same move.
#[tokio::test]
async fn a_queued_update_hands_its_queue_to_the_group_that_moves_it() {
    let rig = Rig::new("depupd-handover-group").await;
    let fallback = "widget:dep:@typescript-eslint/parser-non-major@8.64.0";
    rig.seed_update(fallback, "@typescript-eslint/parser", "minor", rig.repo.id)
        .await;
    rig.set_status(fallback, "queued", None).await;
    let member = |name: &str| {
        format!(
            r#"{{"depName":"{name}","currentVersion":"8.64.0","datasource":"npm","updates":[{{"newVersion":"8.71.1","updateType":"minor","branchName":"renovate/typescript-eslint-monorepo"}}]}}"#
        )
    };
    let line = format!(
        r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"npm":[{{"packageFile":"package.json","deps":[{},{}]}}]}}}}"#,
        member("@typescript-eslint/parser"),
        member("typescript-eslint")
    );

    rig.scan(&line).await.unwrap();

    assert_eq!(rig.status_of(fallback).await, "superseded");
    assert_eq!(
        rig.status_of(
            "widget:dep:typescript-eslint-monorepo@@typescript-eslint/parser=8.64.0+typescript-eslint=8.64.0"
        )
        .await,
        "queued"
    );
}

/// A queued update is the group's move only in the class that member is
/// moved in. The `node` group is `major` because of `node`; it moves
/// `@types/node` only within its major, so a queued *major* `@types/node`
/// update is not its move and keeps its queue.
#[tokio::test]
async fn a_queued_update_is_matched_by_the_members_own_class() {
    let rig = Rig::new("depupd-handover-class").await;
    let id = rig.repo.id;
    let types_major = "widget:dep:@types/node-major@24.13.3";
    let types_minor = "widget:dep:@types/node-non-major@24.13.3";
    let node_major = "widget:dep:node-major@24.0.0";
    rig.seed_update(types_major, "@types/node", "major", id)
        .await;
    rig.seed_update(types_minor, "@types/node", "minor", id)
        .await;
    rig.seed_update(node_major, "node", "major", id).await;
    for fp in [types_major, types_minor, node_major] {
        rig.set_status(fp, "queued", None).await;
    }
    // Both hand over to the group: `once` first, then `exempt`.
    for (fp, mode) in [(types_minor, "once"), (node_major, "exempt")] {
        sqlx::query("UPDATE findings SET budget_override = ?1 WHERE fingerprint = ?2")
            .bind(mode)
            .bind(fp)
            .execute(&rig.pool)
            .await
            .unwrap();
    }
    let line = r#"{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{"npm":[{"packageFile":"package.json","deps":[{"depName":"@types/node","currentVersion":"24.13.3","datasource":"npm","updates":[{"newVersion":"24.19.1","updateType":"minor","branchName":"renovate/node"}]},{"depName":"node","currentVersion":"24.0.0","datasource":"npm","updates":[{"newVersion":"26.0.0","updateType":"major","branchName":"renovate/node"}]}]}]}}"#;

    rig.scan(line).await.unwrap();

    let group = "widget:dep:node@@types/node=24.13.3+node=24.0.0";
    assert_eq!(
        rig.state_of(group).await,
        ("queued".into(), None, Some("exempt".into())),
        "the lasting exemption wins over a one-shot override that came first"
    );
    let logged: String = sqlx::query_scalar(
        "SELECT message FROM events WHERE kind = 'dep_update' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    assert!(logged.contains("/ 2 queued handed over /"), "{logged}");
    assert_eq!(
        rig.status_of(types_major).await,
        "queued",
        "the group moves @types/node within its major only"
    );
    assert_eq!(rig.status_of(types_minor).await, "superseded");
    assert_eq!(rig.status_of(node_major).await, "superseded");
}

/// A queued group is matched by its unit: one member was bumped by hand, so
/// the scan files the same Renovate branch under a new fingerprint. The
/// queue (and budget override) moves to it, so the group is not worked
/// twice, once against stale targets. Another unit -- the group's major
/// branch -- keeps its queue.
#[tokio::test]
async fn a_queued_group_hands_its_queue_to_the_same_branch() {
    let rig = Rig::new("depupd-handover-unit").await;
    let id = rig.repo.id;
    let old = "widget:dep:typescript-eslint-monorepo@@typescript-eslint/parser=8.64.0+typescript-eslint=8.64.0";
    let major = "widget:dep:major-typescript-eslint-monorepo@@typescript-eslint/parser=8.64.0+typescript-eslint=8.64.0";
    rig.seed_update(old, "typescript-eslint-monorepo", "minor", id)
        .await;
    rig.seed_update(major, "major-typescript-eslint-monorepo", "major", id)
        .await;
    rig.set_status(old, "queued", None).await;
    rig.set_status(major, "queued", None).await;
    sqlx::query("UPDATE findings SET budget_override = 'exempt' WHERE fingerprint = ?1")
        .bind(old)
        .execute(&rig.pool)
        .await
        .unwrap();
    let member = |name: &str, current: &str| {
        format!(
            r#"{{"depName":"{name}","currentVersion":"{current}","datasource":"npm","updates":[{{"newVersion":"8.71.1","updateType":"minor","branchName":"renovate/typescript-eslint-monorepo"}}]}}"#
        )
    };
    // parser was bumped by hand to 8.65.0.
    let line = format!(
        r#"{{"name":"renovate","level":20,"msg":"packageFiles with updates","config":{{"npm":[{{"packageFile":"package.json","deps":[{},{}]}}]}}}}"#,
        member("@typescript-eslint/parser", "8.65.0"),
        member("typescript-eslint", "8.64.0")
    );

    rig.scan(&line).await.unwrap();

    let now = "widget:dep:typescript-eslint-monorepo@@typescript-eslint/parser=8.65.0+typescript-eslint=8.64.0";
    assert_eq!(rig.status_of(old).await, "superseded");
    assert_eq!(
        rig.state_of(now).await,
        ("queued".into(), None, Some("exempt".into()))
    );
    assert_eq!(rig.status_of(major).await, "queued", "another unit");
}

/// A paused fix of a queued update: the update has a checkpoint, so a
/// worker is part way through it.
async fn pause_a_fix(rig: &Rig, fingerprint: &str) -> i64 {
    let fid = rig.id_of(fingerprint).await;
    let job: i64 = sqlx::query_scalar(
        "INSERT INTO jobs (kind, repo_id, finding_id, state, session_file, started_at) \
         VALUES ('fix', ?1, ?2, 'suspended', '/s/fix.jsonl', 1000) RETURNING id",
    )
    .bind(rig.repo.id)
    .bind(fid)
    .fetch_one(&rig.pool)
    .await
    .unwrap();
    support::point_checkpoint(&rig.pool, fid, "fix", job).await;
    job
}

/// The fix checkpoint of the finding `fingerprint`, if any.
async fn fix_checkpoint(rig: &Rig, fingerprint: &str) -> Option<i64> {
    rig.store
        .checkpoint_job(
            rig.id_of(fingerprint).await,
            hunter::domain::FindingJobKind::Fix,
        )
        .await
        .unwrap()
        .map(|j| j.id)
}

/// A queued update whose fix is paused part way is not handed over: the
/// scan would supersede it under the checkpoint, and the work done so far
/// would be dropped for a fresh fix of the scan's finding (WORK-MODEL.md
/// G2).
#[tokio::test]
async fn a_paused_fix_keeps_its_queued_update_from_being_handed_over() {
    let rig = Rig::new("depupd-paused-handover").await;
    let fallback = "widget:dep:left-pad-non-major@1.2.0";
    rig.seed_update(fallback, "left-pad", "minor", rig.repo.id)
        .await;
    rig.set_status(fallback, "queued", None).await;
    let paused = pause_a_fix(&rig, fallback).await;

    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();

    assert_eq!(rig.state_of(fallback).await, ("queued".into(), None, None));
    assert_eq!(fix_checkpoint(&rig, fallback).await, Some(paused));
    assert_eq!(
        rig.status_of("widget:dep:left-pad-1.x@1.2.0").await,
        "new",
        "the scan's own finding is not queued in its place"
    );
}

/// A queued update whose fix is paused part way keeps the target the fix
/// is working towards: a newer release does not rewrite it under the
/// checkpoint.
#[tokio::test]
async fn a_paused_fix_keeps_its_queued_update_from_being_refreshed() {
    let rig = Rig::new("depupd-paused-refresh").await;
    let fp = "widget:dep:left-pad-1.x@1.2.0";
    rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();
    rig.set_status(fp, "queued", None).await;
    let paused = pause_a_fix(&rig, fp).await;

    let s = rig.scan(&left_pad("1.2.0", Some("1.4.0"))).await.unwrap();

    assert_eq!(s.ingest.as_ref().map(|i| i.refreshed), Some(0), "{s:?}");
    assert_eq!(
        rig.findings().await,
        vec![(fp.into(), "queued".into(), Some("1.3.0".into()))]
    );
    assert_eq!(fix_checkpoint(&rig, fp).await, Some(paused));
}

/// A queued update whose fix is held keeps it from the scan too, also when
/// the hold has no checkpoint (its tree was never made, or it was a
/// `blocked` fix migrated without a transcript): the hold is the
/// operator's decision to make, and handing the finding over or rewriting
/// its target would make it behind their back (WORK-MODEL.md G2).
#[tokio::test]
async fn a_held_fix_without_a_checkpoint_keeps_its_queued_update() {
    let rig = Rig::new("depupd-held").await;
    let fallback = "widget:dep:left-pad-non-major@1.2.0";
    rig.seed_update(fallback, "left-pad", "minor", rig.repo.id)
        .await;
    rig.set_status(fallback, "queued", None).await;
    let fid = rig.id_of(fallback).await;
    assert!(
        rig.store
            .hold_work(
                fid,
                hunter::domain::FindingJobKind::Fix,
                hunter::domain::HoldReason::Stuck,
                "workspace not created",
                None
            )
            .await
            .unwrap()
    );

    let s = rig.scan(&left_pad("1.2.0", Some("1.3.0"))).await.unwrap();

    assert_eq!(rig.state_of(fallback).await, ("queued".into(), None, None));
    assert_eq!(s.ingest.as_ref().map(|i| i.refreshed), Some(0), "{s:?}");
    assert_eq!(
        rig.status_of("widget:dep:left-pad-1.x@1.2.0").await,
        "new",
        "the scan's own finding is not queued in its place"
    );
}
