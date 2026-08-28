# Clawde Modes + UX — Feature Spec

Status: Draft (post-interview, post-audit — audit corrections in §2, §4, §7, §9)
Scope: TUI/CLI only (explicitly **not** the gateway in v1)
Author: user + agent, from a structured interview (6 rounds)

---

## 1. Origin of the request

The user asked whether Clawde could have **two modes**: the current behavior,
plus a second mode "tailored to my needs." Through the interview, the request
expanded and clarified into a set of distinct features. The user's actual
pain points are more specific than "a second mode":

1. **"Clawde seems abrupt — it doesn't look like it thinks much."** The user
   comes from Cline, Freebuff, and OpenCode, which show more visible
   reasoning/check-ins and ask on ambiguity.
2. **They want per-rule autonomy, not all-or-nothing.** Cline's auto-approval
   is familiar; Cline's "YOLO mode" is not. Over-prompting erodes their
   diligence ("if I keep accepting permissions, I'm MORE likely to approve
   something important"). They want to be prompted only for important things.
3. **Walk-away autonomy.** "Set-it-and-forget-it": let it run unattended,
   deferring decisions to address later rather than blocking.
4. **Recovery beyond git.** "Undo last task / last two tasks" that understands
   the agent's own change boundaries — not just `git reset`.

Two additional UX features emerged that are **global** (both modes), not
scoped to the tailored mode: **ranked suggested followups** and
**color-coding/formatting for glanceable info**.

---

## 2. How Clawde works today (verified against code)

Grounding for everything below — do not re-litigate these in implementation.

### 2.1 The decision layer (corrected by audit)

- **Orchestrator**: `run_query_loop` (`crates/query/src/lib.rs:1343`). A
  code-governed loop: dispatch model turn → decide → repeat. It owns
  turn/stop/retry/compaction/cap discipline. The gateway ships a smaller
  sibling, `run_agent_loop` (`crates/gateway/src/agent.rs:232`).
- **`decide.rs` is currently dead code — audit correction.** `decide_mode`
  and `decide_verify` (`crates/query/src/decide.rs`) have **zero production
  callers**; only their own unit tests reference them. The module doc calls
  them the "single source of truth," but the real enforcement points are:
  - the **spec-mode write gate** `plan_gate_error`
    (`crates/query/src/runner/tools.rs:75`) — blocks file mutators when
    `spec_mode` is on and no approved `/spec` exists for the task,
  - **`verify.rs`** (verification policy),
  - the **permission classifiers** (`bash_classifier.rs`,
    `ps_classifier.rs`) + `PermissionManager`.
- **The real Plan-vs-Execute overrides**: `/plan` (sets
  `PermissionMode::Plan`, `commands/src/session.rs`) and `/spec` / `spec_mode`
  (spec.rs). **`/execute` does not exist** — `decide.rs` mentions it
  aspirationally; there is no `ExecuteCommand`. This matters for §7.1:
  a preset cannot tune a `decide_mode` threshold because nothing calls
  `decide_mode`.
- **Personas are pure prompt text**: `cathead`/`caveman`/`normal` are output
  styles (`crates/core/src/output_styles.rs`) injected as an
  `## Output Style` system-prompt section. They never change the engine —
  only tone. Inline keywords (`cathead`, `caveman`, `ultracode`) are transient
  per-turn (`crates/core/src/keywords.rs`); `/output-style` and `/cathead`
  etc. persist.

### 2.2 Config surface (what a preset can already control)

`Config` (`crates/core/src/lib.rs:1517`) already carries, per project/session:

- `model`, `max_tokens`, `default_effort` (persisted + CLI-overridable)
- `permission_mode` (Default / Plan / AcceptEdits / BypassPermissions)
- `output_style`, `custom_system_prompt`, `append_system_prompt`
- `allowed_tools` / `disallowed_tools`
- `workspace_paths`, `auto_compact`, `compact_threshold`

A "preset" can therefore bind a set of these knobs without engine changes.

### 2.3 Existing mode cycling (verified)

The TUI cycles **three modes** with **Tab**: `build`, `plan`, `image`
(`crates/tui/src/app.rs:3362`, `const MODES: &[&str] = &["build", "plan",
"image"]`), with image mode saving/restoring the model. There is also a
"fast mode" toggle (test: `test_fast_slash_command_toggles_fast_mode`). The
user's "build/plan/image" report is confirmed by code — not just a report.

### 2.4 Snapshot / undo machinery (already built — key finding)

- `ShadowSnapshot::for_session(working_dir)` (`crates/core/src/snapshot/`)
  with a process-global registry (`get_or_create`, keyed by working dir).
- Every writing assistant turn carries a `snapshot_patch`; diffs materialize
  via `materialize_turn_changes`.
- `FileHistory` (`crates/core/src/file_history.rs`) records per-turn file
  changes (`get_entries_for_turn(turn_index)`).
- **Commands already shipped**: `/undo` (revert last assistant turn),
  `/revert [n|uuid]` (revert the n-th most recent turn, or by message id),
  `/checkpoints`, `/snapshot` — all in
  `crates/commands/src/history.rs`. `n > 1` and uuid targeting already work.

**Implication**: "undo last task" is *not* greenfield. The gap is (a) a
**per-prompt-request** boundary (revert everything since the last user
message, which spans multiple assistant turns) vs the current per-turn
boundary, and (b) **human-readable state descriptions** per snapshot.

### 2.5 Reusable machinery (verified — grounds §7 decisions)

- **`AskUserQuestionTool` exists** (`crates/tools/src/ask_user.rs`):
  interactive-only, sends a `UserQuestionEvent`, suspends the query loop
  until the TUI answers; returns an error headless. This is the natural
  mechanism for ask-on-ambiguity and milestone check-ins — no new
  user-question primitive is needed (§7.2, §7.3).
- **Snapshot API is diff-derived** (`crates/core/src/snapshot/shadow.rs`):
  `patch(hash)` → `Patch`, `diff(hash)` → unified diff,
  `diff_full(from, to)` → per-file diffs, all from stored git tree hashes.
  Descriptions can be generated lazily from the stored diff at any time
  (§7.7).
- **Transcript persistence exists** (`crates/core/src/session_storage.rs`,
  JSONL, resumable via `/resume`). The defer queue can ride on transcript
  markers with zero new persistence machinery (§7.5).
- **User-defined named things already have a pattern** — output styles load
  from `.md`/`.json` files in `~/.clawde/output-styles/` and
  `.clawde/output-styles/` (`load_output_styles_dir`). Custom modes should
  mirror it (§7.1).
- **Typed `Config` knobs** (`crates/core/src/lib.rs:1517`) cover model,
  effort, permission mode, output style, allowed tools — a `ModeDef` binds
  these as already-typed fields (§7.1).
- **Auto-approve machinery already exists — audit correction.**
  `crates/core/src/auto_mode.rs` defines `AutoApproveMode`
  (`auto_approves_bash()`, `auto_approves_edits()`, opt-in state with
  reset), and `PermissionManager` already holds `session_rules` with
  `add_session_allow(name)` / `add_session_allow_path(name, path)`
  (`crates/core/src/lib.rs:4625`, checked "persistent first, then
  session"). **Feature F's "auto-approve for this session" is largely
  built**; Phase 4 is mostly defer-queue + narration + posture wiring, not
  new permission machinery (§4.6, §7.5).
- **Ask-on-ambiguity guidance already exists** — the system prompt already
  instructs the model when to ask
  (`crates/core/src/system_prompt.rs:630`: "hard to reverse, ask one
  clarifying question (AskUserQuestion) before acting"; `:576` per-tool
  guidance). `AskUserQuestionTool` is in the default tool set
  (`crates/tools/src/lib.rs:952`) with a full TUI dialog
  (`crates/tui/src/ask_user_dialog.rs`). §7.3 is therefore about
  **modulating existing guidance**, not inventing a trigger (§7.3).

---

## 3. Locked decisions (from the interview)

| # | Question | Decision |
|---|---|---|
| D1 | What should the mode change? | **Both**: a distinct preset (settings) AND some decision-rule customization. |
| D2 | How is it triggered? | **Both**: a persistent default (per project) + transient per-turn/per-request override. |
| D3 | Where does it live? | **TUI/CLI only.** The gateway (/v1/*) is out of scope for v1. |
| D4 | Mode shape | **Named presets** (e.g. "careful", "fast", "planner"), pickable in `/config`, not one bespoke mode. |
| D5 | Starter preset | **Must be locked before Phase 2** (does not block Phase 1). Interview evidence points to a **careful/planner** starter: the user wants check-ins "until the agent proves itself" and "ask on design decisions, autonomous on mechanical edits." Also floated: hotkey Build/Plan subtype toggling (tab+h / tab+l) and an interview-style "advanced plan mode" (§4.2). |
| D6 | Preset knobs | Bundle **plan-vs-execute rules + effort + check-in cadence + ask-on-ambiguity** together (all four selected). |
| D7 | Safety floor | **Allow per-rule opt-ins**, but never a YOLO mode. Relax marginal gates (auto-approve safe writes) with explicit warnings; a hard line stays for irreversible actions. |
| D8 | Ranked followups + color/format | **Both modes / everywhere** (global features, not mode-gated). |
| D9 | Undo boundary | **Per prompt request**: revert everything changed since the user's last message. |
| D10 | Undo safety | **Session-scoped**, safe: only reverts Clawde's own changes; plus **per-session snapshots with a description of the code state** at each snapshot. |
| D11 | Walk-away behavior | **Autonomous + sanitized**: make reasonable safe choices itself; refuse only what is genuinely irreversible. Deferred items queue for later review. |
| D12 | First slice to implement | **Task undo / snapshots** (the recovery net), because it makes the autonomy features safe. |
| D13–D19 | Spec open questions | Resolved in §7 (preset schema 7.1, cadence 7.2, ask trigger 7.3, followup source 7.4, defer persistence 7.5, undesired semantics 7.6, snapshot description timing 7.7). |
| D20 | Audit corrections | Recorded in §2.1/§2.3/§2.5, §4, §7 — see the audit log §9. |

---

## 4. Feature decomposition

### 4.1 Feature A — Named presets (modes framework)

A mode/preset = a named bundle of existing config knobs + optional decision
overrides, persisted in settings, switchable per project and transiently per
turn.

**Preset contents (bundle, per D6):**
- Effort: binds `default_effort` (e.g. low/high) — already configurable.
- Check-in cadence: prompt-level narration + `AskUserQuestion` gating per
  §7.2 — **no loop pause primitives** (loop stays untouched).
- Plan-vs-execute rules: binds the mechanisms that are actually wired —
  `spec_mode` (spec gate), permission mode, and the `/plan` override — per
  §7.1 (audit correction: `decide_mode` has no production callers).
- Ask-on-ambiguity: modulates the existing system-prompt guidance + gating
  of the already-default `AskUserQuestionTool` per §7.3 — no new decision
  point.
- Tool/permission posture: binds `permission_mode` + `allowed_tools` (+ the
  existing `AutoApproveMode` for walk-away presets, §4.6).

**Mechanics (audit-corrected):**
- **One source of mode definitions**: built-ins in code (like
  `output_styles.rs` builtins) + **user-defined from disk**
  (`~/.clawde/modes/`, `.clawde/modes/`), mirroring the output-styles
  pattern. Do **not** add a parallel settings-embedded `"modes": {...}`
  map — two sources for the same concept is needless complexity (§7.1).
- The active mode is a single setting (`"mode": "careful"`), per project.
- `/config mode` picker lists named presets; a per-project default.
- Transient: an inline keyword or `/mode <name>` for one turn, mirroring the
  existing inline keyword system (`keywords.rs`). **Precedence rule**: a
  transient inline keyword beats the preset for that turn (same as inline
  personas beat `/output-style` today) — must be stated explicitly in the
  spec and tested.
- **Do not change the engine**: presets are layered on top of an unchanged
  safety core except where a preset explicitly opts into a marginal-gate
  relaxation (D7, with warnings).

### 4.2 Feature B — Build/Plan subtype toggling (floated; secondary)

The user's idea: the TUI already cycles build/plan(/image) modes with Tab.
They envision **Tab+h / Tab+l to decrease/increase the "subtype"** of the
current mode, plus the possibility of a custom user-defined mode outside the
built-in set. Also floated: an "advanced Clawde plan mode" inspired by
interview-style prompting (Freebuff/Codebuff `/interview`), useful
*sometimes*.

Status: **nice-to-have / design input**, not yet a firm requirement. The
spec records it so the preset system is built with subtype
extensibility in mind (a preset should be able to add or override
sub-behaviors, and custom user modes must be representable).

### 4.3 Feature C — Ranked suggested followups (global)

After a response, suggest followups **ranked by usefulness**:

- Rank classes (user-specified): **Highly recommended, Recommended,
  Optional, For completion, Unimportant, Undesired** — each followup carries
  one rank.
- The ranking should be *generated* (the model proposes followups with a
  rank and a one-line reason) and *rendered* with the rank visible.
- This is an output-layer feature: post-response suggestion rendering +
  prompt guidance to the model about producing ranked suggestions.
  (Typeahead `@`-completions already exist; this is a different, post-turn
  surface.)

### 4.4 Feature D — Color-coding + formatting (global)

Make information **readable at a glance**: color-coding and consistent
formatting of status/result/diff/error/recommendation content in the TUI
renderer. The user explicitly called this out alongside ranked followups as
"both of these will really help."

- Audit the existing renderer (`crates/tui/src/render.rs`) for consistent
  semantic coloring (success/warning/error/neutral), status lines, and
  structured output (diffs, checkpoints, followup ranks).
- Theme-aware (the app already has `theme: Theme` in config).

### 4.5 Feature E — Task undo / snapshots (FIRST SLICE, per D12)

Extend the existing snapshot/history machinery. Audit-added requirements
marked (A).

1. **Per-prompt-request undo**: `/undo` reverts everything changed since the
   user's last message — i.e. the group of assistant turns belonging to one
   prompt — not just a single turn. Boundary = last `Role::User` message.
   (`/revert 2` exists but is turn-based; this is request-based.)
2. **(A) Multi-prompt undo**: the user asked for "undo last two tasks" —
   add `/undo [n]` = the group of assistant turns since the user's message
   n prompts back, mirroring `/revert [n]`'s turn numbering. Without this,
   "undo last two tasks" is unimplementable.
3. **(A) Mandatory confirmation**: undo shows the grouped turns + their
   diffs (reusing `/checkpoints` data) and requires confirmation before
   reverting. Rationale: interleaved chat makes the prompt boundary fuzzy
   (a mid-task "wait, why did you do X?" creates a boundary the user may
   not expect); confirmation removes the ambiguity at the moment it
   matters. On cancel, nothing is reverted.
4. **(A) Transcript cleanup semantics**: `/revert` removes reverted turns
   from the transcript — grouped undo must remove all assistant turns in
   the group and keep the user message. Specify and test this explicitly.
5. **Snapshot descriptions**: each snapshot/checkpoint gains a short
   human-readable description of the code state (e.g. what the change did,
   key files touched, test status) — surfaced in `/checkpoints` and the
   undo confirmation. The user: "a snapshot each session sounds like a good
   idea if it can also include some description about the code state for
   each snapshot."
6. **(A) Description generation is batched + best-effort**: generating a
   description requires a model call; `/checkpoints` on a long session
   would fire one call per checkpoint. Batch all pending descriptions into
   **one** model call; on failure (no provider / API down) fall back to raw
   diff stats (files + line counts), never block the command.
7. **Session-scoped safety** (D10): undo only reverts changes Clawde itself
   made this session (content + timing known via `ShadowSnapshot` /
   `FileHistory`); never touches pre-session edits. Edge cases that cannot
   be cleanly undone produce a clear warning instead of partial revert.
8. Keep the existing turn-based `/revert [n|uuid]` semantics intact.

### 4.6 Feature F — Walk-away autonomy (set-it-and-forget-it, audit-corrected)

Per D11/D7: a per-rule opt-in (e.g. "auto-approve for this session") that
lets the agent run unattended. **Audit correction: the session-level
auto-approve permission machinery already exists** — `AutoApproveMode`
(`crates/core/src/auto_mode.rs`) and `PermissionManager.add_session_allow` /
`session_rules` (`crates/core/src/lib.rs:4625`). Phase 4 therefore is:

- **Wire the existing auto-approve into a preset**: a walk-away preset sets
  the auto-approve mode (edits yes, bash per classifier, etc.) + the
  existing session allow rules. No new permission engine.
- **Defer queue** (the genuinely new piece): in autonomous presets,
  `AskUserQuestion` is disabled or routed to the queue instead of suspending
  the loop; things that would normally prompt are put aside into a
  reviewable list (marked in the transcript) and the agent continues on safe
  work; the user reviews/approves the queue on return. (A running loop must
  never block on a question in walk-away mode — that defeats the purpose.)
- **(A) Stale-approval risk**: a deferred action approved later may no
  longer match the code state. Mitigate: snapshot-stamp each deferred item
  and re-validate against the current tree before executing (e.g. the
  target files still exist and weren't changed since).
- **Sanity safeguard**: if the user approves "all for the rest of the
  session," the agent must not do something dangerous to the project — the
  user explicitly asks whether snapshots solve this; the spec's answer is:
  snapshots (Feature E) are the recovery net that makes autonomy
  acceptable, but a hard irreversibility line still applies (D7).
- Over-prompting hazard is the motivating reason: prompt only for important
  things so the user's approvals stay meaningful.

---

## 5. Sequencing

The user picked **Task undo / snapshots (Feature E) first** because it is
the recovery net that makes everything else safe. Proposed order:

1. **Phase 1 — Feature E**: per-prompt undo (`/undo [n]`) + snapshot
   descriptions + session-scoped safety. (Leverages existing `/undo`,
   `/revert`, `/checkpoints`, `ShadowSnapshot`, `FileHistory`; smallest new
   surface. Audit note: the spec now includes the mandatory-confirmation
   and multi-prompt additions from §4.5.)
2. **Phase 2 — Feature A**: named presets framework (lock D5 first — see
   D-table; config schema, `/config mode` picker, persistent + transient
   activation, bundled knobs per §7.1).
3. **Phase 3 — Features C + D**: ranked followups + color/format (global
   output-layer work; independent of A and E — can run in parallel with
   Phase 2, and directly addresses the user's primary "abrupt" complaint).
4. **Phase 4 — Feature F**: walk-away autonomy (defer queue + wiring the
   existing auto-approve machinery per §4.6), built on the Phase 1 recovery
   net.
5. **Feature B** (subtype toggling, interview-style plan mode): design input
   to keep in mind; implement only if it survives contact with Phases 1–4.

## 5a. Phase 1 status — IMPLEMENTED

Feature E (task undo / snapshots) is implemented in
`crates/commands/src/history.rs`:

- **`/undo [<n>] [--yes]`** — reverts the group of assistant turns since the
  n-th most recent prompt (n=1 default). Prompts are user messages *without*
  `ToolResult` blocks, so multi-round agent runs inside one task undo as a
  unit. `--yes` skips the confirmation; without it, `/undo` previews the
  group (prompt text, turn/file counts, per-turn file lists) and requires
  confirmation. `/revert [n|uuid]` is unchanged (single-turn, immediate).
- **Transcript semantics**: on confirm, `snap.revert(patches)` restores
  files and `session_storage::branch_before(first_assistant_uuid)` keeps the
  prompt on the active leaf, branching the work away — same non-destructive
  pattern as `/revert`.
- **Snapshot descriptions**: `/checkpoints` lazily batch-generates
  one-sentence descriptions for checkpoints missing one — a SINGLE model
  call (`resolve_command_provider`, diffs capped at 60K chars) — cached per
  session in `{transcript_dir}/{session}.checkpoints.json`, rendered under
  each checkpoint, raw file stats as the fallback.
- **Tests**: 17 history tests (grouping, tool-result skipping, preview,
  confirmed multi-prompt revert, description render + cache persistence +
  no-provider fallback, out-of-range reporting). `CLAWDE_HOME` mutation
  serialized on the crate `CLAWDE_HOME_LOCK`; full workspace green, clippy
  `-D warnings` clean.

**Phase 1 audit (post-implementation) findings:**

1. `/undo n` out of range said "Nothing to undo" (misleading) — now errors
   "Cannot go back {n} prompts — this session has {count}", with a friendly
   "no prompts yet" for empty sessions. Test: `undo_out_of_range_reports_prompt_count`.
2. Confirmed-path patch collection now filters to `Role::Assistant` — a
   stray non-assistant `snapshot_patch` could otherwise be reverted
   invisibly and skew the preview count.
3. Success message is n-aware ("since that prompt" for n > 1;
   "/undo {n+1} goes back further").
4. If the transcript has no message id at the undo boundary, files still
   restore and a note explains the transcript was not branched (was silent).
5. `checkpoints_lists_turns_newest_first` now runs under `TestHome` so the
   description-cache probe cannot read a real `~/.clawde` or fire a live
   generation call if a provider env key is present in CI.
6. **Deliberate deviation**: the `/undo` confirmation shows per-turn file
   lists, not full diffs (multi-turn diffs can be huge; `/snapshot <n>`
   provides diffs). This reuses the `/checkpoints` data shape per spec
   §4.5 item 3's intent.
7. **Known limitation (shared with `/revert`, machinery-level)**: file
   revert restores the pre-turn snapshot; a file the user edited after the
   agent's turn is overwritten. The mandatory confirmation mitigates this
   by showing the file list first.

## 5b. Phase 2 status — IMPLEMENTED AND AUDITED

Feature A (named presets / modes) is implemented for TUI/CLI sessions:

- Typed `ModeDef` schema with `PlanKnobs`, `AskAmbiguityMode`, and
  `CheckinCadence`; no type-erased configuration map.
- Built-in `default`, `careful`, and `fast` presets. `careful` is the D5
  starter: Plan permission posture, design-decision questions, and milestone
  check-ins.
- Custom JSON modes load from global `~/.clawde/modes/` and project
  `.clawde/modes/`; project definitions override global definitions, which
  override built-ins.
- Persistent selection through `/config set mode <name>`, with
  `/config get mode` and `/config unset mode`; `default` resets the selection.
- Transient `mode:<name>` inline keyword, scoped to the latest user turn and
  taking precedence over the persistent mode. Names may include letters,
  digits, `-`, and `_`.
- Preset config knobs apply at CLI startup while explicit CLI flags win.
  Mode cadence/ambiguity behavior is injected as per-turn prompt guidance
  using the existing `AskUserQuestion` tool; the orchestrator and safety
  rails remain unchanged.
- Presets can never silently select `BypassPermissions`.

**Phase 2 audit findings and fixes:**

1. The original CLI permission assignment unconditionally reset the effective
   settings value to `Default`, which would have defeated a mode's Plan
   posture; it now changes the value only when `--permission-mode` was
   explicitly supplied.
2. Inline mode scanning originally sliced a lowercased `String` at arbitrary
   byte offsets and could panic on UTF-8 prompts; it now scans byte vectors and
   includes a multibyte regression test.
3. Project mode files were initially omitted; the resolver now merges
   built-ins, global modes, and project modes with deterministic precedence.
4. Unknown mode names are warned/ignored at startup and rejected by
   `/config set mode`; no silent fallback changes agent behavior.
5. Mode changes from `/config` apply to subsequent requests; the current
   request's transient mode remains turn-scoped.

**Verification:** core mode/keyword tests, query mode tests, commands config
 tests, workspace tests, workspace check, and strict workspace Clippy pass.

## 6. Out of scope / explicitly not in v1

- Gateway (`/v1/*`) mode exposure (D3) — TUI/CLI only.
- Rewriting the orchestrator or replacing `decide.rs` wholesale — presets
  reuse the existing override seam.
- YOLO mode / full permission bypass (D7 explicitly rejects it).
- Non-TUI surfaces (headless `--print`, scripts) for the UX features.

## 7. Resolved decisions (open questions now locked)

These resolve the former open questions. Each decision is grounded in
verified machinery (see §2.5).

### 7.1 Preset schema — typed `ModeDef` + reuse of typed `Config` fields

**Decision:** a typed `ModeDef` struct with explicit fields, not a flat
`serde_json::Value` map. The repo bans type erasure at typed boundaries, and
the decision-rule knobs are not plain `Config` fields, so a map would smuggle
untyped data into the loop.

```rust
// sketch (shape only; exact fields TBD at implementation)
struct ModeDef {
    name: String,
    label: String,
    description: String,
    // plain Config knobs — all already-typed fields on Config, optional
    // (None = the preset leaves the setting untouched)
    model: Option<String>,
    effort: Option<EffortLevel>,
    permission_mode: Option<PermissionMode>,
    output_style: Option<String>,
    allowed_tools: Option<Vec<String>>,
    // decision-rule knobs — new, mode-specific
    plan: PlanKnobs,
    ask_on_ambiguity: AskAmbiguityMode,
    checkin_cadence: CheckinCadence,
}

// Audit correction: `decide_mode` has no production callers, so a
// threshold knob would do nothing. Plan posture must bind the mechanisms
// that are actually wired: spec_mode, permission_mode, and the /plan
// override. Wiring decide_mode into the loop is a prerequisite IF a
// threshold knob is ever wanted — flag that as a separate change, do not
// assume it exists.
enum PlanKnobs {
    Default,          // current behavior (no spec gate unless user enables it)
    SpecMode,         // enforce the spec-mode write gate (equivalent of /spec on)
    AlwaysPlan,       // permission_mode = Plan (reads allowed, writes gated)
}

enum AskAmbiguityMode { Off, Balanced, AskOnDesign } // §7.3
enum CheckinCadence { Rare, Milestone, EveryTurn }   // §7.2
```

**Custom modes from disk** mirror the established output-styles pattern
(`load_output_styles_dir`): `.json`/`.md` files in `~/.clawde/modes/`
(global) and `.clawde/modes/` (project). The schema is the same `ModeDef`
shape serialized to JSON. This gives the user their "own custom mode outside
of this scope" (Feature B) for free.

### 7.2 Check-in cadence — prompt instruction + existing AskUserQuestion tool

**Decision:** cadence is a *layered* behavior — a prompt-level instruction
plus gating of the existing `AskUserQuestion` tool — **not** new loop pause
primitives in `run_query_loop`. Rationale: the loop is safety-critical;
adding hard pause points there is the one change the user said to avoid
(layered on top, engine unchanged).

- `Rare` — current behavior (no extra narration, no milestone pauses).
- `Milestone` — the system prompt instructs the model to (a) emit a short
  one-paragraph "here's what I'll do" narration before the first write and
  at tool-round boundaries, and (b) pause via `AskUserQuestion` at
  milestones (before first write, after N tool rounds, before wide
  refactors). `AskUserQuestion` is always available in this cadence.
- `EveryTurn` — narrate and optionally ask before every action.

**Honest limitation:** this is model-disciplined, not enforced — the model
follows the instruction rather than a hard loop gate. If real-world
compliance is poor, the follow-up is a hard milestone pause (a loop-level
"pause after tool round N when cadence != Rare" check), sized then.

### 7.3 Ask-on-ambiguity trigger — modulate existing guidance (audit-corrected)

**Decision:** reuse the existing `AskUserQuestionTool` (verified:
`crates/tools/src/ask_user.rs`; interactive-only, suspends the loop, returns
an error headless). No new confidence classifier in v1. **Audit
correction:** the system prompt **already** instructs the model when to ask
(`crates/core/src/system_prompt.rs:630`: "hard to reverse, ask one
clarifying question (AskUserQuestion) before acting"), the tool is already
in the default tool set (`crates/tools/src/lib.rs:952`), and a full TUI
dialog already exists (`crates/tui/src/ask_user_dialog.rs`). §7.3 is
**modulating existing guidance**, not inventing a trigger.

- **Mechanism:** the preset swaps the ask-guidance block in the system
  prompt (stronger for `AskOnDesign`, near-absent for `Off`) and — for
  walk-away presets — routes `AskUserQuestion` to the defer queue instead
  of suspending (§4.6). `AskBalanced`/`AskOnDesign` guide the model to ask
  when:
  1. requirements conflict or are underspecified in a way that changes the
     implementation,
  2. the change is wide-reaching or not cleanly reversible,
  3. multiple plausible designs exist with materially different tradeoffs.
  The model must **not** ask for mechanical/trivial choices (that is the
  over-prompting failure the user called out).
- **Honest limitation** (unchanged): this is model-disciplined, not
  enforced — the model follows the guidance rather than a hard gate.
- **Safety valve:** the defer queue (Feature F) covers the opposite failure
  (not asking enough under autonomy) — the user's "prompted only for
  important things" concern.

### 7.4 Ranked followups — model-generated with a constrained vocabulary

**Decision:** followups + ranks are generated by the model **within the final
turn** (a structured block at the end of the response), then parsed and
rendered. No extra model call, no latency.

- Constrained rank vocabulary (exactly the user's six): `highly_recommended`
  `recommended` `optional` `for_completion` `unimportant` `undesired`.
- The prompt guides the model to attach a one-line reason per followup.
- **Strip before render (audit addition):** the followup block lives inside
  the final turn's response text — it must be parsed **and removed** from
  the content shown to the user, so it never renders as raw text in the
  conversation (a classic leak for structured blocks).
- **Parse fallback:** an unparseable/absent block renders no followups —
  never invent them.
- **`undesired` semantics (§7.6):** shown but de-emphasized.
- A dedicated post-hoc followup-generation step is a later optimization only
  if ranking quality is poor in practice.

### 7.5 Walk-away defer queue — transcript markers + in-memory queue

**Decision:** deferred decisions are written into the transcript as
structured pending markers **and** kept in an in-memory queue for the
renderer. The transcript is already persisted by `session_storage.rs`
(JSONL) and resumable via `/resume`, so deferred items survive restarts with
zero new persistence machinery. Approving/denying a deferred item updates
the marker in place.

### 7.6 "Undesired" rank — shown, de-emphasized

**Decision:** `undesired` followups are **rendered, dimmed/greyed, still
selectable** — the user listed it as a rank class, so they want to see what
the agent considered (it also acts as a subtle "don't do this" signal), just
not prominently. `undesired` items are never auto-selected when any other
rank exists.

### 7.7 Snapshot descriptions — lazy, from the stored diff

**Decision:** descriptions are generated **lazily on demand** (when
`/checkpoints` or `/undo` is invoked), not at snapshot time. Grounding:
`ShadowSnapshot` derives patches/diffs from stored git tree hashes
(`patch(hash)`, `diff(hash)`, `diff_full(from, to)` — verified), so the diff
is available at any later time without the original model context.

- Lazy generation costs **zero tokens on every writing turn** (the expensive
  path), and only when the user actually asks.
- The description summarizes the stored diff (files touched, what changed,
  test status if derivable), not a fresh model reading of the repo.
- Cache the description on the checkpoint entry after first generation;
  background prefetch after a turn completes is a later optimization.

## 7a. Remaining implementation details (not blocking, decided at build time)

- Exact `ModeDef` JSON schema and file layout for custom modes.
- Where the cadence/ask prompt blocks are injected (reuse the
  `custom_output_style_prompt`-style injection point in
  `crates/core/src/system_prompt.rs`, §7 "Output Style"-adjacent section).
- `/config mode` picker UI layout and the transient per-turn override
  keyword(s).

## 8. Risks / guardrails

- **Do not erode the safety core** (permission bypass, sanitization, cap
  enforcement, cancellation) — presets/autonomy layer on top (D7).
- **Over-prompting → approval fatigue** is the motivation for per-rule
  autonomy; the defer queue must be reviewable, not a black hole.
- **Undo must never touch pre-session edits** (D10); warn on unclean
  boundaries instead of partial reverts.
- **Ranked followups and formatting are global** (D8) — do not gate them
  behind the tailored mode.
- Presets must not silently change `permission_mode` to
  `BypassPermissions`; that is the YOLO line.
- **(A) Stale deferred approvals**: a deferred action approved later may
  not match the current tree — snapshot-stamp + re-validate before
  executing (§4.6).
- **(A) `decide_mode` trap**: do not build plan knobs on `decide.rs` — it
  has no production callers; wire the real mechanisms (`spec_mode`,
  permission mode) or first make `decide_mode` actually used (§7.1).

## 9. Audit log

Post-interview audit of this spec against the codebase found and fixed:

1. **`decide.rs` is dead code** (§2.1, §7.1) — `decide_mode`/`decide_verify`
   have no production callers; the original `PlanKnobs::Threshold` knob
   would have silently done nothing. Replaced with knobs on the real
   mechanisms (`spec_mode`, permission mode).
2. **Feature F's permission machinery already exists** (§2.5, §4.6) —
   `AutoApproveMode` + `PermissionManager.session_rules`/`add_session_allow`
   mean Phase 4 is defer-queue + narration + wiring, not a new permission
   engine.
3. **Ask-on-ambiguity guidance already exists** (§2.5, §7.3) —
   `system_prompt.rs:630` + the default `AskUserQuestionTool` + TUI dialog;
   the feature modulates existing guidance.
4. **Image mode is real** (§2.3) — `MODES = ["build", "plan", "image"]`
   in `tui/app.rs:3362`; the spec now says "verified" not "user reports."
5. **Feature E gaps** (§4.5) — added `/undo [n]` multi-prompt undo
   (user's "last two tasks" was unimplementable otherwise), mandatory
   confirmation listing grouped turns, transcript-cleanup semantics, and
   batched best-effort description generation.
6. **Over-complication removed** (§4.1) — dropped the settings-embedded
   `"modes": {...}` map; one source for mode definitions (built-ins +
   disk files mirroring output styles).
7. **Structured-block leak** (§7.4) — ranked-followup block must be
   stripped from rendered content.
8. **Stale deferred approvals** (§4.6, §8) — snapshot-stamp + re-validate.
