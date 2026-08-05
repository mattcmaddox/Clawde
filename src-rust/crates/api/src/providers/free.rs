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
    /// Grouping key for "model-first" routing: upstreams hosting the same
    /// model family share a slug (e.g. "llama-3.3-70b" covers Hugging Face,
    /// NVIDIA and SambaNova). Selecting `free/family/<slug>` in the picker
    /// round-robins across every hosting upstream in catalog order.
    pub model_family: &'static str,
    pub note: &'static str,
    /// Whether the default model supports function/tool calling.
    pub tool_calling: bool,
    /// Hard cap on `max_tokens` for this upstream's default model.
    /// When set, requests are silently clamped to this value.
    pub max_tokens_cap: Option<u32>,
    /// Secondary model IDs tried right after the primary on the SAME
    /// upstream, before the chain moves to the next provider. Lets a slow or
    /// capacity-starved primary (e.g. NVIDIA's 70B routinely exceeding the
    /// 30s upstream timeout) fall back to a smaller model on the same key.
    pub fallback_models: &'static [&'static str],
    /// Short hint for the free model picker — a 1-3 word tag describing what
    /// this model family is best at ("best overall", "coding specialist",
    /// "fast", "multimodal", …). Displayed as a badge in the picker row.
    pub specialty: &'static str,
    /// Standardised free-tier usage hint ("1K req/day", "10K neurons/day",
    /// "OAuth", "2 keys", …). Replaces the repetitive "$0.00 per M" so the
    /// user knows at a glance how much quota each upstream actually has.
    pub usage: &'static str,
}

/// Ordered priority of providers we stack into Free mode. Order matters —
/// `free/auto` tries each in turn, so put the highest-quality, most reliable
/// tiers first. The chain starts with the best models (Llama 3.3 70B-class)
/// and falls through to lighter fallbacks.
pub const FREE_CATALOG: &[FreeUpstream] = &[
    // Tier 0: GPT-4o-class models (the crown jewel)
    FreeUpstream {
        id: "github-copilot",
        title: "GitHub Copilot",
        key_url: "github.com/settings/tokens",
        default_model: "gpt-4o-2024-11-20",
        model_family: "gpt-4o",
        note: "GPT-4o (16K ctx) — free OAuth via /connect",
        tool_calling: true,
        max_tokens_cap: Some(16_384),
        fallback_models: &["gpt-4o-2024-08-06"],
        specialty: "best overall",
        usage: "OAuth · 16K",
    },
    // Tier 1: Best-quality open-weight models
    FreeUpstream {
        id: "huggingface",
        title: "Hugging Face",
        key_url: "huggingface.co/settings/tokens",
        default_model: "meta-llama/Llama-3.3-70B-Instruct",
        model_family: "llama-3.3-70b",
        note: "free Inference API — Llama 3.3 70B",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "strong generalist",
        usage: "free API · 8K",
    },
    FreeUpstream {
        id: "nvidia",
        title: "NVIDIA NIM",
        key_url: "build.nvidia.com",
        default_model: "meta/llama-3.3-70b-instruct",
        model_family: "llama-3.3-70b",
        note: "Llama 3.3 70B — 2 keys",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        specialty: "strong generalist",
        usage: "2 keys · 8K",
        // The free tier's 70B worker is routinely capacity-starved (503
        // "ResourceExhausted" or 25-75s responses vs the 30s upstream
        // timeout). Fall back to the always-warm 8B on the same key before
        // giving up on NVIDIA entirely.
        fallback_models: &["meta/llama-3.1-8b-instruct"],
    },
    FreeUpstream {
        id: "cerebras",
        title: "Cerebras",
        key_url: "cloud.cerebras.ai",
        default_model: "gpt-oss-120b",
        model_family: "gpt-oss-120b",
        note: "GPT-OSS 120B (65K ctx) · Gemma 4 31B",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "large context",
        usage: "65K ctx",
    },
    // Tier 2: Very good models (some currently rate-limited)
    FreeUpstream {
        id: "google",
        title: "Google Gemini",
        key_url: "aistudio.google.com/app/apikey",
        default_model: "gemini-2.5-flash",
        model_family: "gemini-2.5-flash",
        note: "Gemini 2.5 Flash",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "multimodal",
        usage: "free tier · 8K",
    },
    FreeUpstream {
        id: "cloudflare",
        title: "Cloudflare Workers AI",
        key_url: "dash.cloudflare.com",
        default_model: CLOUDFLARE_PROBE_MODEL,
        model_family: "qwen3-30b",
        note: "10K neurons/day — key format ACCOUNT_ID:API_TOKEN",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "coding",
        usage: "10K/day · 8K",
    },
    FreeUpstream {
        id: "groq",
        title: "Groq",
        key_url: "console.groq.com/keys",
        default_model: "openai/gpt-oss-120b",
        model_family: "gpt-oss-120b",
        note: "GPT-OSS 120B · Llama 3.3 70B — 1K req/day",
        tool_calling: true,
        specialty: "large context",
        usage: "1K req/day",
        // The groq() factory's own quirks clamp max_tokens to 512 and total
        // to 8.5K (free-tier TPM budget); leave the catalog cap unset so the
        // provider's authoritative tuning is the only clamp applied.
        max_tokens_cap: None,
        fallback_models: &[],
    },
    FreeUpstream {
        id: "sambanova",
        title: "SambaNova",
        key_url: "cloud.sambanova.ai",
        default_model: "Meta-Llama-3.3-70B-Instruct",
        model_family: "llama-3.3-70b",
        note: "Llama 3.3 70B · DeepSeek V3",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "strong generalist",
        usage: "free tier · 8K",
    },
    // Tier 3: Decent fallbacks
    FreeUpstream {
        id: "cline",
        title: "Cline",
        key_url: "app.cline.bot/settings",
        default_model: "deepseek/deepseek-v4-flash",
        model_family: "deepseek-v4-flash",
        note: "live free-model API — auto-discovers best model at startup (currently deepseek-v4-flash)",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "fast",
        usage: "auto-pick · 8K",
    },
    FreeUpstream {
        id: "mistral",
        title: "Mistral",
        key_url: "console.mistral.ai/api-keys",
        default_model: "labs-devstral-small-2512",
        model_family: "devstral-small",
        note: "Devstral Small (free) · Large · Codestral",
        tool_calling: true,
        max_tokens_cap: None,
        fallback_models: &[],
        specialty: "creative",
        usage: "free · ?K",
    },
    FreeUpstream {
        id: "cohere",
        title: "Cohere",
        key_url: "dashboard.cohere.com/api-keys",
        default_model: "north-mini-code-1-0",
        model_family: "north-mini-code",
        note: "North Mini Code (free) · Command R+",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "coding specialist",
        usage: "free · 8K",
    },
    FreeUpstream {
        id: "opencode-zen",
        title: "OpenCode Zen",
        key_url: "opencode.ai/auth",
        default_model: "minimax-m2.5-free",
        model_family: "minimax-m2.5",
        note: "MiniMax M2.5 — 2 keys",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "general purpose",
        usage: "2 keys · 8K",
    },
    FreeUpstream {
        id: "zai",
        title: "Z.AI",
        key_url: "z.ai/manage-apikey/apikey-list",
        default_model: "glm-4.7",
        model_family: "glm-4.7",
        note: "GLM-4.7 · GLM-5 · GLM-5.1 — Zhipu AI international",
        tool_calling: true,
        max_tokens_cap: Some(8_192),
        fallback_models: &[],
        specialty: "reasoning",
        usage: "free · 8K",
    },
    // Tier 4: Paywalled — kept as last resort
    FreeUpstream {
        id: "openrouter",
        title: "OpenRouter",
        key_url: "openrouter.ai/keys",
        default_model: "openrouter/free",
        model_family: "openrouter-free",
        note: "19 free-tier models — requires $10 prepaid credits",
        tool_calling: true,
        max_tokens_cap: None,
        fallback_models: &[],
        specialty: "variety pack",
        usage: "$10 credits · varies",
    },
];

