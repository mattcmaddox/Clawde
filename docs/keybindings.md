# Clawde Keybindings Reference

This document covers all keyboard shortcuts in Clawde, how to customize them, vim mode, and special input behaviors.

---

## Table of Contents

1. [Default Keybindings](#default-keybindings)
   - [Global Context](#global-context)
   - [Chat Context](#chat-context)
   - [Confirmation Context](#confirmation-context)
2. [Keybinding Contexts](#keybinding-contexts)
3. [Customizing Keybindings](#customizing-keybindings)
   - [Via /keybindings command](#via-keybindings-command)
   - [Via keybindings.json](#via-keybindingsjson)
   - [Chord Bindings](#chord-bindings)
4. [Non-Rebindable Keys](#non-rebindable-keys)
5. [Vim Mode](#vim-mode)
6. [Special Input Behaviors](#special-input-behaviors)
   - [Shift+Enter for Newline](#shiftenter-for-newline)
   - [ESC During Streaming](#esc-during-streaming)
   - [@file Injection with Typeahead](#file-injection-with-typeahead)
7. [Non-English Keyboard Layout Support](#non-english-keyboard-layout-support)

---

## Default Keybindings

### Global Context

These bindings are active in all contexts.

| Key | Action | Description |
|-----|--------|-------------|
| `Ctrl+C` | interrupt | Interrupt the current operation (non-rebindable) |
| `Ctrl+D` | exit | Exit Clawde (non-rebindable) |
| `Ctrl+L` | redraw | Redraw the terminal screen |
| `Ctrl+/` | showKeybindings | Open the keybinding cheat-sheet overlay |
| `Alt+R` | historySearch | Open interactive history search |
| `Alt+B` | createBranch | Create a new git branch |
| `Alt+C` | compact | Compact the conversation |
| `Alt+G` | openKatbanControls | Open the Katban controls menu (links, boards, IPs) |
| `Alt+S` | showSources | Toggle the sources display |
| `Alt+/` | openHelp | Open the help panel |
| `Alt+Shift+H` | clearFollowupHistory | Clear the followup history |
| `Alt+Shift+U` | clearFollowupUsage | Clear the followup usage counters |

### Chat Context

These bindings are active when focus is in the chat input field.

| Key | Action | Description |
|-----|--------|-------------|
| `Enter` | submit | Submit the current message to the model |
| `Shift+Enter` / `Ctrl+J` / `Alt+Enter` | newline | Insert a literal newline without submitting (`Ctrl+J` is the fallback for terminals without the CSI-u/kitty protocol) |
| `Home` / `Cmd+Left` / `Ctrl+A` | goLineStart | Move cursor to beginning of line |
| `End` / `Cmd+Right` / `Ctrl+E` | goLineEnd | Move cursor to end of line |
| `Ctrl+Left` / `Alt+B` | moveWordBackward | Move one word left |
| `Ctrl+Right` / `Alt+F` | moveWordForward | Move one word right |
| `Ctrl+W` / `Alt+Backspace` | killWord | Delete the word before the cursor |
| `Alt+D` | deleteWord | Delete the word after the cursor |
| `Ctrl+H` | deleteCharBefore | Delete the character before the cursor |
| `Ctrl+U` | killToStart | Delete from the cursor to the beginning of the line |
| `Ctrl+Shift+L` | clearLine | Clear the current input line |
| `Up` | historyPrev | Navigate to the previous entry in input history |
| `Down` / `Ctrl+I` | historyNext | Navigate to the next entry in input history |
| `Shift+K` / `Shift+J` | verticalPrev / verticalNext | Move the selection up / down in list widgets (resolved to the arrow keys) |
| `Ctrl+O` | toggleThinkingExpand | Expand or collapse all thinking blocks |
| `Alt+Left` | previousMessage | Jump to the previous user/assistant message |
| `Alt+Right` | nextMessage | Jump to the next user/assistant message |
| `Alt+N` | jumpToNextError | Jump to the next error / issue |
| `Alt+.` | jumpToPreviousError | Jump to the previous error / issue |
| `Tab` | cycleAgentMode | Complete the open suggestion, otherwise cycle the agent mode (build → plan → image). Text already in the input is kept |
| `Shift+Tab` | cyclePermissionMode | Cycle the permission mode (Default → Accept edits → Bypass → Default) |
| `Alt+P` | expandPaste | Expand a `[Pasted text #N]` placeholder |
| `Alt+I` | pasteImage | Attach an image from the clipboard |
| `Alt+Shift+I` | openAttachments | Open the attachments overlay (toggle / add / remove pending images) |
| `Page Up` / `Page Down` | scrollUp / scrollDown | Scroll the conversation view up / down one page |
| `Alt+M` | openModelPicker | Open the interactive model picker |
| `Alt+Shift+M` | openModePicker | Open the mode picker |
| `Ctrl+,` | openSettings | Open the settings screen |
| `Ctrl+K` | openCommandPalette | Open the slash command palette |
| `Alt+J` / `Alt+K` | openFreeModelPopup | Open the free-model dropdown (auto + every configured free upstream); Enter pins the selection |
| `Alt+U` | cycleFreeUpstream | Cycle to the next free-mode upstream (forward alias) |
| `Alt+T` | cycleFreeTask | Cycle the free-model task filter |
| `Alt+O` | openOllamaConfig | Open the Ollama configuration screen |
| `Alt+H` / `Alt+L` | effortDecrease / effortIncrease | Step reasoning down / up along the model's supported ladder (clamped, no wrap) |
| `Alt+E` | openEffort | Open the effort/reasoning picker |
| `Tab H` / `Tab L` | effortDecrease / effortIncrease | Chord aliases for `Alt+H` / `Alt+L` |

> **Modifier theme:** `Ctrl` drives text editing and app shortcuts; `Alt` drives navigation and configuration. A few shifted chords remain where they avoid a collision — `Ctrl+Shift+L` (clear line), `Alt+Shift+M` (mode picker), `Alt+Shift+I` (attachments overlay), and `Alt+Shift+H` / `Alt+Shift+U` (followup history / usage). Older `keybindings.json` files are auto-migrated.

### Confirmation Context

These bindings are active when Clawde is displaying a yes/no confirmation prompt (e.g., tool permission requests).

| Key | Action | Description |
|-----|--------|-------------|
| `Y` / `y` | confirm | Approve the pending action |
| `N` / `n` | deny | Deny the pending action |
| `A` / `a` | alwaysAllow | Approve and add a permanent allow rule |
| `Enter` | defaultAction | Accept the highlighted default option |
| `Escape` | cancel | Cancel the prompt and deny the action |

### Plugin Context

Active while the plugin-list overlay (bare `/plugin`) is open.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `k` | prev | Move the highlight up |
| `Down` / `j` | next | Move the highlight down |
| `Shift+K` / `Shift+J` | verticalPrev / verticalNext | Move the highlight up / down (resolved to the arrow keys) |
| `Enter` | select | Show/hide the detail panel for the highlighted plugin |
| `Escape` / `q` | cancel | Close the overlay |

### Attachments Context

Active while the attachments overlay (`Alt+Shift+I`) is open over the prompt's pending images.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `k` | prev | Move the highlight up |
| `Down` / `j` | next | Move the highlight down |
| `Shift+K` / `Shift+J` | verticalPrev / verticalNext | Move the highlight up / down (resolved to the arrow keys) |
| `Space` | toggle | Include / exclude the highlighted image from the next send |
| `a` | addAttachment | Attach another image from the clipboard |
| `r` | removeAttachment | Remove the highlighted image |
| `Escape` / `q` | cancel | Close the overlay |

### MCP View Context

Active while the MCP server/tool view is open. It has its own context (rather
than reusing `Select`) so `h`/`l` can cycle panes without colliding with
`Select`'s vim-preset `h`/`l` prev/next.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` | prev | Move the highlight up |
| `Down` | next | Move the highlight down |
| `Tab` / `Left` / `Right` / `h` / `l` | cyclePane | Cycle server list → tool list → tool detail |
| `Escape` / `q` | cancel | Close the view |

`j`/`k` are not bound here on purpose: in this view they are conditional (they
type into the tool filter once it has text), so the view's own handler decides
when they navigate versus type.

### Select Context

Active while a generic modal select is open: the agents menu and the stats
dialog.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `k` | prev | Move the selection, or scroll up (stats dialog) |
| `Down` / `j` | next | Move the selection, or scroll down (stats dialog) |
| `Shift+K` / `Shift+J` | verticalPrev / verticalNext | Resolved to the arrow keys |
| `PageUp` / `PageDown` | pageUp / pageDown | Scroll a page (stats dialog) |
| `Enter` | select | Confirm the highlighted item |
| `Escape` / `q` | cancel | Close the dialog |

Vim mode adds `h` / `l` prev/next (its preset extends this context).

The hooks config menu (`/hooks`) shares this context: `Enter` drills into the
next level and `Escape` / `q` step back one level (closing at the top).

`PageUp` / `PageDown` only act in the stats dialog; the agents menu has nothing
to page. `/` is bound to `search` for this context but no current select
consumes it. Keys that stay with each view's own handler: the agents menu's
`Left` (back), `Right` (confirm) and `Backspace` (close); the stats dialog's
`Tab` / `Right` (next tab), `Shift+Tab` / `Left` (previous tab) and `r` (cycle
range).

### Diff Dialog Context

Active while the diff viewer is open.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `k` | prevDiff | Previous file, or scroll the detail pane up |
| `Down` / `j` | nextDiff | Next file, or scroll the detail pane down |
| `Shift+K` / `Shift+J` | verticalPrev / verticalNext | Resolved to the arrow keys |
| `PageUp` / `PageDown` | pageUp / pageDown | Scroll the detail pane |
| `Escape` / `r` | rejectDiff | Dismiss the viewer |
| `q` | cancel | Dismiss the viewer (same effect as `rejectDiff`) |

Vim mode adds `h` / `l` prevDiff/nextDiff.

The viewer is read-only, so it carries only navigation and dismiss actions —
there is no pending change to accept. `Tab` / `Left` / `Right` (switch pane),
`d` (toggle diff type) and `Space` (collapse the highlighted file in the file
list) stay with the view's own handler.

### Keybindings Context

Active while the `/keybindings` reference overlay is open.

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `Down` | prev / next | Scroll one line |
| `PageUp` / `PageDown` | pageUp / pageDown | Scroll a page |
| `Home` / `End` | first / last | Jump to the top / bottom |
| `Escape` / `q` | cancel | Close the overlay |

Vim mode adds `h` / `l` prev/next. `j` / `k` are deliberately **not** bound:
while the filter is empty they scroll, but once it has text they type into it,
so the view's own handler decides. The filter bar itself (`Backspace`, printable
characters) also stays view-local.

### Paste Viewer Context

Active while the read-only paste viewer is open (opened from a
`[Pasted text #N ...]` placeholder).

| Key | Action | Description |
|-----|--------|-------------|
| `Up` / `k` | prev | Scroll up one row |
| `Down` / `j` | next | Scroll down one row |
| `PageUp` / `PageDown` | pageUp / pageDown | Scroll a page |
| `Home` / `End` | first / last | Jump to the top / bottom |
| `Escape` / `q` | cancel | Close the viewer |

Vim mode adds `h` / `l` prev/next. `g` / `G` (jump to top / bottom) and `Alt+E`
(expand the paste in place, then close) stay with the view's own handler.

---

## Keybinding Contexts

Clawde uses a context system so that the same key can have different effects depending on where focus is. A key is resolved against the active context together with `Global`; where the same chord is bound in both, the context-specific binding wins.

Context strings are the exact `KeyContext` variant names (PascalCase) from `crates/core/src/keybindings.rs` — the same values the `/keybindings` editor writes into `keybindings.json`.

### Active contexts

These are the contexts the TUI produces while handling keys today.

| Context | Description |
|---------|-------------|
| `Global` | Available as a fallback from every context |
| `Chat` | The chat input / prompt, and the fallback when no overlay is focused |
| `Confirmation` | Permission requests, step 2 of the `/rewind` flow, and the import-config dialog |
| `Help` | The help overlay |
| `HistorySearch` | The history-search overlay |
| `ThemePicker` | The theme picker and theme creator screens |
| `Settings` | The settings screen |
| `DiffDialog` | The diff viewer |
| `Select` | Generic modal selects: agents menu, stats dialog, hooks config menu |
| `McpView` | The MCP server/tool view |
| `KeysDialog` | The `/keys` key-management dialog |
| `ModelsMenu` | The `/models` provider menu (Auto + per-provider on/off) |
| `Keybindings` | The `/keybindings` reference overlay |
| `PasteViewer` | The read-only paste viewer |
| `MessageSelector` | Step 1 of the `/rewind` flow (browse messages) |
| `ModelPicker` | The model picker overlay (open with `Alt+M` or `/model`) |
| `Task` | The task-list overlay (`Ctrl+T`) |
| `Plugin` | The plugin-list overlay (bare `/plugin`) |
| `Attachments` | The attachments overlay (`Alt+Shift+I`) over the prompt's pending images |

### Declared contexts

These contexts exist in `KeyContext` and can be named in `keybindings.json`, but no UI currently produces them while handling keys, so a binding in one has no effect yet.

| Context | Intended for |
|---------|-------------|
| `Autocomplete` | The prompt's suggestion / autocomplete list |
| `Transcript` | The transcript pane |
| `Tabs` | The tab bar |
| `Footer` | The status footer |

> **Vim mode is not a context.** It is the `vim` preset plus a per-dialog insert/normal state machine, so vim normal-mode `hjkl` come from the preset rather than a `vim.*` context.

---

## Customizing Keybindings

### Via /keybindings command

The `/keybindings` command opens an interactive TUI keybinding editor:

```
/keybindings
```

The editor lists all bindable actions grouped by context. Use arrow keys to navigate, press `Enter` on an action to enter rebind mode, then press the desired key combination. Press `Escape` to cancel a rebind. Changes are saved immediately to `~/.clawde/keybindings.json`.

### Via keybindings.json

For batch edits or scripted configuration, edit `~/.clawde/keybindings.json` directly. The file format is:

```json
{
  "schema_version": 3,
  "bindings": [
    {
      "context": "Chat",
      "action": "submit",
      "chord": "ctrl+enter"
    },
    {
      "context": "Global",
      "action": "historySearch",
      "chord": "ctrl+p"
    }
  ]
}
```

Each binding object has:

| Field | Type | Description |
|-------|------|-------------|
| `context` | string | Keybinding context (see table above) |
| `action` | string \| null | Action identifier (or `null` to unbind) |
| `chord` | string | Key combination in normalized form |

### Schema Versioning and Smart Merge

`keybindings.json` carries a top-level `schema_version` field (currently `3`). When Clawde's defaults change in a release, the file is auto-migrated on next launch:

1. Clawde reads the file and compares `schema_version` against the bundled `KEYBINDINGS_SCHEMA_VERSION`.
2. If the file is older, Clawde runs a **smart merge**:
   - Your customizations (any binding whose `chord` you set explicitly) are preserved.
   - Stale bindings that match an *old* default that has since changed are dropped — for example, the previous `ctrl+a → openModelPicker` binding is removed because `ctrl+a` is now reserved for select-all in the input, and the retired `tab → indent` is dropped in favour of `tab → cycleAgentMode` because the action never inserted indentation.
   - Any new bindings present in the current defaults but not in your file are added.
3. The migrated file is written back with the new `schema_version`.

A warning is logged whenever a migration occurs. If you want to opt out of the merge for a binding, leave a different action assigned to it — explicit customizations always win.

Setting `"action": null` for a chord explicitly **unbinds** the default — useful when you want a key to do nothing rather than fire its default action.

Key notation uses lowercase letters, with modifier prefixes separated by `+`:

| Prefix | Modifier key |
|--------|-------------|
| `ctrl+` | Control |
| `alt+` | Alt / Option |
| `shift+` | Shift |
| `super+` | Super / Cmd |

Special key names: `enter`, `escape`, `tab`, `backspace`, `delete`, `up`, `down`, `left`, `right`, `home`, `end`, `pageup`, `pagedown`, `f1` through `f12`.

After editing the file, run `/keybindings` and then exit to trigger a reload, or restart Clawde.

### Chord Bindings

Clawde supports chord bindings — multi-key sequences where you press a leader key and then a follow-up key. Chord bindings are defined with a `chord` array instead of a single `key`:

```json
{
  "context": "Chat",
  "action": "openModelPicker",
  "chord": ["ctrl+x", "ctrl+m"]
}
```

The first key in the chord acts as the leader. After pressing the leader key, Clawde enters a brief chord-wait state (500 ms by default). If the follow-up key arrives within that window, the chord fires. If the timeout expires or a different key is pressed, the leader key's default action (if any) fires instead.

Chords can be up to two keys deep. Three-key chords are not supported.

Example — map `Ctrl+X Ctrl+C` to exit:

```json
{
  "context": "Global",
  "action": "exit",
  "chord": ["ctrl+x", "ctrl+c"]
}
```

---

## Non-Rebindable Keys

The following keys have fixed behavior and cannot be rebound:

| Key | Fixed behavior |
|-----|---------------|
| `Ctrl+C` | Interrupt current operation / send SIGINT to foreground process |
| `Ctrl+D` | Exit Clawde when input is empty; signal EOF when input has content |
| `Ctrl+M` | Identical to `Enter` at the terminal level (terminals emit `CR` for both) |

These keys are handled at the terminal input layer before the keybinding system processes events. If any of them appear as a `chord` in `keybindings.json`, Clawde:

1. Logs a warning at startup (`Cannot rebind protected key '<chord>' in keybindings.json`).
2. **Filters the binding out** of the loaded set before resolving any keystrokes.

So overriding a protected key is a no-op in behavior, but you also get a clear signal in the logs that the binding was rejected.

---

## Vim Mode

Vim mode replaces the default line editor with a modal input field that mimics vim's normal, insert, and visual modes.

### Enabling Vim Mode

```
/vim
/vim on
/vim off
```

Or set it persistently:

```
/config set vim true
```

### Vim Mode Keybindings

In vim mode the input field has three modes:

**Insert mode** — behaves like the normal chat input; type freely, `Escape` returns to normal mode.

**Normal mode** — movement and editing commands:

| Key | Action |
|-----|--------|
| `h` / `l` | Move cursor left / right |
| `j` / `k` | History prev / next |
| `w` / `b` | Move forward / backward by word |
| `0` / `$` | Move to line start / end |
| `i` | Enter insert mode at cursor |
| `a` | Enter insert mode after cursor |
| `A` | Enter insert mode at end of line |
| `I` | Enter insert mode at beginning of line |
| `x` | Delete character under cursor |
| `dd` | Delete entire line |
| `u` | Undo last change |
| `Ctrl+R` | Redo |
| `yy` | Yank (copy) line |
| `p` | Paste after cursor |
| `Enter` | Submit message |
| `/` | Enter inline search |
| `Escape` | Clear pending command / return to normal |

**Visual mode** — entered with `v` from normal mode; use movement keys to select text, then:

| Key | Action |
|-----|--------|
| `y` | Yank selection |
| `d` | Delete selection |
| `Escape` | Exit visual mode |

### Vim Mode Indicator

When vim mode is active, a mode indicator (`NORMAL`, `INSERT`, `VISUAL`) is displayed in the status line.

### Vim Navigation in Dialogs and Overlays

When vim mode is enabled, list-based dialogs and overlays (connect, model picker, tasks, help, history search, session browser, mcp view, command palette, and others) accept `j`/`k` (and `h`/`l` for horizontal pickers like effort) as navigation, in addition to the arrow keys. The `j`/`k`/`h`/`l` navigation is **only active when vim mode is on**:

| Vim mode | `j` / `k` in a list dialog |
|----------|---------------------------|
| Off | Types into the search/filter bar (legacy type-to-filter behavior) |
| On | Moves selection up/down (arrow keys keep working in both modes) |

This matches the modal search-bar convention: in vim mode, letters do not type into a popup's filter until you press `i` to enter insert mode. In normal mode, `j`/`k` navigate and `Esc` exits insert first (a second `Esc` closes the popup). Text-entry dialogs (key input, custom provider, free-mode fields) open in insert mode so typing works immediately.

A few dialogs have extra guards:

- **Free-mode dialog:** `j`/`k`/`h`/`l` only navigate when the active field has no pending typed text, so you can't lose a partially typed key by moving rows.
- **Ask-user dialog:** `j`/`k` navigate only when not typing a custom answer.
- **Diff viewer:** it has no filter bar, so the `DiffDialog` context binds `j`/`k` to prev/next unconditionally — they navigate the file list or scroll the detail pane in either vim mode. (`h`/`l` are prev/next only under the vim preset.)

---

## Special Input Behaviors

### Shift+Enter for Newline

Pressing `Shift+Enter` in the chat input field inserts a literal newline character without submitting the message. This is the standard way to write multi-line prompts.

Pressing plain `Enter` always submits the message regardless of the number of lines already in the input buffer.

In vim insert mode, `Enter` also submits. Use `Shift+Enter` for newlines in vim mode as well.

### ESC During Streaming

Pressing `Escape` while the model is streaming a response interrupts the stream. The partial response is preserved in the conversation history and the model stops generating. The input field regains focus and you can send a follow-up message.

This is equivalent to pressing `Ctrl+C` during streaming, except that `Ctrl+C` also signals any tool calls in progress to abort (via `AbortController`), while `Escape` only stops the stream and allows running tools to finish.

---

### @file Injection with Typeahead

Type `@` followed by a path in the prompt to inject a file's contents into your message. The `@` token only triggers when it is at a word boundary (start of input or preceded by whitespace). As you type after the `@`, Clawde opens a typeahead completion overlay scanning the current working directory.

```
explain @src/main.rs and compare to @tests/integration.rs
```

When you press `Enter`, Clawde:

1. Scans the message for `@<path>` tokens at word boundaries.
2. Resolves each path relative to the working directory; `~/` expands to your home directory.
3. Reads the file contents and substitutes them inline before sending the prompt to the model.

The `@` reference works with:

- Plain absolute paths: `@/etc/hosts`
- Paths relative to cwd: `@src/main.rs`
- Home-relative paths: `@~/.bashrc`
- Trailing punctuation is stripped: `@src/main.rs.` is treated as `@src/main.rs`

An `@` that is *not* at a word boundary (e.g. inside an email `me@example.com`) is left alone — neither the typeahead nor the file injection triggers.

**Limits and warnings.** If a referenced path is too large, binary, a directory, or unreadable, Clawde opens a confirmation dialog before sending:

| Issue | Behavior |
|-------|----------|
| File exceeds size limit | Dialog offers "Allow anyway" or "Abort" |
| Binary file | Dialog warns; same choice |
| Path is a directory | Dialog warns; cannot inject (must remove or rewrite the @ref) |
| Path unreadable | Skipped; error shown in dialog |

Files that pass all checks are injected silently — no dialog is shown.

**Images are attached, not injected.** An `@` reference to an image file
(`.png`, `.jpg`/`.jpeg`, `.gif`, `.webp`, `.bmp`) is never pasted as text —
it is attached as a real image block so vision models see the pixels.
Relative paths (`@screenshots/shot.png`) resolve against the working
directory exactly like text files, and the actual MIME type is sniffed from
the file bytes, so a JPEG renamed to `.png` is still sent as `image/jpeg`.
This works without any clipboard tooling, which is what makes it usable over
SSH.

**Configuration.** Two settings in `~/.clawde/settings.json`:

| Setting | Default | Description |
|---------|---------|-------------|
| `fileInjectionEnabled` | `true` | Master switch — set to `false` to disable @-injection entirely |
| `fileInjectionMaxSize` | `100` | Per-file size limit in KB; `0` disables the check (accept all) |

These can also be edited in the in-app settings screen.

**Typeahead navigation.** While the completion overlay is open:

| Key | Action |
|-----|--------|
| `Up` / `Down` | Move selection |
| `Tab` / `Enter` | Insert the highlighted completion |
| `Escape` | Dismiss the overlay (keep typed text) |

---

## Non-English Keyboard Layout Support

### The Problem

Terminal key events for `Ctrl+<key>` combinations are reported as raw control codes (`0x01` through `0x1A` for `Ctrl+A` through `Ctrl+Z`). These codes map to the physical QWERTY key position, not the character printed on the key.

On non-English keyboard layouts (Cyrillic, Arabic, Greek, CJK, etc.), the Latin letters used in Clawde's shortcuts may not appear on the keycaps, and some input methods send layout-translated scan codes for Ctrl combinations — causing Clawde to miss the shortcut entirely.

### The Fix

Clawde resolves this by mapping `Ctrl+<scancode>` events to their QWERTY positional equivalents before keybinding lookup. Concretely:

1. When a `Ctrl+<key>` event arrives, the physical scan position is extracted.
2. That position is mapped to the corresponding QWERTY letter (e.g., physical position of the Cyrillic `Ф` key = QWERTY `A` position).
3. The resulting `Ctrl+A` event is passed to the keybinding system.

This means `Ctrl+Ф` on a Cyrillic layout fires the same action as `Ctrl+A` on a QWERTY layout, regardless of the active input method or language setting. All `Ctrl+<letter>` keybindings in the default table above work by physical position.

### Implications for Custom Bindings

If you add a binding for `ctrl+a` in `keybindings.json`, it will fire when you press `Ctrl` and the key in the `A` position on your physical keyboard, regardless of what character that key is labeled. This is intentional.

If you want a binding that fires only when the `A` character is actually produced (i.e., layout-aware), prefix the key with `char:`:

```json
{
  "context": "Chat",
  "action": "myAction",
  "key": "ctrl+char:a"
}
```

Layout-aware bindings are not recommended for the standard workflow bindings because they break on non-QWERTY layouts.

### Alt Key on macOS

On macOS, `Alt` (Option) key combinations produce special Unicode characters at the OS level before they reach the terminal. Clawde intercepts these at the terminal input layer and re-emits them as `alt+<key>` events using the same positional mapping described above.

If an `alt+<key>` binding does not fire on macOS, check whether your terminal emulator is configured to send `Escape + key` sequences for Option key combinations (the iTerm2 and Alacritty option is "Use Option as Meta Key" or equivalent).
