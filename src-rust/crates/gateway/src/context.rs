//! Loop context management (D13) — reactive overflow compaction.
//!
//! The gateway runs short, client-capped agent loops (≤10 tool turns), so it
//! needs only *reactive* compaction, not the five-layer pipeline the TUI
//! session runs (audit §4 cut #1). On a `ContextOverflow` from any loop turn:
//!
//! 1. Truncate the oldest oversized tool results (deterministic, free),
//! 2. if the turn still overflows, summarise the head of the transcript via
//!    the provider (a reduced port of `crates/query/src/compact.rs`'s
//!    `reactive_compact`, adapted to a small gateway-local config).
//!
//! Retries are bounded at 2 per request ([`OverflowCompactor::MAX_STAGES`]).
//! The cancellation token is checked before and during the summariser call
//! (D16) so a client disconnect aborts compaction too.

use std::sync::Arc;

use clawde_api::provider_types::{ProviderRequest, SystemPrompt};
use clawde_api::{LlmProvider, ProviderError};
use clawde_core::types::{ContentBlock, Message, MessageContent, Role, ToolResultContent};
use tokio_util::sync::CancellationToken;

use crate::agent::AgentFailure;

/// Token budget for the recent tail preserved verbatim after summarisation
/// (mirrors the query crate's `KEEP_RECENT_TOKENS`).
const KEEP_RECENT_TOKENS: u64 = 16_000;

/// Tool-result text longer than this is a truncation candidate.
const TOOL_RESULT_TRUNCATE_THRESHOLD: usize = 500;

/// What a truncated tool result is replaced with (first 120 chars preserved).
const TOOL_RESULT_TRUNCATE_KEEP: usize = 120;

/// Per-request reactive compaction state (D13).
///
/// The loop owns one of these; each `ContextOverflow` advances one stage.
/// Two stages max: truncate tool results, then summarise the head.
#[derive(Clone)]
pub struct OverflowCompactor {
    model: String,
    max_summary_tokens: u32,
    timeout_secs: u64,
    /// Next stage to apply (0 = truncate, 1 = summarise, 2 = exhausted).
    stage: u32,
}

impl OverflowCompactor {
    pub const MAX_STAGES: u32 = 2;

    /// `model` is the wire model id used for the summariser dispatch.
    pub fn new(model: String, max_summary_tokens: u32, timeout_secs: u64) -> Self {
        Self {
            model,
            max_summary_tokens: max_summary_tokens.clamp(256, 8_192),
            timeout_secs,
            stage: 0,
        }
    }

    /// The most recently applied stage (for [`crate::agent::LoopEvent`]s).
    pub fn stage_done(&self) -> u32 {
        self.stage
    }

