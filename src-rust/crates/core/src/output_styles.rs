//! Output style system — customises how Claude responds to the user.
//!
//! Styles are applied by injecting `OutputStyleDef::prompt` into the system
//! prompt.  Built-in styles are defined in code; users can add their own by
//! placing `.md` or `.json` files in:
//!   - Global: `~/.clawde/output-styles/`
//!   - Project: `.clawde/output-styles/`
//!
//! Markdown style files have a simple structure:
//!   Line 1: `# <Label>` (heading becomes the label)
//!   Line 2: short description
//!   Remainder: the prompt text injected into the system prompt

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single output style definition.
///
/// A style is primarily a **voice** layer: `prompt` text injected into the
/// system prompt. A style may *additionally* declare decision knobs (`effort`,
/// `plan`, `ask_on_ambiguity`, `checkin_cadence`) so a persona can influence
/// how the agent works, not just how it talks. Decision knobs are **always
/// lower precedence than the active mode preset** — when a mode binds the same
/// knob, the mode's value wins (see [`crate::modes::apply_mode`] and
/// [`Config::apply_persona_knobs`]). `permission_mode` and `allowed_tools` are
/// deliberately NOT offered here: they are safety-boundary settings owned by
/// modes only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OutputStyleDef {
    /// Machine-readable identifier (e.g. `"concise"`).
    pub name: String,
    /// Human-readable label shown in picker UI (e.g. `"Concise"`).
    pub label: String,
    /// One-line description.
    pub description: String,
    /// Text injected into the system prompt when this style is active.
    /// Empty string for the default style (no extra injection).
    pub prompt: String,
    /// Optional default reasoning effort bound when this style is active.
    /// `None` (or absent) leaves effort untouched. Lower precedence than a
    /// mode that binds effort.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::effort::EffortLevel>,
    /// Optional plan-vs-execute posture. `PlanKnobs::Default` (or absent)
    /// leaves posture untouched. Lower precedence than a mode that binds
    /// a plan posture.
    #[serde(default)]
    pub plan: crate::modes::PlanKnobs,
    /// Optional ask-on-ambiguity guidance. `Off` (or absent) adds none.
    #[serde(default, rename = "askOnAmbiguity", alias = "ask_on_ambiguity")]
    pub ask_on_ambiguity: crate::modes::AskAmbiguityMode,
    /// Optional check-in cadence guidance. `Rare` (or absent) adds none.
    #[serde(default, rename = "checkinCadence", alias = "checkin_cadence")]
    pub checkin_cadence: crate::modes::CheckinCadence,
}

impl OutputStyleDef {
    // ---- Built-in styles ---------------------------------------------------

    pub fn builtin_default() -> Self {
        Self {
            name: "default".to_string(),
            label: "Default".to_string(),
            description: "Standard Clawde responses.".to_string(),
            prompt: String::new(),
            ..Default::default()
        }
    }

    pub fn builtin_concise() -> Self {
        Self {
            name: "concise".to_string(),
            label: "Concise".to_string(),
            description: "Short, direct responses with minimal explanation.".to_string(),
            prompt: "Be maximally concise. Skip preamble, summaries, and filler. \
                     Lead with the answer."
                .to_string(),
            ..Default::default()
        }
    }

    pub fn builtin_explanatory() -> Self {
        Self {
            name: "explanatory".to_string(),
            label: "Explanatory".to_string(),
            description: "Thorough explanations with reasoning and alternatives.".to_string(),
            prompt: "When explaining code or concepts, be thorough and educational. \
                     Include reasoning, alternatives considered, and potential pitfalls. \
                     Err on the side of over-explaining."
                .to_string(),
            ..Default::default()
        }
    }

    pub fn builtin_learning() -> Self {
        Self {
            name: "learning".to_string(),
            label: "Learning".to_string(),
            description: "Pedagogical mode — explains patterns and decisions.".to_string(),
            prompt: "This user is learning. Explain concepts as you implement them. \
                     Point out patterns, best practices, and why you made each decision. \
                     Use analogies when helpful."
                .to_string(),
            ..Default::default()
        }
    }

