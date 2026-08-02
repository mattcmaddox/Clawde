// github.rs — Shared GitHub REST API configuration.
//
// Every request to the GitHub REST API must carry a valid User-Agent, the
// `application/vnd.github+json` media type, and (per the docs) an explicit
// `X-GitHub-Api-Version`. Previously each call site built its own reqwest
// client with a different User-Agent and none of them pinned the API version.
// This module is the single source of truth for the repo path and the
// headers every GitHub API request needs, so `update_check`, `upgrade`,
// `/update`, `/release-notes`, and `/review` behave consistently.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Canonical GitHub repository ("owner/repo"). The single source of truth —
/// call sites that used the pre-rename `kuberwastaken/claurst` path are
/// migrated here so update checks hit the renamed repo.
pub const GITHUB_REPO: &str = "Kuberwastaken/clawde";

/// Base URL for the GitHub REST API.
pub const GITHUB_API_BASE: &str = "https://api.github.com";

/// REST API version pinned on every request (docs: "You should use this
/// header to specify a version of the REST API to use for your request").
pub const GITHUB_API_VERSION: &str = "2022-11-28";

/// Media type for the GitHub REST API.
pub const GITHUB_MEDIA_TYPE: &str = "application/vnd.github+json";

/// The headers every GitHub REST API request must carry.
fn default_headers() -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(GITHUB_MEDIA_TYPE),
    );
    headers.insert(
        "X-GitHub-Api-Version",
        reqwest::header::HeaderValue::from_static(GITHUB_API_VERSION),
    );
    headers
}

/// Build a reqwest client pre-configured for the GitHub REST API.
///
/// Sets the required `User-Agent`, `Accept: application/vnd.github+json`,
/// and `X-GitHub-Api-Version: 2022-11-28` headers plus a 10s timeout.
/// Callers add `Authorization` themselves when the endpoint requires it.
///
/// The builder config is static, so `expect` is safe here.
pub fn api_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("clawde/{}", crate::constants::APP_VERSION))
        .default_headers(default_headers())
        .build()
        .expect("static github client config")
}

// ---------------------------------------------------------------------------
// Rate-limit headers
// ---------------------------------------------------------------------------

/// GitHub API rate-limit status, as reported on every REST response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Total requests allowed in the current window.
    pub limit: u64,
    /// Requests remaining in the current window.
    pub remaining: u64,
    /// Unix timestamp (seconds) at which the window resets.
    pub reset_unix: u64,
}

/// Parse `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `X-RateLimit-Reset`
/// out of a GitHub API response header map. Returns `None` when any header
/// is absent or unparsable.
pub fn parse_rate_limit(headers: &reqwest::header::HeaderMap) -> Option<RateLimit> {
    let limit = headers
        .get("x-ratelimit-limit")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    let remaining = headers
        .get("x-ratelimit-remaining")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    let reset_unix = headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(RateLimit {
        limit,
        remaining,
        reset_unix,
    })
}

// ---------------------------------------------------------------------------
// Last-seen rate-limit (read by /ctx-viz, /update, and the TUI footer)
// ---------------------------------------------------------------------------

static LAST_RATE_LIMIT: OnceLock<Mutex<Option<RateLimit>>> = OnceLock::new();

fn last_rate_limit_slot() -> &'static Mutex<Option<RateLimit>> {
    LAST_RATE_LIMIT.get_or_init(|| Mutex::new(load_rate_limit_from_disk()))
}

fn store_rate_limit_in_memory(limit: RateLimit) {
    if let Ok(mut guard) = last_rate_limit_slot().lock() {
        *guard = Some(limit);
    }
}

/// Remember the most recent GitHub API rate-limit status seen by any GitHub
/// API caller (update check, `/update`, `/release-notes`), and persist it so
/// a fresh launch shows the last-known quota before the first API call.
/// Consumed by `/ctx-viz`, the TUI footer, and the `/update` 403 path.
pub fn store_rate_limit(limit: RateLimit) {
    store_rate_limit_in_memory(limit);
    save_rate_limit_to_disk(&limit);
}

/// The most recently observed GitHub API rate-limit status, or `None` when no
/// GitHub API call has succeeded yet this session. Falls back to the value
/// persisted by a previous session when nothing has been recorded in memory.
pub fn last_rate_limit() -> Option<RateLimit> {
    last_rate_limit_slot().lock().ok().and_then(|g| *g)
}

// --- Disk persistence (mirrors update_check.txt) ---------------------------

const RATE_LIMIT_CACHE_FILE: &str = "github_rate_limit.txt";

