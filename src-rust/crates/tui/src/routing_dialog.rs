// routing_dialog.rs — Task-routing dialog (`/routing edit`), audit spec §8.6.
//
// A modal that shows the current task-to-upstream assignments and lets the
// user pin specific upstreams per task. Pins are written to
// `providers.free.options.routing.task_preferences` (settings.json); saving
// with any pin also flips the routing strategy to `task_based` so the pins
// take effect (pinning implies task routing).
//
// Layout (centered modal):
//
//     ┌─ Task routing · strategy: sequential ─────────────── esc ┐
//     │  Tasks                  │  Upstreams for code generation │
//     │  ▸ code generation  …   │  [x] groq       Groq           │
//     │    code edit        …   │  [ ] cerebras   Cerebras       │
//     │    reasoning        …   │  [ ] huggingface Hugging Face   │
//     │    …                   │   … (default)                   │
//     │                         │                                 │
//     │  ↑/↓ j/k navigate · Tab/←/→ pane · space pin · r reset ·  │
//     │  a reset all · enter save · esc cancel                    │
//     └───────────────────────────────────────────────────────────┘
//
// The left column shows each task's effective assignment: the pinned list
// when overridden, otherwise the built-in default preferences ("auto · …").

use std::cell::Cell;
use std::collections::HashMap;

use clawde_api::providers::free::{task_preference_ids, TaskType, FREE_CATALOG};
use clawde_core::config::Config;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use ratatui::Frame;

use crate::overlays::centered_rect;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Border accent — violet, distinct from the clawde-pink used elsewhere.
const BORDER: Color = Color::Rgb(150, 120, 210);
/// Selected row background.
const SEL_BG: Color = Color::Rgb(66, 58, 96);
/// Pinned checkbox.
const PINNED: Color = Color::Rgb(110, 200, 140);
/// Unpinned checkbox / dim text.
const DIM: Color = Color::Rgb(120, 120, 132);
/// Built-in-default tag on an unpinned upstream.
const DEFAULT_TAG: Color = Color::Rgb(140, 150, 190);
/// Highlight text on the selected row.
const SEL_FG: Color = Color::Rgb(238, 238, 240);

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Which pane the keyboard focus is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingPane {
    /// The seven task types.
    Tasks,
    /// The upstream checkbox list for the selected task.
    Upstreams,
}

/// Interactive state for the `/routing edit` task-pinning dialog.
#[derive(Debug)]
pub struct RoutingDialogState {
    pub visible: bool,
    /// Area used by the modal in the last render (click-outside detection).
    pub last_rect: Cell<Rect>,
    /// Index into `TaskType::ALL`.
    pub selected_task: usize,
    pub pane: RoutingPane,
    /// Index into `FREE_CATALOG` for the upstream pane.
    pub upstream_idx: usize,
    /// Scroll offset for the upstream pane.
    pub upstream_scroll: usize,
    /// Row count of the upstream pane in the last render, used to keep the
    /// cursor visible when the pane is shorter than the catalog.
    pub last_upstream_visible: Cell<usize>,
    /// Task key (`TaskType::key()`) → pinned upstream ids. An absent entry
    /// means "auto" (the built-in `task_preference_ids` apply).
    pub overrides: HashMap<String, Vec<String>>,
    /// Routing strategy when the dialog opened (shown in the header).
    pub strategy: String,
}

impl Default for RoutingDialogState {
    fn default() -> Self {
        Self {
            visible: false,
            last_rect: Cell::new(Rect::default()),
            selected_task: 0,
            pane: RoutingPane::Tasks,
            upstream_idx: 0,
            upstream_scroll: 0,
            last_upstream_visible: Cell::new(0),
            overrides: HashMap::new(),
            strategy: "sequential".to_string(),
        }
    }
}

