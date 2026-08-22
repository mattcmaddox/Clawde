# Transcript Migration Analysis

**Date:** 2026-08-22
**Related spec:** `docs/paths-consistency-spec.md` Section 6.3

---

## Implementation Status — COMPLETE (2026-08-22)

All three recommended steps are implemented and committed:

1. **`/stats` project-identifier fix (Option B)** — `collect_jsonl_paths` in `commands/src/stats.rs` now resolves the project root via `clawde_core::git_utils::project_root(cwd)` instead of encoding the raw cwd.
2. **Migration function (Option A)** — `migrate_cwd_transcript_buckets(config_dir)` in `core/src/session_storage.rs` moves `.jsonl` files from cwd-keyed buckets into git-root buckets. Idempotent, never overwrites, removes empty stale buckets.
3. **Startup wiring** — `main.rs` runs the migration once before the interactive/headless branch, so new sessions land in the canonical bucket from the start.

**Unifying helper:** `git_utils::project_root(start)` (git root, or the path itself when not in a repo) is now the single source of truth for the project identifier. Every subsystem that keys per-project data routes through it: transcript write/read, stats, history, review, spec, session list, memory, teleport, ACP, and TUI spec review.

**Tests added:**
- `migrate_cwd_transcript_buckets_moves_subdirectory_transcripts` — moves + removes stale bucket + idempotency
- `migrate_cwd_transcript_buckets_leaves_other_buckets_alone` — git-root and non-repo buckets untouched
- `run_summary_from_subdirectory_finds_repo_root_sessions` — `/stats` from a subdir finds repo-root sessions
- `project_root_is_stable_across_subdirectories` / `project_root_falls_back_to_cwd_outside_repo`
- `import_sets_project_dir_to_git_root` / `import_sets_project_dir_to_cwd_when_not_a_repo` (teleport)

**Manual smoke test:** launching the debug binary from a git subdirectory with a transcript staged in the git-root bucket, `clawde stats` reports the session (verified end-to-end).

---

## Problem Statement

When unifying the project identifier for session storage (Section 6.3 of the paths spec), existing transcripts stored under the wrong project bucket need to be migrated. This document analyzes the scope of the problem and proposes a migration strategy.

---

## Current State

### Storage Layout

```
~/.clawde/
├── sessions/                        ← Session metadata (UUID.json, NOT project-scoped)
│   ├── abc123.json
│   └── def456.json
├── projects/                        ← Transcript data (project-scoped)
│   ├── <base64(/home/user/project)>/      ← git root bucket
│   │   ├── abc123.jsonl
│   │   └── def456.jsonl
│   └── <base64(/home/user/project/src)>/  ← cwd bucket (diverges when cwd != git root)
│       └── (empty or contains stale transcripts)
└── sessions.db                      ← SQLite index (optional, faster queries)
```

### How Each Subsystem Resolves the Project Bucket

| Subsystem | Key Used | Source |
|-----------|----------|--------|
| **Transcript write** (`main.rs:3036`) | `get_repo_root(&tool_ctx.working_dir) \|\| tool_ctx.working_dir` | Git root (or cwd if not in repo) |
| **Transcript read** (`main.rs:2884`) | Same `transcript_project_root` | Same as write |
| **Stats single-project** (`stats.rs:217`) | `encoded_dir_for_cwd(cwd)` | Process cwd directly |
| **Stats all-projects** (`stats.rs:236`) | Iterates all dirs under `projects/` | Everything |
| **Session list** (`lib.rs:5552`) | N/A — reads `sessions/` dir | Not project-scoped |
| **/history revert** (`history.rs:133`) | `get_repo_root(&ctx.working_dir) \|\| ctx.working_dir` | Git root |
| **delete_session** (`lib.rs:5579`) | Iterates all `projects/` dirs | Everything |

### The Divergence

**When cwd is a subdirectory of a git repo:**

```bash
# User's scenario:
cd ~/project/src
clawde
# transcripts → ~/.clawde/projects/<base64(~/project)>/
# /stats looks → ~/.clawde/projects/<base64(~/project/src)>/
# Result: /stats shows 0 sessions
```

**When cwd is the git root (most common):**

```bash
cd ~/project
clawde
# transcripts → ~/.clawde/projects/<base64(~/project)>/
# /stats looks → ~/.clawde/projects/<base64(~/project)>/
# Result: /stats works correctly
```

**When not in a git repo:**

```bash
cd ~/scratch
clawde
# transcripts → ~/.clawde/projects/<base64(~/scratch)>/
# /stats looks → ~/.clawde/projects/<base64(~/scratch)>/
# Result: /stats works correctly
```

---

## Scope of Impact

### Affected Data

Only sessions where `cwd != git_root` are affected. This happens when:
1. User `cd`s into a subdirectory before launching Clawde
2. User uses `--cwd` flag pointing to a subdirectory
3. ACP sessions provide a subdirectory as the working directory

**Probability:** Low-to-medium. Most users launch from the repo root.

### Affected Commands

| Command | Impact |
|---------|--------|
| `/stats` (single-project) | Cannot find transcripts when cwd is a subdirectory |
| `/stats --all-projects` | Works (scans all buckets) |
| `/stats sessions` | Same as `/stats` |
| `/stats daily` | Same as `/stats` |
| `/stats tools` | Same as `/stats` |
| `/session list` | Works (reads `sessions/` dir, not project-scoped) |
| `/resume` | Works (reads `sessions/` dir) |
| `/undo`, `/revert` | Works (uses git root for transcript lookup) |
| `/checkpoints` | Works (uses git root) |

