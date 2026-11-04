#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Reviews of PRs hunter did not open (migration 035, future.md §8.8):
//! when a round is due, how a failing review is held and released, and
//! which earlier issues a round is shown.

mod support;

use hunter::domain::{ForgeName, PrReviewState};
use hunter::store::{HEAD_STABLE_MS, NewPrReview, PR_REVIEW_HOLD_AT, PrReviewRound, Store, Trust};
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
            head_owner: None,
            trust: Some(Trust::default()),
            requested_by: "tester",
        })
        .await
        .unwrap()
        .id
}

fn due_ids(rows: &[hunter::store::PrReviewRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

/// The first round is due at once; afterwards only a watched review with a
/// new head that has been stable for `HEAD_STABLE_MS` is, and until then
/// that is the next due time the job lane sleeps toward; a held review
/// never is until it is retried, nor one whose PR ended.
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

    jobs.observe_pr_review((id, BINDING), ("bbb", None), "t", PrReviewState::Open)
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
    assert_eq!(held.due_at(), None, "held, whatever its backoff: {held:?}");

    assert!(f.store.retry_pr_review(id).await.unwrap());
    let retried = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert!(!retried.held && retried.failures == 0, "{retried:?}");
    assert_eq!(due(later).await, vec![id]);

    jobs.observe_pr_review((id, BINDING), ("bbb", None), "t", PrReviewState::Merged)
        .await
        .unwrap();
    let ended = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!((ended.held, ended.due_at()), (false, None), "{ended:?}");
}

/// When the request waiting on review `id` was made.
async fn asked_at(f: &Fixture, id: i64) -> i64 {
    let row = f.store.get_pr_review(id).await.unwrap().unwrap();
    row.round_requested_at.expect("a request waits")
}

/// A round answers the request it took as it read the PR (one made before
/// that read), whatever head it then reviewed (the PR's newest), and one
/// made since that read the head it reviewed. A request for a newer head,
/// made while the round runs, stays due at once, watched or not, until the
/// round at that head answers it.
#[tokio::test]
async fn a_round_answers_the_requests_it_read_the_pr_after() {
    let f = fixture("prr-raced").await;
    let id = request(&f, "bbb", false).await;
    let scope = support::scope(&f.store);
    let jobs = scope.store();
    let row = || async { f.store.get_pr_review(id).await.unwrap().unwrap() };
    // The author pushes ccc before the round reads the PR: it reviews
    // ccc, the head the request's round is about by then.
    support::admin(&f.store)
        .store()
        .observe_pr_review((id, BINDING), ("ccc", None), "t", PrReviewState::Open)
        .await
        .unwrap();
    let read = asked_at(&f, id).await + 1;
    jobs.take_pr_review_request((id, BINDING), "ccc", read)
        .await
        .unwrap();
    jobs.record_pr_review_round((id, BINDING), None, ("base", "ccc"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let answered = row().await;
    assert_eq!(
        (answered.round_requested_at, answered.due_at()),
        (None, None),
        "{answered:?}"
    );

    // The round at ddd begins reading the PR; the operator asks for eee
    // before the read returns and the round takes what was asked before it.
    assert_eq!(request(&f, "ddd", false).await, id);
    let read = asked_at(&f, id).await;
    assert_eq!(request(&f, "eee", false).await, id);
    jobs.take_pr_review_request((id, BINDING), "ddd", read)
        .await
        .unwrap();
    jobs.record_pr_review_round((id, BINDING), None, ("ccc", "ddd"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let waiting = row().await;
    assert_eq!(
        waiting.last_reviewed_sha.as_deref(),
        Some("ddd"),
        "{waiting:?}"
    );
    assert_eq!(
        waiting.requested_head_sha.as_deref(),
        Some("eee"),
        "the request for eee waits: {waiting:?}"
    );
    assert_eq!(waiting.due_at(), Some(0), "{waiting:?}");
    let now = hunter::util::now_ms();
    assert_eq!(
        due_ids(&f.store.list_due_pr_reviews(now).await.unwrap()),
        vec![id]
    );

    // The request is repeated while the round at eee runs: it read the
    // head the round reviews, so it is answered all the same.
    let read = asked_at(&f, id).await;
    jobs.take_pr_review_request((id, BINDING), "eee", read)
        .await
        .unwrap();
    assert_eq!(request(&f, "eee", false).await, id);
    jobs.record_pr_review_round((id, BINDING), None, ("ddd", "eee"), PrReviewRound::Ok(&[]))
        .await
        .unwrap();
    let answered = row().await;
    assert_eq!(answered.round_requested_at, None, "{answered:?}");
    assert_eq!(answered.due_at(), None, "{answered:?}");
}

/// What a request waits for is written by requests alone: a head the PR
/// sync reads while a round runs is no request. A review asked for at
/// aaa, whose round at aaa runs while the author pushes bbb, is answered
/// by that round. Unwatched, it then runs no other round (only the one the
/// request asked for); watched, bbb waits out its stability window like
/// any push, so a burst of pushes is still reviewed once.
#[tokio::test]
async fn a_head_read_while_a_round_runs_asks_for_no_round() {
    for watch in [false, true] {
        let f = fixture(&format!("prr-read-no-request-{watch}")).await;
        let id = request(&f, "aaa", watch).await;
        let read = asked_at(&f, id).await + 1;
        let scope = support::scope(&f.store);
        scope
            .store()
            .take_pr_review_request((id, BINDING), "aaa", read)
            .await
            .unwrap();
        // The round at aaa runs; the sync reads the author's push.
        support::admin(&f.store)
            .store()
            .observe_pr_review((id, BINDING), ("bbb", None), "t", PrReviewState::Open)
            .await
            .unwrap();
        scope
            .store()
            .record_pr_review_round((id, BINDING), None, ("base", "aaa"), PrReviewRound::Ok(&[]))
            .await
            .unwrap();
        let row = f.store.get_pr_review(id).await.unwrap().unwrap();
        assert_eq!(row.round_requested_at, None, "watch {watch}: {row:?}");
        let stable = row.head_seen_at.unwrap() + HEAD_STABLE_MS;
        assert_eq!(
            row.due_at(),
            watch.then_some(stable),
            "watch {watch}: {row:?}"
        );
        let now = hunter::util::now_ms();
        assert!(
            f.store.list_due_pr_reviews(now).await.unwrap().is_empty(),
            "watch {watch}: nothing due at once"
        );
    }
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
            head_owner: None,
            trust: Some(Trust::default()),
            requested_by: "tester",
        })
        .await
        .unwrap();
    let review = f.store.get_pr_review(id).await.unwrap().unwrap();
    assert_eq!(
        (
            review.binding,
            review.last_reviewed_sha.as_deref(),
            review.requested_head_sha.as_deref()
        ),
        (BINDING + 1, None, Some("bbb")),
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
            ("ccc", None),
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
            after.requested_head_sha.as_deref(),
            after.failures
        ),
        (PrReviewState::Open, Some("bbb"), None, Some("bbb"), 0)
    );
}
