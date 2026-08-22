# LLM Providers

Clawde supports a wide range of LLM providers through a unified provider abstraction. Every provider implements the same `LlmProvider` trait, so switching between them requires only a configuration change.

> Running a model on your own machine (llama.cpp, LM Studio, Ollama, vLLM)? See
> the dedicated [Local Models](local-models) guide for recommended server flags,
> tool-calling setup, model guidance, and cache accounting.

---

## Selecting a Provider

Use the `--provider` flag on any invocation to override the active provider:

```
clawde --provider openai "refactor this module"
clawde --provider ollama "explain this function"
clawde --provider groq --model llama-3.3-70b-versatile "write tests"
```

The provider can also be set persistently in `~/.clawde/settings.json`:

```json
{
  "provider": "openai"
}
```

When no provider is specified, Clawde defaults to **Free Mode** (`free/auto`),
which routes every request across your configured free upstreams as a smart
router (see [Managing routing with `/routing`](#managing-routing-with-routing)).
Set `"provider": "anthropic"` (or any other provider) to opt out.

---

## Multi-Key Rotation

Clawde supports configuring **multiple API keys** for a single provider. When one
key is exhausted (rate limited, quota exceeded, or auth failure), the system
automatically rotates to the next available key. This is especially useful for
free-tier providers with low per-key rate limits.

### Managing keys with `/keys`

Use the `/keys` slash command to manage keys:

| Command | Description |
|---|---|
| `/keys` | List all providers with configured keys |
| `/keys list [provider]` | Show keys (optionally filtered to one provider) |
| `/keys set <provider> <key1> [key2 ...]` | Replace all keys (clears previous) |
| `/keys add <provider> <key>` | Append a single key |
| `/keys remove <provider> <index>` | Remove key at 1-based index |

**Examples:**

```
/keys set groq gsk_key1 gsk_key2 gsk_key3
/keys add groq gsk_key4
/keys remove groq 1
/keys list groq
```

**Cloudflare composite keys:** Cloudflare's OpenAI-compatible endpoint embeds the
account ID in the URL path, so a stored key must carry both halves joined by a
colon — `ACCOUNT_ID:API_TOKEN`. `/keys` shape-validates this before saving; a
bare token or a key with an empty half is rejected with a format hint.

```
/keys add cloudflare abc123def456:your-api-token
```

### Key rotation behavior

- **Rate limited (429):** The exhausted key cools down for 60 seconds (or the
  `Retry-After` value from the response, whichever is more precise).
- **Quota exceeded:** The key cools down for 1 hour by default.
- **Auth failure (401/403):** The key cools down for 5 minutes.
- When **all keys** for a provider are in cooldown, requests fail with a
  `RateLimited` error containing the time until the earliest key recovers.
- Keys with 2+ configured keys show a rotation indicator in the status bar
  when any keys are exhausted (e.g. `groq:2/3 keys` in yellow/red).

### Storage

Keys are stored in `~/.clawde/auth.json` under the `"keys"` map, separate from
single-key credentials. The `/keys` command manages this automatically.

Keys are read through a single source of truth in the free-provider module
(`crates/api/src/providers/free/mod.rs`), which guarantees the health poller,
the rotation rings, and the Connect Free dialog all agree on which keys exist
and in what order:

- **`resolve_free_upstream_keys`** — the ring-aligned list used to build
  `KeyRotatingProvider` rings *and* to probe health. It prefers the multi-key
  store (credentials are excluded so `key_idx` stays in sync with ring slots),
  trims whitespace, drops <8-char placeholder keys, and uses the documented
  environment/OpenCode CLI fallback when no stored slot exists.
- **`first_free_upstream_key`** — the single-key chain path: first valid ring
  slot, else the stored credential (incl. OAuth tokens), else the provider's
  env var.
- **`all_stored_free_upstream_keys`** — display only: credentials + rotation
  keys merged and deduplicated for the Connect Free dialog's health dots.

OpenCode Zen reads the `opencode-go` key slots as a fallback in all three
resolvers.

Health probes distinguish invalid credentials from transient provider trouble:
401/403-style failures mark a key unhealthy, while 429, 5xx, connection, and
empty-response failures are shown as transient and do not evict the key. Health
polling is bounded-concurrent across configured upstreams, and its key indexes
remain aligned with the rotation rings.

Provider failures also carry a shared recovery class (`invalid_credential`,
`rate_limited`, `quota_exhausted`, `transient_provider`, `context_overflow`,
`malformed_request`, `content_filtered`, and related classes). Free Mode uses
that classification for fallback decisions: context overflow may move to a
larger upstream, while malformed requests and content-filter decisions do not
fan out to every provider. A streamed attempt is considered committed only
when generated text, reasoning, or tool arguments have been emitted; transport
metadata alone does not prevent pre-output fallback, and visible output is
never silently replayed.

Free Mode also records fresh rate-limit utilization headers separately from
credential health. OpenAI-compatible completed responses and streaming
responses contribute when they expose those headers; native Anthropic and Gemini
responses do the same on their completed and streaming paths. Automatic and
family routes softly demote an upstream when reported request or token
utilization reaches 60%, 80%, or 95%; the upstream
remains eligible and can recover when the observation expires after 15 minutes
or when the provider-reported reset time arrives. `Retry-After` and common
request/token reset headers are retained as timing metadata rather than being
used to invalidate credentials. When a rotating provider can identify the
serving slot, the observation is also retained against that exact key index;
the upstream aggregate remains available for routing while key data stays
separate from credential health. During a live process, key rotation prefers
lower-utilization active keys and keeps round-robin order for equal ranks;
missing or expired key observations remain neutral.
Missing or stale headers do not mean zero capacity or an invalid key, and an
explicit provider pin remains first. When persistence is enabled, these
observations are stored privately in `capacity-state/free.json`, separate from
`auth.json` and key cooldown state. For providers without usable headers, Free
Mode also keeps a conservative local sliding-window estimate only where an
explicit catalog limit is known: Groq (1K requests/day), Cerebras (5 RPM / 30K
TPM), and SambaNova (20 RPM / 200K TPD). Dispatches count estimated input
usage and successful completions add known output usage; each window resets on
its own schedule. Providers with ambiguous limits remain neutral, and fresh
server metadata always takes precedence over the local estimate.

Capacity status is intentionally compact and read-only. `/keys health`,
`/routing`, and the `/stats` live key-health view show rows such as
`free · groq  72% used · headers · reset in 1m 30s` when a fresh signal exists.
`headers` means the value came from the provider; `local` means it came from an
explicit local quota estimate. Missing or expired capacity data is omitted
rather than displayed as `0%`, and it never marks a credential invalid.

### Managing routing with `/routing`

Use the `/routing` slash command to view or change how the free-mode router
selects upstream providers:

| Command | Description |
|---|---|
| `/routing` | Show the current routing strategy, per-task assignments, and available capacity status |
| `/routing auto` | **Default.** Classify each request by task and dispatch to the upstreams best suited to it first, ordered by historical latency within the task-preferred group (spec §8.4 — no user config needed) |
| `/routing sequential` | Try upstreams in catalog priority order |
| `/routing random` | Randomize upstream order each request |
| `/routing latency` | Route to the lowest-latency upstream first |
| `/routing task` | Route by request type — each request is classified (code generation, reasoning, verification, …) and dispatched to the upstreams best suited to that task first, falling through the rest on failure |
| `/routing sr` / `/sr` | Quick alias for sequential |
| `/routing rr` / `/rr` | Quick alias for random |
| `/routing lr` / `/lr` | Quick alias for latency |
| `/routing tr` / `/tr` | Quick alias for task |
| `/routing edit` | Open the interactive task-pinning dialog in the TUI (spec §8.6) — shows the 7 task types with their assignments and lets you pin/unpin upstreams per task with the space bar |

**Examples:**

```
/routing
/routing random
/rr
/routing task
/tr
```

The setting is persisted in `~/.clawde/settings.json` under
`providers.free.options.routing.strategy` and applies immediately — the
active provider is rebuilt on the change, so no restart or `/refresh` is
needed.

**Task-based routing (audit spec Phase 2):** with `strategy: "task_based"`,
Clawde classifies each request into a task type (`code_generation`, `code_edit`,
`reasoning`, `planning`, `verification`, `simple_edit`, `search`) and tries the
upstreams best suited to that task first, then the remaining upstreams in
catalog order. The built-in preferences are:

- **code generation** → OpenRouter (DeepSeek), Cerebras, Hugging Face, …
- **reasoning** → Gemini, Groq, SambaNova, …
- **verification** → Groq, Cloudflare, OpenCode Zen, … (fastest tokens)
- **simple edit** → Z.AI, OpenCode Zen, SambaNova, … (cheapest)

Override the per-task preference lists in `settings.json` — a task with an
override uses it verbatim; tasks without one keep their built-in defaults:

```json
{
  "providers": {
    "free": {
      "options": {
        "routing": {
          "strategy": "task_based",
          "task_preferences": {
            "code_generation": ["groq", "cerebras"],
            "verification": ["groq", "cloudflare"]
          }
        }
      }
    }
  }
}
```

Upstream ids must match the free catalog (e.g. `groq`, `cerebras`,
`huggingface`, `google`, `openrouter`, `zai`, `opencode-zen`, `cloudflare`,
`sambanova`, `nvidia`, `cohere`, `mistral`, `cline`).

### Router behaviour (audit spec Phase 2)

Beyond choosing a strategy, the smart router applies a few automatic guards and
refinements on every request:

- **Capability gating** — image-bearing requests only reach vision-capable
  upstreams, and requests whose estimated input tokens exceed an upstream's
  context window are skipped before dispatch. Instead of burning a guaranteed-
  fail round-trip on a text-only or undersized provider, the chain moves
  straight to one that can serve the request.
- **Performance-aware ordering** — within the task-preferred group, upstreams
  with enough dispatch history are ordered by **success rate, then average
  latency**. A task-appropriate upstream that keeps failing yields to one that
  actually succeeds; unmeasured upstreams tail the group in preference order.
- **Persistent cooldowns** — 5xx / server-error and empty-completion cooldowns
  are written to `~/.clawde/empty-cooldown-state/free.json` and restored on the
  next launch, so a flaky upstream is not re-hit after every restart. The
  cooldown duration for server errors is configurable via
  `providers.free.options.routing.upstream_5xx_cooldown_secs` (default 45s);
  set it to `0` to disable.

**Individual free upstreams (`groq/…`, `huggingface/…`, …):** selecting a
free-catalog upstream — `clawde -m groq/llama-3.3-70b-versatile`,
`/model <upstream>/<model>`, `--provider groq`, or a `provider`/`model` pair in
settings.json — routes through the router's *pinned* route: the pinned upstream
is tried first, then the rest of the chain falls through on transient errors.
Dispatch telemetry, cooldowns and key rotation therefore stay attached to the
free chain. Non-catalog providers (OpenAI, Ollama, Azure, …) keep direct
dispatch.

**Headless attribution:** `clawde --print` reports the serving provider on
stderr (`Model: <model> via <upstream>`) and `--output-format json` results
carry `provider` / `upstream` / `model` fields, so you can audit which free
upstream answered (or that a fallback happened).

**Exhaustion errors:** when every upstream in the chain fails, the error names
the **original** failures rather than only the last upstream's raw error — e.g.
`all free-mode upstreams exhausted: groq: [groq] Rate limited, …,
... and 9 more, ollama: [ollama] Model not found: unknown`. Consecutive
duplicates are collapsed first (a pinned upstream retrying its fallback models
against the same provider repeats the same failure), then the list is capped
for readability: the first 5 errors are shown, `… and N more` counts the
omitted middle entries, and the **last** upstream's error is always appended
(since the final fallback's failure is usually the most relevant). With 6 or
fewer failures the full list is shown.

**Model-performance dashboards (spec §8.6):** the router records dispatch
success rates (aggregate and per-task) plus average latency per upstream, and
exposes them in three places:

- `/routing edit` — key-health dots, cooldown tags, capability badges, average
  latency, and success rate per upstream. The `%` column is task-aware: select
  a task in the left pane to see each upstream's rate **for that task**
  (falling back to the aggregate when the task has no dispatches on it).
- `/stats` — the live key-health table shows each upstream's success rate
  (green ≥99%, yellow partial, red 0%) and average latency alongside the key
  dots and cooldown counts.
- `/keys health` — the *Free Upstream Performance* section lists each
  upstream's aggregate success rate, average latency, and per-task success
  rates in CLI form, honoring the same provider/upstream filter.

---

## Provider Reference

### Anthropic

The provider used when you explicitly select it (e.g. `--provider anthropic` or
`"provider": "anthropic"`). Not the built-in default — a fresh config starts in
Free Mode. Uses the `/v1/messages` streaming endpoint.

**Authentication:** `ANTHROPIC_API_KEY` environment variable, or set `api_key` in `settings.json`.

**Default model:** `claude-sonnet-4-6`

**Available models (bundled snapshot):**

| Model ID | Context Window | Max Output | Input ($/1M) | Output ($/1M) |
|---|---|---|---|---|
| `claude-opus-4-6` | 200,000 | 32,000 | $15.00 | $75.00 |
| `claude-sonnet-4-6` | 200,000 | 16,000 | $3.00 | $15.00 |
| `claude-haiku-4-5-20251001` | 200,000 | 8,096 | $0.80 | $4.00 |

All Anthropic models support tool calling, vision, and extended reasoning.

**Configuration:**

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "models_whitelist": ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"]
    }
  }
}
```

**Base URL override:** Set `ANTHROPIC_BASE_URL` to point at a proxy or local mirror.

---

### OpenAI

Uses the OpenAI Chat Completions API (`/v1/chat/completions`).

**Authentication:** `OPENAI_API_KEY` environment variable.

**Default model:** `gpt-4o`

**Available models (bundled snapshot):**

| Model ID | Context Window | Max Output | Reasoning |
|---|---|---|---|
| `gpt-4o` | 128,000 | 16,384 | No |
| `gpt-4o-mini` | 128,000 | 16,384 | No |
| `o3` | 200,000 | 100,000 | Yes |
| `o4-mini` | 200,000 | 100,000 | Yes |

**Configuration:**

```json
{
  "provider": "openai",
  "providers": {
    "openai": {
      "api_key": "sk-...",
      "api_base": "https://api.openai.com/v1"
    }
  }
}
```

---

### Google (Gemini)

Uses the Google Generative Language / Vertex AI API.

**Authentication:** `GOOGLE_API_KEY` environment variable (for AI Studio) or `GOOGLE_APPLICATION_CREDENTIALS` (for Vertex AI).

**Default model:** `gemini-2.5-flash`

**Available models (bundled snapshot):**

| Model ID | Context Window | Max Output |
|---|---|---|
| `gemini-2.5-pro` | 1,048,576 | 65,536 |
| `gemini-2.5-flash` | 1,048,576 | 65,536 |
| `gemini-2.0-flash` | 1,048,576 | 8,192 |

**Configuration:**

```json
{
  "provider": "google",
  "providers": {
    "google": {
      "api_key": "AIza..."
    }
  }
}
```

---

### Azure OpenAI

Uses the Azure OpenAI Chat Completions endpoint. The deployment name acts as the model identifier.

**Authentication:** Three environment variables are required:
- `AZURE_API_KEY` — your Azure OpenAI API key
- `AZURE_RESOURCE_NAME` — your Azure resource name (the subdomain of `.openai.azure.com`)
- `AZURE_API_VERSION` — API version (defaults to `2024-08-01-preview`)

**Default model:** `gpt-4o`

**Request URL format:**

```
https://{AZURE_RESOURCE_NAME}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}
```

**Configuration:**

```json
{
  "provider": "azure",
  "providers": {
    "azure": {
      "api_key": "...",
      "options": {
        "resource_name": "my-azure-resource",
        "api_version": "2024-08-01-preview"
      }
    }
  }
}
```

---

### AWS Bedrock

Uses the Bedrock Converse Streaming API. Supports all Claude models deployed on Bedrock.

**Authentication (two modes):**

1. **Bearer token:** Set `AWS_BEARER_TOKEN_BEDROCK` (takes priority over SigV4).
2. **SigV4 credentials:** Set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.

**Region:** Reads `AWS_REGION` or `AWS_DEFAULT_REGION` (defaults to `us-east-1`).

**Default model:** `anthropic.claude-sonnet-4-6-v1`

The adapter automatically prepends regional cross-inference prefixes (e.g. `us.anthropic.claude-...`) for US-region deployments.

**Configuration:**

```json
{
  "provider": "amazon-bedrock",
  "providers": {
    "amazon-bedrock": {
      "options": {
        "region": "us-east-1"
      }
    }
  }
}
```

---

### GitHub Copilot

Uses the GitHub Copilot Chat Completions API (`https://api.githubcopilot.com/chat/completions`).

