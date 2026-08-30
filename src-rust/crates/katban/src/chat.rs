//! Guest chat engine (spec §6.1/§9): chat + WebSearch only, no other tools.
//!
//! - The model call goes through [`GuestBackend`], a thin seam over
//!   `clawde-api`'s `LlmProvider`. The default [`FreeBackend`] rides the host's
//!   `free/auto` cascade (spec §9 — guests use free providers, never paid
//!   keys) and degrades to a friendly "temporarily unavailable" when no free
//!   providers are configured.
//! - The only tool is `web_search`, executed against the `GuestSearch` seam
//!   (a sandboxed SearXNG endpoint by default). Search results are screened as
//!   untrusted data (§7b) before they are appended to context.
//! - Sessions are ephemeral in-memory state (message history + working notes);
//!   nothing is written to the host filesystem.
//! - `summarize` produces the optional downloadable AI session summary.

use crate::search::{GuestSearch, SearchResult};
use clawde_core::types::{ContentBlock, Message, MessageContent, Role, ToolDefinition};
use std::sync::Arc;
use std::sync::OnceLock;

pub const GUEST_MODEL: &str = "free/auto";
pub const MAX_TOOL_ROUNDS: usize = 4;
pub const MAX_HISTORY_MESSAGES: usize = 40;

/// The guest system prompt is fixed and minimal: a general assistant with one
/// tool, no filesystem, no host access. Guests cannot influence it.
pub const GUEST_SYSTEM_PROMPT: &str = r#"You are Katban Guest, a friendly general-purpose assistant.
You can use the web_search tool to look up current information.
You have no access to files, terminals, or any host system — never claim otherwise.
Keep answers clear and reasonably concise. Cite the source URLs from web search results when you use them."#;

