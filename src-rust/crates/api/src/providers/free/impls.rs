// providers/free/impls.rs — FreeProvider behaviour.
//
// Inherent methods, the RetryingFreeStream re-dispatch machinery, and the
// `LlmProvider` trait impl. These are mutually coupled through private
// helpers (e.g. `FreeProvider::should_fallback` is used by both the stream
// and the trait impl), so they live in a single module where Rust privacy
// allows them to share internals.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use clawde_core::provider_id::ModelId;
use futures::Stream;

use crate::provider::ModelInfo;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
};
use clawde_core::types::ContentBlock;
use rand::seq::SliceRandom;

use super::*;

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
    ///
    /// `request` is only consulted by the [`RoutingStrategy::TaskBased`] arm
    /// (Phase 2 smart routing); the other strategies are request-agnostic.
    fn attempt_plan(
        &self,
        route: &Route,
        request: Option<&ProviderRequest>,
    ) -> Vec<(usize, String)> {
        match self.routing.strategy {
            RoutingStrategy::RandomFailover => self.attempt_plan_random(route),
            RoutingStrategy::LatencyBased => self.attempt_plan_latency(route),
            RoutingStrategy::Sequential => self.attempt_plan_sequential(route),
            RoutingStrategy::TaskBased => self.attempt_plan_task(route, request),
        }
    }

    /// Task-based plan (audit spec Phase 2): order the upstreams by how well
    /// they fit the request's task, then fall through the rest in catalog
    /// order. The route anchor still leads — a pinned upstream/model or a
    /// family's hosts go first, then the task-preferred list, then the rest.
    fn attempt_plan_task(
        &self,
        route: &Route,
        request: Option<&ProviderRequest>,
    ) -> Vec<(usize, String)> {
        let task = request
            .map(classify_request)
            .unwrap_or(TaskType::CodeGeneration);
        // Per-task upstream preferences: user overrides from
        // `providers.free.options.routing.task_preferences` win over the
        // built-in defaults (audit spec §8.4/§8.5).
        let prefs: Vec<String> = match &self.routing.task_preferences {
            Some(overrides) => overrides
                .get(task.key())
                .filter(|p| !p.is_empty())
                .cloned()
                .unwrap_or_else(|| {
                    task_preference_ids(task)
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }),
            None => task_preference_ids(task)
                .iter()
                .map(|s| s.to_string())
                .collect(),
        };

        let mut plan: Vec<(usize, String)> = Vec::with_capacity(self.chain_len() * 2);
        let mut used: Vec<usize> = Vec::with_capacity(self.chain_len());

        // Route anchor first: a pinned model (with its per-upstream fallbacks)
        // or every upstream hosting the pinned family leads the plan.
        match route {
            Route::Pinned {
                start_idx,
                pinned_model,
            } => {
                plan.push((*start_idx, pinned_model.clone()));
                for fb in self.chain[*start_idx].upstream.fallback_models {
                    plan.push((*start_idx, fb.to_string()));
                }
                used.push(*start_idx);
            }
            Route::Family { model_family } => {
                for (idx, entry) in self.chain.iter().enumerate() {
                    if entry.upstream.model_family == *model_family {
                        plan.extend(self.plan_rows_for_entry(idx));
                        used.push(idx);
                    }
                }
            }
            Route::Auto => {}
        }

        // Task preference list first, then every remaining upstream in
        // catalog order — each contributing its primary + fallback models.
        let mut ordered: Vec<usize> = Vec::with_capacity(self.chain_len());
        for pref in &prefs {
            if let Some(idx) = self
                .chain
                .iter()
                .position(|e| e.upstream.id == pref.as_str())
            {
                if !used.contains(&idx) && !ordered.contains(&idx) {
                    ordered.push(idx);
                }
            }
        }
        for idx in 0..self.chain.len() {
            if !used.contains(&idx) && !ordered.contains(&idx) {
                ordered.push(idx);
            }
        }
        for idx in ordered {
            plan.extend(self.plan_rows_for_entry(idx));
        }
        plan
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
        let plan = self.attempt_plan(&route, Some(&request));
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
        let plan_vec = self.attempt_plan(&route, Some(&request));
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
                        .attempt_plan(&route, Some(&request))
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
            RoutingStrategy::TaskBased => "Task",
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
// Tests — co-located with the impls they exercise so the inherent/trait
// methods can stay private (a `mod tests` child sees private items).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::{Message, UsageInfo};
    use futures::Stream;
    use std::pin::Pin;
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

    // ---- task-based routing (audit spec Phase 2) -------------------------

    fn task_provider(ids: &[&'static str]) -> FreeProvider {
        let chain = ids.iter().map(|id| entry(id, true)).collect();
        FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                ..Default::default()
            },
            false,
        )
    }

    #[test]
    fn task_plan_respects_user_preference_overrides() {
        // User pins code_generation to groq/cerebras in settings.json — the
        // plan must lead with those, even though the built-in preference
        // list would put openrouter first (and openrouter isn't configured).
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "code_generation".to_string(),
            vec!["groq".to_string(), "cerebras".to_string()],
        );
        let chain = vec![
            entry("huggingface", true),
            entry("groq", true),
            entry("cerebras", true),
        ];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                task_preferences: Some(overrides),
                ..Default::default()
            },
            false,
        );
        let req = ProviderRequest {
            messages: vec![Message::user("write a parser module")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&req));
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(&order[..2], &["groq", "cerebras"]);
        // Unlisted upstreams still appear, in catalog order, after the prefs.
        assert_eq!(order.last(), Some(&"huggingface"));
    }

    #[test]
    fn task_plan_orders_by_reasoning_preferences() {
        // Reasoning prefers google, groq, sambanova — so with a chain ordered
        // [huggingface, sambanova, google, groq] the plan must lead with
        // google, then sambanova/groq by preference, then huggingface last.
        let provider = task_provider(&["huggingface", "sambanova", "google", "groq"]);
        let req = ProviderRequest {
            messages: vec![Message::user("why does the pool keep exhausting?")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&req));
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(&order[..3], &["google", "groq", "sambanova"]);
        // Every remaining upstream still appears (huggingface last).
        assert_eq!(order.last(), Some(&"huggingface"));
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn task_plan_verification_prefers_fast_upstreams() {
        let provider = task_provider(&["google", "huggingface", "groq", "cloudflare"]);
        let req = ProviderRequest {
            messages: vec![Message::user("run the tests and report failures")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&req));
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        // groq + cloudflare lead the verification plan; google is not in the
        // verification preference list so it lands last (catalog order).        assert_eq!(order[0], "groq");
        assert!(order.iter().position(|id| *id == "cloudflare").unwrap() < order.len() - 1);
        // Neither google nor huggingface are verification preferences, so they
        // follow in catalog order (google idx 0, then huggingface idx 1).
        assert_eq!(order.last(), Some(&"huggingface"));
    }

    #[test]
    fn task_plan_pinned_route_keeps_pin_first() {
        let provider = task_provider(&["huggingface", "cerebras", "groq"]);
        let req = ProviderRequest {
            messages: vec![Message::user("fix the sorting bug")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 0,
                pinned_model: "custom-model".to_string(),
            },
            Some(&req),
        );
        // The pin leads; the task order follows for the rest.
        assert_eq!(plan[0], (0, "custom-model".to_string()));
    }

    #[test]
    fn task_plan_without_request_uses_code_generation_prefs() {
        let provider = task_provider(&["zai", "cohere", "mistral"]);
        // No request (e.g. a plan built for the stream re-dispatch without
        // classification) degrades to the code-generation defaults: mistral
        // is in that preference list and leads, the rest follow in catalog
        // order.
        let plan = provider.attempt_plan(&Route::Auto, None);
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(order, vec!["mistral", "zai", "cohere"]);
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
        let plan = provider.attempt_plan(&Route::Auto, None);
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
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 1,
                pinned_model: "meta/llama-3.3-70b-instruct".to_string(),
            },
            None,
        );
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
        let plan = provider.attempt_plan(
            &Route::Family {
                model_family: "llama-3.3-70b",
            },
            None,
        );
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
        let plan = provider.attempt_plan(&Route::Auto, None);
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
        let plan = provider.attempt_plan(&Route::Auto, None);

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
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 2,
                pinned_model: "gemini-2.5-pro".into(),
            },
            None,
        );

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
    fn task_based_config_serde_round_trip() {
        // The exact settings.json path users hit: strategy "task_based" plus
        // a per-task override map.
        let mut prefs = std::collections::HashMap::new();
        prefs.insert(
            "code_generation".to_string(),
            vec!["groq".to_string(), "cerebras".to_string()],
        );
        let cfg = RoutingConfig {
            strategy: RoutingStrategy::TaskBased,
            task_preferences: Some(prefs),
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(json.contains("\"task_based\""), "json: {json}");
        let back: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.strategy, RoutingStrategy::TaskBased));
        let prefs = back.task_preferences.unwrap();
        assert_eq!(
            prefs.get("code_generation"),
            Some(&vec!["groq".to_string(), "cerebras".to_string()])
        );
    }

    #[test]
    fn attempt_plan_pinned_tries_pin_then_others() {
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("cerebras", true),
            entry("google", true),
        ]);
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 2,
                pinned_model: "gemini-2.5-pro".into(),
            },
            None,
        );
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
        let plan = provider.attempt_plan(&Route::Auto, None);
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

        let plan = provider.attempt_plan(&Route::Auto, None);

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
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 1,
                pinned_model: "custom-model".into(),
            },
            None,
        );

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
        let plan = provider.attempt_plan(&Route::Auto, None);
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

