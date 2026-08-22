// update_check.rs — Background update checker.
//
// Fetches the latest release from GitHub and compares it against the running
// version.  Results are cached on disk for 24 hours so we never hammer the
// GitHub API on every startup.

use std::time::Duration;

use crate::github::{GITHUB_API_BASE, GITHUB_REPO};

const CHECK_INTERVAL_HOURS: u64 = 24;

/// Information about an available update.
#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: String,
    pub release_url: String,
    pub has_update: bool,
}

/// Check for a newer version of Clawde in the background.
///
/// Returns `Some(UpdateInfo)` when a newer release exists on GitHub.
/// The result is cached for `CHECK_INTERVAL_HOURS` hours so repeated
/// calls within that window are served from disk without a network round-trip.
pub async fn check_for_updates() -> Option<UpdateInfo> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    // --- 24-hour rate-limit cache -------------------------------------------
    if let Some(cache_path) = update_cache_path() {
        if cache_path.exists() {
            if let Ok(metadata) = std::fs::metadata(&cache_path) {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(elapsed) = modified.elapsed() {
                        if elapsed < Duration::from_secs(CHECK_INTERVAL_HOURS * 3600) {
                            // Cache is still fresh — use the stored version.
                            if let Ok(cached) = std::fs::read_to_string(&cache_path) {
                                let cached = cached.trim().to_string();
                                if cached.is_empty() {
                                    return None;
                                }
                                let has_update = is_newer(&cached, &current);
                                if has_update {
                                    return Some(UpdateInfo {
                                        current_version: current,
                                        latest_version: cached.clone(),
                                        release_url: format!(
                                            "https://github.com/{GITHUB_REPO}/releases/tag/v{cached}"
                                        ),
                                        has_update: true,
                                    });
                                }
                                return None;
                            }
                        }
                    }
                }
            }
        }
    }

    // --- Network fetch -------------------------------------------------------
    let client = crate::github::api_client();

    let resp = client
        .get(format!(
            "{GITHUB_API_BASE}/repos/{GITHUB_REPO}/releases/latest"
        ))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }

    // Record the last-seen rate-limit status for the /ctx-viz overlay (read
    // the headers before the response is consumed by `.json()`).
    if let Some(limit) = crate::github::parse_rate_limit(resp.headers()) {
        crate::github::store_rate_limit(limit);
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let tag = json.get("tag_name").and_then(|v| v.as_str())?;
    let latest = tag.trim_start_matches('v').to_string();
    let html_url = json
        .get("html_url")
        .and_then(|v| v.as_str())
        .unwrap_or(&format!("https://github.com/{GITHUB_REPO}/releases"))
        .to_string();

    // Cache the fetched version so we don't hit GitHub again for 24 h.
    if let Some(cache_path) = update_cache_path() {
        if let Some(parent) = cache_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&cache_path, &latest);
    }

    let has_update = is_newer(&latest, &current);
    if has_update {
        Some(UpdateInfo {
            current_version: current,
            latest_version: latest,
            release_url: html_url,
            has_update: true,
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn update_cache_path() -> Option<std::path::PathBuf> {
    dirs::cache_dir().map(|d| d.join("clawde").join("update_check.txt"))
}

/// Compare two semver strings.  Returns `true` when `latest` > `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u32> { v.split('.').filter_map(|p| p.parse().ok()).collect() };
    let l = parse(latest);
    let c = parse(current);
    let max_len = l.len().max(c.len());
    for i in 0..max_len {
        let lv = l.get(i).copied().unwrap_or(0);
        let cv = c.get(i).copied().unwrap_or(0);
        if lv > cv {
            return true;
        }
        if lv < cv {
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn newer_minor() {
        assert!(is_newer("0.1.0", "0.0.8"));
    }

    #[test]
    fn same_version() {
        assert!(!is_newer("0.0.8", "0.0.8"));
    }

    #[test]
    fn older_version() {
        assert!(!is_newer("0.0.5", "0.0.8"));
    }

    #[test]
    fn major_bump() {
        assert!(is_newer("1.0.0", "0.9.9"));
    }
}
