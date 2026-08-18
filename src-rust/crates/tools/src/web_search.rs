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
// (429/401/403/5xx), it is skipped for a cooldown period before being retried.

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
    /// SHA-256 fingerprint of the API key. The raw credential is never written.
    #[serde(alias = "key")]
    key_id: String,
    /// Unix timestamp (seconds since epoch) when this key can be retried.
    cooldown_until_secs: u64,
}

const FIRECRAWL_COOLDOWN_FILE: &str = "firecrawl_cooldowns.json";
const FIRECRAWL_HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const FIRECRAWL_SEARCH_URL: &str = "https://api.firecrawl.dev/v2/search";
const MAX_RESULT_FIELD_CHARS: usize = 2_000;
const MAX_SEARCH_OUTPUT_CHARS: usize = 20_000;

/// Returns the path to the Firecrawl cooldown state file.
fn cooldown_state_path() -> PathBuf {
    let dir = clawde_core::config::Settings::config_dir();
    dir.join(FIRECRAWL_COOLDOWN_FILE)
}

/// Return a stable, non-secret identifier for a Firecrawl API key.
pub fn firecrawl_key_fingerprint(key: &str) -> String {
    format!(
        "sha256:{}",
        clawde_core::crypto_utils::sha256_hex_str(key.trim())
    )
}

/// Return a short non-secret label suitable for CLI/TUI display.
pub fn firecrawl_key_label(key: &str) -> String {
    let fingerprint = firecrawl_key_fingerprint(key);
    format!("fc-{}", &fingerprint["sha256:".len()..][..10])
}

/// Global cooldown tracker for Firecrawl key fingerprints.
static FIRECRAWL_COOLDOWNS: OnceLock<std::sync::Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn firecrawl_cooldowns() -> &'static std::sync::Mutex<HashMap<String, Instant>> {
    FIRECRAWL_COOLDOWNS.get_or_init(|| {
        let m = std::sync::Mutex::new(HashMap::new());
        let mut legacy_state = false;
        if let Ok(entries) = load_cooldowns_from_disk() {
            if let Ok(mut guard) = m.lock() {
                let now_secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now = Instant::now();
                for entry in entries {
                    let key_id = if entry.key_id.starts_with("sha256:") {
                        entry.key_id
                    } else {
                        // Migrate the old plaintext-key format in memory; the
                        // next persistence pass rewrites it as a fingerprint.
                        legacy_state = true;
                        firecrawl_key_fingerprint(&entry.key_id)
                    };
                    let remaining = entry.cooldown_until_secs.saturating_sub(now_secs);
                    if remaining > 0 {
                        guard.insert(key_id, now + Duration::from_secs(remaining));
                    }
                }
                if legacy_state {
                    persist_cooldowns_snapshot(&guard);
                }
            }
        }
        m
    })
}

fn persist_cooldowns_snapshot(cooldowns: &HashMap<String, Instant>) {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let entries: Vec<FirecrawlCooldownEntry> = cooldowns
        .iter()
        .map(|(key_id, until)| FirecrawlCooldownEntry {
            key_id: key_id.clone(),
            cooldown_until_secs: now_secs
                + until.saturating_duration_since(Instant::now()).as_secs(),
        })
        .collect();

    if let Ok(json) = serde_json::to_string_pretty(&entries) {
        if let Some(parent) = cooldown_state_path().parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(cooldown_state_path(), json);
    }
}

