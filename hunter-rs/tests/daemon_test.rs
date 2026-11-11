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
    let fid = seed_open_pr(&store, &root.join("repos")).await;

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
    let bins = FakeBins::acquire("daemon-wait-sync");
    bins.script("omp", "exit 0");
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
        format!(r#"{{"ompBin": "omp", "serve": {{"port": {port}}}}}"#),
    )
    .unwrap();
    let cfg = support::load_config(root.path());
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

    let daemon = std::thread::spawn(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime")
            .block_on(hunter::daemon::run_daemon(cfg))
    });

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
            format!(r#"{{"ompBin": "omp", "serve": {{"port": {port}}}}}"#),
        )
        .unwrap();
        std::fs::write(
            root.subdir("playbooks").join("test_gap.md"),
            "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
        )
        .unwrap();
        let cfg = support::load_config(root.path());
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
        let cfg = self.cfg.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("daemon runtime")
                .block_on(hunter::daemon::run_daemon(cfg))
        })
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
    let bins = FakeBins::acquire("daemon-first-sync-engage");
    bins.script("omp", "exit 0");
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
    let bins = FakeBins::acquire("daemon-engage-unrecorded");
    bins.script("omp", "exit 0");
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
