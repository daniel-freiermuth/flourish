#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Clone permits and tree leases (`hunter::clones`): one holder per clone
//! at a time, taken only in a lane, never waiting on itself.

mod support;

use std::sync::mpsc;
use std::time::Duration;

use hunter::clones::{Lane, NestedLane};
use hunter::domain::ForgeName;
use hunter::store::Store;
use support::TempDir;

const SHORT: Duration = Duration::from_millis(50);
const LONG: Duration = Duration::from_secs(5);

/// A store with repos 1 and 2, whose clones live under `dir/repos`, and
/// a job row of each: job 7 of repo 1, job 8 of repo 2.
async fn store_with_two_repos(dir: &TempDir) -> Store {
    let (path, store) = support::fresh_store(dir, "clones").await;
    let repos = dir.subdir("repos");
    for name in ["one", "two"] {
        let url = format!("https://example.com/{name}.git");
        store
            .add_repo(name, &url, &repos, "main", ForgeName::Github)
            .await
            .unwrap();
    }
    let pool =
        sqlx::SqlitePool::connect_with(sqlx::sqlite::SqliteConnectOptions::new().filename(&path))
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO jobs (id, kind, repo_id, state, started_at, finished_at) VALUES \
         (7, 'hunt', 1, 'done', 1000, 2000), (8, 'hunt', 2, 'done', 1000, 2000)",
    )
    .execute(&pool)
    .await
    .unwrap();
    pool.close().await;
    store
}

/// Whether a lane of its own task gets repo `id`'s clone.
async fn free_elsewhere(store: &Store, id: i64) -> bool {
    let store = store.clone();
    tokio::spawn(async move {
        store
            .lane(async |lane| {
                tokio::time::timeout(LONG, lane.lock_clone(id))
                    .await
                    .is_ok_and(|r| r.is_ok())
            })
            .await
            .unwrap()
    })
    .await
    .unwrap()
}

/// A lane of its own task waits for repo `id`'s clone: it is counted
/// among the clone's waiters, and then gives up.
async fn blocked_elsewhere(store: &Store, id: i64) {
    let waiter = tokio::spawn({
        let store = store.clone();
        async move {
            store
                .lane(async |lane| {
                    let _ = lane.lock_clone(id).await;
                })
                .await
                .unwrap();
        }
    });
    support::until_waiting(store, id, 1).await;
    waiter.abort();
    let _ = waiter.await;
}

#[tokio::test]
async fn a_clone_has_one_holder_at_a_time_and_other_clones_do_not_wait() {
    let dir = TempDir::new("clones-one-holder");
    let store = store_with_two_repos(&dir).await;
    store
        .lane(async |lane| {
            let held = lane.lock_clone(1).await.unwrap();
            blocked_elsewhere(&store, 1).await;
            assert!(free_elsewhere(&store, 2).await, "repo 2 waited for repo 1");
            drop(held);
            assert!(free_elsewhere(&store, 1).await);
        })
        .await
        .unwrap();
}

/// Commands under one permit run one after another: a second started
/// while the first runs waits for it, so two of them cannot change the
/// clone at once.
#[tokio::test]
async fn a_permit_runs_one_command_at_a_time() {
    let dir = TempDir::new("clones-one-command");
    let store = store_with_two_repos(&dir).await;
    store
        .lane(async |lane| {
            let (log, logged) = mpsc::channel::<&str>();
            let (started_tx, started_rx) = mpsc::channel::<()>();
            let (end_tx, end_rx) = mpsc::channel::<()>();
            let permit = lane.lock_clone(1).await.unwrap();
            let first = permit.blocking({
                let log = log.clone();
                move |_| {
                    log.send("first starts").unwrap();
                    started_tx.send(()).unwrap();
                    end_rx.recv().unwrap();
                    log.send("first ends").unwrap();
                }
            });
            tokio::pin!(first);
            tokio::select! {
                _ = &mut first => panic!("the first command ended before it was let"),
                r = tokio::task::spawn_blocking(move || started_rx.recv()) => r.unwrap().unwrap(),
            }
            let second = permit.blocking(move |_| log.send("second").unwrap());
            let let_go = async {
                // Time for a second command that did not wait to run.
                tokio::time::sleep(SHORT).await;
                end_tx.send(()).unwrap();
            };
            let (a, b, ()) = tokio::join!(first, second, let_go);
            a.unwrap();
            b.unwrap();
            let order: Vec<&str> = logged.try_iter().collect();
            assert_eq!(order, ["first starts", "first ends", "second"]);
        })
        .await
        .unwrap();
}