/// Persist current cooldowns to disk.
fn persist_cooldowns() {
    let Ok(guard) = firecrawl_cooldowns().lock() else {
        return;
    };
    persist_cooldowns_snapshot(&guard);
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

fn cooldown_until(key_id: &str) -> Option<Instant> {
    firecrawl_cooldowns()
        .lock()
        .ok()
        .and_then(|guard| guard.get(key_id).copied())
}

fn set_cooldown(key_id: String, until: Instant) {
    if let Ok(mut guard) = firecrawl_cooldowns().lock() {
        guard.insert(key_id, until);
    }
}

fn clear_cooldown(key_id: &str) {
    if let Ok(mut guard) = firecrawl_cooldowns().lock() {
        guard.remove(key_id);
    }
}

/// Public API: return the health summary for Firecrawl keys.
/// Each tuple is (key_fingerprint, is_active, cooldown_remaining_secs).
/// Used by /keys health firecrawl.
pub fn firecrawl_key_health() -> Vec<(String, bool, u64)> {
    let Ok(guard) = firecrawl_cooldowns().lock() else {
        return Vec::new();
    };
    let now = Instant::now();
    let mut results: Vec<(String, bool, u64)> = guard
        .iter()
        .map(|(key_id, until)| {
            if now < *until {
                (
                    key_id.clone(),
                    false,
                    until.saturating_duration_since(now).as_secs(),
                )
            } else {
                (key_id.clone(), true, 0)
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
    pub const SERVER_ERROR: Duration = Duration::from_secs(30);
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

/// Resolve the backend override. An explicit environment value wins over the
/// persisted setting, while empty/`auto` values leave automatic fallback on.
fn preferred_backend_from_values(env_value: Option<&str>, configured: &str) -> Option<String> {
    env_value
        .filter(|value| !value.trim().is_empty())
        .or_else(|| (!configured.trim().is_empty()).then_some(configured))
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| value != "auto")
}

/// Remove terminal control characters, collapse whitespace, and cap remote text.
fn clean_result_field(value: &str, max_chars: usize) -> String {
    let cleaned: String = value
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                matches!(ch, '\n' | '\r' | '\t').then_some(' ')
            } else {
                Some(ch)
            }
        })
        .collect();
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&normalized, max_chars)
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", truncated)
    } else {
        truncated
    }
}

fn firecrawl_response_error(data: &Value) -> Option<String> {
    if data.get("success").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    let message = data
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| data.get("message").and_then(Value::as_str))
        .unwrap_or("Firecrawl returned an unsuccessful response");
    Some(clean_result_field(message, MAX_RESULT_FIELD_CHARS))
}

fn retry_after_duration(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn is_retryable_firecrawl_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429) || (500..=599).contains(&status)
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
            let trimmed = key.trim();
            if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                keys.push(trimmed.to_string());
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

    fn network_capable(&self) -> bool {
        true
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        if let Err(error) = ctx.ensure_network_allowed_for_tool(self.name(), self.network_capable())
        {
            return ToolResult::error(error.to_string());
        }
        let params: WebSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let num_results = params.num_results.clamp(1, 10);
        debug!(query = %params.query, num_results, "Web search");

        // An explicit environment override wins over the persisted setting.
        let configured_backend = clawde_core::config::Settings::load_sync()
            .ok()
            .map(|settings| settings.preferred_search_backend)
            .unwrap_or_default();
        let preferred = preferred_backend_from_values(
            std::env::var("PREFERRED_SEARCH_BACKEND").ok().as_deref(),
            &configured_backend,
        );

        if let Some(pref) = preferred.as_deref() {
            match pref {
                "searxng" => {
                    let urls = std::env::var("SEARXNG_URL")
                        .ok()
                        .map(|value| parse_comma_separated(&value))
                        .unwrap_or_default();
                    if urls.is_empty() {
                        return ToolResult::error(
                            "Preferred SearXNG backend has no SEARXNG_URL configured.".to_string(),
                        );
                    }
                    return search_searxng(&params.query, num_results, &urls).await;
                }
                "firecrawl" => {
                    let fc_keys = collect_firecrawl_keys();
                    if fc_keys.is_empty() {
                        return ToolResult::error(
                            "Preferred Firecrawl backend has no API keys configured.".to_string(),
                        );
                    }
                    let fc_refs: Vec<&str> = fc_keys.iter().map(String::as_str).collect();
                    return search_firecrawl(&params.query, num_results, &fc_refs).await;
                }
                "duckduckgo" => return search_duckduckgo(&params.query, num_results).await,
                other => {
                    return ToolResult::error(format!(
                        "Unknown preferred search backend '{}'. Valid values: auto, searxng, firecrawl, duckduckgo.",
                        other
                    ));
                }
            }
        }

        // Auto mode tries each configured backend and continues after a
        // failure. A configured but unavailable SearXNG/Firecrawl instance
        // must not disable the later fallbacks.
        let mut backend_errors = Vec::new();

        if let Ok(env_val) = std::env::var("SEARXNG_URL") {
            let urls = parse_comma_separated(&env_val);
            if !urls.is_empty() {
                let result = search_searxng(&params.query, num_results, &urls).await;
                if !result.is_error {
                    return result;
                }
                backend_errors.push(format!("SearXNG: {}", result.content));
            }
        }

        let fc_keys = collect_firecrawl_keys();
        if !fc_keys.is_empty() {
            let fc_refs: Vec<&str> = fc_keys.iter().map(String::as_str).collect();
            let result = search_firecrawl(&params.query, num_results, &fc_refs).await;
            if !result.is_error {
                return result;
            }
            backend_errors.push(format!("Firecrawl: {}", result.content));
        }

        let result = search_duckduckgo(&params.query, num_results).await;
        if result.is_error && !backend_errors.is_empty() {
            return ToolResult::error(format!(
                "All configured search backends failed.\n{}\nDuckDuckGo: {}",
                backend_errors.join("\n"),
                result.content
            ));
        }
        result
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

        let output = format_searxng_results(&data, num_results);
        record_backend("searxng");
        if output.is_empty() {
            return ToolResult::success("No results found.".to_string());
        }
        return ToolResult::success(format!("[via SearXNG]\n{}", output));
    }

    let msg = last_error.unwrap_or_else(|| "All SearXNG instances failed.".to_string());
    ToolResult::error(msg)
}

