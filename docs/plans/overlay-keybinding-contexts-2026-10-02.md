# Overlay Keybinding Contexts — Refactor Plan (2026-10-02)

Dated record of a refactor whose shape is easy to get wrong. Written so the
reasoning survives the code.

## The problem

Every modal overlay is supposed to resolve its navigation keys through the
configurable keybinding system (a `KeyContext` per overlay, `default_bindings`
entries, and a `handle_*_navigation` helper that calls
`resolve_dialog_action`). Three overlays still hardcode their keys in a raw
`match key.code` inside `handle_key_event`, so those keys cannot be rebound and
drift from the rest of the app:

| Overlay | Where the handler lives | State |
|---|---|---|
| `/keybindings` reference | inline in `handle_key_event` | `KeybindingsOverlayState` |
| hooks config menu | inline in `handle_key_event` | `HooksConfigMenuState` |
| paste viewer | `handle_paste_viewer_key` | `PasteViewer` |

The earlier `q`-close work only touched overlays that *already* had a context
(`McpView`, `Plugin`, `Select`, `DiffDialog`). These three have none, so they
need contexts added before they can be routed.

## Target architecture

One uniform shape per overlay, identical to `handle_mcp_view_navigation` /
`handle_stats_dialog_navigation`:

1. **A context.** `current_key_context()` returns it while the overlay is
   visible, so the `/keybindings` editor can list and rebind its keys.
2. **A handler.** `handle_<overlay>_navigation(&self, key) -> bool` resolves the
   key via `resolve_dialog_action(key, &KeyContext::X)` and matches the action
   string. It returns `false` for unbound keys and for keys the view must decide
   itself (filter typing, vim state), letting the caller fall through to the
   view-local `match`.
3. **Defaults.** `default_bindings()` gains the navigation chords.
4. **View-local keys stay view-local.** Only keys whose *effect* is a genuine
   context concern are bound. Conditional keys (j/k while a filter can hold
   text) are deliberately left unbound, exactly as `McpView` does.

### Contexts to add

- `KeyContext::Keybindings` — the `/keybindings` reference overlay. Its `j`/`k`
  are conditional (they type into the filter once it has text), so they are
  **not** bound; `Up`/`Down` are. A dedicated context (not `Help` or `Select`)
  keeps the filter semantics clear and leaves room for the vim preset's `h`/`l`.
- `KeyContext::PasteViewer` — the read-only paste viewer. No filter, so `j`/`k`
  and the arrows are all unconditional and *are* bound.

### Reuse, don't invent

The **hooks config menu** has no filter and its controls (`prev` / `next` /
`select` / `cancel`) are exactly the `Select` vocabulary. It reuses
`KeyContext::Select` rather than adding a variant: the context is the thing that
must be rebindable, and a select is a select. `current_key_context()` and
`any_modal_open()` gain the overlay; nothing else changes.

### Action vocabulary

| Action | Keybindings overlay | Paste viewer | Hooks menu |
|---|---|---|---|
| `cancel` | `close()` | `close()` | `back()` (closes at the top level) |
| `prev` | `scroll_up()` | `scroll_up(1)` | `select_prev()` |
| `next` | `scroll_down()` | `scroll_down(1)` | `select_next()` |
| `pageUp` / `pageDown` | `page_up()` / `page_down()` | `page_up()` / `page_down()` | — |
| `first` / `last` | `scroll_to_top()` / `scroll_to_bottom()` | `scroll_to_top()` / `scroll_to_bottom()` | — |
| `select` | — | — | `enter()` |

`first`/`last` and `pageUp`/`pageDown` already exist (ModelPicker, Select), so
no new action strings are introduced.

### View-local keys that must stay

- `/keybindings`: `Backspace`/printable chars feed the filter; the `vim_search`
  state machine runs before anything else.
- paste viewer: `g`/`G` jump (kept, alongside `Home`/`End`) and `Alt+E` expands
  the paste in place then closes.
- hooks menu: nothing beyond the four actions.

## Migration steps

1. Add the two `KeyContext` variants and a `context_label` arm (the exhaustive
   match in `overlays.rs` is the compiler-enforced checklist).
2. Add defaults: Keybindings (`escape`/`q` cancel, `up`/`down` prev/next,
   `pageup`/`pagedown`, `home`/`end` first/last) and PasteViewer (`escape`/`q`
   cancel, `up`/`k` prev, `down`/`j` next, `pageup`/`pagedown`,
   `home`/`end`). Add `h`/`l` prev/next for both in the vim preset extras.
3. `current_key_context()`: map `keybindings_overlay`, `paste_viewer`, and
   `hooks_config_menu`.
4. Add the three `handle_*_navigation` helpers and have each inline block call
   its helper first, then fall through to the existing view-local `match`.
5. Tests: for each overlay, assert `q`/`Escape` close and that a navigation
   chord moves the expected state — through `handle_key_event`, so the whole
   dispatch chain is exercised.
6. Docs: `docs/keybindings.md` gains `### Keybindings Context` and
   `### Paste Viewer Context` sections, the hooks menu joins the `Select`
   description, and both new contexts join the "Active contexts" table.

## What this does not do

- It does not change any overlay's on-screen behavior; the same keys do the
  same things, they just resolve through the configurable system now.
- It does not touch the help overlay or history search, which already have
  contexts (`Help`, `HistorySearch`).
- It does not add new action strings.