**Authentication:** `GITHUB_TOKEN` environment variable.

**Default model:** `gpt-4o`

**Configuration:**

```json
{
  "provider": "github-copilot",
  "providers": {
    "github-copilot": {
      "api_key": "ghu_..."
    }
  }
}
```

---

### Cohere

Native Cohere API adapter.

**Authentication:** `COHERE_API_KEY` environment variable.

**Default model:** `command-r-plus`

**Configuration:**

```json
{
  "provider": "cohere",
  "providers": {
    "cohere": {
      "api_key": "..."
    }
  }
}
```

---

### MiniMax

The built-in provider uses the Anthropic-compatible Messages API.

**Authentication:** `MINIMAX_API_KEY` environment variable, or set `api_key` in `settings.json`.

**Default model:** `MiniMax-M3`

| Model | Context window | Input modalities | Thinking |
|---|---:|---|---|
| `MiniMax-M3` | 1,000,000 | Text, image, video | Off by default; supports `adaptive` and `disabled` |
| `MiniMax-M2.7` | 204,800 | Text | Always on |

The catalog retains the model's complete input-modality metadata. Clawde's built-in attachment flow currently sends text and image blocks.

Pricing is in USD per million tokens:

| Model | Service tier | Input range | Input | Output | Cache read | Cache write |
|---|---|---:|---:|---:|---:|---:|
| `MiniMax-M3` | Standard | Up to 512k | $0.30 | $1.20 | $0.06 | Not published |
| `MiniMax-M3` | Standard | Over 512k | $0.60 | $2.40 | $0.12 | Not published |
| `MiniMax-M3` | Priority | Up to 512k | $0.45 | $1.80 | $0.09 | Not published |
| `MiniMax-M3` | Priority | Over 512k | $0.90 | $3.60 | $0.18 | Not published |
| `MiniMax-M2.7` | Standard | All requests | $0.30 | $1.20 | $0.06 | $0.375 |