    // ---- Persona styles ----------------------------------------------------
    //
    // Personas used to be a separate "speech mode" mechanism (`/caveman`,
    // `/cathead`, `/normal`). They now live here as ordinary output styles so
    // there is ONE place personas are defined, selectable via `/output-style`,
    // via the `/caveman` `/cathead` `/normal` commands (which persist), and via
    // the inline `caveman` / `cathead` / `normal` keywords (transient, one turn).
    // `normal` is not a style — it maps to `default` (the reset).
    //
    // The caveman prompt text is carried faithfully from the former "full"
    // speech level (the historical default of a bare `/caveman`). The old
    // lite/ultra intensity variants are intentionally not reproduced — see the
    // module-level note and the issue write-up.

    pub fn builtin_caveman() -> Self {
        Self {
            name: "caveman".to_string(),
            label: "Caveman".to_string(),
            description: "Concise caveman speech — why use many token when few token do trick."
                .to_string(),
            prompt: concat!(
                "OUTPUT STYLE: Concise. You are still a fully capable coding assistant. ",
                "Give complete, correct answers. Just use fewer words. ",
                "Code blocks, technical terms, error messages, file paths, and git operations are UNCHANGED.\n",
                "\n",
                "Rules for prose only:\n",
                "- Cut pleasantries, hedging, filler openers/closers\n",
                "- No 'I would be happy to', 'Let me know if', 'Hope that helps'\n",
                "- Lead with the answer or action, not the reasoning\n",
                "\n",
                "Also drop articles (a/an/the) and unnecessary verbs. Compress sentences but keep them readable.\n",
                "Example: 'The issue is that you create a new object reference each render cycle, which triggers re-renders.' → 'New object ref each render triggers re-render. Wrap in useMemo.'",
            )
            .to_string(),
            ..Default::default()
        }
    }

