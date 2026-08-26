# Clawde OpenAI-Compatible Agent: Implementation Plan

Makes `clawde serve` a full agent, not just a chat relay: the gateway runs a
tool loop, executes built-in tools locally, yields client-defined functions
back, and speaks both wire formats (Chat Completions and the Responses API /
Open Responses). Picks up where `clawde-gateway-implementation-plan.md`
(chat-completions relay) and `gateway-audit.md` (wire-fidelity gaps) left off.

## 1. Goals

- **Turn the gateway into an agent server**: a request can trigger multiple
  model round-trips with tool execution between them (reason → act → observe →
  repeat), capped by `max_tool_calls` — the ReAct loop (Yao et al., 2022) made
  first-class on the wire.
- **Execute Clawde's built-in tools server-side** (file read/write/edit, glob,
  grep, bash, web search/fetch, apply-patch…) via the existing `clawde-tools`
  registry — the "internally-hosted tools" model of Open Responses.
- **Yield client-defined functions back** to the caller for external
  execution — the "externally-hosted tools" model. This is what most OpenAI
  SDK clients expect today and must keep working.
- **Add `POST /v1/responses`** (Responses API / Open Responses wire format):
  items (`message`, `function_call`, `function_call_output`), semantic
  streaming events, state machines, `previous_response_id` continuation, and
  (later) WebSocket transport.
- **Reuse the provider machinery unchanged**: `ProviderRegistry`,
  `FreeProvider` cascade, key rotation, cooldowns — the gateway stays a thin
  adapter on the same seam the TUI uses. No changes to `tui`, `query`, or
  `api` provider internals.
- **Stay safe by default**: local tool execution behind explicit gateway
  config (workspace roots + permission mode). A public agent endpoint that can
  run `bash` is a foot-gun; the default must be deny-write / read-only.

Scope guardrail (v1): one agent loop with two wire surfaces. No memory,
sub-agents, multi-agent handoffs, or persistence beyond session state for
`previous_response_id` continuation.

## 2. Current State (verified against the code)

- **Gateway (relay)** — `crates/gateway`. `POST /v1/chat/completions`
  (stream + non-stream), `GET /v1/models`, `GET /v1/models/{id}`,
  `GET /healthz`, `GET /status`. Bearer auth + per-key RPM/TPM buckets
  (`auth.rs`), OpenAI error envelope (`error.rs`), semaphore in-flight cap +
  timeouts + graceful SSE drain (`router.rs`), OpenAI ⇄ `ProviderRequest` /
  `StreamEvent` translation incl. tool-call streaming
  (`translate.rs`, `StreamTranslator`).
- **Tool calling today is pass-through only**: `parse_messages` converts
  client `tool_calls` / `tool` results into `ContentBlock::ToolUse` /
  `ToolResult`, and `StreamTranslator` streams tool-call deltas. The gateway
  never *executes* a tool; clients that want a loop must do it themselves
  (resubmit with tool results). This is correct relay behavior and must not
  regress.
- **Agent loop already exists in-tree** — `crates/query/src/lib.rs`
  `run_query_loop`: multi-turn loop with tools (`&[Box<dyn Tool>]` +
  `ToolContext`), auto-compact, no-progress detection, effort shaping,
  fallback-model switching, verification. **It is not reusable as-is** (see
  audit §1): the OpenAI-compatible contract is *client-owned* — the client's
  `instructions` and `tools[]` drive the model, and unknown tools must be
  *yielded* back, while `run_query_loop` injects Clawde's system prompt,
  executes every tool call, and assumes TUI session semantics (permission
  dialogs, plan gates, goal re-anchoring). **The gateway gets its own thin
  loop** — but the loop's *operational harness* is ported from the query
  crate where it is provider-agnostic: `compact::reactive_compact`
  (`compact.rs:1615`), the tool-result budget, the no-progress/repeat guard,
  cascade-drift thinking-strip, and the parallel-executor pattern (audit §1
  port table). The loop itself is simple; the harness is where production
  agents spend their code (Claude Code design-space paper: ~1.6% AI logic,
  ~98.4% operational infrastructure).
- **Tools** — `crates/tools`: `all_tools() -> Vec<Box<dyn Tool>>` (~45
  built-ins), `Tool::execute(input, ToolContext) -> ToolResult`, permission
  levels, network-capability metadata. `ToolContext` carries working dir,
  permission handler, cost tracker, session id, cancel token — everything a
  gateway execution context needs. Tools are keyed by `Tool::name()`; a
  client tool with the same name can be bound to a built-in implementation.
