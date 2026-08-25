//! Stats dialog — mirrors src/components/Stats.tsx
//!
//! Four-tab overlay: Overview | Daily Tokens | Cost Heatmap | Models
//! Data source: ~/.clawde/stats.jsonl (append-only per-turn usage log)

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::overlays::{
    begin_modal_buf, modal_header_line_area, render_modal_title_buf, CLAWDE_ACCENT, CLAWDE_MUTED,
    CLAWDE_PANEL_BG,
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single entry in ~/.clawde/stats.jsonl
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatsEntry {
    pub timestamp_ms: u64,
    pub session_id: Option<String>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache fields were added after the original stats format; missing values
    /// in older JSONL records are treated as zero.
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Cost in USD cents (f64)
    pub cost_cents: f64,
    pub project: Option<String>,
}

/// Aggregated stats for display.
#[derive(Debug, Clone, Default)]
pub struct AggregatedStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_cost_cents: f64,
    pub by_model: HashMap<String, ModelStats>,
    /// (date_str "YYYY-MM-DD", tokens) pairs sorted by date
    pub daily_tokens: Vec<(String, u64)>,
    /// (date_str "YYYY-MM-DD", cost_cents) for heatmap
    pub daily_costs: HashMap<String, f64>,
    pub peak_day: Option<String>,
    pub peak_day_tokens: u64,
}

/// Per-model usage stats (used in AggregatedStats.by_model).
#[derive(Debug, Clone, Default)]
pub struct ModelStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_cents: f64,
    pub turns: u64,
}

/// Per-model breakdown entry for the Models tab (cost in USD, not cents).
#[derive(Debug, Clone, Default)]
pub struct ModelBreakdown {
    pub model_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

/// Load and aggregate stats from ~/.clawde/stats.jsonl
pub fn load_stats() -> AggregatedStats {
    let path = clawde_core::config::Settings::config_dir().join("stats.jsonl");

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return AggregatedStats::default(),
    };

    let mut agg = AggregatedStats::default();
    let mut daily: HashMap<String, u64> = HashMap::new();

    for line in content.lines() {
        let Ok(entry) = serde_json::from_str::<StatsEntry>(line) else {
            continue;
        };

        accumulate_entry(&mut agg, &mut daily, &entry);
    }

    // Build sorted daily_tokens
    let mut daily_sorted: Vec<(String, u64)> = daily.into_iter().collect();
    daily_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    agg.peak_day = daily_sorted.iter().max_by_key(|d| d.1).map(|d| d.0.clone());
    agg.peak_day_tokens = daily_sorted.iter().map(|d| d.1).max().unwrap_or(0);
    agg.daily_tokens = daily_sorted;

    agg
}

fn accumulate_entry(
    agg: &mut AggregatedStats,
    daily: &mut HashMap<String, u64>,
    entry: &StatsEntry,
) {
    let total_tokens = entry.input_tokens + entry.output_tokens;
    agg.total_input_tokens += entry.input_tokens;
    agg.total_output_tokens += entry.output_tokens;
    agg.total_cache_read_tokens += entry.cache_read_tokens;
    agg.total_cache_write_tokens += entry.cache_write_tokens;
    agg.total_cost_cents += entry.cost_cents;

    let model_entry = agg.by_model.entry(entry.model.clone()).or_default();
    model_entry.input_tokens += entry.input_tokens;
    model_entry.output_tokens += entry.output_tokens;
    model_entry.cost_cents += entry.cost_cents;
    model_entry.turns += 1;

    let date = timestamp_to_date(entry.timestamp_ms);
    *daily.entry(date.clone()).or_insert(0) += total_tokens;
    *agg.daily_costs.entry(date).or_insert(0.0) += entry.cost_cents;
}

