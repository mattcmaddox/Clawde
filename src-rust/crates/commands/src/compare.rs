// compare.rs — /compare smart-router performance report.

use super::{ArgCompletion, CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;

pub use clawde_api::{build_compare_report, format_compare_report, parse_compare_args};
pub use clawde_api::{CompareFilters, CompareReport, CompareRow};

pub struct CompareCommand;

#[async_trait]
impl SlashCommand for CompareCommand {
    fn name(&self) -> &str {
        "compare"
    }

    fn aliases(&self) -> Vec<&str> {
        vec!["cmp"]
    }

    fn description(&self) -> &str {
        "Compare free-model upstream performance and health"
    }

    fn help(&self) -> &str {
        "Usage: /compare [task] [--provider <upstream>]\n\n\
         Compare configured free upstreams by task-aware success rate, latency,\n\
         dispatch history, cooldown state, and key health.\n\n\
         Examples:\n\
           /compare\n\
           /compare coding\n\
           /compare --task reasoning\n\
           /compare --provider groq"
    }

    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        [
            ("coding", "Code generation and editing performance"),
            ("reasoning", "Reasoning-task performance"),
            ("planning", "Planning-task performance"),
            ("verification", "Verification-task performance"),
            ("search", "Search-task performance"),
            ("--task", "Filter by a task type"),
            ("--provider", "Filter by an upstream"),
        ]
        .into_iter()
        .map(|(value, description)| ArgCompletion {
            value: value.to_string(),
            description: description.to_string(),
            available: true,
        })
        .collect()
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let filters = match parse_compare_args(args) {
            Ok(filters) => filters,
            Err(error) => return CommandResult::Error(error),
        };
        let Some(registry) = ctx.provider_registry.as_deref() else {
            return CommandResult::Error(
                "No live provider registry is available for /compare. Start free mode first."
                    .to_string(),
            );
        };
        CommandResult::Message(format_compare_report(&build_compare_report(
            registry, filters,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parser_and_renderer_are_shared() {
        let filters = parse_compare_args("coding --provider groq").unwrap();
        assert_eq!(filters.task.as_deref(), Some("coding"));
        assert_eq!(filters.provider.as_deref(), Some("groq"));
        let output = format_compare_report(&CompareReport {
            filters,
            rows: vec![CompareRow {
                provider: "free".into(),
                upstream: "groq".into(),
                dispatches: 3,
                success_rate: Some(0.75),
                task_success_rate: None,
                latency_secs: Some(0.125),
                cooldown: None,
                key_health: None,
            }],
        });
        assert!(output.contains("75.0%"));
        assert!(output.contains("125ms"));
        assert!(!output.contains('$'));
    }
}
