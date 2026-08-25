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

**Status: implemented.** `crates/query/src/tool_exec.rs` now holds
`TurnToolState` (error/fatal/hard-fatal counters + check flags + no-progress
signatures, with `observe()` / `clear_turn()`), `ToolExecCtx` (bundled
immutable deps — refactor-loop-health R1), `PreparedTool`, the shared
`prepare_tool_batch` (Phase 1) and `execute_tool_batch` (Phase 2/3). Both
call sites in `lib.rs` now delegate to it; `wrote_files` stays a loop local
(G4) and the provider-path post-steps (auto-lint, file_tracker — D4/D5)
stay at the provider call site (G3). The no-progress detector reads
`turn_state` fields (W5) and `plan_turn_evidence` keeps its individual-arg
signature (G4). B1 landed too: hook/plugin blocks now carry
`PermissionDenied` instead of an unclassified `error_code: None`, so the
no-progress detector treats them as recoverable rather than hard-fatal.
Direct unit tests in `tool_exec.rs` pin the classification, blocked-tool
handling, canonical `is_error`, and `clear_turn` (G1).

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

### Phase C — Budget interplay audit (W4) — executed

Four budgets, all defaults: VerifyPolicy `max_retries = 3` (config.rs),
plan `PLAN_FAILURE_REPLAN_THRESHOLD = 2` + `PLAN_MAX_REPLANS = 3` (plan.rs),
semantic `DEFAULT_SEMANTIC_MAX_ATTEMPTS = 3` / `FIX_MAX_ATTEMPTS = 3` /
`FIX_MAX_TURNS = 5` (config), no-progress `NO_PROGRESS_STOP_STREAK = 3`
window 4 (lib.rs:1006). `SemanticAfterVerifyPolicy` = VerifyPolicy then
SemanticVerifyPolicy (composite), and every `deterministic_failed` turn feeds
BOTH the plan's failure_streak/replan_count AND the no-progress fatal bucket.

Findings:

- **O1 (verify × plan double-count, real)**: each failing-check turn
  increments verify's attempt AND the plan's failure_streak. In interactive
  mode with an active plan, a stubborn test gets 3 verify auto-fix attempts
  (Continue) *plus* a replan recovery turn at ~1732 (failure_streak hits the
  2-turn threshold during verify) *plus* up to 3 replan signals — the budgets
  SUM (~6+ turns on the same failure) instead of sharing one stop authority.
- **O2 (no-progress × plan, real)**: no-progress is plan-blind (lib.rs:1283,
  plain loop-local). With an active plan and no writes between failures
  (a legit replan pattern), no-progress stops at 3 stalled turns while the
  plan still has replan headroom (threshold 2 + 3 replans = up to 5). Writes
  reset no-progress, so the plan budget only gets exercised when the model
  writes between failures.
- **O3 (semantic stacking, structural)**: semantic runs after verify; the
  fixer's writes re-arm verify on the next turn. The plan gate (replan_count
  = 3 → Blocked) is the only hard backstop; the cycle is untested.
- **O4 (mode asymmetry)**: headless mode has no VerifyPolicy — verify's 3
  attempts are interactive-only; headless relies on no-progress (3) + plan
  (5), giving different total headroom for the same task.

Recommendations (C1–C3, from the audit):

- **C1 — plan owns check-failure stops when active**: when an active plan
  exists, exclude check-failure turns from the no-progress streak (the plan's
  failure_streak/replan budget is the stop authority for deterministic
  failures); no-progress keeps tool-loop budgets (unknown/repeated tool
  errors) unconditionally.
- **C2 — cap verify retries by plan headroom**: when a plan is active,
  VerifyPolicy's effective `max_retries` = remaining replan headroom
  (`PLAN_MAX_REPLANS - replan_count`), so a failing test cannot outrun the
  plan fail-close (fixes O1's summing).
- **C3 — pin the semantic cycle**: add an integration test proving a stubborn
  test in semantic mode fail-closes the plan at replan_count = 3 (not before,
  not after), and that verify/semantic budgets cannot exceed the plan gate.
  Document the cycle in `continuation.rs` module docs.
- **C4 (ties to Phase D)**: extend the bounded recovery turn to non-plan
  mode so headless gets verify-equivalent headroom (fixes O4).

Implementation landed:

- **C1 — no-progress defers check failures to an active plan.** New
  `active_plan_replan_headroom()` helper (query/src/lib.rs) loads the
  approved, active plan for the current task and returns
  `PLAN_MAX_REPLANS - replan_count` (floored at 1). The `continue_or_end!`
  macro computes it once; `update_no_progress_state_with_errors` now takes
  `check_failed` + `plan_owns_check_stop`. When an active plan owns the stop
  AND the turn failed a deterministic check, the streak is left untouched
  (fully deferred to the plan's `failure_streak`/`replan_count`). Non-check
  tool loops (e.g. a bad repeated Bash call) still accumulate even under an
  active plan. This fixes O2 — no-progress no longer truncates the plan
  mid-replan.
- **C2 — VerifyPolicy caps retries by plan headroom.** `TurnEndContext` gains
  `plan_replan_headroom: Option<u32>`, wired from the same helper in the
  macro. VerifyPolicy's effective `max_retries` is
  `configured.min(headroom.max(1))`. Fixes O1 — verify's auto-fix attempts
  can no longer outrun the plan fail-close; a stubborn test gets exactly the
  plan's remaining replan budget, then escalates and the plan blocks.
- **C3 — plan fail-close is write-immune.** New core test
  `writes_do_not_reset_failure_streak_when_check_fails` (plan.rs) pins the
  semantic-fixer path: a plan that receives `turn_made_writes &&
  deterministic_failed` every turn still fail-closes at exactly
  `PLAN_MAX_REPLANS`. The fixer's writes reset the loop-local no-progress
  streak but can never mask the plan's replan accounting.
- **C4 — non-plan recovery turn (implements Phase D).** Broadened the
  deterministic-recovery block in `continue_or_end!` (previously active-plan
  only) to fire whenever `!decision.is_continue() && !degradation_turn &&
  deterministic_check_failed`. In default headless mode (`StopPolicy` always
  stops) a bare RunTests/RunLints failure now gets one bounded recovery
  `Continue` telling the model to fix and re-run, instead of a silent stop
  (fixes O4's mode asymmetry). The message branches by whether an active plan
  owns the stop. Safety: the no-progress check runs BEFORE this block, so a
  stuck model that repeats a failing check without writing is still stopped
  at streak 3 — C4 only grants a recovery turn to models that make progress
  (writes reset the streak).
- **Tests**: `default_mode_feeds_failed_check_back_as_recovery_turn` (query,
  C4) — a lone failing RunTests in Default mode grants exactly one recovery
  turn; `writes_do_not_reset_failure_streak_when_check_fails` (core, C3).
  Full Phase C verified: 418 query + 13 core-plan tests pass, clippy `-D
  warnings` clean, fmt clean, `cargo check --workspace` clean.

Phase C is complete (C1–C4). The remaining loop-health backlog is Phase A's
follow-on hygiene (already landed) and any future budget-tuning; see the
Weaknesses section for open items.

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
