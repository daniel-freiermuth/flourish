#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The resume policy: what the scheduler does with a suspended attempt.
//!
//! The mechanism (a `resumed_from` link, a resumable-jobs query, a chain
//! sum, an omp `--resume`) is tested elsewhere. What is tested here is
//! the policy laid over it — when a suspension is chosen, what budget it
//! is granted, when it is abandoned, and what happens when the
//! transcript it named has gone.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use hunter::backend::{Backend, JobClass, Outlook, Verdict};
use hunter::config::Config;
use hunter::domain::{FindingJobKind, FindingStatus, JobKind, JobState, RepoJobKind};
use hunter::scheduler::{
    Attempt, Candidate, ResumePlan, Resumed, kind, pick_next, preview_next, record_job, run_fix,
    run_harvest, run_hunt, run_recheck,
};
use hunter::server::{AppState, SchedulerHandle, router};
use hunter::store::{FindingInsert, JobStart, Store};
use hunter::types::RunResult;
use hunter::workspace::Workspace;
use sqlx::SqlitePool;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, git};
use tower::util::ServiceExt;

/// The per-kind typical cost every test here is calibrated against.
///
/// Three completed hunts at the same cost, so `anticipated_tokens`
/// returns it whether it reads p50 (warm) or p90 (cold) — the tests are
/// about the resume arithmetic, not about cache warmth.
const Z: i64 = 100_000;

/// `input + cacheRead + cacheWrite` of the LAST usage record in the
/// transcript [`seed_session`] writes.
const CTX: i64 = 200_000;

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the seed pool and the `Store` opened on `path`, and SQLite's handles
/// are closed before the files go.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("sched-resume");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Read-write Store: selection retires a suspension it can never
/// continue (past the give-up ceiling, or its working directory gone), so
/// the selection path under test writes.
async fn rw_store(path: &Path) -> Store {
    Store::connect(path).await.unwrap()
}

fn test_config(root: &Path) -> Config {
    Config {
        root: root.to_path_buf(),
        work_root: root.join("data"),
        db_path: root.join("hunter.db"),
        serve_port: 0,
        serve_host: std::net::IpAddr::from(std::net::Ipv4Addr::LOCALHOST),
        allowed_hosts: hunter::config::HostAllowList::default(),
        ui_dir: root.join("ui"),
        omp_bin: "omp".to_owned(),
        stale_after_s: 300.0,
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
        fix_max_wall_s: 2700,
        hunt_rehunt_days: 90,
        scan_interval_days: 1.0,
        modernization_interval_days: 30,
        standards_interval_days: 30,
        renovate_github_token: None,
        review_bots: hunter::forge::ReviewBots::default(),
    }
}

/// One enabled repo whose clone directory really exists.
///
/// It has to exist: a hunt runs in the clone, and a resume is only
/// offered when the directory the suspended worker was using is still
/// there.
async fn seed_repo(pool: &SqlitePool, dir: &TempDir) -> PathBuf {
    let clone = dir.path().join("repo-1");
    std::fs::create_dir_all(&clone).unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/alpha.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(clone.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    clone
}

/// Three completed hunts, all costing [`Z`]. Also makes the repo "warm",
/// which the constant is chosen to be indifferent to.
async fn seed_history(pool: &SqlitePool) {
    seed_history_at(pool, Z).await;
}

/// Three completed hunts, all costing `tokens`, so the per-kind
/// estimate is `tokens` whichever percentile `anticipated_tokens` reads.
async fn seed_history_at(pool: &SqlitePool, tokens: i64) {
    for id in 1..=3_i64 {
        sqlx::query(
            "INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES (?1, 'hunt', 1, 'done', ?2, 1000, 2000)",
        )
        .bind(id)
        .bind(tokens)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A worker transcript whose context at suspension is [`CTX`], in the
/// workspace of chain 10.
fn seed_session(dir: &TempDir) -> PathBuf {
    seed_session_with_ctx(dir, CTX)
}

/// The workspace of the chain whose first job is `origin`, as a resume
/// expects to find it: a tree directory and a session directory. Returns
/// the session directory.
fn seed_workspace(dir: &TempDir, origin: i64) -> PathBuf {
    let root = test_config(dir.path())
        .work_root
        .join("jobs")
        .join(origin.to_string());
    std::fs::create_dir_all(root.join("tree")).unwrap();
    let session = root.join("session");
    std::fs::create_dir_all(&session).unwrap();
    session
}

/// A worker transcript in omp's session format.
///
/// Two assistant calls. The first is a small opening exchange; the
/// second is where the session had got to when it was suspended, and its
/// `input + cacheRead + cacheWrite` is `ctx`. Only that last record
/// should count: re-establishing a session costs the context it had
/// reached, not the sum of every call that built it.
fn seed_session_with_ctx(dir: &TempDir, ctx: i64) -> PathBuf {
    seed_session_in(dir, 10, ctx)
}

/// [`seed_session_with_ctx`] in the workspace of chain `origin`.
fn seed_session_in(dir: &TempDir, origin: i64, ctx: i64) -> PathBuf {
    let path = seed_workspace(dir, origin).join("session.jsonl");
    let opening = serde_json::json!({"message": {"role": "assistant", "usage":
        {"input": 500, "output": 200, "cacheRead": 0, "cacheWrite": 37_000}}});
    let noise = serde_json::json!({"message": {"role": "user", "content": "noise"}});
    let last = serde_json::json!({"message": {"role": "assistant", "usage":
        {"input": 1_000, "output": 900, "cacheRead": ctx - 11_000, "cacheWrite": 10_000}}});
    std::fs::write(&path, format!("{opening}\n{noise}\n{last}\n")).unwrap();
    path
}

/// Finding `id` on repo 1 whose pull request is open but asks for
/// nothing: no tier has work for it.
async fn seed_quiet_finding(pool: &SqlitePool, id: i64) {
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (?1, 'bug', 1, 'fp' || ?1, 'high', 0.9, 'bug ' || ?1, 'pr_open', \
                 1000, 1000)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// Finding `id` on repo 1, its pull request open and flagged for
/// attention: the engage tier's work.
async fn seed_flagged_finding(pool: &SqlitePool, id: i64) {
    seed_quiet_finding(pool, id).await;
    sqlx::query(
        "INSERT INTO pr_state \
         (finding_id, pr_number, state, needs_attention, attention_since, synced_at) \
         VALUES (?1, 1, 'OPEN', 'review_comments', 1000, 5000)",
    )
    .bind(id)
    .execute(pool)
    .await
    .unwrap();
}

/// A suspended finding-kind job on repo 1, with a transcript.
async fn seed_finding_suspension(
    pool: &SqlitePool,
    id: i64,
    kind: &str,
    finding_id: i64,
    session: &Path,
) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, finding_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (?1, ?2, 1, ?3, 'suspended', ?4, 'cap', 40000, 1000, 2000, 'pinned')",
    )
    .bind(id)
    .bind(kind)
    .bind(finding_id)
    .bind(session.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    support::point_checkpoint(pool, finding_id, kind, id).await;
}

/// The `resume` events logged against job `id`, oldest first.
async fn resume_events(pool: &SqlitePool, id: i64) -> Vec<(String, Option<i64>)> {
    sqlx::query_as::<_, (String, Option<i64>)>(
        "SELECT message, finding_id FROM events WHERE kind = 'resume' AND job_id = ?1 \
         ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// One suspended hunt on repo 1, with a transcript and a measured cost:
/// the checkpoint of the repo's hunt.
async fn seed_suspension(pool: &SqlitePool, id: i64, session: &Path, tokens: i64) {
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (?1, 'hunt', 1, 'suspended', ?2, 'cap', ?3, 1000, 2000, 'pinned')",
    )
    .bind(id)
    .bind(session.to_string_lossy().to_string())
    .bind(tokens)
    .execute(pool)
    .await
    .unwrap();
    support::point_repo_checkpoint(pool, 1, "hunt", Some(id)).await;
}

/// A chain of linked attempts at one piece of work, oldest first, with
/// `tokens[i]` as each attempt's own cost. Returns the newest id.
///
/// The newest is the repo's hunt checkpoint, as a chain whose every
/// attempt paused leaves it. So this is exactly the shape `pick_resume`
/// sees when it has to decide whether to continue a chain yet again.
async fn seed_chain(pool: &SqlitePool, session: &Path, tokens: &[i64]) -> i64 {
    let mut id = 9_i64;
    let mut previous: Option<i64> = None;
    for &t in tokens {
        id += 1;
        sqlx::query(
            "INSERT INTO jobs \
             (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
              resumed_from, started_at, finished_at, pinned_sha) \
             VALUES (?1, 'hunt', 1, 'suspended', ?2, 'cap', ?3, ?4, 1000, 2000, 'pinned')",
        )
        .bind(id)
        .bind(session.to_string_lossy().to_string())
        .bind(t)
        .bind(previous)
        .execute(pool)
        .await
        .unwrap();
        previous = Some(id);
    }
    support::point_repo_checkpoint(pool, 1, "hunt", Some(id)).await;
    id
}

async fn job_row(pool: &SqlitePool, id: i64) -> (String, Option<String>, Option<String>) {
    sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT state, killed_reason, notes FROM jobs WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The job repo 1's hunt continues from, if any.
async fn hunt_checkpoint(store: &Store) -> Option<i64> {
    store
        .repo_checkpoint_job(1, RepoJobKind::Hunt)
        .await
        .unwrap()
        .map(|j| j.id)
}

/// The chain ending at `newest` was given up: the hunt's checkpoint is
/// dropped, the row stays as it ended (`suspended` / `cap`), and the one
/// `resume` event against it says why. Returns that message.
async fn assert_given_up(pool: &SqlitePool, store: &Store, newest: i64) -> String {
    assert_eq!(
        hunt_checkpoint(store).await,
        None,
        "the checkpoint is dropped"
    );
    let (state, reason, _) = job_row(pool, newest).await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("suspended", Some("cap")),
        "the given-up attempt's row is left as it ended"
    );
    let events = resume_events(pool, newest).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let (msg, _) = events.into_iter().next().unwrap();
    assert!(msg.contains("giving up"), "{msg}");
    msg
}

fn resume_plan_of(c: Option<Candidate>) -> ResumePlan {
    match c {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    }
}

// -- the reservation ---------------------------------------------------------

/// A resumed attempt reserves its context back, plus what is left of the
/// per-kind typical after everything the chain has already spent.
///
/// The context term is the point: the first call of a resumed session
/// re-establishes the whole transcript, measured at a median ratio of
/// 1.00 across 112 production re-cache events. Reserving only the
/// leftover estimate would under-reserve by exactly the amount that
/// makes resuming cheaper than restarting.
///
/// The context is small here so that the leftover estimate, not the
/// work floor, is the larger work term.
#[tokio::test]
async fn resume_reserves_context_plus_the_remaining_estimate() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let ctx = 20_000;
    let session = seed_session_with_ctx(&dir, ctx);
    let spent = 40_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.predecessor_id(), 10);
    assert_eq!(
        plan.origin_job_id(),
        10,
        "a first resume continues the root"
    );
    assert_eq!(plan.session_file(), session);
    assert_eq!(
        plan.anticipated(),
        ctx + (Z - spent),
        "ctx {ctx} + max({Z} - {spent}, 25_000)"
    );
}

/// A resume reserves at least as much work as it re-sends context, even
/// once the chain has spent past the per-kind typical.
///
/// Re-sending the transcript is pure overhead. Under a flat 25,000
/// work floor, a 100,000-token transcript whose chain had exhausted the
/// typical reserved 125,000, and a window with just that much room
/// admitted an attempt that spent 80% of its budget re-sending and 20%
/// working. Reserving 200,000 makes the gate wait for a window that can
/// fund an attempt that is at least half work.
#[tokio::test]
async fn a_resume_reserves_at_least_as_much_work_as_it_resends_context() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session_with_ctx(&dir, 100_000);
    // Over the typical, so `z - chain_spent` is negative, but well under
    // the give-up ceiling of 3x.
    let spent = Z + 30_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.ctx(), 100_000);
    assert_eq!(
        plan.anticipated(),
        200_000,
        "100_000 of context + at least 100_000 of work"
    );
}

