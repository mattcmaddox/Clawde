# Clawde Gateway

Clawde ships an OpenAI-compatible HTTP gateway that routes requests through
the same provider registry the TUI uses — the free-tier composite
(`FreeProvider`), key rotation, and cooldowns included. Any OpenAI SDK client
(openai-python, LangChain, Cursor, aider, Open WebUI) can point its
`base_url` at the gateway and get the free-catalog fallback chain for free.

The gateway serves **two request surfaces**:

1. **Relay chat completions** — `POST /v1/chat/completions` proxies to the
   upstream model, exactly as before.
2. **Agent mode** — the same endpoint (and the native `POST /v1/responses`
   surface) runs Clawde's agent loop server-side: it can execute a curated
   set of built-in tools (file read/write, grep, bash, web, tests), loop
   until the task is done, compact the transcript on overflow, and return
   the finished answer. See [Agent mode](#agent-mode) and
   [The `/v1/responses` endpoint](#the-v1responses-endpoint).

Related agent surfaces: Clawde also exposes an
[ACP (Agent Client Protocol) server](acp.md) for agent-to-agent
communication, and MCP servers appear as tools ([MCP](mcp.md)).

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
| `POST /v1/chat/completions` | Bearer | Chat completions (relay + agent mode, stream + non-stream) |
| `POST /v1/responses` | Bearer | Agent-native Responses API (Open Responses, stream + non-stream) |
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

## Agent mode

Agent mode runs Clawde's server-side agent loop: the gateway executes tools
on your machine and keeps calling the model until the task is done, then
returns the final answer. It activates on `POST /v1/chat/completions` when
**either**:

- the client sends `max_tool_calls` **and** declares at least one tool that
  maps to a built-in (e.g. `Read`, `Bash`), or
- the gateway is configured with `"agentMode": true` (then every chat
  completion runs the loop with the configured cap).

Without either, the request stays in plain relay mode.

```bash
curl http://127.0.0.1:8787/v1/chat/completions \
  -H "Authorization: Bearer my-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "free/auto",
    "messages": [{"role": "user", "content": "Read src/main.rs and summarize its purpose"}],
    "tools": [{
      "type": "function",
      "function": {"name": "Read", "description": "Read a file",
                   "parameters": {"type": "object",
                                  "properties": {"file_path": {"type": "string"}}}}
    }],
    "max_tool_calls": 5
  }'
```

Behaviour:

- **Silent intermediate turns** — internal tool executions never appear as
  SSE chunks; only the final turn streams. Declared tools that are **not**
  built-ins are yielded to the client as `tool_calls` with
  `finish_reason: tool_calls`, exactly as relay mode does today.
- `max_tool_calls` caps the number of tool executions; the loop force-stops
  cleanly (`finish_reason: stop`) when the cap is hit.
- Tool results are fed back to the model as error observations when they
  fail; the model can self-correct.
- Streaming (`"stream": true`) returns the final answer as normal SSE chunks
  plus an aggregate `usage` chunk when `stream_options.include_usage` is set.

## The `/v1/responses` endpoint

`POST /v1/responses` is the agent-native surface (Open Responses). The loop
is always the engine here: built-in tools execute server-side and every
turn's items are streamed natively.

```bash
curl http://127.0.0.1:8787/v1/responses \
  -H "Authorization: Bearer my-secret-key" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "free/auto",
    "input": [{"role": "user", "content": [{"type": "input_text", "text": "Find the TODO count in src"}]}],
    "tools": [{
      "type": "function",
      "name": "Grep",
      "description": "Grep files",
      "parameters": {"type": "object", "properties": {"pattern": {"type": "string"}}}
    }],
    "max_tool_calls": 5
  }'
```

Request knobs:

| Field | Meaning |
|---|---|
| `input` | String or item array (`message`, `function_call`, `function_call_output`; untyped `{role, content}` chat-style items are tolerated) |
| `instructions` | System prompt, passed **verbatim**; a minimal gateway preamble is injected only when the client sends none |
| `tools` | Flat function-tool list (built-ins execute server-side, others are yielded as `function_call` items) |
| `tool_choice` | Passed through to the upstream (`auto` / `required` / `none` / forced function) |
| `allowed_tools` | Hard allow-list: calls outside it are rejected with a `tool_error` observation (never executed), letting the model self-correct |
| `max_tool_calls` | Cap on tool executions; the response ends `incomplete` / `max_tool_calls` when hit |
| `parallel_tool_calls` | Default `true` (parallel, ordered execution); `false` serializes |
| `previous_response_id` | Continue a previous response (see below) |
| `store` | Accepted; sessions are ephemeral in-memory (see below) — nothing is ever written to disk |
| `n` | Only `1` is supported; `n > 1` → `400` |

Streaming emits Open Responses semantic events: `response.created`,
`response.output_item.added`, `response.function_call_arguments.delta/done`,
`response.output_text.delta/done`, `response.content_part.done`,
`response.output_item.done`, `response.completed` (or
`response.incomplete`), `response.done`, then `[DONE]`. Reasoning text from
thinking models is exposed as a `reasoning` item (raw `content`, capped at
32 KiB with a `…[truncated]` marker).

### Continuation and sessions

Sessions are **ephemeral and in-memory only** (D5): one bounded LRU
(`sessionCapacity` / `sessionTtlSecs`) backs both `store: false` and
`store: true`. Nothing is persisted to disk. A continuation with
`previous_response_id` hydrates the transcript as
`prev.input + prev.output + new input` and is serialized per id — concurrent
continuations on the same id wait for the previous turn to commit (D11).
Referencing an evicted or expired session returns
`400 previous_response_not_found`.

## Built-in tools

The server-side surface is a curated set (default; `gateway.builtinTools` is
a **replacement** list naming real built-ins — no wildcards):

| Tool | What it does |
|---|---|
| `Read` | Read a file |
| `Glob` | Find paths by pattern |
| `Grep` | Search file contents |
| `WebFetch` | Fetch a URL |
| `WebSearch` | Search the web |
| `Write` | Write a file |
| `Edit` | Apply an edit |
| `ApplyPatch` | Apply a patch |
| `Bash` | Run a shell command |
| `RunTests` | Run the project's tests |

The remaining ~35 tools in the TUI (`AskUserQuestion`, plan modes, cron,
worktree, team, …) are session-bound and stay external: if a model calls
one, it is yielded to the client as a `tool_calls` / `function_call` item
rather than executed. Tool names are matched case-insensitively. Tools run
with `gateway.workspacePaths[0]` (or the gateway's working directory) as the
working directory.

## Permissions

`gateway.permissionMode` controls what executes:

| Mode | Behaviour |
|---|---|
| `allow-readonly` (default) | Reads, globs, greps, and web fetches are allowed; writes and shell execution are denied |
| `allow` | Every built-in executes (bypasses permission checks) |
| `deny` | Nothing executes; every call short-circuits to a permission-denied tool error (relay-only posture) |

## Security posture

- **Loopback-only by default** — bind elsewhere only with
  `--allow-non-loopback`; TLS via `--tls-cert` / `--tls-key`.
- **Tool-result sanitization (D14)** — every tool result is stripped of
  terminal control sequences (C0 + ANSI CSI) and truncated to a budget at
  the executor boundary before it is fed back to the model. This is the
  structural defense against prompt injection via tool output (the top agent
  threat per AgentDojo/OWASP).
- **Untrusted framing (D15)** — failed tool calls come back as
  `tool_error: <tool>: <error>` observations with `is_error`, so the model
  sees failures explicitly and can self-correct.
- **Workspace scoping** — tools execute relative to `workspacePaths[0]`.
- **Cancellation** — a per-request token reaches tool execution and context
  compaction; a client disconnect aborts in-flight tools (D16).
- Upstream API keys and raw URLs are never leaked in responses or logs.

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
    "shutdownGraceSecs": 10,
    "agentMode": false,
    "maxToolCalls": 10,
    "permissionMode": "allow-readonly",
    "workspacePaths": [],
    "builtinTools": [],
    "sessionCapacity": 256,
    "sessionTtlSecs": 3600
  }
}
```

| Knob | Default | Meaning |
|---|---|---|
| `agentMode` | `false` | Run the agent loop for every chat completion |
| `maxToolCalls` | `10` | Default tool-call cap (client `max_tool_calls` overrides) |
| `permissionMode` | `allow-readonly` | `allow-readonly` / `allow` / `deny` |
| `workspacePaths` | `[]` | Tool working directory (`[0]` wins); falls back to the gateway cwd |
| `builtinTools` | `[]` | Replacement list for the curated built-in surface |
| `sessionCapacity` | `256` | Max retained response sessions (in-memory LRU) |
| `sessionTtlSecs` | `3600` | Session TTL (seconds) |

An invalid `permissionMode` fails at startup rather than silently defaulting.

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

| Condition | HTTP | `type` / `code` |
|---|---|---|
| Auth failure | 401 | `authentication_error` |
| Rate limit / quota | 429 | `rate_limit_error` |
| `n > 1`, bad input | 400 | `invalid_request_error` |
| `previous_response_id` evicted/expired | 400 | `previous_response_not_found` |
| Context overflow after compaction | 400 | `invalid_request_error` |
| Model not found | 404 | `model_not_found` |
| Upstream 5xx / stream error | 502 | `api_error` |
| Chain exhausted | 503 | `service_unavailable` |
| Cancelled (shutdown/disconnect) | 503 | `service_unavailable` |

Upstream API keys and raw URLs are never leaked in responses or logs.

## Graceful shutdown

On SIGINT/SIGTERM the gateway stops accepting new requests, drains active
SSE streams within `shutdownGraceSecs`, then aborts anything still running
(which cancels in-flight tool execution). `/healthz` returns 503 during the
drain.

## Building

```bash
cd src-rust && cargo build -p clawde-gateway
./target/debug/clawde-gateway --port 8787 --key dev-key
```

The same server runs through the CLI: `clawde serve [--port N] [--key K]
[--allow-non-loopback] [--tls-cert P] [--tls-key P]`.
