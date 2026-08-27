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

## Live SDK smoke tests (plan Phase 2 step 11) — done 2026-08-26

Ran against a debug gateway on `free/groq` (free cascade; Groq + Gemini
fallback) with openai-python 3.3.1 and openai-agents 0.22.0:

- `client.chat.completions.create` — relay round-trip.
- Chat completions agent mode — server-side `Read` executed, final answer
  returned, no yielded `tool_calls` (activation via `extra_body={
  "max_tool_calls": N}` since the knob is not in the SDK schema).
- `client.responses.create` — items + `completed`, `previous_response_id`
  continuation hydrated (`prev.input + prev.output + new input`).
- openai-agents `Runner.run_sync` — client-side `function_tool` over the
  gateway's Responses API (model passed as `OpenAIResponsesModel` instance;
  the SDK's provider-prefix resolver rejects the gateway's `free/` routes).
- curl SSE transcript — full semantic event sequence incl. `response.done`
  and `[DONE]`.

Two real edge cases surfaced only under live traffic, both fixed:

1. **`MALFORMED_FUNCTION_CALL` stop reason** (Gemini fallback): the model
   emitted a ToolUse block whose arguments never streamed, with
   `Other("MALFORMED_FUNCTION_CALL")` instead of `ToolUse`. The loop treated
   it as a completed text turn and returned a `completed` response with
   **empty output**. Fix: any turn containing ToolUse blocks routes through
   the tool path regardless of stop reason; the null-input call becomes a
   `malformed_arguments` error observation (E6) and the model self-corrects.
   Regression test: `malformed_stop_reason_routes_tool_blocks_through_execution`.
2. **Empty terminal turns** (free-cascade upstreams returning 0-token
   completions or thinking-only turns): the loop completed "successfully"
   with no content. Fix: a terminal turn with no text and no tool calls
   (thinking is not an answer) retries once, bounded like D10. Tests:
   `empty_terminal_turn_retries_once`, `thinking_only_terminal_turn_retries`,
   `empty_terminal_turn_does_not_retry_twice`.

## Deferred (not defects)

- **D15 opt-in semantic prompt-injection guard** — plan Phase 3 candidate by
  design; do not add before the docs land.

## Post-SDK full-feature audit

Edge-case sweep across the committed feature (E1-E12, config knobs, error
wire fidelity):

- E1 cap notice, E4 tool_choice passthrough, E10 retry placement (D10 retry
  lives in `dispatch_turn`; FreeProvider handles upstream cooldowns) — all
  verified against code and tests.
- `parallel_tool_calls: false` → serial execution (`parallel_concurrency`
  returns 1); `tool_result_budget` (50K) truncation tested.
- Responses error path wire fidelity: `response.failed` + `response.done` +
  `[DONE]` on loop failure; 400/401/429/404/503/504 mappings carry
  `Retry-After` from provider errors and agent failures.
- New-fix interaction check: malformed-stop (routes ToolUse turns to the tool
  path) and empty-turn retry (turns with neither text nor ToolUse) are
  disjoint — no interaction bug.

One fidelity gap fixed: **`n: 0` was accepted** by both parsers (OpenAI
spec requires `n >= 1`; their API returns 400). Both chat and Responses
parsers now reject `n == 0` with `400 n must be at least 1`; tests
`rejects_n_zero` added on each surface.

## Second sweep: interaction + fidelity audit

Re-audited the committed feature for interactions between the fixes and
remaining fidelity gaps:

- **Fixed — degenerate ToolUse stop, no blocks.** The empty-terminal retry
  excluded `stop_reason == ToolUse`, and the degenerate branch (`ToolUse`
  stop with zero emitted blocks) completed the turn regardless of content —
  so a thinking-only or empty turn with a ToolUse stop reason still
  completed silently empty, the same failure class as the live-found fixes.
  The retry now covers any no-answer, no-calls turn regardless of stop
  reason, before the empty message enters the trajectory. Regression test:
  `degenerate_tool_use_stop_without_blocks_retries`.
- **Fixed — misleading tool-type error.** `parse_responses_tools` reported
  "tool missing 'name'" for spec-defined non-function built-ins
  (`web_search_preview`, `file_search`), which the gateway does not
  implement. Now rejected explicitly with the type named. Test:
  `rejects_unsupported_builtin_tool_type`.
- **Verified clean** — rate limiter (estimate charged up-front, actual
  reconciled on completion, clamped at 0: overshoot self-starvation is by
  design), E2 mixed internal+external ordering (chat yields all; Responses
  executes internals then yields externals), relay stream error mid-stream
  (`[DONE]` + usage reconciliation on every exit path), per-dispatch timeout
  wired into every turn (`dispatch_turn` → `provider_call`).
- **Verified accurate** — `allow-readonly` allows read-only tools anywhere
  (Read's own contract: "You can access any file directly"); glob/grep are
  the only workspace-filtered tools. The docs' "workspace scoping" claim
  (working directory, not a read boundary) matches the code.
- **Known limitation (pre-existing, matches relay)** — the per-dispatch
  timeout wraps stream *setup* only, not consumption; a stalled upstream
  stream hangs until client disconnect / force-cancel. Same semantics as
  the relay path; noted, not changed.

## Verdict

16/16 decisions verified; the one gap (D3 budget) was a small bounded fix.
No architecture drift. Live SDK smoke tests passed and surfaced two
additional loop hardening fixes (malformed stop reasons, empty terminal
retry), now regression-tested. Post-SDK edge sweep found one wire-fidelity
nit (`n: 0`), fixed with tests on both surfaces. Second sweep closed the
last empty-completion hole (degenerate ToolUse stop) and made unsupported
Responses tool types fail honestly.
