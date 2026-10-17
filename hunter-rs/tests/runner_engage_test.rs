#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `run_engage` — withdrawal, push/reply, retry and guard paths — and
//! what a PR closing does to its finding.
//!
//! This is the runner layer, which had no tests until a review found a P1
//! here: `close_pr` was changed to propagate a failed withdrawal comment,
//! but the caller still recorded the withdrawal as complete, leaving the
//! PR open on the forge, closed in the database, and the finding rejected
//! — and `sync_prs` only revisits `pr_open` findings, so nothing would
//! ever reconcile it.
//!
//! A closed PR, whether a worker withdrew it or a human closed it, leaves
//! its finding `closed` for the harvest to classify, never `rejected`:
//! 15 of the first 16 closures were engage withdrawals, 14 of them
//! "superseded/obsolete", and rejecting those suppressed valid findings.
//!
//! Worth reading as a template for the other runners: `support` gives you
//! a git repo with a real origin, a scripted `gh`, and a backend that
//! stages whatever the worker would have left in the worktree.

mod support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use hunter::attention::{
    AnswerRecorded, EngageAnswer, ObservedFixture, ObservedOpenPr, OpenPrRecorded,
};
use hunter::config::Config;
use hunter::domain::{BudgetOverride, FindingStatus, ForgeName, RepoJobKind};
use hunter::forge::Mergeable;
use hunter::scheduler::{Attempt, Resumed};
use hunter::store::FindingInsert;
use hunter::types::RunResult;
use support::{FakeBins, GitRepo, ScriptedBackend, TempDir, done, fresh_store, git};

const BRANCH: &str = "fix/some-bug";
const PR_URL: &str = "https://github.com/acme/widget/pull/7";
const REPO_URL: &str = "https://github.com/acme/widget";

/// The fixture PR's static problems, as sync fingerprints them: its review
/// asks for changes.
const FP: &str = "review:CHANGES_REQUESTED";

/// The fixture PR as `pass` read it, stamped `last_activity_at`, with
/// feedback newer than any engage (`last_activity_at` past the watermark
/// of 0) and changes requested ([`FP`]).
async fn observed(
    pass: &hunter::store::PrSyncPass,
    fid: i64,
    last_activity_at: i64,
) -> ObservedOpenPr<'_> {
    let read = pass
        .begin_read_at(fid, &support::pr_ref(PR_URL), last_activity_at)
        .await
        .unwrap()
        .expect("the pass's first read of the finding");
    ObservedOpenPr::fixture(
        read,
        ObservedFixture {
            mergeable: Mergeable::Mergeable,
            changes_requested: true,
            checks: None,
            failing_checks: Vec::new(),
            head_ref: BRANCH.to_owned(),
            head_sha: "deadbeef".to_owned(),
            last_activity_at,
        },
    )
}

/// Minimal `gh pr view --json` payload that `view_pr_engage` can parse.
const PR_VIEW_JSON: &str = r#"{"state":"OPEN","mergeable":"MERGEABLE","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// The same PR as `sync_prs` sees it once a human has closed it.
const PR_CLOSED_JSON: &str = r#"{"state":"CLOSED","mergeable":"UNKNOWN","reviewDecision":"","comments":[],"reviews":[],"statusCheckRollup":[],"updatedAt":"2026-01-02T03:04:05Z","headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#;

/// The closed PR's diff, as `gh pr diff` prints it.
const PR_DIFF: &str = "diff --git a/CHANGE.md b/CHANGE.md\n+change\n";

/// One usage record in omp's session format: a 100,000-token context, so
/// the handoff reserves 100,000 + max(0, 100,000) = 200,000 under the
/// resume rule, against engage's cold 40,000.
const USAGE: &str = r#"{"message":{"role":"assistant","usage":{"input":100000,"output":100,"cacheRead":0,"cacheWrite":0}}}"#;

/// One valid follow-up entry, so a follow-up that is not filed is known
/// to have been skipped rather than rejected by ingest.
const FOLLOW_UP: &str = r#"[{"type":"bug","fingerprint":"widget:src/lib.rs:left-open","file":"src/lib.rs","bug_class":"logic","severity":"medium","confidence":0.8,"summary":"left open","detail":"src/lib.rs:1 still has it","evidence_plan":"failing test"}]"#;

struct Fixture {
    db: std::path::PathBuf,
    cfg: Config,
    store: hunter::store::Store,
    fid: i64,
    /// Last, because fields drop in declaration order and the directory
    /// has to outlive the Store's SQLite pool -- the same rule `fresh_db`
    /// documents and `TestState` follows.
    ///
    /// Measured, so the comment does not overclaim: on Linux the old
    /// order leaks nothing, because unlinking a file an open handle
    /// still holds is legal and `remove_dir_all` succeeds. What the
    /// order buys here is that the pool is never writing into a
    /// directory that has already been removed, and one rule across
    /// every fixture instead of two -- not a leak fix on this platform.
    _dir: TempDir,
}

impl Fixture {
    /// `pr_state.addressed_fingerprint` — set when an engage cycle has
    /// taken its shot at the current attention reason.
    async fn addressed_fp(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT addressed_fingerprint FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.and_then(|r| r.0)
    }

    /// `pr_state.state` read straight from the database — the runner's
    /// own Store API exposes no reader for it, and the point of the test
    /// is what was persisted.
    async fn pr_state(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(String,)> =
            sqlx::query_as("SELECT state FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.map(|r| r.0)
    }

    /// `pr_state.needs_attention` as `sync_prs` persisted it.
    async fn attention(&self) -> Option<String> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT needs_attention FROM pr_state WHERE finding_id = ?1")
                .bind(self.fid)
                .fetch_optional(&pool)
                .await
                .unwrap();
        row.and_then(|r| r.0)
    }

    /// Every job row, oldest first: `(id, kind, state, resumed_from,
    /// estimated_tokens, pinned_sha)`.
    async fn jobs(&self) -> Vec<JobRow> {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query_as(
            "SELECT id, kind, state, resumed_from, estimated_tokens, pinned_sha \
             FROM jobs ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .unwrap()
    }

    /// `(state, killed_reason, notes)` of one job, straight from the
    /// database: what a retirement rewrote and what it kept.
    async fn job_row(&self, id: i64) -> (String, Option<String>, Option<String>) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query_as("SELECT state, killed_reason, notes FROM jobs WHERE id = ?1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
    }

    /// Run a statement against the fixture database, to put `pr_state`
    /// or the finding into a shape the store has no writer for.
    async fn exec(&self, sql: &'static str) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query(sql).execute(&pool).await.unwrap();
    }

    async fn pr_row(&self) -> hunter::store::RecordedPrState {
        self.store.get_pr_state(self.fid).await.unwrap().unwrap()
    }

    async fn events(&self, kind: &str) -> Vec<String> {
        self.store
            .recent_events(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.message)
            .collect()
    }

    async fn run(
        &self,
        backend: &ScriptedBackend,
    ) -> anyhow::Result<hunter::scheduler::CycleSummary> {
        let finding = self.store.get_finding(self.fid).await.unwrap().unwrap();
        hunter::scheduler::run_engage(
            &support::scope(&self.store),
            self.store.test_lane(),
            &self.cfg,
            Attempt::Cold(&finding),
            backend,
        )
        .await
    }
}

/// A backend that runs `stage` in the worktree and remembers where that
/// worktree was, so a test can check it was released afterwards.
fn recording(
    stage: impl Fn(&Path) + Send + Sync + 'static,
) -> (ScriptedBackend, Arc<OnceLock<PathBuf>>) {
    let seen = Arc::new(OnceLock::new());
    let slot = Arc::clone(&seen);
    let backend = ScriptedBackend::new(move |cwd| -> RunResult {
        let _ = slot.set(cwd.to_path_buf());
        stage(cwd);
        done()
    });
    (backend, seen)
}

/// A worker that commits one change on the PR branch.
fn commit_in(cwd: &Path) {
    std::fs::write(cwd.join("FIX.md"), "addressed\n").unwrap();
    git(cwd, &["add", "FIX.md"]);
    git(cwd, &["commit", "-m", "address review"]);
}

/// The worker's tree is gone once the runner returns.
fn released(seen: &OnceLock<PathBuf>) -> bool {
    !seen.get().expect("the worker ran").exists()
}

type JobRow = (
    i64,
    String,
    String,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// A repo with a published branch, a `pr_open` finding, and `pr_state`
/// flagged for attention — the state `run_engage` expects to be handed.
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (db, store) = fresh_store(&dir, "engage").await;

    // The clone location is the store's to choose (`repos/repo-<id>`), so
    // the fixture's git repo moves to wherever the id lands rather than the
    // row being pointed at the fixture.
    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();

    let (fid, _) = store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "fp-engage-1".to_owned(),
                file: "src/lib.rs".to_owned(),
                severity: hunter::domain::Severity::Medium,
                confidence: 0.9,
                summary: "a fix".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_pr_open(fid, PR_URL).await.unwrap();
    let recorded = store
        .record_open_pr(fid, &observed(&support::pr_sync(&store), fid, 1).await)
        .await
        .unwrap();
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );

    // Hermetic: the playbook is a stub written into the scratch root, not
    // the repo's real one. Reading `../hunter` made the test depend on the
    // developer's checkout layout — it broke the moment cargo-mutants ran
    // it from a temp copy — and coupled it to prompt wording it does not
    // care about. The scripted worker ignores the prompt entirely; all
    // that matters is that rendering succeeds, so the stub uses one slot
    // the engage builder is known to supply.
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("engage.md"), "engage {{WORKTREE}}\n").unwrap();
    // The harvest a withdrawal continues into; these two slots are the
    // ones the handoff tests assert on.
    std::fs::write(
        playbooks.join("harvest-closed.md"),
        "closed: the worktree is {{WORKTREE_STATE}}.\n```diff\n{{PR_DIFF}}\n```\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        _dir: dir,
        db,
        cfg,
        store,
        fid,
    }
}

/// A withdrawal is only real once the forge has accepted it. If the
/// comment fails, `close_pr` never issues the close — so recording the
/// verdict anyway would strand an open PR that nothing revisits.
#[tokio::test]
async fn failed_close_does_not_record_the_withdrawal() {
    let f = fixture("engage-close-fails").await;
    let bins = FakeBins::acquire("engage-close-fails");
    // `gh pr view` succeeds; `gh pr comment` fails; `gh pr close` must
    // therefore never run.
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("WITHDRAW.md", "not worth pursuing");
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .expect("run_engage should not error, it should decline to record");

    assert_eq!(summary.outcome.as_deref(), Some("withdraw-failed"));
    assert!(
        !bins.called_with("gh", "close"),
        "close must not be issued when the comment failed: {:?}",
        bins.calls()
    );

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        FindingStatus::PrOpen,
        "finding must stay pr_open so sync_prs keeps revisiting it"
    );
    assert!(
        after.verdict_reason.is_none(),
        "no verdict may be recorded for a withdrawal the forge rejected"
    );
    assert_eq!(
        f.pr_state().await.as_deref(),
        Some("OPEN"),
        "pr_state must keep the value sync wrote, not be flipped to closed"
    );
}

/// The happy path, so the test above is known to be asserting on a
/// difference rather than on a runner that always declines. A withdrawal
/// closes the PR and leaves the finding `closed`, awaiting its harvest,
/// with the worker's reason; it does not suppress it.
#[tokio::test]
async fn successful_close_records_the_withdrawal() {
    let f = fixture("engage-close-ok").await;
    let bins = FakeBins::acquire("engage-close-ok");
    bins.ok("gh", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::writing("WITHDRAW.md", "superseded by #9");
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    assert!(
        bins.called_with("gh", "close"),
        "close must be issued once the comment landed: {:?}",
        bins.calls()
    );

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(after.verdict_reason.as_deref(), Some("superseded by #9"));
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let suppressed = f.store.suppressions(after.repo_id, "bug").await.unwrap();
    assert!(
        suppressed.is_empty(),
        "a withdrawn finding must not feed the suppression corpus: {suppressed:?}"
    );
}

/// A withdrawal files no follow-ups: the harvest of the closed PR owns
/// them now. Ingesting the withdrawing worker's `FOLLOW-UPS.json` as well
/// would file the same work twice, under two different provenances.
#[tokio::test]
async fn a_withdrawal_files_no_follow_ups() {
    let f = fixture("engage-withdraw-no-followups").await;
    let bins = FakeBins::acquire("engage-withdraw-no-followups");
    bins.ok("gh", PR_VIEW_JSON);

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
        std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        support::done()
    });
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    assert!(summary.ingest.is_none(), "{summary:?}");
    let all = f
        .store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.iter().map(|x| x.id).collect::<Vec<_>>(),
        vec![f.fid],
        "no follow-up may be filed by the withdrawal"
    );
}

/// A PR closed on the forge (here: by a human) leaves its finding
/// `closed`, awaiting the harvest, and out of the suppression corpus.
/// Rejecting it on sight is what silenced valid findings: a closure alone
/// does not say whether the finding was wrong.
#[tokio::test]
async fn a_pr_closed_on_the_forge_waits_for_its_harvest() {
    let f = fixture("sync-closed").await;
    let bins = FakeBins::acquire("sync-closed");
    bins.ok("gh", PR_CLOSED_JSON);

    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;

    assert_eq!(result.closed, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("PR closed without merge; awaiting harvest")
    );
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let suppressed = f.store.suppressions(after.repo_id, "bug").await.unwrap();
    assert!(
        suppressed.is_empty(),
        "a closed PR's finding must not feed the suppression corpus: {suppressed:?}"
    );
}

