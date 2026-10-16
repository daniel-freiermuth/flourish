#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Rotation fairness: what an unsuccessful rotation-scan attempt does to
//! its work item and to `last_<kind>_at` (WORK-MODEL.md §17).
//!
//! A repo kind's timestamp moves only when its runner succeeds. A failure
//! leaves it alone and is counted on the `(repo, kind)` work item instead,
//! so the work is retried as soon as the rotation reaches it again, and
//! three identical failures hold it for the operator: the rotation skips a
//! held kind and the turn passes on. A suspension is a pause: neither a
//! failure nor a turn taken; the resume tier claims it.
//!
//! Driven through `run_cycle`, so the selection that follows each attempt
//! is the scheduler's own.

mod support;

use std::path::PathBuf;

use hunter::config::Config;
use hunter::domain::{HoldReason, JobState, ReleaseAction, RepoJobKind, RepoWorkKind};
use hunter::scheduler::{Candidate, pick_next, run_cycle};
use hunter::store::{RepoHold, Store};
use hunter::types::RunResult;
use support::{GitRepo, ScriptedBackend, TempDir};

/// Where `test_gap`'s rotation timestamp starts: long ago, and the
/// stalest of the eligible kinds, so rotation picks it.
const STALE_TEST_GAP: i64 = 1_000;
/// `dep_update`'s: stale enough to be eligible, newer than `test_gap`,
/// so it is the kind that gets the turn once `test_gap` is held.
const STALE_DEP_UPDATE: i64 = 3_000;
const STALE_REFACTOR: i64 = 4_000;
const STALE_HUNT: i64 = 5_000;

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the `Store` opened on `path`, and SQLite's handles are closed before the
/// files go.
async fn fixture(label: &str) -> (TempDir, PathBuf, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "rotation").await;

    // A real clone with a real origin: `run_analysis_job` syncs before it
    // spawns anything, and refuses a directory whose origin is not the
    // repo's URL.
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.path().join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();

    // modernization and standards are parked in the future so their much
    // longer intervals cannot make them eligible; hunt/dep_update/
    // refactor are eligible but newer than test_gap.
    let future = hunter::util::now_ms() + 999_999;
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, \
          last_hunt_at, last_test_gap_at, last_dep_update_at, last_refactor_at, \
          last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?5, ?6, ?7, ?7)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(STALE_HUNT)
    .bind(STALE_TEST_GAP)
    .bind(STALE_DEP_UPDATE)
    .bind(STALE_REFACTOR)
    .bind(future)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    // Hermetic stub: the scripted worker ignores the prompt, so all the
    // template has to do is render.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("test_gap.md"),
        "test gaps in {{REPO_NAME}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");

    (dir, path, cfg)
}

/// A worker that writes no output and stops for `reason`, leaving a
/// transcript in its chain's session directory (or not).
fn stopped_worker(reason: &'static str, leaves_session: bool) -> ScriptedBackend {
    ScriptedBackend::new(move |tree| {
        let session = leaves_session.then(|| {
            // The chain's `session/` sits beside its `tree/`.
            let file = tree.parent().unwrap().join("session").join("session.jsonl");
            std::fs::write(&file, "{}\n").unwrap();
            file.to_string_lossy().into_owned()
        });
        RunResult {
            exit_code: None,
            killed_reason: Some(reason.to_owned()),
            tokens_new: 30_000,
            calls: 4,
            session_file: session,
            duration_s: 1.0,
            stdout_tail: "stopped".to_owned(),
            usage_delta: None,
        }
    })
}

async fn last_test_gap_at(store: &Store) -> i64 {
    store
        .get_repo_by_id(1)
        .await
        .unwrap()
        .expect("repo 1")
        .last_test_gap_at
        .expect("seeded non-NULL")
}

/// Repo `rid`'s `kind` streak and the key it fails with.
async fn streak(store: &Store, rid: i64, kind: RepoWorkKind) -> (i64, Option<String>) {
    let s = store.repo_work_streak(rid, kind).await.unwrap();
    (s.streak, s.last_failure)
}

/// The active hold on repo `rid`'s `kind` work, if any.
async fn hold_of(store: &Store, rid: i64, kind: RepoWorkKind) -> Option<RepoHold> {
    store
        .active_repo_holds()
        .await
        .unwrap()
        .into_iter()
        .find(|h| h.repo_id == rid && h.kind == kind)
}

fn is_repo_pick(picked: Option<&Candidate>, repo: i64, want: RepoJobKind) -> bool {
    matches!(picked, Some(Candidate::Repo { kind, repo_id, .. }) if *kind == want && *repo_id == repo)
}

