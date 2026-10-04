// key_editor.rs — state machine for editing a provider's key pool, plus the
// `/keys` renderer.
//
// One row per free-catalog upstream, each holding that upstream's stored
// rotation keys (rendered as health dots), a pending new-key buffer, and an
// optional revealed selection cursor. This module owns the *mechanics* —
// navigation, reveal, append, delete-confirm, validation — and the `/keys`
// renderer; the App owns persistence.
//
// The Cloudflare composite two-step (token, then account ID) is handled here,
// so `/keys` can never store an unroutable bare token.
//
// Stored keys are NEVER shown unless a row's `revealed` is `Some` (the value is
// the selection cursor). Expanding is view-only: typing/pasting is blocked
// until the row is collapsed again, so an accidental keystroke can never
// corrupt a stored key. State is deliberately decoupled from `AuthStore` so the
// caller owns persistence.

use clawde_api::{FreeUpstream, FREE_CATALOG};
use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::cell::Cell;

use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};
use crate::vim_search::VimSearch;

/// Shared muted/foreground colors used by the renderer.
const DIM: Color = Color::Rgb(90, 90, 90);
const MUTED: Color = Color::Rgb(180, 180, 180);
const TIP: Color = Color::Rgb(120, 210, 150);

/// One background key-validation ping result: `(field_idx, key_idx, result)`.
/// Named so the receiver type stays readable in the callers (keeps
/// clippy::type_complexity off).
pub type ValidationPing = (usize, usize, Result<(), String>);

/// Spawn one background thread per `(field_idx, key_idx, upstream, key)`
/// target, each probing `validate_upstream_key` and forwarding the result.
fn spawn_validations(
    targets: Vec<(usize, usize, String, String)>,
) -> std::sync::mpsc::Receiver<ValidationPing> {
    let (tx, rx) = std::sync::mpsc::channel();
    for (fi, ki, upstream_id, key) in targets {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
            // Best-effort send; silently fails if the dialog was closed.
            let _ = tx.send((fi, ki, result));
        });
    }
    drop(tx);
    rx
}

/// Provider ids whose stored key is the composite `ACCOUNT_ID:API_TOKEN`
/// rather than a single opaque token. The single predicate shared by the key
/// editor, the single-provider key input dialog, and the App's connect flow.
pub fn is_composite_key_provider(provider_id: &str) -> bool {
    provider_id == "cloudflare"
}

/// Join an account ID and API token into the stored composite key. The single
/// implementation of the composite format, shared by the key editor and the
/// single-provider key input dialog so the two can never drift.
pub fn compose_composite_key(account_id: &str, api_token: &str) -> String {
    format!("{}:{}", account_id.trim(), api_token.trim())
}

/// One row in the editor — one upstream's key list plus the new-key buffer.
#[derive(Debug, Clone)]
pub struct KeyRow {
    pub upstream: &'static FreeUpstream,
    /// Stored rotation keys (one per ring slot, rendered as dots).
    pub keys: Vec<String>,
    /// Parallel to `keys`: per-key validation status (`None` = not tested,
    /// `Some(Ok(()))` = valid, `Some(Err(_))` = invalid).
    pub key_status: Vec<Option<Result<(), String>>>,
    /// New-key input buffer — the blank line that accepts new keys.
    pub pending: String,
    /// Two-step composite-key flow (cloudflare): when `Some`, the API token
    /// was captured on the first Enter and the row now awaits the account ID.
    /// The second Enter joins them into the stored `ACCOUNT_ID:API_TOKEN`.
    pub pending_token: Option<String>,
    /// `Some(i)` = the row is expanded: every stored key is shown inline and
    /// key `i` is the highlighted selection cursor. `None` = masked (health
    /// dots only). The row is view-only while expanded.
    pub revealed: Option<usize>,
    /// When `true`, the keys came from environment variables and are
    /// read-only in this editor (cannot be edited, appended to, or deleted).
    pub from_env: bool,
}

/// State for the "delete this key?" confirmation popup.
#[derive(Debug, Clone, Copy)]
pub struct DeleteConfirm {
    pub field_idx: usize,
    pub key_idx: usize,
}

/// Number of rows shown at once in the scrolling viewport.
pub const VISIBLE_ROWS: usize = 10;