/// A closed PR whose closure cannot be recorded keeps its finding
/// `pr_open`. `sync_prs` only revisits `pr_open` findings, so setting it
/// `closed` over a `pr_state` still reading OPEN would strand it; left
/// `pr_open`, the next sync records both.
#[tokio::test]
async fn a_closure_that_cannot_be_recorded_is_retried_by_the_next_sync() {
    let f = fixture("sync-closed-unrecorded").await;
    let bins = FakeBins::acquire("sync-closed-unrecorded");
    bins.ok("gh", PR_CLOSED_JSON);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_close BEFORE UPDATE OF state ON pr_state \
         WHEN NEW.state = 'CLOSED' \
         BEGIN SELECT RAISE(ABORT, 'injected mark_pr_closed failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;

    assert_eq!(result.closed, 0, "{result:?}");
    assert_eq!(result.errors, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_ne!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("PR closed but not recorded")),
        "{events:?}"
    );

    sqlx::query("DROP TRIGGER no_close")
        .execute(&pool)
        .await
        .unwrap();
    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;

    assert_eq!(result.closed, 1, "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
}

/// On a public repo anyone can comment on hunter's PRs, and engage acts on
/// a comment by committing and pushing. So only a comment by someone who
/// can push, or by a configured review bot, flags the PR for engage; a
/// stranger's leaves it alone.
#[tokio::test]
async fn only_maintainers_and_configured_bots_flag_new_comments() {
    for (author, bots, flagged) in [
        ("stranger", vec![], false),
        ("maintainer", vec![], true),
        ("coderabbitai", vec!["coderabbitai"], true),
    ] {
        let mut f = fixture(&format!("sync-screen-{author}")).await;
        f.cfg.review_bots = hunter::forge::ReviewBots::new(bots);
        let bins = FakeBins::acquire(&format!("sync-screen-{author}"));
        bins.script(
            "gh",
            &format!(
                "case \"$*\" in\n\
                 \x20 *graphql*) printf 'IC_1\\tBot\\n'; exit 0 ;;\n\
                 \x20 *collaborators/maintainer/permission*) echo true; exit 0 ;;\n\
                 \x20 *permission*) echo false; exit 0 ;;\n\
                 esac\n\
                 cat <<'__PR_EOF__'\n\
                 {{\"state\":\"OPEN\",\"mergeable\":\"MERGEABLE\",\"reviewDecision\":\"\",\"statusCheckRollup\":[],\
                 \"updatedAt\":\"2026-01-02T03:04:05Z\",\"headRefName\":\"{BRANCH}\",\"headRefOid\":\"deadbeef\",\
                 \"reviews\":[],\"comments\":[{{\"id\":\"IC_1\",\"author\":{{\"login\":\"{author}\"}},\
                 \"body\":\"please change this\",\"createdAt\":\"2026-01-02T03:04:05Z\"}}]}}\n\
                 __PR_EOF__\nexit 0"
            ),
        );

        let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;

        assert_eq!(result.errors, 0, "{author}: {result:?}");
        assert_eq!(
            f.attention().await.as_deref(),
            flagged.then_some("new_comments"),
            "{author}"
        );
    }
}

/// A failed withdrawal must not become an unbounded retry.
///
/// Left untouched, the finding is straight back in the engage tier, which
/// `pick_next` ranks first among the findings — so a persistently failing
/// forge would run a fresh worker every cycle forever, starving every
/// other kind of work. Until the work model, engage had no failure streak,
/// so the attention reason was marked addressed instead and the PR left
/// review until it changed. Now a refused withdrawal counts in the engage
/// streak like any failed engage: the third in a row holds the engage for
/// the operator, and the attention is left as the forge reports it.
#[tokio::test]
async fn a_failed_withdrawal_counts_toward_the_engage_streak() {
    let f = fixture("engage-close-fails-bounded").await;
    let bins = FakeBins::acquire("engage-close-fails-bounded");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    let backend = ScriptedBackend::writing("WITHDRAW.md", "not worth pursuing");

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_engage(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &backend,
        )
        .await
        .unwrap();
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("withdraw-failed".into()),
            Some("withdraw-failed".into()),
            Some("held".into())
        ]
    );
    assert_eq!(
        f.addressed_fp().await,
        None,
        "the attention is not set aside"
    );
    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held at the limit");
    assert_eq!(hold.reason, hunter::domain::HoldReason::Stuck);
    assert!(hold.detail.starts_with("PR close failed"), "{hold:?}");
    let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(
            picked,
            Some(
                hunter::scheduler::Candidate::Finding { .. }
                    | hunter::scheduler::Candidate::Resume { .. }
            )
        ),
        "a held engage is skipped: {picked:?}"
    );
}

/// A withdrawing engage is an attempt like any other: it spends a `once`
/// override and keeps an `exempt` one, whether the close lands or not.
/// A `once` left behind would hand the closed PR's harvest the
/// prioritized budget and put it at the head of the queue, so a single
/// override would pay for a second job.
#[tokio::test]
async fn a_withdrawal_spends_only_a_once_override() {
    let bins = FakeBins::acquire("engage-withdraw-override");
    for close_fails in [false, true] {
        for (mode, left) in [
            (BudgetOverride::Once, None),
            (BudgetOverride::Exempt, Some(BudgetOverride::Exempt)),
        ] {
            let f = fixture(&format!("engage-withdraw-override-{close_fails}-{mode}")).await;
            if close_fails {
                bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
            } else {
                bins.ok("gh", PR_VIEW_JSON);
            }
            f.store
                .set_budget_override(f.fid, Some(mode))
                .await
                .unwrap();

            let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
            let backend = ScriptedBackend::writing("WITHDRAW.md", "superseded by #9");
            let summary = hunter::scheduler::run_engage(
                &support::scope(&f.store),
                f.store.test_lane(),
                &f.cfg,
                Attempt::Cold(&finding),
                &backend,
            )
            .await
            .unwrap();

            let outcome = if close_fails {
                "withdraw-failed"
            } else {
                "withdrawn"
            };
            assert_eq!(
                summary.outcome.as_deref(),
                Some(outcome),
                "{mode}: {summary:?}"
            );
            let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
            assert_eq!(after.budget_override, left, "{outcome}, {mode} override");
            if !close_fails {
                let next = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
                    .await
                    .unwrap()
                    .expect("the closed PR's harvest is pending");
                assert_eq!(
                    next.budget_override(),
                    left,
                    "the cold harvest must not run under a spent override"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Push and reply
// ---------------------------------------------------------------------------

fn stage_reply(cwd: &Path) {
    std::fs::write(cwd.join("PR-REPLY.md"), "answered the question").unwrap();
}

/// Commits the worker made are force-pushed to the PR branch, and the
/// addressed fingerprint and head are cleared: the push changes the
/// head, so the next sync decides afresh whether the PR still needs
/// attention instead of trusting a verdict about the old head.
#[tokio::test]
async fn pushed_commits_land_on_the_branch_and_clear_the_addressed_state() {
    let f = fixture("engage-push").await;
    let bins = FakeBins::acquire("engage-push");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    // A decline of the PR's current problems to clear, so clearing is
    // observable.
    let ps = f.pr_row().await;
    let pr = support::pr_ref(PR_URL);
    let answered = f
        .store
        .mark_pr_engaged(f.fid, &EngageAnswer::without_push(&pr, None, &ps))
        .await
        .unwrap();
    assert!(
        matches!(answered, AnswerRecorded::Recorded(_)),
        "{answered:?}"
    );
    assert_eq!(f.pr_row().await.addressed_fingerprint.as_deref(), Some(FP));
    let (backend, seen) = recording(commit_in);

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert_eq!(
        git(&origin, &["log", "-1", "--format=%s", BRANCH]).trim(),
        "address review",
        "the worker's commit must be on the PR branch at origin"
    );
    let row = f.pr_row().await;
    assert_eq!(row.addressed_fingerprint, None);
    assert_eq!(row.addressed_head_sha, None);
    assert_eq!(
        row.last_engaged_activity_at,
        Some(0),
        "the watermark is the newest feedback the worker was shown: none"
    );
    assert!(
        !bins.called_with("gh", "comment"),
        "no PR-REPLY.md, so nothing may be posted: {:?}",
        bins.calls()
    );
    assert!(released(&seen));
}

/// A push's answer leaves the problems sync saw on the head it replaced to
/// a sync of the pushed head. Here sync saw a change request on the branch
/// tip, and the engage pushed a commit over that tip: settled from the old
/// head's change request as soon as the push was recorded, the PR was
/// flagged again before any sync had seen the new commit.
#[tokio::test]
async fn a_pushed_engage_leaves_the_replaced_head_s_problems_to_the_next_sync() {
    let f = fixture("engage-push-over").await;
    let bins = FakeBins::acquire("engage-push-over");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    // The last sync saw the PR at the tip the tree is made at, changes
    // requested, with no feedback.
    let tip = git(&origin, &["rev-parse", BRANCH]).trim().to_owned();
    let pass = support::pr_sync(&f.store);
    let at_tip = ObservedOpenPr::fixture(
        pass.begin_read_at(f.fid, &support::pr_ref(PR_URL), 2)
            .await
            .unwrap()
            .unwrap(),
        ObservedFixture {
            mergeable: Mergeable::Mergeable,
            changes_requested: true,
            checks: None,
            failing_checks: Vec::new(),
            head_ref: BRANCH.to_owned(),
            head_sha: tip,
            last_activity_at: 0,
        },
    );
    let recorded = f.store.record_open_pr(f.fid, &at_tip).await.unwrap();
    drop(pass);
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );
    assert_eq!(
        f.pr_row().await.needs_attention.as_deref(),
        Some("changes_requested")
    );
    let (backend, _seen) = recording(commit_in);

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert_eq!(
        f.pr_row().await.needs_attention,
        None,
        "the change request was seen on the head the push replaced"
    );
}

/// A worker that neither commits nor replies still concludes the
/// engagement: the attention reason is marked addressed so the finding
/// is not re-picked until the PR changes.
#[tokio::test]
async fn a_no_op_engage_marks_the_attention_addressed_and_touches_nothing() {
    let f = fixture("engage-noop").await;
    let bins = FakeBins::acquire("engage-noop");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let head = git(&origin, &["rev-parse", BRANCH]);
    let (backend, seen) = recording(|_| {});

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert_eq!(summary.failure, None);
    assert_eq!(git(&origin, &["rev-parse", BRANCH]), head);
    assert!(!bins.called_with("gh", "comment"), "{:?}", bins.calls());
    let row = f.pr_row().await;
    assert_eq!(row.addressed_fingerprint.as_deref(), Some(FP));
    assert_eq!(row.addressed_head_sha.as_deref(), Some("deadbeef"));
    assert_eq!(row.last_engaged_activity_at, Some(0));
    assert!(
        f.events("engage")
            .await
            .iter()
            .any(|m| m.contains("engaged (no-op)")),
        "{:?}",
        f.events("engage").await
    );
    assert!(released(&seen));
}

/// A reply the forge rejects is a retry, exactly like a failed push.
#[tokio::test]
async fn a_failed_reply_is_a_retry() {
    let f = fixture("engage-reply-fails").await;
    let bins = FakeBins::acquire("engage-reply-fails");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    let (backend, seen) = recording(stage_reply);

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert!(
        summary
            .failure
            .as_deref()
            .is_some_and(|m| m.starts_with("PR comment failed")),
        "{summary:?}"
    );
    assert_eq!(f.jobs().await[0].2, "failed");
    assert_eq!(f.pr_row().await.addressed_fingerprint, None);
    assert!(released(&seen));
}

/// A push that landed counts even when the reply after it fails. The
/// engage is a retry and answers nothing, but the PR's head moved: a sync
/// read begun before the push saw the replaced head, and its change
/// request must wait for a read of the pushed one. Uncounted, the stale
/// observation flagged the PR again for a commit that was no longer its
/// head.
#[tokio::test]
async fn a_push_whose_reply_fails_still_counts_as_a_push() {
    let f = fixture("engage-push-then-reply-fails").await;
    let bins = FakeBins::acquire("engage-push-then-reply-fails");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let tip = git(&origin, &["rev-parse", BRANCH]).trim().to_owned();
    let at_tip = |head_sha: String| ObservedFixture {
        mergeable: Mergeable::Mergeable,
        changes_requested: true,
        checks: None,
        failing_checks: Vec::new(),
        head_ref: BRANCH.to_owned(),
        head_sha,
        last_activity_at: 0,
    };
    let pass = support::pr_sync(&f.store);
    let seen = ObservedOpenPr::fixture(
        pass.begin_read_at(f.fid, &support::pr_ref(PR_URL), 2)
            .await
            .unwrap()
            .unwrap(),
        at_tip(tip.clone()),
    );
    let recorded = f.store.record_open_pr(f.fid, &seen).await.unwrap();
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );
    drop(pass);
    let watermark = f.pr_row().await.last_engaged_activity_at;
    // The next sync asks the forge before the engage pushes, and still
    // sees the change request on the old tip.
    let pass = support::pr_sync(&f.store);
    let in_flight = ObservedOpenPr::fixture(
        pass.begin_read_at(f.fid, &support::pr_ref(PR_URL), 3)
            .await
            .unwrap()
            .unwrap(),
        at_tip(tip.clone()),
    );
    let (backend, _seen) = recording(|tree| {
        commit_in(tree);
        stage_reply(tree);
    });

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert_ne!(
        git(&origin, &["rev-parse", BRANCH]).trim(),
        tip,
        "the push landed"
    );
    let recorded = f.store.record_open_pr(f.fid, &in_flight).await.unwrap();
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );
    let row = f.pr_row().await;
    assert_eq!(
        row.needs_attention, None,
        "the change request was read on the head the push replaced"
    );
    assert_eq!(
        row.last_engaged_activity_at, watermark,
        "an unpublished engage answers no feedback"
    );
}

/// A reply that fails after no push counts as nothing at all: the PR's
/// head did not move, so a sync read begun before the engage still saw it,
/// and its change request still stands. Counted as a push, the failed
/// reply made that read wait and unflagged a PR nobody had answered.
#[tokio::test]
async fn a_failed_reply_without_a_push_counts_as_no_push() {
    let f = fixture("engage-reply-fails-no-push").await;
    let bins = FakeBins::acquire("engage-reply-fails-no-push");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let tip = git(&origin, &["rev-parse", BRANCH]).trim().to_owned();
    let at_tip = || ObservedFixture {
        mergeable: Mergeable::Mergeable,
        changes_requested: true,
        checks: None,
        failing_checks: Vec::new(),
        head_ref: BRANCH.to_owned(),
        head_sha: tip.clone(),
        last_activity_at: 0,
    };
    let pass = support::pr_sync(&f.store);
    let seen = ObservedOpenPr::fixture(
        pass.begin_read_at(f.fid, &support::pr_ref(PR_URL), 2)
            .await
            .unwrap()
            .unwrap(),
        at_tip(),
    );
    let recorded = f.store.record_open_pr(f.fid, &seen).await.unwrap();
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );
    drop(pass);
    let pass = support::pr_sync(&f.store);
    let in_flight = ObservedOpenPr::fixture(
        pass.begin_read_at(f.fid, &support::pr_ref(PR_URL), 3)
            .await
            .unwrap()
            .unwrap(),
        at_tip(),
    );
    let (backend, _seen) = recording(stage_reply);

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert_eq!(
        git(&origin, &["rev-parse", BRANCH]).trim(),
        tip,
        "nothing was pushed"
    );
    let recorded = f.store.record_open_pr(f.fid, &in_flight).await.unwrap();
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );
    assert_eq!(
        f.pr_row().await.needs_attention.as_deref(),
        Some("changes_requested"),
        "the head the read saw is still the PR's"
    );
}

