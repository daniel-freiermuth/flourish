#![allow(
    clippy::type_complexity,
    clippy::needless_pass_by_value,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]
//! Facade tests — decide (§4 items 1-21, 27), unaccounted (43-48),
//! `refresh/keep_fresh` (49-57). Skip 5 server-prober tests (round 3).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use hunter::backend::{Backend, Prober, SpendLedger, Verdict};
use hunter::backends::omp_scavenge::OmpScavengeBackend;
use hunter::backends::omp_scavenge::capacity::{FIVE_H_MS, HEADROOM_MS, WEEK_MS, WindowState};
use hunter::config::Config;

mod support;
use support::TempDir;

// ---- constants (shared harness, test_budget.py:22-94) ---------------------

const HOUR_MS: i64 = 3_600_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ---- helpers --------------------------------------------------------------

fn cfg_with(stale_after_s: f64, omp_bin: &str) -> Config {
    Config {
        root: PathBuf::from("/tmp"),
        work_root: PathBuf::from("/tmp"),
        db_path: PathBuf::from("/tmp/test.db"),
        serve_port: 8378,
        serve_host: std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        allowed_hosts: hunter::config::HostAllowList::default(),
        ui_dir: PathBuf::from("/tmp/ui"),
        omp_bin: omp_bin.to_owned(),
        stale_after_s,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        session_grace_s: 120,
        min_free_disk_bytes: 0,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".to_owned(),
        llm_provider: hunter::backends::omp_scavenge::LlmProvider::Anthropic,
        provider_hosts: None,
        network_gate: false,
        hunt_max_wall_s: 1800,
        hunt_max_findings: 8,
        hunt_rehunt_days: 90,
        fix_max_wall_s: 2700,
        scan_interval_days: 1.0,
        modernization_interval_days: 30,
        standards_interval_days: 30,
        renovate_github_token: None,
        review_bots: hunter::forge::ReviewBots::default(),
    }
}

fn cfg() -> Config {
    cfg_with(1800.0, "omp")
}

/// Build a `WindowState` with sensible defaults (test_budget.py:66-78).
fn ws(lid: &str, used: f64, status: &str, resets_at: i64, age_s: f64) -> WindowState {
    let now = now_ms();
    WindowState {
        limit_id: lid.to_owned(),
        used_fraction: Some(used),
        status: Some(status.to_owned()),
        resets_at: Some(resets_at),
        recorded_at: now - (age_s * 1000.0) as i64,
        age_s,
    }
}

/// Standard healthy windows (test_budget.py:83-94).
#[allow(clippy::similar_names)]
fn healthy_windows(w5_used: f64, w5_elapsed_h: f64) -> BTreeMap<String, WindowState> {
    let now = now_ms();
    let resets_5h = now + ((5.0 - w5_elapsed_h) * HOUR_MS as f64) as i64;
    let resets_7d = now + WEEK_MS / 2;
    let mut m = BTreeMap::new();
    m.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", w5_used, "ok", resets_5h, 60.0),
    );
    m.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", resets_7d, 60.0),
    );
    m.insert(
        "anthropic:7d:model-class".to_owned(),
        ws("anthropic:7d:model-class", 0.10, "ok", resets_7d, 60.0),
    );
    m
}

// ---- FakeLedger (test_budget.py:93-100) -----------------------------------

struct FakeLedger {
    running: i64,
    finished: i64,
    /// Per-timestamp overrides for `finished_since` (keyed by `ts_ms`).
    finished_map: std::collections::HashMap<i64, i64>,
    /// Per-limit calibrated capacity. Empty = no history yet, which is
    /// what a fresh store reports and what every other test here wants.
    capacity: std::collections::HashMap<String, f64>,
}

impl FakeLedger {
    fn new(running: i64, finished: i64) -> Self {
        Self {
            running,
            finished,
            finished_map: std::collections::HashMap::new(),
            capacity: std::collections::HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl SpendLedger for FakeLedger {
    async fn running_estimate(&self) -> sqlx::Result<i64> {
        Ok(self.running)
    }
    async fn finished_since(&self, ts_ms: i64) -> sqlx::Result<i64> {
        Ok(*self.finished_map.get(&ts_ms).unwrap_or(&self.finished))
    }
    async fn finished_between(&self, _start_ms: i64, _end_ms: i64) -> sqlx::Result<i64> {
        Ok(0)
    }
    async fn log_window_observation(
        &self,
        _lid: &str,
        _uf: Option<f64>,
        _s: Option<&str>,
        _r: Option<i64>,
        _a: f64,
    ) -> sqlx::Result<()> {
        Ok(())
    }
    async fn last_window_observation(
        &self,
        _lid: &str,
        _r: i64,
    ) -> sqlx::Result<Option<(i64, f64)>> {
        Ok(None)
    }
    async fn record_calibration_sample(
        &self,
        _lid: &str,
        _wr: Option<i64>,
        _d: f64,
        _t: i64,
    ) -> sqlx::Result<()> {
        Ok(())
    }
    async fn estimate_capacity(&self, lid: &str) -> sqlx::Result<Option<f64>> {
        Ok(self.capacity.get(lid).copied())
    }
}

// ---- FakeProber -----------------------------------------------------------

struct FakeProber {
    calls: Mutex<Vec<(Vec<String>, u64)>>,
    rcs: Vec<i32>,
}

impl FakeProber {
    fn new(rc: i32) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            rcs: vec![rc],
        }
    }
}

impl Prober for FakeProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        let mut calls = self.calls.lock().unwrap();
        let idx = calls.len();
        calls.push((
            argv.iter().map(std::string::ToString::to_string).collect(),
            timeout_s,
        ));
        let rc = self
            .rcs
            .get(idx)
            .copied()
            .unwrap_or(*self.rcs.last().unwrap_or(&0));
        (rc, String::new())
    }
}

// ---- backend builder ------------------------------------------------------

fn make_backend(ledger: FakeLedger) -> OmpScavengeBackend {
    make_backend_with(ledger, cfg(), FakeProber::new(0))
}

fn make_backend_with(ledger: FakeLedger, cfg: Config, prober: FakeProber) -> OmpScavengeBackend {
    OmpScavengeBackend {
        cfg,
        ledger: Arc::new(ledger),
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(prober),
    }
}

// ---- verdict helpers ------------------------------------------------------

fn is_granted(v: &Verdict) -> bool {
    matches!(v, Verdict::Granted { .. })
}

fn is_denied(v: &Verdict) -> bool {
    matches!(v, Verdict::Denied { .. })
}

fn reason(v: &Verdict) -> &str {
    match v {
        Verdict::Granted { reason, .. } | Verdict::Denied { reason, .. } => reason,
    }
}

fn cap_tokens(v: &Verdict) -> Option<i64> {
    match v {
        Verdict::Granted { cap_tokens, .. } => *cap_tokens,
        Verdict::Denied { .. } => None,
    }
}

fn retry_at(v: &Verdict) -> Option<f64> {
    match v {
        Verdict::Denied { retry_at, .. } => *retry_at,
        Verdict::Granted { .. } => None,
    }
}

// ============================================================
// Decide tests (§4 items 1-21, 27)
// ============================================================

/// Item 1: empty windows deny.
#[tokio::test]
async fn test_empty_windows_deny() {
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b
        .decide_with_windows(&BTreeMap::new(), 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("no window data"));
}

/// Item 2: stale 5h low usage allows via ramp, not bypass.
#[tokio::test]
async fn test_stale_5h_low_usage_allows_via_ramp() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(
        is_granted(&o.normal),
        "expected Granted, got {:?}",
        o.normal
    );
}

