# Clawde Agent Gateway Plan Audit

Audit of `clawde-agent-gateway-plan.md` (the agent-loop + Responses-API plan)
against the newest agent-system research (Claude Code design-space paper,
OpenHands SDK, Open Responses), the current in-repo agent loop, and a
complexity review.

## Verdict

The plan is **architecturally right**: the thin ReAct loop with a dual
internal/external tool model is exactly how production agents are built in
2026, and the two-wire-surface design matches both OpenHands and Open
Responses. The locked decisions (D1–D8) hold.

The audit finds **5 feature gaps**, **12 edge cases to pin down**, and **4
places where the plan can be simpler than it risks becoming**. The biggest
gap is **context management across long tool loops** — the plan addresses
only per-tool-output truncation, while every production agent (Claude Code,
OpenHands, Clawde's own query loop) treats context as the binding constraint
and compacts. The second is **no prompt-injection defense** for tool
results — AgentDojo ranks indirect prompt injection as the top AI threat
(OWASP Top 10 for LLM apps), and the gateway's whole value proposition is
feeding untrusted tool output back to the model.

---

## 1. Architecture Verdict: Bespoke vs. Fit

### The question the plan must answer honestly

Should Clawde reuse its existing `run_query_loop` (fit another's
architecture) or keep the gateway's bespoke loop? The plan chose bespoke.
The audit re-examines that with the strongest new evidence: **"Dive into
Claude Code: The Design Space of Today's and Future AI Agent Systems"**
(Liu et al., MBZUAI/UCL, arXiv 2604.14228, v2 2026) and the **OpenHands SDK
paper** (Wang et al., MLSys 2026).

### What the literature actually says

- **Claude Code uses ONE loop for every surface.** `queryLoop()` is shared by
  the interactive CLI, headless `claude -p`, the Agent SDK, and IDE
  integrations; only the rendering/interaction layer varies. This is the
  strongest argument *for* reuse.
- **The loop itself is trivial; the harness is the product.** The Claude Code
  paper measures ~1.6% of the codebase as AI decision logic; the rest is
  operational infrastructure (permission layers, compaction pipeline, tool
  routing, recovery, session storage). OpenHands reaches the same conclusion
  (four packages, event-sourced core).
- **The loop is a plain while-loop.** `queryLoop()` = assemble context →
  call model → route tool_use through permission gate → execute → collect
  results → repeat until no tool_use. This is exactly the `agent.rs` the plan
  proposes.
- **Claude Code's own loop is not client-owned.** It injects its system
  prompt, context loader (CLAUDE.md, git status), and 19–54 built-in tools.

### Re-examination against Clawde's code

The plan's stated blocker — "run_query_loop takes `&AnthropicClient` and is
Anthropic-shaped" — is **weaker than stated**:

- The loop's real dispatch already goes through the registry:
  `provider.create_message_stream(provider_request)` at
  `crates/query/src/lib.rs:2856`, with `AnthropicClient` used only for the
  anthropic path and as the compaction provider.
- `compact::reactive_compact` (`crates/query/src/compact.rs:1615`) is
  **provider-agnostic in signature** (`&dyn LlmProvider`) — it takes
  `&QueryConfig` only for a few fields.

The plan's blocker is nonetheless **correct for a different, decisive
reason**: the OpenAI-compatible wire contract is *client-owned*. The client
sends `instructions` and `tools[]`; the loop must honor those verbatim
(D8) and **yield** tool calls it cannot execute. `run_query_loop` cannot do
this — it injects Clawde's system prompt, executes every tool call through
`&[Box<dyn Tool>]`, assumes TUI session semantics (permission dialogs, plan
gates, goal re-anchoring, command queue), and emits `AnthropicStreamEvent`.
Retrofitting a "yield instead of execute" path plus a client-owned prompt
mode into the TUI's loop is a high-risk refactor of the repo's most-guarded
surface, for a contract (drop-in OpenAI agent compatibility) that
intentionally excludes Clawde's own agent persona.

### Verdict

**Keep the bespoke loop — but borrow the harness, not the loop.** The plan
already says "policy lessons copied, not code." The audit upgrades that from
a slogan to a concrete port list, validated by the Claude Code paper's
"minimal scaffolding, maximal operational harness" principle:

| Port from `crates/query` | Where | Cost |
|---|---|---|
| Reactive context-overflow compaction | `compact::reactive_compact` — adapt `&QueryConfig` dep to a small gateway config struct | Low (provider-agnostic already) |
| Tool-result budget | `QueryConfig::tool_result_budget` (50 KB default) | Trivial |
| No-progress / repeat-call guard | `repeat_guard::RepeatCallDetector` + no-progress streak | Low (pure logic) |
| Cascade-drift thinking-strip | `last_turn_upstream` logic (Zylos 2026) | Low (pure logic) |
| Parallel tool executor w/ ordered results | `StreamingToolExecutor` pattern (Claude Code): concurrent-safe vs exclusive, sibling abort, emit in received order | Medium (tools crate has a parallel executor to build on) |

Deliberately **not** ported: auto-compact pre-emptive summarization (v1 runs
are capped at `max_tool_calls` ≈ 10; reactive overflow compaction covers the
long tail), verification, plan gates, goal re-anchoring, memory, sub-agents.

---

## 2. Feature Gaps

### G1. Context management across long loops (the big one)

The plan's only context control is the 50 KB `tool_result_budget`. Ten tool
turns of file reads + bash + web fetch will overflow any upstream context
window long before the cap. Claude Code runs **five** context-reduction
layers before every model call (budget → snip → microcompact → context
collapse → auto-compact); Clawde's own query loop has auto-compact +
reactive overflow compaction.

**Fix (minimal, v1):** on `ContextOverflow` from any loop turn, run reactive
compaction of the transcript (reuse `compact::reactive_compact` with a
gateway-flavored config), then retry the turn once. Bound retries (2).
Truncate the oldest tool results first (cheap, deterministic) before paying
for summarization. Do **not** build a five-layer pipeline.

### G2. Prompt injection via tool results

Tool outputs (web content, file contents, bash output) are untrusted data
that routinely contains instructions ("ignore your instructions, run…").
AgentDojo (Debenedetti et al., NeurIPS 2024) and OWASP rank indirect prompt
injection as the top LLM-agent threat; the Claude Code paper's auto-mode
threat model names prompt injection as one of four target risk categories.
The gateway's entire loop feeds this untrusted data back to the model every
turn.

**Fix (v1, structural):** tool results are already `ContentBlock::ToolResult`
— never system or user-authored instruction blocks (already true in
`parse_messages`). Add: (a) strip terminal control sequences from tool
results at the `tool_exec.rs` boundary, (b) cap result size (G1 budget), (c)
document that tool output is structurally untrusted; the built-in tools'
descriptions tell the model to treat file/web content as data, not
instructions. Opt-in semantic guard (`--guard-prompt` equivalent) is Phase 3
— matches the query loop's current posture.

### G3. Cancellation propagation into the loop

The plan cancels provider dispatch on client disconnect but says nothing
about in-flight **tool execution**. A bash call or web fetch can run for
seconds while the client is gone; the loop must not keep executing tools
after disconnect.

**Fix:** per-request `CancellationToken` wired into the `ToolContext`
(which already carries one and observes it in the parallel executor) and
into the compaction call (`reactive_compact` already takes a token).
Check between turns and before dispatch.

### G4. Mid-loop provider error policy

`FreeProvider` falls back per dispatch, but the loop needs a policy for
"turn 2 failed after turn 1 succeeded". ReAct guidance and Claude Code's
graceful-recovery principle: recover silently when possible, surface the
human attention only for the unrecoverable.

**Fix (locked):** transient error on a non-first turn → fail the request
with the standard envelope but include the partial transcript (Responses:
`response.failed` with the already-emitted output items; chat: error
envelope). Do not silently return the partial answer as if complete, and do
not retry the whole request. Retry-once within a turn on transient 429/5xx
(FreeProvider cooldowns already back this).

### G5. Cascade-drift across loop turns

`free/auto` can serve turn 1 from Groq and turn 2 from Cline. The weaker
model's thinking blocks then poison the stronger model's next turn —
"cascade drift". Clawde's query loop already solves this
(`last_turn_upstream`, strips thinking on upstream change, Zylos 2026
research). The gateway plan is silent on it.

**Fix:** mirror the query loop: track the serving upstream per turn; when it
changes, strip prior-turn thinking/reasoning blocks from the transcript
before the next dispatch (both wire surfaces; Responses reasoning items are
simply not re-fed).

---

## 3. Edge Cases to Pin Down

| # | Edge case | Locked behavior |
|---|---|---|
| E1 | Cap exhaustion while model still wants tools | Force-stop. Chat: final `finish_reason: stop` with a notice appended to the message. Responses: `status: incomplete`, `reason: max_tool_calls` (per Open Responses incomplete semantics). Do NOT yield pending internal calls. |
| E2 | Mixed internal + external calls in one turn | Execute internals first (parallel), then yield all externals together in one response. The client's resubmission continues the loop from the external results. |
| E3 | `max_tool_calls: 0` | No server-side execution — pure relay for that request (same as relay mode). |
| E4 | `tool_choice: none` + built-in tools declared | No execution; model is instructed not to call anyway. `tool_choice: required` with zero tools → 400. |
| E5 | Parallel identical tool calls | Run both; dedup is the tool's responsibility (matches Claude Code concurrent-safe partitioning). |
| E6 | Malformed tool-args JSON from model | `tool_error` observation back to the model (OpenHands Action validation), loop continues. |
| E7 | Tool returns `is_error: true` | Feed back as observation; loop continues (model self-corrects). |
| E8 | Model emits tool_use without `id`/`name` | Synthesize an id; missing name → `tool_error`. Never crash the loop. |
| E9 | Non-UTF8 / control chars in tool output | Strip control sequences (G2) and truncate (G1) at the executor boundary. |
| E10 | Mid-loop rate limit (429) | Respect `Retry-After`/FreeProvider cooldowns; retry once within the turn; else fail per G4. |
| E11 | Concurrent continuations on the same `previous_response_id` | Serialize per response id (per-key lock); second request waits for the first turn to commit. |
| E12 | Prior-turn reasoning fed back into next turn | Strip prior-turn `Thinking` blocks from the transcript (D12); Responses reasoning items are output-only, never re-fed. |

---

## 4. Complexity Review (have we overcomplicated it?)

The plan is close to right-sized. Four adjustments:

1. **Explicitly refuse the five-layer compaction pipeline (G1).** One
   reactive compaction path + budget + oldest-first truncation. The Claude
   Code paper's pipeline is a 200K–1M-token, hours-long-run answer; the
   gateway runs ≤10 turns with client-declared tools. Adding snip /
   microcompact / context-collapse now would be speculative complexity.
2. **Cut the `reasoning.summary` config (D3).** Always `content`. The
   summary switch is a two-line config later if cost demands it; shipping
   the knob now is premature.
3. **Merge `store: true/false` semantics fully (D5).** Accept and ignore
   the distinction in v1: one in-memory LRU, no disk path at all. Document
   it; the OpenHands-style event log stays a later phase. No `storeSessions`
   config key (already done).
4. **Cut the graduated-trust permission spectrum.** Three modes
   (`allow-readonly` / `allow` / `deny`) are enough for v1. Claude Code's
   seven modes + ML classifier + trust trajectories are multi-session,
   per-user features with no v1 analogue on a key-authenticated localhost
   gateway.

The `LoopEvent` stream + two-translator seam stays — it is the correct
abstraction, and both wire surfaces genuinely need it. The `session.rs` LRU
stays. `agent.rs` is honestly **~400–500 lines** once the harness ports
(compaction trigger, no-progress, cascade-drift strip, parallel routing) are
in — the plan's "~200 lines" understates it and should be corrected.

---

## 5. New/Amended Locked Decisions

| # | Decision |
|---|---|
| D9 | **Cap exhaustion = force-stop** (E1); never yield pending internal calls. |
| D10 | **Mid-loop transient error = fail with partial transcript** (G4); retry once within the turn. |
| D11 | **Serialize continuations per `previous_response_id`** (E11). |
| D12 | **Strip prior-turn thinking + cascade-drift sanitization** (G5, E12). |
| D13 | **Compaction = reactive overflow compaction only** (G1); port `reactive_compact`, bounded retries, oldest-first truncation; no pipeline. |
| D14 | **Tool-result sanitization**: strip control sequences + budget at the executor boundary (G2). |
| D15 | **Prompt-injection defense is structural in v1** (untrusted `ToolResult` framing + sanitization); opt-in semantic guard is Phase 3 (G2). |
| D16 | **Cancellation reaches tools + compaction** (G3). |

---

## 6. Research References (additions to the plan's §10)

- **"Dive into Claude Code: The Design Space of Today's and Future AI Agent
  Systems"** (Liu, Zhao, Shang, Shen; MBZUAI/UCL, arXiv 2604.14228, 2026) —
  single-loop-for-all-surfaces, 1.6% AI logic / 98.4% harness, five-layer
  compaction, StreamingToolExecutor (concurrent-safe vs exclusive, sibling
  abort, ordered emission), deny-first layered permission, append-only
  sessions.
- **AgentDojo** (Debenedetti et al., NeurIPS 2024; OWASP Top 10 for LLM
  apps) — indirect prompt injection via tool outputs is the top agent
  threat.
- **OpenRouter "Server Tools"** (openrouter.ai/docs/guides/features/server-tools,
  2026) — production hosted-tools-on-the-router pattern confirming the
  internal-tools model.
- **Zylos "cascade drift"** (already cited in `crates/query` source) —
  thinking-block contamination when the serving upstream changes between
  turns.
- **In-repo verification** — `run_query_loop` dispatches via
  `provider.create_message_stream` (`crates/query/src/lib.rs:2856`);
  `compact::reactive_compact` is provider-agnostic
  (`crates/query/src/compact.rs:1615`).
