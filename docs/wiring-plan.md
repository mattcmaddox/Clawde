# Wiring Plan — Integrating New Functions into Main Codebase

## Executive Summary

This plan outlines how to wire 8 new functions into the main codebase. Each function needs to be integrated at specific points in the CLI and query loop.

---

## Functions to Wire

| # | Function | Source File | Target Location |
|---|---|---|---|
| 1 | `compact_on_resume` | `compact.rs` | `cli/src/main.rs` (session resume) |
| 2 | `extract_before_compact` | `session_memory.rs` | `query/src/lib.rs` (before compaction) |
| 3 | `lint_edited_files` | `verify.rs` | `query/src/lib.rs` (after file writes) |
| 4 | `run_verify_after_edit` | `verify.rs` | `query/src/lib.rs` (after file writes) |
| 5 | `process_correction` | `correction_detector.rs` | `query/src/lib.rs` (after user messages) |
| 6 | `process_context_refresh` | `context_refresh.rs` | `query/src/lib.rs` (before each turn) |
| 7 | `watch_file` | `context_refresh.rs` | `query/src/lib.rs` (when files are read) |
| 8 | `check_for_changes` | `context_refresh.rs` | `query/src/lib.rs` (before each turn) |
| 9 | `generate_title_after_first_prompt` | `session_title.rs` | `cli/src/main.rs` (after first message) |

---

## Integration Points

### 1. Auto-Resume from Summary (`compact_on_resume`)

**Current state**: Only shows a message suggesting compaction.

**Target**: Actually compact the session on resume.

**Location**: `cli/src/main.rs` line 3007

**Implementation**:
```rust
// Auto-compact on resume if session is stale or large
if clawde_core::history::should_auto_compact_on_resume(&session, 128_000) {
    // Get provider for compaction
    let provider = /* resolve provider from config */;
    let cancel = CancellationToken::new();
    
    // Use Aider's recursive summarization approach
    match clawde_query::compact::compact_on_resume(
        provider.as_ref(),
        &session.messages,
        &session.model,
        128_000, // max tokens
        None, // effort
        &cancel,
    ).await {
        Ok(compacted) => {
            session.messages = compacted;
            println!("Session compacted automatically.");
        }
        Err(e) => {
            println!("Auto-compaction failed: {}. Consider running /compact.", e);
        }
    }
}
```

**Dependencies**:
- Need to resolve provider from config before session resume
- Need to handle errors gracefully

---

### 2. Auto-Extract Memories Before Compact (`extract_before_compact`)

**Current state**: Only logs that memory extraction is recommended.

**Target**: Actually extract memories before compaction.

**Location**: `query/src/lib.rs` (before `auto_compact_if_needed`)

**Implementation**:
```rust
// Before compaction, extract memories if enabled
if config.auto_memory_enabled.unwrap_or(true) {
    if let Some(api_client) = /* get API client */ {
        let extractor = SessionMemoryExtractor::new(model, 1000);
        let mut state = SessionMemoryState::default();
        
        if let Ok(memories) = extractor.extract_before_compact(
            messages,
            &tool_ctx.working_dir,
            &api_client,
        ).await {
            // Persist memories before compaction
            let memory_path = auto_memory_path(&tool_ctx.working_dir);
            if let Err(e) = SessionMemoryExtractor::persist(&memories, &memory_path).await {
                warn!("Failed to persist memories before compaction: {}", e);
            }
        }
    }
}
```

**Dependencies**:
- Need API client available in query loop
- Need to check if auto-memory is enabled

---

### 3. Auto-Verify After Edit (`lint_edited_files` + `run_verify_after_edit`)

**Current state**: Only a comment indicating where to add verification.

**Target**: Run verification after file writes.

**Location**: `query/src/lib.rs` (after `wrote_files |= is_write_tool(&tool_name)`)

**Implementation**:
```rust
// After file writes, run verification
if wrote_files && config.auto_verify.unwrap_or(true) {
    // Get list of files that were written
    let edited_files = get_edited_files_from_tool_results(&tool_results);
    
    // Run lint on edited files (Aider's lint_edited pattern)
    if config.auto_lint.unwrap_or(true) {
        let lint_report = lint_edited_files(
            &edited_files,
            &config.verify,
            &tool_ctx.working_dir,
        );
        
        if lint_report.verdict == VerifyVerdict::Fixable {
            // Send lint errors to model for fixing
            if let Some(ref tx) = event_tx {
                let _ = tx.send(QueryEvent::Verify(lint_report));
            }
        }
    }
    
    // Run full verification if configured
    if config.auto_test.unwrap_or(false) {
        let verify_report = run_verify_after_edit(
            &config.verify,
            &tool_ctx.working_dir,
        );
        
        if let Some(ref tx) = event_tx {
            let _ = tx.send(QueryEvent::Verify(verify_report));
        }
    }
}
```

