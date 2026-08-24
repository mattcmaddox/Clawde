// providers/free/catalog.rs — Free-mode upstream catalog.
//
// The ordered list of free-tier providers stacked behind the synthetic
// `free/auto` model, plus the small helpers that look entries up and stash
// the most recently built model defaults for the TUI.

use std::sync::{Mutex, OnceLock};

/// One locally declared quota window. These values are used only when a
/// provider does not expose usable response headers; unknown limits remain
/// unestimated rather than being guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalQuotaWindow {
    pub limit: u64,
    pub window_secs: u64,
}

/// Explicit local quota metadata for an upstream. Request and token windows
/// are independent because providers commonly use different reset periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalQuota {
    pub requests: Option<LocalQuotaWindow>,
    pub tokens: Option<LocalQuotaWindow>,
}

impl LocalQuota {
    pub const fn requests(limit: u64, window_secs: u64) -> Self {
        Self {
            requests: Some(LocalQuotaWindow { limit, window_secs }),
            tokens: None,
        }
    }

    pub const fn windows(
        requests: Option<LocalQuotaWindow>,
        tokens: Option<LocalQuotaWindow>,
    ) -> Self {
        Self { requests, tokens }
    }
}

/// Return only limits that are explicit enough to support conservative local
/// accounting. Ranges, provider-specific units, and unverified limits stay
/// out of this estimator.
pub(crate) fn local_quota_for(id: &str) -> Option<LocalQuota> {
    match id {
        // Catalog usage hint: 1K requests/day.
        "groq" => Some(LocalQuota::requests(1_000, 24 * 60 * 60)),
        // Profile metadata: 5 RPM and 30K TPM.
        "cerebras" => Some(LocalQuota::windows(
            Some(LocalQuotaWindow {
                limit: 5,
                window_secs: 60,
            }),
            Some(LocalQuotaWindow {
                limit: 30_000,
                window_secs: 60,
            }),
        )),
        // Profile metadata: 20 RPM and 200K tokens/day.
        "sambanova" => Some(LocalQuota::windows(
            Some(LocalQuotaWindow {
                limit: 20,
                window_secs: 60,
            }),
            Some(LocalQuotaWindow {
                limit: 200_000,
                window_secs: 24 * 60 * 60,
            }),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// One upstream provider in the free-mode chain.
///
/// `id` is the canonical clawde `ProviderId` string — the auth store key the
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
    /// Whether the upstream's models accept image input. Image-bearing
    /// requests skip non-vision upstreams at plan-build time so a text-only
    /// provider's 400 InvalidRequest can't hard-fail the whole request
    /// (audit spec §8.4 "capability match").
    pub vision: bool,
    /// Hard cap on `max_tokens` for this upstream's default model.
    /// When set, requests are silently clamped to this value.
    pub max_tokens_cap: Option<u32>,
    /// Input context window (tokens) of the upstream's default model.
    /// Requests whose estimated size exceeds this are skipped at plan-build
    /// time (audit spec §8.4 "capability match"), and `discover_models()`
    /// reports it so the picker reflects the real cap.
    pub context_window: u32,
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
        vision: true,
        max_tokens_cap: Some(16_384),
        context_window: 16_384,
        fallback_models: &["gpt-4o-2024-08-06"],
        specialty: "best overall",
        usage: "OAuth · 16K",
    },
    // Tier 1: Best-quality open-weight models
    FreeUpstream {
        id: "poolside",
        title: "Poolside",
        key_url: "poolside.ai",
        default_model: "poolside/laguna-s-2.1",
        model_family: "laguna-s-2.1",
        note: "Laguna S 2.1 (118B MoE, 1M ctx) — free in Preview",
        tool_calling: true,
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 1_024_000,
        fallback_models: &[],
        specialty: "coding specialist",
        usage: "free preview · 1M ctx",
    },
    FreeUpstream {
        id: "nvidia",
        title: "NVIDIA NIM",
        key_url: "build.nvidia.com",
        // NVIDIA retired the llama-3.3-70b family (DEPRECATION 08/25/2026 per
        // its own catalog API); gpt-oss-120b is the current free flagship and
        // the discovery default pick (NVIDIA_PREFERRED_FREE).
        default_model: "openai/gpt-oss-120b",
        model_family: "gpt-oss-120b",
        note: "GPT-OSS 120B — 2 keys",
        tool_calling: true,
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
        specialty: "strong generalist",
        usage: "2 keys · 8K",
        // The free tier's 120B worker is routinely capacity-starved (503
        // "ResourceExhausted" or 25-75s responses vs the 30s upstream
        // timeout). Fall back to the always-warm 20B sibling on the same key
        // before giving up on NVIDIA entirely.
        fallback_models: &["openai/gpt-oss-20b"],
    },
    FreeUpstream {
        id: "cerebras",
        title: "Cerebras",
        key_url: "cloud.cerebras.ai",
        default_model: "gpt-oss-120b",
        model_family: "gpt-oss-120b",
        note: "GPT-OSS 120B (65K ctx) · Gemma 4 31B",
        tool_calling: true,
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 65_536,
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
        vision: true,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
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
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
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
        vision: false,
        specialty: "large context",
        usage: "1K req/day",
        // The groq() factory's own quirks clamp max_tokens to 512 and total
        // to 8.5K (free-tier TPM budget); leave the catalog cap unset so the
        // provider's authoritative tuning is the only clamp applied.
        max_tokens_cap: None,
        context_window: 128_000,
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
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
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
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
        fallback_models: &[],
        specialty: "fast",
        usage: "auto-pick · 8K",
    },
    FreeUpstream {
        id: "mistral",
        title: "Mistral",
        key_url: "console.mistral.ai/api-keys",
        // The free "Experiment" tier rate-limits ALL API models at $0; the
        // old Devstral Small default was retired 2026-03-31. Mistral Large 3
        // (25.12) is the current flagship and is free on that tier.
        default_model: "mistral-large-2512",
        model_family: "mistral-large",
        note: "Mistral Large 3 (free Experiment tier) · Codestral",
        tool_calling: true,
        vision: false,
        max_tokens_cap: None,
        context_window: 128_000,
        fallback_models: &[],
        specialty: "creative",
        usage: "free · ?K",
    },
    FreeUpstream {
        id: "opencode-zen",
        title: "OpenCode Zen",
        key_url: "opencode.ai/auth",
        // Zen's free pool is dynamic. This is only the offline fallback;
        // live discovery selects the current `*-free` model from `/models`.
        default_model: "deepseek-v4-flash-free",
        model_family: "opencode-zen-free",
        note: "dynamic Zen free pool — current `*-free` model via /models",
        tool_calling: true,
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
        fallback_models: &[],
        specialty: "general purpose",
        usage: "2 keys · 8K",
    },
    FreeUpstream {
        id: "zai",
        title: "Z.AI",
        key_url: "z.ai/manage-apikey/apikey-list",
        // GLM-4.7 is paid ($0.60/$2.20 per 1M); the actually-free models are
        // GLM-4.7-Flash / GLM-4.5-Flash / GLM-4.6V-Flash (docs.z.ai pricing).
        default_model: "glm-4.7-flash",
        model_family: "glm-4.7-flash",
        note: "GLM-4.7-Flash (free) · GLM-4.5-Flash · GLM-5 — Zhipu AI international",
        tool_calling: true,
        vision: false,
        max_tokens_cap: Some(8_192),
        context_window: 128_000,
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
        vision: false,
        max_tokens_cap: None,
        context_window: 128_000,
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

/// One configured upstream's discovered free model list:
/// `(upstream_id, upstream_title, model_ids)` with the ids as callable wire
/// IDs in default-pick-first order.
pub type FreeModelListEntry = (String, String, Vec<String>);

/// Static storage for the most recently discovered FULL free model lists per
/// configured upstream. Populated by `build_free_provider` in registry.rs;
/// read by the TUI's Alt+J/K popup to list every currently-free model per
/// provider (model-first). Thread-safe via OnceLock.
static RECENT_FREE_MODEL_LISTS: OnceLock<Mutex<Vec<FreeModelListEntry>>> = OnceLock::new();
fn recent_free_model_lists() -> &'static Mutex<Vec<FreeModelListEntry>> {
    RECENT_FREE_MODEL_LISTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Set the discovered free model lists from a newly-built FreeProvider's
/// chain. Called by `build_free_provider` in registry.rs after constructing
/// the chain. The TUI reads these via [`take_free_model_lists`].
pub fn store_free_model_lists(lists: Vec<FreeModelListEntry>) {
    if let Ok(mut guard) = recent_free_model_lists().lock() {
        *guard = lists;
    }
}

/// Retrieve the stored free model lists as `(upstream_id, title, models)`
/// tuples (default pick first per upstream). Returns an empty vec if none
/// have been stored yet.
pub fn take_free_model_lists() -> Vec<FreeModelListEntry> {
    RECENT_FREE_MODEL_LISTS
        .get()
        .and_then(|m| m.lock().ok())
        .map(|guard| guard.clone())
        .unwrap_or_default()
}

/// Default model used for Cloudflare chat probes (must match the catalog's
/// `default_model` so validation exercises the same endpoint the chain uses).
pub(crate) const CLOUDFLARE_PROBE_MODEL: &str = "@cf/qwen/qwen3-30b-a3b-fp8";
