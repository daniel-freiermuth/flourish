#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Full re-hunts: a hunt that reviews the repo's complete history
//! (`<empty tree>..<tip>`) instead of the commits since its watermark,
//! driven through the scheduler against a real clone — periodic
//! (`hunt.rehuntDays`) and operator-requested (`POST /api/repo/rehunt`).
//!
//! A request jumps the rotation until a hunt chain has started after it,
//! widens the next cold hunt to the complete history, and is settled only
//! by a full-history chain that started after it was made.

mod support;

use std::path::{Path, PathBuf};

use hunter::config::Config;
use hunter::domain::{JobState, RepoJobKind};
use hunter::scheduler::{Attempt, Candidate, Resumed, pick_next, run_cycle, run_hunt};
use hunter::store::Store;
use hunter::types::{Repo, RunResult};
use hunter::util::now_ms;
use support::{GitRepo, ScriptedBackend, TempDir, done, git};

/// Git's empty tree: the base of a diff range covering every commit.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

const USAGE: &str = r#"{"message":{"role":"assistant","usage":{"input":1000,"output":100,"cacheRead":0,"cacheWrite":500}}}"#;

/// Where the repo's hunt history starts.
enum History {
    /// Hunted up to the tip just now, its last full hunt at the given time.
    HuntedToTip { last_full_hunt_at: i64 },
    /// Never hunted.
    Never,
}

struct Fixture {
    cfg: Config,
    store: Store,
    origin: PathBuf,
    /// Last, so the directory outlives the SQLite handles above.
    dir: TempDir,
}

/// Repo 1, cloned, every analysis scan run a moment ago so only a hunt
/// can be due.
async fn fixture(label: &str, history: History) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, "feature");
    let clone = dir.subdir("repos").join("repo-1");
    std::fs::rename(&repo.work, &clone).unwrap();
    let head = git(&clone, &["rev-parse", "origin/main"]).trim().to_owned();
    let (path, pool) = support::fresh_pool(&dir, "rehunt").await;
    let now = now_ms();
    let (sha, hunt_at, full_at) = match history {
        History::HuntedToTip { last_full_hunt_at } => {
            (Some(head), Some(now), Some(last_full_hunt_at))
        }
        History::Never => (None, None, None),
    };
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at, \
         last_hunt_sha, last_hunt_at, last_full_hunt_at, last_test_gap_at, last_dep_update_at, \
         last_refactor_at, last_modernization_at, last_standards_at) \
         VALUES (1, 'alpha', ?1, ?2, 'github', 'main', 1, 1000, ?3, ?4, ?5, ?6, ?6, ?6, ?6, ?6)",
    )
    .bind(repo.origin.to_string_lossy().to_string())
    .bind(clone.to_string_lossy().to_string())
    .bind(sha)
    .bind(hunt_at)
    .bind(full_at)
    .bind(now)
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    let store = Store::connect(&path).await.unwrap();
    std::fs::write(
        dir.subdir("playbooks").join("hunt.md"),
        "hunt {{REPO_PATH}} {{DIFF_RANGE}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        store,
        origin: repo.origin,
        dir,
    }
}

impl Fixture {
    async fn repo(&self) -> Repo {
        self.store.get_repo_by_id(1).await.unwrap().unwrap()
    }

    /// The current tip of origin/main.
    fn tip(&self) -> String {
        git(&self.origin, &["rev-parse", "main"]).trim().to_owned()
    }

    /// Push `n` more commits to origin/main.
    fn push_commits(&self, n: usize) {
        let pusher = self.dir.join("pusher");
        if !pusher.exists() {
            git(
                self.dir.path(),
                &[
                    "clone",
                    self.origin.to_string_lossy().as_ref(),
                    pusher.to_string_lossy().as_ref(),
                ],
            );
        }
        for i in 0..n {
            std::fs::write(pusher.join("LATER.md"), format!("later {i}\n")).unwrap();
            git(&pusher, &["add", "-A"]);
            git(&pusher, &["commit", "-m", &format!("later {i}")]);
        }
        git(&pusher, &["push", "origin", "main"]);
    }

    /// A worker that finishes cleanly with no findings, writing the
    /// (empty) findings file its chain's playbook named.
    fn clean_worker(&self) -> ScriptedBackend {
        let out_dir = self.cfg.work_root.join("out");
        ScriptedBackend::new(move |tree| {
            std::fs::write(findings_path(&out_dir, tree), "[]").unwrap();
            done()
        })
    }

