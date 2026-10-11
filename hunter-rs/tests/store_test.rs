#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Behavior tests for Store, run against a writable copy of dev.db
//! (schema-complete, zero rows). Each test seeds its own fixture copy,
//! closes the writer, then reopens through `Store::connect_read_only` —
//! exercising the same read-only path the server uses.

mod support;

use std::path::{Path, PathBuf};

use hunter::domain::{FindingStatus, FindingType};
use hunter::store::{FindingFilter, Store};
use sqlx::{Row, SqlitePool};
use support::TempDir;

/// Copy dev.db (WAL, checkpointed, no rows) into a scratch directory and
/// open a WRITABLE pool on the copy for fixture inserts.
///
/// The guard comes FIRST in the tuple so every caller binds it first:
/// locals drop in reverse declaration order, so the directory outlives the
/// pool and any `Store` later opened on `path`, and is removed only once
/// both have closed the file. Binding it after the `Store`, or discarding
/// it as `_`, deletes the database out from under whoever is still reading
/// it. Removing the files by hand at the end of the test body was worse: a
/// failing assertion unwinds straight past it, leaking the copy plus its
/// `-wal`/`-shm` sidecars into a tmpfs that other tests share.
async fn fresh_db() -> (TempDir, PathBuf, SqlitePool) {
    let dir = TempDir::new("store");
    let (path, pool) = support::fresh_pool(&dir, "hunter").await;
    (dir, path, pool)
}