// ---------------------------------------------------------------------------
// Guards: each bails before a job exists or the forge is contacted
// ---------------------------------------------------------------------------

/// Run engage on a fixture the test has already bent out of shape and
/// check it bailed with `err`, logged an error event containing `event`,
/// wrote no job, ran no worker and made no forge call.
async fn assert_bails(f: &Fixture, bins: &FakeBins, err: &str, event: &str) {
    let (backend, seen) = recording(|_| {});
    let got = f.run(&backend).await.expect_err("engage must refuse");
    assert!(
        got.to_string().starts_with(err),
        "error {got:#} should start with {err:?}"
    );
    let errors = f.events("error").await;
    assert!(
        errors.iter().any(|m| m.contains(event)),
        "an error event mentioning {event:?} must be logged: {errors:?}"
    );
    assert!(f.jobs().await.is_empty(), "no job row may be written");
    assert!(seen.get().is_none(), "the worker must not run");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
}

/// The repo row is gone from `get_repo_by_id`'s view. `soft_delete_repo`
/// refuses a repo with findings, so the row is marked deleted directly —
/// the state a repo removed between the pick and the run would be in.
#[tokio::test]
async fn a_deleted_repo_bails() {
    let f = fixture("engage-no-repo").await;
    let bins = FakeBins::acquire("engage-no-repo");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE repos SET deleted_at = 1").await;
    assert_bails(&f, &bins, "repo missing", "missing").await;
}

#[tokio::test]
async fn a_finding_never_synced_bails() {
    let f = fixture("engage-no-pr-state").await;
    let bins = FakeBins::acquire("engage-no-pr-state");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("DELETE FROM pr_state").await;
    assert_bails(&f, &bins, "no pr_state", "sync first").await;
}

#[tokio::test]
async fn an_empty_head_ref_bails() {
    let f = fixture("engage-empty-head").await;
    let bins = FakeBins::acquire("engage-empty-head");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE pr_state SET head_ref = ''").await;
    assert_bails(&f, &bins, "no pr_state", "no head_ref").await;
}

/// A PR whose head is the default branch would have engage force-push
/// the worker's commits onto it.
#[tokio::test]
async fn a_head_ref_equal_to_the_default_branch_is_refused() {
    let f = fixture("engage-head-is-main").await;
    let bins = FakeBins::acquire("engage-head-is-main");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE pr_state SET head_ref = 'main'").await;
    assert_bails(
        &f,
        &bins,
        "head_ref equals default branch",
        "head_ref equals default branch",
    )
    .await;
}

#[tokio::test]
async fn an_unparseable_pr_url_bails() {
    let f = fixture("engage-bad-pr-url").await;
    let bins = FakeBins::acquire("engage-bad-pr-url");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE findings SET pr_url = 'not a pr url'").await;
    assert_bails(&f, &bins, "unparseable pr_url", "unparseable pr_url").await;
}

/// The PR an engage answers, closes and replies on is the finding's, its
/// number parsed from its URL. A `pr_state` row naming another PR holds
/// that PR's head and problems, so the engage waits for the sync that
/// rewrites it instead of working from it.
#[tokio::test]
async fn a_pr_state_row_of_another_pr_bails() {
    let f = fixture("engage-row-of-other-pr").await;
    let bins = FakeBins::acquire("engage-row-of-other-pr");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE pr_state SET pr_number = 8").await;
    assert_bails(
        &f,
        &bins,
        "pr_state of another PR",
        "pr_state is PR #8, the finding's PR is #7",
    )
    .await;
}

/// Without a PR URL there is no PR to record the answer against: it lands
/// only while the finding's `pr_url` is still the one the engage started on.
#[tokio::test]
async fn an_empty_pr_url_bails() {
    let f = fixture("engage-no-pr-url").await;
    let bins = FakeBins::acquire("engage-no-pr-url");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE findings SET pr_url = ''").await;
    assert_bails(&f, &bins, "no pr_url", "no pr_url").await;
}

/// A PR branch that is gone from origin fails the fetch before any job
/// is created. The PR view has run by then: the fetch comes after every
/// forge read, so the clone is not held across them.
#[tokio::test]
async fn a_head_ref_missing_from_origin_bails_at_the_fetch() {
    let f = fixture("engage-fetch-fails").await;
    let bins = FakeBins::acquire("engage-fetch-fails");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE pr_state SET head_ref = 'gone/branch'").await;
    let (backend, seen) = recording(|_| {});
    let err = f.run(&backend).await.expect_err("engage must refuse");
    assert!(err.to_string().starts_with("fetch failed"), "{err:#}");
    let errors = f.events("error").await;
    assert!(
        errors
            .iter()
            .any(|m| m.contains("fetch gone/branch failed")),
        "{errors:?}"
    );
    assert!(f.jobs().await.is_empty(), "no job row may be written");
    assert!(seen.get().is_none(), "the worker must not run");
}

/// The PR could not be read from the forge: no job, no worker.
#[tokio::test]
async fn a_failed_pr_view_bails_before_the_job() {
    let f = fixture("engage-view-fails").await;
    let bins = FakeBins::acquire("engage-view-fails");
    bins.fail("gh", 1, "HTTP 502");
    let (backend, seen) = recording(|_| {});
    let err = f.run(&backend).await.expect_err("engage must refuse");
    assert_eq!(err.to_string(), "PR/MR view failed");
    assert!(f.jobs().await.is_empty(), "no job row may be written");
    assert!(seen.get().is_none(), "the worker must not run");
    assert!(
        f.events("error")
            .await
            .iter()
            .any(|m| m.contains("PR/MR view failed")),
        "{:?}",
        f.events("error").await
    );
}

/// A `pr_state` row that lost its number is repaired from the finding's
/// PR URL and the engagement proceeds against that number.
#[tokio::test]
async fn a_missing_pr_number_is_recovered_from_the_pr_url() {
    let f = fixture("engage-self-heal").await;
    let bins = FakeBins::acquire("engage-self-heal");
    bins.ok("gh", PR_VIEW_JSON);
    f.exec("UPDATE pr_state SET pr_number = NULL").await;
    let (backend, _seen) = recording(|_| {});

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert_eq!(summary.pr_number, Some(7));
    assert_eq!(f.pr_row().await.pr_number, Some(7), "the row is repaired");
    assert!(
        bins.calls_to("gh")
            .iter()
            .any(|c| c.get(1).map(String::as_str) == Some("pr") && c.iter().any(|a| a == "7")),
        "the PR must be viewed by the recovered number: {:?}",
        bins.calls()
    );
}

/// A clone directory belonging to a different repository is refused.
///
/// `repos/repo-<id>` is derived from the id, so a delete whose directory
/// removal failed — or anything an operator put there — leaves a clone at
/// the path a later repo would use, and it would otherwise be used as-is:
/// `sync_repo` skips cloning whenever the path exists, so the daemon would
/// hunt one project's code, file the findings against another, and push to
/// whichever URL the database named.
#[tokio::test]
async fn a_clone_of_a_different_repository_is_refused() {
    let dir = TempDir::new("wrong-origin");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "origin").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    // Registered under a URL that is *not* the origin of the checkout
    // sitting at its path — exactly the state a leftover directory leaves.
    let rid = store
        .add_repo(
            "widget",
            "https://github.com/acme/somebody-elses-repo",
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();

    let row = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("hunt.md"), "hunt {{WORKTREE}}\n").unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    // Never reached: the sync refuses before any worker is spawned, which
    // is itself part of the contract.
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let err = hunter::scheduler::run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &backend,
    )
    .await
    .expect_err("a clone of another repository must not be hunted");
    let msg = err.to_string();
    assert!(
        msg.contains("but its origin is") && msg.contains("somebody-elses-repo"),
        "the refusal must name both URLs so the operator can see the mismatch: {msg}"
    );
}

/// A clone whose origin is the repo's URL is not refused because a
/// `url.<base>.insteadOf` rule rewrites that URL.
///
/// `git clone -- <url>` stores the URL as given, but `git remote get-url`
/// prints it after insteadOf rewriting, so a rule in the daemon user's
/// gitconfig (`url."git@github.com:".insteadOf https://github.com/` is
/// the common one) made the clone the daemon itself had just made look
/// like somebody else's: every later job for the repo was refused. The
/// rule sits in the clone's own config here so the test touches no global
/// git config; git applies it identically.
#[tokio::test]
async fn an_insteadof_rewrite_of_the_origin_is_not_refused() {
    let dir = TempDir::new("insteadof-origin");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "insteadof").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    let repo_dir = hunter::store::Store::repo_dir(&repos_root, rid);
    std::fs::rename(&repo.work, &repo_dir).unwrap();
    // Origin is the registered URL, exactly as `git clone -- <url>`
    // records it; the rule resolves it to the local bare origin.
    support::git(&repo_dir, &["remote", "set-url", "origin", REPO_URL]);
    support::git(
        &repo_dir,
        &[
            "config",
            &format!("url.{}.insteadOf", repo.origin.display()),
            REPO_URL,
        ],
    );

    let row = store.get_repo_by_id(rid).await.unwrap().unwrap();
    let playbooks = dir.subdir("playbooks");
    std::fs::write(
        playbooks.join("hunt.md"),
        "hunt {{REPO_PATH}} {{DIFF_RANGE}} -> {{OUT_PATH}}\n",
    )
    .unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let summary = hunter::scheduler::run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&row),
        &backend,
    )
    .await
    .expect("the daemon's own clone must not be refused over an insteadOf rewrite");
    assert!(
        summary.job_id.is_some(),
        "the hunt must get past the sync and run: {summary:?}"
    );
}

/// A hunt whose repo was deleted between the pick and the job insert
/// reports which kind and which repo it skipped.
///
/// `create_job` refuses a soft-deleted repo, and the executor turns that
/// into a skipped cycle rather than an error: deleting a repo is operator
/// traffic, not a crash. The status page and the cycle log render that
/// summary, so "skipped" without the kind and repo would tell the
/// operator nothing about what was dropped. The row handed to `run_hunt`
/// is the one the pick read, taken before the delete, exactly as a cycle
/// holds it.
#[tokio::test]
async fn a_hunt_refused_because_its_repo_was_deleted_names_what_it_skipped() {
    let dir = TempDir::new("refused-hunt");
    let repo = GitRepo::with_branch(&dir, BRANCH);
    let (_db, store) = fresh_store(&dir, "refused").await;

    let repos_root = dir.path().join("repos");
    std::fs::create_dir_all(&repos_root).unwrap();
    let rid = store
        .add_repo(
            "widget",
            &repo.origin.to_string_lossy(),
            &repos_root,
            &repo.default_branch,
            ForgeName::Github,
        )
        .await
        .unwrap();
    std::fs::rename(&repo.work, hunter::store::Store::repo_dir(&repos_root, rid)).unwrap();

    let picked = store.get_repo_by_id(rid).await.unwrap().unwrap();
    store.soft_delete_repo(rid).await.unwrap();

    let playbooks = dir.subdir("playbooks");
    std::fs::write(playbooks.join("hunt.md"), "hunt {{WORKTREE}}\n").unwrap();
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    // Never reached: the job row is refused before any worker is spawned.
    let backend = ScriptedBackend::writing("findings.json", "[]");
    let summary = hunter::scheduler::run_hunt(
        &support::scope(&store),
        store.test_lane(),
        &cfg,
        Attempt::Cold(&picked),
        &backend,
    )
    .await
    .expect("losing the race to a delete is a skipped cycle, not an error");

    assert_eq!(
        (
            summary.kind,
            summary.repo.as_deref(),
            summary.finding_id,
            summary.skipped.as_deref(),
        ),
        (
            Some(RepoJobKind::Hunt.into()),
            Some("widget"),
            None,
            Some(format!("repo {rid} is deleted -- cannot start a hunt job").as_str()),
        ),
        "a refused job must still name the kind and repo it skipped: {summary:?}"
    );
    assert_eq!(summary.job_id, None, "no job row may have been written");
}

// -- a withdrawal continues into its harvest ---------------------------------

/// `gh` for a withdrawal and the harvest after it: the view for every
/// call but `pr diff`.
fn gh_for_handoff(bins: &FakeBins) {
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = diff ]; then\ncat <<'__FAKE_EOF__'\n{PR_DIFF}__FAKE_EOF__\nexit 0\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
}

/// A worker that withdraws on its first run, leaving a transcript in the
/// chain's session directory (unless `keep_transcript` is false, which is
/// how a transcript goes missing between the two runs), and classifies
/// the closure on a run that continues a transcript. A transcript that is
/// not on disk is refused the way the harness refuses it. The withdrawing
/// run also leaves a `FOLLOW-UPS.json`, as engage workers did before the
/// harvest took follow-ups over; nobody verified it, so nobody may file it.
fn withdraw_then_classify(keep_transcript: bool) -> ScriptedBackend {
    ScriptedBackend::staged(move |tree, resume_from| {
        let session = tree.parent().unwrap().join("session").join("engage.jsonl");
        match resume_from {
            None => {
                std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
                std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
                if keep_transcript {
                    std::fs::write(&session, format!("{USAGE}\n")).unwrap();
                }
                hunter::types::RunResult {
                    session_file: Some(session.to_string_lossy().into_owned()),
                    ..support::done()
                }
            }
            Some(file) if !file.exists() => hunter::types::RunResult {
                exit_code: None,
                killed_reason: Some("resume-unavailable".to_owned()),
                tokens_new: 0,
                calls: 0,
                session_file: None,
                duration_s: 0.0,
                stdout_tail: "cannot resume".to_owned(),
                usage_delta: None,
            },
            Some(file) => {
                std::fs::write(
                    tree.join("CLOSE-REASON.json"),
                    r#"{"classification":"superseded","reason":"landed in #9","evidence":"abc1234"}"#,
                )
                .unwrap();
                hunter::types::RunResult {
                    session_file: Some(file.to_string_lossy().into_owned()),
                    ..support::done()
                }
            }
        }
    })
}