| Protocol | Global base URL | China base URL | Path added by Clawde |
|---|---|---|---|
| Anthropic | `https://api.minimax.io/anthropic` | `https://api.minimaxi.com/anthropic` | `/v1/messages` |
| OpenAI-compatible | `https://api.minimax.io/v1` | `https://api.minimaxi.com/v1` | `/chat/completions` |

The built-in `minimax` provider uses the Anthropic row. To use the China endpoint, set `MINIMAX_BASE_URL` or configure `api_base`:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "api_base": "https://api.minimaxi.com/anthropic"
    }
  }
}
```

MiniMax-M3 uses the standard service tier by default. To request priority admission, set `service_tier` in the provider options:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "options": {
        "service_tier": "priority"
      }
    }
  }
}
```

For the OpenAI-compatible protocol, use the custom provider with the corresponding `/v1` base URL:

```json
{
  "provider": "custom-openai",
  "model": "MiniMax-M3",
  "providers": {
    "custom-openai": {
      "api_key": "...",
      "api_base": "https://api.minimax.io/v1"
    }
  }
}
```

---

### Ollama

Connects to an explicitly configured remote Ollama instance, normally a GPU
server. No API key is required. Clawde does **not** fall back to
`localhost:11434`: an unconfigured or loopback endpoint is treated as
unavailable, preventing accidental local CPU inference.

