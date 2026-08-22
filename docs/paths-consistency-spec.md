# Path System Consistency Audit & Spec

**Status:** Implemented (2026-08-22)
**Date:** 2026-08-22
**Scope:** Full audit of how filesystem paths are determined, propagated, and used across all Clawde subsystems.

---

## 1. Executive Summary

Clawde uses **six overlapping path concepts** with no single source of truth. These concepts were introduced organically as features were added, leading to inconsistencies where different subsystems disagree about the "current directory." The most visible symptom is that session stats and transcripts can use different project identifiers, making it appear that sessions vanish when switching between `/stats` and `/session list`. This spec maps every path variable, identifies inconsistencies, and recommends a unified model.

---

## 2. Path Inventory

### 2.1 The Six Core Path Variables

| # | Variable | Type | Owner | Set When |
|---|----------|------|-------|----------|
| 1 | `cwd` | `PathBuf` | `main.rs` | Process launch; from `--cwd` flag or `std::env::current_dir()` |
| 2 | `ToolContext.working_dir` | `PathBuf` | `main.rs` → tools | Set to `cwd` at startup; updated on `/move` and session resume |
| 3 | `Config.project_dir` | `Option<PathBuf>` | `main.rs` → config | Set to `cwd` at startup; updated on `/move`, ACP session, teleport, session resume |
| 4 | `QueryConfig.working_directory` | `Option<String>` | `main.rs` → query loop | Derived from `config.project_dir` as string; set at startup |
| 5 | `get_repo_root()` / `find_git_root()` | `Option<PathBuf>` | Ad-hoc | Computed on demand by walking up from a start path |
| 6 | `App.current_dir` | `Option<String>` | TUI | Display-only; set by `set_working_directory()` |

### 2.2 Additional Path Concepts

| Variable | Location | Purpose |
|----------|----------|---------|
| `clawde_home()` / `Settings::config_dir()` | `core/src/paths.rs` | Data root: `~/.clawde/` or `$CLAWDE_HOME` |
| `config.workspace_paths` | `core/src/lib.rs` | Permission-allowed directories |
| `config.additional_dirs` | `core/src/lib.rs` | `--add-dir` granted directories |
| `project_root` (local var) | Various | Derived via `get_repo_root(&working_dir)` |
| `memory_dir` | `core/src/memdir.rs` | Auto-generated from `project_root` |
| `transcript_dir` | `core/src/session_storage.rs` | Auto-generated from `project_root` |
| `projects_dir` | `core/src/session_storage.rs` | `clawde_home()/projects/` |
| `sessions_dir` | `core/src/lib.rs` | `clawde_home()/sessions/` |
| `agd_path` | `core/src/plan.rs` | Generated from `project_root` |

---

## 3. Path Lifecycle

### 3.1 Startup (CLI / TUI)

```
CLI args
  └── --cwd flag OR std::env::current_dir()
        └── cwd: PathBuf
              ├── Settings::load_hierarchical(&cwd)  ← finds project settings by walking up from cwd
              ├── config.project_dir = Some(cwd.clone())
              ├── ContextBuilder::new(cwd.clone())     ← system prompt, git status, AGENTS.md
              ├── tool_ctx.working_dir = cwd.clone()   ← all tools resolve paths against this
              ├── query_config.working_directory = cwd.to_string()
              └── session.working_dir = None (new session)
```

### 3.2 Session Resume

```
session.working_dir (from saved session)
  ├── if path exists: tool_ctx.working_dir = saved_path
  ├── if path exists: cmd_ctx.working_dir = saved_path
  ├── app.config.project_dir = Some(tool_ctx.working_dir)
  └── note: query_config.working_directory is NOT updated here (set per-turn in run_query_loop)
```

**Issue:** On session resume, `cmd_ctx.config.project_dir` is NOT updated — only `app.config.project_dir` is. This means `resolve_memory_conflict.rs` (which reads `config.project_dir`) sees the old cwd while tools see the restored dir.

### 3.3 /move (Session Directory Change)

