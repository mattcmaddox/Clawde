//! thinking_failure.rs — Free-tier thinking failure handling (spec §6.6).
//!
//! Reasoning models on free upstreams sometimes spend the whole reply inside
//! the thinking block. Two degenerate shapes matter:
//!
//! - **Thinking-only completion** — thinking blocks but no visible text and no
//!   tool calls (the free-tier flake the eval harness scores as unevaluable via
//!   `is_empty_completion`). It is a degraded result, not an answer.
//! - **Budget-eaten thinking** — `reasoning_tokens ≥ 0.9 × max_tokens` with
//!   nothing visible: thinking consumed the entire output budget (spec §2.3's
//!   `reasoning_tokens: 501, completion_tokens: 0`). Retrying with a larger
//!   output budget on a cap-adequate upstream can recover it.
//!
//! This module classifies those shapes from *observable turn state* only
//! (visible content, tool calls, usage) — never from what the model says about
//! itself — and supplies the bounded-retry nudge and the user-facing surface
//! text. The loop owns the retry counter and the max_tokens boost; the
//! classifier here is pure so the loop and its tests agree on the trigger.

/// Extra output budget granted to a budget-eaten retry, in tokens.
pub const BUDGET_EATEN_BOOST_TOKENS: u32 = 4_096;

/// Ceiling for the accumulated budget-eaten boost, so a pathological provider
/// cannot grow the output request without bound.
pub const MAX_BUDGET_EATEN_BOOST_TOKENS: u32 = 16_384;

/// Retries allowed for a single degenerate terminal turn. Bounded to one so a
/// provider that reliably returns nothing does not spin (the spec's "surface
/// rather than silently retrying forever").
pub const DEGENERATE_RETRY_LIMIT: u32 = 1;

/// Reasoning must reach at least this fraction of `max_tokens` to count as
/// budget-eaten (the thinking inspector's `9/10` bar).
const BUDGET_EATEN_NUMERATOR: u64 = 9;
const BUDGET_EATEN_DENOMINATOR: u64 = 10;

/// A degenerate terminal turn: no visible answer, no tool calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingFailure {
    /// Thinking blocks but no visible text (reasoning never became an answer).
    ThinkingOnly,
    /// Reasoning consumed almost the entire output budget.
    BudgetEaten,
    /// Nothing at all — no text, no thinking, no tool calls.
    Empty,
}

impl ThinkingFailure {
    /// The user-facing label for the surface status.
    fn label(self) -> &'static str {
        match self {
            Self::ThinkingOnly => "thinking-only completion (no visible answer)",
            Self::BudgetEaten => "output budget eaten by reasoning",
            Self::Empty => "empty completion",
        }
    }

    /// The retry nudge injected as a user turn before re-dispatching.
    pub(crate) fn retry_nudge(self) -> &'static str {
        match self {
            Self::ThinkingOnly => {
                "Your last reply contained only reasoning and no visible answer. \
                 Produce the actual answer now — the user cannot see your thinking."
            }
            Self::BudgetEaten => {
                "Your last reply spent its entire output budget on reasoning and \
                 produced no visible answer. Answer now with a concise, complete \
                 response; keep further reasoning brief."
            }
            Self::Empty => "Your last reply was empty. Produce the answer now.",
        }
    }

    /// The surface message shown to the user, attributed to the upstream that
    /// actually served (and failed) the turn.
    pub(crate) fn surface_message(self, provider: &str, model: &str, retried: bool) -> String {
        let tail = if retried {
            " — retried once, still no answer"
        } else {
            " — retrying once"
        };
        format!("{}/{} returned {}{}", provider, model, self.label(), tail)
    }
}

/// Read the observable shape of a terminal assistant message: whether it has a
/// *visible* answer (non-empty text or a tool call) and whether it carries any
/// thinking block. Shared by both request paths so they classify identically.
pub(crate) fn content_shape(message: &clawde_core::types::Message) -> (bool, bool) {
    use clawde_core::types::{ContentBlock, MessageContent};
    let mut visible = false;
    let mut thinking = false;
    match &message.content {
        MessageContent::Text(text) => visible = !text.trim().is_empty(),
        MessageContent::Blocks(blocks) => {
            for block in blocks {
                match block {
                    ContentBlock::Text { text } => {
                        if !text.trim().is_empty() {
                            visible = true;
                        }
                    }
                    ContentBlock::ToolUse { .. } => visible = true,
                    ContentBlock::Thinking { .. } => thinking = true,
                    _ => {}
                }
            }
        }
    }
    (visible, thinking)
}

