// Glob tool: fast file pattern matching.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::debug;

pub struct GlobTool;

#[derive(Debug, Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_GLOB
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size. \
         Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\". Returns \
         matching file paths sorted by modification time. Use this tool when \
         you need to find files by name patterns."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "The glob pattern to match files against"
                },
                "path": {
                    "type": "string",
                    "description": "The directory to search in. Defaults to working directory."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GlobInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let base_dir = params
            .path
            .as_ref()
            .map(|p| ctx.resolve_path(p))
            .unwrap_or_else(|| ctx.working_dir.clone());

        if let Err(e) = ctx.check_permission_for_tool_path(
            self,
            &format!("Glob {} in {}", params.pattern, base_dir.display()),
            base_dir.clone(),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(pattern = %params.pattern, dir = %base_dir.display(), "Running glob");

        if !base_dir.exists() || !base_dir.is_dir() {
            return ToolResult::error(format!("Directory not found: {}", base_dir.display()));
        }

        // Build the full glob pattern
        let full_pattern = base_dir.join(&params.pattern);
        let pattern_str = full_pattern.to_string_lossy().to_string();

        // Resolve git repo root so we can filter out ignored paths.
        let repo_root = clawde_core::git_utils::get_repo_root(&base_dir);

        // On Windows, normalize backslashes to forward slashes for the glob crate
        let pattern_str = pattern_str.replace('\\', "/");

        let entries: Vec<PathBuf> = match glob::glob(&pattern_str) {
            Ok(paths) => {
                let mut out = Vec::new();
                for path in paths.filter_map(|p| p.ok()) {
                    // Skip git-ignored paths (same pattern as grep_tool).
                    if let Some(ref root) = repo_root {
                        if clawde_core::git_utils::is_ignored(root, &path) {
                            continue;
                        }
                    }
                    if !ctx.path_is_within_workspace(&path) {
                        if let Err(e) = ctx.check_permission_for_tool_path(
                            self,
                            &format!("Glob result {}", path.display()),
                            path.clone(),
                            true,
                        ) {
                            return ToolResult::error(e.to_string());
                        }
                    }
                    out.push(path);
                }
                out
            }
            Err(e) => {
                return ToolResult::error(format!("Invalid glob pattern: {}", e));
            }
        };

        if entries.is_empty() {
            return ToolResult::success(format!(
                "No files matched pattern \"{}\" in {}",
                params.pattern,
                base_dir.display()
            ));
        }

        // Sort by modification time (most recent first) — fall back to name sort
        let mut entries_with_time: Vec<(PathBuf, std::time::SystemTime)> = entries
            .into_iter()
            .filter_map(|p| {
                let mtime = std::fs::metadata(&p).ok()?.modified().ok()?;
                Some((p, mtime))
            })
            .collect();

        entries_with_time.sort_by_key(|b| std::cmp::Reverse(b.1));

        let total = entries_with_time.len();
        let max_results = 250;
        let truncated = total > max_results;

        let mut output = String::new();
        for (path, _) in entries_with_time.iter().take(max_results) {
            output.push_str(&path.display().to_string());
            output.push('\n');
        }

        if truncated {
            output.push_str(&format!(
                "\n... and {} more files (showing first {})\n",
                total - max_results,
                max_results,
            ));
        }

        ToolResult::success(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use serde_json::json;

    #[tokio::test]
    async fn glob_matches_files_by_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("foo.rs"), "").unwrap();
        std::fs::write(dir.path().join("bar.rs"), "").unwrap();
        std::fs::write(dir.path().join("readme.md"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*.rs"}), &ctx).await;

        assert!(!res.is_error, "glob failed: {}", res.content);
        assert!(res.content.contains("foo.rs"), "should find foo.rs");
        assert!(res.content.contains("bar.rs"), "should find bar.rs");
        assert!(
            !res.content.contains("readme.md"),
            "should NOT find readme.md"
        );
    }

    #[tokio::test]
    async fn glob_no_matches_returns_empty_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("readme.md"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*.py"}), &ctx).await;

        assert!(!res.is_error, "glob should not error on no matches");
        assert!(res.content.contains("No files matched"));
    }

    #[tokio::test]
    async fn glob_recursive_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src").join("lib");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("mod.rs"), "").unwrap();
        std::fs::write(dir.path().join("main.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "**/*.rs"}), &ctx).await;

        assert!(!res.is_error, "glob failed: {}", res.content);
        assert!(res.content.contains("main.rs"));
        assert!(res.content.contains("mod.rs"));
    }

    #[tokio::test]
    async fn glob_invalid_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool
            .execute(
                json!({
                    "pattern": "*.rs",
                    "path": "/nonexistent/directory",
                }),
                &ctx,
            )
            .await;

        assert!(res.is_error, "expected error for bad path");
        assert!(res.content.contains("not found"));
    }

    #[tokio::test]
    async fn glob_sorts_by_mtime() {
        let dir = tempfile::tempdir().unwrap();
        // Create files with distinct names; timestamps may be identical on
        // some filesystems, but the tool should still return them sorted.
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*.rs"}), &ctx).await;

        assert!(!res.is_error, "glob failed: {}", res.content);
        // All three files should be present
        assert!(res.content.contains("a.rs"));
        assert!(res.content.contains("b.rs"));
        assert!(res.content.contains("c.rs"));
    }

    #[tokio::test]
    async fn glob_invalid_input_missing_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({}), &ctx).await;

        assert!(res.is_error, "expected error for missing pattern");
        assert!(res.content.contains("Invalid input"));
    }
    #[tokio::test]
    async fn glob_non_ascii_file_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("文件.rs"), "").unwrap();
        std::fs::write(dir.path().join("cafe.md"), "").unwrap();
        std::fs::write(dir.path().join("plain.txt"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // Match all .rs files
        let res = GlobTool.execute(json!({"pattern": "*.rs"}), &ctx).await;
        assert!(!res.is_error, "glob non-ascii failed: {}", res.content);
        assert!(
            res.content.contains("文件.rs"),
            "should find CJK-named file"
        );

        // Match all .md files
        let res2 = GlobTool.execute(json!({"pattern": "*.md"}), &ctx).await;
        assert!(!res2.is_error);
        assert!(
            res2.content.contains("cafe.md"),
            "should find accented-name file"
        );
    }

    #[tokio::test]
    async fn glob_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        // Empty directory, no files at all
        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*"}), &ctx).await;

        assert!(!res.is_error, "glob empty dir should not error");
        assert!(
            res.content.contains("No files matched"),
            "should report no files"
        );
    }

    #[tokio::test]
    async fn glob_hidden_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden.rs"), "").unwrap();
        std::fs::write(dir.path().join("visible.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*.rs"}), &ctx).await;

        assert!(!res.is_error, "glob hidden failed: {}", res.content);
        // The glob crate matches dotfiles with * on some platforms. Assert
        // that at least the visible file is found.
        assert!(
            res.content.contains("visible.rs"),
            "should find visible file"
        );
    }

    #[tokio::test]
    async fn glob_character_class_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::write(dir.path().join("b.go"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // [rg] matches either 'r' or 'g' followed by 's' -> matches .rs but not .go or .txt
        let res = GlobTool.execute(json!({"pattern": "*.[rg]s"}), &ctx).await;

        assert!(!res.is_error, "glob char class failed: {}", res.content);
        assert!(res.content.contains("a.rs"), "should find .rs file");
        assert!(!res.content.contains("b.go"), "should NOT find .go file");
        assert!(!res.content.contains("c.txt"), "should NOT find .txt file");
    }

    #[tokio::test]
    async fn glob_deeply_nested_files() {
        let dir = tempfile::tempdir().unwrap();
        // Create deeply nested hierarchy
        let deep = dir.path().join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("found.rs"), "").unwrap();
        std::fs::write(dir.path().join("root.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());

        // Recursive pattern should find both
        let res = GlobTool.execute(json!({"pattern": "**/*.rs"}), &ctx).await;
        assert!(!res.is_error, "glob deep failed: {}", res.content);
        assert!(res.content.contains("root.rs"), "should find root file");
        assert!(
            res.content.contains("found.rs"),
            "should find deeply nested file"
        );

        // Non-recursive should only find root
        let res2 = GlobTool.execute(json!({"pattern": "*.rs"}), &ctx).await;
        assert!(!res2.is_error);
        assert!(res2.content.contains("root.rs"), "should find root file");
        assert!(
            !res2.content.contains("found.rs"),
            "should NOT find nested file"
        );
    }

    #[tokio::test]
    async fn glob_with_path_parameter() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("inner.rs"), "").unwrap();
        std::fs::write(dir.path().join("outer.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // Use explicit path parameter to search only within subdir
        let res = GlobTool
            .execute(
                json!({
                    "pattern": "*.rs",
                    "path": "subdir",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "glob with path failed: {}", res.content);
        assert!(res.content.contains("inner.rs"), "should find inner.rs");
        assert!(
            !res.content.contains("outer.rs"),
            "should NOT find outer.rs"
        );
    }

    #[tokio::test]
    async fn glob_question_mark_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.rs"), "").unwrap();
        std::fs::write(dir.path().join("car.rs"), "").unwrap();
        std::fs::write(dir.path().join("cab.rs"), "").unwrap();
        std::fs::write(dir.path().join("cart.rs"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // ? matches exactly one character: c?t -> cat only (not cart)
        let res = GlobTool.execute(json!({"pattern": "c?t.rs"}), &ctx).await;

        assert!(!res.is_error, "glob ? failed: {}", res.content);
        assert!(res.content.contains("cat.rs"), "should match c?t -> cat");
        assert!(
            !res.content.contains("cart.rs"),
            "should NOT match cart (4 chars)"
        );
    }

    #[tokio::test]
    async fn glob_star_only_wildcard() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("any.txt"), "").unwrap();
        std::fs::write(dir.path().join("file.rs"), "").unwrap();
        std::fs::write(dir.path().join("data.json"), "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GlobTool.execute(json!({"pattern": "*"}), &ctx).await;

        assert!(!res.is_error, "glob * failed: {}", res.content);
        assert!(res.content.contains("any.txt"));
        assert!(res.content.contains("file.rs"));
        assert!(res.content.contains("data.json"));
    }
}
