# Multi-Key Rotation & FreeProvider Aggregator — Architecture Analysis

## System Architecture Overview

### Two-Level Fallback Hierarchy

```
Level 1: FreeProvider (across providers)
  ├── upstream[0]:  Groq          (may wrap KeyRotatingProvider inside)
  ├── upstream[1]:  Cerebras      (may wrap KeyRotatingProvider inside)
  ├── upstream[2]:  Google Gemini (may wrap KeyRotatingProvider inside)
  ├── upstream[3]:  Mistral       (may wrap KeyRotatingProvider inside)
  ├── upstream[4]:  SambaNova     (may wrap KeyRotatingProvider inside)
  ├── upstream[5]:  NVIDIA        (may wrap KeyRotatingProvider inside)
  ├── upstream[6]:  Cohere        (may wrap KeyRotatingProvider inside)
  ├── upstream[7]:  OpenRouter    (may wrap KeyRotatingProvider inside)
  ├── upstream[8]:  OpenCode Zen  (may wrap KeyRotatingProvider inside)
  ├── upstream[9]:  Z.AI          (may wrap KeyRotatingProvider inside)
  └── upstream[10]: Zhipu         (may wrap KeyRotatingProvider inside)

Level 2: KeyRotatingProvider (within each upstream, when 2+ keys)
  └── key[0], key[1], key[2], ...  (rotates on exhaustion)
```

### Component Map

| Component | File | Role |
|---|---|---|
| **KeyRing** | `crates/core/src/key_ring.rs` | In-memory state machine: tracks which keys are usable vs in cooldown, round-robins, persists to disk |
| **KeyRotatingProvider** | `crates/api/src/providers/key_rotating.rs` | `LlmProvider` wrapper — wraps any underlying provider with automatic rotation on exhaustion |
| **AuthStore** | `crates/core/src/auth_store.rs` | JSON store at `~/.clawde/auth.json` — holds both `credentials` (single-key, legacy) and `keys` (multi-key) maps |
| **KeysCommand** | `crates/commands/src/keys.rs` | `/keys` slash command — `set`, `add`, `remove`, `list`, `health` subcommands |
| **ProviderRegistry** | `crates/api/src/registry.rs` | Wires everything together at startup — detects 2+ keys and wraps in `KeyRotatingProvider` |
| **time_extract** | `crates/api/src/time_extract.rs` | Parses `Retry-After` headers, ISO 8601 timestamps, and free-text patterns for cooldown estimation |
| **FreeProvider** | `crates/api/src/providers/free.rs` | Composite aggregator that chains upstream providers with ordered fallback |

---

## Algorithm 1: Chain Assembly (`build_free_provider`)

**Location:** `crates/api/src/registry.rs` lines 100–195

**Purpose:** Build the ordered chain of upstream providers at startup.

### Pseudocode

```
for each upstream in FREE_CATALOG (in priority order):
    multi_keys = auth_store.keys_for(upstream.id).filter(|k| k.len() > 1)

    if multi_keys exists:
        → wrap in KeyRotatingProvider
        → push to chain
        → continue to next upstream

    else:
        key = auth_store.api_key_for(upstream.id)
        if key exists and non-empty:
            → build single-provider instance
            → push to chain
        else:
            → skip this upstream entirely
```

### Key Decisions

| Decision | Rationale |
|---|---|
| **`len > 1` threshold** | Exactly 1 key → no rotation. 2+ keys → `KeyRotatingProvider`. No minimum beyond 2. |
| **OpenCode Zen/Go key sharing** | Checks both `auth_store.keys_for("opencode-zen")` and `auth_store.keys_for("opencode-go")` because they share the same underlying API |
| **Silent skip on error** | Unlike `runtime_provider_for()` which panics if `provider_from_key` fails, `build_free_provider()` silently continues to the next catalog entry |
| **Catalog order = fallback priority** | `FREE_CATALOG` is ordered with Groq first (fastest/most generous) and Zhipu last — this ordering IS the fallback priority |

### Complexity

- **Time:** O(K × P) where P = number of catalog entries (≤ 11) and K = keys per provider
- **Space:** O(P)
- **Frequency:** Once at startup

---

## Algorithm 2: Route Resolution (`resolve_route`)

**Location:** `crates/api/src/providers/free.rs` lines 188–226

**Purpose:** Map a user-facing model ID string to a `Route` enum that determines how the request will be dispatched.

