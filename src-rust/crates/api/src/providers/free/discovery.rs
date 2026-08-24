// providers/free/discovery.rs — Live free-model discovery.
//
// Per-upstream endpoints that report the currently-free model list (Cline's
// recommended-models API, OpenRouter's models API, OpenAI-compatible
// /v1/models, Gemini's models API). Results are cached per upstream so
// runtime rebuilds of the free chain never re-run blocking network calls.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use super::{
    fetch_best_free_models_from_modelsdev, modelsdev_free_model_ids, resolve_free_upstream_keys,
};
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
    /// Picks the models.dev auto-detected free model when the live list
    /// confirms it, else the catalog default when available, else `None`
    /// (never an arbitrary, possibly paid model).
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

    /// Fetch NVIDIA's own catalog API (`/v2/search/catalog/resources`). The
    /// catalog marks each endpoint's `PREVIEW` attribute (the "Free
    /// Endpoint" badge on build.nvidia.com) and a `DEPRECATION` date; only
    /// free, non-deprecated, chat-capable models are kept, cross-referenced
    /// against the OpenAI-compatible `/v1/models` list for callable wire IDs.
    NvidiaCatalogModels,
}

/// Map each FREE_CATALOG upstream to its live discovery method.
pub fn discovery_for(upstream_id: &str) -> FreeModelDiscovery {
    match upstream_id {
        "cline" => FreeModelDiscovery::ClineRecommended,
        "openrouter" => FreeModelDiscovery::OpenRouterFreeModels,

        "cerebras" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://api.cerebras.ai/v1",
        },
        // nvidia uses its catalog API for the free-endpoint (PREVIEW) badge —
        // the OpenAI-compat /v1/models list has no free/paid field.
        "nvidia" => FreeModelDiscovery::NvidiaCatalogModels,
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
        "poolside" => FreeModelDiscovery::OpenAiModelList {
            base_url: "https://inference.poolside.ai/v1",
        },
        // github-copilot intentionally stays on the catch-all None here: the
        // CopilotProvider fetches its own /models list internally (with a
        // hardcoded fallback), so it needs no separate discovery probe.
        _ => FreeModelDiscovery::None,
    }
}

/// Per-upstream cache for live free-model discovery results (full lists).
/// Populated on the first build and reused by runtime rebuilds so
/// `refresh_free_provider` (triggered by /keys, /logout, /refresh, the
/// free-mode dialog, and the /ollama toggle) never blocks the UI thread on
/// repeated network fetches. Free-model lists are slow-moving within a
/// session, mirroring the `AUTO_DETECTED_DEFAULTS` models.dev cache.
static LIVE_DISCOVERY_CACHE: OnceLock<Mutex<HashMap<String, Option<Vec<String>>>>> =
    OnceLock::new();
fn live_discovery_cache() -> &'static Mutex<HashMap<String, Option<Vec<String>>>> {
    LIVE_DISCOVERY_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clear the in-process live-discovery cache so the next chain build re-probes
/// every configured upstream (used by `clawde --refresh-models`).
pub(crate) fn clear_live_discovery_cache() {
    if let Some(cache) = LIVE_DISCOVERY_CACHE.get() {
        if let Ok(mut guard) = cache.lock() {
            guard.clear();
        }
    }
}

