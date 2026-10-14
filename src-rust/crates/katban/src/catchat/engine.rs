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
//! - The guest voice defaults to the Cathead persona (same prompt text as the
//!   TUI's `/cathead`), composed behind the fixed guest contract.

use super::search::{GuestSearch, SearchResult};
use clawde_core::effort::EffortLevel;
use clawde_core::types::{ContentBlock, Message, MessageContent, Role, ToolDefinition};
use std::sync::Arc;
use std::sync::OnceLock;

pub const GUEST_MODEL: &str = "free/auto";
pub const MAX_TOOL_ROUNDS: usize = 4;
pub const MAX_HISTORY_MESSAGES: usize = 40;

/// The guest contract: a general assistant with one tool, no filesystem, no
/// host access. This part is fixed — guests cannot influence it.
pub const GUEST_CONTRACT_PROMPT: &str = r#"You are Cat Chat, a friendly general-purpose assistant.
You can use the web_search tool to look up current information.
You have no access to files, terminals, or any host system — never claim otherwise.
Keep answers clear and reasonably concise. Cite the source URLs from web search results when you use them."#;

/// The Cathead voice text, single-sourced from `clawde-core`'s built-in
/// persona — the same prompt `/cathead` activates in the TUI. The dependency
/// on `builtin_cathead` is structural (this fails to compile if the core API
/// changes) and the voice-marker assertion lives in
/// `guest_prompt_carries_the_cathead_voice` (mirroring core's
/// `persona_prompts_carry_signature_voice`), so a rewrite of the persona that
/// drops the cat voice fails tests here and in core instead of drifting
/// silently.
fn cathead_voice() -> String {
    clawde_core::output_styles::OutputStyleDef::builtin_cathead().prompt
}

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
                "no free providers configured — add free-tier keys (e.g. /keys) to enable Cat Chat"
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

/// Which voice a guest's session speaks in.
///
/// The host-fixed default is Cathead — the same persona `/cathead` activates in
/// the TUI, so a friend meets the mascot's voice. A guest may swap it for their
/// own session with `/cathead`, `/caveman` or `/normal`; all three choices are
/// host-authored prompt text, so the guest contract above them cannot be
/// altered by a guest's message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GuestVoice {
    #[default]
    Cathead,
    Caveman,
    /// No persona: the contract alone.
    Normal,
}

impl GuestVoice {
    pub fn as_str(self) -> &'static str {
        match self {
            GuestVoice::Cathead => "cathead",
            GuestVoice::Caveman => "caveman",
            GuestVoice::Normal => "normal",
        }
    }

    /// The persona text appended after the fixed guest contract. `Normal`
    /// appends none — the contract is the whole prompt.
    pub fn prompt(self) -> Option<String> {
        match self {
            GuestVoice::Cathead => Some(cathead_voice()),
            GuestVoice::Caveman => {
                Some(clawde_core::output_styles::OutputStyleDef::builtin_caveman().prompt)
            }
            GuestVoice::Normal => None,
        }
    }
}

/// One guest's ephemeral session state.
#[derive(Debug, Default)]
pub struct GuestSession {
    pub messages: Vec<Message>,
    pub notes: String,
    /// Voice for this session (host default: [`GuestVoice::Cathead`]).
    pub voice: GuestVoice,
    /// Effort override chosen with `/effort`; `None` = the model's default.
    pub effort: Option<EffortLevel>,
    /// Mode chosen with `/mode`. A guest has no tools, so only the mode's
    /// prompt guidance applies; its permission/plan knobs are inert here.
    pub mode: Option<String>,
    /// Model pinned with `/model`; `None` = the host's default chain.
    pub model: Option<String>,
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

