#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Rotation fairness: what an unsuccessful rotation-scan attempt does to
//! `last_<kind>_at`.
//!
//! `run_cycle_inner` bumps that timestamp when a rotation scan's attempt
//! — hunt or analysis — did not succeed, so a scan that fails forever
//! stops being the perpetual "oldest" pick and everything else gets a
//! turn. The condition that decides "ran to an end" is
//! `attempt_ended`, and the cases below are its edges: a kill, a
//! failure and a partly invalid ingest are failed turns and must bump, a
//! suspension is a pause and must not.
//!
//! Driven through `run_cycle` rather than the executor, because the bump
//! lives in the cycle and not in the runners — calling an executor
//! directly would assert on nothing at all.

mod support;

use std::path::PathBuf;

use hunter::config::Config;
use hunter::domain::{JobState, RepoJobKind};
use hunter::scheduler::{Candidate, pick_next, run_cycle};
use hunter::store::Store;
use hunter::types::RunResult;
use sqlx::SqlitePool;
use support::{GitRepo, ScriptedBackend, TempDir};

/// Where `test_gap`'s rotation timestamp starts: long ago, and the
/// stalest of the eligible kinds, so rotation picks it.
const STALE_TEST_GAP: i64 = 1_000;
/// `dep_update`'s: stale enough to be eligible, newer than `test_gap`,
/// so it is the kind that gets the turn once `test_gap` has had one.
const STALE_DEP_UPDATE: i64 = 3_000;
const STALE_REFACTOR: i64 = 4_000;
const STALE_HUNT: i64 = 5_000;

/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// the seed pool and the `Store` opened on `path`, and SQLite's handles
/// are closed before the files go.
async fn fixture(label: &str) -> (TempDir, PathBuf, SqlitePool, Config) {
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

    (dir, path, pool, cfg)
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

/// A killed analysis attempt bumps its rotation timestamp.
///
/// The turn was spent: something ran, and it is over. Leaving the
/// timestamp at its old value would make this kind the stalest pick
/// again next cycle, and the cycle after that — a kind that fails
/// reliably would hold the rotation slot forever while its siblings
/// never ran again.
#[tokio::test]
async fn a_killed_analysis_attempt_bumps_the_rotation_timestamp() {
    let (_dir, path, _pool, cfg) = fixture("rotation-killed").await;
    let store = Store::connect(&path).await.unwrap();
    // An unmetered kill is never a suspension, transcript or not, so this
    // is the plain killed outcome.
    let backend = stopped_worker("unmetered", true);

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(|k| k.to_string()),
        Some("test_gap".to_owned())
    );
    assert_eq!(summary.state, Some(JobState::Killed), "{summary:?}");
    assert!(
        last_test_gap_at(&store).await > STALE_TEST_GAP,
        "a killed attempt must still bump last_test_gap_at, or this kind \
         keeps winning the oldest-first rotation forever"
    );

    // And the turn really did move on: nothing but the timestamp decides
    // who is picked next.
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::DepUpdate,
                ..
            })
        ),
        "after test_gap's turn the next-stalest kind must be selected, got {picked:?}"
    );
}

