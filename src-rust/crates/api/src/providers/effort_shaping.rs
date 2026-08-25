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

/// Whether a provider speaks the OpenAI-compatible chat wire format and can
/// accept OpenAI-style thinking parameters (`reasoning_effort`, `thinking`,
/// `chat_template_kwargs`, …) on its request body. Previously the query
/// layer's private list; moved here so the FreeProvider chain and the future
/// HTTP gateway share one definition with the direct-provider path.
pub fn is_openaiish_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "openai"
            | "azure"
            | "groq"
            | "mistral"
            | "deepseek"
            | "xai"
            | "openrouter"
            | "togetherai"
            | "together-ai"
            | "perplexity"
            | "cerebras"
            | "deepinfra"
            | "venice"
            | "huggingface"
            | "nvidia"
            | "cloudflare"
            | "siliconflow"
            | "sambanova"
            | "moonshot"
            | "zhipu"
            | "zai"
            | "qwen"
            | "alibaba"
            | "nebius"
            | "novita"
            | "ovhcloud"
            | "scaleway"
            | "vultr"
            | "vultr-ai"
            | "baseten"
            | "friendli"
            | "upstage"
            | "stepfun"
            | "fireworks"
            | "ollama"
            | "codex"
            | "openai-codex"
            | "poolside"
            | "lmstudio"
            | "lm-studio"
            | "llamacpp"
            | "llama-cpp"
    )
}