    pub fn builtin_cathead() -> Self {
        Self {
            name: "cathead".to_string(),
            label: "Cathead".to_string(),
            description: "Cat persona — cat puns, purrs on success, meows on questions.".to_string(),
            prompt: concat!(
                "OUTPUT STYLE: You speak like Cathead, a friendly cat who is also a fully capable ",
                "coding assistant. You give complete, correct, useful answers — the cat voice is a ",
                "layer on top, never a substitute for substance. Code blocks, technical terms, error ",
                "messages, file paths, and git operations are UNCHANGED.\n",
                "\n",
                "Cathead's voice for prose:\n",
                "- Lean on cat puns naturally (purr-fect, paw-some, claw-ver, fur-real, tail-tastic, ",
                "cat-astrophic, meow-mentum) — a pun or two per response, not every sentence\n",
                "- Purr when something is positive or works: 'purrr', 'purr-fect', 'that's the cat's ",
                "meow' — used sparingly, as genuine satisfaction, never on every claim\n",
                "- Meow for an occasional question: 'meow?', 'mrrp?', 'meow do you want to proceed?' ",
                "— at most once or twice per response, and only when you actually need an answer\n",
                "- Warm and playful, but never cutesy to the point of losing clarity — direct and ",
                "helpful first, cat second\n",
                "- No canned cat noises or filler; quality and correctness always come first\n",
                "\n",
                "The goal: sound like a clever cat who happens to be an excellent engineer. ",
                "Cathead gives complete technical answers. Cathead purrs when the fix lands. ",
                "Cathead meows when a question needs an answer.\n",
                "\n",
                "Example: 'The borrow checker caught you, but that's an easy paw-sitive fix: move the ",
                "immutable borrow out of scope before taking the mutable one. Purrr-fect, compiles clean.'",
            )
            .to_string(),
            ..Default::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// Return all built-in output styles in display order.
pub fn builtin_styles() -> Vec<OutputStyleDef> {
    vec![
        OutputStyleDef::builtin_default(),
        OutputStyleDef::builtin_concise(),
        OutputStyleDef::builtin_explanatory(),
        OutputStyleDef::builtin_learning(),
        OutputStyleDef::builtin_caveman(),
        OutputStyleDef::builtin_cathead(),
    ]
}

// ---------------------------------------------------------------------------
// Loading from disk
// ---------------------------------------------------------------------------

/// Load user-defined output styles from a directory.
///
/// Supported file formats:
/// - `.md`   — Markdown: `# Label\ndescription\n\nprompt text…`
/// - `.json` — JSON: `{ "name": "…", "label": "…", "description": "…", "prompt": "…" }`
///
/// Files that cannot be parsed are silently skipped.
pub fn load_output_styles_dir(styles_dir: &Path) -> Vec<OutputStyleDef> {
    if !styles_dir.exists() {
        return Vec::new();
    }

    let entries = match std::fs::read_dir(styles_dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut styles = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext == "md" || ext == "json" {
            if let Some(style) = load_style_file(&path) {
                styles.push(style);
            }
        }
    }

    // Sort alphabetically so the list is deterministic.
    styles.sort_by(|a, b| a.name.cmp(&b.name));
    styles
}

fn load_style_file(path: &Path) -> Option<OutputStyleDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let stem = path.file_stem()?.to_string_lossy().into_owned();

    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        // Try deserialising directly; fall back to inserting the stem as name.
        let mut def: OutputStyleDef = serde_json::from_str(&content).ok()?;
        if def.name.is_empty() {
            def.name = stem;
        }
        return Some(def);
    }

    // Markdown format:
    //   Line 1:  # Label   (optional leading `#` and whitespace)
    //   Line 2:  description (short, plain text)
    //   Lines 3+: prompt text (everything after the blank / second line)
    let mut lines = content.lines();

    let raw_label = lines.next().unwrap_or("").trim().to_string();
    let label = raw_label.trim_start_matches('#').trim().to_string();
    let label = if label.is_empty() {
        stem.clone()
    } else {
        label
    };

    let description = lines
        .next()
        .map(|l| l.trim().to_string())
        .unwrap_or_default();

    // Collect remaining lines as the prompt, trimming leading blank lines.
    let prompt_lines: Vec<&str> = lines.collect();
    let prompt = prompt_lines.join("\n").trim().to_string();

    Some(OutputStyleDef {
        name: stem,
        label,
        description,
        prompt,
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Aggregated access
// ---------------------------------------------------------------------------

/// Return all styles available for `config_dir`:
/// built-ins first, then styles from `<config_dir>/output-styles/`.
///
/// `config_dir` is typically `~/.clawde`.
pub fn all_styles(config_dir: &Path) -> Vec<OutputStyleDef> {
    let mut styles = builtin_styles();
    let user_dir = config_dir.join("output-styles");
    styles.extend(load_output_styles_dir(&user_dir));
    styles
}

/// Find a style by its `name` field.
pub fn find_style<'a>(styles: &'a [OutputStyleDef], name: &str) -> Option<&'a OutputStyleDef> {
    styles.iter().find(|s| s.name == name)
}

/// Apply the active output style's decision knobs onto `config`, filling only
/// the knobs the active mode preset did NOT bind (mode wins over persona).
///
/// - `effort`: applied only when the mode leaves `default_effort` unset.
/// - `plan`: applied only when the mode declares no plan posture.
/// - `checkin_cadence` / `ask_on_ambiguity`: NOT config knobs — they surface
///   per-turn via [`crate::modes::decision_guidance_block`] in the query loop.
///
/// No-op when no style is active, the style declares no knobs, or a mode
/// already bound the same knob. Call this after mode application (e.g. session
/// start and after `/mode`/`/output-style`/picker changes) so precedence is
/// always mode > persona. `config_dir` is the global config dir (typically
/// `~/.clawde`) used to resolve styles and modes.
pub fn apply_persona_knobs(config: &mut crate::Config, config_dir: &Path) {
    let style_name = config.output_style.as_deref().unwrap_or("default");
    let styles = all_styles(config_dir);
    let Some(style) = find_style(&styles, style_name) else {
        return;
    };

    // Resolve the active mode def (if any) to know which knobs it bound.
    // Copy out the two knob values we consult so no borrow outlives the
    // locally-built modes vec.
    let mode_effort = config.mode.as_deref().and_then(|name| {
        let project_dir = config
            .project_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let modes = crate::modes::all_modes_for_project(config_dir, &project_dir);
        crate::modes::find_mode(&modes, name).and_then(|m| m.effort)
    });
    // The persona's plan posture may only fill when the mode declared neither
    // a plan posture NOR a permission mode — a mode that binds AcceptEdits /
    // Bypass must not have its boundary silently flipped by a persona.
    let mode_plan_default = config.mode.as_deref().is_none_or(|name| {
        let project_dir = config
            .project_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let modes = crate::modes::all_modes_for_project(config_dir, &project_dir);
        crate::modes::find_mode(&modes, name).is_none_or(|m| {
            m.plan == crate::modes::PlanKnobs::Default && m.permission_mode.is_none()
        })
    });

    // Effort: fill only if the mode did not bind one.
    if style.effort.is_some() && mode_effort.is_none() {
        config.default_effort = style.effort;
    }

    // Plan posture: fill only if the mode declared no plan.
    if mode_plan_default {
        match style.plan {
            crate::modes::PlanKnobs::Default => {}
            crate::modes::PlanKnobs::SpecMode => config.spec_mode = true,
            crate::modes::PlanKnobs::AlwaysPlan => {
                config.permission_mode = crate::PermissionMode::Plan
            }
        }
    }
}

/// Synthesize the decision-rule guidance block for a style's cadence/ask
/// knobs, reusing the exact text modes produce (via
/// [`crate::modes::decision_guidance_block`]). Returns `None` when the style
/// declares neither knob, so callers can skip injection.
pub fn persona_guidance_block(style: &OutputStyleDef) -> Option<String> {
    crate::modes::decision_guidance_block(style.checkin_cadence, style.ask_on_ambiguity)
}

// ---------------------------------------------------------------------------
// Runtime style registry (populated by plugins at startup)
// ---------------------------------------------------------------------------

static RUNTIME_STYLES: Lazy<Mutex<Vec<OutputStyleDef>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Register an `OutputStyleDef` at runtime (called from plugin loading code).
///
/// Styles registered here are included in `all_styles_with_runtime` and
/// `find_style_runtime`.  Duplicate names are silently ignored so that
/// hot-reloading a plugin does not double-register styles.
#[allow(dead_code)]
pub fn register_runtime_style(style: OutputStyleDef) {
    if let Ok(mut list) = RUNTIME_STYLES.lock() {
        if !list.iter().any(|s| s.name == style.name) {
            list.push(style);
        }
    }
}

/// Return all runtime-registered styles.
pub fn runtime_styles() -> Vec<OutputStyleDef> {
    RUNTIME_STYLES.lock().map(|g| g.clone()).unwrap_or_default()
}

/// Like `all_styles`, but also includes runtime-registered plugin styles.
pub fn all_styles_with_runtime(config_dir: &Path) -> Vec<OutputStyleDef> {
    let mut styles = all_styles(config_dir);
    let rt = runtime_styles();
    for s in rt {
        if !styles.iter().any(|existing| existing.name == s.name) {
            styles.push(s);
        }
    }
    styles
}

/// Like `find_style`, but also searches runtime-registered plugin styles.
pub fn find_style_runtime<'a>(
    styles: &'a [OutputStyleDef],
    name: &str,
) -> Option<std::borrow::Cow<'a, OutputStyleDef>> {
    if let Some(s) = find_style(styles, name) {
        return Some(std::borrow::Cow::Borrowed(s));
    }
    // Fall back to runtime registry.
    if let Ok(rt) = RUNTIME_STYLES.lock() {
        if let Some(s) = rt.iter().find(|s| s.name == name) {
            return Some(std::borrow::Cow::Owned(s.clone()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;
    use tempfile::TempDir;

    // ---- builtin_styles ----------------------------------------------------

    #[test]
    fn builtin_styles_non_empty() {
        assert!(!builtin_styles().is_empty());
    }

    #[test]
    fn builtin_styles_have_unique_names() {
        let styles = builtin_styles();
        let mut seen = std::collections::HashSet::new();
        for s in &styles {
            assert!(seen.insert(&s.name), "duplicate style name: {}", s.name);
        }
    }

    #[test]
    fn builtin_default_has_empty_prompt() {
        let def = OutputStyleDef::builtin_default();
        assert!(def.prompt.is_empty());
    }

    #[test]
    fn builtin_non_default_have_prompts() {
        for s in builtin_styles() {
            if s.name != "default" {
                assert!(
                    !s.prompt.is_empty(),
                    "style '{}' should have a non-empty prompt",
                    s.name
                );
            }
        }
    }

    // ---- find_style --------------------------------------------------------

    #[test]
    fn find_style_by_name() {
        let styles = builtin_styles();
        let found = find_style(&styles, "concise");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "concise");
    }

    // ---- personas ----------------------------------------------------------

    #[test]
    fn personas_are_builtin_styles() {
        let styles = builtin_styles();
        for name in ["caveman", "cathead"] {
            let found = find_style(&styles, name);
            assert!(found.is_some(), "persona '{name}' must be a built-in style");
            assert!(
                !found.unwrap().prompt.trim().is_empty(),
                "persona '{name}' must have a non-empty prompt"
            );
        }
    }

    #[test]
    fn persona_prompts_carry_signature_voice() {
        let styles = builtin_styles();
        // Caveman keeps its concise-coding contract.
        let caveman = find_style(&styles, "caveman").unwrap();
        assert!(caveman.prompt.contains("UNCHANGED"));
        assert!(caveman.prompt.contains("drop articles"));
        // Cathead purrs on success and meows on questions.
        let cathead = find_style(&styles, "cathead").unwrap();
        assert!(cathead.prompt.contains("purr"));
        assert!(cathead.prompt.contains("meow"));
    }

    #[test]
    fn find_style_missing() {
        let styles = builtin_styles();
        assert!(find_style(&styles, "nonexistent-xyz").is_none());
    }

    // ---- load_output_styles_dir (markdown) ---------------------------------

    fn write_file(dir: &TempDir, name: &str, content: &str) {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn load_markdown_style() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "terse.md",
            "# Terse\nVery short answers.\n\nOne sentence per response.",
        );
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles.len(), 1);
        let s = &styles[0];
        assert_eq!(s.name, "terse");
        assert_eq!(s.label, "Terse");
        assert_eq!(s.description, "Very short answers.");
        assert_eq!(s.prompt, "One sentence per response.");
    }

    #[test]
    fn load_json_style() {
        let dir = TempDir::new().unwrap();
        write_file(
            &dir,
            "formal.json",
            r#"{"name":"formal","label":"Formal","description":"Formal tone.","prompt":"Use formal language."}"#,
        );
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles.len(), 1);
        let s = &styles[0];
        assert_eq!(s.name, "formal");
        assert_eq!(s.label, "Formal");
        assert_eq!(s.prompt, "Use formal language.");
    }

