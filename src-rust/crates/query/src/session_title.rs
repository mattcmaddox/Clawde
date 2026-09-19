//! Session title generation — short names for conversations.
//!
//! Called at session exit to populate the `ai-title` transcript entry, so the
//! recent-session listings show meaningful labels without requiring the user
//! to manually `/rename`.
//!
//! Transport: the model is resolved by the caller (a fast free-chain route,
//! the same resolution the session synopsizer uses). This module used to call
//! `AnthropicClient` directly, which meant the titler silently produced
//! nothing on any non-Anthropic setup — free mode is the default, so no
//! session ever got a title. Any `LlmProvider` works now.
//!
//! Fallback: when the call fails, times out, or returns something implausible,
//! [`heuristic_title`] distils the opening prompt instead. The caller writes a
//! title either way, so a session listed in `/history` always has a label.

use crate::session_synopsis::empty_request_defaults;
use clawde_api::{LlmProvider, ProviderRequest, SystemPrompt};
use clawde_core::types::{ContentBlock, Message, Role};
use tokio_util::sync::CancellationToken;

/// Recency window: only the first and last few messages are needed for a title.
const LEADING_MESSAGES: usize = 4;
const TRAILING_MESSAGES: usize = 10;

/// Title cap: the generated title should never exceed this many characters.
pub const MAX_TITLE_CHARS: usize = 60;

/// Prompt for the one-shot titler. Kept short: a title needs ~20 tokens.
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

/// Trim a generated line to its cap on a word boundary.
fn clamp_title(line: &str) -> Option<String> {
    let cleaned: String = line
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '`' | '*') || c.is_whitespace())
        .to_string();
    if cleaned.is_empty() {
        return None;
    }
    // An implausibly long line is a hallucinated paragraph, not a title.
    if cleaned.chars().count() > MAX_TITLE_CHARS * 2 {
        return None;
    }
    Some(cleaned.chars().take(MAX_TITLE_CHARS).collect())
}

/// Deterministic fallback title: the opening words of the first user prompt.
/// Returns `None` when there is no user prompt to name the session after.
pub fn heuristic_title(messages: &[Message]) -> Option<String> {
    let first_user = messages.iter().find(|m| m.role == Role::User)?;
    clawde_core::session_digest::title_hint_from_prompt(&first_user.get_all_text())
}

/// Generate a short session title from the conversation messages.
///
/// Only a small window of messages is sent, to keep the call fast and cheap.
/// Returns `None` when the message list is too short for a meaningful title or
/// the call fails or returns garbage — the caller then falls back to
/// [`heuristic_title`].
pub async fn generate_session_title(
    messages: &[Message],
    provider: &dyn LlmProvider,
    model: &str,
    max_tokens: u32,
    cancel: CancellationToken,
) -> Option<String> {
    if messages.len() < 2 {
        return None;
    }

    // Compact head+tail sample, deduped on uuid.
    let mut sample: Vec<Message> = messages.iter().take(LEADING_MESSAGES).cloned().collect();
    let trailing: Vec<Message> = messages
        .iter()
        .rev()
        .take(TRAILING_MESSAGES)
        .rev()
        .cloned()
        .collect();
    for msg in trailing {
        if !sample.iter().any(|s| s.uuid == msg.uuid) {
            sample.push(msg);
        }
    }
    sample.push(Message::user(build_title_prompt(messages.len())));
    let sample = crate::sanitize::sanitize_history(sample);

    let request = ProviderRequest {
        model: model.to_string(),
        messages: sample,
        system_prompt: Some(SystemPrompt::Text(
            "You name coding-agent sessions with one short plain-text title. \
             Output nothing else."
                .to_string(),
        )),
        tools: Vec::new(),
        max_tokens,
        temperature: Some(0.2),
        ..empty_request_defaults()
    };

    let response = tokio::select! {
        _ = cancel.cancelled() => return None,
        result = provider.create_message(request) => match result {
            Ok(r) => r,
            Err(_) => return None,
        },
    };

    let text = response.content.iter().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    })?;

    clamp_title(text.lines().next().unwrap_or(&text))
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_mentions_message_count_and_cap() {
        let prompt = build_title_prompt(42);
        assert!(prompt.contains("42"));
        assert!(prompt.contains(&MAX_TITLE_CHARS.to_string()));
    }

    #[test]
    fn clamp_title_strips_quotes_and_caps_length() {
        assert_eq!(
            clamp_title("\"Fix flaky auth test\"").as_deref(),
            Some("Fix flaky auth test")
        );
        let long = "x".repeat(MAX_TITLE_CHARS * 2 + 1);
        assert!(clamp_title(&long).is_none(), "a paragraph is not a title");
        // Plausible length, over the cap: truncated, not rejected.
        let over = "word ".repeat(20);
        let clamped = clamp_title(&over).expect("clamped title");
        assert_eq!(clamped.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn heuristic_title_uses_the_first_user_prompt() {
        let messages = vec![
            Message::user("Fix the flaky auth test in session_browser.rs please"),
            Message::assistant("ok"),
        ];
        let title = heuristic_title(&messages).expect("heuristic title");
        assert!(title.contains("Fix the flaky auth test"), "got: {title}");
        assert!(title.chars().count() <= MAX_TITLE_CHARS);
    }

    #[test]
    fn heuristic_title_ignores_harness_meta_prompts() {
        let messages = vec![Message::user("/history popup says untitled")];
        assert!(heuristic_title(&messages).is_none());
    }

    #[test]
    fn heuristic_title_is_absent_without_a_user_turn() {
        let messages = vec![Message::assistant("hello")];
        assert!(heuristic_title(&messages).is_none());
    }
}