fn format_searxng_results(data: &Value, max: usize) -> String {
    let mut output = String::new();
    if let Some(items) = data.get("results").and_then(Value::as_array) {
        for (i, item) in items.iter().take(max).enumerate() {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_else(|| "(No title)".to_string());
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}. **{}**\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }
    }
    truncate_chars(&output, MAX_SEARCH_OUTPUT_CHARS)
}

/// Search using the Firecrawl Search API (v2) with key rotation and cooldown tracking.
///
/// `keys` can contain one or more API keys. The function:
/// 1. Skips any key currently in cooldown (from a previous exhaustion).
/// 2. Tries keys in order; on rate-limit (429), auth-failure (401/403), or
///    transient server errors (5xx), records a cooldown and tries the next key.
/// 3. On network errors, also rotates to the next key with a shorter cooldown.
/// 4. Non-retryable errors are returned immediately.
///
/// API docs: https://docs.firecrawl.dev/api-reference/endpoint/search
async fn search_firecrawl(query: &str, num_results: usize, keys: &[&str]) -> ToolResult {
    search_firecrawl_at(query, num_results, keys, FIRECRAWL_SEARCH_URL, true).await
}

async fn search_firecrawl_at(
    query: &str,
    num_results: usize,
    keys: &[&str],
    endpoint: &str,
    persist_state: bool,
) -> ToolResult {
    let client = reqwest::Client::builder()
        .timeout(FIRECRAWL_HTTP_TIMEOUT)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let body = json!({
        "query": query,
        "limit": num_results,
    });

    let mut last_error: Option<String> = None;

    for (idx, api_key) in keys.iter().enumerate() {
        let key_id = firecrawl_key_fingerprint(api_key);
        let now = Instant::now();

        if let Some(until) = cooldown_until(&key_id) {
            if now < until {
                last_error = Some(format!(
                    "Firecrawl key {} in cooldown for {:?}",
                    idx + 1,
                    until.saturating_duration_since(now)
                ));
                continue;
            }
        }

        let response = client
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let message = format!("Firecrawl request failed (key {}): {}", idx + 1, error);
                last_error = Some(message);
                set_cooldown(key_id, Instant::now() + cooldown::NETWORK_ERROR);
                continue;
            }
        };

        let status = response.status().as_u16();
        let retry_after = retry_after_duration(&response);
        if !response.status().is_success() {
            let body_text = response.text().await.unwrap_or_default();
            let message = format!(
                "Firecrawl API returned status {} (key {}): {}",
                status,
                idx + 1,
                clean_result_field(&body_text, MAX_RESULT_FIELD_CHARS)
            );
            if is_retryable_firecrawl_status(status) {
                let cooldown = retry_after.unwrap_or(match status {
                    401 | 403 => cooldown::AUTH_FAILURE,
                    429 => cooldown::RATE_LIMIT,
                    _ => cooldown::SERVER_ERROR,
                });
                set_cooldown(key_id, Instant::now() + cooldown);
                last_error = Some(message);
                continue;
            }
            if persist_state {
                persist_cooldowns();
            }
            return ToolResult::error(message);
        }

        clear_cooldown(&key_id);
        if persist_state {
            persist_cooldowns();
        }

        let body_text = match response.text().await {
            Ok(text) => text,
            Err(error) => {
                return ToolResult::error(format!("Failed to read Firecrawl response: {}", error));
            }
        };
        let data: Value = match serde_json::from_str(&body_text) {
            Ok(value) => value,
            Err(error) => {
                return ToolResult::error(format!("Failed to parse Firecrawl response: {}", error));
            }
        };
        if let Some(error) = firecrawl_response_error(&data) {
            return ToolResult::error(format!("Firecrawl search failed: {}", error));
        }

        record_backend("firecrawl");
        let results = format_firecrawl_results(&data, num_results);
        return ToolResult::success(format!("[via Firecrawl]\n{}", results));
    }

    if persist_state {
        persist_cooldowns();
    }
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
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_else(|| "(No title)".to_string());
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();
            let snippet = item
                .get("description")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();

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
        truncate_chars(&output, MAX_SEARCH_OUTPUT_CHARS)
    }
}