### Pseudocode

```
function resolve_route(model):
    trimmed = model.trim()

    // Branch 1: Auto mode
    if trimmed in {"", "free", "auto", "free/auto"}:
        return Route::Auto

    // Branch 2: Legacy alias normalization
    normalized = if trimmed starts with "zen/":
        replace "zen/" with "opencode-zen/"
    else:
        trimmed

    // Branch 3: Prefix matching for pinned routes
    for each (idx, entry) in chain:
        prefix = entry.upstream.id + "/"
        if normalized starts with prefix:
            rest = normalized without prefix

            if upstream.id == "openrouter" and rest in {"free", "auto", ""}:
                pinned_model = "openrouter/free"    // Special case
            else:
                pinned_model = rest

            return Route::Pinned { start_idx: idx, pinned_model }

    // Branch 4: No match → Auto fallback
    return Route::Auto
```

### Nuances

| Scenario | Input | Result |
|---|---|---|
| Auto mode | `"free"`, `"auto"`, `"free/auto"`, `""` | `Route::Auto` |
| Pinned upstream | `"cerebras/qwen-3-235b"` | `Route::Pinned { start_idx: 1, pinned_model: "qwen-3-235b" }` |
| OpenRouter pinned | `"openrouter/free"` | `Route::Pinned { ..., pinned_model: "openrouter/free" }` |
| Legacy alias | `"zen/big-pickle"` | Normalized to `"opencode-zen/big-pickle"` then pinned |
| Unprefixed model | `"qwen-3-235b"` | No prefix match → `Route::Auto` |

### OpenRouter Special Case

OpenRouter's model IDs are themselves `vendor/model` strings (e.g., `meta-llama/llama-3-8b:free`). The free pool router model is literally `openrouter/free`. When a user types `openrouter/free`, the algorithm strips the `openrouter/` prefix leaving `rest = "free"`, but it needs to send `"openrouter/free"` as the actual model to OpenRouter's API — not just `"free"`. This special case re-inserts the full ID.

### Complexity

- **Time:** O(P) where P = number of configured upstreams (≤ 11)
- **Space:** O(1)
- **Frequency:** Once per request

---

## Algorithm 3: Attempt Plan Construction (`attempt_plan`)

**Location:** `crates/api/src/providers/free.rs` lines 229–252

**Purpose:** Build the ordered list of `(chain_index, model_string)` attempts that the fallback loop will iterate through.

### Pseudocode

```
function attempt_plan(route):
    match route:
        case Route::Auto:
            return [(0, groq.default_model),
                    (1, cerebras.default_model),
                    (2, google.default_model),
                    ...]

        case Route::Pinned { start_idx, pinned_model }:
            plan = [(start_idx, pinned_model)]       // Pinned attempt first
            for each (idx, entry) in chain:
                if idx != start_idx:
                    plan.push((idx, entry.default_model))  // Then all others
            return plan
```

### Example: Pinned Mode with `cerebras/qwen-3-235b` (index 1)

Chain: `[groq, cerebras, google, mistral, ...]`

```
Plan output:
  [(1, "qwen-3-235b"),           // pinned attempt first
   (0, "llama-3.3-70b-versatile"),  // then groq default
   (2, "gemini-2.5-flash"),         // then google default
   (3, "mistral-large-latest"),     // etc.
   ...]
```

### Key Behaviors

| Behavior | Implication |
|---|---|
| **Pinned fallback includes ALL catalog entries** | Pinning a model doesn't lock you into that provider — it just makes it the first attempt |
| **No deduplication** | If the pinned model happens to match one of the default models, that upstream will be attempted twice (once at the pinned position with the user-specified model, once at its natural catalog position with the default) |
| **`with_capacity` optimization** | Pre-allocates the plan vector to exactly chain length |

### Complexity

- **Time:** O(P)
- **Space:** O(P)
- **Frequency:** Once per request

---

## Algorithm 4: Fallback Decision (`should_fallback`)

**Location:** `crates/api/src/providers/free.rs` lines 254–258

**Purpose:** Determine whether an upstream error should trigger fallback to the next provider or be returned to the user immediately.

### Pseudocode & Decision Table

```
function should_fallback(err):
    return !(err is InvalidRequest or ContentFiltered)
```

