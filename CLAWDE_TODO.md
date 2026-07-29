# Clawde — TODO & Improvement Tracker

## Priority Levels
- **P0:** Critical — blocks usage or causes data loss
- **P1:** High — significant functionality gap or pain point
- **P2:** Medium — nice-to-have improvement
- **P3:** Low — polish or long-term

---

## 🔴 P0 — Critical

### [legacy] Legacy ~/.claurst/ backward compat tested
- [x] Test that existing users with `~/.claurst/` dirs are properly detected
- [x] Test that `$CLAURST_HOME` env var still works (or document that it was renamed to `$CLAWDE_HOME`)
- [x] Auto-migration: `config_dir()` now renames `~/.claurst/` to `~/.clawde/` on first run when the legacy dir exists but the new dir doesn't.
  - Implemented in `Settings::config_dir()` (lib.rs:1749-1775)
  - `std::fs::rename` is atomic on same filesystem; graceful fallback to legacy path on error
  - Test: `clawde_home_migrates_legacy_claurst_dir` verifies the rename + file content survival
  - Test: `test_legacy_claurst_fallback` updated to reflect auto-migration behavior

### [tools] Grep tool UTF-8 safety
- [x] The `grep_tool.rs` `content` field — verified intentional, used in `content_lines` Display impl
- [x] Added 4 new unit tests: `grep_non_ascii_utf8`, `grep_empty_file`, `grep_regex_special_chars`, `grep_matches_at_line_start`

### [tests] Env guard discipline
- [x] Audited all env-mutating test sites across the workspace (see report below)
- [x] Made `ENV_LOCK` in `paths.rs` `pub(crate)` and re-exported at module level
- [x] Fixed `core/src/lib.rs`: added ENV_LOCK to 4 test functions (3 ANTHROPIC_API_KEY tests + test_legacy_claurst_fallback)
- [x] Fixed `core/src/accounts.rs`: switched from local `HOME_LOCK` to shared `ENV_LOCK`
- [x] Fixed `core/src/share_export/mod.rs`: added ENV_LOCK to `viewer_url_default_and_override`
- [x] Fixed `mcp/src/lib.rs`: added local `ENV_LOCK` + locks to 5 env-mutating tests
- [x] Still outstanding: `mcp/src/lib.rs` has 2 tests (`test_expand_env_vars_default_value`, `test_expand_env_vars_missing_no_default`) that use `remove_var` without locks (low risk — unique var names like `_CC_MISSING_VAR`)
  **(FIXED)**

---

## ✅ Auto-Compact System (Gaps 1–6 + reactive wiring) — Complete

All six gaps identified in the auto-compact research have been implemented and tested.

| Gap | Description | Status | Files Changed |
|-----|-------------|--------|---------------|
| 1 | Session loop wiring — `auto_compact_if_needed()` called in `run_query_loop` after each assistant turn | ✅ | `query/src/lib.rs` (was already wired) |
| 2 | Generic provider support — replaced `&AnthropicClient` with `&dyn LlmProvider` across all compact functions | ✅ | `query/src/compact.rs`, `query/src/lib.rs` |
| 3 | User-facing config toggle — `Settings::effective_config()` merge, `/auto-compact` command, query loop gate | ✅ | `core/src/lib.rs` (config), `query/src/lib.rs`, `commands/src/lib.rs` |
| 4 | TUI footer context indicator — green/yellow/red "ctx: N%" display with auto-compact on/off state | ✅ | `tui/src/render.rs` |
| 5 | Debounce/hysteresis — min 5 turns + min 60 sec between compactions, first compaction fires immediately | ✅ | `query/src/compact.rs` |
| R | Reactive compact wired into auto_compact config gate — `CLAUDE_REACTIVE_COMPACT=1` now AND-ed with `tool_ctx.config.auto_compact` | ✅ | `query/src/lib.rs` (commit 3d5142b) |

