//! `session/prompt` handler — drives the Clawde query loop and forwards
//! every meaningful event back to the ACP client as a `session/update`
//! notification.

use std::collections::HashMap;
use std::sync::Arc;

use agent_client_protocol_schema as acp;
use clawde_api::streaming::{AnthropicStreamEvent, ContentDelta};
use clawde_core::types::{ContentBlock, ImageSource, Message};
use clawde_query::{QueryEvent, QueryOutcome};
use clawde_tools::ToolContext;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::connection::Connection;
use crate::permission::AcpPermissionHandler;
use crate::runtime::AgentRuntime;
use crate::sessions::SessionState;

/// Handle one `session/prompt` JSON-RPC call.
///
/// Drives the full Clawde query loop with the runtime's tools, MCP servers,
/// and provider registry, while streaming every text delta, thinking delta,
/// and tool invocation back as `session/update` notifications. Returns the
/// final `PromptResponse` with the appropriate `StopReason`.
pub async fn handle(
    runtime: Arc<AgentRuntime>,
    connection: Arc<Connection>,
    session: Arc<SessionState>,
    params: acp::PromptRequest,
) -> Result<acp::PromptResponse, acp::Error> {
    // ACP request dispatch is concurrent, but a session transcript and its
    // tool execution are ordered state. Hold this guard for the full turn so
    // a second prompt cannot clone stale messages and overwrite the first
    // prompt's transcript when it completes.
    let _prompt_guard = session.lock_prompt().await;

    // Convert prompt content blocks → a single user message in Clawde's
    // internal format.
    let prompt_blocks = render_prompt_blocks(&params.prompt);
    if prompt_blocks.is_empty() {
        return Err(acp::Error::invalid_params());
    }

    // Append the user turn to the session transcript, preserving supported
    // image blocks so the provider can receive visual context instead of an
    // apparently empty text-only prompt.
    let mut messages: Vec<Message> = {
        let guard = session.messages.lock();
        guard.clone()
    };
    messages.push(Message::user_blocks(prompt_blocks));

    // Reset the session's cancellation token for this new turn.
    let cancel = session.current_cancel_token();

    // Build per-session configuration. ACP additional directories are
    // permission roots for this session only; never mutate the shared runtime
    // config because multiple ACP sessions may run concurrently.
    let mut session_config = runtime.config.clone();
    session_config.project_dir = Some(session.cwd.clone());
    session_config
        .additional_dirs
        .extend(session.additional_directories.iter().cloned());

    // Build a session-specific tool snapshot when this session owns MCP
    // connections. Empty sessions continue to reuse the startup registry.
    let session_mcp = session.mcp.manager();
    let session_tools = runtime.tools_for_session(session_mcp.clone());

    // Build per-session ToolContext.
    let permission_handler: Arc<dyn clawde_core::PermissionHandler> = Arc::new(
        AcpPermissionHandler::new(session.permission_manager.clone()),
    );
    let tool_ctx = ToolContext {
        working_dir: session.cwd.clone(),
        permission_mode: runtime.config.permission_mode.clone(),
        permission_handler,
        cost_tracker: runtime.cost_tracker.clone(),
        session_id: session.session_id.0.to_string(),
        file_history: session.file_history.clone(),
        current_turn: session.current_turn.clone(),
        non_interactive: false, // ACP routes permissions via the bridge
        mcp_manager: session_mcp.or_else(|| runtime.mcp_manager.clone()),
        config: session_config.clone(),
        provider_registry: Some(runtime.provider_registry.clone()),
        managed_agent_config: session_config.managed_agents.clone(),
        // Effort is rebound per turn by run_query_loop from the QueryConfig.
        effort: None,
        completion_notifier: None,
        pending_permissions: Some(session.pending_permissions.clone()),
        permission_manager: Some(session.permission_manager.clone()),
        user_question_tx: None,
        // Bind to this turn's cancel token so the parallel tool executor and any
        // sub-agents observe cancellation (issue #218). `run_query_loop` also
        // rebinds this to the token it is driven by.
        cancel_token: cancel.clone(),
    };

    // Spawn the permission drainer for this turn.
    let drainer_cancel = CancellationToken::new();
    let drainer = crate::permission::spawn_drainer(
        connection.clone(),
        session.session_id.clone(),
        session.pending_permissions.clone(),
        session.permission_manager.clone(),
        drainer_cancel.clone(),
    );

    // Event channel + forwarder.
    let (ev_tx, ev_rx) = mpsc::unbounded_channel::<QueryEvent>();
    let forwarder = tokio::spawn(forward_events(
        connection.clone(),
        session.session_id.clone(),
        ev_rx,
    ));

    // Run the query loop with the session's working directory reflected in
    // prompt/tool metadata as well as the ToolContext.
    let mut query_config = runtime.query_config.clone();
    query_config.working_directory = Some(session.cwd.display().to_string());
    let outcome = clawde_query::run_query_loop(
        runtime.api_client.as_ref(),
        &mut messages,
        session_tools.as_slice(),
        &tool_ctx,
        &query_config,
        runtime.cost_tracker.clone(),
        Some(ev_tx),
        cancel,
        None,
    )
    .await;

    // Tear down forwarder + drainer.
    drainer_cancel.cancel();
    let _ = drainer.await;
    // Forwarder finishes when ev_tx is dropped at end of run_query_loop.
    let _ = forwarder.await;

    // Persist the updated transcript.
    {
        let mut guard = session.messages.lock();
        *guard = messages;
    }

    let stop_reason = match outcome {
        QueryOutcome::EndTurn { .. } => acp::StopReason::EndTurn,
        QueryOutcome::MaxTokens { .. } => acp::StopReason::MaxTokens,
        QueryOutcome::Cancelled => acp::StopReason::Cancelled,
        QueryOutcome::BudgetExceeded { .. } => acp::StopReason::MaxTurnRequests,
        QueryOutcome::Error(e) => {
            error!(error = ?e, "ACP: query loop errored");
            acp::StopReason::Refusal
        }
    };

    Ok(acp::PromptResponse::new(stop_reason))
}

