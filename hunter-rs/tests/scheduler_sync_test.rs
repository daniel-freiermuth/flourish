#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! `sync_prs` — the PR lifecycle state machine.
//!
//! Every cycle starts here, and what it writes decides what the rest of
//! the cycle does: a merged or closed PR ends the finding's lifecycle, and
//! `needs_attention` is what `list_attention` hands to engage. Getting the
//! flags wrong in one direction silently drops reviewer feedback; in the
//! other it has the daemon answering its own comments forever.
//!
//! The forge is reached through a scripted `gh` on `PATH` (see `support`),
//! so these tests also exercise the real `gh pr view` argv and JSON parse.

mod support;

use hunter::config::Config;
use hunter::domain::{FindingStatus, ForgeName};
use hunter::scheduler::sync_prs;
use hunter::store::{FindingInsert, Store, SyncPrData};
use support::{FakeBins, TempDir, fresh_store};

const PR_URL: &str = "https://github.com/acme/widget/pull/7";
const REPO_URL: &str = "https://github.com/acme/widget";

/// 2024-01-15T12:00:00Z in epoch ms.
const T_NOON_MS: i64 = 1_705_320_000_000;

struct Fixture {
    db: std::path::PathBuf,
    cfg: Config,
    store: Store,
    rid: i64,
    fid: i64,
    /// Last: the directory must outlive the Store's pool (see
    /// `runner_engage_test::Fixture`).
    _dir: TempDir,
}

impl Fixture {
    async fn raw(&self, sql: &'static str) {
        let pool = sqlx::SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(&self.db),
        )
        .await
        .unwrap();
        sqlx::query(sql)
            .bind(self.rid)
            .execute(&pool)
            .await
            .unwrap();
    }

    async fn status(&self) -> FindingStatus {
        self.store
            .get_finding(self.fid)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    async fn pr_state(&self) -> hunter::types::PrState {
        self.store
            .get_pr_state(self.fid)
            .await
            .unwrap()
            .expect("pr_state row")
    }

    async fn events_of(&self, kind: &str) -> Vec<String> {
        self.store
            .recent_events(100)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == kind && e.finding_id == Some(self.fid))
            .map(|e| e.message)
            .collect()
    }

    /// Seed `pr_state` as a previous sync would have left it.
    async fn seed_synced(&self, engaged: i64, attention: Option<&str>, fp: Option<&str>) {
        self.store
            .sync_pr_open(
                self.fid,
                &SyncPrData {
                    pr_number: 7,
                    state: "open".to_owned(),
                    mergeable: "mergeable".to_owned(),
                    checks: None,
                    head_ref: "fix/some-bug".to_owned(),
                    head_sha: "aaaa".to_owned(),
                    last_activity_at: engaged,
                    last_engaged_activity_at: engaged,
                    needs_attention: attention.map(str::to_owned),
                    attention_fingerprint: fp.map(str::to_owned),
                    synced_at: 1,
                    attention_since: Some(attention.map(|_| 1)),
                    clear_addressed: false,
                },
            )
            .await
            .unwrap();
    }
}

async fn fixture(label: &str, pr_url: &str) -> Fixture {
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
                severity: "medium".to_owned(),
                confidence: 0.9,
                summary: "a fix".to_owned(),
                ..Default::default()
            },
            "bug",
            None,
        )
        .await
        .unwrap();
    store.set_finding_pr_open(fid, pr_url).await.unwrap();
    let cfg = Config::load(dir.path()).expect("load config");
    Fixture {
        db,
        cfg,
        store,
        rid,
        fid,
        _dir: dir,
    }
}

/// A `gh pr view --json` payload. `extra` is spliced in as additional
/// top-level fields (comments, reviews, checks, ...).
fn pr_json(state: &str, head_sha: &str, extra: &str) -> String {
    let sep = if extra.is_empty() { "" } else { "," };
    format!(
        r#"{{"state":"{state}","mergeable":"MERGEABLE","reviewDecision":"","statusCheckRollup":[],"comments":[],"reviews":[],"updatedAt":"2024-01-15T12:00:00Z","headRefName":"fix/some-bug","headRefOid":"{head_sha}"{sep}{extra}}}"#
    )
}

