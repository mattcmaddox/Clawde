//! HTTP request security primitives shared by the two independent surfaces.
//!
//! These helpers contain no product policy and no password or session logic.
//! Katban's board server and Cat Chat may both use them without one feature
//! importing the other's implementation.

use axum::extract::ConnectInfo;
use axum::http::HeaderMap;
use std::convert::Infallible;
use std::net::SocketAddr;

pub fn origin_host(value: &str) -> String {
    let without_scheme = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let host_with_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if let Some(rest) = host_with_port.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_lowercase();
    }
    host_with_port
        .split(':')
        .next()
        .unwrap_or(host_with_port)
        .to_lowercase()
}

pub fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

pub fn origin_parts(value: &str) -> (String, Option<u16>) {
    let without_scheme = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .unwrap_or(value);
    let host_with_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if let Some(rest) = host_with_port.strip_prefix('[') {
        let (host, port) = rest.split_once(']').unwrap_or((rest, ""));
        return (
            host.to_lowercase(),
            port.strip_prefix(':').and_then(|value| value.parse().ok()),
        );
    }
    match host_with_port.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            (host.to_lowercase(), port.parse().ok())
        }
        _ => (host_with_port.to_lowercase(), None),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S> axum::extract::FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(address)| *address),
        ))
    }
}

/// Bucket name the lockout ladders file a loopback client under when it
/// arrived without `X-Forwarded-For` (a local `curl`, or a proxy that did not
/// set the header). It is deliberately not an address: there is no client IP
/// to blame, so every such request shares one bucket.
///
/// Because it is not an address, an operator cannot guess it from the URL they
/// were given — `unblock` takes an address, so [`lockout_bucket`] maps the
/// loopback spellings onto this name. Without that, `unblock 127.0.0.1` (and
/// `unblock localhost`) removes a key nothing was ever stored under, and the
/// locked-out client stays locked out.
pub const LOOPBACK_BUCKET: &str = "loopback";

/// The `failedAttempts` bucket an operator's argument refers to.
///
/// `127.0.0.1`, `localhost`, `::1` and their URL spellings (`[::1]`,
/// `127.0.0.1:8789`, `::ffff:127.0.0.1`) all resolve to [`LOOPBACK_BUCKET`],
/// which is what `client_ip` actually files a loopback client under. Anything
/// else (a real address, or an `X-Forwarded-For` value) is passed through
/// unchanged, so an exposed install's per-visitor buckets still work.
pub fn lockout_bucket(input: &str) -> String {
    let trimmed = input.trim();
    let unbracketed = match trimmed.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(rest),
        None => trimmed,
    };
    // Strip an IPv4 `host:port`; a bare IPv6 keeps its colons.
    let bare = match unbracketed.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => host,
        _ => unbracketed,
    };
    let bare = bare.strip_prefix("::ffff:").unwrap_or(bare);
    let lowered = bare.to_ascii_lowercase();
    if lowered == LOOPBACK_BUCKET || is_loopback_host(&lowered) {
        return LOOPBACK_BUCKET.to_string();
    }
    bare.to_string()
}

/// One line naming each live lockout bucket and when it lapses, e.g.
/// `loopback (lockout, 152s)` or `1.2.3.4 (24h block, 86000s)`.
///
/// The two stores (guest links and the board's admin store) both key their
/// ladder by `(bucket, blocked, until)` triples from `active_lockouts`, and
/// every surface that reports a lockout uses this so the bucket an operator
/// must pass to `unblock` is the same name in `status`, in the TUI, and in the
/// unblock miss message.
pub fn describe_lockouts(active: &[(String, bool, u64)], now: u64) -> String {
    active
        .iter()
        .map(|(bucket, blocked, until)| {
            let remaining = until.saturating_sub(now);
            let kind = if *blocked { "24h block" } else { "lockout" };
            format!("{bucket} ({kind}, {remaining}s)")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    match peer {
        Some(address) if !address.ip().is_loopback() => address.ip().to_string(),
        _ => headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next().map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| LOOPBACK_BUCKET.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lockout ladder files a local client under `LOOPBACK_BUCKET`, so
    /// every spelling an operator might pass to `unblock` has to land on that
    /// same key. When this drifts, `unblock 127.0.0.1` removes a key nothing
    /// was stored under and the locked-out client stays locked out.
    #[test]
    fn every_loopback_spelling_resolves_to_the_loopback_bucket() {
        for spelling in [
            "127.0.0.1",
            "localhost",
            "LOCALHOST",
            "::1",
            "[::1]",
            "[::1]:8789",
            "127.0.0.1:8789",
            "::ffff:127.0.0.1",
            "loopback",
            " 127.0.0.1 ",
        ] {
            assert_eq!(lockout_bucket(spelling), LOOPBACK_BUCKET, "{spelling}");
            assert_eq!(
                lockout_bucket(spelling),
                client_ip(&HeaderMap::new(), None),
                "{spelling} must match what client_ip files a local request under"
            );
        }
    }

    #[test]
    fn real_addresses_keep_their_identity_and_lose_only_a_port() {
        for address in ["1.2.3.4", "203.0.113.7", "2001:db8::1", "some.example.com"] {
            assert_eq!(lockout_bucket(address), address);
        }
        // A port is noise: the ladder files the client address, so an operator
        // pasting `host:port` must resolve to the same bucket as the bare host.
        assert_eq!(lockout_bucket("1.2.3.4:8789"), "1.2.3.4");
        assert_eq!(lockout_bucket("[2001:db8::1]:8789"), "2001:db8::1");
    }

    #[test]
    fn describe_lockouts_names_the_bucket_and_the_kind() {
        let described = describe_lockouts(
            &[
                (LOOPBACK_BUCKET.to_string(), false, 1_000),
                ("1.2.3.4".to_string(), true, 5_000),
            ],
            400,
        );
        assert_eq!(
            described,
            "loopback (lockout, 600s), 1.2.3.4 (24h block, 4600s)"
        );
    }
}
