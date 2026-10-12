#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Every reachable database state can roll forward to head.
//!
//! The bootstrap test covers state 0 (no file). It says nothing about an
//! installation sitting at any later state, and "the migration chain
//! works" is a claim about *all* of
//! them: a deployment that has been offline for two releases must be able
//! to catch up, and it must land on exactly the schema a fresh install
//! gets, or the two diverge permanently from that point on.
//!
//! So this is parametric over k: build a database that has applied
//! migrations 1..=k and nothing more, hand it to the real opener, and
//! require that it (a) succeeds, (b) ends at head, (c) ends with the same
//! schema as a fresh install, and (d) still has the rows it started with.
//!
//! (d) is not decoration. Migration 003 rebuilds `findings` by copying it
//! to a new table; a mistake there loses every finding on exactly the
//! upgrade path a fresh install never exercises.
//!
//! Partial states are built using sqlx's *own* migration set — its SQL and
//! its checksums, not a re-derivation — so the resume is the real one. A
//! hand-computed checksum would only prove this test agrees with itself.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingStatus, JobState};
use hunter::store::Store;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Executor, Row, SqlitePool};
use support::TempDir;

/// A scratch directory plus the database path inside it.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives
/// every pool and every `Store` opened on `path` and is removed only once
/// they have let the file go. Binding it after the pool, or discarding it
/// as `_`, would delete the directory out from under a live SQLite handle.
/// Removing the files by hand at the end of the test body was worse: a
/// failing assertion unwinds straight past it, leaking the database plus
/// its `-wal`/`-shm` sidecars into a tmpfs that other tests share.
fn scratch(label: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(label);
    let path = dir.join("hunter.db");
    (dir, path)
}

async fn open(path: &Path) -> SqlitePool {
    SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await
    .unwrap()
}

/// Tables and indexes, with their definitions — what "the same schema"
/// means. `_sqlx_migrations` is bookkeeping and varies by construction.
async fn schema(pool: &SqlitePool) -> Vec<(String, String)> {
    let rows = sqlx::query(
        "SELECT type || ':' || name AS k, COALESCE(sql, '') AS v FROM sqlite_master \
         WHERE name NOT LIKE 'sqlite_%' AND name <> '_sqlx_migrations' ORDER BY k",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    rows.iter()
        .map(|r| {
            (
                r.get::<String, _>("k"),
                r.get::<String, _>("v")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
        })
        .collect()
}

/// Apply migrations 1..=k exactly as sqlx would, recording each so the
/// real migrator resumes at k+1 instead of starting over.
async fn seed_state(path: &Path, k: usize) -> SqlitePool {
    let pool = open(path).await;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS _sqlx_migrations (
             version BIGINT PRIMARY KEY,
             description TEXT NOT NULL,
             installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
             success BOOLEAN NOT NULL,
             checksum BLOB NOT NULL,
             execution_time BIGINT NOT NULL
         )",
    )
    .await
    .unwrap();

    for m in sqlx::migrate!("./migrations").iter().take(k) {
        sqlx::raw_sql(m.sql.as_str())
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("seeding migration {}: {e}", m.version));
        sqlx::query(
            "INSERT INTO _sqlx_migrations \
             (version, description, success, checksum, execution_time) VALUES (?1,?2,1,?3,0)",
        )
        .bind(m.version)
        .bind(m.description.as_ref())
        .bind(m.checksum.as_ref())
        .execute(&pool)
        .await
        .unwrap();
    }
    pool
}

/// A repo and a finding, using only columns present since 001, so the same
/// seed works at every state.
async fn seed_rows(pool: &SqlitePool, fingerprint: &str) {
    sqlx::query(
        "INSERT INTO repos (name, url, path, forge, default_branch, added_at) \
         VALUES ('widget', 'git@github.com:acme/widget.git', '/tmp/widget', 'github', 'main', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO findings (type, repo_id, fingerprint, severity, confidence, summary, \
         status, created_at, updated_at) \
         VALUES ('bug', 1, ?1, 'high', 0.9, 'carried across the upgrade', 'new', 1, 1)",
    )
    .bind(fingerprint)
    .execute(pool)
    .await
    .unwrap();
}

