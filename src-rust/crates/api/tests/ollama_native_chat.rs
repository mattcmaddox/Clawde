//! Ollama native `/api/chat` transport integration tests.
//!
//! Exercises the real `OllamaNativeProvider` HTTP + NDJSON parsing against
//! the deterministic mock server (loopback is fine here — the remote-only
//! rule lives in the host *resolver*, not in the transport struct, matching
//! how the compat provider's own tests reach the same mock).

mod common;

use clawde_api::provider::LlmProvider;
use clawde_api::provider_types::{ProviderRequest, StreamEvent};
use clawde_api::providers::{OllamaNativeProvider, OpenAiCompatProvider};
use common::mock_provider::{MockServer, RequestRecord, ScriptedResponse};
use futures::StreamExt;

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn sample_request(provider_options: serde_json::Value) -> ProviderRequest {
    ProviderRequest {
        model: "qwen2.5-coder:7b".to_string(),
        messages: vec![clawde_core::types::Message::user("list files")],
        system_prompt: Some(clawde_api::provider_types::SystemPrompt::Text(
            "You are a coding agent.".to_string(),
        )),
        tools: vec![clawde_core::types::ToolDefinition {
            name: "list_files".to_string(),
            description: "List files in a directory".to_string(),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }],
        max_tokens: 2_048,
        temperature: Some(0.3),
        top_p: None,
        top_k: None,
        stop_sequences: vec![],
        thinking: None,
        effort_level: None,
        provider_options,
        strict_route: false,
    }
}

fn provider_for(base_url: &str) -> OllamaNativeProvider {
    let inner = OpenAiCompatProvider::new("ollama", "Ollama", format!("{base_url}/v1"));
    OllamaNativeProvider::new(inner, base_url.to_string())
}

/// A complete non-streaming `/api/chat` response body with a tool call.
fn chat_response_json() -> String {
    serde_json::json!({
        "model": "qwen2.5-coder:7b",
        "created_at": "2026-09-03T00:00:00Z",
        "message": {
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "function": {
                    "name": "list_files",
                    "arguments": {"path": "."}
                }
            }]
        },
        "done": true,
        "done_reason": "stop",
        "prompt_eval_count": 120,
        "eval_count": 34
    })
    .to_string()
}

/// NDJSON stream frames: one thinking+text delta, one tool call, one done.
fn chat_stream_frames() -> Vec<String> {
    vec![
        serde_json::json!({
            "model": "qwen2.5-coder:7b",
            "created_at": "2026-09-03T00:00:00Z",
            "message": {
                "role": "assistant",
                "content": "Listing",
                "thinking": "need the dir"
            }
        })
        .to_string(),
        serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "list_files",
                        "arguments": "{\"path\":\".\"}"
                    }
                }]
            }
        })
        .to_string(),
        serde_json::json!({
            "done": true,
            "done_reason": "stop",
            "prompt_eval_count": 120,
            "eval_count": 34
        })
        .to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Request shaping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn request_body_carries_native_options_and_system() {
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: chat_response_json(),
    }]);
    let options = serde_json::json!({
        "num_ctx": 32_768,
        "keep_alive": 600,
        "temperature": 0.2,
    });
    let _ = provider_for(&server.base_url)
        .create_message(sample_request(options))
        .await
        .expect("dispatch");

    let reqs = server.requests();
    assert_eq!(reqs.len(), 1);
    let record: &RequestRecord = &reqs[0];
    assert_eq!(record.path, "/api/chat", "chat must use the native route");
    let body: serde_json::Value = serde_json::from_str(&record.body).unwrap();
    assert_eq!(body["model"], "qwen2.5-coder:7b");
    assert_eq!(
        body["options"]["num_ctx"], 32_768,
        "num_ctx honored natively"
    );
    assert_eq!(body["options"]["temperature"], 0.2);
    // Request temperature (0.3) must not overwrite the explicit persisted one.
    assert_eq!(body["keep_alive"], 600, "keep_alive is a top-level field");
    assert!(!body["stream"].as_bool().unwrap_or(true));
    // System prompt rides as the first system-role message (OpenAI shape).
    assert_eq!(body["messages"][0]["role"], "system");
    // Tools use the OpenAI function envelope.
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "list_files");
}

#[tokio::test]
async fn tool_results_are_keyed_by_tool_name() {
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: chat_response_json(),
    }]);
    let mut request = sample_request(serde_json::Value::Null);
    request.messages.push(clawde_core::types::Message {
        role: clawde_core::types::Role::User,
        content: clawde_core::types::MessageContent::Blocks(vec![
            clawde_core::types::ContentBlock::ToolResult {
                tool_use_id: "call_abc".to_string(),
                content: clawde_core::types::ToolResultContent::Text("a.txt\nb.txt".to_string()),
                is_error: None,
            },
        ]),
        uuid: None,
        cost: None,
        snapshot_patch: None,
        turn_meta: None,
    });

    let _ = provider_for(&server.base_url)
        .create_message(request)
        .await
        .expect("dispatch");

    let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    let tool_msgs: Vec<&serde_json::Value> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect();
    assert_eq!(tool_msgs.len(), 1);
    assert_eq!(
        tool_msgs[0]["tool_name"], "call_abc",
        "tool_call_id is rewritten to the native tool_name key"
    );
    assert!(tool_msgs[0].get("tool_call_id").is_none());
}

