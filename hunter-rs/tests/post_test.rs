#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! POST endpoint tests (API-CONTRACT-WRITES.md): oneshot router with
//! `NullBackend` over a WRITABLE tempdir copy of dev.db, with a scheduler
//! handle standing in for the daemon's loop.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::config::Config;
use hunter::server::{AppState, router};
use hunter::store::{JobStart, Store};
use serde_json::{Value, json};
use tower::util::ServiceExt;

mod support;

/// `AppState` plus the scratch directory its files live in.
///
/// Field order is the contract: Rust drops fields in declaration order,
/// so `state` — and with it the `Store`'s open SQLite pool — is gone
/// before `dir` removes the database out from under it.
///
/// `Deref` keeps every call site writing `&state`, and means the guard
/// can only be dropped by dropping the state it came with.
struct TestState {
    state: AppState,
    dir: support::TempDir,
}

impl std::ops::Deref for TestState {
    type Target = AppState;

    fn deref(&self) -> &AppState {
        &self.state
    }
}

/// Refuse to run when this process can write through a read-only
/// directory.
///
/// Three tests below block reclamation by taking write permission off
/// `repos/`. Root ignores that bit, so under root the removal succeeds,
/// the row is reaped, and every one of them goes green having exercised
/// the opposite of what it claims to test — the silent kind of pass,
/// where the assertions still run and still hold.
///
/// Checked by trying it rather than by comparing the euid to 0: the euid
/// is a proxy, and `CAP_DAC_OVERRIDE` or a filesystem mounted without
/// permission enforcement defeats the tests the same way while leaving
/// it non-zero. It also keeps this to `std`.
///
/// Runs in its own directory and restores it before asserting, so a
/// caller's `repos/` is untouched whether this passes or panics — which
/// is also why it must be called BEFORE the caller's own chmod.
fn require_enforced_permissions() {
    let probe = support::TempDir::new("perm-probe");
    let dir = probe.path();
    let mut perms = std::fs::metadata(dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(dir, perms).unwrap();
    let wrote = std::fs::write(dir.join("probe"), "x").is_ok();
    let mut perms = std::fs::metadata(dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(dir, perms).unwrap();
    assert!(
        !wrote,
        "this process writes through a read-only directory (running as \
         root?), and this test blocks reclamation with one — it would \
         pass while proving the opposite"
    );
}

/// Copy dev.db into `dir`, seed repos/findings through a writable pool,
/// then reopen WRITABLE (POSTs write). Seed layout:
/// - repo 1 "demo" with findings 1 (new, fp-1), 2 (queued, fp-2),
///   3 (new, fp-3) — delete must refuse;
/// - repo 2 "clean" with no findings/jobs — delete must succeed.
async fn seeded_store(dir: &Path) -> Store {
    let db = dir.join("hunter.db");
    std::fs::copy(concat!(env!("CARGO_MANIFEST_DIR"), "/dev.db"), &db).unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&db);
    let pool = sqlx::SqlitePool::connect_with(opts).await.unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) VALUES \
         (1, 'demo', 'https://example.com/demo.git', '/tmp/demo', 'github', 'main', 1, 1000), \
         (2, 'clean', 'https://example.com/clean.git', '/tmp/clean', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    for (id, fp, status) in [
        (1, "fp-1", "new"),
        (2, "fp-2", "queued"),
        (3, "fp-3", "new"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at) \
             VALUES (?, 'bug', 1, ?, 'high', 0.9, 'demo finding', ?, 1000, 1000)",
        )
        .bind(id)
        .bind(fp)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
    pool.close().await;
    Store::connect(&db).await.unwrap()
}

async fn test_state() -> TestState {
    let dir = support::TempDir::new("post");
    dir.subdir("ui");
    let store = seeded_store(dir.path()).await;
    let config = Config {
        root: dir.path().to_path_buf(),
        work_root: dir.join("data"),
        db_path: dir.join("hunter.db"),
        serve_port: 0,
        serve_host: std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        allowed_hosts: hunter::config::HostAllowList::default(),
        ui_dir: dir.join("ui"),
        omp_bin: "omp".into(),
        stale_after_s: 300.0,
        cache_ttl_s: 3600.0,
        poll_s: 2.0,
        session_grace_s: 120,
        min_free_disk_bytes: 0,
        model_default: None,
        model_smol: None,
        model_hunt: None,
        model_fix: None,
        backend_type: "omp-scavenge".into(),
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
    };
    TestState {
        state: AppState {
            store: Arc::new(store),
            config: Arc::new(config),
            backend: Arc::new(hunter::backend::NullBackend),
            repo_notes: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            scheduler: hunter::server::SchedulerHandle {
                running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                overdrive: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                wake: Arc::new(tokio::sync::Notify::new()),
            },
        },
        dir,
    }
}

async fn post_raw(
    state: &AppState,
    path: &str,
    ctype: Option<&str>,
    body: &str,
) -> (StatusCode, Value) {
    let cookie = support::auth_cookie(&state.store).await;
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(axum::http::header::COOKIE, cookie.as_str());
    if let Some(ct) = ctype {
        builder = builder.header("content-type", ct);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn post(state: &AppState, path: &str, body: Value) -> (StatusCode, Value) {
    post_raw(state, path, Some("application/json"), &body.to_string()).await
}

/// Captures `tracing` output for the duration of one request.
///
/// `ApiError::Internal` answers the client with a bare `internal error`
/// envelope on purpose, so the log is the only place the detail exists.
/// A poisoned lock is recovered rather than unwrapped: a panic mid-test
/// must surface as that panic, not as a second one from the log sink.
#[derive(Clone, Default)]
struct LogSink(Arc<std::sync::Mutex<Vec<u8>>>);

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

const POST_PATHS: [&str; 11] = [
    "/api/verdict",
    "/api/cycle",
    "/api/recheck",
    "/api/unqueue",
    "/api/override",
    "/api/repo",
    "/api/repos",
    "/api/repo/delete",
    "/api/repo/notes",
    "/api/repo/rehunt",
    "/api/hold/release",
];

// -- §0.1 Content-Type gate ---------------------------------------------------

#[tokio::test]
async fn content_type_gate_415_on_every_post_path() {
    let state = test_state().await;
    for path in POST_PATHS {
        for ctype in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
        ] {
            let (status, body) = post_raw(&state, path, ctype, "{}").await;
            assert_eq!(
                status,
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "{path} with ctype {ctype:?}"
            );
            assert_eq!(
                body["error"], "Content-Type must be application/json",
                "{path} with ctype {ctype:?}"
            );
        }
    }
    // Prefix match: charset suffix passes the gate (fails later, not 415).
    let (status, _) = post_raw(
        &state,
        "/api/recheck",
        Some("application/json; charset=utf-8"),
        r#"{"id": 1}"#,
    )
    .await;
    assert_ne!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

// -- §0.2 body parsing ---------------------------------------------------------

#[tokio::test]
async fn body_parse_errors() {
    let state = test_state().await;
    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "empty body");

    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "{nope").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid JSON body");

    // ACCEPTED DEVIATION: Python 500s on non-object bodies; we keep the
    // 400 envelope.
    let (status, body) = post_raw(&state, "/api/verdict", Some("application/json"), "[1,2]").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "body must be a JSON object");

    // Booleans are NOT integers (§0.2 quirk decision).
    let (status, body) = post(&state, "/api/recheck", json!({ "id": true })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "id must be an integer");
}

// -- §1 /api/verdict -----------------------------------------------------------

#[tokio::test]
async fn verdict_rejected_with_reason_happy_path() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 1, "status": "rejected", "reason": "dupe of #7" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["status"], "rejected");
    assert_eq!(body["finding"]["verdict_reason"], "dupe of #7");

    // Read-back: status + verdict_reason persisted, event row written.
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(finding.status, hunter::domain::FindingStatus::Rejected);
    assert_eq!(finding.verdict_reason.as_deref(), Some("dupe of #7"));
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "verdict").unwrap();
    assert_eq!(event.message, "finding 1 [fp-1] -> rejected: dupe of #7");
    assert_eq!(event.finding_id, Some(1));
}