fn migration_count() -> usize {
    sqlx::migrate!("./migrations").iter().count()
}

/// The head schema a fresh install produces — the thing every other state
/// has to agree with.
async fn fresh_head_schema() -> Vec<(String, String)> {
    let (_dir, path) = scratch("mig-head");
    let store = Store::connect(&path).await.expect("fresh install migrates");
    drop(store);
    let pool = open(&path).await;
    let s = schema(&pool).await;
    pool.close().await;
    s
}

#[tokio::test]
async fn every_partial_state_rolls_forward_to_the_same_head() {
    let head = fresh_head_schema().await;
    let n = migration_count();
    assert!(n >= 4, "expected the deployed chain, found {n} migrations");

    // k = 0 is the empty file; k = n is an installation already at head,
    // which must be a no-op rather than an error.
    for k in 0..=n {
        // A directory per state, scoped to the iteration: it is removed as
        // the loop advances instead of all n+1 surviving to the end. Every
        // pool below is closed before that happens — pulling the directory
        // out from under a live SQLite handle is the hazard.
        let (_dir, path) = scratch(&format!("mig-state{k}"));
        let pool = seed_state(&path, k).await;

        // From state 1 the tables exist, so the upgrade carries real rows.
        if k >= 1 {
            seed_rows(&pool, &format!("fp-state-{k}")).await;
        }
        pool.close().await;

        let store = Store::connect(&path)
            .await
            .unwrap_or_else(|e| panic!("state {k} could not roll forward: {e}"));
        drop(store);

        let pool = open(&path).await;

        let applied: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations ORDER BY version")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(applied.len(), n, "state {k} did not reach head");

        assert_eq!(
            schema(&pool).await,
            head,
            "state {k} converged on a different schema"
        );

        if k >= 1 {
            let kept: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM findings WHERE fingerprint = ?1")
                    .bind(format!("fp-state-{k}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(kept, 1, "state {k} lost its findings during the upgrade");
        }

        pool.close().await;
    }
}

/// Re-opening an up-to-date database changes nothing. Every restart of the
/// daemon runs the migrator, so this is the most frequently executed path
/// of all and the one where a non-idempotent migration would do its damage
/// repeatedly.
#[tokio::test]
async fn reopening_at_head_is_idempotent() {
    let (_dir, path) = scratch("mig-idem");
    let store = Store::connect(&path).await.unwrap();
    drop(store);
    let pool = open(&path).await;
    seed_rows(&pool, "fp-idem").await;
    let before = schema(&pool).await;
    pool.close().await;

    for _ in 0..3 {
        let store = Store::connect(&path).await.expect("reopen at head");
        drop(store);
    }

    let pool = open(&path).await;
    assert_eq!(schema(&pool).await, before, "reopening altered the schema");
    let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM findings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(kept, 1, "reopening altered the data");
    pool.close().await;
}

/// The migration chain leaves foreign key enforcement on.
///
/// `foreign_keys` is connection-scoped, sqlx sets it when it opens a
/// connection, and it does not reapply connection options when an idle
/// pooled connection is handed out again. Migration 009 runs outside a
/// transaction — the only way its table rebuild can work — so its
/// `PRAGMA foreign_keys = OFF` genuinely takes effect on whichever
/// connection runs it. Without 010 restoring it, that connection goes
/// back into the pool permissive and its next borrower can delete a repo
/// out from under its jobs, visible only later as a `JOIN repos` quietly
/// dropping rows.
///
/// Pinned on a single-connection pool on purpose. Going through `Store`
/// proves nothing: its pool holds several connections, so the query that
/// follows the migration usually lands on a different, freshly
/// configured one and the assertion passes whether or not the chain
/// restores the pragma. One connection makes the mechanism observable
/// rather than probabilistic. Seeded before 009 so the `OFF` executes.
#[tokio::test]
async fn the_migration_chain_leaves_foreign_keys_enforced() {
    let (_dir, path) = scratch("mig-fk");

    let seeded = seed_state(&path, 8).await;
    seed_rows(&seeded, "fp-fk").await;
    seeded.close().await;

    // One connection, configured the way Store configures its own, so the
    // migrator and the assertion below are guaranteed to share it.
    let opts = SqliteConnectOptions::new()
        .filename(&path)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();

    let on: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        on, 1,
        "the migration chain left foreign key enforcement off"
    );

    // And prove it bites, rather than trusting the pragma readout.
    let err = sqlx::query(
        "INSERT INTO findings (type, repo_id, fingerprint, severity, confidence, \
         summary, status, created_at, updated_at) \
         VALUES ('bug', 99999, 'fp-orphan', 'high', 0.9, 's', 'new', 1, 1)",
    )
    .execute(&pool)
    .await
    .expect_err("a finding pointing at no repo must be refused");
    assert!(
        err.to_string().to_lowercase().contains("foreign key"),
        "expected a foreign key violation, got: {err}"
    );

    pool.close().await;
}

/// Upgrading rewrites name-derived clone paths to id-derived ones.
///
/// Rows written before clone directories were keyed by id point at
/// `repos/<name>`. A database that kept the old paths would have the
/// daemon looking in the wrong place, and every repo re-cloning on its
/// next job.
///
/// Seeded at state 6 so that rolling forward actually executes 007.
#[tokio::test]
async fn upgrading_rewrites_name_derived_clone_paths() {
    let (_dir, path) = scratch("mig-paths");

    let pool = seed_state(&path, 6).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) VALUES \
         (1, 'widget', 'https://e/w.git', '/wr/repos/widget', 'github', 'main', 1), \
         (2, 'odd', 'https://e/o.git', '/somewhere/else', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let widget = store.get_repo_by_id(1).await.unwrap().unwrap();
    assert_eq!(
        widget.path, "/wr/repos/repo-1",
        "a name-derived path must become id-derived"
    );
    // A path that is not `<dir>/<name>` was put there by hand, and the
    // migration's WHERE guard leaves it alone: guessing at it would be
    // worse than leaving it visible.
    let odd = store.get_repo_by_id(2).await.unwrap().unwrap();
    assert_eq!(
        odd.path, "/somewhere/else",
        "an unrecognised path is left alone"
    );

    drop(store);
}

/// Upgrading hands the closures that predate the closed-PR harvest to it.
///
/// Every closure before that harvest set its finding `rejected`; the
/// harvest now selects `closed` alone, because a human's later `rejected`
/// looks the same. So 016 moves exactly the closure rejections — a
/// `rejected` finding whose PR is CLOSED and not yet harvested — and must
/// leave every other `rejected` alone: one already harvested was
/// classified, one with an open PR or no PR at all was a human's verdict.
/// The closure's reason stays: the harvest starts from it.
///
/// Seeded at the state just before 016 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_hands_pre_harvest_closures_to_the_closed_pr_harvest() {
    let (_dir, path) = scratch("mig-closed-await-harvest");

    let before_016 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 16)
        .expect("migration 016 exists");
    let pool = seed_state(&path, before_016).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // 1: closure, awaiting harvest. 2: closure, already harvested.
    // 3: rejected with its PR still open. 4: rejected, never had a PR.
    for (id, reason) in [
        (1, "PR closed without merge (closed by maintainer)"),
        (2, "PR closed without merge (harvested)"),
        (3, "wrong: the PR fixes nothing"),
        (4, "wrong: not a bug"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, verdict_reason, created_at, updated_at) \
             VALUES (?1, 'bug', 1, 'fp-' || ?1, 'high', 0.9, 's', 'rejected', ?2, 1, 1)",
        )
        .bind(id)
        .bind(reason)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, synced_at, harvested_at) VALUES \
         (1, 11, 'CLOSED', 1, NULL), (2, 12, 'CLOSED', 1, 5), (3, 13, 'OPEN', 1, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let mut got = Vec::new();
    for id in 1..=4 {
        let f = store.get_finding(id).await.unwrap().unwrap();
        got.push((id, f.status, f.verdict_reason.unwrap()));
    }
    assert_eq!(
        got,
        vec![
            (
                1,
                FindingStatus::Closed,
                "PR closed without merge (closed by maintainer)".to_owned()
            ),
            (
                2,
                FindingStatus::Rejected,
                "PR closed without merge (harvested)".to_owned()
            ),
            (
                3,
                FindingStatus::Rejected,
                "wrong: the PR fixes nothing".to_owned()
            ),
            (4, FindingStatus::Rejected, "wrong: not a bug".to_owned()),
        ],
        "only the unharvested closure moves to closed, keeping its reason"
    );

    let pending: Vec<i64> = store
        .list_pending_harvest()
        .await
        .unwrap()
        .iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(pending, vec![1], "the moved closure is due its harvest");

    drop(store);
}