/// Whether `c` is repo 1's `test_gap`, fresh or resumed.
fn test_gap_pick(c: &Candidate) -> bool {
    c.repo_id() == 1 && c.job_kind().to_string() == "test_gap"
}

/// Run cycles while the selection picks what `wanted` accepts, and return
/// how many ran. Driven by the selection rather than by the hold, because
/// the selection that holds a chain given up at the ceiling goes on to
/// pick the next work in the same cycle.
async fn run_while(
    store: &Store,
    cfg: &Config,
    backend: &ScriptedBackend,
    wanted: impl Fn(&Candidate) -> bool,
) -> usize {
    let mut cycles = 0;
    while pick_next(store, cfg, None)
        .await
        .unwrap()
        .is_some_and(|c| wanted(&c))
    {
        assert!(cycles < 20, "the work was never held");
        run_cycle(&support::recovered(store), cfg, backend, None).await;
        cycles += 1;
    }
    cycles
}

/// Run `backend` for three cycles of `test_gap` that fail the same way:
/// the first two are retried at once, the third holds the work, and the
/// turn passes to the next-stalest kind. The timestamp never moves.
async fn three_failures_hold_test_gap(store: &Store, cfg: &Config, backend: &ScriptedBackend) {
    for attempt in 1..=3 {
        let picked = pick_next(store, cfg, None).await.unwrap();
        assert!(
            is_repo_pick(picked.as_ref(), 1, RepoJobKind::TestGap),
            "attempt {attempt}: a failing scan is retried at once, got {picked:?}"
        );
        let summary = run_cycle(&support::recovered(store), cfg, backend, None).await;
        assert_eq!(
            summary.kind.map(|k| k.to_string()),
            Some("test_gap".to_owned()),
            "{summary:?}"
        );
        let expected = if attempt < 3 { "retry" } else { "held" };
        assert_eq!(summary.outcome.as_deref(), Some(expected), "{summary:?}");
        assert_eq!(
            last_test_gap_at(store).await,
            STALE_TEST_GAP,
            "a failure must leave last_test_gap_at alone"
        );
    }
    let held = hold_of(store, 1, RepoWorkKind::TestGap)
        .await
        .expect("held after three identical failures");
    assert_eq!(held.reason, HoldReason::Stuck);
    assert_eq!(
        streak(store, 1, RepoWorkKind::TestGap).await,
        (0, None),
        "a hold ends the streak"
    );
    let picked = pick_next(store, cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 1, RepoJobKind::DepUpdate),
        "a held kind is skipped: the next-stalest kind gets the turn, got {picked:?}"
    );
}

/// A killed analysis attempt is a failure of its work (`worker killed`),
/// not a turn taken: retried at once, held after three.
#[tokio::test]
async fn a_killed_analysis_attempt_is_retried_then_held() {
    let (_dir, path, cfg) = fixture("rotation-killed").await;
    let store = Store::connect(&path).await.unwrap();
    // An unmetered kill is never a suspension, transcript or not, so this
    // is the plain killed outcome.
    let backend = stopped_worker("unmetered", true);

    let summary = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;
    assert_eq!(summary.state, Some(JobState::Killed), "{summary:?}");
    assert_eq!(
        streak(&store, 1, RepoWorkKind::TestGap).await,
        (1, Some("worker killed".to_owned()))
    );

    // Two more: the held work's checkpoint is the last attempt, which left
    // a transcript, so the operator may continue it.
    for _ in 0..2 {
        run_cycle(&support::recovered(&store), &cfg, &backend, None).await;
    }
    let held = hold_of(&store, 1, RepoWorkKind::TestGap).await.unwrap();
    assert_eq!(
        held.actions(),
        [ReleaseAction::Continue, ReleaseAction::Restart]
    );
    assert_eq!(
        hold_events(&store).await,
        ["alpha test_gap held (stuck): worker killed"],
        "the operator's log names the repo and the work"
    );
    assert!(
        is_repo_pick(
            pick_next(&store, &cfg, None).await.unwrap().as_ref(),
            1,
            RepoJobKind::DepUpdate
        ),
        "the turn passes on once test_gap is held"
    );
}

/// The `hold` events in the log, oldest first.
async fn hold_events(store: &Store) -> Vec<String> {
    let mut events: Vec<String> = store
        .recent_events(500)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "hold")
        .map(|e| e.message)
        .collect();
    events.reverse();
    events
}

