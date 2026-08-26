//! Mode presets — named bundles of config knobs + decision-rule posture.
//!
//! A mode (a.k.a. preset) binds a set of *already-typed* `Config` knobs
//! (model, effort, permission mode, output style, allowed tools) plus three
//! mode-specific decision-rule knobs (plan posture, ask-on-ambiguity,
//! check-in cadence) into one named, switchable profile (spec
//! `docs/plans/clawde-modes-ux-spec.md` §7.1).
//!
//! Design rules:
//! - **Typed fields only** — no `serde_json::Value` maps (repo rule: no type
//!   erasure at typed boundaries).
//! - **One source of definitions** — built-ins in code + user-defined
//!   `.json` files in `<config_dir>/modes/`, mirroring the output-styles
//!   pattern. No parallel settings-embedded `"modes": {}` map.
//! - **Engine untouched** — applying a mode mutates `Config` knobs and
//!   injects prompt guidance; it never rewrites the orchestrator loop.
//!   The cadence/ask knobs are *layered*: they synthesize a system-prompt
//!   block (see [`mode_prompt_block`]) and rely on the existing
//!   `AskUserQuestion` tool — no new loop pause primitives (spec §7.2/§7.3).
//! - **Safety floor** — `apply_mode` never sets
//!   `PermissionMode::BypassPermissions` (spec §8: presets must not silently
//!   enable YOLO mode).

use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Decision-rule knobs
// ---------------------------------------------------------------------------

/// Plan-vs-execute posture (audit correction: binds the mechanisms that are
/// actually wired — `spec_mode`, `permission_mode` — NOT `decide.rs`, which
/// has no production callers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlanKnobs {
    /// Current behavior: no spec gate unless the user enables it.
    #[default]
    Default,
    /// Enforce the spec-mode write gate (`Config::spec_mode` = true — the
    /// equivalent of `/spec` on): file mutators are blocked until an
    /// approved `/spec` exists for the task.
    SpecMode,
    /// `permission_mode` = Plan: reads allowed, writes gated.
    AlwaysPlan,
}

/// How aggressively the model should pause and ask via `AskUserQuestion`
/// when a decision is ambiguous (spec §7.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AskAmbiguityMode {
    /// No mode-injected ask guidance. The base system prompt's existing
    /// "hard to reverse, ask one clarifying question" guidance still applies.
    #[default]
    Off,
    /// Ask when requirements conflict, the change is wide-reaching or not
    /// cleanly reversible, or multiple designs have materially different
    /// tradeoffs. Never for mechanical/trivial choices.
    Balanced,
    /// Like `Balanced`, but explicitly proactive about checking in on
    /// design decisions.
    AskOnDesign,
}

/// How often the model narrates and checks in (spec §7.2). Layered via
/// prompt instruction + gating the existing `AskUserQuestion` tool — the
/// loop is not paused by code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckinCadence {
    /// Current behavior: no mode-injected narration or milestone pauses.
    #[default]
    Rare,
    /// One-paragraph "here's what I'll do" narration before the first write
    /// and at tool-round boundaries; pause via `AskUserQuestion` at
    /// milestones (before first write, after several tool rounds, before
    /// wide refactors).
    Milestone,
    /// Narrate and ask before every action.
    EveryTurn,
}

// ---------------------------------------------------------------------------
// Mode definition
// ---------------------------------------------------------------------------

/// A named mode preset.
///
/// `Option`-typed `Config` knobs are `None` when the preset leaves that
/// setting untouched. The three decision-rule knobs always have a value
/// (they default to the current behavior).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModeDef {
    /// Machine-readable identifier (e.g. `"careful"`).
    #[serde(default)]
    pub name: String,
    /// Human-readable label shown in pickers (e.g. `"Careful"`).
    #[serde(default)]
    pub label: String,
    /// One-line description.
    #[serde(default)]
    pub description: String,
    /// Bind `Config::model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Bind `Config::default_effort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::effort::EffortLevel>,
    /// Bind `Config::permission_mode`. `BypassPermissions` is refused by
    /// [`apply_mode`] (safety floor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<crate::PermissionMode>,
    /// Bind `Config::output_style`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_style: Option<String>,
    /// Bind `Config::allowed_tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<Vec<String>>,
    #[serde(default)]
    pub plan: PlanKnobs,
    #[serde(default)]
    pub ask_on_ambiguity: AskAmbiguityMode,
    #[serde(default)]
    pub checkin_cadence: CheckinCadence,
}