#[tokio::test]
async fn verdict_unknown_finding_404() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/verdict",
        json!({ "id": 999, "status": "note" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no finding 999");
}

/// Unknown strings and the scheduler-owned statuses alike are not verdicts.
/// `blocked` is no status at all any more: held work is released through
/// `/api/hold/release`.
#[tokio::test]
async fn verdict_bad_status_400() {
    let state = test_state().await;
    for bad in [
        "bogus",
        "fixing",
        "blocked",
        "pr_open",
        "closed",
        "superseded",
        "merged",
    ] {
        let (status, body) = post(
            &state,
            "/api/verdict",
            json!({ "id": 1, "status": bad, "reason": "r" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        assert_eq!(
            body["error"],
            "status must be one of ['queued', 'rejected', 'wontfix', 'note']"
        );
        assert_eq!(
            state.store.get_finding(1).await.unwrap().unwrap().status,
            hunter::domain::FindingStatus::New
        );
    }
}

// -- §§3,4 /api/recheck, /api/unqueue -------------------------------------------

#[tokio::test]
async fn recheck_happy_path_and_precondition() {
    let state = test_state().await;
    // Finding 2 is 'queued' -> exact precondition message.
    let (status, body) = post(&state, "/api/recheck", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "finding #2 is 'queued', not 'new'");

    // Finding 1 is 'new' -> queued for recheck.
    let (status, body) = post(&state, "/api/recheck", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["queued"], json!(true));
    assert_eq!(body["finding"]["status"], "rechecking");
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "recheck").unwrap();
    assert_eq!(event.message, "#1 queued for recheck");
}

#[tokio::test]
async fn unqueue_happy_path_and_precondition() {
    let state = test_state().await;
    // Finding 1 is 'new' -> exact precondition message.
    let (status, body) = post(&state, "/api/unqueue", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "finding #1 is 'new', not 'queued'");

    // Finding 2 is 'queued' -> back to new.
    let (status, body) = post(&state, "/api/unqueue", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["status"], "new");
    let events = state.store.recent_events(10).await.unwrap();
    let event = events.iter().find(|e| e.kind == "unqueue").unwrap();
    assert_eq!(event.message, "#2 removed from fix queue");
}

// -- §5 /api/override ------------------------------------------------------------

#[tokio::test]
async fn override_set_once_row_updated_and_wakes_loop() {
    let state = test_state().await;
    let wake = Arc::clone(&state.scheduler.wake);
    let (status, body) = post(&state, "/api/override", json!({ "id": 1, "mode": "once" })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["finding"]["budget_override"], "once");
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(
        finding.budget_override,
        Some(hunter::domain::BudgetOverride::Once)
    );
    // Setting an override must not wait for the loop's next natural wake.
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("setting an override must wake the scheduler loop");
}

#[tokio::test]
async fn override_clear_single() {
    let state = test_state().await;
    let (status, _) = post(
        &state,
        "/api/override",
        json!({ "id": 1, "mode": "exempt" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(&state, "/api/override", json!({ "id": 1, "mode": null })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["finding"]["budget_override"], Value::Null);
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "override" && e.message == "#1 budget override: cleared")
    );
}

#[tokio::test]
async fn override_clear_all_returns_cleared_count() {
    let state = test_state().await;
    post(&state, "/api/override", json!({ "id": 1, "mode": "once" })).await;
    post(
        &state,
        "/api/override",
        json!({ "id": 3, "mode": "exempt" }),
    )
    .await;
    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": "all", "mode": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true, "cleared": 2 }));
    let finding = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(finding.budget_override, None);
}

#[tokio::test]
async fn override_bad_id_and_bad_mode_exact_messages() {
    let state = test_state().await;
    // "all" with non-null mode falls through to the int check.
    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": "all", "mode": "once" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "id must be an integer (or 'all' with mode=null)"
    );

    let (status, body) = post(
        &state,
        "/api/override",
        json!({ "id": 1, "mode": "forever" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "mode must be 'once', 'exempt', or null");
}

// -- §6 /api/repo ------------------------------------------------------------------

#[tokio::test]
async fn repo_update_enabled_toggle_and_no_valid_fields() {
    let state = test_state().await;
    let (status, body) = post(&state, "/api/repo", json!({ "id": 1, "enabled": false })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["repo"]["enabled"], 0);
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "updated demo: enabled=0")
    );

    // Invalid forge is silently skipped -> nothing left to update.
    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "forge": "bitbucket" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no valid fields to update");
}

// -- §7 /api/repos -------------------------------------------------------------------

#[tokio::test]
async fn repo_add_happy_then_duplicate_409() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "newrepo", "url": "https://github.com/x/newrepo.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["repo"]["name"], "newrepo");
    assert_eq!(body["repo"]["forge"], "github");
    assert_eq!(body["repo"]["default_branch"], "main");
    assert_eq!(body["repo"]["enabled"], 1);
    // The clone directory is keyed by id, never by the name: a name is a
    // display string and may differ from another only in case, which is the
    // same directory on NTFS and APFS.
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let path = body["repo"]["path"].as_str().unwrap();
    assert!(
        path.ends_with(&format!("repos/repo-{rid}")),
        "path was {path}"
    );
    assert!(
        !path.contains("newrepo"),
        "name must not reach the path: {path}"
    );

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "newrepo", "url": "https://github.com/x/other.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "repo 'newrepo' already exists");
}

#[tokio::test]
async fn repo_add_bad_name_and_missing_url() {
    let state = test_state().await;
    let too_long = "x".repeat(101);
    for name in [
        "",
        "two\nlines",
        "tab\there",
        "nul\0",
        "line\u{2028}sep",
        "para\u{2029}sep",
        "rlo\u{202E}txt.exe",
        "alm\u{061C}x",
        "lrm\u{200E}x",
        "rlm\u{200F}x",
        "lre\u{202A}x",
        "lri\u{2066}x",
        "pdi\u{2069}x",
        too_long.as_str(),
    ] {
        let (status, body) = post(
            &state,
            "/api/repos",
            json!({ "name": name, "url": "https://github.com/x/y.git" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "name {name:?}");
        assert_eq!(body["error"], "invalid repo name", "name {name:?}");
    }
    let (status, body) = post(&state, "/api/repos", json!({ "name": "ok_name-1.0" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "name and url are required");
}

/// The name is a label, not a path: spaces and path-looking names are
/// accepted and still clone into `repo-<id>`.
#[tokio::test]
async fn repo_add_name_never_reaches_the_path() {
    let state = test_state().await;
    let longest = "y".repeat(100);
    // U+202F (narrow no-break space) sits just past the refused U+202A-202E
    // bidi range and is ordinary typography; Hebrew needs no bidi controls.
    let names = [
        "My Repo (fork)",
        "../../etc",
        "café",
        "שלום-repo",
        "a\u{202F}b",
        longest.as_str(),
    ];
    for name in names {
        let (status, body) = post(
            &state,
            "/api/repos",
            json!({ "name": name, "url": "https://github.com/x/y.git" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "name {name:?}: {body}");
        assert_eq!(body["repo"]["name"], name);
        let rid = body["repo"]["id"].as_i64().unwrap();
        let path = body["repo"]["path"].as_str().unwrap();
        assert!(
            path.ends_with(&format!("repos/repo-{rid}")),
            "path was {path}"
        );
    }
}

#[tokio::test]
async fn repo_add_detects_gitlab_from_url_and_rejects_unknown_forge() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "glrepo", "url": "https://gitlab.com/g/r.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["repo"]["forge"], "gitlab");

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "bb", "url": "https://bitbucket.org/x/y.git", "forge": "bitbucket" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "unknown forge 'bitbucket' (choose from github, gitlab)"
    );
}

// -- §8 /api/repo/delete ----------------------------------------------------------------

#[tokio::test]
async fn repo_delete_refused_with_history_then_success_when_clean() {
    let state = test_state().await;
    // Repo 1 has 3 findings, 0 jobs -> exact store refusal message.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "repo 1 has 3 finding(s) and 0 job(s) -- cannot delete without losing history; \
         pause it instead"
    );

    // Repo 2 is clean -> deleted, bare {"ok": true} body.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "ok": true }));
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "deleted clean (#2)")
    );

    // Gone: a second delete is a 404.
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": 2 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no repo 2");
}

// -- §9 /api/repo/notes -----------------------------------------------------------------

#[tokio::test]
async fn repo_notes_append_201_and_validations() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": 1, "note": "check the flaky retry test", "category": "testing" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], json!(true));
    let notes = body["notes"].as_str().unwrap();
    assert!(notes.contains("# Notes: demo"), "notes: {notes}");
    assert!(notes.contains("## testing"), "notes: {notes}");
    assert!(
        notes.contains("check the flaky retry test"),
        "notes: {notes}"
    );
    let events = state.store.recent_events(10).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "repo" && e.message == "note added to demo [testing]")
    );

    let (status, body) = post(&state, "/api/repo/notes", json!({ "id": 1, "note": "  " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "note must be a non-empty string");

    let (status, body) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": 1, "note": "x", "category": 7 }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "category must be a string");

    let (status, body) = post(&state, "/api/repo/notes", json!({ "id": 99, "note": "x" })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no repo 99");
}

// -- §2 /api/cycle ------------------------------------------------------------------------

/// Contract §2: an idle scheduler accepts the trigger with 202.
#[tokio::test]
async fn cycle_starts_returns_202() {
    let state = test_state().await;
    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["started"], true);
}

/// Contract §2: the 202 is not cosmetic -- it must actually reach the loop.
/// The handler notifies before responding, so the permit is already held
/// and `notified()` resolves immediately.
#[tokio::test]
async fn cycle_wakes_the_scheduler_loop() {
    let state = test_state().await;
    let wake = Arc::clone(&state.scheduler.wake);
    let (status, _) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("POST /api/cycle must wake the scheduler loop");
}

/// Contract §2: a cycle already under way is refused, not queued twice.
#[tokio::test]
async fn cycle_while_running_is_409_busy() {
    let state = test_state().await;
    state.scheduler.running.store(true, Ordering::SeqCst);
    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"], "busy");
}

/// Deleting a repo must remove its notes file.
///
/// The original reason was id reuse: SQLite handed a freed rowid to the
/// next INSERT, so a notes file left behind was inherited by the next
/// repo to take that id. Migration 009 made `repos.id` AUTOINCREMENT
/// and closed that premise — see `a_reclaimed_id_is_never_reissued`.
///
/// The removal still has to happen, and since notes moved out of the
/// clone to `notes/repo-{id}.md` it has to be explicit: removing
/// `repo-{id}` no longer takes them with it. `reap_repo` only drops the
/// row once both are gone, so notes that survive keep a deleted repo's
/// private context on disk and its row alive indefinitely.
#[tokio::test]
async fn deleting_a_repo_removes_its_notes() {
    let state = test_state().await;

    // A repo with history cannot be deleted at all, so add a fresh one.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let (status, _) = post(
        &state,
        "/api/repo/notes",
        json!({ "id": rid, "note": "context for later" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    // Via the production path builder: notes have moved once already.
    let notes_path = Store::notes_path(&state.config.work_root, rid);
    assert!(notes_path.exists(), "fixture: notes file was written");

    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !notes_path.exists(),
        "notes survived the delete: the deleted repo's private notes are still on disk"
    );
}

/// ...and when it cannot remove them, it must say so.
///
/// Deletion is two-phase, so a failure here is not the caller's problem:
/// the row is already flagged deleted and invisible to every query, and
/// the reaper owns the retry. The caller gets a 200.
///
/// What the failure must not do is drop the row, because the row is what
/// holds the id while the old repo's files are still sitting at its path.
///
/// The obstruction is `repos/` made read-only: unlinking `repo-{id}`
/// needs write permission on the parent, not on the directory itself.
///
/// The envelope stays generic — `ApiError::Internal` deliberately never
/// leaks internals to a client — so the id and the remediation reach the
/// operator through the log, and that is where they are asserted.
#[tokio::test]
async fn a_repo_whose_files_survive_keeps_its_id_reserved_until_reaped() {
    require_enforced_permissions();
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // Make the repo's directory impossible to remove by taking write
    // permission off its parent, which is what the handler needs in order
    // to unlink the entry.
    let repos_root = state.config.work_root.join("repos");
    let repo_dir = repos_root.join(format!("repo-{rid}"));
    std::fs::create_dir_all(&repo_dir).unwrap();
    std::fs::write(repo_dir.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_root).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_root, perms).unwrap();
    let sink = LogSink::default();
    let capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish(),
    );
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    drop(capture);
    // Before any assertion can panic: a directory left unwritable defeats
    // the scratch dir's cleanup and leaks it into /tmp for good.
    let mut perms = std::fs::metadata(&repos_root).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_root, perms).unwrap();
    // The repo *is* deleted — only reclaiming its files failed, and that
    // is not the caller's problem to retry.
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let logged = sink.text();
    assert!(
        logged.contains(&format!("repo {rid} deleted, but reclaiming "))
            && logged.contains("its files stay until the reaper retries"),
        "a failed reclamation must say the id is still held: {logged}"
    );

    // No retry here on purpose: a second delete re-attempts reclamation,
    // which would succeed now that permissions are restored and release
    // the very id this test is about. Retry behaviour has its own test.

    // ...but the row is still holding the id, which is the whole point:
    // handing it to the next INSERT while those files remain is what put
    // one repo's clone under another repo's name.
    assert_eq!(
        state.store.deleted_repo_ids().await.unwrap(),
        vec![rid],
        "an unreclaimed repo must keep its id reserved"
    );

    // The reaper finishes the job once the obstruction is gone.
    let reaped = hunter::server::reap_deleted_repos(
        &state.store,
        &state.config.work_root,
        &state.repo_notes,
    )
    .await;
    assert_eq!(reaped, 1);
    assert!(!repo_dir.exists(), "the reaper must remove the directory");
    assert!(
        state.store.deleted_repo_ids().await.unwrap().is_empty(),
        "and only then release the id"
    );
}