/// Item 3: stale 5h high usage denies.
#[tokio::test]
async fn test_stale_5h_high_usage_denies() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.90, "ok", now + HOUR_MS, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 4: own finished jobs count toward `effective_used`.
#[tokio::test]
async fn test_own_finished_jobs_count() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, 30.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 1_600_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 5: denied by 7d ramp when 7d has high usage.
#[tokio::test]
async fn test_denied_by_7d_ramp() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    // Override 7d: high usage, tight ramp.
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 6: 5h and 7d unaccounted reservations are independent.
#[tokio::test]
async fn test_unaccounted_reservations_independent_7d() {
    let now = now_ms();
    // (a) 7d .02 resets now+.9w → ramp .10; finished=20M → res_7d≈.298 → Denied 7d.
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.0,
            "ok",
            now + (0.5 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.02,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 20_000_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 6b: independent reservations — 5h denied when 5h ramp is the gate.
#[tokio::test]
async fn test_unaccounted_reservations_independent_5h() {
    let now = now_ms();
    // (b) 5h used 0, elapsed 3h (ramp .556); finished=5M → res_5h=2.5 → Denied 5h.
    let elapsed_3h = now + (2.0 * HOUR_MS as f64) as i64; // resets in 2h → 3h elapsed
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.0, "ok", elapsed_3h, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.02, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 5_000_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

/// Item 7: 7d used above ramp → Denied; `retry_at` round-trip.
#[tokio::test]
async fn test_7d_used_above_ramp_deny() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("ramp"));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    let expected = now as f64 + 0.20 * WEEK_MS as f64;
    assert!(
        (rt - expected).abs() < 2000.0,
        "retry_at off: got {rt}, expected ≈{expected}"
    );
}

/// Item 8: 7d used below ramp → Granted.
#[tokio::test]
async fn test_7d_used_below_ramp_allow() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.30, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 9: 5h first 30min deny (headroom).
#[tokio::test]
async fn test_5h_first_30min_deny() {
    let now = now_ms();
    // elapsed 0.25h → ramp 0.
    let elapsed_ms = (0.25 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.05, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
    assert!(reason(&o.normal).contains("ramp"));
}

/// Item 10: 5h at exactly 30min → deny (ramp=0, used .01 ≥ 0).
#[tokio::test]
async fn test_5h_at_exactly_30min_deny() {
    let now = now_ms();
    let elapsed_ms = HEADROOM_MS; // exactly 30 min
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.01, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
}

/// Item 11: 5h halfway low usage → Granted.
#[tokio::test]
async fn test_5h_halfway_low_usage_allow() {
    let now = now_ms();
    // 2.75h elapsed → ramp 0.5.
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.05, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 12: 5h halfway high usage → Denied; `retry_at`.
#[tokio::test]
async fn test_5h_halfway_high_usage_deny() {
    let now = now_ms();
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.60, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
    assert!(reason(&o.normal).contains("ramp"));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    // retry_at ≈ NOW + 0.45 * 3600 * 1000 (ramp inverse of 0.60)
    let expected = now as f64 + 0.45 * HOUR_MS as f64;
    assert!(
        (rt - expected).abs() < 5000.0,
        "retry_at off: got {rt}, expected ≈{expected}"
    );
}

/// Item 13: 5h nearly done, high usage → Granted.
#[tokio::test]
async fn test_5h_end_high_usage_allow() {
    let now = now_ms();
    // 4.95h elapsed → ramp ≈ 0.989.
    let elapsed_ms = (4.95 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.90, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 14: 5h exhausted → Denied; `retry_at` == `resets_at` exactly.
#[tokio::test]
async fn test_5h_exhausted_deny() {
    let now = now_ms();
    let resets = now + WEEK_MS / 2;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 1.0, "exhausted", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    assert!(
        (rt - resets as f64).abs() < 1.0,
        "retry_at should equal resets_at"
    );
}

/// Item 15: 5h exhausted but stale still denies.
#[tokio::test]
async fn test_5h_exhausted_stale_still_denies() {
    let now = now_ms();
    let resets = now + 4 * 60 * 1000; // resets in 4 min
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 1.0, "exhausted", resets, 3600.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    let rt = retry_at(&o.normal).expect("retry_at should be Some");
    assert!(
        (rt - resets as f64).abs() < 1.0,
        "retry_at should equal resets_at"
    );
}

/// Item 16: 7d denial during 5h headroom uses 7d retry.
#[allow(clippy::similar_names)]
#[tokio::test]
async fn test_7d_denial_during_5h_headroom() {
    let now = now_ms();
    // 5h in headroom (elapsed 0.25h), 7d over ramp.
    let resets_5h = now + (4.75 * HOUR_MS as f64) as i64;
    let resets_7d = now + (0.9 * WEEK_MS as f64) as i64;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.0, "ok", resets_5h, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.30, "ok", resets_7d, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        reason(&o.normal).starts_with("anthropic:7d"),
        "reason should start with 7d lid: {}",
        reason(&o.normal)
    );
    let rt = retry_at(&o.normal).expect("retry_at");
    let expected = now as f64 + 0.20 * WEEK_MS as f64;
    assert!((rt - expected).abs() < 2000.0);
}

/// Item 17: no 5h window, only 7d(.10) → Granted.
#[tokio::test]
async fn test_no_5h_window_allow() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(cap_tokens(&o.normal).is_some_and(|c| c > 0));
}

/// Item 18: no 5h, 7d over ramp → Denied.
#[tokio::test]
async fn test_no_5h_window_7d_over_deny() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 19: expired model-class window ignored.
///
/// Fully used on purpose: an expired window's 7d ramp is 1.0, so at 0.99
/// it would pass the ramp anyway and the expiry skip would go untested.
#[tokio::test]
async fn test_expired_model_class_window_ignored() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    // Add expired model-class window.
    let expired_resets = now - 3 * WEEK_MS;
    w.insert(
        "anthropic:7d:abandoned-model".to_owned(),
        WindowState {
            limit_id: "anthropic:7d:abandoned-model".to_owned(),
            used_fraction: Some(1.0),
            status: Some("ok".to_owned()),
            resets_at: Some(expired_resets),
            recorded_at: now - 26 * 24 * HOUR_MS,
            age_s: 26.0 * 24.0 * 3600.0,
        },
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal), "{:?}", o.normal);
}

/// `resets_at = 0` means "reset unknown", not "expired": the window still
/// gates, against a ramp of 1.0, so reaching it exactly denies.
#[tokio::test]
async fn test_7d_window_with_unknown_reset_denies_once_full() {
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 1.0, "ok", 0, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal), "{:?}", o.normal);
    assert!(
        reason(&o.normal).starts_with("anthropic:7d:"),
        "{:?}",
        o.normal
    );
}