    /// The suspended chain the resume tier offers next.
    async fn resume_plan(&self) -> hunter::scheduler::ResumePlan {
        match pick_next(&support::scope(&self.store), &self.cfg, None)
            .await
            .unwrap()
        {
            Some(Candidate::Resume { plan, .. }) => *plan,
            other => panic!("expected the suspended hunt to be resumed, got {other:?}"),
        }
    }

    /// Whether the next pick is the repo's hunt, started cold.
    async fn hunt_is_picked(&self) -> bool {
        matches!(
            pick_next(&support::scope(&self.store), &self.cfg, None)
                .await
                .unwrap(),
            Some(Candidate::Repo {
                kind: RepoJobKind::Hunt,
                ..
            })
        )
    }
}

/// `<work_root>/out/job<origin>.findings.json` for the chain whose tree is
/// `<work_root>/jobs/<origin>/tree`.
fn findings_path(out_dir: &Path, tree: &Path) -> PathBuf {
    let origin = tree
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    out_dir.join(format!("job{origin}.findings.json"))
}

/// A worker stopped at its cap, transcript in the chain's session dir.
fn suspended_at_cap(tree: &Path) -> RunResult {
    let file = tree.parent().unwrap().join("session").join("session.jsonl");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, format!("{USAGE}\n")).unwrap();
    RunResult {
        exit_code: None,
        killed_reason: Some("cap".to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(file.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// So a request and a job start never share a millisecond.
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

// -- periodic ----------------------------------------------------------------

/// A periodic full re-hunt that suspends and is resumed is still a full
/// re-hunt when it finishes: it records `last_full_hunt_at`, so the next
/// cold hunt goes back to incremental instead of starting the whole
/// history over.
#[tokio::test]
async fn a_resumed_periodic_rehunt_records_its_full_hunt() {
    // Long past the 90-day re-hunt interval.
    let f = fixture(
        "rehunt-resumed",
        History::HuntedToTip {
            last_full_hunt_at: 1_000,
        },
    )
    .await;
    let head = f.tip();
    let row = f.repo().await;

    let cold = ScriptedBackend::new(suspended_at_cap);
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &cold,
    )
    .await
    .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    assert_eq!(first.full_rehunt, Some(true));

    let plan = f.resume_plan().await;
    let resumed = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(
        resumed.full_rehunt,
        Some(true),
        "the resume kept full scope"
    );
    assert_eq!(
        resumed.diff_range.as_deref(),
        Some(format!("{EMPTY_TREE}..{head}").as_str())
    );

    let after = f.repo().await;
    assert!(
        after.last_full_hunt_at.unwrap() > 1_000,
        "a resumed full re-hunt records its full hunt"
    );
    assert_eq!(after.last_hunt_sha.as_deref(), Some(head.as_str()));

    // The next cold hunt is incremental again: nothing new, so skipped.
    let next = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&after),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(next.skipped.as_deref(), Some("no new commits"), "{next:?}");
}

// -- requested ---------------------------------------------------------------

/// The request jumps a rotation with nothing due, the hunt it starts
/// reviews the complete history although the repo was hunted up to its
/// tip, and finishing that hunt settles the request.
#[tokio::test]
async fn a_requested_rehunt_runs_now_over_the_complete_history() {
    let f = fixture(
        "rehunt-runs",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    let head = f.tip();
    assert!(
        pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
            .is_none(),
        "every scan is within its interval"
    );

    assert!(f.store.request_full_hunt(1).await.unwrap());
    assert!(
        f.hunt_is_picked().await,
        "the requested hunt is picked at once"
    );

    let before = f.repo().await;
    let backend = f.clean_worker();
    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&before),
        &backend,
    )
    .await
    .unwrap();
    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.full_rehunt, Some(true));
    assert_eq!(
        summary.diff_range.as_deref(),
        Some(format!("{EMPTY_TREE}..{head}").as_str()),
        "not skipped as 'no new commits', and not an incremental range"
    );
    assert!(backend.runs()[0].prompt.contains(EMPTY_TREE));
    assert_eq!(summary.head.as_deref(), Some(head.as_str()));

    let after = f.repo().await;
    assert_eq!(after.full_hunt_requested_at, None, "the request is settled");
    assert!(after.last_full_hunt_at > before.last_full_hunt_at);
    assert_eq!(after.last_hunt_sha.as_deref(), Some(head.as_str()));
    assert!(
        pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
            .is_none(),
        "a settled request leaves nothing due"
    );
}

