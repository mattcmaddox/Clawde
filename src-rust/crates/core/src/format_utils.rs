//! Formatting utilities for cost, duration, and token counts.
//! Mirrors src/utils/formatters.ts and related TS helpers.

/// Format a cost in USD cents as a human-readable string.
/// 0 → "$0.00", 150 → "$1.50", 0.5 → "$0.01"
pub fn format_cost_usd(cents: f64) -> String {
    if cents < 0.01 {
        "<$0.01".to_string()
    } else {
        format!("${:.2}", cents / 100.0)
    }
}

/// Format a duration in milliseconds as a human-readable string.
/// < 1000ms → "Xms", < 60s → "Xs", < 60m → "Xm Ys", else "Xh Ym"
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1_000 {
        format!("{}ms", ms)
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if ms < 3_600_000 {
        let minutes = ms / 60_000;
        let seconds = (ms % 60_000) / 1_000;
        format!("{}m {}s", minutes, seconds)
    } else {
        let hours = ms / 3_600_000;
        let minutes = (ms % 3_600_000) / 60_000;
        format!("{}h {}m", hours, minutes)
    }
}

/// Format a retry wait (cooldown remaining) as a human-readable string.
///
/// Seconds are shown only while the wait is short enough to act on — under a
/// minute. Past that the countdown drops them entirely: `5m 0s` and `2m 43s`
/// claim a precision nobody waits on, and the seconds digit is the only part
/// that changes between polls.
///
/// Examples: `48.0s`, `1m`, `5m`, `1h 5m`, `1d 2h`.
pub fn format_retry_wait(secs: u64) -> String {
    if secs < 60 {
        format_duration_ms(secs * 1_000)
    } else if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let hours = secs / 3_600;
        let minutes = (secs % 3_600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {minutes}m")
        }
    } else {
        let days = secs / 86_400;
        let hours = (secs % 86_400) / 3_600;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d {hours}h")
        }
    }
}

/// Format a token count compactly: 1234 → "1.2K", 1234567 → "1.2M"
pub fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.0}K", count as f64 / 1_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// Format a token/cost summary line for the status bar.
/// Example: "3.2K tokens · $0.04"
pub fn format_usage_summary(tokens: u64, cost_cents: f64) -> String {
    format!(
        "{} tokens · {}",
        format_tokens(tokens),
        format_cost_usd(cost_cents)
    )
}

/// Format an absolute wall-clock timestamp compactly for listings.
/// Same year → `"Aug 3 14:22"`; other year → `"Aug 3 2025"`.
/// Clock skew (mtime in the future) degrades to the raw local time.
pub fn format_short_absolute_time(mtime: std::time::SystemTime) -> String {
    use chrono::{DateTime, Datelike, Local};
    let dt: DateTime<Local> = mtime.into();
    let now = chrono::Local::now();
    if dt.year() == now.year() {
        dt.format("%b %d %H:%M").to_string()
    } else {
        dt.format("%b %d %Y").to_string()
    }
}

/// Format a relative time string (for session listings).
/// "just now", "2 minutes ago", "3 hours ago", "yesterday", "Mar 15"
pub fn format_relative_time(ts_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let diff_ms = now_ms.saturating_sub(ts_ms);
    let diff_secs = diff_ms / 1000;

    if diff_secs < 60 {
        "just now".to_string()
    } else if diff_secs < 3600 {
        let m = diff_secs / 60;
        format!("{} minute{} ago", m, if m == 1 { "" } else { "s" })
    } else if diff_secs < 86400 {
        let h = diff_secs / 3600;
        format!("{} hour{} ago", h, if h == 1 { "" } else { "s" })
    } else if diff_secs < 172800 {
        "yesterday".to_string()
    } else {
        let days = diff_secs / 86400;
        format!("{} days ago", days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_cost() {
        assert_eq!(format_cost_usd(0.0), "<$0.01");
        assert_eq!(format_cost_usd(150.0), "$1.50");
        assert_eq!(format_cost_usd(2.0), "$0.02");
    }

    #[test]
    fn format_duration() {
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(5000), "5.0s");
        assert_eq!(format_duration_ms(90_000), "1m 30s");
    }

    /// A retry wait shows seconds only under a minute; above that the seconds
    /// component is dropped, never rounded into a "0s" tail.
    #[test]
    fn retry_wait_drops_seconds_from_a_minute_up() {
        assert_eq!(format_retry_wait(0), "0ms");
        assert_eq!(format_retry_wait(48), "48.0s");
        assert_eq!(format_retry_wait(59), "59.0s");
        assert_eq!(format_retry_wait(60), "1m");
        assert_eq!(format_retry_wait(90), "1m");
        assert_eq!(format_retry_wait(300), "5m");
        assert_eq!(format_retry_wait(3_600), "1h");
        assert_eq!(format_retry_wait(3_900), "1h 5m");
        assert_eq!(format_retry_wait(90_000), "1d 1h");
    }

    #[test]
    fn format_tokens_cases() {
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1500), "1.5K");
        assert_eq!(format_tokens(50_000), "50K");
    }

    #[test]
    fn format_short_absolute_time_same_year() {
        // A fixed instant in the middle of August 2026.
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_784_000_000);
        let s = format_short_absolute_time(mtime);
        // Format is "Mon DD HH:MM" — assert shape rather than exact month name
        // so the test is timezone-independent.
        assert!(s.len() >= 11, "got {:?}", s);
        assert!(
            s.contains(' '),
            "expected a space-separated date, got {:?}",
            s
        );
        let parts: Vec<&str> = s.split(' ').collect();
        assert!(parts.len() >= 2, "got {:?}", s);
    }
}
