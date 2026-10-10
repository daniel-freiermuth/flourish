#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Scheduler selection tests: `anticipated_tokens` (warm/cold percentile
//! choice) and `pick_next` (priority order + eligibility), plus one
//! router-level summary integration over `NullBackend`. Fixture pattern
//! matches the store tests: copy dev.db into a `support::TempDir`, seed via
//! a writable pool, reopen read-only through `Store::connect_read_only`.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::config::Config;
use hunter::domain::{FindingJobKind, RepoJobKind};
use hunter::scheduler::{anticipated_tokens, pick_next};
use hunter::server::{AppState, router};
use hunter::store::Store;
use hunter::util::now_ms;
use serde_json::Value;
use sqlx::SqlitePool;
use support::TempDir;
use tower::util::ServiceExt;

/// Copy dev.db (schema-complete, zero rows) into a scratch directory and
/// open a WRITABLE pool on the copy for fixture inserts.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives the
/// pool and the `Store` opened on `path`. Removing the files by hand at the
/// end of the test body leaked the whole set on any failing assertion.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Close the writer and reopen the same file read-only through Store.
async fn open_store(pool: SqlitePool, path: &Path) -> Store {
    pool.close().await;
    Store::connect_read_only(path).await.unwrap()
}

fn test_config(cache_ttl_s: f64) -> Config {
    let dir = std::env::temp_dir();
    Config {
        root: dir.clone(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        serve_host: std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        allowed_hosts: hunter::config::HostAllowList::default(),
        ui_dir: dir.join("ui"),
        omp_bin: "omp".to_owned(),
        stale_after_s: 300.0,
        cache_ttl_s,
        poll_s: 2.0,
        session_grace_s: 120,
        min_free_disk_bytes: 0,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".to_owned(),
        llm_provider: hunter::backends::omp_scavenge::LlmProvider::Anthropic,
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

/// One enabled repo. `path` deliberately nonexistent unless a test says
/// otherwise (a huntable-but-not-cloned repo).
async fn seed_repo(pool: &SqlitePool, id: i64, name: &str, enabled: i64, path: &str) {
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (?1, ?2, 'https://example.com/r.git', ?3, 'github', 'main', ?4, 1000)",
    )
    .bind(id)
    .bind(name)
    .bind(path)
    .bind(enabled)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_finding(pool: &SqlitePool, id: i64, repo_id: i64, status: &str, summary: &str) {
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (?1, 'bug', ?2, ?3, 'high', 0.9, ?4, ?5, 1000, 1000)",
    )
    .bind(id)
    .bind(repo_id)
    .bind(format!("fp-{id}"))
    .bind(summary)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

/// Ten finished hunt jobs on repo 1 with `tokens_new` 100, 200, ..., 1000,
/// all finished in the distant past (cold for any sane TTL).
/// p50 index = int(10 * 0.5) = 5 -> 600; p90 index = int(10 * 0.9) = 9 -> 1000.
async fn seed_history(pool: &SqlitePool) {
    for i in 1..=10_i64 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES (?1, 'hunt', 1, 'done', ?2, 1000, 2000)",
        )
        .bind(i)
        .bind(i * 100)
        .execute(pool)
        .await
        .unwrap();
    }
}

// -- anticipated_tokens -------------------------------------------------------

#[tokio::test]
async fn anticipated_tokens_empty_history_is_zero() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 0);
}

#[tokio::test]
async fn anticipated_tokens_cold_p90() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    // Everything finished at epoch-ms 2000 -> stone cold -> p90.
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000);
}

#[tokio::test]
async fn anticipated_tokens_warm_p50() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    // Recent non-denied hunt on the SAME repo. tokens_new NULL keeps the
    // 10-value history intact (the history query filters IS NOT NULL).
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 1, 'done', ?1, ?1)",
    )
    .bind(now_ms() - 60_000)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 600);
}

