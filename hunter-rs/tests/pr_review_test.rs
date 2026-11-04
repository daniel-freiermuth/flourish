#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Reviews of PRs hunter did not open (migration 035, future.md §8.8):
//! when a round is due, how a failing review is held and released, and
//! which earlier issues a round is shown.

mod support;

use hunter::domain::{ForgeName, PrReviewState};
use hunter::store::{HEAD_STABLE_MS, NewPrReview, PR_REVIEW_HOLD_AT, PrReviewRound, Store};
use support::{TempDir, fresh_store};

const REPO_URL: &str = "https://github.com/acme/widget";

/// The binding of a review no request bound to a new URL: its first.
const BINDING: i64 = 1;

struct Fixture {
    store: Store,
    rid: i64,
    /// Last: fields drop in declaration order, and the directory must
    /// outlive the Store's pool.
    _dir: TempDir,
}

/// Repo "widget" (GitHub, `REPO_URL`).
async fn fixture(label: &str) -> Fixture {
    let dir = TempDir::new(label);
    let (_, store) = fresh_store(&dir, "hunter").await;
    let rid = store
        .add_repo(
            "widget",
            REPO_URL,
            &dir.subdir("repos"),
            "main",
            ForgeName::Github,
        )
        .await
        .unwrap();
    Fixture {
        store,
        rid,
        _dir: dir,
    }
}

async fn request(f: &Fixture, head: &str, watch: bool) -> i64 {
    f.store
        .request_pr_review(&NewPrReview {
            repo_id: f.rid,
            pr_number: 7,
            repo_url: REPO_URL,
            title: "make the widget faster",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: head,
            watch,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap()
}

fn due_ids(rows: &[hunter::store::PrReviewRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

/// The first round is due at once; afterwards only a watched review with a
/// new head that has been stable for `HEAD_STABLE_MS` is, and until then
/// that is the next due time the job lane sleeps toward; a held review
/// never is until it is retried.
#[tokio::test]
async fn due_reviews_follow_watch_head_stability_and_hold() {
    let f = fixture("prr-due").await;
    let now = hunter::util::now_ms();
    let id = request(&f, "aaa", false).await;
    let due = |at: i64| {
        let store = f.store.clone();
        async move { due_ids(&store.list_due_pr_reviews(at).await.unwrap()) }
    };
    assert_eq!(
        due(now).await,
        vec![id],
        "the first round waits for nothing"
    );

    let scope = support::scope(&f.store);
    let jobs = scope.store();
    jobs.record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    assert!(due(now).await.is_empty(), "reviewed head: nothing due");

    jobs.observe_pr_review((id, BINDING), "bbb", "t", PrReviewState::Open)
        .await
        .unwrap();
    let later = hunter::util::now_ms() + HEAD_STABLE_MS;
    assert!(
        due(later).await.is_empty(),
        "unwatched: a new head is not reviewed"
    );

    assert!(f.store.set_pr_review_watch(id, true).await.unwrap());
    assert!(
        due(hunter::util::now_ms()).await.is_empty(),
        "watched, but the new head has just been seen"
    );
    assert_eq!(due(later).await, vec![id], "stable for the window: due");
    let seen = f
        .store
        .get_pr_review(id)
        .await
        .unwrap()
        .unwrap()
        .head_seen_at;
    let next = |at: i64| {
        let store = f.store.clone();
        async move { store.next_pr_review_due_at(at).await.unwrap() }
    };
    assert_eq!(
        next(hunter::util::now_ms()).await,
        seen.map(|at| at + HEAD_STABLE_MS),
        "the next due time"
    );
    assert_eq!(next(later).await, None, "due already: the pick's");
    let settled = seen.unwrap() + HEAD_STABLE_MS;
    assert_eq!(due(settled).await, vec![id], "due the moment it settles");
    assert_eq!(next(settled).await, None, "due at that moment: the pick's");

    for _ in 0..PR_REVIEW_HOLD_AT {
        jobs.record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "bbb"),
            PrReviewRound::Failed("boom"),
        )
        .await
        .unwrap();
    }
    let held = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(held.held, "{held:?}");
    assert_eq!(held.last_failure.as_deref(), Some("boom"));
    assert!(due(later).await.is_empty(), "held: never due");

    assert!(f.store.retry_pr_review(id).await.unwrap());
    let retried = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(!retried.held && retried.failures == 0, "{retried:?}");
    assert_eq!(due(later).await, vec![id]);
}

/// What an incremental round is told was already reported spans the whole
/// chain since the last review from scratch: a round that found nothing new
/// does not drop the issues before it, and a round that started over (after
/// a force-push its base is no earlier head) starts a new chain.
#[tokio::test]
async fn earlier_issues_span_the_incremental_chain() {
    let f = fixture("prr-chain").await;
    let id = request(&f, "aaa", true).await;
    let issue = |summary: &str| hunter::types::PrReviewIssueOut {
        kind: "introduced".into(),
        blocking: true,
        severity: None,
        confidence: None,
        file: None,
        line: None,
        summary: summary.into(),
        detail: String::new(),
    };
    let jobs = support::scope(&f.store);
    let store = jobs.store();
    let summaries = || async {
        store
            .chain_pr_review_issues(id)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.summary)
            .collect::<Vec<_>>()
    };
    let round = |range: (&'static str, &'static str), issues: Vec<_>| async move {
        store
            .record_pr_review_round((id, BINDING), None, range, PrReviewRound::Ok(&issues))
            .await
            .unwrap();
    };

    round(("base", "aaa"), vec![issue("first")]).await;
    round(("aaa", "bbb"), vec![]).await;
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("bbb", "ccc"),
            PrReviewRound::Failed("worker killed"),
        )
        .await
        .unwrap();
    assert_eq!(
        summaries().await,
        ["first"],
        "an empty round keeps the chain"
    );
    round(("bbb", "ccc"), vec![issue("third")]).await;
    assert_eq!(summaries().await, ["first", "third"]);

    round(("base", "ddd"), vec![issue("after force-push")]).await;
    assert_eq!(summaries().await, ["after force-push"]);
}

