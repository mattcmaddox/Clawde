// theme_screen.rs — Theme quick-pick overlay opened by /theme.
//
// Shows the built-in themes plus any custom themes saved in
// ~/.clawde/themes, with colour swatches. Arrow keys navigate (scrolls with
// a scrollbar when the list overflows), Enter selects, Esc cancels.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, render_scrollbar,
};
use crate::theme_colors::{current_palette, delete_theme, list_custom_themes, ColorPalette};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single theme option shown in the picker.
#[derive(Debug, Clone)]
pub struct ThemeOption {
    pub name: String,
    pub label: String,
    pub description: String,
    /// Whether this entry is a user-created custom theme (vs a built-in).
    pub custom: bool,
    /// A few representative colours used for the swatch preview.
    pub swatch: [Color; 4],
}

/// Maximum number of theme rows shown before the list scrolls.
const QUICK_PICK_VIEWPORT: usize = 12;

/// Actions the quick-pick can request from the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemePickAction {
    /// Apply the named theme (navigation preview or Enter confirm).
    Apply(String),
    /// Open the theme creator in new-theme mode.
    Create,
}

pub struct ThemeScreen {
    pub visible: bool,
    pub themes: Vec<ThemeOption>,
    pub selected_idx: usize,
    /// First theme index visible in the viewport (for the scrollbar).
    pub scroll_offset: usize,
    /// Delete confirmation pending for the selected custom theme.
    pub confirm_delete: bool,
    /// Transient status / error line shown in the footer.
    pub notice: Option<String>,
}

impl ThemeScreen {
    pub fn new() -> Self {
        Self {
            visible: false,
            themes: all_themes(),
            selected_idx: 0,
            scroll_offset: 0,
            confirm_delete: false,
            notice: None,
        }
    }

    pub fn open(&mut self, current_theme: &str) {
        self.visible = true;
        self.confirm_delete = false;
        self.notice = None;
        self.refresh();
        // Select the current theme, if found
        if let Some(idx) = self.themes.iter().position(|t| t.name == current_theme) {
            self.selected_idx = idx;
            self.ensure_visible();
        } else {
            self.selected_idx = 0;
        }
    }

