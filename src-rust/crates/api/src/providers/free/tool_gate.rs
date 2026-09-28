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
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolDialectState {
    prose: Vec<u32>,
    structured: Vec<u32>,
    /// Attempts that narrated a tool action while emitting no tool call.
    unbacked: Vec<u32>,
    /// The upstream each slot belongs to, in slot order.
    ///
    /// The counts are indexed by position in the free chain, but the chain's
    /// composition and order depend on which upstreams the user has configured
    /// (`build_free_provider` skips unconfigured ones in catalog order). Adding
    /// a key for a higher-priority upstream inserts it and shifts every later
    /// index, so a tally written by position alone would re-attach one lane's
    /// prose verdict to a different provider. Its three siblings
    /// (`CooldownState`, `LatencyState`, `CapacityState`) all remap persisted
    /// state by upstream id for exactly this reason; this one now does too.
    ///
    /// A file written before this field existed has no ids and is discarded on
    /// load rather than mapped positionally, because positional mapping is the
    /// misattribution this field exists to prevent. The tally is a 7-day
    /// heuristic, so losing it costs nothing but a little re-learning.
    #[serde(default)]
    upstream_ids: Vec<String>,
    /// Whether this tally has ever been written to disk. An untouched state is
    /// not worth persisting, and skipping it keeps a fresh install's
    /// `free-state/` free of an empty file.
    #[serde(default)]
    dirty: bool,
    /// Whether this state may touch disk at all. Runtime-only: never serialized.
    /// Mirrors the `persist` flag `FreeProvider::with_routing` passes to the
    /// other three state tracks. Without it a test that builds the chain with
    /// `persist: false` still read and rewrote `free-state/tool-dialect.json`,
    /// so its assertions depended on ambient files on disk (a real tally with 21
    /// structured groq samples kept the demotion gate from firing) and the
    /// per-process scratch home was shared between tests.
    #[serde(skip)]
    persist: bool,
}

/// How long a persisted tally is trusted. Long enough that a lane stays
/// demoted across a working session, short enough that a lane which starts
/// behaving recovers on its own.
const TALLY_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// Wire format for the persisted tally.
#[derive(serde::Serialize, serde::Deserialize)]
struct TallyFile {
    saved_at_unix: u64,
    state: ToolDialectState,
}