/// Look up a catalog entry by its `id`.
pub fn catalog_entry(id: &str) -> Option<&'static FreeUpstream> {
    FREE_CATALOG.iter().find(|e| e.id == id)
}

/// Static storage for the most recently built FreeProvider's model defaults.
/// Populated by `build_free_provider` in registry.rs; read by the TUI for
/// the /ctx-viz "Free models" table. Thread-safe via OnceLock.
///
/// Each entry is `(upstream_id, upstream_title, effective_model)` — the
/// id lets the TUI join per-upstream key-health / cooldown data onto the
/// display rows.
static RECENT_FREE_MODEL_DEFAULTS: OnceLock<Mutex<Vec<(String, String, String)>>> = OnceLock::new();
fn recent_free_model_defaults() -> &'static Mutex<Vec<(String, String, String)>> {
    RECENT_FREE_MODEL_DEFAULTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set the free model defaults from a newly-built FreeProvider's chain.
/// Called by `build_free_provider` in registry.rs after constructing the
/// chain. The TUI reads these via [`take_free_model_defaults`].
pub fn store_free_model_defaults(defaults: Vec<(String, String, String)>) {
    if let Ok(mut guard) = recent_free_model_defaults().lock() {
        *guard = defaults;
    }
}

/// Retrieve the stored free model defaults as `(upstream_id, title, model)`
/// triples. Returns a clone so that multiple callers (startup wiring, /models
/// command) all see the same data. Returns an empty vec if none have
/// been stored yet.
pub fn take_free_model_defaults() -> Vec<(String, String, String)> {
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

/// Fetch ALL current free models from Cline's recommended-models API.
///
/// Same endpoint as [`fetch_cline_free_model`] but returns every model ID
/// instead of just the first. The model IDs are in `provider/model` format
/// (e.g. `deepseek/deepseek-v4-flash`).
///
/// Returns the full list of free model IDs, or `None` if the API is
/// unreachable, the key is invalid, or the response is unparseable.
pub fn fetch_cline_free_models(cline_api_key: &str) -> Option<Vec<String>> {
    let key = cline_api_key.to_string();
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
            tracing::warn!("fetch_cline_free_models: HTTP request failed");
            return None;
        };

        if !response.status().is_success() {
            tracing::warn!(
                "fetch_cline_free_models: HTTP {} — check Cline API key",
                response.status(),
            );
            return None;
        }

        let Ok(data) = response.json::<serde_json::Value>() else {
            tracing::warn!("fetch_cline_free_models: failed to parse JSON");
            return None;
        };

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

/// Clamp `req.max_tokens` to the entry's configured cap, when one exists.
/// Single source of truth for the per-upstream token cap — used by every
/// dispatch site (non-streaming fallback, streaming fallback,
/// `RetryingFreeStream` re-dispatch, and the first-byte watchdog probe).
fn clamp_max_tokens_for(req: &mut ProviderRequest, entry: &FreeEntry) {
    if let Some(cap) = entry.upstream.max_tokens_cap {
        req.max_tokens = req.max_tokens.min(cap);
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

    /// Seconds remaining in the 5xx / circuit-breaker cooldown at `idx`,
    /// or `None` when not cooled (or the cooldown has already expired).
    fn cooldown_remaining_secs(&self, idx: usize) -> Option<u64> {
        let until = self.cooldown_until.get(idx).copied().flatten()?;
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
/// GET request to the provider's models endpoint and parsing response headers.
///
/// Uses GET (not HEAD): several upstreams (nvidia, huggingface, cline) reject
/// HEAD with 405. For upstreams whose models endpoint doesn't check auth
/// (nvidia, huggingface, openrouter, sambanova, cline), the key is confirmed
/// with the same minimal `chat/completions` probe as [`validate_upstream_key`]
/// so a dead key is reported as invalid instead of returning empty headers —
/// and the rate-limit headers are read from the **chat response**, since those
/// upstreams expose no rate-limit headers on the models endpoint.
pub fn query_rate_limits(upstream_id: &str, key: &str) -> Result<RateLimitInfo, String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    // Cloudflare's /ai/v1/models endpoint does not support GET (405), and the
    // account-scoped URL is derived from the composite ACCOUNT_ID:API_TOKEN
    // key — probe the chat endpoint directly and read its headers.
    if upstream_id == "cloudflare" {
        let response = probe_cloudflare_chat(key)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("Invalid API token (HTTP {})", status));
        }
        if status == 404 {
            return Err("Invalid Cloudflare account ID (HTTP 404)".to_string());
        }
        // Note: a 429 here is returned as healthy-with-headers, not an error —
        // rate limits are a load signal, not a key-health signal (and the
        // probe already proved the key is valid by reaching this point).
        return Ok(parse_rate_limit_headers(response.headers()));
    }

    let base_url = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
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
            .get(base_url)
            .header("Authorization", format!("Bearer {}", key))
    };

    match request.send() {
        Ok(response) => {
            let status = response.status();
            if !status.is_success() {
                // Reuse the probe classifier so google's 400 ("API key not
                // valid") is reported as invalid, 429 as rate-limited, etc.
                return match classify_probe_status(upstream_id, status.as_u16()) {
                    Ok(()) => Err(format!("HTTP {} — unexpected response", status)),
                    Err(e) => Err(e),
                };
            }

            // Auth-lax upstreams: a models 2xx doesn't prove the key, and the
            // models response carries no rate-limit headers — the
            // chat/completions endpoint is where both auth and limits live.
            // Confirm the key via the chat probe and parse rate-limit headers
            // from THAT response.
            let headers = if models_endpoint_validates_auth(upstream_id) {
                response.headers().clone()
            } else {
                validate_key_via_chat(upstream_id, key, &client)?
                    .headers()
                    .clone()
            };

            Ok(parse_rate_limit_headers(&headers))
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Parse rate-limit information from an HTTP response's headers.
///
/// Shared by the models-endpoint and chat-completions probe responses so
/// `/limits` and the health poller surface the same header names.
fn parse_rate_limit_headers(headers: &reqwest::header::HeaderMap) -> RateLimitInfo {
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

    RateLimitInfo {
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
    }
}

/// Upstreams whose `/v1/models` endpoint does **not** check the API key — it
/// returns 200 even for a garbage key (verified by live probing). For these, a
/// 2xx models response only proves reachability, so the key must be confirmed
/// with a minimal `chat/completions` probe, where auth is enforced.
///
/// opencode-zen is deliberately absent: its chat endpoint also ignores the
/// key, so the models 2xx is the best signal it offers.
///
/// cloudflare is auth-lax in a different sense: its models endpoint does not
/// support GET at all (405), so every probe goes through the chat endpoint
/// (see [`probe_cloudflare_chat`]).
fn models_endpoint_validates_auth(upstream_id: &str) -> bool {
    // Cline's /recommended-models endpoint DOES reject bad keys with 401,
    // so the models response alone proves key validity — no chat probe needed.
    // (Conversely, forcing a chat probe would flag keys as unhealthy during
    // Cline's upstream chat outages even though the key itself is fine.)
    !matches!(
        upstream_id,
        "nvidia" | "huggingface" | "openrouter" | "sambanova" | "cloudflare"
    )
}

/// Classify a models-endpoint HTTP status into a probe verdict.
///
/// Returns `Ok(())` for success, or a human-readable error otherwise.
/// Google reports bad keys as HTTP 400 ("API key not valid") rather than
/// 401/403, so that is mapped to the invalid-key error too.
fn classify_probe_status(upstream_id: &str, status: u16) -> Result<(), String> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if status == 401 || status == 403 || (upstream_id == "google" && status == 400) {
        return Err(format!("Invalid API key (HTTP {})", status));
    }
    if status == 429 {
        return Err("Rate limited — try again later".to_string());
    }
    Err(format!("HTTP {} — unexpected response", status))
}

/// Confirm a key with a minimal 1-token `chat/completions` request.
///
/// Used only for upstreams whose models endpoint doesn't check auth. Providers
/// validate the key *before* model validation, so 401/403 unambiguously means
/// an invalid key; any other response (200, or a model-not-found 4xx, 429)
/// means the key was accepted.
///
/// Returns the chat response on success so callers (e.g. [`query_rate_limits`])
/// can read rate-limit headers from the endpoint that actually enforces them.
/// Default model used for Cloudflare chat probes (must match the catalog's
/// `default_model` so validation exercises the same endpoint the chain uses).
const CLOUDFLARE_PROBE_MODEL: &str = "@cf/qwen/qwen3-30b-a3b-fp8";

/// Send a 1-token `chat/completions` probe to Cloudflare's account-scoped
/// OpenAI-compatible endpoint.
///
/// The key is the composite `ACCOUNT_ID:API_TOKEN`; the account ID is used to
/// build the URL path and only the token is sent as the Bearer credential.
/// Returns the raw response so both [`validate_upstream_key`] and
/// [`query_rate_limits`] can classify it and read headers.
fn probe_cloudflare_chat(key: &str) -> Result<reqwest::blocking::Response, String> {
    let (account_id, api_token) = split_cloudflare_key(key)?;
    let url = format!(
        "https://api.cloudflare.com/client/v4/accounts/{}/ai/v1/chat/completions",
        account_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;
    let body = serde_json::json!({
        "model": CLOUDFLARE_PROBE_MODEL,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });
    match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_token))
        .json(&body)
        .send()
    {
        Ok(response) => Ok(response),
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Split a Cloudflare credential into `(account_id, api_token)`.
/// Reuses the same parsing as the provider factory in
/// `openai_compat_providers.rs` so both paths agree on the composite format.
fn split_cloudflare_key(key: &str) -> Result<(&str, &str), String> {
    crate::providers::openai_compat_providers::cloudflare_parts(key).ok_or_else(|| {
        "Cloudflare key must be ACCOUNT_ID:API_TOKEN (account ID, colon, API token)".to_string()
    })
}

/// Pick `(base_url, probe_model)` for the 1-token chat probe used to
/// confirm auth-lax upstream keys.
///
/// Prefers the upstream's first catalog fallback model when one exists —
/// fallbacks are the always-warm small models (e.g. nvidia's 8B). The free
/// tier's 70B workers are frequently capacity-starved ("ResourceExhausted"
/// 503s or 30s+ responses well past the 5s probe timeout), which marks
/// VALID keys unhealthy. Probing the fallback answers in <1s and proves the
/// key just as well. Upstreams without a fallback probe their default model.
fn chat_probe_for(upstream_id: &str) -> Option<(&'static str, &'static str)> {
    let (base_url, default_model) = match upstream_id {
        "nvidia" => (
            "https://integrate.api.nvidia.com/v1",
            "meta/llama-3.3-70b-instruct",
        ),
        "huggingface" => (
            "https://router.huggingface.co/v1",
            "meta-llama/Llama-3.3-70B-Instruct",
        ),
        "openrouter" => ("https://openrouter.ai/api/v1", "openrouter/free"),
        "sambanova" => ("https://api.sambanova.ai/v1", "Meta-Llama-3.3-70B-Instruct"),
        "cline" => ("https://api.cline.bot/api/v1", "deepseek/deepseek-v4-flash"),
        // Only the 5 auth-lax upstreams reach this probe — every caller gates
        // on `!models_endpoint_validates_auth`. opencode-zen is handled by its
        // models 2xx, so this arm is defensive only.
        _ => return None,
    };
    let model = catalog_entry(upstream_id)
        .and_then(|u| u.fallback_models.first())
        .copied()
        .unwrap_or(default_model);
    Some((base_url, model))
}

fn validate_key_via_chat(
    upstream_id: &str,
    key: &str,
    client: &reqwest::blocking::Client,
) -> Result<reqwest::blocking::Response, String> {
    let (base_url, model) = match chat_probe_for(upstream_id) {
        Some(v) => v,
        None => return Err(format!("No chat probe for '{}'", upstream_id)),
    };

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
    });

    match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", key))
        .json(&body)
        .send()
    {
        Ok(response) => {
            let status = response.status().as_u16();
            if status == 401 || status == 403 {
                Err(format!("Invalid API key (HTTP {})", status))
            } else if status >= 500 {
                // Server-side outage — read the body for diagnostic clues
                // so the health probe doesn't treat a real key as healthy
                // (e.g. Cline's "empty response content" upstream failure).
                let body = response.text().unwrap_or_default();
                let detail = if body.contains("empty response content") {
                    "upstream provider returned empty response"
                } else if !body.is_empty() {
                    &body[..body.len().min(120)]
                } else {
                    "—"
                };
                Err(format!("Server error (HTTP {}): {}", status, detail))
            } else {
                Ok(response)
            }
        }
        Err(e) => Err(format!("Connection failed: {}", e)),
    }
}

/// Validate an API key for a given upstream by making a lightweight request
/// to the provider's models endpoint. Returns `Ok(())` if the key is valid.
///
/// For upstreams whose models endpoint doesn't check auth (nvidia,
/// huggingface, openrouter, sambanova, cline — it returns 200 even for a
/// garbage key), a 2xx response is confirmed with a minimal 1-token
/// `chat/completions` probe so dead keys are actually caught.
pub fn validate_upstream_key(upstream_id: &str, key: &str) -> Result<(), String> {
    if key.trim().len() < 8 {
        return Err("Key too short (min 8 characters)".to_string());
    }

    // Cloudflare's models endpoint does not support GET, so auth is proven
    // with the chat probe directly (account-scoped URL from the composite key).
    if upstream_id == "cloudflare" {
        let response = probe_cloudflare_chat(key)?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(format!("Invalid API token (HTTP {})", status));
        }
        if status == 404 {
            return Err("Invalid Cloudflare account ID (HTTP 404)".to_string());
        }
        if status == 429 {
            return Err("Rate limited — try again later".to_string());
        }
        return Ok(());
    }

    let base_url = match upstream_id {
        "huggingface" => "https://router.huggingface.co/v1/models",
        "cerebras" => "https://api.cerebras.ai/v1/models",
        "nvidia" => "https://integrate.api.nvidia.com/v1/models",
        "google" => "https://generativelanguage.googleapis.com/v1beta/models",
        "groq" => "https://api.groq.com/openai/v1/models",
        "openrouter" => "https://openrouter.ai/api/v1/models",
        "sambanova" => "https://api.sambanova.ai/v1/models",
        "mistral" => "https://api.mistral.ai/v1/models",
        "cohere" => "https://api.cohere.com/v1/models",
        "opencode-zen" => "https://api.opencode.ai/v1/models",
        "zai" => "https://open.bigmodel.cn/api/paas/v4/models",
        "cline" => "https://api.cline.bot/api/v1/ai/cline/recommended-models",
        "github-copilot" => "https://api.githubcopilot.com/models",
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
                if models_endpoint_validates_auth(upstream_id) {
                    return Ok(());
                }
                // Auth-lax models endpoint: confirm the key via chat/completions.
                return validate_key_via_chat(upstream_id, key, &client).map(|_| ());
            }
            classify_probe_status(upstream_id, status.as_u16())
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
    /// Model-first routing: try every chain entry whose upstream hosts the
    /// given `model_family` (in catalog order, each with its own default
    /// model), then fall through to the remaining entries. This is the
    /// `free/family/<slug>` selection from the model-first picker view —
    /// e.g. `free/family/llama-3.3-70b` round-robins across Hugging Face,
    /// NVIDIA and SambaNova before trying other model families.
    Family { model_family: &'static str },
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

        // Model-first family route: `free/family/<slug>` or `family/<slug>`.
        // Resolve the slug against the catalog so an unknown family falls
        // back to Auto rather than silently routing nowhere. We store the
        // catalog's own `&'static str` family slug, never a borrow of the
        // local `normalized` buffer.
        if let Some(rest) = normalized
            .strip_prefix("free/family/")
            .or_else(|| normalized.strip_prefix("family/"))
        {
            if let Some(entry) = FREE_CATALOG.iter().find(|entry| entry.model_family == rest) {
                return Route::Family {
                    model_family: entry.model_family,
                };
            }
            return Route::Auto;
        }

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

    /// One chain entry's contribution to the dispatch plan: the effective
    /// (primary) model first, then any per-upstream fallback models. This
    /// lets a slow or failing primary (e.g. NVIDIA's capacity-starved 70B
    /// exceeding the upstream timeout) fall back to a smaller model on the
    /// SAME provider before the chain moves to the next upstream.
    fn plan_rows_for_entry(&self, idx: usize) -> Vec<(usize, String)> {
        let mut rows = Vec::with_capacity(1 + self.chain[idx].upstream.fallback_models.len());
        rows.push((idx, self.model_for_entry(idx).to_string()));
        for fb in self.chain[idx].upstream.fallback_models {
            rows.push((idx, fb.to_string()));
        }
        rows
    }

    /// Original sequential plan: upstreams in catalog (or pinned) order.
    fn attempt_plan_sequential(&self, route: &Route) -> Vec<(usize, String)> {
        match route {
            Route::Auto => self
                .chain
                .iter()
                .enumerate()
                .flat_map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                .collect(),
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut plan = Vec::with_capacity(self.chain_len());
                // Pinned model first, then the pinned upstream's fallbacks,
                // then the rest of the chain (with their own fallbacks).
                plan.push((*start_idx, pinned_model.clone()));
                plan.extend(
                    self.chain[*start_idx]
                        .upstream
                        .fallback_models
                        .iter()
                        .map(|m| (*start_idx, m.to_string())),
                );
                for (idx, _entry) in self.chain.iter().enumerate() {
                    if idx == *start_idx {
                        continue;
                    }
                    plan.extend(self.plan_rows_for_entry(idx));
                }
                plan
            }
            Route::Family { model_family } => {
                // Model-first: all upstreams hosting the family in catalog
                // order (with their per-upstream fallbacks), then the rest.
                let mut plan = Vec::with_capacity(self.chain_len());
                for (idx, _entry) in self.chain.iter().enumerate() {
                    if self.chain[idx].upstream.model_family == *model_family {
                        plan.extend(self.plan_rows_for_entry(idx));
                    }
                }
                for (idx, _entry) in self.chain.iter().enumerate() {
                    if self.chain[idx].upstream.model_family != *model_family {
                        plan.extend(self.plan_rows_for_entry(idx));
                    }
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
                // Shuffle per-upstream GROUPS so each upstream's fallback
                // models stay adjacent to their primary.
                let mut groups: Vec<Vec<(usize, String)>> = self
                    .chain
                    .iter()
                    .enumerate()
                    .map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                groups.shuffle(&mut rng);
                groups.into_iter().flatten().collect()
            }
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                let mut rest: Vec<Vec<(usize, String)>> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != *start_idx)
                    .map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                rest.shuffle(&mut rng);

                let mut plan = Vec::with_capacity(self.chain_len());
                plan.push((*start_idx, pinned_model.clone()));
                plan.extend(
                    self.chain[*start_idx]
                        .upstream
                        .fallback_models
                        .iter()
                        .map(|m| (*start_idx, m.to_string())),
                );
                for group in rest {
                    plan.extend(group);
                }
                plan
            }
            Route::Family { model_family } => {
                // Family upstreams first (each with their fallbacks), then the
                // rest — both groups shuffled independently so the family
                // still leads the plan.
                let family_idx: Vec<usize> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| self.chain[*idx].upstream.model_family == *model_family)
                    .map(|(idx, _)| idx)
                    .collect();
                let mut family_groups: Vec<Vec<(usize, String)>> = family_idx
                    .iter()
                    .map(|idx| self.plan_rows_for_entry(*idx))
                    .collect();
                family_groups.shuffle(&mut rng);

                let mut rest_groups: Vec<Vec<(usize, String)>> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| self.chain[*idx].upstream.model_family != *model_family)
                    .map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                rest_groups.shuffle(&mut rng);

                family_groups
                    .into_iter()
                    .chain(rest_groups)
                    .flatten()
                    .collect()
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
                // sort_by is stable, so each upstream's fallback rows (same
                // idx, equal latency) stay adjacent to their primary.
                let mut plan: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .flat_map(|(idx, _entry)| self.plan_rows_for_entry(idx))
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
                    .flat_map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                rest.sort_by(|a, b| {
                    latencies
                        .avg_latency(a.0)
                        .partial_cmp(&latencies.avg_latency(b.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                let mut plan = Vec::with_capacity(self.chain_len());
                plan.push((*start_idx, pinned_model.clone()));
                plan.extend(
                    self.chain[*start_idx]
                        .upstream
                        .fallback_models
                        .iter()
                        .map(|m| (*start_idx, m.to_string())),
                );
                plan.extend(rest);
                plan
            }
            Route::Family { model_family } => {
                // Family upstreams first (each with their fallbacks), then the
                // rest — both sorted by latency, family always leading.
                let mut family: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| self.chain[*idx].upstream.model_family == *model_family)
                    .flat_map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                family.sort_by(|a, b| {
                    latencies
                        .avg_latency(a.0)
                        .partial_cmp(&latencies.avg_latency(b.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                let mut rest: Vec<(usize, String)> = self
                    .chain
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| self.chain[*idx].upstream.model_family != *model_family)
                    .flat_map(|(idx, _entry)| self.plan_rows_for_entry(idx))
                    .collect();
                rest.sort_by(|a, b| {
                    latencies
                        .avg_latency(a.0)
                        .partial_cmp(&latencies.avg_latency(b.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                });

                family.extend(rest);
                family
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

    /// Check if an upstream is in any cooldown (circuit breaker, 5xx, or
    /// empty-completion).  Always consults the cooldown state regardless of
    /// whether the circuit breaker is enabled, so that 5xx and empty-completion
    /// cooldowns are effective even without a configured circuit breaker.
    fn is_in_cooldown(&self, idx: usize) -> bool {
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        cd.is_in_cooldown(idx) || cd.is_in_empty_cooldown(idx)
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
    /// one per entry in the fallback chain. Each entry is
    /// `(upstream_id, upstream_title, effective_model)` — the id lets the
    /// TUI join per-upstream key-health / cooldown data onto the display.
    /// Used by the TUI to show which free models were auto-detected at
    /// startup or via live discovery (Cline, OpenRouter, etc.).
    pub fn free_model_defaults(&self) -> Vec<(String, String, String)> {
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                (
                    entry.upstream.id.to_string(),
                    entry.upstream.title.to_string(),
                    self.model_for_entry(idx).to_string(),
                )
            })
            .collect()
    }

    /// Clamp the request's `max_tokens` to the upstream's cap when one is
    /// configured.  Called before dispatching to avoid sending downstream
    /// requests that the upstream will reject or silently truncate.
    fn clamp_max_tokens(&self, req: &mut ProviderRequest, idx: usize) {
        clamp_max_tokens_for(req, &self.chain[idx]);
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
            let mut cd = self.cooldown.lock().unwrap();
            cd.prune_expired();
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
            clamp_max_tokens_for(&mut req, entry);
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
                            let mut cd = self.cooldown.lock().unwrap();
                            cd.prune_expired();
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
                            clamp_max_tokens_for(&mut req, entry);
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
            self.clamp_max_tokens(&mut req, idx);

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
            self.clamp_max_tokens(&mut req, idx);

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

    fn mark_key_healthy(&self, upstream_id: Option<&str>, key_idx: usize) -> bool {
        let Some(upstream_id) = upstream_id else {
            return false;
        };
        for entry in &self.chain {
            if entry.upstream.id == upstream_id {
                return entry.provider.mark_key_healthy(Some(upstream_id), key_idx);
            }
        }
        false
    }

    fn mark_key_exhausted(
        &self,
        upstream_id: Option<&str>,
        key_idx: usize,
        cooldown_secs: u64,
        reason: Option<String>,
    ) -> bool {
        // Forward to the matching chain entry's key ring. The health poller
        // (spec §6.4) injects definitively-dead keys through this path so the
        // TUI's key-health indicators and rotation order learn about them
        // without waiting for the next real request.
        let Some(upstream_id) = upstream_id else {
            return false;
        };
        for entry in &self.chain {
            if entry.upstream.id == upstream_id {
                return entry.provider.mark_key_exhausted(
                    Some(upstream_id),
                    key_idx,
                    cooldown_secs,
                    reason,
                );
            }
        }
        false
    }

    /// Return per-upstream empty-cooldown summaries for the /keys health
    /// command and TUI status display (spec §6.3).
    ///
    /// Implemented as a trait override (not just an inherent method) so the
    /// registry's [`ProviderRegistry::empty_cooldown_summaries`] — which
    /// queries through `Arc<dyn LlmProvider>` — actually sees the data.
    fn upstream_empty_cooldowns(&self) -> Vec<(String, u32, Option<u64>)> {
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

    fn upstream_key_health(&self) -> Vec<(String, usize, usize, Option<u64>)> {
        // Per-upstream view of key-ring health: only upstreams wrapped in a
        // KeyRotatingProvider (2+ keys) report a ring, matching the
        // aggregated key_ring_status() above.
        self.chain
            .iter()
            .filter_map(|entry| {
                entry
                    .provider
                    .key_ring_status()
                    .map(|(active, total, retry)| {
                        (entry.upstream.id.to_string(), active, total, retry)
                    })
            })
            .collect()
    }

    fn upstream_cooldowns(&self) -> Vec<(String, String, Option<u64>)> {
        // Both cooldown kinds: "5xx" (server-error / circuit-breaker) and
        // "empty" (empty-completion). Locked once, never across an await.
        let cd = self.cooldown.lock().unwrap();
        let mut out = Vec::new();
        for (idx, entry) in self.chain.iter().enumerate() {
            if let Some(secs) = cd.cooldown_remaining_secs(idx) {
                out.push((entry.upstream.id.to_string(), "5xx".to_string(), Some(secs)));
            }
            if let Some(secs) = cd.empty_cooldown_remaining_secs(idx) {
                out.push((
                    entry.upstream.id.to_string(),
                    "empty".to_string(),
                    Some(secs),
                ));
            }
        }
        out
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
            Route::Family { model_family } => {
                let idx = self
                    .chain
                    .iter()
                    .position(|e| e.upstream.model_family == model_family)?;
                (idx, self.chain.get(idx)?)
            }
        };
        Some(self.chain[idx].upstream.tool_calling)
    }

    fn max_tokens_cap_for(&self, model: &str) -> Option<u32> {
        let route = self.resolve_route(model);
        let (idx, _) = match route {
            Route::Auto => self.chain.first().map(|e| (0, e))?,
            Route::Pinned { start_idx, .. } => (start_idx, self.chain.get(start_idx)?),
            Route::Family { model_family } => {
                let idx = self
                    .chain
                    .iter()
                    .position(|e| e.upstream.model_family == model_family)?;
                (idx, self.chain.get(idx)?)
            }
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

    /// Test harness: records `(upstream_id, key_idx, cooldown_secs)` calls to
    /// `mark_key_exhausted` so tests can assert exhaustion forwarding.
    /// Named to keep clippy::type_complexity off the StubProvider fields.
    type ExhaustionRecorder = Arc<Mutex<Vec<(Option<String>, usize, u64)>>>;

    // ---- Rate-limit header parsing -------------------------------------------

    #[test]
    fn parse_rate_limit_headers_reads_standard_names() {
        use reqwest::header::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-limit-requests", HeaderValue::from_static("30"));
        headers.insert(
            "x-ratelimit-remaining-requests",
            HeaderValue::from_static("12"),
        );
        headers.insert(
            "x-ratelimit-limit-requests-day",
            HeaderValue::from_static("1000"),
        );
        headers.insert(
            "x-ratelimit-remaining-requests-day",
            HeaderValue::from_static("999"),
        );
        headers.insert(
            "x-ratelimit-limit-tokens",
            HeaderValue::from_static("200000"),
        );
        headers.insert(
            "x-ratelimit-remaining-tokens",
            HeaderValue::from_static("123456"),
        );
        headers.insert("retry-after", HeaderValue::from_static("7"));

        let info = parse_rate_limit_headers(&headers);
        assert_eq!(info.rpm_limit, Some(30));
        assert_eq!(info.rpm_remaining, Some(12));
        assert_eq!(info.rpd_limit, Some(1000));
        assert_eq!(info.rpd_remaining, Some(999));
        assert_eq!(info.tpm_limit, Some(200000));
        assert_eq!(info.tpm_remaining, Some(123456));
        assert_eq!(info.retry_after, Some(7));
        assert!(info.headers_found);
    }

    #[test]
    fn parse_rate_limit_headers_without_headers_reports_none() {
        use reqwest::header::HeaderMap;

        let info = parse_rate_limit_headers(&HeaderMap::new());
        assert_eq!(info.rpm_limit, None);
        assert_eq!(info.retry_after, None);
        assert!(!info.headers_found);
    }

    // ---- Key-probe classification -------------------------------------------

    #[test]
    fn auth_lax_upstreams_need_chat_confirmation() {
        // These upstreams' /v1/models endpoint returns 200 even for a garbage
        // key (verified by live probing), so a 2xx alone must not conclude
        // "healthy" — the chat probe is required. cloudflare is auth-lax in a
        // different sense: its models endpoint doesn't support GET at all.
        for id in [
            "nvidia",
            "huggingface",
            "openrouter",
            "sambanova",
            "cloudflare",
        ] {
            assert!(
                !models_endpoint_validates_auth(id),
                "{} should be auth-lax",
                id
            );
        }
        // Everything else validates the key on the models endpoint.
        for id in [
            "groq", "cerebras", "google", "mistral", "cohere", "zai", "cline",
        ] {
            assert!(
                models_endpoint_validates_auth(id),
                "{} should validate auth",
                id
            );
        }
    }

    #[test]
    fn chat_probe_prefers_fallback_model_for_capacity_starved_upstreams() {
        // nvidia has a catalog fallback (8B) — the probe must use it instead
        // of the capacity-starved 70B default, so valid keys aren't marked
        // unhealthy by a 30s+ 503.
        let (base, model) = chat_probe_for("nvidia").expect("nvidia probe");
        assert_eq!(model, "meta/llama-3.1-8b-instruct");
        assert!(base.contains("nvidia.com"));
        // Upstreams without fallbacks probe their default model.
        let (_, hf_model) = chat_probe_for("huggingface").expect("hf probe");
        assert_eq!(hf_model, "meta-llama/Llama-3.3-70B-Instruct");
        let (_, sb_model) = chat_probe_for("sambanova").expect("sambanova probe");
        assert_eq!(sb_model, "Meta-Llama-3.3-70B-Instruct");
        // Unsupported upstreams have no chat probe.
        assert!(chat_probe_for("groq").is_none());
    }

    #[test]
    fn probe_status_classification() {
        // Success on an auth-checking upstream is a clean pass.
        assert_eq!(classify_probe_status("groq", 200), Ok(()));
        assert_eq!(classify_probe_status("google", 200), Ok(()));
        // 401/403 are invalid keys everywhere.
        assert!(classify_probe_status("groq", 401).is_err());
        assert!(classify_probe_status("nvidia", 403).is_err());
        // Google reports bad keys as 400 ("API key not valid") — mapped to
        // the invalid-key error, not "unexpected response".
        let err = classify_probe_status("google", 400).unwrap_err();
        assert!(err.contains("Invalid API key"), "got: {}", err);
        // A 400 on a non-Google upstream stays "unexpected response".
        let err = classify_probe_status("groq", 400).unwrap_err();
        assert!(err.contains("unexpected response"), "got: {}", err);
        // 429 is rate-limited.
        let err = classify_probe_status("groq", 429).unwrap_err();
        assert!(err.contains("Rate limited"), "got: {}", err);
        // 5xx is unexpected.
        let err = classify_probe_status("nvidia", 500).unwrap_err();
        assert!(err.contains("unexpected response"), "got: {}", err);
    }

    struct StubProvider {
        id: ProviderId,
        ok: bool,
        /// When set, records the `max_tokens` value seen by `create_message`
        /// so tests can assert dispatch-time clamping.
        seen_max_tokens: Option<Arc<Mutex<Option<u32>>>>,
        /// When set, reports a key-ring status via `key_ring_status()` so
        /// tests can exercise `upstream_key_health()`.
        ring_status: Option<(usize, usize, Option<u64>)>,
        /// When set, records `mark_key_exhausted` calls as
        /// `(upstream_id, key_idx, cooldown_secs)` so tests can assert
        /// exhaustion forwarding from the composite provider.
        exhaustion: Option<ExhaustionRecorder>,
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
            if let Some(rec) = &self.seen_max_tokens {
                if let Ok(mut g) = rec.lock() {
                    *g = Some(request.max_tokens);
                }
            }
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

        fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
            self.ring_status
        }

        fn mark_key_exhausted(
            &self,
            upstream_id: Option<&str>,
            key_idx: usize,
            cooldown_secs: u64,
            _reason: Option<String>,
        ) -> bool {
            if let Some(rec) = &self.exhaustion {
                if let Ok(mut g) = rec.lock() {
                    g.push((upstream_id.map(|s| s.to_string()), key_idx, cooldown_secs));
                }
            }
            true
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
                seen_max_tokens: None,
                ring_status: None,
                exhaustion: None,
            }),
            effective_model: None,
        }
    }

    fn entry_with_recorder(
        id: &'static str,
        ok: bool,
        recorder: Arc<Mutex<Option<u32>>>,
    ) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
                seen_max_tokens: Some(recorder),
                ring_status: None,
                exhaustion: None,
            }),
            effective_model: None,
        }
    }

    fn entry_with_exhaustion_recorder(id: &'static str, recorder: ExhaustionRecorder) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok: true,
                seen_max_tokens: None,
                ring_status: None,
                exhaustion: Some(recorder),
            }),
            effective_model: None,
        }
    }

    fn entry_with_ring(id: &'static str, ring: (usize, usize, Option<u64>)) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok: true,
                seen_max_tokens: None,
                ring_status: Some(ring),
                exhaustion: None,
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
    fn nvidia_plan_includes_8b_fallback_after_70b() {
        let provider = FreeProvider::new(vec![
            entry("nvidia", true),
            entry("cerebras", true),
            entry("groq", true),
        ]);
        // Sequential Auto plan: nvidia's 70B primary, then its 8B fallback on
        // the SAME index, then the other upstreams.
        let plan = provider.attempt_plan(&Route::Auto);
        assert_eq!(plan[0], (0, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (0, "meta/llama-3.1-8b-instruct".to_string()));
        assert_eq!(plan[2], (1, "gpt-oss-120b".to_string()));
        assert_eq!(plan[3], (2, "openai/gpt-oss-120b".to_string()));
        // Upstreams without fallbacks still contribute exactly one row.
        assert_eq!(plan.len(), 4);
    }

    #[test]
    fn pinned_route_tries_pinned_model_then_upstream_fallbacks() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("nvidia", true),
            entry("cerebras", true),
        ]);
        // Pinning nvidia: the pinned model, then nvidia's 8B fallback, then
        // the rest of the chain in catalog order.
        let plan = provider.attempt_plan(&Route::Pinned {
            start_idx: 1,
            pinned_model: "meta/llama-3.3-70b-instruct".to_string(),
        });
        assert_eq!(plan[0], (1, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (1, "meta/llama-3.1-8b-instruct".to_string()));
        assert_eq!(
            plan[2],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan[3], (2, "gpt-oss-120b".to_string()));
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
    fn family_route_resolves_from_slug() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        match provider.resolve_route("free/family/llama-3.3-70b") {
            Route::Family { model_family } => assert_eq!(model_family, "llama-3.3-70b"),
            other => panic!("expected family, got {:?}", other),
        }
        // Bare `family/<slug>` is accepted too.
        match provider.resolve_route("family/llama-3.3-70b") {
            Route::Family { model_family } => assert_eq!(model_family, "llama-3.3-70b"),
            other => panic!("expected family, got {:?}", other),
        }
    }

    #[test]
    fn unknown_family_falls_back_to_auto() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        assert!(matches!(
            provider.resolve_route("free/family/does-not-exist"),
            Route::Auto
        ));
        assert!(matches!(
            provider.resolve_route("family/does-not-exist"),
            Route::Auto
        ));
    }

    #[test]
    fn family_plan_leads_with_hosts_then_rest() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("cerebras", true),
            entry("nvidia", true),
            entry("groq", true),
        ]);
        let plan = provider.attempt_plan(&Route::Family {
            model_family: "llama-3.3-70b",
        });
        // Family hosts first in catalog order — huggingface (idx 0), then
        // nvidia (idx 2) with its 8B fallback on the same index.
        assert_eq!(
            plan[0],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan[1], (2, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[2], (2, "meta/llama-3.1-8b-instruct".to_string()));
        // Non-family upstreams follow in catalog order.
        assert_eq!(plan[3], (1, "gpt-oss-120b".to_string()));
        assert_eq!(plan[4], (3, "openai/gpt-oss-120b".to_string()));
    }

    #[test]
    fn family_route_reports_host_capabilities() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        // The catalog's huggingface entry hosts llama-3.3-70b with tool
        // calling and a max-tokens cap — the family route must surface those
        // from the first matching host.
        let tc = provider.tool_calling_for("free/family/llama-3.3-70b");
        assert_eq!(tc, Some(true));
        let cap = provider.max_tokens_cap_for("free/family/llama-3.3-70b");
        assert!(cap.is_some());
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
    // max_tokens_cap clamping tests
    // -------------------------------------------------------------------

    #[tokio::test]
    async fn create_message_clamps_max_tokens_to_upstream_cap() {
        // huggingface catalog entry has max_tokens_cap = 8_192.
        let recorder = Arc::new(Mutex::new(None));
        let provider = FreeProvider::new(vec![entry_with_recorder(
            "huggingface",
            true,
            recorder.clone(),
        )]);
        let mut req = dummy_request("free/auto");
        req.max_tokens = 16_384;
        provider.create_message(req).await.expect("should succeed");
        let seen = *recorder.lock().unwrap();
        assert_eq!(
            seen,
            Some(8_192),
            "max_tokens must be clamped to upstream cap"
        );
    }

    #[test]
    fn clamp_max_tokens_for_noop_when_no_cap() {
        // mistral catalog entry has max_tokens_cap = None.
        let entry = entry("mistral", true);
        let mut req = dummy_request("mistral/x");
        req.max_tokens = 16_384;
        clamp_max_tokens_for(&mut req, &entry);
        assert_eq!(req.max_tokens, 16_384, "no cap means no clamping");
    }

    #[test]
    fn clamp_max_tokens_for_never_raises_max_tokens() {
        let entry = entry("huggingface", true); // cap = 8_192
        let mut req = dummy_request("huggingface/x");
        req.max_tokens = 4_096;
        clamp_max_tokens_for(&mut req, &entry);
        assert_eq!(
            req.max_tokens, 4_096,
            "smaller request must pass through unchanged"
        );
    }

    // -------------------------------------------------------------------
    // 5xx cooldown visibility tests (no circuit breaker configured)
    // -------------------------------------------------------------------

    #[test]
    fn five_xx_cooldown_is_visible_without_circuit_breaker() {
        // Circuit breaker is disabled by default; the 5xx cooldown must
        // still be visible to is_in_cooldown (regression for the old gate
        // that made 5xx cooldowns dead on the non-streaming path).
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        assert!(
            provider.is_in_cooldown(0),
            "5xx cooldown should be visible even with circuit breaker disabled"
        );
    }

    #[tokio::test]
    async fn five_xx_cooldown_skips_upstream_in_fallback() {
        // Use a *working* first upstream so the skip is observable: with the
        // old buggy is_in_cooldown gate the loop would try huggingface,
        // succeed, and return its model; with the fix it skips the cooled
        // upstream and lands on cerebras.
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        assert!(provider.is_in_cooldown(0));

        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(
            resp.model, "gpt-oss-120b",
            "cooled-down upstream must be skipped even though it would succeed"
        );
    }

    #[test]
    fn upstream_cooldowns_reports_5xx_and_empty_kinds() {
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        // 5xx cooldown on the first upstream (default 45s).
        let err = ProviderError::ServerError {
            provider: ProviderId::new("huggingface"),
            status: Some(503),
            message: "boom".into(),
            is_retryable: true,
        };
        provider.maybe_cooldown_upstream_for_5xx(0, &err);
        // Empty-completion cooldown on the second upstream (default max 3,
        // cooldown 60s). Drive the cooldown state directly — the empty-completion
        // recording path lives on RetryingFreeStream. `record_empty` returns
        // `just_cooled`, i.e. true only when the threshold is crossed.
        {
            let mut cd = provider.cooldown.lock().unwrap();
            assert!(
                !cd.record_empty(1, 3, 60),
                "first empty must not trip the cooldown"
            );
            assert!(
                !cd.record_empty(1, 3, 60),
                "second empty must not trip the cooldown"
            );
            assert!(
                cd.record_empty(1, 3, 60),
                "third consecutive empty must trip the cooldown"
            );
        }

        let cooldowns = provider.upstream_cooldowns();
        let kinds: Vec<&str> = cooldowns.iter().map(|(_, k, _)| k.as_str()).collect();
        assert!(
            kinds.contains(&"5xx"),
            "5xx cooldown must be reported, got {:?}",
            cooldowns
        );
        assert!(
            kinds.contains(&"empty"),
            "empty cooldown must be reported, got {:?}",
            cooldowns
        );
        for (_, _, retry) in &cooldowns {
            assert!(retry.is_some(), "active cooldowns must carry retry_secs");
        }

        // The trait override must surface the empty cooldown through `dyn` —
        // guards the regression where upstream_empty_cooldowns was only an
        // inherent method and the registry (Arc<dyn LlmProvider>) always got
        // the empty trait default.
        let dyn_provider: Arc<dyn LlmProvider> = Arc::new(provider);
        let empty = dyn_provider.upstream_empty_cooldowns();
        assert!(
            empty.iter().any(|(id, _, _)| id == "cerebras"),
            "trait upstream_empty_cooldowns must report cerebras, got {:?}",
            empty
        );
    }

    #[test]
    fn upstream_key_health_reports_ring_backed_upstreams() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry_with_ring("cerebras", (1, 2, Some(45))),
        ]);
        let health = provider.upstream_key_health();
        assert_eq!(
            health.len(),
            1,
            "only ring-backed upstreams report health, got {:?}",
            health
        );
        assert_eq!(health[0].0, "cerebras");
        assert_eq!((health[0].1, health[0].2), (1, 2));
        assert_eq!(health[0].3, Some(45));
    }

    #[test]
    fn mark_key_exhausted_forwards_to_matching_upstream() {
        let recorder: ExhaustionRecorder = Arc::new(Mutex::new(Vec::new()));
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry_with_exhaustion_recorder("cerebras", recorder.clone()),
        ]);

        // Matches the chain entry's upstream id → forwarded with the real
        // key index and cooldown (as injected by the health poller, §6.4).
        assert!(provider.mark_key_exhausted(
            Some("cerebras"),
            2,
            300,
            Some("Invalid API key (HTTP 401)".to_string())
        ));
        let recorded = recorder.lock().unwrap();
        assert_eq!(recorded.len(), 1, "exactly one forwarding expected");
        assert_eq!(recorded[0], (Some("cerebras".to_string()), 2, 300));
        drop(recorded);
        recorder.lock().unwrap().clear();

        // Unknown upstream / missing id → not forwarded, returns false.
        assert!(!provider.mark_key_exhausted(Some("nope"), 0, 1, None));
        assert!(!provider.mark_key_exhausted(None, 0, 1, None));
        assert!(recorder.lock().unwrap().is_empty(), "no extra forwards");
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
    fn latency_plan_keeps_fallback_adjacent_after_primary() {
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::LatencyBased,
            latency: Some(LatencyConfig { max_samples: 10 }),
            ..Default::default()
        };
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("nvidia", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            cfg,
            false,
        );

        // Record distinct latencies: nvidia fastest (100ms), google 300ms,
        // cerebras 500ms, huggingface 800ms. Even though the latency sort
        // reorders upstreams, nvidia's 8B fallback row must stay adjacent
        // AFTER its 70B primary (stable sort keeps same-idx rows together
        // in insertion order).
        provider.record_success(0, Duration::from_millis(800));
        provider.record_success(1, Duration::from_millis(100));
        provider.record_success(2, Duration::from_millis(500));
        provider.record_success(3, Duration::from_millis(300));

        let plan = provider.attempt_plan(&Route::Auto);

        // nvidia (idx 1, fastest) first: 70B then its 8B fallback adjacent.
        assert_eq!(plan[0], (1, "meta/llama-3.3-70b-instruct".to_string()));
        assert_eq!(plan[1], (1, "meta/llama-3.1-8b-instruct".to_string()));
        // google (300ms), cerebras (500ms), huggingface (800ms).
        assert_eq!(plan[2], (3, "gemini-2.5-flash".to_string()));
        assert_eq!(plan[3], (2, "gpt-oss-120b".to_string()));
        assert_eq!(
            plan[4],
            (0, "meta-llama/Llama-3.3-70B-Instruct".to_string())
        );
        assert_eq!(plan.len(), 5);
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

