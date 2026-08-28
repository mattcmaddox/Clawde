// mode_panel.rs — Mode quick-pick overlay opened by bare `/mode`.
//
// Shows the built-in mode presets plus any custom modes defined in
// ~/.clawde/modes/ or .clawde/modes/, marking the active one. Arrow keys
// navigate (scrolls with a scrollbar when the list overflows), Enter applies
// the mode, Esc cancels. Mirrors the theme quick-pick overlay informally;
// mode application itself reuses clawde_core::modes::apply_mode so the picker
// and the /mode command can never diverge on what a preset binds.

use std::path::Path;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, render_scrollbar,
};
use crate::theme_colors::current_palette;

/// A single mode preset shown in the picker.
#[derive(Debug, Clone)]
pub struct ModeOption {
    pub name: String,
    pub label: String,
    pub description: String,
    /// Whether this entry is a user-defined custom mode (vs a built-in).
    pub custom: bool,
    /// Whether this is the currently active session mode.
    pub active: bool,
}

/// Maximum number of mode rows shown before the list scrolls.
pub const MODE_PICK_VIEWPORT: usize = 12;

/// Actions the quick-pick can request from the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModePickAction {
    /// Apply the named mode preset.
    Apply(String),
}

pub struct ModePanel {
    pub visible: bool,
    pub modes: Vec<ModeOption>,
    pub selected_idx: usize,
    /// First mode index visible in the viewport (for the scrollbar).
    pub scroll_offset: usize,
}

impl ModePanel {
    pub fn new() -> Self {
        Self {
            visible: false,
            modes: Vec::new(),
            selected_idx: 0,
            scroll_offset: 0,
        }
    }

    /// Open the picker with the resolved mode presets for the session.
    /// `current_mode` is `config.mode` (fall back to `"default"`).
    pub fn open(&mut self, current_mode: &str, global_dir: &Path, project_dir: &Path) {
        let defs = clawde_core::modes::all_modes_for_project(global_dir, project_dir);
        // A mode is "custom" when it is not one of the built-in presets that
        // ship with the binary (i.e. it came from ~/.clawde/modes or
        // .clawde/modes).
        let builtin_names: std::collections::HashSet<String> = clawde_core::modes::builtin_modes()
            .into_iter()
            .map(|m| m.name)
            .collect();
        self.modes = defs
            .into_iter()
            .map(|m| ModeOption {
                active: m.name == current_mode,
                custom: !builtin_names.contains(&m.name),
                name: m.name.clone(),
                label: if m.label.is_empty() {
                    m.name.clone()
                } else {
                    m.label.clone()
                },
                description: m.description.clone(),
            })
            .collect();
        // Select the active mode when present; otherwise start at the top.
        self.selected_idx = self.modes.iter().position(|m| m.active).unwrap_or(0);
        self.scroll_offset = 0;
        self.ensure_visible();
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Keep the selected row within the scrolling viewport.
    fn ensure_visible(&mut self) {
        if self.selected_idx < self.scroll_offset {
            self.scroll_offset = self.selected_idx;
        } else if self.selected_idx >= self.scroll_offset + MODE_PICK_VIEWPORT {
            self.scroll_offset = self.selected_idx - MODE_PICK_VIEWPORT + 1;
        }
    }

    pub fn select_prev(&mut self) {
        let count = self.modes.len();
        if count == 0 {
            return;
        }
        if self.selected_idx == 0 {
            self.selected_idx = count - 1;
        } else {
            self.selected_idx -= 1;
        }
        self.ensure_visible();
    }

    pub fn select_next(&mut self) {
        let count = self.modes.len();
        if count == 0 {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
        self.ensure_visible();
    }

    /// Return the name of the currently selected mode.
    pub fn selected_name(&self) -> Option<&str> {
        self.modes.get(self.selected_idx).map(|m| m.name.as_str())
    }
}

impl Default for ModePanel {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle a key while the mode picker is visible. Returns `Some(action)` when
/// the key confirms a selection (Enter).
pub fn handle_mode_key(
    panel: &mut ModePanel,
    key: crossterm::event::KeyEvent,
) -> Option<ModePickAction> {
    use crossterm::event::KeyCode;

    if !panel.visible {
        return None;
    }

    match key.code {
        KeyCode::Esc => {
            panel.close();
            None
        }
        KeyCode::Enter => {
            let name = panel.selected_name().map(String::from);
            panel.close();
            name.map(ModePickAction::Apply)
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('h') => {
            panel.select_prev();
            None
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => {
            panel.select_next();
            None
        }
        _ => None,
    }
}

/// Render the mode quick-pick overlay.
pub fn render_mode_panel(frame: &mut Frame, panel: &ModePanel, area: Rect) {
    if !panel.visible {
        return;
    }

    let p = current_palette();
    let shown = panel.modes.len().min(MODE_PICK_VIEWPORT);
    let rows = ((shown as u16 * 2) + 2).min(area.height.saturating_sub(6));
    let layout = begin_modal_frame(frame, area, 86, rows + 6, 2, 1);
    render_modal_title_frame(frame, layout.header_area, "Choose a mode", "esc");

    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        let subtitle = if panel.modes.len() > MODE_PICK_VIEWPORT {
            format!(
                " {} presets — scrollable list (hjkl/arrows to move)",
                panel.modes.len()
            )
        } else {
            format!(" {} presets", panel.modes.len())
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                subtitle,
                Style::default().fg(p.disabled),
            )])),
            subtitle_area,
        );
    }