| Error Type | Fall Through? | Rationale |
|---|---|---|
| `RateLimited` | Yes | Next upstream might have quota available |
| `QuotaExceeded` | Yes | Next upstream might not be exhausted |
| `AuthFailed` | Yes | Next upstream might have valid keys configured |
| `ServerError` | Yes | Transient — next upstream might be healthy |
| `ConnectionError` | Yes | Transient network issue |
| `InvalidRequest` | **No** | User's request is malformed — same on every upstream |
| `ContentFiltered` | **No** | User's prompt triggered safety filter — same on every upstream |

### Critical Implication

If the request itself is broken (e.g., negative `max_tokens`), `InvalidRequest` will be returned immediately without trying any fallback. This saves unnecessary API calls that would all fail identically.

### Complexity

- **Time:** O(1)
- **Space:** O(1)
- **Frequency:** Once per failed upstream attempt

---

## Algorithm 5: Main Fallback Loop (`create_message` / `create_message_stream`)

**Location:** `crates/api/src/providers/free.rs` lines 282–316 and 318–361

**Purpose:** Iterate through the attempt plan, dispatching to each upstream until one succeeds.

### Pseudocode

```
function create_message(request):
    if chain is empty:
        return AuthFailed("no upstreams configured")

    route = resolve_route(request.model)
    plan = attempt_plan(route)
    last_err = None

    for each (idx, upstream_model) in plan:
        entry = chain[idx]
        modified_request = clone(request)
        modified_request.model = upstream_model

        result = entry.provider.create_message(modified_request).await

        match result:
            case Ok(response):
                return Ok(response)              // SUCCESS — return immediately
            case Err(err) if should_fallback(err):
                log_warning(entry.id, err)
                last_err = err
                continue                          // TRY NEXT UPSTREAM
            case Err(err):
                return Err(err)                   // NON-FALLBACK ERROR — abort

    // All upstreams exhausted
    return Err(ServerError("all free-mode upstreams exhausted"))
```

### Key Behaviors

| Behavior | Detail |
|---|---|
| **`request.model` is rewritten** | The user's abstract model (e.g., `"free/auto"`) is replaced with the concrete upstream model (e.g., `"llama-3.3-70b-versatile"`) |
| **Full request clone per attempt** | `request.clone()` is called for each upstream — messages and tools are copied. Necessary because each upstream needs a different model name |
| **Streams NOT retried mid-stream** | Once a stream connection is established and `Ok(stream)` is returned, mid-stream failures are handled by the caller, NOT by FreeProvider |
| **`last_err` preserves last error** | If all upstreams fail, the last `Err` value is returned — giving the user the actual error from the last attempted provider |
| **`should_fallback` is checked on every error** | Both `create_message` and `create_message_stream` use the same predicate |

### Complexity

- **Time:** O(P × R) where P = upstream count and R = request size (clone cost)
- **Space:** O(R) per attempt (request clone)
- **Frequency:** Once per request (until success or all exhausted)

---

## Algorithm 6: Key Ring Status Aggregation (`key_ring_status`)

**Location:** `crates/api/src/providers/free.rs` lines 410–436

**Purpose:** Aggregate key ring status across all upstream providers into a single summary for the TUI.

### Pseudocode

```
function key_ring_status():
    total_active = 0
    total_keys = 0
    earliest_retry = None
    any_has_ring = false

    for each entry in chain:
        result = entry.provider.key_ring_status()
        if result is Some((active, total, retry)):
            total_active += active
            total_keys += total
            any_has_ring = true
            if retry is Some(secs):
                earliest_retry = min(earliest_retry, secs)  // MIN, not sum

    if any_has_ring:
        return Some((total_active, total_keys, earliest_retry))
    else:
        return None
```

### Aggregation Strategy

| Metric | Strategy | Logic |
|---|---|---|
| `active` | **Sum** | Total usable keys across all upstreams |
| `total` | **Sum** | Total configured keys across all upstreams |
| `retry` | **Min** | Earliest time any key across ANY upstream becomes available |

### Example Scenarios

**Scenario A: Groq has 3 keys (all exhausted, retry in 3600s), Cerebras has 1 key (active)**
```
active = 1 (from Cerebras)
total = 4 (3 + 1)
retry = None (Cerebras has no exhausted keys to report)
```

**Scenario B: All keys across all upstreams exhausted**
```
active = 0
total = 4
retry = min(grok_earliest, cerebras_earliest)
```

