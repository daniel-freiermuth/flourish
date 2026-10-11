//! Config loading — reads hunter/config.json (API-CONTRACT.md §12).
//! `root` is the hunter/ project dir (contains config.json, ui/, data/);
//! relative paths resolve against it.

use std::path::{Path, PathBuf};

use crate::backends::omp_scavenge::LlmProvider;
use crate::forge::ReviewBots;
use anyhow::Context;
use serde::Deserialize;

/// Free space `work_root` must have for the scheduler to start a job.
///
/// Below it the cycle is denied before anything is picked. The clones,
/// every job's tree, the per-repo cargo caches and the database all live
/// under `work_root` by default, and running out mid-job fails in ways
/// that outlast the shortage: on 2026-09-29 a full disk failed every
/// checkout and a restart during one left a locked worktree that wedged
/// the fix queue.
pub const MIN_FREE_DISK_BYTES: u64 = 1 << 30;

/// On-disk shape of config.json — only the keys the serve path needs
/// (API-CONTRACT.md §12). Unknown keys are ignored (serde default).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    #[serde(rename = "workRoot")]
    work_root: Option<String>,
    #[serde(rename = "dbPath")]
    db_path: Option<String>,
    #[serde(rename = "ompBin")]
    omp_bin: Option<String>,
    #[serde(rename = "pollS")]
    poll_s: Option<f64>,
    #[serde(rename = "sessionGraceS")]
    session_grace_s: Option<u64>,
    serve: RawServe,
    budget: RawBudget,
    models: RawModels,
    backend: RawBackend,
    hunt: RawCaps,
    fix: RawCaps,
    scan: RawScan,
    modernization: RawScan,
    standards: RawScan,
    renovate: RawRenovate,
    feedback: RawFeedback,
    network: RawNetwork,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawServe {
    port: Option<u16>,
    host: Option<String>,
    #[serde(rename = "allowedHosts")]
    allowed_hosts: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBudget {
    #[serde(rename = "staleAfterS")]
    stale_after_s: Option<f64>,
    #[serde(rename = "cacheTtlS")]
    cache_ttl_s: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawModels {
    default: Option<String>,
    smol: Option<String>,
    hunt: Option<String>,
    fix: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawBackend {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "llmProvider")]
    llm_provider: Option<String>,
    #[serde(rename = "providerHosts")]
    provider_hosts: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawCaps {
    #[serde(rename = "maxWallS")]
    max_wall_s: Option<i64>,
    #[serde(rename = "maxFindings")]
    max_findings: Option<i64>,
    #[serde(rename = "rehuntDays")]
    rehunt_days: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawScan {
    #[serde(rename = "intervalDays")]
    interval_days: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawRenovate {
    #[serde(rename = "githubToken")]
    github_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawFeedback {
    bots: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawNetwork {
    gate: Option<bool>,
}

/// A credential read from config.json. `Debug` prints a placeholder, so
/// logging the `Config` never logs the value.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// Which `Host` header names the HTTP server answers to.
///
/// The check exists for DNS rebinding: a page the operator visits can point
/// its own hostname at this machine and talk to the port same-origin, and
/// the one thing that still gives it away is the attacker's name in `Host`.
/// Loopback names are always accepted -- no attacker's hostname is
/// `localhost` -- so a list only ever *adds* names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAllowList {
    /// Loopback names plus these (lowercased; `Host` names are
    /// case-insensitive).
    Names(Vec<String>),
    /// `serve.allowedHosts: ["*"]` -- any `Host`. Rebinding protection is
    /// off; only for a listener on a network the operator trusts entirely.
    Any,
}

impl Default for HostAllowList {
    fn default() -> Self {
        Self::Names(Vec::new())
    }
}

impl HostAllowList {
    /// Whether a `Host` name (port already stripped) is accepted.
    pub fn allows(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        matches!(name.as_str(), "localhost" | "127.0.0.1" | "::1")
            || match self {
                Self::Any => true,
                Self::Names(names) => names.contains(&name),
            }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The hunter/ project root this daemon reads from.
    pub root: PathBuf,
    pub work_root: PathBuf,
    pub db_path: PathBuf,
    pub serve_port: u16,
    /// Address the HTTP server binds (`serve.host`). Loopback unless the
    /// operator opts in, e.g. `0.0.0.0` for every IPv4 interface.
    pub serve_host: std::net::IpAddr,
    /// `Host` names accepted beyond loopback (`serve.allowedHosts`).
    pub allowed_hosts: HostAllowList,
    /// root/ui — index.html + compiled app.js.
    pub ui_dir: PathBuf,
    // -- backend inputs (BACKEND-CONTRACT.md §3; `types.Config`) ----------
    pub omp_bin: String,
    /// budget.staleAfterS — `keep_fresh` gate + status stale tone. Prod
    /// default 300 (the Python test harness overrides to 1800).
    pub stale_after_s: f64,
    /// budget.cacheTtlS — `anticipated_tokens` warm/cold split.
    pub cache_ttl_s: f64,
    /// pollS — how often the harness re-reads a running worker's ledger.
    pub poll_s: f64,
    /// How long a worker may run before its session ledger must have
    /// appeared, after which the run is killed as unmetered. Injectable
    /// only so the accounting path is testable without a 2-minute test —
    /// `maxWallS` next to it in the same loop is a parameter for the same
    /// reason.
    pub session_grace_s: u64,
    /// Free bytes `work_root` needs before a job starts. Always
    /// [`MIN_FREE_DISK_BYTES`]; a field only so the gate is testable
    /// without filling a disk.
    pub min_free_disk_bytes: u64,
    pub model_default: Option<String>,
    pub model_smol: Option<String>,
    pub model_hunt: Option<String>,
    pub model_fix: Option<String>,
    /// backend.type discriminator; only "omp-scavenge" exists.
    pub backend_type: String,
    pub llm_provider: LlmProvider,
    /// backend.providerHosts — the hosts a worker needs, each `host` or
    /// `host:port`, in place of the provider's own
    /// (`LlmProvider::api_hosts`). `None` = the provider's.
    pub provider_hosts: Option<Vec<String>>,
    /// network.gate — whether a cycle probes the hosts its candidate
    /// needs before dispatching it (`scheduler::network_gate`). Off for a
    /// machine that reaches them only through a proxy, where a direct
    /// connection never succeeds.
    pub network_gate: bool,

    pub hunt_max_wall_s: i64,
    pub hunt_max_findings: i64,
    pub hunt_rehunt_days: i64,
    pub fix_max_wall_s: i64,
    /// scan.intervalDays — per-repo hunt cadence gate
    /// (`types.Config.scan_interval_days`, read in `Config.load`).
    pub scan_interval_days: f64,
    /// modernization.intervalDays (`types.Config.modernization_interval_days`, read in
    /// `Config.load`).
    pub modernization_interval_days: i64,
    /// standards.intervalDays — per-repo standards audit cadence gate.
    pub standards_interval_days: i64,
    /// renovate.githubToken — the token Renovate's github.com lookups use
    /// for GitHub-hosted repos, in place of the operator's `gh` login
    /// (see `dep_scan::github_token`).
    pub renovate_github_token: Option<Secret>,
    /// feedback.bots — review bots whose PR comments engage and harvest
    /// read, besides those of people who can push to the repo. Everyone
    /// else's are dropped (`forge::screen_feedback`).
    pub review_bots: ReviewBots,
}

impl Config {
    /// (`model_hunt` if kind == "hunt" else `model_fix`) or `model_default`
    /// (`types.Config.model_for`).
    pub fn model_for(&self, kind: &str) -> Option<&str> {
        let specific = if kind == "hunt" {
            self.model_hunt.as_deref()
        } else {
            self.model_fix.as_deref()
        };
        specific.or(self.model_default.as_deref())
    }
}

impl Config {
    /// Load <root>/config.json (missing file = all defaults, like Python).
    /// Keys: workRoot (default "data"), dbPath (default "data/hunter.db"),
    /// serve.port (default 8377). Relative paths resolve against `root`.
    #[allow(
        clippy::too_many_lines,
        reason = "one linear validation pass per config key; each `ensure!` \
                  carries the prose explaining its bound, and splitting it \
                  into per-section helpers would scatter that reasoning \
                  across functions that are each called exactly once"
    )]
    pub fn load(root: &Path) -> anyhow::Result<Self> {
        const MS_PER_DAY: i64 = 86_400_000;
        // Git resolves worktree destinations from its clone, while workers
        // resolve their cwd from the daemon. Both must receive absolute paths.
        let root = std::path::absolute(root)
            .with_context(|| format!("resolving hunter root {}", root.display()))?;
        let cfg_path = root.join("config.json");
        let raw = match std::fs::read_to_string(&cfg_path) {
            Ok(text) => serde_json::from_str::<RawConfig>(&text)
                .with_context(|| format!("parsing {}", cfg_path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => RawConfig::default(),
            Err(err) => {
                return Err(
                    anyhow::Error::new(err).context(format!("reading {}", cfg_path.display()))
                );
            }
        };
        let resolve = |p: &str| -> PathBuf {
            let p = Path::new(p);
            if p.is_absolute() {
                p.to_owned()
            } else {
                root.join(p)
            }
        };
        // pollS drives the harness meter tick (harness.rs); zero busy-loops
        // the poll and negative/NaN panics Duration::from_secs_f64.
        let poll_s = raw.poll_s.unwrap_or(2.0);
        anyhow::ensure!(
            poll_s.is_finite() && poll_s > 0.0,
            "{}: pollS must be a finite number greater than 0 (got {poll_s})",
            cfg_path.display()
        );
        let session_grace_s = raw.session_grace_s.unwrap_or(120);
        anyhow::ensure!(
            session_grace_s > 0,
            "{}: sessionGraceS must be greater than 0",
            cfg_path.display()
        );
        // A zero or negative wall-clock limit trips `elapsed >= max_wall_s`
        // on the worker's first meter tick, so the run is killed before it
        // does anything. Zero is not a way to disable the limit, it is the
        // tightest possible one.
        let limit = |field: &str, value: i64| -> anyhow::Result<i64> {
            anyhow::ensure!(
                value > 0,
                "{}: {field} must be greater than 0 (got {value})",
                cfg_path.display()
            );
            Ok(value)
        };
        let hunt_max_wall_s = limit("hunt.maxWallS", raw.hunt.max_wall_s.unwrap_or(1800))?;
        let fix_max_wall_s = limit("fix.maxWallS", raw.fix.max_wall_s.unwrap_or(2700))?;
        // staleAfterS is compared as `age_s <= stale_after_s`, so 0 has a
        // defined meaning (only a just-recorded window counts as fresh).
        // A negative makes every window permanently stale, which makes
        // `keep_fresh` probe on every call.
        let stale_after_s = raw.budget.stale_after_s.unwrap_or(300.0);
        anyhow::ensure!(
            stale_after_s.is_finite() && stale_after_s >= 0.0,
            "{}: budget.staleAfterS must be a finite number greater than or equal to 0 (got {stale_after_s})",
            cfg_path.display()
        );
        // Cadences are validated at the precision the scheduler actually
        // uses: it converts days to milliseconds and compares against
        // elapsed time, so a value that rounds to a non-positive interval
        // makes its job eligible on every single cycle rather than "never"
        // or "rarely", which is the opposite of what an operator typing a
        // small number intends.
        #[allow(clippy::cast_precision_loss)]
        let max_days = (i64::MAX / MS_PER_DAY) as f64;
        let scan_interval_days = raw.scan.interval_days.unwrap_or(1.0);
        anyhow::ensure!(
            scan_interval_days.is_finite()
                && scan_interval_days > 0.0
                && scan_interval_days <= max_days
                && (scan_interval_days * MS_PER_DAY as f64) >= 1.0,
            "{}: scan.intervalDays must be a finite number greater than 0 and at most {max_days} \
             (got {scan_interval_days})",
            cfg_path.display()
        );
        // These two are whole days by the time the scheduler sees them, so
        // a fraction is truncated — 0.5 silently becomes 0, i.e. every
        // cycle. Reject it rather than reinterpret it.
        let whole_days = |field: &str, raw_days: Option<f64>| -> anyhow::Result<i64> {
            let d = raw_days.unwrap_or(30.0);
            anyhow::ensure!(
                d.is_finite() && d >= 1.0 && d <= max_days && d.fract() == 0.0,
                "{}: {field} must be a whole number of days, at least 1 and at most {max_days} \
                 (got {d})",
                cfg_path.display()
            );
            #[allow(clippy::cast_possible_truncation)]
            Ok(d as i64)
        };
        let modernization_interval_days = whole_days(
            "modernization.intervalDays",
            raw.modernization.interval_days,
        )?;
        let standards_interval_days =
            whole_days("standards.intervalDays", raw.standards.interval_days)?;
        // An empty token would win over the `gh` login and then fail every
        // lookup, which is worse than not setting it.
        let renovate_github_token = match raw.renovate.github_token {
            Some(t) if t.trim().is_empty() => anyhow::bail!(
                "{}: renovate.githubToken must not be empty (omit it to use the gh login)",
                cfg_path.display()
            ),
            t => t.map(Secret),
        };
        let llm_provider = match raw.backend.llm_provider.as_deref() {
            None => LlmProvider::Anthropic,
            Some(name) => LlmProvider::parse(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "{}: backend.llmProvider must be {} (got {name:?})",
                    cfg_path.display(),
                    LlmProvider::expected_names()
                )
            })?,
        };
        // A blank entry matches no login and reads as a typo for one; a
        // login never contains whitespace or a slash.
        let raw_bots = raw.feedback.bots.unwrap_or_default();
        anyhow::ensure!(
            raw_bots.iter().all(|b| {
                let b = b.trim();
                !b.is_empty() && !b.contains(|c: char| c.is_whitespace() || c == '/')
            }),
            "{}: feedback.bots entries must be account logins such as \"coderabbitai\" (got {raw_bots:?})",
            cfg_path.display()
        );
        let review_bots = ReviewBots::new(&raw_bots);
        // An entry is handed to a TCP connect as `host[:port]`, so a URL
        // or a blank would probe nothing and deny every cycle.
        if let Some(hosts) = &raw.backend.provider_hosts {
            anyhow::ensure!(
                hosts.iter().all(|h| {
                    !h.is_empty() && !h.contains(|c: char| c.is_whitespace() || c == '/')
                }),
                "{}: backend.providerHosts entries must be host names such as \
                 \"api.anthropic.com\", optionally with a :port (got {hosts:?})",
                cfg_path.display()
            );
        }

        let serve_host = match raw.serve.host.as_deref() {
            None => std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
            Some(h) => h.parse().map_err(|_| {
                anyhow::anyhow!(
                    "{}: serve.host must be an IP address such as 127.0.0.1 or 0.0.0.0 (got {h:?})",
                    cfg_path.display()
                )
            })?,
        };
        let allowed_hosts = match raw.serve.allowed_hosts {
            None => HostAllowList::default(),
            Some(raw_names) => {
                // Normalised before any check, so " * " is the wildcard and
                // not a name that slips past the combination rule below.
                let names: Vec<String> = raw_names
                    .iter()
                    .map(|n| n.trim().to_ascii_lowercase())
                    .collect();
                if names.iter().any(|n| n == "*") {
                    // "*" means all; next to other names it reads as a
                    // mistake, and guessing which one was meant is how a
                    // guard ends up wider than the operator intended.
                    anyhow::ensure!(
                        names.len() == 1,
                        "{}: serve.allowedHosts: \"*\" allows every host and cannot be combined with names",
                        cfg_path.display()
                    );
                    HostAllowList::Any
                } else {
                    anyhow::ensure!(
                        names.iter().all(|n| {
                            !n.is_empty()
                                && (!n.contains(':') || n.parse::<std::net::Ipv6Addr>().is_ok())
                        }),
                        "{}: serve.allowedHosts entries must be host names or IP addresses, without a port (got {raw_names:?})",
                        cfg_path.display()
                    );
                    // IPv6 in canonical (RFC 5952) form, which is what a
                    // browser puts in `Host`: `fe80:0:0:0:0:0:0:1` must
                    // still match `[fe80::1]`.
                    HostAllowList::Names(
                        names
                            .into_iter()
                            .map(|n| n.parse::<std::net::Ipv6Addr>().map_or(n, |a| a.to_string()))
                            .collect(),
                    )
                }
            }
        };

        Ok(Self {
            work_root: resolve(raw.work_root.as_deref().unwrap_or("data")),
            db_path: resolve(raw.db_path.as_deref().unwrap_or("data/hunter.db")),
            serve_port: raw.serve.port.unwrap_or(8377),
            serve_host,
            allowed_hosts,
            ui_dir: root.join("ui"),
            root,
            omp_bin: raw.omp_bin.unwrap_or_else(|| "omp".to_owned()),
            stale_after_s,
            cache_ttl_s: raw.budget.cache_ttl_s.unwrap_or(3600.0),
            poll_s,
            session_grace_s,
            min_free_disk_bytes: MIN_FREE_DISK_BYTES,
            model_default: raw.models.default,
            model_smol: raw.models.smol,
            model_hunt: raw.models.hunt,
            model_fix: raw.models.fix,
            backend_type: raw
                .backend
                .kind
                .unwrap_or_else(|| "omp-scavenge".to_owned()),
            llm_provider,
            provider_hosts: raw.backend.provider_hosts,
            network_gate: raw.network.gate.unwrap_or(true),
            hunt_max_wall_s,
            hunt_max_findings: raw.hunt.max_findings.unwrap_or(8),
            hunt_rehunt_days: raw.hunt.rehunt_days.unwrap_or(90),
            fix_max_wall_s,
            scan_interval_days,
            modernization_interval_days,
            standards_interval_days,
            renovate_github_token,
            review_bots,
        })
    }
}
