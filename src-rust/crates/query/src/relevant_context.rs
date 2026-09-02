//! Deterministic relevant-context selection.
//!
//! The transcript remains complete and durable. This module creates a bounded
//! request view that favors the active instruction, recent evidence, changed
//! files, failures, and messages mentioning the same terms.

use clawde_core::types::{ContentBlock, Message, MessageContent, Role};
use std::collections::HashSet;

const MAX_SCORE: i32 = 1_000;

#[derive(Debug, Clone)]
pub struct ContextSelection {
    pub messages: Vec<Message>,
    pub omitted_messages: usize,
    pub estimated_tokens: u64,
}

/// Provider-safe request context. The selector may omit only completed
/// history; mandatory state and the active tool trajectory remain separate.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub messages: Vec<Message>,
    pub omitted_messages: usize,
    pub estimated_tokens: u64,
    pub used_full_history: bool,
    pub fallback_reason: Option<String>,
}

/// Build a bounded request history without mutating the durable transcript.
///
/// The latest user turn and the active assistant/tool-result chain are always
/// retained. If the history cannot be safely partitioned, this returns the
/// sanitized full history rather than guessing.
pub fn build_request_context(
    messages: &[Message],
    query: &str,
    changed_files: &[String],
    max_tokens: u64,
) -> RequestContext {
    let sanitized = crate::sanitize::sanitize_history(messages.to_vec());
    let Some(last_user_index) = sanitized
        .iter()
        .rposition(|message| message.role == Role::User)
    else {
        return RequestContext {
            estimated_tokens: estimate_messages(&sanitized),
            omitted_messages: 0,
            messages: sanitized,
            used_full_history: true,
            fallback_reason: Some("history has no user turn".to_string()),
        };
    };

    // A request must include the latest user turn, and any assistant/tool
    // rounds immediately leading into it. Older history is the only candidate
    // for relevance pruning.
    let mandatory_start = latest_tool_chain_start(&sanitized, last_user_index);
    let mandatory = &sanitized[mandatory_start..];
    let mandatory_tokens = estimate_messages(mandatory);
    if mandatory_tokens > max_tokens {
        return RequestContext {
            messages: sanitized,
            omitted_messages: 0,
            estimated_tokens: estimate_messages(messages),
            used_full_history: true,
            fallback_reason: Some("mandatory current tool chain exceeds budget".to_string()),
        };
    }

    let older = &sanitized[..mandatory_start];
    let remaining = max_tokens.saturating_sub(mandatory_tokens);
    let selected = select_relevant_context(older, query, changed_files, remaining);
    let mut output = selected.messages;
    output.extend_from_slice(mandatory);
    let output = crate::sanitize::sanitize_history(output);
    if !has_latest_user_turn(&output, &sanitized[last_user_index]) {
        return RequestContext {
            messages: sanitized,
            omitted_messages: 0,
            estimated_tokens: estimate_messages(messages),
            used_full_history: true,
            fallback_reason: Some("selection dropped latest user turn".to_string()),
        };
    }
    RequestContext {
        omitted_messages: sanitized.len().saturating_sub(output.len()),
        estimated_tokens: estimate_messages(&output),
        messages: output,
        used_full_history: false,
        fallback_reason: None,
    }
}

fn latest_tool_chain_start(messages: &[Message], last_user_index: usize) -> usize {
    let mut start = last_user_index;
    while start > 0 {
        if messages[start - 1].role != Role::Assistant {
            break;
        }
        if !messages[start - 1].has_tool_use() {
            break;
        }
        start -= 1;
        if start == 0 || messages[start - 1].role != Role::User {
            break;
        }
        start -= 1;
    }
    start
}

fn has_latest_user_turn(messages: &[Message], expected: &Message) -> bool {
    messages.iter().rev().any(|message| {
        message.role == expected.role && message.get_all_text() == expected.get_all_text()
    })
}

