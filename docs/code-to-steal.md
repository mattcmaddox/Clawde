# Code to Steal — Patterns from Aider & Other Projects

## Executive Summary

After researching Aider, Claude Code, and other open source projects, I've identified **6 well-written code patterns** that can fill the gaps in our automated features.

---

## Gap 1: Auto-Resume from Summary

**Source**: Aider's `history.py` — `ChatSummary` class

**Pattern**: Token-budgeted summarization with recursive splitting

```python
# Aider's approach: summarize head, keep tail
def summarize_real(self, messages, depth=0):
    sized = self.tokenize(messages)
    total = sum(tokens for tokens, _ in sized)
    if total <= self.max_tokens and depth == 0:
        return messages
    
    # Split at half max tokens, keep tail
    tail_tokens = 0
    split_index = len(messages)
    half_max_tokens = self.max_tokens // 2
    
    for i in range(len(sized) - 1, -1, -1):
        tokens, _ = sized[i]
        if tail_tokens + tokens < half_max_tokens:
            tail_tokens += tokens
            split_index = i
        else:
            break
    
    # Summarize head, keep tail
    tail = messages[split_index:]
    summary = self.summarize_all(messages[:split_index])
    return summary + tail
```

**Rust adaptation**:
```rust
pub async fn compact_on_resume(
    session: &mut ConversationSession,
    provider: &dyn LlmProvider,
    max_tokens: u64,
) -> bool {
    let total_tokens = estimate_tokens(&session.messages);
    if total_tokens <= max_tokens {
        return false; // No compaction needed
    }
    
    // Split at half max tokens, keep tail
    let half_max = max_tokens / 2;
    let split_at = find_split_index(&session.messages, half_max);
    
    // Summarize head, keep tail
    let head = &session.messages[..split_at];
    let tail = &session.messages[split_at..];
    
    if let Ok(summary) = summarize_messages(provider, head).await {
        session.messages = [summary, tail.to_vec()].concat();
        true
    } else {
        false
    }
}
```

---

## Gap 2: Auto-Verify After Edit

**Source**: Aider's `base_coder.py` — `lint_edited()` method

**Pattern**: Run linter on edited files after each write

```python
# Aider's approach: lint after edit
def lint_edited(self, fnames):
    res = ""
    for fname in fnames:
        if not fname:
            continue
        errors = self.linter.lint(self.abs_root_path(fname))
        if errors:
            res += "\n" + errors + "\n"
    
    if res:
        self.io.tool_warning(res)
    return res

# Called after file edits:
if edited and self.auto_lint:
    lint_errors = self.lint_edited(edited)
    self.auto_commit(edited, context="Ran the linter")
    self.lint_outcome = not lint_errors
```

**Rust adaptation**:
```rust
pub async fn verify_after_edit(
    edited_files: &[PathBuf],
    config: &VerifyConfig,
    working_dir: &Path,
) -> VerifyReport {
    if !config.auto_lint && !config.auto_test {
        return VerifyReport::skipped();
    }
    
    let mut results = Vec::new();
    for file in edited_files {
        if let Some(errors) = lint_file(file, config).await {
            results.push(CheckResult {
                name: format!("lint:{}", file.display()),
                ok: false,
                output: errors,
                ..Default::default()
            });
        }
    }
    
    VerifyReport {
        verdict: if results.iter().all(|r| r.ok) {
            VerifyVerdict::Pass
        } else {
            VerifyVerdict::Fixable
        },
        results,
        ..Default::default()
    }
}
```

---

## Gap 3: Auto-Learn from Corrections

**Source**: Claude Code's auto-memory system

**Pattern**: Detect corrections and save as memories

```python
# Claude Code's approach: detect corrections and save
CORRECTION_PATTERNS = [
    "no, that's wrong",
    "actually, i meant",
    "don't do that",
    "the correct way is",
    "you should have",
]

def is_correction(user_message, agent_response):
    if not agent_response:
        return False
    
    text = user_message.lower()
    return any(pattern in text for pattern in CORRECTION_PATTERNS)

def save_correction_memory(correction, working_dir):
    memory_dir = auto_memory_path(working_dir)
    filename = f"correction_{timestamp()}.md"
    content = f"# User Correction\n\n{correction}\n"
    write_file(memory_dir / filename, content)
```

**Rust adaptation**:
```rust
pub fn is_correction(user_message: &Message, agent_response: Option<&Message>) -> bool {
    if agent_response.is_none() {
        return false;
    }
    
    let text = user_message.get_all_text().to_lowercase();
    
    // More specific patterns to reduce false positives
    let patterns = [
        "no, that's wrong",
        "actually, i meant",
        "don't do that",
        "the correct way is",
        "you should have",
        "that's not what i",
        "i meant to say",
    ];
    
    patterns.iter().any(|pattern| text.contains(pattern))
}
```

---

## Gap 4: Auto-Context-Refresh

**Source**: Aider's file watcher pattern

**Pattern**: Track file modification times and refresh on change

```python
# Aider's approach: track file modifications
class FileWatcher:
    def __init__(self):
        self.file_mtimes = {}
    
    def watch_file(self, fname):
        self.file_mtimes[fname] = os.path.getmtime(fname)
    
    def check_for_changes(self):
        changed = []
        for fname, mtime in self.file_mtimes.items():
            if os.path.getmtime(fname) > mtime:
                changed.append(fname)
                self.file_mtimes[fname] = os.path.getmtime(fname)
        return changed
```

