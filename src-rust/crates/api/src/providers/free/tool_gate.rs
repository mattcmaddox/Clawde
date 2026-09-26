/// Lightweight detection of a *non-native* tool call in a model response: a
/// tool invocation written as text (a "prose" tool call) rather than a
/// structured provider call. The query loop owns the full lift/parse; this is
/// only the routing signal the free chain needs to learn which upstreams answer
/// tool-bearing requests with prose instead of structured calls, so it can
/// deprioritize them for tool work. It is intentionally a cheap marker scan
/// (no JSON parsing) so it never misfires on a valid structured response.
pub(crate) fn text_has_non_native_tool_call(text: &str) -> bool {
    const TAG_MARKERS: &[&str] = &[
        "tool_call>",       // <tool_call> and the ZWSP/plain forms
        "[TOOL_CALLS]",     // Qwen/Hermes array form
        "tool_calls_begin", // <|tool_calls_begin|> special tokens
        "execute_bash>",    // Claude-Code shell tag
        "<bash>",           // <bash>…</bash>
        "antml:",           // antml:function_calls / antml:invoke
        "function_calls>",  // <function_calls>…</function_calls>
        "<tool ",           // <tool name="Bash">…</tool>
    ];
    if TAG_MARKERS.iter().any(|m| text.contains(m)) {
        return true;
    }
    // JSON-shaped tool call: require BOTH a name and an arguments key so an
    // ordinary mention of `"name"` in prose does not count.
    text.contains("\"name\"") && text.contains("\"arguments\"")
}

/// Phrases a model uses to *describe* a tool interaction it never performed.
/// Observed on a weak free lane: asked to read an existing file, it replied
/// "I verified this by searching…" and rendered a fabricated `<content>` block,
/// with no tool call in the turn. Cheap substring check, same spirit as the
/// dialect scan above.
const FALSE_ACTION_MARKERS: &[&str] = &[
    "i verified",
    "i searched",
    "i checked the file",
    "i read the file",
    "let me verify",
    "after reading the file",
    "as shown in the file contents",
    "the file does not exist",
    "file not found in the project",
];

/// Whether the text narrates a tool interaction that produced no tool call.
///
/// This is a distinct failure from [`text_has_non_native_tool_call`]: there the
/// model *did* try to call a tool but encoded it as prose (recoverable by the
/// lift). Here it makes no call at all yet writes as though it did, so the user
/// reads fabricated tool output as fact.
pub(crate) fn text_claims_unbacked_action(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    FALSE_ACTION_MARKERS.iter().any(|m| lower.contains(m))
}

/// Minimum completed tool-bearing attempts before an upstream's prose rate is
/// trusted enough to demote it. Prevents a single stray prose call from
/// condemning a healthy lane.
const MIN_TOOL_SAMPLES: u32 = 3;

/// Percentage of tool-bearing attempts that must be prose before an upstream is
/// considered prose-prone for routing.
const PROSE_PCT: u32 = 60;

/// Fewer unbacked-claim samples are needed than prose samples: a fabricated
/// tool result misleads the user, whereas prose is only untidy.
const UNBACKED_SAMPLES: u32 = 2;

/// Rolling per-upstream tally of how an upstream answers tool-bearing requests:
/// a structured tool call vs a non-native (prose) one. Persisted alongside the
/// other free-provider routing signals so a lane that habitually emits prose is
/// demoted for tool work across turns (and processes), steering the chain toward
/// upstreams that actually produce structured calls.
#[derive(Debug, Default, Clone)]
pub struct ToolDialectState {
    prose: Vec<u32>,
    structured: Vec<u32>,
    /// Attempts that narrated a tool action while emitting no tool call.
    unbacked: Vec<u32>,
}

impl ToolDialectState {
    pub fn new(n: usize) -> Self {
        Self {
            prose: vec![0; n],
            structured: vec![0; n],
            unbacked: vec![0; n],
        }
    }

    /// Record one completed tool-bearing attempt for the upstream at `idx`.
    /// `prose` = the attempt answered with a non-native tool call.
    pub fn record(&mut self, idx: usize, prose: bool) {
        let slot = if prose {
            &mut self.prose
        } else {
            &mut self.structured
        };
        if let Some(cell) = slot.get_mut(idx) {
            *cell = cell.saturating_add(1);
        }
    }

    /// Record that a tool-bearing attempt narrated an action but produced no
    /// tool call. Counted separately from prose: this lane emitted nothing the
    /// lift could recover, which is strictly worse.
    pub fn record_unbacked(&mut self, idx: usize) {
        if let Some(cell) = self.unbacked.get_mut(idx) {
            *cell = cell.saturating_add(1);
        }
    }

