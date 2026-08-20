// ResolveMemoryConflict tool: apply a user's verdict on a pending memory
// conflict to the memory frontmatter deterministically.
//
// This is the enforcement layer behind the conversational conflict-resolution
// flow: the agent asks the user via AskUserQuestion (keep the new fact / keep
// the old fact / both are true / I don't know) and then calls this tool. The
// frontmatter state machine (`conflicts:` → `supersedes:`, dropping a claim,
// stamping `asked:`) is applied in code — never by a model hand-editing the
// file — so a wrong answer cannot corrupt memory structure and the state
// transitions are exactly the ones documented in
// `clawde_core::memdir::resolve_memory_conflict`.
//
// The tool is deliberately narrow: it only rewrites validated frontmatter
// inside the project memory dir, and the user's AskUserQuestion answer is the
// consent for that write, so it carries `PermissionLevel::None` instead of
// prompting a second time.

use crate::{PermissionLevel, Tool, ToolContext, ToolErrorCode, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ResolveMemoryConflictTool;

#[derive(Debug, Deserialize)]
struct ResolveConflictInput {
    /// Memory file carrying the claim (`conflicts:` frontmatter naming
    /// `target`), relative to the project memory dir.
    claimant: String,
    /// Memory file the claim is about.
    target: String,
    /// User verdict: `keep_new` | `keep_old` | `both` | `unknown`.
    decision: String,
}

#[async_trait]
impl Tool for ResolveMemoryConflictTool {
    fn name(&self) -> &str {
        clawde_core::constants::TOOL_NAME_RESOLVE_MEMORY_CONFLICT
    }

    fn description(&self) -> &str {
        "Apply the user's verdict on a pending memory conflict. Use this AFTER \
         the user answers an AskUserQuestion about a conflict listed in the \
         Pending Memory Conflicts section of the memory injection. Pass the \
         claimant file (the one whose `conflicts:` frontmatter names the \
         target), the target file, and the decision: keep_new (the claim is \
         right — promotes it to a confirmed supersession), keep_old (the \
         established fact is right — drops the claim), both (both are true in \
         different contexts — drops the claim and records the verdict), or \
         unknown (the user doesn't know — marks the conflict asked so it is \
         never re-asked). The tool updates the frontmatter itself; never \
         hand-edit these fields."
    }

    fn permission_level(&self) -> PermissionLevel {
        // The user's AskUserQuestion answer IS the consent for this narrow
        // write: the tool only rewrites validated frontmatter inside the
        // project memory dir. Gating it at Write level would prompt twice.
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "claimant": {
                    "type": "string",
                    "description": "Memory file whose `conflicts:` frontmatter names `target` (relative to the project memory dir)"
                },
                "target": {
                    "type": "string",
                    "description": "Memory file the claim is about"
                },
                "decision": {
                    "type": "string",
                    "enum": ["keep_new", "keep_old", "both", "unknown"],
                    "description": "The user's verdict: keep_new promotes the claim to a confirmed supersession; keep_old and both drop the claim; unknown marks it asked and leaves it pending"
                }
            },
            "required": ["claimant", "target", "decision"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: ResolveConflictInput = match serde_json::from_value(input) {
            Ok(params) => params,
            Err(e) => {
                return ToolResult::error_with_code(
                    ToolErrorCode::InvalidInput,
                    format!("Invalid input: {}", e),
                );
            }
        };
        let decision = match clawde_core::memdir::ConflictDecision::parse(&params.decision) {
            Some(decision) => decision,
            None => {
                return ToolResult::error_with_code(
                    ToolErrorCode::InvalidInput,
                    format!(
                        "Invalid decision '{}' — expected keep_new, keep_old, both, or unknown",
                        params.decision
                    ),
                );
            }
        };
        // The project memory master switch disables the whole system; respect it.
        if ctx.config.memory.enabled == Some(false) {
            return ToolResult::error("Project memory is disabled (autoMemoryEnabled=false)");
        }
        // Single source of truth for the project root: the session's
        // `project_dir` (kept in lockstep with the query loop's
        // `working_directory`, which the memory injection and the dream both
        // key on) with the tool working dir as fallback. Resolving here
        // guarantees the tool writes to the SAME memdir whose conflicts were
        // injected, even when the session cwd is a subdirectory.
        let project = ctx
            .config
            .project_dir
            .as_ref()
            .cloned()
            .unwrap_or_else(|| ctx.working_dir.clone());
        let memory_dir = clawde_core::memdir::auto_memory_path(&project);
        if !memory_dir.is_dir() {
            return ToolResult::error(format!(
                "No project memory directory at {} — nothing to resolve",
                memory_dir.display()
            ));
        }
        match clawde_core::memdir::resolve_memory_conflict(
            &memory_dir,
            &params.claimant,
            &params.target,
            decision,
        ) {
            Ok(resolution) => ToolResult::success(format!(
                "{} (decision: {})",
                resolution.summary,
                resolution.decision.as_str()
            )),
            Err(e) => ToolResult::error_with_code(ToolErrorCode::InvalidInput, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::allow_all_context;
    use std::path::{Path, PathBuf};

    /// Redirect `CLAWDE_HOME` to a temp dir for the lifetime of the guard.
    /// Serializes on the crate-wide [`crate::TEST_ENV_LOCK`] per AGENTS.md so
    /// it cannot race the other env-mutating tests in this crate.
    struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        prev: Option<std::ffi::OsString>,
    }

    impl TestHome {
        fn new() -> Self {
            let lock = crate::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let prev = std::env::var_os("CLAWDE_HOME");
            let tmp = tempfile::tempdir().unwrap();
            std::env::set_var("CLAWDE_HOME", tmp.path());
            TestHome {
                _lock: lock,
                _tmp: tmp,
                prev,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match &self.prev {
                Some(value) => std::env::set_var("CLAWDE_HOME", value),
                None => std::env::remove_var("CLAWDE_HOME"),
            }
        }
    }

    /// Write a claimant (with `conflicts:`) and a target into a project's
    /// auto-memory dir under a temp home; return the memory dir.
    fn seed_memdir(project: &Path) -> PathBuf {
        let memory_dir = clawde_core::memdir::auto_memory_path(project);
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("auth-flow-v1.md"),
            "---\ndescription: JWT is used\ntype: project\n---\nJWT auth.",
        )
        .unwrap();
        std::fs::write(
            memory_dir.join("auth-flow-v2.md"),
            "---\ndescription: OAuth is used\ntype: project\nconflicts: auth-flow-v1.md\n---\nOAuth auth.",
        )
        .unwrap();
        memory_dir
    }

    #[tokio::test]
    async fn keep_new_promotes_conflict_to_supersedes() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let memory_dir = seed_memdir(project.path());
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "keep_new",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.contains("supersession"), "got: {}", res.content);

        let content = std::fs::read_to_string(memory_dir.join("auth-flow-v2.md")).unwrap();
        let fm = clawde_core::memdir::parse_frontmatter_quick(&content);
        assert!(fm.conflicts.is_empty());
        assert_eq!(fm.supersedes, vec!["auth-flow-v1.md".to_string()]);
    }

    #[tokio::test]
    async fn unknown_stamps_asked_and_keeps_conflict() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let memory_dir = seed_memdir(project.path());
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "unknown",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);

        let content = std::fs::read_to_string(memory_dir.join("auth-flow-v2.md")).unwrap();
        let fm = clawde_core::memdir::parse_frontmatter_quick(&content);
        assert_eq!(fm.conflicts, vec!["auth-flow-v1.md".to_string()]);
        // The ask stamp is per-pair with a date: `target:YYYY-MM-DD`.
        assert_eq!(fm.asked.len(), 1, "asked must be stamped");
        assert!(
            fm.asked[0].starts_with("auth-flow-v1.md:"),
            "got: {:?}",
            fm.asked
        );
    }

    #[tokio::test]
    async fn invalid_decision_errors() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        seed_memdir(project.path());
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "maybe",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("Invalid decision"), "{}", res.content);
    }

    #[tokio::test]
    async fn missing_claimant_errors() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        seed_memdir(project.path());
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "missing.md",
                    "target": "auth-flow-v1.md",
                    "decision": "keep_old",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("not found"), "{}", res.content);
    }

    #[tokio::test]
    async fn already_asked_unknown_refuses() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let memory_dir = seed_memdir(project.path());
        // Pre-stamp `asked:` on the claimant.
        let claimant_path = memory_dir.join("auth-flow-v2.md");
        let content = std::fs::read_to_string(&claimant_path).unwrap();
        std::fs::write(
            &claimant_path,
            content.replace(
                "conflicts: auth-flow-v1.md",
                "conflicts: auth-flow-v1.md\nasked: 2026-08-01",
            ),
        )
        .unwrap();
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "unknown",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("already asked"), "{}", res.content);
    }

    #[tokio::test]
    async fn project_dir_wins_over_working_dir() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        // A subdirectory cwd (the case that used to diverge from the
        // injection's `working_directory`): the tool must resolve the memdir
        // from `config.project_dir`, not the raw cwd.
        let subdir = project.path().join("packages/foo");
        std::fs::create_dir_all(&subdir).unwrap();
        let mut ctx = allow_all_context(subdir.clone());
        ctx.config.project_dir = Some(project.path().to_path_buf());
        let memory_dir = seed_memdir(project.path());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "keep_old",
                }),
                &ctx,
            )
            .await;
        assert!(!res.is_error, "{}", res.content);
        let content = std::fs::read_to_string(memory_dir.join("auth-flow-v2.md")).unwrap();
        assert!(
            content.contains("resolved: auth-flow-v1.md"),
            "got: {}",
            content
        );
    }

    #[tokio::test]
    async fn missing_memory_dir_errors() {
        let _home = TestHome::new();
        let project = tempfile::tempdir().unwrap();
        let ctx = allow_all_context(project.path().to_path_buf());

        let res = ResolveMemoryConflictTool
            .execute(
                json!({
                    "claimant": "auth-flow-v2.md",
                    "target": "auth-flow-v1.md",
                    "decision": "keep_old",
                }),
                &ctx,
            )
            .await;
        assert!(res.is_error);
        assert!(
            res.content.contains("No project memory directory"),
            "{}",
            res.content
        );
    }
}