/// A suspended analysis attempt does NOT bump its rotation timestamp.
///
/// A cap kill with a transcript is a pause, and re-selecting that work
/// belongs to the resume tier, which claims the job by id at a priority
/// above rotation. Bumping here would record a turn as taken while the
/// work had not started — and the job would then be offered by the
/// resume tier and skipped by rotation at the same time.
#[tokio::test]
async fn a_suspended_analysis_attempt_does_not_bump_the_rotation_timestamp() {
    let (_dir, path, _pool, cfg) = fixture("rotation-suspended").await;
    let store = Store::connect(&path).await.unwrap();
    // A cap kill that left a transcript is what the harness reports when
    // the window ran out of headroom mid-run.
    let backend = stopped_worker("cap", true);

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(
        summary.kind.map(|k| k.to_string()),
        Some("test_gap".to_owned())
    );
    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    assert_eq!(
        last_test_gap_at(&store).await,
        STALE_TEST_GAP,
        "a suspension must leave last_test_gap_at alone: the scan was paused \
         before it produced anything, so bumping it records a turn that was \
         never taken. The resume tier owns re-selecting this work, so a bump \
         would have the same job offered as a resume AND passed over by \
         rotation, and the repo's record would claim a scan ran when none did"
    );

    // The other half of the same contract: the work is not lost by being
    // left out of the bump, it is claimed one tier higher.
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
/// suspended, no rotation bump, and the resume tier claims it next.
#[tokio::test]
async fn an_attempt_that_died_after_doing_work_is_suspended_for_resume() {
    let (_dir, path, _pool, cfg) = fixture("rotation-died").await;
    let store = Store::connect(&path).await.unwrap();

    let summary = run_cycle(&store, &cfg, &died_worker(105_327, true), None).await;

    assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
    assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
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
/// resume would only run into again.
#[tokio::test]
async fn an_attempt_that_died_before_doing_any_work_stays_failed() {
    let (_dir, path, _pool, cfg) = fixture("rotation-died-empty").await;
    let store = Store::connect(&path).await.unwrap();

    let summary = run_cycle(&store, &cfg, &died_worker(0, true), None).await;

    assert_eq!(summary.state, Some(JobState::Failed), "{summary:?}");
    assert!(last_test_gap_at(&store).await > STALE_TEST_GAP);
}

/// Whether selection has abandoned a chain at the give-up ceiling.
async fn given_up(pool: &SqlitePool) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM jobs WHERE killed_reason = 'give-up'")
        .fetch_one(pool)
        .await
        .unwrap()
        > 0
}

/// A chain the give-up ceiling abandons hands the rotation turn on.
///
/// A wallclock kill after metered work is a suspension, so no single
/// attempt of a runaway bumps the timestamp: each is a pause the resume
/// tier continues. The chain's end is where its turn is over. Left at its
/// old value there, the timestamp makes rotation start the same runaway
/// fresh in the very selection that abandoned it, one whole chain after
/// another, while every sibling kind waits.
#[tokio::test]
async fn an_abandoned_analysis_chain_bumps_the_rotation_timestamp() {
    let (_dir, path, pool, cfg) = fixture("rotation-given-up").await;
    let store = Store::connect(&path).await.unwrap();
    let backend = stopped_worker("wallclock", true);

    // The fresh attempt and every resume the chain is allowed.
    let mut attempts = 0;
    let picked = loop {
        let picked = pick_next(&store, &cfg, None).await.unwrap();
        if given_up(&pool).await {
            break picked;
        }
        assert!(attempts < 10, "the give-up ceiling never retired the chain");
        let summary = run_cycle(&store, &cfg, &backend, None).await;
        assert_eq!(
            summary.kind.map(|k| k.to_string()),
            Some("test_gap".to_owned())
        );
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        assert_eq!(last_test_gap_at(&store).await, STALE_TEST_GAP);
        attempts += 1;
    };

    assert!(
        last_test_gap_at(&store).await > STALE_TEST_GAP,
        "an abandoned chain must bump last_test_gap_at, or rotation restarts \
         the same runaway as the stalest pick, chain after chain"
    );
    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::DepUpdate,
                ..
            })
        ),
        "the selection that abandons test_gap's chain must pass the turn to \
         the next-stalest kind, got {picked:?}"
    );
}

/// A resumed analysis attempt that is killed bumps its rotation
/// timestamp, exactly like a fresh one.
///
/// The kill ends the chain: the killed row is not resumable, so the
/// resume tier has nothing left to claim and rotation decides the next
/// pick alone. Skipping the bump because the attempt was a resume rather
/// than a fresh start would hand `test_gap` the slot again.
#[tokio::test]
async fn a_killed_resume_of_an_analysis_chain_bumps_the_rotation_timestamp() {
    let (_dir, path, _pool, cfg) = fixture("rotation-resume-killed").await;
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

    let started = run_cycle(&store, &cfg, &backend, None).await;
    assert_eq!(started.state, Some(JobState::Suspended), "{started:?}");
    let resumed = run_cycle(&store, &cfg, &backend, None).await;
    assert_eq!(
        resumed.kind.map(|k| k.to_string()),
        Some("test_gap".to_owned())
    );
    assert_eq!(resumed.state, Some(JobState::Killed), "{resumed:?}");
    let runs = backend.runs();
    assert_eq!(runs.len(), 2);
    assert!(
        runs[1].resume_from.is_some(),
        "the second run must be the resume"
    );

    assert!(
        last_test_gap_at(&store).await > STALE_TEST_GAP,
        "a killed resume must bump last_test_gap_at like a killed fresh attempt"
    );
    let picked = pick_next(&store, &cfg, None).await.unwrap();
    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::DepUpdate,
                ..
            })
        ),
        "after test_gap's chain is killed the next-stalest kind must be \
         selected, got {picked:?}"
    );
}