### Gap 6 (discovered during audit — FIXED)
- [x] **Runtime ConfigChange propagation**: `app.auto_compact_enabled` now syncs in both `ConfigChange` and `ConfigChangeMessage` handlers in `main.rs`. The `/auto-compact` command updates the TUI footer indicator immediately in-session.

### Tests added (this session)
- [x] **27 integration tests** in `crates/cli/tests/auto_compact_integration.rs` — command toggle, config flow, threshold, debounce, footer state derivation, E2E chain (commit e5aff11)
- [x] **3 gate tests** with `GateMockProvider` in `query/src/compact.rs` — verifies provider not called when disabled/below-threshold, called when enabled+above-threshold (commit 1ec1c11)
- [x] **5 unit tests** for `/auto-compact` command in `commands/src/lib.rs` — on/off/toggle/noop/error
- [x] **All passing**: `cargo test --workspace — ~1,800 tests, 0 failures`

---

## 🟠 P1 — High

### [commands] /plan command
- [x] `PlanCommand` implemented in `session.rs`
- [x] Integrated into command registry
- [x] 4 tests registered and passing
- [x] `EnterPlanModeTool` / `ExitPlanModeTool` properly connected

### [commands] /compact command improvements
- [x] `build_conversation_transcript()` fixed for UTF-8 safety using `char_indices()`
- [x] Added `test_compact_non_ascii_messages` test with CJK, accented, and emoji characters
- [ ] Consider timeout / cancellation for long compaction requests

### [commands] /ctx-viz command
- [x] Context visualization command added
- [x] 5 tests registered and passing
- [ ] Hooks from the query loop to populate real-time token data are still needed

### [commands] /summary command
- [x] Summary command added
- [x] 6 tests registered and passing
- [ ] Verify it works with both short and long conversations in practice

### [tests] Test coverage gaps
- [x] `grep_tool.rs` — added 4 comprehensive unit tests (non-ASCII UTF-8, empty file, regex special chars, line anchors)
- [x] `glob_tool.rs` — 14 unit tests added (non-ASCII paths, empty dirs, recursive patterns, edge cases)
- [ ] Tools crate overall still has low test coverage
- [ ] Commands crate tests are many but many are basic registry checks

### [tui] Context visualization
- [x] The `context_viz.rs` module now shows a FreeProvider key health table.
  - Replaced Anthropic-only 5h/7d rate limit bars with a per-upstream key health table
  - Table shows Provider, Keys (active/total with green/yellow/red), and Retry columns
  - Data sourced from KeyRing via `key_ring_summaries()` → callback on App → polled per render frame
  - Works for all FreeProvider upstreams (Groq, Cerebras, Gemini, etc.) — not just Anthropic
  - Old Anthropic rate limit headers still populate footer display; overlay shows the broader table
  - Spec: `spec/free_provider_key_health_table.md`

---

## 🟡 P2 — Medium

### [docs] Create Cargo.toml descriptions
- [x] Added descriptions to all 10 crates: api, bridge, buddy, cli, commands, core, mcp, query, tools, tui
- [x] Fixed "Claurst" -> "Clawde" in acp and plugins crate descriptions

### [docs] Spec/documentation drift
- [x] Added historical reference banner to `spec/INDEX.md` noting specs describe the original TypeScript codebase
- [x] Created `CLAWDE_REFERENCE.md` as comprehensive developer architecture reference
- [x] Created `CLAWDE_TODO.md` as rolling improvement tracker
- [ ] `docs/` directory has good user-facing docs but no developer architecture docs (covered by CLAWDE_REFERENCE.md now)

### [ci] CI workflow restoration
- [x] Restored 3-platform matrix (ubuntu, windows, macos)
- [x] Restored path-based triggering (only `src-rust/` and `.github/workflows/ci.yml`)
- [x] Restored concurrency group with cancel-in-progress
- [x] Re-added caching for cargo registry/git/build
- [x] Added `--test-threads=1` with comment about env var mutation races
- [x] Restored clippy (Linux only, -D warnings) and rustfmt (advisory, continue-on-error)