/// An unreadable transcript leaves the first call's cost unknown, and
/// the per-kind typical is the only other estimate of this work that
/// exists. Reserving nothing would let the ramp grant a job whose very
/// first call it cannot afford.
#[tokio::test]
async fn resume_falls_back_to_the_per_kind_estimate_when_the_transcript_is_unreadable() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let missing = seed_workspace(&dir, 10).join("never-written.jsonl");
    let spent = 40_000;
    seed_suspension(&pool, 10, &missing, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(
        plan.ctx(),
        Z,
        "the per-kind typical stands in for the context"
    );
    assert_eq!(
        plan.anticipated(),
        2 * Z,
        "ctx {Z} + max({Z} - {spent}, as much work as context)"
    );
}

/// One line of an omp transcript: an assistant message with its usage.
fn assistant(stop: &str, input: i64, cache_read: i64, cache_write: i64) -> String {
    format!(
        r#"{{"type":"message","message":{{"role":"assistant","stopReason":"{stop}","usage":{{"input":{input},"output":0,"cacheRead":{cache_read},"cacheWrite":{cache_write}}}}}}}"#
    )
}

/// What omp writes when the watchdog SIGTERMs a worker mid-call.
const SIGTERM_EXIT: &str =
    r#"{"type":"custom","customType":"session_exit","data":{"reason":"sigterm","kind":"signal"}}"#;

fn write_session(dir: &TempDir, lines: &[String]) -> PathBuf {
    let path = seed_workspace(dir, 10).join("session.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

/// A cap kill ends the transcript with the killed call: a `session_exit`
/// record, then an `aborted` assistant message whose usage is all zeros.
/// That record measured nothing; the context the resume re-caches is the
/// last REAL call's. Shaped on production job 4374, whose last real call
/// had 2 + 76,466 + 423 of context and whose resume's first call read
/// 76,889 from cache — while the reservation was computed from ctx 0.
#[tokio::test]
async fn resume_measures_context_from_the_last_real_call_not_the_killed_one() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = write_session(
        &dir,
        &[
            assistant("toolUse", 3, 0, 36_900),
            assistant("toolUse", 2, 70_100, 6_366),
            assistant("toolUse", 2, 76_466, 423),
            SIGTERM_EXIT.to_owned(),
            assistant("aborted", 0, 0, 0),
        ],
    );
    let spent = 40_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(
        hunter::backends::omp_scavenge::harness::ctx_at_suspension(&session),
        Some(2 + 76_466 + 423)
    );
    assert_eq!(plan.ctx(), 76_891);
    assert_eq!(
        plan.anticipated(),
        2 * 76_891,
        "ctx 76_891 + max({Z} - {spent}, as much work as context)"
    );
}

/// A transcript whose only calls were killed holds no measurement, which
/// is the same as holding none: the per-kind typical stands in, exactly
/// as for an unreadable file. A 0 would reserve nothing for
/// the re-cache the resume's first call pays.
#[tokio::test]
async fn resume_falls_back_to_the_per_kind_estimate_when_every_call_was_killed() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = write_session(
        &dir,
        &[SIGTERM_EXIT.to_owned(), assistant("aborted", 0, 0, 0)],
    );
    let spent = 40_000;
    seed_suspension(&pool, 10, &session, spent).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(
        hunter::backends::omp_scavenge::harness::ctx_at_suspension(&session),
        None
    );
    assert_eq!(
        plan.ctx(),
        Z,
        "the per-kind typical stands in for the context"
    );
    assert_eq!(plan.anticipated(), 2 * Z);
}

// -- the give-up ceiling -----------------------------------------------------

/// Resumed links that spent nothing — zero or unrecorded spend, as a
/// provider failure before the first response leaves — consume no work
/// retry; later progress can still continue the chain.
#[tokio::test]
async fn zero_progress_handoffs_do_not_exhaust_the_useful_work_retry_limit() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[120_000, 0, 0, 110_000]).await;
    // The handoffs failed before any response, so they were not cap kills;
    // one recorded no spend at all, as a failed backend leaves it.
    sqlx::query(
        "UPDATE jobs SET killed_reason = NULL, \
         tokens_new = CASE id WHEN ?1 THEN NULL ELSE tokens_new END \
         WHERE id IN (?1, ?2)",
    )
    .bind(newest - 1)
    .bind(newest - 2)
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let stats = store.resume_chain_stats(newest, None).await.unwrap();
    assert_eq!(stats.attempts, 2);
    assert_eq!(stats.total, 230_000);
    let picked = pick_next(&support::scope(&store), &test_config(dir.path()), None)
        .await
        .unwrap();
    assert!(
        matches!(picked, Some(Candidate::Resume { plan, .. }) if plan.predecessor_id() == newest)
    );
}

/// Cap kills re-suspend on their own, so they count as attempts even when
/// they recorded no spend: otherwise nothing would ever end such a chain.
#[tokio::test]
async fn zero_spend_cap_kills_still_exhaust_the_attempt_limit() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[120_000, 0, 0, 0]).await;
    let store = rw_store(&path).await;
    assert_eq!(
        store
            .resume_chain_stats(newest, None)
            .await
            .unwrap()
            .attempts,
        4
    );

    let picked = pick_next(&support::scope(&store), &test_config(dir.path()), None)
        .await
        .unwrap();

    assert!(
        !matches!(picked, Some(Candidate::Resume { .. })),
        "a chain of four cap kills must not be resumed again, got {picked:?}"
    );
    assert_given_up(&pool, &store, newest).await;
}

/// A chain that has been tried too many times is abandoned, however
/// cheap each attempt was.
///
/// This is the arm that actually fires in practice. A suspension that
/// resumes, does a little, and suspends again never trips a spend
/// threshold, so without a count the chain runs forever with every link
/// looking individually reasonable. The give-up is a failure of the hunt
/// (`resume chain gave up`), and nothing else moves: the hunt is as stale
/// as before, so the same selection starts it fresh (WORK-MODEL.md §17).
#[tokio::test]
async fn a_chain_out_of_attempts_is_given_up_and_counted() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Four cheap attempts: 40_000 all told, nowhere near 3x the 100_000
    // typical, so only the attempt count can end this.
    let newest = seed_chain(&pool, &session, &[10_000; 4]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                ..
            })
        ),
        "the hunt starts over fresh, got {picked:?}"
    );
    let streak = store
        .repo_work_streak(1, hunter::domain::RepoWorkKind::Hunt)
        .await
        .unwrap();
    assert_eq!(
        (streak.streak, streak.last_failure.as_deref()),
        (1, Some("resume chain gave up"))
    );
    let why = assert_given_up(&pool, &store, newest).await;
    assert!(
        why.contains("40000 tok") && why.contains("4 attempts"),
        "the record must name what the chain cost and how often it was tried, got {why:?}"
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a given-up chain must never be offered again"
    );
}

/// A chain whose attempts OTHER than its biggest have outspent the
/// per-kind typical three times over is abandoned.
///
/// The second arm, and the one that catches a chain burning real money
/// before it has been tried four times.
#[tokio::test]
async fn a_chain_that_outspends_the_typical_outside_its_biggest_attempt_is_retired() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Two attempts, so the count arm cannot fire: 800_000 spent, of
    // which 400_000 is outside the larger attempt, against 3x 100_000.
    let newest = seed_chain(&pool, &session, &[400_000, 400_000]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    assert!(
        matches!(picked, Some(Candidate::Repo { .. })),
        "must fall through to normal selection, got {picked:?}"
    );
    assert_given_up(&pool, &store, newest).await;
}

/// A chain of ONE attempt is never retired, however expensive it was.
///
/// A first suspension is a single attempt whose spend is, by definition
/// of a cap kill, at least its own cap — so comparing it against a
/// multiple of the per-kind typical retires the work before resume has
/// been tried even once, which is the exact opposite of what this
/// feature is for. Live symptom: every hunt on one repo recorded
/// `failed`/`give-up` with 88,697 spent against an 80,493 cap, and no
/// job was ever resumed at all.
#[tokio::test]
async fn a_chain_of_one_attempt_is_never_retired() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    // Fifty times the typical, on a single attempt that has never been
    // continued. There is nothing here to conclude that continuing does
    // not work, because continuing has not been tried.
    seed_suspension(&pool, 10, &session, 50 * Z).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.predecessor_id(), 10);
    let (state, _, _) = job_row(&pool, 10).await;
    assert_eq!(state, JobState::Suspended.as_str());
}

/// One enormous attempt is evidence about the size of the JOB, not
/// about the chain being stuck, so the chain is judged on what it spent
/// besides that attempt.
///
/// Without setting it aside, the per-kind typical is the wrong
/// yardstick for this job and a big-but-healthy piece of work is retired
/// on its first resume — the whole chain here is over 3x the typical,
/// and all but 10,000 of it is one attempt.
#[tokio::test]
async fn the_biggest_attempt_is_set_aside_before_the_chain_is_judged() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[500_000, 10_000]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.predecessor_id(), newest);
    assert_eq!(
        plan.chain_spent(),
        510_000,
        "5x the typical, and still fine"
    );
}

