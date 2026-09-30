// `/keys` popup — a j/k-navigable key manager with CRUD.
//
// The popup shows one row per free-tier upstream (in FREE_CATALOG order,
// matching fallback priority). Each row lists that upstream's stored
// rotation keys masked as health dots; Enter reveals a key inline, typing
// into the pending line appends a new key, and Delete on a revealed key
// asks for confirmation before removing it. `store_updates()` returns the
// edited key map; the App persists it to the AuthStore via `apply_values`.
//
// Stored keys are NEVER shown by default — a key is only visible while its
// index is `revealed` (cleared on nav, Esc, and every mutation). The state
// is deliberately decoupled from `AuthStore` so the App owns persistence,
// mirroring `FreeModeDialogState` in `free_mode_dialog.rs`.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use clawde_api::{FreeUpstream, FREE_CATALOG};

use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};
use crate::vim_search::VimSearch;
use std::cell::Cell;

pub use crate::free_mode_dialog::ValidationPing;

/// One row in the dialog — one upstream's key list plus the new-key buffer.
#[derive(Debug, Clone)]
pub struct KeysField {
    pub upstream: &'static FreeUpstream,
    /// Stored rotation keys (one per ring slot, rendered as dots).
    pub keys: Vec<String>,
    /// Parallel to `keys`: per-key validation status (`None` = not tested,
    /// `Some(Ok(()))` = valid, `Some(Err(_))` = invalid).
    pub key_status: Vec<Option<Result<(), String>>>,
    /// New-key input buffer — the blank line that accepts new keys.
    pub pending: String,
    /// Index of the key currently revealed inline (view-only). `None` = masked.
    pub revealed: Option<usize>,
    /// When `true`, the keys came from environment variables and are
    /// read-only in this dialog (cannot be edited, appended to, or deleted).
    pub from_env: bool,
}

/// State for the "delete this key?" confirmation popup.
#[derive(Debug, Clone, Copy)]
pub struct KeysDeleteConfirm {
    pub field_idx: usize,
    pub key_idx: usize,
}

/// Number of rows shown at once in the scrolling viewport.
pub const VISIBLE_ROWS: usize = 10;

pub struct KeysDialogState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub fields: Vec<KeysField>,
    /// Active provider row.
    pub active_idx: usize,
    /// First visible field index (for scrolling when fields > viewport).
    pub scroll_offset: usize,
    /// When set, the delete-confirmation popup is open and captures input.
    pub delete_confirm: Option<KeysDeleteConfirm>,
    /// Vim-modal insert state (only used when vim is enabled). The dialog is
    /// a key-entry form, so it opens in insert; `Esc` exits insert before
    /// the unreveal → clear → close cascade runs.
    pub vim_search: VimSearch,
    /// `true` while a background validation sweep is in flight (its results
    /// land on the rows' `key_status` dots via `set_validation_result`).
    pub is_validating: bool,
}

impl Default for KeysDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeysDialogState {
    pub fn new() -> Self {
        let fields = FREE_CATALOG
            .iter()
            .map(|upstream| KeysField {
                upstream,
                keys: Vec::new(),
                key_status: Vec::new(),
                pending: String::new(),
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
        self.active_idx = self.visible_field_indices().first().copied().unwrap_or(0);
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
            field.revealed = None;
        }
    }

    /// Return indices of fields that currently hold at least one key. The
    /// dialog only navigates rows with keys so empty upstreams stay out of
    /// the way unless the user explicitly starts adding to them.
    /// Indices of every provider row in catalog order, so the user can also
    /// pick an unconfigured upstream and add its first key there.
    pub fn visible_field_indices(&self) -> Vec<usize> {
        (0..self.fields.len()).collect()
    }

    fn ensure_active_visible(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible
            .iter()
            .position(|i| *i == self.active_idx)
            .unwrap_or(0);
        if pos < self.scroll_offset {
            self.scroll_offset = pos;
        } else if pos >= self.scroll_offset + VISIBLE_ROWS {
            self.scroll_offset = pos + 1 - VISIBLE_ROWS;
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

    /// Discard revealed key + typed text on the active row before nav.
    fn unreveal_and_clear(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.revealed = None;
            field.pending.clear();
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

    /// Enter on the active row: appends a typed new key (create) or toggles
    /// the first masked key (read). Returns `true` when a new key was
    /// appended — the caller then persists and fires a validity check.
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

    /// Commit the typed new-key buffer as an additional stored key (a new
    /// dot). Returns `true` when a key was appended.
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
        field.keys.push(key);
        field.key_status.push(None);
        field.pending.clear();
        field.revealed = None;
        self.ensure_active_visible();
        true
    }

    /// Committed key count across every row (for the title / status line).
    pub fn filled_count(&self) -> usize {
        self.fields.iter().map(|f| f.keys.len()).sum()
    }

    /// Discard the active row's typed new-key text. Returns `true` if there
    /// was anything to clear (Esc cascade: reveal → clear → close).
    pub fn clear_pending(&mut self) -> bool {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if !field.pending.is_empty() {
                field.pending.clear();
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

    /// Insert a character into the active row's new-key buffer.
    pub fn insert_char(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if !field.from_env {
                field.pending.push(c);
            }
        }
    }

    /// Paste clipboard text (Ctrl+V) into the active row's new-key buffer.
    /// Newlines and surrounding whitespace are trimmed so a pasted key that
    /// carries a trailing line feed lands as a single token.
    pub fn paste_key(&mut self, text: &str) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            if field.from_env {
                return;
            }
            let cleaned = text.trim();
            if cleaned.is_empty() {
                return;
            }
            field.pending.push_str(cleaned);
        }
    }

    /// Backspace the active row's new-key buffer.
    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.pending.pop();
        }
    }