/// Item 20: active model-class window still gates.
#[tokio::test]
async fn test_active_model_class_window_gates() {
    let now = now_ms();
    let mut w = healthy_windows(0.05, 4.5);
    w.insert(
        "anthropic:7d:active-model".to_owned(),
        ws(
            "anthropic:7d:active-model",
            0.30,
            "ok",
            now + (0.95 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("7d"));
}

/// Item 21: healthy windows → Granted with cap > 0.
#[tokio::test]
async fn test_healthy_allow() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(cap_tokens(&o.normal).is_some_and(|c| c > 0));
}

/// Item 27: fresh rollover + unaccounted alone → Denied.
#[tokio::test]
async fn test_decide_denies_on_unaccounted_alone_through_rollover() {
    let now = now_ms();
    // 5h: used 0, rolled over, elapsed 3h (ramp .556).
    let resets = now + 2 * HOUR_MS; // resets in 2h → elapsed 3h
    let window_start = resets - FIVE_H_MS;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        WindowState {
            limit_id: "anthropic:5h".to_owned(),
            used_fraction: Some(0.0),
            status: Some("ok".to_owned()),
            resets_at: Some(resets),
            recorded_at: window_start, // probe at window start
            age_s: (now - window_start) as f64 / 1000.0,
        },
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    // ledger finished=2_900_000 → res_5h ≈ 1.45 → eff 1.45 ≥ .556 → Denied.
    let b = make_backend(FakeLedger::new(0, 2_900_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(reason(&o.normal).contains("5h"));
}

// ============================================================
// Unaccounted tests (§4 items 43-48)
//
// These test _unaccounted_fraction indirectly through decide_with_windows.
// FakeLedger ignores ts in finished_since, so the per-window probe_at
// distinction doesn't affect the RETURNED fraction — we verify by checking
// whether the resulting verdict grants or denies.
// ============================================================

/// Item 43: no jobs → unaccounted (0, 0) → Granted.
#[tokio::test]
async fn test_unaccounted_no_jobs() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 44: running job counted via `cap_tokens`.
#[tokio::test]
async fn test_unaccounted_running_job() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(150_000, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    // With 150k running and 2M cap, res ≈ 0.075 — still granted.
    assert!(is_granted(&o.normal));
}

/// Item 45: anticipated added to both fields.
#[tokio::test]
async fn test_unaccounted_anticipated() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 80_000, now_ms()).await.unwrap();
    // With anticipated 80k and 2M cap, res ≈ 0.04 — still granted.
    assert!(is_granted(&o.normal));
}

/// Item 46: finished job scoped to each window's own probe.
#[tokio::test]
async fn test_unaccounted_finished_job_probe_scope() {
    let w = healthy_windows(0.05, 4.5);
    // FakeLedger always returns finished=50_000 regardless of ts.
    let b = make_backend(FakeLedger::new(0, 50_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    // res ≈ 50k/2M = 0.025 — small, still granted.
    assert!(is_granted(&o.normal));
}

/// Item 47: stale 7d probe doesn't drag 5h baseline back.
/// (Verifies per-window `probe_at`, not shared min.)
#[tokio::test]
async fn test_unaccounted_stale_7d_probe() {
    // With FakeLedger (finished_since ignores ts), we can't
    // differentiate per-window probe_at. This test verifies the
    // structure doesn't break; real Store tests (58-66) cover the delta.
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
}

/// Item 48: fallback to min when window missing.
#[tokio::test]
async fn test_unaccounted_fallback_min_probe() {
    let now = now_ms();
    // Only 7d present (probe 1h ago); 10k tok after → (10k, 10k).
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 3600.0),
    );
    let b = make_backend(FakeLedger::new(0, 10_000));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    // 10k on 67.2M cap is tiny → Granted.
    assert!(is_granted(&o.normal));
}

// ============================================================
// Refresh / keep_fresh tests (§4 items 49-57)
// ============================================================

/// Create a fixture agent.db for `keep_fresh` tests. Windows are
/// controlled by inserting `usage_history` rows.
///
/// The returned `TempDir` is the cleanup guard: bind it for the whole
/// test rather than discarding it, or the database goes away mid-test.
async fn make_agent_db(rows: &[(&str, Option<f64>, &str, Option<i64>, i64)]) -> (TempDir, PathBuf) {
    use sqlx::sqlite::SqliteConnectOptions;

    let dir = TempDir::new("facade-agent-db");
    let path = dir.join("agent.db");
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();

    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS usage_history (\
         id INTEGER PRIMARY KEY AUTOINCREMENT,\
         recorded_at INTEGER NOT NULL,\
         provider TEXT NOT NULL,\
         account_key TEXT NOT NULL,\
         limit_id TEXT NOT NULL,\
         label TEXT NOT NULL,\
         used_fraction REAL,\
         status TEXT,\
         resets_at INTEGER)",
    )
    .execute(&pool)
    .await
    .unwrap();

    for &(lid, uf, status, resets, recorded) in rows {
        // omp files each limit under the provider that prefixes its id.
        let provider = lid.split(':').next().unwrap();
        sqlx::query(
            "INSERT INTO usage_history \
             (recorded_at, provider, account_key, limit_id, label, \
              used_fraction, status, resets_at) \
             VALUES (?, ?, 'acct', ?, ?, ?, ?, ?)",
        )
        .bind(recorded)
        .bind(provider)
        .bind(lid)
        .bind(lid)
        .bind(uf)
        .bind(status)
        .bind(resets)
        .execute(&pool)
        .await
        .unwrap();
    }

    pool.close().await;
    (dir, path)
}

// The prober is behind Arc<dyn Prober>, so we can't downcast to check
// calls. Use a shared-state approach:

struct SharedProber {
    calls: Arc<Mutex<Vec<(Vec<String>, u64)>>>,
    rcs: Vec<i32>,
}

impl SharedProber {
    fn new(rc: i32) -> (Self, Arc<Mutex<Vec<(Vec<String>, u64)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                rcs: vec![rc],
            },
            calls,
        )
    }
    fn with_rcs(rcs: Vec<i32>) -> (Self, Arc<Mutex<Vec<(Vec<String>, u64)>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                calls: calls.clone(),
                rcs,
            },
            calls,
        )
    }
}

impl Prober for SharedProber {
    fn run(&self, argv: &[&str], timeout_s: u64) -> (i32, String) {
        let mut calls = self.calls.lock().unwrap();
        let idx = calls.len();
        calls.push((
            argv.iter().map(std::string::ToString::to_string).collect(),
            timeout_s,
        ));
        let rc = self
            .rcs
            .get(idx)
            .copied()
            .unwrap_or(*self.rcs.last().unwrap_or(&0));
        (rc, String::new())
    }
}

/// Item 49 (re-done with shared prober): no windows → True; calls == [INVALIDATE, READ].
#[tokio::test]
async fn test_no_windows_forces_probe_shared() {
    let (_dir, db) = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2);
    assert!(c[0].0.contains(&"invalidate".to_string()));
    assert!(!c[1].0.contains(&"invalidate".to_string()));
}

/// Item 50: fresh window → no probe.
#[tokio::test]
async fn test_fresh_window_no_probe() {
    let now = now_ms();
    let (_dir, db) = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 60_000, // age 60s, fresh
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(), // stale_after_s = 1800
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);
}

/// Item 51: stale window → probe.
#[tokio::test]
async fn test_stale_window_forces_probe() {
    let now = now_ms();
    let (_dir, db) = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 2_000_000, // age 2000s > 1800
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    assert_eq!(calls.lock().unwrap().len(), 2);
}

/// Item 52: age just inside `stale_after_s` → fresh, no probe. The exact
/// `<=` boundary is pinned by `test_age_exactly_at_threshold_is_fresh`.
#[tokio::test]
async fn test_age_just_inside_threshold_no_probe() {
    let now = now_ms();
    let (_dir, db) = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 1_800_000 + 5_000, // comfortably inside threshold (5s margin for CI)
    )])
    .await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);
}

/// Item 52 (boundary): an age landing EXACTLY on `stale_after_s` is fresh,
/// one millisecond past it is not. Goes through `is_fresh` — the predicate
/// `keep_fresh` gates the probe on — with `age_s` injected directly, because
/// the agent.db path derives `age_s` from the clock read inside `keep_fresh`
/// and so can never land on the threshold deterministically.
#[test]
fn test_age_exactly_at_threshold_is_fresh() {
    let now = now_ms();
    let (prober, _calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(), // stale_after_s = 1800
        ledger: Arc::new(FakeLedger::new(0, 0)),
        // Never read: is_fresh inspects the passed-in windows only.
        agent_db: PathBuf::from("/nonexistent/agent.db"),
        prober: Arc::new(prober),
    };

    let at = |age_s| {
        BTreeMap::from([(
            "anthropic:5h".to_owned(),
            ws("anthropic:5h", 0.10, "ok", now + HOUR_MS, age_s),
        )])
    };

    assert!(
        b.is_fresh(&at(1800.0)),
        "age == stale_after_s is fresh (inclusive ≤), so keep_fresh skips the probe"
    );
    assert!(
        !b.is_fresh(&at(1800.001)),
        "one millisecond past the threshold is stale"
    );
}