    /// The prompts the guest typed in this chat, oldest first.
    ///
    /// Commands are never recorded as messages (they are answered by the host,
    /// not by the model), so this is exactly what the guest asked.
    pub fn prompts(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| message.role == Role::User)
            .filter_map(|message| match &message.content {
                MessageContent::Text(text) => Some(text.clone()),
                MessageContent::Blocks(_) => None,
            })
            .filter(|text| !text.trim().is_empty())
            .collect()
    }

    /// Drop this chat's transcript and working notes. `/new`: the session, its
    /// link, its device token and its settings all stay — only the conversation
    /// starts over.
    pub fn clear_history(&mut self) {
        self.messages.clear();
        self.notes.clear();
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
            let request = self.build_request(session, &history, tool_round == 0);
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
                return Ok(visible_text(&response.content));
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
                                // The tool result is a prompt, so it has to say
                                // what to do instead: a bare "search failed"
                                // invites the model to invent results or to
                                // claim it searched. Name the failure and
                                // require it to be admitted.
                                Err(error) => format!(
                                    "web search is unavailable on this host ({error}). Do not guess, \
                                     and do not claim you searched: answer from what you already know \
                                     and tell the guest you could not reach the web."
                                ),
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
        Ok(visible_text(&response.content))
    }

    fn build_request(
        &self,
        session: &GuestSession,
        history: &[Message],
        with_tools: bool,
    ) -> clawde_api::provider_types::ProviderRequest {
        let tools = if with_tools {
            vec![web_search_definition()]
        } else {
            Vec::new()
        };
        clawde_api::provider_types::ProviderRequest {
            // A pinned model still rides the free chain when the pin is absent;
            // the host's `--models` list is what a pin is validated against
            // (`catchat::commands`), so an unlisted id never reaches here.
            model: session
                .model
                .clone()
                .unwrap_or_else(|| GUEST_MODEL.to_string()),
            messages: history.to_vec(),
            system_prompt: Some(clawde_api::provider_types::SystemPrompt::Text(
                guest_system_prompt_for(session),
            )),
            tools,
            max_tokens: 1024,
            temperature: Some(0.7),
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            effort_level: session.effort,
            provider_options: serde_json::Value::Object(Default::default()),
            strict_route: false,
        }
    }
}

/// The guest system prompt for a session: the fixed contract first (the
/// security posture — no host access, cite sources), then the session's voice,
/// then the mode guidance its `/mode` choice asked for.
///
/// Every part is host-authored text. A guest's message can never alter the
/// contract, and `/cathead`-style commands can only swap in another host
/// persona or a built-in mode block — the ordering here is what keeps the
/// contract first no matter what the session holds.
pub fn guest_system_prompt_for(session: &GuestSession) -> String {
    let mut prompt = GUEST_CONTRACT_PROMPT.to_string();
    if let Some(voice) = session.voice.prompt() {
        prompt.push_str("\n\n");
        prompt.push_str(&voice);
    }
    if let Some(mode) = &session.mode {
        // `&[]` = the built-in registry: a guest session cannot load modes from
        // the host's disk, and an unknown name yields no block.
        if let Some(block) = clawde_core::modes::resolve_mode_block(&[], mode) {
            prompt.push_str("\n\n");
            prompt.push_str(&block);
        }
    }
    prompt
}

/// The default guest prompt: the contract with the Cathead voice and no mode —
/// exactly a fresh [`GuestSession`].
pub fn guest_system_prompt() -> String {
    guest_system_prompt_for(&GuestSession::default())
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

/// The text a *guest* may see from one assistant turn.
///
/// Deliberately narrower than [`render_text`]. That one tags the reasoning and
/// tool-use blocks into the string it builds, which is right for the transcript
/// fed back to the model and wrong for the answer shown to a friend: a guest
/// must never be handed the model's chain of thought. Here
///
/// * `Thinking` and tool-use blocks contribute nothing, and
/// * a text block is stripped of any reasoning the serving upstream folded into
///   it (`reasoning</think>answer`) — the free chain includes upstreams
///   that leave the chain of thought in the text field, and one live guest
///   summary came back as `Hello there!</think>Hello there!`.
fn visible_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            out.push_str(&strip_inline_reasoning(text));
        }
    }
    out.trim().to_string()
}

