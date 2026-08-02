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

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAURST_PANEL_BG};

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
    }

    /// Close and clear the dialog.
    pub fn close(&mut self) {
        self.visible = false;
        self.input.clear();
        self.cursor_pos = 0;
        self.pending_token = None;
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
        self.input = format!("{}:{}", id, token);
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
pub fn render_key_input_dialog(frame: &mut Frame, state: &KeyInputDialogState, area: Rect) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let dialog_bg = CLAURST_PANEL_BG;

    // ── Darken the entire background ──
    render_dark_overlay(frame, area);

    // ── Dialog size ──
    let width = 60u16.min(area.width.saturating_sub(4));
    let height = 9u16;
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

    // ── Build lines ──
    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row: "Connect {provider}" on left, "esc" on right
    let title_text = format!("Connect {}", state.provider_name);
    let title_pad = inner.width.saturating_sub(title_text.len() as u16 + 5) as usize;
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

    // "API Key:" or "Cloudflare Account ID:" label, depending on the step.
    let awaiting_id = state.pending_token.is_some();
    lines.push(Line::from(vec![Span::styled(
        if awaiting_id {
            " Cloudflare Account ID:"
        } else {
            " API Key:"
        },
        Style::default().fg(Color::Rgb(180, 180, 180)),
    )]));

    // Masked key display (show last 4 chars, mask the rest). During the
    // two-step flow the placeholder asks for the account ID explicitly.
    let masked = if state.input.is_empty() {
        if awaiting_id {
            "Paste your Cloudflare ID now...".to_string()
        } else {
            "paste your API key here...".to_string()
        }
    } else {
        let len = state.input.len();
        if len <= 4 {
            state.input.clone()
        } else {
            format!("{}{}", "\u{2022}".repeat(len - 4), &state.input[len - 4..])
        }
    };

    let input_style = if state.input.is_empty() {
        Style::default().fg(dim)
    } else {
        Style::default().fg(Color::White)
    };

    lines.push(Line::from(vec![
        Span::styled(format!(" {}", masked), input_style),
        Span::styled("_", Style::default().fg(pink)), // cursor
    ]));

    // Blank line
    lines.push(Line::from(""));

    // Hint row
    lines.push(Line::from(vec![
        Span::styled(" enter", Style::default().fg(dim)),
        Span::styled(" confirm", Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}