    let mut lines: Vec<Line> = Vec::new();
    let start = panel.scroll_offset;
    let end = (start + MODE_PICK_VIEWPORT).min(panel.modes.len());
    for (i, mode) in panel.modes[start..end].iter().enumerate() {
        let real_i = start + i;
        let is_selected = real_i == panel.selected_idx;
        let bg = if is_selected { p.accent } else { p.panel_bg };
        let fg = if is_selected {
            Color::White
        } else {
            p.text_light
        };
        let desc_fg = if is_selected {
            Color::Rgb(232, 235, 244)
        } else {
            p.disabled
        };

        let mut row_spans: Vec<Span> = Vec::new();
        row_spans.push(Span::styled(" ", Style::default().bg(bg)));
        let active_marker = if mode.active { "● " } else { "  " };
        let badge = if mode.custom { " (custom)" } else { "" };
        row_spans.push(Span::styled(
            format!("{active_marker}{:<14}{}", mode.label, badge),
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
        row_spans.push(Span::styled(
            mode.description.clone(),
            Style::default().fg(desc_fg).bg(bg),
        ));
        let bar_col = if panel.modes.len() > MODE_PICK_VIEWPORT {
            1
        } else {
            0
        };
        let used: usize = row_spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let pad = layout
            .body_area
            .width
            .saturating_sub(used as u16)
            .saturating_sub(bar_col as u16) as usize;
        if pad > 0 {
            row_spans.push(Span::styled(" ".repeat(pad), Style::default().bg(bg)));
        }

        lines.push(Line::from(row_spans));
        lines.push(Line::from(""));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(p.panel_bg)),
        layout.body_area,
    );

    if panel.modes.len() > MODE_PICK_VIEWPORT {
        render_scrollbar(
            frame,
            &p,
            layout.body_area,
            panel.scroll_offset,
            panel.modes.len(),
            MODE_PICK_VIEWPORT,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn panel_selects_active_mode_on_open() {
        let ws = temp_project_with_modes();
        let mut panel = ModePanel::new();
        panel.open("fast", &ws.global, &ws.project);
        assert!(panel.visible);
        assert!(
            panel
                .modes
                .iter()
                .find(|m| m.name == "fast")
                .map(|m| m.active)
                .unwrap_or(false),
            "active mode marked"
        );
        assert!(
            panel.modes.iter().len() >= 4,
            "built-in default/careful/fast/walkaway present"
        );
        clean(&ws);
    }

    #[test]
    fn navigation_wraps_and_enter_returns_name() {
        let ws = temp_project_with_modes();
        let mut panel = ModePanel::new();
        panel.open("default", &ws.global, &ws.project);
        let n = panel.modes.len();
        assert!(n >= 4);
        panel.select_prev(); // wrap to last
        assert_eq!(panel.selected_idx, n - 1);
        panel.select_next(); // wrap back to first
        assert_eq!(panel.selected_idx, 0);

        let name = panel
            .selected_name()
            .expect("a selection to exist")
            .to_string();
        let action = handle_mode_key(&mut panel, key(KeyCode::Enter));
        assert!(!panel.visible, "enter closes the panel");
        assert_eq!(action, Some(ModePickAction::Apply(name)));
        clean(&ws);
    }

    #[test]
    fn esc_closes_without_action() {
        let ws = temp_project_with_modes();
        let mut panel = ModePanel::new();
        panel.open("default", &ws.global, &ws.project);
        let action = handle_mode_key(&mut panel, key(KeyCode::Esc));
        assert!(!panel.visible);
        assert_eq!(action, None);
        clean(&ws);
    }

    #[test]
    fn panel_renders() {
        let ws = temp_project_with_modes();
        let mut panel = ModePanel::new();
        panel.open("fast", &ws.global, &ws.project);
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_mode_panel(frame, &panel, frame.area()))
            .unwrap();
        let content = terminal.backend().buffer();
        let joined: String = content
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(joined.contains("Choose a mode"), "title rendered");
        assert!(joined.contains("Fast"), "mode label listed");
        clean(&ws);
    }

    // ---- helpers ------------------------------------------------------

    struct Layout {
        global: std::path::PathBuf,
        project: std::path::PathBuf,
    }

    fn temp_project_with_modes() -> Layout {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("clawde-modepanel-{}-{stamp}", std::process::id()));
        let global = root.join("global");
        let project = root.join("project");
        let _ = std::fs::create_dir_all(project.join(".clawde").join("modes"));
        // A custom project mode to exercise the custom flag.
        std::fs::write(
            project.join(".clawde").join("modes").join("review.json"),
            r#"{"name":"review","label":"Review","description":"My project review mode"}"#,
        )
        .unwrap();
        Layout { global, project }
    }

    fn clean(layout: &Layout) {
        let _ = std::fs::remove_dir_all(layout.project.parent().unwrap());
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyEvent, KeyModifiers};
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}