/// A `test_gap` attempt that ingests some entries and rejects others has
/// not succeeded — the runner leaves its timestamp alone — so the cycle
/// bumps it. Without the bump the scan is re-run straight away.
#[tokio::test]
async fn a_partly_invalid_analysis_attempt_bumps_the_rotation_timestamp() {
    let (_dir, path, _pool, cfg) = fixture("rotation-partly-invalid").await;
    let store = Store::connect(&path).await.unwrap();
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
                "fingerprint": "alpha:src/e.rs:m:test-gap", "type": "test_gap",
                "file": "src/e.rs", "severity": "medium", "confidence": 0.8,
                "summary": "untested", "missing_tests": ["boundary: empty"],
                "test_file": "tests/e.rs"
            },
            { "type": "test_gap" }
        ]);
        std::fs::write(
            out_dir.join(format!("job{origin}.test_gaps.json")),
            body.to_string(),
        )
        .unwrap();
        support::done()
    });

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    let ingest = summary.ingest.expect("the output was ingested");
    assert_eq!((ingest.inserted, ingest.invalid), (1, 1), "{ingest:?}");
    assert!(last_test_gap_at(&store).await > STALE_TEST_GAP);
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

/// After `alpha`'s hunt attempt, the turn has to pass to `beta`; and
/// `alpha`'s watermark must not move, so its retry reviews the same
/// commits.
async fn assert_turn_passed_to_beta(store: &Store, cfg: &Config) {
    let alpha = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert!(
        alpha.last_hunt_at.unwrap() > STALE_HUNT,
        "the attempt is recorded"
    );
    assert_eq!(alpha.last_hunt_sha, None, "but nothing is marked hunted");
    let picked = pick_next(store, cfg, None).await.unwrap();
    assert!(
        matches!(
            picked,
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                repo_id: 2,
                ..
            })
        ),
        "beta's hunt must get the next turn, got {picked:?}"
    );
}

/// A hunt whose worker fails is a turn taken. Left unrecorded, it would
/// be re-picked every cycle, holding the stalest slot forever and
/// starving every other repo's hunt.
#[tokio::test]
async fn a_failed_hunt_passes_the_turn_on() {
    let (_dir, store, cfg) = two_hunts_due("rotation-hunt-failed").await;
    let backend = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        ..support::done()
    });

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.repo.as_deref(), Some("alpha"));
    assert_eq!(summary.state, Some(JobState::Failed), "{summary:?}");
    assert_turn_passed_to_beta(&store, &cfg).await;
}

/// A hunt that finishes but files one invalid finding beside a valid one
/// does not advance its watermark. Left unrecorded, the attempt would be
/// re-run every few seconds.
#[tokio::test]
async fn a_partly_invalid_hunt_passes_the_turn_on() {
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

    let summary = run_cycle(&store, &cfg, &backend, None).await;

    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    let ingest = summary.ingest.expect("the output was ingested");
    assert_eq!((ingest.inserted, ingest.invalid), (1, 1), "{ingest:?}");
    assert_turn_passed_to_beta(&store, &cfg).await;
}

/// A hunt chain the give-up ceiling abandons hands the turn on like an
/// analysis chain does: otherwise the selection that abandons it starts
/// the same hunt fresh, chain after chain, and no other repo is hunted.
#[tokio::test]
async fn an_abandoned_hunt_chain_passes_the_turn_on() {
    let (_dir, store, cfg) = two_hunts_due("rotation-hunt-given-up").await;
    let backend = stopped_worker("wallclock", true);

    // Selection retires the chain at the ceiling and, in that same pick,
    // must move on to beta.
    let mut attempts = 0;
    while !matches!(
        pick_next(&store, &cfg, None).await.unwrap(),
        Some(Candidate::Repo { repo_id: 2, .. })
    ) {
        assert!(attempts < 10, "the give-up ceiling never retired the chain");
        let summary = run_cycle(&store, &cfg, &backend, None).await;
        assert_eq!(summary.repo.as_deref(), Some("alpha"), "{summary:?}");
        assert_eq!(summary.state, Some(JobState::Suspended), "{summary:?}");
        attempts += 1;
    }
    assert!(
        attempts > 1,
        "the chain was resumed before it was abandoned"
    );
    assert_turn_passed_to_beta(&store, &cfg).await;
}

/// A hunt whose fetch fails never gets as far as a job; the runner's
/// error still ends its turn, or every cycle would retry the fetch ahead
/// of every other repo's hunt.
#[tokio::test]
async fn a_hunt_whose_fetch_fails_passes_the_turn_on() {
    let (dir, store, cfg) = two_hunts_due("rotation-hunt-fetch").await;
    // `GitRepo` puts alpha's origin here; without it, the fetch fails.
    std::fs::remove_dir_all(dir.join("origin.git")).unwrap();

    let summary = run_cycle(&store, &cfg, &ScriptedBackend::noop(), None).await;

    assert!(summary.error.is_some(), "{summary:?}");
    assert_turn_passed_to_beta(&store, &cfg).await;
}
