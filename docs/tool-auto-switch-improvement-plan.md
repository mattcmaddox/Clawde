# Tool-Capable Model Auto-Switch: Improvement Plan

## Audit Summary

Eight issues were identified through automated testing (32 checks, 11 scenarios)
and manual codebase inspection. This document details each issue with codebase
evidence, research from leading LLM routing systems, and a prioritized
improvement plan.

---

## Codebase Architecture (Key Files)

| File | Role |
|---|---|
| `crates/api/src/providers/free/catalog.rs` | `FREE_CATALOG` — 13 upstreams, each with `tool_calling: bool` |
| `crates/api/src/providers/free/impls.rs` | `entry_fits_request()` — capability gate (vision + context only) |
| `crates/api/src/providers/free/task_classifier.rs` | `classify_request()` — 7 task types, keyword-based |
| `crates/api/src/model_registry.rs` | `ModelEntry.tool_calling` from models.dev snapshot |
| `crates/query/src/lib.rs:2212` | Auto-switch logic — `!caps.tool_calling && !tools.is_empty()` |
| `crates/query/src/lib.rs:2340` | System prompt rebuild — `enabled_tools = Some(vec![])` |
| `crates/api/src/provider_types.rs` | `ProviderCapabilities.tool_calling` — per-provider capability |
| `crates/api/src/health_poller.rs` | Startup + periodic health probe (key validity only) |

### Key Finding: All 13 FreeCatalog Upstreams Have `tool_calling: true`

Every entry in `FREE_CATALOG` has `tool_calling: true`. This means:
- The auto-switch for FreeProvider always finds a tool-capable model
- The system prompt rebuild path is unreachable in normal operation
- The capability gate gap (Issue 8) has no practical impact *today* but is a
  correctness issue that would matter if a non-tool upstream is added

---

## Issues and Improvement Plans

### Issue 1: FreeProvider Routing Overrides `--tool-model`

**Severity**: Medium | **Effort**: Low | **Priority**: P1

**Codebase Evidence**:
- `resolve_route()` (impls.rs:216) parses `free/google/gemini-2.5-flash` as
  `Route::Pinned { start_idx: 2, pinned_model: "gemini-2.5-flash" }`
- `attempt_plan()` (impls.rs:302) then reorders based on success rates,
  latency, and task classification — the pin is a *hint*, not a command
- The `attempt_plan_task()` function applies per-task upstream preferences
  that can override the pin order

**Research**:
- **RouteLLM** (arXiv:2406.18665): Uses trained routers (matrix factorization,
  pairwise comparison) to select between strong/weak models. Key insight:
  routers achieve same quality as commercial offerings at >40% lower cost.
  Their `cost_threshold` parameter controls the quality-cost tradeoff.
- **OpenRouter Auto Router**: Uses session stickiness — once a model is chosen,
  subsequent turns prefer it. The router re-ranks candidates each turn but
  reuses the remembered model while it's still a top candidate.
- **LiteLLM**: Uses explicit fallback mappings (`fallbacks=[{"gpt-3.5-turbo":
  ["gpt-4"]}]`) that are always respected. No reordering by routing layer.

**Improvement Plan**:

1. **Add `Route::Strict` variant** that bypasses `attempt_plan_task()` reordering.
   When `--tool-model` is set with a provider prefix, use `Route::Strict` so the
   exact model is used. This is the LiteLLM approach.

2. **Add session-level model cache.** After the auto-switch picks a model,
   cache `(provider_id, model_id)` in `QueryConfig`. Subsequent turns reuse
   the cached model unless it fails. This matches OpenRouter's session
   stickiness.

3. **Emit routing decision telemetry.** When the routed model differs from
   `--tool-model`, log: "Requested X, routed to Y (reason: task preferences)".

**Files**: `lib.rs`, `main.rs`, `free/impls.rs`

---

### Issue 2: Auto-Switch Fires Every Turn

**Severity**: Low | **Effort**: Low | **Priority**: P3

**Codebase Evidence**:
- The auto-switch block at `lib.rs:2223` runs inside the per-turn dispatch
- `build_system_prompt()` (runner/prompt.rs:15) loads memory from disk via
  `build_memory_prompt_content_with_budget()` on every call
- When `!caps.tool_calling && !tools.is_empty()`, the rebuild fires every turn

**Research**:
- **LiteLLM**: Caches provider health state in Redis. Only re-evaluates
  routing when cooldowns expire or health checks change.
- **OpenRouter**: Session stickiness avoids re-routing per turn.
- **Portkey**: Uses config-driven routing rules evaluated once per request.

**Improvement Plan**:

1. **Cache switch result in session state.** Add a `tool_switch_cache:
   Option<(String, String)>` to the per-session state. On first turn, evaluate
   and cache. On subsequent turns, reuse if `(provider_id, model_id)` unchanged.

