// `/search` command.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SearchCommand;

// ---- /search -------------------------------------------------------------

#[async_trait]
impl SlashCommand for SearchCommand {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Search across all sessions"
    }
    fn help(&self) -> &str {
        "Usage: /search <query>\n\n\
         Searches session titles and message content in the local SQLite\n\
         session database (~/.clawde/sessions.db).  Returns the 50 best\n\
         matching sessions, ordered by most recently updated.\n\n\
         Example: /search refactor authentication"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let query = args.trim();
        if query.is_empty() {
            return CommandResult::Error(
                "Usage: /search <query>\n\
                 Provide a search term to look up across all sessions."
                    .to_string(),
            );
        }

        let db_path = clawde_core::config::Settings::config_dir().join("sessions.db");

        let store = match clawde_core::SqliteSessionStore::open(&db_path) {
            Ok(s) => s,
            Err(e) => {
                return CommandResult::Error(format!(
                    "Failed to open session database: {}\n\
                     The database is created automatically once sessions are stored.",
                    e
                ))
            }
        };

        let results = match store.search_sessions(query) {
            Ok(r) => r,
            Err(e) => return CommandResult::Error(format!("Search failed: {}", e)),
        };

        if results.is_empty() {
            return CommandResult::Message(format!("No sessions found matching \"{}\".", query));
        }

        let mut out = format!(
            "Search results for \"{}\": {} session(s)\n\n",
            query,
            results.len()
        );
        for s in &results {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            out.push_str(&format!(
                "  [{}] {} — {} ({} messages, updated {})\n",
                &s.id[..s.id.len().min(12)],
                title,
                s.model,
                s.message_count,
                &s.updated_at[..s.updated_at.len().min(10)],
            ));
        }
        out.push_str("\nTip: use /resume <session-id> to continue a session.");
        CommandResult::Message(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context() -> CommandContext {
        CommandContext {
            config: clawde_core::Config::default(),
            cost_tracker: clawde_core::CostTracker::new(),
            messages: Vec::new(),
            working_dir: std::path::PathBuf::from("."),
            session_id: "search-test".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
            tool_use_tracker: None,
        }
    }

    #[tokio::test]
    async fn search_command_requires_a_query() {
        let mut ctx = test_context();
        let result = SearchCommand.execute("  ", &mut ctx).await;
        match result {
            CommandResult::Error(message) => assert!(message.contains("Usage: /search <query>")),
            other => panic!("expected usage error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_command_formats_title_and_content_matches() {
        let _home = crate::keys::tests::TestHome::new();
        let db_path = clawde_core::config::Settings::config_dir().join("sessions.db");
        let store = clawde_core::SqliteSessionStore::open(&db_path).expect("open sqlite store");
        store
            .save_session("session-123456789", Some("OAuth migration"), "model-x")
            .expect("save session");
        store
            .save_message(
                "session-123456789",
                "message-1",
                "assistant",
                "The callback was updated",
                None,
                None,
                None,
                None,
            )
            .expect("save message");

        let mut ctx = test_context();
        let result = SearchCommand.execute("callback", &mut ctx).await;
        match result {
            CommandResult::Message(message) => {
                assert!(message.contains("Search results for \"callback\": 1 session(s)"));
                assert!(message.contains("[session-1234] OAuth migration — model-x (1 messages"));
                assert!(message.contains("/resume <session-id>"));
            }
            other => panic!("expected search results, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_command_reports_no_matches() {
        let _home = crate::keys::tests::TestHome::new();
        let mut ctx = test_context();
        let result = SearchCommand.execute("missing-term", &mut ctx).await;
        assert!(matches!(
            result,
            CommandResult::Message(message)
                if message == "No sessions found matching \"missing-term\"."
        ));
    }
}