/// Upgrading closes the job rows the Python daemon left `queued`.
///
/// `JobState` no longer has a `queued` variant, so a row still carrying it
/// would fail to decode wherever jobs are listed. 017 turns exactly those
/// rows into `failed` jobs finished at their start, with no tokens, and
/// leaves every other row as it was.
///
/// Seeded at the state just before 017 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_closes_jobs_left_queued() {
    let (_dir, path) = scratch("mig-queued-jobs");

    let before_017 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 17)
        .expect("migration 017 exists");
    let pool = seed_state(&path, before_017).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at, tokens_new) VALUES \
         (1, 'hunt', 1, 'queued', 1000, NULL, NULL), \
         (2, 'hunt', 1, 'done', 2000, 3000, 500)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let mut got: Vec<(i64, JobState)> = store
        .list_jobs(10)
        .await
        .expect("every job row decodes")
        .iter()
        .map(|j| (j.job.id, j.job.state))
        .collect();
    got.sort_unstable_by_key(|(id, _)| *id);
    assert_eq!(got, vec![(1, JobState::Failed), (2, JobState::Done)]);

    let pool = open(&path).await;
    let (finished_at, tokens_new, notes): (Option<i64>, Option<i64>, Option<String>) =
        sqlx::query_as("SELECT finished_at, tokens_new, notes FROM jobs WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(finished_at, Some(1000), "finished at its start");
    assert_eq!(tokens_new, None, "no spend is invented");
    assert!(notes.unwrap().contains("never started"));

    drop(store);
}

