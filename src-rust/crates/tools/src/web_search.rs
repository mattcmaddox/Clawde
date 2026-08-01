// WebSearch tool that queries SearXNG, the Firecrawl Search API, or DuckDuckGo
// depending on which backend is configured.
//
// Key sources (checked in order):
//   SEARXNG_URL         → SearXNG URLs (comma-separated for rotation)
//   FIRECRAWL_API_KEY   → Firecrawl keys (comma-separated for rotation)
//   AuthStore firecrawl → Firecrawl keys from /keys command
//   (none)              → DuckDuckGo (fallback, Instant Answers only)
//
// Cooldown tracking is used for Firecrawl keys: after a key is exhausted
// (429/401/403), it is skipped for a cooldown period before being retried.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tracing::debug;

pub struct WebSearchTool;

// ---------------------------------------------------------------------------
// Cooldown persistence (like KeyRing, but for Firecrawl keys)
// ---------------------------------------------------------------------------

/// Persisted cooldown entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FirecrawlCooldownEntry {
    /// The API key.
    key: String,
    /// Unix timestamp (seconds since epoch) when this key can be retried.
    cooldown_until_secs: u64,
    /// Human-readable reason for the cooldown.
    reason: String,
}

/// Returns the path to the Firecrawl cooldown state file.
fn cooldown_state_path() -> PathBuf {
    let dir = clawde_core::config::Settings::config_dir();
    dir.join("firecrawl_cooldowns.json")
}

/// Global cooldown tracker for Firecrawl API keys.
/// Maps key → Instant when the key was exhausted and should be skipped.
static FIRECRAWL_COOLDOWNS: OnceLock<std::sync::Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn firecrawl_cooldowns() -> &'static std::sync::Mutex<HashMap<String, Instant>> {
    FIRECRAWL_COOLDOWNS.get_or_init(|| {
        let m = std::sync::Mutex::new(HashMap::new());
        // Load persisted state on first access.
        if let Ok(entries) = load_cooldowns_from_disk() {
            if let Ok(mut guard) = m.lock() {
                for entry in entries {
                    let until = Instant::now()
                        + Duration::from_secs(
                            entry.cooldown_until_secs.saturating_sub(
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs(),
                            ),
                        );
                    if Instant::now() < until {
                        guard.insert(entry.key, until);
                    }
                }
            }
        }
        m
    })
}

/// Persist current cooldowns to disk.
fn persist_cooldowns() {
    let guard = match firecrawl_cooldowns().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entries: Vec<FirecrawlCooldownEntry> = guard
        .iter()
        .map(|(key, until)| {
            let cooldown_until_secs =
                now_secs + until.saturating_duration_since(Instant::now()).as_secs();
            FirecrawlCooldownEntry {
                key: key.clone(),
                cooldown_until_secs,
                reason: String::new(),
            }
        })
        .collect();
    drop(guard);

    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        if let Some(parent) = cooldown_state_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(cooldown_state_path(), json);
    }
}

/// Load persisted cooldown entries from disk.
fn load_cooldowns_from_disk() -> Result<Vec<FirecrawlCooldownEntry>, String> {
    let path = cooldown_state_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

/// Public API: return the health summary for Firecrawl keys.
/// Each tuple is (key_preview, is_active, cooldown_remaining_secs).
/// Used by /keys health firecrawl.
pub fn firecrawl_key_health() -> Vec<(String, bool, u64)> {
    let guard = match firecrawl_cooldowns().lock() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    let now = Instant::now();
    let mut results: Vec<(String, bool, u64)> = guard
        .iter()
        .map(|(key, until)| {
            if now < *until {
                (
                    key.clone(),
                    false,
                    until.saturating_duration_since(now).as_secs(),
                )
            } else {
                (key.clone(), true, 0)
            }
        })
        .collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));
    results
}

/// Default cooldown durations per error type.
mod cooldown {
    use std::time::Duration;
    pub const RATE_LIMIT: Duration = Duration::from_secs(60);
    pub const AUTH_FAILURE: Duration = Duration::from_secs(300);
    pub const NETWORK_ERROR: Duration = Duration::from_secs(30);
}

// ---------------------------------------------------------------------------
// Search backend tracking
// ---------------------------------------------------------------------------

/// The last search backend used. "searxng", "firecrawl", or "duckduckgo".
static LAST_SEARCH_BACKEND: OnceLock<std::sync::Mutex<String>> = OnceLock::new();

