# Sessions & History

Clawde keeps **four separate records**. They store different things, live in
different places, and are meant for different jobs. This page explains what
each one is, how to use it, and — importantly — what it is **not**.

| Concept | Records | Lives in | Accessed via |
|---|---|---|---|
| **Session history** | Full conversation: messages, title, model, cost | `~/.clawde/sessions/<id>.json` | `/session`, `/resume`, `/fork`, `/rename` |
| **Project history** | All sessions for the current directory/project | `~/.clawde/projects/<encoded-path>/<id>.jsonl` | Welcome-screen "Recent activity", `/history`, `/stats` |
| **Prompt history** | Your past prompts, tagged by project + session | `~/.clawde/history.jsonl` | `↑` / `↓` in the input box |
| **File history / checkpoints** | Files changed per assistant turn | In-memory + shadow-git snapshot in the session file | `/undo`, `/revert`, `/checkpoints`, `/snapshot`, `/files` |

*(Default location is `~/.clawde`; set `CLAWDE_HOME` to relocate it.)*

---

## 1. Session history

**What it is.** The authoritative record of a conversation — every user and
assistant message, the session title, model, working directory, and running
cost/token totals. Saved after every turn, every ~30 seconds while you are
chatting, and on exit (even on SIGTERM).

**How to use it.**

- `/resume` or `clawde --resume` — continue your **most recent** session.
  `/resume <id>` / `clawde --resume <id>` — resume a specific one.
- `/session` — show the current session + a list of recent ones;
  `/session list` — all sessions.
- `/rename <title>` — give the session a searchable name.
- `/fork [index]` — branch a copy of the conversation from a message index.
- `/cost` — cost for the current session.

**What it is NOT.**

- ✗ Not a backup of your files — it stores the *conversation*, not your
  working tree.
- ✗ Not cloud-synced by default — it is local-only unless you are using a
  remote-session bridge.
- ✗ Not a log of every keystroke — only submitted prompts and model replies.
- ✗ Session IDs are the *only* stable handle — renaming a title does not
  change the ID, and deleting the file deletes the session.

---

## 2. Project (directory) history

**What it is.** All sessions grouped by the **project root** — the git repo
root of the directory you launched Clawde in (or the directory itself if not
a repo). The root path is encoded into the folder name, so every session
opened in that project lands in one place, and the welcome screen shows your
most recent sessions **for that project**.

**How to use it.**

- Launch Clawde in a project — the welcome screen's right column lists that
  project's recent sessions with AI-generated titles and timestamps; click
  one to resume it.
- `/history` — print an overview of all sessions for the current project
  with full timestamps and store locations.
- `/stats` (optionally `/stats --all`) — aggregated usage across your
  projects.

**What it is NOT.**

- ✗ Not a per-folder *file* history — it is a per-project *session* list.
  File changes are tracked by the in-memory file history and shadow-git
  snapshots, not the JSONL transcript.
- ✗ Not a backup — the JSONL transcript is append-only and capped at 50 MB.
  For durable history, rely on the session JSON files in `~/.clawde/sessions/`.

---

## 3. Prompt history

**What it is.** A global, append-only log of every prompt you have submitted
across all sessions. Each line is a JSON object with the prompt text, session
ID, project root, and timestamp. Tagged by project for per-project recall.

**How to use it.**

- `↑` / `↓` in the input box — cycle through recent prompts (per-project by
  default, with prefix-matching).
- `/search <query>` — find prompts across sessions.

**What it is NOT.**

- ✗ Not a full transcript — it only stores the user's prompts, not the
  model's responses.
- ✗ Not a versioned history — it is append-only and never edited.
- ✗ Not a keystroke log — only prompts that are submitted.

---

## 4. File history / checkpoints

**What it is.** A record of every file change Clawde has made during the
current session, tracked in memory and persisted as shadow-git snapshots.
Every assistant turn that writes files gets a lightweight snapshot, forming a
linear undo stack.

**How to use it.**

- `/undo` — undo the file changes from the last assistant turn.
- `/revert [n]` — revert to the state at turn `n` (non-destructive — uses
  the branch/leaf system in the JSONL transcript).
- `/checkpoints` — list all snapshots with timestamps.
- `/snapshot [n]` — show the diff for a specific checkpoint.
- `/files` — list all files tracked (read or written) in the active session.

**What it is NOT.**

- ✗ Not a replacement for `git commit` — snapshots are ephemeral and scoped
  to the session. They are cleaned up periodically and do not survive a
  `git reset` or worktree deletion.
- ✗ Not a full VCS — there is no branching, merging, or remote sync. The
  shadow-git store is a linear undo buffer, not a version-control system.

---

## Cheat sheet

| Command | What it does |
|---|---|
| `clawde --resume [id]` | Launch and continue a session |
| `/resume [id]` | Continue a session in-place |
| `/session [list]` | Inspect the current / recent sessions |
| `/history` | List all sessions for the current project |
| `/new`, `/fork [i]` | Start fresh / branch the conversation |
| `/rename <title>` | Name a session |
| `/undo` | Roll back file changes from the last turn |
| `/revert [n]` | Non-destructively revert to turn `n` |
| `/checkpoints` | List all file-change snapshots |
| `/snapshot [n]` | Show diff for a checkpoint |
| `/search <query>` | Find sessions by title / tags / content |
| `/stats` | Usage across your project transcripts |