/// Shape per-provider thinking / effort parameters into `options`.
///
/// Single source of truth for the per-provider mapping, consumed by:
///
/// - the query layer's `build_provider_options` (direct providers), and
/// - FreeProvider's `shape_thinking_for_upstream` (per chain entry at
///   dispatch time — the query layer cannot know which upstream will serve a
///   `free` request).
///
/// Both callers previously maintained their own copies and drifted; every
/// provider arm below is shared. Provider-specific *extras* that only the
/// direct path needs (Codex effort tiers, `textVerbosity`, settings merge,
/// …) stay in the query layer and are layered on after this returns.
///
/// `model_id` must be pre-lowercased by the caller. `max_tokens` is the
/// request's (already capped) output budget and only affects Google 2.5's
/// `thinkingBudget`, which must stay below `maxOutputTokens`. No-op for
/// providers/models without thinking control.
pub fn shape_provider_thinking(
    options: &mut serde_json::Map<String, serde_json::Value>,
    provider_id: &str,
    model_id: &str,
    effort_level: Option<EffortLevel>,
    thinking_budget: Option<u32>,
    max_tokens: Option<u32>,
) {
    // Gemini: thinkingConfig lives in generationConfig and is expressed as a
    // budget (2.5 models) or a level (3.x models).
    if provider_id == "google" && model_id.contains("gemini") {
        if model_id.contains("2.5") {
            match effort_level {
                Some(EffortLevel::None) => {
                    options.insert(
                        "thinkingConfig".to_string(),
                        serde_json::json!({
                            "includeThoughts": false,
                            "thinkingBudget": 0,
                        }),
                    );
                }
                Some(level) => {
                    let mut budget = thinking_budget
                        .or_else(|| level.thinking_budget_tokens())
                        .unwrap_or(0);
                    // Gemini requires budget < maxOutputTokens; clamp against
                    // the request's (already capped) max_tokens when known.
                    if let Some(max_tokens) = max_tokens {
                        budget = budget.min(max_tokens.saturating_sub(1));
                    }
                    options.insert(
                        "thinkingConfig".to_string(),
                        serde_json::json!({
                            "includeThoughts": true,
                            "thinkingBudget": budget,
                        }),
                    );
                }
                None => {
                    // No effort override: leave the provider default.
                }
            }
        } else if model_id.contains("3.") || model_id.contains("gemini-3") {
            let off = effort_level == Some(EffortLevel::None);
            options.insert(
                "thinkingConfig".to_string(),
                serde_json::json!({
                    "includeThoughts": !off,
                    "thinkingLevel": if off {
                        "minimal"
                    } else {
                        google_thinking_level_for_effort(effort_level)
                    },
                }),
            );
        }
    }

    // DeepSeek exposes thinking independently of the OpenAI GPT reasoning
    // families. Model-gated so a deepseek model on any OpenAI-compatible
    // upstream (direct `deepseek`, or free-chain cline / opencode-zen /
    // openrouter) shapes identically. `none` and `low` both disable it.
    if model_id.starts_with("deepseek") || model_id.contains("/deepseek") {
        let disabled = matches!(
            effort_level,
            Some(EffortLevel::None) | Some(EffortLevel::Low)
        );
        options.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": if disabled { "disabled" } else { "enabled" } }),
        );
        if disabled {
            // Drop any leftover effort key so a disabled request never
            // carries reasoning parameters.
            options.remove("reasoningEffort");
        } else {
            options.insert(
                "reasoningEffort".to_string(),
                serde_json::json!(deepseek_reasoning_effort_for_level(
                    effort_level.unwrap_or(EffortLevel::Medium)
                )),
            );
        }
    }

    // Qwen (DashScope): `enable_thinking` boolean, on only when a budget was
    // requested. Qwen3 models on *other* OpenAI-compatible upstreams fall
    // through to the reasoningEffort arm below (their APIs accept it).
    if provider_id == "qwen" && thinking_budget.is_some() && !model_id.contains("kimi-k2-thinking")
    {
        options.insert("enable_thinking".to_string(), serde_json::json!(true));
    }

    // Z.AI / Zhipu: GLM models expose thinking via `thinking.type` with
    // `clear_thinking` for preserved thinking across coding turns.
    if provider_id == "zai" || provider_id == "zhipu" {
        let enabled = effort_level != Some(EffortLevel::None);
        options.insert(
            "thinking".to_string(),
            serde_json::json!({
                "type": if enabled { "enabled" } else { "disabled" },
                "clear_thinking": false,
            }),
        );
    }

    // Poolside: binary thinking toggle via chat_template_kwargs. Thinking is
    // on by default and consumes from max_tokens; effort None is the only
    // level that turns it off.
    if provider_id == "poolside" && effort_level == Some(EffortLevel::None) {
        options.insert(
            "chat_template_kwargs".to_string(),
            serde_json::json!({ "enable_thinking": false }),
        );
    }

    // OpenAI reasoning families (GPT-5 / O-series / Qwen3) on OpenAI-
    // compatible providers. Explicit-off maps to "none" so the model thinks
    // at its minimum rather than not at all. DeepSeek models and the qwen
    // provider are handled by their own arms above.
    if provider_id != "qwen"
        && is_openaiish_provider(provider_id)
        && !model_id.starts_with("deepseek")
        && !model_id.contains("/deepseek")
        && openai_compat_reasoning_model(model_id)
    {
        options.insert(
            "reasoningEffort".to_string(),
            serde_json::json!(openai_reasoning_effort_for_level(
                effort_level.unwrap_or(EffortLevel::Medium)
            )),
        );
    }
}

// ---------------------------------------------------------------------------
// Thinking inspector (read-only)
// ---------------------------------------------------------------------------

use super::free::{FreeLastRoute, FreeUpstream};

/// How Clawde's shaping treats thinking for the inspected model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Clawde sends an explicit enable (or a non-minimum behavioral tier).
    Enabled,
    /// Clawde sends an explicit disable.
    Disabled,
    /// The model/provider has no thinking knob in Clawde's shaping.
    NotSupported,
}

/// The kind of thinking control the provider's API exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingControl {
    /// A hard token budget (Gemini 2.5 `thinkingBudget`).
    Budget,
    /// A behavioral level, not a strict token budget
    /// (`reasoningEffort`, `thinkingLevel`).
    Behavioral,
    /// A binary on/off (`thinking.type`, `enable_thinking`,
    /// `chat_template_kwargs`).
    Toggle,
}