/// Upgrading sends the blocker reports that predate `blocked` to triage.
///
/// Until `blocked` existed, a worker's BLOCKED.md rejected its finding and
/// logged "#<id> blocked by worker: ...", which put valid work in the
/// suppression corpus. 021 moves exactly those rejections to `new`,
/// keeping the report as the reason. It leaves alone a rejection a human
/// confirmed after the report, a worker's decline, and a finding that has
/// left `rejected` since.
///
/// Seeded at the state just before 021 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_sends_pre_blocked_reports_to_triage() {
    let (_dir, path) = scratch("mig-pre-blocked");

    let before_021 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 21)
        .expect("migration 021 exists");
    let pool = seed_state(&path, before_021).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // 1: blocked report. 2: blocked report, then a human's verdict.
    // 3: a worker's decline. 4: blocked report, queued again since.
    for (id, status, reason) in [
        (1, "rejected", "# BLOCKED: needs Kotlin 2.2"),
        (2, "rejected", "duplicate of #9"),
        (3, "rejected", "# NOT-A-BUG: intended"),
        (4, "queued", "# BLOCKED: needs a rig"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, verdict_reason, created_at, updated_at) \
             VALUES (?1, 'bug', 1, 'fp-' || ?1, 'high', 0.9, 's', ?2, ?3, 1, 1)",
        )
        .bind(id)
        .bind(status)
        .bind(reason)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO events (id, at, kind, message, finding_id) VALUES \
         (1, 1, 'fix', '#1 blocked by worker: # BLOCKED: needs Kotlin 2.2', 1), \
         (2, 1, 'fix', '#2 blocked by worker: # BLOCKED: needs a rig', 2), \
         (3, 2, 'verdict', 'finding 2 [fp-2] -> rejected: duplicate of #9', 2), \
         (4, 1, 'fix', '#3 rejected by worker: # NOT-A-BUG: intended', 3), \
         (5, 1, 'fix', '#4 blocked by worker: # BLOCKED: needs a rig', 4)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let mut got = Vec::new();
    for id in 1..=4 {
        let f = store.get_finding(id).await.unwrap().unwrap();
        got.push((id, f.status, f.verdict_reason.unwrap()));
    }
    assert_eq!(
        got,
        vec![
            (
                1,
                FindingStatus::New,
                "# BLOCKED: needs Kotlin 2.2".to_owned()
            ),
            (2, FindingStatus::Rejected, "duplicate of #9".to_owned()),
            (
                3,
                FindingStatus::Rejected,
                "# NOT-A-BUG: intended".to_owned()
            ),
            (
                4,
                FindingStatus::Queued,
                "# BLOCKED: needs a rig".to_owned()
            ),
        ],
        "only the unreviewed blocker rejection moves to new, keeping its report"
    );

    drop(store);
}

