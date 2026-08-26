//! The gateway agent loop (Phase 0).
//!
//! A thin ReAct loop (Yao et al., 2022) over [`LlmProvider`]: dispatch →
//! inspect tool calls → execute built-ins (internal) or yield client
//! functions (external) → append results → repeat until the model stops
//! calling tools, the cap is hit, or the request is cancelled.
//!
//! The loop is deliberately NOT `run_query_loop` (audit §1): the OpenAI wire
//! contract is client-owned, so the loop honors the client's messages/tools
//! verbatim and yields calls it cannot execute. The *harness* is ported from
//! the query crate where provider-agnostic:
//!
//! - max_tool_calls cap, force-stop on exhaustion (D9)
//! - mid-loop transient retry once, else fail with the partial transcript (D10)
//! - no-progress guard (identical consecutive calls stop early)
//! - cascade-drift thinking strip when the serving upstream changes (D12)
//! - cancellation reaching dispatch and tool execution (D16)
//!
//! Context-overflow compaction (D13) is wired in Phase 1 (`context.rs`).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use clawde_api::provider_types::{ProviderRequest, StopReason, StreamEvent};
use clawde_api::{LlmProvider, ProviderError};
use clawde_core::types::{ContentBlock, Message, MessageContent, Role, UsageInfo};
use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::tool_exec::GatewayToolExecutor;

/// How the loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    /// The model finished without (further) tool calls.
    Completed,
    /// The loop stopped to yield external tool calls to the client.
    Yielding,
    /// `max_tool_calls` exhausted; force-stopped (D9).
    CapExhausted,
    /// The no-progress guard stopped the loop.
    NoProgress,
    /// A provider error occurred mid-loop (D10).
    Failed,
    /// The request was cancelled (D16).
    Cancelled,
}

/// The result of a completed (or stopped) agent loop run.
#[derive(Debug, Clone)]
pub struct AgentOutcome {
    pub status: AgentStatus,
    /// The final assistant message (or the partial transcript on failure).
    pub message: Message,
    /// Aggregated usage across turns.
    pub usage: UsageInfo,
    /// Stop reason of the last model turn.
    pub stop_reason: StopReason,
    /// Number of model dispatches.
    pub turns: u32,
    /// Number of internal tool calls executed.
    pub tool_calls_executed: u32,
    /// The upstream that served the last turn (cascade-drift tracking).
    pub upstream: Option<String>,
    /// External tool calls yielded to the client, if `status == Yielding`.
    pub pending_external_calls: Vec<ContentBlock>,
}

impl AgentOutcome {
    fn completed(
        message: Message,
        usage: UsageInfo,
        stop_reason: StopReason,
        turns: u32,
        tool_calls_executed: u32,
        upstream: Option<String>,
    ) -> Self {
        Self {
            status: AgentStatus::Completed,
            message,
            usage,
            stop_reason,
            turns,
            tool_calls_executed,
            upstream,
            pending_external_calls: Vec::new(),
        }
    }
}

/// A non-recoverable loop failure (D10: fail with the partial transcript).
#[derive(Debug, Clone)]
pub struct AgentFailure {
    pub message: String,
    /// The partial transcript (assistant messages produced before the failure).
    pub partial: Option<Box<Message>>,
    /// `Retry-After` seconds when the failure was a rate limit.
    pub retry_after_secs: Option<u64>,
}

impl AgentFailure {
    pub fn cancelled() -> Self {
        Self {
            message: "Request cancelled".to_string(),
            partial: None,
            retry_after_secs: None,
        }
    }

    fn from_provider(err: &ProviderError, partial: Option<Message>) -> Self {
        let partial = partial.map(Box::new);
        use ProviderError::*;
        let (message, retry_after_secs) = match err {
            RateLimited { retry_after, .. } => {
                ("Upstream rate limit exceeded".to_string(), *retry_after)
            }
            QuotaExceeded { .. } => ("Upstream quota exhausted".to_string(), Some(3600)),
            ContextOverflow { .. } => ("Request exceeds model context window".to_string(), None),
            other => (format!("Upstream error: {other}"), None),
        };
        Self {
            message,
            partial,
            retry_after_secs,
        }
    }
}

/// Events the loop emits for wire translators to render progress.
#[derive(Debug, Clone)]
pub enum LoopEvent {
    /// A model turn began.
    TurnStart { turn: u32 },
    /// Incremental text from the model.
    TextDelta { text: String },
    /// Incremental thinking from the model.
    ThinkingDelta { thinking: String },
    /// A tool call the gateway will execute (internal).
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool call yielded to the client (external).
    ExternalToolCall {
        id: String,
        name: String,
        input: Value,
    },
    /// An internal tool finished executing.
    ToolExecuted {
        id: String,
        name: String,
        result: String,
        is_error: bool,
    },
    /// A model turn ended.
    TurnEnd { stop_reason: StopReason },
}