fn last_search_backend() -> &'static std::sync::Mutex<String> {
    LAST_SEARCH_BACKEND.get_or_init(|| std::sync::Mutex::new(String::new()))
}

/// Record which backend was used for the last search.
fn record_backend(name: &str) {
    if let Ok(mut guard) = last_search_backend().lock() {
        *guard = name.to_string();
    }
}

/// Check if a specific search backend is properly configured (has the required env vars).
/// Returns Ok(()) if configured, or an error message explaining what's missing.
pub fn check_backend_configured(backend: &str) -> Result<(), String> {
    match backend {
        "searxng" => {
            if std::env::var("SEARXNG_URL")
                .ok()
                .filter(|s| !s.is_empty())
                .is_some()
            {
                Ok(())
            } else {
                Err("SEARXNG_URL env var not set".to_string())
            }
        }
        "firecrawl" => {
            let keys = collect_firecrawl_keys();
            if keys.is_empty() {
                Err("No Firecrawl API key configured (FIRECRAWL_API_KEY env var or /keys set firecrawl)"
                    .to_string())
            } else {
                Ok(())
            }
        }
        "duckduckgo" => Ok(()),
        "auto" => {
            // Auto mode always has DuckDuckGo as fallback
            Ok(())
        }
        other => Err(format!("Unknown search backend '{}'", other)),
    }
}

/// Public API: return the last search backend name.
pub fn get_last_search_backend() -> String {
    last_search_backend()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Parse a comma-separated list from an env var value into individual items.
fn parse_comma_separated(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Collect Firecrawl API keys from all sources: env var first, then AuthStore.
pub fn collect_firecrawl_keys() -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. From env var (backward compatible)
    if let Ok(env_val) = std::env::var("FIRECRAWL_API_KEY") {
        for key in parse_comma_separated(&env_val) {
            if seen.insert(key.clone()) {
                keys.push(key);
            }
        }
    }

    // 2. From AuthStore (managed via /keys command)
    let store = clawde_core::AuthStore::load();
    if let Some(stored_keys) = store.keys_for("firecrawl") {
        for key in stored_keys {
            if seen.insert(key.clone()) {
                keys.push(key.clone());
            }
        }
    }

    keys
}

#[derive(Debug, Deserialize)]
struct WebSearchInput {
    query: String,
    #[serde(default = "default_num_results")]
    num_results: usize,
}

fn default_num_results() -> usize {
    5
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_WEB_SEARCH
    }

    fn description(&self) -> &str {
        "Search the web for information. Returns a list of relevant web pages with \
         titles, URLs, and snippets. Use this when you need current information \
         not available in your training data, or when searching for documentation, \
         examples, or news."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query"
                },
                "num_results": {
                    "type": "number",
                    "description": "Number of results to return (default: 5, max: 10)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: WebSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let num_results = params.num_results.clamp(1, 10);
        debug!(query = %params.query, num_results, "Web search");

        // Optional: respect PREFERRED_SEARCH_BACKEND env var to pin to a
        // specific backend. Valid values: "searxng", "firecrawl", "duckduckgo".
        let preferred = std::env::var("PREFERRED_SEARCH_BACKEND").ok();

        if let Some(ref pref) = preferred {
            match pref.as_str() {
                "searxng" => {
                    if let Ok(env_val) = std::env::var("SEARXNG_URL") {
                        let urls = parse_comma_separated(&env_val);
                        if !urls.is_empty() {
                            return search_searxng(&params.query, num_results, &urls).await;
                        }
                    }
                    return ToolResult::error(
                        "PREFERRED_SEARCH_BACKEND=searxng but SEARXNG_URL is not set.".to_string(),
                    );
                }
                "firecrawl" => {
                    let fc_keys = collect_firecrawl_keys();
                    if !fc_keys.is_empty() {
                        let fc_refs: Vec<&str> = fc_keys.iter().map(|s| s.as_str()).collect();
                        return search_firecrawl(&params.query, num_results, &fc_refs).await;
                    }
                    return ToolResult::error(
                        "PREFERRED_SEARCH_BACKEND=firecrawl but no Firecrawl keys configured."
                            .to_string(),
                    );
                }
                "duckduckgo" => {
                    return search_duckduckgo(&params.query, num_results).await;
                }
                other => {
                    debug!(
                        "Unknown PREFERRED_SEARCH_BACKEND '{}' — falling through to auto",
                        other
                    );
                }
            }
        }

        // The tool tries SearXNG first, then Firecrawl Search, then DuckDuckGo
        // as a final fallback.
        //
        // SearXNG: supports comma-separated URLs for rotation.
        if let Ok(env_val) = std::env::var("SEARXNG_URL") {
            let urls = parse_comma_separated(&env_val);
            if !urls.is_empty() {
                return search_searxng(&params.query, num_results, &urls).await;
            }
        }

        // Firecrawl: keys from env var + AuthStore, with cooldown tracking.
        let fc_keys = collect_firecrawl_keys();
        if !fc_keys.is_empty() {
            let fc_refs: Vec<&str> = fc_keys.iter().map(|s| s.as_str()).collect();
            return search_firecrawl(&params.query, num_results, &fc_refs).await;
        }

        // DuckDuckGo: final fallback.
        search_duckduckgo(&params.query, num_results).await
    }
}

