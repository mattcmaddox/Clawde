// Session & output tools: `/skills`, `/rewind`, `/stats`, `/files`, `/rename`, `/effort`, `/summary`, `/commit`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct SkillsCommand;
pub struct RewindCommand;
pub struct StatsCommand;
pub struct FilesCommand;
pub struct RenameCommand;
pub struct EffortCommand;
pub struct SummaryCommand;
pub struct CommitCommand;

// ---- /skills -------------------------------------------------------------

#[async_trait]
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }
    fn aliases(&self) -> Vec<&str> {
        vec!["skill"]
    }
    fn description(&self) -> &str {
        "List available skills in .clawde/commands/"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let mut found: Vec<String> = Vec::new();
        let dirs = [
            ctx.working_dir.join(".clawde").join("commands"),
            clawde_core::config::Settings::config_dir().join("commands"),
        ];

        for dir in &dirs {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.extension().is_some_and(|e| e == "md") {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            let name = stem.to_string();
                            if !found.contains(&name) {
                                found.push(name);
                            }
                        }
                    }
                }
            }
        }

        // Include skills contributed by installed plugins.
        if let Some(registry) = clawde_plugins::global_plugin_registry() {
            for skill_dir in registry.all_skill_paths() {
                if let Ok(entries) = std::fs::read_dir(&skill_dir) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        // Skills can be individual .md files or subdirs with SKILL.md.
                        if p.is_dir() {
                            if p.join("SKILL.md").exists() || p.join("skill.md").exists() {
                                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                                    let skill_name = name.to_string();
                                    if !found.contains(&skill_name) {
                                        found.push(skill_name);
                                    }
                                }
                            }
                        } else if p.extension().is_some_and(|e| e == "md") {
                            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                                let name = stem.to_string();
                                if !found.contains(&name) {
                                    found.push(name);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Include discovered skills from .clawde/skills/ and configured paths/URLs.
        let discovered = clawde_core::discover_skills(&ctx.working_dir, &ctx.config.skills);

        let mut output = if found.is_empty() && discovered.is_empty() {
            return CommandResult::Message(
                "No skills found.\nCreate .md files in .clawde/commands/ to define skills.\n\
                 Example: .clawde/commands/review.md"
                    .to_string(),
            );
        } else if found.is_empty() {
            String::new()
        } else {
            found.sort();
            format!(
                "Available skills ({}):\n{}",
                found.len(),
                found
                    .iter()
                    .map(|s| format!("  /{}", s))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };

        if !discovered.is_empty() {
            let mut disc_list: Vec<(&String, &clawde_core::DiscoveredSkill)> =
                discovered.iter().collect();
            disc_list.sort_by_key(|(name, _)| name.as_str());

            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("\nDiscovered skills ({}):\n", disc_list.len()));
            for (name, skill) in disc_list {
                output.push_str(&format!(
                    "  /{} — {} ({})\n",
                    name,
                    skill.description,
                    skill.source_path.display()
                ));
            }
        }

        CommandResult::Message(output.trim_end().to_string())
    }
}

// ---- /rewind -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RewindCommand {
    fn name(&self) -> &str {
        "rewind"
    }
    fn description(&self) -> &str {
        "Interactively select a message to rewind to"
    }
    fn help(&self) -> &str {
        "Usage: /rewind\n\
         Opens an interactive overlay to select the message to rewind to.\n\
         Use ↑↓ to navigate, Enter to select, y/n to confirm."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        if ctx.messages.is_empty() {
            return CommandResult::Message(
                "Nothing to rewind — conversation is empty.".to_string(),
            );
        }
        CommandResult::OpenRewindOverlay
    }
}

// ---- /stats --------------------------------------------------------------