**Rust adaptation**:
```rust
pub struct FileWatcher {
    modifications: HashMap<PathBuf, SystemTime>,
}

impl FileWatcher {
    pub fn new() -> Self {
        Self {
            modifications: HashMap::new(),
        }
    }
    
    pub fn watch_file(&mut self, path: &Path) {
        if let Ok(metadata) = std::fs::metadata(path) {
            if let Ok(modified) = metadata.modified() {
                self.modifications.insert(path.to_path_buf(), modified);
            }
        }
    }
    
    pub fn check_for_changes(&mut self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (path, mtime) in &self.modifications {
            if let Ok(metadata) = std::fs::metadata(path) {
                if let Ok(current_mtime) = metadata.modified() {
                    if current_mtime > *mtime {
                        changed.push(path.clone());
                        self.modifications.insert(path.clone(), current_mtime);
                    }
                }
            }
        }
        changed
    }
}
```

---

## Gap 5: Auto-Extract Memories Before Compact

**Source**: Aider's `ChatSummary.summarize_all()` method

**Pattern**: Extract key facts before summarization

```python
# Aider's approach: summarize with context preservation
def summarize_all(self, messages):
    content = ""
    for msg in messages:
        role = msg["role"].upper()
        if role not in ("USER", "ASSISTANT"):
            continue
        content += f"# {role}\n"
        content += msg["content"]
        if not content.endswith("\n"):
            content += "\n"
    
    # Use summarization prompt
    summarize_messages = [
        dict(role="system", content=prompts.summarize),
        dict(role="user", content=content),
    ]
    
    for model in self.models:
        try:
            summary = model.simple_send_with_retries(summarize_messages)
            if summary is not None:
                return [dict(role="user", content=summary)]
        except Exception as e:
            continue
    
    raise ValueError("summarizer unexpectedly failed for all models")
```

**Rust adaptation**:
```rust
pub async fn extract_memories_before_compact(
    messages: &[Message],
    provider: &dyn LlmProvider,
    working_dir: &Path,
) -> Vec<ExtractedMemory> {
    // Extract key facts from conversation
    let facts = extract_key_facts(messages);
    
    // Save to memory system
    let mut memories = Vec::new();
    for fact in facts {
        if let Ok(memory) = save_memory(&fact, working_dir).await {
            memories.push(memory);
        }
    }
    
    memories
}

fn extract_key_facts(messages: &[Message]) -> Vec<String> {
    let mut facts = Vec::new();
    
    // Look for patterns that indicate important information
    for msg in messages {
        let text = msg.get_all_text();
        
        // User preferences
        if text.contains("i prefer") || text.contains("i like") {
            facts.push(format!("User preference: {}", text));
        }
        
        // Project facts
        if text.contains("the project uses") || text.contains("we use") {
            facts.push(format!("Project fact: {}", text));
        }
        
        // Decisions
        if text.contains("we decided") || text.contains("let's use") {
            facts.push(format!("Decision: {}", text));
        }
    }
    
    facts
}
```

---

## Gap 6: Auto-Title After First Prompt

**Source**: Claude Code's session title generation

**Pattern**: Generate title from first user message

```python
# Claude Code's approach: generate title from first message
def generate_title(first_message, model):
    prompt = f"""Generate a short, descriptive title (max 60 chars) for this coding task:

{first_message[:200]}"""
    
    response = model.simple_send_with_retries([
        dict(role="user", content=prompt)
    ])
    
    title = response.strip().strip('"')
    if len(title) > 60:
        title = title[:60]
    
    return title
```

**Rust adaptation**:
```rust
pub async fn generate_title_from_first_message(
    first_message: &Message,
    provider: &dyn LlmProvider,
) -> Option<String> {
    let text = first_message.get_all_text();
    if text.len() < 10 {
        return None;
    }
    
    let prompt = format!(
        "Generate a short, descriptive title (max 60 chars) for this coding task:\n\n{}",
        text.chars().take(200).collect::<String>()
    );
    
    let request = CreateMessageRequest::builder(provider.model_name(), 80)
        .messages(vec![ApiMessage::user(prompt)])
        .build();
    
    match provider.create_message(request).await {
        Ok(response) => {
            let title = response.content.iter()
                .find_map(|block| {
                    if block.get("type")?.as_str()? == "text" {
                        block.get("text")?.as_str().map(str::to_owned)
                    } else {
                        None
                    }
                })?;
            
            let title = title.lines().next().unwrap_or(&title).trim().trim_matches('"');
            if title.is_empty() || title.len() > 60 {
                None
            } else {
                Some(title.to_string())
            }
        }
        Err(_) => None,
    }
}
```

---

## Implementation Priority

| # | Gap | Source | Effort | Priority |
|---|---|---|---|---|
| 1 | Auto-Resume | Aider history.py | Low | P0 |
| 2 | Auto-Verify | Aider base_coder.py | Medium | P0 |
| 3 | Auto-Corrections | Claude Code auto-memory | Low | P1 |
| 4 | Auto-Context | Aider file watcher | Medium | P1 |
| 5 | Auto-Memory | Aider ChatSummary | Medium | P2 |
| 6 | Auto-Title | Claude Code sessions | Low | P2 |

---

## Sources

- Aider — `history.py`, `base_coder.py`, `linter.py`, `run_cmd.py`
- Claude Code — Sessions documentation, Memory documentation
- LangChain — Agent patterns