    /// Whether the upstream at `idx` has repeatedly claimed tool activity it
    /// never performed. Demoted for tool work on a lower bar than prose-prone
    /// because a fabricated tool result is actively misleading, whereas prose
    /// is merely untidy.
    pub fn is_unbacked_claimer(&self, idx: usize) -> bool {
        let total = self.prose.get(idx).copied().unwrap_or(0)
            + self.structured.get(idx).copied().unwrap_or(0);
        self.unbacked.get(idx).copied().unwrap_or(0) >= UNBACKED_SAMPLES
            && self.unbacked.get(idx).copied().unwrap_or(0) * 2 >= total.max(1)
    }

    /// Whether the upstream at `idx` is prose-prone enough to demote for
    /// tool-bearing routing. Requires a minimum sample and a majority-prose rate.
    pub fn is_prose_prone(&self, idx: usize) -> bool {
        let (Some(p), Some(s)) = (self.prose.get(idx), self.structured.get(idx)) else {
            return false;
        };
        let total = p.saturating_add(*s);
        total >= MIN_TOOL_SAMPLES && p.saturating_mul(100) >= total.saturating_mul(PROSE_PCT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> ToolDialectState {
        ToolDialectState::new(3)
    }

    #[test]
    fn detects_unbacked_action_claims() {
        // Observed on a weak free lane: claimed to have searched for a file
        // that existed and rendered a fabricated <content> block, with no tool
        // call in the turn at all.
        assert!(text_claims_unbacked_action(
            "Could not execute it because a.py does not exist. I verified this by searching."
        ));
        assert!(text_claims_unbacked_action(
            "Let me verify the contents first."
        ));
        // A normal answer that merely uses the word is not a claim.
        assert!(!text_claims_unbacked_action("The test suite is green."));
        assert!(!text_claims_unbacked_action(
            "Here is the file: <content>x=1</content>"
        ));
    }

    #[test]
    fn unbacked_claimer_needs_a_repeat_offender() {
        let mut st = ToolDialectState::new(2);
        // One strike is not enough — it could be a one-off.
        st.record_unbacked(0);
        assert!(!st.is_unbacked_claimer(0), "single sample must not demote");
        st.record_unbacked(0);
        assert!(st.is_unbacked_claimer(0), "repeat claimer is demoted");
        // A lane that actually calls tools is never flagged, however it reads.
        let mut ok = ToolDialectState::new(2);
        ok.record(1, false);
        ok.record(1, false);
        ok.record(1, false);
        assert!(!ok.is_unbacked_claimer(1));
    }

    #[test]
    fn detects_known_non_native_dialects() {
        assert!(text_has_non_native_tool_call(
            "<tool_call>shell<arg_key>command</arg_key>ls</tool_call>"
        ));
        assert!(text_has_non_native_tool_call(
            "<tool_call>shell<arg_key>command</arg_key>ls</tool_call>"
        ));
        assert!(text_has_non_native_tool_call(
            "[TOOL_CALLS][{\"name\":\"Bash\"}]"
        ));
        assert!(text_has_non_native_tool_call(
            "<|tool_calls_begin|>funcs.0.name[Bash]"
        ));
        assert!(text_has_non_native_tool_call(
            "<execute_bash>ls</execute_bash>"
        ));
        assert!(text_has_non_native_tool_call("Let me run <bash>pwd</bash>"));
        assert!(text_has_non_native_tool_call(
            "{\"name\":\"Bash\",\"arguments\":{}}"
        ));
    }

    #[test]
    fn ignores_plain_text_and_weak_json_mentions() {
        assert!(!text_has_non_native_tool_call("just a normal sentence"));
        assert!(!text_has_non_native_tool_call(
            "the \"name\" field is optional"
        ));
        assert!(!text_has_non_native_tool_call("no tools here"));
    }

    #[test]
    fn prose_prone_requires_min_samples_and_majority() {
        let mut s = st();
        // Not enough samples yet.
        s.record(0, true);
        s.record(0, true);
        assert!(!s.is_prose_prone(0));
        // Third prose sample crosses the minimum; all-prose -> prone.
        s.record(0, true);
        assert!(s.is_prose_prone(0));
        // A structured sample dilutes the rate. 3 prose / 5 total is still 60%
        // (at threshold), so add a third structured sample: 3/6 = 50% < 60%.
        s.record(0, false);
        s.record(0, false);
        s.record(0, false);
        assert!(!s.is_prose_prone(0));
        // Other lanes unaffected.
        assert!(!s.is_prose_prone(1));
        assert!(!s.is_prose_prone(2));
    }
}
