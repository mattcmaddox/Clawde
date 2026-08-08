// provider.rs — Core trait definitions for the provider abstraction layer.
//
// Every LLM provider adapter must implement `LlmProvider`.  The trait is
// intentionally minimal: only what is needed to send messages, list models,
// and report capabilities.  Auth concerns live in `auth.rs`.

use async_trait::async_trait;
use clawde_core::provider_id::{ModelId, ProviderId};
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

use crate::error_handling::parse_error_response;
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
};

// ---------------------------------------------------------------------------
// ModelInfo
// ---------------------------------------------------------------------------

/// Static metadata about a model available through a provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    /// The model's unique identifier (e.g. `"claude-opus-4-5"`).
    pub id: ModelId,

    /// The provider that hosts this model.
    pub provider_id: ProviderId,

    /// Human-readable display name (e.g. `"Claude Opus 4.5"`).
    pub name: String,

    /// Total context window size in tokens.
    pub context_window: u32,

    /// Maximum number of tokens the model can emit in a single response.
    pub max_output_tokens: u32,

    /// First public availability (ISO 8601 date), when known.  Catalog-backed
    /// providers populate this from the models.dev snapshot; live-discovery
    /// providers may leave it `None`.  Drives date-DESC listing in pickers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,

    /// Lifecycle status string (`"active"`, `"beta"`, …), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

impl Default for ModelInfo {
    /// Empty placeholder used with struct-update syntax
    /// (`ModelInfo { id, .., ..Default::default() }`) so the optional metadata
    /// fields don't have to be repeated at every construction site.
    fn default() -> Self {
        Self {
            id: ModelId::new(""),
            provider_id: ProviderId::new(""),
            name: String::new(),
            context_window: 0,
            max_output_tokens: 0,
            release_date: None,
            status: None,
        }
    }
}

// ---------------------------------------------------------------------------
// LlmProvider
// ---------------------------------------------------------------------------

