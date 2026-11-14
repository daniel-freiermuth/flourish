//! Worker credentials through omp's auth gateway.
//!
//! No worker reads the operator's stored provider login. The daemon runs
//! two children ([`AuthServices`]): `omp auth-broker serve`, the only
//! process that holds the OAuth refresh tokens (it reads the operator's
//! `agent.db`), and `omp auth-gateway serve` in front of it, which
//! forwards a worker's provider calls with the broker's credentials. Each
//! worker run gets a token of its own ([`ClientToken`]): 32 random bytes,
//! admitted because its SHA-256 is listed in the gateway's clients file
//! (`<work_root>/gateway/clients.json`), and taken out of that file when
//! the run ends, however it ends. The gateway re-reads the file every
//! 10 s and refuses a token that is gone with a 401 on its next request,
//! so a token is good for its own run and never past its job.
//!
//! A worker reaches the gateway through `ANTHROPIC_BASE_URL` and the omp
//! flag `--api-key <token>` (measured against omp 18.12.3). The flag is
//! what makes it stick: omp prefers a runtime `--api-key` over a stored
//! OAuth login, but a stored login over an `ANTHROPIC_API_KEY` in the
//! environment, so the variable alone would quietly keep using the
//! operator's credentials. A refused token fails the worker's calls; it
//! does not fall back to the local login for the provider.
//!
//! Anthropic only. No route through the gateway has been verified for
//! `openai-codex` or `github-copilot`, so for those the daemon runs
//! neither child and their workers use the local login as before.
//!
//! When the broker or the gateway does not answer, no job that would run
//! a worker starts: the scheduler holds it as it holds a job the budget
//! denies ([`crate::backend::Backend::auth_unavailable`]), and a run whose
//! token the gateway does not admit in time ends as [`AUTH_UNAVAILABLE`]
//! without spawning a worker, which the scheduler holds the same way.
//!
//! The ports only count as the daemon's while its own children run on
//! them ([`AuthGateway::health`]): a second daemon with the same ports, or
//! an orphaned gateway, answers health checks but reads another clients
//! file and would refuse every token. A child whose port is taken exits
//! at once (`EADDRINUSE`), and the hold then says the port is taken by
//! another process. Each child gets `PR_SET_PDEATHSIG`, so a daemon that
//! dies without stopping it leaves no orphan behind.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::harness::WorkerAuth;
use crate::backend::AUTH_UNAVAILABLE;
use crate::config::Config;
use crate::types::RunResult;
use crate::util::kill_tree;

/// The variable that points an Anthropic worker at the gateway.
const BASE_URL_ENV: &str = "ANTHROPIC_BASE_URL";

/// The models a worker's token is granted: its provider's, all of them.
const GRANT: &str = "anthropic/*";

/// How long a run waits for the gateway to admit its token. The gateway
/// re-reads its clients file every 10 s, so twice that is a gateway that
/// is not reading it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How often a run asks whether its token is admitted yet.
const READY_POLL: Duration = Duration::from_millis(250);

/// Connect, send and first-line bound of one loopback request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// How often a supervisor looks at its child and at the stop flag.
const TICK: Duration = Duration::from_millis(200);

/// Restart backoff of a child that exited: doubled per exit, up to the
/// maximum, and back to the minimum after a run of [`STABLE_RUN`].
const RESTART_MIN: Duration = Duration::from_secs(1);
const RESTART_MAX: Duration = Duration::from_mins(1);
const STABLE_RUN: Duration = Duration::from_mins(1);

/// How long a child must have run before what answers on its port counts
/// as its own. Measured against omp 18.12.3: the gateway listens about
/// 3 s after it starts, and one whose port is taken exits by then
/// (`EADDRINUSE`), so an answer before it may be another process's.
const CHILD_SETTLE: Duration = Duration::from_secs(5);

/// Output lines kept of a child, to tell why it exited.
const TAIL_LINES: usize = 20;

/// The daemon's view of its auth gateway: where the broker and the
/// gateway listen, and the clients file with every live run's token.
pub struct AuthGateway {
    broker_port: u16,
    gateway_port: u16,
    clients: PathBuf,
    ready_timeout: Duration,
    /// Client name -> hex SHA-256 of its token: exactly what the clients
    /// file lists, rewritten whole on every change under this lock.
    issued: Mutex<BTreeMap<String, String>>,
    /// Makes every client name unique: a duplicate name rejects the whole
    /// file, every other run's token with it.
    next: AtomicU64,
    /// What the supervisors know of the broker and the gateway they run
    /// ([`AuthServices`]); a child no supervisor started never runs.
    broker_child: Arc<Mutex<ChildState>>,
    gateway_child: Arc<Mutex<ChildState>>,
    child_settle: Duration,
}

