// Grep tool: content search with ripgrep-style options.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use tracing::debug;
use walkdir::WalkDir;

pub struct GrepTool;

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "type")]
    file_type: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default = "default_output_mode")]
    output_mode: String,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default, rename = "-i")]
    case_insensitive: bool,
    #[serde(default, rename = "-n")]
    show_line_numbers: Option<bool>,
    #[serde(default)]
    head_limit: Option<usize>,
    #[serde(default)]
    multiline: bool,
}

fn default_output_mode() -> String {
    "files_with_matches".to_string()
}

/// Map file type shorthand to extensions (similar to ripgrep --type).
fn extensions_for_type(t: &str) -> Vec<&'static str> {
    match t {
        "rust" | "rs" => vec!["rs"],
        "js" => vec!["js", "jsx", "mjs", "cjs"],
        "ts" => vec!["ts", "tsx", "mts", "cts"],
        "py" | "python" => vec!["py", "pyi"],
        "go" => vec!["go"],
        "java" => vec!["java"],
        "c" => vec!["c", "h"],
        "cpp" => vec!["cpp", "hpp", "cc", "hh", "cxx"],
        "rb" | "ruby" => vec!["rb"],
        "php" => vec!["php"],
        "swift" => vec!["swift"],
        "kt" | "kotlin" => vec!["kt", "kts"],
        "css" => vec!["css", "scss", "sass", "less"],
        "html" => vec!["html", "htm"],
        "json" => vec!["json"],
        "yaml" | "yml" => vec!["yaml", "yml"],
        "toml" => vec!["toml"],
        "xml" => vec!["xml"],
        "md" | "markdown" => vec!["md", "markdown"],
        "sh" | "shell" | "bash" => vec!["sh", "bash", "zsh"],
        _ => vec![],
    }
}

#[async_trait]
impl Tool for GrepTool {
    // Gates itself: calls `ctx.check_permission_for_path` in `execute()` (#210).
    fn self_gates(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_GREP
    }

    fn description(&self) -> &str {
        "A powerful search tool built on regex. Supports full regex syntax. \
         Filter files with the `glob` parameter or `type` parameter. Output \
         modes: \"content\" shows matching lines, \"files_with_matches\" shows \
         only file paths (default), \"count\" shows match counts."
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
                    "description": "The regular expression pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search in. Defaults to working directory."
                },
                "type": {
                    "type": "string",
                    "description": "File type to search (e.g. js, py, rust, go)"
                },
                "glob": {
                    "type": "string",
                    "description": "Glob pattern to filter files (e.g. \"*.js\")"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode (default: files_with_matches)"
                },
                "context": {
                    "type": "number",
                    "description": "Number of context lines before and after each match"
                },
                "-i": {
                    "type": "boolean",
                    "description": "Case insensitive search"
                },
                "-n": {
                    "type": "boolean",
                    "description": "Show line numbers (for content mode)"
                },
                "head_limit": {
                    "type": "number",
                    "description": "Limit output to first N entries (default 250)"
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable multiline mode where . matches newlines"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: GrepInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let search_path = params
            .path
            .as_ref()
            .map(|p| ctx.resolve_path(p))
            .unwrap_or_else(|| ctx.working_dir.clone());

        if let Err(e) = ctx.check_permission_for_tool_path(
            self,
            &format!("Grep {} in {}", params.pattern, search_path.display()),
            search_path.clone(),
            true,
        ) {
            return ToolResult::error(e.to_string());
        }

        debug!(pattern = %params.pattern, path = %search_path.display(), "Running grep");

        // Compile regex
        let regex = match RegexBuilder::new(&params.pattern)
            .case_insensitive(params.case_insensitive)
            .dot_matches_new_line(params.multiline)
            .multi_line(params.multiline)
            .build()
        {
            Ok(r) => r,
            Err(e) => return ToolResult::error(format!("Invalid regex: {}", e)),
        };

        let head_limit = params.head_limit.unwrap_or(250);
        let context_lines = params.context.unwrap_or(0);
        let show_line_numbers = params.show_line_numbers.unwrap_or(true);

        // Collect candidate file extensions
        let type_exts: Vec<&str> = params
            .file_type
            .as_deref()
            .map(extensions_for_type)
            .unwrap_or_default();

        // Build glob matcher for filtering
        let glob_pattern = params.glob.as_deref();

        // If the search path is a single file, just search it.
        if search_path.is_file() {
            return self.search_file(
                &search_path,
                &regex,
                &params.output_mode,
                context_lines,
                show_line_numbers,
            );
        }

        // Walk directory tree
        let mut results: Vec<String> = Vec::new();
        let mut match_count = 0usize;

        // Resolve the git repo root so we can skip ignored directories.
        // Only consulted when inside a git worktree — avoids wasted process
        // spawns in non-git workspaces.
        let repo_root = clawde_core::git_utils::get_repo_root(&search_path);

        for entry in WalkDir::new(&search_path)
            .follow_links(true)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                if name.starts_with('.')
                    || name == "node_modules"
                    || name == "target"
                    || name == "__pycache__"
                    || name == ".git"
                {
                    return false;
                }
                // Skip git-ignored directories so we don't descend into
                // e.g. dist/, build/, etc.
                if let Some(ref root) = repo_root {
                    if e.file_type().is_dir() && clawde_core::git_utils::is_ignored(root, e.path())
                    {
                        return false;
                    }
                }
                true
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let path = entry.path();

            if !ctx.path_is_within_workspace(path) {
                if let Err(e) = ctx.check_permission_for_tool_path(
                    self,
                    &format!("Grep result {}", path.display()),
                    path.to_path_buf(),
                    true,
                ) {
                    return ToolResult::error(e.to_string());
                }
            }

            // Type filter
            if !type_exts.is_empty() {
                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !type_exts.contains(&ext) {
                    continue;
                }
            }

            // Glob filter
            if let Some(pattern) = glob_pattern {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Ok(m) = glob::Pattern::new(pattern) {
                    if !m.matches(name) {
                        continue;
                    }
                }
            }

            // Read file (skip binary)
            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let lines: Vec<&str> = content.lines().collect();
            let mut file_matches: Vec<(usize, &str)> = Vec::new();

            for (i, line) in lines.iter().enumerate() {
                if regex.is_match(line) {
                    file_matches.push((i, line));
                }
            }

            if file_matches.is_empty() {
                continue;
            }

            match params.output_mode.as_str() {
                "files_with_matches" => {
                    results.push(path.display().to_string());
                    match_count += 1;
                }
                "count" => {
                    results.push(format!("{}:{}", path.display(), file_matches.len()));
                    match_count += 1;
                }
                "content" => {
                    for (line_idx, _) in &file_matches {
                        let start = line_idx.saturating_sub(context_lines);
                        let end = (*line_idx + context_lines + 1).min(lines.len());

                        for (ci, line) in lines.iter().enumerate().take(end).skip(start) {
                            let prefix = if show_line_numbers {
                                format!("{}:{}:", path.display(), ci + 1)
                            } else {
                                format!("{}:", path.display())
                            };
                            results.push(format!("{}{}", prefix, line));
                        }

                        if context_lines > 0 {
                            results.push("--".to_string());
                        }

                        match_count += 1;
                    }
                }
                _ => {
                    results.push(path.display().to_string());
                    match_count += 1;
                }
            }

            if match_count >= head_limit {
                break;
            }
        }

