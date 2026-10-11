//! Axum router + handlers — the service's HTTP surface.
//! Parity contract: API-CONTRACT.md (JSON shapes in types.rs).
//!
//! Deliberate deviations from the Python implementation it replaces:
//! - Param validation failures return 400 {"error": ...} where Python
//!   leaks a 500 via its `ValueError` catch-all (the UI only reads the
//!   `error` key). EXCEPTION: /api/finding, where missing/non-numeric/
//!   unknown ids all return 404 {"error":"no such finding"} exactly,
//!   matching Python (contract §5).
//! - Static files are served by a hand-rolled fallback, NOT `ServeDir`:
//!   the Python server's extension allow-list, traversal guard, and
//!   404-JSON behavior (contract §2) don't match `ServeDir` semantics.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::auth::{self, CurrentUser};
use crate::config::{Config, HostAllowList};
use crate::domain::{BudgetOverride, FindingStatus, FindingType, ForgeName, Severity};
use crate::store::{FindingFilter, RepoUpdate, Store, StoreWriteError};
use crate::types::{
    ActivityStatus, BudgetState, Event, Finding, FindingDetail, FindingOut, JobListEntry, Repo,
    RepoBrief, Stats, Summary,
};

/// Handle to this process's scheduler loop.
#[derive(Clone)]
pub struct SchedulerHandle {
    /// Set by the loop for the duration of a cycle. Drives `cycle_running`,
    /// the "working" activity status, and the busy check on manual triggers.
    pub running: Arc<AtomicBool>,
    /// Prevents new scheduler cycles; an already-running worker is allowed
    /// to finish and remains visible as running.
    pub paused: Arc<AtomicBool>,
    /// Promotes every budget decision to the backend's prioritized path.
    /// Pacing limits are bypassed, but provider hard stops remain enforced.
    pub overdrive: Arc<AtomicBool>,
    /// Interrupts the loop's sleep so the next cycle starts immediately.
    /// A notify delivered mid-cycle is held as a permit, so a trigger is
    /// never lost -- it just lands on the following iteration.
    pub wake: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
    /// Budget/status brain. Summary reads `status_html()` and `decide()`;
    /// POST handlers never touch it directly.
    pub backend: Arc<dyn crate::backend::Backend>,
    /// The scheduler loop this server fronts.
    pub scheduler: SchedulerHandle,
    /// Held across "check the repo exists, then touch its notes file".
    /// `reap_repo` removes the clone, then the notes file at
    /// `Store::notes_path`, then drops the row; appending a note
    /// validates the row and then writes that same file. Interleaved,
    /// the append lands after the reaper has walked past the path and
    /// before the row is gone, leaving a notes file for a repo that no
    /// longer exists and nothing that will ever retry it.
    ///
    /// The earlier version of this comment described notes living at
    /// `repo-{id}/NOTES.md` inside the clone, where the same race also
    /// recreated a *directory* and `sync_repo` then refused to clone
    /// into it. Notes moved out of the clone; the race did not move
    /// with them, so this is still load-bearing.
    pub repo_notes: Arc<tokio::sync::Mutex<()>>,
}

/// Response shape for GET /api/repo/notes.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct RepoNotesResponse {
    pub notes: String,
}

/// Handler-level error. Body is always the `{"error": "<msg>"}` envelope
/// the UI expects on non-200s (contract §1, §13).
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    /// 409 — duplicate repo name (WRITES contract §7).
    #[error("{0}")]
    Conflict(String),
    /// 415 — POST Content-Type gate (WRITES contract §0.1).
    #[error("Content-Type must be application/json")]
    UnsupportedMediaType,
    /// 403 — a `Host` that is not a loopback name (WRITES contract §0.1).
    #[error("forbidden: non-local origin")]
    Forbidden,
    /// 401 — no live session, or a failed login.
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

fn error_body(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(msg) => error_body(StatusCode::BAD_REQUEST, &msg),
            Self::NotFound(msg) => error_body(StatusCode::NOT_FOUND, &msg),
            Self::Conflict(msg) => error_body(StatusCode::CONFLICT, &msg),
            Self::UnsupportedMediaType => error_body(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Content-Type must be application/json",
            ),
            Self::Forbidden => error_body(StatusCode::FORBIDDEN, "forbidden: non-local origin"),
            Self::Unauthorized(msg) => error_body(StatusCode::UNAUTHORIZED, msg),
            Self::Db(err) => {
                tracing::error!(error = %err, "database error");
                error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
            Self::Internal(err) => {
                tracing::error!(error = %err, "internal error");
                error_body(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
            }
        }
    }
}

/// Reject requests whose `Host` is not an accepted name: loopback, plus
/// whatever `serve.allowedHosts` adds ([`HostAllowList`]).
///
/// By default the listener binds 127.0.0.1, so this is not about
/// reachability from the network — it is about DNS rebinding. A page the operator
/// visits can point its own hostname at 127.0.0.1 and then talk to this
/// port as same-origin, which defeats CORS; the one thing that still
/// differs is the `Host` header, which carries the attacker's name.
///
/// So it applies to reads as well as writes. Findings carry file paths,
/// code excerpts and repo notes from private repositories, and leaking
/// those is the same class of loss as an unauthorised mutation.
///
/// Refused with 403 `forbidden: non-local origin`, the Python daemon's
/// status and message. An absent `Host` is allowed: only an HTTP/1.0 client
/// can omit it, and a rebinding attack runs in a browser, which always
/// sends one.
fn require_allowed_host(headers: &HeaderMap, allowed: &HostAllowList) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if host.is_empty() || allowed.allows(host_name(host)) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

/// The name part of a `Host` header: `name`, `name:port`, `[v6]` or
/// `[v6]:port` (RFC 7230 §5.4).
///
/// Not `split(':')`: that cuts an IPv6 literal at its first colon, so the
/// real loopback form `[::1]:8377` read as `[` and was refused, while a
/// bare `::1` -- and any `:anything` -- read as the empty string, which is
/// the absent-header case and was let through. Only a trailing all-digit
/// port is stripped, and only when what precedes it has no colon of its
/// own.
fn host_name(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').map_or(host, |(name, _)| name);
    }
    match host.rsplit_once(':') {
        Some((name, port))
            if !name.contains(':')
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            name
        }
        _ => host,
    }
}

