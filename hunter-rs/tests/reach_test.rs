#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The reachability probe against local sockets only: a listener is a
//! reachable host, a port nobody listens on is an unreachable one. No
//! test here touches the network.

mod support;

use hunter::reach::{any_accepts, first_unreachable};

/// A local port that accepts connections while the listener lives.
async fn listener() -> (tokio::net::TcpListener, String) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    (l, addr)
}

/// A local port nobody listens on: bound, then released, so a connect
/// is refused at once.
async fn closed_port() -> String {
    let (l, addr) = listener().await;
    drop(l);
    addr
}

#[tokio::test]
async fn a_listening_host_is_reachable() {
    let (_l, up) = listener().await;
    assert_eq!(first_unreachable(&[up]).await, None);
}

#[tokio::test]
async fn a_closed_port_is_unreachable() {
    let down = closed_port().await;
    assert_eq!(
        first_unreachable(std::slice::from_ref(&down)).await,
        Some(down)
    );
}

#[tokio::test]
async fn the_unreachable_host_is_named_among_reachable_ones() {
    let (_l, up) = listener().await;
    let down = closed_port().await;
    assert_eq!(
        first_unreachable(&[up.clone(), down.clone(), up]).await,
        Some(down)
    );
}

/// Every host down: the report names the first in the order given, so a
/// denial reads the same however the probes happen to finish.
#[tokio::test]
async fn of_several_unreachable_hosts_the_first_is_named() {
    let first = closed_port().await;
    let second = closed_port().await;
    assert_eq!(
        first_unreachable(&[first.clone(), second]).await,
        Some(first)
    );
}

#[tokio::test]
async fn no_hosts_need_no_network() {
    assert_eq!(first_unreachable(&[]).await, None);
}

// ---------------------------------------------------------------------------
// any_accepts: every resolved address at once
// ---------------------------------------------------------------------------

fn addr(s: &str) -> std::net::SocketAddr {
    s.parse().unwrap()
}

#[tokio::test]
async fn one_accepting_address_is_enough_in_either_order() {
    let (_l, up) = listener().await;
    let down = closed_port().await;
    assert!(any_accepts([addr(&down), addr(&up)]).await);
    assert!(any_accepts([addr(&up), addr(&down)]).await);
}

#[tokio::test]
async fn no_accepting_address_is_unreachable() {
    let a = closed_port().await;
    let b = closed_port().await;
    assert!(!any_accepts([addr(&a), addr(&b)]).await);
    assert!(!any_accepts([]).await);
}

/// A listener whose accept queue is full: the kernel drops further SYNs
/// without answering, so a connect to it hangs, as one to an address a
/// network silently drops does. The connections that filled the queue
/// are returned so they stay open.
async fn silent_port() -> (tokio::net::TcpListener, Vec<tokio::net::TcpStream>, String) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind(addr("127.0.0.1:0")).unwrap();
    let listener = socket.listen(0).unwrap();
    let target = listener.local_addr().unwrap();
    let mut held = Vec::new();
    for _ in 0..16 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(200),
            tokio::net::TcpStream::connect(target),
        )
        .await
        {
            Ok(Ok(stream)) => held.push(stream),
            Ok(Err(e)) => panic!("filling the accept queue: {e}"),
            Err(_) => return (listener, held, target.to_string()),
        }
    }
    panic!("the accept queue never filled");
}

/// The case the race exists for: an address that never answers, listed
/// first, does not hold up one that accepts. Tried in turn, the silent one
/// would use up the whole timeout.
#[tokio::test]
async fn a_silent_address_does_not_hide_an_accepting_one() {
    let (_silent, _held, silent) = silent_port().await;
    let (_l, up) = listener().await;
    let raced = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        any_accepts([addr(&silent), addr(&up)]),
    )
    .await;
    assert_eq!(raced.ok(), Some(true));
}
