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

    /// List the parseable spec JSON files in `dir/specs/`, newest-first by
    /// modification time.
    ///
    /// Used by the spec review dialog's picker (several specs) and by
    /// [`Spec::latest_in`]. Entries that cannot be read, have no usable
    /// mtime, or fail to parse are skipped — never aborting the scan — so a
    /// broken spec never hides the valid ones around it. Returns an empty
    /// vec when the `specs/` directory is absent or holds nothing usable.
    pub fn list_specs(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let specs_dir = dir.join("specs");
        let entries = match std::fs::read_dir(&specs_dir) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };
        // Collect every spec JSON with its mtime; unreadable entries are
        // skipped (never abort the scan).
        let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(metadata) = entry.metadata() {
                if let Ok(modified) = metadata.modified() {
                    candidates.push((path, modified));
                }
            }
        }
        // Newest first, dropping anything that fails to parse so every
        // returned path opens cleanly in the review dialog.
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates
            .into_iter()
            .filter_map(|(path, _)| {
                let raw = std::fs::read_to_string(&path).ok()?;
                Spec::parse_json(&raw).ok()?;
                Some(path)
            })
            .collect()
    }

    /// Load the most recently modified spec from `dir/specs/*.json`, if any.
    ///
    /// Used by the spec-mode continuation policy and the verify loop to find
    /// the spec currently under review. `None` when the `specs/` directory
    /// does not exist, holds no `.json` files, or none of them parse. A single
    /// unreadable or unparseable entry never aborts the scan — it is skipped.
    pub fn latest_in(dir: &std::path::Path) -> Option<(std::path::PathBuf, Spec)> {
        let path = Spec::list_specs(dir).into_iter().next()?;
        // list_specs only returns parseable files, so re-reading cannot fail
        // on a race-free filesystem; skip on any IO error regardless.
        let raw = std::fs::read_to_string(&path).ok()?;
        let spec = Spec::parse_json(&raw).ok()?;
        Some((path, spec))
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
    fn latest_in_picks_newest_parsable_spec() {
        let dir = std::env::temp_dir().join(format!("clawde-latest-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        let mut old = sample_spec();
        old.title = "Old Spec".to_string();
        old.write_to(&dir.join("specs/old.json")).unwrap();
        // Ensure a distinct, later mtime even on coarse-granularity
        // filesystems so the recency selection is deterministic.
        std::thread::sleep(std::time::Duration::from_millis(10));
        // Newer spec, different title — must win by mtime.
        let mut fresh = sample_spec();
        fresh.title = "Fresh Spec".to_string();
        fresh.write_to(&dir.join("specs/fresh.json")).unwrap();

        let (path, spec) = Spec::latest_in(&dir).expect("latest spec");
        assert!(path.ends_with("fresh.json"), "path: {}", path.display());
        assert_eq!(spec.title, "Fresh Spec");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_in_skips_broken_newest_and_falls_back() {
        let dir = std::env::temp_dir().join(format!("clawde-latest-fb-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        sample_spec()
            .write_to(&dir.join("specs/valid.json"))
            .unwrap();
        // A broken spec that is (by mtime) the newest — must be skipped in
        // favour of the valid older one.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.join("specs/broken.json"), "not json").unwrap();

        let (path, spec) = Spec::latest_in(&dir).expect("fallback to valid spec");
        assert!(path.ends_with("valid.json"), "path: {}", path.display());
        assert_eq!(spec.title, "Rate-Limiting Middleware");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_specs_returns_newest_first_and_skips_broken() {
        let dir = std::env::temp_dir().join(format!("clawde-list-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("specs")).unwrap();
        // A non-JSON file and a broken JSON file must both be skipped.
        std::fs::write(dir.join("specs/notes.txt"), "not a spec").unwrap();
        std::fs::write(dir.join("specs/broken.json"), "not json").unwrap();
        let mut first = sample_spec();
        first.title = "First Spec".to_string();
        first.write_to(&dir.join("specs/first.json")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut second = sample_spec();
        second.title = "Second Spec".to_string();
        second.write_to(&dir.join("specs/second.json")).unwrap();

        let specs = Spec::list_specs(&dir);
        assert_eq!(specs.len(), 2, "broken and non-JSON entries skipped");
        // Newest by mtime first.
        assert!(specs[0].ends_with("second.json"));
        assert!(specs[1].ends_with("first.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_specs_empty_without_specs_dir() {
        let dir = std::env::temp_dir().join(format!("clawde-list-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Spec::list_specs(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_in_returns_none_without_specs_dir() {
        let dir = std::env::temp_dir().join(format!("clawde-latest-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Spec::latest_in(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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