/// Exactly at the ceiling is still resumable: "more than three times the
/// typical" is the condition, and a chain that has spent precisely 3x
/// outside its biggest attempt has not passed it.
#[tokio::test]
async fn the_give_up_ceiling_is_exclusive() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[400_000, 3 * Z]).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );
    assert_eq!(plan.predecessor_id(), newest);
}

// -- selection order ---------------------------------------------------------

/// A resume outranks starting new background work, but not a human
/// waiting on a pull request.
///
/// A queued fix is a finding someone triaged and asked for; the resume
/// tier sits below it and above repo rotation.
#[tokio::test]
async fn a_queued_fix_outranks_a_resume_which_outranks_rotation() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (5, 'bug', 1, 'fp5', 'high', 0.9, 'queued bug', 'queued', 1000, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();
    assert_eq!(
        picked.as_ref().map(Candidate::target_id),
        Some(5),
        "the human-queued fix wins, got {picked:?}"
    );

    // Drop the fix and the same suspension beats repo rotation.
    sqlx::query("DELETE FROM findings WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );
    assert_eq!(plan.predecessor_id(), 10);
}

/// A suspension whose chain tree is gone is retired, not merely
/// skipped, and never offered.
///
/// A resumed worker continues a conversation, not a filesystem: its
/// transcript refers to files in that tree, so no later cycle can
/// continue it either. Skipping it left it offered forever — never
/// resumed, never ended. Dropping the checkpoint at selection, in the same
/// walk, keeps it from being offered again, which matters because this
/// tier outranks rotation. The row stays as the attempt ended.
#[tokio::test]
async fn a_suspension_whose_working_directory_is_gone_is_given_up() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let cfg = test_config(dir.path());
    let workspace = cfg.work_root.join("jobs").join("10");
    std::fs::remove_dir_all(workspace.join("tree")).unwrap();
    let store = rw_store(&path).await;

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                ..
            })
        ),
        "a suspension with no tree falls through to rotation, got {picked:?}"
    );
    assert_eq!(hunt_checkpoint(&store).await, None);
    let (state, reason, _) = job_row(&pool, 10).await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("suspended", Some("cap"))
    );
    let msg = format!(
        "resume hunt alpha: job 10 given up, workspace {} is gone",
        workspace.display()
    );
    assert_eq!(resume_events(&pool, 10).await, vec![(msg, None)]);
}

/// A flagged pull request whose engage was cap-killed continues that
/// engage instead of starting a fresh one.
///
/// The finding tiers outrank the resume tier, so they are the ones that
/// must notice. Live: engage job 4358 suspended with a valid transcript;
/// nine minutes later the attention tier picked the same finding and
/// started a fresh engage for 92,294 tokens where resuming would have
/// re-cached about 75,000 — and the fresh attempt's worktree handling
/// removed the directory the suspended transcript refers to.
///
/// Two newer suspensions sit in front of the right one — an engage for
/// another finding and a fix for this one — so matching on either half
/// of (finding, kind) alone picks the wrong job.
#[tokio::test]
async fn a_flagged_pr_resumes_its_suspended_engage_instead_of_starting_fresh() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_flagged_finding(&pool, 5).await;
    seed_quiet_finding(&pool, 6).await;
    for (id, kind, finding) in [(20, "engage", 5), (21, "engage", 6), (22, "fix", 5)] {
        let session = seed_session_in(&dir, id, CTX);
        seed_finding_suspension(&pool, id, kind, finding, &session).await;
    }
    let store = rw_store(&path).await;

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    let Some(Candidate::Resume { label, plan, .. }) = picked else {
        panic!("expected a resume of the suspended engage, got {picked:?}");
    };
    assert_eq!(plan.predecessor_id(), 20);
    assert_eq!(plan.kind(), JobKind::from(FindingJobKind::Engage));
    assert_eq!(plan.finding_id(), Some(5));
    assert_eq!(
        label.as_deref(),
        Some("bug 5"),
        "the tier keeps its own label"
    );
}

/// A plan reaches a runner only as a [`Resumed`] of its own kind: an
/// engage's plan loaded for the fix runner is refused, where handed to
/// `run_fix` beside its finding it would have started a row of the
/// engage's kind and run the fix's logic in it. Loaded as an engage, its
/// subject is the finding the plan names.
#[tokio::test]
async fn a_plan_loads_only_as_its_own_kind() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_flagged_finding(&pool, 5).await;
    let session = seed_session_in(&dir, 20, CTX);
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;
    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );
    assert_eq!(plan.kind(), JobKind::from(FindingJobKind::Engage));

    assert!(
        Resumed::<kind::Fix>::load(&store, &plan).await.is_err(),
        "an engage's plan must not load as a fix"
    );

    let engage = Resumed::<kind::Engage>::load(&store, &plan).await.unwrap();
    assert_eq!(Attempt::Resume(&engage).subject().id, 5);
}

/// A budget-overridden finding with a suspended attempt resumes it and
/// keeps the override.
///
/// The override tier jumps the queue, so it must continue rather than
/// restart just like the tiers below it — and the resume still carries
/// the override, or the summary preview judges it by the normal budget
/// while the executor runs it under the prioritized one.
#[tokio::test]
async fn an_overridden_finding_resumes_its_suspended_attempt_under_the_override() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    let session = seed_session_in(&dir, 20, CTX);
    seed_flagged_finding(&pool, 5).await;
    sqlx::query("UPDATE findings SET budget_override = 'once' WHERE id = 5")
        .execute(&pool)
        .await
        .unwrap();
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        picked.budget_override(),
        Some(hunter::domain::BudgetOverride::Once)
    );
    assert_eq!(resume_plan_of(Some(picked)).predecessor_id(), 20);
}

/// The same flagged pull request, but the suspended engage's tree is
/// gone: the work item gives that checkpoint up and the tier starts fresh.
///
/// This is job 4358 after a fresh engage removed its worktree. It can
/// never be resumed, so it is given up on the first cycle that sees it.
/// The job row stays as it ended; only the pointer goes.
#[tokio::test]
async fn a_flagged_pr_whose_suspended_engage_lost_its_worktree_starts_fresh() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    let session = seed_session_in(&dir, 20, CTX);
    let workspace = cfg.work_root.join("jobs").join("20");
    std::fs::remove_dir_all(workspace.join("tree")).unwrap();
    seed_flagged_finding(&pool, 5).await;
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    assert!(
        matches!(
            picked,
            Some(Candidate::Finding {
                kind: FindingJobKind::Engage,
                finding_id: 5,
                ..
            })
        ),
        "a suspension that cannot be continued must not block the fresh engage, got {picked:?}"
    );
    let (state, reason, _) = job_row(&pool, 20).await;
    assert_eq!(state, JobState::Suspended.as_str());
    assert_eq!(reason.as_deref(), Some("cap"));
    assert_eq!(checkpoint_of(&store, 5, FindingJobKind::Engage).await, None);
    assert_eq!(
        resume_events(&pool, 20).await,
        vec![(
            format!(
                "resume engage alpha: job 20 given up, workspace {} is gone",
                workspace.display()
            ),
            Some(5)
        )]
    );
}

// -- fresh work supersedes a suspension --------------------------------------

/// Starting a finding's work fresh drops that work item's checkpoint.
///
/// Whatever path started it, the fresh attempt now owns the work, and
/// its worktree handling reclaims the directory the old transcript
/// refers to. The old job row stays as it ended. Other findings and other
/// kinds are different work and keep their checkpoints.
#[tokio::test]
async fn fresh_work_on_a_finding_supersedes_its_suspended_attempt() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let session = seed_session(&dir);
    seed_flagged_finding(&pool, 5).await;
    seed_quiet_finding(&pool, 6).await;
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    seed_finding_suspension(&pool, 21, "engage", 6, &session).await;
    seed_finding_suspension(&pool, 22, "fix", 5, &session).await;
    let store = rw_store(&path).await;

    let created = support::scope(&store)
        .create_job(
            JobStart::fresh_finding(FindingJobKind::Engage, 5),
            None,
            Some(40_000),
        )
        .await
        .unwrap();
    let fresh = created.job.disown();
    assert_eq!(
        created.superseded,
        vec![20],
        "the caller is told which chain to release before building its own"
    );

    let (state, reason, notes) = job_row(&pool, 20).await;
    assert_eq!(state, JobState::Suspended.as_str());
    assert_eq!(reason.as_deref(), Some("cap"));
    assert_eq!(notes, None);
    assert_eq!(checkpoint_of(&store, 5, FindingJobKind::Engage).await, None);
    let msg = format!("resume engage alpha: checkpoint job 20 superseded by fresh job {fresh}");
    assert_eq!(resume_events(&pool, 20).await, vec![(msg, Some(5))]);
    for (other, fid, kind) in [
        (21, 6, FindingJobKind::Engage),
        (22, 5, FindingJobKind::Fix),
    ] {
        assert_eq!(
            checkpoint_of(&store, fid, kind).await,
            Some(other),
            "job {other} is different work and must stay resumable"
        );
    }
}

/// Starting a repo-level kind fresh drops that repo's checkpoint at the
/// same kind, and nothing else. The row it named stays as it ended.
#[tokio::test]
async fn fresh_work_on_a_repo_supersedes_its_suspended_attempt() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/beta.git', '/nowhere', 'github', 'main', 1, \
                 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    for (id, kind, repo) in [(11, "test_gap", 1), (12, "hunt", 2)] {
        sqlx::query(
            "INSERT INTO jobs \
             (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
              started_at, finished_at) \
             VALUES (?1, ?2, ?3, 'suspended', ?4, 'cap', 40000, 1000, 2000)",
        )
        .bind(id)
        .bind(kind)
        .bind(repo)
        .bind(session.to_string_lossy().to_string())
        .execute(&pool)
        .await
        .unwrap();
        support::point_repo_checkpoint(&pool, repo, kind, Some(id)).await;
    }
    let store = rw_store(&path).await;

    let fresh = support::scope(&store)
        .create_job(
            JobStart::fresh_repo(RepoJobKind::Hunt, 1),
            None,
            Some(40_000),
        )
        .await
        .unwrap()
        .job
        .disown();

    assert_eq!(hunt_checkpoint(&store).await, None);
    let (state, reason, _) = job_row(&pool, 10).await;
    assert_eq!(
        (state.as_str(), reason.as_deref()),
        ("suspended", Some("cap")),
        "the superseded checkpoint's row is left as it ended"
    );
    let msg = format!("resume hunt alpha: checkpoint job 10 superseded by fresh job {fresh}");
    assert_eq!(resume_events(&pool, 10).await, vec![(msg, None)]);
    for (other, repo, kind) in [(11, 1, RepoJobKind::TestGap), (12, 2, RepoJobKind::Hunt)] {
        assert_eq!(
            store
                .repo_checkpoint_job(repo, kind)
                .await
                .unwrap()
                .map(|j| j.id),
            Some(other),
            "job {other} is different work and must stay resumable"
        );
    }
}

