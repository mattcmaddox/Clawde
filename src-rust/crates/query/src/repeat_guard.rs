/// Repeat-tool-reminder guard.
///
/// Tracks consecutive identical tool calls and injects escalating reminders
/// when a threshold is hit. Inspired by deepseek-harness's
/// `repeat-tool-reminder` guard. Breaks infinite loops where the model
/// hammers the same tool with identical arguments.
///
/// Design:
/// - Per-chain tracking: `(tool_name, canonical_args)` → count
/// - Args are deep-key-sorted before comparison so `{a:1,b:2}` == `{b:2,a:1}`
/// - Thresholds default to [3, 5, 8]: first is gentle, later are detailed
/// - Legacy call-only reminders remain advisory; result-aware tracking warns
///   on the third unchanged result and requests a pause on the fourth.
/// - Reset on user messages (repetition across user input is not a loop)
///
/// Default consecutive-repeat counts that trigger a reminder.
const DEFAULT_THRESHOLDS: &[u32] = &[3, 5, 8];

/// Maximum characters of canonical arguments quoted in the detailed
/// reminder.
const DEFAULT_ARGS_PREVIEW_CHARS: usize = 500;

/// Tracks one agent's consecutive-repeat chain.
struct Chain {
    /// Canonical key: `tool_name:canonical_args`.
    key: String,
    /// How many consecutive identical calls.
    count: u32,
}

/// Detects consecutive identical tool calls and produces reminders.
pub(crate) struct RepeatCallDetector {
    last_chain: Option<Chain>,
    thresholds: Vec<u32>,
    args_preview_chars: usize,
    /// Result fingerprints stay separate from reminder text and tool-call ids.
    results: std::collections::HashMap<String, u64>,
    unchanged_counts: std::collections::HashMap<String, u32>,
    pause_requested: bool,
}

impl RepeatCallDetector {
    pub fn new() -> Self {
        Self {
            last_chain: None,
            thresholds: DEFAULT_THRESHOLDS.to_vec(),
            args_preview_chars: DEFAULT_ARGS_PREVIEW_CHARS,
            results: std::collections::HashMap::new(),
            unchanged_counts: std::collections::HashMap::new(),
            pause_requested: false,
        }
    }

    /// Record a tool call. Returns a reminder string if a threshold is hit.
    pub fn observe(&mut self, tool_name: &str, args: &serde_json::Value) -> Option<String> {
        let canonical = canonicalize_args(args);
        let key = format!("{}:{}", tool_name, canonical);

        let count = if let Some(ref chain) = self.last_chain {
            if chain.key == key {
                chain.count + 1
            } else {
                1
            }
        } else {
            1
        };

        self.last_chain = Some(Chain { key, count });

        if !self.thresholds.contains(&count) {
            return None;
        }

        if count == self.thresholds[0] {
            Some(
                "You are repeating the exact same tool call with identical \
                 arguments. Carefully analyze the previous result before \
                 calling again: if the task is not complete, try a different \
                 approach or different arguments instead of repeating the call."
                    .to_string(),
            )
        } else {
            let preview = truncate_args(&canonical, self.args_preview_chars);
            Some(format!(
                "Repeated tool call detected:\n\
                 - tool: {}\n\
                 - consecutive_calls: {}\n\
                 - arguments: {}\n\
                 The repeated calls are not making progress. Do not call \
                 this tool with these exact arguments again. Inspect the \
                 latest result and choose a different action, different \
                 arguments, or finish the task if enough evidence has been \
                 gathered.",
                tool_name, count, preview
            ))
        }
    }

