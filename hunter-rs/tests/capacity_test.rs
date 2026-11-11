#![allow(
    clippy::type_complexity,
    clippy::needless_pass_by_value,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]
//! Capacity tests — pure ramp/retry math (§4 items 28-42) and `read_windows`
//! with agent.db fixtures (items 22-26 + edge cases).

use std::path::PathBuf;

use hunter::backends::omp_scavenge::LlmProvider;
use hunter::backends::omp_scavenge::capacity::*;

mod support;
use support::TempDir;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// ============================================================
// ramp_7d (§4 items 28-32)
// ============================================================

#[test]
fn test_ramp_7d_none_returns_1() {
    assert_eq!(ramp_linear(None, WEEK_MS, now_ms()), 1.0);
}

#[test]
fn test_ramp_7d_expired_returns_1() {
    let now = now_ms();
    assert_eq!(ramp_linear(Some(now - 1000), WEEK_MS, now), 1.0);
}

#[test]
fn test_ramp_7d_midpoint_returns_half() {
    let now = now_ms();
    let resets = now + WEEK_MS / 2;
    let r = ramp_linear(Some(resets), WEEK_MS, now);
    assert!((r - 0.5).abs() < 1e-6, "expected ~0.5, got {r}");
}

#[test]
fn test_ramp_7d_start_returns_zero() {
    let now = now_ms();
    let resets = now + WEEK_MS;
    let r = ramp_linear(Some(resets), WEEK_MS, now);
    assert!((r - 0.0).abs() < 1e-6, "expected ~0.0, got {r}");
}

#[test]
fn test_ramp_7d_bad_data_clamped() {
    let now = now_ms();
    let resets = now + 2 * WEEK_MS;
    let r = ramp_linear(Some(resets), WEEK_MS, now);
    assert!(r <= 1.0, "expected ≤1.0, got {r}");
}

// ============================================================
// ramp_5h (§4 items 33-37)
// ============================================================

#[test]
fn test_ramp_5h_none_returns_none() {
    assert!(ramp_after_headroom(None, FIVE_H_MS, now_ms()).is_none());
}

#[test]
fn test_ramp_5h_expired_returns_none() {
    let now = now_ms();
    assert!(ramp_after_headroom(Some(now - 1000), FIVE_H_MS, now).is_none());
}

#[test]
fn test_ramp_5h_15min_elapsed_returns_zero() {
    let now = now_ms();
    // 15 min elapsed: resets_at = now + (5h - 15m)
    let resets = now + FIVE_H_MS - 15 * 60 * 1000;
    let r = ramp_after_headroom(Some(resets), FIVE_H_MS, now).expect("should be Some");
    assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {r}");
}

#[test]
fn test_ramp_5h_halfway_returns_half() {
    let now = now_ms();
    // 2.75h elapsed: resets_at = now + (5h - 2.75h) = now + 2.25h
    let elapsed_ms = (2.75 * 3_600_000.0) as i64;
    let resets = now + FIVE_H_MS - elapsed_ms;
    let r = ramp_after_headroom(Some(resets), FIVE_H_MS, now).expect("should be Some");
    assert!((r - 0.5).abs() < 1e-6, "expected ~0.5, got {r}");
}

#[test]
fn test_ramp_5h_just_started_never_negative() {
    let now = now_ms();
    let resets = now + FIVE_H_MS; // just started, 0 elapsed
    let r = ramp_after_headroom(Some(resets), FIVE_H_MS, now).expect("should be Some");
    assert!(r >= 0.0, "expected ≥0.0, got {r}");
    assert!((r - 0.0).abs() < 1e-9, "expected 0.0, got {r}");
}

// ============================================================
// retry_at_7d (§4 items 38-39)
// ============================================================

#[test]
fn test_retry_at_7d_none_returns_none() {
    assert!(retry_at_linear(None, WEEK_MS, 0.5).is_none());
}

