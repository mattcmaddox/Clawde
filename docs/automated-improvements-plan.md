# Automated Improvements Plan for Clawde

## Executive Summary

After researching Claude Code, Aider, LangChain's latest patterns, and academic work on LLM agents, I've identified **8 improvements that should be automated** in Clawde. These are improvements that can run without user intervention, making Clawde more efficient and autonomous.

---

## 1. AUTO-RESUME FROM SUMMARY (High Impact, Low Effort)

**What**: When resuming a session inactive >1 hour and >100K tokens, automatically compact the full history into a summary before continuing.

**Why automate**: Users shouldn't have to manually run `/compact` after stepping away. The prompt cache expires anyway, so re-caching the full history is wasteful.

**Implementation**:
```rust
// In resume_session or load_session:
if session_inactive_duration > 1 hour && message_tokens > 100_000 {
    auto_compact_on_resume(session, provider);
}
```

**Claude Code reference**: "Resume from a summary" feature

---

## 2. AUTO-TITLE SESSIONS (Medium Impact, Low Effort)

**What**: Generate session titles from the first prompt using a small/fast model (like Haiku).

**Why automate**: Users rarely name sessions manually. Auto-titles make the session picker much more useful.

**Implementation**:
```rust
// After first user message in a session:
if session.title.is_none() {
    let title = generate_title_from_prompt(&first_message, weak_model).await;
    session.title = Some(title);
    save_session(&session).await;
}
```

**Claude Code reference**: "Generated title" feature

---

## 3. AUTO-VERIFY AFTER CODE CHANGES (High Impact, Medium Effort)

**What**: After making code changes, automatically run tests/build and iterate until pass.

**Why automate**: Without verification, "looks done" is the only signal. Auto-verify closes the loop.

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

**Claude Code reference**: "Give Claude a way to verify its work"

---

## 4. AUTO-PRUNE STALE SESSIONS (Low Impact, Low Effort)

**What**: Automatically delete sessions older than N days (configurable, default 30).

**Why automate**: Sessions accumulate indefinitely. Auto-prune prevents storage bloat.

**Implementation**:
```rust
// On startup or periodically:
if config.auto_prune_days > 0 {
    let cutoff = Utc::now() - Duration::days(config.auto_prune_days);
    let stale = list_sessions().await
        .filter(|s| s.updated_at < cutoff && s.id != active_session_id);
    for session in stale {
        delete_session(&session.id).await;
    }
}
```

**Already implemented**: `/session prune [days]` command exists

---

## 5. AUTO-EXTRACT MEMORIES ON COMPACT (Medium Impact, Low Effort)

**What**: When compaction happens, also run memory extraction to preserve key facts.

**Why automate**: Compaction loses detail. Extracting memories before compaction ensures important facts survive.

**Implementation**:
```rust
// In compact_conversation or auto_compact_if_needed:
if config.auto_memory_enabled && should_extract_memories(messages) {
    extract_and_save_memories(messages, working_dir).await;
}
```

**Already partially implemented**: `SessionMemoryExtractor` exists but only runs on 20+ messages

---

## 6. AUTO-SCROLL TOKEN WARNING (Low Impact, Low Effort)

**What**: Show a warning when context window is getting full, before compaction is needed.

**Why automate**: Users don't know context is filling up until performance degrades.

**Implementation**:
```rust
// In calculate_token_warning_state:
if pct_used > 75% && pct_used < 90% {
    // Show warning: "Context 75% full. Consider /compact."
    emit_token_warning(TokenWarningLevel::Caution);
}
```

**Already partially implemented**: `calculate_token_warning_state` exists

---

## 7. AUTO-LEARN FROM CORRECTIONS (High Impact, Medium Effort)

**What**: When user corrects the agent, automatically extract and save the correction as a memory.

**Why automate**: Claude Code's "auto memory" learns from corrections without manual effort.

**Implementation**:
```rust
// Detect correction patterns:
// - "No, I meant X instead of Y"
// - "That's wrong, the correct way is..."
// - "Don't do X, do Y instead"
if detect_correction_pattern(&user_message) {
    let memory = extract_correction(&user_message, &agent_response).await;
    save_to_auto_memory(&memory, working_dir).await;
}
```

**Claude Code reference**: "Auto memory: notes Claude writes itself based on your corrections"

---

## 8. AUTO-SKIP PLANNING FOR SIMPLE TASKS (Low Impact, Low Effort)

**What**: Skip plan mode for obvious simple tasks (typo fixes, single-line changes).

**Why automate**: Planning adds overhead. For trivial tasks, it's wasted tokens.

**Implementation**:
```rust
// In should_enter_plan_mode:
if is_simple_task(&user_message) {
    return false; // Skip plan mode
}

fn is_simple_task(message: &str) -> bool {
    // Heuristics:
    // - Very short message (<50 chars)
    // - Contains "fix typo", "rename", "add log"
    // - No multi-file indicators
    message.len() < 50 || contains_simple_task_patterns(message)
}
```

**Claude Code reference**: "Plan mode is useful, but also adds overhead"

---

## Priority Matrix

| # | Improvement | Impact | Effort | Priority |
|---|---|---|---|---|
| 1 | Auto-Resume from Summary | High | Low | **P0** |
| 2 | Auto-Verify After Code Changes | High | Medium | **P0** |
| 3 | Auto-Extract Memories on Compact | Medium | Low | **P1** |
| 4 | Auto-Learn from Corrections | High | Medium | **P1** |
| 5 | Auto-Title Sessions | Medium | Low | **P1** |
| 6 | Auto-Prune Stale Sessions | Low | Low | **P2** |
| 7 | Auto-Scroll Token Warning | Low | Low | **P2** |
| 8 | Auto-Skip Planning for Simple Tasks | Low | Low | **P2** |

---

## Implementation Order

1. **Phase 1 (Quick Wins)**: Auto-Title, Auto-Prune, Auto-Scroll Warning
2. **Phase 2 (Core Value)**: Auto-Resume from Summary, Auto-Extract Memories
3. **Phase 3 (Advanced)**: Auto-Verify, Auto-Learn from Corrections
4. **Phase 4 (Polish)**: Auto-Skip Planning

---

## Sources

- Claude Code Sessions Documentation
- Claude Code Memory Documentation
- Claude Code Best Practices
- Aider Repository Map Architecture
- LangChain "What is an AI Agent?" (2026)
- Anthropic "Building Effective AI Agents" (2024)