        if results.is_empty() {
            return ToolResult::success(format!(
                "No matches found for pattern \"{}\" in {}",
                params.pattern,
                search_path.display()
            ));
        }

        let output = results.join("\n");
        ToolResult::success(output)
    }
}

impl GrepTool {
    fn search_file(
        &self,
        path: &PathBuf,
        regex: &regex::Regex,
        output_mode: &str,
        context_lines: usize,
        show_line_numbers: bool,
    ) -> ToolResult {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                return ToolResult::error(format!("Failed to read {}: {}", path.display(), e))
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let mut matching_lines: Vec<usize> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if regex.is_match(line) {
                matching_lines.push(i);
            }
        }

        if matching_lines.is_empty() {
            return ToolResult::success(format!("No matches found in {}", path.display()));
        }

        match output_mode {
            "files_with_matches" => ToolResult::success(path.display().to_string()),
            "count" => ToolResult::success(format!("{}:{}", path.display(), matching_lines.len())),
            _ => {
                let mut results = Vec::new();
                for line_idx in &matching_lines {
                    let start = line_idx.saturating_sub(context_lines);
                    let end = (*line_idx + context_lines + 1).min(lines.len());
                    for (ci, line) in lines.iter().enumerate().take(end).skip(start) {
                        if show_line_numbers {
                            results.push(format!("{}:{}", ci + 1, line));
                        } else {
                            results.push(line.to_string());
                        }
                    }
                    if context_lines > 0 {
                        results.push("--".to_string());
                    }
                }
                ToolResult::success(results.join("\n"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use serde_json::json;

    #[tokio::test]
    async fn grep_matches_in_file_files_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.rs");
        std::fs::write(&path, "fn hello() {\n    println!(\"hello world\");\n}\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "hello",
                    "path": path.to_string_lossy(),
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(res.content.contains("test.rs"));
    }

    #[tokio::test]
    async fn grep_content_mode_shows_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.txt");
        std::fs::write(&path, "apple\nbanana\ncherry\napple pie\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "apple",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(res.content.contains("apple"), "should contain 'apple'");
        assert!(
            res.content.contains("apple pie"),
            "should contain 'apple pie'"
        );
        assert!(
            !res.content.contains("banana"),
            "should not contain 'banana'"
        );
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_match.txt");
        std::fs::write(&path, "nothing to see here\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "zzz_nonexistent_999",
                    "path": path.to_string_lossy(),
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep should not error on no matches");
        assert!(res.content.contains("No matches"));
    }

    #[tokio::test]
    async fn grep_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("case.txt");
        std::fs::write(&path, "Hello World\nHELLO WORLD\nhello world\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // Without -i: only first line matches (capital H)
        let res_cs = GrepTool
            .execute(
                json!({
                    "pattern": "^Hello",
                    "path": path.to_string_lossy(),
                    "output_mode": "count",
                }),
                &ctx,
            )
            .await;
        assert!(!res_cs.is_error, "case-sensitive grep failed");
        assert!(
            res_cs.content.contains(":1"),
            "expected 1 match: {:?}",
            res_cs.content
        );

        // With -i: all three lines match
        let res_ci = GrepTool
            .execute(
                json!({
                    "pattern": "^hello",
                    "path": path.to_string_lossy(),
                    "output_mode": "count",
                    "-i": true,
                }),
                &ctx,
            )
            .await;
        assert!(!res_ci.is_error, "case-insensitive grep failed");
        assert!(
            res_ci.content.contains(":3"),
            "expected 3 matches: {:?}",
            res_ci.content
        );
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("count.txt");
        std::fs::write(&path, "match\nskip\nmatch\nskip\nmatch\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "match",
                    "path": path.to_string_lossy(),
                    "output_mode": "count",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(res.content.contains(":3"), "expected 3 matches");
    }

    #[tokio::test]
    async fn grep_invalid_regex() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "[invalid",
                }),
                &ctx,
            )
            .await;

        assert!(res.is_error, "expected error for invalid regex");
        assert!(res.content.contains("Invalid regex"));
    }

    #[tokio::test]
    async fn grep_content_mode_prefixed_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("search.txt");
        std::fs::write(&path, "target line\nother content\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "target",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                    "-n": true,
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            res.content.contains("target line"),
            "should contain matched line"
        );
        assert!(res.content.contains("1:"), "should show line number prefix");
        assert!(
            !res.content.contains("other content"),
            "should NOT contain non-matching line"
        );
    }

    #[tokio::test]
    async fn grep_context_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("context_test.txt");
        std::fs::write(&path, "before\nmatch\nafter\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "match",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                    "context": 1,
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep failed: {}", res.content);
        assert!(
            res.content.contains("before"),
            "context should include line before"
        );
        assert!(
            res.content.contains("match"),
            "context should include match"
        );
        assert!(
            res.content.contains("after"),
            "context should include line after"
        );
    }

    #[tokio::test]
    async fn grep_non_ascii_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("utf8.txt");
        std::fs::write(
            &path,
            "Hello 世界!\ncafé résumé naïve\n日本語 特殊文字\nemoji 👋 test\nplain english\n",
        )
        .unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "世界",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep non-ASCII failed: {}", res.content);
        assert!(res.content.contains("世界"), "should match CJK chars");
        assert!(
            res.content.contains("Hello"),
            "should include full matching line"
        );
    }

