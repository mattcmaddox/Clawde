use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::overlays::centered_rect;
use crate::image_paste::PastedImage;

/// Outcome of the file injection dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileInjectionOutcome {
    /// Inject all files, including oversized/binary ones.
    InjectAll,
    /// Inject only within-limit files; skip oversized/binary.
    SkipOversized,
    /// Abort (restore input to prompt for editing).
    Abort,
}

/// State for the file injection warning dialog.
/// Shown when oversized or binary files are detected in @refs.
#[derive(Debug, Clone)]
pub struct FileInjectionDialogState {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// Stashed input text (taken from prompt, must be re-set or sent).
    pub pending_input: Option<String>,
    /// Stashed image attachments at submit time.
    pub pending_imgs: Vec<PastedImage>,
    /// Files that exceeded limits or had issues: (path, size_kb, display_issue).
    pub oversized: Vec<(String, usize, String)>,
    /// Currently selected option: 0 = InjectAll, 1 = SkipOversized, 2 = Abort.
    pub selected: usize,
    /// Set when user confirms; consumed by main.rs to trigger send.
    pub outcome: Option<FileInjectionOutcome>,
}

impl FileInjectionDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            pending_input: None,
            pending_imgs: Vec::new(),
            oversized: Vec::new(),
            selected: 0, // Default to "Inject anyway"
            outcome: None,
        }
    }

    /// Show the dialog with stashed input and oversized files.
    pub fn show(
        &mut self,
        input: String,
        imgs: Vec<PastedImage>,
        oversized: Vec<(String, usize, String)>,
    ) {
        self.visible = true;
        self.pending_input = Some(input);
        self.pending_imgs = imgs;
        self.oversized = oversized;
        self.selected = 0; // Default to "Inject anyway"
        self.outcome = None;
    }

    /// Move selection up (wraps).
    pub fn select_prev(&mut self) {
        self.selected = if self.selected == 0 { 2 } else { self.selected - 1 };
    }

    /// Move selection down (wraps).
    pub fn select_next(&mut self) {
        self.selected = if self.selected == 2 { 0 } else { self.selected + 1 };
    }

    /// Returns the currently-selected outcome option.
    pub fn current_outcome(&self) -> FileInjectionOutcome {
        match self.selected {
            0 => FileInjectionOutcome::InjectAll,
            1 => FileInjectionOutcome::SkipOversized,
            _ => FileInjectionOutcome::Abort,
        }
    }

    /// Returns `true` if the currently-selected option is not "Abort".
    pub fn is_accept_selected(&self) -> bool {
        self.current_outcome() != FileInjectionOutcome::Abort
    }

    /// Confirm the selected option.
    pub fn confirm(&mut self) {
        self.outcome = Some(self.current_outcome());
    }

    /// Dismiss the dialog.
    pub fn dismiss(&mut self) {
        self.visible = false;
        self.pending_input = None;
        self.pending_imgs.clear();
        self.oversized.clear();
        self.outcome = None;
    }

    /// Take the outcome (if set) along with stashed input and images.
    /// Returns None if no outcome is set.
    pub fn take_outcome(&mut self) -> Option<(FileInjectionOutcome, String, Vec<PastedImage>)> {
        let outcome = self.outcome.take()?;
        let input = self.pending_input.take()?;
        let imgs = std::mem::take(&mut self.pending_imgs);
        self.visible = false;
        Some((outcome, input, imgs))
    }
}

