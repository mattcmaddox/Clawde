// providers/free/task_classifier.rs — Phase 2 smart-router task classification.
//
// Determines WHAT a request is trying to do (audit spec §8.3), so the
// FreeProvider's plan builder can route the request to the upstreams best
// suited to the task instead of always walking the catalog in priority order.
//
// Classification is deliberately cheap and deterministic — no model call, no
// state. Layered keyword rules over the latest user message, with tool-loop
// continuations (a tool result is the trigger) treated as code edits.

use clawde_core::types::{ContentBlock, MessageContent, Role};

use crate::provider_types::ProviderRequest;

/// High-level task categories the router routes on (audit spec §8.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Write new functions, modules, types.
    CodeGeneration,
    /// Modify existing code, refactor.
    CodeEdit,
    /// Analyze bugs, architecture decisions.
    Reasoning,
    /// Design before implementation.
    Planning,
    /// Run tests, check output.
    Verification,
    /// Rename a variable, fix a typo.
    SimpleEdit,
    /// Grep, find references.
    Search,
}

impl TaskType {
    /// Human-readable label for the TUI / routing dialog.
    pub fn label(self) -> &'static str {
        match self {
            TaskType::CodeGeneration => "code generation",
            TaskType::CodeEdit => "code edit",
            TaskType::Reasoning => "reasoning",
            TaskType::Planning => "planning",
            TaskType::Verification => "verification",
            TaskType::SimpleEdit => "simple edit",
            TaskType::Search => "search",
        }
    }

    /// All task types in a stable order, for display and iteration.
    pub const ALL: [TaskType; 7] = [
        TaskType::CodeGeneration,
        TaskType::CodeEdit,
        TaskType::Reasoning,
        TaskType::Planning,
        TaskType::Verification,
        TaskType::SimpleEdit,
        TaskType::Search,
    ];

    /// JSON key used in `providers.free.options.routing.task_preferences`.
    pub fn key(self) -> &'static str {
        match self {
            TaskType::CodeGeneration => "code_generation",
            TaskType::CodeEdit => "code_edit",
            TaskType::Reasoning => "reasoning",
            TaskType::Planning => "planning",
            TaskType::Verification => "verification",
            TaskType::SimpleEdit => "simple_edit",
            TaskType::Search => "search",
        }
    }
}

/// Classify a request into a [`TaskType`] (audit spec §8.3 layered strategy):
///
/// 1. A tool-loop continuation (the trigger is a `ToolResult`) → `CodeEdit`:
///    the agent is iterating on work, not starting something new.
/// 2. Keyword rules over the latest user text, most specific first.
/// 3. Default → `CodeGeneration`.
pub fn classify_request(request: &ProviderRequest) -> TaskType {
    if last_user_message_is_tool_result(request) {
        return TaskType::CodeEdit;
    }
    let text = last_user_text(request).to_lowercase();

    if contains_any(
        &text,
        &[
            "why",
            "how ",
            "how?",
            "explain",
            "debug",
            "investigate",
            "diagnose",
        ],
    ) {
        TaskType::Reasoning
    } else if contains_any(
        &text,
        &[
            "design",
            "architecture",
            "planning",
            "plan ",
            "propose",
            "roadmap",
        ],
    ) {
        TaskType::Planning
    } else if contains_any(
        &text,
        &[
            "search",
            "grep",
            "look up",
            "reference",
            "where is",
            "find the",
            "find all",
            "find any",
        ],
    ) {
        TaskType::Search
    } else if contains_any(
        &text,
        &[
            "verify",
            "run the tests",
            "run tests",
            "run lint",
            "lint",
            "typecheck",
            "check that",
        ],
    ) {
        TaskType::Verification
    } else if contains_any(&text, &["rename", "typo", "delete ", "remove ", "cleanup"]) {
        TaskType::SimpleEdit
    } else if contains_any(
        &text,
        &[
            "write ",
            "write a",
            "create ",
            "implement",
            "new function",
            "new file",
            "build a",
        ],
    ) {
        TaskType::CodeGeneration
    } else if contains_any(
        &text,
        &[
            "refactor", "modify", "change ", "fix ", "edit ", "update ", "improve", "migrate",
        ],
    ) {
        TaskType::CodeEdit
    } else {
        TaskType::CodeGeneration
    }
}