/// Loop configuration (gateway knobs; defaults per the plan's decisions).
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum internal/external tool-use turns processed per request (D9).
    pub max_tool_calls: u32,
    /// Maximum model dispatches (tool turns + final turn).
    pub max_turns: u32,
    /// Tool-result truncation budget in bytes (matches the query loop default).
    pub tool_result_budget: usize,
    /// Per-dispatch upstream timeout (0 = no timeout).
    pub timeout_secs: u64,
    /// Consecutive identical tool-call signature before the no-progress guard fires.
    pub no_progress_stop_streak: u32,
    /// When a turn mixes internal and external calls:
    /// - `true` (chat completions): yield ALL calls, execute none — the client
    ///   runs every declared function and resubmits (relay semantics).
    /// - `false` (Responses): execute internal calls, yield only external.
    pub yield_mixed_turns: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_tool_calls: 10,
            max_turns: 11,
            tool_result_budget: 50_000,
            timeout_secs: 120,
            no_progress_stop_streak: 3,
            yield_mixed_turns: true,
        }
    }
}

/// Run the agent loop to completion (or a stop condition).
///
/// `request` carries the client's model, messages, system prompt, and tools;
/// the loop clones it per turn with the growing transcript. `executor`
/// provides the built-in tool surface. Events are emitted to `event_tx` when
/// provided (streaming translators).
pub async fn run_agent_loop(
    provider: Arc<dyn LlmProvider>,
    request: ProviderRequest,
    executor: &GatewayToolExecutor,
    config: &AgentConfig,
    cancel: CancellationToken,
    event_tx: Option<UnboundedSender<LoopEvent>>,
) -> Result<AgentOutcome, AgentFailure> {
    let max_turns = config.max_turns.max(1);
    let mut messages = request.messages.clone();
    let mut usage = UsageInfo::default();
    let mut turns: u32 = 0;
    let mut tool_calls_executed: u32 = 0;
    let mut last_upstream: Option<String> = None;
    let mut last_signature: Option<String> = None;
    let mut no_progress_streak: u32 = 0;

    loop {
        if cancel.is_cancelled() {
            return Err(AgentFailure::cancelled());
        }
        turns += 1;
        if turns > max_turns {
            let last = messages
                .iter()
                .rev()
                .find(|m| m.role == Role::Assistant)
                .cloned()
                .unwrap_or_else(|| Message::assistant(""));
            return Ok(AgentOutcome {
                status: AgentStatus::CapExhausted,
                message: last,
                usage,
                stop_reason: StopReason::EndTurn,
                turns: turns - 1,
                tool_calls_executed,
                upstream: last_upstream,
                pending_external_calls: Vec::new(),
            });
        }

        emit(&event_tx, LoopEvent::TurnStart { turn: turns });

        let mut req = request.clone();
        req.messages = messages.clone();

        // Dispatch, retrying once on transient errors within the turn (D10).
        let (msg, stop_reason, turn_usage, upstream) =
            match dispatch_turn(provider.clone(), &req, config, &cancel).await {
                Ok(v) => v,
                Err(mut failure) => {
                    // D10: fail with the partial transcript (assistant
                    // messages produced before the failure).
                    failure.partial = messages
                        .iter()
                        .rev()
                        .find(|m| m.role == Role::Assistant)
                        .cloned()
                        .map(Box::new);
                    return Err(failure);
                }
            };

        usage = sum_usage(usage, turn_usage);

        // Cascade drift (D12): a changed serving upstream must not inherit
        // the previous (possibly weaker) model's thinking blocks.
        //
        // Accepted limitation (deliberate, matches `run_query_loop`): the
        // upstream is only known AFTER a dispatch returns (via
        // `ProviderAttribution`), so this strip lags one turn — the first
        // turn served by a new upstream sees the previous upstream's
        // thinking, and every subsequent dispatch is clean. Blast radius is
        // one turn (the fallback turn, already degraded by the switch), and
        // pinned routes (free/<upstream>) never switch at all. Revisit only
        // if (a) empirical evidence shows contamination matters on free-tier
        // cascades, or (b) FreeProvider exposes the planned upstream before
        // dispatch, which would let the strip run at request-build time.
        if let Some(u) = &upstream {
            if last_upstream.as_deref().is_some_and(|prev| prev != u) {
                strip_thinking_from_trajectory(&mut messages);
            }
            last_upstream = Some(u.clone());
        }

        messages.push(msg.clone());
        emit(
            &event_tx,
            LoopEvent::TurnEnd {
                stop_reason: stop_reason.clone(),
            },
        );

        if stop_reason != StopReason::ToolUse {
            return Ok(AgentOutcome::completed(
                msg,
                usage,
                stop_reason,
                turns,
                tool_calls_executed,
                last_upstream,
            ));
        }

        let calls = extract_tool_uses(&msg);
        if calls.is_empty() {
            // Degenerate: model signalled ToolUse but emitted none.
            return Ok(AgentOutcome::completed(
                msg,
                usage,
                StopReason::EndTurn,
                turns,
                tool_calls_executed,
                last_upstream,
            ));
        }

        // D9: cap exhaustion force-stops; never execute, never yield.
        if tool_calls_executed >= config.max_tool_calls {
            return Ok(AgentOutcome {
                status: AgentStatus::CapExhausted,
                message: msg,
                usage,
                stop_reason,
                turns,
                tool_calls_executed,
                upstream: last_upstream,
                pending_external_calls: Vec::new(),
            });
        }

        let (internal, external) = executor.partition_calls(&calls);

        if !external.is_empty() {
            if config.yield_mixed_turns {
                // Chat completions: yield EVERYTHING (relay semantics). The
                // client executes every declared function and resubmits.
                for call in &calls {
                    if let ContentBlock::ToolUse {
                        id, name, input, ..
                    } = call
                    {
                        emit(
                            &event_tx,
                            LoopEvent::ExternalToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                            },
                        );
                    }
                }
                return Ok(AgentOutcome {
                    status: AgentStatus::Yielding,
                    message: msg,
                    usage,
                    stop_reason,
                    turns,
                    tool_calls_executed,
                    upstream: last_upstream,
                    pending_external_calls: calls,
                });
            }
            // Responses: execute internal calls, then yield external.
            let results = execute_internal(
                executor,
                &internal,
                &cancel,
                config.tool_result_budget,
                &event_tx,
            )
            .await;
            tool_calls_executed += internal.len() as u32;
            messages.push(Message::user_blocks(results));
            return Ok(AgentOutcome {
                status: AgentStatus::Yielding,
                message: msg,
                usage,
                stop_reason,
                turns,
                tool_calls_executed,
                upstream: last_upstream,
                pending_external_calls: external,
            });
        }

        // All-internal turn: execute and continue the loop.
        let results = execute_internal(
            executor,
            &internal,
            &cancel,
            config.tool_result_budget,
            &event_tx,
        )
        .await;
        tool_calls_executed += internal.len() as u32;
        messages.push(Message::user_blocks(results));

        // No-progress guard: `no_progress_stop_streak` consecutive identical
        // tool-call signatures stop the loop early.
        let signature = calls_signature(&internal);
        if last_signature.as_deref() == Some(signature.as_str()) {
            no_progress_streak += 1;
        } else {
            no_progress_streak = 1;
            last_signature = Some(signature);
        }
        if no_progress_streak >= config.no_progress_stop_streak {
            return Ok(AgentOutcome {
                status: AgentStatus::NoProgress,
                message: msg,
                usage,
                stop_reason,
                turns,
                tool_calls_executed,
                upstream: last_upstream,
                pending_external_calls: Vec::new(),
            });
        }
    }
}

