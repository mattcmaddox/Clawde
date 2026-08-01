// free_mode_dialog.rs — Setup dialog for the composite "Free" provider.
//
// Walks the user through the multi-provider free-mode caveats and collects
// API keys from any subset of the supported upstreams. The chain stacks
// many free tiers (Groq, Cerebras, Google, Mistral, SambaNova, NVIDIA,
// Cohere, OpenRouter, OpenCode Zen, Z.AI, Zhipu) behind one synthetic
// `free/auto` model — the more keys the user pastes in, the more
// providers the router can fall back to. Minimum 1 key to enable; more
// is better.
//
// Layout:
//   ┌─ Connect Free (multi-provider) ───────────────── esc ┐
//   │  Stack the free tiers from many providers behind     │
//   │  one endpoint. ⚠ context management is worse than    │
//   │  paid models; long sessions truncate aggressively.   │
//   │                                                      │
//   │  Paste any keys you have — more = better availability│
//   │  and higher daily caps. Minimum 1 key to enable.     │
//   │                                                      │
//   │  ▸ Groq                          console.groq.com/.. │
//   │    ••••••••AbCd_                                     │
//   │    Cerebras                      cloud.cerebras.ai   │
//   │    paste your API key here...                        │
//   │    Google Gemini                 aistudio.google.com │
//   │    ••••••••wxyz                                      │
//   │    …7 more — tab/↑↓ to scroll                        │
//   │                                                      │
//   │  ↑/↓ next   enter confirm (1+ keys)                  │
//   └──────────────────────────────────────────────────────┘

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use clawde_api::{FreeUpstream, FREE_CATALOG};

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAURST_PANEL_BG};
use std::cell::Cell;

/// One row in the dialog — one provider's name, URL, and the user's
/// (possibly empty) typed key.
#[derive(Debug, Clone)]
pub struct FreeModeField {
    pub upstream: &'static FreeUpstream,
    pub key: String,
    /// When `true`, this upstream is hidden behind the "show all" toggle.
    pub collapsed: bool,
    /// Whether this upstream is enabled in the free provider chain.
    /// Disabled upstreams are skipped by `take_values()` even if they have keys.
    pub enabled: bool,
    /// Result of key validation: `None` = not tested, `Some(Ok(()))` = valid,
    /// `Some(Err(reason))` = invalid.
    pub validation_status: Option<Result<(), String>>,
    /// When `true`, the key was detected from an environment variable
    /// and is read-only in this dialog (cannot be edited).
    pub from_env: bool,
}

pub struct FreeModeDialogState {
    pub visible: bool,
    /// The area used by this dialog in the last render (for click-outside detection).
    pub last_rect: Cell<Rect>,
    pub fields: Vec<FreeModeField>,
    pub active_idx: usize,
    /// First visible field index (for scrolling when fields > viewport).
    pub scroll_offset: usize,
    /// When `true`, all upstreams are shown (none collapsed).
    pub show_all: bool,
    /// When `true`, a key validation is in progress (prevents rapid Ctrl+V).
    pub is_validating: bool,
}

impl Default for FreeModeDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeModeDialogState {
    pub fn new() -> Self {
        let fields = FREE_CATALOG
            .iter()
            .map(|upstream| FreeModeField {
                upstream,
                key: String::new(),
                collapsed: true,
                enabled: true,
                validation_status: None,
                from_env: false,
            })
            .collect();
        Self {
            visible: false,
            fields,
            active_idx: 0,
            scroll_offset: 0,
            show_all: false,
            is_validating: false,
            last_rect: Cell::new(Rect::default()),
        }
    }

