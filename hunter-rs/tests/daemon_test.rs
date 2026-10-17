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
//! scratch so omp's real usage database is never read, and a port nothing
//! else uses.
//!
//! It also pins a bug found while rebasing the pause feature: the paused
//! wait listened for a private `ctrl_c()`, which sees only SIGINT, so
//! `systemctl stop` (SIGTERM) on a paused daemon went unanswered until
//! systemd's stop timeout killed it.
//!
//! Two tests stop their daemon by sending SIGTERM to `Pid::this()`. That
//! reaches only the test that sent it: `FakeBins::acquire` refuses to run
//! outside nextest's process-per-test mode.

mod support;

use std::time::{Duration, Instant};

use serde_json::Value;
use support::{FakeBins, TempDir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

#[tokio::test]
async fn a_paused_daemon_reports_it_and_stops_on_sigterm() {
    let bins = FakeBins::acquire("daemon");
    bins.script("omp", "exit 0");
    let root = TempDir::new("daemon-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = support::load_config(root.path());
    // The API wants a session; the account goes in before the daemon opens
    // the database.
    let cookie = {
        std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
        let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
        support::auth_cookie(&store).await
    };

    // Its own thread and runtime, as `main` gives it: the daemon future is
    // not `Send`, so it cannot be a task on this test's runtime.
    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

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

    nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
        .expect("send SIGTERM to this test process");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !daemon.is_finished() {
        assert!(
            Instant::now() < deadline,
            "a paused daemon must stop promptly on SIGTERM, as systemctl stop sends it"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon
        .join()
        .expect("run_daemon thread panicked")
        .expect("run_daemon returned an error");
}

/// The admin lane runs inside `run_daemon`, on its own: with no job to
/// run, a started daemon syncs its open PR and records what the forge
/// showed, and the summary carries the lane's activity.
#[tokio::test]
async fn a_running_daemon_syncs_open_prs_on_its_admin_lane() {
    let bins = FakeBins::acquire("daemon-admin");
    bins.script("omp", "exit 0");
    bins.ok(
        "gh",
        r#"{"state":"OPEN","mergeable":"CONFLICTING","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
    let root = TempDir::new("daemon-admin-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = support::load_config(root.path());
    std::fs::create_dir_all(cfg.db_path.parent().unwrap()).unwrap();
    let store = hunter::store::Store::connect(&cfg.db_path).await.unwrap();
    let cookie = support::auth_cookie(&store).await;
    let repos = root.join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let rid = store
        .add_repo(
            "widget",
            "https://github.com/acme/widget",
            &repos,
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

    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

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
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let ps = store.get_pr_state(fid).await.unwrap();
        if ps.as_ref().and_then(|p| p.mergeable.as_deref()) == Some("CONFLICTING") {
            break;
        }
        assert!(!daemon.is_finished(), "run_daemon returned before syncing");
        assert!(
            Instant::now() < deadline,
            "the admin lane never synced the open PR: {ps:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

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

/// `serve.host` is what the daemon binds, not a hard-coded loopback.
///
/// Proven without touching a real interface: Linux routes all of
/// 127.0.0.0/8 to loopback, so a daemon told to bind 127.0.0.2 must answer
/// there and refuse 127.0.0.1 -- which a hard-coded bind would accept.
#[tokio::test]
async fn the_daemon_binds_the_configured_address() {
    let bins = FakeBins::acquire("daemon-bind");
    bins.script("omp", "exit 0");
    let root = TempDir::new("daemon-bind-root");
    let _home = bins.env("HOME", root.path());
    let port = free_port();
    std::fs::write(
        root.join("config.json"),
        format!(r#"{{"ompBin": "omp", "serve": {{"host": "127.0.0.2", "port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = support::load_config(root.path());
    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

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
    let bins = FakeBins::acquire("daemon-sweep");
    bins.fail("omp", 1, "fake omp: no usage data here");
    let _home = bins.env("HOME", dir.path());
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.db_path = support::db_copy(&dir, "hunter");
    cfg.serve_port = free_port();
    cfg.omp_bin = "omp".to_owned();
    let leftover = cfg.work_root.join("wt").join("f57");
    std::fs::create_dir_all(leftover.join("src")).unwrap();
    std::fs::write(leftover.join("src").join("lib.rs"), "half removed\n").unwrap();

    let daemon = tokio::spawn(hunter::daemon::run_daemon(cfg));
    let deadline = Instant::now() + Duration::from_secs(30);
    while leftover.exists() {
        assert!(
            !daemon.is_finished(),
            "the daemon exited instead of running: {:?}",
            daemon.await
        );
        assert!(
            Instant::now() < deadline,
            "a running daemon never reclaimed {}",
            leftover.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.abort();
}

/// A starting daemon turns git's automatic maintenance off in its own
/// clones before any job, also in one no fetch has reached since: a fix or
/// a resume starts its worker without fetching, and the worker's commit
/// would otherwise start detached housekeeping in the clone.
#[tokio::test(flavor = "multi_thread")]
async fn a_starting_daemon_turns_automatic_maintenance_off_in_its_clones() {
    let dir = TempDir::new("daemon-quiet");
    let bins = FakeBins::acquire("daemon-quiet");
    bins.fail("omp", 1, "fake omp: no usage data here");
    let _home = bins.env("HOME", dir.path());
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.serve_port = free_port();
    cfg.omp_bin = "omp".to_owned();
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

    let daemon = tokio::spawn(hunter::daemon::run_daemon(cfg));
    let deadline = Instant::now() + Duration::from_secs(30);
    while String::from_utf8_lossy(&git_config(&clone).stdout).trim() != "false" {
        assert!(
            !daemon.is_finished(),
            "the daemon exited instead of running: {:?}",
            daemon.await
        );
        assert!(
            Instant::now() < deadline,
            "a running daemon never turned maintenance off in {}",
            clone.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    daemon.abort();
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

/// The status page counts down to the loop's wake: now plus the sleep,
/// in milliseconds, whole or fractional seconds alike.
#[test]
fn the_next_wake_is_the_sleep_after_now_in_ms() {
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 0.0), 1_000);
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 2.5), 3_500);
    assert_eq!(hunter::daemon::next_wake_ms(1_000, 900.0), 901_000);
}