/// The same must hold when it is the *notes* that cannot be removed.
///
/// Reclamation removes two things, and the sibling test above only ever
/// reaches the first: the clone directory fails, so the notes removal is
/// never attempted and neither is the branch that has to propagate its
/// failure. Make notes best-effort — `let _ = remove_file(..)`, which is
/// tempting since they are secondary to the clone — and the row is
/// dropped while the operator's private context is still on disk, with
/// nothing left that will ever retry it. That is the one outcome
/// two-phase deletion exists to prevent, and nothing else fails.
///
/// The obstruction is a directory at the notes path: `unlink` refuses a
/// directory outright, so this needs no permission games and holds under
/// root, where the sibling test has to bow out.
#[tokio::test]
async fn notes_that_cannot_be_removed_keep_the_id_reserved_too() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "throwaway", "url": "https://example.com/throwaway.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let notes_path = Store::notes_path(&state.config.work_root, rid);
    std::fs::create_dir_all(&notes_path).unwrap();
    std::fs::write(notes_path.join("occupied"), "x").unwrap();
    // The clone was never made, so its removal reports NotFound and is
    // skipped -- this test only gets to say anything because the notes
    // step runs after it.
    let clone_dir = Store::repo_dir(&state.config.work_root.join("repos"), rid);
    assert!(
        !clone_dir.exists(),
        "fixture: the clone must be absent, or it fails first"
    );

    let sink = LogSink::default();
    let capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(sink.clone())
            .finish(),
    );
    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    drop(capture);
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        state.store.deleted_repo_ids().await.unwrap(),
        vec![rid],
        "notes left on disk must keep the repo reapable, not release its id"
    );
    let logged = sink.text();
    assert!(
        logged.contains(&format!("repo {rid} deleted, but reclaiming ")),
        "a failed reclamation must be logged: {logged}"
    );

    // And once the obstruction is gone the reaper finishes, which is what
    // makes the retained row a retry rather than a wedge.
    std::fs::remove_dir_all(&notes_path).unwrap();
    let reaped = hunter::server::reap_deleted_repos(
        &state.store,
        &state.config.work_root,
        &state.repo_notes,
    )
    .await;
    assert_eq!(reaped, 1);
    assert!(
        state.store.deleted_repo_ids().await.unwrap().is_empty(),
        "and only then release the id"
    );
}

/// A repo URL that would execute when clicked is rejected at the write.
///
/// Defence in depth, not the only guard: `ReposPage` builds an `<a href>`
/// only when `isHttpUrl(repo.url)` accepts the value and renders anything
/// else as text, so such a URL is inert in today's UI. The write gate
/// matters because the UI is not the only reader of `repo.url`, a guard on
/// one page is one refactor from being dropped, and rejecting once at the
/// write is cheaper than escaping every read.
/// Both spellings of an ssh clone URL must keep working: the scp-like
/// `git@host:owner/repo.git`, which carries no scheme at all, and the
/// explicit `ssh://git@host/owner/repo.git`, which is allow-listed.
#[tokio::test]
async fn repo_urls_with_an_executable_scheme_are_rejected() {
    let state = test_state().await;

    for bad in [
        "javascript:alert(document.domain)",
        "JavaScript:alert(1)",
        "data:text/html,<script>alert(1)</script>",
        "vbscript:msgbox(1)",
    ] {
        let (status, body) =
            post(&state, "/api/repos", json!({ "name": "evil", "url": bad })).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} was accepted: {body}"
        );
    }

    // And the schemes the daemon actually clones with still work.
    for good in [
        "https://github.com/acme/widget.git",
        "http://git.internal/acme/widget.git",
        "git@github.com:acme/widget.git",
        // The rejection message promises ssh; the explicit spelling of an
        // ssh clone URL has to be accepted for that to be true.
        "ssh://git@github.com/acme/widget.git",
    ] {
        let name = format!("ok{}", good.len());
        let (status, body) = post(&state, "/api/repos", json!({ "name": name, "url": good })).await;
        assert_eq!(status, StatusCode::CREATED, "{good} was rejected: {body}");
    }
}

