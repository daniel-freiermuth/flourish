#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `sync_prs` — the PR lifecycle state machine run every cycle.
//!
//! It is the only code that moves a finding out of `pr_open` and the only
//! code that records what the forge shows of an open PR, from which
//! `needs_attention` -- ranked second by `pick_next`, for engage work -- is
//! settled. Its decisions are what is pinned here, through the public
//! entry point and a scripted `gh pr view`:
//!
//! * merged / unparseable / failing-view / URL-less / repo-gone findings;
//! * the first-sync watermark baseline, so our own PR chatter is not news;
//! * static reasons, their fingerprint, and the suppression an engage
//!   decline buys — lifted by a push or by a changed reason;
//! * `attention_since` moving only when the reason string changes;
//! * how forge timestamps and check rollups are read (`iso_ms`,
//!   `checks_summary`), observed through the columns they feed.
//!
//! Closed PRs are covered in `runner_engage_test.rs`, next to the
//! withdrawal path that produces most of them.

mod support;

use hunter::attention::{
    AnswerRecorded, Declined, EngageAnswer, ObservedFixture, ObservedOpenPr, OpenPrRecorded,
    ProblemsAnswer,
};
use hunter::config::Config;
use hunter::domain::{FindingStatus, ForgeName};
use hunter::forge::Mergeable;
use hunter::store::{FindingInsert, RecordedPrState, SeenActivity, Store};
use serde_json::{Value, json};
use support::{FakeBins, TempDir, fresh_store, pr_ref, pr_sync};

const PR_URL: &str = "https://github.com/acme/widget/pull/7";
const REPO_URL: &str = "https://github.com/acme/widget";
const BRANCH: &str = "fix/some-bug";
const SHA: &str = "deadbeef";

/// `updatedAt` of every scripted PR unless a test overrides it.
const UPDATED: &str = "2026-01-02T03:04:05Z";
/// [`UPDATED`] as epoch ms.
const UPDATED_MS: i64 = 1_767_323_045_000;
/// `2026-01-02T04:00:00Z` as epoch ms: activity after [`UPDATED`].
const LATER_MS: i64 = 1_767_326_400_000;

struct Fixture {
    cfg: Config,
    db: std::path::PathBuf,
    store: Store,
    fid: i64,
    /// Last: fields drop in declaration order, and the directory has to
    /// outlive the Store's SQLite pool (the rule `fresh_db` documents).
    _dir: TempDir,
}

impl Fixture {
    async fn sync(&self) -> hunter::scheduler::SyncResult {
        hunter::scheduler::sync_prs(&pr_sync(&self.store), &self.cfg).await
    }

