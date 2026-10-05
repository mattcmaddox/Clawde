// key_input_dialog.rs — Masked text input overlay for entering API keys.
//
// Provides a modal dialog that collects an API key from the user with
// masked display (showing only the last 4 characters).

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::cell::Cell;

use crate::input_layout::{byte_offset_of_char, split_at_display_col, WrappedInput};
use crate::key_editor::{compose_composite_key, is_composite_key_provider};
use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};
use crate::vim_search::VimSearch;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// State for the API key input dialog.
pub struct KeyInputDialogState {
    pub visible: bool,
    pub provider_id: String,
    pub provider_name: String,
    pub input: String,
    pub cursor_pos: usize,
    /// Two-step composite-key flow (cloudflare): when `Some`, the API token
    /// was captured on the first Enter and the dialog now awaits the account
    /// ID in `input`. The next Enter joins them into the stored
    /// `ACCOUNT_ID:API_TOKEN`.
    pub pending_token: Option<String>,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    /// Vim-modal insert state (only used when vim is enabled). The dialog is
    /// a text entry, so it opens in insert; `Esc` exits insert before closing.
    pub vim_search: VimSearch,
}

impl Default for KeyInputDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyInputDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            provider_id: String::new(),
            provider_name: String::new(),
            input: String::new(),
            cursor_pos: 0,
            pending_token: None,
            last_rect: Cell::new(Rect::default()),
            vim_search: VimSearch::new(),
        }
    }

    /// Open the dialog for a specific provider.
    pub fn open(&mut self, provider_id: String, provider_name: String) {
        self.visible = true;
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.input.clear();
        self.cursor_pos = 0;
        self.pending_token = None;
        self.vim_search.enter_insert();
    }

    /// Close and clear the dialog.
    pub fn close(&mut self) {
        self.visible = false;
        self.input.clear();
        self.cursor_pos = 0;
        self.pending_token = None;
        self.vim_search.reset();
    }

    /// Capture the typed token and switch to the account-ID prompt.
    /// Returns `false` when there is nothing to capture.
    pub fn capture_token(&mut self) -> bool {
        let token = self.input.trim().to_string();
        if token.is_empty() {
            return false;
        }
        self.pending_token = Some(token);
        self.input.clear();
        self.cursor_pos = 0;
        true
    }

    /// Join the typed account ID with the captured token and store it back
    /// into `input` as the composite `ACCOUNT_ID:API_TOKEN`. Returns `false`
    /// when there is no captured token (or the ID is empty).
    pub fn compose_with_id(&mut self) -> bool {
        let Some(token) = self.pending_token.take() else {
            return false;
        };
        let id = self.input.trim().to_string();
        if id.is_empty() {
            self.pending_token = Some(token);
            return false;
        }
        self.input = compose_composite_key(&id, &token);
        self.cursor_pos = self.input.len();
        true
    }

    /// Cancel the two-step flow, restoring the captured token to `input` so
    /// it can be re-entered. Returns `true` if a capture was undone.
    pub fn cancel_token(&mut self) -> bool {
        if let Some(token) = self.pending_token.take() {
            self.input = token;
            self.cursor_pos = self.input.len();
            true
        } else {
            false
        }
    }

    /// Insert a character at the cursor position.
    pub fn insert_char(&mut self, c: char) {
        self.input.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor_pos > 0 {
            // Find the previous char boundary
            let prev = self.input[..self.cursor_pos]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input.remove(prev);
            self.cursor_pos = prev;
        }
    }

    /// Delete the character under the cursor.
    pub fn delete_char(&mut self) {
        if self.cursor_pos < self.input.len() {
            self.input.remove(self.cursor_pos);
        }
    }

    pub fn move_cursor_left(&mut self) {
        if self.cursor_pos == 0 {
            return;
        }
        self.cursor_pos = self.input[..self.cursor_pos]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    pub fn move_cursor_right(&mut self) {
        if self.cursor_pos >= self.input.len() {
            return;
        }
        self.cursor_pos += self.input[self.cursor_pos..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| i)
            .unwrap_or_else(|| self.input.len() - self.cursor_pos);
    }

    pub fn move_cursor_home(&mut self) {
        self.cursor_pos = 0;
    }

    pub fn move_cursor_end(&mut self) {
        self.cursor_pos = self.input.len();
    }

    /// Take the entered key and close the dialog.
    pub fn take_key(&mut self) -> String {
        let key = self.input.clone();
        self.close();
        key
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the key input dialog overlay — OpenCode-style: dark overlay, no
/// border, minimal and polished.
pub fn render_key_input_dialog(
    frame: &mut Frame,
    state: &KeyInputDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let dialog_bg = CLAWDE_PANEL_BG;

    // ── Darken the entire background ──
    render_dark_overlay(frame, area);

    // ── Dialog size ──
    // Width (and therefore wrap width) is fixed first; the lines are then
    // built and the height sized to fit, so a long key grows the dialog
    // instead of being clipped.
    let width = 60u16.min(area.width.saturating_sub(4));
    let inner_w = width.saturating_sub(2);
    let wrap_w = inner_w.saturating_sub(1) as usize;

    // "API Key:" or "Cloudflare Account ID:" label, depending on the step.
    let awaiting_id =
        is_composite_key_provider(&state.provider_id) && state.pending_token.is_some();

    // Masked key display (show last 4 chars, mask the rest). During the
    // two-step flow the placeholder asks for the account ID explicitly.
    let masked = if state.input.is_empty() {
        if awaiting_id {
            "Paste your Cloudflare ID now...".to_string()
        } else {
            "paste your API key here...".to_string()
        }
    } else {
        mask_key(&state.input)
    };
    // The mask preserves the character count, so map the raw char-indexed
    // cursor back to a byte offset in the masked string.
    let cursor = if state.input.is_empty() {
        0
    } else {
        let char_index = state.input[..state.cursor_pos.min(state.input.len())]
            .chars()
            .count();
        byte_offset_of_char(&masked, char_index)
    };

    let input_style = if state.input.is_empty() {
        Style::default().fg(dim)
    } else {
        Style::default().fg(Color::White)
    };
    let masked_layout = WrappedInput::layout(&masked, cursor, wrap_w);

    // ── Build lines ──
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row: "Connect {provider}" on left, "esc" on right
    let title_text = format!("Connect {}", state.provider_name);
    let title_pad = inner_w.saturating_sub(title_text.len() as u16 + 5) as usize;
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", title_text),
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>width$}", "esc ", width = title_pad),
            Style::default().fg(dim),
        ),
    ]));

    // Blank line
    lines.push(Line::from(""));

    lines.push(Line::from(vec![Span::styled(
        if awaiting_id {
            " Cloudflare Account ID:"
        } else {
            " API Key:"
        },
        Style::default().fg(Color::Rgb(180, 180, 180)),
    )]));

    // One `Line` per wrapped row, with a block cursor at the insertion point.
    for (i, line_text) in masked_layout.lines.iter().enumerate() {
        let mut spans = vec![Span::styled(" ".to_string(), input_style)];
        if i == masked_layout.cursor_row {
            let (before, after) = split_at_display_col(line_text, masked_layout.cursor_col);
            if !before.is_empty() {
                spans.push(Span::styled(before, input_style));
            }
            spans.push(Span::styled("█".to_string(), Style::default().fg(pink)));
            if !after.is_empty() {
                spans.push(Span::styled(after, input_style));
            }
        } else {
            spans.push(Span::styled(line_text.clone(), input_style));
        }
        lines.push(Line::from(spans));
    }

    // Blank line
    lines.push(Line::from(""));

    // Hint row
    let mut hint_spans = vec![
        Span::styled(" enter", Style::default().fg(dim)),
        Span::styled(" confirm", Style::default().fg(dim)),
    ];
    if vim_enabled && state.vim_search.insert {
        hint_spans.push(Span::styled(
            "   -- INSERT --",
            Style::default().fg(dim).add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(hint_spans));

    let height = (lines.len() as u16 + 2)
        .min(area.height.saturating_sub(2))
        .max(9);
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);

    // ── Fill dialog background (no border) ──
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

/// Mask an API key, keeping only the last four characters visible.
fn mask_key(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        value.to_string()
    } else {
        let visible: String = chars[chars.len() - 4..].iter().collect();
        format!("{}{}", "\u{2022}".repeat(chars.len() - 4), visible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_two_step_composes_via_the_shared_helper() {
        let mut s = KeyInputDialogState::new();
        s.open("cloudflare".into(), "Cloudflare".into());
        for c in "tok-123456789".chars() {
            s.insert_char(c);
        }
        assert!(s.capture_token());
        assert_eq!(s.pending_token.as_deref(), Some("tok-123456789"));
        assert!(s.input.is_empty(), "token prompt clears the input");
        for c in "acct-987654321".chars() {
            s.insert_char(c);
        }
        assert!(s.compose_with_id());
        assert_eq!(s.input, "acct-987654321:tok-123456789");
        assert_eq!(s.pending_token, None);
    }

    #[test]
    fn cancel_token_restores_the_input_line() {
        let mut s = KeyInputDialogState::new();
        s.open("cloudflare".into(), "Cloudflare".into());
        s.insert_char('a');
        s.insert_char('b');
        assert!(s.capture_token());
        assert!(s.cancel_token());
        assert_eq!(s.input, "ab");
        assert_eq!(s.pending_token, None);
    }

    #[test]
    fn compose_with_id_without_a_token_is_a_no_op() {
        let mut s = KeyInputDialogState::new();
        s.open("cloudflare".into(), "Cloudflare".into());
        s.insert_char('x');
        assert!(!s.compose_with_id());
        assert_eq!(s.input, "x");
    }

    #[test]
    fn cursor_edits_at_insertion_point() {
        let mut s = KeyInputDialogState::new();
        s.open("openrouter".into(), "OpenRouter".into());
        for c in "helo".chars() {
            s.insert_char(c);
        }
        s.move_cursor_left(); // between 'l' and 'o'
        s.insert_char('l');
        assert_eq!(s.input, "hello");
        assert_eq!(s.cursor_pos, 4);

        s.move_cursor_home();
        s.delete_char(); // delete the leading 'h'
        assert_eq!(s.input, "ello");

        s.move_cursor_end();
        s.backspace();
        assert_eq!(s.input, "ell");
    }

    #[test]
    fn mask_key_keeps_the_last_four_characters() {
        assert_eq!(mask_key("abcd"), "abcd");
        let expected = format!("{}{}", "\u{2022}".repeat(9), "1234");
        assert_eq!(mask_key("sk-or-v1-1234"), expected);
    }

    #[test]
    fn long_key_wraps_instead_of_clipping() {
        let key = "sk-or-v1-".to_string() + &"a".repeat(80);
        let masked = mask_key(&key);
        let layout = WrappedInput::layout(&masked, masked.len(), 40);
        assert!(layout.lines.len() > 1, "a long masked key must wrap");
        for line in &layout.lines {
            assert!(line.chars().count() <= 40);
        }
    }
}