    /// Re-scan the themes directory, clamping selection and scroll offset.
    pub fn refresh(&mut self) {
        self.themes = all_themes();
        if self.selected_idx >= self.themes.len() {
            self.selected_idx = self.themes.len().saturating_sub(1);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(self.themes.len().saturating_sub(QUICK_PICK_VIEWPORT));
    }

    /// Whether the currently selected entry is a user-created custom theme.
    fn selected_is_custom(&self) -> bool {
        self.themes.get(self.selected_idx).is_some_and(|t| t.custom)
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Keep the selected row within the scrolling viewport.
    fn ensure_visible(&mut self) {
        if self.selected_idx < self.scroll_offset {
            self.scroll_offset = self.selected_idx;
        } else if self.selected_idx >= self.scroll_offset + QUICK_PICK_VIEWPORT {
            self.scroll_offset = self.selected_idx - QUICK_PICK_VIEWPORT + 1;
        }
    }

    pub fn select_prev(&mut self) {
        let count = self.themes.len();
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
        let count = self.themes.len();
        if count == 0 {
            return;
        }
        self.selected_idx = (self.selected_idx + 1) % count;
        self.ensure_visible();
    }

    /// Return the name of the currently selected theme.
    pub fn selected_name(&self) -> Option<&str> {
        self.themes.get(self.selected_idx).map(|t| t.name.as_str())
    }
}

impl Default for ThemeScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in themes
// ---------------------------------------------------------------------------

/// The built-in theme list (also used by the theme creator).
pub fn builtin_themes() -> Vec<ThemeOption> {
    vec![
        ThemeOption {
            name: "default".to_string(),
            label: "Default".to_string(),
            description: "Clawde default — dark background, cyan accents".to_string(),
            custom: false,
            swatch: [Color::Black, Color::Cyan, Color::Green, Color::White],
        },
        ThemeOption {
            name: "dark".to_string(),
            label: "Dark".to_string(),
            description: "High-contrast dark theme".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(18, 18, 18),
                Color::Rgb(97, 175, 239),
                Color::Rgb(152, 195, 121),
                Color::Rgb(229, 229, 229),
            ],
        },
        ThemeOption {
            name: "light".to_string(),
            label: "Light".to_string(),
            description: "Light background with dark text".to_string(),
            custom: false,
            swatch: [Color::White, Color::Blue, Color::DarkGray, Color::Black],
        },
        ThemeOption {
            name: "solarized".to_string(),
            label: "Solarized".to_string(),
            description: "Solarized Dark — warm tones with blue accents".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(0, 43, 54),
                Color::Rgb(38, 139, 210),
                Color::Rgb(133, 153, 0),
                Color::Rgb(131, 148, 150),
            ],
        },
        ThemeOption {
            name: "nord".to_string(),
            label: "Nord".to_string(),
            description: "Nord — cool blue-grey palette".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(46, 52, 64),
                Color::Rgb(136, 192, 208),
                Color::Rgb(163, 190, 140),
                Color::Rgb(216, 222, 233),
            ],
        },
        ThemeOption {
            name: "dracula".to_string(),
            label: "Dracula".to_string(),
            description: "Dracula — purple/pink dark theme".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(40, 42, 54),
                Color::Rgb(139, 233, 253),
                Color::Rgb(80, 250, 123),
                Color::Rgb(248, 248, 242),
            ],
        },
        ThemeOption {
            name: "monokai".to_string(),
            label: "Monokai".to_string(),
            description: "Monokai — vibrant colours on dark background".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(39, 40, 34),
                Color::Rgb(102, 217, 239),
                Color::Rgb(166, 226, 46),
                Color::Rgb(248, 248, 242),
            ],
        },
        ThemeOption {
            name: "deuteranopia".to_string(),
            label: "Deuteranopia".to_string(),
            description: "Red-green color blind friendly — blue/yellow/gray palette".to_string(),
            custom: false,
            swatch: [
                Color::Rgb(18, 18, 18),
                Color::Rgb(0, 122, 204),   // Blue
                Color::Rgb(255, 180, 0),   // Gold/Yellow
                Color::Rgb(200, 200, 200), // Light gray
            ],
        },
    ]
}