/// The core trait every LLM provider adapter must implement.
///
/// Implementors are required to be `Send + Sync` so they can be held behind an
/// `Arc<dyn LlmProvider>` and shared across async tasks.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Unique machine-readable identifier, e.g. `"anthropic"`, `"openai"`.
    fn id(&self) -> &ProviderId;

    /// Human-readable display name, e.g. `"Anthropic"`, `"OpenAI"`.
    fn name(&self) -> &str;

    /// Send a message and receive a complete (non-streaming) response.
    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError>;

    /// Send a message and receive a streaming response as a pinned `Stream` of
    /// provider-agnostic `StreamEvent`s.
    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>;

    /// Discover models exposed by a *live* endpoint (e.g. `GET /v1/models` for
    /// a local Ollama/LM Studio server, or a Copilot entitlement query).
    ///
    /// Catalog-backed providers (Anthropic, OpenAI, Google, …) do **not**
    /// override this: their model list is a read-only projection of the
    /// models.dev catalog held in [`crate::ModelRegistry`], so the picker never
    /// turns a provider return value into the displayed list.  The default impl
    /// therefore returns an empty vector — only providers whose models cannot be
    /// Map an HTTP error status and response body to a [`ProviderError`].
    ///
    /// The default delegates to [`parse_error_response`]; providers with
    /// custom error formats (e.g. Cohere) override this.
    fn map_http_error(&self, status: u16, body: &str) -> ProviderError {
        parse_error_response(status, body, self.id())
    }

    /// Discover models exposed by a *live* endpoint (e.g. `GET /v1/models` for
    /// a local Ollama/LM Studio server, or a Copilot entitlement query).
    ///
    /// Catalog-backed providers (Anthropic, OpenAI, Google, …) do **not**
    /// override this: their model list is a read-only projection of the
    /// models.dev catalog held in [`crate::ModelRegistry`], so the picker never
    /// turns a provider return value into the displayed list.  The default impl
    /// therefore returns an empty vector — only providers whose models cannot be
    /// known ahead of time (local runtimes, dynamic gateways) implement it.
    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }

    /// Check whether the provider is authenticated and reachable.
    ///
    /// Typically involves a lightweight API call (e.g. listing models or
    /// fetching account info).  Should not be called on the hot path.
    async fn health_check(&self) -> Result<ProviderStatus, ProviderError>;

    /// Return the static capabilities of this provider.
    ///
    /// This must not make a network call — it describes the provider's known
    /// feature set as compiled in.
    fn capabilities(&self) -> ProviderCapabilities;

    /// Return whether a specific model supports tool/function calling.
    ///
    /// Returns `None` when the provider doesn't have per-model differentiation
    /// (callers fall back to [`capabilities().tool_calling`](Self::capabilities)).
    /// Returns `Some(bool)` when the provider can answer for a specific model.
    ///
    /// Compositing providers (e.g. `FreeProvider`) override this to give an
    /// accurate answer for the currently-routed upstream rather than the union
    /// of all upstreams.
    fn tool_calling_for(&self, _model: &str) -> Option<bool> {
        None
    }

    /// Return the max output tokens cap for a specific model, if one exists.
    ///
    /// Returns `None` when the provider doesn't have a per-model cap (callers
    /// use the request's `max_tokens` as-is). Returns `Some(cap)` when the
    /// provider knows the model has a lower output ceiling.
    fn max_tokens_cap_for(&self, _model: &str) -> Option<u32> {
        None
    }

    /// Return key rotation status if this provider supports automatic key
    /// rotation. Returns `(active_count, total_keys, earliest_retry_secs)`:
    /// - `active_count`: number of keys not in cooldown
    /// - `total_keys`: total number of keys in the ring
    /// - `earliest_retry_secs`: seconds until the next key becomes available,
    ///    or `None` when all keys are active (no cooldowns).
    ///
    /// The default implementation returns `None` — most providers do not
    /// have a key ring.
    fn key_ring_status(&self) -> Option<(usize, usize, Option<u64>)> {
        None
    }

    /// Return the name of the active routing strategy, if this provider
    /// supports multiple routing strategies (e.g. sequential, random,
    /// latency-based). The default implementation returns `None`.
    fn routing_strategy_name(&self) -> Option<&'static str> {
        None
    }

    /// Report per-upstream empty-completion cooldown state (spec §6.3).
    /// Each entry is `(upstream_id, consecutive_empties, retry_secs)` where
    /// `retry_secs` is the seconds remaining in the empty-completion
    /// cooldown, or `None` when the upstream is not currently cooled for
    /// empty completions. Only upstreams that have recorded at least one
    /// empty completion are listed.
    ///
    /// The default implementation returns an empty vector — most providers
    /// do not multiplex multiple upstreams. Composite providers (e.g.
    /// `FreeProvider`) override this so the TUI status display and
    /// `/keys health` can show which upstreams are cooled down for empty
    /// completions.
    fn upstream_empty_cooldowns(&self) -> Vec<(String, u32, Option<u64>)> {
        Vec::new()
    }

    /// Report per-upstream key-ring health for composite providers.
    /// Each entry is `(upstream_id, active_keys, total_keys, retry_secs)`
    /// where `retry_secs` is the seconds until the earliest exhausted key
    /// recovers, or `None` when all keys are active. The default
    /// implementation returns an empty vector — only composite providers
    /// that multiplex upstreams (e.g. `FreeProvider`) override this.
    fn upstream_key_health(&self) -> Vec<(String, usize, usize, Option<u64>)> {
        Vec::new()
    }

    /// Report per-upstream cooldown state for composite providers.
    /// Each entry is `(upstream_id, kind, retry_secs)` where `kind` is
    /// `"empty"` (empty-completion cooldown) or `"5xx"` (server-error /
    /// circuit-breaker cooldown) and `retry_secs` is the seconds remaining
    /// in the cooldown. The default implementation returns an empty vector
    /// — only composite providers that multiplex upstreams override this.
    fn upstream_cooldowns(&self) -> Vec<(String, String, Option<u64>)> {
        Vec::new()
    }

    /// Per-upstream historical average latency in seconds, for the routing
    /// dialog's model-performance view (spec §8.6). `None` means no samples
    /// recorded yet. The default returns an empty vector — only composite
    /// providers that multiplex upstreams override this.
    fn upstream_latencies(&self) -> Vec<(String, Option<f64>)> {
        Vec::new()
    }

    /// Per-upstream capability metadata for the routing dialog's model view
    /// (spec §8.6): `(upstream_id, vision, context_window_tokens)`. Lets the
    /// UI explain why the capability gate (spec §8.4) routes image-bearing or
    /// oversized requests away from some upstreams. The default returns an
    /// empty vector — only composite providers that multiplex upstreams
    /// override this.
    fn upstream_capabilities(&self) -> Vec<(String, bool, u32)> {
        Vec::new()
    }

    /// Inject an external key exhaustion signal into the provider's key ring
    /// (e.g. from the health poller — spec §6.4).  Returns `true` if the
    /// key was marked exhausted; `false` when this provider has no key ring
    /// or the index is out of bounds.
    ///
    /// The default implementation returns `false` — only providers that
    /// support automatic key rotation (e.g. `KeyRotatingProvider`) override
    /// this.
    /// Clear an externally-injected key exhaustion (e.g. the health poller
    /// has confirmed the key is healthy again after a previous definitive
    /// failure — spec §6.4).  Returns `true` if the key's cooldown was
    /// cleared; `false` when this provider has no key ring or the
    /// upstream/index isn't found.
    fn mark_key_healthy(&self, _upstream_id: Option<&str>, _key_idx: usize) -> bool {
        false
    }

    /// Inject an external key exhaustion signal (e.g. from the health
    /// poller — spec §6.4).  Returns `true` if the key was marked
    /// exhausted; `false` when this provider has no key ring or the
    /// upstream/index isn't found.
    ///
    /// The default implementation returns `false` — only providers that
    /// support automatic key rotation (e.g. `KeyRotatingProvider`) or
    /// multiplex upstreams (e.g. `FreeProvider`) override this.
    fn mark_key_exhausted(
        &self,
        _upstream_id: Option<&str>,
        _key_idx: usize,
        _cooldown_secs: u64,
        _reason: Option<String>,
    ) -> bool {
        false
    }
}