/// Execute internal tool calls, emitting per-call events (parallel, ordered).
async fn execute_internal(
    executor: &GatewayToolExecutor,
    calls: &[ContentBlock],
    cancel: &CancellationToken,
    budget: usize,
    event_tx: &Option<UnboundedSender<LoopEvent>>,
) -> Vec<ContentBlock> {
    for call in calls {
        if let ContentBlock::ToolUse {
            id, name, input, ..
        } = call
        {
            emit(
                event_tx,
                LoopEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
            );
        }
    }
    let results = executor.execute_all(calls, cancel, 4, budget).await;
    for (call, block) in calls.iter().zip(results.iter()) {
        if let ContentBlock::ToolUse { id, name, .. } = call {
            if let ContentBlock::ToolResult {
                is_error, content, ..
            } = block
            {
                let result = match content {
                    clawde_core::types::ToolResultContent::Text(t) => t.clone(),
                    clawde_core::types::ToolResultContent::Blocks(_) => String::new(),
                };
                emit(
                    event_tx,
                    LoopEvent::ToolExecuted {
                        id: id.clone(),
                        name: name.clone(),
                        result,
                        is_error: is_error.unwrap_or(false),
                    },
                );
            }
        }
    }
    results
}

/// Dispatch one turn through the provider's streaming path (which carries
/// upstream attribution for cascade-drift tracking) and accumulate the
/// assistant message. Retries once on transient errors (D10).
async fn dispatch_turn(
    provider: Arc<dyn LlmProvider>,
    req: &ProviderRequest,
    config: &AgentConfig,
    cancel: &CancellationToken,
) -> Result<(Message, StopReason, UsageInfo, Option<String>), AgentFailure> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let stream = match provider_call(provider.clone(), req, config.timeout_secs).await {
            Ok(s) => s,
            Err(e) => {
                if attempt == 1 && is_transient(&e) {
                    continue;
                }
                return Err(AgentFailure::from_provider(&e, None));
            }
        };
        match collect_turn(stream, cancel).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt == 1 && is_transient(&e) {
                    continue;
                }
                return Err(AgentFailure::from_provider(&e, None));
            }
        }
    }
}