#[tokio::test]
async fn anticipated_tokens_warm_requires_same_repo_and_non_denied() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_repo(&pool, 2, "beta", 1, "/nonexistent/beta").await;
    seed_history(&pool).await;
    let recent = now_ms() - 60_000;
    // Recent hunt on a DIFFERENT repo: does not warm repo 1.
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 2, 'done', ?1, ?1)",
    )
    .bind(recent)
    .execute(&pool)
    .await
    .unwrap();
    // Recent DENIED hunt on repo 1: state != 'denied' excludes it.
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (12, 'hunt', 1, 'denied', ?1, ?1)",
    )
    .bind(recent)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000); // still cold -> p90
}

#[tokio::test]
async fn anticipated_tokens_warm_cold_boundary_via_cache_ttl() {
    // Same fixture, same finished_at (1 minute ago) — only the TTL moves:
    // TTL 3600 s puts the job inside the window (warm -> p50), TTL 1 s
    // puts it outside (cold -> p90).
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_history(&pool).await;
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) \
         VALUES (11, 'hunt', 1, 'done', ?1, ?1)",
    )
    .bind(now_ms() - 60_000)
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;

    let warm_cfg = test_config(3600.0);
    let got = anticipated_tokens(&store, &warm_cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 600, "within TTL -> warm -> p50");

    let cold_cfg = test_config(1.0);
    let got = anticipated_tokens(&store, &cold_cfg, 1, RepoJobKind::Hunt.into())
        .await
        .unwrap();
    assert_eq!(got, 1000, "outside TTL -> cold -> p90");
}

// -- pick_next ----------------------------------------------------------------

#[tokio::test]
async fn pick_next_empty_db_is_none() {
    let (_dir, path, pool) = fresh_db().await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    assert!(pick_next(&store, &cfg, None).await.unwrap().is_none());
}

#[tokio::test]
async fn pick_next_disabled_repo_never_selected() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 0, "/nonexistent/alpha").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    assert!(pick_next(&store, &cfg, None).await.unwrap().is_none());
}

#[tokio::test]
async fn pick_next_queued_finding_beats_huntable_repo() {
    let (_dir, path, pool) = fresh_db().await;
    // Never-cloned enabled repo: would be a "hunt" candidate on its own.
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Fix.into());
    assert_eq!(c.target_id(), 5);
    assert_eq!(c.repo_id(), 1);
    assert!(matches!(c, hunter::scheduler::Candidate::Finding { .. }));
    assert_eq!(c.budget_override(), None);
    assert_eq!(c.label(), Some("queued bug"));
}

#[tokio::test]
async fn pick_next_attention_beats_queued_fix_and_orders_by_attention_since() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    // Two attention-flagged pr_open findings; #7 has been waiting longer
    // (attention_since 1000 < 2000) and must win despite the lower id
    // sorting later in list_findings' id-DESC order.
    seed_finding(&pool, 7, 1, "pr_open", "older attention").await;
    seed_finding(&pool, 8, 1, "pr_open", "newer attention").await;
    for (fid, since) in [(7_i64, 1000_i64), (8, 2000)] {
        sqlx::query(
            "INSERT INTO pr_state \
             (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
             VALUES (?1, 1, 'OPEN', 'review_comments', ?2, 5000)",
        )
        .bind(fid)
        .bind(since)
        .execute(&pool)
        .await
        .unwrap();
    }
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Engage.into());
    assert_eq!(c.target_id(), 7, "oldest-outstanding attention first");
    assert!(matches!(c, hunter::scheduler::Candidate::Finding { .. }));
}

