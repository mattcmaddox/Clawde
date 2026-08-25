use super::*;
use async_trait::async_trait;

pub struct HealthCommand;

// ---- /health -------------------------------------------------------------

#[async_trait]
impl SlashCommand for HealthCommand {
    fn name(&self) -> &str {
        "health"
    }
    fn description(&self) -> &str {
        "Probe all stored free-mode API keys and report per-key health"
    }
    fn help(&self) -> &str {
        "Usage: /health [<upstream>]\n\
         Runs a live probe (GET /v1/models, 5s timeout; a 1-token\n\
         chat/completions confirmation for upstreams whose models endpoint\n\
         doesn't check auth — nvidia, openrouter, sambanova, cloudflare,\n\
         poolside) against every stored key for each configured free-mode\n\
         upstream and reports per-key results. Keys that fail authentication\n\
         are marked exhausted in the running key rings (visible in the\n\
         footer and /ctx-viz).\n\n\
         /health <upstream>  — probe only that upstream (e.g. /health nvidia)\n\n\
         The same probe runs automatically at startup and every\n\
         health_poll_interval_secs (default 300s) in the background."
    }

    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        // Suggest upstreams that have stored keys (single credential or
        // multi-key store) so /health <upstream> is easy to tab-complete.
        let store = clawde_core::AuthStore::load();
        let mut upstreams: Vec<String> = store.keys.keys().cloned().collect();
        for pid in store.credentials.keys() {
            if !upstreams.contains(pid) {
                upstreams.push(pid.clone());
            }
        }
        // Only free-catalog upstreams are actually probed; intersect with
        // the catalog so unrelated providers (e.g. anthropic) aren't offered.
        upstreams.retain(|id| clawde_api::providers::free::catalog_entry(id).is_some());
        upstreams.sort();
        upstreams
            .into_iter()
            .filter(|u| u.starts_with(partial))
            .map(|u| ArgCompletion {
                value: u.clone(),
                description: format!("Probe only {} keys", u),
                available: true,
            })
            .collect()
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        // Reject unknown upstreams up front — an empty filter on a bogus id
        // would otherwise probe nothing and report a misleading "no keys
        // configured".
        let filter = args.trim().to_string();
        if let Some(err) = health_filter_error(&filter) {
            return CommandResult::Error(err);
        }
        // probe_sync()/probe_sync_for() internally spawn a plain OS thread so
        // the blocking HTTP clients are created/dropped outside this async
        // context; the spawn_blocking wrapper keeps the executor free while
        // it joins.
        let run = move || {
            if filter.is_empty() {
                clawde_api::health_poller::probe_sync()
            } else {
                clawde_api::health_poller::probe_sync_for(&filter)
            }
        };
        match tokio::task::spawn_blocking(run).await {
            Ok(outcome) => CommandResult::Message(format_health_report(&outcome)),
            Err(e) => CommandResult::Error(format!("Health probe task failed: {}", e)),
        }
    }
}

/// Validate the `/health <upstream>` filter.
///
/// Returns `None` for an empty filter (full sweep) or a real free-catalog
/// upstream; otherwise an error message that lists the valid upstreams so
/// the user can correct the argument.
fn health_filter_error(filter: &str) -> Option<String> {
    if filter.is_empty() || clawde_api::providers::free::catalog_entry(filter).is_some() {
        return None;
    }
    let valid: Vec<&str> = clawde_api::providers::free::FREE_CATALOG
        .iter()
        .map(|e| e.id)
        .collect();
    Some(format!(
        "Unknown upstream '{filter}'. Valid upstreams: {}",
        valid.join(", ")
    ))
}

