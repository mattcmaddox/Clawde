# Automated Features Audit — Gaps & Issues

## Executive Summary

After auditing the 6 recently implemented automated features, I found **8 gaps** and **4 issues** that need to be addressed.

---

## Feature 1: Auto-Resume from Summary

**Status**: Partially implemented

**What works**:
- Detects stale sessions (>30 min inactive or >80% context full)
- Shows message suggesting compaction

**Gaps**:
1. **No actual compaction** — Only shows a message, doesn't auto-compact
2. **No TUI integration** — Message only prints to stdout, not shown in TUI status bar
3. **No config option** — No way to disable or configure thresholds

**Fix needed**:
```rust
// Should auto-compact, not just suggest
if clawde_core::history::should_auto_compact_on_resume(&session, 128_000) {
    // Actually compact the session
    let provider = /* get provider */;
    clawde_core::history::auto_compact_on_resume(&mut session, provider, 128_000).await;
}
```

---

## Feature 2: Auto-Title After First Prompt

**Status**: Partially implemented

**What works**:
- Generates title after first user message
- Updates session title and terminal title

**Gaps**:
4. **No error handling** — If API call fails, title generation fails silently
5. **No fallback** — If Haiku is unavailable, no fallback to other models
6. **No deduplication** — Could generate multiple titles if user sends multiple messages quickly

**Fix needed**:
```rust
// Add error handling and fallback
match generate_title_after_first_prompt(...).await {
    Some(title) => { /* update title */ }
    None => { /* use fallback: truncated first message */ }
}
```

---

## Feature 3: Auto-Extract Memories Before Compact

**Status**: Not implemented (placeholder only)

**What works**:
- Logs that memory extraction is recommended

**Gaps**:
7. **No actual extraction** — Only logs, doesn't extract memories
8. **No API client** — Can't call API from compact function (circular dependency)

**Fix needed**:
- Move extraction to CLI layer where API client is available
- Or use existing `SessionMemoryExtractor` in the query loop

---

## Feature 4: Auto-Verify After Each Edit

**Status**: Not implemented (placeholder only)

**What works**:
- Comment indicates where to add verification

**Gaps**:
9. **No actual verification** — Only a comment, no code
10. **No config integration** — Doesn't use `auto_test`/`auto_lint` from config

**Fix needed**:
```rust
// After file writes, run verification
if wrote_files && config.auto_verify {
    let verify_config = /* get from config */;
    let _ = run_verify_after_edit(&verify_config, &tool_ctx.working_dir);
}
```

---

## Feature 5: Auto-Learn from Corrections

**Status**: Implemented but has issues

**What works**:
- Detects correction patterns
- Saves corrections as memories

**Gaps**:
11. **False positives** — Patterns like "no" and "not" trigger on non-corrections
12. **No context awareness** — Doesn't check if correction is relevant to agent's response
13. **No deduplication** — Could save duplicate corrections

**Fix needed**:
```rust
// Add more specific patterns and context awareness
const CORRECTION_PATTERNS: &[&str] = &[
    "no, that's wrong",
    "actually, i meant",
    "don't do that",
    // More specific patterns
];
```

---

## Feature 6: Auto-Context-Refresh

**Status**: Implemented but not wired

**What works**:
- `FileModificationTracker` struct exists
- `process_context_refresh` function exists

**Gaps**:
14. **Not wired into query loop** — Only logs status, doesn't actually refresh
15. **No file tracking** — Doesn't track which files are in context
16. **No integration with tools** — Doesn't refresh files read by tools

**Fix needed**:
```rust
// Wire into query loop
let modified = process_context_refresh(&mut tracker, &context_files).await;
for (path, content) in modified {
    // Update context with new content
}
```

---

## Priority Matrix

| # | Gap | Impact | Effort | Priority |
|---|---|---|---|---|
| 1 | No actual compaction on resume | High | Low | P0 |
| 2 | No TUI integration for resume | Medium | Low | P1 |
| 3 | No config option for resume | Low | Low | P2 |
| 4 | No error handling for title | Medium | Low | P1 |
| 5 | No fallback for title | Low | Low | P2 |
| 6 | No deduplication for title | Low | Low | P2 |
| 7 | No actual memory extraction | High | High | P0 |
| 8 | No API client in compact | High | High | P0 |
| 9 | No actual verification | High | Medium | P0 |
| 10 | No config integration for verify | Medium | Low | P1 |
| 11 | False positives in correction | Medium | Medium | P1 |
| 12 | No context awareness in correction | Medium | Medium | P1 |
| 13 | No deduplication in correction | Low | Low | P2 |
| 14 | Context refresh not wired | High | Medium | P0 |
| 15 | No file tracking | Medium | Medium | P1 |
| 16 | No tool integration | Medium | High | P2 |

---

## Recommended Fixes (P0)

1. **Implement actual compaction on resume** — Use existing `compact_conversation` function
2. **Implement actual memory extraction** — Wire `SessionMemoryExtractor` into query loop
3. **Implement actual verification** — Wire `run_verify_after_edit` into tool execution
4. **Wire context refresh** — Track files and refresh on modification

---

## Sources

- Clawde codebase — `cli/src/main.rs`, `query/src/lib.rs`, `query/src/compact.rs`
- Clawde codebase — `query/src/verify.rs`, `query/src/correction_detector.rs`
- Clawde codebase — `query/src/context_refresh.rs`, `query/src/session_title.rs`