- **Registry** — `crates/api`: `LlmProvider::create_message(_stream)`,
  `ProviderRequest` already has `tools`, `tool_choice`, `effort_level`,
  `provider_options`. `FreeProvider` owns fallback; `resolve_model` in the
  gateway maps model strings.

## 3. Research Summary (bleeding-edge state, Jan–Aug 2026)

### 3a. The industry is moving to the Responses API; chat completions is legacy for agents

- **Open Responses spec (Jan 2026, openresponses.org; initiated by OpenAI,
  built with HF + community)** is the open standard built on OpenAI's
  Responses API. It exists precisely because "the Chat Completion format …
  was designed for turn-based conversations and falls short for agentic use
  cases" (HF blog, Jan 2026).
- Core ideas we adopt: **items** (bidirectional: `message`, `function_call`,
  `function_call_output` — input to and output from the model), **semantic
  streaming events** (state transitions `response.in_progress/completed`
  + deltas `response.output_item.added`, `response.output_text.delta`,
  `response.function_call_arguments.delta`, `response.output_item.done`),
  **state machines** (`in_progress` / `completed` / `incomplete` per item and
  response), **`tool_choice`** (`auto` / `required` / `none` / forced
  function), **`allowed_tools`** (subset enforcement without mutating `tools`
  — cache-preserving control), **`previous_response_id`** (continue without
  resending the transcript), **`max_tool_calls`** (cap loop iterations),
  **WebSocket transport** (added 2026-04-24).