/// Built-in themes followed by custom themes loaded from ~/.clawde/themes.
/// Custom names that collide with a built-in are skipped so the list never
/// shows the same theme twice (in-app saves block builtin names, but a
/// hand-placed file could still match one).
fn all_themes() -> Vec<ThemeOption> {
    let mut themes = builtin_themes();
    for name in list_custom_themes() {
        if themes.iter().any(|t| t.name == name) {
            continue;
        }
        let pal = ColorPalette::for_theme(&name);
        themes.push(ThemeOption {
            name: name.clone(),
            label: name.clone(),
            description: format!("Custom theme — ~/.clawde/themes/{}.json", name),
            custom: true,
            swatch: [pal.panel_bg, pal.accent, pal.success, pal.text_light],
        });
    }
    themes
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the theme quick-pick overlay into `frame`.
pub fn render_theme_screen(frame: &mut Frame, screen: &ThemeScreen, area: Rect) {
    if !screen.visible {
        return;
    }

    let p = current_palette();
    // Cap the dialog to the scrolling viewport; each entry renders as two
    // lines (row + blank), plus header/footer margins.
    let shown = screen.themes.len().min(QUICK_PICK_VIEWPORT);
    let rows = ((shown as u16 * 2) + 2).min(area.height.saturating_sub(6));
    let layout = begin_modal_frame(frame, area, 70, rows + 6, 2, 1);
    render_modal_title_frame(frame, layout.header_area, "Choose a theme", "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        let subtitle = if screen.themes.len() > QUICK_PICK_VIEWPORT {
            format!(
                " {} themes — scrollable list (hjkl to move)",
                screen.themes.len()
            )
        } else {
            format!(" {} themes", screen.themes.len())
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
    let start = screen.scroll_offset;
    let end = (start + QUICK_PICK_VIEWPORT).min(screen.themes.len());
    for (i, theme) in screen.themes[start..end].iter().enumerate() {
        let real_i = start + i;
        let is_selected = real_i == screen.selected_idx;
        let bg = if is_selected { p.accent } else { p.panel_bg };
        let fg = if is_selected {
            Color::White
        } else {
            p.text_light
        };
        let desc_fg = if is_selected {
            Color::Rgb(248, 220, 236)
        } else {
            p.disabled
        };

        // Build the swatch using block characters with background colour
        let swatch_spans: Vec<Span> = theme
            .swatch
            .iter()
            .map(|&c| Span::styled("  ", Style::default().bg(c)))
            .collect();

        let badge = if theme.custom { " (custom)" } else { "" };
        let mut row_spans: Vec<Span> = Vec::new();
        row_spans.push(Span::styled(" ", Style::default().bg(bg)));
        row_spans.extend(swatch_spans);
        row_spans.push(Span::styled("  ", Style::default().bg(bg)));
        row_spans.push(Span::styled(
            format!("{:<12}{}", theme.label, badge),
            Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
        ));
        row_spans.push(Span::styled(
            theme.description.clone(),
            Style::default().fg(desc_fg).bg(bg),
        ));
        // Reserve one column for the scrollbar when the list overflows.
        let bar_col = if screen.themes.len() > QUICK_PICK_VIEWPORT {
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

    // Scrollbar on the right edge when the list overflows the viewport.
    if screen.themes.len() > QUICK_PICK_VIEWPORT {
        render_scrollbar(
            frame,
            &p,
            layout.body_area,
            screen.scroll_offset,
            screen.themes.len(),
            QUICK_PICK_VIEWPORT,
        );
    }
    // Footer: transient notice (delete confirm / result) or the action hints.
    let (footer_text, footer_style) = if let Some(notice) = &screen.notice {
        let style = if screen.confirm_delete {
            Style::default().fg(p.warning).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(p.disabled)
                .add_modifier(Modifier::ITALIC)
        };
        (notice.clone(), style)
    } else {
        (
            " j/k/h/l navigate · enter apply · n create · d delete · esc close".to_string(),
            Style::default()
                .fg(p.disabled)
                .add_modifier(Modifier::ITALIC),
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(footer_text, footer_style)])),
        layout.footer_area,
    );
}

// ---------------------------------------------------------------------------
// Key handling helpers (called from app.rs)
// ---------------------------------------------------------------------------

/// Returns the action the user took, `None` otherwise. Call this from the
/// app's key handler when `theme_screen.visible`.
pub fn handle_theme_key(
    screen: &mut ThemeScreen,
    key: crossterm::event::KeyEvent,
) -> Option<ThemePickAction> {
    use crossterm::event::KeyCode;

    if !screen.visible {
        return None;
    }

    match key.code {
        KeyCode::Esc => {
            screen.close();
            None
        }
        KeyCode::Enter => {
            let name = screen.selected_name().map(String::from);
            screen.close();
            name.map(ThemePickAction::Apply)
        }
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('h') => {
            screen.select_prev();
            // Navigation resets any pending delete confirmation so the next
            // 'd' targets the newly selected theme, not the old one.
            screen.confirm_delete = false;
            screen.notice = None;
            screen
                .selected_name()
                .map(|n| ThemePickAction::Apply(n.to_string()))
        }
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('l') => {
            screen.select_next();
            // See note above: reset delete confirmation on navigation.
            screen.confirm_delete = false;
            screen.notice = None;
            screen
                .selected_name()
                .map(|n| ThemePickAction::Apply(n.to_string()))
        }
        KeyCode::Char('n') => {
            screen.close();
            Some(ThemePickAction::Create)
        }
        KeyCode::Char('d') | KeyCode::Char('y') if screen.confirm_delete => {
            screen.confirm_delete = false;
            if let Some(name) = screen.selected_name() {
                if screen.selected_is_custom() {
                    let _ = delete_theme(name);
                    screen.notice = Some(format!("Deleted '{}'.", name));
                    screen.refresh();
                }
            }
            None
        }
        KeyCode::Char('d') => {
            if screen.selected_is_custom() {
                screen.confirm_delete = true;
                screen.notice = Some(format!(
                    "Press d again to delete '{}' (esc to cancel).",
                    screen.selected_name().unwrap_or("")
                ));
            } else {
                screen.notice = Some("Only custom themes can be deleted.".into());
            }
            None
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn theme_screen_renders_current_theme() {
        let mut screen = ThemeScreen::new();
        screen.open("dark");

        let backend = TestBackend::new(90, 28);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render_theme_screen(frame, &screen, frame.area()))
            .unwrap();

        let rendered = terminal.backend().buffer();
        let content = rendered
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(content.contains("Choose a theme"));
        assert!(content.contains("Dark"));
    }

    #[test]
    fn theme_navigation_wraps() {
        let mut screen = ThemeScreen::new();
        screen.open("default");
        assert_eq!(screen.selected_idx, 0, "default is the first built-in");

        // Prev from the first entry wraps to the last entry.
        screen.select_prev();
        assert_eq!(screen.selected_idx, screen.themes.len() - 1);

        // Next from the last entry wraps back to the first.
        screen.select_next();
        assert_eq!(screen.selected_idx, 0);
    }

    #[test]
    fn quick_pick_includes_custom_themes() {
        // With no custom themes on disk the list still contains all built-ins
        // and nothing is marked custom.
        let screen = ThemeScreen::new();
        assert!(screen.themes.iter().any(|t| t.name == "default"));
        assert!(screen.themes.iter().any(|t| !t.custom));
        // Custom entries (when present) sort after built-ins.
        let first_custom = screen.themes.iter().position(|t| t.custom);
        if let Some(idx) = first_custom {
            assert!(screen.themes[..idx].iter().all(|t| !t.custom));
        }
    }

    #[test]
    fn quick_pick_scrolls_when_overflown() {
        let mut screen = ThemeScreen::new();
        screen.open("default");
        for i in 0..(QUICK_PICK_VIEWPORT + 5) {
            screen.themes.push(ThemeOption {
                name: format!("custom_{}", i),
                label: format!("custom_{}", i),
                description: "custom".into(),
                custom: true,
                swatch: [Color::Reset; 4],
            });
        }
        // Selecting far down scrolls the viewport down.
        screen.selected_idx = screen.themes.len() - 1;
        screen.scroll_offset = 0;
        screen.ensure_visible();
        assert_eq!(
            screen.scroll_offset,
            screen.themes.len() - QUICK_PICK_VIEWPORT
        );
        assert!(screen.selected_idx < screen.scroll_offset + QUICK_PICK_VIEWPORT);
        // Selecting far up scrolls the viewport back to the top.
        screen.selected_idx = 0;
        screen.scroll_offset = screen.themes.len() - QUICK_PICK_VIEWPORT;
        screen.ensure_visible();
        assert_eq!(screen.scroll_offset, 0);
    }

    #[test]
    fn handle_theme_key_navigates_with_jk_and_applies() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut screen = ThemeScreen::new();
        screen.open("default");
        let first = screen.selected_name().map(String::from);

        // 'k' should return Apply(prev) so the theme is applied live.
        let up = KeyEvent::new(KeyCode::Char('k'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, up);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        // The previewed theme differs from the starting selection (wrap).
        assert_ne!(action, Some(ThemePickAction::Apply(first.clone().unwrap())));
        assert!(screen.visible, "Picker should stay open after navigation");

        // 'j' should also return Apply(name) and stay open.
        let down = KeyEvent::new(KeyCode::Char('j'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, down);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        assert!(screen.visible, "Picker should stay open after navigation");

        // Enter returns Apply and closes.
        let enter = KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, enter);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        assert!(!screen.visible, "Picker should close after Enter");

        // Esc returns None and closes.
        screen.open("default");
        let esc = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, esc);
        assert!(action.is_none(), "Esc should return None");
        assert!(!screen.visible, "Picker should close after Esc");
    }

    #[test]
    fn handle_theme_key_navigates_with_hl() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut screen = ThemeScreen::new();
        screen.open("default");
        let first = screen.selected_name().map(String::from);

        // 'h' navigates to the previous theme (live preview, stays open).
        let h = KeyEvent::new(KeyCode::Char('h'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, h);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        assert_ne!(action, Some(ThemePickAction::Apply(first.clone().unwrap())));
        assert!(screen.visible, "Picker should stay open after 'h'");

        // 'l' navigates back to the next theme (wraps to the first).
        let l = KeyEvent::new(KeyCode::Char('l'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, l);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        assert_eq!(action, Some(ThemePickAction::Apply(first.unwrap())));
        assert!(screen.visible, "Picker should stay open after 'l'");
    }

    #[test]
    fn handle_theme_key_n_requests_create() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut screen = ThemeScreen::new();
        screen.open("default");
        let action = handle_theme_key(
            &mut screen,
            KeyEvent::new(KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE),
        );
        assert_eq!(action, Some(ThemePickAction::Create));
        assert!(
            !screen.visible,
            "Picker should close before the creator opens"
        );
    }

    #[test]
    fn handle_theme_key_delete_requires_confirm_and_resets_on_navigate() {
        use crossterm::event::{KeyCode, KeyEvent};
        let mut screen = ThemeScreen::new();
        screen.open("default");
        // Add a fake custom entry that maps to a non-existent file; deleting
        // a missing file is a silent no-op via delete_theme.
        screen.themes.push(ThemeOption {
            name: "does_not_exist_custom".into(),
            label: "does_not_exist_custom".into(),
            description: "custom".into(),
            custom: true,
            swatch: [Color::Reset; 4],
        });
        screen.selected_idx = screen.themes.len() - 1;

        // First 'd' arms the confirmation but does not delete.
        let d = KeyEvent::new(KeyCode::Char('d'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, d);
        assert!(action.is_none());
        assert!(screen.confirm_delete);
        assert!(screen.notice.is_some());

        // Navigating resets the confirmation (so a later 'd' targets the new
        // selection instead of deleting the old one).
        let j = KeyEvent::new(KeyCode::Char('j'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, j);
        assert!(matches!(action, Some(ThemePickAction::Apply(_))));
        assert!(!screen.confirm_delete);
        assert!(screen.notice.is_none());

        // Re-arm and confirm with 'y' — the entry leaves the list.
        screen.selected_idx = screen.themes.len() - 1;
        handle_theme_key(&mut screen, d);
        let y = KeyEvent::new(KeyCode::Char('y'), crossterm::event::KeyModifiers::NONE);
        let action = handle_theme_key(&mut screen, y);
        assert!(action.is_none());
        assert!(!screen.confirm_delete);
        assert!(!screen
            .themes
            .iter()
            .any(|t| t.name == "does_not_exist_custom"));

        // 'd' on a built-in theme just shows a notice.
        screen.selected_idx = 0;
        let action = handle_theme_key(&mut screen, d);
        assert!(action.is_none());
        assert!(!screen.confirm_delete);
        assert!(screen.notice.is_some());
    }
}