/// Searches multiple SearXNG instances in order, rotating to the next on failure.
/// Accepts a list of base URLs (comma-separated in SEARXNG_URL).
async fn search_searxng(query: &str, num_results: usize, urls: &[String]) -> ToolResult {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_default();

    let mut last_error: Option<String> = None;

    for base in urls {
        let search_url = format!(
            "{}/search?q={}&format=json&safesearch=0",
            base.trim_end_matches('/'),
            urlencoding_simple(query)
        );

        let resp = match client
            .get(&search_url)
            .header("Accept", "application/json")
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                last_error = Some(format!("SearXNG '{}' failed: {}", base, e));
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            last_error = Some(format!(
                "SearXNG '{}' returned status {} (is JSON format enabled in settings.yml?)",
                base, status
            ));
            continue;
        }

        let data: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                last_error = Some(format!(
                    "Failed to parse SearXNG '{}' response: {}",
                    base, e
                ));
                continue;
            }
        };

        let mut output = String::new();
        if let Some(items) = data.get("results").and_then(|r| r.as_array()) {
            for (i, item) in items.iter().take(num_results).enumerate() {
                let title = item
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("(No title)");
                let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
                let snippet = item.get("content").and_then(|s| s.as_str()).unwrap_or("");
                output.push_str(&format!(
                    "{}. **{}**\n   URL: {}\n   {}\n\n",
                    i + 1,
                    title,
                    url,
                    snippet
                ));
            }
        }

        record_backend("searxng");
        if output.is_empty() {
            return ToolResult::success("No results found.".to_string());
        }
        return ToolResult::success(format!("[via SearXNG]\n{}", output));
    }

    let msg = last_error.unwrap_or_else(|| "All SearXNG instances failed.".to_string());
    ToolResult::error(msg)
}

/// Search using the Firecrawl Search API (v2) with key rotation and cooldown tracking.
///
/// `keys` can contain one or more API keys. The function:
/// 1. Skips any key currently in cooldown (from a previous exhaustion).
/// 2. Tries keys in order; on rate-limit (429) or auth-failure (401/403), records
///    a cooldown and tries the next key.
/// 3. On network errors, also rotates to the next key with a shorter cooldown.
/// 4. All other errors are returned immediately.
///
/// API docs: https://docs.firecrawl.dev/api-reference/endpoint/search
async fn search_firecrawl(query: &str, num_results: usize, keys: &[&str]) -> ToolResult {
    let client = reqwest::Client::new();
    let body = json!({
        "query": query,
        "limit": num_results,
    });

    let mut last_error: Option<String> = None;
    let now = Instant::now();

    for (idx, api_key) in keys.iter().enumerate() {
        // Skip keys that are still in cooldown.
        {
            let cd_map = firecrawl_cooldowns().lock().unwrap();
            if let Some(&cooldown_until) = cd_map.get(*api_key) {
                if now < cooldown_until {
                    last_error = Some(format!(
                        "Firecrawl key {} in cooldown for {:?}",
                        idx + 1,
                        cooldown_until.saturating_duration_since(now)
                    ));
                    continue;
                }
            }
        }

        let resp = match client
            .post("https://api.firecrawl.dev/v2/search")
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("Firecrawl request failed (key {}): {}", idx + 1, e);
                last_error = Some(msg.clone());
                // Mark as cooldown for network errors too.
                firecrawl_cooldowns()
                    .lock()
                    .unwrap()
                    .insert(api_key.to_string(), now + cooldown::NETWORK_ERROR);
                continue;
            }
        };

        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            let err_msg = format!(
                "Firecrawl API returned status {} (key {}): {}",
                status,
                idx + 1,
                body_text
            );
            // Rotate on rate-limit or auth-failure — likely a free-tier key that's exhausted.
            if status == 429 || status == 401 || status == 403 {
                let cooldown = if status == 429 {
                    cooldown::RATE_LIMIT
                } else {
                    cooldown::AUTH_FAILURE
                };
                firecrawl_cooldowns()
                    .lock()
                    .unwrap()
                    .insert(api_key.to_string(), now + cooldown);
                last_error = Some(err_msg);
                continue;
            }
            return ToolResult::error(err_msg);
        }

        // Success! Clear any previous cooldown for this key.
        {
            firecrawl_cooldowns().lock().unwrap().remove(*api_key);
        }
        persist_cooldowns();

        record_backend("firecrawl");

        let data: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return ToolResult::error(format!("Failed to parse Firecrawl response: {}", e))
            }
        };

        let results = format_firecrawl_results(&data, num_results);
        return ToolResult::success(format!("[via Firecrawl]\n{}", results));
    }

    // All keys exhausted — persist final state and include note in error.
    persist_cooldowns();
    let msg = last_error.unwrap_or_else(|| "All Firecrawl API keys exhausted.".to_string());
    ToolResult::error(msg)
}