/// A suspended analysis attempt is neither a failure nor a turn taken.
///
/// A cap kill with a transcript is a pause, and re-selecting that work
/// belongs to the resume tier, which claims it through the work item's
/// checkpoint at a priority above rotation.
#[tokio::test]
async fn a_suspended_analysis_attempt_is_a_pause() {
    let (_dir, path, cfg) = fixture("rotation-suspended").await;
    let store = Store::connect(&path).await.unwrap();
    // A cap kill that left a transcript is what the harness reports when
    // the window ran out of headroom mid-run.
    let backend = stopped_worker("cap", true);

    let summary = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;

    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
    assert_eq!(streak(&store, 1, RepoWorkKind::TestGap).await, (0, None));
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    let plan = match picked {
        Some(Candidate::Resume { plan, .. }) => plan,
        other => panic!("the resume tier must claim the suspension, got {other:?}"),
    };
    assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
    assert_eq!(plan.kind.to_string(), "test_gap");
}

/// A worker that exited unsuccessfully on its own, after doing work.
fn died_worker(tokens_new: i64, leaves_session: bool) -> ScriptedBackend {
    ScriptedBackend::new(move |tree| {
        let session = leaves_session.then(|| {
            let file = tree.parent().unwrap().join("session").join("session.jsonl");
            std::fs::write(&file, "{}\n").unwrap();
            file.to_string_lossy().into_owned()
        });
        RunResult {
            exit_code: Some(1),
            killed_reason: None,
            tokens_new,
            calls: 26,
            session_file: session,
            duration_s: 1.0,
            stdout_tail: "Working...\naborted".to_owned(),
            usage_delta: None,
        }
    })
}

/// A worker that died on its own after doing work -- job 4912, whose
/// stream died across a laptop suspend -- is a pause like a cap kill:
/// suspended, not counted, and the resume tier claims it next.
#[tokio::test]
async fn an_attempt_that_died_after_doing_work_is_suspended_for_resume() {
    let (_dir, path, cfg) = fixture("rotation-died").await;
    let store = Store::connect(&path).await.unwrap();

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &died_worker(105_327, true),
        None,
    )
    .await;

    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
    assert_eq!(streak(&store, 1, RepoWorkKind::TestGap).await, (0, None));
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    match picked {
        Some(Candidate::Resume { plan, .. }) => {
            assert_eq!(plan.predecessor_id, summary.job_id.unwrap());
        }
        other => panic!("the resume tier must claim the died attempt, got {other:?}"),
    }
}

/// The same exit with no metered work stays a failure: in the whole job
/// history that is the signature of a configuration error
/// (`model_not_supported`, a rate limit before the first answer), which a
/// resume would only run into again. Three of them hold the scan.
#[tokio::test]
async fn an_attempt_that_died_before_doing_any_work_is_counted() {
    let (_dir, path, cfg) = fixture("rotation-died-empty").await;
    let store = Store::connect(&path).await.unwrap();
    let backend = died_worker(0, true);

    three_failures_hold_test_gap(&store, &cfg, &backend).await;

    let held = hold_of(&store, 1, RepoWorkKind::TestGap).await.unwrap();
    assert!(held.detail.starts_with("worker failed"), "{held:?}");
}

/// A worker that finished without writing its output file did not do the
/// work: `no findings file`, counted like any failure.
#[tokio::test]
async fn a_scan_that_writes_nothing_is_counted() {
    let (_dir, path, cfg) = fixture("rotation-no-file").await;
    let store = Store::connect(&path).await.unwrap();

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;

    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert_eq!(
        streak(&store, 1, RepoWorkKind::TestGap).await,
        (1, Some("no findings file".to_owned()))
    );
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
}

/// Whether selection has abandoned a chain at the give-up ceiling: the
/// `resume` events that drop its checkpoint. The rows stay as they ended.
async fn given_up(store: &Store) -> usize {
    store
        .recent_events(500)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == "resume" && e.message.contains("giving up"))
        .count()
}

