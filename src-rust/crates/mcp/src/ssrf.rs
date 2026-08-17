//! SSRF protection for HTTP/SSE MCP connections.
//!
//! This module provides:
//! - URL validation (scheme enforcement + literal-IP range blocking)
//! - A DNS resolver that validates every resolved address and fails closed on
//!   blocked ranges (closes the check-then-use gap for DNS rebinding)
//! - A reqwest client builder that wires the pinned resolver and validates
//!   every redirect hop before following it

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolving};

/// Error type for SSRF validation failures.
#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("URL must use HTTPS in production mode: {0}")]
    HttpsRequired(String),

    #[error("URL resolves to blocked IP address: {ip} (host: {host})")]
    BlockedIp { host: String, ip: IpAddr },

    #[error("URL has invalid format: {0}")]
    InvalidUrl(String),

    #[error("URL uses blocked scheme: {0}")]
    BlockedScheme(String),

    #[error("URL contains localhost/loopback address: {0}")]
    LoopbackBlocked(String),
}

/// Check if an IP address is in a blocked range.
///
/// Blocked ranges:
/// - Loopback: 127.0.0.0/8, ::1
/// - Private: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
/// - Link-local: 169.254.0.0/16 (includes cloud metadata endpoints)
/// - Private IPv6: fc00::/7, fe80::/10
/// - 0.0.0.0/8 (current network)
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ipv4) => is_blocked_ipv4(ipv4),
        IpAddr::V6(ipv6) => is_blocked_ipv6(ipv6),
    }
}

/// Check if an IPv4 address is in a blocked range.
fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();

    // Loopback: 127.0.0.0/8
    if octets[0] == 127 {
        return true;
    }

    // Private: 10.0.0.0/8
    if octets[0] == 10 {
        return true;
    }

    // Private: 172.16.0.0/12
    if octets[0] == 172 && (octets[1] >= 16 && octets[1] <= 31) {
        return true;
    }

    // Private: 192.168.0.0/16
    if octets[0] == 192 && octets[1] == 168 {
        return true;
    }

    // Link-local: 169.254.0.0/16
    if octets[0] == 169 && octets[1] == 254 {
        return true;
    }

    // 0.0.0.0/8 (current network)
    if octets[0] == 0 {
        return true;
    }

    false
}

/// Check if an IPv6 address is in a blocked range.
fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    let segments = ip.segments();

    // Loopback: ::1 (also covered by is_loopback, kept explicit for clarity)
    if ip.is_loopback() {
        return true;
    }

    // Link-local: fe80::/10
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }

    // Unique local: fc00::/7
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }

    // IPv4-mapped IPv6: ::ffff:0:0/96 — check the embedded IPv4
    if segments[0] == 0
        && segments[1] == 0
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
    {
        let ipv4 = Ipv4Addr::new(
            (segments[5] >> 8) as u8,
            (segments[5] & 0xff) as u8,
            (segments[6] >> 8) as u8,
            (segments[6] & 0xff) as u8,
        );
        if is_blocked_ipv4(ipv4) {
            return true;
        }
    }

    false
}

/// Strip IPv6 brackets so `[::1]` (as returned by the URL parser's host
/// component) parses as an `IpAddr`.
fn strip_ipv6_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// True when the host is `localhost` or a literal loopback address.
/// Loopback is allowed because local MCP servers are legitimate for
/// development and because the ACP server itself runs on loopback.
pub fn is_localhost_host(host: &str) -> bool {
    if matches!(host, "localhost") {
        return true;
    }
    if let Ok(ip) = strip_ipv6_brackets(host).parse::<IpAddr>() {
        if ip.is_loopback() {
            return true;
        }
    }
    false
}

/// Validate a URL for SSRF protection.
///
/// Requirements:
/// - Must be HTTP or HTTPS
/// - HTTPS required in production mode for non-localhost hosts
/// - Cannot point to blocked IP ranges
pub fn validate_url(url: &str, production_mode: bool) -> Result<(), SsrfError> {
    let parsed = url::Url::parse(url).map_err(|e| SsrfError::InvalidUrl(e.to_string()))?;

    // Check scheme
    match parsed.scheme() {
        "http" => {
            if production_mode && !is_localhost_host(parsed.host_str().unwrap_or("")) {
                return Err(SsrfError::HttpsRequired(url.to_string()));
            }
        }
        "https" => {}
        scheme => {
            return Err(SsrfError::BlockedScheme(scheme.to_string()));
        }
    }

    if let Some(host) = parsed.host_str() {
        if is_localhost_host(host) {
            return Ok(());
        }

        if let Ok(ip) = strip_ipv6_brackets(host).parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(SsrfError::BlockedIp {
                    host: host.to_string(),
                    ip,
                });
            }
        }
    }

    Ok(())
}

/// Validate that a resolved address may be connected to.
///
/// Localhost hosts are allowed; every other blocked range fails closed.
pub fn validate_resolved_address(host: &str, ip: IpAddr) -> Result<(), SsrfError> {
    if is_localhost_host(host) {
        return Ok(());
    }
    if is_blocked_ip(ip) {
        return Err(SsrfError::BlockedIp {
            host: host.to_string(),
            ip,
        });
    }
    Ok(())
}

/// Resolve a host to socket addresses without connecting.
async fn resolve_host(host: &str) -> anyhow::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, 0)]);
    }
    let addrs = tokio::net::lookup_host((host, 0))
        .await
        .map_err(|e| anyhow::anyhow!("DNS resolution failed for '{}': {}", host, e))?;
    Ok(addrs.collect())
}