/// A cycle forced onto one repo resumes that repo's suspension and no
/// other.
///
/// Forcing is the operator asking for work on THIS repo. Continuing
/// another repo's suspended job instead would spend the budget they
/// meant for this one on work they did not ask for, and report it as the
/// cycle they forced. Checked from both sides, so the answer cannot
/// depend on which suspension the query happens to list first.
#[tokio::test]
async fn a_forced_cycle_resumes_only_the_forced_repos_suspension() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let beta = dir.path().join("repo-2");
    std::fs::create_dir_all(&beta).unwrap();
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (2, 'beta', 'https://example.com/beta.git', ?1, 'github', 'main', 1, 1000)",
    )
    .bind(beta.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs \
         (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
          started_at, finished_at, pinned_sha) \
         VALUES (20, 'hunt', 2, 'suspended', ?1, 'cap', 10000, 1000, 2000, 'pinned')",
    )
    .bind(seed_session_in(&dir, 20, CTX).to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    support::point_repo_checkpoint(&pool, 2, "hunt", Some(20)).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    for (forced, repo_id, suspended) in [("alpha", 1, 10), ("beta", 2, 20)] {
        let picked = pick_next(&support::scope(&store), &cfg, Some(forced))
            .await
            .unwrap();
        assert_eq!(
            picked.as_ref().map(Candidate::repo_id),
            Some(repo_id),
            "forcing {forced} must stay on {forced}, got {picked:?}"
        );
        assert_eq!(resume_plan_of(picked).predecessor_id(), suspended);
    }
}

/// A resume is shown under its repo's name, as the fresh hunt it
/// replaces would be: the `/api/summary` preview displays the
/// candidate's label, and an unlabelled resume there reads as work on
/// nothing in particular.
#[tokio::test]
async fn a_resume_is_labelled_with_its_repos_name() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 10_000).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    assert!(
        matches!(picked, Some(Candidate::Resume { .. })),
        "{picked:?}"
    );
    assert_eq!(picked.as_ref().and_then(Candidate::label), Some("alpha"));
}

// -- the transcript that vanished --------------------------------------------

/// When the resume source has gone, the attempt fails and the
/// predecessor stops being the checkpoint.
///
/// Nothing was spawned, so `failed` is the honest state for the attempt.
/// The checkpoint must go: keeping it would offer the same missing
/// session every cycle forever. The predecessor's row is not rewritten —
/// that attempt really did stop on `cap`, and overwriting it with the
/// successor's problem would lose the only record of why the work
/// stopped. The event says what happened.
#[tokio::test]
async fn resume_unavailable_fails_the_attempt_and_gives_up_the_checkpoint() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    let store = rw_store(&path).await;
    // Picked before the attempt exists: selection skips a place a running
    // job holds.
    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &test_config(dir.path()), None)
            .await
            .unwrap(),
    );

    let attempt = support::scope(&store)
        .create_job(JobStart::Resume { checkpoint: 10 }, None, Some(260_000))
        .await
        .unwrap()
        .job;
    let attempt_id = attempt.id();
    // What the harness returns when it refuses to hand omp a path it
    // cannot resolve: nothing spawned, no exit code, no transcript.
    let rr = RunResult {
        exit_code: None,
        killed_reason: Some("resume-unavailable".to_owned()),
        tokens_new: 0,
        calls: 0,
        session_file: None,
        duration_s: 0.0,
        stdout_tail: "cannot resume: session directory is gone".to_owned(),
        usage_delta: None,
    };

    let state = record_job(&store, attempt, &rr, None, Some(&plan))
        .await
        .unwrap()
        .state();

    assert_eq!(
        state,
        JobState::Failed,
        "nothing ran, so nothing was killed"
    );
    let (attempt_state, _, _) = job_row(&pool, attempt_id).await;
    assert_eq!(attempt_state, JobState::Failed.as_str());

    let (pred_state, pred_reason, _) = job_row(&pool, 10).await;
    assert_eq!(
        (pred_state.as_str(), pred_reason.as_deref()),
        ("suspended", Some("cap")),
        "the predecessor's row is not rewritten"
    );
    assert_eq!(hunt_checkpoint(&store).await, None);
    let gone: Vec<String> =
        sqlx::query_scalar("SELECT message FROM events WHERE kind = 'error' AND job_id = ?1")
            .bind(attempt_id)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        gone,
        ["resume hunt alpha: session gone, checkpoint job 10 given up"]
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "a missing transcript must not be retried forever"
    );
}

// -- executing a resumed attempt ---------------------------------------------

/// What the executor actually handed the backend.
#[derive(Debug)]
struct Seen {
    cwd: PathBuf,
    prompt: String,
    resume_from: Option<PathBuf>,
}

/// A backend that records its `run` arguments and stages a worker's
/// output at a path fixed when it is built.
///
/// `ScriptedBackend` only sees the worktree, and the three things a
/// resume changes about a run — the prompt, the resume source, and
/// which output file the worker is really writing — are invisible
/// through it.
struct RecordingBackend {
    seen: OnceLock<Seen>,
    writes: PathBuf,
    body: String,
}

impl RecordingBackend {
    fn new(writes: PathBuf, body: String) -> Self {
        Self {
            seen: OnceLock::new(),
            writes,
            body,
        }
    }

    fn seen(&self) -> &Seen {
        self.seen.get().expect("the backend was never run")
    }
}

#[async_trait::async_trait]
impl Backend for RecordingBackend {
    async fn decide(&self, _anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        let granted = Verdict::Granted {
            cap_tokens: None,
            reason: "test: always granted".to_owned(),
        };
        Ok(Outlook {
            normal: granted.clone(),
            prioritized: granted,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn worker_hosts(&self) -> Vec<String> {
        Vec::new()
    }

    async fn run(
        &self,
        ws: &Workspace,
        prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        // `set` fails only if this backend ran twice, which no test
        // here does — each one builds its own.
        let _ = self.seen.set(Seen {
            cwd: ws.tree.clone(),
            prompt: prompt.to_owned(),
            resume_from: resume_from.map(Path::to_path_buf),
        });
        std::fs::write(&self.writes, &self.body).expect("stage worker output");
        Ok(RunResult {
            exit_code: Some(0),
            killed_reason: None,
            tokens_new: 30_000,
            calls: 4,
            session_file: Some(
                resume_from
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            duration_s: 1.0,
            stdout_tail: String::new(),
            usage_delta: None,
        })
    }
}

/// A real clone with a bare origin, registered as repo 1.
async fn cloned_repo(dir: &TempDir, pool: &SqlitePool) -> GitRepo {
    let repo = GitRepo::with_branch(dir, "feature");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(repo.work.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    repo
}

/// [`cloned_repo`] plus history, a transcript and a suspended hunt whose
/// id is `pred`.
async fn executable_repo(dir: &TempDir, pool: &SqlitePool, pred: i64) -> (GitRepo, PathBuf) {
    let repo = cloned_repo(dir, pool).await;
    seed_history(pool).await;
    let session = seed_session_in(dir, pred, CTX);
    seed_suspension(pool, pred, &session, 40_000).await;
    let head = git(&repo.work, &["rev-parse", "HEAD"]);
    sqlx::query("UPDATE jobs SET pinned_sha = ?1 WHERE id = ?2")
        .bind(head.trim())
        .bind(pred)
        .execute(pool)
        .await
        .unwrap();
    (repo, session)
}

fn one_finding() -> String {
    serde_json::json!([{
        "fingerprint": "alpha:src/lib.rs:parse:1",
        "type": "bug",
        "file": "src/lib.rs",
        "line": 12,
        "bug_class": "boundary",
        "severity": "high",
        "confidence": 0.9,
        "summary": "off-by-one in the parser",
        "detail": "found before the suspension",
        "evidence_plan": "failing test first"
    }])
    .to_string()
}

/// A resumed hunt hands omp the predecessor's transcript and a one-line
/// continuation, and reads back the output file the ORIGINAL prompt
/// named.
///
/// The output path is the part that makes the whole feature work or not.
/// The playbook bakes `out/job<N>.findings.json` into the prompt, so a
/// continuing worker writes the first attempt's path forever. An
/// executor that looked at its own id would find nothing, ingest
/// nothing, and advance no watermark — putting the identical work back
/// in the rotation next cycle, which is the loop this feature exists to
/// end.
#[tokio::test]
async fn a_resumed_hunt_continues_the_session_and_ingests_the_origin_output_path() {
    let (dir, path, pool) = fresh_db().await;
    let (_repo, session) = executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let plan = match pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    };
    let backend = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );

    let summary = run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Resume(&Resumed::load(&store, &plan).await.unwrap()),
        &backend,
    )
    .await
    .unwrap();

    let seen = backend.seen();
    assert_eq!(
        seen.prompt,
        "Continue the work you were doing in this session. \
         You were interrupted; pick up where you left off.",
        "a resumed run must not re-send the playbook it is already looking at"
    );
    assert_eq!(seen.resume_from.as_deref(), Some(session.as_path()));
    assert_eq!(
        seen.cwd,
        cfg.work_root.join("jobs").join("10").join("tree"),
        "a resume runs in its chain's tree"
    );
    assert_eq!(
        summary.ingest.map(|i| i.inserted),
        Some(1),
        "the findings the continuing worker wrote must be ingested"
    );

    let (kind, resumed_from, estimated): (String, Option<i64>, Option<i64>) =
        sqlx::query_as("SELECT kind, resumed_from, estimated_tokens FROM jobs WHERE id = ?1")
            .bind(summary.job_id.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(kind, "hunt", "a resume runs as the kind that was suspended");
    assert_eq!(resumed_from, Some(10));
    assert_eq!(
        estimated,
        Some(plan.anticipated()),
        "the row records the resume's reservation, not a cold estimate"
    );

    let after = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert!(
        after.last_hunt_sha.is_some(),
        "a resume that completes advances the watermark like any other Done hunt"
    );
}

/// A resumed run does not sync the clone.
///
/// Fast-forwarding moves the tree out from under a session whose
/// transcript describes the old one — and `run_hunt` writes the HEAD it
/// reads as the hunt watermark on Done, so a resume that pulled first
/// would mark commits reviewed that no worker ever looked at.
///
/// Proven by removing the remote the sync needs: a cold hunt cannot get
/// past it, and the resume does not care.
#[tokio::test]
async fn a_resumed_hunt_does_not_sync_the_clone() {
    let (dir, path, pool) = fresh_db().await;
    let (repo, _session) = executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let plan = match pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    };
    git(&repo.work, &["remote", "remove", "origin"]);
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();

    let cold = RecordingBackend::new(cfg.work_root.join("out").join("cold.json"), String::new());
    run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &cold,
    )
    .await
    .expect_err("a cold hunt has to sync, and cannot without a remote");

    let warm = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );
    let summary = run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Resume(&Resumed::load(&store, &plan).await.unwrap()),
        &warm,
    )
    .await
    .expect("a resume must not touch the remote at all");
    assert_eq!(summary.state, Some(JobState::Done));
}