/// Item 53: respects configured `stale_after_s`.
#[tokio::test]
async fn test_respects_configured_stale_after_s() {
    let now = now_ms();
    let (_dir, db) = make_agent_db(&[(
        "anthropic:5h",
        Some(0.10),
        "ok",
        Some(now + HOUR_MS),
        now - 500_000, // age 500s
    )])
    .await;

    // threshold 1800 → 500s is fresh
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg_with(1800.0, "omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
    assert_eq!(calls.lock().unwrap().len(), 0);

    // threshold 300 → 500s is stale
    let (prober2, calls2) = SharedProber::new(0);
    let b2 = OmpScavengeBackend {
        cfg: cfg_with(300.0, "omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober2),
    };
    let result2 = b2.keep_fresh().await.unwrap();
    assert!(result2);
    assert_eq!(calls2.lock().unwrap().len(), 2);
}

/// Item 54: invalidates before reading.
#[tokio::test]
async fn test_invalidates_before_reading() {
    let (_dir, db) = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    b.keep_fresh().await.unwrap();
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2);
    assert!(
        c[0].0.contains(&"invalidate".to_string()),
        "first call should be invalidate"
    );
    assert!(
        !c[1].0.contains(&"invalidate".to_string()),
        "second call should be read"
    );
}

/// Item 55: uses configured `omp_bin`.
#[tokio::test]
async fn test_uses_configured_omp_bin() {
    let (_dir, db) = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::new(0);
    let b = OmpScavengeBackend {
        cfg: cfg_with(1800.0, "/custom/path/omp"),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    b.keep_fresh().await.unwrap();
    let c = calls.lock().unwrap();
    assert_eq!(c[0].0[0], "/custom/path/omp");
    assert_eq!(c[1].0[0], "/custom/path/omp");
}

/// Item 56: failed probe → false.
#[tokio::test]
async fn test_failed_probe_returns_false() {
    let (_dir, db) = make_agent_db(&[]).await;
    let (prober, _calls) = SharedProber::new(1); // rc=1 for both
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(!result);
}

/// Item 57: failed invalidate does not block the read attempt.
#[tokio::test]
async fn test_failed_invalidate_does_not_block_read() {
    let (_dir, db) = make_agent_db(&[]).await;
    let (prober, calls) = SharedProber::with_rcs(vec![1, 0]); // invalidate rc=1, read rc=0
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db: db.clone(),
        prober: Arc::new(prober),
    };
    let result = b.keep_fresh().await.unwrap();
    assert!(result);
    let c = calls.lock().unwrap();
    assert_eq!(c.len(), 2, "both calls should be attempted");
}

// ============================================================
// Prioritized verdict tests (monotonicity / prio override)
// ============================================================

/// Both normal and prioritized denied when empty windows.
#[tokio::test]
async fn test_prio_both_denied_empty_windows() {
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b
        .decide_with_windows(&BTreeMap::new(), 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&o.normal));
    assert!(is_denied(&o.prioritized));
}

/// Normal granted → prioritized == normal (or upgraded cap).
#[tokio::test]
async fn test_prio_granted_when_normal_granted() {
    let w = healthy_windows(0.05, 4.5);
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_granted(&o.normal));
    assert!(is_granted(&o.prioritized));
    // Prio headroom should be ≥ normal headroom.
    let nc = cap_tokens(&o.normal);
    let pc = cap_tokens(&o.prioritized);
    assert!(pc >= nc, "prio cap {pc:?} should be >= normal cap {nc:?}");
}

/// Normal denied by 7d pacing → prioritized gets prio override.
#[tokio::test]
async fn test_prio_override_7d_pacing() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    // 7d slightly over ramp: used .11, ramp .10 (resets near start).
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.11,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal), "normal should be denied");
    assert!(
        is_granted(&o.prioritized),
        "prioritized should be granted (prio override)"
    );
    assert!(
        reason(&o.prioritized).contains("prio override"),
        "reason should contain 'prio override': {}",
        reason(&o.prioritized)
    );
}

/// Normal denied by 5h pacing → prioritized gets prio override.
#[tokio::test]
async fn test_prio_override_5h_pacing() {
    let now = now_ms();
    // 5h halfway, usage just over ramp.
    let elapsed_ms = (2.75 * HOUR_MS as f64) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws("anthropic:5h", 0.55, "ok", resets, 60.0),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        is_granted(&o.prioritized),
        "prioritized should be granted: {:?}",
        o.prioritized
    );
}

/// 7d exhausted → prio NOT waived (exhaustion beats prio).
#[tokio::test]
async fn test_prio_exhausted_not_waived() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 1.0, "exhausted", now + WEEK_MS / 2, 60.0),
    );
    let b = make_backend(FakeLedger::new(0, 0));
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    assert!(is_denied(&o.normal));
    assert!(
        is_denied(&o.prioritized),
        "exhaustion should not be waived by prio"
    );
}

/// The 7d token cap uses the 7d capacity estimate when one exists.
///
/// An earlier revision of BACKEND-CONTRACT.md asserted the opposite —
/// that 7d tokens ALWAYS derive from the 5h capacity times the period
/// ratio, and told the port to reproduce that asymmetry verbatim. It was
/// a mis-transcription; `facade.py:224` reads
/// `estimate_capacity("anthropic:7d") or (cap_5h / _5H_7D_RATIO)`, so the
/// ratio is the fallback, not the rule.
///
/// Nothing covered this: the fake ledger reported no capacity for any
/// limit, so every test took the fallback and the branch the contract
/// described was never executed either way.
#[tokio::test]
async fn the_7d_cap_prefers_the_7d_estimate_over_the_5h_ratio() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws(
            "anthropic:7d",
            0.11,
            "ok",
            now + (0.9 * WEEK_MS as f64) as i64,
            60.0,
        ),
    );

    let mut ledger = FakeLedger::new(0, 0);
    // Deliberately unlike cap_5h / RATIO, so the two rules give different
    // answers and the assertion can tell them apart.
    ledger.capacity.insert("anthropic:5h".to_owned(), 200_000.0);
    ledger
        .capacity
        .insert("anthropic:7d".to_owned(), 9_000_000.0);

    let b = make_backend(ledger);
    let o = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();

    let cap = match &o.prioritized {
        hunter::backend::Verdict::Granted { cap_tokens, .. } => cap_tokens.expect("a token cap"),
        other @ hunter::backend::Verdict::Denied { .. } => {
            panic!("expected a prio override grant, got {other:?}")
        }
    };
    // headroom = 1.0 - used(0.11) = 0.89 of the 7d capacity.
    let from_estimate = (0.89 * 9_000_000.0) as i64;
    assert!(
        (cap - from_estimate).abs() <= 1,
        "cap {cap} should be 0.89 of the 7d estimate ({from_estimate}); \
         deriving it from the 5h capacity and the period ratio instead \
         gives a different number, which is the bug this pins"
    );
}

// ============================================================
// Cap vs gate: the anticipated job is counted once, not twice
// ============================================================

/// A grant must be big enough to pay for the job it grants.
///
/// The gate and the cap used to share one reservation, the one that
/// includes the anticipated spend. For the gate that is right — it asks
/// whether the window survives this job. For the cap it charges the job
/// for itself: the grant is then the headroom left over AFTER the job has
/// run, so a job that clears the gate by a hair is handed nearly nothing.
/// In production that shipped caps of a few thousand tokens against a
/// session floor around 32k, and the watchdog killed those workers seconds
/// in, having done no work.
///
/// Numbers here: 5h capacity 1M, used .69, ramp .90, anticipated 200k
/// (res .20) — the gate grants with .89 against .90, a margin of .01.
/// Headroom for the job is (.90 − .69) × 1M = 210k. Counting the job's
/// own 200k against it leaves (.90 − .89) × 1M = 10k.
#[tokio::test]
async fn test_granted_cap_covers_the_anticipated_job() {
    const ANTICIPATED: i64 = 200_000;
    let now = now_ms();
    let mut w = BTreeMap::new();
    // Elapsed 4.55h of 5h → ramp = (4.55 − 0.5) / 4.5 = 0.90.
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.69,
            "ok",
            now + (0.45 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    // Roomy 7d so the 5h window is the binding one.
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );

    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 1_000_000.0);

    let b = make_backend(ledger);
    let o = b
        .decide_with_windows(&w, ANTICIPATED, now_ms())
        .await
        .unwrap();

    assert!(
        is_granted(&o.normal),
        "expected Granted, got {:?}",
        o.normal
    );
    let cap = cap_tokens(&o.normal).expect("a granted cap");
    assert!(
        cap >= ANTICIPATED,
        "granted a {ANTICIPATED}-token job a cap of only {cap}: the cap \
         counted the anticipated spend against itself, so the watchdog \
         kills the worker long before it can do the work it was granted"
    );
    assert!(
        (cap - 210_000).abs() <= 100,
        "cap {cap} should be the 5h headroom, (ramp .90 − used .69) × 1M"
    );
}

