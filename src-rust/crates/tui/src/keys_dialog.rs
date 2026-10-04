// keys_dialog.rs — the `/keys` popup: a thin renderer over the shared key
// editor.
//
// The state machine (navigation, reveal, append, delete-confirm, validation)
// lives in `crate::key_editor`. This module owns only the `/keys`-specific
// rendering and re-exports the state type so existing call sites keep working.
//
// Stored keys are NEVER shown by default — a row's key list is only visible
// while `revealed` is `Some` (the value is the selection cursor). Expanding is
// view-only: typing/pasting is blocked until the row is collapsed again, so an
// accidental keystroke can never corrupt a stored key. The state is
// deliberately decoupled from `AuthStore` so the App owns persistence.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::Frame;

pub use crate::key_editor::ValidationPing;
use crate::key_editor::{render_key_editor, KeyEditorChrome, KeyEditorState, KeyRow};

/// Backward-compatible alias: one row in the editor.
pub type KeysField = KeyRow;

/// State for the `/keys` popup. A newtype over the shared [`KeyEditorState`]
/// with `Deref`, so every navigation/CRUD method is the shared implementation.
pub struct KeysDialogState {
    pub editor: KeyEditorState,
}

impl Default for KeysDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeysDialogState {
    pub fn new() -> Self {
        Self {
            editor: KeyEditorState::new(),
        }
    }
}

impl std::ops::Deref for KeysDialogState {
    type Target = KeyEditorState;
    fn deref(&self) -> &Self::Target {
        &self.editor
    }
}

impl std::ops::DerefMut for KeysDialogState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.editor
    }
}