/// All three static reasons at once: changes requested, conflict, and a
/// failing check (plus a passing one that must not appear).
const STATIC_PROBLEMS: &str = r#""reviewDecision":"CHANGES_REQUESTED","mergeable":"CONFLICTING","statusCheckRollup":[{"name":"lint","conclusion":"FAILURE"},{"name":"test","conclusion":"SUCCESS"}]"#;

// ---------------------------------------------------------------------------
// Terminal transitions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_merged_pr_marks_the_finding_merged() {
    let fx = fixture("sync-merged", PR_URL).await;
    let bins = FakeBins::acquire("sync-merged");
    bins.ok("gh", &pr_json("MERGED", "aaaa", ""));

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.merged, r.closed, r.synced, r.errors), (1, 0, 0, 0));
    assert_eq!(fx.status().await, FindingStatus::Merged);
    let ps = fx.pr_state().await;
    assert_eq!(ps.state.as_deref(), Some("MERGED"));
    assert_eq!(ps.pr_number, Some(7));
    assert!(ps.needs_attention.is_none());
    // The URL's owner/repo and number are what reached the forge.
    assert!(bins.called_with("gh", "acme/widget"));
    assert!(bins.called_with("gh", "7"));
}

#[tokio::test]
async fn a_closed_pr_rejects_the_finding_with_a_verdict() {
    let fx = fixture("sync-closed", PR_URL).await;
    let bins = FakeBins::acquire("sync-closed");
    bins.ok("gh", &pr_json("CLOSED", "aaaa", ""));
    // A stale attention flag from an earlier sync must not survive the close,
    // or a dead PR keeps getting picked for engage.
    fx.seed_synced(0, Some("new_comments"), None).await;

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.merged, r.closed, r.synced, r.errors), (0, 1, 0, 0));
    let f = fx.store.get_finding(fx.fid).await.unwrap().unwrap();
    assert_eq!(f.status, FindingStatus::Rejected);
    assert!(
        f.verdict_reason
            .as_deref()
            .is_some_and(|v| v.contains("PR closed without merge")),
        "closed PR must feed the suppression corpus with a reason: {:?}",
        f.verdict_reason
    );
    let ps = fx.pr_state().await;
    assert_eq!(ps.state.as_deref(), Some("CLOSED"));
    assert!(ps.needs_attention.is_none());
}

// ---------------------------------------------------------------------------
// Engaged watermark
// ---------------------------------------------------------------------------

/// First sync has no watermark: it is baselined at the PR's current
/// activity, so the comments present when the PR was opened (our own
/// PR-creation chatter included) are not "new". A comment after that is.
#[tokio::test]
async fn first_sync_baselines_the_watermark_and_later_comments_flag_it() {
    let fx = fixture("sync-watermark", PR_URL).await;
    let bins = FakeBins::acquire("sync-watermark");
    bins.ok(
        "gh",
        &pr_json(
            "OPEN",
            "aaaa",
            r#""comments":[{"author":{"login":"hunter-bot"},"body":"opened","createdAt":"2024-01-15T11:00:00Z"}]"#,
        ),
    );

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.synced, r.attention, r.errors), (1, 0, 0));
    let ps = fx.pr_state().await;
    assert!(
        ps.needs_attention.is_none(),
        "own chatter flagged: {:?}",
        ps.needs_attention
    );
    // max(updatedAt, latest comment) — updatedAt is the later one here.
    assert_eq!(ps.last_engaged_activity_at, Some(T_NOON_MS));
    assert_eq!(ps.last_activity_at, Some(T_NOON_MS - 3_600_000));

    // A reviewer comments after the baseline.
    bins.ok(
        "gh",
        &pr_json(
            "OPEN",
            "aaaa",
            r#""comments":[{"author":{"login":"hunter-bot"},"body":"opened","createdAt":"2024-01-15T11:00:00Z"}],"reviews":[{"author":{"login":"alice"},"body":"why?","submittedAt":"2024-01-15T13:00:00Z","state":"COMMENTED"}]"#,
        ),
    );

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.synced, r.attention), (1, 1));
    let ps = fx.pr_state().await;
    assert_eq!(ps.needs_attention.as_deref(), Some("new_comments"));
    // The watermark only moves when engage handles the comment; sync
    // keeps the stored one rather than re-baselining past the review.
    assert_eq!(ps.last_engaged_activity_at, Some(T_NOON_MS));
    assert!(ps.attention_since.is_some());
    assert_eq!(fx.status().await, FindingStatus::PrOpen);
    assert!(
        fx.events_of("engage")
            .await
            .iter()
            .any(|m| m.contains("needs attention: new_comments")),
    );
}