/// A lane opened inside another one of the same task could wait for a
/// permit the outer lane holds and never gets back: it is refused. A task
/// spawned from inside a lane is its own task and opens its own.
#[tokio::test]
async fn a_lane_is_not_opened_inside_another_of_its_task() {
    let dir = TempDir::new("clones-nested");
    let store = store_with_two_repos(&dir).await;
    let (nested, spawned) = store
        .lane(async |_outer: &mut Lane| {
            let nested = store.lane(async |_inner: &mut Lane| ()).await;
            let spawned = tokio::spawn({
                let store = store.clone();
                async move { store.lane(async |_: &mut Lane| ()).await.is_ok() }
            })
            .await
            .unwrap();
            (nested, spawned)
        })
        .await
        .unwrap();
    assert!(matches!(nested, Err(NestedLane)));
    assert!(spawned, "a spawned task's lane was refused");
}

/// A blocking command keeps the clone held past its permit and past the
/// future that ran it being dropped. The lane that dropped them then
/// waits for the command like anyone else.
#[tokio::test]
async fn a_lane_waits_for_a_command_it_dropped() {
    let dir = TempDir::new("clones-dropped-command");
    let store = store_with_two_repos(&dir).await;
    store
        .lane(async |lane| {
            let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
            let (end_tx, end_rx) = std::sync::mpsc::channel::<()>();
            let permit = lane.lock_clone(1).await.unwrap();
            let command = permit.blocking(move |_| {
                started_tx.send(()).unwrap();
                end_rx.recv().unwrap();
            });
            // Started, then dropped with the permit while still running.
            let _ = tokio::time::timeout(SHORT, command).await;
            started_rx.recv().unwrap();
            drop(permit);
            blocked_elsewhere(&store, 1).await;
            let again = lane.lock_clone(1);
            tokio::pin!(again);
            // Pending, neither refused nor granted, while the command runs.
            assert!(tokio::time::timeout(SHORT, &mut again).await.is_err());
            end_tx.send(()).unwrap();
            let granted = tokio::time::timeout(LONG, again).await;
            assert!(granted.is_ok_and(|r| r.is_ok()));
        })
        .await
        .unwrap();
}

/// The waiter count the wait tests rely on: a lane blocked on the lock is
/// counted, and stops being counted once it has the clone or gives up.
#[tokio::test]
async fn a_waiting_lane_is_counted_until_it_stops_waiting() {
    let dir = TempDir::new("clones-waiters");
    let store = store_with_two_repos(&dir).await;
    store
        .lane(async |lane| {
            let held = lane.lock_clone(1).await.unwrap();
            let waiter = tokio::spawn({
                let store = store.clone();
                async move {
                    store
                        .lane(async |lane| lane.lock_clone(1).await.is_ok())
                        .await
                        .unwrap()
                }
            });
            support::until_waiting(&store, 1, 1).await;
            assert_eq!(store.clone_waiters(2), 0);
            drop(held);
            assert!(waiter.await.unwrap());
            assert_eq!(store.clone_waiters(1), 0);
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_tree_is_leased_until_its_last_lease_is_dropped() {
    let dir = TempDir::new("clones-lease");
    let store = store_with_two_repos(&dir).await;
    let chain = store.chain_ref(7).await.unwrap().unwrap();
    store
        .lane(async |lane| {
            let permit = lane.lock_clone(1).await.unwrap();
            let first = permit.lease(&chain).unwrap();
            assert!(!first.shared());
            let second = permit.lease(&chain).unwrap();
            assert!(first.shared() && second.shared());
            drop(second);
            assert!(!first.shared());
            // The lease left is still counted: one taken now shares the
            // tree with it, as a harvest's lease shares its engage's.
            let third = permit.lease(&chain).unwrap();
            assert!(first.shared() && third.shared());
        })
        .await
        .unwrap();
}

/// The sweep looks for a chain's leases under its own clone's permit, so
/// a chain is leased only under that one: job 8's chain is repo 2's.
#[tokio::test]
async fn a_chain_is_leased_only_under_its_own_clones_permit() {
    let dir = TempDir::new("clones-foreign-chain");
    let store = store_with_two_repos(&dir).await;
    let chain = store.chain_ref(8).await.unwrap().unwrap();
    store
        .lane(async |lane| {
            let permit = lane.lock_clone(1).await.unwrap();
            let refused = permit.lease(&chain);
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|e| (e.chain_repo, e.held_repo) == (2, 1)),
                "leased under the wrong clone"
            );
        })
        .await
        .unwrap();
    assert_eq!(store.chain_ref(99).await.unwrap(), None, "no job, no chain");
}

/// A repo has one scan tree, so one scan of it at a time.
#[tokio::test]
async fn a_repos_scan_tree_has_one_lease_at_a_time() {
    let dir = TempDir::new("clones-scan-lease");
    let store = store_with_two_repos(&dir).await;
    store
        .lane(async |lane| {
            let permit = lane.lock_clone(1).await.unwrap();
            let scan = permit.lease_scan().unwrap();
            assert!(permit.lease_scan().is_err(), "two scans of repo 1");
            drop(scan);
            assert!(permit.lease_scan().is_ok());
        })
        .await
        .unwrap();
}