/// Is this provider error worth a single in-turn retry?
fn is_transient(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::RateLimited { .. }
            | ProviderError::ServerError {
                is_retryable: true,
                ..
            }
    )
}

/// Call `create_message_stream` with the configured timeout.
async fn provider_call(
    provider: Arc<dyn LlmProvider>,
    req: &ProviderRequest,
    timeout_secs: u64,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError> {
    if timeout_secs == 0 {
        return provider.create_message_stream(req.clone()).await;
    }
    match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        provider.create_message_stream(req.clone()),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(ProviderError::ServerError {
            provider: provider.id().clone(),
            status: Some(504),
            message: "Upstream request timed out".to_string(),
            is_retryable: true,
        }),
    }
}

/// Accumulate one turn's stream into an assistant message. Tracks the
/// serving upstream via `ProviderAttribution`.
async fn collect_turn(
    mut stream: Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
    cancel: &CancellationToken,
) -> Result<(Message, StopReason, UsageInfo, Option<String>), ProviderError> {
    use futures::StreamExt;

    let mut blocks: Vec<ContentBlock> = Vec::new();
    // Tool-call argument fragments keyed by block index (accumulated, never
    // replaced — OpenAI streaming tool-call semantics).
    let mut tool_args: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut tool_index_to_block: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = UsageInfo::default();
    let mut upstream: Option<String> = None;

    loop {
        let event = tokio::select! {
            event = stream.next() => event,
            _ = cancel.cancelled() => None,
        };
        let Some(event) = event else { break };
        let event = event?;
        match event {
            StreamEvent::MessageStart {
                id: _,
                model: _,
                usage: u,
            } => {
                usage.input_tokens = u.input_tokens;
                usage.cache_creation_input_tokens = u.cache_creation_input_tokens;
                usage.cache_read_input_tokens = u.cache_read_input_tokens;
            }
            StreamEvent::ProviderAttribution { upstream_id, .. } => {
                upstream = Some(upstream_id);
            }
            StreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                // Assign the provider's block index our own positional slot.
                let pos = blocks.len();
                tool_index_to_block.insert(index, pos);
                blocks.push(content_block);
            }
            StreamEvent::TextDelta { index, text } => {
                append_text(&mut blocks, index, &tool_index_to_block, text);
            }
            StreamEvent::ThinkingDelta { index, thinking } => {
                append_thinking(&mut blocks, index, &tool_index_to_block, thinking);
            }
            StreamEvent::ReasoningDelta { index, reasoning } => {
                append_thinking(&mut blocks, index, &tool_index_to_block, reasoning);
            }
            StreamEvent::InputJsonDelta {
                index,
                partial_json,
            } => {
                let entry = tool_args.entry(index).or_default();
                entry.push_str(&partial_json);
            }
            StreamEvent::SignatureDelta { index, signature } => {
                if let Some(pos) = tool_index_to_block.get(&index).copied() {
                    if let Some(ContentBlock::ToolUse {
                        thought_signature, ..
                    }) = blocks.get_mut(pos)
                    {
                        *thought_signature = Some(signature);
                    }
                }
            }
            StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta {
                stop_reason: sr,
                usage: u,
            } => {
                if let Some(sr) = sr {
                    stop_reason = sr;
                }
                if let Some(u) = u {
                    usage.output_tokens = u.output_tokens;
                    usage.reasoning_tokens = u.reasoning_tokens;
                }
            }
            StreamEvent::MessageStop => break,
            StreamEvent::Error { message, .. } => {
                return Err(ProviderError::StreamError {
                    provider: clawde_core::ProviderId::from("gateway"),
                    message,
                    partial_response: None,
                });
            }
            _ => {}
        }
    }

    // Finalize tool-call arguments: parse accumulated JSON (E6 — a parse
    // failure becomes a null input, which the executor rejects safely).
    for (index, args) in tool_args {
        if let Some(pos) = tool_index_to_block.get(&index).copied() {
            if let Some(ContentBlock::ToolUse { input, .. }) = blocks.get_mut(pos) {
                *input = serde_json::from_str(&args).unwrap_or(Value::Null);
            }
        }
    }

    let message = Message::assistant_blocks(blocks);
    Ok((message, stop_reason, usage, upstream))
}