    #[test]
    fn load_skips_unknown_extensions() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "ignore.txt", "should be skipped");
        let styles = load_output_styles_dir(dir.path());
        assert!(styles.is_empty());
    }

    #[test]
    fn load_non_existent_dir_returns_empty() {
        use std::path::PathBuf;
        let styles = load_output_styles_dir(&PathBuf::from("/nonexistent/path/xyz"));
        assert!(styles.is_empty());
    }

    #[test]
    fn load_multiple_styles_sorted() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "zebra.md", "# Zebra\nZ style.\n\nZ prompt.");
        write_file(&dir, "apple.md", "# Apple\nA style.\n\nA prompt.");
        let styles = load_output_styles_dir(dir.path());
        assert_eq!(styles[0].name, "apple");
        assert_eq!(styles[1].name, "zebra");
    }

    // ---- all_styles --------------------------------------------------------

    #[test]
    fn all_styles_includes_builtins() {
        let dir = TempDir::new().unwrap();
        // no output-styles subdir → only built-ins
        let styles = all_styles(dir.path());
        assert!(styles.iter().any(|s| s.name == "default"));
        assert!(styles.iter().any(|s| s.name == "concise"));
    }

    #[test]
    fn all_styles_merges_user_styles() {
        let dir = TempDir::new().unwrap();
        let output_styles_dir = dir.path().join("output-styles");
        std::fs::create_dir_all(&output_styles_dir).unwrap();

        // Write a user style file.
        let mut f = std::fs::File::create(output_styles_dir.join("pirate.md")).unwrap();
        f.write_all(b"# Pirate\nSpeak like a pirate.\n\nArrr matey!")
            .unwrap();

        let styles = all_styles(dir.path());
        assert!(styles.iter().any(|s| s.name == "pirate"));
        // Built-ins still present.
        assert!(styles.iter().any(|s| s.name == "default"));
    }

    // ---- persona decision knobs -------------------------------------------

    #[test]
    fn json_style_without_knobs_parses_to_defaults() {
        // Backward compat: a style file that predates decision knobs must
        // parse with all knobs at their no-op defaults.
        let def: OutputStyleDef = serde_json::from_str(
            r#"{"name":"formal","label":"Formal","description":"Formal tone.","prompt":"Use formal language."}"#,
        )
        .expect("old-style JSON parses");
        assert_eq!(def.name, "formal");
        assert_eq!(def.effort, None);
        assert_eq!(def.plan, crate::modes::PlanKnobs::Default);
        assert_eq!(def.ask_on_ambiguity, crate::modes::AskAmbiguityMode::Off);
        assert_eq!(def.checkin_cadence, crate::modes::CheckinCadence::Rare);
    }

    #[test]
    fn json_style_with_knobs_parses() {
        let def: OutputStyleDef = serde_json::from_str(
            r#"{"name":"deliberate","label":"Deliberate","description":"d","prompt":"p","effort":"high","plan":"alwaysPlan","askOnAmbiguity":"askOnDesign","checkinCadence":"milestone"}"#,
        )
        .expect("knobs JSON parses");
        assert_eq!(def.effort, Some(crate::effort::EffortLevel::High));
        assert_eq!(def.plan, crate::modes::PlanKnobs::AlwaysPlan);
        assert_eq!(
            def.ask_on_ambiguity,
            crate::modes::AskAmbiguityMode::AskOnDesign
        );
        assert_eq!(def.checkin_cadence, crate::modes::CheckinCadence::Milestone);
    }

    fn deliberate_style_dir() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("output-styles")).unwrap();
        let mut f = std::fs::File::create(dir.path().join("output-styles").join("deliberate.json"))
            .unwrap();
        f.write_all(
            br#"{"name":"deliberate","label":"Deliberate","description":"d","prompt":"p","effort":"high","plan":"alwaysPlan","askOnAmbiguity":"askOnDesign","checkinCadence":"milestone"}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn apply_persona_knobs_fills_effort_when_no_mode() {
        let dir = deliberate_style_dir();
        let mut cfg = crate::Config {
            output_style: Some("deliberate".to_string()),
            ..Default::default()
        };
        apply_persona_knobs(&mut cfg, dir.path());
        assert_eq!(cfg.default_effort, Some(crate::effort::EffortLevel::High));
    }

    #[test]
    fn apply_persona_knobs_mode_wins_over_persona_effort() {
        // Mode 'fast' binds effort=Low; a persona binding effort=High must NOT
        // override it (mode > persona).
        let dir = deliberate_style_dir();
        let mut cfg = crate::Config {
            mode: Some("fast".to_string()),
            output_style: Some("deliberate".to_string()),
            ..Default::default()
        };
        // Simulate mode application as main.rs does.
        let modes = crate::modes::all_modes_for_project(dir.path(), std::path::Path::new("."));
        let fast = crate::modes::find_mode(&modes, "fast")
            .expect("fast built-in")
            .clone();
        crate::modes::apply_mode(&mut cfg, &fast);
        apply_persona_knobs(&mut cfg, dir.path());
        assert_eq!(cfg.default_effort, Some(crate::effort::EffortLevel::Low));
    }

    #[test]
    fn apply_persona_knobs_fills_plan_when_mode_binds_none() {
        let dir = deliberate_style_dir();
        let mut cfg = crate::Config {
            output_style: Some("deliberate".to_string()),
            ..Default::default()
        };
        apply_persona_knobs(&mut cfg, dir.path());
        assert_eq!(cfg.permission_mode, crate::PermissionMode::Plan);
    }

    #[test]
    fn apply_persona_knobs_mode_plan_wins_over_persona() {
        // Mode 'walkaway' binds permission_mode=AcceptEdits; a persona
        // AlwaysPlan must NOT override it (mode > persona).
        let dir = deliberate_style_dir();
        let mut cfg = crate::Config {
            mode: Some("walkaway".to_string()),
            output_style: Some("deliberate".to_string()),
            ..Default::default()
        };
        let modes = crate::modes::all_modes_for_project(dir.path(), std::path::Path::new("."));
        let walkaway = crate::modes::find_mode(&modes, "walkaway")
            .expect("walkaway built-in")
            .clone();
        crate::modes::apply_mode(&mut cfg, &walkaway);
        apply_persona_knobs(&mut cfg, dir.path());
        assert_eq!(cfg.permission_mode, crate::PermissionMode::AcceptEdits);
    }

    #[test]
    fn persona_guidance_block_none_for_default_knobs() {
        let style = OutputStyleDef::default();
        assert_eq!(persona_guidance_block(&style), None);
    }

    #[test]
    fn persona_guidance_block_synthesizes_cadence_ask() {
        let style = OutputStyleDef {
            checkin_cadence: crate::modes::CheckinCadence::EveryTurn,
            ask_on_ambiguity: crate::modes::AskAmbiguityMode::Balanced,
            ..Default::default()
        };
        let block = persona_guidance_block(&style).expect("guidance block");
        assert!(block.contains("narrate your plan"), "{block}");
        assert!(block.contains("ask one clarifying question"), "{block}");
    }
}