/// Classify a terminal turn that produced no visible answer.
///
/// Returns `None` when the turn has visible text or a tool call — those are
/// answers, however short, and must not be retried. `has_thinking` marks a
/// thinking block in the reply (Anthropic folds reasoning into
/// `output_tokens`, so `reasoning_tokens` alone cannot see it); `max_tokens` is
/// the request's output budget as sent (a provider clamp is unknowable here).
pub(crate) fn classify(
    has_visible_output: bool,
    has_tool_calls: bool,
    has_thinking: bool,
    reasoning_tokens: u64,
    max_tokens: u32,
) -> Option<ThinkingFailure> {
    if has_visible_output || has_tool_calls {
        return None;
    }
    if reasoning_tokens > 0 {
        if max_tokens > 0
            && reasoning_tokens
                >= u64::from(max_tokens) * BUDGET_EATEN_NUMERATOR / BUDGET_EATEN_DENOMINATOR
        {
            return Some(ThinkingFailure::BudgetEaten);
        }
        return Some(ThinkingFailure::ThinkingOnly);
    }
    if has_thinking {
        return Some(ThinkingFailure::ThinkingOnly);
    }
    Some(ThinkingFailure::Empty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_text_is_never_a_failure() {
        assert_eq!(classify(true, false, false, 0, 8_192), None);
        assert_eq!(classify(false, true, false, 0, 8_192), None);
    }

    #[test]
    fn thinking_without_text_is_thinking_only() {
        assert_eq!(
            classify(false, false, false, 10, 8_192),
            Some(ThinkingFailure::ThinkingOnly)
        );
    }

    #[test]
    fn a_thinking_block_is_thinking_only_even_without_reasoning_tokens() {
        // Anthropic folds reasoning into output_tokens, so `reasoning_tokens`
        // is 0 there; the thinking block itself must still classify.
        assert_eq!(
            classify(false, false, true, 0, 8_192),
            Some(ThinkingFailure::ThinkingOnly)
        );
    }

    #[test]
    fn reasoning_at_the_budget_bar_is_budget_eaten() {
        // 9/10 of 8_192 = 7_372 (integer floor of 7_372.8).
        assert_eq!(
            classify(false, false, false, 7_372, 8_192),
            Some(ThinkingFailure::BudgetEaten)
        );
        assert_eq!(
            classify(false, false, false, 7_371, 8_192),
            Some(ThinkingFailure::ThinkingOnly)
        );
    }

    #[test]
    fn no_output_and_no_thinking_is_empty() {
        assert_eq!(
            classify(false, false, false, 0, 8_192),
            Some(ThinkingFailure::Empty)
        );
    }

    #[test]
    fn content_shape_reads_text_tool_and_thinking_blocks() {
        use clawde_core::types::{ContentBlock, Message, MessageContent};
        let text = Message {
            role: clawde_core::types::Role::Assistant,
            content: MessageContent::Text("answer".to_string()),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        };
        assert_eq!(content_shape(&text), (true, false));

        let thinking = Message {
            content: MessageContent::Blocks(vec![ContentBlock::Thinking {
                thinking: "reasoning".to_string(),
                signature: String::new(),
            }]),
            ..text.clone()
        };
        assert_eq!(content_shape(&thinking), (false, true));

        let tool = Message {
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "t".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }]),
            ..text.clone()
        };
        assert_eq!(content_shape(&tool), (true, false));
    }

    #[test]
    fn messages_are_distinct_and_attributed() {
        let thinking = ThinkingFailure::ThinkingOnly.retry_nudge();
        let budget = ThinkingFailure::BudgetEaten.retry_nudge();
        assert_ne!(thinking, budget);
        assert!(thinking.contains("thinking"));
        assert!(budget.contains("budget"));

        let surface =
            ThinkingFailure::BudgetEaten.surface_message("poolside", "laguna-s-2.1", true);
        assert!(surface.contains("poolside/laguna-s-2.1"));
        assert!(surface.contains("budget"));
        assert!(surface.contains("still no answer"));
    }
}
