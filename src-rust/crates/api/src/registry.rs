// registry.rs — Registry of all available LLM providers.
//
// Holds an `Arc<dyn LlmProvider>` for each registered provider and exposes
// lookup, health-check, and default-provider helpers.

use std::collections::HashMap;
use std::sync::Arc;

use clawde_core::ProviderId;

use crate::client::ClientConfig;
use crate::provider::LlmProvider;
use crate::provider_types::ProviderStatus;
use crate::providers::{
    AnthropicProvider, AzureProvider, BedrockProvider, CodexProvider, CohereProvider,
    CopilotProvider, FreeEntry, FreeProvider, GoogleProvider, KeyRotatingProvider, MinimaxProvider,
    OpenAiProvider, RoutingConfig, FREE_CATALOG,
};

fn normalize_openai_compat_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{}/v1", trimmed)
    }
}

fn normalize_openai_base(override_base: &str) -> String {
    let trimmed = override_base.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.trim_end_matches("/v1").to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn resolve_provider_api_base(
    config: &clawde_core::config::Config,
    provider_id: &str,
) -> Option<String> {
    let base = config.resolve_provider_api_base(provider_id)?;
    if provider_id == "openai" {
        Some(normalize_openai_base(&base))
    } else if crate::providers::openai_compat_providers::provider_for_id(provider_id).is_some() {
        Some(normalize_openai_compat_base(&base))
    } else {
        Some(base)
    }
}

/// Registry of all available LLM providers.
/// Holds `Arc<dyn LlmProvider>` for each registered provider.
#[derive(Clone)]
pub struct ProviderRegistry {
    providers: HashMap<ProviderId, Arc<dyn LlmProvider>>,
    default_provider_id: ProviderId,
}

fn provider_from_key(provider_id: &str, key: String) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    // Cloudflare's OpenAI-compat endpoint embeds the account ID in the URL
    // path, so the stored key is the composite `ACCOUNT_ID:API_TOKEN` and
    // must be parsed by its dedicated factory rather than injected as a
    // plain Bearer token.
    if provider_id == "cloudflare" {
        return Some(Arc::new(p::cloudflare_with_key(&key)));
    }

    if let Some(provider) = p::provider_for_id(provider_id) {
        return Some(Arc::new(provider.with_api_key(key)));
    }

    match provider_id {
        "anthropic" => Some(Arc::new(AnthropicProvider::from_config(ClientConfig {
            api_key: key,
            ..Default::default()
        }))),
        "minimax" => Some(Arc::new(MinimaxProvider::new(key))),
        "openai" => Some(Arc::new(OpenAiProvider::new(key))),
        "google" => Some(Arc::new(GoogleProvider::new(key))),
        "github-copilot" => Some(Arc::new(CopilotProvider::new(key))),
        "codex" | "openai-codex" => {
            // The Codex provider is OAuth-based; the `key` field is not used.
            // Load from the stored token file instead.
            CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
        }
        "cohere" => Some(Arc::new(CohereProvider::new(key))),
        "custom-openai" => Some(Arc::new(p::custom_openai().with_api_key(key))),
        // "free" is handled by `build_free_provider` and `runtime_provider_for`
        // because it needs to iterate the full catalog, not a single key.
        _ => None,
    }
}