impl ModeDef {
    // ---- Built-in presets -------------------------------------------------

    /// Current behavior — no knob binds, no injected guidance.
    pub fn builtin_default() -> Self {
        Self {
            name: "default".to_string(),
            label: "Default".to_string(),
            description: "Current Clawde behavior — no preset knobs or guidance.".to_string(),
            model: None,
            effort: None,
            permission_mode: None,
            output_style: None,
            allowed_tools: None,
            plan: PlanKnobs::Default,
            ask_on_ambiguity: AskAmbiguityMode::Off,
            checkin_cadence: CheckinCadence::Rare,
        }
    }

    /// The D5 starter preset (spec D-table): planner-flavored "careful"
    /// behavior — plan posture, ask on design decisions, milestone
    /// check-ins. Matches the user's stated preferences from the interview:
    /// "check-ins until the agent proves itself" + "ask on design decisions,
    /// autonomous on mechanical edits".
    pub fn builtin_careful() -> Self {
        Self {
            name: "careful".to_string(),
            label: "Careful".to_string(),
            description: "Plan before writing, ask on design decisions, check in at milestones."
                .to_string(),
            model: None,
            effort: None,
            permission_mode: None,
            output_style: None,
            allowed_tools: None,
            plan: PlanKnobs::AlwaysPlan,
            ask_on_ambiguity: AskAmbiguityMode::AskOnDesign,
            checkin_cadence: CheckinCadence::Milestone,
        }
    }

    /// A fast lane: low effort, no ask guidance, no extra narration.
    pub fn builtin_fast() -> Self {
        Self {
            name: "fast".to_string(),
            label: "Fast".to_string(),
            description: "Low reasoning effort, minimal check-ins, no extra asks.".to_string(),
            model: None,
            effort: Some(crate::effort::EffortLevel::Low),
            permission_mode: None,
            output_style: None,
            allowed_tools: None,
            plan: PlanKnobs::Default,
            ask_on_ambiguity: AskAmbiguityMode::Off,
            checkin_cadence: CheckinCadence::Rare,
        }
    }
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// Return all built-in modes in display order.
pub fn builtin_modes() -> Vec<ModeDef> {
    vec![
        ModeDef::builtin_default(),
        ModeDef::builtin_careful(),
        ModeDef::builtin_fast(),
    ]
}

// ---------------------------------------------------------------------------
// Loading from disk
// ---------------------------------------------------------------------------

/// Load user-defined modes from a directory (mirrors `output_styles`):
/// `.json` files with the same `ModeDef` shape. Files that cannot be parsed
/// are silently skipped; the file stem becomes the name when `name` is empty.
pub fn load_modes_dir(modes_dir: &Path) -> Vec<ModeDef> {
    if !modes_dir.exists() {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(modes_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut modes = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(content) = std::fs::read_to_string(&path).ok() else {
            continue;
        };
        let Ok(mut def) = serde_json::from_str::<ModeDef>(&content) else {
            continue;
        };
        if def.name.is_empty() {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                def.name = stem.to_string();
            } else {
                continue;
            }
        }
        modes.push(def);
    }
    // Deterministic order; built-ins keep their display order in `all_modes`.
    modes.sort_by(|a, b| a.name.cmp(&b.name));
    modes
}

/// Return all modes available for `config_dir`: built-ins first, then
/// user-defined modes from `<config_dir>/modes/`. User-defined modes win over
/// built-ins with the same name (they override the built-in).
pub fn all_modes(config_dir: &Path) -> Vec<ModeDef> {
    merge_modes(builtin_modes(), load_modes_dir(&config_dir.join("modes")))
}

/// Return modes from the built-ins, global mode directory, and the nearest
/// project mode directory. Project definitions win over global definitions,
/// which win over built-ins. This is the normal session-facing resolver.
pub fn all_modes_for_project(global_dir: &Path, project_dir: &Path) -> Vec<ModeDef> {
    let modes = merge_modes(builtin_modes(), load_modes_dir(&global_dir.join("modes")));
    merge_modes(
        modes,
        load_modes_dir(&project_dir.join(".clawde").join("modes")),
    )
}

fn merge_modes(mut base: Vec<ModeDef>, overrides: Vec<ModeDef>) -> Vec<ModeDef> {
    for mode in overrides {
        if let Some(existing) = base.iter_mut().find(|m| m.name == mode.name) {
            *existing = mode;
        } else {
            base.push(mode);
        }
    }
    base
}

/// Find a mode by its `name` field.
pub fn find_mode<'a>(modes: &'a [ModeDef], name: &str) -> Option<&'a ModeDef> {
    modes.iter().find(|m| m.name == name)
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