/// Standard fixture set: one repo, findings across statuses/types/severities,
/// finished jobs, events, and a `pr_state` row for the `pr_open` finding.
async fn seed(pool: &SqlitePool) {
    sqlx::raw_sql(
        r"
        INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at)
        VALUES (1, 'alpha', 'https://example.com/alpha.git', '/tmp/alpha', 'github', 'main', 1, 1000);

        INSERT INTO findings
            (id, type, repo_id, fingerprint, severity, confidence, summary, status, created_at, updated_at)
        VALUES
            (1, 'bug',        1, 'fp-low',  'low',    0.9, 'low bug',    'new',     1000, 1000),
            (2, 'bug',        1, 'fp-med',  'medium', 0.8, 'medium bug', 'new',     1001, 1001),
            (3, 'test_gap',   1, 'fp-high', 'high',   0.7, 'high gap',   'pr_open', 1002, 1002),
            (4, 'dep_update', 1, 'fp-dep',  'medium', 0.6, 'dep bump',   'merged',  1003, 1003);

        INSERT INTO jobs
            (id, kind, repo_id, finding_id, state, tokens_new, calls, usage_delta, started_at, finished_at)
        VALUES
            (1, 'hunt', 1, NULL, 'done', 1500, 3, 0.01, 1000, 1100),
            (2, 'fix',  1, 3,    'done', 2500, 5, 0.02, 1200, 1300);

        INSERT INTO events (id, at, kind, message, job_id, finding_id)
        VALUES
            (1, 1000, 'cycle', 'cycle start', NULL, NULL),
            (2, 1001, 'fix',   'fixing',      2,    3);

        INSERT INTO pr_state (finding_id, pr_number, state, needs_attention)
        VALUES (3, 7, 'OPEN', 'review_comments');
        ",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// Close the writer and reopen the same file read-only through Store.
async fn open_store(pool: SqlitePool, path: &Path) -> Store {
    pool.close().await;
    Store::connect_read_only(path).await.unwrap()
}

#[tokio::test]
async fn status_counts_zero_fills_all_statuses() {
    let (_dir, path, pool) = fresh_db().await;
    seed(&pool).await;
    let store = open_store(pool, &path).await;

    let counts = store.status_counts().await.unwrap();
    // Every enum status present even when unobserved.
    assert_eq!(counts.len(), FindingStatus::ALL.len());
    for status in FindingStatus::ALL.map(hunter::domain::FindingStatus::as_str) {
        assert!(counts.contains_key(status), "missing status key {status}");
    }
    assert_eq!(counts["new"], 2);
    assert_eq!(counts["pr_open"], 1);
    assert_eq!(counts["merged"], 1);
    assert_eq!(counts["queued"], 0);
    assert_eq!(counts["rechecking"], 0);
    assert_eq!(counts["fixing"], 0);
    assert_eq!(counts["rejected"], 0);
    assert_eq!(counts["wontfix"], 0);
    assert_eq!(counts["note"], 0);
    // The two a closed PR passes through: a dashboard that drops them
    // hides every finding waiting on, or settled by, its harvest.
    assert_eq!(counts["closed"], 0);
    assert_eq!(counts["superseded"], 0);
}

#[tokio::test]
async fn list_findings_severity_and_combined_filters() {
    let (_dir, path, pool) = fresh_db().await;
    seed(&pool).await;
    let store = open_store(pool, &path).await;

    // No filters: everything, id DESC.
    let all = store
        .list_findings(&FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![4, 3, 2, 1]
    );
    assert_eq!(all[3].kind, FindingType::Bug); // `type` column lands in the `kind` field

    // severity=medium (rank 2): excludes low, keeps medium+high, id DESC.
    let medium_up = store
        .list_findings(&FindingFilter {
            min_severity_rank: Some(2),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        medium_up.iter().map(|f| f.id).collect::<Vec<_>>(),
        vec![4, 3, 2]
    );

    // All four filters AND together.
    let combined = store
        .list_findings(&FindingFilter {
            status: Some(hunter::domain::FindingStatus::New),
            repo_id: Some(1),
            kind: Some(FindingType::Bug),
            min_severity_rank: Some(2),
        })
        .await
        .unwrap();
    assert_eq!(combined.len(), 1);
    assert_eq!(combined[0].id, 2);
    assert_eq!(combined[0].severity, hunter::domain::Severity::Medium);

    // The old "bogus status matches nothing" test is gone: FindingFilter.status
    // is now Option<FindingStatus>, so invalid values are a compile error.
}

#[tokio::test]
async fn stats_totals_empty_db_preserves_sum_null_semantics() {
    let (_dir, path, pool) = fresh_db().await;
    // No seeding: zero job rows.
    let store = open_store(pool, &path).await;

    let totals = store.stats_totals().await.unwrap();
    assert_eq!(totals.jobs, 0);
    assert_eq!(totals.total_tokens, None);
    assert_eq!(totals.total_calls, None);
    assert_eq!(totals.total_usage_delta, None);
    assert_eq!(totals.done, None);
    assert_eq!(totals.denied, None);
}

#[tokio::test]
async fn current_job_populates_finding_keys_only_with_a_finding() {
    // Newest running job HAS a finding -> summary/fingerprint populated.
    let (_dir, path, pool) = fresh_db().await;
    seed(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state) \
         VALUES (10, 'engage', 1, 3, 'running')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let store = open_store(pool, &path).await;
    let job = store.current_job().await.unwrap().unwrap();
    assert_eq!(job.id, 10);
    assert_eq!(job.state, hunter::domain::JobState::Running);
    assert_eq!(job.repo_name, "alpha");
    assert_eq!(job.finding_summary.as_deref(), Some("high gap"));
    assert_eq!(job.finding_fingerprint.as_deref(), Some("fp-high"));

    // Newest running job has NO finding -> both keys None (serialized absent).
    let (_dir2, path2, pool2) = fresh_db().await;
    seed(&pool2).await;
    sqlx::raw_sql(
        "INSERT INTO jobs (id, kind, repo_id, finding_id, state) \
         VALUES (11, 'hunt', 1, NULL, 'running')",
    )
    .execute(&pool2)
    .await
    .unwrap();
    let store2 = open_store(pool2, &path2).await;
    let job2 = store2.current_job().await.unwrap().unwrap();
    assert_eq!(job2.id, 11);
    assert_eq!(job2.finding_id, None);
    assert_eq!(job2.finding_summary, None);
    assert_eq!(job2.finding_fingerprint, None);
}

#[test]
fn repo_notes_missing_short_and_truncated() {
    let dir = TempDir::new("notes");
    let work_root: &Path = dir.path();
    // Built by the production path helper rather than by hand: notes have
    // moved out of the clone once already.
    let notes_file = Store::notes_path(work_root, 1);
    std::fs::create_dir_all(notes_file.parent().unwrap()).unwrap();

    // Missing file -> "".
    assert_eq!(Store::repo_notes(work_root, 2), "");

    // Short file passes through untouched.
    std::fs::write(&notes_file, "short note").unwrap();
    assert_eq!(Store::repo_notes(work_root, 1), "short note");

    // >4000 chars -> prefix + exactly the tail 4000 chars.
    let content = "abcde".repeat(1000); // 5000 chars
    std::fs::write(&notes_file, &content).unwrap();
    let notes = Store::repo_notes(work_root, 1);
    let prefix = "...(older notes truncated)...\n";
    assert!(notes.starts_with(prefix));
    let tail = &notes[prefix.len()..];
    assert_eq!(tail.chars().count(), 4000);
    assert_eq!(tail, &content[1000..]);
}

/// The SQL of `Store::{name}`, read out of `src/store.rs` itself.
///
/// `sqlx::query_scalar!` needs a string literal, so the two ledger
/// queries cannot be lifted into a `const` that both the daemon and a
/// test could use. Copying them into the test instead produces a test
/// of the copy: the previous version of this one planned its own SQL
/// and stayed green while the predicate it exists to protect was
/// deleted from the real query.
///
/// Reading the literal back out of the source is the link that has no
/// copy. Anchoring on `async fn <name>(` and the first raw string after
/// it survives rustfmt, comment rewrites and renamed bindings — the
/// query text itself is inside a raw literal, which no formatter
/// touches. Renaming the function or changing the literal's delimiter
/// does break it, and both panic here by name rather than passing.
fn production_sql(name: &str) -> String {
    const SOURCE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/store.rs"));
    let after_fn = SOURCE
        .split_once(&format!("async fn {name}("))
        .unwrap_or_else(|| panic!("src/store.rs has no `async fn {name}(`"))
        .1;
    let from_literal = after_fn
        .split_once("r#\"")
        .unwrap_or_else(|| panic!("no raw-string SQL after `async fn {name}(`"))
        .1;
    from_literal
        .split_once("\"#")
        .unwrap_or_else(|| panic!("unterminated raw-string SQL in `{name}`"))
        .0
        .to_owned()
}

/// The ledger's window sums must be able to use `jobs_finished_at`.
///
/// The index is partial (`finished_at IS NOT NULL AND tokens_new IS NOT
/// NULL`), and SQLite will only use a partial index when the query's
/// WHERE clause implies the index predicate. `SUM` ignores NULLs, so
/// `AND tokens_new IS NOT NULL` looks like a redundant line that a later
/// cleanup would happily delete — and deleting it silently turns both of
/// these back into full scans of every job the daemon has ever run, on
/// the endpoint the UI polls every five seconds. Nothing else would fail.
///
/// So assert the plan, not the result: these two queries must SEARCH
/// using the index, never SCAN. The queries are the daemon's own, taken
/// from `src/store.rs` by `production_sql`.
#[tokio::test]
async fn ledger_window_sums_use_the_finished_at_index() {
    let (_dir, _path, pool) = fresh_db().await;

    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/a.git', '/tmp/a', 'github', 'main', 1, 1000)",
    )
    .execute(&pool)
    .await
    .unwrap();

    // A plan is only meaningful once the planner has rows to reason about;
    // with an empty table SQLite may pick a scan regardless.
    for i in 0..500i64 {
        sqlx::query("INSERT INTO jobs (repo_id, kind, state, finished_at, tokens_new) VALUES (1, 'hunt', 'done', ?1, ?2)")
            .bind(1_000_000 + i * 1_000)
            // Half the rows have no token count, which is what makes the
            // partial index worth having.
            .bind(if i % 2 == 0 { Some(i * 7) } else { None })
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("ANALYZE").execute(&pool).await.unwrap();

    for name in ["finished_since", "finished_between"] {
        let sql = production_sql(name);
        // Bound, not substituted: the planner sees the statement the
        // daemon prepares, and a literal in place of a parameter is a
        // different statement to plan.
        let mut explain = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN QUERY PLAN {sql}")));
        for _ in 0..sql.matches('?').count() {
            explain = explain.bind(1_000_000i64);
        }
        let plan: Vec<String> = explain
            .fetch_all(&pool)
            .await
            .unwrap()
            .iter()
            .map(|r| r.get::<String, _>("detail"))
            .collect();
        let plan = plan.join(" | ");
        assert!(
            plan.contains("USING INDEX jobs_finished_at"),
            "{name} should use the partial index, planned as: {plan}\nSQL: {sql}"
        );
    }
}

// -- kind_token_history -------------------------------------------------------

/// One enabled repo and nothing else: the shared `seed` fixture carries
/// finished hunt/fix jobs of its own, which would sit in the middle of the
/// history these tests are counting.
async fn seed_bare_repo(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO repos (id, name, url, path, forge, default_branch, enabled, added_at) \
         VALUES (1, 'alpha', 'https://example.com/a.git', '/tmp/a', 'github', 'main', 1, 1000)",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// A run of hunt jobs that all ended in `state`, one per `tokens` entry.
async fn seed_hunt_jobs(pool: &SqlitePool, state: &str, tokens: &[i64]) {
    for &t in tokens {
        sqlx::query(
            "INSERT INTO jobs (kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES ('hunt', 1, ?1, ?2, 1000, 2000)",
        )
        .bind(state)
        .bind(t)
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A killed job's `tokens_new` is not a measurement of what the work costs
/// — it is whatever bound killed it. Killed rows therefore pile up at the
/// cap, and feeding them to the percentile makes the estimator measure its
/// own failures instead of the job.
#[tokio::test]
async fn kind_token_history_excludes_killed_jobs() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    seed_hunt_jobs(&pool, "done", &[1_000, 2_000, 3_000, 4_000, 5_000]).await;
    seed_hunt_jobs(&pool, "killed", &[200_000; 5]).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();
    assert_eq!(history, vec![1_000, 2_000, 3_000, 4_000, 5_000]);
}

/// Two completed samples are not a distribution, and filtering to none at
/// all would estimate 0 — which makes the budget gate reserve nothing and
/// wave through work it cannot fund. Below the floor the unfiltered history
/// is the better of the two bad options, so the killed rows come back.
#[tokio::test]
async fn kind_token_history_falls_back_below_three_completed() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    seed_hunt_jobs(&pool, "done", &[1_000, 2_000]).await;
    seed_hunt_jobs(&pool, "killed", &[200_000; 4]).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();
    assert_eq!(
        history,
        vec![1_000, 2_000, 200_000, 200_000, 200_000, 200_000]
    );
}

/// The boundary itself: three completed samples is the smallest count for
/// which a percentile index picks anything other than an endpoint, so it is
/// the first count that filters.
#[tokio::test]
async fn kind_token_history_filters_at_exactly_three_completed() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    seed_hunt_jobs(&pool, "done", &[1_000, 2_000, 3_000]).await;
    seed_hunt_jobs(&pool, "killed", &[200_000; 4]).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();
    assert_eq!(history, vec![1_000, 2_000, 3_000]);
}

/// The estimate is built from the most recent completed jobs only, so a
/// mis-metered era cannot hold it down forever.
///
/// The five cheap rows here are what the live table actually holds: 130
/// of 189 completed hunts recorded under 3,000 tokens, from before the
/// metering repair, when a single worker call writes a ~37,000-token
/// prompt cache. Those rows are wrong, cannot be recomputed, and will
/// never leave the table — an estimator that reads all history is
/// hostage to them, and to every repo that has since changed size. They
/// dragged the hunt p50 to 1,876 against 43,901 over the recent window,
/// and an estimate that small makes the cap kill every hunt it funds.
///
/// Asserted as the whole window rather than a percentile: what the
/// percentile picks is `anticipated_tokens`' business, and the property
/// this query owns is which rows it is allowed to pick from.
#[tokio::test]
async fn kind_token_history_reads_only_the_most_recent_completed_jobs() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    // Oldest first: ids ascend with insertion, so these five are the
    // ones the window must drop.
    seed_hunt_jobs(&pool, "done", &[1, 2, 3, 4, 5]).await;
    let recent: Vec<i64> = (0..20).map(|i| 50_000 + i * 1_000).collect();
    seed_hunt_jobs(&pool, "done", &recent).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();

    assert_eq!(
        history, recent,
        "exactly the 20 newest completed hunts, ascending for the percentile index"
    );
}

/// One hunt attempt that stopped at the cap, and the attempt that
/// continued it to `done`.
async fn seed_resumed_hunt_chain(pool: &SqlitePool, suspended: i64, done: i64) {
    let first = sqlx::query(
        "INSERT INTO jobs (kind, repo_id, state, tokens_new, killed_reason, started_at, \
         finished_at) VALUES ('hunt', 1, 'suspended', ?1, 'cap', 1000, 2000)",
    )
    .bind(suspended)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO jobs (kind, repo_id, state, tokens_new, resumed_from, started_at, \
         finished_at) VALUES ('hunt', 1, 'done', ?1, ?2, 3000, 4000)",
    )
    .bind(done)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();
}

