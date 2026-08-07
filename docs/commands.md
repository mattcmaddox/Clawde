# Clawde Slash Commands Reference

This document is the complete reference for every slash command available in Clawde, the Rust reimplementation of Claude Code CLI. Commands are invoked by typing `/command-name` at the REPL prompt.

---

## Table of Contents

1. [Command System Overview](#command-system-overview)
2. [Session & Navigation](#session--navigation)
3. [Model & Provider](#model--provider) — `/model`, `/providers`, `/connect`, `/thinking`, `/effort`, `/advisor`, `/fast`
4. [Configuration & Settings](#configuration--settings) — `/config`, `/keybindings`, `/permissions`, `/hooks`, `/privacy-settings`, `/mcp`, `/output-style`, `/theme`, `/statusline`, `/vim`, `/voice`, `/terminal-setup`
5. [Code & Git](#code--git) — `/commit`, `/diff`, `/undo`, `/review`, `/spec`, `/spec-mode`, `/spec-review`, `/security-review`, `/init`, `/search`
6. [Search & Files](#search--files) — `/files`, `/context`
7. [Memory & Context](#memory--context) — `/memory`, `/usage`, `/cost`, `/stats`, `/status`, `/insights`
8. [Agents & Tasks](#agents--tasks) — `/agents`, `/tasks`, `/goal`, `/managed-agents`, `/agent`
9. [Planning & Review](#planning--review) — `/plan`, `/ultraplan`, `/ultrareview`
10. [MCP & Integrations](#mcp--integrations) — `/mcp`, `/skills`, `ultracode`, `/plugin`, `/chrome`
11. [Authentication](#authentication) — `/login`, `/logout`, `/accounts`, `/switch`, `/refresh`
12. [Display & Terminal](#display--terminal) — `/theme`, `/output-style`, `/statusline`, `/vim`, `/terminal-setup`, `/caveman`, `/rocky`, `/normal`, `/mobile`, `/color`, `/stickers`
13. [Diagnostics & Info](#diagnostics--info) — `/doctor`, `/health`, `/verify`, `/version`, `/update`
14. [Export & Sharing](#export--sharing) — `/export`, `/copy`
15. [Advanced & Internal](#advanced--internal) — `/thinking`, `/connect`, `/fork`, `/effort`, `/summary`, `/brief`, `/sandbox-toggle`, `/think-back`, `/thinkback-play`
16. [Command Availability](#command-availability)

---

## Command System Overview

Commands are registered in a priority-ordered registry. When you type a command name, Clawde resolves it through this chain:

```
bundledSkills -> builtinPluginSkills -> skillDirCommands ->
workflowCommands -> pluginCommands -> pluginSkills -> COMMANDS()
```

### Command Types

| Type | Behavior |
|------|----------|
| `local` | Runs synchronously; returns text output directly |
| `local-jsx` | Renders an interactive TUI component (model picker, theme selector, etc.) |
| `prompt` | Expands to a prompt sent to the model via the main inference loop |

Commands support aliases — for example `/h`, `/?`, and `/help` all invoke the same handler.

### Usage Syntax

```
/command-name [arguments]
```

Arguments are passed as a single string after the command name. Most commands that accept arguments are documented with an `argumentHint` shown in the command palette.

---

## Session & Navigation

### /help
**Aliases:** `h`, `?`

Display all available commands with their descriptions. Respects `isHidden` flags — internal or rarely-needed commands are suppressed unless you are an Anthropic employee.

```
/help
/h
/?
```

---

### /clear
**Aliases:** `reset`, `c`

Clear the current conversation history. The session id and its on-disk file are retained — only the in-memory message list is wiped, so you stay in the same session. Use `/new` to start a genuinely fresh session instead.

```
/clear
```

---

### /new

Start a fresh session, mirroring opencode's `/new`. The transcript resets to a blank home and a brand-new session id begins, while your current model, provider, effort level and working directory carry over. The new session is *lazy* — it is not written to disk until your first message, so opening `/new` without typing anything leaves no trace.

Unlike `/clear` (which keeps the same session id and history file), `/new` opens a clean, separate session.

```
/new
```

---

### /move

Re-home the current session to another worktree or directory of the **same project**, mirroring opencode's `/move`. Any uncommitted changes in the current directory are carried over to the destination and reset in the original location; the model is informed of the new working directory on its next turn.

The destination must belong to the same git repository (typically a linked `git worktree`); moving to an unrelated project is refused. Pass `--no-changes` to re-home the session without relocating working-tree changes.

```
/move <directory>
/move ../myapp-feature
/move --no-changes /path/to/other/worktree
```

**Adaptation note:** opencode presents an interactive worktree picker and can create a new worktree on the fly. Clawde takes the destination directory as an argument and re-homes the live session's working directory (clawde has no separate session-per-worktree registry). Uncommitted changes are relocated with `git diff`/`git apply` and the source is reset with `git checkout` (index preserved) plus `git clean` (untracked removed), matching opencode's `move-session` change handling.

---

### /exit
**Aliases:** `quit`

Exit the Clawde REPL. Equivalent to pressing `Ctrl+D`. Unsaved session state is flushed before exit.

```
/exit
/quit
```

---

### /resume
**Aliases:** `continue`

Resume a previous session from the session store. Displays a list of recent sessions with timestamps and summaries. Select one to restore its message history and file state.

```
/resume
/resume <session-id>
```

---

### /session
**Aliases:** `remote`, `history`

Manage active and stored sessions. Subcommands allow listing, switching, deleting, and attaching to remote sessions.

```
/session
/session list
/session delete <session-id>
/session attach <session-id>
```

`/history` is an alias for `/session` that opens the session browser in the TUI; the prompt autocomplete shows `/session` as if it had been typed.

Aliases are defined per-command in the commands crate (`SlashCommand::aliases`). Every declared alias is picked up automatically as a **hidden alias**: typing its prefix in the prompt suggests the canonical command name, and executing it resolves to the canonical command.

---

### /fork

Fork the current session into a new independent session that begins from the current conversation state. Useful for exploring two different approaches without losing either.

```
/fork
/fork <new-session-name>
```

---

### /rename

Rename the current session. The new name is used in session listings and exports.

```
/rename <new-name>
```

---

### /rewind
**Aliases:** `checkpoint`

Rewind the conversation to a previous message. Displays a numbered list of messages; enter a number to truncate history to that point and resume from there.

```
/rewind
/rewind <message-index>
```

---

### /compact

Summarize and compress the conversation history to reduce context window usage. The model is asked to produce a dense summary of the prior exchange; that summary replaces the raw messages.

```
/compact
```

---

## Model & Provider

### /model

Open the interactive model picker. Displays a searchable list of available models from all configured providers. The selected model is used for all subsequent inference in the current session.

```
/model
/model claude-opus-4-5
/model claude-sonnet-4-6
```

#### Capability filtering (`--capability`)

Filter the model picker to only show models that support a specific capability.

```
/model --capability vision        # only vision-capable models
/model --capability audio          # only audio-capable models
/model --capability tools          # only models with tool/function calling
/model --capability reasoning      # only models with extended thinking
/model --capability json           # only models with structured JSON output
/model --capability vision,tools   # AND: models with both capabilities
/model --capability vision|audio   # OR: models with either capability
```

Available capabilities:

| Value | Aliases | Description |
|---|---|---|
| `vision` | `image` | Image understanding & processing |
| `audio` | — | Audio input & processing |
| `pdf` | — | PDF document processing |
| `video` | — | Video input & processing |
| `tools` | `tool_calling`, `tool-calling` | Tool / function calling |
| `reasoning` | — | Extended reasoning / chain-of-thought |
| `json` | `structured_output`, `structured-output` | Structured JSON-schema output |

---

### /image

Switch to a model with a specific capability (defaults to vision). After switching, paste an image with Ctrl+V to include it in your prompt.

```
/image                                    — switch to a vision-capable model
/image --capability audio                  — switch to an audio-capable model
/image --capability tools                  — switch to a tool-calling model
/image --capability pdf                    — switch to a PDF-capable model
/image --capability reasoning|video        — switch to a model with reasoning OR video
```

Available capabilities are the same as [/model --capability](#capability-filtering---capability).

---

### /providers

List all configured AI providers and their connection status. Shows provider name, base URL, and whether credentials are present.

```
/providers
```

---

### /task

Cycle the free-model task sort, or jump straight to a named task. The task
sort reorders the `/models` picker so the best-fit models float to the top
for each common task (see the 1-7 legend in the picker header). Bare `/task`
cycles forward; pass a task name to jump directly.

```
/task
/task coding
/task reasoning
/task creative
/task all
```

Available tasks: `all`, `coding`, `reasoning`, `creative`, `fast`,
`multimodal`, `context` — plus the short legend forms shown in the picker
header (`code`, `reason`, `multi`, `ctx`). The same cycle is bound to
**Alt+T**, and the active sort shows as a badge in the status line when the
free provider is active. The sort persists across restarts and resets when
you switch to a non-free provider.

> Note: `/task` (singular) controls the free-model task **sort**, distinct
> from `/tasks` (plural), which manages background tasks.

---

### /connect

Connect to a remote AI provider or configure a custom provider endpoint. Supports OpenAI-compatible APIs, Anthropic direct, and others.

```
/connect
/connect <provider-name>
/connect openai https://api.openai.com/v1
```

---

### /thinking

Configure extended thinking for the current session. Extended thinking allows the model to reason through problems before responding, at the cost of additional tokens.

```
/thinking
/thinking on
/thinking off
```

See also `/effort` for a higher-level interface to thinking depth.

---

### /effort

Set the thinking effort level. This is a convenience wrapper over `/thinking` that maps human-readable levels to token budgets.

| Level | Description |
|-------|-------------|
| `low` | Minimal thinking; fastest responses |
| `medium` | Balanced thinking and speed |
| `high` | Deep reasoning; slower responses |
| `max` | Maximum token budget for thinking |

```
/effort low
/effort medium
/effort high
/effort max
```

---

### /advisor

Set or unset a secondary advisor model that provides supplementary suggestions alongside the main model. When set, the advisor model's context is available to improve main-model responses.

```
/advisor                          — show current advisor setting
/advisor claude-opus-4-6          — set advisor model by name
/advisor provider/model           — set advisor using provider/model format
/advisor off                      — disable the advisor
/advisor unset                    — disable the advisor
```

The advisor model persists to `~/.clawde/settings.json` under `advisorModel`. Model IDs must start with `claude-` or contain a `/` (provider/model format).

---

### /fast
**Aliases:** `speed`

Toggle fast mode. In fast mode, Clawde switches to the active provider's smaller, faster model for quick responses. Useful when you want rapid answers and deep reasoning is not required.

```
/fast          — toggle fast mode on/off
/fast on       — enable fast mode
/fast off      — disable fast mode
```

Setting persists to `~/.clawde/ui-settings.json`.

---

## Configuration & Settings

### /config
**Aliases:** `settings`

View or modify Clawde configuration values. Without arguments, renders an interactive settings panel. With arguments, acts as a key-value accessor.

```
/config
/config get <key>
/config set <key> <value>
/config reset <key>
```

Common keys:

| Key | Description |
|-----|-------------|
| `model` | Default model name |
| `theme` | Color theme name |
| `vim` | Vim mode enabled (`true`/`false`) |
| `outputStyle` | Output rendering style |
| `autoApprove` | Auto-approve tool calls |

---

### /keybindings

Open the interactive keybinding configurator. Displays all bound actions with their current shortcuts. Select an action to rebind it. Changes are written to `~/.clawde/keybindings.json`.

```
/keybindings
```

See [keybindings.md](./keybindings.md) for the full keybindings reference.

---

### /permissions
**Aliases:** `allowed-tools`

View and manage tool permission rules. Permissions control which tools can run without prompting, which are blocked, and which always require confirmation.

```
/permissions
/permissions list
/permissions allow <tool-name>
/permissions deny <tool-name>
/permissions reset
```

---

### /hooks

Manage event hooks. Hooks are shell commands or scripts that execute when lifecycle events fire (e.g., before/after tool calls, on session start/end).

```
/hooks
/hooks list
/hooks add <event> <command>
/hooks remove <hook-id>
```

Available events: `pre-tool`, `post-tool`, `session-start`, `session-end`, `message-send`, `message-receive`.

---

### /privacy-settings

Open Clawde privacy settings. Launches a browser to the Anthropic privacy portal where you can review data usage preferences, conversation retention, and account privacy options.

```
/privacy-settings
```

---

### /mcp

Configure and manage Model Context Protocol (MCP) servers. MCP servers expose additional tools and resources to the agent.

```
/mcp
/mcp list
/mcp add <name> <command>
/mcp remove <name>
/mcp restart <name>
```

---

### /output-style

Select how the model's output is rendered in the terminal. Choices include `auto`, `plain`, `markdown`, `streaming`, and others depending on terminal capabilities.

```
/output-style
/output-style plain
/output-style markdown
```

---

### /theme

Open the theme quick-pick popup. Browse the built-in and custom themes with
live swatches and select one for the Clawde TUI.

Inside the popup:

- `j`/`k` (or arrow keys) — navigate; the highlighted theme applies live
- `enter` — apply the selected theme and close
- `n` — jump straight into the theme creator's new-theme editor
- `d` — delete the selected custom theme (press `d`/`y` again to confirm; `esc` cancels)
- `esc` — close without changing the theme

```
/theme
/theme dark
/theme light
/theme solarized
```

In the TUI, `/theme create` opens the interactive theme creator: browse
built-in + custom themes in a scrollable list, create new themes from the
full ANSI 256-color grid, edit or delete custom themes, and apply on enter.
Custom themes are saved to `~/.clawde/themes/<name>.json`.

```
/theme create
/theme list
/theme delete <name>
```

#### Palette slots

Every theme (built-in or custom) defines **17 colour slots**. In the theme
creator's editor, `j`/`k` highlight a slot and `enter`/`space` (or `o` to stay)
assigns the grid cursor colour to it; `r` randomises the whole palette and `u`
undoes the last change. The slots are saved by name to
`~/.clawde/themes/<name>.json` (each value is either an ANSI 256 index
`0–255` or an `[r, g, b]` array).

| Slot | JSON key | Colours |
|---|---|---|
| `error` | `error` | Errors and alerts |
| `success` | `success` | Success indicators |
| `warning` | `warning` | Warnings and cautions |
| `info` | `info` | Informational messages |
| `action` | `action` | Interactive elements and action buttons |
| `disabled` | `disabled` | Dimmed or disabled states |
| `accent` | `accent` | Primary accent |
| `secondary_accent` | `secondary_accent` | Secondary accent |
| `panel_bg` | `panel_bg` | Main panel / dialog background |
| `text_light` | `text_light` | Text on dark backgrounds |
| `text_dark` | `text_dark` | Text on light backgrounds |
| `border` | `border` | Borders and dividers |
| `model_name` | `model_name` | Active model name in the prompt status line |
| `hint` | `hint` | Muted hint / shortcut text (e.g. `? shortcuts · Ctrl+/ keys`) |
| `effort` | `effort` | Effort-level indicator in the status line |
| `routing` | `routing` | Free-provider routing-strategy badge |
| `vim_hint` | `vim_hint` | Vim-mode navigation hint (`K↑J↓H↑L↓`) |

Older theme files that predate the `model_name`/`hint`/`effort`/`routing`/
`vim_hint` keys still load — the missing slots fall back to a sensible
existing colour (`text_light` for `model_name`, `disabled` for `hint`,
`secondary_accent` for `effort`, `action` for `routing`, `success` for
`vim_hint`).

---

### /statusline

Configure the status line displayed at the bottom of the TUI. Toggle individual elements such as model name, token count, session name, and git branch.

```
/statusline
/statusline toggle model
/statusline toggle tokens
```

---

### /vim

Toggle vim keybinding mode on or off. In vim mode the input field behaves like a vim editor (normal/insert/visual modes). Persisted to config.

```
/vim
/vim on
/vim off
```

---

### /voice

Configure voice input/output. Requires a supported audio backend. Subcommands control microphone selection, TTS voice, and push-to-talk behavior.

```
/voice
/voice on
/voice off
/voice mic <device>
/voice tts <voice-name>
```

---

### /terminal-setup

Run the terminal capability detection and setup wizard. Checks for true-color support, font ligatures, Unicode rendering, and configures Clawde accordingly.

```
/terminal-setup
```

---

## Code & Git

### /commit

Stage and commit changes to the current git repository. The model drafts a commit message based on the diff. You can review and edit the message before confirming.

```
/commit
/commit "optional message override"
```

---

### /diff

Show file diffs for changes made during the current session. Displays a unified diff of all files Clawde has written or edited since the session started.

```
/diff
/diff <file-path>
```

---

### /undo

Undo file changes made during the current session. Restores files to their state before Clawde's last write operation. Can be called multiple times to step further back.

```
/undo
/undo <file-path>
```

---

### /review

Initiate a code review pass over recent changes. The model examines all modified files and produces inline comments and a summary of issues found.

```
/review
/review <file-path>
/review --since HEAD~3
```

---

### /spec

Generate a structured specification for a non-trivial task *before* writing code (Spec-Driven Development). The model analyzes the repository (tracked files + current diff) and produces a spec containing requirements, a file plan, data models, acceptance tests, and edge cases, saved to `specs/<title>.json` in the repository root.

```
/spec add a rate-limiting middleware to the API server
```

The acceptance tests in the spec become the verification criteria when the task is later implemented (see the Verify loop).

---

### /spec-mode

Toggle Spec-Driven Development mode (audit spec §10). When enabled, the agent stops after generating a spec (`specs/<title>.json`) and waits for your review before writing any code.

```
/spec-mode          # toggle
/spec-mode on       # enable
/spec-mode off      # disable
```

The setting is persisted in settings.json as `"specMode"`. In spec mode the agent writes a structured spec first (via `/spec` or the `EnterSpecMode` tool), then stops for review — the review dialog auto-opens on the generated spec (§10.2). **Accepting** a spec in the review dialog also turns spec mode off, so the implementation turn runs to completion instead of stopping again to re-offer the same spec.

---

### /spec-review

Open the Spec-Driven Development review dialog for a generated spec (audit spec §10). Shows the structured spec — requirements, file plan, data models, acceptance tests, edge cases — and lets you **Accept** (queue an implementation turn against the spec), **Edit** (open the JSON in your editor), or **Reject**.

```
/spec-review              # newest spec in ./specs/ (picker when several exist)
/spec-review specs/foo.json
```

Navigation: `↑/↓` or `j/k` scroll the content, `←/→` or `h/l` move between the Accept / Edit Spec / Reject actions, `Enter` activates the selected action, `Esc` closes.

With several specs in `specs/`, a bare `/spec-review` opens a picker (newest first): `↑/↓` or `j/k` highlights a spec, `Enter` opens it, `Esc` closes. In spec mode the dialog also opens automatically after a turn that generated a spec.

---

### /security-review

Run a security-focused review pass. The model looks specifically for vulnerabilities, credential exposure, injection risks, and other security concerns in modified files.

```
/security-review
/security-review <file-path>
```

---

### /init

Initialize Clawde project configuration in the current directory. Creates a `CLAUDE.md` file that acts as persistent project-level context injected at the start of every session.

```
/init
```

---

### /search

Search the codebase using natural language or regex patterns. Wraps the GrepTool and GlobTool with a higher-level interface.

```
/search <query>
/search "TODO" --type ts
/search "function.*export" --regex
```

---

## Search & Files

### /files

List all files currently tracked (read or written) in the active session. Useful for reviewing what context the model has access to.

```
/files
/files --written
/files --read
```

---

### /context

Analyze context window usage. Shows a breakdown of tokens consumed by system prompt, conversation history, file contents, and tool results. Helps identify what to compact or drop.

```
/context
```

---

## Memory & Context

### /memory

Manage memory files: the AGENTS.md instruction files that provide project context, plus the project auto-memory store (`MEMORY.md` index + session summaries) that is injected into the system prompt at session start.

```
/memory               — show all AGENTS.md memory files
/memory edit          — open the project AGENTS.md in your editor
/memory edit global   — open the global ~/.clawde/AGENTS.md in your editor
/memory clear         — clear the project AGENTS.md
/memory clear global  — clear the global ~/.clawde/AGENTS.md
/memory status        — show the project auto-memory dir, MEMORY.md index state,
                        memory-file count, and session summaries
/memory init          — seed architecture/conventions/decisions/tasks templates
                        plus a starter MEMORY.md index
```

AGENTS.md locations checked (in priority order):

1. `<project>/.claurst/AGENTS.md`
2. `<project>/AGENTS.md`
3. `~/.clawde/AGENTS.md`  (global)

Project auto-memory lives under `~/.clawde/projects/<project>/memory/`
(`MEMORY.md` plus `sessions/`). When present, the index and the most recent
session summary are injected into the system prompt's `<memory>` block each
turn, so the model starts every session already knowing the project's
architecture, conventions, and recent work. The auto-dream consolidation
pass maintains these files automatically; `/memory status` shows their
state. Disable the injection with the `CLAURST_DISABLE_AUTO_MEMORY` env var.

---

### /usage

Display a detailed token usage breakdown for the current session. Shows input tokens, output tokens, cache reads, cache writes, and estimated cost per API call.

```
/usage
```

---

### /cost

Show the total token usage and estimated cost for the current session. Provides a quick summary without the per-call breakdown of `/usage`.

```
/cost
```

---

### /stats

Display session statistics: number of messages, tool calls, files modified, tokens used, session duration, and model used.

```
/stats
```

---

### /status

Show the current session status. Includes active model, permission mode, thinking config, connected MCP servers, and loaded plugins.

```
/status
```

---

### /insights
**Aliases:** `ctx-viz`

Generate an analytical report of the current session. Prints a structured breakdown of conversation statistics including turn count, token usage (input/output/total), average tokens per exchange, estimated cost, total tool calls, and the most frequently invoked tool.

```
/insights
```

Sample output:
```
Session Insights
──────────────────────────────────────
Conversation
├─ User turns          : 12
├─ Assistant turns     : 12
└─ Completed exchanges : 12

Tokens
├─ Input               : 48320
├─ Output              : 9140
├─ Total               : 57460
└─ Avg per exchange    : 4788

Cost
└─ Estimated USD       : $0.1823

Tools
├─ Total calls         : 34
└─ Most used           : Bash (18 calls)
```

---

## Agents & Tasks

### /agents

Manage sub-agents. Sub-agents are parallel model instances that can be spawned to work on independent tasks simultaneously.

```
/agents
/agents list
/agents stop <agent-id>
/agents output <agent-id>
```

---

### /tasks
**Aliases:** `bashes`

Manage tracked background tasks. Tasks are shell commands or model invocations running asynchronously. Monitor progress, fetch output, or stop tasks from this interface.

```
/tasks
/tasks list
/tasks output <task-id>
/tasks stop <task-id>
```

---

### /goal

Set a durable multi-turn autonomous goal. When a goal is active, Clawde continues working across turns until the goal is marked complete, paused, or a 200-turn runaway guard fires. Designed for complex, sustained tasks that would otherwise require repeated manual re-prompting.

```
/goal <objective>                    — set a new goal and begin working autonomously
/goal --tokens 250K <objective>      — set a goal with a soft token budget cap
/goal                                — show current goal status
/goal status                         — show current goal status
/goal pause                          — pause the active goal
/goal resume                         — resume a paused goal
/goal clear                          — delete the current goal
/goal complete                       — request a completion audit
```

When the model believes the goal has been achieved, it calls the `GoalComplete` tool with an audit summary and evidence. Goals can be disabled globally by setting `CLAURST_GOALS=0` in your environment.

See [Goal System](./advanced.md#goal-system) in the advanced guide.

---

### /managed-agents

Configure the manager-executor agent architecture, where a manager model delegates subtasks to one or more executor agents working in parallel. Includes budget controls and isolation options.

```
/managed-agents                                       — show current configuration
/managed-agents status                                — show current configuration
/managed-agents presets                               — list built-in presets
/managed-agents preset <name>                         — apply a named preset
/managed-agents setup                                 — show setup instructions
/managed-agents enable                                — enable managed agents
/managed-agents disable                               — disable managed agents
/managed-agents reset                                 — remove all managed-agent configuration
/managed-agents configure manager-model <model>       — set the manager model
/managed-agents configure executor-model <model>      — set the executor model
/managed-agents configure executor-turns <n>          — set executor max turns
/managed-agents configure concurrent <n>              — set max concurrent executors
/managed-agents configure isolation on|off            — toggle executor isolation
/managed-agents configure budget-split shared         — shared token pool
/managed-agents configure budget-split percentage:<n> — percentage split (manager gets n%)
/managed-agents configure budget-split fixed:<m>:<e>  — fixed USD caps (manager / executor)
/managed-agents budget <amount>                       — set total budget in USD (0 to clear)
```

Model format: `provider/model` (e.g., `anthropic/claude-opus-4-6`, `openai/gpt-4o`). Configuration persists to `~/.clawde/settings.json` under `managed_agents`.

> **Preview feature.** Behaviour may change across releases.

See [Managed Agents](./advanced.md#managed-agents) in the advanced guide.

---

### /agent

List all available named agents, or show details for a specific agent. Named agents are predefined configurations with their own system prompts, model bindings, and access levels. Useful for discovering what agents are available before starting a session.

```
/agent             — list all visible named agents with access levels
/agent <name>      — show full details for a specific named agent
```

To activate an agent, start Clawde with `--agent <name>`. See [agents.md](./agents.md) for defining custom agents.

---

## Planning & Review

### /plan

Enter plan mode (read-only). In plan mode the model can read files and reason about changes but cannot write, edit, or execute anything. Use this to draft an approach before allowing writes.

```
/plan
```

To exit plan mode, use `/plan off` or the `/exit-plan` internal action.

---

### /ultraplan

Extended planning mode with deeper reasoning. Like `/plan` but with an elevated thinking budget to allow more thorough analysis before acting.

```
/ultraplan
```

---

### /ultrareview

Run an exhaustive multi-dimensional code review over the current working directory or a specified path. Goes significantly beyond `/review` and `/security-review`, covering:

- **Security** — OWASP Top 10, injection vulnerabilities, cryptographic weaknesses, path traversal, race conditions, dependency risks
- **Performance** — algorithmic complexity, allocations, N+1 queries, blocking I/O, memory leaks
- **Maintainability** — function length, nesting depth, DRY violations, naming, dead code
- **Error handling** — swallowed errors, panic paths, missing input validation
- **Test coverage** — missing tests, brittle tests, missing edge cases
- **API design, documentation, accessibility, and architecture**

Each finding is tagged by category and severity.

```
/ultrareview
/ultrareview <path>
/ultrareview <PR-number>
```

---

## MCP & Integrations

### /mcp

Documented above under [Configuration & Settings](#configuration--settings).

---

### /skills

List and manage skills. Skills are bundled prompt-commands that extend Clawde's capabilities without writing code. They appear alongside built-in commands in the registry.

```
/skills
/skills list
/skills enable <skill-name>
/skills disable <skill-name>
/skills reload
```

---

### ultracode (top effort + keyword)

Run a disciplined **ultracode** workflow for serious coding tasks. Ultracode is clawde's take on Claude Code's `ultrathink`: a supervised procedure that classifies the task, picks a mode, and — when it genuinely helps — delegates bounded work across clawde's native agent primitives, then integrates and verifies in the parent session.

Ultracode is the **highest effort level** — it sits past `max` on the "Smarter" end of the effort ladder and runs the model's top reasoning **plus** the workflow procedure. (It is no longer a `/skill`.) There are two ways to trigger it:

- **In the effort selector.** Run `/effort` and pick **ultracode** — the rightmost level, past the `│` divider, drawn with an animated purple spectrum. Applies for subsequent turns until you change the effort.
- **As a keyword.** Type the single word `ultracode` anywhere in a normal prompt. The keyword renders with a purple gradient in the input, and for that turn the effort is set to ultracode (its operating procedure is injected as a system-prompt addendum). No keyword means no change to normal prompts.

```
please ultracode <task>    — activate ultracode for this one turn via the inline keyword
/effort  →  ultracode      — set ultracode as the current effort level
```

**What it does**

1. **Classify** the task by type, risk, blast radius, verification needs, and whether independent packets exist.
2. **Pick a mode** — *Direct* (small, tightly-coupled work), *Workflow* (multi-phase work executed as isolated passes), or *Delegated* (the default for non-trivial work with independent packets).
3. **Delegate** in delegated mode using native primitives: `Agent` for bounded subagents (with `isolation: "worktree"` / `run_in_background: true`), `TeamCreate` for parallel swarms, and `TaskCreate` for background tasks. It fans out **2–4** subagents (cap ~5) on non-overlapping packets while the parent keeps the blocking critical path.
4. **Integrate** every result in the parent, checking claimed edits against the files and rejecting evidence-free outputs.
5. **Verify** with checks scaled to risk (targeted tests → lint/typecheck → build → smoke → independent review), reporting any skipped checks honestly.

**Composes with `/goal`.** Ultracode governs *how* a turn plans, delegates, integrates, and verifies; `/goal <objective>` keeps the work going *across* turns. Combine them for long, autonomous objectives — the goal loop spans turns while ultracode structures each one.

---

### /plugin
**Aliases:** `plugins`, `marketplace`

Manage plugins. Plugins are loadable modules that can register new commands, tools, and hooks. Browse the marketplace or install from a local path.

```
/plugin
/plugin list
/plugin install <name>
/plugin install <path>
/plugin remove <name>
/plugin reload
```

---

### /chrome

Browser automation via Chrome DevTools Protocol (CDP). Connects to a running Chrome or Chromium instance and lets Clawde control it — navigate pages, click elements, fill forms, evaluate JavaScript, and take screenshots.

First, launch Chrome with remote debugging enabled:

```bash
chrome --remote-debugging-port=9222 --no-first-run
```

Then:

```
/chrome connect [--port 9222]      — connect to Chrome on the given port (default: 9222)
/chrome navigate <url>             — navigate to a URL
/chrome screenshot                 — take a screenshot, saved to a temp file
/chrome click <selector>           — click a CSS selector
/chrome fill <selector> <text>     — fill an input field
/chrome eval <js>                  — evaluate JavaScript and return the result
/chrome disconnect                 — disconnect from Chrome
```

Useful for testing web applications, scraping, or automating browser-based workflows without a separate browser-automation tool.

---

## Authentication

Clawde supports **multiple named accounts per provider** — Anthropic (Claude.ai or Console) and Codex (OpenAI ChatGPT subscription). Each login creates a profile under `~/.clawde/accounts/<provider>/<id>/` and the registry at `~/.clawde/accounts.json` tracks which one is active.

See [Authentication Guide](./auth.md#multi-account-profiles) for the full story and on-disk layout.

### /login

Authenticate with Anthropic or Codex via OAuth PKCE. Opens a browser for the flow and saves tokens under the active profile (or creates a new profile if none exists).

```
/login                            — Claude.ai OAuth (Bearer token, default)
/login --console                  — Console OAuth (creates an API key)
/login --codex                    — Codex / ChatGPT OAuth
/login --label work               — name the new profile "work"
/login --codex --label personal   — Codex login, name the profile "personal"
```

If a profile matching the JWT's email or account_id already exists, that profile is refreshed in place — re-logging-in is idempotent. Use `--label` to either name a fresh profile or to disambiguate.

---

### /logout

Remove credentials. By default removes only the **active** profile for the provider; other stored profiles remain switchable.

```
/logout                — clear active Anthropic profile (drops it from registry)
/logout --codex        — clear active Codex profile
/logout --all          — purge every Anthropic profile + clear any API key in settings
/logout --codex --all  — purge every Codex profile
```

---

### /accounts

List every stored account across providers. The active profile in each provider is marked with `*`.

```
/accounts
```

Sample output:

```
Anthropic:
  * personal [pro]    kuber@personal.example
    work     [max]    kuber@company.example
Codex:
    work              kuber@company.example
```

---

### /switch

Switch the active account for a provider. Anthropic by default; pass `--codex` for Codex. Run `/accounts` first to see available profile ids.

```
/switch work                     — set active Anthropic profile to "work"
/switch --codex personal         — set active Codex profile to "personal"
```

---

### /refresh

Refresh the provider authentication state. Forces a token refresh without full re-authentication. Useful when a session token has expired mid-session.

```
/refresh
```

---

## Display & Terminal

### /theme

Documented above under [Configuration & Settings](#configuration--settings).

---

### /output-style

Documented above under [Configuration & Settings](#configuration--settings).

---

### /statusline

Documented above under [Configuration & Settings](#configuration--settings).

---

### /vim

Documented above under [Configuration & Settings](#configuration--settings).

---

### /terminal-setup

Documented above under [Code & Git](#code--git).

---

### /caveman

Activate caveman speech mode. In caveman mode the model strips pleasantries, hedging, articles, and transitional phrases from its responses, producing dense, telegraphic output. Useful for reducing verbosity and saving tokens on long sessions.

```
/caveman             — activate full caveman mode (~75% token reduction)
/caveman lite        — remove pleasantries only (~40% reduction)
/caveman full        — compress sentences and drop articles (default, ~75% reduction)
/caveman ultra       — maximum compression, imperative phrases only (~85% reduction)
```

Deactivate with `/normal`.

---

### /rocky

Activate Rocky speech mode. Rocky is the Eridian alien engineer from *Project Hail Mary* who communicates in a distinctive pidgin English with specific grammar rules and expressive emphasis. In rocky mode the model adopts Rocky's communication style.

```
/rocky             — activate full Rocky mode (~75% token reduction)
/rocky lite        — grammar rules only, minimal emphasis (~40% reduction)
/rocky full        — full Rocky grammar + regular emphasis (default, ~75% reduction)
/rocky ultra       — maximum Rocky personality, frequent emphasis, alien observations
```

Deactivate with `/normal`.

---

### /normal

Deactivate any active speech mode (caveman or rocky) and return the model to its standard response style.

```
/normal
```

---

### /mobile

Display a QR code and download links for the Claude mobile app. Supports a `session` subcommand that generates a QR code linking directly to an active remote Clawde session.

```
/mobile             — show QR code for claude.ai/mobile (works for both platforms)
/mobile ios         — show QR code for the iOS App Store
/mobile android     — show QR code for Google Play
/mobile session     — show QR code linking to the active remote session (requires --remote)
```

---

### /color

Set the prompt bar color for the current session. Accepts standard color names or hex values. The color resets when the session ends unless saved via `/config`.

```
/color               — open the interactive color picker
/color <name>        — set to a named color (e.g., blue, red, green)
/color #ff6b6b       — set to a hex color value
/color default       — reset to the theme default
```

---

### /stickers

Opens the Clawde sticker page (`stickermule.com/claudecode`) in your default browser. Falls back to printing the URL if no browser can be launched.

```
/stickers
```

---

## Diagnostics & Info

### /doctor

Run the Clawde diagnostics suite. Checks configuration integrity, provider connectivity, tool availability, MCP server health, and reports any issues.

```
/doctor
```

---

### /health

Probe every stored free-mode API key and report per-key health. Each key is
checked with a live request to the provider's `/v1/models` endpoint (5s
timeout). Upstreams whose models endpoint does not validate the key —
**nvidia**, **huggingface**, **openrouter**, **sambanova**, **cline** — get a
1-token `chat/completions` confirmation probe instead, so dead keys on those
providers are actually caught (their models endpoint returns 200 even for a
garbage key). Keys that fail authentication are marked exhausted in the
running key rings (visible in the footer and `/ctx-viz`).

Pass an upstream id to probe just that provider — useful when chasing one bad
key without waiting for the whole catalog.

```
/health
/health nvidia
/health groq
```

The same probe runs automatically at startup and every
`health_poll_interval_secs` (default 300s) in the background; the footer shows
a `⚠ N dead` marker when the last sweep found unhealthy keys.

---

### /verify

Run a single execute-and-verify round now: detects the project's test suite
and linter/typechecker, runs them in the sandbox configured by
`verify.sandbox` (`direct` / `git worktree` / `container`), and renders the
boxed per-check report — the same box the auto-verify loop draws after a
writing turn. Use it to check the tree at any time, or after disabling
auto-verify: a manual `/verify` overrides `verify.enabled: false`, so the
auto-loop's off-switch never blocks an on-demand check.

```
/verify          # run both tests and lints (default)
/verify test     # run only the test suite
/verify lint     # run only the linter/typechecker
```

After any verification round (auto-loop or manual `/verify`) the footer shows
a persistent `✓ verify` / `✗ verify` badge so the last round's outcome stays
visible even after the boxed report scrolls out of view; mid-loop auto-fix
rounds add the attempt counter (e.g. `✗ verify (2/3)`). Click the badge to
jump the transcript back to the latest verify box. While a round is running,
the status row shows a spinning `verifying…` indicator so the checks (which
can take a while in the container sandbox) never look like a hang. Each check
row in the box carries its wall-clock duration — e.g. `PASS (12s)` /
`FAIL (1s)` — so a slow test is visible at a glance.

Configure the round via `settings.json` (see the Verify loop section of
`configuration.md`): `verify.sandbox`, `verify.auto_test`, `verify.auto_lint`,
`verify.timeout_secs`, and for the container sandbox `verify.container_image`.

---

### /version
**Aliases:** `v`

Display the current Clawde version string and build metadata.

```
/version
/v
```

---

### /update
**Aliases:** `upgrade`

Check for available updates. Queries the GitHub releases API and displays the latest version. If a newer version exists, prints the download URL or upgrade instructions. Does not auto-update.

All GitHub API calls send the required `User-Agent`, `Accept`, and
`X-GitHub-Api-Version` headers.

```
/update
/upgrade
```

#### GitHub API rate-limit surfacing

When the anonymous API quota runs low (≤ 5 requests remaining), `/update` and
`/release-notes` append a warning with the reset countdown. A `403` response
appends a precise retry hint (`Retry after ~X min`) parsed from the current
response's `Retry-After` header or `X-RateLimit-Reset` timestamp — a
primary-limit 403 carries the reset in the body headers, so the timing shown is
the real one, not a stale store value.

The last-seen quota is persisted to disk (pruned once its reset window
passes) and surfaced in three places:

- the TUI footer as a `⚠ gh N/M (resets in ~X min)` badge when ≤ 5 requests
  remain (red at 0, yellow otherwise)
- `/ctx-viz` as a **GitHub API** section with `Requests: N / M`
  (green > 5, yellow ≤ 5, red at 0)
- `/doctor` as a `GitHub API: N/M requests` line

---

## Export & Sharing

### /export

Export the current session transcript. Supported formats include Markdown, JSON, and plain text. The output is written to a file or printed to stdout.

```
/export
/export --format markdown
/export --format json --output session.json
/export --stdout
```

---

### /copy

Copy the most recent assistant response to the system clipboard. Pass a number to copy the Nth most-recent response. On Linux a `wl-clipboard` or `xclip` backend is used; on macOS and Windows the native clipboard API is used.

```
/copy         — copy the most recent response
/copy 2       — copy the second most recent response
/copy N       — copy the Nth most recent response
```

---

## Advanced & Internal

### /thinking

Documented above under [Model & Provider](#model--provider).

---

### /connect

Documented above under [Model & Provider](#model--provider).

---

### /fork

Documented above under [Session & Navigation](#session--navigation).

---

### /effort

Documented above under [Model & Provider](#model--provider).

---

### /summary

Generate a summary of the current session. The model produces a condensed description of what was accomplished. Primarily used internally for session metadata.

```
/summary
```

---

### /brief

Output a brief status message for use in non-interactive contexts. Renders minimal session info without the full TUI.

```
/brief
```

---

### /context

Documented above under [Search & Files](#search--files).

---

### /sandbox-toggle
**Aliases:** `sandbox`

Enable or disable sandboxed execution of shell commands. When sandbox mode is on, bash/shell commands run in an isolated environment to limit unintended side effects. Supported on macOS, Linux, and WSL2.

```
/sandbox-toggle                          — toggle sandbox mode on/off
/sandbox-toggle on                       — enable sandbox mode
/sandbox-toggle off                      — disable sandbox mode
/sandbox-toggle status                   — show current state and excluded patterns
/sandbox-toggle exclude <pattern>        — add a command pattern to the exclusion list
```

> A restart is recommended after toggling for full effect. On Windows (non-WSL), sandbox mode is not supported.

---

### /think-back
**Aliases:** `thinkback`

Display the extended-thinking traces from previous model responses in the current session. Only available when extended thinking was used for those responses. Pass a number to view the Nth most-recent trace.

```
/think-back         — show the most recent thinking trace
/think-back 2       — show the second most recent thinking trace
/thinkback          — alias
```

Thinking traces appear when the model uses extended thinking mode (see `/thinking`). If no traces are found, Clawde suggests enabling extended thinking.

---

### /thinkback-play

Replay a previous extended-thinking trace as a formatted, step-numbered walkthrough. Useful for reviewing the model’s reasoning path in detail.

```
/thinkback-play         — replay the most recent thinking trace
/thinkback-play 2       — replay the second most recent thinking trace
```

---

## Command Availability

Not all commands are available in all contexts.

### Remote Mode

When running with `--remote`, only a restricted set of commands is available:

`session`, `exit`, `clear`, `help`, `theme`, `vim`, `cost`, `usage`, `plan`, `keybindings`, `statusline`

### Bridge Mode

Over the Remote Control bridge (used by IDE integrations), only `local`-type commands are forwarded:

`compact`, `clear`, `cost`, `files`

### Internal-Only Commands

The following commands are only available when the `USER_TYPE` environment variable is set to `ant` (Anthropic internal builds):

`commit-push-pr`, `ctx_viz`, `good-claude`, `issue`, `init-verifiers`, `mock-limits`, `bridge-kick`, `ultraplan`, `summary`, `teleport`, `ant-trace`, `perf-issue`, `env`, `oauth-refresh`, `debug-tool-call`, `autofix-pr`, `bughunter`, `backfill-sessions`, `break-cache`

### Availability-Restricted Commands

Some commands are available only under certain account or platform conditions:

| Command | Restriction |
|---------|-------------|
| `/fast` | Available when a fast-mode model is configured for the active provider |
| `/privacy-settings` | Opens Anthropic privacy portal (useful for claude.ai accounts) |
| `/sandbox-toggle` | Functional on macOS, Linux, WSL2 only; no-op on native Windows |

### Feature-Flagged Commands

Some commands check `isEnabled()` at runtime. For example, voice-related commands check for audio device availability; the desktop command checks for a display server.