    /// Record the raw result before notices are appended. Changed results and
    /// successful writes invalidate repetition evidence; alternating calls are
    /// still tracked by canonical arguments, not just by the last call.
    pub fn observe_result(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        content: &str,
        is_error: bool,
        made_write: bool,
    ) -> Option<String> {
        use std::hash::{Hash, Hasher};

        if made_write && !is_error {
            self.reset();
            return None;
        }
        let key = format!("{}:{}", tool_name, canonicalize_args(args));
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        content.hash(&mut hasher);
        is_error.hash(&mut hasher);
        let fingerprint = hasher.finish();
        if self.results.get(&key) != Some(&fingerprint) {
            // New evidence anywhere in the sequence invalidates the old run.
            self.unchanged_counts.clear();
            self.last_chain = None;
            self.pause_requested = false;
        }
        self.results.insert(key.clone(), fingerprint);
        let legacy_reminder = self.observe(tool_name, args);
        let count = self.unchanged_counts.entry(key).or_default();
        *count += 1;
        if *count == DEFAULT_THRESHOLDS[0] {
            Some(format!(
                "Possible tool loop: {tool_name} returned the same result three times without a state change. Try a different approach, or ask the user how to proceed."
            ))
        } else if *count == DEFAULT_THRESHOLDS[0] + 1 {
            self.pause_requested = true;
            Some(format!(
                "Tool loop paused: {tool_name} returned the same result four times without a state change. Choose a different approach before continuing."
            ))
        } else {
            // Keep the existing later reminder rungs for callers that inspect
            // results without acting on the pause request.
            legacy_reminder
        }
    }

    pub fn pause_requested(&self) -> bool {
        self.pause_requested
    }

    /// Reset on genuine user input or progress, not autonomous continuation.
    pub fn reset(&mut self) {
        self.last_chain = None;
        self.results.clear();
        self.unchanged_counts.clear();
        self.pause_requested = false;
    }
}

/// Deep key-sort a JSON value so two objects differing only in property
/// order canonicalize identically.
fn sort_json_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            // Collect into a Vec first, then sort by key, then insert into a
            // new Map. This is necessary because serde_json::Map with the
            // `preserve_order` feature uses IndexMap (insertion order), not
            // BTreeMap (sorted order).
            let mut pairs: Vec<_> = map
                .iter()
                .map(|(k, v)| (k.clone(), sort_json_value(v)))
                .collect();
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
            let sorted: serde_json::Map<String, serde_json::Value> = pairs.into_iter().collect();
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json_value).collect())
        }
        other => other.clone(),
    }
}

/// Canonical string form of tool call arguments: deep key-sort, then
/// serialize. This ensures `{a:1,b:2}` and `{b:2,a:1}` produce the
/// same string.
fn canonicalize_args(value: &serde_json::Value) -> String {
    serde_json::to_string(&sort_json_value(value)).unwrap_or_default()
}