#[tokio::test]
async fn pick_next_budget_override_jumps_the_queue() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    // Attention row would normally win over a queued fix ...
    seed_finding(&pool, 7, 1, "pr_open", "flagged pr").await;
    sqlx::query(
        "INSERT INTO pr_state \
         (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
         VALUES (7, 1, 'OPEN', 'review_comments', 1000, 5000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // ... but an overridden queued finding jumps everything un-overridden.
    seed_finding(&pool, 5, 1, "queued", "urgent fix").await;
    sqlx::query("UPDATE findings SET budget_override = 'once' WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Fix.into());
    assert_eq!(c.target_id(), 5);
    assert_eq!(
        c.budget_override(),
        Some(hunter::domain::BudgetOverride::Once)
    );
}

// -- pick_next: the rotation's intervals ----------------------------------------

const DAY_MS: i64 = 86_400_000;

/// When each rotation scan of a repo last ran, as days before now.
#[derive(Clone, Copy)]
struct Ran {
    hunt: f64,
    test_gap: f64,
    modernization: f64,
    standards: f64,
}

/// Every scan ran just now: nothing is due.
const FRESH: Ran = Ran {
    hunt: 0.0,
    test_gap: 0.0,
    modernization: 0.0,
    standards: 0.0,
};

/// An enabled, cloned repo (its path exists) whose scans last ran as
/// `ran` says; `dep_update` and `refactor` ran just now.
async fn seed_scanned_repo(pool: &SqlitePool, id: i64, name: &str, path: &Path, ran: Ran) {
    let now = now_ms();
    #[allow(clippy::cast_possible_truncation, reason = "whole milliseconds")]
    let ago = |days: f64| now - (days * DAY_MS as f64) as i64;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, \
         last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
         last_modernization_at, last_standards_at) \
         VALUES (?1, ?2, 'https://example.com/r.git', ?3, 'github', 'main', 1, 1000, \
                 ?4, ?5, ?6, ?6, ?7, ?8)",
    )
    .bind(id)
    .bind(name)
    .bind(path.to_string_lossy().to_string())
    .bind(ago(ran.hunt))
    .bind(ago(ran.test_gap))
    .bind(now)
    .bind(ago(ran.modernization))
    .bind(ago(ran.standards))
    .execute(pool)
    .await
    .unwrap();
}

/// What `pick_next` selects for one repo that last ran as `ran`, under a
/// 2-day scan interval and 30-day modernization and standards intervals.
async fn rotation_pick(ran: Ran) -> Option<RepoJobKind> {
    let (dir, path, pool) = fresh_db().await;
    seed_scanned_repo(&pool, 1, "alpha", dir.path(), ran).await;
    let store = open_store(pool, &path).await;
    let mut cfg = test_config(3600.0);
    cfg.scan_interval_days = 2.0;
    match pick_next(&store, &cfg, None).await.unwrap() {
        None => None,
        Some(hunter::scheduler::Candidate::Repo { kind, .. }) => Some(kind),
        Some(other) => panic!("expected a rotation pick, got {other:?}"),
    }
}

#[tokio::test]
async fn a_scan_is_due_only_once_its_interval_has_passed() {
    let within = Ran {
        test_gap: 1.5,
        ..FRESH
    };
    assert_eq!(rotation_pick(within).await, None);
    let past = Ran {
        test_gap: 3.0,
        ..FRESH
    };
    assert_eq!(rotation_pick(past).await, Some(RepoJobKind::TestGap));
}

#[tokio::test]
async fn modernization_is_due_only_once_its_interval_has_passed() {
    let within = Ran {
        modernization: 20.0,
        ..FRESH
    };
    assert_eq!(rotation_pick(within).await, None);
    let past = Ran {
        modernization: 40.0,
        ..FRESH
    };
    assert_eq!(rotation_pick(past).await, Some(RepoJobKind::Modernization));
}

#[tokio::test]
async fn standards_is_due_only_once_its_interval_has_passed() {
    let within = Ran {
        standards: 20.0,
        ..FRESH
    };
    assert_eq!(rotation_pick(within).await, None);
    let past = Ran {
        standards: 40.0,
        ..FRESH
    };
    assert_eq!(rotation_pick(past).await, Some(RepoJobKind::Standards));
}

