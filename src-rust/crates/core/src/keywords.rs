// keywords.rs — the shared registry of inline prompt keywords.
//
// An *inline keyword* is a single word that, typed anywhere in a prompt,
// changes how *that one turn* behaves (transient), mirroring how the `/effort`
// or `/output-style` selectors change it *persistently*. `ultracode` was the
// first of these; this module generalises the mechanism so personas
// (`cathead`, `caveman`, `normal`) ride the exact same rails.
//
// This registry is the single source of truth shared across the workspace:
//   - the TUI prompt box highlights each keyword with its themed gradient
//     (see `crates/tui/src/prompt_input.rs`),
//   - the query loop reads the last user message to decide the effective
//     effort / persona for the turn (see `crates/query/src/lib.rs`).
//
// The generic matcher [`keyword_match_ranges`] is the generalisation of the
// original `effort::ultracode_match_ranges`; `ultracode_match_ranges` now
// delegates to it so ultracode behaves exactly as before.

use crate::effort::EffortLevel;

// ---------------------------------------------------------------------------
// Registry types
// ---------------------------------------------------------------------------

/// What an inline keyword does for the turn it appears in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineEffect {
    /// Raise the turn's effort to this level (e.g. `ultracode`).
    Effort(EffortLevel),
    /// Apply the named output-style persona for the turn.
    ///
    /// The name refers to a built-in output style (see
    /// [`crate::output_styles`]). The reserved name `"default"` means "reset to
    /// no persona for this turn" — that is what `normal` does.
    Persona(&'static str),
    /// Activate the named mode preset for the turn (e.g. `mode:careful`).
    ///
    /// Unlike personas, mode names are resolved dynamically (built-in +
    /// user-defined disk modes), so the static registry only carries the
    /// built-in names for highlighting; the query loop uses
    /// [`inline_mode_name`] for the actual resolution.
    Mode(&'static str),
}

/// One inline keyword and the effect it triggers.
#[derive(Debug, Clone, Copy)]
pub struct InlineKeyword {
    /// The single word that activates it. ASCII, matched whole-word and
    /// case-insensitively.
    pub keyword: &'static str,
    /// The effect applied to the turn the keyword appears in.
    pub effect: InlineEffect,
    /// Whether the prompt box paints this keyword with a themed gradient.
    ///
    /// `normal` is a reset and intentionally has **no** gradient (typing it in
    /// ordinary prose should not light up), so its flag is `false`.
    pub gradient: bool,
}

impl InlineKeyword {
    /// The output-style name this keyword selects, if it is a persona keyword.
    ///
    /// Returns `Some("default")` for the reset keyword (`normal`).
    pub fn persona_style(&self) -> Option<&'static str> {
        match self.effect {
            InlineEffect::Persona(name) => Some(name),
            InlineEffect::Effort(_) | InlineEffect::Mode(_) => None,
        }
    }
}