/// Key-editor state machine. The `/keys` popup wraps this as
/// `KeysDialogState`.
pub struct KeyEditorState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub fields: Vec<KeyRow>,
    /// Active provider row.
    pub active_idx: usize,
    /// First visible field index (for scrolling when fields > viewport).
    pub scroll_offset: usize,
    /// When set, the delete-confirmation popup is open and captures input.
    pub delete_confirm: Option<DeleteConfirm>,
    /// Vim-modal insert state (only used when vim is enabled). The dialog is
    /// a key-entry form, so it opens in insert; `Esc` exits insert before
    /// the unreveal → clear → close cascade runs.
    pub vim_search: VimSearch,
    /// `true` while a background validation sweep is in flight (its results
    /// land on the rows' `key_status` dots via `set_validation_result`).
    pub is_validating: bool,
}

impl Default for KeyEditorState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyEditorState {
    /// A fresh editor with one masked row per free-catalog upstream.
    pub fn new() -> Self {
        let fields = FREE_CATALOG
            .iter()
            .map(|upstream| KeyRow {
                upstream,
                keys: Vec::new(),
                key_status: Vec::new(),
                pending: String::new(),
                pending_token: None,
                revealed: None,
                from_env: false,
            })
            .collect();
        Self {
            visible: false,
            fields,
            active_idx: 0,
            scroll_offset: 0,
            delete_confirm: None,
            last_rect: Cell::new(Rect::default()),
            vim_search: VimSearch::new(),
            is_validating: false,
        }
    }

