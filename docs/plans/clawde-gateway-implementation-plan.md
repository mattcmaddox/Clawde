# Clawde Gateway Implementation Plan

> **Superseded.** This was the relay-only gateway plan (chat completions
> proxy). The agent-capable design replaced it — see
> [clawde-agent-gateway-plan.md](clawde-agent-gateway-plan.md); the shipped
> surface is documented in [docs/gateway.md](../../gateway.md).

## 1. Goals

- Expose an OpenAI-compatible HTTP API (`POST /v1/chat/completions`,
  `GET /v1/models`) that routes requests through Clawde's provider registry,
  giving the free-tier composite (`FreeProvider`) and key rotation the same
  surface any OpenAI SDK client expects (openai-python, LangChain, Cursor,
  aider, Open WebUI).
- Support streaming (SSE) and non-streaming requests with strict OpenAI wire
  fidelity — clients are picky about chunk shape, not feature breadth.
- Reuse the existing provider machinery unchanged: `ProviderRegistry`,
  `FreeProvider` fallback, `KeyRotatingProvider` rotation, KeyRing cooldowns.
- Keep the gateway a separate process from the TUI so the TUI repaint cadence
  and idle-CPU guarantees are untouched.
- Auth, rate limiting, and error envelopes consistent with OpenAI's API so
  drop-in client config (`base_url` override) just works.

Scope guardrail: the gateway proxies **chat completions only**. It does not
run the agent loop, execute tools, manage sessions, or expose the TUI.

## 2. Current Architecture (verified against the code)

- **Provider registry** — `crates/api/src/registry.rs`. `ProviderRegistry`
  holds `HashMap<ProviderId, Arc<dyn LlmProvider>>`; default provider is
  `free`. `rebuild_free(&mut self, config)` rebuilds the composite in place at
  runtime (this is the refresh hook the gateway needs — see §7 risk R3).
- **`LlmProvider` trait** — `crates/api/src/provider.rs`. The seam the
  gateway drives:
  - `async fn create_message(&self, request: ProviderRequest) -> Result<ProviderResponse, ProviderError>` (non-streaming)
  - `async fn create_message_stream(&self, request: ProviderRequest) -> Result<Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>, ProviderError>`
  - `async fn discover_models(&self) -> Result<Vec<ModelInfo>, ProviderError>`
  - `id()`, `name()`, `key_ring_status()`
- **FreeProvider** — `crates/api/src/providers/free/`. A composite stacking
  the free catalog in priority order. All mutable state (`cooldown`,
  `latencies`, `capacity`) is `Arc<Mutex<>>`; trait methods take `&self`, so a
  single `Arc<dyn LlmProvider>` is safe to share across concurrent requests —
  no per-request construction needed. `resolve_route(&self, model)` maps model
  strings to `Auto` / `Pinned { start_idx, pinned_model }` / family routes.
- **Free catalog** (order = fallback priority; `FREE_CATALOG` in
  `crates/api/src/providers/free/catalog.rs`): GitHub Copilot, Poolside,
  NVIDIA NIM, Cerebras, Google Gemini, Cloudflare, Groq, SambaNova, Cline,
  Mistral, OpenCode Zen, Z.AI, OpenRouter.
- **Key rotation** — `KeyRotatingProvider` wraps any upstream with 2+ keys;
  exhaustion/cooldown state lives in the `KeyRing` and is exposed via
  `key_ring_status()` / `ProviderRegistry::key_ring_summaries()`.
- **Existing servers, not greenfield**: the CLI already ships two server modes
  — an **ACP server** (`clawde acp`: stdio or TCP with TLS, `--allow-non-loopback`
  opt-in, `CancellationToken`-based graceful shutdown) and **MCP server**
  support. The gateway reuses these patterns: config shape, subcommand
  dispatch, cancel wiring, loopback-only default. HTTP itself is new to the
  repo.

## 3. Proposed Architecture

