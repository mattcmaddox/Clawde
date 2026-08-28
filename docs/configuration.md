# Clawde Configuration Reference

Clawde is configured through a layered system of JSON files, environment
variables, and command-line flags. This document describes every option.

---

## Configuration File Location

The global settings file lives at:

```
~/.clawde/settings.json
```

The directory `~/.clawde/` is created automatically on first run if it does
not exist. The file is standard JSON (or JSONC — comments are stripped before
parsing).

### Per-project settings

Clawde walks up from the current working directory looking for a project-level
settings file. The first file found wins (project settings take precedence over
global settings):

```
<project-root>/.clawde/settings.json
<project-root>/.clawde/settings.jsonc
```

Settings that appear in the project file override the corresponding global
values. Keys absent from the project file fall back to the global value.

---

## Top-level Settings Structure

```json
{
  "version": 1,
  "provider": "anthropic",
  "config": { ... },
  "providers": { ... },
  "modelOverrides": { ... },
  "projects": { ... },
  "commands": { ... },
  "formatter": { ... },
  "agents": { ... },
  "skills": { ... },
  "permissionRules": [],
  "enabledPlugins": [],
  "disabledPlugins": [],
  "hasCompletedOnboarding": false
}
```

Most day-to-day options live inside the `config` object. Provider credentials
live in the `providers` map. Corrected model metadata for self-hosted or
unknown models lives in the `modelOverrides` map — see
[Model metadata overrides](providers.md#overriding-model-metadata).

---

## The `config` Object

The `config` object holds runtime behaviour options.

### Model and token settings

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `api_key` | string \| null | null | Anthropic API key. Overrides `ANTHROPIC_API_KEY` env var. Prefer the env var in shared environments. |
| `model` | string \| null | provider default | Model ID to use. When absent, the provider's default is used (`free/auto` in Free Mode; e.g. `claude-sonnet-4-6` for Anthropic, `gpt-4o` for OpenAI). |
| `max_tokens` | integer \| null | 8192 | Maximum tokens per model response. |
| `provider` | string \| null | `"free"` | Active provider (`free` = Free Mode router across your configured free upstreams). See the [Providers](#providers) section. |
| `defaultEffort` | string \| null | null | Persisted thinking-effort default: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultracode`. Applies whenever no session or CLI override exists. Manage with `/config set|get|unset default-effort`. |

### Reasoning and effort

Thinking depth is controlled by a single canonical effort level. Per-turn resolution, highest wins:

```text
session override (/effort, /thinking on|off)
  > CLI --effort
  > persisted settings.json  config.defaultEffort
  > provider/model default
```

- `/effort <level>` and `/thinking on|off` are **session-scoped**: they apply to the current conversation (and survive a saved-session resume) but never write to `settings.json`.
- `none` is an explicit "disable thinking" override, distinct from absent. It beats keyword activation (e.g. `ultracode`) and disables thinking where the provider supports it; providers without a disable knob fall back to their minimum level.
- `defaultEffort` is only the fallback — `/effort` or `/thinking` in the session always wins.

Each provider translates the level to its native thinking parameters:

| Provider family | Mapping |
|---|---|
| Anthropic | `budget_tokens` from the level (1024 … 20000), `< max_tokens` |
| OpenAI (GPT-5 / o-series) | `reasoning_effort: none \| minimal \| low \| medium \| high` |
| Google Gemini 2.5 | `thinkingConfig.thinkingBudget` + `includeThoughts` (budget clamped below `maxOutputTokens`) |
| Google Gemini 3.x | `thinkingConfig.thinkingLevel` (`minimal` … `high`) + `includeThoughts` |
| DeepSeek V4 | `thinking.type: enabled\|disabled` + `reasoning_effort: high \| max` |
| Qwen3 (OpenAI-compatible hosts) | `reasoning_effort` |
| Other models (Llama, GLM, gpt-oss, …) | no thinking parameters — the level only tunes temperature/prompt behaviour |

In Free Mode the router applies the override per upstream at dispatch time, so `/effort high` correctly turns on Gemini thinking when the chain lands on `free/google/…` and emits `reasoning_effort` for a DeepSeek or Qwen3 model elsewhere — each upstream only ever sees parameters its model family accepts.

### Ollama remote GPU and offline tool mode

Ollama is remote-only by default. Configure the endpoint through
`config.provider_configs.ollama.api_base` (the `/connect` and `/settings` write
target), `OLLAMA_HOST`, or the compatibility path
`providers.ollama.api_base` / `providers.ollama.options.default_host` with a
non-loopback remote endpoint. Clawde fails closed rather than using
`http://localhost:11434`, so it cannot silently run inference on the local CPU.
The nested `config.provider_configs` value wins when both locations are set.

Normal `ollama:auto` mode keeps tools and web search available. Isolated
`ollama:offline` mode removes network-capable tools and rejects them at dispatch,
even under bypass permissions, while still allowing inference through the
configured Ollama endpoint. Isolated mode also removes shell, interpreter,
sub-agent, LSP, MCP-resource, test/lint, formatter, and other indirect process
capabilities from the active tool set. A separate OS/container firewall is still
recommended for defense in depth. The mode is process-wide, so do not run
conflicting isolated and normal sessions in the same process.

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

### Permission mode

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `permission_mode` | string | `"default"` | Controls how tool permissions are enforced. One of `"default"`, `"acceptEdits"`, `"bypassPermissions"`, `"plan"`. |

See [Permission Modes](#permission-modes) for a full description of each value.

### Interface and output

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `theme` | string | `"default"` | Color theme for the TUI. One of `"default"`, `"dark"`, `"light"`, `"deuteranopia"`. |
| `output_style` | string \| null | null | Named output style (persona). Built-in values: `"default"`, `"concise"`, `"explanatory"`, `"learning"`, `"caveman"`, `"cathead"`. Custom styles can be added as Markdown or JSON files under `~/.clawde/output-styles/`; a JSON style may additionally declare decision knobs (`effort`, `plan`, `askOnAmbiguity`, `checkinCadence`). See [Output styles (personas)](advanced.md#output-styles-personas). |
| `output_format` | string | `"text"` | Output format for headless (`--print`) mode. One of `"text"`, `"json"`, `"stream-json"`. |
| `verbose` | boolean | false | Enable debug-level log output. |

### Context compaction

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `auto_compact` | boolean | true | Automatically compact the conversation context when the context window nears capacity. |
| `compact_threshold` | float | 0.75 | Fraction of the context window that triggers auto-compaction (0.0–1.0). 75% is the research-backed optimal quality point (Chroma 2025); the remaining 25% is working memory for reasoning. |

### System prompt

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `custom_system_prompt` | string \| null | null | Replace the default Clawde system prompt entirely with this text. |
| `append_system_prompt` | string \| null | null | Append this text to the end of the assembled system prompt (after AGENTS.md content). |

### Verify loop (execute-and-verify)

After a turn that wrote or edited files, Clawde automatically runs the
project's test suite and linter (detected from the project structure — cargo
test/clippy for Rust, pytest/ruff for Python, npm test/eslint for JS/TS, …),
feeds any failures back to the model for auto-fix, and repeats up to
`max_retries` times before surfacing the result. This happens inside the query
loop via the `Verify` continuation mode; disable it entirely with
`"enabled": false`.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `verify.enabled` | boolean | true | Enable the execute-and-verify loop. Set `false` to return to the plain stop-after-one-turn behaviour. |
| `verify.max_retries` | integer | 3 | Maximum auto-fix attempts before failures are surfaced to you. |
| `verify.sandbox` | string | `direct` | Where verification runs. `direct` runs in the project directory (fast, but leaves build artifacts and other side effects). `git worktree` creates a temporary detached worktree, copies your uncommitted changes into it (tracked edits via a git diff, new files verbatim — gitignored files are excluded), runs the checks there, and removes it afterwards — clean isolation with no side effects on your working tree, but it requires the project to be inside a git repository and the first build is cold (no shared `target/` cache). `container` runs each check inside a fresh `--rm` Docker/podman container (docker preferred, podman fallback) with the project directory mounted at `/workspace` and the container's own toolchain — so the host toolchain and everything outside the mount are isolated, and each container is removed on exit. The image is the `CLAWDE_VERIFY_IMAGE` env var when set, otherwise a default per detected language (e.g. `rust:latest`, `node:latest`, `python:latest`, `golang:latest`, `eclipse-temurin:latest`, `gcc:latest`). Requires a container runtime and a project with a recognizable toolchain; unknown projects demand an explicit `CLAWDE_VERIFY_IMAGE`. Note the mount is read-write, so build artifacts (e.g. `target/`) still land in the project directory — `worktree` is the mode to pick when you need zero side effects on the working tree. |
| `verify.auto_test` | boolean | true | Run the detected test suite during verification. |
| `verify.auto_lint` | boolean | true | Run the detected linter/typechecker during verification. |
| `verify.skip_when_no_writes` | boolean | true | Skip verification on turns that only read/searched and wrote no files. |
| `verify.timeout_secs` | integer | 180 | Per-command timeout in seconds. A hung command is killed and reported as a failure. |
| `verify.container_image` | string | unset | Image used by the `container` sandbox. When set it wins over the `CLAWDE_VERIFY_IMAGE` env var and the per-language default, so a project can pin its own toolchain image (e.g. `node:20-slim`) in settings. |

Example:

```json
{
  "config": {
    "verify": {
      "enabled": true,
      "max_retries": 3,
      "sandbox": "direct",
      "auto_lint": true,
      "auto_test": true,
      "skip_when_no_writes": true,
      "timeout_secs": 180
    }
    // ...or, to verify inside a pinned container image:
    // "verify": {
    //   "sandbox": "container",
    //   "container_image": "node:20-slim"
    // }
  }
}
```

### Memory injection (project auto-memory)

Project memory lives under `~/.clawde/projects/<project>/memory/` (the memdir
convention; `CLAUDE_COWORK_MEMORY_PATH_OVERRIDE` overrides the whole path, and
`CLAWDE_DISABLE_AUTO_MEMORY` disables the injection). When present, the
`MEMORY.md` index plus the most recent `sessions/*.md` summary are injected
into the system prompt's `<memory>` block every turn, so each session starts
already knowing the project's architecture, conventions, and recent work.

The auto-dream consolidation pass maintains these files automatically (now
per-project, so different projects never share memory); `/memory init` seeds
the `architecture.md` / `conventions.md` / `decisions.md` / `tasks.md`
templates plus a starter index, and `/memory status` shows their state. After
a verify round detects the project's test/lint commands, they are recorded
into `conventions.md` (idempotently) so future sessions know how to build and
verify without re-discovery.

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `memory.autoMemoryEnabled` | boolean \| null | enabled | Master switch for the project-memory system (injection + auto-dream consolidation + conventions recording). `false` disables injection even when memory files exist. `null` (unset) defers to the env vars and defaults in `is_auto_memory_enabled` — note `CLAWDE_DISABLE_AUTO_MEMORY=1` always wins over this setting. |
| `memory.maxTokens` | integer | unset | Cap on the combined `<memory>` injection in tokens (~4 bytes per token). When the index + session summary exceed it, the summary is dropped first, then the index is clamped at a line boundary. Unset uses the built-in per-file caps (25 KB index / 4 KB summary). Snake_case keys (`auto_memory_enabled`, `max_tokens`) are also accepted. |

Example:

```json
{
  "config": {
    "memory": {
      "autoMemoryEnabled": true,
      "maxTokens": 1500
    }
  }
}
```

### Tool access

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `allowed_tools` | array of strings | [] (all) | Expose only these tools and treat matching calls as explicitly approved. An empty array means all tools are eligible. |
| `disallowed_tools` | array of strings | [] | Hide and hard-block these tools at runtime; deny wins if a name appears in both lists. |

Tool names match the internal names: `Bash`, `Read`, `Write`, `Edit`, `Glob`,
`Grep`, `WebSearch`, `WebFetch`, `TodoWrite`, `TodoRead`, and MCP tool names
prefixed with their server name (`myserver_toolname`).

### Directory access

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `additional_dirs` | array of strings | [] | Additional filesystem paths Clawde is allowed to read and write. Equivalent to passing `--add-dir` on the command line. |

### MCP servers

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `mcp_servers` | array of `McpServerConfig` | [] | Model Context Protocol servers to connect at startup. |

Each `McpServerConfig` object:

```json
{
  "name": "my-server",
  "command": "/path/to/server",
  "args": ["--flag"],
  "env": { "MY_VAR": "value" },
  "type": "stdio"
}
```

`type` can be `"stdio"` (default) or `"http"` (for HTTP-SSE servers, in which
case `command` is the base URL).

### Environment variables injected into tools

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `env` | object (string → string) | {} | Environment variables injected into every tool execution. Useful for setting project-specific tokens without polluting the system environment. Values may reference existing env vars using `{env:VARNAME}` syntax. |

### Hooks

Hooks let you run shell commands in response to lifecycle events. They are
defined as a map from event name to an array of hook entries.

```json
"hooks": {
  "PreToolUse": [
    { "command": "echo tool=$TOOL_NAME", "blocking": false }
  ],
  "PostToolUse": [
    { "command": "/path/to/my-logger.sh", "tool_filter": "Bash", "blocking": false }
  ],
  "Stop": [
    { "command": "notify-send 'Clawde done'", "blocking": false }
  ]
}
```

Available events:

| Event | When it fires |
|-------|--------------|
| `PreToolUse` | Before a tool executes. Receives event JSON on stdin. |
| `PostToolUse` | After a tool returns its result. |
| `Stop` | When the model finishes its turn (stop reason). |
| `PostModelTurn` | After the model samples a response, before tool execution. |
| `UserPromptSubmit` | When the user submits a prompt. |
| `Notification` | General-purpose notification event. |

Hook entry fields:

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Shell command to execute. |
| `tool_filter` | string \| null | Only run for this tool name (`PreToolUse`/`PostToolUse` only). |
| `blocking` | boolean | If true, a non-zero exit code blocks the operation. Default: false. |

---

## Permission Modes

The `permission_mode` field (and `--permission-mode` CLI flag) controls how
tool calls are approved.

### `default`

Read-only operations (file reads, searches, glob, and read-only network tools)
are permitted automatically. Writes, execution, and stateful coordination
operations prompt the user in the TUI, or are denied in headless mode.

### `acceptEdits`

File-edit operations are automatically accepted without prompting. Execution
and stateful coordination still follow their normal approval policy. This is
useful for trusted edit-heavy workflows without making arbitrary execution
implicit.

### `bypassPermissions`

Ordinary approval prompts are skipped. Explicit deny rules, forbidden
capabilities, and network-isolation boundaries still block calls. This mode
cannot be used when running as root or via `sudo` on Unix systems (Clawde
blocks it).

Use with caution: the model can read and modify any file reachable from the
current working directory without any user confirmation.

### `plan`

Read-only mode. File reads and searches are allowed; file writes, command
execution, and stateful coordination are blocked. This matches the built-in
`plan` agent's behaviour and is useful for code analysis sessions where you
want to prevent accidental modifications.

The permission mode can also be overridden per-session on the command line:

```bash
clawde --permission-mode acceptEdits "refactor the auth module"
clawde --dangerously-skip-permissions "..."  # equivalent to bypassPermissions
```

---

## AGENTS.md Memory Files

AGENTS.md files are plain Markdown documents that Clawde injects into the
system prompt at startup. They let you give the model persistent context about
your project, coding standards, or personal preferences without repeating
yourself in every session.

### File locations and priority

Clawde loads AGENTS.md files from four locations. They are processed in the
following order (earlier = higher priority, later content is appended below):

| Scope | Path | Description |
|-------|------|-------------|
| Managed | `~/.clawde/rules/*.md` | Global policy files. All `.md` files in this directory are loaded in alphabetical order. |
| User | `~/.clawde/AGENTS.md` | Your personal preferences and instructions, applied to all projects. |
| Project | `<project-root>/AGENTS.md` | Project-level context: architecture notes, conventions, workflows. Typically committed to version control. |
| Local | `<project-root>/.clawde/AGENTS.md` | Local overrides not committed to version control (add `.clawde/` to `.gitignore`). |

Files from all four locations are concatenated (separated by blank lines) into
a single system-prompt fragment. If the same instruction appears at multiple
levels, the narrower scope (Project/Local) effectively wins because it appears
later in the prompt.

### CLAUDE.md compatibility

Files named `CLAUDE.md` in the same locations are treated identically to
`AGENTS.md`. Both names are supported for compatibility with the TypeScript
Claude Code CLI.

### YAML frontmatter

AGENTS.md files may begin with optional YAML frontmatter to control loading:

```markdown
---
memory_type: project
priority: 10
scope: project
---

# My Project Notes

Always use 4-space indentation. Prefer `anyhow` for error handling.
```

Frontmatter fields:

| Field | Description |
|-------|-------------|
| `memory_type` | Informal label (currently informational only). |
| `priority` | Integer sort priority (lower numbers are prepended first within the same scope). |
| `scope` | Informational label for documentation purposes. |

### @include directives

AGENTS.md files support `@include` to pull in content from other files:

```markdown
# Project Guide

@include ./docs/architecture.md
@include ~/shared-notes/coding-standards.md
```

Paths may be relative to the including file, absolute, or tilde-expanded.
Circular includes are detected and skipped. Files larger than 40 KB are
skipped with a warning comment.

### Disabling AGENTS.md loading

To skip all AGENTS.md files for a session:

```bash
clawde --no-claude-md "your prompt"
```

Or in a session, use the `--bare` flag to disable AGENTS.md, hooks, and
plugins simultaneously.

---

## Providers

Clawde can send requests to multiple LLM providers. Set the active provider
via the `provider` key in settings or the `--provider` CLI flag.

### Provider IDs

| Provider ID | Default model |
|-------------|--------------|
| `anthropic` | `claude-sonnet-4-6` (or latest) |
| `openai` | `gpt-4o` |
| `google` | `gemini-2.5-flash` |
| `groq` | `llama-3.3-70b-versatile` |
| `cerebras` | `llama-3.3-70b` |
| `deepseek` | `deepseek-chat` |
| `mistral` | `mistral-large-latest` |
| `xai` | `grok-2` |
| `openrouter` | `anthropic/claude-sonnet-4` |
| `togetherai` | `meta-llama/Llama-3.3-70B-Instruct-Turbo` |
| `perplexity` | `sonar-pro` |
| `cohere` | `command-r-plus` |
| `deepinfra` | `meta-llama/Llama-3.3-70B-Instruct` |
| `github-copilot` | `gpt-4o` |
| `ollama` | `llama3.2` |
| `lmstudio` | `default` |
| `llamacpp` | `default` |
| `azure` | `gpt-4o` |
| `amazon-bedrock` | `anthropic.claude-sonnet-4-6-v1` |
| `venice` | `llama-3.3-70b` |

### Per-provider configuration

Each provider can have its own entry in the `providers` map (top-level in
`settings.json`) or in `config.provider_configs`. Provider-level `api_key`
and `api_base` override the corresponding environment variables.

```json
"providers": {
  "anthropic": {
    "api_key": "sk-ant-...",
    "api_base": "https://api.anthropic.com",
    "enabled": true,
    "models_whitelist": [],
    "models_blacklist": []
  },
  "openai": {
    "api_key": "sk-...",
    "enabled": true
  },
  "ollama": {
    "api_base": "http://gpu-host.example:11434",
    "enabled": true
  }
}
```

`ProviderConfig` fields:

| Field | Type | Description |
|-------|------|-------------|
| `api_key` | string \| null | API key for this provider. |
| `api_base` | string \| null | Override the default API base URL. |
| `enabled` | boolean | Whether this provider is active. Default: true. |
| `models_whitelist` | array | If non-empty, only these model IDs are offered. |
| `models_blacklist` | array | These model IDs are never offered. |
| `options` | object | Provider-specific passthrough options. |

---

## Environment Variables

| Variable | Description |
|----------|-------------|
| `ANTHROPIC_API_KEY` | Anthropic API key. Checked after the `config.api_key` setting. |
| `ANTHROPIC_BASE_URL` | Override the Anthropic API base URL. |
| `CLAWDE_PROVIDER` | Active provider. Equivalent to `--provider`. |
| `CLAWDE_API_BASE` | Override the API base URL for the active provider. Equivalent to `--api_base`. |
| `CLAWDE_GOALS` | Set to `0` to disable the goal system (`/goal` command and `GoalCompleteTool`). |
| `OPENAI_API_KEY` | API key for the `openai` provider. |
| `GOOGLE_API_KEY` | API key for the `google` provider. |
| `GROQ_API_KEY` | API key for the `groq` provider. |
| `XAI_API_KEY` | API key for the `xai` provider. |
| `MISTRAL_API_KEY` | API key for the `mistral` provider. |
| `OPENROUTER_API_KEY` | API key for the `openrouter` provider. |
| `DEEPSEEK_API_KEY` | API key for the `deepseek` provider. |
| `COHERE_API_KEY` | API key for the `cohere` provider. |
| `DEEPINFRA_API_KEY` | API key for the `deepinfra` provider. |
| `VENICE_API_KEY` | API key for the `venice` provider. |
| `GITHUB_TOKEN` | Token for the `github-copilot` provider. |
| `AZURE_API_KEY` | API key for the `azure` provider. |
| `HF_TOKEN` | Token for the `huggingface` provider. |
| `NVIDIA_API_KEY` | API key for the `nvidia` provider. |
| `CLAWDE_BRIDGE_URL` | Enable the remote-control bridge by setting the server URL. |
| `CLAWDE_BRIDGE_TOKEN` | Bearer token for the remote-control bridge. |
| `RUST_LOG` | Tracing filter (e.g. `debug`, `clawde_core=trace`). |

---

## Custom Slash Commands

User-defined slash commands can be added to the `commands` map:

```json
"commands": {
  "review": {
    "template": "Please review the following code for bugs and style: $ARGUMENTS",
    "description": "Review code",
    "agent": "plan",
    "model": null
  }
}
```

`CommandTemplate` fields:

| Field | Description |
|-------|-------------|
| `template` | Template string. `$ARGUMENTS` is replaced with whatever the user types after the command name. |
| `description` | Short description shown in `/help`. |
| `agent` | Optional named agent to use (e.g. `"plan"`, `"build"`, `"explore"`). |
| `model` | Optional model override for this command. |

Use the command with `/review path/to/file.rs`.

---

## Named Agents

Agents are named configurations that combine a system prompt prefix, model,
permission level, and turn limit. Three are built in:

| Agent | Access | Description |
|-------|--------|-------------|
| `build` | full | Read, write, and execute. For feature implementation. |
| `plan` | read-only | Read files; no writes or commands. For analysis and planning. |
| `explore` | search-only | Search and read. For rapid codebase exploration. |

You can define custom agents in `settings.json`:

```json
"agents": {
  "review": {
    "description": "Code review agent",
    "model": "anthropic/claude-haiku-4-5",
    "temperature": 0.3,
    "prompt": "You are a senior engineer doing code review. Be thorough and direct.",
    "access": "read-only",
    "visible": true,
    "max_turns": 30,
    "color": "magenta"
  }
}
```

`AgentDefinition` fields:

| Field | Type | Description |
|-------|------|-------------|
| `description` | string \| null | Description shown in `@agent` autocomplete. |
| `model` | string \| null | Model override for this agent. |
| `temperature` | float \| null | Sampling temperature override. |
| `prompt` | string \| null | System prompt prefix (prepended before the main system prompt). |
| `access` | string | Permission level: `"full"`, `"read-only"`, or `"search-only"`. |
| `visible` | boolean | Whether to show in autocomplete. Default: true. |
| `max_turns` | integer \| null | Maximum agentic turns. |
| `color` | string \| null | ANSI display color: `"cyan"`, `"magenta"`, `"green"`, `"yellow"`, etc. |

Invoke an agent with `@agentname` in the TUI or `--agent agentname` on the CLI.

---

## Managed Agents Configuration

The `managed_agents` key stores the managed-agents architecture configuration set via `/managed-agents configure`. It is written automatically by the command and rarely needs to be edited manually.

```json
"managed_agents": {
  "enabled": true,
  "manager_model": "anthropic/claude-opus-4-6",
  "executor_model": "anthropic/claude-sonnet-4-6",
  "executor_max_turns": 20,
  "max_concurrent": 3,
  "executor_isolation": true,
  "budget_split": {
    "type": "Percentage",
    "manager_pct": 20
  },
  "total_budget_usd": 5.00
}
```

`budget_split` types:

| Type | JSON | Description |
|------|------|-------------|
| `SharedPool` | `{ "type": "SharedPool" }` | All agents draw from a single pool |
| `Percentage` | `{ "type": "Percentage", "manager_pct": 20 }` | Manager gets N% of total budget |
| `FixedCaps` | `{ "type": "FixedCaps", "manager_usd": 0.50, "executor_usd": 2.00 }` | Hard USD caps per role |

Configure via `/managed-agents configure` or `/managed-agents preset <name>`. Set `enabled: false` to disable without removing the configuration.

---

## File Formatters

Formatters run automatically after Clawde writes a file whose extension
matches. They are defined in the `formatter` map:

```json
"formatter": {
  "prettier": {
    "command": ["prettier", "--write"],
    "extensions": [".ts", ".tsx", ".js", ".json"],
    "disabled": false
  },
  "rustfmt": {
    "command": ["rustfmt"],
    "extensions": [".rs"],
    "disabled": false
  }
}
```

| Field | Description |
|-------|-------------|
| `command` | Command array. The filename is appended as the final argument. |
| `extensions` | File extensions this formatter handles (include the leading dot). |
| `disabled` | Set to true to temporarily disable without removing the entry. |

---

## Annotated Example `settings.json`

```json
{
  // Settings schema version
  "version": 1,

  // Active provider (can be overridden per-session with --provider)
  "provider": "anthropic",

  "config": {
    // Omit api_key here; use ANTHROPIC_API_KEY env var instead
    "api_key": null,

    // Model — leave null to use the provider's default
    "model": null,

    // Cap responses at 8 192 tokens
    "max_tokens": 8192,

    // In the TUI, ask before writing files or running commands
    "permission_mode": "default",

    // Dark theme for the TUI
    "theme": "dark",

    // Compact when context window is 75% full
    "auto_compact": true,
    "compact_threshold": 0.75,

    // Show debug logs
    "verbose": false,

    // Plain text output in --print mode
    "output_format": "text",

    // Add a custom instruction to every session
    "append_system_prompt": "Always explain your reasoning before making changes.",

    // Block the Bash tool globally
    "disallowed_tools": ["Bash"],

    // Inject a variable into every tool execution
    "env": {
      "MY_PROJECT_TOKEN": "{env:HOME}/.project_token"
    },

    // Run a script after every tool use
    "hooks": {
      "PostToolUse": [
        {
          "command": "/home/user/scripts/audit-log.sh",
          "blocking": false
        }
      ]
    },

    // Connect an MCP server at startup
    "mcp_servers": [
      {
        "name": "filesystem",
        "command": "mcp-server-filesystem",
        "args": ["/home/user/projects"],
        "env": {},
        "type": "stdio"
      }
    ]
  },

  // Per-provider credentials and options
  "providers": {
    "anthropic": {
      "api_key": null,
      "enabled": true
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "api_base": "http://gpu-host.example:11434",
      "enabled": true
    }
  },

  // Correct metadata for self-hosted / unknown models (keyed by provider/model).
  // Overrides win over the models.dev catalog.
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM"
    }
  },

  // Custom slash commands
  "commands": {
    "test": {
      "template": "Run the tests for $ARGUMENTS and report any failures.",
      "description": "Run and report tests"
    }
  },

  // Auto-run prettier on JS/TS file writes
  "formatter": {
    "prettier": {
      "command": ["prettier", "--write"],
      "extensions": [".ts", ".tsx", ".js", ".jsx"],
      "disabled": false
    }
  }
}
```