/// A round stopped partway is in the chain from its base: what it wrote
/// counts, and the round that reviews the same commits again continues
/// the chain from that base, keeping what came before it. Only a finished
/// round moves the reviewed head and ends the streak.
#[tokio::test]
async fn a_partial_round_keeps_the_chain_at_its_base() {
    let f = fixture("prr-chain-partial").await;
    let id = request(&f, "aaa", true).await;
    let issue = |summary: &str| hunter::types::PrReviewIssueOut {
        kind: "introduced".into(),
        blocking: true,
        severity: None,
        confidence: None,
        file: None,
        line: None,
        summary: summary.into(),
        detail: String::new(),
    };
    let jobs = support::scope(&f.store);
    let store = jobs.store();
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("base", "aaa"),
            PrReviewRound::Ok(&[issue("first")]),
        )
        .await
        .unwrap();
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "bbb"),
            PrReviewRound::Partial {
                issues: &[issue("stopped")],
                why: "worker suspended before it finished",
            },
        )
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (review.last_reviewed_sha.as_deref(), review.failures),
        (Some("aaa"), 1)
    );
    store
        .record_pr_review_round((id, BINDING), None, ("aaa", "bbb"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    store
        .record_pr_review_round((id, BINDING), None, ("bbb", "ccc"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (review.last_reviewed_sha.as_deref(), review.failures),
        (Some("ccc"), 0)
    );
    let earlier: Vec<String> = store
        .chain_pr_review_issues(id)
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.summary)
        .collect();
    assert_eq!(
        earlier,
        ["first", "stopped"],
        "one chain from the merge-base"
    );
}

/// A request that binds a review to the repo's new URL starts it afresh:
/// its rounds so far were about the PR at the earlier URL, so nothing they
/// reviewed counts at the new one, whose first round reviews the PR from
/// its merge-base. A round and a read of the earlier PR picked before the
/// rebind (a request can land while a round runs) move nothing of it.
#[tokio::test]
async fn a_rebound_review_starts_afresh() {
    let f = fixture("prr-rebound").await;
    let id = request(&f, "aaa", true).await;
    let jobs = support::scope(&f.store);
    let store = jobs.store();
    store
        .record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();

    let moved = "https://github.com/someone-else/widget";
    f.store
        .request_pr_review(&NewPrReview {
            repo_id: f.rid,
            pr_number: 7,
            repo_url: moved,
            title: "make the widget faster",
            author: "outsider",
            base_ref: "main",
            cross_repo: false,
            head_sha: "bbb",
            watch: true,
            vouched_author: None,
            requested_by: "tester",
        })
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (
            review.binding,
            review.last_reviewed_sha.as_deref(),
            review.round_requested
        ),
        (BINDING + 1, None, true),
        "reviewed afresh, from the merge-base"
    );

    store
        .record_pr_review_round((id, BINDING), None, ("aaa", "ccc"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    store
        .record_pr_review_round(
            (id, BINDING),
            None,
            ("aaa", "ccc"),
            PrReviewRound::Failed("boom"),
        )
        .await
        .unwrap();
    store
        .observe_pr_review(
            (id, BINDING),
            "ccc",
            "the earlier PR",
            PrReviewState::Merged,
        )
        .await
        .unwrap();
    let after = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (
            after.state,
            after.head_sha.as_deref(),
            after.last_reviewed_sha.as_deref(),
            after.round_requested,
            after.failures
        ),
        (PrReviewState::Open, Some("bbb"), None, true, 0)
    );
}