    /// Offer the delete-confirmation popup when the active row's key is
    /// revealed. Returns `true` when the popup opened.
    pub fn try_open_delete_confirm(&mut self) -> bool {
        let Some(field) = self.fields.get(self.active_idx) else {
            return false;
        };
        if field.from_env {
            return false;
        }
        if let Some(i) = field.revealed {
            if i < field.keys.len() {
                self.delete_confirm = Some(KeysDeleteConfirm {
                    field_idx: self.active_idx,
                    key_idx: i,
                });
                return true;
            }
        }
        false
    }

    /// Confirm the pending delete: remove the key (and its dot) locally.
    /// Changes are applied to the auth store on commit (Ctrl+Enter / Ctrl+S).
    pub fn confirm_delete(&mut self) {
        let Some(dc) = self.delete_confirm.take() else {
            return;
        };
        if let Some(field) = self.fields.get_mut(dc.field_idx) {
            if dc.key_idx < field.keys.len() {
                field.keys.remove(dc.key_idx);
                field.key_status.remove(dc.key_idx);
                field.revealed = None;
            }
        }
    }

    /// Cancel the delete popup (key is kept).
    pub fn cancel_delete(&mut self) {
        self.delete_confirm = None;
    }

    /// Collect the edited key map, keyed by upstream id — the caller applies
    /// it to the AuthStore. Every non-env row is included, even empty ones:
    /// an empty list clears that upstream's stored keys (deleting the last
    /// key must reach the store, and an upstream the user never touched must
    /// also keep its stored zero).
    pub fn store_updates(&self) -> Vec<(&'static str, Vec<String>)> {
        self.fields
            .iter()
            .filter(|f| !f.from_env)
            .map(|f| (f.upstream.id, f.keys.clone()))
            .collect()
    }

    /// Fire a background validation sweep over every stored key in the
    /// dialog. Each key gets probed by `validate_upstream_key` and the
    /// result lands on its dot via `set_validation_result`. Returns a
    /// `Receiver` the main loop drains with `poll_keys_dialog_validation`,
    /// or `None` when there is nothing to probe or a sweep is already
    /// running. Mirrors `FreeModeDialogState::start_validate`.
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

        let (tx, rx) = std::sync::mpsc::channel();
        self.is_validating = true;
        for (fi, ki, upstream_id, key) in targets {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
                // Best-effort send; silently fails if the dialog was closed.
                let _ = tx.send((fi, ki, result));
            });
        }
        drop(tx);
        Some(rx)
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

/// Health-dot color for a stored key's status.
fn dot_color(status: &Option<Result<(), String>>) -> Color {
    match status {
        Some(Ok(())) => Color::Rgb(120, 210, 150),
        Some(Err(_)) => Color::Rgb(230, 110, 110),
        None => Color::Rgb(140, 140, 140),
    }
}

