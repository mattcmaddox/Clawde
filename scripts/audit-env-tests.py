#!/usr/bin/env python3
"""Audit: ensure no test-module code mutates process-global state without a lock.

Process-global state (env vars via `set_var`/`remove_var`, the working dir via
`set_current_dir`) races when `cargo test` runs tests in parallel. Every such
mutation inside a `#[cfg(test)]` module must be protected by a lock guard held
for the whole test fn (e.g. `crate::paths::ENV_LOCK`, a local `ENV_LOCK`,
`CLAWDE_HOME_LOCK`, or `HOME_LOCK`).

Usage: python3 scripts/audit-env-tests.py   (exit 1 if unguarded sites found)
"""
import re
import pathlib
import sys

# Guard sources (heuristics, see header for rationale):
#  1. Any identifier bound to a mutex guard (the patterns in use today):
#        let _lock = ...ENV_LOCK.lock()...
#        let _guard = ENV_LOCK.lock()...
#        let _lock = ...Mutex::new(())...   (local static locks)
#  2. Any `let _name = ...` binding: underscore-prefixed bindings exist to
#     keep a value alive (guard holders, test-home helpers like
#     `let _home = TestHome::new();`).
GUARD_RE = re.compile(
    r"(?:lock|guard|_g|_lock|_guard)\s*(?:=|:|\()"
    r"|\.lock\(\)"
    r"|Mutex::new\(\)"
    r"|OnceLock::new\(\)"
    r"|let\s+_[a-zA-Z][a-zA-Z0-9_]*\s*="
)
# Process-global mutations that must be guarded when inside test modules.
MUTATION_RE = re.compile(r"set_var|remove_var|set_current_dir")


def find_test_blocks(lines):
    """Return (start, end) line indexes of `#[cfg(test)]` modules."""
    blocks = []
    for i, line in enumerate(lines):
        if "#[cfg(test)]" not in line:
            continue
        # The item following the attribute must open a block (mod/fn/impl).
        j = i + 1
        while j < len(lines) and not lines[j].strip():
            j += 1
        if j >= len(lines) or not re.match(r"\s*(pub\s+)?(mod|fn|struct|impl)\b", lines[j]):
            continue
        d = lines[j].count("{") - lines[j].count("}")
        start = j
        while j < len(lines) and d > 0:
            j += 1
            if j >= len(lines):
                break
            d += lines[j].count("{") - lines[j].count("}")
        blocks.append((start, j - 1))
    return blocks


def enclosing_fn_start(lines, i):
    """Index of the enclosing `fn` line, or None."""
    depth = 0
    for j in range(i, -1, -1):
        depth += lines[j].count("}") - lines[j].count("{")
        if re.search(r"\bfn\b", lines[j]) and "{" in lines[j] and depth <= 0:
            return j
    return None


def is_guarded(lines, fn_start, i):
    """True if the mutation at i (inside fn_start..) is protected by a guard.

    In addition to the guard-source regex, a mutation inside `fn drop` is
    assumed safe: `TestHome`-style helpers restore env vars in Drop while the
    struct still owns its lock-guard field (fields drop after Drop::drop runs).
    """
    window = lines[fn_start:i + 1]
    if any(GUARD_RE.search(w) for w in window):
        return True
    if fn_start is not None and re.search(r"fn\s+drop\s*\(", lines[fn_start]):
        return True
    return False


def main() -> int:
    issues = []
    total = 0
    for p in sorted(pathlib.Path(__file__).resolve().parent.parent.rglob("*.rs")):
        if "target" in p.parts:
            continue
        lines = p.read_text(errors="replace").splitlines()
        blocks = find_test_blocks(lines)
        for i, line in enumerate(lines):
            if not MUTATION_RE.search(line):
                continue
            total += 1
            stripped = line.strip()
            if stripped.startswith("//"):
                continue  # comment, not a mutation
            if not any(s <= i <= e for s, e in blocks):
                continue  # production code, not a test concern
            fn_start = enclosing_fn_start(lines, i)
            if not is_guarded(lines, fn_start, i):
                issues.append(f"{p}:{i + 1}: {stripped[:90]}")
    print(f"checked {total} process-global mutations across the workspace")
    if issues:
        print(f"UNGUARDED test-module mutations: {len(issues)}")
        for it in issues:
            print("  " + it)
        return 1
    print("all test-module mutations are lock-guarded")
    return 0


if __name__ == "__main__":
    sys.exit(main())
