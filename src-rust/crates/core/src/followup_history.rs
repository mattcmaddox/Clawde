//! Bounded durable storage for displayed followup suggestions.

use crate::RankedFollowup;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "followup_history.json";
const MAX_ITEMS: usize = 20;
const MAX_TEXT_CHARS: usize = 512;
const MAX_REASON_CHARS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct FollowupHistory {
    items: VecDeque<RankedFollowup>,
}

impl FollowupHistory {
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(FILE_NAME);
        let Ok(contents) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        let Ok(mut history) = serde_json::from_str::<Self>(&contents) else {
            return Self::default();
        };
        history.retain_bounds();
        history
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

    pub fn items(&self) -> &VecDeque<RankedFollowup> {
        &self.items
    }

    pub fn insert(&mut self, followup: &RankedFollowup) {
        let text: String = followup.text.trim().chars().take(MAX_TEXT_CHARS).collect();
        if text.is_empty() {
            return;
        }
        let reason: String = followup
            .reason
            .trim()
            .chars()
            .take(MAX_REASON_CHARS)
            .collect();
        let normalized = RankedFollowup {
            text,
            rank: followup.rank,
            reason,
        };
        if let Some(index) = self
            .items
            .iter()
            .position(|item| item.text == normalized.text)
        {
            let existing = self
                .items
                .remove(index)
                .unwrap_or_else(|| normalized.clone());
            let rank = if existing.rank.order() <= normalized.rank.order() {
                existing.rank
            } else {
                normalized.rank
            };
            let reason = if normalized.reason.is_empty() {
                existing.reason
            } else {
                normalized.reason.clone()
            };
            self.items.push_back(RankedFollowup {
                text: normalized.text,
                rank,
                reason,
            });
        } else {
            if self.items.len() >= MAX_ITEMS {
                self.items.pop_front();
            }
            self.items.push_back(normalized);
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    pub fn save(&self, config_dir: &Path) -> anyhow::Result<()> {
        let path = config_dir.join(FILE_NAME);
        std::fs::create_dir_all(config_dir)?;
        let tmp = temporary_path(&path);
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    fn retain_bounds(&mut self) {
        self.items.retain(|item| {
            !item.text.trim().is_empty()
                && item.text.chars().count() <= MAX_TEXT_CHARS
                && item.reason.chars().count() <= MAX_REASON_CHARS
        });
        while self.items.len() > MAX_ITEMS {
            self.items.pop_front();
        }
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

/// Directory holding project-scoped followup data: `<project_root>/.clawde/`,
/// falling back to the global config dir when no project root is known. This
/// mirrors the per-project `.clawde/` convention already used for custom modes
/// and memory dirs, so followup data never leaks across projects.
pub fn followup_data_dir(project_root: Option<&Path>) -> PathBuf {
    match project_root {
        Some(root) => root.join(".clawde"),
        None => crate::config::Settings::config_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn item(text: &str, rank: FollowupRank, reason: &str) -> RankedFollowup {
        RankedFollowup {
            text: text.into(),
            rank,
            reason: reason.into(),
        }
    }

    use crate::FollowupRank;

    #[test]
    fn persists_and_recovers_bounded_history() {
        let dir = tempdir().expect("tempdir");
        let mut history = FollowupHistory::default();
        for index in 0..25 {
            history.insert(&item(&format!("item {index}"), FollowupRank::Optional, ""));
        }
        history.save(dir.path()).expect("save");
        let loaded = FollowupHistory::load(dir.path());
        assert_eq!(loaded.items().len(), MAX_ITEMS);
        assert_eq!(
            loaded.items().front().map(|item| item.text.as_str()),
            Some("item 5")
        );
    }

    #[test]
    fn duplicate_keeps_highest_rank() {
        let mut history = FollowupHistory::default();
        history.insert(&item(
            "run tests",
            FollowupRank::HighlyRecommended,
            "verify",
        ));
        history.insert(&item("run tests", FollowupRank::Undesired, "later"));
        assert_eq!(history.items()[0].rank, FollowupRank::HighlyRecommended);
        assert_eq!(history.items()[0].reason, "later");
    }

    #[test]
    fn load_preferring_falls_back_to_legacy_and_migrates_one_way() {
        let primary = tempdir().unwrap();
        let legacy = tempdir().unwrap();
        // Legacy-only: load falls back to the global file.
        let mut history = FollowupHistory::default();
        history.insert(&item("legacy item", FollowupRank::Optional, ""));
        history.save(legacy.path()).unwrap();
        let loaded = FollowupHistory::load_preferring(primary.path(), legacy.path());
        assert_eq!(loaded.items().len(), 1);
        assert_eq!(loaded.items()[0].text, "legacy item");
        // Saving migrates to the project dir and removes the legacy file.
        loaded
            .save_migrating(primary.path(), legacy.path())
            .unwrap();
        assert!(primary.path().join(FILE_NAME).exists());
        assert!(!legacy.path().join(FILE_NAME).exists());
        // The project file now wins on subsequent loads.
        let again = FollowupHistory::load_preferring(primary.path(), legacy.path());
        assert_eq!(again.items().len(), 1);
    }

    #[test]
    fn load_preferring_prefers_an_existing_project_file() {
        let primary = tempdir().unwrap();
        let legacy = tempdir().unwrap();
        let mut history = FollowupHistory::default();
        history.insert(&item("project item", FollowupRank::Optional, ""));
        history.save(primary.path()).unwrap();
        let mut legacy_history = FollowupHistory::default();
        legacy_history.insert(&item("legacy item", FollowupRank::Optional, ""));
        legacy_history.save(legacy.path()).unwrap();
        let loaded = FollowupHistory::load_preferring(primary.path(), legacy.path());
        assert_eq!(loaded.items().len(), 1);
        assert_eq!(loaded.items()[0].text, "project item");
    }

    #[test]
    fn followup_data_dir_resolves_per_project_and_global_fallback() {
        let project = tempdir().unwrap();
        assert_eq!(
            followup_data_dir(Some(project.path())),
            project.path().join(".clawde")
        );
        assert_eq!(
            followup_data_dir(None),
            crate::config::Settings::config_dir()
        );
    }
}