fn format_firecrawl_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    let web_results = data
        .get("data")
        .and_then(|d| d.get("web"))
        .and_then(|w| w.as_array());

    if let Some(items) = web_results {
        for (i, item) in items.iter().take(max).enumerate() {
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("(No title)");
            let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
            let snippet = item
                .get("description")
                .and_then(|s| s.as_str())
                .unwrap_or("");

            output.push_str(&format!(
                "{}. **{}**\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }
    }

    if output.is_empty() {
        "No results found.".to_string()
    } else {
        output
    }
}

/// Fallback: DuckDuckGo Instant Answer API.
/// Note: this doesn't return full search results, only instant answers.
async fn search_duckduckgo(query: &str, num_results: usize) -> ToolResult {
    let client = reqwest::Client::new();
    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding_simple(query)
    );

    let resp = match client
        .get(&url)
        .header("User-Agent", "Claurst/1.0")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return ToolResult::error(format!("Search request failed: {}", e)),
    };

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        return ToolResult::error(format!("DuckDuckGo API returned status {}", status));
    }

    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return ToolResult::error(format!("Failed to parse response: {}", e)),
    };

    let output = format_ddg_results(&data, num_results);
    record_backend("duckduckgo");
    ToolResult::success(output)
}

fn format_ddg_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    let mut count = 0;

    // Abstract (main answer)
    if let Some(abstract_text) = data.get("Abstract").and_then(|a| a.as_str()) {
        if !abstract_text.is_empty() {
            let source = data
                .get("AbstractSource")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let url = data
                .get("AbstractURL")
                .and_then(|u| u.as_str())
                .unwrap_or("");
            output.push_str(&format!(
                "**{}**\n{}\nURL: {}\n\n",
                source, abstract_text, url
            ));
            count += 1;
        }
    }

    // Related topics
    if let Some(topics) = data.get("RelatedTopics").and_then(|t| t.as_array()) {
        for topic in topics.iter().take(max.saturating_sub(count)) {
            if let Some(text) = topic.get("Text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    let url = topic.get("FirstURL").and_then(|u| u.as_str()).unwrap_or("");
                    output.push_str(&format!("- {}\n  {}\n\n", text, url));
                }
            }
        }
    }

    if output.is_empty() {
        format!(
            "No instant answer found for '{}'. Try using the Firecrawl Search API \
             by setting the FIRECRAWL_API_KEY environment variable for full web search.",
            data.get("QuerySearchQuery")
                .and_then(|q| q.as_str())
                .unwrap_or("your query")
        )
    } else {
        output
    }
}

/// Minimal percent-encoding for URL query parameters.
fn urlencoding_simple(s: &str) -> String {
    let mut encoded = String::new();
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                encoded.push(ch);
            }
            ' ' => encoded.push('+'),
            _ => {
                for byte in ch.to_string().as_bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    encoded
}
