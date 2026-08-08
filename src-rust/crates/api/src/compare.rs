//! Shared smart-router comparison report.
//!
//! This module is deliberately provider-registry based and never performs a
//! network request. Commands and the TUI consume the same joined telemetry so
//! rankings and filters cannot drift between surfaces.

use crate::registry::{
    UpstreamCooldownSummaries, UpstreamDispatchCountSummaries, UpstreamKeyHealthSummaries,
    UpstreamLatencySummaries, UpstreamSuccessRateSummaries, UpstreamTaskSuccessRateSummaries,
};
use crate::ProviderRegistry;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompareFilters {
    pub task: Option<String>,
    pub provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareRow {
    pub provider: String,
    pub upstream: String,
    pub dispatches: u32,
    pub success_rate: Option<f64>,
    pub task_success_rate: Option<f64>,
    pub latency_secs: Option<f64>,
    pub cooldown: Option<String>,
    pub key_health: Option<(usize, usize, Option<u64>)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompareReport {
    pub filters: CompareFilters,
    pub rows: Vec<CompareRow>,
}

pub fn parse_compare_args(args: &str) -> Result<CompareFilters, String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut filters = CompareFilters::default();
    let mut index = 0;
    while index < words.len() {
        match words[index] {
            "--task" | "-t" => {
                if filters.task.is_some() {
                    return Err("Only one task filter may be supplied".to_string());
                }
                index += 1;
                let value = words.get(index).ok_or("--task requires a value")?;
                if value.starts_with('-') {
                    return Err("--task requires a value".to_string());
                }
                filters.task = Some((*value).to_ascii_lowercase());
            }
            value if value.starts_with("--task=") => {
                let task = value.trim_start_matches("--task=");
                if task.is_empty() {
                    return Err("--task requires a value".to_string());
                }
                if filters.task.is_some() {
                    return Err("Only one task filter may be supplied".to_string());
                }
                filters.task = Some(task.to_ascii_lowercase());
            }
            "--provider" | "-p" => {
                if filters.provider.is_some() {
                    return Err("Only one provider filter may be supplied".to_string());
                }
                index += 1;
                let value = words.get(index).ok_or("--provider requires a value")?;
                if value.starts_with('-') {
                    return Err("--provider requires a value".to_string());
                }
                filters.provider = Some((*value).to_ascii_lowercase());
            }
            value if value.starts_with("--provider=") => {
                let provider = value.trim_start_matches("--provider=");
                if provider.is_empty() {
                    return Err("--provider requires a value".to_string());
                }
                if filters.provider.is_some() {
                    return Err("Only one provider filter may be supplied".to_string());
                }
                filters.provider = Some(provider.to_ascii_lowercase());
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "Unknown option '{}'. Use --task or --provider.",
                    value
                ));
            }
            value => {
                if filters.task.is_some() {
                    return Err("Only one task filter may be supplied".to_string());
                }
                filters.task = Some(value.to_ascii_lowercase());
            }
        }
        index += 1;
    }
    Ok(filters)
}

