// upstream_health.rs — Agent-facing upstream performance tool.
//
// Exposes the free router's measured per-upstream telemetry (spec §8.6) to
// the model as a read-only tool: dispatch success rate, average latency,
// dispatch count (the trust signal), and per-task success rates. The agent
// can call this to reason about upstream health — e.g. why a provider keeps
// failing, whether to pin a task to a faster upstream, or how much history a
// rate is based on.
//
// Zero token cost when idle: nothing is injected into the prompt; the data is
// produced only when the tool is invoked. The tool reads the live
// `ProviderRegistry` from `ToolContext` — the same registry the router runs
// against — so it sees in-session measurements, not a disk snapshot.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};

/// Format a compact human-readable health report from the provider summary
/// vectors. Pure function so the report layout is unit-testable without a
/// live registry.
pub fn format_health_report(
    latencies: &[(String, Option<f64>)],
    success_rates: &[(String, Option<f64>)],
    task_rates: &[(String, Vec<(String, Option<f64>)>)],
    dispatch_counts: &[(String, u32)],
) -> String {
    // Union of every upstream id across the sources, deduped and sorted for a
    // stable report.
    let mut ids: Vec<String> = Vec::new();
    for id in latencies
        .iter()
        .map(|(id, _)| id)
        .chain(success_rates.iter().map(|(id, _)| id))
        .chain(task_rates.iter().map(|(id, _)| id))
        .chain(dispatch_counts.iter().map(|(id, _)| id))
    {
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.clone());
        }
    }
    ids.sort();

    if ids.is_empty() {
        return "No upstream performance data recorded yet.".to_string();
    }

    let rate_of = |id: &str| -> Option<f64> {
        success_rates
            .iter()
            .find(|(u, _)| u == id)
            .and_then(|(_, r)| *r)
    };
    let latency_of = |id: &str| -> Option<f64> {
        latencies
            .iter()
            .find(|(u, _)| u == id)
            .and_then(|(_, l)| *l)
    };
    let count_of = |id: &str| -> u32 {
        dispatch_counts
            .iter()
            .find(|(u, _)| u == id)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    };

    let mut lines = vec![
        "Free upstream health (measured over this session)".to_string(),
        "(rates are reliable once an upstream has 3+ dispatches)".to_string(),
    ];
    for id in &ids {
        let rate_cell = match rate_of(id) {
            Some(r) => format!("{:>4.0}% success", r * 100.0),
            None => "    — success".to_string(),
        };
        let latency_cell = match latency_of(id) {
            Some(s) => format!("{:.1}s avg", s),
            None => "— avg".to_string(),
        };
        let mut line = format!(
            "  {:<14}{} · {} · {} dispatch",
            id,
            rate_cell,
            latency_cell,
            count_of(id),
        );
        if let Some(tasks) = task_rates.iter().find(|(u, _)| u == id) {
            let parts: Vec<String> = tasks
                .1
                .iter()
                .filter_map(|(k, r)| r.map(|rate| format!("{} {:.0}%", k, rate * 100.0)))
                .collect();
            if !parts.is_empty() {
                line.push_str(&format!(" · [{}]", parts.join(", ")));
            }
        }
        lines.push(line);
    }
    lines.join("\n")
}

/// Read-only tool: report measured upstream performance (success rate,
/// latency, dispatch count, per-task rates) from the live provider registry.
pub struct UpstreamHealthTool;

#[async_trait]
impl Tool for UpstreamHealthTool {
    fn name(&self) -> &str {
        "UpstreamHealth"
    }

    fn description(&self) -> &str {
        "Report measured per-upstream performance for the free-mode router: dispatch success rate, \
         average latency, dispatch count (how much history the rate is based on), and per-task \
         success rates. Read-only; use it to reason about which providers are currently healthy \
         and reliable before relying on them."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _input: Value, ctx: &ToolContext) -> ToolResult {
        let Some(registry) = &ctx.provider_registry else {
            return ToolResult::success("No provider health data available in this context.");
        };
        let free_id =
            clawde_core::provider_id::ProviderId::new(clawde_core::provider_id::ProviderId::FREE);
        let Some(provider) = registry.get(&free_id) else {
            return ToolResult::success("No free-mode provider is registered in this context.");
        };
        let report = format_health_report(
            &provider.upstream_latencies(),
            &provider.upstream_success_rates(),
            &provider.upstream_task_success_rates(),
            &provider.upstream_dispatch_counts(),
        );
        ToolResult::success(report)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_shows_all_columns_and_task_rates() {
        let report = format_health_report(
            &[
                ("groq".to_string(), Some(0.4)),
                ("nvidia".to_string(), None),
            ],
            &[
                ("groq".to_string(), Some(1.0)),
                ("nvidia".to_string(), None),
            ],
            &[(
                "groq".to_string(),
                vec![
                    ("code_generation".to_string(), Some(0.5)),
                    ("verification".to_string(), Some(1.0)),
                ],
            )],
            &[("groq".to_string(), 8), ("nvidia".to_string(), 3)],
        );
        assert!(report.contains("groq"), "report must list groq");
        assert!(report.contains("100% success"));
        assert!(report.contains("0.4s avg"));
        assert!(report.contains("8 dispatch"));
        assert!(report.contains("code_generation 50%, verification 100%"));
        // No samples → em-dash cells, no per-task section on that upstream's
        // own line (the whole report may still contain brackets from groq).
        let nv_line = report
            .lines()
            .find(|l| l.contains("nvidia"))
            .unwrap_or_default();
        assert!(nv_line.contains("— success"));
        assert!(nv_line.contains("— avg"));
        assert!(
            !nv_line.contains("["),
            "no per-task brackets on the nvidia line"
        );
    }

    #[test]
    fn report_sorts_upstreams_stably_and_dedupes() {
        let report = format_health_report(
            &[("z".to_string(), Some(1.0)), ("a".to_string(), Some(2.0))],
            &[("a".to_string(), Some(0.5)), ("z".to_string(), Some(0.9))],
            &[],
            &[("z".to_string(), 1)],
        );
        let a_pos = report.find("\n  a").unwrap();
        let z_pos = report.find("\n  z").unwrap();
        assert!(a_pos < z_pos, "upstreams must be sorted alphabetically");
        assert_eq!(report.matches("  z").count(), 1, "duplicate ids collapse");
    }

    #[test]
    fn report_empty_when_no_data() {
        let report = format_health_report(&[], &[], &[], &[]);
        assert!(report.contains("No upstream performance data"));
    }

    #[tokio::test]
    async fn execute_without_registry_reports_unavailable() {
        let tool = UpstreamHealthTool;
        let ctx = crate::test_support::allow_all_context(std::path::PathBuf::from("/tmp"));
        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.content.contains("No provider health data"));
    }
}
