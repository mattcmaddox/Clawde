// Provider-health status block for `/status`.
//
// This module backs the provider-health section of the `/status` command. It
// reads persisted free-mode runtime state (empty-completion cooldowns,
// per-upstream dispatch telemetry) so users can see why routing chose an
// upstream without digging through state files.

/// Gather provider status information (cooldowns, success rates, routing
/// configuration). Consumed by the session-status `/status` command in
/// `lib.rs` so both views share one invocation.
pub(crate) fn gather_provider_status() -> String {
    let mut lines = vec!["Provider Status:\n".to_string()];

    // Load cooldown state from disk if available
    let cooldown_path = clawde_core::config::Settings::config_dir()
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
    let telemetry_path = clawde_core::config::Settings::config_dir()
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

    // Show configuration
    lines.push("\nConfiguration:".to_string());
    lines.push("  Routing strategy: Auto (task-based)".to_string());
    lines.push("  Parallel attempts: 2 (enabled for prompts <50K tokens)".to_string());
    lines.push("  Exponential backoff: Enabled (±20% jitter)".to_string());

    lines.join("\n")
}