/// The same gate on the update path, which takes a url too.
#[tokio::test]
async fn updating_a_repo_url_is_validated_as_well() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "url": "javascript:alert(1)" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    let (status, _) = post(
        &state,
        "/api/repo",
        json!({ "id": 1, "url": "git@github.com:acme/widget.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// Deleting a repo removes everything keyed to its id.
///
/// Ids are no longer reused (migration 009 made `repos.id` AUTOINCREMENT),
/// but the directory removal is still what releases the row: `reap_repo`
/// deletes the record only once `repo-{id}` is gone. Leaving the clone
/// behind used to hand it to the next repo on that id, and `sync_repo`
/// skips cloning when the path already exists.
#[tokio::test]
async fn deleting_a_repo_removes_its_directory() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "ephemeral", "url": "https://example.com/e.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let repo_dir = std::path::PathBuf::from(body["repo"]["path"].as_str().unwrap());

    // Stand in for a clone and its notes.
    std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
    std::fs::write(repo_dir.join("NOTES.md"), "secret").unwrap();
    std::fs::write(repo_dir.join(".git/config"), "[remote]").unwrap();

    let (status, body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        !repo_dir.exists(),
        "{} survived the delete, so repo {rid}'s row cannot be released and its clone is still on disk",
        repo_dir.display()
    );
}

/// The name comes back immediately; the id never does.
///
/// A name is the operator's to reuse the moment they are told the repo is
/// gone -- blocking it on a filesystem operation they cannot see would be
/// inexplicable. The id is never reissued at all: `repos.id` is
/// AUTOINCREMENT (migration 009), so it cannot land on the old clone the
/// way a recycled rowid once put one repo's code under another's name.
/// What pending reclamation governs is only the name and the clone path.
#[tokio::test]
async fn deleting_frees_the_name_at_once_and_never_reissues_the_id() {
    require_enforced_permissions();
    let state = test_state().await;
    let repos_dir = state.config.work_root.join("repos");

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "recycled", "url": "https://example.com/a.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let first = body["repo"]["id"].as_i64().unwrap();

    // Block reclamation so the flagged row is still there afterwards.
    let blocked = repos_dir.join(format!("repo-{first}"));
    std::fs::create_dir_all(&blocked).unwrap();
    std::fs::write(blocked.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": first })).await;

    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_dir, perms).unwrap();
    assert_eq!(status, StatusCode::OK);

    // Name is free at once, even though the files are not yet reclaimed.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "recycled", "url": "https://example.com/b.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let second = body["repo"]["id"].as_i64().unwrap();

    // ...and the new repo did NOT get handed the old one's id. Since
    // migration 009 made `repos.id` AUTOINCREMENT that holds whether or
    // not reclamation is still pending -- `a_reclaimed_id_is_never_reissued`
    // proves the reaped case -- so this assertion is about the id never
    // moving backwards, not about the flagged row occupying the rowid.
    // What pending reclamation actually governs here is the name being
    // free again and the clone path below.
    assert_ne!(
        second, first,
        "the id was reissued while the previous repo's clone was still on disk"
    );
    assert_ne!(
        body["repo"]["path"].as_str().unwrap(),
        blocked.to_string_lossy(),
        "the replacement must not be pointed at the old clone"
    );
}

/// One deletion, one event.
///
/// Deletion became two phases, and the log call that belonged to the old
/// single phase was left behind next to the new one, so every delete
/// wrote the same row twice — visible in `GET /api/events` and in the log
/// view. The contract specifies one.
#[tokio::test]
async fn deleting_a_repo_logs_exactly_one_event() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "logged-once", "url": "https://example.com/l.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().unwrap();

    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(status, StatusCode::OK);

    let events = state.store.recent_events(200).await.unwrap();
    let deletions: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains(&format!("deleted logged-once (#{rid})")))
        .collect();
    assert_eq!(
        deletions.len(),
        1,
        "expected a single deletion event, got {deletions:#?}"
    );
}

/// A retry lands as success while the deletion is still pending.
///
/// Soft deletion made the row invisible to `get_repo_by_id`, so a client
/// that lost the first response and retried was told the repo never
/// existed — for a request that had in fact succeeded.
///
/// The window is bounded by reclamation, deliberately. Once the
/// directory is gone the row is gone too, and that id is then
/// indistinguishable from one that was never a repo; answering 200
/// forever would mean keeping a tombstone per deleted repo to buy
/// nothing but a status code. So: 200 while pending, 404 after, and 404
/// for an id that never existed.
#[tokio::test]
async fn a_retried_delete_is_a_success_while_reclamation_is_pending() {
    require_enforced_permissions();
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "twice", "url": "https://example.com/t.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().unwrap();

    // Block reclamation so the row stays flagged and the deletion is
    // genuinely still pending when the retry arrives.
    let repos_dir = state.config.work_root.join("repos");
    let dir = repos_dir.join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("occupied"), "x").unwrap();
    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    let (first, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    let (retry, retry_body) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;

    let mut perms = std::fs::metadata(&repos_dir).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&repos_dir, perms).unwrap();

    assert_eq!(first, StatusCode::OK);
    assert_eq!(retry, StatusCode::OK, "a retry must not 404: {retry_body}");

    // Once reclaimed, the id is ordinary again and answers 404.
    hunter::server::reap_deleted_repos(&state.store, &state.config.work_root, &state.repo_notes)
        .await;
    let (after, _) = post(&state, "/api/repo/delete", json!({ "id": rid })).await;
    assert_eq!(after, StatusCode::NOT_FOUND);

    let (unknown, body) = post(&state, "/api/repo/delete", json!({ "id": 987_654 })).await;
    assert_eq!(
        unknown,
        StatusCode::NOT_FOUND,
        "an id that was never a repo is still a 404: {body}"
    );
}

/// A fully reclaimed id is never handed out again.
///
/// Every repo-lifecycle hazard on this branch descended from id reuse: a
/// replacement inheriting a deleted repo's clone, its notes, its cached
/// notes in the browser. Two-phase deletion closes the window where
/// *files* outlive the row, but not the one where a *client* does — a
/// browser tab holding the old id can pause, delete or annotate whatever
/// repo now answers to it, and that window is as long as the tab is
/// open. AUTOINCREMENT removes the premise: `sqlite_sequence` only moves
/// forward, so a stale id can only ever miss.
#[tokio::test]
async fn a_reclaimed_id_is_never_reissued() {
    let state = test_state().await;

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "first", "url": "https://example.com/1.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let first = body["repo"]["id"].as_i64().unwrap();

    // Delete it and let reclamation finish, so the row is entirely gone —
    // the state in which a rowid alias would be reissued.
    let (status, _) = post(&state, "/api/repo/delete", json!({ "id": first })).await;
    assert_eq!(status, StatusCode::OK);
    hunter::server::reap_deleted_repos(&state.store, &state.config.work_root, &state.repo_notes)
        .await;
    assert_eq!(
        state.store.deleted_repo_ids().await.unwrap(),
        [] as [i64; 0]
    );

    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "second", "url": "https://example.com/2.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let second = body["repo"]["id"].as_i64().unwrap();

    assert!(
        second > first,
        "id {first} was reissued as {second}; a client still holding {first} \
         would now be addressing a different repository"
    );
}

/// Changing a repo's URL must not strand its clone.
///
/// `sync_repo` refuses to work in a directory whose origin is not the
/// repo's URL, which is how it recognises somebody else's checkout at
/// the path. Leaving the clone on the old origin turned that check into
/// a permanent block: every job for the repo failed until the directory
/// was removed by hand.
#[tokio::test]
async fn changing_the_url_repoints_the_existing_clone() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "movable", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // A real clone carrying the old origin.
    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }

    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (rc, out) = hunter::util::run_cmd(&["git", "-C", &d, "remote", "get-url", "origin"], 30);
    assert_eq!(rc, 0);
    assert_eq!(
        out.trim(),
        "https://example.com/new.git",
        "the clone must follow the repo to its new url"
    );
}

/// A URL update takes the clone's permit before it writes the new URL.
///
/// Written first, a runner taking the permit in between read the new URL
/// while the clone still had the old origin: `sync_repo` refused it as
/// somebody else's checkout and counted that against the repo. So while
/// another holder (a runner mid-fetch) has the clone, the update waits
/// with the row untouched, then writes and repoints together.
#[tokio::test]
async fn a_url_update_waits_for_the_clones_holder_before_writing() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "busy", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }
    let url_now = || async { state.store.get_repo_by_id(rid).await.unwrap().unwrap().url };

    let held = state.store.test_lane().lock_clone(rid).await.unwrap();
    let update = tokio::spawn({
        let app = AppState::clone(&state);
        async move {
            post(
                &app,
                "/api/repo",
                json!({ "id": rid, "url": "https://example.com/new.git" }),
            )
            .await
        }
    });
    support::until_waiting(&state.store, rid, 1).await;
    assert!(!update.is_finished(), "the update waits for the clone");
    assert_eq!(
        url_now().await,
        "https://example.com/old.git",
        "nothing is written while another holder has the clone"
    );

    drop(held);
    let (status, body) = update.await.unwrap();
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(url_now().await, "https://example.com/new.git");
    let (_, out) = hunter::util::run_cmd(&["git", "-C", &d, "remote", "get-url", "origin"], 30);
    assert_eq!(out.trim(), "https://example.com/new.git");
    // What a sync checks the clone's origin against, read under the
    // permit: the URL the clone was just moved to.
    let permit = state.store.test_lane().lock_clone(rid).await.unwrap();
    assert_eq!(
        state.store.origin_url(&permit).await.unwrap(),
        "https://example.com/new.git"
    );
}