/// Same double-count, the other place it lives: the prio override arm
/// inside a pacing denial, which caps at `1.0 − effective_used`.
///
/// 5h capacity 1M, used .55, ramp .50, anticipated 200k (res .20) → the
/// gate denies on .75 ≥ .50 and prio waives the ramp. The job's headroom
/// to the hard limit is (1.0 − .55) × 1M = 450k; charging it for its own
/// 200k leaves (1.0 − .75) × 1M = 250k.
#[tokio::test]
async fn test_prio_override_cap_excludes_the_anticipated_job() {
    const ANTICIPATED: i64 = 200_000;
    let now = now_ms();
    let mut w = BTreeMap::new();
    // Elapsed 2.75h of 5h → ramp = (2.75 − 0.5) / 4.5 = 0.50.
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.55,
            "ok",
            now + (2.25 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );

    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 1_000_000.0);

    let b = make_backend(ledger);
    let o = b
        .decide_with_windows(&w, ANTICIPATED, now_ms())
        .await
        .unwrap();

    assert!(
        is_denied(&o.normal),
        "normal should be denied by the 5h ramp"
    );
    assert!(
        reason(&o.prioritized).contains("prio override"),
        "expected a prio override, got {:?}",
        o.prioritized
    );
    let cap = cap_tokens(&o.prioritized).expect("a prio override cap");
    assert!(
        (cap - 450_000).abs() <= 2,
        "prio cap {cap} should be (1.0 − used .55) × 1M = 450000; counting \
         the anticipated 200k against the job gives 250000 instead"
    );
}

/// The headroom belongs to the window, not to the applicant: the cap a
/// grant carries cannot shrink as the anticipated cost grows. That shrink
/// IS the double count, seen from the other side. (The gate may still
/// refuse a bigger job; this pins only what a grant is worth.)
///
/// Port of `test_budget.py` `test_cap_is_the_same_headroom_whatever_the_job_anticipates`.
#[tokio::test]
async fn test_cap_is_the_same_headroom_whatever_the_job_anticipates() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    // Same state as the test above: ramp .90, used .69, capacity 1M.
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.69,
            "ok",
            now + (0.45 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 1_000_000.0);
    let b = make_backend(ledger);

    let cold = b.decide_with_windows(&w, 0, now_ms()).await.unwrap();
    let warm = b.decide_with_windows(&w, 200_000, now_ms()).await.unwrap();

    assert!(is_granted(&cold.normal), "{:?}", cold.normal);
    assert!(is_granted(&warm.normal), "{:?}", warm.normal);
    let (cold_cap, warm_cap) = (
        cap_tokens(&cold.normal).expect("cold cap"),
        cap_tokens(&warm.normal).expect("warm cap"),
    );
    assert!(
        (cold_cap - warm_cap).abs() <= 100,
        "the same window granted {cold_cap} to a free job and {warm_cap} to a \
         200k one: the cap is charging the applicant for its own cost"
    );
}

/// The 7d deny branch's override arm must not double-count either.
///
/// 7d capacity 10M, used .60, ramp .50, anticipated 200k (res .02) → normal
/// is denied on .62 ≥ .50 and prio overrides to the hard limit:
/// (1.0 − .60) × 10M = 4M. Charging the job for itself gives 3.8M. The 5h
/// window is given room to spare so the 7d arm is the one that binds.
///
/// Port of `test_budget.py` `test_7d_prio_override_cap_excludes_the_anticipated_job`.
#[tokio::test]
async fn test_7d_prio_override_cap_excludes_the_anticipated_job() {
    const ANTICIPATED: i64 = 200_000;
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.05,
            "ok",
            now + (0.5 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.60, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 100_000_000.0);
    ledger
        .capacity
        .insert("anthropic:7d".to_owned(), 10_000_000.0);
    let b = make_backend(ledger);

    let o = b
        .decide_with_windows(&w, ANTICIPATED, now_ms())
        .await
        .unwrap();

    assert!(is_denied(&o.normal), "{:?}", o.normal);
    assert!(reason(&o.normal).contains("7d"), "{:?}", o.normal);
    assert!(
        reason(&o.prioritized).contains("prio override"),
        "expected a prio override, got {:?}",
        o.prioritized
    );
    let cap = cap_tokens(&o.prioritized).expect("a prio override cap");
    assert!(
        (cap - 4_000_000).abs() <= 2,
        "prio cap {cap} should be (1.0 − used .60) × 10M = 4000000; counting \
         the anticipated 200k against the job gives 3800000 instead"
    );
}

/// Codex uses the shared two-window scavenging policy with its own OMP
/// limit IDs; its warning status retains the reported fraction.
#[tokio::test]
async fn codex_high_short_window_usage_denies() {
    let mut config = cfg();
    config.llm_provider = hunter::backends::omp_scavenge::LlmProvider::OpenAiCodex;
    let backend = make_backend_with(FakeLedger::new(0, 0), config, FakeProber::new(0));
    let now = now_ms();
    let mut windows = BTreeMap::new();
    windows.insert(
        "openai-codex:primary".to_owned(),
        ws("openai-codex:primary", 0.94, "warning", now + HOUR_MS, 60.0),
    );
    windows.insert(
        "openai-codex:secondary".to_owned(),
        ws(
            "openai-codex:secondary",
            0.15,
            "ok",
            now + WEEK_MS / 2,
            60.0,
        ),
    );

    let outlook = backend
        .decide_with_windows(&windows, 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&outlook.normal));
    assert!(
        reason(&outlook.normal).starts_with("openai-codex:primary:"),
        "{}",
        reason(&outlook.normal)
    );
}

/// The long-window pass reads Codex's own weekly limit, not the
/// Anthropic `:7d` substring: `openai-codex:secondary` over its ramp
/// denies even while the short window has room.
#[tokio::test]
async fn codex_long_window_over_its_ramp_denies() {
    let mut config = cfg();
    config.llm_provider = hunter::backends::omp_scavenge::LlmProvider::OpenAiCodex;
    let backend = make_backend_with(FakeLedger::new(0, 0), config, FakeProber::new(0));
    let now = now_ms();
    let mut windows = BTreeMap::new();
    windows.insert(
        "openai-codex:primary".to_owned(),
        ws("openai-codex:primary", 0.05, "ok", now + HOUR_MS, 60.0),
    );
    // Halfway through the week (ramp .50) at .80 used.
    windows.insert(
        "openai-codex:secondary".to_owned(),
        ws(
            "openai-codex:secondary",
            0.80,
            "ok",
            now + WEEK_MS / 2,
            60.0,
        ),
    );

    let outlook = backend
        .decide_with_windows(&windows, 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&outlook.normal), "{:?}", outlook.normal);
    assert!(
        reason(&outlook.normal).starts_with("openai-codex:secondary:"),
        "{:?}",
        outlook.normal
    );
}

// ============================================================
// Running jobs: their reservation is charged against every cap
// ============================================================
//
// The anticipated job is excluded from the cap (tests above); jobs that
// are ALREADY running are not, and every test above runs with none, so
// the `budget_*` term was never anything but zero there.

/// 5h capacity 1M, used .69, ramp .90, one running job reserving 100k
/// (.10): the grant is what is left of the ramp, (.90 − .69 − .10) × 1M.
#[tokio::test]
async fn test_running_jobs_shrink_the_5h_cap() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.69,
            "ok",
            now + (0.45 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(100_000, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 1_000_000.0);

    let o = make_backend(ledger)
        .decide_with_windows(&w, 0, now_ms())
        .await
        .unwrap();

    let cap = cap_tokens(&o.normal).expect("a granted cap");
    assert!((cap - 110_000).abs() <= 100, "cap {cap}, want 110000");
}

