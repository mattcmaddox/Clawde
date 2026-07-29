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

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let args = args.trim();
        let mut parts = args.splitn(3, ' ');
        let subcommand = parts.next().unwrap_or_default();

        match subcommand {
            "" => cmd_list(None),
            "health" => {
                let rest = parts.next().unwrap_or_default().trim();
                cmd_health(if rest.is_empty() { None } else { Some(rest) })
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
                store.set_keys(provider, keys.clone());
                CommandResult::Message(format!(
                    "Keys for '{}' updated — {} key{} configured.\n\
                     The new keys will be picked up when the provider is next loaded.",
                    provider,
                    keys.len(),
                    if keys.len() == 1 { "" } else { "s" },
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
                store.add_key(provider, key.to_string());
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
/// and how much cooldown time remains.
fn cmd_health(provider_filter: Option<&str>) -> CommandResult {
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