    async fn status(&self) -> FindingStatus {
        self.store
            .get_finding(self.fid)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn pr_state(&self) -> RecordedPrState {
        self.store
            .get_pr_state(self.fid)
            .await
            .unwrap()
            .expect("sync_prs wrote a pr_state row")
    }

    /// A `pr_state` row as earlier syncs and an engage would have left it:
    /// the PR's problems observed at `at`, and feedback up to `engaged`
    /// answered.
    async fn seed(&self, mergeable: Mergeable, changes_requested: bool, engaged: i64, at: i64) {
        let observed = |last_activity_at| ObservedFixture {
            mergeable,
            changes_requested,
            checks: None,
            failing_checks: Vec::new(),
            head_ref: BRANCH.to_owned(),
            head_sha: SHA.to_owned(),
            last_activity_at,
        };
        // The feedback arrives only once it is answered, so the problems'
        // attention dates from `at`, not from the engage. Each sync of the
        // two is its own pass.
        let pass = pr_sync(&self.store);
        let pr = pr_ref(PR_URL);
        let read = pass
            .begin_read_at(self.fid, &pr, at)
            .await
            .unwrap()
            .unwrap();
        let first = ObservedOpenPr::fixture(read, observed(0));
        let first = self.store.record_open_pr(self.fid, &first).await.unwrap();
        drop(pass);
        assert!(matches!(first, OpenPrRecorded::Recorded(_)), "{first:?}");
        let answered = self
            .store
            .mark_pr_engaged(
                self.fid,
                &EngageAnswer::fixture(
                    &pr,
                    Some(SeenActivity::fixture(engaged)),
                    ProblemsAnswer::NoneRecorded,
                ),
            )
            .await
            .unwrap();
        assert!(
            matches!(answered, AnswerRecorded::Recorded(_)),
            "{answered:?}"
        );
        let pass = pr_sync(&self.store);
        let read = pass
            .begin_read_at(self.fid, &pr, at)
            .await
            .unwrap()
            .unwrap();
        let second = ObservedOpenPr::fixture(read, observed(engaged));
        let second = self.store.record_open_pr(self.fid, &second).await.unwrap();
        assert!(matches!(second, OpenPrRecorded::Recorded(_)), "{second:?}");
    }

    /// What an engage cycle records when it replied but pushed nothing:
    /// the problems the row records declined, on the head it records.
    async fn decline_what_was_read(&self) {
        let ps = self.pr_state().await;
        assert!(
            Declined::as_read(&ps).is_some(),
            "a sync recorded problems on a head"
        );
        let pr = pr_ref(PR_URL);
        let answered = self
            .store
            .mark_pr_engaged(self.fid, &EngageAnswer::without_push(&pr, None, &ps))
            .await
            .unwrap();
        assert!(
            matches!(answered, AnswerRecorded::Recorded(_)),
            "{answered:?}"
        );
    }
}

/// A GitHub repo and one `pr_open` finding pointing at [`PR_URL`].
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let (db, store) = fresh_store(&dir, "sync").await;
    let repos_root = dir.subdir("repos");
    let rid = store
        .add_repo("widget", REPO_URL, &repos_root, "main", ForgeName::Github)
        .await
        .unwrap();
    let (fid, _) = store
        .upsert_finding(
            rid,
            &FindingInsert {
                fingerprint: "fp-sync-1".to_owned(),
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
    let mut cfg = support::load_config(dir.path());
    cfg.work_root = dir.subdir("work_root");
    Fixture {
        cfg,
        db,
        store,
        fid,
        _dir: dir,
    }
}

/// `gh pr view --json` for an untouched open PR, with `overrides` applied.
fn pr(overrides: &Value) -> String {
    let mut v = json!({
        "state": "OPEN",
        "mergeable": "MERGEABLE",
        "reviewDecision": "",
        "comments": [],
        "reviews": [],
        "statusCheckRollup": [],
        "updatedAt": UPDATED,
        "headRefName": BRANCH,
        "headRefOid": SHA,
    });
    for (k, x) in overrides.as_object().unwrap() {
        v[k] = x.clone();
    }
    v.to_string()
}

/// Script `gh`: `view` for the PR view, and push permission for whoever
/// commented. `sync_prs` asks the forge whether each comment's author can
/// push (`forge::github_can_push`), and only then counts the comment as
/// feedback; answering every call with the view would fail that lookup and
/// the sync with it.
fn serve_pr(bins: &FakeBins, view: &str) {
    bins.script(
        "gh",
        &format!(
            "case \"$*\" in\n\
             \x20 *collaborators/*/permission*) echo true; exit 0 ;;\n\
             esac\n\
             cat <<'__PR_EOF__'\n{view}\n__PR_EOF__\nexit 0"
        ),
    );
}

fn comment_at(created_at: &str) -> Value {
    json!([{ "author": { "login": "reviewer" }, "body": "hm", "createdAt": created_at }])
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A pass reads its open PRs side by side, but never more than
/// `PR_SYNC_CONCURRENCY` at once: each `gh pr view` is a forge round trip,
/// and a serial pass cost their sum. Every PR is still synced once.
///
/// The fake `gh` marks itself live in `live/` for half a second and
/// records how many reads were live when it started. Serial reads would
/// each see only themselves.
#[tokio::test]
async fn a_pass_reads_its_open_prs_side_by_side_within_the_bound() {
    const PRS: i64 = 7;
    let f = fixture("sync-parallel").await;
    let bins = FakeBins::acquire("sync-parallel");
    let rid = f.store.get_finding(f.fid).await.unwrap().unwrap().repo_id;
    for n in 2..=PRS {
        let (fid, _) = f
            .store
            .upsert_finding(
                rid,
                &FindingInsert {
                    fingerprint: format!("fp-sync-{n}"),
                    file: "src/lib.rs".to_owned(),
                    summary: "a fix".to_owned(),
                    ..Default::default()
                },
                "bug",
                None,
            )
            .await
            .unwrap();
        f.store
            .set_finding_pr_open(fid, &format!("{REPO_URL}/pull/{}", 100 + n))
            .await
            .unwrap();
    }
    let live = f.cfg.work_root.join("live");
    let seen = f.cfg.work_root.join("seen");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&seen).unwrap();
    bins.script(
        "gh",
        &format!(
            "touch '{live}/'$$\n\
             ls '{live}' | wc -l > '{seen}/'$$\n\
             sleep 0.5\n\
             rm -f '{live}/'$$\n\
             cat <<'__PR_EOF__'\n{view}\n__PR_EOF__\nexit 0",
            live = live.display(),
            seen = seen.display(),
            view = pr(&json!({})),
        ),
    );

    let result = f.sync().await;

    assert_eq!((result.synced, result.errors), (PRS, 0), "{result:?}");
    assert!(result.complete, "a pass that read every PR");
    let overlaps: Vec<usize> = std::fs::read_dir(&seen)
        .unwrap()
        .map(|e| {
            std::fs::read_to_string(e.unwrap().path())
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    assert_eq!(
        overlaps.len(),
        usize::try_from(PRS).unwrap(),
        "one read per PR"
    );
    let most = overlaps.iter().copied().max().unwrap();
    assert!(most >= 2, "the reads ran one at a time: {overlaps:?}");
    assert!(
        most <= hunter::scheduler::PR_SYNC_CONCURRENCY,
        "more than the bound at once: {overlaps:?}"
    );
}

// ---------------------------------------------------------------------------
// Leaving pr_open, and the paths that must not
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_merged_pr_moves_its_finding_to_merged_and_drops_attention() {
    let f = fixture("sync-merged").await;
    f.seed(Mergeable::Conflicting, false, 1, 1).await;
    let bins = FakeBins::acquire("sync-merged");
    serve_pr(&bins, &pr(&json!({ "state": "MERGED" })));

    let result = f.sync().await;

    assert_eq!(
        (
            result.merged,
            result.synced,
            result.attention,
            result.errors
        ),
        (1, 0, 0, 0),
        "{result:?}"
    );
    assert_eq!(f.status().await, FindingStatus::Merged);
    let ps = f.pr_state().await;
    assert_eq!(ps.state.as_deref(), Some("MERGED"));
    assert_eq!(ps.needs_attention, None, "a merged PR needs nobody");
}

/// A merge whose PR record cannot be written leaves the finding
/// `pr_open`: the move and the record are one write. Set `merged` over a
/// `pr_state` that never says MERGED, the finding would be out of the
/// sync's reach (it revisits only `pr_open`) with no merged PR for its
/// harvest. Left `pr_open`, the next sync records both.
#[tokio::test]
async fn a_merge_that_cannot_be_recorded_is_retried_by_the_next_sync() {
    let f = fixture("sync-merged-unrecorded").await;
    let bins = FakeBins::acquire("sync-merged-unrecorded");
    serve_pr(&bins, &pr(&json!({ "state": "MERGED" })));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    for trigger in [
        "CREATE TRIGGER no_merge_insert BEFORE INSERT ON pr_state \
         WHEN NEW.state = 'MERGED' \
         BEGIN SELECT RAISE(ABORT, 'injected pr_state failure'); END",
        "CREATE TRIGGER no_merge_update BEFORE UPDATE OF state ON pr_state \
         WHEN NEW.state = 'MERGED' \
         BEGIN SELECT RAISE(ABORT, 'injected pr_state failure'); END",
    ] {
        sqlx::query(trigger).execute(&pool).await.unwrap();
    }

    let result = f.sync().await;

    assert_eq!((result.merged, result.errors), (0, 1), "{result:?}");
    assert_eq!(f.status().await, FindingStatus::PrOpen);
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("PR merged but not recorded")),
        "{events:?}"
    );

    sqlx::raw_sql("DROP TRIGGER no_merge_insert; DROP TRIGGER no_merge_update")
        .execute(&pool)
        .await
        .unwrap();
    let result = f.sync().await;

    assert_eq!(result.merged, 1, "{result:?}");
    assert_eq!(f.status().await, FindingStatus::Merged);
    assert_eq!(f.pr_state().await.state.as_deref(), Some("MERGED"));
}

/// An open PR whose observation cannot be recorded is a sync error, not a
/// synced PR: nothing about it was stored, so nothing was settled from it.
/// The next sync records it.
#[tokio::test]
async fn an_open_pr_that_cannot_be_recorded_is_an_error_and_retried() {
    let f = fixture("sync-open-unrecorded").await;
    let bins = FakeBins::acquire("sync-open-unrecorded");
    serve_pr(&bins, &pr(&json!({})));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER no_open_insert BEFORE INSERT ON pr_state \
         BEGIN SELECT RAISE(ABORT, 'injected pr_state failure'); END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = f.sync().await;

    assert_eq!((result.synced, result.errors), (0, 1), "{result:?}");
    assert!(f.store.get_pr_state(f.fid).await.unwrap().is_none());

    sqlx::raw_sql("DROP TRIGGER no_open_insert")
        .execute(&pool)
        .await
        .unwrap();
    let result = f.sync().await;

    assert_eq!((result.synced, result.errors), (1, 0), "{result:?}");
    assert_eq!(f.pr_state().await.state.as_deref(), Some("OPEN"));
}

/// A PR this pass has already read is not read again: counted as an error
/// and left `pr_open`, without asking the forge, so a second observation
/// can never land from the same pass.
#[tokio::test]
async fn a_pr_already_read_in_the_pass_is_an_error_and_not_read_again() {
    let f = fixture("sync-read-twice").await;
    let bins = FakeBins::acquire("sync-read-twice");
    serve_pr(&bins, &pr(&json!({})));
    let pass = pr_sync(&f.store);
    let earlier = pass.begin_read(f.fid, &pr_ref(PR_URL)).await.unwrap();
    assert!(earlier.is_some());

    let result = hunter::scheduler::sync_prs(&pass, &f.cfg).await;

    assert_eq!((result.errors, result.synced), (1, 0), "{result:?}");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    assert_eq!(f.status().await, FindingStatus::PrOpen);
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.message.contains("already read in this pass")),
        "{events:?}"
    );
}