**Base URL:** Reads `providers.ollama.api_base`, then `OLLAMA_HOST`, then
`providers.ollama.options.default_host`. Clawde appends `/v1` to construct the
OpenAI-compatible endpoint. Use a DNS name or non-loopback IP for the remote
GPU host.

**Default model:** `llama3.2`

**Model list:** When using `/connect` or `/model`, the picker queries your local Ollama server via `/api/tags` and shows only the models you have installed (`ollama list`). Cloud models (e.g., `kimi-k2.6:cloud`) appear after you run `ollama pull <model>:cloud`.

**Configuration:**

```json
{
  "provider": "ollama",
  "providers": {
    "ollama": {
      "api_base": "http://gpu-host.example:11434"
    }
  }
}
```

Then run the explicitly configured remote model:

```
clawde --provider ollama --model llama3.2 "explain this code"
```

**VRAM controls:**

- `/ollama status` reports loaded models and the VRAM sizes reported by Ollama.
- `/unload` unloads every currently loaded model on the configured server.
- `/unload <model>` unloads only the named loaded model, for example
  `/unload qwen2.5-coder:7b`.
- Automatic unload on provider/model switch is disabled by default. Enable
  `Ollama: Auto-unload on switch` only when the configured server is dedicated
  to this Clawde session; targeting the previous model still cannot prove that
  another instance is not using the same model.
