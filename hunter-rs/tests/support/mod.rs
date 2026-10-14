//! Shared test support.
//!
//! Integration tests in `tests/` are separate crates, so each one `mod
//! support;`s this file. Everything here exists because it was being
//! reinvented per file, or because a behaviour we shipped a bug in was
//! untestable without it.
//!
//! The four pieces:
//! - [`TempDir`] — scratch directory that cleans up on drop, *including*
//!   when a test panics. The hand-rolled `let _ = fs::remove_dir_all(..)`
//!   at the end of a test body leaks on every failure.
//! - [`fresh_store`] / [`fresh_pool`] — a writable database from the
//!   schema-only `dev.db` that `build.rs` generates.
//! - [`FakeBins`] — scripted `gh` / `glab` / any binary on `PATH`, with an
//!   invocation log. The forge is constructed internally by the runners
//!   (`forge::forge_for(repo.forge)`), so there is no trait seam to inject:
//!   intercepting the subprocess is how you control forge behaviour, and it
//!   has the side benefit of exercising the real argv construction.
//! - [`ScriptedBackend`] — a `Backend` whose `run()` is a closure over the
//!   worktree, so a test can simulate exactly what a worker leaves behind
//!   (a `WITHDRAW.md`, a `findings.json`, nothing at all).
#![allow(unsafe_code)] // env::set_var for PATH interception; tests only
#![allow(dead_code)] // each test crate uses a different subset
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use hunter::backend::JobClass;
use hunter::backend::{Backend, Outlook, Verdict};
use hunter::store::Store;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use sqlx::sqlite::SqliteConnectOptions;

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A scratch directory removed on drop, panic or not.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "hunter-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }

    /// Create a subdirectory and return it.
    pub fn subdir(&self, rel: impl AsRef<Path>) -> PathBuf {
        let p = self.join(rel);
        std::fs::create_dir_all(&p).expect("create subdir");
        p
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Databases
// ---------------------------------------------------------------------------

/// `Config::load` over `root`, with the network gate off.
///
/// No test may reach the network, and the gate would probe the forge of
/// any repo a cycle picks: the fixtures' `https://github.com/acme/widget`
/// included. A test of the gate itself turns it back on.
pub fn load_config(root: &Path) -> hunter::config::Config {
    let mut cfg = hunter::config::Config::load(root).expect("load config");
    cfg.network_gate = false;
    cfg
}

/// Path to the schema-only `dev.db` that `build.rs` regenerates from
/// `migrations/`. Never opened directly — always copied first.
fn dev_db() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dev.db")
}

/// A writable copy of `dev.db` (schema complete, zero rows).
pub fn db_copy(dir: &TempDir, name: &str) -> PathBuf {
    let dst = dir.join(format!("{name}.db"));
    std::fs::copy(dev_db(), &dst).expect("copy dev.db — run `just dev-db` if missing");
    dst
}

/// Raw pool over a fresh database copy, for seeding fixtures.
pub async fn fresh_pool(dir: &TempDir, name: &str) -> (PathBuf, SqlitePool) {
    let path = db_copy(dir, name);
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(&path))
        .await
        .expect("open pool");
    (path, pool)
}

/// The read-write `Store` the daemon uses, over a fresh database copy.
pub async fn fresh_store(dir: &TempDir, name: &str) -> (PathBuf, Store) {
    let path = db_copy(dir, name);
    let store = Store::connect(&path).await.expect("connect store");
    (path, store)
}

/// Put finding `id` at `status` with a raw UPDATE on the database at
/// `db`, for fixtures that need any status -- `fixing` included, which
/// `Store::set_finding_status` refuses because only an owning
/// `CycleScope::claim_fix` may take that claim. The status triggers fire
/// as for any write.
pub async fn seed_status(db: &Path, id: i64, status: hunter::domain::FindingStatus) {
    let pool = SqlitePool::connect_with(SqliteConnectOptions::new().filename(db))
        .await
        .expect("open pool");
    sqlx::query("UPDATE findings SET status = ?1 WHERE id = ?2")
        .bind(status.as_str())
        .bind(id)
        .execute(&pool)
        .await
        .expect("seed status");
    pool.close().await;
}

