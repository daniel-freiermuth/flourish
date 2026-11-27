#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_daemon` end to end, run for real against a scratch root: the
//! loop, the pause, stopping while paused, what the daemon binds, and the
//! startup sweep.
//!
//! Everything else in the suite drives the router or the scheduler
//! directly, so nothing ran the daemon loop itself — mutation testing
//! showed that replacing `run_daemon` with `Ok(())`, or never entering its
//! loop, left every test green. Everything the daemon could reach outside
//! the root is redirected first: a fake `omp` on `PATH`, `HOME` pointed at
//! scratch so omp's real usage database is never read, a port nothing
//! else uses, and an auth broker and gateway that are up
//! (`support::healthy_auth`): the daemon holds every job while they are
//! not.
//!
//! It also pins a bug found while rebasing the pause feature: the paused
//! wait listened for a private `ctrl_c()`, which sees only SIGINT, so
//! `systemctl stop` (SIGTERM) on a paused daemon went unanswered until
//! systemd's stop timeout killed it.
//!
//! Two tests stop their daemon by sending SIGTERM to `Pid::this()`. That
//! reaches only the test that sent it: `FakeBins::acquire` refuses to run
//! outside nextest's process-per-test mode.
//!
//! Each test runs in a cgroup of its own ([`daemon_bins`]): the daemon
//! claims the cgroup it runs in (`hunter::child::start`).

mod support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::{FakeBins, TempDir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The worker model every config here names: an anthropic config needs
/// one (`Config::load`).
const MODELS: &str = r#""models": {"default": "test-model"}"#;

/// A port for a daemon under test to listen on, free now on 127.0.0.1
/// and 127.0.0.2, and below the kernel's ephemeral range
/// (`ip_local_port_range`, 32768-60999 unless changed).
///
/// The daemon binds it only seconds later, once its auth children have
/// settled. A port the kernel handed out to a `bind(0)` and took back is
/// meanwhile anyone's next ephemeral port: another test's `bind(0)`, any
/// outgoing connection's. Mutation-testing baselines failed that way, the
/// daemon gone before it served or "127.0.0.1:<port> answered" in
/// `the_daemon_binds_the_configured_address`. Below that range only an
/// explicit bind takes a port.
fn free_port() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let seed = u64::from(std::process::id()) * 2_654_435_761 + u64::from(nanos);
    (0..10_000u64)
        .map(|i| 10_000 + u16::try_from((seed + i * 7_919) % 22_000).unwrap())
        .find(|&port| {
            ["127.0.0.1", "127.0.0.2"]
                .iter()
                .all(|ip| std::net::TcpListener::bind((*ip, port)).is_ok())
        })
        .expect("a free port below the ephemeral range")
}

/// [`FakeBins::acquire`], with this test's process first in a cgroup of
/// its own, which a daemon it runs then claims: the cgroup nextest's
/// processes share holds every other test's children, which the daemon
/// would refuse to start beside.
fn daemon_bins(label: &str) -> FakeBins {
    support::private_cgroup(label);
    FakeBins::acquire(label)
}

/// `run_daemon(cfg)` on a thread and runtime of its own, as `main` gives
/// it: the daemon future is not `Send`, so it cannot be a task on the
/// test's runtime. An error it returns is printed as well, so a test that
/// finds the daemon gone early (`wait_for_summary`) shows why.
fn spawn_daemon(cfg: hunter::config::Config) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
            .inspect_err(|e| eprintln!("run_daemon returned an error: {e:#}"))
    })
}

/// One HTTP/1.1 request with `Connection: close`; `None` if nothing is
/// listening yet. No client crate: this is the only test that needs one.
async fn http(
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    cookie: &str,
) -> Option<(u16, Value)> {
    http_at("127.0.0.1", port, method, path, body, cookie).await
}

