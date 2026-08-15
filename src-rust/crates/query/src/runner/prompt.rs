// System-prompt assembly for the query loop.
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use crate::*;

/// Build the system prompt from config.
///
/// Delegates to `clawde_core::system_prompt::build_system_prompt` so that all
/// default content (capabilities, safety guidelines, dynamic-boundary marker,
/// etc.) is assembled in one place.  The `QueryConfig` fields map directly to
/// `SystemPromptOptions`:
///
/// - `system_prompt`        → `custom_system_prompt` (added to cacheable block)
/// - `append_system_prompt` → `append_system_prompt` (added after boundary)
pub(crate) fn build_system_prompt(config: &QueryConfig) -> SystemPrompt {
    use clawde_core::system_prompt::SystemPromptOptions;

    // Load project memory (memdir) into the dynamic `<memory>` section when a
    // working directory is known and auto-memory is enabled.  Empty when no
    // memory files exist yet, so the injection is a no-op on first runs.
    let memory_content = config
        .working_directory
        .as_deref()
        .filter(|dir| !dir.is_empty())
        .map(|dir| {
            use clawde_core::memdir::{
                auto_memory_path, build_memory_prompt_content_with_budget, is_auto_memory_enabled,
            };
            if !is_auto_memory_enabled(config.memory_enabled) {
                return String::new();
            }
            // Optional token budget (audit spec §18.3): cap the `<memory>`
            // block at ~4 bytes per configured token, dropping the session
            // summary first when it doesn't fit.
            let budget_bytes = config.memory_max_tokens.map(|tokens| tokens as usize * 4);
            build_memory_prompt_content_with_budget(
                &auto_memory_path(std::path::Path::new(dir)),
                budget_bytes,
            )
        })
        .unwrap_or_default();

    let opts = SystemPromptOptions {
        custom_system_prompt: config.system_prompt.clone(),
        append_system_prompt: config.append_system_prompt.clone(),
        // All other fields use sensible defaults:
        // - prefix:                auto-detect from env
        // - replace_system_prompt: false (additive mode)
        // - coordinator_mode:      false
        memory_content,
        output_style: config.output_style,
        custom_output_style_prompt: config.output_style_prompt.clone(),
        working_directory: config.working_directory.clone(),
        // Forward the session's enabled tool set so per-tool guideline blocks
        // are only emitted for tools that are actually loaded (issue #233).
        enabled_tools: config.enabled_tools.clone(),
        // Let the system prompt know when network tools are blocked so the
        // model doesn't waste turns attempting WebSearch/WebFetch. This is
        // session-scoped; the caller refreshes the snapshot before each turn.
        network_blocked: config.network_blocked,
        ..Default::default()
    };

    let text = clawde_core::system_prompt::build_system_prompt(&opts);
    SystemPrompt::Text(text)
}