/// `store`'s proof of recovery, spending its startup token as "nothing
/// to reconcile" the first time: a fixture's store was made by the test,
/// so no dead process left anything in it. A test of the startup
/// reconcile takes the token itself instead
/// ([`Store::take_unreconciled`]).
pub fn recovered(store: &Store) -> hunter::recovery::Recovered {
    store.recovered().unwrap_or_else(|| {
        store
            .take_unreconciled()
            .expect("the store is mid-reconcile")
            .assume_clean()
    })
}

/// The authority for jobs a test creates or runners it calls directly
/// ([`hunter::recovery::CycleScope`]). What the runner abandons is owed
/// to the store's ledger and settled only by a cycle, so a test asserts
/// what the runner itself left. A fixture job that stands for one a live
/// owner still runs is kept out of the ledger with
/// [`hunter::recovery::RunningJob::disown`] ([`live_job`]).
pub fn scope(store: &Store) -> hunter::recovery::CycleScope {
    hunter::recovery::CycleScope::open(&recovered(store))
}

/// The id of a fixture job left `running` for a live owner: created, and
/// disowned so no settle takes it.
pub fn live_job(created: hunter::recovery::CreatedJob) -> i64 {
    created.job.disown()
}

// ---------------------------------------------------------------------------
// Fake executables on PATH
// ---------------------------------------------------------------------------

/// Fail loudly if this suite is run without per-test process isolation.
///
/// `FakeBins` mutates `PATH` and the environment, which are process-global.
/// Under `cargo nextest run` each test is its own process, so that is
/// private and needs no coordination. Under plain `cargo test` the tests
/// are threads in one process and would silently race — one test's `PATH`
/// landing in another's subprocess, intermittently, usually on CI.
///
/// This crate is nextest-only precisely so that the coordination can be
/// deleted rather than maintained. Asserting on the execution mode rather
/// than merely on `NEXTEST` checks the guarantee we actually depend on.
fn require_process_isolation() {
    assert_eq!(
        std::env::var("NEXTEST_EXECUTION_MODE").ok().as_deref(),
        Some("process-per-test"),
        "these tests mutate PATH and the environment and require per-test \
         process isolation; run them with `cargo nextest run` (plain \
         `cargo test` would race silently)"
    );
}

/// Scripted executables shadowing the real ones for the duration of a test.
///
/// The original `PATH` is appended, so binaries you do *not* script
/// (notably `git`) still resolve normally.
pub struct FakeBins {
    dir: TempDir,
    log: PathBuf,
    original_path: String,
}

impl FakeBins {
    pub fn acquire(label: &str) -> Self {
        require_process_isolation();
        let dir = TempDir::new(&format!("bin-{label}"));
        let log = dir.join("invocations.log");
        std::fs::write(&log, "").expect("create invocation log");
        let original_path = std::env::var("PATH").unwrap_or_default();
        let combined = format!("{}:{original_path}", dir.path().display());
        // SAFETY: this process runs exactly one test (asserted above), so
        // nothing else here reads or writes the environment. Restored in
        // `Drop` regardless, so the assertion is not load-bearing for
        // correctness within the test itself.
        unsafe { std::env::set_var("PATH", &combined) };
        Self {
            dir,
            log,
            original_path,
        }
    }