/// [`http`] against `ip` rather than 127.0.0.1.
async fn http_at(
    ip: &str,
    port: u16,
    method: &str,
    path: &str,
    body: &str,
    cookie: &str,
) -> Option<(u16, Value)> {
    let mut stream = tokio::net::TcpStream::connect((ip, port)).await.ok()?;
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Cookie: {cookie}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.ok()?;
    let text = String::from_utf8_lossy(&raw);
    let status = text.split_whitespace().nth(1)?.parse().ok()?;
    let json = text
        .split_once("\r\n\r\n")
        .and_then(|(_, b)| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    Some((status, json))
}

/// Poll until `check` accepts the summary, or fail with `what`.
///
/// Fails at once if the daemon has already returned: waiting out the
/// deadline for a server that is gone only turns a clear failure into a
/// slow one.
async fn wait_for_summary(
    port: u16,
    daemon: &std::thread::JoinHandle<anyhow::Result<()>>,
    cookie: &str,
    what: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some((200, summary)) = http(port, "GET", "/api/summary", "", cookie).await
            && check(&summary)
        {
            return summary;
        }
        assert!(
            !daemon.is_finished(),
            "run_daemon returned while waiting for {what}"
        );
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll `check` until it holds, or fail with `what`; fails at once if the
/// daemon has returned, as [`wait_for_summary`] does.
async fn wait_until<F, Fut>(
    daemon: &std::thread::JoinHandle<anyhow::Result<()>>,
    what: &str,
    check: F,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(
            !daemon.is_finished(),
            "run_daemon returned while waiting for {what}"
        );
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_paused_daemon_reports_it_and_stops_on_sigterm() {
    a_paused_daemon_stops_on(nix::sys::signal::Signal::SIGTERM).await;
}

/// SIGHUP, what closing the terminal of a hand-run daemon sends, stops it
/// like SIGTERM: left at its default it would kill the daemon outright,
/// leaving every process it started running.
#[tokio::test]
async fn a_paused_daemon_stops_on_sighup() {
    a_paused_daemon_stops_on(nix::sys::signal::Signal::SIGHUP).await;
}

/// Start a daemon, pause it, then send this process `signal`: the daemon
/// must report the pause and stop promptly and cleanly.
async fn a_paused_daemon_stops_on(signal: nix::sys::signal::Signal) {
    let bins = daemon_bins("daemon");
    bins.omp("exit 0");
    let root = TempDir::new("daemon-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let mut cfg = support::load_config(root.path());
    support::healthy_auth(&mut cfg);
    // The API wants a session; the account goes in before the daemon opens
    // the database.
    let cookie = {
        std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
        let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
        support::auth_cookie(&store).await
    };

    let daemon = spawn_daemon(cfg);

    wait_for_summary(
        port,
        &daemon,
        &cookie,
        "the daemon to serve /api/summary",
        |_| true,
    )
    .await;

    let (status, body) = http(
        port,
        "POST",
        "/api/scheduler",
        r#"{"paused": true}"#,
        &cookie,
    )
    .await
    .expect("POST /api/scheduler");
    assert_eq!(status, 200, "body: {body}");

    // Only the loop writes this state, so seeing it proves the loop ran
    // and honoured the pause.
    wait_for_summary(
        port,
        &daemon,
        &cookie,
        "the scheduler loop to report paused",
        |s| s["scheduler_state"]["state"] == "paused",
    )
    .await;

    nix::sys::signal::kill(nix::unistd::Pid::this(), signal).expect("signal this test process");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a paused daemon must stop promptly on {signal}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// A GitHub repo `widget` cloned under `repos`, and one finding of it with
/// PR #7 open: its id.
async fn seed_open_pr(store: &hunter::store::Store, repos: &std::path::Path) -> i64 {
    std::fs::create_dir_all(repos).unwrap();
    let rid = store
        .add_repo(
            "widget",
            "https://github.com/acme/widget",
            repos,
            "main",
            hunter::domain::ForgeName::Github,
        )
        .await
        .unwrap();
    let (fid, _) = store
        .upsert_finding(
            rid,
            &hunter::store::FindingInsert {
                fingerprint: "fp-open-pr".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
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
        .set_finding_pr_open(fid, "https://github.com/acme/widget/pull/7")
        .await
        .unwrap();
    fid
}

/// The admin lane runs inside `run_daemon`, on its own: with no job to
/// run, a started daemon syncs its open PR and records what the forge
/// showed, and the summary carries the lane's activity. A pause stops
/// only jobs: paused, a trigger still syncs, and the PR's merge lands.
#[tokio::test]
async fn a_running_daemon_syncs_open_prs_on_its_admin_lane_even_paused() {
    let bins = daemon_bins("daemon-admin");
    bins.omp("exit 0");
    bins.ok(
        "gh",
        r#"{"state":"OPEN","mergeable":"CONFLICTING","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let root = TempDir::new("daemon-admin-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let mut cfg = support::load_config(root.path());
    support::healthy_auth(&mut cfg);
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
    let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
    let cookie = support::auth_cookie(&store).await;
    let fid = seed_open_pr(&store, &root.join("repos")).await;

    let daemon = spawn_daemon(cfg);

    let summary = wait_for_summary(
        port,
        &daemon,
        &cookie,
        "the daemon to serve /api/summary",
        |_| true,
    )
    .await;
    assert!(
        summary.as_object().unwrap().contains_key("admin_activity"),
        "{summary}"
    );
    wait_until(&daemon, "the admin lane to sync the open PR", || async {
        let ps = store.get_pr_state(fid).await.unwrap();
        ps.as_ref().and_then(|p| p.mergeable.as_deref()) == Some("CONFLICTING")
    })
    .await;

    let (status, body) = http(
        port,
        "POST",
        "/api/scheduler",
        r#"{"paused": true}"#,
        &cookie,
    )
    .await
    .expect("POST /api/scheduler");
    assert_eq!(status, 200, "body: {body}");
    bins.ok(
        "gh",
        r#"{"state":"MERGED","mergedAt":"2026-01-02T03:04:05Z","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let (status, body) = http(port, "POST", "/api/cycle", "{}", &cookie)
        .await
        .expect("POST /api/cycle");
    assert_eq!((status, body["error"].as_str()), (409, Some("paused")));
    wait_until(
        &daemon,
        "a paused daemon's trigger to sync the merge",
        || async {
            let status = store.get_finding(fid).await.unwrap().unwrap().status;
            status == hunter::domain::FindingStatus::Merged
        },
    )
    .await;

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !daemon.is_finished() {
        assert!(Instant::now() < deadline, "the daemon never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// The job lane does not wait for the PR sync: while the first pass is
/// still reading its PR, the lane has already run a cycle (only the loop
/// writes `scheduler_state`). Only engages wait for that pass, held back
/// from the pick; the repo here is disabled and its PR unflagged, so the
/// cycle finds nothing and reports idle.
#[tokio::test]
async fn the_job_lane_runs_cycles_while_the_first_pr_sync_runs() {
    let bins = daemon_bins("daemon-wait-sync");
    bins.omp("exit 0");
    let root = TempDir::new("daemon-wait-sync-root");
    let release = root.join("release-gh");
    // Bounded: a failing assertion below must not leave `gh` waiting.
    bins.script(
        "gh",
        &format!(
            "for _ in $(seq 1 400); do [ -f '{}' ] && break; sleep 0.05; done\n\
             echo '{{\"state\":\"OPEN\",\"mergeable\":\"MERGEABLE\",\"reviewDecision\":\"\",\"comments\":[],\"reviews\":[],\"statusCheckRollup\":[],\"updatedAt\":\"2026-01-02T03:04:05Z\",\"headRefName\":\"fix/some-bug\",\"headRefOid\":\"deadbeef\"}}'",
            release.display()
        ),
    );
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let mut cfg = support::load_config(root.path());
    support::healthy_auth(&mut cfg);
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
    let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
    let cookie = support::auth_cookie(&store).await;
    let fid = seed_open_pr(&store, &root.join("repos")).await;
    let rid = store.get_finding(fid).await.unwrap().unwrap().repo_id;
    store
        .update_repo(
            rid,
            &hunter::store::RepoUpdate {
                enabled: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let daemon = spawn_daemon(cfg);

    let summary = wait_for_summary(port, &daemon, &cookie, "the job lane to run a cycle", |s| {
        !s["scheduler_state"].is_null()
    })
    .await;
    let syncing = summary["admin_activity"]
        .as_array()
        .is_some_and(|a| a.iter().any(|t| t["task"] == "pr_sync"));
    std::fs::write(&release, "").unwrap();
    assert!(
        syncing,
        "fixture: the first PR sync ended before the cycle, so this proves nothing: {summary}"
    );
    assert_eq!(summary["scheduler_state"]["state"], "idle", "{summary}");

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !daemon.is_finished() {
        assert!(Instant::now() < deadline, "the daemon never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// The PR sync's view of PR #7 (`--json state,mergedAt,...`): open, with a
/// conflict, so every pass keeps the PR flagged for its engage.
const SYNC_VIEW_OPEN: &str = r#"{"state":"OPEN","mergeable":"CONFLICTING","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// The engage's view of PR #7 (`--json state,title,body,...`): merged.
const ENGAGE_VIEW_MERGED: &str = r#"{"state":"MERGED","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// `gh` telling the PR sync and the engage different stories about PR #7:
/// the sync's view (its field list has `mergedAt`) runs `before_sync`,
/// then shows the PR open and flagged; the engage's (`title,body`) shows
/// it merged. Anything else (the engage's review comments) is empty.
fn gh_disagreeing(bins: &FakeBins, before_sync: &str) {
    bins.script(
        "gh",
        &format!(
            "case \"$*\" in\n\
             *title,body*) cat <<'__PR_EOF__'\n{ENGAGE_VIEW_MERGED}\n__PR_EOF__\n;;\n\
             *mergedAt*) {before_sync}\ncat <<'__PR_EOF__'\n{SYNC_VIEW_OPEN}\n__PR_EOF__\n;;\n\
             esac\nexit 0"
        ),
    );
}

/// How many engage views of the PR `gh` has answered.
fn engage_views(bins: &FakeBins) -> usize {
    bins.calls_to("gh")
        .iter()
        .filter(|c| c.iter().any(|a| a.contains("title,body")))
        .count()
}

/// How many PR sync views of the PR `gh` has answered: one per pass.
fn sync_views(bins: &FakeBins) -> usize {
    bins.calls_to("gh")
        .iter()
        .filter(|c| c.iter().any(|a| a.contains("mergedAt")))
        .count()
}

/// A daemon's scratch root with an engage to pick and other work below
/// it: `widget`, disabled, with PR #7 open and already flagged by a PR
/// sync from before the daemon started; and `alpha`, enabled and cloned,
/// whose `test_gap` is the only rotation work due. Its omp usage windows
/// have room, so the budget lets both run.
struct EngageFixture {
    root: TempDir,
    cfg: hunter::config::Config,
    store: hunter::store::Store,
    fid: i64,
    alpha: i64,
    _home: support::EnvGuard,
}

impl EngageFixture {
    /// Set up the root; `gh` must already be scripted to show the sync PR
    /// #7 open with a conflict, without waiting.
    async fn new(bins: &FakeBins, label: &str) -> Self {
        let root = TempDir::new(label);
        let home = bins.env("HOME", root.path());
        seed_usage_windows(&root.join(".omp/agent/agent.db")).await;
        let port = free_port();
        std::fs::write(
            root.join("config.json"),
            format!(r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}}}}"#),
        )
        .unwrap();
        std::fs::write(
            root.subdir("playbooks").join("test_gap.md"),
            "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
        )
        .unwrap();
        let mut cfg = support::load_config(root.path());
        support::healthy_auth(&mut cfg);
        std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
        let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
        let repos = root.join("repos");
        let fid = seed_open_pr(&store, &repos).await;
        let widget = store.get_finding(fid).await.unwrap().unwrap().repo_id;
        store
            .update_repo(
                widget,
                &hunter::store::RepoUpdate {
                    enabled: Some(0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let git = support::GitRepo::with_branch(&root, "feature");
        let alpha = store
            .add_repo(
                "alpha",
                &git.origin.to_string_lossy(),
                &repos,
                "main",
                hunter::domain::ForgeName::Github,
            )
            .await
            .unwrap();
        std::fs::rename(&git.work, hunter::store::Store::repo_dir(&repos, alpha)).unwrap();
        let pool = support::open_pool(&cfg.db_path).await;
        sqlx::query(
            "UPDATE repos SET last_hunt_at = ?1, last_test_gap_at = 1000, \
             last_dep_update_at = ?1, last_refactor_at = ?1, \
             last_modernization_at = ?1, last_standards_at = ?1, \
             last_simplification_at = ?1 WHERE id = ?2",
        )
        .bind(hunter::util::now_ms() + 999_999)
        .bind(alpha)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        // The sync before the restart, which flagged the PR.
        let (_keep, running_on) = tokio::sync::watch::channel(false);
        hunter::daemon::admin_duty(
            &support::admin(&store),
            &cfg,
            &tokio::sync::Mutex::new(()),
            &hunter::activity::AdminStatus::default(),
            hunter::types::AdminTask::PrSync,
            &running_on,
        )
        .await;
        let ps = store.get_pr_state(fid).await.unwrap().unwrap();
        assert!(
            ps.needs_attention.is_some(),
            "fixture: the PR is not flagged"
        );

        Self {
            root,
            cfg,
            store,
            fid,
            alpha,
            _home: home,
        }
    }

    /// Whether the job lane has ended a cycle: only its loop writes
    /// `scheduler_state`.
    async fn first_cycle_ended(&self) -> bool {
        self.store.scheduler_state().await.unwrap().is_some()
    }

    /// Whether a job has run on `alpha`.
    async fn alpha_ran(&self) -> bool {
        self.store
            .list_jobs(100)
            .await
            .unwrap()
            .iter()
            .any(|j| j.job.repo_id == self.alpha)
    }

    fn start(&self) -> std::thread::JoinHandle<anyhow::Result<()>> {
        spawn_daemon(self.cfg.clone())
    }
}

/// omp's usage database with both Anthropic windows fresh and nearly
/// unused, late in their periods, so the budget grants every job.
async fn seed_usage_windows(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE usage_history (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         recorded_at INTEGER NOT NULL, provider TEXT NOT NULL, account_key TEXT NOT NULL, \
         limit_id TEXT NOT NULL, label TEXT NOT NULL, used_fraction REAL, status TEXT, \
         resets_at INTEGER)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let now = hunter::util::now_ms();
    for (limit, resets_in) in [
        ("anthropic:7d", 3 * 86_400_000),
        ("anthropic:5h", 1_800_000),
    ] {
        sqlx::query(
            "INSERT INTO usage_history (recorded_at, provider, account_key, limit_id, label, \
             used_fraction, status, resets_at) VALUES (?1, 'anthropic', 'acct', ?2, ?2, 0.0, 'ok', ?3)",
        )
        .bind(now)
        .bind(limit)
        .bind(now + resets_in)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
}

/// SIGTERM the daemon and wait for it to return cleanly.
async fn stop_daemon(daemon: std::thread::JoinHandle<anyhow::Result<()>>) {
    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !daemon.is_finished() {
        assert!(Instant::now() < deadline, "the daemon never stopped");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// A shell loop that waits until `release` exists, bounded so a failing
/// assertion never leaves `gh` waiting.
fn until_released(release: &std::path::Path) -> String {
    format!(
        "for _ in $(seq 1 600); do [ -f '{}' ] && break; sleep 0.05; done",
        release.display()
    )
}

/// Every engage waits for the first PR sync since startup: the PR may have
/// ended while the daemon was down. While that pass is still in `gh`, the
/// job lane runs the repo work below the engage tier and views no PR for
/// an engage; once the pass ends, the lane wakes for the engage at once
/// rather than after its own sleep (a minute, after that job).
#[tokio::test]
async fn the_job_lane_holds_engages_back_until_the_first_pr_sync_ends() {
    let bins = daemon_bins("daemon-first-sync-engage");
    bins.omp("exit 0");
    gh_disagreeing(&bins, "");
    let f = EngageFixture::new(&bins, "daemon-first-sync-engage-root").await;
    let release = f.root.join("release-gh");
    gh_disagreeing(&bins, &until_released(&release));
    let synced_before = sync_views(&bins);

    let daemon = f.start();
    wait_until(&daemon, "the job lane's first cycle to end", || {
        f.first_cycle_ended()
    })
    .await;
    let syncing = sync_views(&bins) > synced_before;
    let alpha_ran = f.alpha_ran().await;
    let views_during_sync = engage_views(&bins);
    std::fs::write(&release, "").unwrap();
    wait_until(
        &daemon,
        "the engage once the first PR sync ended",
        || async { engage_views(&bins) > 0 },
    )
    .await;
    stop_daemon(daemon).await;

    assert!(syncing, "fixture: the first PR sync was not in gh yet");
    assert!(alpha_ran, "the work below the engage tier did not run");
    assert_eq!(
        views_during_sync, 0,
        "an engage was picked before the first PR sync ended"
    );
}

/// An engage whose PR the forge shows merged while every PR sync pass
/// still reads it open (a sync that can never record the end) does not
/// keep the job lane on it. After each such cycle the lane asks for a
/// pass at once (the interval is five minutes), holds the engage back
/// until that pass ends and runs the work below it meanwhile, wakes when
/// the pass ends, then counts the end the pass did not record; the limit
/// holds the engage for the operator. The engage is viewed at most once
/// per pass, never in a loop.
#[tokio::test]
async fn an_engage_whose_pr_end_the_sync_never_records_is_held_without_a_hot_loop() {
    let bins = daemon_bins("daemon-engage-unrecorded");
    // A worker run takes longer than a PR sync pass, as a real one does.
    // alpha's test_gap fails every run (the worker writes nothing) and is
    // held at its streak limit, so its runs must not all fit in the wait
    // for the first pass: the lane needs work left to run while the
    // engage waits for the later ones.
    bins.omp("[ \"$1\" = -p ] && sleep 1\nexit 0");
    gh_disagreeing(&bins, "");
    let f = EngageFixture::new(&bins, "daemon-engage-unrecorded-root").await;
    // The first pass waits for the lane's first cycle, which it holds
    // every engage back for; after that each pass takes a moment, as a
    // real one does: an engage the lane failed to hold back would be
    // viewed many times while one runs.
    let release = f.root.join("release-gh");
    gh_disagreeing(&bins, &format!("{}\nsleep 0.2", until_released(&release)));
    let synced_before = sync_views(&bins);

    let daemon = f.start();
    wait_until(&daemon, "the job lane's first cycle to end", || {
        f.first_cycle_ended()
    })
    .await;
    std::fs::write(&release, "").unwrap();
    wait_until(
        &daemon,
        "the engage to be held for the operator",
        || async {
            support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
                .await
                .is_some()
        },
    )
    .await;
    let views = engage_views(&bins);
    let passes = sync_views(&bins) - synced_before;
    // Long enough for several cycles: a held engage is picked no more.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let views_after = engage_views(&bins);
    let calls = bins.calls();
    stop_daemon(daemon).await;

    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .unwrap();
    assert!(
        hold.detail
            .starts_with(hunter::scheduler::PR_END_UNRECORDED),
        "{hold:?}"
    );
    assert!(
        views <= passes + 1,
        "{views} engage views for {passes} PR sync passes: the lane looped on the engage"
    );
    assert_eq!(views_after, views, "the held engage was picked again");
    // Between the first and the last view of the PR, the lane ran a worker
    // on alpha: the engage held back did not hold the lane.
    let is_view = |c: &Vec<String>| c[0] == "gh" && c.iter().any(|a| a.contains("title,body"));
    let first = calls.iter().position(is_view).unwrap();
    let last = calls.iter().rposition(is_view).unwrap();
    assert!(
        calls[first..last]
            .iter()
            .any(|c| c[0] == "omp" && c.get(1).is_some_and(|a| a == "-p")),
        "no job ran while the engage waited for the sync: {calls:?}"
    );
}

/// `serve.host` is what the daemon binds, not a hard-coded loopback.
///
/// Proven without touching a real interface: Linux routes all of
/// 127.0.0.0/8 to loopback, so a daemon told to bind 127.0.0.2 must answer
/// there and refuse 127.0.0.1 -- which a hard-coded bind would accept.
#[tokio::test]
async fn the_daemon_binds_the_configured_address() {
    let bins = daemon_bins("daemon-bind");
    bins.omp("exit 0");
    let root = TempDir::new("daemon-bind-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(
            r#"{{"ompBin": "omp", {MODELS}, "serve": {{"host": "127.0.0.2", "port": {port}}}}}"#
        ),
    )
    .unwrap();
    let mut cfg = support::load_config(root.path());
    support::healthy_auth(&mut cfg);
    let daemon = spawn_daemon(cfg);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // Any answer will do: this is about who listens, not who may read.
        if http_at("127.0.0.2", port, "GET", "/api/summary", "", "")
            .await
            .is_some()
        {
            break;
        }
        assert!(!daemon.is_finished(), "run_daemon returned before serving");
        assert!(Instant::now() < deadline, "no answer on 127.0.0.2:{port}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err(),
        "127.0.0.1:{port} answered: the daemon ignored serve.host"
    );

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.is_finished() {
        assert!(Instant::now() < deadline, "daemon did not stop on SIGTERM");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon failed");
}

/// A daemon that starts reclaims the trees earlier runs left behind.
///
/// A crash or a restart mid-job leaves a tree on disk that no job will
/// ever use again -- here one from the previous layout, referenced by
/// nothing. The sweep is the only thing that removes it, and it only
/// runs if the daemon runs it: every tree is a full checkout on the disk
/// the clones share, so a daemon that never swept would fill that disk
/// one killed job at a time.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_reclaims_leftover_trees() {
    let dir = TempDir::new("daemon-sweep");
    let bins = daemon_bins("daemon-sweep");
    bins.omp("echo 'fake omp: no usage data here' >&2\nexit 1");
    let _home = bins.env("HOME", dir.path());
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.db_path = support::db_copy(&dir, "hunter");
    cfg.serve_port = free_port();
    cfg.omp_bin = "omp".to_owned();
    support::healthy_auth(&mut cfg);
    let leftover = cfg.work_root.join("wt").join("f57");
    std::fs::create_dir_all(leftover.join("src")).unwrap();
    std::fs::write(leftover.join("src").join("lib.rs"), "half removed\n").unwrap();

    let daemon = spawn_daemon(cfg);
    let deadline = Instant::now() + Duration::from_secs(30);
    while leftover.exists() {
        assert!(
            !daemon.is_finished(),
            "the daemon exited instead of running"
        );
        assert!(
            Instant::now() < deadline,
            "a running daemon never reclaimed {}",
            leftover.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_daemon(daemon).await;
}

/// A starting daemon turns git's automatic maintenance off in its own
/// clones before any job, also in one no fetch has reached since: a fix or
/// a resume starts its worker without fetching, and the worker's commit
/// would otherwise start detached housekeeping in the clone.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_turns_automatic_maintenance_off_in_its_clones() {
    let dir = TempDir::new("daemon-quiet");
    let bins = daemon_bins("daemon-quiet");
    bins.omp("echo 'fake omp: no usage data here' >&2\nexit 1");
    let _home = bins.env("HOME", dir.path());
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.serve_port = free_port();
    cfg.omp_bin = "omp".to_owned();
    support::healthy_auth(&mut cfg);
    let clone = cfg.work_root.join("repos").join("repo-1");
    std::fs::create_dir_all(&clone).unwrap();
    let git_config = |clone: &std::path::Path| {
        std::process::Command::new("git")
            .args(["config", "--local", "--get", "maintenance.auto"])
            .current_dir(clone)
            .output()
            .unwrap()
    };
    assert!(
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&clone)
            .status()
            .unwrap()
            .success()
    );
    let (db, pool) = support::fresh_pool(&dir, "hunter").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/r.git', ?1, 'github', 'main', 0, 1000)",
    )
    .bind(clone.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    cfg.db_path = db;

    let daemon = spawn_daemon(cfg);
    let deadline = Instant::now() + Duration::from_secs(30);
    while String::from_utf8_lossy(&git_config(&clone).stdout).trim() != "false" {
        assert!(
            !daemon.is_finished(),
            "the daemon exited instead of running"
        );
        assert!(
            Instant::now() < deadline,
            "a running daemon never turned maintenance off in {}",
            clone.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_daemon(daemon).await;
}

/// The startup sweep, which the daemon awaits before it serves, leaves the
/// clones' build caches alone: measuring every clone and clearing tens of
/// GiB there would hold up every start. The admin lane's sweep, beside the
/// lanes, clears them. Here measuring a clone's caches (`git ls-files
/// --others`) waits until the test releases it, which it does only once
/// the daemon serves: a startup sweep that measured would never let it.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_serves_before_its_sweep_caps_build_caches() {
    let dir = TempDir::new("daemon-cache-cap");
    let bins = daemon_bins("daemon-cache-cap");
    bins.omp("echo 'fake omp: no usage data here' >&2\nexit 1");
    let _home = bins.env("HOME", dir.path());
    let real_git = std::process::Command::new("sh")
        .args(["-c", "command -v git"])
        .output()
        .unwrap();
    let real_git = String::from_utf8(real_git.stdout).unwrap();
    let release = dir.join("release-git");
    // Bounded, so a failing test's daemon does not wait on forever.
    bins.script(
        "git",
        &format!(
            "case \" $* \" in *' --others '*) for _ in $(seq 1 900); do [ -f '{}' ] && break; sleep 0.05; done ;; esac\nexec '{}' \"$@\"",
            release.display(),
            real_git.trim()
        ),
    );
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.serve_port = free_port();
    cfg.omp_bin = "omp".to_owned();
    cfg.repo_cache_cap_bytes = 1;
    support::healthy_auth(&mut cfg);
    let clone = cfg.work_root.join("repos").join("repo-1");
    std::fs::create_dir_all(&clone).unwrap();
    support::git(&clone, &["init", "-q"]);
    let cache = clone.join("target").join("app");
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, vec![7u8; 64 << 10]).unwrap();
    let (db, pool) = support::fresh_pool(&dir, "hunter").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/r.git', ?1, 'github', 'main', 0, 1000)",
    )
    .bind(clone.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    cfg.db_path = db;
    let port = cfg.serve_port;

    let daemon = spawn_daemon(cfg);
    let deadline = Instant::now() + Duration::from_secs(30);
    while http(port, "GET", "/api/summary", "", "").await.is_none() {
        assert!(
            !daemon.is_finished(),
            "the daemon exited instead of running"
        );
        assert!(
            Instant::now() < deadline,
            "the daemon did not serve while measuring build caches waited"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        cache.exists(),
        "the caches were cleared before the daemon served"
    );

    std::fs::write(&release, "").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while cache.exists() {
        assert!(
            Instant::now() < deadline,
            "the admin lane's sweep never cleared the caches over the cap"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stop_daemon(daemon).await;
}

/// A skipped cycle did no work: its pick was stale (the finding left
/// `queued`), so the next cycle starts at once rather than idling the
/// queue for the 15-minute default.
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "the sleeps are exact constants, not computed values"
)]
async fn a_skipped_cycle_restarts_at_once() {
    let dir = TempDir::new("daemon-skip-sleep");
    let (_db, store) = support::fresh_store(&dir, "skip").await;
    let skipped = hunter::scheduler::CycleSummary {
        skipped: Some("finding #1 is no longer queued".into()),
        ..Default::default()
    };
    assert_eq!(hunter::daemon::compute_sleep_s(&store, &skipped).await, 0.0);
    let idle = hunter::scheduler::CycleSummary::default();
    assert_eq!(hunter::daemon::compute_sleep_s(&store, &idle).await, 900.0);
}

/// A failed cycle backs off five minutes; a finished job goes straight
/// back to the pick, even with nothing queued and no repo enabled; a
/// denial whose `retry_at` is 0 has no estimate and backs off 30.
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "the sleeps are exact constants, not computed values"
)]
async fn a_cycle_sleeps_by_how_it_ended() {
    let dir = TempDir::new("daemon-sleeps");
    let (_db, store) = support::fresh_store(&dir, "sleeps").await;
    for (summary, expected) in [
        (
            hunter::scheduler::CycleSummary {
                error: Some("boom".into()),
                ..Default::default()
            },
            300.0,
        ),
        (
            hunter::scheduler::CycleSummary {
                state: Some(hunter::domain::JobState::Done),
                ..Default::default()
            },
            0.0,
        ),
        (
            hunter::scheduler::CycleSummary {
                denied: Some("pacing".into()),
                retry_at: Some(0),
                ..Default::default()
            },
            1800.0,
        ),
    ] {
        let slept = hunter::daemon::compute_sleep_s(&store, &summary).await;
        assert_eq!(slept, expected, "{summary:?}");
    }
}

/// A cycle that ran a job goes straight back to the pick, even with
/// nothing queued and no repo enabled: only the pick decides whether work
/// is due. When it finds none, that idle cycle sleeps the idle interval,
/// so a job costs at most one extra cycle before any sleep.
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "the sleeps are exact constants, not computed values"
)]
async fn a_finished_job_goes_straight_back_to_the_pick() {
    let dir = TempDir::new("daemon-job-sleep");
    let (_db, store) = support::fresh_store(&dir, "job").await;
    let done = hunter::scheduler::CycleSummary {
        state: Some(hunter::domain::JobState::Done),
        ..Default::default()
    };
    assert_eq!(hunter::daemon::compute_sleep_s(&store, &done).await, 0.0);
    let idle = hunter::scheduler::CycleSummary::default();
    assert_eq!(hunter::daemon::compute_sleep_s(&store, &idle).await, 900.0);
}

/// The idle cycle a job's cycle goes straight back to keeps naming the
/// job in the status: it follows at once, and would otherwise wipe what
/// the job did before anyone could read it. The idle cycle after that
/// says idle alone, as one after a sleep always did.
#[test]
fn the_idle_cycle_after_a_job_keeps_naming_the_job() {
    let done = hunter::scheduler::CycleSummary {
        state: Some(hunter::domain::JobState::Done),
        kind: Some(hunter::domain::JobKind::Finding(
            hunter::domain::FindingJobKind::Fix,
        )),
        finding_id: Some(7),
        outcome: Some("pr opened".into()),
        ..Default::default()
    };
    let idle = hunter::scheduler::CycleSummary {
        idle: Some("nothing to do".into()),
        ..Default::default()
    };
    let mut last_job = None;
    assert_eq!(
        hunter::daemon::cycle_state(&done, &mut last_job).1,
        "last: fix #7 -> pr opened"
    );
    assert_eq!(
        hunter::daemon::cycle_state(&idle, &mut last_job).1,
        "nothing to do; last: fix #7 -> pr opened"
    );
    assert_eq!(
        hunter::daemon::cycle_state(&idle, &mut last_job).1,
        "nothing to do"
    );
}

/// Only an idle cycle keeps naming the job before it: a cycle that ran a
/// job of its own, or was denied, says what it did and nothing older.
#[test]
fn a_cycle_that_is_not_idle_does_not_name_the_job_before_it() {
    let done = |fid| hunter::scheduler::CycleSummary {
        state: Some(hunter::domain::JobState::Done),
        kind: Some(hunter::domain::JobKind::Finding(
            hunter::domain::FindingJobKind::Fix,
        )),
        finding_id: Some(fid),
        outcome: Some("pr opened".into()),
        ..Default::default()
    };
    let denied = hunter::scheduler::CycleSummary {
        denied: Some("budget".into()),
        ..Default::default()
    };
    let mut last_job = None;
    hunter::daemon::cycle_state(&done(7), &mut last_job);
    assert_eq!(
        hunter::daemon::cycle_state(&done(8), &mut last_job).1,
        "last: fix #8 -> pr opened"
    );
    assert_eq!(
        hunter::daemon::cycle_state(&denied, &mut last_job).1,
        "last: budget"
    );
}

/// While a PR is open the job lane never sleeps past the admin lane's
/// interval: the admin lane wakes it only for work a sync made, and an
/// engage due a retry on a PR already flagged is not that. A budget
/// denial keeps its own backoff, which only the budget can lift.
#[tokio::test]
#[allow(
    clippy::float_cmp,
    reason = "the sleeps are exact constants, not computed values"
)]
async fn an_open_pr_caps_the_job_lanes_sleep_unless_denied() {
    let dir = TempDir::new("daemon-pr-cap");
    let (_db, store) = support::fresh_store(&dir, "pr-cap").await;
    let idle = hunter::scheduler::CycleSummary::default();
    let denied = hunter::scheduler::CycleSummary {
        denied: Some("pacing".into()),
        ..Default::default()
    };
    seed_open_pr(&store, &dir.path().join("repos")).await;
    assert_eq!(
        hunter::daemon::compute_sleep_s(&store, &idle).await,
        hunter::daemon::ADMIN_INTERVAL.as_secs_f64()
    );
    assert_eq!(
        hunter::daemon::compute_sleep_s(&store, &denied).await,
        1800.0
    );
}

/// A denied or idle cycle sleeps until the denial lifts (or for the idle
/// 15 minutes) or a kept attempt's free retry can run (`kept_retry_at`),
/// whichever is sooner, 30 s after it and at least a minute; a denial
/// with no estimate counts as 30 minutes.
#[tokio::test]
async fn a_cycle_wakes_for_a_kept_retry_before_its_own_sleep_ends() {
    let dir = TempDir::new("daemon-kept-retry");
    let (_db, store) = support::fresh_store(&dir, "kept-retry").await;
    let now = hunter::util::now_ms();
    let min = 60_000;
    for (denied, retry_at, kept_retry_at, expected) in [
        (true, Some(now + 50 * min), Some(now + 10 * min), 630.0),
        (true, Some(now + 20 * min), Some(now + 55 * min), 1230.0),
        (true, None, Some(now + 10 * min), 630.0),
        (true, None, Some(now + 40 * min), 1800.0),
        (true, Some(now + 20 * min), Some(now - min), 60.0),
        (true, Some(now + 20 * min), None, 1230.0),
        (false, None, Some(now + 10 * min), 630.0),
        (false, None, Some(now + 20 * min), 900.0),
    ] {
        let summary = hunter::scheduler::CycleSummary {
            denied: denied.then(|| "pacing".into()),
            retry_at,
            kept_retry_at,
            ..Default::default()
        };
        let slept = hunter::daemon::compute_sleep_s(&store, &summary).await;
        assert!(
            (slept - expected).abs() < 2.0,
            "{denied}/{retry_at:?}/{kept_retry_at:?}: slept {slept}, expected {expected}"
        );
    }
}

/// The status page counts down to the loop's wake: now plus the sleep,
/// in milliseconds, whole or fractional seconds alike.
#[test]
fn the_next_wake_is_the_sleep_after_now_in_ms() {
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 0.0), 1_000);
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 2.5), 3_500);
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 900.0), 901_000);
}

/// A second daemon started on the work root of a running one stops at the
/// work root's lock, before it touches the running daemon's cgroups: a
/// unit of the running one that is empty at that moment (between its
/// commands) stays, where removing it would fail the running daemon's
/// next spawn in it.
#[tokio::test]
async fn a_second_daemon_on_the_work_root_leaves_the_running_ones_cgroups_alone() {
    let bins = daemon_bins("daemon-second");
    bins.omp("exit 0");
    let root = TempDir::new("daemon-second-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let mut cfg = support::load_config(root.path());
    support::healthy_auth(&mut cfg);
    let cookie = {
        std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
        let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
        support::auth_cookie(&store).await
    };
    let daemon = spawn_daemon(cfg);
    wait_for_summary(port, &daemon, &cookie, "the daemon to serve", |_| true).await;
    // The running daemon's, made in its cgroup, empty for now.
    let unit = hunter::child::Unit::new("cycle").unwrap();
    let cgroup = hunter::child::own_cgroup()
        .unwrap()
        .parent()
        .unwrap()
        .join(unit.name());

    let second = std::process::Command::new(env!("CARGO_BIN_EXE_hunter"))
        .arg("--root")
        .arg(root.path())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    // The daemon logs to stdout (`tracing_subscriber::fmt`).
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        !second.status.success(),
        "the second daemon started: {said}"
    );
    assert!(said.contains("already running"), "{said}");
    assert!(
        cgroup.is_dir(),
        "the second daemon removed {}",
        cgroup.display()
    );
    stop_daemon(daemon).await;
}

/// A daemon started with SIGHUP ignored (`nohup`) keeps it ignored: it
/// runs on through a hangup, as whoever started it so wants, and still
/// stops on SIGTERM. The daemon binary, so that the ignore is inherited
/// the way `nohup` passes it on.
#[tokio::test]
async fn a_daemon_started_with_sighup_ignored_runs_on_through_a_hangup() {
    let bins = daemon_bins("daemon-nohup");
    bins.omp("exit 0");
    let root = TempDir::new("daemon-nohup-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    // No auth gateway to stand up: a provider that runs none.
    std::fs::write(
        root.join("config.json"),
        format!(
            r#"{{"ompBin": "omp", {MODELS}, "backend": {{"llmProvider": "openai-codex"}}, "serve": {{"port": {port}}}}}"#
        ),
    )
    .unwrap();
    let mut daemon = std::process::Command::new("sh")
        .args(["-c", "trap '' HUP; exec \"$0\" --root \"$1\""])
        .arg(env!("CARGO_BIN_EXE_hunter"))
        .arg(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = nix::unistd::Pid::from_raw(i32::try_from(daemon.id()).unwrap());
    let serving = async || http(port, "GET", "/api/summary", "", "").await.is_some();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !serving().await {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon ended early"
        );
        assert!(Instant::now() < deadline, "the daemon never served");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGHUP).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    assert_eq!(
        daemon.try_wait().unwrap(),
        None,
        "the hangup stopped the daemon"
    );
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while daemon.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "the daemon did not stop on SIGTERM"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(daemon.wait().unwrap().success());
}

/// The pids of the processes running `sleep 600` in the cgroup `dir` and
/// below it: the fake omp's auth broker and gateway ([`FakeBins::omp`]).
fn auth_sleeps_in(dir: &std::path::Path) -> Vec<u32> {
    let mut pids: Vec<u32> = std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c == b"sleep\x00600\x00")
        })
        .collect();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            pids.extend(auth_sleeps_in(&entry.path()));
        }
    }
    pids
}

/// Kills everything in the cgroups below `.0` when dropped: what a
/// failing test left there (the test itself runs in `.0`).
struct KillBelow(std::path::PathBuf);

impl Drop for KillBelow {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).unwrap().flatten() {
            let _ = std::fs::write(entry.path().join("cgroup.kill"), "1");
        }
    }
}

/// A daemon stopped while it waits at startup for its auth broker and
/// gateway to answer stops the way it always does: the wait is cut short,
/// it ends both (they are its children) and exits 0. Before its handlers
/// were installed first, the SIGTERM killed it there by its default
/// action, and both ran on. The daemon binary, so that the signal is its
/// own.
#[tokio::test]
async fn a_daemon_stopped_while_its_auth_gateway_starts_ends_it_and_exits_cleanly() {
    let cgroup = support::private_cgroup("daemon-stop-starting");
    let _kill = KillBelow(cgroup.clone());
    let bins = FakeBins::acquire("daemon-stop-starting");
    // Its auth broker and gateway sleep, never answering.
    bins.omp("exit 0");
    let root = TempDir::new("daemon-stop-starting-root");
    let _home = bins.env("HOME", root.path());
    let (port, broker, gateway) = (free_port(), free_port(), free_port());
    assert!(port != broker && port != gateway && broker != gateway);
    std::fs::write(
        root.join("config.json"),
        format!(
            r#"{{"ompBin": "omp", {MODELS}, "serve": {{"port": {port}}},
                "authGateway": {{"brokerPort": {broker}, "gatewayPort": {gateway}}}}}"#
        ),
    )
    .unwrap();
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_hunter"))
        .arg("--root")
        .arg(root.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while auth_sleeps_in(&cgroup).len() < 2 {
        assert!(
            daemon.try_wait().unwrap().is_none(),
            "the daemon ended early"
        );
        assert!(
            Instant::now() < deadline,
            "the auth broker and gateway never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(1)).await;

    let pid = nix::unistd::Pid::from_raw(i32::try_from(daemon.id()).unwrap());
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).unwrap();

    // Well inside the 15 s the startup waits for the gateway at most.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = daemon.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon went on waiting for its gateway"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        status.success(),
        "the daemon did not stop cleanly: {status}"
    );
    assert_eq!(auth_sleeps_in(&cgroup), Vec::<u32>::new(), "left running");
}
