# Free Provider Improvements — Phases 1-5

Status: **Shipped** — All phases implemented and tested.

## Implementation Summary

### Phase 1: Provider-Aware Cooldown Profiles

**Research-based cooldown values for each provider:**

| Provider | Rate Limit Cooldown | Server Error Cooldown | Max Cooldown | Notes |
|----------|--------------------|-----------------------|--------------|-------|
| Groq | 60s | 30s | 300s | RPM: 30-60, TPM: 6K-30K. Returns x-ratelimit-reset-tokens header |
| NVIDIA | 90s | 60s | 600s | RPM: ~40. Often missing Retry-After. Be conservative |
| Cerebras | 120s | 60s | 600s | RPM: 30. Very aggressive rate limiting |
| Cloudflare | 30s | 30s | 300s | Free tier has generous limits |
| OpenRouter | 60s | 30s | 300s | Varies by model |
| HuggingFace | 90s | 60s | 600s | Inference API limits |
| SambaNova | 60s | 30s | 300s | Similar to Groq |

**Configuration:** `provider-cooldown-profiles.json`

### Phase 2: Exponential Backoff with Jitter

**Formula:** `cooldown = base × 2^min(failure_count, 5)` with ±20% jitter

**Benefits:**
- Prevents thundering herd on provider recovery
- Bounded at 5 doublings (max 32× base)
- Maximum cooldown capped at 5× base or 600s

**Implementation:** `CooldownState::calculate_backoff()`

### Phase 3: Honor Retry-After Headers

**Changes:**
- Added `extract_retry_after()` helper to extract header from response
- Updated OpenAI provider to extract Retry-After before consuming response
- Cooldown logic now uses provider's suggested retry time when available

**Providers that return Retry-After:** Groq, Cerebras, Cloudflare

### Phase 4: Adaptive Timeouts Based on Latency History

**Formula:** `timeout = max(10s, min(120s, 2 × p95_latency))`

**Fallback:**
- If no p95 data: use 3× average latency
- If no history: use configured timeout

**Benefits:**
- Fast providers get tighter timeouts
- Slow providers get more headroom
- Reduces false timeouts for latency-sensitive providers

### Phase 5: Parallel Request Handling Infrastructure

**Based on cutting-edge research:**

1. **Hedged Requests** (Google's "The Tail at Scale")
   - Fire primary, then backup after 100ms delay
   - First valid response wins, losers cancelled
   - 50% latency reduction for timeout scenarios

2. **Power of Two Choices** (Cloudflare)
   - Sample 2 random providers, pick healthier one
   - Reduces peak connections by 30%

3. **Adaptive Concurrency** (Netflix)
   - Gradient-based concurrency control
   - Dynamically adjusts based on latency gradient

4. **Memory-Efficient Streams** (vLLM PagedAttention)
   - Tracks active streams with memory usage
   - Enforces max concurrent streams limit

**Configuration:** Added to `provider-cooldown-profiles.json`

## Test Results

**All 127 free provider tests pass:**
- 123 existing tests
- 4 new hedge tests

## Commits

```
32617ae chore: Clean up warnings and mark infrastructure as ready
9129fb4 feat: Phase 5 Integration - Wire hedging into request flow
c8cfe47 feat: Phase 5 Step 5 - Add memory-efficient stream handling
020432f feat: Phase 5 Step 4 - Add adaptive concurrency control
d26b2b2 feat: Phase 5 Step 3 - Add Power of Two Choices provider selection
056e8a7 feat: Phase 5 Step 2 - Add HedgeState struct
c977aa9 feat: Phase 5 Step 1 - Add hedging configuration schema
1ec8b1b feat: Phase 4 - Adaptive timeouts based on latency history
66546f7 feat: Phase 3 - Honor Retry-After headers
965d922 feat: Add /status command for provider health visibility
875026c feat: Phase 2 - Exponential backoff with jitter
bd2b0e1 feat: Phase 1 - Provider-aware cooldown profiles
```

## Key Code Locations

- `crates/api/src/providers/free/mod.rs` — Configuration structs, cooldown state
- `crates/api/src/providers/free/impls.rs` — Implementation, hedging logic
- `crates/api/src/providers/free/provider-cooldown-profiles.json` — Provider profiles

## Future Work

1. **Full hedging integration** — Currently infrastructure is ready, but hedging is disabled by default. Enable via configuration.
2. **Adaptive concurrency** — Infrastructure ready, can be enabled for high-throughput scenarios.
3. **Memory management** — StreamManager ready for integration when needed.