/// What a failed handoff must leave behind: exactly what the withdrawal
/// left. The finding `closed`, the PR unharvested with no failure counted,
/// the chain's tree released, and the harvest tier about to review it
/// cold.
async fn assert_left_for_the_cold_harvest(f: &Fixture, tree: &std::path::Path) {
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Harvest)
            .await
            .unwrap()
            .streak,
        0,
        "a failed handoff is not a harvest failure"
    );
    assert!(!tree.exists(), "the chain's tree must be released");
    match hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(hunter::scheduler::Candidate::Finding {
            kind: hunter::domain::FindingJobKind::Harvest,
            finding_id,
            ..
        }) => assert_eq!(finding_id, f.fid),
        other => panic!("expected the cold harvest next, got {other:?}"),
    }
}

/// A withdrawing engage continues straight into the closed PR's harvest,
/// as the next attempt of its own chain: resumed from the engage job, in
/// the engage's tree, continuing the engage's transcript, told the tree is
/// the PR's head, reserving what a resume of that transcript reserves.
/// The harvest's classification lands, and the tree is released after.
#[tokio::test]
async fn a_withdrawal_continues_into_its_harvest_in_the_same_session() {
    let f = fixture("handoff-ok").await;
    let bins = FakeBins::acquire("handoff-ok");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        runs[1].tree, runs[0].tree,
        "the harvest runs in the engage's tree"
    );
    let transcript = runs[0]
        .tree
        .parent()
        .unwrap()
        .join("session")
        .join("engage.jsonl");
    assert_eq!(runs[1].resume_from.as_deref(), Some(transcript.as_path()));
    // Nothing moves `origin/main` in this test, so the clone's tip is the
    // commit the harvest resolved after its fetch.
    let repo = f
        .store
        .get_repo_by_id(finding.repo_id)
        .await
        .unwrap()
        .unwrap();
    let base = git(Path::new(&repo.path), &["rev-parse", "origin/main"]);
    let base = base.trim();
    assert!(
        runs[1].prompt.contains(&format!(
            "checked out at the PR's head branch; main is commit {base}"
        )),
        "{}",
        runs[1].prompt
    );
    assert!(runs[1].prompt.contains(PR_DIFF), "{}", runs[1].prompt);

    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    let (engage, harvest) = (&jobs[0], &jobs[1]);
    assert_eq!((engage.0, engage.1.as_str()), (engage_job, "engage"));
    assert_eq!(harvest.1, "harvest");
    assert_eq!(harvest.2, "done");
    assert_eq!(harvest.3, Some(engage_job), "resumed from the engage job");
    assert_eq!(
        harvest.4,
        Some(200_000),
        "the resume reservation of the transcript"
    );
    assert_eq!(harvest.5, engage.5, "the chain's pinned commit");

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Superseded);
    assert_eq!(
        after.verdict_reason.as_deref(),
        Some("superseded: landed in #9 (evidence: abc1234)")
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some());
    let all = f
        .store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        1,
        "the engage's leftover FOLLOW-UPS.json must not be filed as the harvest's: {all:?}"
    );
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");
}

/// The admin lane fetches clones while a job's worker runs, so
/// `origin/main` can move under a harvest continuing a withdrawal. The
/// verdict must still be anchored at the commit the harvest resolved right
/// after its own fetch and named to the worker, never at whatever the ref
/// says when the worker is done: commits in between were never judged, and
/// anchoring past them hides their changes from suppression for good.
#[tokio::test]
async fn a_handoff_verdict_is_anchored_at_the_commit_its_worker_was_shown() {
    let f = fixture("handoff-base-moves").await;
    let bins = FakeBins::acquire("handoff-base-moves");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // What `origin/main` named when the harvest's worker started, read by
    // the worker itself; the backend then moves the ref as a fetch would.
    let shown: Arc<OnceLock<String>> = Arc::default();
    let backend = ScriptedBackend::staged({
        let shown = shown.clone();
        move |tree, resume_from| {
            let session = tree.parent().unwrap().join("session").join("engage.jsonl");
            if let Some(file) = resume_from {
                let tip = git(tree, &["rev-parse", "origin/main"]).trim().to_owned();
                shown.set(tip).unwrap();
                // A fetch landing while the worker runs: `origin/main` now
                // names the PR's head, a commit the worker never judged.
                let head = git(tree, &["rev-parse", "HEAD"]).trim().to_owned();
                git(tree, &["update-ref", "refs/remotes/origin/main", &head]);
                std::fs::write(
                    tree.join("CLOSE-REASON.json"),
                    serde_json::json!({
                        "classification": "wrong",
                        "reason": "the guard rejects it",
                        "evidence": "abc1234",
                        "holds_while": "the guard at src/lib.rs:1 rejects it",
                        "depends_on": ["src/lib.rs:1"],
                    })
                    .to_string(),
                )
                .unwrap();
                return RunResult {
                    session_file: Some(file.to_string_lossy().into_owned()),
                    ..done()
                };
            }
            std::fs::write(tree.join("WITHDRAW.md"), "the guard rejects it").unwrap();
            std::fs::write(&session, format!("{USAGE}\n")).unwrap();
            RunResult {
                session_file: Some(session.to_string_lossy().into_owned()),
                ..done()
            }
        }
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Rejected);
    let shown = shown.get().expect("the harvest's worker ran").clone();
    let runs = backend.runs();
    assert!(
        runs[1].prompt.contains(&shown),
        "the worker is named the commit by its sha: {}",
        runs[1].prompt
    );
    let anchors = f
        .store
        .suppression_anchors(finding.repo_id, finding.kind.as_str())
        .await
        .unwrap();
    let anchor = anchors.get(&f.fid).expect("a rejected verdict is anchored");
    assert_eq!(
        anchor.sha, shown,
        "anchored at the commit the worker was shown, not the ref's later tip"
    );
}

/// A sweep while the handoff stands at the harvest's budget gate keeps
/// the engage's tree. At that moment the engage is recorded `done` and
/// the harvest has no row yet, so the job table says no attempt of the
/// chain is live: only the engage's lease on the tree, held across the
/// handoff, keeps the sweep from releasing it. The harvest then runs in
/// that tree, and the tree is released once both are done with it.
#[tokio::test]
async fn a_sweep_during_the_handoff_keeps_the_tree_for_the_harvest() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let f = fixture("handoff-sweep").await;
    let bins = FakeBins::acquire("handoff-sweep");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let sweeps = Arc::new(AtomicUsize::new(0));
    let backend = withdraw_then_classify(true).on_decide({
        let (store, root, sweeps) = (f.store.clone(), f.cfg.work_root.clone(), sweeps.clone());
        move || {
            let (store, root, sweeps) = (store.clone(), root.clone(), sweeps.clone());
            async move {
                hunter::workspace::sweep(store.test_lane(), &root, hunter::util::now_ms())
                    .await
                    .unwrap();
                sweeps.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    // The engage's gate, then the harvest's: the second ran in the gap.
    assert_eq!(sweeps.load(Ordering::SeqCst), 2);
    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        runs[1].tree, runs[0].tree,
        "the harvest runs in the engage's tree"
    );
    let jobs = f.jobs().await;
    assert_eq!(
        (jobs[1].1.as_str(), jobs[1].2.as_str()),
        ("harvest", "done"),
        "{jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Superseded);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");
}

/// A window with room for the engage but not for continuing its
/// transcript into the harvest: nothing else happens. No harvest job, the
/// finding stays `closed` for the cold harvest, and the tree is released.
#[tokio::test]
async fn a_denied_handoff_leaves_the_finding_closed_and_releases_the_tree() {
    let f = fixture("handoff-denied").await;
    let bins = FakeBins::acquire("handoff-denied");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    // Engage reserves its cold 40,000; the handoff reserves 200,000.
    let backend = withdraw_then_classify(true).denying_above(150_000);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.1 == "engage"),
        "a denied handoff writes no job: {jobs:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The engage's transcript is gone by the time the harvest would continue
/// it: the harness refuses (`resume-unavailable`), and nothing else
/// happens either — the engage job is not rewritten, no failure counts,
/// and the finding waits `closed` for the cold harvest.
#[tokio::test]
async fn an_unavailable_transcript_leaves_the_finding_closed_and_releases_the_tree() {
    let f = fixture("handoff-unavailable").await;
    let bins = FakeBins::acquire("handoff-unavailable");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(false);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "{runs:?}");
    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_eq!(jobs[0].2, "done", "the finished engage must stay done");
    assert_eq!(
        (jobs[1].1.as_str(), jobs[1].2.as_str()),
        ("harvest", "failed")
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The handoff's worker finishes but says nothing about why the PR
/// closed, so the handoff fails and the cold harvest reviews the PR again
/// from scratch, follow-ups included. The failed handoff's own follow-ups
/// must not be filed: the cold review re-derives them under slugs of its
/// own choosing, and the same open work would be filed twice.
#[tokio::test]
async fn a_failed_handoff_files_no_follow_ups() {
    let f = fixture("handoff-followups").await;
    let bins = FakeBins::acquire("handoff-followups");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::staged(move |tree, resume_from| {
        let session = tree.parent().unwrap().join("session").join("engage.jsonl");
        if resume_from.is_none() {
            std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
            std::fs::write(&session, format!("{USAGE}\n")).unwrap();
        } else {
            std::fs::write(tree.join("FOLLOW-UPS.json"), FOLLOW_UP).unwrap();
        }
        hunter::types::RunResult {
            session_file: Some(session.to_string_lossy().into_owned()),
            ..support::done()
        }
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "the handoff must have run: {runs:?}");
    let all = f
        .store
        .list_findings(&hunter::store::FindingFilter::default())
        .await
        .unwrap();
    assert_eq!(
        all.len(),
        1,
        "a failed handoff's FOLLOW-UPS.json must not be filed: {all:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The closed PR's diff will not load for the handoff. That fails the
/// handoff before any harvest job exists, but it is still no harvest
/// failure: the cold harvest keeps its full budget of attempts, so no
/// attempt may be counted toward its give-up streak.
#[tokio::test]
async fn a_handoff_whose_diff_fails_counts_no_harvest_attempt() {
    let f = fixture("handoff-diff-fails").await;
    let bins = FakeBins::acquire("handoff-diff-fails");
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = diff ]; then\necho 'HTTP 502' >&2\nexit 1\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(
        runs.len(),
        1,
        "no harvest worker without the diff: {runs:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The forge accepted the close, but the finding could not be set
/// `closed`. Its `pr_state` CLOSED is the same write, so neither lands:
/// a `pr_state` reading CLOSED beside a `pr_open` finding would send a
/// harvest that keeps `pr_open` as if a human had set it and marks the PR
/// harvested, so the `closed` the next sync writes would never be
/// harvested. Nothing continues: the finding stays `pr_open`, and the
/// next sync -- which revisits `pr_open` findings and sees the PR closed
/// -- records both and queues the cold harvest.
///
/// The failing write is injected with a trigger that refuses to set a
/// finding's status to `closed`.
#[tokio::test]
async fn an_unrecorded_closed_verdict_starts_no_harvest_and_waits_for_the_sync() {
    let f = fixture("handoff-verdict-unrecorded").await;
    let bins = FakeBins::acquire("handoff-verdict-unrecorded");
    gh_for_handoff(&bins);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_closed_verdict BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'closed' \
         BEGIN SELECT RAISE(ABORT, 'injected closed verdict failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.3 != Some(engage_job)),
        "no harvest may continue the engage: {jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_ne!(
        f.pr_state().await.as_deref(),
        Some("CLOSED"),
        "the closure lands with the finding's move or not at all"
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");

    // The store recovers; the next sync finds the PR closed on the forge.
    sqlx::query("DROP TRIGGER no_closed_verdict")
        .execute(&pool)
        .await
        .unwrap();
    bins.ok("gh", PR_CLOSED_JSON);
    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;
    assert_eq!(result.closed, 1, "{result:?}");
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The finding left `pr_open` between the engage's read and its `closed`
/// verdict, closed by a decision other than the PR sync's record of this
/// closure (here an operator's). That decision owns the finding, so the
/// engage neither overwrites its verdict nor starts the harvest it would
/// hand to.
///
/// The race is staged with a trigger: the engage's UPDATE first moves the
/// row the way that decision would and is then skipped, exactly as if its
/// write had landed in between.
#[tokio::test]
async fn a_withdrawal_whose_finding_another_decision_closed_keeps_that_verdict() {
    let f = fixture("handoff-other-first").await;
    let bins = FakeBins::acquire("handoff-other-first");
    gh_for_handoff(&bins);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER other_first BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'closed' AND NEW.verdict_reason IS NOT 'operator: closed' \
         BEGIN \
           UPDATE findings SET status = 'closed', verdict_reason = 'operator: closed' \
             WHERE id = OLD.id; \
           UPDATE pr_state SET state = 'CLOSED' WHERE finding_id = OLD.id; \
           SELECT RAISE(IGNORE); \
         END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.3 != Some(engage_job)),
        "no harvest may continue the engage: {jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(after.verdict_reason.as_deref(), Some("operator: closed"));
}

/// The PR sync runs beside the engage, so it can see the PR the engage
/// has just closed on the forge and record that closure before the
/// engage's own write lands. It is the same closure: the engage takes it
/// over and continues into its harvest in the same session, as if its
/// write had landed. Left to the sync, the warm handoff would be lost and
/// a cold harvest would review the closure from scratch.
///
/// Staged with a trigger that records the closure exactly as the sync
/// does -- `closed` with the sync's reason, `pr_state` CLOSED -- in place
/// of the engage's write.
#[tokio::test]
async fn a_withdrawal_the_sync_recorded_first_still_continues_into_its_harvest() {
    let f = fixture("handoff-sync-first").await;
    let bins = FakeBins::acquire("handoff-sync-first");
    gh_for_handoff(&bins);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    assert_eq!(
        hunter::transition::PR_CLOSED_AWAITING_HARVEST,
        "PR closed without merge; awaiting harvest",
        "the trigger below writes the sync's reason"
    );
    sqlx::query(
        "CREATE TRIGGER sync_first BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'closed' AND OLD.status = 'pr_open' \
         BEGIN \
           UPDATE findings SET status = 'closed', \
             verdict_reason = 'PR closed without merge; awaiting harvest' \
             WHERE id = OLD.id; \
           UPDATE pr_state SET state = 'CLOSED' WHERE finding_id = OLD.id; \
           SELECT RAISE(IGNORE); \
         END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "the harvest continues the engage: {runs:?}");
    assert_eq!(runs[1].tree, runs[0].tree);
    let jobs = f.jobs().await;
    let harvest = jobs
        .iter()
        .find(|j| j.1 == "harvest")
        .expect("a harvest ran");
    assert_eq!(harvest.3, Some(engage_job), "resumed from the engage job");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.status,
        FindingStatus::Superseded,
        "the harvest's classification landed"
    );
}

/// The same for the closed PR's view, which the harvest fetches before
/// the diff. The engage's own view (the first) succeeds; the handoff's
/// (the second) fails.
#[tokio::test]
async fn a_handoff_whose_view_fails_counts_no_harvest_attempt() {
    let f = fixture("handoff-view-fails").await;
    let bins = FakeBins::acquire("handoff-view-fails");
    bins.script(
        "gh",
        &format!(
            "if [ \"$2\" = view ]; then\nviews=\"$(dirname \"$0\")/views\"\necho x >> \"$views\"\nif [ \"$(wc -l < \"$views\")\" -gt 1 ]; then\necho 'HTTP 502' >&2\nexit 1\nfi\nfi\ncat <<'__FAKE_EOF__'\n{PR_VIEW_JSON}\n__FAKE_EOF__\nexit 0"
        ),
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let runs = backend.runs();
    assert_eq!(
        runs.len(),
        1,
        "no harvest worker without the view: {runs:?}"
    );
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "error" && e.message.contains("PR/MR view failed")),
        "the handoff reached the harvest's view: {events:?}"
    );
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// The forge accepted the close but `pr_state` could not record it. The
/// harvest reads its playbook off `pr_state`, so continuing into it now
/// would review the closed PR as a merged one and mark it harvested with
/// no closure classification. Nothing continues: the finding stays
/// `pr_open`, the tree is released, and the next sync -- which sees the PR
/// closed on the forge -- records both and queues the cold harvest.
///
/// The failing write is injected with a trigger that refuses to flip a
/// `pr_state` row to CLOSED, the same way `store_write_test` breaks
/// one statement of a sync.
#[tokio::test]
async fn an_unrecorded_close_starts_no_harvest_and_waits_for_the_sync() {
    let f = fixture("handoff-unrecorded").await;
    let bins = FakeBins::acquire("handoff-unrecorded");
    gh_for_handoff(&bins);
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_close BEFORE UPDATE OF state ON pr_state \
         WHEN NEW.state = 'CLOSED' \
         BEGIN SELECT RAISE(ABORT, 'injected mark_pr_closed failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = withdraw_then_classify(true);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"));
    let engage_job = summary.job_id.unwrap();
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "only the engage may run: {runs:?}");
    let jobs = f.jobs().await;
    assert!(
        jobs.iter().all(|j| j.3 != Some(engage_job)),
        "no harvest may continue the engage: {jobs:?}"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert_ne!(f.pr_state().await.as_deref(), Some("CLOSED"));
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.harvested_at, None);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");

    // The store recovers; the next sync finds the PR closed on the forge.
    sqlx::query("DROP TRIGGER no_close")
        .execute(&pool)
        .await
        .unwrap();
    bins.ok("gh", PR_CLOSED_JSON);
    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;
    assert_eq!(result.closed, 1, "{result:?}");
    assert_eq!(f.pr_state().await.as_deref(), Some("CLOSED"));
    assert_left_for_the_cold_harvest(&f, &runs[0].tree).await;
}

/// Point the fixture clone's pushes at its own bare origin (or at `to`)
/// instead of the forge.
async fn push_to(f: &Fixture, to: Option<&str>) -> std::path::PathBuf {
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let clone = std::path::PathBuf::from(
        f.store
            .get_repo_by_id(finding.repo_id)
            .await
            .unwrap()
            .unwrap()
            .path,
    );
    let origin = support::git(&clone, &["remote", "get-url", "origin"]);
    let origin = origin.trim();
    support::git(
        &clone,
        &[
            "config",
            &format!("url.{}.insteadOf", to.unwrap_or(origin)),
            "git@github.com:acme/widget.git",
        ],
    );
    std::path::PathBuf::from(origin)
}

/// A worker that commits to the PR branch and writes `PR-REPLY.md`.
fn committing_and_replying() -> ScriptedBackend {
    ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("review.txt"), "addressed\n").unwrap();
        support::git(tree, &["add", "review.txt"]);
        support::git(tree, &["commit", "-m", "address review"]);
        std::fs::write(tree.join("PR-REPLY.md"), "done, see the new commit").unwrap();
        support::done()
    })
}

/// A finished engage publishes its commits to the PR branch and its reply
/// on the PR, then records the engagement: the PR counts as answered up to
/// the newest feedback the worker was shown (none here), the attention
/// reason is not marked addressed (the push changed the PR), and a one-shot
/// budget override is spent.
#[tokio::test]
async fn a_finished_engage_pushes_replies_and_records_it() {
    let f = fixture("engage-publish").await;
    let bins = FakeBins::acquire("engage-publish");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();
    let backend = committing_and_replying();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert_eq!(
        summary.kind,
        Some(hunter::domain::FindingJobKind::Engage.into())
    );
    assert_eq!(summary.finding_id, Some(f.fid));
    assert_eq!(summary.state, Some(hunter::domain::JobState::Done));
    assert_eq!(summary.pr_number, Some(7));
    assert_eq!(summary.tokens_new, Some(support::done().tokens_new));
    assert!(bins.called_with("gh", "comment"), "{:?}", bins.calls());
    assert_eq!(
        support::git(&origin, &["log", "-1", "--format=%s", BRANCH]).trim(),
        "address review"
    );
    assert_eq!(f.addressed_fp().await, None);
    let events = f.store.recent_events(20).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message == format!("#{} PR #7 engaged (pushed, replied)", f.fid)),
        "{events:?}"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.budget_override, None);
}

/// A reply without commits pushes nothing and marks the current attention
/// reason addressed, so the finding is not engaged again until the PR
/// changes. An exempt override stays.
#[tokio::test]
async fn a_reply_only_engage_marks_the_attention_addressed() {
    let f = fixture("engage-reply-only").await;
    let bins = FakeBins::acquire("engage-reply-only");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let head = support::git(&origin, &["rev-parse", BRANCH]);
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Exempt))
        .await
        .unwrap();
    let backend = ScriptedBackend::writing("PR-REPLY.md", "thanks, answered inline");

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert!(bins.called_with("gh", "comment"), "{:?}", bins.calls());
    assert_eq!(support::git(&origin, &["rev-parse", BRANCH]), head);
    assert_eq!(f.addressed_fp().await.as_deref(), Some(FP));
    let events = f.store.recent_events(20).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message == format!("#{} PR #7 engaged (replied)", f.fid)),
        "{events:?}"
    );
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        finding.budget_override,
        Some(hunter::domain::BudgetOverride::Exempt)
    );
}