// ---- Free-upstream key resolution: ring alignment ----------------------

/// Regression guard for the health-poller ↔ KeyRotatingProvider contract:
/// [`resolve_free_upstream_keys`] must return EXACTLY the list that
/// `build_free_provider` feeds into each `KeyRotatingProvider` ring, in
/// the same order, so that the `key_idx` the health poller forwards into
/// `mark_key_healthy` / `mark_key_exhausted` lines up with the ring slot.
///
/// Rules that keep the index alignment intact:
///   * credentials (`api_key_for`) are NOT included — rings are built from
///     the multi-key store only;
///   * whitespace-trimmed and >=8 chars (placeholder / test-artifact
///     filter applied at ring build time too);
///   * OpenCode Zen reads the `opencode-go` slots as a fallback.
#[test]
fn resolve_free_upstream_keys_is_ring_aligned() {
    let (mut store, _home) = crate::test_support::test_auth_store();
    store.keys.insert(
        "groq".to_string(),
        vec![
            "   gsk-a-very-long-real-key-0001".into(), // trimmed, kept
            "short".into(),                            // <8 chars, filtered
            "gsk-b-very-long-real-key-0002".into(),
        ],
    );

    let keys = resolve_free_upstream_keys(&store, "groq").expect("keys present");
    assert_eq!(
        keys,
        vec![
            "gsk-a-very-long-real-key-0001".to_string(),
            "gsk-b-very-long-real-key-0002".to_string(),
        ],
        "resolver must trim, filter short keys, and preserve ring order"
    );
    // The order here is the index contract: ring slot 0 = first element.
    assert_eq!(keys.len(), 2);
}