/// A resume whose checkpoint moved on between the pick and the insert
/// (an operator restarted the work, another attempt continued it) skips
/// the cycle instead of failing it: the next selection reads where the
/// work is now, so a failure would only back all other work off.
#[tokio::test]
async fn a_resume_whose_checkpoint_moved_on_skips_the_cycle() {
    let (dir, path, pool) = fresh_db().await;
    executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let plan = match pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap()
    {
        Some(Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected a resume candidate, got {other:?}"),
    };
    sqlx::query("UPDATE repo_work SET checkpoint_job_id = NULL WHERE checkpoint_job_id = 10")
        .execute(&pool)
        .await
        .unwrap();
    let backend = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );
    let jobs_before = store.list_jobs(10).await.unwrap().len();

    let summary = run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Resume(&Resumed::load(&store, &plan).await.unwrap()),
        &backend,
    )
    .await
    .expect("a checkpoint that moved on is a skip, not the cycle's error");

    assert!(
        summary.skipped.is_some(),
        "the cycle must report a skip: {summary:?}"
    );
    assert_eq!(
        store.list_jobs(10).await.unwrap().len(),
        jobs_before,
        "no successor row"
    );
}

/// A whole cycle picks the suspension up and runs it as the kind that
/// was suspended.
///
/// The tests above call `run_hunt` directly, which assumes the routing
/// they are exercising. This one goes through `run_cycle`, so the
/// suspended row's `kind` really is what decides which executor runs —
/// a resume is not its own job type, it is a hunt that continues.
#[tokio::test]
async fn a_cycle_dispatches_a_resume_to_the_suspended_kinds_executor() {
    let (dir, path, pool) = fresh_db().await;
    executable_repo(&dir, &pool, 10).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let backend = RecordingBackend::new(
        cfg.work_root.join("out").join("job10.findings.json"),
        one_finding(),
    );

    let summary =
        hunter::scheduler::run_cycle(&mut support::jobs(&store), &cfg, &backend, None).await;

    assert_eq!(summary.kind.map(|k| k.to_string()), Some("hunt".to_owned()));
    assert_eq!(summary.state, Some(JobState::Done));
    assert_eq!(summary.ingest.map(|i| i.inserted), Some(1));
    assert!(
        backend.seen().resume_from.is_some(),
        "the cycle must hand the executor the predecessor's transcript"
    );
    assert!(
        store.list_resumable_jobs().await.unwrap().is_empty(),
        "the successor row takes the suspension out of the pool"
    );
}

// -- the cold reservation ----------------------------------------------------

/// A backend that records the reservation the gate asks about and
/// refuses it, so an executor stops at the gate and nothing runs.
#[derive(Default)]
struct GateProbe {
    reserved: OnceLock<i64>,
}

impl GateProbe {
    fn reserved(&self) -> Option<i64> {
        self.reserved.get().copied()
    }
}

#[async_trait::async_trait]
impl Backend for GateProbe {
    async fn decide(&self, anticipated_tokens: i64) -> anyhow::Result<Outlook> {
        // `set` fails only if one probe gated twice; each run gets its own.
        let _ = self.reserved.set(anticipated_tokens);
        let denied = Verdict::Denied {
            reason: "test: recording the reservation".to_owned(),
            retry_at: None,
        };
        Ok(Outlook {
            normal: denied.clone(),
            prioritized: denied,
        })
    }

    async fn keep_fresh(&self) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn status_html(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }

    fn worker_hosts(&self) -> Vec<String> {
        Vec::new()
    }

    async fn run(
        &self,
        _ws: &Workspace,
        _prompt: &str,
        _cap_tokens: Option<i64>,
        _max_wall_s: i64,
        _job_class: JobClass,
        _resume_from: Option<&Path>,
    ) -> anyhow::Result<RunResult> {
        anyhow::bail!("every reservation was refused, so nothing may run")
    }
}

/// A cold start reserves the per-kind history, but never less than a
/// cold session's fixed context plus as much again for work: 40,000.
///
/// Live: mis-metered rows once dragged the history to 1,876 tokens, and
/// the gate then started hunts with about 10,000 of headroom that died
/// on their second call. A history above the floor is still what gets
/// reserved — the floor is a minimum, not a replacement.
#[tokio::test]
async fn a_cold_start_reserves_at_least_the_efficiency_floor() {
    let (dir, path, pool) = fresh_db().await;
    cloned_repo(&dir, &pool).await;
    seed_history_at(&pool, 1_876).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();

    let collapsed = GateProbe::default();
    let summary = run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &collapsed,
    )
    .await
    .unwrap();
    assert!(summary.denied.is_some(), "the probe refuses: {summary:?}");
    sqlx::query("UPDATE jobs SET tokens_new = 100000 WHERE state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let healthy = GateProbe::default();
    run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &healthy,
    )
    .await
    .unwrap();

    assert_eq!(
        (collapsed.reserved(), healthy.reserved()),
        (Some(40_000), Some(100_000)),
        "max(history, 15_000 + 15_000): the floor over a collapsed history, \
         the history over the floor"
    );
}

