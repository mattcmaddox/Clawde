// spec_review.rs — Spec review dialog (`/spec-review <file>`), audit spec §10.
//
// The user-facing half of Spec-Driven Development mode: after a spec has been
// generated (`/spec <task>` writes `specs/<title>.json`), this dialog shows
// the structured spec — requirements, file plan, data models, acceptance
// tests, edge cases — and asks the user to Accept, Edit, or Reject it
// (§10.2). Accepting queues an implementation turn against the spec;
// Edit opens the JSON in the external editor; Reject discards it.
//
// Layout (centered modal):
//
//     ┌─ Spec Review ─────────────────────────────── esc ┐
//     │  # Rate-Limiting Middleware                      │
//     │                                                  │
//     │  ## Requirements                                 │
//     │   1. Per-IP rate limiting with configurable …    │
//     │   2. Integrates with the tower::Service stack    │
//     │                                                  │
//     │  ## Files to Touch                               │
//     │   [NEW]    crates/api/src/middleware/rate_limit… │
//     │                                                  │
//     │  ## Acceptance Tests                             │
//     │   1. Requests under limit pass through           │
//     │   2. Requests over limit return 429              │
//     │                                                  │
//     │  ↑/↓ j/k scroll · ←/→ h/l action                │
//     │  [ Accept ]   [ Edit Spec ]   [ Reject ]         │
//     └──────────────────────────────────────────────────┘
//
// Rendered content is scrollable (Up/Down + vim j/k); the action row moves
// with Left/Right (+ vim h/l) and activates with Enter.

use std::cell::Cell;
use std::path::PathBuf;

use clawde_core::spec::{FileAction, Spec};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::overlays::centered_rect;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Border accent — teal, distinct from the routing dialog's violet and the
/// clawde-pink used elsewhere.
const BORDER: Color = Color::Rgb(90, 180, 200);
/// Selected action button background.
const SEL_BG: Color = Color::Rgb(40, 90, 100);
/// Highlight text on the selected action.
const SEL_FG: Color = Color::Rgb(238, 238, 240);
/// Dim text (labels, hints).
const DIM: Color = Color::Rgb(120, 120, 132);
/// Section headers inside the content.
const HEADER: Color = Color::Rgb(150, 200, 210);
/// File-action tags: NEW / MODIFY / DELETE.
const TAG_NEW: Color = Color::Rgb(110, 200, 140);
const TAG_MOD: Color = Color::Rgb(230, 190, 110);
const TAG_DEL: Color = Color::Rgb(230, 120, 120);
/// Body text.
const BODY: Color = Color::Rgb(200, 200, 205);

/// Which of the three bottom actions is selected.
pub const ACTION_ACCEPT: usize = 0;
pub const ACTION_EDIT: usize = 1;
pub const ACTION_REJECT: usize = 2;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Interactive state for the spec review dialog.
#[derive(Debug)]
pub struct SpecReviewState {
    pub visible: bool,
    /// Area used by the modal in the last render (click-outside detection).
    pub last_rect: Cell<Rect>,
    /// The spec under review.
    pub spec: Option<Spec>,
    /// The JSON file the spec was loaded from (used by Edit / Accept).
    pub path: Option<PathBuf>,
    /// Index of the selected action (Accept / Edit Spec / Reject).
    pub selected_action: usize,
    /// Scroll offset into the rendered content lines.
    pub scroll: usize,
    /// True while the dialog is in multi-spec picker mode (`/spec-review`
    /// with several specs in `specs/`). While picking, Enter opens the
    /// highlighted spec; Esc leaves the dialog.
    pub picking: bool,
    /// Parseable specs in the working dir, newest-first (picker mode).
    pub available: Vec<PathBuf>,
    /// Highlighted index into `available` (picker mode).
    pub picked: usize,
}

impl Default for SpecReviewState {
    fn default() -> Self {
        Self {
            visible: false,
            last_rect: Cell::new(Rect::default()),
            spec: None,
            path: None,
            selected_action: ACTION_ACCEPT,
            scroll: 0,
            picking: false,
            available: Vec::new(),
            picked: 0,
        }
    }
}

