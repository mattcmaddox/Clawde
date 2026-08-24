//! FreeProvider fallback / replay-safety integration tests.
//!
//! These exercise the real `OpenAiCompatProvider` HTTP + SSE parsing through
//! the deterministic mock server in `common::mock_provider`, so the recovery
//! kernel's behaviour is asserted against controlled wire responses rather
//! than live free-tier providers.

mod common;

use std::pin::Pin;
use std::sync::Arc;

use clawde_api::provider::LlmProvider;
use clawde_api::provider_error::{ProviderError, RecoveryClass};
use clawde_api::provider_types::{ProviderRequest, StreamEvent};
use clawde_api::providers::{
    catalog_entry, FreeEntry, FreeProvider, OpenAiCompatProvider, RoutingConfig, RoutingStrategy,
};
use clawde_core::types::Message;
use futures::{Stream, StreamExt};

use common::mock_provider::{text_stream, MockServer, RequestRecord, ScriptedResponse};

type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

// ---------------------------------------------------------------------------
// Construction helpers
// ---------------------------------------------------------------------------

/// One mock-backed chain entry. `upstream_id` is a real FREE_CATALOG id (so
/// attribution, model resolution, and routing all behave production-like);
/// the provider is pointed at `base_url` instead of the real endpoint.
fn mock_entry(upstream_id: &str, base_url: &str, model: &str) -> FreeEntry {
    let upstream = *catalog_entry(upstream_id).expect("known catalog id");
    let compat = OpenAiCompatProvider::new(upstream_id, upstream_id, base_url.to_string())
        .with_api_key(format!("fake-key-{upstream_id}-1234567890"));
    FreeEntry {
        upstream,
        provider: Arc::new(compat) as Arc<dyn LlmProvider>,
        effective_model: Some(model.to_string()),
    }
}

/// A two-upstream chain over two independent mock servers.
struct Chain {
    provider: FreeProvider,
    first: MockServer,
    second: MockServer,
}

impl Chain {
    fn new(first: Vec<ScriptedResponse>, second: Vec<ScriptedResponse>) -> Self {
        let first = MockServer::new(first);
        let second = MockServer::new(second);
        let entries = vec![
            mock_entry("groq", &first.base_url, "groq/mock-model"),
            mock_entry("poolside", &second.base_url, "poolside/mock-model"),
        ];
        let provider = FreeProvider::with_routing(
            entries,
            RoutingConfig {
                strategy: RoutingStrategy::Sequential,
                ..Default::default()
            },
            false,
        );
        Self {
            provider,
            first,
            second,
        }
    }

    fn requests(&self) -> (Vec<RequestRecord>, Vec<RequestRecord>) {
        (self.first.requests(), self.second.requests())
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        model: "free/auto".to_string(),
        messages: vec![Message::user("hi")],
        system_prompt: None,
        tools: Vec::new(),
        max_tokens: 64,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: Vec::new(),
        thinking: None,
        effort_level: None,
        provider_options: serde_json::Value::Null,
        strict_route: false,
    }
}

async fn collect(mut stream: EventStream) -> (Vec<StreamEvent>, Option<ProviderError>) {
    let mut events = Vec::new();
    let mut error = None;
    while let Some(item) = stream.next().await {
        match item {
            Ok(event) => events.push(event),
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }
    (events, error)
}

fn attribution(events: &[StreamEvent]) -> Option<&str> {
    events.iter().find_map(|event| match event {
        StreamEvent::ProviderAttribution { upstream_id, .. } => Some(upstream_id.as_str()),
        _ => None,
    })
}

fn text(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Assert that a request reached the mock with the expected method, path, and model.
fn assert_request(record: &RequestRecord, model: &str) {
    assert_eq!(record.method, "POST");
    assert!(
        record.path.ends_with("/chat/completions"),
        "unexpected path: {}",
        record.path
    );
    let parsed: serde_json::Value =
        serde_json::from_str(&record.body).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        parsed.get("model").and_then(|v| v.as_str()),
        Some(model),
        "request body was: {}",
        record.body
    );
}

fn json_response(status: u16, reason: &'static str, body: &str) -> ScriptedResponse {
    ScriptedResponse::Json {
        status,
        reason,
        body: body.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Pre-first-byte failures fall through to the next upstream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn server_error_before_first_byte_falls_through() {
    let chain = Chain::new(
        vec![json_response(
            500,
            "Internal Server Error",
            r#"{"error":{"message":"mock 500"}}"#,
        )],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "hello from poolside"),
        }],
    );

    let stream = chain
        .provider
        .create_message_stream(request())
        .await
        .expect("chain succeeds");
    let (events, error) = collect(stream).await;

    assert!(error.is_none(), "unexpected error: {error:?}");
    assert_eq!(attribution(&events), Some("poolside"));
    assert_eq!(text(&events), "hello from poolside");

    let (first, second) = chain.requests();
    assert_eq!(first.len(), 1, "first upstream must be attempted once");
    assert_eq!(second.len(), 1, "second upstream must serve the request");
    assert_request(&first[0], "groq/mock-model");
    assert_request(&second[0], "poolside/mock-model");
}

