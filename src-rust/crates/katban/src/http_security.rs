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

pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    match peer {
        Some(address) if !address.ip().is_loopback() => address.ip().to_string(),
        _ => headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next().map(str::trim))
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "loopback".to_string()),
    }
}