/// The reservation `/api/summary` asked the backend about when it
/// previewed the next candidate.
async fn previewed_reservation(store: &Arc<Store>, root: &Path) -> Option<i64> {
    let probe = Arc::new(GateProbe::default());
    let mut config = test_config(root);
    config.ui_dir = root.join("ui");
    std::fs::create_dir_all(&config.ui_dir).unwrap();
    let state = AppState {
        store: Arc::clone(store),
        config: Arc::new(config),
        backend: probe.clone(),
        repo_notes: Arc::new(tokio::sync::Mutex::new(())),
        scheduler: SchedulerHandle::default(),
    };
    let cookie = support::auth_cookie(&state.store).await;
    let response = router(state)
        .oneshot(
            Request::builder()
                .header(axum::http::header::COOKIE, cookie.as_str())
                .uri("/api/summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    probe.reserved()
}

/// The summary's "what's next" preview asks the budget about the same
/// reservation the gate will: a resume's own, and a cold start's
/// floored estimate.
///
/// Anything else previews a budget decision the scheduler is not going
/// to make — "allowed" for a resume the gate will refuse, or for a hunt
/// the floor holds back.
#[tokio::test]
async fn the_summary_preview_reserves_what_the_gate_reserves() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    let session = seed_session(&dir);
    seed_suspension(&pool, 10, &session, 40_000).await;
    let store = Arc::new(rw_store(&path).await);

    let resume = previewed_reservation(&store, dir.path()).await;

    sqlx::query("DELETE FROM jobs WHERE id = 10")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET tokens_new = 1876 WHERE state = 'done'")
        .execute(&pool)
        .await
        .unwrap();
    let cold = previewed_reservation(&store, dir.path()).await;

    assert_eq!(
        (resume, cold),
        (Some(CTX + CTX), Some(40_000)),
        "the resume's ctx {CTX} + as much work as context; a cold hunt's floor over 1_876"
    );
}

// -- a suspension is not a failure -------------------------------------------

/// A worker stopped at the cap with a transcript: what the harness
/// reports as a suspension.
fn suspended_at_cap(session: &Path) -> RunResult {
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(session.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// A real clone registered as repo 1 with one bug finding in `status`,
/// and stub playbooks that render with the slots each builder supplies.
async fn repo_with_finding(
    dir: &TempDir,
    pool: &SqlitePool,
    store: &Store,
    status: FindingStatus,
) -> i64 {
    let repo = GitRepo::with_branch(dir, "feature");
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(repo.work.to_string_lossy().to_string())
    .execute(pool)
    .await
    .unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("fix.md"), "fix {{WORKTREE}}\n").unwrap();
    std::fs::write(
        playbooks.join("recheck.md"),
        "recheck {{REPO_PATH}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    std::fs::write(playbooks.join("harvest.md"), "harvest {{WORKTREE}}\n").unwrap();
    let (fid, _) = store
        .upsert_finding(
            1,
            &FindingInsert {
                fingerprint: "alpha:README.md:seed:1".to_owned(),
                file: "README.md".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "a real bug".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_status(fid, status).await.unwrap();
    fid
}

/// Three suspensions of one fix in a row leave the finding queued with no
/// failure streak. Counting them rejected a healthy fix as "stuck" after
/// three budget pauses.
#[tokio::test]
async fn repeated_suspensions_of_a_fix_are_not_a_stuck_streak() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Queued).await;
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_fix(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &suspending,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Queued, "{outcomes:?}");
    assert_eq!(streak(&store, fid, FindingJobKind::Fix).await, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}

/// Three suspensions of one recheck in a row leave the finding rechecking
/// with no failure streak. Counting them reset a healthy recheck back to
/// the inbox as "stuck" after three budget pauses.
#[tokio::test]
async fn repeated_suspensions_of_a_recheck_are_not_a_stuck_streak() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_recheck(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &suspending,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Rechecking, "{outcomes:?}");
    assert_eq!(streak(&store, fid, FindingJobKind::Recheck).await, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}

/// Run one confirmed recheck of a medium finding whose verdict file names
/// `updated_severity`, and return the finding as every whole-list read
/// sees it afterwards.
async fn confirmed_recheck_with_severity(raw: &'static str) -> hunter::types::Finding {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    let cfg = test_config(dir.path());
    let out = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
    let worker = ScriptedBackend::new(move |_| {
        let verdict = serde_json::json!({
            "verdict": "confirmed",
            "reason": "still there",
            "updated_severity": raw,
        });
        std::fs::write(&out, verdict.to_string()).unwrap();
        support::done()
    });

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    let summary = run_recheck(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&finding),
        &worker,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("confirmed"), "{summary:?}");

    // The reads are whole-list queries decoding `severity` as a
    // `Severity`: one undecodable row fails every one of them.
    let all = store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .expect("findings still decode after the recheck");
    assert_eq!(all.len(), 1);
    all.into_iter().next().unwrap()
}

/// A worker's `updated_severity` is stored canonically: `"High"` is a
/// high finding, not a row that no read can decode.
#[tokio::test]
async fn a_recheck_severity_in_any_case_is_stored_canonically() {
    let finding = confirmed_recheck_with_severity("High").await;
    assert_eq!(finding.severity, hunter::domain::Severity::High);
    assert_eq!(finding.status, FindingStatus::New);
}

/// A severity outside the enum is dropped; the confirmation still lands
/// and the finding keeps the severity it had.
#[tokio::test]
async fn a_recheck_severity_outside_the_enum_is_not_stored() {
    let finding = confirmed_recheck_with_severity("critical").await;
    assert_eq!(finding.severity, hunter::domain::Severity::Medium);
    assert_eq!(finding.status, FindingStatus::New);
}

/// A stale recheck (the code moved on) supersedes the finding and an
/// invalid one rejects it; only the rejection reaches the suppression list
/// every later scan is shown. Stale used to land as `wontfix`: all seven
/// `wontfix` findings it set in production were code that had been
/// removed or rewritten, suppressed as if someone had decided against them.
/// The rejection is anchored to the commit the recheck's tree was at,
/// watching the finding's file and the ones its `depends_on` names, here
/// written as one string rather than an array.
#[tokio::test]
async fn a_stale_recheck_supersedes_and_only_an_invalid_one_suppresses() {
    for (verdict, status, suppressed) in [
        ("stale", FindingStatus::Superseded, false),
        ("invalid", FindingStatus::Rejected, true),
    ] {
        let (dir, path, pool) = fresh_db().await;
        let store = rw_store(&path).await;
        let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
        let cfg = test_config(dir.path());
        let out = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
        let worker = ScriptedBackend::new(move |_| {
            let body = serde_json::json!({
                "verdict": verdict,
                "reason": "src/x.rs:1 is gone",
                "holds_while": "seed() returns early (src/x.rs:1)",
                "depends_on": "src/x.rs:1, src/y.rs",
            });
            std::fs::write(&out, body.to_string()).unwrap();
            support::done()
        });

        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_recheck(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &worker,
        )
        .await
        .unwrap();

        assert_eq!(summary.outcome.as_deref(), Some(verdict));
        let after = store.get_finding(fid).await.unwrap().unwrap();
        assert_eq!(after.status, status, "{verdict}");
        assert_eq!(
            after.verdict_reason.as_deref(),
            Some("recheck: src/x.rs:1 is gone"),
            "{verdict}"
        );
        let listed = store
            .suppressions(1, "bug")
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == fid);
        assert_eq!(listed, suppressed, "{verdict}");
        let anchor = store.suppression_anchors(1, "bug").await.unwrap();
        let anchor = anchor.get(&fid);
        if suppressed {
            let pinned = store.pinned_sha(summary.job_id.unwrap()).await.unwrap();
            assert_eq!(
                anchor,
                Some(&hunter::store::VerdictAnchor {
                    sha: pinned.unwrap(),
                    files: vec![
                        "README.md".to_owned(),
                        "src/x.rs".to_owned(),
                        "src/y.rs".to_owned()
                    ],
                    holds_while: Some("seed() returns early (src/x.rs:1)".to_owned()),
                }),
            );
        } else {
            assert_eq!(anchor, None, "{verdict}");
        }
    }
}

/// A recheck that leaves no verdict is retried, and the third identical
/// failure in a row holds it for a human, with its streak cleared; the
/// finding stays `rechecking`.
#[tokio::test]
async fn a_recheck_without_a_verdict_retries_then_is_held() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    let cfg = test_config(dir.path());
    let silent = ScriptedBackend::new(|_| support::done());

    let mut seen = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_recheck(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &silent,
        )
        .await
        .unwrap();
        let after = store.get_finding(fid).await.unwrap().unwrap();
        seen.push((
            summary.outcome,
            after.status,
            streak(&store, fid, FindingJobKind::Recheck).await,
        ));
    }

    assert_eq!(
        seen,
        [
            (Some("requeued".into()), FindingStatus::Rechecking, 1),
            (Some("requeued".into()), FindingStatus::Rechecking, 2),
            (Some("held".into()), FindingStatus::Rechecking, 0),
        ],
        "(outcome, status, recheck_attempts) per attempt"
    );
}

/// A recheck whose tree can never be created -- here the repo names a
/// default branch origin does not have -- is a failed recheck like any
/// other: retried, and the third in a row holds it for a human. Left
/// `rechecking` with nothing counted, the recheck tier (above fix) would
/// take it again every cycle and starve everything below it.
#[tokio::test]
async fn a_recheck_whose_tree_can_never_be_made_is_held() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    sqlx::query("UPDATE repos SET default_branch = 'no-such-branch' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let cfg = test_config(dir.path());
    let never = ScriptedBackend::new(|_| panic!("nothing may run without a tree"));

    let mut seen = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_recheck(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &never,
        )
        .await
        .unwrap();
        assert!(
            summary
                .failure
                .as_deref()
                .is_some_and(|r| r.starts_with("workspace not created")),
            "{summary:?}"
        );
        let after = store.get_finding(fid).await.unwrap().unwrap();
        seen.push((
            summary.outcome,
            after.status,
            streak(&store, fid, FindingJobKind::Recheck).await,
        ));
    }

    assert_eq!(
        seen,
        [
            (Some("requeued".into()), FindingStatus::Rechecking, 1),
            (Some("requeued".into()), FindingStatus::Rechecking, 2),
            (Some("held".into()), FindingStatus::Rechecking, 0),
        ],
        "(outcome, status, recheck_attempts) per attempt"
    );
    let hold = support::hold_of(&store, fid, FindingJobKind::Recheck)
        .await
        .unwrap();
    assert!(hold.detail.contains("no-such-branch"), "{hold:?}");
    assert_eq!(hold.checkpoint_job_id, None, "no worker ran");
}

/// Every end of a recheck attempt spends a `once` override — a
/// suspension, a missing verdict, a landed verdict — and keeps an
/// `exempt` one, which lasts until a human clears it.
#[tokio::test]
async fn every_end_of_a_recheck_attempt_spends_only_a_once_override() {
    use hunter::domain::BudgetOverride;
    for end in ["suspended", "requeued", "confirmed"] {
        for (mode, left) in [
            (BudgetOverride::Once, None),
            (BudgetOverride::Exempt, Some(BudgetOverride::Exempt)),
        ] {
            let (dir, path, pool) = fresh_db().await;
            let store = rw_store(&path).await;
            let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
            store.set_budget_override(fid, Some(mode)).await.unwrap();
            let cfg = test_config(dir.path());
            let out = cfg.work_root.join("out").join(format!("recheck{fid}.json"));
            let worker = ScriptedBackend::new(move |tree| match end {
                "suspended" => {
                    suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
                }
                "requeued" => support::done(),
                _ => {
                    let body =
                        serde_json::json!({ "verdict": "confirmed", "reason": "still there" });
                    std::fs::write(&out, body.to_string()).unwrap();
                    support::done()
                }
            });

            let finding = store.get_finding(fid).await.unwrap().unwrap();
            let summary = run_recheck(
                &support::scope(&store),
                store.test_lane(),
                &cfg,
                Attempt::Cold(&finding),
                &worker,
            )
            .await
            .unwrap();

            assert_eq!(summary.outcome.as_deref(), Some(end), "{end} {mode}");
            let after = store.get_finding(fid).await.unwrap().unwrap();
            assert_eq!(after.budget_override, left, "{end} {mode} override");
        }
    }
}

/// Minimal `gh pr view --json` payload that `view_pr_engage` can parse.
const PR_VIEW_JSON: &str = r#"{"state":"MERGED","mergeable":"MERGEABLE","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"feature","headRefOid":"deadbeef"}"#;

/// Three suspensions of one harvest in a row leave the PR unharvested with
/// no failure streak. Counting them gave up on a healthy harvest after
/// three budget pauses, marking the PR harvested unreviewed.
#[tokio::test]
async fn repeated_suspensions_of_a_harvest_are_not_a_stuck_streak() {
    let bins = FakeBins::acquire("resume-harvest-streak");
    bins.ok("gh", PR_VIEW_JSON);
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Merged).await;
    // `gh` is handed the repo URL, so it has to be one the forge parses;
    // the clone still fetches from its own origin.
    sqlx::query("UPDATE repos SET url = 'https://github.com/acme/widget' WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at) \
         VALUES (?1, 7, 'MERGED', 1)",
    )
    .bind(fid)
    .execute(&pool)
    .await
    .unwrap();
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let suspending = ScriptedBackend::new(|tree| {
        suspended_at_cap(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = store.get_finding(fid).await.unwrap().unwrap();
        let summary = run_harvest(
            &support::scope(&store),
            store.test_lane(),
            &cfg,
            Attempt::Cold(&finding),
            &suspending,
        )
        .await
        .unwrap();
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        outcomes.push(summary.outcome);
    }

    let ps = store.get_pr_state(fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None, "still pending harvest: {outcomes:?}");
    assert_eq!(streak(&store, fid, FindingJobKind::Harvest).await, 0);
    assert!(outcomes.iter().all(|o| o.as_deref() == Some("suspended")));
}

/// A recheck, engage or harvest checkpoint is continued only while its
/// finding is at that kind's status. Once the finding moves on — the
/// operator rejects a finding mid-recheck, the PR an engage was answering
/// merges, a closed PR awaiting harvest is reopened — the work no longer
/// applies: the move drops the checkpoint, selection stops offering it,
/// and the sweep releases its tree instead of keeping both forever. The
/// job row stays as it ended.
#[tokio::test]
async fn a_checkpoint_whose_finding_moved_on_is_dropped_and_its_tree_released() {
    for (kind, at, moved_to) in [
        ("recheck", "rechecking", "rejected"),
        ("engage", "pr_open", "merged"),
        ("harvest", "closed", "pr_open"),
    ] {
        let (dir, path, pool) = fresh_db().await;
        seed_repo(&pool, &dir).await;
        seed_history(&pool).await;
        seed_quiet_finding(&pool, 5).await;
        let session = seed_session_in(&dir, 20, CTX);
        seed_finding_suspension(&pool, 20, kind, 5, &session).await;
        let set_status = |status: &'static str| {
            sqlx::query("UPDATE findings SET status = ?1 WHERE id = 5")
                .bind(status)
                .execute(&pool)
        };
        set_status(at).await.unwrap();
        let cfg = test_config(dir.path());
        let tree = cfg.work_root.join("jobs").join("20").join("tree");
        let store = rw_store(&path).await;

        let picked = resume_plan_of(
            pick_next(&support::scope(&store), &cfg, None)
                .await
                .unwrap(),
        );
        assert_eq!(picked.predecessor_id(), 20, "{kind} resumes at {at}");
        hunter::workspace::sweep(store.test_lane(), &cfg.work_root, hunter::util::now_ms())
            .await
            .unwrap();
        assert!(tree.is_dir(), "{kind} at {at} keeps its tree");

        set_status(moved_to).await.unwrap();
        let picked = pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap();
        assert!(
            !matches!(picked, Some(Candidate::Resume { .. })),
            "{kind} at {moved_to} must not resume, got {picked:?}"
        );
        hunter::workspace::sweep(store.test_lane(), &cfg.work_root, hunter::util::now_ms())
            .await
            .unwrap();
        let (state, reason, notes) = job_row(&pool, 20).await;
        assert_eq!(
            (state.as_str(), reason.as_deref(), notes),
            ("suspended", Some("cap"), None),
            "{kind}"
        );
        let msg = format!("resume {kind}: checkpoint job 20 dropped, finding #5 is {moved_to}");
        assert_eq!(resume_events(&pool, 20).await, vec![(msg, Some(5))]);
        assert!(!tree.exists(), "{kind} at {moved_to} releases its tree");
    }
}

