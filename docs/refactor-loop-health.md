# Clawde Loop-Health Refactor Plan

Audit of Clawde's loop-health machinery — error classification, deterministic
check handling, no-progress detection, and verification — with a phased
refactor plan to consolidate the duplicated tool-execution pipelines and align
the system with current agent-reliability research (AgentDebug, Aegis) and
industry practice (Aider, Claude Code verification loops).

Status: proposal — no code from this document is committed yet.

## 1. Goals

- Make every loop-health feature (error counting, check observation, telemetry,
  hooks) a **one-site edit** instead of a two-site edit.
- Consolidate the ~12 scattered per-turn health locals into one accumulator.
- Replace fragile content-substring check parsing with structured check
  results from the tools themselves (Aegis: offload deterministic processing
  to the environment).
- Make the interaction between the four overlapping retry budgets explicit,
  tested, and documented.

Scope guardrail: this refactor changes *where* turn-health state is computed,
not the user-facing semantics of verification, no-progress stopping, or plan
gating. Behavioral changes (e.g. unifying the two sites' batch-cancel hook
handling) are deliberate, pinned by tests, and called out per step.

## 2. Current Architecture (verified against the code)

### 2.1 The loop (`crates/query/src/lib.rs`, ~8,300 lines)

The `run_query_loop` body has **two live request paths**, each carrying its own
full Phase 1/2/3 tool executor:

1. **Provider dispatch path** (`if let Some(mut provider) = provider`, line
   ~2348): `provider.create_message_stream(...)` → stream assembly → tool
   execution at ~3260 (collects `tool_use_blocks` from `content_blocks`).
2. **Anthropic path** (`client.create_message_stream(request, handler)` at
   ~3617): stop-reason match at ~4106, `"tool_use"` arm at ~4370 (uses a local
   `PreparedTool` struct).