### Quantifying the Problem

To estimate how many users are affected, you'd need to check:
1. How often `session.working_dir` differs from `get_repo_root(session.working_dir)`
2. The distribution of `cwd` values across all sessions

A quick heuristic: sessions where `working_dir` contains `src/`, `lib/`, `app/`, or other common subdirectory prefixes are likely affected.

---

## Migration Strategy

### Option A: Move Transcripts to Git Root Bucket (Recommended)

**When:** On first run after the fix is deployed.

**How:**
1. Scan `~/.clawde/projects/` for all encoded directory names
2. For each, decode the base64 to get the original path
3. Check if that path is a subdirectory of a git repo (via `get_repo_root()`)
4. If yes: move all `.jsonl` files from the current bucket to the git root bucket
5. If no: leave the bucket as-is (non-repo projects stay in their cwd bucket)

**Pseudocode:**
```rust
fn migrate_transcript_buckets(config_dir: &Path) {
    let projects = config_dir.join("projects");
    if !projects.exists() { return; }
    
    for entry in fs::read_dir(&projects).flatten() {
        if !entry.path().is_dir() { continue; }
        
        let encoded = entry.file_name().to_string_lossy().to_string();
        let Ok(decoded_bytes) = URL_SAFE_NO_PAD.decode(&encoded) else { continue; };
        let Ok(decoded) = String::from_utf8(decoded_bytes) else { continue; };
        let old_path = PathBuf::from(&decoded);
        
        // Only migrate if the path is a subdirectory of a git repo
        let Some(git_root) = get_repo_root(&old_path) else { continue; };
        if git_root == old_path { continue; } // Already at git root
        
        let new_bucket = transcript_dir_in(config_dir, &git_root);
        fs::create_dir_all(&new_bucket).ok();
        
        for jsonl in fs::read_dir(&entry.path()).flatten() {
            let dest = new_bucket.join(jsonl.file_name());
            if dest.exists() { continue; } // Don't overwrite
            fs::rename(jsonl.path(), &dest).ok();
        }
        
        // Remove empty old bucket
        if fs::read_dir(&entry.path()).ok().map(|mut r| r.next().is_none()).unwrap_or(false) {
            fs::remove_dir(&entry.path()).ok();
        }
    }
}
```

**When to run:**
- At startup, before any transcript writes
- Idempotent: safe to run multiple times
- Non-blocking: can run in a background task

**Edge cases:**
- **Symlinked paths:** `get_repo_root()` walks from the symlink target, not the symlink. The migration should use the original (symlinked) path as the key, not the canonicalized one. This matches current transcript write behavior.
- **Deleted repos:** If the git repo no longer exists, `get_repo_root()` returns `None` and the bucket is left as-is.
- **Conflicting buckets:** If a git root bucket already exists (e.g., user sometimes launched from root, sometimes from subdirectory), merge the JSONL files. Files with the same session ID should not conflict because a session is only written by one process.

### Option B: Change Stats to Use Git Root

**How:** Change `stats.rs:217` to use `get_repo_root(&cwd) || cwd` instead of encoding `cwd` directly.

**Pros:** No data migration needed. Existing transcripts are already in git root buckets.
**Cons:** `/stats` now shows sessions from the entire git repo, not just the subdirectory the user is in. This is arguably more correct but changes behavior.

**Recommendation:** This is the simpler fix and should be done FIRST. Then Option A handles the historical data.

### Option C: Do Both (Recommended)

1. Fix `/stats` to use git root (Option B) — immediate behavioral fix
2. Migrate existing transcripts (Option A) — cleanup historical data

---

## Recommended Implementation Order — ALL COMPLETE

1. ✅ **Fix `stats.rs`** to use `get_repo_root(&cwd) || cwd` as the project identifier (Option B).
2. ✅ **Add migration function** to `session_storage.rs` (`migrate_cwd_transcript_buckets`).
3. ✅ **Run migration at startup** in `main.rs` before the interactive/headless branch.
4. ✅ **Add tests** for migration, stats-from-subdirectory, `project_root` stability, and teleport `project_dir`.

---

## Risks

| Risk | Mitigation | Status |
|------|-----------|--------|
| Migration runs during active session | Run once at startup, not in hot path | ✅ Implemented |
| Race condition (two instances migrating) | Use file locking on a marker file | Not implemented — migration is idempotent and `rename` is atomic; two concurrent runs converge to the same result |
| Git repo detection fails for shallow clones | `get_repo_root()` checks for `.git` dir existence, works for shallow clones | ✅ Inherently handled |
| User has legitimate reason for subdirectory bucket | Unlikely — the divergence is unintentional | Not implemented — no env override added; the divergence was a bug, not a feature |
| Base64 decoding fails for corrupt dir names | Skip those entries (they're already broken) | ✅ Implemented |

---

## Testing Strategy — ALL COMPLETE

1. ✅ **Unit test for `project_root`:** `project_root_is_stable_across_subdirectories` / `project_root_falls_back_to_cwd_outside_repo`.
2. ✅ **Integration test for migration:** `migrate_cwd_transcript_buckets_moves_subdirectory_transcripts` / `migrate_cwd_transcript_buckets_leaves_other_buckets_alone`.
3. ✅ **Regression test for stats:** `run_summary_from_subdirectory_finds_repo_root_sessions`.
4. ✅ **Manual test:** Debug binary launched from a git subdirectory; `clawde stats` reports the git-root session (verified end-to-end).
