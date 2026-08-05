// providers/free/discovery.rs — Live free-model discovery.
//
// Per-upstream endpoints that report the currently-free model list (Cline's
// recommended-models API, OpenRouter's models API, OpenAI-compatible
// /v1/models, Gemini's models API). Results are cached per upstream so
// runtime rebuilds of the free chain never re-run blocking network calls.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use super::{fetch_best_free_models_from_modelsdev, resolve_free_upstream_keys};

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
        /// The base URL of the OpenAI-compatible API, e.g.
        /// `"https://api.groq.com/openai/v1"`.
        base_url: &'static str,
    },
    /// Fetch from Google Gemini's `/v1beta/models` endpoint.
    /// Uses query-parameter auth (`?key=`). Response has a `models`
    /// array with `name` fields like `"models/gemini-2.5-flash"`.
    /// Strips the `models/` prefix to get the model ID.
    GeminiModels,
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
        // cloudflare: /ai/v1/models does not support GET (405) — the
        // hardcoded default_model is authoritative.
        "cloudflare" => FreeModelDiscovery::None,
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
/// Results (including failures) are cached per upstream id after the first
/// fetch, so runtime rebuilds of the free chain don't re-run blocking
/// network calls on the UI thread.
pub fn run_live_discovery(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<String> {
    // Fast path: previously discovered (or previously failed) result.
    if let Some(cached) = live_discovery_cache()
        .lock()
        .ok()
        .and_then(|guard| guard.get(upstream_id).cloned())
    {
        return cached;
    }
    let result = run_live_discovery_uncached(upstream_id, auth_store);
    if let Ok(mut guard) = live_discovery_cache().lock() {
        guard.insert(upstream_id.to_string(), result.clone());
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
        FreeModelDiscovery::GeminiModels => {
            let key = first_upstream_key(auth_store, "google")?;
            fetch_gemini_models(&key)
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
fn blocking_get_json(
    url: String,
    auth_bearer: Option<String>,
    context: String,
) -> Option<serde_json::Value> {
    std::thread::spawn(move || {
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| {
                let mut request = client.get(&url);
                if let Some(key) = auth_bearer {
                    request = request.header("Authorization", format!("Bearer {}", key));
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

    // Try to find the models.dev-recommended free model in the available list.
    let auto_detected = fetch_best_free_models_from_modelsdev();
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
    let first = available[0].to_string();
    tracing::info!(
        "{} live model list returned first model: {} ({} available)",
        upstream_id,
        first,
        available.len(),
    );
    Some(first)
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
