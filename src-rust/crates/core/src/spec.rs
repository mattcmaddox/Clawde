// Structured specification data types for Spec-Driven Development mode
// (audit spec §10). A `Spec` is the artifact the agent produces *before*
// writing code for a non-trivial task: requirements, a file plan, data
// models, acceptance tests, and edge cases. The user reviews and approves
// it, then implementation proceeds against the spec with its acceptance
// tests as the verification criteria (§10.4).

use serde::{Deserialize, Serialize};

/// What the spec plans to do to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileAction {
    /// Create a new file.
    #[serde(rename = "Create", alias = "create")]
    Create,
    /// Modify an existing file.
    #[serde(rename = "Modify", alias = "modify")]
    Modify,
    /// Delete an existing file.
    #[serde(rename = "Delete", alias = "delete")]
    Delete,
}

/// One file the implementation plan will touch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePlan {
    /// Path relative to the repository root, e.g. `crates/api/src/middleware.rs`.
    pub path: String,
    /// What to do with the file.
    pub action: FileAction,
    /// Why this file changes / what changes inside it.
    pub description: String,
}

/// A data structure the spec introduces or changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataModel {
    /// Type name, e.g. `RateLimiter`.
    pub name: String,
    /// Fields or shape, e.g. `window: Duration, max_requests: u32`.
    pub definition: String,
}

/// One acceptance test the implementation must satisfy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptanceTest {
    /// Human-readable description, e.g. "Requests over limit return 429".
    pub description: String,
}

/// A structured specification for a task (audit spec §10.3).
///
/// The LLM produces one of these (as JSON) via `/spec`; the user reviews it
/// in the TUI before implementation begins.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Spec {
    /// Short task title, e.g. "Rate-Limiting Middleware".
    pub title: String,
    /// Ordered functional requirements, plain language.
    pub requirements: Vec<String>,
    /// Planned file changes.
    pub files_to_touch: Vec<FilePlan>,
    /// Data structures introduced or changed.
    pub data_models: Vec<DataModel>,
    /// Acceptance tests the implementation must pass.
    pub acceptance_tests: Vec<AcceptanceTest>,
    /// Edge cases the implementation should handle.
    pub edge_cases: Vec<String>,
}

impl Spec {
    /// Serialize to pretty-printed JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Write the spec to a JSON file at `path`, creating parent directories.
    pub fn write_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json())
    }

    /// Parse a spec from raw LLM output.
    ///
    /// Strips surrounding markdown code fences (```` ```json ```` / ```` ``` ````)
    /// if the model wrapped the JSON, then parses. Returns an error message
    /// describing what went wrong, so the caller can surface it to the user
    /// instead of panicking.
    pub fn parse_json(raw: &str) -> Result<Spec, String> {
        let body = strip_fences(raw.trim());
        let spec: Spec =
            serde_json::from_str(body).map_err(|e| format!("Could not parse spec JSON: {e}"))?;
        if spec.title.trim().is_empty() {
            return Err("Spec JSON parsed but has an empty `title`.".to_string());
        }
        Ok(spec)
    }
}

/// Extract JSON from raw model output: strip prose before/after and a single
/// pair of surrounding markdown code fences if present.
fn strip_fences(input: &str) -> &str {
    let mut s = input.trim();
    // Find the first opening fence (```json or ```) anywhere in the output.
    if let Some(start) = s.find("```") {
        s = s[start + 3..].trim_start();
        // Drop an optional language tag on the fence line. A single-line
        // fenced block (```json {...}```) has no newline to skip — the tag
        // directly abuts the JSON, so scan for where the JSON actually starts.
        if !s.starts_with('{') {
            match s.find('{') {
                Some(idx) => s = s[idx..].trim_start(),
                None => s = "",
            }
        }
        // Trailing closing fence.
        if let Some(idx) = s.rfind("```") {
            s = s[..idx].trim_end();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> Spec {
        Spec {
            title: "Rate-Limiting Middleware".to_string(),
            requirements: vec![
                "Per-IP rate limiting with configurable window".to_string(),
                "Integrates with the existing tower::Service stack".to_string(),
            ],
            files_to_touch: vec![
                FilePlan {
                    path: "crates/api/src/middleware/rate_limit.rs".to_string(),
                    action: FileAction::Create,
                    description: "New middleware".to_string(),
                },
                FilePlan {
                    path: "crates/api/src/middleware/mod.rs".to_string(),
                    action: FileAction::Modify,
                    description: "Wire the module".to_string(),
                },
            ],
            data_models: vec![DataModel {
                name: "RateLimiter".to_string(),
                definition: "window: Duration, max_requests: u32".to_string(),
            }],
            acceptance_tests: vec![AcceptanceTest {
                description: "Requests under the limit pass through".to_string(),
            }],
            edge_cases: vec!["IPv6 addresses normalized".to_string()],
        }
    }

    #[test]
    fn spec_json_round_trip_preserves_all_fields() {
        let spec = sample_spec();
        let json = spec.to_json();
        let parsed: Spec = serde_json::from_str(&json).expect("round-trip parse");
        assert_eq!(parsed, spec);
        assert_eq!(parsed.files_to_touch[1].action, FileAction::Modify);
    }

    #[test]
    fn parse_json_handles_fenced_output() {
        let spec = sample_spec();
        let fenced = format!("Here is the spec:\n```json\n{}\n```", spec.to_json());
        let parsed = Spec::parse_json(&fenced).expect("fence-stripped parse");
        assert_eq!(parsed, spec);
    }

    #[test]
    fn parse_json_handles_single_line_fenced_output() {
        let spec = sample_spec();
        let fenced = format!("Here is the spec: ```json {}```", spec.to_json());
        let parsed = Spec::parse_json(&fenced).expect("single-line fence parse");
        assert_eq!(parsed, spec);
    }

    #[test]
    fn file_action_accepts_lowercase_alias() {
        let json = r#"{"title":"T","requirements":[],"files_to_touch":[{"path":"a.rs","action":"create","description":"d"}],"data_models":[],"acceptance_tests":[],"edge_cases":[]}"#;
        let spec = Spec::parse_json(json).expect("lowercase action parse");
        assert_eq!(spec.files_to_touch[0].action, FileAction::Create);
    }

    #[test]
    fn parse_json_rejects_garbage() {
        assert!(Spec::parse_json("definitely not json").is_err());
    }

    #[test]
    fn parse_json_rejects_empty_title() {
        let json = r#"{"title":"  ","requirements":[]}"#;
        assert!(Spec::parse_json(json).is_err());
    }

    #[test]
    fn write_to_creates_parent_dirs() {
        let dir = std::env::temp_dir().join(format!("clawde-spec-test-{}", std::process::id()));
        let path = dir.join("nested").join("spec.json");
        let spec = sample_spec();
        spec.write_to(&path).expect("write spec");
        let on_disk = std::fs::read_to_string(&path).expect("read spec back");
        assert_eq!(Spec::parse_json(&on_disk).expect("parse"), spec);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
