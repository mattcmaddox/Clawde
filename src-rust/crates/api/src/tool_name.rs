//! Tool-name sanitization for model responses.
//!
//! Some serving stacks leak their chat-template's channel markers into
//! function-call names. The measured case (2026-09-06 katban live smoke):
//! NVIDIA NIM serving `gpt-oss-120b` returned tool names like
//! `Grep<|channel|>`, `Write<|channel|>json`, `Write<|channel|>analysis` —
//! the gpt-oss harmony protocol's `<|channel|>` token bleeding out of the
//! tool-call channel into the name field. Clawde rejected each such call as
//! "Unknown tool: Grep<|channel|>", burning agent turns on no-ops.
//!
//! [`sanitize_tool_name`] strips any `<|...|>` marker so the wire name
//! resolves to the real tool. It is deliberately applied at every site that
//! reads a tool name off a model response (OpenAI-compat streaming +
//! non-streaming, Ollama native, Codex, MiniMax), because any host can serve
//! a harmony-format model.

/// Sanitize a tool name returned by a model. The measured leak shape is
/// `<tool><|marker|><channel-name>` (e.g. `Write<|channel|>json`) — the
/// serving stack's chat template bleeding its channel switch into the name
/// field — so everything from the first `<|` marker onward is noise: the
/// function returns the text *before* it. A clean name is unchanged.
////// The `<` and `|` characters cannot appear in a registered tool's name, so
/// a marker at any position can never be a legitimate part of one.
pub fn sanitize_tool_name(name: &str) -> String {
    match name.find("<|") {
        Some(cut) => name[..cut].to_string(),
        None => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_tool_name;

    #[test]
    fn strips_harmony_channel_markers() {
        // The exact names the katban smoke caught from nvidia's gpt-oss.
        assert_eq!(sanitize_tool_name("Grep<|channel|>"), "Grep");
        assert_eq!(sanitize_tool_name("Write<|channel|>json"), "Write");
        assert_eq!(sanitize_tool_name("Write<|channel|>analysis"), "Write");
        assert_eq!(sanitize_tool_name("Bash<|channel|>commentary"), "Bash");
    }

    #[test]
    fn multiple_and_unterminated_markers() {
        // A leading marker means the model never emitted a real name; the
        // empty result is correct (the call fails loudly as unknown-tool
        // rather than dispatching to a hallucinated tool).
        assert_eq!(sanitize_tool_name("<|channel|>Edit"), "");
        // Unterminated marker: same rule, cut at `<|`.
        assert_eq!(sanitize_tool_name("Read<|channel"), "Read");
        assert_eq!(sanitize_tool_name("Read<|"), "Read");
    }

    #[test]
    fn clean_names_pass_through_unchanged() {
        assert_eq!(sanitize_tool_name("Grep"), "Grep");
        assert_eq!(sanitize_tool_name("web_search"), "web_search");
        assert_eq!(sanitize_tool_name(""), "");
        // Generic template markers with a different inner word.
        assert_eq!(sanitize_tool_name("Foo<|end|>"), "Foo");
        assert_eq!(sanitize_tool_name("Foo<|message|>Bar"), "Foo");
    }
}
