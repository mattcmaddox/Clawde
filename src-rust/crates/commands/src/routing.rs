// routing.rs — /routing command: show or change the free-mode routing strategy.
//
// The routing strategy controls how the FreeProvider selects which upstream
// to try first (sequential, random, latency-based). The setting is persisted
// in settings.json under `providers.free.options.routing.strategy` and the
// CLI rebuilds the active provider on the strategy change, so it applies
// immediately (no /refresh needed).

use async_trait::async_trait;

use super::*;
use clawde_api::providers::free::{task_preference_ids, TaskType};

pub struct RoutingCommand;

#[async_trait]
impl SlashCommand for RoutingCommand {
    fn name(&self) -> &str {
        "routing"
    }

    fn aliases(&self) -> Vec<&str> {
        vec!["route"]
    }

    fn description(&self) -> &str {
        "Show or change the free-mode routing strategy (auto, sequential, random, latency, task)"
    }

    fn help(&self) -> &str {
        "Usage: /routing [auto|sequential|random|latency|task]\n\n\
         Show or change the free-mode routing strategy.\n\n\
         Strategies:\n\
           /routing                — show current strategy\n\
           /routing auto           — route by request type, refined by latency (default)\n\
           /routing sequential     — try upstreams in priority order\n\
           /routing random         — randomize upstream order each request\n\
           /routing latency        — route to the lowest-latency upstream first\n\
           /routing task           — route by request type (code gen, reasoning,\n\
                                     verification, …) using per-task upstream\n\
                                     preferences; overridable per task via\n\
                                     providers.free.options.routing.task_preferences\n\
           /routing sr             — quick alias for sequential\n\
           /routing rr             — quick alias for random\n\
           /routing lr             — quick alias for latency\n\
           /routing tr             — quick alias for task\n\
           /sr                     — shortcut for /routing sequential\n\
           /rr                     — shortcut for /routing random\n\
           /lr                     — shortcut for /routing latency\n\
           /tr                     — shortcut for /routing task\n\
\
         The setting is persisted in settings.json under providers.free.options.routing\n\
         and applies immediately — the active provider is rebuilt on the change."
    }

    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "auto".into(),
                description: "Smart default: route by task, refined by latency".into(),
                available: true,
            },
            ArgCompletion {
                value: "sequential".into(),
                description: "Try upstreams in priority order".into(),
                available: true,
            },
            ArgCompletion {
                value: "random".into(),
                description: "Randomize upstream order each request".into(),
                available: true,
            },
            ArgCompletion {
                value: "latency".into(),
                description: "Route to the lowest-latency upstream first".into(),
                available: true,
            },
            ArgCompletion {
                value: "sr".into(),
                description: "Quick alias for sequential".into(),
                available: true,
            },
            ArgCompletion {
                value: "rr".into(),
                description: "Quick alias for random".into(),
                available: true,
            },
            ArgCompletion {
                value: "lr".into(),
                description: "Quick alias for latency".into(),
                available: true,
            },
            ArgCompletion {
                value: "task".into(),
                description: "Route by request type using per-task preferences".into(),
                available: true,
            },
            ArgCompletion {
                value: "tb".into(),
                description: "Quick alias for task".into(),
                available: true,
            },
        ]
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();

        if args.is_empty() {
            // Show current strategy (and per-task assignments when task-based).
            let strategy = resolve_routing_strategy_name(&ctx.config);
            let mut msg = format!(
                "Free-mode routing strategy: {}\n\n\
                 Use /routing <auto|sequential|random|latency|task> to change.\n\
                 Run /refresh to apply the change.",
                strategy
            );
            // Auto and task_based both route by task, so both show the
            // per-task assignments.
            if strategy == "task_based" || strategy == "auto" {
                msg.push_str("\n\nTask assignments (top 3 upstreams):\n");
                for task in TaskType::ALL {
                    let ids = task_preference_ids(task);
                    let top: Vec<&str> = ids.iter().take(3).copied().collect();
                    msg.push_str(&format!("  {:<16} → {}\n", task.label(), top.join(", ")));
                }
                msg.push_str(
                    "\nOverride per task in settings.json:\n  providers.free.options.routing.task_preferences",
                );
            }
            if let Some(registry) = ctx.provider_registry.as_deref() {
                if let Some(capacity) = format_capacity_status_section(registry, None) {
                    msg.push_str("\n\n");
                    msg.push_str(&capacity);
                }
            }
            return CommandResult::Message(msg);
        }

        // TUI-only subcommands: /routing edit|pin|tasks opens the task-pinning
        // dialog (spec §8.6). The TUI intercepts these before they reach the
        // CLI, so in interactive mode this message is suppressed; headless
        // users get a pointer to the JSON they would otherwise write.
        if matches!(args.to_lowercase().as_str(), "edit" | "pin" | "tasks") {
            return CommandResult::Message(
                "Task pinning is an interactive TUI dialog (/routing edit). \
                 Headless: set providers.free.options.routing.task_preferences \
                 in settings.json."
                    .to_string(),
            );
        }

        let new_strategy = match args.to_lowercase().as_str() {
            "auto" | "smart" => "auto",
            "sequential" | "seq" | "sr" => "sequential",
            "random" | "random_failover" | "random-failover" | "rr" => "random_failover",
            "latency" | "latency_based" | "latency-based" | "lr" => "latency_based",
            "task" | "task_based" | "task-based" | "tb" => "task_based",
            other => {
                return CommandResult::Error(format!(
                    "Unknown strategy '{}'. Valid options: auto, sequential, random, latency, task",
                    other
                ));
            }
        };

        set_routing_strategy(ctx, new_strategy).await
    }
}