```
  OpenAI-compatible client (openai-python / curl / Cursor / aider)
                    |
                    v
  +---------------------------------------------+
  |  crates/gateway  (axum HTTP server)         |
  |  POST /v1/chat/completions   GET /v1/models |
  |  GET /v1/models/{id}         GET /healthz   |
  |  +----------+ +---------+ +----------------+|
  |  | auth.rs  | | translate.rs |  error.rs   ||
  |  | bearer + | | OpenAI <->   |  ProviderErr ||
  |  | RPM/TPM  | | ProviderReq  |  -> envelope  ||
  |  | limits   | +---------+ +----------------+|
  |  +----+-----+ +---------+ +--------+-------+
  |       |  shutdown.rs (drain + cancel)       |
  +----------------------+----------------------+
                         | create_message(_stream)
                         v
  +---------------------------------------------+
  |  crates/api  ProviderRegistry (shared lib)  |
  |  FreeProvider / KeyRotatingProvider /       |
  |  single providers (openai, google, ollama…) |
  +----------------------+----------------------+
                         |
                         v
             Upstream LLM providers
```

### Key design decisions

1. **New library crate `crates/gateway` + `clawde serve` subcommand.** HTTP
   concerns stay out of `tui`/`cli` core paths. A thin bin target
   (`clawde-gateway`) is optional; the primary entry point is
   `clawde serve [--port N]`, mirroring how `clawde acp` is dispatched today.
2. **Dependency decision (explicit):** axum is the first HTTP-server dependency
   in the workspace. `subtle` (constant-time comparison for bearer keys) and
   `tower-http` (cors) are the only other new runtime deps; everything else
   is already in the workspace. (Cargo.lock has no axum today; only `hyper` transitively
   via reqwest). Acceptable for a purpose-built server crate: axum is the
   standard tokio-native choice and keeps handler code minimal. Keep the new
   tree small: `axum`, `tokio` (already present), `serde`/`serde_json`
   (present), and `tower-http` with the `cors` feature only. **Do not** add
   `tower_governor` — hand-roll the rate limiter (§5c) to avoid a second new
   framework dep.
3. **Reuse, do not duplicate wire types.** The gateway translates the OpenAI
   payload into `ProviderRequest` (crates/api) and `StreamEvent` back into
   OpenAI chunks. Where a shared serde struct is needed by both the gateway
   and `openai_compat_providers.rs`, factor it into `crates/api` rather than
   defining a parallel copy.
4. **Routing via existing machinery.** Model strings map straight onto
   `FreeProvider::resolve_route`:
   - `free/auto` / `auto` / `free` → Auto fallback chain (a FrugalGPT-style
     cascade — see §6)
   - `free/family/<slug>` → model-first family route (round-robin across hosts)
   - `<upstream>/<model>` (e.g. `groq/gpt-oss-120b`, `nvidia/openai/gpt-oss-120b`) → pinned dispatch
   - unknown / bare OpenAI-ish names (`gpt-4o`) → configurable alias table, else `400 invalid_request_error`
5. **Streaming is the default path.** SSE is first-class; non-streaming calls
   `create_message` directly (the trait has a dedicated non-streaming method —
   no stream collection).
6. **Effort/thinking passthrough.** The gateway maps an optional
   `reasoning_effort` / `effort` body field to `ProviderRequest.effort_level`.
   Per-upstream thinking shaping is already handled inside FreeProvider
   (`shape_thinking_for_upstream`) and in the query layer for direct
   providers — the gateway must not re-shape, only pass the level through.
7. **Build once, refresh on a timer.** The registry is built once at startup
   via the same code path as the CLI, then re-`rebuild_free` on the discovery
   cache TTL (6 h) so a long-running process does not serve stale model lists
   (§7 risk R3).

## 4. Components to Create

### a. `crates/gateway` crate

- Workspace member depending on `clawde-api`, `clawde-core`, `axum`, `tokio`,
  `serde`, `serde_json`, `tower-http` (cors).
- Routes: `POST /v1/chat/completions`, `GET /v1/models`,
  `GET /v1/models/{id}`, `GET /healthz`, `GET /status` (ring/cooldown, auth-gated).

### b. `src/translate.rs` — wire translation

