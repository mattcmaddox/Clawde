// provider_error.rs — Unified error type for all provider adapters.
//
// Every provider implementation maps its own error representation onto
// `ProviderError` so that the application-layer code can handle errors
// generically without knowing which provider was involved.

use clawde_core::error::ClaudeError;
use clawde_core::provider_id::ProviderId;
use std::fmt;

// ---------------------------------------------------------------------------
// ProviderError
// ---------------------------------------------------------------------------

/// The operational class that determines how an error may be recovered.
///
/// This is intentionally separate from [`ProviderError`]'s wire/provider
/// details. Routing, key rotation, and the agent loop can make policy decisions
/// from one stable taxonomy without matching provider-specific strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryClass {
    InvalidCredential,
    RateLimited,
    QuotaExhausted,
    TransientProvider,
    ContextOverflow,
    UnsupportedCapability,
    MalformedRequest,
    ContentFiltered,
    ModelUnavailable,
    VisibleStreamFailure,
    Unknown,
}

impl RecoveryClass {
    /// Stable machine-readable name for logs, diagnostics, and tests.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidCredential => "invalid_credential",
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "quota_exhausted",
            Self::TransientProvider => "transient_provider",
            Self::ContextOverflow => "context_overflow",
            Self::UnsupportedCapability => "unsupported_capability",
            Self::MalformedRequest => "malformed_request",
            Self::ContentFiltered => "content_filtered",
            Self::ModelUnavailable => "model_unavailable",
            Self::VisibleStreamFailure => "visible_stream_failure",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the same logical request may be sent to another upstream.
    ///
    /// A **malformed request** is fallbackable on purpose. This classification
    /// cannot tell a genuinely bad request (a broken tool schema, an invalid
    /// message shape) from a *provider-specific* rejection — an upstream that
    /// refuses a parameter, model, or capability another upstream accepts. The
    /// Groq `thinking` 400 is the canonical case: the request was valid for
    /// every other upstream in the chain, but `MalformedRequest` marked it
    /// terminal and the whole turn failed instead of moving on.
    ///
    /// Trying the next upstream is cheap and bounded: the free chain caps the
    /// walk with `turn_walk_budget_secs`, and the query layer switches fallback
    /// models at most once. It does not blame the credential (`cools_key` is
    /// false) or retry the same provider (`may_retry_same_provider` is false).
    /// A request that really is bad fails on every upstream and the last error
    /// surfaces — the same message the caller would have shown anyway.
    ///
    /// Content filters and already-visible stream failures stay terminal:
    /// re-sending filtered content to another provider is evasion, and a
    /// partially-rendered turn cannot be replayed without duplicating output.
    pub const fn may_fallback(self) -> bool {
        !matches!(self, Self::ContentFiltered | Self::VisibleStreamFailure)
    }

    /// Whether retrying the same provider can be useful. Key rotation may
    /// still choose a different key for credential/quota classes.
    pub const fn may_retry_same_provider(self) -> bool {
        matches!(self, Self::RateLimited | Self::TransientProvider)
    }

    /// Whether the failure should cool down a credential slot. Provider-wide
    /// outages and request-shape failures must not evict individual keys.
    pub const fn cools_key(self) -> bool {
        matches!(
            self,
            Self::InvalidCredential | Self::RateLimited | Self::QuotaExhausted
        )
    }

    /// Whether it is safe to replay the request when no output was committed.
    pub const fn replay_safe(self) -> bool {
        !matches!(self, Self::VisibleStreamFailure)
    }
}

/// A structured error produced by any provider adapter.
#[derive(Debug, Clone)]
pub enum ProviderError {
    /// The request exceeded the model's context window.
    ContextOverflow {
        provider: ProviderId,
        message: String,
        /// The provider's advertised context limit in tokens, if known.
        max_tokens: Option<u64>,
    },

    /// The provider returned HTTP 429 or an equivalent rate-limit signal.
    RateLimited {
        provider: ProviderId,
        /// How long to wait before retrying, in seconds (if provided).
        retry_after: Option<u64>,
    },

    /// The API key or credentials were rejected by the provider.
    AuthFailed {
        provider: ProviderId,
        message: String,
    },