/// Convert ACP prompt blocks into Clawde's provider-facing content blocks.
///
/// ACP images are base64 payloads (or URI-backed when data is empty), and the
/// core/API message model already has a compatible image representation. Audio
/// remains deliberately unsupported until the provider request path supports
/// it end-to-end; it is not silently converted to text.
fn render_prompt_blocks(blocks: &[acp::ContentBlock]) -> Vec<ContentBlock> {
    let mut rendered = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(text) => {
                if !text.text.is_empty() {
                    rendered.push(ContentBlock::Text {
                        text: text.text.clone(),
                    });
                }
            }
            acp::ContentBlock::ResourceLink(link) => rendered.push(ContentBlock::Text {
                text: format!("[resource link: {}]", link.uri),
            }),
            acp::ContentBlock::Resource(resource) => {
                // Preserve embedded text. Binary resources require a resolver;
                // retain an explicit bounded marker instead of dropping context.
                let json = serde_json::to_value(resource).unwrap_or_default();
                if let Some(text) = json
                    .get("resource")
                    .and_then(|value| value.get("text"))
                    .and_then(|value| value.as_str())
                {
                    rendered.push(ContentBlock::Text {
                        text: text.to_string(),
                    });
                } else {
                    rendered.push(ContentBlock::Text {
                        text: "[embedded binary resource omitted]".to_string(),
                    });
                }
            }
            acp::ContentBlock::Image(image) => {
                let source = if !image.data.is_empty() {
                    ImageSource {
                        source_type: "base64".to_string(),
                        media_type: Some(image.mime_type.clone()),
                        data: Some(image.data.clone()),
                        url: None,
                    }
                } else if let Some(uri) = image.uri.clone() {
                    ImageSource {
                        source_type: "url".to_string(),
                        media_type: Some(image.mime_type.clone()),
                        data: None,
                        url: Some(uri),
                    }
                } else {
                    warn!("ACP: dropping image with neither data nor URI");
                    continue;
                };
                rendered.push(ContentBlock::Image { source });
            }
            acp::ContentBlock::Audio(_) => {
                warn!("ACP: ignoring audio content block (capability not advertised)");
            }
            _ => warn!("ACP: ignoring unknown content block variant"),
        }
    }
    rendered
}