- Separate Clawde instances should use explicit `/unload <model>` only when
  they coordinate ownership of a shared Ollama server.

**Remote GPU default:**

For a GPU server shared across machines on a LAN, `api_base` is the clearest
configuration. A `default_host` option is also supported when you want every
Clawde instance to target the same remote endpoint:

```json
{
  "provider": "ollama",
  "providers": {
    "ollama": {
      "options": {
        "default_host": "http://gpu-host.example:11434"
      }
    }
  }
}
```

The host resolution order is:
1. `providers.ollama.api_base` (explicit override)
2. `OLLAMA_HOST`
3. `providers.ollama.options.default_host`
4. no endpoint — Ollama is unavailable

Loopback names and addresses such as `localhost`, `127.0.0.1`, and `0.0.0.0`
are rejected. The old `require_explicit_host` option is retained for settings
compatibility but is no longer needed: remote-only fail-closed behavior is now
the default.

**Tools and offline mode:**

Normal Ollama mode (`ollama:auto`) keeps Clawde's tools enabled, including
`WebSearch` and `WebFetch`. Toggle `/ollama` to `ollama:offline` / isolated mode
when the model must not use online tools. In isolated mode Clawde removes
network-capable tools (web search/fetch, remote triggers, MCP tools) and blocks
them again at dispatch, including with `--dangerously-skip-permissions`.
Ollama inference itself remains available through the configured remote GPU
endpoint. Isolated mode also removes shell/interpreter execution, sub-agents,
LSP/MCP resources, test/lint commands, and configured formatter subprocesses;
use an OS/container firewall as defense in depth for a strict air gap.