/// The canonical registry of inline keywords.
///
/// Order matters only for display; matching is done per-keyword. Keep the
/// words mutually non-overlapping (no keyword is a substring boundary of
/// another) so the gradient renderer never has to resolve conflicts.
pub const INLINE_KEYWORDS: &[InlineKeyword] = &[
    InlineKeyword {
        keyword: "ultracode",
        effect: InlineEffect::Effort(EffortLevel::Ultracode),
        gradient: true,
    },
    InlineKeyword {
        keyword: "cathead",
        effect: InlineEffect::Persona("cathead"),
        gradient: true,
    },
    InlineKeyword {
        keyword: "caveman",
        effect: InlineEffect::Persona("caveman"),
        gradient: true,
    },
    InlineKeyword {
        keyword: "normal",
        effect: InlineEffect::Persona("default"),
        gradient: false,
    },
    InlineKeyword {
        keyword: "mode:careful",
        effect: InlineEffect::Mode("careful"),
        gradient: true,
    },
    InlineKeyword {
        keyword: "mode:fast",
        effect: InlineEffect::Mode("fast"),
        gradient: true,
    },
];

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// Find every whole-word, case-insensitive occurrence of `keyword` in `text`,
/// returned as non-overlapping `(start, end)` byte ranges.
///
/// The keyword is expected to be ASCII, so an ASCII-lowercased copy preserves
/// byte length and offsets exactly; every match therefore maps back onto `text`
/// at valid char boundaries. "Whole-word" means the byte immediately
/// before/after a match must not be ASCII alphanumeric (so `ultracoder` does
/// not match `ultracode`).
///
/// This is the generalisation of the original ultracode-only matcher; passing
/// `"ultracode"` reproduces it exactly.
pub fn keyword_match_ranges(text: &str, keyword: &str) -> Vec<(usize, usize)> {
    if keyword.is_empty() {
        return Vec::new();
    }
    let hay = text.as_bytes().to_ascii_lowercase();
    let bytes = text.as_bytes();
    let needle = keyword.as_bytes().to_ascii_lowercase();
    let k = needle.as_slice();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < hay.len() {
        if i + k.len() <= hay.len() && &hay[i..i + k.len()] == k {
            let end = i + k.len();
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
            if left_ok && right_ok {
                ranges.push((i, end));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    ranges
}

/// Look up an inline keyword by its exact (case-insensitive) word.
pub fn find_inline_keyword(word: &str) -> Option<&'static InlineKeyword> {
    INLINE_KEYWORDS
        .iter()
        .find(|kw| kw.keyword.eq_ignore_ascii_case(word))
}

/// The transient mode selected by an inline `mode:<name>` keyword in `text`,
/// if any.
///
/// Matches whole-word `mode:<name>` tokens case-insensitively; the name is
/// `[A-Za-z0-9_-]+` (so `mode:careful` and `mode:my-mode` both work). When
/// several appear, the one whose last occurrence is **latest** in the text
/// wins (mirrors [`inline_persona_style`]). The name is resolved against the
/// session's mode registry by the caller — disk-defined modes work even
/// though they are not in the static [`INLINE_KEYWORDS`] registry.
pub fn inline_mode_name(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    // Lowercase into a byte vector (like `keyword_match_ranges`) so scanning
    // never slices a String at a non-char-boundary byte when the text contains
    // multi-byte UTF-8 (e.g. '—'): `Vec<u8>` slicing has no boundary checks.
    let lower = bytes.to_ascii_lowercase();
    const PREFIX: &[u8] = b"mode:";
    let mut best: Option<(usize, &str)> = None;
    let mut i = 0usize;
    while i < lower.len() {
        if i + PREFIX.len() <= lower.len() && &lower[i..i + PREFIX.len()] == PREFIX {
            let left_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            if left_ok {
                let name_start = i + PREFIX.len();
                let mut end = name_start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'-'
                        || bytes[end] == b'_')
                {
                    end += 1;
                }
                let right_ok = end == bytes.len() || !bytes[end].is_ascii_alphanumeric();
                // The name run is pure ASCII, so `name_start..end` is a char
                // boundary in `text` — slicing the original String is safe.
                if end > name_start && right_ok {
                    best = Some((i, &text[name_start..end]));
                }
                i = end.max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    best.map(|(_, name)| name)
}

/// The transient persona selected by inline keywords in `text`, if any.
///
/// Returns the output-style name to use for *this turn* — which may be
/// `"default"` (the reset produced by `normal`). When several persona keywords
/// appear, the one whose last occurrence is **latest** in the text wins, so a
/// trailing `normal` reliably clears an earlier `cathead`. Effort keywords (like
/// `ultracode`) are ignored here — they are resolved separately by the effort
/// path.
pub fn inline_persona_style(text: &str) -> Option<&'static str> {
    let mut best: Option<(usize, &'static str)> = None;
    for kw in INLINE_KEYWORDS {
        let Some(style) = kw.persona_style() else {
            continue;
        };
        if let Some((start, _)) = keyword_match_ranges(text, kw.keyword).into_iter().last() {
            if best.map(|(pos, _)| start >= pos).unwrap_or(true) {
                best = Some((start, style));
            }
        }
    }
    best.map(|(_, style)| style)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_matcher_matches_ultracode_like_original() {
        let text = "please ultracode this";
        let r = keyword_match_ranges(text, "ultracode");
        assert_eq!(r.len(), 1);
        let (s, e) = r[0];
        assert_eq!(&text[s..e], "ultracode");
    }

    #[test]
    fn generic_matcher_is_case_insensitive() {
        assert_eq!(keyword_match_ranges("CaThEaD now", "cathead").len(), 1);
        assert_eq!(keyword_match_ranges("CAVEMAN mode", "caveman").len(), 1);
    }

    #[test]
    fn generic_matcher_respects_word_boundaries() {
        assert!(keyword_match_ranges("catheadphones", "cathead").is_empty());
        assert!(keyword_match_ranges("cavemanic", "caveman").is_empty());
        assert!(keyword_match_ranges("abnormal", "normal").is_empty());
    }

    #[test]
    fn empty_keyword_never_matches() {
        assert!(keyword_match_ranges("anything", "").is_empty());
    }

    #[test]
    fn registry_covers_all_personas_and_ultracode() {
        assert!(matches!(
            find_inline_keyword("ultracode").map(|k| k.effect),
            Some(InlineEffect::Effort(EffortLevel::Ultracode))
        ));
        assert_eq!(
            find_inline_keyword("cathead").unwrap().persona_style(),
            Some("cathead")
        );
        assert_eq!(
            find_inline_keyword("caveman").unwrap().persona_style(),
            Some("caveman")
        );
        // `normal` resets to the default (no persona).
        assert_eq!(
            find_inline_keyword("normal").unwrap().persona_style(),
            Some("default")
        );
        assert!(find_inline_keyword("nope").is_none());
    }

    #[test]
    fn normal_has_no_gradient_but_personas_do() {
        assert!(!find_inline_keyword("normal").unwrap().gradient);
        assert!(find_inline_keyword("cathead").unwrap().gradient);
        assert!(find_inline_keyword("caveman").unwrap().gradient);
        assert!(find_inline_keyword("ultracode").unwrap().gradient);
    }

    #[test]
    fn inline_persona_style_picks_persona_keyword() {
        assert_eq!(inline_persona_style("please cathead this"), Some("cathead"));
        assert_eq!(inline_persona_style("caveman it up"), Some("caveman"));
        assert_eq!(
            inline_persona_style("back to normal please"),
            Some("default")
        );
        assert_eq!(inline_persona_style("nothing special here"), None);
    }

    #[test]
    fn inline_persona_style_ignores_ultracode() {
        // Ultracode is an effort keyword, not a persona; it must not be
        // reported by the persona resolver.
        assert_eq!(inline_persona_style("ultracode this hard problem"), None);
    }

    #[test]
    fn inline_persona_style_last_wins() {
        // A trailing `normal` clears an earlier `cathead`.
        assert_eq!(
            inline_persona_style("cathead then back to normal"),
            Some("default")
        );
        // ...and vice-versa.
        assert_eq!(inline_persona_style("normal then cathead"), Some("cathead"));
    }

    #[test]
    fn registry_covers_builtin_mode_keywords() {
        assert_eq!(
            find_inline_keyword("mode:careful").unwrap().effect,
            InlineEffect::Mode("careful")
        );
        assert_eq!(
            find_inline_keyword("mode:fast").unwrap().effect,
            InlineEffect::Mode("fast")
        );
        // Mode keywords are not personas.
        assert_eq!(
            find_inline_keyword("mode:careful").unwrap().persona_style(),
            None
        );
        assert!(find_inline_keyword("mode:careful").unwrap().gradient);
    }

    #[test]
    fn inline_mode_name_picks_mode_keyword() {
        assert_eq!(inline_mode_name("do it mode:careful"), Some("careful"));
        assert_eq!(inline_mode_name("Mode:FAST now"), Some("FAST"));
        assert_eq!(inline_mode_name("mode:my-mode please"), Some("my-mode"));
        assert_eq!(inline_mode_name("plain text here"), None);
        // `mode:` with no name does not match.
        assert_eq!(inline_mode_name("insert mode: now"), None);
        // No left boundary: `xmode:careful` is not a mode keyword.
        assert_eq!(inline_mode_name("xmode:careful"), None);
        // The name token is greedy `[A-Za-z0-9_-]+`: `mode:carefulx` parses
        // as the (unknown) mode "carefulx" — resolution happens downstream.
        assert_eq!(inline_mode_name("mode:carefulx"), Some("carefulx"));
    }

    #[test]
    fn inline_mode_name_last_wins() {
        // A trailing keyword overrides an earlier one (mirrors personas).
        assert_eq!(
            inline_mode_name("mode:fast then mode:careful"),
            Some("careful")
        );
        assert_eq!(
            inline_mode_name("mode:careful then mode:fast"),
            Some("fast")
        );
    }

    #[test]
    fn inline_mode_name_ignored_by_persona_resolver() {
        // The persona resolver must not report mode keywords as personas.
        assert_eq!(inline_persona_style("mode:careful please"), None);
    }

    #[test]
    fn inline_mode_name_survives_multibyte_utf8() {
        // Regression: scanning must not slice at a non-char-boundary byte when
        // the prompt contains multi-byte characters (e.g. '—').
        assert_eq!(
            inline_mode_name("summarize — mode:careful now"),
            Some("careful")
        );
        assert_eq!(
            inline_mode_name("→ do it mode:fast"),
            Some("fast"),
            "multibyte before the keyword must not panic"
        );
        assert_eq!(inline_mode_name("—plain—"), None);
    }
}
