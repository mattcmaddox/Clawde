#!/usr/bin/env python3
"""Workspace unwired-code scanner.

Enumerates every item that the dead-code guard skips or that is clearly
unwired, so the audit has a complete inventory rather than a manual one.

For each `#[allow(dead_code)]`-annotated `pub fn`/`pub async fn` and each
`pub mod`, report the file + name, and count how many times the name appears
workspace-wide (1 = its own declaration = fully unwired).

What this is NOT
----------------
It is an inventory, not a verdict. `#[allow(dead_code)]` is used deliberately in
this workspace as reserved surface (~265 sites), and a name mentioned elsewhere
is not proof of wiring — the count is lexical. Treat every hit as a lead that
needs a one-line "is this capability reachable?" check before any retire or
implement decision. Two shapes in particular read as findings and are not:

  * `pub mod` — rustc never reports a public module as dead code. Section C
    compares mentions *outside the declaring file*, which a name re-exported or
    registered from `lib.rs` may legitimately have zero of.
  * a name mentioned only in a doc comment or a test.

Precision notes: the counter is occurrence-based, so a bare `UNWIRED` means the
name occurs exactly once in the whole workspace — never outside its own
declaration. The identifier sweep includes comments and string literals, so a
name that appears only in prose counts as referenced.

Exit status: 0 when the inventory is produced, 1 when `--self-test` detects a
drifted counter (the fixtures below run on every invocation, so a rotted
detector fails loudly instead of reporting a false all-clear), and — only with
`--fail-on-unwired` — 1 when at least one fully unwired item exists. CI runs the
default report-only mode and records the inventory; the check that gates the
tree is `feature-gate-scan.py`, whose condition is a true invariant.

Usage:
    python3 scripts/audits/unwired-code-scan.py [--root DIR] [--json PATH]
                                                [--summary] [--fail-on-unwired]
                                                [--self-test]
"""
from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import sys