fn rate_limit_cache_path() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("clawde").join(RATE_LIMIT_CACHE_FILE))
}

/// Encode a rate limit as `"<limit> <remaining> <reset_unix>"`.
fn encode_rate_limit(limit: &RateLimit) -> String {
    format!("{} {} {}", limit.limit, limit.remaining, limit.reset_unix)
}

/// Decode the format written by [`encode_rate_limit`]. Returns `None` for
/// empty or malformed content.
fn decode_rate_limit(s: &str) -> Option<RateLimit> {
    let mut parts = s.split_whitespace();
    let limit = parts.next()?.parse().ok()?;
    let remaining = parts.next()?.parse().ok()?;
    let reset_unix = parts.next()?.parse().ok()?;
    Some(RateLimit {
        limit,
        remaining,
        reset_unix,
    })
}

/// `true` when the persisted reset window has already passed, meaning the
/// cached status is from a previous window and must not be surfaced.
fn is_rate_limit_expired(limit: &RateLimit, now: u64) -> bool {
    limit.reset_unix <= now
}

fn load_rate_limit_from_disk() -> Option<RateLimit> {
    let path = rate_limit_cache_path()?;
    let limit = decode_rate_limit(&std::fs::read_to_string(&path).ok()?)?;
    // A persisted status whose reset window has already passed is stale — a
    // fresh launch must not show a quota from a previous window. Remove the
    // stale file so a later launch reads nothing rather than a dead value.
    if is_rate_limit_expired(&limit, unix_now()) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(limit)
}

fn save_rate_limit_to_disk(limit: &RateLimit) {
    let Some(path) = rate_limit_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, encode_rate_limit(limit));
}

/// Human-friendly note when the remaining quota is low (≤ 5 requests).
/// Returns `None` while the quota is healthy.
/// Current wall-clock time as a Unix timestamp in seconds.
/// Falls back to `0` if the system clock is before the Unix epoch.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human-friendly reset countdown for a rate-limit window. Returns
/// "resets in ~X min" for future resets ≥ 1 minute out, and "resets
/// shortly" for stale (clock-skewed) or sub-minute resets — never a bogus
/// "~0 min" countdown.
pub fn format_reset(reset_unix: u64, now: u64) -> String {
    match reset_unix.checked_sub(now) {
        Some(secs) if secs >= 60 => format!("resets in ~{} min", secs / 60),
        _ => "resets shortly".to_string(),
    }
}