/// Head-truncate the canonical arguments for quoting in the detailed
/// reminder. Bounds only the model-visible text — the chain key always
/// uses the full canonical string.
fn truncate_args(canonical: &str, max_chars: usize) -> String {
    if canonical.len() <= max_chars {
        canonical.to_string()
    } else {
        let cut = canonical.floor_char_boundary(max_chars);
        format!(
            "{}… (+{} more bytes)",
            &canonical[..cut],
            canonical.len() - cut
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unchanged_results_warn_then_pause() {
        let mut detector = RepeatCallDetector::new();
        let args = json!({"command": "cargo test"});
        for _ in 0..2 {
            assert!(detector
                .observe_result("Bash", &args, "failed", true, false)
                .is_none());
        }
        assert!(detector
            .observe_result("Bash", &args, "failed", true, false)
            .unwrap()
            .contains("Possible tool loop"));
        assert!(!detector.pause_requested());
        assert!(detector
            .observe_result("Bash", &args, "failed", true, false)
            .unwrap()
            .contains("Tool loop paused"));
        assert!(detector.pause_requested());
    }

    #[test]
    fn changed_results_and_writes_reset_repetition() {
        let mut detector = RepeatCallDetector::new();
        let args = json!({"command": "cargo test"});
        for _ in 0..3 {
            detector.observe_result("Bash", &args, "failed", true, false);
        }
        assert!(detector
            .observe_result("Bash", &args, "different failure", true, false)
            .is_none());
        assert!(!detector.pause_requested());
        detector.observe_result("Bash", &args, "different failure", true, false);
        detector.observe_result(
            "Edit",
            &json!({"file_path": "foo.rs"}),
            "written",
            false,
            true,
        );
        assert!(detector
            .observe_result("Bash", &args, "different failure", true, false)
            .is_none());
        assert!(!detector.pause_requested());
    }

    #[test]
    fn alternating_calls_with_unchanged_results_are_detected() {
        let mut detector = RepeatCallDetector::new();
        let a = json!({"path": "a"});
        let b = json!({"path": "b"});
        detector.observe_result("Read", &a, "a", false, false);
        detector.observe_result("Read", &b, "b", false, false);
        for _ in 0..2 {
            detector.observe_result("Read", &a, "a", false, false);
            detector.observe_result("Read", &b, "b", false, false);
        }
        assert!(detector
            .observe_result("Read", &a, "a", false, false)
            .is_some());
        assert!(!detector.pause_requested());
        detector.observe_result("Read", &a, "a", false, false);
        assert!(detector.pause_requested());
    }

    #[test]
    fn failed_write_does_not_reset_repetition() {
        let mut detector = RepeatCallDetector::new();
        for _ in 0..4 {
            detector.observe_result("Edit", &json!({}), "permission denied", true, true);
        }
        assert!(detector.pause_requested());
        detector.reset();
        assert!(!detector.pause_requested());
    }

    #[test]
    fn argument_preview_preserves_unicode_boundaries() {
        let args = "é".repeat(300);
        assert!(truncate_args(&args, 501).starts_with(&"é".repeat(250)));
    }

    #[test]
    fn no_reminder_below_threshold() {
        let mut det = RepeatCallDetector::new();
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
    }

    #[test]
    fn gentle_reminder_at_first_threshold() {
        let mut det = RepeatCallDetector::new();
        // 3 consecutive identical calls
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
        let reminder = det.observe("read_files", &json!({"path": "foo.rs"}));
        assert!(reminder.is_some());
        let text = reminder.unwrap();
        assert!(text.contains("repeating the exact same tool call"));
    }

    #[test]
    fn detailed_reminder_at_second_threshold() {
        let mut det = RepeatCallDetector::new();
        // 5 consecutive identical calls
        for _ in 0..4 {
            det.observe("read_files", &json!({"path": "foo.rs"}));
        }
        let reminder = det.observe("read_files", &json!({"path": "foo.rs"}));
        assert!(reminder.is_some());
        let text = reminder.unwrap();
        assert!(text.contains("consecutive_calls: 5"));
        assert!(text.contains("read_files"));
    }

    #[test]
    fn different_args_resets_chain() {
        let mut det = RepeatCallDetector::new();
        det.observe("read_files", &json!({"path": "foo.rs"}));
        det.observe("read_files", &json!({"path": "foo.rs"}));
        // Different args — counter resets to 1
        assert!(det
            .observe("read_files", &json!({"path": "bar.rs"}))
            .is_none());
        // New chain: count=2 for bar.rs
        assert!(det
            .observe("read_files", &json!({"path": "bar.rs"}))
            .is_none());
        // New chain: count=3 for bar.rs — hits threshold
        let reminder = det.observe("read_files", &json!({"path": "bar.rs"}));
        assert!(reminder.is_some());
    }

    #[test]
    fn different_tool_resets_chain() {
        let mut det = RepeatCallDetector::new();
        det.observe("read_files", &json!({"path": "foo.rs"}));
        det.observe("read_files", &json!({"path": "foo.rs"}));
        // Different tool — counter resets
        assert!(det
            .observe("code_search", &json!({"pattern": "foo"}))
            .is_none());
    }

    #[test]
    fn reset_clears_chain() {
        let mut det = RepeatCallDetector::new();
        det.observe("read_files", &json!({"path": "foo.rs"}));
        det.observe("read_files", &json!({"path": "foo.rs"}));
        det.reset();
        // After reset, count starts from 1 again
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
        assert!(det
            .observe("read_files", &json!({"path": "foo.rs"}))
            .is_none());
        // count=3 — hits threshold
        let reminder = det.observe("read_files", &json!({"path": "foo.rs"}));
        assert!(reminder.is_some());
    }

    #[test]
    fn json_key_order_does_not_matter() {
        let mut det = RepeatCallDetector::new();
        det.observe("tool", &json!({"a": 1, "b": 2}));
        // count=1: no reminder
        det.observe("tool", &json!({"b": 2, "a": 1}));
        // count=2: different key order, but same canonical form — no reminder
        assert!(det.observe("tool", &json!({"a": 1, "b": 2})).is_some());
        // count=3: first threshold — key order did not matter
    }

    #[test]
    fn truncated_args_in_detailed_reminder() {
        let mut det = RepeatCallDetector::new();
        let big_input = json!({"data": "x".repeat(1000)});
        for _ in 0..4 {
            det.observe("tool", &big_input);
        }
        let reminder = det.observe("tool", &big_input).unwrap();
        assert!(reminder.contains("…"));
        assert!(reminder.contains("more bytes"));
    }
}