/// A pass asked to stop reads no further PR: a daemon shutting down waits
/// for its PR sync's run, and each PR is a forge round trip. The finding is
/// left as it was, for the next daemon's first sync.
#[tokio::test]
async fn a_pass_told_to_stop_reads_no_more_prs() {
    let f = fixture("sync-stop").await;
    let bins = FakeBins::acquire("sync-stop");
    serve_pr(&bins, &pr(&json!({"state": "MERGED"})));
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let pass = pr_sync(&f.store).until(stopping);
    stop.send_replace(true);

    let result = hunter::scheduler::sync_prs(&pass, &f.cfg).await;

    assert_eq!((result.synced, result.merged, result.errors), (0, 0, 0));
    assert!(!result.complete, "a stopped pass read every PR");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    assert_eq!(f.status().await, FindingStatus::PrOpen);
}

/// A read whose push count cannot be taken is never begun: an error, the
/// forge not asked, the finding left `pr_open` for the next sync.
#[tokio::test]
async fn a_read_that_cannot_be_begun_is_an_error_and_retried() {
    let f = fixture("sync-read-unbegun").await;
    let bins = FakeBins::acquire("sync-read-unbegun");
    serve_pr(&bins, &pr(&json!({})));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("ALTER TABLE pr_state RENAME TO pr_state_away")
        .execute(&pool)
        .await
        .unwrap();

    let result = f.sync().await;

    assert_eq!((result.errors, result.synced), (1, 0), "{result:?}");
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    assert_eq!(f.status().await, FindingStatus::PrOpen);

    sqlx::query("ALTER TABLE pr_state_away RENAME TO pr_state")
        .execute(&pool)
        .await
        .unwrap();
    let result = f.sync().await;

    assert_eq!((result.errors, result.synced), (0, 1), "{result:?}");
}