- `parse_chat_completion_request(Value) -> Result<ProviderRequest, GatewayError>`:
  accepts `model`, `messages[]` (`role`/`content`/`tool_calls`/`tool_call_id`),
  `tools[]`, `tool_choice`, `temperature`, `max_tokens`, `stream`, `stop`,
  `reasoning_effort`, `response_format`, `seed`, `logprobs`, `top_logprobs`;
  **tolerates unknown fields** (OpenAI clients send `stream_options`, `user`,
  `n`, …) — serde must NOT set `deny_unknown_fields`. Reject only
  structurally invalid bodies (`400 invalid_request_error`).
  - `tool_choice` (`auto`/`none`/`required`/`{type,function}`) maps to
    `ProviderRequest.tool_choice` so `tool_choice: "none"` suppresses tool
    calls instead of being silently dropped (audit §1a).
  - `response_format` / `seed` / `logprobs` / `top_logprobs` are tolerated
    and passed through when the upstream supports them; documented as such
    (audit §1b–1d).
  - `n > 1` is rejected with `400` in v1 (upstreams are single-choice);
    documented as a future multi-call mapping (audit §1f).
- `to_openai_response(ProviderResponse) -> Value`: `id`, `object:
  "chat.completion"`, `created`, `model`, `choices[0].message`,
  `usage`. Provider-side thinking (`reasoning_content`) maps to
  `message.reasoning_content` in BOTH streaming and non-streaming responses
  (matches poolside/deepseek wire behavior; audit §1e).
- Tool-call argument accumulation (streaming): the first chunk with a tool
  call carries `delta.tool_calls[].index` + `id` + `function.name` +
  `function.arguments` (possibly empty); subsequent chunks carry `index` +
  `function.arguments` deltas. The gateway ACCUMULATES `arguments` strings
  across chunks for the same `index` — never replaces (audit §2a).
- `to_openai_stream_event(StreamEvent) -> Option<Chunk>`: the event stream is
  Anthropic-shaped (`MessageStart`, `ContentBlockStart`, `TextDelta`,
  `ThinkingDelta`, tool-call deltas, `MessageStop`) — accumulate into OpenAI
  `chat.completion.chunk` frames:
  - first chunk carries `delta.role: "assistant"` + `choices[0].index: 0`
  - text deltas → `delta.content`; thinking deltas → `delta.reasoning_content`
  - tool-call argument fragments accumulate into `delta.tool_calls[].function.arguments`
  - terminal chunk carries `finish_reason` (`stop` | `tool_calls` | `length`)
  - `[DONE]` terminator; with `stream_options.include_usage`, a final
    usage-only chunk before `[DONE]`. Dedup rule: if the upstream's terminal
    chunk already carries `usage`, emit it there; otherwise emit a separate
    usage-only chunk (audit §2c).
  - `finish_reason`: `null` on all non-terminal chunks; the actual reason
    (`stop` | `length` | `tool_calls` | `content_filter`) only on the
    terminal chunk (audit §2d).
- Fixtures: golden request/response/chunk transcripts in
  `crates/gateway/tests/fixtures/`, plus a raw-socket and curl smoke test.

### c. `src/auth.rs` — authentication & rate limiting

- Bearer key from `Authorization` header, validated against
  `GatewayConfig.allowed_keys` (settings) or `CLAWDE_GATEWAY_KEY` (env).
  Constant-time comparison via `subtle::ConstantTimeEq` (audit §3a).
- **Two-dimensional token bucket per key** (hand-rolled, `Mutex<HashMap<Key,
  Bucket>>` behind a small axum middleware), following the LiteLLM "virtual
  key" model:
  - **RPM**: requests per minute (token bucket — the strong general-purpose
    default per the rate-limiting literature, §6d; allows controlled bursts).
  - **TPM**: tokens per minute, refilled from `usage` on every response/stream
    completion. Without this, a caller can burn a free-tier upstream's daily
    token quota through the gateway even while staying under the request cap.
  - Optional **daily spend cap** in USD/tokens per key, also counted from
    `usage` — the free-tier user story is "don't silently exhaust my free
    quota", so surface remaining budget in the 429/`X-RateLimit-*` headers.
  - Fixed key table from config; unknown keys rejected before any state
    allocation.
