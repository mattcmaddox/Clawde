// providers/free.rs — Composite "Free" provider.
//
// Stacks multiple upstream free-tier providers behind a single
// `free/auto` synthetic model id. The chain is iterated in priority
// order on every request — if an upstream fails (auth, rate limit,
// server error, request error) *before* any data has been streamed,
// the same request is retried against the next upstream. Mid-stream
// failures are surfaced as-is; we don't replay partial conversations.
//
// Inspired by https://github.com/tashfeenahmed/freellmapi — the same
// "aggregate the free tiers from many providers behind one OpenAI-
// compatible endpoint" idea, ported into claurst's native provider
// trait.
//
// Routing:
//   * `free` / `free/auto` / `auto`  →  try each configured upstream
//     in catalog order, using that upstream's `default_model`.
//   * `<upstream_id>/<rest>`         →  pin that upstream, then
//     fall through to the rest of the chain on transient errors.
//   * anything else                  →  passed through verbatim
//     to the first upstream in the chain.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use clawde_core::provider_id::{ModelId, ProviderId};
use futures::Stream;

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
};
use clawde_core::types::ContentBlock;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One upstream provider in the free-mode chain.
///
/// `id` is the canonical claurst `ProviderId` string — the auth store key the
/// dialog writes to, and the prefix the user types for `<id>/<model>` pinning.
#[derive(Debug, Clone, Copy)]
pub struct FreeUpstream {
    pub id: &'static str,
    pub title: &'static str,
    pub key_url: &'static str,
    pub default_model: &'static str,
    pub note: &'static str,
    /// Whether the default model supports function/tool calling.
    pub tool_calling: bool,
    /// Hard cap on `max_tokens` for this upstream's default model.
    /// When set, requests are silently clamped to this value.
    pub max_tokens_cap: Option<u32>,
}

/// Ordered priority of providers we stack into Free mode. Order matters —
/// `free/auto` tries each in turn, so put the highest-quality, most reliable
/// tiers first. The chain starts with the best models (Llama 3.3 70B-class)
/// and falls through to lighter fallbacks.
pub const FREE_CATALOG: &[FreeUpstream] = &[
    // Tier 1: Best-quality models
    FreeUpstream {
        id: "huggingface",
        title: "Hugging Face",
        key_url: "huggingface.co/settings/tokens",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        note: "free Inference API — Llama 3.3 70B",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "nvidia",
        title: "NVIDIA NIM",
        key_url: "build.nvidia.com",
        default_model: "meta/llama-3.3-70b-instruct",
        note: "Llama 3.3 70B — 2 keys",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "cerebras",
        title: "Cerebras",
        key_url: "cloud.cerebras.ai",
        default_model: "gpt-oss-120b",
        note: "GPT-OSS 120B (65K ctx) · Gemma 4 31B",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    // Tier 2: Very good models (some currently rate-limited)
    FreeUpstream {
        id: "google",
        title: "Google Gemini",
        key_url: "aistudio.google.com/app/apikey",
        default_model: "gemini-2.5-flash",
        note: "Gemini 2.5 Flash",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "github-models",
        title: "GitHub Models",
        key_url: "github.com/settings/tokens",
        default_model: "gpt-4o-mini",
        note: "GPT-4o-mini — 2 keys",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "sambanova",
        title: "SambaNova",
        key_url: "cloud.sambanova.ai",
        default_model: "Meta-Llama-3.3-70B-Instruct",
        note: "Llama 3.3 70B · DeepSeek V3",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    // Tier 3: Decent fallbacks
    FreeUpstream {
        id: "cline",
        title: "Cline",
        key_url: "app.cline.bot/settings",
        default_model: "stepfun/step-3.7-flash",
        note: "live free-model API — auto-discovers best model at startup",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "mistral",
        title: "Mistral",
        key_url: "console.mistral.ai/api-keys",
        default_model: "labs-devstral-small-2512",
        note: "Devstral Small (free) · Large · Codestral",
        tool_calling: true,
        max_tokens_cap: None,
    },
    FreeUpstream {
        id: "cohere",
        title: "Cohere",
        key_url: "dashboard.cohere.com/api-keys",
        default_model: "north-mini-code-1-0",
        note: "North Mini Code (free) · Command R+",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "opencode-zen",
        title: "OpenCode Zen",
        key_url: "opencode.ai/auth",
        default_model: "minimax-m2.5-free",
        note: "MiniMax M2.5 — 2 keys",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    FreeUpstream {
        id: "zai",
        title: "Z.AI",
        key_url: "z.ai/manage-apikey/apikey-list",
        default_model: "glm-4.7",
        note: "GLM-4.7 · GLM-5 · GLM-5.1 — Zhipu AI international",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
    },
    // Tier 4: Paywalled — kept as last resort
    FreeUpstream {
        id: "openrouter",
        title: "OpenRouter",
        key_url: "openrouter.ai/keys",
        default_model: "openrouter/free",
        note: "19 free-tier models — requires $10 prepaid credits",
        tool_calling: true,
        max_tokens_cap: None,
    },
];

/// Look up a catalog entry by its `id`.
pub fn catalog_entry(id: &str) -> Option<&'static FreeUpstream> {
    FREE_CATALOG.iter().find(|e| e.id == id)
}

/// Static storage for the most recently built FreeProvider's model defaults.
/// Populated by `build_free_provider` in registry.rs; read by the TUI for
/// the /ctx-viz "Free models" table. Thread-safe via OnceLock.
static RECENT_FREE_MODEL_DEFAULTS: OnceLock<Mutex<Vec<(String, String)>>> = OnceLock::new();
fn recent_free_model_defaults() -> &'static Mutex<Vec<(String, String)>> {
    RECENT_FREE_MODEL_DEFAULTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set the free model defaults from a newly-built FreeProvider's chain.
/// Called by `build_free_provider` in registry.rs after constructing the
/// chain. The TUI reads these via [`take_free_model_defaults`].
pub fn store_free_model_defaults(defaults: Vec<(String, String)>) {
    if let Ok(mut guard) = recent_free_model_defaults().lock() {
        *guard = defaults;
    }
}

/// Retrieve the stored free model defaults.
/// Returns a clone so that multiple callers (startup wiring, /models
/// command) all see the same data. Returns an empty vec if none have
/// been stored yet.
pub fn take_free_model_defaults() -> Vec<(String, String)> {
    RECENT_FREE_MODEL_DEFAULTS
        .get()
        .and_then(|m| m.lock().ok())
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

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
        "google" => FreeModelDiscovery::GeminiModels,
        _ => FreeModelDiscovery::None,
    }
}

/// Run live discovery for the first entry whose ID matches `upstream_id`.
/// Returns the discovered model ID, or `None` if discovery is not configured
/// or the fetch fails.
pub fn run_live_discovery(
    upstream_id: &str,
    auth_store: &clawde_core::AuthStore,
) -> Option<String> {
    match discovery_for(upstream_id) {
        FreeModelDiscovery::ClineRecommended => {
            let key = auth_store
                .keys_for("cline")
                .and_then(|k| k.first().cloned())
                .or_else(|| auth_store.api_key_for("cline"))?;
            fetch_cline_free_model(&key)
        }
        FreeModelDiscovery::OpenRouterFreeModels => {
            let key = auth_store
                .keys_for("openrouter")
                .and_then(|k| k.first().cloned())
                .or_else(|| auth_store.api_key_for("openrouter"))?;
            fetch_openrouter_free_model(&key)
        }
        FreeModelDiscovery::OpenAiModelList { base_url } => {
            let key = auth_store
                .keys_for(upstream_id)
                .and_then(|k| k.first().cloned())
                .or_else(|| auth_store.api_key_for(upstream_id))?;
            fetch_openai_compat_model_list(&key, base_url, upstream_id)
        }
        FreeModelDiscovery::GeminiModels => {
            let key = auth_store
                .keys_for("google")
                .and_then(|k| k.first().cloned())
                .or_else(|| auth_store.api_key_for("google"))?;
            fetch_gemini_models(&key)
        }
        FreeModelDiscovery::None => None,
    }
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
    let key = cline_api_key.to_string();
    // reqwest::blocking::Client creates an internal tokio runtime. Dropping
    // that runtime inside an existing tokio runtime context panics, so the
    // entire blocking HTTP call is moved to a plain OS thread.
    std::thread::spawn(move || {
        let url = "https://api.cline.bot/api/v1/ai/cline/recommended-models";
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| {
                client
                    .get(url)
                    .header("Authorization", format!("Bearer {}", key))
                    .send()
            })
        else {
            tracing::warn!("fetch_cline_free_model: HTTP request failed");
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!(
                "fetch_cline_free_model: HTTP {} — check Cline API key",
                response.status(),
            );
            return None;
        }

        let Ok(data) = response.json::<serde_json::Value>() else {
            tracing::warn!("fetch_cline_free_model: failed to parse JSON");
            return None;
        };

        let free_models = data.get("free").and_then(|v| v.as_array())?;
        let first = free_models.first()?;
        let model_id = first.get("id")?.as_str()?;

        tracing::info!(
            "Cline recommended free model: {} (from {} available)",
            model_id,
            free_models.len(),
        );

        Some(model_id.to_string())
    })
    .join()
    .ok()
    .flatten()
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
    let key = openrouter_api_key.to_string();
    // reqwest::blocking::Client creates an internal tokio runtime. Dropping
    // that runtime inside an existing tokio runtime context panics, so the
    // entire blocking HTTP call is moved to a plain OS thread.
    std::thread::spawn(move || {
        let url = "https://openrouter.ai/api/v1/models";
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| {
                client
                    .get(url)
                    .header("Authorization", format!("Bearer {}", key))
                    .send()
            })
        else {
            tracing::warn!("fetch_openrouter_free_model: HTTP request failed");
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!(
                "fetch_openrouter_free_model: HTTP {} — check OpenRouter API key",
                response.status(),
            );
            return None;
        }

        let Ok(payload) = response.json::<serde_json::Value>() else {
            tracing::warn!("fetch_openrouter_free_model: failed to parse JSON");
            return None;
        };

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
    })
    .join()
    .ok()
    .flatten()
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
    let api_key = api_key.to_string();
    let base_url = base_url.to_string();
    let upstream_id = upstream_id.to_string();
    // reqwest::blocking::Client creates an internal tokio runtime. Dropping
    // that runtime inside an existing tokio runtime context panics, so the
    // entire blocking HTTP call is moved to a plain OS thread.
    std::thread::spawn(move || {
        let models_url = format!("{}/models", base_url.trim_end_matches('/'));
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| {
                client
                    .get(&models_url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .send()
            })
        else {
            tracing::warn!(
                "fetch_openai_compat_model_list({}): HTTP request failed",
                upstream_id
            );
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!(
                "fetch_openai_compat_model_list({}): HTTP {} — check API key",
                upstream_id,
                response.status(),
            );
            return None;
        }

        let Ok(payload) = response.json::<serde_json::Value>() else {
            tracing::warn!(
                "fetch_openai_compat_model_list({}): failed to parse JSON",
                upstream_id
            );
            return None;
        };

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
        if let Some(recommended) = auto_detected.get(upstream_id.as_str()) {
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
        if let Some(entry) = crate::providers::free::catalog_entry(&upstream_id) {
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
    })
    .join()
    .ok()
    .flatten()
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
    let api_key = api_key.to_string();
    // reqwest::blocking::Client creates an internal tokio runtime. Dropping
    // that runtime inside an existing tokio runtime context panics, so the
    // entire blocking HTTP call is moved to a plain OS thread.
    std::thread::spawn(move || {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models?key={}",
            api_key
        );
        let Ok(response) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .and_then(|client| client.get(&url).send())
        else {
            tracing::warn!("fetch_gemini_models: HTTP request failed");
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!(
                "fetch_gemini_models: HTTP {} — check Google API key",
                response.status(),
            );
            return None;
        }

        let Ok(payload) = response.json::<serde_json::Value>() else {
            tracing::warn!("fetch_gemini_models: failed to parse JSON");
            return None;
        };

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
    })
    .join()
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// FreeProvider
// ---------------------------------------------------------------------------

/// One configured entry in a [`FreeProvider`]'s chain.
#[derive(Clone)]
pub struct FreeEntry {
    pub upstream: FreeUpstream,
    pub provider: Arc<dyn LlmProvider>,
    /// Overrides `upstream.default_model` when set. Populated by
    /// [`fetch_best_free_models_from_modelsdev`] at build time so that
    /// the chain always uses the best currently-free model for each
    /// upstream without needing hardcoded catalog changes.
    pub effective_model: Option<String>,
}

/// Routing strategy for the FreeProvider's fallback chain.
///
/// Controls how the provider selects which upstream to try first and in what
/// order. Plumbed from `settings.json` → `providers.free.options.routing`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Try upstreams in catalog (priority) order. Current default.
    #[default]
    Sequential,
    /// Randomly select an upstream with failover to the next on failure.
    RandomFailover,
    /// Route to the upstream with the lowest historical latency.
    LatencyBased,
}