/// A request made while a full chain runs asked for a pass that chain
/// cannot give: the chain does not settle it, and finishing does not cost
/// the request its place at the front of the queue.
#[tokio::test]
async fn a_request_made_during_a_full_chain_outlives_it() {
    let f = fixture(
        "rehunt-during",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    tick().await;
    let row = f.repo().await;
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &ScriptedBackend::new(suspended_at_cap),
    )
    .await
    .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");

    tick().await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    let again = f.repo().await.full_hunt_requested_at.unwrap();

    let plan = f.resume_plan().await;
    let resumed = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(
        resumed.full_rehunt,
        Some(true),
        "the resume kept full scope"
    );

    let after = f.repo().await;
    assert!(after.last_full_hunt_at > row.last_full_hunt_at);
    assert_eq!(
        after.full_hunt_requested_at,
        Some(again),
        "the request made mid-chain is still pending"
    );
    assert!(
        f.hunt_is_picked().await,
        "and still at the front of the queue"
    );
}

/// An incremental chain already running when the request lands neither
/// settles it nor, by finishing, pushes it back to the scan interval.
#[tokio::test]
async fn an_incremental_chain_neither_settles_nor_delays_a_request() {
    let f = fixture(
        "rehunt-incremental",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    // A commit since the watermark, so the hunt has work to review.
    f.push_commits(1);
    let row = f.repo().await;
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &ScriptedBackend::new(suspended_at_cap),
    )
    .await
    .unwrap();
    assert_eq!(first.full_rehunt, Some(false), "{first:?}");

    tick().await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    let plan = f.resume_plan().await;
    let resumed = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(resumed.full_rehunt, Some(false));
    assert!(
        f.repo().await.full_hunt_requested_at.is_some(),
        "the request waits for a full chain"
    );
    assert!(
        f.hunt_is_picked().await,
        "and the chain's finish did not push it back to the scan interval"
    );
}

/// A requested hunt that fails is counted on the hunt's work item: the
/// request stops jumping the queue — the hunt waits for its turn in the
/// rotation like any other failing hunt — but stays pending, so the repo's
/// next hunt is still a full one.
#[tokio::test]
async fn a_failed_requested_hunt_waits_for_the_next_hunt_turn() {
    let f = fixture(
        "rehunt-failed",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    tick().await;
    let failing = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        ..done()
    });
    let summary = run_cycle(&mut support::jobs(&f.store), &f.cfg, &failing, None).await;
    assert_eq!(summary.state, Some(JobState::Failed), "{summary:?}");

    assert!(
        pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
            .is_none(),
        "the hunt is not retried before its interval"
    );
    let row = f.repo().await;
    assert!(row.full_hunt_requested_at.is_some());

    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&row),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(summary.full_rehunt, Some(true), "{summary:?}");
}

/// A full re-hunt that does not run keeps the repo's watermark: the scope
/// is that hunt's alone, and only a hunt that succeeds moves the watermark.
/// It used to be cleared before the budget gate, so a denied full hunt
/// (or one that failed) lost it.
#[tokio::test]
async fn a_full_rehunt_that_is_denied_or_fails_keeps_the_watermark() {
    let f = fixture(
        "rehunt-keeps-watermark",
        History::HuntedToTip {
            last_full_hunt_at: 1_000,
        },
    )
    .await;
    let watermark = f.repo().await.last_hunt_sha;
    assert!(watermark.is_some());

    let denying = ScriptedBackend::new(|_| panic!("a denied hunt runs nothing")).denying_above(0);
    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&f.repo().await),
        &denying,
    )
    .await
    .unwrap();
    assert!(summary.denied.is_some(), "{summary:?}");
    assert_eq!(f.repo().await.last_hunt_sha, watermark, "denied");

    let failing = ScriptedBackend::new(|_| RunResult {
        exit_code: Some(1),
        ..done()
    });
    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&f.repo().await),
        &failing,
    )
    .await
    .unwrap();
    assert_eq!(summary.full_rehunt, Some(true), "{summary:?}");
    assert_eq!(summary.state, Some(JobState::Failed), "{summary:?}");
    assert_eq!(f.repo().await.last_hunt_sha, watermark, "failed");
}