/// Build a [`FreeProvider`] by walking [`FREE_CATALOG`] and pulling any keys
/// the user has stored in the auth store. Each catalog entry whose upstream
/// has a key becomes one link in the fallback chain.
///
/// When an upstream has **multiple** keys in the auth store's multi-key store
/// (set via `set_keys` / `add_key`), that upstream is wrapped in a
/// [`KeyRotatingProvider`] so keys are automatically rotated on exhaustion.
/// Single-key entries work exactly as before.
///
/// Returns `None` only if *no* catalog entry has a configured key — a single
/// key is enough to run, and more is better.
pub fn build_free_provider(config: &clawde_core::config::Config) -> Option<Arc<dyn LlmProvider>> {
    let auth_store = clawde_core::AuthStore::load();
    let mut chain: Vec<FreeEntry> = Vec::new();

    // Parse optional routing config from `settings.json` → `providers.free.options.routing`.
    // If absent or malformed, the default (Sequential) is used.
    let routing = config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
        .and_then(|v| serde_json::from_value::<RoutingConfig>(v.clone()).ok());
    let disabled_upstreams: Vec<&str> = routing
        .as_ref()
        .map(|r| r.disabled_upstreams.iter().map(|s| s.as_str()).collect())
        .unwrap_or_default();

    // Auto-detect the best free model for each upstream from models.dev.
    // Falls back to the hardcoded default_model when models.dev is unreachable
    // or no free model is found for a given upstream.
    let auto_defaults = crate::providers::free::fetch_best_free_models_from_modelsdev();
    let effective_model = |upstream_id: &str| auto_defaults.get(upstream_id).cloned();

    for upstream in FREE_CATALOG {
        // Skip disabled upstreams even if they have keys configured.
        if disabled_upstreams.contains(&upstream.id) {
            continue;
        }
        // --- multi-key path (2+ keys → wrap in KeyRotatingProvider) ---
        // Uses the ring-aligned resolver so the ring's key order and slot
        // indices are exactly the list the health poller probes (it forwards
        // key_idx into these rings). OpenCode Zen/Go slot sharing and the
        // >=8-char placeholder filter live in that one helper.
        let multi_keys: Option<Vec<String>> =
            crate::providers::free::resolve_free_upstream_keys(&auth_store, upstream.id)
                .filter(|k| k.len() > 1);

        if let Some(keys) = multi_keys {
            let upstream_id = upstream.id.to_string();
            let upstream_name = upstream.title.to_string();
            let mut rotating = KeyRotatingProvider::new_with_persistence(
                upstream_id.clone(),
                upstream_name,
                keys,
                move |key| {
                    let key_owned = key.to_string();
                    match upstream_id.as_str() {
                        "google" => {
                            Arc::new(GoogleProvider::new(key_owned)) as Arc<dyn LlmProvider>
                        }
                        "cohere" => {
                            Arc::new(CohereProvider::new(key_owned)) as Arc<dyn LlmProvider>
                        }
                        "cloudflare" => Arc::new(
                            crate::providers::openai_compat_providers::cloudflare_with_key(
                                &key_owned,
                            ),
                        ) as Arc<dyn LlmProvider>,
                        id => {
                            let p = crate::providers::openai_compat_providers::provider_for_id(id)
                                .unwrap_or_else(|| {
                                    panic!("KeyRotatingProvider: no upstream factory for '{}'", id)
                                });
                            Arc::new(p.with_api_key(key_owned)) as Arc<dyn LlmProvider>
                        }
                    }
                },
            );
            // The FreeProvider already handles fallback at a higher level —
            // disable the per-upstream recovery loop so that an exhausted
            // key returns immediately instead of sleeping/retrying.
            rotating.set_skip_recovery_loop(true);
            chain.push(FreeEntry {
                upstream: *upstream,
                provider: Arc::new(rotating),
                effective_model: effective_model(upstream.id),
            });
            continue;
        }

        // --- single-key path ---
        // Same validation as the multi-key resolver (trim + >=8 placeholder
        // guard, OpenCode Zen/Go slot sharing) via first_free_upstream_key.
        let Some(key) = crate::providers::free::first_free_upstream_key(&auth_store, upstream.id)
        else {
            continue;
        };

        let provider: Option<Arc<dyn LlmProvider>> = match upstream.id {
            "google" => Some(Arc::new(GoogleProvider::new(key))),
            "cohere" => Some(Arc::new(CohereProvider::new(key))),
            "cloudflare" => Some(Arc::new(
                crate::providers::openai_compat_providers::cloudflare_with_key(&key),
            ) as Arc<dyn LlmProvider>),
            "github-copilot" => Some(Arc::new(CopilotProvider::new(key)) as Arc<dyn LlmProvider>),
            id => crate::providers::openai_compat_providers::provider_for_id(id)
                .map(|p| Arc::new(p.with_api_key(key)) as Arc<dyn LlmProvider>),
        };

        if let Some(provider) = provider {
            chain.push(FreeEntry {
                upstream: *upstream,
                provider,
                effective_model: effective_model(upstream.id),
            });
        }
    }

    // Run live free-model discovery for any upstream that supports it.
    // This is the extensible pattern for providers like Cline whose free
    // models change frequently and can be queried via a live API endpoint.
    // To add a new provider: add a variant to FreeModelDiscovery, wire it
    // in discovery_for() and run_live_discovery() in free.rs.
    for entry in &mut chain {
        let upstream_id = entry.upstream.id;
        if let Some(free_model) =
            crate::providers::free::run_live_discovery(upstream_id, &auth_store)
        {
            entry.effective_model = Some(free_model);
        }
    }

    // When Ollama is in Auto mode, append it to the free-model fallback
    // chain as a local last-resort provider. No API key needed — it uses
    // the already-built Ollama provider from the registry. In Isolated
    // mode Ollama stays out of the free chain entirely.
    if config.resolve_ollama_mode() == clawde_core::OllamaMode::Auto {
        let ollama_provider = crate::providers::ollama();
        chain.push(FreeEntry {
            upstream: crate::providers::FreeUpstream {
                id: "ollama",
                title: "Ollama",
                key_url: "ollama.com",
                default_model: "llama3.2",
                note: "local fallback — no key needed",
                tool_calling: true,
                vision: false,
                max_tokens_cap: Some(4_096),
                context_window: 8_192,
                fallback_models: &[],
                model_family: "llama3.2",
                specialty: "local",
                usage: "local · 4K",
            },
            provider: Arc::new(ollama_provider),
            effective_model: Some("llama3.2".to_string()),
        });
    }

    if chain.is_empty() {
        return None;
    }
    let provider = FreeProvider::with_routing(
        chain,
        routing.unwrap_or_default(),
        FreeProvider::ENABLE_EMPTY_COOLDOWN_PERSISTENCE,
    );
    // Store free model defaults for the TUI /ctx-viz overlay
    crate::providers::free::store_free_model_defaults(provider.free_model_defaults());
    Some(Arc::new(provider) as Arc<dyn LlmProvider>)
}

