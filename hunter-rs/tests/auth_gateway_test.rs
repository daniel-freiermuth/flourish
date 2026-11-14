#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Worker credentials through the daemon's auth gateway
//! (`backends::omp_scavenge::gateway`): every worker run reaches its
//! provider with a token of its own, listed in the gateway's clients file
//! exactly while the run lasts.
//!
//! The broker and the gateway are loopback HTTP fakes on free ports. The
//! fake gateway admits a token the way the real one does: when the SHA-256
//! of the bearer is listed in the clients file it reads.

mod support;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hunter::backend::{Backend, JobClass};
use hunter::backends::omp_scavenge::OmpScavengeBackend;
use hunter::backends::omp_scavenge::gateway::{AuthGateway, AuthServices};
use hunter::config::Config;
use hunter::types::RunResult;
use support::{TempDir, listed, serve, sha_hex};

/// A port nothing listens on.
fn dead_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A gateway that is up and never admits a token: one that is not
/// reading its clients file.
fn deaf_gateway() -> u16 {
    serve(|path, _| if path == "/healthz" { 200 } else { 401 })
}

struct Fixture {
    dir: TempDir,
    cfg: Config,
}

impl Fixture {
    /// A config whose worker is a script recording what it was handed
    /// into `rec/` -- its argv, its `ANTHROPIC_BASE_URL` and the clients
    /// file as it was while it ran -- and then exiting `exit`.
    fn new(label: &str, exit: i32) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new(label);
        let mut cfg = support::test_config(dir.path());
        cfg.work_root = dir.subdir("work_root");
        cfg.poll_s = 0.02;
        let rec = dir.subdir("rec");
        let omp = dir.join("omp");
        std::fs::write(
            &omp,
            format!(
                "#!/bin/sh\n\
                 printf '%s' \"$ANTHROPIC_BASE_URL\" > '{rec}/base_url'\n\
                 for a in \"$@\"; do printf '%s\\n' \"$a\"; done > '{rec}/argv'\n\
                 cp '{clients}' '{rec}/clients'\n\
                 exit {exit}\n",
                rec = rec.display(),
                clients = AuthGateway::clients_file(&cfg.work_root).display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
        cfg.omp_bin = omp.to_string_lossy().into_owned();
        Self { dir, cfg }
    }

    fn clients(&self) -> PathBuf {
        AuthGateway::clients_file(&self.cfg.work_root)
    }

    fn rec(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join("rec").join(name)).ok()
    }

    /// The gateway on `ports` (broker, gateway), its clients file emptied
    /// as at daemon start.
    fn gateway(&mut self, (broker, gateway): (u16, u16), ready: Duration) -> Arc<AuthGateway> {
        self.cfg.auth_gateway.broker_port = broker;
        self.cfg.auth_gateway.gateway_port = gateway;
        let gw = Arc::new(AuthGateway::new(&self.cfg).with_ready_timeout(ready));
        gw.reset().unwrap();
        gw
    }

    /// One worker run of chain 7 through `gateway`.
    async fn run(&self, gateway: Arc<AuthGateway>) -> RunResult {
        static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (_db, store) = support::fresh_store(&self.dir, &format!("gw-{n}")).await;
        let backend = OmpScavengeBackend {
            cfg: self.cfg.clone(),
            ledger: Arc::new(store),
            agent_db: PathBuf::from("/nonexistent/agent.db"),
            prober: Arc::new(hunter::backend::CmdProber),
            gateway: Some(gateway),
        };
        let ws = hunter::workspace::Workspace::for_chain(
            &self.cfg.work_root,
            &self.dir.subdir("clone"),
            7,
        );
        std::fs::create_dir_all(&ws.tree).unwrap();
        backend
            .run(&ws, "prompt", Some(1_000_000), 30, JobClass::Hunt, None)
            .await
            .unwrap()
    }
}

/// The worker is pointed at the gateway and presents, as `--api-key`, a
/// token the clients file lists while it runs; once the run is over the
/// file lists nothing, so the gateway refuses that token from then on.
async fn token_lives_exactly_as_long_as_the_run(label: &str, exit: i32) {
    let mut fx = Fixture::new(label, exit);
    let ports = (support::fake_broker(), support::fake_gateway(fx.clients()));
    let gw = fx.gateway(ports, Duration::from_secs(5));

    let rr = fx.run(gw.clone()).await;

    assert_eq!(rr.exit_code, Some(exit), "{rr:?}");
    assert_eq!(
        fx.rec("base_url").as_deref(),
        Some(format!("http://127.0.0.1:{}", ports.1).as_str())
    );
    let argv = fx.rec("argv").unwrap();
    let argv: Vec<&str> = argv.lines().collect();
    let at = argv
        .iter()
        .position(|a| *a == "--api-key")
        .expect("--api-key");
    let token = argv[at + 1];
    assert_eq!(token.len(), 64, "32 random bytes, hex");
    let during = listed(&fx.dir.join("rec").join("clients"));
    assert_eq!(during, [sha_hex(token)], "listed while it ran, and alone");
    assert_eq!(
        listed(&fx.clients()),
        Vec::<String>::new(),
        "withdrawn after"
    );
    assert!(!rr.stdout_tail.contains(token), "the token is never echoed");
}

