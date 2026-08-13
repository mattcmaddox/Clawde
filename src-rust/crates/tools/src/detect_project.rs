// detect_project.rs — Detect the project's language, build system, test
// framework and lint tools by scanning for well-known config files.
//
// This is the foundation of the execute-and-verify loop: `DetectProjectTool`
// exposes the result to the model, while `RunTestsTool` / `RunLintsTool`
// reuse `detect_project_info` to pick sensible default commands when the model
// does not supply an explicit one.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// The dominant programming language of a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectLanguage {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    Java,
    Cpp,
    Unknown(String),
}

impl ProjectLanguage {
    pub fn label(&self) -> String {
        match self {
            ProjectLanguage::Rust => "Rust".to_string(),
            ProjectLanguage::Python => "Python".to_string(),
            ProjectLanguage::TypeScript => "TypeScript".to_string(),
            ProjectLanguage::JavaScript => "JavaScript".to_string(),
            ProjectLanguage::Go => "Go".to_string(),
            ProjectLanguage::Java => "Java".to_string(),
            ProjectLanguage::Cpp => "C/C++".to_string(),
            ProjectLanguage::Unknown(other) => other.clone(),
        }
    }
}

/// Everything `DetectProject` reports about a project root.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub language: ProjectLanguage,
    /// Candidate test commands, most specific first.
    pub test_commands: Vec<String>,
    /// Candidate lint/typecheck commands, most specific first.
    pub lint_commands: Vec<String>,
    pub build_command: Option<String>,
    pub package_manager: Option<String>,
}

/// Best-effort project detection by scanning for well-known config files.
/// Ordered by specificity: Rust > Go > Java > Python > TypeScript > JavaScript.
pub fn detect_project_info(root: &Path) -> ProjectInfo {
    // Rust
    if root.join("Cargo.toml").exists() {
        return ProjectInfo {
            language: ProjectLanguage::Rust,
            test_commands: vec!["cargo test --workspace".to_string()],
            lint_commands: vec!["cargo clippy --workspace --all-targets -- -D warnings".to_string()],
            build_command: Some("cargo build".to_string()),
            package_manager: Some("cargo".to_string()),
        };
    }

    // Go
    if root.join("go.mod").exists() {
        return ProjectInfo {
            language: ProjectLanguage::Go,
            test_commands: vec!["go test ./...".to_string()],
            lint_commands: vec!["go vet ./...".to_string()],
            build_command: Some("go build ./...".to_string()),
            package_manager: Some("go".to_string()),
        };
    }

    // Java (Maven > Gradle)
    if root.join("pom.xml").exists()
        || root.join("build.gradle").exists()
        || root.join("build.gradle.kts").exists()
    {
        let (test, lint, build, pkg) = if root.join("pom.xml").exists() {
            (
                "mvn test".to_string(),
                "mvn verify -DskipTests".to_string(),
                "mvn compile".to_string(),
                "maven".to_string(),
            )
        } else {
            (
                "gradle test".to_string(),
                "gradle check -x test".to_string(),
                "gradle build -x test".to_string(),
                "gradle".to_string(),
            )
        };
        return ProjectInfo {
            language: ProjectLanguage::Java,
            test_commands: vec![test],
            lint_commands: vec![lint],
            build_command: Some(build),
            package_manager: Some(pkg),
        };
    }

    // Python
    if root.join("pyproject.toml").exists()
        || root.join("setup.py").exists()
        || root.join("setup.cfg").exists()
        || root.join("requirements.txt").exists()
    {
        let mut test_commands = Vec::new();
        if root.join("tox.ini").exists() {
            test_commands.push("tox".to_string());
        }
        // `python3` rather than `python`: modern distros (Debian/Ubuntu) ship
        // only `python3`, and a bare `python` may be absent from PATH — which
        // would make the detected test command spawn-fail and skip.
        test_commands.push("python3 -m pytest".to_string());

        let mut lint_commands = Vec::new();
        if root.join("ruff.toml").exists() || root.join(".ruff.toml").exists() {
            lint_commands.push("ruff check .".to_string());
        }
        // mypy only when there is a real signal it is used (config file or a
        // [tool.mypy] table in pyproject.toml) — a bare pyproject.toml alone
        // must not default to mypy, which may not even be installed.
        let mypy_configured = root.join("mypy.ini").exists()
            || root.join(".mypy.ini").exists()
            || root
                .join("pyproject.toml")
                .is_file()
                .then(|| std::fs::read_to_string(root.join("pyproject.toml")).ok())
                .flatten()
                .is_some_and(|content| content.contains("[tool.mypy]"));
        if mypy_configured {
            lint_commands.push("mypy .".to_string());
        }
        if lint_commands.is_empty() {
            lint_commands.push("ruff check .".to_string());
        }

        let pkg_mgr = if root.join("uv.lock").exists() {
            Some("uv".to_string())
        } else if root.join("poetry.lock").exists() {
            Some("poetry".to_string())
        } else {
            Some("pip".to_string())
        };

        return ProjectInfo {
            language: ProjectLanguage::Python,
            test_commands,
            lint_commands,
            build_command: None,
            package_manager: pkg_mgr,
        };
    }

    // TypeScript / JavaScript (package.json)
    if root.join("package.json").exists() {
        let is_ts = root.join("tsconfig.json").exists()
            || root.join("tsconfig.app.json").exists()
            || root.join("tsconfig.node.json").exists();
        let pkg_mgr = if root.join("pnpm-lock.yaml").exists() {
            "pnpm".to_string()
        } else if root.join("yarn.lock").exists() {
            "yarn".to_string()
        } else if root.join("bun.lockb").exists() || root.join("bun.lock").exists() {
            "bun".to_string()
        } else {
            "npm".to_string()
        };

        let test_commands = vec![format!("{} test", pkg_mgr)];
        let mut lint_commands = vec![format!("{} run lint", pkg_mgr)];
        if is_ts {
            lint_commands.push("npx tsc --noEmit".to_string());
        }

        return ProjectInfo {
            language: if is_ts {
                ProjectLanguage::TypeScript
            } else {
                ProjectLanguage::JavaScript
            },
            test_commands,
            lint_commands,
            build_command: Some(format!("{} run build", pkg_mgr)),
            package_manager: Some(pkg_mgr),
        };
    }

    // C / C++ (CMake or Makefile)
    if root.join("CMakeLists.txt").exists() || root.join("Makefile").exists() {
        return ProjectInfo {
            language: ProjectLanguage::Cpp,
            test_commands: vec!["ctest".to_string()],
            lint_commands: Vec::new(),
            build_command: Some("cmake --build .".to_string()),
            package_manager: None,
        };
    }

    ProjectInfo {
        language: ProjectLanguage::Unknown("unknown".to_string()),
        test_commands: Vec::new(),
        lint_commands: Vec::new(),
        build_command: None,
        package_manager: None,
    }
}