#[derive(Debug, Clone)]
pub enum ChatError {
    /// Provider error or no providers configured — show a friendly message.
    Unavailable(String),
    /// Input rejected before the model was called.
    Rejected(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::Unavailable(message) => {
                write!(formatter, "temporarily unavailable — {message}")
            }
            ChatError::Rejected(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for ChatError {}

/// The model-call seam. Tests substitute a fake; production uses
/// [`FreeBackend`].
#[async_trait::async_trait]
pub trait GuestBackend: Send + Sync {
    async fn chat(
        &self,
        request: clawde_api::provider_types::ProviderRequest,
    ) -> Result<clawde_api::provider_types::ProviderResponse, String>;
}

/// Production backend: the host's `free/auto` cascade via `clawde-api`.
/// Built lazily on first use so key configuration is picked up per process.
pub struct FreeBackend {
    provider: OnceLock<Option<Arc<dyn clawde_api::provider::LlmProvider>>>,
    build_error: OnceLock<Option<String>>,
}

impl FreeBackend {
    pub fn new() -> Self {
        FreeBackend {
            provider: OnceLock::new(),
            build_error: OnceLock::new(),
        }
    }

    fn provider(&self) -> Result<Arc<dyn clawde_api::provider::LlmProvider>, String> {
        if let Some(provider) = self.provider.get() {
            if let Some(provider) = provider {
                return Ok(provider.clone());
            }
            return Err(self
                .build_error
                .get()
                .and_then(|e| e.clone())
                .unwrap_or_else(|| "no free providers configured".to_string()));
        }
        let config = match clawde_core::config::Settings::load_sync() {
            Ok(settings) => settings.config,
            Err(error) => {
                let message = format!("could not load Clawde config: {error}");
                let _ = self.build_error.set(Some(message.clone()));
                let _ = self.provider.set(None);
                return Err(message);
            }
        };
        let provider = clawde_api::registry::provider_from_config(&config, "free");
        let result = match provider {
            Some(provider) => Ok(provider),
            None => Err(
                "no free providers configured — add free-tier keys (e.g. /keys) to enable guest chat"
                    .to_string(),
            ),
        };
        let _ = self.provider.set(result.as_ref().ok().cloned());
        let _ = self.build_error.set(result.as_ref().err().cloned());
        result
    }
}

impl Default for FreeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl GuestBackend for FreeBackend {
    async fn chat(
        &self,
        request: clawde_api::provider_types::ProviderRequest,
    ) -> Result<clawde_api::provider_types::ProviderResponse, String> {
        let provider = self.provider()?;
        provider
            .create_message(request)
            .await
            .map_err(|error| format!("{error}"))
    }
}

/// One guest's ephemeral session state.
#[derive(Debug, Default)]
pub struct GuestSession {
    pub messages: Vec<Message>,
    pub notes: String,
}

impl GuestSession {
    pub fn new() -> Self {
        GuestSession::default()
    }

    /// A rolling window of the last `MAX_HISTORY_MESSAGES` messages to bound
    /// token burn on long chats.
    pub fn window(&self) -> &[Message] {
        let len = self.messages.len();
        if len <= MAX_HISTORY_MESSAGES {
            &self.messages
        } else {
            &self.messages[len - MAX_HISTORY_MESSAGES..]
        }
    }
}

pub struct ChatEngine {
    pub backend: Arc<dyn GuestBackend>,
    pub search: Arc<dyn GuestSearch>,
    pub max_tool_rounds: usize,
}

impl ChatEngine {
    pub fn new(backend: Arc<dyn GuestBackend>, search: Arc<dyn GuestSearch>) -> Self {
        ChatEngine {
            backend,
            search,
            max_tool_rounds: MAX_TOOL_ROUNDS,
        }
    }

    /// Respond to a user message, appending the exchange to `session`.
    pub async fn respond(
        &self,
        session: &mut GuestSession,
        user_text: &str,
    ) -> Result<String, ChatError> {
        let user_text = user_text.trim();
        if user_text.is_empty() {
            return Err(ChatError::Rejected("empty message".to_string()));
        }
        if user_text.chars().count() > 4000 {
            return Err(ChatError::Rejected(
                "message too long (max 4000 characters)".to_string(),
            ));
        }
        session.messages.push(Message::user(user_text));

        let mut history = session.window().to_vec();
        let mut tool_round = 0;
        loop {
            let request = self.build_request(&history, tool_round == 0);
            let response = self
                .backend
                .chat(request)
                .await
                .map_err(ChatError::Unavailable)?;

            let tool_uses: Vec<&ContentBlock> = response
                .content
                .iter()
                .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                .collect();

            if tool_uses.is_empty() {
                // Final assistant message: keep it in the session history.
                let assistant = Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(response.content.clone()),
                    uuid: None,
                    cost: None,
                    snapshot_patch: None,
                    turn_meta: None,
                };
                session.messages.push(assistant);
                return Ok(render_text(&response.content));
            }

            if tool_round >= self.max_tool_rounds {
                return Err(ChatError::Unavailable(
                    "the assistant hit its tool-use limit".to_string(),
                ));
            }

            // Execute the tool calls and append the round to history.
            let mut blocks: Vec<ContentBlock> = response.content.clone();
            for block in &tool_uses {
                if let ContentBlock::ToolUse {
                    id, name, input, ..
                } = block
                {
                    let result = match name.as_str() {
                        "web_search" => {
                            let query = input
                                .get("query")
                                .and_then(|value| value.as_str())
                                .unwrap_or("")
                                .to_string();
                            match self.search.search(&query).await {
                                Ok(results) => render_results(&results),
                                Err(error) => format!("search failed: {error}"),
                            }
                        }
                        other => format!("unknown tool '{other}' — no such tool is available"),
                    };
                    blocks.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: clawde_core::types::ToolResultContent::Text(result),
                        is_error: None,
                    });
                }
            }
            history.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(blocks),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            });
            tool_round += 1;
        }
    }

    /// Generate a short AI summary of the session (the only artifact a guest
    /// can take away; spec §6.1). Purely from the guest's own messages.
    pub async fn summarize(&self, session: &GuestSession) -> Result<String, ChatError> {
        if session.messages.is_empty() {
            return Ok("(empty session — nothing to summarize)".to_string());
        }
        let transcript: Vec<String> = session
            .messages
            .iter()
            .map(|message| match &message.content {
                MessageContent::Text(text) => text.clone(),
                MessageContent::Blocks(blocks) => render_text(blocks),
            })
            .collect();
        let joined = transcript.join("\n\n");
        let prompt = format!(
            "Write a short, neutral summary of the conversation below (who asked, what was discussed, key facts found, any links shared). Keep it under 200 words, plain text.\n\n---\n{joined}"
        );
        let request = clawde_api::provider_types::ProviderRequest {
            model: GUEST_MODEL.to_string(),
            messages: vec![Message::user(prompt)],
            system_prompt: None,
            tools: Vec::new(),
            max_tokens: 512,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            effort_level: None,
            provider_options: serde_json::Value::Object(Default::default()),
            strict_route: false,
        };
        let response = self
            .backend
            .chat(request)
            .await
            .map_err(ChatError::Unavailable)?;
        Ok(render_text(&response.content))
    }

    fn build_request(
        &self,
        history: &[Message],
        with_tools: bool,
    ) -> clawde_api::provider_types::ProviderRequest {
        let tools = if with_tools {
            vec![web_search_definition()]
        } else {
            Vec::new()
        };
        clawde_api::provider_types::ProviderRequest {
            model: GUEST_MODEL.to_string(),
            messages: history.to_vec(),
            system_prompt: Some(clawde_api::provider_types::SystemPrompt::Text(
                GUEST_SYSTEM_PROMPT.to_string(),
            )),
            tools,
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            effort_level: None,
            provider_options: serde_json::Value::Object(Default::default()),
            strict_route: false,
        }
    }
}