**Scenario C: No upstream has a KeyRotatingProvider**
```
any_has_ring = false → return None → TUI skips this provider
```

### The `any_has_ring` Sentinel

If NO upstream implements `key_ring_status()` (e.g., all are single-key providers), the method returns `None`. This means the registry-level aggregation in `key_ring_summaries()` will skip the FreeProvider entirely — it won't appear in the TUI status bar.

### Complexity

- **Time:** O(P)
- **Space:** O(1)
- **Frequency:** Once per render frame (~20–60 times/second)

---

## Algorithm 7: Registry-Level Aggregation (`key_ring_summaries`)

**Location:** `crates/api/src/registry.rs` lines 429–440

**Purpose:** Collect key ring summaries from ALL registered providers (not just FreeProvider) for TUI display.

### Pseudocode

```
function key_ring_summaries():
    summaries = []
    for each (id, provider) in registered providers:
        result = provider.key_ring_status()
        if result is Some((active, total, retry)) and total > 0:
            summaries.push((id.to_string(), active, total, retry))

    summaries.sort_by(canonical provider name)  // alphabetical
    return summaries
```

### Key Behaviors

- **`total > 0` filter** — Providers with zero configured keys are excluded from the UI
- **Alphabetical sort** — Ensures deterministic display order regardless of registration order

### Complexity

- **Time:** O(N log N) where N = number of registered providers (~40 max)
- **Space:** O(N)
- **Frequency:** Once per render frame

---

## Algorithm 8: TUI Consumption of Aggregated Data

**Location:** `crates/tui/src/render.rs` lines 2510–2613

**Purpose:** Render key exhaustion indicators in the status bar.

### Rendering Logic

```
function should_render_status_row(app):
    has_exhausted_keys = app.registry.key_ring_summaries()
        .any(|(_, active, total, _)| active < total)

    return app.voice_recording
        || app.is_streaming
        || app.status_message.is_set()
        || (app.is_streaming and interesting)
        || has_exhausted_keys

function render_status_row():
    for each (provider, active, total, retry_secs) in summaries:
        if active < total:
            color = RED if active == 0 else YELLOW
            label = "provider:active/total"
            if retry_secs is Some(s):
                label += " (retry in Xs)"
            render(label, color)
```

### Visual Hierarchy

| State | Color | Meaning |
|---|---|---|
| `active == total` | (hidden) | All keys healthy — no indicator shown |
| `0 < active < total` | **Yellow** | Some keys exhausted, others still usable |
| `active == 0` | **Red** | All keys exhausted, requests are failing |

### Example Status Bar Output

```
✽ Thinking… │ groq:1/3 (retry in 42s) cerebras:0/1 (retry in 3599s)
```

---

## Complete Algorithmic Complexity Summary

| Algorithm | Time | Space | Frequency |
|---|---|---|---|
| `build_free_provider` | O(K × P) | O(P) | Once at startup |
| `resolve_route` | O(P) | O(1) | Once per request |
| `attempt_plan` | O(P) | O(P) | Once per request |
| `should_fallback` | O(1) | O(1) | Once per failed attempt |
| Main fallback loop | O(P × R) | O(R) | Once per request |
| `key_ring_status` (FreeProvider) | O(P) | O(1) | ~20–60/s (per frame) |
| `key_ring_summaries` (registry) | O(N) | O(N) | ~20–60/s (per frame) |
| TUI rendering | O(N) | O(N) | ~20–60/s (per frame) |

Where:
- **P** = number of configured upstream providers (max 11)
- **K** = number of keys per provider
- **R** = size of the request (messages + tools)
- **N** = number of registered providers (bounded by ~40)

---

## Data Flow Summary

```
User types a message
       │
       ▼
   FreeProvider
   ├── resolve_route()     → Auto or Pinned
   ├── attempt_plan()      → Ordered list of (idx, model)
   │
   ├── For each attempt:   → try_with_rotation (inside KeyRotatingProvider)
   │   ├── next_available()
   │   ├── dispatch provider request
   │   └── mark_exhausted() on failure, retry next key
   │
   └── All exhausted?      → return error to user

   TUI (every frame)
   ├── key_ring_summaries() → Aggregate across all providers
   ├── Filter to active < total
   ├── Color-code by severity (yellow/red)
   └── Render in status bar
```

---

## Error-Triggered Rotation (KeyRotatingProvider)

### Error Classification