/// A hunt with no new commits records its watermark and skips; a skipped
/// cycle restarts at once. If that write fails, the hunt must fail rather
/// than skip, or the daemon would re-pick the same repo in a tight loop.
#[tokio::test]
async fn a_no_commit_hunt_whose_watermark_cannot_be_written_fails() {
    let (dir, path, pool) = fresh_db().await;
    let repo = cloned_repo(&dir, &pool).await;
    let tip = git(&repo.work, &["rev-parse", "main"]);
    sqlx::query("UPDATE repos SET last_hunt_sha = ?1 WHERE id = 1")
        .bind(tip.trim())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "CREATE TRIGGER no_watermark BEFORE UPDATE OF last_hunt_at ON repos \
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    let row = store.get_repo_by_id(1).await.unwrap().unwrap();
    let never = ScriptedBackend::new(|_| panic!("no new commits: nothing may run"));

    let err = run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &never,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("injected write failure"), "{err}");
}

/// Sparse history dominated by failed handoffs is no cost allowance.
#[tokio::test]
async fn resume_with_a_zero_cost_estimate_uses_the_attempt_limit_not_a_zero_cost_limit() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let session = seed_session(&dir);
    let newest = seed_chain(&pool, &session, &[120_000, 110_000]).await;
    for id in [12, 13, 14] {
        sqlx::query("INSERT INTO jobs (id, kind, repo_id, state, tokens_new, started_at, finished_at) VALUES (?1, 'hunt', 1, 'failed', 0, 1000, 2000)")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE jobs SET finished_at = ?1")
        .bind(hunter::util::now_ms())
        .execute(&pool)
        .await
        .unwrap();
    let store = rw_store(&path).await;
    assert_eq!(
        hunter::scheduler::anticipated_tokens(
            &store,
            &test_config(dir.path()),
            1,
            JobKind::Repo(RepoJobKind::Hunt)
        )
        .await
        .unwrap(),
        0
    );
    let picked = pick_next(&support::scope(&store), &test_config(dir.path()), None)
        .await
        .unwrap();
    assert!(
        matches!(picked, Some(Candidate::Resume { plan, .. }) if plan.predecessor_id() == newest)
    );
}

// -- a given-up chain is a failed attempt ------------------------------------

/// A worker killed at wallclock after doing work, with a transcript: a
/// suspension, like a cap kill.
fn suspended_at_wallclock(session: &Path) -> RunResult {
    std::fs::create_dir_all(session.parent().unwrap()).unwrap();
    std::fs::write(session, "").unwrap();
    RunResult {
        killed_reason: Some("wallclock".to_owned()),
        ..suspended_at_cap(session)
    }
}

/// A recheck whose worker overruns its wall clock on every attempt ends
/// held as `gave_up`, as three killed attempts ended stuck before wallclock
/// kills became suspensions.
///
/// Every attempt is a suspension, which never counts toward the failure
/// streak, so the only bound is the chain's give-up ceiling. Uncounted,
/// the give-up left the finding `rechecking`, the recheck tier started a
/// fresh chain in the same cycle, and the finding took every cycle
/// forever: no hunt or other work ever ran.
#[tokio::test]
async fn a_recheck_that_keeps_overrunning_is_held_instead_of_restarting_forever() {
    let (dir, path, pool) = fresh_db().await;
    let store = rw_store(&path).await;
    let fid = repo_with_finding(&dir, &pool, &store, FindingStatus::Rechecking).await;
    let cfg = test_config(dir.path());
    // The transcript is in the session directory of the chain being run.
    let overrunning = ScriptedBackend::new(|tree| {
        suspended_at_wallclock(&tree.parent().unwrap().join("session").join("session.jsonl"))
    });

    // Three chains of MAX_RESUME_ATTEMPTS each: the third give-up is the
    // third identical failure.
    let mut kinds = Vec::new();
    for _ in 0..12 {
        let summary =
            hunter::scheduler::run_cycle(&mut support::jobs(&store), &cfg, &overrunning, None)
                .await;
        kinds.push(summary.kind.map(|k| k.to_string()));
    }
    let next = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    let finding = store.get_finding(fid).await.unwrap().unwrap();
    assert_eq!(finding.status, FindingStatus::Rechecking);
    assert_eq!(
        support::hold_of(&store, fid, FindingJobKind::Recheck)
            .await
            .map(|h| h.reason),
        Some(hunter::domain::HoldReason::GaveUp),
        "cycles: {kinds:?}, next: {next:?}"
    );
    assert!(
        !matches!(
            next,
            Some(Candidate::Finding { .. } | Candidate::Resume { .. })
        ),
        "the finding must stop taking the cycle, got {next:?}"
    );
}

/// A suspended chain of `MAX_RESUME_ATTEMPTS` attempts at finding `fid`'s
/// `kind` work, ids `first..first + 4`, in the workspace of chain `first`.
async fn seed_finding_chain(pool: &SqlitePool, dir: &TempDir, kind: &str, fid: i64, first: i64) {
    let session = seed_session_in(dir, first, CTX);
    for id in first..first + 4 {
        sqlx::query(
            "INSERT INTO jobs \
             (id, kind, repo_id, finding_id, state, session_file, killed_reason, tokens_new, \
              resumed_from, started_at, finished_at, pinned_sha) \
             VALUES (?1, ?2, 1, ?3, 'suspended', ?4, 'wallclock', 10000, ?5, 1000, 2000, 'pinned')",
        )
        .bind(id)
        .bind(kind)
        .bind(fid)
        .bind(session.to_string_lossy().to_string())
        .bind((id > first).then_some(id - 1))
        .execute(pool)
        .await
        .unwrap();
    }
    support::point_checkpoint(pool, fid, kind, first + 3).await;
}

/// Finding 1 on repo 1 in `status`, with a merged pull request.
async fn seed_finding_in(pool: &SqlitePool, status: &str) {
    sqlx::query(
        "INSERT INTO findings \
         (id, type, repo_id, fingerprint, severity, confidence, summary, status, \
          created_at, updated_at) \
         VALUES (1, 'bug', 1, 'fp1', 'high', 0.9, 'bug 1', ?1, 1000, 1000)",
    )
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at) \
         VALUES (1, 1, 'MERGED', 5000)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Give up three chains of `kind` work on finding 1, one per selection,
