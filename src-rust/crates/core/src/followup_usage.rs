//! Bounded, privacy-safe persistence for aggregated followup usage.
//!
//! Schema V2: per-text lifecycle counts (`selected` / `submitted` /
//! `completed`). Legacy V1 files (a plain `text -> count` map of selections)
//! are migrated on load; the next save writes V2.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "followup_usage.json";
const MAX_ENTRIES: usize = 64;
const MAX_TEXT_CHARS: usize = 256;
const SCHEMA_VERSION: u32 = 2;

/// Lifecycle counts for a single followup text: how often it was selected,
/// submitted as a prompt, and completed with successful assistant output.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FollowupLifecycle {
    pub selected: u32,
    pub submitted: u32,
    pub completed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FollowupUsage {
    version: u32,
    entries: BTreeMap<String, FollowupLifecycle>,
}

/// On-disk shapes across schema versions, tried in order by serde.
#[derive(Deserialize)]
#[serde(untagged)]
enum UsageFile {
    V2 {
        version: u32,
        entries: BTreeMap<String, FollowupLifecycle>,
    },
    V1 {
        entries: BTreeMap<String, u32>,
    },
}

impl FollowupUsage {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(FILE_NAME);
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(file) = serde_json::from_str::<UsageFile>(&contents) else {
            return Self::default();
        };
        let mut usage = match file {
            // `version` is carried on the wire for forward evolution; the
            // current reader accepts only the shapes declared above.
            UsageFile::V2 { entries, version } => {
                let _ = version;
                Self {
                    version: SCHEMA_VERSION,
                    entries,
                }
            }
            // V1 only tracked selections; submitted/completed start at zero.
            UsageFile::V1 { entries } => Self {
                version: SCHEMA_VERSION,
                entries: entries
                    .into_iter()
                    .map(|(text, selected)| {
                        (
                            text,
                            FollowupLifecycle {
                                selected,
                                ..Default::default()
                            },
                        )
                    })
                    .collect(),
            },
        };
        usage.retain_bounds();
        usage
    }

    /// Load preferring the project-scoped `primary_dir`, falling back to the
    /// legacy global `fallback_dir` when no project file exists yet (one-way
    /// migration; the legacy file is removed on the next successful save).
    pub fn load_preferring(primary_dir: &Path, fallback_dir: &Path) -> Self {
        if primary_dir.join(FILE_NAME).exists() {
            Self::load(primary_dir)
        } else {
            Self::load(fallback_dir)
        }
    }

    /// Save to `primary_dir` and, on success, remove the legacy global file.
    pub fn save_migrating(&self, primary_dir: &Path, legacy_dir: &Path) -> anyhow::Result<()> {
        self.save(primary_dir)?;
        let legacy = legacy_dir.join(FILE_NAME);
        if legacy.exists() {
            let _ = std::fs::remove_file(legacy);
        }
        Ok(())
    }

    /// Record a followup selection. Bounded: when the map is full the
    /// least-used entry is evicted.
    pub fn record(&mut self, text: &str) {
        let text = self.normalize_text(text);
        if text.is_empty() {
            return;
        }
        self.ensure_capacity(&text);
        let lifecycle = self.entries.entry(text).or_default();
        lifecycle.selected = lifecycle.selected.saturating_add(1);
    }

    /// Record that a selected followup was submitted as a prompt.
    pub fn record_submitted(&mut self, text: &str) {
        let text = self.normalize_text(text);
        if text.is_empty() {
            return;
        }
        self.ensure_capacity(&text);
        let lifecycle = self.entries.entry(text).or_default();
        lifecycle.submitted = lifecycle.submitted.saturating_add(1);
    }

    /// Record that a submitted followup's turn completed with output.
    pub fn record_completed(&mut self, text: &str) {
        let text = self.normalize_text(text);
        if text.is_empty() {
            return;
        }
        self.ensure_capacity(&text);
        let lifecycle = self.entries.entry(text).or_default();
        lifecycle.completed = lifecycle.completed.saturating_add(1);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Lifecycle counts for a followup text, or `None` when never tracked.
    pub fn lifecycle_for(&self, text: &str) -> Option<&FollowupLifecycle> {
        self.entries.get(text)
    }

    /// Iterate `(text, lifecycle)` pairs ordered by selected count desc, then
    /// text asc — the ordering used by the summary, markdown mirror, and
    /// status report.
    pub fn sorted(&self) -> Vec<(&String, &FollowupLifecycle)> {
        let mut rows: Vec<_> = self.entries.iter().collect();
        rows.sort_by(|(a, al), (b, bl)| bl.selected.cmp(&al.selected).then_with(|| a.cmp(b)));
        rows
    }

    /// Human-readable one-line summary for system-prompt insertion: top texts
    /// with all three lifecycle counts. JSON-escaped and newline-free so the
    /// prompt cannot be broken out of by model-generated text.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for (text, lifecycle) in self.sorted().into_iter().take(5) {
            let escaped = serde_json::to_string(text).unwrap_or_else(|_| "\"?\"".to_string());
            let row = format!(
                "{}{} (selected {}, submitted {}, completed {})",
                if out.is_empty() { "" } else { ", " },
                escaped,
                lifecycle.selected,
                lifecycle.submitted,
                lifecycle.completed
            );
            if out.chars().count() + row.chars().count() > 1_500 {
                break;
            }
            out.push_str(&row);
        }
        if out.is_empty() {
            String::new()
        } else {
            format!("The user has previously acted on these followups: {out}")
        }
    }

    pub fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        let path = config_dir.join(FILE_NAME);
        std::fs::create_dir_all(config_dir)?;
        let tmp = temporary_path(&path);
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&tmp, contents)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn normalize_text(&self, text: &str) -> String {
        text.trim().chars().take(MAX_TEXT_CHARS).collect()
    }

    fn ensure_capacity(&mut self, text: &str) {
        if !self.entries.contains_key(text) && self.entries.len() >= MAX_ENTRIES {
            if let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, lifecycle)| lifecycle.selected)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&key);
            }
        }
    }

    fn retain_bounds(&mut self) {
        self.entries
            .retain(|text, _| !text.trim().is_empty() && text.chars().count() <= MAX_TEXT_CHARS);
        while self.entries.len() > MAX_ENTRIES {
            if let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(_, lifecycle)| lifecycle.selected)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&key);
            } else {
                break;
            }
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_counts_are_bounded_and_summary_is_escaped() {
        let mut usage = FollowupUsage::default();
        usage.record("item 0 \"quoted\"\nnext");
        usage.record("item 0 \"quoted\"\nnext");
        for i in 1..70 {
            usage.record(&format!("item {i} \"quoted\"\nnext"));
        }
        usage.record_submitted("item 0 \"quoted\"\nnext");
        usage.record_completed("item 0 \"quoted\"\nnext");
        assert_eq!(usage.len(), MAX_ENTRIES);
        let summary = usage.summary();
        assert!(!summary.contains('\n'));
        assert!(summary.contains("\\\"quoted\\\""));
        assert!(summary.contains("selected 2"));
        assert!(summary.contains("submitted 1"));
        assert!(summary.contains("completed 1"));
        assert_eq!(
            usage.lifecycle_for("item 0 \"quoted\"\nnext").unwrap(),
            &FollowupLifecycle {
                selected: 2,
                submitted: 1,
                completed: 1
            }
        );
    }

    #[test]
    fn v1_files_migrate_selections_into_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        // Legacy V1 shape: `{"entries": {"text": count}}`.
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"entries": {"run tests": 3}}"#,
        )
        .unwrap();
        let usage = FollowupUsage::load(dir.path());
        assert_eq!(
            usage.lifecycle_for("run tests").unwrap(),
            &FollowupLifecycle {
                selected: 3,
                submitted: 0,
                completed: 0
            }
        );
        // Saving rewrites as V2.
        usage.save(dir.path()).unwrap();
        let contents = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(contents.contains("\"version\": 2"));
        assert!(contents.contains("\"submitted\": 0"));
    }

    #[test]
    fn load_preferring_migrates_legacy_usage_one_way() {
        let primary = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let mut usage = FollowupUsage::default();
        usage.record("legacy usage");
        usage.save(legacy.path()).unwrap();
        let loaded = FollowupUsage::load_preferring(primary.path(), legacy.path());
        assert_eq!(loaded.len(), 1);
        loaded
            .save_migrating(primary.path(), legacy.path())
            .unwrap();
        assert!(primary.path().join(FILE_NAME).exists());
        assert!(!legacy.path().join(FILE_NAME).exists());
    }

    #[test]
    fn full_map_evicts_least_used_entry() {
        let mut usage = FollowupUsage::default();
        usage.record("popular");
        usage.record("popular");
        for i in 0..(MAX_ENTRIES + 1) {
            usage.record(&format!("filler {i}"));
        }
        assert_eq!(usage.len(), MAX_ENTRIES);
        // "popular" was selected twice; the single-use fillers compete and one
        // of them is evicted, while "popular" survives.
        assert!(usage.lifecycle_for("popular").is_some());
    }
}
