//! The client's address, as seen through the reverse proxy.
//!
//! Behind Caddy every connection comes from `127.0.0.1`. The proxy adds the real address to
//! `X-Forwarded-For`; that header is believed only when the connection comes from a configured
//! trusted proxy, and it is read from the right, skipping trusted proxies, so that a client cannot
//! choose its own address by sending the header itself.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;

use crate::state::AppState;

/// Address of the client that sent the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

/// The client address for a connection from `peer` carrying `forwarded` (`X-Forwarded-For`).
pub fn resolve(peer: IpAddr, forwarded: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    let Some(list) = forwarded else {
        return peer;
    };
    // Right to left: the rightmost entries were added by our own proxies.
    for entry in list.rsplit(',') {
        let Ok(ip) = entry.trim().parse::<IpAddr>() else {
            // An unparsable hop: stop trusting the chain here.
            return peer;
        };
        if !trusted.contains(&ip) {
            return ip;
        }
    }
    peer
}

/// Middleware: records [`ClientIp`] for the quota layer and the handlers.
pub async fn layer(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(a)| a.ip());
    if let Some(peer) = peer {
        let forwarded = req
            .headers()
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect::<Vec<_>>()
            .join(",");
        let forwarded = (!forwarded.is_empty()).then_some(forwarded);
        let ip = resolve(peer, forwarded.as_deref(), &state.config().trusted_proxies);
        req.extensions_mut().insert(ClientIp(ip));
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::resolve;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_forwarded_address_is_believed_only_from_a_trusted_proxy() {
        let trusted = [ip("127.0.0.1"), ip("::1")];
        // Behind the local proxy: the client's address.
        assert_eq!(
            resolve(ip("127.0.0.1"), Some("203.0.113.7"), &trusted),
            ip("203.0.113.7")
        );
        // A client that forges the header: its own entry is left of the proxy's, which wins.
        assert_eq!(
            resolve(ip("127.0.0.1"), Some("10.0.0.1, 203.0.113.7"), &trusted),
            ip("203.0.113.7")
        );
        // Straight from the internet: the header is ignored.
        assert_eq!(
            resolve(ip("198.51.100.4"), Some("10.0.0.1"), &trusted),
            ip("198.51.100.4")
        );
        // No proxy configured: never believed.
        assert_eq!(
            resolve(ip("127.0.0.1"), Some("203.0.113.7"), &[]),
            ip("127.0.0.1")
        );
        // Garbage, or only proxies: the connection's address.
        assert_eq!(
            resolve(ip("127.0.0.1"), Some("not-an-ip"), &trusted),
            ip("127.0.0.1")
        );
        assert_eq!(resolve(ip("::1"), Some("127.0.0.1"), &trusted), ip("::1"));
        assert_eq!(
            resolve(ip("::1"), Some("2001:db8::5"), &trusted),
            ip("2001:db8::5")
        );
    }
}