    /// Scope an environment variable for the lifetime of the returned
    /// guard.
    ///
    /// Hangs off `FakeBins` so the process-isolation assertion in
    /// `acquire` covers environment mutation too, not just `PATH`.
    ///
    /// Needed because some seams are only reachable through the
    /// environment — a worker that ignored `--session-dir` would fall
    /// back to `$OMP_HOME`, and redirecting that at a `TempDir` has no
    /// other route than the process environment; without it such a
    /// worker writes into the real `~/.omp/agent/sessions` that a live
    /// daemon is using.
    #[allow(
        clippy::unused_self,
        reason = "the receiver is the point: taking `&FakeBins` forces the \
                  caller through `acquire`, whose process-isolation \
                  assertion is what makes mutating the process environment \
                  safe. An associated function would let a test skip it"
    )]
    pub fn env(&self, key: &'static str, value: &Path) -> EnvGuard {
        let previous = std::env::var(key).ok();
        // SAFETY: one test per process; restored on drop.
        unsafe { std::env::set_var(key, value) };
        EnvGuard { key, previous }
    }

    /// Remove an environment variable for the lifetime of the returned
    /// guard: for tests about what the daemon does when the operator did
    /// NOT export something that the test runner itself may have.
    #[allow(
        clippy::unused_self,
        reason = "same as `env`: the receiver forces the caller through \
                  `acquire` and its process-isolation assertion"
    )]
    pub fn unset_env(&self, key: &'static str) -> EnvGuard {
        let previous = std::env::var(key).ok();
        // SAFETY: one test per process; restored on drop.
        unsafe { std::env::remove_var(key) };
        EnvGuard { key, previous }
    }

    /// Drop the real `PATH`, leaving only the scripted directory.
    ///
    /// `acquire` appends the original `PATH` so unscripted binaries (git)
    /// still resolve. Use this when the point of the test is that a binary
    /// is ABSENT — otherwise a developer's real `npx`/`gh` answers instead.
    pub fn isolate(&self) {
        // SAFETY: one test per process; restored in `Drop`.
        unsafe { std::env::set_var("PATH", self.dir.path()) };
    }

    /// Install `name` as an executable running `body` (POSIX sh). The script
    /// appends its argv to the invocation log before running `body`, one
    /// line per call with tab-separated arguments. Backslashes, tabs and
    /// newlines inside an argument are escaped (`\\`, `\t`, `\n`) and
    /// [`Self::calls`] undoes that, so a multi-line `--body` is recorded
    /// as the one argument it was, not split into bogus calls.
    pub fn script(&self, name: &str, body: &str) {
        let path = self.dir.join(name);
        // `x` survives command substitution's trailing-newline strip.
        let escape = r#"e=$(printf '%sx' "$a" | sed -e ':a' -e '$!N' -e '$!ba' -e 's/\\/\\\\/g' -e 's/\t/\\t/g' -e 's/\n/\\n/g')"#;
        let contents = format!(
            "#!/bin/sh\nprintf '%s' \"{name}\" >> '{log}'\nfor a in \"$@\"; do {escape}; printf '\\t%s' \"${{e%x}}\" >> '{log}'; done\nprintf '\\n' >> '{log}'\n{body}\n",
            name = name,
            log = self.log.display(),
        );
        std::fs::write(&path, contents).expect("write fake binary");
        set_executable(&path);
    }

    /// Succeed, printing `stdout`.
    pub fn ok(&self, name: &str, stdout: &str) {
        self.script(
            name,
            &format!("cat <<'__FAKE_EOF__'\n{stdout}\n__FAKE_EOF__\nexit 0"),
        );
    }

    /// Fail with `code`, printing `stderr` on fd 2.
    pub fn fail(&self, name: &str, code: i32, stderr: &str) {
        self.script(
            name,
            &format!("cat >&2 <<'__FAKE_EOF__'\n{stderr}\n__FAKE_EOF__\nexit {code}"),
        );
    }

    /// Succeed for every invocation except the one whose leading arguments
    /// are `action`, which fails.
    ///
    /// Matches on the subcommand, not "any argument contains", because
    /// `gh pr view --json title,body,comments,...` contains the word
    /// "comment" in its field list and a substring match fails the wrong
    /// call.
    pub fn ok_unless_action(&self, name: &str, action: &str, stdout: &str) {
        let words: Vec<&str> = action.split_whitespace().collect();
        let n = words.len();
        let joined = words.join(" ");
        self.script(
            name,
            &format!(
                "got=\"\"\nfor a in \"$@\"; do case \"$a\" in -*) break;; esac; got=\"${{got:+$got }}$a\"; n=$(echo \"$got\" | wc -w); [ \"$n\" -ge {n} ] && break; done\nif [ \"$got\" = \"{joined}\" ]; then echo \"fake {name}: refusing {joined}\" >&2; exit 1; fi\ncat <<'__FAKE_EOF__'\n{stdout}\n__FAKE_EOF__\nexit 0"
            ),
        );
    }

    /// Every recorded invocation, as argv vectors (`[name, arg, arg, ...]`).
    pub fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.split('\t').map(unescape_arg).collect())
            .collect()
    }

    /// Invocations of `name` only.
    pub fn calls_to(&self, name: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|c| c.first().is_some_and(|n| n == name))
            .collect()
    }

    /// Whether any invocation of `name` contained `arg`.
    pub fn called_with(&self, name: &str, arg: &str) -> bool {
        self.calls_to(name)
            .iter()
            .any(|c| c.iter().any(|a| a == arg))
    }
}