/// Detect the project's language, build system, test framework and lint tools.
/// Call once at session start (or whenever tooling is unknown); results are
/// cached in project memory so later calls are cheap.
pub struct DetectProjectTool;

#[async_trait]
impl Tool for DetectProjectTool {
    fn name(&self) -> &str {
        "DetectProject"
    }

    fn description(&self) -> &str {
        "Analyze the project structure to detect the language, build system, \\\n\
         test framework and lint tools. Call once at session start if the \\\n\
         project's tooling is unknown — RunTests / RunLints use the detected \\\n\
         commands as defaults."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "project_root": {
                    "type": "string",
                    "description": "Optional project root path. Defaults to the working directory."
                }
            },
            "required": []
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let project_root = input
            .get("project_root")
            .and_then(|v| v.as_str())
            .map(|s| ctx.resolve_path(s))
            .unwrap_or_else(|| ctx.working_dir.clone());

        if !project_root.is_dir() {
            return ToolResult::error(format!(
                "Project root '{}' is not a directory",
                project_root.display()
            ));
        }

        let info = detect_project_info(&project_root);
        let mut lines = vec![format!("Language: {}", info.language.label())];
        if let Some(pkg) = &info.package_manager {
            lines.push(format!("Package manager: {}", pkg));
        }
        if let Some(build) = &info.build_command {
            lines.push(format!("Build command: {}", build));
        }
        if info.test_commands.is_empty() {
            lines.push("Test commands: (none detected)".to_string());
        } else {
            lines.push(format!("Test commands: {}", info.test_commands.join(", ")));
        }
        if info.lint_commands.is_empty() {
            lines.push("Lint commands: (none detected)".to_string());
        } else {
            lines.push(format!("Lint commands: {}", info.lint_commands.join(", ")));
        }

        ToolResult::success(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn detects_rust() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let info = detect_project_info(dir.path());
        assert_eq!(info.language, ProjectLanguage::Rust);
        assert_eq!(info.test_commands, vec!["cargo test --workspace"]);
        assert!(info.lint_commands[0].contains("cargo clippy"));
        assert_eq!(info.package_manager.as_deref(), Some("cargo"));
    }

    #[test]
    fn detects_python_prefers_uv() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "");
        write(dir.path(), "uv.lock", "");
        let info = detect_project_info(dir.path());
        assert_eq!(info.language, ProjectLanguage::Python);
        assert_eq!(info.package_manager.as_deref(), Some("uv"));
        assert!(info.test_commands.iter().any(|c| c.contains("pytest")));
        // Bare pyproject.toml without ruff/mypy config defaults to ruff, and
        // must NOT assume mypy is installed.
        assert_eq!(
            info.lint_commands.first().map(String::as_str),
            Some("ruff check .")
        );
        assert!(
            !info.lint_commands.iter().any(|c| c.contains("mypy")),
            "mypy should not be defaulted from a bare pyproject.toml"
        );
    }

    #[test]
    fn python_mypy_requires_explicit_config() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[tool.mypy]\nstrict = true\n");
        let info = detect_project_info(dir.path());
        assert!(
            info.lint_commands.iter().any(|c| c.contains("mypy")),
            "mypy should be detected when pyproject.toml has [tool.mypy]"
        );
    }

    #[test]
    fn detects_typescript_vs_javascript() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "package.json", "{}");
        let info = detect_project_info(dir.path());
        assert_eq!(info.language, ProjectLanguage::JavaScript);

        write(dir.path(), "tsconfig.json", "{}");
        let info = detect_project_info(dir.path());
        assert_eq!(info.language, ProjectLanguage::TypeScript);
        assert!(info.lint_commands.iter().any(|c| c.contains("tsc")));
    }

    #[test]
    fn detects_go() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/foo\n");
        let info = detect_project_info(dir.path());
        assert_eq!(info.language, ProjectLanguage::Go);
        assert_eq!(info.test_commands, vec!["go test ./..."]);
    }

    #[test]
    fn unknown_project_has_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        let info = detect_project_info(dir.path());
        assert!(info.test_commands.is_empty());
        assert!(info.lint_commands.is_empty());
        assert!(info.build_command.is_none());
    }

    #[tokio::test]
    async fn execute_reports_detected_language() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        let ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        let res = DetectProjectTool.execute(serde_json::json!({}), &ctx).await;
        assert!(!res.is_error, "detect failed: {}", res.content);
        assert!(res.content.contains("Language: Rust"));
        assert!(res.content.contains("cargo test --workspace"));
    }
}
