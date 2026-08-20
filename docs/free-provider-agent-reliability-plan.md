# Free Provider and Agent Reliability Implementation Plan

Status: active implementation plan
Date: 2026-08-19
Scope: Free Mode routing, multi-key rotation, provider failover, streaming continuity, and the agent/tool execution loop.

This plan follows the research review completed on 2026-08-19. It deliberately separates deterministic reliability work from later learned routing. The existing checkout already contains several completed audit fixes; this document records what remains and the acceptance criteria for each phase.

### Implementation checkpoint

- Phase 1 typed recovery kernel: implemented in `crates/api/src/provider_error.rs` and consumed by Free Mode.
- Phase 2 attempt boundary: initial protection implemented; transport metadata no longer commits a stream before generated output is emitted.
- Phase 3 capacity routing: upstream-level observation, reset/retry timing, native Anthropic/Gemini response capture, exact rotating-key attribution, local estimates, and compact command/TUI status exposure are implemented; richer dedicated visualization remains follow-up work.
- Phase 6 mock-provider harness: the deterministic local SSE mock (scripted pre-first-byte failures and mid-stream truncation) is implemented in `crates/api/tests/common/mock_provider.rs` and exercised by `crates/api/tests/free_recovery.rs`; learned routing and the remaining injection modes (latency / quota / tool side effects) remain follow-up work.
- Phase 2 replay-safety hardening: every adapter now accumulates visible output and attaches it as `StreamError::partial_response` (via the shared `StreamEvent::committed_output_text` helper), so a mid-stream failure classifies as `VisibleStreamFailure` (replay-unsafe) for any caller, not just `RetryingFreeStream`'s first-byte watchdog. Covered: openai_compat, openai, azure, anthropic, minimax, google, cohere, copilot, codex, bedrock.
- Remaining phases: not started in this implementation pass.

## Design principles

1. **Hard constraints precede ranking.** Capability, authentication, context size, tool support, cooldown, and safety constraints must be resolved before latency or quality scores.
2. **Diagnose before recovery.** A retry or fallback decision must use a typed recovery class, not a generic string match.
3. **A provider attempt is isolated.** Only a completed attempt may mutate the committed conversation transcript.
4. **Never replay visible output.** Once streamed output is visible, the same logical request must not be silently replayed through another provider.
5. **Transient does not mean invalid.** Provider availability, quota, rate limits, and credential validity are separate state dimensions.
6. **Keep the agent loop simple.** Prefer deterministic workflows and stronger tool contracts over an additional hierarchy of autonomous agents.
7. **Measure repeated outcomes.** Reliability requires repeated-run and continuity metrics, not only one successful response.

## Existing implementation to preserve

The current code already includes these relevant capabilities and they should not be reimplemented:

- Exact key-slot attribution under concurrent rotation.
- Bounded key-ring health checks and bounded-concurrent health polling.
- Typed `Valid` / `Invalid` / `Transient` key-probe verdicts.
- Persistence and private atomic writes for key-ring and cooldown state.
- Context-overflow fallback when the provider identifies an overflow.
- First-byte and mid-stream replay protections.
- Out-of-band empty-completion handling.
- Time-decayed routing telemetry and stale telemetry cleanup.
- Provider/model attribution and per-turn observability.

## Phase 1 — Typed recovery kernel

### Goal

Create one provider-independent recovery taxonomy used by Free Mode today and available to the query/tool loop later.

### Work

- Add `RecoveryClass` to the API error layer.
- Classify every `ProviderError` into a stable class:
  - `InvalidCredential`
  - `RateLimited`
  - `QuotaExhausted`
  - `TransientProvider`
  - `ContextOverflow`
  - `UnsupportedCapability`
  - `MalformedRequest`
  - `ContentFiltered`
  - `ModelUnavailable`
  - `Unknown`
- Add explicit policy helpers for:
  - whether another upstream may be tried;
  - whether the same provider may be retried;
  - whether a key should be cooled down;
  - whether the request is safe to replay;
  - whether a visible stream prevents replay.
