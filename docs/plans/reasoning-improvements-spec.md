# Clawde Reasoning Improvements — Spec

Status: Draft (post-interview)
Scope: TUI/CLI, free-mode-compatible, same conversational flow
Author: agent + user, from structured interview (3 rounds)

---

## 1. Origin of the request

The user finds Clawde's reasoning shallow: thinking may be enabled but produces
superficial results — solutions without considering alternatives, missed edge
cases, wrong assumptions not caught, rushing to code without planning. This
affects all task types equally (code changes, debugging, design, review). The
user comes from Cline/Freebuff/OpenCode which show more visible reasoning,
planning phases, clarifying questions, decision explanations, and progress
check-ins.

The user wants Clawde to **reason deeply and produce better final answers**,
auto-detect when a task needs more reasoning depth, show raw thinking as it
streams, ask questions on medium ambiguity/risk (not for obvious work, but not
steamroll real uncertainty), narrate progress/check for loops/rabbit holes/
divergence from the original prompt, and explain decisions naturally in the
response. Must work in free mode (no paid keys). Same conversational interaction
flow — better answers, not a new UI paradigm.

---

## 2. How Clawde works today (verified against code)

### 2.1 Thinking control stack

- **`EffortLevel`** (`crates/core/src/effort.rs`): canonical enum —
  `None`/`Minimal`/`Low`/`Medium`/`High`/`XHigh`/`Max`/`Ultracode`. Ordered
  ascending; `Medium` is default. Each level carries a thinking budget
  (`thinking_budget_tokens()`) and an optional temperature override.
  - `None`/`Low` → no thinking
  - `Minimal` → 1,024 tokens
  - `Medium` → 5,000
  - `High` → 10,000
  - `XHigh` → 16,000
  - `Max`/`Ultracode` → 20,000
  - Note the non-monotonicity: **Low disables thinking, Minimal enables
    1,024-token thinking** — a real source of "effort ignored" confusion
    (documented in the thinking inspector spec).

  **Budget values — now grounded in the provider's own docs (this closes the
  earlier "unconfirmed" gap).** The ladder is internally consistent and maps
  cleanly onto Anthropic's published extended-thinking guidance
  (platform.claude.com/docs/en/build-with-claude/extended-thinking, 2026):
  - `budget_tokens` must be **at least 1,024** and **less than `max_tokens`**; the
    budget is a *target*, not a strict cap (the model may stop reasoning well
    before it is exhausted), and `max_tokens` remains the hard ceiling on total
    output.
  - Anthropic's own tuning rule: **"For simple tasks, start near the 1,024-token
    minimum and increase incrementally… For complex tasks, start with a larger
    budget of 16,000 tokens or more."** That validates the shape of Clawde's
    ladder — `Minimal`=1,024 sits exactly on the documented minimum, and
    `XHigh`=16,000 lands exactly on the documented complex-task floor.
  - Budgets above **32k** need batch processing (long-running requests hit system
    timeouts and open-connection limits) — Clawde's 20,000 ceiling is safely
    below that threshold, so the high rungs are not in the risky zone.
  - The ladder's top three rungs (XHigh=16k, Max=20k, Ultracode=20k) still collapse
    to the same `reasoning_effort: high` on OpenAI-family hosts
    (`effort_shaping.rs` L25); they differ only on providers with a native numeric
    thinking budget (Gemini).
  - Anthropic's newer **effort** control (`output_config.effort` ∈
    `low|medium|high|xhigh|max`) is now the recommended way to steer depth, and
    its per-model guidance is that **`high` is the default and `xhigh` is the
    recommended starting point for coding and agentic work** — "Complex reasoning,
    difficult coding problems, agentic tasks" = `high`; "Long-running agentic and
    coding tasks (over 30 minutes)" = `xhigh`
    (platform.claude.com/docs/en/build-with-claude/effort, 2026). That is a direct
    statement that a *coding* workload should sit at high–xhigh, i.e. Clawde's
    `High`–`XHigh` (10k–16k), **not** the `Medium`=5k default. It is the external
    justification for §6.2's auto-raise: the ladder numbers are right, but the
    *default rung* for coding work is too low.
  - Effort applies to **all** output tokens (text, tool calls, thinking), and at
    lower effort levels the model still thinks on sufficiently hard inputs — it
    just thinks less. So raising effort is a genuine behavioral signal, not merely
    a thinking-token cap.

  **Cache constraint the implementation must respect (new, from the same docs).**
  Anthropic documents that **changing `budget_tokens` between requests invalidates
  prompt-cache breakpoints** — "pick a budget and hold it stable for the life of a
  cached conversation." On models that support **per-message
  `output_config.effort`** (Claude Fable 5.1, Opus 5.5/5), changing effort
  mid-conversation **preserves** the cache. Implication for §6.2: on manual-budget
  providers, a per-turn auto-raise that changes `budget_tokens` every turn can cost
  cache hits — make the auto-raise **sticky per task** (raise once when the task is
  classified, hold it for the task) rather than re-decided each turn, and prefer a
  per-message effort channel where the provider supports one.

  **Recommendation:** keep the current values as the user-facing ladder — the
  spacing now matches Anthropic's own simple/complex guidance (1,024 min → 16,000+
  for complex), and the 32k batch caveat is respected. Use the §6.2 auto-raise
  floor to make coding turns land at `High`/`XHigh` by default rather than changing
  the ladder. The one structural oddity worth fixing is the `Low`/`Minimal`
  non-monotonicity (L14-15) — `Low` disables thinking while `Minimal` enables
  1,024 tokens — a real "effort ignored" confusion source that should be documented
  prominently in the thinking inspector; flattening it is a separate decision
  touching the Anthropic API contract (the API rejects budgets below 1,024), so
  don't do it casually.

  **Per-upstream values: what each provider actually documents (checked first-hand,
  2026-10-10).** The ladder above is Anthropic-shaped, but only two upstreams accept
  a *numeric* budget; every other provider takes a **named level**, and Clawde
  already maps the ladder to those names in `shape_provider_thinking()`
  (`crates/api/src/providers/effort_shaping.rs`). Verified against each vendor's own
  docs:

  | Upstream | Documented control | Documented values | Clawde today |
  |---|---|---|---|
  | Anthropic | `budget_tokens` (newer: `output_config.effort`) | min 1,024, `< max_tokens`, soft target; effort `low\|medium\|high\|xhigh\|max` | numeric ladder 1,024→20,000 ✔ |
  | OpenAI (GPT-5/6, O-series) | `reasoning.effort` | `none\|minimal\|low\|medium\|high\|xhigh\|max`, **model-dependent** | `none\|minimal\|low\|medium\|high`; XHigh/Max/Ultracode collapse to `high` |
  | Google Gemini 2.5 | `thinkingBudget` | a *soft limit*; `0` disables; the doc's own example is 1,024 | numeric ladder, clamped `< max_tokens`; `0` when off ✔ |
  | Google Gemini 3.x | `thinkingLevel` | `MINIMAL\|LOW\|MEDIUM\|HIGH` (per-model subset; defaults differ by model) | `minimal`…`high` ✔ (shared shaper) |
  | DeepSeek | `thinking.type` + `reasoning_effort` | effort `low\|high\|max`; **thinking on by default, default effort `high`** | `disabled` for None/Low, else `high`/`max` ✔ |
  | Qwen (DashScope) | `enable_thinking` (via `extra_body`) | boolean; some models also take `reasoning_effort` | `true` only when a budget was requested ✔ |
  | Z.AI / Zhipu | `thinking.type` (+ `reasoning_effort` on GLM-5.2+) | `enabled\|disabled`; effort `max\|high\|low` | `enabled\|disabled`; **never sends `reasoning_effort`** |
  | Poolside | `chat_template_kwargs.enable_thinking` | binary, **off only** (thinking is on by default) | off only for `EffortLevel::None` ✔ |

  Four findings that sharpen the sections below:

  1. **The vendor defaults are already ~`High` for coding.** OpenAI's effort table
     lists `medium` as the default "for most workloads" with **agentic coding** as
     its use case, and `high` as "Recommended for complex workflows and agentic
     tasks"; Google's Gemini 3 table gives `HIGH` as "Best for complex prompts
     requiring deep reasoning, multi-step problem solving, **formal code
     verification, or multi-turn tool execution**"; DeepSeek's thinking mode is on
     by default at effort `high`; Anthropic recommends `high`–`xhigh` for coding.
     Clawde's default is `Medium` (5,000). §6.2's auto-raise is therefore justified
     by four vendors at once, not by Anthropic alone.
  2. **`none` is not universally legal.** OpenAI documents that **GPT-6 Astra
     returns HTTP 400 for `reasoning.effort: none`**, and GPT-6.1 Sol rejects
     `none`/`minimal` and defaults to `medium`. Clawde maps `EffortLevel::None` to
     the literal string `"none"` (`openai_reasoning_effort_for_level`). The shared
     shaper's generic arm is model-gated (`openai_reasoning_model` matches
     `gpt-5*`/`o1`/`o3`/`o4`, so a `gpt-6*` id does not reach it today), but the
     Codex/Copilot **Responses** arms forward `reasoningEffort` from provider
     options without a model gate (`crates/api/src/providers/codex.rs`), and those
     are the paths where a user-set `none` could reach a model that 400s on it.
     Verify the gate at implementation; the correct shape is the model-gating the
     DeepSeek arm already has.
  3. **Gemini 3 rejects the old numeric knob — and the direct Google path still
     sends it.** Google documents `thinking_budget` as **deprecated for Gemini 3 and
     later** ("can cause intermittent 400: INVALID_ARGUMENT errors"), and sending
     both `thinking_level` and `thinking_budget` errors outright. The shared shaper's
     split is right (`2.5` → budget, `3.` → level), but the direct provider path does
     not share it: `google.rs` inserts `thinkingConfig.thinkingBudget` whenever
     `Self::supports_thinking(model)` is true, and that helper matches `2.5`, `3.0`,
     `3.1`, and `gemini-3` (`crates/api/src/providers/google.rs`). So a direct
     `google/gemini-3-*` request that carries a thinking budget sends a parameter
     Google documents as an error risk. Fix: route the direct path through
     `shape_provider_thinking`, or make `supports_thinking` version-aware. This is a
     concrete defect surfaced by this research, not a design question.
  4. **Z.AI errors on disable for its newest models.** GLM-5.3 and GLM-5.3-FLASH no
     longer support `thinking.type: disabled` ("an error will occur if the
     thinking.type parameter in the API request is set to disabled"); GLM-5.2 and
     earlier accept it. Clawde writes `disabled` for `EffortLevel::None` on every
     `zai`/`zhipu` model. Also currently unused: GLM-5.2+'s `reasoning_effort`
     (`max` default and recommended, `high`, `low`), whose vendor mapping
     (`xhigh`→`max`, `low`/`medium`→`high`, `none`/`minimal` stop thinking) is the
     same collapse Clawde already implements for DeepSeek.

  **Implication for the ladder:** keep the numbers as the Anthropic + Gemini-2.5
  ladder, but stop describing them as a cross-provider scale — the numbers only ever
  reach those two. The per-provider thing that matters is the *name* in the table
  above, so any new provider arm must land in `shape_provider_thinking` and in this
  table in the same change.

- **`shape_provider_thinking()`**
  (`crates/api/src/providers/effort_shaping.rs`): maps an
  `(provider_id, model_id, effort_level, thinking_budget, max_tokens)` tuple to
  provider-native wire params. Per-provider arms:
  - **Gemini 2.5**: `thinkingConfig.thinkingBudget` + `includeThoughts` (budget
    clamped below `max_tokens − 1`)
  - **Gemini 3.x**: `thinkingConfig.thinkingLevel` (`minimal`…`high`) +
    `includeThoughts`
  - **DeepSeek** (model-gated, any OpenAI-compatible host): `thinking.type:
    enabled|disabled` + `reasoningEffort: high|max`
  - **Qwen (DashScope provider)**: `enable_thinking: true` only when a budget
    was requested
  - **Z.AI/Zhipu**: `thinking.type: enabled|disabled` + `clear_thinking`
  - **Poolside**: `chat_template_kwargs.enable_thinking: false` only when
    effort is `None` (thinking on by default, consumes from `max_tokens`)
  - **OpenAI reasoning families** (GPT-5/O-series/Qwen3 on OpenAI-compatible
    hosts): `reasoningEffort: none|minimal|low|medium|high`
  - Models without a thinking knob (Llama, gpt-oss, etc.) get no thinking
    params — the level only tunes temperature/prompt behavior

- **Query loop application** (`crates/query/src/lib.rs`): per-turn
  `effective_thinking_budget` = explicit `thinking_budget` (highest) OR
  `effective_effort_level.thinking_budget_tokens()`. When a budget exists,
  `ThinkingConfig::enabled(budget)` is set on the request.

- **Effort resolution (highest wins)**:
  `session override (/effort, ing on|off)` > `CLI --effort` >
  `settings.json config.defaultEffort` > `provider/model default`

- **Free-mode shaping**: The FreeProvider re-shapes the same request per chain
  entry at dispatch time via `shape_thinking_for_upstream`, so `/effort high`
  correctly turns on Gemini thinking when the chain lands on
  `free/google/...` and emits `reasoning_effort` for DeepSeek/Qwen3 elsewhere.
  Each upstream only sees params its model family accepts.

### 2.2 Thinking content capture and display

- Thinking deltas arrive as `StreamEvent::ThinkingDelta { thinking: delta }`
  and are accumulated into `ContentBlock::Thinking { thinking, thought_signature }`.
- The TUI renders thinking as collapsible blocks. `Ctrl+O` toggles expand/collapse
  all thinking blocks. `Alt+H`/`Alt+L` step reasoning effort down/up. `Alt+E`
  opens the effort/reasoning picker.
- The `-back` / `back-play` commands show replay previous thinking traces.
- Thinking content is **not** rendered by default in all surfaces — the user is
  unsure whether they currently see thinking blocks. This is itself part of the
  problem: if thinking is happening but invisible, the agent looks abrupt.

### 2.3 What the codebase already knows about reasoning gaps

From `docsing-inspector-spec.md` and the effort shaping tests, the following
known failure modes exist:

| Symptom | Root cause |
|---|---|
| "Low disables thinking while Minimal enables 1024-token budget" | Non-monotonic effort ladder rungs |
| "`reasoningEffort: none` is minimum thinking, not off" | OpenAI-family `none` is the minimum tier, not disable |
| "Gemini `thinkingBudget` sometimes ignored" | Google platform quirk; reasoning token count is ground truth |
| "Poolside thinking eats the output budget" | `reasoning_tokens: 501, completion_tokens: 0` — thinking consumed the entire `max_tokens` |
| "Model thinks forever, never edits files" (local models) | Missing `--jinja` or template doesn't support tool calls; model too weak at agentic tool use |
| "Thinking-only completion" (free-tier flakes) | Reasoning model spends whole reply in thinking block: zero `text_delta`, 0 output tokens, thinking-only assistant message |
| Cascade drift | Weaker model's thinking contaminates next turn's stronger model (stripped on upstream change, one-turn lag accepted) |

### 2.4 Decision machinery (audit findings)

- **`decide.rs` is dead code** — `decide_mode`, `decide_verify`, etc. have zero
  production callers. Only their own unit tests reference them. The real
  enforcement points are the spec-mode write gate (`plan_gate_error`),
  `verify.rs`, and the permission classifiers.
- **The query loop IS the orchestrator** — `run_query_loop` owns turn/stop/retry/
  compaction/cap discipline. It injects system prompts, executes tool calls, and
  applies effort/thinking shaping. It already has:
  - Task context injection (`task_context`)
  - Goal loop with re-anchoring (`GOAL_REANCHOR_INTERVAL = 6` tool calls)
  - Step budget warnings + degradation turn
  - Prose-tool-intent detection + nudging
  - Semantic verify/fix runners (opt-in)
  - Continuation policies (StopPolicy, GoalPolicy)
- **`AskUserQuestionTool` exists** (`crates/tools/src/ask_user.rs`): interactive-only,
  suspends the query loop until the TUI answers, returns an error headless. Already
  in the default tool set. A full TUI dialog exists (`crates/tui/src/ask_user_dialog.rs`).
- **System prompt already instructs asking**: `crates/core/src/system_prompt.rs`
  tells the model "hard to reverse, ask one clarifying question (AskUserQuestion)
  before acting" and has per-tool guidance.
- **`ultracode` keyword** (`crates/core/src/effort.rs` +
  `crates/core/src/keywords.rs`): triggers `EffortLevel::Ultracode` + injects
  `ULTRACODE_PROCEDURE` as a per-turn system-prompt addendum. The procedure is a
  plan→classify→delegate→integrate→verify workflow. The user has **not tried it**.
- **Task classification for routing exists**: `/routing task` classifies requests
  as `coding`, `reasoning`, `creative`, `fast`, `verification`, `simple_edit`,
  `search` and tries upstreams best suited to each. The classifier is in
  `crates/core/src/task_classifier.rs` (referenced via `free/impls.rs`).

### 2.5 What is NOT built

- No **planning phase** that pauses before coding to present a plan for approval.
- No **interview-style confirmation** for Plan Mode (the user specifically wants
  this — "Plan Mode should have more of an interview or set of questions to
  confirm before working").
- No **loop-level progress detection** for loops/rabbit holes/divergence — the
  model is instructed but not monitored; no guard detects "stuck repeating the
  same thing" or "drifted from the original prompt."
- No **adaptive reasoning depth** — effort is set explicitly or by default, not
  auto-classified per task.
- No **structured decision explanation** in responses — the model may explain
  naturally but there's no prompt guidance or output structure enforcing it.
- The **thinking inspector** (`docsing-inspector-spec.md`) is a spec for a
  diagnostic UI ("why is my effort being ignored") but may not be built yet — it
  is read-only introspection, not a fix.

---

## 3. Locked decisions (from interview)

| # | Question | Decision |
|---|---|---|
| D1 | Primary problem | Thinking is shallow when enabled; answers lack depth, alternatives, edge-case consideration, and explanation. Thinking may also be invisible to the user. |
| D2 | Task scope | All task types (code changes, debugging, design, review) equally. |
| D3 | What "good reasoning" means | Multiple: visible thinking before acting, asks clarifying questions on ambiguity, checks its own work, explains decisions in the response, takes more budget on hard problems. |
| D4 | Cost/speed preference | **Depth over speed.** Token cost is acceptable; the user wants thorough reasoning. |
| D5 | Auto vs manual depth | **Auto-detect** when to think harder, but expose the control. The user should not have to think about thinking for routine work. |
| D6 | Thinking display | Show raw thinking as it streams (live thinking blocks). |
| D7 | Asking behavior | Ask on **medium ambiguity + medium risk**. Don't stop for simple/obvious work. Don't steamroll real uncertainty. |
| D8 | Check-in behavior | Detect loops, non-progress, inaction, rabbit holes, and divergence from the original prompt. Narrate progress. |
| D9 | Explanation style | Natural language woven into the response (not a separate structured section), but only when there are real trade-offs — not obvious choices. |
| D10 | Scope of change | **Redesign the reasoning pipeline more broadly** — not just tuning. But must preserve the same conversational flow (no new UI paradigm for the user). |
| D11 | Ultracode | User has not tried it. It exists and is relevant; the spec should account for it (build on it, improve it, or replace parts of it). |
| D12 | Mode constraint | **Must work in free mode** — no dependence on a specific paid provider's thinking features. Free-mode routing across upstreams is the primary environment. |
| D13 | Safety constraint | Don't break existing behavior. Changes are additive/improvements. |
| D14 | Top priority | **Reasoning deeply AND better final answers** — these two are inseparable; better final answers are the proof that reasoning improved. |

---

## 4. Problem statement

### 4.1 Primary problem

Clawde's reasoning is shallow. When thinking is enabled (default `Medium`, 5,000
tokens), the visible thinking content and/or the final answer does not demonstrate
genuine reasoning: weighing alternatives, identifying risks, catching edge cases,
planning before acting, or explaining decisions. The user cannot tell whether
Clawde is thinking at all, and when they can see thinking, it doesn't seem to
produce better outcomes.

### 4.2 Contributing factors (hypothesized from code + interview)

1. **Default effort may be too low for complex work.** `Medium` = 5,000 thinking
   tokens may be insufficient for multi-file refactors, architectural decisions,
   or hard debugging. The user never changes it (leaves it on default).
2. **No auto-classification of task difficulty.** Effort is static per session;
   the loop does not raise reasoning budget when the task is complex/hard/risky.
3. **Prompt guidance for reasoning quality may be weak.** The system prompt may
   not sufficiently encourage the model to reason before acting, consider
   alternatives, check edge cases, or explain decisions.
4. **Thinking visibility is inconsistent.** The user is unsure whether thinking
   blocks are visible; if thinking happens but isn't seen, the agent looks abrupt
   and non-reasoning.
5. **No planning/interview phase before acting on complex tasks.** The user wants
   Plan Mode to have an interview/confimation step; this doesn't exist.
6. **No loop-level guards against degeneration.** Loops, rabbit holes, non-progress,
   and divergence from the original prompt are not detected — the model can waste
   turns without the system noticing.
7. **Free-tier model variance.** Different free upstreams have wildly different
   reasoning quality and thinking fidelity. The router may not steer reasoning-heavy
   tasks to the better reasoners, and thinking-only completions / budget-eating
   thinking are known flakes.

### 4.3 What "better" looks like (acceptance criteria, qualitative)

- On a complex task, the thinking block lays out the problem, considers at least
  one alternative approach, identifies risks/edge cases, and plans before coding.
- On an ambiguous task (medium ambiguity/risk), Clawde asks a clarifying question
  instead of making a wrong assumption and acting on it.
- The final response explains non-obvious decisions in natural language
  ("I considered X but chose Y because...").
- On long/multi-step tasks, Clawde narrates progress and/or flags when it detects
  a loop, stall, rabbit hole, or drift from the original prompt.
- The user can tell (by seeing thinking or by the quality of the answer) that
  Clawde reasoned before acting.
- None of this depends on a paid provider; it works through free-mode routing.

**Measurement note (implement, not scope-change):** §4.3 criteria are qualitative. The
implementation should pair any "did reasoning improve" claim with a small pre/post task set
scored per-case, split by whether the baseline was already correct — the paired-scoring
discipline from the §12 reflection literature (pre-vs-post delta, over-correction rate) — so
that "more reasoned answers" can't hide regressions on already-good cases. See §12 tertiary
sources.

---

## 5. Proposed approach

### 5.1 Guiding principle

**Improve reasoning quality at the source (prompts + effort shaping + loop
discipline) rather than building a new interaction paradigm.** The user wants the
same conversational flow with better answers. The main levers, in priority order:

1. **Prompt guidance** — make the system prompt explicitly require reasoning
   behavior (plan before code, consider alternatives, flag ambiguity, explain
   non-obvious decisions, self-check).
2. **Adaptive effort** — auto-raise reasoning depth when the task warrants it,
   using task classification + complexity signals + user cues. Keep explicit
   override available.
3. **Thinking visibility** — ensure thinking is visible by default in the TUI
   (or at least clearly discoverable), so the user sees the reasoning happening.
4. **Planning/interview for Plan Mode** — add an interview/confirmation step when
   Plan Mode is active, before work begins.
5. **Loop-level progress/degneration guards** — detect loops, stalls, rabbit holes,
   and divergence; narrate progress; surface when the agent is stuck or off-track.
6. **Free-tier steering** — route reasoning-heavy tasks toward upstreams/models
   that actually reason well, and handle thinking-only completions / budget-eating
   thinking more gracefully.

### 5.2 What NOT to build

- A new UI paradigm or a separate "reasoning mode" screen. Same flow.
- A hard loop pause that blocks for user input on every turn (over-prompting
  fatigue is a known hazard — the interview confirmed the user wants asks only on
  medium ambiguity/risk).
- Dependence on any specific paid provider's thinking features.

---

## 6. Detailed design

### 6.1 Prompt guidance for reasoning behavior

**Goal:** make the model actually reason, visibly, and explain its decisions.

**Mechanism:** inject a reasoning-guidance block into the system prompt (in addition
to existing output-style and mode guidance). This is model-disciplined, not
enforced — the model follows the instruction.

**The guidance should instruct the model to, on every turn:**

1. **Before acting on a non-trivial task, think through:**
   - What is the actual goal / what does "done" look like?
   - What are 1–2 alternative approaches and why pick this one?
   - What are the risks, edge cases, and things that could go wrong?
   - What existing code patterns/conventions should I follow?

2. **When the task is ambiguous (medium+ ambiguity or risk), ask** via
   `AskUserQuestion` rather than guessing — but **don't ask for obvious/trivial
   choices.** Distinguish:
   - Mechanical/trivial: just do it.
   - Real uncertainty that affects the approach: ask.

3. **Explain non-obvious decisions in the final response** in natural language:
   "I considered X but chose Y because..." — but only when there were real
   trade-offs, not for obvious mechanical steps.

4. **Self-check before presenting the answer:**
   - Does this actually solve the stated problem?
   - Are there edge cases or failure modes?
   - Did I follow existing conventions?

**What counts as "non-trivial" (trigger for step 1's full reasoning) vs
"trivial" (just do it):** reuse the existing signals already in the codebase —
the task classifier in `crates/api/src/providers/free/task_classifier.rs` (7
`TaskType` variants: `CodeGeneration`, `CodeEdit`, `Reasoning`, `Planning`,
`Verification`, `SimpleEdit`, `Search`) and the plan-gate logic in
`crates/query/src/decide.rs` (`decide_mode`: multi-file code change OR single
code file > `PLAN_LINE_THRESHOLD = 30` lines = Plan). A task is **non-trivial**
when any of: it is classified `Reasoning`/`Planning`/`CodeEdit`/`CodeGeneration`
with multi-file or >30-line scope; it touches code the model has not yet read
(needs context-gathering first); it is a debugging/architecture question. A task
is **trivial** (skip the full ceremony) when it is a single-file read-only query,
a simple edit under threshold (`SimpleEdit`), formatting, grep/find (`Search`),
or an obvious mechanical fix. This definition is deliberately tied to existing
classifier + plan-gate signals so the §6.1 guidance and the §6.2 auto-raise
classifier share one notion of task difficulty — do not invent a second
classifier.

**Medium+ ambiguity (trigger for step 2's ask):** requirements conflict or are
underspecified in a way that changes the implementation; the change is wide-reaching
or not cleanly reversible; multiple plausible designs exist with materially different
tradeoffs. This is the same definition used in the modes spec's ask-on-ambiguity
trigger (§7.3 of `docs/plans/clawde-modes-ux-spec.md`) — keep them aligned.

**This guidance should be tunable per mode/preset** (see §6.2) — a "careful"
preset strengthens it, a "fast" preset weakens it. Today's `medium` default
should already include a baseline version of this guidance.

**Interaction with existing machinery:**
- This complements (not replaces) the existing `AskUserQuestion` tool and the
  existing system-prompt guidance at `system_prompt.rs:630`.
- This complements (not replaces) `ultracode` — ultracode is the top-tier version
  of this with delegation/verification workflow added.
- Step 4's self-check is a *prompt-instruction* gate (one round, shares the turn's
  thinking budget), NOT a `VerifyPolicy` call. `VerifyPolicy` (see below) handles
  command execution; the self-check handles "did I solve the right problem" — those
  are different questions and must not be conflated.

### 6.2 Adaptive reasoning depth (auto-detect when to think harder)

**Goal:** Clawde invests more reasoning on hard tasks without the user configuring
effort, while keeping explicit override available.

**Mechanism:** a **reasoning-depth classifier** that runs before each turn (or at
task start) and picks an effort level above the user's default when signals
indicate the task is complex/risky. The user's explicit setting (CLI `--effort`,
`/effort`, `ing`, `ultracode` keyword, `defaultEffort`) always wins over the
auto-raise. The auto-raise is a floor boost, not an override.

**Signals to use (all five the user named are in scope):**

1. **Task type / intent classification** — reuse or extend the existing task
   classifier (`/routing task` machinery). "reasoning", "planning", "debugging",
   "architecture/design" tasks get a higher floor. "simple_edit", "fast",
   "search" stay at or below default.
2. **Complexity of the request itself** — prompt length, number of distinct
   concepts/files mentioned, specificity. Longer, multi-concept prompts → higher
   floor.
3. **Whether the task involves writing code vs understanding** — code-writing
   tasks (especially multi-file) get higher reasoning floor than pure Q&A.
4. **Past failures / uncertainty signals** — if the current run has a failed
   verify round, a corrected correction, or the model expressed uncertainty, raise
   the floor on the retry.
5. **User signals** — `ultracode` keyword forces top tier (existing behavior).
   `/effort`, `--effort`, `defaultEffort`, `ing` are explicit overrides that win.

**Delivery:** the auto-raised level is injected as the effective effort for the
turn (via `QueryConfig.effort_level` / `thinking_budget`), same path as today's
effort application. The TUI should show **what level was picked and whether it
was auto-raised** (so the user understands why a turn got more/less reasoning).

**Boundaries:**
- Auto-raise is bounded: it can raise toward `High`/`XHigh` on strong signals but
  should not silently push to `Ultracode` (that's an explicit-user-intent tier with
  the delegation workflow). `Ultracode` stays keyword/CLI-gated.
- Auto-raise does not lower below the user's explicit/default level — it only
  raises.
- **Auto-raise must be cache-safe (see §2.1).** Raise once per task and hold the
  effective effort sticky for that task, rather than re-deciding every turn: on
  providers whose depth is a `budget_tokens` value, changing it mid-conversation
  invalidates prompt-cache breakpoints. Where the provider exposes a per-message
  effort channel, prefer that (it preserves the cache). On free-tier
  OpenAI-compatible arms, which use a discrete `reasoning_effort` rather than a
  numeric budget, per-request changes are already cache-neutral.
- On free mode, the raise must be safe across the cascade: raising effort only
  helps upstream models that support thinking; models without a thinking knob get
  the temperature/prompt-behavior effect only (existing behavior, no change).
- **Budget the raise against the upstream's output cap.** OpenAI's reasoning guide
  recommends reserving "at least 25,000 tokens for reasoning and outputs", and
  Google's Gemini 3 table makes `HIGH` the level for "formal code verification, or
  multi-turn tool execution" — but several free upstreams cap `max_tokens` far below
  that (`max_tokens_cap` in `crates/api/src/providers/free/catalog.rs`, e.g. Poolside
  8,192). That is precisely the "Poolside thinking eats the output budget" failure in
  §2.3 (`reasoning_tokens: 501, completion_tokens: 0`): raising effort on a small-cap
  upstream converts reasoning into an empty reply. So the auto-raise must be
  conditional on the upstream that will serve the turn — and where the cap bites,
  prefer routing the turn to a larger-cap upstream (§6.6) over raising effort in
  place.
- **Shipped (Phase 5):** the §6.6 routing hint now rides the free chain's
  plan build (`FreeProvider::attempt_plan_inner`, `Route::Auto`): a raised turn
  (effort level with a thinking budget) stably orders upstreams whose
  `max_tokens_cap` can hold the level's thinking budget plus the answer reserve
  (and that support thinking) ahead of those that cannot. It is ordering only —
  it never drops a lane and never blocks fallback. The raise itself is still
  bounded by the budget the request asks for (not the serving upstream's cap):
  bounding it by the smallest `max_tokens_cap` in `FREE_CATALOG` (8_192) made
  every rung unreachable (the cheapest needs 12_048), i.e. the raise never fired
  on the default provider at all. The dispatch-time clamps (`max_tokens`, and
  Gemini's numeric `thinkingBudget`) still keep a raised level inside what a
  small-cap upstream can hold.

### 6.3 Thinking visibility (ensure the user sees reasoning)

**Goal:** the user should not have to guess whether thinking is happening. If
thinking is on, the user should see it or have an obvious way to see it.

**Mechanism:**
- In the TUI, thinking blocks should be **visible by default** (not collapsed)
  when they're short, and collapsible when long. Today's behavior (collapsed by
  default?) should be audited and adjusted.
- The status line / footer should indicate when the model is thinking (a "thinking…"
  indicator with the current effort level), so even before content arrives the user
  knows reasoning is underway.
- The effort indicator in the status bar should show the **effective** effort
  including any auto-raise, with a hint when it was auto-raised.

**This is largely plumbing + prompt-assembly + render defaults** — low risk,
high visibility impact. The `toggleThinkingExpand` binding and `Ctrl+O` still let
the user collapse if they want.

### 6.4 Plan Mode interview/confirmation step

**Goal:** when Plan Mode is active, before Clawde starts working, it interviews
the user to confirm the approach — especially for medium+ ambiguity/risk tasks.

**Mechanism:**
- When Plan Mode is entered (`/plan` or permission mode = Plan), and the task is
  non-trivial, Clawde enters an **interview phase** before any file editing:
  1. It reads the relevant context (files, existing patterns).
  2. It presents a short plan: goal, approach, key files/touches, risks, open
     questions.
  3. It asks clarifying questions for any medium-ambiguity items.
  4. The user confirms/adjusts. Only then does Clawde exit Plan Mode (or switch
     to execute) and start editing.
- This uses the existing `AskUserQuestionTool` and the existing Plan permission
  mode. It is an **orchestration-layer behavior** layered on top of existing
  primitives, not a new engine.

**The user's specific ask:** "Plan Mode should have more of an interview or set
of questions to confirm before working." This is the concrete feature that
addresses it.

**Scope guard:** this is for Plan Mode specifically. Normal (non-Plan) sessions
still get the prompt-guidance version (§6.1) — ask when ambiguous, but don't
require a formal interview before every task.

### 6.5 Loop-level progress / degeneration guards

**Goal:** detect when the agent is stuck, looping, drifting, or making no progress,
and either narrate or surface to the user rather than wasting turns silently.

**What already exists (method components, used below — do not rebuild):**
the loop already has `GOAL_REANCHOR_INTERVAL = 6` tool calls (`lib.rs:746`,
restates the goal so drift is detectable), `tool_result_budget = 50_000`
(`lib.rs:319`, caps tool-result size in the trajectory), `BUDGET_WARNING_TURNS = 2`
(`lib.rs:694`, warns the model 2 turns before the step budget runs out),
`PROSE_TOOL_NUDGE_CAP = 2` (`lib.rs:731`, bounds prose-tool nudges), the
`repeat_guard` module (`crates/query/src/repeat_guard.rs`) and the
`correction_detector` module (`crates/query/src/correction_detector.rs`). The
degeneration guards in this section compose those existing pieces into a single
detector with explicit thresholds — they do not introduce new telemetry.

**Mechanism (loop-level, in `run_query_loop` or a continuation-policy adjunct):**

1. **Stall detection:** if a turn produces negligible output tokens AND no tool
   calls AND no file writes for N consecutive turns, flag "appears stuck" and
   either narrate a progress check or surface to the user.

   **Default threshold: the shipped no-progress constants** — a turn counts as
   negligible when `turn_output_tokens < NO_PROGRESS_TOKEN_THRESHOLD` (**500**; see
   the grounding table below, which is also where the measured per-turn token
   distribution lives) AND `tool_calls == 0` AND no file writes that turn, and the
   streak bar is `NO_PROGRESS_STALL_TURNS` (**3**). These signals are
   already observable today (`TurnEndContext::turn_output_tokens`,
   `TurnEndContext::turn_made_writes`, tool-call counts in the trajectory) — no new
   telemetry needed. The 3-turn streak is consistent with the existing loop's
   step-budget warning cadence (`BUDGET_WARNING_TURNS = 2` at
   `crates/query/src/lib.rs:694`, which warns at 2 turns remaining) — stall fires
   *after* the step-budget warning has had time to land the work, not before.
   Narrate-only at first; surface to user on the 4th consecutive stall turn.

2. **Loop detection:** if the same tool-call pattern or the same error recurs
   across turns without resolution, flag a possible loop and either change approach
   or ask.

   **Default threshold: same `(tool_name, args_digest)` appearing 3+ times with
   no intervening state change** — where "state change" means at least one file
   write OR one tool result whose content differs OR one new file touched between
   the repeated calls. "args_digest" is a hash of the canonicalized JSON args so
   `/tmp/foo` and `/tmp/bar` are different calls but `Bash(command="cargo build")`
   repeated is the same. This is the IAL-Scan lesson applied directly (§9 Q5):
   key on *state change*, not repetition alone — legitimate iteration (re-running
   tests after a real edit) is not a loop; re-running the same bash command 3× with
   no file change between is. Action: narrate + offer to vary approach on the 3rd
   repeat; surface to user if it repeats again. The existing
   `repeat_guard` module (`crates/query/src/repeat_guard.rs`) is the natural home
   for this — confirm it exposes the right hooks, else add the detector alongside
   the existing `correction_detector` (`crates/query/src/correction_detector.rs`).

3. **Divergence detection:** compare the current work (files touched, tool calls,
   stated intent) against the original prompt/task. If the work has drifted
   materially (e.g., started doing something the user didn't ask for), flag it.

   **Default threshold: the shipped complexity ledger's own bars** — divergence
   fires when `COMPLEXITY_THRESHOLDS.scope_expansions` (**2**) is exceeded, or when
   the drift composite crosses its per-session baseline (`DriftWeights` in
   `crates/query/src/drift.rs`: scope expansion **3.0**, repeated failure **2.0**,
   failed tool **1.0**, files touched **0.25**). Note the deliberate weighting:
   files touched is a *weak* drift signal (0.25) while scope expansions and
   repeated failures are strong ones (3.0 / 2.0), so an "N extra files" count alone
   must not trip this guard — which is what an earlier draft's ">2 off-scope files"
   got wrong. "Files touched"
   is already computed by the compaction machinery (`extract_file_operations` in
   `crates/query/src/compact.rs`) and the trajectory tracks tool calls per turn, so
   this is observable from existing state.
   The drift check reuses the existing goal re-anchoring cadence
   (`GOAL_REANCHOR_INTERVAL = 6` tool calls, `lib.rs:746`) as its restatement
   backbone: every 6 tool calls the model restates the goal (already happens), and
   the divergence guard compares that restatement against the original prompt.
   Action: narrate + offer to refocus; do not hard-stop (the model may be doing
   useful adjacent work the user didn't name).

4. **Rabbit hole detection:** if the agent is spending many turns on a sub-problem
   without converging, narrate and offer to refocus or ask the user.

   **Default threshold: the shipped no-progress window** — a signature seen in the
   recent `NO_PROGRESS_WINDOW` (**4**) turns with no advancing write and no
   successful verify round, accumulating to `NO_PROGRESS_STOP_STREAK` (**3**).
   "Same file_or_topic" = same primary file touched or same tool-name
   cluster (e.g. repeated `Bash` invocations that are all `cargo test` on the same
   crate). "Advances the task" = a file write that changes a file in the original
   scope, or a verify pass. This is distinguishable from legitimate iteration
   (each turn changes the tree) by the "no file change / no verify pass" condition.
   Action: narrate + offer to refocus or ask.

**Threshold grounding — shipped constants first, then measured, then external (read
this before implementing).** This block was re-checked against the code and against
Clawde's own transcripts on 2026-10-10; it replaces an earlier version whose numbers
were partly invented.

*Source of truth — constants the loop already ships. Do not invent parallel values:*

| Constant | Value | Where | Role |
|---|---|---|---|
| `NO_PROGRESS_TOKEN_THRESHOLD` | `500` | `crates/query/src/goal_loop.rs` | min output tokens for a goal turn to count as progress |
| `NO_PROGRESS_STALL_TURNS` | `3` | `crates/query/src/goal_loop.rs` | consecutive no-progress goal turns → pause as stalled |
| `NO_PROGRESS_STOP_STREAK` | `3` | `crates/query/src/lib.rs` | consecutive no-progress tool turns → loop-health stop |
| `NO_PROGRESS_WINDOW` | `4` | `crates/query/src/lib.rs` | how many recent no-progress signatures count as a repeat (catches A,B,A,B cycles) |
| `DEFAULT_THRESHOLDS` | `[3, 5, 8]` | `crates/query/src/repeat_guard.rs` | consecutive identical `(tool, canonical args)` reminder rungs |
| `COMPLEXITY_THRESHOLDS` | files 8, tool calls 60, failed tools 4, repeated failures 3, scope expansions 2 | `crates/query/src/task_state.rs` | the drift/complexity ledger the loop already narrates from |
| `DriftWeights` | scope 3.0, repeated failure 2.0, failed tool 1.0, files 0.25 | `crates/query/src/drift.rs` | relative weight of the drift signals |
| `VerifyConfig::default` | `max_retries: 3`, `timeout_secs: 180`, `skip_when_no_writes: true` | `crates/core/src/lib.rs` | the verify gate's own bounds |

*The "negligible output" floor is 500, not 50 — measured.* A scan of this machine's
own transcripts (**2,545 assistant turns across 96 real sessions**,
`~/.clawde/projects/**/*.jsonl`, field `message.cost.output_tokens`) gives p10 = 57,
p25 = 120, median = 252, p75 = 629, p90 = 1,631, max 8,192 output tokens per turn. So a
`< 50` floor would flag only 227 turns (8.9%) — but it is not a *progress* bar: it sits
below the p10 of all turns and an order of magnitude below the bar the loop already
uses. `500` is ~2× the measured median (252), i.e. "substantial output" means well above
the median turn, which is why it is only meaningful ANDed with "no writes": a 250-token
turn that edited a file is progress. Use `500`.

*External sources — what each does and does not support.* Verified first-hand this
session:
- **NousResearch/hermes-agent #41477** specifies the stall detector's **key** —
  "(same action signature: tool name + normalized args hash)" — which is exactly this
  section's `(name, args_digest)`. It does **not** fix N (the threshold in that issue
  is configurable). Cite it for the mechanism, not the number.
- **OpenHands** ships and documents a stuck detector (`stuck_detection=True`,
  docs.openhands.dev "Agent stuck detector"), but the published docs quote no numeric
  constant. Do not cite OpenHands for a number.
- **arXiv:2607.00038** ("Stop Hand-Holding Your Coding Agent: Engineering the Loops
  that Replace Step-by-Step Prompting", Macedo, Jun 2026) is the real source for this
  section's *shape*: it defines the "loop specification" as trigger + goal +
  verification step + **stopping rule** + memory, gives a five-level verification
  ladder, and reports that 74% of the fifty loops it codes name their terminal states.
  It supplies **no token threshold** — do not attribute the 500 to it (an earlier
  draft did, via a code comment in `goal_loop.rs`; the code's attribution should be
  corrected when that file is next touched).
- **CrewAI `max_iter` 20 / `max_retry_limit` 2** and **LangGraph's `recursion_limit`**
  remain useful *context* for bounded iteration, but they are total-iteration and
  error-retry ceilings, not detection windows for this section's guards. Keep them as
  range sanity checks.
- **arXiv:2505.02709** ("Evaluating Goal Drift in Language Model Agents") confirms goal
  drift is real and **gradual/subtle**, which supports a conservative
  narrate-don't-stop divergence guard. Mechanism only; it gives no file-count threshold.
- **Dropped as unverifiable.** An earlier draft cited `nearai/ironclaw #2240` ("warn at
  3 consecutive duplicate failures, stop at 5") and an AWS Strands `DebounceHook`
  write-up (`window_size = 3`) as proof that "three independent implementations converge
  on 3". Re-checking produced no reproducible primary text: the ironclaw issue number
  itself did not reproduce (a search surfaced #6124 instead), and no vendor page, repo
  file, or code snippet carried the quoted counts. The 3s in this section stand on
  Clawde's own shipped constants — an honest basis — so **do not reinstate the
  "industry convergence" claim** without a primary source.

*Still to be re-fit (not invented, not converged).* The divergence and rabbit-hole bars
are the shipped `COMPLEXITY_THRESHOLDS` / `NO_PROGRESS_WINDOW` values reused, which keeps
them cheap because the guards narrate rather than block. Fit them from the eval
harness's own drift metrics — `scripts/eval/drift_ab.py` writes `scope_expansions`,
`repeated_failures_per_target`, `failed_tools`, and `files_touched` per run — rather than
adding new constants.

**Behavior:** these guards produce **narration first** (a progress/rationale
update) and escalate to **user-visible surfacing** (a question or status) only
when the condition is significant. This matches the user's "check in during long
tasks" + "detect loops/rabbit holes/divergence" ask, and avoids over-prompting on
normal work.

**This is model-disciplined + heuristic, and observable-first.** The heuristics
(stall count, repeated-pattern-with-no-state-change detection, divergence scoring) are
approximate; the fallback is narration, not hard blocking. If a heuristic fires wrong,
the cost is a confusing narration, not a blocked turn.

**Shipped (2026-10-11) — where each guard lives now.** Loop detection is
result-aware in `crates/query/src/repeat_guard.rs` (`observe_result`: the identical
`(tool, canonical args)` returning the identical result three times appends a
"possible tool loop" notice to the result; a fourth stops the loop after the batch).
The other three are `crates/query/src/degeneration.rs`, wired into the loop:
`StallTracker` (narrate at `STALL_NARRATE_TURNS` = 3 through a bounded continuation
message, surface to the user at `STALL_SURFACE_TURNS` = 4, capped by
`STALL_NARRATE_CAP`), `DivergenceTracker` (a request-only refocus pin on the goal
re-anchor cadence, primed at loop open so the opening turns are the baseline, and
gated so one crossing narrates once), and `rabbit_hole_due`
(`NO_PROGRESS_STOP_STREAK - 1` = 2: one bounded narration before the shipped stop
owns the turn). All three read observable trajectory state only — never model
self-report.

**Verification-cadence note (measured 2026-10-10 on this repo, which is what this
decision was waiting on).** The existing `VerifyPolicy` (`crates/query/src/verify.rs`)
fires at `end_turn` whenever `turn_made_writes` is true (`VerifyConfig`:
`skip_when_no_writes: true`), and for a Rust project the detected commands are
`cargo test --workspace` (test) plus `cargo clippy --workspace --all-targets --
-D warnings` (lint) — `crates/tools/src/detect_project.rs` — each bounded by
`timeout_secs = 180` with `max_retries = 3` (`crates/core/src/lib.rs`,
`impl Default for VerifyConfig`). Measured here with a warm target dir: **lint
8.25 s, test 249.75 s**. That splits the cadence in two:

- The cheap half is genuinely cheap: ~8 s per write for the lint gate.
- The expensive half does not fit its own budget: 249.75 s > the 180 s per-command
timeout, so on a repo this size the project-wide test command **cannot complete per
write** — it is killed at 180 s and surfaced as a failure, and with `max_retries = 3`
one failing verify can burn up to ~9 minutes of wall clock before the turn ends. The
per-write full-suite rule is therefore only literally true on small repos; on large
ones the cadence is timeout-bounded, not policy-bounded.
- Frequency context (same transcript scan: 394 writes across 130 sessions; only 21
sessions — 16% — wrote at all, median 4 writes per writing session, p90 56, max 116):
the gate is not a per-turn tax for most sessions, but the sessions it hits are the long
ones, where a ~250 s test run per write compounds.

**The reference-agent evidence agrees with that split.** Aider's own docs (aider.chat,
"Linting and testing", 2026): "By default, aider will lint any files which it edits"
(`--no-auto-lint` disables it), while the heavy path is opt-in — "You can configure
aider to run your test suite after each time the AI edits your code using the
`--test-cmd` and `--auto-test` switch." Cheap per-edit lint on by default, per-edit
full-suite test explicitly opted into — which is exactly Clawde's
`lint_edited_files` + `run_checks` split. So: keep the per-write **lint**, and treat
the project-wide **test** command as the tunable — raise `timeout_secs` deliberately,
run the full suite at task end rather than per write, or scope the test command to the
changed crate. The spec's earlier "don't slow the cadence without data" is now backed by
data, and the data says the cheap/expensive split is where to tune, not the general rule.
**Do not** turn §6.1 step 4's self-check into a
second `VerifyPolicy` call — the self-check answers "did I solve the right
problem / did I follow conventions," which is a *model-side reasoning* question
bounded to one round sharing the turn's thinking budget, not a command execution.
Keep them separate: `VerifyPolicy` = did the tests pass; §6.1 step 4 = did I
solve the right problem. The §6.5 degeneration guards re-use `VerifyPolicy`'s
verdict (        a verify pass clears a rabbit-hole signal; a failing verify with
        retries left is a loop candidate) but do not replace it.

    **Prefer external/observable verification over model self-report.** The §12 reflection
literature (self-critique shares the original answer's blind spots; over-correction can
regress already-good answers) and the silent-failures paper (§9 Q5: model self-report
is unreliable — fail-plausible) both argue that degeneration detection and recovery must
key off observable trajectory state (tool calls, tool results, file changes, test
outcomes, token output), **not** off what the model says about its own progress. Where a
task has an external verifier (the existing execute-and-verify loop, a typed checker, a
retrieval check), use it (CRITIC pattern); where it doesn't, be conservative about
triggering any "try correcting yourself" recovery because of over-correction + cost
risk. Any self-correction round, if attempted, must be bounded (one round, external
signal) — not an open-ended Reflexion buffer.

### 6.6 Free-tier steering and thinking failure handling

**Goal:** in free mode, route reasoning-heavy tasks toward better reasoners and
handle thinking-only completions / budget-eating thinking gracefully.

**Mechanism:**
- When the reasoning-depth classifier (§6.2) decides a task needs deep reasoning,
  the routing should prefer upstreams/models with strong reasoning capability when
  multiple are available. This is a **routing hint**, not a guarantee — free mode
  still falls through on failure.
- **Thinking-only completion handling:** when a turn produces a thinking-only
  assistant message (thinking block but zero text/output tokens), treat it as a
  degraded result, not a success. The existing eval harness already scores these
  as `run_error`/unevaluable via `is_empty_completion`; the query loop should
  handle them similarly — retry once with the same or a different model rather than
  presenting an empty answer. This is already partly covered by "empty terminal
  turn retries once" logic; verify and extend if needed.
- **Budget-eating thinking handling:** when `reasoning_tokens ≥ 0.9 × max_tokens`
  and visible content is empty (the "budget eaten" flag from the thinking
  inspector spec), the turn effectively produced nothing. On free tier, retries
  with a higher `max_tokens` or a different model may help; surface to the user
  rather than silently retrying forever.

**Shipped (2026-10-11):**
- **Thinking-only + budget-eaten handling** lives in
  `crates/query/src/thinking_failure.rs` and the query loop's provider path. A
  terminal turn with no visible output and no tool calls is classified from
  observable state (visible content, usage) as thinking-only, budget-eaten
  (`reasoning_tokens ≥ 0.9 × max_tokens`), or empty, and retried **once** with a
  nudge before the loop falls back to its silent-halt placeholder. A
  budget-eaten retry also raises the request's output budget (bounded by
  `BUDGET_EATEN_BOOST_TOKENS` / `MAX_BUDGET_EATEN_BOOST_TOKENS`), and every
  failure is surfaced through a `Status` event — retried or exhausted — rather
  than silently retrying forever. The upstream that served the turn is named in
  the surface text. This also required threading `reasoning_tokens` out of the
  stream's `MessageDelta` usage, which the loop had been dropping (the field
  stayed 0, so the classifier could never have fired).
- **Routing hint** — see §6.2's shipped note; it is the
  `attempt_plan_inner` ordering preference for cap-adequate, thinking-capable
  upstreams on a raised `Route::Auto` turn.

**This section is about resilience on free tier,** where reasoning quality and
thinking fidelity vary most. It complements (not replaces) the deeper-routing work
in the free-provider chain.

---

## 7. Component map (where the work lives)

| Concern | Primary files | Notes |
|---|---|---|
| Effort level + budget | `crates/core/src/effort.rs` | Existing; add clarity around auto-raise semantics |
| Provider thinking shaping | `crates/api/src/providers/effort_shaping.rs` | Existing; verify free-tier shaping is correct for all arms |
| Query loop + request shaping | `crates/query/src/lib.rs` | Apply auto-raised effort; inject reasoning guidance; add degeneration guards |
| System prompt assembly | `crates/core/src/system_prompt.rs` | Add reasoning-guidance block (tunable per mode) |
| Task classification | `crates/core/src/task_classifier.rs` + free routing | Reuse/extend for adaptive depth signals |
| Plan Mode + interview | `crates/commands/` (plan command), `crates/core/src/modes.rs`, `crates/tools/src/ask_user.rs` | Add interview phase before Plan Mode work |
| TUI thinking visibility | `crates/tui/src/render.rs`, `crates/tui/src/effort_picker.rs`, `crates/tui/src/app.rs` | Default-expand short thinking; thinking-in-progress indicator; show auto-raised effort |
| Free-tier steering + failure handling | `crates/api/src/providers/free/impls.rs`, `crates/query/src/lib.rs` (retry) | Thinking-only completion + budget-eaten handling; routing hint for reasoning tasks |
| Thinking inspector (diagnostic) | `docsing-inspector-spec.md` + `crates/api/src/providers/effort_shaping.rs::inspect_thinking` | Read-only diagnostic; supports "why is my effort being ignored" — useful alongside these changes |
| Memory summarization threshold | `crates/query/src/decide.rs:318` (`MEMORY_SUMMARIZE_THRESHOLD = 10_000`) | **Re-fit pending.** Value was fitted 2026-08-14 to n=6 turns of one task (max 1250, p95 1250); the fitted note itself says "re-fit after more per-turn data lands" and that never happened. Re-fit method: pull per-turn `context_tokens_est` from the eval harness (`scripts/eval/run_eval.py` L483 records it from stream events; emitted in `TurnComplete` at `crates/query/src/lib.rs:5314` / `crates/cli/src/main.rs:3478`), build the distribution across a real multi-task series, set threshold at ~8× p99 (the existing note's own logic). Provisional value stays 10_000 until re-fit — at n=6 the observed p99 was 1250, so 10k ≈ 8× p99 and is *probably* still fine, but confirm against real per-turn data before trusting it. The re-fit is a real ticket, not a comment.

---

## 8. Sequencing

### Phase 1 — Prompt guidance + thinking visibility (highest impact, lowest risk)

1. Add a **reasoning-guidance block** to the system prompt (baseline for `medium`
   default; tunable per mode/preset). Covers: think before acting on non-trivial
   tasks, consider alternatives, flag medium+ ambiguity and ask, explain non-obvious
   decisions, self-check.
2. Make **thinking visible by default** in the TUI (short blocks expanded, long
   blocks collapsible; thinking-in-progress indicator; show effective effort
   including auto-raise).
3. Verify the thinking inspector diagnostics are usable so the user can debug
   "why didn't my effort take effect" themselves.

**End of Phase 1, the user should see thinking happening and the thinking should
be more substantive.** This alone may address a large part of the complaint.

### Phase 2 — Adaptive reasoning depth

4. Build the **reasoning-depth classifier** using the five signal groups.
5. Wire it to auto-raise the effective effort (bounded, never below user's
   explicit level, never silently to Ultracode).
6. Show in the TUI when effort was auto-raised and why (briefly).

### Phase 3 — Plan Mode interview

7. Add the **interview/confirmation phase** to Plan Mode: read context, present
   plan + open questions, ask medium-ambiguity questions, wait for confirmation
   before editing.

### Phase 4 — Degeneration guards

8. Add **stall/loop/divergence/rabbit-hole detection** in the loop, with narration
   first and escalation to user-visible surfacing on significant conditions.

### Phase 5 — Free-tier resilience

9. Handle **thinking-only completions** and **budget-eating thinking** more
   gracefully in the query loop (retry once, surface rather than silent failure).
10. Add a **routing hint** so reasoning-heavy tasks prefer stronger free-tier
    reasoners when available.

**Shipped (2026-10-11).** Item 9 is `crates/query/src/thinking_failure.rs` plus
its bounded retry in the loop's provider path; item 10 is the `attempt_plan_inner`
ordering preference for cap-adequate thinking upstreams on a raised `Route::Auto`
turn. See the shipped notes under §6.6 and §6.2.

Phases 1 and 2 are the core of the user's request. Phases 3–5 address specific
sub-asks (Plan Mode interview, check-ins/loop detection, free-tier reliability).

---

## 9. Open questions — resolved by research (arXiv:2604.05164, 2604.14228, 2605.22166, 2607.01641, 2606.14589; Hermes #13663; Atlan ReAct-vs-Plan guide; LLM Agentic Failure Modes taxonomy)

These six questions were flagged as open in the interview. Research into bleeding-edge
agent papers (Apr–Sep 2026) resolves five of them with known best practice; one remains
a user-preference call. Findings below are cited; the spec body in §6 should be updated to
match before implementation.

### Q1: How strong should the default (medium) reasoning guidance be?

**Resolved — research points to a specific calibration.** The Claude Code design-space
paper (arXiv:2604.14228, Liu et al. 2026) reports that Claude Code's core loop is a
"simple while-loop that calls the model, runs tools, and repeats," and that **most of the
code lives in the systems around the loop** — a five-layer compaction pipeline, a permission
system with seven modes, four extensibility mechanisms, subagent delegation, and
append-only session storage. Its guiding principle is **"minimal scaffolding, maximal
operational harness"**: invest in operational infrastructure that lets the model reason
freely rather than in explicit decision scaffolding. That argues for **prompt guidance that
is strong on * WHAT to reason about* (alternatives, risks, edge cases, self-check, explain
non-obvious decisions) but light on * prescriptive structure* (no mandatory XML blocks, no
forced section shapes) — the model should be guided, not constrained.

The Atlasan ReAct-vs-Plan guide (2026) independently warns that ReAct's repeated-reasoning
failure mode is "the agent spends tokens reconsidering decisions that a stable plan could
have fixed once," and that goal drift is the accumulation of locally-useful steps away from
the original objective. This argues for guidance that **encourages a stable plan early and
sticks to it**, not perpetual re-reasoning.

**Recommendation:** baseline guidance should be a concise, instruction-style block (a few
sentences, not a long ceremony) that says: before editing on a non-trivial task, think
through goal/alternatives/risks/edge cases/conventions; flag medium+ ambiguity and ask;
explain non-obvious decisions in the answer; self-check the specific risks/edge cases before presenting (targeted, not a blanket rewrite — see §12 tertiary sources on over-correction risk). Calibrate
empirically using the thinking inspector's last-response telemetry (reasoning_tokens vs
completion_tokens, and whether the final answer actually shows the requested behaviors) —
the same way the TAB paper (arXiv:2604.05164, Jali et al. 2026) uses accuracy-vs-tokens
tradeoff as the optimization signal. Start lighter than you think; the paper evidence is that
over-thinking easy turns wastes budget that harder turns need (TAB shows up to 35% token
savings by allocating less to easy turns).

### Q2: What is the auto-raise ceiling?

**Resolved — there is direct community evidence for the exact ceiling.** The Hermes Agent
feature request #13663 (NousResearch, Apr–Jul 2026, now closed/completed) is precisely this
feature: "smart reasoning_effort routing based on task complexity." It documents the identical
problem (static reasoning_effort is too much for trivial turns, too little for complex ones),
the identical five signal groups the user named (task type, complexity, code-vs-query,
past failures, user cues), and the identical UX requirements (resolve the actual
provider-side effort per request, surface the chosen effort in the footer, keep explicit
override as hard override, don't escalate for simple action commands just because tools are
available). The accepted solution there was **Option B: rule-based smart routing**
(keyword/regex rules mapping task types to effort levels), with Option C (LLM-based
classifier) as a later upgrade — and a commit (Abd0r, Jul 15 2026) references the issue as
addressed.

The mapping the community converged on (Hermes #13663, two independent commenters):

- **low**: simple Q&A, confirmations, single-file patches, formatting, device control
- **medium**: default assistant work, normal planning/summarization
- **high**: coding, debugging, multi-step automation, architecture
- **xhigh**: only explicit "think hard / deep design" requests
- **ultracode/max**: explicit user intent only

**Recommendation:** auto-raise ceiling = **`High` by default, `XHigh` on strong
signals** (multi-file refactor, architecture/design, failed verify round, explicit
ambiguity flagged). **`Ultracode` stays user-intent-gated** (keyword/CLI) — the Claude Code
paper's "human decision authority" value and the Hermes UX constraint both agree that
escalating to the top tier silently is the wrong default. This directly answers Q2: the
ceiling is `XHigh`, not `Ultracode`, and the mapping is the community-standard one above.

The TAB paper reinforces the ceiling choice: adaptive budgets work by **saving tokens on
easy turns to invest in hard ones**, so the ceiling should be high enough to genuinely help
hard turns (High/XHigh) without making every turn expensive. A ceiling of `High` (10,000
thinking tokens) for strong signals is a reasonable default; `XHigh` (16,000) when multiple
strong signals concur.

### Q3: How does auto-raise interact with free-tier model variance?

**Resolved — the Claude Code paper and the failure-modes taxonomy both address this.** The
Claude Code design-space paper's core architectural decision is that **reasoning lives in the
model, enforcement lives in the harness** — the model emits tool_use, the harness parses,
checks permissions, dispatches. This means the harness cannot guarantee which model serves a
free-tier request (the cascade may fall through), but it CAN guarantee that the request
carries the right thinking parameters for whichever model does serve it. That's exactly what
`shape_provider_thinking` already does per upstream.

**Recommendation:** auto-raise should set the effective effort on the request (the harness
side), and the existing per-upstream shaping (§2.1) handles the model side. No change needed
to the free-tier cascade for this to work. The raise is not wasted — even for models without
a thinking knob, the effort level affects temperature and prompt behavior (existing behavior).

The open question is whether to **prefer models that support thinking when raising**. The
MorphLLM router guide (2026) and LiteLLM auto-routing both treat **difficulty × ambiguity →
model tier + reasoning effort** as a combined decision, suggesting that routing and effort
should be co-optimized. **Recommendation:** when the classifier raises effort for a reasoning-
heavy task, add a **routing hint** (§6.6) that prefers free-tier upstreams whose default
model has `reasoning: true` in the models.dev registry — but only as a hint, never as a
guarantee, and never preventing fallback. This is a free-tier resilience improvement, not a
requirement for auto-raise to function.

### Q4: Plan Mode interview — always, or only for non-trivial tasks?

**Resolved — research favors gated, not always.** The Atlan guide is explicit that
Plan-and-Execute's weakness is **plan staleness** and **incorrect premise**: "the plan begins
with valid assumptions, but conditions change during execution... Without validation
checkpoints or re-planning, the executor may continue until a later step fails." And:
"Poor re-planning triggers: the system fails to revisit the plan after a tool error... If
triggers are too broad, the agent re-plans so often that it loses the benefit of planning
upfront." This is a direct caution against always-on interviewing: too much planning friction
destroys the benefit.

The LLM Agentic Failure Modes taxonomy (ceaksan, 2026) gives the positive pattern:
**"Confirmation loops: require confirmation for irreversible actions"** and **"Dry-run mode:
show what I will do before execution"** as part of a multi-layer defense for incorrect tool
invocation and task drift. And the Task Drift entry specifically recommends **"planning before
acting"** (OpenSpec/ADR approach) as a defense against drift, with the caveat that planning
should be gated to when it helps.

**Recommendation:** the Plan Mode interview is **gated to non-trivial tasks**, using the same
medium-ambiguity/medium-risk gating the user specified for asking. For trivial Plan Mode
sessions (single-file read-only analysis, a quick question), skip the formal interview and
just do the work with the §6.1 guidance. For non-trivial Plan Mode sessions, run the
interview: read context, present plan + open questions, ask medium-ambiguity questions, wait
for confirmation before editing. This matches the user's own "medium ambiguity and medium
risk" rule and the literature's caution about over-planning.

### Q5: Degeneration heuristics — how aggressive?

**Resolved with specific patterns from three papers.** There is now a real literature on
agent degeneration detection:

- **Infinite Agentic Loops (arXiv:2607.01641, Wang et al. 2026):** static analysis of
  6,549 repos found 68 confirmed IAL failures across 47 projects (91.9% precision). The
  defining pattern: a feedback path that "repeatedly triggers costly or state-growing actions
  without an effective stopping bound," where "continuation may still be controlled by model
  outputs, tool observations, external state, exceptions, or delegation decisions." Key
  insight for heuristics: **the danger is unbounded repetition of costly/state-growing ops,
  not any repetition.** Legitimate iteration is fine; the heuristic should key on
  *costly/state-growing* ops (writes, tool calls with side effects) repeating without
  progress, not on any repeated pattern.

- **Life-Harness (arXiv:2605.22166, Xu/Wen/Li 2026):** a lifecycle-aware runtime harness
  that "detects degenerate patterns such as repetition, stagnation, invalid retries, or budget
exhaustion, and triggers recovery when needed." It organizes adaptation into four lifecycle
  layers, with **Trajectory Regulation** as the post-execution layer that monitors for
  degeneration. Average 88.5% relative improvement across 18 model backbones by adapting the
  harness, not the model — supporting the spec's thesis that degeneration detection belongs in
  the loop/harness, not in the model.

- **LLM Agentic Failure Modes taxonomy (ceaksan, 2026):** the **Degeneration Loops** entry:
  "the model gets stuck repeating the same failed approach... Detection signal: repeated
  identical tool calls with no state change, or repeated error + retry without variation."
  Defense: "max step limits + tooling constraints: hard limits on agent loops."

- **Silent Failures taxonomy (arXiv:2606.14589, Wu 2026):** longitudinal study of a
  production agent runtime; 70% of silent failures were caught by human user-view observation,
  not tests/health checks. Key lesson: **detection that depends on the model self-reporting is
  unreliable** (fail-plausible class: the model narrates errors into fluent false output).
  Heuristics must be **independent of model self-report** — key off observable trajectory
  state (tool calls, tool results, file changes, token output), not off what the model says.

**Recommendation:** degeneration heuristics should be **conservative and observable-only**:

1. **Stall:** consecutive turns with negligible output tokens
   (`NO_PROGRESS_TOKEN_THRESHOLD` = 500, the measured floor — §6.5's grounding table) AND
   no tool calls AND no file writes, to a streak of `NO_PROGRESS_STALL_TURNS` = 3 →
   narration ("I'm not making progress on X; here's where I am") and optionally surface to
   user. Not a hard stop.
2. **Loop:** same tool call (same name + same args hash) repeating with no state change
   between → flag as possible loop, vary approach or ask. Key off *state change*, not off
   repetition alone (IAL-Scan's lesson).
3. **Divergence:** compare current file touches / stated intent against original prompt;
   if materially drifted (doing something the user didn't ask for) → flag and narrate,
   offer to refocus. Use goal re-anchoring (already exists: GOAL_REANCHOR_INTERVAL = 6) as
   the mechanism.
4. **Rabbit hole:** many turns on a sub-problem without converging (no tool result change,
   no file change, no answer progress) → narrate and offer to refocus or ask.

The **concrete constants for these four are in §6.5's threshold-grounding table** (with
the measurements behind the stall floor); this list records the research that motivates
the guards, §6.5 records the values to implement. Do not re-derive numbers here.

Start conservative: narration first, escalate to user-visible surfacing only on strong
signals (multiple turns of clear stagnation, or a loop that's doing real work with no progress).
The silent-failures paper's lesson is that **the user noticing is the most reliable detector**;
the heuristics should amplify that, not replace it. This matches the user's ask exactly:
"detect a possible loop or non progress or inaction or rabbit holes or work that is very
divergent from the original prompt."

### Q6: Does the user want a way to see past thinking traces more easily?

**NOT resolved — this is a user-preference / UX-discoverability question, not a research
question.** `-back` and `back-play` exist (docs/commands.md §"-back"). The research does not
speak to whether they're discoverable enough — that's a UI/UX judgment call that depends on
how the user actually works. The Claude Code paper notes that "humans can observe actions in
real time, approve or reject proposed operations, interrupt compatible in-progress operations,
and audit after the fact" — but that's about real-time observation, not past-trace discoverability.

**This one goes back to the user** (see suggestions at end of file).
---

## 10. Out of scope (for this spec)

- Rewriting the query loop orchestrator wholesale.
- A new interaction paradigm / separate reasoning UI.
- Paid-provider-specific thinking features (must work in free mode).
- Gateway (`/v1/*`) exposure — TUI/CLI only, consistent with the modes spec.
- YOLO / full permission bypass (already rejected by the safety floor).

---

## 11. Relationship to existing specs

- **Modes/UX spec** (`docs/plans/clawde-modes-ux-spec.md`): the preset system
  (Feature A) is where reasoning-guidance strength and auto-raise posture could be
  bundled as mode knobs. This spec is complementary — it defines the reasoning
  behavior; the modes spec defines how presets bind it.
- **Thinking inspector spec** (`docsing-inspector-spec.md`): the diagnostic that
  lets the user see "what thinking params were sent, was effort ignored, did
  thinking eat the budget." Supports Phases 1 and 5. If not yet built, Phase 1
  should consider whether to build the parts needed for self-debugging.
- **Ultracode** (`crates/core/src/effort.rs` `ULTRACODE_PROCEDURE`): the existing
  top-tier reasoning + workflow. Phase 2's auto-raise ceiling and Phase 1's prompt
  guidance should compose with (not duplicate) ultracode's procedure.
- **Free-provider chain + routing** (`crates/api/src/providers/free/`, AGENTS.md
  §Multi-Key Rotation): the environment these changes run in. Phase 5 depends on
  the existing fallback/retry machinery.

---

## 12. Research bibliography (cited in §9 resolutions)

Papers and sources consulted to resolve the §9 open questions. All are bleeding-edge
(2026) unless noted.

### Primary sources (directly resolve open questions)

- **Jali, Nayak & Joshi — "Not All Turns Are Equally Hard: Adaptive Thinking Budgets
  For Efficient Multi-Turn Reasoning" (arXiv:2604.05164v2, Apr 2026).** CMU. Formulates
  multi-turn reasoning as a sequential compute allocation MDP; TAB policy (GRPO-trained)
  allocates smaller budgets to easy turns, saves up to 35% tokens while maintaining
  accuracy. Key takeaways for this spec: (a) adapt budgets per turn, not uniformly;
  (b) easy turns waste budget that hard turns need; (c) the optimization signal is
  accuracy-vs-tokens, not brevity alone. Resolves Q1 (calibration) and reinforces Q2
  (ceiling rationale).

- **Liu, Zhao, Shang & Shen (VILA Lab / UCL) — "Dive into Claude Code: The Design Space
  of Today's and Future AI Agent Systems" (arXiv:2604.14228v2, Apr 2026, cited 51).**
  Source-level analysis of Claude Code's architecture. Core findings: the loop is a simple
  while-loop; most code is operational harness (5-layer compaction, 7-mode permissions,
  4 extensibility mechanisms, subagent delegation, append-only storage); guiding principle
  is "minimal scaffolding, maximal operational harness"; human decision authority is a
  first-class value. Resolves Q1 (guidance should be light on prescriptive structure),
  Q3 (reasoning in model, enforcement in harness — auto-raise sets request params, per-
  upstream shaping handles the model), and informs Q5 (observable-only heuristics).

- **Xu, Wen & Li (Peking U) — "Adapting the Interface, Not the Model: Runtime Harness
  Adaptation for Deterministic LLM Agents" (arXiv:2605.22166v2, May 2026).** Life-Harness,
  a lifecycle-aware runtime harness that improves frozen LLM agents without changing model
  weights, evolving "recurring interaction failures into reusable interventions" across
  environment contracts, procedural skills, action realization, and trajectory regulation
  (the post-execution monitoring layer this spec's §6.5 borrows from). Evaluated on seven
  deterministic environments from τ-bench, τ²-bench, and AgentBench: improves **116 of 126
  model–environment settings across 18 model backbones**, **88.5% average relative
  improvement**, and harnesses evolved from one 4B model's trajectories transfer to 17
  others. Resolves Q5 (degeneration detection belongs in the harness/loop, not the model;
  trajectory regulation is the layer to build it in).

- **Hou, Wang, Zhao & Wang (HUST) — "When Agents Do Not Stop: Uncovering Infinite Agentic
  Loops in LLM Agents" (arXiv:2607.01641v1, Jul 2026).** IAL-Scan, a static analyzer over
  an Agentic Loop Dependence Graph, run on 6,549 agent repos: **74 potential findings, 68
  confirmed IAL failures across 47 projects, 91.9% precision**. Defines
  IAL as a feedback path repeatedly triggering costly/state-growing actions without an
  effective bound, where continuation is controlled by model outputs/tool observations/
  state/exceptions/delegation. Resolves Q5 (the danger is unbounded costly/state-growing
  repetition, not any repetition; heuristics must key on state change + cost, not mere
  repetition).

- **Wu — "When Errors Become Narratives: A Longitudinal Taxonomy of Silent Failures in a
  Production LLM Agent Runtime" (arXiv:2606.14589v1, Jun 2026).** 8-week study of a
  production agent runtime: 22 incidents, 5-class silent-failure taxonomy, fail-plausible
  class (LLM narrates errors into fluent false output). ~70% of silent failures caught by
  human user-view observation, not tests/health checks. Resolves Q5 (heuristics must be
  independent of model self-report; the user noticing is the most reliable detector; amplify
  human noticing rather than replace it).

- **NousResearch/hermes-agent #13663 — "Smart reasoning_effort routing based on task
  complexity" (GitHub, Apr–Jul 2026, closed/completed).** Identical problem statement to
  this spec's Q2/Q3: static reasoning_effort is too much for trivial turns, too little for
  complex ones. Community-converged mapping: low = simple Q&A/single-file/formatting/
  device control; medium = default/planning/summarization; high = coding/debugging/multi-
  step/architecture; xhigh = explicit "think hard" only; ultracode/max = user-intent only.
  Accepted solution: rule-based smart routing (Option B), with LLM classifier (Option C) as
  later upgrade. UX requirements: resolve actual provider-side effort per request, surface
  in footer, explicit override is hard override, don't escalate for simple action commands
  just because tools are available. Directly resolves Q2 (ceiling + mapping) and informs
  Q3 (routing + effort co-optimization).

- **Atlan — "ReAct vs. Plan-and-Execute: Agent Architecture Guide [2026]."**
  Compares ReAct (reason-act-observe loop) vs Plan-and-Execute (plan upfront, executor,
  re-plan on trigger). Key cautions: ReAct's repeated-reasoning failure mode wastes tokens
  reconsidering what a stable plan fixed once; goal drift accumulates locally-useful steps
  away from the objective; Plan-and-Execute's risk is plan staleness and poor re-planning
  triggers ("if triggers are too broad, the agent re-plans so often it loses the benefit").
  Resolves Q1 (encourage stable plan early, don't re-reason perpetually) and Q4 (Plan Mode
  interview should be gated, not always-on; too much planning friction destroys the benefit).

- **"LLM Agentic Failure Modes" (ceaksan, Apr 2026).** Eight-agent-failure-mode taxonomy:
  task drift, incorrect tool invocation, reward hacking, positional bias, mode collapse,
  degeneration loops, alignment faking, version drift. Degeneration loops: "repeated identical
  tool calls with no state change, or repeated error + retry without variation." Defense:
  goal anchoring (restate original goal every N steps), planning before acting (OpenSpec/ADR),
  multi-attempt + reflection, max step limits + tooling constraints, cross-model verification.
  Task drift: "every N steps, ask the agent to restate the original goal. If the definition
  shifts, drift is active." Resolves Q5 (specific detection signals and defenses; goal
  re-anchoring is the drift mechanism — already exists in Clawde as GOAL_REANCHOR_INTERVAL).

### Secondary sources (context, not direct resolution)

- Alenezi — "From Prompt–Response to Goal-Directed Systems: The Evolution of Agentic AI Software Architecture" (arXiv:2602.10479v1, Feb 2026). Reference architecture separating cognitive reasoning from execution via typed tool interfaces; BDI foundations; convergence toward standardized agent loops, registries, auditable control. Context for §2.4 and §6 component map.

- Sebastian Raschka — "Controlling Reasoning Effort in LLMs" (2026) and "Language Models for Text Classification" (2026). Background on reasoning-effort parameters and the role of lightweight classifiers as pre-screeners — context for Q2/Q3 classifier discussion.

- LiteLLM auto-routing docs + MorphLLM LLM-router guide (2026). Production router practice: difficulty × ambiguity → model tier + reasoning effort; unmatched turns fall to defaults. Context for Q3 routing-hint discussion.

- OpenAI — "A practical guide to building agents" (Jan 2026). Three-component agent model (model, tools, orchestration). General context.

- Yao et al. (2022/2023) — ReAct original. Referenced by the above; Claude Code paper and Atlan guide both trace their reasoning-to-action lineage here.


### Tertiary sources (refine §6.1 self-check, §6.5 guards, and measurement discipline)

These did not change any §9 resolution, but they sharpen the implementation guidance in
§6.1, §6.5, and the measurement discipline the spec should adopt. Added post-§9-resolution
so the implementation team has the nuance. **Status (re-checked 2026-10-10): all three gaps
are closed.** Gap 1 (effort budget values) now rests on each vendor's own docs, fetched
directly — §2.1's per-upstream table. Gap 3 (degeneration thresholds) now rests on the
constants Clawde already ships plus a measured scan of 2,545 real turns on this machine —
§6.5's grounding table. Gap 4 (verify cadence) now rests on measured wall times for the
two commands `detect_project.rs` actually runs — §6.5's cadence note. Two citations in the
earlier draft (ironclaw #2240, a Strands `DebounceHook` write-up) could not be reproduced
and have been removed; the "industry convergence on 3" claim they supported is withdrawn.
Everything below is first-hand-fetched or repo-measured.

- **Anthropic — "Extended thinking" (platform.claude.com/docs/en/build-with-claude/
  extended-thinking, 2026).** The authoritative numeric guidance for `budget_tokens`:
  minimum 1,024, must be `< max_tokens`, target-not-cap, simple tasks start at the minimum
  and complex tasks at **16,000+**, budgets above 32k need batch processing, and changing
  `budget_tokens` between requests **invalidates prompt-cache breakpoints**. Closes Gap 1
  (§2.1) and adds the §6.2 cache constraint.

- **Anthropic — "Effort" (platform.claude.com/docs/en/build-with-claude/effort, 2026).**
  The `output_config.effort` levels `low|medium|high|xhigh|max`; `high` is the default on
  most models and **`xhigh` the recommended starting point for coding and agentic work**;
  effort governs all output tokens including tool calls; per-message effort preserves the
  prompt cache on newer models. Closes Gap 1 (§2.1: the ladder default rung is too low for
  coding) and constrains §6.2 (cache-safe, sticky auto-raise).

- **Google — "Thinking" (Gemini Enterprise Agent Platform / Vertex AI,
  docs.cloud.google.com, fetched 2026-10-10).** The numeric/deprecation authority for
  Gemini: Gemini 3+ takes `thinking_level` with values `MINIMAL|LOW|MEDIUM|HIGH`
  (per-model subsets **and per-model defaults** — 3.x Flash `MEDIUM`, 3.1 Pro `HIGH`,
  3.5 Flash-Lite `MINIMAL`), and **`thinking_budget` is deprecated for Gemini 3+**
  ("Using thinking_budget with these models can cause intermittent 400:
  INVALID_ARGUMENT errors"; specifying both errors outright). Gemini 2.5 and earlier
  use `thinking_budget` as a soft limit, with `0` disabling thinking. `HIGH` is
  documented as best for "deep reasoning, multi-step problem solving, **formal code
  verification, or multi-turn tool execution**". Closes Gap 1 for Google and supplies
  the §2.1 warning about the direct `google.rs` path.

- **OpenAI — "Reasoning models" (developers.openai.com/api/docs/guides/reasoning,
  fetched 2026-10-10).** `reasoning.effort` accepts
  `none|minimal|low|medium|high|xhigh|max`, model-dependent; **GPT-6 Astra returns HTTP
  400 for `none`** and GPT-6.1 Sol for `none`/`minimal` (it defaults to `medium`);
  `gpt-5.5` defaults to `medium`; the effort table places "agentic coding" under both
  `medium` ("Default configuration for most workloads") and `high` ("Recommended for
  complex workflows and agentic tasks"); `xhigh` is "Only use when your evals show a
  clear benefit"; and it recommends **reserving at least 25,000 tokens for reasoning
  and outputs**. Closes Gap 1 for OpenAI and supplies §6.2's output-cap constraint.

- **DeepSeek — "Thinking Mode" (api-docs.deepseek.com, fetched 2026-10-10).**
  `thinking.type` + `reasoning_effort`; the OpenAI-format accepted set is
  **`low|high|max`**; thinking is **enabled by default at effort `high`**; the vendor's
  own mapping collapses minimal→low, medium→high, xhigh→high, ultra→max (the same
  collapse Clawde implements); thinking mode ignores temperature/penalties and clamps
  `top_p` to 0.95–1.0; and **`reasoning_content` must be echoed back on every
  subsequent tool-call request or the API returns 400** — already covered by
  `ProviderQuirks::requires_reasoning_roundtrip`. Closes Gap 1 for DeepSeek and
  validates the existing arm.

- **Z.AI — "Deep Thinking" (docs.z.ai, fetched 2026-10-10).** `thinking.type`
  `enabled|disabled` — with **GLM-5.3 / GLM-5.3-FLASH erroring on `disabled`** — plus
  `reasoning_effort` on GLM-5.2+ (`max` default and recommended, `high`, `low`; GLM-5.2
  additionally accepts `xhigh|medium|minimal|none`, collapsing low/medium→high and
  xhigh→max). Closes Gap 1 for Z.AI and records the two divergences in §2.1.

- **Alibaba Model Studio — "Use deep thinking models via API" (alibabacloud.com,
  fetched 2026-10-10).** Mixed mode toggles per request via `enable_thinking`
  true/false (non-standard, passed through `extra_body`); thinking-only mode cannot be
  disabled; thinking text returns in `reasoning_content`; model defaults differ
  (qwen3.8-*/qwen3.7-* default thinking **on**, `qwen-plus`/`qwen-flash` default
  **off**); the page documents **no numeric `thinking_budget`**, so the boolean is the
  right control for Clawde's Qwen arm.

- **Poolside — "OpenAI-compatible API examples" (docs.poolside.ai, fetched
  2026-10-10).** Thinking is on by default; off via
  `chat_template_kwargs.enable_thinking: false`; reasoning returns in
  `reasoning_content`; documented generation defaults are `max_tokens` **32768**
  (range 1–262144), temperature 1.0, top_k 20; and agentic workflows must
  "preserve `reasoning_content` from assistant responses when you include those
  responses in follow-up requests" — covered by `requires_reasoning_roundtrip`.
  Validates the existing Poolside arm; the 32k default feeds §6.2's output-cap note.

- **Aider — "Linting and testing" (aider.chat, fetched 2026-10-10).** "By default, aider
  will lint any files which it edits" (disable with `--no-auto-lint`); the test path is
  opt-in — "You can configure aider to run your test suite after each time the AI edits
  your code using the `--test-cmd` and `--auto-test` switch." Also: the lint command
  "should accept the filenames of the files to lint" while the test command "will run the
  test command without any arguments" — i.e. the cheap gate is file-scoped and the
  expensive one is project-scoped. Closes Gap 4: per-edit verification with exactly
  Clawde's `lint_edited_files` + `run_checks` split, now paired with the measured
  wall times in §6.5 (8.25 s vs 249.75 s).

- **CrewAI — "Agents" (docs.crewai.com, 2026).** `max_iter` default **20**, `max_retry_limit`
  default **2**. *Context only*: these are total-iteration and error-retry ceilings, not
  detection windows — they bound the run, they do not say when the run is degenerate. Gap 3
  does **not** rest on them (see §6.5's grounding table).

- **LangGraph — "GRAPH_RECURSION_LIMIT" (docs.langchain.com, 2026).** A graph that loops
  hits the recursion limit; the documented remedy is a higher `recursion_limit`, i.e.
  bounded iteration is the guardrail. *Context only* for Gap 3, for the same reason as
  CrewAI.

- **NousResearch/hermes-agent #41477 — "Add stall detector" (GitHub, fetched 2026-10-10).**
  Specifies the stall detector's key: "(same action signature: tool name + normalized args
  hash)" — the direct precedent for §6.5's `(name, args_digest)` loop key. The issue's
  repeat count is **configurable**, so it is cited for the mechanism only, not a threshold.

- **OpenHands — "Agent stuck detector" (docs.openhands.dev, fetched 2026-10-10).** The SDK
  ships stuck detection as a first-class feature (`stuck_detection=True` when constructing
  a conversation). Evidence that a shipped coding agent treats "stuck" as a harness
  concern; the docs quote **no** numeric constant, so it is cited for the mechanism only.

- **Macedo — "Stop Hand-Holding Your Coding Agent: Engineering the Loops that Replace
  Step-by-Step Prompting" (arXiv:2607.00038, Jun 2026).** The loop-specification study: a
  loop spec is trigger + goal + verification step + **stopping rule** + memory; it offers a
  five-level verification ladder, a taxonomy of named terminal states (74% of the fifty
  coded loops name theirs), and anti-patterns grounded in self-correction, reward-hacking
  and model-as-judge fragility. Source for §6.5's stop-rule framing and for §6.1 step 4's
  "verify, don't self-report" posture. It specifies **no token threshold** — the code
  comment in `goal_loop.rs` that attributes `NO_PROGRESS_TOKEN_THRESHOLD` to it is not
  supported by the paper and should be corrected when that file is next touched.

- **arXiv:2505.02709 — "Technical Report: Evaluating Goal Drift in Language Model
  Agents."** Detecting/measuring drift from an agent's original objective over time; drift
  is gradual and shows only subtle behavioral change. Supports §6.5's divergence item
  (conservative, narration-first) but supplies no numeric threshold.

---

## 13. Final unresolved item — resolved by user (Q6)

**Q6: past thinkingtrace discoverability.** Research could not resolve this (it's a UX/
discoverability call, not a best-practice question). User answered: wants a **thinking
history panel / sidebar** — a dedicated view listing past thinking traces across the session,
selectable, so the agent's reasoning can be reviewed without re-running anything. `-back`/
`back-play` exist but are not the desired surface.

**Implication for implementation:** Phase 1's thinking-visibility work (§6.3) should include
a session-scoped thinking history that the TUI can render as a panel/sidebar, in addition to
the live thinking blocks and the existing `-back`/`back-play` commands. Stored thinking
content is already captured as `ContentBlock::Thinking` in assistant messages (§2.2), so this
is a read+render capability atop existing stored state, not new persistence. The panel should
be available during and after the session, list traces per turn, and let the user select one
to expand/read. This is scoped to TUI; headless/`--print` surfaces are out of scope for v1
(consistent with the modes spec's "non-TUI surfaces out of scope" decision).

---

*Spec ends here. No code changes yet.*
