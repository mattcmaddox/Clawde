//! Project registry: maps a board project name to the git repository the
//! board's cards run against.
//!
//! A board is keyed by a project *name* (`--project NAME`, default `default`),
//! but the runner can't make a git worktree without knowing which repository
//! that name refers to. This small registry persists `name -> repo_root` in
//! `~/.clawde/katban/projects.json` (spec §16a E1: Project and Site are
//! distinct — a Site needs no repo, a board Project is always a repo).
//!
//! A project may have no repo registered yet — the board still works for
//! planning (cards, deps, status) and the web UI. Only the runner (`clawde
//! katban run`) consults this mapping and skips card execution for projects
//! without one, leaving the cards in **queued**. Registration is a one-line
//! command (`clawde katban project set <NAME> <DIR>`), so wiring a board to a
//! repo is explicit, not guessed from cwd.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRegistry {
    /// project name (as used on the board) -> canonical repo root.
    #[serde(default)]
    pub projects: BTreeMap<String, String>,
}

pub fn registry_path() -> PathBuf {
    crate::config::katban_data_dir().join("projects.json")
}

pub fn load() -> anyhow::Result<ProjectRegistry> {
    let path = registry_path();
    if !path.exists() {
        return Ok(ProjectRegistry::default());
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let registry: ProjectRegistry =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(registry)
}

pub fn save(registry: &ProjectRegistry) -> anyhow::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::caddy::write_atomic(&path, &serde_json::to_string_pretty(registry)?)?;
    Ok(())
}

/// Canonical repo root for a project, if it is registered. Resolves the stored
/// path (relative entries are rooted at cwd at registration time, so this
/// checks it still exists).
pub fn repo_root(project: &str) -> Option<PathBuf> {
    let registry = load().ok()?;
    let stored = registry.projects.get(project)?;
    let path = PathBuf::from(stored);
    // Store canonical absolute paths so the mapping never depends on the cwd
    // of a later process. If it isn't absolute (older entry), try cwd-relative.
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    };
    path.canonicalize().ok().filter(|p| p.is_dir())
}

/// Register (or re-point) a project's repo root. Returns an error when the
/// target is not a directory — we refuse to wire a board to a nonexistent
/// path, since the runner would fail every spawn.
pub fn set_repo_root(project: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let canon = dir
        .canonicalize()
        .with_context(|| format!("repo dir does not exist: {}", dir.display()))?;
    if !canon.is_dir() {
        anyhow::bail!("repo dir is not a directory: {}", canon.display());
    }
    let project = project.trim().to_string();
    if project.is_empty() {
        anyhow::bail!("project name must not be empty");
    }
    // Refuse a project name that would collide with another under the lossless
    // dir encoding (mirrors the board's own collision guard).
    if crate::board::project_dir_name(&project) == "project" && project != "project" {
        anyhow::bail!("project name '{project}' maps to an unsafe board dir");
    }
    let mut registry = load()?;
    registry
        .projects
        .insert(project, canon.to_string_lossy().into_owned());
    save(&registry)?;
    Ok(canon)
}

/// Project names that have a registered repo (those the runner can execute).
pub fn registered_projects() -> Vec<String> {
    load()
        .map(|registry| registry.projects.keys().cloned().collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("CLAWDE_HOME").ok();
        std::env::set_var("CLAWDE_HOME", dir);
        let result = f();
        match previous {
            Some(value) => std::env::set_var("CLAWDE_HOME", value),
            None => std::env::remove_var("CLAWDE_HOME"),
        }
        result
    }

    #[test]
    fn default_registry_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            assert!(load().unwrap().projects.is_empty());
            assert!(repo_root("default").is_none());
        });
    }

    #[test]
    fn set_and_resolve_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let canon = set_repo_root("demo", repo.path()).unwrap();
            assert_eq!(repo_root("demo").unwrap(), canon);
            // Unregistered projects resolve to nothing.
            assert!(repo_root("other").is_none());
            // Relative dirs with cwd indirection still resolve via absolute
            // storage? Not needed by tests; registry stores absolute.
        });
    }

    #[test]
    fn refuses_nonexistent_repo_dir() {
        let tmp = tempfile::tempdir().unwrap();
        with_home(tmp.path(), || {
            let missing = tmp.path().join("nope");
            assert!(set_repo_root("demo", &missing).is_err());
            assert!(repo_root("demo").is_none());
        });
    }
}
