//! Ranked followups emitted by the model and rendered by interactive clients.
//!
//! The parser is deliberately strict: malformed structured output produces no
//! followups rather than inventing actions. Callers should remove the parsed
//! block from the visible response before rendering it.

use serde::{Deserialize, Serialize};

/// The six user-requested followup priority classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowupRank {
    HighlyRecommended,
    Recommended,
    Optional,
    ForCompletion,
    Unimportant,
    Undesired,
}

impl FollowupRank {
    pub fn label(self) -> &'static str {
        match self {
            Self::HighlyRecommended => "Highly recommended",
            Self::Recommended => "Recommended",
            Self::Optional => "Optional",
            Self::ForCompletion => "For completion",
            Self::Unimportant => "Unimportant",
            Self::Undesired => "Undesired",
        }
    }

    pub fn order(self) -> u8 {
        match self {
            Self::HighlyRecommended => 0,
            Self::Recommended => 1,
            Self::Optional => 2,
            Self::ForCompletion => 3,
            Self::Unimportant => 4,
            Self::Undesired => 5,
        }
    }
}

/// One suggested next action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedFollowup {
    pub text: String,
    pub rank: FollowupRank,
    #[serde(default)]
    pub reason: String,
}

/// Parsed followup block and the response text with the block removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFollowups {
    pub visible_text: String,
    pub followups: Vec<RankedFollowup>,
    /// Whether a complete structured block was found, including malformed
    /// blocks. Callers must use this to prevent metadata leakage.
    pub had_block: bool,
}

const OPEN: &str = "<clawde_followups>";
const CLOSE: &str = "</clawde_followups>";

/// Parse and strip one strict XML-like structured block.
///
/// Expected payload:
/// ```text
/// <clawde_followups>
/// [{"text":"Run tests","rank":"highly_recommended","reason":"..."}]
/// </clawde_followups>
/// ```
///
/// Any malformed payload produces no followups, while a complete block is
/// still removed from visible text to prevent metadata leakage. Only the first
/// complete block is accepted; additional blocks are removed but ignored.
pub fn parse_and_strip(text: &str) -> ParsedFollowups {
    let Some(start) = text.find(OPEN) else {
        return ParsedFollowups {
            visible_text: text.to_string(),
            followups: Vec::new(),
            had_block: false,
        };
    };
    let Some(relative_end) = text[start + OPEN.len()..].find(CLOSE) else {
        return ParsedFollowups {
            visible_text: text.to_string(),
            followups: Vec::new(),
            had_block: false,
        };
    };
    let end = start + OPEN.len() + relative_end + CLOSE.len();
    let payload = &text[start + OPEN.len()..start + OPEN.len() + relative_end];
    let followups = serde_json::from_str::<Vec<RankedFollowup>>(payload.trim())
        .ok()
        .filter(|items| !items.is_empty())
        .map(|mut items| {
            items.retain(|item| !item.text.trim().is_empty());
            items.sort_by_key(|item| item.rank.order());
            items
        })
        .unwrap_or_default();
    let mut visible_text = String::with_capacity(text.len().saturating_sub(end - start));
    visible_text.push_str(&text[..start]);
    let remainder = &text[end..];
    if let Some(duplicate_start) = remainder.find(OPEN) {
        if let Some(duplicate_end) = remainder[duplicate_start + OPEN.len()..].find(CLOSE) {
            let duplicate_end = duplicate_start + OPEN.len() + duplicate_end + CLOSE.len();
            visible_text.push_str(&remainder[..duplicate_start]);
            visible_text.push_str(&remainder[duplicate_end..]);
        } else {
            visible_text.push_str(remainder);
        }
    } else {
        visible_text.push_str(remainder);
    }
    ParsedFollowups {
        visible_text: visible_text.trim_end().to_string(),
        followups,
        had_block: true,
    }
}

/// Serialize the model-facing instruction for the final response.
pub fn prompt_instruction() -> &'static str {
    "If useful, end your response with exactly one <clawde_followups> block containing a JSON array of objects with text, rank (highly_recommended, recommended, optional, for_completion, unimportant, or undesired), and a one-line reason. Do not include the block unless a followup is genuinely useful."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_strips_ranked_block() {
        let parsed = parse_and_strip(
            "Done.\n<clawde_followups>\n[{\"text\":\"Run tests\",\"rank\":\"highly_recommended\",\"reason\":\"Verify the change.\"}]\n</clawde_followups>",
        );
        assert_eq!(parsed.visible_text, "Done.");
        assert!(parsed.had_block);
        assert_eq!(parsed.followups.len(), 1);
        assert_eq!(parsed.followups[0].rank, FollowupRank::HighlyRecommended);
    }

    #[test]
    fn sorts_by_rank_and_drops_empty_text() {
        let parsed = parse_and_strip(
            "x<clawde_followups>[{\"text\":\"later\",\"rank\":\"optional\"},{\"text\":\"now\",\"rank\":\"recommended\"},{\"text\":\"\",\"rank\":\"undesired\"}]</clawde_followups>",
        );
        assert_eq!(parsed.visible_text, "x");
        assert_eq!(parsed.followups.len(), 2);
        assert_eq!(parsed.followups[0].text, "now");
    }

    #[test]
    fn malformed_payload_does_not_invent_followups() {
        let parsed = parse_and_strip("answer <clawde_followups>{bad}</clawde_followups>");
        assert_eq!(parsed.visible_text, "answer");
        assert!(parsed.had_block);
        assert!(parsed.followups.is_empty());
    }

    #[test]
    fn duplicate_blocks_are_removed_and_only_first_is_used() {
        let parsed = parse_and_strip(
            "answer <clawde_followups>[{\"text\":\"first\",\"rank\":\"recommended\"}]</clawde_followups> tail <clawde_followups>[{\"text\":\"second\",\"rank\":\"optional\"}]</clawde_followups>",
        );
        assert_eq!(parsed.visible_text, "answer  tail");
        assert_eq!(parsed.followups.len(), 1);
        assert_eq!(parsed.followups[0].text, "first");

        let absent = parse_and_strip("answer");
        assert_eq!(absent.visible_text, "answer");
        assert!(!absent.had_block);
        assert_eq!(
            parse_and_strip("answer <clawde_followups>bad").visible_text,
            "answer <clawde_followups>bad"
        );
    }
}