pub fn provider_from_config(
    config: &clawde_core::config::Config,
    provider_id: &str,
) -> Option<Arc<dyn LlmProvider>> {
    let provider_cfg = config.provider_configs.get(provider_id);
    if provider_cfg.is_some_and(|provider| !provider.enabled) {
        return None;
    }

    let api_key = config.resolve_provider_api_key(provider_id);
    let api_base = resolve_provider_api_base(config, provider_id).filter(|base| !base.is_empty());

    use crate::providers;

    match provider_id {
        "anthropic" => None,
        // Composite "Free" provider — two keys are pulled internally from the
        // auth store; the `api_key` resolved above is ignored.
        "free" => build_free_provider(config),
        "openai" => {
            let mut provider = OpenAiProvider::new(api_key.unwrap_or_default());
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "google" => api_key.map(|key| Arc::new(GoogleProvider::new(key)) as Arc<dyn LlmProvider>),
        "minimax" => api_key.map(|key| {
            let mut provider = MinimaxProvider::new(key);
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            if let Some(service_tier) = provider_cfg
                .and_then(|config| config.options.get("service_tier"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
            {
                provider = provider.with_service_tier(service_tier);
            }
            Arc::new(provider) as Arc<dyn LlmProvider>
        }),
        "azure" => {
            let resource_name = provider_cfg
                .and_then(|provider| provider.options.get("resource_name"))
                .and_then(|value| value.as_str())
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| {
                    std::env::var("AZURE_RESOURCE_NAME")
                        .ok()
                        .filter(|value| !value.is_empty())
                });

            match (resource_name, api_key) {
                (Some(resource_name), Some(key)) => {
                    Some(Arc::new(AzureProvider::new(resource_name, key)) as Arc<dyn LlmProvider>)
                }
                _ => None,
            }
        }
        "ollama" => {
            let mut provider = providers::ollama();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "lmstudio" | "lm-studio" => {
            let mut provider = providers::lm_studio();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "llamacpp" | "llama-cpp" | "llama-server" => {
            let mut provider = providers::llama_cpp();
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "deepseek" => {
            let mut provider = providers::deepseek();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "groq" => {
            let mut provider = providers::groq();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "xai" => {
            let mut provider = providers::xai();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "openrouter" => {
            let mut provider = providers::openrouter();
            if let Some(key) = api_key {
                provider = provider.with_api_key(key);
            }
            if let Some(base) = api_base {
                provider = provider.with_base_url(base);
            }
            Some(Arc::new(provider))
        }
        "cohere" => api_key.map(|key| Arc::new(CohereProvider::new(key)) as Arc<dyn LlmProvider>),
        "github-copilot" => {
            // Try env/api_key first, fall back to stored OAuth token from /connect.
            api_key
                .map(|key| Arc::new(CopilotProvider::new(key)) as Arc<dyn LlmProvider>)
                .or_else(|| {
                    CopilotProvider::from_auth_store().map(|p| Arc::new(p) as Arc<dyn LlmProvider>)
                })
        }
        "codex" | "openai-codex" => {
            CodexProvider::from_stored().map(|provider| Arc::new(provider) as Arc<dyn LlmProvider>)
        }
        _ => api_key.and_then(|key| provider_from_key(provider_id, key)),
    }
}

pub fn runtime_provider_for(provider_id: &str) -> Option<Arc<dyn LlmProvider>> {
    use crate::providers::openai_compat_providers as p;

    // Local providers never require an API key — build them directly so that
    // the auth-store bypass below doesn't silently drop them.
    // Accept both the hyphenated canonical IDs ("llama-cpp", "lm-studio") and
    // the non-hyphenated aliases ("llamacpp", "lmstudio") used throughout the
    // TUI / connect dialog.
    match provider_id {
        "ollama" => return Some(Arc::new(p::ollama())),
        "lmstudio" | "lm-studio" => return Some(Arc::new(p::lm_studio())),
        // "llama-server" is the binary name for the modern llama.cpp server.
        "llamacpp" | "llama-cpp" | "llama-server" => return Some(Arc::new(p::llama_cpp())),
        "codex" | "openai-codex" => {
            return CodexProvider::from_stored().map(|p| Arc::new(p) as Arc<dyn LlmProvider>);
        }
        // "free" pulls two keys (Zen + OpenRouter) from the auth store and
        // wraps them in a fallback composite — handled here so the generic
        // single-key path below doesn't short-circuit on a missing key.
        // Load the settings config so routing strategy can be threaded through.
        "free" => {
            let cfg = clawde_core::config::Settings::load_sync()
                .map(|s| s.effective_config())
                .unwrap_or_default();
            return build_free_provider(&cfg);
        }
        _ => {}
    }

    let auth_store = clawde_core::AuthStore::load();

    // Check for multi-key setup first: when 2+ keys are configured, wrap
    // the provider in a KeyRotatingProvider for automatic rotation.
    if let Some(keys) = auth_store.keys_for(provider_id) {
        // Cloud API keys are always at least 8 characters. Shorter values
        // are placeholders or test artifacts that would fail with AuthFailed
        // and poison the whole rotation pool (see key_ring.rs).
        let real_keys: Vec<String> = keys
            .iter()
            .map(|k| k.trim().to_string())
            .filter(|k| k.len() >= 8)
            .collect();
        if real_keys.len() > 1 {
            let pid_owned = provider_id.to_string();
            let rotating = KeyRotatingProvider::new_with_persistence(
                pid_owned.clone(),
                pid_owned.clone(),
                real_keys,
                move |key| {
                    provider_from_key(&pid_owned, key.to_string())
                        .expect("runtime_provider_for: provider_from_key failed")
                },
            );
            return Some(Arc::new(rotating));
        }
    }

    let key = auth_store.api_key_for(provider_id)?;
    if key.is_empty() {
        return None;
    }
    // Cloud API keys are always at least 8 characters. Shorter values are
    // placeholders or test artifacts that would fail with AuthFailed.
    if key.trim().len() < 8 {
        return None;
    }
    provider_from_key(provider_id, key)
}

/// Type alias for the empty-cooldown summaries returned by [`ProviderRegistry::empty_cooldown_summaries`].
pub type EmptyCooldownSummaries = Vec<(String, Vec<(String, u32, Option<u64>)>)>;

/// Type alias for per-upstream key-health summaries returned by
/// [`ProviderRegistry::upstream_key_health_summaries`].
pub type UpstreamKeyHealthSummaries = Vec<(String, Vec<(String, usize, usize, Option<u64>)>)>;

/// Type alias for per-upstream cooldown summaries returned by
/// [`ProviderRegistry::upstream_cooldown_summaries`].
pub type UpstreamCooldownSummaries = Vec<(String, Vec<(String, String, Option<u64>)>)>;

impl ProviderRegistry {
    /// Create an empty registry with Anthropic as the default provider ID.
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            default_provider_id: ProviderId::new(ProviderId::ANTHROPIC),
        }
    }

    /// Rebuild the "free" composite provider from the current config and
    /// register (or remove) it in-place.  This is safe to call at runtime
    /// so that `/ollama` and similar toggles take effect immediately.
    pub fn rebuild_free(&mut self, config: &clawde_core::config::Config) {
        let free_id = ProviderId::new("free");
        if let Some(new_free) = build_free_provider(config) {
            self.providers.insert(free_id, new_free);
        } else {
            self.providers.remove(&free_id);
        }
    }

    /// Register a provider. Returns `&mut self` for builder chaining.
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) -> &mut Self {
        let id = provider.id().clone();
        self.providers.insert(id, provider);
        self
    }

    /// Set the default provider by ID.
    ///
    /// # Panics
    /// Panics if no provider with that ID has been registered.
    pub fn set_default(&mut self, id: ProviderId) -> &mut Self {
        assert!(
            self.providers.contains_key(&id),
            "set_default: provider '{}' is not registered",
            id,
        );
        self.default_provider_id = id;
        self
    }

    /// Get a provider by ID.
    pub fn get(&self, id: &ProviderId) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(id)
    }

    /// Get the default provider.
    pub fn default_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.get(&self.default_provider_id)
    }

    /// Get the default provider ID.
    pub fn default_provider_id(&self) -> &ProviderId {
        &self.default_provider_id
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<&ProviderId> {
        self.providers.keys().collect()
    }

    /// Collect key-ring summaries from all registered providers that support
    /// automatic key rotation. Each entry is `(provider_name, active_count,
    /// total_keys, earliest_retry_secs)` where `earliest_retry_secs` is the
    /// seconds until the next key becomes available, or `None` when all keys
    /// are active.
    pub fn key_ring_summaries(&self) -> Vec<(String, usize, usize, Option<u64>)> {
        let mut summaries = Vec::new();
        for (id, provider) in &self.providers {
            if let Some((active, total, retry)) = provider.key_ring_status() {
                if total > 0 {
                    summaries.push((id.to_string(), active, total, retry));
                }
            }
        }
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Collect empty-completion cooldown summaries from all registered
    /// providers that multiplex upstreams. Each entry is
    /// `(provider_name, Vec<(upstream_id, consecutive_empties, retry_secs)>)`
    /// and only includes upstreams with at least one recorded empty
    /// completion. Sorted by provider name.
    pub fn empty_cooldown_summaries(&self) -> EmptyCooldownSummaries {
        let mut summaries = Vec::new();
        for (id, provider) in &self.providers {
            let entries = provider.upstream_empty_cooldowns();
            if !entries.is_empty() {
                summaries.push((id.to_string(), entries));
            }
        }
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Collect per-upstream key-ring health from all registered composite
    /// providers. Each entry is `(provider_name, Vec<(upstream_id,
    /// active_keys, total_keys, retry_secs)>)` and only includes providers
    /// that report at least one upstream with a key ring. Sorted by provider
    /// name.
    pub fn upstream_key_health_summaries(&self) -> UpstreamKeyHealthSummaries {
        let mut summaries = Vec::new();
        for (id, provider) in &self.providers {
            let entries = provider.upstream_key_health();
            if !entries.is_empty() {
                summaries.push((id.to_string(), entries));
            }
        }
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Collect per-upstream cooldown state (empty-completion + 5xx /
    /// circuit-breaker) from all registered composite providers. Each entry
    /// is `(provider_name, Vec<(upstream_id, kind, retry_secs)>)` where
    /// `kind` is `"empty"` or `"5xx"`. Sorted by provider name.
    pub fn upstream_cooldown_summaries(&self) -> UpstreamCooldownSummaries {
        let mut summaries = Vec::new();
        for (id, provider) in &self.providers {
            let entries = provider.upstream_cooldowns();
            if !entries.is_empty() {
                summaries.push((id.to_string(), entries));
            }
        }
        summaries.sort_by(|a, b| a.0.cmp(&b.0));
        summaries
    }

    /// Check health of all providers sequentially.
    /// Returns `(provider_id, status)` pairs.
    pub async fn check_all_health(&self) -> Vec<(ProviderId, ProviderStatus)> {
        let mut results = Vec::new();
        for (id, provider) in &self.providers {
            let status = provider
                .health_check()
                .await
                .unwrap_or(ProviderStatus::Unavailable {
                    reason: "health check failed".to_string(),
                });
            results.push((id.clone(), status));
        }
        results
    }

    /// Convenience: build a registry with just Anthropic registered as the
    /// default provider.  Takes the same [`ClientConfig`] that
    /// [`AnthropicClient`] takes.
    ///
    /// [`AnthropicClient`]: crate::client::AnthropicClient
    pub fn with_anthropic(config: ClientConfig) -> Self {
        let mut registry = Self::new();
        let provider = Arc::new(AnthropicProvider::from_config(config));
        registry.register(provider);
        registry
    }

    pub fn from_config(
        config: &clawde_core::config::Config,
        anthropic_config: ClientConfig,
    ) -> Self {
        // Apply the user-configured request timeout (issue #175) before any
        // provider HTTP clients are built, so they all honour it. Uses the
        // active provider's resolved value (per-provider override or global).
        crate::set_request_timeout_secs(
            config.resolve_request_timeout_secs(config.selected_provider_id()),
        );
        let mut registry = Self::from_environment_with_auth_store(anthropic_config);
        let active_provider = config.selected_provider_id();

        let mut configured_provider_ids: Vec<String> =
            config.provider_configs.keys().cloned().collect();
        if configured_provider_ids
            .iter()
            .all(|id| id != active_provider)
        {
            configured_provider_ids.push(active_provider.to_string());
        }

        for provider_id in configured_provider_ids {
            if let Some(provider) = provider_from_config(config, &provider_id) {
                registry.register(provider);
            }
        }

        let default_provider_id = ProviderId::new(active_provider);
        if registry.get(&default_provider_id).is_some() {
            registry.set_default(default_provider_id);
        }

        registry
    }

    /// Register providers from the auth store's multi-key store (`keys` map)
    /// that weren't already registered from credentials or env vars.
    ///
    /// Providers with 2+ keys get wrapped in a [`KeyRotatingProvider`] for
    /// automatic key rotation on exhaustion. Single-key entries are registered
    /// directly as normal providers.
    fn register_key_store_providers(&mut self, auth_store: &clawde_core::AuthStore) {
        for provider_id in auth_store.keys.keys() {
            let pid = clawde_core::ProviderId::new(provider_id);
            if self.get(&pid).is_some() {
                continue;
            }

            let Some(keys) = auth_store.keys_for(provider_id) else {
                continue;
            };

            if keys.len() > 1 {
                let keys_vec: Vec<String> = keys.to_vec();
                let pid_owned = provider_id.clone();
                let rotating = KeyRotatingProvider::new_with_persistence(
                    pid_owned.clone(),
                    pid_owned.clone(),
                    keys_vec,
                    move |key| {
                        provider_from_key(&pid_owned, key.to_string())
                            .expect("KeyRotatingProvider: provider_from_key failed")
                    },
                );
                self.register(Arc::new(rotating));
            } else if let Some(key) = keys.first() {
                if let Some(p) = provider_from_key(provider_id, key.clone()) {
                    self.register(p);
                }
            }
        }
    }

    /// Register [`GoogleProvider`] if `GOOGLE_API_KEY` or
    /// `GOOGLE_GENERATIVE_AI_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_google_if_key_set(&mut self) -> &mut Self {
        let key = std::env::var("GOOGLE_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_GENERATIVE_AI_API_KEY"));
        if let Ok(key) = key {
            let provider = Arc::new(GoogleProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`OpenAiProvider`] if `OPENAI_API_KEY` is set in the
    /// environment.  Returns `&mut self` for builder chaining.
    pub fn with_openai_if_key_set(&mut self) -> &mut Self {
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let provider = Arc::new(OpenAiProvider::new(key));
            self.register(provider);
        }
        self
    }

    /// Register [`AzureProvider`] if `AZURE_API_KEY` and `AZURE_RESOURCE_NAME`
    /// are set in the environment.  Returns `&mut self` for builder chaining.
    pub fn with_azure_if_configured(&mut self) -> &mut Self {
        if let Some(p) = AzureProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`BedrockProvider`] if AWS credentials are available in the
    /// environment (`AWS_ACCESS_KEY_ID`+`AWS_SECRET_ACCESS_KEY` or
    /// `AWS_BEARER_TOKEN_BEDROCK`).  Returns `&mut self` for builder chaining.
    pub fn with_bedrock_if_configured(&mut self) -> &mut Self {
        if let Some(p) = BedrockProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CopilotProvider`] if `GITHUB_TOKEN` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_copilot_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CopilotProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CodexProvider`] if stored Codex OAuth tokens are available in
    /// `~/.claurst/codex_tokens.json`.  Returns `&mut self` for builder chaining.
    pub fn with_codex_if_configured(&mut self) -> &mut Self {
        if let Some(p) = CodexProvider::from_stored() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Register [`CohereProvider`] if `COHERE_API_KEY` is set in the environment.
    /// Returns `&mut self` for builder chaining.
    pub fn with_cohere_if_key_set(&mut self) -> &mut Self {
        if let Some(p) = CohereProvider::from_env() {
            self.register(Arc::new(p));
        }
        self
    }

    /// Build a registry with **all** providers that have credentials configured
    /// in the environment.  Anthropic is always the default provider.
    ///
    /// This is the recommended constructor for production use.
    pub fn from_environment(anthropic_config: ClientConfig) -> Self {
        let mut registry = Self::with_anthropic(anthropic_config);
        registry
            .with_openai_if_key_set()
            .with_google_if_key_set()
            .with_azure_if_configured()
            .with_bedrock_if_configured()
            .with_copilot_if_configured()
            .with_codex_if_configured()
            .with_cohere_if_key_set()
            .with_available_providers();
        registry
    }

    /// Build a registry that checks **both** environment variables and the
    /// persistent [`AuthStore`] (`~/.claurst/auth.json`) for credentials.
    ///
    /// This ensures that API keys stored via `/connect` or `clawde auth` are
    /// picked up at startup, not just env vars.  Falls back to
    /// `from_environment` for providers that only support env-var config, and
    /// adds any extra providers that have keys in the auth store.
    ///
    /// [`AuthStore`]: clawde_core::AuthStore
    pub fn from_environment_with_auth_store(anthropic_config: ClientConfig) -> Self {
        // Start with env-based registration.
        let mut registry = Self::from_environment(anthropic_config);

        // Now check the auth store for providers that weren't registered from
        // env vars.
        let auth_store = clawde_core::AuthStore::load();

        for provider_id in auth_store.credentials.keys() {
            let pid = clawde_core::ProviderId::new(provider_id.as_str());
            // Skip if already registered from env vars.
            if registry.get(&pid).is_some() {
                continue;
            }
            // Try to get a usable key from the auth store.
            if let Some(key) = auth_store.api_key_for(provider_id) {
                if key.is_empty() {
                    continue;
                }
                let provider = provider_from_key(provider_id, key);
                if let Some(p) = provider {
                    registry.register(p);
                }
            }
        }

        // Register multi-key providers from the keys store (not in credentials).
        registry.register_key_store_providers(&auth_store);

        registry
    }

    /// Register all providers that have environment variable credentials set.
    ///
    /// Local providers (Ollama, LM Studio, llama.cpp) are always registered
    /// regardless of credentials — `health_check()` will report them as
    /// unavailable if the server is not running.
    ///
    /// Remote API-key providers are only registered when their respective
    /// environment variables are set (non-empty).
    ///
    /// Returns `&mut self` for builder chaining.
    pub fn with_available_providers(&mut self) -> &mut Self {
        use crate::providers::openai_compat_providers as p;

        // Local providers — always try to register.
        self.register(Arc::new(p::ollama()));
        self.register(Arc::new(p::lm_studio()));
        self.register(Arc::new(p::llama_cpp()));

        // Remote providers — only register when an API key is present.
        if std::env::var("DEEPSEEK_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::deepseek()));
        }
        if std::env::var("GROQ_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::groq()));
        }
        if std::env::var("XAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::xai()));
        }
        if std::env::var("OPENROUTER_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::openrouter()));
        }
        if std::env::var("TOGETHER_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::together_ai()));
        }
        if std::env::var("PERPLEXITY_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::perplexity()));
        }
        if std::env::var("CEREBRAS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::cerebras()));
        }
        if std::env::var("DEEPINFRA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::deepinfra()));
        }
        if std::env::var("VENICE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::venice()));
        }
        if std::env::var("DASHSCOPE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::qwen()));
        }
        if std::env::var("MISTRAL_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::mistral()));
        }
        if std::env::var("SAMBANOVA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::sambanova()));
        }
        if std::env::var("HF_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::huggingface()));
        }
        if std::env::var("MINIMAX_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            let key = std::env::var("MINIMAX_API_KEY").unwrap_or_default();
            self.register(Arc::new(MinimaxProvider::new(key)));
        }
        if std::env::var("NVIDIA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::nvidia()));
        }
        if std::env::var("SILICONFLOW_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::siliconflow()));
        }
        if std::env::var("MOONSHOT_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::moonshot()));
        }
        if std::env::var("ZHIPU_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::zhipu()));
        }
        if std::env::var("ZAI_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::zai()));
        }
        if std::env::var("NEBIUS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::nebius()));
        }
        if std::env::var("NOVITA_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::novita()));
        }
        if std::env::var("OVHCLOUD_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::ovhcloud()));
        }
        if std::env::var("SCALEWAY_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::scaleway()));
        }
        if std::env::var("VULTR_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::vultr_ai()));
        }
        if std::env::var("BASETEN_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::baseten()));
        }
        if std::env::var("FRIENDLI_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::friendli()));
        }
        if std::env::var("UPSTAGE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::upstage()));
        }
        if std::env::var("STEPFUN_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::stepfun()));
        }
        if std::env::var("FIREWORKS_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::fireworks()));
        }
        if std::env::var("OPENCODE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::opencode_go()));
        }
        if std::env::var("CLINE_API_KEY")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::cline()));
        }
        // Cloudflare Workers AI — token env var set (account ID either in
        // CLOUDFLARE_ACCOUNT_ID or as the composite ACCOUNT_ID:API_TOKEN).
        if std::env::var("CLOUDFLARE_API_TOKEN")
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            self.register(Arc::new(p::cloudflare()));
        }
        self
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests — routing-strategy wiring through the /refresh rebuild path
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::config::{Config, ProviderConfig};

    /// Build a [`Config`] whose free-provider routing options carry the given
    /// strategy JSON, mirroring what `providers.free.options.routing` looks
    /// like after `/routing <strategy>` writes it to settings.json.
    fn config_with_routing_strategy(strategy: &str) -> Config {
        let mut options = std::collections::HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({ "strategy": strategy }),
        );
        let mut provider_configs = std::collections::HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            ProviderConfig {
                options,
                ..Default::default()
            },
        );
        Config {
            provider_configs,
            ..Default::default()
        }
    }

    /// Seed a fake key for an upstream that has no live model discovery
    /// (mistral → `FreeModelDiscovery::None`) so `build_free_provider` builds
    /// its chain without a live-discovery call. (The unconditional
    /// `fetch_best_free_models_from_modelsdev` OnceLock may still attempt one
    /// HTTP fetch per test process; it degrades to an empty map on failure.)
    fn seed_key(store: &mut clawde_core::AuthStore) {
        store.set(
            "mistral",
            clawde_core::StoredCredential::ApiKey {
                key: "fake-mistral-key-1234567890".to_string(),
            },
        );
    }

    #[test]
    fn build_free_provider_applies_auto_strategy_from_config() {
        // /refresh rebuilds the registry from the in-memory config that
        // /routing auto returned via ConfigChangeMessage — this asserts that
        // the rebuild actually applies the strategy (spec §8.4), so switching
        // /routing auto takes effect in-session without a restart.
        let (mut store, _home) = crate::test_support::test_auth_store();
        seed_key(&mut store);
        let config = config_with_routing_strategy("auto");

        let provider = build_free_provider(&config)
            .expect("a seeded free-mode key should build the free chain");
        assert_eq!(
            provider.routing_strategy_name(),
            Some("Auto"),
            "rebuilt free provider must use the Auto strategy from the config"
        );
    }

    #[test]
    fn build_free_provider_respects_explicit_sequential_strategy() {
        // An explicit `sequential` in settings.json survives the rebuild —
        // only configs WITHOUT a strategy key get the Auto default.
        let (mut store, _home) = crate::test_support::test_auth_store();
        seed_key(&mut store);
        let config = config_with_routing_strategy("sequential");

        let provider = build_free_provider(&config)
            .expect("a seeded free-mode key should build the free chain");
        assert_eq!(
            provider.routing_strategy_name(),
            Some("Seq"),
            "explicit sequential strategy must survive the rebuild"
        );
    }

    #[test]
    fn build_free_provider_defaults_to_auto_without_strategy_key() {
        // No routing key at all → the Auto default (smart routing, §8.4).
        let (mut store, _home) = crate::test_support::test_auth_store();
        seed_key(&mut store);
        let config = Config::default();

        let provider = build_free_provider(&config)
            .expect("a seeded free-mode key should build the free chain");
        assert_eq!(
            provider.routing_strategy_name(),
            Some("Auto"),
            "missing strategy key must fall back to the Auto default"
        );
    }
}
