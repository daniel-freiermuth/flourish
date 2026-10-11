//! Reachability probe: can this machine open a connection to a host?
//!
//! The daemon runs on a laptop that travels and sleeps, so being offline
//! is normal. Work started offline fails for a reason that has nothing to
//! do with the work, so the scheduler asks before it starts any (#95).
//!
//! A probe is a TCP connect and nothing more: no TLS, no request, no
//! credentials. It answers "is there a route to this host", which is the
//! question; whether the service behind it is healthy is the job's own
//! business.

use std::time::Duration;

/// How long one probe may take, name lookup included.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// The port a host named without one is probed on: every forge API and
/// every LLM provider endpoint the daemon talks to is HTTPS.
pub const HTTPS_PORT: u16 = 443;

/// The first of `hosts` that cannot be reached, in the order given;
/// `None` when every one can, and for an empty list.
///
/// An entry is `host` or `host:port` (`[addr]:port` for an IPv6 literal);
/// without a port it is probed on [`HTTPS_PORT`]. Reached means one of
/// the name's addresses accepted a TCP connection within
/// [`PROBE_TIMEOUT`], lookup included ([`any_accepts`]); a name that does
/// not resolve is unreachable like a refused or silent port. The hosts are
/// probed concurrently, so a machine that is offline waits one timeout,
/// not one per host.
pub async fn first_unreachable(hosts: &[String]) -> Option<String> {
    let probes: Vec<_> = hosts
        .iter()
        .map(|host| tokio::spawn(reachable(socket_target(host))))
        .collect();
    for (host, probe) in hosts.iter().zip(probes) {
        // A probe task that panicked proved nothing reachable.
        if !probe.await.unwrap_or(false) {
            return Some(host.clone());
        }
    }
    None
}

/// Whether `target` (`host:port`) resolves to an address that accepts a
/// TCP connection, all within [`PROBE_TIMEOUT`].
async fn reachable(target: String) -> bool {
    let probe = async {
        match tokio::net::lookup_host(target).await {
            Ok(addrs) => any_accepts(addrs).await,
            Err(_) => false,
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, probe)
        .await
        .unwrap_or(false)
}

/// Whether any of `addrs` accepts a TCP connection, connecting to all of
/// them at once; `false` once every one has failed. No timeout of its
/// own: the caller bounds it.
///
/// At once, not in turn, because one address that silently drops traffic
/// must not use up the time the others needed. Plenty of networks a
/// laptop joins (hotels, trains, hotspots) hand out IPv6 and then drop it;
/// a host's AAAA address comes first there, and trying it first would
/// call a host offline that `git`, `gh` and the provider client reach over
/// IPv4 (curl falls back after ~200 ms).
pub async fn any_accepts(addrs: impl IntoIterator<Item = std::net::SocketAddr>) -> bool {
    let mut connects = tokio::task::JoinSet::new();
    for addr in addrs {
        connects.spawn(async move { tokio::net::TcpStream::connect(addr).await.is_ok() });
    }
    // Dropping the set aborts the connects still pending.
    while let Some(accepted) = connects.join_next().await {
        if matches!(accepted, Ok(true)) {
            return true;
        }
    }
    false
}

/// `host` as a `host:port` that `TcpStream::connect` resolves, with
/// [`HTTPS_PORT`] filled in when it names no port.
fn socket_target(host: &str) -> String {
    if host.starts_with('[') {
        if host.contains("]:") {
            host.to_owned()
        } else {
            format!("{host}:{HTTPS_PORT}")
        }
    } else {
        match host.matches(':').count() {
            0 => format!("{host}:{HTTPS_PORT}"),
            1 => host.to_owned(),
            // A bare IPv6 literal: its colons are the address's own.
            _ => format!("[{host}]:{HTTPS_PORT}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::socket_target;

    #[test]
    fn a_host_without_a_port_is_probed_on_https() {
        assert_eq!(socket_target("github.com"), "github.com:443");
        assert_eq!(socket_target("[::1]"), "[::1]:443");
        assert_eq!(socket_target("::1"), "[::1]:443");
    }

    #[test]
    fn a_host_with_a_port_keeps_it() {
        assert_eq!(socket_target("127.0.0.1:8443"), "127.0.0.1:8443");
        assert_eq!(socket_target("[::1]:8443"), "[::1]:8443");
    }
}
