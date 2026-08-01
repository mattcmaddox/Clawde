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
         \n\
         Examples:\n\
           /keys set groq gsk_key1 gsk_key2 gsk_key3\n\
           /keys add groq gsk_key4\n\
           /keys remove groq 1\n\
           /keys list groq\n\
           /keys health"
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
        ];

        // Second-level completions: provider IDs (from both credentials
        // and the multi-key store so users who only use /keys can tab-complete).
        if partial.starts_with("list ") || partial.starts_with("set ") {
            let prefix = partial
                .trim_start_matches("list ")
                .trim_start_matches("set ");
            let store = AuthStore::load();
            let mut providers: Vec<String> = Vec::new();
            for pid in store.credentials.keys() {
                providers.push(pid.clone());
            }
            for pid in store.keys.keys() {
                if !providers.contains(pid) {
                    providers.push(pid.clone());
                }
            }
            providers.sort();
            let cmd_part = partial.split_once(' ').map(|(s, _)| s).unwrap_or("");
            for pid in providers {
                if pid.starts_with(prefix) {
                    completions.push(ArgCompletion {
                        value: format!("{} {}", cmd_part, pid),
                        description: String::new(),
                        available: true,
                    });
                }
            }
        }

        completions
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        let mut parts = args.splitn(3, ' ');
        let subcommand = parts.next().unwrap_or_default();

        match subcommand {
            "" => cmd_list(None),
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
                store.add_key(provider, valid[0].clone());
                let total = store.keys_for(provider).map(|k| k.len()).unwrap_or(0);
                CommandResult::Message(format!(
                    "Key added to '{}' — now has {} key{}.\n\
                     Key rotation is active when 2+ keys are configured.",
                    provider,
                    total,
                    if total == 1 { "" } else { "s" },
                ))
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
        return if provider_filter.is_some() {
            CommandResult::Message(format!(
                "No keys found for '{}'.\n\
                 Use /keys set {} <key> to configure.",
                provider_filter.unwrap(),
                provider_filter.unwrap(),
            ))
        } else {
            CommandResult::Message(
                "No API keys configured yet.\n\
                 Use /connect to set up a provider, or /keys set <provider> <key>."
                    .to_string(),
            )
        };
    }

    let mut lines = Vec::new();
    lines.push("Multi-Key Health".to_string());
    lines.push("━━━━━━━━━━━━━━━━━".to_string());

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
                .map(|(k, _, remaining)| (k.as_str(), *remaining))
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
                let preview = if key.len() > 12 {
                    format!("{}..{}", &key[..6], &key[key.len() - 4..])
                } else {
                    key.clone()
                };
                if let Some(&remaining) = exhausted.get(key.as_str()) {
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
    }

    if provider_filter.is_none() {
        lines.push(
            "\nUse /keys health <provider> to see a single provider's key status.".to_string(),
        );
    }

    CommandResult::Message(lines.join("\n"))
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
         Makes a lightweight HEAD/GET request to each configured provider's\n\
         models endpoint and parses X-RateLimit-* response headers.\n\
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
    /// empty-completion cooldowns (spec §6.3). The real `CooldownState` is
    /// private to the api crate, so a stub is the cleanest way to drive the
    /// registry aggregation + `cmd_health` rendering.
    struct CooldownStubProvider {
        id: ProviderId,
        cooldowns: Vec<(String, u32, Option<u64>)>,
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
}
