// Correction detection for auto-learning from user corrections.
//
// When a user corrects the agent, we detect the pattern and save
// the correction as a memory for future sessions.

use clawde_core::types::Message;
use std::path::Path;
use tracing::info;

/// Correction patterns that indicate the user is correcting the agent.
/// Based on Claude Code's auto-memory system with more specific patterns
/// to reduce false positives.
const CORRECTION_PATTERNS: &[&str] = &[
    // Direct corrections
    "no, that's wrong",
    "actually, i meant",
    "don't do that",
    "the correct way is",
    "you should have",
    "that's not what i",
    "i meant to say",
    "wrong approach",
    "bad approach",
    // Pattern-based corrections (more specific than before)
    "no, the",
    "actually, the",
    "instead of that",
    "don't use",
    "use this instead",
    "that's incorrect",
    "that is incorrect",
    "not like that",
    "like this instead",
    // Fix/redo patterns
    "fix this please",
    "try again with",
    "redo this",
    "start over with",
    // Preference patterns
    "i prefer",
    "i like",
    "i want",
    "i need",
];

/// Detect if a user message is a correction to the agent's previous response.
pub fn is_correction(user_message: &Message, agent_response: Option<&Message>) -> bool {
    // Only check user messages
    if user_message.role != clawde_core::types::Role::User {
        return false;
    }

    let text = user_message.get_all_text().to_lowercase();

    // Check for correction patterns
    let has_correction_pattern = CORRECTION_PATTERNS
        .iter()
        .any(|pattern| text.contains(pattern));

    // Also check if there was a previous agent response (corrections usually follow agent output)
    let has_agent_response = agent_response
        .map(|r| r.role == clawde_core::types::Role::Assistant)
        .unwrap_or(false);

    has_correction_pattern && has_agent_response
}

/// Extract a correction memory from a user correction.
pub fn extract_correction_memory(
    user_message: &Message,
    _agent_response: Option<&Message>,
) -> Option<String> {
    let user_text = user_message.get_all_text();

    // Simple extraction: take the correction part
    // For now, just return the user's correction message
    if user_text.len() > 10 && user_text.len() < 500 {
        Some(format!("User correction: {}", user_text))
    } else {
        None
    }
}

/// Save a correction memory to the auto-memory system.
pub async fn save_correction_memory(memory: &str, working_dir: &Path) -> anyhow::Result<()> {
    use clawde_core::memdir::{auto_memory_path, is_auto_memory_enabled};

    let memory_enabled = clawde_core::config::Settings::load_sync()
        .ok()
        .and_then(|s| s.config.memory.enabled);

    if !is_auto_memory_enabled(memory_enabled) {
        return Ok(());
    }

    let memory_dir = auto_memory_path(working_dir);
    if !memory_dir.exists() {
        return Ok(());
    }

    // Create a filename based on timestamp
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!("correction_{}.md", timestamp);

    // Write the memory file using std::fs
    let content = format!("# User Correction\n\n{}\n", memory);
    let path = memory_dir.join(&filename);
    std::fs::write(&path, &content)?;

    info!(filename, "Saved correction memory");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_correction_with_pattern() {
        let user_msg =
            Message::user("No, that's wrong. The correct way is to use a different approach");
        let agent_msg = Message::assistant("I'll implement it this way");

        assert!(is_correction(&user_msg, Some(&agent_msg)));
    }

    #[test]
    fn test_is_not_correction_without_pattern() {
        let user_msg = Message::user("Please continue with the implementation");
        let agent_msg = Message::assistant("I'll implement it this way");

        assert!(!is_correction(&user_msg, Some(&agent_msg)));
    }

    #[test]
    fn test_is_not_correction_without_agent_response() {
        let user_msg = Message::user("No, that's wrong");

        assert!(!is_correction(&user_msg, None));
    }

    #[test]
    fn test_extract_correction_memory() {
        let user_msg = Message::user("No, I meant to use a different approach");

        let memory = extract_correction_memory(&user_msg, None);
        assert!(memory.is_some());
        assert!(memory.unwrap().contains("User correction"));
    }
}
