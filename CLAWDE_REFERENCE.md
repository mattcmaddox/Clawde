# Clawde — Architecture Reference

## Overview

Clawde is an **open-source, multi-provider terminal coding agent** built from scratch in Rust. It
reimplements the behavior of Claude Code (from its leaked sourcemap spec) as a clean-room Rust
reimplementation. It features a TUI pair programmer, multi-provider support, plugin system,
chat forking, memory consolidation, sub-agent delegation, and much more.

**Version:** 0.1.7  
**License:** GPL-3.0  
**Repository:** https://github.com/mattcmaddox/Clawde  
**Binary:** `clawde` (formerly `claurst`)  
**Config directory:** `~/.clawde/` (legacy: `~/.claurst/` fallback)

---

## Crate Architecture (Layer-Cake)

The project is organized as a Rust workspace in `src-rust/` with 12 crates:

```
                     ┌──────────┐
                     │    cli   │  Entry point / CLI argument parsing
                     ├──────────┤
         ┌───────────┤  tui     ├──────────┐
         │           ├──────────┤           │
         ▼           │ commands │           ▼
   ┌──────────┐      ├──────────┤      ┌──────────┐
   │  tools   │      │  query   │      │   acp    │
   └────┬─────┘      ├──────────┤      └────┬─────┘
        │            │  bridge  │            │
        │            ├──────────┤            │
        ▼            ▼          ▼            ▼
   ┌──────────────────────────────────────────┐
   │                  core                     │
   │     (config, auth, storage, types)         │
   ├──────────────────────────────────────────┤
   │            api / mcp / plugins            │
   └──────────────────────────────────────────┘
```

### Crate Dependency Graph (simplified)

```
cli ──┬── acp ──┬── api ──┬── core
      │         ├── mcp    │
      │         ├── query ─┤
      │         ├── tools ─┤
      │         ├── plugins│
      │         └── core   │
      ├── api ─────────────┤
      ├── tools ───────────┤
      ├── commands ──┬─────┤
      │              ├── bridge
      │              ├── tui
      │              └── query
      ├── tui ──┬── core
      │         └── query
      ├── bridge
      └── core
```

---

## Crate-by-Crate Breakdown

### 1. `core` (5,500+ lines) — Foundation

**File:** `src-rust/crates/core/src/lib.rs` + 50+ modules

The largest and most fundamental crate. Provides all shared types, configuration, storage,
authentication, and utility infrastructure.

**Key modules:**

| Module | Purpose |
|--------|---------|
| `lib.rs` | Central re-export hub. Re-exports `Settings`, `Config`, `Message`, `ContentBlock`, `Role`, `ProviderId`, `PermissionManager`, `CostTracker`, etc. |
| `paths.rs` | Config directory resolution. `clawde_home()` -> `Settings::config_dir()`. Precedence: `$CLAWDE_HOME` > `~/.clawde/` > `~/.claurst/` (legacy) > XDG |
| `provider_id.rs` | ~50 `ProviderId` constants (`ANTHROPIC`, `OPENAI`, `GOOGLE`, etc.) as branded string newtypes |
| `settings_sync.rs` | Settings sync with claude.ai: keys `SYNC_KEY_USER_SETTINGS`, `SYNC_KEY_USER_MEMORY` |
| `keybindings.rs` | Configurable keybinding system. `KeyContext`, `ParsedKeystroke`, `Chord`, `ParsedBinding` |
| `auth_store.rs` | API key storage and retrieval across providers |
| `session_storage.rs` | `SqliteSessionStore`, `ConversationSession`, `SessionSummary` |
| `sqlite_storage.rs` | SQLite-backed session persistence |
| `effort.rs` | `EffortLevel` enum: `None` < `Minimal` < `Low` < `Medium`(default) < `High` < `XHigh` < `Max` |
| `permission.rs` | `PermissionMode`, `PermissionHandler` trait, `PermissionManager` |
| `snapshot/` | Git snapshot types: `FileDiff`, `FileStatus`, `Patch`, shadow snapshots |
| `token_budget.rs` | Token budget tracking and estimation |
| `truncate.rs` | Message truncation/compaction for context window management |
| `feature_flags.rs` | Feature gate system |
| `system_prompt.rs` | System prompt assembly |
| `claudemd.rs` | Clawde's markdown dialect parsing/rendering |
| `git_utils.rs` | Git operations (status, diff, worktree) |
| `update_check.rs` | GitHub release update checking |
| `share_export/` | HTML/CSS/JS templates for sharing sessions as GitHub Gists |
| `voice.rs` | Voice/microphone capture (requires `libasound2-dev`) |
| `memdir.rs` | Long-term memory directory management |
| `goal.rs` | `/goal` command types: `Goal`, `GoalStatus`, `GoalStore` |
| `attachments.rs` | File attachment handling |
| `crypto_utils.rs` | Encryption utilities |
| `oauth_config.rs`, `device_code.rs`, `codex_oauth.rs` | OAuth flow for providers |

