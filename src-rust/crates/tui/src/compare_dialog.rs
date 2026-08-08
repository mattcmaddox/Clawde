// compare_dialog.rs — live smart-router comparison view (`/compare`).

use clawde_api::{build_compare_report, CompareFilters, CompareReport, ProviderRegistry};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

#[derive(Debug, Default)]
pub struct CompareDialogState {
    pub visible: bool,
    pub task_filter: Option<String>,
    pub provider_filter: Option<String>,
    report: Option<CompareReport>,
    selected: usize,
}

pub fn parse_compare_filters(args: &str) -> Result<(Option<String>, Option<String>), String> {
    let filters = clawde_api::parse_compare_args(args)?;
    Ok((filters.task, filters.provider))
}

impl CompareDialogState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(
        &mut self,
        registry: Option<&ProviderRegistry>,
        task_filter: Option<String>,
        provider_filter: Option<String>,
    ) {
        self.task_filter = task_filter;
        self.provider_filter = provider_filter;
        self.report = registry.map(|registry| {
            build_compare_report(
                registry,
                CompareFilters {
                    task: self.task_filter.clone(),
                    provider: self.provider_filter.clone(),
                },
            )
        });
        self.selected = 0;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.report = None;
    }

    pub fn select_next(&mut self) {
        if let Some(report) = &self.report {
            if !report.rows.is_empty() {
                self.selected = (self.selected + 1).min(report.rows.len() - 1);
            }
        }
    }

    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
}

fn centered_rect(area: Rect) -> Rect {
    let height = area.height.min(21);
    let width = area.width.saturating_mul(84) / 100;
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

fn format_rate(value: Option<f64>) -> String {
    value
        .map(|rate| format!("{:>5.1}%", rate * 100.0))
        .unwrap_or_else(|| "  n/a".to_string())
}

fn format_latency(value: Option<f64>) -> String {
    value
        .map(|seconds| {
            if seconds < 1.0 {
                format!("{:>4.0}ms", seconds * 1000.0)
            } else {
                format!("{:>4.1}s", seconds)
            }
        })
        .unwrap_or_else(|| "  n/a".to_string())
}

pub fn render_compare_dialog(frame: &mut Frame, state: &CompareDialogState, area: Rect) {
    let dialog = centered_rect(area);
    frame.render_widget(Clear, dialog);
    let scope = match (&state.task_filter, &state.provider_filter) {
        (Some(task), Some(provider)) => format!("task={}, provider={}", task, provider),
        (Some(task), None) => format!("task={}", task),
        (None, Some(provider)) => format!("provider={}", provider),
        (None, None) => "all tasks".to_string(),
    };
    let block = Block::default()
        .title(format!(" Compare free upstreams · {} ", scope))
        .title_bottom(" j/k or ↑/↓ select · r refresh · Esc close ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Rgb(150, 120, 210)));
    let inner = block.inner(dialog);
    frame.render_widget(block, dialog);

    let mut lines = vec![Line::from(Span::styled(
        "Rank  Upstream          Success  Latency  Calls  Health",
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let Some(report) = &state.report else {
        lines.push(Line::from("No live provider registry is available."));
        lines.push(Line::from("Start free mode, then open /compare again."));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
        return;
    };
    if report.rows.is_empty() {
        lines.push(Line::from(
            "No recorded free-provider dispatches match this filter.",
        ));
        lines.push(Line::from(
            "Run a few free-mode requests, then press r to refresh.",
        ));
    } else {
        let max_rows = inner.height.saturating_sub(1) as usize;
        let start = state
            .selected
            .saturating_sub(max_rows.saturating_sub(1))
            .min(report.rows.len().saturating_sub(max_rows));
        for (index, row) in report.rows.iter().enumerate().skip(start).take(max_rows) {
            let selected = index == state.selected;
            let rate = row.task_success_rate.or(row.success_rate);
            let health = row
                .cooldown
                .clone()
                .or_else(|| {
                    row.key_health.map(|(active, total, retry)| {
                        retry.map_or(format!("keys {active}/{total}"), |seconds| {
                            format!("keys {active}/{total} · {seconds}s")
                        })
                    })
                })
                .unwrap_or_else(|| "ready".to_string());
            let line = Line::from(vec![
                Span::styled(
                    format!("{:>4}  ", index + 1),
                    if selected {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("{:<16} ", row.upstream),
                    if selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(format!(
                    "{:>7}  {:>7}  {:>5}  {}",
                    format_rate(rate),
                    format_latency(row.latency_secs),
                    row.dispatches,
                    health
                )),
            ]);
            lines.push(if selected {
                line.style(Style::default().bg(Color::Rgb(66, 58, 96)))
            } else {
                line
            });
        }
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
