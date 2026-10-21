// credential_import_dialog.rs — the "import key from file" popup opened from
// `/keys`. It owns only the path buffer and the last error; reading and
// parsing the file lives in `AuthStore::read_opencode_credential_file`, and
// storing the key is the App's job (it goes through the same persistence
// path as the `/keys` editor).

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

#[derive(Debug, Default)]
pub struct CredentialImportState {
    pub visible: bool,
    pub path: String,
    pub error: Option<String>,
}

impl CredentialImportState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self) {
        self.visible = true;
        self.path.clear();
        self.error = None;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.path.clear();
        self.error = None;
    }

    pub fn insert_char(&mut self, c: char) {
        self.path.push(c);
        self.error = None;
    }

    pub fn backspace(&mut self) {
        self.path.pop();
        self.error = None;
    }

    /// Paste a clipboard path. Only the first line is taken, so a trailing
    /// newline from the clipboard does not end up in the path.
    pub fn paste(&mut self, text: &str) {
        if let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) {
            self.path.push_str(line);
            self.error = None;
        }
    }

    /// The typed path with a leading `~/` expanded to the home directory, or
    /// `None` when the buffer is blank.
    pub fn resolved_path(&self) -> Option<std::path::PathBuf> {
        let trimmed = self.path.trim();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.strip_prefix("~/") {
            Some(rest) => dirs::home_dir().map(|home| home.join(rest)),
            None => Some(std::path::PathBuf::from(trimmed)),
        }
    }
}

/// Render the import popup centered over `area`.
pub fn render_credential_import(frame: &mut Frame, state: &CredentialImportState, area: Rect) {
    if !state.visible {
        return;
    }
    let width = area.width.min(78);
    let height = area.height.min(9);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Import OpenCode key from file ");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new(Line::styled(
            "Path to an auth.json, .env, or text file holding an oc_sk_ key",
            Style::default().fg(Color::Rgb(90, 90, 90)),
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("> "),
            Span::raw(state.path.as_str()),
            Span::styled("_", Style::default().fg(Color::Rgb(90, 90, 90))),
        ])),
        rows[1],
    );
    let (text, color) = match &state.error {
        Some(err) => (err.as_str(), Color::Red),
        None => (
            "enter import · esc cancel · ctrl+v paste",
            Color::Rgb(90, 90, 90),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(text, Style::default().fg(color))).alignment(Alignment::Left),
        rows[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_edits_the_path_and_clears_errors() {
        let mut s = CredentialImportState::new();
        s.open();
        s.insert_char('a');
        s.error = Some("bad".into());
        s.insert_char('b');
        assert_eq!(s.path, "ab");
        assert_eq!(s.error, None, "typing clears the previous error");
        s.backspace();
        assert_eq!(s.path, "a");
    }

    #[test]
    fn paste_takes_the_first_non_blank_line_only() {
        let mut s = CredentialImportState::new();
        s.open();
        s.paste("  /tmp/key.env \n/other\n");
        assert_eq!(s.path, "/tmp/key.env");
    }

    #[test]
    fn blank_path_resolves_to_none() {
        let mut s = CredentialImportState::new();
        s.open();
        s.path = "   ".into();
        assert_eq!(s.resolved_path(), None);
    }

    #[test]
    fn close_resets_the_buffer() {
        let mut s = CredentialImportState::new();
        s.open();
        s.insert_char('x');
        s.close();
        assert!(!s.visible);
        assert!(s.path.is_empty());
    }
}
