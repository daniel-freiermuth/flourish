#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The auth gate: while workers cannot get provider credentials through
//! the daemon's auth gateway, no job that would run one starts. The cycle
//! is held as a budget denial holds it -- `denied` with the reason, a
//! `deny` event, the status page's "denied" -- and the work is still there
//! once the gateway is back.

mod support;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use hunter::backends::omp_scavenge::OmpScavengeBackend;
use hunter::backends::omp_scavenge::gateway::AuthGateway;
use hunter::config::Config;
use hunter::daemon::describe_cycle;
use hunter::scheduler::{pick_next, run_cycle};
use hunter::store::Store;
use hunter::types::RunResult;
use support::{GitRepo, ScriptedBackend, TempDir};

/// An enabled repo with a real clone, whose `test_gap` is due: a cycle
/// with credentials would run it.
async fn fixture(label: &str) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "auth").await;
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    // The clone's origin is the repo's URL, as the daemon's own clone's is,
    // and git is pointed from it to the local bare repo, so a fetch never
    // leaves the machine.
    let url = "https://github.com/acme/widget";
    let origin = repo.origin.to_string_lossy().to_string();
    support::git(&clone, &["remote", "set-url", "origin", url]);
    support::git(&clone, &["config", &format!("url.{origin}.insteadOf"), url]);
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at, last_simplification_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, 1000, ?3, ?3, ?3, ?3, ?3)",
    )
    .bind(url)
    .bind(clone.to_string_lossy().to_string())
    .bind(future)
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
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

/// A port nothing listens on.
fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The cycle was held for `why`, with a `deny` event, shown as "denied"
/// with the reason where a budget denial is, to be looked at again in a
/// minute.
async fn assert_held(store: &Store, summary: &hunter::scheduler::CycleSummary, before: i64) {
    let after = hunter::util::now_ms();
    let why = summary.denied.clone().unwrap_or_default();
    assert!(why.starts_with("auth: "), "{summary:?}");
    let minute = 60_000;
    assert!(
        summary
            .retry_at
            .is_some_and(|t| (before + minute..=after + minute).contains(&t)),
        "{summary:?}"
    );
    let events = store.recent_events(10).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "deny" && e.message == why),
        "{events:?}"
    );
    assert_eq!(
        describe_cycle(summary),
        ("denied".to_owned(), format!("last: {why}"))
    );
}

