//! Config-independent native diagnostics command, plus the opt-in live smoke.
//!
//! The default (`clawde diagnostics`) intentionally runs before settings,
//! credentials, providers, MCP, or the normal query loop are initialized; it
//! exercises only Clawde's deterministic semantic-pipeline harness.
//!
//! `clawde diagnostics --live` additionally runs the live FreeProvider
//! semantic smoke: a real free model reviews a synthetic fixture through the
//! production provider stack, using the user's stored free-model keys. This is
//! the acceptance evidence the native harness intentionally cannot produce.

pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let json = args.iter().any(|arg| arg == "--json");
    let live = args.iter().any(|arg| arg == "--live");
    let unsupported = args
        .iter()
        .filter(|arg| arg.as_str() != "--json" && arg.as_str() != "--live")
        .cloned()
        .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        anyhow::bail!(
            "Unknown diagnostics argument '{}'. Use `clawde diagnostics [--json] [--live]`.",
            unsupported.join(" ")
        );
    }

    let native = clawde_query::run_native_diagnostics().await;
    let live_report = if live {
        Some(clawde_query::run_live_semantic_smoke().await)
    } else {
        None
    };

    if json {
        let payload = serde_json::json!({
            "native": native,
            "live": live_report,
        });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!(
            "Clawde native diagnostics: {}",
            if native.ok { "PASS" } else { "FAIL" }
        );
        println!("  live provider calls: {}", native.live_provider_calls);
        println!("  project mutated: {}", native.project_mutated);
        if let Some(verdict) = &native.semantic_verdict {
            println!("  semantic verdict: {verdict}");
        }
        for check in &native.checks {
            println!(
                "  {} {} — {}",
                if check.ok { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        if let Some(live) = &live_report {
            println!(
                "Clawde live FreeProvider smoke: {}",
                if live.ok { "PASS" } else { "FAIL" }
            );
            println!(
                "  deterministic verdict: {}",
                live.deterministic_verdict.as_deref().unwrap_or("n/a")
            );
            println!(
                "  semantic verdict: {}",
                live.verdict.as_deref().unwrap_or("n/a")
            );
            if let Some(model) = &live.model {
                println!("  model: {model}");
            }
            if let Some(strategy) = &live.routing_strategy {
                println!("  routing: {strategy}");
            }
            println!("  latency: {} ms", live.latency_ms);
            if let Some(summary) = &live.summary {
                println!("  summary: {summary}");
            }
            if let Some(prod) = &live.production {
                println!(
                    "  production AgentTool runner: {}",
                    if prod.ok { "PASS" } else { "FAIL" }
                );
                println!("    attempts: {}", prod.attempts);
                if let Some(verdict) = &prod.verdict {
                    println!("    verdict: {verdict}");
                }
                if let Some(summary) = &prod.summary {
                    println!("    summary: {summary}");
                }
                for finding in &prod.findings {
                    println!("    finding: {finding}");
                }
                if let Some(error) = &prod.error {
                    println!("    error: {error}");
                }
            }
            if let Some(direct_error) = &live.direct_error {
                println!("  direct-path note: {direct_error}");
            }
            if let Some(error) = &live.error {
                println!("  error: {error}");
            }
        }
    }

    let live_ok = live_report.as_ref().map(|report| report.ok).unwrap_or(true);
    if native.ok && live_ok {
        Ok(())
    } else {
        anyhow::bail!("Diagnostics failed")
    }
}