impl SpecReviewState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the dialog for a spec JSON file. Returns `Err(message)` when the
    /// file cannot be read or parsed, leaving the dialog closed.
    pub fn open(&mut self, path: PathBuf) -> Result<(), String> {
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
        let spec = Spec::parse_json(&raw)?;
        self.spec = Some(spec);
        self.path = Some(path);
        self.selected_action = ACTION_ACCEPT;
        self.scroll = 0;
        self.picking = false;
        self.available.clear();
        self.picked = 0;
        self.visible = true;
        Ok(())
    }

    /// Open the most recently modified spec in `dir/specs/`. With several
    /// specs present, enters picker mode (newest-first) so the user chooses;
    /// with exactly one, opens it directly; with none, returns an error.
    pub fn open_latest(&mut self, dir: &std::path::Path) -> Result<(), String> {
        let mut specs = Spec::list_specs(dir);
        match specs.len() {
            0 => Err("No spec found — run /spec <task> to generate one first.".to_string()),
            1 => {
                let path = specs.remove(0);
                self.open(path)
            }
            _ => {
                // Picker mode: show the candidates, newest first.
                self.available = specs;
                self.picked = 0;
                self.picking = true;
                self.spec = None;
                self.path = None;
                self.selected_action = ACTION_ACCEPT;
                self.scroll = 0;
                self.visible = true;
                Ok(())
            }
        }
    }

    /// Confirm the highlighted entry in picker mode, opening that spec.
    /// Returns `Some(message)` when the spec can no longer be read/parsed
    /// (e.g. deleted between the scan and the open), leaving the dialog
    /// closed instead of a blank zombie modal.
    pub fn confirm_pick(&mut self) -> Option<String> {
        if !self.picking {
            return None;
        }
        let path = self.available.get(self.picked).cloned()?;
        self.picking = false;
        self.available.clear();
        match self.open(path) {
            Ok(()) => None,
            Err(msg) => {
                self.visible = false;
                Some(msg)
            }
        }
    }

    /// Move the picker highlight up (clamped at the first entry).
    pub fn pick_prev(&mut self) {
        if self.picking {
            self.picked = self.picked.saturating_sub(1);
        }
    }

    /// Move the picker highlight down (clamped at the last entry).
    pub fn pick_next(&mut self) {
        if self.picking {
            let max = self.available.len().saturating_sub(1);
            self.picked = (self.picked + 1).min(max);
        }
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.picking = false;
        self.available.clear();
        self.picked = 0;
    }

    /// Move the action selection left/right (clamped).
    pub fn select_prev(&mut self) {
        self.selected_action = self.selected_action.saturating_sub(1);
    }

    pub fn select_next(&mut self) {
        self.selected_action = (self.selected_action + 1).min(ACTION_REJECT);
    }

    /// Scroll the content up (toward the top).
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub fn scroll_down(&mut self, content_lines: usize, visible: usize) {
        let max = content_lines.saturating_sub(visible);
        self.scroll = (self.scroll + 1).min(max);
    }

    /// The accepted-spec implementation message queued when the user presses
    /// Enter on Accept: tells the model to implement against the spec and run
    /// its acceptance tests.
    pub fn accept_message(&self) -> Option<String> {
        let spec = self.spec.as_ref()?;
        let path = self.path.as_ref()?;
        let criteria = spec
            .acceptance_tests
            .iter()
            .map(|t| t.description.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let mut msg = format!(
            "The spec \"{}\" (saved at {}) has been ACCEPTED. Implement it:\n\n{}",
            spec.title,
            path.display(),
            spec.to_json()
        );
        if !criteria.is_empty() {
            msg.push_str(&format!(
                "\n\nRun and pass every acceptance test:\n{criteria}"
            ));
        }
        Some(msg)
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Number of content lines a spec renders to (scroll bounds).
///
/// Public so app.rs can bound `scroll_down` without building the styled lines.
pub fn spec_content_line_count(spec: &Spec) -> usize {
    let mut count = 2; // title + blank
    if !spec.requirements.is_empty() {
        count += 2 + spec.requirements.len(); // header + blank + rows
    }
    if !spec.files_to_touch.is_empty() {
        count += 2; // header + blank
        for f in &spec.files_to_touch {
            count += 1;
            if !f.description.is_empty() {
                count += 1;
            }
        }
    }
    if !spec.data_models.is_empty() {
        count += 2 + spec.data_models.len();
    }
    if !spec.acceptance_tests.is_empty() {
        count += 2 + spec.acceptance_tests.len();
    }
    if !spec.edge_cases.is_empty() {
        count += 2 + spec.edge_cases.len();
    }
    count
}

/// Build the scrollable content lines for the spec (title + sections).
fn content_lines(spec: &Spec) -> Vec<Line<'static>> {
    use clawde_core::spec::AcceptanceTest;
    let mut lines: Vec<Line<'static>> = Vec::new();

    let title_style = Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![Span::styled(
        format!("# {}", spec.title),
        title_style,
    )]));
    lines.push(Line::from(""));

    if !spec.requirements.is_empty() {
        lines.push(Line::from(Span::styled(
            "## Requirements",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )));
        for (i, req) in spec.requirements.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!("  {}. {req}", i + 1),
                Style::default().fg(BODY),
            )));
        }
        lines.push(Line::from(""));
    }

    if !spec.files_to_touch.is_empty() {
        lines.push(Line::from(Span::styled(
            "## Files to Touch",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )));
        for f in &spec.files_to_touch {
            let (tag, color) = match f.action {
                FileAction::Create => ("NEW", TAG_NEW),
                FileAction::Modify => ("MODIFY", TAG_MOD),
                FileAction::Delete => ("DELETE", TAG_DEL),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  [{tag}] "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(f.path.clone(), Style::default().fg(BODY)),
            ]));
            if !f.description.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("           {desc}", desc = f.description),
                    Style::default().fg(DIM),
                )));
            }
        }
        lines.push(Line::from(""));
    }

    if !spec.data_models.is_empty() {
        lines.push(Line::from(Span::styled(
            "## Data Models",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )));
        for d in &spec.data_models {
            lines.push(Line::from(Span::styled(
                format!("  `{}` — {}", d.name, d.definition),
                Style::default().fg(BODY),
            )));
        }
        lines.push(Line::from(""));
    }

    if !spec.acceptance_tests.is_empty() {
        lines.push(Line::from(Span::styled(
            "## Acceptance Tests",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )));
        let tests: Vec<&AcceptanceTest> = spec.acceptance_tests.iter().collect();
        for (i, t) in tests.iter().enumerate() {
            lines.push(Line::from(Span::styled(
                format!("  {}. {}", i + 1, t.description),
                Style::default().fg(BODY),
            )));
        }
        lines.push(Line::from(""));
    }

    if !spec.edge_cases.is_empty() {
        lines.push(Line::from(Span::styled(
            "## Edge Cases",
            Style::default().fg(HEADER).add_modifier(Modifier::BOLD),
        )));
        for e in &spec.edge_cases {
            lines.push(Line::from(Span::styled(
                format!("  - {e}"),
                Style::default().fg(BODY),
            )));
        }
        lines.push(Line::from(""));
    }

    lines
}