/// The auth broker and gateway do not answer: the cycle is held before
/// any job starts, and the work still waits.
#[tokio::test]
async fn a_cycle_is_held_while_the_auth_gateway_is_down() {
    let (dir, store, mut cfg) = fixture("auth-gate-down").await;
    cfg.auth_gateway.broker_port = dead_port();
    cfg.auth_gateway.gateway_port = dead_port();
    // A worker that ran would leave this behind.
    let ran = dir.join("worker-ran");
    let omp = dir.join("omp");
    std::fs::write(&omp, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    cfg.omp_bin = omp.to_string_lossy().into_owned();
    let gateway = Arc::new(AuthGateway::new(&cfg));
    gateway.reset().unwrap();
    let backend = OmpScavengeBackend {
        cfg: cfg.clone(),
        ledger: Arc::new(store.clone()),
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(hunter::backend::CmdProber),
        gateway: Some(gateway),
    };

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_held(&store, &summary, before).await;
    assert!(
        summary
            .denied
            .as_deref()
            .unwrap()
            .starts_with("auth: broker"),
        "the broker is checked first: {summary:?}"
    );
    assert_eq!(summary.job_id, None, "{summary:?}");
    assert!(store.list_jobs(100).await.unwrap().is_empty(), "no job row");
    assert!(!ran.exists(), "no worker ran");
    assert!(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap()
            .is_some(),
        "the work is still waiting"
    );
}

/// A loopback port that answers every request 200: another process that
/// holds it (a second daemon's gateway, an orphan).
fn foreign_listener() -> u16 {
    support::serve(|_, _| 200)
}

/// Another process holds the gateway's port: our gateway child exits on
/// it (`EADDRINUSE`), and what answers there is not ours, so the cycle is
/// held saying the port is taken, and no worker runs.
#[tokio::test]
async fn a_cycle_is_held_while_another_process_holds_the_gateway_port() {
    let (dir, store, mut cfg) = fixture("auth-gate-taken").await;
    let (broker, taken) = (foreign_listener(), foreign_listener());
    cfg.auth_gateway.broker_port = broker;
    cfg.auth_gateway.gateway_port = taken;
    let ran = dir.join("worker-ran");
    let omp = dir.join("omp");
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\ncase \"$1\" in\n\
             auth-broker) exec sleep 60 ;;\n\
             auth-gateway) echo 'error: Failed to start server. Is port {taken} in use?' >&2\n\
             echo ' code: \"EADDRINUSE\"' >&2\n\
             echo '      at async F (/$bunfs/root/omp-linux-x64:400:12)' >&2\n\
             exit 1 ;;\n\
             *) touch '{}' ;;\nesac\n",
            ran.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    cfg.omp_bin = omp.to_string_lossy().into_owned();
    // Long enough that a restarted gateway child, which exits at once,
    // never counts as settled.
    let gateway =
        Arc::new(AuthGateway::new(&cfg).with_child_settle(std::time::Duration::from_secs(2)));
    let services =
        hunter::backends::omp_scavenge::gateway::AuthServices::start(&cfg, &gateway).unwrap();
    let taken_reason = format!("auth: gateway port {taken} is taken by another process");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while gateway.health().as_deref() != Some(taken_reason.as_str()) {
        assert!(
            std::time::Instant::now() < deadline,
            "{:?}",
            gateway.health()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let backend = OmpScavengeBackend {
        cfg: cfg.clone(),
        ledger: Arc::new(store.clone()),
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(hunter::backend::CmdProber),
        gateway: Some(gateway),
    };

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_held(&store, &summary, before).await;
    assert_eq!(summary.denied.as_deref(), Some(taken_reason.as_str()));
    assert!(store.list_jobs(100).await.unwrap().is_empty(), "no job row");
    assert!(!ran.exists(), "no worker ran");
    drop(services);
}

/// The gateway did not admit a run's token in time, so the backend spawned
/// no worker (`auth-unavailable`): the job is closed as unstarted, the
/// cycle is held like the gate holds it, and nothing counts against the
/// work, which is still waiting.
#[tokio::test]
async fn a_run_refused_its_credentials_is_held_not_counted() {
    let (_dir, store, cfg) = fixture("auth-gate-refused").await;
    let why = "auth: gateway on 127.0.0.1:4000 did not admit job-1-0 within 20.0s (answered 401)";
    let backend = ScriptedBackend::new(move |_| RunResult {
        exit_code: None,
        killed_reason: Some("auth-unavailable".to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: why.to_owned(),
        usage_delta: None,
    });

    let before = hunter::util::now_ms();
    let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_held(&store, &summary, before).await;
    assert_eq!(summary.denied.as_deref(), Some(why));
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Failed);
    assert_eq!(summary.job_id, Some(jobs[0].job.id), "{summary:?}");
    assert_eq!(
        (summary.kind, summary.finding_id),
        (Some(hunter::domain::RepoJobKind::TestGap.into()), None),
        "the denial names its work, the repo's"
    );
    let streak = store
        .repo_work_streak(1, hunter::domain::RepoWorkKind::TestGap)
        .await
        .unwrap()
        .streak;
    assert_eq!(streak, 0, "not the work's failure");
    assert!(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap()
            .is_some(),
        "the work is still waiting"
    );
}

/// The daemon's backend is the overdrive wrapper around the provider's:
/// overdrive or not, it holds work while workers get no credentials.
#[tokio::test]
async fn the_overdrive_wrapper_holds_work_while_workers_get_no_credentials() {
    let (_dir, store, cfg) = fixture("auth-gate-overdrive").await;
    for overdrive in [false, true] {
        let provider = ScriptedBackend::new(|_| panic!("no worker without credentials"))
            .auth_down("auth: gateway is not running");
        let backend = hunter::backend::OverdriveBackend::new(
            Arc::new(provider),
            Arc::new(std::sync::atomic::AtomicBool::new(overdrive)),
        );

        let before = hunter::util::now_ms();
        let summary = run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

        assert_held(&store, &summary, before).await;
        assert!(
            store.list_jobs(100).await.unwrap().is_empty(),
            "{overdrive}"
        );
    }
}

/// The gateway or broker went down while a worker ran, after its token
/// was admitted: its calls failed and it exited without doing any metered
/// work. That is the outage's failure, not the work's: the job is closed
/// as never started, nothing is counted, and the cycle is held for it.
#[tokio::test]
async fn a_worker_the_gateway_failed_under_is_held_not_counted() {
    let (_dir, store, cfg) = fixture("auth-gate-lost").await;
    let repo = store.get_repo_by_id(1).await.unwrap().unwrap();
    let why = "auth: gateway is not running (last output: Received SIGTERM)";
    let backend = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        tokens_new: 0,
        calls: 0,
        ..support::done()
    })
    .auth_down(why);

    let before = hunter::util::now_ms();
    let summary = hunter::scheduler::run_test_gap(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        hunter::scheduler::Attempt::Cold(&repo),
        &backend,
    )
    .await
    .unwrap();

    assert_held(&store, &summary, before).await;
    assert_eq!(summary.denied.as_deref(), Some(why));
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1, "the worker ran");
    assert_eq!(jobs[0].job.state, hunter::domain::JobState::Failed);
    let streak = store
        .repo_work_streak(1, hunter::domain::RepoWorkKind::TestGap)
        .await
        .unwrap()
        .streak;
    assert_eq!(streak, 0, "not the work's failure");
}