/// Activity at exactly the watermark is not new: the comparison is
/// strict, or the daemon's own reply (which sets the watermark to its
/// timestamp) would re-flag itself every cycle.
#[tokio::test]
async fn activity_at_the_watermark_is_not_new() {
    let fx = fixture("sync-watermark-eq", PR_URL).await;
    let bins = FakeBins::acquire("sync-watermark-eq");
    fx.seed_synced(T_NOON_MS, None, None).await;
    bins.ok(
        "gh",
        &pr_json(
            "OPEN",
            "aaaa",
            r#""comments":[{"body":"done","createdAt":"2024-01-15T12:00:00Z"}]"#,
        ),
    );

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!(r.attention, 0);
    assert!(fx.pr_state().await.needs_attention.is_none());
}

// ---------------------------------------------------------------------------
// Static attention reasons and their suppression
// ---------------------------------------------------------------------------

#[tokio::test]
async fn static_problems_are_flagged_with_a_fingerprint() {
    let fx = fixture("sync-static", PR_URL).await;
    let bins = FakeBins::acquire("sync-static");
    fx.seed_synced(T_NOON_MS, None, None).await;
    bins.ok("gh", &pr_json("OPEN", "aaaa", STATIC_PROBLEMS));

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.synced, r.attention), (1, 1));
    let ps = fx.pr_state().await;
    assert_eq!(
        ps.needs_attention.as_deref(),
        Some("changes_requested,conflict,checks_failing")
    );
    assert_eq!(
        ps.attention_fingerprint.as_deref(),
        Some("review:CHANGES_REQUESTED|mergeable:CONFLICTING|checks:lint")
    );
    assert_eq!(ps.checks.as_deref(), Some("1 pass / 1 fail"));
    assert_eq!(ps.mergeable.as_deref(), Some("CONFLICTING"));
}

/// A clean PR clears a previous flag and its `attention_since`.
#[tokio::test]
async fn a_resolved_problem_clears_the_flag() {
    let fx = fixture("sync-clear", PR_URL).await;
    let bins = FakeBins::acquire("sync-clear");
    fx.seed_synced(T_NOON_MS, Some("conflict"), Some("mergeable:CONFLICTING"))
        .await;
    bins.ok("gh", &pr_json("OPEN", "aaaa", ""));

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.synced, r.attention), (1, 0));
    let ps = fx.pr_state().await;
    assert!(ps.needs_attention.is_none());
    assert!(ps.attention_fingerprint.is_none());
    assert!(ps.attention_since.is_none());
}

/// An engage reply that pushed nothing records the static fingerprint it
/// declined. Until the snapshot or the head commit changes, sync must not
/// re-flag it — otherwise engage runs on the same unfixable problem every
/// cycle. A push (new head SHA) lifts the suppression and clears the
/// addressed record.
#[tokio::test]
async fn an_addressed_problem_stays_quiet_until_the_head_moves() {
    let fx = fixture("sync-suppress", PR_URL).await;
    let bins = FakeBins::acquire("sync-suppress");
    let fp = "review:CHANGES_REQUESTED|mergeable:CONFLICTING|checks:lint";
    fx.seed_synced(
        T_NOON_MS,
        Some("changes_requested,conflict,checks_failing"),
        Some(fp),
    )
    .await;
    fx.store
        .mark_pr_engaged(fx.fid, T_NOON_MS, 2, Some(fp), Some("aaaa"))
        .await
        .unwrap();

    bins.ok("gh", &pr_json("OPEN", "aaaa", STATIC_PROBLEMS));
    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.synced, r.attention), (1, 0));
    let ps = fx.pr_state().await;
    assert!(ps.needs_attention.is_none(), "suppressed reason re-flagged");
    assert_eq!(ps.addressed_fingerprint.as_deref(), Some(fp));
    assert_eq!(ps.addressed_head_sha.as_deref(), Some("aaaa"));

    // Someone pushes: same static snapshot, new head.
    bins.ok("gh", &pr_json("OPEN", "bbbb", STATIC_PROBLEMS));
    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!(r.attention, 1);
    let ps = fx.pr_state().await;
    assert_eq!(
        ps.needs_attention.as_deref(),
        Some("changes_requested,conflict,checks_failing")
    );
    assert!(ps.addressed_fingerprint.is_none());
    assert!(ps.addressed_head_sha.is_none());
}