// -- what counts as new feedback after an engage ------------------------------

/// Script `gh` as the PR now reads: open, nothing else wrong with it, and
/// `comments` (login, body) a day apart. `lead` can push, nobody else.
fn pr_with_comments(bins: &FakeBins, comments: &[(&str, String)]) {
    let comments: Vec<String> = comments
        .iter()
        .enumerate()
        .map(|(i, (login, body))| {
            format!(
                r#"{{"id":"IC_{i}","author":{{"login":"{login}"}},"body":{},"createdAt":"2026-01-0{}T00:00:00Z"}}"#,
                serde_json::to_string(body).unwrap(),
                i + 2
            )
        })
        .collect();
    bins.script(
        "gh",
        &format!(
            "case \"$*\" in\n\
             \x20 *collaborators/lead/permission*) echo true; exit 0 ;;\n\
             \x20 *permission*) echo false; exit 0 ;;\n\
             esac\n\
             cat <<'__PR_EOF__'\n\
             {{\"state\":\"OPEN\",\"mergeable\":\"MERGEABLE\",\"reviewDecision\":\"\",\"statusCheckRollup\":[],\
             \"updatedAt\":\"2026-01-09T00:00:00Z\",\"headRefName\":\"{BRANCH}\",\"headRefOid\":\"deadbeef\",\
             \"title\":\"a fix\",\"body\":\"because\",\"reviews\":[],\"comments\":[{}]}}\n\
             __PR_EOF__\nexit 0",
            comments.join(",")
        ),
    );
}

/// Run a reply-only engage against a PR showing `shown`, then sync the PR
/// as it reads afterwards; the attention that sync leaves.
async fn attention_after_reply(
    label: &str,
    shown: &[(&str, String)],
    afterwards: &[(&str, String)],
) -> Option<String> {
    let f = fixture(label).await;
    let bins = FakeBins::acquire(label);
    pr_with_comments(&bins, shown);
    push_to(&f, None).await;
    let backend = ScriptedBackend::writing("PR-REPLY.md", "answered");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();
    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");

    pr_with_comments(&bins, afterwards);
    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;
    assert_eq!(result.errors, 0, "{result:?}");
    f.attention().await
}

/// Feedback posted while the worker ran -- after it read the PR, before
/// its reply landed -- was never answered, so the next sync raises it.
/// The engagement used to be stamped "now" on a reply, which buried every
/// such comment for good.
#[tokio::test]
async fn feedback_posted_while_an_engage_runs_is_new_after_its_reply() {
    let attention = attention_after_reply(
        "engage-feedback-mid-run",
        &[],
        &[
            ("lead", "one more thing".to_owned()),
            ("lead", hunter::forge::mark_own("answered")),
        ],
    )
    .await;
    assert_eq!(attention.as_deref(), Some("new_comments"));
}

/// What the worker was shown is what its reply answers: that feedback is
/// not raised again, though the engage did not run at the time it was
/// posted.
#[tokio::test]
async fn feedback_the_worker_was_shown_is_answered_by_its_reply() {
    let asked = ("lead", "please rename it".to_owned());
    let attention = attention_after_reply(
        "engage-feedback-shown",
        std::slice::from_ref(&asked),
        &[asked.clone(), ("lead", hunter::forge::mark_own("renamed"))],
    )
    .await;
    assert_eq!(attention, None);
}

/// Hunter may post as the operator, so its reply and the operator's
/// comment can share a login: the mark `comment_pr` leaves on the reply is
/// what keeps hunter from answering itself, while the operator's comment
/// under the same login still counts.
#[tokio::test]
async fn hunters_own_reply_is_not_new_feedback_but_the_operators_comment_is() {
    for (body, flagged) in [
        (hunter::forge::mark_own("answered"), None),
        ("answered".to_owned(), Some("new_comments")),
    ] {
        let label = format!("engage-own-reply-{}", flagged.is_some());
        let attention = attention_after_reply(&label, &[], &[("lead", body.clone())]).await;
        assert_eq!(attention.as_deref(), flagged, "{body:?}");
    }
}

/// An engage worker stopped at the cap with a transcript; continued, it
/// replies on the PR.
fn suspend_then_reply() -> ScriptedBackend {
    ScriptedBackend::staged(|tree, resume_from| match resume_from {
        None => {
            let session = tree.parent().unwrap().join("session").join("engage.jsonl");
            std::fs::write(&session, format!("{USAGE}\n")).unwrap();
            hunter::types::RunResult {
                exit_code: None,
                killed_reason: Some("cap".to_owned()),
                session_file: Some(session.to_string_lossy().into_owned()),
                ..support::done()
            }
        }
        Some(file) => {
            std::fs::write(tree.join("PR-REPLY.md"), "answered").unwrap();
            hunter::types::RunResult {
                session_file: Some(file.to_string_lossy().into_owned()),
                ..support::done()
            }
        }
    })
}

/// A resumed worker is only told to carry on, so feedback that arrived
/// while its chain was suspended never reached it. Its reply must leave
/// that feedback new.
#[tokio::test]
async fn feedback_posted_while_an_engage_was_suspended_is_new_after_it_resumes() {
    let f = fixture("engage-feedback-suspended").await;
    let bins = FakeBins::acquire("engage-feedback-suspended");
    bins.ok("gh", PR_VIEW_JSON);
    push_to(&f, None).await;

    let summary = suspend_and_resume(&f, suspend_then_reply(), || {
        pr_with_comments(&bins, &[("lead", "one more thing".to_owned())]);
    })
    .await;
    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    assert!(bins.called_with("gh", "comment"), "{:?}", bins.calls());

    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;
    assert_eq!(result.errors, 0, "{result:?}");
    assert_eq!(f.attention().await.as_deref(), Some("new_comments"));
}

/// A resume inherits what its chain's first attempt was shown, so the
/// reply it finally posts answers that feedback even though the resumed
/// attempt never reads the PR.
#[tokio::test]
async fn feedback_a_resumed_worker_was_shown_is_answered_by_its_reply() {
    let f = fixture("engage-feedback-shown-resumed").await;
    let bins = FakeBins::acquire("engage-feedback-shown-resumed");
    let asked = ("lead", "please rename it".to_owned());
    pr_with_comments(&bins, std::slice::from_ref(&asked));
    push_to(&f, None).await;

    let summary = suspend_and_resume(&f, suspend_then_reply(), || {}).await;
    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");

    pr_with_comments(
        &bins,
        &[asked, ("lead", hunter::forge::mark_own("renamed"))],
    );
    let result = hunter::scheduler::sync_prs(&support::pr_sync(&f.store), &f.cfg).await;
    assert_eq!(result.errors, 0, "{result:?}");
    assert_eq!(f.attention().await, None);
}

/// A push that fails leaves the engage incomplete: no reply is posted, the
/// finished job is recorded failed, nothing is marked addressed or engaged
/// (so the next cycle tries again), and the attempt still spends a one-shot
/// override.
#[tokio::test]
async fn an_engage_whose_push_fails_posts_nothing_and_fails_its_job() {
    let f = fixture("engage-push-fails").await;
    let bins = FakeBins::acquire("engage-push-fails");
    bins.ok("gh", PR_VIEW_JSON);
    push_to(&f, Some("/nonexistent/origin.git")).await;
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();
    let backend = committing_and_replying();

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert!(
        summary
            .failure
            .as_deref()
            .is_some_and(|m| m.starts_with("push failed")),
        "{summary:?}"
    );
    assert!(!bins.called_with("gh", "comment"), "{:?}", bins.calls());
    let jobs = f.jobs().await;
    assert_eq!(jobs.last().unwrap().2, "failed", "{jobs:?}");
    let ps = f.pr_row().await;
    assert_eq!(ps.addressed_fingerprint, None);
    assert_eq!(ps.last_engaged_activity_at, None, "watermark untouched");
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(finding.budget_override, None);
}