impl AuthGateway {
    pub fn new(cfg: &Config) -> Self {
        Self {
            broker_port: cfg.auth_gateway.broker_port,
            gateway_port: cfg.auth_gateway.gateway_port,
            clients: Self::clients_file(&cfg.work_root),
            ready_timeout: READY_TIMEOUT,
            issued: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(0),
            broker_child: Arc::default(),
            gateway_child: Arc::default(),
            child_settle: CHILD_SETTLE,
        }
    }

    /// How long a run waits for its token to be admitted, in place of
    /// [`READY_TIMEOUT`]: a test's gateway that never admits one need not
    /// take 20 s to say so.
    #[must_use]
    pub const fn with_ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    /// How long a child must have run before its port counts as its own,
    /// in place of [`CHILD_SETTLE`]: a test's fake child need not wait.
    #[must_use]
    pub const fn with_child_settle(mut self, settle: Duration) -> Self {
        self.child_settle = settle;
        self
    }

    /// `<work_root>/gateway/clients.json`.
    pub fn clients_file(work_root: &Path) -> PathBuf {
        work_root.join("gateway").join("clients.json")
    }

    /// The broker's URL, for the gateway's `OMP_AUTH_BROKER_URL`.
    pub fn broker_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.broker_port)
    }

    /// The gateway's URL, a worker's provider endpoint.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.gateway_port)
    }

    fn issued(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.issued.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Empty the clients file, making its directory (0700) if needed. At
    /// daemon start: a token a crashed daemon left listed belongs to no
    /// run, and must not stay good.
    pub fn reset(&self) -> io::Result<()> {
        let mut issued = self.issued();
        issued.clear();
        if let Some(dir) = self.clients.parent() {
            use std::os::unix::fs::PermissionsExt;
            fs::create_dir_all(dir)?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        self.write(&issued)
    }

    /// Replace the clients file with `issued`, atomically: the gateway
    /// rejects a file it cannot parse whole, every listed token with it,
    /// so it must never read one half written. 0600: the digests are not
    /// secrets, but nobody else has business reading who runs.
    fn write(&self, issued: &BTreeMap<String, String>) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let clients: serde_json::Map<String, serde_json::Value> = issued
            .iter()
            .map(|(name, digest)| {
                (
                    name.clone(),
                    serde_json::json!({ "tokenSha256": digest, "models": [GRANT] }),
                )
            })
            .collect();
        let tmp = self.clients.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(serde_json::Value::Object(clients).to_string().as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, &self.clients)
    }

    /// A new token for one run of chain `chain`, listed in the clients
    /// file. Withdrawn when the returned [`ClientToken`] drops.
    pub fn issue(self: &Arc<Self>, chain: i64) -> io::Result<ClientToken> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes)
            .map_err(|e| io::Error::other(format!("no randomness for a gateway token: {e}")))?;
        let token = crate::auth::hex(&bytes);
        let name = format!("job-{chain}-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let mut issued = self.issued();
        issued.insert(
            name.clone(),
            crate::auth::hex(&Sha256::digest(token.as_bytes())),
        );
        if let Err(e) = self.write(&issued) {
            issued.remove(&name);
            return Err(e);
        }
        drop(issued);
        Ok(ClientToken {
            gateway: Arc::clone(self),
            name,
            token,
        })
    }

    /// A token for one run of chain `chain` that the gateway admits:
    /// issued, then asked about until the gateway has read it. `Err` says
    /// why not, and the token is withdrawn again.
    pub fn admit(self: &Arc<Self>, chain: i64) -> Result<ClientToken, String> {
        let token = self
            .issue(chain)
            .map_err(|e| format!("auth: cannot list a gateway token: {e}"))?;
        token.await_ready()?;
        Ok(token)
    }

    /// Why a worker cannot reach its provider through the gateway right
    /// now, or `None` when the broker and the gateway are both the
    /// daemon's own children ([`AuthServices`]), running for a few seconds
    /// at least, and answer their health checks. A port that answers while
    /// no child of ours runs is another process's (a second daemon, an
    /// orphan), whose gateway would refuse every token we list. Blocking.
    pub fn health(&self) -> Option<String> {
        [
            (
                "broker",
                &self.broker_child,
                self.broker_port,
                "/v1/healthz",
            ),
            (
                "gateway",
                &self.gateway_child,
                self.gateway_port,
                "/healthz",
            ),
        ]
        .into_iter()
        .find_map(|(what, child, port, path)| {
            if let Some(why) = lock(child).down(what, port, self.child_settle) {
                return Some(why);
            }
            match http_status(port, path, None) {
                Ok(200) => None,
                Ok(code) => Some(format!(
                    "auth: {what} on 127.0.0.1:{port} answered {code} to {path}"
                )),
                Err(e) => Some(format!(
                    "auth: {what} on 127.0.0.1:{port} unreachable ({e})"
                )),
            }
        })
    }

    /// [`Self::health`], asked until it is `None` or `timeout` has passed;
    /// the last answer. Blocking.
    pub fn await_healthy(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let why = self.health()?;
            if Instant::now() >= deadline {
                return Some(why);
            }
            std::thread::sleep(READY_POLL);
        }
    }
}

