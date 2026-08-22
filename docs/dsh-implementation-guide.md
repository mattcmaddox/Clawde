# Deepseek-Harness Patterns: Implementation Guide for Clawde

Three patterns from [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) that would improve Clawde. Each section covers: what it does, the DSH reference, where it fits in Clawde, and step-by-step implementation.

**Source code verified against:** `src-rust/crates/query/src/lib.rs` (query loop), `src-rust/crates/query/src/runner/tools.rs` (tool execution), `src-rust/crates/tools/src/lib.rs` (Tool trait + ToolResult), `src-rust/crates/query/src/compact.rs` (pruner/collapse).

---

## 1. Repeat-Tool-Reminder Guard

### What It Does

Detects when the model makes the same tool call with identical arguments consecutively, and injects escalating reminders into the context. Breaks infinite loops where the model hammers a denied or failing tool call.

### DSH Reference (`packages/guard/repeat-tool-reminder/src/index.ts`)

**Core algorithm:**
- Maintains a per-agent `Chain { key: string, count: number }` via `WeakMap<Agent, Chain>`
- The `key` is `JSON.stringify([toolName, canonicalizedArguments])` — deep key-sort ensures `{a:1, b:2}` and `{b:2, a:1}` produce the same key
- On every tool execution, the chain advances: same key → count++, different key → count=1
- At configured thresholds (default `[3, 5, 8]`), a reminder is injected as a `UserMessage` into `additionalContexts`
- First threshold → gentle reminder ("try a different approach")
- Later thresholds → detailed reminder naming the tool, count, and truncated arguments
- Reset on user interjection (new `agent/pre-step` with user messages)

**Key design decisions:**
- Advisory only — never vetoes or rewrites calls, just enriches context
- Counts even denied calls (a model hammering a denied call is the loop worth breaking)
- Argument preview capped at 500 chars to bound context growth
- Untracked tools (filtered by include/exclude patterns) are transparent — neither counted nor reset

### Where It Fits in Clawde

**Verified integration point:** The tool execution loop in `lib.rs` lines 3163–3240:

```
for (tool_id, tool_name, tool_input) in tool_use_blocks {
    // ... execute tool, collect result ...
    tool_results.push(ContentBlock::ToolResult { ... });
}
messages.push(Message { role: User, content: Blocks(tool_results), ... });
```

The guard should observe each `(tool_name, tool_input)` pair in this loop and, when a threshold is hit, inject the reminder **after** the tool results message — as a separate user message, NOT as a ToolResult (which would require a matching ToolUse the model never made).

**Side effects to handle per-tool:**
- `wrote_files |= is_write_tool(...)` — collect per-tool, OR after
- `turn_tool_signatures.push(...)` — collect per-tool, extend after
- `deterministic_check_observation(...)` — collect per-tool, OR after
- `turn_tool_error_count += 1` — collect per-tool, sum after
- `event_tx.send(ToolStart/ToolEnd)` — fine, mpsc is Send

### Implementation Steps

**Step 1: Add `RepeatCallDetector` struct**

New file: `src-rust/crates/query/src/repeat_guard.rs`

```rust
/// Tracks consecutive identical tool calls to detect loops.
pub(crate) struct RepeatCallDetector {
    last_key: Option<String>,
    count: u32,
    thresholds: Vec<u32>,
    args_preview_chars: usize,
}

impl RepeatCallDetector {
    pub fn new() -> Self {
        Self {
            last_key: None,
            count: 0,
            thresholds: vec![3, 5, 8],
            args_preview_chars: 500,
        }
    }

    /// Record a tool call. Returns a reminder string if a threshold is hit.
    pub fn observe(&mut self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let canonical = canonicalize_args(args);
        let key = format!("{}:{}", tool_name, canonical);

        let count = if self.last_key.as_deref() == Some(&key) {
            self.count + 1
        } else {
            1
        };
        self.last_key = Some(key);
        self.count = count;

        if !self.thresholds.contains(&count) {
            return None;
        }

        if count == self.thresholds[0] {
            Some("You are repeating the exact same tool call with identical \
                  arguments. Carefully analyze the previous result before \
                  calling again: if the task is not complete, try a different \
                  approach or different arguments instead of repeating the call."
                .to_string())
        } else {
            let preview = truncate_args(&canonical, self.args_preview_chars);
            Some(format!(
                "Repeated tool call detected:\n\
                 - tool: {}\n\
                 - consecutive_calls: {}\n\
                 - arguments: {}\n\
                 The repeated calls are not making progress. Do not call \
                 this tool with these exact arguments again.",
                tool_name, count, preview
            ))
        }
    }

    pub fn reset(&mut self) {
        self.last_key = None;
        self.count = 0;
    }
}

fn canonicalize_args(value: &serde_json::Value) -> String {
    serde_json::to_string(&sort_json_value(value)).unwrap_or_default()
}

fn sort_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let sorted: serde_json::Map<String, serde_json::Value> = map.iter()
                .map(|(k, v)| (k.clone(), sort_json_value(v)))
                .collect();
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json_value).collect())
        }
        other => other.clone(),
    }
}

fn truncate_args(canonical: &str, max_chars: usize) -> String {
    if canonical.len() <= max_chars {
        canonical.to_string()
    } else {
        format!("{}… (+{} more chars)", &canonical[..max_chars], canonical.len() - max_chars)
    }
}
```