/// Upgrading moves every failure streak onto its work item.
///
/// 023 copies the three streak column pairs (`fix`, `recheck` on the
/// finding, `harvest` on its `pr_state` row) into `finding_work`, one row
/// per (finding, kind) that had a streak, and drops the columns. A finding
/// with no streak gets no row, which reads as no streak.
///
/// Seeded at the state just before 023 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_moves_failure_streaks_onto_their_work_items() {
    use hunter::domain::FindingJobKind;
    use hunter::store::WorkStreak;

    let (_dir, path) = scratch("mig-finding-work");

    let before_023 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 23)
        .expect("migration 023 exists");
    let pool = seed_state(&path, before_023).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // 1: a fix streak and a recheck streak. 2: a harvest streak.
    // 3: no streak at all.
    for (id, status, fix, fix_key, recheck, recheck_key) in [
        (
            1,
            "queued",
            2,
            Some("no commits"),
            1,
            Some("no verdict file"),
        ),
        (2, "merged", 0, None, 0, None),
        (3, "new", 0, None, 0, None),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at, fix_attempts, last_fix_failure, \
             recheck_attempts, last_recheck_failure) \
             VALUES (?1, 'bug', 1, 'fp-' || ?1, 'high', 0.9, 's', ?2, 1, 1, ?3, ?4, ?5, ?6)",
        )
        .bind(id)
        .bind(status)
        .bind(fix)
        .bind(fix_key)
        .bind(recheck)
        .bind(recheck_key)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO pr_state (finding_id, pr_number, state, harvest_attempts, \
         last_harvest_failure) VALUES (2, 7, 'MERGED', 2, 'pr view failed'), \
         (3, 8, 'MERGED', 0, NULL)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let streak = |n: i64, key: &str| WorkStreak {
        streak: n,
        last_failure: Some(key.to_owned()),
    };
    let mut got = Vec::new();
    for id in 1..=3 {
        for kind in [
            FindingJobKind::Fix,
            FindingJobKind::Recheck,
            FindingJobKind::Harvest,
        ] {
            let s = store.work_streak(id, kind).await.unwrap();
            if s != WorkStreak::default() {
                got.push((id, kind, s));
            }
        }
    }
    assert_eq!(
        got,
        vec![
            (1, FindingJobKind::Fix, streak(2, "no commits")),
            (1, FindingJobKind::Recheck, streak(1, "no verdict file")),
            (2, FindingJobKind::Harvest, streak(2, "pr view failed")),
        ]
    );
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM finding_work")
        .fetch_one(&open(&path).await)
        .await
        .unwrap();
    assert_eq!(rows, 3, "no row for work that had no streak");

    drop(store);
}