/// A never-hunted repo's request widens its first hunt from the bounded
/// recent window to the complete history, and a suspension does not lose
/// that: the resumed chain records its full hunt and settles the request.
#[tokio::test]
async fn a_never_hunted_repo_keeps_a_requested_full_scope_across_a_resume() {
    let f = fixture("rehunt-first", History::Never).await;
    // More history than a first hunt takes (its last 30 commits).
    f.push_commits(31);
    assert!(f.store.request_full_hunt(1).await.unwrap());
    tick().await;
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&f.repo().await),
        &ScriptedBackend::new(suspended_at_cap),
    )
    .await
    .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    assert_eq!(first.full_rehunt, Some(true), "{first:?}");

    let plan = f.resume_plan().await;
    let resumed = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(
        resumed.full_rehunt,
        Some(true),
        "the resume kept full scope"
    );
    assert_eq!(
        f.repo().await.full_hunt_requested_at,
        None,
        "the request is settled"
    );
}

/// The same repo without a request takes the bounded first hunt — which
/// is what the test above has to be different from.
#[tokio::test]
async fn a_never_hunted_repo_without_a_request_takes_a_bounded_first_hunt() {
    let f = fixture("rehunt-first-bounded", History::Never).await;
    f.push_commits(31);
    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&f.repo().await),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.full_rehunt, Some(false), "{summary:?}");
}

/// A request that lands while an incremental hunt is being prepared — the
/// hunt read the repo row before it — is not that hunt's: it neither
/// widens it nor loses its place at the front of the queue to it.
#[tokio::test]
async fn a_request_made_while_an_incremental_hunt_is_prepared_stays_unanswered() {
    let f = fixture(
        "rehunt-setup-incremental",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    f.push_commits(1);
    // The row the hunt was picked with, read before the request.
    let picked = f.repo().await;
    assert!(f.store.request_full_hunt(1).await.unwrap());

    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&picked),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.full_rehunt, Some(false), "{summary:?}");
    assert!(f.repo().await.full_hunt_requested_at.is_some());
    assert!(
        f.hunt_is_picked().await,
        "the request is still at the front"
    );
}

/// A request that replaces the one a full hunt was prepared for survives
/// that hunt: the hunt settles only the request it read.
#[tokio::test]
async fn a_request_replaced_while_a_full_hunt_is_prepared_survives_it() {
    let f = fixture(
        "rehunt-setup-full",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    let picked = f.repo().await;
    tick().await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    let replaced = f.repo().await.full_hunt_requested_at;

    let summary = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&picked),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(summary.state, Some(JobState::Done), "{summary:?}");
    assert_eq!(summary.full_rehunt, Some(true), "{summary:?}");
    assert_eq!(f.repo().await.full_hunt_requested_at, replaced);
    assert!(
        f.hunt_is_picked().await,
        "the new request is still at the front"
    );
}

/// A requested hunt whose fetch fails has made its attempt: the request
/// stops jumping the queue, or a fetch that keeps failing would be
/// retried every cycle ahead of everything else, and stays pending.
#[tokio::test]
async fn a_requested_hunt_whose_fetch_fails_waits_for_the_next_hunt_turn() {
    let f = fixture(
        "rehunt-fetch-fails",
        History::HuntedToTip {
            last_full_hunt_at: now_ms(),
        },
    )
    .await;
    assert!(f.store.request_full_hunt(1).await.unwrap());
    std::fs::remove_dir_all(&f.origin).unwrap();

    let summary = run_cycle(
        &mut support::jobs(&f.store),
        &f.cfg,
        &ScriptedBackend::noop(),
        None,
    )
    .await;

    assert!(summary.error.is_some(), "{summary:?}");
    assert!(
        pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
            .is_none(),
        "the hunt is not retried before its interval"
    );
    assert!(f.repo().await.full_hunt_requested_at.is_some());
}

/// A never-hunted repo's bounded first hunt that suspends resumes as the
/// bounded hunt it was: a resume takes its scope from the chain, not from
/// the absence of a watermark.
#[tokio::test]
async fn a_resumed_bounded_first_hunt_stays_bounded() {
    let f = fixture("rehunt-first-resumed", History::Never).await;
    f.push_commits(31);
    let first = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&f.repo().await),
        &ScriptedBackend::new(suspended_at_cap),
    )
    .await
    .unwrap();
    assert_eq!(first.state, Some(JobState::Suspended), "{first:?}");
    assert_eq!(first.full_rehunt, Some(false), "{first:?}");

    let plan = f.resume_plan().await;
    let resumed = run_hunt(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &f.clean_worker(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.state, Some(JobState::Done), "{resumed:?}");
    assert_eq!(resumed.full_rehunt, Some(false), "{resumed:?}");
    assert_eq!(resumed.diff_range, first.diff_range);
}
