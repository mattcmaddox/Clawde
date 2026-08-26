// `/keys` command — manage multiple API keys per provider.
//
// Subcommands:
//   /keys                  — list all providers that have configured keys
//   /keys list [provider]  — show keys for all providers or a specific one
//   /keys set <provider> <key1> [key2 ...] — replace all keys (clears previous)
//   /keys add <provider> <key>             — append a single key
//   /keys remove <provider> <index>        — remove key at 0-based index
//
// Keys are stored in the AuthStore's multi-key store (`~/.clawde/auth.json`)
// and are picked up at runtime by `build_free_provider()` and
// `from_environment_with_auth_store()`.

use super::*;
use async_trait::async_trait;
use clawde_core::key_ring::KeyRing;
use clawde_core::AuthStore;
use clawde_tools::web_search;

/// Split a raw key list into `(valid, dropped)` where dropped are the
/// placeholder-looking values (shorter than 8 chars after trimming). Cloud
/// API keys are always at least 8 characters; shorter values are placeholders
/// or test artifacts that would fail with AuthFailed.
fn partition_placeholder_keys(keys: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut valid = Vec::new();
    let mut dropped = Vec::new();
    for key in keys {
        let trimmed = key.trim().to_string();
        if trimmed.len() >= 8 {
            valid.push(trimmed);
        } else {
            dropped.push(trimmed);
        }
    }
    (valid, dropped)
}

/// Validate a Cloudflare composite key (`ACCOUNT_ID:API_TOKEN`).
///
/// Cloudflare's OpenAI-compatible endpoint embeds the account ID in the URL
/// path, so a bare API token cannot be routed anywhere on its own. The stored
/// credential must carry both halves joined by a colon — the same shape the
/// free-mode dialog collects in two steps. Mirrors `cloudflare_parts` in the
/// api crate so every entry point agrees on the composite format.
fn validate_cloudflare_key(key: &str) -> Result<String, String> {
    let trimmed = key.trim().to_string();
    let Some((account, token)) = trimmed.split_once(':') else {
        return Err(
            "Cloudflare keys must be ACCOUNT_ID:API_TOKEN — a bare API token has\n\
             no account ID to route requests to.\n\
             Example: abc123def456:your-api-token"
                .to_string(),
        );
    };
    if account.is_empty() || token.is_empty() {
        return Err(
            "Cloudflare key has an empty half — expected ACCOUNT_ID:API_TOKEN.\n\
             Example: abc123def456:your-api-token"
                .to_string(),
        );
    }
    // Wrong-order guard: Cloudflare account IDs are 32-char lowercase hex;
    // API tokens are longer mixed-case alphanumeric/`_`/`-` strings. If the
    // halves look swapped, reject instead of storing a credential that can
    // never authenticate (the account ID would be sent as the Bearer token).
    if looks_like_account_id(token) && !looks_like_account_id(account) {
        return Err(
            "That looks like TOKEN:ACCOUNT_ID — the halves are swapped.\n\
             Cloudflare expects ACCOUNT_ID:API_TOKEN (account ID first).\n\
             Example: abc123def456:your-api-token"
                .to_string(),
        );
    }
    Ok(trimmed)
}

/// Cloudflare account IDs are 32-character lowercase hex strings
/// (e.g. `1a2b3c4d5e6f7890abcdef1234567890`). Used only to catch clearly
/// swapped composite pastes — permissive by design, never a hard requirement.
fn looks_like_account_id(s: &str) -> bool {
    s.len() == 32
        && s.chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Provider IDs worth suggesting even before any keys are stored.
///
/// Covers the well-known `ProviderId` constants plus credential-backed tools.
/// Firecrawl is a search backend rather than an LLM provider, so it has no
/// `ProviderId` constant — but `/keys set firecrawl <key>` is the documented
/// way to configure it and must autocomplete from a cold store.
fn known_provider_ids() -> Vec<&'static str> {
    use clawde_core::ProviderId as P;
    vec![
        P::ANTHROPIC,
        P::OPENAI,
        P::GOOGLE,
        P::GOOGLE_VERTEX,
        P::AMAZON_BEDROCK,
        P::AZURE,
        P::GITHUB_COPILOT,
        P::MISTRAL,
        P::XAI,
        P::GROQ,
        P::DEEPINFRA,
        P::CEREBRAS,
        P::CROF,
        P::TOGETHER_AI,
        P::PERPLEXITY,
        P::OPENROUTER,
        P::OLLAMA,
        P::LM_STUDIO,
        P::LLAMA_CPP,
        P::DEEPSEEK,
        P::GITLAB,
        P::CLOUDFLARE,
        P::VENICE,
        P::SAP,
        P::SAMBANOVA,
        P::NVIDIA,
        P::SILICONFLOW,
        P::MOONSHOT,
        P::ZHIPU,
        P::ZAI,
        P::NEBIUS,
        P::OVHCLOUD,
        P::SCALEWAY,
        P::VULTR,
        P::BASETEN,
        P::FRIENDLI,
        P::UPSTAGE,
        P::STEPFUN,
        P::FIREWORKS,
        P::NOVITA,
        P::MINIMAX,
        P::CODEX,
        P::OPENCODE_GO,
        P::OPENCODE_ZEN,
        P::NEURALWATT,
        P::CLINE,
        P::FREE,
        "firecrawl",
    ]
}

pub struct KeysCommand;

#[async_trait]
impl SlashCommand for KeysCommand {
    fn name(&self) -> &str {
        "keys"
    }

    fn description(&self) -> &str {
        "Manage multiple API keys for providers (set, add, remove, list, health)"
    }

    fn help(&self) -> &str {
        "Usage: /keys [subcommand [args...]]\n\
         \n\
         Manage API keys in the multi-key store. Providers with 2+ keys\n\
         automatically rotate between them when one is exhausted.\n\
         \n\
         Subcommands:\n\
           /keys                        — list all providers with stored keys\n\
           /keys list [<provider>]      — show keys (optionally for one provider)\n\
           /keys set <p> <k1> [k2 ...]  — replace all keys for a provider\n\
           /keys add <p> <key>          — append a key to a provider\n\
           /keys remove <p> <index>     — remove key at 1-based index (see /keys list)\n\
           /keys health [<provider>]    — show runtime key status + cooldowns\n\
           /keys doctor                 — diagnose the auth store itself\n\
         \n\
         Cloudflare keys are composite ACCOUNT_ID:API_TOKEN credentials (both\n\
         halves joined by a colon) and are shape-validated before saving.\n\
         \n\
         Examples:\n\
           /keys set groq gsk_key1 gsk_key2 gsk_key3\n\
           /keys add groq gsk_key4\n\
           /keys remove groq 1\n\
           /keys list groq\n\
           /keys health\n\
           /keys doctor"
    }