```
destination: PathBuf
  ├── tool_ctx.working_dir = destination.clone()
  ├── cmd_ctx.working_dir = destination.clone()
  ├── cmd_ctx.config.project_dir = Some(destination.clone())
  ├── tool_ctx.config.project_dir = Some(destination.clone())
  ├── app.config.project_dir = Some(destination.clone())
  ├── base_query_config.working_directory = destination.display().to_string()
  ├── session.working_dir = Some(destination.display().to_string())
  └── session.updated_at = now
```

**This is the most complete path update — all six variables are updated together.**

### 3.4 Teleport Import

```
bundle.working_dir: String
  ├── ctx.working_dir = restored_dir        ← ToolContext
  ├── std::env::set_current_dir(&restored_dir)  ← process-global!
  └── note: config.project_dir is NOT updated
```

**Issue:** Teleport updates `tool_ctx.working_dir` and `set_current_dir()` but does NOT update `config.project_dir`. This means memory conflict resolution and other `config.project_dir` consumers will use the OLD project directory.

### 3.5 ACP Session

```
working_dir: PathBuf
  └── config.project_dir = Some(working_dir.clone())
```

**This only sets `project_dir`, not `working_dir` on the ToolContext — the ToolContext isn't built in this path.**

---

## 4. Identified Inconsistencies

### 4.1 Session Resume Does Not Update `cmd_ctx.config.project_dir`

**Location:** `cli/src/main.rs:4070-4075`
**Impact:** `resolve_memory_conflict.rs` reads `config.project_dir` for memory dir resolution. After resume, this may point to the wrong directory.
**Severity:** Medium — memory operations may write to the wrong project's memory dir.

### 4.2 Teleport Does Not Update `config.project_dir`

**Location:** `commands/src/teleport.rs:310`
**Impact:** Memory, snapshot, and other `config.project_dir` consumers use stale path.
**Severity:** High — teleport explicitly changes the working directory but `project_dir` stays behind.

### 4.3 `find_git_root()` Duplicates `get_repo_root()`

**Location:** `query/src/agent_tool.rs:36-44` (private) vs `core/src/git_utils.rs:12-20` (public)
**Impact:** Two identical implementations; maintenance risk if one is updated.
**Severity:** Low — currently identical logic, but divergence risk.

### 4.4 Stats vs Transcripts Use Different Project Identifiers

**Location:**
- `commands/src/stats.rs:217` — `projects_dir()` encodes `cwd` (URL-safe base64)
- `core/src/session_storage.rs:259` — `transcript_dir()` encodes `project_root` (git root)

**Impact:** When `cwd` is a subdirectory of a git repo, the stats command looks under `~/.clawde/projects/<base64(cwd)>/` while transcripts are stored under `~/.clawde/projects/<base64(git_root)>/`. Sessions appear to vanish.
**Severity:** High — user-visible data inconsistency.

### 4.5 Inconsistent Canonicalization

**Canonicalized sites:**
- `continuation.rs:69-91` — `path_is_within_working_dir()` canonicalizes both paths
- `agent_tool.rs:1027-1030` — `patch_targets_are_scoped()` canonicalizes for access control
- `spec.rs:197-198` — `latest_in()` canonicalizes to prevent path traversal

**Non-canonicalized sites:**
- `tools/src/lib.rs:424-430` — `ToolContext.resolve_path()` does NOT canonicalize
- `glob_tool.rs`, `grep_tool.rs` — resolve paths against working_dir without canonicalizing
- `run_lints.rs` — uses working_dir as-is

**Impact:** If `working_dir` contains a symlink, `resolve_path()` returns a symlinked path, but `path_is_within_working_dir()` compares against the canonical target. The path can fail the containment check even though it's logically within the workspace.
**Severity:** Medium — affects edge cases with symlinked directories (common in monorepos).

### 4.6 `query_config.working_directory` Is a String, Not PathBuf

**Location:** `query/src/lib.rs:127`
**Impact:** Lost path semantics (no normalization, no comparison operators). Every consumer must re-parse from string.
**Severity:** Low — cosmetic but makes consistency harder to enforce.

### 4.7 No Single Source of Truth for "Current Directory"

**Impact:** There is no guaranteed invariant that `working_dir == project_dir.display() == working_directory`. They are set at different times and updated at different call sites.
**Severity:** High — root cause of many of the above inconsistencies.