**Key types re-exported:**
- `Role` (enum: `User`, `Assistant`, `System`, `ToolResult`)
- `ContentBlock` (enum: `Text`, `ToolUse`, `ToolResult`, `Thinking`)
- `Message` (struct: `role`, `content: Vec<ContentBlock>`)
- `UsageInfo` (struct: `input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`)
- `MessageCost` (struct)
- `Settings` (struct: top-level config)
- `Config` (struct: merged settings)
- `PermissionMode` (enum), `PermissionHandler` (trait), `PermissionManager`
- `EffortLevel` (enum)
- `ProviderId` (newtype + constants)
- `ModelId` (type alias)
- `clawde_home()` -> `PathBuf`

---

### 2. `api` (1,700 lines) — Provider Abstraction

**File:** `src-rust/crates/api/src/lib.rs` + 20+ modules

Multi-provider LLM API abstraction layer.

**Key modules:**

| Module | Purpose |
|--------|---------|
| `provider.rs` | `LlmProvider` trait — the core abstraction for all LLM providers |
| `auth.rs` | `AuthProvider` trait |
| `registry.rs` | Provider registry — maps `ProviderId` -> `Arc<dyn LlmProvider>` |
| `model_registry.rs` | `ModelRegistry` — canonical model IDs with metadata (context window, capabilities) |
| `providers/` | Concrete provider implementations: `anthropic.rs`, `openai.rs`, `google.rs`, `bedrock.rs`, `azure.rs`, `cohere.rs`, `minimax.rs`, `codex.rs`, `copilot.rs`, `free.rs`, `openai_compat.rs` |
| `providers/openai_compat_providers.rs` | All OpenAI-compatible provider configurations (OpenRouter, Groq, DeepSeek, etc.) |
| `stream_parser.rs` | SSE stream parsing for streaming responses |
| `provider_types.rs` | Unified provider request/response types |
| `provider_error.rs` | Standardized error types across providers |
| `error_handling.rs` | Provider-aware error handling |
| `transform.rs` | Message transformation trait |
| `transformers/` | Wire-format transformers: `anthropic.rs`, `openai_chat.rs` |
| `protocol/` | Low-level protocol types: `openai_chat.rs` |
| `effort_support.rs` | Effort-to-reasoning-budget mapping |
| `variants.rs` | Opencode variant support |
| `bun_tls.rs` | Bun TLS integration |
| `codex_adapter.rs` | Codex (Claude Code's API) adapter |

**Provider architecture pattern:**
1. Each provider implements `LlmProvider` trait (request shaping, response parsing, streaming)
2. Providers registered in `registry.rs`
3. Model metadata in `model_registry.rs`
4. Auth in `auth_store.rs` (core crate) + `auth.rs` (api crate)

**Supported providers (50+):**
Anthropic, OpenAI, Google/Gemini, Google Vertex, AWS Bedrock, Azure, GitHub Copilot,
OpenAI-compatible (OpenRouter, Groq, DeepSeek, Together, Perplexity, Fireworks, etc.),
Cohere, MiniMax, Ollama, LM Studio, llama.cpp, Free tier, Codex, Copilot, and more.

---

### 3. `cli` (5,000+ lines) — Entry Point

**Files:** `src-rust/crates/cli/src/main.rs`, `oauth_flow.rs`, `codex_oauth_flow.rs`, `upgrade.rs`

The binary entry point. Uses `clap` for argument parsing.

**Modes of operation:**
- **Interactive REPL mode:** Full TUI (ratatui)
- **Headless mode (`--print` / `-p`):** Single query to stdout
- **ACP mode (`acp` subcommand):** JSON-RPC 2.0 over stdio for editor integration
- **Auth commands:** `auth login`, `auth logout`
- **Upgrade:** Self-updating via GitHub releases

**Key flow:**
1. Parse CLI args (clap)
2. Load config from `settings.json`
3. Build context (git status, AGENTS.md)
4. Initialize permission manager, cost tracker
5. Launch TUI or headless mode

---

### 4. `commands` (2,500 lines) — Slash Commands

**File:** `src-rust/crates/commands/src/lib.rs` + 30+ command modules

All `/` slash commands for the interactive TUI.

**Core trait:**
```rust
trait SlashCommand {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn aliases(&self) -> Vec<&str>;
    fn execute(&self, ctx: &CommandContext) -> CommandResult;
}
```

**Key struct:**
```rust
struct CommandContext {
    config: Arc<Config>,
    cost_tracker: Arc<CostTracker>,
    messages: Vec<Message>,
    working_dir: PathBuf,
    session_id: String,
    // optional: McpManager, auth handlers
}
```

**Commands (30+):**

| Category | Commands |
|----------|----------|
| Session | `/clear`, `/rewind`, `/forget`, `/new`, `/summary` |
| Config | `/settings`, `/config`, `/connect`, `/auth` |
| Providers | `/provider`, `/model`, `/effort`, `/retry` |
| Tools | `/permission`, `/review`, `/doctor` |
| Memory | `/memory`, `/forget` |
| Sharing | `/share`, `/export` |
| Debug | `/stats`, `/usage`, `/history` |
| Agent | `/agent`, `/managed`, `/goal` |
| Plugin | `/plugin`, `/hook` |
| Other | `/help`, `/copy`, `/teleport`, `/search` |

**Registration pattern:** Commands are registered in a `Vec<Box<dyn SlashCommand>>` via
`fn all_commands() -> Vec<Box<dyn SlashCommand>>`. Found by name via `find_command()`.

---

## Arg Completion System

The arg completion system provides inline typeahead for slash command arguments —
type `/effort ` (with space) and see available options with availability highlighting,
arrow-key navigation, and Enter-to-select.

### Architecture

```
User types /<cmd> <partial> in prompt
           │
           ▼
PromptInputState::update_suggestions()
  │  calls compute_slash_suggestions()
  │  which detects space → delegates to arg_completions_fn
           │
           ▼
App::arg_completions: Option<Arc<dyn Fn + Send + Sync>>
  │  (set by cli/src/main.rs to avoid circular dep)
           │
           ▼
clawde_commands::get_arg_completions(cmd_name, partial)
  │  uses OnceLock-cached all_commands() list
  │  finds command → calls cmd.arg_completions(partial)
  │  filters by partial prefix (case-insensitive)
           │
           ▼
Vec<ArgCompletion> → Vec<TypeaheadSuggestion>
  │  text: "/cmd val", arg_value: Some("val")
  │  faded: !available (dimmed, unselectable)
           │
           ▼
render_prompt_suggestions() in render.rs
  │  TypeaheadSource::ArgCompletion: uses arg_value
  │  faded items: DarkGray + DIM
```

### Key Types

| Type | Location | Purpose |
|------|----------|---------|
| `ArgCompletion` | `commands/src/lib.rs` | `{ value, description, available }` — argument option |
| `arg_completions(partial)` | `SlashCommand` trait | Default: empty vec. Override per-command |
| `get_arg_completions(cmd, partial)` | `commands/src/lib.rs` | Public helper with `OnceLock` caching + filtering |
| `TypeaheadSuggestion` | `tui/src/prompt_input.rs` | `{ text, description, source, faded, arg_value }` — renderable suggestion |
| `TypeaheadSource::ArgCompletion` | `tui/src/prompt_input.rs` | Variant for argument-level suggestions |
| `App::arg_completions` | `tui/src/app.rs` | `Option<Arc<dyn Fn + Send + Sync>>` — closure bridge from CLI layer |

### Commands with Arg Completions

| Command | Completions | Source File |
|---------|-------------|-------------|
| `/effort` | none, minimal, low, medium, high, xhigh, max, ultracode | `session_tools.rs` |
| `/auto-compact` | on, off | `lib.rs` |
| `/theme` | default, dark, light, catppuccin | `appearance.rs` |
| `/output-style` | (dynamic — from disk, OnceLock-cached) | `appearance.rs` |
| `/diff` | --stat, --staged | `lib.rs` |
| `/agent` | (built-in visible agents, OnceLock-cached) | `providers.rs` |
| `/model` | (~4500 model IDs from bundled snapshot, OnceLock-cached) | `lib.rs` |
| `/managed-agents` | status, presets, preset, setup, configure, enable, disable, reset, budget | `managed_agents.rs` |

### Circular Dependency Resolution

The TUI crate (`clawde-tui`) defines `TypeaheadSuggestion` but the commands crate
(`clawde-commands`) defines `ArgCompletion`. The commands crate already depends on
the TUI crate (for `HelpEntry`), so the TUI crate cannot import from commands.

**Solution:** The `App` struct holds `arg_completions: Option<Arc<dyn Fn(...) + Send + Sync>>`
which is set from `cli/src/main.rs` at startup. The CLI layer imports both crates,
converts `ArgCompletion` → `TypeaheadSuggestion`, and sets the closure on the app.

### Adding Arg Completions to a Command

1. Override `arg_completions(&self, partial: &str)` in the `SlashCommand` impl
2. Return a `Vec<ArgCompletion>` with `value` (the argument text), `description`,
   and `available` (set false for options not available in current context)
3. For dynamic lists (models, styles, agents): use `OnceLock` to cache the first
   computation. The filtering by user-typed prefix happens automatically in
   `get_arg_completions()`
4. Add unit tests in the test module covering: empty partial, filtering, case
   insensitivity, and completeness (all expected completions present)

### Rendering

In `tui/src/render.rs` (`render_prompt_suggestions`), `TypeaheadSource::ArgCompletion`:
- **Label**: Uses `suggestion.arg_value.as_deref().unwrap_or(&suggestion.text)`
  (previously a fragile `split_whitespace().nth(1)` hack)
- **Dimmed**: Faded items (`available: false`) get `Color::DarkGray + Modifier::DIM`
  and cannot be selected via arrow keys
- **Description**: Shown beside the label in the remaining space

### Selection

In `tui/src/prompt_input.rs` (`accept_suggestion`), when the source is `ArgCompletion`:
- The entire `suggestion.text` replaces the prompt input (e.g. `/effort medium`)
- Faded items are silently ignored (arrow keys skip over them)

---

### 4. `commands` (2,500 lines) — Slash Commands (continued)

**File:** `src-rust/crates/query/src/lib.rs` + 12 modules + `runner/` subdirectory

The core agent loop. Manages the turn-based interaction between the user and the LLM.

**Key modules:**

| Module | Purpose |
|--------|---------|
| `coordinator.rs` | `AgentMode` enum (`Coordinator`, `Worker`, `Normal`), coordinator-only/banned tools |
| `managed_orchestrator.rs` | Managed agent mode system prompt and orchestration |
| `runner/` | Query execution sub-crate |
| `runner/mod.rs` | Runner module exports |
| `runner/prompt.rs` | System prompt assembly for each turn |
| `runner/provider_options.rs` | Provider options assembly (effort -> reasoning budget) |
| `runner/single.rs` | Single-shot (non-agentic) queries |
| `runner/stream.rs` | Provider stream event -> Anthropic event shape mapping |
| `runner/tools.rs` | Tool execution: argument parsing, permission gating, execution |
| `runner/tool_budget.rs` | Tool result budgeting during compaction |
| `runner/hooks.rs` | Post-sampling and stop hooks |
| `continuation.rs` | `ContinuationDecision`, `ContinuationMode`, `StopPolicy`, `TurnEndContext` |
| `compact.rs` | Conversation compaction: `compact_conversation()`, `estimate_context_tokens()`, `should_auto_compact()` |
| `goal_loop.rs` | `/goal` loop: `check_and_continue_goal()`, `decide_goal_continuation()` |
| `context_analyzer.rs` | Context analysis for smart compaction decisions |
| `session_memory.rs` | Memory extraction: `ExtractedMemory`, `SessionMemoryExtractor` |
| `auto_dream.rs` | Background memory consolidation during idle time |
| `away_summary.rs` | Away/summary generation |
| `agent_tool.rs` | Sub-agent spawning (`init_team_swarm_runner`, `AgentTool`) |
| `command_queue.rs` | `CommandQueue`, `QueuedCommand`, `CommandPriority` |
| `cron_scheduler.rs` | Cron job scheduling for periodic tasks |
| `sanitize.rs` | Input sanitization |
| `skill_prefetch.rs` | Skill indexing and prefetching |

**Agent loop (simplified):**
1. User sends a message
2. System prompt + conversation history assembled
3. Provider API called (streaming)
4. Response parsed for tool calls
5. Tools executed with permission gating
6. Tool results fed back to provider
7. Repeat until `StopPolicy` triggers or user interrupts

---

### 6. `tools` (970 lines lib.rs + 40+ tool modules) — Tool Implementations

**File:** `src-rust/crates/tools/src/lib.rs` + 42 source files

All tools that the agent can invoke (like Bash, Read, Write, Edit, WebSearch, etc.).

**Registration pattern:** Tools are declared as modules and re-exported. Each tool is a struct
implementing a `Tool` trait (defined in core or locally).

**Tool categories:**

| Category | Tools |
|----------|-------|
| **File** | `FileReadTool`, `FileWriteTool`, `FileEditTool`, `ApplyPatchTool`, `BatchEditTool` |
| **Search** | `GlobTool`, `GrepTool`, `ToolSearchTool` |
| **Shell** | `PtyBashTool`, `PowershellTool`, `ReplTool` |
| **Web** | `WebSearchTool`, `WebFetchTool` |
| **Agent** | `AgentTool` (spawn sub-agents), `TeamTool` (create swarms), `SendMessageTool` |
| **Task** | `TaskTool`, `TaskOutputTool`, `TaskStopTool`, `CronTool` |
| **Plan** | `EnterPlanModeTool`, `ExitPlanModeTool` |
| **MCP** | `McpResourcesTool`, `McpAuthTool` |
| **Dev** | `LspTool`, `NotebookEditTool`, `ComputerUseTool`, `MonitorTool` |
| **Meta** | `BundledSkillsTool`, `SkillTool`, `SleepTool` |
| **Config** | `ConfigTool`, `TodoWriteTool`, `GoalCompleteTool`, `AskUserTool`, `BriefTool` |
| **Remote** | `RemoteTriggerTool` |
| **Formatter** | `FormatterTool` |
| **Support** | `TestSupportTool`, `SyntheticOutputTool`, `WorktreeTool`, `LineEndingsTool` |

**Tool trait (conceptual — verify actual signature in `tools/src/lib.rs`):**
```rust
trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    // Actual signature may be async and use different context/result types
    fn execute(&self, input: &str, ctx: &ToolContext) -> ToolResult;
}
```

**Key test pattern:** Tools have registry validation tests:
- `test_all_tools_non_empty()`
- `test_all_tools_have_unique_names()`
- `test_all_tools_have_non_empty_descriptions()`
- `test_all_tools_have_valid_input_schema()`

---

### 7. `tui` (1,600 lines lib.rs + 50+ modules) — Terminal UI

**File:** `src-rust/crates/tui/src/lib.rs` + 50+ source files

Ratatui-based terminal user interface.

**Key components:**

| Component | Purpose |
|-----------|---------|
| `app.rs` | Main application state machine |
| `render.rs` | Rendering loop |
| `input.rs` | Input handling (keyboard, bracketed paste) |
| `prompt_input.rs` | Prompt input area |
| `messages/` | Message rendering: `markdown.rs`, `markdown_enhanced.rs` |
| `transcript_turn.rs` | Single conversation turn display |
| `dialogs.rs` | Dialog system |
| `diff_viewer.rs` | File diff viewer |
| `context_viz.rs` | Context/token usage visualization |
| `settings_screen.rs` | Settings UI |
| `model_picker.rs` | Model selection dialog |
| `effort_picker.rs` | Effort level selector |
| `session_browser.rs` | Session history browser |
| `theme_colors.rs` | Theme/color system |
| `rustle.rs` | Rustle companion character |
| `voice_capture.rs` | Voice recording UI |
| `session_branching.rs` | Chat fork/branch UI |
| `mcp_view.rs` | MCP server management view |
| `paste_viewer.rs` | Paste content viewer |

**TUI structure:**
```
┌─────────────────────────────────────┐
│ Header (model, context, cost)       │
├─────────────────────────────────────┤
│                                     │
│  Conversation Transcript            │
│  (virtual scrolling)                │
│                                     │
├─────────────────────────────────────┤
│ Prompt Input (multi-line, editable) │
├─────────────────────────────────────┤
│ Footer (keybindings, status)        │
└─────────────────────────────────────┘
```

---

### 8. `acp` (90 lines lib.rs + 6 modules) — Agent Client Protocol

**File:** `src-rust/crates/acp/src/lib.rs` + `connection.rs`, `permission.rs`, `prompt.rs`,
`runtime.rs`, `server.rs`, `sessions.rs`

JSON-RPC 2.0 server for editor integration (Zed, Neovim, etc.).

**Protocol methods:**
- `initialize` — capability negotiation
- `session/new` — create a new agent session
- `session/prompt` — send a prompt to the session
- `session/cancel` — cancel the current generation

**Notifications:**
- `session/update` — text deltas, thinking, tool calls with progress
- `session/request_permission` — tool permission approval dialog

**Registry template:** `src-rust/crates/acp/registry-template/agent.json`

---

### 9. `mcp` (2,000 lines) — Model Context Protocol

**File:** `src-rust/crates/mcp/src/lib.rs` + `backend.rs`, `connection_manager.rs`,
`oauth.rs`, `registry.rs`, `rmcp_backend.rs`

MCP client for connecting to MCP servers that expose tools and resources.

---

### 10. `plugins` (700 lines) — Plugin System

**File:** `src-rust/crates/plugins/src/lib.rs` + `plugin.rs`, `hooks.rs`, `manifest.rs`,
`loader.rs`, `marketplace.rs`, `registry.rs`

Plugin runtime for extending Clawde with new commands, tools, and hooks.

---

### 11. `bridge` (1,700 lines) — Remote Bridge Protocol

**File:** `src-rust/crates/bridge/src/lib.rs`

WebSocket/SSE-based bridging for cloud-synced sessions and IDE connectivity.

**`buddy` (1,100 lines)** — Complementary helper crate for the bridge.

---

## Key Design Patterns

### 1. Provider Abstraction
All LLM providers implement `LlmProvider` trait. The `Registry` maps provider IDs to instances.
`ModelRegistry` holds capability metadata. Pipeline: `request -> transform -> send -> parse -> emit`

### 2. Tool Registration
Tools are standalone structs registered in a central `Vec<Arc<dyn Tool>>`. Each tool has
a JSON Schema input definition. Permission gating is handled by `PermissionManager`.

### 3. Command Registration
Commands implement `SlashCommand` trait. Registered in `all_commands()`. Lookup by name or alias.

### 4. Permission System
`PermissionMode` controls auto-approve vs interactive approval. `PermissionHandler` trait allows
custom approval UIs. `PermissionManager` stores rules persistently.

### 5. Config Directory Resolution
```rust
$CLAWDE_HOME (if set)
  -> ~/.clawde/ (if exists)
    -> ~/.claurst/ (legacy, if exists)
      -> $XDG_CONFIG_HOME/clawde/ or ~/.config/clawde/
```

### 6. Effort Levels
`EffortLevel` enum ordered from `None` to `Max`. Maps to provider-specific reasoning budgets.
Default is `Medium`.

---

## Auto-Compact System (Gap 1–6)

The auto-compact system automatically compresses conversation history when the context
window fills up, keeping the agent functional in long sessions.  It spans 6 crates
and was implemented across 6 gaps.

### Architecture Overview

```
settings.json                 User toggles /auto-compact
      │                              │
      ▼                              ▼
Settings::effective_config()   AutoCompactCommand
      │                         (commands crate)
      ▼                              │
   Config                          ConfigChangeMessage
      │                              │
      ▼                              ▼
run_query_loop               main.rs event loop
  (query crate)                 app.auto_compact_enabled
      │                              │
      ├─ config.auto_compact gate    │
      │                              │
      ├─ compact_provider                     │
      │   (Arc<dyn LlmProvider>)              │
      │                                       │
      ├─ auto_compact_if_needed()             │
      │   ├─ should_auto_compact (threshold)  │
      │   ├─ debounce (turn-gap + time-gap)   │
      │   └─ compact_conversation()           │
      │       └─ summarise_head() → API call  │
      │                                       │
      └─ TokenWarning → QueryEvent ──────────►│
                                              ▼
                                        render_footer()
                                          (tui crate)
                                          ctx: N%  green/yellow/red
```

### Config Flow

**`Settings::auto_compact`** (serde default `true`, key `"autoCompact"`)
→ merged into `Config::auto_compact` via `effective_config()` in `core/src/lib.rs`:
```rust
config.auto_compact = self.auto_compact || config.auto_compact;
```

The `||` ensures the top-level setting takes precedence while respecting an explicit `true`
in the nested config block.

**Runtime propagation** (`cli/src/main.rs`):
- Startup: `app.auto_compact_enabled = live_config.auto_compact;` (line ~1940)
- ConfigChange/ConfigChangeMessage handlers: `app.auto_compact_enabled = applied_cfg.auto_compact;` (Gap 6 fix)

### Command: `/auto-compact`

**Crate:** `commands`, **File:** `src/lib.rs`  
**Struct:** `AutoCompactCommand`, **Name:** `"auto-compact"`, **Alias:** `"autocompact"`  
**Category:** `"AI & Thinking"`

| Argument | Behavior |
|----------|----------|
| `on` / `enable` / `1` / `true` | Enable auto-compact, persist to settings.json |
| `off` / `disable` / `0` / `false` | Disable auto-compact, persist to settings.json |
| _(no args)_ | Toggle current state |

Returns `CommandResult::ConfigChangeMessage(new_config, msg)` so the runtime
`app.auto_compact_enabled` is synced immediately (Gap 6).

**Tests (5):** `commands/src/lib.rs` tests module
- `auto_compact_command_toggle_on_from_off`
- `auto_compact_command_toggle_off_from_on`
- `auto_compact_command_toggle_no_args_flips_state`
- `auto_compact_command_noop_when_already_in_state`
- `auto_compact_command_rejects_unknown_arg`

### Query Loop Gate (Gap 3)

**File:** `query/src/lib.rs`, **Function:** `run_query_loop`

The entire compact_provider block (reactive collapse, reactive compact, *and* proactive
auto-compact) is gated behind:
```rust
if tool_ctx.config.auto_compact {
    // all compaction logic
}
```
When `false`, no compaction runs — the user is responsible for manual `/compact`.

### Provider Abstraction (Gap 2)

**File:** `query/src/compact.rs`

All compaction functions use `&dyn LlmProvider` (not `&AnthropicClient`), enabling
auto-compact with any configured provider (OpenAI, Gemini, etc.):

| Function | Signature |
|----------|-----------|
| `auto_compact_if_needed` | `&dyn LlmProvider, &[Message], u64, &str, u64, &mut AutoCompactState` |
| `compact_conversation` | `&dyn LlmProvider, &[Message], &str` |
| `summarise_head` | `&dyn LlmProvider, &[Message], usize, &str, u32` (private) |
| `reactive_compact` | `&dyn LlmProvider, Vec<Message>, &QueryConfig, CancellationToken, &[PathBuf]` |
| `context_collapse` | `&dyn LlmProvider, Vec<Message>, &QueryConfig` |
| `micro_compact_if_needed` | `&dyn LlmProvider, &[Message], u64, &str, &MicroCompactConfig` |

Non-streaming `provider.create_message(ProviderRequest)` replaces the old
`client.create_message_stream()` + `StreamAccumulator` pattern. A local helper
`compact_text_from_blocks()` extracts text from the response.

**Compact provider resolution** (`query/src/lib.rs`, before the loop):
1. Try `config.provider_registry` first (non-Anthropic providers)
2. Fall back to building a fresh `ProviderRegistry` from config
3. Result is `Option<Arc<dyn LlmProvider>>` — gracefully skipped when `None`

### Debounce / Hysteresis (Gap 5)

**File:** `query/src/compact.rs`, **Struct:** `AutoCompactState`

Prevents compaction thrashing when context hovers near the 90 % threshold:

| Guard | Value | Description |
|-------|-------|-------------|
| Turn gap | 5 turns | Minimum turns between compactions |
| Time gap | 60 seconds | Minimum wall-clock time between compactions |

**First compaction exemption:** Debounce only gates AFTER the first compaction
has occurred (`if let Some(last) = state.last_compact_at`). The first compaction
fires immediately at threshold.

**State fields:**
```rust
pub struct AutoCompactState {
    pub compaction_count: u32,           // total compactions this session
    pub consecutive_failures: u32,       // resets on success
    pub disabled: bool,                  // circuit breaker (≥3 consecutive failures)
    pub turns_since_last_compact: u32,   // debounce counter
    pub last_compact_at: Option<Instant>, // debounce timestamp
}
```

### TUI Footer Indicator (Gap 4)

**File:** `tui/src/render.rs`, **Function:** `render_footer` (line ~2837)

Displays context usage in the bottom-right of the footer with color thresholds:

| Usage | Auto-Compact ON | Auto-Compact OFF |
|-------|-----------------|------------------|
| < 70 % | `ctx: N%` (green) | `ctx: N% (off)` (dim gray) |
| 70–84 % | `ctx: N%` (yellow) | `ctx: N% (off)` (yellow) |
| 85–94 % | `ctx: N% — compact soon` (yellow bold) | _(same, urgency overrides state)_ |
| ≥ 95 % | `ctx: N% — /compact now` (red bold) | _(same, urgency overrides state)_ |

**Dependencies:** Reads `app.auto_compact_enabled`, `app.context_used_tokens`,
and `app.context_window_size` — all refreshed per-frame. An update-available
notification takes priority over the context display when context is below 85 %.

### Token Warning Events

**File:** `query/src/lib.rs`

The query loop emits `QueryEvent::TokenWarning { state: TokenWarningState, pct_used }`
when usage crosses 80 % (Warning) or 95 % (Critical). The TUI app handler in
`app.rs` (line ~7162) pushes a notification banner and tracks thresholds to avoid
repeating the same warning level.

### Key Files Summary

| File | Gap | What |
|------|-----|------|
| `core/src/lib.rs` | G3 | `effective_config()` merge of `auto_compact` |
| `query/src/compact.rs` | G2, G5 | Provider-agnostic compaction, debounce, `AutoCompactState` |
| `query/src/lib.rs` | G1, G3 | Loop wiring, config gate, compact_provider resolution |
| `commands/src/lib.rs` | G3, G6 | `AutoCompactCommand`, tests |
| `tui/src/render.rs` | G4 | Footer context indicator with color thresholds |
| `cli/src/main.rs` | G6 | Runtime ConfigChange sync for `auto_compact_enabled` |

---

## Major Conventions

### Coding Rules (from AGENTS.md)
- No `.unwrap()` / `.expect()` in production (tests OK)
- Avoid speculative `.clone()` — borrow first
- No `unsafe` without `// SAFETY:` comment
- No type erasure (`Box<dyn Any>`, `serde_json::Value` through typed boundaries)
- Keybindings must flow through `keybindings.rs`, not hardcoded
- Never modify generated files (`Cargo.lock`, `npm/package.json` version)
- Crate convention: `clawde-<name>`

### Naming
- Crates: `clawde-core`, `clawde-api`, `clawde-cli`, etc.
- Provider IDs: `pub const ANTHROPIC: &str = "anthropic";`
- Config dir: `$CLAWDE_HOME`, `~/.clawde/`, legacy `~/.claurst/`
- Binary: `clawde`

### Testing Patterns
- Tests in `lib.rs` at the bottom of the file using `#[cfg(test)] mod tests { ... }`
- Async tests with `#[tokio::test]`
- Registry validation tests for commands and tools
- `#[test] fn test_<name>()` naming convention
- Run with `cargo test --workspace -- --test-threads=1` (serial due to env var mutation)

### Build Commands
```bash
# From src-rust/
cargo check --workspace              # Quick compilation check
cargo clippy --workspace --all-targets -- -D warnings  # Lint
cargo fmt --all                      # Format
cargo test --workspace -- --test-threads=1              # Test (serial!)
cargo test --package clawde-core                        # Single crate
cargo build --release --package clawde-cli              # Release build
```

---

## Architecture Diagrams

### Request/Response Flow
```
User Input
    │
    ▼
CLI/main.rs ──► TUI (ratatui)
    │                 │
    │                 ▼
    │         Commands (/) ──► Immediate action
    │                 │
    │                 ▼
    │         Query Loop
    │         ┌──────────────┐
    │         │ System Prompt│
    │         │ + History    │────► API (provider)
    │         │              │◄──── Streaming response
    │         │ Tool Exec    │
    │         └──────────────┘
    │                 │
    ▼                 ▼
Headless mode    Output to user
```

### Crate Dependency (upside-down view)
*(Simplified — actual edges are more detailed. The `Cargo.toml` of each crate is the source of truth.)*
```
          clawde-cli
         /    |     \
   clawde-acp  clawde-tui  clawde-commands
    /    |        |    \        |
clawde-api  clawde-query  clawde-tools
   |       /    |    \       |
   +-- clawde-core ---------+
   |       |
clawde-mcp  clawde-plugins

Bridge and Buddy are additional deps of commands and cli respectively.
```

---

## State Management

### Session State
- SQLite-backed (via `rusqlite`) 
- `SqliteSessionStore` manages persistence
- `ConversationSession` is the in-memory representation
- Sessions can be forked/branched

### Configuration State
- `settings.json` in config directory
- `Settings` struct (user config) -> `Config` (merged with defaults)
- `ProjectSettings` from `.clawde/settings.local.json` in project roots
- Environment variables override file settings

### Memory
- Long-term memory via `memdir/` in config directory
- Auto-consolidation during idle (`auto_dream`)
- Session memory extraction (`SessionMemoryExtractor`)
- Team memory sync (`team_memory_sync`)

### Cost Tracking
- `CostTracker` tracks per-session token usage and costs
- Reports via `/stats` and footer display

---

## Quick-Start Guides for Common Tasks

### Adding a New Provider
1. Add `ProviderId` constant in `core/src/provider_id.rs`
2. Create provider adapter in `api/src/providers/<name>.rs` implementing `LlmProvider`
   - OpenAI-compatible: add one line to `openai_compat_providers.rs`
   - Custom wire format: mirror `anthropic.rs` or `google.rs` structure
3. Register in `api/src/registry.rs`
4. Add model metadata in `api/src/model_registry.rs`
5. Wire auth in `core/src/auth_store.rs` (env var, OAuth, etc.)
6. Add tests with mocked HTTP fixtures
7. Document in `docs/providers.md` and `README.md`

### Adding a New Tool
1. Create `<name>.rs` in `tools/src/` with a struct implementing the `Tool` trait
2. Add `pub mod <name>; pub use <name>::<Name>Tool;` in `tools/src/lib.rs`
3. The tool is auto-discovered by the registry validation tests
4. Add unit tests following the `test_<name>_*` pattern in `tools/src/lib.rs`

### Adding a New Slash Command
1. Create a command module or add to an existing one in `commands/src/`
2. Implement `SlashCommand` trait with `name()`, `description()`, `aliases()`, `execute()`
3. Register in `all_commands()` in `commands/src/lib.rs`
4. Add tests for registry acceptance and command execution

### Adding a New Provider (OpenAI-Compatible)
1. Add entry to `openai_compat_providers.rs` — one line with base URL
2. Add `ProviderId` constant
3. Add model entries to `model_registry.rs`

---

## Key Technical Details

### UTF-8 Handling
- Use `.char_indices()` for safe string truncation — never byte-slice `&str` directly
- Build conversation transcripts with `build_conversation_transcript()` using `char_indices()`

### Error Handling
- `thiserror` for library errors
- `anyhow` for application-level errors
- `ProviderError` for provider-specific errors (standardized across providers)

### Concurrency
- `tokio` async runtime
- `Arc` for shared state
- `DashMap` for concurrent maps
- `parking_lot::Mutex` for guarded state

### CLI Argument Parsing
- `clap` with derive API
- Flags: `--print`, `--model`, `--provider`, `--effort`, `--max-turns`, etc.

---

## Provider ID Constants

All defined in `crates/core/src/provider_id.rs`:

**Major:** `ANTHROPIC`, `OPENAI`, `GOOGLE`, `GOOGLE_VERTEX`, `AMAZON_BEDROCK`, `AZURE`, `GITHUB_COPILOT`

**OpenAI-Compatible:** `DEEPINFRA`, `CEREBRAS`, `COHERE`, `TOGETHER_AI`, `PERPLEXITY`, `OPENROUTER`,
`CLOUDFLARE`, `HUGGINGFACE`, `NVIDIA`, `SILICONFLOW`, `NEBIUS`, `OVHCLOUD`, `SCALEWAY`, `VULTR`,
`BASETEN`, `FRIENDLI`, `FIREWORKS`, `NOVITA`, `MISTRAL`, `XAI`, `GROQ`, `DEEPSEEK`

**International:** `MOONSHOT`, `ZHIPU`, `ZAI`, `UPSTAGE`, `STEPFUN`, `MINIMAX`

**Local:** `OLLAMA`, `LM_STUDIO`, `LLAMA_CPP`

**Other:** `CROF`, `GITLAB`, `SAP`, `SAMBANOVA`, `CODEX`, `OPENCODE_GO`, `OPENCODE_ZEN`,
`NEURALWATT`, `VENICE`, `ROUTING`, `SYNTHETIC`

---

## File Sizes (indicative)

| Crate | Lines (entry point) | Total files |
|-------|---------------------|-------------|
| core | 5,500 | 55+ |
| cli | 5,000 | 5 |
| query | 3,300 | 16 |
| commands | 2,500 | 33 |
| mcp | 2,000 | 6 |
| bridge | 1,700 | 1 |
| api | 1,700 | 22 |
| tui | 1,600 | 53 |
| buddy | 1,100 | 1 |
| tools | 970 | 42 |
| plugins | 700 | 7 |
| acp | 90 | 7 |
| **Total** | **~26,000** | **~250** |
