#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The network gate: a cycle whose candidate needs a host this machine
//! cannot reach starts nothing of it (#95).
//!
//! The daemon runs on a laptop that travels and sleeps. Work started
//! offline fails for no fault of its own, and a failure counts toward the
//! item's streak until it is held for an operator. The gate probes the
//! candidate's forge and provider before dispatch and denies the cycle
//! instead.
//!
//! No network and no sockets: the probe's answer comes from the test
//! backend, every host reachable unless `ScriptedBackend::offline` marks it
//! down. The probe itself is `reach_test`'s.

mod support;

use hunter::config::Config;
use hunter::domain::{FindingStatus, Severity};
use hunter::scheduler::{Attempt, CycleSummary, run_cycle, work_hosts};
use hunter::store::{FindingInsert, Store};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hunter::backend::{NullBackend, OverdriveBackend};
use support::{GitRepo, ScriptedBackend, TempDir};

/// The forge the fixture's repo is on, as the probe sees it.
const FORGE: &str = "forge.example";

/// The provider a worker of the test backend talks to.
const PROVIDER: &str = "provider.example";

/// The forge URL of a repo served at `addr`.
fn forge_url(addr: &str) -> String {
    format!("https://{addr}/acme/widget")
}

/// Last `test_gap` stamp of the seeded repo, which only an attempt moves.
const LAST_TEST_GAP_AT: i64 = 1000;

/// One enabled repo at `url`, with the gate on. Cloned, it is due for a
/// `test_gap` scan and nothing else; not cloned, it is due for the hunt
/// that clones it.
async fn fixture(label: &str, url: &str, cloned: bool) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "net").await;
    let clone = dir.path().join("repo-1");
    if cloned {
        let repo = GitRepo::with_branch(&dir, "feature");
        std::fs::rename(&repo.work, &clone).unwrap();
        // The clone's origin is the URL, as the daemon's own clone's is,
        // and git is pointed from it to the local bare repo, so a fetch
        // never leaves the machine.
        let origin = repo.origin.to_string_lossy().to_string();
        support::git(&clone, &["remote", "set-url", "origin", url]);
        support::git(&clone, &["config", &format!("url.{origin}.insteadOf"), url]);
    }
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at, last_simplification_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(url)
    .bind(clone.to_string_lossy().to_string())
    .bind(future)
    .bind(LAST_TEST_GAP_AT)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    std::fs::write(
        dir.subdir("playbooks").join("test_gap.md"),
        "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.min_free_disk_bytes = 0;
    // The shared test config has it off; these tests are about it.
    cfg.network_gate = true;
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

/// The cycle was denied for `host`, to be retried in three minutes, and
/// left a `deny` event naming it.
async fn assert_denied_for(store: &Store, summary: &CycleSummary, host: &str, before: i64) {
    let after = hunter::util::now_ms();
    let reason = format!("offline: {host} unreachable");
    assert_eq!(
        summary.denied.as_deref(),
        Some(reason.as_str()),
        "{summary:?}"
    );
    assert_eq!(summary.job_id, None, "{summary:?}");
    let three_min = 3 * 60_000;
    assert!(
        summary
            .retry_at
            .is_some_and(|t| (before + three_min..=after + three_min).contains(&t)),
        "an offline cycle is looked at again in three minutes: {summary:?}"
    );
    let events = store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "deny" && e.message == reason),
        "{events:?}"
    );
    assert!(
        store.list_jobs(100).await.unwrap().is_empty(),
        "no job row is created"
    );
}