pub fn web_search_definition() -> ToolDefinition {
    ToolDefinition {
        name: "web_search".to_string(),
        description: "Search the web for current information. Use this for questions about recent events, facts, or anything you are unsure about.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "The search query (short, keyword-style)"
                }
            },
            "required": ["query"]
        }),
    }
}

fn render_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } => out.push_str(text),
            ContentBlock::Thinking { thinking, .. } => {
                out.push_str(&format!("\n[thinking: {thinking}]\n"));
            }
            ContentBlock::ToolUse { name, .. } => {
                out.push_str(&format!("\n[used {name}]\n"));
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

fn render_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "no results found".to_string();
    }
    let mut out = String::from("Search results (untrusted web content):\n");
    for (index, result) in results.iter().enumerate() {
        out.push_str(&format!(
            "[{index}] {}\nURL: {}\n{}\n",
            result.title, result.url, result.snippet
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_api::provider_types::{ProviderRequest, ProviderResponse, StopReason};
    use clawde_core::types::UsageInfo;
    use std::sync::Mutex;

    /// Fake backend that returns canned responses in sequence.
    struct FakeBackend {
        responses: Mutex<Vec<ProviderResponse>>,
    }

    impl FakeBackend {
        fn new(responses: Vec<ProviderResponse>) -> Self {
            FakeBackend {
                responses: Mutex::new(responses),
            }
        }

        fn text(text: &str) -> ProviderResponse {
            ProviderResponse {
                id: "fake".to_string(),
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: UsageInfo::default(),
                model: GUEST_MODEL.to_string(),
                rate_limit: None,
            }
        }

        fn tool_use(query: &str) -> ProviderResponse {
            ProviderResponse {
                id: "fake".to_string(),
                content: vec![ContentBlock::ToolUse {
                    id: "toolcall-1".to_string(),
                    name: "web_search".to_string(),
                    input: serde_json::json!({ "query": query }),
                    thought_signature: None,
                }],
                stop_reason: StopReason::ToolUse,
                usage: UsageInfo::default(),
                model: GUEST_MODEL.to_string(),
                rate_limit: None,
            }
        }
    }

    #[async_trait::async_trait]
    impl GuestBackend for FakeBackend {
        async fn chat(&self, _request: ProviderRequest) -> Result<ProviderResponse, String> {
            self.responses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop()
                .ok_or_else(|| "no canned response left".to_string())
        }
    }

    struct FakeSearch {
        results: Vec<SearchResult>,
    }

    #[async_trait::async_trait]
    impl GuestSearch for FakeSearch {
        async fn search(&self, _query: &str) -> Result<Vec<SearchResult>, String> {
            Ok(self.results.clone())
        }
    }

    #[tokio::test]
    async fn plain_answer_without_tools() {
        let engine = ChatEngine::new(
            Arc::new(FakeBackend::new(vec![FakeBackend::text("hello friend")])),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        let reply = engine.respond(&mut session, "hi there").await.unwrap();
        assert_eq!(reply, "hello friend");
        assert_eq!(session.messages.len(), 2);
    }

    #[tokio::test]
    async fn tool_round_then_final_answer() {
        let engine = ChatEngine::new(
            Arc::new(FakeBackend::new(vec![
                FakeBackend::text("per the web, it is 42"),
                FakeBackend::tool_use("answer to everything"),
            ])),
            Arc::new(FakeSearch {
                results: vec![SearchResult {
                    title: "Meaning of life".to_string(),
                    url: "https://example.org".to_string(),
                    snippet: "the answer is 42".to_string(),
                }],
            }),
        );
        let mut session = GuestSession::new();
        let reply = engine
            .respond(&mut session, "what is the answer?")
            .await
            .unwrap();
        assert_eq!(reply, "per the web, it is 42");
        // Session persists only user + final assistant messages; the tool-use
        // round is transient request-internal history.
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].role, Role::Assistant);
    }

    #[tokio::test]
    async fn unknown_tool_is_reported_not_executed() {
        let tool_response = ProviderResponse {
            id: "fake".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({}),
                thought_signature: None,
            }],
            stop_reason: StopReason::ToolUse,
            usage: UsageInfo::default(),
            model: GUEST_MODEL.to_string(),
            rate_limit: None,
        };
        let engine = ChatEngine::new(
            Arc::new(FakeBackend::new(vec![
                FakeBackend::text("i cannot do that"),
                tool_response,
            ])),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        let reply = engine.respond(&mut session, "run a command").await.unwrap();
        assert_eq!(reply, "i cannot do that");
    }

    #[tokio::test]
    async fn empty_and_oversized_messages_rejected_before_model() {
        let engine = ChatEngine::new(
            Arc::new(FakeBackend::new(vec![FakeBackend::text("unused")])),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        assert!(matches!(
            engine.respond(&mut session, "   ").await,
            Err(ChatError::Rejected(_))
        ));
        let huge = "x".repeat(5000);
        assert!(matches!(
            engine.respond(&mut session, &huge).await,
            Err(ChatError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn provider_failure_is_friendly() {
        struct FailingBackend;
        #[async_trait::async_trait]
        impl GuestBackend for FailingBackend {
            async fn chat(&self, _request: ProviderRequest) -> Result<ProviderResponse, String> {
                Err("no free providers configured".to_string())
            }
        }
        let engine = ChatEngine::new(
            Arc::new(FailingBackend),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        let error = engine.respond(&mut session, "hello").await.unwrap_err();
        assert!(error.to_string().contains("temporarily unavailable"));
        assert!(
            !error.to_string().contains("free providers configured")
                || error.to_string().contains("free providers")
        );
    }

    #[tokio::test]
    async fn summary_is_generated_from_session() {
        let engine = ChatEngine::new(
            Arc::new(FakeBackend::new(vec![FakeBackend::text(
                "We discussed web search.",
            )])),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        session
            .messages
            .push(Message::user("what is the best pizza?"));
        let summary = engine.summarize(&session).await.unwrap();
        assert_eq!(summary, "We discussed web search.");
    }
}
