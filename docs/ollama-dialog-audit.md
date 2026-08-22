# Ollama TUI Audit

## Scope

This audit covers the remote Ollama configuration dialog, model discovery,
endpoint resolution, persistence, provider activation, and asynchronous ping
handling.

## Findings and status

### Fixed: selected model propagation

The dialog previously saved the selected model only in
`config.provider_configs.ollama.options.model`, while provider activation cleared
`config.model`. The runtime therefore fell back to `ollama/llama3.2` regardless
of the model selected in the picker.

The activation path now accepts an explicit model override and stores it in
`Config.model`. Bare Ollama tags such as `qwen2.5-coder:7b` are preserved and are
converted to the correct request model by the Ollama provider path.

### Fixed: first-use model selection

If no model was saved, pressing Enter previously selected the hardcoded
`qwen2.5-coder:3b` tag without checking the server. Enter now starts model
discovery, so the user must select a model actually returned by Ollama.

### Fixed: retry behavior

The PingFailed screen advertised Enter as retry, but Enter only returned to the
default screen. Enter now starts a new model-discovery ping.

### Fixed: stale asynchronous results

Ping events now carry a request ID and whether they are model-discovery pings or
background health checks. Results from an older request, a closed dialog, or a
newer dialog state are ignored.

### Fixed: large model lists

The model picker now maintains a viewport and scroll offset. It displays up to
10 rows at a time and keeps the selected row visible.

### Fixed: Vim ping shortcut

The ping shortcut is now `Ctrl+P`, which passes through Vim insert-mode handling
without being inserted into the host or model field. The edit-mode hint exposes
the shortcut.

### Fixed: health refresh

Opening the dialog with a saved host starts a background health check while
preserving the fast Enter-to-connect view. Health-only results update the dot
without opening the model picker. Editing the host resets the health state.

### Fixed: documented endpoint compatibility

`config.provider_configs.ollama.api_base` remains the canonical TUI/settings
write target. The documented top-level `providers.ollama.api_base` remains a
compatibility fallback and now also retains native Ollama discovery behavior.

## Remaining limitations

- The health result is held in the dialog state and is not persisted with a
  timestamp across process restarts.
- The picker lists server models and supports navigation, but does not provide
  fuzzy filtering or autocomplete.
- The dialog does not pull missing models; models must already exist on the
  Ollama server.
- Ping verifies `/api/tags`; it does not perform a generation request, so model
  loading and inference readiness are not fully verified until the first request.
- The model discovery path is covered by state and event tests, but a mocked HTTP
  fixture for `ping_ollama_and_fetch_models` would provide stronger parser and
  timeout coverage.

## Validation

The repaired implementation has been validated with:

- Ollama-focused TUI tests
- Ollama-focused core tests
- `cargo check --workspace`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Live `GET /api/tags` and `GET /v1/models` checks against the configured remote
  Windows Ollama host