---

### LM Studio (local)

Connects to a locally running LM Studio server. No API key required.

**Base URL:** Reads `LM_STUDIO_HOST` (defaults to `http://localhost:1234`). Clawde appends `/v1`.

**Default model:** `default` (whichever model is loaded in LM Studio)

**Configuration:**

```json
{
  "provider": "lmstudio",
  "providers": {
    "lmstudio": {
      "api_base": "http://localhost:1234/v1"
    }
  }
}
```

---

### LLaMA.cpp (local)

Connects to a locally running llama.cpp HTTP server. No API key required.

**Base URL:** Reads `LLAMA_CPP_HOST` (defaults to `http://localhost:8080`). Clawde appends `/v1`.

**Default model:** `default`

**Configuration:**

```json
{
  "provider": "llamacpp",
  "providers": {
    "llamacpp": {
      "api_base": "http://localhost:8080/v1"
    }
  }
}
```

Start llama.cpp with the `--server` flag before use.

For recommended `llama-server` flags (tool calling, context sizing, prompt
caching), model guidance, and cache-accounting details, see the
[Local Models](local-models) guide.

---

### Groq

Fast inference cloud with OpenAI-compatible API.

**Authentication:** `GROQ_API_KEY` environment variable.

**Base URL:** `https://api.groq.com/openai/v1`

