// custom_provider_dialog.rs — Modal dialog for entering a custom provider URL and API key.
//
// Collects both a base URL and an API key for the custom OpenAI-compatible
// provider used by /connect.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::input_layout::{byte_offset_of_char, split_at_display_col, WrappedInput};
use crate::overlays::{
    centered_rect, render_dark_overlay, render_dialog_bg, CLAWDE_ACCENT, CLAWDE_PANEL_BG,
};
use crate::vim_search::VimSearch;
use std::cell::Cell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomProviderField {
    Url,
    ApiKey,
}

pub struct CustomProviderDialogState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub provider_id: String,
    pub provider_name: String,
    pub url_input: String,
    pub url_cursor: usize,
    pub api_key_input: String,
    pub api_key_cursor: usize,
    pub active_field: CustomProviderField,
    /// Vim-modal insert state (only used when vim is enabled). The dialog is
    /// a text entry, so it opens in insert; `Esc` exits insert before closing.
    pub vim_search: VimSearch,
}

impl Default for CustomProviderDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl CustomProviderDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            provider_id: String::new(),
            provider_name: String::new(),
            url_input: String::new(),
            url_cursor: 0,
            api_key_input: String::new(),
            api_key_cursor: 0,
            active_field: CustomProviderField::Url,
            last_rect: Cell::new(Rect::default()),
            vim_search: VimSearch::new(),
        }
    }

    pub fn open(
        &mut self,
        provider_id: String,
        provider_name: String,
        current_url: Option<String>,
    ) {
        self.visible = true;
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.url_input = current_url.unwrap_or_default();
        self.url_cursor = self.url_input.len();
        self.api_key_input.clear();
        self.api_key_cursor = 0;
        self.active_field = CustomProviderField::Url;
        self.vim_search.enter_insert();
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.url_input.clear();
        self.url_cursor = 0;
        self.api_key_input.clear();
        self.api_key_cursor = 0;
        self.active_field = CustomProviderField::Url;
        self.vim_search.reset();
    }

    /// The active field's text and its byte cursor, borrowed together so the
    /// editing helpers below share one implementation.
    fn active_text_and_cursor(&mut self) -> (&mut String, &mut usize) {
        match self.active_field {
            CustomProviderField::Url => (&mut self.url_input, &mut self.url_cursor),
            CustomProviderField::ApiKey => (&mut self.api_key_input, &mut self.api_key_cursor),
        }
    }

    /// Park the cursor at the end of the newly focused field.
    fn focus_active_end(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        *cursor = text.len();
    }

    pub fn move_next_field(&mut self) {
        self.active_field = match self.active_field {
            CustomProviderField::Url => CustomProviderField::ApiKey,
            CustomProviderField::ApiKey => CustomProviderField::Url,
        };
        self.focus_active_end();
    }

    pub fn move_prev_field(&mut self) {
        self.active_field = match self.active_field {
            CustomProviderField::Url => CustomProviderField::ApiKey,
            CustomProviderField::ApiKey => CustomProviderField::Url,
        };
        self.focus_active_end();
    }

    pub fn insert_char(&mut self, c: char) {
        let (text, cursor) = self.active_text_and_cursor();
        text.insert(*cursor, c);
        *cursor += c.len_utf8();
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        if *cursor == 0 {
            return;
        }
        let prev = text[..*cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
        text.remove(prev);
        *cursor = prev;
    }

    /// Delete the character under the cursor.
    pub fn delete_char(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        if *cursor < text.len() {
            text.remove(*cursor);
        }
    }

    pub fn move_cursor_left(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        if *cursor == 0 {
            return;
        }
        *cursor = text[..*cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0);
    }

    pub fn move_cursor_right(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        if *cursor >= text.len() {
            return;
        }
        let step = text[*cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| i)
            .unwrap_or_else(|| text.len() - *cursor);
        *cursor += step;
    }

    pub fn move_cursor_home(&mut self) {
        let (_, cursor) = self.active_text_and_cursor();
        *cursor = 0;
    }

    pub fn move_cursor_end(&mut self) {
        let (text, cursor) = self.active_text_and_cursor();
        *cursor = text.len();
    }

    pub fn can_submit(&self) -> bool {
        !self.url_input.trim().is_empty()
    }

    pub fn take_values(&mut self) -> (String, String) {
        let url = self.url_input.trim().to_string();
        let api_key = self.api_key_input.clone();
        self.close();
        (url, api_key)
    }
}

