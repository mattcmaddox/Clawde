//! SSRF protection for HTTP/SSE MCP connections.
//!
//! This module provides URL validation and IP range blocking to prevent
//! Server-Side Request Forgery attacks when connecting to HTTP/SSE MCP servers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Error type for SSRF validation failures.
#[derive(Debug, thiserror::Error)]
pub enum SsrfError {
    #[error("URL must use HTTPS in production mode: {0}")]
    HttpsRequired(String),

    #[error("URL resolves to blocked IP address: {ip} (domain: {domain})")]
    BlockedIp { domain: String, ip: IpAddr },

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
/// - Link-local: 169.254.0.0/16
/// - Private IPv6: fc00::/7, fe80::/10
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

    // Loopback: ::1
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

    // IPv4-mapped IPv6: ::ffff:0:0/96 - check the embedded IPv4
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

/// Validate a URL for SSRF protection.
///
/// Requirements:
/// - Must be HTTP or HTTPS
/// - HTTPS required in production mode (non-localhost)
/// - Cannot point to blocked IP ranges
pub fn validate_url(url: &str, production_mode: bool) -> Result<(), SsrfError> {
    let parsed = url::Url::parse(url).map_err(|e| SsrfError::InvalidUrl(e.to_string()))?;

    // Check scheme
    match parsed.scheme() {
        "http" => {
            // HTTP only allowed for localhost in development mode
            if production_mode && !is_localhost_host(parsed.host_str().unwrap_or("")) {
                return Err(SsrfError::HttpsRequired(url.to_string()));
            }
        }
        "https" => {
            // HTTPS is always allowed
        }
        scheme => {
            return Err(SsrfError::BlockedScheme(scheme.to_string()));
        }
    }

    // Check for localhost/loopback
    if let Some(host) = parsed.host_str() {
        if is_localhost_host(host) {
            // Loopback is allowed (needed for development)
            return Ok(());
        }

        // Try to parse as IP address
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(SsrfError::BlockedIp {
                    domain: host.to_string(),
                    ip,
                });
            }
        }
    }

    Ok(())
}

/// Check if a host string represents localhost.
fn is_localhost_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Validate a URL and resolve it to check if the IP is blocked.
/// This performs DNS resolution to catch DNS rebinding attacks.
pub async fn validate_url_with_resolution(
    url: &str,
    production_mode: bool,
) -> Result<(), SsrfError> {
    // First validate the URL format
    validate_url(url, production_mode)?;

    let parsed = url::Url::parse(url).map_err(|e| SsrfError::InvalidUrl(e.to_string()))?;

    if let Some(host) = parsed.host_str() {
        // Skip DNS resolution for localhost (it's allowed anyway)
        if is_localhost_host(host) {
            return Ok(());
        }

        // Try to parse as IP address first (no DNS needed)
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(SsrfError::BlockedIp {
                    domain: host.to_string(),
                    ip,
                });
            }
            return Ok(());
        }

        // For domain names, perform DNS resolution
        // Note: In production, you might want to use a DNS resolver that
        // prevents rebinding (e.g., resolve once and cache)
        match tokio::net::lookup_host(format!("{}:0", host)).await {
            Ok(addrs) => {
                for addr in addrs {
                    if is_blocked_ip(addr.ip()) {
                        return Err(SsrfError::BlockedIp {
                            domain: host.to_string(),
                            ip: addr.ip(),
                        });
                    }
                }
            }
            Err(_) => {
                // DNS resolution failed - could be intentional blocking
                // or just a non-existent domain. We'll let the connection
                // attempt fail naturally rather than blocking here.
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(!is_blocked_ipv4(Ipv4Addr::new(8, 8, 8, 8))); // Google DNS
        assert!(!is_blocked_ipv4(Ipv4Addr::new(1, 1, 1, 1))); // Cloudflare DNS
    }

    #[test]
    fn test_blocked_ipv6_loopback() {
        assert!(is_blocked_ipv6(Ipv6Addr::LOCALHOST));
    }

    #[test]
    fn test_blocked_ipv6_link_local() {
        assert!(is_blocked_ipv6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)));
    }

    #[test]
    fn test_blocked_ipv6_unique_local() {
        assert!(is_blocked_ipv6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)));
    }

    #[test]
    fn test_validate_url_localhost_http() {
        assert!(validate_url("http://localhost:8080", true).is_ok());
        assert!(validate_url("http://127.0.0.1:8080", true).is_ok());
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
    }

    #[test]
    fn test_is_localhost_host() {
        assert!(is_localhost_host("localhost"));
        assert!(is_localhost_host("127.0.0.1"));
        assert!(is_localhost_host("::1"));
        assert!(is_localhost_host("[::1]"));
        assert!(!is_localhost_host("example.com"));
    }
}