### [branding] Claurst -> Clawde rename completeness
- [x] Renamed config directory from `~/.claurst/` to `~/.clawde/` with backward compat fallback
- [x] Renamed env var from `$CLAURST_HOME` to `$CLAWDE_HOME`
- [x] Updated all `docs/*.md`, `index.html`, `session/index.html`, `.devcontainer`, install scripts
- [x] Updated `scripts/bump-version.py`, `.github/workflows/*.yml`
- [x] Updated `npm/install.js`, `npm/package.json`
- [x] Updated all Cargo.toml references (crate names, dependencies)
- [x] Updated project-level settings scanner to check `.clawde/settings.json` first with `.claurst/` fallback
- [x] Updated `AGENTS.md` tmux session names, crate test commands, etc.
- [x] Renamed .deb references in CLAWDE_TODO.md

### [acp] ACP registry listing
- [ ] Submit the `agent.json` manifest to the official ACP registry
- [ ] Create an icon SVG for the registry listing

### [plugins] Plugin marketplace
- [ ] The `marketplace.rs` module exists but plugin discovery/installation flow may not be complete

### [tools] Computer use tool
- [ ] `computer_use.rs` exists — verify it's feature-complete for the agent to control the desktop

### [tools] Bundled skills
- [ ] `bundled_skills.rs` exists — review what skills are bundled and ensure they work

---

## 🟢 P3 — Low

### [polish] Graceful shutdown
- [x] Added SIGTERM signal handler via tokio::signal::unix that sets an Arc<AtomicBool> flag
- [x] TUI event loop checks the flag each frame; cancels in-flight streaming and sets should_exit
- [x] Fallback ctrl_c() handler for non-Unix platforms
- [x] Session state save added to cleanup path before `restore_terminal()` — fires on SIGTERM, normal exit, and all break-path exits

### [polish] Spinner styles
- [ ] `spinner.rs` in core — consider adding more spinner styles

### [polish] Theme system extension
- [x] Added catppuccin theme (Mocha palette) to `theme_colors.rs`
- [x] Added `from_json_file()` method for custom theme import from JSON files in `~/.clawde/themes/<name>.json`
  - Uses serde_json (already a TUI dep) and clawde_core::Settings::config_dir()
  - Falls back gracefully to default_theme on error
  - Validates theme name (alphanumeric + underscore only) to prevent path traversal
  - Logs warning via tracing::warn! on parse failure
- [x] Documented the custom theme JSON format below

### Custom Theme JSON Format

Create a `.json` file in `~/.clawde/themes/<name>.json` with the following structure.
Each color value is a 3-element `[r, g, b]` array with values 0-255.
Theme names must be alphanumeric (underscores allowed, no spaces or hyphens).

**Example: `~/.clawde/themes/nord_custom.json`** with Nord-inspired colors:

```json
{
  "error": [191, 97, 106],
  "success": [163, 190, 140],
  "warning": [235, 203, 139],
  "info": [136, 192, 208],
  "action": [136, 192, 208],
  "disabled": [76, 86, 106],
  "accent": [136, 192, 208],
  "secondary_accent": [191, 97, 106],
  "text_light": [236, 239, 244],
  "text_dark": [46, 52, 64],
  "border": [67, 76, 94]
}
```

**Required fields (all 11 must be present):**

| Field | Description |
|-------|-------------|
| `error` | Error messages and alerts |
| `success` | Success indicators |
| `warning` | Warning/caution messages |
| `info` | Information messages |
| `action` | Action buttons and interactive elements |
| `disabled` | Disabled or dimmed states |
| `accent` | Primary accent color |
| `secondary_accent` | Secondary accent |
| `text_light` | Text on dark backgrounds |
| `text_dark` | Text on light backgrounds |
| `border` | Borders and dividers |