    fn arg_completions(&self, partial: &str) -> Vec<ArgCompletion> {
        let mut completions = vec![
            ArgCompletion {
                value: "list".into(),
                description: "Show configured keys".into(),
                available: true,
            },
            ArgCompletion {
                value: "set".into(),
                description: "Replace all keys for a provider".into(),
                available: true,
            },
            ArgCompletion {
                value: "add".into(),
                description: "Append a single key to a provider".into(),
                available: true,
            },
            ArgCompletion {
                value: "remove".into(),
                description: "Remove a key by 1-based index (see /keys list)".into(),
                available: true,
            },
            ArgCompletion {
                value: "health".into(),
                description: "Show runtime key status and cooldown timers".into(),
                available: true,
            },
            ArgCompletion {
                value: "doctor".into(),
                description: "Diagnose the auth store (load errors, salvaged keys, backups)".into(),
                available: true,
            },
        ];

        let Some((subcommand, rest)) = partial.split_once(' ') else {
            return completions;
        };
        let provider_commands = ["list", "set", "add", "remove", "health"];
        if !provider_commands.contains(&subcommand) {
            return completions;
        }

        // Provider IDs are safe to suggest from both credential maps, the
        // rotation-key map, and the known-provider list. The known list makes
        // providers like firecrawl autocomplete even before the first key is
        // stored. API key values themselves are never suggested.
        let store = AuthStore::load();
        let mut providers: Vec<String> = store.credentials.keys().cloned().collect();
        for provider in store.keys.keys() {
            if !providers.contains(provider) {
                providers.push(provider.clone());
            }
        }
        for provider in known_provider_ids() {
            if !providers.iter().any(|p| p == provider) {
                providers.push(provider.to_string());
            }
        }
        providers.sort();
        providers.dedup();

        let trimmed_rest = rest.trim_start();
        let (provider_prefix, typed_provider, remaining) = match trimmed_rest.split_once(' ') {
            Some((provider, remaining)) => (provider, Some(provider), Some(remaining)),
            None => (trimmed_rest, None, None),
        };
        if typed_provider.is_none() {
            for provider in providers {
                if provider.starts_with(provider_prefix) {
                    completions.push(ArgCompletion {
                        value: format!("{subcommand} {provider}"),
                        description: String::new(),
                        available: true,
                    });
                }
            }
            return completions;
        }

        let provider = typed_provider.unwrap_or_default();
        let remaining = remaining.unwrap_or_default();
        match subcommand {
            "set" | "add" => {
                // Keys are sensitive free-form values. Show the next required
                // argument as a dimmed hint only while it is still empty;
                // never make it selectable or expose an existing credential in
                // the popup. Once the user starts typing the key the hint
                // disappears (the typed text is already visible in the input).
                // The description points at the provider's key page so the
                // user knows what the value is and where to get one.
                let typed_keys = remaining.trim();
                let description = match clawde_api::providers::provider_metadata(provider) {
                    clawde_api::providers::MetaLookup::Meta(m) => format!(
                        "Type the {provider} API key (get one at {}); credentials are never suggested",
                        m.key_url
                    ),
                    clawde_api::providers::MetaLookup::MissingProvider => {
                        "Type the API key manually; credentials are never suggested".to_string()
                    }
                };
                if let Some(hint) = crate::free_form_arg_hint(
                    &format!("{subcommand} {provider}"),
                    "<api-key>",
                    &description,
                    !typed_keys.is_empty(),
                ) {
                    completions.push(hint);
                }
            }
            "remove" => {
                let count = store.keys_for(provider).map(|keys| keys.len()).unwrap_or(0);
                for index in 1..=count {
                    completions.push(ArgCompletion {
                        value: format!("remove {provider} {index}"),
                        description: String::new(),
                        available: true,
                    });
                }
            }
            "list" | "health" => {}
            _ => {}
        }

        completions
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        let mut parts = args.splitn(3, ' ');
        let subcommand = parts.next().unwrap_or_default();

        match subcommand {
            "" => cmd_list(None),
            "doctor" => cmd_doctor(),
            "health" => {
                let rest = parts.next().unwrap_or_default().trim();
                cmd_health(
                    if rest.is_empty() { None } else { Some(rest) },
                    ctx.provider_registry.as_deref(),
                )
            }
            "list" => {
                let rest = parts.next().unwrap_or_default().trim();
                cmd_list(if rest.is_empty() { None } else { Some(rest) })
            }
            "set" => {
                let provider = parts.next().unwrap_or_default().trim();
                if provider.is_empty() {
                    return CommandResult::Error(
                        "Usage: /keys set <provider> <key1> [key2 ...]\n\
                         Provide the provider ID and at least one API key."
                            .to_string(),
                    );
                }
                let remaining = parts.next().unwrap_or_default();
                let keys: Vec<String> = split_command_args(remaining)
                    .into_iter()
                    .filter(|k| !k.is_empty())
                    .collect();
                if keys.is_empty() {
                    return CommandResult::Error(
                        "Usage: /keys set <provider> <key1> [key2 ...]\n\
                         At least one API key is required."
                            .to_string(),
                    );
                }
                let mut store = AuthStore::load();
                // Cloud API keys are always at least 8 characters. Shorter
                // values are placeholders or test artifacts that would fail
                // with AuthFailed and poison the rotation pool — refuse them.
                let (valid, dropped) = partition_placeholder_keys(keys);
                if valid.is_empty() {
                    return CommandResult::Error(
                        "Refusing to store placeholder-looking key(s).\n\
                         Cloud API keys are at least 8 characters; `k1`, `k2`, `test`, etc.\n\
                         are treated as placeholders and not saved."
                            .to_string(),
                    );
                }
                // Cloudflare composite keys: every entry must be the
                // ACCOUNT_ID:API_TOKEN shape or it cannot be routed at all.
                if provider == "cloudflare" {
                    for k in &valid {
                        if let Err(err) = validate_cloudflare_key(k) {
                            return CommandResult::Error(format!(
                                "Invalid Cloudflare key '{}':\n{}",
                                k, err
                            ));
                        }
                    }
                }
                store.set_keys(provider, valid.clone());
                let dropped_note = if dropped.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\nSkipped {} placeholder-looking key{}: {}",
                        dropped.len(),
                        if dropped.len() == 1 { "" } else { "s" },
                        dropped.join(", ")
                    )
                };
                CommandResult::Message(format!(
                    "Keys for '{}' updated — {} key{} configured.{}",
                    provider,
                    valid.len(),
                    if valid.len() == 1 { "" } else { "s" },
                    dropped_note,
                ))
            }
            "add" => {
                let provider = parts.next().unwrap_or_default().trim();
                let key = parts.next().unwrap_or_default().trim();
                if provider.is_empty() || key.is_empty() {
                    return CommandResult::Error(
                        "Usage: /keys add <provider> <key>\n\
                         Provide the provider ID and the API key to add."
                            .to_string(),
                    );
                }
                let mut store = AuthStore::load();
                // Same placeholder guard as /keys set — reject keys too short
                // to be real cloud API keys.
                let (valid, _) = partition_placeholder_keys(vec![key.to_string()]);
                if valid.is_empty() {
                    return CommandResult::Error(
                        "Refusing to store placeholder-looking key.\n\
                         Cloud API keys are at least 8 characters; `k1`, `k2`, `test`, etc.\n\
                         are treated as placeholders and not saved."
                            .to_string(),
                    );
                }
                // Cloudflare stores a composite ACCOUNT_ID:API_TOKEN — a bare
                // token passes the length guard but cannot be routed, so check
                // the composite shape explicitly.
                if provider == "cloudflare" {
                    if let Err(err) = validate_cloudflare_key(&valid[0]) {
                        return CommandResult::Error(err);
                    }
                }
                store.add_key(provider, valid[0].clone());
                let total = store.keys_for(provider).map(|k| k.len()).unwrap_or(0);
                let mut msg = format!(
                    "Key added to '{}' — now has {} key{}.\n\
                     Key rotation is active when 2+ keys are configured.",
                    provider,
                    total,
                    if total == 1 { "" } else { "s" },
                );
                if provider == "cloudflare" {
                    msg.push_str(
                        "\nStored as ACCOUNT_ID:API_TOKEN — account ID first, token second,\n\
                         joined by a colon.",
                    );
                }
                CommandResult::Message(msg)
            }
            "remove" => {
                let provider = parts.next().unwrap_or_default().trim();
                let index_str = parts.next().unwrap_or_default().trim();
                if provider.is_empty() || index_str.is_empty() {
                    return CommandResult::Error(
                        "Usage: /keys remove <provider> <index>\n\
                         Provide the provider ID and the 1-based index of the key to remove (see /keys list)."
                            .to_string(),
                    );
                }
                let one_based: usize = match index_str.parse() {
                    Ok(i) if i >= 1 => i,
                    Ok(_) => {
                        return CommandResult::Error(
                            "Invalid index '0' — indices start at 1. Use /keys list to see indices."
                                .to_string(),
                        );
                    }
                    Err(_) => {
                        return CommandResult::Error(format!(
                            "Invalid index '{}' — must be a positive integer.",
                            index_str
                        ));
                    }
                };
                // Convert to 0-based for internal operations.
                let zero_based = one_based - 1;
                let mut store = AuthStore::load();
                if store.remove_key(provider, zero_based) {
                    let remaining = store.keys_for(provider).map(|k| k.len()).unwrap_or(0);
                    CommandResult::Message(format!(
                        "Removed key {} from '{}' — {} key{} remaining.",
                        one_based,
                        provider,
                        remaining,
                        if remaining == 1 { "" } else { "s" },
                    ))
                } else {
                    let total = store.keys_for(provider).map(|k| k.len()).unwrap_or(0);
                    CommandResult::Error(format!(
                        "No key removed from '{}' at index {}.\n\
                         Provider '{}' has {} key{}. Valid indices are 1–{}.",
                        provider,
                        one_based,
                        provider,
                        total,
                        if total == 1 { "" } else { "s" },
                        total,
                    ))
                }
            }
            other => CommandResult::Error(format!(
                "Unknown subcommand '{}'.\n\
                 Usage: /keys [list|set|add|remove] ...\n\
                 Run /help keys for full details.",
                other
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Show key store status.
/// Format a duration in seconds to a human-readable string.
/// Examples: 42s, 5m 23s, 2h 15m, 1d 3h, 11h 59m 55s
fn format_duration(total_secs: u64) -> String {
    let days = total_secs / 86400;
    let hours = (total_secs % 86400) / 3600;
    let minutes = (total_secs % 3600) / 60;
    let secs = total_secs % 60;

    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{}d", days));
    }
    if hours > 0 {
        parts.push(format!("{}h", hours));
    }
    if minutes > 0 {
        parts.push(format!("{}m", minutes));
    }
    if secs > 0 || parts.is_empty() {
        parts.push(format!("{}s", secs));
    }
    parts.join(" ")
}

/// Render the free-mode upstream model-performance section (spec §8.6) for
/// `/keys health`: per-upstream dispatch success rate, average latency, and
/// per-task success rates (the same data as the /routing edit dashboard, in
/// CLI form). Respects the same provider/upstream filter as the rest of the
/// command: `free` or no filter shows every upstream; a specific upstream id
/// shows only that row.
fn render_free_upstream_performance(
    lines: &mut Vec<String>,
    reg: &clawde_api::ProviderRegistry,
    provider_filter: Option<&str>,
) {
    let success = reg.upstream_success_rate_summaries();
    let latencies = reg.upstream_latency_summaries();
    let task_rates = reg.upstream_task_success_rate_summaries();
    let failures = reg.upstream_last_failure_summaries(); // Join the snapshots on (provider, upstream) label. A provider is
                                                          // included when ANY of the sources has an entry for it; rows are built
                                                          // from the union of upstream ids seen. The sources have different inner
                                                          // types, so each is collected separately.
    let mut upstreams: Vec<(String, String)> = Vec::new();
    for (provider, entries) in &success {
        for (upstream, _) in entries {
            let label = (provider.clone(), upstream.clone());
            if !upstreams.contains(&label) {
                upstreams.push(label);
            }
        }
    }
    for (provider, entries) in &latencies {
        for (upstream, _) in entries {
            let label = (provider.clone(), upstream.clone());
            if !upstreams.contains(&label) {
                upstreams.push(label);
            }
        }
    }
    for (provider, entries) in &task_rates {
        for (upstream, _) in entries {
            let label = (provider.clone(), upstream.clone());
            if !upstreams.contains(&label) {
                upstreams.push(label);
            }
        }
    }
    for (provider, entries) in &failures {
        for (upstream, _) in entries {
            let label = (provider.clone(), upstream.clone());
            if !upstreams.contains(&label) {
                upstreams.push(label);
            }
        }
    }
    if upstreams.is_empty() {
        return;
    }

    // The free provider is the only composite that reports per-upstream
    // performance today; tolerate others generically via the label.
    let show_section = provider_filter
        .map(|f| f == "free" || upstreams.iter().any(|(_, u)| u == f))
        .unwrap_or(true);
    let upstream_filter = provider_filter.filter(|f| *f != "free");
    if !show_section {
        return;
    }

    let rate_for = |provider: &str, upstream: &str| -> Option<f64> {
        success
            .iter()
            .find(|(p, _)| p == provider)
            .and_then(|(_, e)| e.iter().find(|(u, _)| u == upstream))
            .and_then(|(_, r)| *r)
    };
    let latency_for = |provider: &str, upstream: &str| -> Option<f64> {
        latencies
            .iter()
            .find(|(p, _)| p == provider)
            .and_then(|(_, e)| e.iter().find(|(u, _)| u == upstream))
            .and_then(|(_, l)| *l)
    };
    let tasks_for = |provider: &str, upstream: &str| -> Vec<(String, f64)> {
        task_rates
            .iter()
            .find(|(p, _)| p == provider)
            .and_then(|(_, e)| e.iter().find(|(u, _)| u == upstream))
            .map(|(_, t)| {
                t.iter()
                    .filter_map(|(k, r)| r.map(|rate| (k.clone(), rate)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let failure_for = |provider: &str, upstream: &str| -> Option<String> {
        failures
            .iter()
            .find(|(p, _)| p == provider)
            .and_then(|(_, e)| e.iter().find(|(u, _)| u == upstream))
            .map(|(_, reason)| reason.clone())
    };

    lines.push("\nFree Upstream Performance".to_string());
    lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
    let mut rendered = 0usize;
    for (provider, upstream) in &upstreams {
        if upstream_filter.is_some_and(|f| f != upstream.as_str()) {
            continue;
        }
        let rate = rate_for(provider, upstream);
        let latency = latency_for(provider, upstream);
        let tasks = tasks_for(provider, upstream);
        let failure = failure_for(provider, upstream);
        if rate.is_none() && latency.is_none() && tasks.is_empty() && failure.is_none() {
            continue;
        }
        let rate_str = rate
            .map(|r| format!("{:.0}%", r * 100.0))
            .unwrap_or_else(|| "—".to_string());
        let latency_str = latency
            .map(|s| format!("{:.1}s", s))
            .unwrap_or_else(|| "—".to_string());
        let mut line = format!(
            "  {} · {}  {} success · {} avg",
            provider, upstream, rate_str, latency_str
        );
        if let Some(reason) = &failure {
            line.push_str(&format!("   last fail: {}", reason));
        }
        if !tasks.is_empty() {
            let tasks_str: Vec<String> = tasks
                .iter()
                .map(|(k, r)| format!("{} {:.0}%", k, r * 100.0))
                .collect();
            line.push_str(&format!("   ({})", tasks_str.join(", ")));
        }
        lines.push(line);
        rendered += 1;
    }
    if rendered == 0 {
        // No upstream has performance data yet — keep the section out rather
        // than showing an empty box.
        lines.pop();
        lines.pop();
    }
}

/// Show runtime key health from persisted cooldown state files.
///
/// Reads the `key-ring-state` files written by `KeyRotatingProvider`
/// on each key exhaustion, showing which keys are active vs exhausted
/// and how much cooldown time remains. When a live `provider_registry`
/// is available, also shows in-memory free-mode upstream
/// empty-completion cooldowns (spec §6.3) which are not persisted.
fn cmd_health(
    provider_filter: Option<&str>,
    registry: Option<&clawde_api::ProviderRegistry>,
) -> CommandResult {
    let store = AuthStore::load();
    // A corrupt/unreadable store means the user's keys may still be on disk
    // but invisible — surface that instead of a misleading "no keys".
    let load_warning = store.load_error.as_ref().map(|err| {
        format!(
            "Warning: your auth store at {} failed to load — no keys could be read from it. \
             Fix or remove the file and retry; the original is backed up before any overwrite.\n{err}",
            AuthStore::path().display()
        )
    });

    let mut provider_ids: Vec<String> = store.keys.keys().cloned().collect();
    provider_ids.sort();

    // Also check credential-only providers (they might have state files
    // from a previous multi-key setup).
    for pid in store.credentials.keys() {
        if !provider_ids.contains(pid) {
            let state_path = KeyRing::default_state_path(pid);
            if state_path.exists() {
                provider_ids.push(pid.clone());
            }
        }
    }
    provider_ids.sort();
    provider_ids.dedup();

    if let Some(filter) = provider_filter {
        provider_ids.retain(|p| p == filter);
    }

    if provider_ids.is_empty() {
        let msg = if provider_filter.is_some() {
            format!(
                "No keys found for '{}'.\n\
                 Use /keys set {} <key> to configure.",
                provider_filter.unwrap(),
                provider_filter.unwrap(),
            )
        } else {
            "No API keys configured yet.\n\
             Use /connect to set up a provider, or /keys set <provider> <key>."
                .to_string()
        };
        return CommandResult::Message(match load_warning {
            Some(w) => format!("{w}\n\n{msg}"),
            None => msg,
        });
    }

    let mut lines = Vec::new();
    lines.push("Multi-Key Health".to_string());
    lines.push("━━━━━━━━━━━━━━━━━".to_string());
    if let Some(ref w) = load_warning {
        lines.push(format!("\n{w}"));
    }

    for pid in &provider_ids {
        let keys = store.keys_for(pid);
        let key_count = keys.map(|k| k.len()).unwrap_or(0);

        // Load persisted cooldown state into a ring with the actual keys.
        let state_path = KeyRing::default_state_path(pid);
        let keys_vec: Vec<String> = store.keys_for(pid).map(|k| k.to_vec()).unwrap_or_default();
        let mut ring = KeyRing::new(pid.clone(), keys_vec);
        ring.load_from_file(&state_path);

        if key_count > 0 {
            lines.push(format!(
                "\n  {} — {} key{}",
                pid,
                key_count,
                if key_count == 1 { "" } else { "s" },
            ));
            if key_count > 1 {
                lines.push("  (rotation active)".to_string());
            }

            for (i, key) in store.keys_for(pid).unwrap().iter().enumerate() {
                let preview = if key.len() > 12 {
                    format!("{}..{}", &key[..6], &key[key.len() - 4..])
                } else {
                    key.clone()
                };

                // Check ring status for this key
                let statuses = ring.statuses();
                let key_status = statuses.iter().find(|s| s.key == *key);

                let status_line = match key_status {
                    Some(s) if s.active => {
                        format!("    [{}] {}  ACTIVE", i + 1, preview)
                    }
                    Some(s) => {
                        let remaining = s.cooldown_remaining_secs.unwrap_or(0);
                        let error_info = s.last_error.as_deref().unwrap_or("unknown");
                        format!(
                            "    [{}] {}  EXHAUSTED ({} remaining — {})",
                            i + 1,
                            preview,
                            format_duration(remaining),
                            error_info,
                        )
                    }
                    None => {
                        // Key not found in state file → active (no cooldown)
                        format!("    [{}] {}  ACTIVE", i + 1, preview)
                    }
                };
                lines.push(status_line);
            }
        } else {
            // No keys in auth store, but state file exists (e.g. from
            // a credential-only provider that was exhausted). Show the
            // state file contents directly.
            lines.push(format!(
                "\n  {} — (no stored keys, checking disk state)",
                pid
            ));
            for entry in &ring.statuses() {
                let remaining = entry.cooldown_remaining_secs.unwrap_or(0);
                let preview = if entry.key.len() > 12 {
                    format!("{}..{}", &entry.key[..6], &entry.key[entry.key.len() - 4..])
                } else {
                    entry.key.clone()
                };
                if entry.active || remaining == 0 {
                    lines.push(format!("  {}  ACTIVE", preview));
                } else {
                    let error_info = entry.last_error.as_deref().unwrap_or("unknown");
                    lines.push(format!(
                        "  {}  EXHAUSTED ({} remaining — {})",
                        preview,
                        format_duration(remaining),
                        error_info,
                    ));
                }
            }
        }
    }

    // Check Firecrawl key health (separate persistence from KeyRing).
    let show_firecrawl = provider_filter.map(|f| f == "firecrawl").unwrap_or(true);
    if show_firecrawl {
        let fc_keys = web_search::collect_firecrawl_keys();
        if !fc_keys.is_empty() {
            let fc_health = web_search::firecrawl_key_health();
            // Build a lookup of exhausted keys.
            let exhausted: std::collections::HashMap<&str, u64> = fc_health
                .iter()
                .filter(|(_, active, _)| !active)
                .map(|(key_id, _, remaining)| (key_id.as_str(), *remaining))
                .collect();

            lines.push(format!(
                "\n  firecrawl — {} key{}",
                fc_keys.len(),
                if fc_keys.len() == 1 { "" } else { "s" },
            ));
            if fc_keys.len() > 1 {
                lines.push("  (rotation active)".to_string());
            }
            for (i, key) in fc_keys.iter().enumerate() {
                let preview = web_search::firecrawl_key_label(key);
                let key_id = web_search::firecrawl_key_fingerprint(key);
                if let Some(&remaining) = exhausted.get(key_id.as_str()) {
                    lines.push(format!(
                        "    [{}] {}  EXHAUSTED ({} remaining)",
                        i + 1,
                        preview,
                        format_duration(remaining),
                    ));
                } else {
                    lines.push(format!("    [{}] {}  ACTIVE", i + 1, preview));
                }
            }
        }
    }

    // Live free-mode upstream empty-completion cooldowns (spec §6.3).
    // These live in-memory on the FreeProvider and are only visible when a
    // provider registry is available to the command. Only upstreams that
    // have recorded at least one empty completion are listed.
    if let Some(reg) = registry {
        let cooldowns = reg.empty_cooldown_summaries();
        // Show the section for the unfiltered view, the `free` provider id,
        // or any specific upstream id that appears inside the summaries
        // (e.g. `/keys health groq` still surfaces groq's cooldown).
        let show_section = provider_filter
            .map(|f| {
                f == "free"
                    || cooldowns
                        .iter()
                        .any(|(_, e)| e.iter().any(|(u, _, _)| u == f))
            })
            .unwrap_or(true);
        // When filtering for a specific upstream id (e.g. `/keys health groq`),
        // only render that upstream's cooldown entries — the section content
        // must mirror the rest of the command's filter behaviour. The `free`
        // filter (or no filter) shows the full aggregate view.
        let upstream_filter = provider_filter.filter(|f| *f != "free");
        if show_section && cooldowns.iter().any(|(_, entries)| !entries.is_empty()) {
            lines.push("\nFree Upstream Empty-Cooldowns".to_string());
            lines.push("━━━━━━━━━━━━━━━━━━━━━━━━━━━".to_string());
            for (provider, entries) in &cooldowns {
                for (upstream, count, retry_secs) in entries {
                    if upstream_filter.is_some_and(|f| f != upstream) {
                        continue;
                    }
                    let label = match retry_secs {
                        Some(secs) => format!(
                            "  {} · {}  COOLED (retry in {})",
                            provider,
                            upstream,
                            format_duration(*secs),
                        ),
                        None => format!(
                            "  {} · {}  {} consecutive empties",
                            provider, upstream, count,
                        ),
                    };
                    lines.push(label);
                }
            }
        }

        // Live free-mode upstream model-performance (spec §8.6): dispatch
        // success rate, average latency, and per-task success rates, so the
        // CLI health view matches the /routing edit dashboard. Only visible
        // when a provider registry is available.
        render_free_upstream_performance(&mut lines, reg, provider_filter);
        if let Some(capacity) = format_capacity_status_section(reg, provider_filter) {
            lines.push(format!("\n{capacity}"));
        }
    }

    if provider_filter.is_none() {
        lines.push(
            "\nUse /keys health <provider> to see a single provider's key status.".to_string(),
        );
    }

    CommandResult::Message(lines.join("\n"))
}

/// Validate the auth store end-to-end: load status, salvaged/recovered state,
/// placeholder slots that resolvers would filter, and corrupt-file backups
/// waiting to be recovered. Complements `/keys health` (which shows per-key
/// rotation status) by diagnosing the store itself.
fn cmd_doctor() -> CommandResult {
    let (report, _) = auth_store_doctor_report();
    CommandResult::Message(format!(
        "{report}\n\nTip: /keys health shows per-key rotation status; /keys set <provider> <key> adds keys."
    ))
}

/// Headless-friendly doctor report, shared by `/keys doctor`, the
/// `--check-keys` CLI flag, and the once-per-run headless banner.
///
/// Returns the multi-line report plus whether the auth or settings store
/// failed to load — the exit-code signal for `--check-keys` (placeholder
/// slots are reported but do not fail the check, since they never resolve).
pub fn auth_store_doctor_report() -> (String, bool) {
    let store = AuthStore::load();
    let path = AuthStore::path();
    let mut lines = Vec::new();
    let mut has_problems = false;
    lines.push("Auth Store Doctor".to_string());
    lines.push("━━━━━━━━━━━━━━━━━".to_string());

    lines.push(format!("\nStore file: {}", path.display()));
    if !path.exists() {
        lines.push("  Not present — keys resolve from environment variables only.".to_string());
    } else if let Some(ref err) = store.load_error {
        has_problems = true;
        lines.push(format!("  LOAD FAILED:\n{err}"));
        lines.push(
            "  Recovered entries are already in use. The original file is backed up as\n  \
             auth.json.corrupt-<timestamp> before any overwrite; fix or remove the file to\n  \
             restore the dropped entries."
                .to_string(),
        );
    } else {
        lines.push("  Loaded OK.".to_string());
    }

    // Credentials + rotation key slots.
    let mut providers: Vec<&String> = store.credentials.keys().chain(store.keys.keys()).collect();
    providers.sort();
    providers.dedup();
    if providers.is_empty() {
        lines.push("\nNo stored credentials or rotation keys.".to_string());
    } else {
        lines.push(format!("\nStored providers ({}):", providers.len()));
        for pid in providers {
            let key_count = store.keys_for(pid).map(|k| k.len()).unwrap_or(0);
            let mut parts: Vec<String> = Vec::new();
            if let Some(cred) = store.get(pid) {
                match cred {
                    clawde_core::auth_store::StoredCredential::ApiKey { key } => {
                        if key.trim().is_empty() || key.trim().len() < 8 {
                            parts.push(
                                "credential (api) — BLANK/short, treated as absent".to_string(),
                            );
                        } else {
                            parts.push("credential (api)".to_string());
                        }
                    }
                    clawde_core::auth_store::StoredCredential::OAuthToken { .. } => {
                        parts.push("credential (oauth)".to_string());
                    }
                }
            }
            if key_count > 0 {
                parts.push(format!(
                    "{} rotation key{}",
                    key_count,
                    if key_count == 1 { "" } else { "s" }
                ));
            }
            // Slots that resolvers will filter out (whitespace, <8 chars).
            let (_valid, bad) = partition_placeholder_keys(
                store.keys_for(pid).map(|k| k.to_vec()).unwrap_or_default(),
            );
            if !bad.is_empty() {
                parts.push(format!(
                    "{} placeholder/blank slot{} (filtered at use): {}",
                    bad.len(),
                    if bad.len() == 1 { "" } else { "s" },
                    bad.join(", ")
                ));
            }
            lines.push(format!("  {pid} — {}", parts.join(", ")));
        }
    }

    // Corrupt-file backups (auth + settings) waiting to be recovered.
    let backups = find_corrupt_backups("auth.json.corrupt-");
    let settings_backups = find_corrupt_backups("settings.json.corrupt-");
    let total = backups.len() + settings_backups.len();
    if total == 0 {
        lines.push("\nNo corrupt-file backups.".to_string());
    } else {
        lines.push(format!("\nCorrupt-file backups ({}):", total));
        for b in backups.into_iter().chain(settings_backups) {
            let meta = std::fs::metadata(&b).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            lines.push(format!("  {} ({} bytes)", b.display(), size));
        }
        lines.push(
            "  Restore a backup by fixing its JSON by hand and moving it back over the\n  \
             live file."
                .to_string(),
        );
    }

    // Settings store status — a corrupt settings.json is the same class of
    // trapdoor (settings silently defaulted), so `--check-keys` must flag it.
    let settings = clawde_core::config::Settings::load_sync().unwrap_or_default();
    if let Some(ref err) = settings.load_error {
        has_problems = true;
        lines.push(format!("\nSettings store (settings.json) FAILED:\n{err}"));
    } else {
        lines.push("\nSettings store (settings.json): OK".to_string());
    }

    (lines.join("\n"), has_problems)
}

/// List backup files in the auth-store directory matching a prefix (e.g.
/// `auth.json.corrupt-`), sorted by name (timestamp-suffixed).
fn find_corrupt_backups(prefix: &str) -> Vec<std::path::PathBuf> {
    let dir = AuthStore::path()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut out: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                out.push(entry.path());
            }
        }
    }
    out.sort();
    out
}

/// Show key store status.
fn cmd_list(provider_filter: Option<&str>) -> CommandResult {
    let store = AuthStore::load();

    // Collect all provider IDs that have keys.
    let mut entries: Vec<(String, Vec<String>)> = store
        .keys
        .iter()
        .map(|(p, k)| (p.clone(), k.clone()))
        .collect();

    // Collect all providers that have only credentials (no multi-keys)
    // so the list shows providers where the user could add more keys.
    let credential_only: Vec<&str> = store
        .credentials
        .keys()
        .filter(|p| !store.keys.contains_key(*p))
        .map(|s| s.as_str())
        .collect();

    // Apply optional provider filter.
    if let Some(filter) = provider_filter {
        entries.retain(|(p, _)| p == filter);
    }

    if entries.is_empty() && credential_only.is_empty() {
        let msg = if provider_filter.is_some() {
            format!(
                "No keys found for '{}'.\n\
                 Use /keys set {} <key> to add keys for key rotation.",
                provider_filter.unwrap(),
                provider_filter.unwrap(),
            )
        } else {
            "No API keys configured yet.\n\
             Use /connect to set up a provider, or /keys set <provider> <key1> [key2 ...]\n\
             to configure multiple keys for automatic rotation."
                .to_string()
        };
        return CommandResult::Message(msg);
    }

    let mut lines = Vec::new();

    if !entries.is_empty() {
        lines.push("Multi-key store:".to_string());
        lines.push("━━━━━━━━━━━━━━━━".to_string());
    }

    for (provider, keys) in &entries {
        lines.push(format!(
            "\n  {} — {} key{}",
            provider,
            keys.len(),
            if keys.len() == 1 { "" } else { "s" },
        ));
        let rotation_hint = if keys.len() > 1 {
            "  (rotation active)"
        } else {
            "  (1 key — add more for rotation)"
        };
        lines.push(rotation_hint.to_string());
        for (i, key) in keys.iter().enumerate() {
            let preview = if key.len() > 12 {
                format!("{}..{}", &key[..6], &key[key.len() - 4..])
            } else {
                key.clone()
            };
            lines.push(format!("    [{}] {}", i + 1, preview)); // 1-based display
        }
    }

    if provider_filter.is_none() && !credential_only.is_empty() {
        lines.push("\nProviders with single keys (no rotation):".to_string());
        for pid in credential_only {
            lines.push(format!(
                "  {} — use /keys add {} <key> to enable rotation",
                pid, pid
            ));
        }
    }

    lines.push(
        "\nProviders with 2+ keys automatically rotate between them when one is exhausted."
            .to_string(),
    );

    CommandResult::Message(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// /limits — query rate-limit information from provider APIs
// ---------------------------------------------------------------------------

pub struct LimitsCommand;

#[async_trait]
impl SlashCommand for LimitsCommand {
    fn name(&self) -> &str {
        "limits"
    }

    fn description(&self) -> &str {
        "Query rate-limit information from configured provider APIs"
    }

    fn help(&self) -> &str {
        "Usage: /limits [provider]\n\
         \n\
         Makes a lightweight GET request to each configured provider's\n\
         models endpoint and parses X-RateLimit-* response headers.\n\
         Upstreams whose models endpoint doesn't check auth (nvidia,\n\
         openrouter, sambanova, cloudflare, poolside) get a 1-token\n\
         chat/completions confirmation first.\n\
         \n\
         Most free-tier providers (including Gemini) don't expose rate-limit\n\
         headers — the command reports when they're unavailable.\n\
         \n\
         Examples:\n\
           /limits           — query all configured providers\n\
           /limits groq      — query only Groq\n\
           /limits google    — query only Google Gemini"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let provider_filter = {
            let s = args.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        };

        let store = AuthStore::load();
        let mut provider_ids: Vec<String> = store.keys.keys().cloned().collect();
        for pid in store.credentials.keys() {
            if !provider_ids.contains(pid) {
                provider_ids.push(pid.clone());
            }
        }
        provider_ids.sort();
        provider_ids.dedup();

        if let Some(ref filter) = provider_filter {
            provider_ids.retain(|p| p == filter);
        }

        if provider_ids.is_empty() {
            return CommandResult::Message(
                "No API keys configured. Use /connect to set up a provider first.".to_string(),
            );
        }

        let mut lines = Vec::new();
        lines.push("Rate-Limit Query".to_string());
        lines.push("━━━━━━━━━━━━━━━━".to_string());

        for pid in &provider_ids {
            let key = store
                .keys_for(pid)
                .and_then(|k| k.first().cloned())
                .or_else(|| store.api_key_for(pid));

            let Some(key) = key else {
                lines.push(format!("\n  {} — no key found", pid));
                continue;
            };

            lines.push(format!("\n  {}", pid));

            match clawde_api::providers::free::query_rate_limits(pid, &key) {
                Ok(info) => {
                    if !info.headers_found {
                        lines.push(format!("    No rate-limit headers exposed by {}", pid));
                        if pid == "google" {
                            lines.push(
                                "    → Check AI Studio: aistudio.google.com/app/apikey".to_string(),
                            );
                        } else {
                            lines.push(format!(
                                "    → Check the {} dashboard for current limits",
                                pid
                            ));
                        }
                    } else {
                        if let Some(v) = info.rpm_limit {
                            lines.push(format!(
                                "    RPM: {} / {} remaining",
                                info.rpm_remaining.unwrap_or(0),
                                v
                            ));
                        }
                        if let Some(v) = info.rpd_limit {
                            lines.push(format!(
                                "    RPD: {} / {} remaining",
                                info.rpd_remaining.unwrap_or(0),
                                v
                            ));
                        }
                        if let Some(v) = info.tpm_limit {
                            lines.push(format!(
                                "    TPM: {} / {} remaining",
                                info.tpm_remaining.unwrap_or(0),
                                v
                            ));
                        }
                        if let Some(s) = info.retry_after {
                            lines.push(format!("    Retry-After: {}s", s));
                        }
                    }
                }
                Err(e) => {
                    lines.push(format!("    Error: {}", e));
                }
            }
        }

        if provider_filter.is_none() && provider_ids.len() > 1 {
            lines.push("\nUse /limits <provider> to query a single provider.".to_string());
        }

        CommandResult::Message(lines.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clawde_api::LlmProvider;
    use clawde_api::ProviderError;
    use clawde_api::ProviderRegistry;
    use clawde_api::ProviderRequest;
    use clawde_api::ProviderResponse;
    use clawde_api::ProviderStatus;
    use clawde_api::StreamEvent;
    use clawde_core::ProviderId;
    use futures::Stream;
    use std::pin::Pin;

    /// Panic-safe guard: points `CLAWDE_HOME` at a temp dir so the auth store
    /// (and key-ring state files) resolve inside it, and restores the original
    /// env var on drop — even during unwinding from a panic.
    ///
    /// Holds the shared [`crate::tests::CLAWDE_HOME_LOCK`] for its lifetime so
    /// tests that mutate the process-global env var never race each other.
    pub(crate) struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev_clawde_home: Option<std::ffi::OsString>,
    }

    impl TestHome {
        pub(crate) fn new() -> Self {
            let lock = crate::tests::CLAWDE_HOME_LOCK
                .get_or_init(|| std::sync::Mutex::new(()))
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _lock: lock,
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

    /// Minimal `LlmProvider` that reports a fixed set of per-upstream
    /// empty-completion cooldowns (spec §6.3), success rates, latencies and
    /// per-task success rates (spec §8.6). The real `CooldownState` /
    /// `LatencyState` are private to the api crate, so a stub is the cleanest
    /// way to drive the registry aggregation + `cmd_health` rendering.
    struct CooldownStubProvider {
        id: ProviderId,
        cooldowns: Vec<(String, u32, Option<u64>)>,
        success_rates: Vec<(String, Option<f64>)>,
        latencies: Vec<(String, Option<f64>)>,
        task_rates: clawde_api::provider::UpstreamTaskSuccessRates,
        last_failures: Vec<(String, Option<String>)>,
        capacity: Vec<clawde_api::UpstreamCapacityStatus>,
    }

    #[async_trait]
    impl LlmProvider for CooldownStubProvider {
        fn id(&self) -> &ProviderId {
            &self.id
        }

        fn name(&self) -> &str {
            "cooldown-stub"
        }

        async fn create_message(
            &self,
            _request: ProviderRequest,
        ) -> Result<ProviderResponse, ProviderError> {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: "stub".into(),
                is_retryable: false,
            })
        }

        async fn create_message_stream(
            &self,
            _request: ProviderRequest,
        ) -> Result<
            Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            Err(ProviderError::ServerError {
                provider: self.id.clone(),
                status: None,
                message: "stub".into(),
                is_retryable: false,
            })
        }

        async fn health_check(&self) -> Result<ProviderStatus, ProviderError> {
            Ok(ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> clawde_api::ProviderCapabilities {
            clawde_api::ProviderCapabilities {
                streaming: true,
                tool_calling: false,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: clawde_api::SystemPromptStyle::SystemMessage,
            }
        }

        fn upstream_empty_cooldowns(&self) -> Vec<(String, u32, Option<u64>)> {
            self.cooldowns.clone()
        }

        fn upstream_success_rates(&self) -> Vec<(String, Option<f64>)> {
            self.success_rates.clone()
        }

        fn upstream_latencies(&self) -> Vec<(String, Option<f64>)> {
            self.latencies.clone()
        }

        fn upstream_task_success_rates(&self) -> clawde_api::provider::UpstreamTaskSuccessRates {
            self.task_rates.clone()
        }

        fn upstream_last_failures(&self) -> Vec<(String, Option<String>)> {
            self.last_failures.clone()
        }

        fn upstream_capacity(&self) -> Vec<clawde_api::UpstreamCapacityStatus> {
            self.capacity.clone()
        }
    }

    /// Seed the auth store (inside the `CLAWDE_HOME` temp dir) with keys so
    /// `cmd_health` doesn't take its "no keys configured" early-return, then
    /// build a registry whose free provider reports:
    ///   groq     — in empty-completion cooldown (60s remaining)
    ///   cerebras — 2 consecutive empties, not yet cooled
    fn test_registry() -> (ProviderRegistry, TestHome) {
        let home = TestHome::new();

        // Seed keys so provider_ids is non-empty for every filter case:
        // `cmd_health(Some("free"), ...)` retains `provider_ids` to only
        // `free`, so a key must exist under that id or the function
        // early-returns "No keys found for 'free'" before the
        // empty-cooldown section is rendered.
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk_test_key_1".into()]);
        store.set_keys("cerebras", vec!["csk_test_key_1".into()]);
        store.set_keys("free", vec!["fsk_test_key_1".into()]);
        store.save();

        let stub = CooldownStubProvider {
            id: ProviderId::new(ProviderId::FREE),
            cooldowns: vec![
                ("groq".to_string(), 0, Some(60)),
                ("cerebras".to_string(), 2, None),
            ],
            success_rates: vec![
                ("groq".to_string(), Some(1.0)),
                ("cerebras".to_string(), Some(0.5)),
            ],
            latencies: vec![
                ("groq".to_string(), Some(2.1)),
                ("cerebras".to_string(), Some(9.4)),
            ],
            task_rates: vec![(
                "groq".to_string(),
                vec![
                    ("code_generation".to_string(), Some(1.0)),
                    ("reasoning".to_string(), Some(0.0)),
                ],
            )],
            last_failures: vec![
                ("groq".to_string(), None),
                (
                    "cerebras".to_string(),
                    Some("cerebras: [cerebras] Server error 500".into()),
                ),
            ],
            capacity: vec![
                clawde_api::UpstreamCapacityStatus {
                    upstream_id: "groq".to_string(),
                    source: clawde_api::CapacityStatusSource::Headers,
                    utilization_pct: 72.0,
                    tokens_pct_used: Some(0.72),
                    requests_pct_used: None,
                    retry_after_secs: Some(90),
                    reset_at_unix: None,
                },
                clawde_api::UpstreamCapacityStatus {
                    upstream_id: "cerebras".to_string(),
                    source: clawde_api::CapacityStatusSource::LocalEstimate,
                    utilization_pct: 81.0,
                    tokens_pct_used: None,
                    requests_pct_used: Some(0.81),
                    retry_after_secs: None,
                    reset_at_unix: None,
                },
            ],
        };
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(stub));
        (registry, home)
    }

    fn message_text(result: CommandResult) -> String {
        match result {
            CommandResult::Message(text) => text,
            other => panic!("expected CommandResult::Message, got: {:?}", other),
        }
    }

    #[test]
    fn health_empty_cooldown_section_unfiltered() {
        let (registry, _home) = test_registry();
        let out = message_text(cmd_health(None, Some(&registry)));

        assert!(
            out.contains("Free Upstream Empty-Cooldowns"),
            "unfiltered health must show the section: {}",
            out
        );
        assert!(
            out.contains("  free · groq  COOLED (retry in 1m)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("  free · cerebras  2 consecutive empties"),
            "got: {}",
            out
        );
    }

    #[test]
    fn health_empty_cooldown_section_free_filter() {
        let (registry, _home) = test_registry();
        // `/keys health free` shows the aggregate view like the unfiltered one.
        let out = message_text(cmd_health(Some("free"), Some(&registry)));

        assert!(
            out.contains("Free Upstream Empty-Cooldowns"),
            "free filter must show the section: {}",
            out
        );
        assert!(
            out.contains("  free · groq  COOLED (retry in 1m)"),
            "got: {}",
            out
        );
        assert!(
            out.contains("  free · cerebras  2 consecutive empties"),
            "got: {}",
            out
        );
    }

    #[test]
    fn health_empty_cooldown_section_upstream_filter() {
        let (registry, _home) = test_registry();
        // `/keys health groq` surfaces the section but only groq's entry.
        let out = message_text(cmd_health(Some("groq"), Some(&registry)));

        assert!(
            out.contains("Free Upstream Empty-Cooldowns"),
            "upstream filter must show the section: {}",
            out
        );
        assert!(
            out.contains("  free · groq  COOLED (retry in 1m)"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("cerebras"),
            "upstream filter must exclude other upstreams: {}",
            out
        );
    }

    #[test]
    fn health_empty_cooldown_section_other_upstream_filter() {
        let (registry, _home) = test_registry();
        // `/keys health cerebras` shows only the not-yet-cooled upstream.
        let out = message_text(cmd_health(Some("cerebras"), Some(&registry)));

        assert!(
            out.contains("Free Upstream Empty-Cooldowns"),
            "upstream filter must show the section: {}",
            out
        );
        assert!(
            out.contains("  free · cerebras  2 consecutive empties"),
            "got: {}",
            out
        );
        assert!(
            !out.contains("groq"),
            "upstream filter must exclude other upstreams: {}",
            out
        );
    }

    #[test]
    fn health_performance_section_unfiltered() {
        let (registry, _home) = test_registry();
        let out = message_text(cmd_health(None, Some(&registry)));

        assert!(
            out.contains("Free Upstream Performance"),
            "unfiltered health must show the performance section: {}",
            out
        );
        assert!(
            out.contains(
                "  free · groq  100% success · 2.1s avg   (code_generation 100%, reasoning 0%)"
            ),
            "got: {}",
            out
        );
        assert!(
            out.contains("  free · cerebras  50% success · 9.4s avg"),
            "got: {}",
            out
        );
    }

    #[test]
    fn health_performance_section_shows_last_failure_reason() {
        let (registry, _home) = test_registry();
        let out = message_text(cmd_health(None, Some(&registry)));

        assert!(
            out.contains("Free Upstream Performance"),
            "unfiltered health must show the performance section: {}",
            out
        );
        assert!(
            out.contains("  free · cerebras  50% success · 9.4s avg   last fail: cerebras: [cerebras] Server error 500"),
            "last failure reason must be rendered for a degraded upstream: {}",
            out
        );
        // groq has no recorded failure — its row carries no stale reason.
        let groq_line = out
            .lines()
            .find(|l| l.contains("free · groq"))
            .unwrap_or("");
        assert!(
            !groq_line.contains("last fail"),
            "no stale failure reason for groq: {}",
            groq_line
        );
    }

    #[test]
    fn health_performance_section_upstream_filter() {
        let (registry, _home) = test_registry();
        // `/keys health groq` surfaces the section but only groq's row.
        let out = message_text(cmd_health(Some("groq"), Some(&registry)));

        assert!(
            out.contains("Free Upstream Performance"),
            "upstream filter must show the section: {}",
            out
        );
        assert!(
            out.contains(
                "  free · groq  100% success · 2.1s avg   (code_generation 100%, reasoning 0%)"
            ),
            "got: {}",
            out
        );
        assert!(
            !out.contains("cerebras"),
            "upstream filter must exclude other upstreams: {}",
            out
        );
    }

    #[test]
    fn health_capacity_section_shows_source_and_filter() {
        let (registry, _home) = test_registry();
        let out = message_text(cmd_health(Some("groq"), Some(&registry)));
        assert!(
            out.contains("Capacity"),
            "capacity section missing: {}",
            out
        );
        assert!(
            out.contains("free · groq   72% used · headers · reset in 1m 30s"),
            "header capacity status missing: {}",
            out
        );
        assert!(
            !out.contains("cerebras   81% used"),
            "filter leaked: {}",
            out
        );
    }

    #[test]
    fn health_capacity_section_is_omitted_without_capacity_data() {
        let _home = TestHome::new();
        let mut registry = ProviderRegistry::new();
        registry.register(Arc::new(CooldownStubProvider {
            id: ProviderId::new("free"),
            cooldowns: Vec::new(),
            success_rates: Vec::new(),
            latencies: Vec::new(),
            task_rates: Vec::new(),
            last_failures: Vec::new(),
            capacity: Vec::new(),
        }));
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk_test_key_1".into()]);
        store.save();
        let out = message_text(cmd_health(None, Some(&registry)));
        assert!(
            !out.contains("\nCapacity\n"),
            "missing data should stay quiet: {}",
            out
        );
    }

    #[test]
    fn health_performance_section_missing_without_registry() {
        let _home = TestHome::new();
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk_test_key_1".into()]);
        store.save();
        drop(store);

        let out = message_text(cmd_health(None, None));
        assert!(
            !out.contains("Free Upstream Performance"),
            "section must be absent without a registry: {}",
            out
        );
    }

    #[test]
    fn health_empty_cooldown_section_missing_without_registry() {
        let _home = TestHome::new();
        // Seed keys so cmd_health does NOT hit the "no keys configured"
        // early-return — this genuinely exercises the `registry: None` branch
        // and proves the in-memory section is suppressed without a registry.
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk_test_key_1".into()]);
        store.save();
        drop(store);

        let out = message_text(cmd_health(None, None));
        assert!(
            out.contains("Multi-Key Health"),
            "health should render key status: {}",
            out
        );
        assert!(
            !out.contains("Free Upstream Empty-Cooldowns"),
            "section must be absent without a registry: {}",
            out
        );
    }

    fn test_ctx() -> CommandContext {
        CommandContext {
            config: clawde_core::config::Config::default(),
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
            tool_use_tracker: None,
        }
    }

    #[test]
    fn set_rejects_placeholder_keys() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result = rt.block_on(cmd.execute("set groq k1 k2", &mut test_ctx()));
        assert!(
            matches!(result, CommandResult::Error(_)),
            "placeholder keys must be rejected, got: {:?}",
            result
        );
        assert!(
            AuthStore::load().keys_for("groq").is_none(),
            "rejected keys must not be persisted"
        );

        let result = rt.block_on(cmd.execute("add groq k1", &mut test_ctx()));
        assert!(matches!(result, CommandResult::Error(_)));

        // Real-looking keys still work.
        let result =
            rt.block_on(cmd.execute("set groq gsk_real_key_1 gsk_real_key_2", &mut test_ctx()));
        assert!(
            matches!(result, CommandResult::Message(_)),
            "real keys must be accepted, got: {:?}",
            result
        );
        assert_eq!(AuthStore::load().keys_for("groq").map(|k| k.len()), Some(2));
    }

    #[test]
    fn cloudflare_add_rejects_bare_token_with_format_hint() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        // A bare token passes the 8-char length guard but has no account ID —
        // the composite validation must reject it with a format hint.
        let result = rt.block_on(cmd.execute("add cloudflare tok_12345678", &mut test_ctx()));
        match result {
            CommandResult::Error(err) => {
                assert!(
                    err.contains("ACCOUNT_ID:API_TOKEN"),
                    "error must explain the composite format, got: {}",
                    err
                );
            }
            other => panic!("expected CommandResult::Error, got: {:?}", other),
        }
        assert!(
            AuthStore::load().keys_for("cloudflare").is_none(),
            "rejected key must not be persisted"
        );
    }

    #[test]
    fn cloudflare_add_accepts_composite_key() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        let result =
            rt.block_on(cmd.execute("add cloudflare abc123def456:tok_12345678", &mut test_ctx()));
        assert!(
            matches!(result, CommandResult::Message(_)),
            "composite key must be accepted, got: {:?}",
            result
        );
        let store = AuthStore::load();
        let keys = store.keys_for("cloudflare").unwrap();
        assert_eq!(keys, vec!["abc123def456:tok_12345678"]);
    }

    #[test]
    fn cloudflare_add_rejects_empty_half() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Account ID present, empty token half — the composite check must catch
        // it even though the whole string clears the 8-char guard.
        let result = rt.block_on(cmd.execute("add cloudflare abc123def456:", &mut test_ctx()));
        assert!(
            matches!(result, CommandResult::Error(_)),
            "empty token half must be rejected, got: {:?}",
            result
        );
    }

    #[test]
    fn cloudflare_set_rejects_invalid_key_atomically() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        // One bad composite in a multi-key set must fail the whole set.
        let result = rt.block_on(cmd.execute(
            "set cloudflare abc123def456:tok_12345678 bare_token_12345",
            &mut test_ctx(),
        ));
        assert!(
            matches!(result, CommandResult::Error(_)),
            "a bare token in a set must fail the whole set, got: {:?}",
            result
        );
        assert!(
            AuthStore::load().keys_for("cloudflare").is_none(),
            "failed set must not persist any keys"
        );

        // All-composite set still works.
        let result = rt.block_on(cmd.execute(
            "set cloudflare abc123def456:tok_12345678 fedcba098765:tok_98765432",
            &mut test_ctx(),
        ));
        assert!(
            matches!(result, CommandResult::Message(_)),
            "all-composite set must be accepted, got: {:?}",
            result
        );
        assert_eq!(
            AuthStore::load().keys_for("cloudflare").map(|k| k.len()),
            Some(2)
        );
    }

    #[test]
    fn cloudflare_add_rejects_swapped_halves_with_hint() {
        let _home = TestHome::new();
        let cmd = KeysCommand;
        let rt = tokio::runtime::Runtime::new().unwrap();

        // TOKEN:ACCOUNT_ID — the right side is a 32-char lowercase hex account
        // ID, so the validator must catch the swap and refuse to store it.
        let token = "AbC123dEf456GhIj789KLMnopQrstUvWxYz_ab";
        let account = "1a2b3c4d5e6f7890abcdef1234567890";
        let result = rt.block_on(cmd.execute(
            &format!("add cloudflare {}:{}", token, account),
            &mut test_ctx(),
        ));
        match result {
            CommandResult::Error(err) => {
                assert!(
                    err.contains("swapped"),
                    "error must mention the swap, got: {}",
                    err
                );
                assert!(
                    err.contains("ACCOUNT_ID:API_TOKEN"),
                    "error must show the correct format, got: {}",
                    err
                );
            }
            other => panic!("expected CommandResult::Error, got: {:?}", other),
        }
        assert!(
            AuthStore::load().keys_for("cloudflare").is_none(),
            "swapped key must not be persisted"
        );
    }

    #[test]
    fn cloudflare_accepts_32_hex_account_first() {
        // The real-world shape: 32-char lowercase hex account ID, then token.
        let account = "1a2b3c4d5e6f7890abcdef1234567890";
        assert!(validate_cloudflare_key(&format!("{}:tok_12345678", account)).is_ok());
        // A plain token without a colon still fails the composite check.
        assert!(validate_cloudflare_key("tok_12345678").is_err());
        // A short non-hex "account" half must not trip the swap detector.
        assert!(validate_cloudflare_key("abc123def456:tok_12345678").is_ok());
    }

    #[test]
    fn doctor_reports_corrupt_store_and_backups() {
        let _home = TestHome::new();
        // Partially corrupt store: valid groq credential + broken openai entry.
        std::fs::write(
            AuthStore::path(),
            r#"{"credentials":{"openai":{"type":"api"},"groq":{"type":"api","key":"gsk-still-recoverable"}}}"#,
        )
        .unwrap();

        let result = cmd_doctor();
        let CommandResult::Message(msg) = result else {
            panic!("expected Message, got: {result:?}")
        };
        assert!(msg.contains("Auth Store Doctor"), "msg: {msg}");
        assert!(msg.contains("LOAD FAILED"), "msg: {msg}");
        assert!(msg.contains("credentials[openai]"), "msg: {msg}");
        assert!(msg.contains("groq"), "msg: {msg}");

        // A deliberate save backs the corrupt file up; doctor then lists it.
        let mut store = AuthStore::load();
        store.set(
            "openai",
            clawde_core::auth_store::StoredCredential::ApiKey {
                key: "sk-new-12345678".into(),
            },
        );
        let result = cmd_doctor();
        let CommandResult::Message(msg) = result else {
            panic!("expected Message, got: {result:?}")
        };
        assert!(
            msg.contains("Corrupt-file backups (1)"),
            "doctor must list the backup, got: {msg}"
        );
    }

    #[test]
    fn doctor_report_flags_problems_for_corrupt_store() {
        let _home = TestHome::new();
        std::fs::write(AuthStore::path(), "{ not valid json ").unwrap();
        let (report, problems) = auth_store_doctor_report();
        assert!(problems, "corrupt store must flag problems");
        assert!(report.contains("LOAD FAILED"), "report: {report}");
        assert!(report.contains("Settings store"), "report: {report}");
    }

    #[test]
    fn doctor_report_clean_store_no_problems() {
        let _home = TestHome::new();
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk-real-key-12345678".into()]);
        let (report, problems) = auth_store_doctor_report();
        assert!(
            !problems,
            "clean store must not flag problems, got: {report}"
        );
        assert!(report.contains("Loaded OK"), "report: {report}");
        assert!(
            report.contains("Settings store (settings.json): OK"),
            "report: {report}"
        );
    }

    #[test]
    fn doctor_report_flags_problems_for_corrupt_settings_store() {
        let _home = TestHome::new();
        // Clean auth store + corrupt settings file: the settings branch of
        // the problems flag must fire.
        let mut store = AuthStore::load();
        store.set_keys("groq", vec!["gsk-real-key-12345678".into()]);
        let settings_path = clawde_core::config::Settings::global_settings_path();
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, "{ not valid json ").unwrap();

        let (report, problems) = auth_store_doctor_report();
        assert!(problems, "corrupt settings store must flag problems");
        assert!(
            report.contains("Settings store (settings.json) FAILED"),
            "report: {report}"
        );
    }

    #[test]
    fn doctor_report_placeholders_do_not_fail_check() {
        let _home = TestHome::new();
        let mut store = AuthStore::load();
        // Seed malformed legacy state directly: canonical setters reject
        // placeholders, while doctor must still diagnose an already-existing
        // hand-edited/legacy file.
        store.keys.insert("groq".into(), vec!["k1".into()]);
        store.save();
        let (report, problems) = auth_store_doctor_report();
        assert!(!problems, "placeholder slots must not fail the check");
        assert!(
            report.contains("placeholder/blank slot"),
            "report: {report}"
        );
    }

    #[test]
    fn keys_arg_completions_include_doctor() {
        let completions = crate::get_arg_completions("keys", "d");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert_eq!(values, vec!["doctor"], "completions: {values:?}");
    }

    #[test]
    fn keys_provider_completions_include_firecrawl_from_cold_store() {
        let _home = TestHome::new();
        // No keys stored: known providers must still autocomplete.
        let completions = crate::keys::KeysCommand.arg_completions("set fire");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(
            values.contains(&"set firecrawl"),
            "firecrawl must complete with an empty store: {values:?}"
        );
        assert!(
            values.contains(&"set fireworks"),
            "known providers must complete: {values:?}"
        );

        let groq = crate::keys::KeysCommand.arg_completions("set gro");
        let groq_values: Vec<&str> = groq.iter().map(|c| c.value.as_str()).collect();
        assert!(
            groq_values.contains(&"set groq"),
            "groq must complete: {groq_values:?}"
        );
    }

    #[test]
    fn keys_provider_completions_merge_stored_and_known() {
        let _home = TestHome::new();
        let mut store = AuthStore::load();
        store.set_keys("custom-provider", vec!["cpk-real-key-12345678".into()]);
        store.save();

        let completions = crate::keys::KeysCommand.arg_completions("set custom");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        assert!(
            values.contains(&"set custom-provider"),
            "stored providers must complete alongside known ones: {values:?}"
        );

        // A stored provider that is also known is not duplicated.
        let completions = crate::keys::KeysCommand.arg_completions("set groq");
        let values: Vec<&str> = completions.iter().map(|c| c.value.as_str()).collect();
        let matches = values.iter().filter(|v| **v == "set groq").count();
        assert_eq!(matches, 1, "no duplicate suggestions: {values:?}");
    }

    #[test]
    fn doctor_flags_placeholder_slots() {
        let _home = TestHome::new();
        let mut store = AuthStore::load();
        // Seed one valid and one malformed legacy slot directly so doctor can
        // report the malformed slot without weakening canonical writes.
        store.keys.insert(
            "groq".into(),
            vec!["gsk-real-key-12345678".into(), "k1".into()],
        );
        store.save();

        let result = cmd_doctor();
        let CommandResult::Message(msg) = result else {
            panic!("expected Message, got: {result:?}")
        };
        assert!(msg.contains("placeholder/blank slot"), "msg: {msg}");
        assert!(msg.contains("k1"), "msg: {msg}");
        assert!(
            msg.contains("gsk-real-key-12345678") || msg.contains("rotation key"),
            "msg: {msg}"
        );
    }
}