/// Two URL updates that overlap leave the row and the clone's origin on
/// the same URL. Both read the repo before either takes the clone; the
/// second used to repoint from that stale read, found the clone already
/// moved by the first, left it, and wrote its own URL beside the first's
/// origin -- so every later sync refused the clone as somebody else's.
/// The old URL of a repoint is the one the row has under the permit.
#[tokio::test]
async fn overlapping_url_updates_leave_the_row_and_the_origin_agreeing() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "contested", "url": "https://example.com/u0.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");
    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/u0.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }
    let update = |url: &'static str| {
        let app = AppState::clone(&state);
        tokio::spawn(async move { post(&app, "/api/repo", json!({ "id": rid, "url": url })).await })
    };

    // Both queue behind a runner holding the clone, each having read U0.
    let held = state.store.test_lane().lock_clone(rid).await.unwrap();
    let first = update("https://example.com/u1.git");
    support::until_waiting(&state.store, rid, 1).await;
    let second = update("https://example.com/u2.git");
    support::until_waiting(&state.store, rid, 2).await;
    drop(held);
    for done in [first, second] {
        let (status, body) = done.await.unwrap();
        assert_eq!(status, StatusCode::OK, "body: {body}");
    }

    let row = state.store.get_repo_by_id(rid).await.unwrap().unwrap().url;
    let (_, origin) = hunter::util::run_cmd(
        &["git", "-C", &d, "config", "--get", "remote.origin.url"],
        30,
    );
    assert_eq!(origin.trim(), row, "the clone's origin is the row's URL");
}

/// Only a URL change touches the clone. An update that sends the URL the
/// repo already has, beside a field that is not the clone's, does not
/// wait for a runner holding the clone mid-fetch.
#[tokio::test]
async fn an_update_keeping_the_url_does_not_wait_for_the_clone() {
    let state = test_state().await;
    let held = state.store.test_lane().lock_clone(1).await.unwrap();
    let update = tokio::spawn({
        let app = AppState::clone(&state);
        async move {
            post(
                &app,
                "/api/repo",
                json!({ "id": 1, "url": "https://example.com/demo.git", "enabled": false }),
            )
            .await
        }
    });
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(5), update)
        .await
        .expect("the update waited for a clone it does not change")
        .unwrap();
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["repo"]["enabled"], 0);
    drop(held);
}

/// The repoint reads the origin the way `sync_repo` does, so an insteadOf
/// rule in the user's gitconfig does not stop it.
///
/// `remote get-url` prints the URL after `url.<base>.insteadOf`
/// rewriting, which never equals the stored old URL under such a rule;
/// the clone was left on the old origin and `sync_repo` then refused it
/// on every job. The rule sits in the clone's own config here so the
/// test touches no global git config; git applies it identically.
#[tokio::test]
async fn changing_the_url_repoints_a_clone_under_an_insteadof_rule() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "aliased", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
        vec![
            "git",
            "-C",
            &d,
            "config",
            "url.git@example.com:.insteadOf",
            "https://example.com/",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }

    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (rc, out) = hunter::util::run_cmd(
        &["git", "-C", &d, "config", "--get", "remote.origin.url"],
        30,
    );
    assert_eq!(rc, 0);
    assert_eq!(
        out.trim(),
        "https://example.com/new.git",
        "the clone must follow the repo to its new url"
    );
}

/// A clone belonging to something else is left alone to trip the check.
#[tokio::test]
async fn changing_the_url_leaves_an_unrelated_clone_untouched() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "occupied", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    let dir = state
        .config
        .work_root
        .join("repos")
        .join(format!("repo-{rid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &d, "init", "-q"],
        vec![
            "git",
            "-C",
            &d,
            "remote",
            "add",
            "origin",
            "https://example.com/someone-else.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }

    let (status, _) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, out) = hunter::util::run_cmd(&["git", "-C", &d, "remote", "get-url", "origin"], 30);
    assert_eq!(
        out.trim(),
        "https://example.com/someone-else.git",
        "an unrelated checkout must not be rewritten -- that refusal is the point"
    );
}

/// The clone that gets repointed is the one the scheduler actually uses.
///
/// Migration 007 left any path that did not end in the repo's name
/// exactly as it was — an operator-edited one — and every scheduler
/// runner syncs `repo.path`. A repoint that derived the directory from
/// the id would rewrite nothing for such a row while `sync_repo` kept
/// finding the old origin, which is the permanent refusal this is meant
/// to avoid.
#[tokio::test]
async fn the_repoint_follows_an_operator_edited_path() {
    let state = test_state().await;
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "moved", "url": "https://example.com/old.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
    let rid = body["repo"]["id"].as_i64().expect("new repo id");

    // A clone somewhere the id does not name, recorded on the row.
    let custom = state.config.work_root.join("elsewhere").join("checkout");
    std::fs::create_dir_all(&custom).unwrap();
    let cs = custom.to_string_lossy().to_string();
    for argv in [
        vec!["git", "-C", &cs, "init", "-q"],
        vec![
            "git",
            "-C",
            &cs,
            "remote",
            "add",
            "origin",
            "https://example.com/old.git",
        ],
    ] {
        let (rc, out) = hunter::util::run_cmd(&argv, 30);
        assert_eq!(rc, 0, "fixture: {argv:?} failed: {out}");
    }
    // Straight to the column: no production code path sets a custom
    // path, which is the point -- these rows predate the id layout.
    let pool = sqlx::SqlitePool::connect(&format!(
        "sqlite://{}",
        state.config.db_path.to_string_lossy()
    ))
    .await
    .unwrap();
    sqlx::query("UPDATE repos SET path = ?1 WHERE id = ?2")
        .bind(&cs)
        .bind(rid)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let (status, body) = post(
        &state,
        "/api/repo",
        json!({ "id": rid, "url": "https://example.com/new.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (_, out) = hunter::util::run_cmd(&["git", "-C", &cs, "remote", "get-url", "origin"], 30);
    assert_eq!(
        out.trim(),
        "https://example.com/new.git",
        "the clone named by repo.path is the one the scheduler syncs"
    );
}

/// The scratch directory does not outlive the test that made it.
///
/// Each state carries a copy of `dev.db` plus whatever the POSTs wrote
/// under `work_root`; left behind, a full run of this suite deposits one
/// directory per test in the system temp dir, which on this machine is a
/// tmpfs shared with every build. It filled twice in one day, and a
/// truncated file mid-checkout is how that failure presents — nowhere
/// near the tests that caused it.
#[tokio::test]
async fn the_scratch_dir_is_removed_with_the_state() {
    let root = {
        let state = test_state().await;
        // Writes so the directory is not merely the db copy.
        let (status, _) = post(&state, "/api/repo/notes", json!({ "id": 1, "note": "x" })).await;
        assert_eq!(status, StatusCode::CREATED);
        state.config.root.clone()
    };
    assert!(
        !root.exists(),
        "{} outlived its test and leaks into the temp dir",
        root.display()
    );
}

/// A URL git would read as an option is refused at the door.
///
/// `git clone --upload-pack=<cmd> <dir>` runs `<cmd>`, and the clone is
/// built from whatever URL the row holds, so accepting one here is what
/// turns a repo row into an argument. The clone passes `--` as well;
/// this is the half that keeps the value out of the database.
#[tokio::test]
async fn a_url_that_looks_like_a_git_option_is_refused() {
    let state = test_state().await;

    for bad in [
        "--upload-pack=touch /tmp/pwned",
        "-u../../etc/passwd",
        "--config=core.sshCommand=id",
    ] {
        let (status, body) =
            post(&state, "/api/repos", json!({ "name": "probe", "url": bad })).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} must not be accepted; body: {body}"
        );
    }

    // A hyphen anywhere else is ordinary and must still work.
    let (status, body) = post(
        &state,
        "/api/repos",
        json!({ "name": "fine", "url": "https://github.com/acme/my-repo.git" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "body: {body}");
}

// -- /api/scheduler (pause / resume) ---------------------------------------------

async fn summary_paused(state: &AppState) -> bool {
    let cookie = support::auth_cookie(&state.store).await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .header(axum::http::header::COOKIE, cookie.as_str())
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["scheduler_paused"]
        .as_bool()
        .expect("/api/summary must report scheduler_paused")
}

/// Pausing is a round trip the dashboard can see: the flag the daemon
/// loop reads, the summary field the button renders from, and the refusal
/// of a manual cycle while paused -- then all three undone by resuming.
#[tokio::test]
async fn pausing_and_resuming_the_scheduler_round_trips() {
    let state = test_state().await;
    assert!(
        !summary_paused(&state).await,
        "a fresh daemon is not paused"
    );

    let (status, body) = post(&state, "/api/scheduler", json!({ "paused": true })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "paused": true }));
    assert!(
        state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst),
        "the flag the daemon loop reads must be set"
    );
    assert!(
        summary_paused(&state).await,
        "the dashboard must see the pause"
    );

    let (status, body) = post(&state, "/api/cycle", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body, json!({ "error": "paused" }));

    let (status, body) = post(&state, "/api/scheduler", json!({ "paused": false })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "paused": false }));
    assert!(
        !state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert!(!summary_paused(&state).await);
}