2. **Cache rebuilt system prompt.** Key by `(provider_id, model_id,
   enabled_tools_hash)`. Reuse on subsequent turns.

**Files**: `lib.rs`

---

### Issue 3: `tool_calling` Flag Accuracy

**Severity**: High | **Effort**: Medium | **Priority**: P0

**Codebase Evidence**:
- `ModelEntry.tool_calling` comes from models.dev `tool_call` field
  (model_registry.rs:335)
- `FreeUpstream.tool_calling` is hardcoded in `FREE_CATALOG` (catalog.rs:60)
- No runtime verification exists — the flag is trusted blindly
- `ProviderCapabilities.tool_calling` is the runtime capability
  (provider_types.rs:440)
- The health poller (`health_poller.rs`) only validates key authenticity,
  not capability — it probes `/v1/models` or a 1-token chat, neither of
  which verifies tool calling

**Research**:
- **RouteLLM**: Trained routers learn which models handle which tasks from
  preference data. No static capability flags — the router *discovers*
  capabilities through usage.
- **OpenRouter**: Uses aggregate spend data as a proxy — if developers spend
  on a model for code tasks, it supports tools.
- **LiteLLM**: Per-provider health checks probe actual capabilities at startup.
  Their health check endpoint (`GET /health`) verifies that models can serve
  requests, though not specifically tool calling.

**Improvement Plan**:

1. **Add startup capability probe.** When the session starts, send a minimal
   tool-use request (`{"tools": [{"type": "function", "function": {"name":
   "test", "parameters": {}}}], "messages": [{"role": "user", "content": "hi"}]}`)
   to the configured model. Check if the response contains `tool_use` blocks.
   Cache the result.

2. **Add runtime tool-use failure detection.** After each turn, check if
   tools were available but the response has no `tool_use` blocks. If so,
   mark the model as "tool_use_unreliable" and increase its auto-switch
   priority.

3. **Add `tool_calling_verified` field to `ModelEntry`.** Set to `true`
   after the startup probe succeeds. Use this instead of the raw
   `tool_calling` flag in the auto-switch decision.

**Files**: `model_registry.rs`, `lib.rs`, `query/runner/prompt.rs`

---

### Issue 4: Hardcoded Known-Provider List

**Severity**: Medium | **Effort**: Low | **Priority**: P2

**Codebase Evidence**:
- `lib.rs:1998-2048` contains a hardcoded array of ~40 provider strings
- Used to parse `--tool-model openai/gpt-4` into provider + model
- New providers require a code change to be recognized

**Research**:
- **LiteLLM**: Uses a config-driven provider list (`model_list` in YAML).
  Providers are added via config, not code.
- **Portkey**: Supports 1600+ models across 40+ providers via a registry.

**Improvement Plan**:

1. **Replace with model registry lookup.** Use
   `model_registry.find_provider_for_model()` to detect provider prefix.
   Fall back to current behavior for unknown providers.

2. **Add `--tool-model-provider` flag** for explicit provider specification
   when auto-detection is ambiguous.

**Files**: `lib.rs`

---

### Issue 5: System Prompt Rebuild Untested

**Severity**: Medium | **Effort**: Low | **Priority**: P2

**Codebase Evidence**:
- Rebuild at `lib.rs:2347`: `patched_sys.enabled_tools = Some(vec![])`
- Only fires when `!caps.tool_calling && !tools.is_empty() && !degradation_turn`
- All 13 FreeCatalog upstreams have `tool_calling: true` → rebuild never fires
- The rebuild calls `build_system_prompt()` which loads memory from disk

**Research**:
- **LiteLLM**: Uses `mock_testing_fallbacks=True` to force fallback scenarios
  in tests. Provides `mock_testing_context_fallbacks` and
  `mock_testing_content_policy_fallbacks` for specific error types.

**Improvement Plan**:

1. **Add `--force-no-tools` dev flag** that bypasses auto-switch and always
   fires the rebuild. Useful for testing without needing a specific provider.

2. **Add integration test** with a mock FreeProvider containing only
   non-tool-capable upstreams.

**Files**: `main.rs`, `tool-switch-audit.py`

---

### Issue 6: Model Doesn't Use Tools After Switch

**Severity**: High | **Effort**: Medium | **Priority**: P1

**Codebase Evidence**:
- Auto-switch sets `caps.tool_calling = true` and sends tools
- But the model's behavior is non-deterministic — it may respond with text
  instead of tool calls
- The TUI test showed mistral responding "I don't have direct access" despite
  tools being available

**Research**:
- **RouteLLM**: Trained routers learn from preference data which models
  handle which tasks. Models that don't perform well get lower routing
  scores.
- **OpenRouter**: Market-based routing means models that don't handle tools
  get fewer spend signals and are deprioritized over time.
- **LiteLLM**: Uses success rate tracking per deployment to influence routing.