/// One run's gateway token, listed in the clients file for as long as
/// this lives. Dropping it, on whichever path the run ends (a panic
/// included), takes the token out of the file. No `Debug`: nothing may
/// print the token.
pub struct ClientToken {
    gateway: Arc<AuthGateway>,
    name: String,
    token: String,
}

impl ClientToken {
    /// How a worker presents this token ([`super::harness::run_worker`]).
    pub fn worker_auth(&self) -> WorkerAuth<'_> {
        WorkerAuth {
            base_url_env: BASE_URL_ENV,
            base_url: self.gateway.base_url(),
            api_key: &self.token,
        }
    }

    /// Wait until the gateway admits the token: `GET /v1/models` with it
    /// answers 200. Until the gateway's next read of the clients file it
    /// answers 401, and a worker spawned then would fail its first call.
    fn await_ready(&self) -> Result<(), String> {
        let port = self.gateway.gateway_port;
        let deadline = Instant::now() + self.gateway.ready_timeout;
        loop {
            let last = match http_status(port, "/v1/models", Some(&self.token)) {
                Ok(200) => return Ok(()),
                Ok(code) => format!("answered {code}"),
                Err(e) => e.to_string(),
            };
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "auth: gateway on 127.0.0.1:{port} did not admit {} within {:.1}s ({last})",
                    self.name,
                    self.gateway.ready_timeout.as_secs_f64()
                ));
            }
            std::thread::sleep(READY_POLL.min(deadline - now));
        }
    }
}

impl Drop for ClientToken {
    fn drop(&mut self) {
        let mut issued = self.gateway.issued();
        issued.remove(&self.name);
        // Left listed only until the next successful write (any other
        // run's token, or the next daemon start): the map no longer has
        // it, and every write lists the map whole.
        if let Err(e) = self.gateway.write(&issued) {
            tracing::error!(
                "auth gateway: cannot withdraw token {} from {}: {e}",
                self.name,
                self.gateway.clients.display()
            );
        }
    }
}

/// The result of a run that never spawned its worker because the gateway
/// did not admit its token: `why` is the operator's to read, and the
/// scheduler holds the job ([`AUTH_UNAVAILABLE`]).
pub fn refused(why: &str) -> RunResult {
    RunResult {
        exit_code: None,
        killed_reason: Some(AUTH_UNAVAILABLE.to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: why.to_owned(),
        usage_delta: None,
    }
}

/// The status code `GET <path>` on loopback `port` answers, with `bearer`
/// as its `Authorization` if given. Only the status line is read. The
/// daemon has no HTTP client, and these are three fixed requests to its
/// own children. Blocking.
fn http_status(port: u16, path: &str, bearer: Option<&str>) -> io::Result<u16> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, HTTP_TIMEOUT)?;
    stream.set_read_timeout(Some(HTTP_TIMEOUT))?;
    stream.set_write_timeout(Some(HTTP_TIMEOUT))?;
    let auth = bearer.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}Connection: close\r\n\r\n")
            .as_bytes(),
    )?;
    let mut line = String::new();
    BufReader::new(stream.take(1024)).read_line(&mut line)?;
    let mut words = line.split_whitespace();
    words
        .next()
        .filter(|v| v.starts_with("HTTP/"))
        .and_then(|_| words.next())
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not an HTTP response"))
}

