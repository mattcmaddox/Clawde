// providers/openai_compat.rs — OpenAI-Compatible generic provider adapter.
//
// A configurable OpenAI Chat Completions adapter that can target any
// provider exposing an OpenAI-compatible API.  Configure base URL, auth,
// extra headers, and per-provider behavioural quirks via the builder API.

use std::pin::Pin;

use async_stream::stream;
use async_trait::async_trait;
use clawde_core::provider_id::{ModelId, ProviderId};
use clawde_core::types::ContentBlock;
use futures::Stream;
use serde_json::{json, Value};

use crate::provider::{LlmProvider, ModelInfo};
use crate::provider_error::ProviderError;
use crate::provider_types::{
    ProviderCapabilities, ProviderRequest, ProviderResponse, ProviderStatus, RateLimitObservation,
    StreamEvent, SystemPromptStyle,
};

// Re-use the message transformation helpers from openai.rs.
use super::openai::OpenAiProvider;
use super::request_options::merge_openai_compatible_options;

// ---------------------------------------------------------------------------
// ProviderQuirks
// ---------------------------------------------------------------------------

/// Provider-specific behavioural quirks that alter how the generic adapter
/// builds and interprets requests/responses.
#[derive(Debug, Clone)]
pub struct ProviderQuirks {
    /// Truncate tool call IDs to at most this many characters before sending.
    /// For example, Mistral requires tool IDs of at most 9 characters.
    pub tool_id_max_len: Option<usize>,

    /// If `true`, strip all non-alphanumeric characters from tool IDs.
    pub tool_id_alphanumeric_only: bool,

    /// Extra error-message substrings (or regex-like patterns) that indicate
    /// the request exceeded the model's context window.
    pub overflow_patterns: Vec<String>,

    /// Whether to send `{"stream_options": {"include_usage": true}}` when
    /// streaming.  Required by some providers to receive token counts.
    pub include_usage_in_stream: bool,

    /// Override the sampling temperature when the request does not specify one.
    pub default_temperature: Option<f64>,

    /// Some providers (e.g. older Mistral releases) reject a message sequence
    /// that goes …tool_result → user… without an intervening assistant turn.
    /// When `true`, an `{"role":"assistant","content":"Done."}` message is
    /// inserted between any `role: tool` message and a following `role: user`
    /// message.
    pub fix_tool_user_sequence: bool,

    /// Name of the JSON field in the assistant message that carries extended
    /// reasoning / thinking text.  `None` means the provider does not expose
    /// reasoning output.  Example: `Some("reasoning_content")` for DeepSeek.
    pub reasoning_field: Option<String>,

    /// Whether this provider requires reasoning_content to be echoed back on
    /// subsequent turns in multi-turn conversations.  DeepSeek V4 and OpenCode
    /// Zen's thinking-mode free models are currently the providers with this
    /// requirement; most providers ignore this field. When false, reasoning is
    /// not included in outbound messages to save tokens.
    pub requires_reasoning_roundtrip: bool,

    /// Hard cap on `max_tokens` sent to this provider.  When the request
    /// carries a higher value it is silently clamped down to this limit.
    /// Use this for providers whose models have a lower output ceiling than
    /// the default we request (e.g. DeepSeek Chat caps at 8 192).
    pub max_tokens_cap: Option<u32>,

    /// Hard cap on total tokens (prompt + max_tokens) for this provider.
    /// When set, the provider truncates the system prompt to ensure the
    /// total estimated token count stays under this limit.
    /// Use this for providers with tight TPM rate limits (e.g. Groq free
    /// tier limits to 12 000 TPM).
    pub max_total_tokens: Option<u32>,

    /// Set to `true` for providers that never require an API key (e.g.
    /// Ollama, LM Studio, llama.cpp).  When `true`, `health_check()` will
    /// always attempt a live network probe regardless of whether the base URL
    /// points to a local or remote host, instead of short-circuiting with
    /// "No API key configured".
    pub no_api_key_required: bool,

    /// When set, `discover_models()` uses Ollama's native `/api/tags` endpoint
    /// (and optionally `/api/show` for per-model metadata) instead of the
    /// OpenAI-compatible `/v1/models` endpoint.  The value is the Ollama host
    /// root (e.g. `"http://gpu-host.example:11434"`) so the native API can be
    /// called independently of the `/v1` base URL used for chat completions.
    pub ollama_native_host: Option<String>,

    /// Estimated bytes-per-token ratio for prompt truncation.
    /// Default is `4.0` (typical for English prose). Code-heavy content
    /// like system prompts / tool definitions tokenizes at ~1.5 bytes/token.
    pub bytes_per_token: f64,

    /// Set to `true` for providers that reject OpenAI-style content arrays
    /// and require `message.content` to be a plain string (e.g. Cloudflare
    /// Workers AI).  When `true`, any content array (built from multi-block
    /// user messages) is flattened to the text parts joined with newlines,
    /// and `content: null` on assistant tool-call turns is replaced with `""`.
    pub string_content_only: bool,
}

impl Default for ProviderQuirks {
    fn default() -> Self {
        Self {
            tool_id_max_len: None,
            tool_id_alphanumeric_only: false,
            overflow_patterns: Vec::new(),
            include_usage_in_stream: false,
            default_temperature: None,
            fix_tool_user_sequence: false,
            reasoning_field: None,
            requires_reasoning_roundtrip: false,
            max_tokens_cap: None,
            max_total_tokens: None,
            no_api_key_required: false,
            ollama_native_host: None,
            bytes_per_token: 4.0, // prose-safe default; code-heavy providers override lower
            string_content_only: false,
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAiCompatProvider
// ---------------------------------------------------------------------------

pub struct OpenAiCompatProvider {
    pub(crate) id: ProviderId,
    pub(crate) name: String,
    base_url: String,
    api_key: Option<String>,
    extra_headers: Vec<(String, String)>,
    pub(crate) quirks: ProviderQuirks,
    http_client: reqwest::Client,
}

impl OpenAiCompatProvider {
    /// Create a new compat provider.  `base_url` should already include any
    /// path prefix (e.g. `"https://api.groq.com/openai/v1"`).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(crate::request_timeout())
            .build()
            .expect("failed to build reqwest client");

        Self {
            id: ProviderId::new(id),
            name: name.into(),
            base_url: base_url.into(),
            api_key: None,
            extra_headers: Vec::new(),
            quirks: ProviderQuirks::default(),
            http_client,
        }
    }

    /// Set an API key that will be sent as `Authorization: Bearer <key>`.
    pub fn with_api_key(mut self, key: String) -> Self {
        self.api_key = if key.is_empty() { None } else { Some(key) };
        self
    }

    /// Append a custom header sent on every request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    /// Return whether an exact custom header is configured.
    ///
    /// This is crate-visible so provider factories can regression-test their
    /// protocol-specific headers without exposing the internal header store.
    #[cfg(test)]
    pub(crate) fn has_header(&self, name: &str, value: &str) -> bool {
        self.extra_headers
            .iter()
            .any(|(configured_name, configured_value)| {
                configured_name == name && configured_value == value
            })
    }

    /// Apply provider-specific quirks.
    pub fn with_quirks(mut self, quirks: ProviderQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Override the base URL (e.g. from a user-supplied --api-base flag).
    ///
    /// When the provider uses Ollama's native API host (set by the `ollama()`
    /// factory), keep it in sync with the new base URL.  Otherwise health
    /// checks and native model discovery would keep targeting the original
    /// (localhost) host even though chat completions go to the overridden
    /// server.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        if self.quirks.ollama_native_host.is_some() {
            let native_host = self
                .base_url
                .trim_end_matches('/')
                .trim_end_matches("/v1")
                .trim_end_matches('/')
                .to_string();
            self.quirks.ollama_native_host = Some(native_host);
        }
        self
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Returns `true` when the provider has no usable API key.
    fn has_no_key(&self) -> bool {
        self.api_key.is_none()
    }

    /// Scrub a tool-call ID according to the configured quirks.
    fn scrub_tool_id(&self, id: &str) -> String {
        let mut s = id.to_string();
        if self.quirks.tool_id_alphanumeric_only {
            s = s.chars().filter(|c| c.is_alphanumeric()).collect();
        }
        if let Some(max_len) = self.quirks.tool_id_max_len {
            let truncated: String = s.chars().take(max_len).collect();
            s = format!("{:0<width$}", truncated, width = max_len);
        }
        s
    }

    /// Apply `scrub_tool_id` to every tool-call id/tool_call_id in a messages
    /// array that was already built by `OpenAiProvider::to_openai_messages`.
    fn apply_tool_id_quirks(&self, messages: &mut [Value]) {
        if self.quirks.tool_id_max_len.is_none() && !self.quirks.tool_id_alphanumeric_only {
            return;
        }
        for msg in messages.iter_mut() {
            // assistant message tool_calls[].id
            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|v| v.as_array_mut()) {
                for tc in tcs.iter_mut() {
                    if let Some(id_val) = tc.get("id").and_then(|v| v.as_str()) {
                        let scrubbed = self.scrub_tool_id(id_val);
                        if let Some(obj) = tc.as_object_mut() {
                            obj.insert("id".to_string(), json!(scrubbed));
                        }
                    }
                }
            }
            // tool message tool_call_id
            if let Some(id_val) = msg.get("tool_call_id").and_then(|v| v.as_str()) {
                let scrubbed = self.scrub_tool_id(id_val);
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("tool_call_id".to_string(), json!(scrubbed));
                }
            }
        }
    }