    /// The account's usage quota has been exhausted.
    QuotaExceeded {
        provider: ProviderId,
        message: String,
    },

    /// The requested model does not exist or is not accessible.
    ModelNotFound {
        provider: ProviderId,
        model: String,
        /// Alternative model IDs the caller might try instead.
        suggestions: Vec<String>,
    },

    /// The provider returned a 5xx or equivalent server-side error.
    ServerError {
        provider: ProviderId,
        /// HTTP status code, if applicable.
        status: Option<u16>,
        message: String,
        /// Whether the caller should retry this request.
        is_retryable: bool,
    },

    /// The request itself was malformed or contained invalid parameters.
    InvalidRequest {
        provider: ProviderId,
        message: String,
    },

    /// The response was blocked by the provider's content-safety system.
    ContentFiltered {
        provider: ProviderId,
        message: String,
    },

    /// An error occurred during streaming after the response had already begun.
    StreamError {
        provider: ProviderId,
        message: String,
        /// Any content blocks that had been received before the error, if any.
        partial_response: Option<String>,
    },

    /// A catch-all variant for errors that do not fit any of the above.
    Other {
        provider: ProviderId,
        message: String,
        /// HTTP status code, if applicable.
        status: Option<u16>,
        /// Raw response body, if available.
        body: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// impl ProviderError
// ---------------------------------------------------------------------------

impl ProviderError {
    /// Server-provided retry hint in seconds, when the error carries one.
    ///
    /// Currently only [`ProviderError::RateLimited`] transports it (from the
    /// `Retry-After` header or the error body). Callers pace a same-upstream
    /// retry with it instead of guessing a backoff schedule.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
    /// Classify this error for routing, key rotation, and agent recovery.
    pub fn recovery_class(&self) -> RecoveryClass {
        match self {
            Self::ContextOverflow { .. } => RecoveryClass::ContextOverflow,
            Self::RateLimited { .. } => RecoveryClass::RateLimited,
            Self::AuthFailed { .. } => RecoveryClass::InvalidCredential,
            Self::QuotaExceeded { .. } => RecoveryClass::QuotaExhausted,
            Self::ModelNotFound { .. } => RecoveryClass::ModelUnavailable,
            Self::ServerError { .. } => RecoveryClass::TransientProvider,
            Self::InvalidRequest { message, .. } => {
                if crate::error_handling::is_context_overflow(message) {
                    RecoveryClass::ContextOverflow
                } else {
                    RecoveryClass::MalformedRequest
                }
            }
            Self::ContentFiltered { .. } => RecoveryClass::ContentFiltered,
            Self::StreamError {
                partial_response: Some(_),
                ..
            } => RecoveryClass::VisibleStreamFailure,
            Self::StreamError {
                partial_response: None,
                ..
            } => RecoveryClass::TransientProvider,
            Self::Other {
                status,
                message,
                body,
                ..
            } => {
                let body_text = body.as_deref().unwrap_or("");
                if crate::error_handling::is_context_overflow(message)
                    || crate::error_handling::is_context_overflow(body_text)
                {
                    RecoveryClass::ContextOverflow
                } else {
                    match status {
                        Some(401 | 403) => RecoveryClass::InvalidCredential,
                        Some(402) => RecoveryClass::QuotaExhausted,
                        Some(413) => RecoveryClass::ContextOverflow,
                        Some(429) => RecoveryClass::RateLimited,
                        Some(408 | 425 | 500..=599) => RecoveryClass::TransientProvider,
                        // Any other client error is the upstream rejecting the
                        // request, not a transport or credential problem. Fold
                        // it into the same class a typed `InvalidRequest` gets
                        // (which `parse_error_response` produces for bodies that
                        // carry `invalid_request_error`), so the fallback policy
                        // no longer depends on whether the body happened to
                        // name an error code: an untyped 400 and a typed one now
                        // behave identically.
                        Some(400..=499) => RecoveryClass::MalformedRequest,
                        _ => RecoveryClass::Unknown,
                    }
                }
            }
        }
    }

    /// Returns `true` if the caller should retry the request after a delay.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } => true,
            ProviderError::ServerError { is_retryable, .. } => *is_retryable,
            ProviderError::StreamError { .. } => true,
            _ => false,
        }
    }