/// Bind a mode's knobs onto `config`. Only fields the mode sets are touched;
/// `BypassPermissions` is deliberately refused (spec §8 — a preset must
/// never silently enable YOLO mode; use `--dangerously-skip-permissions`
/// explicitly instead).
pub fn apply_mode(config: &mut crate::Config, mode: &ModeDef) {
    if let Some(model) = &mode.model {
        config.model = Some(model.clone());
    }
    if let Some(effort) = mode.effort {
        config.default_effort = Some(effort);
    }
    if let Some(pm) = &mode.permission_mode {
        if *pm != crate::PermissionMode::BypassPermissions {
            config.permission_mode = pm.clone();
        }
    }
    if let Some(style) = &mode.output_style {
        config.output_style = Some(style.clone());
    }
    if let Some(tools) = &mode.allowed_tools {
        config.allowed_tools = tools.clone();
    }
    match mode.plan {
        PlanKnobs::Default => {}
        PlanKnobs::SpecMode => config.spec_mode = true,
        PlanKnobs::AlwaysPlan => config.permission_mode = crate::PermissionMode::Plan,
    }
}

// ---------------------------------------------------------------------------
// System-prompt block
// ---------------------------------------------------------------------------

/// Synthesize the system-prompt block for a mode's cadence/ask knobs.
///
/// Returns `None` when the mode adds no guidance (default / fast: Off + Rare)
/// so the injected prompt is a no-op for current behavior. The guidance is
/// model-disciplined (spec §7.2/§7.3 honest limitation): it instructs the
/// model and gates the existing `AskUserQuestion` tool — the loop is not
/// paused by code.
pub fn mode_prompt_block(mode: &ModeDef) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match mode.checkin_cadence {
        CheckinCadence::Rare => {}
        CheckinCadence::Milestone => parts.push(
            "Check-in cadence: before the first file write and at tool-round \
             boundaries, give a short one-paragraph \"here's what I'll do\" \
             narration. At milestones (before the first write, after several \
             tool rounds, before wide refactors), pause and check in with the \
             user via the AskUserQuestion tool before continuing."
                .to_string(),
        ),
        CheckinCadence::EveryTurn => parts.push(
            "Check-in cadence: narrate your plan and ask the user before every \
             action."
                .to_string(),
        ),
    }
    match mode.ask_on_ambiguity {
        AskAmbiguityMode::Off => {}
        AskAmbiguityMode::Balanced => parts.push(
            "Asking on ambiguity: when requirements conflict or are \
             underspecified, the change is wide-reaching or not cleanly \
             reversible, or multiple plausible designs exist with materially \
             different tradeoffs, ask one clarifying question via the \
             AskUserQuestion tool before acting. Do not ask for mechanical or \
             trivial choices."
                .to_string(),
        ),
        AskAmbiguityMode::AskOnDesign => parts.push(
            "Asking on design decisions: when requirements conflict or are \
             underspecified, the change is wide-reaching or not cleanly \
             reversible, or multiple plausible designs exist with materially \
             different tradeoffs, ask one clarifying question via the \
             AskUserQuestion tool before acting. Be proactive about checking \
             in on design choices. Do not ask for mechanical or trivial \
             choices."
                .to_string(),
        ),
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("## Active Mode\n{}", parts.join("\n")))
    }
}

