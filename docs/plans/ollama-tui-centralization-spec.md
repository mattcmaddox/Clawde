# Ollama TUI Centralization and Remote Server UX Specification

## Status

Implementation-ready specification. No code implementation is included in this document.

## Final decisions

### Canonical entry point

- `/ollama` with no arguments opens the full Ollama TUI screen.
- `/ollama config` is an alias for the same screen.
- `/connect ollama` reuses the same screen and state.
- The general `/model` picker is not the authoritative Ollama picker in the first implementation.

### Remote-only behavior

- Ollama is only a remote GPU provider in Clawde.
- Reject `localhost`, `127.0.0.1`, `::1`, `0.0.0.0`, and other unspecified/loopback addresses at every entry point.
- `allow_local_host` must not bypass this rule.
- Online/Isolated controls Clawde tool/network access only; it does not control server locality.
- New Ollama configuration defaults to Online.
- Mode changes apply immediately and persist.

### Model/server behavior

- Remember one last successful remote host and model.
- Prepopulate them without claiming the host is healthy until checked.
- Use explicit service discovery first when requested; retain manual host entry.
- If multiple servers are found, show a picker with host, latency, and model count; never choose silently.
- Use `/api/tags` as the installed-model source.
- Use `/api/ps` to mark loaded models when available.
- Use `/api/show` for optional metadata enrichment.
- Do not cache `/api/show` metadata.
- Refresh on screen open, host change, before final apply, and via explicit Refresh.
- If the selected model disappeared, choose the first available model and notify.
- If the remote server is unavailable, keep Ollama selected and offer retry; never silently fall back to Free Mode.

## Transport decision

Use the existing OpenAI-compatible transport as the only chat transport for the initial implementation:

```text
POST <remote-host>/v1/chat/completions
```

Reasons:

- It is already the production Ollama path in Clawde.
- It reuses existing streaming, tool-call, error, and response handling.
- It avoids a speculative rewrite of the provider adapter.

Use native Ollama endpoints only for discovery/status:

```text
GET  <remote-host>/api/tags
GET  <remote-host>/api/ps
POST <remote-host>/api/show
```

Do not implement native `/api/chat` in this feature. A native chat path requires a separate decision, exact wire-format mapping, streaming fixtures, and compatibility tests. It may be added later only for a demonstrated `/v1` limitation.

## Request option wire shape

Use the existing `ProviderRequest` and provider-options pipeline:

- `temperature` and `top_p` remain standard top-level request fields because Clawde already models and emits them there.
- Ollama-specific settings are placed under the Ollama provider's `provider_options`/`options` object.
- Do not duplicate a setting in both locations.
- Omit unset settings entirely. The remote Ollama server/model default remains authoritative.
- Keep the mapping in one centralized Ollama options conversion helper, consumed by both TUI-applied configuration and command/config paths.

Initial portable option mapping:

| UI setting | Request location | Default when unset |
|---|---|---|
| Temperature | top-level `temperature` | omitted; Ollama/model default |
| Top-p | top-level `top_p` | omitted; Ollama/model default |
| Context size | `options.num_ctx` | omitted; Ollama/model default |
| Max output | `options.num_predict` | omitted; Ollama/model default |
| Keep-alive | `options.keep_alive` | omitted; Ollama/model default |
| Seed | `options.seed` | omitted |
| Stop sequences | `options.stop` | omitted |
| Repeat penalty | `options.repeat_penalty` | omitted |
| Repeat last-n | `options.repeat_last_n` | omitted |
| Min-p | `options.min_p` | omitted |
| Typical-p | `options.typical_p` | omitted |
| TFS-z | `options.tfs_z` | omitted |
| Mirostat | `options.mirostat` | omitted |
| Mirostat tau | `options.mirostat_tau` | omitted |
| Mirostat eta | `options.mirostat_eta` | omitted |

Only include an option when the user has explicitly set it. Clearing an option removes it from the canonical configuration and request payload.

If a compatibility endpoint rejects an option, surface the provider error. Do not silently retry with a different wire shape or pretend a server-level setting was applied.

## Option defaults and UI priorities

### Default policy

Use **Ollama/model defaults when unset**. The UI may show suggestions/placeholders but must not persist or send those values simply because the screen opened.

Display unset fields as:

```text
Ollama/model default
```

Suggested values are informational only and are not Clawde defaults:

- context: model default;
- max output: model default;
- keep-alive: model default;
- temperature: model default;
- top-p: model default.

### Common controls

Expanded by default and ordered by frequency:

1. Context size (`num_ctx`)
2. Max output tokens (`num_predict`)
3. Keep-alive
4. Temperature
5. Top-p

The first three reuse the repository's existing Ollama controls. Temperature and top-p are added because the provider request type already supports them and they are common sampling controls.

