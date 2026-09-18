//! Session synopsis generation — two one-line summaries per session.
//!
//! Called at session exit alongside the auto-titler. Produces:
//! - **about**: what the session was (first) about
//! - **left_off**: where the session left off (last known working point)
//!
//! The philosophy is that a few unique, important words per session about the
//! start and the last known working point beat nothing. Generation uses ONE
//! cheap completion routed through the free chain's Auto route (prefers Groq's
//! fast free upstreams); the model id is passed in by the caller so this
//! module stays provider-agnostic. When the call fails (offline, no keys,
//! cooldowns), a deterministic heuristic fallback distills the first user
//! prompt so the browser still shows something useful.

use clawde_api::{ProviderRequest, SystemPrompt};
use clawde_core::types::{ContentBlock, Message, Role};
use tokio_util::sync::CancellationToken;

/// Neutral defaults for the remaining `ProviderRequest` fields (stop
/// sequences, thinking, effort, routing flags). The struct has no `Default`
/// impl because most fields are required at real call sites.
fn empty_request_defaults() -> ProviderRequest {
    ProviderRequest {
        model: String::new(),
        messages: Vec::new(),
        system_prompt: None,
        tools: Vec::new(),
        max_tokens: 0,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: Vec::new(),
        thinking: None,
        effort_level: None,
        provider_options: serde_json::Value::Object(serde_json::Map::new()),
        strict_route: false,
    }
}

/// Sample windows: the synopsis only needs the head and tail of the session.
const LEADING_MESSAGES: usize = 4;
const TRAILING_MESSAGES: usize = 8;

/// Per-line synopsis caps. The browser renders these as single rows, so both
/// the prompt instruction and the post-trim enforce the same budget.
pub const MAX_ABOUT_CHARS: usize = 70;
pub const MAX_LEFT_OFF_CHARS: usize = 70;

/// Render one `Message` as compact text for the model: role-prefixed content
/// text, tool uses summarized as `→ tool_name`, tool results elided.
fn message_text(m: &Message) -> Option<String> {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut parts: Vec<String> = Vec::new();
    let blocks: &[ContentBlock] = match &m.content {
        clawde_core::types::MessageContent::Text(t) => {
            // Whitespace-collapse so multi-line prompts stay one line.
            let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
            if collapsed.is_empty() {
                return None;
            }
            return Some(format!("{role}: {collapsed}"));
        }
        clawde_core::types::MessageContent::Blocks(b) => b,
    };
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                // Whitespace-collapse so multi-line prompts stay one line.
                parts.push(text.split_whitespace().collect::<Vec<_>>().join(" "));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                // Include the first string-ish argument value for context
                // (e.g. the file path being edited).
                let detail = input
                    .as_object()
                    .and_then(|o| o.values().find_map(|v| v.as_str().map(str::to_owned)));
                match detail {
                    Some(d) => {
                        let d: String = d.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
                        let d: String = d.chars().take(80).collect();
                        parts.push(format!("→ {name}: {d}"));
                    }
                    None => parts.push(format!("→ {name}")),
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        return None;
    }
    let joined = parts.join(" | ");
    Some(format!("{role}: {}", {
        let joined: String = joined.chars().take(300).collect();
        joined
    }))
}

fn build_synopsis_prompt(sample: &[Message]) -> String {
    let mut transcript = String::new();
    for m in sample {
        if let Some(text) = message_text(m) {
            transcript.push_str(&text);
            transcript.push('\n');
        }
    }
    format!(
        "Below is a transcript of a coding-agent session. Reply with EXACTLY two lines \
         and nothing else:\n\
         Line 1: what this session was about, at most {about} characters.\n\
         Line 2: where it left off (the last known working point or next step), \
         at most {left_off} characters.\n\
         Rules: plain text, no quotes, no prefixes like \"About:\" or \"Line 1:\", \
         keep the most specific nouns (file names, commands, project names).\n\n\
         --- transcript ---\n{transcript}--- end ---",
        about = MAX_ABOUT_CHARS,
        left_off = MAX_LEFT_OFF_CHARS,
    )
}