/// Upgrading points every work item at the checkpoint it had.
///
/// Until 024 a finding's checkpoint was its newest `suspended` attempt
/// that nothing continued, and only while the finding was in that kind's
/// tier: the sweep retired the others. 024 turns exactly those into
/// `finding_work.checkpoint_job_id`, and points nothing at a suspension
/// outside its tier, one already continued, a repo-level one, or a
/// harvest whose PR is already harvested.
///
/// Seeded at the state just before 024 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_points_each_work_item_at_its_checkpoint() {
    use hunter::domain::FindingJobKind;

    let (_dir, path) = scratch("mig-checkpoints");

    let before_024 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 24)
        .expect("migration 024 exists");
    let pool = seed_state(&path, before_024).await;
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    // 1: queued, a fix chain 10 -> 11. 2: rejected, a recheck left behind.
    // 3: merged and harvested. 4: pr_open, an engage. 5: blocked, its held fix.
    for (id, status) in [
        (1, "queued"),
        (2, "rejected"),
        (3, "merged"),
        (4, "pr_open"),
        (5, "blocked"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at) \
             VALUES (?1, 'bug', 1, 'fp-' || ?1, 'high', 0.9, 's', ?2, 1, 1)",
        )
        .bind(id)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO pr_state (finding_id, pr_number, state, harvested_at) \
             VALUES (3, 7, 'MERGED', 5000); \
         INSERT INTO jobs (id, kind, repo_id, finding_id, state, session_file, resumed_from, \
                           started_at) VALUES \
             (10, 'fix', 1, 1, 'suspended', '/s/10.jsonl', NULL, 1), \
             (11, 'fix', 1, 1, 'suspended', '/s/10.jsonl', 10, 2), \
             (12, 'recheck', 1, 2, 'suspended', '/s/12.jsonl', NULL, 3), \
             (13, 'harvest', 1, 3, 'suspended', '/s/13.jsonl', NULL, 4), \
             (14, 'engage', 1, 4, 'suspended', '/s/14.jsonl', NULL, 5), \
             (15, 'fix', 1, 5, 'suspended', '/s/15.jsonl', NULL, 6), \
             (16, 'hunt', 1, NULL, 'suspended', '/s/16.jsonl', NULL, 7), \
             (17, 'engage', 1, 4, 'done', '/s/17.jsonl', NULL, 4); \
         UPDATE jobs SET blocker = 'needs a rig' WHERE id = 15;",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let mut got = Vec::new();
    for id in 1..=5 {
        for kind in FindingJobKind::ALL {
            if let Some(job) = store.checkpoint_job(id, kind).await.unwrap() {
                got.push((id, kind, job.id));
            }
        }
    }
    assert_eq!(
        got,
        vec![
            (1, FindingJobKind::Fix, 11),
            (4, FindingJobKind::Engage, 14),
            (5, FindingJobKind::Fix, 15),
        ]
    );

    drop(store);
}

/// Before 025: finding 1 blocked on a worker's BLOCKED.md, finding 2 on a
/// failure streak whose tree was never made, finding 3 an ordinary queued
/// fix.
async fn seed_blocked_fixes(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
         VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1)",
    )
    .execute(pool)
    .await
    .unwrap();
    // 1: a worker's BLOCKED.md. 2: a failure streak, its tree never made.
    // 3: an ordinary queued fix. 4: blocked, but no attempt carries a
    // report, only a suspended one 024 pointed at.
    for (id, status) in [
        (1, "blocked"),
        (2, "blocked"),
        (3, "queued"),
        (4, "blocked"),
    ] {
        sqlx::query(
            "INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, \
             summary, status, created_at, updated_at) \
             VALUES (?1, 'bug', 1, 'fp-' || ?1, 'high', 0.9, 's', ?2, 1, 1)",
        )
        .bind(id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state, session_file, blocker, \
                           started_at, finished_at) VALUES \
             (10, 'fix', 1, 1, 'suspended', '/s/10.jsonl', 'Needs Kotlin 2.2', 1, 5000), \
             (20, 'fix', 1, 2, 'suspended', NULL, \
              'stuck: 3 consecutive fix attempts hit the same failure: workspace not created\n\ngit: fatal', \
              2, 6000), \
             (40, 'fix', 1, 4, 'suspended', '/s/40.jsonl', NULL, 3, 7000); \
         INSERT INTO finding_work (finding_id, kind, checkpoint_job_id) VALUES \
             (1, 'fix', 10), (2, 'fix', 20), (4, 'fix', 40);",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Upgrading turns every `blocked` fix into a hold (WORK-MODEL.md §11.4).
///
/// The held job's report is the hold's detail, and its prefix says why: a
/// failure streak's report is `stuck`, anything else a worker's
/// `prerequisite`. The held job becomes the checkpoint only if a worker ran
/// in it (it has a transcript); a hold whose tree was never made has
/// nothing to continue. The finding goes back to `queued`, and `blocked`
/// and `jobs.blocker` are gone.
///
/// Seeded at the state just before 025 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_turns_blocked_fixes_into_holds() {
    use hunter::domain::{FindingJobKind, HoldReason};

    let (_dir, path) = scratch("mig-holds");

    let before_025 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 25)
        .expect("migration 025 exists");
    let pool = seed_state(&path, before_025).await;
    seed_blocked_fixes(&pool).await;
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let holds: Vec<_> = store
        .active_holds()
        .await
        .unwrap()
        .into_iter()
        .map(|h| {
            (
                h.finding_id,
                h.kind,
                h.reason,
                h.held_at,
                h.checkpoint_job_id,
                h.status,
            )
        })
        .collect();
    assert_eq!(
        holds,
        vec![
            (
                4,
                FindingJobKind::Fix,
                HoldReason::Prerequisite,
                1,
                Some(40),
                FindingStatus::Queued
            ),
            (
                1,
                FindingJobKind::Fix,
                HoldReason::Prerequisite,
                5000,
                Some(10),
                FindingStatus::Queued
            ),
            (
                2,
                FindingJobKind::Fix,
                HoldReason::Stuck,
                6000,
                None,
                FindingStatus::Queued
            ),
        ]
    );
    let details: Vec<String> = store
        .active_holds()
        .await
        .unwrap()
        .into_iter()
        .map(|h| h.detail)
        .collect();
    assert_eq!(details[1], "Needs Kotlin 2.2");
    assert!(
        details[2].starts_with("stuck: 3 consecutive"),
        "{details:?}"
    );
    assert_eq!(
        details[0], "blocked before the upgrade; no report was kept",
        "a hold says why it holds, even with no report to show"
    );
    assert_eq!(
        store.get_finding(3).await.unwrap().unwrap().status,
        FindingStatus::Queued
    );
    drop(store);
    let pool = open(&path).await;
    let blocker: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = 'blocker'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(blocker, 0, "jobs.blocker is dropped");
    pool.close().await;
}