### Advanced controls

Collapsed initially. Show only request-level options that can be represented through the `/v1` request path:

- seed;
- stop sequences;
- repeat penalty;
- repeat last-n;
- min-p;
- typical-p;
- tfs-z;
- mirostat, mirostat tau, and mirostat eta.

Do not expose GPU layer count, thread count, batch size, or parallel slots in the first Ollama screen. These are normally server startup/process controls, not reliable per-request settings through the selected transport. Document them as future server-management work rather than exposing controls that may not apply.

### Effective-options preview

Before applying a selected model, show what is active:

- explicit Clawde overrides with their values;
- omitted settings as `Ollama/model default`;
- no unsupported controls presented as active.

The preview is informational and must not trigger extra requests beyond the required model refresh.

## TUI layout

One modal screen:

1. Run mode (first focus; Online/Isolated plus tool-access explanation)
2. Remote host and health/latency
3. Check / Discover / Refresh actions
4. Installed model list
5. Common options
6. Collapsed advanced options
7. Effective-options preview and Apply/help footer

Each model uses two rows:

- Row 1: exact tag, friendly name if available, installed size, loaded-state marker.
- Row 2: parameter count, quantization, context window, and other available metadata.

Loaded states must use symbols/colors plus a text legend:

- loaded in VRAM;
- installed but not loaded;
- metadata unknown;
- stale/unavailable.

## `/ollama` command behavior

- `/ollama` opens the full screen.
- `/ollama config` opens the same screen.
- `/ollama status` shows configured host, loaded models, and VRAM information.
- `/ollama online` applies and persists Online mode immediately.
- `/ollama isolated` applies and persists Isolated mode immediately.
- `/ollama refresh` refreshes the live remote model list.
- `/ollama discover` runs service discovery and opens the server picker.

All command paths use the centralized Ollama configuration/state helpers. No direct per-key JSON mutation in command code.

## Centralized configuration abstraction

Create one authoritative Ollama configuration type/helper in core or the existing configuration module. It owns:

- normalized remote host;
- selected model;
- Online/Isolated mode;
- optional common and advanced request options;
- last successful host/model state;
- defaults and serialization;
- validation/normalization;
- conversion to `ProviderRequest` fields and provider options.

Canonical writes use one location and naming scheme. Existing keys are read/migrated where practical. `require_explicit_host` can remain readable for compatibility but remote-only behavior is unconditional.

Applying a screen transaction:

1. validate host;
2. refresh `/api/tags`;
3. ensure selected model exists or choose the first available;
4. validate options;
5. persist the complete canonical config;
6. update active provider/model/options;
7. update mode/tool policy;
8. record last successful host/model.

No partial persistence or partial runtime application.

## Discovery constraints

- Explicit action only; no scan per frame/request.
- Prefer mDNS/service discovery where available.
- Manual entry remains fully supported.
- Candidate validation requires remote-host validation and successful `/api/tags`.
- Never scan loopback, unspecified, or public address ranges.
- Bound concurrency, per-host timeout, and total scan duration.
- No credentials or prompts sent during discovery.
- Discovery failure degrades to manual entry.

## Testing acceptance criteria

### Core

- Remote DNS/IP accepted; local/loopback/unspecified rejected.
- `allow_local_host` cannot bypass rejection.
- Online is the default mode.
- Options round-trip and unset options remain absent.
- Legacy options resolve consistently.

### API/provider

- Exact `/api/tags` tags preserved.
- `/api/ps` loaded-state parsing handles partial responses.
- `/api/show` failure does not block selection.
- Request payload uses top-level standard fields and nested Ollama-specific options.
- Unset options are omitted.
- No native `/api/chat` dependency.

### TUI/command

- `/ollama` and `/ollama config` open the same screen.
- `/connect ollama` reuses it.
- Remembered host/model prepopulate without implying health.
- Refresh and pre-apply discovery work.
- Missing model selects first available and notifies.
- Two-row model display shows loaded state and metadata fallback.
- Common options precede collapsed advanced options.
- Effective preview distinguishes overrides from remote defaults.
- Applying updates host, provider, model, options, and mode coherently.
- Offline remote server remains selected and does not trigger Free Mode.

## Documentation

Update `docs/providers.md`, `docs/configuration.md`, `docs/local-models.md`, and command/help references with the final behavior, transport, option mapping, remote-only rule, model picker, loaded-state indicators, and command list.

## Non-goals

- Local Ollama fallback.
- Silent server selection.
- Free Mode fallback.
- Automatic model download/delete.
- Automatic unload on shared servers.
- Speculative native chat transport.
- GPU driver, CUDA, Windows GPU, or remote-host OS management.
- Per-request exposure of server startup controls without a verified API contract.