#[test]
fn test_retry_at_7d_round_trip() {
    let now = now_ms();
    let resets = now + (0.4 * WEEK_MS as f64) as i64;
    for u in [0.0, 0.1, 0.5, 0.9] {
        let rt = retry_at_linear(Some(resets), WEEK_MS, u).expect("should be Some");
        let back = ramp_linear(Some(resets), WEEK_MS, rt as i64);
        assert!(
            (back - u).abs() < 1e-9,
            "round-trip failed for u={u}: ramp_linear(resets, WEEK_MS, retry_at_linear(resets, WEEK_MS, {u})) = {back}"
        );
    }
}

// ============================================================
// retry_at_5h (§4 items 40-42)
// ============================================================

#[test]
fn test_retry_at_5h_none_returns_none() {
    assert!(retry_at_after_headroom(None, FIVE_H_MS, 0.5).is_none());
}

#[test]
fn test_retry_at_5h_round_trip() {
    let now = now_ms();
    let resets = now + (3.2 * 3_600_000.0) as i64;
    for u in [0.0, 0.25, 0.5, 0.9] {
        let rt = retry_at_after_headroom(Some(resets), FIVE_H_MS, u).expect("should be Some");
        let back = ramp_after_headroom(Some(resets), FIVE_H_MS, rt as i64).expect("should be Some");
        assert!(
            (back - u).abs() < 1e-9,
            "round-trip failed for u={u}: ramp_after_headroom(resets, FIVE_H_MS, retry_at_after_headroom(resets, FIVE_H_MS, {u})) = {back}"
        );
    }
}

#[test]
fn test_retry_at_5h_zero_equals_window_start_plus_headroom() {
    let now = now_ms();
    let resets = now + 4 * 3_600_000; // resets in 4h → elapsed 1h
    let rt = retry_at_after_headroom(Some(resets), FIVE_H_MS, 0.0).expect("should be Some");
    let window_start = (resets - FIVE_H_MS) as f64;
    let expected = window_start + HEADROOM_MS as f64;
    assert!(
        (rt - expected).abs() < 1.0,
        "expected retry_at_after_headroom(resets, FIVE_H_MS, 0) = window_start + HEADROOM; got {rt}, expected {expected}"
    );
}

// ============================================================
// read_windows with agent.db fixture (§4 items 22-26 + edges)
// ============================================================

/// Create a fixture agent.db with `usage_history` rows.
/// Each row: (`limit_id`, `used_fraction`, status, `resets_at`, `recorded_at`).
async fn make_agent_db(
    dir: &TempDir,
    rows: &[(&str, Option<f64>, &str, Option<i64>, i64)],
) -> PathBuf {
    make_agent_db_with_provider(dir, "anthropic", rows).await
}

async fn make_agent_db_with_provider(
    dir: &TempDir,
    provider: &str,
    rows: &[(&str, Option<f64>, &str, Option<i64>, i64)],
) -> PathBuf {
    use sqlx::sqlite::SqliteConnectOptions;

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
    path
}

#[tokio::test]
async fn test_read_windows_no_db_returns_empty() {
    let db = PathBuf::from("/nonexistent/agent.db");
    let now = now_ms();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    assert!(windows.is_empty());
}

#[tokio::test]
async fn test_read_windows_empty_table_returns_empty() {
    let dir = TempDir::new("cap-empty");
    let db_path = make_agent_db(&dir, &[]).await;
    let now = now_ms();
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    assert!(windows.is_empty());
}