/// Upgrading points every repo work item at the checkpoint it had.
///
/// Until 026 a repo-level job's checkpoint was its newest `suspended`
/// attempt that nothing continued; rows the give-up ceiling, a gone tree or
/// fresh work retired were rewritten out of `suspended`. 026 turns exactly
/// the live ones into `repo_work.checkpoint_job_id`, per (repo, kind), and
/// points nothing at a retired row, a continued link or a finding's job.
///
/// Seeded at the state just before 026 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_points_each_repo_work_item_at_its_checkpoint() {
    use hunter::domain::RepoJobKind;

    let (_dir, path) = scratch("mig-repo-checkpoints");

    let before_026 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 26)
        .expect("migration 026 exists");
    let pool = seed_state(&path, before_026).await;
    sqlx::raw_sql(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) VALUES \
             (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1), \
             (2, 'gadget', 'https://e/g.git', '/wr/repos/repo-2', 'github', 'main', 1); \
         INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
                               status, created_at, updated_at) \
             VALUES (1, 'bug', 1, 'fp-1', 'high', 0.9, 's', 'queued', 1, 1); \
         INSERT INTO jobs (id, kind, repo_id, finding_id, state, killed_reason, session_file, \
                           resumed_from, started_at) VALUES \
             (10, 'hunt', 1, NULL, 'suspended', 'cap', '/s/10.jsonl', NULL, 1), \
             (11, 'hunt', 1, NULL, 'suspended', 'cap', '/s/10.jsonl', 10, 2), \
             (12, 'test_gap', 1, NULL, 'suspended', 'cap', '/s/12.jsonl', NULL, 3), \
             (13, 'test_gap', 1, NULL, 'suspended', 'cap', '/s/13.jsonl', NULL, 4), \
             (14, 'refactor', 1, NULL, 'failed', 'give-up', '/s/14.jsonl', NULL, 5), \
             (15, 'standards', 2, NULL, 'suspended', 'wallclock', '/s/15.jsonl', NULL, 6), \
             (16, 'fix', 1, 1, 'suspended', 'cap', '/s/16.jsonl', NULL, 7), \
             (17, 'modernization', 2, NULL, 'killed', 'superseded', '/s/17.jsonl', NULL, 8);",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let mut got = Vec::new();
    for repo in 1..=2 {
        for kind in RepoJobKind::ALL {
            if let Some(job) = store.repo_checkpoint_job(repo, kind).await.unwrap() {
                got.push((repo, kind, job.id));
            }
        }
    }
    assert_eq!(
        got,
        vec![
            (1, RepoJobKind::Hunt, 11),
            (1, RepoJobKind::TestGap, 13),
            (2, RepoJobKind::Standards, 15),
        ]
    );
    drop(store);
}