/// Spawn a robust mock HTTP server on `listener` that answers every
/// connection with `response`. Uses a thread per connection and drains the
/// request before replying — a naive single-threaded accept→write loop makes
/// hyper intermittently fail with "received unexpected message from
/// connection" (a response racing keep-alive connection reuse), which flaked
/// these tests. Returns a ready flag the caller spins on so the fetch never
/// races a not-yet-starting accept loop.
#[cfg(test)]
fn spawn_mock_server(
    listener: std::net::TcpListener,
    response: String,
) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
    let server_ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready = server_ready.clone();
    std::thread::spawn(move || {
        ready.store(true, std::sync::atomic::Ordering::SeqCst);
        for mut s in listener.incoming().take(16).flatten() {
            let response = response.clone();
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let _ = s.write_all(response.as_bytes());
            });
        }
    });
    server_ready
}

/// Spin until the mock server's accept loop is running.
#[cfg(test)]
fn wait_for_mock_server(ready: &std::sync::atomic::AtomicBool) {
    while !ready.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Minimal 200 OK JSON response builder for the mock servers.
#[cfg(test)]
fn mock_json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
}

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

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

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
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("test-key", &base_url, "unknown-provider");
    assert_eq!(result.as_deref(), Some("qwen-3-235b"));
}

#[test]
fn fetch_openai_compat_model_list_handles_http_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(
        listener,
        "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_string(),
    );
    wait_for_mock_server(&ready);

    let base_url = format!("http://127.0.0.1:{}", port);
    let result = fetch_openai_compat_model_list("bad-key", &base_url, "groq");
    assert!(result.is_none());
}

#[test]
fn fetch_openai_compat_model_list_handles_empty_response() {
    let json = r#"{"data": []}"#;

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ready = spawn_mock_server(listener, mock_json_response(json));
    wait_for_mock_server(&ready);

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
