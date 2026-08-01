// time_extract.rs — Cooldown/retry-time extraction from API error responses.
//
// Provides utilities for extracting timing information from:
//   - HTTP `Retry-After` headers (delta-seconds and HTTP-date)
//   - Error response bodies (free-text patterns like "retry after N seconds",
//     "reset in N seconds", "resets at <ISO 8601>", etc.)
//   - Structured JSON error codes ("insufficient_quota", "rate_limit_error")
//
// These feed into the `KeyRing` cooldown tracker so the system knows how long
// to wait before retrying an exhausted key.

use std::time::{SystemTime, UNIX_EPOCH};

/// Confidence level for a cooldown estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// From an explicit HTTP header (Retry-After).
    High,
    /// Parsed from a human-readable error body string.
    Medium,
    /// Default fallback based on the error type (no specific timing info).
    Low,
}

/// Extracted cooldown/retry information from an API error response.
#[derive(Debug, Clone)]
pub struct ExtractedCooldown {
    /// Seconds to wait before retrying this key. `None` if no timing info
    /// could be extracted at all.
    pub retry_after_secs: Option<u64>,
    /// How confident we are in this estimate.
    pub confidence: Confidence,
    /// A human-readable description of the source (for logging/display).
    pub source: &'static str,
}

impl ExtractedCooldown {
    /// A cooldown with no usable timing information — caller should use a
    /// sensible default (e.g. 60s for rate limits, 3600s for quota).
    pub fn unknown() -> Self {
        Self {
            retry_after_secs: None,
            confidence: Confidence::Low,
            source: "unknown",
        }
    }

    /// Unwrap the retry-after value, falling back to `default_secs` if None.
    pub fn or_secs(self, default_secs: u64) -> u64 {
        self.retry_after_secs.unwrap_or(default_secs)
    }