/// Undo the fake binaries' argument escaping (see [`FakeBins::script`]).
fn unescape_arg(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

impl Drop for FakeBins {
    fn drop(&mut self) {
        // SAFETY: one test per process.
        unsafe { std::env::set_var("PATH", &self.original_path) };
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .expect("stat fake binary")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("chmod fake binary");
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) {}

// ---------------------------------------------------------------------------
// A backend that simulates a worker
// ---------------------------------------------------------------------------

type RunFn = Box<dyn Fn(&Path, Option<&Path>) -> RunResult + Send + Sync>;

/// What one `run()` of a [`ScriptedBackend`] was handed.
#[derive(Debug, Clone)]
pub struct Run {
    pub tree: PathBuf,
    pub prompt: String,
    pub resume_from: Option<PathBuf>,
}

/// A `Backend` whose `run()` is a closure over the chain's worktree
/// (`Workspace::tree`) it is handed.
///
/// Use it to stage exactly what a worker would leave behind. `decide()`
/// always grants, so a test never has to satisfy the budget gate. The
/// headroom it grants is `None` — no token bound — unless
/// [`ScriptedBackend::granting`] says otherwise, or a window too small for
/// large reservations ([`ScriptedBackend::denying_above`]). It reports no
/// worker hosts unless [`ScriptedBackend::needing`] names some, and it
/// answers the reachability probe itself, without a socket: every host is
/// reachable unless [`ScriptedBackend::offline`] marks it down. Every run is
/// recorded ([`ScriptedBackend::runs`]), so a test can assert on the prompt
/// and the transcript a worker was given.
pub struct ScriptedBackend {
    run: RunFn,
    cap_tokens: Option<i64>,
    deny_above: Option<i64>,
    /// `run` returns `Err` instead of a result: the harness itself failed
    /// (a panicked blocking task), not the worker.
    fails: bool,
    hosts: Vec<String>,
    /// The hosts the reachability probe reports unreachable.
    offline: Vec<String>,
    runs: Mutex<Vec<Run>>,
}

impl ScriptedBackend {
    /// Run the given closure against the worktree.
    pub fn new(run: impl Fn(&Path) -> RunResult + Send + Sync + 'static) -> Self {
        Self::staged(move |tree, _| run(tree))
    }

    /// Run the given closure against the worktree and the transcript the
    /// run was asked to continue, if any.
    pub fn staged(run: impl Fn(&Path, Option<&Path>) -> RunResult + Send + Sync + 'static) -> Self {
        Self {
            run: Box::new(run),
            cap_tokens: None,
            deny_above: None,
            fails: false,
            hosts: Vec::new(),
            offline: Vec::new(),
            runs: Mutex::new(Vec::new()),
        }
    }

    /// Deny every reservation above `tokens`: a window with room for some
    /// jobs and not others.
    #[must_use]
    pub fn denying_above(mut self, tokens: i64) -> Self {
        self.deny_above = Some(tokens);
        self
    }

    /// Grant `cap_tokens` as the backend's headroom for every verdict.
    #[must_use]
    pub fn granting(mut self, cap_tokens: Option<i64>) -> Self {
        self.cap_tokens = cap_tokens;
        self
    }

    /// Report `hosts` as the hosts a worker needs (`Backend::worker_hosts`).
    #[must_use]
    pub fn needing(mut self, hosts: &[&str]) -> Self {
        self.hosts = hosts.iter().map(|&h| h.to_owned()).collect();
        self
    }

    /// Answer the reachability probe with `hosts` down
    /// (`Backend::first_unreachable`): the machine is offline for them.
    #[must_use]
    pub fn offline(mut self, hosts: &[&str]) -> Self {
        self.offline = hosts.iter().map(|&h| h.to_owned()).collect();
        self
    }

    /// A worker that writes `contents` to `name` in the worktree and exits 0.
    pub fn writing(name: &'static str, contents: &'static str) -> Self {
        Self::new(move |cwd| {
            std::fs::write(cwd.join(name), contents).expect("stage worker output");
            done()
        })
    }

    /// A worker that does nothing and exits 0.
    pub fn noop() -> Self {
        Self::new(|_| done())
    }

    /// A backend whose `run` fails outright: no worker, no transcript.
    pub fn failing() -> Self {
        Self {
            fails: true,
            ..Self::noop()
        }
    }

    /// Every run so far, oldest first.
    pub fn runs(&self) -> Vec<Run> {
        self.runs.lock().unwrap().clone()
    }
}

/// A successful, cheap `RunResult`.
pub fn done() -> RunResult {
    RunResult {
        exit_code: Some(0),
        killed_reason: None,
        tokens_new: 1_000,
        calls: 1,
        session_file: None,
        duration_s: 0.1,
        stdout_tail: String::new(),
        usage_delta: None,
    }
}

/// A worker in `tree` that finished, leaving a one-record transcript in its
/// workspace's `session/`: an attempt that can be continued.
pub fn done_with_transcript(tree: &Path) -> RunResult {
    let session = tree.parent().unwrap().join("session/session.jsonl");
    std::fs::write(
        &session,
        "{\"message\":{\"role\":\"assistant\",\"usage\":{\"input\":1000,\"output\":100}}}\n",
    )
    .unwrap();
    RunResult {
        session_file: Some(session.to_string_lossy().into_owned()),
        ..done()
    }
}

#[async_trait::async_trait]
impl Backend for ScriptedBackend {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let verdict = if self
            .deny_above
            .is_some_and(|limit| anticipated_tokens > limit)
        {
            Verdict::Denied {
                reason: format!("test: {anticipated_tokens} tok does not fit"),
                retry_at: None,
            }
        } else {
            Verdict::Granted {
                cap_tokens: self.cap_tokens,
                reason: "test: always granted".to_owned(),
            }
        };
        Ok(Outlook {
            normal: verdict.clone(),
            prioritized: verdict,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn worker_hosts(&self) -> Vec<String> {
        self.hosts.clone()
    }

    /// No socket: the first of `hosts` marked [`ScriptedBackend::offline`].
    async fn first_unreachable(&self, hosts: &[String]) -> Option<String> {
        hosts.iter().find(|h| self.offline.contains(h)).cloned()
    }

    async fn run(
        &self,
        ws: &hunter::workspace::Workspace,
        prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        self.runs.lock().unwrap().push(Run {
            tree: ws.tree.clone(),
            prompt: prompt.to_owned(),
            resume_from: resume_from.map(Path::to_path_buf),
        });
        if self.fails {
            anyhow::bail!("test: the backend failed before the worker ran");
        }
        Ok((self.run)(&ws.tree, resume_from))
    }
}

// ---------------------------------------------------------------------------
// Git fixtures
// ---------------------------------------------------------------------------

/// A working repo with a bare `origin` it can fetch from, plus one feature
/// branch published on that origin.
///
/// The runners all start with `git fetch origin <head_ref>` followed by
/// `git worktree add`, so a bare origin is the minimum that lets them run
/// at all. Uses `-c` overrides rather than touching global git config.
pub struct GitRepo {
    pub origin: PathBuf,
    pub work: PathBuf,
    pub default_branch: String,
}

impl GitRepo {
    /// Build `origin` (bare) and `work` (a clone) under `dir`, with an
    /// initial commit on `main` and `branch` pushed to origin.
    pub fn with_branch(dir: &TempDir, branch: &str) -> Self {
        let origin = dir.join("origin.git");
        let work = dir.join("work");
        git(
            dir.path(),
            &[
                "init",
                "--bare",
                "-b",
                "main",
                origin.to_string_lossy().as_ref(),
            ],
        );
        git(
            dir.path(),
            &[
                "clone",
                origin.to_string_lossy().as_ref(),
                work.to_string_lossy().as_ref(),
            ],
        );
        std::fs::write(work.join("README.md"), "seed\n").expect("seed file");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "seed"]);
        git(&work, &["push", "origin", "main"]);
        git(&work, &["checkout", "-b", branch]);
        std::fs::write(work.join("CHANGE.md"), "change\n").expect("branch file");
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-m", "change"]);
        git(&work, &["push", "origin", branch]);
        git(&work, &["checkout", "main"]);
        Self {
            origin,
            work,
            default_branch: "main".to_owned(),
        }
    }
}

/// Run git in `cwd` with identity and hook config forced, so the fixture
/// does not depend on (or disturb) the developer's global git setup.
pub fn git(cwd: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.email=test@hunter.invalid",
            "-c",
            "user.name=hunter test",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}{}",
        cwd.display(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Restores (or removes) an environment variable when dropped.
pub struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: one test per process.
        match &self.previous {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

// ---------------------------------------------------------------------------
// Domain fixtures
// ---------------------------------------------------------------------------

/// A `Repo` with every field populated, for tests that need one but do
/// not care what is in it. `Repo` has no `Default` in production and
/// should not grow one just for tests.
pub fn sample_repo() -> hunter::types::Repo {
    hunter::types::Repo {
        id: 1,
        name: "widget".to_owned(),
        url: "https://github.com/acme/widget".to_owned(),
        path: "/tmp/widget".to_owned(),
        forge: hunter::domain::ForgeName::Github,
        default_branch: "main".to_owned(),
        last_hunt_sha: Some("abc1234".to_owned()),
        last_hunt_at: Some(1),
        enabled: 1,
        added_at: 0,
        last_full_hunt_at: None,
        last_test_gap_at: None,
        last_dep_update_at: None,
        last_refactor_at: None,
        last_modernization_at: None,
        last_standards_at: None,
        full_hunt_requested_at: None,
        full_hunt_request_attempted: None,
    }
}

/// A `Finding` of the given type with every optional field populated.
///
/// Populated rather than `None`-filled on purpose: a prompt builder that
/// reads a field only when it is `Some` would otherwise never exercise
/// that path, and the slot it fills would go unrendered.
pub fn sample_finding(kind: hunter::domain::FindingType) -> hunter::types::Finding {
    hunter::types::Finding {
        id: 42,
        kind,
        repo_id: 1,
        fingerprint: "fp-sample".to_owned(),
        file: Some("src/lib.rs".to_owned()),
        symbol: Some("do_thing".to_owned()),
        line: Some(17),
        severity: hunter::domain::Severity::Medium,
        confidence: 0.8,
        summary: "a summary".to_owned(),
        detail: Some("a detail".to_owned()),
        status: hunter::domain::FindingStatus::New,
        pr_url: Some("https://github.com/acme/widget/pull/7".to_owned()),
        created_at: 0,
        updated_at: 0,
        bug_class: Some(hunter::domain::BugClass::Logic),
        evidence_plan: Some("a plan".to_owned()),
        introduced_by: Some("deadbeef".to_owned()),
        rung_achieved: Some(2),
        verdict_reason: Some("a reason".to_owned()),
        budget_override: None,
        ecosystem: Some("npm".to_owned()),
        package: Some("left-pad".to_owned()),
        current_version: Some("1.2.0".to_owned()),
        latest_version: Some("1.3.0".to_owned()),
        update_type: Some("minor".to_owned()),
        security_advisory: Some("CVE-0000-0000".to_owned()),
        missing_tests: Some("[\"case a\"]".to_owned()),
        test_file: Some("tests/lib_test.rs".to_owned()),
        smell_type: Some("duplication".to_owned()),
        suggested_refactor: Some("extract a helper".to_owned()),
        modernization_class: Some("deprecated-api".to_owned()),
        current_approach: Some("the old way".to_owned()),
        proposed_approach: Some("the new way".to_owned()),
        standard_section: Some("Type safety / Domain types over primitives".to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Logged-in requests
// ---------------------------------------------------------------------------

/// A `Cookie` header value carrying a live session for the test account
/// `tester`, created on first use. Every `/api` route but `/api/login`
/// needs one.
///
/// The account's password hash is a placeholder: nothing here logs in
/// through `/api/login`, and hashing for real would cost each test an
/// argon2 run.
pub async fn auth_cookie(store: &Store) -> String {
    let user = if let Some((user, _)) = store.login_candidate("tester").await.unwrap() {
        user
    } else {
        let id = store.create_user("tester", "unused").await.unwrap();
        hunter::auth::CurrentUser {
            id,
            username: "tester".to_owned(),
        }
    };
    let token = hunter::auth::new_session_token().unwrap();
    store
        .create_session(
            &user,
            &hunter::auth::token_hash(&token),
            hunter::util::now_ms() + hunter::auth::SESSION_TTL_MS,
        )
        .await
        .unwrap();
    format!("{}={token}", hunter::auth::SESSION_COOKIE)
}

/// Session token [`seed_session`] stores; [`TEST_COOKIE`] carries it.
const TEST_TOKEN: &str = "test-session";

/// `Cookie` header value for the session [`seed_session`] creates, for
/// tests whose `Store` is read-only and so cannot use [`auth_cookie`].
pub const TEST_COOKIE: &str = "hunter_session=test-session";

/// Insert the `tester` account and the session [`TEST_COOKIE`] names,
/// through a writable pool on a database a test later opens read-only.
pub async fn seed_session(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at) \
         VALUES (1000, 'tester', 'unused', 0)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) \
         VALUES (?1, 1000, 0, ?2)",
    )
    .bind(hunter::auth::token_hash(TEST_TOKEN))
    .bind(i64::MAX)
    .execute(pool)
    .await
    .unwrap();
}

/// Make job `job` the checkpoint of finding `fid`'s `kind` work item, as a
/// finding job that ended paused does (`Store::complete_job`). For tests
/// that seed suspended job rows by hand: a finding's work continues from
/// this pointer, not from whichever row reads `suspended`.
pub async fn point_checkpoint(pool: &SqlitePool, fid: i64, kind: &str, job: i64) {
    sqlx::query(
        "INSERT INTO finding_work (finding_id, kind, checkpoint_job_id) VALUES (?1, ?2, ?3) \
         ON CONFLICT (finding_id, kind) DO UPDATE SET checkpoint_job_id = excluded.checkpoint_job_id",
    )
    .bind(fid)
    .bind(kind)
    .bind(job)
    .execute(pool)
    .await
    .unwrap();
}

/// Make job `job` the checkpoint of repo `repo`'s `kind` work item, as a
/// repo-level job that ended paused does (`Store::complete_job`); `None`
/// drops it. For tests that seed suspended hunt or scan rows by hand.
pub async fn point_repo_checkpoint(pool: &SqlitePool, repo: i64, kind: &str, job: Option<i64>) {
    sqlx::query(
        "INSERT INTO repo_work (repo_id, kind, checkpoint_job_id) VALUES (?1, ?2, ?3) \
         ON CONFLICT (repo_id, kind) DO UPDATE SET checkpoint_job_id = excluded.checkpoint_job_id",
    )
    .bind(repo)
    .bind(kind)
    .bind(job)
    .execute(pool)
    .await
    .unwrap();
}

/// The operator account `tester`, created on first use: who releases a
/// hold in a test.
pub async fn operator(store: &Store) -> hunter::auth::CurrentUser {
    if let Some((user, _)) = store.login_candidate("tester").await.unwrap() {
        return user;
    }
    let id = store.create_user("tester", "unused").await.unwrap();
    hunter::auth::CurrentUser {
        id,
        username: "tester".to_owned(),
    }
}

/// The active hold on finding `fid`'s `kind` work, if any.
pub async fn hold_of(
    store: &Store,
    fid: i64,
    kind: hunter::domain::FindingJobKind,
) -> Option<hunter::store::WorkHold> {
    store
        .active_holds()
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.finding_id == fid && h.kind == kind)
}

/// Release finding `fid`'s `kind` hold with `action`, as `tester`.
pub async fn release(
    store: &Store,
    fid: i64,
    kind: hunter::domain::FindingJobKind,
    action: hunter::domain::ReleaseAction,
) {
    let by = operator(store).await;
    store
        .release_hold(
            fid,
            kind,
            &hunter::store::HoldRelease {
                action,
                reason: None,
                classification: None,
                by: &by,
            },
        )
        .await
        .unwrap();
}

/// The forge a test repo is on, as the reachability probe sees it: a name,
/// never a host the test reaches (`ScriptedBackend::offline` marks it down).
pub const FORGE_HOST: &str = "forge.example";

/// The provider a test backend's worker talks to (`ScriptedBackend::needing`).
pub const PROVIDER_HOST: &str = "provider.example";