/// A chain that had to be continued counts once, at what the whole chain
/// cost — not at what its last attempt added.
///
/// A resumed attempt meters only its own delta: the harness subtracts the
/// transcript's pre-spawn baseline, so the row that reaches `done` records
/// the tail of the work and the attempts that paid for the rest stay
/// `suspended`, which this query never reads. Live numbers, from engage
/// 4375: 38,193 recorded against a chain that cost 142,256, the other
/// 104,063 on its suspended predecessor 4374. Left uncorrected the
/// estimate is dragged down by exactly the jobs that were too big to
/// finish in one go — the ones a cold reservation, a resume's own
/// reservation and the give-up ceiling all have to be sized for.
#[tokio::test]
async fn kind_token_history_counts_a_resumed_chain_at_its_full_cost() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    seed_hunt_jobs(&pool, "done", &[7_000, 50_000]).await;
    seed_resumed_hunt_chain(&pool, 104_063, 38_193).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();

    assert_eq!(
        history,
        vec![7_000, 50_000, 142_256],
        "the chain contributes 104_063 + 38_193 once, not 38_193"
    );
}

/// A job that was never resumed is its own whole chain, so it still
/// contributes exactly its own `tokens_new` — and a suspension that no
/// completed job continues contributes nothing at all.
///
/// The chain walk must not reach sideways: it follows `resumed_from` from
/// a `done` row and nowhere else, so an unrelated suspended attempt is
/// neither a sample of its own (its `tokens_new` is the bound that stopped
/// it) nor part of anyone else's total.
#[tokio::test]
async fn kind_token_history_counts_an_unresumed_job_at_its_own_cost() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    seed_hunt_jobs(&pool, "done", &[10_000, 20_000, 30_000]).await;
    seed_hunt_jobs(&pool, "suspended", &[200_000]).await;
    let store = open_store(pool, &path).await;

    let history = store.kind_token_history("hunt").await.unwrap();

    assert_eq!(
        history,
        vec![10_000, 20_000, 30_000],
        "each completed job alone, with the orphan suspension nowhere in it"
    );
}