    /// Apply the next compaction stage. Returns `Ok(Some(new_messages))` when
    /// the transcript changed (the caller replaces its transcript and retries
    /// the turn), `Ok(None)` when no stage remains or nothing could be
    /// compacted (the caller must surface the overflow). Inputs are owned so
    /// the returned future is Send without holding borrows across awaits.
    pub async fn compact(
        &mut self,
        messages: Vec<Message>,
        provider: Arc<dyn LlmProvider>,
        cancel: CancellationToken,
    ) -> Result<Option<Vec<Message>>, AgentFailure> {
        let mut messages = messages;
        while self.stage < Self::MAX_STAGES {
            let progress = match self.stage {
                0 => truncate_oldest_tool_results(&mut messages),
                1 => {
                    let new_msgs = summarise_head(
                        provider.clone(),
                        messages,
                        self.model.clone(),
                        self.max_summary_tokens,
                        self.timeout_secs,
                        cancel.clone(),
                    )
                    .await?;
                    self.stage += 1;
                    return Ok(new_msgs);
                }
                _ => unreachable!("stage bounded by MAX_STAGES"),
            };
            self.stage += 1;
            if progress {
                return Ok(Some(messages));
            }
        }
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Stage 0: truncate the oldest oversized tool results (free, deterministic)
// ---------------------------------------------------------------------------

/// Replace oversized `ToolResult` text blocks with a short marker, walking
/// oldest messages first. Returns `true` when at least one result was
/// truncated. This is the deterministic first pass of D13: tool output is the
/// bulk of an agent transcript, so trimming old results usually frees enough
/// room without an API call.
fn truncate_oldest_tool_results(messages: &mut [Message]) -> bool {
    let mut changed = false;
    for msg in messages.iter_mut() {
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            for block in blocks.iter_mut() {
                if let ContentBlock::ToolResult { content, .. } = block {
                    let text = match content {
                        ToolResultContent::Text(t) => t.clone(),
                        ToolResultContent::Blocks(_) => continue,
                    };
                    if text.len() > TOOL_RESULT_TRUNCATE_THRESHOLD {
                        let mut keep = TOOL_RESULT_TRUNCATE_KEEP.min(text.len());
                        while !text.is_char_boundary(keep) {
                            keep -= 1;
                        }
                        *content = ToolResultContent::Text(format!(
                            "{}…[truncated by gateway compaction]",
                            &text[..keep]
                        ));
                        changed = true;
                    }
                }
            }
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Stage 1: summarise the transcript head via the provider
// ---------------------------------------------------------------------------

/// Summarise everything older than the recent tail (token-budgeted, snapped
/// to a tool_use↔tool_result-safe boundary) into one `<compact-summary>` user
/// message. Returns `Ok(Some(new_messages))` when the transcript was
/// rewritten, `Ok(None)` when there was nothing to summarise.
async fn summarise_head(
    provider: Arc<dyn LlmProvider>,
    messages: Vec<Message>,
    model: String,
    max_summary_tokens: u32,
    timeout_secs: u64,
    cancel: CancellationToken,
) -> Result<Option<Vec<Message>>, AgentFailure> {
    if cancel.is_cancelled() {
        return Err(AgentFailure::cancelled());
    }
    let split_at = snap_to_pairing_boundary(&messages, keep_index(&messages));
    if split_at == 0 {
        // Whole conversation fits the keep-recent budget; nothing to summarise.
        return Ok(None);
    }

    let head = &messages[..split_at];
    let tail = &messages[split_at..];
    let transcript = build_transcript(head);

    let compact_prompt = format!(
        "{NO_TOOLS_PREAMBLE}\n\
         Summarise the conversation inside <conversation_to_summarize>. Your summary must\n\
         preserve, verbatim where possible:\n\
         - the user's requests and every explicit constraint (do/don't/never/keep/only),\n\
         - file paths, function names, code snippets, and architectural decisions,\n\
         - errors encountered and how they were fixed,\n\
         - what was in progress when the transcript ends.\n\
         The most recent substantive user instruction must survive the summary VERBATIM.\n\
         Keep it thorough but concise; structure it with numbered sections.\n\n\
         <conversation_to_summarize>\n{transcript}\n</conversation_to_summarize>",
        transcript = transcript,
    );

    let request = ProviderRequest {
        model,
        messages: vec![Message::user(compact_prompt)],
        system_prompt: Some(SystemPrompt::Text(
            "You are a precise conversation summariser. Never call tools; respond with the \
             summary text only."
                .to_string(),
        )),
        tools: Vec::new(),
        max_tokens: max_summary_tokens,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: Vec::new(),
        thinking: None,
        effort_level: None,
        provider_options: Default::default(),
        strict_route: false,
    };

    // Cancellation is checked before and after the summariser call (D16); the
    // timeout wraps the provider future so a slow summariser fails the request
    // rather than hanging it (compaction retries are bounded anyway).
    let response = provider.create_message(request);
    let response = if timeout_secs > 0 {
        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), response).await {
            Ok(result) => result,
            Err(_) => {
                return Err(AgentFailure {
                    message: "Compaction summariser timed out".to_string(),
                    partial: None,
                    retry_after_secs: None,
                    context_overflow: true,
                });
            }
        }
    } else {
        response.await
    };
    if cancel.is_cancelled() {
        return Err(AgentFailure::cancelled());
    }
    let response = match response {
        Ok(r) => r,
        Err(e) => return Err(summariser_failure(&e)),
    };

    let raw = text_from_blocks(&response.content);
    let summary = format_compact_summary(&raw);
    if summary.is_empty() {
        return Err(AgentFailure {
            message: "Compaction summary was empty".to_string(),
            partial: None,
            retry_after_secs: None,
            context_overflow: true,
        });
    }

    let notice = Message::user(format!(
        "Context compaction: the earlier portion of this conversation ({} messages) was \
         summarised to stay within the model's context window.\n\n<compact-summary>\n{summary}\n</compact-summary>",
        split_at
    ));

    let mut new_messages = vec![notice];
    new_messages.extend_from_slice(tail);
    Ok(Some(new_messages))
}

/// Map a summariser provider error into an [`AgentFailure`].
fn summariser_failure(err: &ProviderError) -> AgentFailure {
    use ProviderError::*;
    let (message, retry_after_secs) = match err {
        RateLimited { retry_after, .. } => (
            "Upstream rate limit exceeded during compaction".to_string(),
            *retry_after,
        ),
        QuotaExceeded { .. } => (
            "Upstream quota exhausted during compaction".to_string(),
            Some(3600),
        ),
        other => (format!("Upstream error during compaction: {other}"), None),
    };
    AgentFailure {
        message,
        partial: None,
        retry_after_secs,
        context_overflow: false,
    }
}

const NO_TOOLS_PREAMBLE: &str = "CRITICAL: Respond with TEXT ONLY. Do NOT call any tools.\n";

/// Render a transcript of the head messages for the summariser prompt.
fn build_transcript(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::User => "Human",
            Role::Assistant => "Assistant",
        };
        let text = msg.get_all_text();
        if !text.is_empty() {
            out.push_str(&format!("{role}: {text}\n\n"));
        }
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                match block {
                    ContentBlock::ToolUse {
                        name, input, id, ..
                    } => {
                        out.push_str(&format!(
                            "[Tool Call: {name} (id={id})]\nInput: {input}\n\n"
                        ));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        let text = match content {
                            ToolResultContent::Text(t) => t.as_str(),
                            ToolResultContent::Blocks(_) => "[complex content]",
                        };
                        let err = if is_error.unwrap_or(false) {
                            " [ERROR]"
                        } else {
                            ""
                        };
                        out.push_str(&format!(
                            "[Tool Result (id={tool_use_id}){err}]\n{text}\n\n"
                        ));
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// Extract plain text from the summariser's response blocks.
fn text_from_blocks(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Strip `<analysis>…</analysis>` and collapse the `<summary>` wrapper, then
/// collapse blank runs. A reduced port of the query crate's formatter.
fn format_compact_summary(raw: &str) -> String {
    let without_analysis =
        if let (Some(start), Some(end)) = (raw.find("<analysis>"), raw.find("</analysis>")) {
            format!("{}{}", &raw[..start], &raw[end + "</analysis>".len()..])
        } else {
            raw.to_string()
        };
    let formatted = if let (Some(start), Some(end)) = (
        without_analysis.find("<summary>"),
        without_analysis.find("</summary>"),
    ) {
        let before = &without_analysis[..start];
        let content = without_analysis[start + "<summary>".len()..end].trim();
        let after = &without_analysis[end + "</summary>".len()..];
        format!("{before}Summary:\n{content}{after}")
    } else {
        without_analysis
    };
    let mut result = String::new();
    let mut blank = 0usize;
    for line in formatted.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank <= 1 {
                result.push('\n');
            }
        } else {
            blank = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

// ---------------------------------------------------------------------------
// Keep-tail split (ported from crates/query/src/compact.rs)
// ---------------------------------------------------------------------------

/// Rough token estimate: chars / 4, padded by 4/3 (mirrors the query crate).
fn estimate_tokens_for_messages(messages: &[Message]) -> usize {
    let chars: usize = messages
        .iter()
        .map(|m| match &m.content {
            MessageContent::Text(t) => t.len(),
            MessageContent::Blocks(blocks) => blocks.iter().map(estimate_block_chars).sum(),
        })
        .sum();
    (chars / 4) * 4 / 3
}

fn estimate_block_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.len(),
        ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
        ContentBlock::ToolResult { content, .. } => match content {
            ToolResultContent::Text(t) => t.len(),
            ToolResultContent::Blocks(blocks) => blocks.iter().map(estimate_block_chars).sum(),
        },
        ContentBlock::Thinking { thinking, .. } => thinking.len(),
        ContentBlock::RedactedThinking { data } => data.len(),
        _ => 200,
    }
}

/// Index of the first message to keep verbatim, budgeted by `KEEP_RECENT_TOKENS`.
fn keep_index(messages: &[Message]) -> usize {
    if messages.is_empty() {
        return 0;
    }
    let mut accumulated: u64 = 0;
    let mut keep_from = messages.len();
    for (i, msg) in messages.iter().enumerate().rev() {
        let est = estimate_tokens_for_messages(std::slice::from_ref(msg)) as u64;
        if accumulated + est > KEEP_RECENT_TOKENS {
            keep_from = i + 1;
            break;
        }
        accumulated += est;
        keep_from = i;
    }
    keep_from
}

/// Snap the split to a tool_use↔tool_result-safe boundary: walk backwards
/// (keeping MORE — never less) until the tail's first message carries no
/// orphaned `tool_result`.
fn snap_to_pairing_boundary(messages: &[Message], idx: usize) -> usize {
    let len = messages.len();
    let mut idx = idx.min(len);
    while idx > 0 && idx < len && message_has_tool_result(&messages[idx]) {
        idx -= 1;
    }
    idx
}

fn message_has_tool_result(msg: &Message) -> bool {
    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool_result(id: &str, text: &str) -> Message {
        Message::user_blocks(vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: ToolResultContent::Text(text.to_string()),
            is_error: Some(false),
        }])
    }

    /// Extract the Text content of the first ToolResult block in a message.
    fn tool_result_text(msg: &Message) -> String {
        match &msg.content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .find_map(|b| match b {
                    ContentBlock::ToolResult { content, .. } => match content {
                        ToolResultContent::Text(t) => Some(t.clone()),
                        ToolResultContent::Blocks(_) => None,
                    },
                    _ => None,
                })
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn tool_use(id: &str, name: &str) -> Message {
        Message::assistant_blocks(vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input: json!({"path": "x"}),
            thought_signature: None,
        }])
    }

    #[test]
    fn truncate_removes_only_oversized_results() {
        let mut msgs = vec![
            tool_result("c1", &"x".repeat(10_000)),
            tool_use("c2", "Read"),
            tool_result("c3", "small"),
        ];
        let changed = truncate_oldest_tool_results(&mut msgs);
        assert!(changed);
        // Oldest oversized result truncated; small result untouched.
        let first = match &msgs[0].content {
            MessageContent::Blocks(b) => b[0].clone(),
            _ => panic!(),
        };
        let ContentBlock::ToolResult { content, .. } = first else {
            panic!()
        };
        let ToolResultContent::Text(t) = content else {
            panic!()
        };
        assert!(t.contains("truncated by gateway compaction"));
        assert!(t.len() < 500);
        let third = match &msgs[2].content {
            MessageContent::Blocks(b) => b[0].clone(),
            _ => panic!(),
        };
        let ContentBlock::ToolResult { content, .. } = third else {
            panic!()
        };
        assert!(matches!(content, ToolResultContent::Text(t) if t == "small"));
    }

    #[test]
    fn truncate_noop_when_all_small() {
        let mut msgs = vec![tool_result("c1", "small"), tool_use("c2", "Read")];
        assert!(!truncate_oldest_tool_results(&mut msgs));
    }

    #[test]
    fn keep_index_and_pairing_boundary() {
        // A single huge tool result forces keep_from past it; the boundary
        // snap must not start the tail on the orphaned result's message.
        let msgs = vec![
            tool_result("c1", &"y".repeat(200_000)),
            tool_use("c2", "Read"),
            tool_result("c3", "small"),
            Message::assistant("done"),
        ];
        let raw = keep_index(&msgs);
        let snapped = snap_to_pairing_boundary(&msgs, raw);
        assert!(snapped <= raw);
        assert!(
            snapped == msgs.len() || !message_has_tool_result(&msgs[snapped]),
            "tail must not start on an orphaned tool result"
        );
    }

    #[test]
    fn format_summary_strips_analysis() {
        let raw = "<analysis>think</analysis>\n<summary>\n1. Request: fix bug\n</summary>";
        let out = format_compact_summary(raw);
        assert!(!out.contains("<analysis>"));
        assert!(out.contains("Summary:"));
        assert!(out.contains("1. Request: fix bug"));
    }

    #[test]
    fn build_transcript_includes_tool_rounds() {
        let msgs = vec![tool_use("c1", "Read"), tool_result("c1", "file contents")];
        let t = build_transcript(&msgs);
        assert!(t.contains("Tool Call: Read"));
        assert!(t.contains("file contents"));
    }

    #[test]
    fn snap_keeps_more_never_less() {
        let msgs = vec![
            Message::user("a"),
            tool_use("c1", "Read"),
            tool_result("c1", "r"),
            Message::assistant("ok"),
        ];
        // Force a raw cut directly on the tool_result message (index 2).
        let snapped = snap_to_pairing_boundary(&msgs, 2);
        assert_eq!(snapped, 1, "cut moves back to include the tool_use");
        assert!(!message_has_tool_result(&msgs[snapped]));
    }

    // ------------------------------------------------------------------
    // Mock provider + full compaction pass
    // ------------------------------------------------------------------

    struct SummariserMock {
        id: clawde_core::provider_id::ProviderId,
        calls: std::sync::atomic::AtomicUsize,
        summary: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for SummariserMock {
        fn id(&self) -> &clawde_core::provider_id::ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "mock-summariser"
        }

        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<clawde_api::provider_types::ProviderResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(clawde_api::provider_types::ProviderResponse {
                id: "sum_1".to_string(),
                content: vec![ContentBlock::Text {
                    text: self.summary.clone(),
                }],
                stop_reason: clawde_api::provider_types::StopReason::EndTurn,
                usage: clawde_core::types::UsageInfo::default(),
                model: "mock".to_string(),
                rate_limit: None,
            })
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            std::pin::Pin<
                Box<
                    dyn futures::Stream<
                            Item = Result<clawde_api::provider_types::StreamEvent, ProviderError>,
                        > + Send,
                >,
            >,
            ProviderError,
        > {
            Err(ProviderError::ServerError {
                provider: self.id().clone(),
                status: Some(500),
                message: "mock only supports non-streaming".to_string(),
                is_retryable: false,
            })
        }

        async fn health_check(
            &self,
        ) -> Result<clawde_api::provider_types::ProviderStatus, ProviderError> {
            Ok(clawde_api::provider_types::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> clawde_api::provider_types::ProviderCapabilities {
            clawde_api::provider_types::ProviderCapabilities {
                streaming: false,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: clawde_api::provider_types::SystemPromptStyle::SystemMessage,
            }
        }
    }

    #[tokio::test]
    async fn summarise_replaces_head_and_keeps_tail() {
        let provider: Arc<dyn LlmProvider> = Arc::new(SummariserMock {
            id: clawde_core::provider_id::ProviderId::new("mock"),
            calls: std::sync::atomic::AtomicUsize::new(0),
            summary: "<summary>\n1. Request: build the thing\n</summary>".to_string(),
        });
        let msgs = vec![
            Message::user("request"),
            Message::user("x".repeat(200_000)), // exceeds the keep-recent budget
            Message::assistant("working"),
        ];
        let new_msgs = summarise_head(
            provider,
            msgs,
            "free/auto".to_string(),
            1024,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("transcript should be rewritten");
        // Head summarised into a notice message; the recent tail is kept.
        assert_eq!(new_msgs.len(), 2);
        assert!(new_msgs[0].get_all_text().contains("build the thing"));
        assert!(new_msgs[0].get_all_text().contains("<compact-summary>"));
    }

    #[tokio::test]
    async fn summarise_noop_when_tail_fits_budget() {
        let provider: Arc<dyn LlmProvider> = Arc::new(SummariserMock {
            id: clawde_core::provider_id::ProviderId::new("mock"),
            calls: std::sync::atomic::AtomicUsize::new(0),
            summary: String::new(),
        });
        let msgs = vec![Message::user("hi"), Message::assistant("hello")];
        let progress = summarise_head(
            provider,
            msgs,
            "free/auto".to_string(),
            1024,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(progress.is_none());
    }

    #[tokio::test]
    async fn compact_advances_stages_on_repeated_overflow() {
        let provider: Arc<dyn LlmProvider> = Arc::new(SummariserMock {
            id: clawde_core::provider_id::ProviderId::new("mock"),
            calls: std::sync::atomic::AtomicUsize::new(0),
            summary: "<summary>summary text</summary>".to_string(),
        });
        let mut c = OverflowCompactor::new("free/auto".to_string(), 1024, 0);
        let cancel = CancellationToken::new();

        // Overflow 1: a huge tool result -> truncation stage makes progress.
        let msgs = vec![
            tool_result("c1", &"z".repeat(50_000)),
            Message::assistant("ok"),
        ];
        let out = c
            .compact(msgs, provider.clone(), cancel.clone())
            .await
            .unwrap();
        let msgs = out.expect("truncation should make progress");
        assert_eq!(c.stage_done(), 1);
        assert!(tool_result_text(&msgs[0]).contains("truncated by gateway compaction"));

        // Overflow 2: nothing oversized left -> summarise stage runs (the
        // transcript must exceed the keep-recent token budget for a split).
        let msgs2 = vec![
            tool_result("c1", &"z".repeat(100_000)),
            Message::assistant("ok"),
        ];
        let out = c
            .compact(msgs2, provider.clone(), cancel.clone())
            .await
            .unwrap();
        let msgs2 = out.expect("summarise should make progress");
        assert_eq!(c.stage_done(), 2);
        assert!(msgs2[0].get_all_text().contains("summary text"));

        // Overflow 3: exhausted -> no progress.
        let msgs3 = vec![Message::user("hi"), Message::assistant("ok")];
        assert!(c.compact(msgs3, provider, cancel).await.unwrap().is_none());
    }
}