// ---------------------------------------------------------------------------
// The daemon's children
// ---------------------------------------------------------------------------

/// One child the daemon keeps running: `omp <args>` with `env` added, and
/// what its supervisor knows of it (`state`, shared with
/// [`AuthGateway::health`]).
struct Service {
    name: &'static str,
    omp_bin: String,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    state: Arc<Mutex<ChildState>>,
}

/// What a supervisor knows of its child, for [`AuthGateway::health`]: a
/// port only counts as the daemon's while its own child runs.
#[derive(Default)]
struct ChildState {
    /// Since when the current child runs; `None` while none does.
    running_since: Option<Instant>,
    /// The current child's last output lines.
    tail: VecDeque<String>,
    /// The last exited child's last output lines.
    exit_tail: VecDeque<String>,
}

impl ChildState {
    /// Why `what`'s child on `port` does not serve, or `None` when it has
    /// run for `settle`: a child whose port is taken exits within its
    /// first seconds, and until then a port that answers may be another
    /// process's.
    fn down(&self, what: &str, port: u16, settle: Duration) -> Option<String> {
        let taken = || {
            self.exit_tail
                .iter()
                .any(|l| l.contains("EADDRINUSE") || l.contains(" in use"))
        };
        match self.running_since {
            Some(since) if since.elapsed() >= settle => None,
            _ if taken() => Some(format!(
                "auth: {what} port {port} is taken by another process"
            )),
            Some(_) => Some(format!("auth: {what} is starting")),
            None => Some(match self.exit_tail.back() {
                Some(last) => format!("auth: {what} is not running (last output: {last})"),
                None => format!("auth: {what} is not running"),
            }),
        }
    }
}

/// The auth broker and the auth gateway, each run and restarted by a
/// supervisor thread of its own until this drops. Dropping it stops both
/// and waits for them, which the daemon does only once its job lane has
/// finished the run in progress: that run's worker talks to the gateway
/// until it ends.
pub struct AuthServices {
    /// One per supervisor, never sent on: dropping it is the stop.
    stops: Vec<mpsc::Sender<()>>,
    supervisors: Vec<JoinHandle<()>>,
}

impl AuthServices {
    /// Empty `gateway`'s clients file ([`AuthGateway::reset`]), then start
    /// `omp auth-broker serve` and `omp auth-gateway serve --clients=<it>`
    /// on their configured loopback ports. An error is a clients file that
    /// cannot be emptied or a thread that cannot start: the daemon does
    /// not start without either.
    pub fn start(cfg: &Config, gateway: &AuthGateway) -> io::Result<Self> {
        gateway.reset()?;
        let services = [
            Service {
                name: "broker",
                omp_bin: cfg.omp_bin.clone(),
                args: vec![
                    "auth-broker".to_owned(),
                    "serve".to_owned(),
                    format!("--bind=127.0.0.1:{}", gateway.broker_port),
                ],
                env: Vec::new(),
                state: Arc::clone(&gateway.broker_child),
            },
            Service {
                name: "gateway",
                omp_bin: cfg.omp_bin.clone(),
                args: vec![
                    "auth-gateway".to_owned(),
                    "serve".to_owned(),
                    format!("--bind=127.0.0.1:{}", gateway.gateway_port),
                    format!("--clients={}", gateway.clients.display()),
                ],
                env: vec![("OMP_AUTH_BROKER_URL", gateway.broker_url())],
                state: Arc::clone(&gateway.gateway_child),
            },
        ];
        let mut started = Self {
            stops: Vec::new(),
            supervisors: Vec::new(),
        };
        for service in services {
            let (stop, stopped) = mpsc::channel();
            started.stops.push(stop);
            // On error `started` drops, stopping what already runs.
            let handle = std::thread::Builder::new()
                .name(format!("auth-{}", service.name))
                .spawn(move || supervise(&service, &Stop(stopped)))?;
            started.supervisors.push(handle);
        }
        Ok(started)
    }
}

impl Drop for AuthServices {
    fn drop(&mut self) {
        self.stops.clear();
        for supervisor in self.supervisors.drain(..) {
            if supervisor.join().is_err() {
                tracing::error!("auth: a supervisor thread panicked");
            }
        }
    }
}

/// A supervisor's end of its stop: [`AuthServices`] dropped the sender.
struct Stop(mpsc::Receiver<()>);

