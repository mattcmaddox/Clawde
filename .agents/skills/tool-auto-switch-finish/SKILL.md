# Tool Auto-Switch: Autonomous Completion Protocol

## Purpose

You are an autonomous agent implementing the remaining phases of the tool-auto-switch
improvement plan. Load this skill, then execute the phases below without stopping
until all stop conditions are met.

---

## STOP CONDITIONS — Check after EVERY issue

Stop and report to the user when ANY of these are true:

1. **All P0 + P1 issues are done** (Issues 3, 1b, 1c from the remaining list)
2. **`cargo check --workspace` fails** after 3 fix attempts
3. **`cargo clippy --workspace --all-targets -- -D warnings` fails** after 3 fix attempts
4. **`cargo test --package clawde-query` has failures** after 3 fix attempts
5. **You detect a breaking change** in the codebase that invalidates the guides below

When stopping, report:
- Which issues you completed
- Which issues you attempted but couldn't finish (with error details)
- What the current state of the codebase is (git diff --stat)

---

## PHASE EXECUTION ORDER

Execute these in order. After each phase, run the verification checklist.
If verification passes, proceed to the next phase.

| Phase | Issue | Priority | Description |
|---|---|---|---|
| 1 | 1b | P1 | Session-level model cache |
| 2 | 1c | P1 | Routing telemetry |
| 3 | 5 | P2 | `--force-no-tools` dev flag |
| 4 | 2 | P3 | Session-level switch cache |
| 5 | 7 | P3 | `ModelInfo` event to TUI |

**Skip Issue 3 (startup probe)** — the lazy detection from Issue 6's
`ToolUseTracker` is sufficient. Don't implement the startup probe unless
the user explicitly requests it.

---

## RESEARCH PROTOCOL

**When to research**: Before implementing ANY issue, if the code has changed
since this skill was written, or if the guide's code doesn't match the actual
codebase.

**How to research**:

1. **Read the actual code first** — the guides below were written at a point
   in time. The code may have changed. Always read the current state of the
   file before editing.

2. **Search for patterns** — if you need to find where something is defined:
   ```
   code_search: pattern "function_name" flags="-n -A 10"
   ```

3. **Search online** — if you need to understand an API or library:
   ```
   web_search: query="[provider] [feature] API documentation 2025"
   ```
   Then read the official docs:
   ```
   read_url: url="[official documentation URL]"
   ```

4. **Verify source of truth** — for this codebase:
   - Provider implementations: `crates/api/src/providers/<name>.rs`
   - Model capabilities: `crates/api/src/model_registry.rs`
   - Query loop logic: `crates/query/src/lib.rs`
   - CLI arguments: `crates/cli/src/main.rs`
   - TUI events: `crates/tui/src/app.rs`

5. **Never guess API shapes** — if the guide shows `ProviderRequest { ... }`
   and the struct has changed, read the struct definition first.

---

## DECISION FRAMEWORK

For each phase, follow this loop:

```
1. READ the guide below for the phase
2. READ the actual current code files mentioned in the guide
3. COMPARE guide vs reality:
   a. If they match → implement as written
   b. If they diverge → update your understanding, adapt the implementation
   c. If the guide is wrong → fix the guide, then implement
4. IMPLEMENT the change
5. VERIFY with the checklist
6. If verification fails → go to ERROR RECOVERY
7. If verification passes → proceed to next phase
```

---

## ERROR RECOVERY

### When `cargo check` fails:

```
1. Read the full error message
2. Identify which file/line has the error
3. Read that file (full context around the error)
4. Fix the specific error
5. Run `cargo check --workspace` again
6. If still failing after 3 attempts → STOP and report
```

### When `cargo clippy` warns:

```
1. Read the warning
2. Apply the suggested fix (clippy is usually right)
3. If the fix seems wrong, read the clippy lint documentation
4. Run clippy again
5. If still warning after 2 attempts → add #[allow(clippy::lint_name)] with justification
```

### When tests fail:

```
1. Read the failing test name and assertion
2. Read the code the test is testing
3. Determine: is the test wrong, or is the implementation wrong?
4. If the test is testing old behavior that changed → update the test
5. If the implementation is wrong → fix the implementation
6. Run the specific failing test: cargo test --package clawde-query -- test_name
7. If still failing after 3 attempts → STOP and report
```

### When the guide code doesn't compile:

```
1. The guide was written at a point in time — the code may have changed
2. Read the current struct/function signatures
3. Adapt the guide's code to match current signatures
4. Implement the adapted version
5. Verify
```

---

## VERIFICATION CHECKLIST

Run after EVERY phase:

```bash
cd src-rust/
cargo check --workspace                              # Must pass
cargo clippy --workspace --all-targets -- -D warnings  # Must pass (0 warnings)
cargo test --package clawde-query                    # Must pass (0 failures)
cargo fmt --all                                      # Always format
```

Run after ALL phases complete:

```bash
cd src-rust/
cargo test --package clawde-api                      # Must pass
cargo test --package clawde-core                     # Must pass
```

---

## PHASE 1: Session-Level Model Cache (Issue 1b)

**Goal**: After auto-switch picks a model, remember it for subsequent turns
so we don't re-evaluate every turn.

**Files to read**:
- `src-rust/crates/query/src/lib.rs` — search for `tool_model_switched`

**What to do**:

1. Find the `tool_model_switched` variable declaration (it's a `let mut` before
   the auto-switch block)

2. Add a new variable right next to it:
   ```rust
   let mut cached_tool_model: Option<(String, String)> = None;
   ```

3. After the auto-switch block where `tool_model_switched = true`, cache it:
   ```rust
   if tool_model_switched {
       cached_tool_model = Some((provider_id_str.clone(), model_id_str.clone()));
   }
   ```

4. At the TOP of the auto-switch block (before the `if !caps.tool_calling` check),
   add a cache check:
   ```rust
   // Use cached model from previous auto-switch if still valid
   if let Some((ref cached_pid, ref cached_mid)) = cached_tool_model {
       if provider_id_str == "free" || provider_id_str == cached_pid.as_str() {
           provider_id_str = cached_pid.clone();
           model_id_str = cached_mid.clone();
           // Re-resolve provider...
       }
   }
   ```

5. Invalidate cache when the user explicitly changes model:
   - Search for `/model` command handling
   - Set `cached_tool_model = None` there

**Verification**: cargo check, clippy, test, fmt

---

## PHASE 2: Routing Telemetry (Issue 1c)

**Goal**: Log when the routed model differs from `--tool-model`.

**Files to read**:
- `src-rust/crates/query/src/lib.rs` — search for `Auto-switched to tool-capable model`

**What to do**:

1. Find the `debug!(old_model = ..., new_model = ..., "Auto-switched to tool-capable model")` line

2. Right BEFORE it, add a status event when the routed model differs from `--tool-model`:
   ```rust
   // Emit routing telemetry when --tool-model was overridden
   if config.tool_model.is_some() {
       if let Some(ref tx) = event_tx {
           let _ = tx.send(QueryEvent::Status(format!(
               "Requested '{}', routed to '{}/{}' (reason: {})",
               config.tool_model.as_deref().unwrap_or("?"),
               provider_id_str,
               model_id_str,
               if model_is_unreliable {
                   "model unreliable for tool use"
               } else {
                   "model lacks tool calling capability"
               }
           )));
       }
   }
   ```

3. Also add a `tracing::info!` for post-hoc analysis:
   ```rust
   tracing::info!(
       requested = ?config.tool_model,
       routed_provider = %provider_id_str,
       routed_model = %model_id_str,
       reason = if model_is_unreliable { "unreliable" } else { "no_tool_calling" },
       "routing_telemetry: --tool-model overridden"
   );
   ```

**Verification**: cargo check, clippy, test, fmt

---

## PHASE 3: Force-No-Tools Flag (Issue 5)

**Goal**: `--force-no-tools` dev flag that bypasses auto-switch for testing
the system prompt rebuild path.

**Files to read**:
- `src-rust/crates/cli/src/main.rs` — search for `--tool-model` to see the pattern
- `src-rust/crates/query/src/lib.rs` — search for `force_no_tools` or `tool_model:`
- `src-rust/crates/query/src/agent_tool.rs` — search for `tool_model:` to see the pattern

**What to do**:

1. **Add field to QueryConfig** (lib.rs):
   ```rust
   pub force_no_tools: bool,
   ```

2. **Add default value** — find all `tool_model: None,` lines and add after them:
   ```rust
   force_no_tools: false,
   ```