/// One row of the thinking inspector: what Clawde would send for a
/// (provider, model, effort) triple, plus the upstream's constraints and the
/// last successful dispatch's usage.
#[derive(Debug, Clone, PartialEq)]
pub struct ThinkingInspection {
    pub provider_id: String,
    pub provider_title: String,
    pub model_id: String,
    pub mode: ThinkingMode,
    pub control: Option<ThinkingControl>,
    /// Human-readable wire param, e.g. `reasoningEffort: high`.
    pub wire_param: Option<String>,
    /// Effective thinking budget after clamping (budget-type controls only).
    pub effective_budget: Option<u32>,
    pub max_tokens_cap: Option<u32>,
    pub context_window: Option<u32>,
    pub tool_calling: bool,
    pub vision: bool,
    pub warnings: Vec<String>,
    pub last_response: Option<LastResponseInspection>,
}

/// Usage of the last successful dispatch, plus diagnostic flags.
#[derive(Debug, Clone, PartialEq)]
pub struct LastResponseInspection {
    pub upstream_id: String,
    pub model: String,
    pub reasoning_tokens: u64,
    pub completion_tokens: u64,
    pub stop_reason: Option<String>,
    /// Human-readable diagnostics ("budget eaten", "truncated", …).
    pub flags: Vec<String>,
}

