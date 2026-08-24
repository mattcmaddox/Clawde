// context_viz.rs — Context window and rate-limit visualization overlay.
// Triggered by /ctx-viz (or /context). Shows horizontal progress bars
// and a FreeProvider key health table.
// Data hooks from the query loop are wired via app.context_used_tokens,
// app.context_window_size, app.messages, and app.key_ring_data_fn —
// all updated per-turn.

use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::Frame;

use clawde_tools::web_search::{collect_firecrawl_keys, firecrawl_key_health};

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, render_scrollbar,
    CLAWDE_ACCENT, CLAWDE_MUTED, CLAWDE_PANEL_BG,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One row in the FreeProvider key health table.
#[derive(Debug, Clone)]
pub struct KeyRingRow {
    /// Display name (e.g. "Groq", "Cerebras").
    pub provider_name: String,
    /// How many keys are currently active.
    pub active: usize,
    /// Total configured keys.
    pub total: usize,
    /// Seconds until the earliest exhausted key recovers, if any.
    pub retry_secs: Option<u64>,
    /// HTTP rate limit: tokens usage fraction (0.0–1.0), or None if unavailable.
    pub tokens_pct: Option<f32>,
    /// HTTP rate limit: requests usage fraction (0.0–1.0), or None if unavailable.
    pub requests_pct: Option<f32>,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ContextVizState {
    pub visible: bool,
    /// Vertical scroll offset (in wrapped rows) for the modal body. Clamped
    /// against the rendered content height at draw time (render_app holds an
    /// immutable `&App`, matching the help-overlay pattern), so this value may
    /// temporarily exceed the max while content is short — it is never drawn
    /// out of bounds.
    pub scroll_offset: usize,
}

impl ContextVizState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self) {
        self.visible = true;
        self.scroll_offset = 0;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.scroll_offset = 0;
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
        if !self.visible {
            self.scroll_offset = 0;
        }
    }

    pub fn scroll_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(1);
    }

    pub fn page_up(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_sub(8);
    }

    pub fn page_down(&mut self) {
        self.scroll_offset = self.scroll_offset.saturating_add(8);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn scroll_to_bottom(&mut self, max: usize) {
        self.scroll_offset = max.saturating_sub(1);
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_context_viz(
    frame: &mut Frame,
    state: &ContextVizState,
    area: Rect,
    context_used: u64,
    context_total: u64,
    key_ring_rows: Vec<KeyRingRow>,
    cost_usd: f64,
    msg_total: usize,
    msg_user: usize,
    msg_assistant: usize,
    tool_calls: usize,
    free_model_defaults: Vec<(String, String, String)>,
    // Per-upstream key-ring health `(upstream_id, active, total, retry_secs)`
    // for the free provider — drives the status dot in the Free models table.
    free_upstream_health: Vec<(String, usize, usize, Option<u64>)>,
    // Per-upstream cooldown annotations `(upstream_id, kind, retry_secs)`
    // where `kind` is `"empty"` or `"5xx"`.
    free_upstream_cooldowns: Vec<(String, String, Option<u64>)>,
    // Active free-model task sort — rendered as a status line so the user can
    // see /models is pre-sorted by task (hidden when the default `All`).
    free_task_sort: crate::model_picker::FreeTask,
) {
    if !state.visible {
        return;
    }

    let layout = begin_modal_frame(frame, area, 72, 32, 2, 1);
    render_modal_title_frame(frame, layout.header_area, "Context & usage", "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                " Token window, key health, and session cost.",
                Style::default().fg(CLAWDE_MUTED),
            )])),
            subtitle_area,
        );
    }
    let inner = layout.body_area;

    // bar_width: leave room for "  label  [" prefix (14 chars) and "] 100%" suffix (6 chars)
    let bar_width = (inner.width as usize).saturating_sub(22).max(4);

    let ctx_pct = if context_total > 0 {
        (context_used as f32 / context_total as f32).min(1.0)
    } else {
        0.0
    };
    let ctx_color = if ctx_pct > 0.95 {
        Color::Red
    } else if ctx_pct > 0.80 {
        Color::Yellow
    } else {
        Color::Green
    };

    let ctx_warning = if ctx_pct > 0.95 {
        Some(Span::styled(
            " \u{26a0} CRITICAL — consider compacting or starting a new session",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ))
    } else if ctx_pct > 0.80 {
        Some(Span::styled(
            " \u{26a0} High usage — compact soon to free space",
            Style::default().fg(Color::Yellow),
        ))
    } else {
        None
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // -- Context window ----------------------------------------------------------
    lines.push(Line::from(vec![Span::styled(
        " Context window",
        Style::default()
            .fg(CLAWDE_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));

    let filled = ((ctx_pct * bar_width as f32) as usize).min(bar_width);
    let empty = bar_width - filled;
    lines.push(Line::from(vec![
        Span::styled(" [", Style::default().fg(CLAWDE_MUTED)),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(ctx_color)),
        Span::styled("\u{2591}".repeat(empty), Style::default().fg(CLAWDE_MUTED)),
        Span::styled(
            format!(
                "]  {:.0}%  ({} / {})",
                ctx_pct * 100.0,
                format_tokens(context_used),
                format_tokens(context_total),
            ),
            Style::default().fg(ctx_color),
        ),
    ]));

    if let Some(warning) = ctx_warning {
        lines.push(Line::from(vec![warning]));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(""));

    // -- Free models ----------------------------------------------------------
    if !free_model_defaults.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " Free models",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        // Index upstream key-health by id so the status dot can be drawn.
        let health_map: HashMap<&str, (usize, usize, Option<u64>)> = free_upstream_health
            .iter()
            .map(|(id, active, total, retry)| (id.as_str(), (*active, *total, *retry)))
            .collect();
        // Index upstream cooldown annotations by id: (kind, retry_secs).
        let mut cooldown_map: HashMap<&str, Vec<(&str, u64)>> = HashMap::new();
        for (id, kind, retry) in &free_upstream_cooldowns {
            if let Some(secs) = retry {
                cooldown_map
                    .entry(id.as_str())
                    .or_default()
                    .push((kind.as_str(), *secs));
            }
        }

        for (position, (upstream_id, upstream_name, model_id)) in
            free_model_defaults.iter().enumerate()
        {
            // Truncate long model IDs using display width (not byte length)
            // and char-safe slicing so multi-byte characters aren't split.
            let truncated = if unicode_width::UnicodeWidthStr::width(model_id.as_str()) > 32 {
                let chars: Vec<char> = model_id.chars().collect();
                let head: String = chars.iter().take(15).collect();
                let tail: String = chars
                    .iter()
                    .rev()
                    .take(15)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                format!("{}…{}", head, tail)
            } else {
                model_id.clone()
            };
            let (dot, dot_color) = upstream_dot(upstream_id, &health_map, &cooldown_map);
            let mut spans = vec![
                Span::styled(format!(" {} ", dot), Style::default().fg(dot_color)),
                Span::styled(
                    format!("{:<16}", truncate_name(upstream_name, 16)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("  {}", truncated),
                    Style::default().fg(Color::Green),
                ),
            ];
            // Persistent cooldown annotations — the /ctx-viz equivalent of
            // the transient status-row badges, so users can see *why* an
            // upstream is being skipped.
            if let Some(annotations) = cooldown_map.get(upstream_id.as_str()) {
                for (kind, secs) in annotations {
                    spans.push(Span::styled(
                        format!(" ({}-cooldown {}s)", kind, secs),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
            lines.push(Line::from(spans));

            // Detail line: fallback-chain priority and key-ring state, so
            // users can see which upstream is tried first and how healthy its
            // keys are without opening /keys health.
            let keys_detail = match health_map.get(upstream_id.as_str()) {
                Some((active, total, retry)) => {
                    let retry_part = if *active == 0 && *total > 0 {
                        match retry {
                            Some(r) => format!(", retry {}s", r),
                            None => String::new(),
                        }
                    } else {
                        String::new()
                    };
                    format!("keys {}/{}", active, total) + &retry_part
                }
                None => "keys \u{2014}".to_string(),
            };
            // Fallback models (secondary models tried on the same upstream
            // before the chain moves on) — surfaced so users can see which
            // providers auto-recover from a slow primary.
            let fallback_note = clawde_api::FREE_CATALOG
                .iter()
                .find(|u| u.id == upstream_id)
                .map(|u| u.fallback_models)
                .unwrap_or(&[]);
            let mut detail = format!("     priority #{} \u{00b7} {}", position + 1, keys_detail);
            if !fallback_note.is_empty() {
                // Truncate each fallback ID like the primary model above so
                // the note can't overflow the modal on narrow terminals.
                let fb_list: Vec<String> = fallback_note
                    .iter()
                    .map(|m| {
                        if unicode_width::UnicodeWidthStr::width(*m) > 32 {
                            let chars: Vec<char> = m.chars().collect();
                            let head: String = chars.iter().take(15).collect();
                            let tail: String = chars
                                .iter()
                                .rev()
                                .take(15)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect();
                            format!("{}…{}", head, tail)
                        } else {
                            (*m).to_string()
                        }
                    })
                    .collect();
                detail.push_str(" \u{00b7} fb: ");
                detail.push_str(&fb_list.join(", "));
            }
            lines.push(Line::from(vec![Span::styled(
                detail,
                Style::default().fg(CLAWDE_MUTED),
            )]));
        }

        lines.push(Line::from(""));
    }

    // -- Key health ------------------------------------------------------------
    if !key_ring_rows.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " Key health",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        // Column header
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:<16} {:>5} {:>5} {:>5} {:>7}",
                "Provider", "Keys", "Tok%", "Req%", "Retry"
            ),
            Style::default().fg(CLAWDE_MUTED),
        )]));

        for row in &key_ring_rows {
            let keys_color = if row.total == 0 {
                CLAWDE_MUTED
            } else if row.active == 0 {
                Color::Red
            } else if row.active < row.total {
                Color::Yellow
            } else {
                Color::Green
            };

            let keys_text = if row.total == 0 {
                "\u{2014}".to_string()
            } else {
                format!("{}/{}", row.active, row.total)
            };

            let tok_text = match row.tokens_pct {
                Some(p) => format!("{:.0}%", p * 100.0),
                None => "\u{2014}".to_string(),
            };
            let tok_color = pct_color(row.tokens_pct);

            let req_text = match row.requests_pct {
                Some(p) => format!("{:.0}%", p * 100.0),
                None => "\u{2014}".to_string(),
            };
            let req_color = pct_color(row.requests_pct);

            let retry_text = match row.retry_secs {
                Some(s) if row.active == 0 && row.total > 0 => format!("{}s", s),
                _ => "\u{2014}".to_string(),
            };

            let row_style = if row.total == 0 {
                Style::default().fg(CLAWDE_MUTED)
            } else {
                Style::default().fg(Color::White)
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<16}", truncate_name(&row.provider_name, 16)),
                    row_style,
                ),
                Span::styled(
                    format!(" {:>4} ", keys_text),
                    Style::default().fg(keys_color),
                ),
                Span::styled(format!("{:>4} ", tok_text), Style::default().fg(tok_color)),
                Span::styled(format!("{:>4} ", req_text), Style::default().fg(req_color)),
                Span::styled(format!("{:>7}", retry_text), row_style),
            ]));
        }
    }

    // -- Free model sort -------------------------------------------------------
    if free_task_sort != crate::model_picker::FreeTask::All {
        lines.push(Line::from(vec![Span::styled(
            " Free model sort",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(vec![
            Span::styled("  Task: ", Style::default().fg(Color::White)),
            Span::styled(
                free_task_sort.label(),
                Style::default()
                    .fg(free_task_sort.color())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (rows pre-sorted; 1-7 jumps in /models)",
                Style::default().fg(CLAWDE_MUTED),
            ),
        ]));
        lines.push(Line::from(""));
    }

    // -- GitHub API ------------------------------------------------------------
    if let Some(gh) = clawde_core::github::last_rate_limit() {
        lines.push(Line::from(vec![Span::styled(
            " GitHub API",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        let gh_color = if gh.remaining == 0 {
            Color::Red
        } else if gh.remaining <= 5 {
            Color::Yellow
        } else {
            Color::Green
        };
        let reset_text =
            clawde_core::github::format_reset(gh.reset_unix, clawde_core::github::unix_now());

        lines.push(Line::from(vec![
            Span::styled("  Requests: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{} / {}", gh.remaining, gh.limit),
                Style::default().fg(gh_color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ({reset_text})"),
                Style::default().fg(CLAWDE_MUTED),
            ),
        ]));

        lines.push(Line::from(""));
    }

    // -- Firecrawl keys ---------------------------------------------------------
    let fc_keys = collect_firecrawl_keys();
    if !fc_keys.is_empty() {
        let fc_health = firecrawl_key_health();
        let exhausted: std::collections::HashMap<&str, u64> = fc_health
            .iter()
            .filter(|(_, active, _)| !active)
            .map(|(key_id, _, remaining)| (key_id.as_str(), *remaining))
            .collect();

        lines.push(Line::from(vec![Span::styled(
            " Firecrawl keys",
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        lines.push(Line::from(vec![Span::styled(
            format!("  {:<16} {:>5} {:>7}", "Key", "Status", "Retry"),
            Style::default().fg(CLAWDE_MUTED),
        )]));

        for key in fc_keys.iter() {
            let preview = clawde_tools::web_search::firecrawl_key_label(key);
            let key_id = clawde_tools::web_search::firecrawl_key_fingerprint(key);

            if let Some(&remaining) = exhausted.get(key_id.as_str()) {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:<16}", truncate_name(&preview, 16)),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(" EXHAU", Style::default().fg(Color::Red)),
                    Span::styled(
                        format!(" {:>6}s", remaining),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("  {:<16}", truncate_name(&preview, 16)),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled(" ACTIVE", Style::default().fg(Color::Green)),
                    Span::styled(
                        format!(" {:>6}", "\u{2014}"),
                        Style::default().fg(CLAWDE_MUTED),
                    ),
                ]));
            }
        }
        lines.push(Line::from(""));
    }

    // -- Messages ---------------------------------------------------------------
    lines.push(Line::from(vec![Span::styled(
        " Messages",
        Style::default()
            .fg(CLAWDE_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));

    lines.push(Line::from(vec![
        Span::styled("  Total:     ", Style::default().fg(Color::White)),
        Span::styled(
            msg_total.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  User:      ", Style::default().fg(Color::White)),
        Span::styled(msg_user.to_string(), Style::default()),
        Span::styled("  Assistant: ", Style::default().fg(Color::White)),
        Span::styled(msg_assistant.to_string(), Style::default()),
        Span::styled("  Tool calls: ", Style::default().fg(Color::White)),
        Span::styled(tool_calls.to_string(), Style::default()),
    ]));

    lines.push(Line::from(""));

    // -- Cost --------------------------------------------------------------------
    // Free sessions price at $0.00 — hide the readout entirely when zero.
    if cost_usd > 0.0 {
        lines.push(Line::from(vec![
            Span::styled(" Session cost:  ", Style::default().fg(Color::White)),
            Span::styled(
                format!("${:.4}", cost_usd),
                Style::default()
                    .fg(CLAWDE_ACCENT)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    // Total wrapped rows (long lines wrap inside the modal), so the scroll
    // offset can be clamped and a scrollbar shown when the body overflows —
    // e.g. long free-model chains with per-upstream detail lines would
    // otherwise push the key-health / cost sections out of view.
    // (`Paragraph::line_count` is an unstable ratatui API, so wrap each line
    // by its unicode width, matching the `Wrap { trim: false }` behaviour.)
    let inner_width = (inner.width as usize).max(1);
    let total_rows: usize = lines
        .iter()
        .map(|line| {
            let w = line.width();
            if w == 0 {
                1
            } else {
                w.div_ceil(inner_width)
            }
        })
        .sum();
    let max_scroll = total_rows.saturating_sub(inner.height as usize);
    let scroll = state.scroll_offset.min(max_scroll);

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll as u16, 0))
        .style(Style::default().bg(CLAWDE_PANEL_BG))
        .render(inner, frame.buffer_mut());

    // Thin vertical scrollbar along the body's right edge when content
    // overflows (reuses the shared overlay scrollbar helper).
    if max_scroll > 0 {
        render_scrollbar(
            frame,
            &crate::theme_colors::current_palette(),
            inner,
            scroll,
            total_rows,
            inner.height as usize,
        );
    }

    let footer_hint = if max_scroll > 0 {
        format!(
            " \u{2191}\u{2193}/j/k/pgup/pgdn scroll \u{00b7} {}/{} \u{00b7} enter/esc close",
            scroll + 1,
            total_rows
        )
    } else {
        " enter/esc close".to_string()
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            footer_hint,
            Style::default()
                .fg(CLAWDE_MUTED)
                .add_modifier(Modifier::ITALIC),
        )])),
        layout.footer_area,
    );
}

/// Health dot for one free-mode upstream:
///   ● green  — healthy (no cooldown, keys active / no key ring)
///   ◐ yellow — some keys exhausted
///   ○ red    — all keys down, or a 5xx / empty cooldown is active
fn upstream_dot(
    id: &str,
    health: &HashMap<&str, (usize, usize, Option<u64>)>,
    cooldowns: &HashMap<&str, Vec<(&str, u64)>>,
) -> (char, Color) {
    if cooldowns.contains_key(id) {
        return ('\u{25cb}', Color::Red); // ○
    }
    match health.get(id) {
        Some((active, total, _)) if *total > 0 && *active == 0 => ('\u{25cb}', Color::Red),
        Some((active, total, _)) if *total > 0 && *active < *total => ('\u{25d0}', Color::Yellow), // ◐
        _ => ('\u{25cf}', Color::Green), // ●
    }
}

/// Build `/ctx-viz` key-health rows from a provider registry. Shared by the
/// CLI's callback wiring and the TUI render path (which prefers the live app
/// registry so rows stay fresh after key / routing changes).
pub fn key_ring_rows_from_registry(reg: &clawde_api::ProviderRegistry) -> Vec<KeyRingRow> {
    reg.key_ring_summaries()
        .into_iter()
        .map(|(name, active, total, retry)| KeyRingRow {
            provider_name: name,
            active,
            total,
            retry_secs: retry,
            tokens_pct: None,
            requests_pct: None,
        })
        .collect()
}

fn pct_color(pct: Option<f32>) -> Color {
    match pct {
        Some(p) if p > 0.90 => Color::Red,
        Some(p) if p > 0.70 => Color::Yellow,
        Some(_) => Color::Green,
        None => CLAWDE_MUTED,
    }
}

fn truncate_name(name: &str, max: usize) -> String {
    if unicode_width::UnicodeWidthStr::width(name) <= max {
        name.to_string()
    } else {
        let mut result = String::with_capacity(max + 1);
        let mut width = 0usize;
        for ch in name.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
            if width + cw > max.saturating_sub(1) {
                result.push('\u{2026}');
                break;
            }
            width += cw;
            result.push(ch);
        }
        result
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn context_viz_defaults_hidden() {
        let state = ContextVizState::new();
        assert!(!state.visible);
    }

    #[test]
    fn context_viz_toggle() {
        let mut state = ContextVizState::new();
        state.toggle();
        assert!(state.visible);
        state.toggle();
        assert!(!state.visible);
    }

    #[test]
    fn context_viz_renders_without_panic() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = ContextVizState::new();
        state.open();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state,
                    frame.area(),
                    50_000,
                    200_000,
                    vec![KeyRingRow {
                        provider_name: "Groq".into(),
                        active: 2,
                        total: 3,
                        retry_secs: None,
                        tokens_pct: Some(0.23),
                        requests_pct: None,
                    }],
                    0.42,
                    12,
                    4,
                    5,
                    3,
                    vec![("groq".into(), "Groq".into(), "llama-3.3-70b".into())],
                    vec![("groq".into(), 2, 3, Some(30))],
                    vec![("groq".into(), "empty".into(), Some(42))],
                    crate::model_picker::FreeTask::All,
                );
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
        assert!(content.contains("Context") || content.contains("Key"));
    }

    #[test]
    fn context_viz_shows_fallback_models_for_upstreams_that_have_them() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = ContextVizState::new();
        state.open();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state,
                    frame.area(),
                    50_000,
                    200_000,
                    Vec::new(),
                    0.0,
                    0,
                    0,
                    0,
                    0,
                    // nvidia has a catalog fallback; groq does not.
                    vec![
                        (
                            "nvidia".into(),
                            "NVIDIA NIM".into(),
                            "openai/gpt-oss-120b".into(),
                        ),
                        ("groq".into(), "Groq".into(), "openai/gpt-oss-120b".into()),
                    ],
                    vec![("nvidia".into(), 2, 2, None), ("groq".into(), 2, 2, None)],
                    Vec::new(),
                    crate::model_picker::FreeTask::All,
                );
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
        // The fallback note appears on nvidia's detail line exactly once
        // (flat buffer — no newlines between rows).
        assert!(
            content.contains("fb: openai/gpt-oss-20b"),
            "expected fallback note, got: {:?}",
            &content[content.find("Free").unwrap_or(0)..]
        );
        // groq has no fallbacks — its detail line must not carry a fb note.
        assert_eq!(
            content.matches("fb:").count(),
            1,
            "expected exactly one fb note (nvidia only), got: {:?}",
            &content[content.find("Free").unwrap_or(0)..]
        );
    }

    #[test]
    fn context_viz_scroll_state_resets_on_open_close() {
        let mut state = ContextVizState::new();
        state.open();
        state.scroll_down();
        state.scroll_down();
        assert_eq!(state.scroll_offset, 2);
        state.scroll_up();
        assert_eq!(state.scroll_offset, 1);
        state.close();
        assert!(!state.visible);
        assert_eq!(state.scroll_offset, 0);
        // Reopening starts at the top again.
        state.open();
        state.scroll_to_bottom(usize::MAX);
        assert_eq!(state.scroll_offset, usize::MAX - 1);
        state.scroll_to_top();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn context_viz_overflow_renders_scrollable() {
        // A long free-model chain (10 upstreams with per-upstream detail
        // lines) overflows the modal body on a 30-row terminal. Rendering with
        // a scroll offset must not panic and the footer must advertise
        // scrolling so the user knows lower sections are reachable.
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = ContextVizState::new();
        state.open();
        state.scroll_down();
        state.scroll_down();
        let upstreams: Vec<(String, String, String)> = (0..10)
            .map(|i| {
                (
                    format!("upstream-{}", i),
                    format!("Upstream {}", i),
                    format!("model-{}", i),
                )
            })
            .collect();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state,
                    frame.area(),
                    50_000,
                    200_000,
                    vec![],
                    0.0,
                    0,
                    0,
                    0,
                    0,
                    upstreams.clone(),
                    vec![],
                    vec![],
                    crate::model_picker::FreeTask::All,
                );
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
        // Overflowing content shows the scroll hint in the footer.
        assert!(
            content.contains("scroll"),
            "footer should advertise scrolling when the body overflows"
        );
    }

    #[test]
    fn context_viz_shows_free_task_sort_when_not_all() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = ContextVizState::new();
        state.open();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state,
                    frame.area(),
                    50_000,
                    200_000,
                    Vec::new(),
                    0.0,
                    0,
                    0,
                    0,
                    0,
                    vec![],
                    vec![],
                    vec![],
                    crate::model_picker::FreeTask::Coding,
                );
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
        assert!(
            content.contains("Free model sort"),
            "ctx-viz must show the Free model sort section when a task is active"
        );
        assert!(content.contains("coding"));
        // The default All must NOT show the section.
        let mut state_all = ContextVizState::new();
        state_all.open();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state_all,
                    frame.area(),
                    50_000,
                    200_000,
                    Vec::new(),
                    0.0,
                    0,
                    0,
                    0,
                    0,
                    vec![],
                    vec![],
                    vec![],
                    crate::model_picker::FreeTask::All,
                );
            })
            .unwrap();
        let content_all: String = terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            !content_all.contains("Free model sort"),
            "ctx-viz must hide the sort section when the task is All"
        );
    }

    #[test]
    fn context_viz_hidden_renders_nothing() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let state = ContextVizState::new();
        let before = terminal.backend().buffer().clone();
        terminal
            .draw(|frame| {
                render_context_viz(
                    frame,
                    &state,
                    frame.area(),
                    0,
                    0,
                    vec![],
                    0.0,
                    0,
                    0,
                    0,
                    0,
                    vec![],
                    vec![],
                    vec![],
                    crate::model_picker::FreeTask::All,
                );
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer().content(), before.content());
    }
}