pub fn render_custom_provider_dialog(
    frame: &mut Frame,
    state: &CustomProviderDialogState,
    vim_enabled: bool,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let pink = CLAWDE_ACCENT;
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAWDE_PANEL_BG;

    render_dark_overlay(frame, area);

    // The value lines wrap, so the dialog's width (and therefore the wrap
    // width) is fixed first; the lines are then built and the height is sized
    // to fit them.
    let width = 76u16.min(area.width.saturating_sub(4));
    let inner_w = width.saturating_sub(2);
    let wrap_w = inner_w.saturating_sub(1) as usize;

    let title_text = format!("Connect {}", state.provider_name);
    let title_pad = inner_w.saturating_sub(title_text.len() as u16 + 5) as usize;

    let url_style = if state.active_field == CustomProviderField::Url {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let key_style = if state.active_field == CustomProviderField::ApiKey {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let url_text = if state.url_input.is_empty() {
        "https://your-openai-compatible-endpoint/v1".to_string()
    } else {
        state.url_input.clone()
    };
    let url_cursor = state.url_cursor.min(state.url_input.len());

    let masked_key = if state.api_key_input.is_empty() {
        "paste your API key here...".to_string()
    } else {
        mask_api_key(&state.api_key_input)
    };
    // The mask preserves the character count, so map the raw char-indexed
    // cursor back to a byte offset in the masked string.
    let key_cursor = if state.api_key_input.is_empty() {
        0
    } else {
        let char_index = state.api_key_input[..state.api_key_cursor.min(state.api_key_input.len())]
            .chars()
            .count();
        byte_offset_of_char(&masked_key, char_index)
    };

    let url_layout = WrappedInput::layout(&url_text, url_cursor, wrap_w);
    let key_layout = WrappedInput::layout(&masked_key, key_cursor, wrap_w);

    let confirm_hint = if state.can_submit() {
        " enter confirm"
    } else {
        " fill URL field"
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
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
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " URL:",
        Style::default().fg(muted),
    )]));
    push_wrapped_value(
        &mut lines,
        &url_layout,
        url_style,
        state.active_field == CustomProviderField::Url,
        pink,
    );
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        " API Key:",
        Style::default().fg(muted),
    )]));
    push_wrapped_value(
        &mut lines,
        &key_layout,
        key_style,
        state.active_field == CustomProviderField::ApiKey,
        pink,
    );
    lines.push(Line::from(""));
    let mut hint_spans = vec![
        Span::styled(" tab", Style::default().fg(dim)),
        Span::styled(" switch field  ", Style::default().fg(dim)),
        Span::styled(confirm_hint, Style::default().fg(dim)),
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
        .max(11);
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
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
fn mask_api_key(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 4 {
        value.to_string()
    } else {
        let visible: String = chars[chars.len() - 4..].iter().collect();
        format!("{}{}", "•".repeat(chars.len() - 4), visible)
    }
}

/// Append one `Line` per wrapped row of a field value, drawing a block cursor
/// on the insertion row when the field is focused.
fn push_wrapped_value(
    lines: &mut Vec<Line<'static>>,
    layout: &WrappedInput,
    style: Style,
    active: bool,
    cursor_fg: Color,
) {
    for (i, text) in layout.lines.iter().enumerate() {
        let mut spans = vec![Span::styled(" ".to_string(), style)];
        if active && i == layout.cursor_row {
            let (before, after) = split_at_display_col(text, layout.cursor_col);
            if !before.is_empty() {
                spans.push(Span::styled(before, style));
            }
            spans.push(Span::styled(
                "█".to_string(),
                Style::default().fg(cursor_fg),
            ));
            if !after.is_empty() {
                spans.push(Span::styled(after, style));
            }
        } else {
            spans.push(Span::styled(text.clone(), style));
        }
        lines.push(Line::from(spans));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_edits_at_insertion_point() {
        let mut s = CustomProviderDialogState::new();
        s.open("custom".into(), "Custom".into(), None);
        for c in "helo".chars() {
            s.insert_char(c);
        }
        s.move_cursor_left(); // between 'l' and 'o'
        s.insert_char('l');
        assert_eq!(s.url_input, "hello");
        assert_eq!(s.url_cursor, 4);

        s.move_cursor_home();
        s.delete_char(); // delete the leading 'h'
        assert_eq!(s.url_input, "ello");

        s.move_cursor_end();
        s.backspace();
        assert_eq!(s.url_input, "ell");
    }

    #[test]
    fn switching_fields_parks_the_cursor_at_the_end() {
        let mut s = CustomProviderDialogState::new();
        s.open(
            "custom".into(),
            "Custom".into(),
            Some("https://x.example/v1".into()),
        );
        assert_eq!(s.url_cursor, s.url_input.len());
        s.move_next_field();
        assert_eq!(s.active_field, CustomProviderField::ApiKey);
        for c in "sk-abc".chars() {
            s.insert_char(c);
        }
        assert_eq!(s.api_key_cursor, 6);
        s.move_prev_field();
        assert_eq!(s.active_field, CustomProviderField::Url);
        // The URL cursor is still where it was; the key cursor is untouched.
        assert_eq!(s.api_key_cursor, 6);
    }

    #[test]
    fn long_url_wraps_instead_of_clipping() {
        let url = "https://a-very-long-openai-compatible-endpoint.example.com/v1/chat/completions";
        let layout = WrappedInput::layout(url, url.len(), 40);
        assert!(layout.lines.len() > 1, "long URL must wrap");
        for line in &layout.lines {
            assert!(line.chars().count() <= 40);
        }
        assert_eq!(layout.cursor_row, layout.lines.len() - 1);
    }
}
