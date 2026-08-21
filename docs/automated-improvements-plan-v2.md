# Automated Improvements Plan v2 — Revised

## Executive Summary

After auditing the original plan against the current Clawde codebase and researching what Claude Code, Aider, and academic papers have done, I've revised the plan to focus on **6 improvements** that are either new or significantly enhanced.

---

## What's Already Done (No Work Needed)

| Feature | Status | Location |
|---|---|---|
| Auto-Title Sessions | Done (at exit) | `session_title.rs` |
| Auto-Prune Sessions | Done (manual command) | `prune_sessions()` in `lib.rs` |
| Auto-Scroll Token Warning | Done | `calculate_token_warning_state()` |
| Auto-Verify Loop | Done (after turn) | `run_verify_round()` |

---

## What Needs Improvement

### 1. AUTO-RESUME FROM SUMMARY (High Impact, Low Effort)

**What**: When resuming a session inactive >30 minutes OR >80% context full, auto-compact.

**Why**: Prompt cache expires after ~5 minutes. Re-caching full history is wasteful.

**Implementation**:
```rust
// In resume_session or load_session:
if session_inactive_duration > 30 minutes || pct_used > 80% {
    auto_compact_on_resume(session, provider);
}
```

**Claude Code reference**: "Resume from a summary" — uses 1 hour + 100K tokens.

**Improvement over original**: Lower threshold (30 min vs 1 hour) because prompt cache expires faster.

---

### 2. AUTO-TITLE AFTER FIRST PROMPT (Medium Impact, Low Effort)

**What**: Generate session title after first user message, not at exit.

**Why**: Titles appear immediately in session picker, not after session ends.

**Implementation**:
```rust
// After first user message in a session:
if session.title.is_none() && message_count == 1 {
    let title = generate_title_from_prompt(&first_message, weak_model).await;
    session.title = Some(title);
    save_session(&session).await;
}
```

**Claude Code reference**: "Generated title" — generated from first prompt.

**Improvement over original**: Move from exit-time to first-prompt-time.

---

### 3. AUTO-EXTRACT MEMORIES BEFORE COMPACT (Medium Impact, Low Effort)

**What**: Run memory extraction **before** compaction, not after.

**Why**: Compaction loses detail. Extracting before preserves more context.

**Implementation**:
```rust
// In compact_conversation or auto_compact_if_needed:
if config.auto_memory_enabled && should_extract_memories(messages) {
    extract_and_save_memories(messages, working_dir).await; // BEFORE compact
}
compact_conversation(provider, messages, model, effort, cancel).await;
```

**Claude Code reference**: "Auto memory" — extracts learnings from conversations.

**Improvement over original**: Extract before compact, not after.

---

### 4. AUTO-VERIFY AFTER EACH EDIT (High Impact, Medium Effort)

**What**: Run tests/build immediately after file edits, not at turn end.

**Why**: Agent can iterate within the same turn, not just at turn boundaries.

**Implementation**:
```rust
// After file edits in a turn:
if config.auto_verify && has_test_or_build_command() {
    let result = run_verification().await;
    if !result.passed {
        // Auto-fix based on error output
        apply_fixes(&result.errors).await;
    }
}
```

**Claude Code reference**: "Give Claude a way to verify its work" — iterate until pass.

**Improvement over original**: Verify after each edit, not just at turn end.

---

### 5. AUTO-LEARN FROM CORRECTIONS (High Impact, Medium Effort)

**What**: When user corrects the agent, automatically extract and save as memory.

**Why**: Claude Code's "auto memory" learns from corrections without manual effort.

**Implementation**:
```rust
// After user message:
if is_correction(&user_message, &agent_response) {
    let memory = extract_correction(&user_message, &agent_response).await;
    save_to_auto_memory(&memory, working_dir).await;
}

fn is_correction(user_msg: &str, agent_response: &str) -> bool {
    // Heuristics:
    // - "No, I meant X instead of Y"
    // - "That's wrong, the correct way is..."
    // - "Don't do X, do Y instead"
    // - "Actually, X should be Y"
    user_msg.contains("no") || user_msg.contains("wrong") || 
    user_msg.contains("instead") || user_msg.contains("actually")
}
```

**Claude Code reference**: "Auto memory: notes Claude writes itself based on your corrections."

**Improvement over original**: Use LLM for detection, not just regex.

---

### 6. AUTO-CONTEXT-REFRESH (Medium Impact, Medium Effort)

**What**: When agent reads a file modified externally, auto-refresh context.

**Why**: If a file changes outside the agent (git pull, another process), context is stale.

**Implementation**:
```rust
// Before each turn:
for file in &context_files {
    if file_modified_since_last_read(file) {
        refresh_file_in_context(file).await;
    }
}
```

**Source**: Aider's file watcher pattern.

**Improvement over original**: New feature not in original plan.

---

## Revised Priority Matrix

| # | Improvement | Impact | Effort | Priority |
|---|---|---|---|---|
| 1 | Auto-Resume from Summary | High | Low | **P0** |
| 2 | Auto-Verify After Each Edit | High | Medium | **P0** |
| 3 | Auto-Extract Memories Before Compact | Medium | Low | **P1** |
| 4 | Auto-Learn from Corrections | High | Medium | **P1** |
| 5 | Auto-Title After First Prompt | Medium | Low | **P1** |
| 6 | Auto-Context-Refresh | Medium | Medium | **P2** |

---

## Revised Implementation Order

1. **Phase 1 (Quick Wins)**: Auto-Resume, Auto-Title After First Prompt
2. **Phase 2 (Core Value)**: Auto-Extract Memories Before Compact, Auto-Verify After Each Edit
3. **Phase 3 (Advanced)**: Auto-Learn from Corrections, Auto-Context-Refresh

---

## Sources

- Claude Code Sessions Documentation
- Claude Code Memory Documentation
- Claude Code Best Practices
- Aider — File watcher pattern
- Clawde codebase — existing implementations