/// A worker that did not finish publishes nothing, whatever it staged.
#[tokio::test]
async fn an_unfinished_engage_publishes_nothing() {
    let f = fixture("engage-unfinished").await;
    let bins = FakeBins::acquire("engage-unfinished");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let head = support::git(&origin, &["rev-parse", BRANCH]);
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("review.txt"), "half done\n").unwrap();
        support::git(tree, &["add", "review.txt"]);
        support::git(tree, &["commit", "-m", "half done"]);
        std::fs::write(tree.join("PR-REPLY.md"), "almost").unwrap();
        let mut result = support::done();
        result.exit_code = Some(1);
        result
    });

    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    let state = summary.state.unwrap();
    assert_ne!(state, hunter::domain::JobState::Done);
    assert_eq!(summary.failure, Some(format!("worker {state}")));
    assert!(!bins.called_with("gh", "comment"), "{:?}", bins.calls());
    assert_eq!(support::git(&origin, &["rev-parse", BRANCH]), head);
}

/// Only a suspended engage keeps its tree for the resume, and its event
/// says where; an engage that simply failed says nothing of the kind.
#[tokio::test]
async fn only_a_suspended_engage_reports_its_kept_worktree() {
    for suspended in [true, false] {
        let f = fixture("engage-kept-tree").await;
        let bins = FakeBins::acquire("engage-kept-tree");
        bins.ok("gh", PR_VIEW_JSON);
        let backend = ScriptedBackend::new(move |tree| {
            let mut result = support::done();
            if suspended {
                let session = tree.parent().unwrap().join("session/session.jsonl");
                std::fs::write(&session, format!("{USAGE}\n")).unwrap();
                result.exit_code = None;
                result.killed_reason = Some("cap".to_owned());
                result.session_file = Some(session.to_string_lossy().into_owned());
            } else {
                result.exit_code = Some(1);
            }
            result
        });

        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_engage(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &backend,
        )
        .await
        .unwrap();

        let (outcome, said) = if suspended {
            ("suspended", "suspended (")
        } else {
            ("retry", "incomplete (")
        };
        assert_eq!(summary.outcome.as_deref(), Some(outcome), "{summary:?}");
        assert_eq!(
            summary.state == Some(hunter::domain::JobState::Suspended),
            suspended,
            "{summary:?}"
        );
        let events = f.store.recent_events(20).await.unwrap();
        let incomplete = events
            .iter()
            .find(|e| e.message.starts_with(&format!("#{} {said}", f.fid)))
            .unwrap_or_else(|| panic!("{events:?}"));
        assert_eq!(
            incomplete.message.contains("; worktree kept at "),
            suspended,
            "{}",
            incomplete.message
        );
        drop(bins);
    }
}

// -- a resumed engage and the branch it pushes to ----------------------------

/// An engage worker that rewrites the PR's commit, as the playbook lets it,
/// and is stopped at the cap with a transcript; continued, it finishes.
fn rewrite_then_suspend() -> ScriptedBackend {
    ScriptedBackend::staged(|tree, resume_from| match resume_from {
        None => {
            std::fs::write(tree.join("CHANGE.md"), "change, as the reviewer asked\n").unwrap();
            support::git(
                tree,
                &["commit", "-a", "--amend", "-m", "change (reviewed)"],
            );
            let session = tree.parent().unwrap().join("session").join("engage.jsonl");
            std::fs::write(&session, format!("{USAGE}\n")).unwrap();
            hunter::types::RunResult {
                exit_code: None,
                killed_reason: Some("cap".to_owned()),
                session_file: Some(session.to_string_lossy().into_owned()),
                ..support::done()
            }
        }
        Some(file) => hunter::types::RunResult {
            session_file: Some(file.to_string_lossy().into_owned()),
            ..support::done()
        },
    })
}

/// Run the engage cold until it suspends, then continue it the way the
/// scheduler would; `meanwhile` runs between the two.
async fn suspend_and_resume(
    f: &Fixture,
    backend: ScriptedBackend,
    meanwhile: impl FnOnce(),
) -> hunter::scheduler::CycleSummary {
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let first = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();
    assert_eq!(
        first.state,
        Some(hunter::domain::JobState::Suspended),
        "{first:?}"
    );

    meanwhile();

    let plan = match hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(hunter::scheduler::Candidate::Resume { plan, .. }) => *plan,
        other => panic!("expected the suspended engage to be resumed, got {other:?}"),
    };
    assert_eq!(plan.predecessor_id(), first.job_id.unwrap());
    hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &backend,
    )
    .await
    .unwrap()
}

/// A resumed engage never overwrites commits pushed to the PR branch while
/// it was suspended.
///
/// A resume continues in the tree its chain was created in, at the PR head
/// of that moment, and nothing is fetched for it; a suspension can last
/// until the budget next admits the job. A reviewer's commit, or GitHub's
/// "Update branch" merge, pushed in between is not in that tree, so a
/// force push of it would delete that work from the PR without a trace.
/// The push is refused instead, and the attention stays unaddressed, so a
/// cold engage takes it up on the branch as it now is.
#[tokio::test]
async fn a_resumed_engage_keeps_commits_pushed_while_it_was_suspended() {
    let f = fixture("engage-resume-lease").await;
    let bins = FakeBins::acquire("engage-resume-lease");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let reviewer = TempDir::new("engage-resume-lease-reviewer");
    let checkout = reviewer.join("checkout");

    let summary = suspend_and_resume(&f, rewrite_then_suspend(), || {
        support::git(
            reviewer.path(),
            &[
                "clone",
                "-b",
                BRANCH,
                origin.to_string_lossy().as_ref(),
                checkout.to_string_lossy().as_ref(),
            ],
        );
        std::fs::write(checkout.join("REVIEWER.md"), "a reviewer's commit\n").unwrap();
        support::git(&checkout, &["add", "-A"]);
        support::git(&checkout, &["commit", "-m", "reviewer's suggestion"]);
        support::git(&checkout, &["push", "origin", BRANCH]);
    })
    .await;

    let theirs = support::git(&checkout, &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let branch = support::git(&origin, &["rev-list", BRANCH]);
    assert!(
        branch.lines().any(|c| c == theirs),
        "the reviewer's commit {theirs} must still be on {BRANCH}; it now holds:\n{branch}"
    );
    assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
    assert_eq!(
        f.addressed_fp().await,
        None,
        "an engage that published nothing has not addressed the attention"
    );
}

/// The happy path, so the test above is known to be asserting on a
/// difference rather than on a push that never happens: with nothing
/// pushed in between, the resumed engage publishes its tree's history,
/// rewritten commit and all.
#[tokio::test]
async fn a_resumed_engage_publishes_its_rewritten_history() {
    let f = fixture("engage-resume-push").await;
    let bins = FakeBins::acquire("engage-resume-push");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;

    let summary = suspend_and_resume(&f, rewrite_then_suspend(), || {}).await;

    assert_eq!(summary.outcome.as_deref(), Some("engaged"), "{summary:?}");
    let head = support::git(&origin, &["log", "-1", "--format=%s", BRANCH]);
    assert_eq!(head.trim(), "change (reviewed)");
}
// -- a suspended engage ------------------------------------------------------

/// A worker stopped by `killed_reason` after doing work, its transcript
/// in the chain's session directory: what `job_state` records as
/// `suspended` for both a cap kill and a wallclock kill that did work.
fn suspended_by(tree: &std::path::Path, killed_reason: &str) -> hunter::types::RunResult {
    let session = tree.parent().unwrap().join("session").join("engage.jsonl");
    std::fs::write(&session, format!("{USAGE}\n")).unwrap();
    hunter::types::RunResult {
        exit_code: None,
        killed_reason: Some(killed_reason.to_owned()),
        tokens_new: 30_000,
        calls: 3,
        session_file: Some(session.to_string_lossy().into_owned()),
        duration_s: 1.0,
        stdout_tail: "stopped".to_owned(),
        usage_delta: None,
    }
}

/// A wallclock-killed engage that had already withdrawn the PR concluded
/// its work: the finding is no longer `pr_open`, so no resume could ever
/// continue it. Its work item keeps no checkpoint -- the row stays as the
/// attempt ended, `wallclock` -- rather than one the resume tier offers
/// every cycle, and it still hands off into the closed PR's harvest in its
/// own session before the tree is released.
#[tokio::test]
async fn a_suspended_engage_that_withdrew_keeps_no_checkpoint_and_continues_into_its_harvest() {
    let f = fixture("engage-suspended-withdrew").await;
    let bins = FakeBins::acquire("engage-suspended-withdrew");
    gh_for_handoff(&bins);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::staged(|tree, resume_from| match resume_from {
        None => {
            std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
            suspended_by(tree, "wallclock")
        }
        Some(file) => {
            std::fs::write(
                tree.join("CLOSE-REASON.json"),
                r#"{"classification":"superseded","reason":"landed in #9","evidence":"abc1234"}"#,
            )
            .unwrap();
            hunter::types::RunResult {
                session_file: Some(file.to_string_lossy().into_owned()),
                ..support::done()
            }
        }
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("withdrawn"), "{summary:?}");
    assert_eq!(summary.state, Some(hunter::domain::JobState::Suspended));
    assert!(bins.called_with("gh", "close"), "{:?}", bins.calls());
    let engage_job = summary.job_id.unwrap();
    let (state, killed_reason, _) = f.job_row(engage_job).await;
    assert_eq!(
        (state.as_str(), killed_reason.as_deref()),
        ("suspended", Some("wallclock")),
        "the row says how the attempt ended"
    );

    let runs = backend.runs();
    assert_eq!(runs.len(), 2, "the harvest continues the engage: {runs:?}");
    let transcript = runs[0]
        .tree
        .parent()
        .unwrap()
        .join("session")
        .join("engage.jsonl");
    assert_eq!(runs[1].resume_from.as_deref(), Some(transcript.as_path()));
    let jobs = f.jobs().await;
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert_eq!(
        (jobs[1].1.as_str(), jobs[1].2.as_str(), jobs[1].3),
        ("harvest", "done", Some(engage_job))
    );

    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Superseded);
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(ps.harvested_at.is_some());
    assert!(
        f.store.list_resumable_jobs().await.unwrap().is_empty(),
        "nothing of the chain is left to resume"
    );
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");
}

/// A cap-killed engage whose withdrawal the forge refused keeps no
/// checkpoint either: the refusal is a failed engage, and a failure ends
/// the checkpoint (WORK-MODEL.md §6.3). A checkpoint left behind would be
/// resumed into the same withdrawal, and would pin its tree.
#[tokio::test]
async fn a_suspended_engage_whose_withdrawal_failed_keeps_no_checkpoint_and_is_released() {
    let f = fixture("engage-suspended-withdraw-failed").await;
    let bins = FakeBins::acquire("engage-suspended-withdraw-failed");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("WITHDRAW.md"), "not worth pursuing").unwrap();
        suspended_by(tree, "cap")
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        summary.outcome.as_deref(),
        Some("withdraw-failed"),
        "{summary:?}"
    );
    let job = summary.job_id.unwrap();
    let (state, killed_reason, _) = f.job_row(job).await;
    assert_eq!(
        (state.as_str(), killed_reason.as_deref()),
        ("suspended", Some("cap"))
    );
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        1,
        "counted as a failed engage"
    );
    assert!(f.store.list_resumable_jobs().await.unwrap().is_empty());
    let runs = backend.runs();
    assert_eq!(runs.len(), 1, "no harvest without a close: {runs:?}");
    assert_eq!(f.jobs().await.len(), 1);
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
    assert!(!runs[0].tree.exists(), "the chain's tree must be released");
}

/// A cap-killed engage that neither withdrew nor finished is a pause.
/// It committed and left a reply, but a suspended attempt's output is
/// unfinished: nothing is pushed (the failure is the suspension, not a
/// push error), no reply is posted, and the attention watermark is left
/// alone so the resume -- or the next cycle -- still acts on it. The job
/// stays `suspended` and resumable, not failed, and keeps its tree.
#[tokio::test]
async fn a_suspended_engage_that_did_not_withdraw_keeps_its_tree_for_the_resume() {
    let f = fixture("engage-suspended-retry").await;
    let bins = FakeBins::acquire("engage-suspended-retry");
    bins.ok("gh", PR_VIEW_JSON);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let backend = ScriptedBackend::new(|tree| {
        support::git(tree, &["commit", "--allow-empty", "-m", "half done"]);
        std::fs::write(tree.join("PR-REPLY.md"), "working on it").unwrap();
        suspended_by(tree, "cap")
    });

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("suspended"), "{summary:?}");
    assert_eq!(summary.failure, None, "a pause is not a failure");
    assert!(
        !bins.called_with("gh", "comment"),
        "a suspended attempt must not post its reply: {:?}",
        bins.calls()
    );
    let job = summary.job_id.unwrap();
    let (state, killed_reason, _) = f.job_row(job).await;
    assert_eq!(state, "suspended", "a pause is not failed");
    assert_eq!(killed_reason.as_deref(), Some("cap"));
    assert_eq!(
        f.store
            .list_resumable_jobs()
            .await
            .unwrap()
            .iter()
            .map(|j| j.id)
            .collect::<Vec<_>>(),
        vec![job]
    );

    assert_eq!(
        f.addressed_fp().await,
        None,
        "the attention stays unaddressed"
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert_eq!(ps.last_engaged_activity_at, None, "never engaged");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);

    let tree = &backend.runs()[0].tree;
    assert!(tree.is_dir(), "the suspended engage's tree is its resume");
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "engage"
            && e.message.contains(&format!(
                "worktree kept at {} for the resume",
                tree.display()
            ))),
        "{events:?}"
    );
}

