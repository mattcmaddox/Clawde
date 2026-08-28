//! Bounded, privacy-safe persistence for aggregated followup usage.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "followup_usage.json";
const MAX_ENTRIES: usize = 64;
const MAX_TEXT_CHARS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FollowupUsage {
    entries: BTreeMap<String, u32>,
}

impl FollowupUsage {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(FILE_NAME);
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(mut usage) = serde_json::from_str::<Self>(&contents) else {
            return Self::default();
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

    pub fn record(&mut self, text: &str) {
        let text: String = text.trim().chars().take(MAX_TEXT_CHARS).collect();
        if text.is_empty() {
            return;
        }
        if !self.entries.contains_key(&text) && self.entries.len() >= MAX_ENTRIES {
            if let Some(key) = self.entries.keys().next().cloned() {
                self.entries.remove(&key);
            }
        }
        let count = self.entries.entry(text).or_insert(0);
        *count = count.saturating_add(1);
    }

    pub fn summary(&self) -> String {
        let mut rows: Vec<_> = self.entries.iter().collect();
        rows.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
        let mut out = String::new();
        for (text, count) in rows.into_iter().take(5) {
            let escaped = serde_json::to_string(text).unwrap_or_else(|_| "\"?\"".to_string());
            let row = format!(
                "{}{} (used {} times)",
                if out.is_empty() { "" } else { ", " },
                escaped,
                count
            );
            if out.chars().count() + row.chars().count() > 1_500 {
                break;
            }
            out.push_str(&row);
        }
        if out.is_empty() {
            String::new()
        } else {
            format!("The user has previously selected these followups: {out}")
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

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn retain_bounds(&mut self) {
        self.entries
            .retain(|text, _| !text.trim().is_empty() && text.chars().count() <= MAX_TEXT_CHARS);
        while self.entries.len() > MAX_ENTRIES {
            let Some(key) = self.entries.keys().next().cloned() else {
                break;
            };
            self.entries.remove(&key);
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
    fn record_is_bounded_and_summary_is_escaped() {
        let mut usage = FollowupUsage::default();
        for i in 0..70 {
            usage.record(&format!("item {i} \"quoted\"\nnext"));
        }
        assert_eq!(usage.len(), MAX_ENTRIES);
        assert!(!usage.summary().contains('\n'));
        assert!(usage.summary().contains("\\\"quoted\\\""));
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
}