/// Fallback: DuckDuckGo Instant Answer API.
/// Note: this doesn't return full search results, only instant answers.
async fn search_duckduckgo(query: &str, num_results: usize) -> ToolResult {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default();
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
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();
            let abstract_text = clean_result_field(abstract_text, MAX_RESULT_FIELD_CHARS);
            let url = data
                .get("AbstractURL")
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_default();
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
                    let text = clean_result_field(text, MAX_RESULT_FIELD_CHARS);
                    let url = topic
                        .get("FirstURL")
                        .and_then(Value::as_str)
                        .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                        .unwrap_or_default();
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
                .and_then(Value::as_str)
                .map(|value| clean_result_field(value, MAX_RESULT_FIELD_CHARS))
                .unwrap_or_else(|| "your query".to_string())
        )
    } else {
        truncate_chars(&output, MAX_SEARCH_OUTPUT_CHARS)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    #[derive(Debug)]
    struct CapturedRequest {
        method: String,
        target: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    async fn read_mock_request(stream: &mut TcpStream) -> Result<CapturedRequest, String> {
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 4096];
        let header_end = loop {
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|error| format!("read mock request: {error}"))?;
            if read == 0 {
                return Err("mock client closed before sending headers".to_string());
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break position + 4;
            }
            if bytes.len() > 64 * 1024 {
                return Err("mock request headers exceeded 64 KiB".to_string());
            }
        };

        let header_text = std::str::from_utf8(&bytes[..header_end])
            .map_err(|error| format!("mock request headers were not UTF-8: {error}"))?;
        let mut lines = header_text.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "mock request had no request line".to_string())?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| "mock request had no method".to_string())?
            .to_string();
        let target = request_parts
            .next()
            .ok_or_else(|| "mock request had no target".to_string())?
            .to_string();
        let headers = lines
            .filter(|line| !line.is_empty())
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect::<HashMap<_, _>>();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let read = stream
                .read(&mut chunk)
                .await
                .map_err(|error| format!("read mock request body: {error}"))?;
            if read == 0 {
                return Err("mock client closed before sending the body".to_string());
            }
            bytes.extend_from_slice(&chunk[..read]);
        }

        Ok(CapturedRequest {
            method,
            target,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        })
    }

    async fn spawn_http_mock(
        responses: Vec<(u16, String)>,
    ) -> (String, JoinHandle<Result<Vec<CapturedRequest>, String>>) {
        let responses = responses
            .into_iter()
            .map(|(status, body)| (status, Vec::new(), body))
            .collect();
        spawn_http_mock_with_headers(responses).await
    }

    async fn spawn_http_mock_with_headers(
        responses: Vec<(u16, Vec<(String, String)>, String)>,
    ) -> (String, JoinHandle<Result<Vec<CapturedRequest>, String>>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind local HTTP mock");
        let address = listener.local_addr().expect("read local HTTP mock address");
        let handle = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(responses.len());
            for (status, headers, body) in responses {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .map_err(|error| format!("accept mock request: {error}"))?;
                let request = read_mock_request(&mut stream).await?;
                let reason = if status == 200 { "OK" } else { "Error" };
                let extra_headers = headers
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}\r\n"))
                    .collect::<String>();
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .map_err(|error| format!("write mock response: {error}"))?;
                requests.push(request);
            }
            Ok(requests)
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn key_fingerprint_and_label_never_expose_the_key() {
        let key = "fc-secret-key-123456789";
        let fingerprint = firecrawl_key_fingerprint(key);
        let label = firecrawl_key_label(key);

        assert!(fingerprint.starts_with("sha256:"));
        assert!(!fingerprint.contains(key));
        assert!(label.starts_with("fc-"));
        assert!(!label.contains(key));
        assert_eq!(fingerprint, firecrawl_key_fingerprint(key));
    }

    #[test]
    fn preferred_backend_uses_environment_before_config() {
        assert_eq!(
            preferred_backend_from_values(Some(" FIRECRAWL "), "duckduckgo"),
            Some("firecrawl".to_string())
        );
        assert_eq!(
            preferred_backend_from_values(None, "firecrawl"),
            Some("firecrawl".to_string())
        );
        assert_eq!(preferred_backend_from_values(None, "auto"), None);
        assert_eq!(
            parse_comma_separated(" https://one.example, ,https://two.example "),
            vec!["https://one.example", "https://two.example"]
        );
    }

    #[test]
    fn legacy_cooldown_entry_deserializes_without_retaining_plaintext_shape() {
        let entry: FirecrawlCooldownEntry = serde_json::from_value(json!({
            "key": "legacy-secret",
            "cooldown_until_secs": 123,
            "reason": "rate limited"
        }))
        .expect("legacy cooldown entry should remain readable");
        assert_eq!(entry.key_id, "legacy-secret");
        assert_eq!(
            firecrawl_key_fingerprint(&entry.key_id),
            firecrawl_key_fingerprint("legacy-secret")
        );
    }

    #[test]
    fn unsuccessful_firecrawl_envelope_is_reported() {
        let response = json!({"success": false, "error": "request timed out"});
        assert_eq!(
            firecrawl_response_error(&response).as_deref(),
            Some("request timed out")
        );
        assert!(firecrawl_response_error(&json!({"data": {"web": []}})).is_none());
    }

    #[test]
    fn firecrawl_retry_statuses_include_transient_server_errors() {
        assert!(is_retryable_firecrawl_status(401));
        assert!(is_retryable_firecrawl_status(429));
        assert!(is_retryable_firecrawl_status(503));
        assert!(!is_retryable_firecrawl_status(400));
    }

    #[test]
    fn remote_result_text_is_sanitized_and_bounded() {
        let cleaned = clean_result_field("title\u{1b}[31m\nwith\tcontrol", 30);
        assert_eq!(cleaned, "title[31m with control");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
    }

    #[test]
    fn searxng_results_are_sanitized_and_bounded_control() {
        let response = json!({
            "results": [{
                "title": "A[31m Title\\u{1b}[31m\nTitle",
                "url": "https://example.com",
                "content": "Useful\tcontent"
            }]
        });
        let output = format_searxng_results(&response, 5);
        assert!(output.contains("**A"));
        assert!(output.contains("Useful"));
        assert!(!output.contains(char::from_u32(27).unwrap()));
        assert!(output.chars().count() <= MAX_SEARCH_OUTPUT_CHARS);
    }

    #[test]
    fn ddg_results_are_sanitized_and_bounded_control() {
        let response = json!({
            "Abstract": "Answer with control",
            "AbstractSource": "Source",
            "AbstractURL": "https://example.com",
            "RelatedTopics": []
        });
        let output = format_ddg_results(&response, 5);
        assert!(output.contains("Answer"));
        assert!(!output.contains(char::from_u32(27).unwrap()));
        assert!(output.chars().count() <= MAX_SEARCH_OUTPUT_CHARS);
    }

    #[test]
    fn searxng_results_strip_terminal_controls() {
        let response = json!({
            "results": [{
                "title": format!("A{}[31m\nTitle", char::from_u32(27).unwrap()),
                "url": "https://example.com",
                "content": "Useful\tcontent"
            }]
        });
        let output = format_searxng_results(&response, 5);
        assert!(output.contains("**A[31m Title"));
        assert!(output.contains("Useful content"));
        assert!(!output.contains(char::from_u32(27).unwrap()));
        assert!(output.chars().count() <= MAX_SEARCH_OUTPUT_CHARS);
    }

    #[tokio::test]
    async fn searxng_request_and_response_use_local_http_mock() {
        let (base_url, server) = spawn_http_mock(vec![(
            200,
            json!({
                "results": [{
                    "title": "Rust HTTP",
                    "url": "https://example.com/rust",
                    "content": "A local fixture result"
                }]
            })
            .to_string(),
        )])
        .await;
        let result = search_searxng("rust async", 2, std::slice::from_ref(&base_url)).await;
        assert!(
            !result.is_error,
            "unexpected SearXNG error: {}",
            result.content
        );
        assert!(result.content.contains("[via SearXNG]"));
        assert!(result.content.contains("Rust HTTP"));

        let requests = server
            .await
            .expect("SearXNG mock task should join")
            .expect("SearXNG mock should capture request");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].target,
            "/search?q=rust+async&format=json&safesearch=0"
        );
        assert_eq!(
            requests[0].headers.get("accept").map(String::as_str),
            Some("application/json")
        );
        assert!(requests[0].body.is_empty());
    }

    #[tokio::test]
    async fn searxng_rotates_to_the_next_configured_instance() {
        let (base_url, server) = spawn_http_mock(vec![
            (500, "temporary failure".to_string()),
            (
                200,
                json!({
                    "results": [{
                        "title": "Recovered SearXNG",
                        "url": "https://example.com/recovered",
                        "content": "Second instance succeeded"
                    }]
                })
                .to_string(),
            ),
        ])
        .await;
        let urls = vec![base_url.clone(), base_url];
        let result = search_searxng("rotate", 1, &urls).await;
        assert!(
            !result.is_error,
            "unexpected SearXNG error: {}",
            result.content
        );
        assert!(result.content.contains("Recovered SearXNG"));

        let requests = server
            .await
            .expect("SearXNG rotation mock task should join")
            .expect("SearXNG rotation mock should capture requests");
        assert_eq!(requests.len(), 2);
        assert!(requests[0].target.starts_with("/search?q=rotate"));
        assert!(requests[1].target.starts_with("/search?q=rotate"));
    }

    #[tokio::test]
    async fn searxng_malformed_http_response_is_reported() {
        let (base_url, server) = spawn_http_mock(vec![(200, "not-json".to_string())]).await;
        let result = search_searxng("malformed", 1, &[base_url]).await;
        assert!(result.is_error);
        assert!(result.content.contains("Failed to parse SearXNG"));
        server
            .await
            .expect("SearXNG malformed mock task should join")
            .expect("SearXNG malformed mock should capture request");
    }

    #[tokio::test]
    async fn firecrawl_request_and_response_use_local_http_mock() {
        let (base_url, server) = spawn_http_mock(vec![(
            200,
            json!({
                "success": true,
                "data": {
                    "web": [{
                        "title": "Firecrawl HTTP",
                        "url": "https://example.com/firecrawl",
                        "description": "A local Firecrawl fixture"
                    }]
                }
            })
            .to_string(),
        )])
        .await;
        let endpoint = format!("{base_url}/v2/search");
        let key = "local-firecrawl-key-success-123456";
        let result = search_firecrawl_at("rust async", 3, &[key], &endpoint, false).await;
        assert!(
            !result.is_error,
            "unexpected Firecrawl error: {}",
            result.content
        );
        assert!(result.content.contains("[via Firecrawl]"));
        assert!(result.content.contains("Firecrawl HTTP"));

        let requests = server
            .await
            .expect("Firecrawl mock task should join")
            .expect("Firecrawl mock should capture request");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/v2/search");
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer local-firecrawl-key-success-123456")
        );
        let body: Value = serde_json::from_slice(&requests[0].body).expect("valid JSON body");
        assert_eq!(
            body.get("query").and_then(Value::as_str),
            Some("rust async")
        );
        assert_eq!(body.get("limit").and_then(Value::as_u64), Some(3));
    }

    #[tokio::test]
    async fn firecrawl_http_5xx_rotates_to_the_next_key() {
        let (base_url, server) = spawn_http_mock(vec![
            (503, "temporary failure".to_string()),
            (
                200,
                json!({
                    "success": true,
                    "data": {
                        "web": [{
                            "title": "Recovered",
                            "url": "https://example.com/recovered",
                            "description": "Second key succeeded"
                        }]
                    }
                })
                .to_string(),
            ),
        ])
        .await;
        let endpoint = format!("{base_url}/v2/search");
        let first_key = "local-firecrawl-key-first-123456";
        let second_key = "local-firecrawl-key-second-123456";
        let result =
            search_firecrawl_at("retry me", 1, &[first_key, second_key], &endpoint, false).await;
        assert!(
            !result.is_error,
            "unexpected Firecrawl error: {}",
            result.content
        );
        assert!(result.content.contains("Recovered"));

        let requests = server
            .await
            .expect("Firecrawl retry mock task should join")
            .expect("Firecrawl retry mock should capture requests");
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers.get("authorization").map(String::as_str),
            Some("Bearer local-firecrawl-key-first-123456")
        );
        assert_eq!(
            requests[1].headers.get("authorization").map(String::as_str),
            Some("Bearer local-firecrawl-key-second-123456")
        );
    }

    #[tokio::test]
    async fn firecrawl_retry_after_header_controls_cooldown() {
        let (base_url, server) = spawn_http_mock_with_headers(vec![(
            429,
            vec![("Retry-After".to_string(), "60".to_string())],
            "rate limited".to_string(),
        )])
        .await;
        let endpoint = format!("{base_url}/v2/search");
        let key = "local-firecrawl-key-retry-after-123456";
        let started = Instant::now();
        let result = search_firecrawl_at("retry-after", 1, &[key], &endpoint, false).await;
        assert!(result.is_error);
        let cooldown =
            cooldown_until(&firecrawl_key_fingerprint(key)).expect("429 should record a cooldown");
        assert!(cooldown.saturating_duration_since(started) >= Duration::from_secs(59));
        clear_cooldown(&firecrawl_key_fingerprint(key));

        let requests = server
            .await
            .expect("Retry-After mock task should join")
            .expect("Retry-After mock should capture request");
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn firecrawl_malformed_http_response_is_reported() {
        let (base_url, server) = spawn_http_mock(vec![(200, "not-json".to_string())]).await;
        let endpoint = format!("{base_url}/v2/search");
        let result = search_firecrawl_at(
            "malformed",
            1,
            &["local-firecrawl-key-malformed-123456"],
            &endpoint,
            false,
        )
        .await;
        assert!(result.is_error);
        assert!(result
            .content
            .contains("Failed to parse Firecrawl response"));
        server
            .await
            .expect("Firecrawl malformed mock task should join")
            .expect("Firecrawl malformed mock should capture request");
    }

    #[test]
    fn firecrawl_results_match_v2_response_and_are_bounded() {
        let response = json!({
            "success": true,
            "data": {
                "web": [{
                    "title": "A\nTitle",
                    "url": "https://example.com",
                    "description": "A useful result"
                }]
            }
        });
        let output = format_firecrawl_results(&response, 5);
        assert!(output.contains("**A Title**"));
        assert!(output.contains("https://example.com"));
        assert!(output.contains("A useful result"));
        assert!(output.chars().count() <= MAX_SEARCH_OUTPUT_CHARS);
    }
}