    /// Mark upstreams whose keys came from environment variables.
    /// These are shown as read-only in the dialog — the user can see them
    /// but they must edit the env var in their shell profile to change.
    pub fn set_env_var_keys(&mut self, env_var_keys: &[(&str, String)]) {
        for (id, _key) in env_var_keys {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                field.from_env = true;
            }
        }
    }

    /// Open the dialog, pre-populating each row from `existing[upstream.id]`
    /// when present. Fields with keys are expanded; empty fields are collapsed.
    /// Also reads `disabled_upstreams` from settings to set the enabled state.
    pub fn open(&mut self, existing: &[(&str, String)]) {
        self.visible = true;
        self.show_all = false;
        // Preserve any existing keys rather than clearing (incremental editing).
        for (id, key) in existing {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                // Don't overwrite env-var keys with auth_store keys (env var wins).
                if !field.from_env {
                    field.key = key.clone();
                }
            }
        }
        // Read disabled upstreams from settings so toggle state persists.
        let disabled_upstreams: Vec<String> = clawde_core::config::Settings::load_sync()
            .map(|s| s.effective_config())
            .unwrap_or_default()
            .provider_configs
            .get("free")
            .and_then(|pc| pc.options.get("routing"))
            .and_then(|v| v.get("disabled_upstreams"))
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
            .unwrap_or_default();

        // Collapse empty fields, expand fields with keys.
        for field in &mut self.fields {
            field.collapsed = field.key.trim().is_empty();
            field.enabled = !disabled_upstreams.contains(&field.upstream.id.to_string());
        }
        // Start on the first empty (non-collapsed visible) field, or the first
        // field if none are empty.
        let visible = self.visible_field_indices();
        self.active_idx = visible
            .iter()
            .find(|&&i| self.fields[i].key.trim().is_empty())
            .copied()
            .unwrap_or(*visible.first().unwrap_or(&0));
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    /// Spawn background validation pings for every non-empty, enabled upstream.
    /// Returns a receiver the caller drains in the main loop. Each received
    /// `(idx, result)` should be passed to `set_validation_result()`.
    pub fn start_auto_pings(
        &mut self,
    ) -> Option<std::sync::mpsc::Receiver<(usize, Result<(), String>)>> {
        let targets: Vec<(usize, String, String)> = self
            .fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.enabled && !f.key.trim().is_empty())
            .map(|(i, f)| (i, f.upstream.id.to_string(), f.key.trim().to_string()))
            .collect();

        if targets.is_empty() {
            return None;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        for (idx, upstream_id, key) in targets {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
                let _ = tx.send((idx, result));
            });
        }
        drop(tx);

        Some(rx)
    }

    pub fn close(&mut self) {
        self.visible = false;
        // Don't clear keys — preserves state for incremental editing if the
        // user re-opens the dialog.
        self.active_idx = 0;
        self.scroll_offset = 0;
        self.show_all = false;
        self.is_validating = false; // Clear any pending validation state
                                    // Reset all collapsed flags so the next open() recalculates.
        for field in &mut self.fields {
            field.collapsed = false;
        }
    }

    /// Number of rows shown at once in the scrolling viewport.
    pub const VISIBLE_ROWS: usize = 4;

    /// Return indices of fields that are currently visible (non-collapsed or
    /// show_all is active).
    pub fn visible_field_indices(&self) -> Vec<usize> {
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, f)| self.show_all || !f.collapsed)
            .map(|(i, _)| i)
            .collect()
    }

    /// Toggle whether the active upstream is enabled/disabled.
    /// Disabled upstreams are skipped by `take_values()` even if they have keys.
    /// The disabled list is persisted to settings.json immediately, preserving
    /// any existing routing strategy.
    pub fn toggle_enabled(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.enabled = !field.enabled;
            // Persist the disabled upstreams to settings.json.
            let disabled: Vec<String> = self
                .fields
                .iter()
                .filter(|f| !f.enabled)
                .map(|f| f.upstream.id.to_string())
                .collect();
            if let Ok(mut settings) = clawde_core::config::Settings::load_sync() {
                // Preserve existing routing configuration (strategy, etc.)
                let mut cfg = settings
                    .config
                    .provider_configs
                    .get("free")
                    .and_then(|pc| pc.options.get("routing"))
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"strategy": "sequential"}));
                if let Some(obj) = cfg.as_object_mut() {
                    obj.insert(
                        "disabled_upstreams".to_string(),
                        serde_json::json!(disabled),
                    );
                }
                settings
                    .config
                    .provider_configs
                    .entry("free".to_string())
                    .or_default()
                    .options
                    .insert("routing".to_string(), cfg);
                let _ = settings.save_sync();
            }
        }
    }

    /// Toggle between showing only non-collapsed fields and all fields.
    pub fn toggle_show_all(&mut self) {
        self.show_all = !self.show_all;
        // If hiding collapsed fields, ensure active_idx is still on a visible one.
        if !self.show_all {
            let visible = self.visible_field_indices();
            if !visible.contains(&self.active_idx) {
                self.active_idx = visible.first().copied().unwrap_or(0);
            }
        }
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    /// Collapsed count (unconfigured upstreams currently hidden).
    pub fn collapsed_count(&self) -> usize {
        self.fields.iter().filter(|f| f.collapsed).count()
    }

    pub fn move_next(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        match pos {
            Some(p) if p + 1 < visible.len() => self.active_idx = visible[p + 1],
            _ => self.active_idx = visible[0],
        }
        self.ensure_active_visible();
    }

    pub fn move_prev(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        let pos = visible.iter().position(|i| *i == self.active_idx);
        match pos {
            Some(p) if p > 0 => self.active_idx = visible[p - 1],
            _ => self.active_idx = *visible.last().unwrap(),
        }
        self.ensure_active_visible();
    }

    fn ensure_active_visible(&mut self) {
        let visible = self.visible_field_indices();
        if visible.is_empty() {
            return;
        }
        // Convert absolute active_idx to its position within visible fields.
        let pos = visible
            .iter()
            .position(|i| *i == self.active_idx)
            .unwrap_or(0);
        if pos < self.scroll_offset {
            self.scroll_offset = pos;
        } else if pos >= self.scroll_offset + Self::VISIBLE_ROWS {
            self.scroll_offset = pos + 1 - Self::VISIBLE_ROWS;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            // Skip env-var keys — they are read-only.
            if field.from_env {
                return;
            }
            field.key.push(c);
            // Auto-expand collapsed field when user starts typing.
            if field.collapsed && !field.key.trim().is_empty() {
                field.collapsed = false;
            }
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            // Skip env-var keys — they are read-only.
            if field.from_env {
                return;
            }
            field.key.pop();
        }
    }

    /// Start validating the active field's API key in the background.
    /// Returns a `Receiver` that the caller (App) must drain in the main loop.
    /// Only one validation runs at a time.
    pub fn start_validate(
        &mut self,
    ) -> Option<std::sync::mpsc::Receiver<(usize, Result<(), String>)>> {
        if self.is_validating {
            return None;
        }
        let field = self.fields.get(self.active_idx)?;
        let key = field.key.trim().to_string();
        if key.is_empty() {
            return None;
        }
        let upstream_id = field.upstream.id.to_string();
        let idx = self.active_idx;

        let (tx, rx) = std::sync::mpsc::channel();
        self.is_validating = true;

        std::thread::spawn(move || {
            let result = clawde_api::providers::free::validate_upstream_key(&upstream_id, &key);
            // Best-effort send; silently fails if the dialog was closed.
            let _ = tx.send((idx, result));
        });

        Some(rx)
    }

    /// Set the validation result for a given field index.
    /// Called from the main loop when a validation result arrives.
    pub fn set_validation_result(&mut self, idx: usize, result: Result<(), String>) {
        self.is_validating = false;
        if let Some(field) = self.fields.get_mut(idx) {
            field.validation_status = Some(result);
        }
    }

    /// Enabling Free mode requires at least one non-empty key on an enabled
    /// upstream. More is better. Env-var keys count.
    pub fn can_submit(&self) -> bool {
        self.fields
            .iter()
            .any(|f| f.enabled && !f.key.trim().is_empty())
    }

    pub fn filled_count(&self) -> usize {
        self.fields
            .iter()
            .filter(|f| !f.key.trim().is_empty())
            .count()
    }

    pub fn env_var_count(&self) -> usize {
        self.fields.iter().filter(|f| f.from_env).count()
    }

    /// Consume the dialog state, returning every non-empty `(provider_id, key)`
    /// pair the user entered. Does NOT close the dialog — the caller closes it
    /// explicitly so incremental editing preserves state.
    /// Disabled upstreams are excluded even if they have keys.
    pub fn take_values(&mut self) -> Vec<(&'static str, String)> {
        self.fields
            .iter()
            .filter_map(|f| {
                if !f.enabled {
                    return None;
                }
                let trimmed = f.key.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some((f.upstream.id, trimmed.to_string()))
                }
            })
            .collect()
    }

    /// Apply the current values to the auth store without closing the dialog.
    /// This lets users add keys incrementally: type a key, press Ctrl+S to save
    /// it, then move to the next field and repeat.
    /// Returns the number of keys saved.
    pub fn apply_values(&mut self) -> usize {
        let values = self.take_values();
        let count = values.len();
        let mut auth_store = clawde_core::AuthStore::load();
        for (provider_id, key) in values {
            auth_store.set(provider_id, clawde_core::StoredCredential::ApiKey { key });
        }
        auth_store.save();
        count
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn mask_key(input: &str) -> String {
    if input.is_empty() {
        "paste your API key here...".to_string()
    } else {
        let chars: Vec<char> = input.chars().collect();
        if chars.len() <= 4 {
            input.to_string()
        } else {
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{}{}", "\u{2022}".repeat(chars.len() - 4), tail)
        }
    }
}

pub fn render_free_mode_dialog(frame: &mut Frame, state: &FreeModeDialogState, area: Rect) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let tip = Color::Rgb(120, 210, 150);
    let dialog_bg = CLAURST_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 24u16.min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    state.last_rect.set(dialog_area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let total = state.fields.len();
    let filled = state.filled_count();
    let env_count = state.env_var_count();
    let title_text = format!(
        "Connect Free (multi-provider \u{2014} {}/{} keys)",
        filled, total
    );
    let title_pad = inner
        .width
        .saturating_sub(title_text.chars().count() as u16 + 5) as usize;

    let confirm_hint = if state.can_submit() {
        format!(
            " enter confirm ({} key{} — more = better)",
            filled,
            if filled == 1 { "" } else { "s" }
        )
    } else {
        " paste at least 1 key — as many as you can add is better".to_string()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row
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

    // Description (one tight line) + tip.
    lines.push(Line::from(vec![Span::styled(
        " Stack free tiers behind one endpoint.",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(vec![
        Span::styled(
            " TIP ",
            Style::default().fg(tip).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "More keys = better availability and higher caps.",
            Style::default().fg(tip),
        ),
    ]));
    // Show env-var key hint when any are detected.
    if env_count > 0 {
        lines.push(Line::from(vec![
            Span::styled(
                " \u{1f512} ",
                Style::default().fg(Color::Rgb(180, 160, 80)),
            ),
            Span::styled(
                format!(
                    "{} key(s) from env vars \u{2014} read-only here; edit in your shell profile to change.",
                    env_count
                ),
                Style::default()
                    .fg(Color::Rgb(180, 160, 80))
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    lines.push(Line::from(""));

    // Determine which fields are visible
    let visible_indices = state.visible_field_indices();
    let visible_count = visible_indices.len();

    // Show collapse hint when there are collapsed fields and we're not showing all
    if !state.show_all {
        let collapsed = state.collapsed_count();
        if collapsed > 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "   \u{2192} {} upstream{} collapsed",
                        collapsed,
                        if collapsed == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(dim).add_modifier(Modifier::ITALIC),
                ),
                Span::styled(
                    "  [tab to show all]",
                    Style::default()
                        .fg(Color::Rgb(120, 120, 140))
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    // Key health summary bar
    let valid_count = state
        .fields
        .iter()
        .filter(|f| matches!(f.validation_status, Some(Ok(()))))
        .count();
    let invalid_count = state
        .fields
        .iter()
        .filter(|f| matches!(f.validation_status, Some(Err(_))))
        .count();
    let untested_count = state
        .fields
        .iter()
        .filter(|f| !f.key.trim().is_empty() && f.validation_status.is_none())
        .count();
    if valid_count > 0 || invalid_count > 0 || untested_count > 0 {
        let health_text = if valid_count > 0 && invalid_count == 0 && untested_count == 0 {
            format!(
                "   \u{2713} {} key{} valid",
                valid_count,
                if valid_count == 1 { "" } else { "s" }
            )
        } else {
            let mut parts: Vec<String> = Vec::new();
            if valid_count > 0 {
                parts.push(format!("\u{2713} {} ok", valid_count));
            }
            if invalid_count > 0 {
                parts.push(format!("\u{2717} {} bad", invalid_count));
            }
            if untested_count > 0 {
                parts.push(format!("\u{231b} {} pending", untested_count));
            }
            format!("   {}", parts.join("  "))
        };
        lines.push(Line::from(vec![Span::styled(
            health_text,
            Style::default().fg(if invalid_count > 0 {
                Color::Yellow
            } else {
                tip
            }),
        )]));
    }

    // Field viewport: use visible indices only
    let start = state.scroll_offset.min(visible_count.saturating_sub(1));
    let end = (start + FreeModeDialogState::VISIBLE_ROWS).min(visible_count);
    if start > 0 {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2191} {} above", start),
            Style::default().fg(dim),
        )]));
    }

    let row_label_width: usize = state
        .fields
        .iter()
        .map(|f| f.upstream.title.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);

    for &idx in visible_indices.iter().skip(start).take(end - start) {
        let field = &state.fields[idx];
        let active = idx == state.active_idx;
        let marker = if active { "\u{25b8}" } else { " " };
        let label_style = if active {
            if field.enabled {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Rgb(140, 80, 80))
                    .add_modifier(Modifier::BOLD)
            }
        } else if field.enabled {
            Style::default().fg(muted)
        } else {
            Style::default().fg(dim)
        };
        let url_style = Style::default().fg(dim);

        let label_padded = format!("{:<width$}", field.upstream.title, width = row_label_width);
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", marker), Style::default().fg(pink)),
            Span::styled(label_padded, label_style),
            Span::styled("   ", Style::default()),
            Span::styled(field.upstream.key_url.to_string(), url_style),
        ]));

        let masked = mask_key(&field.key);
        let input_style = if field.from_env {
            Style::default()
                .fg(Color::Rgb(180, 160, 80))
                .add_modifier(Modifier::ITALIC)
        } else if field.key.is_empty() {
            Style::default().fg(dim)
        } else if active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let cursor = if active && !field.from_env { "_" } else { "" };
        let mut input_line = vec![
            Span::styled("     ", Style::default()),
            Span::styled(masked, input_style),
            Span::styled(cursor.to_string(), Style::default().fg(pink)),
        ];

        // Env-var indicator
        if field.from_env {
            input_line.push(Span::styled(
                "  [env]",
                Style::default()
                    .fg(Color::Rgb(160, 140, 60))
                    .add_modifier(Modifier::DIM),
            ));
        }

        // Validation status indicator
        if let Some(ref status) = field.validation_status {
            match status {
                Ok(()) => {
                    input_line.push(Span::styled(
                        "  \u{2713}",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                Err(reason) => {
                    input_line.push(Span::styled(
                        "  \u{2717}",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                    if active {
                        let short = if reason.len() > 20 {
                            format!("{}…", &reason[..20])
                        } else {
                            reason.clone()
                        };
                        input_line.push(Span::styled(
                            format!(" {}", short),
                            Style::default().fg(Color::Rgb(255, 100, 100)),
                        ));
                    }
                }
            }
        }

        lines.push(Line::from(input_line));
    }

    if end < visible_count {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2193} {} more", visible_count - end),
            Style::default().fg(dim),
        )]));
    }

    // Show-all / collapse toggle when there are collapsed upstreams
    if !state.show_all && state.collapsed_count() > 0 {
        lines.push(Line::from(vec![Span::styled(
            "   [tab] show all upstreams",
            Style::default()
                .fg(Color::Rgb(100, 100, 140))
                .add_modifier(Modifier::DIM),
        )]));
    } else if state.show_all {
        lines.push(Line::from(vec![Span::styled(
            "   [tab] show configured only",
            Style::default()
                .fg(Color::Rgb(100, 100, 140))
                .add_modifier(Modifier::DIM),
        )]));
    }

    lines.push(Line::from(""));

    // Footer
    lines.push(Line::from(vec![
        Span::styled(" \u{2191}/\u{2193}", Style::default().fg(dim)),
        Span::styled(" next   ", Style::default().fg(dim)),
        Span::styled("ctrl+d", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" toggle on/off   ", Style::default().fg(dim)),
        Span::styled("tab", Style::default().fg(Color::Rgb(140, 140, 160))),
        Span::styled(" collapsed   ", Style::default().fg(dim)),
        Span::styled(confirm_hint, Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hidden() {
        let s = FreeModeDialogState::new();
        assert!(!s.visible);
        assert_eq!(s.fields.len(), FREE_CATALOG.len());
    }

    #[test]
    fn open_starts_on_first_empty_field() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(s.visible);
        // All fields are empty and collapsed; open() falls back to index 0.
        assert_eq!(s.active_idx, 0);
    }

    #[test]
    fn open_seeds_existing_keys_and_shows_only_configured() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, "existing-key".to_string())]);
        assert_eq!(s.fields[0].key, "existing-key");
        // Field 0 has a key (not collapsed). Other fields are collapsed.
        // visible = [0]; no empty visible fields, so active_idx = visible[0] = 0.
        assert_eq!(s.active_idx, 0);
        // Collapsed upstreams are hidden.
        assert!(
            !s.fields[0].collapsed,
            "configured field should be expanded"
        );
        assert!(s.fields[1].collapsed, "empty field should be collapsed");
    }

    #[test]
    fn open_with_show_all_shows_all_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all();
        assert!(s.show_all);
        assert_eq!(s.visible_field_indices().len(), s.fields.len());
    }

    #[test]
    fn move_next_wraps_within_visible() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // All fields visible
        let n = s.fields.len();
        s.active_idx = n - 1;
        s.move_next();
        assert_eq!(s.active_idx, 0, "should wrap to first field");
    }

    #[test]
    fn move_prev_wraps_within_visible() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // All fields visible
        s.active_idx = 0;
        s.move_prev();
        assert_eq!(
            s.active_idx,
            s.fields.len() - 1,
            "should wrap to last field"
        );
    }

    #[test]
    fn move_next_skips_collapsed_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[
            (FREE_CATALOG[0].id, "k1".into()),
            (FREE_CATALOG[2].id, "k3".into()),
        ]);
        // Only fields 0 and 2 are expanded (have keys).
        let visible = s.visible_field_indices();
        assert_eq!(
            visible,
            vec![0, 2],
            "only configured fields should be visible"
        );
        // active_idx = first empty visible field → none with keys → first visible = 0
        assert_eq!(s.active_idx, 0);
        s.move_next();
        assert_eq!(s.active_idx, 2, "should skip to field 2 (next visible)");
        s.move_next();
        assert_eq!(s.active_idx, 0, "should wrap to first visible");
    }

    #[test]
    fn toggle_show_all_expands_all_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        // Initially all collapsed.
        assert_eq!(s.visible_field_indices().len(), 0);
        s.toggle_show_all();
        assert_eq!(s.visible_field_indices().len(), s.fields.len());
    }

    #[test]
    fn collapsed_count_reflects_empty_fields() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert_eq!(s.collapsed_count(), s.fields.len());
        s.open(&[(FREE_CATALOG[0].id, "k1".into())]);
        assert_eq!(s.collapsed_count(), s.fields.len() - 1);
    }

    #[test]
    fn insert_and_backspace_target_active_field() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char('a');
        s.insert_char('b');
        assert_eq!(s.fields[0].key, "ab");
        s.backspace();
        assert_eq!(s.fields[0].key, "a");
    }

    #[test]
    fn can_submit_requires_at_least_one_key() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(!s.can_submit());
        s.insert_char('k');
        assert!(s.can_submit());
    }

    #[test]
    fn take_values_returns_only_non_empty_trimmed_pairs_and_does_not_close() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.toggle_show_all(); // Show all fields so move_next works
        s.insert_char(' ');
        s.insert_char('a');
        s.insert_char(' ');
        s.move_next();
        s.insert_char('b');
        let values = s.take_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], (FREE_CATALOG[0].id, "a".to_string()));
        assert_eq!(values[1], (FREE_CATALOG[1].id, "b".to_string()));
        // take_values no longer closes — caller is responsible.
        assert!(s.visible);
        s.close();
        assert!(!s.visible);
    }

    #[test]
    fn mask_key_hides_all_but_last_four() {
        assert_eq!(mask_key(""), "paste your API key here...");
        assert_eq!(mask_key("abc"), "abc");
        assert_eq!(mask_key("abcdefgh"), "\u{2022}\u{2022}\u{2022}\u{2022}efgh");
    }
}
