# sub2api Architecture Reference

A reference for architectural patterns from the [sub2api](https://github.com/Wei-Shaw/sub2api) Go project that are relevant to Clawde's FreeProvider fallback chain. Use this when improving routing, failover, or protocol bridging in `crates/api/src/providers/free/`.

## Overview

sub2api is a production AI API gateway (Go + Postgres + Redis) that manages multi-account quota distribution across Claude, OpenAI, Gemini, and Grok upstreams. It is architecturally different from Clawde (server vs local TUI, multi-tenant vs single-user), but several of its patterns informed improvements to the FreeProvider.

## Patterns Already Applied to Clawde

### 1. Same-upstream retry with exponential backoff (from failover_loop.go)

**Source:** `backend/internal/handler/failover_loop.go`

sub2api distinguishes between two failover actions:
- `RetryableOnSameAccount` — retry the same account with backoff (500ms base, 2x, capped at 8s) before switching
- `ShouldRetryNextAccount` — switch to a different account

The error carries flags: `RetryableOnSameAccount`, `SameAccountRetryMax`, `SameAccountRetryDeadline`.

**Applied to Clawde in:** `crates/api/src/providers/free/impls.rs` — the `fallback_retries` config field (previously dead) is now wired. When `err.recovery_class().may_retry_same_provider()` returns true (RateLimited, TransientProvider), the dispatch loop retries the same upstream with exponential backoff before advancing to the next plan entry. Timeouts also retry the same upstream.

**Helper:** `same_upstream_retry_delay_ms(retry_count)` — 500ms base, 2x per retry, capped at 8s.

### 2. TTFT (time-to-first-token) tracking (from openai_account_scheduler.go)

**Source:** `backend/internal/service/openai_account_scheduler.go`

sub2api tracks per-account EWMA error rate and time-to-first-token (TTFT), using both as weighted factors in candidate scoring. TTFT is a better UX proxy than total latency.

**Applied to Clawde in:** `crates/api/src/providers/free/mod.rs` — `LatencyState` now tracks `ttft_samples` alongside total latency samples. TTFT is recorded in `RetryingFreeStream` when `first_byte_received` transitions to true. The `preferred_order_key` uses `avg_ttft` as a secondary tiebreaker after success rate and total latency.

## Patterns Studied But NOT Applied

### 3. EWMA-based runtime stats (not applied — sliding window is sufficient)

sub2api uses `updateEWMAAtomic` for lock-free EWMA updates on error rate and TTFT. Clawde's `LatencyState` uses a fixed sliding window with `max_samples`. EWMA would be simpler (one atomic) and more responsive, but the sliding window gives percentile data (p95) which EWMA cannot. The tradeoff favors keeping the sliding window.

### 4. Weighted random selection with xorshift RNG (not applied — unnecessary complexity)

sub2api's `buildOpenAIWeightedSelectionOrder` uses xorshift64* RNG for deterministic-but-weighted account selection, shifting scores to positive range and doing weighted sampling without replacement. Clawde already has `RoutingStrategy::RandomFailover` (plain shuffle) and the weighted approach adds complexity without clear benefit for a 13-upstream local chain.

### 5. apicompat protocol bridge (not applied — Clawde already has transformers)

sub2api's `internal/pkg/apicompat/` is a comprehensive bidirectional converter between Anthropic Messages, OpenAI Responses, and Chat Completions formats, including streaming SSE event translation and encrypted reasoning signature round-tripping. Clawde's `transformers/` module (`anthropic.rs`, `openai_chat.rs`) and the `StreamEvent` enum already handle the common cases. The signature/encrypted-content round-trip is only relevant for reasoning models, which the free chain doesn't serve.

### 6. Concurrency slot management (not applicable — server-only)

sub2api's `ConcurrencyHelper` manages Redis-backed distributed locking, wait queues with exponential backoff + jitter, and SSE ping keepalives during slot acquisition. This is inherently a server feature. Clawde's `AdaptiveConcurrency` and `StreamManager` are scaffolded as `#[allow(dead_code)]` for the same reason: a local TUI doesn't need distributed concurrency control.

### 7. Account scheduling with heap-based top-K (not applicable — requires database)

sub2api's `defaultOpenAIAccountScheduler` uses a min-heap to select top-K candidates by weighted score (priority, load, queue depth, error rate, TTFT, quota headroom, upstream cost, reset proximity). This requires a database of accounts with real-time load info. Clawde's `CapacityState` provides a simpler rank (0-3) that demotes high-utilization upstreams without hard-skipping.

## Key Files in sub2api for Reference

| File | Pattern |
|---|---|
| `handler/failover_loop.go` | Same-account retry, exponential backoff, switch counting |
| `service/openai_account_scheduler.go` | EWMA stats, TTFT, weighted scoring, top-K selection |
| `pkg/apicompat/types.go` | Anthropic/OpenAI Responses/Chat Completions type definitions |
| `pkg/apicompat/responses_to_anthropic.go` | Streaming SSE event translation between protocols |
| `handler/gateway_helper.go` | Concurrency slots, exponential backoff with jitter, SSE pings |
| `service/proxy_fallback.go` | Proxy chain fallback with cycle detection |

## Clawde Implementation Locations

| Component | File | What changed |
|---|---|---|
| `same_upstream_retry_delay_ms()` | `crates/api/src/providers/free/impls.rs` | New helper: exponential backoff for same-upstream retries |
| `create_message` dispatch loop | `crates/api/src/providers/free/impls.rs` | Rewired from `for` to `while` with VecDeque, same-upstream retry on transient errors and timeouts |
| `create_message_stream` dispatch loop | `crates/api/src/providers/free/impls.rs` | Same retry logic, plus `pos` tracking for remaining plan slice |
| `preferred_order_key()` | `crates/api/src/providers/free/impls.rs` | Now returns `(u8, f64, f64, f64)` — added TTFT as 4th tuple element |
| `LatencyState.ttft_samples` | `crates/api/src/providers/free/mod.rs` | New sliding window for TTFT samples |
| `LatencyState::record_ttft()` | `crates/api/src/providers/free/mod.rs` | New method to record a TTFT sample |
| `LatencyState::avg_ttft()` | `crates/api/src/providers/free/mod.rs` | New method: average TTFT for routing |
| `LatencyState::percentile_ttft()` | `crates/api/src/providers/free/mod.rs` | New method: percentile TTFT (for future first-byte timeout) |
| `RetryingFreeStream` poll loop | `crates/api/src/providers/free/impls.rs` | Records TTFT when `first_byte_received` transitions to true |
| `UpstreamTelemetrySnapshot` | `crates/api/src/providers/free/mod.rs` | Added `ttft_samples` field with `#[serde(default)]` for backward compatibility |