fn format_health_report(outcome: &clawde_api::health_poller::ProbeOutcome) -> String {
    if outcome.checked == 0 {
        return "No free-mode API keys configured.\n\
                Add keys via /connect (Free mode) to enable health probing."
            .to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "Health check — {} key(s) probed, {} unhealthy",
        outcome.checked, outcome.unhealthy
    ));
    for r in &outcome.results {
        // ✓ healthy, ~ transient (key not proven invalid, upstream busy),
        // ✗ dead (definitive auth rejection).
        let mark = if !r.ok {
            "\u{2717}" // ✗
        } else if r.transient {
            "~"
        } else {
            "\u{2713}" // ✓
        };
        let detail = match &r.err {
            Some(e) => format!(" — {}", e),
            None => String::new(),
        };
        lines.push(format!(
            "  {} {:<16} key #{}  {}",
            mark,
            r.upstream,
            r.key_idx + 1,
            detail
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_health_report_renders_per_key_lines() {
        let outcome = clawde_api::health_poller::ProbeOutcome {
            checked: 2,
            unhealthy: 1,
            results: vec![
                clawde_api::health_poller::HealthProbeResult {
                    upstream: "groq".to_string(),
                    key_idx: 0,
                    ok: true,
                    transient: false,
                    err: None,
                },
                clawde_api::health_poller::HealthProbeResult {
                    upstream: "nvidia".to_string(),
                    key_idx: 1,
                    ok: false,
                    transient: false,
                    err: Some("Invalid API key (HTTP 401)".to_string()),
                },
            ],
        };
        let text = format_health_report(&outcome);
        assert!(text.contains("2 key(s) probed, 1 unhealthy"));
        assert!(text.contains("\u{2713} groq"));
        assert!(text.contains("\u{2717} nvidia"));
        assert!(text.contains("key #2"));
        assert!(text.contains("Invalid API key (HTTP 401)"));
    }

    /// A transient failure (5xx / connection / rate limit) is not a dead key:
    /// it renders with a `~` marker, not `✗`, and is not counted unhealthy.
    #[test]
    fn format_health_report_marks_transient_distinctly() {
        let outcome = clawde_api::health_poller::ProbeOutcome {
            checked: 1,
            unhealthy: 0,
            results: vec![clawde_api::health_poller::HealthProbeResult {
                upstream: "nvidia".to_string(),
                key_idx: 0,
                ok: true,
                transient: true,
                err: Some("Connection failed: timed out".to_string()),
            }],
        };
        let text = format_health_report(&outcome);
        assert!(text.contains("1 key(s) probed, 0 unhealthy"));
        assert!(text.contains("~ nvidia"));
        assert!(
            !text.contains("\u{2717} nvidia"),
            "transient is not a dead key"
        );
        assert!(text.contains("Connection failed: timed out"));
    }

    #[test]
    fn format_health_report_empty() {
        let outcome = clawde_api::health_poller::ProbeOutcome::default();
        assert!(format_health_report(&outcome).contains("No free-mode API keys"));
    }

    #[test]
    fn arg_completions_offer_only_stored_catalog_upstreams() {
        // Points CLAWDE_HOME at a temp dir so the auth store is isolated.
        let _home = crate::keys::tests::TestHome::new();

        let mut store = clawde_core::AuthStore::load();
        // nvidia + google are catalog upstreams; anthropic is a non-free
        // provider that /health never probes.
        store.set_keys("nvidia", vec!["nvk_test_key_1".into()]);
        store.set_keys("google", vec!["aistudio_test_key_1".into()]);
        store.set_keys("anthropic", vec!["sk-ant-test".into()]);
        store.save();

        let cmd = HealthCommand;
        // Empty partial → every stored catalog upstream.
        let completions = cmd.arg_completions("");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(values.contains(&"nvidia"), "got: {:?}", values);
        assert!(values.contains(&"google"), "got: {:?}", values);
        // Non-catalog providers (anthropic) are never offered — /health only
        // probes free-mode upstreams.
        assert!(!values.contains(&"anthropic"), "got: {:?}", values);

        // Prefix filtering works.
        let completions = cmd.arg_completions("n");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["nvidia"], "got: {:?}", values);

        // Only upstreams with stored keys are offered.
        let completions = cmd.arg_completions("");
        assert!(
            completions.iter().all(|c| c.value != "cerebras"),
            "cerebras has no stored key and must not be offered"
        );
    }

    #[test]
    fn unknown_upstream_filter_reports_valid_upstreams() {
        // Empty filter = full sweep; real catalog upstream = targeted probe.
        assert!(health_filter_error("").is_none());
        assert!(health_filter_error("nvidia").is_none());
        // Non-free providers and garbage are rejected with a helpful list.
        let err = health_filter_error("anthropic").expect("should reject non-free provider");
        assert!(err.contains("Unknown upstream 'anthropic'"), "got: {}", err);
        assert!(
            err.contains("nvidia"),
            "valid list should be included: {}",
            err
        );
        // The message names the invalid upstream exactly once (in the
        // "Unknown upstream" prefix) — never in the valid-upstream list.
        assert_eq!(err.matches("anthropic").count(), 1, "got: {}", err);
    }
}
