# Clawde Cheat Sheet

The essentials. Type `/command` at the prompt. For everything else, type `/help` or press `Ctrl+K` for the command palette.

---

## Start & quit

| Command | What it does |
|---------|--------------|
| `clawde` | Start the interactive TUI (from any directory) |
| `clawde -p "fix the bug in main.rs"` | One-shot headless query, prints result and exits |
| `clawde --resume` | Resume your most recent session |
| `/exit` or `/quit` | Quit Clawde |

## The 10 you'll use every day

| Command | What it does |
|---------|--------------|
| `/help` | Show help |
| `/model` | Change the AI model |
| `/new` | Start a fresh session |
| `/resume` | Resume a previous session |
| `/clear` | Clear the current conversation |
| `/compact` | Free up context when it fills up |
| `/commit` | Commit your current changes |
| `/diff` | See the current git diff |
| `/undo` | Undo the last assistant turn |
| `/context` | Check context / rate-limit usage |

## Sessions

| Command | What it does |
|---------|--------------|
| `/rename` | Rename the current session |
| `/history` | List recent sessions for this project |
| `/session` | Browse and manage all sessions |
| `/fork` | Fork the session into a new branch |

## Model & provider

| Command | What it does |
|---------|--------------|
| `/connect` | Set up an AI provider (first-time setup) |
| `/providers` | List providers and their status |
| `/keys` | Add / manage API keys |
| `/effort` | Set effort level: low / medium / high / max |
| `/fast` | Toggle fast mode |
| `/ollama` | Toggle Ollama connectivity mode (online / isolated) |

## Code & git

| Command | What it does |
|---------|--------------|
| `/review` | Review your changes (git diff) |
| `/spec` | Create or review an implementation spec |
| `/init` | Initialize AGENTS.md for this project |
| `/checkpoints` | List file-change checkpoints |
| `/revert` | Revert a file change from a turn |

## Memory & info

| Command | What it does |
|---------|--------------|
| `/memory` | Browse AGENTS.md memory files |
| `/stats` | Token and cost stats |
| `/cost` | Cost breakdown |
| `/status` | Provider and session status |
| `/doctor` | Run diagnostics if something's wrong |

## Self-hosting: Cat Chat & Katban

Two separate products:

- **Cat Chat** is a public-facing sandbox for guest conversations.
- **Katban** is the real development feature for agent Kanban work, project repos, execution, review, verification, and dev sites.
- **Katban attempts:N** — `clawde katban board attempts <N>` runs each card up to N times on different free-catalog model families; the verify gate promotes the first pass. `board attempts-upstreams [ids...]` pins the rotation (empty = auto). `board runtime incus` moves attempts into ephemeral Incus containers (in-container verify, FS-manifest scope gate via `board card scope <ID> <paths...>`); default `host`.

They use separate state directories and separate password policies. See [docs/catchat.md](catchat.md) and [docs/katban.md](katban.md).

| Command | What it does |
|---------|--------------|
| `/chat` | Open the Cat Chat link manager (popup) |
| `/chat create <NAME>` | Create a guest link — prints the password once |
| `/chat links` / `/chat show <ID>` | List links / link details |
| `/chat password <ID>` | Rotate a Cat Chat link password (`--set "PW"` chooses the exact guest password) |
| `/chat revoke <ID>` | Delete a link (kicks its devices) |
| `/chat unblock <IP>` | Clear login lockouts for an IP |
| `/chat status` | Links, devices, exposure, lockouts |
| `/katban ...` | Katban development: `board list`, `card add`, `project list`, `site list`, `board password` |
| `Alt+G` | Katban controls menu (board quick actions) |

Guest-link management lives under `/chat`, not `/katban` — old `/katban link ...` commands redirect there.

## Shell (outside Clawde)

| Command | What it does |
|---------|--------------|
| `clawde --version` | Show version |
| `clawde upgrade` | Update to the latest version |
| `clawde build` | Rebuild from source + update the running binary |
| `clawde stats` | Session stats without launching the TUI |
| `clawde --cwd <dir>` | Start in a specific directory |
| `clawde catchat serve` | Run the Cat Chat server (guest chat) |
| `clawde catchat expose` | Put Cat Chat on a public domain via caddy |
| `clawde catchat links ...` | Manage Cat Chat guest links from the shell |
| `clawde catchat status` | Cat Chat overview (links, devices, exposure) |
| `clawde katban ...` | Kanban board + site hosting from the shell |

**Build & update:** after changing code, run `clawde build` — it compiles the local source and replaces itself, so `clawde` from any directory is current. Add `--debug` for a fast dev build, `--no-install` to only compile.

---

## Handy keys

| Key | What it does |
|-----|--------------|
| `Enter` | Submit your message |
| `Shift+Enter` | New line (multi-line message) |
| `Ctrl+K` | Command palette (search any command) |
| `Ctrl+,` | Open settings |
| `Alt+J / Alt+K` | Free models (dropdown) |
| `Alt+H / Alt+L` | Reasoning down / up (clamped) |
| `Alt+R` | Search command history |
| `Ctrl+O` | Expand / collapse thinking blocks |
| `Alt+/` | Open help |
| `Esc` | Close a dialog / cancel. While streaming: pause the live transcript — press any key to resume, or `Esc` again to stop the turn. `Ctrl+C` stops directly |
| `Up / Down` | Previous / next prompt in history |

---

**Rule of thumb:** If you forget a command, press `Ctrl+K` and type what you want to do — the palette finds it.
