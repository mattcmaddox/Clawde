# Gateway Plan Audit

Audit of `clawde-gateway-implementation-plan.md` against OpenAI wire format
research, axum SSE best practices, and CS literature.

## Verdict

The plan is **solid** — architecture, error mapping, and shutdown semantics are
well-researched and correct. The issues below are **feature gaps** (missing
request/response fields) and **implementation details** (accumulation logic,
serde configuration), not architectural flaws.

---

## 1. Feature Gaps (Missing Wire Format Fields)

### 1a. `tool_choice` passthrough

The plan mentions `tools[]` in the request parser but omits `tool_choice`.
OpenAI supports:

```json
{
  "tool_choice": "auto" | "none" | "required" | {"type":"function","function":{"name":"..."}}
}
```

This maps directly to `ProviderRequest.tool_choice` (already in the
`LlmProvider` trait). Without it, clients that send `tool_choice: "none"` to
suppress tool calls will have that field silently dropped, causing the model to
call tools when the client didn't want them.

**Fix:** Add `tool_choice` to `parse_chat_completion_request` in §4b.

### 1b. `response_format` passthrough

OpenAI supports:

```json
{
  "response_format": {"type": "json_object"} | {"type":"json_schema","json_schema":{...}}
}
```

This is used by clients that want structured JSON output. The provider trait
may not support it yet, but the gateway should at minimum **not reject** it
(serde `deny_unknown_fields` must NOT be used) and ideally pass it through
when the provider supports it.

**Fix:** Tolerate in the parser (already planned), add a `response_format`
field to `ProviderRequest` if the trait doesn't have one, and pass through
when present.

### 1c. `seed` for reproducibility

OpenAI supports `seed: integer` for deterministic outputs. Clients like
LangChain use this for testing. The gateway should tolerate it (already
planned) and pass through when the provider supports it.

**Fix:** Tolerate in parser; document as "passed through when upstream supports
it."

### 1d. `logprobs` and `top_logprobs`

OpenAI supports `logprobs: true` and `top_logprobs: integer` for returning
token log probabilities. This is used by research clients. The gateway should
tolerate it and pass through when the provider supports it.

**Fix:** Tolerate in parser; document as "passed through when upstream supports
it."

### 1e. `reasoning_content` in non-streaming response

The plan mentions `reasoning_content` for streaming chunks (§4b) but doesn't
mention it for non-streaming responses. DeepSeek/Poolside return
`reasoning_content` in the `message` object of non-streaming responses. The
gateway should surface this.

**Fix:** Add `reasoning_content` to the non-streaming response translation in
§4b (`to_openai_response`).

### 1f. `n` (multiple choices)

OpenAI supports `n: integer` to request multiple completion choices. The
gateway should tolerate it but can reject with `400` if the upstream doesn't
support it, or map it to a single choice with a warning.

**Fix:** Document as "v1: rejects `n > 1` with 400; future: map to multiple
upstream calls."

---

## 2. Implementation Details

### 2a. Tool-call argument accumulation (streaming)

The plan says "tool-call argument fragments accumulate into
`delta.tool_calls[].function.arguments`" (§4b). This is correct but the
accumulation logic needs care:

- **First chunk** with a tool call: carries `delta.tool_calls[].index`,
  `delta.tool_calls[].id`, `delta.tool_calls[].function.name`,
  `delta.tool_calls[].function.arguments` (may be empty string or partial).
- **Subsequent chunks**: carry `delta.tool_calls[].index` and
  `delta.tool_calls[].function.arguments` (the delta).
- The gateway must **accumulate** `arguments` strings across chunks for the
  same `index`, not replace them.

This is the standard OpenAI streaming tool-call pattern. The plan should
explicitly state the accumulation semantics.

### 2b. Serde configuration for request parsing

The plan says "tolerates unknown fields" (§4b). This means the request struct
must use `#[serde(deny_unknown_fields)]` is NOT set (default serde behavior
tolerates unknown fields). But the plan should also mention:

- Use `#[serde(default)]` on optional fields to handle missing values.
- Use `#[serde(rename_all = "snake_case")]` for field naming.
- Consider `#[serde(skip_serializing_if = "Option::is_none")]` for response
  structs to avoid emitting `null` values.

### 2c. `stream_options.include_usage` handling

The plan mentions this (§4b) but the deduplication logic needs clarification.
When `stream_options.include_usage` is true:

1. The upstream may already send a final usage-only chunk (some providers do).
2. The gateway should emit usage in the terminal chunk regardless.
3. If the upstream already sent usage, the gateway should **not** emit a
   duplicate.

The plan should state: "If the upstream's terminal chunk already contains
`usage`, emit it there. If not, emit a separate usage-only chunk before
`[DONE]`."

### 2d. `finish_reason` on terminal chunk

The plan says "terminal chunk carries `finish_reason`" (§4b). This is correct.
But some providers emit `finish_reason: null` on non-terminal chunks and only
set it on the final chunk. The gateway should:

- Emit `finish_reason: null` on all non-terminal chunks.
- Emit the actual `finish_reason` only on the terminal chunk.
- Map upstream finish reasons to OpenAI's enum: `stop`, `length`,
  `tool_calls`, `content_filter`.

---

## 3. Security Considerations

### 3a. Key comparison

The plan says "constant-time comparison" (§4c) for bearer key validation. This
is correct and important — use `subtle::ConstantTimeEq` or equivalent to
prevent timing attacks.

### 3b. Error message sanitization