/// A finding something else moved off `pr_open` while its PR was being
/// viewed keeps that move: the sync neither overwrites it nor records the
/// PR merged for it. The other write is staged with a trigger that lands
/// it just before the sync's and then skips the sync's, exactly as if it
/// had landed in between.
#[tokio::test]
async fn a_finding_moved_off_pr_open_during_the_sync_keeps_that_move() {
    let f = fixture("sync-merged-moved").await;
    let bins = FakeBins::acquire("sync-merged-moved");
    serve_pr(&bins, &pr(&json!({ "state": "MERGED" })));
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query(
        "CREATE TRIGGER moved_first BEFORE UPDATE OF status ON findings \
         WHEN NEW.status = 'merged' \
         BEGIN \
           UPDATE findings SET status = 'closed', verdict_reason = 'withdrawn' \
             WHERE id = OLD.id; \
           SELECT RAISE(IGNORE); \
         END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let result = f.sync().await;

    assert_eq!((result.merged, result.errors), (0, 0), "{result:?}");
    let after = f.store.get_finding(f.fid).await.unwrap().unwrap();
    assert_eq!(after.status, FindingStatus::Closed);
    assert_eq!(after.verdict_reason.as_deref(), Some("withdrawn"));
    let ps = f.store.get_pr_state(f.fid).await.unwrap();
    assert!(
        ps.as_ref()
            .is_none_or(|p| p.state.as_deref() != Some("MERGED")),
        "{ps:?}"
    );
}

/// `pr_open` without a URL is a fix that failed before opening its PR:
/// requeued for another attempt, not counted as a sync error, and never
/// sent to the forge.
#[tokio::test]
async fn a_pr_open_finding_without_a_url_is_requeued() {
    let f = fixture("sync-no-url").await;
    f.store.set_finding_pr_open(f.fid, "").await.unwrap();
    let bins = FakeBins::acquire("sync-no-url");
    serve_pr(&bins, &pr(&json!({})));

    let result = f.sync().await;

    assert_eq!(
        (result.errors, result.synced, result.requeued),
        (0, 0, 1),
        "{result:?}"
    );
    assert!(
        result.made_work(),
        "a requeued fix is work for the job lane"
    );
    assert_eq!(f.status().await, FindingStatus::Queued);
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
}

#[tokio::test]
async fn an_unparseable_pr_url_is_an_error_and_stays_pr_open() {
    let f = fixture("sync-bad-url").await;
    f.store
        .set_finding_pr_open(f.fid, "https://github.com/acme/widget/issues/7")
        .await
        .unwrap();
    let bins = FakeBins::acquire("sync-bad-url");
    serve_pr(&bins, &pr(&json!({})));

    let result = f.sync().await;

    assert_eq!((result.errors, result.synced), (1, 0), "{result:?}");
    assert_eq!(f.status().await, FindingStatus::PrOpen);
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("unparseable pr_url")),
        "{events:?}"
    );
}