/// Only a JSON boolean pauses. A truthy string must not, and must not be
/// read as a resume either: the flag stays where it was.
#[tokio::test]
async fn a_non_boolean_pause_request_is_refused() {
    let state = test_state().await;
    for body in [
        json!({ "paused": "yes" }),
        json!({ "paused": 1 }),
        json!({}),
    ] {
        let (status, got) = post(&state, "/api/scheduler", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {got}");
        assert_eq!(got, json!({ "error": "paused must be a boolean" }));
    }
    assert!(
        !state
            .scheduler
            .paused
            .load(std::sync::atomic::Ordering::SeqCst)
    );
}

// -- /api/overdrive -----------------------------------------------------------

async fn summary_overdrive(state: &AppState) -> bool {
    let cookie = support::auth_cookie(&state.store).await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .header(axum::http::header::COOKIE, cookie.as_str())
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    v["scheduler_overdrive"]
        .as_bool()
        .expect("/api/summary must report scheduler_overdrive")
}

#[tokio::test]
async fn overdrive_round_trips_and_wakes_the_scheduler() {
    let state = test_state().await;
    assert!(!summary_overdrive(&state).await);

    let (status, body) = post(&state, "/api/overdrive", json!({ "enabled": "yes" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    assert_eq!(body, json!({ "error": "enabled must be a boolean" }));
    assert!(!summary_overdrive(&state).await);

    let (status, body) = post(&state, "/api/overdrive", json!({ "enabled": true })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "overdrive": true }));
    assert!(
        state
            .scheduler
            .overdrive
            .load(std::sync::atomic::Ordering::SeqCst),
        "the flag shared with the budget backend must be set"
    );
    assert!(summary_overdrive(&state).await);
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            state.scheduler.wake.notified()
        )
        .await
        .is_ok(),
        "enabling overdrive must wake a budget-blocked scheduler"
    );

    let (status, body) = post(&state, "/api/overdrive", json!({ "enabled": false })).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body, json!({ "overdrive": false }));
    assert!(!summary_overdrive(&state).await);
}

// -- accounts and sessions -------------------------------------------------------

/// One request with an optional `Cookie`: (status, `Set-Cookie`, body).
async fn request(
    state: &AppState,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Option<String>, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(cookie) = cookie {
        builder = builder.header(axum::http::header::COOKIE, cookie);
    }
    let body = match body {
        Some(json) => {
            builder = builder.header("content-type", "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    let response = router(state.clone())
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let set_cookie = response
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .map(|v| v.to_str().unwrap().to_owned());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, set_cookie, value)
}

/// The `name=value` part of a `Set-Cookie` header, as a browser sends it back.
fn cookie_pair(set_cookie: &str) -> String {
    set_cookie.split(';').next().unwrap().to_owned()
}

/// Log `username` in through the API; the session cookie to send back.
async fn log_in(state: &AppState, username: &str, password: &str) -> String {
    let (status, set_cookie, body) = request(
        state,
        "POST",
        "/api/login",
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    cookie_pair(&set_cookie.expect("a login sets the session cookie"))
}

/// Every API route but `/api/login` answers 401 to a request without a
/// live session -- no cookie, a made-up token, or the cookie of a session
/// that ended -- reads and writes alike, before any handler runs.
#[tokio::test]
async fn every_api_route_needs_a_session() {
    let state = test_state().await;
    let routes = [
        ("GET", "/api/summary"),
        ("GET", "/api/findings"),
        ("GET", "/api/finding?id=1"),
        ("GET", "/api/jobs"),
        ("GET", "/api/repos"),
        ("POST", "/api/repos"),
        ("POST", "/api/repo"),
        ("GET", "/api/repo/notes?id=1"),
        ("POST", "/api/repo/notes"),
        ("POST", "/api/repo/delete"),
        ("POST", "/api/repo/rehunt"),
        ("GET", "/api/events"),
        ("GET", "/api/stats"),
        ("POST", "/api/verdict"),
        ("POST", "/api/cycle"),
        ("POST", "/api/scheduler"),
        ("POST", "/api/overdrive"),
        ("POST", "/api/recheck"),
        ("POST", "/api/unqueue"),
        ("POST", "/api/override"),
        ("POST", "/api/hold/release"),
        ("GET", "/api/me"),
        ("POST", "/api/logout"),
    ];
    let ended = support::auth_cookie(&state.store).await;
    let (status, _, _) =
        request(&state, "POST", "/api/logout", Some(&ended), Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);

    for cookie in [None, Some("hunter_session=made-up"), Some(ended.as_str())] {
        for (method, path) in routes {
            let body = (method == "POST").then(|| json!({ "id": 1 }));
            let (status, _, body) = request(&state, method, path, cookie, body).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} with {cookie:?}"
            );
            assert_eq!(
                body,
                json!({ "error": "login required" }),
                "{method} {path}"
            );
        }
    }
    let after = state.store.get_finding(1).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        hunter::domain::FindingStatus::New,
        "nothing was written"
    );
}