    /// Combine all sources: `fallback` is used when `self` has no retry-after.
    pub fn or(self, fallback: ExtractedCooldown) -> ExtractedCooldown {
        if self.retry_after_secs.is_some() {
            self
        } else {
            fallback
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Parse the value of an HTTP `Retry-After` header.
///
/// Supports both formats defined in RFC 7231:
/// - **Delta-seconds**: `Retry-After: 120`  (a plain integer)
/// - **HTTP-date**:     `Retry-After: Wed, 21 Oct 2015 07:28:00 GMT`
pub fn extract_retry_after_header(value: &str) -> Option<u64> {
    let trimmed = value.trim();

    // Try delta-seconds first (most common).
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(secs);
    }

    // Try HTTP-date format using the `time` crate's parsing (or manual).
    parse_http_date_duration(trimmed)
}

/// Scan a human-readable error body for time-related patterns.
///
/// Matches patterns like:
/// - "retry after N seconds" / "retry after Ns"
/// - "try again in N seconds" / "try again in N minutes"
/// - "reset in N seconds" / "reset in Ns"
/// - "please wait N seconds before"
/// - "rate limit exceeded" (no time — returns None)
/// - "N seconds" preceded by retry/wait/backoff keywords
/// - "N minutes" with retry context
pub fn extract_cooldown_from_body(body: &str) -> Option<u64> {
    let lower = body.to_lowercase();

    // Try ISO 8601 "resets at" timestamps first.
    if let Some(ts) = extract_reset_timestamp_from_str(&lower) {
        return Some(ts);
    }

    // Timing keywords to search for, followed by a number + unit.
    let keywords = &[
        "retry after",
        "try again in",
        "reset in",
        "please wait",
        "back off",
        "retry in",
        "will reset",
        "available again in",
    ];

    for &keyword in keywords {
        if let Some(secs) = parse_time_after_keyword(&lower, keyword) {
            return Some(secs);
        }
    }

    None
}

/// Extract a Unix timestamp from an ISO 8601 date string embedded in text.
///
/// Looks for patterns like:
/// - "resets at 2026-07-28T00:00:00Z"
/// - "resets at 2026-07-28T00:00:00+00:00"
/// - "available at 2026-07-28 00:00:00 UTC"
pub fn extract_reset_timestamp(body: &str) -> Option<u64> {
    let lower = body.to_lowercase();
    extract_reset_timestamp_from_str(&lower)
}

/// High-level cooldown estimation combining header + body + error metadata.
///
/// Precedence:
///   1. `retry_after_header` (if present and parseable) — High confidence
///   2. Body text pattern matching — Medium confidence
///   3. Default fallback based on error type — Low confidence
///
/// `err_message` is the human-readable message from the error (e.g. from
/// `ProviderError`'s message field). `raw_body` is the full error response
/// body (may contain more context).
pub fn estimate_cooldown(
    retry_after_header: Option<&str>,
    err_message: &str,
    raw_body: Option<&str>,
) -> ExtractedCooldown {
    // 1. Header — highest confidence.
    if let Some(header_val) = retry_after_header {
        if let Some(secs) = extract_retry_after_header(header_val) {
            return ExtractedCooldown {
                retry_after_secs: Some(secs),
                confidence: Confidence::High,
                source: "Retry-After header",
            };
        }
    }

    // 2. Body text — medium confidence. Try the raw body first (more data),
    //    then the message text.
    let body_to_scan = raw_body.unwrap_or(err_message);
    if let Some(secs) = extract_cooldown_from_body(body_to_scan) {
        return ExtractedCooldown {
            retry_after_secs: Some(secs),
            confidence: Confidence::Medium,
            source: "error body text",
        };
    }

    // Also try just the message text (shorter, often cleaner).
    if raw_body.is_some() {
        if let Some(secs) = extract_cooldown_from_body(err_message) {
            return ExtractedCooldown {
                retry_after_secs: Some(secs),
                confidence: Confidence::Medium,
                source: "error message text",
            };
        }
    }

    // 3. No timing info found.
    ExtractedCooldown::unknown()
}

/// Get a sensible default cooldown (in seconds) for a given HTTP status code
/// and error context. Used when no timing info could be extracted.
pub fn default_cooldown_for_status(status: u16) -> u64 {
    match status {
        429 => 60,       // Rate limit: retry after 1 minute
        401 | 403 => 0,  // Auth failure: immediate retry won't help
        500..=599 => 30, // Server error: back off 30 seconds
        _ => 60,         // Default: 1 minute
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Parse an RFC 7231 HTTP-date string and return seconds until that time.
fn parse_http_date_duration(date_str: &str) -> Option<u64> {
    // RFC 7231 format: "Wed, 21 Oct 2015 07:28:00 GMT"
    // Try to parse it using chrono (which is already a dependency).
    use chrono::NaiveDateTime;

    // Strip day-of-week prefix and timezone suffix, parse what remains.
    let cleaned = date_str
        .trim()
        .trim_start_matches(|c: char| c.is_ascii_alphabetic())
        .trim_start_matches(", ")
        .trim_end_matches(" GMT")
        .trim_end_matches(" UTC");

    // Try multiple date formats.
    let fmts = &["%d %b %Y %H:%M:%S", "%Y-%m-%dT%H:%M:%S"];

    for fmt in fmts {
        if let Ok(dt) = NaiveDateTime::parse_from_str(cleaned, fmt) {
            let epoch =
                NaiveDateTime::parse_from_str("1970-01-01T00:00:00", "%Y-%m-%dT%H:%M:%S").ok()?;
            let seconds = dt.signed_duration_since(epoch).num_seconds();
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;

            let delta = seconds - now;
            if delta > 0 {
                return Some(delta as u64);
            }
        }
    }

    None
}

/// Extract a Unix timestamp (seconds since epoch) from text containing
/// "resets at <ISO 8601>" or similar patterns.
fn extract_reset_timestamp_from_str(lower: &str) -> Option<u64> {
    use chrono::{NaiveDateTime, TimeZone, Utc};

    // Look for ISO 8601 patterns near reset keywords.
    let reset_keywords = &["resets at", "available at", "until", "resets on"];

    for kw in reset_keywords {
        if let Some(pos) = lower.find(kw) {
            let after = &lower[pos + kw.len()..];
            // Try to extract a date from the next ~35 chars.
            let candidate = after.trim_start();
            // Snip at common delimiters.
            let end = candidate
                .find(|c| ['.', ',', ')', '\n'].contains(&c))
                .unwrap_or(candidate.len().min(40));
            let date_str = &candidate[..end].trim();

            // Normalise casing: input was lowercased for keyword search, but
            // chrono's format parser is case-sensitive (expects 'T', 'Z').
            let upper = date_str.to_uppercase();
            let mut cleaned: &str = upper.trim();

            // Remove trailing "Z" or "UTC".
            if cleaned.ends_with('Z') || cleaned.ends_with("UTC") {
                cleaned = cleaned.trim_end_matches('Z').trim_end_matches("UTC").trim();
            }
            // Remove trailing +HH:MM or -HH:MM offset.
            if cleaned.len() > 6 {
                let maybe_tz = &cleaned[cleaned.len() - 6..];
                if (maybe_tz.starts_with('+') || maybe_tz.starts_with('-'))
                    && maybe_tz.chars().nth(3) == Some(':')
                {
                    cleaned = cleaned[..cleaned.len() - 6].trim();
                }
            }

            // Try common datetime formats, then date-only.
            if let Ok(naive) = NaiveDateTime::parse_from_str(cleaned, "%Y-%m-%dT%H:%M:%S") {
                return Some(Utc.from_utc_datetime(&naive).timestamp() as u64);
            }
            if let Ok(naive) = NaiveDateTime::parse_from_str(cleaned, "%Y-%m-%d %H:%M:%S") {
                return Some(Utc.from_utc_datetime(&naive).timestamp() as u64);
            }
            if let Ok(naive_date) = chrono::NaiveDate::parse_from_str(cleaned, "%Y-%m-%d") {
                let naive = naive_date.and_hms_opt(0, 0, 0).unwrap();
                return Some(Utc.from_utc_datetime(&naive).timestamp() as u64);
            }
        }
    }

    None
}

/// Find a number after a keyword and multiply by the implied unit to get
/// seconds. E.g. "retry after 30 seconds" → 30, "try again in 5 minutes" → 300.
/// Handles filler words like "for" between keyword and number, e.g.
/// "back off for 2 minutes" → 120.
fn parse_time_after_keyword(text: &str, keyword: &str) -> Option<u64> {
    let kw_lower = keyword.to_lowercase();
    let mut search_start = 0;

    while let Some(pos) = text[search_start..].find(&kw_lower) {
        let abs_pos = search_start + pos;
        let after = &text[abs_pos + kw_lower.len()..];
        let after = after.trim_start();

        // Skip over any non-numeric words (like "for", "of", "about") before
        // the actual number.
        let after = skip_filler_words(after);

        // Extract the first number.
        let num_end = after
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after.len());
        let num_str = &after[..num_end];
        if num_str.is_empty() {
            search_start = abs_pos + 1;
            continue;
        }
        let num: u64 = match num_str.parse() {
            Ok(n) if n > 0 => n,
            _ => {
                search_start = abs_pos + 1;
                continue;
            }
        };
        let after_num = after[num_end..].trim_start();

        // Determine the unit (seconds, minutes, hours, or just "s", "m", "h").
        let seconds = if after_num.starts_with("hour")
            || after_num.starts_with("hours")
            || after_num.starts_with('h') && !after_num.starts_with("http")
        {
            num.checked_mul(3600)?
        } else if after_num.starts_with("minute")
            || after_num.starts_with("minutes")
            || after_num.starts_with('m')
        {
            num.checked_mul(60)?
        } else if after_num.starts_with("second")
            || after_num.starts_with("seconds")
            || after_num.starts_with('s')
        {
            num
        } else {
            // No unit word found — assume seconds if the keyword was
            // explicitly a retry keyword.
            num
        };

        return Some(seconds);
    }

    None
}

/// Skip leading filler words (non-numeric tokens) in a string, returning
/// the rest starting from a digit or the end.
fn skip_filler_words(s: &str) -> &str {
    let mut rest = s;
    loop {
        let trimmed = rest.trim_start();
        // If empty or starts with a digit, we're done.
        if trimmed.is_empty() || trimmed.starts_with(|c: char| c.is_ascii_digit()) {
            return trimmed;
        }
        // Find the end of this word and skip it.
        let word_end = trimmed
            .find(|c: char| c.is_whitespace())
            .unwrap_or(trimmed.len());
        if word_end == trimmed.len() {
            return "";
        }
        rest = &trimmed[word_end..];
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- extract_retry_after_header ----

    #[test]
    fn header_delta_seconds() {
        assert_eq!(extract_retry_after_header("120"), Some(120));
        assert_eq!(extract_retry_after_header("0"), Some(0));
        assert_eq!(extract_retry_after_header("   30   "), Some(30));
    }

    #[test]
    fn header_invalid_returns_none() {
        assert!(extract_retry_after_header("abc").is_none());
        assert!(extract_retry_after_header("").is_none());
        assert!(extract_retry_after_header(" ").is_none());
    }

    // ---- extract_cooldown_from_body ----

    #[test]
    fn retry_after_seconds() {
        assert_eq!(
            extract_cooldown_from_body("Rate limit exceeded. Retry after 30 seconds."),
            Some(30)
        );
    }

    #[test]
    fn try_again_in_minutes() {
        assert_eq!(
            extract_cooldown_from_body("Too many requests. Try again in 5 minutes."),
            Some(300)
        );
    }

    #[test]
    fn reset_in_seconds() {
        assert_eq!(
            extract_cooldown_from_body("Free tier limit reset in 3600 seconds"),
            Some(3600)
        );
    }

    #[test]
    fn retry_in_hours() {
        assert_eq!(
            extract_cooldown_from_body("Quota exceeded. Will reset in 2 hours."),
            Some(7200)
        );
    }

    #[test]
    fn no_timing_info_returns_none() {
        assert!(extract_cooldown_from_body("Something went wrong").is_none());
        assert!(extract_cooldown_from_body("").is_none());
        assert!(extract_cooldown_from_body("rate limit exceeded").is_none());
    }

    #[test]
    fn please_wait_pattern() {
        assert_eq!(
            extract_cooldown_from_body("Please wait 45 seconds before retrying."),
            Some(45)
        );
    }

    #[test]
    fn retry_in_seconds_short() {
        assert_eq!(
            extract_cooldown_from_body("Rate limit exceeded. Retry in 30s"),
            Some(30)
        );
        assert_eq!(
            extract_cooldown_from_body("Rate limit exceeded. Retry in 5m"),
            Some(300)
        );
        assert_eq!(
            extract_cooldown_from_body("Rate limit exceeded. Retry in 1h"),
            Some(3600)
        );
    }

    #[test]
    fn available_again_pattern() {
        assert_eq!(
            extract_cooldown_from_body("API will be available again in 60 seconds."),
            Some(60)
        );
    }

    #[test]
    fn back_off_pattern() {
        assert_eq!(
            extract_cooldown_from_body("Back off for 2 minutes"),
            Some(120)
        );
    }

    // ---- extract_reset_timestamp ----

    #[test]
    fn reset_timestamp_iso8601() {
        // This test uses a future date so the diff is always positive.
        // We can't hardcode the expected value since it depends on "now".
        let body = "Quota resets at 2099-01-01T00:00:00Z";
        let secs = extract_reset_timestamp(body);
        assert!(secs.is_some());
        assert!(secs.unwrap() > 4_000_000_000); // well in the future
    }

    #[test]
    fn reset_timestamp_with_offset() {
        let body = "Quota resets at 2099-01-01T00:00:00+00:00";
        assert!(extract_reset_timestamp(body).is_some());
    }

    #[test]
    fn reset_timestamp_no_match() {
        assert!(extract_reset_timestamp("no date here").is_none());
        assert!(extract_reset_timestamp("").is_none());
    }

    #[test]
    fn reset_timestamp_space_separated() {
        let body = "Free tier available at 2099-06-15 12:00:00";
        assert!(extract_reset_timestamp(body).is_some());
    }

    // ---- estimate_cooldown ----

    #[test]
    fn estimate_header_takes_priority() {
        let result = estimate_cooldown(
            Some("120"),
            "Rate limit exceeded",
            Some("{\"error\": {\"message\": \"rate limited\"}}"),
        );
        assert_eq!(result.retry_after_secs, Some(120));
        assert_eq!(result.confidence, Confidence::High);
    }

    #[test]
    fn estimate_body_fallback() {
        let result = estimate_cooldown(None, "Retry after 30 seconds.", None);
        assert_eq!(result.retry_after_secs, Some(30));
        assert_eq!(result.confidence, Confidence::Medium);
    }

    #[test]
    fn estimate_unknown_when_no_info() {
        let result = estimate_cooldown(None, "something broke", None);
        assert!(result.retry_after_secs.is_none());
        assert_eq!(result.confidence, Confidence::Low);
    }

    #[test]
    fn estimate_body_from_raw_overrides_message() {
        // Raw body has timing info, message doesn't.
        let result = estimate_cooldown(
            None,
            "rate limit error",
            Some("{\"error\": {\"message\": \"Retry after 45 seconds.\"}}"),
        );
        assert_eq!(result.retry_after_secs, Some(45));
    }

    // ---- default_cooldown_for_status ----

    #[test]
    fn default_429_is_60() {
        assert_eq!(default_cooldown_for_status(429), 60);
    }

    #[test]
    fn default_500_is_30() {
        assert_eq!(default_cooldown_for_status(503), 30);
    }

    #[test]
    fn default_401_is_0() {
        assert_eq!(default_cooldown_for_status(401), 0);
        assert_eq!(default_cooldown_for_status(403), 0);
    }

    #[test]
    fn default_unknown_is_60() {
        assert_eq!(default_cooldown_for_status(400), 60);
        assert_eq!(default_cooldown_for_status(0), 60);
    }

    // ---- ExtractedCooldown helpers ----

    #[test]
    fn extracted_or_secs_uses_default() {
        let c = ExtractedCooldown::unknown();
        assert_eq!(c.or_secs(60), 60);
    }

    #[test]
    fn extracted_or_secs_uses_value() {
        let c = ExtractedCooldown {
            retry_after_secs: Some(30),
            confidence: Confidence::High,
            source: "test",
        };
        assert_eq!(c.or_secs(60), 30);
    }

    #[test]
    fn extracted_or_fallback() {
        let a = ExtractedCooldown::unknown();
        let b = ExtractedCooldown {
            retry_after_secs: Some(30),
            confidence: Confidence::Medium,
            source: "test",
        };
        let combined = a.or(b);
        assert_eq!(combined.retry_after_secs, Some(30));
    }

    #[test]
    fn extracted_or_skips_fallback() {
        let a = ExtractedCooldown {
            retry_after_secs: Some(10),
            confidence: Confidence::High,
            source: "header",
        };
        let b = ExtractedCooldown {
            retry_after_secs: Some(30),
            confidence: Confidence::Medium,
            source: "body",
        };
        let combined = a.or(b);
        assert_eq!(combined.retry_after_secs, Some(10));
        assert_eq!(combined.source, "header");
    }

    // ---- Case insensitivity ----

    #[test]
    fn retry_after_case_insensitive() {
        assert_eq!(
            extract_cooldown_from_body("RATE LIMIT EXCEEDED. RETRY AFTER 30 SECONDS."),
            Some(30)
        );
    }

    #[test]
    fn mixed_case_matches() {
        assert_eq!(
            extract_cooldown_from_body("Please Wait 10 seconds before trying again"),
            Some(10)
        );
    }

    // ---- Edge cases ----

    #[test]
    fn very_large_number_is_safe() {
        // 100 million seconds is ~3.17 years — should still parse.
        let result = extract_cooldown_from_body("retry after 100000000 seconds");
        assert_eq!(result, Some(100000000));
    }

    #[test]
    fn zero_is_not_a_valid_retry() {
        // "retry after 0 seconds" doesn't make sense — skip it.
        let result = extract_cooldown_from_body("retry after 0 seconds");
        assert!(result.is_none());
    }

    #[test]
    fn keyword_not_followed_by_number() {
        let result = extract_cooldown_from_body("retry after some time");
        assert!(result.is_none());
    }
}