/// Suppression covers the static reasons only: a new comment on a
/// suppressed PR still gets engaged.
#[tokio::test]
async fn suppression_does_not_hide_new_comments() {
    let fx = fixture("sync-suppress-comment", PR_URL).await;
    let bins = FakeBins::acquire("sync-suppress-comment");
    let fp = "mergeable:CONFLICTING";
    fx.seed_synced(T_NOON_MS, None, Some(fp)).await;
    fx.store
        .mark_pr_engaged(fx.fid, T_NOON_MS, 2, Some(fp), Some("aaaa"))
        .await
        .unwrap();
    bins.ok(
        "gh",
        &pr_json(
            "OPEN",
            "aaaa",
            r#""mergeable":"CONFLICTING","comments":[{"body":"ping","createdAt":"2024-01-15T14:00:00Z"}]"#,
        ),
    );

    sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!(
        fx.pr_state().await.needs_attention.as_deref(),
        Some("new_comments")
    );
}

// ---------------------------------------------------------------------------
// Error paths and the missing-URL boundary
// ---------------------------------------------------------------------------

/// Every error path leaves the finding `pr_open` (so the next cycle retries
/// it), counts an error, and logs one naming the finding.
async fn assert_sync_error(fx: &Fixture, needle: &str) {
    let r = sync_prs(&fx.store, &fx.cfg).await;
    assert_eq!((r.errors, r.synced, r.merged, r.closed), (1, 0, 0, 0));
    assert_eq!(fx.status().await, FindingStatus::PrOpen);
    assert!(fx.store.get_pr_state(fx.fid).await.unwrap().is_none());
    let errors = fx.events_of("error").await;
    assert!(
        errors.iter().any(|m| m.contains(needle)),
        "no error event containing {needle:?}: {errors:?}"
    );
}

#[tokio::test]
async fn a_missing_repo_is_an_error() {
    let fx = fixture("sync-no-repo", PR_URL).await;
    let bins = FakeBins::acquire("sync-no-repo");
    bins.ok("gh", &pr_json("MERGED", "aaaa", ""));
    fx.raw("UPDATE repos SET deleted_at = 1 WHERE id = ?1")
        .await;

    assert_sync_error(&fx, "missing").await;
    assert!(bins.calls_to("gh").is_empty());
}

#[tokio::test]
async fn an_unparseable_pr_url_is_an_error() {
    let fx = fixture("sync-bad-url", "https://github.com/acme/widget").await;
    let bins = FakeBins::acquire("sync-bad-url");
    bins.ok("gh", &pr_json("MERGED", "aaaa", ""));

    assert_sync_error(&fx, "unparseable pr_url").await;
    assert!(bins.calls_to("gh").is_empty());
}

#[tokio::test]
async fn a_failed_forge_view_is_an_error() {
    let fx = fixture("sync-gh-fail", PR_URL).await;
    let bins = FakeBins::acquire("sync-gh-fail");
    bins.fail("gh", 1, "HTTP 502");

    assert_sync_error(&fx, "PR view failed").await;
}

/// `pr_open` with an empty URL means the fix failed before the PR was
/// created. It is requeued for another attempt, not reported as a sync
/// error and not sent to the forge.
#[tokio::test]
async fn pr_open_without_a_url_is_requeued() {
    let fx = fixture("sync-no-url", "").await;
    let bins = FakeBins::acquire("sync-no-url");
    bins.ok("gh", &pr_json("MERGED", "aaaa", ""));

    let r = sync_prs(&fx.store, &fx.cfg).await;

    assert_eq!((r.errors, r.synced, r.merged), (0, 0, 0));
    assert_eq!(fx.status().await, FindingStatus::Queued);
    assert!(bins.calls_to("gh").is_empty());
    assert!(
        fx.events_of("fix")
            .await
            .iter()
            .any(|m| m.contains("requeued")),
    );
}
