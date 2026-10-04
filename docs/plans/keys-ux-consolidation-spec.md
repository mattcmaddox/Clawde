# Keys UX Consolidation — Spec / Plan (2026-10-03)

Status: **draft for review** — not yet started.

## Problem

The free-key feature has one simple intent — set keys, forget them until a key
goes stale, remove it, and have Clawde stop using it everywhere — but that
intent is implemented across three editors and four selection surfaces:

- `/keys` popup (`crates/tui/src/keys_dialog.rs`)
- `/connect free` setup dialog (`crates/tui/src/free_mode_dialog.rs`)
- `/keys set|add|remove|list|health|doctor` (`crates/commands/src/keys.rs`)
- model/provider selection split across `/model` (`model_picker.rs`), the
  free-model popup (`free_model_popup.rs`, Alt+J/K), `/routing edit`
  (`routing_dialog.rs`), and the provider on/off list
  (`disabled_upstreams`, edited in `free_mode_dialog.rs` and
  `settings_screen.rs`)

`keys_dialog.rs` originally re-exported `free_mode_dialog::ValidationPing` and
its header stated it "mirror[s] the Connect Free dialog" — the duplication was
explicit. Both now re-export `key_editor::ValidationPing` and share one
validation channel (`App::key_validation_rx`, typed
`Receiver<key_editor::ValidationPing>`).

The cost was not cosmetic: `free_mode_dialog::apply_values()` loaded its **own**
`AuthStore` while `App` held a second, then the caller reloaded — two snapshots
racing for `auth.json`. `disabled_upstreams` had three writers (free-mode
Ctrl+D, settings comma-string, and the chain reader).

`free_mode_dialog.rs` and its `KeyContext::FreeModeDialog` have since been
deleted outright (step 9), so the module is no longer part of the tree.

## Target UX

Three surfaces, each answering exactly one question:

| Question | Surface |
|---|---|
| "Configure providers/models/on-off" | **`/models`** menu |
| "Quickly switch free model" | **Alt+J/K** popup (unchanged) |
| "What are my keys?" | **`/keys`** |
| "Which provider handles reasoning vs editing?" | **`/routing edit`** |

**`/models`** is the configurable menu: Auto is the first row; selecting a
provider pins it, selecting Auto returns to routing across all enabled
providers. Each provider row carries its on/off toggle and its model list. It
absorbs the picker role of `/model`.

**Two model surfaces, distinct roles** (resolves audit D/E):

- **Alt+J/K** — quick pick, free models only, flat list, one keystroke. Kept
  exactly as it is (`free_model_popup.rs`).
- **`/models`** — configurable. The menu does not replace the popup and must
  not contradict it (both read the same `free_model_defaults` /
  `free_model_lists`).

Naming (avoids the existing read-only `/providers` text command):