    /// Insert `{"role":"assistant","content":"Done."}` between any
    /// `role: tool` message that is immediately followed by a `role: user`
    /// message.
    fn apply_fix_tool_user_sequence(messages: &mut Vec<Value>) {
        let mut i = 0;
        while i + 1 < messages.len() {
            let current_is_tool = messages[i].get("role").and_then(|v| v.as_str()) == Some("tool");
            let next_is_user = messages[i + 1].get("role").and_then(|v| v.as_str()) == Some("user");

            if current_is_tool && next_is_user {
                messages.insert(i + 1, json!({ "role": "assistant", "content": "Done." }));
                i += 2; // skip past the inserted message and the user message
            } else {
                i += 1;
            }
        }
    }

    /// Build the full messages array, applying all quirks.
    fn build_messages(&self, request: &ProviderRequest) -> Vec<Value> {
        let mut messages = OpenAiProvider::to_openai_messages_pub(
            &request.messages,
            request.system_prompt.as_ref(),
        );

        self.apply_tool_id_quirks(&mut messages);

        if self.quirks.fix_tool_user_sequence {
            Self::apply_fix_tool_user_sequence(&mut messages);
        }

        // For providers that require reasoning_content in multi-turn conversations
        // (e.g. DeepSeek V4), inject reasoning text back into assistant messages
        // that contain tool calls. Non-tool-call turns omit the field to save tokens.
        // Only providers with requires_reasoning_roundtrip=true need this.
        if self.quirks.requires_reasoning_roundtrip {
            if let Some(ref field) = self.quirks.reasoning_field {
                Self::inject_reasoning_for_tool_turns(&mut messages, &request.messages, field);
            }
        }

        // Some providers (DeepSeek when reasoning_roundtrip enabled, Ollama) reject
        // `content: null` on assistant messages — replace with an empty string.
        if self.quirks.requires_reasoning_roundtrip || self.quirks.no_api_key_required {
            Self::ensure_content_not_null(&mut messages);
        }

        // Providers that require string content (Cloudflare Workers AI) get
        // every content value flattened to a plain string and `content: null`
        // replaced with `""`. Both transformations must run before truncation
        // so the byte estimate sees the final wire format.
        if self.quirks.string_content_only {
            Self::flatten_content_to_string(&mut messages);
            Self::ensure_content_not_null(&mut messages);
        }

        // Max-total-tokens truncation: when `max_total_tokens` is set, estimate
        // the total token count (prompt + max_tokens) using a simple byte heuristic
        // and truncate the system message to fit within the budget.
        // Rough estimate: 1 token ≈ 4 bytes for typical text.
        if let Some(total_limit) = self.quirks.max_total_tokens {
            let max_tokens = self.quirks.max_tokens_cap.unwrap_or(request.max_tokens);
            let budget_prompt_tokens = total_limit.saturating_sub(max_tokens) as usize;
            if budget_prompt_tokens < 100 {
                // Budget too small for any meaningful prompt.
                return messages;
            }
            // Convert the token budget to a byte budget using the provider's
            // configured bytes-per-token ratio. Code-heavy content (Clawde
            // system prompt, AGENTS.md, git context) tokenizes at ~1.4-1.5
            // bytes/token, much denser than the 4.0 default for English prose.
            let ratio = self.quirks.bytes_per_token;
            let budget_bytes = (budget_prompt_tokens as f64 * ratio) as usize;

            // The tools array is a separate request-body field and its JSON
            // bytes count against the provider's token budget too (Groq's TPM
            // limit is enforced on the whole request). Reserve it out of the
            // prompt budget so truncation accounts for the full request size.
            // When the tools array alone exceeds the prompt budget, reserve
            // what fits (keep at least a 64-byte prompt floor) so truncation
            // still runs instead of silently skipping.
            let tools_bytes: usize = if request.tools.is_empty() {
                0
            } else {
                OpenAiProvider::to_openai_tools_pub(&request.tools)
                    .iter()
                    .map(|tool| tool.to_string().len())
                    .sum()
            };
            let prompt_budget_bytes = budget_bytes.saturating_sub(tools_bytes);

            // Estimate current total byte size of all serialised messages.
            // Using serde_json's Display (compact JSON) for accuracy.
            let current_bytes: usize = messages.iter().map(|m| m.to_string().len()).sum();

            if current_bytes > prompt_budget_bytes {
                if tracing::enabled!(tracing::Level::DEBUG) {
                    // Only serialize for the debug log; the truncation path
                    // below re-computes sizes for the actual budget math.
                    let system_bytes: usize = messages
                        .first()
                        .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
                        .map(|m| m.to_string().len())
                        .unwrap_or(0);
                    tracing::debug!(
                        current_bytes,
                        system_bytes,
                        non_system_bytes = current_bytes.saturating_sub(system_bytes),
                        prompt_budget_bytes,
                        tools_bytes,
                        budget_bytes,
                        total_limit,
                        max_tokens,
                        ratio,
                        "max_total_tokens: request exceeds budget, truncating system prompt"
                    );
                }
                // Pre-compute the byte size of non-system messages so we don't
                // need to borrow `messages` again while holding a mutable ref.
                let non_system_bytes: usize =
                    messages.iter().skip(1).map(|m| m.to_string().len()).sum();

                // Truncate the first (system) message content to fit the budget.
                // Even when multi-turn history dominates the budget, always
                // shrink the system prompt down to its floor (14 bytes) instead
                // of skipping truncation: the system prompt is the largest
                // single removable block, and leaving it untouched guarantees
                // the request stays over budget.
                if let Some(system_msg) = messages.first_mut() {
                    if system_msg.get("role").and_then(|r| r.as_str()) == Some("system") {
                        if let Some(content_val) = system_msg.get_mut("content") {
                            if let Some(content) = content_val.as_str() {
                                let content_bytes = content.len();
                                let sys_budget =
                                    prompt_budget_bytes.saturating_sub(non_system_bytes);
                                // Reserve ~50 bytes for the truncation suffix.
                                let max_content_bytes = sys_budget.saturating_sub(50);
                                // Truncate whenever the system prompt exceeds the
                                // budget. There is no minimum-budget skip: when
                                // tools+history consume the whole budget
                                // (max_content_bytes = 0), still shrink to the
                                // 14-byte floor rather than sending the request
                                // untruncated (which guaranteed the TPM error).
                                if content_bytes > max_content_bytes {
                                    // Need to truncate. Keep at least 14 bytes.
                                    let keep_bytes =
                                        std::cmp::max(max_content_bytes, 14).min(content_bytes);
                                    let truncate_to = content
                                        .char_indices()
                                        .take_while(|(b, _)| *b < keep_bytes)
                                        .last()
                                        .map(|(i, c)| i + c.len_utf8())
                                        .unwrap_or(0)
                                        .min(content_bytes);
                                    let truncated = &content[..truncate_to];
                                    *content_val = json!(format!(
                                        "{}... [truncated to fit provider token limit]",
                                        truncated
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }

        messages
    }

    /// For providers that expose a reasoning field, inject the reasoning
    /// text into assistant messages that contain tool calls.
    ///
    /// DeepSeek's thinking mode requires `reasoning_content` to be sent back
    /// on turns where tool calls occurred. MiniMax Console (the strict backend
    /// behind OpenCode Zen's gateway) additionally requires the field on
    /// **every** assistant tool-call turn once the conversation is in thinking
    /// mode — even turns where the model emitted no reasoning. Flash-class
    /// thinking models skip thinking on some turns, so a strict 1:1 pairing
    /// leaves those turns without `reasoning_content` and the strict backend
    /// rejects the whole request. The fix: turns with their own captured
    /// reasoning use it; turns without one carry forward the most recent
    /// reasoning so the field is always present.
    fn inject_reasoning_for_tool_turns(
        json_messages: &mut [Value],
        original_messages: &[clawde_core::types::Message],
        field: &str,
    ) {
        use clawde_core::types::{MessageContent, Role};

        // Build one reasoning record per original assistant tool-call turn, in
        // order. A turn uses its own captured thinking when present; otherwise
        // it carries forward the most recent reasoning (flash-class thinking
        // models skip thinking on some turns, and MiniMax Console's strict
        // backend still demands the field once the conversation is in thinking
        // mode). Turns before any reasoning has been emitted stay `None` — the
        // API cannot demand reasoning it never saw.
        let mut per_turn: Vec<Option<String>> = Vec::new();
        let mut last_reasoning: Option<&str> = None;
        for msg in original_messages {
            if msg.role != Role::Assistant {
                continue;
            }
            let blocks = match &msg.content {
                MessageContent::Blocks(b) => b,
                _ => continue,
            };
            if !blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            {
                continue;
            }
            let thinking: Vec<&str> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
                    _ => None,
                })
                .collect();
            if !thinking.is_empty() {
                last_reasoning = Some(thinking[0]);
                per_turn.push(Some(thinking.join("")));
            } else if let Some(last) = last_reasoning {
                per_turn.push(Some(last.to_string()));
            } else {
                per_turn.push(None);
            }
        }

        if per_turn.iter().all(Option::is_none) {
            return;
        }

        // Inject into JSON messages: the i-th assistant tool-call message
        // corresponds to the i-th original tool-call turn, so consume the
        // per-turn records in order.
        let mut turn_idx = 0;
        for msg in json_messages.iter_mut() {
            if turn_idx >= per_turn.len() {
                break;
            }
            let is_assistant = msg.get("role").and_then(|r| r.as_str()) == Some("assistant");
            let has_tool_calls = msg
                .get("tool_calls")
                .and_then(|tc| tc.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !(is_assistant && has_tool_calls) {
                continue;
            }
            if let Some(reasoning) = per_turn[turn_idx].as_ref() {
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert(field.to_string(), Value::String(reasoning.clone()));
                }
            }
            turn_idx += 1;
        }
    }

    /// Flatten OpenAI-style content arrays into plain string content.
    ///
    /// Some providers (Cloudflare Workers AI) reject `message.content` when
    /// it is an array of `{type, text}` parts and require a plain string.
    /// Text parts are joined with newlines; non-text parts (images, etc.)
    /// are dropped since these providers do not support them anyway.
    fn flatten_content_to_string(messages: &mut [Value]) {
        for msg in messages.iter_mut() {
            let Some(obj) = msg.as_object_mut() else {
                continue;
            };
            let Some(content) = obj.get_mut("content") else {
                continue;
            };
            let Some(parts) = content.as_array() else {
                continue;
            };
            let text = parts
                .iter()
                .filter_map(|part| {
                    if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                        part.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            *content = Value::String(text);
        }
    }

    /// Replace `content: null` with `content: ""` on all assistant messages.
    ///
    /// DeepSeek's API rejects assistant messages that have `content: null`
    /// (it treats null as absent and then complains that neither content nor
    /// tool_calls is set).  Replacing with an empty string satisfies the
    /// validation while preserving semantics.
    fn ensure_content_not_null(messages: &mut [Value]) {
        for msg in messages.iter_mut() {
            let is_assistant = msg.get("role").and_then(|r| r.as_str()) == Some("assistant");
            if !is_assistant {
                continue;
            }
            if let Some(obj) = msg.as_object_mut() {
                if let Some(content) = obj.get("content") {
                    if content.is_null() {
                        obj.insert("content".to_string(), Value::String(String::new()));
                    }
                }
            }
        }
    }

    /// Resolve the temperature to use: request value takes priority, then
    /// the quirk default, then nothing (let the API default apply).
    fn resolve_temperature(&self, request: &ProviderRequest) -> Option<f64> {
        request.temperature.or(self.quirks.default_temperature)
    }

    /// Attach the authorization header if an API key is configured.
    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            builder.header("Authorization", format!("Bearer {}", key))
        } else {
            builder
        }
    }

    /// Attach all configured extra headers.
    fn apply_extra_headers(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.extra_headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }

    // -----------------------------------------------------------------------
    // Non-streaming
    // -----------------------------------------------------------------------

    async fn create_message_non_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        let messages = self.build_messages(request);
        let tools = OpenAiProvider::to_openai_tools_pub(&request.tools);

        let max_tokens = match self.quirks.max_tokens_cap {
            Some(cap) => request.max_tokens.min(cap),
            None => request.max_tokens,
        };
        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": false,
        });

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(t) = self.resolve_temperature(request) {
            body["temperature"] = json!(t);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }
        merge_openai_compatible_options(&mut body, &request.provider_options);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json");
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        let rate_limit = {
            let headers = resp.headers();
            let tokens = crate::client::extract_rate_limit_pct(
                headers,
                "x-ratelimit-remaining-tokens",
                "x-ratelimit-limit-tokens",
            );
            let requests = crate::client::extract_rate_limit_pct(
                headers,
                "x-ratelimit-remaining-requests",
                "x-ratelimit-limit-requests",
            );
            let (retry_after_secs, reset_at_unix) = crate::client::extract_rate_limit_timing(
                headers,
                &["x-ratelimit-reset-tokens", "x-ratelimit-reset-requests"],
            );
            (tokens.is_some()
                || requests.is_some()
                || retry_after_secs.is_some()
                || reset_at_unix.is_some())
            .then_some(RateLimitObservation {
                key_idx: None,
                tokens_pct_used: tokens,
                requests_pct_used: requests,
                retry_after_secs,
                reset_at_unix,
            })
        };
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read response body: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            if status == 404 {
                if let Some(retry) = self
                    .retry_ollama_if_model_advertised(&request.model, &url, &body, None)
                    .await
                {
                    let retry_response = retry?;
                    let retry_status = retry_response.status().as_u16();
                    let retry_text =
                        retry_response
                            .text()
                            .await
                            .map_err(|error| ProviderError::Other {
                                provider: self.id.clone(),
                                message: format!("Failed to read Ollama retry response: {error}"),
                                status: Some(retry_status),
                                body: None,
                            })?;
                    if !(200..300).contains(&(retry_status as usize)) {
                        return Err(self.map_http_error(retry_status, &retry_text));
                    }
                    let retry_json: Value = serde_json::from_str(&retry_text).map_err(|error| {
                        ProviderError::Other {
                            provider: self.id.clone(),
                            message: format!("Failed to parse Ollama retry response JSON: {error}"),
                            status: Some(retry_status),
                            body: None,
                        }
                    })?;
                    return OpenAiProvider::parse_non_streaming_response_pub(&retry_json, &self.id);
                }
            }
            // Small local Ollama models (1B–3B) often don't support tool
            // calling. When that's the 400 reason, retry without tools.
            if status == 400 && text.contains("does not support tools") && !tools.is_empty() {
                let retry_resp = self
                    .retry_without_tools(&url, &mut body, None /* no Accept */)
                    .await?;
                let retry_status = retry_resp.status().as_u16();
                let retry_text = retry_resp.text().await.map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("Failed to read retry response: {}", e),
                    status: Some(retry_status),
                    body: None,
                })?;
                if !(200..300).contains(&(retry_status as usize)) {
                    return Err(self.map_http_error(retry_status, &retry_text));
                }
                let json: Value =
                    serde_json::from_str(&retry_text).map_err(|e| ProviderError::Other {
                        provider: self.id.clone(),
                        message: format!("Failed to parse retry response JSON: {}", e),
                        status: Some(retry_status),
                        body: Some(retry_text.clone()),
                    })?;
                return OpenAiProvider::parse_non_streaming_response_pub(&json, &self.id);
            }
            return Err(self.map_http_error(status, &text));
        }

        let json: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse response JSON: {}", e),
            status: Some(status),
            body: Some(text.clone()),
        })?;

        let mut response = OpenAiProvider::parse_non_streaming_response_pub(&json, &self.id)?;
        response.rate_limit = rate_limit;
        Ok(response)
    }

    /// Retry one transient Ollama 404 when the remote daemon still advertises
    /// the requested model. Ollama can briefly return 404 while a model is
    /// being loaded or its OpenAI-compatible route is settling; treating that
    /// first response as permanent makes the live semantic smoke flaky.
    ///
    /// This is deliberately restricted to providers with an Ollama native host
    /// and requires an exact `/api/tags` model match. Other providers and a
    /// genuinely absent model remain fail-fast.
    async fn retry_ollama_if_model_advertised(
        &self,
        model: &str,
        url: &str,
        body: &serde_json::Value,
        accept_header: Option<&str>,
    ) -> Option<Result<reqwest::Response, ProviderError>> {
        let host = self.quirks.ollama_native_host.as_deref()?;
        // Keep this recovery path remote-only even when an explicitly isolated
        // local profile has populated `ollama_native_host`. The normal provider
        // resolver already rejects loopback by default; this independent gate
        // prevents a future caller from bypassing that boundary here. Use the
        // normalized value too, so a configured `/v1` suffix never leaks into
        // the native `/api/tags` route.
        let tags_url = ollama_tags_url(host)?;
        let tags_response = self.http_client.get(&tags_url).send().await.ok()?;
        if !tags_response.status().is_success() {
            return None;
        }
        let tags: Value = tags_response.json().await.ok()?;
        let advertised = ollama_tags_advertise_model(&tags, model);
        if !advertised {
            return None;
        }

        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let mut builder = self
            .http_client
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(accept) = accept_header {
            builder = builder.header("Accept", accept);
        }
        builder = self.apply_auth(builder);
        builder = self.apply_extra_headers(builder);
        Some(
            builder
                .json(body)
                .send()
                .await
                .map_err(|error| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!(
                        "HTTP request failed (retry after transient Ollama 404): {error}"
                    ),
                    status: None,
                    body: None,
                }),
        )
    }

    /// Retry the request without tools when the model doesn't support them.
    /// Used by both streaming and non-streaming paths; small local Ollama
    /// models (1B–3B) often lack tool-calling support in their modelfile.
    async fn retry_without_tools(
        &self,
        url: &str,
        body: &mut serde_json::Value,
        accept_header: Option<&str>,
    ) -> Result<reqwest::Response, ProviderError> {
        body.as_object_mut().and_then(|obj| obj.remove("tools"));
        let mut builder = self
            .http_client
            .post(url)
            .header("Content-Type", "application/json");
        if let Some(h) = accept_header {
            builder = builder.header("Accept", h);
        }
        builder = self.apply_auth(builder);
        builder = self.apply_extra_headers(builder);
        builder
            .json(&*body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed (retry without tools): {}", e),
                status: None,
                body: None,
            })
    }

    // -----------------------------------------------------------------------
    // Streaming
    // -----------------------------------------------------------------------

    async fn do_streaming(
        &self,
        request: &ProviderRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let messages = self.build_messages(request);
        let tools = OpenAiProvider::to_openai_tools_pub(&request.tools);

        let max_tokens = match self.quirks.max_tokens_cap {
            Some(cap) => request.max_tokens.min(cap),
            None => request.max_tokens,
        };
        let mut body = json!({
            "model": request.model,
            "max_tokens": max_tokens,
            "messages": messages,
            "stream": true,
        });

        if self.quirks.include_usage_in_stream {
            body["stream_options"] = json!({ "include_usage": true });
        }

        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(t) = self.resolve_temperature(request) {
            body["temperature"] = json!(t);
        }
        if let Some(p) = request.top_p {
            body["top_p"] = json!(p);
        }
        if !request.stop_sequences.is_empty() {
            body["stop"] = json!(request.stop_sequences);
        }
        merge_openai_compatible_options(&mut body, &request.provider_options);

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let builder = self
            .http_client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream");
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Other {
                provider: self.id.clone(),
                message: format!("HTTP request failed: {}", e),
                status: None,
                body: None,
            })?;

        let status = resp.status().as_u16();
        if !(200..300).contains(&(status as usize)) {
            let text = resp.text().await.unwrap_or_default();
            if status == 404 {
                if let Some(retry) = self
                    .retry_ollama_if_model_advertised(
                        &request.model,
                        &url,
                        &body,
                        Some("text/event-stream"),
                    )
                    .await
                {
                    let retry_response = retry?;
                    let retry_status = retry_response.status().as_u16();
                    if (200..300).contains(&(retry_status as usize)) {
                        return Ok(retry_response);
                    }
                    let retry_text = retry_response.text().await.unwrap_or_default();
                    return Err(self.map_http_error(retry_status, &retry_text));
                }
            }
            // Small local Ollama models (1B–3B) often don't support tool
            // calling. When that's the 400 reason, retry without tools so
            // the user can still chat with these models.
            if status == 400 && text.contains("does not support tools") && !tools.is_empty() {
                let retry_resp = self
                    .retry_without_tools(&url, &mut body, Some("text/event-stream"))
                    .await?;
                let retry_status = retry_resp.status().as_u16();
                if !(200..300).contains(&(retry_status as usize)) {
                    let retry_text = retry_resp.text().await.unwrap_or_default();
                    return Err(self.map_http_error(retry_status, &retry_text));
                }
                return Ok(retry_resp);
            }
            return Err(self.map_http_error(status, &text));
        }

        Ok(resp)
    }

    // -----------------------------------------------------------------------
    // Ollama native model discovery
    // -----------------------------------------------------------------------

    /// List models using Ollama's native `/api/tags` endpoint, then enrich
    /// each model with metadata from `/api/show` (context window, parameter
    /// size, quantization level).
    ///
    /// Models are sorted with coding-oriented models first (names containing
    /// "code" or "coder"), then by parameter size descending, so the best
    /// local coding model naturally appears at the top.
    async fn discover_models_ollama_native(
        &self,
        ollama_host: &str,
    ) -> Result<Vec<ModelInfo>, ProviderError> {
        let tags_url = format!("{}/api/tags", ollama_host.trim_end_matches('/'));

        let resp =
            self.http_client
                .get(&tags_url)
                .send()
                .await
                .map_err(|e| ProviderError::Other {
                    provider: self.id.clone(),
                    message: format!("Ollama /api/tags request failed: {}", e),
                    status: None,
                    body: None,
                })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read /api/tags response: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            return Err(self.map_http_error(status, &text));
        }

        let json: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse /api/tags JSON: {}", e),
            status: Some(status),
            body: Some(text),
        })?;

        let models_arr = match json.get("models").and_then(|m| m.as_array()) {
            Some(m) => m,
            None => return Ok(vec![]),
        };

        // Collect model names from /api/tags.
        let model_names: Vec<String> = models_arr
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();

        // Fetch detailed metadata for each model via /api/show.
        let show_url_base = format!("{}/api/show", ollama_host.trim_end_matches('/'));
        let provider_id = self.id.clone();

        let mut models: Vec<(ModelInfo, bool, u64)> = Vec::with_capacity(model_names.len());

        for name in &model_names {
            let (context_window, max_output, is_coder, param_size) =
                self.fetch_ollama_model_info(&show_url_base, name).await;

            models.push((
                ModelInfo {
                    id: ModelId::new(name.as_str()),
                    provider_id: provider_id.clone(),
                    name: Self::ollama_display_name(name),
                    context_window,
                    max_output_tokens: max_output,
                    ..Default::default()
                },
                is_coder,
                param_size,
            ));
        }

        // Sort: coding models first, then by parameter size descending.
        models.sort_by(|a, b| {
            b.1.cmp(&a.1) // coders first
                .then_with(|| b.2.cmp(&a.2)) // larger models first
        });

        Ok(models.into_iter().map(|(info, _, _)| info).collect())
    }

    /// Call `/api/show` for a single model to extract its actual context
    /// window, parameter count, and whether it's coding-oriented.
    ///
    /// Returns `(context_window, max_output_tokens, is_coder, param_size_bytes)`.
    /// Falls back to sensible defaults if the request fails.
    async fn fetch_ollama_model_info(
        &self,
        show_url: &str,
        model_name: &str,
    ) -> (u32, u32, bool, u64) {
        let default_ctx = 4_096u32;
        let default_out = 2_048u32;
        let lower = model_name.to_lowercase();
        let is_coder_by_name = lower.contains("code")
            || lower.contains("coder")
            || lower.contains("codestral")
            || lower.contains("starcoder")
            || lower.contains("deepseek-coder")
            || lower.contains("qwen2.5-coder");

        let body = serde_json::json!({ "name": model_name });
        let resp = match self.http_client.post(show_url).json(&body).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => return (default_ctx, default_out, is_coder_by_name, 0),
        };

        let json: Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => return (default_ctx, default_out, is_coder_by_name, 0),
        };

        // Extract parameter size from model_info.
        let param_size = json
            .get("model_info")
            .and_then(|mi| mi.get("general.parameter_count").and_then(|v| v.as_u64()))
            .unwrap_or(0);

        // Extract num_ctx from the modelfile parameters or model_info.
        let num_ctx = Self::extract_num_ctx(&json).unwrap_or(default_ctx);

        // Max output is typically a fraction of context window for local
        // models.  Use half the context or 4096, whichever is smaller.
        let max_output = std::cmp::min(num_ctx / 2, 4_096);

        // Check if the model family or template indicates coding capability.
        let family = json
            .get("model_info")
            .and_then(|mi| mi.get("general.basename").and_then(|v| v.as_str()))
            .unwrap_or("");
        let is_coder = is_coder_by_name || family.contains("code") || family.contains("coder");

        (num_ctx, max_output, is_coder, param_size)
    }

    /// Extract `num_ctx` (context window) from the `/api/show` response.
    ///
    /// Ollama stores this in the modelfile parameters string (e.g.
    /// `"num_ctx 32768"`) or in `model_info` under context-length keys.
    fn extract_num_ctx(json: &Value) -> Option<u32> {
        // 1. Check model_info for context length keys.
        if let Some(mi) = json.get("model_info") {
            for key in &[
                "llama.context_length",
                "qwen2.context_length",
                "gemma.context_length",
                "gemma2.context_length",
                "phi3.context_length",
                "mistral.context_length",
                "starcoder2.context_length",
                "deepseek2.context_length",
                "command-r.context_length",
                "granite.context_length",
            ] {
                if let Some(v) = mi.get(*key).and_then(|v| v.as_u64()) {
                    return Some(v as u32);
                }
            }

            // Fallback: scan all keys ending in ".context_length"
            if let Some(obj) = mi.as_object() {
                for (k, v) in obj {
                    if k.ends_with(".context_length") {
                        if let Some(n) = v.as_u64() {
                            return Some(n as u32);
                        }
                    }
                }
            }
        }

        // 2. Parse from the modelfile parameters string.
        if let Some(params) = json.get("parameters").and_then(|p| p.as_str()) {
            for line in params.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("num_ctx") {
                    if let Ok(n) = rest.trim().parse::<u32>() {
                        return Some(n);
                    }
                }
            }
        }

        None
    }

    /// Produce a human-readable display name from an Ollama model name.
    ///
    /// `"qwen2.5-coder:32b-instruct-q4_K_M"` → `"Qwen 2.5 Coder (32B, Q4_K_M)"`
    fn ollama_display_name(raw: &str) -> String {
        let (base, tag) = raw.split_once(':').unwrap_or((raw, "latest"));

        let pretty_base = base
            .replace(['-', '_'], " ")
            .split_whitespace()
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    None => String::new(),
                    Some(c) => {
                        let upper: String = c.to_uppercase().collect();
                        format!("{}{}", upper, chars.as_str())
                    }
                }
            })
            .collect::<Vec<_>>()
            .join(" ");

        if tag == "latest" {
            return pretty_base;
        }

        let tag_parts: Vec<&str> = tag.split('-').collect();
        let mut size_part = None;
        let mut quant_part = None;
        for part in &tag_parts {
            let lower = part.to_lowercase();
            if lower.ends_with('b') && lower.trim_end_matches('b').parse::<f64>().is_ok() {
                size_part = Some(part.to_uppercase());
            } else if lower.starts_with('q') && lower.len() > 1 {
                quant_part = Some(part.to_uppercase());
            }
        }

        match (size_part, quant_part) {
            (Some(s), Some(q)) => format!("{} ({}, {})", pretty_base, s, q),
            (Some(s), None) => format!("{} ({})", pretty_base, s),
            (None, Some(q)) => format!("{} ({})", pretty_base, q),
            (None, None) => format!("{} ({})", pretty_base, tag),
        }
    }
}