pub fn rate_limit_warning(limit: &RateLimit) -> Option<String> {
    if limit.remaining > 5 {
        return None;
    }
    let reset_clause = format_reset(limit.reset_unix, unix_now());
    if limit.remaining == 0 {
        Some(format!(
            "GitHub API rate limit exhausted (0 requests remaining) — {reset_clause}"
        ))
    } else {
        Some(format!(
            "GitHub API rate limit low: {} requests remaining, {reset_clause}",
            limit.remaining
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn rate_limit_headers(limit: &str, remaining: &str, reset: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("X-RateLimit-Limit", HeaderValue::from_str(limit).unwrap());
        headers.insert(
            "X-RateLimit-Remaining",
            HeaderValue::from_str(remaining).unwrap(),
        );
        headers.insert("X-RateLimit-Reset", HeaderValue::from_str(reset).unwrap());
        headers
    }

    #[test]
    fn default_headers_carry_media_type_and_api_version() {
        let headers = default_headers();
        assert_eq!(
            headers.get(reqwest::header::ACCEPT).unwrap(),
            GITHUB_MEDIA_TYPE
        );
        assert_eq!(
            headers.get("X-GitHub-Api-Version").unwrap(),
            GITHUB_API_VERSION
        );
    }

    #[test]
    fn constants_shape() {
        // owner/repo, with no leading or trailing slash.
        let parts: Vec<&str> = GITHUB_REPO.split('/').collect();
        assert_eq!(parts.len(), 2);
        assert!(!parts[0].is_empty() && !parts[1].is_empty());

        assert!(GITHUB_API_BASE.starts_with("https://"));
        assert!(!GITHUB_API_BASE.ends_with('/'));

        // Version looks like a date: YYYY-MM-DD.
        let fields: Vec<&str> = GITHUB_API_VERSION.split('-').collect();
        assert_eq!(fields.len(), 3);
        assert!(fields.iter().all(|f| f.chars().all(|c| c.is_ascii_digit())));

        assert!(GITHUB_MEDIA_TYPE.starts_with("application/vnd.github"));
    }

    #[test]
    fn parse_rate_limit_reads_headers_case_insensitively() {
        let headers = rate_limit_headers("60", "12", "1700000000");
        let limit = parse_rate_limit(&headers).expect("valid headers should parse");
        assert_eq!(limit.limit, 60);
        assert_eq!(limit.remaining, 12);
        assert_eq!(limit.reset_unix, 1700000000);
    }

    #[test]
    fn parse_rate_limit_missing_header_is_none() {
        assert_eq!(parse_rate_limit(&HeaderMap::new()), None);
    }

    #[test]
    fn parse_rate_limit_invalid_value_is_none() {
        let headers = rate_limit_headers("60", "not-a-number", "1700000000");
        assert_eq!(parse_rate_limit(&headers), None);
    }

    #[test]
    fn rate_limit_warning_healthy_is_none() {
        let limit = RateLimit {
            limit: 60,
            remaining: 100,
            reset_unix: 0,
        };
        assert_eq!(rate_limit_warning(&limit), None);
    }

    #[test]
    fn rate_limit_warning_low_reports_reset() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let limit = RateLimit {
            limit: 60,
            remaining: 2,
            reset_unix: now + 1800,
        };
        let warning = rate_limit_warning(&limit).expect("low quota should warn");
        assert!(warning.contains("2 requests remaining"));
        assert!(warning.contains("30 min"));
    }

    #[test]
    fn rate_limit_warning_exhausted() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let limit = RateLimit {
            limit: 60,
            remaining: 0,
            reset_unix: now + 3600,
        };
        let warning = rate_limit_warning(&limit).expect("exhausted quota should warn");
        assert!(warning.contains("exhausted"));
    }

    #[test]
    fn rate_limit_warning_stale_reset_says_shortly() {
        // Reset timestamp in the past (clock skew / stale headers) must not
        // produce a bogus "resets in ~0 min" countdown.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let limit = RateLimit {
            limit: 60,
            remaining: 1,
            reset_unix: now - 10,
        };
        let warning = rate_limit_warning(&limit).expect("stale reset should still warn");
        assert!(warning.contains("resets shortly"));
        assert!(!warning.contains("0 min"));
    }

    #[test]
    fn last_rate_limit_store_round_trips() {
        // Restore the prior static value even if an assertion panics, so the
        // shared store is never left polluted for other tests in this binary.
        // Uses the in-memory-only variant so the test never writes to the real
        // cache dir (store_rate_limit also persists to disk).
        struct Restore(Option<RateLimit>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Ok(mut guard) = last_rate_limit_slot().lock() {
                    *guard = self.0;
                }
            }
        }
        let _restore = Restore(last_rate_limit());

        store_rate_limit_in_memory(RateLimit {
            limit: 60,
            remaining: 42,
            reset_unix: 1_700_000_000,
        });
        assert_eq!(
            last_rate_limit(),
            Some(RateLimit {
                limit: 60,
                remaining: 42,
                reset_unix: 1_700_000_000,
            })
        );
    }

    #[test]
    fn encode_decode_rate_limit_round_trips() {
        let limit = RateLimit {
            limit: 60,
            remaining: 7,
            reset_unix: 1_700_000_000,
        };
        let encoded = encode_rate_limit(&limit);
        assert_eq!(decode_rate_limit(&encoded), Some(limit));

        // Malformed / empty input yields None rather than a panic.
        assert_eq!(decode_rate_limit(""), None);
        assert_eq!(decode_rate_limit("60 not-a-number 1700000000"), None);
        assert_eq!(decode_rate_limit("60 7"), None);
    }

    #[test]
    fn persisted_limit_expires_after_reset_window() {
        let now = unix_now();
        // Future reset: still fresh.
        assert!(!is_rate_limit_expired(
            &RateLimit {
                limit: 60,
                remaining: 12,
                reset_unix: now + 1800,
            },
            now
        ));
        // Reset already passed (or clock skew): stale, must not surface.
        assert!(is_rate_limit_expired(
            &RateLimit {
                limit: 60,
                remaining: 12,
                reset_unix: now - 10,
            },
            now
        ));
        assert!(is_rate_limit_expired(
            &RateLimit {
                limit: 60,
                remaining: 12,
                reset_unix: now,
            },
            now
        ));
    }

    #[test]
    fn format_reset_handles_stale_and_future() {
        assert!(format_reset(1_800, 0).contains("30 min"));
        assert!(format_reset(59, 0).contains("shortly"));
        assert!(format_reset(0, 1_800).contains("shortly"));
    }
}