pub fn build_compare_report(registry: &ProviderRegistry, filters: CompareFilters) -> CompareReport {
    build_compare_report_from_summaries(
        registry.upstream_dispatch_count_summaries(),
        registry.upstream_success_rate_summaries(),
        registry.upstream_latency_summaries(),
        registry.upstream_task_success_rate_summaries(),
        registry.upstream_cooldown_summaries(),
        registry.upstream_key_health_summaries(),
        filters,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_compare_report_from_summaries(
    dispatches: UpstreamDispatchCountSummaries,
    successes: UpstreamSuccessRateSummaries,
    latencies: UpstreamLatencySummaries,
    task_successes: UpstreamTaskSuccessRateSummaries,
    cooldowns: UpstreamCooldownSummaries,
    key_health: UpstreamKeyHealthSummaries,
    filters: CompareFilters,
) -> CompareReport {
    // Union every telemetry source so an unhealthy or newly configured upstream
    // remains visible even before it has recorded a dispatch.
    let mut identities = std::collections::BTreeSet::new();
    for (provider, entries) in &dispatches {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _)| (provider.clone(), upstream.clone())),
        );
    }
    for (provider, entries) in &successes {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _)| (provider.clone(), upstream.clone())),
        );
    }
    for (provider, entries) in &latencies {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _)| (provider.clone(), upstream.clone())),
        );
    }
    for (provider, entries) in &task_successes {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _)| (provider.clone(), upstream.clone())),
        );
    }
    for (provider, entries) in &cooldowns {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _, _)| (provider.clone(), upstream.clone())),
        );
    }
    for (provider, entries) in &key_health {
        identities.extend(
            entries
                .iter()
                .map(|(upstream, _, _, _)| (provider.clone(), upstream.clone())),
        );
    }

    let mut rows = Vec::new();
    for (provider, upstream) in identities {
        if !matches_filter(&provider, &upstream, &filters) {
            continue;
        }
        let success_rate = find_metric(&successes, &provider, &upstream);
        let latency_secs = find_metric(&latencies, &provider, &upstream);
        let task_success_rate = filters
            .task
            .as_deref()
            .and_then(|task| find_task_metric(&task_successes, &provider, &upstream, task));
        rows.push(CompareRow {
            provider: provider.clone(),
            upstream: upstream.clone(),
            dispatches: find_dispatch_count(&dispatches, &provider, &upstream),
            success_rate,
            task_success_rate,
            latency_secs,
            cooldown: find_cooldown(&cooldowns, &provider, &upstream),
            key_health: find_key_health(&key_health, &provider, &upstream),
        });
    }
    rows.sort_by(|left, right| {
        let left_rate = left.task_success_rate.or(left.success_rate);
        let right_rate = right.task_success_rate.or(right.success_rate);
        right_rate
            .is_some()
            .cmp(&left_rate.is_some())
            .then_with(|| {
                right_rate
                    .unwrap_or_default()
                    .total_cmp(&left_rate.unwrap_or_default())
            })
            .then_with(|| {
                left.latency_secs
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right.latency_secs.unwrap_or(f64::INFINITY))
            })
            .then_with(|| right.dispatches.cmp(&left.dispatches))
            .then_with(|| left.upstream.cmp(&right.upstream))
    });
    CompareReport { filters, rows }
}

fn find_dispatch_count(
    summaries: &UpstreamDispatchCountSummaries,
    provider: &str,
    upstream: &str,
) -> u32 {
    summaries
        .iter()
        .find(|(name, _)| name == provider)
        .and_then(|(_, entries)| entries.iter().find(|(name, _)| name == upstream))
        .map_or(0, |(_, count)| *count)
}

fn matches_filter(provider: &str, upstream: &str, filters: &CompareFilters) -> bool {
    filters.provider.as_deref().is_none_or(|needle| {
        provider.to_ascii_lowercase().contains(needle)
            || upstream.to_ascii_lowercase().contains(needle)
    })
}

fn find_metric(
    summaries: &UpstreamSuccessRateSummaries,
    provider: &str,
    upstream: &str,
) -> Option<f64> {
    summaries
        .iter()
        .find(|(name, _)| name == provider)
        .and_then(|(_, entries)| entries.iter().find(|(name, _)| name == upstream))
        .and_then(|(_, value)| *value)
}

fn find_task_metric(
    summaries: &UpstreamTaskSuccessRateSummaries,
    provider: &str,
    upstream: &str,
    task: &str,
) -> Option<f64> {
    summaries
        .iter()
        .find(|(name, _)| name == provider)
        .and_then(|(_, entries)| entries.iter().find(|(name, _)| name == upstream))
        .and_then(|(_, tasks)| {
            let matching_rates: Vec<f64> = tasks
                .iter()
                .filter(|(name, _)| task_matches(name, task))
                .filter_map(|(_, rate)| *rate)
                .collect();
            (!matching_rates.is_empty())
                .then(|| matching_rates.iter().sum::<f64>() / matching_rates.len() as f64)
        })
}

fn task_matches(key: &str, filter: &str) -> bool {
    let normalized = filter.replace(['-', ' '], "_");
    match normalized.as_str() {
        "coding" | "code" => matches!(key, "code_generation" | "code_edit"),
        "simple" => key == "simple_edit",
        value => key == value,
    }
}

fn find_cooldown(
    summaries: &UpstreamCooldownSummaries,
    provider: &str,
    upstream: &str,
) -> Option<String> {
    summaries
        .iter()
        .find(|(name, _)| name == provider)
        .and_then(|(_, entries)| {
            entries
                .iter()
                .find(|(name, _, remaining)| name == upstream && remaining.is_some())
        })
        .map(|(_, kind, remaining)| {
            format!("{} cooldown ({}s)", kind, remaining.unwrap_or_default())
        })
}

fn find_key_health(
    summaries: &UpstreamKeyHealthSummaries,
    provider: &str,
    upstream: &str,
) -> Option<(usize, usize, Option<u64>)> {
    summaries
        .iter()
        .find(|(name, _)| name == provider)
        .and_then(|(_, entries)| entries.iter().find(|(name, _, _, _)| name == upstream))
        .map(|(_, active, total, retry)| (*active, *total, *retry))
}