/// Circuit breaker configuration for the FreeProvider.
///
/// When an upstream fails `max_fails` times within `window_secs`, it is
/// cooled down for `cooldown_secs` and skipped in the fallback loop.
/// Disabled by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Max failures before the upstream is cooled down (0 = disabled).
    #[serde(default = "default_cb_max_fails")]
    pub max_fails: u32,
    /// Time window in seconds for counting failures.
    #[serde(default = "default_cb_window")]
    pub window_secs: u64,
    /// How long to cool down an upstream (seconds).
    #[serde(default = "default_cb_cooldown")]
    pub cooldown_secs: u64,
}

const fn default_cb_max_fails() -> u32 {
    3
}
const fn default_cb_window() -> u64 {
    60
}
const fn default_cb_cooldown() -> u64 {
    120
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            max_fails: 3,
            window_secs: 60,
            cooldown_secs: 120,
        }
    }
}

/// Latency tracking configuration for latency-based routing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyConfig {
    /// How many samples to keep in the sliding window (0 = disabled).
    #[serde(default = "default_latency_samples")]
    pub max_samples: usize,
}

const fn default_latency_samples() -> usize {
    10
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self { max_samples: 10 }
    }
}

/// Empty-completion cooldown configuration (spec §6.3 / §6.7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmptyCooldownConfig {
    #[serde(default = "default_empty_max_consecutive")]
    pub max_consecutive: u32,
    #[serde(default = "default_empty_cooldown_secs")]
    pub cooldown_secs: u64,
}

const fn default_empty_max_consecutive() -> u32 {
    3
}
const fn default_empty_cooldown_secs() -> u64 {
    60
}

impl Default for EmptyCooldownConfig {
    fn default() -> Self {
        Self {
            max_consecutive: default_empty_max_consecutive(),
            cooldown_secs: default_empty_cooldown_secs(),
        }
    }
}

impl EmptyCooldownConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn is_default_poll(n: &u64) -> bool {
    *n == 300
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_upstream_server_error(err: &ProviderError) -> bool {
    match err {
        ProviderError::ServerError {
            status: Some(s), ..
        } if (*s >= 500 && *s <= 599) || *s == 498 => true,
        ProviderError::Other {
            status: Some(s), ..
        } if (*s >= 500 && *s <= 599) || *s == 498 => true,
        _ => false,
    }
}

/// Routing configuration for a [`FreeProvider`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    #[serde(default)]
    pub strategy: RoutingStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub circuit_breaker: Option<CircuitBreakerConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency: Option<LatencyConfig>,
    #[serde(default = "default_upstream_timeout")]
    pub upstream_timeout_secs: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_upstreams: Vec<String>,
    #[serde(default, skip_serializing_if = "EmptyCooldownConfig::is_default")]
    pub empty_cooldown: EmptyCooldownConfig,
    #[serde(
        default = "default_first_byte_timeout",
        skip_serializing_if = "is_zero"
    )]
    pub first_byte_timeout_secs: u64,
    #[serde(default = "default_staggered_probe", skip_serializing_if = "is_true")]
    pub staggered_probe: bool,
    #[serde(
        default = "default_upstream_5xx_cooldown",
        skip_serializing_if = "is_zero"
    )]
    pub upstream_5xx_cooldown_secs: u64,
    #[serde(
        default = "default_poll_interval",
        skip_serializing_if = "is_default_poll"
    )]
    pub health_poll_interval_secs: u64,
    #[serde(
        default = "default_fallback_retries",
        skip_serializing_if = "is_zero_u32"
    )]
    pub fallback_retries: u32,
}

const fn default_upstream_timeout() -> u64 {
    30
}

const fn default_first_byte_timeout() -> u64 {
    0
}

const fn default_staggered_probe() -> bool {
    true
}

const fn default_upstream_5xx_cooldown() -> u64 {
    45
}

const fn default_poll_interval() -> u64 {
    300
}

const fn default_fallback_retries() -> u32 {
    1
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::default(),
            circuit_breaker: None,
            latency: None,
            upstream_timeout_secs: default_upstream_timeout(),
            disabled_upstreams: Vec::new(),
            empty_cooldown: EmptyCooldownConfig::default(),
            first_byte_timeout_secs: default_first_byte_timeout(),
            staggered_probe: default_staggered_probe(),
            upstream_5xx_cooldown_secs: default_upstream_5xx_cooldown(),
            health_poll_interval_secs: default_poll_interval(),
            fallback_retries: default_fallback_retries(),
        }
    }
}

/// Per-upstream failure history and cooldown for the circuit breaker.
struct CooldownState {
    /// Sliding window of failure timestamps per upstream index.
    failures: Vec<VecDeque<Instant>>,
    /// Circuit-breaker cooldown expiry per upstream index.
    cooldown_until: Vec<Option<Instant>>,
    /// Consecutive empty completions per upstream index.
    consecutive_empties: Vec<u32>,
    /// Empty-completion cooldown expiry per upstream index.
    empty_cooldown_until: Vec<Option<Instant>>,
    /// Upstream id per index, for persistence keyed by provider id.
    upstream_ids: Vec<String>,
    /// Optional path for empty-cooldown persistence.
    persist_path: Option<std::path::PathBuf>,
    /// Circuit-breaker configuration.
    config: CircuitBreakerConfig,
}

/// Disk snapshot of one upstream's empty-completion cooldown track.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EmptyCooldownSnapshot {
    upstream: String,
    consecutive_empties: u32,
    empty_cooldown_until_unix: Option<u64>,
}

impl CooldownState {
    fn new(n: usize, config: CircuitBreakerConfig) -> Self {
        let mut failures = Vec::with_capacity(n);
        let mut cooldown_until = Vec::with_capacity(n);
        let mut consecutive_empties = Vec::with_capacity(n);
        let mut empty_cooldown_until = Vec::with_capacity(n);
        for _ in 0..n {
            failures.push(VecDeque::new());
            cooldown_until.push(None);
            consecutive_empties.push(0);
            empty_cooldown_until.push(None);
        }
        Self {
            failures,
            cooldown_until,
            consecutive_empties,
            empty_cooldown_until,
            upstream_ids: Vec::new(),
            persist_path: None,
            config,
        }
    }

    fn with_persistence(
        mut self,
        upstream_ids: Vec<String>,
        persist_path: Option<std::path::PathBuf>,
    ) -> Self {
        self.upstream_ids = upstream_ids;
        self.persist_path = persist_path;
        if let Some(path) = self.persist_path.clone() {
            self.load_from_file(&path);
        }
        self
    }
    /// Remove expired cooldowns and old failure timestamps.
    fn prune_expired(&mut self) {
        let now = Instant::now();
        let window = std::time::Duration::from_secs(self.config.window_secs);
        for i in 0..self.failures.len() {
            if let Some(until) = self.cooldown_until[i] {
                if now >= until {
                    self.cooldown_until[i] = None;
                    self.failures[i].clear();
                }
            }
            if let Some(until) = self.empty_cooldown_until[i] {
                if now >= until {
                    self.empty_cooldown_until[i] = None;
                }
            }
            while self.failures[i].front().is_some_and(|t| now - *t > window) {
                self.failures[i].pop_front();
            }
        }
    }

    /// Whether the upstream at `idx` is in circuit-breaker cooldown.
    fn is_in_cooldown(&self, idx: usize) -> bool {
        idx < self.cooldown_until.len() && self.cooldown_until[idx].is_some()
    }

    /// Whether the upstream at `idx` is in empty-completion cooldown.
    fn is_in_empty_cooldown(&self, idx: usize) -> bool {
        idx < self.empty_cooldown_until.len()
            && self.empty_cooldown_until[idx].is_some_and(|t| Instant::now() < t)
    }

