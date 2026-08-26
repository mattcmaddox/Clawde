use clawde_api::provider_types::{ProviderResponse, StopReason, StreamEvent};
use clawde_core::types::{ContentBlock, UsageInfo};
use clawde_gateway::translate::{
    parse_chat_completion_request, to_openai_response, StreamTranslator,
};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let contents = std::fs::read_to_string(path).expect("fixture must exist");
    serde_json::from_str(&contents).expect("fixture must be valid JSON")
}

fn without_created(mut value: Value) -> Value {
    match &mut value {
        Value::Array(items) => {
            for item in items {
                *item = without_created(item.take());
            }
        }
        Value::Object(object) => {
            object.remove("created");
            for item in object.values_mut() {
                let current = item.take();
                *item = without_created(current);
            }
        }
        _ => {}
    }
    value
}

fn usage(input: u64, output: u64) -> UsageInfo {
    UsageInfo {
        input_tokens: input,
        output_tokens: output,
        ..UsageInfo::default()
    }
}

fn translate_events(events: impl IntoIterator<Item = StreamEvent>, include_usage: bool) -> Value {
    let mut translator = StreamTranslator::new(include_usage);
    let chunks: Vec<Value> = events
        .into_iter()
        .flat_map(|event| translator.push(&event))
        .collect();
    without_created(Value::Array(chunks))
}

#[test]
fn text_stream_matches_golden_transcript() {
    let actual = translate_events(
        [
            StreamEvent::MessageStart {
                id: "msg_text".into(),
                model: "free/auto".into(),
                usage: usage(10, 0),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Hello".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: " world".into(),
            },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(usage(10, 3)),
            },
            StreamEvent::MessageStop,
        ],
        false,
    );
    assert_eq!(actual, fixture("text_stream.json"));
}

#[test]
fn tool_call_stream_matches_golden_transcript() {
    let actual = translate_events(
        [
            StreamEvent::MessageStart {
                id: "msg_tool".into(),
                model: "free/auto".into(),
                usage: usage(10, 0),
            },
            StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: "call_weather".into(),
                    name: "get_weather".into(),
                    input: json!({}),
                    thought_signature: None,
                },
            },
            StreamEvent::InputJsonDelta {
                index: 0,
                partial_json: "{\"city\":".into(),
            },
            StreamEvent::InputJsonDelta {
                index: 0,
                partial_json: "\"SF\"}".into(),
            },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Some(usage(10, 8)),
            },
            StreamEvent::MessageStop,
        ],
        false,
    );
    assert_eq!(actual, fixture("tool_call_stream.json"));
}

#[test]
fn reasoning_stream_matches_golden_transcript() {
    let actual = translate_events(
        [
            StreamEvent::MessageStart {
                id: "msg_reason".into(),
                model: "free/auto".into(),
                usage: usage(10, 0),
            },
            StreamEvent::ThinkingDelta {
                index: 0,
                thinking: "Need to inspect the request.".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Done.".into(),
            },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(usage(10, 7)),
            },
            StreamEvent::MessageStop,
        ],
        false,
    );
    assert_eq!(actual, fixture("reasoning_stream.json"));
}

#[test]
fn usage_stream_matches_golden_transcript() {
    let actual = translate_events(
        [
            StreamEvent::MessageStart {
                id: "msg_usage".into(),
                model: "free/auto".into(),
                usage: usage(10, 0),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "Done.".into(),
            },
            StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(usage(10, 7)),
            },
            StreamEvent::MessageStop,
        ],
        true,
    );
    assert_eq!(actual, fixture("usage_stream.json"));
}

#[test]
fn non_stream_response_matches_golden_transcript() {
    let document = fixture("text_response.json");
    let parsed = parse_chat_completion_request(&document["request"]).unwrap();
    assert!(matches!(
        parsed.provider_request.system_prompt,
        Some(clawde_api::provider_types::SystemPrompt::Text(ref text))
            if text == "Answer concisely."
    ));
    assert_eq!(parsed.provider_request.messages.len(), 1);

    let upstream: ProviderResponse =
        serde_json::from_value(document["upstream_response"].clone()).unwrap();
    let actual = without_created(to_openai_response(&upstream));
    assert_eq!(actual, document["expected"]);
}

#[test]
fn stream_error_transcript_terminates_at_translator_boundary() {
    let mut translator = StreamTranslator::new(false);
    let chunks = translator.push(&StreamEvent::Error {
        error_type: "server_error".into(),
        message: "upstream failed".into(),
    });
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["error"]["message"], "upstream failed");
}