- Replace Free Mode's local `should_fallback` string/pattern policy with the shared classification, retaining the special rule that context overflow may fall through to a larger upstream.

### Acceptance criteria

- Every `ProviderError` maps to exactly one recovery class.
- Existing fallback behavior remains unchanged except for tests that expose a classification bug.
- Context overflow remains fallbackable; ordinary invalid requests and content filters do not.
- Unit tests cover all variants and policy decisions.

## Phase 2 — Attempt transaction and continuity contract

### Goal

Make provider attempts explicit and prove that fallback cannot duplicate committed assistant output or tool side effects.

### Work

- Add an attempt record containing request ID, provider, upstream, model, key slot when available, start/end time, recovery class, and visible-output state.
- Keep attempt-local streamed text, thinking, and tool-call buffers separate from committed conversation state.
- Commit assistant content only after a successful completion event.
- Do not replay after visible output or after a non-idempotent tool call.
- Preserve stable message and tool-call IDs across provider conversion.
- Add mock-provider integration scenarios for pre-first-byte failure, mid-stream failure, context overflow, tool-call failover, and concurrent requests.

### Metrics

- Continuity Preservation Rate.
- Continuity Latency Overhead.
- Duplicate visible-output rate.
- Duplicate tool-call rate.
- Fallback success rate.

## Phase 3 — Conservative quota-aware routing

### Goal

Use known capacity information before dispatch without misclassifying stale or missing headers.

### Work

- Introduce a persisted `CapacityObservation` separate from credential validity.
- Capture rate-limit observations from normal responses, not only health probes.
- Track request/token remaining values, reset time, retry-after, timestamp, and confidence.
- Start with upstream-level observations; add key-level observations only when the provider stream carries an unambiguous key slot.
- Demote near-exhausted entries before dispatch; hard-skip only on explicit exhaustion/reset signals.
- Apply TTL expiration and never interpret missing headers as zero capacity.
- Add local sliding-window request/token estimates for providers without useful headers; cold-start key selection should continue to treat missing observations as neutral.
- Expose a concise capacity reason in `/keys health`, `/routing`, and the TUI status view.

### Current implementation slice

- `CapacityState` stores fresh per-upstream token/request utilization observations separately from key health and circuit-breaker state.
- Streaming `RateLimitHeaders` events and completed responses from OpenAI-compatible adapters update the selected upstream's observation.
- Native Anthropic stream metadata is retained when its stream is assembled into a completed response, and Gemini captures the same standard utilization/timing headers on both complete and streaming paths when present.
- Key-rotating providers annotate completed and streaming observations with the exact selected slot; Free Mode persists key-level observations alongside the upstream aggregate without treating them as credential health.
- Within a live process, `KeyRotatingProvider` uses fresh key observations to prefer lower-utilization active keys, while equal-ranked keys remain round-robin and stale/reset observations return to the neutral rank.
- Fresh observations softly demote utilization tiers at 60%, 80%, and 95%; stable ordering preserves the existing route order within a tier.
- `Retry-After` and provider reset headers are normalized to delta/Unix timing metadata; a known reset expires the observation before the general TTL.
- Observations persist privately under `capacity-state/free.json` when Free Mode persistence is enabled and expire after 15 minutes.
- Explicit catalog limits enable a conservative local sliding-window estimate for providers without usable headers. Current declarations are limited to Groq (1K requests/day), Cerebras (5 RPM/30K TPM), and SambaNova (20 RPM/200K TPD); ambiguous or provider-specific units remain neutral.
- Local request accounting happens at dispatch, adds known output usage at completion, resets at each declared window, and never overrides a fresh server-derived observation.
- Explicit pinned routes are preserved; capacity ordering applies to automatic and family routes.
- `/keys health`, `/routing`, and the `/stats` live key-health view show fresh capacity signals only when available, with `headers` versus `local` provenance and known reset timing; missing or expired state stays quiet.

### Acceptance criteria