fn timestamp_to_date(ts_ms: u64) -> String {
    // Simple ISO date from Unix timestamp in ms
    let secs = ts_ms / 1000;
    let days_since_epoch = secs / 86400;
    // Rough Gregorian calendar calculation
    let year = 1970 + (days_since_epoch * 4 + 2) / 1461;
    let day_of_year = days_since_epoch - (year - 1970) * 365 - (year - 1970 - 1) / 4;
    let (month, day) = day_of_year_to_month_day(day_of_year as u32, is_leap_year(year as u32));
    format!("{:04}-{:02}-{:02}", year, month, day)
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn day_of_year_to_month_day(doy: u32, leap: bool) -> (u32, u32) {
    let months = if leap {
        [31u32, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31u32, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut remaining = doy;
    for (i, &m) in months.iter().enumerate() {
        if remaining < m {
            return (i as u32 + 1, remaining + 1);
        }
        remaining -= m;
    }
    (12, 31)
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsTab {
    Overview,
    DailyTokens,
    CostHeatmap,
    Models,
}

#[derive(Debug, Clone, PartialEq)]
struct ProviderHealthRow {
    label: String,
    active_keys: usize,
    total_keys: usize,
    retry_secs: Option<u64>,
    cooldowns: usize,
    /// Dispatch success rate 0.0–1.0 for this upstream (`None` = no
    /// dispatches yet), captured at open time for the model-performance
    /// fact-check alongside the key state.
    success_rate: Option<f64>,
    /// Sliding-window average dispatch latency in seconds (`None` = no
    /// samples yet).
    avg_latency: Option<f64>,
    /// Fresh capacity telemetry, when headers or an explicit local estimate
    /// exists. Missing and expired capacity remains absent rather than
    /// rendering as a misleading zero.
    capacity: Option<clawde_api::UpstreamCapacityStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderActivityRow {
    provider_id: String,
    upstream_id: Option<String>,
    model: String,
    requests: u32,
    total_elapsed_ms: u64,
    retries: u32,
    fallbacks: u32,
}

impl ProviderActivityRow {
    fn average_elapsed_ms(&self) -> u64 {
        self.total_elapsed_ms / u64::from(self.requests.max(1))
    }
}

#[derive(Debug, Clone)]
pub struct StatsDialogState {
    pub visible: bool,
    pub tab: StatsTab,
    pub range_days: u32, // 7, 30, or 0 = all
    pub data: Option<AggregatedStats>,
    pub scroll: u16,
    /// Per-model breakdown for the Models tab (cost in USD).
    pub model_breakdown: Vec<ModelBreakdown>,
    /// How many consecutive days the user has had activity (ending today).
    pub current_streak_days: u32,
    /// The longest streak ever recorded.
    pub longest_streak_days: u32,
    /// Live key-ring health captured when the dialog opens. This is intentionally
    /// not persisted: it reflects the currently active provider registry.
    live_provider_health: Vec<ProviderHealthRow>,
    /// Per-upstream live model probe picks (`upstream → last-picked model`)
    /// from the free discovery cache, refreshed when the dialog opens.
    live_probe_picks: Vec<(String, String)>,
    /// Bounded, session-local completion telemetry keyed by provider/model.
    provider_activity: Vec<ProviderActivityRow>,
}

impl StatsDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            tab: StatsTab::Overview,
            range_days: 30,
            data: None,
            scroll: 0,
            model_breakdown: Vec::new(),
            current_streak_days: 0,
            longest_streak_days: 0,
            live_provider_health: Vec::new(),
            live_probe_picks: Vec::new(),
            provider_activity: Vec::new(),
        }
    }

    /// Refresh live key health and cooldowns from the active provider registry.
    ///
    /// The registry already owns this state for routing and `/ctx-viz`; taking
    /// a snapshot here avoids inventing a second persistence pipeline for
    /// provider telemetry.
    pub fn refresh_provider_health(&mut self, registry: &clawde_api::ProviderRegistry) {
        let mut rows = Vec::new();
        let mut cooldown_counts = HashMap::new();

        for (provider, entries) in registry.upstream_cooldown_summaries() {
            for (upstream, _kind, _retry) in entries {
                *cooldown_counts
                    .entry(format!("{provider}/{upstream}"))
                    .or_insert(0usize) += 1;
            }
        }

        // Per-upstream model-performance snapshots keyed by `provider/upstream`
        // label so the key-health rows below can carry the fact-check columns
        // (does this upstream actually succeed, and how fast?). The registry
        // already owns this state for /routing edit; the same summary methods
        // feed both surfaces.
        let mut success_by_label: HashMap<String, Option<f64>> = HashMap::new();
        for (provider, entries) in registry.upstream_success_rate_summaries() {
            for (upstream, rate) in entries {
                success_by_label.insert(format!("{provider}/{upstream}"), rate);
            }
        }
        let mut latency_by_label: HashMap<String, Option<f64>> = HashMap::new();
        for (provider, entries) in registry.upstream_latency_summaries() {
            for (upstream, avg) in entries {
                latency_by_label.insert(format!("{provider}/{upstream}"), avg);
            }
        }
        let capacity_by_label: HashMap<String, clawde_api::UpstreamCapacityStatus> = registry
            .upstream_capacity_summaries()
            .into_iter()
            .flat_map(|(provider, statuses)| {
                statuses
                    .into_iter()
                    .map(move |status| (format!("{provider}/{}", status.upstream_id), status))
            })
            .collect();
        let capacity_for = |label: &str| capacity_by_label.get(label).cloned();
        let snapshot_for = |label: &str| {
            (
                success_by_label.get(label).copied().flatten(),
                latency_by_label.get(label).copied().flatten(),
            )
        };

        for (provider, active, total, retry) in registry.key_ring_summaries() {
            let (success_rate, avg_latency) = snapshot_for(&provider);
            let capacity = capacity_for(&provider);
            rows.push(ProviderHealthRow {
                cooldowns: cooldown_counts.get(&provider).copied().unwrap_or(0),
                label: provider,
                active_keys: active,
                total_keys: total,
                retry_secs: retry,
                success_rate,
                avg_latency,
                capacity,
            });
        }
        for (provider, entries) in registry.upstream_key_health_summaries() {
            for (upstream, active, total, retry) in entries {
                let label = format!("{provider}/{upstream}");
                let (success_rate, avg_latency) = snapshot_for(&label);
                let capacity = capacity_for(&label);
                rows.push(ProviderHealthRow {
                    cooldowns: cooldown_counts.get(&label).copied().unwrap_or(0),
                    label,
                    active_keys: active,
                    total_keys: total,
                    retry_secs: retry,
                    success_rate,
                    avg_latency,
                    capacity,
                });
            }
        }
        // Live model probe picks: what the free-discovery poller last selected
        // per upstream (from the persisted live-discovery cache), so the stats
        // view shows the effective model without leaving the session.
        self.live_probe_picks = clawde_api::providers::free::live_discovery_snapshot()
            .map(|(probes, _saved_at)| {
                let mut picks: Vec<(String, String)> = probes.into_iter().collect();
                picks.sort();
                picks
            })
            .unwrap_or_default();

        // Performance rows for every configured upstream, even those without a
        // key ring (single-key setups report no rings, so the key-health loop
        // above would skip them entirely). This is what makes the
        // model-performance fact-check useful for the common single-key case:
        // each configured upstream shows its success rate / latency even
        // though it has no active/total key dots. Rows already added by the
        // ring loops are not duplicated.
        for (provider, entries) in registry.upstream_success_rate_summaries() {
            for (upstream, rate) in entries {
                let label = format!("{provider}/{upstream}");
                if rows.iter().any(|r| r.label == label) {
                    continue;
                }
                let avg_latency = latency_by_label.get(&label).copied().flatten();
                let capacity = capacity_for(&label);
                rows.push(ProviderHealthRow {
                    cooldowns: cooldown_counts.get(&label).copied().unwrap_or(0),
                    label,
                    active_keys: 0,
                    total_keys: 0,
                    retry_secs: None,
                    success_rate: rate,
                    avg_latency,
                    capacity,
                });
            }
        }

        // Capacity may be the only telemetry available for a configured
        // upstream (for example, before its first completed request). Keep it
        // visible in the existing health view without inventing key-health or
        // performance values.
        for (label, capacity) in capacity_by_label {
            if rows.iter().any(|row| row.label == label) {
                continue;
            }
            rows.push(ProviderHealthRow {
                label,
                active_keys: 0,
                total_keys: 0,
                retry_secs: None,
                cooldowns: 0,
                success_rate: None,
                avg_latency: None,
                capacity: Some(capacity),
            });
        }
        rows.sort_by(|a, b| a.label.cmp(&b.label));
        self.live_provider_health = rows;
    }

    /// Record one completed provider request. Keep the list bounded so a long
    /// interactive session cannot grow the TUI state without limit.
    pub fn record_provider_activity(
        &mut self,
        provider_id: &str,
        upstream_id: Option<&str>,
        model: &str,
        elapsed_ms: u64,
        retries: u32,
        fallback_used: bool,
    ) {
        if let Some(row) = self.provider_activity.iter_mut().find(|row| {
            row.provider_id == provider_id
                && row.upstream_id.as_deref() == upstream_id
                && row.model == model
        }) {
            row.requests = row.requests.saturating_add(1);
            row.total_elapsed_ms = row.total_elapsed_ms.saturating_add(elapsed_ms);
            row.retries = row.retries.saturating_add(retries);
            row.fallbacks = row.fallbacks.saturating_add(u32::from(fallback_used));
            return;
        }

        const MAX_PROVIDER_ACTIVITY_ROWS: usize = 12;
        if self.provider_activity.len() >= MAX_PROVIDER_ACTIVITY_ROWS {
            self.provider_activity.remove(0);
        }
        self.provider_activity.push(ProviderActivityRow {
            provider_id: provider_id.to_string(),
            upstream_id: upstream_id.map(str::to_string),
            model: model.to_string(),
            requests: 1,
            total_elapsed_ms: elapsed_ms,
            retries,
            fallbacks: u32::from(fallback_used),
        });
    }

    pub fn open(&mut self) {
        let stats = load_stats();
        self.model_breakdown = build_model_breakdown(&stats);
        let (current, longest) = compute_streaks(&stats);
        self.current_streak_days = current;
        self.longest_streak_days = longest;
        self.data = Some(stats);
        self.live_provider_health.clear();
        self.live_probe_picks.clear();
        self.visible = true;
        self.tab = StatsTab::Overview;
        self.scroll = 0;
    }

    /// Billing-only views are useful when at least one recorded turn has a
    /// nonzero cost. Free-model usage still keeps token and model analytics,
    /// but should not expose empty cost visualizations.
    fn has_paid_usage(&self) -> bool {
        self.data
            .as_ref()
            .is_some_and(|stats| stats.total_cost_cents > 0.0)
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    pub fn next_tab(&mut self) {
        let current = if !self.has_paid_usage() && self.tab == StatsTab::CostHeatmap {
            StatsTab::DailyTokens
        } else {
            self.tab
        };
        self.tab = match current {
            StatsTab::Overview => StatsTab::DailyTokens,
            StatsTab::DailyTokens if self.has_paid_usage() => StatsTab::CostHeatmap,
            StatsTab::DailyTokens => StatsTab::Models,
            StatsTab::CostHeatmap => StatsTab::Models,
            StatsTab::Models => StatsTab::Overview,
        };
        self.scroll = 0;
    }

    pub fn prev_tab(&mut self) {
        let current = if !self.has_paid_usage() && self.tab == StatsTab::CostHeatmap {
            StatsTab::DailyTokens
        } else {
            self.tab
        };
        self.tab = match current {
            StatsTab::Overview => StatsTab::Models,
            StatsTab::DailyTokens => StatsTab::Overview,
            StatsTab::CostHeatmap => StatsTab::DailyTokens,
            StatsTab::Models if self.has_paid_usage() => StatsTab::CostHeatmap,
            StatsTab::Models => StatsTab::DailyTokens,
        };
        self.scroll = 0;
    }

    pub fn cycle_range(&mut self) {
        self.range_days = match self.range_days {
            7 => 30,
            30 => 0,
            _ => 7,
        };
    }

    /// Record usage for a model, accumulating into `model_breakdown`.
    /// `cost` is in USD (not cents).
    pub fn add_model_usage(&mut self, model_id: &str, input: u64, output: u64, cost: f64) {
        if let Some(entry) = self
            .model_breakdown
            .iter_mut()
            .find(|e| e.model_id == model_id)
        {
            entry.input_tokens += input;
            entry.output_tokens += output;
            entry.cost_usd += cost;
        } else {
            self.model_breakdown.push(ModelBreakdown {
                model_id: model_id.to_string(),
                input_tokens: input,
                output_tokens: output,
                cost_usd: cost,
            });
        }
    }
}

impl Default for StatsDialogState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers: build model breakdown and compute streaks
// ---------------------------------------------------------------------------

fn build_model_breakdown(stats: &AggregatedStats) -> Vec<ModelBreakdown> {
    let mut breakdown: Vec<ModelBreakdown> = stats
        .by_model
        .iter()
        .map(|(model_id, ms)| ModelBreakdown {
            model_id: model_id.clone(),
            input_tokens: ms.input_tokens,
            output_tokens: ms.output_tokens,
            cost_usd: ms.cost_cents / 100.0,
        })
        .collect();
    breakdown.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    breakdown
}

/// Compute (current_streak, longest_streak) in days from the aggregated stats.
/// A streak is a consecutive run of calendar days with any activity, ending on
/// the most-recent active day.
fn compute_streaks(stats: &AggregatedStats) -> (u32, u32) {
    if stats.daily_tokens.is_empty() {
        return (0, 0);
    }

    // Collect sorted unique active dates
    let mut dates: Vec<&str> = stats.daily_tokens.iter().map(|(d, _)| d.as_str()).collect();
    dates.dedup();

    let mut longest: u32 = 1;
    let mut current_run: u32 = 1;

    for window in dates.windows(2) {
        if consecutive_dates(window[0], window[1]) {
            current_run += 1;
            if current_run > longest {
                longest = current_run;
            }
        } else {
            current_run = 1;
        }
    }

    // The "current" streak is the run ending on the last active date.
    // Recompute from the end.
    let mut current_streak: u32 = 1;
    for window in dates.windows(2).rev() {
        if consecutive_dates(window[0], window[1]) {
            current_streak += 1;
        } else {
            break;
        }
    }

    (current_streak, longest)
}

/// Returns true when `next` is exactly one calendar day after `prev`.
/// Both strings must be "YYYY-MM-DD".
fn consecutive_dates(prev: &str, next: &str) -> bool {
    let prev_days = date_to_days_since_epoch(prev);
    let next_days = date_to_days_since_epoch(next);
    match (prev_days, next_days) {
        (Some(p), Some(n)) => n == p + 1,
        _ => false,
    }
}

fn date_to_days_since_epoch(date: &str) -> Option<u64> {
    // Expect "YYYY-MM-DD"
    if date.len() != 10 {
        return None;
    }
    let year: u64 = date[0..4].parse().ok()?;
    let month: u64 = date[5..7].parse().ok()?;
    let day: u64 = date[8..10].parse().ok()?;
    // Days from 1970-01-01 (approximate, good enough for streak detection)
    let y = year - 1970;
    let leap_days = if y > 0 {
        (y - 1) / 4 - (y - 1) / 100 + (y - 1) / 400 + 1
    } else {
        0
    };
    let days_in_years = y * 365 + leap_days;
    let leap = is_leap_year(year as u32);
    let months = if leap {
        [0u64, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335]
    } else {
        [0u64, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334]
    };
    let month_days = months.get((month as usize).saturating_sub(1))?;
    Some(days_in_years + month_days + day - 1)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the stats dialog overlay.
pub fn render_stats_dialog(state: &StatsDialogState, area: Rect, buf: &mut Buffer) {
    if !state.visible {
        return;
    }

    let layout = begin_modal_buf(buf, area, 92, 30, 2, 1);
    render_modal_title_buf(buf, layout.header_area, "Cost & stats", "esc");

    let content_area = layout.body_area;

    let Some(data) = &state.data else {
        Paragraph::new("Loading\u{2026}")
            .style(Style::default().fg(CLAWDE_MUTED).bg(CLAWDE_PANEL_BG))
            .render(content_area, buf);
        return;
    };

    let cost_tab_available = data.total_cost_cents > 0.0;
    let active_tab = if !cost_tab_available && state.tab == StatsTab::CostHeatmap {
        StatsTab::DailyTokens
    } else {
        state.tab
    };
    let mut tab_spans = vec![
        tab_span("Overview", active_tab == StatsTab::Overview),
        Span::styled("  ·  ", Style::default().fg(CLAWDE_MUTED)),
        tab_span("Daily Tokens", active_tab == StatsTab::DailyTokens),
    ];
    if cost_tab_available {
        tab_spans.push(Span::styled("  ·  ", Style::default().fg(CLAWDE_MUTED)));
        tab_spans.push(tab_span(
            "Cost Heatmap",
            active_tab == StatsTab::CostHeatmap,
        ));
    }
    tab_spans.push(Span::styled("  ·  ", Style::default().fg(CLAWDE_MUTED)));
    tab_spans.push(tab_span("Models", active_tab == StatsTab::Models));
    let tab_line = Line::from(tab_spans);
    if let Some(tab_area) = modal_header_line_area(layout.header_area, 1) {
        Paragraph::new(tab_line).render(tab_area, buf);
    }

    match active_tab {
        StatsTab::Overview => render_overview(data, state, content_area, buf),
        StatsTab::DailyTokens => render_daily_tokens(data, state.range_days, content_area, buf),
        StatsTab::CostHeatmap => render_cost_heatmap(data, content_area, buf),
        StatsTab::Models => render_models(state, content_area, buf),
    }
    Paragraph::new(Line::from(vec![Span::styled(
        " tab/←/→ switch tabs  ·  r cycle range  ·  ↑↓ scroll",
        Style::default()
            .fg(CLAWDE_MUTED)
            .add_modifier(Modifier::ITALIC),
    )]))
    .render(layout.footer_area, buf);
}

fn tab_span(label: &str, active: bool) -> Span<'static> {
    if active {
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(CLAWDE_ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )
    } else {
        Span::styled(label.to_string(), Style::default().fg(CLAWDE_MUTED))
    }
}

fn current_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn format_capacity_badge(status: &clawde_api::UpstreamCapacityStatus, now: u64) -> (String, Color) {
    let color = if status.utilization_pct >= 95.0 {
        Color::Red
    } else if status.utilization_pct >= 80.0 {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let reset = status
        .reset_at_unix
        .filter(|reset| *reset > now)
        .map(|reset| compact_duration(reset - now))
        .or_else(|| status.retry_after_secs.map(compact_duration))
        .map(|remaining| format!(" · {}", remaining))
        .unwrap_or_default();
    (
        format!(
            "  cap {:>3.0}% {}{}",
            status.utilization_pct,
            status.source.label(),
            reset
        ),
        color,
    )
}

fn compact_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    if days > 0 {
        format!("{}d", days)
    } else if hours > 0 {
        format!("{}h", hours)
    } else if minutes > 0 {
        format!("{}m", minutes)
    } else {
        format!("{}s", seconds)
    }
}

// ---------------------------------------------------------------------------
// Overview tab
// ---------------------------------------------------------------------------

#[allow(clippy::vec_init_then_push)]
fn render_overview(data: &AggregatedStats, state: &StatsDialogState, area: Rect, buf: &mut Buffer) {
    let total_tokens = data.total_input_tokens + data.total_output_tokens;
    let mut lines = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("  Input:    ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_tokens(data.total_input_tokens)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Output:   ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_tokens(data.total_output_tokens)),
    ]));
    if let Some(cache_summary) =
        format_cache_summary(data.total_cache_read_tokens, data.total_cache_write_tokens)
    {
        lines.push(Line::from(vec![
            Span::styled("  Cache:    ", Style::default().fg(Color::DarkGray)),
            Span::raw(cache_summary),
        ]));
    }
    lines.push(Line::default());
    let usage_summary = if data.total_cost_cents > 0.0 {
        clawde_core::format_utils::format_usage_summary(total_tokens, data.total_cost_cents)
    } else {
        format!("{} tokens", format_tokens(total_tokens))
    };
    lines.push(Line::from(vec![Span::styled(
        usage_summary,
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));

    // Streak display
    lines.push(Line::default());
    {
        let current = state.current_streak_days;
        let longest = state.longest_streak_days;
        let streak_value = Span::styled(
            format!("● {} day{}", current, if current == 1 { "" } else { "s" }),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        let streak_longest = Span::styled(
            format!(
                "  (longest: {} day{})",
                longest,
                if longest == 1 { "" } else { "s" }
            ),
            Style::default().fg(Color::DarkGray),
        );
        lines.push(Line::from(vec![
            Span::styled("Streak: ", Style::default().fg(Color::DarkGray)),
            streak_value,
            streak_longest,
        ]));
    }

    if let Some(peak) = &data.peak_day {
        lines.push(Line::default());
        lines.push(Line::from(vec![
            Span::styled("Peak day: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} ({} tokens)", peak, format_tokens(data.peak_day_tokens)),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }

    if !state.provider_activity.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            "Session provider activity:",
            Style::default().fg(Color::DarkGray),
        )]));
        for row in state.provider_activity.iter().take(8) {
            let mut detail = format!(
                "  {}{}  {} req  avg {}ms",
                row.provider_id,
                row.upstream_id
                    .as_deref()
                    .map(|upstream| format!("/{}", upstream))
                    .unwrap_or_default(),
                row.requests,
                row.average_elapsed_ms()
            );
            if row.retries > 0 {
                detail.push_str(&format!(
                    "  {} retr{}",
                    row.retries,
                    if row.retries == 1 { "y" } else { "ies" }
                ));
            }
            if row.fallbacks > 0 {
                detail.push_str(&format!(
                    "  {} fallback{}",
                    row.fallbacks,
                    if row.fallbacks == 1 { "" } else { "s" }
                ));
            }
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<28}", row.model),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(detail, Style::default().fg(Color::White)),
            ]));
        }
    }

    if !state.live_provider_health.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            "Live key health:",
            Style::default().fg(Color::DarkGray),
        )]));
        let now = current_unix_secs();
        for row in state.live_provider_health.iter().take(8) {
            // Rows without a key ring (single-key upstreams) carry no
            // active/total dots — their dot follows the success rate instead,
            // so a healthy upstream is green rather than a misleading red
            // (the `active_keys == 0` branch would otherwise fire).
            let color = if row.total_keys == 0 {
                match row.success_rate {
                    Some(r) if r >= 0.99 => Color::Green,
                    Some(r) if r > 0.0 => Color::Yellow,
                    Some(_) => Color::Red,
                    None => Color::DarkGray,
                }
            } else if row.active_keys == 0 {
                Color::Red
            } else if row.active_keys < row.total_keys {
                Color::Yellow
            } else {
                Color::Green
            };
            let retry = row
                .retry_secs
                .map(|secs| format!("  retry {}s", secs))
                .unwrap_or_default();
            let cooldowns = if row.cooldowns > 0 {
                format!(
                    "  {} cooldown{}",
                    row.cooldowns,
                    if row.cooldowns == 1 { "" } else { "s" }
                )
            } else {
                String::new()
            };
            // Model-performance fact-check (spec §8.6): dispatch success rate
            // and average latency when recorded. Success rates color-code like
            // the key dots — green all-success, yellow degraded, red failing —
            // so the row answers "is this upstream actually working?" at a
            // glance. Em-dashes pad to the same column width as the numeric
            // rates so rows stay aligned.
            let (success, success_color) = match row.success_rate {
                Some(r) if r >= 0.99 => (format!("  {:>3.0}%", r * 100.0), Color::Green),
                Some(r) if r > 0.0 => (format!("  {:>3.0}%", r * 100.0), Color::Yellow),
                Some(_) => (format!("  {:>3.0}%", 0.0), Color::Red),
                None => ("     —".to_string(), Color::DarkGray),
            };
            let latency = row
                .avg_latency
                .map(|secs| format!("  {:.1}s", secs))
                .unwrap_or_else(|| "     —".to_string());
            // Key-health dots only exist for key-ring upstreams (2+ keys);
            // single-key performance rows render with no keys suffix.
            let keys = if row.total_keys > 0 {
                format!("{}/{} keys", row.active_keys, row.total_keys)
            } else {
                String::new()
            };
            let capacity = row
                .capacity
                .as_ref()
                .map(|status| {
                    let (text, color) = format_capacity_badge(status, now);
                    Span::styled(text, Style::default().fg(color))
                })
                .unwrap_or_else(|| Span::raw(""));
            lines.push(Line::from(vec![
                Span::styled("  ● ", Style::default().fg(color)),
                Span::styled(
                    format!("{:<28}", row.label),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(keys, Style::default().fg(color)),
                Span::styled(retry, Style::default().fg(Color::Yellow)),
                Span::styled(cooldowns, Style::default().fg(Color::DarkGray)),
                Span::styled(success, Style::default().fg(success_color)),
                Span::styled(latency, Style::default().fg(Color::White)),
                capacity,
            ]));
        }
    }

    if !state.live_probe_picks.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            "Live model probes:",
            Style::default().fg(Color::DarkGray),
        )]));
        for (upstream, model) in state.live_probe_picks.iter().take(8) {
            lines.push(Line::from(vec![
                Span::styled("  ● ", Style::default().fg(Color::Green)),
                Span::styled(
                    format!("{:<28}", upstream),
                    Style::default().fg(Color::Cyan),
                ),
                Span::styled(model, Style::default().fg(Color::White)),
            ]));
        }
    }

    if !data.by_model.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(
            "By model:",
            Style::default().fg(Color::DarkGray),
        )]));
        let mut models: Vec<_> = data.by_model.iter().collect();
        models.sort_by(|a, b| {
            b.1.cost_cents
                .partial_cmp(&a.1.cost_cents)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (model, stats) in models.iter().take(5) {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:40} ", model), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!(
                        "{} turns  {}",
                        stats.turns,
                        format_tokens(stats.input_tokens + stats.output_tokens)
                    ),
                    Style::default().fg(Color::White),
                ),
                if data.total_cost_cents > 0.0 {
                    Span::styled(
                        format!("  ${:.2}", stats.cost_cents / 100.0),
                        Style::default().fg(Color::DarkGray),
                    )
                } else {
                    Span::raw("")
                },
            ]));
        }
    }

    Paragraph::new(lines).render(area, buf);
}