/// Render the multi-spec picker as a centered modal (entered by
/// `/spec-review` when several specs exist in `specs/`).
pub fn render_spec_picker(frame: &mut Frame, state: &SpecReviewState, size: Rect) {
    let width = 92.min(size.width.saturating_sub(2));
    let height = 26.min(size.height.saturating_sub(2));
    let area = centered_rect(width, height, size);
    state.last_rect.set(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " Choose Spec ",
            Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left);
    frame.render_widget(Clear, area);
    frame.render_widget(block.clone(), area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(5), // content rows; 4 for hint
    };
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let buf = frame.buffer_mut();
    let visible = inner.height as usize;
    let count = state.available.len();
    // Keep the highlighted entry inside the visible window when the list
    // overflows the modal: scroll the window with the highlight.
    let scroll = state
        .picked
        .saturating_sub(visible)
        .min(count.saturating_sub(visible));
    let shown = count.min(visible);
    let mut row = inner.y;
    for i in 0..shown {
        let path = &state.available[scroll + i];
        let selected = i == state.picked;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        let name_style = if selected {
            Style::default()
                .fg(SEL_FG)
                .bg(SEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(BODY)
        };
        let mut x = inner.x;
        if selected {
            for (j, ch) in "\u{203a} ".chars().enumerate() {
                if x + j as u16 >= inner.x + inner.width {
                    break;
                }
                buf[(x + j as u16, row)]
                    .set_symbol(&ch.to_string())
                    .set_style(name_style);
            }
            x += 2;
        }
        for (j, ch) in name.chars().enumerate() {
            let bx = x + j as u16;
            if bx >= inner.x + inner.width {
                break;
            }
            buf[(bx, row)]
                .set_symbol(&ch.to_string())
                .set_style(name_style);
        }
        let dim_style = if selected {
            Style::default().fg(SEL_FG).bg(SEL_BG)
        } else {
            Style::default().fg(DIM)
        };
        let dx = x + name.chars().count() as u16 + 2;
        let rel = path.display().to_string();
        for (j, ch) in rel.chars().enumerate() {
            let bx = dx + j as u16;
            if bx >= inner.x + inner.width {
                break;
            }
            buf[(bx, row)]
                .set_symbol(&ch.to_string())
                .set_style(dim_style);
        }
        row += 1;
    }

    let hint_y = area.y + area.height - 2;
    let hint = "\u{2191}/\u{2193} j/k navigate \u{b7} enter open \u{b7} esc";
    for (i, ch) in hint.chars().enumerate() {
        let bx = area.x + i as u16;
        if bx >= area.x + area.width - 1 {
            break;
        }
        buf[(bx, hint_y)]
            .set_symbol(&ch.to_string())
            .set_style(Style::default().fg(DIM));
    }
}

/// Render the spec review dialog as a centered modal.
pub fn render_spec_review(
    frame: &mut Frame,
    state: &SpecReviewState,
    _vim_enabled: bool,
    size: Rect,
) {
    if !state.visible {
        return;
    }
    if state.picking {
        render_spec_picker(frame, state, size);
        return;
    }
    let Some(spec) = &state.spec else { return };
    let width = 92.min(size.width.saturating_sub(2));
    let height = 26.min(size.height.saturating_sub(2));
    let area = centered_rect(width, height, size);
    state.last_rect.set(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            " Spec Review ",
            Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD),
        ))
        .title_alignment(ratatui::layout::Alignment::Left);
    frame.render_widget(Clear, area);
    frame.render_widget(block.clone(), area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(5), // content rows; 4 for actions+hints
    };
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let lines = content_lines(spec);
    let visible_rows = inner.height as usize;
    let scroll = state.scroll.min(lines.len().saturating_sub(visible_rows));
    let paragraph = Paragraph::new(lines)
        .scroll((scroll as u16, 0))
        .wrap(Wrap { trim: false })
        .style(Style::default().fg(BODY));
    frame.render_widget(paragraph, inner);

    // Action row + hints, at the bottom.
    let buf = frame.buffer_mut();
    let action_y = area.y + area.height - 3;
    let hint_y = area.y + area.height - 2;
    let labels = ["Accept", "Edit Spec", "Reject"];
    // Lay the buttons out centered-ish from the left: [ Accept ] [ Edit Spec ] [ Reject ]
    let mut x = area.x + 2;
    for (i, label) in labels.iter().enumerate() {
        let selected = i == state.selected_action;
        let text = format!("[ {label} ]");
        let style = if selected {
            Style::default()
                .fg(SEL_FG)
                .bg(SEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        for (j, ch) in text.chars().enumerate() {
            let bx = x + j as u16;
            if bx >= area.x + area.width - 2 {
                break;
            }
            buf[(bx, action_y)]
                .set_symbol(&ch.to_string())
                .set_style(style);
        }
        x += text.chars().count() as u16 + 3;
    }

    let hint =
        "\u{2191}/\u{2193} j/k scroll \u{b7} \u{2190}/\u{2192} h/l action \u{b7} enter \u{b7} esc";
    for (i, ch) in hint.chars().enumerate() {
        let bx = area.x + i as u16;
        if bx >= area.x + area.width - 1 {
            break;
        }
        buf[(bx, hint_y)]
            .set_symbol(&ch.to_string())
            .set_style(Style::default().fg(DIM));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> Spec {
        Spec {
            title: "Rate-Limiting Middleware".to_string(),
            requirements: vec!["Per-IP rate limiting".to_string()],
            files_to_touch: vec![clawde_core::spec::FilePlan {
                path: "crates/api/src/middleware.rs".to_string(),
                action: FileAction::Create,
                description: "New middleware".to_string(),
            }],
            data_models: vec![],
            acceptance_tests: vec![clawde_core::spec::AcceptanceTest {
                description: "Requests under limit pass through".to_string(),
            }],
            edge_cases: vec!["IPv6 normalized".to_string()],
        }
    }

    #[test]
    fn open_loads_spec_from_file() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-review-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rate-limiting.json");
        sample_spec().write_to(&path).unwrap();

        let mut dialog = SpecReviewState::new();
        dialog.open(path.clone()).expect("open spec");
        assert!(dialog.visible);
        assert_eq!(
            dialog.spec.as_ref().unwrap().title,
            "Rate-Limiting Middleware"
        );
        assert_eq!(dialog.path.as_ref().unwrap(), &path);
        assert_eq!(dialog.selected_action, ACTION_ACCEPT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rejects_bad_file() {
        let dir =
            std::env::temp_dir().join(format!("clawde-spec-review-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        let mut dialog = SpecReviewState::new();
        let err = dialog.open(path).expect_err("bad json must fail");
        assert!(err.contains("Could not parse spec JSON"));
        assert!(!dialog.visible);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_latest_single_spec_opens_directly() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-latest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        let mut fresh = sample_spec();
        fresh.title = "Fresh Spec".to_string();
        fresh.write_to(&dir.join("specs/fresh.json")).unwrap();

        let mut dialog = SpecReviewState::new();
        dialog.open_latest(&dir).expect("latest spec found");
        // Exactly one spec: open it directly, no picker.
        assert!(!dialog.picking);
        assert_eq!(dialog.spec.as_ref().unwrap().title, "Fresh Spec");
        assert_eq!(dialog.selected_action, ACTION_ACCEPT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_latest_errors_without_specs_dir() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut dialog = SpecReviewState::new();
        let err = dialog.open_latest(&dir).expect_err("no specs dir");
        assert!(err.contains("No spec found"));
        assert!(!dialog.visible);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_latest_enters_picker_with_multiple_specs() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-pick-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        sample_spec().write_to(&dir.join("specs/one.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        sample_spec().write_to(&dir.join("specs/two.json")).unwrap();

        let mut dialog = SpecReviewState::new();
        dialog.open_latest(&dir).expect("picker entered");
        assert!(dialog.visible);
        assert!(dialog.picking);
        assert_eq!(dialog.available.len(), 2);
        // Newest spec is listed first and pre-highlighted.
        assert!(dialog.available[0].ends_with("two.json"));
        assert_eq!(dialog.picked, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn confirm_pick_opens_selected_spec() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-confirm-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        let mut one = sample_spec();
        one.title = "One".to_string();
        one.write_to(&dir.join("specs/one.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut two = sample_spec();
        two.title = "Two".to_string();
        two.write_to(&dir.join("specs/two.json")).unwrap();

        let mut dialog = SpecReviewState::new();
        dialog.open_latest(&dir).unwrap();
        assert!(dialog.picking);
        // Highlight the older spec, then open it.
        dialog.pick_next();
        dialog.confirm_pick();
        assert!(!dialog.picking);
        assert!(dialog.visible);
        assert_eq!(dialog.spec.as_ref().unwrap().title, "One");
        assert!(dialog.path.as_ref().unwrap().ends_with("one.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pick_navigation_clamps() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-nav-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        sample_spec().write_to(&dir.join("specs/a.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        sample_spec().write_to(&dir.join("specs/b.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        sample_spec().write_to(&dir.join("specs/c.json")).unwrap();

        let mut dialog = SpecReviewState::new();
        dialog.open_latest(&dir).unwrap();
        dialog.pick_prev(); // clamps at 0
        assert_eq!(dialog.picked, 0);
        for _ in 0..10 {
            dialog.pick_next();
        }
        assert_eq!(dialog.picked, 2); // clamps at last
                                      // Navigation is a no-op once the spec is open.
        dialog.confirm_pick();
        dialog.pick_next();
        assert!(!dialog.picking);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_selection_clamps() {
        let mut dialog = SpecReviewState::new();
        dialog.selected_action = ACTION_ACCEPT;
        dialog.select_prev();
        assert_eq!(dialog.selected_action, ACTION_ACCEPT); // clamped at Accept
        dialog.select_next();
        dialog.select_next();
        dialog.select_next();
        assert_eq!(dialog.selected_action, ACTION_REJECT); // clamped at Reject
    }

    #[test]
    fn accept_message_includes_spec_and_criteria() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-msg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rate-limiting.json");
        sample_spec().write_to(&path).unwrap();
        let mut dialog = SpecReviewState::new();
        dialog.open(path).unwrap();
        let msg = dialog.accept_message().expect("accept message");
        assert!(msg.contains("ACCEPTED"));
        assert!(msg.contains("Rate-Limiting Middleware"));
        assert!(msg.contains("Requests under limit pass through"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scroll_down_is_bounded() {
        let mut dialog = SpecReviewState::new();
        for _ in 0..100 {
            dialog.scroll_down(50, 20);
        }
        assert_eq!(dialog.scroll, 30); // 50 - 20
    }
}