#[async_trait]
impl SlashCommand for StatsCommand {
    fn name(&self) -> &str {
        "stats"
    }
    fn description(&self) -> &str {
        "Show token usage and cost statistics"
    }
    fn help(&self) -> &str {
        "Usage: /stats\n\n\
         Shows detailed token usage and cost breakdown for the current session,\n\
         including cache creation/read token counts, turn counts, and session duration.\n\
         Use /usage for quota and account info. Use /cost for a quick cost summary."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        let input = ctx.cost_tracker.input_tokens();
        let output = ctx.cost_tracker.output_tokens();
        let cache_creation = ctx.cost_tracker.cache_creation_tokens();
        let cache_read = ctx.cost_tracker.cache_read_tokens();
        let total = ctx.cost_tracker.total_tokens();
        let cost = ctx.cost_tracker.total_cost_usd();
        let model = ctx.config.effective_model();

        // Count user/assistant turns separately.
        let user_turns = ctx
            .messages
            .iter()
            .filter(|m| m.role == clawde_core::types::Role::User)
            .count();
        let assistant_turns = ctx
            .messages
            .iter()
            .filter(|m| m.role == clawde_core::types::Role::Assistant)
            .count();

        // Count tool-use invocations.
        let tool_calls: usize = ctx
            .messages
            .iter()
            .map(|m| m.get_tool_use_blocks().len())
            .sum();

        // Cost breakdown note: cache-read tokens are cheaper than input, and
        // cache-creation tokens are slightly more expensive. Provide a note if
        // caching is active.
        let cache_note = if cache_creation > 0 || cache_read > 0 {
            format!(
                "\n  (Cache write: {:>10}    Cache read: {:>10})",
                cache_creation, cache_read
            )
        } else {
            String::new()
        };

        CommandResult::Message(format!(
            "Session Statistics\n\
             ══════════════════\n\
             Model:          {model}\n\
             \n\
             Conversation:\n\
               User turns:     {user_turns:>10}\n\
               Assistant turns:{assistant_turns:>10}\n\
               Tool calls:     {tool_calls:>10}\n\
             \n\
             Token usage:\n\
               Input:          {input:>10}\n\
               Output:         {output:>10}\n\
               Total:          {total:>10}{cache_note}\n\
             \n\
             Estimated cost:   ${cost:.4}\n\
             \n\
             Use /usage for quota info · /cost for quick cost · /extra-usage for per-call breakdown",
            model = model,
            user_turns = user_turns,
            assistant_turns = assistant_turns,
            tool_calls = tool_calls,
            input = input,
            output = output,
            total = total,
            cache_note = cache_note,
            cost = cost,
        ))
    }
}

// ---- /files --------------------------------------------------------------