// ---------------------------------------------------------------------------
// Daily Tokens tab
// ---------------------------------------------------------------------------

fn render_daily_tokens(data: &AggregatedStats, range_days: u32, area: Rect, buf: &mut Buffer) {
    // Filter to range
    let filtered: Vec<_> = if range_days == 0 {
        data.daily_tokens.iter().collect()
    } else {
        data.daily_tokens
            .iter()
            .rev()
            .take(range_days as usize)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };

    if filtered.is_empty() {
        Paragraph::new("No data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .render(area, buf);
        return;
    }

    let range_label = match range_days {
        7 => "7 days",
        30 => "30 days",
        _ => "all time",
    };
    let label_line = Line::from(vec![Span::styled(
        format!("Range: {} [r: cycle]", range_label),
        Style::default().fg(Color::DarkGray),
    )]);
    Paragraph::new(label_line).render(
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        buf,
    );

    let chart_area = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2),
    };

    // Build bar chart data
    let max_val = filtered.iter().map(|d| d.1).max().unwrap_or(1).max(1);
    let bar_data: Vec<(&str, u64)> = filtered
        .iter()
        .map(|d| {
            let label: &str = if d.0.len() >= 5 {
                &d.0[5..]
            } else {
                d.0.as_str()
            };
            (label, d.1 * (chart_area.height as u64 - 1) / max_val)
        })
        .collect();

    // Render ASCII bar chart manually (ratatui BarChart needs 'static strs)
    for (i, (label, height)) in bar_data.iter().enumerate() {
        let x = chart_area.x + i as u16 * 6;
        if x + 5 >= chart_area.x + chart_area.width {
            break;
        }
        let bar_height = (*height as u16).min(chart_area.height.saturating_sub(1));
        for row in 0..bar_height {
            let y = chart_area.y + chart_area.height - 1 - row;
            let cell = buf.cell_mut((x + 1, y));
            if let Some(c) = cell {
                c.set_symbol("\u{2588}");
                c.set_style(Style::default().fg(Color::Cyan));
            }
            let cell2 = buf.cell_mut((x + 2, y));
            if let Some(c) = cell2 {
                c.set_symbol("\u{2588}");
                c.set_style(Style::default().fg(Color::Cyan));
            }
        }
        // Label
        let y = chart_area.y + chart_area.height - 1;
        let label_short: String = label.chars().take(4).collect();
        for (j, ch) in label_short.chars().enumerate() {
            let cell = buf.cell_mut((x + j as u16, y));
            if let Some(c) = cell {
                c.set_symbol(&ch.to_string());
                c.set_style(Style::default().fg(Color::DarkGray));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cost Heatmap tab (GitHub-style)
// ---------------------------------------------------------------------------

fn render_cost_heatmap(data: &AggregatedStats, area: Rect, buf: &mut Buffer) {
    if data.daily_costs.is_empty() || !data.daily_costs.values().any(|cost| *cost > 0.0) {
        Paragraph::new("No paid cost data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .render(area, buf);
        return;
    }

    let max_cost = data
        .daily_costs
        .values()
        .cloned()
        .fold(0.0_f64, f64::max)
        .max(0.01);

    // Header legend
    Paragraph::new(Line::from(vec![
        Span::styled(
            "Cost Heatmap (last 12 weeks)   no activity ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("\u{25a0}", Style::default().fg(Color::Rgb(30, 30, 30))),
        Span::styled(" low ", Style::default().fg(Color::DarkGray)),
        Span::styled("\u{25a0}", Style::default().fg(Color::Rgb(0, 100, 0))),
        Span::styled(" med ", Style::default().fg(Color::DarkGray)),
        Span::styled("\u{25a0}", Style::default().fg(Color::Rgb(0, 200, 0))),
        Span::styled(" high ", Style::default().fg(Color::DarkGray)),
        Span::styled("\u{25a0}", Style::default().fg(Color::Rgb(0, 255, 0))),
    ]))
    .render(
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
        buf,
    );

    // Weekday labels column (Mon..Sun order)
    let weekday_labels = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let heatmap_area = Rect {
        x: area.x + 4, // leave 4 cols for "Mon" etc.
        y: area.y + 2,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(3),
    };

    for (i, label) in weekday_labels.iter().enumerate() {
        let y = heatmap_area.y + i as u16;
        if y >= heatmap_area.y + heatmap_area.height {
            break;
        }
        Paragraph::new(Line::from(vec![Span::styled(
            label.to_string(),
            Style::default().fg(Color::DarkGray),
        )]))
        .render(
            Rect {
                x: area.x,
                y,
                width: 3,
                height: 1,
            },
            buf,
        );
    }

    // 12 weeks x 7 days grid — sorted ascending, display newest on right
    let sorted_dates: Vec<_> = {
        let mut v: Vec<_> = data.daily_costs.iter().collect();
        v.sort_by(|a, b| a.0.cmp(b.0));
        v
    };

    // We group into chunks of 7 calendar days (by index, as in the original)
    // and place week columns right-to-left from the most-recent week.
    let chunks: Vec<_> = sorted_dates.chunks(7).collect();
    let total_chunks = chunks.len();
    let start_chunk = total_chunks.saturating_sub(12);

    for (display_col, chunk) in chunks[start_chunk..].iter().enumerate() {
        let x = heatmap_area.x + display_col as u16 * 2;
        if x >= heatmap_area.x + heatmap_area.width {
            break;
        }
        for (day_idx, (_, cost)) in chunk.iter().enumerate() {
            let y = heatmap_area.y + day_idx as u16;
            if y >= heatmap_area.y + heatmap_area.height {
                break;
            }
            let intensity = (*cost / max_cost).min(1.0);
            let color = heatmap_color(intensity);
            let cell = buf.cell_mut((x, y));
            if let Some(c) = cell {
                c.set_symbol("\u{25a0}");
                c.set_style(Style::default().fg(color));
            }
        }
    }
}

/// Map a 0..=1 intensity to a green-shade color matching the GitHub heatmap spec.
fn heatmap_color(intensity: f64) -> Color {
    if intensity < 0.01 {
        Color::Rgb(30, 30, 30)
    } else if intensity < 0.25 {
        Color::Rgb(0, 100, 0)
    } else if intensity < 0.50 {
        Color::Rgb(0, 150, 0)
    } else if intensity < 0.75 {
        Color::Rgb(0, 200, 0)
    } else {
        Color::Rgb(0, 255, 0)
    }
}

// ---------------------------------------------------------------------------
// Models tab
// ---------------------------------------------------------------------------

fn render_models(state: &StatsDialogState, area: Rect, buf: &mut Buffer) {
    if state.model_breakdown.is_empty() {
        Paragraph::new("No model usage data yet.")
            .style(Style::default().fg(Color::DarkGray))
            .render(area, buf);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let show_cost = state
        .model_breakdown
        .iter()
        .any(|entry| entry.cost_usd > 0.0);

    // Table header
    let mut header_spans = vec![Span::styled(
        format!("{:<42} {:>12} {:>13}", "Model", "Input", "Output"),
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )];
    if show_cost {
        header_spans.push(Span::styled(
            format!(" {:>10}", "Cost"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header_spans));
    // Separator
    lines.push(Line::from(vec![Span::styled(
        "\u{2500}".repeat(area.width.saturating_sub(2) as usize),
        Style::default().fg(Color::DarkGray),
    )]));

    let mut total_input: u64 = 0;
    let mut total_output: u64 = 0;
    let mut total_cost: f64 = 0.0;

    for entry in &state.model_breakdown {
        total_input += entry.input_tokens;
        total_output += entry.output_tokens;
        total_cost += entry.cost_usd;

        // Truncate long model IDs
        let model_display = if entry.model_id.len() > 42 {
            format!("{}...", &entry.model_id[..39])
        } else {
            entry.model_id.clone()
        };

        let mut row_spans = vec![
            Span::styled(
                format!("{:<42} ", model_display),
                Style::default().fg(Color::Cyan),
            ),
            Span::styled(
                format!("{:>12} ", format_tokens(entry.input_tokens)),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!("{:>13} ", format_tokens(entry.output_tokens)),
                Style::default().fg(Color::White),
            ),
        ];
        if show_cost {
            row_spans.push(Span::styled(
                format!("{:>9}", format!("${:.4}", entry.cost_usd)),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(row_spans));
    }

    // Grand total separator + row
    lines.push(Line::from(vec![Span::styled(
        "\u{2500}".repeat(area.width.saturating_sub(2) as usize),
        Style::default().fg(Color::DarkGray),
    )]));
    let mut total_spans = vec![
        Span::styled(
            format!("{:<42} ", "TOTAL"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>12} ", format_tokens(total_input)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>13} ", format_tokens(total_output)),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if show_cost {
        total_spans.push(Span::styled(
            format!("{:>9}", format!("${:.4}", total_cost)),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(total_spans));

    Paragraph::new(lines).render(area, buf);
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn format_cache_summary(read_tokens: u64, write_tokens: u64) -> Option<String> {
    if read_tokens == 0 && write_tokens == 0 {
        None
    } else {
        Some(format!(
            "{} read · {} written",
            format_tokens(read_tokens),
            format_tokens(write_tokens)
        ))
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 10_000 {
        format!("{:.0}K", n as f64 / 1_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
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

    // ---- helpers -----------------------------------------------------------

    fn make_state_with_models(entries: &[(&str, u64, u64, f64)]) -> StatsDialogState {
        let mut state = StatsDialogState::new();
        for (model, input, output, cost) in entries {
            state.add_model_usage(model, *input, *output, *cost);
        }
        state
    }

    fn make_agg_with_dates(dates: &[&str]) -> AggregatedStats {
        let mut agg = AggregatedStats::default();
        for date in dates {
            agg.daily_tokens.push((date.to_string(), 100));
        }
        agg
    }

    // ---- free-mode cost presentation ---------------------------------------

    fn free_state() -> StatsDialogState {
        let mut state = make_state_with_models(&[("free/model", 1200, 300, 0.0)]);
        state.data = Some(AggregatedStats {
            total_input_tokens: 1200,
            total_output_tokens: 300,
            total_cost_cents: 0.0,
            ..AggregatedStats::default()
        });
        state
    }

    #[test]
    fn provider_activity_aggregates_and_renders() {
        let mut state = free_state();
        state.record_provider_activity("free", Some("groq"), "llama-3.3", 100, 1, true);
        state.record_provider_activity("free", Some("groq"), "llama-3.3", 300, 0, false);
        assert_eq!(state.provider_activity[0].requests, 2);
        assert_eq!(state.provider_activity[0].average_elapsed_ms(), 200);
        assert_eq!(state.provider_activity[0].retries, 1);
        assert_eq!(state.provider_activity[0].fallbacks, 1);

        let area = Rect::new(0, 0, 110, 30);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Session provider activity:"));
        assert!(content.contains("free/groq"));
        assert!(content.contains("2 req"));
        assert!(content.contains("avg 200ms"));
        assert!(content.contains("1 retry"));
        assert!(content.contains("1 fallback"));
    }

    #[test]
    fn provider_activity_is_bounded() {
        let mut state = StatsDialogState::new();
        for i in 0..20 {
            state.record_provider_activity("provider", None, &format!("model-{i}"), i, 0, false);
        }
        assert_eq!(state.provider_activity.len(), 12);
        assert_eq!(state.provider_activity[0].model, "model-8");
    }

    #[test]
    fn live_provider_health_renders_key_state_and_cooldown() {
        let mut state = free_state();
        state.live_provider_health.push(ProviderHealthRow {
            label: "free/groq".into(),
            active_keys: 1,
            total_keys: 2,
            retry_secs: Some(30),
            cooldowns: 1,
            success_rate: None,
            avg_latency: None,
            capacity: None,
        });
        let area = Rect::new(0, 0, 110, 24);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Live key health:"));
        assert!(content.contains("free/groq"));
        assert!(content.contains("1/2 keys"));
        assert!(content.contains("retry 30s"));
        assert!(content.contains("1 cooldown"));
    }

    #[test]
    fn live_probe_picks_render_upstream_and_model() {
        let mut state = free_state();
        state.live_probe_picks = vec![
            (
                "cloudflare".to_string(),
                "@cf/qwen/qwen3-30b-a3b-fp8".to_string(),
            ),
            ("groq".to_string(), "openai/gpt-oss-120b".to_string()),
        ];
        let area = Rect::new(0, 0, 110, 24);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Live model probes:"));
        assert!(content.contains("cloudflare"));
        assert!(content.contains("@cf/qwen/qwen3-30b-a3b-fp8"));
        assert!(content.contains("groq"));
        assert!(content.contains("openai/gpt-oss-120b"));
    }

    #[test]
    fn live_provider_health_renders_capacity_source_and_reset() {
        let mut state = free_state();
        state.live_provider_health.push(ProviderHealthRow {
            label: "free/groq".into(),
            active_keys: 0,
            total_keys: 0,
            retry_secs: None,
            cooldowns: 0,
            success_rate: None,
            avg_latency: None,
            capacity: Some(clawde_api::UpstreamCapacityStatus {
                upstream_id: "groq".into(),
                source: clawde_api::CapacityStatusSource::Headers,
                utilization_pct: 72.0,
                tokens_pct_used: Some(0.72),
                requests_pct_used: None,
                retry_after_secs: Some(90),
                reset_at_unix: Some(current_unix_secs().saturating_add(90)),
            }),
        });
        let area = Rect::new(0, 0, 120, 24);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("cap  72% headers"), "got: {content}");
        assert!(content.contains("1m"), "reset timing missing: {content}");
    }

    #[test]
    fn live_provider_health_renders_success_rate_and_latency() {
        let mut state = free_state();
        state.live_provider_health.push(ProviderHealthRow {
            label: "free/poolside".into(),
            active_keys: 0,
            total_keys: 0,
            retry_secs: None,
            cooldowns: 0,
            success_rate: Some(0.25),
            avg_latency: Some(9.4),
            capacity: None,
        });
        state.live_provider_health.push(ProviderHealthRow {
            label: "free/groq".into(),
            active_keys: 0,
            total_keys: 0,
            retry_secs: None,
            cooldowns: 0,
            success_rate: Some(1.0),
            avg_latency: Some(2.1),
            capacity: None,
        });
        let area = Rect::new(0, 0, 110, 24);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("free/poolside"));
        assert!(content.contains("25%"));
        assert!(content.contains("9.4s"));
        assert!(content.contains("free/groq"));
        assert!(content.contains("100%"));
        assert!(content.contains("2.1s"));
        // Single-key rows (no ring) must NOT render a "0/0 keys" suffix.
        assert!(!content.contains("0/0 keys"));
    }

    /// Point CLAWDE_HOME at a throwaway temp dir for the duration of a test so
    /// `build_free_provider`'s auth-store read never touches the real home.
    /// Mirrors the TestHome helper in app.rs / commands/keys.rs. Serializes on
    /// the crate-wide [`crate::TEST_ENV_LOCK`] per AGENTS.md.
    struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_clawde_home: Option<std::ffi::OsString>,
    }

    impl TestHome {
        fn acquire() -> TestHome {
            let _lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _lock,
                _tmp: tmp,
                prev_clawde_home: prev,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.prev_clawde_home {
                Some(v) => std::env::set_var("CLAWDE_HOME", v),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    /// Build a config whose free chain is exactly one upstream (mistral) with
    /// Ollama isolated, so a fresh single-key setup exercises the
    /// performance-row path (no key rings) without network calls.
    fn single_upstream_config() -> clawde_core::config::Config {
        use clawde_core::config::ProviderConfig;
        let mut options = std::collections::HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({
                "disabled_upstreams": [
                    "nvidia", "cerebras", "google", "cloudflare",
                    "groq", "sambanova", "cline", "opencode-zen",
                    "zai", "openrouter", "poolside"
                ]
            }),
        );
        let mut provider_configs = std::collections::HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            ProviderConfig {
                options,
                ..Default::default()
            },
        );
        provider_configs.insert(
            "ollama".to_string(),
            ProviderConfig {
                options: [("mode".to_string(), serde_json::json!("isolated"))]
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        );
        clawde_core::config::Config {
            provider_configs,
            ..Default::default()
        }
    }

    #[test]
    fn refresh_provider_health_surfaces_performance_rows_without_rings() {
        // A FreeProvider with a single key per upstream reports no key rings,
        // so the health rows must come from the success-rate / latency
        // summaries. Every configured upstream shows a row (with em-dash
        // metrics until its first dispatch), even though none has a ring.
        use clawde_api::ProviderRegistry;

        let _home = TestHome::acquire();
        // Seed a single mistral key into the temp home's auth store (set()
        // persists to CLAWDE_HOME/auth.json immediately).
        let mut store = clawde_core::AuthStore::load();
        store.set(
            "mistral",
            clawde_core::StoredCredential::ApiKey {
                key: "fake-mistral-key-1234567890".to_string(),
            },
        );

        let config = single_upstream_config();
        // registry.rebuild_free is the public path app.rs uses at runtime; it
        // builds the chain from the seeded auth store and registers it.
        let mut registry = ProviderRegistry::new();
        registry.rebuild_free(&config);

        let mut state = free_state();
        state.refresh_provider_health(&registry);
        assert!(
            !state.live_provider_health.is_empty(),
            "single-key chain must surface performance rows"
        );
        for row in &state.live_provider_health {
            assert_eq!(
                row.total_keys, 0,
                "single-key chain reports no key rings, got {} keys for {}",
                row.total_keys, row.label
            );
        }
        let mistral = state
            .live_provider_health
            .iter()
            .find(|r| r.label == "free/mistral");
        assert!(mistral.is_some(), "free/mistral row must be present");
        assert_eq!(mistral.unwrap().success_rate, None);
        assert_eq!(mistral.unwrap().avg_latency, None);
    }

    #[test]
    fn free_usage_skips_cost_heatmap_when_cycling_tabs() {
        let mut state = free_state();
        state.tab = StatsTab::Overview;
        state.next_tab();
        assert_eq!(state.tab, StatsTab::DailyTokens);
        state.next_tab();
        assert_eq!(state.tab, StatsTab::Models);
        state.prev_tab();
        assert_eq!(state.tab, StatsTab::DailyTokens);

        state.tab = StatsTab::Models;
        state.prev_tab();
        assert_eq!(state.tab, StatsTab::DailyTokens);

        state.tab = StatsTab::CostHeatmap;
        state.prev_tab();
        assert_eq!(state.tab, StatsTab::Overview);
    }

    #[test]
    fn paid_usage_keeps_cost_heatmap_in_tab_cycle() {
        let mut state = free_state();
        state.data.as_mut().unwrap().total_cost_cents = 1.0;
        state.tab = StatsTab::DailyTokens;
        state.next_tab();
        assert_eq!(state.tab, StatsTab::CostHeatmap);
        state.next_tab();
        assert_eq!(state.tab, StatsTab::Models);
        state.prev_tab();
        assert_eq!(state.tab, StatsTab::CostHeatmap);
    }

    #[test]
    fn free_models_render_tokens_without_cost_column() {
        let state = free_state();
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        render_models(&state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Model"));
        assert!(content.contains("Input"));
        assert!(content.contains("Output"));
        assert!(!content.contains("Cost"));
        assert!(!content.contains('$'));
        assert!(content.contains("1.2K"));
    }

    #[test]
    fn free_overview_uses_token_summary_without_cost() {
        let state = free_state();
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("1.5K tokens"));
        assert!(!content.contains('$'));
    }

    #[test]
    fn free_render_hides_cost_heatmap_tab_and_remaps_stale_selection() {
        let mut state = free_state();
        state.visible = true;
        state.tab = StatsTab::CostHeatmap;
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        render_stats_dialog(&state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(!content.contains("Cost Heatmap"));
        assert!(content.contains("Daily Tokens"));
        assert!(content.contains("No data yet."));
    }

    #[test]
    fn paid_render_keeps_cost_heatmap_tab() {
        let mut state = free_state();
        state.visible = true;
        state.data.as_mut().unwrap().total_cost_cents = 1.0;
        state.model_breakdown[0].cost_usd = 0.01;
        state.tab = StatsTab::CostHeatmap;
        let area = Rect::new(0, 0, 100, 40);
        let mut buf = Buffer::empty(area);
        render_stats_dialog(&state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Cost Heatmap"));
    }

    #[test]
    fn legacy_stats_entry_without_cache_fields_deserializes() {
        let entry: StatsEntry = serde_json::from_str(
            r#"{
                "timestamp_ms": 1700000000000,
                "model": "free/model",
                "input_tokens": 100,
                "output_tokens": 20,
                "cost_cents": 0.0
            }"#,
        )
        .expect("legacy stats records should remain readable");
        assert_eq!(entry.cache_read_tokens, 0);
        assert_eq!(entry.cache_write_tokens, 0);
    }

    #[test]
    fn cache_usage_aggregates_and_renders_only_when_present() {
        let mut agg = AggregatedStats::default();
        let mut daily = HashMap::new();
        accumulate_entry(
            &mut agg,
            &mut daily,
            &StatsEntry {
                model: "free/model".into(),
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 500,
                cache_write_tokens: 40,
                timestamp_ms: 1_700_000_000_000,
                ..StatsEntry::default()
            },
        );
        assert_eq!(agg.total_cache_read_tokens, 500);
        assert_eq!(agg.total_cache_write_tokens, 40);
        assert_eq!(
            format_cache_summary(500, 40).as_deref(),
            Some("500 read · 40 written")
        );
        assert_eq!(format_cache_summary(0, 0), None);

        let mut state = free_state();
        state.data = Some(agg);
        let area = Rect::new(0, 0, 100, 20);
        let mut buf = Buffer::empty(area);
        render_overview(state.data.as_ref().unwrap(), &state, area, &mut buf);
        let content: String = buf.content().iter().map(|cell| cell.symbol()).collect();
        assert!(content.contains("Cache:"));
        assert!(content.contains("500 read"));
    }

    // ---- model breakdown: add_model_usage ----------------------------------

    #[test]
    fn test_add_model_usage_new_model() {
        let mut state = StatsDialogState::new();
        state.add_model_usage("claude-3-opus", 1000, 500, 0.05);

        assert_eq!(state.model_breakdown.len(), 1);
        let e = &state.model_breakdown[0];
        assert_eq!(e.model_id, "claude-3-opus");
        assert_eq!(e.input_tokens, 1000);
        assert_eq!(e.output_tokens, 500);
        assert!((e.cost_usd - 0.05).abs() < 1e-9);
    }

    #[test]
    fn test_add_model_usage_accumulates_same_model() {
        let mut state = StatsDialogState::new();
        state.add_model_usage("claude-3-opus", 1000, 500, 0.05);
        state.add_model_usage("claude-3-opus", 2000, 800, 0.10);

        assert_eq!(state.model_breakdown.len(), 1);
        let e = &state.model_breakdown[0];
        assert_eq!(e.input_tokens, 3000);
        assert_eq!(e.output_tokens, 1300);
        assert!((e.cost_usd - 0.15).abs() < 1e-9);
    }

    #[test]
    fn test_add_model_usage_multiple_models() {
        let state = make_state_with_models(&[
            ("claude-3-opus", 1000, 500, 0.05),
            ("claude-3-haiku", 500, 200, 0.01),
            ("claude-3-sonnet", 800, 400, 0.03),
        ]);

        assert_eq!(state.model_breakdown.len(), 3);
        let ids: Vec<&str> = state
            .model_breakdown
            .iter()
            .map(|e| e.model_id.as_str())
            .collect();
        assert!(ids.contains(&"claude-3-opus"));
        assert!(ids.contains(&"claude-3-haiku"));
        assert!(ids.contains(&"claude-3-sonnet"));
    }

    #[test]
    fn test_model_breakdown_totals() {
        let state = make_state_with_models(&[
            ("model-a", 1_000_000, 200_000, 1.00),
            ("model-b", 500_000, 100_000, 0.50),
        ]);
        let total_input: u64 = state.model_breakdown.iter().map(|e| e.input_tokens).sum();
        let total_output: u64 = state.model_breakdown.iter().map(|e| e.output_tokens).sum();
        let total_cost: f64 = state.model_breakdown.iter().map(|e| e.cost_usd).sum();
        assert_eq!(total_input, 1_500_000);
        assert_eq!(total_output, 300_000);
        assert!((total_cost - 1.50).abs() < 1e-9);
    }

    // ---- streak tracking ---------------------------------------------------

    #[test]
    fn test_streak_consecutive_days() {
        let agg = make_agg_with_dates(&["2025-01-01", "2025-01-02", "2025-01-03"]);
        let (current, longest) = compute_streaks(&agg);
        assert_eq!(current, 3);
        assert_eq!(longest, 3);
    }

    #[test]
    fn test_streak_gap_resets_current() {
        // Two separate runs: 3 days then a gap, then 2 days.
        let agg = make_agg_with_dates(&[
            "2025-01-01",
            "2025-01-02",
            "2025-01-03",
            "2025-01-10",
            "2025-01-11",
        ]);
        let (current, longest) = compute_streaks(&agg);
        assert_eq!(current, 2);
        assert_eq!(longest, 3);
    }

    #[test]
    fn test_streak_single_day() {
        let agg = make_agg_with_dates(&["2025-03-15"]);
        let (current, longest) = compute_streaks(&agg);
        assert_eq!(current, 1);
        assert_eq!(longest, 1);
    }

    #[test]
    fn test_streak_empty() {
        let agg = AggregatedStats::default();
        let (current, longest) = compute_streaks(&agg);
        assert_eq!(current, 0);
        assert_eq!(longest, 0);
    }

    #[test]
    fn test_streak_longer_tail_wins_longest() {
        // Five days, then a gap, then one day.
        let agg = make_agg_with_dates(&[
            "2025-02-01",
            "2025-02-02",
            "2025-02-03",
            "2025-02-04",
            "2025-02-05",
            "2025-02-20",
        ]);
        let (current, longest) = compute_streaks(&agg);
        assert_eq!(current, 1);
        assert_eq!(longest, 5);
    }

    #[test]
    fn test_consecutive_dates_helper() {
        assert!(consecutive_dates("2025-01-31", "2025-02-01"));
        assert!(consecutive_dates("2024-02-28", "2024-02-29")); // 2024 is a leap year
        assert!(!consecutive_dates("2025-01-01", "2025-01-03"));
        assert!(!consecutive_dates("2025-01-05", "2025-01-04")); // reversed
    }

    // ---- heatmap color -----------------------------------------------------

    #[test]
    fn test_heatmap_color_zero() {
        assert_eq!(heatmap_color(0.0), Color::Rgb(30, 30, 30));
    }

    #[test]
    fn test_heatmap_color_max() {
        assert_eq!(heatmap_color(1.0), Color::Rgb(0, 255, 0));
    }

    #[test]
    fn test_heatmap_color_mid() {
        // 0.60 -> high bracket
        assert_eq!(heatmap_color(0.60), Color::Rgb(0, 200, 0));
    }

    // ---- build_model_breakdown sorting -------------------------------------

    #[test]
    fn test_build_model_breakdown_sorted_by_cost_desc() {
        let mut agg = AggregatedStats::default();
        agg.by_model.insert(
            "cheap".to_string(),
            ModelStats {
                input_tokens: 100,
                output_tokens: 50,
                cost_cents: 10.0,
                turns: 1,
            },
        );
        agg.by_model.insert(
            "expensive".to_string(),
            ModelStats {
                input_tokens: 200,
                output_tokens: 100,
                cost_cents: 500.0,
                turns: 2,
            },
        );
        agg.by_model.insert(
            "mid".to_string(),
            ModelStats {
                input_tokens: 150,
                output_tokens: 75,
                cost_cents: 100.0,
                turns: 1,
            },
        );

        let breakdown = build_model_breakdown(&agg);
        assert_eq!(breakdown[0].model_id, "expensive");
        assert_eq!(breakdown[1].model_id, "mid");
        assert_eq!(breakdown[2].model_id, "cheap");
    }
}