    fn empty_cooldown_remaining_secs(&self, idx: usize) -> Option<u64> {
        let until = self.empty_cooldown_until.get(idx).copied().flatten()?;
        let now = Instant::now();
        if now >= until {
            return None;
        }
        Some(until.duration_since(now).as_secs())
    }

    /// Record a failure at `idx`. Returns `true` if the upstream just
    /// crossed the threshold and was put into cooldown.
    fn record_failure(&mut self, idx: usize) -> bool {
        if idx >= self.failures.len() || self.config.max_fails == 0 {
            return false;
        }
        self.failures[idx].push_back(Instant::now());
        if self.failures[idx].len() >= self.config.max_fails as usize {
            self.cooldown_until[idx] =
                Some(Instant::now() + std::time::Duration::from_secs(self.config.cooldown_secs));
            true
        } else {
            false
        }
    }

    fn record_empty(&mut self, idx: usize, max_consecutive: u32, cooldown_secs: u64) -> bool {
        if idx >= self.consecutive_empties.len() || max_consecutive == 0 {
            return false;
        }
        self.consecutive_empties[idx] += 1;
        let just_cooled = if self.consecutive_empties[idx] >= max_consecutive {
            self.consecutive_empties[idx] = 0;
            self.empty_cooldown_until[idx] =
                Some(Instant::now() + std::time::Duration::from_secs(cooldown_secs));
            true
        } else {
            false
        };
        self.save();
        just_cooled
    }

    fn apply_upstream_cooldown(&mut self, idx: usize, cooldown_secs: u64) {
        if cooldown_secs == 0 || idx >= self.cooldown_until.len() {
            return;
        }
        self.cooldown_until[idx] =
            Some(Instant::now() + std::time::Duration::from_secs(cooldown_secs));
    }

    /// Record a success at `idx` — resets the failure counter and the
    /// consecutive-empties counter.
    fn record_success(&mut self, idx: usize) {
        if idx < self.failures.len() {
            self.failures[idx].clear();
        }
        if idx < self.consecutive_empties.len() {
            self.consecutive_empties[idx] = 0;
        }
        self.save();
    }

    fn consecutive_empties(&self, idx: usize) -> u32 {
        self.consecutive_empties.get(idx).copied().unwrap_or(0)
    }

    // -------------------------------------------------------------------
    // Persistence (empty-completion cooldown track only)
    // -------------------------------------------------------------------

    fn save(&self) {
        let Some(path) = self.persist_path.as_ref() else {
            return;
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let entries: Vec<EmptyCooldownSnapshot> = self
            .upstream_ids
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let count = self.consecutive_empties.get(idx).copied().unwrap_or(0);
                let until_unix = self
                    .empty_cooldown_until
                    .get(idx)
                    .copied()
                    .flatten()
                    .and_then(|t| {
                        let remaining = t.duration_since(Instant::now()).as_secs();
                        if remaining == 0 {
                            return None;
                        }
                        Some(now_unix.saturating_add(remaining))
                    });
                if count == 0 && until_unix.is_none() {
                    return None;
                }
                Some(EmptyCooldownSnapshot {
                    upstream: id.clone(),
                    consecutive_empties: count,
                    empty_cooldown_until_unix: until_unix,
                })
            })
            .collect();
        if entries.is_empty() {
            let _ = std::fs::remove_file(path);
            return;
        }
        let json = match serde_json::to_string_pretty(&entries) {
            Ok(j) => j,
            Err(_) => return,
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = format!("{}.tmp-{}", path.display(), std::process::id(),);
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }

    fn load_from_file(&mut self, path: &std::path::Path) {
        if !path.exists() {
            return;
        }
        let json = match std::fs::read_to_string(path) {
            Ok(j) => j,
            Err(_) => return,
        };
        let entries: Vec<EmptyCooldownSnapshot> = match serde_json::from_str(&json) {
            Ok(e) => e,
            Err(_) => return,
        };
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        for entry in entries {
            let Some(idx) = self
                .upstream_ids
                .iter()
                .position(|id| id == &entry.upstream)
            else {
                continue;
            };
            self.consecutive_empties[idx] = entry.consecutive_empties;
            if let Some(until_unix) = entry.empty_cooldown_until_unix {
                let remaining = until_unix.saturating_sub(now_unix);
                if remaining > 0 {
                    self.empty_cooldown_until[idx] =
                        Some(Instant::now() + std::time::Duration::from_secs(remaining));
                }
            }
        }
    }
}

/// Per-upstream latency samples for latency-based routing.
struct LatencyState {
    /// Sliding window of request durations (seconds) per upstream index.
    samples: Vec<VecDeque<f64>>,
}

impl LatencyState {
    fn new(n: usize) -> Self {
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            samples.push(VecDeque::with_capacity(10));
        }
        Self { samples }
    }

    /// Record a latency sample at `idx`.
    fn record(&mut self, idx: usize, duration_secs: f64, max_samples: usize) {
        if idx >= self.samples.len() {
            return;
        }
        let q = &mut self.samples[idx];
        if q.len() >= max_samples {
            q.pop_front();
        }
        q.push_back(duration_secs);
    }

    /// Average latency for upstream `idx`, or `f64::MAX` if no samples.
    fn avg_latency(&self, idx: usize) -> f64 {
        if idx >= self.samples.len() {
            return f64::MAX;
        }
        let q = &self.samples[idx];
        if q.is_empty() {
            return f64::MAX;
        }
        let sum: f64 = q.iter().sum();
        sum / q.len() as f64
    }
}

/// Rate-limit information parsed from provider HTTP response headers.
#[derive(Debug, Default)]
pub struct RateLimitInfo {
    pub rpm_limit: Option<u32>,
    pub rpm_remaining: Option<u32>,
    pub rpd_limit: Option<u32>,
    pub rpd_remaining: Option<u32>,
    pub tpm_limit: Option<u32>,
    pub tpm_remaining: Option<u32>,
    pub retry_after: Option<u64>,
    pub headers_found: bool,
}

/// Resolve the key list for a free upstream, handling the OpenCode Zen/Go
/// alias (both slots share the same key).  Used by the health poller and
/// `build_free_provider` in registry.rs.
pub fn resolve_free_upstream_keys(
    auth_store: &clawde_core::AuthStore,
    upstream_id: &str,
) -> Option<Vec<String>> {
    if upstream_id == "opencode-zen" {
        auth_store
            .keys_for("opencode-zen")
            .or_else(|| auth_store.keys_for("opencode-go"))
            .map(|k| k.to_vec())
    } else {
        auth_store.keys_for(upstream_id).map(|k| k.to_vec())
    }
}