/// Read-only view of what thinking parameters Clawde would send for a
/// (provider, model, effort) triple — the thinking inspector's row data.
///
/// Mirrors [`shape_provider_thinking`] exactly: it calls the real shaping
/// function on a fresh options map and interprets what was written, so the
/// inspector can never drift from dispatch behavior. `upstream` supplies the
/// catalog constraints (cap, context, capabilities); pass `None` for
/// direct (non-free) providers. `last_route` supplies the previous turn's
/// usage; pass `None` when there is none.
///
/// Pure and deterministic over its inputs — safe for the TUI to call on
/// every repaint of the pickers.
pub fn inspect_thinking(
    provider_id: &str,
    model_id: &str,
    effort_level: Option<EffortLevel>,
    thinking_budget: Option<u32>,
    max_tokens: Option<u32>,
    upstream: Option<&FreeUpstream>,
    last_route: Option<&FreeLastRoute>,
) -> ThinkingInspection {
    let model = model_id.to_ascii_lowercase();
    let mut options = serde_json::Map::new();
    shape_provider_thinking(
        &mut options,
        provider_id,
        &model,
        effort_level,
        thinking_budget,
        max_tokens,
    );

    let mut warnings = Vec::new();

    // Ladder quirks that hold regardless of the wire param.
    if effort_level == Some(EffortLevel::Low) {
        warnings.push(
            "Low disables thinking; Minimal enables a 1,024-token budget — use Minimal for cheap thinking"
                .to_string(),
        );
        warnings.push("Low forces temperature 0.0".to_string());
    }
    if let Some(upstream) = upstream {
        if let (Some(cap), Some(mt)) = (upstream.max_tokens_cap, max_tokens) {
            if mt > cap {
                warnings.push(format!("max_tokens {mt} clamped to upstream cap {cap}"));
            }
        }
    }

    // Interpret what shape_provider_thinking wrote. Ordered to match the
    // arms: google thinkingConfig, then thinking.type, then enable_thinking,
    // then chat_template_kwargs, then the reasoningEffort catch-all.
    let mut mode = ThinkingMode::NotSupported;
    let mut control: Option<ThinkingControl> = None;
    let mut wire_param: Option<String> = None;
    let mut effective_budget: Option<u32> = None;

    if let Some(cfg) = options.get("thinkingConfig") {
        // Google: 2.5 models take a budget, 3.x take a level.
        let include = cfg
            .get("includeThoughts")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if let Some(budget) = cfg.get("thinkingBudget").and_then(|v| v.as_u64()) {
            if !include || budget == 0 {
                mode = ThinkingMode::Disabled;
                control = Some(ThinkingControl::Budget);
                wire_param = Some("thinkingConfig: off".to_string());
            } else {
                mode = ThinkingMode::Enabled;
                control = Some(ThinkingControl::Budget);
                effective_budget = Some(budget as u32);
                wire_param = Some(format!("thinkingBudget: {budget}"));
                // Surface the clamp the shaping applied (budget < max_tokens).
                let raw = thinking_budget
                    .or_else(|| effort_level.and_then(|l| l.thinking_budget_tokens()))
                    .unwrap_or(budget as u32);
                if let Some(mt) = max_tokens {
                    if raw >= mt {
                        warnings.push(format!(
                            "budget {raw} clamped below max_tokens {mt} (Gemini requires budget < maxOutputTokens)"
                        ));
                    }
                }
            }
        } else if let Some(level) = cfg.get("thinkingLevel").and_then(|v| v.as_str()) {
            if include {
                mode = ThinkingMode::Enabled;
                control = Some(ThinkingControl::Behavioral);
                wire_param = Some(format!("thinkingLevel: {level}"));
            } else {
                mode = ThinkingMode::Disabled;
                wire_param = Some(format!("thinkingLevel: {level} (off)"));
            }
        }
    } else if let Some(thinking) = options.get("thinking") {
        // Z.AI / Zhipu / DeepSeek: thinking.type.
        let t = thinking
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("enabled");
        if t == "disabled" {
            mode = ThinkingMode::Disabled;
            control = Some(ThinkingControl::Toggle);
            wire_param = Some("thinking.type: disabled".to_string());
        } else {
            mode = ThinkingMode::Enabled;
            control = Some(ThinkingControl::Toggle);
            wire_param = Some("thinking.type: enabled".to_string());
            // DeepSeek also writes a behavioral effort tier alongside.
            if let Some(effort) = options.get("reasoningEffort").and_then(|v| v.as_str()) {
                wire_param = Some(format!(
                    "thinking.type: enabled · reasoningEffort: {effort}"
                ));
            }
        }
    } else if options.get("enable_thinking").and_then(|v| v.as_bool()) == Some(true) {
        mode = ThinkingMode::Enabled;
        control = Some(ThinkingControl::Toggle);
        wire_param = Some("enable_thinking: true".to_string());
    } else if let Some(kwargs) = options.get("chat_template_kwargs") {
        if kwargs.get("enable_thinking").and_then(|v| v.as_bool()) == Some(false) {
            mode = ThinkingMode::Disabled;
            control = Some(ThinkingControl::Toggle);
            wire_param = Some("chat_template_kwargs.enable_thinking: false".to_string());
        }
    } else if let Some(effort) = options.get("reasoningEffort").and_then(|v| v.as_str()) {
        mode = ThinkingMode::Enabled;
        control = Some(ThinkingControl::Behavioral);
        wire_param = Some(format!("reasoningEffort: {effort}"));
        if effort == "none" {
            warnings
                .push("reasoningEffort \"none\" is the minimum thinking tier, not off".to_string());
        }
    }

    // Arms that write nothing on purpose: describe the default rather than
    // claiming the model has no thinking knob.
    if mode == ThinkingMode::NotSupported {
        if provider_id == "poolside" && effort_level != Some(EffortLevel::None) {
            mode = ThinkingMode::Enabled;
            control = Some(ThinkingControl::Toggle);
            wire_param =
                Some("chat_template_kwargs.enable_thinking: true (provider default)".to_string());
            warnings.push(
                "poolside thinking is on by default and consumes from max_tokens".to_string(),
            );
        } else if provider_id == "qwen" && thinking_budget.is_none() {
            mode = ThinkingMode::Disabled;
            control = Some(ThinkingControl::Toggle);
            wire_param = Some("enable_thinking: not requested (no thinking budget)".to_string());
        } else if effort_level.is_none() && provider_id == "google" && model.contains("gemini") {
            // No override: the request carries no thinking param and the
            // provider default stands.
            mode = ThinkingMode::Enabled;
            control = Some(if model.contains("2.5") {
                ThinkingControl::Budget
            } else {
                ThinkingControl::Behavioral
            });
            wire_param = Some("provider default (no effort override)".to_string());
        }
    }

    let last_response = last_route.map(|route| {
        let mut flags = Vec::new();
        if route.usage.reasoning_tokens > 0 {
            if let Some(mt) = max_tokens {
                let mt = mt as u64;
                if route.usage.reasoning_tokens >= mt.saturating_mul(9) / 10
                    && route.usage.output_tokens == 0
                {
                    flags.push(
                        "budget eaten — reasoning ≈ max_tokens with no visible output".to_string(),
                    );
                } else if route.usage.reasoning_tokens >= mt / 2 {
                    flags.push("thinking-heavy — reasoning ≥ half the output budget".to_string());
                }
            }
        }
        if route.stop_reason.as_deref() == Some("MaxTokens") {
            flags.push("truncated — hit the max_tokens limit".to_string());
        }
        LastResponseInspection {
            upstream_id: route.upstream_id.clone(),
            model: route.model.clone(),
            reasoning_tokens: route.usage.reasoning_tokens,
            completion_tokens: route.usage.output_tokens,
            stop_reason: route.stop_reason.clone(),
            flags,
        }
    });

    ThinkingInspection {
        provider_id: provider_id.to_string(),
        provider_title: upstream
            .map(|u| u.title.to_string())
            .unwrap_or_else(|| provider_id.to_string()),
        model_id: model_id.to_string(),
        mode,
        control,
        wire_param,
        effective_budget,
        max_tokens_cap: upstream.and_then(|u| u.max_tokens_cap),
        context_window: upstream.map(|u| u.context_window),
        tool_calling: upstream.map(|u| u.tool_calling).unwrap_or(false),
        vision: upstream.map(|u| u.vision).unwrap_or(false),
        warnings,
        last_response,
    }
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

    #[test]
    fn shape_google_25_clamps_budget_below_max_tokens() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::High),
            None,
            Some(32),
        );
        assert_eq!(o["thinkingConfig"]["thinkingBudget"], serde_json::json!(31));
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(true)
        );

        // Explicit off maps to a zero budget with thoughts disabled.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(o["thinkingConfig"]["thinkingBudget"], serde_json::json!(0));
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn shape_google_3_uses_level_and_floors_off_at_minimal() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-3-flash-preview",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert_eq!(
            o["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("medium")
        );

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "google",
            "gemini-3-flash-preview",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(
            o["thinkingConfig"]["thinkingLevel"],
            serde_json::json!("minimal")
        );
        assert_eq!(
            o["thinkingConfig"]["includeThoughts"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn shape_deepseek_model_any_provider_enabled_max_disabled() {
        // Model-gated: shapes the same on the direct deepseek provider and on
        // free-chain openai-compatible upstreams.
        for provider in ["deepseek", "openrouter"] {
            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek/deepseek-v4-flash",
                Some(EffortLevel::High),
                None,
                None,
            );
            assert_eq!(o["thinking"]["type"], serde_json::json!("enabled"));
            assert_eq!(o["reasoningEffort"], serde_json::json!("high"));

            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek-v4",
                Some(EffortLevel::Max),
                None,
                None,
            );
            assert_eq!(o["reasoningEffort"], serde_json::json!("max"));

            let mut o = serde_json::Map::new();
            shape_provider_thinking(
                &mut o,
                provider,
                "deepseek-v4",
                Some(EffortLevel::None),
                None,
                None,
            );
            assert_eq!(o["thinking"]["type"], serde_json::json!("disabled"));
            assert!(o.get("reasoningEffort").is_none());
        }
    }

    #[test]
    fn shape_catch_all_gates_reasoning_families_per_provider() {
        // Qwen3 on an openaiish upstream gets reasoningEffort; explicit off
        // maps to "none" (think at minimum, don't error).
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "groq",
            "qwen/qwen3-30b-a3b-fp8",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert_eq!(o["reasoningEffort"], serde_json::json!("medium"));

        // The qwen provider itself is handled by enable_thinking, never the
        // reasoningEffort arm — DashScope would reject the unknown param.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "qwen",
            "qwen3-235b-a22b",
            Some(EffortLevel::Medium),
            Some(5_000),
            None,
        );
        assert_eq!(o["enable_thinking"], serde_json::json!(true));
        assert!(o.get("reasoningEffort").is_none());

        // Non-reasoning models must not receive a parameter their API may
        // reject.
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "sambanova",
            "meta-llama/llama-3.3-70b-instruct",
            Some(EffortLevel::High),
            None,
            None,
        );
        assert!(o.is_empty());
    }

    #[test]
    fn shape_zai_and_poolside_toggles() {
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "zai",
            "glm-4.7-flash",
            Some(EffortLevel::High),
            None,
            None,
        );
        assert_eq!(o["thinking"]["type"], serde_json::json!("enabled"));
        assert_eq!(o["thinking"]["clear_thinking"], serde_json::json!(false));

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "zai",
            "glm-4.7-flash",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(o["thinking"]["type"], serde_json::json!("disabled"));

        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "poolside",
            "poolside/laguna-s-2.1",
            Some(EffortLevel::None),
            None,
            None,
        );
        assert_eq!(
            o["chat_template_kwargs"]["enable_thinking"],
            serde_json::json!(false)
        );

        // Non-None effort leaves poolside on its default (thinking on).
        let mut o = serde_json::Map::new();
        shape_provider_thinking(
            &mut o,
            "poolside",
            "poolside/laguna-s-2.1",
            Some(EffortLevel::Medium),
            None,
            None,
        );
        assert!(o.is_empty());
    }

    // -------------------------------------------------------------------
    // inspect_thinking (the thinking inspector row)
    // -------------------------------------------------------------------

    fn poolside_upstream() -> &'static crate::providers::free::FreeUpstream {
        crate::providers::free::catalog_entry("poolside").expect("poolside in catalog")
    }

    #[test]
    fn inspect_google_25_budget_clamped_and_warned() {
        let insp = inspect_thinking(
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::High),
            None,
            Some(1_000),
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Enabled);
        assert_eq!(insp.control, Some(ThinkingControl::Budget));
        assert_eq!(
            insp.effective_budget,
            Some(999),
            "clamped to max_tokens − 1"
        );
        assert_eq!(insp.wire_param.as_deref(), Some("thinkingBudget: 999"));
        assert!(insp
            .warnings
            .iter()
            .any(|w| w.contains("clamped below max_tokens")));
    }

    #[test]
    fn inspect_google_25_none_disables() {
        let insp = inspect_thinking(
            "google",
            "gemini-2.5-flash",
            Some(EffortLevel::None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Disabled);
        assert_eq!(insp.wire_param.as_deref(), Some("thinkingConfig: off"));
    }

    #[test]
    fn inspect_google_3_behavioral_level() {
        let insp = inspect_thinking(
            "google",
            "gemini-3-pro-preview",
            Some(EffortLevel::Medium),
            None,
            None,
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Enabled);
        assert_eq!(insp.control, Some(ThinkingControl::Behavioral));
        assert_eq!(insp.wire_param.as_deref(), Some("thinkingLevel: medium"));
        assert_eq!(
            insp.effective_budget, None,
            "no hard budget for a level control"
        );
    }

    #[test]
    fn inspect_zai_toggle_on_off() {
        let on = inspect_thinking(
            "zai",
            "glm-4.7",
            Some(EffortLevel::High),
            None,
            None,
            None,
            None,
        );
        assert_eq!(on.mode, ThinkingMode::Enabled);
        assert_eq!(on.control, Some(ThinkingControl::Toggle));
        assert_eq!(on.wire_param.as_deref(), Some("thinking.type: enabled"));

        let off = inspect_thinking(
            "zai",
            "glm-4.7",
            Some(EffortLevel::None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(off.mode, ThinkingMode::Disabled);
        assert_eq!(off.wire_param.as_deref(), Some("thinking.type: disabled"));
    }

    #[test]
    fn inspect_poolside_default_on_when_effort_not_none() {
        // Medium effort writes nothing for poolside (thinking on by default) —
        // the inspector must say "enabled (default)", not "no knob".
        let insp = inspect_thinking(
            "poolside",
            "poolside/laguna-3",
            Some(EffortLevel::Medium),
            None,
            Some(20_000),
            Some(poolside_upstream()),
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Enabled);
        assert_eq!(insp.control, Some(ThinkingControl::Toggle));
        assert_eq!(
            insp.wire_param.as_deref(),
            Some("chat_template_kwargs.enable_thinking: true (provider default)")
        );
        assert_eq!(insp.max_tokens_cap, Some(8_192));
        assert_eq!(insp.context_window, Some(262_144));
        assert!(insp.tool_calling);
        assert!(!insp.vision);
        // 20K request against the 8K cap is flagged.
        assert!(insp
            .warnings
            .iter()
            .any(|w| w.contains("clamped to upstream cap 8192")));
    }

    #[test]
    fn inspect_deepseek_on_openaiish_provider_combines_toggle_and_effort() {
        let insp = inspect_thinking(
            "groq",
            "deepseek-chat",
            Some(EffortLevel::XHigh),
            None,
            None,
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Enabled);
        assert_eq!(insp.control, Some(ThinkingControl::Toggle));
        assert_eq!(
            insp.wire_param.as_deref(),
            Some("thinking.type: enabled · reasoningEffort: max")
        );
    }

    #[test]
    fn inspect_plain_chat_model_is_not_supported() {
        let insp = inspect_thinking(
            "groq",
            "llama-3.3-70b-versatile",
            Some(EffortLevel::High),
            None,
            None,
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::NotSupported);
        assert_eq!(insp.control, None);
        assert_eq!(insp.wire_param, None);
    }

    #[test]
    fn inspect_openai_effort_none_is_minimum_not_off() {
        let insp = inspect_thinking(
            "groq",
            "gpt-5.2",
            Some(EffortLevel::None),
            None,
            None,
            None,
            None,
        );
        assert_eq!(insp.mode, ThinkingMode::Enabled);
        assert_eq!(insp.control, Some(ThinkingControl::Behavioral));
        assert_eq!(insp.wire_param.as_deref(), Some("reasoningEffort: none"));
        assert!(insp
            .warnings
            .iter()
            .any(|w| w.contains("minimum thinking tier, not off")));
    }

    #[test]
    fn inspect_low_effort_flags_ladder_quirk() {
        let insp = inspect_thinking(
            "groq",
            "gpt-5.2",
            Some(EffortLevel::Low),
            None,
            None,
            None,
            None,
        );
        assert!(insp
            .warnings
            .iter()
            .any(|w| w.contains("Low disables thinking; Minimal enables")));
        assert!(insp.warnings.iter().any(|w| w.contains("temperature 0.0")));
    }

    #[test]
    fn inspect_last_response_flags_budget_eaten_and_truncation() {
        let route = FreeLastRoute {
            upstream_id: "poolside".to_string(),
            model: "poolside/laguna-3".to_string(),
            usage: clawde_core::types::UsageInfo {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                reasoning_tokens: 950,
            },
            stop_reason: Some("MaxTokens".to_string()),
        };
        let insp = inspect_thinking(
            "poolside",
            "poolside/laguna-3",
            Some(EffortLevel::Medium),
            None,
            Some(1_000),
            Some(poolside_upstream()),
            Some(&route),
        );
        let last = insp.last_response.expect("route provided");
        assert_eq!(last.reasoning_tokens, 950);
        assert_eq!(last.completion_tokens, 0);
        assert_eq!(last.stop_reason.as_deref(), Some("MaxTokens"));
        assert!(last.flags.iter().any(|f| f.contains("budget eaten")));
        assert!(last.flags.iter().any(|f| f.contains("truncated")));
    }

    #[test]
    fn inspect_last_response_thinking_heavy_with_output_present() {
        let route = FreeLastRoute {
            upstream_id: "zai".to_string(),
            model: "glm-4.7".to_string(),
            usage: clawde_core::types::UsageInfo {
                input_tokens: 100,
                output_tokens: 300,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                reasoning_tokens: 600,
            },
            stop_reason: Some("EndTurn".to_string()),
        };
        let insp = inspect_thinking(
            "zai",
            "glm-4.7",
            Some(EffortLevel::High),
            None,
            Some(1_000),
            None,
            Some(&route),
        );
        let last = insp.last_response.expect("route provided");
        assert!(last.flags.iter().any(|f| f.contains("thinking-heavy")));
        assert!(!last.flags.iter().any(|f| f.contains("budget eaten")));
        assert!(!last.flags.iter().any(|f| f.contains("truncated")));
    }
}