- `/models` (bare) opens the menu overlay.
- `/model <name>` stays the quick set (alias into the menu's selection).
- `/providers` stays the headless text inventory (works in `--print`/ACP);
  `/models list` may mirror it in the TUI. Mirrors the existing bare-vs-
  subcommand convention (`/keys` vs `/keys list`).

Alt+U stays as the free-upstream cycle; the menu shows the same state but does
not replace it.

**`/keys`** is the single interactive key editor (add / reveal / delete /
health dots / Cloudflare composite entry).

**`/connect free`** becomes an onboarding hand-off: it explains Free mode and,
when no key exists, points at `/keys`. It is not a second editor.

### Settings rule — no mode-scoped duplicates

- **Provider on/off → global.** "Don't use Groq" applies everywhere. One list.
- **Routing strategy + task preferences → Auto only.** Meaningless while pinned.
- **Model → whatever was last picked.** Auto has none; a pin has one.

Selecting Auto clears the pinned model; selecting a provider sets it.

## Non-goals

- No change to the headless `/keys` subcommands. They are the only key surface
  in `--print` / ACP, and `auth_store_doctor_report` backs `--check-keys`.
- No change to the `auth.json` shape (`keys` map) or to `FREE_CATALOG` order.
- No change to rotation/cooldown behaviour.

## Design

### 1. One key-editor state machine — `crates/tui/src/key_editor.rs`

```
KeyRow           keys, key_status, pending, pending_token, revealed, from_env
KeyEditorState   fields, active_idx, scroll_offset, delete_confirm,
                 is_validating, vim_search, visible, last_rect
```

An interim `KeyEditorPurpose { Manage, Onboard }` selector was introduced to
carry the Connect-Free extras (enable toggle, empty-row collapse, `show_all`)
through the port; step 9 removed it along with the module, so the Manage shape
above is the final one — no generics, no `Box<dyn Any>`. One
`render_key_editor(frame, state, vim, area, chrome)`.

### 2. One persistence path — `App::apply_key_edits`

```rust
fn apply_key_edits(
    &mut self,
    updates: &[(&'static str, Vec<String>)],
    removals: &[&'static str],
) -> usize {
    self.auth_store.reload();
    for id in removals { self.auth_store.remove_credential(id); }
    for (id, keys) in updates { self.auth_store.set_keys(id, keys.clone()); }
    self.auth_store.save();
    self.refresh_free_provider();   // rebuild + clear_last_sweep + republish poller
    updates.iter().map(|(_, k)| k.len()).sum()
}
```

The caller drains `store_updates()` (which returns the `(writes, removals)`
tuple) and starts validation itself; the status line (`Saved N key(s)`) uses the
returned count. Replaces `persist_keys_dialog` and
`free_mode_dialog::apply_values`; both now route through it.

### 3. One validation channel

Replace `validation_rx` + `keys_dialog_rx` + `free_reprobe_rx` with one
receiver. Lift the identical `validate_upstream_key` thread spawn into
`key_editor::spawn_validations`.

### 4. `/models` menu

New `models_menu.rs` (or a mode of `model_picker.rs`): Auto row + one row per
provider (enabled state, model list when expanded). Writes go through the
existing `set_model` / `persist_provider_and_model` and a single new
`set_upstream_enabled(id, bool)` that owns the on/off setting for both provider
classes (audit N). `/model <name>` remains an alias.

### 5. Single owner for `disabled_upstreams`

Writer: `set_upstream_enabled` (called from the menu). Readers unchanged
(`registry.rs`, `status.rs`, `katban/container.rs`). Delete the free-mode Ctrl+D
writer and the `settings_screen` comma-string field.

## Migration steps (each compiles and ships alone)

1. Extract `KeyRow` + state machine into `key_editor.rs`; port `keys_dialog`
   first (simpler: no NodePos/collapsed/enabled). Keep `keys_dialog::tests`
   green unchanged — they are the spec.
2. Port `free_mode_dialog` onto the shared machine; keep its Onboard extras
   behind a `KeyEditorPurpose` selector. Move the Cloudflare two-step into the
   shared `append_pending`.
3. Collapse persistence to `App::apply_key_edits`; delete `apply_values`.
   **DONE** — every key editor routed through it (only `/keys` remains after
   step 9).
4. Collapse validation channels.
   **DONE** — one `key_validation_rx`; the probe thread spawn lives in
   `key_editor::spawn_validations`, which after step 9 returns a plain
   `Receiver<ValidationPing>` with no per-purpose tag.
5. Unify renderers behind `render_key_editor`.
   **DONE** — one `render_key_editor(frame, state, vim, area, chrome)` in
   `key_editor.rs`; `render_keys_dialog` builds a `KeyEditorChrome` (dialog
   size/title/description) and delegates. Row layout, viewport, delete
   confirmation, and footer are shared. The interim `KeyEditorPurpose`
   selector is gone (step 9): the Manage presentation is the only one left.
6. Build the `/models` menu; retire `/model`'s picker role (keep the alias).
   The Alt+J/K free-model popup stays as-is (quick pick).
   **DONE (menu only)** — `models_menu.rs` + `App::open_models_menu` /
   `set_upstream_enabled` / `handle_models_menu_navigation`, bound through a
   new `KeyContext::ModelsMenu`. Bare `/models` now opens the menu;
   `/models --capability` keeps the free picker. **`/model`'s picker role was
   deliberately kept** (non-destructive choice): `/model` still opens its own
   picker, so audit D's retirement is deferred to a follow-up.
7. Reduce `/connect free` to a hand-off; remove the two extra
   `disabled_upstreams` writers.
   **DONE** — `/connect free` now runs `App::hand_off_free_mode`: it activates
   Free mode when a key exists and otherwise points at `/keys`; it never opens
   the Connect Free editor. The free-mode Ctrl+D writer
   (`KeyEditorState::toggle_enabled`) and the `settings_screen.rs`
   comma-separated field are deleted, so `App::set_upstream_enabled` is the
   sole writer of the global `disabled_upstreams` setting (katban's
   `pin_settings_json` still writes its own ephemeral container config, which
   is out of scope).
8. Update docs in the same change.
   **DONE** — `docs/commands.md` (new `/models` section, `/model` note, `/task`
   picker wording), `docs/providers.md` (`/models` routing row),
   `docs/keybindings.md` (`ModelsMenu` context; `ModelPicker` row).
9. Delete the now-unreachable `free_mode_dialog` module and collapse
   `key_editor` to its Manage half.
   **DONE** — `crates/tui/src/free_mode_dialog.rs` deleted along with
   `KeyContext::FreeModeDialog`; `key_editor.rs` dropped `KeyEditorPurpose`,
   `NodePos`, the Onboard node cursor, `show_all`/`collapsed`/`enabled`, and
   `ValidationMsg`. `key_validation_rx` is now a plain
   `Receiver<ValidationPing>`, and the `key_editor` tests moved onto
   `/keys` (`test_vim_keys_dialog_modal`,
   `test_vim_keys_dialog_hjkl_navigation`).

## Test plan

- Keep `keys_dialog::tests` green (including the Cloudflare two-step tests).
- New: `apply_key_edits` writes once and rebuilds; removing a key drops the
  cached health sweep and the footer `dead` marker (extends existing
  `refresh_free_provider_clears_the_cached_health_sweep`).
- New: menu on/off writes one `disabled_upstreams` list and the chain rebuild
  reflects it.
- Keep `scripts/audit-env-tests.py` and `cargo check -p clawde-tui --tests`
  green.

## Audit — oversights and gaps found

- **A. Cloudflare two-step.** The shared `append_pending` owns it (and
  `clear_pending` cancels a captured token), so no editor can store a bare,
  unroutable token — regression test
  `keys_dialog::tests::cloudflare_entry_is_two_step_in_manage_too`.
  `key_input_dialog` is consolidated too: the provider predicate
  (`key_editor::is_composite_key_provider`) and the composite format
  (`key_editor::compose_composite_key`) are single shared functions used by
  the editor, the App connect flow, and the single-provider dialog — no
  remaining duplicate copy of the format or the provider check.
- **B. Saved-count status.** `apply_values()` returns a count used by free-mode
  Ctrl+S for the "Saved N key(s)" message. `apply_key_edits` must preserve it.
- **C. `open()` side effects.** The Connect-Free `open()` read
  `disabled_upstreams` and derived `collapsed`/`enabled`. Resolved by step 9:
  the module is gone, so `/keys` `open()` seeds from the auth store only and
  `App::set_upstream_enabled` is the sole reader/writer of that setting.
- **D. Two model pickers remain.** Alt+J/K (free-model popup) and Alt+M
  (`/model`) both select a model. The menu must absorb both, or the "one menu"
  claim is false.
- **E. Alt+U cycle.** `free_upstream_index` drives a footer label independent of
  the popup. Decide whether the menu replaces it or it stays.
- **F. Env-var rows.** `from_env` read-only handling must survive the merge.
- **G. Placeholder guard.** Resolved — the store boundary is now shared
  (`AuthStore::FREE_KEY_MIN_LEN` / `is_usable_free_key`, used by
  `clean_free_keys`) and `render_key_editor` shows an inline warning via
  `KeyEditorState::pending_key_too_short` while the typed key is under it, so
  the drop is surfaced instead of a phantom dot. Regression test
  `keys_dialog::tests::short_pending_key_is_flagged_before_it_is_dropped`.
- **H. Two probe semantics.** `validate_upstream_key` (dialog keys) vs
  `probe_sync_for` (health-poller path). Unifying the channel must not silently
  collapse them into one meaning.
- **I. External readers of `disabled_upstreams`.** `katban/container.rs`,
  `status.rs`, and `registry.rs` read the settings shape; keep it byte-stable.
- **J. Onboarding `ProviderSetup` page.** It lists env-var setup and
  `clawde auth login`; the `/connect free` hand-off must not orphan it.
- **K. `set_model` inference.** It infers the provider from the model string;
  menu selections must route through it so `config.provider`/`config.model`
  stay consistent.
- **L. Repaint cadence.** A static menu must not be added to
  `App::needs_fast_repaint()`; only add it if it animates. Check with
  `scripts/probes/idle-cpu-probe.py`.
- **M. Docs.** `docs/providers.md`, `docs/commands.md`, and
  `docs/canonical-free-key-storage.md` describe the current surfaces and must be
  updated in the same change (AGENTS.md honesty rule).
- **N. On/off means two different settings.** A free-catalog upstream is
  disabled via `providers.free.options.routing.disabled_upstreams`; a non-free
  provider (Anthropic, OpenAI, …) is disabled via `providers.<id>.enabled`
  (`ProviderConfig::enabled`). The menu's toggle must map to the correct one by
  provider class, or one class silently ignores the toggle.
- **O. `/connect <provider>` key flow.** `key_input_dialog` + the OAuth branch
  in `app.rs` (~7120-7340) is how a *single* provider gets a key today. If the
  menu only selects, this flow must stay reachable, or the menu needs its own
  "add key" affordance.
- **P. `free/auto` vs a bare provider.** `set_model("free/auto")` leaves
  `config.provider = "free"`; a pinned provider sets `config.provider = <id>`.
  The menu must preserve that distinction so routing vs direct dispatch is
  unchanged.

## Readiness

**Steps 1–5 (the internal consolidation) are ready to begin now.** They are
mechanical, guarded by the existing dialog tests, carry no open UX decisions,
and deliver the real fix (one persistence path, one validation channel, one
editor). Recommended first move: step 1 — extract `key_editor.rs`, port
`/keys` first, keep `keys_dialog::tests` green.

**Steps 6–8 (the menu) are unblocked.** The menu absorbs `/model`'s picker role
and is named `/models`; the Alt+J/K free-model popup stays as the quick pick
(not absorbed), and Alt+U stays. Audit D/E are resolved. Everything else in the
audit maps to a concrete action.

No rabbit holes: the audit surfaced the expected integration points (Cloudflare
entry, two on/off backings, two probe semantics, key-input flow) but nothing
that changes the shape. The plan is safe to start at step 1 without resolving
the menu question.