/// [`require_allowed_host`] for every route, reads included.
async fn host_guard(
    State(state): State<AppState>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    match require_allowed_host(req.headers(), &state.config.allowed_hosts) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// Every `/api` route but `/api/login` needs a live session: the
/// request's [`CurrentUser`] goes into its extensions for the handler, and
/// without one the answer is 401 `login required`. Static files stay
/// public -- the UI bundle carries no data, and it has to load to show
/// the login form.
async fn require_session(
    State(state): State<AppState>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(token) = auth::session_token(req.headers()) else {
        return ApiError::Unauthorized(LOGIN_REQUIRED).into_response();
    };
    match state
        .store
        .session_user(&auth::token_hash(token), crate::util::now_ms())
        .await
    {
        Ok(Some(user)) => {
            req.extensions_mut().insert(user);
            next.run(req).await
        }
        Ok(None) => ApiError::Unauthorized(LOGIN_REQUIRED).into_response(),
        Err(e) => ApiError::Db(e).into_response(),
    }
}

const LOGIN_REQUIRED: &str = "login required";

/// Refused -> 400 with the store's exact message (WRITES contract §8:
/// "repo <id> has <n> finding(s) and <m> job(s) -- cannot delete without
/// losing history; pause it instead"); Db -> 500 envelope.
impl From<StoreWriteError> for ApiError {
    fn from(err: StoreWriteError) -> Self {
        match err {
            StoreWriteError::Refused(msg) => Self::BadRequest(msg),
            StoreWriteError::Db(err) => Self::Db(err),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/summary", get(summary))
        .route("/api/findings", get(findings))
        .route("/api/finding", get(finding_detail))
        .route("/api/jobs", get(jobs))
        .route("/api/repos", get(repos).post(add_repo))
        .route("/api/repo", post(update_repo))
        .route("/api/repo/notes", get(repo_notes).post(add_repo_note))
        .route("/api/repo/delete", post(delete_repo))
        .route("/api/repo/rehunt", post(request_rehunt))
        .route("/api/events", get(events))
        .route("/api/stats", get(stats))
        .route("/api/verdict", post(verdict))
        .route("/api/cycle", post(cycle))
        .route("/api/scheduler", post(set_scheduler_paused))
        .route("/api/overdrive", post(set_scheduler_overdrive))
        .route("/api/recheck", post(recheck))
        .route("/api/unqueue", post(unqueue))
        .route("/api/override", post(override_))
        .route("/api/me", get(me))
        .route("/api/logout", post(logout))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session,
        ));
    Router::new()
        .merge(api)
        .route("/api/login", post(login))
        .fallback(static_files)
        // Host check ahead of every handler and the static files, so a
        // rebound hostname cannot read findings either (contract §0.1).
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            host_guard,
        ))
        // Cache-Control: no-store on EVERY response, static files included
        // (contract §1) — `overriding` so nothing downstream can win.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .with_state(state)
}

/// Query param lookup with Python's `parse_qs` semantics: blank values are
/// dropped, so empty string == absent (the `parse_qs` call in `server.Handler.do_GET`).
fn param<'q>(params: &'q HashMap<String, String>, key: &str) -> Option<&'q str> {
    params
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

// -- /api/summary (contract §3) ---------------------------------------------

async fn summary(State(state): State<AppState>) -> Result<Json<Summary>, ApiError> {
    use crate::backend::Verdict;
    use crate::scheduler;
    use crate::types::NextCandidate;

    let store = &state.store;
    let counts = store.status_counts().await?;
    let type_counts = store.type_counts().await?;
    let repos: Vec<RepoBrief> = store
        .list_repos()
        .await?
        .iter()
        .map(RepoBrief::from)
        .collect();
    let last_cycle = store.last_cycle_event().await?;
    let current_job = store.current_job().await?;
    let scheduler_state = store.scheduler_state().await?;

    let backend_status_html = state.backend.status_html().await?;
    // True only while the loop is inside a cycle.
    let cycle_running = state.scheduler.running.load(Ordering::SeqCst);
    let scheduler_paused = state.scheduler.paused.load(Ordering::SeqCst);
    let scheduler_overdrive = state.scheduler.overdrive.load(Ordering::SeqCst);

    // "What's next" preview (`server.Handler._summary`): only when nothing is
    // running, from the SAME selection the scheduler itself makes, minus its
    // writes (`preview_next`), and the same decide.
    // preview_next errors are swallowed to None (the `try`/`except` around
    // `pick_next` in `server.Handler._summary`); anticipated_tokens/decide
    // errors propagate (500), as in Python where only pick_next sits inside
    // the try.
    let next_candidate: Option<NextCandidate> = if current_job.is_none() {
        match scheduler::preview_next(store, &state.config).await {
            Ok(Some(c)) => {
                let anticipated =
                    scheduler::candidate_reservation(store, &state.config, &c).await?;
                let outlook = state.backend.decide(anticipated).await?;
                let is_prioritized = scheduler_overdrive || c.budget_override().is_some();
                let verdict = if is_prioritized {
                    outlook.prioritized
                } else {
                    outlook.normal
                };
                let (budget_state, budget_reason, budget_retry_at) = match verdict {
                    Verdict::Denied { reason, retry_at } => (BudgetState::Denied, reason, retry_at),
                    Verdict::Granted { reason, .. } => (BudgetState::Allowed, reason, None),
                };
                Some(NextCandidate {
                    kind: c.job_kind(),
                    id: c.target_id(),
                    label: c.label().map(str::to_owned),
                    is_finding: c.job_kind().is_finding(),
                    is_prioritized,
                    budget_state,
                    budget_reason,
                    budget_retry_at,
                })
            }
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(error = %err, "pick_next failed; suppressing candidate preview");
                None
            }
        }
    } else {
        None
    };

    // Full priority chain per contract §3 / `server._activity_status`.
    let activity_status = if let Some(job) = &current_job {
        ActivityStatus::Running {
            job: Box::new(job.clone()),
        }
    } else if cycle_running {
        // Priority 2: a cycle is under way but has not created a job row yet.
        ActivityStatus::Working
    } else if let Some(s) = scheduler_state.as_ref().filter(|s| s.state == "error") {
        ActivityStatus::Error {
            detail: s.detail.clone(),
        }
    } else if let Some(s) = scheduler_state.as_ref().filter(|s| s.state == "offline") {
        // Ahead of the candidate preview: the budget may well allow it, and
        // "ready" would hide that the last cycle could not reach its hosts.
        ActivityStatus::Offline {
            detail: s.detail.clone(),
        }
    } else if let Some(c) = &next_candidate {
        if c.budget_state == BudgetState::Denied {
            ActivityStatus::Paused {
                candidate: c.clone(),
            }
        } else {
            ActivityStatus::Ready {
                candidate: c.clone(),
            }
        }
    } else if scheduler_state.is_some() {
        ActivityStatus::Idle
    } else {
        ActivityStatus::WarmingUp
    };

    Ok(Json(Summary {
        backend_status_html,
        counts,
        type_counts,
        repos,
        last_cycle,
        cycle_running,
        scheduler_paused,
        scheduler_overdrive,
        current_job,
        next_candidate,
        scheduler_state,
        activity_status,
    }))
}

// -- /api/findings (contract §4) --------------------------------------------