/// Upgrading gives `work_holds` its repo subject without touching a hold.
///
/// 027 rebuilds the table, so every finding hold, active or released,
/// keeps its id and every column; the triggers that release a finding's
/// hold when it leaves its tier still do; and the new rows it allows are
/// repo holds, one active per repo and kind.
///
/// Seeded at the state just before 027 so that rolling forward executes it.
#[tokio::test]
async fn upgrading_keeps_every_hold_and_adds_the_repo_subject() {
    use hunter::domain::{FindingJobKind, HoldReason, RepoWorkKind};

    let (_dir, path) = scratch("mig-repo-holds");

    let before_027 = sqlx::migrate!("./migrations")
        .iter()
        .position(|m| m.version == 27)
        .expect("migration 027 exists");
    let pool = seed_state(&path, before_027).await;
    sqlx::raw_sql(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, added_at) \
             VALUES (1, 'widget', 'https://e/w.git', '/wr/repos/repo-1', 'github', 'main', 1); \
         INSERT INTO findings (id, type, repo_id, fingerprint, severity, confidence, summary, \
                               status, created_at, updated_at) VALUES \
             (1, 'bug', 1, 'fp-1', 'high', 0.9, 's', 'queued', 1, 1), \
             (2, 'bug', 1, 'fp-2', 'high', 0.9, 's', 'rechecking', 1, 1); \
         INSERT INTO work_holds (id, finding_id, kind, reason, detail, held_at, released_at, \
                                 released_by, release_action) VALUES \
             (7, 1, 'fix', 'prerequisite', 'needs a rig', 100, NULL, NULL, NULL), \
             (8, 2, 'recheck', 'stuck', 'no verdict', 200, NULL, NULL, NULL), \
             (9, 1, 'fix', 'stuck', 'older', 50, 60, 'tester', 'restart');",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;

    let store = Store::connect(&path).await.expect("roll forward");

    let holds: Vec<_> = store
        .active_holds()
        .await
        .unwrap()
        .into_iter()
        .map(|h| (h.id, h.finding_id, h.kind, h.reason, h.detail, h.held_at))
        .collect();
    assert_eq!(
        holds,
        [
            (
                7,
                1,
                FindingJobKind::Fix,
                HoldReason::Prerequisite,
                "needs a rig".to_owned(),
                100
            ),
            (
                8,
                2,
                FindingJobKind::Recheck,
                HoldReason::Stuck,
                "no verdict".to_owned(),
                200
            ),
        ]
    );
    // The released row is kept as it was.
    drop(store);
    let pool = open(&path).await;
    let released: (i64, String, String) = sqlx::query_as(
        "SELECT released_at, released_by, release_action FROM work_holds WHERE id = 9",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(released, (60, "tester".to_owned(), "restart".to_owned()));
    pool.close().await;

    // The tier trigger still releases a hold whose finding moves on.
    let store = Store::connect(&path).await.unwrap();
    store
        .set_finding_status(2, FindingStatus::Rejected)
        .await
        .unwrap();
    assert_eq!(
        store
            .active_holds()
            .await
            .unwrap()
            .iter()
            .map(|h| h.id)
            .collect::<Vec<_>>(),
        [7]
    );

    // And a repo's work can now be held, once per kind.
    assert!(
        store
            .hold_repo_work(
                1,
                RepoWorkKind::Sync,
                HoldReason::Stuck,
                "fetch failed",
                None
            )
            .await
            .unwrap()
    );
    assert!(
        !store
            .hold_repo_work(1, RepoWorkKind::Sync, HoldReason::Stuck, "again", None)
            .await
            .unwrap()
    );
    drop(store);
}