pub fn select_relevant_context(
    messages: &[Message],
    query: &str,
    changed_files: &[String],
    max_tokens: u64,
) -> ContextSelection {
    if messages.is_empty() || max_tokens == 0 {
        return ContextSelection {
            messages: Vec::new(),
            omitted_messages: messages.len(),
            estimated_tokens: 0,
        };
    }

    let query_terms = terms(query);
    let repository_terms = changed_files
        .iter()
        .flat_map(|file| repository_terms(file))
        .collect::<HashSet<_>>();
    let file_terms = changed_files
        .iter()
        .flat_map(|file| terms(file))
        .collect::<HashSet<_>>();
    let mut groups = group_messages(messages);
    let group_count = groups.len();
    for (index, group) in groups.iter_mut().enumerate() {
        group.score = score_group(
            group,
            index,
            group_count,
            &query_terms,
            &file_terms,
            &repository_terms,
        );
    }

    // Always retain the newest group, then take the highest-value older groups.
    groups.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.index.cmp(&left.index))
    });

    let mut selected = Vec::new();
    let mut estimated_tokens: u64 = 0;
    for group in groups {
        let group_tokens = estimate_messages(&group.messages);
        if selected.is_empty() || estimated_tokens.saturating_add(group_tokens) <= max_tokens {
            estimated_tokens = estimated_tokens.saturating_add(group_tokens);
            selected.push((group.index, group.messages));
        }
    }
    selected.sort_by_key(|(index, _)| *index);

    let mut output = Vec::new();
    for (_, group) in selected {
        output.extend(group);
    }
    // A selected tool result is only valid when its assistant tool call is
    // also selected. Grouping normally guarantees this, but repair the edge
    // case defensively before a provider sees the request.
    output = repair_tool_pairing(output);
    let omitted_messages = messages.len().saturating_sub(output.len());
    ContextSelection {
        messages: output,
        omitted_messages,
        estimated_tokens,
    }
}

#[derive(Debug, Clone)]
struct MessageGroup {
    index: usize,
    messages: Vec<Message>,
    score: i32,
}

fn group_messages(messages: &[Message]) -> Vec<MessageGroup> {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut index = 0;
    for message in messages {
        current.push(message.clone());
        if message.role == Role::Assistant {
            groups.push(MessageGroup {
                index,
                messages: std::mem::take(&mut current),
                score: 0,
            });
            index += 1;
        }
    }
    if !current.is_empty() {
        groups.push(MessageGroup {
            index,
            messages: current,
            score: 0,
        });
    }
    groups
}

fn score_group(
    group: &MessageGroup,
    index: usize,
    total: usize,
    query_terms: &HashSet<String>,
    file_terms: &HashSet<String>,
    repository_terms: &HashSet<String>,
) -> i32 {
    let text = group
        .messages
        .iter()
        .map(Message::get_all_text)
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let mut score = ((index + 1) as i32 * 20).min(300);
    if index + 1 == total {
        score += 400;
    }
    for term in query_terms {
        if text.contains(term) {
            score = (score + 80).min(MAX_SCORE);
        }
    }
    for term in file_terms {
        if text.contains(term) {
            score = (score + 100).min(MAX_SCORE);
        }
    }
    for term in repository_terms {
        if text.contains(term) {
            score = (score + 45).min(MAX_SCORE);
        }
    }
    if group.messages.iter().any(message_has_error) {
        score = (score + 180).min(MAX_SCORE);
    }
    score
}

fn repair_tool_pairing(messages: Vec<Message>) -> Vec<Message> {
    let mut tool_ids = HashSet::new();
    for message in &messages {
        if let MessageContent::Blocks(blocks) = &message.content {
            for block in blocks {
                if let ContentBlock::ToolUse { id, .. } = block {
                    tool_ids.insert(id.clone());
                }
            }
        }
    }
    messages
        .into_iter()
        .filter(|message| match &message.content {
            MessageContent::Blocks(blocks) => !blocks.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if !tool_ids.contains(tool_use_id))
            }),
            _ => true,
        })
        .collect()
}

fn message_has_error(message: &Message) -> bool {
    matches!(&message.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { is_error: Some(true), .. })))
}