/// 7d capacity 10M, used .30, ramp .50, running 1M (.10), no 5h window:
/// the grant is (.50 − .30 − .10) × 10M.
#[tokio::test]
async fn test_running_jobs_shrink_the_7d_cap() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.30, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(1_000_000, 0);
    ledger
        .capacity
        .insert("anthropic:7d".to_owned(), 10_000_000.0);

    let o = make_backend(ledger)
        .decide_with_windows(&w, 0, now_ms())
        .await
        .unwrap();

    let cap = cap_tokens(&o.normal).expect("a granted cap");
    assert!((cap - 1_000_000).abs() <= 100, "cap {cap}, want 1000000");
}

/// 5h capacity 1M, used .55, ramp .50, running 100k (.10): denied, and
/// the prio override may spend up to the hard limit less what is running,
/// (1.0 − .55 − .10) × 1M.
#[tokio::test]
async fn test_running_jobs_shrink_the_5h_prio_override_cap() {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.55,
            "ok",
            now + (2.25 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.01, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(100_000, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 1_000_000.0);

    let o = make_backend(ledger)
        .decide_with_windows(&w, 0, now_ms())
        .await
        .unwrap();

    assert!(is_denied(&o.normal), "{:?}", o.normal);
    let cap = cap_tokens(&o.prioritized).expect("a prio override cap");
    assert!((cap - 350_000).abs() <= 2, "cap {cap}, want 350000");
}

/// 7d capacity 10M, used .60, ramp .50, running 1M (.10): the override
/// cap is (1.0 − .60 − .10) × 10M. The 5h window has room to spare.
#[tokio::test]
async fn test_running_jobs_shrink_the_7d_prio_override_cap() {
    let o = seven_day_override(0.60).await;
    assert!(is_denied(&o.normal), "{:?}", o.normal);
    let cap = cap_tokens(&o.prioritized).expect("a prio override cap");
    assert!((cap - 3_000_000).abs() <= 2, "cap {cap}, want 3000000");
}

/// Same, at .95 used: running work already fills the rest of the week, so
/// there is nothing for prio to hand out and it is denied, not granted a
/// zero-token cap.
#[tokio::test]
async fn test_7d_prio_override_denied_when_running_jobs_fill_the_window() {
    let o = seven_day_override(0.95).await;
    assert!(is_denied(&o.normal), "{:?}", o.normal);
    assert!(is_denied(&o.prioritized), "{:?}", o.prioritized);
}

async fn seven_day_override(used_7d: f64) -> hunter::backend::Outlook {
    let now = now_ms();
    let mut w = BTreeMap::new();
    w.insert(
        "anthropic:5h".to_owned(),
        ws(
            "anthropic:5h",
            0.05,
            "ok",
            now + (0.5 * HOUR_MS as f64) as i64,
            60.0,
        ),
    );
    w.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", used_7d, "ok", now + WEEK_MS / 2, 60.0),
    );
    let mut ledger = FakeLedger::new(1_000_000, 0);
    ledger
        .capacity
        .insert("anthropic:5h".to_owned(), 100_000_000.0);
    ledger
        .capacity
        .insert("anthropic:7d".to_owned(), 10_000_000.0);
    make_backend(ledger)
        .decide_with_windows(&w, 0, now_ms())
        .await
        .unwrap()
}

// ============================================================
// keep_fresh observations and calibration, against a real ledger
// ============================================================

/// What one `keep_fresh` wrote to a real `Store`: calibration samples as
/// `(limit_id, used_fraction_delta, hunter_tokens)` and its window
/// observations as `(limit_id, used_fraction)`, both by `limit_id`.
#[derive(Debug)]
struct Observed {
    samples: Vec<(String, f64, i64)>,
    logged: Vec<(String, Option<f64>)>,
}

