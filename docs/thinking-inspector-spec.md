# Thinking Inspector — Spec (refined)

Read-only panel showing, for the currently selected model, exactly what Clawde
is about to send on the wire: which provider serves it, what thinking mode and
budget that provider's API gets, what `max_tokens` cap applies, and what the
effective budget is after clamping. Plus a live "last response" line from the
previous turn. Goal: make the invisible failure modes visible — empty content
from budget-eaten thinking, silent clamping, "why is my effort being ignored" —
without adding a single new user knob.

Deliberately **not** an editor. Tuners were rejected: the thinking space is a
provider × model × effort matrix, not a dial; `free/auto` routes to any of 13
upstreams so a per-provider slider would be a lie; and the raw wire params
(`thinking.type`, `reasoning_effort`, `chat_template_kwargs`) are exactly the
foot-guns the effort ladder exists to hide. Users who want precision already
have `settings.json` via `provider_configs.options`.

## User stories this exists for (validated by upstream issues)

1. **"I toggled thinking/effort and forgot what state it's in."** Claude Code
   #13158 (users can't see thinking mode / reasoning effort in the status
   line; higher effort consumes more tokens, invisibly). Clawde's footer
   already shows the dial persistently (`· ◐ Medium` in `render.rs`), so the
   *state* is covered — the inspector adds the *consequence*.
2. **"I turned thinking off but the model still thinks."** Claude Code #81449
   ("Option+T toggle has no effect on Opus/Sonnet"). Some models ignore the
   param, some providers have no knob at all. The inspector shows what the
   selected model/upstream actually supports.
3. **"The response came back empty / truncated and I don't know why."**
   Reasoning consumed the output budget (the poolside
   `reasoning_tokens: 501, completion_tokens: 0` bug class). The inspector's
   "last response" row flags it.
4. **"Why does my 20K budget feel clamped?"** `max_tokens_cap` (poolside 8K)
   and the `max_tokens − 1` Gemini rule clamp silently. The inspector shows
   raw → effective with the reason.

## Where it plugs in

- **Primary surface:** the free-model popup (`Alt+J/K`, `free_model_popup.rs`).
  A selected row shows a two-line footer under the list: provider, thinking
  mode + effective budget, cap, and any clamp/ignore warnings.
- **Secondary surface:** the model picker (`Alt+M`, `model_picker.rs`) and the
  effort picker (`Alt+H/L`, `effort_picker.rs`) get the same one-line summary so
  users see the consequence of dialing effort up/down *at the moment they dial*.
- **Deep-dive:** `/ctx-viz` gains an optional "thinking" tab with the full
  table below.
- **Reuse:** the row data is computed by one pure function in `clawde-api`
  (`inspector_row(...)`), consumed by all three surfaces. The TUI never
  re-implements the mapping.

## Row-by-row data (one function, `inspector_row`)