/// Run live discovery for the first entry whose ID matches `upstream_id` and
/// return the current best free model for it.
///
/// This is the single-model view over [`run_live_discovery_models`]: the
/// full discovered list is fetched once (cached), and the chain's effective
/// model is its first entry — every discovery variant orders its candidates
/// so the default pick comes first (preference-ordered allowlists, catalog
/// default, largest-context ranking), keeping the chain pick byte-identical
/// to the old single-model probes.
pub fn run_live_discovery(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<String> {
    run_live_discovery_models(upstream_id, auth_store)?
        .into_iter()
        .next()
}

/// Run live discovery for the first entry whose ID matches `upstream_id` and
/// return the FULL list of currently-free models for it (all callable wire
/// IDs, in default-pick-first order). `None` when discovery is not configured
/// or the fetch fails.
///
/// Successful results are cached per upstream id after the first fetch, so
/// runtime rebuilds of the free chain and the Alt+J/K popup don't re-run
/// blocking network calls on the UI thread. Failed results are deliberately
/// retried on later calls.
pub fn run_live_discovery_models(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<Vec<String>> {
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
    let result = run_live_discovery_models_uncached(upstream_id, auth_store);
    // Cache only successful discovery. A transient outage, missing key, or
    // pre-authentication probe must not permanently mask a later recovery.
    if let Some(models) = result.as_ref() {
        super::save_live_discovery_cache(upstream_id, Some(models.clone()));
        if let Ok(mut guard) = live_discovery_cache().lock() {
            guard.insert(upstream_id.to_string(), Some(models.clone()));
        }
    }
    result
}

/// The uncached full-list discovery fetch — see [`run_live_discovery_models`].
fn run_live_discovery_models_uncached(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<Vec<String>> {
    match discovery_for(upstream_id) {
        FreeModelDiscovery::ClineRecommended => {
            let key = first_upstream_key(auth_store, "cline")?;
            fetch_cline_free_models(&key)
        }
        FreeModelDiscovery::OpenRouterFreeModels => {
            let key = first_upstream_key(auth_store, "openrouter")?;
            fetch_openrouter_free_models(&key)
        }
        FreeModelDiscovery::OpenAiModelList { base_url } => {
            let key = first_upstream_key(auth_store, upstream_id)?;
            fetch_openai_compat_free_models(&key, base_url, upstream_id)
        }
        FreeModelDiscovery::OpenCodeZenFreeModels { base_url } => {
            // `/models` is public and must remain discoverable even when the
            // stored Zen credential is stale or currently creditless.
            fetch_opencode_zen_free_models(base_url)
        }
        FreeModelDiscovery::GeminiModels => {
            let key = first_upstream_key(auth_store, "google")?;
            fetch_gemini_free_models(&key)
        }
        FreeModelDiscovery::CloudflareModels => {
            let key = first_upstream_key(auth_store, "cloudflare")?;
            fetch_cloudflare_available_free_models(&key)
        }

        FreeModelDiscovery::NvidiaCatalogModels => {
            // The catalog API is public; pass the stored key through for the
            // /v1/models wire-ID cross-reference when one exists.
            let key = first_upstream_key(auth_store, "nvidia");
            fetch_nvidia_catalog_free_models(key.as_deref())
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
    fetch_openrouter_free_models(openrouter_api_key)?
        .into_iter()
        .next()
}

/// Fetch ALL of OpenRouter's currently-free models, sorted by context window
/// descending so the default pick (largest context) comes first.
///
/// Same endpoint and filtering as [`fetch_openrouter_free_model`]; returns
/// every qualifying model id instead of just the best.
pub fn fetch_openrouter_free_models(openrouter_api_key: &str) -> Option<Vec<String>> {
    let payload = blocking_get_json(
        "https://openrouter.ai/api/v1/models".to_string(),
        Some(openrouter_api_key.to_string()),
        &[],
        "fetch_openrouter_free_models".to_string(),
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

    // Sort by context window descending — the largest-context free model is
    // the default pick, everything else follows it.
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    if candidates.is_empty() {
        tracing::warn!("fetch_openrouter_free_models: no free tool-capable models found");
        return None;
    }
    let ids: Vec<String> = candidates
        .into_iter()
        .map(|(id, _)| id.to_string())
        .collect();
    tracing::info!("OpenRouter free models: {} (first: {})", ids.len(), ids[0],);
    Some(ids)
}

/// Fetch OpenCode Zen's current free model list.
///
/// Zen exposes paid and free models from the same public `/models` endpoint.
/// Only IDs ending in `-free` (plus the `ZEN_KNOWN_FREE` exceptions) are
/// eligible; do not fall back to the first arbitrary model because that could
/// turn Free mode into a paid request.
pub fn fetch_opencode_zen_free_model(base_url: &str) -> Option<String> {
    fetch_opencode_zen_free_models(base_url)?.into_iter().next()
}

/// Fetch all current OpenCode Zen free model IDs.
///
/// The endpoint contains both paid and free models. Only IDs ending in
/// `-free` (plus the `ZEN_KNOWN_FREE` exceptions) are returned, so this
/// remains safe as the public catalog changes.
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

/// Zen free models whose API ids do not carry the `-free` suffix convention.
/// Zen's pricing page lists these at $0/1M tokens (e.g. `big-pickle`) even
/// though the `/models` endpoint returns them unsuffixed, so the suffix rule
/// alone would silently drop them from the free pool.
const ZEN_KNOWN_FREE: &[&str] = &["big-pickle"];

/// Select all free models from a Zen `/models` payload: `*-free` suffixed ids
/// in endpoint order first (so the default pick is unchanged), then the
/// known-free unsuffixed exceptions. Kept separate from HTTP so the
/// paid-model exclusion is regression-tested without a network dependency.
fn select_opencode_zen_free_models(payload: &serde_json::Value) -> Option<Vec<String>> {
    let models = payload.get("data").and_then(|v| v.as_array())?;
    let ids: Vec<&str> = models
        .iter()
        .filter_map(|model| model.get("id").and_then(|id| id.as_str()))
        .collect();
    let mut free_models: Vec<String> = ids
        .iter()
        .copied()
        .filter(|id| id.ends_with("-free"))
        .map(str::to_owned)
        .collect();
    // Known-free exceptions are only eligible when the live payload actually
    // still lists them (an allowlist entry must never fabricate a model Zen
    // has since removed).
    for id in ZEN_KNOWN_FREE {
        if ids.contains(id) {
            free_models.push((*id).to_string());
        }
    }
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
/// found, returns `None` so the chain keeps the known-free catalog default.
pub fn fetch_openai_compat_model_list(
    api_key: &str,
    base_url: &str,
    upstream_id: &str,
) -> Option<String> {
    fetch_openai_compat_free_models(api_key, base_url, upstream_id)?
        .into_iter()
        .next()
}

/// Fetch ALL known-free models from a standard OpenAI-compatible `/v1/models`
/// endpoint, default pick first.
///
/// The live list mixes paid and free models with no per-model free field, so
/// the candidates are the curated known-free allowlist + the models.dev free
/// set for this upstream + the catalog fallbacks, intersected with what the
/// endpoint actually serves. This is the "everything free on this provider"
/// signal behind the Alt+J/K popup for OpenAI-compatible upstreams.
pub fn fetch_openai_compat_free_models(
    api_key: &str,
    base_url: &str,
    upstream_id: &str,
) -> Option<Vec<String>> {
    let models_url = format!("{}/models", base_url.trim_end_matches('/'));
    let payload = blocking_get_json(
        models_url,
        Some(api_key.to_string()),
        &[],
        format!("fetch_openai_compat_free_models({})", upstream_id),
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
            "fetch_openai_compat_free_models({}): no models in response",
            upstream_id
        );
        return None;
    }

    let list = select_available_models(
        upstream_id,
        &available,
        fetch_best_free_models_from_modelsdev(),
    );
    if list.is_empty() {
        tracing::warn!(
            "fetch_openai_compat_free_models({}): no known-free model available ({} listed)",
            upstream_id,
            available.len(),
        );
        return None;
    }
    Some(list)
}

/// NVIDIA catalog API base URL. The search response is paged with a `q`
/// parameter containing `{"query":"*:*","page":N,"pageSize":24}`; each
/// page's `results[].resources[]` entry carries the model's `displayName` and
/// an `attributes` array whose `PREVIEW` key marks the "Free Endpoint" badge
/// and whose `DEPRECATION` key (when present) marks a model being retired.
const NVIDIA_CATALOG_BASE: &str = "https://api.ngc.nvidia.com/v2/search/catalog/resources";

/// Page size for the catalog search. The API rejects pageSize > 24.
const NVIDIA_CATALOG_PAGE_SIZE: usize = 24;

/// Safety cap on catalog pages (the catalog has ~7 pages today).
const NVIDIA_CATALOG_MAX_PAGES: usize = 8;

/// NVIDIA's OpenAI-compatible `/v1/models` endpoint — public, returns the
/// callable wire IDs (`publisher/name`) used to map catalog display names.
const NVIDIA_MODELS_URL: &str = "https://integrate.api.nvidia.com/v1/models";

/// NVIDIA free-endpoint models that are not chat models (audio, video,
/// embedding, image-gen, driving, safety guards, translation, TTS, rerank,
/// OCR). The catalog API has no task field for ENDPOINT resources, so the
/// exclusion is curated — matching the `CLOUDFLARE_PAID_REQUIRED` deny-list
/// pattern. Names are pre-normalized (lowercase, non-alphanumerics folded to
/// `-`) to match [`normalize_model_id`] output.
const NVIDIA_NON_CHAT: &[&str] = &[
    "active-speaker-detection",
    "background-noise-removal",
    "bevformer",
    "cosmos-transfer1-7b",
    "cosmos-transfer2-5-2b",
    "cosmos3-nano",
    "cosmos3-nano-reasoner",
    "diffusiongemma-26b-a4b-it",
    "esm2-650m",
    "esmfold",
    "ising-calibration-1-35b-a3b",
    "ising-calibration-1-5-31b",
    "llama-3-1-nemotron-safety-guard-8b-v3",
    "llama-guard-4-12b",
    "magpie-tts-zeroshot",
    "nemotron-3-embed-1b",
    "nemotron-3-5-content-safety",
    "nemotron-voicechat",
    "nv-embed-v1",
    "nv-embedcode-7b-v1",
    "rerank-qa-mistral-4b",
    "riva-translate-4b-instruct-v1-1",
    "riva-translate-4b-instruct-v2",
    "sparsedrive",
    "streampetr",
    "studio-voice",
    "synthetic-video-detector",
];

/// Preference order for the default NVIDIA pick when several free chat
/// models are live. `gpt-oss-120b` is the same free strong generalist Clawde
/// pins for groq/cerebras; `nemotron-3.5-lightning` is NVIDIA's current free
/// flagship. The rest of the list keeps catalog order (deduped).
const NVIDIA_PREFERRED_FREE: &[&str] = &[
    "openai/gpt-oss-120b",
    "nvidia/nemotron-3.5-lightning-30b-a3b",
];

/// Fetch the current free chat model list directly from NVIDIA's own catalog
/// API, cross-referenced against the OpenAI-compatible `/v1/models` list.
///
/// Pipeline (all provider-authoritative, no models.dev dependency):
///   1. Page the catalog API (`/v2/search/catalog/resources`) and keep every
///      ENDPOINT whose `PREVIEW` attribute is `"true"` (the "Free Endpoint"
///      badge on build.nvidia.com).
///   2. Drop models NVIDIA marks `DEPRECATION` and the curated non-chat
///      exclusion list (audio/video/embedding/driving/etc).
///   3. Map the catalog display name to a callable wire ID by matching the
///      normalized short name against `/v1/models` (e.g. `llama-3.3-70b-…` →
///      `meta/llama-3.3-70b-instruct`).
///
/// Returns `None` when the catalog or models endpoint is unreachable or no
/// qualifying model survives the filters (the chain then keeps the catalog
/// default). A transient page failure keeps whatever earlier pages yielded.
pub fn fetch_nvidia_catalog_free_models(nvidia_key: Option<&str>) -> Option<Vec<String>> {
    let mut entries: Vec<(String, bool)> = Vec::new();
    for page in 0..NVIDIA_CATALOG_MAX_PAGES {
        let q = serde_json::json!({ "query": "*:*", "page": page, "pageSize": NVIDIA_CATALOG_PAGE_SIZE })
            .to_string();
        let url = format!(
            "{}?resource-type=ENDPOINT&group-labels-by-labelset=true&q={}",
            NVIDIA_CATALOG_BASE,
            urlencoding::encode(&q),
        );
        let Some(payload) = blocking_get_json(
            url,
            None,
            &[],
            "fetch_nvidia_catalog_free_models".to_string(),
        ) else {
            tracing::warn!(
                "fetch_nvidia_catalog_free_models: catalog page {} failed",
                page
            );
            break;
        };
        let page_entries = collect_nvidia_catalog_entries(&payload);
        if page_entries.is_empty() {
            break;
        }
        entries.extend(page_entries);
    }
    if entries.is_empty() {
        tracing::warn!("fetch_nvidia_catalog_free_models: no free-endpoint models found");
        return None;
    }

    // Wire IDs from the OpenAI-compatible endpoint (public; the user's key is
    // passed through when present for full account visibility).
    let payload = blocking_get_json(
        NVIDIA_MODELS_URL.to_string(),
        nvidia_key.map(str::to_string),
        &[],
        "fetch_nvidia_catalog_free_models".to_string(),
    )?;
    let wire_ids: Vec<&str> = payload
        .get("data")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if wire_ids.is_empty() {
        tracing::warn!("fetch_nvidia_catalog_free_models: no wire ids from /v1/models");
        return None;
    }

    let list = select_nvidia_catalog_free_models(&entries, &wire_ids);
    if list.is_empty() {
        tracing::warn!("fetch_nvidia_catalog_free_models: no callable free chat models");
        return None;
    }
    tracing::info!(
        "NVIDIA free chat models: {} (first: {})",
        list.len(),
        list[0],
    );
    Some(list)
}

/// Extract `(display_name, deprecated)` for every free-endpoint (`PREVIEW` ==
/// `"true"`) ENDPOINT resource in one catalog page payload. Kept pure so the
/// filter is regression-tested without a network dependency.
fn collect_nvidia_catalog_entries(payload: &serde_json::Value) -> Vec<(String, bool)> {
    let mut entries = Vec::new();
    let Some(groups) = payload.get("results").and_then(|v| v.as_array()) else {
        return entries;
    };
    for group in groups {
        let Some(resources) = group.get("resources").and_then(|v| v.as_array()) else {
            continue;
        };
        for resource in resources {
            let Some(name) = resource
                .get("displayName")
                .and_then(|v| v.as_str())
                .or_else(|| resource.get("name").and_then(|v| v.as_str()))
            else {
                continue;
            };
            let attributes: Vec<(&str, &str)> = resource
                .get("attributes")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| {
                            let key = a.get("key").and_then(|k| k.as_str())?;
                            let value = a.get("value").and_then(|v| v.as_str()).unwrap_or("");
                            Some((key, value))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let is_preview = attributes
                .iter()
                .any(|(key, value)| *key == "PREVIEW" && *value == "true");
            if !is_preview {
                continue;
            }
            let deprecated = attributes.iter().any(|(key, _)| *key == "DEPRECATION");
            entries.push((name.to_string(), deprecated));
        }
    }
    entries
}

/// Lowercase a model name and fold every non-alphanumeric run into a single
/// `-`, so the catalog's `llama-3.3-70b-instruct` / `riva-…-v1_1` spellings
/// match the wire ID tail `meta/llama-3.3-70b-instruct` / `…v1.1`.
fn normalize_model_id(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Select the callable free chat models from the catalog entries: drop
/// deprecated and curated non-chat models, then match each remaining display
/// name against the `/v1/models` wire IDs by normalized short name. Results
/// are deduped, with [`NVIDIA_PREFERRED_FREE`] first (stable default pick),
/// then alphabetical. Kept pure for unit testing.
fn select_nvidia_catalog_free_models(entries: &[(String, bool)], wire_ids: &[&str]) -> Vec<String> {
    let mut by_tail: HashMap<String, &str> = HashMap::new();
    for wid in wire_ids {
        let tail = wid.rsplit('/').next().unwrap_or(wid);
        by_tail.entry(normalize_model_id(tail)).or_insert(wid);
    }
    let mut found: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (name, deprecated) in entries {
        if *deprecated {
            continue;
        }
        let norm = normalize_model_id(name);
        if NVIDIA_NON_CHAT.contains(&norm.as_str()) {
            continue;
        }
        if let Some(wid) = by_tail.get(&norm) {
            if seen.insert((*wid).to_string()) {
                found.push((*wid).to_string());
            }
        }
    }
    found.sort_by(|a, b| {
        let pa = NVIDIA_PREFERRED_FREE
            .iter()
            .position(|p| p == a)
            .unwrap_or(usize::MAX);
        let pb = NVIDIA_PREFERRED_FREE
            .iter()
            .position(|p| p == b)
            .unwrap_or(usize::MAX);
        pa.cmp(&pb).then_with(|| a.cmp(b))
    });
    found
}

/// Curated known-free model ids per upstream, in preference order, checked
/// before the models.dev cross-reference. models.dev is missing entirely for
/// sambanova and cloudflare and stale for groq (its free pick can be a
/// non-chat model like `allam-2-7b`), cerebras, and cohere. These entries
/// are the providers' own free-tier designations; only ids actually present
/// in the live list are eligible, so a removed model never gets fabricated.
const KNOWN_FREE_MODELS: &[(&str, &[&str])] = &[
    ("groq", &["openai/gpt-oss-120b", "llama-3.3-70b-versatile"]),
    ("cerebras", &["gpt-oss-120b"]),
    ("sambanova", &["Meta-Llama-3.3-70B-Instruct"]),
    ("cloudflare", &[super::catalog::CLOUDFLARE_PROBE_MODEL]),
    // Poolside: free-in-Preview, flagship coding model is laguna-s-2.1.
    (
        "poolside",
        &["poolside/laguna-s-2.1", "poolside/laguna-xs-2.1"],
    ),
    // Mistral's models.dev free pick (labs-devstral-small-2512) is retired
    // (3/31/2026); the Experiment tier makes every current model free, so
    // pin the flagship. Z.AI's docs mark GLM-4.7-Flash / GLM-4.5-Flash free
    // (GLM-4.7 itself is paid); models.dev agrees but the catalog fallback
    // must never land on the paid variant.
    ("mistral", &["mistral-large-2512"]),
    ("zai", &["glm-4.7-flash", "glm-4.5-flash"]),
];

/// Upstreams with a generous credit-based free tier where ALL models on the
/// live list are usable (not just the allowlisted picks). These providers
/// give monthly token credits rather than per-model free access, so the
/// Alt+J/K popup should show every model the API returns.
///
/// - Mistral: Experiment tier (~1B tokens/month, all models)
/// - SambaNova: Developer tier (~600M tokens/month, all models)
const CREDIT_BASED_FREE: &[&str] = &["mistral", "sambanova"];

/// Shared selection rule for live model lists: prefer the curated
/// known-free allowlist when it matches, then the models.dev auto-detected
/// pick when the live list confirms it, then the catalog's `default_model`.
/// Returns `None` when none is on the live list — never an arbitrary
/// (possibly paid) model; the chain then keeps the known-free catalog
/// default.
///
/// Kept pure (no network) so every discovery path — OpenAI-compat, Cohere,
/// Cloudflare — uses one unit-testable precedence.
fn select_available_model(
    upstream_id: &str,
    available: &[&str],
    auto_detected: &HashMap<String, String>,
) -> Option<String> {
    // Curated known-free picks first — models.dev is missing or stale for
    // several upstreams, so this allowlist is the authoritative free signal.
    if let Some((_, known)) = KNOWN_FREE_MODELS
        .iter()
        .find(|(id, _)| *id == upstream_id)
        .copied()
    {
        for model in known {
            if available.contains(model) {
                tracing::info!(
                    "{} live model list confirmed known-free pick: {}",
                    upstream_id,
                    model,
                );
                return Some((*model).to_string());
            }
        }
    }
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
    // No safe pick: neither the models.dev free model nor the catalog default
    // is live here. Return None so the chain keeps the known-free catalog
    // default instead of risking an arbitrary (possibly paid) model.
    tracing::warn!(
        "{} live model list: no known-free model available ({} listed)",
        upstream_id,
        available.len(),
    );
    None
}

/// Full-list variant of [`select_available_model`]: every known-free model
/// from the live list, default pick first.
///
/// Ordering contract: the first element is EXACTLY what
/// [`select_available_model`] would pick (so the chain's effective model is
/// unchanged when a caller derives it as `.first()`), followed by the rest of
/// the models.dev free set for this upstream that the live list confirms, then
/// the catalog's fallback models.
fn select_available_models(
    upstream_id: &str,
    available: &[&str],
    auto_detected: &HashMap<String, String>,
) -> Vec<String> {
    select_available_models_from(
        upstream_id,
        available,
        auto_detected,
        &modelsdev_free_model_ids(upstream_id),
    )
}

/// Pure core of [`select_available_models`] with the models.dev free set
/// injected, so the ordering is unit-testable without a network fetch.
fn select_available_models_from(
    upstream_id: &str,
    available: &[&str],
    auto_detected: &HashMap<String, String>,
    modelsdev_free: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    // Default pick first — the single-model precedence (known-free allowlist
    // → models.dev pick → catalog default) must stay authoritative for the
    // chain, so it is computed via the single selector and prepended.
    if let Some(pick) = select_available_model(upstream_id, available, auto_detected) {
        out.push(pick);
    }
    // Then the REST of the curated allowlist (the single selector only ever
    // returns the first match). These are the provider's own free-tier
    // designations, so they are authoritative even when models.dev has no
    // entry for the upstream (e.g. poolside — its laguna-xs-2.1 sibling
    // would otherwise never surface in the Alt+J/K popup).
    if let Some((_, known)) = KNOWN_FREE_MODELS
        .iter()
        .find(|(id, _)| *id == upstream_id)
        .copied()
    {
        for model in known {
            let owned = (*model).to_string();
            if !out.contains(&owned) && available.contains(model) {
                out.push(owned);
            }
        }
    }
    // Then the rest of models.dev's free set for this upstream (context-desc
    // order), restricted to ids the live list actually serves.
    for model in modelsdev_free {
        if !out.contains(model) && available.contains(&model.as_str()) {
            out.push(model.clone());
        }
    }
    // Finally the catalog fallback models when the live list serves them.
    if let Some(entry) = crate::providers::free::catalog_entry(upstream_id) {
        for model in entry.fallback_models {
            let owned = model.to_string();
            if !out.contains(&owned) && available.contains(model) {
                out.push(owned);
            }
        }
    }
    // For credit-based free tiers (Mistral Experiment, SambaNova Developer),
    // ALL models on the live list are usable within the monthly allowance.
    // Append every remaining live model so the Alt+J/K popup shows the full
    // catalog, not just the curated picks.
    if CREDIT_BASED_FREE.contains(&upstream_id) {
        for model in available {
            let owned = (*model).to_string();
            if !out.contains(&owned) {
                out.push(owned);
            }
        }
    }
    out
}

/// Fetch the best Cloudflare Workers AI model available to this account.
///
/// Cloudflare's OpenAI-compatible `/ai/v1/models` route does not support GET
/// (405), so this probes the account-scoped catalog instead:
/// `GET /accounts/{ACCOUNT_ID}/ai/models/search` authenticated with the
/// composite `ACCOUNT_ID:API_TOKEN` credential. The response's `result[].name`
/// carries the request-time model IDs (`@cf/qwen/qwen3-30b-a3b-fp8`) and
/// `result[].source` distinguishes neuron-billed `hosted` models (free-tier
/// eligible) from third-party-billed `proxied` ones — only hosted models are
/// candidates. Because the endpoint is account-scoped, it also catches
/// models that models.dev marks free but this account cannot actually serve.
///
/// Fetch ALL free-eligible Cloudflare Workers AI models available to this
/// account, default pick first (the best is available as the first element).
pub fn fetch_cloudflare_available_free_models(cloudflare_key: &str) -> Option<Vec<String>> {
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
                    "fetch_cloudflare_available_free_models: key is not ACCOUNT_ID:API_TOKEN and \
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
        "fetch_cloudflare_available_free_models".to_string(),
    )?;
    let available = collect_cloudflare_available_models(&payload);
    if available.is_empty() {
        tracing::warn!("fetch_cloudflare_available_free_models: no @cf/ models in response");
        return None;
    }
    let list = select_available_models(
        "cloudflare",
        &available,
        fetch_best_free_models_from_modelsdev(),
    );
    if list.is_empty() {
        tracing::warn!(
            "fetch_cloudflare_available_free_models: no known-free model available ({} listed)",
            available.len(),
        );
        return None;
    }
    Some(list)
}

/// Model IDs Cloudflare marks as requiring a paid billing method (Workers
/// Paid plan or prepaid AI Gateway credits) despite being `@cf/`-prefixed
/// hosted models. Named on the Workers AI pricing page; everything else on
/// the platform is covered by the 10K-neurons/day free allocation.
const CLOUDFLARE_PAID_REQUIRED: &[&str] = &[
    "@cf/moonshotai/kimi-k2.6",
    "@cf/moonshotai/kimi-k2.7-code",
    "@cf/zai-org/glm-5.2",
    "@cf/deepseek-ai/deepseek-v4-flash-0731",
    "@cf/deepseek-ai/deepseek-v4-pro-0813",
];

/// Extract available Cloudflare model IDs from an `/ai/models/search` payload.
///
/// The search response lists models as objects whose `name` carries the
/// request-time model ID (`@cf/...`) and whose `task.name` distinguishes
/// text-generation LLMs from embeddings/classifiers. The `source` field is
/// the free-tier signal: `"hosted"` means neuron-billed and covered by the
/// free allocation, while `"proxied"` routes to a third-party provider and
/// bills separately (never free). Models whose `source` is missing or
/// `"proxied"` are excluded, as are the named paid-required models above.
/// Text-generation models are kept first in the candidate list; selection is
/// by membership (`select_available_model`), so the ordering is informational.
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
    // `source` is Cloudflare's delivery-mode marker: hosted = neuron-billed
    // (free-tier eligible), proxied = third-party billed (never free). Treat
    // a missing source as NOT free-eligible so a schema change can't
    // accidentally admit a proxied model.
    //
    // Cloudflare changed this field from string `"hosted"` to number `1`
    // (circa 2026-08); accept both so either wire format works.
    let is_hosted = |model: &serde_json::Value| match model.get("source") {
        Some(serde_json::Value::String(s)) => s == "hosted",
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    };
    let mut text_generation: Vec<&str> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for model in models {
        let Some(id) = model.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !id.starts_with("@cf/") || !is_hosted(model) || CLOUDFLARE_PAID_REQUIRED.contains(&id) {
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

/// Fetch Google Gemini's current available models from their models API.
///
/// Gemini's API at `https://generativelanguage.googleapis.com/v1beta/models`
/// uses query-parameter auth (`?key=`). Response has a `models` array with
/// `name` fields like `"models/gemini-2.5-flash"`. Strips the `models/`
/// prefix to get the bare model ID.
///
/// Returns the catalog default when the live list serves it, else the
/// models.dev free pick, else `None` — never an arbitrary (possibly paid)
/// model. `None` also when the API is unreachable, the key is invalid, or no
/// generateContent-capable models are found.
pub fn fetch_gemini_models(api_key: &str) -> Option<String> {
    fetch_gemini_free_models(api_key)?.into_iter().next()
}

/// Known-free Gemini models on the Developer API free tier (August 2026).
///
/// Gemini's free tier is NOT every generateContent-capable model — Pro
/// models left the free tier on April 1, 2026, and TTS / image-generation /
/// deep-research / robotics models are paid-only. The /v1beta/models API
/// lists all models regardless of tier, so we intersect against this
/// curated set to avoid routing free-mode requests to paid endpoints.
///
/// Pattern: Flash and Flash-Lite variants are free; Gemma is free
/// (open-weight hosted). Pro, image, TTS, and specialty models are not.
const GEMINI_KNOWN_FREE: &[&str] = &[
    // Flash
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-2.5-flash",
    "gemini-flash-latest",
    // Flash-Lite
    "gemini-3.5-flash-lite",
    "gemini-3.1-flash-lite",
    "gemini-2.5-flash-lite",
    "gemini-flash-lite-latest",
    // Open-weight (Gemma) — free hosted on AI Studio
    "gemma-4-26b-a4b-it",
    "gemma-4-31b-it",
];

/// Fetch known-free Gemini models, default pick first.
///
/// The /v1beta/models API lists all models regardless of tier. We intersect
/// against [`GEMINI_KNOWN_FREE`] so paid-only models (Pro, TTS, image, deep
/// research, robotics, etc.) are excluded. The catalog default
/// (gemini-2.5-flash) is ordered first via [`select_gemini_model`]'s
/// precedence so the chain pick is unchanged; the rest follow in API order
/// for the Alt+J/K popup.
pub fn fetch_gemini_free_models(api_key: &str) -> Option<Vec<String>> {
    let payload = blocking_get_json(
        format!(
            "https://generativelanguage.googleapis.com/v1beta/models?key={}",
            api_key
        ),
        None, // Gemini uses query-parameter auth (?key=), not a Bearer header
        &[],
        "fetch_gemini_free_models".to_string(),
    )?;

    let models = payload.get("models").and_then(|v| v.as_array())?;

    // Collect generateContent-capable model IDs, then intersect with the
    // known-free allowlist so paid-only models (Pro, TTS, image, deep
    // research, robotics, etc.) are excluded.
    let model_ids: Vec<&str> = models
        .iter()
        .filter_map(|model| {
            let name = model.get("name").and_then(|v| v.as_str())?;
            let supported = model
                .get("supportedGenerationMethods")
                .and_then(|v| v.as_array())
                .map(|methods| {
                    methods
                        .iter()
                        .any(|m| m.as_str() == Some("generateContent"))
                })
                .unwrap_or(false);
            supported.then_some(name.strip_prefix("models/").unwrap_or(name))
        })
        .filter(|id| GEMINI_KNOWN_FREE.contains(id))
        .collect();

    if model_ids.is_empty() {
        tracing::warn!("fetch_gemini_free_models: no known-free models on live list");
        return None;
    }

    // Move the safe default pick to the front (catalog default → models.dev
    // pick per select_gemini_model), keeping the rest in API order.
    let mut ids: Vec<String> = model_ids.into_iter().map(str::to_owned).collect();
    if let Some(pick) = select_gemini_model(
        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        fetch_best_free_models_from_modelsdev(),
    ) {
        if let Some(pos) = ids.iter().position(|m| *m == pick) {
            let picked = ids.remove(pos);
            ids.insert(0, picked);
        }
    }
    tracing::info!("Gemini free models: {} (first: {})", ids.len(), ids[0],);
    Some(ids)
}

/// Selection for Gemini's live model list. Gemini's free tier is quota-based
/// on the API key rather than a per-model cost, so the catalog default
/// (gemini-2.5-flash) wins whenever the live list still serves it; the
/// models.dev pick is a secondary fallback. Never returns an arbitrary first
/// model, which could be paid.
///
/// Kept pure (no network) so the precedence is unit-testable.
fn select_gemini_model(
    available: &[&str],
    auto_detected: &HashMap<String, String>,
) -> Option<String> {
    if let Some(entry) = crate::providers::free::catalog_entry("google") {
        if available.contains(&entry.default_model) {
            return Some(entry.default_model.to_string());
        }
    }
    if let Some(recommended) = auto_detected.get("google") {
        if available.contains(&recommended.as_str()) {
            return Some(recommended.clone());
        }
    }
    None
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
    fn opencode_discovery_keeps_suffixed_free_models_then_known_free() {
        let payload = serde_json::json!({
            "data": [
                {"id": "minimax-m2.5"},
                {"id": "deepseek-v4-flash-free"},
                {"id": "big-pickle"},
                {"id": "mimo-v2.5-free"}
            ]
        });
        // Paid (minimax-m2.5) is dropped; `-free` suffixed ids keep endpoint
        // order and stay ahead of the known-free exception (big-pickle), so
        // the default pick is unchanged.
        assert_eq!(
            select_opencode_zen_free_models(&payload),
            Some(vec![
                "deepseek-v4-flash-free".to_string(),
                "mimo-v2.5-free".to_string(),
                "big-pickle".to_string(),
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
    fn opencode_discovery_known_free_exception_included_without_suffix() {
        // big-pickle is $0 on Zen's pricing page but has no `-free` suffix;
        // the allowlist must catch it on its own.
        let payload = serde_json::json!({
            "data": [{"id": "big-pickle"}]
        });
        assert_eq!(
            select_opencode_zen_free_models(&payload),
            Some(vec!["big-pickle".to_string()])
        );
    }

    #[test]
    fn opencode_discovery_returns_none_without_free_models() {
        let payload = serde_json::json!({
            "data": [{"id": "minimax-m2.5"}, {"id": "gpt-5.6-luna"}]
        });
        assert_eq!(select_opencode_zen_free_models(&payload), None);
    }

    #[test]
    fn cloudflare_discovery_extracts_cf_models_and_prefers_text_generation() {
        let payload = serde_json::json!({
            "success": true,
            "result": [
                {"name": "@cf/baai/bge-m3", "source": "hosted", "task": {"name": "Text Embeddings"}},
                {"name": "@cf/qwen/qwen3-30b-a3b-fp8", "source": "hosted", "task": {"name": "Text Generation"}},
                {"name": "@cf/openai/gpt-oss-120b", "source": "hosted", "task": {"name": "Text Generation"}},
                {"name": "not-a-cf-model", "source": "hosted", "task": {"name": "Text Generation"}}
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
    fn cloudflare_discovery_excludes_proxied_and_paid_required_models() {
        // `source: "proxied"` models route to third-party providers and are
        // never free; the named paid-required models are hosted but excluded
        // from the free allocation per the pricing page. Both must be dropped
        // from the free-eligible candidate list.
        let payload = serde_json::json!({
            "success": true,
            "result": [
                {"name": "@cf/openai/gpt-5.5", "source": "proxied", "task": {"name": "Text Generation"}},
                {"name": "@cf/deepseek-ai/deepseek-v4-flash-0731", "source": "hosted", "task": {"name": "Text Generation"}},
                {"name": "@cf/qwen/qwen3-30b-a3b-fp8", "source": "hosted", "task": {"name": "Text Generation"}}
            ]
        });
        assert_eq!(
            collect_cloudflare_available_models(&payload),
            vec!["@cf/qwen/qwen3-30b-a3b-fp8"]
        );
    }

    #[test]
    fn cloudflare_discovery_excludes_models_without_source_field() {
        // A missing `source` is treated as NOT free-eligible so a schema
        // change can't accidentally admit a proxied model.
        let payload = serde_json::json!({
            "result": [
                {"name": "@cf/qwen/qwen3-30b-a3b-fp8", "task": {"name": "Text Generation"}}
            ]
        });
        assert!(collect_cloudflare_available_models(&payload).is_empty());
    }

    #[test]
    fn cloudflare_discovery_accepts_numeric_source() {
        // Cloudflare changed `source` from string "hosted" to number 1
        // circa 2026-08; both formats must be accepted.
        let payload = serde_json::json!({
            "result": [
                {"name": "@cf/qwen/qwen3-30b-a3b-fp8", "source": 1, "task": {"name": "Text Generation"}},
                {"name": "@cf/openai/gpt-oss-120b", "source": 1, "task": {"name": "Text Generation"}},
                {"name": "@cf/baai/bge-m3", "source": 1, "task": {"name": "Text Embeddings"}}
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
    fn select_available_model_prefers_known_free_over_modelsdev() {
        // Groq's models.dev free pick can be a non-chat model (allam-2-7b);
        // the curated known-free allowlist must win when its entries are live.
        let available: Vec<&str> = vec!["allam-2-7b", "openai/gpt-oss-120b"];
        let auto = HashMap::from([("groq".to_string(), "allam-2-7b".to_string())]);
        assert_eq!(
            select_available_model("groq", &available, &auto).as_deref(),
            Some("openai/gpt-oss-120b")
        );
    }

    #[test]
    fn select_available_model_known_free_falls_through_when_absent() {
        // No allowlist entry on the live list → models.dev pick applies.
        let available: Vec<&str> = vec!["allam-2-7b"];
        let auto = HashMap::from([("groq".to_string(), "allam-2-7b".to_string())]);
        assert_eq!(
            select_available_model("groq", &available, &auto).as_deref(),
            Some("allam-2-7b")
        );
    }

    #[test]
    fn select_available_model_mistral_pins_current_model_over_retired_modelsdev_pick() {
        // models.dev still marks labs-devstral-small-2512 (retired 3/31/2026)
        // as free; the allowlist must win and pin the current flagship.
        let available: Vec<&str> = vec!["labs-devstral-small-2512", "mistral-large-2512"];
        let auto = HashMap::from([(
            "mistral".to_string(),
            "labs-devstral-small-2512".to_string(),
        )]);
        assert_eq!(
            select_available_model("mistral", &available, &auto).as_deref(),
            Some("mistral-large-2512")
        );
    }

    #[test]
    fn select_available_model_zai_pins_free_flash_over_paid_catalog_default() {
        // GLM-4.7 (catalog's old default) is paid; the allowlist must pick the
        // free flash variant even though models.dev also knows it.
        let available: Vec<&str> = vec!["glm-4.7", "glm-4.7-flash"];
        assert_eq!(
            select_available_model("zai", &available, &HashMap::new()).as_deref(),
            Some("glm-4.7-flash")
        );
    }

    #[test]
    fn select_available_model_returns_none_when_no_safe_pick() {
        // Neither the models.dev pick nor the catalog default is on the live
        // list — must NOT fall back to an arbitrary (possibly paid) model.
        let available: Vec<&str> = vec!["m1", "m2"];
        assert_eq!(
            select_available_model("cloudflare", &available, &HashMap::new()),
            None
        );
        assert_eq!(
            select_available_model("cloudflare", &[], &HashMap::new()),
            None
        );
    }

    #[test]
    fn select_available_models_leads_with_single_pick_then_free_set() {
        // The full-list selector must keep the single-model pick first (so
        // the chain's effective model is unchanged when derived as `.first()`)
        // and then list every other known-free model the live list serves.
        let available: Vec<&str> = vec![
            "allam-2-7b",
            "llama-3.3-70b-versatile",
            "openai/gpt-oss-120b",
        ];
        let auto = HashMap::from([("groq".to_string(), "allam-2-7b".to_string())]);
        let modelsdev_free = vec![
            "allam-2-7b".to_string(),
            "llama-3.3-70b-versatile".to_string(),
        ];
        let list = select_available_models_from("groq", &available, &auto, &modelsdev_free);
        // Known-free allowlist wins the single pick (over the models.dev
        // pick allam-2-7b) and leads the full list.
        assert_eq!(
            list.first().map(String::as_str),
            Some("openai/gpt-oss-120b")
        );
        assert!(list.contains(&"llama-3.3-70b-versatile".to_string()));
        // `select_available_model` agrees with `.first()`.
        assert_eq!(
            select_available_model("groq", &available, &auto),
            list.first().cloned()
        );
    }

    #[test]
    fn select_available_models_never_includes_unlisted_models() {
        // A models.dev free id that the live list does NOT serve must not
        // appear; only live-confirmed candidates are returned.
        let available: Vec<&str> = vec!["openai/gpt-oss-120b"];
        let auto = HashMap::from([("groq".to_string(), "allam-2-7b".to_string())]);
        let modelsdev_free = vec!["allam-2-7b".to_string()];
        let list = select_available_models_from("groq", &available, &auto, &modelsdev_free);
        assert_eq!(list, vec!["openai/gpt-oss-120b".to_string()]);
    }

    #[test]
    fn select_available_models_returns_empty_without_safe_picks() {
        let available: Vec<&str> = vec!["m1", "m2"];
        assert!(
            select_available_models_from("cloudflare", &available, &HashMap::new(), &[]).is_empty()
        );
    }

    #[test]
    fn credit_based_providers_include_full_live_list() {
        // Mistral and SambaNova have credit-based free tiers where ALL models
        // are usable. The full list must include every live model, not just
        // the curated allowlist pick.
        let available: Vec<&str> = vec![
            "mistral-large-2512",
            "mistral-small-latest",
            "codestral-latest",
            "pixtral-12b",
        ];
        let list = select_available_models_from("mistral", &available, &HashMap::new(), &[]);
        // Allowlisted model is first.
        assert_eq!(list.first().map(String::as_str), Some("mistral-large-2512"));
        // All live models are present.
        assert_eq!(list.len(), 4);
        assert!(list.contains(&"mistral-small-latest".to_string()));
        assert!(list.contains(&"codestral-latest".to_string()));
        assert!(list.contains(&"pixtral-12b".to_string()));
    }

    #[test]
    fn non_credit_based_providers_only_show_curated_picks() {
        // Groq is rate-limited per model (not credit-based), so only the
        // allowlisted + models.dev picks appear — NOT every live model.
        let available: Vec<&str> = vec![
            "openai/gpt-oss-120b",
            "llama-3.3-70b-versatile",
            "llama-3.1-8b-instant",
        ];
        let auto = HashMap::from([("groq".to_string(), "llama-3.1-8b-instant".to_string())]);
        let list = select_available_models_from("groq", &available, &auto, &[]);
        // Only the allowlisted model and models.dev pick — not all 3.
        assert!(list.contains(&"openai/gpt-oss-120b".to_string()));
        assert!(!list.contains(&"llama-3.1-8b-instant".to_string()));
    }

    #[test]
    fn select_gemini_model_prefers_catalog_default_when_available() {
        // gemini-2.5-flash is the catalog default; it wins over the models.dev
        // pick (a gemma model) because Gemini free tier is quota-based.
        let available: Vec<&str> = vec!["gemini-2.5-pro", "gemini-2.5-flash", "gemma-3-12b-it"];
        let auto = HashMap::from([("google".to_string(), "gemma-3-12b-it".to_string())]);
        assert_eq!(
            select_gemini_model(&available, &auto).as_deref(),
            Some("gemini-2.5-flash")
        );
    }

    #[test]
    fn select_gemini_model_falls_back_to_modelsdev_pick() {
        let available: Vec<&str> = vec!["gemini-2.5-pro", "gemma-3-12b-it"];
        let auto = HashMap::from([("google".to_string(), "gemma-3-12b-it".to_string())]);
        assert_eq!(
            select_gemini_model(&available, &auto).as_deref(),
            Some("gemma-3-12b-it")
        );
    }

    #[test]
    fn select_gemini_model_never_picks_arbitrary_first_model() {
        let available: Vec<&str> = vec!["gemini-2.5-pro", "gemini-1.5-pro"];
        assert_eq!(select_gemini_model(&available, &HashMap::new()), None);
    }

    #[test]
    fn gemini_known_free_excludes_pro_and_specialty_models() {
        // The GEMINI_KNOWN_FREE list must exclude Pro, TTS, image, deep
        // research, and other paid-only models that the /v1beta/models API
        // returns alongside genuinely free models.
        assert!(GEMINI_KNOWN_FREE.contains(&"gemini-2.5-flash"));
        assert!(GEMINI_KNOWN_FREE.contains(&"gemini-3.7-flash"));
        assert!(GEMINI_KNOWN_FREE.contains(&"gemini-3.1-flash-lite"));
        assert!(GEMINI_KNOWN_FREE.contains(&"gemma-4-31b-it"));
        // Pro models — paid only since April 2026
        assert!(!GEMINI_KNOWN_FREE.contains(&"gemini-2.5-pro"));
        assert!(!GEMINI_KNOWN_FREE.contains(&"gemini-3.1-pro-preview"));
        // TTS / image / specialty — paid only
        assert!(!GEMINI_KNOWN_FREE.contains(&"gemini-2.5-flash-preview-tts"));
        assert!(!GEMINI_KNOWN_FREE.contains(&"gemini-3-pro-image"));
        assert!(!GEMINI_KNOWN_FREE.contains(&"deep-research-preview-04-2026"));
        assert!(!GEMINI_KNOWN_FREE.contains(&"gemini-robotics-er-2-preview"));
        assert!(!GEMINI_KNOWN_FREE.contains(&"lyria-3-pro-preview"));
    }

    #[test]
    fn nvidia_catalog_collects_only_preview_entries_and_marks_deprecated() {
        // `PREVIEW == "true"` is the "Free Endpoint" badge; a DEPRECATION
        // attribute marks the model as being retired.
        let payload = serde_json::json!({
            "results": [{
                "groupValue": "ENDPOINT",
                "resources": [
                    {"displayName": "gpt-oss-120b", "attributes": [{"key": "PREVIEW", "value": "true"}]},
                    {"displayName": "llama-3.3-70b-instruct", "attributes": [
                        {"key": "PREVIEW", "value": "true"},
                        {"key": "DEPRECATION", "value": "08/25/2026"}
                    ]},
                    {"displayName": "some-paid-model", "attributes": [{"key": "PREVIEW", "value": "false"}]},
                    {"displayName": "no-attributes"},
                    {"name": "name-only-model", "attributes": [{"key": "PREVIEW", "value": "true"}]}
                ]
            }]
        });
        assert_eq!(
            collect_nvidia_catalog_entries(&payload),
            vec![
                ("gpt-oss-120b".to_string(), false),
                ("llama-3.3-70b-instruct".to_string(), true),
                ("name-only-model".to_string(), false),
            ]
        );
        assert!(collect_nvidia_catalog_entries(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn nvidia_selection_excludes_deprecated_nonchat_and_unlisted() {
        // deprecated llama, non-chat riva, and a model absent from /v1/models
        // must all be dropped; only live, chat-capable, callable models remain.
        let entries = vec![
            ("gpt-oss-120b".to_string(), false),
            ("llama-3.3-70b-instruct".to_string(), true), // deprecated
            ("riva-translate-4b-instruct-v2".to_string(), false), // non-chat
            ("paligemma".to_string(), false),             // not in wire list
            ("nemotron-3.5-lightning-30b-a3b".to_string(), false),
            ("nemotron-3.5-lightning-30b-a3b".to_string(), false), // duplicate
        ];
        let wire_ids = vec![
            "openai/gpt-oss-120b",
            "meta/llama-3.3-70b-instruct",
            "nvidia/riva-translate-4b-instruct-v2",
            "nvidia/nemotron-3.5-lightning-30b-a3b",
        ];
        // gpt-oss-120b is NVIDIA_PREFERRED_FREE[0] → default pick; the rest
        // sort alphabetically; the duplicate is deduped.
        assert_eq!(
            select_nvidia_catalog_free_models(&entries, &wire_ids),
            vec![
                "openai/gpt-oss-120b".to_string(),
                "nvidia/nemotron-3.5-lightning-30b-a3b".to_string(),
            ]
        );
    }

    #[test]
    fn nvidia_selection_matches_dot_and_underscore_spellings() {
        // Catalog display names use `_` (v1_1) and `.` (3.5) where the wire
        // IDs use `.` — normalization must reconcile them.
        let entries = vec![
            ("nemotron-3.5-lightning-30b-a3b".to_string(), false),
            ("riva-translate-4b-instruct-v1_1".to_string(), false),
        ];
        let wire_ids = vec!["nvidia/nemotron-3.5-lightning-30b-a3b"];
        assert_eq!(
            select_nvidia_catalog_free_models(&entries, &wire_ids),
            vec!["nvidia/nemotron-3.5-lightning-30b-a3b".to_string()]
        );
        // riva is non-chat and dropped even though its wire ID exists.
        let wire_ids_all = vec![
            "nvidia/nemotron-3.5-lightning-30b-a3b",
            "nvidia/riva-translate-4b-instruct-v1.1",
        ];
        assert_eq!(
            select_nvidia_catalog_free_models(&entries, &wire_ids_all),
            vec!["nvidia/nemotron-3.5-lightning-30b-a3b".to_string()]
        );
    }

    #[test]
    fn nvidia_selection_prefers_known_default_then_alphabetical() {
        let entries = vec![
            ("muse-glimmer-30b".to_string(), false),
            ("gpt-oss-120b".to_string(), false),
            ("step-3.7-flash".to_string(), false),
        ];
        let wire_ids = vec![
            "nvidia/step-3.7-flash",
            "openai/gpt-oss-120b",
            "meta/muse-glimmer-30b",
        ];
        assert_eq!(
            select_nvidia_catalog_free_models(&entries, &wire_ids),
            vec![
                "openai/gpt-oss-120b".to_string(),
                "meta/muse-glimmer-30b".to_string(),
                "nvidia/step-3.7-flash".to_string(),
            ]
        );
    }

    #[test]
    #[ignore = "manual: hits live NVIDIA APIs"]
    fn nvidia_live_fetch_returns_current_free_chat_models() {
        let list = fetch_nvidia_catalog_free_models(None).expect("live fetch");
        eprintln!("live NVIDIA free chat models ({}): {:?}", list.len(), list);
        assert!(!list.is_empty());
    }

    #[test]
    fn nvidia_selection_returns_empty_when_nothing_callable() {
        let entries = vec![("some-model".to_string(), false)];
        assert!(select_nvidia_catalog_free_models(&entries, &[]).is_empty());
        // everything deprecated or non-chat → empty
        let entries = vec![
            ("llama-3.3-70b-instruct".to_string(), true),
            ("sparsedrive".to_string(), false),
        ];
        let wire_ids = vec!["meta/llama-3.3-70b-instruct", "nvidia/sparsedrive"];
        assert!(select_nvidia_catalog_free_models(&entries, &wire_ids).is_empty());
    }
}
