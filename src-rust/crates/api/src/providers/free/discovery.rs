// providers/free/discovery.rs — Live free-model discovery.
//
// Per-upstream endpoints that report the currently-free model list (Cline's
// recommended-models API, OpenRouter's models API, OpenAI-compatible
// /v1/models, Gemini's models API). Results are cached per upstream so
// runtime rebuilds of the free chain never re-run blocking network calls.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{fetch_best_free_models_from_modelsdev, resolve_free_upstream_keys};
use crate::providers::openai_compat_providers::{cloudflare_parts, CLINE_SDK_CLIENT_TYPE};

// ---------------------------------------------------------------------------
// Live free-model discovery (per-provider API endpoints)
// ---------------------------------------------------------------------------

/// Describes how to discover the current best free model for an upstream
/// at provider-runtime build time. Each variant encapsulates the provider-
/// specific API endpoint, auth mechanism, and response parsing needed.
///
/// To add a new provider with live discovery:
///   1. Add a variant to this enum
///   2. Wire it in [`discovery_for`]
///   3. Add the fetch function that implements the variant's logic
///   4. Wire the variant in [`run_live_discovery`]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FreeModelDiscovery {
    /// No live discovery — use the hardcoded `default_model` (or
    /// models.dev auto-detection which runs separately).
    None,
    /// Fetch from Cline's recommended-models API.
    ClineRecommended,
    /// Fetch from OpenRouter's models API — finds free (pricing=0)
    /// models that support tool calling, picks the one with the
    /// largest context window.
    OpenRouterFreeModels,
    /// Fetch from a standard OpenAI-compatible `/v1/models` endpoint.
    /// Returns the first available model ID that matches models.dev's
    /// auto-detected free model for this upstream (to verify it's
    /// actually live), or the first model from the endpoint if no
    /// match is found.
    OpenAiModelList {
        /// The base URL of the OpenAI-compatible API.
        base_url: &'static str,
    },
    /// Fetch OpenCode Zen's public model list and select a model whose ID
    /// explicitly ends in `-free`. The endpoint contains paid and free models;
    /// the suffix is the authoritative free-tier marker.
    OpenCodeZenFreeModels {
        /// The base URL of the OpenAI-compatible API, e.g.
        /// `"https://api.groq.com/openai/v1"`.
        base_url: &'static str,
    },
    /// Fetch from Google Gemini's `/v1beta/models` endpoint.
    /// Uses query-parameter auth (`?key=`). Response has a `models`
    /// array with `name` fields like `"models/gemini-2.5-flash"`.
    /// Strips the `models/` prefix to get the model ID.
    GeminiModels,
    /// Fetch from Cloudflare's account-scoped models API
    /// (`/accounts/{ACCOUNT_ID}/ai/models/search`). Cloudflare's
    /// OpenAI-compatible `/models` route does not support GET (405), and the
    /// account-scoped endpoint additionally reflects which models this
    /// account can actually serve — picking up new models without waiting
    /// for a models.dev refresh.
    CloudflareModels,
    /// Fetch from Cohere's `/v1/models` endpoint. Cohere's response uses a
    /// top-level `models` array with the model ID in `name` (not OpenAI's
    /// `data` shape).
    CohereModels,
}

/// Map each FREE_CATALOG upstream to its live discovery method.
pub fn discovery_for(upstream_id: &str) -> FreeModelDiscovery {
    match upstream_id {
        "cline" => FreeModelDiscovery::ClineRecommended,
        "openrouter" => FreeModelDiscovery::OpenRouterFreeModels,
        "huggingface" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://router.huggingface.co/v1",
        },
        "cerebras" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.cerebras.ai/v1",
        },
        "nvidia" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://integrate.api.nvidia.com/v1",
        },
        "groq" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.groq.com/openai/v1",
        },
        "google" => FreeModelDiscovery::GeminiModels,
        "opencode-zen" => FreeModelDiscovery::OpenCodeZenFreeModels {
            base_url: "https://opencode.ai/zen/v1",
        },
        "mistral" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.mistral.ai/v1",
        },
        "sambanova" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.sambanova.ai/v1",
        },
        "zai" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.z.ai/api/coding/paas/v4",
        },
        // cloudflare: /ai/v1/models does not support GET (405), so probe the
        // account-scoped models/search API instead — it reflects per-account
        // availability and sees new models without a models.dev refresh.
        "cloudflare" => FreeModelDiscovery::CloudflareModels,
        "cohere" => FreeModelDiscovery::CohereModels,
        // github-copilot intentionally stays on the catch-all None here: the
        // CopilotProvider fetches its own /models list internally (with a
        // hardcoded fallback), so it needs no separate discovery probe.
        _ => FreeModelDiscovery::None,
    }
}