**Improvement Plan**:

1. **Add tool-use success rate tracker.** After each turn, record whether
   tools were available and whether `tool_use` blocks appeared in the
   response. Track per-model. Use in auto-switch ranking.

2. **Add tool-use nudge to system prompt.** When tools are available and
   the model was auto-switched, inject: "You have access to tools. Use them
   for file operations, code execution, and system commands."

3. **Add retry heuristic.** If the model responds without tools when tools
   were available, retry with a different model from the same provider.

**Files**: `lib.rs`, `system_prompt.rs`

---

### Issue 7: TUI Attribution Is Only Indicator

**Severity**: Low | **Effort**: Low | **Priority**: P3

**Codebase Evidence**:
- Attribution badge `⤷ model · $cost` is the only persistent evidence
- Status bar shows *selected* model, not *served* model
- Status message is transient (redraws on next frame)

**Research**:
- **OpenRouter**: Returns `model` field in every response showing actual model
- **LiteLLM**: Logs resolved model in proxy access logs
- **Portkey**: Full request/response logging with model attribution

**Improvement Plan**:

1. **Add `ModelInfo` event to query stream.** After auto-switch, emit event
   with: original_model, switched_model, reason, provider.

2. **Enhance `/model` command** to show switch history and current state.

3. **Log switch to session JSONL** for post-hoc analysis.

**Files**: `lib.rs`, `commands/session_tools.rs`

---

### Issue 8: Capability Gate Missing `tool_calling`

**Severity**: High | **Effort**: Low | **Priority**: P0 (COMPLETED)

**Codebase Evidence**:
- `entry_fits_request()` (impls.rs:371) only checked `vision` and `context_window`
- `FreeUpstream.tool_calling` exists in the catalog but was unused by the gate
- `capability_block_reason()` (impls.rs:404) only reported vision/context blocks
- All current upstreams have `tool_calling: true`, so this is a correctness fix

**Research**:
- **LiteLLM**: Capability-based routing filters by model features before
  dispatch. Their `model_list` entries can specify supported capabilities.
- **OpenRouter**: Auto Router filters by task type and model capabilities
  before selection. Models that don't support a capability are excluded.
- **Portkey**: Routing rules can filter by model metadata including
  capability flags.

**Implementation (DONE)**:

1. ✅ Added `has_tools` parameter to `entry_fits_request()`. Check
   `entry.upstream.tool_calling` when the request has tools. Skip upstreams
   that don't support tool calling.

2. ✅ Updated `capability_block_reason()` to report when no tool-capable
   upstream is available.

**Files**: `free/impls.rs`

---

## Priority Matrix

| Issue | Severity | Effort | Research Basis | Priority | Status |
|---|---|---|---|---|---|
| 8: Capability gate | High | Low | LiteLLM, OpenRouter, Portkey all filter by capabilities | **P0** | ✅ DONE |
| 3: tool_calling accuracy | High | Medium | RouteLLM discovers capabilities from usage data | **P0** | Pending |
| 6: Model doesn't use tools | High | Medium | RouteLLM/OpenRouter use success rates for routing | **P1** | Pending |
| 1: --tool-model override | Medium | Low | LiteLLM uses explicit fallback mappings | **P1** | Pending |
| 4: Hardcoded provider list | Medium | Low | LiteLLM uses config-driven lists | **P2** | Pending |
| 5: Rebuild path untested | Medium | Low | LiteLLM uses mock flags for testing | **P2** | Pending |
| 2: Per-turn performance | Low | Low | LiteLLM/OpenRouter use session caching | **P3** | Pending |
| 7: Attribution visibility | Low | Low | OpenRouter returns model field; Portkey logs | **P3** | Pending |

## Research Sources

| Source | Type | Key Insight |
|---|---|---|
| RouteLLM (arXiv:2406.18665) | Whitepaper | Trained routers reduce costs 85% at 95% quality via preference data |
| OpenRouter Auto Router | Production | Market-based routing + session stickiness + ~30 task types |
| LiteLLM Router | OSS (9K+ stars) | Config-driven fallbacks, capability filtering, mock testing flags |
| Portkey Gateway | OSS (10K+ stars) | 1600+ models, guardrails, retry/fallback, <1ms latency |

## Implementation Order

1. **P0**: Add `tool_calling` to FreeProvider capability gate (Issue 8) — ✅ DONE
2. **P0**: Add startup capability probe (Issue 3) — Next
3. **P1**: Add tool-use success rate tracking (Issue 6)
4. **P1**: Add `Route::Strict` for `--tool-model` (Issue 1)
5. **P2**: Replace hardcoded provider list (Issue 4)
6. **P2**: Add `--force-no-tools` test flag (Issue 5)
7. **P3**: Add session-level switch cache (Issue 2)
8. **P3**: Add `ModelInfo` event to query stream (Issue 7)