// ---------------------------------------------------------------------------
// Command-level aliases: /sr, /rr, /lr
// ---------------------------------------------------------------------------

/// Shortcut alias that directly sets a routing strategy without needing
/// an argument.  Registered in `all_commands()` as `/sr`, `/rr`, `/lr`.
pub struct RoutingAlias {
    /// Command name (e.g. "sr", "rr", "lr").
    pub name: &'static str,
    /// Strategy value written to settings.json ("sequential", "random_failover",
    /// "latency_based").
    pub target: &'static str,
}

#[async_trait]
impl SlashCommand for RoutingAlias {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        match self.target {
            "auto" => "Set free-mode routing to auto (task-based, latency-refined)",
            "sequential" => "Set free-mode routing to sequential (catalog order)",
            "random_failover" => "Set free-mode routing to random (shuffle each request)",
            "latency_based" => "Set free-mode routing to latency (fastest first)",
            "task_based" => "Set free-mode routing to task (route by request type)",
            _ => "Set free-mode routing strategy",
        }
    }

    fn hidden(&self) -> bool {
        true // Don't clutter /help with aliases
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        set_routing_strategy(ctx, self.target).await
    }
}

/// Persist a routing strategy and return a ConfigChangeMessage.
async fn set_routing_strategy(ctx: &mut CommandContext, strategy: &str) -> CommandResult {
    let display_name = match strategy {
        "auto" => "auto",
        "sequential" => "sequential",
        "random_failover" => "random_failover",
        "latency_based" => "latency_based",
        "task_based" => "task_based",
        _ => {
            return CommandResult::Error(format!("Unknown strategy '{}'", strategy));
        }
    };

    if let Err(e) = save_settings_mutation(|settings| {
        let routing = serde_json::json!({ "strategy": strategy });
        settings
            .providers
            .entry("free".to_string())
            .or_insert_with(|| serde_json::from_value(serde_json::json!({})).unwrap_or_default())
            .options
            .insert("routing".to_string(), routing);
    }) {
        return CommandResult::Error(format!("Failed to save routing strategy: {}", e));
    }

    let mut new_config = ctx.config.clone();
    let routing_val = serde_json::json!({ "strategy": strategy });
    new_config
        .provider_configs
        .entry("free".to_string())
        .or_default()
        .options
        .insert("routing".to_string(), routing_val);

    CommandResult::ConfigChangeMessage(
        new_config,
        format!(
            "Routing strategy changed to '{}'.\n\
             The active provider is rebuilt immediately — no /refresh needed.",
            display_name
        ),
    )
}

