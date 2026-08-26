use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use clawde_api::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, StopReason,
    StreamEvent, SystemPromptStyle,
};
use clawde_api::providers::{
    catalog_entry, FreeEntry, FreeProvider, RoutingConfig, RoutingStrategy,
};
use clawde_api::{LlmProvider, ProviderError, ProviderRegistry};
use clawde_core::provider_id::ProviderId;
use clawde_core::types::{ContentBlock, UsageInfo};
use clawde_gateway::auth::RateLimiter;
use clawde_gateway::config::EffectiveGatewayConfig;
use clawde_gateway::router::{build_router, GatewayState};
use futures::Stream;
use tower::ServiceExt;

struct ScriptedProvider {
    id: ProviderId,
    calls: AtomicUsize,
    result: ScriptedResult,
}

enum ScriptedResult {
    Failure,
    Success,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        "gateway-test-provider"
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.result {
            ScriptedResult::Failure => Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: Some(503),
                message: "scripted upstream failure".to_string(),
                is_retryable: true,
            }),
            ScriptedResult::Success => Ok(ProviderResponse {
                id: "gateway-test-response".to_string(),
                content: vec![ContentBlock::Text {
                    text: "fallback worked".to_string(),
                }],
                stop_reason: StopReason::EndTurn,
                usage: UsageInfo {
                    input_tokens: 3,
                    output_tokens: 2,
                    ..UsageInfo::default()
                },
                model: request.model,
                rate_limit: None,
            }),
        }
    }

    async fn create_message_stream(
        &self,
        _request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        Err(ProviderError::ServerError {
            provider: self.id.clone(),
            status: Some(503),
            message: "streaming is not part of this test".to_string(),
            is_retryable: true,
        })
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        Ok(ProviderStatus::Healthy)
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: false,
            tool_calling: false,
            thinking: false,
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

fn free_entry(id: &'static str, provider: Arc<ScriptedProvider>) -> FreeEntry {
    FreeEntry {
        upstream: *catalog_entry(id).expect("test upstream must be in the free catalog"),
        provider,
        effective_model: Some(format!("{id}/test-model")),
    }
}

fn gateway_state(registry: ProviderRegistry) -> GatewayState {
    GatewayState {
        limiter: Arc::new(RateLimiter::new(100, 100_000)),
        registry: Arc::new(registry),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        active_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        in_flight: Arc::new(tokio::sync::Semaphore::new(8)),
        force_cancel: tokio_util::sync::CancellationToken::new(),
        config: EffectiveGatewayConfig {
            allowed_keys: vec!["gateway-test-key".to_string()],
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn chat_completion_falls_through_free_upstream_and_returns_openai_response() {
    let first = Arc::new(ScriptedProvider {
        id: ProviderId::new("groq"),
        calls: AtomicUsize::new(0),
        result: ScriptedResult::Failure,
    });
    let second = Arc::new(ScriptedProvider {
        id: ProviderId::new("poolside"),
        calls: AtomicUsize::new(0),
        result: ScriptedResult::Success,
    });

    let free = FreeProvider::with_routing(
        vec![
            free_entry("groq", first.clone()),
            free_entry("poolside", second.clone()),
        ],
        RoutingConfig {
            strategy: RoutingStrategy::Sequential,
            ..RoutingConfig::default()
        },
        false,
    );
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(free));

    let response = build_router(gateway_state(registry))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer gateway-test-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"free/auto","messages":[{"role":"user","content":"hello"}],"max_tokens":32}"#,
                ))
                .expect("request builds"),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("response body reads");
    let json: serde_json::Value = serde_json::from_slice(&body).expect("response is JSON");

    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "fallback worked");
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn chat_completion_rejects_invalid_gateway_key_before_dispatch() {
    let provider = Arc::new(ScriptedProvider {
        id: ProviderId::new("groq"),
        calls: AtomicUsize::new(0),
        result: ScriptedResult::Success,
    });
    let mut registry = ProviderRegistry::new();
    registry.register(Arc::new(FreeProvider::with_routing(
        vec![free_entry("groq", provider.clone())],
        RoutingConfig::default(),
        false,
    )));

    let response = build_router(gateway_state(registry))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("authorization", "Bearer wrong-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"free/auto","messages":[{"role":"user","content":"hello"}]}"#,
                ))
                .expect("request builds"),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}