/// Convert a query tool result into ACP's raw-output shape.
///
/// Successful results retain their existing JSON-or-string representation.
/// Failed results are wrapped so ACP clients can distinguish stable tool
/// categories such as `network_isolation_blocked` from a generic execution
/// failure without parsing human-readable text.
fn acp_tool_output(result: &str, is_error: bool, error_code: Option<&str>) -> serde_json::Value {
    if !is_error && error_code.is_none() {
        return serde_json::from_str(result)
            .unwrap_or_else(|_| serde_json::Value::String(result.to_string()));
    }

    let mut output = serde_json::Map::new();
    output.insert(
        "error".to_string(),
        serde_json::Value::String(result.to_string()),
    );
    if let Some(code) = error_code {
        output.insert(
            "error_code".to_string(),
            serde_json::Value::String(code.to_string()),
        );
    }
    serde_json::Value::Object(output)
}

/// Pump QueryEvents → `session/update` SessionNotifications.
async fn forward_events(
    connection: Arc<Connection>,
    session_id: acp::SessionId,
    mut rx: mpsc::UnboundedReceiver<QueryEvent>,
) {
    // Track tool calls so ToolEnd updates carry the right title and kind.
    let mut active_tools: HashMap<String, ToolMeta> = HashMap::new();

    while let Some(event) = rx.recv().await {
        match event {
            QueryEvent::Stream(AnthropicStreamEvent::ContentBlockDelta { delta, .. }) => {
                match delta {
                    ContentDelta::TextDelta { text } => {
                        send_text_chunk(&connection, &session_id, &text, false).await;
                    }
                    ContentDelta::ThinkingDelta { thinking } => {
                        send_text_chunk(&connection, &session_id, &thinking, true).await;
                    }
                    _ => {}
                }
            }
            QueryEvent::ToolStart {
                tool_name,
                tool_id,
                input_json,
            } => {
                let kind = classify_tool_kind(&tool_name);
                let raw_input = serde_json::from_str::<serde_json::Value>(&input_json).ok();
                let title = tool_title(&tool_name, raw_input.as_ref());
                active_tools.insert(
                    tool_id.clone(),
                    ToolMeta {
                        title: title.clone(),
                        kind,
                    },
                );
                let mut tool_call =
                    acp::ToolCall::new(acp::ToolCallId::new(tool_id.as_str()), title)
                        .kind(kind)
                        .status(acp::ToolCallStatus::InProgress);
                if let Some(input) = raw_input {
                    tool_call = tool_call.raw_input(Some(input));
                }
                send_session_update(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCall(tool_call),
                )
                .await;
            }
            QueryEvent::ToolEnd {
                tool_name: _,
                tool_id,
                result,
                is_error,
                error_code,
            } => {
                let status = if is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let content = vec![acp::ToolCallContent::Content(acp::Content::new(
                    acp::ContentBlock::Text(acp::TextContent::new(result.clone())),
                ))];
                // Preserve the human-readable result in `content`, while
                // carrying the stable machine-readable category through ACP
                // raw_output for clients that need recovery/telemetry.
                let raw_output = acp_tool_output(&result, is_error, error_code.as_deref());
                let fields = acp::ToolCallUpdateFields::new()
                    .status(status)
                    .content(content)
                    .raw_output(Some(raw_output));
                let update =
                    acp::ToolCallUpdate::new(acp::ToolCallId::new(tool_id.as_str()), fields);
                send_session_update(
                    &connection,
                    &session_id,
                    acp::SessionUpdate::ToolCallUpdate(update),
                )
                .await;
                active_tools.remove(&tool_id);
            }
            QueryEvent::Error(msg) => {
                send_text_chunk(
                    &connection,
                    &session_id,
                    &format!("\n[error: {}]", msg),
                    false,
                )
                .await;
            }
            _ => {}
        }
    }
}

struct ToolMeta {
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    kind: acp::ToolKind,
}

async fn send_text_chunk(
    connection: &Arc<Connection>,
    session_id: &acp::SessionId,
    text: &str,
    is_thought: bool,
) {
    let chunk = acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)));
    let update = if is_thought {
        acp::SessionUpdate::AgentThoughtChunk(chunk)
    } else {
        acp::SessionUpdate::AgentMessageChunk(chunk)
    };
    send_session_update(connection, session_id, update).await;
}