async fn findings(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<Vec<FindingOut>>, ApiError> {
    let store = &state.store;
    // Default "1"; any other present, non-empty value selects legacy mode:
    // no `type` filter, no `category` key (the UI never sends this param).
    let unified = param(&params, "unified").unwrap_or("1") == "1";

    // Python built `status = ?` from the raw value (`Store.list_findings`), so
    // an unrecognised non-empty filter matched no rows. Parsing it into an
    // Option and dropping the failure would turn it into *no predicate* —
    // a typo would silently return the whole corpus instead of nothing.
    // The typed filter cannot express "impossible", so answer directly.
    let status_param = param(&params, "status");
    let status = status_param.and_then(|s| {
        FindingStatus::ALL
            .iter()
            .find(|fs| fs.as_str() == s)
            .copied()
    });
    if status_param.is_some() && status.is_none() {
        return Ok(Json(Vec::new()));
    }

    let mut filter = FindingFilter {
        status,
        ..FindingFilter::default()
    };
    if let Some(repo_key) = param(&params, "repo") {
        let repo = if repo_key.bytes().all(|b| b.is_ascii_digit()) {
            match repo_key.parse::<i64>() {
                Ok(id) => store.get_repo_by_id(id).await?,
                // All digits but past i64::MAX: no such repo id.
                Err(_) => None,
            }
        } else {
            store.get_repo_by_name(repo_key).await?
        };
        let repo = repo.ok_or_else(|| ApiError::BadRequest(format!("unknown repo {repo_key}")))?;
        filter.repo_id = Some(repo.id);
    }
    if let Some(sev) = param(&params, "severity") {
        let sev = Severity::parse(sev)
            .ok_or_else(|| ApiError::BadRequest(format!("invalid severity {sev}")))?;
        filter.min_severity_rank = Some(sev.rank());
    }
    if unified {
        let kind_param = param(&params, "type");
        filter.kind = kind_param.and_then(|s| s.parse::<FindingType>().ok());
        if kind_param.is_some() && filter.kind.is_none() {
            return Ok(Json(Vec::new()));
        }
    }

    let rows = store.list_findings(&filter).await?;
    let ids: Vec<i64> = rows.iter().map(|f| f.id).collect();
    let mut timelines = store.events_by_finding(&ids).await?;
    let attention = store.pr_attention().await?;
    let mut blockers = store.held_fix_blockers().await?;
    let mut follow_up_of: HashMap<i64, i64> = HashMap::new();
    let mut follow_ups: HashMap<i64, Vec<i64>> = HashMap::new();
    for (source, follow_up) in store.follow_up_pairs(&ids).await? {
        follow_up_of.insert(follow_up, source);
        follow_ups.entry(source).or_default().push(follow_up);
    }

    let out = rows
        .into_iter()
        .map(|finding| {
            // `category` only in unified mode; absent for unknown types.
            let category = if unified { finding.category() } else { None };
            let timeline = timelines.remove(&finding.id).unwrap_or_default();
            // Key present ONLY for pr_open rows that HAVE a pr_state row;
            // map hit with a NULL column -> Some(None) -> serialized null.
            let needs_attention = if finding.status == FindingStatus::PrOpen {
                attention.get(&finding.id).cloned()
            } else {
                None
            };
            FindingOut {
                blocker: blockers.remove(&finding.id),
                follow_up_of: follow_up_of.get(&finding.id).copied(),
                follow_ups: follow_ups.remove(&finding.id).unwrap_or_default(),
                finding,
                category,
                timeline,
                needs_attention,
            }
        })
        .collect();
    Ok(Json(out))
}

// -- /api/finding (contract §5) ----------------------------------------------

async fn finding_detail(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<FindingDetail>, ApiError> {
    let store = &state.store;
    // Missing, non-numeric, and unknown ids are indistinguishable to the
    // client: 404 {"error":"no such finding"} for all three (contract §5).
    let no_such = || ApiError::NotFound("no such finding".to_owned());
    let fid: i64 = param(&params, "id")
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|v| v.parse().ok())
        .ok_or_else(no_such)?;
    if !store.finding_exists(fid).await? {
        return Err(no_such());
    }
    Ok(Json(FindingDetail {
        jobs: store.jobs_by_finding(fid).await?,
        pr_state: store.get_pr_state(fid).await?,
    }))
}

// -- /api/jobs, /api/repos, /api/events, /api/stats (contract §§6,7,9,10) ----

async fn jobs(State(state): State<AppState>) -> Result<Json<Vec<JobListEntry>>, ApiError> {
    Ok(Json(state.store.list_jobs(50).await?))
}

async fn repos(State(state): State<AppState>) -> Result<Json<Vec<Repo>>, ApiError> {
    Ok(Json(state.store.list_repos().await?))
}

async fn events(State(state): State<AppState>) -> Result<Json<Vec<Event>>, ApiError> {
    Ok(Json(state.store.recent_events(100).await?))
}

// -- /api/login, /api/logout, /api/me ------------------------------------------

/// Response for a login and for `/api/me`: who the session belongs to.
#[derive(Debug, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct SessionUser {
    pub username: String,
}

const BAD_LOGIN: &str = "invalid username or password";

/// POST /api/login `{"username", "password"}`: on success a session
/// cookie and the username; otherwise 401 with one message for an unknown
/// name, a disabled account and a wrong password alike.
async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ApiError::BadRequest(format!("{key} must be a string")))
    };
    let username = field("username")?;
    let password = field("password")?;
    let candidate = state.store.login_candidate(&username).await?;
    // An unknown name is checked against a decoy hash, so it takes as long
    // as a wrong password and timing does not reveal which names exist.
    let hash = candidate
        .as_ref()
        .map_or_else(|| auth::DECOY_HASH.to_owned(), |(_, h)| h.clone());
    let matches = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(|e| anyhow::anyhow!("password check: {e}"))?;
    let Some((user, _)) = candidate.filter(|_| matches) else {
        tracing::info!("failed login for {username:?}");
        return Err(ApiError::Unauthorized(BAD_LOGIN));
    };
    let token = auth::new_session_token()?;
    state
        .store
        .create_session(
            &user,
            &auth::token_hash(&token),
            crate::util::now_ms() + auth::SESSION_TTL_MS,
        )
        .await?;
    tracing::info!("{} logged in", user.username);
    Ok((
        [(header::SET_COOKIE, auth::session_cookie(&token)?)],
        Json(SessionUser {
            username: user.username,
        }),
    )
        .into_response())
}