#[tokio::test]
async fn a_finished_run_leaves_no_token_behind() {
    token_lives_exactly_as_long_as_the_run("gw-done", 0).await;
}

#[tokio::test]
async fn a_failed_run_leaves_no_token_behind() {
    token_lives_exactly_as_long_as_the_run("gw-failed", 3).await;
}

/// Every run gets a token of its own, never one an earlier run had.
#[tokio::test]
async fn each_run_gets_a_new_token() {
    let mut fx = Fixture::new("gw-each", 0);
    let ports = (support::fake_broker(), support::fake_gateway(fx.clients()));
    let gw = fx.gateway(ports, Duration::from_secs(5));
    let token_of = |argv: String| {
        let argv: Vec<String> = argv.lines().map(str::to_owned).collect();
        let at = argv.iter().position(|a| a == "--api-key").unwrap();
        argv[at + 1].clone()
    };

    fx.run(gw.clone()).await;
    let first = token_of(fx.rec("argv").unwrap());
    fx.run(gw).await;
    let second = token_of(fx.rec("argv").unwrap());

    assert_ne!(first, second);
}

/// The gateway re-reads its clients file every 10 s, so a new token is
/// refused until its next read: the run waits for it to be admitted
/// instead of giving up on the first refusal, and its worker runs.
#[tokio::test]
async fn a_run_waits_for_the_gateway_to_admit_its_token() {
    let mut fx = Fixture::new("gw-slow", 0);
    let clients = fx.clients();
    let asked = std::sync::atomic::AtomicUsize::new(0);
    // Not read yet for the first three asks, as before a reload.
    let slow = serve(move |path, bearer| match path {
        "/healthz" => 200,
        "/v1/models" if asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 => 401,
        "/v1/models" => match bearer {
            Some(token) if listed(&clients).contains(&sha_hex(token)) => 200,
            _ => 401,
        },
        _ => 404,
    });
    let gw = fx.gateway((support::fake_broker(), slow), Duration::from_secs(5));

    let rr = fx.run(gw).await;

    assert_eq!(rr.exit_code, Some(0), "{rr:?}");
    assert!(fx.rec("argv").is_some(), "the worker ran");
}

/// A gateway that does not admit the run's token in time: no worker is
/// spawned, the run ends `auth-unavailable` saying why, and the token is
/// withdrawn.
#[tokio::test]
async fn a_token_the_gateway_does_not_admit_spawns_no_worker() {
    let mut fx = Fixture::new("gw-deaf", 0);
    let ports = (support::fake_broker(), deaf_gateway());
    let gw = fx.gateway(ports, Duration::from_millis(300));

    let rr = fx.run(gw).await;

    assert_eq!(
        rr.killed_reason.as_deref(),
        Some("auth-unavailable"),
        "{rr:?}"
    );
    assert!(
        rr.stdout_tail.contains("did not admit"),
        "the reason is the operator's to read: {rr:?}"
    );
    assert_eq!(fx.rec("argv"), None, "no worker ran");
    assert_eq!(listed(&fx.clients()), Vec::<String>::new());
}

/// The health check the scheduler holds work on: the broker and the
/// gateway must both be our own running children and answer their health
/// checks. Ports that answer while none of ours runs are another
/// process's; the reason names the part that is down.
#[test]
fn health_needs_our_running_children_and_their_ports_answering() {
    let dir = TempDir::new("gw-health");
    let (omp, _log) = child_script(&dir, true);
    let (up_broker, up_gateway) = (support::fake_broker(), deaf_gateway());
    let with = |broker, gateway| {
        let mut cfg = support::test_config(dir.path());
        cfg.work_root = dir.subdir("work_root");
        cfg.omp_bin.clone_from(&omp);
        cfg.auth_gateway.broker_port = broker;
        cfg.auth_gateway.gateway_port = gateway;
        let gw = AuthGateway::new(&cfg).with_child_settle(Duration::ZERO);
        (cfg, gw)
    };

    let (cfg, gw) = with(up_broker, up_gateway);
    assert_eq!(
        gw.health().as_deref(),
        Some("auth: broker is not running"),
        "answering ports, but no child of ours"
    );
    let services = AuthServices::start(&cfg, &gw).unwrap();
    until(|| gw.health().is_none());
    drop(services);
    assert_eq!(gw.health().as_deref(), Some("auth: broker is not running"));

    for (broker, gateway, what) in [
        (dead_port(), up_gateway, "broker"),
        (up_broker, dead_port(), "gateway"),
    ] {
        let (cfg, gw) = with(broker, gateway);
        let services = AuthServices::start(&cfg, &gw).unwrap();
        until(|| gw.health().is_some_and(|why| why.contains("unreachable")));
        let why = gw.health().unwrap();
        assert!(
            why.starts_with(&format!("auth: {what} on 127.0.0.1:")),
            "{why}"
        );
        drop(services);
    }
}