/// A forge outage must leave the finding where the next sync can retry it.
#[tokio::test]
async fn a_failing_pr_view_is_an_error_and_stays_pr_open() {
    let f = fixture("sync-view-fails").await;
    let bins = FakeBins::acquire("sync-view-fails");
    bins.fail("gh", 1, "HTTP 502: Bad Gateway");

    let result = f.sync().await;

    assert_eq!((result.errors, result.synced), (1, 0), "{result:?}");
    assert_eq!(f.status().await, FindingStatus::PrOpen);
    assert!(f.store.get_pr_state(f.fid).await.unwrap().is_none());
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("PR view failed")),
        "{events:?}"
    );
}

/// A repo soft-deleted while one of its findings still sits in `pr_open`.
/// `soft_delete_repo` refuses a repo with findings, so the row is marked
/// directly — the state a removal racing the cycle leaves behind. The
/// finding must stay where a later sync can reach it, without a forge call
/// for a repo the daemon no longer knows.
#[tokio::test]
async fn a_finding_whose_repo_is_gone_is_an_error_and_stays_pr_open() {
    let f = fixture("sync-repo-gone").await;
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&f.db))
            .await
            .unwrap();
    sqlx::query("UPDATE repos SET deleted_at = 1")
        .execute(&pool)
        .await
        .unwrap();
    let bins = FakeBins::acquire("sync-repo-gone");
    serve_pr(&bins, &pr(&json!({ "state": "MERGED" })));

    let result = f.sync().await;

    assert_eq!(
        (result.errors, result.synced, result.merged),
        (1, 0, 0),
        "{result:?}"
    );
    assert_eq!(f.status().await, FindingStatus::PrOpen);
    assert!(bins.calls_to("gh").is_empty(), "{:?}", bins.calls());
    assert!(f.store.get_pr_state(f.fid).await.unwrap().is_none());
    let events = f.store.recent_events(50).await.unwrap();
    assert!(
        events.iter().any(|e| e.kind == "error"
            && e.finding_id == Some(f.fid)
            && e.message.contains("repo")
            && e.message.contains("missing")),
        "{events:?}"
    );
}

