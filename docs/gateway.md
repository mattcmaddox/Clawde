# Clawde Gateway

Clawde ships an OpenAI-compatible HTTP gateway that routes chat completion
requests through the same provider registry the TUI uses — the free-tier
composite (`FreeProvider`), key rotation, and cooldowns included. Any
OpenAI SDK client (openai-python, LangChain, Cursor, aider, Open WebUI) can
point its `base_url` at the gateway and get the free-catalog fallback chain
for free.

## Scope

The gateway proxies **chat completions only** (`POST /v1/chat/completions`).
It does not run the agent loop, execute tools, manage sessions, or expose the
TUI.

## Quick start

```bash
# Start with a bearer key (loopback only by default)
CLAWDE_GATEWAY_KEY=my-secret-key clawde serve --port 8787

# Or add keys to settings.json under "gateway": { "allowedKeys": [...] }
```

Then point a client at it:

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer my-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "free/auto",
    "messages": [{"role": "user", "content": "Hello!"}]
  }'
```

Streaming works the same way with `"stream": true` (SSE).

### openai-python

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://127.0.0.1:8787/v1",
    api_key="my-secret-key",
)

resp = client.chat.completions.create(
    model="free/auto",
    messages=[{"role": "user", "content": "Hello!"}],
)
print(resp.choices[0].message.content)
```

## Endpoints

| Endpoint | Auth | Description |
|---|---|---|
| `POST /v1/chat/completions` | Bearer | Chat completions (stream + non-stream) |
| `GET /v1/models` | Bearer | List models |
| `GET /v1/models/{id}` | Bearer | Get a single model |
| `GET /healthz` | none | Liveness (503 while draining) |
| `GET /status` | Bearer | Key-ring/cooldown status |

## Model routing

Model strings map onto Clawde's existing routing:

- `free/auto` (or `auto` / `free`) — the full fallback chain in catalog
  priority order (a FrugalGPT-style cascade).
- `free/<upstream>` (e.g. `free/groq`) — pin to one upstream family.
- `<upstream>/<model>` (e.g. `groq/gpt-oss-120b`) — pinned dispatch to a
  specific upstream + model.
- `<provider>/<model>` for direct providers (e.g. `anthropic/claude-haiku-4-5`)
  when that provider has credentials.
- Unknown names → `404 model_not_found`.

## Authentication

Every endpoint except `/healthz` requires `Authorization: Bearer <key>`.
Accepted keys come from:

1. `CLAWDE_GATEWAY_KEY` env var
2. `--key <KEY>` CLI flag
3. `gateway.allowedKeys` in `settings.json`

Keys are compared in constant time. Unknown keys are rejected before any
rate-limit state is allocated.

## Rate limiting

Per-key two-dimensional token buckets (hand-rolled):

- **RPM** — requests per minute (default 60).
- **TPM** — tokens per minute (default 100 000), enforced on the request
  estimate up-front and the actual usage on completion.

Exhausted budgets return `429 rate_limit_error` with `Retry-After` and
`X-RateLimit-*` headers. Configure via `gateway.rpm` / `gateway.tpm` in
`settings.json`.

## Configuration

All knobs live under `"gateway"` in `~/.clawde/settings.json`:

```json
{
  "gateway": {
    "enabled": false,
    "listen": "127.0.0.1:8787",
    "allowNonLoopback": false,
    "tlsCertPath": null,
    "tlsKeyPath": null,
    "allowedKeys": [],
    "rpm": 60,
    "tpm": 100000,
    "maxInFlightPerUpstream": 8,
    "requestTimeoutSecs": 120,
    "discoveryRefreshSecs": 21600,
    "shutdownGraceSecs": 10
  }
}
```

Loopback-only is the default. Bind elsewhere only with
`--allow-non-loopback` (the gateway has bearer auth, but loopback is the safe
default). TLS is optional via `--tls-cert` / `--tls-key`.

## Errors

All failures return the OpenAI error envelope:

```json
{
  "error": {
    "message": "...",
    "type": "invalid_request_error",
    "param": null,
    "code": null
  }
}
```

| Upstream error | HTTP | `type` |
|---|---|---|
| Auth failure | 401 | `authentication_error` |
| Rate limit / quota | 429 | `rate_limit_error` |
| Invalid request / content filtered | 400 | `invalid_request_error` |
| Model not found | 404 | `model_not_found` |
| Upstream 5xx / stream error | 502 | `api_error` |
| Chain exhausted | 503 | `service_unavailable` |

Upstream API keys and raw URLs are never leaked in responses or logs.

## Graceful shutdown

On SIGINT/SIGTERM the gateway stops accepting new requests, drains active
SSE streams within `shutdownGraceSecs`, then aborts anything still running.
`/healthz` returns 503 during the drain.

## Building

```bash
cd src-rust && cargo build -p clawde-gateway
./target/debug/clawde-gateway --port 8787 --key dev-key
```