pub fn format_compare_report(report: &CompareReport) -> String {
    let scope = match (&report.filters.task, &report.filters.provider) {
        (Some(task), Some(provider)) => format!("task={}, provider={}", task, provider),
        (Some(task), None) => format!("task={}", task),
        (None, Some(provider)) => format!("provider={}", provider),
        (None, None) => "all tasks".to_string(),
    };
    let mut output = format!("Smart-router comparison ({scope})\n\n");
    if report.rows.is_empty() {
        output.push_str("No recorded free-provider dispatches match this filter.\nRun a few free-mode requests first, then use /compare again.\n");
        return output;
    }
    output.push_str("Rank  Upstream          Success   Latency   Dispatches  Health\n");
    output.push_str("────  ────────────────  ────────  ────────  ──────────  ─────────────\n");
    for (index, row) in report.rows.iter().enumerate() {
        let rate = row
            .task_success_rate
            .or(row.success_rate)
            .map(|value| format!("{:>6.1}%", value * 100.0))
            .unwrap_or_else(|| "   n/a".to_string());
        let latency = row
            .latency_secs
            .map(format_latency)
            .unwrap_or_else(|| "n/a".to_string());
        let health = row
            .cooldown
            .clone()
            .or_else(|| {
                row.key_health.map(|(active, total, retry)| {
                    retry.map_or(format!("keys {active}/{total}"), |seconds| {
                        format!("keys {active}/{total}, retry {seconds}s")
                    })
                })
            })
            .unwrap_or_else(|| "ready".to_string());
        output.push_str(&format!(
            "{:>4}  {:<16}  {:>8}  {:>8}  {:>10}  {}\n",
            index + 1,
            row.upstream,
            rate,
            latency,
            row.dispatches,
            health
        ));
    }
    output.push_str("\nSorted by task success (or aggregate success), then latency and history.\nUse /compare <task>, /compare --provider <upstream>, or /routing edit for controls.");
    output
}

fn format_latency(seconds: f64) -> String {
    if seconds < 1.0 {
        format!("{:.0}ms", seconds * 1000.0)
    } else {
        format!("{:.1}s", seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_normalizes_filters() {
        assert_eq!(
            parse_compare_args("coding --provider groq").unwrap(),
            CompareFilters {
                task: Some("coding".into()),
                provider: Some("groq".into())
            }
        );
        assert_eq!(
            parse_compare_args("--task=reasoning").unwrap().task,
            Some("reasoning".into())
        );
        assert!(parse_compare_args("coding planning").is_err());
        assert!(parse_compare_args("--task=").is_err());
        assert!(parse_compare_args("--task coding --task reasoning").is_err());
        assert!(parse_compare_args("--provider").is_err());
        assert!(parse_compare_args("--provider groq --provider cerebras").is_err());
    }

    #[test]
    fn coding_filter_averages_generation_and_edit_rates() {
        let report = build_compare_report_from_summaries(
            vec![("free".into(), vec![("groq".into(), 4)])],
            vec![],
            vec![],
            vec![(
                "free".into(),
                vec![(
                    "groq".into(),
                    vec![
                        ("code_generation".into(), Some(0.8)),
                        ("code_edit".into(), Some(0.6)),
                    ],
                )],
            )],
            vec![],
            vec![],
            CompareFilters {
                task: Some("coding".into()),
                provider: None,
            },
        );
        assert_eq!(report.rows[0].task_success_rate, Some(0.7));
    }

    #[test]
    fn report_includes_health_only_upstreams() {
        let report = build_compare_report_from_summaries(
            vec![("free".into(), vec![("groq".into(), 2)])],
            vec![],
            vec![],
            vec![],
            vec![(
                "free".into(),
                vec![("cerebras".into(), "rate-limit".into(), Some(30))],
            )],
            vec![(
                "free".into(),
                vec![
                    ("groq".into(), 2, 2, None),
                    ("cerebras".into(), 0, 1, Some(30)),
                ],
            )],
            CompareFilters::default(),
        );
        assert_eq!(report.rows.len(), 2);
        let cerebras = report
            .rows
            .iter()
            .find(|row| row.upstream == "cerebras")
            .unwrap();
        assert_eq!(cerebras.dispatches, 0);
        assert!(cerebras.cooldown.as_deref().unwrap().contains("30s"));
    }
}
