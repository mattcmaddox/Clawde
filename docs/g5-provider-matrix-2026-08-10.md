# G5 provider/model retry matrix

Date: 2026-08-10

This note records bounded live attempts to find a configured provider/model that
can complete the production semantic fixer. Tests used disposable `CLAWDE_HOME`
profiles, redacted output, bounded timeouts, and never touched the user project.
The known rate-limited `opencode-zen` route was excluded from retries.

## Acceptance contract

A candidate is accepted only if all of these are observed:

- semantic verifier reaches `fixable`;
- fresh fixer changes a scoped fixture file;
- `fix_verified=true`;
- `cargo_verified=true`;
- semantic re-verification passes; and
- goal continuation occurs only after that acceptance.

## Results

| Candidate | Result | Classification |
|---|---|---|
| `free/auto` (first run, OpenCode Zen excluded) | selected `openai/gpt-oss-120b`; production attempts exhausted | `rate_limited` |
| explicit `groq/openai/gpt-oss-120b` | runner unavailable in profile without copied auth state | `runner_unavailable` |
| `free -> groq` (auth + key-ring state copied) | production attempts exhausted | `authentication_error` |
| `free -> cline` | production attempts exhausted | `provider_error` |
| `free -> nvidia` | production attempts exhausted | `provider_error` |
| `free -> cloudflare` | production attempts exhausted | `provider_error` |
| `free/auto` retry (OpenCode Zen excluded; stored state copied) | production attempts exhausted | `authentication_error` |
| remote Ollama `qwen2.5-coder:7b` | verifier reached `fixable`; fixer made no disk change | `model_tool_call_incompatible` |

The Ollama result is independently confirmed outside Clawde: both native
`/api/chat` and OpenAI-compatible `/v1/chat/completions` returned valid HTTP 200
responses with zero tool calls, even when a tool was supplied and tool choice was
required. Clawde correctly rejected the no-op rather than accepting prose.

## Current conclusion

No configured candidate produced G5 acceptance in this matrix. This is not a
reason to weaken the fixer contract. The next successful run needs either:

1. a valid, non-exhausted credential for a free provider that emits structured
   tool calls; or
2. a different model installed on the remote GPU host whose Ollama template
   actually emits tool calls.

The local CPU Ollama path remains intentionally excluded.