fn repository_terms(value: &str) -> HashSet<String> {
    let normalized = value.replace('\\', "/");
    let mut result = terms(&normalized);
    if let Some(filename) = normalized.rsplit('/').next() {
        result.extend(terms(filename));
        if let Some((stem, extension)) = filename.rsplit_once('.') {
            result.insert(stem.to_ascii_lowercase());
            result.insert(extension.to_ascii_lowercase());
        }
    }
    result
}

fn terms(value: &str) -> HashSet<String> {
    value
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .map(str::to_ascii_lowercase)
        .filter(|term| term.len() >= 3)
        .collect()
}

fn estimate_messages(messages: &[Message]) -> u64 {
    let chars = messages
        .iter()
        .map(|message| message.get_all_text().len())
        .sum::<usize>();
    (chars as u64 / 4).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_chronological_order_after_ranking() {
        let messages = vec![
            Message::user("old unrelated discussion"),
            Message::assistant("old response"),
            Message::user("parser.rs needs a fix"),
            Message::assistant("parser.rs evidence"),
            Message::user("latest instruction"),
            Message::assistant("latest response"),
        ];
        let result =
            select_relevant_context(&messages, "parser.rs", &["parser.rs".to_string()], 100);
        let text = result
            .messages
            .iter()
            .map(Message::get_all_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("parser.rs evidence"));
        assert!(text.contains("latest response"));
        let positions = result
            .messages
            .iter()
            .map(Message::get_all_text)
            .collect::<Vec<_>>();
        assert!(positions.contains(&"latest response".to_string()));
    }

    #[test]
    fn request_context_keeps_latest_user_and_sanitizes_history() {
        let messages = vec![
            Message::user("old task"),
            Message::assistant("old answer"),
            Message::user("latest request"),
        ];
        let context = build_request_context(&messages, "latest", &[], 100);
        assert_eq!(
            context.messages.last().and_then(Message::get_text),
            Some("latest request")
        );
        assert!(!context.used_full_history);
        assert!(context.fallback_reason.is_none());
    }

    #[test]
    fn request_context_falls_back_when_mandatory_chain_exceeds_budget() {
        let messages = vec![
            Message::user("task"),
            Message::assistant_blocks(vec![ContentBlock::ToolUse {
                id: "1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"path":"src/lib.rs"}),
                thought_signature: None,
            }]),
            Message::user_blocks(vec![ContentBlock::ToolResult {
                tool_use_id: "1".to_string(),
                content: clawde_core::types::ToolResultContent::Text("large result".repeat(1000)),
                is_error: None,
            }]),
        ];
        let context = build_request_context(&messages, "task", &[], 0);
        assert!(context.used_full_history);
        assert!(context.fallback_reason.is_some());
    }

    #[test]
    fn repository_path_and_identifier_terms_boost_matching_history() {
        let messages = vec![
            Message::user("unrelated deployment discussion"),
            Message::assistant("deployment is ready"),
            Message::user("src/parser.rs ParserState needs review"),
            Message::assistant("ParserState is defined in src/parser.rs"),
        ];
        let result =
            select_relevant_context(&messages, "ParserState", &["src/parser.rs".to_string()], 20);
        let text = result
            .messages
            .iter()
            .map(Message::get_all_text)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("ParserState is defined"));
    }

    #[test]
    fn retains_tool_round_as_a_group() {
        let tool = Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: "1".to_string(),
            name: "Read".to_string(),
            input: serde_json::json!({"file_path":"src/lib.rs"}),
            thought_signature: None,
        }]);
        let result = Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: "1".to_string(),
            content: clawde_core::types::ToolResultContent::Text("content".to_string()),
            is_error: None,
        }]);
        let selection = select_relevant_context(
            &[Message::user("inspect"), tool, result],
            "lib.rs",
            &[],
            100,
        );
        assert!(selection
            .messages
            .iter()
            .any(|message| message.has_tool_use()));
        assert!(selection.messages.iter().any(|message| matches!(&message.content, MessageContent::Blocks(blocks) if blocks.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))));
    }
}