#[tokio::test]
async fn rate_limit_before_first_byte_falls_through() {
    let chain = Chain::new(
        vec![json_response(
            429,
            "Too Many Requests",
            r#"{"error":{"message":"mock 429"}}"#,
        )],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "recovered after rate limit"),
        }],
    );

    let stream = chain
        .provider
        .create_message_stream(request())
        .await
        .expect("chain succeeds");
    let (events, error) = collect(stream).await;

    assert!(error.is_none());
    assert_eq!(attribution(&events), Some("poolside"));
    assert_eq!(text(&events), "recovered after rate limit");
    assert_eq!(chain.requests().1.len(), 1);
}

#[tokio::test]
async fn auth_failure_before_first_byte_falls_through() {
    // A dead key on one upstream must not block later upstreams, each of which
    // carries its own credential.
    let chain = Chain::new(
        vec![json_response(
            401,
            "Unauthorized",
            r#"{"error":{"message":"mock bad key"}}"#,
        )],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "valid key answered"),
        }],
    );

    let stream = chain
        .provider
        .create_message_stream(request())
        .await
        .expect("chain succeeds");
    let (events, error) = collect(stream).await;

    assert!(error.is_none());
    assert_eq!(attribution(&events), Some("poolside"));
    assert_eq!(text(&events), "valid key answered");
}

#[tokio::test]
async fn context_overflow_before_first_byte_falls_through() {
    let chain = Chain::new(
        vec![json_response(
            413,
            "Payload Too Large",
            r#"{"error":{"message":"mock overflow"}}"#,
        )],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "larger context answered"),
        }],
    );

    let stream = chain
        .provider
        .create_message_stream(request())
        .await
        .expect("chain succeeds");
    let (events, error) = collect(stream).await;

    assert!(error.is_none());
    assert_eq!(attribution(&events), Some("poolside"));
    assert_eq!(text(&events), "larger context answered");
}

// ---------------------------------------------------------------------------
// Non-fallbackable request errors are surfaced, not retried
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_request_is_surfaced_without_fallback() {
    // `invalid_request_error` maps to RecoveryClass::MalformedRequest, which
    // must never be retried on another upstream: it would fail identically
    // everywhere. The second (healthy) upstream must not be contacted.
    let chain = Chain::new(
        vec![json_response(
            400,
            "Bad Request",
            r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#,
        )],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "should never be used"),
        }],
    );

    // The non-fallbackable error is surfaced directly by the dispatch call
    // (before any stream exists), never turned into a retry on another
    // upstream.
    let error = match chain.provider.create_message_stream(request()).await {
        Err(error) => error,
        Ok(_) => panic!("malformed request must surface as an error"),
    };
    assert_eq!(error.recovery_class(), RecoveryClass::MalformedRequest);

    let (first, second) = chain.requests();
    assert_eq!(first.len(), 1, "only the first upstream is attempted");
    assert_eq!(second.len(), 0, "healthy upstream must not be contacted");
}

// ---------------------------------------------------------------------------
// Mid-stream truncation: no replay after visible output
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mid_stream_truncation_surfaces_error_without_replay() {
    // The first upstream emits partial visible text, then the connection dies
    // before the declared Content-Length is satisfied. Replaying the request
    // on the second upstream would duplicate visible output, so the error must
    // surface instead and the healthy upstream must never be contacted.
    let chain = Chain::new(
        vec![ScriptedResponse::SseTruncated {
            frames: vec![
                common::mock_provider::sse_first_delta("groq/mock-model", "partial "),
                common::mock_provider::sse_text_delta("visible output"),
            ],
        }],
        vec![ScriptedResponse::SseStream {
            frames: text_stream("poolside/mock-model", "must not replay"),
        }],
    );

    let stream = chain
        .provider
        .create_message_stream(request())
        .await
        .expect("stream opens against the first upstream");
    let (events, error) = collect(stream).await;

    // The partial text was exposed before the failure...
    assert_eq!(text(&events), "partial visible output");
    // ...and the failure is surfaced, not swallowed into a silent retry.
    let error = error.expect("mid-stream failure must surface as an error");
    assert_eq!(
        error.recovery_class(),
        RecoveryClass::VisibleStreamFailure,
        "a read error after visible output must be replay-unsafe: {error:?}"
    );
    // The adapter must carry the already-exposed content so any caller (not
    // just RetryingFreeStream) can refuse to replay it.
    match &error {
        ProviderError::StreamError {
            partial_response, ..
        } => assert_eq!(
            partial_response.as_deref(),
            Some("partial visible output"),
            "partial output must be attached to the stream error"
        ),
        other => panic!("expected StreamError, got: {other:?}"),
    }

    let (first, second) = chain.requests();
    assert_eq!(first.len(), 1, "first upstream is attempted exactly once");
    assert_eq!(second.len(), 0, "no replay after visible output");
}

// ---------------------------------------------------------------------------
// Non-streaming path uses the same fallthrough policy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn non_streaming_server_error_falls_through() {
    let chain = Chain::new(
        vec![json_response(
            500,
            "Internal Server Error",
            r#"{"error":{"message":"mock 500"}}"#,
        )],
        vec![json_response(
            200,
            "OK",
            r#"{"id":"cmpl-hf","model":"poolside/mock-model","choices":[{"index":0,"message":{"role":"assistant","content":"non-streaming answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
        )],
    );

    let response = chain
        .provider
        .create_message(request())
        .await
        .expect("chain succeeds");

    assert_eq!(
        response
            .content
            .first()
            .map(|block| match block {
                clawde_core::types::ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .unwrap_or(""),
        "non-streaming answer"
    );

    let (first, second) = chain.requests();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
}
