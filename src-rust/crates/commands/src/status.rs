// Provider-health status block for `/status`.
//
// This module backs the provider-health section of the `/status` command. It
// reads persisted free-mode runtime state (empty-completion cooldowns,
// per-upstream dispatch telemetry) so users can see why routing chose an
// upstream without digging through state files.

/// Gather provider status information (cooldowns, success rates, routing
/// configuration). Consumed by the session-status `/status` command in
/// `lib.rs` so both views share one invocation.
///
/// The configuration block reports the *live* settings — the same
/// `providers.free.options.routing` object `build_free_provider` reads, plus
/// the shipped provider profiles — instead of fixed claims about the default
/// setup. It previously printed a hardcoded "Auto (task-based)" and a
/// "Parallel attempts: 2 (prompts <50K tokens)" line for behaviour no version
/// of this code has had: nothing gates attempts on a token count, and hedging
/// ships disabled.
pub(crate) fn gather_provider_status(config: &clawde_core::config::Config) -> String {
    let mut lines = vec!["Provider Status:\n".to_string()];

    // Load cooldown state from disk if available
    let cooldown_path = clawde_core::config::Settings::state_dir()
        .join("empty-cooldown-state")
        .join("free.json");

    if cooldown_path.exists() {
        match std::fs::read_to_string(&cooldown_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => {
                    lines.push("Cooldown States:".to_string());
                    if let Some(cooldowns) = json.get("cooldown_until_unix") {
                        if let Some(arr) = cooldowns.as_array() {
                            for (i, entry) in arr.iter().enumerate() {
                                if let Some(ts) = entry.as_u64() {
                                    let now = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    if ts > now {
                                        let remaining = ts - now;
                                        lines.push(format!(
                                            "  Upstream {}: {}s remaining",
                                            i, remaining
                                        ));
                                    } else {
                                        lines.push(format!("  Upstream {}: OK", i));
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    lines.push(format!("  Error parsing cooldown state: {}", e));
                }
            },
            Err(e) => {
                lines.push(format!("  No cooldown state found: {}", e));
            }
        }
    } else {
        lines.push("No cooldown state file found.".to_string());
    }

    // Load telemetry state from disk if available
    let telemetry_path = clawde_core::config::Settings::state_dir()
        .join("telemetry-state")
        .join("free.json");

    if telemetry_path.exists() {
        match std::fs::read_to_string(&telemetry_path) {
            Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                Ok(json) => {
                    lines.push("\nSuccess Rates:".to_string());
                    if let Some(upstreams) = json.get("upstreams") {
                        if let Some(obj) = upstreams.as_object() {
                            for (provider, data) in obj {
                                let successes =
                                    data.get("successes").and_then(|v| v.as_u64()).unwrap_or(0);
                                let failures =
                                    data.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
                                let total = successes + failures;
                                let rate = if total > 0 {
                                    (successes as f64 / total as f64 * 100.0) as u32
                                } else {
                                    0
                                };
                                lines.push(format!(
                                    "  {}: {}% ({}/{} success/total)",
                                    provider, rate, successes, total
                                ));
                            }
                        }
                    }
                }
                Err(e) => {
                    lines.push(format!("  Error parsing telemetry: {}", e));
                }
            },
            Err(e) => {
                lines.push(format!("  No telemetry found: {}", e));
            }
        }
    } else {
        lines.push("\nNo telemetry file found.".to_string());
    }

    // Show configuration — read it, never restate a default.
    let routing = config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
        .and_then(|v| {
            serde_json::from_value::<clawde_api::providers::free::RoutingConfig>(v.clone()).ok()
        })
        .unwrap_or_default();
    let profiles = clawde_api::providers::free::ProviderProfiles::load();
    let strategy = crate::routing::resolve_routing_strategy_name(config);
    // Auto and task_based both route by request type; only they have per-task
    // preferences to name.
    let task_note = if strategy == "auto" || strategy == "task_based" {
        " (task-based)"
    } else {
        ""
    };

    lines.push("\nConfiguration:".to_string());
    lines.push(format!("  Routing strategy: {strategy}{task_note}"));
    lines.push(format!(
        "  Same-upstream retries: {} per upstream",
        routing.fallback_retries
    ));
    lines.push(format!(
        "  Disabled upstreams: {}",
        if routing.disabled_upstreams.is_empty() {
            "none".to_string()
        } else {
            routing.disabled_upstreams.join(", ")
        }
    ));
    lines.push(format!(
        "  Parallel attempts: {} (hedging {}), hedge delay {}ms",
        profiles.parallel.strategy,
        if profiles.parallel.hedging.enabled {
            "on"
        } else {
            "off"
        },
        profiles.parallel.hedging.delay_ms,
    ));
    lines.push(format!(
        "  Walk budget: {}",
        if routing.turn_walk_budget_secs == 0 {
            "unbounded".to_string()
        } else {
            format!("{}s per dispatch", routing.turn_walk_budget_secs)
        }
    ));

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::config::{Config, ProviderConfig};
    use std::collections::HashMap;

    fn config_with_routing(routing: serde_json::Value) -> Config {
        let mut options = HashMap::new();
        options.insert("routing".to_string(), routing);
        let mut provider_configs = HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            ProviderConfig {
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
    fn configuration_block_reports_the_live_routing_config() {
        let out = gather_provider_status(&config_with_routing(serde_json::json!({
            "strategy": "sequential",
            "fallback_retries": 3,
            "disabled_upstreams": ["groq"],
            "turn_walk_budget_secs": 90
        })));

        assert!(out.contains("Routing strategy: sequential"), "{out}");
        assert!(
            !out.contains("(task-based)"),
            "chained strategies are not task-based: {out}"
        );
        assert!(out.contains("Same-upstream retries: 3"), "{out}");
        assert!(out.contains("Disabled upstreams: groq"), "{out}");
        assert!(out.contains("Walk budget: 90s per dispatch"), "{out}");
    }

    #[test]
    fn configuration_block_never_states_a_default_that_is_not_running() {
        // With no routing block the defaults apply, and they must be the real
        // ones: /status used to print "Parallel attempts: 2 (enabled for prompts
        // <50K tokens)" for a gate that does not exist, next to hedging that
        // ships disabled.
        let out = gather_provider_status(&Config::default());

        assert!(out.contains("Routing strategy: auto (task-based)"), "{out}");
        assert!(out.contains("Same-upstream retries: 1"), "{out}");
        assert!(out.contains("Disabled upstreams: none"), "{out}");
        assert!(out.contains("hedging off"), "{out}");
        assert!(out.contains("Walk budget: 240s per dispatch"), "{out}");
        assert!(
            !out.contains("50K tokens"),
            "the token-counted parallel-attempt claim was never true: {out}"
        );
    }

    #[test]
    fn a_zero_walk_budget_reads_as_unbounded() {
        let out = gather_provider_status(&config_with_routing(serde_json::json!({
            "turn_walk_budget_secs": 0
        })));
        assert!(out.contains("Walk budget: unbounded"), "{out}");
    }
}
