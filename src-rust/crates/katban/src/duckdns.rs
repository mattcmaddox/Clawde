//! DuckDNS subdomain automation (spec §10.1, decision C4): point a
//! per-project subdomain at the host's public IP when a site is exposed.
//!
//! DuckDNS' free tier has no wildcard DNS, so each subdomain is created via
//! their update API. The API responds `OK` or `KO`; callers treat a failed
//! update as best-effort (warn, don't fail the whole expose).

use anyhow::Context;

pub const DUCKDNS_UPDATE_URL: &str = "https://www.duckdns.org/update";

/// Build the update URL. `domain` is the single DuckDNS label (e.g. `demo`,
/// not `demo.example.com`). Omitting `ip` makes DuckDNS use the
/// requester's public IP.
pub fn update_url(domain: &str, token: &str, ip: Option<&str>) -> String {
    let mut url = format!("{DUCKDNS_UPDATE_URL}?domains={domain}&token={token}");
    if let Some(ip) = ip {
        url.push_str(&format!("&ip={ip}"));
    }
    url
}

/// DuckDNS returns `OK` (or `KO` on failure).
pub fn parse_response(body: &str) -> bool {
    body.trim().eq_ignore_ascii_case("OK")
}

/// The DuckDNS label is the first dot-segment of a subdomain.
pub fn duckdns_label(subdomain: &str) -> &str {
    subdomain.split('.').next().unwrap_or(subdomain)
}

/// Point the subdomain at the host's public IP (best-effort).
pub async fn update_subdomain(domain: &str, token: &str, ip: Option<&str>) -> anyhow::Result<bool> {
    let url = update_url(domain, token, ip);
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("duckdns update request failed for '{domain}'"))?;
    let body = response
        .text()
        .await
        .context("duckdns update response unreadable")?;
    Ok(parse_response(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_url_includes_domain_token_and_optional_ip() {
        let url = update_url("demo", "abc123", None);
        assert_eq!(
            url,
            "https://www.duckdns.org/update?domains=demo&token=abc123"
        );
        let with_ip = update_url("demo", "abc123", Some("203.0.113.9"));
        assert!(with_ip.ends_with("&ip=203.0.113.9"));
    }

    #[test]
    fn parse_response_accepts_ok_only() {
        assert!(parse_response("OK"));
        assert!(parse_response("ok\n"));
        assert!(!parse_response("KO"));
        assert!(!parse_response("ERROR"));
    }

    #[test]
    fn duckdns_label_is_first_segment() {
        assert_eq!(duckdns_label("demo.example.com"), "demo");
        assert_eq!(duckdns_label("demo"), "demo");
    }
}