/// POST /api/logout: end this browser's session and drop its cookie.
async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    if let Some(token) = auth::session_token(&headers) {
        state.store.delete_session(&auth::token_hash(token)).await?;
    }
    Ok((
        [(header::SET_COOKIE, auth::cleared_session_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response())
}

/// GET /api/me: the account this session belongs to.
async fn me(Extension(user): Extension<CurrentUser>) -> Json<SessionUser> {
    Json(SessionUser {
        username: user.username,
    })
}

async fn stats(State(state): State<AppState>) -> Result<Json<Stats>, ApiError> {
    Ok(Json(Stats {
        totals: state.store.stats_totals().await?,
        by_kind: state.store.stats_by_kind().await?,
        by_finding: state.store.stats_by_finding().await?,
    }))
}

// -- /api/repo/notes (contract §8) -------------------------------------------

async fn repo_notes(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<RepoNotesResponse>, ApiError> {
    let raw = param(&params, "id")
        .filter(|v| v.bytes().all(|b| b.is_ascii_digit()))
        .ok_or_else(|| ApiError::BadRequest("id query param must be an integer".to_owned()))?;
    let repo = match raw.parse::<i64>() {
        Ok(id) => state.store.get_repo_by_id(id).await?,
        // All digits but past i64::MAX: behaves like any nonexistent id.
        Err(_) => None,
    };
    let repo = repo.ok_or_else(|| ApiError::NotFound(format!("no repo {raw}")))?;
    let notes = Store::repo_notes(&state.config.work_root, repo.id);
    Ok(Json(RepoNotesResponse { notes }))
}

// -- static files + 404 fallback (contract §2) --------------------------------

async fn static_files(State(state): State<AppState>, uri: Uri) -> Response {
    let path = uri.path();
    if path.starts_with("/api/") {
        return error_body(StatusCode::NOT_FOUND, "not found");
    }
    if path == "/" || path == "/index.html" {
        return match tokio::fs::read(state.config.ui_dir.join("index.html")).await {
            Ok(bytes) => {
                ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response()
            }
            Err(_) => error_body(StatusCode::NOT_FOUND, "ui/index.html missing"),
        };
    }
    // Python: suffix = path.rsplit(".", 1)[-1] when the path contains a
    // dot; only these three extensions are ever served (contract §2).
    let content_type = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("map") => "application/json",
        _ => return error_body(StatusCode::NOT_FOUND, "not found"),
    };
    let asset = state.config.ui_dir.join(path.trim_start_matches('/'));
    // Traversal guard: resolve symlinks and require the target to stay
    // strictly under ui_dir (Python: `UI_DIR in asset.resolve().parents`).
    // Canonicalize failure == missing file == escape attempt == 404 JSON.
    let (Ok(ui_dir), Ok(resolved)) = (
        tokio::fs::canonicalize(&state.config.ui_dir).await,
        tokio::fs::canonicalize(&asset).await,
    ) else {
        return error_body(StatusCode::NOT_FOUND, "not found");
    };
    if !resolved.starts_with(&ui_dir) || resolved == ui_dir {
        return error_body(StatusCode::NOT_FOUND, "not found");
    }
    match tokio::fs::read(&resolved).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type)], bytes).into_response(),
        // Directory or race-deleted file: same 404 as Python's is_file gate.
        Err(_) => error_body(StatusCode::NOT_FOUND, "not found"),
    }
}

// ============================================================================
// POST endpoints (API-CONTRACT-WRITES.md). Shared plumbing first (§0), then
// one handler per contract section. Error strings are byte-copied from the
// contract; deliberate deviations are commented ACCEPTED DEVIATION inline.
// ============================================================================

/// Content-Type gate (contract §0.1): prefix match on "application/json"
/// (so "; charset=utf-8" passes), applied to every POST — including
/// /api/cycle, which never reads a body. Missing header == "".
fn post_gate(headers: &HeaderMap) -> Result<(), ApiError> {
    let ctype = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if ctype.starts_with("application/json") {
        Ok(())
    } else {
        Err(ApiError::UnsupportedMediaType)
    }
}

/// Body parse (contract §0.2). "empty body" is byte-exact Python; malformed
/// JSON gets a stable message instead of Python's json.JSONDecodeError text
/// (serde messages differ anyway); valid-JSON-non-object returns a 400
/// envelope where Python leaks a raw `TypeError` as `500 internal error`
/// (ACCEPTED DEVIATION — keep OUR error envelope, the UI only displays it).
fn parse_object(body: &[u8]) -> Result<Map<String, Value>, ApiError> {
    if body.is_empty() {
        return Err(ApiError::BadRequest("empty body".to_owned()));
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|_| ApiError::BadRequest("invalid JSON body".to_owned()))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(ApiError::BadRequest(
            "body must be a JSON object".to_owned(),
        )),
    }
}

/// `isinstance(x, int)` with the boolean quirk closed: JSON true/false are
/// NOT integers here (contract §0.2 decision — Python treats them as 1/0),
/// and floats are rejected exactly like Python.
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

fn require_int(body: &Map<String, Value>, key: &str, msg: &str) -> Result<i64, ApiError> {
    body.get(key)
        .and_then(as_int)
        .ok_or_else(|| ApiError::BadRequest(msg.to_owned()))
}

/// Python truthiness over a JSON value (for /api/repo's `enabled` coercion
/// and the falsy-forge/-reason fallthroughs).
fn python_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

async fn fetch_finding(store: &Store, fid: i64) -> Result<Finding, ApiError> {
    store
        .get_finding(fid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no finding {fid}")))
}

async fn fetch_repo(store: &Store, rid: i64) -> Result<Repo, ApiError> {
    store
        .get_repo_by_id(rid)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("no repo {rid}")))
}

// -- POST /api/verdict (contract §1) ------------------------------------------

async fn verdict(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    // Non-string status is simply "not a verdict status", like Python.
    let status_str = body.get("status").and_then(Value::as_str).unwrap_or("");
    let status = FindingStatus::ALL
        .iter()
        .find(|fs| fs.as_str() == status_str)
        .copied()
        .filter(|fs| fs.is_verdict());
    let Some(status) = status else {
        // ACCEPTED DEVIATION: Python's f-string renders StrEnum reprs here
        // (`<Status.QUEUED: 'queued'>`, ...). We emit the plain value list —
        // the UI only ever displays this string.
        return Err(ApiError::BadRequest(
            "status must be one of ['queued', 'rejected', 'wontfix', 'note']".to_owned(),
        ));
    };
    // (body.get("reason") or "").strip() or None — absent/null/empty/
    // whitespace (and Python-falsy non-strings) all become None
    // (`server.Handler._verdict`). ACCEPTED DEVIATION: a truthy non-string reason 400s
    // here where Python leaks a 500 AttributeError.
    let reason = match body.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        }
        Some(other) if !python_truthy(other) => None,
        Some(_) => {
            return Err(ApiError::BadRequest("reason must be a string".to_owned()));
        }
    };
    if status.reason_required() && reason.is_none() {
        return Err(ApiError::BadRequest(format!(
            "reason required for status '{status}'"
        )));
    }
    let finding = fetch_finding(&state.store, fid).await?;
    // The operator gives a verdict only where the finding asks for one;
    // anywhere else a job or the forge owns it. Same precondition shape as
    // /api/recheck and /api/unqueue.
    if !finding.status.awaits_verdict() || finding.status == status {
        return Err(ApiError::BadRequest(format!(
            "finding #{fid} is '{}'; a verdict applies only to new, blocked, note or closed findings, \
             and must change its status",
            finding.status
        )));
    }
    // Conditional on the status just read, so a job that claimed the
    // finding in between keeps it.
    let applied = match reason.as_deref() {
        Some(r) => {
            state
                .store
                .set_verdict_if(fid, finding.status, status, r)
                .await?
        }
        None => {
            state
                .store
                .claim_in_progress(fid, finding.status, status)
                .await?
        }
    };
    if !applied {
        return Err(ApiError::Conflict(format!(
            "finding #{fid} changed while the verdict was being applied; reload and retry"
        )));
    }
    // fingerprint from the row read BEFORE the update (`server.Handler._verdict`).
    let mut msg = format!("finding {fid} [{}] -> {status}", finding.fingerprint);
    if let Some(reason) = &reason {
        msg.push_str(": ");
        msg.push_str(reason);
    }
    state
        .store
        .log_user_event(&user, "verdict", &msg, Some(fid))
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // A verdict changes the queue (a queued or resumed fix, a finding taken
    // out of it): act on it now, not at the loop's next natural wake.
    state.scheduler.wake.notify_one();
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/cycle (contract §2) --------------------------------------------