The plan says "Never leak upstream API keys or raw URLs" (§4e). This is
correct. The error mapper should:

- Strip `Authorization` headers from upstream error responses.
- Strip API keys from URLs in error messages.
- Log status codes + route only, never request/response bodies.

### 3c. CORS configuration

The plan doesn't mention CORS. If the gateway is accessed from a browser
(e.g., Open WebUI), CORS headers are needed. For v1 (localhost only), CORS
isn't critical, but the plan should document:

- Default: no CORS headers (localhost-only, no browser access expected).
- Optional: `tower-http` CORS middleware with configurable origins.

---

## 4. Rate Limiting Refinements

### 4a. Token bucket initialization

The plan says "fixed key table from config" (§4c). This means keys are known
at startup. But the bucket state should be initialized lazily on first use,
not pre-allocated for all configured keys (some keys may never be used).

### 4b. TPM counting accuracy

The plan says "TPM: tokens per minute, refilled from `usage` on every
response/stream completion" (§4c). For streaming responses, the `usage` is
only available at the end. This means:

- During streaming, the gateway can't enforce TPM (it doesn't know how many
  tokens will be consumed).
- The gateway should enforce RPM during streaming and TPM after completion.
- If the stream is aborted, the gateway should estimate TPM from the
  `usage` field if available, or from the chunk count otherwise.

### 4c. 429 response headers

The plan mentions `Retry-After` and `X-RateLimit-*` headers (§4c). The
OpenAI standard headers are:

- `X-RateLimit-Limit-Requests`: max requests per window
- `X-RateLimit-Remaining-Requests`: remaining requests
- `X-RateLimit-Reset-Requests`: time until reset (ISO 8601 or seconds)
- `X-RateLimit-Limit-Tokens`: max tokens per window
- `X-RateLimit-Remaining-Tokens`: remaining tokens
- `X-RateLimit-Reset-Tokens`: time until reset

The gateway should emit all of these for both RPM and TPM limits.

---

## 5. Shutdown Semantics

### 5a. Active-stream counter

The plan says `Arc<AtomicUsize>` for the counter (§4g). This is correct. But
the counter should be incremented **before** the handler starts processing
and decremented in a `Drop` guard or `finally` block, not at the end of the
handler (which could miss panics).

### 5b. `/healthz` during drain

The plan says `/healthz` returns `503` during drain (§4g). This is the
Kubernetes readiness-drain pattern and is correct. But the plan should also
mention:

- `/healthz` should return `200` with `{"status":"draining"}` during the
  grace period (so load balancers know to stop sending new traffic but
  existing connections are still being served).
- After grace expiry, `/healthz` should return `503` with
  `{"status":"shutting_down"}`.

### 5c. Client disconnect detection

The plan says "per-request token is cancelled when the SSE body is dropped"
(§4g). In axum, when the client disconnects, the response body's `poll_next`
returns `None` and the task is dropped. The `CancellationToken` should be
cancelled in the `Drop` impl of a guard struct, not in the stream's `poll_next`
(since `poll_next` may not be called after disconnect).

---

## 6. Testing Strategy

### 6a. Missing test cases

The plan mentions "golden SSE transcripts" (§8) but should also include:

- **Tool-call streaming**: verify argument accumulation across chunks.
- **Mixed content + tool calls**: verify interleaved text and tool-call
  deltas.
- **Reasoning passthrough**: verify `reasoning_content` appears in both
  streaming and non-streaming responses.
- **Error propagation**: verify upstream 429/500/timeout maps to correct
  OpenAI error envelope.
- **Rate limiting**: verify RPM/TPM enforcement and 429 headers.
- **Graceful shutdown**: verify drain with active SSE connections.
- **Client disconnect**: verify upstream stream is aborted when client
  disconnects.

### 6b. Fixture format

The plan mentions "golden request/response/chunk transcripts" (§4b). The
fixture format should be:

```json
{
  "request": { ... OpenAI request body ... },
  "upstream_response": { ... ProviderResponse ... },
  "expected_openai_response": { ... OpenAI response body ... },
  "expected_stream_chunks": [ ... array of SSE events ... ]
}
```

This allows testing both streaming and non-streaming translation against the
same fixture.

---

## 7. Recommendations

1. **Add `tool_choice`, `response_format`, `seed`, `logprobs` to the request
   parser** — these are standard OpenAI fields that clients expect.
2. **Add `reasoning_content` to non-streaming responses** — DeepSeek/Poolside
   return this.
3. **Explicitly document tool-call argument accumulation** in the streaming
   translation.
4. **Add CORS configuration** as an optional middleware.
5. **Add test cases** for tool-call streaming, reasoning passthrough, and
   error propagation.
6. **Use `Drop` guard** for the active-stream counter to handle panics.
7. **Emit full `X-RateLimit-*` headers** for both RPM and TPM limits.

---

## 8. Research References

- **OpenAI API Reference** — Chat Completions streaming events and create
  endpoint (verified against live docs).
- **axum SSE backpressure** — bounded mpsc approach confirmed as correct
  (Rust forum discussion, Aug 2025).
- **hyper#2787** — `with_graceful_shutdown` never terminates while SSE is
  active (plan correctly identifies and mitigates).
- **LiteLLM virtual keys** — two-dimensional RPM/TPM rate limiting pattern
  (plan correctly adopts).
- **FrugalGPT (Stanford, 2023)** — cascade routing pattern (plan correctly
  notes FreeProvider already implements this).