/// An engage whose worker keeps failing the same way counts in the engage
/// streak, and the third failure in a row holds it for the operator. Until
/// the work model engage had no streak: the Err arm said `retry` and the
/// still-flagged PR was picked again every cycle, the highest finding tier
/// with no counter at all (#23).
#[tokio::test]
async fn a_failing_engage_is_held_at_the_streak_limit() {
    let f = fixture("engage-fails-thrice").await;
    let bins = FakeBins::acquire("engage-fails-thrice");
    bins.ok("gh", PR_VIEW_JSON);
    let failing = ScriptedBackend::new(|_| hunter::types::RunResult {
        exit_code: Some(1),
        ..done()
    });

    let mut outcomes = Vec::new();
    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_engage(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &failing,
        )
        .await
        .unwrap();
        outcomes.push(summary.outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("retry".into()),
            Some("held".into())
        ]
    );
    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held at the limit");
    assert!(hold.detail.starts_with("worker failed"), "{hold:?}");
    assert_eq!(
        hold.actions(),
        [
            hunter::domain::ReleaseAction::Restart,
            hunter::domain::ReleaseAction::Dismiss
        ]
    );
    let ps = f.store.get_pr_state(f.fid).await.unwrap().unwrap();
    assert!(
        ps.needs_attention.is_some(),
        "holding leaves the flag alone"
    );
}

/// An engage that fails before its worker runs — here the PR will not load
/// — is a failed engage too: counted, and held at the limit.
#[tokio::test]
async fn an_engage_whose_pr_will_not_load_is_held_at_the_streak_limit() {
    let f = fixture("engage-view-fails").await;
    let bins = FakeBins::acquire("engage-view-fails");
    bins.script("gh", "echo \"HTTP 502 at $(date +%s%N)\" >&2\nexit 1");
    let never = ScriptedBackend::new(|_| panic!("no worker without the PR"));

    for _ in 0..3 {
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let err = hunter::scheduler::run_engage(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never,
        )
        .await
        .expect_err("a PR that will not load fails the engage");
        assert!(err.to_string().contains("view failed"), "{err}");
    }

    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held at the limit");
    assert!(hold.detail.starts_with("PR/MR view failed"), "{hold:?}");
    assert_eq!(hold.checkpoint_job_id, None, "no worker ran");
}

/// A held engage stays held when its PR gets new activity: only the
/// operator releases it (WORK-MODEL.md §9.4). The activity is shown on the
/// hold, since it may well be what the hold waits for.
#[tokio::test]
async fn a_held_engage_stays_held_through_new_pr_activity() {
    let f = fixture("engage-held-activity").await;
    assert!(
        f.store
            .hold_work(
                f.fid,
                hunter::domain::FindingJobKind::Engage,
                hunter::domain::HoldReason::Stuck,
                "push failed",
                None
            )
            .await
            .unwrap()
    );
    let held = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .unwrap();
    assert!(!held.new_activity);

    // The reviewer answers: the next sync raises the PR's activity.
    let pass = support::pr_sync(&f.store);
    let reply = observed(&pass, f.fid, held.held_at + 1).await;
    let recorded = f.store.record_open_pr(f.fid, &reply).await.unwrap();
    drop(pass);
    assert!(
        matches!(recorded, OpenPrRecorded::Recorded(_)),
        "{recorded:?}"
    );

    let after = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("still held");
    assert!(after.new_activity);
    let picked = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(picked, Some(hunter::scheduler::Candidate::Finding { .. })),
        "{picked:?}"
    );
}

/// A paused engage is continued without asking the forge for the PR: the
/// view only feeds the playbook, which a resume does not send. A laptop
/// that wakes before its network is back used to fail that view, count it,
/// and drop the checkpoint with the worker's unpushed commits.
#[tokio::test]
async fn a_paused_engage_continues_without_the_forge() {
    let f = fixture("engage-resume-offline").await;
    let bins = FakeBins::acquire("engage-resume-offline");
    bins.ok("gh", PR_VIEW_JSON);
    let pausing = ScriptedBackend::new(|tree| suspended_by(tree, "cap"));
    let paused = f.run(&pausing).await.unwrap();
    assert_eq!(paused.outcome.as_deref(), Some("suspended"));
    bins.script("gh", "echo 'dial tcp: no route to host' >&2\nexit 1");
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("the paused engage resumes");
    };

    let resumed = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &pausing,
    )
    .await
    .unwrap();

    assert_eq!(resumed.outcome.as_deref(), Some("suspended"), "{resumed:?}");
    let checkpoint = f
        .store
        .checkpoint_job(f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .unwrap()
        .map(|j| j.id);
    assert_eq!(checkpoint, resumed.job_id);
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        0
    );
}

/// The streak counts *identical* failures, by what failed: two refused
/// pushes are the same failure whatever git says about each (the key drops
/// the text after the colon), while a push, a reply and an unfinished
/// worker are three different ones and hold nothing.
#[tokio::test]
async fn engage_failures_count_by_what_failed() {
    let f = fixture("engage-failure-keys").await;
    let bins = FakeBins::acquire("engage-failure-keys");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    // Pushes to a remote that refuses every one.
    push_to(&f, Some("/nonexistent/remote.git")).await;
    let pushing = ScriptedBackend::new(|tree| {
        git(tree, &["commit", "--allow-empty", "-qm", "address review"]);
        done()
    });
    let replying = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("PR-REPLY.md"), "answered").unwrap();
        done()
    });
    let unfinished = ScriptedBackend::new(|_| hunter::types::RunResult {
        exit_code: Some(1),
        ..done()
    });
    let streak = async || {
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
    };

    let mut seen = Vec::new();
    for backend in [&pushing, &pushing, &replying, &unfinished] {
        let summary = f.run(backend).await.unwrap();
        let s = streak().await;
        seen.push((summary.outcome, s.streak, s.last_failure));
    }

    let failure = |key: &str| Some(key.to_owned());
    assert_eq!(
        seen,
        [
            (Some("retry".into()), 1, failure("push failed")),
            (Some("retry".into()), 2, failure("push failed")),
            (Some("retry".into()), 1, failure("PR comment failed")),
            (Some("retry".into()), 1, failure("worker failed")),
        ]
    );
}

/// An engage that succeeds ends its streak: engage is the one kind whose
/// outcome does not leave its tier, so nothing else resets it. Fail,
/// succeed, fail, fail is a streak of two, not the three that hold.
#[tokio::test]
async fn a_successful_engage_ends_its_failure_streak() {
    let f = fixture("engage-success-clears").await;
    let bins = FakeBins::acquire("engage-success-clears");
    bins.ok("gh", PR_VIEW_JSON);
    let failing = ScriptedBackend::new(|_| hunter::types::RunResult {
        exit_code: Some(1),
        ..done()
    });
    let noop = ScriptedBackend::new(|_| done());

    let mut outcomes = Vec::new();
    for backend in [&failing, &noop, &failing, &failing] {
        outcomes.push(f.run(backend).await.unwrap().outcome);
    }

    assert_eq!(
        outcomes,
        [
            Some("retry".into()),
            Some("engaged".into()),
            Some("retry".into()),
            Some("retry".into()),
        ]
    );
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        2
    );
    assert_eq!(
        support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage).await,
        None
    );
}

/// A PR view that fails while the forge cannot be reached is the
/// machine's failure, not the engage's: nothing is counted, and the cycle
/// ends denied for the host, to be retried, as the network gate's own
/// denial does (#95). The same failure with the forge reachable is the
/// engage's, and counts.
#[tokio::test]
async fn a_failed_pr_view_counts_only_while_the_forge_is_reachable() {
    let bins = FakeBins::acquire("engage-view-offline");
    bins.script("gh", "echo 'connection reset' >&2\nexit 1");
    let host = support::FORGE_HOST;
    for counted in [false, true] {
        let mut f = fixture("engage-view-offline").await;
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE repos SET url = ?1")
            .bind(format!("https://{host}/acme/widget"))
            .execute(&pool)
            .await
            .unwrap();
        f.cfg.network_gate = true;
        let down: &[&str] = if counted { &[] } else { &[host] };
        let never = ScriptedBackend::new(|_| panic!("no worker without the PR")).offline(down);

        let result = f.run(&never).await;

        let streak = f
            .store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak;
        if counted {
            assert!(result.is_err(), "{result:?}");
            assert_eq!(streak, 1, "the forge answered: counted");
            continue;
        }
        let summary = result.unwrap();
        assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
        assert!(summary.retry_at.is_some(), "{summary:?}");
        assert_eq!(streak, 0, "the network's failure counts nothing");
    }
}

/// Pause the fixture's engage (the forge is answered), then point its repo
/// at a `forge` on `host` and turn the network gate
/// on, so failures are re-probed (#95 layer 2); the paused engage's resume.
/// A resume reads no PR view, so the move only shows where the engage
/// publishes. A GitLab repo's PR is a merge request, so the finding's
/// `pr_url` moves with it: the engage reads its PR's number from the URL.
async fn paused_then_served_at(
    f: &mut Fixture,
    forge: &str,
    host: &str,
) -> hunter::scheduler::ResumePlan {
    let paused = f
        .run(&ScriptedBackend::new(|tree| suspended_by(tree, "cap")))
        .await
        .unwrap();
    assert_eq!(paused.outcome.as_deref(), Some("suspended"), "{paused:?}");
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE repos SET url = ?1, forge = ?2")
        .bind(format!("https://{host}/acme/widget"))
        .bind(forge)
        .execute(&pool)
        .await
        .unwrap();
    if forge == "gitlab" {
        sqlx::query("UPDATE findings SET pr_url = ?1")
            .bind(format!("https://{host}/acme/widget/-/merge_requests/7"))
            .execute(&pool)
            .await
            .unwrap();
    }
    f.cfg.network_gate = true;
    match hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap()
    {
        Some(hunter::scheduler::Candidate::Resume { plan, .. }) => *plan,
        other => panic!("the paused engage resumes, got {other:?}"),
    }
}

/// A finished engage whose reply cannot be posted while the forge cannot be
/// reached is the network's failure, not the engage's: nothing is counted,
/// the attempt becomes the checkpoint, so its tree and the drafted
/// `PR-REPLY.md` are kept, and the next cycle continues it to post; the
/// cycle ends denied for the host (#95). The same failure with the forge
/// reachable is the engage's, and counts.
#[tokio::test]
async fn a_failed_reply_counts_only_while_the_forge_is_reachable() {
    let bins = FakeBins::acquire("engage-reply-offline");
    bins.ok("gh", PR_VIEW_JSON);
    let host = support::FORGE_HOST;
    for counted in [false, true] {
        let mut f = fixture("engage-reply-offline").await;
        // A GitHub repo on a host that is not GitHub's: the reply cannot be
        // posted.
        let plan = paused_then_served_at(&mut f, "github", host).await;
        let down: &[&str] = if counted { &[] } else { &[host] };
        let replying = ScriptedBackend::new(|tree| {
            std::fs::write(tree.join("PR-REPLY.md"), "answered").unwrap();
            support::done_with_transcript(tree)
        })
        .offline(down);

        let summary = hunter::scheduler::run_engage(
            &support::scope(&f.store),
            f.store.test_lane(),
            &f.cfg,
            Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
            &replying,
        )
        .await
        .unwrap();

        let streak = f
            .store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak;
        if counted {
            assert_eq!(summary.outcome.as_deref(), Some("retry"), "{summary:?}");
            assert_eq!(streak, 1, "the forge answered: counted");
            continue;
        }
        assert_eq!(summary.unreachable.as_deref(), Some(host), "{summary:?}");
        assert_eq!(streak, 0, "the network's failure counts nothing");
        let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
            hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
                .await
                .unwrap()
        else {
            panic!("the unpublished engage is continued");
        };
        assert_eq!(Some(plan.predecessor_id()), summary.job_id);
        assert!(
            plan.workspace().tree.join("PR-REPLY.md").exists(),
            "the reply is kept"
        );
    }
}

/// A withdrawal the forge cannot be reached to make is the network's
/// failure: nothing is counted, the withdrawing attempt becomes the
/// checkpoint, where a refused withdrawal drops it, and the cycle ends
/// denied for the host (#95).
#[tokio::test]
async fn a_withdrawal_while_the_forge_is_unreachable_keeps_its_checkpoint() {
    let bins = FakeBins::acquire("engage-withdraw-offline");
    bins.ok("gh", PR_VIEW_JSON);
    bins.fail("glab", 1, "dial tcp: connect: connection refused");
    let mut f = fixture("engage-withdraw-offline").await;
    let down = support::FORGE_HOST;
    // GitLab, which takes any host: the close is attempted, and fails.
    let plan = paused_then_served_at(&mut f, "gitlab", down).await;
    let withdrawing = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("WITHDRAW.md"), "superseded by #9").unwrap();
        support::done_with_transcript(tree)
    })
    .offline(&[down]);

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &withdrawing,
    )
    .await
    .unwrap();

    assert_eq!(summary.unreachable.as_deref(), Some(down), "{summary:?}");
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        0
    );
    assert_eq!(
        f.store
            .checkpoint_job(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .map(|j| j.id),
        summary.job_id,
        "the withdrawing attempt is kept to withdraw once the forge is back"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen);
}

/// An engage whose backend cannot run its worker, or whose tree cannot be
/// made, is a failed engage like any other (WORK-MODEL.md §7, G9): counted
/// under a fixed key, and held at the limit. Uncounted, the flagged PR
/// stayed in the engage tier, the highest of them, every cycle.
#[tokio::test]
async fn an_engage_that_never_reaches_its_worker_is_held() {
    for case in ["backend", "tree"] {
        let f = fixture(&format!("engage-no-worker-{case}")).await;
        let bins = FakeBins::acquire(&format!("engage-no-worker-{case}"));
        bins.ok("gh", PR_VIEW_JSON);
        let backend = if case == "backend" {
            ScriptedBackend::failing()
        } else {
            std::fs::write(f.cfg.work_root.join("jobs"), "not a directory").unwrap();
            ScriptedBackend::new(|_| panic!("nothing may run without a tree"))
        };
        for _ in 0..3 {
            let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
            let result = hunter::scheduler::run_engage(
                &support::scope(&f.store),
                f.store.test_lane(),
                &f.cfg,
                Attempt::Cold(&finding),
                &backend,
            )
            .await;
            match case {
                "backend" => assert!(result.is_err(), "{result:?}"),
                _ => assert!(
                    result
                        .unwrap()
                        .failure
                        .unwrap()
                        .starts_with("workspace not created"),
                    "{case}"
                ),
            }
        }
        let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap_or_else(|| panic!("{case}: held at the limit"));
        let key = if case == "backend" {
            "runner error"
        } else {
            "workspace not created"
        };
        assert!(hold.detail.starts_with(key), "{case}: {hold:?}");
        drop(bins);
    }
}