/// `agent_rows` is what omp reports now; `earlier` the observations
/// already logged, as `(limit_id, used_fraction, resets_at, observed_at)`;
/// `spend` hunter's finished jobs as `(tokens_new, finished_at)`.
async fn keep_fresh_against_ledger(
    agent_rows: &[(&str, Option<f64>, &str, Option<i64>, i64)],
    earlier: &[(&str, f64, i64, i64)],
    spend: &[(i64, i64)],
) -> Observed {
    let (_agent_dir, agent_db) = make_agent_db(agent_rows).await;
    let dir = TempDir::new("facade-ledger");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    sqlx::raw_sql(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for &(lid, used, resets, observed_at) in earlier {
        sqlx::query(
            "INSERT INTO window_log (observed_at, limit_id, used_fraction, status, resets_at) \
             VALUES (?, ?, ?, 'ok', ?)",
        )
        .bind(observed_at)
        .bind(lid)
        .bind(used)
        .bind(resets)
        .execute(&pool)
        .await
        .unwrap();
    }
    for &(tokens, finished_at) in spend {
        sqlx::query(
            "INSERT INTO jobs (kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES ('hunt', 1, 'done', ?, ?, ?)",
        )
        .bind(tokens)
        .bind(finished_at - 60_000)
        .bind(finished_at)
        .execute(&pool)
        .await
        .unwrap();
    }

    let store = hunter::store::Store::connect(&path).await.unwrap();
    let b = OmpScavengeBackend {
        cfg: cfg(),
        ledger: Arc::new(store),
        agent_db,
        prober: Arc::new(FakeProber::new(0)),
    };
    let started = now_ms();
    b.keep_fresh().await.unwrap();

    let samples = sqlx::query_as(
        "SELECT limit_id, used_fraction_delta, hunter_tokens \
         FROM calibration_samples ORDER BY limit_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let logged = sqlx::query_as(
        "SELECT limit_id, used_fraction FROM window_log \
         WHERE observed_at >= ? ORDER BY limit_id",
    )
    .bind(started)
    .fetch_all(&pool)
    .await
    .unwrap();
    Observed { samples, logged }
}

/// Every window is logged; the short and long window grew since the
/// last observation in the same cycle while hunter spent 60k, so each
/// gets a calibration sample of its own growth. The per-model-class
/// window grew too, but has no period to calibrate against.
#[tokio::test]
async fn keep_fresh_logs_every_window_and_calibrates_short_and_long() {
    let now = now_ms();
    let (r5, r7) = (now + 2 * HOUR_MS, now + 72 * HOUR_MS);
    let observed = keep_fresh_against_ledger(
        &[
            ("anthropic:5h", Some(0.40), "ok", Some(r5), now - 60_000),
            ("anthropic:7d", Some(0.30), "ok", Some(r7), now - 60_000),
            (
                "anthropic:7d:fable",
                Some(0.50),
                "ok",
                Some(r7),
                now - 60_000,
            ),
        ],
        &[
            ("anthropic:5h", 0.25, r5, now - HOUR_MS),
            ("anthropic:7d", 0.28, r7, now - HOUR_MS),
            ("anthropic:7d:fable", 0.45, r7, now - HOUR_MS),
        ],
        &[(60_000, now - HOUR_MS / 2)],
    )
    .await;

    assert_eq!(
        observed.logged,
        vec![
            ("anthropic:5h".to_owned(), Some(0.40)),
            ("anthropic:7d".to_owned(), Some(0.30)),
            ("anthropic:7d:fable".to_owned(), Some(0.50)),
        ]
    );
    let samples: Vec<(&str, i64)> = observed
        .samples
        .iter()
        .map(|(lid, _, tokens)| (lid.as_str(), *tokens))
        .collect();
    assert_eq!(
        samples,
        [("anthropic:5h", 60_000), ("anthropic:7d", 60_000)]
    );
    assert!((observed.samples[0].1 - 0.15).abs() < 1e-9, "{observed:?}");
    assert!((observed.samples[1].1 - 0.02).abs() < 1e-9, "{observed:?}");
}

/// A sample needs all three: growth since the baseline, a baseline no
/// older than the window's period, and hunter tokens spent in between.
/// Each case below lacks exactly one, against the positive case above.
#[tokio::test]
async fn calibration_needs_growth_a_recent_baseline_and_hunter_spend() {
    let now = now_ms();
    let r5 = now + 2 * HOUR_MS;
    let current = [("anthropic:5h", Some(0.40), "ok", Some(r5), now - 60_000)];
    let spent = [(60_000, now - HOUR_MS / 2)];

    let no_growth = keep_fresh_against_ledger(
        &current,
        &[("anthropic:5h", 0.40, r5, now - HOUR_MS)],
        &spent,
    )
    .await;
    assert_eq!(no_growth.samples, [], "usage did not grow");

    let stale_baseline = keep_fresh_against_ledger(
        &current,
        &[("anthropic:5h", 0.25, r5, now - 6 * HOUR_MS)],
        &spent,
    )
    .await;
    assert_eq!(stale_baseline.samples, [], "baseline older than 5h");

    let no_spend =
        keep_fresh_against_ledger(&current, &[("anthropic:5h", 0.25, r5, now - HOUR_MS)], &[])
            .await;
    assert_eq!(no_spend.samples, [], "hunter spent nothing");
}

// ============================================================
// run: the usage-delta sandwich
// ============================================================

/// `usage_delta` is how far the long window moved while the worker ran.
/// The fake worker stands in for omp's usage mirror catching up: it swaps
/// a later agent.db in. It writes no session ledger, so the run itself is
/// flagged unmetered; that is `run_worker`'s business, not this test's.
#[tokio::test]
async fn run_reports_how_far_the_long_window_moved_during_the_job() {
    use std::os::unix::fs::PermissionsExt;

    let now = now_ms();
    let resets = now + WEEK_MS / 2;
    let (_before_dir, agent_db) =
        make_agent_db(&[("anthropic:7d", Some(0.30), "ok", Some(resets), now - 60_000)]).await;
    let (_after_dir, after_db) =
        make_agent_db(&[("anthropic:7d", Some(0.42), "ok", Some(resets), now - 1_000)]).await;
    let dir = TempDir::new("facade-run");
    let omp = dir.join("omp");
    std::fs::write(
        &omp,
        format!(
            "#!/bin/sh\ncp '{}' '{}'\n",
            after_db.display(),
            agent_db.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut config = cfg_with(1800.0, omp.to_str().unwrap());
    config.work_root = dir.subdir("work-root");
    config.poll_s = 0.02;
    let ws = hunter::workspace::Workspace::for_chain(&config.work_root, &dir.subdir("clone"), 1);
    std::fs::create_dir_all(&ws.tree).unwrap();
    let b = OmpScavengeBackend {
        cfg: config,
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db,
        prober: Arc::new(FakeProber::new(0)),
    };

    let rr = b
        .run(
            &ws,
            "prompt",
            Some(1_000_000),
            30,
            hunter::backend::JobClass::Hunt,
            None,
        )
        .await
        .unwrap();

    let delta = rr.usage_delta.expect("both snapshots saw a 7d window");
    assert!(
        (delta - 0.12).abs() < 1e-9,
        "usage_delta {delta}, want 0.12"
    );
}

// ============================================================
// status_html
// ============================================================

/// The status bars come from the configured provider's rows only: a Codex
/// daemon sharing an agent.db with Anthropic usage shows its own two
/// windows, labelled by role.
#[tokio::test]
async fn codex_status_html_shows_only_codex_windows() {
    let now = now_ms();
    let (_dir, agent_db) = make_agent_db(&[
        (
            "anthropic:5h",
            Some(0.50),
            "ok",
            Some(now + HOUR_MS),
            now - 60_000,
        ),
        (
            "openai-codex:primary",
            Some(0.94),
            "warning",
            Some(now + HOUR_MS),
            now - 60_000,
        ),
        (
            "openai-codex:secondary",
            Some(0.15),
            "ok",
            Some(now + WEEK_MS / 2),
            now - 60_000,
        ),
    ])
    .await;
    let mut config = cfg();
    config.llm_provider = hunter::backends::omp_scavenge::LlmProvider::OpenAiCodex;
    let b = OmpScavengeBackend {
        cfg: config,
        ledger: Arc::new(FakeLedger::new(0, 0)),
        agent_db,
        prober: Arc::new(FakeProber::new(0)),
    };

    let html = b.status_html().await.unwrap();

    assert_eq!(
        html.matches(r#"<div class="scv-win">"#).count(),
        2,
        "{html}"
    );
    assert!(html.contains("<b>5h window</b><span>94% used"), "{html}");
    assert!(html.contains("<b>7d window</b><span>15% used"), "{html}");
    assert!(!html.contains("50% used"), "Anthropic row leaked: {html}");
}

/// Gate and cap judge the same instant. The cap used to re-read the wall
/// clock, so a window that expired between the window read and that second
/// read stopped capping: here, at a decision instant long past, both
/// windows would look expired (5h skipped, 7d ramp 1.0) and the grant
/// would be capped by 0.9 of the week, not by the 5h ramp.
#[tokio::test]
async fn gate_and_cap_judge_the_same_instant() {
    const DECIDED_AT: i64 = 1_700_000_000_000;
    let mut windows = BTreeMap::new();
    windows.insert(
        "anthropic:5h".to_owned(),
        // Elapsed 4.5h of 5h: ramp (4.5h - 0.5h) / 4.5h = 0.889.
        ws("anthropic:5h", 0.5, "ok", DECIDED_AT + HOUR_MS / 2, 60.0),
    );
    windows.insert(
        "anthropic:7d".to_owned(),
        ws("anthropic:7d", 0.10, "ok", DECIDED_AT + WEEK_MS / 2, 60.0),
    );

    let o = make_backend(FakeLedger::new(0, 0))
        .decide_with_windows(&windows, 0, DECIDED_AT)
        .await
        .unwrap();

    // (0.889 - 0.5) of the 2M fallback 5h capacity.
    let cap = cap_tokens(&o.normal).expect("granted with a cap");
    assert!((cap - 777_777).abs() <= 2, "normal cap {cap}");
    // Prioritized: (1.0 - 0.5) of the same 5h capacity.
    let prio = cap_tokens(&o.prioritized).expect("granted with a cap");
    assert!((prio - 1_000_000).abs() <= 2, "prioritized cap {prio}");
}

fn copilot_backend(ledger: FakeLedger) -> OmpScavengeBackend {
    let mut config = cfg();
    config.llm_provider = hunter::backends::omp_scavenge::LlmProvider::GitHubCopilot;
    make_backend_with(ledger, config, FakeProber::new(0))
}

fn current_month_bounds(now: i64) -> (i64, i64) {
    use chrono::Datelike;

    let start = chrono::DateTime::from_timestamp_millis(now)
        .unwrap()
        .date_naive()
        .with_day(1)
        .unwrap();
    let reset = start.checked_add_months(chrono::Months::new(1)).unwrap();
    (
        start
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
        reset
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis(),
    )
}

/// Copilot has only a monthly quota: normal spending tracks elapsed calendar
/// time, while priority may use the remaining quota without bypassing its cap.
#[tokio::test]
async fn copilot_monthly_pacing_and_prioritized_caps() {
    const CALIBRATED_CAPACITY: f64 = 10_000_000.0;
    let now = now_ms();
    let (start, reset) = current_month_bounds(now);
    let elapsed = (now - start) as f64 / (reset - start) as f64;
    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("copilot:premium".to_owned(), CALIBRATED_CAPACITY);
    let backend = copilot_backend(ledger);
    let over_ramp = f64::midpoint(1.0, elapsed);
    let windows = BTreeMap::from([(
        "copilot:premium".to_owned(),
        ws("copilot:premium", over_ramp, "ok", reset, 60.0),
    )]);

    let outlook = backend.decide_with_windows(&windows, 0, now).await.unwrap();
    assert!(is_denied(&outlook.normal), "{:?}", outlook.normal);
    let priority_cap = cap_tokens(&outlook.prioritized).expect("priority may bypass pacing");
    let expected_priority_cap = ((1.0 - over_ramp) * CALIBRATED_CAPACITY) as i64;
    assert!(
        (priority_cap - expected_priority_cap).abs() <= 2,
        "priority cap {priority_cap}, expected {expected_priority_cap}"
    );
    let retry = retry_at(&outlook.normal).expect("monthly pacing provides a retry time");
    let expected_retry = start as f64 + over_ramp * (reset - start) as f64;
    assert!(
        (retry - expected_retry).abs() < 1.0,
        "monthly retry {retry}, expected {expected_retry}"
    );

    let under_ramp = elapsed / 2.0;
    let windows = BTreeMap::from([(
        "copilot:premium".to_owned(),
        ws("copilot:premium", under_ramp, "ok", reset, 60.0),
    )]);
    let outlook = backend.decide_with_windows(&windows, 0, now).await.unwrap();
    let normal_cap = cap_tokens(&outlook.normal).expect("usage below monthly pacing is granted");
    let expected_normal_cap = ((elapsed - under_ramp) * CALIBRATED_CAPACITY) as i64;
    assert!(
        (normal_cap - expected_normal_cap).abs() <= 1,
        "monthly cap {normal_cap}, expected {expected_normal_cap}"
    );
    assert!(is_granted(&outlook.prioritized));
    assert!(cap_tokens(&outlook.prioritized).unwrap() >= normal_cap);
}

/// Before any month has been calibrated, Copilot's token capacity is the
/// long-window fallback of 67.2M, and a grant below the ramp is sized from
/// it. Every other Copilot test injects a calibrated capacity.
#[tokio::test]
async fn copilot_uncalibrated_capacity_is_the_long_window_fallback() {
    let at = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    };
    let (start, now, reset) = (
        at("2024-02-01T00:00:00Z"),
        at("2024-02-15T00:00:00Z"),
        at("2024-03-01T00:00:00Z"),
    );
    let elapsed = (now - start) as f64 / (reset - start) as f64;
    let used = elapsed / 2.0;
    let windows = BTreeMap::from([(
        "copilot:premium".to_owned(),
        ws("copilot:premium", used, "ok", reset, 60.0),
    )]);

    let outlook = copilot_backend(FakeLedger::new(0, 0))
        .decide_with_windows(&windows, 0, now)
        .await
        .unwrap();

    let cap = cap_tokens(&outlook.normal).expect("usage below the ramp is granted");
    let expected = ((elapsed - used) * 67_200_000.0) as i64;
    assert!(
        (cap - expected).abs() <= 1,
        "cap {cap}, expected {expected}"
    );
}

/// A monthly observation stops funding work exactly at its reset, even if
/// it was read before the boundary and still reports unused premium quota.
#[tokio::test]
async fn copilot_monthly_quota_expires_at_the_exact_reset() {
    let reset = chrono::DateTime::parse_from_rfc3339("2024-03-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let windows = BTreeMap::from([(
        "copilot:premium".to_owned(),
        ws("copilot:premium", 0.25, "ok", reset, 0.0),
    )]);
    let mut ledger = FakeLedger::new(0, 0);
    ledger
        .capacity
        .insert("copilot:premium".to_owned(), 1_000_000.0);
    let backend = copilot_backend(ledger);

    let active = backend
        .decide_with_windows(&windows, 50_000, reset - 1)
        .await
        .unwrap();
    assert!(is_granted(&active.normal), "{:?}", active.normal);
    assert!(is_granted(&active.prioritized), "{:?}", active.prioritized);
    assert!(cap_tokens(&active.normal).unwrap() >= 50_000);

    for now in [reset, reset + 1] {
        let expired = backend
            .decide_with_windows(&windows, 50_000, now)
            .await
            .unwrap();
        assert!(is_denied(&expired.normal), "{:?}", expired.normal);
        assert!(is_denied(&expired.prioritized), "{:?}", expired.prioritized);
    }
}

/// The provider's exhausted status is authoritative even when its reported
/// request fraction is below one; priority cannot waive actual exhaustion.
#[tokio::test]
async fn copilot_exhausted_status_refuses_normal_and_prioritized() {
    let (_, reset) = current_month_bounds(now_ms());
    let windows = BTreeMap::from([(
        "copilot:premium".to_owned(),
        ws("copilot:premium", 0.1, "exhausted", reset, 60.0),
    )]);
    let outlook = copilot_backend(FakeLedger::new(0, 0))
        .decide_with_windows(&windows, 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&outlook.normal), "{:?}", outlook.normal);
    assert!(is_denied(&outlook.prioritized), "{:?}", outlook.prioritized);
}