/// A chain the give-up ceiling abandons is a failure of its work
/// (`resume chain gave up`), where it used to bump the rotation timestamp.
///
/// A wallclock kill after metered work is a suspension, so every attempt of
/// a runaway is a pause the resume tier continues. Its end counts once, and
/// the same selection starts the scan fresh; the third chain in a row holds
/// it as `gave_up`, with the last chain as its checkpoint, and only then
/// does the turn pass on.
#[tokio::test]
async fn an_abandoned_analysis_chain_is_counted_and_held_on_the_third() {
    let (_dir, path, cfg) = fixture("rotation-given-up").await;
    let store = Store::connect(&path).await.unwrap();
    let backend = stopped_worker("wallclock", true);

    let attempts = run_while(&store, &cfg, &backend, test_gap_pick).await;

    // Three chains of four attempts each: the streak counts chains, never
    // their attempts.
    assert_eq!(attempts, 12);
    assert_eq!(given_up(&store).await, 3);
    assert_eq!(
        last_test_gap_at(&store).await,
        STALE_TEST_GAP,
        "no attempt and no give-up moves the timestamp"
    );
    let held = hold_of(&store, 1, RepoWorkKind::TestGap).await.unwrap();
    assert_eq!(held.reason, HoldReason::GaveUp);
    assert!(held.detail.starts_with("resume chain gave up"), "{held:?}");
    assert!(
        held.checkpoint_job_id.is_some(),
        "the chain is kept to continue"
    );
    assert!(
        is_repo_pick(
            pick_next(&store, &cfg, None).await.unwrap().as_ref(),
            1,
            RepoJobKind::DepUpdate
        ),
        "the turn passes on once test_gap is held"
    );
}

/// The operator continues a held scan: the first attempt resumes its chain
/// with a prompt that says why it was held, and the give-up ceiling counts
/// from the release, so the chain gets a fresh budget.
#[tokio::test]
async fn a_continued_scan_resumes_its_chain_past_the_ceiling() {
    let (_dir, path, cfg) = fixture("rotation-continue").await;
    let store = Store::connect(&path).await.unwrap();
    let backend = stopped_worker("wallclock", true);
    run_while(&store, &cfg, &backend, test_gap_pick).await;
    let checkpoint = hold_of(&store, 1, RepoWorkKind::TestGap)
        .await
        .unwrap()
        .checkpoint_job_id;

    let by = support::operator(&store).await;
    store
        .release_repo_hold(1, RepoWorkKind::TestGap, ReleaseAction::Continue, &by)
        .await
        .unwrap();

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    let plan = match picked {
        Some(Candidate::Resume { plan, .. }) => plan,
        other => panic!("a continued hold resumes its chain, got {other:?}"),
    };
    assert_eq!(Some(plan.predecessor_id), checkpoint);
    assert_eq!(
        plan.after_hold.as_ref().map(|(reason, _)| *reason),
        Some(HoldReason::GaveUp)
    );
}

/// A resumed analysis attempt that is killed fails its work like a fresh
/// one: the kill ends the chain, and the scan starts over fresh.
#[tokio::test]
async fn a_killed_resume_of_an_analysis_chain_is_counted() {
    let (_dir, path, cfg) = fixture("rotation-resume-killed").await;
    let store = Store::connect(&path).await.unwrap();
    // Suspends when started, is killed when continued.
    let backend = ScriptedBackend::staged(|tree, resume_from| {
        let fresh = resume_from.is_none();
        let session = fresh.then(|| {
            let file = tree.parent().unwrap().join("session").join("session.jsonl");
            std::fs::write(&file, "{}\n").unwrap();
            file.to_string_lossy().into_owned()
        });
        RunResult {
            exit_code: None,
            killed_reason: Some(if fresh { "cap" } else { "unmetered" }.to_owned()),
            tokens_new: 30_000,
            calls: 4,
            session_file: session,
            duration_s: 1.0,
            stdout_tail: "stopped".to_owned(),
            usage_delta: None,
        }
    });

    let started = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;
    assert_eq!(started.state, Some(JobState::Suspended), "{started:?}");
    let resumed = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;
    assert_eq!(resumed.state, Some(JobState::Killed), "{resumed:?}");
    let runs = backend.runs();
    assert_eq!(runs.len(), 2);
    assert!(
        runs[1].resume_from.is_some(),
        "the second run must be the resume"
    );

    assert_eq!(
        streak(&store, 1, RepoWorkKind::TestGap).await,
        (1, Some("worker killed".to_owned()))
    );
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
    assert!(
        is_repo_pick(
            pick_next(&store, &cfg, None).await.unwrap().as_ref(),
            1,
            RepoJobKind::TestGap
        ),
        "the chain is over, so the scan starts over fresh"
    );
}

/// A worker writing `body` (JSON) to its `test_gap` output path.
fn test_gap_output(cfg: &Config, body: serde_json::Value) -> ScriptedBackend {
    let out_dir = cfg.work_root.join("out");
    ScriptedBackend::new(move |tree| {
        let origin = tree
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        std::fs::write(
            out_dir.join(format!("job{origin}.test_gaps.json")),
            body.to_string(),
        )
        .unwrap();
        support::done()
    })
}