    /// Open the dialog, pre-populating each row from `existing[upstream.id]`
    /// when present. Each string is one stored key (rendered as a dot).
    pub fn open(&mut self, existing: &[(&str, Vec<String>)]) {
        self.visible = true;
        self.delete_confirm = None;
        self.is_validating = false;
        self.vim_search.enter_insert();
        // Reset every field; the dialog is re-seeded from the store each
        // time it opens so discarded edits never leak back in. `from_env` is
        // rederived by the caller via `set_env_var_keys` after `open`.
        for field in &mut self.fields {
            field.keys.clear();
            field.key_status.clear();
            field.pending.clear();
            field.pending_token = None;
            field.revealed = None;
            field.from_env = false;
        }
        for (id, keys) in existing {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                // Don't overwrite env-var keys with auth_store keys (env var wins).
                if !field.from_env {
                    field.keys = keys
                        .iter()
                        .filter(|k| !k.trim().is_empty())
                        .cloned()
                        .collect();
                    field.key_status = vec![None; field.keys.len()];
                }
            }
        }
        let visible = self.visible_field_indices();
        self.active_idx = visible.first().copied().unwrap_or(0);
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    /// Mark upstreams whose keys came from environment variables. These are
    /// shown as read-only in the dialog.
    pub fn set_env_var_keys(&mut self, env_var_keys: &[(&str, String)]) {
        for (id, _key) in env_var_keys {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                field.from_env = true;
            }
        }
    }

    /// Close the dialog, discarding transient + seeded state (next `open()`
    /// re-seeds from the auth store).
    pub fn close(&mut self) {
        self.visible = false;
        self.active_idx = 0;
        self.scroll_offset = 0;
        self.delete_confirm = None;
        self.vim_search.reset();
        self.is_validating = false;
        for field in &mut self.fields {
            field.keys.clear();
            field.key_status.clear();
            field.pending.clear();
            field.pending_token = None;
            field.revealed = None;
        }
    }

    /// Return indices of fields that are currently navigable — every provider
    /// row in catalog order, so the user can also pick an unconfigured
    /// upstream and add its first key there.
    pub fn visible_field_indices(&self) -> Vec<usize> {
        (0..self.fields.len()).collect()
    }

    fn ensure_active_visible(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let rows = VISIBLE_ROWS;
        let pos = visible
            .iter()
            .position(|i| *i == self.active_idx)
            .unwrap_or(0);
        if pos < self.scroll_offset {
            self.scroll_offset = pos;
        } else if pos >= self.scroll_offset + rows {
            self.scroll_offset = pos + 1 - rows;
        }
    }

    /// Move to the next provider row, discarding transient state.
    pub fn move_next(&mut self) {
        self.unreveal_and_clear();
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        self.active_idx = match pos {
            Some(p) if p + 1 < visible.len() => visible[p + 1],
            _ => visible[0],
        };
        self.ensure_active_visible();
    }

    /// Move to the previous provider row, discarding transient state.
    pub fn move_prev(&mut self) {
        self.unreveal_and_clear();
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        self.active_idx = match pos {
            Some(p) if p > 0 => visible[p - 1],
            _ => *visible.last().unwrap(),
        };
        self.ensure_active_visible();
    }

    /// Discard revealed key + typed text (+ captured composite token) on the
    /// active row before nav.
    fn unreveal_and_clear(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.revealed = None;
            field.pending.clear();
            field.pending_token = None;
        }
    }

    /// Whether the active row's new-key buffer is empty. Gates vim-style
    /// j/k navigation — once the user starts typing a key, those letters
    /// belong to the key, not the cursor.
    pub fn pending_is_empty(&self) -> bool {
        self.fields
            .get(self.active_idx)
            .map(|f| f.pending.is_empty())
            .unwrap_or(true)
    }

    /// Enter on the active row: append a typed new key (create), else toggle
    /// the row's key list. Expanding shows every stored key inline with the
    /// selection cursor on the first one; a second Enter collapses the row.
    ///
    /// Returns `true` when a new key was appended — the caller then persists
    /// and fires a validity check.
    pub fn enter_active(&mut self) -> bool {
        if self.append_pending() {
            return true;
        }
        let Some(field) = self.fields.get_mut(self.active_idx) else {
            return false;
        };
        if field.revealed.is_some() {
            field.revealed = None;
        } else if !field.keys.is_empty() {
            field.revealed = Some(0);
        }
        false
    }

    /// Whether the active row is currently expanded (its key list is shown).
    pub fn active_is_revealed(&self) -> bool {
        self.fields
            .get(self.active_idx)
            .map(|f| f.revealed.is_some())
            .unwrap_or(false)
    }

    /// Number of stored keys on the active row (0 for an unknown row).
    pub fn active_key_count(&self) -> usize {
        self.fields
            .get(self.active_idx)
            .map(|f| f.keys.len())
            .unwrap_or(0)
    }

    /// Move the selection cursor to the next key on the expanded active row.
    /// No-op unless the row is expanded; wraps at the end.
    pub fn select_next_key(&mut self) {
        self.select_key_by(1);
    }

    /// Move the selection cursor to the previous key on the expanded active
    /// row. No-op unless the row is expanded; wraps at the start.
    pub fn select_prev_key(&mut self) {
        self.select_key_by(-1);
    }

    fn select_key_by(&mut self, delta: isize) {
        let Some(field) = self.fields.get_mut(self.active_idx) else {
            return;
        };
        let Some(i) = field.revealed else {
            return;
        };
        let len = field.keys.len();
        if len == 0 {
            field.revealed = None;
            return;
        }
        let next = ((i as isize + delta).rem_euclid(len as isize)) as usize;
        field.revealed = Some(next);
    }

    /// Commit the typed new-key buffer as an additional stored key (a new
    /// dot). Returns `true` when a key was appended.
    ///
    /// Handles the Cloudflare two-step composite flow, so `/keys` can never
    /// store an unroutable bare token: the first Enter captures the API token
    /// into `pending_token` and switches the row to an account-ID prompt; the
    /// second Enter joins them into the stored `ACCOUNT_ID:API_TOKEN`.
    pub fn append_pending(&mut self) -> bool {
        let Some(field) = self.fields.get_mut(self.active_idx) else {
            return false;
        };
        if field.from_env {
            return false;
        }
        let key = field.pending.trim().to_string();
        if key.is_empty() {
            return false;
        }
        // Two-step composite flow: first Enter captures the API token.
        if is_composite_key_provider(field.upstream.id) && field.pending_token.is_none() {
            field.pending_token = Some(key);
            field.pending.clear();
            return true;
        }
        // Second Enter: join the typed account ID with the captured token.
        if let Some(token) = field.pending_token.take() {
            let composite = compose_composite_key(&key, &token);
            field.keys.push(composite);
            field.key_status.push(None);
            field.pending.clear();
            return true;
        }
        field.keys.push(key);
        field.key_status.push(None);
        field.pending.clear();
        field.revealed = None;
        self.ensure_active_visible();
        true
    }

    /// Committed key count across every row (for the title / status line).
    pub fn filled_count(&self) -> usize {
        self.fields
            .iter()
            .map(|f| f.keys.iter().filter(|k| !k.trim().is_empty()).count())
            .sum()
    }

    /// Whether the active row's typed new key would be rejected by the free
    /// key-store normalizer (shorter than `AuthStore::FREE_KEY_MIN_LEN`).
    /// Rendered as an inline warning so a typed key is never silently dropped
    /// at save time.
    pub fn pending_key_too_short(&self) -> bool {
        self.fields.get(self.active_idx).is_some_and(|f| {
            let typed = f.pending.trim();
            !typed.is_empty() && !clawde_core::AuthStore::is_usable_free_key(typed)
        })
    }

    /// Discard the active row's typed new-key text. Returns `true` if there
    /// was anything to clear (Esc cascade: reveal → clear → close).
    ///
    /// For the Cloudflare two-step flow, an Esc with an empty ID buffer cancels
    /// the captured token (restoring it to the new-key line so it can be
    /// re-entered), so a second Esc proceeds to close.
    pub fn clear_pending(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if !field.pending.is_empty() {
                field.pending.clear();
                return true;
            }
            if let Some(token) = field.pending_token.take() {
                field.pending = token;
                return true;
            }
        }
        false
    }

    /// Re-mask the revealed key of the active row. Returns `true` if a key
    /// was revealed (and is now hidden again).
    pub fn unreveal_active(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.revealed.is_some() {
                field.revealed = None;
                return true;
            }
        }
        false
    }

    /// Insert a character into the active row's new-key buffer. Ignored while
    /// the row is expanded (view-only) so a keystroke cannot corrupt a key.
    pub fn insert_char(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.from_env || field.revealed.is_some() {
                return;
            }
            field.pending.push(c);
        }
    }

    /// Paste clipboard text (Ctrl+V) into the active row's new-key buffer.
    /// Newlines and surrounding whitespace are trimmed so a pasted key that
    /// carries a trailing line feed lands as a single token.
    pub fn paste_key(&mut self, text: &str) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.from_env || field.revealed.is_some() {
                return;
            }
            let cleaned = text.trim();
            if cleaned.is_empty() {
                return;
            }
            field.pending.push_str(cleaned);
        }
    }

    /// Backspace the active row's new-key buffer. Ignored while the row is
    /// expanded (view-only) or env-provided.
    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.revealed.is_none() && !field.from_env {
                field.pending.pop();
            }
        }
    }

    /// Offer the delete-confirmation popup for the active row's selected key.
    /// Returns `true` when the popup opened.
    pub fn try_open_delete_confirm(&mut self) -> bool {
        let Some(field) = self.fields.get(self.active_idx) else {
            return false;
        };
        if field.from_env {
            return false;
        }
        if let Some(i) = field.revealed {
            if i < field.keys.len() {
                self.delete_confirm = Some(DeleteConfirm {
                    field_idx: self.active_idx,
                    key_idx: i,
                });
                return true;
            }
        }
        false
    }

    /// Confirm the pending delete: remove the selected key (and its dot)
    /// locally and keep the row expanded with the cursor clamped onto a
    /// surviving key. Returns `true` when a key was actually removed — the
    /// caller then persists the edited map to the auth store.
    pub fn confirm_delete(&mut self) -> bool {
        let Some(dc) = self.delete_confirm.take() else {
            return false;
        };
        let Some(field) = self.fields.get_mut(dc.field_idx) else {
            return false;
        };
        if dc.key_idx >= field.keys.len() {
            return false;
        }
        field.keys.remove(dc.key_idx);
        field.key_status.remove(dc.key_idx);
        if field.keys.is_empty() {
            field.revealed = None;
        } else {
            // Keep the row expanded; land the cursor on the next surviving
            // key (or the last one when the removed key was the tail).
            field.revealed = Some(dc.key_idx.min(field.keys.len() - 1));
        }
        true
    }

    /// Cancel the delete popup (key is kept).
    pub fn cancel_delete(&mut self) {
        self.delete_confirm = None;
    }

    /// Collect the edited key map, keyed by upstream id. Every non-env row is
    /// included in `writes`, even empty ones — an empty list clears that
    /// upstream's stored keys. The `/keys` popup writes canonical pools only,
    /// so there are never credential removals.
    pub fn store_updates(&self) -> (Vec<(&'static str, Vec<String>)>, Vec<&'static str>) {
        let writes = self
            .fields
            .iter()
            .filter(|f| !f.from_env)
            .map(|f| (f.upstream.id, f.keys.clone()))
            .collect();
        (writes, Vec::new())
    }

    /// Fire a background validation sweep over every stored key on every row.
    /// Each key is probed by `validate_upstream_key` and the result lands on
    /// its dot via `set_validation_result`. Returns a `Receiver` the main loop
    /// drains with `poll_key_validation`, or `None` when there is nothing to
    /// probe or a sweep is already running.
    pub fn start_validate(&mut self) -> Option<std::sync::mpsc::Receiver<ValidationPing>> {
        if self.is_validating {
            return None;
        }
        let targets: Vec<(usize, usize, String, String)> = self
            .fields
            .iter()
            .enumerate()
            .flat_map(|(fi, f)| {
                if f.from_env {
                    return Vec::new();
                }
                f.keys
                    .iter()
                    .enumerate()
                    .filter(|(_, k)| !k.trim().is_empty())
                    .map(|(ki, k)| (fi, ki, f.upstream.id.to_string(), k.trim().to_string()))
                    .collect()
            })
            .collect();
        if targets.is_empty() {
            return None;
        }

        self.is_validating = true;
        Some(spawn_validations(targets))
    }

    /// Record the outcome of one probed key. Called from the main loop as
    /// validation results arrive.
    pub fn set_validation_result(
        &mut self,
        field_idx: usize,
        key_idx: usize,
        result: Result<(), String>,
    ) {
        self.is_validating = false;
        if let Some(field) = self.fields.get_mut(field_idx) {
            if let Some(slot) = field.key_status.get_mut(key_idx) {
                *slot = Some(result);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Caller-supplied presentation for [`render_key_editor`]. The state machine
/// and row layout are shared; this carries the dialog size, title, and the
/// caller's description block.
pub struct KeyEditorChrome {
    /// Dialog width, clamped to the terminal.
    pub width: u16,
    /// Dialog height, clamped to the terminal.
    pub height: u16,
    /// Full title text (the caller already appends the key count).
    pub title: String,
    /// Lines rendered immediately under the title. The caller decides the
    /// spacing (the block ends with a blank line for the row list).
    pub description: Vec<Line<'static>>,
}

/// Health color for a key dot: green = valid, red = invalid, dim = untested.
fn manage_dot_color(status: &Option<Result<(), String>>) -> Color {
    match status {
        Some(Ok(())) => Color::Rgb(120, 210, 150),
        Some(Err(_)) => Color::Rgb(230, 110, 110),
        None => MUTED,
    }
}

/// Append the `/keys` presentation of a single row to `lines`.
fn push_manage_row(lines: &mut Vec<Line<'static>>, state: &KeyEditorState, fi: usize) {
    let Some(field) = state.fields.get(fi) else {
        return;
    };
    let is_active = fi == state.active_idx;
    let fg = if is_active { CLAWDE_ACCENT } else { MUTED };

    let key_count = field.keys.len();
    let key_label = if key_count == 1 { "key" } else { "keys" };
    let env_marker = if field.from_env {
        " \u{00b7} env-read-only"
    } else {
        ""
    };
    let name_style = if is_active {
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(fg)
    };
    lines.push(Line::from(vec![
        Span::styled(
            format!(
                "  {}{}",
                field.upstream.title,
                if is_active { " \u{25b8}" } else { "" }
            ),
            name_style,
        ),
        Span::styled(
            format!("   ({} {}{})", key_count, key_label, env_marker),
            Style::default().fg(DIM),
        ),
    ]));

    // Key line: health dots when masked, one key per line (with the selection
    // cursor highlighted) when the row is expanded.
    if field.keys.is_empty() {
        lines.push(Line::styled("      (no keys)", Style::default().fg(DIM)));
    } else if let Some(sel) = field.revealed {
        for (ki, key) in field.keys.iter().enumerate() {
            let all: Vec<char> = key.chars().collect();
            let body: String = if all.len() > 64 {
                format!("{}\u{2026}", all.iter().take(64).collect::<String>())
            } else {
                key.clone()
            };
            let selected = ki == sel;
            let marker = if selected {
                Span::styled(
                    "\u{25b8} ",
                    Style::default()
                        .fg(CLAWDE_ACCENT)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("  ", Style::default().fg(DIM))
            };
            let style = if selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(CLAWDE_ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                marker,
                Span::styled(format!("[{}]", body), style),
            ]));
        }
        lines.push(Line::styled(
            format!(
                "      key {}/{} \u{00b7} \u{2190}/\u{2192} select \u{00b7} del delete \u{00b7} esc hide",
                sel + 1,
                field.keys.len()
            ),
            Style::default().fg(DIM),
        ));
    } else {
        let mut spans: Vec<Span<'static>> = vec![Span::styled("      ", Style::default())];
        for status in field.key_status.iter() {
            spans.push(Span::styled(
                "\u{25cf} ",
                Style::default().fg(manage_dot_color(status)),
            ));
        }
        lines.push(Line::from(spans));
    }

    // Pending new-key line (only meaningful on the active row).
    let pending_style = Style::default().fg(if is_active { TIP } else { DIM });
    let pending_label = if field.from_env {
        "      env-provided key \u{2014} edit in your shell profile".to_string()
    } else if field.pending.is_empty() {
        if field.pending_token.is_some() {
            "      Paste your Cloudflare ID now\u{2026}".to_string()
        } else if is_active {
            "      new key\u{2026}".to_string()
        } else {
            String::new()
        }
    } else {
        format!(
            "      {}{}",
            field.pending,
            if is_active { "\u{258d}" } else { "" }
        )
    };
    if !pending_label.is_empty() {
        lines.push(Line::styled(pending_label, pending_style));
    }
}

/// Render the key editor. The caller supplies the dialog size, title, and
/// description via `chrome`; the rows, delete prompt, and footer are shared.
pub fn render_key_editor(
    frame: &mut Frame,
    state: &KeyEditorState,
    vim_enabled: bool,
    area: Rect,
    chrome: &KeyEditorChrome,
) {
    if !state.visible {
        return;
    }
    let pink = CLAWDE_ACCENT;
    let dim = DIM;
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = chrome.width.min(area.width.saturating_sub(4));
    let height = chrome.height.min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row.
    lines.push(Line::from(vec![Span::styled(
        format!(" {}", chrome.title),
        Style::default().fg(pink).add_modifier(Modifier::BOLD),
    )]));

    // Caller-supplied description block.
    lines.extend(chrome.description.iter().cloned());

    let visible = state.visible_field_indices();
    let start = state.scroll_offset;
    let end = (start + VISIBLE_ROWS).min(visible.len());
    for &idx in visible.iter().skip(start).take(end - start) {
        push_manage_row(&mut lines, state, idx);
    }

    // A typed key shorter than the store's minimum would be dropped at save
    // time — say so instead of showing a phantom dot.
    if state.pending_key_too_short() {
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  \u{26a0} key looks too short (min {} chars) \u{2014} it will not be saved",
                clawde_core::AuthStore::FREE_KEY_MIN_LEN
            ),
            Style::default().fg(Color::Rgb(230, 160, 60)),
        )]));
    }

    // Delete-confirm prompt, rendered inline in the row list.
    if let Some(dc) = state.delete_confirm {
        if let Some(field) = state.fields.get(dc.field_idx) {
            let key_preview: String = field
                .keys
                .get(dc.key_idx)
                .map(|k| format!("{}\u{2026}", k.chars().take(12).collect::<String>()))
                .unwrap_or_else(|| "?".to_string());
            lines.push(Line::styled(
                format!(
                    "  \u{26a0} Delete key {} from {}?  [y]es / [n]o  \u{2014} {}",
                    dc.key_idx + 1,
                    field.upstream.id,
                    key_preview,
                ),
                Style::default().fg(Color::Rgb(230, 160, 60)),
            ));
        }
    }

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);

    // Static keybind footer pinned to the bottom of the panel.
    let footer = if vim_enabled {
        "  j/k move \u{00b7} h/l select key \u{00b7} enter reveal/add \u{2192} auto-save+validate \u{00b7} del confirm delete \u{00b7} esc close"
    } else {
        "  \u{2191}/\u{2193} move \u{00b7} \u{2190}/\u{2192} select key \u{00b7} enter reveal/add \u{2192} auto-save+validate \u{00b7} del confirm delete \u{00b7} esc close"
    };
    let footer_widget =
        Paragraph::new(Line::styled(footer, Style::default().fg(dim))).bg(dialog_bg);
    frame.render_widget(
        footer_widget,
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(2),
            width: inner.width,
            height: 1,
        },
    );
}