| Error | Default Cooldown | Classification |
|---|---|---|
| `QuotaExceeded` | 3600s (1 hour) | Quota |
| `RateLimited` | 60s (1 minute) | RateLimit |
| `AuthFailed` | 300s (5 minutes) | Auth |
| `Other { status: 429 }` | 60s (from body/header) | RateLimit |
| `Other { status: 401\|403 }` | 300s (from body/header) | Auth |

### Cooldown Estimation Priority

1. **`retry_after` from `RateLimited` error** — if the provider adapter parsed it
2. **`Retry-After` HTTP header** — supports both delta-seconds and HTTP-date
3. **Error body text** — patterns like `"retry after N seconds"`, `"reset in N minutes"`, ISO 8601 timestamps
4. **Default** — per error type (60s/300s/3600s)

### Short-Cooldown Recovery

When all keys are exhausted but the shortest cooldown is ≤ 60s, the system sleeps and retries (up to 3 times) instead of immediately failing. This means a 60-second rate-limit on a single key recovers transparently without the user seeing an error.

### Thread Safety

`KeyRing` is behind `Arc<Mutex<KeyRing>>`. The mutex lock scope is carefully bounded:
- Lock → get next available key → unlock (before any `.await`)
- Lock → mark key exhausted → save to file → unlock

This prevents deadlocks when multiple concurrent requests share the same `KeyRotatingProvider`.

### State Persistence

Each `KeyRotatingProvider` saves cooldown state to `~/.clawde/key-ring-state/{provider_id}.json` immediately when a key is exhausted. On next startup, `new_with_persistence()` loads this state so a 12-hour cooldown survives an app restart. Atomic write (temp file + rename) prevents corruption.

---

## Industry Research: Routing Strategies Terminology & Patterns

Research conducted on LiteLLM, OpenRouter, Portkey, One API, and Bifrost — the major open-source AI gateway projects.

### Industry Standard Terminology

| My Original Term | Industry Standard | Used By |
|---|---|---|
| "Rule Sets" / "Modes" | **Routing Strategies** | LiteLLM (`routing_strategy`), Portkey (`strategies`) |
| — | **Channels / Deployments** | One API (`channels`), LiteLLM (`deployments`) |
| "Rules" | **Policies / Provider Preferences** | LiteLLM (`policies`), OpenRouter (`provider` object) |
| "Fallthrough" | **Fallbacks / Failover** | Everyone (universal) |

**The consensus term is "routing strategies"** (LiteLLM uses this as its exact parameter name).

### The Three-Layer Architecture (Industry Consensus)

All major AI gateways organize routing into three distinct layers:

```
Layer 1: LOAD BALANCING STRATEGY
  How to select which deployment in a healthy pool gets the next request.
  e.g., simple-shuffle, least-busy, latency-based, cost-based, usage-based

Layer 2: FALLBACK STRATEGY
  What to do when the selected deployment fails.
  e.g., sequential fallback, parallel hedging, content-policy fallback

Layer 3: GATE POLICY
  Constraints and filters that modify the pool before strategy applies.
  e.g., provider allow/deny lists, capability requirements, circuit breakers
```

### Complete Industry Strategy Catalog

| Industry Strategy | LiteLLM | OpenRouter | Portkey | One API |
|---|---|---|---|---|
| **simple-shuffle** | ✅ (`routing_strategy: "simple-shuffle"`) | ✅ (default) | ✅ | ✅ |
| **weighted** | ✅ (`weight` param) | — | ✅ (weighted load balancing) | ✅ |
| **latency-based** | ✅ (`latency-based-routing`) | automatic | — | — |
| **usage-based** | ✅ (`usage-based-routing-v2`) | — | — | — |
| **cost-based** | ✅ (`cost-based-routing`) | via plugin | — | — |
| **least-busy** | ✅ (`least-busy`) | — | — | — |
| **sequental fallback** | ✅ (`fallbacks`) | ✅ (default) | ✅ | ✅ |
| **hedging** | — | — | — | — |
| **content-policy fallback** | ✅ | automatic | — | — |
| **circuit breaker** | ✅ (cooldown) | automatic | — | — |
| **session affinity** | — | ✅ (`session_id`) | ✅ (sticky) | — |
| **capability filter** | custom class | auto-router plugin | conditional | — |
| **provider preferences** | — | ✅ (`provider` object) | ✅ | — |
| **smart/auto router** | ✅ (`auto_router/*`) | ✅ (`openrouter/auto-beta`) | — | — |