/// Unreachable forge, reachable provider: the scan does not start, and
/// does not count as its turn either.
#[tokio::test]
async fn an_unreachable_forge_denies_a_scan() {
    let down = FORGE.to_owned();
    let (_dir, store, cfg) = fixture("net-forge-down", &forge_url(&down), true).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"))
        .needing(&[PROVIDER])
        .offline(&[FORGE]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
    assert_eq!(summary.unreachable.as_deref(), Some(down.as_str()));
    assert_eq!(
        hunter::daemon::describe_cycle(&summary),
        ("offline".to_owned(), format!("{down} unreachable")),
        "the status page shows the cycle as offline, naming the host"
    );
    let repo = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_eq!(
        repo.last_test_gap_at,
        Some(LAST_TEST_GAP_AT),
        "a denied scan has not had its turn"
    );
}

/// Reachable forge, unreachable provider: the same scan is denied for the
/// provider, so a worker kind needs both of its hosts.
#[tokio::test]
async fn an_unreachable_provider_denies_a_scan() {
    let down = PROVIDER.to_owned();
    let (_dir, store, cfg) = fixture("net-provider-down", &forge_url(FORGE), true).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"))
        .needing(&[PROVIDER])
        .offline(&[PROVIDER]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
}

/// The hunt that clones a new repo is gated like any scan: denied, it has
/// not tried the clone.
#[tokio::test]
async fn an_unreachable_forge_denies_the_cloning_hunt() {
    let down = FORGE.to_owned();
    let (dir, store, cfg) = fixture("net-hunt-down", &forge_url(&down), false).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline")).offline(&[FORGE]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(hunter::domain::JobKind::as_str),
        Some("hunt"),
        "{summary:?}"
    );
    assert_denied_for(&store, &summary, &down, before).await;
    assert!(!dir.path().join("repo-1").exists(), "no clone was tried");
}

/// A queued fix denied offline is still queued, unclaimed and uncounted:
/// the gate runs before anything touches the finding.
#[tokio::test]
async fn a_denied_fix_leaves_its_finding_queued() {
    let down = FORGE.to_owned();
    let (_dir, store, cfg) = fixture("net-fix-down", &forge_url(&down), true).await;
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "fp-queued".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: Severity::Medium,
                confidence: 0.9,
                summary: "a queued bug".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store
        .set_finding_status(fid, FindingStatus::Queued)
        .await
        .unwrap();
    let before_row = store.get_finding(fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline")).offline(&[FORGE]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(hunter::domain::JobKind::as_str),
        Some("fix"),
        "{summary:?}"
    );
    assert_denied_for(&store, &summary, &down, before).await;
    let after_row = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(after_row.status, FindingStatus::Queued);
    assert_eq!(
        format!("{after_row:?}"),
        format!("{before_row:?}"),
        "the finding is untouched"
    );
}

/// The daemon runs its backend behind the overdrive switch, so the
/// provider's hosts have to come through it: otherwise the gate would only
/// ever see the forge.
#[tokio::test]
async fn the_overdrive_wrapper_reports_the_providers_hosts() {
    let down = PROVIDER.to_owned();
    let (_dir, store, cfg) = fixture("net-overdrive", &forge_url(FORGE), true).await;
    let provider = ScriptedBackend::new(|_| panic!("no worker may start offline"))
        .needing(&[PROVIDER])
        .offline(&[PROVIDER]);
    let backend = OverdriveBackend::new(Arc::new(provider), Arc::new(AtomicBool::new(false)));

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
}

/// A backend with no hosts of its own (here the router tests' `NullBackend`)
/// adds nothing to probe: over a local clone the cycle passes the gate and
/// reaches the budget, which that backend always refuses.
#[tokio::test]
async fn a_backend_without_hosts_adds_nothing_to_probe() {
    let (_dir, store, cfg) = fixture("net-null", "/srv/git/widget", true).await;

    let summary = run_cycle(&mut support::jobs(&store), &cfg, &NullBackend, None).await;

    assert_eq!(
        summary.denied.as_deref(),
        Some("no window data -- deny until fresh"),
        "{summary:?}"
    );
}

/// Both hosts answer: the candidate runs.
#[tokio::test]
async fn reachable_hosts_let_the_candidate_run() {
    let (_dir, store, cfg) = fixture("net-up", &forge_url(FORGE), true).await;
    let backend = ScriptedBackend::noop().needing(&[PROVIDER]);

    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(summary.denied, None, "{summary:?}");
    assert_eq!(
        summary.kind.map(hunter::domain::JobKind::as_str),
        Some("test_gap"),
        "{summary:?}"
    );
    assert!(summary.job_id.is_some(), "{summary:?}");
    assert_eq!(backend.runs().len(), 1, "the worker ran");
}

/// A second enabled repo, `beta`, cloned at `url` and due for a `test_gap`
/// scan just like the fixture's `alpha`, which is ahead of it in the
/// rotation. The returned directory holds beta's origin.
async fn add_beta(dir: &TempDir, store: &Store, url: &str) -> TempDir {
    let remote = TempDir::new("net-beta-origin");
    let repo = GitRepo::with_branch(&remote, "feature");
    let clone = dir.path().join("repo-2");
    std::fs::rename(&repo.work, &clone).unwrap();
    let origin = repo.origin.to_string_lossy().to_string();
    support::git(&clone, &["remote", "set-url", "origin", url]);
    support::git(&clone, &["config", &format!("url.{origin}.insteadOf"), url]);
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    // The fixture's database, which `fixture` seeded the same way.
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("net.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at, last_simplification_at) \
         VALUES (2, 'beta', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(url)
    .bind(clone.to_string_lossy().to_string())
    .bind(alpha.last_hunt_at)
    .bind(LAST_TEST_GAP_AT)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    remote
}

/// One repo's forge unreachable while another's answers, with the provider
/// up: the cycle passes over the unreachable repo's work to the next
/// candidate instead of being denied, cycle after cycle, for a host the
/// other repo never needed. This machine is online; only that forge is not.
#[tokio::test]
async fn an_unreachable_forge_does_not_block_another_repos_work() {
    const OTHER: &str = "other.example";
    let (dir, store, cfg) = fixture("net-one-forge-down", &forge_url(FORGE), true).await;
    let _beta_origin = add_beta(&dir, &store, &forge_url(OTHER)).await;
    let backend = ScriptedBackend::noop()
        .needing(&[PROVIDER])
        .offline(&[FORGE]);

    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(summary.denied, None, "{summary:?}");
    assert_eq!(summary.repo.as_deref(), Some("beta"), "{summary:?}");
    assert_eq!(backend.runs().len(), 1, "beta's scan ran");
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_eq!(
        alpha.last_test_gap_at,
        Some(LAST_TEST_GAP_AT),
        "alpha's scan, passed over, has not had its turn"
    );
    let events = store.recent_events(20).await.unwrap();
    let passed_over = format!("offline: {FORGE} unreachable; passed over test_gap on alpha");
    assert!(
        events
            .iter()
            .any(|e| e.kind == "deny" && e.message == passed_over),
        "the work passed over is named, with the host it waits for: {events:?}"
    );
}

/// The forge and the provider both down, as on a laptop with no network:
/// the forge is the first host the gate finds unreachable, but no other
/// repo's work could run either. The cycle is one denial, as before the
/// pass-over, with no work passed over and no event for it.
#[tokio::test]
async fn an_offline_machine_passes_nothing_over() {
    const OTHER: &str = "other.example";
    let down = FORGE.to_owned();
    let (dir, store, cfg) = fixture("net-all-down", &forge_url(FORGE), true).await;
    let _beta_origin = add_beta(&dir, &store, &forge_url(OTHER)).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"))
        .needing(&[PROVIDER])
        .offline(&[FORGE, OTHER, PROVIDER]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
    let denials: Vec<_> = store
        .recent_events(20)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "deny")
        .map(|e| e.message)
        .collect();
    assert_eq!(
        denials,
        [format!("offline: {down} unreachable")],
        "one denial, nothing passed over"
    );
}

/// Two repos on the same unreachable forge, the provider up: both are
/// passed over, and the forge, found down for the first, is not probed
/// again for the second, so a cycle with many repos on a VPN-only forge
/// does not wait out one probe timeout per repo.
#[tokio::test]
async fn a_forge_found_down_is_not_probed_again_in_the_cycle() {
    let down = FORGE.to_owned();
    let (dir, store, cfg) = fixture("net-forge-once", &forge_url(FORGE), true).await;
    let _beta_origin = add_beta(&dir, &store, &forge_url(FORGE)).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"))
        .needing(&[PROVIDER])
        .offline(&[FORGE]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
    let events = store.recent_events(20).await.unwrap();
    for repo in ["alpha", "beta"] {
        let passed_over = format!("offline: {FORGE} unreachable; passed over test_gap on {repo}");
        assert!(
            events
                .iter()
                .any(|e| e.kind == "deny" && e.message == passed_over),
            "{repo}'s work is passed over: {events:?}"
        );
    }
    let forge_probes = backend
        .probes()
        .iter()
        .filter(|hosts| hosts.contains(&down))
        .count();
    assert_eq!(forge_probes, 1, "probes: {:?}", backend.probes());
}

/// Work passed over for its unreachable forge, and the next candidate
/// denied by the budget: the cycle ends on that budget denial, which may
/// have no estimate (30 minutes) or one an hour away. The work passed
/// over still waits only on the network, so the lane looks again within
/// the gate's three minutes, as it would had that work been denied
/// itself.
#[tokio::test]
async fn work_passed_over_is_looked_at_again_after_a_budget_denial() {
    const OTHER: &str = "other.example";
    let (dir, store, cfg) = fixture("net-pass-then-budget", &forge_url(FORGE), true).await;
    let _beta_origin = add_beta(&dir, &store, &forge_url(OTHER)).await;
    let backend = ScriptedBackend::new(|_| panic!("the budget denies every worker"))
        .needing(&[PROVIDER])
        .offline(&[FORGE])
        .denying_above(-1);

    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert!(
        summary
            .denied
            .as_deref()
            .is_some_and(|d| d.contains("does not fit")),
        "beta's scan, picked after alpha's was passed over, is denied by the budget: {summary:?}"
    );
    let sleep_s = hunter::daemon::compute_sleep_s(&store, &summary).await;
    assert!(
        sleep_s <= 3.0 * 60.0 + 30.0,
        "the lane looks again when the network gate would, not after {sleep_s} s: {summary:?}"
    );
}

/// With `network.gate` off nothing is probed: a machine that reaches its
/// hosts only through a proxy would otherwise never run anything.
#[tokio::test]
async fn the_gate_switched_off_probes_nothing() {
    let (_dir, store, mut cfg) = fixture("net-off", &forge_url(FORGE), true).await;
    cfg.network_gate = false;
    let backend = ScriptedBackend::noop()
        .needing(&[PROVIDER])
        .offline(&[FORGE, PROVIDER]);

    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(summary.denied, None, "{summary:?}");
    assert_eq!(backend.runs().len(), 1, "the worker ran");
}

/// The repo's streaks, `sync` and `test_gap`: what a failure would count.
async fn repo_streaks(store: &Store) -> (i64, i64) {
    use hunter::domain::RepoWorkKind;
    (
        store
            .repo_work_streak(1, RepoWorkKind::Sync)
            .await
            .unwrap()
            .streak,
        store
            .repo_work_streak(1, RepoWorkKind::TestGap)
            .await
            .unwrap()
            .streak,
    )
}

/// Layer 2 for repo work: a fetch that fails while the forge has become
/// unreachable is the network's failure. The scan's runner is called
/// directly, as a cycle that passed the gate a moment earlier would; it
/// ends denied for the host and counts nothing on the repo's `sync`.
#[tokio::test]
async fn a_fetch_that_fails_while_offline_is_not_counted() {
    let (dir, store, cfg) = fixture("net-fetch-offline", &forge_url(FORGE), true).await;
    std::fs::remove_dir_all(dir.join("origin.git")).unwrap();
    let repo = store.get_repo_by_id(1).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline")).offline(&[FORGE]);

    let summary = hunter::scheduler::run_test_gap(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&repo),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        summary.denied.as_deref(),
        Some("offline: forge.example unreachable"),
        "{summary:?}"
    );
    assert_eq!(repo_streaks(&store).await, (0, 0));

    // The same failure with the forge reachable is the repo's own.
    let up = ScriptedBackend::new(|_| panic!("no worker may start without a fetch"));
    let failed = hunter::scheduler::run_test_gap(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&repo),
        &up,
    )
    .await;
    assert!(failed.is_err(), "{failed:?}");
    assert_eq!(repo_streaks(&store).await, (1, 0));
}

/// Layer 2 for repo work: a backend that cannot start the worker while the
/// forge is unreachable closes the job and counts nothing.
#[tokio::test]
async fn a_runner_error_while_offline_is_not_counted() {
    let (_dir, store, cfg) = fixture("net-runner-offline", &forge_url(FORGE), true).await;
    let repo = store.get_repo_by_id(1).await.unwrap().unwrap();
    let backend = ScriptedBackend::failing().offline(&[FORGE]);

    let summary = hunter::scheduler::run_test_gap(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&repo),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        summary.denied.as_deref(),
        Some("offline: forge.example unreachable"),
        "{summary:?}"
    );
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].job.state,
        hunter::domain::JobState::Failed,
        "the job is closed, not left running"
    );
    assert_eq!(repo_streaks(&store).await, (0, 0));
    assert_eq!(
        (summary.job_id, summary.finding_id),
        (Some(jobs[0].job.id), None),
        "the denial names its job, and no finding: the work is the repo's"
    );
}

/// A worker kind needs its forge and its provider; a local clone has no
/// forge to reach.
#[test]
fn work_needs_the_forge_and_the_provider() {
    let backend = ScriptedBackend::noop().needing(&[PROVIDER]);
    assert_eq!(
        work_hosts("git@forge.example:acme/widget.git", &backend),
        [FORGE, PROVIDER]
    );
    assert_eq!(work_hosts("/srv/git/widget", &backend), [PROVIDER]);
}
