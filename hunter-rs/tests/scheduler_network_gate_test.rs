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
//! No network: an unreachable host is a local port nobody listens on, a
//! reachable one a local listener.

mod support;

use hunter::config::Config;
use hunter::domain::{FindingStatus, Severity};
use hunter::scheduler::{CycleSummary, run_cycle, work_hosts};
use hunter::store::{FindingInsert, Store};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use hunter::backend::{NullBackend, OverdriveBackend};
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir};
use tokio::net::TcpListener;

/// A local port that accepts connections while the listener lives.
async fn listener() -> (TcpListener, String) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    (l, addr)
}

/// A local port nobody listens on.
async fn closed_port() -> String {
    let (l, addr) = listener().await;
    drop(l);
    addr
}

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
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?3, ?3, ?3, ?3)",
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
    let down = closed_port().await;
    let (_l, provider) = listener().await;
    let (_dir, store, cfg) = fixture("net-forge-down", &forge_url(&down), true).await;
    let backend =
        ScriptedBackend::new(|_| panic!("no worker may start offline")).needing(&[&provider]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
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
    let (_l, forge) = listener().await;
    let down = closed_port().await;
    let (_dir, store, cfg) = fixture("net-provider-down", &forge_url(&forge), true).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline")).needing(&[&down]);

    let before = hunter::util::now_ms();
    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
}

/// The hunt that clones a new repo is gated like any scan: denied, it has
/// not tried the clone.
#[tokio::test]
async fn an_unreachable_forge_denies_the_cloning_hunt() {
    let down = closed_port().await;
    let (dir, store, cfg) = fixture("net-hunt-down", &forge_url(&down), false).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"));

    let before = hunter::util::now_ms();
    let summary = run_cycle(&store, &cfg, &backend, None).await;

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
    let down = closed_port().await;
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
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"));

    let before = hunter::util::now_ms();
    let summary = run_cycle(&store, &cfg, &backend, None).await;

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
    let (_l, forge) = listener().await;
    let down = closed_port().await;
    let (_dir, store, cfg) = fixture("net-overdrive", &forge_url(&forge), true).await;
    let provider =
        ScriptedBackend::new(|_| panic!("no worker may start offline")).needing(&[&down]);
    let backend = OverdriveBackend::new(Arc::new(provider), Arc::new(AtomicBool::new(false)));

    let before = hunter::util::now_ms();
    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_denied_for(&store, &summary, &down, before).await;
}

/// A backend with no hosts of its own (here the router tests' `NullBackend`)
/// adds nothing to probe: over a local clone the cycle passes the gate and
/// reaches the budget, which that backend always refuses.
#[tokio::test]
async fn a_backend_without_hosts_adds_nothing_to_probe() {
    let (_dir, store, cfg) = fixture("net-null", "/srv/git/widget", true).await;

    let summary = run_cycle(&store, &cfg, &NullBackend, None).await;

    assert_eq!(
        summary.denied.as_deref(),
        Some("no window data -- deny until fresh"),
        "{summary:?}"
    );
}

/// An offline cycle still reports the PR sync it ran before picking, as a
/// disk denial does: the summary says what the cycle did.
#[tokio::test]
async fn an_offline_cycle_still_reports_its_pr_sync() {
    let bins = FakeBins::acquire("net-sync");
    bins.fail("gh", 1, "offline");
    let down = closed_port().await;
    let (_dir, store, cfg) = fixture("net-sync", &forge_url(&down), true).await;
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "fp-open-pr".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: Severity::Medium,
                confidence: 0.9,
                summary: "a bug with a PR".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store
        .set_finding_pr_open(fid, &format!("{}/pull/7", forge_url(&down)))
        .await
        .unwrap();
    let backend = ScriptedBackend::new(|_| panic!("no worker may start offline"));

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(
        summary.denied,
        Some(format!("offline: {down} unreachable")),
        "{summary:?}"
    );
    let sync = summary.sync.as_ref().expect("the sync is reported");
    assert_eq!(
        sync.synced + sync.errors,
        1,
        "the one open PR was looked at: {summary:?}"
    );
}

/// Both hosts answer: the candidate runs.
#[tokio::test]
async fn reachable_hosts_let_the_candidate_run() {
    let (_forge_l, forge) = listener().await;
    let (_provider_l, provider) = listener().await;
    let (_dir, store, cfg) = fixture("net-up", &forge_url(&forge), true).await;
    let backend = ScriptedBackend::noop().needing(&[&provider]);

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.denied, None, "{summary:?}");
    assert_eq!(
        summary.kind.map(hunter::domain::JobKind::as_str),
        Some("test_gap"),
        "{summary:?}"
    );
    assert!(summary.job_id.is_some(), "{summary:?}");
    assert_eq!(backend.runs().len(), 1, "the worker ran");
}

/// With `network.gate` off nothing is probed: a machine that reaches its
/// hosts only through a proxy would otherwise never run anything.
#[tokio::test]
async fn the_gate_switched_off_probes_nothing() {
    let down = closed_port().await;
    let (_dir, store, mut cfg) = fixture("net-off", &forge_url(&down), true).await;
    cfg.network_gate = false;
    let backend = ScriptedBackend::noop().needing(&[&down]);

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.denied, None, "{summary:?}");
    assert_eq!(backend.runs().len(), 1, "the worker ran");
}

/// A worker kind needs its forge and its provider; a local clone has no
/// forge to reach.
#[test]
fn work_needs_the_forge_and_the_provider() {
    let backend = ScriptedBackend::noop().needing(&["api.anthropic.com"]);
    assert_eq!(
        work_hosts("git@github.com:acme/widget.git", &backend),
        ["github.com", "api.anthropic.com"]
    );
    assert_eq!(
        work_hosts("/srv/git/widget", &backend),
        ["api.anthropic.com"]
    );
}