/// A login sets an `HttpOnly`, `SameSite=Strict` session cookie that
/// identifies the account; logging out ends that session and clears the
/// cookie.
#[tokio::test]
async fn a_login_session_lasts_until_logout() {
    let state = test_state().await;
    hunter::auth::add_user(&state.store, "Alice", "correct horse battery".to_owned())
        .await
        .unwrap();

    let (status, set_cookie, body) = request(
        &state,
        "POST",
        "/api/login",
        None,
        Some(json!({ "username": "alice", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "username": "Alice" }),
        "names compare case-insensitively"
    );
    let set_cookie = set_cookie.unwrap();
    // Whole attributes, not substrings: `Max-Age=2592000000` must not pass.
    let attributes: Vec<&str> = set_cookie.split(';').map(str::trim).collect();
    for attribute in ["HttpOnly", "SameSite=Strict", "Path=/", "Max-Age=2592000"] {
        assert!(attributes.contains(&attribute), "{set_cookie}");
    }
    let cookie = cookie_pair(&set_cookie);

    let (status, _, body) = request(&state, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(
        (status, body),
        (StatusCode::OK, json!({ "username": "Alice" }))
    );

    let (status, cleared, _) = request(
        &state,
        "POST",
        "/api/logout",
        Some(&cookie),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(cleared.unwrap().contains("Max-Age=0"));
    let (status, _, _) = request(&state, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the session ended with the logout"
    );
}

/// A wrong password, an unknown name and a disabled account get the same
/// answer, and none of them gets a cookie.
#[tokio::test]
async fn failed_logins_look_alike() {
    let state = test_state().await;
    hunter::auth::add_user(&state.store, "alice", "correct horse battery".to_owned())
        .await
        .unwrap();
    hunter::auth::add_user(&state.store, "bob", "another long password".to_owned())
        .await
        .unwrap();
    hunter::auth::disable_user(&state.store, "bob")
        .await
        .unwrap();

    for (username, password) in [
        ("alice", "wrong password!!"),
        ("nobody", "correct horse battery"),
        ("bob", "another long password"),
    ] {
        let (status, set_cookie, body) = request(
            &state,
            "POST",
            "/api/login",
            None,
            Some(json!({ "username": username, "password": password })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{username}");
        assert_eq!(
            body,
            json!({ "error": "invalid username or password" }),
            "{username}"
        );
        assert!(set_cookie.is_none(), "{username}");
    }
}

/// Changing a password or disabling an account ends every session it
/// has, so a leaked password stops working everywhere at once; a session
/// past its expiry is refused too.
#[tokio::test]
async fn sessions_end_with_a_password_change_a_disable_or_their_expiry() {
    let state = test_state().await;
    hunter::auth::add_user(&state.store, "alice", "correct horse battery".to_owned())
        .await
        .unwrap();
    hunter::auth::add_user(&state.store, "bob", "another long password".to_owned())
        .await
        .unwrap();

    let alice = log_in(&state, "alice", "correct horse battery").await;
    hunter::auth::change_password(&state.store, "alice", "a brand new password".to_owned())
        .await
        .unwrap();
    let (status, _, _) = request(&state, "GET", "/api/me", Some(&alice), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "after passwd");
    log_in(&state, "alice", "a brand new password").await;

    let bob = log_in(&state, "bob", "another long password").await;
    hunter::auth::disable_user(&state.store, "bob")
        .await
        .unwrap();
    let (status, _, _) = request(&state, "GET", "/api/me", Some(&bob), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "after disable");

    let (user, _) = state.store.login_candidate("alice").await.unwrap().unwrap();
    let token = hunter::auth::new_session_token().unwrap();
    let now = hunter::util::now_ms();
    state
        .store
        .create_session(&user, &hunter::auth::token_hash(&token), now + 60_000)
        .await
        .unwrap();
    let cookie = format!("hunter_session={token}");
    let (status, _, _) = request(&state, "GET", "/api/me", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "a minute before expiry");
    assert_eq!(
        state
            .store
            .session_user(&hunter::auth::token_hash(&token), now + 60_000)
            .await
            .unwrap(),
        None,
        "at its expiry"
    );
}

/// Accounts are created with a valid name and a long enough password,
/// once per name regardless of case.
#[tokio::test]
async fn accounts_need_a_valid_name_a_long_password_and_a_free_name() {
    let state = test_state().await;
    let add = |name: &'static str, password: &'static str| {
        hunter::auth::add_user(&state.store, name, password.to_owned())
    };
    assert!(
        add("alice", "short")
            .await
            .unwrap_err()
            .to_string()
            .contains("at least 12")
    );
    assert!(
        add("al ice", "correct horse battery")
            .await
            .unwrap_err()
            .to_string()
            .contains("may only contain")
    );
    assert!(add("", "correct horse battery").await.is_err());
    add("alice", "correct horse battery").await.unwrap();
    assert!(
        add("ALICE", "correct horse battery")
            .await
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
    assert!(
        hunter::auth::change_password(&state.store, "nobody", "correct horse battery".to_owned())
            .await
            .is_err()
    );
    assert!(
        hunter::auth::disable_user(&state.store, "nobody")
            .await
            .is_err()
    );
}

/// What a person does through the API is recorded as theirs, and the
/// event feed says who.
#[tokio::test]
async fn api_writes_are_attributed_to_the_logged_in_account() {
    let state = test_state().await;
    hunter::auth::add_user(&state.store, "alice", "correct horse battery".to_owned())
        .await
        .unwrap();
    let cookie = log_in(&state, "alice", "correct horse battery").await;

    let (status, _, body) = request(
        &state,
        "POST",
        "/api/verdict",
        Some(&cookie),
        Some(json!({ "id": 1, "status": "queued" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, _, events) = request(&state, "GET", "/api/events", Some(&cookie), None).await;
    let verdict = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "verdict")
        .expect("a verdict event");
    assert_eq!(verdict["username"], "alice");

    // The finding's own timeline says who, too.
    let (_, _, findings) = request(&state, "GET", "/api/findings", Some(&cookie), None).await;
    let timeline = &findings
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == 1)
        .expect("finding 1")["timeline"];
    let entry = timeline
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "verdict")
        .unwrap_or_else(|| panic!("no verdict in {timeline}"));
    assert_eq!(entry["username"], "alice");
    assert_eq!(entry["finding_id"], 1);
}

/// GET `path` as the test user and parse the JSON body.
async fn get_json(state: &AppState, path: &str) -> Value {
    let cookie = support::auth_cookie(&state.store).await;
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header(axum::http::header::COOKIE, cookie.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn verdict_reason_required_before_any_status_change() {
    let state = test_state().await;
    for verdict in ["rejected", "wontfix"] {
        for reason in [None, Some(Value::Null), Some(json!(" \n\t "))] {
            let mut request = json!({ "id": 1, "status": verdict });
            if let Some(reason) = reason {
                request["reason"] = reason;
            }
            let (status, _) = post(&state, "/api/verdict", request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "verdict {verdict}");
            let finding = state.store.get_finding(1).await.unwrap().unwrap();
            assert_eq!(finding.status, hunter::domain::FindingStatus::New);
            assert_eq!(finding.verdict_reason, None);
            assert_eq!(finding.updated_at, 1000);
        }
    }
    assert!(
        !state
            .store
            .recent_events(50)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "verdict")
    );
}

/// Finding 1 queued, its fix held as a missing prerequisite after a job
/// whose worker ran: the state, the job and the report.
async fn held_fix_state() -> (TestState, i64, String) {
    use hunter::domain::{FindingJobKind, FindingStatus, FinishedState, HoldReason};
    use hunter::store::JobOutcome;

    let state = test_state().await;
    state
        .store
        .set_finding_status(1, FindingStatus::Queued)
        .await
        .unwrap();
    let job_id = support::scope(&state.store)
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Fix, 1),
            Some(5000),
            Some(1000),
        )
        .await
        .unwrap()
        .job
        .complete(&JobOutcome {
            state: FinishedState::Done,
            tokens_new: 900,
            calls: 4,
            exit_code: Some(0),
            killed_reason: None,
            session_file: Some("/s/fix/session.jsonl"),
            notes: Some("implemented fix"),
            model: None,
            usage_delta: None,
            finished_at: 2000,
        })
        .await
        .unwrap()
        .id();
    let reason = format!(
        "Baseline verification is unavailable.\n{}\nDiagnostic tail",
        "x".repeat(800)
    );
    assert!(
        state
            .store
            .hold_work(
                1,
                FindingJobKind::Fix,
                HoldReason::Prerequisite,
                &reason,
                Some(job_id)
            )
            .await
            .unwrap()
    );
    (state, job_id, reason)
}

/// A fix held for the operator stays `queued`, unsuppressed, its work
/// item skipped; `/api/findings` shows the hold with the actions that can
/// release it, and `POST /api/hold/release` with `continue` gives the
/// scheduler back the very checkpoint the hold retained.
#[tokio::test]
async fn a_held_fix_is_shown_and_released_through_the_hold_endpoint() {
    let (state, job_id, reason) = held_fix_state().await;
    assert!(state.store.suppressions(1, "bug").await.unwrap().is_empty());
    let row = |rows: &Value, id: i64| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .cloned()
            .unwrap()
    };

    let rows = get_json(&state, "/api/findings").await;
    let held = row(&rows, 1);
    assert_eq!(held["status"], "queued");
    assert_eq!(
        held["verdict_reason"],
        Value::Null,
        "the report is not copied"
    );
    assert_eq!(held["holds"][0]["kind"], "fix");
    assert_eq!(held["holds"][0]["reason"], "prerequisite");
    assert_eq!(held["holds"][0]["detail"], reason.as_str());
    assert_eq!(
        held["holds"][0]["actions"],
        json!(["continue", "restart", "reject", "wontfix"])
    );
    assert_eq!(row(&rows, 2)["holds"], json!([]));

    for (body, expected) in [
        (
            json!({ "finding_id": 1, "kind": "fix", "action": "nope" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "finding_id": 1, "kind": "fix", "action": "reject" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "finding_id": 1, "kind": "fix", "action": "dismiss" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "finding_id": 1, "kind": "recheck", "action": "continue" }),
            StatusCode::CONFLICT,
        ),
    ] {
        let (status, reply) = post(&state, "/api/hold/release", body.clone()).await;
        assert_eq!(status, expected, "{body}: {reply}");
    }

    let (status, body) = post(
        &state,
        "/api/hold/release",
        json!({ "finding_id": 1, "kind": "fix", "action": "continue" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["finding"]["status"], "queued");
    let resumable = state.store.list_resumable_jobs().await.unwrap();
    assert_eq!(
        resumable.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![job_id]
    );
    assert_eq!(
        resumable[0].session_file.as_deref(),
        Some("/s/fix/session.jsonl")
    );
    let rows = get_json(&state, "/api/findings").await;
    assert_eq!(row(&rows, 1)["holds"], json!([]));
    let events = state.store.recent_events(10).await.unwrap();
    let released = events.iter().find(|e| e.kind == "release").unwrap();
    assert_eq!(released.username.as_deref(), Some("tester"), "{released:?}");

    let (status, body) = post(
        &state,
        "/api/hold/release",
        json!({ "finding_id": 1, "kind": "fix", "action": "continue" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "no longer active: {body}");
}

/// `reject` and `wontfix` release a held fix into the operator's verdict,
/// which needs its reason, as `/api/verdict` does.
#[tokio::test]
async fn rejecting_a_held_fix_through_the_hold_endpoint() {
    use hunter::domain::{FindingJobKind, FindingStatus, HoldReason};

    let state = test_state().await;
    state
        .store
        .set_finding_status(1, FindingStatus::Queued)
        .await
        .unwrap();
    assert!(
        state
            .store
            .hold_work(
                1,
                FindingJobKind::Fix,
                HoldReason::Stuck,
                "no commits",
                None
            )
            .await
            .unwrap()
    );

    let (status, body) = post(
        &state,
        "/api/hold/release",
        json!({ "finding_id": 1, "kind": "fix", "action": "wontfix", "reason": "out of scope" }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["finding"]["status"], "wontfix");
    assert_eq!(body["finding"]["verdict_reason"], "out of scope");
    assert_eq!(state.store.active_holds().await.unwrap(), []);
}

/// The operator gives a verdict only where the finding asks for one: on a
/// `new`, `note` or `closed` finding, and only to change its status. Held
/// work is released through `/api/hold/release` instead.
/// Everywhere else a job or the forge owns the finding (`fixing`,
/// `rechecking`, `pr_open`, ...), so the verdict is refused and the row is
/// left exactly as it was.
#[tokio::test]
async fn verdicts_follow_the_finding_state_machine() {
    use hunter::domain::FindingStatus;

    let state = test_state().await;
    let verdicts = FindingStatus::ALL.into_iter().filter(|s| s.is_verdict());
    for to in verdicts {
        for from in FindingStatus::ALL {
            support::seed_status(&state.config.db_path, 1, from).await;
            let before = state.store.get_finding(1).await.unwrap().unwrap();
            let (status, body) = post(
                &state,
                "/api/verdict",
                json!({ "id": 1, "status": to.as_str(), "reason": "operator reason" }),
            )
            .await;
            let after = state.store.get_finding(1).await.unwrap().unwrap();
            let asks = matches!(
                from,
                FindingStatus::New | FindingStatus::Note | FindingStatus::Closed
            );
            if asks && from != to {
                assert_eq!(status, StatusCode::OK, "{from} -> {to}: {body}");
                assert_eq!(after.status, to, "{from} -> {to}");
            } else {
                assert_eq!(status, StatusCode::BAD_REQUEST, "{from} -> {to}: {body}");
                assert_eq!(
                    body["error"],
                    format!(
                        "finding #1 is '{from}'; a verdict applies only to new, note or \
                         closed findings, and must change its status"
                    ),
                    "{from} -> {to}"
                );
                assert_eq!(after.status, before.status, "{from} -> {to}");
                assert_eq!(
                    after.verdict_reason, before.verdict_reason,
                    "{from} -> {to}"
                );
                assert_eq!(after.updated_at, before.updated_at, "{from} -> {to}");
            }
        }
    }
}

/// Every operator write is conditional on the status it just checked, so
/// a job that claims the finding in between keeps it. The race is staged
/// with a trigger: the operator's UPDATE first moves the row the way the
/// job would (here to `fixing` / `rejected`) and is then skipped, exactly
/// as if the job's write had landed between the endpoint's read and its
/// write. The answer is 409 and the job's status stands.
#[tokio::test]
async fn operator_writes_lose_to_a_status_change_after_their_check() {
    let state = test_state().await;
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(state.dir.join("hunter.db")),
    )
    .await
    .unwrap();
    // A verdict with a reason and one without take different writes
    // (`Store::transition` writes the reason and anchor only for the
    // first), so both are staged.
    for (path, id, operator_to, reason, job_to) in [
        ("/api/unqueue", 2, "new", None, "fixing"),
        ("/api/recheck", 1, "rechecking", None, "rejected"),
        ("/api/verdict", 3, "note", None, "rechecking"),
        (
            "/api/verdict",
            3,
            "rejected",
            Some("duplicate"),
            "rechecking",
        ),
    ] {
        sqlx::query("UPDATE findings SET status = 'new' WHERE id = 3")
            .execute(&pool)
            .await
            .unwrap();
        // Test-only DDL built from the fixed table above.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP TRIGGER IF EXISTS job_first; \
             CREATE TRIGGER job_first BEFORE UPDATE OF status ON findings \
             WHEN OLD.id = {id} AND NEW.status = '{operator_to}' BEGIN \
               UPDATE findings SET status = '{job_to}' WHERE id = {id}; \
               SELECT RAISE(IGNORE); \
             END;"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let body = if path == "/api/verdict" {
            json!({ "id": id, "status": operator_to, "reason": reason })
        } else {
            json!({ "id": id })
        };
        let (status, body) = post(&state, path, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{path}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("reload and retry"),
            "{path}: {body}"
        );
        let after = state.store.get_finding(id).await.unwrap().unwrap();
        assert_eq!(after.status.as_str(), job_to, "{path} {reason:?}");
        assert_eq!(after.verdict_reason, None, "{path} {reason:?}");
    }
}

/// A verdict, a recheck request and an unqueue all change what the
/// scheduler should pick next, so each wakes the loop instead of waiting
/// for its next natural wake (up to 15 minutes when idle).
#[tokio::test]
async fn queue_changes_wake_the_scheduler_loop() {
    for (path, body) in [
        ("/api/verdict", json!({ "id": 1, "status": "queued" })),
        ("/api/recheck", json!({ "id": 3 })),
        ("/api/unqueue", json!({ "id": 2 })),
    ] {
        let state = test_state().await;
        let wake = Arc::clone(&state.scheduler.wake);
        let (status, body) = post(&state, path, body).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
            .await
            .unwrap_or_else(|_| panic!("{path} must wake the scheduler loop"));
    }
}

// -- §8.1 /api/repo/rehunt -----------------------------------------------------

/// Queuing a full re-hunt records the request on the repo, shows it in
/// the summary the Repos page reads, attributes it, and wakes the loop.
#[tokio::test]
async fn queuing_a_full_rehunt_marks_the_repo_and_wakes_the_scheduler() {
    let state = test_state().await;
    let wake = Arc::clone(&state.scheduler.wake);
    let before = hunter::util::now_ms();
    let (status, body) = post(&state, "/api/repo/rehunt", json!({ "id": 1 })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    let at = body["repo"]["full_hunt_requested_at"]
        .as_i64()
        .expect("the request time is on the returned row");
    assert!(at >= before, "{at} predates the request");
    tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
        .await
        .expect("queuing a re-hunt must wake the scheduler loop");

    let summary = get_json(&state, "/api/summary").await;
    let row = summary["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap();
    assert_eq!(row["full_hunt_requested_at"], json!(at));

    let events = get_json(&state, "/api/events").await;
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"] == "full re-hunt queued for demo"),
        "{events}"
    );
}

#[tokio::test]
async fn queuing_a_rehunt_needs_an_existing_repo() {
    let state = test_state().await;
    let (status, body) = post(&state, "/api/repo/rehunt", json!({ "id": "1" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, json!({ "error": "id must be an integer" }));
    let (status, body) = post(&state, "/api/repo/rehunt", json!({ "id": 999 })).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body, json!({ "error": "no repo 999" }));
}

/// Repo 1 with its `sync` and `hunt` held (no checkpoint) and its
/// `test_gap` failing once.
async fn held_repo_state() -> TestState {
    use hunter::domain::{HoldReason, RepoWorkKind};

    let state = test_state().await;
    let store = &state.store;
    for kind in [RepoWorkKind::Sync, RepoWorkKind::Hunt] {
        assert!(
            store
                .hold_repo_work(1, kind, HoldReason::Stuck, "git fetch origin failed", None)
                .await
                .unwrap()
        );
    }
    store
        .record_repo_failure(1, RepoWorkKind::TestGap, "runner error")
        .await
        .unwrap();

    state
}

/// A repo's held and failing work rides on the summary's `repos` and on
/// `/api/repos` (WORK-MODEL.md §17).
#[tokio::test]
async fn a_repos_holds_and_failing_work_are_shown() {
    let state = held_repo_state().await;
    let summary = get_json(&state, "/api/summary").await;
    let repo = summary["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap()
        .clone();
    let holds: Vec<(Value, Value)> = repo["holds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["kind"].clone(), h["actions"].clone()))
        .collect();
    assert_eq!(
        holds,
        [
            (json!("sync"), json!(["restart"])),
            (json!("hunt"), json!(["restart"])),
        ],
        "no checkpoint, so no continue: {repo}"
    );
    assert_eq!(
        repo["failing"],
        json!([{ "kind": "test_gap", "streak": 1, "last_failure": "runner error" }])
    );
    let repos = get_json(&state, "/api/repos").await;
    let alpha = repos
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == 1)
        .unwrap();
    assert_eq!(alpha["holds"], repo["holds"], "{repos}");
    assert_eq!(alpha["failing"], repo["failing"], "{repos}");
}

/// `POST /api/hold/release` with a `repo_id` releases a repo's hold with the
/// actions its kind allows, and refuses everything else.
#[tokio::test]
async fn a_repos_hold_is_released_through_the_hold_endpoint() {
    let state = held_repo_state().await;
    for (body, expected) in [
        (
            json!({ "repo_id": 1, "finding_id": 1, "kind": "sync", "action": "restart" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "repo_id": 1, "kind": "fix", "action": "restart" }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "repo_id": 999, "kind": "sync", "action": "restart" }),
            StatusCode::NOT_FOUND,
        ),
        (
            json!({ "repo_id": 1, "kind": "sync", "action": "continue" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "repo_id": 1, "kind": "hunt", "action": "continue" }),
            StatusCode::CONFLICT,
        ),
        (
            json!({ "repo_id": 1, "kind": "test_gap", "action": "restart" }),
            StatusCode::CONFLICT,
        ),
    ] {
        let (status, reply) = post(&state, "/api/hold/release", body.clone()).await;
        assert_eq!(status, expected, "{body}: {reply}");
    }

    let wake = Arc::clone(&state.scheduler.wake);
    let (status, reply) = post(
        &state,
        "/api/hold/release",
        json!({ "repo_id": 1, "kind": "sync", "action": "restart" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    assert_eq!(reply["repo"]["id"], 1);
    assert_eq!(
        reply["repo"]["holds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["kind"].clone())
            .collect::<Vec<_>>(),
        [json!("hunt")]
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), wake.notified())
            .await
            .is_ok(),
        "released work wakes the scheduler"
    );
    let (status, _) = post(
        &state,
        "/api/hold/release",
        json!({ "repo_id": 1, "kind": "sync", "action": "restart" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a released hold is gone");
}