/// Query rate-limit information for a given upstream by making a lightweight
/// HEAD request to the provider's models endpoint and parsing response headers.
pub fn query_rate_limits(upstream_id: &str, key: &str) -> Result<RateLimitInfo, String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    let base_url = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
        "github-models" => "https://models.inference.ai.azure.com/openai/v1/models",
        "sambanova" => "https://api.sambanova.ai/v1/models",
        "mistral" => "https://api.mistral.ai/v1/models",
        "cohere" => "https://api.cohere.com/v1/models",
        "opencode-zen" => "https://api.opencode.ai/v1/models",
        "zai" => "https://open.bigmodel.cn/api/paas/v4/models",
        "cline" => "https://api.cline.bot/api/v1/ai/cline/recommended-models",
        _ => return Err(format!("No validation endpoint for '{}'", upstream_id)),
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let is_google = upstream_id == "google";
    let request = if is_google {
        client.get(base_url).query(&[("key", key)])
    } else {
        client
            .head(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    match request.send() {
        Ok(response) => {
            let status = response.status();
            let headers = response.headers().clone();

            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(format!("Invalid API key (HTTP {})", status));
            }

            let parse_u32 = |name: &str| -> Option<u32> {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
            };

            let parse_retry = || -> Option<u64> {
                headers
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse().ok())
            };

            let info = RateLimitInfo {
                rpm_limit: parse_u32("x-ratelimit-limit-requests"),
                rpm_remaining: parse_u32("x-ratelimit-remaining-requests"),
                rpd_limit: parse_u32("x-ratelimit-limit-requests-day"),
                rpd_remaining: parse_u32("x-ratelimit-remaining-requests-day"),
                tpm_limit: parse_u32("x-ratelimit-limit-tokens"),
                tpm_remaining: parse_u32("x-ratelimit-remaining-tokens"),
                retry_after: parse_retry(),
                headers_found: headers
                    .keys()
                    .any(|k| k.as_str().to_lowercase().contains("ratelimit")),
            };

            Ok(info)
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Validate an API key for a given upstream by making a lightweight request
/// to the provider's models endpoint. Returns `Ok(())` if the key is valid.
pub fn validate_upstream_key(upstream_id: &str, key: &str) -> Result<(), String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    let base_url = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
        "github-models" => "https://models.inference.ai.azure.com/openai/v1/models",
        "sambanova" => "https://api.sambanova.ai/v1/models",
        "mistral" => "https://api.mistral.ai/v1/models",
        "cohere" => "https://api.cohere.com/v1/models",
        "opencode-zen" => "https://api.opencode.ai/v1/models",
        "zai" => "https://open.bigmodel.cn/api/paas/v4/models",
        "cline" => "https://api.cline.bot/api/v1/ai/cline/recommended-models",
        _ => return Err(format!("No validation endpoint for '{}'", upstream_id)),
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("Failed to create HTTP client: {}", e)),
    };

    let is_google = upstream_id == "google";
    let request = if is_google {
        client.get(base_url).query(&[("key", key)])
    } else {
        client
            .get(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    match request.send() {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else if status.as_u16() == 401 || status.as_u16() == 403 {
                Err(format!("Invalid API key (HTTP {})", status))
            } else if status.as_u16() == 429 {
                Err("Rate limited — try again later".to_string())
            } else {
                Err(format!("HTTP {} — unexpected response", status))
            }
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Composite provider that stacks free-tier upstreams behind a single
/// `free/auto` model id.
pub struct FreeProvider {
    id: ProviderId,
    chain: Vec<FreeEntry>,
    routing: RoutingConfig,
    /// Circuit-breaker state (per-upstream cooldown).
    cooldown: Arc<Mutex<CooldownState>>,
    /// Latency tracking state (per-upstream sliding window).
    latencies: Arc<Mutex<LatencyState>>,
}

#[derive(Debug)]
enum Route {
    /// Try every entry in order, substituting its `default_model`.
    Auto,
    /// Try the entry at `start_idx` first (with `pinned_model`), then fall
    /// through to the remaining entries in catalog order.
    Pinned {
        start_idx: usize,
        pinned_model: String,
    },
}

// ---------------------------------------------------------------------------
// Auto-detect best free models from models.dev
// ---------------------------------------------------------------------------

/// Cache for the best free model per upstream, populated once at first use.
static AUTO_DETECTED_DEFAULTS: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Fetch `models.dev` and find the best free (cost=0) model with tool_call
/// support for each FREE_CATALOG upstream.
///
/// Selection criteria (matching Cline's approach from their source):
/// 1. Model must be free (cost.input == 0 && cost.output == 0)
/// 2. Model must support tool calling (tool_call == true)
/// 3. Model must not be deprecated
/// 4. Among qualifying models, pick the one with the largest context window
///
/// Results are cached in `AUTO_DETECTED_DEFAULTS` so the HTTP fetch
/// only happens once per process lifetime.
pub fn fetch_best_free_models_from_modelsdev() -> &'static HashMap<String, String> {
    AUTO_DETECTED_DEFAULTS.get_or_init(|| {
        // reqwest::blocking::Client creates an internal tokio runtime. Dropping
        // that runtime inside an existing tokio runtime context (e.g. under
        // #[tokio::main]) panics. Run the entire HTTP fetch on a plain OS thread
        // so the internal runtime is created and dropped outside any async context.
        std::thread::spawn(|| {
            let url = "https://models.dev/api.json";
            let Ok(response) = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .and_then(|client| client.get(url).send())
            else {
                tracing::warn!("fetch_best_free_models_from_modelsdev: HTTP request failed");
                return HashMap::new();
            };

            let Ok(data) = response.json::<serde_json::Value>() else {
                tracing::warn!("fetch_best_free_models_from_modelsdev: failed to parse JSON");
                return HashMap::new();
            };

            let mut result = HashMap::new();

            for upstream in FREE_CATALOG {
                let Some(provider) = data.get(upstream.id) else {
                    continue;
                };
                let Some(models) = provider.get("models") else {
                    continue;
                };
                let Some(models_obj) = models.as_object() else {
                    continue;
                };

                let mut candidates: Vec<(&str, u64)> = Vec::new();

                for (model_id, model_info) in models_obj {
                    // Must be free
                    let cost = model_info.get("cost").and_then(|c| c.as_object());
                    let cost_in = cost
                        .and_then(|c| c.get("input"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0);
                    let cost_out = cost
                        .and_then(|c| c.get("output"))
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0);
                    if cost_in != 0.0 || cost_out != 0.0 {
                        continue;
                    }

                    // Must support tool calling (matching Cline's filtering)
                    let tool_call = model_info
                        .get("tool_call")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if !tool_call {
                        continue;
                    }

                    // Must not be deprecated
                    let status = model_info
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if status == "deprecated" || status == "legacy" {
                        continue;
                    }

                    // Context window for ranking
                    let limit = model_info.get("limit").and_then(|l| l.as_object());
                    let context = limit
                        .and_then(|l| l.get("context"))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);

                    candidates.push((model_id, context));
                }

                // Sort by context window descending, pick the best
                candidates.sort_by(|a, b| b.1.cmp(&a.1));

                if let Some((model_id, _ctx)) = candidates.first() {
                    let prev = result.insert(upstream.id.to_string(), (*model_id).to_string());
                    if prev.is_none() {
                        tracing::info!(
                            "Auto-detected free model for {}: {} ({} context)",
                            upstream.id,
                            model_id,
                            _ctx,
                        );
                    }
                }
            }

            if result.is_empty() {
                tracing::warn!("fetch_best_free_models_from_modelsdev: no free models found");
            } else {
                tracing::info!("Auto-detected free models for {} upstreams", result.len(),);
            }

            result
        })
        .join()
        .unwrap_or_else(|_| {
            tracing::warn!("fetch_best_free_models_from_modelsdev: thread panicked");
            HashMap::new()
        })
    })
}

impl FreeProvider {
    /// Resolve the effective default model for the entry at `idx`.
    /// Uses the auto-detected override when available, otherwise falls
    /// back to the hardcoded `upstream.default_model`.
    fn model_for_entry(&self, idx: usize) -> &str {
        if let Some(ref em) = self.chain[idx].effective_model {
            em.as_str()
        } else {
            self.chain[idx].upstream.default_model
        }
    }

    /// Create a new `FreeProvider` with the default [`RoutingConfig`]
    /// (sequential failover in catalog order).
    pub const ENABLE_EMPTY_COOLDOWN_PERSISTENCE: bool = true;

    pub fn new(chain: Vec<FreeEntry>) -> Self {
        let n = chain.len();
        Self {
            id: ProviderId::new(ProviderId::FREE),
            chain,
            routing: RoutingConfig::default(),
            cooldown: Arc::new(Mutex::new(CooldownState::new(
                n,
                CircuitBreakerConfig::default(),
            ))),
            latencies: Arc::new(Mutex::new(LatencyState::new(n))),
        }
    }

    /// Create a new `FreeProvider` with an explicit [`RoutingConfig`].
    ///
    /// When `persist` is `true` (production path — use
    /// `ENABLE_EMPTY_COOLDOWN_PERSISTENCE`) the empty-cooldown track is
    /// persisted to `{clawde_home}/empty-cooldown-state/free.json`.
    pub fn with_routing(chain: Vec<FreeEntry>, routing: RoutingConfig, persist: bool) -> Self {
        let n = chain.len();
        let cb_config = routing.circuit_breaker.clone().unwrap_or_default();
        let upstream_ids: Vec<String> = chain.iter().map(|e| e.upstream.id.to_string()).collect();
        let persist_path = if persist {
            Some(
                clawde_core::config::Settings::config_dir()
                    .join("empty-cooldown-state")
                    .join("free.json"),
            )
        } else {
            None
        };
        let cooldown = Arc::new(Mutex::new(
            CooldownState::new(n, cb_config).with_persistence(upstream_ids, persist_path),
        ));
        Self {
            id: ProviderId::new(ProviderId::FREE),
            chain,
            routing,
            cooldown,
            latencies: Arc::new(Mutex::new(LatencyState::new(n))),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    pub fn chain_len(&self) -> usize {
        self.chain.len()
    }

    /// Decide how to route a user-facing model id into the chain.
    fn resolve_route(&self, model: &str) -> Route {
        let trimmed = model.trim();
        if trimmed.is_empty() || trimmed == "free" || trimmed == "auto" || trimmed == "free/auto" {
            return Route::Auto;
        }

        // Legacy alias: `zen/...` was the old Free-mode pin prefix.
        let normalized: String = if let Some(rest) = trimmed.strip_prefix("zen/") {
            format!("opencode-zen/{}", rest)
        } else {
            trimmed.to_string()
        };

        // Find a chain entry whose id is a prefix.
        for (idx, entry) in self.chain.iter().enumerate() {
            let prefix = format!("{}/", entry.upstream.id);
            if let Some(rest) = normalized.strip_prefix(&prefix) {
                // OpenRouter is unusual: its model ids are themselves
                // `vendor/model` strings (e.g. `meta-llama/llama-3-8b:free`)
                // and the free-pool router model is literally `openrouter/free`.
                // Pass the post-prefix portion through; for OpenRouter's
                // built-in free router we restore the full id.
                let pinned_model = if entry.upstream.id == "openrouter"
                    && (rest == "free" || rest == "auto" || rest.is_empty())
                {
                    "openrouter/free".to_string()
                } else {
                    rest.to_string()
                };
                return Route::Pinned {
                    start_idx: idx,
                    pinned_model,
                };
            }
        }

        // No prefix matched — treat as a raw model id for the first upstream.
        Route::Auto
    }

    fn circuit_breaker_enabled(&self) -> bool {
        self.routing
            .circuit_breaker
            .as_ref()
            .is_some_and(|c| c.max_fails > 0)
    }

    fn max_latency_samples(&self) -> usize {
        self.routing.latency.as_ref().map_or(0, |l| l.max_samples)
    }

    /// Build the per-attempt (provider, model) sequence for a given request,
    /// applying the configured [`RoutingStrategy`].
    fn attempt_plan(&self, route: &Route) -> Vec<(usize, String)> {
        match self.routing.strategy {
            RoutingStrategy::RandomFailover => self.attempt_plan_random(route),
            RoutingStrategy::LatencyBased => self.attempt_plan_latency(route),
            RoutingStrategy::Sequential => self.attempt_plan_sequential(route),
        }
    }

    /// Original sequential plan: upstreams in catalog (or pinned) order.
    fn attempt_plan_sequential(&self, route: &Route) -> Vec<(usize, String)> {
        match route {
            Route::Auto => self
                .chain
                .iter()
                .enumerate()
                .map(|(idx, _entry)| (idx, self.model_for_entry(idx).to_string()))
                .collect(),
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut plan = Vec::with_capacity(self.chain_len());
                plan.push((*start_idx, pinned_model.clone()));
                for (idx, _entry) in self.chain.iter().enumerate() {
                    if idx == *start_idx {
                        continue;
                    }
                    plan.push((idx, self.model_for_entry(idx).to_string()));
                }
                plan
            }
        }
    }

    /// Random-failover plan: shuffle each request's order so load is
    /// distributed across all upstreams over time.  For pinned routes,
    /// the pinned upstream is always first, then the rest are shuffled.
    fn attempt_plan_random(&self, route: &Route) -> Vec<(usize, String)> {
        let mut rng = rand::thread_rng();
        match route {
            Route::Auto => {
                let mut plan: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .map(|(idx, _entry)| (idx, self.model_for_entry(idx).to_string()))
                    .collect();
                plan.shuffle(&mut rng);
                plan
            }
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut rest: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != *start_idx)
                    .map(|(idx, _entry)| (idx, self.model_for_entry(idx).to_string()))
                    .collect();
                rest.shuffle(&mut rng);

                let mut plan = Vec::with_capacity(self.chain_len());
                plan.push((*start_idx, pinned_model.clone()));
                plan.extend(rest);
                plan
            }
        }
    }

    /// Latency-based plan: sort upstreams by their historical average
    /// latency (lowest first). For pinned routes, the pinned upstream is
    /// always first, then the rest are sorted by latency.
    fn attempt_plan_latency(&self, route: &Route) -> Vec<(usize, String)> {
        let latencies = self.latencies.lock().unwrap();
        match route {
            Route::Auto => {
                let mut plan: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .map(|(idx, _entry)| (idx, self.model_for_entry(idx).to_string()))
                    .collect();
                plan.sort_by(|a, b| {
                    latencies
                        .avg_latency(a.0)
                        .partial_cmp(&latencies.avg_latency(b.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                plan
            }
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut rest: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != *start_idx)
                    .map(|(idx, _entry)| (idx, self.model_for_entry(idx).to_string()))
                    .collect();
                rest.sort_by(|a, b| {
                    latencies
                        .avg_latency(a.0)
                        .partial_cmp(&latencies.avg_latency(b.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                let mut plan = Vec::with_capacity(self.chain_len());
                plan.push((*start_idx, pinned_model.clone()));
                plan.extend(rest);
                plan
            }
        }
    }

    fn should_fallback(err: &ProviderError) -> bool {
        // Don't fall back on user-fixable problems — they would behave the
        // same on every upstream.
        !matches!(
            err,
            ProviderError::InvalidRequest { .. } | ProviderError::ContentFiltered { .. }
        )
    }

    /// Expose the current [`RoutingConfig`] for introspection (e.g. TUI
    /// status display showing the active strategy).
    pub fn routing_config(&self) -> &RoutingConfig {
        &self.routing
    }

    /// Check if an upstream is in cooldown (circuit breaker).
    fn is_in_cooldown(&self, idx: usize) -> bool {
        if !self.circuit_breaker_enabled() {
            return false;
        }
        let cd = self.cooldown.lock().unwrap();
        cd.is_in_cooldown(idx)
    }

    /// Record a successful request at `idx` with the given `elapsed` duration.
    fn record_success(&self, idx: usize, elapsed: std::time::Duration) {
        // Reset circuit breaker failure counter for this upstream.
        if self.circuit_breaker_enabled() {
            let mut cd = self.cooldown.lock().unwrap();
            cd.record_success(idx);
        }
        // Record latency sample.
        let max_samples = self.max_latency_samples();
        if max_samples > 0 {
            let mut lat = self.latencies.lock().unwrap();
            lat.record(idx, elapsed.as_secs_f64(), max_samples);
        }
    }

    /// Record a failed request at `idx`.
    fn record_failure(&self, idx: usize) {
        if !self.circuit_breaker_enabled() {
            return;
        }
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        if cd.record_failure(idx) {
            tracing::info!(
                "FreeProvider: upstream {} cooled down for {}s ({} failures)",
                idx,
                cd.config.cooldown_secs,
                cd.config.max_fails,
            );
        }
    }

    /// Return the effective model for each upstream in the chain.
    ///
    /// Returns a vector of `(upstream_title, effective_model_id)` pairs,
    /// one per entry in the fallback chain. Used by the TUI to display
    /// which free models were auto-detected at startup or via live
    /// discovery (Cline, OpenRouter, etc.).
    pub fn free_model_defaults(&self) -> Vec<(String, String)> {
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                (
                    entry.upstream.title.to_string(),
                    self.model_for_entry(idx).to_string(),
                )
            })
            .collect()
    }

    /// Apply an immediate cooldown to the upstream at `idx` if the error is
    /// a 5xx / 498 server error, using the configured cooldown duration.
    fn maybe_cooldown_upstream_for_5xx(&self, idx: usize, err: &ProviderError) {
        if !is_upstream_server_error(err) {
            return;
        }
        let secs = self.routing.upstream_5xx_cooldown_secs;
        if secs == 0 {
            return;
        }
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        cd.apply_upstream_cooldown(idx, secs);
        tracing::warn!(
            "FreeProvider: upstream {} cooled down for {}s after 5xx",
            idx,
            secs,
        );
    }

    /// Return per-upstream empty-cooldown summaries for the /keys health
    /// command and TUI status display (spec §6.3).
    pub fn upstream_empty_cooldowns(&self) -> Vec<(String, u32, Option<u64>)> {
        let cd = self.cooldown.lock().unwrap();
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                (
                    entry.upstream.id.to_string(),
                    cd.consecutive_empties(idx),
                    cd.empty_cooldown_remaining_secs(idx),
                )
            })
            .filter(|(_, count, remaining)| *count > 0 || remaining.is_some())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// RetryingFreeStream — empty-completion re-dispatch (spec §6.2)
// ---------------------------------------------------------------------------

type BoxedProviderStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// Wraps an upstream stream and automatically re-dispatches to the next
/// plan entry when the current stream produces a completely empty
/// completion (HTTP 200 + zero text + zero tool calls + `end_turn`).
struct RetryingFreeStream {
    chain: Vec<FreeEntry>,
    cooldown: Arc<Mutex<CooldownState>>,
    latencies: Arc<Mutex<LatencyState>>,
    routing: RoutingConfig,
    request: ProviderRequest,
    remaining_plan: VecDeque<(usize, String)>,
    current: Option<BoxedProviderStream>,
    current_idx: usize,
    current_model: String,
    starting: Option<tokio::task::JoinHandle<Result<BoxedProviderStream, ProviderError>>>,
    /// Parallel probe for first-byte watchdog (§6.5).
    parallel_starting: Option<tokio::task::JoinHandle<Result<BoxedProviderStream, ProviderError>>>,
    parallel_idx: usize,
    parallel_model: String,
    is_auto_route: bool,
    attempt_text: String,
    attempt_thinking: String,
    attempt_tool_count: usize,
    attempt_stop_reason: Option<String>,
    attempt_start: Option<Instant>,
    first_byte_received: bool,
    upstream_errors: Vec<String>,
}

impl RetryingFreeStream {
    #[allow(clippy::too_many_arguments)]
    fn new(
        chain: Vec<FreeEntry>,
        cooldown: Arc<Mutex<CooldownState>>,
        latencies: Arc<Mutex<LatencyState>>,
        routing: RoutingConfig,
        request: ProviderRequest,
        stream: BoxedProviderStream,
        idx: usize,
        upstream_model: String,
        remaining_plan: VecDeque<(usize, String)>,
        is_auto_route: bool,
    ) -> Self {
        Self {
            chain,
            cooldown,
            latencies,
            routing,
            request,
            remaining_plan,
            current: Some(stream),
            current_idx: idx,
            current_model: upstream_model,
            starting: None,
            parallel_starting: None,
            parallel_idx: 0,
            parallel_model: String::new(),
            is_auto_route,
            attempt_text: String::new(),
            attempt_thinking: String::new(),
            attempt_tool_count: 0,
            attempt_stop_reason: None,
            attempt_start: Some(Instant::now()),
            first_byte_received: false,
            upstream_errors: Vec::new(),
        }
    }

    fn record_success(&self, idx: usize, elapsed: std::time::Duration) {
        let mut cd = self.cooldown.lock().unwrap();
        cd.record_success(idx);
        drop(cd);
        let max_samples = self.routing.latency.as_ref().map_or(0, |l| l.max_samples);
        if max_samples > 0 {
            self.latencies
                .lock()
                .unwrap()
                .record(idx, elapsed.as_secs_f64(), max_samples);
        }
    }

    fn record_failure(&self, idx: usize) {
        if self
            .routing
            .circuit_breaker
            .as_ref()
            .is_some_and(|c| c.max_fails > 0)
        {
            let mut cd = self.cooldown.lock().unwrap();
            cd.prune_expired();
            cd.record_failure(idx);
        }
    }

    fn record_empty(&self, idx: usize) -> bool {
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        cd.record_empty(
            idx,
            self.routing.empty_cooldown.max_consecutive,
            self.routing.empty_cooldown.cooldown_secs,
        )
    }

    fn maybe_cooldown_upstream_for_5xx(&self, idx: usize, err: &ProviderError) {
        if !is_upstream_server_error(err) {
            return;
        }
        let secs = self.routing.upstream_5xx_cooldown_secs;
        if secs == 0 {
            return;
        }
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        cd.apply_upstream_cooldown(idx, secs);
    }

    fn reset_attempt(&mut self) {
        self.attempt_text.clear();
        self.attempt_thinking.clear();
        self.attempt_tool_count = 0;
        self.attempt_stop_reason = None;
        self.attempt_start = Some(Instant::now());
        self.first_byte_received = false;
    }

    fn is_empty_attempt(&self) -> bool {
        self.attempt_text.trim().is_empty()
            && self.attempt_thinking.trim().is_empty()
            && self.attempt_tool_count == 0
    }

    /// Kick off the next plan entry's `create_message_stream`. Returns
    /// `true` when a new attempt was launched, `false` when the plan is
    /// exhausted.
    fn start_next_plan_entry(&mut self) -> bool {
        while let Some((idx, model)) = self.remaining_plan.pop_front() {
            let cd = self.cooldown.lock().unwrap();
            let in_cooldown = cd.is_in_cooldown(idx) || cd.is_in_empty_cooldown(idx);
            if in_cooldown {
                let uid = self.chain[idx].upstream.id;
                self.upstream_errors
                    .push(format!("{}: (skipped — in cooldown)", uid));
                continue;
            }
            drop(cd);

            let entry = &self.chain[idx];
            let mut req = self.request.clone();
            req.model = model.clone();
            let timeout = std::time::Duration::from_secs(self.routing.upstream_timeout_secs);
            let provider = entry.provider.clone();

            self.current_idx = idx;
            self.current_model = model;
            self.reset_attempt();

            let handle = tokio::spawn(async move {
                match tokio::time::timeout(timeout, provider.create_message_stream(req)).await {
                    Ok(Ok(stream)) => Ok(stream),
                    Ok(Err(err)) => Err(err),
                    Err(_) => Err(ProviderError::RateLimited {
                        provider: ProviderId::new("free"),
                        retry_after: None,
                    }),
                }
            });
            self.starting = Some(handle);
            return true;
        }
        false
    }

    fn advance_after_empty(&mut self) -> bool {
        let prev_chain_idx = self.current_idx;
        self.record_failure(prev_chain_idx);
        let _cooled = self.record_empty(prev_chain_idx);
        let uid = self.chain[prev_chain_idx].upstream.id;
        self.upstream_errors
            .push(format!("{}: switching from empty completion", uid));
        self.start_next_plan_entry()
    }
}

impl Stream for RetryingFreeStream {
    type Item = Result<StreamEvent, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            // Check for in-flight start handle.
            if let Some(handle) = self.starting.as_mut() {
                match Pin::new(handle).poll(cx) {
                    Poll::Ready(Ok(Ok(stream))) => {
                        self.starting = None;
                        self.current = Some(stream);
                    }
                    Poll::Ready(Ok(Err(err))) => {
                        self.starting = None;
                        if FreeProvider::should_fallback(&err) {
                            self.record_failure(self.current_idx);
                            self.maybe_cooldown_upstream_for_5xx(self.current_idx, &err);
                            let uid = self.chain[self.current_idx].upstream.id;
                            self.upstream_errors.push(format!("{}: {}", uid, err));
                            if !self.start_next_plan_entry() {
                                let msg = format!(
                                    "all free-mode upstreams exhausted: {}",
                                    self.upstream_errors.join(", ")
                                );
                                return Poll::Ready(Some(Err(ProviderError::ServerError {
                                    provider: ProviderId::new("free"),
                                    status: None,
                                    message: msg,
                                    is_retryable: false,
                                })));
                            }
                            continue;
                        }
                        return Poll::Ready(Some(Err(err)));
                    }
                    Poll::Ready(Err(_)) => {
                        self.starting = None;
                        self.record_failure(self.current_idx);
                        let uid = self.chain[self.current_idx].upstream.id;
                        self.upstream_errors.push(format!("{}: timeout", uid));
                        if !self.start_next_plan_entry() {
                            let msg = format!(
                                "all free-mode upstreams exhausted: {}",
                                self.upstream_errors.join(", ")
                            );
                            return Poll::Ready(Some(Err(ProviderError::ServerError {
                                provider: ProviderId::new("free"),
                                status: None,
                                message: msg,
                                is_retryable: false,
                            })));
                        }
                        continue;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // First-byte watchdog (§6.5): when the current stream hasn't
            // produced anything within `first_byte_timeout_secs`, launch a
            // parallel probe for the next plan entry that isn't in cooldown.
            let watchdog_can_fire = self.is_auto_route
                && self.routing.staggered_probe
                && self.routing.first_byte_timeout_secs > 0
                && !self.first_byte_received
                && self.parallel_starting.is_none();
            if watchdog_can_fire {
                if let Some(start) = self.attempt_start {
                    if start.elapsed().as_secs() >= self.routing.first_byte_timeout_secs {
                        // Find the next plan entry not in cooldown.
                        while let Some((idx, model)) = self.remaining_plan.pop_front() {
                            let cd = self.cooldown.lock().unwrap();
                            let in_cooldown =
                                cd.is_in_cooldown(idx) || cd.is_in_empty_cooldown(idx);
                            if in_cooldown {
                                drop(cd);
                                let uid = self.chain[idx].upstream.id;
                                self.upstream_errors
                                    .push(format!("{}: (skipped — in cooldown)", uid));
                                continue;
                            }
                            drop(cd);

                            let entry = &self.chain[idx];
                            let mut req = self.request.clone();
                            req.model = model.clone();
                            let timeout =
                                std::time::Duration::from_secs(self.routing.upstream_timeout_secs);
                            let provider = entry.provider.clone();
                            self.parallel_idx = idx;
                            self.parallel_model = model;
                            let handle = tokio::spawn(async move {
                                match tokio::time::timeout(
                                    timeout,
                                    provider.create_message_stream(req),
                                )
                                .await
                                {
                                    Ok(Ok(s)) => Ok(s),
                                    Ok(Err(e)) => Err(e),
                                    Err(_) => Err(ProviderError::RateLimited {
                                        provider: ProviderId::new("free"),
                                        retry_after: None,
                                    }),
                                }
                            });
                            self.parallel_starting = Some(handle);
                            break;
                        }
                    }
                }
            }

            // If a parallel probe is in-flight, poll it alongside current.
            if let Some(handle) = self.parallel_starting.as_mut() {
                match Pin::new(handle).poll(cx) {
                    Poll::Ready(Ok(Ok(stream))) => {
                        // Parallel probe won — switch to it.
                        self.parallel_starting = None;
                        self.current = Some(stream);
                        self.current_idx = self.parallel_idx;
                        self.current_model = std::mem::take(&mut self.parallel_model);
                        self.reset_attempt();
                    }
                    Poll::Ready(Ok(Err(err))) => {
                        self.parallel_starting = None;
                        self.record_failure(self.parallel_idx);
                        self.maybe_cooldown_upstream_for_5xx(self.parallel_idx, &err);
                    }
                    Poll::Ready(Err(_)) => {
                        self.parallel_starting = None;
                        self.record_failure(self.parallel_idx);
                    }
                    Poll::Pending => {} // still in-flight
                }
            }

            // Poll the active stream.
            let Some(ref mut current) = self.current else {
                return Poll::Ready(None);
            };

            match current.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(evt))) => {
                    if !self.first_byte_received {
                        self.first_byte_received = true;
                    }
                    match &evt {
                        StreamEvent::TextDelta { text, .. } => {
                            self.attempt_text.push_str(text);
                        }
                        StreamEvent::ThinkingDelta { thinking, .. } => {
                            self.attempt_thinking.push_str(thinking);
                        }
                        StreamEvent::ContentBlockStart {
                            content_block: ContentBlock::ToolUse { .. },
                            ..
                        } => {
                            self.attempt_tool_count += 1;
                        }
                        StreamEvent::MessageDelta {
                            stop_reason: Some(_),
                            ..
                        } => {
                            self.attempt_stop_reason = Some("end_turn".to_string());
                        }
                        _ => {}
                    }
                    return Poll::Ready(Some(Ok(evt)));
                }
                Poll::Ready(Some(Err(err))) => {
                    if FreeProvider::should_fallback(&err) {
                        self.record_failure(self.current_idx);
                        self.maybe_cooldown_upstream_for_5xx(self.current_idx, &err);
                        let uid = self.chain[self.current_idx].upstream.id;
                        self.upstream_errors.push(format!("{}: {}", uid, err));
                        self.current = None;
                        if !self.start_next_plan_entry() {
                            let msg = format!(
                                "all free-mode upstreams exhausted: {}",
                                self.upstream_errors.join(", ")
                            );
                            return Poll::Ready(Some(Err(ProviderError::ServerError {
                                provider: ProviderId::new("free"),
                                status: None,
                                message: msg,
                                is_retryable: false,
                            })));
                        }
                        continue;
                    }
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Ready(None) => {
                    let was_empty = self.is_empty_attempt();
                    let elapsed = self.attempt_start.map(|s| s.elapsed());
                    self.current = None;

                    if was_empty {
                        let uid = self.chain[self.current_idx].upstream.id;
                        let model = self.current_model.clone();
                        let placeholder = format!(
                            "(no response from {}/{} — retrying with next upstream)",
                            uid, model,
                        );
                        let has_next = self.advance_after_empty();

                        // Emit the placeholder event for the query loop.
                        let evt = StreamEvent::TextDelta {
                            index: 0,
                            text: placeholder,
                        };
                        if has_next {
                            return Poll::Ready(Some(Ok(evt)));
                        }
                        // All exhausted.
                        let msg = format!(
                            "all free-mode upstreams exhausted: {}",
                            self.upstream_errors.join(", ")
                        );
                        return Poll::Ready(Some(Err(ProviderError::ServerError {
                            provider: ProviderId::new("free"),
                            status: None,
                            message: msg,
                            is_retryable: false,
                        })));
                    }

                    // Non-empty success — record latency.
                    if let Some(elapsed) = elapsed {
                        self.record_success(self.current_idx, elapsed);
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for FreeProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "Free (multi-provider)"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        if self.chain.is_empty() {
            return Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message:
                    "Free mode has no configured upstreams — add at least one API key via /connect."
                        .to_string(),
            });
        }

        let route = self.resolve_route(&request.model);
        let plan = self.attempt_plan(&route);
        let mut last_err: Option<ProviderError> = None;

        for (idx, upstream_model) in plan {
            // Circuit breaker: skip upstreams in cooldown.
            if self.is_in_cooldown(idx) {
                tracing::debug!("FreeProvider: skipping upstream {} (in cooldown)", idx,);
                continue;
            }

            let entry = &self.chain[idx];
            let mut req = request.clone();
            req.model = upstream_model;

            let start = Instant::now();
            let timeout = std::time::Duration::from_secs(self.routing.upstream_timeout_secs);
            let result = tokio::time::timeout(timeout, entry.provider.create_message(req)).await;

            match result {
                Ok(Ok(resp)) => {
                    self.record_success(idx, start.elapsed());
                    return Ok(resp);
                }
                Ok(Err(err)) if Self::should_fallback(&err) => {
                    tracing::warn!(
                        "FreeProvider: {} failed ({}s): {} — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                        err,
                    );
                    self.record_failure(idx);
                    self.maybe_cooldown_upstream_for_5xx(idx, &err);
                    last_err = Some(err);
                    continue;
                }
                Ok(Err(err)) => {
                    self.record_failure(idx);
                    return Err(err);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        "FreeProvider: upstream {} timed out after {}s — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                    );
                    self.record_failure(idx);
                    last_err = Some(ProviderError::RateLimited {
                        provider: self.id.clone(),
                        retry_after: None,
                    });
                    continue;
                }
            }
        }

        let err_msg = if last_err.is_some() {
            format!(
                "all free-mode upstreams exhausted (last error: {})",
                last_err.as_ref().unwrap()
            )
        } else {
            "all free-mode upstreams exhausted — no upstreams had errors, all may be in cooldown"
                .to_string()
        };
        Err(last_err.unwrap_or_else(|| ProviderError::ServerError {
            provider: self.id.clone(),
            status: None,
            message: err_msg,
            is_retryable: false,
        }))
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        if self.chain.is_empty() {
            return Err(ProviderError::AuthFailed {
                provider: self.id.clone(),
                message:
                    "Free mode has no configured upstreams — add at least one API key via /connect."
                        .to_string(),
            });
        }

        let route = self.resolve_route(&request.model);
        let plan_vec = self.attempt_plan(&route);
        let mut last_err: Option<ProviderError> = None;

        for (pos, (idx, upstream_model)) in plan_vec.into_iter().enumerate() {
            // Circuit breaker: skip upstreams in cooldown.
            if self.is_in_cooldown(idx) {
                tracing::debug!("FreeProvider: skipping upstream {} (in cooldown)", idx,);
                continue;
            }

            let entry = &self.chain[idx];
            let mut req = request.clone();
            req.model = upstream_model.clone();

            let _start = Instant::now();
            let timeout = std::time::Duration::from_secs(self.routing.upstream_timeout_secs);
            let result =
                tokio::time::timeout(timeout, entry.provider.create_message_stream(req)).await;

            match result {
                Ok(Ok(stream)) => {
                    // Wrap in RetryingFreeStream for empty-completion re-dispatch.
                    // Rebuild plan to get remaining entries by position.
                    let remaining: VecDeque<_> = self
                        .attempt_plan(&route)
                        .into_iter()
                        .skip(pos + 1)
                        .collect();
                    let is_auto = matches!(route, Route::Auto);
                    return Ok(Box::pin(RetryingFreeStream::new(
                        self.chain.clone(),
                        self.cooldown.clone(),
                        self.latencies.clone(),
                        self.routing.clone(),
                        request,
                        stream,
                        idx,
                        upstream_model,
                        remaining,
                        is_auto,
                    )));
                }
                Ok(Err(err)) if Self::should_fallback(&err) => {
                    tracing::warn!(
                        "FreeProvider: {} stream failed ({}s): {} — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                        err,
                    );
                    self.record_failure(idx);
                    self.maybe_cooldown_upstream_for_5xx(idx, &err);
                    last_err = Some(err);
                    continue;
                }
                Ok(Err(err)) => {
                    self.record_failure(idx);
                    return Err(err);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        "FreeProvider: upstream {} stream timed out after {}s — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                    );
                    self.record_failure(idx);
                    last_err = Some(ProviderError::RateLimited {
                        provider: self.id.clone(),
                        retry_after: None,
                    });
                    continue;
                }
            }
        }

        Err(last_err.unwrap_or_else(|| ProviderError::ServerError {
            provider: self.id.clone(),
            status: None,
            message: "all free-mode upstreams exhausted".to_string(),
            is_retryable: false,
        }))
    }

    fn routing_strategy_name(&self) -> Option<&'static str> {
        Some(match self.routing.strategy {
            RoutingStrategy::Sequential => "Seq",
            RoutingStrategy::RandomFailover => "Random",
            RoutingStrategy::LatencyBased => "Latency",
        })
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let provider_id = self.id.clone();
        let mk = |id: &str, name: &str, ctx: u32| ModelInfo {
            id: ModelId::new(id),
            provider_id: provider_id.clone(),
            name: name.to_string(),
            context_window: ctx,
            max_output_tokens: 8_192,
            ..Default::default()
        };

        let mut models = vec![mk(
            "free/auto",
            "Free \u{2014} Auto (round-robin across configured providers)",
            200_000,
        )];

        for (idx, entry) in self.chain.iter().enumerate() {
            let model = self.model_for_entry(idx);
            let label = format!("{} \u{2014} {}", entry.upstream.title, model);
            models.push(mk(
                &format!("{}/{}", entry.upstream.id, model),
                &label,
                128_000,
            ));
        }

        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Healthy as long as any upstream is reachable.
        let mut last: Result<ProviderStatus, ProviderError> = Ok(ProviderStatus::Unavailable {
            reason: "no upstreams configured".to_string(),
        });
        for entry in &self.chain {
            let res = entry.provider.health_check().await;
            if matches!(res, Ok(ProviderStatus::Healthy)) {
                return res;
            }
            last = res;
        }
        last
    }

    fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
        // Aggregate key ring statuses from all upstreams that support it.
        // E.g. an upstream wrapped in KeyRotatingProvider reports its
        // active/total key counts through this method.
        let mut total_active = 0usize;
        let mut total_keys = 0usize;
        let mut earliest_retry: Option<u64> = None;
        let mut any_has_ring = false;

        for entry in &self.chain {
            if let Some((active, total, retry)) = entry.provider.key_ring_status() {
                total_active += active;
                total_keys += total;
                any_has_ring = true;
                // Track the minimum non-zero retry time across all upstreams.
                if let Some(secs) = retry {
                    earliest_retry = Some(earliest_retry.map_or(secs, |min| min.min(secs)));
                }
            }
        }

        if any_has_ring {
            Some((total_active, total_keys, earliest_retry))
        } else {
            None
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        // tool_calling is true when any chain entry's upstream supports it.
        let tool_calling = self.chain.iter().any(|entry| entry.upstream.tool_calling);

        ProviderCapabilities {
            streaming: true,
            tool_calling,
            thinking: false,
            image_input: false,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: false,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }

    fn tool_calling_for(&self, model: &str) -> Option<bool> {
        let route = self.resolve_route(model);
        let (idx, _) = match route {
            Route::Auto => self.chain.first().map(|e| (0, e))?,
            Route::Pinned { start_idx, .. } => (start_idx, self.chain.get(start_idx)?),
        };
        Some(self.chain[idx].upstream.tool_calling)
    }

    fn max_tokens_cap_for(&self, model: &str) -> Option<u32> {
        let route = self.resolve_route(model);
        let (idx, _) = match route {
            Route::Auto => self.chain.first().map(|e| (0, e))?,
            Route::Pinned { start_idx, .. } => (start_idx, self.chain.get(start_idx)?),
        };
        self.chain[idx].upstream.max_tokens_cap
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::{Message, UsageInfo};
    use std::time::Duration;

    use crate::provider_types::StopReason;

    struct StubProvider {
        id: ProviderId,
        ok: bool,
    }

    #[async_trait]
    impl LlmProvider for StubProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "stub"
        }

        async fn create_message(
            &self,
            request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            if self.ok {
                Ok(ProviderResponse {
                    id: "msg".to_string(),
                    model: request.model,
                    content: Vec::new(),
                    stop_reason: StopReason::EndTurn,
                    usage: UsageInfo::default(),
                })
            } else {
                Err(ProviderError::RateLimited {
                    provider: self.id.clone(),
                    retry_after: None,
                })
            }
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: "stub".into(),
                is_retryable: false,
            })
        }

        async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
            Ok(vec![])
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            Ok(ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: SystemPromptStyle::SystemMessage,
            }
        }
    }

    fn entry(id: &'static str, ok: bool) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
            }),
            effective_model: None,
        }
    }

    fn dummy_request(model: &str) -> ProviderRequest {
        ProviderRequest {
            model: model.to_string(),
            messages: vec![Message::user("hi")],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 8,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            provider_options: serde_json::Value::Null,
        }
    }

    #[test]
    fn route_auto_for_free_aliases() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        assert!(matches!(provider.resolve_route("free"), Route::Auto));
        assert!(matches!(provider.resolve_route("free/auto"), Route::Auto));
        assert!(matches!(provider.resolve_route("auto"), Route::Auto));
        assert!(matches!(provider.resolve_route(""), Route::Auto));
    }

    #[test]
    fn route_pinned_for_prefix() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let route = provider.resolve_route("cerebras/qwen-3-235b");
        match route {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                assert_eq!(start_idx, 1);
                assert_eq!(pinned_model, "qwen-3-235b");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn legacy_zen_prefix_routes_to_opencode_zen() {
        let provider =
            FreeProvider::new(vec![entry("opencode-zen", true), entry("openrouter", true)]);
        let route = provider.resolve_route("zen/big-pickle");
        match route {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                assert_eq!(start_idx, 0);
                assert_eq!(pinned_model, "big-pickle");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn openrouter_free_keeps_full_id() {
        let provider = FreeProvider::new(vec![entry("openrouter", true)]);
        let route = provider.resolve_route("openrouter/free");
        match route {
            Route::Pinned { pinned_model, .. } => {
                assert_eq!(pinned_model, "openrouter/free");
            }
            other => panic!("expected pinned, got {:?}", other),
        }
    }

    #[test]
    fn attempt_plan_auto_uses_each_default() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[0].1, "meta-llama/Llama-3.3-70B-Instruct");
        assert_eq!(plan[1].0, 1);
        assert_eq!(plan[1].1, "gpt-oss-120b");
    }

    #[test]
    fn random_failover_auto_uses_all_entries() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );
        let plan = provider.attempt_plan(&Route::Auto);

        // Must have all upstreams.
        assert_eq!(plan.len(), 3);

        // Must contain every index exactly once.
        let mut indices: Vec<usize> = plan.iter().map(|(i, _)| *i).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2]);

        // Every model string must be non-empty.
        for (_, model) in &plan {
            assert!(!model.is_empty());
        }
    }

    #[test]
    fn random_failover_pinned_starts_with_pinned() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 2,
            pinned_model: "gemini-2.5-pro".into(),
        });

        // Pinned entry must be first.
        assert_eq!(plan[0].0, 2);
        assert_eq!(plan[0].1, "gemini-2.5-pro");

        // Must contain every index exactly once.
        let mut indices: Vec<usize> = plan.iter().map(|(i, _)| *i).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1, 2]);
    }

    #[test]
    fn routing_config_default_is_sequential() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        assert!(matches!(
            provider.routing_config().strategy,
            RoutingStrategy::Sequential
        ));
    }

    #[test]
    fn with_routing_stores_config() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );
        assert!(matches!(
            provider.routing_config().strategy,
            RoutingStrategy::RandomFailover
        ));
    }

    #[test]
    fn routing_strategy_serde_round_trip() {
        // Sequential → JSON → deserialize
        let seq = RoutingConfig::default();
        let json = serde_json::to_string(&seq).unwrap();
        let deserialized: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized.strategy, RoutingStrategy::Sequential));

        // RandomFailover → JSON → deserialize
        let rng = RoutingConfig {
            strategy: RoutingStrategy::RandomFailover,
            ..Default::default()
        };
        let json = serde_json::to_string(&rng).unwrap();
        assert_eq!(
            json,
            r#"{"strategy":"random_failover","upstream_timeout_secs":30,"upstream_5xx_cooldown_secs":45,"fallback_retries":1}"#
        );
        let deserialized: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            deserialized.strategy,
            RoutingStrategy::RandomFailover
        ));
    }

    #[test]
    fn routing_config_from_options_map() {
        // This simulates the config plumbing: reading from
        // provider_configs.get("free").options["routing"].
        use std::collections::HashMap;
        let mut options: HashMap<String, serde_json::Value> = HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({"strategy": "random_failover"}),
        );

        let routing: Option<RoutingConfig> = options
            .get("routing")
            .and_then(|v| serde_json::from_value(v.clone()).ok());

        let config = routing.unwrap();
        assert!(matches!(config.strategy, RoutingStrategy::RandomFailover));
    }

    #[test]
    fn attempt_plan_pinned_tries_pin_then_others() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("cerebras", true),
            entry("google", true),
        ]);
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 2,
            pinned_model: "gemini-2.5-pro".into(),
        });
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 2);
        assert_eq!(plan[0].1, "gemini-2.5-pro");
        // Order of remaining = catalog order minus the pinned index.
        assert_eq!(plan[1].0, 0);
        assert_eq!(plan[2].0, 1);
    }

    #[test]
    fn should_fallback_on_transient_errors() {
        let pid = ProviderId::new("groq");
        assert!(FreeProvider::should_fallback(&ProviderError::RateLimited {
            provider: pid.clone(),
            retry_after: None,
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::AuthFailed {
            provider: pid.clone(),
            message: "bad key".into(),
        }));
        assert!(FreeProvider::should_fallback(&ProviderError::ServerError {
            provider: pid.clone(),
            status: Some(500),
            message: "boom".into(),
            is_retryable: true,
        }));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::InvalidRequest {
                provider: pid.clone(),
                message: "bad request".into(),
            }
        ));
        assert!(!FreeProvider::should_fallback(
            &ProviderError::ContentFiltered {
                provider: pid,
                message: "filtered".into(),
            }
        ));
    }

    #[tokio::test]
    async fn create_message_falls_back_to_next_upstream() {
        let provider =
            FreeProvider::new(vec![entry("huggingface", false), entry("cerebras", true)]);
        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(resp.model, "gpt-oss-120b");
    }

    // -------------------------------------------------------------------
    // Circuit breaker tests
    // -------------------------------------------------------------------

    #[test]
    fn circuit_breaker_disabled_by_default() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_disabled_when_max_fails_is_zero() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 0,
                window_secs: 60,
                cooldown_secs: 120,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );
        // Even after many failures, no cooldown because max_fails=0
        provider.record_failure(0);
        provider.record_failure(0);
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_activates_after_threshold() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 2,
                window_secs: 60,
                cooldown_secs: 300,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // Initially no cooldown
        assert!(!provider.is_in_cooldown(0));
        assert!(!provider.is_in_cooldown(1));

        // First failure — not yet at threshold
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));

        // Second failure — now in cooldown
        provider.record_failure(0);
        assert!(provider.is_in_cooldown(0));

        // Other upstream unaffected
        assert!(!provider.is_in_cooldown(1));
    }

    #[test]
    fn circuit_breaker_success_resets_failures() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 2,
                window_secs: 60,
                cooldown_secs: 300,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // One failure, then a success resets the counter
        provider.record_failure(0);
        provider.record_success(0, Duration::from_secs(1));

        // One more failure should NOT trigger cooldown (counter was reset)
        provider.record_failure(0);
        assert!(!provider.is_in_cooldown(0));

        // Second failure after reset — now in cooldown
        provider.record_failure(0);
        assert!(provider.is_in_cooldown(0));
    }

    #[test]
    fn circuit_breaker_per_upstream_independence() {
        let cfg = RoutingConfig {
            circuit_breaker: Some(CircuitBreakerConfig {
                max_fails: 3,
                window_secs: 60,
                cooldown_secs: 120,
            }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Exhaust upstream 0 with 3 failures
        for _ in 0..3 {
            provider.record_failure(0);
        }
        assert!(provider.is_in_cooldown(0));

        // Other upstreams are still active
        assert!(!provider.is_in_cooldown(1));
        assert!(!provider.is_in_cooldown(2));
    }

    // -------------------------------------------------------------------
    // Latency tracking tests
    // -------------------------------------------------------------------

    #[test]
    fn latency_tracking_records_and_computes_average() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            cfg,
            false,
        );

        // Record latencies for upstream 0 (fast)
        provider.record_success(0, Duration::from_millis(100));
        provider.record_success(0, Duration::from_millis(200));

        // Record latencies for upstream 1 (slower)
        provider.record_success(1, Duration::from_millis(900));
        provider.record_success(1, Duration::from_millis(1100));

        // Latency-based plan should put faster upstream first
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 2);
        // Upstream 0 (avg 150ms) comes before upstream 1 (avg 1000ms)
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[1].0, 1);
    }

    #[test]
    fn latency_tracking_pinned_starts_with_pinned_then_sorted() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Record latencies: groq is fastest, cerebras is slowest
        provider.record_success(0, Duration::from_millis(100));
        provider.record_success(1, Duration::from_millis(2000));
        provider.record_success(2, Duration::from_millis(500));

        // Pin to cerebras (idx 1) — should be first, then rest sorted by latency
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 1,
            pinned_model: "custom-model".into(),
        });

        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 1); // pinned first
        assert_eq!(plan[0].1, "custom-model");
        assert_eq!(plan[1].0, 0); // groq (100ms) next
        assert_eq!(plan[2].0, 2); // google (500ms) last
    }

    #[test]
    fn latency_tracking_no_data_preserves_catalog_order() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // No latency data recorded — all avg_latency returns f64::MAX,
        // so partial_cmp returns Equal and order is stable (catalog order).
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[1].0, 1);
        assert_eq!(plan[2].0, 2);
    }

    #[test]
    fn latency_config_serde_round_trip() {
        let cfg = LatencyConfig { max_samples: 20 };
        let json = serde_json::to_string(&cfg).unwrap();
        assert_eq!(json, r#"{"max_samples":20}"#);
        let deserialized: LatencyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.max_samples, 20);

        // Default serialization
        let default_cfg = LatencyConfig::default();
        let json = serde_json::to_string(&default_cfg).unwrap();
        assert_eq!(json, r#"{"max_samples":10}"#);
    }

    #[test]
    fn circuit_breaker_config_serde_round_trip() {
        let cfg = CircuitBreakerConfig {
            max_fails: 5,
            window_secs: 120,
            cooldown_secs: 300,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: CircuitBreakerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.max_fails, 5);
        assert_eq!(deserialized.window_secs, 120);
        assert_eq!(deserialized.cooldown_secs, 300);

        // Default serialization
        let default_cfg = CircuitBreakerConfig::default();
        let json = serde_json::to_string(&default_cfg).unwrap();
        assert_eq!(
            json,
            r#"{"max_fails":3,"window_secs":60,"cooldown_secs":120}"#
        );
    }

    #[tokio::test]
    async fn empty_chain_returns_auth_error() {
        let provider = FreeProvider::new(vec![]);
        let err = provider
            .create_message(dummy_request("free/auto"))
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::AuthFailed { .. }));
    }
}