- `429` responses carry OpenAI-style bodies, `Retry-After`, and the full
  OpenAI `X-RateLimit-*` header set for BOTH dimensions (audit §4c):
  `X-RateLimit-Limit-Requests`, `X-RateLimit-Remaining-Requests`,
  `X-RateLimit-Reset-Requests`, `X-RateLimit-Limit-Tokens`,
  `X-RateLimit-Remaining-Tokens`, `X-RateLimit-Reset-Tokens`.
- Bucket state is allocated lazily on first use per key, not pre-allocated
  for every configured key at startup (audit §4a). TPM is enforced after
  stream completion (usage is only known then); a stream aborted mid-flight
  estimates from `usage` if present, else from chunk count (audit §4b).
- Distributed rate limiting (Redis) is **explicitly out of scope for v1** —
  the gateway is a single localhost process; if multi-instance deployment ever
  appears, the per-key state moves behind a shared counter (see §6d for the
  algorithm trade-offs).
- `GET /v1/models` and `/status` require the same key (they reveal key-health
  and cooldown state); `GET /healthz` is unauthenticated liveness.

### d. `src/router.rs` — provider integration

- Build `ProviderRegistry` once at startup: register every configured
  non-free provider (same path the CLI uses) + `build_free_provider(config)`.
- `resolve(model) -> (Arc<dyn LlmProvider>, wire_model)`: via
  `FreeProvider::resolve_route` for free/auto/family/pinned ids; direct
  providers for `<provider>/<model>` where provider is not a free upstream;
  alias table last.
- Forward `req.model` set to the resolved wire model; let the provider handle
  the rest. Do NOT re-implement fallback — FreeProvider already walks the
  chain and reports per-upstream errors.
- **Per-upstream in-flight cap**: a `tokio::sync::Semaphore` per upstream
  (max concurrent requests) protects against a burst of client requests
  hammering one upstream while its cooldowns are still hot. The repo already
  contains a Netflix-gradient `AdaptiveConcurrency` implementation
  (`free/mod.rs`, currently dormant); the fixed semaphore is the v1 default,
  with a note that AdaptiveConcurrency can replace it later without API
  changes.

### e. `src/error.rs` — error envelope

`ProviderError` → OpenAI `{ "error": { "message", "type", "param", "code" } }`:

| ProviderError | HTTP | error.type |
|---|---|---|
| `AuthFailed` | 401 | `authentication_error` |
| `RateLimited` / `QuotaExceeded` | 429 | `rate_limit_error` (+ `Retry-After` when known) |
| `InvalidRequest` / `MalformedRequest` / `ContentFiltered` | 400 | `invalid_request_error` |
| `ContextOverflow` | 400 | `invalid_request_error` (message includes max context) |
| `ModelNotFound` | 404 | `model_not_found` |
| `Other { status }` | passthrough status | `api_error` |
| Chain exhaustion (all upstreams failed before first byte) | 503 | `service_unavailable` (message lists attempted upstream ids, never keys) |

Never leak upstream API keys or raw URLs. Log status codes + route only.

### f. `src/config.rs` + CLI wiring

- `GatewayConfig` mirrors the existing `AcpServerConfig` pattern in
  `crates/core` settings: `enabled`, `listen` (default `127.0.0.1:8787`),
  `allow_non_loopback` (opt-in, default false), `tls_cert_path` /
  `tls_key_path` (reuse the ACP TLS setup), `allowed_keys`,
  `rate_limits` (rpm/tpm/spend per key), `max_in_flight_per_upstream`,
  `request_timeout_secs`, `discovery_refresh_secs`, `shutdown_grace_secs`.
- `clawde serve [--port N] [--allow-non-loopback] [--tls-cert …] [--tls-key …]`
  — same dispatch style as `clawde acp` (main.rs ACP_USAGE pattern), plus a
  standalone `clawde-gateway` bin target.

### g. `src/shutdown.rs` — graceful shutdown with SSE