/// An engage whose feedback needs a maintainer's decision writes
/// `BLOCKED.md` (WORK-MODEL.md §9.1): it is held as a missing
/// prerequisite on the report, nothing is pushed or posted, and the PR's
/// attention stays unaddressed for the continue.
#[tokio::test]
async fn an_engage_that_needs_a_maintainer_is_held_on_its_report() {
    let f = fixture("engage-blocked").await;
    f.store
        .set_budget_override(f.fid, Some(hunter::domain::BudgetOverride::Once))
        .await
        .unwrap();
    let bins = FakeBins::acquire("engage-blocked");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let before = git(&origin, &["rev-parse", BRANCH]);
    let backend = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("BLOCKED.md"), "two reviewers want opposite APIs").unwrap();
        std::fs::write(tree.join("PR-REPLY.md"), "not posted").unwrap();
        git(tree, &["commit", "--allow-empty", "-qm", "not pushed"]);
        support::done_with_transcript(tree)
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("held"));
    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held on the report");
    assert_eq!(
        (hold.reason, hold.detail.as_str(), hold.checkpoint_job_id),
        (
            hunter::domain::HoldReason::Prerequisite,
            "two reviewers want opposite APIs",
            summary.job_id
        )
    );
    let calls = bins.calls_to("gh");
    assert!(
        calls.iter().all(|c| !c.iter().any(|a| a == "comment")),
        "nothing posted: {calls:?}"
    );
    assert_eq!(
        git(&origin, &["rev-parse", BRANCH]),
        before,
        "nothing pushed"
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.budget_override,
        Some(hunter::domain::BudgetOverride::Once),
        "a hold concluded nothing: the override is for the continue"
    );
    assert_eq!(f.addressed_fp().await, None, "attention left unaddressed");
}

/// Move the fixture's finding to `merged` straight in the database: what
/// the admin lane's PR sync writes when it sees the merge, done here
/// without a sync pass so the test can place it exactly.
async fn merge_finding(db: &Path, fid: i64) {
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(db))
            .await
            .unwrap();
    sqlx::query("UPDATE findings SET status = 'merged' WHERE id = ?1")
        .bind(fid)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// PR sync runs on the admin lane, beside the job lane, so it can record a
/// merge after an engage was picked and before its job goes in -- the
/// window holds the budget gate, a PR view and a fetch. The insert refuses
/// it under its write lock: the cycle is skipped, no job row is written,
/// and no worker runs against the PR that has ended.
#[tokio::test]
async fn an_engage_whose_pr_ended_after_the_pick_never_starts() {
    let f = fixture("engage-ended-before-insert").await;
    let bins = FakeBins::acquire("engage-ended-before-insert");
    bins.ok("gh", PR_VIEW_JSON);
    push_to(&f, None).await;
    let (db, fid) = (f.db.clone(), f.fid);
    let backend =
        ScriptedBackend::new(|_| panic!("no worker may run on an ended PR")).on_decide(move || {
            let db = db.clone();
            async move { merge_finding(&db, fid).await }
        });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .expect("a PR that ended after the pick is a skip, not the cycle's error");

    assert!(summary.skipped.is_some(), "a skip: {summary:?}");
    assert!(backend.runs().is_empty());
    assert!(
        f.store.jobs_by_finding(f.fid).await.unwrap().is_empty(),
        "a refused engage writes no job row"
    );
}

/// An engage picked from a `pr_open` row whose PR the forge already shows
/// merged -- the PR sync, on its own timer, has not recorded it yet, as
/// right after a restart -- runs no worker and writes no job. The PR's
/// end is the sync's to record, so the finding is left as it is, the
/// cycle asks the job lane to wait for a sync, and nothing counts against
/// the finding. Run, the worker would spend a session on a merged PR.
#[tokio::test]
async fn an_engage_whose_pr_the_forge_shows_merged_never_starts() {
    let f = fixture("engage-merged-on-forge").await;
    f.store
        .set_budget_override(f.fid, Some(BudgetOverride::Once))
        .await
        .unwrap();
    let bins = FakeBins::acquire("engage-merged-on-forge");
    // As `gh` answers: a PR's state only when the view asks for it.
    bins.script(
        "gh",
        r#"case "$*" in
  *"--json state"*|*",state"*) echo '{"state":"MERGED","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}' ;;
  *) echo '{"title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}' ;;
esac"#,
    );
    push_to(&f, None).await;
    let backend = ScriptedBackend::new(|_| panic!("no worker may run on a merged PR"));
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    let summary = hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .expect("a PR the forge has ended is a skip, not the cycle's error");

    assert!(summary.awaits_pr_sync, "{summary:?}");
    assert!(backend.runs().is_empty());
    assert!(
        f.store.jobs_by_finding(f.fid).await.unwrap().is_empty(),
        "no job row"
    );
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        0
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::PrOpen, "left for the sync");
    assert_eq!(
        after.budget_override, None,
        "the one-shot override is spent"
    );
}

/// `gh` answering every view with the PR merged.
fn gh_merged(bins: &FakeBins) {
    bins.ok(
        "gh",
        r#"{"state":"MERGED","title":"a fix","body":"because","comments":[],"reviews":[],"statusCheckRollup":[],"headRefName":"fix/some-bug","headRefOid":"deadbeef"}"#,
    );
}

/// The engage tier is the top one, and after a cycle that found its PR
/// ended on the forge the job lane waits for a PR sync pass and picks
/// again. If that pass cannot record the end (its view or its write keeps
/// failing for this one PR), the same engage is picked at once, views the
/// PR, waits for another pass, and so on for good, with no tier below it
/// ever running. Each such round counts against the engage, so the limit
/// holds it for the operator and the selection moves on.
#[tokio::test]
async fn an_ended_pr_the_sync_cannot_record_holds_its_engage() {
    let f = fixture("engage-ended-unrecorded").await;
    let bins = FakeBins::acquire("engage-ended-unrecorded");
    gh_merged(&bins);
    let never = ScriptedBackend::new(|_| panic!("no worker may run on a merged PR"));

    for _ in 0..3 {
        let scope = support::scope(&f.store);
        let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
        let summary = hunter::scheduler::run_engage(
            &scope,
            f.store.test_lane(),
            &f.cfg,
            Attempt::Cold(&finding),
            &never,
        )
        .await
        .unwrap();
        assert!(summary.awaits_pr_sync, "{summary:?}");
        // The pass the job lane waited for ran, and failed to record it.
        hunter::scheduler::count_unrecorded_pr_end(&scope, &summary).await;
    }

    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held at the limit");
    assert!(
        hold.detail
            .starts_with(hunter::scheduler::PR_END_UNRECORDED),
        "{hold:?}"
    );
    let next = hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
        .await
        .unwrap();
    assert!(
        !matches!(
            next,
            Some(hunter::scheduler::Candidate::Finding {
                kind: hunter::domain::FindingJobKind::Engage,
                ..
            })
        ),
        "the held engage is picked no more: {next:?}"
    );
}

/// A pass that did record the end leaves nothing to count, or to log: the
/// finding has left `pr_open`, the common case, and the engage is no
/// failure of its own.
#[tokio::test]
async fn an_ended_pr_the_sync_recorded_counts_nothing() {
    let f = fixture("engage-ended-recorded").await;
    let bins = FakeBins::acquire("engage-ended-recorded");
    gh_merged(&bins);
    let never = ScriptedBackend::new(|_| panic!("no worker may run on a merged PR"));
    let scope = support::scope(&f.store);
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();
    let summary = hunter::scheduler::run_engage(
        &scope,
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &never,
    )
    .await
    .unwrap();
    merge_finding(&f.db, f.fid).await;

    hunter::scheduler::count_unrecorded_pr_end(&scope, &summary).await;

    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        0
    );
    let logged = f.store.recent_events(50).await.unwrap();
    assert!(
        !logged
            .iter()
            .any(|e| e.message.contains(hunter::scheduler::PR_END_UNRECORDED)),
        "{logged:?}"
    );
}

/// A merge recorded while the engage's worker runs leaves its `BLOCKED.md`
/// nothing to hold: the finding has left the engage tier. That is the
/// attempt's outcome, logged, not the cycle's error, which would back
/// every other piece of work off for five minutes.
#[tokio::test]
async fn a_blocked_engage_whose_pr_ended_meanwhile_is_an_outcome() {
    let f = fixture("engage-blocked-ended").await;
    let bins = FakeBins::acquire("engage-blocked-ended");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let before = git(&origin, &["rev-parse", BRANCH]);
    let (db, fid) = (f.db.clone(), f.fid);
    let backend = ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("BLOCKED.md"), "two reviewers want opposite APIs").unwrap();
        merge_finding_from_worker(&db, fid);
        support::done_with_transcript(tree)
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .expect("BLOCKED.md on an ended PR is an outcome, not the cycle's error");

    assert!(
        support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .is_none(),
        "nothing is held for a finding out of the tier"
    );
    assert_eq!(
        git(&origin, &["rev-parse", BRANCH]),
        before,
        "nothing pushed"
    );
}

/// [`merge_finding`] from inside a worker: the worker runs inside the
/// runner's future, so the admin lane's write comes from a thread with a
/// runtime of its own.
fn merge_finding_from_worker(db: &Path, fid: i64) {
    let db = db.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(merge_finding(&db, fid));
    })
    .join()
    .unwrap();
}

/// A merge recorded while the engage's worker runs leaves it nothing to
/// publish: its commits are not pushed to the merged PR's branch and its
/// reply is not posted, and nothing is counted against the next PR's
/// engage. A one-shot override is spent, so the harvest does not jump the
/// queue on it.
#[tokio::test]
async fn an_engage_whose_pr_ended_while_its_worker_ran_publishes_nothing() {
    let f = fixture("engage-ended-mid-run").await;
    f.store
        .set_budget_override(f.fid, Some(BudgetOverride::Once))
        .await
        .unwrap();
    let bins = FakeBins::acquire("engage-ended-mid-run");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let before = git(&origin, &["rev-parse", BRANCH]);
    let (db, fid) = (f.db.clone(), f.fid);
    let backend = ScriptedBackend::new(move |tree| {
        std::fs::write(tree.join("PR-REPLY.md"), "addressed").unwrap();
        git(tree, &["commit", "--allow-empty", "-qm", "address review"]);
        merge_finding_from_worker(&db, fid);
        support::done_with_transcript(tree)
    });
    let finding = f.store.get_finding(f.fid).await.unwrap().unwrap();

    hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Cold(&finding),
        &backend,
    )
    .await
    .unwrap();

    assert_eq!(
        git(&origin, &["rev-parse", BRANCH]),
        before,
        "nothing pushed"
    );
    let calls = bins.calls_to("gh");
    assert!(
        calls.iter().all(|c| !c.iter().any(|a| a == "comment")),
        "nothing posted: {calls:?}"
    );
    assert_eq!(
        f.store
            .work_streak(f.fid, hunter::domain::FindingJobKind::Engage)
            .await
            .unwrap()
            .streak,
        0
    );
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(
        after.budget_override, None,
        "the one-shot override is spent"
    );
}

/// A `BLOCKED.md` holds the engage whatever state its attempt ended in: a
/// worker that asked for a maintainer and then lost its connection (a
/// suspension) asked all the same. It used to be read only from a worker
/// that finished; the resume deleted it as stale and pushed what the
/// worker had committed before blocking.
#[tokio::test]
async fn a_suspended_engage_that_wrote_blocked_is_held_and_pushes_nothing() {
    let f = fixture("engage-blocked-suspended").await;
    let bins = FakeBins::acquire("engage-blocked-suspended");
    bins.ok("gh", PR_VIEW_JSON);
    let origin = push_to(&f, None).await;
    let before = git(&origin, &["rev-parse", BRANCH]);
    let backend = ScriptedBackend::new(|tree| {
        git(tree, &["commit", "--allow-empty", "-qm", "half done"]);
        std::fs::write(tree.join("BLOCKED.md"), "needs a maintainer's call").unwrap();
        suspended_by(tree, "cap")
    });

    let summary = f.run(&backend).await.unwrap();

    assert_eq!(summary.outcome.as_deref(), Some("held"), "{summary:?}");
    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held on the report");
    assert_eq!(
        (hold.reason, hold.detail.as_str(), hold.checkpoint_job_id),
        (
            hunter::domain::HoldReason::Prerequisite,
            "needs a maintainer's call",
            summary.job_id
        )
    );
    assert_eq!(
        git(&origin, &["rev-parse", BRANCH]),
        before,
        "nothing pushed"
    );
}

/// The operator's `continue` on an engage held for repeated failures
/// resumes it with a prompt that says so, the failure as context
/// (WORK-MODEL.md §9.1), rather than the bare "you were interrupted".
#[tokio::test]
async fn a_continued_stuck_engage_resumes_with_its_reason() {
    let f = fixture("engage-continue").await;
    let bins = FakeBins::acquire("engage-continue");
    bins.ok_unless_action("gh", "pr comment", PR_VIEW_JSON);
    // Finished, with a transcript to continue, but its reply never lands.
    let failing = ScriptedBackend::new(|tree| {
        std::fs::write(tree.join("PR-REPLY.md"), "answered").unwrap();
        support::done_with_transcript(tree)
    });
    for _ in 0..3 {
        f.run(&failing).await.unwrap();
    }
    let hold = support::hold_of(&f.store, f.fid, hunter::domain::FindingJobKind::Engage)
        .await
        .expect("held at the limit");
    assert!(hold.checkpoint_job_id.is_some(), "{hold:?}");
    support::release(
        &f.store,
        f.fid,
        hunter::domain::FindingJobKind::Engage,
        hunter::domain::ReleaseAction::Continue,
    )
    .await;
    let Some(hunter::scheduler::Candidate::Resume { plan, .. }) =
        hunter::scheduler::pick_next(&support::scope(&f.store), &f.cfg, None)
            .await
            .unwrap()
    else {
        panic!("a continued hold resumes its checkpoint");
    };
    let resumed = ScriptedBackend::new(|_| done());

    hunter::scheduler::run_engage(
        &support::scope(&f.store),
        f.store.test_lane(),
        &f.cfg,
        Attempt::Resume(&Resumed::load(&f.store, &plan).await.unwrap()),
        &resumed,
    )
    .await
    .unwrap();

    let prompt = &resumed.runs()[0].prompt;
    assert!(prompt.contains("kept failing the same way"), "{prompt}");
    assert!(prompt.contains("PR comment failed"), "{prompt}");
}