impl Default for FileInjectionDialogState {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the file injection warning dialog over the frame.
pub fn render_file_injection_dialog(
    frame: &mut Frame,
    state: &FileInjectionDialogState,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let dialog_width = 72u16.min(area.width.saturating_sub(4));
    let dialog_height = 16u16.min(area.height.saturating_sub(4));
    let dialog_area = centered_rect(dialog_width, dialog_height, area);

    frame.render_widget(Clear, dialog_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![Span::styled(
            " ⚠  File Injection Warning ",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::default().fg(Color::Yellow));

    let inner = block.inner(dialog_area);
    frame.render_widget(block, dialog_area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![Span::styled(
        "The following file(s) cannot be auto-injected:",
        Style::default().fg(Color::White),
    )]));
    lines.push(Line::from(""));

    for (path, size_kb, issue) in &state.oversized {
        let text = if issue.contains("TooLarge") {
            format!("• {} ({} KB, exceeds limit)", path, size_kb)
        } else if issue.contains("Binary") {
            format!("• {} (binary file)", path)
        } else {
            format!("• {} ({})", path, issue)
        };

        lines.push(Line::from(vec![Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )]));
    }

    lines.push(Line::from(""));

    // Options
    let inject_style = if state.selected == 0 {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Green)
    };
    let skip_style = if state.selected == 1 {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Yellow)
    };
    let abort_style = if state.selected == 2 {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(Color::Red)
    };

    lines.push(Line::from(vec![
        Span::styled("[I] ", Style::default().fg(Color::DarkGray)),
        Span::styled("Inject anyway", inject_style),
        Span::raw("    "),
        Span::styled("[S] ", Style::default().fg(Color::DarkGray)),
        Span::styled("Skip these", skip_style),
        Span::raw("    "),
        Span::styled("[Esc] ", Style::default().fg(Color::DarkGray)),
        Span::styled("Abort", abort_style),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  ↑↓ / I/S/Esc to select  ·  Enter to confirm",
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    )]));

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn file_injection_dialog_defaults_hidden() {
        let state = FileInjectionDialogState::new();
        assert!(!state.visible);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn file_injection_dialog_show_sets_visible() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![("file.txt".to_string(), 100, "TooLarge".to_string())]);
        assert!(state.visible);
        assert_eq!(state.selected, 0);
        assert!(!state.oversized.is_empty());
    }

    #[test]
    fn file_injection_dialog_navigate() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![]);
        assert_eq!(state.current_outcome(), FileInjectionOutcome::InjectAll);
        state.select_next();
        assert_eq!(state.current_outcome(), FileInjectionOutcome::SkipOversized);
        state.select_next();
        assert_eq!(state.current_outcome(), FileInjectionOutcome::Abort);
        state.select_prev();
        assert_eq!(state.current_outcome(), FileInjectionOutcome::SkipOversized);
    }

    #[test]
    fn file_injection_dialog_navigate_wraps() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![]);
        state.select_next(); // 0 → 1
        state.select_next(); // 1 → 2
        state.select_next(); // 2 → 0
        assert_eq!(state.current_outcome(), FileInjectionOutcome::InjectAll);
        state.select_prev(); // 0 → 2
        assert_eq!(state.current_outcome(), FileInjectionOutcome::Abort);
    }

    #[test]
    fn file_injection_dialog_confirm() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![]);
        state.select_next(); // Go to SkipOversized
        state.confirm();
        assert_eq!(state.outcome, Some(FileInjectionOutcome::SkipOversized));
    }

    #[test]
    fn file_injection_dialog_take_outcome() {
        let mut state = FileInjectionDialogState::new();
        state.show("test input".to_string(), vec![], vec![]);
        state.select_next();
        state.confirm();
        let (outcome, input, _) = state.take_outcome().unwrap();
        assert_eq!(outcome, FileInjectionOutcome::SkipOversized);
        assert_eq!(input, "test input");
        assert!(!state.visible);
        assert_eq!(state.outcome, None);
    }

    #[test]
    fn file_injection_dialog_renders_without_panic() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("large_file.rs".to_string(), 250, "TooLarge".to_string())],
        );
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_file_injection_dialog(frame, &state, area);
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(content.contains("Warning") || content.contains("File"));
    }

    #[test]
    fn file_injection_dialog_hidden_renders_nothing() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let state = FileInjectionDialogState::new(); // visible = false
        let before = terminal.backend().buffer().clone();
        terminal
            .draw(|frame| {
                render_file_injection_dialog(frame, &state, frame.area());
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer().content(), before.content());
    }
}