impl Stop {
    /// Wait up to `timeout`; whether the supervisor is to stop, which
    /// ends the wait at once.
    fn wait(&self, timeout: Duration) -> bool {
        matches!(
            self.0.recv_timeout(timeout),
            Err(mpsc::RecvTimeoutError::Disconnected)
        )
    }
}

/// Run `service` until `stop`, restarting it whenever it exits with a
/// doubling backoff ([`RESTART_MIN`]..[`RESTART_MAX`]). On `stop` the
/// child's process group is terminated ([`kill_tree`]).
fn supervise(service: &Service, stop: &Stop) {
    let name = service.name;
    let mut backoff = RESTART_MIN;
    loop {
        let started = Instant::now();
        let mut cmd = Command::new(&service.omp_bin);
        cmd.args(&service.args)
            .envs(service.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own group: `kill_tree` stops all of it, and a Ctrl-C to the
        // daemon's terminal does not take the gateway down under a
        // worker the daemon is still letting finish.
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
            // A daemon that dies without stopping it (SIGKILL, a crash)
            // takes it along: the kernel sends SIGTERM when the thread that
            // spawned it ends, and this supervisor thread outlives its child
            // on every other path. Without it, an orphaned gateway would
            // hold the port the next daemon's child needs.
            #[allow(
                unsafe_code,
                reason = "pre_exec is the only way to set PR_SET_PDEATHSIG in the child"
            )]
            // SAFETY: the closure runs between fork and exec and only calls
            // prctl(2), which is async-signal-safe and touches no memory of
            // the parent.
            unsafe {
                cmd.pre_exec(|| {
                    nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGTERM)
                        .map_err(io::Error::from)
                });
            }
        }
        match cmd.spawn() {
            Ok(mut child) => {
                tracing::info!("auth {name}: started (pid {})", child.id());
                {
                    let mut state = lock(&service.state);
                    state.running_since = Some(Instant::now());
                    state.tail.clear();
                }
                let logs = [
                    log_lines(name, child.stdout.take(), Arc::clone(&service.state)),
                    log_lines(name, child.stderr.take(), Arc::clone(&service.state)),
                ];
                let exited = loop {
                    match child.try_wait() {
                        Ok(None) if stop.wait(TICK) => break None,
                        Ok(None) => {}
                        Ok(Some(status)) => break Some(status.to_string()),
                        Err(e) => break Some(format!("cannot be waited on: {e}")),
                    }
                };
                // Also after an exit of its own: a descendant left behind
                // would hold the pipes the log threads read to the end.
                kill_tree(&mut child);
                for log in logs {
                    let _ = log.join();
                }
                {
                    let mut state = lock(&service.state);
                    state.running_since = None;
                    state.exit_tail = std::mem::take(&mut state.tail);
                }
                let Some(exited) = exited else {
                    tracing::info!("auth {name}: stopped");
                    return;
                };
                if started.elapsed() >= STABLE_RUN {
                    backoff = RESTART_MIN;
                }
                tracing::warn!(
                    "auth {name}: exited ({exited}); restarting in {}s",
                    backoff.as_secs()
                );
            }
            Err(e) => tracing::error!(
                "auth {name}: cannot start {}: {e}; retrying in {}s",
                service.omp_bin,
                backoff.as_secs()
            ),
        }
        if stop.wait(backoff) {
            tracing::info!("auth {name}: stopped");
            return;
        }
        backoff = (backoff * 2).min(RESTART_MAX);
    }
}

/// Copy a child's output into the log, a line at a time, until the pipe
/// closes, keeping the last [`TAIL_LINES`] in `state` for the health
/// check to read why it exited.
fn log_lines<R: Read + Send + 'static>(
    name: &'static str,
    pipe: Option<R>,
    state: Arc<Mutex<ChildState>>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(pipe) = pipe else {
            return;
        };
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        while matches!(reader.read_until(b'\n', &mut line), Ok(n) if n > 0) {
            let text = String::from_utf8_lossy(&line).trim_end().to_owned();
            tracing::info!("auth {name}: {text}");
            let mut state = lock(&state);
            if state.tail.len() == TAIL_LINES {
                state.tail.pop_front();
            }
            state.tail.push_back(text);
            drop(state);
            line.clear();
        }
    })
}

/// `m`'s guard, poisoned or not: every holder leaves it consistent.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
