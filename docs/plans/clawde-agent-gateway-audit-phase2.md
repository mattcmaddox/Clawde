# Clawde Agent Gateway — Post-Phase 2 Verification Audit

Status: complete (2026-08-26). Verifies the committed Phases 0–2
(agent loop, chat-completions agent mode, `/v1/responses`) against the locked
decisions in `clawde-agent-gateway-plan.md` §9.

## Method

Each locked decision D1–D16 was checked against the committed code path that
implements it, plus its test. Decisions implemented by absence (D4) were
confirmed by grep for the forbidden pattern. The two plan-listed Phase 2
deliverables that were missing tests (golden Open Responses transcript,
`tool_choice` passthrough) were added during this audit.

## Decision verification

| # | Decision | Verdict | Evidence |
|---|---|---|---|
| D1 | Silent intermediate turns (chat) | **Pass** | `agent_stream_chunks` renders only the segment after the last `TurnStart` (`rposition`, translate.rs:421); external calls stream as relay (`finish_reason: tool_calls`). Responses streams items natively per D1 note. |
| D2 | Curated 10-tool surface, no wildcard | **Pass** | `DEFAULT_BUILTIN_TOOLS` = exactly 10 (Read, Glob, Grep, WebFetch, WebSearch, Write, Edit, ApplyPatch, Bash, RunTests); `builtinTools` is a replacement list; case-insensitive binding (tool_exec.rs:70-82, 122). |
| D3 | Reasoning: raw `content`, truncated to budget | **Fixed (was gap)** | Raw `content` was correct but **no budget existed** anywhere. Added `THINKING_TEXT_BUDGET` (32 KiB) cap on the accumulated reasoning item text in `ResponsesItemBuilder` (char-boundary truncation + `…[truncated]` marker on close; streamed deltas stay raw). Test: `thinking_text_is_capped_at_budget`. |
| D4 | No text-prompt tool fallback in v1 | **Pass** | No schema/regex tool-mixin code in gateway. |
| D5 | Ephemeral in-memory LRU (256 / 1 h) | **Pass** | `session_capacity` 256 / `session_ttl_secs` 3600 defaults in core; router builds `SessionStore` from config (router.rs:1135). No disk writes. |
| D6 | `allowed_tools` → `tool_error` observation | **Pass** | `partition_by_allowed` + error observation loop (agent.rs); integration test `responses_allowed_tools_denied_becomes_error_observation`. |
| D7 | `n > 1` → 400 | **Pass** | Rejected in parser; integration test. |
| D8 | `instructions` verbatim; preamble only when none | **Pass** | Parser: client instructions verbatim, `GATEWAY_PREAMBLE` only when neither instructions nor system input; unit tests. |
| D9 | Cap force-stop; never yield pending internals | **Pass** | Cap check precedes execution/yield (agent.rs:381); chat → `finish_reason: stop` (translate.rs:375); Responses → `incomplete`/`max_tool_calls`; integration tests. |
| D10 | Fail with partial transcript; retry once in-turn | **Pass** | Transient classification + one in-turn retry (agent.rs:276); `AgentFailure.partial` surfaced as items (router.rs:611) and 400/429/503 mapping (error.rs). |
| D11 | Serialize continuations per id | **Pass** | `continuation_lock` (owned-mutex per id) held across the request; deterministic concurrency test. |
| D12 | Cascade-drift thinking strip + accepted one-turn lag | **Pass** | `strip_thinking_from_trajectory` on upstream change; lag documented in code + plan (revisit triggers recorded). |
| D13 | Reactive overflow compaction only (≤2 stages) | **Pass** | `OverflowCompactor`: truncate → summarise, `ContextOverflow` retry once (agent.rs:283-320); stage-ladder unit tests. |
| D14 | Sanitize tool results at executor boundary | **Pass** | `sanitize_result`: strips C0 + ANSI CSI, truncates to budget on UTF-8 boundary (tool_exec.rs:262-299). |
| D15 | Structural injection defense in v1 | **Pass** | Untrusted framing: `tool_error:` prefix + `is_error` flags + D14 sanitization. Semantic guard correctly deferred (no such code). |
| D16 | Cancellation reaches tools + compaction | **Pass** | Token threaded into `ToolContext`/`execute_all`/`compact`; checked between turns; `CancelOnDrop` cancels on disconnect. |

## Gaps closed during this audit

1. **D3 reasoning budget (code fix)** — see table. Only decision not faithfully
   implemented.
2. **Golden Open Responses transcript** (plan Phase 2 step 10) — added
   `tests/fixtures/responses_golden_stream.json` (18 events, full deterministic
   stream for a canonical internal-tool + final-text run) and
   `responses_golden_stream_transcript` which pins every event, in order,
   including `sequence_number`, item ids, and payloads.
3. **`tool_choice` passthrough test** — responses parser was wired but untested;
   added `parses_tool_choice_passthrough` (forced-function) and
   `parses_string_tool_choice` (`"none"`).
4. **Eviction → `previous_response_not_found`** (plan Phase 2 step 10) —
   added `responses_evicted_session_returns_not_found` (capacity-1 store,
   LRU eviction, 400 with `previous_response_not_found`).

## Deferred (not defects)

- **SDK smoke tests** (plan Phase 2 step 11: openai-python, Agents SDK, curl
  SSE transcript) — manual, require a live gateway; defer to Phase 3 hardening.
- **D15 opt-in semantic prompt-injection guard** — plan Phase 3 candidate by
  design; do not add before the docs land.

## Verdict

16/16 decisions verified; the one gap (D3 budget) was a small bounded fix.
No architecture drift. The phase boundary is clean for Phase 3 (docs + hygiene).