/// A withdrawal hands off into the closed PR's harvest as the next
/// attempt of the engage's chain, so the harvest is `resumed_from` an
/// engage. The engage's cost is engage work: counted into the harvest's
/// chain it would bring a harvest to its give-up ceiling on its first
/// resume, and counted into the harvest history it would inflate every
/// harvest's reservation. The tree is the one thing shared across the
/// kinds, so the workspace origin still reaches the engage.
#[tokio::test]
async fn a_handoff_harvest_is_accounted_apart_from_the_engage_it_continues() {
    let (_dir, path, pool) = fresh_db().await;
    seed_bare_repo(&pool).await;
    for t in [10_000, 20_000, 30_000] {
        sqlx::query(
            "INSERT INTO jobs (kind, repo_id, state, tokens_new, started_at, finished_at) \
             VALUES ('harvest', 1, 'done', ?1, 1000, 2000)",
        )
        .bind(t)
        .execute(&pool)
        .await
        .unwrap();
    }
    let insert = |kind: &'static str, state: &'static str, tokens: i64, from: Option<i64>| {
        let pool = pool.clone();
        async move {
            sqlx::query(
                "INSERT INTO jobs (kind, repo_id, state, tokens_new, resumed_from, started_at, \
                 finished_at) VALUES (?1, 1, ?2, ?3, ?4, 3000, 4000)",
            )
            .bind(kind)
            .bind(state)
            .bind(tokens)
            .bind(from)
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid()
        }
    };
    let engage = insert("engage", "done", 500_000, None).await;
    let handoff = insert("harvest", "suspended", 40_000, Some(engage)).await;
    let resumed = insert("harvest", "done", 5_000, Some(handoff)).await;
    let store = open_store(pool, &path).await;

    let chain = store.resume_chain_stats(handoff, None).await.unwrap();
    assert_eq!(
        (chain.total, chain.attempts, chain.max_single),
        (45_000, 2, 40_000),
        "only the harvest's own attempts count toward its give-up ceiling"
    );
    let history = store.kind_token_history("harvest").await.unwrap();
    assert_eq!(
        history,
        vec![10_000, 20_000, 30_000, 45_000],
        "the handoff chain is a harvest sample without the engage's cost"
    );
    assert_eq!(
        store.resume_origin_job(resumed).await.unwrap(),
        engage,
        "the handoff harvest keeps working in the engage chain's workspace"
    );
}