/// A forced cycle picks the named repo's work, though another repo's is
/// staler, and though the named repo is paused.
#[tokio::test]
async fn a_forced_repo_is_picked_over_a_staler_one() {
    let (dir, path, pool) = fresh_db().await;
    let stale = Ran {
        hunt: 10.0,
        ..FRESH
    };
    let due = Ran { hunt: 3.0, ..FRESH };
    seed_scanned_repo(&pool, 1, "alpha", dir.path(), stale).await;
    seed_scanned_repo(&pool, 2, "beta", dir.path(), due).await;
    sqlx::query("UPDATE repos SET enabled = 0 WHERE id = 2")
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);

    let unforced = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(
        unforced.repo_id(),
        1,
        "unforced, the stalest repo goes first"
    );
    let forced = pick_next(&store, &cfg, Some("beta"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(forced.repo_id(), 2);
    assert_eq!(forced.job_kind(), RepoJobKind::Hunt.into());
}

#[tokio::test]
async fn pick_next_rechecking_beats_queued_fix_oldest_first() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    // Two rechecks: #6 is the older, and list_findings returns id DESC.
    seed_finding(&pool, 6, 1, "rechecking", "older recheck").await;
    seed_finding(&pool, 9, 1, "rechecking", "newer recheck").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Recheck.into());
    assert_eq!(c.target_id(), 6, "oldest recheck first");
    assert_eq!(c.budget_override(), None);
}

/// Overrides are scanned in the normal kind order, and the override's
/// own tier decides the job kind: an overridden recheck runs as a
/// recheck, ahead of an un-overridden attention row and of an overridden
/// queued fix.
#[tokio::test]
async fn pick_next_budget_override_keeps_its_tier_kind_and_order() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 7, 1, "pr_open", "flagged pr").await;
    sqlx::query(
        "INSERT INTO pr_state \
         (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
         VALUES (7, 1, 'OPEN', 'review_comments', 1000, 5000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_finding(&pool, 5, 1, "queued", "urgent fix").await;
    seed_finding(&pool, 6, 1, "rechecking", "urgent recheck").await;
    sqlx::query("UPDATE findings SET budget_override = 'once' WHERE id IN (5, 6)")
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), FindingJobKind::Recheck.into());
    assert_eq!(c.target_id(), 6);
    assert_eq!(c.budget_override(), Some("once"));
}

/// With no finding work, the enabled repo hunted longest ago is hunted;
/// a disabled repo is skipped even though it has never been hunted.
#[tokio::test]
async fn pick_next_no_finding_work_hunts_least_recently_hunted_enabled_repo() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_repo(&pool, 2, "beta", 1, "/nonexistent/beta").await;
    seed_repo(&pool, 3, "gamma", 0, "/nonexistent/gamma").await;
    for (id, at) in [(1_i64, 5000_i64), (2, 1000)] {
        sqlx::query("UPDATE repos SET last_hunt_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(at)
            .execute(&pool)
            .await
            .unwrap();
    }
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), RepoJobKind::Hunt.into());
    assert_eq!(c.repo_id(), 2);
    assert!(matches!(c, hunter::scheduler::Candidate::Repo { .. }));
}