- **Real-world demand signal**: Cursor Agent mode already sends Responses-API
  payloads and breaks chat-completions-only proxies (Cursor forum, Feb 2026;
  workaround projects like Cursor-OpenAI-BYOK-Bridge exist to translate the
  other way). Inference servers are converging: vLLM has an open Responses
  extensions issue (#32850), llama-stack tracks `parallel_tool_calls` for
  Responses (#4123).
- **Conclusion**: implement `/v1/responses` as the agent-native surface, keep
  `/v1/chat/completions` for SDK/legacy clients (with an agent mode bolted on
  for built-in tool execution).

### 3b. Agent loop architecture (CS literature)

- **ReAct (Yao et al., 2022, arXiv 2210.03629)** — interleave reasoning
  traces and actions; the canonical loop. Practical guidance distilled from
  the literature: **always cap iterations (5–15)** to prevent runaway loops
  (Alice Labs, 2026; IBM, 2026). Our `max_tool_calls` default: 10.
- **Anthropic, "Building Effective Agents" (Dec 2024)** — "agents are models
  using tools in a loop"; keep the loop simple, use the smallest tool surface
  that works, and prefer workflows over agents when a deterministic path
  exists. We expose a curated built-in tool subset, not all 45 tools, by
  default.
- **OpenHands Software Agent SDK (Wang et al., MLSys 2026, arXiv
  2511.03690)** — the strongest recent reference architecture for exactly
  this problem:
  - **Event-sourced state**: append-only event log, single `ConversationState`
    as the only mutable component; deterministic replay; measured sub-ms
    persist latency and <20 ms crash recovery; **61% reduction in
    system-attributable failures** vs the monolithic V0.
  - **Action–Execution–Observation tool contract**: validate LLM JSON args
    against a schema before execution, execute, convert result to an
    LLM-compatible observation. We mirror this in the gateway tool executor.
  - **Two API surfaces**: Chat Completions for broad compat + Responses API
    for reasoning models — exactly our two-surface design.
  - **NonNativeToolCallingMixin**: models without native function calling get
    text-prompt tool schemas + regex extraction. Relevant because free-tier
    upstreams vary wildly in tool-call fidelity (worth an optional gateway
    fallback, out of v1 scope).
- **FrugalGPT (Stanford, 2023)** — LLM cascades; already embodied by
  `FreeProvider::free/auto`. The agent loop inherits it for free; each loop
  turn re-dispatches through the cascade.
- **"Dive into Claude Code: The Design Space of Today's and Future AI Agent
  Systems"** (Liu et al., MBZUAI/UCL, arXiv 2604.14228, 2026) — the
  strongest current reference on agent harness architecture: ONE loop for all
  surfaces (CLI/headless/SDK/IDE) with ~1.6% of code as AI logic and ~98.4%
  as operational harness; a **five-layer compaction pipeline** (context is
  the binding resource); a **StreamingToolExecutor** that starts tools as
  they stream in, partitions into concurrent-safe (read-only, parallel) vs
  exclusive (state-modifying, serialized), aborts siblings on error, and
  emits results in received order; deny-first layered permissions;
  append-only session transcripts. Adopted: harness-over-loop philosophy,
  compaction (reduced to reactive-only for v1, see D13), the parallel
  executor pattern, ordered result emission. Not adopted: the full
  five-layer pipeline and graduated trust (multi-session features, see §4
  audit).
- **AgentDojo (Debenedetti et al., NeurIPS 2024)** + OWASP Top 10 for LLM
  apps — indirect prompt injection via tool outputs is the top agent
  security threat. Adopted: structural untrusted-data framing + tool-result
  sanitization (D14/D15).
- **OpenRouter "Server Tools"** (2026) — production hosted-tools-on-the-
  router pattern, confirming the internal-tools model the gateway executes
  server-side.

### 3c. Streaming the loop

- Chat Completions SSE carries **one assistant message per stream**. For
  server-side tool execution, intermediate turns cannot be emitted as
  separate messages cleanly. Two viable patterns in the wild:
  - yield tool-call deltas + `finish_reason: tool_calls`, let the client
    resubmit (external tool model — current gateway behavior, keep);
  - run intermediate server-side turns silently and stream only the final
    assistant turn (internal/hosted tool model — used by NIM-style agentic
    endpoints). v1 agent mode for chat completions uses this.
- The Responses API makes multi-turn streaming natural (items are streamed as
  they happen, including tool calls *and* their outputs) — another argument
  for making Responses the agent-native surface.

## 4. Proposed Architecture

```
  OpenAI client (openai-python / Cursor / aider / Agents SDK)
                    |
                    v
  +-------------------------------------------------------------+
  |  crates/gateway  (axum)                                     |
  |  POST /v1/chat/completions   POST /v1/responses             |
  |  GET /v1/models  GET /v1/models/{id}  /healthz  /status     |
  |                                                             |
  |  relay.rs      translate.rs     responses.rs (new)          |
  |  (chat handler) (wire fidelity) (items/events translate)    |
  |        \            |                /                      |
  |         v           v               v                       |
  |      agent.rs (new) — the loop: dispatch -> inspect ->      |
  |        execute-or-yield -> append -> repeat (max_tool_calls)|
  |        |                      |                            |
  |        v                      v                             |
  |  tool_exec.rs (new)     session.rs (new)                    |
  |  built-in ToolContext   previous_response_id store          |
  |  + permission gate      (in-memory LRU, optional persist)   |
  +-----------------------------+------------------------------+
                                | create_message(_stream)
                                v
  +---------------------------------------------+
  |  crates/api ProviderRegistry (unchanged)    |
  |  FreeProvider / KeyRotating / direct        |
  +---------------------------------------------+
```

### Key design decisions

1. **Gateway-native loop in `agent.rs`, not `run_query_loop` (audit §1).**
   The loop must map 1:1 onto wire items: dispatch `ProviderRequest` →
   inspect `ProviderResponse`/`StreamEvent` for `ToolUse` blocks → for each:
   execute (built-in) or yield (external) → append results as messages →
   re-dispatch. Reuse is rejected because the OpenAI contract is
   client-owned (the loop cannot inject Clawde's prompt or execute
   client-declared functions). The loop is ~400–500 lines (honest estimate
   including the harness ports below); the *harness* is ported from
   `crates/query` where provider-agnostic: reactive compaction (D13),
   tool-result budget, no-progress guard, cascade-drift thinking-strip
   (D12), parallel-executor pattern. The loop stays thin; the harness does
   the work — the Claude Code paper's 1.6% / 98.4% split.
2. **Two wire surfaces, one loop.** `responses.rs` is a second translator over
   the same `agent.rs` core, exactly as `translate.rs` is a translator over
   `LlmProvider`. Chat completions keeps relay mode (zero behavior change);
   agent mode activates when a client tool maps to a built-in and
   `max_tool_calls` is present (or gateway `agentMode` is enabled).
3. **Internal vs external tools (Open Responses taxonomy).** A client
   `tools[]` entry is *internal* if its `function.name` matches a built-in
   Clawde tool name AND the gateway's built-in tool surface is enabled; else
   *external*. Internal → execute via `clawde-tools`; external → yield
   `function_call` back. This is the single most important semantic in the
   plan — it decides whether the gateway is a relay or an agent per request.
4. **`ProviderRequest` is the loop seam.** Loop turns re-use
   `create_message` / `create_message_stream` with growing `messages`. The
   `tool_choice` passthrough already in `provider_options` lets us honor
   `tool_choice: none` / forced-function without touching providers.
5. **Permission gate mirrors the ACP server.** `ToolContext.permission_handler`
   is configurable; the gateway ships a headless handler: `allow` (all built-in
   tools), `deny` (nothing executes; yields a `permission_denied` tool error
   back to the model — matches Clawde's existing blocked-tool semantics), or
   `allow-readonly` (default: file read/glob/grep/web search allowed; write +
   execute denied). `gateway.workspace_paths` restricts file access roots.
6. **Sessions are ephemeral (locked).** `previous_response_id` needs
   per-response state (transcript + tools + config). In-memory bounded LRU
   (`session.rs`, capacity 256 / TTL 1 h). `store: true` is *accepted and
   honored* as in-memory retention (the response stays available for
   continuation while the process lives) — no disk persistence in v1 (see
   §9 D5). Expiry + eviction policy documented.
7. **Parallel tool calls.** When a message carries multiple `ToolUse` blocks,
   execute concurrently under a semaphore (the tools crate already has a
   parallel executor); `parallel_tool_calls: false` serializes. External calls
   are always yielded (the client parallelizes).

## 5. Component Design

### a. `agent.rs` — the loop

```text
fn run_agent_loop(state, req, tools, max_tool_calls, mode) -> Stream<LoopEvent>
  turns = 0
  messages = req.messages
  loop:
    dispatch (create_message or create_message_stream)
    on provider error -> fail (map to envelope)
    inspect stop reason:
      EndTurn / StopSequence / MaxTokens -> emit final, stop
      ToolUse ->
        turns += 1
        if turns > max_tool_calls -> emit tool_calls back (client must
          finish) or force-stop with a note, per mode
        partition blocks: internal (execute) vs external (yield)
        emit internal tool events (start/output) + external function_calls
        append ToolResult messages for internal; append ToolUse (pending)
          for external
        if any external -> stop, yield to client (chat: finish_reason
          tool_calls; responses: function_call items)
        else continue
```

- Streams `LoopEvent`s so both translators can render progress. Usage is
  aggregated across turns (TPM accounting, spend caps).
- Reuses the gateway's existing timeouts / in-flight semaphore per dispatch.
- **Harness ports (audit §1, §2):**
  - *Cancellation* (D16): the per-request `CancellationToken` is wired into
    the `ToolContext` and the compaction call, and checked between turns.
    Client disconnect aborts in-flight tools, not just the provider call.
  - *No-progress guard* (ported from the query loop's
    `RepeatCallDetector`): identical consecutive tool calls or a small
    alternating cycle stop the loop early instead of burning the cap.
  - *Cascade-drift strip* (D12): track the serving upstream per turn; when
    it changes, strip prior-turn `Thinking` blocks from the transcript
    (free/auto can serve turn 1 from Groq and turn 2 from Cline).
  - *Mid-loop error policy* (D10): transient error on a non-first turn →
    fail with the partial transcript; retry once within the turn on
    transient 429/5xx.
  - *Cap exhaustion* (D9): force-stop with a notice — chat: final
    `finish_reason: stop`; Responses: `status: incomplete, reason:
    max_tool_calls`. Never yield pending internal calls.
- **Parallel tool execution (Claude Code StreamingToolExecutor pattern):**
  partition tool calls into concurrent-safe (read-only) vs exclusive
  (state-modifying); run concurrent-safe in parallel under the in-flight
  semaphore, serialize exclusive; abort siblings when one errors; emit
  results in the order the calls were received (models expect
  received-order results).

### b. `tool_exec.rs` — built-in tool execution

- Maps `ToolDefinition` (client-declared schema) → `Box<dyn Tool>` by name via
  `clawde_tools::all_tools()`. The tool's own `input_schema` (not the client's
  copy) validates the arguments — OpenHands Action-validation pattern: parse
  args as JSON, fail with a `tool_error` observation on mismatch instead of
  executing garbage.
- Builds a `ToolContext` per request (working dir = gateway
  `workspace_paths[0]` or process cwd, permission handler per config, session
  id from the request, cancel token per request).
- Result → `ContentBlock::ToolResult { tool_use_id, content, is_error }`,
  truncated to the `tool_result_budget` (default 50 KB, matches the query
  loop).
- **Curated built-in surface (locked, D2)**: `FileReadTool`, `GlobTool`,
  `GrepTool`, `WebFetchTool`, `WebSearchTool`, `FileWriteTool`,
  `FileEditTool`, `ApplyPatchTool`, `PtyBashTool`, `RunTestsTool`. Everything
  else stays external/yielded even if a built-in with that name exists. No
  `"*"` wildcard in v1: `gateway.builtinTools` is a *replacement* list
  (specifying it swaps the default surface), and every entry must name an
  actual built-in. Rationale: the TUI-bound tools (`AskUserQuestion`,
  `EnterPlanMode`, `TodoWrite`, `Cron*`, `Worktree*`, `Team*`, `Tasks*`,
  `SendMessage`) have no meaningful semantics over a stateless HTTP agent
  endpoint and would multiply the attack surface for zero agent value.
- **Result sanitization (D14)**: every tool result is stripped of terminal
  control sequences and truncated to `tool_result_budget` at this boundary,
  before it becomes a `ToolResult` observation — the prompt-injection
  defense is structural (tool output is untrusted data, never
  instructions), per AgentDojo/OWASP (audit G2, D15).

### c. `responses.rs` — Responses / Open Responses wire format

- `POST /v1/responses`:
  - parse `input` (items: `message` w/ roles, `function_call_output`,
    `function_call` from history) → `Vec<Message>`; `instructions` →
    `system_prompt`; `tools[]` (flat function form per Open Responses) →
    `ToolDefinition`s; `tool_choice`, `allowed_tools`, `max_output_tokens`,
    `max_tool_calls`, `parallel_tool_calls`, `previous_response_id`,
    `stream`, `store`, `reasoning`.
  - `previous_response_id` → hydrate transcript from `session.rs`, append new
    `input`, continue.
- Response object: `id`, `object: "response"`, `status` (`in_progress` /
  `completed` / `incomplete` / `failed`), `output[]` (items), `usage`,
  `error`. Items: `message` (role/content), `function_call` (id, call_id,
  name, arguments), `reasoning` (content / summary / encrypted_content —
  locked D3: `content` only, from `Thinking` blocks; `summary` switch behind
  config if cost pressure appears; `encrypted_content` never).
- Non-stream: complete `output[]` array. Stream: semantic events
  (`response.created`, `response.in_progress`, `response.output_item.added`,
  `response.output_text.delta` / `.done`, `response.function_call_arguments.delta`
  / `.done`, `response.function_call_output.done`, `response.completed`,
  terminal `[DONE]`). `created` timestamps and `sequence_number` ordering per
  the Open Responses spec.
- Reuses `StreamTranslator`-style accumulation for argument deltas (already
  proven in `translate.rs`).

### d. `session.rs` — response state (ephemeral, locked D5)

- Key: response id (`resp_…`); value: `{ input, output, tools, config }`.
- One in-memory `Mutex<LruCache>` (capacity `gateway.session_capacity`,
  default 256; TTL `gateway.session_ttl_secs`, default 1 h) backs BOTH
  `store: false` and `store: true`. `store` only affects retention intent
  (both are retained in the cache for continuation); neither writes to disk
  in v1. A later phase can add the append-only JSON event log (OpenHands
  pattern) without changing the API.
- Continuation semantics per Open Responses: model samples over
  `prev.input + prev.output + new input`; `previous_response_not_found` error
  code when evicted or expired.

### e. chat completions agent mode (bolt-on to existing handler)

- New parsed fields: `max_tool_calls` (also honored from gateway config),
  `parallel_tool_calls`. `stream: false` agent mode: run loop to completion,
  return the *final* assistant message (tool calls executed server-side
  invisible to the client, unless external tools were requested — then yield
  them).
- `stream: true` agent mode (locked D1): **silent intermediate turns** —
  internal tool executions never surface as SSE chunks; only the final
  turn streams, and external tool calls stream deltas with
  `finish_reason: tool_calls` exactly as relay mode does today. Rationale:
  streaming a server-executed tool call as `finish_reason: tool_calls` would
  invite the client to execute it *again* and resubmit — a double-execution
  hazard. Clients that want per-tool progress use `/v1/responses`, which
  streams items natively.
- Usage = aggregate across turns; `include_usage` final chunk carries the
  total.

### f. config & auth additions

```json
{
  "gateway": {
    "agentMode": false,
    "maxToolCalls": 10,
    "workspacePaths": ["/absolute/path"],
    "permissionMode": "allow-readonly",
    "builtinTools": ["read", "glob", "grep", "web_fetch", "web_search"],
    "sessionCapacity": 256,
    "sessionTtlSecs": 3600
  }
}
```

- `permissionMode`: `allow-readonly` (default) | `allow` | `deny`.
- `agentMode: true` enables server-side tool execution even when the client
  didn't send `max_tool_calls` (loop cap still applies).
- `builtinTools` replaces the default 10-tool surface when present; no
  wildcard in v1 (D2).
- No `storeSessions` knob in v1 — sessions are ephemeral (D5).
- Auth/rate limiting unchanged; agent loops count each dispatch against RPM
  and aggregate usage into TPM.

### g. context.rs — loop context management (D13, audit G1)

- **Reactive overflow compaction only** — no five-layer pipeline (the
  gateway runs ≤10 client-capped turns, not hours-long sessions; the Claude
  Code pipeline is the wrong size for v1).
- On a `ContextOverflow` from any loop turn: (1) truncate the oldest tool
  results first (deterministic, free), (2) if still overflowing, run
  `compact::reactive_compact` (ported from `crates/query/src/compact.rs`,
  provider-agnostic — adapt its `&QueryConfig` dep to a small gateway config
  struct), (3) retry the turn once. Bound retries at 2 per request.
- Token budget across turns: each turn's `max_tokens` is clamped to the
  remaining `max_output_tokens` budget; aggregate usage feeds TPM/spend
  accounting. Exceeding the total budget ends the loop as incomplete
  (Responses) / a final message (chat).

## 6. Implementation Steps

**Phase 0 — scaffold (`agent.rs` core, no wire changes)**
1. `run_agent_loop` over `LlmProvider` with a `ScriptedTool` mock; unit-test
   loop semantics: end-turn, tool-use → result → end-turn, cap enforcement
   (D9), parallel tool calls (concurrent-safe vs exclusive, ordered
   emission), provider error mid-loop (D10), tool error observation (E6/E7),
   cancellation (D16), no-progress guard, cascade-drift strip (D12).

**Phase 1 — chat completions agent mode**
2. Parser: `max_tool_calls`, `parallel_tool_calls`, `tool_choice` hardening.
3. `tool_exec.rs` (curated surface + permission gate + ToolContext +
   sanitization D14).
4. `context.rs` (reactive overflow compaction, D13).
5. Wire into `chat_completions` for stream + non-stream; keep relay mode
   default unless `max_tool_calls`/agent mode.
6. Tests: mock provider scripted multi-turn; assert final message + aggregate
   usage; external-tool yield path unchanged (existing golden tests stay
   green); permission-deny path; stream shape of agent mode; overflow→
   compaction→retry path; mid-loop error envelope (D10); disconnect
   cancels tools (D16).

**Phase 2 — Responses API**
7. `responses.rs` request/response/items translation + event streaming
   (incl. prior-turn reasoning strip, D12).
8. `session.rs` (in-memory LRU only — D5; per-key continuation lock, D11).
9. `POST /v1/responses` route, auth-gated; `GET /v1/models` unchanged.
10. Golden event transcripts (Open Responses spec fixtures); continuation
    tests (`previous_response_id`, eviction → `previous_response_not_found`,
    concurrent continuation serialization D11); `tool_choice`/`allowed_tools`
    enforcement tests; incomplete/max_tool_calls cap test (D9).
11. openai-python (`client.responses.create`) + Agents SDK smoke tests; curl
    SSE transcript.

**Phase 3 — hardening**
12. Docs (`docs/gateway.md` rewrite: agent mode, responses endpoint, config,
    security posture incl. D14/D15), README, cross-link ACP/MCP.
13. Hygiene: `cargo test --workspace`, clippy `-D warnings`, fmt; idle-CPU
    probe unaffected (separate process).

## 7. Risks and Mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | Server-side tool execution is dangerous (bash, writes) | Default `permissionMode: allow-readonly`, `workspacePaths` required for write/execute tools; `deny` mode = relay only. Headless permission handler never prompts; blocked tools return an explicit `permission_denied` tool error to the model (matches Clawde semantics). |
| R2 | Loop never terminates (model keeps calling tools) | `max_tool_calls` cap (default 10, ReAct guidance), per-request timeout already present, no-progress detection ported from the query loop (identical consecutive call / small cycle → stop). |
| R3 | Huge tool outputs blow context / cost | `tool_result_budget` truncation (50 KB default); usage aggregation feeds TPM/spend accounting per key. |
| R4 | Client tools collide with built-in names | Curated v1 surface (D2) — only unambiguous names; a client tool with a built-in name but different schema is executed against the built-in's own schema (validation fails safely on mismatch). |
| R5 | Responses API fidelity rejected by clients | Golden Open Responses event transcripts; `sequence_number` ordering; `created` stability; terminal `[DONE]`; openai-python + Agents SDK integration tests. |
| R6 | Session store grows unbounded | Bounded LRU + TTL (single in-memory cache, D5); eviction is the GC. |
| R7 | Parallel tool execution races on shared state | Tools crate already has a parallel executor with per-tool cancellation; execute under the gateway's in-flight semaphore; serial when `parallel_tool_calls: false`. |
| R8 | Provider doesn't support tool calls on a routed model | FreeProvider already gates capability (`tool_calling_for`); the loop treats a tool-less model's text reply as terminal (no infinite re-dispatch). |
| R9 | Breaking existing relay behavior | Relay mode remains the default; agent mode only activates with `max_tool_calls`/`agentMode`; existing golden chat-completion tests must stay green untouched. |
| R10 | Context overflow on long loops | Reactive compaction (D13): oldest-tool-result truncation → `reactive_compact` → retry once, ≤2 retries; per-turn max_tokens clamped to remaining budget. |
| R11 | Prompt injection via tool output | Structural: `ToolResult` is never an instruction block; control sequences stripped + budget truncated (D14/D15). |
| R12 | Cascade drift across free/auto turns | Track serving upstream per turn; strip prior-turn thinking on change (D12). |
| R13 | Client disconnect leaves tools running | Cancellation token wired into ToolContext + compaction, checked between turns (D16). |

## 8. Deliverables

1. `agent.rs` loop + harness (cancellation D16, no-progress, cascade-drift
   D12, mid-loop error policy D10, cap exhaustion D9) + `tool_exec.rs`
   executor (curated built-ins + permission gate + sanitization D14).
2. `context.rs` reactive overflow compaction (D13).
3. Chat completions agent mode (stream + non-stream) with `max_tool_calls`.
4. `POST /v1/responses` (items, semantic events, non-stream + stream) per
   Open Responses.
5. `session.rs` (`previous_response_id` continuation, ephemeral in-memory
   only — D5; per-key lock D11).
6. `tool_choice` / `allowed_tools` enforcement on both surfaces.
7. Tests: loop semantics incl. E1–E12, tool execution + permission paths,
   Responses golden transcripts, continuation/eviction, compaction,
   injection-sanitization, openai-python + Agents SDK smoke.
8. `docs/gateway.md` rewrite + README.

## 9. Decisions (locked)

All defaults below are resolved; revisit only with a concrete feature request.

| # | Question | Locked decision | Rationale |
|---|---|---|---|
| D1 | Loop stream shape, chat completions agent mode | **Silent intermediate turns.** Internal tool executions never appear as SSE chunks; only the final turn streams. External tool calls stream exactly as relay mode does today (`finish_reason: tool_calls`). | Streaming a server-executed tool call invites the client to execute it again (double-execution hazard). Per-tool progress belongs on `/v1/responses`, which streams items natively. |
| D2 | Built-in tool surface | **The curated 10-tool list; no wildcard.** `gateway.builtinTools` is a replacement list naming real built-ins. | The remaining ~35 tools are TUI/session-bound (`AskUserQuestion`, `EnterPlanMode`, `TodoWrite`, `Cron*`, `Worktree*`, `Team*`, `Tasks*`, `SendMessage`) with no meaning over a stateless HTTP endpoint; smallest tool surface that works (Anthropic). |
| D3 | Responses `reasoning` exposure | **Raw `content`** from `Thinking` blocks, truncated to a budget. `summary` behind config only if cost pressure appears; `encrypted_content` never. | Mirrors existing `reasoning_content` in chat completions (consistency across surfaces); Open Responses explicitly permits raw content. |
| D4 | Non-native tool-calling fallback (OpenHands mixin) | **Not in v1.** | FreeProvider already gates tool capability and the loop treats a tool-less model's text reply as terminal (R8). Text-prompt schemas + regex parsing are fragile; revisit as Phase 3 candidate on demand. |
| D5 | Session storage | **Ephemeral in-memory only.** Single bounded LRU (256 / 1 h TTL) backs both `store: false` and `store: true`; no disk writes in v1. | Localhost single-process gateway; persistence adds disk/GC/recovery complexity for zero v1 value. OpenHands-style event-log persistence is a clean later phase behind the same API. |
| D6 | `allowed_tools` enforcement | **Reject** disallowed calls with a `tool_error` observation (`is_error: true`), letting the model self-correct. | Matches Clawde's existing permission-denied semantics; silent translation hides the failure from the model and degrades answers. Open Responses explicitly permits either. |
| D7 | `n > 1` in Responses | **Reject with 400**, same as chat completions (tolerate absent/`n == 1`). | Consistency; multi-choice mapping out of scope. |
| D8 | Gateway agent system prompt | **Client `instructions` verbatim**; a minimal gateway tool-use preamble is injected **only when the client sends none**. | Overriding a client's system prompt breaks persona-driven clients; each tool's description already documents its contract. |
| D9 | Cap exhaustion behavior | **Force-stop** with a notice (chat: final `finish_reason: stop`; Responses: `incomplete` / `max_tool_calls`). Never yield pending internal calls. | Yielding a server-side tool call invites the client to double-execute it (same hazard as D1). |
| D10 | Mid-loop provider error | **Fail with the partial transcript** (Responses: `response.failed` with emitted items); retry once within the turn on transient 429/5xx. | Graceful recovery per Claude Code paper + ReAct guidance; don't silently return a partial answer as complete. |
| D11 | Concurrent continuations | **Serialize per `previous_response_id`** (per-key lock; second request waits for the first turn to commit). | Response state is append-only per Open Responses; parallel mutation would corrupt the transcript. |
| D12 | Cascade drift + prior-turn thinking | **Strip prior-turn `Thinking` blocks from the transcript** whenever the serving upstream changes between turns (free/auto cascade). **Known limitation (accepted): the strip lags one turn** — the upstream is only known after dispatch, so the first turn served by a new upstream sees the previous upstream's thinking; every subsequent dispatch is clean. Blast radius: one turn (the fallback turn, already degraded by the switch); pinned routes never switch. Matches `run_query_loop`. Revisit only if (a) empirical contamination evidence on free-tier cascades, or (b) FreeProvider exposes the planned upstream pre-dispatch (then the strip moves to request-build time with no lag). | Zylos "cascade drift": a weaker model's reasoning contaminates the next turn's stronger model. Query loop already does this. |
| D13 | Context compaction | **Reactive overflow compaction only** (oldest-tool-result truncation first, then `reactive_compact`, retry once, ≤2 retries). No five-layer pipeline. | The Claude Code pipeline is sized for 200K–1M-token hour-long runs; the gateway runs ≤10 capped turns. Refusing the pipeline is the simplicity check. |
| D14 | Tool-result sanitization | **Strip control sequences + truncate to budget** at the executor boundary. | Prompt injection via tool output is the top agent threat (AgentDojo/OWASP); structural defense is the v1 posture. |
| D15 | Prompt-injection defense | **Structural in v1**: untrusted `ToolResult` framing + sanitization (D14). Opt-in semantic guard (`--guard-prompt` equivalent) is Phase 3. | Matches the query loop's current posture; a semantic classifier is out of scope. |
| D16 | Cancellation scope | **Per-request `CancellationToken` reaches tool execution and compaction**, checked between turns. | Client disconnect must not leave a bash/web call running. |

## 10. Research References

- **Open Responses specification** — openresponses.org/specification (items,
  semantic events, state machines, tool_choice, allowed_tools,
  previous_response_id, max_tool_calls, WebSocket transport, reasoning
  content/summary/encrypted_content).
- **Hugging Face, "Open Responses: What you need to know"** (Jan 2026) —
  motivation, router vs model-provider split, hosted vs external tools.
- **OpenAI Responses API reference** — create endpoint, streaming events
  (`response.output_item.added`, `response.output_text.delta`,
  `response.function_call_arguments.delta`), usage.
- **OpenAI Agents SDK** — runner performs the tool loop, pauses for
  approval (human-in-the-loop interruptions), sessions.
- **OpenHands Software Agent SDK** (Wang et al., MLSys 2026, arXiv
  2511.03690) — event-sourced state, Action–Execution–Observation, two API
  surfaces (chat + responses), NonNativeToolCallingMixin, RouterLLM, 61%
  system-failure reduction.
- **ReAct** (Yao et al., 2022, arXiv 2210.03629) — reason/act interleave;
  iteration caps 5–15 per practitioner guidance (Alice Labs, IBM 2026).
- **Anthropic, "Building Effective Agents"** (Dec 2024) — agents as tools-in-
  a-loop, minimal tool surface, workflows-first.
- **FrugalGPT** (Stanford, 2023) — LLM cascades; already embodied by
  `FreeProvider`.
- **Industry convergence data points** — Cursor Agent sending Responses-API
  payloads (Cursor forum, Feb 2026); vLLM Responses extensions issue #32850;
  llama-stack Responses `parallel_tool_calls` issue #4123.
- **In-repo prior art** — `docs/plans/clawde-gateway-implementation-plan.md`
  (relay), `docs/plans/gateway-audit.md` (wire-fidelity gaps),
  `docs/plans/clawde-agent-gateway-audit.md` (this plan's audit: feature
  gaps G1–G5, edge cases E1–E12, simplicity review), `crates/query`
  `run_query_loop` (harness reference; dispatch via registry at lib.rs:2856;
  `compact::reactive_compact` at compact.rs:1615), `crates/tools` (Tool
  trait, `all_tools()`), `crates/gateway` (existing relay + auth + shutdown).
- **"Dive into Claude Code"** (Liu et al., arXiv 2604.14228, 2026) —
  single-loop architecture, harness/loop split, compaction pipeline,
  StreamingToolExecutor (parallel partitioning + sibling abort + ordered
  emission), deny-first permissions.
- **AgentDojo** (Debenedetti et al., NeurIPS 2024; OWASP Top 10 for LLM
  apps) — prompt injection via tool outputs as the top agent threat.
- **OpenRouter Server Tools** (2026) — hosted tools on the router side
  (internal-tools model in production).
- **Zylos "cascade drift"** (cited in `crates/query`) — thinking-block
  contamination across upstream switches.