---

## 5. Path Concept Definitions (Proposed)

### 5.1 `working_dir` (The Canonical Path)

**Definition:** The directory Clawde is currently "in" — where tools execute, where relative paths are resolved, and where the model's mental model of the workspace is rooted.

**Update points:**
1. Process launch (from `--cwd` or `env::current_dir()`)
2. Session resume (from `session.working_dir`)
3. `/move` command (to destination)
4. Teleport import (from bundle)
5. ACP session start (from client-provided cwd)

**Invariant:** Exactly one path owns the current directory. All other path concepts are derived from it.

### 5.2 `project_root` (Derived)

**Definition:** The git repository root, determined by `get_repo_root(&working_dir)`. Falls back to `working_dir` when not inside a git repo.

**Derived from:** `working_dir` at query time (not stored).
**Used by:** Transcript storage, memory dir, session history, diff viewer, AGENTS.md discovery, git operations.

### 5.3 `config.project_dir` (Project Root)

**Definition:** The project root directory — the git repository root, or `working_dir` when not inside a git repo. Used by `resolve_memory_conflict.rs`, memory injection, snapshot system, and session storage.

**Current bug:** Initialized to `cwd` at startup instead of `get_repo_root(&cwd)`. This means when `cwd` is a subdirectory, `project_dir` points to the subdirectory instead of the project root.

**Recommendation:** Keep this field but initialize it to `get_repo_root(&cwd) || cwd` at all update points. This preserves the semantic distinction between "where I am" (`working_dir`) and "what project this is" (`project_dir`).

**Semantic distinction:**
- `working_dir` = the user's current directory (where tools execute, relative paths resolve)
- `project_dir` = the project root (where .git, memdir, AGENTS.md live)

**Test validation:** The test `project_dir_wins_over_working_dir` in `resolve_memory_conflict.rs` explicitly validates this distinction: it sets `working_dir` to a subdirectory but `project_dir` to the parent root, then verifies the memory system uses the root.

### 5.4 `clawde_home()` (Unrelated)

**Definition:** The data root (`~/.clawde/`). Not a working directory — this is where Clawde stores its data. Unchanged by this spec.

---

## 5A. Two Memory Systems

Clawde has two distinct memory systems with different storage locations and purposes:

### AGENTS.md (Project-Local Instructions)

| Aspect | Details |
|--------|---------|
| **Location** | `{project_root}/.clawde/AGENTS.md` (local) or `{project_root}/AGENTS.md` (project) |
| **Purpose** | Project-specific instructions, team-shared knowledge |
| **Committed to git?** | Yes (or gitignored) |
| **Keyed on** | Project root (git root or cwd) |
| **Scope** | `MemoryScope::Project` or `MemoryScope::Local` |

AGENTS.md files live inside the project directory and are meant to be committed (or gitignored). They contain project-level instructions that the team shares.

### Memdir (User's Personal Memory)

| Aspect | Details |
|--------|---------|
| **Location** | `~/.clawde/projects/<encoded(project_root)>/memory/` |
| **Purpose** | User's personal memory about the project (what they learned, feedback, references) |
| **Committed to git?** | No — lives outside the repo |
| **Keyed on** | `config.project_dir` (should be git root) |
| **Scope** | Per-user, per-project |

Memdir is global (under `~/.clawde/`) for several reasons:

1. **Git hygiene.** If memory lived at `project/.clawde/memory/`, it would be inside the git repo. Users would either commit it (leaking personal memory) or add it to `.gitignore` (extra friction).
2. **Privacy.** Memory is per-user, not per-project. Your memory about a project shouldn't be visible to collaborators.
3. **Multi-worktree.** If you have `project-main/` and `project-feature/` checked out, they share the same memory because they have the same git root.
4. **Backup/sync.** The entire `~/.clawde/` directory can be backed up as a unit.

### Why This Matters for Path Consistency

The memdir system uses `config.project_dir` (via `auto_memory_path(project_root)`) to determine where to store memory. If `config.project_dir` is initialized to `cwd` instead of the git root, memory gets stored under the wrong bucket:

```
# cwd = /home/user/project/src/feature/
# git_root = /home/user/project/

# WRONG (current behavior):
# config.project_dir = /home/user/project/src/feature/
# memdir = ~/.clawde/projects/<encoded(project/src/feature)>/memory/

# CORRECT (proposed fix):
# config.project_dir = /home/user/project/
# memdir = ~/.clawde/projects/<encoded(project)>/memory/
```

This is the root cause of the stats/transcripts divergence: transcripts use git root (correct) while stats and memdir use cwd (wrong).

---

## 6. Recommendations

### 6.1 Initialize `config.project_dir` to Git Root

**Priority:** P0

**Changes:**
1. At startup (`main.rs:765`): Change `config.project_dir = Some(cwd.clone())` to `config.project_dir = Some(get_repo_root(&cwd).unwrap_or_else(|| cwd.clone()))`.
2. On `/move` (`main.rs:3998-4000`): Compute git root from destination: `config.project_dir = Some(get_repo_root(&destination).unwrap_or(destination.clone()))`.
3. On session resume (`main.rs:4085`): Compute git root from restored `working_dir`.
4. On teleport import: Compute git root from restored dir.
5. On ACP session (`acp/src/runtime.rs:96`): Compute git root from `working_dir`.

**Rationale:** `config.project_dir` has a distinct semantic purpose (project root) that must be preserved. The memory system, transcript storage, and session history all need the project root, not the cwd. The test `project_dir_wins_over_working_dir` validates this distinction.

**Why not remove `config.project_dir`?** The memory system uses `config.project_dir` to find the memdir. If you replace it with `tool_ctx.working_dir`, memory would be keyed on the cwd subdirectory instead of the project root, producing different memory locations for every subdirectory the user opens. The two fields serve different purposes:
- `working_dir` = "where I am" (tools execute here)
- `project_dir` = "what project this is" (memdir, transcripts, AGENTS.md live here)

### 6.2 Deduplicate `find_git_root()`

**Priority:** P1

**Changes:**
1. Delete the private `find_git_root()` in `query/src/agent_tool.rs:36-44`.
2. Replace all call sites with `clawde_core::git_utils::get_repo_root()`.

**Rationale:** Identical logic in two places is a maintenance hazard. One function, one behavior.

### 6.3 Unify Project Identifier for Session Storage

**Priority:** P1

**Changes:**
1. `stats.rs` and `session_storage.rs` must use the same project identifier.
2. The canonical identifier should be: `get_repo_root(&working_dir)` when available, else `working_dir` (resolved to absolute).
3. Both `projects_dir()` and `transcript_dir()` should encode the same value.

**Rationale:** Sessions should not vanish when switching between `/stats` and `/session list`.

**Two-step fix:**
- **Step 1 (behavioral fix):** Change `stats.rs:217` to use `get_repo_root(&cwd) || cwd` instead of encoding `cwd` directly. This is a 1-line change that immediately fixes the inconsistency for new sessions.
- **Step 2 (data migration):** Add a startup migration that moves transcripts from cwd-based buckets to git-root-based buckets for historical sessions. See `docs/paths-migration-analysis.md` for the full migration strategy.

**Migration scope:** Only sessions where `cwd != git_root` are affected (user launched from a subdirectory). Most users launch from the repo root, so the impact is low-to-medium. The migration is idempotent and can run in a background task at startup.

### 6.4 Canonicalize at Entry, Trust the Result

**Priority:** P2 — **REVISED after implementation audit**

**Original recommendation:** Canonicalize `resolve_path()`'s result and remove redundant canonicalization at comparison sites.

**Audit finding (2026-08-22):** The original recommendation is **not applicable** to the current codebase:

1. **Every containment check already canonicalizes both sides.** `path_is_within_working_dir` (continuation.rs:69), `path_is_within_workspace` (tools/lib.rs:628), `is_path_within_roots` (core/lib.rs:4445), and `patch_targets_are_scoped` / `changed_files_are_scoped` (agent_tool.rs:626/828) all canonicalize the candidate path AND the working_dir/root before comparing. The claimed impact ("path can fail the containment check even though it's logically within the workspace") does not occur — verified by a new symlink test (`test_path_is_within_workspace_handles_symlinked_working_dir`).
2. **Canonicalizing `resolve_path()`'s result would BREAK new-file creation.** `resolve_path()` is used by Write/Edit/BatchEdit/NotebookEdit/ApplyPatch, which create files that don't exist yet, and `Path::canonicalize()` fails on non-existent paths.

