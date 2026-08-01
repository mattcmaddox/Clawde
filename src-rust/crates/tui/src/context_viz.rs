// context_viz.rs — Context window and rate-limit visualization overlay.
// Triggered by /ctx-viz (or /context). Shows horizontal progress bars
// and a FreeProvider key health table.
// Data hooks from the query loop are wired via app.context_used_tokens,
// app.context_window_size, app.messages, and app.key_ring_data_fn —
// all updated per-turn.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use ratatui::Frame;

use clawde_tools::web_search::{collect_firecrawl_keys, firecrawl_key_health};

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, CLAURST_ACCENT,
    CLAURST_MUTED, CLAURST_PANEL_BG,
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
}

impl ContextVizState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self) {
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn toggle(&mut self) {
        self.visible = !self.visible;
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
    free_model_defaults: Vec<(String, String)>,
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
                Style::default().fg(CLAURST_MUTED),
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
            .fg(CLAURST_ACCENT)
            .add_modifier(Modifier::BOLD),
    )]));

    let filled = ((ctx_pct * bar_width as f32) as usize).min(bar_width);
    let empty = bar_width - filled;
    lines.push(Line::from(vec![
        Span::styled(" [", Style::default().fg(CLAURST_MUTED)),
        Span::styled("\u{2588}".repeat(filled), Style::default().fg(ctx_color)),
        Span::styled("\u{2591}".repeat(empty), Style::default().fg(CLAURST_MUTED)),
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
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        for (upstream_name, model_id) in &free_model_defaults {
            let truncated = if model_id.len() > 32 {
                format!("{}…{}", &model_id[..15], &model_id[model_id.len() - 15..])
            } else {
                model_id.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<16}", truncate_name(upstream_name, 16)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("  {}", truncated),
                    Style::default().fg(Color::Green),
                ),
            ]));
        }

        lines.push(Line::from(""));
    }

    // -- Key health ------------------------------------------------------------
    if !key_ring_rows.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            " Key health",
            Style::default()
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        // Column header
        lines.push(Line::from(vec![Span::styled(
            format!(
                "  {:<16} {:>5} {:>5} {:>5} {:>7}",
                "Provider", "Keys", "Tok%", "Req%", "Retry"
            ),
            Style::default().fg(CLAURST_MUTED),
        )]));

        for row in &key_ring_rows {
            let keys_color = if row.total == 0 {
                CLAURST_MUTED
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
                Style::default().fg(CLAURST_MUTED)
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

    lines.push(Line::from(""));

    // -- Firecrawl keys ---------------------------------------------------------
    let fc_keys = collect_firecrawl_keys();
    if !fc_keys.is_empty() {
        let fc_health = firecrawl_key_health();
        let exhausted: std::collections::HashMap<&str, u64> = fc_health
            .iter()
            .filter(|(_, active, _)| !active)
            .map(|(k, _, remaining)| (k.as_str(), *remaining))
            .collect();

        lines.push(Line::from(vec![Span::styled(
            " Firecrawl keys",
            Style::default()
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        )]));

        lines.push(Line::from(vec![Span::styled(
            format!("  {:<16} {:>5} {:>7}", "Key", "Status", "Retry"),
            Style::default().fg(CLAURST_MUTED),
        )]));

        for key in fc_keys.iter() {
            let preview = if key.len() > 16 {
                format!("{}..{}", &key[..8], &key[key.len() - 4..])
            } else {
                key.clone()
            };

            if let Some(&remaining) = exhausted.get(key.as_str()) {
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
                        Style::default().fg(CLAURST_MUTED),
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
            .fg(CLAURST_ACCENT)
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
    lines.push(Line::from(vec![
        Span::styled(" Session cost:  ", Style::default().fg(Color::White)),
        Span::styled(
            format!("${:.4}", cost_usd),
            Style::default()
                .fg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(CLAURST_PANEL_BG))
        .render(inner, frame.buffer_mut());
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " enter/esc close",
            Style::default()
                .fg(CLAURST_MUTED)
                .add_modifier(Modifier::ITALIC),
        )])),
        layout.footer_area,
    );
}

fn pct_color(pct: Option<f32>) -> Color {
    match pct {
        Some(p) if p > 0.90 => Color::Red,
        Some(p) if p > 0.70 => Color::Yellow,
        Some(_) => Color::Green,
        None => CLAURST_MUTED,
    }
}

fn truncate_name(name: &str, max: usize) -> String {
    if name.len() <= max {
        name.to_string()
    } else {
        let mut result = String::with_capacity(max + 1);
        for (count, ch) in name.chars().enumerate() {
            if count >= max.saturating_sub(1) {
                result.push('\u{2026}');
                break;
            }
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
                    vec![("Groq".into(), "llama-3.3-70b".into())],
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
                );
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer().content(), before.content());
    }
}