/// Trim a generated line to its cap on a word boundary, appending an ellipsis.
fn clamp_line(line: &str, max: usize) -> String {
    let cleaned: String = line
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '`' | '*') || c.is_whitespace())
        .to_string();
    if cleaned.chars().count() <= max {
        return cleaned;
    }
    let mut cut: String = cleaned.chars().take(max.saturating_sub(1)).collect();
    if let Some(space) = cut.rfind(' ') {
        if space > max / 2 {
            cut.truncate(space);
        }
    }
    format!("{}\u{2026}", cut.trim_end())
}

/// Heuristic fallback: distill the first user prompt into an about-line.
/// Never fails — the philosophy is "a few important words beat nothing".
pub fn heuristic_about(messages: &[Message]) -> Option<String> {
    let first_user = messages.iter().find(|m| m.role == Role::User)?;
    let text = first_user.get_all_text();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    // Skip Clawde-internal meta prompts (slash-command expansions, system
    // reminders) — they describe the harness, not the user's goal.
    if text.starts_with('<') || text.starts_with('/') {
        return None;
    }
    Some(clamp_line(&text, MAX_ABOUT_CHARS))
}

/// Generate the (about, left_off) synopsis pair from the conversation messages.
///
/// One completion against `model` (expected on a fast/cheap route). Falls back
/// to `heuristic_about` for the first line when the call fails or returns
/// garbage; `left_off` is simply absent in that case.
pub async fn generate_session_synopsis(
    messages: &[Message],
    provider: &dyn clawde_api::LlmProvider,
    model: &str,
    max_tokens: u32,
    cancel: CancellationToken,
) -> Option<(String, Option<String>)> {
    if messages.len() < 2 {
        return None;
    }

    // Compact head+tail sample (deduped on uuid, like the titler).
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
    sample.push(Message::user(build_synopsis_prompt(&sample)));
    let sample = crate::sanitize::sanitize_history(sample);

    let request = ProviderRequest {
        model: model.to_string(),
        messages: sample,
        system_prompt: Some(SystemPrompt::Text(
            "You summarize coding-agent sessions in exactly two short plain-text lines. \
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

    // First text block, if any.
    let text = response.content.iter().find_map(|b| match b {
        ContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    })?;

    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| {
            // Reject list/numbered/prefixed formats — the model was told to
            // emit bare lines; anything else is treated as garbage.
            !l.starts_with('-')
                && !l.starts_with('*')
                && !l.starts_with(|c: char| c.is_ascii_digit())
        });
    let about = lines.next()?;
    let left_off = lines.next();

    let about = clamp_line(about, MAX_ABOUT_CHARS);
    if about.is_empty() {
        return None;
    }
    let left_off = left_off.map(|l| clamp_line(l, MAX_LEFT_OFF_CHARS));
    let left_off = left_off.filter(|l| !l.is_empty());

    Some((about, left_off))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_line_trims_on_word_boundary() {
        let s = clamp_line(
            "fix the flaky auth token refresh race in the login flow",
            30,
        );
        assert!(s.chars().count() <= 30);
        assert!(s.ends_with('\u{2026}'));
        assert!(!s.ends_with(" \u{2026}"));
    }

    #[test]
    fn clamp_line_strips_wrapping_quotes() {
        let s = clamp_line("\"a short title\"", 70);
        assert_eq!(s, "a short title");
    }

    #[test]
    fn heuristic_about_uses_first_user_prompt() {
        let msgs = vec![
            Message::assistant("hi"),
            Message::user("help me fix the snowflake shimmer animation in the TUI\nsecond line"),
            Message::assistant("ok"),
        ];
        let about = heuristic_about(&msgs).unwrap();
        assert!(about.contains("snowflake"), "got: {about}");
        assert!(about.chars().count() <= MAX_ABOUT_CHARS);
    }

    #[test]
    fn heuristic_about_rejects_meta_prompts() {
        let msgs = vec![Message::user("<system-reminder>internal</system-reminder>")];
        assert!(heuristic_about(&msgs).is_none());
    }
}