/// Drop reasoning a provider folded into a text block.
///
/// Two shapes show up in practice: a complete `think` span in either spelling,
/// and a response that opens with the reasoning and then carries a lone closing
/// tag because the upstream ate the opening marker. A bare closing tag means
/// everything before it was reasoning, so it is dropped too. With no marker at
/// all the text is left alone — guessing there would risk eating a real answer
/// that merely mentions the tag.
fn strip_inline_reasoning(text: &str) -> String {
    const TAGS: [(&str, &str); 2] = [("<think>", "</think>"), ("<thinking>", "</thinking>")];
    let mut out = text.to_string();
    for (open, close) in TAGS {
        while let Some(start) = out.find(open) {
            let after_open = start + open.len();
            match out[after_open..].find(close) {
                Some(offset) => {
                    let end = after_open + offset + close.len();
                    out.replace_range(start..end, "");
                }
                None => {
                    // Unterminated: the rest of the block is reasoning.
                    out.truncate(start);
                    break;
                }
            }
        }
    }
    for (_, close) in TAGS {
        if let Some(at) = out.rfind(close) {
            out = out[at + close.len()..].to_string();
        }
    }
    out.trim().to_string()
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

    /// Fake backend that returns canned responses in sequence and keeps every
    /// request it was handed, so a test can inspect what the model actually saw
    /// (e.g. the text of a tool result).
    struct FakeBackend {
        responses: Mutex<Vec<ProviderResponse>>,
        requests: Mutex<Vec<ProviderRequest>>,
    }

    impl FakeBackend {
        fn new(responses: Vec<ProviderResponse>) -> Self {
            FakeBackend {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }

        /// The most recent request the engine sent.
        fn last_request(&self) -> Option<ProviderRequest> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .last()
                .cloned()
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
        async fn chat(&self, request: ProviderRequest) -> Result<ProviderResponse, String> {
            self.requests
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(request);
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

    /// A guest must never be handed the model's chain of thought, in either
    /// shape: a `Thinking` block, or reasoning an upstream folded into the text
    /// block it returned (one live guest summary came back as
    /// `Hello there!</think>Hello there!`).
    #[test]
    fn guest_visible_text_drops_thinking_tool_markers_and_inline_reasoning() {
        let blocks = vec![
            ContentBlock::Thinking {
                thinking: "weighing the options".to_string(),
                signature: String::new(),
            },
            ContentBlock::Text {
                text: "<think>2+2 is basic arithmetic</think>The answer is 4.".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "web_search".to_string(),
                input: serde_json::json!({ "query": "2+2" }),
                thought_signature: None,
            },
        ];
        assert_eq!(visible_text(&blocks), "The answer is 4.");
    }

    #[test]
    fn guest_visible_text_drops_a_lone_closing_reasoning_tag() {
        // The upstream ate the opening marker, so everything before the closing
        // tag is reasoning. Guessing this shape away is what keeps
        // `Hello there!</think>Hello there!` from reaching a friend.
        assert_eq!(
            strip_inline_reasoning("Hello there!</think>Hello there!"),
            "Hello there!"
        );
        assert_eq!(
            strip_inline_reasoning("<thinking>a long deliberation</thinking>Paris"),
            "Paris"
        );
    }

    #[test]
    fn guest_visible_text_leaves_plain_answers_alone() {
        assert_eq!(strip_inline_reasoning("Paris"), "Paris");
        assert_eq!(strip_inline_reasoning("1 < 2 and 3 > 2"), "1 < 2 and 3 > 2");
        // Unterminated reasoning: nothing after the marker is an answer.
        assert_eq!(strip_inline_reasoning("<think>still thinking"), "");
    }

    #[test]
    fn guest_visible_text_of_a_reasoning_only_turn_is_empty() {
        let blocks = vec![ContentBlock::Thinking {
            thinking: "no answer, only deliberation".to_string(),
            signature: String::new(),
        }];
        assert_eq!(visible_text(&blocks), "");
    }

    #[async_trait::async_trait]
    impl GuestSearch for FakeSearch {
        async fn search(&self, _query: &str) -> Result<Vec<SearchResult>, String> {
            Ok(self.results.clone())
        }
    }

    #[test]
    fn guest_prompt_carries_the_cathead_voice() {
        // The guest default persona is Cathead, single-sourced from core's
        // builtin — the same prompt `/cathead` activates in the TUI. Assert
        // the signature voice markers (mirroring core's
        // `persona_prompts_carry_signature_voice`) so a persona rewrite that
        // drops the cat voice fails here instead of drifting silently.
        let prompt = guest_system_prompt();
        assert!(prompt.contains("Cathead"), "missing Cathead persona text");
        assert!(prompt.contains("purr"), "missing the purr voice marker");
        assert!(prompt.contains("meow"), "missing the meow voice marker");
        // The guest contract must survive the composition verbatim — the
        // security posture comes first and is never rewritten by the voice.
        assert!(prompt.starts_with(GUEST_CONTRACT_PROMPT));
        assert!(prompt.contains("web_search"));
        assert!(prompt.contains("no access to files, terminals, or any host system"));
        // Identity is blended, not replaced: the guest stays Cat Chat.
        assert!(prompt.contains("Cat Chat"));
    }

    #[test]
    fn guest_prompt_matches_core_cathead_prompt_exactly() {
        // Single-sourcing check: the voice half of the guest prompt must be
        // byte-identical to core's builtin cathead prompt, so a change in one
        // place is a change in both.
        let prompt = guest_system_prompt();
        let voice = cathead_voice();
        assert!(
            prompt.ends_with(&voice),
            "guest prompt must end with core's cathead voice verbatim"
        );
        assert_eq!(
            voice,
            clawde_core::output_styles::OutputStyleDef::builtin_cathead().prompt
        );
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

    /// Through the real reply path, not just the helper: an upstream that
    /// leaves its reasoning in the text block must not put it in front of the
    /// guest, while the session keeps the raw blocks for the next turn.
    #[tokio::test]
    async fn a_reply_never_carries_upstream_reasoning() {
        let engine = ChatEngine::new(
            // Split so the tag is not written whole in this file (a literal
            // closing reasoning tag in source is easy to mangle); the value the
            // backend returns is the real `… reasoning …</think>answer` shape.
            Arc::new(FakeBackend::new(vec![FakeBackend::text(concat!(
                "The user asked for the capital.</th",
                "ink>Paris."
            ))])),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        let reply = engine
            .respond(&mut session, "capital of France?")
            .await
            .unwrap();
        assert_eq!(reply, "Paris.");
        // The stored turn is untouched, so a provider that needs its reasoning
        // echoed back on the next turn still gets it.
        let MessageContent::Blocks(blocks) = &session.messages.last().unwrap().content else {
            panic!("assistant turn should be kept as blocks");
        };
        assert!(
            matches!(blocks.first(), Some(ContentBlock::Text { text }) if text.contains("asked for the capital"))
        );
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

    /// A failed search must be named to the model, not swallowed into an empty
    /// result set: a bare "search failed" invites it to invent an answer or to
    /// tell the guest it looked something up. The tool result it receives has
    /// to forbid both.
    #[tokio::test]
    async fn an_unreachable_search_is_named_in_the_tool_result() {
        struct FailingSearch;

        #[async_trait::async_trait]
        impl GuestSearch for FailingSearch {
            async fn search(&self, _query: &str) -> Result<Vec<SearchResult>, String> {
                Err("search endpoint returned 404 Not Found".to_string())
            }
        }

        let backend = Arc::new(FakeBackend::new(vec![
            FakeBackend::text("I could not reach the web just now."),
            FakeBackend::tool_use("clawde release notes"),
        ]));
        let engine = ChatEngine::new(backend.clone(), Arc::new(FailingSearch));
        let mut session = GuestSession::new();
        let reply = engine
            .respond(&mut session, "what shipped in the last release?")
            .await
            .unwrap();
        assert_eq!(reply, "I could not reach the web just now.");

        // The final round is the one that carries the tool result back.
        let request = backend.last_request().expect("a second round was sent");
        let result_text = request
            .messages
            .iter()
            .filter_map(|message| match &message.content {
                MessageContent::Blocks(blocks) => Some(blocks),
                _ => None,
            })
            .flatten()
            .find_map(|block| match block {
                ContentBlock::ToolResult {
                    content: clawde_core::types::ToolResultContent::Text(text),
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .expect("the model was handed a tool result");
        assert!(
            result_text.contains("web search is unavailable on this host"),
            "tool result must name the failure: {result_text}"
        );
        assert!(
            result_text.contains("404"),
            "tool result must carry the cause: {result_text}"
        );
        assert!(
            result_text.contains("do not claim you searched"),
            "tool result must forbid claiming a search that never happened: {result_text}"
        );
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

    /// What a guest changed with `/cathead`, `/mode`, `/effort` and `/model`
    /// reaches the request — and the fixed contract still comes first, so a
    /// session setting can never reorder or replace the security posture.
    #[tokio::test]
    async fn session_settings_reach_the_request_under_the_contract() {
        let backend: Arc<FakeBackend> = Arc::new(FakeBackend::new(vec![FakeBackend::text("ok")]));
        let engine = ChatEngine::new(
            backend.clone(),
            Arc::new(FakeSearch {
                results: Vec::new(),
            }),
        );
        let mut session = GuestSession::new();
        session.voice = GuestVoice::Caveman;
        session.mode = Some("careful".to_string());
        session.effort = Some(EffortLevel::High);
        session.model = Some("free/groq/x".to_string());

        engine.respond(&mut session, "hi").await.unwrap();

        let request = backend.last_request().expect("a request was sent");
        assert_eq!(request.model, "free/groq/x");
        assert_eq!(request.effort_level, Some(EffortLevel::High));
        let prompt = match request.system_prompt {
            Some(clawde_api::provider_types::SystemPrompt::Text(text)) => text,
            other => panic!("expected a text system prompt, got {other:?}"),
        };
        assert!(
            prompt.starts_with(GUEST_CONTRACT_PROMPT),
            "the contract must come first: {prompt}"
        );
        // Compare against core's own persona text: the caveman prompt never
        // says "Caveman" (its text is the concise-output rules), so asserting
        // on the label would pass only by accident.
        let expected_voice = clawde_core::output_styles::OutputStyleDef::builtin_caveman().prompt;
        assert!(
            prompt.contains(&expected_voice),
            "voice text missing: {prompt}"
        );
        // Compare against core's own block so a rewrite of the mode guidance
        // follows here instead of drifting.
        let expected =
            clawde_core::modes::resolve_mode_block(&[], "careful").expect("careful is a builtin");
        assert!(
            prompt.ends_with(&expected),
            "mode guidance missing: {prompt}"
        );
    }

    /// `/normal` drops the persona entirely: contract only, byte for byte.
    #[test]
    fn normal_voice_is_the_bare_contract() {
        let session = GuestSession {
            voice: GuestVoice::Normal,
            ..Default::default()
        };
        assert_eq!(guest_system_prompt_for(&session), GUEST_CONTRACT_PROMPT);
        // A fresh session still composes the historical default (contract +
        // Cathead), which is what `guest_system_prompt` promised before.
        assert_eq!(
            guest_system_prompt_for(&GuestSession::default()),
            guest_system_prompt()
        );
    }

    /// `/history` reads the guest's own prompts, and `/new` clears the
    /// transcript (plus the working notes) without touching the session's
    /// settings.
    #[test]
    fn prompts_lists_user_turns_and_clear_history_keeps_settings() {
        let mut session = GuestSession::new();
        session.notes = "scratch".to_string();
        session.messages.push(Message::user("first"));
        session.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("an answer".to_string()),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        });
        session.messages.push(Message::user("second"));
        assert_eq!(session.prompts(), vec!["first", "second"]);

        session.voice = GuestVoice::Caveman;
        session.clear_history();
        assert!(session.prompts().is_empty());
        assert!(session.notes.is_empty());
        assert_eq!(session.voice, GuestVoice::Caveman);
    }
}