/// Trigger a scheduler cycle now (contract §2): 409 if one is already
/// running, otherwise 202 and the loop starts immediately.
///
/// The busy check is advisory, as Python's non-blocking lock acquire was:
/// a trigger that races the loop into its next cycle is held as a notify
/// permit and runs on the following iteration rather than being dropped.
async fn cycle(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    if state.scheduler.paused.load(Ordering::SeqCst) {
        return Ok(error_body(StatusCode::CONFLICT, "paused"));
    }
    if state.scheduler.running.load(Ordering::SeqCst) {
        return Ok(error_body(StatusCode::CONFLICT, "busy"));
    }
    state.scheduler.wake.notify_one();
    Ok((StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response())
}

async fn set_scheduler_paused(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let paused = body
        .get("paused")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::BadRequest("paused must be a boolean".to_owned()))?;
    state.scheduler.paused.store(paused, Ordering::SeqCst);
    state.scheduler.wake.notify_one();
    Ok(Json(json!({ "paused": paused })).into_response())
}

async fn set_scheduler_overdrive(
    State(state): State<AppState>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::BadRequest("enabled must be a boolean".to_owned()))?;
    state.scheduler.overdrive.store(enabled, Ordering::SeqCst);
    state.scheduler.wake.notify_one();
    Ok(Json(json!({ "overdrive": enabled })).into_response())
}

// -- POST /api/recheck, /api/unqueue (contract §§3,4) --------------------------