/// §4 item 23: single live 7d row kept.
#[tokio::test]
async fn test_read_windows_keeps_active_window() {
    let now = now_ms();
    let dir = TempDir::new("cap-active");
    let db_path = make_agent_db(
        &dir,
        &[(
            "anthropic:7d",
            Some(0.3),
            "ok",
            Some(now + WEEK_MS / 2),
            now - 60_000,
        )],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    assert!(windows.contains_key("anthropic:7d"));
    let w = &windows["anthropic:7d"];
    assert!((w.used_fraction.unwrap() - 0.3).abs() < 1e-9);
}

/// §4 item 22: expired per-model-class row dropped, active 7d kept.
#[tokio::test]
async fn test_read_windows_drops_expired_model_class() {
    let now = now_ms();
    let dir = TempDir::new("cap-model-class");
    let db_path = make_agent_db(
        &dir,
        &[
            (
                "anthropic:7d",
                Some(0.3),
                "ok",
                Some(now + WEEK_MS / 2),
                now - 60_000,
            ),
            (
                "anthropic:7d:fable",
                Some(0.56),
                "ok",
                Some(now - 26 * 24 * 3_600_000), // expired 26 days ago
                now - 26 * 24 * 3_600_000,
            ),
        ],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let keys: Vec<&String> = windows.keys().collect();
    assert_eq!(keys, vec!["anthropic:7d"]);
}

/// An ACTIVE per-model-class row is read like the account-wide ones.
/// `decide` gates on every `:7d` window it is handed, so a reader that
/// fetched only `anthropic:5h` and `anthropic:7d` would silently stop
/// enforcing the per-model weekly limit.
#[tokio::test]
async fn test_read_windows_keeps_active_model_class() {
    let now = now_ms();
    let dir = TempDir::new("cap-model-class-active");
    let resets = Some(now + WEEK_MS / 2);
    let db_path = make_agent_db(
        &dir,
        &[
            ("anthropic:7d", Some(0.3), "ok", resets, now - 60_000),
            ("anthropic:7d:fable", Some(0.96), "ok", resets, now - 60_000),
        ],
    )
    .await;
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db_path, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let keys: Vec<&String> = windows.keys().collect();
    assert_eq!(keys, vec!["anthropic:7d", "anthropic:7d:fable"]);
    assert_eq!(windows["anthropic:7d:fable"].used_fraction, Some(0.96));
}

/// §4 item 24: expired 5h window rolled forward.
#[tokio::test]
async fn test_read_windows_rolls_forward_expired_5h() {
    let now = now_ms();
    // 5h window expired 47 min ago.
    let old_resets = now - 47 * 60 * 1000;
    let old_recorded = old_resets - 3_600_000; // recorded 1h before that reset
    let dir = TempDir::new("cap-roll-5h");
    let db_path = make_agent_db(
        &dir,
        &[(
            "anthropic:5h",
            Some(0.36),
            "ok",
            Some(old_resets),
            old_recorded,
        )],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let w = &windows["anthropic:5h"];
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
    assert_eq!(w.status.as_deref(), Some("ok"));
    // resets = old + _5H_MS
    assert_eq!(w.resets_at, Some(old_resets + FIVE_H_MS));
    // recorded_at = old resets (current cycle's actual start)
    assert_eq!(w.recorded_at, old_resets);
    // age is measured from the ROLLED recorded_at, not the original. It
    // drives staleness, which gates spending, so a rolled window that
    // looked fresher than it is would let hunter spend on stale data.
    assert!(
        (w.age_s - 47.0 * 60.0).abs() < 1.0,
        "age_s should be time since the current cycle started, got {}",
        w.age_s
    );
}

/// §4 item 25: expired 7d window rolled forward.
#[tokio::test]
async fn test_read_windows_rolls_forward_expired_7d() {
    let now = now_ms();
    let old_resets = now - 2 * 3_600_000; // expired 2h ago
    let dir = TempDir::new("cap-roll-7d");
    let db_path = make_agent_db(
        &dir,
        &[(
            "anthropic:7d",
            Some(0.55),
            "ok",
            Some(old_resets),
            old_resets - 3_600_000,
        )],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let w = &windows["anthropic:7d"];
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
    assert_eq!(w.resets_at, Some(old_resets + WEEK_MS));
    assert_eq!(w.recorded_at, old_resets);
}

/// §4 item 26: rolls forward through multiple missed cycles.
#[tokio::test]
async fn test_read_windows_rolls_forward_multiple_missed_cycles() {
    let now = now_ms();
    // 5h expired 2.3 periods ago.
    let old_resets = now - (2.3 * FIVE_H_MS as f64) as i64;
    let dir = TempDir::new("cap-roll-multi");
    let db_path = make_agent_db(
        &dir,
        &[(
            "anthropic:5h",
            Some(0.80),
            "ok",
            Some(old_resets),
            old_resets - 1_000_000,
        )],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let w = &windows["anthropic:5h"];
    assert!(w.resets_at.unwrap() > now, "resets should be in the future");
    assert!(
        w.resets_at.unwrap() - now <= FIVE_H_MS,
        "resets - now should be ≤ 5h (current cycle)"
    );
    assert!((w.used_fraction.unwrap() - 0.0).abs() < 1e-9);
}

/// NULL `resets_at` rows are kept verbatim.
#[tokio::test]
async fn test_read_windows_null_resets_kept() {
    let now = now_ms();
    let dir = TempDir::new("cap-null-resets");
    let db_path = make_agent_db(
        &dir,
        &[("anthropic:5h", Some(0.10), "ok", None, now - 60_000)],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let w = &windows["anthropic:5h"];
    assert_eq!(w.resets_at, None);
    assert!((w.used_fraction.unwrap() - 0.10).abs() < 1e-9);
}

/// Some(0) for `resets_at` is treated as falsy (kept verbatim, no rollover).
#[tokio::test]
async fn test_read_windows_zero_resets_kept() {
    let now = now_ms();
    let dir = TempDir::new("cap-zero-resets");
    let db_path = make_agent_db(
        &dir,
        &[("anthropic:5h", Some(0.10), "ok", Some(0), now - 60_000)],
    )
    .await;
    let db = db_path.clone();
    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::Anthropic, now))
            .await
            .unwrap();
    let w = &windows["anthropic:5h"];
    assert_eq!(w.resets_at, Some(0));
    assert!((w.used_fraction.unwrap() - 0.10).abs() < 1e-9);
}

// ============================================================
// Edge cases: Some(0) behaves like None (falsy)
// ============================================================

#[test]
fn test_ramp_7d_zero_resets_returns_1() {
    assert_eq!(ramp_linear(Some(0), WEEK_MS, now_ms()), 1.0);
}

#[test]
fn test_ramp_5h_zero_resets_returns_none() {
    assert!(ramp_after_headroom(Some(0), FIVE_H_MS, now_ms()).is_none());
}

#[test]
fn test_retry_at_7d_zero_returns_none() {
    assert!(retry_at_linear(Some(0), WEEK_MS, 0.5).is_none());
}

#[test]
fn test_retry_at_5h_zero_returns_none() {
    assert!(retry_at_after_headroom(Some(0), FIVE_H_MS, 0.5).is_none());
}

// ============================================================
// Provider quota tables
// ============================================================

/// The window order is policy (first denial wins, the shortest window
/// keys freshness, the longest one the usage delta), and a fallback
/// capacity pointing at a missing window would resolve to 0 and stop
/// reservations from counting. Both must hold for every provider before
/// any calibration history exists.
#[test]
fn every_quota_is_ordered_longest_first_with_positive_fallbacks() {
    use hunter::backends::omp_scavenge::provider::Period;
    // A calendar month at its shortest, so the order holds for every month.
    let shortest_ms = |period: Period| match period {
        Period::Fixed(period_ms) => period_ms,
        Period::CalendarMonth => 28 * 24 * 3_600_000,
    };
    for provider in LlmProvider::ALL {
        let quota = provider.quota();
        assert!(!quota.windows.is_empty(), "{provider:?} has no windows");
        assert!(
            quota
                .windows
                .windows(2)
                .all(|pair| shortest_ms(pair[0].period) > shortest_ms(pair[1].period)),
            "{provider:?} windows are not longest first"
        );
        let no_history = vec![None; quota.windows.len()];
        for (index, window) in quota.windows.iter().enumerate() {
            let capacity = quota.capacity(index, &no_history);
            assert!(
                capacity > 0.0,
                "{provider:?} {}: fallback capacity {capacity}",
                window.limit_id
            );
        }
    }
}

// ============================================================
// read_windows failure visibility
// ============================================================

/// `read_windows` falls back to an empty map on failure, which downstream
/// is indistinguishable from "no window data": `decide` denies until fresh
/// and `keep_fresh` probes. The log line at that boundary is the only
/// thing separating a broken agent.db read from an idle daemon, so it is
/// part of the contract.
///
/// Same shape as `post_test.rs`'s sink: a poisoned lock is recovered
/// rather than unwrapped, so a panic mid-test surfaces as that panic.
#[derive(Clone, Default)]
struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl LogSink {
    fn buf(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.buf()).into_owned()
    }
}

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for LogSink {
    type Writer = Self;

    fn make_writer(&self) -> Self {
        self.clone()
    }
}

/// Run `f` with a thread-local log capture. Thread-local rather than
/// global so it also works inside `spawn_blocking`, where `read_windows`
/// has to be called (its `block_on` cannot run on a runtime thread).
fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    let sink = LogSink::default();
    let capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish(),
    );
    let out = f();
    drop(capture);
    (out, sink.text())
}

#[tokio::test]
async fn test_read_windows_unreadable_db_is_logged() {
    let dir = TempDir::new("cap-corrupt");
    let db_path = dir.join("agent.db");
    // Exists, so the missing-file shortcut does not fire, but it is not a
    // database — the same shape as a truncated or foreign file.
    std::fs::write(&db_path, b"this is not a sqlite database").unwrap();
    let now = now_ms();
    let db = db_path.clone();
    let (windows, logs) = tokio::task::spawn_blocking(move || {
        capture_logs(|| read_windows(&db, LlmProvider::Anthropic, now))
    })
    .await
    .unwrap();
    assert!(windows.is_empty(), "fallback must stay an empty map");
    assert!(
        logs.contains("reading omp usage windows failed"),
        "failure must be logged, got: {logs}"
    );
    assert!(
        logs.contains(&db_path.display().to_string()),
        "log must name the path, got: {logs}"
    );
}

#[test]
fn test_read_windows_without_runtime_is_logged() {
    let db = PathBuf::from("/nonexistent/agent.db");
    let (windows, logs) = capture_logs(|| read_windows(&db, LlmProvider::Anthropic, now_ms()));
    assert!(windows.is_empty());
    assert!(
        logs.contains("outside a Tokio runtime"),
        "missing runtime must be logged, got: {logs}"
    );
}

/// An agent DB with no rows yet is not a failure: it must stay silent, or
/// the log fills with noise on a fresh install.
#[tokio::test]
async fn test_read_windows_empty_sources_log_nothing() {
    let dir = TempDir::new("cap-quiet");
    let empty_db = make_agent_db(&dir, &[]).await;
    let missing = PathBuf::from("/nonexistent/agent.db");
    let now = now_ms();
    let ((), logs) = tokio::task::spawn_blocking(move || {
        capture_logs(|| {
            read_windows(&missing, LlmProvider::Anthropic, now);
            read_windows(&empty_db, LlmProvider::Anthropic, now);
        })
    })
    .await
    .unwrap();
    assert!(
        logs.is_empty(),
        "legitimately-empty case must be silent: {logs}"
    );
}

/// Append a row whose `recorded_at` holds TEXT. Raw SQL rather than a
/// bind, because sqlx types the parameter: SQLite's INTEGER affinity
/// leaves a non-numeric string as TEXT, which is what makes the cell
/// undecodable as `i64`.
async fn insert_undecodable_row(path: &std::path::Path) {
    let pool = support::open_pool(path).await;
    sqlx::raw_sql(
        "INSERT INTO usage_history \
         (recorded_at, provider, account_key, limit_id, label, \
          used_fraction, status, resets_at) \
         VALUES ('not-a-timestamp', 'anthropic', 'acct', 'anthropic:5h', \
                 '5h', 0.42, 'ok', NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
}

/// omp owns `agent.db`, so every cell in it is foreign input. A value
/// that will not decode as the requested Rust type must reach the logged
/// fallback, not abort the read: the read runs inside `spawn_blocking`,
/// so a panic surfaces to callers as a `JoinError` through `.await?` and
/// takes down `/api/summary` on every poll plus every worker's
/// pre-snapshot, while the error log for exactly this case never runs.
///
/// The good row is present on purpose: one undecodable cell discards the
/// whole snapshot. A map that merely lost its `anthropic:5h` row still
/// looks authoritative to `decide_inner`, which refuses only when the
/// map is empty, so it would grant spending against a window that can no
/// longer be read. Empty is the designed "capacity unknown" state.
#[tokio::test]
async fn test_read_windows_undecodable_row_degrades() {
    let now = now_ms();
    let dir = TempDir::new("cap-baddecode");
    let db_path = make_agent_db(
        &dir,
        &[("anthropic:7d", Some(0.2), "ok", Some(now + WEEK_MS), now)],
    )
    .await;
    insert_undecodable_row(&db_path).await;

    let db = db_path.clone();
    let (windows, logs) = tokio::task::spawn_blocking(move || {
        capture_logs(|| read_windows(&db, LlmProvider::Anthropic, now))
    })
    .await
    .expect("an undecodable row must not unwind the blocking task");

    assert!(
        windows.is_empty(),
        "a partially-undecodable agent.db must degrade to capacity-unknown, got: {windows:?}"
    );
    assert!(
        logs.contains("reading omp usage windows failed"),
        "the decode failure must be logged, got: {logs}"
    );
}

/// Copilot's provider column is authoritative: its limit ID deliberately
/// does not start with `github-copilot`, and other providers may share a DB.
#[tokio::test]
async fn copilot_read_windows_selects_provider_not_limit_prefix() {
    let now = chrono::DateTime::parse_from_rfc3339("2024-02-15T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let reset = chrono::DateTime::parse_from_rfc3339("2024-03-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    let dir = TempDir::new("cap-copilot-provider");
    let db = make_agent_db_with_provider(
        &dir,
        "github-copilot",
        &[
            (
                "copilot:premium",
                Some(0.25),
                "ok",
                Some(reset),
                now - 60_000,
            ),
            // Limits hunter never gates on are not read at all, so a row
            // omp stopped writing (chat/completions once unlimited) cannot
            // linger as a stale bar.
            ("copilot:chat", Some(0.1), "ok", Some(reset), now - 600_000),
            ("copilot:completions", Some(0.0), "ok", Some(reset), now),
            ("copilot:model:gpt-5", Some(0.3), "ok", Some(reset), now),
        ],
    )
    .await;
    // Newer foreign rows with the same limit ID must not shadow Copilot.
    make_agent_db_with_provider(
        &dir,
        "anthropic",
        &[
            ("copilot:premium", Some(1.0), "exhausted", Some(reset), now),
            ("anthropic:7d", Some(0.9), "ok", Some(now + WEEK_MS), now),
        ],
    )
    .await;
    make_agent_db_with_provider(
        &dir,
        "openai-codex",
        &[(
            "openai-codex:secondary",
            Some(0.8),
            "ok",
            Some(now + WEEK_MS),
            now,
        )],
    )
    .await;

    let windows =
        tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::GitHubCopilot, now))
            .await
            .unwrap();
    assert_eq!(
        windows.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["copilot:premium"]
    );
    let monthly = &windows["copilot:premium"];
    assert_eq!(monthly.used_fraction, Some(0.25));
    assert_eq!(monthly.status.as_deref(), Some("ok"));
    assert_eq!(monthly.resets_at, Some(reset));
    assert_eq!(monthly.recorded_at, now - 60_000);
    assert_eq!(monthly.age_s, 60.0);
}

/// Calendar quota cannot be replenished by guessing a 30-day period:
/// at the reset and throughout missed months it stays unknown until refresh.
#[tokio::test]
async fn copilot_read_windows_discards_expired_monthly_quota() {
    let reset = chrono::DateTime::parse_from_rfc3339("2024-03-01T00:00:00Z")
        .unwrap()
        .timestamp_millis();
    for timestamp in [
        "2024-03-01T00:00:00Z",
        "2024-03-02T00:00:00Z",
        "2024-04-15T00:00:00Z",
    ] {
        let now = chrono::DateTime::parse_from_rfc3339(timestamp)
            .unwrap()
            .timestamp_millis();
        let dir = TempDir::new("cap-copilot-expired");
        let db = make_agent_db_with_provider(
            &dir,
            "github-copilot",
            &[(
                "copilot:premium",
                Some(0.9),
                "ok",
                Some(reset),
                reset - 3_600_000,
            )],
        )
        .await;
        let windows =
            tokio::task::spawn_blocking(move || read_windows(&db, LlmProvider::GitHubCopilot, now))
                .await
                .unwrap();
        assert!(
            windows.is_empty(),
            "expired quota was fabricated at {timestamp}: {windows:?}"
        );
    }
}

/// Monthly pacing and its retry inverse follow actual calendar boundaries,
/// including leap February and the December-to-January year transition.
#[test]
fn copilot_monthly_ramp_and_retry_use_actual_calendar_month() {
    let monthly = LlmProvider::GitHubCopilot.quota().windows.first().unwrap();
    for (start, reset) in [
        ("2023-02-01T00:00:00Z", "2023-03-01T00:00:00Z"),
        ("2024-02-01T00:00:00Z", "2024-03-01T00:00:00Z"),
        ("2024-04-01T00:00:00Z", "2024-05-01T00:00:00Z"),
        ("2024-12-01T00:00:00Z", "2025-01-01T00:00:00Z"),
    ] {
        let start = chrono::DateTime::parse_from_rfc3339(start)
            .unwrap()
            .timestamp_millis();
        let reset = chrono::DateTime::parse_from_rfc3339(reset)
            .unwrap()
            .timestamp_millis();
        for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let at = start + ((reset - start) as f64 * fraction) as i64;
            let ramp = monthly.ramp(Some(reset), at).unwrap();
            assert!(
                (ramp - fraction).abs() < 1e-9,
                "calendar ramp {ramp}, expected {fraction}"
            );
            let retry = monthly.retry_at(Some(reset), fraction).unwrap();
            assert!(
                (retry - at as f64).abs() < 1.0,
                "calendar retry {retry}, expected {at}"
            );
        }
    }
}

/// The cycle start is the first of the previous month, the same boundary
/// `Store::estimate_capacity`'s SQL uses, even when the reset is not on a
/// month boundary: pacing and calibration must agree on one cycle.
#[test]
fn copilot_off_boundary_reset_starts_at_the_previous_month_start() {
    let monthly = LlmProvider::GitHubCopilot.quota().windows.first().unwrap();
    let ms = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    };
    let start = ms("2026-10-01T00:00:00Z");
    let reset = ms("2026-11-15T12:00:00Z");
    let mid = start + (reset - start) / 2;
    assert!((monthly.ramp(Some(reset), mid).unwrap() - 0.5).abs() < 1e-9);
    let retry = monthly.retry_at(Some(reset), 0.5).unwrap();
    assert!(
        (retry - mid as f64).abs() < 1.0,
        "retry {retry}, expected {mid}"
    );
}