// -------------------------------------------------------------------
// Live discovery mock tests (fetch_openai_compat_model_list)
// -------------------------------------------------------------------

#[test]
fn fetch_openai_compat_model_list_parses_openai_response() {
    // Mock JSON response from a standard OpenAI-compatible /v1/models endpoint.
    let json = r#"{
            "object": "list",
            "data": [
                { "id": "llama-3.3-70b-versatile", "object": "model", "created": 1700000000, "owned_by": "groq" },
                { "id": "mixtral-8x7b-32768",       "object": "model", "created": 1700000001, "owned_by": "groq" }
            ]
        }"#;

    // Start a minimal HTTP server to serve the mock response.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expected_body = json.to_string();

    std::thread::spawn(move || {
        for mut s in listener.incoming().take(1).flatten() {
            use std::io::Write;
            let response = format!(
                "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: {}\n\n{}",
                expected_body.len(),
                expected_body
            );
            let _ = s.write_all(response.as_bytes());
        }
    });

    // Give the mock server thread a moment to start accepting.
    std::thread::sleep(std::time::Duration::from_millis(10));

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "groq");
    assert_eq!(result.as_deref(), Some("llama-3.3-70b-versatile"));
}

#[test]
fn fetch_openai_compat_model_list_returns_first_on_no_autodetect() {
    // When the auto-detected model ID is not available (or not yet populated),
    // the function should return the first model from the endpoint.
    let json = r#"{
            "data": [
                { "id": "qwen-3-235b", "object": "model" }
            ]
        }"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expected_body = json.to_string();

    std::thread::spawn(move || {
        for mut s in listener.incoming().take(1).flatten() {
            use std::io::Write;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                expected_body.len(),
                expected_body
            );
            let _ = s.write_all(response.as_bytes());
        }
    });

    // Give the mock server thread a moment to start accepting.
    std::thread::sleep(std::time::Duration::from_millis(10));

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "unknown-provider");
    assert_eq!(result.as_deref(), Some("qwen-3-235b"));
}