async fn send_session_update(
    connection: &Arc<Connection>,
    session_id: &acp::SessionId,
    update: acp::SessionUpdate,
) {
    let notif = acp::SessionNotification::new(session_id.clone(), update);
    if let Err(e) = connection.send_notification("session/update", notif).await {
        warn!(?e, "ACP: failed to send session/update");
    } else {
        debug!("ACP: sent session/update");
    }
}

fn classify_tool_kind(tool_name: &str) -> acp::ToolKind {
    match tool_name {
        "Read" | "FileRead" => acp::ToolKind::Read,
        "Edit" | "FileEdit" | "Write" | "FileWrite" | "BatchEdit" | "ApplyPatch" => {
            acp::ToolKind::Edit
        }
        "Bash" | "Shell" | "Execute" => acp::ToolKind::Execute,
        "WebFetch" | "WebSearch" => acp::ToolKind::Fetch,
        "Glob" | "Grep" | "GlobTool" => acp::ToolKind::Search,
        "Delete" | "Rm" => acp::ToolKind::Delete,
        "Move" | "Rename" => acp::ToolKind::Move,
        "Think" | "Sequential" => acp::ToolKind::Think,
        _ => acp::ToolKind::Other,
    }
}

/// Compose a short, human-readable title for a tool call. Falls back to the
/// tool's bare name if no descriptive field is present.
fn tool_title(tool_name: &str, raw_input: Option<&serde_json::Value>) -> String {
    if let Some(input) = raw_input {
        // Prefer path-like fields for file tools.
        for key in &["file_path", "path", "filename", "url", "pattern", "command"] {
            if let Some(v) = input.get(*key).and_then(|x| x.as_str()) {
                return format!("{}: {}", tool_name, v);
            }
        }
    }
    tool_name.to_string()
}

#[cfg(test)]
mod tests {
    use super::render_prompt_blocks;
    use agent_client_protocol_schema as acp;
    use clawde_core::types::ContentBlock as CoreContentBlock;

    #[test]
    fn image_prompt_block_becomes_provider_image() {
        let blocks = render_prompt_blocks(&[acp::ContentBlock::Image(acp::ImageContent::new(
            "aGVsbG8=",
            "image/png",
        ))]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            CoreContentBlock::Image { source } => {
                assert_eq!(source.source_type, "base64");
                assert_eq!(source.media_type.as_deref(), Some("image/png"));
                assert_eq!(source.data.as_deref(), Some("aGVsbG8="));
                assert!(source.url.is_none());
            }
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[test]
    fn uri_image_prompt_block_preserves_url_source() {
        let blocks = render_prompt_blocks(&[acp::ContentBlock::Image(
            acp::ImageContent::new("", "image/jpeg").uri("https://example.test/image.jpg"),
        )]);
        match &blocks[0] {
            CoreContentBlock::Image { source } => {
                assert_eq!(source.source_type, "url");
                assert_eq!(
                    source.url.as_deref(),
                    Some("https://example.test/image.jpg")
                );
                assert!(source.data.is_none());
            }
            other => panic!("expected URL image block, got {other:?}"),
        }
    }

    #[test]
    fn empty_text_does_not_create_a_prompt_block() {
        let blocks = render_prompt_blocks(&[acp::ContentBlock::Text(acp::TextContent::new(""))]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn failed_tool_output_preserves_stable_error_code() {
        let output = super::acp_tool_output(
            "Tool is unavailable in Ollama offline mode",
            true,
            Some("network_isolation_blocked"),
        );
        assert_eq!(
            output.get("error").and_then(|value| value.as_str()),
            Some("Tool is unavailable in Ollama offline mode")
        );
        assert_eq!(
            output.get("error_code").and_then(|value| value.as_str()),
            Some("network_isolation_blocked")
        );
    }

    #[test]
    fn successful_tool_output_keeps_existing_json_shape() {
        let output = super::acp_tool_output(r#"{"status":"ok"}"#, false, None);
        assert_eq!(output, serde_json::json!({"status": "ok"}));
    }
}