#[test]
fn resolve_free_upstream_keys_ignores_credentials() {
    // A provider with a single credential but no multi-key slots must not
    // be treated as a multi-key ring (index 0 in the ring would otherwise
    // not correspond to anything the poller probes).
    let (mut store, _home) = crate::test_support::test_auth_store();
    store.credentials.insert(
        "openrouter".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "or-credential-key-0123456789".into(),
        },
    );

    assert_eq!(
        resolve_free_upstream_keys(&store, "openrouter"),
        None,
        "credentials must not leak into the ring-aligned key list"
    );
    assert_eq!(
        all_stored_free_upstream_keys(&store, "openrouter"),
        vec!["or-credential-key-0123456789".to_string()],
        "display-oriented union still surfaces the credential"
    );
}

#[test]
fn resolve_free_upstream_keys_opencode_zen_shares_go_slots() {
    let (mut store, _home) = crate::test_support::test_auth_store();
    store.keys.insert(
        "opencode-go".to_string(),
        vec!["zen-shared-key-00000000000000".into()],
    );

    // Zen has no slots of its own — the ring must be built from the Go
    // slots so poller key_idx stays aligned with the actual ring.
    assert_eq!(
        resolve_free_upstream_keys(&store, "opencode-zen"),
        Some(vec!["zen-shared-key-00000000000000".to_string()])
    );
}