// ---------------------------------------------------------------------------
// The engaged watermark
// ---------------------------------------------------------------------------

/// The first sync has no watermark yet, and nothing has answered what is
/// already on the PR: a review left before we ever looked is new. Hunter's
/// own comments carry its mark and are no activity at all, so none has to
/// be baselined away -- doing that hid such reviews for good.
#[tokio::test]
async fn the_first_sync_flags_feedback_already_on_the_pr() {
    let f = fixture("sync-first-baseline").await;
    let bins = FakeBins::acquire("sync-first-baseline");
    serve_pr(
        &bins,
        &pr(&json!({
            "comments": [{
                "author": { "login": "reviewer" },
                "body": hunter::forge::mark_own("hunter's description"),
                "createdAt": "2026-01-02T04:00:00Z",
            }],
            "reviews": [{
                "author": { "login": "reviewer" },
                "body": "",
                "state": "COMMENTED",
                "submittedAt": "2026-01-02T03:30:00Z",
            }],
        })),
    );

    let result = f.sync().await;

    assert_eq!((result.synced, result.attention), (1, 1), "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.last_activity_at, Some(LATER_MS - 30 * 60 * 1000));
    assert_eq!(
        ps.last_engaged_activity_at, None,
        "sync writes no engage watermark"
    );
    assert_eq!(ps.needs_attention.as_deref(), Some("new_comments"));
}

#[tokio::test]
async fn activity_after_the_watermark_is_new_comments() {
    let f = fixture("sync-new-comments").await;
    f.seed(Mergeable::Mergeable, false, UPDATED_MS, 1).await;
    let bins = FakeBins::acquire("sync-new-comments");
    serve_pr(
        &bins,
        &pr(&json!({ "comments": comment_at("2026-01-02T04:00:00Z") })),
    );
    let before = now_ms();

    let result = f.sync().await;

    assert_eq!((result.synced, result.attention), (1, 1), "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.needs_attention.as_deref(), Some("new_comments"));
    assert_eq!(
        ps.last_engaged_activity_at,
        Some(UPDATED_MS),
        "only an engage cycle advances the watermark"
    );
    assert!(ps.attention_since.unwrap() >= before, "{ps:?}");
    // Comments are not a static reason, so nothing is fingerprinted.
    assert_eq!(ps.attention_fingerprint, None);
}

// ---------------------------------------------------------------------------
// Static reasons and their fingerprint
// ---------------------------------------------------------------------------

/// Every static reason at once, with a rollup mixing `CheckRun` entries
/// (`name`/`conclusion`) and legacy `StatusContext` entries
/// (`context`/`state`), a check failing twice under one name, and one
/// still running.
#[tokio::test]
async fn static_reasons_are_flagged_and_fingerprinted() {
    let f = fixture("sync-static").await;
    let bins = FakeBins::acquire("sync-static");
    serve_pr(
        &bins,
        &pr(&json!({
            "reviewDecision": "CHANGES_REQUESTED",
            "mergeable": "CONFLICTING",
            "statusCheckRollup": [
                { "name": "lint", "conclusion": "FAILURE" },
                { "name": "lint", "conclusion": "TIMED_OUT" },
                { "name": "build", "conclusion": "SUCCESS" },
                { "context": "ci/legacy", "state": "FAILURE" },
                { "name": "deploy", "conclusion": "" },
            ],
        })),
    );

    let result = f.sync().await;

    assert_eq!((result.synced, result.attention), (1, 1), "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(
        ps.needs_attention.as_deref(),
        Some("changes_requested,conflict,checks_failing")
    );
    // Failing names deduplicated and sorted; counts are per entry.
    assert_eq!(
        ps.attention_fingerprint.as_deref(),
        Some("review:CHANGES_REQUESTED|mergeable:CONFLICTING|checks:ci/legacy,lint")
    );
    assert_eq!(ps.checks.as_deref(), Some("1 pass / 3 fail / 1 pending"));
}

#[tokio::test]
async fn passing_or_absent_checks_raise_nothing() {
    let f = fixture("sync-checks-clean").await;
    let bins = FakeBins::acquire("sync-checks-clean");

    serve_pr(&bins, &pr(&json!({})));
    f.sync().await;
    let ps = f.pr_state().await;
    assert_eq!(ps.checks, None, "no rollup is no summary, not \"0 pass\"");
    assert_eq!(ps.attention_fingerprint, None);
    assert_eq!(ps.needs_attention, None);

    serve_pr(
        &bins,
        &pr(&json!({
            "statusCheckRollup": [
                { "name": "build", "conclusion": "SUCCESS" },
                { "context": "ci/legacy", "state": "PENDING" },
            ],
        })),
    );
    let result = f.sync().await;
    assert_eq!(result.attention, 0, "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.checks.as_deref(), Some("1 pass / 1 pending"));
    assert_eq!(ps.attention_fingerprint, None);
    assert_eq!(ps.needs_attention, None);

    serve_pr(
        &bins,
        &pr(&json!({
            "statusCheckRollup": [
                { "name": "build", "conclusion": "SUCCESS" },
                { "context": "ci/legacy", "state": "SUCCESS" },
            ],
        })),
    );
    f.sync().await;
    let ps = f.pr_state().await;
    assert_eq!(
        ps.checks.as_deref(),
        Some("2 pass"),
        "nothing pending, none listed"
    );
    assert_eq!(ps.needs_attention, None);
}

// ---------------------------------------------------------------------------
// Suppression after an engage decline
// ---------------------------------------------------------------------------

const CHANGES_REQUESTED_FP: &str = "review:CHANGES_REQUESTED";

/// An engage cycle that replied without pushing has had its shot at this
/// exact reason on this exact head; flagging it again would run a fresh
/// worker on the same declined request every cycle.
#[tokio::test]
async fn a_declined_reason_is_not_reflagged_on_the_same_head() {
    let f = fixture("sync-suppressed").await;
    let bins = FakeBins::acquire("sync-suppressed");
    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED" })),
    );
    f.sync().await;
    assert_eq!(
        f.pr_state().await.needs_attention.as_deref(),
        Some("changes_requested")
    );
    f.decline_what_was_read().await;

    let result = f.sync().await;

    assert_eq!((result.synced, result.attention), (1, 0), "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.needs_attention, None);
    assert_eq!(ps.attention_since, None, "a cleared reason clears its age");
    assert_eq!(
        ps.attention_fingerprint.as_deref(),
        Some(CHANGES_REQUESTED_FP)
    );
    assert_eq!(
        ps.addressed_fingerprint.as_deref(),
        Some(CHANGES_REQUESTED_FP),
        "suppression must survive the sync that applies it"
    );
    assert_eq!(ps.addressed_head_sha.as_deref(), Some(SHA));
}

/// Suppression covers static reasons only: a reviewer answering the
/// decline is new, and must still reach an engage worker.
#[tokio::test]
async fn suppression_does_not_hide_new_comments() {
    let f = fixture("sync-suppressed-comment").await;
    let bins = FakeBins::acquire("sync-suppressed-comment");
    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED" })),
    );
    f.sync().await;
    f.decline_what_was_read().await;

    serve_pr(
        &bins,
        &pr(&json!({
            "reviewDecision": "CHANGES_REQUESTED",
            "comments": comment_at("2026-01-02T04:00:00Z"),
        })),
    );
    f.sync().await;

    let ps = f.pr_state().await;
    assert_eq!(ps.needs_attention.as_deref(), Some("new_comments"));
    assert_eq!(
        ps.addressed_fingerprint.as_deref(),
        Some(CHANGES_REQUESTED_FP)
    );
}

/// New code since the decline — ours or a human's — reopens the question
/// even though the review decision reads the same.
#[tokio::test]
async fn a_push_lifts_suppression() {
    let f = fixture("sync-push-lifts").await;
    let bins = FakeBins::acquire("sync-push-lifts");
    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED" })),
    );
    f.sync().await;
    f.decline_what_was_read().await;

    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED", "headRefOid": "cafef00d" })),
    );
    let result = f.sync().await;

    assert_eq!(result.attention, 1, "{result:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.needs_attention.as_deref(), Some("changes_requested"));
    assert_eq!(ps.head_sha.as_deref(), Some("cafef00d"));
    assert_eq!(ps.addressed_fingerprint, None);
    assert_eq!(ps.addressed_head_sha, None);
}

/// A new static reason on the same head is not what was declined.
#[tokio::test]
async fn a_changed_reason_lifts_suppression() {
    let f = fixture("sync-reason-lifts").await;
    let bins = FakeBins::acquire("sync-reason-lifts");
    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED" })),
    );
    f.sync().await;
    f.decline_what_was_read().await;

    serve_pr(
        &bins,
        &pr(&json!({
            "reviewDecision": "CHANGES_REQUESTED",
            "statusCheckRollup": [{ "name": "test", "conclusion": "FAILURE" }],
        })),
    );
    f.sync().await;

    let ps = f.pr_state().await;
    assert_eq!(
        ps.needs_attention.as_deref(),
        Some("changes_requested,checks_failing")
    );
    assert_eq!(
        ps.attention_fingerprint.as_deref(),
        Some("review:CHANGES_REQUESTED|checks:test")
    );
    assert_eq!(ps.addressed_fingerprint, None);
    assert_eq!(ps.addressed_head_sha, None);
}

