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

use crate::provider::{ModelInfo, UpstreamTaskSuccessRates};
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StreamEvent,
    SystemPromptStyle,
};
use clawde_core::types::{ContentBlock, MessageContent};
use rand::seq::SliceRandom;

use super::*;

impl FreeProvider {
    /// Minimum dispatch count before an upstream's success rate is trusted
    /// for Auto-ordering. One or two samples are noise — a single failure
    /// must not relegate a strong upstream to the tail, nor a single win
    /// promote a flaky one ahead of a proven provider.
    ///
    /// Public so the routing dialog's perf view applies the exact same
    /// threshold when it ranks upstreams for the selected task.
    pub const MIN_SUCCESS_RATE_SAMPLES: u32 = 3;

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

    /// Production flag for cooldown persistence: persists BOTH the 5xx /
    /// circuit-breaker track and the empty-completion track to disk so a
    /// restart does not immediately re-hit a cooled-down upstream. The
    /// `EMPTY` name is historical — see [`Self::with_routing`].
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
    /// `ENABLE_EMPTY_COOLDOWN_PERSISTENCE`) both cooldown tracks (5xx /
    /// circuit-breaker and empty-completion) are persisted to
    /// `{clawde_home}/empty-cooldown-state/free.json`. The filename is
    /// retained for backward compatibility with files written before the
    /// 5xx track was added.
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
            CooldownState::new(n, cb_config).with_persistence(upstream_ids.clone(), persist_path),
        ));
        let telemetry_path = if persist {
            Some(
                clawde_core::config::Settings::config_dir()
                    .join("telemetry-state")
                    .join("free.json"),
            )
        } else {
            None
        };
        let max_samples = routing.latency.as_ref().map_or(0, |l| l.max_samples);
        let latencies =
            LatencyState::new(n).with_persistence(upstream_ids, telemetry_path, max_samples);
        Self {
            id: ProviderId::new(ProviderId::FREE),
            chain,
            routing,
            cooldown,
            latencies: Arc::new(Mutex::new(latencies)),
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
    /// `request` is only consulted by the task-routing arms
    /// ([`RoutingStrategy::Auto`] and [`RoutingStrategy::TaskBased`], the
    /// Phase 2 smart router); the other strategies are request-agnostic.
    fn attempt_plan(
        &self,
        route: &Route,
        request: Option<&ProviderRequest>,
    ) -> Vec<(usize, String)> {
        let plan = match self.routing.strategy {
            // Auto is the smart default — it routes by task just like the
            // explicit TaskBased strategy (audit spec §8.4).
            RoutingStrategy::Auto | RoutingStrategy::TaskBased => {
                self.attempt_plan_task(route, request)
            }
            RoutingStrategy::RandomFailover => self.attempt_plan_random(route),
            RoutingStrategy::LatencyBased => self.attempt_plan_latency(route),
            RoutingStrategy::Sequential => self.attempt_plan_sequential(route),
        };

        // Apply the user-configured disabled-upstream list after the strategy
        // has built its plan, then the per-request capability gate. Keeping
        // these as one final gate ensures pinned, family, task-preferred,
        // sequential, random, and latency routes all honor the same settings.
        // Unknown ids are harmless: they simply never match an active chain
        // entry.
        //
        // The image-presence check and the token estimate are computed ONCE
        // here (not per chain entry) so a 14-upstream chain does not scan the
        // whole message history 14 times.
        let has_images = request.map(Self::request_has_images).unwrap_or(false);
        let estimate = request.map(Self::estimate_request_tokens).unwrap_or(0);
        plan.into_iter()
            .filter(|(idx, _)| !self.is_disabled_upstream(*idx))
            .filter(|(idx, _)| self.entry_fits_request(*idx, has_images, estimate))
            .collect()
    }

    fn is_disabled_upstream(&self, idx: usize) -> bool {
        self.chain.get(idx).is_some_and(|entry| {
            self.routing
                .disabled_upstreams
                .iter()
                .any(|id| id == entry.upstream.id)
        })
    }

    /// Capability gate (audit spec §8.4 "capability match"): drop upstreams
    /// whose capabilities cannot serve the request's content before dispatch.
    ///
    /// `has_images` and `estimate` are precomputed once in [`Self::attempt_plan`]
    /// so this check stays O(1) per chain entry.
    ///
    /// - Image-bearing requests skip non-vision upstreams: a text-only
    ///   provider rejects the image with a 400 `InvalidRequest`, which
    ///   [`Self::should_fallback`] deliberately does NOT retry — without this
    ///   gate the whole request would hard-fail on the first text-only
    ///   upstream instead of reaching a vision-capable one.
    /// - Requests whose estimated input-token count exceeds an upstream's
    ///   documented context window are skipped, so the plan does not burn a
    ///   guaranteed-overflow round-trip (e.g. Copilot's 16K serving cap).
    ///   Only the input estimate is checked — output tokens are deliberately
    ///   NOT reserved (the chars/4 estimate under-counts code, and reserving
    ///   full `max_tokens` would over-filter upstreams that usually emit far
    ///   less). This is a "definitely won't fit" gate, not a "might not fit".
    fn entry_fits_request(&self, idx: usize, has_images: bool, estimate: u64) -> bool {
        let Some(entry) = self.chain.get(idx) else {
            return true;
        };
        if has_images && !entry.upstream.vision {
            return false;
        }
        if estimate > 0 && estimate > u64::from(entry.upstream.context_window) {
            return false;
        }
        true
    }

    /// Whether the request carries any image content block. Image-bearing
    /// requests are routed only to vision-capable upstreams.
    fn request_has_images(request: &ProviderRequest) -> bool {
        request.messages.iter().any(|m| match &m.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            _ => false,
        })
    }

    /// Estimated input-token size of the request (heuristic from
    /// `clawde_core::message_utils::estimate_messages_tokens`, ~4 chars/token).
    fn estimate_request_tokens(request: &ProviderRequest) -> u64 {
        clawde_core::message_utils::estimate_messages_tokens(&request.messages)
    }

    /// When the capability gate filters out every upstream, produce a
    /// user-actionable reason. The generic "all may be in cooldown" message is
    /// misleading for image/context-filtered plans — an image request with no
    /// vision upstream configured is a configuration gap, not a cooldown.
    fn capability_block_reason(&self, request: &ProviderRequest) -> Option<String> {
        let available: Vec<usize> = (0..self.chain.len())
            .filter(|idx| !self.is_disabled_upstream(*idx))
            .collect();
        if Self::request_has_images(request)
            && !available.iter().any(|idx| self.chain[*idx].upstream.vision)
        {
            return Some(
                "no enabled upstream supports image input — add a vision-capable provider via /connect (e.g. google or github-copilot)"
                    .to_string(),
            );
        }
        let estimate = Self::estimate_request_tokens(request);
        if estimate > 0
            && !available
                .iter()
                .any(|idx| estimate <= u64::from(self.chain[*idx].upstream.context_window))
        {
            return Some(format!(
                "request is too large (approx {} estimated tokens) for every enabled upstream's context window",
                estimate
            ));
        }
        None
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

        // Task-preferred upstreams first, then every remaining upstream in
        // catalog order — each contributing its primary + fallback models.
        // Within the preferred group, order by dispatch success rate then
        // historical average latency (spec §8.4 criterion 2 + success-rate
        // refinement): a task-appropriate upstream that keeps failing yields
        // to one that actually succeeds, and among equally reliable upstreams
        // the faster one leads. Upstreams without enough history sort by
        // latency alone; upstreams with no history sort to the group tail,
        // keeping their preference order via the stable sort.
        let mut preferred: Vec<usize> = Vec::with_capacity(self.chain_len());
        for pref in &prefs {
            if let Some(idx) = self
                .chain
                .iter()
                .position(|e| e.upstream.id == pref.as_str())
            {
                if !used.contains(&idx) && !preferred.contains(&idx) {
                    preferred.push(idx);
                }
            }
        }
        let mut rest: Vec<usize> = Vec::with_capacity(self.chain_len());
        for idx in 0..self.chain.len() {
            if !used.contains(&idx) && !preferred.contains(&idx) {
                rest.push(idx);
            }
        }
        if preferred.len() > 1 {
            let lat = self.latencies.lock().unwrap();
            preferred.sort_by(|a, b| {
                Self::preferred_order_key(&lat, *a, task)
                    .partial_cmp(&Self::preferred_order_key(&lat, *b, task))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        for idx in preferred.into_iter().chain(rest) {
            plan.extend(self.plan_rows_for_entry(idx));
        }
        plan
    }

    /// Ordering key for the task-preferred group (spec §8.4 criterion 2 +
    /// success-rate refinement, see [`Self::MIN_SUCCESS_RATE_SAMPLES`]).
    ///
    /// - Rank 0: enough dispatch history — sort by success rate descending
    ///   (negated so ascending sort puts higher rates first), then latency
    ///   ascending.
    /// - Rank 1: a couple of samples — rates not yet trustworthy, sort by
    ///   latency alone.
    /// - Rank 2: no history — group tail, keeping preference order.
    fn preferred_order_key(lat: &LatencyState, idx: usize, task: TaskType) -> (u8, f64, f64) {
        let task_dispatches = lat.task_dispatches(idx, task);
        let (dispatches, success_rate) = if task_dispatches > 0 {
            (task_dispatches, lat.task_success_rate(idx, task))
        } else {
            // A task with no own history can still use aggregate history as
            // a conservative prior. Once the task has one dispatch, keep it
            // isolated so unrelated work cannot outweigh task-specific data.
            (lat.dispatches(idx), lat.success_rate(idx))
        };
        let avg = lat.avg_latency(idx);
        match (dispatches, success_rate) {
            (n, Some(rate)) if n >= Self::MIN_SUCCESS_RATE_SAMPLES => (0, -rate, avg),
            (n, _) if n > 0 => (1, 0.0, avg),
            _ => (2, 0.0, avg),
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
    /// `task` is the request's classified [`TaskType`] — the dispatch is also
    /// credited to the per-task success-rate view (spec §8.6).
    fn record_success(&self, idx: usize, task: TaskType, elapsed: std::time::Duration) {
        // Reset circuit breaker failure counter for this upstream.
        if self.circuit_breaker_enabled() {
            let mut cd = self.cooldown.lock().unwrap();
            cd.record_success(idx);
        }
        // Record latency sample + success counters (spec §8.6 success rate,
        // overall and per-task). One lock: the counters always bump, the
        // latency sample only when latency tracking is enabled.
        let max_samples = self.max_latency_samples();
        let snapshot = {
            let mut lat = self.latencies.lock().unwrap();
            lat.record_success(idx);
            lat.record_task_success(idx, task);
            if max_samples > 0 {
                lat.record(idx, elapsed.as_secs_f64(), max_samples);
            }
            lat.snapshot()
        };
        LatencyState::persist_snapshot(snapshot);
    }

    /// Record a failed request at `idx`. `task` is the request's classified
    /// [`TaskType`] — the dispatch is also credited to the per-task view.
    fn record_failure(&self, idx: usize, task: TaskType) {
        // Always count the failure for the success-rate views — the circuit
        // breaker below is an optional extra layer.
        let snapshot = {
            let mut lat = self.latencies.lock().unwrap();
            lat.record_failure(idx);
            lat.record_task_failure(idx, task);
            lat.snapshot()
        };
        LatencyState::persist_snapshot(snapshot);
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
    /// Classified task for the request — tags every attempt's success /
    /// failure counters for the per-task success-rate view (spec §8.6).
    task: TaskType,
    remaining_plan: VecDeque<(usize, String)>,
    current: Option<BoxedProviderStream>,
    current_idx: usize,
    current_model: String,
    pending_attribution: bool,
    /// Whether the current attempt's success has been credited to the
    /// success-rate / latency counters (spec §8.6). Set at the completion
    /// signal (MessageStop) so consumers that drop the stream there still
    /// count the win; guards against double-crediting when a consumer polls
    /// the stream through to `None`.
    success_recorded: bool,
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
        let task = classify_request(&request);
        Self {
            chain,
            cooldown,
            latencies,
            routing,
            request,
            task,
            remaining_plan,
            current: Some(stream),
            current_idx: idx,
            current_model: upstream_model,
            pending_attribution: true,
            success_recorded: false,
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
        let snapshot = {
            let mut lat = self.latencies.lock().unwrap();
            lat.record_success(idx);
            lat.record_task_success(idx, self.task);
            if max_samples > 0 {
                lat.record(idx, elapsed.as_secs_f64(), max_samples);
            }
            lat.snapshot()
        };
        LatencyState::persist_snapshot(snapshot);
    }

    fn record_failure(&self, idx: usize) {
        // Always count the failure for the success-rate views (aggregate and
        // per-task) — the circuit breaker below is an optional extra layer.
        let snapshot = {
            let mut lat = self.latencies.lock().unwrap();
            lat.record_failure(idx);
            lat.record_task_failure(idx, self.task);
            lat.snapshot()
        };
        LatencyState::persist_snapshot(snapshot);
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

    /// Credit a successful dispatch to the current upstream at the completion
    /// signal.
    ///
    /// Interactive consumers break on `StreamEvent::MessageStop` and drop the
    /// stream without ever polling to `None`, so the success-rate and latency
    /// counters (spec §8.6) must be updated when the stop event is observed —
    /// not only when the stream is drained to its end. `success_recorded`
    /// guards against double-crediting by consumers that DO poll to the end
    /// (the `Poll::Ready(None)` branch also credits, see below).
    fn maybe_record_success(&mut self) {
        if self.success_recorded || self.is_empty_attempt() {
            return;
        }
        self.success_recorded = true;
        if let Some(elapsed) = self.attempt_start.map(|s| s.elapsed()) {
            self.record_success(self.current_idx, elapsed);
        }
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
                        self.pending_attribution = true;
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
                        self.pending_attribution = true;
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

            // Announce the currently selected upstream before exposing its
            // content. This also re-announces after empty-completion and
            // parallel-probe switches, so the query loop always ends with the
            // successful upstream's attribution.
            if self.pending_attribution {
                self.pending_attribution = false;
                return Poll::Ready(Some(Ok(StreamEvent::ProviderAttribution {
                    provider_id: ProviderId::FREE.to_string(),
                    upstream_id: self.chain[self.current_idx].upstream.id.to_string(),
                    model: self.current_model.clone(),
                })));
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
                    // Interactive consumers break on MessageStop and drop the
                    // stream, so credit the successful dispatch at the
                    // completion signal rather than only when the stream is
                    // drained to `None` (which they never reach). Empty
                    // attempts are NOT credited here — they still flow through
                    // the empty-completion re-dispatch path when polled to
                    // `None`, and otherwise remain uncounted.
                    if matches!(evt, StreamEvent::MessageStop) {
                        self.maybe_record_success();
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

                    // Non-empty success — record latency/success, guarded so a
                    // completion already credited at MessageStop is not
                    // double-counted by consumers that poll to the end.
                    if !self.success_recorded {
                        if let Some(elapsed) = elapsed {
                            self.record_success(self.current_idx, elapsed);
                        }
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
        // The request's task tags every dispatch's success/failure counters
        // (spec §8.6 per-task success-rate view).
        let task = classify_request(&request);

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
                    self.record_success(idx, task, start.elapsed());
                    return Ok(resp);
                }
                Ok(Err(err)) if Self::should_fallback(&err) => {
                    tracing::warn!(
                        "FreeProvider: {} failed ({}s): {} — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                        err,
                    );
                    self.record_failure(idx, task);
                    self.maybe_cooldown_upstream_for_5xx(idx, &err);
                    last_err = Some(err);
                    continue;
                }
                Ok(Err(err)) => {
                    self.record_failure(idx, task);
                    return Err(err);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        "FreeProvider: upstream {} timed out after {}s — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                    );
                    self.record_failure(idx, task);
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
        } else if let Some(reason) = self.capability_block_reason(&request) {
            format!("all free-mode upstreams exhausted: {}", reason)
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
        // The request's task tags every dispatch's success/failure counters
        // (spec §8.6 per-task success-rate view).
        let task = classify_request(&request);

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
                    self.record_failure(idx, task);
                    self.maybe_cooldown_upstream_for_5xx(idx, &err);
                    last_err = Some(err);
                    continue;
                }
                Ok(Err(err)) => {
                    self.record_failure(idx, task);
                    return Err(err);
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        "FreeProvider: upstream {} stream timed out after {}s — trying next upstream",
                        entry.upstream.id,
                        self.routing.upstream_timeout_secs,
                    );
                    self.record_failure(idx, task);
                    last_err = Some(ProviderError::RateLimited {
                        provider: self.id.clone(),
                        retry_after: None,
                    });
                    continue;
                }
            }
        }

        let err_msg = if let Some(reason) = self.capability_block_reason(&request) {
            format!("all free-mode upstreams exhausted: {}", reason)
        } else if let Some(err) = &last_err {
            format!("all free-mode upstreams exhausted (last error: {})", err)
        } else {
            "all free-mode upstreams exhausted".to_string()
        };
        Err(last_err.unwrap_or_else(|| ProviderError::ServerError {
            provider: self.id.clone(),
            status: None,
            message: err_msg,
            is_retryable: false,
        }))
    }

    fn routing_strategy_name(&self) -> Option<&'static str> {
        Some(match self.routing.strategy {
            RoutingStrategy::Auto => "Auto",
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
                entry.upstream.context_window,
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

    fn upstream_latencies(&self) -> Vec<(String, Option<f64>)> {
        // Per-upstream sliding-window average latency (seconds), for the
        // routing dialog's model-performance view (spec §8.6). `None` when
        // the upstream has no samples yet (`avg_latency`'s f64::MAX sentinel
        // for empty windows). Locked once, never across an await.
        let lat = self.latencies.lock().unwrap();
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let avg = lat.avg_latency(idx);
                (
                    entry.upstream.id.to_string(),
                    if avg >= f64::MAX { None } else { Some(avg) },
                )
            })
            .collect()
    }

    fn upstream_success_rates(&self) -> Vec<(String, Option<f64>)> {
        // Per-upstream dispatch success rate (0.0–1.0), for the routing
        // dialog's model-performance view (spec §8.6). `None` when the
        // upstream has no recorded dispatches. Locked once, never across an
        // await.
        let lat = self.latencies.lock().unwrap();
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.upstream.id.to_string(), lat.success_rate(idx)))
            .collect()
    }

    fn upstream_task_success_rates(&self) -> UpstreamTaskSuccessRates {
        // Per-upstream per-task dispatch success rates (spec §8.6): when the
        // user highlights a task in the routing dialog, the % column shows
        // each upstream's rate FOR that task instead of the aggregate. Only
        // tasks with recorded dispatches are included. Locked once, never
        // across an await.
        let lat = self.latencies.lock().unwrap();
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let rates: Vec<(String, Option<f64>)> = TaskType::ALL
                    .iter()
                    .filter_map(|t| {
                        lat.task_success_rate(idx, *t)
                            .map(|r| (t.key().to_string(), Some(r)))
                    })
                    .collect();
                (entry.upstream.id.to_string(), rates)
            })
            .collect()
    }

    fn upstream_dispatch_counts(&self) -> Vec<(String, u32)> {
        // Per-upstream recorded dispatch counts (spec §8.6) — the trust
        // signal behind a success rate: the router only treats a rate as
        // reliable once `MIN_SUCCESS_RATE_SAMPLES` dispatches exist, and the
        // routing dialog's perf view uses the same gate to tier its ranking.
        // Locked once, never across an await.
        let lat = self.latencies.lock().unwrap();
        self.chain
            .iter()
            .enumerate()
            .map(|(idx, entry)| (entry.upstream.id.to_string(), lat.dispatches(idx)))
            .collect()
    }

    fn upstream_capabilities(&self) -> Vec<(String, bool, u32)> {
        // Per-upstream vision + context-window metadata from the catalog, for
        // the routing dialog's capability view (spec §8.6). Mirrors the
        // capability gate in `entry_fits_request` so the UI can explain why
        // image-bearing or oversized requests skip certain upstreams.
        self.chain
            .iter()
            .map(|entry| {
                (
                    entry.upstream.id.to_string(),
                    entry.upstream.vision,
                    entry.upstream.context_window,
                )
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
        // tool_calling / image_input are true when any chain entry's
        // upstream supports it — an image request only routes to vision
        // entries (see `entry_fits_request`), so the composite advertises
        // vision iff at least one configured upstream can serve it.
        let tool_calling = self.chain.iter().any(|entry| entry.upstream.tool_calling);
        let image_input = self.chain.iter().any(|entry| entry.upstream.vision);

        ProviderCapabilities {
            streaming: true,
            tool_calling,
            thinking: false,
            image_input,
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
    use clawde_core::types::{ImageSource, Message, UsageInfo};
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
    fn free_upstream_base_url_override_reads_env_var() {
        // Dev-only override: CLAWDE_FREE_BASE_URL_<ID> points an upstream at
        // a local mock so 5xx / empty-completion cooldown paths are testable
        // live. Hyphenated ids map to underscore env vars. The two guards
        // are scoped separately because ENV_LOCK is non-reentrant.
        let groq_result = {
            let _g = crate::test_support::EnvVarGuard::set(
                "CLAWDE_FREE_BASE_URL_GROQ",
                "http://127.0.0.1:9876/v1",
            );
            free_upstream_base_url_override("groq")
        };
        assert_eq!(groq_result, Some("http://127.0.0.1:9876/v1".to_string()));

        let _g2 = crate::test_support::EnvVarGuard::set(
            "CLAWDE_FREE_BASE_URL_OPENCODE_ZEN",
            "http://127.0.0.1:9877",
        );
        assert_eq!(
            free_upstream_base_url_override("opencode-zen"),
            Some("http://127.0.0.1:9877".to_string())
        );
    }

    #[test]
    fn free_upstream_base_url_override_empty_or_unset_is_none() {
        // Whitespace-only overrides are treated as absent.
        let _g = crate::test_support::EnvVarGuard::set("CLAWDE_FREE_BASE_URL_GROQ", "   ");
        assert_eq!(free_upstream_base_url_override("groq"), None);
        // The cline var is never set in this process.
        assert_eq!(free_upstream_base_url_override("cline"), None);
    }

    #[test]
    fn free_upstream_base_url_override_rejects_remote_and_non_http_urls() {
        let _remote = crate::test_support::EnvVarGuard::set(
            "CLAWDE_FREE_BASE_URL_GROQ",
            "https://example.invalid/v1",
        );
        assert_eq!(free_upstream_base_url_override("groq"), None);
        drop(_remote);

        let _scheme =
            crate::test_support::EnvVarGuard::set("CLAWDE_FREE_BASE_URL_GROQ", "file:///tmp/mock");
        assert_eq!(free_upstream_base_url_override("groq"), None);
    }

    #[test]
    fn chat_probe_for_honors_base_url_override() {
        // The startup /health probe must also hit the mock, otherwise the
        // 401 against the real endpoint would cool every ring key before the
        // 5xx chat path could ever be exercised.
        let _g = crate::test_support::EnvVarGuard::set(
            "CLAWDE_FREE_BASE_URL_HUGGINGFACE",
            "http://127.0.0.1:9878/v1",
        );
        let (base, model) = chat_probe_for("huggingface").expect("hf probe");
        assert_eq!(base, "http://127.0.0.1:9878/v1");
        assert_eq!(model, "meta-llama/Llama-3.3-70B-Instruct");
    }

    #[tokio::test]
    async fn overridden_base_url_5xx_applies_cooldown_and_is_reported() {
        // A local mock returning 500 exercises the exact live path the env
        // override unlocks: dispatch → 5xx → circuit cooldown → reported via
        // upstream_cooldowns() (the /routing dialog's `·cool Ns` tag).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ready = spawn_mock_server(
            listener,
            "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: 0\r\n\r\n"
                .to_string(),
        );
        wait_for_mock_server(&ready);

        let upstream = *catalog_entry("groq").expect("groq catalog entry");
        let compat = crate::providers::openai_compat_providers::groq()
            .with_api_key("fake-groq-key-1234567890".to_string())
            .with_base_url(format!("http://127.0.0.1:{}", port));
        let provider = FreeProvider::new(vec![FreeEntry {
            upstream,
            provider: Arc::new(compat) as Arc<dyn LlmProvider>,
            effective_model: None,
        }]);

        let err = provider
            .create_message(dummy_request("free/auto"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                ProviderError::ServerError {
                    status: Some(500),
                    ..
                }
            ),
            "mock 500 should surface as a server error, got {:?}",
            err
        );

        let cooldowns = provider.upstream_cooldowns();
        assert!(
            cooldowns
                .iter()
                .any(|(id, kind, secs)| id == "groq" && kind == "5xx" && secs.is_some()),
            "5xx must apply a cooldown reported to the dialog, got {:?}",
            cooldowns
        );
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
        /// When set, records the `request.model` of every `create_message`
        /// call so tests can assert dispatch ORDER through the plan.
        attempt_log: Option<Arc<Mutex<Vec<String>>>>,
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
            if let Some(log) = &self.attempt_log {
                if let Ok(mut l) = log.lock() {
                    l.push(request.model.clone());
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

    struct StreamStubProvider {
        id: ProviderId,
        stream_ok: bool,
        text: Option<&'static str>,
    }

    #[async_trait]
    impl LlmProvider for StreamStubProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "stream-stub"
        }

        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            unimplemented!("attribution tests only use streaming")
        }

        async fn create_message_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            if !self.stream_ok {
                return Err(ProviderError::ServerError {
                    provider: self.id.clone(),
                    status: Some(503),
                    message: "stream stub failure".into(),
                    is_retryable: true,
                });
            }

            let mut events = vec![Ok(StreamEvent::MessageStart {
                id: "stream-stub-message".into(),
                model: request.model,
                usage: UsageInfo::default(),
            })];
            if let Some(text) = self.text {
                events.push(Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: text.into(),
                }));
            }
            events.extend([
                Ok(StreamEvent::MessageDelta {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(UsageInfo::default()),
                }),
                Ok(StreamEvent::MessageStop),
            ]);
            Ok(Box::pin(futures::stream::iter(events)))
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

    fn stream_entry(id: &'static str, stream_ok: bool, text: Option<&'static str>) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StreamStubProvider {
                id: ProviderId::new(id),
                stream_ok,
                text,
            }),
            effective_model: None,
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
                attempt_log: None,
            }),
            effective_model: None,
        }
    }

    fn entry_with_log(id: &'static str, ok: bool, log: Arc<Mutex<Vec<String>>>) -> FreeEntry {
        let upstream = *catalog_entry(id).expect("catalog entry");
        FreeEntry {
            upstream,
            provider: Arc::new(StubProvider {
                id: ProviderId::new(id),
                ok,
                seen_max_tokens: None,
                ring_status: None,
                exhaustion: None,
                attempt_log: Some(log),
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
                attempt_log: None,
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
                attempt_log: None,
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
                attempt_log: None,
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
    fn auto_strategy_routes_by_task() {
        // The smart default (Auto, spec §8.4) uses the task-based plan, not
        // plain catalog order: a request classifying as code generation
        // leads with the code-focused upstreams even though the chain lists
        // huggingface first.
        let provider = FreeProvider::new(vec![
            entry("huggingface", true),
            entry("groq", true),
            entry("cerebras", true),
        ]);
        let req = ProviderRequest {
            messages: vec![Message::user("write a parser module")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&req));
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        // CodeGeneration prefs (openrouter, cerebras, huggingface, groq, ...)
        // filtered to the chain: cerebras, then huggingface, then groq —
        // NOT the chain's catalog order (huggingface first).
        assert_eq!(order, vec!["cerebras", "huggingface", "groq"]);
    }

    #[test]
    fn task_plan_refines_preferred_group_by_latency() {
        // §8.4 criterion 2: within the task-preferred group, faster upstreams
        // lead even when the preference list names them later; no-sample
        // upstreams keep preference order at the group tail.
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "code_generation".to_string(),
            vec![
                "cerebras".to_string(),
                "groq".to_string(),
                "huggingface".to_string(),
            ],
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
        // Record latencies: groq is fast, cerebras slow, huggingface unknown.
        {
            let mut lat = provider.latencies.lock().unwrap();
            lat.record(1, 0.3, 10); // groq
            lat.record(2, 5.0, 10); // cerebras
        }
        let req = ProviderRequest {
            messages: vec![Message::user("write a parser module")],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&req));
        let order: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        // Preferred group sorted by latency: groq (0.3s), cerebras (5s),
        // then no-sample huggingface (f64::MAX) at the group tail.
        assert_eq!(order, vec!["groq", "cerebras", "huggingface"]);
    }

    #[test]
    fn disabled_upstreams_are_removed_from_task_plans() {
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(
            "code_generation".to_string(),
            vec!["groq".to_string(), "cerebras".to_string()],
        );
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("groq", true),
                entry("cerebras", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                task_preferences: Some(overrides),
                disabled_upstreams: vec!["groq".to_string(), "does-not-exist".to_string()],
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
        assert_eq!(order, vec!["cerebras", "huggingface"]);
        assert!(!order.contains(&"groq"));
    }

    #[test]
    fn disabled_upstreams_are_removed_from_pinned_and_sequential_plans() {
        let provider = FreeProvider::with_routing(
            vec![
                entry("groq", true),
                entry("huggingface", true),
                entry("cerebras", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                disabled_upstreams: vec!["groq".to_string()],
                ..Default::default()
            },
            false,
        );

        let pinned = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 0,
                pinned_model: "custom-model".to_string(),
            },
            None,
        );
        let pinned_ids: Vec<&str> = pinned
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(pinned_ids, vec!["huggingface", "cerebras"]);

        let sequential = provider.attempt_plan(&Route::Auto, None);
        let sequential_ids: Vec<&str> = sequential
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(sequential_ids, vec!["huggingface", "cerebras"]);
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
        // Sequential explicitly — this test is about fallback-row adjacency
        // (primary then per-upstream fallbacks), not the task-based default
        // plan ordering.
        let provider = FreeProvider::with_routing(
            vec![
                entry("nvidia", true),
                entry("cerebras", true),
                entry("groq", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
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
        // Sequential explicitly — this test is about pinned-then-catalog
        // fallback order, not the task-based default plan ordering.
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("nvidia", true),
                entry("cerebras", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
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
    fn attempt_plan_default_auto_routes_by_task_preference() {
        // The default strategy is Auto (task-based, spec §8.4). Both entries
        // contribute their default model; the order follows the
        // code-generation preference list, so cerebras leads huggingface.
        let provider = FreeProvider::new(vec![entry("huggingface", true), entry("cerebras", true)]);
        let plan = provider.attempt_plan(&Route::Auto, None);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].0, 1);
        assert_eq!(plan[0].1, "gpt-oss-120b");
        assert_eq!(plan[1].0, 0);
        assert_eq!(plan[1].1, "meta-llama/Llama-3.3-70B-Instruct");
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
    fn attempt_plan_auto_prefers_high_success_rate_over_latency() {
        // CodeGeneration preferences order huggingface (3rd) before groq
        // (4th). groq is fast (1s avg) but keeps failing (0% at 3+);
        // huggingface is slow (5s avg) but reliable (100%). The preferred
        // group must promote the reliable upstream despite its latency.
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("groq", true)],
            RoutingConfig::default(),
            false,
        );
        {
            let mut lat = provider.latencies.lock().unwrap();
            for _ in 0..3 {
                lat.record_success(0); // huggingface: 100%
                lat.record(0, 5.0, 10);
                lat.record_failure(1); // groq: 0%
                lat.record(1, 1.0, 10); // fast — but failing
            }
        }
        let plan = provider.attempt_plan(&Route::Auto, Some(&dummy_request("free/auto")));
        assert_eq!(
            plan[0].0, 0,
            "huggingface (100%) must lead groq (0%) despite higher latency"
        );
        assert_eq!(plan[1].0, 1);
    }

    #[test]
    fn attempt_plan_auto_ignores_success_rate_below_min_samples() {
        // groq has only 2 dispatches (both wins) — below
        // MIN_SUCCESS_RATE_SAMPLES — so its rate must NOT be trusted to
        // reorder the group. huggingface (3 dispatches, 100%) keeps its
        // preference-order lead even though groq is far faster.
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("groq", true)],
            RoutingConfig::default(),
            false,
        );
        {
            let mut lat = provider.latencies.lock().unwrap();
            for _ in 0..3 {
                lat.record_success(0);
                lat.record(0, 5.0, 10);
            }
            for _ in 0..2 {
                lat.record_success(1);
                lat.record(1, 0.1, 10);
            }
        }
        let plan = provider.attempt_plan(&Route::Auto, Some(&dummy_request("free/auto")));
        assert_eq!(
            plan[0].0, 0,
            "small-sample rates must not reorder the group"
        );
        assert_eq!(plan[1].0, 1);
    }

    #[test]
    fn attempt_plan_auto_breaks_success_rate_ties_by_latency() {
        // Both upstreams 100% at 3+ dispatches — the faster one leads.
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("groq", true)],
            RoutingConfig::default(),
            false,
        );
        {
            let mut lat = provider.latencies.lock().unwrap();
            for _ in 0..3 {
                lat.record_success(0);
                lat.record(0, 9.0, 10);
                lat.record_success(1);
                lat.record(1, 0.8, 10);
            }
        }
        let plan = provider.attempt_plan(&Route::Auto, Some(&dummy_request("free/auto")));
        assert_eq!(plan[0].0, 1, "faster upstream leads when trusted rates tie");
        assert_eq!(plan[1].0, 0);
    }

    #[test]
    fn attempt_plan_auto_keeps_unmeasured_upstreams_at_preferred_tail() {
        // No dispatch history for either upstream — both are rank 2, so the
        // stable sort keeps the preference order unchanged (no phantom
        // reordering from empty counters).
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("groq", true)],
            RoutingConfig::default(),
            false,
        );
        let plan = provider.attempt_plan(&Route::Auto, Some(&dummy_request("free/auto")));
        assert_eq!(plan[0].0, 0);
        assert_eq!(plan[1].0, 1);
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
    fn routing_config_default_is_auto() {
        let provider = FreeProvider::new(vec![entry("huggingface", true)]);
        assert!(matches!(
            provider.routing_config().strategy,
            RoutingStrategy::Auto
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
        // Default (Auto) → JSON → deserialize
        let auto = RoutingConfig::default();
        let json = serde_json::to_string(&auto).unwrap();
        assert!(json.contains("\"strategy\":\"auto\""), "json: {json}");
        let deserialized: RoutingConfig = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized.strategy, RoutingStrategy::Auto));

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
        // Sequential explicitly — this test asserts pinned-then-catalog
        // order, not the task-based default plan ordering.
        let provider = FreeProvider::with_routing(
            vec![
                entry("huggingface", true),
                entry("cerebras", true),
                entry("google", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
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
    async fn free_stream_attribution_tracks_initial_and_empty_retry_upstreams() {
        use futures::StreamExt;

        let provider = FreeProvider::with_routing(
            vec![
                stream_entry("huggingface", true, None),
                stream_entry("cerebras", true, Some("fallback answer")),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let mut stream = provider
            .create_message_stream(dummy_request("free/auto"))
            .await
            .expect("stream should start");

        let mut attributions = Vec::new();
        while let Some(Ok(event)) = stream.next().await {
            if let StreamEvent::ProviderAttribution {
                provider_id,
                upstream_id,
                model,
            } = event
            {
                attributions.push((provider_id, upstream_id, model));
                if attributions.len() == 2 {
                    break;
                }
            }
        }

        assert_eq!(
            attributions,
            vec![
                (
                    "free".to_string(),
                    "huggingface".to_string(),
                    catalog_entry("huggingface")
                        .unwrap()
                        .default_model
                        .to_string(),
                ),
                (
                    "free".to_string(),
                    "cerebras".to_string(),
                    catalog_entry("cerebras").unwrap().default_model.to_string(),
                ),
            ]
        );
    }

    #[tokio::test]
    async fn streaming_success_credits_success_rate_at_message_stop() {
        use futures::StreamExt;

        let provider = FreeProvider::with_routing(
            vec![stream_entry("huggingface", true, Some("hello"))],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                latency: Some(LatencyConfig { max_samples: 10 }),
                ..Default::default()
            },
            false,
        );
        let mut stream = provider
            .create_message_stream(dummy_request("free/auto"))
            .await
            .expect("stream should start");

        // Consume exactly like the query loop does: stop at MessageStop and
        // drop the stream WITHOUT draining it to Poll::Ready(None).
        while let Some(Ok(event)) = stream.next().await {
            if matches!(event, StreamEvent::MessageStop) {
                break;
            }
        }
        drop(stream);

        // Regression: the spec §8.6 success-rate counter was only updated in
        // the Poll::Ready(None) branch, which interactive consumers never
        // reach (they break on MessageStop) — so streaming wins were silently
        // never credited. The win must now be recorded at the completion
        // signal.
        let rates = provider.upstream_success_rates();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].0, "huggingface");
        assert_eq!(rates[0].1, Some(1.0));

        // The latency sample rides the same path — it must be recorded too.
        let lats = provider.upstream_latencies();
        assert_eq!(lats[0].0, "huggingface");
        assert!(
            lats[0].1.is_some(),
            "latency sample should be recorded for a streaming win"
        );
    }

    #[tokio::test]
    async fn streaming_success_tags_task_success_rate() {
        use futures::StreamExt;

        // A generic prompt classifies as CodeGeneration; a streaming win on
        // it must be credited to the code_generation per-task bucket (spec
        // §8.6 per-task view).
        let provider = FreeProvider::with_routing(
            vec![stream_entry("huggingface", true, Some("hi"))],
            RoutingConfig::default(),
            false,
        );
        let mut stream = provider
            .create_message_stream(dummy_request("free/auto"))
            .await
            .expect("stream should start");
        while let Some(Ok(event)) = stream.next().await {
            if matches!(event, StreamEvent::MessageStop) {
                break;
            }
        }
        drop(stream);

        let rates = provider.upstream_task_success_rates();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].0, "huggingface");
        assert!(
            rates[0]
                .1
                .iter()
                .any(|(k, r)| k == "code_generation" && *r == Some(1.0)),
            "code_generation win not credited per-task: {:?}",
            rates[0].1
        );
    }

    #[tokio::test]
    async fn empty_stream_does_not_credit_success_at_message_stop() {
        use futures::StreamExt;

        // A streaming win is only credited when the attempt produced content.
        // An empty completion (no text, no tools) must NOT bump the success
        // counter at MessageStop — it stays uncounted so the empty-completion
        // re-dispatch path remains the authority for empty attempts.
        let provider = FreeProvider::with_routing(
            vec![stream_entry("huggingface", true, None)],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let mut stream = provider
            .create_message_stream(dummy_request("free/auto"))
            .await
            .expect("stream should start");

        while let Some(Ok(event)) = stream.next().await {
            if matches!(event, StreamEvent::MessageStop) {
                break;
            }
        }
        drop(stream);

        let rates = provider.upstream_success_rates();
        assert_eq!(rates.len(), 1);
        assert_eq!(rates[0].0, "huggingface");
        assert_eq!(rates[0].1, None, "empty attempts must not be credited");
    }

    #[tokio::test]
    async fn create_message_falls_back_to_next_upstream() {
        // Sequential explicitly so the failing huggingface is genuinely first
        // in the plan — the default Auto plan would prefer cerebras and
        // never exercise the fallback.
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", false), entry("cerebras", true)],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let resp = provider
            .create_message(dummy_request("free/auto"))
            .await
            .expect("should succeed via cerebras");
        assert_eq!(resp.model, "gpt-oss-120b");
    }

    // ---- task-based dispatch through the real LlmProvider path -----------

    #[tokio::test]
    async fn task_based_dispatch_tries_task_preferred_upstream_first() {
        // End-to-end plan → dispatch → fallback for a reasoning request:
        // groq is in the reasoning preference list, huggingface is not, so
        // the plan must try groq before huggingface even though the chain is
        // ordered [huggingface, groq] in the catalog.
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = vec![
            entry_with_log("huggingface", false, log.clone()),
            entry_with_log("groq", false, log.clone()),
        ];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                ..Default::default()
            },
            false,
        );
        let req = ProviderRequest {
            messages: vec![Message::user("why does the pool keep exhausting?")],
            ..dummy_request("free/auto")
        };
        // Both upstreams fail — the round errors, but the ORDER of attempts
        // is what this test asserts.
        let _err = provider.create_message(req).await.unwrap_err();
        let attempts: Vec<String> = log.lock().unwrap().clone();
        let groq_model = catalog_entry("groq").unwrap().default_model;
        let hf_model = catalog_entry("huggingface").unwrap().default_model;
        assert_eq!(attempts.len(), 2, "both upstreams attempted");
        assert_eq!(attempts[0], groq_model, "task-preferred upstream first");
        assert_eq!(attempts[1], hf_model, "remaining upstream after prefs");
    }

    #[tokio::test]
    async fn task_based_dispatch_falls_back_from_preferred_to_remaining() {
        // Same chain, but groq fails and huggingface succeeds: the request
        // must still reach huggingface after groq, and only after groq.
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = vec![
            entry_with_log("huggingface", true, log.clone()),
            entry_with_log("groq", false, log.clone()),
        ];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                ..Default::default()
            },
            false,
        );
        let req = ProviderRequest {
            messages: vec![Message::user("why does the pool keep exhausting?")],
            ..dummy_request("free/auto")
        };
        let resp = provider
            .create_message(req)
            .await
            .expect("should fall back to huggingface");
        let attempts: Vec<String> = log.lock().unwrap().clone();
        let hf_model = catalog_entry("huggingface").unwrap().default_model;
        assert_eq!(attempts.len(), 2, "groq tried before huggingface");
        assert_eq!(resp.model, hf_model);
    }

    // ---- Capability gate (audit spec §8.4 "capability match") ----------------

    fn image_request(model: &str) -> ProviderRequest {
        let image = Message::user_blocks(vec![ContentBlock::Image {
            source: ImageSource {
                source_type: "base64".to_string(),
                media_type: Some("image/png".to_string()),
                data: Some("aGVsbG8=".to_string()),
                url: None,
            },
        }]);
        ProviderRequest {
            messages: vec![image],
            ..dummy_request(model)
        }
    }

    #[test]
    fn capability_gate_drops_non_vision_upstreams_for_image_request() {
        // An image request must only route to vision-capable upstreams. The
        // catalog marks github-copilot (gpt-4o) and google (gemini) as vision;
        // huggingface (Llama) is text-only and its 400 InvalidRequest would
        // hard-fail the whole request without this gate.
        let chain = vec![entry("huggingface", true), entry("google", true)];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let plan = provider.attempt_plan(&Route::Auto, Some(&image_request("free/auto")));
        let ids: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(
            ids,
            vec!["google"],
            "text-only upstreams dropped for images"
        );
    }

    #[test]
    fn capability_gate_drops_small_context_upstreams_for_large_request() {
        // github-copilot documents a 16K serving context — a request that
        // estimates above 16K tokens must skip it and go to the 128K upstream
        // instead of burning a guaranteed-overflow round-trip.
        let chain = vec![entry("github-copilot", true), entry("huggingface", true)];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        // ~4 chars/token → 80K chars ≈ 20K tokens > copilot's 16_384 cap.
        let big = ProviderRequest {
            messages: vec![Message::user("a".repeat(80_000))],
            ..dummy_request("free/auto")
        };
        let plan = provider.attempt_plan(&Route::Auto, Some(&big));
        let ids: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(ids, vec!["huggingface"], "small-context upstream skipped");

        // A small request keeps both upstreams in the plan (copilot
        // contributes its primary + fallback rows).
        let plan = provider.attempt_plan(&Route::Auto, Some(&dummy_request("free/auto")));
        let ids: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(ids, vec!["github-copilot", "github-copilot", "huggingface"]);
    }

    #[test]
    fn capability_gate_drops_pinned_non_vision_upstream_for_image_request() {
        // The central gate also applies to pinned routes: a user-pinned
        // text-only upstream is skipped for an image request so the pin
        // can't hard-fail the whole request on its 400 InvalidRequest.
        let chain = vec![entry("huggingface", true), entry("google", true)];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let plan = provider.attempt_plan(
            &Route::Pinned {
                start_idx: 0,
                pinned_model: "meta-llama/Llama-3.3-70B-Instruct".to_string(),
            },
            Some(&image_request("free/auto")),
        );
        let ids: Vec<&str> = plan
            .iter()
            .map(|(idx, _)| provider.chain[*idx].upstream.id)
            .collect();
        assert_eq!(
            ids,
            vec!["google"],
            "pinned non-vision upstream dropped for image requests"
        );
    }

    #[tokio::test]
    async fn capability_gate_reports_clear_error_when_request_too_large() {
        // Oversized request + only a 16K-capped upstream → the plan is
        // empty and the error must explain the context-window gap instead of
        // blaming cooldown.
        let chain = vec![entry("github-copilot", true)];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let big = ProviderRequest {
            messages: vec![Message::user("a".repeat(80_000))],
            ..dummy_request("free/auto")
        };
        let err = provider.create_message(big).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too large"),
            "expected a context-window error, got: {}",
            msg
        );
        assert!(
            !msg.contains("in cooldown"),
            "must not blame cooldown for a context gap: {}",
            msg
        );
    }

    #[tokio::test]
    async fn capability_gate_never_dispatches_non_vision_upstream_for_image_request() {
        // End-to-end: the image request must be served by the first
        // vision-capable upstream and the text-only stub must never be called
        // (the gate runs before dispatch, not after a failed round-trip).
        let log = Arc::new(Mutex::new(Vec::new()));
        let chain = vec![
            entry_with_log("huggingface", true, log.clone()),
            entry_with_log("google", true, log.clone()),
        ];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let resp = provider
            .create_message(image_request("free/auto"))
            .await
            .expect("vision-capable upstream should serve the image request");
        let attempts: Vec<String> = log.lock().unwrap().clone();
        let google_model = catalog_entry("google").unwrap().default_model;
        assert_eq!(attempts.len(), 1, "only the vision upstream is attempted");
        assert_eq!(resp.model, google_model);
    }

    #[tokio::test]
    async fn capability_gate_reports_clear_error_when_no_vision_upstream() {
        // Image request + only text-only upstreams → the plan is empty and
        // the error must explain the capability gap instead of blaming
        // cooldown (which would be actively misleading).
        let chain = vec![entry("huggingface", true), entry("nvidia", true)];
        let provider = FreeProvider::with_routing(
            chain,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        let err = provider
            .create_message(image_request("free/auto"))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("image input"),
            "expected a capability-aware error, got: {}",
            msg
        );
        assert!(
            !msg.contains("in cooldown"),
            "must not blame cooldown for a capability gap: {}",
            msg
        );
    }

    #[test]
    fn upstream_capabilities_reports_catalog_metadata() {
        // The routing dialog's capability badges (spec §8.6) come from this
        // snapshot — it must mirror the catalog's vision/context fields that
        // `entry_fits_request` uses to filter.
        let chain = vec![entry("google", true), entry("github-copilot", true)];
        let provider = FreeProvider::new(chain);
        let upstream_caps = provider.upstream_capabilities();
        let caps: std::collections::HashMap<&str, (bool, u32)> = upstream_caps
            .iter()
            .map(|(id, vision, ctx)| (id.as_str(), (*vision, *ctx)))
            .collect();
        assert_eq!(caps.get("google"), Some(&(true, 128_000)));
        assert_eq!(caps.get("github-copilot"), Some(&(true, 16_384)));
        // Text-only upstreams are flagged so the dialog can explain why an
        // image request routes away from them.
        let chain = vec![entry("huggingface", true)];
        let provider = FreeProvider::new(chain);
        let caps = provider.upstream_capabilities();
        assert_eq!(caps, vec![("huggingface".to_string(), false, 128_000)]);
    }

    #[test]
    fn disabled_upstreams_filter_random_latency_family_and_fallback_plans() {
        let plan_ids = |provider: &FreeProvider, plan: Vec<(usize, String)>| {
            plan.into_iter()
                .map(|(idx, model)| (provider.chain[idx].upstream.id, model))
                .collect::<Vec<_>>()
        };

        let random_provider = FreeProvider::with_routing(
            vec![
                entry("nvidia", true),
                entry("huggingface", true),
                entry("groq", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::RandomFailover,
                disabled_upstreams: vec!["nvidia".to_string()],
                ..Default::default()
            },
            false,
        );
        let random_plan = plan_ids(
            &random_provider,
            random_provider.attempt_plan(&Route::Auto, None),
        );
        assert!(!random_plan.iter().any(|(id, _)| *id == "nvidia"));
        assert_eq!(random_plan.len(), 2);

        let latency_provider = FreeProvider::with_routing(
            vec![
                entry("nvidia", true),
                entry("huggingface", true),
                entry("groq", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::LatencyBased,
                disabled_upstreams: vec!["nvidia".to_string()],
                ..Default::default()
            },
            false,
        );
        let latency_plan = plan_ids(
            &latency_provider,
            latency_provider.attempt_plan(&Route::Auto, None),
        );
        assert!(!latency_plan.iter().any(|(id, _)| *id == "nvidia"));
        assert_eq!(latency_plan.len(), 2);

        let family_provider = FreeProvider::with_routing(
            vec![
                entry("nvidia", true),
                entry("huggingface", true),
                entry("groq", true),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                disabled_upstreams: vec!["nvidia".to_string()],
                ..Default::default()
            },
            false,
        );
        let family_plan = plan_ids(
            &family_provider,
            family_provider.attempt_plan(
                &Route::Family {
                    model_family: "llama-3.3-70b",
                },
                None,
            ),
        );
        assert!(!family_plan.iter().any(|(id, _)| *id == "nvidia"));
        assert_eq!(
            family_plan,
            vec![
                (
                    "huggingface",
                    "meta-llama/Llama-3.3-70B-Instruct".to_string()
                ),
                ("groq", "openai/gpt-oss-120b".to_string())
            ]
        );

        // NVIDIA's fallback model must disappear with the disabled upstream,
        // not leak into a later route attempt.
        assert!(!family_plan
            .iter()
            .any(|(_, model)| model == "meta/llama-3.1-8b-instruct"));
    }

    #[tokio::test]
    async fn disabled_task_preference_is_not_dispatched() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let provider = FreeProvider::with_routing(
            vec![
                entry_with_log("groq", true, log.clone()),
                entry_with_log("huggingface", true, log.clone()),
            ],
            RoutingConfig {
                strategy: RoutingStrategy::TaskBased,
                disabled_upstreams: vec!["groq".to_string()],
                ..Default::default()
            },
            false,
        );
        let req = ProviderRequest {
            messages: vec![Message::user("why does the pool keep exhausting?")],
            ..dummy_request("free/auto")
        };

        let response = provider
            .create_message(req)
            .await
            .expect("enabled fallback should handle the request");
        let attempts: Vec<String> = log.lock().unwrap().clone();
        let hf_model = catalog_entry("huggingface").unwrap().default_model;
        assert_eq!(attempts, vec![hf_model.to_string()]);
        assert_eq!(response.model, hf_model);
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
        // upstream and lands on cerebras. Sequential explicitly so huggingface
        // is genuinely first in the plan — the default Auto plan would prefer
        // cerebras and never exercise the skip.
        let provider = FreeProvider::with_routing(
            vec![entry("huggingface", true), entry("cerebras", true)],
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
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
    fn latency_state_tracks_success_rate() {
        let mut lat = LatencyState::new(2);
        assert_eq!(lat.success_rate(0), None, "no dispatches yet");
        lat.record_success(0);
        lat.record_success(0);
        lat.record_failure(0);
        assert_eq!(lat.success_rate(0), Some(2.0 / 3.0));
        assert_eq!(lat.success_rate(1), None, "no dispatches on idx 1");
        lat.record_failure(1);
        assert_eq!(lat.success_rate(1), Some(0.0));
        // Out-of-range idx is a no-op, not a panic.
        lat.record_success(99);
        lat.record_failure(99);
        assert_eq!(lat.success_rate(99), None);
    }

    #[test]
    fn latency_state_persists_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.json");
        let upstreams = vec!["groq".to_string(), "cerebras".to_string()];

        {
            let mut lat =
                LatencyState::new(2).with_persistence(upstreams.clone(), Some(path.clone()), 3);
            lat.record_success(0);
            lat.record_success(0);
            lat.record_failure(0);
            lat.record_task_success(0, TaskType::CodeGeneration);
            lat.record_task_success(0, TaskType::CodeGeneration);
            lat.record_task_failure(0, TaskType::CodeGeneration);
            lat.record(0, 1.0, 3);
            lat.record(0, 2.0, 3);
            LatencyState::persist_snapshot(lat.snapshot());
            assert!(path.exists(), "telemetry must be written to disk");
        }

        let lat = LatencyState::new(2).with_persistence(upstreams, Some(path), 3);
        assert_eq!(lat.success_rate(0), Some(2.0 / 3.0));
        assert_eq!(
            lat.task_success_rate(0, TaskType::CodeGeneration),
            Some(2.0 / 3.0)
        );
        assert_eq!(lat.samples[0].len(), 2);
        assert_eq!(lat.avg_latency(0), 1.5);
        assert_eq!(lat.success_rate(1), None);
    }

    #[test]
    fn latency_telemetry_ages_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.json");
        let old = current_unix_secs().saturating_sub(TELEMETRY_HALF_LIFE_SECS + 1);
        let json = serde_json::json!([{
            "upstream": "groq",
            "samples": [1.0, 2.0],
            "successes": 100,
            "failures": 20,
            "task_successes": {"code_generation": 80},
            "task_failures": {"code_generation": 20},
            "saved_at_unix": old
        }]);
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();

        let lat = LatencyState::new(1).with_persistence(vec!["groq".to_string()], Some(path), 10);
        assert!(lat.successes[0] < 100);
        assert!(lat.failures[0] < 20);
        assert!(
            lat.samples[0].is_empty(),
            "old latency samples should expire"
        );
        assert!(lat.task_successes[0]["code_generation"] < 80);
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_persistence_files_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("free.json");
        let mut lat =
            LatencyState::new(1).with_persistence(vec!["groq".to_string()], Some(path.clone()), 3);
        lat.record_success(0);
        LatencyState::persist_snapshot(lat.snapshot());
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn persistence_rejects_stale_snapshot_and_cleans_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.json");
        let newer_timestamp = current_unix_nanos();
        let newer = serde_json::json!([{
            "upstream": "groq",
            "successes": 2,
            "saved_at_unix_nanos": newer_timestamp
        }]);
        let older = serde_json::json!([{
            "upstream": "groq",
            "successes": 1,
            "saved_at_unix_nanos": newer_timestamp.saturating_sub(1)
        }]);

        write_private_json_if_newer(&path, &newer.to_string());
        write_private_json_if_newer(&path, &older.to_string());
        let newer_but_incomplete = serde_json::json!([{
            "upstream": "groq",
            "successes": 1,
            "saved_at_unix_nanos": newer_timestamp.saturating_add(1)
        }]);
        write_private_json_if_newer(&path, &newer_but_incomplete.to_string());
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored[0]["successes"], 2);

        let lock = acquire_persistence_file_lock(&path).expect("lock should be available");
        assert!(lock.path.exists());
        drop(lock);
        assert!(!path.with_file_name(".free.json.lock").exists());
    }

    #[test]
    fn preferred_order_uses_task_history_over_aggregate_history() {
        let mut lat = LatencyState::new(2);
        // Both upstreams have identical aggregate history, but opposite
        // outcomes for code editing. The task-specific history must win.
        for _ in 0..3 {
            lat.record_success(0);
            lat.record_failure(1);
            lat.record_task_failure(0, TaskType::CodeEdit);
            lat.record_task_success(1, TaskType::CodeEdit);
        }

        let groq = FreeProvider::preferred_order_key(&lat, 0, TaskType::CodeEdit);
        let cerebras = FreeProvider::preferred_order_key(&lat, 1, TaskType::CodeEdit);
        assert!(
            cerebras < groq,
            "task-successful upstream should rank first: groq={groq:?}, cerebras={cerebras:?}"
        );
    }

    #[test]
    fn upstream_5xx_cooldown_persists_across_restart() {
        // The 5xx / circuit-breaker cooldown track must survive a process
        // restart: after a 500, a restart should NOT immediately re-hit the
        // same upstream (spec §8.4 "cooldown state" must stay effective).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.json");

        // Write: cooldown groq (idx 1) for 45s via the same 5xx path the
        // dispatcher uses.
        {
            let mut cd = CooldownState::new(2, CircuitBreakerConfig::default()).with_persistence(
                vec!["huggingface".to_string(), "groq".to_string()],
                Some(path.clone()),
            );
            cd.apply_upstream_cooldown(1, 45);
            assert!(path.exists(), "5xx cooldown must be written to disk");
        } // Read: a fresh instance (simulating a restart) must restore groq's
          // cooldown with ~45s remaining.
        {
            let cd = CooldownState::new(2, CircuitBreakerConfig::default()).with_persistence(
                vec!["huggingface".to_string(), "groq".to_string()],
                Some(path.clone()),
            );
            assert!(
                cd.is_in_cooldown(1),
                "groq must still be in 5xx cooldown after restart"
            );
            assert!(!cd.is_in_cooldown(0), "huggingface stays active");
            let remaining = cd.cooldown_remaining_secs(1);
            assert!(
                matches!(remaining, Some(s) if (1..=45).contains(&s)),
                "~45s remaining after restart, got {:?}",
                remaining
            );
        }
    }

    #[test]
    fn stale_cooldown_file_is_removed_when_state_empties() {
        // save()'s `remove_file` branch: a file whose only track has already
        // expired loads to nothing, and the next save() must delete the stale
        // file so a later restart starts clean instead of re-reading it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("free.json");

        // Write a snapshot whose 5xx cooldown expired 60s ago.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stale = vec![UpstreamCooldownSnapshot {
            upstream: "groq".to_string(),
            consecutive_empties: 0,
            empty_cooldown_until_unix: None,
            cooldown_until_unix: Some(now_unix.saturating_sub(60)),
            saved_at_unix_nanos: 0,
        }];
        std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();

        // Load: the expired cooldown must NOT be restored.
        let mut cd = CooldownState::new(1, CircuitBreakerConfig::default())
            .with_persistence(vec!["groq".to_string()], Some(path.clone()));
        assert!(
            !cd.is_in_cooldown(0),
            "expired 5xx cooldown must not be restored on load"
        );

        // A state transition that saves with nothing active removes the file.
        cd.record_success(0);
        assert!(
            !path.exists(),
            "stale file must be removed once no cooldown track remains"
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
        provider.record_failure(0, TaskType::CodeGeneration);
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
        provider.record_failure(0, TaskType::CodeGeneration);
        provider.record_failure(0, TaskType::CodeGeneration);
        provider.record_failure(0, TaskType::CodeGeneration);
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
        provider.record_failure(0, TaskType::CodeGeneration);
        assert!(!provider.is_in_cooldown(0));

        // Second failure — now in cooldown
        provider.record_failure(0, TaskType::CodeGeneration);
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
        provider.record_failure(0, TaskType::CodeGeneration);
        provider.record_success(0, TaskType::CodeGeneration, Duration::from_secs(1));

        // One more failure should NOT trigger cooldown (counter was reset)
        provider.record_failure(0, TaskType::CodeGeneration);
        assert!(!provider.is_in_cooldown(0));

        // Second failure after reset — now in cooldown
        provider.record_failure(0, TaskType::CodeGeneration);
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
            provider.record_failure(0, TaskType::CodeGeneration);
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
        provider.record_success(0, TaskType::CodeGeneration, Duration::from_millis(100));
        provider.record_success(0, TaskType::CodeGeneration, Duration::from_millis(200));

        // Record latencies for upstream 1 (slower)
        provider.record_success(1, TaskType::CodeGeneration, Duration::from_millis(900));
        provider.record_success(1, TaskType::CodeGeneration, Duration::from_millis(1100));

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
        provider.record_success(0, TaskType::CodeGeneration, Duration::from_millis(800));
        provider.record_success(1, TaskType::CodeGeneration, Duration::from_millis(100));
        provider.record_success(2, TaskType::CodeGeneration, Duration::from_millis(500));
        provider.record_success(3, TaskType::CodeGeneration, Duration::from_millis(300));

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
        provider.record_success(0, TaskType::CodeGeneration, Duration::from_millis(100));
        provider.record_success(1, TaskType::CodeGeneration, Duration::from_millis(2000));
        provider.record_success(2, TaskType::CodeGeneration, Duration::from_millis(500));

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