async fn recheck(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    let finding = fetch_finding(&state.store, fid).await?;
    if finding.status != FindingStatus::New {
        return Err(ApiError::BadRequest(format!(
            "finding #{fid} is '{}', not 'new'",
            finding.status
        )));
    }
    // Conditional on `new`, like every operator write: whatever moved the
    // finding since the read above wins.
    if !state
        .store
        .claim_in_progress(fid, FindingStatus::New, FindingStatus::Rechecking)
        .await?
    {
        return Err(ApiError::Conflict(format!(
            "finding #{fid} changed while it was being queued for recheck; reload and retry"
        )));
    }
    state
        .store
        .log_user_event(
            &user,
            "recheck",
            &format!("#{fid} queued for recheck"),
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // The recheck tier has work now; start it without waiting for the
    // loop's next natural wake.
    state.scheduler.wake.notify_one();
    // NB: success key is `queued`, not `ok` (contract §3).
    Ok(Json(json!({ "queued": true, "finding": refreshed })).into_response())
}

async fn unqueue(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let fid = require_int(&body, "id", "id must be an integer")?;
    let finding = fetch_finding(&state.store, fid).await?;
    if finding.status != FindingStatus::Queued {
        return Err(ApiError::BadRequest(format!(
            "finding #{fid} is '{}', not 'queued'",
            finding.status
        )));
    }
    // Conditional on `queued`: a fix that claimed the finding since the
    // read above owns it now.
    if !state
        .store
        .claim_in_progress(fid, FindingStatus::Queued, FindingStatus::New)
        .await?
    {
        return Err(ApiError::Conflict(format!(
            "finding #{fid} changed while it was being unqueued; reload and retry"
        )));
    }
    state
        .store
        .log_user_event(
            &user,
            "unqueue",
            &format!("#{fid} removed from fix queue"),
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // The queue changed; the next pick should see it now, not at the
    // loop's next natural wake.
    state.scheduler.wake.notify_one();
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/override (contract §5) ------------------------------------------

async fn override_(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let mode_val = body.get("mode").cloned().unwrap_or(Value::Null);
    // Special case FIRST (`server.Handler._override`): {"id": "all", "mode": null}
    // clears every override; "all" with a non-null mode falls through to
    // the int check below.
    if body.get("id").and_then(Value::as_str) == Some("all") && mode_val.is_null() {
        let n = state.store.clear_all_overrides().await?;
        state
            .store
            .log_user_event(
                &user,
                "override",
                &format!("cleared all budget overrides ({n} findings)"),
                None,
            )
            .await?;
        return Ok(Json(json!({ "ok": true, "cleared": n })).into_response());
    }
    let fid = require_int(
        &body,
        "id",
        "id must be an integer (or 'all' with mode=null)",
    )?;
    let mode = match &mode_val {
        Value::Null => None,
        v => Some(
            v.as_str()
                .and_then(|s| s.parse::<BudgetOverride>().ok())
                .ok_or_else(|| {
                    ApiError::BadRequest("mode must be 'once', 'exempt', or null".to_owned())
                })?,
        ),
    };
    fetch_finding(&state.store, fid).await?;
    state.store.set_budget_override(fid, mode).await?;
    let label = mode.map_or("cleared", BudgetOverride::as_str);
    state
        .store
        .log_user_event(
            &user,
            "override",
            &format!("#{fid} budget override: {label}"),
            Some(fid),
        )
        .await?;
    let refreshed = fetch_finding(&state.store, fid).await?;
    // Setting an override should take effect now, not at the loop's next
    // natural wake (contract §5 -- otherwise up to ~60 min later).
    if mode.is_some() {
        state.scheduler.wake.notify_one();
    }
    Ok(Json(json!({ "ok": true, "finding": refreshed })).into_response())
}

// -- POST /api/repo (contract §6) ----------------------------------------------

async fn update_repo(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let repo = fetch_repo(&state.store, rid).await?;
    // Silent-skip field coercion (`server.Handler._update_repo`): invalid
    // values are dropped, never rejected — except a non-empty `url` failing
    // `valid_repo_url`, which 400s. `action` mirrors Python's dict insertion
    // order (enabled, url, default_branch, forge).
    let mut fields = RepoUpdate::default();
    let mut action: Vec<String> = Vec::new();
    if let Some(v) = body.get("enabled") {
        let enabled = i64::from(python_truthy(v));
        action.push(format!("enabled={enabled}"));
        fields.enabled = Some(enabled);
    }
    if let Some(Value::String(s)) = body.get("url") {
        let s = s.trim();
        if !s.is_empty() {
            if !valid_repo_url(s) {
                return Err(ApiError::BadRequest(
                    "url must be http(s) or an ssh clone URL".to_owned(),
                ));
            }
            action.push(format!("url={s}"));
            fields.url = Some(s.to_owned());
        }
    }
    if let Some(Value::String(s)) = body.get("default_branch") {
        let s = s.trim();
        if !s.is_empty() {
            action.push(format!("default_branch={s}"));
            fields.default_branch = Some(s.to_owned());
        }
    }
    if let Some(Value::String(s)) = body.get("forge")
        && let Ok(f) = s.trim().parse::<ForgeName>()
    {
        action.push(format!("forge={f}"));
        fields.forge = Some(f);
    }
    if action.is_empty() {
        // Exact message from `server.Handler._update_repo` (so {"id":1,"forge":
        // "bitbucket"} yields this, not a forge-specific error).
        return Err(ApiError::BadRequest("no valid fields to update".to_owned()));
    }
    state.store.update_repo(rid, &fields).await?;
    if let Some(new_url) = fields.url.as_deref()
        && new_url != repo.url
    {
        repoint_clone_origin(rid, Path::new(&repo.path), &repo.url, new_url).await;
    }
    state
        .store
        .log_user_event(
            &user,
            "repo",
            &format!("updated {}: {}", repo.name, action.join(", ")),
            None,
        )
        .await?;
    let refreshed = fetch_repo(&state.store, rid).await?;
    Ok(Json(json!({ "ok": true, "repo": refreshed })).into_response())
}

/// Point an existing clone at the repo's new URL.
///
/// `sync_repo` refuses to work in a directory whose `origin` is not the
/// repo's URL — correctly, since that is how it recognises somebody
/// else's checkout sitting at the path. But changing the URL through
/// this endpoint used to leave the clone on the old origin, which turned
/// that safety check into a permanent block: every hunt, fix and recheck
/// for the repo failed until an operator deleted the directory by hand.
///
/// Only when the clone still carries the OLD url. An origin that is
/// neither the old nor the new URL is exactly the case the refusal
/// exists for, so it is left alone to trip it.
///
/// Best-effort: the update itself has already been committed, and a
/// missing or unreadable clone is not an error — the next cycle clones
/// it fresh. Failure is logged for the operator rather than surfaced to
/// the caller, whose write did succeed.
async fn repoint_clone_origin(rid: i64, dir: &Path, old_url: &str, new_url: &str) {
    // `repo.path`, not a path derived from the id. They agree for every
    // row migration 007 rewrote, but 007 deliberately left alone any
    // path that did not end in the repo's name — an operator-edited one
    // — and the scheduler syncs whatever `repo.path` says. Deriving the
    // directory here would repoint nothing for those rows and leave the
    // old origin exactly where `sync_repo` looks, which is the permanent
    // refusal this function exists to prevent.
    if !dir.join(".git").exists() {
        return;
    }
    let dir_s = dir.to_string_lossy().into_owned();
    // The configured URL, as `sync_repo` reads it: `remote get-url` is
    // rewritten through `url.<base>.insteadOf` and would never match the
    // raw `old_url` under such a rule.
    let (rc, out) = {
        let d = dir_s.clone();
        tokio::task::spawn_blocking(move || {
            crate::util::run_cmd(
                &["git", "-C", &d, "config", "--get", "remote.origin.url"],
                30,
            )
        })
        .await
        .unwrap_or((127, "spawn error".to_owned()))
    };
    if rc != 0 {
        return;
    }
    if out.trim() != old_url {
        tracing::warn!(
            "repo {rid} url changed but {dir_s} has origin {} -- left as is; \
             remove the directory to re-clone",
            out.trim()
        );
        return;
    }
    let new = new_url.to_owned();
    let (rc, out) = tokio::task::spawn_blocking(move || {
        crate::util::run_cmd(
            &["git", "-C", &dir_s, "remote", "set-url", "origin", &new],
            30,
        )
    })
    .await
    .unwrap_or((127, "spawn error".to_owned()));
    if rc != 0 {
        tracing::warn!("repo {rid} url changed but its clone origin could not be updated: {out}");
    }
}

// -- POST /api/repos (contract §7) -----------------------------------------------

/// Reject a repo URL whose scheme could execute when rendered as a link.
///
/// `Repo.url` is displayed as an `<a href>` in the UI, so a stored
/// `javascript:` (or `data:`/`vbscript:`) URL runs on click. Only the
/// operator can add repos, so this is a stored-XSS-against-yourself at
/// worst — but the store is also what a future multi-user mode would
/// serve, and validating a write is cheaper than remembering to escape
/// every read.
///
/// Allow-listing `http`/`https` alone would break SSH clone URLs, which
/// the forge layer accepts (`git@host:owner/repo.git`). Those carry no
/// scheme at all — the colon belongs to the scp-like syntax — so the
/// rule is: if there is a URL scheme, it must be http, https or ssh.
fn valid_repo_url(url: &str) -> bool {
    // A leading `-` makes git read the value as an option rather than a
    // URL: `git clone --upload-pack=<cmd> <dir>` runs <cmd>. The clone
    // is built from the stored URL, so accepting one here is what turns
    // a repo row into an argument. Belt and braces -- the clone also
    // passes `--` now -- because this is the field an operator pastes
    // and every other consumer of it would need the same guard.
    if url.starts_with('-') {
        return false;
    }
    let Some(colon) = url.find(':') else {
        return true; // no scheme at all (scp-like or a bare path)
    };
    let scheme = &url[..colon];
    // A scheme is ALPHA *( ALPHA / DIGIT / "+" / "-" / "." ) per RFC 3986;
    // anything else means the colon is part of a path, not a scheme.
    let looks_like_scheme = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !looks_like_scheme {
        return true;
    }
    // ssh is here because the rejection message promises it: the scp-like
    // `git@host:path` form has no scheme and is accepted above, but the
    // explicit `ssh://git@host/path` spelling is equally ordinary and was
    // being refused by a message that claimed to allow it. What stays out
    // is anything a browser would execute from an href -- javascript:,
    // data:, vbscript: -- which is what this guard exists for.
    ["http", "https", "ssh"]
        .iter()
        .any(|s| scheme.eq_ignore_ascii_case(s))
}

/// Longest accepted repo name, in characters. GitHub's own limit.
const MAX_REPO_NAME_CHARS: usize = 100;

/// A repo name is a display label only: the clone and notes paths are keyed
/// by id (`Store::repo_dir`, `Store::notes_path`), so no character can
/// reach the filesystem. What stays out is what would corrupt the places the
/// name is printed -- event and log lines, the UI, worker prompts -- i.e.
/// control characters such as newlines, the Unicode line and paragraph
/// separators U+2028/U+2029 (category Zl/Zp, which `char::is_control`
/// misses), the Unicode `Bidi_Control` characters, which reorder the text
/// around them on screen (U+202E can make a name read as something else),
/// plus anything absurdly long.
fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_REPO_NAME_CHARS
        && !name.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    '\u{2028}' | '\u{2029}' // line/paragraph separator
                        | '\u{061C}' | '\u{200E}' | '\u{200F}' // ALM, LRM, RLM
                        | '\u{202A}'..='\u{202E}' // LRE, RLE, PDF, LRO, RLO
                        | '\u{2066}'..='\u{2069}' // LRI, RLI, FSI, PDI
                )
        })
}

