# Automated Improvements Plan — Audit & Gaps

## Executive Summary

After auditing the original plan against the current Clawde codebase and researching what Claude Code, Aider, and academic papers have done, I found **6 gaps** and **4 improvements** to the original plan.

---

## Gap Analysis

### Gap 1: Auto-Title Sessions — Already Implemented

**Original plan**: Generate session titles from first prompt using a small model.

**Reality**: `session_title.rs` already does this. It generates titles at session exit using Haiku.

**What's missing**: The title is generated at **exit**, not after the **first prompt**. Claude Code generates titles after the first prompt, which is better for the session picker.

**Fix**: Move title generation to after the first user message, not at exit.

---

### Gap 2: Auto-Prune Sessions — Already Implemented

**Original plan**: Automatically delete sessions older than N days.

**Reality**: `prune_sessions()` exists in `lib.rs`. The `/session prune` command exists.

**What's missing**: Auto-prune on startup. Currently requires manual command.

**Fix**: Add `auto_prune_days` config and run on startup.

---

### Gap 3: Auto-Scroll Token Warning — Already Implemented

**Original plan**: Show warning when context is getting full.

**Reality**: `calculate_token_warning_state()` exists with Critical (95%) and Warning (80%) thresholds.

**What's missing**: The warning is computed but may not be surfaced to the user in all cases.

**Fix**: Verify the warning is displayed in the TUI status bar and CLI output.

---

### Gap 4: Auto-Verify After Code Changes — Partially Implemented

**Original plan**: Run tests/build automatically after edits.

**Reality**: `run_verify_round()` exists. `auto_test` and `auto_lint` config exists. The auto-verify loop exists.

**What's missing**: The auto-verify loop runs **after the turn ends**, not **immediately after file edits**. This means the agent doesn't iterate within the same turn.

**Fix**: Consider adding a "verify after each edit" mode for critical changes.

---

### Gap 5: Auto-Extract Memories on Compact — Partially Implemented

**Original plan**: Extract memories before compaction.

**Reality**: `SessionMemoryExtractor` exists but only runs on 20+ messages or on compact.

**What's missing**: The extractor runs **after** compaction, not **before**. This means some context is already lost.

**Fix**: Run extraction **before** compaction to preserve more detail.

---

### Gap 6: Auto-Learn from Corrections — Not Implemented

**Original plan**: Detect correction patterns and save as memory.

**Reality**: No implementation exists. Claude Code's "auto memory" does this.

**What's missing**: The entire feature.

**Fix**: Implement correction detection and memory extraction.

---

## Improvements to Original Plan

### Improvement 1: Add Auto-Compaction on Resume (Better Implementation)

**Original**: Compact if inactive >1 hour AND >100K tokens.

**Better**: Compact if:
- Inactive >30 minutes (prompt cache expires after ~5 min, so 30 min is generous)
- OR tokens >80% of context window (regardless of time)
- AND user hasn't opted out (some users want full history)

**Source**: Claude Code's "Resume from a summary" — they use 1 hour + 100K tokens.

---

### Improvement 2: Add Auto-Memory on Corrections (Better Detection)

**Original**: Regex pattern matching ("No, I meant X").

**Better**: Use the LLM to detect corrections:
```rust
// After user message:
if is_correction(&user_message, &agent_response) {
    let memory = extract_correction(&user_message, &agent_response).await;
    save_to_auto_memory(&memory, working_dir).await;
}
```

**Source**: Claude Code's auto memory — "Claude records feedback: corrections you give Claude and approaches you confirm."

---

### Improvement 3: Add Auto-Skip Planning (Better Heuristics)

**Original**: Message length <50 chars.

**Better**: Multi-factor scoring:
- Message length
- Whether it references specific files
- Whether it's a question vs. command
- Whether it's a follow-up to an existing task

**Source**: Claude Code — "Plan mode is useful, but also adds overhead... If you could describe the diff in one sentence, skip the plan."

---

### Improvement 4: Add Auto-Context-Refresh (New Feature)

**What**: When the agent reads a file that was modified externally, auto-refresh the context.

**Why**: If a file changes outside the agent (e.g., git pull, another process), the agent's context is stale.

**Implementation**:
```rust
// Before each turn:
if file_modified_since_last_read(&file_path) {
    refresh_file_in_context(&file_path).await;
}
```

**Source**: Aider's file watcher pattern.

---

## Revised Priority Matrix

| # | Improvement | Impact | Effort | Priority | Status |
|---|---|---|---|---|---|
| 1 | Auto-Resume from Summary | High | Low | **P0** | Partially done |
| 2 | Auto-Title After First Prompt | Medium | Low | **P1** | Already done (at exit) |
| 3 | Auto-Verify After Edits | High | Medium | **P0** | Partially done |
| 4 | Auto-Extract Memories Before Compact | Medium | Low | **P1** | Partially done |
| 5 | Auto-Learn from Corrections | High | Medium | **P1** | Not done |
| 6 | Auto-Prune on Startup | Low | Low | **P2** | Partially done |
| 7 | Auto-Context-Refresh | Medium | Medium | **P2** | Not done |
| 8 | Auto-Skip Planning | Low | Low | **P2** | Not done |

---

## Revised Implementation Order

1. **Phase 1 (Quick Wins)**: Auto-Resume, Auto-Prune on Startup, Auto-Title After First Prompt
2. **Phase 2 (Core Value)**: Auto-Extract Memories Before Compact, Auto-Verify After Edits
3. **Phase 3 (Advanced)**: Auto-Learn from Corrections, Auto-Context-Refresh
4. **Phase 4 (Polish)**: Auto-Skip Planning

---

## Sources

- Claude Code Sessions Documentation — "Resume from a summary"
- Claude Code Memory Documentation — "Auto memory"
- Claude Code Best Practices — "Give Claude a way to verify its work"
- Aider — File watcher pattern
- Clawde codebase — `session_title.rs`, `prune_sessions()`, `calculate_token_warning_state()`, `run_verify_round()`, `SessionMemoryExtractor`