**Step 2: Wire into the query loop**

In `lib.rs`, add `mod repeat_guard;` and instantiate the detector. Then inside the tool execution loop, observe each call:

```rust
// Before the tool execution loop (~line 3163):
let mut repeat_detector = repeat_guard::RepeatCallDetector::new();

// Inside the for loop, after execute_tool_for_task returns:
if let Some(reminder) = repeat_detector.observe(&tool_name, &tool_input) {
    // Inject as a user message AFTER the tool results
    tool_results.push(ContentBlock::ToolResult {
        tool_use_id: tool_id,
        content: ToolResultContent::Text(
            format!("[SYSTEM NOTICE: {}]", reminder)
        ),
        is_error: Some(false),
    });
}
```

**Important:** The reminder goes as a ToolResult with the same `tool_use_id` as the last tool call. This is a pragmatic choice — the model sees it as additional context attached to the tool result. An alternative is to push it as a separate user message after the tool results block, but that requires restructuring the message assembly.

**Step 3: Reset on user messages**

At the start of each turn (after user input is processed, before the model call), reset the detector:

```rust
repeat_detector.reset();
```

**Step 4: Add to module exports**

Add `mod repeat_guard;` to `src-rust/crates/query/src/lib.rs` and add tests.

---

## 2. Output Spilling

### What It Does

When a tool result exceeds a size threshold, persists the full text to a session-scoped file on disk and replaces the inline result with a bounded head/tail preview plus a locator. The model can retrieve the full content later if needed.

### DSH Reference (`packages/spill/`)

**Architecture:**
- `SpillStore` (abstract service) → `spill-local` (filesystem implementation)
- `spill-policy` (plugin) → `tools/post-execute` listener that decides WHEN to spill
- `output-retention` → `TextRetainer` for head/tail preview generation

**Key design decisions:**
- Spill is best-effort: save failure never turns a successful tool call into an error
- Only plain-text results are spilled (non-text blocks left untouched)
- `read` tool is skipped to avoid `read → spill → read again` loops
- Session-scoped directories under a private temp root (0700 permissions)
- Filenames: random hex prefix + sanitized suggested name (unpredictable, collision-free)
- The model-facing result is: `head (50%) + "..." + tail (50%)` within the byte budget

### Where It Fits in Clawde

**Verified:** The pruner runs at `lib.rs:3709`, which is AFTER the tool execution loop (ends ~3240). So the pruner does handle current-turn tool results. The existing `prune_oversized_tool_results` discards the middle content — spilling would replace that discard with a save-to-disk.

The pruner operates on `&mut [Message]` and modifies `ToolResultContent::Text` in-place. Spilling would extend this: instead of just truncating, save the full text to disk and leave a preview + path reference.

### Implementation Steps

**Step 1: Add `SpillStore` to `crates/query/src/spill.rs` (new module)**

```rust
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub(crate) struct SpillRef {
    pub path: PathBuf,
    pub bytes: usize,
}

pub(crate) struct LocalSpillStore {
    root: PathBuf,
}

impl LocalSpillStore {
    pub fn new() -> Self {
        Self { root: spill_root() }
    }

    pub fn save(
        &self,
        session_id: &str,
        tool_name: &str,
        content: &str,
    ) -> std::io::Result<SpillRef> {
        let dir = self.root.join(format!(
            "session-{}",
            &session_id[..12.min(session_id.len())]
        ));
        std::fs::create_dir_all(&dir)?;

        let filename = format!(
            "{:08x}-{}.txt",
            rand::random::<u32>(),
            sanitize_filename(tool_name)
        );
        let path = dir.join(&filename);
        std::fs::write(&path, content)?;

        Ok(SpillRef { path, bytes: content.len() })
    }
}

fn spill_root() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let base = std::env::temp_dir().join("clawde-spill");
        std::fs::create_dir_all(&base).ok();
        base
    }).clone()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
        .take(64)
        .collect()
}
```

**Step 2: Modify `prune_oversized_tool_results` to optionally spill**

In `compact.rs`, add a `spill_store` parameter:

```rust
pub fn prune_oversized_tool_results(
    messages: &mut [Message],
    config: &ToolResultPrunerConfig,
    spill_store: Option<&crate::spill::LocalSpillStore>,
    session_id: &str,
) -> PruneOutcome {
    // ... existing pruning logic ...
    // When spill_store is Some and text exceeds threshold:
    if let Some(store) = spill_store {
        if let Ok(ref_ref) = store.save(session_name, &tool_name, &text) {
            // Replace with preview + path reference
            *content = ToolResultContent::Text(format!(
                "{}\n\n[Full output spilled to: {} ({} bytes)]",
                head_tail_preview(&text, 1000),
                ref_ref.path.display(),
                ref_ref.bytes
            ));
        }
    }
}
```

**Step 3: Wire into the query loop**

```rust
let spill = spill::LocalSpillStore::new();
let outcome = compact::prune_oversized_tool_results(
    messages,
    &compact::ToolResultPrunerConfig::default(),
    Some(&spill),
    &session_id,
);
```

---

## 3. Parallel Tool Execution

### What It Does

Executes multiple tool calls concurrently when they are classified as "parallel-safe", using a bounded pool. Dramatically reduces latency for independent tools (e.g., multiple file reads, multiple searches).

### DSH Reference (`packages/core/agent-loop/src/tool-calls.ts`)

**Core algorithm:**
- Tools are classified as `exclusive` (barrier) or `parallel` via `executionMode()`
- Parallel calls fill a bounded pool (default `maxParallelToolCalls = 10`)
- Results commit in model order regardless of completion order
- Abort stops new dispatches, drains started calls, records synthetic results for skipped calls

**Key design decisions:**
- Results ALWAYS commit in model order — the model sees tool results in the same order it requested them
- Exclusive calls form barriers: all pending parallel calls must complete before the exclusive call starts
- Scheduler failure stops new dispatches but lets in-flight calls finish
- Skipped calls (after abort) get synthetic error results so the session log stays valid

### Where It Fits in Clawde

**Verified integration point:** The tool execution loop at `lib.rs:3163-3240`:

```
for (tool_id, tool_name, tool_input) in tool_use_blocks {
    // ... execute, side effects ...
}
```

**Key constraint:** `execute_tool_for_task` takes `&[Box<dyn Tool>]` — a borrowed slice. This is NOT `Send`, so we cannot use `tokio::spawn` or `buffer_unordered` (which requires `Send`). We must use `futures::future::join_all` on the same task, which still provides concurrency for I/O-bound tools.

**Side effects to handle per-tool (must be collected, not mutated in parallel):**
- `wrote_files |= is_write_tool(...)` → collect `Vec<bool>`, `.any()` after
- `turn_tool_signatures.push(...)` → collect `Vec<String>`, extend after
- `deterministic_check_observation(...)` → collect `Vec<(bool, bool)`, fold after
- `turn_tool_error_count += 1` → collect count, sum after
- `event_tx.send(ToolStart/ToolEnd)` → fine, `mpsc::Sender` is `Clone + Send`

### Implementation Steps

**Step 1: Classify tools as parallel or exclusive**

```rust
/// Whether a tool is safe to run concurrently with others.
/// Read-only tools that don't mutate shell state or files.
fn is_parallel_safe(tool_name: &str) -> bool {
    matches!(tool_name,
        "read_files" | "code_search" | "glob" | "list_directory"
        | "web_search" | "read_url"
    )
}
```

**Step 2: Partition tool calls into groups**

```rust
fn partition_by_exclusive(
    blocks: Vec<(String, String, serde_json::Value)>,
) -> Vec<Vec<(String, String, serde_json::Value)>> {
    let mut groups = Vec::new();
    let mut current_parallel = Vec::new();

    for block in blocks {
        if is_parallel_safe(&block.1) {
            current_parallel.push(block);
        } else {
            if !current_parallel.is_empty() {
                groups.push(std::mem::take(&mut current_parallel));
            }
            groups.push(vec![block]);
        }
    }
    if !current_parallel.is_empty() {
        groups.push(current_parallel);
    }
    groups
}
```

**Step 3: Replace the sequential loop**

Replace the `for (tool_id, tool_name, tool_input) in tool_use_blocks` loop with:

```rust
use futures::future::join_all;

let tool_groups = partition_by_exclusive(tool_use_blocks);
let mut tool_results = Vec::new();

for group in tool_groups {
    if group.len() == 1 {
        // Single tool — execute directly (no allocation overhead)
        let (tool_id, tool_name, tool_input) = group.into_iter().next().unwrap();
        // ... existing sequential execution logic ...
    } else {
        // Multiple parallel-safe tools — execute concurrently
        let futures: Vec<_> = group.into_iter().map(|(tool_id, tool_name, tool_input)| {
            let tools = tools;          // &[], implements Copy for the reference
            let tool_ctx = tool_ctx;    // &ToolContext, implements Copy
            let event_tx = event_tx.clone();
            let malformed = malformed_tool_calls.contains(&tool_id);
            let is_write = is_write_tool(&tool_name);

            async move {
                // Send ToolStart
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::ToolStart {
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        input_json: tool_input.to_string(),
                    }).await;
                }

                let result = if malformed {
                    ToolResult::error(format!(
                        "Tool call '{}' was not executed: arguments were malformed.",
                        tool_name
                    ))
                } else {
                    execute_tool_for_task(
                        &tool_name, &tool_input, tools, tool_ctx, None
                    ).await
                };

                // Send ToolEnd
                if let Some(ref tx) = event_tx {
                    let _ = tx.send(QueryEvent::ToolEnd {
                        tool_name: tool_name.clone(),
                        tool_id: tool_id.clone(),
                        result: result.content.clone(),
                        is_error: result.is_error,
                        error_code: result.error_code.map(|c| c.as_str().to_string()),
                    }).await;
                }

                (tool_id, tool_name, result, is_write)
            }
        }).collect();

        // Execute all concurrently, results come back in input order
        let results = join_all(futures).await;

        // Commit results in model order (they already are, since join_all preserves order)
        for (tool_id, tool_name, result, is_write) in results {
            wrote_files |= is_write;
            turn_tool_signatures.push(format!(
                "{}:{}",
                tool_name,
                serde_json::to_string(&serde_json::Value::Null).unwrap_or_default()
            ));
            let (check_run, check_failed) = deterministic_check_observation(&tool_name, &result);
            turn_deterministic_check_run |= check_run;
            turn_deterministic_check_failed |= check_failed;
            if result.is_error {
                turn_tool_error_count += 1;
            }
            tool_results.push(ContentBlock::ToolResult {
                tool_use_id: tool_id,
                content: ToolResultContent::Text(result.content),
                is_error: Some(result.is_error),
            });
        }
    }
}
```

**Important note on `turn_tool_signatures`:** The current code serializes `tool_input` for each tool. In the parallel path, we'd need to capture `tool_input` before the async move. The simplest fix is to serialize before spawning the future:

```rust
let input_json = serde_json::to_string(&tool_input).unwrap_or_default();
// ... pass input_json into the async block ...
// In the result handler:
turn_tool_signatures.push(format!("{}:{}", tool_name, input_json));
```

**Step 4: Verify `futures` is in dependencies**

Already confirmed: `src-rust/crates/query/Cargo.toml` line 14 has `futures = { workspace = true }`.

**Step 5: Handle the `active_task_id` parameter**

`execute_tool_for_task` takes `active_task_id: Option<&str>`. For parallel execution, pass `None` (no active task context) or clone the string. The current code passes `active_task_id.as_deref()` — for parallel, each future would need its own copy. Since it's `Option<&str>`, we'd need to convert to `Option<String>` or use `Arc<str>`.

**Step 6: Tests**

- Two parallel reads execute concurrently (verify with timing)
- A write after reads blocks until reads complete
- Mixed parallel+exclusive grouping works correctly
- Event ordering is preserved (ToolStart before ToolEnd for each tool)
- Malformed tool calls still produce error results in parallel

---

## Summary: Priority and Effort

| Feature | Impact | Effort | Key Files |
|---|---|---|---|
| Repeat-tool-reminder | High — breaks infinite loops | Low (~100 lines) | New `repeat_guard.rs`, `lib.rs` |
| Parallel tools | High — reduces latency 2-5x | Medium (~150 lines) | `lib.rs` tool execution loop |
| Output spilling | Medium — preserves pruned content | Medium (~200 lines) | New `spill.rs`, `compact.rs`, `lib.rs` |

**Recommended implementation order:**
1. **Repeat-tool-reminder** — smallest change, highest immediate value
2. **Parallel tools** — moderate change, high latency improvement
3. **Output spilling** — largest change, useful for long sessions

### Critical Notes for Implementation

1. **Don't use `buffer_unordered`** — `execute_tool_for_task` takes `&[Box<dyn Tool>]` which is not `Send`. Use `futures::future::join_all` instead (concurrent on same task, not parallel across threads).

2. **Repeat-tool-reminder injects as ToolResult, not UserMessage** — Clawde's message model doesn't have DSH's `additionalContexts` concept. The pragmatic approach is to attach the reminder to the last tool result's `tool_use_id`. A cleaner approach would be to push a separate user message after the tool results block.

3. **Spilling extends the existing pruner** — don't create a separate pass. Modify `prune_oversized_tool_results` to accept an optional `SpillStore` parameter.

4. **Side effects in parallel execution** — all mutable state (`wrote_files`, `turn_tool_signatures`, etc.) must be collected per-tool and aggregated after `join_all` completes, not mutated inside the futures.