fn one_test_gap() -> serde_json::Value {
    serde_json::json!({
        "fingerprint": "alpha:src/e.rs:m:test-gap", "type": "test_gap",
        "file": "src/e.rs", "severity": "medium", "confidence": 0.8,
        "summary": "untested", "missing_tests": ["boundary: empty"],
        "test_file": "tests/e.rs"
    })
}

/// A `test_gap` attempt that ingests some entries and rejects others files
/// the valid ones, but has not succeeded: `invalid entries`, counted, and
/// the timestamp stays.
#[tokio::test]
async fn a_partly_invalid_analysis_attempt_files_its_valid_entries_and_is_counted() {
    let (_dir, path, cfg) = fixture("rotation-partly-invalid").await;
    let store = Store::connect(&path).await.unwrap();
    let backend = test_gap_output(
        &cfg,
        serde_json::json!([one_test_gap(), { "type": "test_gap" }]),
    );

    let summary = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;

    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    let ingest = summary.ingest.as_ref().expect("the output was ingested");
    assert_eq!((ingest.inserted, ingest.invalid), (1, 1), "{ingest:?}");
    assert_eq!(summary.outcome.as_deref(), Some("retry"));
    assert_eq!(
        streak(&store, 1, RepoWorkKind::TestGap).await,
        (1, Some("invalid entries".to_owned()))
    );
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
}

/// A clean scan is the work's outcome: the timestamp moves and the streak
/// a failure left ends.
#[tokio::test]
async fn a_clean_scan_moves_its_timestamp_and_ends_the_streak() {
    let (_dir, path, cfg) = fixture("rotation-success").await;
    let store = Store::connect(&path).await.unwrap();
    run_cycle(
        &support::recovered(&store),
        &cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;
    assert_eq!(streak(&store, 1, RepoWorkKind::TestGap).await.0, 1);

    let backend = test_gap_output(&cfg, serde_json::json!([one_test_gap()]));
    let summary = run_cycle(&support::recovered(&store), &cfg, &backend, None).await;

    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.outcome, None, "{summary:?}");
    assert!(last_test_gap_at(&store).await > STALE_TEST_GAP);
    assert_eq!(streak(&store, 1, RepoWorkKind::TestGap).await, (0, None));
}

/// A backend that cannot start the worker closes its job, which is not
/// left `running`, and is counted as `runner error`.
#[tokio::test]
async fn a_runner_error_closes_the_job_and_is_counted() {
    let (_dir, path, cfg) = fixture("rotation-runner-error").await;
    let store = Store::connect(&path).await.unwrap();

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &ScriptedBackend::failing(),
        None,
    )
    .await;

    assert!(summary.error.is_some(), "{summary:?}");
    let jobs = store.list_jobs(10).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job.state, JobState::Failed, "{:?}", jobs[0]);
    assert_eq!(
        streak(&store, 1, RepoWorkKind::TestGap).await,
        (1, Some("runner error".to_owned()))
    );
}

// -- hunts ---------------------------------------------------------------------

/// Two cloned repos whose only due scan is a hunt: `alpha` (repo 1, never
/// hunted to a watermark, the stalest) and `beta` (repo 2, due).
async fn two_hunts_due(label: &str) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "rotation-hunt").await;
    let alpha = GitRepo::with_branch(&dir, "feature");
    let alpha_clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&alpha.work, &alpha_clone).unwrap();
    // beta is never run in these tests; its clone only has to exist.
    let beta_clone = dir.subdir("repos").join("repo-2");
    let now = hunter::util::now_ms();
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, last_hunt_at, \
          last_test_gap_at, last_dep_update_at, last_refactor_at, last_modernization_at, \
          last_standards_at) VALUES \
         (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?4, ?6, ?6, ?6, ?6, ?6), \
         (2, 'beta', 'https://example.com/beta.git', ?3, 'github', 'main', 1, 1000, ?5, \
          ?6, ?6, ?6, ?6, ?6)",
    )
    .bind(alpha.origin.to_string_lossy().to_string())
    .bind(alpha_clone.to_string_lossy().to_string())
    .bind(beta_clone.to_string_lossy().to_string())
    .bind(STALE_HUNT)
    .bind(now - 2 * 86_400_000)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    std::fs::write(
        dir.subdir("playbooks").join("hunt.md"),
        "hunt {{REPO_PATH}} {{DIFF_RANGE}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

/// Run `backend` while the selection picks `alpha`, and check that ends
/// with `alpha`'s `kind` work held and the turn passed to `beta`.
/// `alpha`'s watermark and timestamp never move, so a retry reviews the
/// same commits.
async fn hold_alpha_then_beta(
    store: &Store,
    cfg: &Config,
    backend: &ScriptedBackend,
    kind: RepoWorkKind,
) -> RepoHold {
    run_while(store, cfg, backend, |c| c.repo_id() == 1).await;
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_eq!(alpha.last_hunt_at, Some(STALE_HUNT), "no failure moves it");
    assert_eq!(alpha.last_hunt_sha, None, "nothing is marked hunted");
    let picked = pick_next(store, cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 2, RepoJobKind::Hunt),
        "beta's hunt must get the next turn, got {picked:?}"
    );
    hold_of(store, 1, kind).await.unwrap()
}

