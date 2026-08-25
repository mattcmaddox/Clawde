// check_summary.rs — Machine-readable verdict for deterministic project
// checks (RunTests / RunLints), attached to `ToolResult::metadata`.
//
// The query loop's `deterministic_check_observation` previously classified
// check results by matching prose substrings in the tool output ("tests
// passed", "tests failed", "lint issues found") — fragile across test
// frameworks and locales. The built-in check tools now attach a structured
// summary (refactor-loop-health Phase B / Aegis: offload deterministic
// parsing to the environment), and the loop reads it first, keeping the
// substring heuristics only as a fallback for third-party tools.

use serde_json::{json, Value};

/// Which deterministic check produced the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckKind {
    Tests,
    Lints,
}

/// Structured verdict for one deterministic check invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckSummary {
    pub kind: CheckKind,
    /// The command exited 0 — the runner reported success. Note this does not
    /// claim every assertion passed; it is the exit-code signal the tool
    /// itself can vouch for.
    pub passed: bool,
    /// The command was killed after the configured timeout.
    pub timed_out: bool,
    /// Raw exit code, when the command actually started.
    pub exit_code: Option<i32>,
}

/// Metadata key under which the summary is attached to a `ToolResult`.
pub const CHECK_SUMMARY_METADATA_KEY: &str = "check_summary";

impl CheckSummary {
    /// Build the `metadata` value to attach to a check `ToolResult`:
    /// `{ "check_summary": { ... } }`.
    pub fn metadata_value(&self) -> Value {
        json!({ CHECK_SUMMARY_METADATA_KEY: self })
    }

    /// Parse a structured summary out of a `ToolResult`'s metadata, if the
    /// result carries one.
    pub fn from_metadata(metadata: Option<&Value>) -> Option<CheckSummary> {
        let meta = metadata?;
        let summary = meta.get(CHECK_SUMMARY_METADATA_KEY)?;
        serde_json::from_value(summary.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_roundtrips() {
        let summary = CheckSummary {
            kind: CheckKind::Tests,
            passed: true,
            timed_out: false,
            exit_code: Some(0),
        };
        let meta = summary.metadata_value();
        let parsed = CheckSummary::from_metadata(Some(&meta)).expect("must parse");
        assert_eq!(parsed, summary);
    }

    #[test]
    fn missing_or_malformed_metadata_is_none() {
        assert_eq!(CheckSummary::from_metadata(None), None);
        let wrong = json!({ "other": 1 });
        assert_eq!(CheckSummary::from_metadata(Some(&wrong)), None);
        let malformed = json!({ "check_summary": { "kind": "tests" } }); // missing fields
        assert_eq!(CheckSummary::from_metadata(Some(&malformed)), None);
    }

    #[test]
    fn kind_serializes_lowercase() {
        let meta = CheckSummary {
            kind: CheckKind::Lints,
            passed: false,
            timed_out: true,
            exit_code: None,
        }
        .metadata_value();
        assert_eq!(
            serde_json::to_string(&meta).unwrap(),
            r#"{"check_summary":{"exit_code":null,"kind":"lints","passed":false,"timed_out":true}}"#
        );
    }
}