    /// Whether another upstream may receive this logical request.
    pub fn may_fallback(&self) -> bool {
        self.recovery_class().may_fallback()
    }

    /// Whether replaying this request is safe after this error.
    pub fn replay_safe(&self) -> bool {
        self.recovery_class().replay_safe()
    }

    /// Returns the `ProviderId` of the provider that produced this error.
    pub fn provider_id(&self) -> &ProviderId {
        match self {
            ProviderError::ContextOverflow { provider, .. } => provider,
            ProviderError::RateLimited { provider, .. } => provider,
            ProviderError::AuthFailed { provider, .. } => provider,
            ProviderError::QuotaExceeded { provider, .. } => provider,
            ProviderError::ModelNotFound { provider, .. } => provider,
            ProviderError::ServerError { provider, .. } => provider,
            ProviderError::InvalidRequest { provider, .. } => provider,
            ProviderError::ContentFiltered { provider, .. } => provider,
            ProviderError::StreamError { provider, .. } => provider,
            ProviderError::Other { provider, .. } => provider,
        }
    }
}

// ---------------------------------------------------------------------------
// Display
// ---------------------------------------------------------------------------

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::ContextOverflow {
                provider,
                message,
                max_tokens,
            } => {
                write!(f, "[{}] Context overflow: {}", provider, message)?;
                if let Some(max) = max_tokens {
                    write!(f, " (max {} tokens)", max)?;
                }
                Ok(())
            }
            ProviderError::RateLimited {
                provider,
                retry_after,
            } => {
                write!(f, "[{}] Rate limited", provider)?;
                if let Some(secs) = retry_after {
                    write!(f, "; retry after {}s", secs)?;
                }
                Ok(())
            }
            ProviderError::AuthFailed { provider, message } => {
                write!(f, "[{}] Authentication failed: {}", provider, message)
            }
            ProviderError::QuotaExceeded { provider, message } => {
                write!(f, "[{}] Quota exceeded: {}", provider, message)
            }
            ProviderError::ModelNotFound {
                provider,
                model,
                suggestions,
            } => {
                write!(f, "[{}] Model not found: {}", provider, model)?;
                if !suggestions.is_empty() {
                    write!(f, " (suggestions: {})", suggestions.join(", "))?;
                }
                Ok(())
            }
            ProviderError::ServerError {
                provider,
                status,
                message,
                ..
            } => match status {
                Some(s) => write!(f, "[{}] Server error {}: {}", provider, s, message),
                None => write!(f, "[{}] Server error: {}", provider, message),
            },
            ProviderError::InvalidRequest { provider, message } => {
                write!(f, "[{}] Invalid request: {}", provider, message)
            }
            ProviderError::ContentFiltered { provider, message } => {
                write!(f, "[{}] Content filtered: {}", provider, message)
            }
            ProviderError::StreamError {
                provider, message, ..
            } => {
                write!(f, "[{}] Stream error: {}", provider, message)
            }
            ProviderError::Other {
                provider,
                message,
                status,
                ..
            } => match status {
                Some(s) => write!(f, "[{}] Error {}: {}", provider, s, message),
                None => write!(f, "[{}] Error: {}", provider, message),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// std::error::Error
// ---------------------------------------------------------------------------

impl std::error::Error for ProviderError {}

// ---------------------------------------------------------------------------
// From<ProviderError> for ClaudeError
// ---------------------------------------------------------------------------

impl From<ProviderError> for ClaudeError {
    fn from(err: ProviderError) -> Self {
        match &err {
            ProviderError::ContextOverflow { .. } => ClaudeError::ContextWindowExceeded,
            ProviderError::RateLimited { .. } => ClaudeError::RateLimit,
            ProviderError::AuthFailed { message, .. } => ClaudeError::Auth(message.clone()),
            ProviderError::ServerError {
                status: Some(s),
                message,
                ..
            } => ClaudeError::ApiStatus {
                status: *s,
                message: message.clone(),
            },
            _ => ClaudeError::Api(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderError, RecoveryClass};
    use clawde_core::provider_id::ProviderId;

    fn provider() -> ProviderId {
        ProviderId::new("test")
    }

    #[test]
    fn recovery_classes_have_stable_policy() {
        let cases = [
            (
                ProviderError::AuthFailed {
                    provider: provider(),
                    message: "invalid".into(),
                },
                RecoveryClass::InvalidCredential,
                true,
                true,
            ),
            (
                ProviderError::QuotaExceeded {
                    provider: provider(),
                    message: "quota".into(),
                },
                RecoveryClass::QuotaExhausted,
                true,
                true,
            ),
            (
                ProviderError::ServerError {
                    provider: provider(),
                    status: Some(503),
                    message: "busy".into(),
                    is_retryable: true,
                },
                RecoveryClass::TransientProvider,
                true,
                false,
            ),
            (
                ProviderError::InvalidRequest {
                    provider: provider(),
                    message: "bad parameter".into(),
                },
                RecoveryClass::MalformedRequest,
                // Malformed requests are fallbackable: another upstream may
                // accept what this one rejected. The credential is still not
                // blamed (cools_key false) and the same provider is not
                // retried.
                true,
                false,
            ),
            (
                ProviderError::ContentFiltered {
                    provider: provider(),
                    message: "blocked".into(),
                },
                RecoveryClass::ContentFiltered,
                false,
                false,
            ),
        ];

        for (error, class, may_fallback, cools_key) in cases {
            assert_eq!(error.recovery_class(), class);
            assert_eq!(error.may_fallback(), may_fallback);
            assert_eq!(class.cools_key(), cools_key);
            assert!(!class.as_str().is_empty());
        }
    }

    #[test]
    fn untyped_client_errors_classify_as_malformed_and_fall_back() {
        // The same HTTP status must not mean different things depending on
        // whether the body carried an `invalid_request_error` code: a 400 with
        // no code lands in `Other`, and it has to match the typed path.
        for status in [400u16, 422] {
            let error = ProviderError::Other {
                provider: provider(),
                message: "property 'thinking' is unsupported".into(),
                status: Some(status),
                body: Some("{\"error\":\"unsupported\"}".into()),
            };
            assert_eq!(
                error.recovery_class(),
                RecoveryClass::MalformedRequest,
                "status {status}"
            );
            assert!(error.may_fallback(), "status {status} must fall back");
            // A rejected request shape is not the credential's fault and must
            // not be retried on the same upstream.
            assert!(!error.recovery_class().cools_key());
            assert!(!error.recovery_class().may_retry_same_provider());
        }
    }

    #[test]
    fn content_filtered_stays_terminal_even_though_malformed_falls_back() {
        let filtered = ProviderError::ContentFiltered {
            provider: provider(),
            message: "blocked".into(),
        };
        assert!(!filtered.may_fallback());
    }

    #[test]
    fn context_overflow_is_distinct_from_malformed_request() {
        let error = ProviderError::InvalidRequest {
            provider: provider(),
            message: "maximum context length exceeded".into(),
        };
        assert_eq!(error.recovery_class(), RecoveryClass::ContextOverflow);
        assert!(error.may_fallback());
        assert!(error.replay_safe());
    }

    #[test]
    fn visible_stream_failure_is_not_replay_safe() {
        let error = ProviderError::StreamError {
            provider: provider(),
            message: "connection closed".into(),
            partial_response: Some("already visible".into()),
        };
        assert_eq!(error.recovery_class(), RecoveryClass::VisibleStreamFailure);
        assert!(!error.may_fallback());
        assert!(!error.replay_safe());
    }

    #[test]
    fn status_based_other_errors_are_classified() {
        let cases = [
            (401, RecoveryClass::InvalidCredential),
            (402, RecoveryClass::QuotaExhausted),
            (413, RecoveryClass::ContextOverflow),
            (429, RecoveryClass::RateLimited),
            (503, RecoveryClass::TransientProvider),
        ];
        for (status, expected) in cases {
            let error = ProviderError::Other {
                provider: provider(),
                message: "provider response".into(),
                status: Some(status),
                body: None,
            };
            assert_eq!(error.recovery_class(), expected);
        }
    }
}