- A recently rate-limited entry is demoted without being marked credential-invalid.
- Stale observations expire and do not permanently remove an upstream.
- The route planner remains deterministic for equal observations.
- Persistence is private, atomic, bounded, and migration-safe.

## Phase 4 — Tool contract and agent safety

### Goal

Make recovery decisions safe around tools and untrusted external data.

### Work

- Add tool metadata for read-only/mutating, idempotent/non-idempotent, retry-safe/never-retry, approval requirement, and workspace scope.
- Permit automatic retries only for retry-safe operations.
- Treat MCP and tool output as untrusted data, never as instructions.
- Require provenance and revocation metadata for persistent memory.
- Enforce workspace roots and network policy outside the model.
- Add bounded structured tool-result output and artifact references for large results.
- Add a compact repository map to reduce context pressure before introducing more planning agents.

## Phase 5 — Durable execution journal

### Goal

Make long-running or interrupted agent runs resumable and auditable.

### Work

- Record run and attempt lifecycle events in the existing session storage layer.
- Persist checkpoints around mutating tool calls.
- Detect incomplete tool calls on resume and fail closed rather than replaying blindly.
- Keep provider fallback attempts separate from committed transcript entries.
- Add `/diagnostics` output for the latest run state and recovery reason.

## Phase 6 — Evaluation and adaptive routing

### Goal

Only introduce learned routing after Clawde has local evidence.

### Work

- Build a deterministic local mock-provider harness with configurable latency, quota, failures, stream interruptions, and tool side effects. (Done for failures + stream interruption — `crates/api/tests/common/mock_provider.rs`; latency / quota / tool side-effect injection remain.)
- Measure pass@1 and repeated-run pass^k reliability.
- Compare every adaptive policy against the best fixed-provider baseline.
- Add minimum-sample gates, time decay, confidence thresholds, and a safe fallback when router confidence is low.
- Consider constrained contextual-bandit routing only after held-out evaluation data exists.
- Keep exploration disabled by default and cap exploration budget explicitly.

## Deferred by design

These should not be wired speculatively:

- Always-on parallel speculative requests.
- Unbounded provider hedging.
- A multi-agent hierarchy for ordinary coding turns.
- Learned routing without local outcome labels.
- Automatic retries after non-idempotent tool calls.
- Persistent memory without provenance and revocation.

## Validation policy

Every phase must include:

1. Focused unit tests for classification and state transitions.
2. Mock-provider integration tests for fallback and concurrency.
3. `cargo fmt --all -- --check`.
4. `cargo check --workspace`.
5. `cargo clippy --workspace --all-targets -- -D warnings`.
6. Relevant package tests, followed by `cargo test --workspace` for cross-crate changes.
7. A focused diff review that excludes unrelated working-tree modifications.

## Research references

- Anthropic, [Building Effective Agents](https://www.anthropic.com/engineering/building-effective-agents)
- OpenAI, [Agents SDK](https://openai.github.io/openai-agents-python/)
- [RouteLLM](https://arxiv.org/abs/2406.18665)
- [FrugalGPT](https://arxiv.org/abs/2305.05176)
- [LLMRouter](https://arxiv.org/abs/2608.06867)
- [WISERouter](https://arxiv.org/abs/2607.23765)
- [ContinuityBench](https://arxiv.org/abs/2607.15899)
- [Diagnosis Before Recovery](https://arxiv.org/abs/2608.11772)
- [SWE-agent](https://arxiv.org/abs/2405.15793)
- [OpenHands](https://arxiv.org/abs/2407.16741)
- [Aider repository map](https://aider.chat/docs/repomap.html)
- [tau-bench](https://arxiv.org/abs/2406.12045)
- [ToolSandbox](https://arxiv.org/abs/2408.04682)
- [AgentDojo](https://arxiv.org/abs/2406.13352)
- [Model Context Protocol specification](https://modelcontextprotocol.io/specification/2025-06-18)

The 2026 papers are recent arXiv preprints as of this plan date. They are useful design signals, not substitutes for local production evaluation.