async fn add_repo(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    // A non-string name is just an unusable name: it falls into "invalid
    // repo name" like any other. This was once a documented deviation,
    // because Python raised AttributeError off `.strip()` and answered
    // 500; it now guards with isinstance and answers 400 as well.
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if !valid_repo_name(&name) {
        return Err(ApiError::BadRequest("invalid repo name".to_owned()));
    }
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err(ApiError::BadRequest("name and url are required".to_owned()));
    }
    if !valid_repo_url(&url) {
        return Err(ApiError::BadRequest(
            "url must be http(s) or an ssh clone URL".to_owned(),
        ));
    }
    let branch = match body.get("branch").and_then(Value::as_str).map(str::trim) {
        Some(b) if !b.is_empty() => b.to_owned(),
        _ => "main".to_owned(),
    };
    let forge: ForgeName = match body.get("forge") {
        // Python-falsy forge (`body.get("forge") or None`) -> auto-detect.
        Some(v) if python_truthy(v) => match v.as_str() {
            Some(f) => match f.parse::<ForgeName>() {
                Ok(fg) => fg,
                Err(_) => {
                    return Err(ApiError::BadRequest(format!(
                        "unknown forge '{f}' (choose from github, gitlab)"
                    )));
                }
            },
            // Truthy non-string: JSON-render it in the message (Python
            // repr-renders; no client sends this shape).
            None => {
                return Err(ApiError::BadRequest(format!(
                    "unknown forge '{v}' (choose from github, gitlab)"
                )));
            }
        },
        _ => crate::forge::detect_forge(&url),
    };
    // Dupe check ports Python's get_repo(name) key routing: an all-digit
    // name looks up BY ID (`Store.get_repo`), so a repo named "123" dupe-checks
    // against repo id 123 — preexisting oddity, ported as-is (contract §7).
    let existing = if name.bytes().all(|b| b.is_ascii_digit()) {
        match name.parse::<i64>() {
            Ok(id) => state.store.get_repo_by_id(id).await?,
            Err(_) => None,
        }
    } else {
        state.store.get_repo_by_name(&name).await?
    };
    if existing.is_some() {
        return Err(ApiError::Conflict(format!("repo '{name}' already exists")));
    }
    // No containment check on the name any more: the clone directory is
    // `repos/repo-<id>`, so the name never reaches a path and cannot escape
    // one. See Store::repo_dir for why deriving it from the name was the
    // wrong shape rather than merely under-validated.
    let repos_dir = state.config.work_root.join("repos");
    // NO git clone here — deferred to the scheduler: pick_next selects a
    // hunt for a repo whose path doesn't exist, and run_hunt does the clone
    // (contract §7, `scheduler.run_hunt`).
    let rid = state
        .store
        .add_repo(&name, &url, &repos_dir, &branch, forge)
        .await?;
    let path_str = Store::repo_dir(&repos_dir, rid)
        .to_string_lossy()
        .into_owned();
    state
        .store
        .log_user_event(
            &user,
            "repo",
            &format!("added {name} ({forge}) -> {path_str}"),
            None,
        )
        .await?;
    let repo = fetch_repo(&state.store, rid).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "repo": repo })),
    )
        .into_response())
}

// -- POST /api/repo/delete (contract §8) ----------------------------------------