/// and return what the streak and the finding looked like after each,
/// whether the given-up chain is held as a blocked fix's checkpoint, and
/// whether that same selection started finding 1's work fresh.
async fn give_up_three_chains(
    kind: &str,
    status: &str,
) -> Vec<(i64, FindingStatus, bool, bool, bool)> {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    seed_finding_in(&pool, status).await;
    if kind == "engage" {
        // An open PR, flagged: the engage tier's work.
        sqlx::query(
            "UPDATE pr_state SET state = 'OPEN', needs_attention = 'new_comments' \
             WHERE finding_id = 1",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let mut seen = Vec::new();
    for first in [10, 20, 30] {
        seed_finding_chain(&pool, &dir, kind, 1, first).await;
        let next = pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap();
        let fresh = matches!(next, Some(Candidate::Finding { finding_id: 1, .. }));
        let (state, _, _) = job_row(&pool, first + 3).await;
        assert_eq!(
            state, "suspended",
            "the given-up job row is left as it ended"
        );
        let held = support::hold_of(&store, 1, kind.parse().unwrap())
            .await
            .is_some();
        let checkpoint = checkpoint_of(&store, 1, kind.parse().unwrap()).await;
        assert_eq!(
            checkpoint,
            held.then_some(first + 3),
            "a given-up chain keeps its checkpoint only when it is held"
        );
        let f = store.get_finding(1).await.unwrap().unwrap();
        let ps = store.get_pr_state(1).await.unwrap().unwrap();
        let streak = streak(&store, 1, kind.parse().unwrap()).await;
        seen.push((streak, f.status, ps.harvested_at.is_some(), held, fresh));
    }
    seen
}

/// Each given-up fix chain is one failed fix attempt, and the fix starts
/// over; the third holds the fix for the operator, with that chain as its
/// checkpoint, like any three identical failures, and starts nothing. The
/// finding stays `queued`.
#[tokio::test]
async fn a_given_up_fix_chain_counts_toward_the_failure_streak() {
    let seen = give_up_three_chains("fix", "queued").await;
    assert_eq!(
        seen,
        [
            (1, FindingStatus::Queued, false, false, true),
            (2, FindingStatus::Queued, false, false, true),
            (0, FindingStatus::Queued, false, true, false),
        ]
    );
}

/// Each given-up recheck chain is one failed recheck, and the recheck
/// starts over; the third holds it, with that chain as its checkpoint.
#[tokio::test]
async fn a_given_up_recheck_chain_counts_toward_the_failure_streak() {
    let seen = give_up_three_chains("recheck", "rechecking").await;
    assert_eq!(
        seen,
        [
            (1, FindingStatus::Rechecking, false, false, true),
            (2, FindingStatus::Rechecking, false, false, true),
            (0, FindingStatus::Rechecking, false, true, false),
        ]
    );
}

/// Each given-up harvest chain is one failed harvest, and the harvest
/// starts over; the third holds it, with that chain as its checkpoint,
/// where it used to give the PR up as harvested, unreviewed.
#[tokio::test]
async fn a_given_up_harvest_chain_counts_toward_the_failure_streak() {
    let seen = give_up_three_chains("harvest", "merged").await;
    assert_eq!(
        seen,
        [
            (1, FindingStatus::Merged, false, false, true),
            (2, FindingStatus::Merged, false, false, true),
            (0, FindingStatus::Merged, false, true, false),
        ]
    );
}

/// Every streak event a given-up chain writes, through the one that ends
/// the work, says why the chain was given up. The streak key alone
/// ("resume chain gave up") left whoever triages the finding to dig the
/// reason out of a separate `resume` event.
#[tokio::test]
async fn a_given_up_chains_streak_events_say_why_it_was_given_up() {
    for (kind, status) in [
        ("fix", "queued"),
        ("recheck", "rechecking"),
        ("harvest", "merged"),
    ] {
        let (dir, path, pool) = fresh_db().await;
        seed_repo(&pool, &dir).await;
        seed_history(&pool).await;
        seed_finding_in(&pool, status).await;
        let store = rw_store(&path).await;
        let cfg = test_config(dir.path());
        for first in [10, 20, 30] {
            seed_finding_chain(&pool, &dir, kind, 1, first).await;
            pick_next(&support::scope(&store), &cfg, None)
                .await
                .unwrap();
        }

        let counted: Vec<String> = store
            .recent_events(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind != "hold" && e.message.contains("resume chain gave up"))
            .map(|e| e.message)
            .collect();

        assert_eq!(counted.len(), 3, "{kind}: {counted:?}");
        assert!(
            counted
                .iter()
                .all(|m| m.contains("giving up after") && m.contains("4 attempts, the limit")),
            "{kind}: {counted:?}"
        );
    }
}

/// A given-up engage chain is one failed engage, as for every other kind:
/// it counts in the engage streak and the flagged PR's engage starts
/// fresh; the third holds it for the operator. The PR's attention is not
/// touched. Until the work model engage had no streak, so a given-up chain
/// set the attention aside instead, and the PR left review until it
/// changed with nothing but an error event to show for it.
#[tokio::test]
async fn a_given_up_engage_chain_counts_toward_the_failure_streak() {
    let seen = give_up_three_chains("engage", "pr_open").await;
    assert_eq!(
        seen,
        [
            (1, FindingStatus::PrOpen, false, false, true),
            (2, FindingStatus::PrOpen, false, false, true),
            (0, FindingStatus::PrOpen, false, true, false),
        ]
    );
}

/// Finding 1 queued for a fix that has failed twice on given-up chains,
/// with a third chain past the ceiling: the next selection holds it.
async fn fix_one_give_up_from_blocked() -> (TempDir, PathBuf, SqlitePool) {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    seed_finding_in(&pool, "queued").await;
    sqlx::query(
        "INSERT INTO finding_work (finding_id, kind, streak, last_failure) \
         VALUES (1, 'fix', 2, 'resume chain gave up')",
    )
    .execute(&pool)
    .await
    .unwrap();
    seed_finding_chain(&pool, &dir, "fix", 1, 30).await;
    (dir, path, pool)
}

/// The summary's preview changes nothing: a chain the scheduler would
/// give up, and count as the failure that holds the finding, is left for
/// the scheduler — the dashboard polls the preview, paused or not.
#[tokio::test]
async fn the_summary_preview_neither_retires_nor_counts_a_given_up_chain() {
    let (dir, path, pool) = fix_one_give_up_from_blocked().await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());

    let previewed = preview_next(&store, &cfg).await.unwrap();
    let after_preview = (
        job_row(&pool, 33).await.0,
        streak(&store, 1, FindingJobKind::Fix).await,
    );
    pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();
    let after_pick = (
        job_row(&pool, 33).await.0,
        support::hold_of(&store, 1, FindingJobKind::Fix)
            .await
            .map(|h| h.reason),
    );

    assert!(
        matches!(previewed, Some(Candidate::Finding { finding_id: 1, .. })),
        "{previewed:?}"
    );
    assert_eq!(after_preview, ("suspended".to_owned(), 2));
    assert_eq!(
        after_pick,
        (
            "suspended".to_owned(),
            Some(hunter::domain::HoldReason::GaveUp)
        )
    );
}

/// `continue` on a fix held after its third given-up chain resumes that
/// chain: the ceiling counts only what the scheduler did since the
/// release (WORK-MODEL.md §8), so the operator's call gets a fresh budget
/// where giving the chain up again would turn it into a fresh start that
/// leaves the checkpoint behind.
#[tokio::test]
async fn a_continued_fix_resumes_past_the_ceiling() {
    let (dir, path, pool) = fix_one_give_up_from_blocked().await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();
    assert!(
        support::hold_of(&store, 1, FindingJobKind::Fix)
            .await
            .is_some()
    );

    support::release(
        &store,
        1,
        FindingJobKind::Fix,
        hunter::domain::ReleaseAction::Continue,
    )
    .await;
    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.predecessor_id(), 33);
    assert_eq!(job_row(&pool, 33).await.0, "suspended");
}

/// Nothing but an operator's release resets the ceiling: a hold that ended
/// because the finding left its tier, a released claim or a daemon restart
/// leave the count where it was (`operator_requeued` used to infer an
/// operator from a leftover column, which any of those could set).
#[tokio::test]
async fn only_a_release_gives_a_chain_a_fresh_budget() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    seed_history(&pool).await;
    seed_finding_in(&pool, "queued").await;
    seed_finding_chain(&pool, &dir, "fix", 1, 30).await;
    let store = rw_store(&path).await;
    let cfg = test_config(dir.path());
    // A chain of four, its attempts long before any release.
    sqlx::query("UPDATE jobs SET started_at = 1000 WHERE id BETWEEN 30 AND 33")
        .execute(&pool)
        .await
        .unwrap();
    // A hold released before the chain started gives it nothing.
    sqlx::query(
        "INSERT INTO work_holds (finding_id, kind, reason, detail, held_at, released_at, \
         released_by, release_action) VALUES (1, 'fix', 'stuck', 'x', 100, 500, 'tester', 'continue')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let picked = preview_next(&store, &cfg).await.unwrap();

    assert!(
        matches!(picked, Some(Candidate::Finding { finding_id: 1, .. })),
        "the chain is past the ceiling and gives way to a fresh fix: {picked:?}"
    );
}

/// The failure streak of finding `fid`'s `kind` work.
async fn streak(store: &Store, fid: i64, kind: FindingJobKind) -> i64 {
    store.work_streak(fid, kind).await.unwrap().streak
}

/// The job finding `fid`'s `kind` work continues from, if any.
async fn checkpoint_of(store: &Store, fid: i64, kind: FindingJobKind) -> Option<i64> {
    store.checkpoint_job(fid, kind).await.unwrap().map(|j| j.id)
}

/// `dismiss` ends a held engage: the operator has said this PR's attention
/// needs nothing from hunter. So it drops the engage's checkpoint too, and
/// nothing resumes the engage that was held, where it used to be picked up
/// by the next cycle and push or reply on the PR the operator had just
/// dismissed (WORK-MODEL.md §6.3).
#[tokio::test]
async fn a_dismissed_engage_does_not_resume() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_flagged_finding(&pool, 5).await;
    let session = seed_session_in(&dir, 20, CTX);
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;
    assert!(
        store
            .hold_work(
                5,
                FindingJobKind::Engage,
                hunter::domain::HoldReason::Prerequisite,
                "two reviewers disagree",
                Some(20)
            )
            .await
            .unwrap()
    );
    assert_eq!(
        checkpoint_of(&store, 5, FindingJobKind::Engage).await,
        Some(20)
    );

    support::release(
        &store,
        5,
        FindingJobKind::Engage,
        hunter::domain::ReleaseAction::Dismiss,
    )
    .await;

    let next = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(
            next,
            Some(Candidate::Resume { .. } | Candidate::Finding { .. })
        ),
        "{next:?}"
    );
    assert_eq!(checkpoint_of(&store, 5, FindingJobKind::Engage).await, None);
}

/// Engage is active while its finding is `pr_open`; the attention flag only
/// decides whether it may start fresh (WORK-MODEL.md §6.1, G1). A paused
/// engage whose PR lost its flag for a cycle — `sync_prs` recomputes it,
/// and it flaps with a flaky check — is still continued by the engage
/// tier, ahead of a queued fix. It used to wait for the resume tier,
/// below every finding tier.
#[tokio::test]
async fn a_paused_engage_continues_while_its_pr_is_open_whatever_the_flag() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_quiet_finding(&pool, 5).await;
    let session = seed_session_in(&dir, 20, CTX);
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    sqlx::query(
        "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) \
         VALUES (6, 'bug', 1, 'fp6', 'high', 0.9, 'bug 6', 'queued', 1000, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = rw_store(&path).await;

    let plan = resume_plan_of(
        pick_next(&support::scope(&store), &cfg, None)
            .await
            .unwrap(),
    );

    assert_eq!(plan.predecessor_id(), 20);
    assert_eq!(plan.kind(), JobKind::from(FindingJobKind::Engage));
}

/// The flag still gates a fresh start: a flagless PR whose engage
/// checkpoint can no longer be continued (its tree is gone) has nothing to
/// do, and the selection moves on rather than starting an engage nobody
/// asked for.
#[tokio::test]
async fn a_flagless_pr_never_starts_a_fresh_engage() {
    let (dir, path, pool) = fresh_db().await;
    seed_repo(&pool, &dir).await;
    let cfg = test_config(dir.path());
    seed_quiet_finding(&pool, 5).await;
    let session = seed_session_in(&dir, 20, CTX);
    std::fs::remove_dir_all(cfg.work_root.join("jobs").join("20").join("tree")).unwrap();
    seed_finding_suspension(&pool, 20, "engage", 5, &session).await;
    let store = rw_store(&path).await;

    let previewed = preview_next(&store, &cfg).await.unwrap();
    let picked = pick_next(&support::scope(&store), &cfg, None)
        .await
        .unwrap();

    for c in [&previewed, &picked] {
        assert!(
            !matches!(
                c,
                Some(Candidate::Finding { .. } | Candidate::Resume { .. })
            ),
            "{c:?}"
        );
    }
    assert_eq!(checkpoint_of(&store, 5, FindingJobKind::Engage).await, None);
}