Naive `axum::serve(...).with_graceful_shutdown(signal)` **never stops while an
SSE stream is active** (hyper#2787) — the upstream stream task keeps the
future alive. The gateway therefore tracks active streams explicitly:

- `Arc<AtomicUsize>` active-stream counter; each handler increments on start,
  decrements in a `Drop` guard (not at the end of the handler, so panics
  can't leak the count; audit §5a).
- On SIGINT/SIGTERM (same `CancellationToken` wiring as `clawde acp`):
  1. stop accepting new requests; `/healthz` returns `200` with
     `{"status":"draining"}` during the grace period (load balancers stop
     sending new traffic), then `503` with `{"status":"shutting_down"}`
     after grace expiry (audit §5b),
  2. wait up to `shutdown_grace_secs` for the counter to reach zero
     (finishing in-flight completions), then
  3. abort remaining streams via their per-request `CancellationToken`s and
     exit.
- Client disconnect: the per-request token is cancelled when the SSE body is
  dropped, aborting the upstream stream task (see risk R6). This also covers
  the hyper#2787 trap — a disconnected-but-not-drained stream cannot pin the
  shutdown future.

## 5. Implementation Steps

1. **Scaffold** `crates/gateway` (lib + bin), workspace member; axum hello on
   8787; `cargo check --workspace` clean.
2. **Non-streaming** `/v1/chat/completions`: parse → resolve → 
   `provider.create_message(req)` → OpenAI JSON. Unit-test translation
   against fixture bodies.
3. **`GET /v1/models`**: synthetic free-catalog entries (`free/auto`,
   `free/family/<slug>`, per-upstream discovered lists from
   `take_free_model_lists`) + registered direct providers, shaped as
   `{ "object": "list", "data": [...] }`.
4. **Streaming**: `create_message_stream` → SSE chunks; verify with `curl -N`
   and a scripted client; golden transcript test; slow-client test.
5. **Auth + rate limiting**: bearer validation, constant-time compare,
   RPM/TPM token buckets, spend counter, 401/429 paths.
6. **Error mapping**: central `IntoResponse` for all gateway errors.
7. **Fallback integration test**: reuse the `common::mock_provider` harness
   (`ScriptedResponse` 429/500 upstreams, as in `free_recovery.rs`) and assert
   the chain walk and final status/body.
8. **Graceful shutdown**: signal handling, drain with SSE counter, abort on
   grace expiry; test with a hung mock upstream.
9. **CLI wiring**: `clawde serve` + `GatewayConfig` plumbing + TLS.
10. **Docs**: `docs/gateway.md` (endpoints, auth, curl + openai-python
    `base_url` examples), README section, cross-link ACP/MCP server docs.
11. **Hygiene**: `cargo test --workspace`, clippy `-D warnings`, fmt;
    idle-CPU probe unaffected (separate process).

## 6. Research Notes & Prior Art

### a. LLM gateways (the product category)

- **LiteLLM proxy** (Python, self-hosted) — the canonical OpenAI-compatible
  facade. Its "virtual keys" model is the strongest pattern for this plan:
  each caller gets a key with its own **rpm/tpm budgets, per-model budgets,
  and spend tracking**; the router layer owns load balancing, fallbacks, and
  retries; cooldown groups fall back to a specific model once every member of
  a group is cooling down. Lessons adopted: two-dimensional RPM/TPM limits and
  a spend counter (§5c); the router must own failover — which Clawde's
  FreeProvider already does, so the gateway stays a thin adapter.
- **Portkey / Helicone** — gateway + observability. We deliberately take only
  the minimal observability slice (request logs with route/status/tokens and a
  `/status` surface) rather than shipping a dashboard.
- **one-api / new-api** — self-hosted multi-provider gateways popular in CN
  deployments; confirm the virtual-key + quota pattern at scale. No direct
  lessons beyond LiteLLM.

### b. LLM routing & cascade literature

- **FrugalGPT (Stanford, 2023)** — LLM *cascades*: try the cheapest/weakest
  model first, escalate only when its answer is low-confidence; reports up to
  ~98% cost reduction while matching the best single model. **FreeProvider's
  `free/auto` chain is exactly a cascade** — ordered upstreams with
  fall-through on failure. The gateway inherits this for free; the plan makes
  no change beyond documenting it.
- **RouteLLM (2024)** — learns a router (preference data) to send easy
  queries to a weak model and hard ones to a strong one (~85% cost savings at
  95% quality). Clawde's `task_classifier` in FreeProvider is the rule-based
  cousin of this (task → preferred upstream order). The gateway should not
  add learned routing; the existing classifier already runs per request
  inside the Auto strategy.
- **Dynamic Model Routing survey (arXiv 2603.04445)** — confirms cascades and
  routed ensembles are the two dominant strategies; reinforces keeping routing
  policy data-driven (`FREE_CATALOG` + config), never hardcoded per provider.

### c. Distributed-systems classics already embodied in the codebase

The repo's FreeProvider was built from these; the gateway inherits them and
must not re-implement them:

- **"The Tail at Scale" (Dean & Barroso, Google 2013)** — hedged requests:
  fire a backup request to another replica when the first is slow. Already
  implemented in `RetryingFreeStream` (`start_hedge_request`, on by default).
- **Power of Two Choices (Mitzenmacher)** — already present as a routing
  profile (`P2CConfig` in `free/mod.rs`).
- **Netflix adaptive concurrency (gradient-based limits)** — already present
  as `AdaptiveConcurrency` (`free/mod.rs`, dormant). This plan's §5d semaphore
  is the v1 stand-in; AdaptiveConcurrency is the documented upgrade path.
- **Circuit breaker (Fowler / Hystrix)** — already present as
  `CooldownState` (5xx + empty-completion cooldowns, persisted across
  processes). The gateway just surfaces it via `/status`.

### d. Rate-limiting algorithms

- Token bucket is the **strong general-purpose default** for APIs (Arcjet
  survey): it allows bounded bursts and is O(1) per request with per-client
  state. Single-process → plain in-memory buckets are correct and simple.
- **Kong** recommends sliding-window counters when the limiter must scale
  across nodes (approximate, low memory). **arXiv 2602.11741** formalizes the
  accuracy/availability/scalability trade-off for distributed limiters.
- Decision: v1 = in-memory token buckets (RPM + TPM), single process. If
  multi-instance ever lands, move the bucket state behind a Redis-backed
  counter (sliding window) — a config swap, not a redesign, because the
  middleware boundary stays the same.

### e. Streaming & shutdown engineering

- **hyper#2787**: `with_graceful_shutdown` alone never terminates while an SSE
  connection is open — the direct motivation for §4g's explicit drain/abort.
- **Kubernetes drain pattern**: readiness endpoint returns 503 once shutdown
  begins, then a grace period, then force-close — adopted verbatim in §4g.
- **axum SSE backpressure**: there is no built-in backpressure; the tokio
  stream backpressures naturally when the SSE writer's channel is bounded.
  Plan: a bounded `mpsc` between the upstream stream task and the SSE writer;
  a slow client stalls the channel rather than buffering unboundedly, and the
  per-request inactivity timeout aborts the connection (which cancels the
  upstream task via the drop hook). `axum-socket-backpressure` exists for
  per-connection monitoring if ever needed — not a v1 dependency.
- **Twelve-Factor config**: every gateway knob from env/settings file, never
  compiled-in defaults beyond safe ones (loopback bind).

## 7. Risks and Mitigations

| # | Risk | Mitigation |
|---|---|---|
| R1 | Breaking TUI/CLI behavior | New crate; zero changes to `tui`. Registry and trait boundary are untouched — the gateway is a second adapter on the same core, exactly like the ACP server. |
| R2 | Streaming fidelity rejects by clients | Golden SSE transcripts against openai-python + curl; strict chunk-shape tests; `stream_options.include_usage` final-usage chunk; `finish_reason` on the terminal chunk. |
| R3 | **Stale discovery in a long-running process** (new) | Registry built once, then `rebuild_free` on `discovery_refresh_secs` (default 6 h, matching the discovery cache TTL) and on `SIGHUP`/`POST /status/refresh`. Model lists, defaults, and configured keys stay current without a process restart. |
| R4 | Concurrent access to KeyRing/AuthStore | KeyRing is `Arc<Mutex<>>` with lock-never-across-await discipline; FreeProvider state is all `Arc<Mutex<>>`. Gateway handlers must hold no lock across `.await`. Cross-process persistence is already safe (file-lock + atomic writes in `free/mod.rs`). |
| R5 | Upstream key leakage | Central error mapper strips credentials; `allowed_keys` are the only keys the gateway accepts; upstream keys never appear in responses or logs. |
| R6 | Client disconnect hangs the upstream | Each request gets a `CancellationToken` (pattern already used by ACP); on disconnect, abort the upstream stream task and drop the SSE body. |
| R7 | Rate-limit state growth | Fixed key table from config; unknown keys rejected before allocation. |
| R8 | Non-loopback exposure | Default bind `127.0.0.1`; `--allow-non-loopback` required to bind elsewhere (same posture as ACP); TLS optional via `--tls-cert`/`--tls-key`. |
| R9 | New dependency surface (axum) | Confined to `crates/gateway`; the crate is purpose-built as an HTTP server, so a server framework is unavoidable. No new deps in `api`/`core`/`tui`. |
| R10 | **SSE streams pin graceful shutdown** (hyper#2787) | §4g: explicit active-stream counter, drain window, abort-on-grace-expiry; readiness 503 during drain. Tested with a hung mock upstream. |
| R11 | **Slow clients / unbounded buffering** | Bounded mpsc between upstream stream and SSE writer; per-request inactivity timeout aborts (cancelling the upstream task). No unbounded buffers anywhere in the path. |
| R12 | **Bursts hammering a hot upstream** | Per-upstream `Semaphore` in-flight cap (§5d); cooldown state already prevents fallback loops. AdaptiveConcurrency is the documented upgrade. |

## 8. Deliverables

1. `crates/gateway` (library + `clawde-gateway` bin) and `clawde serve` subcommand.
2. Endpoints: `POST /v1/chat/completions` (stream + non-stream),
   `GET /v1/models`, `GET /v1/models/{id}`, `GET /healthz`, `GET /status`.
3. Translation layer with fixture-tested OpenAI wire compatibility (requests,
   responses, SSE chunks, tool calls, reasoning passthrough).
4. Bearer auth + per-key RPM/TPM rate limiting + spend counter (hand-rolled
   token buckets).
5. Full fallback/key-rotation behavior inherited from
   `FreeProvider`/`KeyRotatingProvider` — no re-implementation.
6. Graceful shutdown with SSE drain + readiness semantics.
7. OpenAI-compatible error envelope on every failure path.
8. `docs/gateway.md` + README entry, cross-linked with ACP/MCP docs.
9. Tests: unit (translation), integration (mock-upstream fallback walk,
   hung-stream shutdown), smoke (`curl` script), golden SSE transcripts.

## 9. Open Questions

1. Alias table scope: ship a small built-in map (`gpt-4o` → copilot's
   `gpt-4o-2024-11-20`, `claude-sonnet` → anthropic) or require explicit
   config? Default: explicit config, empty built-in table.
2. Should `/v1/models` include the discovered per-upstream free lists (can be
   long) or only the defaults? Default: defaults + `free/auto` + family slugs;
   full lists behind `?detail=full`.
3. Per-request upstream key override (user supplies their own `Authorization`
   to be forwarded) — explicitly out of scope v1; revisit if demand appears.
4. TPM/spend budget defaults: per-key defaults (e.g. 60 RPM / 100K TPM / no
   spend cap) vs. require explicit config? Default: sensible built-ins,
   overridable per key.
5. In-flight cap default per upstream (e.g. 8) — pick from observed free-tier
   concurrency behavior, or expose `AdaptiveConcurrency` from day one?
6. CORS: default no CORS headers (localhost-only, no browser access expected);
   optional `tower-http` CORS middleware with configurable origins for
   browser clients (Open WebUI). Ship the middleware behind a config flag
   (audit §3c).
7. TPM counting during streaming: enforce RPM only mid-stream (usage unknown),
   TPM after completion; document the abort-estimation fallback (audit §4b).
8. Test cases to add beyond golden transcripts: tool-call argument
   accumulation, mixed content + tool calls, reasoning passthrough (stream +
   non-stream), upstream 429/500/timeout mapping, RPM/TPM enforcement +
   429 headers, drain with active SSE, client-disconnect abort (audit §6a).