/// A hunt whose worker fails is retried at once, not after the scan
/// interval, and held after three: then the turn passes on.
#[tokio::test]
async fn a_failing_hunt_is_held_and_passes_the_turn_on() {
    let (_dir, store, cfg) = two_hunts_due("rotation-hunt-failed").await;
    let backend = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        ..support::done()
    });

    let held = hold_alpha_then_beta(&store, &cfg, &backend, RepoWorkKind::Hunt).await;

    assert_eq!(backend.runs().len(), 3);
    assert!(held.detail.starts_with("worker failed"), "{held:?}");
}

/// A hunt that finishes but files one invalid finding beside a valid one
/// does not advance its watermark, and counts as a failure.
#[tokio::test]
async fn a_partly_invalid_hunt_is_held_and_passes_the_turn_on() {
    let (_dir, store, cfg) = two_hunts_due("rotation-hunt-invalid").await;
    let out_dir = cfg.work_root.join("out");
    let backend = ScriptedBackend::new(move |tree| {
        let origin = tree
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        let body = serde_json::json!([
            {
                "fingerprint": "alpha:README.md:seed:1", "type": "bug", "file": "README.md",
                "line": 1, "bug_class": "boundary", "severity": "high", "confidence": 0.9,
                "summary": "real", "detail": "d", "evidence_plan": "e"
            },
            { "type": "bug" }
        ]);
        std::fs::write(
            out_dir.join(format!("job{origin}.findings.json")),
            body.to_string(),
        )
        .unwrap();
        support::done()
    });

    let held = hold_alpha_then_beta(&store, &cfg, &backend, RepoWorkKind::Hunt).await;

    assert!(held.detail.contains("1 invalid entries"), "{held:?}");
}

/// A hunt chain the give-up ceiling abandons is counted like an analysis
/// chain: three in a row hold the hunt, and only then is another repo
/// hunted.
#[tokio::test]
async fn an_abandoned_hunt_chain_is_held_on_the_third() {
    let (_dir, store, cfg) = two_hunts_due("rotation-hunt-given-up").await;
    let backend = stopped_worker("wallclock", true);

    let held = hold_alpha_then_beta(&store, &cfg, &backend, RepoWorkKind::Hunt).await;

    assert_eq!(held.reason, HoldReason::GaveUp);
}

