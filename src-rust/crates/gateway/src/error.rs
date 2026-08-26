//! Gateway error envelope — maps `ProviderError` and gateway-local errors to
//! OpenAI-style `{ "error": { "message", "type", "param", "code" } }` responses.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

/// A gateway error that maps to an OpenAI-style error envelope.
#[derive(Debug, Clone)]
pub struct GatewayError {
    pub status: StatusCode,
    pub error_type: String,
    pub message: String,
    /// Optional `param` field (e.g. the offending request field).
    pub param: Option<String>,
    /// Optional `code` field (e.g. `rate_limit_exceeded`).
    pub code: Option<String>,
    /// Optional `Retry-After` seconds for 429 responses.
    pub retry_after_secs: Option<u64>,
}

impl GatewayError {
    pub fn new(status: StatusCode, error_type: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            error_type: error_type.to_string(),
            message: message.into(),
            param: None,
            code: None,
            retry_after_secs: None,
        }
    }

    /// `400 invalid_request_error` — structurally invalid body or unsupported
    /// feature (e.g. `n > 1`).
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request_error", message)
    }

    /// `401 authentication_error` — missing/invalid bearer key.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "authentication_error", message)
    }

    /// `429 rate_limit_error` — RPM/TPM budget exhausted.
    pub fn rate_limited(message: impl Into<String>, retry_after_secs: u64) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            error_type: "rate_limit_error".to_string(),
            message: message.into(),
            param: None,
            code: Some("rate_limit_exceeded".to_string()),
            retry_after_secs: Some(retry_after_secs),
        }
    }

    /// `404 model_not_found` — unknown model id.
    pub fn model_not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "model_not_found", message)
    }

    /// `503 service_unavailable` — chain exhaustion before first byte.
    pub fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            message,
        )
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        let mut resp = Json(json!({
            "error": {
                "message": self.message,
                "type": self.error_type,
                "param": self.param,
                "code": self.code,
            }
        }))
        .into_response();
        *resp.status_mut() = self.status;
        if let Some(retry_after) = self.retry_after_secs {
            if let Ok(v) = axum::http::HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

/// Map a `ProviderError` from the upstream to a `GatewayError`.
///
/// Never leaks upstream API keys or raw URLs — only status codes + route.
/// Chain exhaustion (all upstreams failed before first byte) is handled by
/// the router, which lists attempted upstream ids (never keys).
pub fn map_provider_error(err: &clawde_api::ProviderError) -> GatewayError {
    use clawde_api::ProviderError::*;
    match err {
        AuthFailed { .. } => {
            GatewayError::unauthorized("Authentication failed with the upstream provider")
        }
        RateLimited { retry_after, .. } => {
            let retry_after = retry_after.unwrap_or(60);
            GatewayError::rate_limited("Upstream rate limit exceeded", retry_after)
        }
        QuotaExceeded { .. } => GatewayError::rate_limited("Upstream quota exhausted", 3600),
        ServerError { status, .. } => {
            let status = status.unwrap_or(502);
            GatewayError {
                status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                error_type: "api_error".to_string(),
                message: format!("Upstream server error (HTTP {status})"),
                param: None,
                code: None,
                retry_after_secs: None,
            }
        }
        InvalidRequest { .. } => {
            GatewayError::invalid_request("Invalid request rejected by upstream")
        }
        ContentFiltered { .. } => {
            GatewayError::invalid_request("Response blocked by upstream content filter")
        }
        ContextOverflow { .. } => {
            GatewayError::invalid_request("Request exceeds model context window")
        }
        ModelNotFound { model, .. } => {
            GatewayError::model_not_found(format!("Model '{model}' not found on upstream"))
        }
        StreamError { .. } => GatewayError {
            status: StatusCode::BAD_GATEWAY,
            error_type: "api_error".to_string(),
            message: "Upstream stream error".to_string(),
            param: None,
            code: None,
            retry_after_secs: None,
        },
        Other { status, .. } => {
            let status = status.unwrap_or(502);
            GatewayError {
                status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                error_type: "api_error".to_string(),
                message: format!("Upstream error (HTTP {status})"),
                param: None,
                code: None,
                retry_after_secs: None,
            }
        }
    }
}

/// Convenience: a `Value`-shaped OpenAI error body (used in tests).
pub fn error_body(e: &GatewayError) -> Value {
    json!({
        "error": {
            "message": e.message,
            "type": e.error_type,
            "param": e.param,
            "code": e.code,
        }
    })
}