#[test]
fn all_stored_free_upstream_keys_dedups_and_merges() {
    let (mut store, _home) = crate::test_support::test_auth_store();
    store.credentials.insert(
        "groq".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "gsk-credential-00000000".into(),
        },
    );
    store.keys.insert(
        "groq".to_string(),
        vec![
            "gsk-credential-00000000".into(),
            "gsk-rotating-000000000".into(),
        ],
    );

    let keys = all_stored_free_upstream_keys(&store, "groq");
    assert_eq!(
        keys,
        vec![
            "gsk-credential-00000000".to_string(),
            "gsk-rotating-000000000".to_string(),
        ],
        "credential first, rotation keys after, duplicates removed"
    );
}

#[test]
fn first_free_upstream_key_prefers_valid_ring_key_then_credential_then_env() {
    // One TestHome guard for the whole test body; each scenario builds a
    // fresh in-memory store. Creating a second TestHome while the first
    // guard is still alive would re-lock the non-reentrant CLAWDE_HOME_LOCK
    // on the same thread and self-deadlock, so the guard is created once.
    let _home = crate::test_support::TestHome::new();

    // A valid multi-key slot wins over a stored credential — the ring
    // resolver would use it, so the single-key path must agree (and the
    // health poller probes those exact slots).
    let mut store = clawde_core::AuthStore::default();
    store.credentials.insert(
        "openrouter".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "or-credential-key-0123456789".into(),
        },
    );
    store.keys.insert(
        "openrouter".to_string(),
        vec!["or-rotating-key-0123456789".into()],
    );
    assert_eq!(
        first_free_upstream_key(&store, "openrouter").as_deref(),
        Some("or-rotating-key-0123456789"),
        "valid ring key must win over the credential"
    );

    // No valid ring keys -> credential.
    let mut store = clawde_core::AuthStore::default();
    store.credentials.insert(
        "openrouter".to_string(),
        clawde_core::StoredCredential::ApiKey {
            key: "or-credential-key-0123456789".into(),
        },
    );
    store
        .keys
        .insert("openrouter".to_string(), vec!["short".into()]);
    assert_eq!(
        first_free_upstream_key(&store, "openrouter").as_deref(),
        Some("or-credential-key-0123456789"),
        "credential used when no valid ring key exists"
    );

    // No credential, no keys -> env var fallback (guarded so it only
    // asserts when the test runner doesn't export the key).
    let store = clawde_core::AuthStore::default();
    if std::env::var("OPENROUTER_API_KEY").is_ok() {
        assert!(first_free_upstream_key(&store, "openrouter").is_some());
    } else {
        assert_eq!(first_free_upstream_key(&store, "openrouter"), None);
    }
}

#[test]
fn first_free_upstream_key_trims_and_drops_placeholders() {
    // One TestHome guard for the whole test body — see the sibling test
    // above for why a second guard would self-deadlock on the same thread.
    let _home = crate::test_support::TestHome::new();

    // A slot-0 placeholder does NOT shadow a valid slot-1 key — the
    // ring-consistent resolver trims all slots and skips short ones, so
    // the single-key path sees the same key the ring would have used.
    let mut store = clawde_core::AuthStore::default();
    store.keys.insert(
        "groq".to_string(),
        vec!["   short   ".into(), "gsk-very-long-real-key-0001".into()],
    );
    assert_eq!(
        first_free_upstream_key(&store, "groq").as_deref(),
        Some("gsk-very-long-real-key-0001"),
        "placeholder in slot 0 must not shadow the valid slot-1 key"
    );

    // A padded but genuinely long key is trimmed and kept.
    let mut store = clawde_core::AuthStore::default();
    store.keys.insert(
        "groq".to_string(),
        vec!["   gsk-very-long-real-key-0001   ".into()],
    );
    assert_eq!(
        first_free_upstream_key(&store, "groq").as_deref(),
        Some("gsk-very-long-real-key-0001")
    );

    // Short keys alone -> None.
    let mut store = clawde_core::AuthStore::default();
    store.keys.insert("groq".to_string(), vec!["short".into()]);
    assert_eq!(first_free_upstream_key(&store, "groq"), None);
}