**Default model:** `llama-3.3-70b-versatile`

**Configuration:**

```json
{
  "provider": "groq",
  "providers": {
    "groq": {
      "api_key": "gsk_..."
    }
  }
}
```

---

### DeepSeek

OpenAI-compatible API with extended reasoning output via a `reasoning_content` field.

**Authentication:** `DEEPSEEK_API_KEY` environment variable.

**Base URL:** `https://api.deepseek.com/v1`

**Default model:** `deepseek-chat`

**Configuration:**

```json
{
  "provider": "deepseek",
  "providers": {
    "deepseek": {
      "api_key": "sk-..."
    }
  }
}
```

---

### Mistral AI

OpenAI-compatible API with Mistral-specific protocol quirks (tool call ID formatting, tool-user sequence injection).

**Authentication:** `MISTRAL_API_KEY` environment variable.

**Base URL:** `https://api.mistral.ai/v1`

**Default model:** `mistral-large-latest`

**Configuration:**

```json
{
  "provider": "mistral",
  "providers": {
    "mistral": {
      "api_key": "..."
    }
  }
}
```

---

### xAI (Grok)

**Authentication:** `XAI_API_KEY` environment variable.

**Base URL:** `https://api.x.ai/v1`

**Default model:** `grok-2`

**Configuration:**

```json
{
  "provider": "xai",
  "providers": {
    "xai": {
      "api_key": "xai-..."
    }
  }
}
```

---

### OpenRouter

Unified API gateway to many models. Sends `HTTP-Referer: https://mattcmaddox.github.io/Clawde/` and `X-Title: Clawde` headers automatically.

**Authentication:** `OPENROUTER_API_KEY` environment variable.

**Base URL:** `https://openrouter.ai/api/v1`

**Default model:** `anthropic/claude-sonnet-4`

Model identifiers use OpenRouter's routing format: `provider/model-name`.

**Configuration:**

```json
{
  "provider": "openrouter",
  "providers": {
    "openrouter": {
      "api_key": "sk-or-..."
    }
  }
}
```

---

### Together AI

Hosted open-source models.

**Authentication:** `TOGETHER_API_KEY` environment variable.

**Base URL:** `https://api.together.xyz/v1`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct-Turbo`

**Configuration:**

```json
{
  "provider": "togetherai",
  "providers": {
    "togetherai": {
      "api_key": "..."
    }
  }
}
```

---

### Perplexity

Search-augmented LLM API.

**Authentication:** `PERPLEXITY_API_KEY` environment variable.

**Base URL:** `https://api.perplexity.ai`

**Default model:** `sonar-pro`

**Configuration:**

```json
{
  "provider": "perplexity",
  "providers": {
    "perplexity": {
      "api_key": "pplx-..."
    }
  }
}
```

---

### DeepInfra

Hosted open-weight models on OpenAI-compatible API.

**Authentication:** `DEEPINFRA_API_KEY` environment variable.

**Base URL:** `https://api.deepinfra.com/v1/openai`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct`

**Configuration:**

```json
{
  "provider": "deepinfra",
  "providers": {
    "deepinfra": {
      "api_key": "..."
    }
  }
}
```

---

### Venice AI

Privacy-focused inference.

**Authentication:** `VENICE_API_KEY` environment variable.

**Base URL:** `https://api.venice.ai/api/v1`

**Default model:** `llama-3.3-70b` (resolved from the model registry at runtime)

**Configuration:**

```json
{
  "provider": "venice",
  "providers": {
    "venice": {
      "api_key": "..."
    }
  }
}
```

