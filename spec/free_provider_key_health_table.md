# FreeProvider Key Health Table — Specification

## Summary

Replace the current Anthropic-specific `rate_limit_5h_pct` / `rate_limit_7day_pct` bars in the
`context_viz` overlay with a per-upstream key health table that works for ALL providers
in the FreeProvider chain — not just Anthropic.

---

## Motivation

The current rate limit system (implemented in the `RateLimitUpdate` wiring) only works for
the direct `AnthropicClient` API path. It parses `anthropic-ratelimit-*` headers inside
`process_sse_stream()` and pipes them through `AnthropicStreamEvent::RateLimitHeaders` →
`QueryEvent::RateLimitUpdate` → `app.rate_limit_5h_pct` / `app.rate_limit_7day_pct`.

For free-tier users who rely on the FreeProvider chain (Groq, Cerebras, Gemini, etc.),
these fields always show "no data." The FreeProvider already has a richer data source
available — the `KeyRing` per-upstream key state machine — but that data isn't surfaced
in the overlay.

The user wants a generalized key health table that shows every configured upstream
with live key counts, cooldown timers, and (when available) HTTP rate limit percentages.

---

## Current State (Baseline)

### What exists today

| Component | File | What it does |
|-----------|------|--------------|
| `ContextVizState` | `tui/src/context_viz.rs:20` | `pub visible: bool` — open/close/toggle |
| `render_context_viz()` | `tui/src/context_viz.rs:46` | Renders context window bar, 2 rate limit bars (5h/7d), message stats, cost |
| `app.rate_limit_5h_pct` | `tui/src/app.rs:1514` | `Option<f32>` — set by `QueryEvent::RateLimitUpdate` (Anthropic only) |
| `app.rate_limit_7day_pct` | `tui/src/app.rs:1516` | `Option<f32>` — same |
| `extract_rate_limit_pct()` | `api/src/lib.rs:1180` | Parses `anthropic-ratelimit-*-remaining` / `*-limit` into 0.0–1.0 |
| `KeyRing` | `core/src/key_ring.rs` | In-memory key state: per-key cooldown tracking, round-robin, disk persistence |
| `key_ring_summaries()` | `api/src/registry.rs:429` | Collects all providers' status, filters `total > 0`, sorts alphabetically |
| `key_ring_status()` | `api/src/providers/free.rs:410` | Aggregates across upstreams: active=sum, total=sum, retry=min |
| `FREE_CATALOG` | `api/src/providers/free.rs:54` | 11 upstreams: Groq, Cerebras, Google, Mistral, SambaNova, NVIDIA, Cohere, OpenRouter, OpenCode Zen, Z.AI, Zhipu |
| TUI status bar | `tui/src/render.rs:2510` | Shows `provider:active/total (retry in Xs)` when keys are exhausted |

### What providers return for HTTP rate limit headers

| Provider | Header prefix | Tokens remaining/limit | Requests remaining/limit |
|----------|--------------|----------------------|------------------------|
| Anthropic | `anthropic-ratelimit-` | `*-tokens-remaining` / `*-tokens-limit` | `*-requests-remaining` / `*-requests-limit` |
| OpenAI | `x-ratelimit-` | `*-remaining-tokens` / `*-limit-tokens` | `*-remaining-requests` / `*-limit-requests` |
| Groq | `x-ratelimit-` | `*-remaining-tokens` / `*-limit-tokens` | `*-remaining-requests` / `*-limit-requests` |
| Google Gemini | N/A | No standard headers | No standard headers |
| Others | Varies | Unknown | Unknown |

### Data already available without HTTP headers

The `KeyRing` in `core/src/key_ring.rs` tracks per-key:
- **Active**: key is available for dispatch
- **Exhausted**: key is in cooldown (rate-limited, quota exceeded, or auth failure)
- **Cooldown end time**: when the key becomes available again
- **Round-robin index**: which key is next in the rotation

`key_ring_summaries()` in `api/src/registry.rs` returns `Vec<(String, KeyRingSummary)>` where
each `KeyRingSummary` has `active`, `total`, and `retry_secs`. Only providers with `total > 0`
are included.

This data is already live and doesn't require any HTTP header parsing.

---

## Design

### Rendering: Table replacing the two rate limit bars

**Location:** `render_context_viz()` in `tui/src/context_viz.rs`
**Change:** Remove the current "Rate limits" section (two horizontal bars for 5h/7d) and
replace it with a "Key health" table.