FN_RE = re.compile(r"\bpub\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
MOD_RE = re.compile(r"\bpub\s+mod\s+([A-Za-z_][A-Za-z0-9_]*)")
IDENT_RE = re.compile(r"\b([A-Za-z_][A-Za-z0-9_]*)\b")


def collect_rs_files(root: pathlib.Path) -> list[pathlib.Path]:
    found: list[pathlib.Path] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for filename in filenames:
            if filename.endswith(".rs"):
                found.append(pathlib.Path(dirpath) / filename)
    return sorted(found)


def load_sources(root: pathlib.Path) -> dict[str, str]:
    return {
        str(path.relative_to(root)): path.read_text(errors="replace")
        for path in collect_rs_files(root)
    }


def count_identifiers(
    sources: dict[str, str],
) -> tuple[dict[str, int], dict[str, set[str]]]:
    """Total occurrences per identifier, plus the files each name appears in.

    The old implementation incremented once per *file*, so a name used five
    times in one file scored the same as a name never used at all, and anything
    mentioned in one file was reported as unwired. Occurrences are counted
    instead: a fully unwired name scores exactly 1 (its own declaration).
    """
    counts: dict[str, int] = {}
    files_by_name: dict[str, set[str]] = {}
    for path, text in sources.items():
        for match in IDENT_RE.finditer(text):
            name = match.group(1)
            counts[name] = counts.get(name, 0) + 1
            files_by_name.setdefault(name, set()).add(path)
    return counts, files_by_name


def preceded_by_allow_dead_code(content: str, start: int, lookback: int = 5) -> bool:
    end = start
    for _ in range(lookback):
        newline = content[:end].rfind("\n")
        if newline == -1:
            break
        line = content[newline + 1 : end].strip()
        if line.startswith("#[allow(dead_code") or line.startswith("#[cfg_attr(not(feature"):
            return True
        end = newline
    return False


def dead_code_fns(
    sources: dict[str, str], counts: dict[str, int]
) -> list[dict]:
    """`#[allow(dead_code)]`-annotated public fns with their occurrence count."""
    items: list[dict] = []
    for path in sorted(sources):
        content = sources[path]
        for match in FN_RE.finditer(content):
            if not preceded_by_allow_dead_code(content, match.start()):
                continue
            name = match.group(1)
            count = counts.get(name, 0)
            items.append(
                {
                    "file": path,
                    "name": name,
                    "occurrences": count,
                    "unwired": count <= 1,
                }
            )
    return items


def pub_mods(
    sources: dict[str, str], files_by_name: dict[str, set[str]], counts: dict[str, int]
) -> list[dict]:
    """`pub mod` declarations and whether anything else ever names the module.

    A module used only from its own declaring file (`pub mod x;` plus
    `x::foo()` in the same file) is wired, so the candidate test is total
    occurrences <= 1 — nothing anywhere but the declaration line.
    """
    items: list[dict] = []
    for path in sorted(sources):
        for match in MOD_RE.finditer(sources[path]):
            name = match.group(1)
            elsewhere = len(files_by_name.get(name, set()) - {path})
            occurrences = counts.get(name, 0)
            items.append(
                {
                    "file": path,
                    "name": name,
                    "occurrences": occurrences,
                    "files_mentioning_elsewhere": elsewhere,
                    "unwired": occurrences <= 1,
                }
            )
    return items


# ---------------------------------------------------------------------------
# Self-test: the counter must stay honest about its own fixtures
# ---------------------------------------------------------------------------

SELF_TEST_SOURCES = {
    "crates/alpha/src/lib.rs": (
        "pub mod wired;\n"
        "pub mod orphan;\n"
        "#[allow(dead_code)]\n"
        "pub fn lonely() {}\n"
        "#[allow(dead_code)]\n"
        "pub fn reused() {}\n"
    ),
    "crates/alpha/src/wired.rs": "pub struct Thing;\n",
    "crates/alpha/src/caller.rs": (
        "pub fn call() { reused(); }\n"
        "// wired::Thing is re-exported from lib.rs\n"
    ),
}


def self_test() -> int:
    failures = 0
    counts, files_by_name = count_identifiers(SELF_TEST_SOURCES)
    fns = {item["name"]: item for item in dead_code_fns(SELF_TEST_SOURCES, counts)}
    mods = {
        item["name"]: item
        for item in pub_mods(SELF_TEST_SOURCES, files_by_name, counts)
    }

    def check(label: str, expected, got) -> None:
        nonlocal failures
        if expected != got:
            print(f"  self-test FAIL: {label}: expected {expected!r}, got {got!r}")
            failures += 1

    check("declaration names itself once", 1, counts.get("lonely"))
    check("reused fn counts its call site", 2, counts.get("reused"))
    check("lonely is unwired", True, fns["lonely"]["unwired"])
    check("reused is wired", False, fns["reused"]["unwired"])
    check("a mod nobody names is a candidate", True, mods["orphan"]["unwired"])
    check(
        "a mod used from another file is wired",
        False,
        mods["wired"]["unwired"],
    )
    check("wired mod is mentioned elsewhere", 1, mods["wired"]["files_mentioning_elsewhere"])

    if failures:
        print(
            "unwired-code-scan: detector self-test failed — the counter is not "
            "reporting what it claims, so its inventory means nothing."
        )
        return 1
    print("unwired-code-scan: detector self-test passed (7 fixtures)")
    return 0


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root",
        default=str(pathlib.Path(__file__).resolve().parents[2] / "src-rust"),
        help="workspace root to scan (default: <repo>/src-rust)",
    )
    parser.add_argument("--json", metavar="PATH", help="write the inventory as JSON")
    parser.add_argument(
        "--summary", action="store_true", help="print counts only, no per-item lines"
    )
    parser.add_argument(
        "--fail-on-unwired",
        action="store_true",
        help="exit 1 when a fully unwired item exists (off: this is an inventory)",
    )
    parser.add_argument(
        "--self-test", action="store_true", help="run the detector fixtures only"
    )
    args = parser.parse_args(argv)

    if self_test() != 0:
        return 1
    if args.self_test:
        return 0

    root = pathlib.Path(args.root).resolve()
    sources = load_sources(root)
    if not sources:
        print(f"unwired-code-scan: no Rust sources found under {root}")
        return 2

    counts, files_by_name = count_identifiers(sources)
    fns = dead_code_fns(sources, counts)
    mods = pub_mods(sources, files_by_name, counts)
    unwired = [item for item in fns if item["unwired"]]
    candidates = [item for item in mods if item["unwired"]]

    print("=" * 100)
    print("A. ITEMS MARKED #[allow(dead_code)] that the guard skips")
    print("   (UNWIRED = the name occurs exactly once workspace-wide: its own")
    print("    declaration. Anything else is a lead, not a finding.)")
    print("=" * 100)
    if not args.summary:
        for item in fns:
            flag = "UNWIRED" if item["unwired"] else f"({item['occurrences']} refs)"
            print(f"{flag:10s} {item['file']}: {item['name']}")

    print()
    print("=" * 100)
    print("B. UNWIRED #[allow(dead_code)] pub fns (occurrences <= 1), by module")
    print("=" * 100)
    if not args.summary:
        for item in unwired:
            print(f"{item['file']:55s} {item['name']}")

    print()
    print("=" * 100)
    print("C. pub mod declarations nothing in the workspace ever names again")
    print("   (candidates for whole-module scaffold; rustc never reports a public")
    print("    module as dead code, and a module used only from its own declaring")
    print("    file is wiring, not a candidate)")
    print("=" * 100)
    if not args.summary:
        for item in candidates:
            print(f"{item['file']:55s} {item['name']}")

    print()
    print(
        f"unwired-code-scan: {len(fns)} #[allow(dead_code)] pub fn(s), "
        f"{len(unwired)} fully unwired; {len(mods)} pub mod(s), "
        f"{len(candidates)} named nowhere but their declaration"
    )

    if args.json:
        pathlib.Path(args.json).write_text(
            json.dumps(
                {
                    "rust_files": len(sources),
                    "allow_dead_code_fns": fns,
                    "pub_mods": mods,
                    "counts": {
                        "allow_dead_code_fns": len(fns),
                        "unwired_fns": len(unwired),
                        "pub_mods": len(mods),
                        "unwired_pub_mods": len(candidates),
                    },
                },
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        print(f"unwired-code-scan: inventory written to {args.json}")

    if unwired and args.fail_on_unwired:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