// ---------------------------------------------------------------------------
// Response parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_streaming_parses_tool_call_and_usage() {
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: chat_response_json(),
    }]);
    let response = provider_for(&server.base_url)
        .create_message(sample_request(serde_json::Value::Null))
        .await
        .expect("dispatch");

    assert_eq!(
        response.stop_reason,
        clawde_api::provider_types::StopReason::EndTurn
    );
    assert_eq!(response.usage.input_tokens, 120);
    assert_eq!(response.usage.output_tokens, 34);
    let tool = response
        .content
        .iter()
        .find_map(|b| match b {
            clawde_core::types::ContentBlock::ToolUse { name, input, .. } => {
                Some((name.as_str(), input.clone()))
            }
            _ => None,
        })
        .expect("tool call parsed");
    assert_eq!(tool.0, "list_files");
    assert_eq!(tool.1["path"], ".", "object arguments pass through");
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_decodes_thinking_text_and_tool_call() {
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: chat_stream_frames().join("\n") + "\n",
    }]);
    let mut stream = provider_for(&server.base_url)
        .create_message_stream(sample_request(serde_json::Value::Null))
        .await
        .expect("stream opened");

    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_json = String::new();
    let mut saw_stop = false;
    while let Some(event) = stream.next().await {
        match event.expect("event") {
            StreamEvent::TextDelta { text: delta, .. } => text.push_str(&delta),
            StreamEvent::ThinkingDelta {
                thinking: delta, ..
            } => thinking.push_str(&delta),
            StreamEvent::InputJsonDelta { partial_json, .. } => tool_json.push_str(&partial_json),
            StreamEvent::MessageStart { .. } => {}
            StreamEvent::ContentBlockStart { .. } | StreamEvent::ContentBlockStop { .. } => {}
            StreamEvent::MessageDelta { stop_reason, .. } => {
                saw_stop = stop_reason.is_some();
            }
            StreamEvent::MessageStop => {}
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert_eq!(text, "Listing");
    assert_eq!(thinking, "need the dir");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&tool_json).unwrap()["path"],
        ".",
        "string arguments decode to JSON"
    );
    assert!(saw_stop, "final MessageDelta carries the stop reason");
}

#[tokio::test]
async fn assistant_tool_call_arguments_are_object_encoded() {
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: chat_response_json(),
    }]);
    let mut request = sample_request(serde_json::Value::Null);
    // An assistant turn that made a tool call (OpenAI string-encodes the
    // arguments), followed by the tool result. Ollama requires the history
    // arguments to be a JSON object.
    request.messages.insert(
        1,
        clawde_core::types::Message {
            role: clawde_core::types::Role::Assistant,
            content: clawde_core::types::MessageContent::Blocks(vec![
                clawde_core::types::ContentBlock::Text {
                    text: String::new(),
                },
                clawde_core::types::ContentBlock::ToolUse {
                    id: "call_abc".to_string(),
                    name: "list_files".to_string(),
                    input: serde_json::json!({"path": "."}),
                    thought_signature: None,
                },
            ]),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        },
    );

    let _ = provider_for(&server.base_url)
        .create_message(request)
        .await
        .expect("dispatch");

    let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant turn present");
    let args = &assistant["tool_calls"][0]["function"]["arguments"];
    assert!(
        args.is_object(),
        "arguments must be a JSON object on /api/chat, got: {args}"
    );
    assert_eq!(args["path"], ".");
}

#[tokio::test]
async fn mid_stream_error_carries_partial_output() {
    let frames = [
        serde_json::json!({"message": {"role": "assistant", "content": "partial"}}).to_string(),
        serde_json::json!({"error": "model exploded"}).to_string(),
    ];
    let server = MockServer::new(vec![ScriptedResponse::Json {
        status: 200,
        reason: "OK",
        body: frames.join("\n") + "\n",
    }]);
    let mut stream = provider_for(&server.base_url)
        .create_message_stream(sample_request(serde_json::Value::Null))
        .await
        .expect("stream opened");

    let mut last_err = None;
    while let Some(event) = stream.next().await {
        if let Err(e) = event {
            last_err = Some(e);
        }
    }
    let err = last_err.expect("stream must end in the injected error");
    match err {
        clawde_api::provider_error::ProviderError::StreamError {
            partial_response, ..
        } => {
            assert_eq!(
                partial_response.as_deref(),
                Some("partial"),
                "replay-unsafe classification keeps the committed text"
            );
        }
        other => panic!("expected StreamError, got {other:?}"),
    }
}