3. **Add CLI arg** (main.rs) — find the `--tool-model` arg definition and add a new one:
   ```rust
   .arg(
       Arg::new("force_no_tools")
           .long("force-no-tools")
           .help("Dev flag: bypass auto-switch and fire system prompt rebuild path")
           .action(ArgAction::SetTrue),
   )
   ```

4. **Wire CLI arg to QueryConfig** (main.rs) — find where `tool_model` is wired:
   ```rust
   force_no_tools: matches.get_flag("force_no_tools"),
   ```

5. **Add early return in auto-switch** (lib.rs) — find the auto-switch block and add:
   ```rust
   if config.force_no_tools {
       // Dev flag: skip auto-switch to test system prompt rebuild path
   } else if (!caps.tool_calling || model_is_unreliable) && !tools.is_empty() && !degradation_turn {
       // existing auto-switch logic
   }
   ```

6. **Add to agent_tool.rs** — find `tool_model: None,` and add:
   ```rust
   force_no_tools: false,
   ```

**Verification**: cargo check, clippy, test, fmt

---

## PHASE 4: Session-Level Switch Cache (Issue 2)

**Goal**: Avoid re-evaluating auto-switch every turn when nothing changed.

**Files to read**:
- `src-rust/crates/query/src/lib.rs` — search for `tool_model_switched`

**What to do**:

1. Add a cache variable next to `cached_tool_model` (from Phase 1):
   ```rust
   let mut last_switch_eval: Option<(String, String, bool)> = None;
   // (provider_id, model_id, tools_available)
   ```

2. Before the auto-switch block, add a short-circuit:
   ```rust
   if let Some((ref lp, ref lm, lt)) = last_switch_eval {
       if *lp == provider_id_str && *lm == model_id_str && lt == !tools.is_empty() {
           // Same conditions as last turn — skip re-evaluation
           // (The cached result is still valid)
       } else {
           // Conditions changed — need to re-evaluate
       }
   }
   ```

3. After the auto-switch block (whether it fired or not), update the cache:
   ```rust
   last_switch_eval = Some((
       provider_id_str.clone(),
       model_id_str.clone(),
       !tools.is_empty(),
   ));
   ```

4. Invalidate when user changes model:
   ```rust
   last_switch_eval = None;
   ```

**Key constraint**: The auto-switch MUST still fire on the first turn (when
`last_switch_eval` is `None`). The cache only prevents redundant re-evaluation
on subsequent turns.

**Verification**: cargo check, clippy, test, fmt

---

## PHASE 5: ModelInfo Event (Issue 7)

**Goal**: Surface auto-switch decisions in TUI and session log.

**Files to read**:
- `src-rust/crates/query/src/lib.rs` — search for `QueryEvent` enum or `enum QueryEvent`
- `src-rust/crates/tui/src/app.rs` — search for `QueryEvent::` to see how events are handled

**What to do**:

1. **Add event variant** — find the `QueryEvent` enum and add:
   ```rust
   ModelInfo {
       original_model: String,
       switched_model: String,
       reason: String,
       provider: String,
   },
   ```

2. **Emit after auto-switch** — find the `QueryEvent::Status` that says "doesn't
   support tools — switched to" and add a `ModelInfo` event right after:
   ```rust
   let _ = tx.send(QueryEvent::ModelInfo {
       original_model: old_model.clone(),
       switched_model: model_id_str.clone(),
       reason: if model_is_unreliable {
           "model unreliable for tool use".to_string()
       } else {
           "model lacks tool calling capability".to_string()
       },
       provider: provider_id_str.clone(),
   });
   ```

3. **Handle in TUI** — find where `QueryEvent::Status` is handled in the TUI
   and add a case for `ModelInfo`. For now, just log it:
   ```rust
   QueryEvent::ModelInfo { original_model, switched_model, reason, provider } => {
       tracing::info!(
           original = %original_model,
           switched = %switched_model,
           reason = %reason,
           provider = %provider,
           "model_info: auto-switch occurred"
       );
   }
   ```

4. **If the QueryEvent enum has many variants**, search for where it's defined
   and where it's matched. Add the new variant to both.

**Verification**: cargo check, clippy, test, fmt

---

## AFTER ALL PHASES

1. Run the full verification checklist
2. Run `git diff --stat` to show what changed
3. Run `git status` to confirm no unintended changes
4. Report to the user:
   - Which phases completed successfully
   - Total files changed
   - Any issues encountered
   - Whether they want to commit

**DO NOT COMMIT** unless the user explicitly asks.