/// A hunt whose fetch fails never gets as far as a job. The failure is the
/// repo's, not the hunt's: it is counted on `(alpha, sync)`, and once that
/// is held no kind of `alpha` is picked, while `beta` still is.
#[tokio::test]
async fn a_repo_whose_fetch_fails_is_held_once_for_every_kind() {
    let (dir, store, cfg) = two_hunts_due("rotation-hunt-fetch").await;
    // `GitRepo` puts alpha's origin here; without it, the fetch fails.
    std::fs::remove_dir_all(dir.join("origin.git")).unwrap();

    let summary = run_cycle(
        &support::recovered(&store),
        &cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;
    assert!(summary.error.is_some(), "{summary:?}");
    assert_eq!(
        streak(&store, 1, RepoWorkKind::Sync).await,
        (1, Some("fetch failed".to_owned()))
    );
    assert_eq!(streak(&store, 1, RepoWorkKind::Hunt).await, (0, None));

    let held =
        hold_alpha_then_beta(&store, &cfg, &ScriptedBackend::noop(), RepoWorkKind::Sync).await;
    assert_eq!(held.actions(), [ReleaseAction::Restart]);
    let events = hold_events(&store).await;
    assert!(
        events.len() == 1
            && events[0].starts_with("alpha sync held (stuck): git fetch origin failed"),
        "{events:?}"
    );
    assert!(store.list_jobs(10).await.unwrap().is_empty());

    // Forced onto alpha, nothing of it is picked either.
    assert!(
        pick_next(&store, &cfg, Some("alpha"))
            .await
            .unwrap()
            .is_none()
    );

    // `restart` is "retry now": alpha is back in the rotation.
    let by = support::operator(&store).await;
    store
        .release_repo_hold(1, RepoWorkKind::Sync, ReleaseAction::Restart, &by)
        .await
        .unwrap();
    assert!(is_repo_pick(
        pick_next(&store, &cfg, None).await.unwrap().as_ref(),
        1,
        RepoJobKind::Hunt
    ));
}

/// A repo whose clone keeps failing is held on `sync`, and the "an uncloned
/// repo is always a hunt" branch skips it too: nothing of it is picked.
#[tokio::test]
async fn a_repo_whose_clone_keeps_failing_is_left_alone_once_held() {
    let dir = TempDir::new("rotation-clone");
    let (path, pool) = support::fresh_pool(&dir, "rotation-clone").await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000)",
    )
    .bind(dir.join("no-such-origin.git").to_string_lossy().to_string())
    .bind(
        dir.join("repos")
            .join("repo-1")
            .to_string_lossy()
            .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    let store = Store::connect(&path).await.unwrap();

    for _ in 0..3 {
        let summary = run_cycle(
            &support::recovered(&store),
            &cfg,
            &ScriptedBackend::noop(),
            None,
        )
        .await;
        assert!(summary.error.is_some(), "{summary:?}");
    }

    let held = hold_of(&store, 1, RepoWorkKind::Sync).await.unwrap();
    assert!(held.detail.starts_with("clone failed"), "{held:?}");
    assert!(pick_next(&store, &cfg, None).await.unwrap().is_none());
}

// -- what a hold takes out of the selection ---------------------------------

/// Two repos for selection only, no worker: `alpha` (repo 1) and `beta`
/// (repo 2), each a plain directory standing in for its clone. Only the
/// hunt of each is due; `beta`'s is the staler one, and `alpha`'s
/// `test_gap` has never run. With `alpha_cloned` false, `alpha` has no
/// clone and was never hunted, so it goes first.
async fn two_repos_for_selection(label: &str, alpha_cloned: bool) -> (TempDir, Store, Config) {
    let dir = TempDir::new(label);
    let (path, pool) = support::fresh_pool(&dir, "rotation-select").await;
    let alpha = dir.join("repos").join("repo-1");
    if alpha_cloned {
        std::fs::create_dir_all(&alpha).unwrap();
    }
    let beta = dir.subdir("repos").join("repo-2");
    std::fs::create_dir_all(&beta).unwrap();
    let now = hunter::util::now_ms();
    sqlx::query(
        "INSERT INTO repos \
         (id, name, url, path, forge, default_branch, enabled, added_at, last_hunt_at, \
          last_test_gap_at, last_dep_update_at, last_refactor_at, last_modernization_at, \
          last_standards_at) VALUES \
         (1, 'alpha', 'https://example.com/alpha.git', ?1, 'github', 'main', 1, 1000, ?3, \
          NULL, ?4, ?4, ?4, ?4), \
         (2, 'beta', 'https://example.com/beta.git', ?2, 'github', 'main', 1, 1000, 1000, \
          ?4, ?4, ?4, ?4, ?4)",
    )
    .bind(alpha.to_string_lossy().to_string())
    .bind(beta.to_string_lossy().to_string())
    .bind(alpha_cloned.then_some(now - 30 * 86_400_000))
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    let store = Store::connect(&path).await.unwrap();
    (dir, store, cfg)
}

async fn hold(store: &Store, rid: i64, kind: RepoWorkKind) {
    assert!(
        store
            .hold_repo_work(rid, kind, HoldReason::Stuck, "kept failing", None)
            .await
            .unwrap()
    );
}

/// An uncloned repo is always a hunt, unless that hunt is held: then
/// nothing of the repo can run, and the turn passes on.
#[tokio::test]
async fn an_uncloned_repo_whose_hunt_is_held_is_skipped() {
    let (_dir, store, cfg) = two_repos_for_selection("rotation-uncloned-held", false).await;
    assert!(is_repo_pick(
        pick_next(&store, &cfg, None).await.unwrap().as_ref(),
        1,
        RepoJobKind::Hunt
    ));

    hold(&store, 1, RepoWorkKind::Hunt).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 2, RepoJobKind::Hunt),
        "{picked:?}"
    );
}

