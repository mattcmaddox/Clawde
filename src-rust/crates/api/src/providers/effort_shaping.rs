//! Shared mappings from the canonical effort ladder to provider-native
//! thinking parameters.
//!
//! Single source of truth for two consumers that must never disagree:
//!
//! - the query layer, which shapes `provider_options` for direct (non-free)
//!   providers, and
//! - the composite FreeProvider, which re-shapes the same request per chain
//!   entry at dispatch time (the query layer cannot know which upstream will
//!   actually serve the request).
//!
//! Every function here is pure and deterministic over its inputs.

use clawde_core::effort::EffortLevel;

/// OpenAI-family `reasoning_effort` string for a level. Passed through
/// verbatim to OpenAI-compatible endpoints; `none`/`minimal` are the two rungs
/// below `low` and are only offered where the model's ladder exposes them.
pub fn openai_reasoning_effort_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::None => "none",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // XHigh/Max/Ultracode collapse to "high" for the generic OpenAI-family
        // `reasoning_effort` value. Providers that accept a higher tier get it
        // via a provider-specific mapping (e.g. DeepSeek's "max" below);
        // defaulting to "high" keeps unknown providers safe.
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

/// DeepSeek's `reasoning_effort` tier: the API only accepts `high` and `max`
/// (low/medium are absorbed by "high"; only the top of the ladder gets "max").
pub fn deepseek_reasoning_effort_for_level(level: EffortLevel) -> &'static str {
    match level {
        EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => "max",
        _ => "high",
    }
}

/// Google `thinkingLevel` for an effort override. Google's thinkingLevel has
/// no "none" — it floors at "low"; "minimal" is a real gemini-3 level, so
/// it passes through. `None` (no override) means the provider default which
/// Google treats as the highest tier, so we map it to "high".
pub fn google_thinking_level_for_effort(effort: Option<EffortLevel>) -> &'static str {
    match effort.unwrap_or(EffortLevel::High) {
        EffortLevel::None => "low",
        EffortLevel::Minimal => "minimal",
        EffortLevel::Low => "low",
        EffortLevel::Medium => "medium",
        // Gemini's top thinking level is "high".
        EffortLevel::High | EffortLevel::XHigh | EffortLevel::Max | EffortLevel::Ultracode => {
            "high"
        }
    }
}

/// Whether a model id belongs to the OpenAI reasoning families that accept
/// `reasoning_effort` on OpenAI-compatible endpoints (GPT-5 and O-series).
pub fn openai_reasoning_model(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("gpt-5")
        || model_id.starts_with("o1")
        || model_id.starts_with("o3")
        || model_id.starts_with("o4")
}

/// Whether a model id is an OpenAI-compatible model known to expose thinking
/// via `reasoning_effort` on free-chain upstreams (DeepSeek, Qwen3, GPT-5,
/// O-series). Gate keeps unsupported models from receiving a parameter their
/// API would reject with a 400.
pub fn openai_compat_reasoning_model(model_id: &str) -> bool {
    let model_id = model_id.to_ascii_lowercase();
    model_id.starts_with("deepseek")
        || model_id.contains("/deepseek")
        || model_id.contains("qwen3")
        || model_id.contains("qwen-3")
        || openai_reasoning_model(&model_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_effort_matches_query_ladder() {
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::None), "none");
        assert_eq!(
            openai_reasoning_effort_for_level(EffortLevel::Minimal),
            "minimal"
        );
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::Low), "low");
        assert_eq!(
            openai_reasoning_effort_for_level(EffortLevel::Medium),
            "medium"
        );
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::High), "high");
        assert_eq!(openai_reasoning_effort_for_level(EffortLevel::Max), "high");
    }

    #[test]
    fn deepseek_effort_only_has_high_and_max() {
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::Medium),
            "high"
        );
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::High),
            "high"
        );
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::XHigh),
            "max"
        );
        assert_eq!(deepseek_reasoning_effort_for_level(EffortLevel::Max), "max");
        assert_eq!(
            deepseek_reasoning_effort_for_level(EffortLevel::Ultracode),
            "max"
        );
    }

    #[test]
    fn google_level_floors_none_at_low() {
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::None)),
            "low"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Minimal)),
            "minimal"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Medium)),
            "medium"
        );
        assert_eq!(
            google_thinking_level_for_effort(Some(EffortLevel::Ultracode)),
            "high"
        );
        assert_eq!(google_thinking_level_for_effort(None), "high");
    }

    #[test]
    fn reasoning_model_guards_are_case_and_prefix_tolerant() {
        assert!(openai_reasoning_model("gpt-5-mini"));
        assert!(openai_reasoning_model("o3-2025"));
        assert!(!openai_reasoning_model("gpt-4o"));
        assert!(!openai_reasoning_model("openai/gpt-oss-120b"));
        assert!(openai_compat_reasoning_model("deepseek-v4-flash"));
        assert!(openai_compat_reasoning_model("deepseek/deepseek-v4-flash"));
        assert!(openai_compat_reasoning_model("qwen/qwen3-30b-a3b-fp8"));
        assert!(openai_compat_reasoning_model("gpt-5-codex"));
        assert!(!openai_compat_reasoning_model(
            "meta-llama/Llama-3.3-70B-Instruct"
        ));
        assert!(!openai_compat_reasoning_model("openai/gpt-oss-120b"));
    }
}