/// Render the `/keys` popup: j/k-navigable row list with masked key dots,
/// reveal-on-Enter, a pending new-key line, and the delete-confirm popup.
pub fn render_keys_dialog(
    frame: &mut Frame,
    state: &KeysDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let tip = Color::Rgb(120, 210, 150);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 88u16.min(area.width.saturating_sub(4));
    let height = 30u16.min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let total_keys = state.filled_count();
    let title_text = format!(
        "/keys — {} key{}",
        total_keys,
        if total_keys == 1 { "" } else { "s" }
    );

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row.
    lines.push(Line::from(vec![Span::styled(
        format!(" {}", title_text),
        Style::default().fg(pink).add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::styled(
        "  j/k nav · enter reveal/add+saves · ctrl+v paste · del delete · esc close",
        Style::default().fg(dim),
    ));
    lines.push(Line::styled("", Style::default()));

    // Rows.
    let visible = state.visible_field_indices();
    for (row, fi) in visible.iter().enumerate().skip(state.scroll_offset) {
        if row >= state.scroll_offset + VISIBLE_ROWS {
            break;
        }
        let Some(field) = state.fields.get(*fi) else {
            continue;
        };
        let is_active = *fi == state.active_idx;
        let fg = if is_active { pink } else { muted };

        // Provider name + key count.
        let key_count = field.keys.len();
        let key_label = if key_count == 1 { "key" } else { "keys" };
        let env_marker = if field.from_env {
            " · env-read-only"
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
                    if is_active { " ▸" } else { "" }
                ),
                name_style,
            ),
            Span::styled(
                format!("   ({} {}{})", key_count, key_label, env_marker),
                Style::default().fg(dim),
            ),
        ]));

        // Key dots / revealed key line.
        if field.keys.is_empty() {
            lines.push(Line::styled("      (no keys)", Style::default().fg(dim)));
        } else {
            let mut spans: Vec<Span<'static>> = vec![Span::styled("      ", Style::default())];
            for (ki, status) in field.key_status.iter().enumerate() {
                if field.revealed == Some(ki) {
                    if let Some(key) = field.keys.get(ki) {
                        let all: Vec<char> = key.chars().collect();
                        let body: String = if all.len() > 30 {
                            format!("{}…", all.iter().take(30).collect::<String>())
                        } else {
                            key.clone()
                        };
                        spans.push(Span::styled(
                            format!("[{}] ", body),
                            Style::default().fg(Color::Rgb(210, 210, 210)),
                        ));
                    }
                } else {
                    spans.push(Span::styled(
                        "\u{25cf} ",
                        Style::default().fg(dot_color(status)),
                    ));
                }
            }
            lines.push(Line::from(spans));
        }

        // Pending new-key line (only for the active row when it has no
        // from_env restriction, so the key counts as typed to the row).
        let pending_style = Style::default().fg(if is_active { tip } else { dim });
        let pending_label = if field.from_env {
            "      env-provided key — edit in your shell profile".to_string()
        } else if field.pending.is_empty() {
            if is_active {
                "      new key…".to_string()
            } else {
                String::new()
            }
        } else {
            format!(
                "      {}{}",
                field.pending,
                if is_active { "▍" } else { "" }
            )
        };
        if !pending_label.is_empty() {
            lines.push(Line::styled(pending_label, pending_style));
        }
    }

    // Delete-confirm subpopup.
    if let Some(dc) = state.delete_confirm {
        if let Some(field) = state.fields.get(dc.field_idx) {
            let key_preview: String = field
                .keys
                .get(dc.key_idx)
                .map(|k| format!("{}…", k.chars().take(12).collect::<String>()))
                .unwrap_or("?".to_string());
            lines.push(Line::styled(
                format!(
                    "  \u{26a0} Delete key {} from {}?  [y]es / [n]o  — {}",
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

    // Static keybind footer (vim vs arrows).
    let footer = if vim_enabled {
        "  j/k move · enter reveal → auto-save+validate · type+enter add · ctrl+v paste · del confirm delete · esc close"
    } else {
        "  ↑/↓ move · enter reveal → auto-save+validate · type+enter add · ctrl+v paste · del confirm delete · esc close"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> KeysDialogState {
        let mut s = KeysDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, vec!["k1".into(), "k2".into()])]);
        s
    }

    #[test]
    fn defaults_hidden_with_all_catalog_rows() {
        let s = KeysDialogState::new();
        assert!(!s.visible);
        assert_eq!(s.fields.len(), FREE_CATALOG.len());
    }

    #[test]
    fn open_seeds_existing_keys_masked() {
        let s = seeded();
        assert!(s.visible);
        assert_eq!(s.fields[0].keys, vec!["k1", "k2"]);
        assert_eq!(s.fields[0].key_status.len(), 2);
        assert_eq!(s.fields[0].revealed, None, "keys start masked");
    }

    #[test]
    fn nav_covers_every_catalog_row() {
        let mut s = seeded();
        // visible_field_indices covers all rows (even unconfigured ones), so
        // the user can move to an empty upstream and add its first key.
        assert_eq!(s.visible_field_indices().len(), s.fields.len());
        s.active_idx = 0;
        s.move_next();
        assert_eq!(s.active_idx, 1);
        s.move_prev();
        assert_eq!(s.active_idx, 0);
    }

    #[test]
    fn move_next_wraps() {
        let mut s = seeded();
        s.active_idx = s.fields.len() - 1;
        s.move_next();
        assert_eq!(s.active_idx, 0, "should wrap to first field");
    }

    #[test]
    fn enter_reveals_first_key_then_hides() {
        let mut s = seeded();
        s.enter_active();
        assert_eq!(s.fields[0].revealed, Some(0));
        s.enter_active();
        assert_eq!(s.fields[0].revealed, None, "enter toggles reveal");
    }

    #[test]
    fn enter_appends_pending_and_creates_new_dot() {
        let mut s = seeded();
        s.fields[0].pending = "new-key".into();
        s.enter_active();
        assert_eq!(s.fields[0].keys, vec!["k1", "k2", "new-key"]);
        assert_eq!(s.fields[0].pending, "", "pending cleared after commit");
        assert_eq!(s.fields[0].key_status.len(), 3);
    }

    #[test]
    fn insert_and_backspace_edit_only_pending() {
        let mut s = seeded();
        s.insert_char('a');
        s.insert_char('b');
        assert_eq!(s.fields[0].pending, "ab");
        s.backspace();
        assert_eq!(s.fields[0].pending, "a");
        assert_eq!(s.fields[0].keys.len(), 2, "stored keys untouched");
    }

    #[test]
    fn append_pending_ignores_blank() {
        let mut s = seeded();
        s.fields[0].pending = "   ".into();
        assert!(!s.append_pending());
        assert_eq!(s.fields[0].keys.len(), 2);
    }

    #[test]
    fn delete_requires_revealed_key_then_confirms() {
        let mut s = seeded();
        // Not revealed — no confirm popup.
        assert!(!s.try_open_delete_confirm());
        s.enter_active();
        assert!(s.try_open_delete_confirm());
        assert!(s.delete_confirm.is_some());
        s.confirm_delete();
        assert!(s.delete_confirm.is_none());
        assert_eq!(s.fields[0].keys, vec!["k2"], "revealed key removed");
        assert_eq!(s.fields[0].key_status.len(), 1);
        assert_eq!(s.fields[0].revealed, None);
    }

    #[test]
    fn cancel_delete_keeps_key() {
        let mut s = seeded();
        s.enter_active();
        s.try_open_delete_confirm();
        s.cancel_delete();
        assert!(s.delete_confirm.is_none());
        assert_eq!(s.fields[0].keys.len(), 2);
    }

    #[test]
    fn esc_cascade_hides_then_clears_then_closes() {
        let mut s = seeded();
        s.enter_active(); // reveal
        assert!(s.unreveal_active(), "first Esc re-masks");
        assert!(!s.unreveal_active());
        s.fields[0].pending = "abc".into();
        assert!(s.clear_pending(), "second Esc drops typed text");
        assert!(!s.clear_pending());
        s.close();
        assert!(!s.visible);
    }

    #[test]
    fn store_updates_includes_empty_rows_to_clear_store() {
        let s = seeded();
        let updates = s.store_updates();
        // Every non-env field is present — an empty row signals "clear".
        assert_eq!(updates.len(), s.fields.len());
        let (id, keys) = &updates[0];
        assert_eq!(*id, FREE_CATALOG[0].id);
        assert_eq!(*keys, vec!["k1", "k2"]);
    }

    #[test]
    fn env_var_rows_are_read_only_and_excluded_from_updates() {
        let mut s = KeysDialogState::new();
        s.open(&[]);
        s.set_env_var_keys(&[(FREE_CATALOG[1].id, "env-key".into())]);
        assert!(s.fields[1].from_env);
        let updates = s.store_updates();
        assert!(
            !updates.iter().any(|(id, _)| *id == FREE_CATALOG[1].id),
            "env-var rows never reach the auth store"
        );
        s.fields[1].pending = "x".into();
        assert!(!s.append_pending(), "env rows reject new keys");
    }

    #[test]
    fn filled_count_sums_all_rows() {
        let s = seeded();
        assert_eq!(s.filled_count(), 2);
    }
}