/// A full re-hunt request is urgent only while its hunt can run. Held, it
/// neither starts the hunt nor puts the repo ahead of the rotation for its
/// other kinds; the request stays pending for the hunt's release.
#[tokio::test]
async fn a_held_hunt_takes_its_full_rehunt_request_out_of_the_queue() {
    let (_dir, store, cfg) = two_repos_for_selection("rotation-urgent-held", true).await;
    assert!(store.request_full_hunt(1).await.unwrap());
    assert!(
        is_repo_pick(
            pick_next(&store, &cfg, None).await.unwrap().as_ref(),
            1,
            RepoJobKind::Hunt
        ),
        "an urgent request goes first"
    );

    hold(&store, 1, RepoWorkKind::Hunt).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 2, RepoJobKind::Hunt),
        "beta's staler hunt goes first, not alpha's test_gap, got {picked:?}"
    );
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert!(alpha.full_hunt_requested_at.is_some(), "the request waits");
    assert_eq!(alpha.full_hunt_request_attempted, None);
}

/// Running is no different from held: while the requested hunt still
/// has a `running` row, the request does not put the repo's other kinds
/// ahead of every other repo.
#[tokio::test]
async fn a_running_hunt_takes_its_full_rehunt_request_out_of_the_queue() {
    let (dir, store, cfg) = two_repos_for_selection("rotation-urgent-running", true).await;
    assert!(store.request_full_hunt(1).await.unwrap());
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("rotation-select.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state, started_at) \
         VALUES (30, 'hunt', 1, NULL, 'running', 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 2, RepoJobKind::Hunt),
        "beta's staler hunt goes first, not alpha's test_gap, got {picked:?}"
    );
}

/// A held `sync` holds every kind of its repo, its paused chains included:
/// the resume tier skips a checkpoint whose own kind is not held, and the
/// checkpoint stays for the release.
#[tokio::test]
async fn a_held_sync_keeps_the_resume_tier_off_the_repos_chains() {
    let (dir, store, cfg) = two_repos_for_selection("rotation-sync-resume", true).await;
    // A resumable test_gap chain on alpha: its tree, its transcript, its
    // pinned commit and the work item's pointer.
    let ws = hunter::workspace::Workspace::for_chain(
        &cfg.work_root,
        &dir.join("repos").join("repo-1"),
        10,
    );
    std::fs::create_dir_all(&ws.tree).unwrap();
    std::fs::create_dir_all(&ws.session).unwrap();
    let session = ws.session.join("session.jsonl");
    std::fs::write(&session, "{}\n").unwrap();
    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new().filename(dir.join("rotation-select.db")),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, session_file, killed_reason, tokens_new, \
                           started_at, finished_at, pinned_sha) \
         VALUES (10, 'test_gap', 1, 'suspended', ?1, 'cap', 1000, 1, 2, 'pinned')",
    )
    .bind(session.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();
    support::point_repo_checkpoint(&pool, 1, "test_gap", Some(10)).await;
    pool.close().await;
    assert!(
        matches!(
            pick_next(&store, &cfg, None).await.unwrap(),
            Some(Candidate::Resume { .. })
        ),
        "unheld, the chain is resumed"
    );

    hold(&store, 1, RepoWorkKind::Sync).await;

    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        is_repo_pick(picked.as_ref(), 2, RepoJobKind::Hunt),
        "{picked:?}"
    );
    assert_eq!(
        store
            .repo_checkpoint_job(1, RepoJobKind::TestGap)
            .await
            .unwrap()
            .map(|j| j.id),
        Some(10),
        "skipped, not given up"
    );
}

/// A hunt with no new commits since its watermark is the hunt's outcome:
/// it ends a streak earlier failures left.
#[tokio::test]
async fn a_hunt_with_no_new_commits_ends_its_streak() {
    let (dir, store, cfg) = two_hunts_due("rotation-hunt-skip").await;
    let tip = support::git(
        &dir.join("repos").join("repo-1"),
        &["rev-parse", "origin/main"],
    )
    .trim()
    .to_owned();
    store.set_last_hunt(1, &tip).await.unwrap();
    store
        .record_repo_failure(1, RepoWorkKind::Hunt, "no findings file")
        .await
        .unwrap();
    let repo = store.get_repo_by_id(1).await.unwrap().unwrap();

    let summary = hunter::scheduler::run_hunt(
        &support::scope(&store),
        &cfg,
        &repo,
        &ScriptedBackend::noop(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        summary.skipped.as_deref(),
        Some("no new commits"),
        "{summary:?}"
    );
    assert_eq!(streak(&store, 1, RepoWorkKind::Hunt).await, (0, None));
}