#[tokio::test]
async fn pick_next_never_hunted_repo_beats_hunted_one() {
    let (_dir, path, pool) = fresh_db().await;
    // "alpha" sorts first by name; only staleness may put "beta" ahead.
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_repo(&pool, 2, "beta", 1, "/nonexistent/beta").await;
    sqlx::query("UPDATE repos SET last_hunt_at = 1000 WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let c = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(c.job_kind(), RepoJobKind::Hunt.into());
    assert_eq!(c.repo_id(), 2);
}

#[tokio::test]
async fn pick_next_unknown_force_repo_is_an_error() {
    let (_dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);
    let err = pick_next(&store, &cfg, Some("nosuch")).await.unwrap_err();
    assert_eq!(err.to_string(), r#"unknown repo "nosuch""#);
}

// -- summary integration (oneshot router, NullBackend) -------------------------

/// `/api/summary` over a queued fix finding (id 5) and no running job,
/// with the daemon's backend wiring: `NullBackend` behind the overdrive
/// switch.
async fn summary_over_queued_fix(overdrive: bool) -> Value {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, 1, "alpha", 1, "/nonexistent/alpha").await;
    seed_finding(&pool, 5, 1, "queued", "queued bug").await;
    support::seed_session(&pool).await;
    let store = open_store(pool, &path).await;

    let mut config = test_config(3600.0);
    config.ui_dir = dir.subdir("ui");
    let overdrive = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(overdrive));
    let state = AppState {
        store: Arc::new(store),
        config: Arc::new(config),
        backend: Arc::new(hunter::backend::OverdriveBackend::new(
            Arc::new(hunter::backend::NullBackend),
            overdrive.clone(),
        )),
        repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        scheduler: hunter::server::SchedulerHandle {
            running: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            overdrive,
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        },
    };

    let cookie = support::TEST_COOKIE;
    let response = router(state)
        .oneshot(
            Request::builder()
                .header(axum::http::header::COOKIE, cookie)
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn summary_paused_on_denied_candidate() {
    let v = summary_over_queued_fix(false).await;
    assert!(v["current_job"].is_null());
    assert_eq!(v["cycle_running"], Value::Bool(false));

    // NullBackend always denies -> the fix candidate surfaces as denied.
    let nc = &v["next_candidate"];
    assert_eq!(nc["kind"], "fix");
    assert_eq!(nc["id"], 5);
    assert_eq!(nc["label"], "queued bug");
    assert_eq!(nc["is_finding"], Value::Bool(true));
    assert_eq!(nc["is_prioritized"], Value::Bool(false));
    assert_eq!(nc["budget_state"], "denied");
    assert_eq!(nc["budget_reason"], "no window data -- deny until fresh");
    assert!(nc["budget_retry_at"].is_null());

    assert_eq!(v["activity_status"]["kind"], "paused");
    assert_eq!(v["activity_status"]["candidate"]["id"], 5);
    assert_eq!(
        v["backend_status_html"],
        r#"<div class="scv-note">No window data available</div>"#
    );
}

#[tokio::test]
async fn summary_overdrive_prioritizes_candidate_but_keeps_hard_denial() {
    let v = summary_over_queued_fix(true).await;
    assert_eq!(v["scheduler_overdrive"], Value::Bool(true));

    // Overdrive routes the plain (un-overridden) candidate through the
    // prioritized path, and the preview must say so ...
    let nc = &v["next_candidate"];
    assert_eq!(nc["id"], 5);
    assert_eq!(nc["is_prioritized"], Value::Bool(true));
    // ... but a provider hard stop still denies it.
    assert_eq!(nc["budget_state"], "denied");
    assert_eq!(nc["budget_reason"], "no window data -- deny until fresh");
    assert_eq!(v["activity_status"]["kind"], "paused");
}

/// A repo with an unanswered full re-hunt request goes first in the
/// rotation, ahead of a staler repo whose own hunt is due.
#[tokio::test]
async fn a_requested_full_rehunt_jumps_a_staler_repo() {
    let (dir, path, pool) = fresh_db().await;
    let stale = Ran {
        hunt: 10.0,
        ..FRESH
    };
    seed_scanned_repo(&pool, 1, "alpha", dir.path(), stale).await;
    seed_scanned_repo(&pool, 2, "beta", dir.path(), FRESH).await;
    sqlx::query("UPDATE repos SET full_hunt_requested_at = ?1 WHERE id = 2")
        .bind(now_ms())
        .execute(&pool)
        .await
        .unwrap();
    let store = open_store(pool, &path).await;
    let cfg = test_config(3600.0);

    let picked = pick_next(&store, &cfg, None).await.unwrap().unwrap();
    assert_eq!(picked.repo_id(), 2);
    assert_eq!(picked.job_kind(), RepoJobKind::Hunt.into());
}