/// DNS resolver that validates every resolved address against the SSRF policy.
///
/// The lookup and the validation happen inside the resolver, and reqwest
/// connects only to the addresses this resolver returns. Because the addresses
/// are pinned at lookup time, a domain that re-resolves to an internal address
/// after validation cannot redirect the connection (fail-closed pinning).
pub struct SsrfAwareDnsResolver;

impl reqwest::dns::Resolve for SsrfAwareDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs = resolve_host(&host).await?;
            for addr in &addrs {
                validate_resolved_address(&host, addr.ip())?;
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Build a reqwest client hardened against SSRF for one server URL.
///
/// - Validates the URL up front (scheme + literal-IP ranges).
/// - Installs [`SsrfAwareDnsResolver`] so DNS rebinding cannot reach
///   internal ranges at connect time.
/// - Validates every redirect hop with the same policy before following it.
pub fn build_ssrf_aware_client(
    url: &str,
    production_mode: bool,
) -> anyhow::Result<reqwest::Client> {
    validate_url(url, production_mode)?;

    let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
        let next = attempt.url().to_string();
        if validate_url(&next, production_mode).is_err() {
            attempt.error(format!("redirect target blocked by SSRF policy: {}", next))
        } else if attempt.previous().len() > 5 {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    });

    reqwest::Client::builder()
        .dns_resolver(SsrfAwareDnsResolver)
        .redirect(redirect_policy)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build SSRF-aware HTTP client: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::dns::Resolve as _;

    #[test]
    fn test_blocked_ipv4_loopback() {
        assert!(is_blocked_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(is_blocked_ipv4(Ipv4Addr::new(127, 255, 255, 255)));
    }

    #[test]
    fn test_blocked_ipv4_private() {
        assert!(is_blocked_ipv4(Ipv4Addr::new(10, 0, 0, 1)));
        assert!(is_blocked_ipv4(Ipv4Addr::new(172, 16, 0, 1)));
        assert!(is_blocked_ipv4(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn test_blocked_ipv4_link_local() {
        assert!(is_blocked_ipv4(Ipv4Addr::new(169, 254, 1, 1)));
    }

    #[test]
    fn test_allowed_ipv4_public() {
        assert!(!is_blocked_ipv4(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_blocked_ipv4(Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn test_blocked_ipv6_ranges() {
        assert!(is_blocked_ipv6(Ipv6Addr::LOCALHOST));
        assert!(is_blocked_ipv6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)));
        assert!(is_blocked_ipv6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)));
    }

    #[test]
    fn test_validate_url_localhost_http() {
        assert!(validate_url("http://localhost:8080", true).is_ok());
        assert!(validate_url("http://127.0.0.1:8080", true).is_ok());
        assert!(validate_url("http://[::1]:8080", true).is_ok());
    }

    #[test]
    fn test_validate_url_https_allowed() {
        assert!(validate_url("https://example.com", true).is_ok());
    }

    #[test]
    fn test_validate_url_http_blocked_in_production() {
        assert!(validate_url("http://example.com", true).is_err());
    }

    #[test]
    fn test_validate_url_http_allowed_in_development() {
        assert!(validate_url("http://example.com", false).is_ok());
    }

    #[test]
    fn test_validate_url_blocked_ip() {
        assert!(validate_url("http://192.168.1.1:8080", false).is_err());
        assert!(validate_url("http://10.0.0.1:8080", false).is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data/", false).is_err());
    }

    #[test]
    fn test_validate_url_blocked_scheme() {
        assert!(validate_url("file:///etc/passwd", false).is_err());
        assert!(validate_url("ftp://example.com/file", false).is_err());
    }

    #[test]
    fn test_is_localhost_host() {
        assert!(is_localhost_host("localhost"));
        assert!(is_localhost_host("127.0.0.1"));
        assert!(is_localhost_host("::1"));
        assert!(is_localhost_host("[::1]"));
        assert!(!is_localhost_host("example.com"));
    }

    #[test]
    fn test_validate_resolved_address() {
        // Localhost hosts allow loopback addresses.
        assert!(validate_resolved_address("localhost", IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok());
        assert!(validate_resolved_address("127.0.0.1", IpAddr::V4(Ipv4Addr::LOCALHOST)).is_ok());

        // Public domains must not resolve into private ranges.
        assert!(validate_resolved_address(
            "example.com",
            IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))
        )
        .is_ok());
        assert!(validate_resolved_address(
            "example.com",
            IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))
        )
        .is_err());
        assert!(
            validate_resolved_address("example.com", IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
                .is_err()
        );
        assert!(validate_resolved_address(
            "example.com",
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))
        )
        .is_err());
    }

    #[tokio::test]
    async fn resolver_rejects_blocked_literal_ip() {
        let resolver = SsrfAwareDnsResolver;
        let name: Name = "10.0.0.1".parse().expect("valid dns name");
        let result = resolver.resolve(name).await;
        assert!(result.is_err(), "private IP must fail closed");
    }

    #[tokio::test]
    async fn resolver_allows_localhost_literal_ip() {
        let resolver = SsrfAwareDnsResolver;
        let name: Name = "127.0.0.1".parse().expect("valid dns name");
        let result = resolver.resolve(name).await;
        assert!(result.is_ok(), "loopback must be allowed");
    }

    #[test]
    fn ssrf_aware_client_rejects_blocked_url() {
        assert!(build_ssrf_aware_client("http://192.168.1.1:8080/mcp", true).is_err());
        assert!(build_ssrf_aware_client("http://example.com/mcp", true).is_err());
        assert!(build_ssrf_aware_client("https://example.com/mcp", true).is_ok());
        assert!(build_ssrf_aware_client("http://localhost:8080/mcp", true).is_ok());
    }
}