Input: `provider_id`, `model_id`, `effort_level`, `max_tokens` (the request's),
`thinking_budget` (request's or default), and the catalog's `FreeUpstream`.

| Row | Shown value | Source (existing) | Notes |
|---|---|---|---|
| Provider | upstream title + id | `take_free_model_defaults()` triple, joined to `FreeUpstream` by id | For `free/auto` show "auto — first healthy of 13" |
| Thinking mode | `enabled` / `disabled` / `n/a` | `shape_provider_thinking` gate logic | `n/a` when the model has no knob (plain openai-compat chat) — answers story #2 |
| Wire param | exact key+value sent | `shape_provider_thinking` | e.g. `reasoning_effort: "high"`, `thinking.type: "enabled"`, `thinkingBudget: 20000`, `enable_thinking: true` |
| Control type | **budget** vs **behavioral** | per-provider map | Gemini `thinkingBudget`/Anthropic `budget_tokens` are hard budgets; Claude `output_config.effort`, OpenAI `reasoning_effort` are *behavioral signals* ("not a strict token budget" — Claude platform docs). Show "behavioral — no hard budget" instead of a fake number for those. |
| Effective budget | tokens after clamp | `level.thinking_budget_tokens()` vs `max_tokens − 1` | The Gemini/Clawde clamp rule; only meaningful for budget-type controls |
| `max_tokens` cap | cap value | `FreeUpstream.max_tokens_cap` (e.g. poolside 8K) | |
| Context window | tokens | `FreeUpstream.context_window` (e.g. poolside 256K) | |
| Capabilities | tool-call / vision | `FreeUpstream.tool_calling`, `.vision` | Mirrors what plan-build already enforces |
| Clamp warnings | computed | effective budget vs cap; budget ≥ max_tokens | e.g. "budget 20K clamped to 8K by poolside cap" |
| Ladder quirks | computed | `EffortLevel` table | Surfacing the non-monotonic rungs: **Low disables thinking while Minimal enables a 1024-token budget** — a genuine "effort ignored" confusion. Also Low forces `temperature: 0.0` |
| Last response | `reasoning_tokens` / `completion_tokens` / `finish_reason` | phase-1 telemetry (done) | "thinking ate the budget" flag below |

### Last-response diagnostics (heuristic)

All numbers read from the phase-1 `last_route` telemetry. Tolerances are
deliberately conservative — better to under-flag than cry wolf.

| Condition | Flag | Suggested fix shown |
|---|---|---|
| `reasoning_tokens ≥ 0.9 × max_tokens` and visible content empty | 🔴 budget eaten | "raise effort budget or lower effort; completion_tokens ≈ 0" |
| `reasoning_tokens ≥ 0.5 × max_tokens` and content present | 🟡 thinking-heavy | "content still produced; budget is tight" |
| `finish_reason == length` (MaxTokens) | 🟡 truncated | "raise max_tokens or lower effort" |
| `reasoning_tokens > 0` but effort was set to disable thinking | 🔴 param ignored | "this model thinks even with thinking off" (story #2) |
| wire param `thinking.type: enabled` on a model whose family the provider gates as non-reasoning | 🔴 mismatch | model-registry gate vs wire shape disagreement |

### Known upstream quirks to document (not fix)

- Gemini: `thinkingBudget` is *sometimes ignored* (googleapis/python-genai
  #782) — the inspector shows the requested value but the "last response"
  reasoning count is the ground truth, which is exactly why the last-response
  row exists.
- OpenAI o-series: only `low/medium/high` accepted; GPT-5.x takes the full
  ladder including `xhigh`. Sending an unsupported value → 400. The inspector
  shows the value Clawde will send, so the error is never a surprise.

## Data status

**Phase 1 (data) — DONE, implemented:**

- `UsageInfo.reasoning_tokens` (`#[serde(default)]`) added in core; filled by
  the google (`thoughtsTokenCount`), openai-compat
  (`completion_tokens_details.reasoning_tokens` + top-level `reasoning_tokens`
  for poolside/zai/deepseek), codex, and copilot parsers; 0 elsewhere
  (Anthropic folds thinking into `output_tokens`).
- `FreeLastRoute { upstream_id, model, usage }` with `store_free_last_route` /
  `take_free_last_route` (catalog.rs, re-exported from `free/mod.rs`), written
  at the **non-streaming** dispatch success point in `impls.rs`.

**Pending for phase 1.5:** the *streaming* success path
(`RetryingFreeStream`) doesn't write telemetry yet. Hook the same
`store_free_last_route` call at the stream's terminal `MessageStop`/usage
event so streaming sessions populate the "last response" row too (streaming is
the default path — this matters more than the non-streaming hook).

## Research & prior art

### How other tools surface this

- **Claude Code** deliberately hides thinking *content* (beta header
  `showThinkingSummaries`), and users filed #13158 asking for thinking
  mode/effort in the status line — the exact "state + cost visibility" gap.
  Implication: show counts and knobs, never render `reasoning_content` (Clawde
  already has Ctrl+O for content visibility).
- **OpenCode** ships *built-in variants* — Anthropic `high`/`max` (thinking
  budget), OpenAI `none`→`xhigh`, Google `low`/`high` — plus user-defined
  variants with a `variant_cycle` keybind. This is the strongest counterexample
  to "no tuner": variants work for OpenCode because its model surface is
  narrow and per-model. Clawde's 13-upstream `free/auto` matrix makes the
  analogue dishonest — a "high effort" variant means different wire params
  depending on which upstream answers. The read-only inspector keeps the
  precision knob where it belongs: `provider_configs.options`.
- **Open WebUI** renders reasoning content in a collapsible block per model —
  the content-visibility half Clawde already has (Ctrl+O / thinking blocks).

### Conventions confirmed by research

- **Effort is a behavioral signal, not a strict token budget** (Claude
  platform docs). Only Gemini `thinkingBudget` / Anthropic `budget_tokens` are
  hard budgets. The inspector must not present a fake effective-budget number
  for behavioral controls.
- **Effort changes invalidate prompt-cache prefixes** (Claude docs). A
  long-session user who dials effort mid-conversation silently loses cache
  hits. Worth a one-line note in the effort picker's inspector footer when the
  session has prior turns: "effort change re-encodes the context".
- **Thinking tokens bill at output rates** (Gemini pricing, universally).
  The inspector's numbers double as cost intuition.

## What the inspector must NOT do

- No new settings, no sliders, no write path to `provider_configs`.
- No per-provider overrides surfaced to the user (that's the rejected tuner).
- No model-registry changes: everything renders from data the catalog,
  effort-shaping, and usage already carry.
- No effect on the request path — pure read/render; a bug in the inspector can
  only mis-render, never mis-send.
- Never render thinking content — counts only.

## Phasing

1. **Data** — DONE (see above).
2. **Phase 1.5:** streaming telemetry hook (see above).
3. **Core row:** `inspector_row()` in `clawde-api` + unit tests against the
   known catalog entries (poolside cap, google clamp, zai thinking.type, qwen
   enable_thinking, plain chat n/a, Low-vs-Minimal quirk, behavioral-vs-budget).
4. **Surfaces:** free-model popup footer → effort/model picker one-liner →
   `/ctx-viz` tab.
5. **Verify:** `cargo test --workspace`, clippy, and the existing Alt+J/K /
   Alt+H/L tmux smoke probes.

## Open questions

- Should the popup footer show only *effective* values (budget after clamp) or
  also the raw request value? Spec assumes "raw → effective" with the clamp
  note, but that's a 2-line rendering choice.
- Should `inspector_row` accept an overridden `thinking_budget` (from a
  future user config) or always use the level default? Spec assumes level
  default; passing the request's actual value is a trivial extension.
- `last_route` telemetry covers the free chain only; the direct-provider path
  (non-`free`) would need its own hook if the inspector should cover it too.
  Out of scope for v1 — the popup only lists free models anyway.
- Is the "effort change re-encodes the context" cache note worth showing, or
  is it noise for a free-tier product? Spec includes it; happy to cut.
