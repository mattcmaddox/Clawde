# Followup Feature Audit TODO

## Scope

Audit and harden the followup suggestion feature without sweeping unrelated work into the change. The current worktree contains followup changes mixed with mode, keybinding, and other edits; those must be separated before committing.

## Priority 1: correctness blockers

- [x] Replace bare followup row indexes with a typed target that records whether the row belongs to current suggestions or durable history.
- [x] Build click targets from the same logical followup layout used for rendering.
- [x] Account for wrapped followup text and reason lines so every visual row maps to the correct logical suggestion.
- [x] Clear or invalidate stale row maps whenever transcript, followup mode, selection, or viewport changes.
- [x] Confirm the actual `QueryEvent` ordering, especially `MessageStop`, stream flushes, and `TurnComplete`.
- [x] Make completion attribution depend on reliable successful assistant-output state rather than a buffer that may already have been flushed.
- [x] Ensure error, cancellation, interruption, abort, and empty-output paths clear pending attribution and never increment completion counts.

## Priority 2: command and lifecycle consistency

- [x] `/followups status` reports real bounded state: current/saved/usage counts, top-5 lifecycle rows, and storage location (`App::followup_status_report`).
- [x] Keyboard and slash-command clear operations share identical semantics via `App::clear_followups` (in-memory lists, lifecycle counts, persisted files, md mirror).
- [x] Clear operations clear all relevant lifecycle + durable state.
- [x] Explicit tests for current/history keyboard + mouse selection; also fixed Down-from-no-selection skipping the first followup.

## Priority 3: persistence and privacy (DECIDED: project-scoped, .clawde/ in project root)

LOCKED by user on 2026-08-28:
- Durable history is project-scoped (not global ~/.clawde).
- Data lives in `.clawde/` inside the project root (git root via `git_utils::project_root`, cwd fallback).
- Machine-readable JSON is the source of truth; a generated `.clawde/followups.md` is a human-readable mirror (never re-parsed for feedback).
- Lifecycle aggregates (selected/submitted/completed) persist with the same project scope.

- [x] Migrate `followup_history.json` / `followup_usage.json` from `~/.clawde` to the project dir; read fallback when the old global file exists (`load_preferring`), legacy file removed on first project save (`save_migrating`).
- [x] Write `.clawde/followups.md` mirror on change (history save, usage record, clear) — generated only, never parsed back.
- [x] Add bounded retention and clear/reset behavior (caps apply: 20 history items, 64 usage entries).
- [x] Handle corrupt files, stale temporary files, concurrent processes safely (tmp+rename atomic; corrupt JSON degrades to empty, never crashes; stale `.tmp` files are never read).
- [ ] Schema version field — deferred to P4, when the lifecycle schema actually extends the file shape.
- [x] Avoid synchronous filesystem writes on latency-sensitive paths (saves are user-initiated: selection, turn end, clear).

## Priority 4: feedback quality (DECIDED: aggregates persist per-project, injection stays automatic)

LOCKED by user on 2026-08-28:
- Followup-usage feedback stays auto-injected into the system prompt on every submit.
- Lifecycle aggregates persist (project-scoped JSON) so the learning signal survives restarts.

- [x] Lifecycle schema V2: `FollowupLifecycle { selected, submitted, completed }` persisted per text in `followup_usage.json` (version field added — closes the P3 deferral). Legacy V1 files (selection-only counts) migrate on load.
- [x] Failed/cancelled turns are the complement (`submitted − completed`), not separate counters — the completion guard already excludes error/cancel/abort/interrupt stops.
- [x] Deterministic, bounded, versioned, prompt-safe: top-5 by selected desc, JSON-escaped + newline-free, 64-entry cap evicting least-used, saturating counters.
- [x] Missing ≠ zero: counts persist across restarts, so the summary never shows a misleading zero after relaunch; empty stores inject nothing.
- [x] Summary, markdown mirror, and `/followups status` all read the same sorted lifecycle store (single source of truth).

## Priority 5: verification and cleanup — DONE (faa8d05, 0.2.78)

- [x] Add regression tests for wrapping, scrolling, stale row maps, persistence reload, clear parity, and completion lifecycle.
- [x] Test Tab chord behavior separately from followup behavior (committed with the Tab-chord feature, 8a20865).
- [x] Remove unnecessary `allow(dead_code)` annotations and rename inconsistent test identifiers.
- [x] Run `cargo fmt --all`.
- [x] Run `cargo check --workspace`.
- [x] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] Run `cargo test --workspace` (TUI 1,106 passed, 0 failures).
- [x] Commit only reviewed files belonging to this remediation.

## Immediate implementation order

## Immediate implementation order

1. Inspect the exact event and rendering paths. [x]
2. Introduce typed row targets and shared layout metadata. [x]
3. Fix wrapped-row hit testing and stale-map invalidation. [x]
4. Fix completion attribution against the verified event lifecycle. [x]
5. Add focused tests and run package-level verification. [x]