/// A fake `omp` for the daemon's children: logs each start's argv and
/// pid, then stays up (`sleep`) or exits at once.
fn child_script(dir: &TempDir, stays_up: bool) -> (String, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("starts.log");
    let omp = dir.join("omp-children");
    let tail = if stays_up { "exec sleep 60" } else { "exit 1" };
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\necho \"$$ $* broker=$OMP_AUTH_BROKER_URL\" >> '{}'\n{tail}\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();
    (omp.to_string_lossy().into_owned(), log)
}

fn starts(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Wait up to 10 s for `done`.
fn until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Daemon start empties a clients file a crashed daemon left behind,
/// starts the broker and the gateway on their ports, and stopping them
/// leaves neither running.
#[test]
fn start_empties_a_stale_clients_file_and_stop_ends_both_children() {
    let dir = TempDir::new("gw-start");
    let (omp, log) = child_script(&dir, true);
    let mut cfg = support::test_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.omp_bin = omp;
    cfg.auth_gateway.broker_port = dead_port();
    cfg.auth_gateway.gateway_port = dead_port();
    let clients = AuthGateway::clients_file(&cfg.work_root);
    std::fs::create_dir_all(clients.parent().unwrap()).unwrap();
    std::fs::write(
        &clients,
        format!(
            r#"{{"job-1-0":{{"tokenSha256":"{}","models":["anthropic/*"]}}}}"#,
            sha_hex("stale")
        ),
    )
    .unwrap();
    let gw = AuthGateway::new(&cfg);

    let services = AuthServices::start(&cfg, &gw).unwrap();

    assert_eq!(
        listed(&clients),
        Vec::<String>::new(),
        "the stale token is gone"
    );
    until(|| starts(&log).len() == 2);
    let started = starts(&log);
    let broker = started.iter().find(|l| l.contains("auth-broker")).unwrap();
    let gateway = started.iter().find(|l| l.contains("auth-gateway")).unwrap();
    assert!(
        broker.contains(&format!(
            "serve --bind=127.0.0.1:{}",
            cfg.auth_gateway.broker_port
        )),
        "{broker}"
    );
    assert!(
        gateway.contains(&format!(
            "serve --bind=127.0.0.1:{} --clients={} broker=http://127.0.0.1:{}",
            cfg.auth_gateway.gateway_port,
            clients.display(),
            cfg.auth_gateway.broker_port
        )),
        "{gateway}"
    );

    drop(services);

    for line in started {
        let pid: i32 = line.split_whitespace().next().unwrap().parse().unwrap();
        assert!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err(),
            "child {pid} still running"
        );
    }
}

/// A child that exits is started again, after a backoff that doubles with
/// every exit (1 s, then 2 s), never in a hot loop; and a stop ends the
/// wait at once instead of sitting out the backoff.
#[test]
fn a_child_that_exits_is_restarted_with_a_growing_backoff() {
    let dir = TempDir::new("gw-restart");
    let (omp, log) = child_script(&dir, false);
    let mut cfg = support::test_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.omp_bin = omp;
    let gw = AuthGateway::new(&cfg);
    let gateway_starts = || {
        starts(&log)
            .iter()
            .filter(|l| l.contains("auth-gateway"))
            .count()
    };

    let services = AuthServices::start(&cfg, &gw).unwrap();
    until(|| gateway_starts() >= 1);
    std::thread::sleep(Duration::from_millis(700));
    assert_eq!(gateway_starts(), 1, "restarted before its 1 s backoff");
    until(|| gateway_starts() >= 2);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(gateway_starts(), 2, "restarted before its 2 s backoff");

    let stopping = Instant::now();
    drop(services);
    assert!(
        stopping.elapsed() < Duration::from_millis(800),
        "the stop sat out the backoff: {:?}",
        stopping.elapsed()
    );
}

/// Our child is trusted with its port only once it has run for a while: a
/// child whose port another process holds exits within its first seconds
/// (`EADDRINUSE`), and until then the port's answer may be that other
/// process's. A child still running past that is the daemon's.
#[test]
fn a_port_counts_as_ours_only_once_our_child_has_settled() {
    let dir = TempDir::new("gw-settle");
    let (omp, _log) = child_script(&dir, true);
    let mut cfg = support::test_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    cfg.omp_bin = omp;
    cfg.auth_gateway.broker_port = support::fake_broker();
    cfg.auth_gateway.gateway_port = deaf_gateway();
    let gw = AuthGateway::new(&cfg).with_child_settle(Duration::from_secs(2));

    let services = AuthServices::start(&cfg, &gw).unwrap();
    let started = Instant::now();
    until(|| gw.health().is_some_and(|why| why.ends_with("is starting")));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "not trusted at once"
    );
    until(|| gw.health().is_none());
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "trusted before it settled"
    );
    drop(services);
}