/// Missing request fraction or monthly reset cannot be treated as spare
/// capacity, including the no-limits response on some Copilot accounts.
#[tokio::test]
async fn copilot_unknown_quota_refuses_normal_and_prioritized() {
    let (_, reset) = current_month_bounds(now_ms());
    let backend = copilot_backend(FakeLedger::new(0, 0));
    for (fraction, reset) in [(None, Some(reset)), (Some(0.0), None), (None, None)] {
        let mut monthly = ws(
            "copilot:premium",
            0.0,
            "ok",
            reset.unwrap_or_default(),
            60.0,
        );
        monthly.used_fraction = fraction;
        monthly.resets_at = reset;
        let windows = BTreeMap::from([("copilot:premium".to_owned(), monthly)]);
        let outlook = backend
            .decide_with_windows(&windows, 0, now_ms())
            .await
            .unwrap();
        assert!(is_denied(&outlook.normal), "{:?}", outlook.normal);
        assert!(is_denied(&outlook.prioritized), "{:?}", outlook.prioritized);
    }
    let outlook = backend
        .decide_with_windows(&BTreeMap::new(), 0, now_ms())
        .await
        .unwrap();
    assert!(is_denied(&outlook.normal));
    assert!(is_denied(&outlook.prioritized));
}

/// A monthly-only provider uses its premium observation's age, not a missing
/// short window or a fresher foreign provider's observation.
#[test]
fn copilot_monthly_freshness_uses_configured_stale_threshold() {
    let (_, reset) = current_month_bounds(now_ms());
    let mut config = cfg_with(300.0, "omp");
    config.llm_provider = hunter::backends::omp_scavenge::LlmProvider::GitHubCopilot;
    let backend = make_backend_with(FakeLedger::new(0, 0), config, FakeProber::new(0));
    for (age, expected_fresh) in [(60.0, true), (300.0, true), (300.001, false)] {
        let windows = BTreeMap::from([
            (
                "copilot:premium".to_owned(),
                ws("copilot:premium", 0.1, "ok", reset, age),
            ),
            (
                "anthropic:5h".to_owned(),
                ws("anthropic:5h", 0.0, "ok", now_ms() + FIVE_H_MS, 0.0),
            ),
        ]);
        assert_eq!(
            backend.is_fresh(&windows),
            expected_fresh,
            "monthly observation age {age}"
        );
    }
    let foreign_only = healthy_windows(0.0, 4.5);
    assert!(!backend.is_fresh(&foreign_only));
}

// ---- worker_hosts: what the reachability gate probes -----------------------

/// A worker talks to its provider's API, so that is what is probed.
#[test]
fn worker_hosts_are_the_configured_providers() {
    use hunter::backends::omp_scavenge::LlmProvider;
    for (provider, host) in [
        (LlmProvider::Anthropic, "api.anthropic.com"),
        (LlmProvider::OpenAiCodex, "chatgpt.com"),
        (LlmProvider::GitHubCopilot, "api.githubcopilot.com"),
    ] {
        let cfg = Config {
            llm_provider: provider,
            ..cfg()
        };
        let backend = make_backend_with(FakeLedger::new(0, 0), cfg, FakeProber::new(0));
        assert_eq!(backend.worker_hosts(), [host], "{provider:?}");
    }
}

/// `backend.providerHosts` replaces the provider's hosts outright: an
/// operator behind a gateway names the gateway, and the provider's own
/// host is then not one the worker needs.
#[test]
fn configured_provider_hosts_replace_the_providers() {
    let cfg = Config {
        provider_hosts: Some(vec!["llm-gw.corp:8443".to_owned()]),
        ..cfg()
    };
    let backend = make_backend_with(FakeLedger::new(0, 0), cfg, FakeProber::new(0));
    assert_eq!(backend.worker_hosts(), ["llm-gw.corp:8443"]);
}