// ---------------------------------------------------------------------------
// attention_since fairness
// ---------------------------------------------------------------------------

/// `list_attention` orders by `attention_since`: restamping it on every
/// sync would send a long-waiting PR to the back of the queue each cycle.
/// A sync counts as raised only the attention it changed, which is what
/// wakes the job lane; a PR still flagged for the same reasons is not.
#[tokio::test]
async fn attention_since_moves_only_when_the_reason_changes() {
    let f = fixture("sync-attention-since").await;
    f.seed(Mergeable::Mergeable, true, UPDATED_MS, 42).await;
    let bins = FakeBins::acquire("sync-attention-since");

    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED" })),
    );
    let same = f.sync().await;
    assert_eq!(f.pr_state().await.attention_since, Some(42), "unchanged");
    assert_eq!((same.attention, same.raised), (1, 0), "{same:?}");
    assert!(!same.made_work(), "{same:?}");

    serve_pr(
        &bins,
        &pr(&json!({ "reviewDecision": "CHANGES_REQUESTED", "mergeable": "CONFLICTING" })),
    );
    let before = now_ms();
    let changed = f.sync().await;
    assert_eq!((changed.attention, changed.raised), (1, 1), "{changed:?}");
    let ps = f.pr_state().await;
    assert_eq!(
        ps.needs_attention.as_deref(),
        Some("changes_requested,conflict")
    );
    assert!(ps.attention_since.unwrap() >= before, "restamped: {ps:?}");

    serve_pr(&bins, &pr(&json!({})));
    let cleared = f.sync().await;
    assert_eq!((cleared.attention, cleared.raised), (0, 0), "{cleared:?}");
    let ps = f.pr_state().await;
    assert_eq!(ps.needs_attention, None);
    assert_eq!(ps.attention_since, None, "cleared");
}