**What was done instead:**
1. Documented the contract on `resolve_path()` — it returns the un-canonicalized join; callers doing containment MUST canonicalize both sides (which all current call sites already do).
2. Added `test_path_is_within_workspace_handles_symlinked_working_dir` to lock in the symlink-safe behavior.
3. **No** removal of comparison-site canonicalization — that code is correct and defensive; removing it would be a regression.

### 6.5 Change `working_directory` to `PathBuf`

**Priority:** P3

**Changes:**
1. Change `QueryConfig.working_directory: Option<String>` to `Option<PathBuf>`.

**Rationale:** Typed paths are less error-prone than string representations.

---

## 7. Path Flow Diagram (Proposed)

```
                          ┌──────────────┐
                          │  --cwd flag  │
                          │  or env::cwd │
                          └──────┬───────┘
                                 │
                                 ▼
                     ┌───────────────────────────────┐
                     │  working_dir: PathBuf          │ ◄── WHERE I AM
                     │  (ToolContext)                 │     (tools execute here)
                     └───────────┬───────────────────┘
                                 │
                                 │  get_repo_root(&working_dir)
                                 ▼
                     ┌───────────────────────────────┐
                     │  project_dir: PathBuf          │ ◄── WHAT PROJECT THIS IS
                     │  (Config)                     │     (memdir, transcripts,
                     │  = git_root || working_dir     │      AGENTS.md live here)
                     └───────────┬───────────────────┘
                                 │
            ┌────────────────────┼────────────────────┐
            │                    │                     │
            ▼                    ▼                     ▼
   ┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
   │ AGENTS.md      │  │ Memdir          │  │ Transcript       │
   │ (local)        │  │ (global)        │  │ (global)         │
   │                │  │                 │  │                  │
   │ {project_root} │  │ ~/.clawde/      │  │ ~/.clawde/       │
   │ /.clawde/     │  │ projects/<enc>  │  │ projects/<enc>/  │
   │ AGENTS.md      │  │ /memory/        │  │ {session}.jsonl  │
   └────────────────┘  └─────────────────┘  └──────────────────┘
            │                    │                     │
            ▼                    ▼                     ▼
   ┌────────────────┐  ┌─────────────────┐  ┌──────────────────┐
   │ Committed to   │  │ Per-user,       │  │ Per-user,        │
   │ git (or        │  │ per-project     │  │ per-project      │
   │ gitignored)    │  │ (private)       │  │ (private)        │
   └────────────────┘  └─────────────────┘  └──────────────────┘
```

**Key insight:** AGENTS.md is project-local (committed to git), while memdir and transcripts are global (under `~/.clawde/`) but keyed on the project root. Both global stores use `config.project_dir` as the project identifier, so `config.project_dir` must be the git root, not the cwd.

---

## 8. Testing Strategy

### 8.1 Existing Tests to Verify

| Test | Location | What it covers |
|------|----------|----------------|
| `clawde_home_*` | `core/src/paths.rs` | Home dir resolution |
| `path_is_within_working_dir` | `query/src/continuation.rs` | Containment checks |
| `patch_targets_are_scoped` | `query/src/agent_tool.rs` | Access control |
| `session_directories_require_absolute_paths` | `acp/src/server.rs` | ACP path validation |

### 8.2 New Tests to Add

1. **Session resume path consistency:** After resume, verify `tool_ctx.working_dir == app.config.project_dir == query_config.working_directory`.
2. **Teleport path consistency:** After import, verify all path variables point to the restored directory.
3. **Stats/transcript project ID parity:** Verify `encoded_dir_for_cwd()` and `transcript_dir()` produce the same project bucket for the same directory.
4. **Symlink resolution:** Create a symlinked working dir, verify `resolve_path()` and `path_is_within_working_dir()` agree on containment.
5. **/move path propagation:** After `/move`, verify all six path variables are updated.