impl RoutingDialogState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the dialog, seeding the overrides from the live config's
    /// `providers.free.options.routing.task_preferences`.
    pub fn open(&mut self, config: &Config) {
        self.overrides = parse_task_preferences(config);
        self.strategy = parse_routing_strategy(config);
        self.selected_task = 0;
        self.pane = RoutingPane::Tasks;
        self.upstream_idx = 0;
        self.upstream_scroll = 0;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// The task currently selected in the left pane.
    pub fn current_task(&self) -> TaskType {
        TaskType::ALL[self.selected_task.min(TaskType::ALL.len() - 1)]
    }

    /// The pinned upstream ids for `task` (empty when the task is auto).
    pub fn task_override(&self, task: TaskType) -> &[String] {
        self.overrides
            .get(task.key())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// The effective assignment summary shown in the left pane: the pinned
    /// list, or "auto · <top-2 built-in defaults>" when not overridden.
    pub fn assignment_summary(&self, task: TaskType) -> String {
        let pinned = self.task_override(task);
        if pinned.is_empty() {
            let defaults = task_preference_ids(task);
            let top: Vec<&str> = defaults.iter().take(2).copied().collect();
            format!("auto \u{b7} {}", top.join(","))
        } else {
            let ids: Vec<&str> = pinned.iter().map(|s| s.as_str()).collect();
            format!("pinned \u{b7} {}", ids.join(","))
        }
    }

    pub fn is_pinned(&self, upstream_id: &str) -> bool {
        self.task_override(self.current_task())
            .iter()
            .any(|id| id == upstream_id)
    }

    /// Pin/unpin the upstream at the current upstream-pane cursor position.
    pub fn toggle_selected_upstream(&mut self) {
        if let Some(upstream) = FREE_CATALOG.get(self.upstream_idx) {
            self.toggle_pin(upstream.id);
        }
    }

    pub fn toggle_pin(&mut self, upstream_id: &str) {
        let task = self.current_task();
        let entry = self.overrides.entry(task.key().to_string()).or_default();
        if let Some(pos) = entry.iter().position(|id| id == upstream_id) {
            entry.remove(pos);
        } else {
            entry.push(upstream_id.to_string());
        }
        if entry.is_empty() {
            self.overrides.remove(task.key());
        }
    }

    /// Clear the selected task's override (back to auto).
    pub fn reset_task(&mut self) {
        self.overrides.remove(self.current_task().key());
    }

    /// Clear every task's override.
    pub fn reset_all(&mut self) {
        self.overrides.clear();
    }

    pub fn has_pins(&self) -> bool {
        self.overrides.values().any(|v| !v.is_empty())
    }

    /// The non-empty override map, ready to persist as `task_preferences`.
    pub fn build_task_preferences(&self) -> HashMap<String, Vec<String>> {
        self.overrides
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    /// Move up within the active pane.
    pub fn select_prev(&mut self) {
        match self.pane {
            RoutingPane::Tasks => {
                self.selected_task = self.selected_task.saturating_sub(1);
            }
            RoutingPane::Upstreams => {
                if self.upstream_idx > 0 {
                    self.upstream_idx -= 1;
                }
                self.scroll_upstream_into_view(self.last_upstream_visible.get().max(1));
            }
        }
    }

    /// Move down within the active pane.
    pub fn select_next(&mut self) {
        match self.pane {
            RoutingPane::Tasks => {
                if self.selected_task + 1 < TaskType::ALL.len() {
                    self.selected_task += 1;
                }
            }
            RoutingPane::Upstreams => {
                if self.upstream_idx + 1 < FREE_CATALOG.len() {
                    self.upstream_idx += 1;
                }
                self.scroll_upstream_into_view(self.last_upstream_visible.get().max(1));
            }
        }
    }

    /// Keep the upstream cursor visible given a pane height of `visible` rows
    /// (called from the renderer with the computed list height).
    pub fn scroll_upstream_into_view(&mut self, visible: usize) {
        let visible = visible.max(1);
        if self.upstream_idx < self.upstream_scroll {
            self.upstream_scroll = self.upstream_idx;
        } else if self.upstream_idx >= self.upstream_scroll + visible {
            self.upstream_scroll = self.upstream_idx + 1 - visible;
        }
        self.upstream_scroll = self
            .upstream_scroll
            .min(FREE_CATALOG.len().saturating_sub(visible));
    }

    /// Switch the focused pane.
    pub fn switch_pane(&mut self) {
        self.pane = match self.pane {
            RoutingPane::Tasks => RoutingPane::Upstreams,
            RoutingPane::Upstreams => RoutingPane::Tasks,
        };
    }
}

// ---------------------------------------------------------------------------
// Config parsing
// ---------------------------------------------------------------------------

fn routing_value(config: &Config) -> Option<&serde_json::Value> {
    config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
}

fn parse_task_preferences(config: &Config) -> HashMap<String, Vec<String>> {
    routing_value(config)
        .and_then(|v| v.get("task_preferences"))
        .and_then(|v| serde_json::from_value::<HashMap<String, Vec<String>>>(v.clone()).ok())
        .unwrap_or_default()
}

fn parse_routing_strategy(config: &Config) -> String {
    routing_value(config)
        .and_then(|v| v.get("strategy"))
        .and_then(|v| v.as_str())
        .unwrap_or("sequential")
        .to_string()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the task-routing dialog as a centered modal.
pub fn render_routing_dialog(
    frame: &mut Frame,
    state: &RoutingDialogState,
    _vim_enabled: bool,
    size: Rect,
) {
    if !state.visible {
        return;
    }
    let width = 88.min(size.width.saturating_sub(2));
    let height = 22.min(size.height.saturating_sub(2));
    let area = centered_rect(width, height, size);
    state.last_rect.set(area);
    let strategy_note = if state.strategy == "task_based" {
        "strategy: task_based".to_string()
    } else {
        format!(
            "strategy: {} \u{b7} saving pins switches to task",
            state.strategy
        )
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Line::from(vec![
            Span::styled(
                " Task routing \u{b7} ",
                Style::default().fg(SEL_FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(strategy_note, Style::default().fg(DEFAULT_TAG)),
        ]))
        .title_alignment(ratatui::layout::Alignment::Left);
    frame.render_widget(Clear, area);
    frame.render_widget(block.clone(), area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(3), // reserve the hint row
    };
    let left_width = 40.min(inner.width.saturating_div(2));
    let right_area = Rect {
        x: inner.x + left_width,
        y: inner.y,
        width: inner.width.saturating_sub(left_width),
        height: inner.height,
    };
    let left_area = Rect {
        x: inner.x,
        y: inner.y,
        width: left_width,
        height: inner.height,
    };

    let buf = frame.buffer_mut();
    render_tasks_pane(buf, left_area, state);
    render_upstreams_pane(buf, right_area, state);

    // Hint row — one row above the bottom border (the inner rect already
    // leaves `area.height - 3` for the panes, so this row is free).
    let hint = "\u{2191}/\u{2193} j/k nav \u{b7} Tab/\u{2190}/\u{2192} pane \u{b7} space pin \u{b7} r reset \u{b7} a reset all \u{b7} enter save \u{b7} esc";
    let hint_y = area.y + area.height - 2;
    for (i, ch) in hint.chars().enumerate() {
        if area.x + i as u16 >= area.x + area.width {
            break;
        }
        buf[(area.x + i as u16, hint_y)]
            .set_symbol(&ch.to_string())
            .set_style(Style::default().fg(DIM));
    }
}

fn render_tasks_pane(buf: &mut Buffer, area: Rect, state: &RoutingDialogState) {
    let pane = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(" Tasks ", Style::default().fg(BORDER)));
    pane.render(area, buf);
    let rows_start = area.y + 1;

    for (i, task) in TaskType::ALL.iter().enumerate() {
        let row_y = rows_start + i as u16;
        if row_y >= area.y + area.height - 1 {
            break;
        }
        let focused = state.pane == RoutingPane::Tasks && i == state.selected_task;
        let marker = if focused { "\u{25b8} " } else { "  " };
        let label = format!("{:<15}", task.label());
        let summary = state.assignment_summary(*task);
        let mut row = String::with_capacity(area.width as usize);
        row.push_str(marker);
        row.push_str(&label);
        row.push(' ');
        row.push_str(&summary);

        let style = if focused {
            Style::default()
                .fg(SEL_FG)
                .bg(SEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(DIM)
        };
        draw_clipped(
            buf,
            area.x + 1,
            row_y,
            area.width.saturating_sub(2),
            &row,
            style,
        );
    }
}

fn render_upstreams_pane(buf: &mut Buffer, area: Rect, state: &RoutingDialogState) {
    let task = state.current_task();
    let pane = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title(Span::styled(
            format!(" Upstreams for {} ", task.label()),
            Style::default().fg(BORDER),
        ));
    pane.render(area, buf);

    let inner_h = area.height.saturating_sub(2) as usize;
    if inner_h == 0 {
        return;
    }
    state.last_upstream_visible.set(inner_h);
    let defaults = task_preference_ids(task);

    for (i, upstream) in FREE_CATALOG.iter().enumerate() {
        let rel = i as isize - state.upstream_scroll as isize;
        if rel < 0 || rel >= inner_h as isize {
            continue;
        }
        let row_y = area.y + 1 + rel as u16;
        let focused = state.pane == RoutingPane::Upstreams && i == state.upstream_idx;
        let pinned = state.is_pinned(upstream.id);
        let is_default = defaults.contains(&upstream.id);

        let mut row = String::with_capacity(area.width as usize);
        row.push_str(if focused { "\u{25b8} " } else { "  " });
        if pinned {
            row.push_str("[x] ");
        } else {
            row.push_str("[ ] ");
        }
        row.push_str(upstream.id);
        row.push(' ');
        row.push_str(upstream.title);
        if !pinned && is_default {
            row.push_str(" (default)");
        }

        let style = if focused {
            Style::default()
                .fg(SEL_FG)
                .bg(SEL_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(if pinned { PINNED } else { DIM })
        };
        draw_clipped(
            buf,
            area.x + 1,
            row_y,
            area.width.saturating_sub(2),
            &row,
            style,
        );
    }
}
/// Write `text` at (x, y), clipping to `max_chars` columns with an
/// ellipsis when the text is cut short.
fn draw_clipped(buf: &mut Buffer, x: u16, y: u16, max_chars: u16, text: &str, style: Style) {
    if max_chars == 0 {
        return;
    }
    let width = text.chars().count();
    let cut = width > max_chars as usize;
    let limit = if cut {
        (max_chars as usize).saturating_sub(1)
    } else {
        max_chars as usize
    };
    for (i, ch) in text.chars().enumerate() {
        if i >= limit {
            break;
        }
        buf[(x + i as u16, y)]
            .set_symbol(&ch.to_string())
            .set_style(style);
    }
    if cut {
        buf[(x + limit as u16, y)]
            .set_symbol("\u{2026}")
            .set_style(style);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(prefs: Option<serde_json::Value>, strategy: &str) -> Config {
        let mut routing = serde_json::json!({ "strategy": strategy });
        if let Some(p) = prefs {
            routing["task_preferences"] = p;
        }
        let mut options = std::collections::HashMap::new();
        options.insert("routing".to_string(), routing);
        let mut provider_configs = std::collections::HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            clawde_core::config::ProviderConfig {
                options,
                ..Default::default()
            },
        );
        Config {
            provider_configs,
            ..Default::default()
        }
    }

    #[test]
    fn open_parses_existing_overrides_and_strategy() {
        let cfg = config_with(
            Some(serde_json::json!({ "code_generation": ["groq", "cerebras"] })),
            "task_based",
        );
        let mut dialog = RoutingDialogState::new();
        dialog.open(&cfg);
        assert!(dialog.visible);
        assert_eq!(dialog.strategy, "task_based");
        assert_eq!(
            dialog.task_override(TaskType::CodeGeneration),
            &["groq".to_string(), "cerebras".to_string()][..]
        );
        assert!(dialog.task_override(TaskType::Reasoning).is_empty());
    }

    #[test]
    fn toggle_pin_adds_and_removes() {
        let mut dialog = RoutingDialogState::new();
        dialog.open(&Config::default());
        dialog.selected_task = 0; // CodeGeneration
        assert!(!dialog.is_pinned("groq"));
        dialog.toggle_pin("groq");
        assert!(dialog.is_pinned("groq"));
        assert_eq!(
            dialog.task_override(TaskType::CodeGeneration),
            &["groq".to_string()][..]
        );
        dialog.toggle_pin("groq");
        assert!(!dialog.is_pinned("groq"));
        // Removing the last pin drops the override entry entirely.
        assert!(dialog.task_override(TaskType::CodeGeneration).is_empty());
        assert!(dialog.overrides.is_empty());
    }

    #[test]
    fn reset_task_and_reset_all_clear_overrides() {
        let cfg = config_with(
            Some(serde_json::json!({
                "code_generation": ["groq"],
                "reasoning": ["google"]
            })),
            "task_based",
        );
        let mut dialog = RoutingDialogState::new();
        dialog.open(&cfg);
        dialog.selected_task = 0;
        dialog.reset_task();
        assert!(dialog.task_override(TaskType::CodeGeneration).is_empty());
        assert!(dialog.has_pins());
        dialog.reset_all();
        assert!(!dialog.has_pins());
        assert!(dialog.build_task_preferences().is_empty());
    }

    #[test]
    fn build_task_preferences_drops_empty_entries() {
        let mut dialog = RoutingDialogState::new();
        dialog.open(&Config::default());
        dialog.toggle_pin("groq");
        dialog.toggle_pin("groq"); // back to empty
        assert!(dialog.build_task_preferences().is_empty());
        dialog.toggle_pin("groq");
        let prefs = dialog.build_task_preferences();
        assert_eq!(
            prefs.get("code_generation"),
            Some(&vec!["groq".to_string()])
        );
    }

    #[test]
    fn assignment_summary_shows_pins_or_auto_defaults() {
        let mut dialog = RoutingDialogState::new();
        dialog.open(&Config::default());
        let auto = dialog.assignment_summary(TaskType::Verification);
        assert!(auto.starts_with("auto \u{b7} "), "got: {auto}");
        dialog.toggle_pin("groq");
        let pinned = dialog.assignment_summary(TaskType::CodeGeneration);
        assert!(pinned.starts_with("pinned \u{b7} groq"), "got: {pinned}");
    }

    #[test]
    fn navigation_clamps_to_bounds() {
        let mut dialog = RoutingDialogState::new();
        dialog.open(&Config::default());
        dialog.select_prev();
        assert_eq!(dialog.selected_task, 0);
        for _ in 0..10 {
            dialog.select_next();
        }
        assert_eq!(dialog.selected_task, TaskType::ALL.len() - 1);

        dialog.switch_pane();
        assert_eq!(dialog.pane, RoutingPane::Upstreams);
        dialog.select_next();
        assert_eq!(dialog.upstream_idx, 1);
        for _ in 0..20 {
            dialog.select_next();
        }
        assert_eq!(dialog.upstream_idx, FREE_CATALOG.len() - 1);
        dialog.scroll_upstream_into_view(5);
        assert!(dialog.upstream_scroll <= FREE_CATALOG.len().saturating_sub(5));
    }

    #[test]
    fn scroll_keeps_cursor_visible() {
        let mut dialog = RoutingDialogState::new();
        dialog.open(&Config::default());
        dialog.switch_pane();
        dialog.last_upstream_visible.set(5);
        // Navigate past the window — select_next must scroll to keep the
        // cursor in view (production path, no manual scroll call).
        for _ in 0..10 {
            dialog.select_next();
        }
        assert!(dialog.upstream_idx < dialog.upstream_scroll + 5);
        assert!(dialog.upstream_idx >= dialog.upstream_scroll);
        // And back up again.
        for _ in 0..10 {
            dialog.select_prev();
        }
        assert!(dialog.upstream_idx < dialog.upstream_scroll + 5);
        assert!(dialog.upstream_idx >= dialog.upstream_scroll);
    }
}
