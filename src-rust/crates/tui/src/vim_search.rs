// vim_search.rs — Modal search-bar convention for popups when vim mode is active.
//
// Convention (applies at every level of popup navigation):
// - Popup navigation always uses hjkl + arrows (the popups' existing nav arms).
// - Letters do NOT type into a popup's search/filter bar until the user presses
//   `i` to enter insert mode.
// - `Esc` exits insert mode back to normal (navigation) mode; a second `Esc`
//   closes the popup as usual.
// - When vim mode is OFF, popup search bars behave exactly as before
//   (type-to-filter immediately).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Per-popup insert-mode state for vim-modal search bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VimSearch {
    /// `true` while the popup's search bar is in vim insert mode (typing).
    pub insert: bool,
}

impl VimSearch {
    /// A search bar that starts in normal (navigation) mode.
    pub fn new() -> Self {
        Self { insert: false }
    }

    /// Reset to normal mode — call when a popup opens or closes so every
    /// popup starts in navigation mode.
    pub fn reset(&mut self) {
        self.insert = false;
    }

    /// Enter insert mode immediately. Used by text-entry dialogs (key input,
    /// custom provider, free-mode fields) whose primary purpose is typing:
    /// they open in insert so typing works right away, and `Esc` exits insert
    /// before the dialog closes (mirroring the search-bar convention).
    pub fn enter_insert(&mut self) {
        self.insert = true;
    }

    /// Feed one key into a vim-modal search bar.
    ///
    /// When `vim_enabled` is `false` this is a no-op returning
    /// [`VimSearchKey::Passthrough`] — the caller keeps its legacy
    /// type-to-filter behavior.
    ///
    /// When vim is enabled:
    /// - normal mode: bare `i` starts insert mode; every other key passes
    ///   through so the popup's navigation / action arms handle it;
    /// - insert mode: bare chars (incl. Shift-held letters) edit the filter,
    ///   `Backspace` removes a char, `Esc` returns to normal mode; `Enter`,
    ///   arrows, hjkl and other keys pass through so selecting, confirming and
    ///   closing still work (hjkl only navigate after leaving insert mode).
    pub fn handle_key(&mut self, vim_enabled: bool, key: &KeyEvent) -> VimSearchKey {
        if !vim_enabled {
            return VimSearchKey::Passthrough;
        }
        if self.insert {
            match key.code {
                KeyCode::Esc => {
                    self.insert = false;
                    VimSearchKey::Consumed
                }
                KeyCode::Backspace => VimSearchKey::PopChar,
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    VimSearchKey::PushChar(c)
                }
                _ => VimSearchKey::Passthrough,
            }
        } else if key.code == KeyCode::Char('i') && key.modifiers.is_empty() {
            self.insert = true;
            VimSearchKey::Consumed
        } else {
            VimSearchKey::Passthrough
        }
    }
}

/// What a key means for a vim-modal popup search bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimSearchKey {
    /// Key was consumed by the modal state machine (mode toggle).
    Consumed,
    /// A character should be appended to the popup's search filter.
    PushChar(char),
    /// The last character of the search filter should be removed.
    PopChar,
    /// Not a modal key — the popup's normal key handling should run.
    Passthrough,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    #[test]
    fn vim_disabled_is_passthrough_and_never_enters_insert() {
        let mut s = VimSearch::new();
        assert_eq!(
            s.handle_key(false, &key(KeyCode::Char('i'))),
            VimSearchKey::Passthrough
        );
        assert!(!s.insert);
        assert_eq!(
            s.handle_key(false, &key(KeyCode::Char('x'))),
            VimSearchKey::Passthrough
        );
        assert_eq!(
            s.handle_key(false, &key(KeyCode::Backspace)),
            VimSearchKey::Passthrough
        );
    }

    #[test]
    fn normal_mode_i_enters_insert_and_other_keys_pass() {
        let mut s = VimSearch::new();
        // Letters pass through in normal mode (popup nav / actions handle them).
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('j'))),
            VimSearchKey::Passthrough
        );
        assert!(!s.insert);
        // i starts insert mode.
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('i'))),
            VimSearchKey::Consumed
        );
        assert!(s.insert);
    }

    #[test]
    fn shift_i_does_not_enter_insert() {
        let mut s = VimSearch::new();
        assert_eq!(
            s.handle_key(true, &shift_key(KeyCode::Char('I'))),
            VimSearchKey::Passthrough
        );
        assert!(!s.insert);
    }

    #[test]
    fn insert_mode_types_and_edits_filter() {
        let mut s = VimSearch::new();
        s.handle_key(true, &key(KeyCode::Char('i')));
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('s'))),
            VimSearchKey::PushChar('s')
        );
        assert_eq!(
            s.handle_key(true, &shift_key(KeyCode::Char('O'))),
            VimSearchKey::PushChar('O')
        );
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Backspace)),
            VimSearchKey::PopChar
        );
    }

    #[test]
    fn insert_mode_esc_returns_to_normal() {
        let mut s = VimSearch::new();
        s.handle_key(true, &key(KeyCode::Char('i')));
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Esc)),
            VimSearchKey::Consumed
        );
        assert!(!s.insert);
        // After exiting insert, a second Esc is NOT consumed here — the popup
        // handles it (closing), and letters pass through to navigation again.
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Esc)),
            VimSearchKey::Passthrough
        );
    }

    #[test]
    fn insert_mode_ctrl_keys_pass_through() {
        let mut s = VimSearch::new();
        s.handle_key(true, &key(KeyCode::Char('i')));
        let ctrl_p = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(s.handle_key(true, &ctrl_p), VimSearchKey::Passthrough);
        // Enter passes through so confirming a selection still works.
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Enter)),
            VimSearchKey::Passthrough
        );
    }

    #[test]
    fn reset_returns_to_normal_mode() {
        let mut s = VimSearch::new();
        s.handle_key(true, &key(KeyCode::Char('i')));
        s.reset();
        assert!(!s.insert);
    }

    #[test]
    fn enter_insert_starts_typing_and_esc_exits() {
        let mut s = VimSearch::new();
        s.enter_insert();
        assert!(s.insert);
        // In insert-first mode a letter types immediately...
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('x'))),
            VimSearchKey::PushChar('x')
        );
        // ...and Esc exits insert rather than closing.
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Esc)),
            VimSearchKey::Consumed
        );
        assert!(!s.insert);
        // After exiting, letters pass through (normal mode) and i re-enters.
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('x'))),
            VimSearchKey::Passthrough
        );
        assert_eq!(
            s.handle_key(true, &key(KeyCode::Char('i'))),
            VimSearchKey::Consumed
        );
        assert!(s.insert);
    }
}