**Dependencies**:
- Need to track which files were written in current turn
- Need VerifyConfig available in QueryConfig

---

### 4. Auto-Learn from Corrections (`process_correction`)

**Current state**: Implemented but not wired.

**Target**: Detect corrections and save as memories.

**Location**: `query/src/lib.rs` (after user messages are processed)

**Implementation**:
```rust
// Auto-learn from corrections: detect user corrections and save as memories
if let Some(last_user_msg) = messages.iter().rev().find(|m| m.role == Role::User) {
    let agent_response = messages.iter().rev().find(|m| m.role == Role::Assistant);
    
    if correction_detector::is_correction(last_user_msg, agent_response) {
        let working_dir = &tool_ctx.working_dir;
        
        if let Some(memory) = correction_detector::extract_correction_memory(
            last_user_msg,
            agent_response,
        ) {
            if let Err(e) = correction_detector::save_correction_memory(&memory, working_dir).await {
                warn!("Failed to save correction memory: {}", e);
            } else {
                debug!("Saved correction memory");
            }
        }
    }
}
```

**Dependencies**:
- Already implemented, just needs to be wired

---

### 5. Auto-Context-Refresh (`process_context_refresh` + `watch_file` + `check_for_changes`)

**Current state**: Implemented but not wired.

**Target**: Track files and refresh on modification.

**Location**: `query/src/lib.rs` (before each turn)

**Implementation**:
```rust
// Auto-context-refresh: check for external file modifications
let mut file_tracker = context_refresh::FileModificationTracker::new();

// Watch files that are in context
for file in &context_files {
    context_refresh::watch_file(&mut file_tracker, file);
}

// Check for changes before each turn
let modified_files = context_refresh::check_for_changes(&mut file_tracker);
if !modified_files.is_empty() {
    // Refresh modified files
    let refreshed = context_refresh::process_context_refresh(
        &mut file_tracker,
        &context_files,
    ).await;
    
    // Update context with refreshed content
    for (path, content) in refreshed {
        update_file_in_context(&path, &content);
    }
    
    if let Some(ref tx) = event_tx {
        let _ = tx.send(QueryEvent::Status(format!(
            "Refreshed {} modified file(s)",
            refreshed.len()
        )));
    }
}
```

**Dependencies**:
- Need to track which files are in context
- Need to integrate with file reading tools

---

### 6. Auto-Title After First Prompt (`generate_title_after_first_prompt`)

**Current state**: Implemented but not wired.

**Target**: Generate title after first user message.

**Location**: `cli/src/main.rs` (after first message)

**Implementation**:
```rust
// Auto-title after first prompt: generate AI title if this is the first message
if session.messages.is_empty() && session.title.is_none() {
    let first_msg = Message::user(input.clone());
    let title_config = clawde_query::session_title::SessionTitleConfig::default();
    let cancel_token = CancellationToken::new();
    
    if let Some(title) = clawde_query::session_title::generate_title_after_first_prompt(
        &first_msg,
        client.as_ref(),
        &title_config,
        cancel_token,
    ).await {
        session.title = Some(title.clone());
        cmd_ctx.session_title = Some(title.clone());
        clawde_tui::update_terminal_title(Some(&title));
    }
}
```

**Dependencies**:
- Already implemented, just needs to be wired

---

## Implementation Order

| # | Function | Effort | Priority |
|---|---|---|---|
| 1 | `process_correction` | Low | P0 |
| 2 | `generate_title_after_first_prompt` | Low | P0 |
| 3 | `compact_on_resume` | Medium | P0 |
| 4 | `lint_edited_files` + `run_verify_after_edit` | Medium | P1 |
| 5 | `process_context_refresh` | Medium | P1 |
| 6 | `extract_before_compact` | High | P2 |

---

## Testing Strategy

1. **Unit tests**: Each function should have unit tests
2. **Integration tests**: Test wiring in the query loop
3. **Manual tests**: Run clawde and verify each feature works

---

## Risk Assessment

| # | Risk | Mitigation |
|---|---|---|
| 1 | Performance impact | Add config options to disable features |
| 2 | Error handling | Graceful degradation on failures |
| 3 | Compatibility | Test with existing sessions |
| 4 | Memory usage | Limit tracked files |

---

## Sources

- Clawde codebase — `cli/src/main.rs`, `query/src/lib.rs`
- Aider — `base_coder.py`, `history.py`, `linter.py`
- Claude Code — Sessions documentation, Memory documentation