/// Build Ollama's native tags URL only for a validated remote host.
fn ollama_tags_url(host: &str) -> Option<String> {
    let normalized = clawde_core::config::normalize_ollama_host(host)?;
    Some(format!("{}/api/tags", normalized.trim_end_matches('/')))
}

/// Return whether Ollama's native tags response contains an exact model name.
///
/// Ollama tags may include related names such as `model:latest`; substring or
/// prefix matching would incorrectly authorize a retry for a different model.
fn ollama_tags_advertise_model(tags: &Value, model: &str) -> bool {
    tags.get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("name").and_then(Value::as_str) == Some(model))
        })
}

// ---------------------------------------------------------------------------
// LlmProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn id(&self) -> &ProviderId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    async fn create_message(
        &self,
        request: ProviderRequest,
    ) -> Result<ProviderResponse, ProviderError> {
        if self.has_no_key() {
            // Providers that have no key set are considered unconfigured.
            // We allow the call to proceed in case the provider genuinely needs
            // no auth (e.g. Ollama), but callers that gate on health_check()
            // will see Unavailable first.
        }
        self.create_message_non_streaming(&request).await
    }

    async fn create_message_stream(
        &self,
        request: ProviderRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>
    {
        let resp = self.do_streaming(&request).await?;
        let provider_id = self.id.clone();
        let reasoning_field = self.quirks.reasoning_field.clone();

        // Extract x-ratelimit-* headers before consuming the body stream.
        // Uses the shared rate-limit percentage utility (also used by
        // AnthropicClient for anthropic-ratelimit-* headers).
        let rate_limit_event: Option<StreamEvent> = {
            let headers = resp.headers();
            let tokens = crate::client::extract_rate_limit_pct(
                headers,
                "x-ratelimit-remaining-tokens",
                "x-ratelimit-limit-tokens",
            );
            let requests = crate::client::extract_rate_limit_pct(
                headers,
                "x-ratelimit-remaining-requests",
                "x-ratelimit-limit-requests",
            );
            let (retry_after_secs, reset_at_unix) = crate::client::extract_rate_limit_timing(
                headers,
                &["x-ratelimit-reset-tokens", "x-ratelimit-reset-requests"],
            );
            if tokens.is_some()
                || requests.is_some()
                || retry_after_secs.is_some()
                || reset_at_unix.is_some()
            {
                Some(StreamEvent::RateLimitHeaders {
                    provider_id: provider_id.to_string(),
                    tokens_pct_used: tokens.unwrap_or(0.0),
                    requests_pct_used: requests.unwrap_or(0.0),
                    retry_after_secs,
                    reset_at_unix,
                    key_idx: None,
                })
            } else {
                None
            }
        };

        let s = stream! {
            use futures::StreamExt;

            // Yield rate-limit headers as the first event, if extracted.
            if let Some(evt) = rate_limit_event {
                yield Ok(evt);
            }

            let mut byte_stream = resp.bytes_stream();
            // Byte-buffering line decoder (#228): complete UTF-8 lines only, so
            // a multibyte codepoint straddling a chunk boundary is never lost.
            let mut byte_decoder = crate::SseByteDecoder::new();
            // Sans-IO OpenAI-Chat protocol decoder (#228): owns all message /
            // reasoning / tool-call / finish-reason decoding for this wire
            // format (extracted from this loop into `protocol::openai_chat`).
            let mut chat_decoder = crate::OpenAiChatDecoder::new(reasoning_field);

            // Bound infinite mid-stream stalls (issue #185): some
            // OpenAI-compatible providers begin a streamed tool call and then
            // pause indefinitely before sending the arguments. Wrap each chunk
            // read in a generous idle timeout so a stall surfaces as an error
            // instead of hanging forever. Each chunk resets the timer, so
            // slow-but-progressing local models are never cut off.
            let idle_timeout = crate::stream_idle_timeout();
            loop {
                let chunk_result = match tokio::time::timeout(
                    idle_timeout,
                    byte_stream.next(),
                )
                .await
                {
                    Ok(Some(chunk_result)) => chunk_result,
                    // Stream ended normally.
                    Ok(None) => break,
                    // No bytes for `idle_timeout` — provider stalled mid-stream.
                    Err(_) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: format!(
                                "Stream stalled: no data received for {}s; aborting to avoid hanging",
                                idle_timeout.as_secs()
                            ),
                            partial_response: None,
                        });
                        return;
                    }
                };
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        yield Err(ProviderError::StreamError {
                            provider: provider_id.clone(),
                            message: format!("Stream read error: {}", e),
                            partial_response: None,
                        });
                        return;
                    }
                };

                for line in byte_decoder.push(&chunk) {
                    let mut events = Vec::new();
                    let stop = chat_decoder.feed_line(&line, &mut events);
                    for event in events {
                        yield Ok(event);
                    }
                    if stop {
                        return;
                    }
                }
            }

            // Byte stream ended: flush a trailing MessageStop if content began
            // but no explicit `[DONE]` sentinel arrived.
            let mut tail = Vec::new();
            chat_decoder.finish(&mut tail);
            for event in tail {
                yield Ok(event);
            }
        };

        Ok(Box::pin(s))
    }

    async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // Use Ollama native API when configured — provides richer metadata
        // (parameter size, quantization, actual context window) than the
        // generic OpenAI-compat /v1/models endpoint.
        if let Some(ref ollama_host) = self.quirks.ollama_native_host {
            return self.discover_models_ollama_native(ollama_host).await;
        }

        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let builder = self.http_client.get(&url);
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        let resp = builder.send().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("HTTP request failed: {}", e),
            status: None,
            body: None,
        })?;

        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to read response body: {}", e),
            status: Some(status),
            body: None,
        })?;

        if !(200..300).contains(&(status as usize)) {
            return Err(self.map_http_error(status, &text));
        }

        let json: Value = serde_json::from_str(&text).map_err(|e| ProviderError::Other {
            provider: self.id.clone(),
            message: format!("Failed to parse models JSON: {}", e),
            status: Some(status),
            body: Some(text),
        })?;

        let data = match json.get("data").and_then(|d| d.as_array()) {
            Some(d) => d,
            None => return Ok(vec![]),
        };

        let provider_id = self.id.clone();
        let models: Vec<ModelInfo> = data
            .iter()
            .filter_map(|m| {
                let id = m.get("id").and_then(|v| v.as_str())?;
                Some(ModelInfo {
                    id: ModelId::new(id),
                    provider_id: provider_id.clone(),
                    name: id.to_string(),
                    context_window: match id {
                        "gpt-5" | "gpt-5.4" | "gpt-5.2" | "gpt-5-mini" | "gpt-5-nano"
                        | "gpt-5-chat-latest" | "gpt-5.2-codex" | "gpt-5.1-codex"
                        | "gpt-5.1-codex-mini" | "gpt-5.1-codex-max" => 400_000,
                        "o3" | "o3-mini" | "o4-mini" => 200_000,
                        _ => 128_000,
                    },
                    max_output_tokens: 16_384,
                    ..Default::default()
                })
            })
            .collect();

        Ok(models)
    }

    async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
        // Providers that need an API key but have none configured are
        // immediately unavailable without making a network call.
        if self.has_no_key() {
            // Providers that never require an API key (Ollama, LM Studio,
            // llama.cpp) should always proceed to the live health probe,
            // regardless of whether the base URL is local or remote.  This
            // allows remote/VPS-hosted instances to be used without a key.
            //
            // For all other providers a missing key means the env var was
            // absent or empty; report that without making a network call,
            // distinguishing only by URL when the quirk is not set.
            if !self.quirks.no_api_key_required {
                let is_local = self.base_url.contains("localhost")
                    || self.base_url.contains("127.0.0.1")
                    || self.base_url.contains("::1");

                if !is_local {
                    return Ok(ProviderStatus::Unavailable {
                        reason: "No API key configured".to_string(),
                    });
                }
            }
        }

        // For Ollama, prefer the native `/api/tags` endpoint over the
        // OpenAI-compatible `/v1/models` one — older Ollama versions do not
        // expose `/v1/models` and would return 404.
        let url = if let Some(ref host) = self.quirks.ollama_native_host {
            format!("{}/api/tags", host.trim_end_matches('/'))
        } else {
            format!("{}/models", self.base_url.trim_end_matches('/'))
        };
        let builder = self.http_client.get(&url);
        let builder = self.apply_auth(builder);
        let builder = self.apply_extra_headers(builder);

        match builder.send().await {
            Ok(r) if r.status().is_success() => Ok(ProviderStatus::Healthy),
            Ok(r) => Ok(ProviderStatus::Unavailable {
                reason: format!("models endpoint returned {}", r.status()),
            }),
            Err(e) => Ok(ProviderStatus::Unavailable {
                reason: e.to_string(),
            }),
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            tool_calling: true,
            thinking: self.quirks.reasoning_field.is_some(),
            image_input: true,
            pdf_input: false,
            audio_input: false,
            video_input: false,
            caching: false,
            structured_output: true,
            system_prompt_style: SystemPromptStyle::SystemMessage,
        }
    }

    fn max_tokens_cap_for(&self, _model: &str) -> Option<u32> {
        self.quirks.max_tokens_cap
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider_types::SystemPrompt;
    use serde_json::json;

    #[test]
    fn reasoning_roundtrip_injects_reasoning_content_on_tool_calls() {
        use clawde_core::types::{ContentBlock, Message, MessageContent, Role};
        use serde_json::json; // DeepSeek V4 / OpenCode Zen thinking models reject multi-turn tool-call
                              // requests that drop the previous turn's reasoning_content. The quirks
                              // must inject it back onto every assistant tool-call message.
        let provider = OpenAiCompatProvider::new(
            ProviderId::OPENCODE_ZEN,
            "OpenCode Zen",
            "http://example.test/v1",
        )
        .with_quirks(ProviderQuirks {
            reasoning_field: Some("reasoning_content".to_string()),
            requires_reasoning_roundtrip: true,
            ..Default::default()
        });

        let assistant_turn = |thinking: Option<&str>, id: &str| Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(
                thinking
                    .map(|t| ContentBlock::Thinking {
                        thinking: t.to_string(),
                        signature: String::new(),
                    })
                    .into_iter()
                    .chain(std::iter::once(ContentBlock::ToolUse {
                        id: id.to_string(),
                        name: "Write".to_string(),
                        input: json!({ "path": "/tmp/a.txt" }),
                        thought_signature: None,
                    }))
                    .collect(),
            ),
            uuid: None,
            cost: None,
            snapshot_patch: None,
            turn_meta: None,
        };

        let request = ProviderRequest {
            model: "opencode-zen/deepseek-v4-flash-free".to_string(),
            messages: vec![
                Message::user("write the file"),
                // Turn 1: thinking + tool call.
                assistant_turn(Some("first I plan"), "call_1"),
                // Turn 2: tool call WITHOUT thinking — flash models skip
                // thinking on some turns; the roundtrip must carry forward
                // the most recent reasoning so the strict backend sees the
                // field on every tool-call turn.
                assistant_turn(None, "call_2"),
                // Turn 3: thinking + tool call again.
                assistant_turn(Some("third I check"), "call_3"),
            ],
            system_prompt: None,
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);
        let assistants: Vec<&serde_json::Value> = messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"))
            .collect();
        assert_eq!(
            assistants.len(),
            3,
            "expected three assistant tool-call turns"
        );
        fn reasoning_of(m: &serde_json::Value) -> Option<&str> {
            m.get("reasoning_content").and_then(|v| v.as_str())
        }
        assert_eq!(
            reasoning_of(assistants[0]),
            Some("first I plan"),
            "turn with own reasoning keeps it"
        );
        assert_eq!(
            reasoning_of(assistants[1]),
            Some("first I plan"),
            "turn without reasoning carries forward the most recent text"
        );
        assert_eq!(
            reasoning_of(assistants[2]),
            Some("third I check"),
            "later turn with own reasoning replaces the carried text"
        );
        // The same quirk must also replace content:null with an empty string,
        // which thinking-mode APIs reject.
        for assistant in assistants {
            assert_eq!(
                assistant.get("content").and_then(|v| v.as_str()),
                Some(""),
                "content:null must be normalized to empty string"
            );
        }
    }

    #[test]
    fn max_total_tokens_truncates_large_system_prompt() {
        // Provider with max_total_tokens=1000, max_tokens_cap=100, bytes_per_token=1.5
        // Prompt budget = (1000 - 100) * 1.5 = 1350 bytes
        let provider = OpenAiCompatProvider::new("test", "Test", "https://example.com")
            .with_quirks(ProviderQuirks {
                max_total_tokens: Some(1_000),
                max_tokens_cap: Some(100),
                bytes_per_token: 1.5,
                ..Default::default()
            });

        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![],
            system_prompt: Some(SystemPrompt::Text("x".repeat(2000))),
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);

        // Must have at least one message (the system prompt).
        assert!(!messages.is_empty(), "expected at least the system message");

        // The system message content should contain the truncation suffix.
        let content = messages[0]["content"].as_str().unwrap_or("");
        assert!(
            content.contains("[truncated to fit provider token limit]"),
            "expected truncation suffix, got: {}",
            &content[content.len().saturating_sub(100)..]
        );

        // Total serialised byte size should be within budget (with some slack
        // for the JSON envelope overhead, but the system prompt content itself
        // is the dominant component).
        let total_bytes: usize = messages.iter().map(|m| m.to_string().len()).sum();
        let budget_bytes = ((1000 - 100) as f64 * 1.5) as usize;
        // Allow 100 bytes of slack for JSON keys/brackets not counted in content.
        assert!(
            total_bytes <= budget_bytes + 100,
            "total bytes {} exceeds budget {} + 100 slack",
            total_bytes,
            budget_bytes
        );
    }

    #[test]
    fn max_total_tokens_reserves_tools_array_bytes() {
        // The tools array is a separate request-body field that Groq counts
        // against its TPM limit. Truncation must reserve those bytes out of
        // the prompt budget so the system prompt is shrunk enough for the
        // full request (messages + tools) to fit.
        let provider = OpenAiCompatProvider::new("groq", "Groq", "https://example.com")
            .with_quirks(ProviderQuirks {
                max_total_tokens: Some(7_500),
                max_tokens_cap: Some(512),
                bytes_per_token: 4.5,
                ..Default::default()
            });

        use clawde_core::types::{Message, MessageContent, Role, ToolDefinition};
        // A system prompt that alone would fit the raw budget but not the
        // budget minus the tools array. Raw budget = (7500-512)*4.5 ~= 31.4KB;
        // with ~5.3KB of tools the reserved prompt budget is ~26KB. A 28KB
        // prompt overflows only once the tools bytes are reserved, isolating
        // the tools-reservation behavior.
        let system = "x".repeat(28_000);
        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("write the file".to_string()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
            system_prompt: Some(SystemPrompt::Text(system)),
            tools: vec![ToolDefinition {
                name: "Bash".to_string(),
                description: "run a command".repeat(200),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "cmd".repeat(200)},
                    },
                    "required": ["command"],
                }),
            }],
            max_tokens: 1_000,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);
        let system_content = messages[0]["content"].as_str().unwrap_or("");
        assert!(
            system_content.contains("[truncated to fit provider token limit]"),
            "expected system prompt to be truncated when tools are present"
        );
        // The truncated system prompt plus the tools array must fit the total
        // budget with the usual slack.
        let tools = OpenAiProvider::to_openai_tools_pub(&request.tools);
        let tools_bytes: usize = tools.iter().map(|t| t.to_string().len()).sum();
        let messages_bytes: usize = messages.iter().map(|m| m.to_string().len()).sum();
        let budget_bytes = ((7_500 - 512) as f64 * 4.5) as usize;
        assert!(
            messages_bytes + tools_bytes <= budget_bytes + 200,
            "messages+tools {} exceeds budget {} + slack",
            messages_bytes + tools_bytes,
            budget_bytes
        );
    }

    #[test]
    fn max_total_tokens_truncates_when_tools_consume_budget() {
        // Regression test for the guard bug: when the tools array alone
        // exceeds the byte budget, the old `max_content_bytes >= 14` guard
        // silently skipped truncation, so the request went out untruncated
        // and Groq rejected it (observed: `Limit 8000, Requested 10211` with
        // tools_bytes=19897 > budget). Truncation must still run and shrink
        // the system prompt down to its 14-byte floor.
        let provider = OpenAiCompatProvider::new("groq", "Groq", "https://example.com")
            .with_quirks(ProviderQuirks {
                max_total_tokens: Some(7_500),
                max_tokens_cap: Some(512),
                bytes_per_token: 4.5,
                ..Default::default()
            });

        use clawde_core::types::{Message, MessageContent, Role, ToolDefinition};
        // A large system prompt plus a tools array whose serialised bytes
        // exceed the whole prompt budget (~31.4KB), so `prompt_budget_bytes`
        // saturates to zero — the old `max_content_bytes >= 14` guard skipped
        // truncation entirely in this case (observed in live trials:
        // `Limit 8000, Requested 10211` with tools_bytes=19897 > budget).
        let system = "system-instruction-".repeat(2_000); // ~39KB
        let tool_schema = json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "long description ".repeat(2_000),
                }
            },
            "required": ["command"],
        });
        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("do the thing".to_string()),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
            system_prompt: Some(SystemPrompt::Text(system)),
            tools: vec![ToolDefinition {
                name: "Bash".to_string(),
                description: "run a command".repeat(2_000),
                input_schema: tool_schema,
            }],
            max_tokens: 1_000,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        // Sanity-check the fixture really overflows: tools bytes must exceed
        // the budget so the truncation runs under the guard-fix path.
        let tools_json = OpenAiProvider::to_openai_tools_pub(&request.tools);
        let tools_bytes: usize = tools_json.iter().map(|t| t.to_string().len()).sum();
        let budget_bytes = ((7_500 - 512) as f64 * 4.5) as usize;
        assert!(
            tools_bytes > budget_bytes,
            "fixture tools {} bytes must exceed budget {} for guard regression",
            tools_bytes,
            budget_bytes
        );

        let messages = provider.build_messages(&request);
        let system_content = messages[0]["content"].as_str().unwrap_or("");
        assert!(
            system_content.contains("[truncated to fit provider token limit]"),
            "expected system prompt to be truncated even when tools consume the budget"
        );
        // The truncated system prompt must be at its floor, not left intact.
        assert!(
            system_content.len() < 2_000,
            "expected system prompt shrunk to floor, got {} bytes",
            system_content.len()
        );
    }

    #[test]
    fn string_content_only_flattens_user_content_arrays() {
        // Cloudflare Workers AI rejects content arrays; the quirk must
        // flatten multi-block user messages to a plain string.
        let provider = OpenAiCompatProvider::new("cloudflare", "Cloudflare", "https://example.com")
            .with_quirks(ProviderQuirks {
                string_content_only: true,
                ..Default::default()
            });

        use clawde_core::types::{ContentBlock, Message, Role};

        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: clawde_core::types::MessageContent::Blocks(vec![
                    ContentBlock::Text {
                        text: "part one".to_string(),
                    },
                    ContentBlock::Text {
                        text: "part two".to_string(),
                    },
                ]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
            system_prompt: None,
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);
        assert_eq!(messages.len(), 1, "expected exactly one user message");
        let content = &messages[0]["content"];
        assert!(
            content.is_string(),
            "expected content to be a plain string, got: {}",
            content
        );
        assert_eq!(content.as_str().unwrap(), "part one\npart two");
    }

    #[test]
    fn string_content_only_fixes_assistant_null_content() {
        // Cloudflare also rejects `content: null` on assistant tool-call
        // turns (`'string' not in 'null'`); the quirk must rewrite it to "".
        let provider = OpenAiCompatProvider::new("cloudflare", "Cloudflare", "https://example.com")
            .with_quirks(ProviderQuirks {
                string_content_only: true,
                ..Default::default()
            });

        use clawde_core::types::{Message, MessageContent, Role};

        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
            system_prompt: None,
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);
        assert_eq!(messages.len(), 1);
        let content = &messages[0]["content"];
        assert!(
            content.is_string(),
            "expected content to be a string (not null), got: {}",
            content
        );
        assert_eq!(content.as_str().unwrap(), "");
    }

    #[test]
    fn without_string_content_only_keeps_content_arrays() {
        // Default behaviour: multi-block user messages stay as content arrays
        // (correct for OpenAI, Groq, etc.).
        let provider = OpenAiCompatProvider::new("groq", "Groq", "https://example.com");

        use clawde_core::types::{ContentBlock, Message, Role};

        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: clawde_core::types::MessageContent::Blocks(vec![ContentBlock::Text {
                    text: "part one".to_string(),
                }]),
                uuid: None,
                cost: None,
                snapshot_patch: None,
                turn_meta: None,
            }],
            system_prompt: None,
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);
        let content = &messages[0]["content"];
        assert!(
            content.is_array(),
            "expected content to stay an array without the quirk, got: {}",
            content
        );
    }

    #[test]
    fn max_total_tokens_skips_truncation_for_small_prompt() {
        // Same provider, but a small system prompt that fits within budget.
        let provider = OpenAiCompatProvider::new("test", "Test", "https://example.com")
            .with_quirks(ProviderQuirks {
                max_total_tokens: Some(1_000),
                max_tokens_cap: Some(100),
                bytes_per_token: 1.5,
                ..Default::default()
            });

        let request = ProviderRequest {
            model: "test-model".to_string(),
            messages: vec![],
            system_prompt: Some(SystemPrompt::Text("Hello, world!".to_string())),
            tools: vec![],
            max_tokens: 200,
            temperature: None,
            top_k: None,
            top_p: None,
            thinking: None,
            effort_level: None,
            stop_sequences: vec![],
            provider_options: Default::default(),
        };

        let messages = provider.build_messages(&request);

        // Should have one system message with the original content unchanged.
        assert_eq!(messages.len(), 1);
        let content = messages[0]["content"].as_str().unwrap_or("");
        assert_eq!(content, "Hello, world!");
    }

    #[test]
    fn bytes_per_token_default_four() {
        // Verify the default bytes_per_token is 4.0 (English prose conservative).
        let quirks = ProviderQuirks::default();
        assert!(
            (quirks.bytes_per_token - 4.0).abs() < f64::EPSILON,
            "expected default bytes_per_token=4.0, got {}",
            quirks.bytes_per_token
        );
    }

    #[test]
    fn mistral_tool_ids_match_opencode_style() {
        let provider = OpenAiCompatProvider::new("mistral", "Mistral", "https://example.com")
            .with_quirks(ProviderQuirks {
                tool_id_max_len: Some(9),
                tool_id_alphanumeric_only: true,
                ..Default::default()
            });

        assert_eq!(provider.scrub_tool_id("call-123456789abc"), "call12345");
        assert_eq!(provider.scrub_tool_id("x"), "x00000000");
    }

    #[test]
    fn fix_tool_user_sequence_inserts_done_between_tool_and_user() {
        let mut messages = vec![
            json!({"role": "tool", "tool_call_id": "call_1", "content": "ok"}),
            json!({"role": "user", "content": "continue"}),
        ];

        OpenAiCompatProvider::apply_fix_tool_user_sequence(&mut messages);

        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["content"], json!("Done."));
    }

    #[test]
    fn ollama_tags_match_only_the_exact_model_name() {
        let tags = json!({
            "models": [
                {"name": "qwen2.5-coder:7b"},
                {"name": "qwen2.5-coder:7b-instruct"},
            ]
        });

        assert!(ollama_tags_advertise_model(&tags, "qwen2.5-coder:7b"));
        assert!(ollama_tags_advertise_model(
            &tags,
            "qwen2.5-coder:7b-instruct"
        ));
        assert!(!ollama_tags_advertise_model(&tags, "qwen2.5-coder"));
        assert!(!ollama_tags_advertise_model(&tags, "qwen2.5-coder:latest"));
    }

    #[test]
    fn ollama_retry_host_requires_remote_normalization() {
        assert_eq!(
            ollama_tags_url("http://192.0.2.10:11434/v1").as_deref(),
            Some("http://192.0.2.10:11434/api/tags"),
        );
        assert!(ollama_tags_url("http://localhost:11434").is_none());
        assert!(ollama_tags_url("http://127.0.0.1:11434").is_none());
    }

    #[test]
    fn with_base_url_retargets_ollama_native_host() {
        // Use a remote test endpoint for Ollama's /v1 base URL and native host.
        let provider =
            OpenAiCompatProvider::new("ollama", "Ollama", "http://gpu-host.example:11434/v1")
                .with_quirks(ProviderQuirks {
                    no_api_key_required: true,
                    ollama_native_host: Some("http://gpu-host.example:11434".to_string()),
                    ..Default::default()
                });

        // Overriding the base URL with a configured remote api_base (as the
        // registry does for `providers.ollama.api_base`) must also retarget the
        // native host used by health_check() and native model discovery.
        let provider = provider.with_base_url("http://192.0.2.10:11434/v1");

        assert_eq!(provider.base_url, "http://192.0.2.10:11434/v1");
        assert_eq!(
            provider.quirks.ollama_native_host.as_deref(),
            Some("http://192.0.2.10:11434"),
        );
    }
}
