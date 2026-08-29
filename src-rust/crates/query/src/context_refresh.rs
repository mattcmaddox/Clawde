// Auto-context-refresh for files modified externally.
//
// When a file is modified outside the agent (e.g., git pull, another process),
// we detect the change and refresh the context to avoid stale information.
// Based on Aider's file watcher pattern.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::debug;

/// Tracks file modification times for change detection.
/// Based on Aider's FileWatcher pattern.
#[derive(Debug, Default)]
pub struct FileModificationTracker {
    /// Map of file path to last known modification time.
    modifications: HashMap<PathBuf, SystemTime>,
}

impl FileModificationTracker {
    /// Create a new tracker.
    pub fn new() -> Self {
        Self {
            modifications: HashMap::new(),
        }
    }

    /// Record the current modification time of a file.
    pub fn record_file(&mut self, path: &Path) {
        if let Ok(metadata) = std::fs::metadata(path) {
            if let Ok(modified) = metadata.modified() {
                self.modifications.insert(path.to_path_buf(), modified);
            }
        }
    }

    /// Check if a file has been modified since it was last recorded.
    pub fn is_modified(&self, path: &Path) -> bool {
        if let Ok(metadata) = std::fs::metadata(path) {
            if let Ok(current_modified) = metadata.modified() {
                if let Some(last_known) = self.modifications.get(path) {
                    return current_modified > *last_known;
                }
            }
        }
        false
    }

    /// Update the recorded modification time for a file after refreshing.
    pub fn update_file(&mut self, path: &Path) {
        self.record_file(path);
    }
}

/// Check if any files in the context have been modified externally.
pub fn check_for_external_modifications(
    tracker: &FileModificationTracker,
    context_files: &[PathBuf],
) -> Vec<PathBuf> {
    context_files
        .iter()
        .filter(|path| tracker.is_modified(path))
        .cloned()
        .collect()
}

/// Refresh a file in context by re-reading it.
pub async fn refresh_file_in_context(path: &Path) -> anyhow::Result<String> {
    let content = tokio::fs::read_to_string(path).await?;
    debug!(path = %path.display(), "Refreshed file in context");
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_file_modification_tracker() {
        let temp_dir = TempDir::new().unwrap();
        let test_file = temp_dir.path().join("test.txt");

        // Create a file
        let mut file = File::create(&test_file).unwrap();
        writeln!(file, "initial content").unwrap();

        let mut tracker = FileModificationTracker::new();
        tracker.record_file(&test_file);

        // File should not be modified yet
        assert!(!tracker.is_modified(&test_file));

        // Modify the file
        std::thread::sleep(std::time::Duration::from_millis(10));
        let mut file = File::create(&test_file).unwrap();
        writeln!(file, "modified content").unwrap();

        // File should now be detected as modified
        assert!(tracker.is_modified(&test_file));
    }
}