---

### Cerebras

Wafer-scale inference hardware.

**Authentication:** `CEREBRAS_API_KEY` environment variable.

**Base URL:** `https://api.cerebras.ai/v1`

**Default model:** `llama-3.3-70b`

**Configuration:**

```json
{
  "provider": "cerebras",
  "providers": {
    "cerebras": {
      "api_key": "..."
    }
  }
}
```

---

## Per-Provider Configuration in settings.json

The `providers` map in `~/.clawde/settings.json` accepts per-provider `ProviderConfig` objects:

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "api_base": "https://api.anthropic.com",
      "enabled": true,
      "models_whitelist": [],
      "models_blacklist": [],
      "options": {}
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "enabled": true,
      "api_base": "http://gpu-host.example:11434/v1"
    }
  }
}
```

**Fields:**

| Field | Type | Description |
|---|---|---|
| `api_key` | string | Override the environment variable API key |
| `api_base` | string | Override the default base URL |
| `enabled` | bool | Enable or disable the provider (default: `true`) |
| `models_whitelist` | array of strings | If non-empty, only listed model IDs are allowed |
| `models_blacklist` | array of strings | Listed model IDs are refused |
| `options` | object | Provider-specific pass-through options |

## Model Whitelist and Blacklist

When `models_whitelist` is non-empty for a provider, only the listed model IDs can be selected for that provider. Any model ID in `models_blacklist` is rejected regardless of the whitelist:

```json
{
  "providers": {
    "openai": {
      "models_whitelist": ["gpt-4o", "gpt-4o-mini"],
      "models_blacklist": ["gpt-4o-mini"]
    }
  }
}
```

The above example allows only `gpt-4o` (whitelist minus blacklist).

## Model Registry

Clawde ships a bundled snapshot of models for Anthropic, OpenAI, and Google. At runtime it optionally refreshes from the public `https://models.dev/api.json` API (cached to `~/.clawde/models_cache.json`, refreshed at most every 5 minutes). Network failures are swallowed silently; the bundled snapshot is always sufficient for normal operation.

When no model is explicitly set, Clawde scores available models by priority patterns to pick the best default. Well-known model prefixes (`claude-*`, `gpt-*`, `gemini-*`, etc.) are always routed to their canonical provider regardless of gateway entries in the remote cache.

### Overriding model metadata

Self-hosted endpoints (the `custom-openai` / Ollama / LM Studio / llama.cpp
providers) and model aliases that models.dev does not know can end up with the
wrong context window or max-output size — either because the alias is matched to
an unrelated catalog entry, or because there is no catalog entry at all. The
`modelOverrides` map lets you supply or correct that metadata. **User overrides
take precedence over the models.dev catalog and over the built-in defaults.**

Add it at the top level of `~/.clawde/settings.json` (or inside the `config`
object), keyed by the fully-qualified `"provider/model"` id:

```json
{
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM",
      "releaseDate": "2026-01-01",
      "status": "beta"
    },
    "ollama/qwen3-coder-30b": {
      "contextWindow": 262144
    }
  }
}
```

**Fields** (all optional — an unset field keeps the catalog value):

| Field | Type | Description |
|---|---|---|
| `contextWindow` | integer | Total context window size in tokens |
| `maxOutputTokens` | integer | Maximum tokens the model can emit in one response |
| `name` | string | Human-readable display name shown in the model picker |
| `releaseDate` | string | ISO 8601 date; drives newest-first ordering in the picker |
| `status` | string | Lifecycle status (`active`, `beta`, `alpha`, `deprecated`) |

Field names accept both camelCase (`contextWindow`) and snake_case
(`context_window`). The key **must** contain a `/` — a bare model id is ignored,
because the registry is keyed by `provider/model`.

When the keyed model exists in the catalog, the override patches it in place.
When it does not (a self-hosted alias), Clawde materialises a synthetic entry
so the corrected values flow everywhere the metadata is read: the `/model`
picker, the token-usage warnings, and the auto-compact thresholds.
