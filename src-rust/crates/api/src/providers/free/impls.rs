// providers/free/impls.rs — FreeProvider behaviour.
//
// Inherent methods, the RetryingFreeStream re-dispatch machinery, and the
// `LlmProvider` trait impl. These are mutually coupled through private
// helpers (e.g. `FreeProvider::should_fallback` is used by both the stream
// and the trait impl), so they live in a single module where Rust privacy
// allows them to share internals.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::Stream;

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
    pub(super) fn resolve_route(&self, model: &str) -> Route {
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
    pub(super) fn attempt_plan(&self, route: &Route) -> Vec<(usize, String)> {
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

    pub(super) fn should_fallback(err: &ProviderError) -> bool {
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
    pub(super) fn is_in_cooldown(&self, idx: usize) -> bool {
        let mut cd = self.cooldown.lock().unwrap();
        cd.prune_expired();
        cd.is_in_cooldown(idx) || cd.is_in_empty_cooldown(idx)
    }

    /// Record a successful request at `idx` with the given `elapsed` duration.
    pub(super) fn record_success(&self, idx: usize, elapsed: std::time::Duration) {
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
    pub(super) fn record_failure(&self, idx: usize) {
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
    pub(super) fn maybe_cooldown_upstream_for_5xx(&self, idx: usize, err: &ProviderError) {
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