Both sites run: Phase 1 sequential PreToolUse hooks (permission dialogs /
blocking), Phase 2 parallel dispatch via `run_tool_batch` (bounded by the
cancel token, issue #218), Phase 3 post-hooks + `ToolEnd` events + result-block
assembly. Every loop-health change must currently be made **twice** — e.g. the
`is_recoverable` wiring (`turn_fatal_tool_error_count`) and the
`turn_hard_fatal_error_count` refinement both touched both sites.

**Drift already observed:** the Anthropic-path site guards PostToolUse hooks
with `if !batch_cancelled` (skips external-command hooks when the batch was
aborted); the provider-path site's Phase 3 does not show the same guard. The
two sites also differ in how they build prepared tools (provider path filters
`malformed_tool_calls`; Anthropic path uses the local `PreparedTool` struct).

### 2.2 Turn-health state (scattered locals, ~line 1250–1310)

Declared once per loop iteration and mutated at both execution sites:

- `turn_tool_error_count`
- `turn_fatal_tool_error_count` (fatal = `!is_recoverable()` or missing code)
- `turn_hard_fatal_error_count` (fatal minus deterministic check failures)
- `turn_deterministic_check_run` / `turn_deterministic_check_failed`
- `turn_tool_signatures` (no-progress detector input, joined at turn end)
- `wrote_files`, `turn_diff`, `turn_snapshot`
- `turn_deterministic_check_*` consumed by `plan_turn_evidence` /
  `PlanAdvanceEvidence`

### 2.3 Check classification (`deterministic_check_observation`, ~line 620)

`is_deterministic_check_tool` matches `RunTests` / `RunLints`. Observation is
heuristic: `ToolErrorCode::TestFailed` / `LintFailed` codes OR content
substrings (`"tests passed"`, `"tests failed"`, `"lint issues found"`,
`"timed out"`). Fragile across test frameworks / locales; Aegis prescribes
environment-side deterministic parsing instead.

### 2.4 No-progress detector (`update_no_progress_state_with_errors`, ~line 1030)

7 positional args: `signature, recent, streak, wrote_files, has_diff,
had_tool_errors, had_fatal_tool_errors`. Fatal-error turns collapse to a
stable `<tool-error>` sentinel (changing failure names = one stalled pattern);
recoverable-error turns keep the real signature (new approach resets, identical
repeat accumulates); stops at `NO_PROGRESS_STOP_STREAK = 3`. The stop message
now distinguishes hard-fatal stalls from check stalls
(`turn_deterministic_check_failed`).

### 2.5 Verification machinery (already strong)

- **`VerifyPolicy`** (`crates/query/src/verify.rs`, on by default via
  `VerifyConfig.enabled = true`): after a turn that wrote files, runs the
  project's test suite + linter (auto-detected), auto-fixes up to
  `max_retries = 3`, sandboxed (`direct` / `git worktree` / `container`),
  per-command timeout (180 s), output redirected to a temp file. Verdicts:
  `Pass` / `Fixable` / `Escalate`.
- **`SemanticVerifyPolicy`** / `SemanticAfterVerifyPolicy`
  (`crates/query/src/continuation.rs`, opt-in via `semantic_verify: true`):
  read-only semantic review after deterministic checks pass, fresh-executor
  fixer (G5), tolerant envelope parsing, bounded attempts.
- **Plan gate** (`PlanAdvanceEvidence.deterministic_failed`, ~line 1655):
  failed RunTests/RunLints block plan advancement, consume replan budget, and
  can force a bounded recovery turn even in headless mode (~line 1734).
- **Other loop guards**: `repeat_detector` (escalating reminders),
  goal re-anchoring + instruction pin (request-only), trajectory sanitization
  on upstream switch, turn cap + degradation turn, max_tokens recovery.

## 3. Findings

### Strengths (do not regress)

| Mechanism | Where | Notes |
|---|---|---|
| Execute-and-verify loop | `verify.rs` | Aider auto-test + Claude Code verification loop, on by default |
| Semantic review + fresh fixer | `continuation.rs` | Beyond most agents; opt-in |
| Plan-gate backpressure | `PlanAdvanceEvidence` | Ralph Wiggum pattern |
| Environment-side check parsing | `deterministic_check_observation` | Aegis-aligned (but heuristic — see W3) |
| Recoverable/fatal classification | `ToolErrorCode::is_recoverable` | Wired into no-progress detector |
| No-progress + repeat + goal guards | loop | Research-backed thresholds |

### Weaknesses

- **W1 (HIGH)** — Two live near-duplicate tool-execution pipelines
  (provider path ~3260 vs Anthropic path ~4370). Two-site edits for every
  loop-health change; sites have already drifted (batch-cancel hook guard).
- **W2 (MEDIUM)** — ~12 scattered per-turn health locals, interleaved with
  result assembly and event emission at both sites.
- **W3 (MEDIUM)** — Heuristic content-substring check parsing; should come
  structured from RunTests/RunLints.
- **W4 (LOW-MED)** — Four overlapping retry budgets (VerifyPolicy
  `max_retries`, plan replan budget, semantic `max_attempts`, no-progress
  streak) with untested combined behavior on failing-check trajectories.
- **W5 (LOW)** — `update_no_progress_state_with_errors` takes 7 positional
  args; hard to extend, easy to misorder.
- **W6 (LOW)** — `is_recoverable` binary axis (call vs approach). Documented;
  keep — a tri-state adds complexity without a consumer.

## 3b. Drift audit (executed)

A line-by-line diff of the two tool-execution sites found eight differences.
Three were real bugs/gaps fixed immediately; the rest are documented for the
Phase A unification.

| # | Drift | Severity | Disposition |
|---|-------|----------|-------------|
| D1 | Provider path ran **no PreToolUse/PostToolUse hooks** (config or plugin) — the default path for free + non-Anthropic providers silently skipped all user hooks | HIGH | **Fixed** — mirrored the Anthropic path's Phase 1 pre-hooks (with the hook-veto-before-malformed priority) and Phase 3 post-hooks under the `!batch_cancelled` guard |
| D2 | `total_tool_calls += prepared.len()` existed only on the Anthropic path — goal re-anchoring never advanced on the provider path | HIGH | **Fixed** — added the increment after the provider path's Phase 1 |
| D3 | `is_error` wire form differed: provider path emitted `Some(is_error)` (always `Some`), Anthropic path `if is_error { Some(true) } else { None }` | MEDIUM | **Fixed** — unified to the canonical form (omit on success) |
| D4 | Within-turn `lint_edited_files` auto-lint after writes exists only on the provider path | MEDIUM | Documented — provider-path-only feature; Phase A decides whether both paths get it (VerifyPolicy covers end-of-turn for both) |
| D5 | Auto-context-refresh `file_tracker` read tracking exists only on the provider path | LOW | Documented — provider-path-only; Phase A moves it into the shared core |
| D6 | Malformed-tool-call filtering exists only on the provider path (Anthropic path's stream handler doesn't detect them) | LOW | Likely intentional (different stream handlers); Phase A preserves the filter in the shared prepare step |
| D7 | Prepared-tool struct names differ (`PreparedProviderTool` vs `PreparedTool`) | LOW | Cosmetic — one struct in the shared core |
| D8 | Batch-cancel PostToolUse hook guard existed only on the Anthropic path | MEDIUM | **Fixed as part of D1** — the `!batch_cancelled` guard now wraps post-hooks on both paths |

Pin test: `provider_path_fires_pre_and_post_tool_hooks` drives a `noop_tool`
round through the real loop on the provider path with marker-appending hooks
and asserts both PreToolUse and PostToolUse fired, plus the canonical
`is_error = None` wire form on success.

## 4. Proposed Refactor (phased)

### Phase A — Extract shared tool-execution core (W1, W2, W5)

New module `crates/query/src/tool_exec.rs`:

- `struct TurnToolState` — one accumulator replacing the scattered locals:
  `errors`, `fatal_errors`, `hard_fatal_errors`, `check_run`, `check_failed`,
  `signatures`; method `observe(tool_name, &ToolResult)`; consumed by the
  no-progress detector (kills the 7-arg signature) and by
  `plan_turn_evidence`.
- `fn execute_tool_batch(prepared, tool_ctx, state, event_tx, ...) ->
  (Vec<ContentBlock>, Vec<ToolResult>, cancelled)` — the shared Phase 2/3
  (parallel dispatch, post-hooks, check observation, error counting, event
  emission, result-block assembly).
- Both call sites shrink to Phase 1 (pre-hooks — genuinely divergent) +
  `execute_tool_batch`.

Deliberate unifications (pin each with a test):

1. Batch-cancel PostToolUse hook guard (`if !batch_cancelled`) applied on both
   paths — the Anthropic path's behavior is the intended one.
2. `malformed_tool_calls` filtering (provider path) preserved on both paths.
3. The Anthropic path's `PreparedTool` struct becomes the single
   prepare representation (or both convert to a shared one).

Exit criteria: `cargo check --workspace` clean, clippy `-D warnings` clean,
full `cargo test -p clawde-query` green, plus new tests pinning (1)–(3).

### Phase B — Structured check results (W3)

- `RunTests` / `RunLints` return a machine-readable check summary (pass/fail/
  skip counts) carried in `ToolResult` (structured field or stable prefix),
  so `deterministic_check_observation` reads structured data first and falls
  back to content heuristics only for legacy/third-party tools.
- Removes i18n / framework-fragile substring matching for the built-in tools.

### Phase C — Budget interplay audit (W4)

- Trace one failing-check trajectory through all four budgets; document the
  combined behavior in this doc and in `continuation.rs` / `verify.rs`
  module docs.
- Align responsibilities: plan gate owns write-verification budgets; the
  no-progress detector owns tool-loop budgets; VerifyPolicy owns end-of-turn
  auto-fix retries.
- Add an integration test for the combined path (test fails → verify retries →
  plan replan → no-progress) asserting no budget is double-consumed silently.

### Phase D (optional) — In-turn check failure feed

- The deterministic recovery turn (~line 1734) already feeds RunTests failures
  back to the model for **active plans**; extend it to non-plan mode so a bare
  in-turn `RunTests` failure always gets one bounded fix turn.

## 5. Risks

- **R1 (Phase A): behavioral drift.** The two sites are not bit-identical;
  unifying them may change event ordering or hook firing. Mitigation: the
  deliberate-unification list above is test-pinned first; run the full query
  suite + a live tmux smoke test after landing.
- **R2 (Phase A): scope creep.** `tool_exec.rs` must extract the *shared*
  post-processing only; Phase 1 pre-hooks and stream assembly stay in place.
- **R3 (Phase B): tool result schema.** `ToolResult` is a public type consumed
  by tools, TUI, and tests; add the structured field with a default
  (`None`) so existing tools compile unchanged.
- **R4 (Phase C): over-alignment.** Budgets may legitimately overlap (verify
  retries after a plan is active). The audit may conclude the current behavior
  is correct; the deliverable is then documentation + tests, not a rewrite.

## 6. Suggested Order

1. Phase A (highest leverage — every future loop-health feature becomes a
   one-site edit; Phase B/C slot into the extracted core naturally).
2. Phase B (small, contained in `tool_exec.rs` + the two tools).
3. Phase C (audit + tests; code only if the audit finds a real bug).
4. Phase D (optional, small).