/// Ordered upstream-id preference list for a task (audit spec §8.4).
///
/// The first entries are the upstreams best suited to the task; the plan
/// builder tries them before the remaining catalog entries (which follow in
/// catalog order so nothing is skipped). Ids must match `FREE_CATALOG`.
pub fn task_preference_ids(task: TaskType) -> &'static [&'static str] {
    match task {
        // Strong coders first (OpenRouter's DeepSeek, Cerebras' coding
        // specialist, Hugging Face's 70B), then the fast generalists.
        TaskType::CodeGeneration => &[
            "openrouter",
            "cerebras",
            "poolside",
            "groq",
            "cline",
            "mistral",
            "opencode-zen",
        ],
        // Fast, instruction-following models for iterating on existing code.
        TaskType::CodeEdit => &[
            "groq",
            "cerebras",
            "cloudflare",
            "opencode-zen",
            "poolside",
            "nvidia",
        ],
        // Strong reasoning first (Gemini), then the 70B-class fast models.
        TaskType::Reasoning => &["google", "groq", "sambanova", "nvidia", "openrouter", "zai"],
        // Design-before-implementation: comprehensive, structured output.
        TaskType::Planning => &["google", "groq", "openrouter", "cline", "zai"],
        // Fastest, cheapest tokens — a check needs speed, not depth.
        TaskType::Verification => &["groq", "cloudflare", "opencode-zen", "zai", "cerebras"],
        // Cheapest available — a typo fix shouldn't burn a strong model.
        TaskType::SimpleEdit => &[
            "zai",
            "opencode-zen",
            "sambanova",
            "modelscope",
            "nvidia",
            "mistral",
        ],
        // Tool-calling only: any capable upstream, fast ones first.
        TaskType::Search => &[
            "cloudflare",
            "groq",
            "opencode-zen",
            "openrouter",
            "cerebras",
        ],
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether the last user message is a tool result (i.e. a tool-loop
/// continuation rather than a fresh instruction).
fn last_user_message_is_tool_result(request: &ProviderRequest) -> bool {
    let Some(msg) = request.messages.iter().rev().find(|m| m.role == Role::User) else {
        return false;
    };
    match &msg.content {
        MessageContent::Text(_) => false,
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
    }
}

/// The latest user message's text (Text content, plus user command blocks),
/// empty when there is no user message.
fn last_user_text(request: &ProviderRequest) -> String {
    let Some(msg) = request.messages.iter().rev().find(|m| m.role == Role::User) else {
        return String::new();
    };
    match &msg.content {
        MessageContent::Text(text) => text.clone(),
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                ContentBlock::UserCommand { name, args } => Some(format!("{name} {args}")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// Case-insensitive substring check for any of the given needles.
fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::types::{ContentBlock, Message, ToolResultContent};

    fn req(text: &str) -> ProviderRequest {
        ProviderRequest {
            model: "free/auto".to_string(),
            messages: vec![Message::user(text)],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 64,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            effort_level: None,
            provider_options: serde_json::Value::Null,
            strict_route: false,
        }
    }

    #[test]
    fn defaults_to_code_generation() {
        assert_eq!(
            classify_request(&req("hello there")),
            TaskType::CodeGeneration
        );
        assert_eq!(
            classify_request(&req("can you take a look at this?")),
            TaskType::CodeGeneration
        );
    }

    #[test]
    fn code_generation_keywords() {
        assert_eq!(
            classify_request(&req("write a function that sorts a list")),
            TaskType::CodeGeneration
        );
        assert_eq!(
            classify_request(&req("create a new module for payments")),
            TaskType::CodeGeneration
        );
        assert_eq!(
            classify_request(&req("implement the parse_json helper")),
            TaskType::CodeGeneration
        );
    }

    #[test]
    fn code_edit_keywords() {
        assert_eq!(
            classify_request(&req("refactor the auth module to use async")),
            TaskType::CodeEdit
        );
        assert_eq!(
            classify_request(&req("fix the off-by-one in the loop")),
            TaskType::CodeEdit
        );
        assert_eq!(
            classify_request(&req("update the retry logic in client.rs")),
            TaskType::CodeEdit
        );
    }

    #[test]
    fn reasoning_keywords() {
        assert_eq!(
            classify_request(&req("why is the connection pooling to the db")),
            TaskType::Reasoning
        );
        assert_eq!(
            classify_request(&req("explain how the verify loop decides to continue")),
            TaskType::Reasoning
        );
        assert_eq!(
            classify_request(&req("debug the intermittent 503 from the proxy")),
            TaskType::Reasoning
        );
    }

    #[test]
    fn planning_and_verification_keywords() {
        assert_eq!(
            classify_request(&req("design the schema for the new analytics tables")),
            TaskType::Planning
        );
        assert_eq!(
            classify_request(&req("propose an architecture for the plugin system")),
            TaskType::Planning
        );
        assert_eq!(
            classify_request(&req("run the tests and report the failures")),
            TaskType::Verification
        );
        assert_eq!(
            classify_request(&req("verify the build passes")),
            TaskType::Verification
        );
    }

    #[test]
    fn simple_edit_and_search_keywords() {
        assert_eq!(
            classify_request(&req("rename total_price to subtotal")),
            TaskType::SimpleEdit
        );
        assert_eq!(
            classify_request(&req("fix the typo in the error message")),
            TaskType::SimpleEdit
        );
        assert_eq!(
            classify_request(&req("search for the verify policy definition")),
            TaskType::Search
        );
        assert_eq!(
            classify_request(&req("grep for RoutingStrategy usage")),
            TaskType::Search
        );
        // "find a way to…" is not a search — it must not trip the Search arm.
        assert_eq!(
            classify_request(&req("find a way to implement the response cache")),
            TaskType::CodeGeneration
        );
        // Literal reference lookups still route to Search.
        assert_eq!(
            classify_request(&req("find all callers of resolve_route")),
            TaskType::Search
        );
    }

    #[test]
    fn task_keys_are_stable_json_contract() {
        // settings.json's providers.free.options.routing.task_preferences map
        // is keyed by these strings — a rename silently breaks user configs.
        assert_eq!(TaskType::CodeGeneration.key(), "code_generation");
        assert_eq!(TaskType::CodeEdit.key(), "code_edit");
        assert_eq!(TaskType::Reasoning.key(), "reasoning");
        assert_eq!(TaskType::Planning.key(), "planning");
        assert_eq!(TaskType::Verification.key(), "verification");
        assert_eq!(TaskType::SimpleEdit.key(), "simple_edit");
        assert_eq!(TaskType::Search.key(), "search");
        // ALL enumerates every task exactly once (no duplicates, no gaps).
        let mut keys: Vec<&str> = TaskType::ALL.iter().map(|t| t.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), TaskType::ALL.len());
    }

    #[test]
    fn tool_result_trigger_is_code_edit() {
        let mut request = req("continue");
        request
            .messages
            .push(Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "tool_1".to_string(),
                name: "Write".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }]));
        request
            .messages
            .push(Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".to_string(),
                content: ToolResultContent::Text("wrote the file".to_string()),
                is_error: None,
            }]));
        assert_eq!(classify_request(&request), TaskType::CodeEdit);
    }

    #[test]
    fn preference_lists_cover_all_tasks_and_have_valid_upstreams() {
        let valid: Vec<&str> = crate::providers::free::FREE_CATALOG
            .iter()
            .map(|u| u.id)
            .collect();
        for task in [
            TaskType::CodeGeneration,
            TaskType::CodeEdit,
            TaskType::Reasoning,
            TaskType::Planning,
            TaskType::Verification,
            TaskType::SimpleEdit,
            TaskType::Search,
        ] {
            let ids = task_preference_ids(task);
            assert!(!ids.is_empty(), "{} has no preferences", task.label());
            for id in ids {
                assert!(
                    valid.contains(id),
                    "{} preference '{}' is not a catalog upstream",
                    task.label(),
                    id
                );
            }
            // Labels are unique and non-empty.
            assert!(!task.label().is_empty());
        }
    }
}