fn append_text(
    blocks: &mut [ContentBlock],
    index: usize,
    mapping: &std::collections::HashMap<usize, usize>,
    text: String,
) {
    let pos = mapping
        .get(&index)
        .copied()
        .unwrap_or_else(|| blocks.len().saturating_sub(1));
    if let Some(ContentBlock::Text { text: existing }) = blocks.get_mut(pos) {
        existing.push_str(&text);
    } else if let Some(ContentBlock::Text { text: existing }) = blocks.last_mut() {
        existing.push_str(&text);
    }
}

fn append_thinking(
    blocks: &mut [ContentBlock],
    index: usize,
    mapping: &std::collections::HashMap<usize, usize>,
    thinking: String,
) {
    let pos = mapping
        .get(&index)
        .copied()
        .unwrap_or_else(|| blocks.len().saturating_sub(1));
    if let Some(ContentBlock::Thinking {
        thinking: existing, ..
    }) = blocks.get_mut(pos)
    {
        existing.push_str(&thinking);
    } else if let Some(ContentBlock::Thinking {
        thinking: existing, ..
    }) = blocks.last_mut()
    {
        existing.push_str(&thinking);
    }
}

/// Collect `ToolUse` blocks from an assistant message.
fn extract_tool_uses(msg: &Message) -> Vec<ContentBlock> {
    match &msg.content {
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

/// A stable signature for the no-progress guard (name + input, in order).
fn calls_signature(calls: &[ContentBlock]) -> String {
    let mut parts = Vec::with_capacity(calls.len());
    for call in calls {
        if let ContentBlock::ToolUse { name, input, .. } = call {
            parts.push(format!("{}:{}", name, input));
        }
    }
    parts.join("|")
}

fn sum_usage(a: UsageInfo, b: UsageInfo) -> UsageInfo {
    UsageInfo {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
        cache_creation_input_tokens: a.cache_creation_input_tokens + b.cache_creation_input_tokens,
        cache_read_input_tokens: a.cache_read_input_tokens + b.cache_read_input_tokens,
        reasoning_tokens: a.reasoning_tokens + b.reasoning_tokens,
    }
}

/// Strip `Thinking` blocks from prior assistant turns (D12 cascade drift).
fn strip_thinking_from_trajectory(messages: &mut [Message]) {
    for msg in messages.iter_mut() {
        if msg.role != Role::Assistant {
            continue;
        }
        if let MessageContent::Blocks(blocks) = &mut msg.content {
            blocks.retain(|b| !matches!(b, ContentBlock::Thinking { .. }));
        }
    }
}

fn emit(tx: &Option<UnboundedSender<LoopEvent>>, event: LoopEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_exec::{GatewayPermissionMode, GatewayToolExecutor};
    use async_trait::async_trait;
    use clawde_api::provider_types::{
        ProviderCapabilities, ProviderResponse, ProviderStatus, SystemPrompt, SystemPromptStyle,
    };
    use clawde_core::provider_id::ProviderId;
    use clawde_core::types::{ToolDefinition, ToolResultContent};
    use futures::stream;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ------------------------------------------------------------------
    // Mock provider
    // ------------------------------------------------------------------

    #[derive(Clone)]
    struct MockToolCall {
        id: String,
        name: String,
        input: Value,
    }

    #[derive(Clone)]
    struct MockTurn {
        /// Tool calls the model makes this turn (empty = text reply).
        tool_calls: Vec<MockToolCall>,
        /// Text reply (used when `tool_calls` is empty).
        text: String,
        /// Thinking prefix emitted before the text/tools.
        thinking: Option<String>,
        /// Emit a provider error instead of a normal turn.
        error: Option<ProviderError>,
        /// Upstream attribution for this turn (cascade-drift tests).
        upstream: Option<String>,
    }

    impl MockTurn {
        fn text(text: &str) -> Self {
            Self {
                tool_calls: Vec::new(),
                text: text.to_string(),
                thinking: None,
                error: None,
                upstream: None,
            }
        }

        fn tool_call(name: &str, id: &str, input: Value) -> Self {
            Self {
                tool_calls: vec![MockToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    input,
                }],
                text: String::new(),
                thinking: None,
                error: None,
                upstream: None,
            }
        }

        fn tool_calls(calls: Vec<MockToolCall>) -> Self {
            Self {
                tool_calls: calls,
                text: String::new(),
                thinking: None,
                error: None,
                upstream: None,
            }
        }

        fn error(err: ProviderError) -> Self {
            Self {
                tool_calls: Vec::new(),
                text: String::new(),
                thinking: None,
                error: Some(err),
                upstream: None,
            }
        }
    }

    struct MockProvider {
        id: ProviderId,
        script: Vec<MockTurn>,
        calls: AtomicUsize,
        /// Messages received on each dispatch (for cascade-drift assertions).
        received: std::sync::Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "mock-agent-provider"
        }

        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: Some(500),
                message: "mock only supports streaming".to_string(),
                is_retryable: false,
            })
        }

        async fn create_message_stream(
            &self,
            request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let idx = self.calls.fetch_add(1, Ordering::SeqCst);
            if let Ok(mut received) = self.received.lock() {
                received.push(request.messages.clone());
            }
            let turn = self
                .script
                .get(idx)
                .cloned()
                .unwrap_or_else(|| MockTurn::text("fallback end turn (script exhausted)"));

            if let Some(err) = turn.error {
                return Ok(Box::pin(stream::iter(vec![Err(err)])));
            }

            let mut events: Vec<Result<StreamEvent, ProviderError>> = Vec::new();
            if let Some(upstream) = &turn.upstream {
                events.push(Ok(StreamEvent::ProviderAttribution {
                    provider_id: "free".to_string(),
                    upstream_id: upstream.clone(),
                    model: "mock-model".to_string(),
                }));
            }
            events.push(Ok(StreamEvent::MessageStart {
                id: format!("msg_{idx}"),
                model: "mock-model".to_string(),
                usage: UsageInfo {
                    input_tokens: 5,
                    output_tokens: 0,
                    ..UsageInfo::default()
                },
            }));

            if let Some(thinking) = &turn.thinking {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    content_block: ContentBlock::Thinking {
                        thinking: String::new(),
                        signature: String::new(),
                    },
                }));
                events.push(Ok(StreamEvent::ThinkingDelta {
                    index: 0,
                    thinking: thinking.clone(),
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
            }

            let mut next_index = if turn.thinking.is_some() { 1 } else { 0 };
            for call in &turn.tool_calls {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: next_index,
                    content_block: ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: Value::Null,
                        thought_signature: None,
                    },
                }));
                let args = call.input.to_string();
                if !args.is_empty() {
                    events.push(Ok(StreamEvent::InputJsonDelta {
                        index: next_index,
                        partial_json: args,
                    }));
                }
                events.push(Ok(StreamEvent::ContentBlockStop { index: next_index }));
                next_index += 1;
            }

            if turn.tool_calls.is_empty() && !turn.text.is_empty() {
                events.push(Ok(StreamEvent::ContentBlockStart {
                    index: next_index,
                    content_block: ContentBlock::Text {
                        text: String::new(),
                    },
                }));
                events.push(Ok(StreamEvent::TextDelta {
                    index: next_index,
                    text: turn.text.clone(),
                }));
                events.push(Ok(StreamEvent::ContentBlockStop { index: next_index }));
            }

            let stop = if turn.tool_calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolUse
            };
            events.push(Ok(StreamEvent::MessageDelta {
                stop_reason: Some(stop),
                usage: Some(UsageInfo {
                    input_tokens: 5,
                    output_tokens: 7,
                    ..UsageInfo::default()
                }),
            }));
            events.push(Ok(StreamEvent::MessageStop));

            Ok(Box::pin(stream::iter(events)))
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            Ok(ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: true,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: SystemPromptStyle::SystemMessage,
            }
        }
    }

    fn provider(script: Vec<MockTurn>) -> Arc<MockProvider> {
        Arc::new(MockProvider {
            id: ProviderId::new("mock"),
            script,
            calls: AtomicUsize::new(0),
            received: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn executor(mode: GatewayPermissionMode) -> GatewayToolExecutor {
        GatewayToolExecutor::new(mode, &[], "agent-loop-test", &[], CancellationToken::new())
    }

    fn request(messages: Vec<Message>) -> ProviderRequest {
        ProviderRequest {
            model: "free/auto".to_string(),
            messages,
            system_prompt: Some(SystemPrompt::Text("test".to_string())),
            tools: vec![ToolDefinition {
                name: "Read".to_string(),
                description: "Read a file".to_string(),
                input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            }],
            max_tokens: 256,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Vec::new(),
            thinking: None,
            effort_level: None,
            provider_options: json!({}),
            strict_route: false,
        }
    }

    fn read_call(id: &str, path: &str) -> MockToolCall {
        MockToolCall {
            id: id.to_string(),
            name: "Read".to_string(),
            input: json!({"path": path}),
        }
    }

    fn run(
        provider: Arc<dyn LlmProvider>,
        executor: &GatewayToolExecutor,
        config: &AgentConfig,
    ) -> Result<AgentOutcome, AgentFailure> {
        let cancel = CancellationToken::new();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_agent_loop(
                provider,
                request(vec![Message::user("hello")]),
                executor,
                config,
                cancel,
                None,
            ))
    }

    #[test]
    fn end_turn_returns_text_message() {
        let mock = provider(vec![MockTurn::text("hi there")]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        assert_eq!(out.message.get_text(), Some("hi there"));
        assert_eq!(out.turns, 1);
        assert_eq!(out.tool_calls_executed, 0);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn tool_use_executes_then_end_turn() {
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", json!({"path": "/nonexistent/x"})),
            MockTurn::text("done reading"),
        ]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        assert_eq!(out.message.get_text(), Some("done reading"));
        assert_eq!(out.tool_calls_executed, 1);
        assert_eq!(out.turns, 2);
        // The transcript sent on the second dispatch includes the tool result.
        let received = mock.received.lock().unwrap();
        let second = &received[1];
        assert!(second.iter().any(|m| {
            matches!(
                &m.content,
                MessageContent::Blocks(blocks)
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            )
        }));
    }

    #[test]
    fn cap_exhausted_force_stops() {
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", json!({"path": "/nonexistent/x"})),
            MockTurn::tool_call("Read", "c2", json!({"path": "/nonexistent/x"})),
            MockTurn::text("unused"),
        ]);
        let config = AgentConfig {
            max_tool_calls: 1,
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::CapExhausted);
        assert_eq!(out.tool_calls_executed, 1);
        // Only two dispatches: the second tool turn is force-stopped, the
        // text turn is never reached.
        assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn no_progress_guard_stops_after_repeats() {
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", json!({"path": "/nonexistent/x"})),
            MockTurn::tool_call("Read", "c2", json!({"path": "/nonexistent/x"})),
            MockTurn::tool_call("Read", "c3", json!({"path": "/nonexistent/x"})),
            MockTurn::text("unused"),
        ]);
        let config = AgentConfig {
            no_progress_stop_streak: 3,
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::NoProgress);
        assert_eq!(out.tool_calls_executed, 3);
    }

    #[test]
    fn external_tool_yielded_in_chat_mode() {
        let mock = provider(vec![MockTurn::tool_call(
            "get_weather",
            "c1",
            json!({"city": "SF"}),
        )]);
        let config = AgentConfig {
            yield_mixed_turns: true,
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Yielding);
        assert_eq!(out.pending_external_calls.len(), 1);
        assert_eq!(out.tool_calls_executed, 0);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn mixed_turn_yields_everything_in_chat_mode() {
        let mock = provider(vec![MockTurn::tool_calls(vec![
            read_call("c1", "/nonexistent/x"),
            MockToolCall {
                id: "c2".into(),
                name: "get_weather".into(),
                input: json!({"city": "SF"}),
            },
        ])]);
        let config = AgentConfig {
            yield_mixed_turns: true,
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Yielding);
        // BOTH calls yielded; nothing executed (relay semantics).
        assert_eq!(out.pending_external_calls.len(), 2);
        assert_eq!(out.tool_calls_executed, 0);
    }

    #[test]
    fn mixed_turn_executes_internal_then_yields_external() {
        let mock = provider(vec![MockTurn::tool_calls(vec![
            read_call("c1", "/nonexistent/x"),
            MockToolCall {
                id: "c2".into(),
                name: "get_weather".into(),
                input: json!({"city": "SF"}),
            },
        ])]);
        let config = AgentConfig {
            yield_mixed_turns: false, // Responses mode
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Yielding);
        assert_eq!(out.pending_external_calls.len(), 1);
        let ContentBlock::ToolUse {
            id, name, input, ..
        } = &out.pending_external_calls[0]
        else {
            panic!("expected tool use");
        };
        assert_eq!(id, "c2");
        assert_eq!(name, "get_weather");
        assert_eq!(input, &json!({"city": "SF"}));
        assert_eq!(out.tool_calls_executed, 1);
    }

    #[test]
    fn provider_error_mid_loop_fails_with_partial() {
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", json!({"path": "/nonexistent/x"})),
            MockTurn::error(ProviderError::ServerError {
                provider: ProviderId::new("mock"),
                status: Some(503),
                message: "boom".to_string(),
                is_retryable: false,
            }),
        ]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        );
        assert!(out.is_err());
        let failure = out.unwrap_err();
        assert!(failure.message.contains("boom"));
    }

    #[test]
    fn transient_error_retries_once() {
        let mock = provider(vec![
            MockTurn::error(ProviderError::ServerError {
                provider: ProviderId::new("mock"),
                status: Some(503),
                message: "transient".to_string(),
                is_retryable: true,
            }),
            MockTurn::text("recovered"),
        ]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        assert_eq!(out.message.get_text(), Some("recovered"));
        assert_eq!(mock.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rate_limited_turn_fails_with_retry_hint() {
        // RateLimited is transient → retried once; a second failure surfaces
        // with the retry hint (D10).
        let mock = provider(vec![
            MockTurn::error(ProviderError::RateLimited {
                provider: ProviderId::new("mock"),
                retry_after: Some(30),
            }),
            MockTurn::error(ProviderError::RateLimited {
                provider: ProviderId::new("mock"),
                retry_after: Some(30),
            }),
        ]);
        let failure = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap_err();
        assert_eq!(failure.retry_after_secs, Some(30));
    }

    #[test]
    fn pre_cancelled_loop_returns_cancelled() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mock = provider(vec![MockTurn::text("hi")]);
        let ex = executor(GatewayPermissionMode::Allow);
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(run_agent_loop(
                mock.clone(),
                request(vec![Message::user("hello")]),
                &ex,
                &AgentConfig::default(),
                cancel,
                None,
            ));
        assert!(matches!(result, Err(AgentFailure { .. })));
    }

    #[test]
    fn cascade_drift_strips_prior_thinking() {
        // Turn 1 is served by groq, turn 2 by cline (a switch). The strip is
        // applied after a turn's attribution arrives, so dispatch 3 sees the
        // switch: turn 1's thinking must be gone, turn 2's own thinking
        // (same upstream as turn 3) is preserved.
        let mock = provider(vec![
            MockTurn {
                tool_calls: vec![read_call("c1", "/nonexistent/a")],
                text: String::new(),
                thinking: Some("weak model reasoning".to_string()),
                error: None,
                upstream: Some("groq".to_string()),
            },
            MockTurn {
                tool_calls: vec![read_call("c2", "/nonexistent/b")],
                text: String::new(),
                thinking: Some("strong model reasoning".to_string()),
                error: None,
                upstream: Some("cline".to_string()),
            },
            MockTurn {
                tool_calls: Vec::new(),
                text: "final".to_string(),
                thinking: None,
                error: None,
                upstream: Some("cline".to_string()),
            },
        ]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        assert_eq!(out.upstream.as_deref(), Some("cline"));

        let received = mock.received.lock().unwrap();
        let final_dispatch = &received[2];
        // Turn 1 (groq) was stripped after the switch; turn 2 (cline) keeps
        // its own thinking since turn 3 shares its upstream.
        let assistant_blocks: Vec<&MessageContent> = final_dispatch
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .map(|m| &m.content)
            .collect();
        assert_eq!(assistant_blocks.len(), 2);
        let first = assistant_blocks[0];
        let second = assistant_blocks[1];
        let has_thinking = |content: &MessageContent| match content {
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { .. })),
            _ => false,
        };
        assert!(
            !has_thinking(first),
            "stale thinking from the switched-away upstream must be stripped"
        );
        assert!(
            has_thinking(second),
            "current upstream's own thinking is preserved"
        );
    }

    #[test]
    fn strip_thinking_removes_only_assistant_thinking() {
        let mut messages = vec![
            Message::user("user text"),
            Message::assistant_blocks(vec![
                ContentBlock::Thinking {
                    thinking: "think".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "text".into(),
                },
            ]),
            Message::assistant("plain"),
        ];
        strip_thinking_from_trajectory(&mut messages);
        assert!(matches!(
            &messages[1].content,
            MessageContent::Blocks(blocks)
                if blocks.len() == 1
                    && matches!(blocks[0], ContentBlock::Text { .. })
        ));
        assert!(matches!(&messages[2].content, MessageContent::Text(_)));
        assert!(matches!(&messages[0].content, MessageContent::Text(_)));
    }

    #[test]
    fn malformed_tool_args_produce_error_observation() {
        // Model emits a ToolUse whose arguments fail to parse (garbage JSON).
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", Value::Null),
            MockTurn::text("recovered"),
        ]);
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &AgentConfig::default(),
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        let received = mock.received.lock().unwrap();
        let second = &received[1];
        assert!(second.iter().any(|m| {
            matches!(
                &m.content,
                MessageContent::Blocks(blocks)
                    if blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { is_error: Some(true), .. }))
            )
        }));
    }

    #[test]
    fn tool_result_budget_truncates() {
        let mock = provider(vec![
            MockTurn::tool_call("Read", "c1", json!({"path": "/nonexistent/x"})),
            MockTurn::text("done"),
        ]);
        let config = AgentConfig {
            tool_result_budget: 10,
            ..AgentConfig::default()
        };
        let out = run(
            mock.clone(),
            &executor(GatewayPermissionMode::Allow),
            &config,
        )
        .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        let received = mock.received.lock().unwrap();
        let second = &received[1];
        for msg in second {
            if let MessageContent::Blocks(blocks) = &msg.content {
                for b in blocks {
                    if let ContentBlock::ToolResult {
                        content: ToolResultContent::Text(t),
                        ..
                    } = b
                    {
                        assert!(t.len() <= 10 + 16, "result must be budget-truncated");
                    }
                }
            }
        }
    }
}