**Table columns:**
| Column | Source | Example | Notes |
|--------|--------|---------|-------|
| Provider | `FREE_CATALOG[].title` | "Groq" | Always shown, even if 0 keys |
| Keys | `KeyRing.active` / `KeyRing.total` | "2/3" | Colored: green=all active, yellow=partial, red=0 active |
| Tokens % | HTTP header (when available) | "23%" | `x-ratelimit-remaining-tokens` / `x-ratelimit-limit-tokens` or `anthropic-ratelimit-tokens-*` |
| Requests % | HTTP header (when available) | "12%" | `x-ratelimit-remaining-requests` / `x-ratelimit-limit-requests` or `anthropic-ratelimit-requests-*` |
| Retry | `KeyRing` cooldown | "—" or "47s" | When all keys are exhausted, shows time until first key recovers |

**Row coloring:**
- **Not configured** (0 keys): Dim text, shows "—" for all data columns
- **All active** (active == total): Normal text, keys column white
- **Partial** (0 < active < total): Keys column yellow
- **All exhausted** (active == 0): Keys column red, retry column shows countdown

### Data flow

```
KeyRing (core/src/key_ring.rs)
  │  key_ring_summaries() — returns Vec<(String, KeyRingSummary)>
  │  already available, already live, no new code needed
  ▼
App.render_context_viz() call in render.rs
  │  reads key_ring_summaries() via ProviderRegistry or FreeProvider
  │  polled every render frame (Mutex read, no new event type needed)
  ▼
render_context_viz() in context_viz.rs
  │  renders the table with the data
  ▼
User sees: per-upstream key health in the /ctx-viz overlay
```

### HTTP rate limit headers (secondary data source)

For upstreams that return standard rate limit headers (Anthropic, OpenAI, Groq), we can
also extract per-request usage. This is a **nice-to-have** and can be implemented as a
follow-up. The primary data source (KeyRing key counts) works for every provider
immediately.

The existing `extract_rate_limit_pct()` helper in `api/src/lib.rs` can be generalized to
accept a header prefix parameter:

```rust
fn extract_rate_limit_pct(
    headers: &HeaderMap,
    prefix: &str,         // "anthropic-ratelimit-" or "x-ratelimit-"
    metric: &str,         // "tokens" or "requests"
    suffix_order: &str,   // "remaining-limit" (anthropic) or "limit-remaining" (openai)
) -> Option<f32>
```

But this is secondary — the table works without it.

### What gets removed

1. `app.rate_limit_5h_pct: Option<f32>` — no longer needed
2. `app.rate_limit_7day_pct: Option<f32>` — no longer needed
3. `QueryEvent::RateLimitUpdate` — no longer needed
4. `AnthropicStreamEvent::RateLimitHeaders` — no longer needed
5. `extract_rate_limit_pct()` — may be generalized or removed
6. The two rate limit bars in the overlay — replaced by the table
7. The rate limit section in the footer (5h/7d) — if present

### What gets added

1. **`render_context_viz()` signature change**: Accept `key_ring_data: Vec<(String, KeyRingSummary)>` instead of `rate_5h` / `rate_7d`
2. **Table rendering** in the overlay: Rows for each upstream, columns as specified above
3. **Render loop change**: In `render.rs`, read `key_ring_summaries()` from the provider registry and pass it to `render_context_viz()`
4. **Modal height increase**: Table of 11 upstreams needs more vertical space than 2 bars. Increase from 24 to ~30 rows.

### KeyRing data access from the TUI

The TUI `App` struct needs access to the `KeyRing` or `ProviderRegistry` to call
`key_ring_summaries()`. Options:

**Option A: Pass ProviderRegistry to App**
- `App` already has `model_registry: Option<Arc<ModelRegistry>>`
- Add `provider_registry: Option<Arc<ProviderRegistry>>` (or a callback)
- Call `reg.key_ring_summaries()` in the render loop

**Option B: Add a callback/fn pointer**
- Store `key_ring_fn: Option<Arc<dyn Fn() -> Vec<...> + Send + Sync>>` on App
- Set from CLI main.rs like the `arg_completions` callback
- Clean separation of concerns

**Option C: Store the data on App, refresh via event**
- Add `key_ring_data: Vec<...>` field on App
- Emit a new event (or poll) to update it
- More complex, unnecessary for read-only display

**Decision: Option B** — callback pattern. Consistent with `arg_completions`.
The CLI layer already has access to the provider registry and FreeProvider, so it
can set the callback at startup.

### Modal height

Current: 24 rows (increased from 20 in the previous change)
New: ~30 rows needed for 11 upstreams + headers + context window section

Alternatively: decrease row height (compact single line per upstream) to fit in 24 rows.