### 8.3 Regression Guard

The `idle-cpu-probe.py` and `cargo check -p clawde-tui --tests` pre-commit checks remain unchanged. Add a `cargo test --workspace` CI gate that specifically runs the new path consistency tests.

---

## 9. Migration & Backward Compatibility

### 9.1 Transcript Path Migration

When unifying the project identifier (Section 6.3), existing transcripts stored under `projects/<base64(cwd)>/` where `cwd` is a subdirectory of a repo need to be migrated to `projects/<base64(git_root)>/`.

**Strategy:** On first run after the change, scan `projects_dir()` for encoded directory names that decode to a path that is a subdirectory of a known git repo. Move those JSONL files to the git-root bucket. This can be done in a background task at startup.

### 9.2 Session Working Dir Migration

Existing sessions may have `working_dir` set to a subdirectory of a repo. On resume, the system should canonicalize and use the git root if available, or keep the saved path if not.

---

## 10. Open Questions — Resolved (2026-08-22)

1. **Should `set_current_dir()` be called on /move?** **No.** Audited: every subprocess-spawning tool (Bash/PTY, RunTests, RunLints, Powershell) sets its cwd explicitly from `ctx.working_dir` (which IS updated on /move), not from `std::env::current_dir()`. Adding a process-global `set_current_dir()` mid-session would mutate shared state (affecting settings sync, TUI path completion, memory notifications) without fixing any correctness bug. Teleport calls it because import is a restore-to-a-different-state operation, not an in-session relocation. The existing design — repointing every cwd-aware surface via the context objects — is correct.

2. **Should the ACP runtime update `ToolContext.working_dir`?** **Already handled.** The per-session `ToolContext` in `acp/src/prompt.rs:78` sets `working_dir: session.cwd.clone()`. The runtime-level `config.project_dir` is now set to the git root (Phase 0 fix). No further change needed.

3. **How should non-git projects be handled?** **Inherent limitation, documented.** When `get_repo_root()` returns `None`, the project identifier falls back to `working_dir`. Without a git root there is no way to know two subdirectories belong to the same project. This is correct and requires no change.

---

## 11. File-Level Change Summary (as implemented)

| File | Change | Priority |
|------|--------|----------|
| `core/src/git_utils.rs` | Added `project_root()` — the canonical project-identifier derivation (git root or cwd fallback) | P0 |
| `cli/src/main.rs` | All `config.project_dir` / `working_directory` set sites use `project_root()` (startup, /move, resume ×2) | P0 |
| `commands/src/teleport.rs` | Teleport import sets `project_dir` via `project_root()`; added 2 import tests | P0 |
| `acp/src/runtime.rs` | ACP session start uses `project_root()` | P0 |
| `acp/src/prompt.rs` | ACP prompt uses `project_root()` | P0 |
| `query/src/agent_tool.rs` | Removed duplicate `find_git_root()`, uses `get_repo_root()` | P1 |
| `commands/src/stats.rs` | Uses `project_root(cwd)` as project identifier; added subdirectory regression test | P1 |
| `core/src/session_storage.rs` | Added `migrate_cwd_transcript_buckets()`; added 2 migration tests | P1 |
| `cli/src/main.rs` | Startup runs transcript-bucket migration before the interactive/headless branch | P1 |
| `query/src/lib.rs`, `commands/src/{history,review,spec,session}.rs`, `query/src/runner/tools.rs`, `tui/src/app.rs` | All project-identifier derivations use `project_root()` | P1 |
| `tools/src/lib.rs` | Documented `resolve_path()` canonicalization contract; added symlink containment test | P2 |
| `query/src/lib.rs`, `core/src/system_prompt.rs`, `acp/*`, `cli/src/main.rs` | `working_directory` changed `Option<String>` → `Option<PathBuf>` | P3 |

**Not implemented (audited as unnecessary/harmful):**
- Canonicalizing `resolve_path()` result — would break new-file creation; all containment checks already canonicalize both sides (Section 6.4).
- `set_current_dir()` on /move — tools already use `ctx.working_dir`; process-global mutation adds risk without fixing a bug (Section 10).