/// Resolve a mode's synthesized prompt block by name.
///
/// Consults `modes` first; falls back to the built-ins when the name is not
/// present (so callers with a disk-mode registry still resolve built-ins,
/// and callers without a registry — tests, sub-agents — resolve built-ins).
pub fn resolve_mode_block(modes: &[ModeDef], name: &str) -> Option<String> {
    if let Some(mode) = find_mode(modes, name) {
        return mode_prompt_block(mode);
    }
    builtin_modes()
        .into_iter()
        .find(|m| m.name == name)
        .and_then(|m| mode_prompt_block(&m))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_modes_have_unique_names() {
        let modes = builtin_modes();
        let mut seen = std::collections::HashSet::new();
        for m in &modes {
            assert!(seen.insert(&m.name), "duplicate mode name: {}", m.name);
        }
        // default first, then the D5 starter, then fast.
        assert_eq!(modes[0].name, "default");
        assert_eq!(modes[1].name, "careful");
        assert_eq!(modes[2].name, "fast");
    }

    #[test]
    fn default_mode_binds_nothing_and_injects_nothing() {
        let mut cfg = crate::Config {
            default_effort: Some(crate::effort::EffortLevel::High),
            ..crate::Config::default()
        };
        let default = ModeDef::builtin_default();
        apply_mode(&mut cfg, &default);
        assert_eq!(
            cfg.default_effort,
            Some(crate::effort::EffortLevel::High),
            "default mode must not clobber existing settings"
        );
        assert!(mode_prompt_block(&default).is_none());
    }

    #[test]
    fn careful_mode_binds_plan_posture_and_injects_guidance() {
        let mut cfg = crate::Config::default();
        let careful = ModeDef::builtin_careful();
        apply_mode(&mut cfg, &careful);
        assert_eq!(cfg.permission_mode, crate::PermissionMode::Plan);
        let block = mode_prompt_block(&careful).unwrap();
        assert!(block.contains("Check-in cadence"));
        assert!(block.contains("AskUserQuestion"));
        assert!(block.contains("## Active Mode"));
    }

    #[test]
    fn fast_mode_binds_effort_and_injects_nothing() {
        let mut cfg = crate::Config::default();
        let fast = ModeDef::builtin_fast();
        apply_mode(&mut cfg, &fast);
        assert_eq!(cfg.default_effort, Some(crate::effort::EffortLevel::Low));
        assert!(mode_prompt_block(&fast).is_none());
    }

    #[test]
    fn spec_mode_knob_enables_spec_gate() {
        let mut cfg = crate::Config::default();
        let mut mode = ModeDef::builtin_default();
        mode.plan = PlanKnobs::SpecMode;
        apply_mode(&mut cfg, &mode);
        assert!(cfg.spec_mode);
    }

    #[test]
    fn apply_mode_refuses_bypass_permissions() {
        let mut cfg = crate::Config::default();
        let mut mode = ModeDef::builtin_default();
        mode.permission_mode = Some(crate::PermissionMode::BypassPermissions);
        apply_mode(&mut cfg, &mode);
        assert_eq!(cfg.permission_mode, crate::PermissionMode::Default);
    }

    #[test]
    fn apply_mode_binds_typed_knobs() {
        let mut cfg = crate::Config::default();
        let mut mode = ModeDef::builtin_default();
        mode.model = Some("free/auto".to_string());
        mode.effort = Some(crate::effort::EffortLevel::XHigh);
        mode.output_style = Some("concise".to_string());
        mode.allowed_tools = Some(vec!["read".to_string()]);
        mode.permission_mode = Some(crate::PermissionMode::AcceptEdits);
        apply_mode(&mut cfg, &mode);
        assert_eq!(cfg.model, Some("free/auto".to_string()));
        assert_eq!(cfg.default_effort, Some(crate::effort::EffortLevel::XHigh));
        assert_eq!(cfg.output_style, Some("concise".to_string()));
        assert_eq!(cfg.allowed_tools, vec!["read".to_string()]);
        assert_eq!(cfg.permission_mode, crate::PermissionMode::AcceptEdits);
    }

    #[test]
    fn every_turn_cadence_injects_narration() {
        let mut mode = ModeDef::builtin_default();
        mode.checkin_cadence = CheckinCadence::EveryTurn;
        let block = mode_prompt_block(&mode).unwrap();
        assert!(block.contains("before every action"));
    }

    #[test]
    fn balanced_ask_injects_guidance_but_off_does_not() {
        let mut mode = ModeDef::builtin_default();
        mode.ask_on_ambiguity = AskAmbiguityMode::Balanced;
        let block = mode_prompt_block(&mode).unwrap();
        assert!(block.contains("Asking on ambiguity"));
        assert!(block.contains("mechanical or trivial"));
        mode.ask_on_ambiguity = AskAmbiguityMode::Off;
        assert!(mode_prompt_block(&mode).is_none());
    }

    #[test]
    fn load_modes_dir_parses_json_and_skips_bad_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let modes_dir = dir.path().join("modes");
        std::fs::create_dir_all(&modes_dir).unwrap();
        std::fs::write(
            modes_dir.join("stealth.json"),
            r#"{
                "name": "stealth",
                "label": "Stealth",
                "description": "No narration, no asks.",
                "effort": "low",
                "checkinCadence": "rare",
                "askOnAmbiguity": "off"
            }"#,
        )
        .unwrap();
        // Unparseable file is skipped.
        std::fs::write(modes_dir.join("broken.json"), "{ not json").unwrap();
        // A non-mode file is ignored.
        std::fs::write(modes_dir.join("notes.md"), "# notes").unwrap();

        let modes = load_modes_dir(&modes_dir);
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].name, "stealth");
        assert_eq!(modes[0].effort, Some(crate::effort::EffortLevel::Low));
    }

    #[test]
    fn load_modes_dir_uses_stem_as_name_when_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let modes_dir = dir.path().join("modes");
        std::fs::create_dir_all(&modes_dir).unwrap();
        std::fs::write(
            modes_dir.join("my-mode.json"),
            r#"{"label": "My Mode", "description": "d"}"#,
        )
        .unwrap();
        let modes = load_modes_dir(&modes_dir);
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].name, "my-mode");
    }

    #[test]
    fn all_modes_merges_user_modes_and_overrides_builtins() {
        let dir = tempfile::TempDir::new().unwrap();
        let modes_dir = dir.path().join("modes");
        std::fs::create_dir_all(&modes_dir).unwrap();
        // Override the built-in `fast` mode with a custom one.
        std::fs::write(
            modes_dir.join("fast.json"),
            r#"{
                "name": "fast",
                "label": "My Fast",
                "description": "custom override",
                "effort": "high",
                "checkinCadence": "everyTurn"
            }"#,
        )
        .unwrap();
        std::fs::write(
            modes_dir.join("zen.json"),
            r#"{"name": "zen", "label": "Zen", "description": "custom"}"#,
        )
        .unwrap();

        let modes = all_modes(dir.path());
        let fast = find_mode(&modes, "fast").unwrap();
        assert_eq!(fast.effort, Some(crate::effort::EffortLevel::High));
        assert_eq!(fast.checkin_cadence, CheckinCadence::EveryTurn);
        assert!(find_mode(&modes, "zen").is_some());
        assert!(find_mode(&modes, "careful").is_some());
        assert_eq!(modes.len(), 4);
    }

    #[test]
    fn project_modes_override_global_modes() {
        let root = tempfile::TempDir::new().unwrap();
        let project = tempfile::TempDir::new().unwrap();
        let global_modes = root.path().join("modes");
        let project_modes = project.path().join(".clawde").join("modes");
        std::fs::create_dir_all(&global_modes).unwrap();
        std::fs::create_dir_all(&project_modes).unwrap();
        std::fs::write(
            global_modes.join("shared.json"),
            r#"{"name":"shared","label":"Global","description":"global"}"#,
        )
        .unwrap();
        std::fs::write(
            project_modes.join("shared.json"),
            r#"{"name":"shared","label":"Project","description":"project"}"#,
        )
        .unwrap();
        let modes = all_modes_for_project(root.path(), project.path());
        assert_eq!(find_mode(&modes, "shared").unwrap().label, "Project");
    }

    #[test]
    fn resolve_mode_block_falls_back_to_builtins() {
        // An empty registry still resolves built-in names.
        assert!(resolve_mode_block(&[], "careful").is_some());
        assert!(resolve_mode_block(&[], "default").is_none());
        assert!(resolve_mode_block(&[], "nope").is_none());
        // A registry wins over built-ins with the same name.
        let mut custom = ModeDef::builtin_default();
        custom.name = "careful".to_string();
        custom.checkin_cadence = CheckinCadence::Rare;
        custom.ask_on_ambiguity = AskAmbiguityMode::Off;
        let block = resolve_mode_block(&[custom.clone()], "careful");
        assert!(
            block.is_none(),
            "custom careful with Off+Rare injects nothing"
        );
    }

    #[test]
    fn mode_def_roundtrips_through_json() {
        let mode = ModeDef::builtin_careful();
        let json = serde_json::to_string_pretty(&mode).unwrap();
        let back: ModeDef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, mode);
    }

    #[test]
    fn user_mode_can_be_applied() {
        let dir = tempfile::TempDir::new().unwrap();
        let modes_dir = dir.path().join("modes");
        std::fs::create_dir_all(&modes_dir).unwrap();
        std::fs::write(
            modes_dir.join("guard.json"),
            r#"{
                "name": "guard",
                "label": "Guard",
                "description": "spec gate on",
                "plan": "specMode",
                "permissionMode": "acceptEdits"
            }"#,
        )
        .unwrap();
        let modes = all_modes(dir.path());
        let guard = find_mode(&modes, "guard").unwrap();
        let mut cfg = crate::Config::default();
        apply_mode(&mut cfg, guard);
        assert!(cfg.spec_mode);
        assert_eq!(cfg.permission_mode, crate::PermissionMode::AcceptEdits);
    }
}