    #[tokio::test]
    async fn grep_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.txt");
        std::fs::write(&path, "").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": ".*",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep empty file failed: {}", res.content);
        assert!(
            res.content.contains("No matches"),
            "empty file should report no matches"
        );
    }

    #[tokio::test]
    async fn grep_regex_special_chars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regex_special.txt");
        std::fs::write(&path, "a+b*c\nfoo.bar\nfooXbar\n[test]\n(special)\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        // Test dot as literal (escaped)
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "foo\\.bar",
                    "path": path.to_string_lossy(),
                    "output_mode": "content",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep literal dot failed: {}", res.content);
        assert!(res.content.contains("foo.bar"), "should match literal dot");
        assert!(
            !res.content.contains("fooXbar"),
            "should not match with dot as literal"
        );
    }

    #[tokio::test]
    async fn grep_matches_at_line_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("start_anchor.txt");
        std::fs::write(&path, "start here\nnot start\nstart again\nmiddle start\n").unwrap();

        let ctx = allow_all_context(dir.path().to_path_buf());
        let res = GrepTool
            .execute(
                json!({
                    "pattern": "^start",
                    "path": path.to_string_lossy(),
                    "output_mode": "count",
                }),
                &ctx,
            )
            .await;

        assert!(!res.is_error, "grep line start failed: {}", res.content);
        assert!(
            res.content.contains(":2"),
            "expected 2 matches for '^start': {:?}",
            res.content
        );
    }
}