fn tally_path() -> std::path::PathBuf {
    clawde_core::config::Settings::state_dir()
        .join("free-state")
        .join("tool-dialect.json")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl ToolDialectState {
    pub fn new(n: usize) -> Self {
        Self {
            prose: vec![0; n],
            structured: vec![0; n],
            unbacked: vec![0; n],
            upstream_ids: Vec::new(),
            dirty: false,
            persist: false,
        }
    }

    /// Attach the upstream identity for each slot and, when `persist` is set,
    /// restore a previously-saved tally. This is the constructor production
    /// uses; tests pass `persist: false` for a state that never touches disk.
    pub fn with_persistence(mut self, upstream_ids: Vec<String>, persist: bool) -> Self {
        self.persist = persist;
        if persist {
            self.upstream_ids = upstream_ids;
            self.load_from_file();
        } else {
            self.upstream_ids = upstream_ids;
        }
        self
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
            self.dirty = true;
        }
    }

    /// Record that a tool-bearing attempt narrated an action but produced no
    /// tool call. Counted separately from prose: this lane emitted nothing the
    /// lift could recover, which is strictly worse.
    pub fn record_unbacked(&mut self, idx: usize) {
        if let Some(cell) = self.unbacked.get_mut(idx) {
            *cell = cell.saturating_add(1);
            self.dirty = true;
        }
    }

    /// Restore a previously-saved tally into the current slot layout, ignoring a
    /// stale or corrupt file. Counts are remapped by upstream id, never by
    /// position, so a chain whose composition changed keeps each verdict on the
    /// lane that earned it. A slot with no matching persisted upstream starts at
    /// zero, which is the conservative direction for this gate.
    fn load_from_file(&mut self) {
        let Some(json) = std::fs::read_to_string(tally_path()).ok() else {
            return;
        };
        let Ok(file) = serde_json::from_str::<TallyFile>(&json) else {
            return;
        };
        if now_unix().saturating_sub(file.saved_at_unix) > TALLY_TTL_SECS {
            return;
        }
        let stored = file.state;
        if stored.upstream_ids.is_empty() {
            // Pre-id file: no way to tell which lane a count belongs to.
            return;
        }
        for (old_idx, id) in stored.upstream_ids.iter().enumerate() {
            let Some(new_idx) = self.upstream_ids.iter().position(|cur| cur == id) else {
                continue;
            };
            if let Some(v) = self.prose.get_mut(new_idx) {
                *v = stored.prose.get(old_idx).copied().unwrap_or(0);
            }
            if let Some(v) = self.structured.get_mut(new_idx) {
                *v = stored.structured.get(old_idx).copied().unwrap_or(0);
            }
            if let Some(v) = self.unbacked.get_mut(new_idx) {
                *v = stored.unbacked.get(old_idx).copied().unwrap_or(0);
            }
        }
        // A restored tally is already on disk; only new observations need a write.
        self.dirty = false;
    }

    /// Persist the tally. Best-effort: routing must never fail because the
    /// state file is unwritable. A no-op when persistence is off.
    pub fn save(&self) {
        if !self.persist || !self.dirty {
            return;
        }
        let path = tally_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = TallyFile {
            saved_at_unix: now_unix(),
            state: self.clone(),
        };
        debug_assert!(
            file.state.upstream_ids.len() == file.state.prose.len(),
            "a persisted tally must name every slot it counts"
        );
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = std::fs::write(path, json);
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

    /// Serialises the tests below that read and write the one real
    /// `free-state/tool-dialect.json` under the scratch home. Without it they
    /// race each other on the parallel runner.
    static TALLY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Write a tally straight to the path production reads, so `load_from_file`
    /// is exercised rather than `serde` in isolation.
    fn write_tally(stored: &ToolDialectState) {
        let path = tally_path();
        std::fs::create_dir_all(path.parent().expect("tally parent")).expect("create free-state");
        let file = TallyFile {
            saved_at_unix: now_unix(),
            state: stored.clone(),
        };
        std::fs::write(&path, serde_json::to_string(&file).expect("encode tally"))
            .expect("write tally");
    }

    #[test]
    fn tally_survives_a_save_load_round_trip() {
        // The gate's value is that a lane stays demoted in the NEXT process.
        // That only holds if the tally is actually persisted, which it was not:
        // the state was rebuilt empty on every startup while the docs claimed
        // otherwise.
        let _guard = TALLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = ids(&["groq", "sambanova", "cerebras"]);

        let mut st = ToolDialectState::new(3).with_persistence(home.clone(), true);
        st.record(0, true);
        st.record(0, true);
        st.record(0, true);
        st.record_unbacked(1);
        st.record_unbacked(1);
        st.record(2, false);
        st.save();

        let loaded = ToolDialectState::new(3).with_persistence(home, true);
        assert!(loaded.is_prose_prone(0), "prose verdict survived");
        assert!(loaded.is_unbacked_claimer(1), "unbacked verdict survived");
        assert!(!loaded.is_prose_prone(2), "clean lane not condemned");
    }

    /// Adding a key for a higher-priority upstream inserts it into the chain and
    /// shifts every later index, so a tally keyed by position would hand one
    /// lane's verdict to a different provider. The counts must follow the id.
    #[test]
    fn a_tally_follows_its_upstream_when_the_chain_is_reordered() {
        let _guard = TALLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut stored =
            ToolDialectState::new(3).with_persistence(ids(&["alpha", "beta", "gamma"]), true);
        // Only `beta` (slot 1) is prose-prone.
        stored.record(1, true);
        stored.record(1, true);
        stored.record(1, true);
        stored.record(0, false);
        write_tally(&stored);

        // `delta` is configured, so it is now first and every index shifts.
        let reloaded = ToolDialectState::new(4)
            .with_persistence(ids(&["delta", "alpha", "beta", "gamma"]), true);
        assert!(
            reloaded.is_prose_prone(2),
            "beta keeps its verdict at its new index"
        );
        assert!(
            !reloaded.is_prose_prone(1),
            "alpha must not inherit beta's verdict from slot 1"
        );
        assert!(
            !reloaded.is_prose_prone(0),
            "the newly-configured lane starts clean"
        );
        assert!(!reloaded.is_prose_prone(3), "gamma stays clean");
    }

    /// A file written before ids existed cannot be attributed, so it is dropped
    /// rather than mapped positionally — positional mapping is the bug.
    #[test]
    fn a_tally_without_upstream_ids_is_discarded() {
        let _guard = TALLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut stored = ToolDialectState::new(2);
        stored.record(1, true);
        stored.record(1, true);
        stored.record(1, true);
        stored.record(1, false);
        write_tally(&stored);

        let loaded = ToolDialectState::new(2).with_persistence(ids(&["groq", "sambanova"]), true);
        assert!(
            !loaded.is_prose_prone(1),
            "an unattributable tally must not condemn a lane"
        );
    }

    /// `persist: false` must mean the state never touches disk, so a test that
    /// builds the chain that way cannot read ambient files (the real tally held
    /// 21 structured groq samples, which silenced the demotion gate) or leave
    /// one behind for the next test in the process.
    #[test]
    fn persist_false_neither_reads_nor_writes_the_tally() {
        let _guard = TALLY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = ids(&["groq", "sambanova"]);

        // Seed disk with a verdict that would suppress a demotion.
        let mut stored = ToolDialectState::new(2).with_persistence(home.clone(), true);
        stored.record(1, true);
        stored.record(1, true);
        stored.record(1, true);
        stored.save();

        let ephemeral = ToolDialectState::new(2).with_persistence(home.clone(), false);
        assert!(
            !ephemeral.is_prose_prone(1),
            "a non-persisting state must start empty regardless of the file"
        );

        let mut ephemeral = ephemeral;
        ephemeral.record(0, true);
        ephemeral.record(0, true);
        ephemeral.record(0, true);
        ephemeral.save();
        // The seeded verdict must be intact: nothing was written over it.
        let reloaded = ToolDialectState::new(2).with_persistence(home, true);
        assert!(reloaded.is_prose_prone(1), "seeded verdict untouched");
        assert!(
            !reloaded.is_prose_prone(0),
            "the ephemeral write never landed"
        );
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
