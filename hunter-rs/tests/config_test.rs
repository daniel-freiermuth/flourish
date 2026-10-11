#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
//! Config loading — the `load()` validation gate. Every rejection here
//! exists because the bad value is accepted silently by serde and only
//! surfaces as a worker that dies on its first meter tick, or a budget
//! probe that never stops firing.

use std::path::Path;

use hunter::config::{Config, HostAllowList};

mod support;
use support::TempDir;

/// Write `config.json` under a scratch root and load it.
fn load(dir: &TempDir, json: &str) -> anyhow::Result<Config> {
    std::fs::write(dir.join("config.json"), json).expect("write config.json");
    Config::load(dir.path())
}

fn err(dir: &TempDir, json: &str) -> String {
    match load(dir, json) {
        Ok(_) => panic!("expected {json} to be rejected"),
        Err(e) => format!("{e:#}"),
    }
}

// ---------------------------------------------------------------------------
// renovate.githubToken
// ---------------------------------------------------------------------------

/// An empty token would take precedence over the `gh` login and then fail
/// every GitHub lookup.
#[test]
fn empty_renovate_github_token_rejected() {
    let dir = TempDir::new("cfg-renovate-token-empty");
    let msg = err(&dir, r#"{"renovate": {"githubToken": "  "}}"#);
    assert!(msg.contains("renovate.githubToken"), "{msg}");
}

/// The token loads, and printing the config does not print it.
#[test]
fn renovate_github_token_loads_and_is_redacted() {
    let dir = TempDir::new("cfg-renovate-token");
    let cfg = load(&dir, r#"{"renovate": {"githubToken": "ghp_s3cret"}}"#).unwrap();
    assert_eq!(
        cfg.renovate_github_token
            .as_ref()
            .map(hunter::config::Secret::expose),
        Some("ghp_s3cret")
    );
    let printed = format!("{cfg:?}");
    assert!(!printed.contains("ghp_s3cret"), "{printed}");
    // Redacted, not hidden: the operator can still see a token is set.
    assert!(
        printed.contains("renovate_github_token: Some(Secret(***))"),
        "{printed}"
    );
}

// ---------------------------------------------------------------------------
// feedback.bots
// ---------------------------------------------------------------------------

/// Bots load in either spelling of their login, and a blank or malformed
/// entry is refused rather than silently matching nobody -- the operator
/// would see their bot's reviews ignored with no clue why.
#[test]
fn feedback_bots_load_and_reject_non_logins() {
    let dir = TempDir::new("cfg-feedback-bots");
    let cfg = load(&dir, r#"{"feedback": {"bots": ["CodeRabbitAI[bot]"]}}"#).unwrap();
    assert!(cfg.review_bots.contains("coderabbitai"));
    assert!(!cfg.review_bots.contains("stranger"));
    assert!(
        !load(&dir, "{}")
            .unwrap()
            .review_bots
            .contains("coderabbitai")
    );

    for bad in [
        r#"[""]"#,
        r#"["  "]"#,
        r#"["code rabbit"]"#,
        r#"["acme/bot"]"#,
    ] {
        let msg = err(&dir, &format!(r#"{{"feedback": {{"bots": {bad}}}}}"#));
        assert!(msg.contains("feedback.bots"), "{bad}: {msg}");
    }
}

// ---------------------------------------------------------------------------
// Worker limits: zero is the tightest limit, not "disabled"
// ---------------------------------------------------------------------------

#[test]
fn test_hunt_max_wall_s_zero_rejected() {
    let dir = TempDir::new("cfg-hunt-wall-zero");
    let msg = err(&dir, r#"{"hunt": {"maxWallS": 0}}"#);
    assert!(msg.contains("hunt.maxWallS"), "{msg}");
}

#[test]
fn test_fix_max_wall_s_zero_rejected() {
    let dir = TempDir::new("cfg-fix-wall-zero");
    let msg = err(&dir, r#"{"fix": {"maxWallS": 0}}"#);
    assert!(msg.contains("fix.maxWallS"), "{msg}");
}

// ---------------------------------------------------------------------------
// Retired keys
// ---------------------------------------------------------------------------

/// `hunt.capNewTokens` and `fix.capNewTokens` are gone: a job's token
/// bound is now the budget ramp's headroom and nothing else. Deployed
/// config.json files still carry the keys, so loading one must keep
/// working and simply ignore them — an operator does not get a daemon
/// that refuses to start because a stale key is still in the file.
#[test]
fn retired_cap_keys_are_ignored_not_rejected() {
    let dir = TempDir::new("cfg-retired-caps");
    let cfg = load(
        &dir,
        r#"{"hunt": {"capNewTokens": 200000, "maxWallS": 600},
             "fix": {"capNewTokens": 150000, "maxWallS": 900}}"#,
    )
    .expect("a config that still names the retired cap keys must load");
    assert_eq!(cfg.hunt_max_wall_s, 600);
    assert_eq!(cfg.fix_max_wall_s, 900);

    // Not merely tolerated — not read at all. Both values were fatal
    // while the keys were live (zero is the tightest cap, not "no cap"),
    // so accepting them is what separates an ignored key from a
    // validated one.
    let dir = TempDir::new("cfg-retired-caps-fatal");
    load(
        &dir,
        r#"{"hunt": {"capNewTokens": 0}, "fix": {"capNewTokens": -1}}"#,
    )
    .expect("a value that used to be rejected must now be inert");
}

// ---------------------------------------------------------------------------
// budget.staleAfterS: negatives make every window permanently stale, but
// zero is a defined threshold (`age_s <= stale_after_s`).
// ---------------------------------------------------------------------------

#[test]
fn test_stale_after_s_negative_rejected() {
    let dir = TempDir::new("cfg-stale-neg");
    let msg = err(&dir, r#"{"budget": {"staleAfterS": -1}}"#);
    assert!(
        msg.contains("budget.staleAfterS") && msg.contains("-1"),
        "{msg}"
    );
}

#[test]
fn test_stale_after_s_zero_accepted() {
    let dir = TempDir::new("cfg-stale-zero");
    let cfg = load(
        &dir,
        r#"{"budget": {"staleAfterS": 0},
             "hunt": {"maxWallS": 1},
             "fix": {"maxWallS": 1}}"#,
    )
    .expect("staleAfterS 0 is a valid threshold");
    assert_eq!(cfg.stale_after_s, 0.0);
    assert_eq!(cfg.hunt_max_wall_s, 1);
    assert_eq!(cfg.fix_max_wall_s, 1);
}

#[test]
fn test_defaults_load_without_config_file() {
    let dir = TempDir::new("cfg-defaults");
    let cfg = Config::load(dir.path()).expect("missing config.json = all defaults");
    assert_eq!(cfg.hunt_max_wall_s, 1800);
    assert_eq!(cfg.fix_max_wall_s, 2700);
    assert_eq!(cfg.stale_after_s, 300.0);
    assert_eq!(cfg.root, Path::new(dir.path()));
}

/// Cadences are validated at the precision the scheduler uses.
///
/// `scan.intervalDays` is multiplied by `86_400_000` and cast to `i64`;
/// the modernization and standards cadences are cast to `i64` days
/// first. So a value that rounds to a non-positive interval does not
/// mean "never" or "rarely" — it makes that job eligible on every cycle,
/// the opposite of what a small number is meant to express.
#[test]
fn cadences_that_round_to_no_interval_are_rejected() {
    for (field, value) in [
        // Truncates to 0 days -> eligible every cycle.
        ("modernization", "0.5"),
        ("standards", "0.5"),
        ("modernization", "-0.5"),
        // Zero and negative outright.
        ("modernization", "0"),
        ("standards", "-1"),
        ("scan", "0"),
        ("scan", "-1"),
        // Sub-millisecond: * 86_400_000 still floors to 0 ms.
        ("scan", "0.00000000001"),
        // Would overflow the millisecond conversion.
        ("modernization", "1e300"),
        ("scan", "1e300"),
    ] {
        let dir = TempDir::new("cfg-cadence");
        std::fs::write(
            dir.join("config.json"),
            format!(r#"{{"{field}": {{"intervalDays": {value}}}}}"#),
        )
        .unwrap();
        let err = Config::load(dir.path())
            .expect_err(&format!("{field}.intervalDays = {value} was accepted"));
        assert!(
            format!("{err}").contains(&format!("{field}.intervalDays")),
            "error should name the field, got: {err}"
        );
    }
}

/// The values an operator actually writes still load.
#[test]
fn ordinary_cadences_load() {
    let dir = TempDir::new("cfg-cadence-ok");
    std::fs::write(
        dir.join("config.json"),
        r#"{"scan": {"intervalDays": 0.5},
            "modernization": {"intervalDays": 30},
            "standards": {"intervalDays": 14}}"#,
    )
    .unwrap();
    let cfg = Config::load(dir.path()).expect("valid cadences");
    // Half a day is meaningful for the scan scheduler: it keeps a
    // sub-day interval because that one is not truncated to whole days.
    assert!((cfg.scan_interval_days - 0.5).abs() < f64::EPSILON);
    assert_eq!(cfg.modernization_interval_days, 30);
    assert_eq!(cfg.standards_interval_days, 14);
}

// -- serve.host / serve.allowedHosts ------------------------------------------

/// No config means today's behaviour exactly: loopback bind, loopback names.
#[test]
fn the_listener_is_loopback_only_by_default() {
    let dir = TempDir::new("cfg-serve-default");
    let cfg = load(&dir, "{}").unwrap();
    assert_eq!(cfg.serve_host, std::net::IpAddr::from([127, 0, 0, 1]));
    assert_eq!(cfg.allowed_hosts, HostAllowList::default());
    assert!(cfg.allowed_hosts.allows("localhost"));
    assert!(cfg.allowed_hosts.allows("127.0.0.1"));
    assert!(cfg.allowed_hosts.allows("::1"));
    assert!(!cfg.allowed_hosts.allows("hunter-box"));
}

/// Listing names adds them to loopback -- `localhost` keeps working -- and
/// matching ignores case, as `Host` names do. An IPv6 entry matches the
/// canonical form a browser sends, however it was written.
#[test]
fn allowed_hosts_add_names_to_loopback() {
    let dir = TempDir::new("cfg-serve-names");
    let cfg = load(
        &dir,
        r#"{"serve": {"host": "0.0.0.0", "allowedHosts": ["Hunter-Box", "192.168.1.20", "FE80:0:0:0:0:0:0:0001"]}}"#,
    )
    .unwrap();
    assert_eq!(cfg.serve_host, std::net::IpAddr::from([0, 0, 0, 0]));
    for ok in [
        "hunter-box",
        "HUNTER-BOX",
        "192.168.1.20",
        "fe80::1",
        "localhost",
    ] {
        assert!(cfg.allowed_hosts.allows(ok), "{ok} should be accepted");
    }
    assert!(!cfg.allowed_hosts.allows("rebound.attacker.example"));
}

/// `"*"` switches the check off, and only on its own.
#[test]
fn a_lone_star_allows_every_host_and_nothing_else_does() {
    let dir = TempDir::new("cfg-serve-any");
    let cfg = load(&dir, r#"{"serve": {"allowedHosts": ["*"]}}"#).unwrap();
    assert_eq!(cfg.allowed_hosts, HostAllowList::Any);
    assert!(cfg.allowed_hosts.allows("rebound.attacker.example"));

    // Padding does not disguise the wildcard as a name.
    for combined in [r#"["*", "hunter-box"]"#, r#"[" * ", "hunter-box"]"#] {
        let e = err(
            &dir,
            &format!(r#"{{"serve": {{"allowedHosts": {combined}}}}}"#),
        );
        assert!(e.contains("cannot be combined"), "{combined}: {e}");
    }
}

/// Misconfigurations are refused at load time, naming the field, instead
/// of producing a guard that is wider or narrower than intended.
#[test]
fn bad_serve_settings_are_refused() {
    let dir = TempDir::new("cfg-serve-bad");
    let e = err(&dir, r#"{"serve": {"host": "hunter-box"}}"#);
    assert!(e.contains("serve.host must be an IP address"), "{e}");
    for bad in [r#"["hunter-box:8377"]"#, r#"[""]"#, r#"["  "]"#] {
        let e = err(&dir, &format!(r#"{{"serve": {{"allowedHosts": {bad}}}}}"#));
        assert!(e.contains("serve.allowedHosts entries"), "{bad}: {e}");
    }
}

#[test]
fn llm_provider_defaults_to_anthropic_and_accepts_supported_providers() {
    let default_dir = TempDir::new("cfg-provider-default");
    let default = Config::load(default_dir.path()).expect("default configuration");
    assert_eq!(
        default.llm_provider,
        hunter::backends::omp_scavenge::LlmProvider::Anthropic
    );

    let explicit_dir = TempDir::new("cfg-provider-anthropic");
    let explicit = load(
        &explicit_dir,
        r#"{"backend": {"llmProvider": "anthropic"}}"#,
    )
    .expect("explicit Anthropic provider configuration");
    assert_eq!(
        explicit.llm_provider,
        hunter::backends::omp_scavenge::LlmProvider::Anthropic
    );

    let codex_dir = TempDir::new("cfg-provider-codex");
    let codex = load(
        &codex_dir,
        r#"{"backend": {"llmProvider": "openai-codex"}}"#,
    )
    .expect("Codex provider configuration");
    assert_eq!(
        codex.llm_provider,
        hunter::backends::omp_scavenge::LlmProvider::OpenAiCodex
    );

    let copilot_dir = TempDir::new("cfg-provider-copilot");
    let copilot = load(
        &copilot_dir,
        r#"{"backend": {"llmProvider": "github-copilot"}}"#,
    )
    .expect("Copilot provider configuration");
    assert_eq!(
        copilot.llm_provider,
        hunter::backends::omp_scavenge::LlmProvider::GitHubCopilot
    );
}

#[test]
fn unknown_llm_provider_is_rejected() {
    let dir = TempDir::new("cfg-provider-invalid");
    let message = err(&dir, r#"{"backend": {"llmProvider": "unknown-provider"}}"#);
    assert!(message.contains("backend.llmProvider"), "{message}");
}

// ---------------------------------------------------------------------------
// backend.providerHosts, network.gate
// ---------------------------------------------------------------------------

/// Unset, the backend probes its provider's own hosts and the gate is on.
#[test]
fn the_network_gate_is_on_and_probes_the_providers_hosts_by_default() {
    let dir = TempDir::new("cfg-network-defaults");
    let cfg = load(&dir, "{}").unwrap();
    assert!(cfg.network_gate);
    assert_eq!(cfg.provider_hosts, None);
}

#[test]
fn provider_hosts_and_the_gate_switch_load() {
    let dir = TempDir::new("cfg-network-set");
    let cfg = load(
        &dir,
        r#"{"backend": {"providerHosts": ["llm-gw.corp:8443"]}, "network": {"gate": false}}"#,
    )
    .unwrap();
    assert!(!cfg.network_gate);
    assert_eq!(
        cfg.provider_hosts,
        Some(vec!["llm-gw.corp:8443".to_owned()])
    );
}

/// An entry goes to a TCP connect as `host[:port]`: a URL or a blank
/// resolves nowhere, and would hold every cycle as offline.
#[test]
fn provider_hosts_that_are_not_host_names_are_rejected() {
    let dir = TempDir::new("cfg-provider-hosts-invalid");
    for bad in [
        r#"["https://api.anthropic.com"]"#,
        r#"[""]"#,
        r#"["api anthropic.com"]"#,
    ] {
        let msg = err(
            &dir,
            &format!(r#"{{"backend": {{"providerHosts": {bad}}}}}"#),
        );
        assert!(msg.contains("backend.providerHosts"), "{bad}: {msg}");
    }
}