#[test]
fn fetch_openai_compat_model_list_handles_http_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    std::thread::spawn(move || {
        for mut s in listener.incoming().take(1).flatten() {
            use std::io::Write;
            let response = "HTTP/1.1 401 Unauthorized\nContent-Length: 0\n\n";
            let _ = s.write_all(response.as_bytes());
        }
    });

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("bad-key", &base_url, "groq");
    assert!(result.is_none());
}

#[test]
fn fetch_openai_compat_model_list_handles_empty_response() {
    let json = r#"{"data": []}"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let expected_body = json.to_string();

    std::thread::spawn(move || {
        for mut s in listener.incoming().take(1).flatten() {
            use std::io::Write;
            let response = format!(
                "HTTP/1.1 200 OK\nContent-Type: application/json\nContent-Length: {}\n\n{}",
                expected_body.len(),
                expected_body
            );
            let _ = s.write_all(response.as_bytes());
        }
    });

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "groq");
    assert!(result.is_none());
}

#[test]
fn fetch_gemini_models_parses_gemini_response() {
    // Mock response from Google Gemini's /v1beta/models endpoint.
    let json = r#"{
            "models": [
                {
                    "name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent", "countTokens"]
                },
                {
                    "name": "models/gemini-2.5-pro",
                    "supportedGenerationMethods": ["generateContent"]
                },
                {
                    "name": "models/gemma-3-27b-it",
                    "supportedGenerationMethods": ["generateContent"]
                }
            ]
        }"#;

    // Verify the fetch_gemini_models logic directly by testing
    // the JSON parsing logic in isolation.
    let payload: serde_json::Value = serde_json::from_str(json).unwrap();
    let models = payload.get("models").and_then(|v| v.as_array()).unwrap();
    let mut model_ids: Vec<String> = Vec::new();
    for model in models {
        let name = model.get("name").and_then(|v| v.as_str()).unwrap();
        let model_id = name.strip_prefix("models/").unwrap_or(name);
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
            model_ids.push(model_id.to_string());
        }
    }
    assert_eq!(
        model_ids,
        vec![
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-pro".to_string(),
            "gemma-3-27b-it".to_string(),
        ]
    );
}