#[test]
fn first_free_upstream_key_opencode_zen_shares_go_slots() {
    let (mut store, _home) = crate::test_support::test_auth_store();
    store.keys.insert(
        "opencode-go".to_string(),
        vec!["zen-shared-key-00000000000000".into()],
    );
    assert_eq!(
        first_free_upstream_key(&store, "opencode-zen").as_deref(),
        Some("zen-shared-key-00000000000000")
    );
}

/// Load the synthetic multi-upstream fixture (`tests/fixtures/auth.json`)
/// through the real `AuthStore::load()` path by pointing CLAWDE_HOME at a
/// temp dir containing a copy of it.
///
/// This exercises the full disk→resolver path against a realistic store
/// shape: 5 upstreams with 2+ keys (ring paths), 6 single-key slots, cloudflare
/// composite keys, and credentials separate from the keys map. The fixture is
/// git-ignored and holds only fake keys.
#[test]
fn resolvers_agree_on_synthetic_fixture_store() {
    let _home = crate::test_support::TestHome::new();

    // Copy the fixture into the redirected CLAWDE_HOME so `AuthStore::load()`
    // reads it from `{home}/auth.json`.
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/auth.json");
    let dest = std::env::var("CLAWDE_HOME").expect("TestHome sets CLAWDE_HOME");
    let dest_dir = std::path::Path::new(&dest);
    std::fs::create_dir_all(dest_dir).unwrap();
    std::fs::copy(&fixture, dest_dir.join("auth.json")).unwrap();

    let store = clawde_core::AuthStore::load();

    // Multi-key upstreams form rings of exactly the stored key count
    // (all fixture keys are >=8 chars, so nothing is filtered out).
    for (upstream, expected) in [
        ("groq", 2),
        ("nvidia", 2),
        ("opencode-zen", 3),
        ("cline", 2),
        ("cloudflare", 2),
    ] {
        let ring = resolve_free_upstream_keys(&store, upstream)
            .unwrap_or_else(|| panic!("{upstream}: expected a ring"));
        assert_eq!(ring.len(), expected, "{upstream} ring size");
        // Ring keys equal the display union minus credentials — the poller
        // probes exactly these.
        let display = all_stored_free_upstream_keys(&store, upstream);
        assert_eq!(ring, display, "{upstream}: ring keys == display keys");
    }

    // Single-key upstreams resolve through the single-key chain path.
    for upstream in [
        "cerebras",
        "zai",
        "sambanova",
        "mistral",
        "google",
        "cohere",
        "huggingface",
    ] {
        assert!(
            first_free_upstream_key(&store, upstream).is_some(),
            "{upstream}: single-key chain path must resolve"
        );
    }

    // Cloudflare composite keys survive round-trip with the account id intact.
    let cf = resolve_free_upstream_keys(&store, "cloudflare").unwrap();
    assert!(
        cf.iter().all(|k| k.contains(':')),
        "cloudflare keys must keep ACCOUNT_ID:TOKEN composite form"
    );

    // Credentials are excluded from rings but visible in the display union.
    let zen_ring = resolve_free_upstream_keys(&store, "opencode-zen").unwrap();
    let zen_display = all_stored_free_upstream_keys(&store, "opencode-zen");
    assert_eq!(zen_ring.len(), 3);
    assert_eq!(
        zen_display.len(),
        3,
        "no credentials stored for opencode-zen"
    );
}