// ---------------------------------------------------------------------------
// Forge timestamps
// ---------------------------------------------------------------------------

/// Every activity watermark is read through the same hand-rolled
/// RFC-3339 parser; observed here as `last_activity_at`, which is the
/// latest comment's stamp. Expected values computed independently
/// (Python `datetime.fromisoformat(...).timestamp()`).
#[tokio::test]
async fn forge_timestamps_are_read_as_utc_epoch_ms() {
    let f = fixture("sync-timestamps").await;
    let bins = FakeBins::acquire("sync-timestamps");
    let cases: &[(&str, i64)] = &[
        ("2026-01-02T03:04:05Z", UPDATED_MS),
        ("2026-01-02T03:04:05+05:30", 1_767_303_245_000),
        ("2026-01-02T03:04:05-08:00", 1_767_351_845_000),
        ("2026-01-02T03:04:05.5Z", 1_767_323_045_500),
        ("2026-01-02T03:04:05.25Z", 1_767_323_045_250),
        ("2026-01-02T03:04:05.123456Z", 1_767_323_045_123),
        ("2026-01-02T03:04:05", UPDATED_MS),
        ("2024-02-29T23:59:59Z", 1_709_251_199_000),
        ("not a timestamp", 0),
        ("2026-01-02", 0),
        ("", 0),
    ];
    for &(stamp, want) in cases {
        serve_pr(&bins, &pr(&json!({ "comments": comment_at(stamp) })));
        let result = f.sync().await;
        assert_eq!(result.errors, 0, "{stamp:?}: {result:?}");
        assert_eq!(f.pr_state().await.last_activity_at, Some(want), "{stamp:?}");
    }
}
