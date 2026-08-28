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

- [ ] Make `/followups status` report useful bounded state: current/history counts, usage count, and lifecycle counts.
- [ ] Make keyboard and slash-command clear operations have identical semantics.
- [ ] Ensure clear operations clear all relevant lifecycle state and durable state.
- [ ] Add explicit tests for current/history keyboard and mouse selection.

## Priority 3: persistence and privacy

- [ ] Decide and document whether durable history is global, project-scoped, or session-scoped.
- [ ] Prefer project/session isolation for raw model-generated followup text.
- [ ] Add bounded retention, opt-out, and clear/reset behavior.
- [ ] Handle corrupt files, stale temporary files, concurrent processes, and schema evolution safely.
- [ ] Avoid synchronous filesystem writes on latency-sensitive input paths where practical.

## Priority 4: feedback quality

- [ ] Define the lifecycle schema for selected, submitted, completed, failed, and cancelled.
- [ ] Decide whether lifecycle aggregates persist independently of displayed history.
- [ ] Keep feedback deterministic, bounded, versioned, and safe for system-prompt insertion.
- [ ] Do not imply that missing lifecycle data means zero when it means unavailable.

## Priority 5: verification and cleanup

- [ ] Add regression tests for wrapping, scrolling, stale row maps, persistence reload, clear parity, and completion lifecycle.
- [ ] Test Tab chord behavior separately from followup behavior.
- [ ] Remove unnecessary `allow(dead_code)` annotations and rename inconsistent test identifiers.
- [ ] Run `cargo fmt --all`.
- [ ] Run `cargo check --workspace`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo test --workspace`.
- [ ] Commit only reviewed files belonging to this remediation.

## Immediate implementation order

1. Inspect the exact event and rendering paths. [x]
2. Introduce typed row targets and shared layout metadata. [x]
3. Fix wrapped-row hit testing and stale-map invalidation. [x]
4. Fix completion attribution against the verified event lifecycle. [x]
5. Add focused tests and run package-level verification. [x]