Apply a custom theme with `/theme <name>`. A `tracing::warn!` log is emitted
on parse failure, and the default theme is used as fallback.

### [polish] Mouse support in TUI
- [x] Fully implemented and verified across all interaction surfaces
  - `EnableMouseCapture`/`DisableMouseCapture` wired in `setup_terminal`/`restore_terminal_cleanup`
  - `Event::Mouse(mouse)` dispatched in main.rs event loop → `App::handle_mouse_event()`
  - Config field `mouse_capture` defaults to `true`
  - Handles scroll, click-to-select dialog items, context menu hover, text selection
  - OSC-8 hyperlinks (`osc8.rs`) render clickable URLs
  - Click-outside detection via `get_active_popup_rect()` + `point_in_rect()` across 11 dialog types

### [polish] Diff viewer enhancements
- [ ] `diff_viewer.rs` — add syntax highlighting, side-by-side mode, line numbers

### [polish] Session search
- [ ] `/search` command exists but could be enhanced with full-text search across all sessions

### [polish] Keybinding presets
- [ ] Add Vim/Emacs keybinding presets

### [polish] Multi-language LSP support
- [ ] `lsp_tool.rs` / `core/src/lsp.rs` — expand language server support beyond Rust
  - Current: `rust-analyzer` configured via `make_config()`
  - Desired: Add defaults for TypeScript (`typescript-language-server` or `ts_ls`), Python (`pylsp`), Go (`gopls`), etc.
  - LSP config defaults can follow the `LspServerConfig` pattern already in `lsp.rs`
- [x] seed_with_defaults() added to LspConfig — auto-registers common language servers on startup

### [polish] Session export formats
- [ ] Support PDF and plain text export in addition to HTML/Gist share

---

## 🧪 Experimental Features

Marked as `[EXPERIMENTAL]` in the README. May be unstable or incomplete:

- `/share` — Share sessions via GitHub Gists
- `/goal` — Multi-turn goal-oriented mode
- `ultracode` — Highest effort level with sub-agent workflow
- Free mode in `/connect`
- Voice/microphone support (requires ALSA)
- Remote bridging via WebSocket/SSE

---

## Known Issues

1. **Env var mutation in tests:** Many tests mutate process-global env vars (like `HOME`, `ANTHROPIC_API_KEY`). The `EnvGuard` in `paths.rs` uses a `Mutex<()>` to serialize test access, but not all tests follow this pattern. This causes non-deterministic test failures in CI. Fix: ensure ALL env-mutating tests use `EnvGuard`.

2. **Large monorepo compilation:** The workspace has many crates depending on each other, causing long compilation times even for small changes. The `Cargo.lock` has 1000+ entries.

3. **Cargo.lock churn:** The lockfile has been modified extensively during the claurst->clawde rename. A fresh `cargo update` and `cargo generate-lockfile` may be needed to stabilize it.

4. **CI workflow restored:** The `.github/workflows/ci.yml` was restored from a single-Ubuntu check back to a 3-platform matrix (Ubuntu, macOS, Windows) with clippy + fmt checks and serial test execution (`--test-threads=1`). The original simplification lost the multi-platform coverage.

5. **Cargo.toml descriptions added:** All 10 workspace crates now have `description` fields. Updated "Claurst" -> "Clawde" in acp and plugins crate descriptions.

6. **Untracked files cleaned up:**
   - `CLAWDE_REFERENCE.md` — tracked, comprehensive architecture reference
   - `CLAWDE_TODO.md` — tracked, this improvement tracker
   - `*.deb` build artifacts — added to `.gitignore` and removed from repo

7. **Custom theme loading:** Added `from_json_file()` to `ColorPalette` in `theme_colors.rs`. Themes are loaded from `~/.clawde/themes/<name>.json`. The JSON format expects each color field to be a 3-element `[r, g, b]` array. Name is sanitized to alphanumeric characters only. On parse failure, falls back silently to `default_theme` with a `tracing::warn!` log.