/// Per-upstream cache for live free-model discovery results. Populated on
/// the first build and reused by runtime rebuilds so `refresh_free_provider`
/// (triggered by /keys, /logout, /refresh, the free-mode dialog, and the
/// /ollama toggle) never blocks the UI thread on repeated network fetches.
/// Free-model lists are slow-moving within a session, mirroring the
/// `AUTO_DETECTED_DEFAULTS` models.dev cache.
static LIVE_DISCOVERY_CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
fn live_discovery_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    LIVE_DISCOVERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run live discovery for the first entry whose ID matches `upstream_id`.
/// Returns the discovered model ID, or `None` if discovery is not configured
/// or the fetch fails.
///
/// Successful results are cached per upstream id after the first fetch, so
/// runtime rebuilds of the free chain don't re-run blocking network calls on
/// the UI thread. Failed results are deliberately retried on later calls.
pub fn run_live_discovery(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<String> {
    // Fast path: previously successful result.
    if let Some(cached) = live_discovery_cache()
        .lock()
        .ok()
        .and_then(|guard| guard.get(upstream_id).cloned())
    {
        return cached;
    }
    // F2 (audit fix): disk cache — a fresh CLI process should not re-probe
    // every configured upstream at startup. Successful discoveries persist for
    // DISCOVERY_CACHE_TTL_SECS; failures are left uncached so a recovering
    // upstream is re-probed promptly on the next process.
    if let Some(cached) = super::load_live_discovery_cache(upstream_id) {
        if let Ok(mut guard) = live_discovery_cache().lock() {
            guard.insert(upstream_id.to_string(), Some(cached.clone()));
        }
        return Some(cached);
    }
    let result = run_live_discovery_uncached(upstream_id, auth_store);
    // Cache only successful discovery. A transient outage, missing key, or
    // pre-authentication probe must not permanently mask a later recovery.
    if let Some(model) = result.as_ref() {
        super::save_live_discovery_cache(upstream_id, Some(model.clone()));
        if let Ok(mut guard) = live_discovery_cache().lock() {
            guard.insert(upstream_id.to_string(), Some(model.clone()));
        }
    }
    result
}

/// The uncached discovery fetch — see [`run_live_discovery`].
fn run_live_discovery_uncached(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<String> {
    match discovery_for(upstream_id) {
        FreeModelDiscovery::ClineRecommended => {
            let key = first_upstream_key(auth_store, "cline")?;
            fetch_cline_free_model(&key)
        }
        FreeModelDiscovery::OpenRouterFreeModels => {
            let key = first_upstream_key(auth_store, "openrouter")?;
            fetch_openrouter_free_model(&key)
        }
        FreeModelDiscovery::OpenAiModelList { base_url } => {
            let key = first_upstream_key(auth_store, upstream_id)?;
            fetch_openai_compat_model_list(&key, base_url, upstream_id)
        }
        FreeModelDiscovery::OpenCodeZenFreeModels { base_url } => {
            // `/models` is public and must remain discoverable even when the
            // stored Zen credential is stale or currently creditless.
            fetch_opencode_zen_free_model(base_url)
        }
        FreeModelDiscovery::GeminiModels => {
            let key = first_upstream_key(auth_store, "google")?;
            fetch_gemini_models(&key)
        }
        FreeModelDiscovery::CloudflareModels => {
            let key = first_upstream_key(auth_store, "cloudflare")?;
            fetch_cloudflare_available_model(&key)
        }
        FreeModelDiscovery::CohereModels => {
            let key = first_upstream_key(auth_store, "cohere")?;
            fetch_cohere_model_list(&key)
        }
        FreeModelDiscovery::None => None,
    }
}

/// First configured key for a free-catalog upstream: rotation keys first,
/// then the single-key / OAuth credential as fallback (matching the original
/// per-upstream discovery lookup order).
fn first_upstream_key(auth_store: &clawde_core::AuthStore, upstream_id: &str) -> Option<String> {
    resolve_free_upstream_keys(auth_store, upstream_id)
        .and_then(|k| k.first().cloned())
        .or_else(|| auth_store.api_key_for(upstream_id))
}

/// Perform a blocking GET inside a plain OS thread and parse the response as
/// JSON.
///
/// `reqwest::blocking::Client` creates an internal tokio runtime; dropping it
/// inside an existing tokio runtime context panics, so the entire HTTP call is
/// moved to a plain OS thread.
///
/// `auth_bearer` is sent as the `Authorization: Bearer <key>` header; pass
/// `None` for query-parameter auth (e.g. Gemini's `?key=`).
///
/// Returns `None` on transport failure, non-2xx status, or unparseable JSON,
/// logging a warning labelled with `context`.
fn request_headers(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect()
}

fn blocking_get_json(
    url: String,
    auth_bearer: Option<String>,
    headers: &[(&str, &str)],
    context: String,
) -> Option<serde_json::Value> {
    let headers = request_headers(headers);
    std::thread::spawn(move || {
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| {
                let mut request = client.get(&url);
                if let Some(key) = auth_bearer {
                    request = request.header("Authorization", format!("Bearer {}", key));
                }
                for (name, value) in &headers {
                    request = request.header(name.as_str(), value.as_str());
                }
                request.send()
            })
        else {
            tracing::warn!("{}: HTTP request failed", context);
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!("{}: HTTP {} — check API key", context, response.status());
            return None;
        }

        match response.json::<serde_json::Value>() {
            Ok(data) => Some(data),
            Err(_) => {
                tracing::warn!("{}: failed to parse JSON", context);
                None
            }
        }
    })
    .join()
    .ok()
    .flatten()
}

/// Fetch Cline's current free models from their recommended-models API.
///
/// Cline's API at `https://api.cline.bot/api/v1/ai/cline/recommended-models`
/// returns a `{ "free": [...] }` array of currently available free models.
/// This list rotates as Cline updates their free tier.
///
/// Returns the first free model ID, or `None` if the API is unreachable,
/// the key is invalid, or no free models are currently offered.
pub fn fetch_cline_free_model(cline_api_key: &str) -> Option<String> {
    let data = blocking_get_json(
        "https://api.cline.bot/api/v1/ai/cline/recommended-models".to_string(),
        Some(cline_api_key.to_string()),
        &[("X-CLIENT-TYPE", CLINE_SDK_CLIENT_TYPE)],
        "fetch_cline_free_model".to_string(),
    )?;

    let free_models = data.get("free").and_then(|v| v.as_array())?;
    let first = free_models.first()?;
    let model_id = first.get("id")?.as_str()?;

    tracing::info!(
        "Cline recommended free model: {} (from {} available)",
        model_id,
        free_models.len(),
    );

    Some(model_id.to_string())
}

/// Fetch ALL current free models from Cline's recommended-models API.
///
/// Same endpoint as [`fetch_cline_free_model`] but returns every model ID
/// instead of just the first. The model IDs are in `provider/model` format
/// (e.g. `deepseek/deepseek-v4-flash`).
///
/// Returns the full list of free model IDs, or `None` if the API is
/// unreachable, the key is invalid, or the response is unparseable.
pub fn fetch_cline_free_models(cline_api_key: &str) -> Option<Vec<String>> {
    let data = blocking_get_json(
        "https://api.cline.bot/api/v1/ai/cline/recommended-models".to_string(),
        Some(cline_api_key.to_string()),
        &[("X-CLIENT-TYPE", CLINE_SDK_CLIENT_TYPE)],
        "fetch_cline_free_models".to_string(),
    )?;

    let free_models = data.get("free").and_then(|v| v.as_array())?;
    let ids: Vec<String> = free_models
        .iter()
        .filter_map(|m| m.get("id")?.as_str().map(String::from))
        .collect();

    if ids.is_empty() {
        tracing::warn!("fetch_cline_free_models: no free models in response");
        return None;
    }

    tracing::info!(
        "Cline free models: {} (first: {})",
        ids.len(),
        ids.first().unwrap_or(&String::new()),
    );

    Some(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cline_discovery_headers_include_sdk_client_type() {
        assert_eq!(
            request_headers(&[("X-CLIENT-TYPE", CLINE_SDK_CLIENT_TYPE)]),
            vec![(
                "X-CLIENT-TYPE".to_string(),
                CLINE_SDK_CLIENT_TYPE.to_string()
            )]
        );
    }

    #[test]
    fn opencode_discovery_ignores_paid_models() {
        let payload = serde_json::json!({
            "data": [
                {"id": "minimax-m2.5"},
                {"id": "deepseek-v4-flash-free"},
                {"id": "big-pickle"},
                {"id": "mimo-v2.5-free"}
            ]
        });
        assert_eq!(
            select_opencode_zen_free_models(&payload),
            Some(vec![
                "deepseek-v4-flash-free".to_string(),
                "mimo-v2.5-free".to_string()
            ])
        );
        assert_eq!(
            select_opencode_zen_free_models(&payload)
                .and_then(|models| models.into_iter().next())
                .as_deref(),
            Some("deepseek-v4-flash-free")
        );
    }

    #[test]
    fn opencode_discovery_returns_none_without_free_models() {
        let payload = serde_json::json!({
            "data": [{"id": "minimax-m2.5"}, {"id": "big-pickle"}]
        });
        assert_eq!(select_opencode_zen_free_models(&payload), None);
    }

    #[test]
    fn cloudflare_discovery_extracts_cf_models_and_prefers_text_generation() {
        let payload = serde_json::json!({
            "success": true,
            "result": [
                {"name": "@cf/baai/bge-m3", "task": {"name": "Text Embeddings"}},
                {"name": "@cf/qwen/qwen3-30b-a3b-fp8", "task": {"name": "Text Generation"}},
                {"name": "@cf/openai/gpt-oss-120b", "task": {"name": "Text Generation"}},
                {"name": "not-a-cf-model", "task": {"name": "Text Generation"}}
            ]
        });
        assert_eq!(
            collect_cloudflare_available_models(&payload),
            vec![
                "@cf/qwen/qwen3-30b-a3b-fp8",
                "@cf/openai/gpt-oss-120b",
                "@cf/baai/bge-m3",
            ]
        );
    }

    #[test]
    fn cloudflare_discovery_returns_empty_for_invalid_payload() {
        assert!(collect_cloudflare_available_models(&serde_json::json!({})).is_empty());
        assert!(
            collect_cloudflare_available_models(&serde_json::json!({ "result": "nope" }))
                .is_empty()
        );
    }

    #[test]
    fn cohere_discovery_extracts_model_names() {
        let payload = serde_json::json!({
            "models": [
                {"name": "north-mini-code-1-0"},
                {"name": "command-r-plus", "context_length": 128000}
            ]
        });
        assert_eq!(
            collect_cohere_model_ids(&payload),
            vec!["north-mini-code-1-0", "command-r-plus"]
        );
        assert!(collect_cohere_model_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn select_available_model_prefers_modelsdev_pick() {
        let available: Vec<&str> = vec!["m2", "@cf/qwen/qwen3-30b-a3b-fp8", "m1"];
        let auto = HashMap::from([(
            "cloudflare".to_string(),
            "@cf/qwen/qwen3-30b-a3b-fp8".to_string(),
        )]);
        assert_eq!(
            select_available_model("cloudflare", &available, &auto).as_deref(),
            Some("@cf/qwen/qwen3-30b-a3b-fp8")
        );
    }

    #[test]
    fn select_available_model_prefers_catalog_default_when_modelsdev_pick_absent() {
        let available: Vec<&str> = vec!["m2", "@cf/qwen/qwen3-30b-a3b-fp8", "m1"];
        let auto = HashMap::from([("cloudflare".to_string(), "some-other-model".to_string())]);
        // models.dev pick is not on the live list → catalog default wins.
        assert_eq!(
            select_available_model("cloudflare", &available, &auto).as_deref(),
            Some("@cf/qwen/qwen3-30b-a3b-fp8")
        );
    }

    #[test]
    fn select_available_model_falls_back_to_first() {
        let available: Vec<&str> = vec!["m1", "m2"];
        assert_eq!(
            select_available_model("cloudflare", &available, &HashMap::new()).as_deref(),
            Some("m1")
        );
        assert_eq!(
            select_available_model("cloudflare", &[], &HashMap::new()),
            None
        );
    }
}

/// Fetch OpenRouter's current free models from their models API.
///
/// OpenRouter's API at `https://openrouter.ai/api/v1/models` returns
/// a `{ "data": [...] }` array of all models with pricing. Free models
/// have `pricing.prompt`, `pricing.completion`, and `pricing.request`
/// all set to `"0"`.
///
/// Selection criteria:
/// 1. Model must be free (all pricing fields = "0")
/// 2. Model must not be archived
/// 3. Model must support tool calling (`supported_parameters` includes "tools")
/// 4. Among qualifying models, pick the one with the largest `context_length`
///
/// Returns the best free model ID, or `None` if the API is unreachable,
/// the key is invalid, or no qualifying free models are found.
pub fn fetch_openrouter_free_model(openrouter_api_key: &str) -> Option<String> {
    let payload = blocking_get_json(
        "https://openrouter.ai/api/v1/models".to_string(),
        Some(openrouter_api_key.to_string()),
        &[],
        "fetch_openrouter_free_model".to_string(),
    )?;

    let models = payload.get("data").and_then(|v| v.as_array())?;

    // Collect free (all pricing=0), non-archived, tool-supporting models.
    let mut candidates: Vec<(&str, u64)> = Vec::new();

    for model in models {
        let model_id = match model.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => continue,
        };

        // Skip archived models
        if model
            .get("archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }

        // Check pricing: all must be "0"
        let pricing = match model.get("pricing").and_then(|v| v.as_object()) {
            Some(p) => p,
            None => continue,
        };
        let prompt_cost = pricing.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let completion_cost = pricing
            .get("completion")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let request_cost = pricing
            .get("request")
            .and_then(|v| v.as_str())
            .unwrap_or("0");
        if prompt_cost != "0" || completion_cost != "0" || request_cost != "0" {
            continue;
        }

        // Check tool calling support
        let supports_tools = model
            .get("supported_parameters")
            .and_then(|v| v.as_array())
            .map(|params| params.iter().any(|p| p.as_str() == Some("tools")))
            .unwrap_or(false);
        if !supports_tools {
            continue;
        }

        // Context window for ranking
        let ctx = model
            .get("context_length")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        candidates.push((model_id, ctx));
    }

    // Sort by context window descending, pick the best
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    if let Some((model_id, ctx)) = candidates.first() {
        tracing::info!(
            "OpenRouter recommended free model: {} ({} context, from {} candidates)",
            model_id,
            ctx,
            candidates.len(),
        );
        Some((*model_id).to_string())
    } else {
        tracing::warn!("fetch_openrouter_free_model: no free tool-capable models found");
        None
    }
}

/// Fetch OpenCode Zen's current free model list.
///
/// Zen exposes paid and free models from the same public `/models` endpoint.
/// Only IDs ending in `-free` are eligible; do not fall back to the first
/// arbitrary model because that could turn Free mode into a paid request.
pub fn fetch_opencode_zen_free_model(base_url: &str) -> Option<String> {
    fetch_opencode_zen_free_models(base_url)?.into_iter().next()
}

/// Fetch all current OpenCode Zen free model IDs.
///
/// The endpoint contains both paid and free models. Only IDs ending in
/// `-free` are returned, so this remains safe as the public catalog changes.
pub fn fetch_opencode_zen_free_models(base_url: &str) -> Option<Vec<String>> {
    let models_url = format!("{}/models", base_url.trim_end_matches('/'));
    let payload = blocking_get_json(
        models_url,
        None,
        &[],
        "fetch_opencode_zen_free_models".to_string(),
    )?;
    select_opencode_zen_free_models(&payload)
}

/// Select all explicitly free models from a Zen `/models` payload.
/// Kept separate from HTTP so the paid-model exclusion is regression-tested
/// without a network dependency.
fn select_opencode_zen_free_models(payload: &serde_json::Value) -> Option<Vec<String>> {
    let models = payload.get("data").and_then(|v| v.as_array())?;
    let free_models: Vec<String> = models
        .iter()
        .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
        .filter(|id| id.ends_with("-free"))
        .map(str::to_owned)
        .collect();
    if free_models.is_empty() {
        return None;
    }
    tracing::info!(
        "OpenCode Zen free models: {} (first: {})",
        free_models.len(),
        free_models[0],
    );
    Some(free_models)
}

/// Fetch model list from a standard OpenAI-compatible `/v1/models` endpoint.
///
/// Cross-references the list with models.dev auto-detected free models
/// (from [`fetch_best_free_models_from_modelsdev`]) to find the best known
/// free model that's actually available on this provider. If no match is
/// found, returns the first available model ID as a fallback.
pub fn fetch_openai_compat_model_list(
    api_key: &str,
    base_url: &str,
    upstream_id: &str,
) -> Option<String> {
    let models_url = format!("{}/models", base_url.trim_end_matches('/'));
    let payload = blocking_get_json(
        models_url,
        Some(api_key.to_string()),
        &[],
        format!("fetch_openai_compat_model_list({})", upstream_id),
    )?;

    // Collect available model IDs from the response.
    let available: Vec<&str> = payload
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .collect()
        })
        .unwrap_or_default();

    if available.is_empty() {
        tracing::warn!(
            "fetch_openai_compat_model_list({}): no models in response",
            upstream_id
        );
        return None;
    }

    select_available_model(
        upstream_id,
        &available,
        fetch_best_free_models_from_modelsdev(),
    )
}

/// Shared selection rule for live model lists: prefer the models.dev
/// auto-detected pick when the live list confirms it is actually available,
/// then the catalog's `default_model`, then the first available ID.
///
/// Kept pure (no network) so every discovery path — OpenAI-compat, Cohere,
/// Cloudflare — uses one unit-testable precedence.
fn select_available_model(
    upstream_id: &str,
    available: &[&str],
    auto_detected: &HashMap<String, String>,
) -> Option<String> {
    // Prefer the models.dev-recommended free model when the live list
    // confirms it is actually available.
    if let Some(recommended) = auto_detected.get(upstream_id) {
        if available.contains(&recommended.as_str()) {
            tracing::info!(
                "{} live model list confirmed models.dev pick: {}",
                upstream_id,
                recommended,
            );
            return Some(recommended.clone());
        }
    }
    // Fallback: prefer the catalog's default_model when it's available.
    if let Some(entry) = crate::providers::free::catalog_entry(upstream_id) {
        if available.contains(&entry.default_model) {
            tracing::info!(
                "{} live model list: catalog default {} is available",
                upstream_id,
                entry.default_model,
            );
            return Some(entry.default_model.to_string());
        }
    }
    // Last resort: return the first available model.
    let first = available.first()?.to_string();
    tracing::info!(
        "{} live model list returned first model: {} ({} available)",
        upstream_id,
        first,
        available.len(),
    );
    Some(first)
}

/// Fetch the best Cloudflare Workers AI model available to this account.
///
/// Cloudflare's OpenAI-compatible `/ai/v1/models` route does not support GET
/// (405), so this probes the account-scoped catalog instead:
/// `GET /accounts/{ACCOUNT_ID}/ai/models/search` authenticated with the
/// composite `ACCOUNT_ID:API_TOKEN` credential. The response's `result[].name`
/// carries the request-time model IDs (`@cf/qwen/qwen3-30b-a3b-fp8`). Because
/// the endpoint is account-scoped, it also catches models that models.dev
/// marks free but this account cannot actually serve.
///
/// Returns `None` (→ catalog default) when the account ID is unknown, the
/// request fails, or the account exposes no `@cf/` models.
pub fn fetch_cloudflare_available_model(cloudflare_key: &str) -> Option<String> {
    // Composite `ACCOUNT_ID:API_TOKEN` key; fall back to the dedicated env
    // vars when the key has no separator (mirrors `cloudflare_with_key`).
    let (account, token) = match cloudflare_parts(cloudflare_key) {
        Some((a, t)) => (a.to_string(), t.to_string()),
        None => {
            let account = std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default();
            if account.is_empty() {
                // Unlike cloudflare_with_key (which builds a provider with an
                // empty account and fails only at request time), fail fast
                // here: discovery is optional and the catalog default applies.
                tracing::warn!(
                    "fetch_cloudflare_available_model: key is not ACCOUNT_ID:API_TOKEN and \
                     CLOUDFLARE_ACCOUNT_ID is unset"
                );
                return None;
            }
            (account, cloudflare_key.to_string())
        }
    };

    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/models/search?per_page=100",
        account
    );
    let payload = blocking_get_json(
        url,
        Some(token),
        &[],
        "fetch_cloudflare_available_model".to_string(),
    )?;
    let available = collect_cloudflare_available_models(&payload);
    if available.is_empty() {
        tracing::warn!("fetch_cloudflare_available_model: no @cf/ models in response");
        return None;
    }
    select_available_model(
        "cloudflare",
        &available,
        fetch_best_free_models_from_modelsdev(),
    )
}

/// Extract available Cloudflare model IDs from an `/ai/models/search` payload.
///
/// The search response lists models as objects whose `name` carries the
/// request-time model ID (`@cf/...`) and whose `task.name` distinguishes
/// text-generation LLMs from embeddings/classifiers. Text-generation models
/// are returned first so the first-available fallback prefers an LLM.
fn collect_cloudflare_available_models(payload: &serde_json::Value) -> Vec<&str> {
    let Some(models) = payload.get("result").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let is_text_generation = |model: &serde_json::Value| {
        model
            .get("task")
            .and_then(|t| t.get("name"))
            .and_then(|n| n.as_str())
            .map(|name| name.eq_ignore_ascii_case("Text Generation"))
            .unwrap_or(false)
    };
    let mut text_generation: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for model in models {
        let Some(id) = model.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !id.starts_with("@cf/") {
            continue;
        }
        if is_text_generation(model) {
            text_generation.push(id);
        } else {
            other.push(id);
        }
    }
    text_generation.extend(other);
    text_generation
}

/// Fetch Cohere's current model list from their models API.
///
/// Cohere's API at `https://api.cohere.com/v1/models` returns a top-level
/// `models` array (not OpenAI's `data` shape), with each entry's model ID in
/// `name`. Selection mirrors the OpenAI-compat path: models.dev pick →
/// catalog default → first available.
pub fn fetch_cohere_model_list(api_key: &str) -> Option<String> {
    let payload = blocking_get_json(
        "https://api.cohere.com/v1/models".to_string(),
        Some(api_key.to_string()),
        &[],
        "fetch_cohere_model_list".to_string(),
    )?;
    let mut available = collect_cohere_model_ids(&payload);
    if available.is_empty() {
        tracing::warn!("fetch_cohere_model_list: no models in response");
        return None;
    }
    // Cohere's list mixes free and paid models in one response. Keep catalog-
    // family models (e.g. `north-mini-code`) first so the first-available
    // fallback prefers the free coding family when the models.dev pick and
    // catalog default are both absent.
    if let Some(family) = crate::providers::free::catalog_entry("cohere").map(|e| e.model_family) {
        available.sort_by_key(|id| !id.starts_with(family));
    }
    select_available_model(
        "cohere",
        &available,
        fetch_best_free_models_from_modelsdev(),
    )
}

/// Extract model IDs from a Cohere `/v1/models` payload (`models[].name`).
fn collect_cohere_model_ids(payload: &serde_json::Value) -> Vec<&str> {
    payload
        .get("models")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// Fetch Google Gemini's current available models from their models API.
///
/// Gemini's API at `https://generativelanguage.googleapis.com/v1beta/models`
/// uses query-parameter auth (`?key=`). Response has a `models` array with
/// `name` fields like `"models/gemini-2.5-flash"`. Strips the `models/`
/// prefix to get the bare model ID.
///
/// Returns the first available model ID, or `None` if the API is unreachable,
/// the key is invalid, or no models are found.
pub fn fetch_gemini_models(api_key: &str) -> Option<String> {
    let payload = blocking_get_json(
        format!(
            "https://generativelanguage.googleapis.com/v1beta/models?key={}",
            api_key
        ),
        None, // Gemini uses query-parameter auth (?key=), not a Bearer header
        &[],
        "fetch_gemini_models".to_string(),
    )?;

    let models = payload.get("models").and_then(|v| v.as_array())?;

    // Collect model IDs, stripping the "models/" prefix
    let mut model_ids: Vec<&str> = Vec::new();
    for model in models {
        let name = match model.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => continue,
        };
        // Strip "models/" prefix to get bare model ID
        let model_id = name.strip_prefix("models/").unwrap_or(name);
        // Skip deprecated/not-yet-supported models
        let supported = model
            .get("supportedGenerationMethods")
            .and_then(|v| v.as_array())
            .map(|methods| {
                methods
                    .iter()
                    .any(|m| m.as_str() == Some("generateContent"))
            })
            .unwrap_or(false);
        if supported {
            model_ids.push(model_id);
        }
    }

    let first = model_ids.first()?;
    tracing::info!(
        "Gemini available models: {} ({} support generateContent)",
        first,
        model_ids.len(),
    );
    Some((*first).to_string())
}