### Key Insights

1. **Hedging (parallel racing) is not implemented by any major project** — this is genuinely novel. Everyone does sequential fallback. The reason is token waste: parallel requests consume quota on multiple providers simultaneously.

2. **Circuit breaker is called "cooldown" in LiteLLM** but works identically: track failures in a window, skip deployments that exceed the threshold, re-try after cooldown expires.

3. **Session affinity is called "sticky" in Portkey** and is done via `session_id` in OpenRouter. Both use request metadata to pin a user to a provider.

4. **Load balancing strategies are independent from fallback strategies** — they compose. You can use latency-based load balancing with sequential fallback.

5. **Gate policies are the least standardized** — each project implements them differently. LiteLLM uses custom classes, OpenRouter uses a `provider` JSON object, Portkey uses nested conditional logic.

### Project Decision: Deprioritize Money-Based Routing

Clawde is focused on free providers and is no longer pursuing paid support. Do not prioritize
routing by dollar cost, billing price, or estimated spend. Cost metadata may remain useful as
passive provider metadata for future integrations, but it should not drive FreeProvider
selection, fallback order, TUI complexity, or user configuration.

Prefer signals that improve the free-tier experience:

- provider capability and request compatibility
- key health, quota availability, and cooldown state
- latency and first-byte responsiveness
- reliability and recent fallback history
- explicit user/provider preferences

### Revised: Proposed Architecture (Informed by Industry Research)

```rust
struct FreeProvider {
    chain: Vec<FreeEntry>,
    routing_config: Option<RoutingConfig>,
}

struct RoutingConfig {
    /// Layer 1: How to order/select from the pool of healthy upstreams.
    load_balancing: LoadBalancingStrategy,

    /// Layer 2: What to do when a selected upstream fails.
    fallback: FallbackStrategy,

    /// Layer 3: Constraints that filter the pool before selection.
    gate_policies: Vec<GatePolicy>,
}

enum LoadBalancingStrategy {
    /// Random shuffle among healthy deployments (current behavior)
    Shuffle,
    /// Weighted random (weights sum to 100)
    Weighted { upstream_weights: Vec<(&'static str, u8)> },
    /// Fastest historical TTFT/latency
    LatencyBased,
    /// Deferred: money-based routing is intentionally not a project priority.
    /// Keep pricing as passive metadata only if an integration needs it.
    // CostBased is deliberately omitted from the active strategy enum.
    /// Highest remaining quota (rate-limit aware)
    UsageBased,
}

enum FallbackStrategy {
    /// Try next upstream in order (current behavior)
    Sequential,
    /// Fire to 2+ upstreams in parallel, first wins
    Hedging { parallel_count: usize },
    /// On content filter, redact and retry
    ContentPolicyFallback,
}

enum GatePolicy {
    /// Skip upstreams that have failed N times in last M seconds
    CircuitBreaker { max_fails: u32, window_secs: u64, cooldown_secs: u64 },
    /// Skip upstreams that don't support required capabilities
    CapabilityFilter,
    /// User-configured upstream allow/deny lists
    ProviderPreferences { allow: Vec<String>, deny: Vec<String> },
    /// Pin to one upstream for conversation consistency
    SessionAffinity,
}
```

### Updated Implementation Priority

| Phase | Strategies | Complexity | Impact |
|---|---|---|---|
| **Phase 1** | `capability-filter`, `session-affinity` | Low | High — prevents silent failures, improves consistency |
| **Phase 2** | `circuit-breaker`, `provider-preferences` | Medium | High — protects against death-spiral retries, gives users control |
| **Phase 3** | `latency-based`, `hedging` | High | Medium — tail latency reduction, token waste tradeoff |
| **Phase 4** | `weighted`, `usage-based`, `content-policy-fallback` | Medium | Low — defer until higher-value free-tier signals are complete |

### Notes from Industry Research

- LiteLLM's `routing_strategy` documentation: https://docs.litellm.ai/docs/routing
- OpenRouter's provider selection: https://openrouter.ai/docs/guides/routing/provider-selection
- OpenRouter's auto router: https://openrouter.ai/docs/guides/routing/routers/auto-router
- Portkey strategies: conditional routing, fallbacks, weighted load balancing
- The term "routing strategy" is standard across the industry — FreeProvider's current approach is closest to "sequential fallback with static priority"