/// Render the `/keys` popup: j/k-navigable row list with masked key dots,
/// reveal-on-Enter, a pending new-key line, and the inline delete prompt.
pub fn render_keys_dialog(
    frame: &mut Frame,
    state: &KeysDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }
    let total_keys = state.filled_count();
    let chrome = KeyEditorChrome {
        width: 88,
        height: 30,
        title: format!(
            "/keys \u{2014} {} key{}",
            total_keys,
            if total_keys == 1 { "" } else { "s" }
        ),
        description: vec![
            Line::styled(
                "  j/k nav \u{00b7} enter reveal/add \u{00b7} \u{2190}/\u{2192} select key \u{00b7} del delete \u{00b7} ctrl+v paste \u{00b7} esc close",
                Style::default().fg(Color::Rgb(90, 90, 90)),
            ),
            Line::styled("", Style::default()),
        ],
    };
    render_key_editor(frame, state, vim_enabled, area, &chrome);
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_api::FREE_CATALOG;

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
    fn enter_expands_all_keys_then_collapses() {
        let mut s = seeded();
        s.enter_active();
        assert_eq!(
            s.fields[0].revealed,
            Some(0),
            "cursor lands on the first key"
        );
        assert!(s.active_is_revealed());
        assert_eq!(s.active_key_count(), 2, "every stored key is shown");
        s.enter_active();
        assert_eq!(s.fields[0].revealed, None, "enter toggles expansion");
        assert!(!s.active_is_revealed());
    }

    #[test]
    fn selection_cursor_moves_and_wraps_within_the_expanded_row() {
        let mut s = seeded();
        s.select_next_key();
        assert_eq!(s.fields[0].revealed, None, "no-op while masked");
        s.enter_active();
        s.select_next_key();
        assert_eq!(s.fields[0].revealed, Some(1));
        s.select_next_key();
        assert_eq!(s.fields[0].revealed, Some(0), "wraps past the last key");
        s.select_prev_key();
        assert_eq!(s.fields[0].revealed, Some(1), "wraps before the first key");
    }

    #[test]
    fn delete_removes_the_selected_key_not_always_the_first() {
        let mut s = seeded();
        s.enter_active();
        s.select_next_key();
        assert_eq!(s.fields[0].revealed, Some(1));
        assert!(s.try_open_delete_confirm());
        assert!(s.confirm_delete());
        assert_eq!(s.fields[0].keys, vec!["k1"], "the selected second key went");
        assert_eq!(
            s.fields[0].revealed,
            Some(0),
            "cursor clamps onto the survivor"
        );
    }

    #[test]
    fn delete_collapses_a_row_that_loses_its_last_key() {
        let mut s = seeded();
        s.enter_active();
        s.select_next_key();
        s.try_open_delete_confirm();
        assert!(s.confirm_delete());
        // Now delete the remaining key.
        s.try_open_delete_confirm();
        assert!(s.confirm_delete());
        assert!(s.fields[0].keys.is_empty());
        assert_eq!(s.fields[0].revealed, None, "empty row re-masks");
    }

    #[test]
    fn expanded_row_is_view_only() {
        let mut s = seeded();
        s.enter_active();
        s.insert_char('x');
        s.paste_key("y");
        s.backspace();
        assert_eq!(s.fields[0].pending, "", "typing is blocked while expanded");
        s.enter_active(); // collapse
        s.insert_char('x');
        assert_eq!(s.fields[0].pending, "x", "typing resumes once collapsed");
    }

    #[test]
    fn row_nav_collapses_the_expanded_row() {
        let mut s = seeded();
        s.enter_active();
        assert!(s.active_is_revealed());
        s.move_next();
        assert_eq!(s.fields[0].revealed, None, "leaving the row re-masks it");
        assert_eq!(s.active_idx, 1);
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
    fn paste_key_trims_to_a_single_token() {
        let mut s = seeded();
        // A clipboard key often carries a trailing newline and padding.
        s.paste_key("  sk-abc123\n");
        assert_eq!(s.fields[0].pending, "sk-abc123");
        // Pasting again appends rather than replacing.
        s.paste_key("tail");
        assert_eq!(s.fields[0].pending, "sk-abc123tail");
    }

    #[test]
    fn paste_key_ignores_blank_and_env_rows() {
        let mut s = seeded();
        s.paste_key("   \n  ");
        assert_eq!(s.fields[0].pending, "", "whitespace-only paste is dropped");
        s.set_env_var_keys(&[(FREE_CATALOG[0].id, "env-key".into())]);
        s.paste_key("should-not-land");
        assert_eq!(s.fields[0].pending, "", "env rows reject pasted text");
    }

    #[test]
    fn append_pending_ignores_blank() {
        let mut s = seeded();
        s.fields[0].pending = "   ".into();
        assert!(!s.append_pending());
        assert_eq!(s.fields[0].keys.len(), 2);
    }

    #[test]
    fn cloudflare_entry_is_two_step_in_manage_too() {
        // Audit A: `/keys` must never store a bare Cloudflare token — the
        // shared append_pending owns the composite flow for both purposes.
        let cf_idx = FREE_CATALOG
            .iter()
            .position(|u| u.id == "cloudflare")
            .expect("cloudflare in catalog");
        let mut s = KeysDialogState::new();
        s.open(&[]);
        s.active_idx = cf_idx;
        for c in "tok-123456789".chars() {
            s.insert_char(c);
        }
        assert!(s.append_pending());
        assert_eq!(
            s.fields[cf_idx].pending_token.as_deref(),
            Some("tok-123456789"),
            "first Enter captures the token"
        );
        assert!(
            s.fields[cf_idx].keys.is_empty(),
            "no key stored until the account ID is entered"
        );
        for c in "acct-987654321".chars() {
            s.insert_char(c);
        }
        assert!(s.append_pending());
        assert_eq!(
            s.fields[cf_idx].keys,
            vec!["acct-987654321:tok-123456789"],
            "second Enter joins the ID and token"
        );
    }

    #[test]
    fn delete_requires_an_expanded_key_then_confirms() {
        let mut s = seeded();
        // Not expanded — no confirm popup.
        assert!(!s.try_open_delete_confirm());
        s.enter_active();
        assert!(s.try_open_delete_confirm());
        assert!(s.delete_confirm.is_some());
        assert!(s.confirm_delete());
        assert!(s.delete_confirm.is_none());
        assert_eq!(s.fields[0].keys, vec!["k2"], "selected key removed");
        assert_eq!(s.fields[0].key_status.len(), 1);
        assert_eq!(s.fields[0].revealed, Some(0), "row stays expanded");
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
    fn confirm_delete_without_a_pending_confirmation_is_a_no_op() {
        let mut s = seeded();
        assert!(!s.confirm_delete());
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
        let (updates, removals) = s.store_updates();
        // Every non-env field is present — an empty row signals "clear".
        assert_eq!(updates.len(), s.fields.len());
        assert!(removals.is_empty(), "Manage never removes credentials");
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
        let (updates, _removals) = s.store_updates();
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

    #[test]
    fn short_pending_key_is_flagged_before_it_is_dropped() {
        // Audit G: a typed key under the store minimum must be surfaced, not
        // silently discarded at save time.
        let mut s = seeded();
        s.fields[0].pending = "abc".into();
        assert!(s.pending_key_too_short());
        s.fields[0].pending = "gsk-12345678".into();
        assert!(!s.pending_key_too_short());
        s.fields[0].pending = "   ".into();
        assert!(!s.pending_key_too_short(), "blank pending is not a key");
    }
}