/// Remove one flagged repo's directory, then drop its row.
///
/// Ordering is the whole point: the row is what marks `repos/repo-<id>`
/// as still owned, so it may only be dropped once nothing of the repo is
/// left on disk. Drop it first and the directory becomes an orphan no
/// row accounts for and no pass will ever revisit. Leaving it flagged is
/// always safe — the repo is already invisible, and the next pass tries
/// again.
pub async fn reap_repo(
    store: &Store,
    repos_dir: &Path,
    rid: i64,
    notes_lock: &tokio::sync::Mutex<()>,
) -> std::io::Result<()> {
    // Taken here rather than by the callers, because there are two of
    // them and only one used to hold it: the delete handler did, but the
    // reaper pass that runs at startup and before every cycle did not.
    // An append interleaving with that pass recreates the notes file
    // after the reaper has walked past it, and the row is dropped a
    // moment later — leaving a file for a repo that no longer exists and
    // nothing that will ever retry it. Python takes the lock inside its
    // reaper for the same reason.
    let _guard = notes_lock.lock().await;
    let dir = Store::repo_dir(repos_dir, rid);
    // `tokio::fs`, not `std::fs`: removing a clone is explicitly not
    // instant — that slowness is the whole reason deletion is two-phase —
    // and the synchronous call blocks a tokio worker for its duration,
    // here while holding the notes mutex, so every append would queue
    // behind a multi-gigabyte rmtree.
    match tokio::fs::remove_dir_all(&dir).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    // Notes live outside the clone, so removing the clone does not take
    // them with it. Left behind they would be read by whatever repo next
    // holds this id -- which AUTOINCREMENT makes unlikely, but the file
    // is the operator's private context either way and deletion is
    // supposed to mean deletion.
    if let Some(work_root) = repos_dir.parent() {
        match tokio::fs::remove_file(Store::notes_path(work_root, rid)).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    store
        .forget_deleted_repo(rid)
        .await
        .map_err(std::io::Error::other)?;
    Ok(())
}

/// Move notes written under the old layout out of the clone directories.
///
/// Notes used to live at `repos/repo-<id>/NOTES.md`, inside the working
/// tree of a real checkout. Any installation that wrote a note before
/// this changed still has one there, where it is both committable by a
/// worker and invisible to the reader, which now looks beside the
/// clones. One pass at startup, and a no-op from then on.
///
/// Never overwrites: a note already at the new path is the current one.
///
/// Never touches a `NOTES.md` the project itself tracks. Hunter's name for
/// the file is a common one, and moving a tracked file out would leave the
/// clone dirty with a deletion a worker can commit, and serve the project's
/// own document as the operator's notes. Only a file git positively reports
/// as untracked is moved; if git cannot say, the file stays where it is,
/// because leaving it is recoverable and a wrong move is not.
pub fn migrate_repo_notes(work_root: &Path) -> usize {
    let repos_dir = work_root.join("repos");
    let Ok(entries) = std::fs::read_dir(&repos_dir) else {
        return 0;
    };
    let mut moved = 0;
    for entry in entries.flatten() {
        let Some(rid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_prefix("repo-"))
            .and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        let old = entry.path().join("NOTES.md");
        if !old.is_file() {
            continue;
        }
        // `--error-unmatch` exits 0 when tracked, 1 when not; anything
        // else (128: not a repository) means git could not tell.
        let (rc, _) = crate::util::run_cmd_in(
            &["git", "ls-files", "--error-unmatch", "--", "NOTES.md"],
            Some(&entry.path()),
            30,
        );
        if rc != 1 {
            if rc == 0 {
                tracing::info!(
                    "repo {rid}: NOTES.md in the clone is tracked by the project; \
                     leaving it, it is not hunter's"
                );
            } else {
                tracing::warn!(
                    "repo {rid}: could not ask git whether {} is tracked (exit {rc}); \
                     leaving it in place",
                    old.display()
                );
            }
            continue;
        }
        let new = Store::notes_path(work_root, rid);
        if let Some(parent) = new.parent()
            && std::fs::create_dir_all(parent).is_err()
        {
            continue;
        }
        // Already migrated: the live notes are the ones at the new path,
        // so the old file is not content to keep -- but it must still
        // leave the checkout, because that is the whole point. Parked
        // beside the current notes rather than deleted, since it is the
        // operator's writing and this pass is not the place to decide it
        // is worthless.
        if new.exists() {
            // Copy-stage-rename, not a bare rename: `repos/` can be a
            // mount of its own, where renaming out of it fails with
            // EXDEV and would leave the legacy file sitting in the
            // checkout -- the one outcome this whole pass exists to
            // prevent.
            let parked = new.with_extension("legacy.md");
            let staged = new.with_extension("legacy.md.part");
            if std::fs::copy(&old, &staged).is_ok()
                && std::fs::rename(&staged, &parked).is_ok()
                && std::fs::remove_file(&old).is_ok()
            {
                tracing::info!(
                    "repo {rid} already had notes at the new path; parked the copy \
                     left in its clone at {}",
                    parked.display()
                );
            } else {
                let _ = std::fs::remove_file(&staged);
                tracing::warn!(
                    "repo {rid} has notes at the new path and a copy still in its \
                     clone at {} that could not be moved aside",
                    old.display()
                );
            }
            continue;
        }
        // Copy to a temporary beside the destination and rename it into
        // place, so `new` only ever appears complete. A copy written
        // straight to `new` and interrupted leaves a truncated file that
        // every later pass skips, and `repo_notes` would serve the
        // truncation forever.
        //
        // Copy rather than rename the original: `repos/` can be a mount
        // of its own, where a cross-device rename fails with EXDEV. The
        // temporary is beside `new`, so that rename is always same-device.
        let staged = new.with_extension("md.part");
        if std::fs::copy(&old, &staged).is_err() {
            let _ = std::fs::remove_file(&staged);
            continue;
        }
        if std::fs::rename(&staged, &new).is_err() {
            let _ = std::fs::remove_file(&staged);
            continue;
        }
        if std::fs::remove_file(&old).is_ok() {
            moved += 1;
            tracing::info!("moved repo {rid} notes out of its clone directory");
        } else {
            // The content is safe at the new path; what is left is the
            // copy in the checkout, which is the hazard.
            tracing::warn!(
                "repo {rid} notes were copied out of its clone but {} could not be \
                 removed; it is still in the working tree",
                old.display()
            );
        }
    }
    moved
}

/// Finish any deletion left flagged by a crash or a failed reclamation.
pub async fn reap_deleted_repos(
    store: &Store,
    work_root: &Path,
    notes_lock: &tokio::sync::Mutex<()>,
) -> usize {
    let repos_dir = work_root.join("repos");
    let ids = store.deleted_repo_ids().await.unwrap_or_default();
    let mut reaped = 0;
    for id in ids {
        match reap_repo(store, &repos_dir, id, notes_lock).await {
            Ok(()) => reaped += 1,
            Err(e) => tracing::warn!(
                "repo {id} is flagged deleted but {} could not be removed ({e}); \
                 its row stays until the reaper retries",
                Store::repo_dir(&repos_dir, id).display()
            ),
        }
    }
    reaped
}

async fn delete_repo(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    // Not `_guard`: this one is released explicitly below, before
    // reclamation takes the same lock itself.
    let guard = state.repo_notes.lock().await;
    let repos_dir = state.config.work_root.join("repos");
    let Some(repo) = state.store.get_repo_by_id(rid).await? else {
        // Deleting something already deleted is not an error. A client
        // that lost the first response and retried must not be told the
        // repo never existed — and the retry is a useful nudge to
        // reattempt reclamation. An id that was never a repo still 404s.
        //
        // The window closes when reclamation does: once the directory is
        // gone the row is gone, and that id is then indistinguishable
        // from one that was never used.
        if state.store.repo_is_deleted(rid).await? {
            drop(guard);
            let _ = reap_repo(&state.store, &repos_dir, rid, &state.repo_notes).await;
            return Ok(Json(json!({ "ok": true })).into_response());
        }
        return Err(ApiError::NotFound(format!("no repo {rid}")));
    };
    // Phase one: flag the row. StoreWriteError::Refused -> 400 with the
    // store's exact message via From<StoreWriteError>. After this the
    // repo is gone from every read path, so the caller is told the truth
    // by a 200 even though the files are still on disk.
    state.store.soft_delete_repo(rid).await?;
    state
        .store
        .log_user_event(
            &user,
            "repo",
            &format!("deleted {} (#{rid})", repo.name),
            None,
        )
        .await?;
    // Phase two, attempted inline so the common case finishes before the
    // response: reclaim the directory, then drop the row. The row is what
    // records that `repos/repo-<id>` is still on disk, so it goes only once
    // the files have -- the id itself is never reissued (`repos.id` is
    // AUTOINCREMENT, migration 009). A failure here is not an error for the
    // caller — the repo *is* deleted — it just leaves the files on disk
    // until the reaper retries on the next cycle.
    // The guard goes before reclamation, which now takes the lock
    // itself: holding it here as well would deadlock, and the row is
    // already flagged, so an append can no longer resolve this repo.
    drop(guard);
    if let Err(e) = reap_repo(&state.store, &repos_dir, rid, &state.repo_notes).await {
        tracing::warn!(
            "repo {rid} deleted, but reclaiming {} failed ({e}); \
             its files stay until the reaper retries",
            Store::repo_dir(&repos_dir, rid).display()
        );
    }
    Ok(Json(json!({ "ok": true })).into_response())
}

// -- POST /api/repo/rehunt (WRITES contract §8.1) -------------------------------

/// Queue a full re-hunt of one repo: its next cold hunt reviews the
/// complete history, and the rotation runs it ahead of the repo's other
/// scans. A paused repo keeps the request until it is resumed.
async fn request_rehunt(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let repo = fetch_repo(&state.store, rid).await?;
    // The lookup above and this write are separate statements; a delete
    // landing between them leaves nothing to queue the hunt on.
    if !state.store.request_full_hunt(rid).await? {
        return Err(ApiError::NotFound(format!("no repo {rid}")));
    }
    state
        .store
        .log_user_event(
            &user,
            "repo",
            &format!("full re-hunt queued for {}", repo.name),
            None,
        )
        .await?;
    let refreshed = fetch_repo(&state.store, rid).await?;
    // The rotation has an urgent hunt now; pick it without waiting for
    // the loop's next natural wake.
    state.scheduler.wake.notify_one();
    Ok(Json(json!({ "ok": true, "repo": refreshed })).into_response())
}

// -- POST /api/repo/notes (contract §9) -------------------------------------------

async fn add_repo_note(
    State(state): State<AppState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    raw: Bytes,
) -> Result<Response, ApiError> {
    post_gate(&headers)?;
    let body = parse_object(&raw)?;
    let rid = require_int(&body, "id", "id must be an integer")?;
    let note = match body.get("note") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_owned(),
        _ => {
            return Err(ApiError::BadRequest(
                "note must be a non-empty string".to_owned(),
            ));
        }
    };
    // Present, non-null, non-string -> 400; empty/whitespace string -> None.
    let category = match body.get("category") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_owned())
        }
        Some(_) => {
            return Err(ApiError::BadRequest("category must be a string".to_owned()));
        }
    };
    let _guard = state.repo_notes.lock().await;
    let repo = fetch_repo(&state.store, rid).await?;
    let notes = Store::append_repo_note(
        &state.config.work_root,
        rid,
        &repo.name,
        &note,
        category.as_deref(),
    )
    .map_err(|err| ApiError::Internal(err.into()))?;
    let mut msg = format!("note added to {}", repo.name);
    if let Some(category) = &category {
        use std::fmt::Write;
        let _ = write!(msg, " [{category}]");
    }
    state
        .store
        .log_user_event(&user, "repo", &msg, None)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "ok": true, "notes": notes })),
    )
        .into_response())
}