#[async_trait]
impl SlashCommand for FilesCommand {
    fn name(&self) -> &str {
        "files"
    }
    fn description(&self) -> &str {
        "List files referenced in the current conversation"
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        use std::collections::HashSet;
        // Scan message content for file paths (simple heuristic)
        let mut files: HashSet<String> = HashSet::new();
        let path_re =
            regex::Regex::new(r#"(?m)([A-Za-z]:[\\/][^\s,;:"'<>]+|/[^\s,;:"'<>]{3,})"#).ok();

        for msg in &ctx.messages {
            let text = msg.get_all_text();
            if let Some(ref re) = path_re {
                for cap in re.captures_iter(&text) {
                    let path = cap[1].trim().to_string();
                    if std::path::Path::new(&path).exists() {
                        files.insert(path);
                    }
                }
            }
        }

        if files.is_empty() {
            return CommandResult::Message(
                "No referenced files detected in the conversation.".to_string(),
            );
        }

        let mut sorted: Vec<String> = files.into_iter().collect();
        sorted.sort();

        CommandResult::Message(format!(
            "Referenced files ({}):\n{}",
            sorted.len(),
            sorted
                .iter()
                .map(|f| format!("  {}", f))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

// ---- /rename -------------------------------------------------------------

#[async_trait]
impl SlashCommand for RenameCommand {
    fn name(&self) -> &str {
        "rename"
    }
    fn description(&self) -> &str {
        "Rename the current session"
    }
    fn help(&self) -> &str {
        "Usage: /rename [new name]\n\n\
         With a name: sets the session title immediately.\n\
         With no argument: auto-generates a kebab-case name from the conversation.\n\n\
         Examples:\n\
           /rename fix-login-bug\n\
           /rename              — auto-generate from conversation history"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let name = args.trim();

        if !name.is_empty() {
            // Explicit name provided: rename immediately.
            return CommandResult::RenameSession(name.to_string());
        }

        // No name given — auto-generate from conversation context.
        if ctx.messages.is_empty() {
            return CommandResult::Error(
                "No conversation context yet. Usage: /rename <name>".to_string(),
            );
        }

        // Build a short conversation excerpt (up to ~2000 chars) for the model.
        let excerpt: String = ctx
            .messages
            .iter()
            .take(20)
            .filter_map(|m| {
                let text = m.get_all_text();
                if text.is_empty() {
                    return None;
                }
                let role = match m.role {
                    clawde_core::types::Role::User => "User",
                    clawde_core::types::Role::Assistant => "Assistant",
                };
                Some(format!(
                    "{}: {}",
                    role,
                    text.chars().take(300).collect::<String>()
                ))
            })
            .collect::<Vec<_>>()
            .join("\n");

        if excerpt.is_empty() {
            return CommandResult::Error(
                "No text content in conversation. Usage: /rename <name>".to_string(),
            );
        }

        let provider = match resolve_command_provider(ctx).await {
            Some(provider) => provider,
            None => {
                return CommandResult::Error(
                    "Could not create a provider client for auto-naming.\n\
                     Use /rename <name> to set the name manually."
                        .to_string(),
                );
            }
        };
        let rename_model = resolve_fast_model_id(&ctx.config);

        let system_prompt = "Generate a short kebab-case name (2-4 words) that captures the \
            main topic of this conversation. Use lowercase words separated by hyphens. \
            Examples: fix-login-bug, add-auth-feature, refactor-api-client. \
            Respond with ONLY the name, nothing else.";

        let request = clawde_api::ProviderRequest {
            model: rename_model,
            messages: vec![Message::user(format!(
                "Conversation to name:\n\n{}",
                &excerpt[..excerpt.len().min(2000)]
            ))],
            system_prompt: Some(clawde_api::SystemPrompt::Text(system_prompt.to_string())),
            tools: vec![],
            max_tokens: 64,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            effort_level: ctx.effort,
            provider_options: serde_json::Value::Object(Default::default()),
            strict_route: false,
        };

        match provider.create_message(request).await {
            Ok(response) => {
                let raw_text = text_from_content_blocks(&response.content)
                    .trim()
                    .to_string();

                let generated = raw_text
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '-')
                    .collect::<String>();

                // Trim leading/trailing hyphens and ensure non-empty.
                let cleaned = generated.trim_matches('-').to_string();
                if cleaned.is_empty() {
                    return CommandResult::Error(
                        "Could not generate a valid name from conversation. \
                         Use /rename <name> to set manually."
                            .to_string(),
                    );
                }

                CommandResult::RenameSession(cleaned)
            }
            Err(e) => CommandResult::Error(format!(
                "Auto-name generation failed: {e}\n\
                 Use /rename <name> to set the name manually."
            )),
        }
    }
}

// ---- /effort -------------------------------------------------------------

#[async_trait]
impl SlashCommand for EffortCommand {
    fn name(&self) -> &str {
        "effort"
    }
    fn description(&self) -> &str {
        "Set the model's thinking effort (low | normal | high)"
    }
    fn help(&self) -> &str {
        "Usage: /effort [low|normal|high]\n\
         Sets how much computation the model uses for reasoning.\n\
         'high' enables extended thinking with a larger budget."
    }

    fn arg_completions(&self, _partial: &str) -> Vec<super::ArgCompletion> {
        vec![
            super::ArgCompletion {
                value: "none".into(),
                description: "No reasoning at all".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "minimal".into(),
                description: "Smallest reasoning budget".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "low".into(),
                description: "Quick, straightforward implementation".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "medium".into(),
                description: "Balanced approach (default)".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "high".into(),
                description: "Comprehensive with extensive testing".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "xhigh".into(),
                description: "Extended reasoning, higher thinking budget".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "max".into(),
                description: "Maximum capability, deepest reasoning".into(),
                available: true,
            },
            super::ArgCompletion {
                value: "ultracode".into(),
                description: "Top reasoning + delegation workflow".into(),
                available: true,
            },
        ]
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        use clawde_core::effort::EffortLevel;
        let args = args.trim();
        if args.is_empty() {
            return CommandResult::Message(
                "Usage: /effort [none|minimal|low|medium|high|xhigh|max|ultracode]\n\n\
                 Current effort: normal\n\n\
                 Available levels:\n\
                   none      — No reasoning at all\n\
                   minimal   — Smallest reasoning budget\n\
                   low       — Quick, straightforward implementation\n\
                   medium    — Balanced approach (default, also 'normal')\n\
                   high      — Comprehensive with extensive testing\n\
                   xhigh     — Extended reasoning, higher thinking budget\n\
                   max       — Maximum capability, deepest reasoning\n\
                   ultracode — Top reasoning + delegation workflow"
                    .to_string(),
            );
        }

        let level = match EffortLevel::from_str(args) {
            Some(l) => l,
            None => {
                return CommandResult::Error(format!(
                    "Unknown effort level '{}'. Use: none | minimal | low | medium | high | xhigh | max | ultracode",
                    args
                ));
            }
        };

        let label = format!("{} {}", level.symbol(), level.label());
        CommandResult::ConfigChangeMessage(ctx.config.clone(), format!("Effort: {label}"))
    }
}

// ---- /summary ------------------------------------------------------------

#[async_trait]
impl SlashCommand for SummaryCommand {
    fn name(&self) -> &str {
        "summary"
    }
    fn description(&self) -> &str {
        "Generate a brief summary of the conversation so far"
    }
    fn arg_completions(&self, _partial: &str) -> Vec<ArgCompletion> {
        vec![
            ArgCompletion {
                value: "decisions".into(),
                description: "Highlight key decisions made".into(),
                available: true,
            },
            ArgCompletion {
                value: "files".into(),
                description: "Focus on files created or modified".into(),
                available: true,
            },
        ]
    }
    fn help(&self) -> &str {
        "Usage: /summary [focus]\n\n\
         Generates a concise 3-5 sentence summary of the conversation using\n\
         the active provider.  The summary focuses on what has been accomplished\n\
         and the current state.\n\n\
         An optional focus argument tailors the summary:\n\
           /summary decisions     — highlight key decisions made\n\
           /summary files         — focus on files created or modified"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let count = ctx.messages.len();
        if count == 0 {
            return CommandResult::Message(
                "No messages in conversation yet. Start a conversation first.".to_string(),
            );
        }

        // For very short conversations, fall back to the old UserMessage approach
        // since an API call isn't worth the overhead.
        if count < 3 {
            let focus = if args.trim().is_empty() {
                String::new()
            } else {
                format!(" Focus on: {}.", args.trim())
            };
            return CommandResult::UserMessage(format!(
                "Please provide a brief (2-3 sentence) summary of our conversation \
                 so far, focusing on what has been accomplished and the current state.{}",
                focus
            ));
        }

        // Get the active provider.
        let provider = match resolve_command_provider(ctx).await {
            Some(p) => p,
            None => {
                return CommandResult::Error(
                    "No provider available for summarisation. Configure an API key first."
                        .to_string(),
                );
            }
        };
        let summary_model = resolve_fast_model_id(&ctx.config);

        // Build a concise conversation excerpt (up to the most relevant messages).
        let excerpt = build_summary_excerpt(&ctx.messages, 4000);

        // Determine the focus instruction.
        let focus_instruction = if args.trim().is_empty() {
            String::new()
        } else {
            format!("\n\nFocus specifically on: {}.", args.trim())
        };

        let system_prompt = "You are an expert conversation summariser. Produce a concise \
            3-5 sentence summary that captures what has been accomplished and the \
            current state of the work. Be specific about file names, features built, \
            and decisions made. Use plain text only — no XML, no markdown headings.";

        let user_message = format!(
            "Please summarise the following conversation:\n\n{}{}",
            excerpt, focus_instruction
        );

        let request = clawde_api::ProviderRequest {
            model: summary_model.clone(),
            messages: vec![Message::user(user_message)],
            system_prompt: Some(clawde_api::SystemPrompt::Text(system_prompt.to_string())),
            tools: vec![],
            max_tokens: 1024,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: vec![],
            thinking: None,
            effort_level: ctx.effort,
            provider_options: serde_json::Value::Object(Default::default()),
            strict_route: false,
        };

        match provider.create_message(request).await {
            Ok(response) => {
                let raw_text = text_from_content_blocks(&response.content);
                if raw_text.trim().is_empty() {
                    return CommandResult::Error(
                        "Generated summary was empty. Try again.".to_string(),
                    );
                }

                CommandResult::Message(format!(
                    "Conversation Summary\n\
                     ═══════════════════════\n\
                     Messages: {count}  ·  Model: {summary_model}\n\n\
                     {summary}\n",
                    count = count,
                    summary_model = summary_model,
                    summary = raw_text.trim(),
                ))
            }
            Err(_e) => {
                // Fall back to the old UserMessage approach if the API call fails.
                let focus = if args.trim().is_empty() {
                    String::new()
                } else {
                    format!(" Focus on: {}.", args.trim())
                };
                CommandResult::UserMessage(format!(
                    "Please provide a brief (3-5 sentence) summary of our conversation \
                     so far, focusing on what has been accomplished and the current state.{}",
                    focus
                ))
            }
        }
    }
}

/// Build a concise conversation excerpt limited to approximately `max_chars`.
/// Takes messages from both ends: the first few (context) and the last several
/// (most recent activity), with a gap marker in between if truncated.
fn build_summary_excerpt(messages: &[Message], max_chars: usize) -> String {
    let total = messages.len();
    if total == 0 {
        return String::new();
    }

    // Build an indexed transcript.
    let mut parts: Vec<(usize, String)> = Vec::with_capacity(total);
    for (i, msg) in messages.iter().enumerate() {
        let role_label = match msg.role {
            clawde_core::types::Role::User => "User",
            clawde_core::types::Role::Assistant => "Assistant",
        };
        let text = msg.get_all_text();
        // Truncate very long individual messages to 500 chars
        let truncated: String = text.chars().take(500).collect();
        let ellipsis = if text.len() > 500 {
            "… (truncated)"
        } else {
            ""
        };
        parts.push((i, format!("{}: {}{}", role_label, truncated, ellipsis)));
    }

    // Estimate full size.
    let full_size: usize = parts.iter().map(|(_, s)| s.len()).sum();
    if full_size <= max_chars {
        return parts
            .into_iter()
            .map(|(_, s)| s)
            .collect::<Vec<_>>()
            .join("\n\n");
    }

    // Keep the first 2 messages (context) and the last N messages (recent activity).
    let keep_head = 2.min(total);
    let keep_tail = 3.min(total.saturating_sub(keep_head));
    let mut available = max_chars;
    let mut result = Vec::new();

    // Always include the first messages for context.
    for (_, s) in parts.iter().take(keep_head) {
        if s.len() <= available {
            result.push(s.clone());
            available = available.saturating_sub(s.len());
        }
    }

    // Only add a gap marker when messages are actually omitted.
    let omitted = total.saturating_sub(keep_head + keep_tail);
    if omitted > 0 {
        let gap = if omitted == 1 {
            "… (1 message omitted)\n".to_string()
        } else {
            format!("… ({} messages omitted)\n", omitted)
        };
        let gap_len = gap.len();
        if gap_len <= available {
            result.push(gap);
            available = available.saturating_sub(gap_len);
        }
    }

    // Include the last few messages (most recent activity).
    let keep_tail = 3.min(total.saturating_sub(keep_head));
    for (_, s) in parts.iter().rev().take(keep_tail).rev() {
        if s.len() <= available {
            result.push(s.clone());
            available = available.saturating_sub(s.len());
        }
    }

    result.join("\n\n")
}

// ---- /commit -------------------------------------------------------------

#[async_trait]
impl SlashCommand for CommitCommand {
    fn name(&self) -> &str {
        "commit"
    }
    fn description(&self) -> &str {
        "Ask Clawde to commit staged changes"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let extra = if args.trim().is_empty() {
            String::new()
        } else {
            format!(" with message: {}", args.trim())
        };

        CommandResult::UserMessage(format!(
            "Please commit the currently staged git changes{}. \
             Run `git diff --cached` to see what's staged, \
             write an appropriate commit message following the repository's conventions, \
             and run `git commit`.",
            extra
        ))
    }
}