**Compact row format:**
```
Groq       2/3  12%  20%  —
Cerebras   —    —    —   not configured
```
6 chars for provider + spaces, 6 chars for keys, 5 chars for each %, 10 chars for retry.
Each row: ~36 chars wide. 11 rows + header = ~14 rows of table.

If we show unconfigured upstreams compactly (just name + dash / "not configured"),
the table might fit in the existing 24-row modal without expansion.

---

## Implementation Plan (rough)

### Phase 1: Core table data (no HTTP headers — KeyRing only)

1. **Add callback to App** (`tui/src/app.rs`)
   - `key_ring_data_fn: Option<Arc<dyn Fn() -> Vec<KeyRingRow> + Send + Sync>>`
   - `KeyRingRow { provider_name: String, active: u32, total: u32, retry_secs: Option<u64> }`

2. **Set callback in CLI** (`cli/src/main.rs`)
   - Wire `app.key_ring_data_fn` from the registry's `key_ring_summaries()`

3. **Update `render_context_viz()`** (`tui/src/context_viz.rs`)
   - Replace `rate_5h: Option<f32>, rate_7d: Option<f32>` params with `key_ring_rows: Vec<KeyRingRow>`
   - Remove the two rate limit bar sections
   - Add table rendering with the specified columns
   - Adjust modal height if needed

4. **Update call site** (`tui/src/render.rs`)
   - Call `app.key_ring_data_fn()` to get rows
   - Pass rows to `render_context_viz()`
   - Remove references to `app.rate_limit_5h_pct` / `app.rate_limit_7day_pct`

5. **Cleanup**:
   - Remove `app.rate_limit_5h_pct` and `app.rate_limit_7day_pct` fields
   - Remove `QueryEvent::RateLimitUpdate` and its handler
   - Remove `AnthropicStreamEvent::RateLimitHeaders` variant
   - Remove `extract_rate_limit_pct()` or keep/generalize it
   - Remove the `RateLimitHeaders` arm from `map_stream_event` in `providers/anthropic.rs`
   - Remove the rate limit extraction block in `process_sse_stream()`
   - Remove the `StreamAccumulator` no-op arm

6. **Update tests**:
   - Update `context_viz_renders_without_panic` to pass key row data instead of rate limit pcts
   - Update `context_viz_hidden_renders_nothing`
   - Add test for the table rendering with mock key data

### Phase 2 (follow-up): HTTP rate limit headers for non-Anthropic providers

1. Generalize `extract_rate_limit_pct()` to accept header prefix + field ordering
2. Add extraction to the provider dispatch path (for providers that support it)
3. Merge HTTP rate limit data with KeyRing data in the table rows
4. Show "—" when HTTP headers aren't available for a given provider

---

## Edge Cases

1. **No keys configured**: Show all 11 upstreams with "0" keys and "—" for all data. Dim styling.
2. **All keys exhausted**: Red keys column, show retry countdown.
3. **Some providers don't return rate limit headers**: "—" in the HTTP rate limit columns. KeyRing data still works.
4. **Provider registry not available** (headless mode, tests): Pass empty vec, table shows nothing or is hidden.
5. **Overlay not visible**: No key ring queries made (guard behind `context_viz.visible`).
6. **Rapid key state changes**: KeyRing behind `Arc<Mutex<>>` — poll on render is safe since lock scope is short.
7. **Provider that IS configured but ISN'T in FREE_CATALOG**: Only FreeProvider upstreams are in the catalog. Other providers (direct Anthropic, OpenAI via API key, etc.) that have KeyRing data can be added by iterating `key_ring_summaries()` from all providers, not just FreeProvider.

---

## Resolved Design Decisions

1. **Scope — FreeProvider only.** The table shows ONLY the 11 FREE_CATALOG upstreams
   (Groq, Cerebras, Google Gemini, Mistral, SambaNova, NVIDIA, Cohere, OpenRouter,
   OpenCode Zen, Z.AI, Zhipu). Standalone providers with direct API keys (Anthropic,
   OpenAI, etc.) do NOT appear in this table — they aren't part of the free-tier
   key rotation concern this feature targets.

2. **Footer unchanged.** The status bar key rotation indicator stays as-is. It's
   useful when the overlay is closed, and there's no duplication issue since the
   overlay and status bar are never visible simultaneously (the overlay is a modal).

3. **Color convention — same as status bar (green/yellow/red).**
   - Green: all keys active (active == total)
   - Yellow: partial exhaustion (0 < active < total)
   - Red: all exhausted (active == 0)
   - Dim/gray: not configured (0 keys)
   Matches the existing convention at `tui/src/render.rs:2510`.