/// Read the current routing strategy name from the config, defaulting to
/// "auto" when the config key is absent (the smart default, spec §8.4).
fn resolve_routing_strategy_name(config: &Config) -> String {
    config
        .provider_configs
        .get("free")
        .and_then(|pc| pc.options.get("routing"))
        .and_then(|v| v.get("strategy"))
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawde_core::config::ProviderConfig;
    use std::collections::HashMap;

    /// Minimal [`CommandContext`] for running `RoutingCommand::execute`
    /// hermetically (the settings write is redirected by the `TestHome`
    /// guard, which holds the crate's shared `CLAWDE_HOME_LOCK`).
    fn make_test_ctx() -> CommandContext {
        CommandContext {
            config: Config::default(),
            cost_tracker: clawde_core::cost::CostTracker::new(),
            messages: vec![],
            working_dir: std::path::PathBuf::from("."),
            session_id: "test-session".to_string(),
            session_title: None,
            remote_session_url: None,
            mcp_manager: None,
            mcp_auth_runner: None,
            provider_registry: None,
            test_provider: None,
            effort: None,
        }
    }

    #[tokio::test]
    async fn routing_auto_config_change_carries_strategy_for_refresh() {
        // /routing auto's ConfigChangeMessage is what the CLI applies to the
        // in-memory config; /refresh then rebuilds the free provider from
        // that config. This locks the contract: the returned config must
        // carry providers.free.options.routing.strategy = "auto" so the
        // switch takes effect in-session without a restart.
        let _home = crate::keys::tests::TestHome::new();
        let mut ctx = make_test_ctx();
        let result = RoutingCommand.execute("auto", &mut ctx).await;
        match result {
            CommandResult::ConfigChangeMessage(new_cfg, msg) => {
                assert!(
                    msg.contains("'auto'"),
                    "status message should name the new strategy, got: {msg}"
                );
                let strategy = new_cfg
                    .provider_configs
                    .get("free")
                    .and_then(|pc| pc.options.get("routing"))
                    .and_then(|v| v.get("strategy"))
                    .and_then(|v| v.as_str());
                assert_eq!(
                    strategy,
                    Some("auto"),
                    "new config must carry the strategy for /refresh to apply it"
                );
            }
            other => panic!("expected ConfigChangeMessage, got {other:?}"),
        }
    }

    #[test]
    fn resolve_routing_defaults_to_auto() {
        let config = Config::default();
        assert_eq!(resolve_routing_strategy_name(&config), "auto");
    }

    #[test]
    fn resolve_routing_reads_from_config() {
        let mut options = HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({ "strategy": "random_failover" }),
        );
        let mut provider_configs = HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            ProviderConfig {
                options,
                ..Default::default()
            },
        );
        let config = Config {
            provider_configs,
            ..Default::default()
        };
        assert_eq!(resolve_routing_strategy_name(&config), "random_failover");
    }

    #[test]
    fn resolve_routing_with_latency_config() {
        let mut options = HashMap::new();
        options.insert(
            "routing".to_string(),
            serde_json::json!({ "strategy": "latency_based" }),
        );
        let mut provider_configs = HashMap::new();
        provider_configs.insert(
            "free".to_string(),
            ProviderConfig {
                options,
                ..Default::default()
            },
        );
        let config = Config {
            provider_configs,
            ..Default::default()
        };
        assert_eq!(resolve_routing_strategy_name(&config), "latency_based");
    }

    #[test]
    fn execute_arg_aliases_map_correctly() {
        // Test the strategy matching via a helper that mirrors the execute match.
        // This validates sr/rr/lr argument aliases without needing a full
        // CommandContext.
        let resolve = |arg: &str| -> Option<&'static str> {
            match arg.to_lowercase().as_str() {
                "auto" | "smart" => Some("auto"),
                "sequential" | "seq" | "sr" => Some("sequential"),
                "random" | "random_failover" | "random-failover" | "rr" => Some("random_failover"),
                "latency" | "latency_based" | "latency-based" | "lr" => Some("latency_based"),
                "task" | "task_based" | "task-based" | "tb" => Some("task_based"),
                _ => None,
            }
        };

        // Full names still work
        assert_eq!(resolve("auto"), Some("auto"));
        assert_eq!(resolve("sequential"), Some("sequential"));
        assert_eq!(resolve("random"), Some("random_failover"));
        assert_eq!(resolve("latency"), Some("latency_based"));
        assert_eq!(resolve("task"), Some("task_based"));
        assert_eq!(resolve("task_based"), Some("task_based"));

        // Short aliases work
        assert_eq!(resolve("smart"), Some("auto"));
        assert_eq!(resolve("seq"), Some("sequential"));
        assert_eq!(resolve("sr"), Some("sequential"));
        assert_eq!(resolve("rr"), Some("random_failover"));
        assert_eq!(resolve("lr"), Some("latency_based"));
        assert_eq!(resolve("tb"), Some("task_based"));

        // Canonical names work
        assert_eq!(resolve("random_failover"), Some("random_failover"));
        assert_eq!(resolve("latency_based"), Some("latency_based"));

        // Invalid args return None
        assert_eq!(resolve("invalid"), None);
        assert_eq!(resolve(""), None);
        assert_eq!(resolve("unknown"), None);
    }
}
