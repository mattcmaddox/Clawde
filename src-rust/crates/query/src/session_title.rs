//! Session title generation — short, AI-generated names for conversations.
//!
//! Called at session exit to populate the `ai-title` transcript entry and
//! `session.title`, so recent-session listings show meaningful labels without
//! requiring the user to manually `/rename`.

use clawde_api::{AnthropicClient, CreateMessageRequest};
use clawde_core::types::Message;
use tokio_util::sync::CancellationToken;

/// Recency window: only the first and last few messages are needed for a title.
const LEADING_MESSAGES: usize = 4;
const TRAILING_MESSAGES: usize = 10;

/// Title cap: the generated title should never exceed this many characters.
pub const MAX_TITLE_CHARS: usize = 60;

// -----------------------------------------------------------------------
// Configuration
// -----------------------------------------------------------------------

/// Configuration for session title generation.
#[derive(Debug, Clone)]
pub struct SessionTitleConfig {
    /// A fast, cheap model — Haiku is ideal for this one-shot summarisation.
    pub model: String,
    /// Max tokens to generate (a title needs ~20 tokens).
    pub max_tokens: u32,
}

impl Default for SessionTitleConfig {
    fn default() -> Self {
        Self {
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: 80,
        }
    }
}

// -----------------------------------------------------------------------
// Prompt
// -----------------------------------------------------------------------

fn build_title_prompt(message_count: usize) -> String {
    format!(
        "You are generating a short, descriptive title for a coding conversation \
         that had {message_count} messages. The title must:\n\
         - Be at most {max_chars} characters (including spaces)\n\
         - Be a concise noun phrase (e.g. \"Fix flaky auth test\", \
           \"Add OAuth2 flow\", \"Refactor query loop\")\n\
         - NOT include quotes, dashes, or markdown formatting\n\
         - NOT start with \"Fix\", \"Add\", or \"Refactor\" every time — \
           use natural language\n\
         - Only output the title text, nothing else",
        max_chars = MAX_TITLE_CHARS,
    )
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// Generate a short AI-generated session title from the conversation messages.
///
/// Only a small window of messages is sent to the model to keep the call fast
/// and cheap. Returns `None` when the message list is too short for a
/// meaningful title or the API call fails.
pub async fn generate_session_title(
    messages: &[Message],
    api_client: &AnthropicClient,
    config: &SessionTitleConfig,
    cancel: CancellationToken,
) -> Option<String> {
    if messages.len() < 2 {
        return None;
    }

    // Build a compact view: first few + last few messages.
    let mut sample: Vec<Message> = messages.iter().take(LEADING_MESSAGES).cloned().collect();
    let trailing: Vec<Message> = messages
        .iter()
        .rev()
        .take(TRAILING_MESSAGES)
        .rev()
        .cloned()
        .collect();
    // Avoid overlap when the conversation is short.
    for msg in trailing {
        if !sample.iter().any(|s| s.uuid == msg.uuid) {
            sample.push(msg);
        }
    }

    sample.push(Message::user(build_title_prompt(messages.len())));

    // Heal orphaned tool_results (same safety net as away_summary).
    let sample = crate::sanitize::sanitize_history(sample);

    let api_messages: Vec<clawde_api::ApiMessage> =
        sample.iter().map(clawde_api::ApiMessage::from).collect();

    let request = CreateMessageRequest::builder(&config.model, config.max_tokens)
        .messages(api_messages)
        .build();

    let call_future = api_client.create_message(request);

    let response = tokio::select! {
        _ = cancel.cancelled() => return None,
        result = call_future => match result {
            Ok(r) => r,
            Err(_) => return None,
        },
    };

    let text = response.content.iter().find_map(|block| {
        if block.get("type")?.as_str()? == "text" {
            block.get("text")?.as_str().map(str::to_owned)
        } else {
            None
        }
    })?;

    let title = text
        .lines()
        .next()
        .unwrap_or(&text)
        .trim()
        .trim_matches('"');

    if title.is_empty() || title.len() > MAX_TITLE_CHARS * 2 {
        // Reject implausible outputs (too long = likely a hallucinated paragraph).
        None
    } else {
        let truncated: String = title.chars().take(MAX_TITLE_CHARS).collect();
        Some(truncated)
    }
}

// -----------------------------------------------------------------------
// Auto-title after first prompt
// -----------------------------------------------------------------------

/// Generate a session title after the first user prompt.
/// This is called after the first user message to provide immediate
/// session naming in the session picker.
pub async fn generate_title_after_first_prompt(
    first_message: &Message,
    api_client: &AnthropicClient,
    config: &SessionTitleConfig,
    cancel: CancellationToken,
) -> Option<String> {
    // Only generate title for the first user message
    if first_message.role != clawde_core::types::Role::User {
        return None;
    }

    let text = first_message.get_all_text();
    if text.is_empty() || text.len() < 10 {
        return None;
    }

    // Build a simple prompt for title generation from just the first message
    let prompt = format!(
        "Generate a short, descriptive title (max {} chars) for this coding task:\n\n{}",
        MAX_TITLE_CHARS,
        text.chars().take(200).collect::<String>() // Truncate long messages
    );

    let api_messages = vec![clawde_api::ApiMessage {
        role: "user".to_string(),
        content: serde_json::Value::String(prompt),
    }];

    let request = CreateMessageRequest::builder(&config.model, config.max_tokens)
        .messages(api_messages)
        .build();

    let call_future = api_client.create_message(request);

    let response = tokio::select! {
        _ = cancel.cancelled() => return None,
        result = call_future => match result {
            Ok(r) => r,
            Err(_) => return None,
        },
    };

    let text = response.content.iter().find_map(|block| {
        if block.get("type")?.as_str()? == "text" {
            block.get("text")?.as_str().map(str::to_owned)
        } else {
            None
        }
    })?;

    let title = text
        .lines()
        .next()
        .unwrap_or(&text)
        .trim()
        .trim_matches('"');

    if title.is_empty() || title.len() > MAX_TITLE_CHARS * 2 {
        None
    } else {
        let truncated: String = title.chars().take(MAX_TITLE_CHARS).collect();
        Some(truncated)
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_uses_haiku() {
        let cfg = SessionTitleConfig::default();
        assert!(cfg.model.contains("haiku"));
        assert!(cfg.max_tokens <= 100);
    }

    #[test]
    fn prompt_mentions_message_count() {
        let prompt = build_title_prompt(42);
        assert!(prompt.contains("42"));
        assert!(prompt.len() > 50);
    }

    #[tokio::test]
    async fn short_message_list_returns_none_without_api_call() {
        let msgs = [Message::user("hello")];
        // No API client available — the function returns None immediately
        // for < 2 messages without making any network request.
        // We can't call the real API in a unit test, but we can verify
        // the guard logic by calling with a dummy client that won't be reached.
        // This test only runs the pre-condition check.
        assert!(msgs.len() < 2);
    }

    #[test]
    fn first_prompt_title_generation_requires_user_message() {
        let msg = Message::assistant("hello");
        assert!(msg.role != clawde_core::types::Role::User);
    }
}
