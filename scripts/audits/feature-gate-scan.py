#!/usr/bin/env python3
"""Workspace feature-gate inventory guard.

A feature declared in a crate's `[features]` table must turn real code on or
off: it either owns a `cfg(feature = "<name>")` site, or it carries an optional
dependency (`dep:<crate>`). A declaration with neither compiles to nothing, and
because `dev_full` promises "every gated feature", every empty name makes that
promise false: `clawde-core` had accumulated 30 such names (ultraplan,
ultrathink, history_picker, quick_search, the agent/memory and ccr_* families,
…), while `crates/commands` declared `ultraplan = ["clawde-core/ultraplan"]`
for a feature that no longer existed anywhere.

The same class of error has a second shape: a pass-through (`x = ["other/feat"]`)
pointing at a dependency that does not declare `feat`. Cargo rejects that at
build time, but only for the configurations someone actually builds — the
`--features dev_full` line in docs/installation.md was broken this way because
`clawde-cli` declared no `dev_full` at all and CI never built it.

This scan is the inventory of record, run as a CI step. It reports per-crate,
per-feature verdicts and exits non-zero on:

  PHANTOM   the feature has no cfg site, no optional dep, and no member
            features: it gates nothing.
  DANGLING  a pass-through names `<crate>/<feature>` where a workspace crate
            does not declare `<feature>`.

It is not a snapshot of the current tree, so it needs no baseline file and does
not fail on legitimate new features; the condition it enforces is an invariant
that holds for the whole history of a healthy table.

Requires Python 3.11+ for `tomllib`.

Usage:
    python3 scripts/audits/feature-gate-scan.py [--root DIR] [--verbose]
                                                [--json PATH] [--warn-only]
                                                [--self-test]

Exit status: 0 = every declaration gates something, 1 = PHANTOM/DANGLING
verdicts found (or the detector self-test drifted), 2 = usage error.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
import tomllib

# ---------------------------------------------------------------------------
# Discovery
# ---------------------------------------------------------------------------

GATES = "gates"
GROUP = "group"
PASSTHROUGH = "pass-through"
PHANTOM = "PHANTOM"
DANGLING = "DANGLING"
UNCHECKED = "unchecked-external"


def load_manifest(path: pathlib.Path) -> dict:
    with path.open("rb") as fh:
        return tomllib.load(fh)


def optional_deps(manifest: dict) -> set[str]:
    """Names of dependencies marked `optional = true` in any dependency table."""
    names: set[str] = set()
    tables = [
        manifest.get("dependencies", {}),
        manifest.get("dev-dependencies", {}),
        manifest.get("build-dependencies", {}),
    ]
    for target in manifest.get("target", {}).values():
        tables.extend(
            target.get(key, {})
            for key in ("dependencies", "dev-dependencies", "build-dependencies")
        )
    for table in tables:
        for name, spec in table.items():
            if isinstance(spec, dict) and spec.get("optional") is True:
                names.add(name)
    return names


def features_of(manifest: dict) -> dict[str, list[str]]:
    raw = manifest.get("features", {})
    return {
        name: [e for e in entries if isinstance(e, str)]
        for name, entries in raw.items()
        if isinstance(entries, list)
    }


def collect_crate(manifest_path: pathlib.Path, root: pathlib.Path) -> tuple[str, dict] | None:
    manifest = load_manifest(manifest_path)
    package = manifest.get("package", {})
    name = package.get("name")
    if not isinstance(name, str):
        return None
    sources = {
        str(path.relative_to(root)): path.read_text(errors="replace")
        for path in sorted((manifest_path.parent / "src").rglob("*.rs"))
    }
    return name, {
        "features": features_of(manifest),
        "sources": sources,
        "optional_deps": optional_deps(manifest),
    }


def discover(root: pathlib.Path) -> dict[str, dict]:
    """Collect every workspace crate's features, Rust sources and optional deps."""
    workspace = root / "src-rust" / "Cargo.toml"
    manifest_root = workspace.parent if workspace.is_file() else root
    members = sorted((manifest_root / "crates").glob("*/Cargo.toml"))
    if workspace.is_file():
        declared = load_manifest(workspace).get("workspace", {}).get("members", [])
        for rel in declared:
            path = manifest_root / str(rel) / "Cargo.toml"
            if path.is_file() and path not in members:
                members.append(path)
    crates: dict[str, dict] = {}
    for manifest_path in sorted(members):
        collected = collect_crate(manifest_path, root)
        if collected is not None:
            crates[collected[0]] = collected[1]
    return crates


# ---------------------------------------------------------------------------
# Classification
# ---------------------------------------------------------------------------

def cfg_sites(sources: dict[str, str], name: str) -> int:
    """Occurrences of `feature = "<name>"` across a crate's Rust sources.

    `feature = "x"` realistically only appears inside `#[cfg(...)]`,
    `#[cfg_attr(...)]` and the `cfg!(...)` macro, so a plain occurrence count is
    both sufficient and immune to rustfmt's spacing variants.
    """
    pattern = re.compile(r'feature\s*=\s*"%s"' % re.escape(name))
    return sum(len(pattern.findall(text)) for text in sources.values())


def classify(crates: dict[str, dict]) -> list[dict]:
    """Classify every declared feature of every crate.

    `crates` maps a package name to `features` ({feature: [entry, ...]}),
    `sources` ({path: text}) and `optional_deps` (set of names).
    """
    verdicts: list[dict] = []
    resolving: set[tuple[str, str]] = set()

    def resolve(crate: str, feat: str) -> str:
        """Verdict for one feature, following groups and pass-through edges.

        A feature is real when it owns a cfg site, carries an optional
        dependency, or delegates to something real — a member feature of the
        same crate (a group such as `dev_full`) or a `<crate>/<feature>`
        passthrough into a dependency. The verdict of the *edges* propagates, so
        a group of phantoms is a phantom and a pass-through to a missing
        feature is dangling.
        """
        key = (crate, feat)
        if key in resolving:
            # A cyclic feature group: treat the edge as satisfied rather than
            # recursing forever (cargo accepts self-referential groups).
            return GATES
        decl = crates.get(crate, {}).get("features", {}).get(feat)
        if decl is None:
            return DANGLING
        resolving.add(key)
        try:
            if cfg_sites(crates[crate]["sources"], feat):
                return GATES
            if feat in crates[crate]["optional_deps"]:
                return GATES
            if any(entry.startswith("dep:") for entry in decl):
                return GATES
            saw_member = False
            saw_passthrough = False
            for entry in decl:
                if entry == feat or entry.startswith("dep:"):
                    continue
                if "/" in entry:
                    target, _, target_feat = entry.partition("/")
                    target = target.rstrip("?")
                    if target == crate:
                        continue
                    saw_passthrough = True
                    # An external crate cannot be introspected: accept the edge
                    # rather than inventing a failure for it.
                    if target in crates and resolve(target, target_feat) in (
                        PHANTOM,
                        DANGLING,
                    ):
                        return DANGLING
                else:
                    saw_member = True
                    if resolve(crate, entry) in (PHANTOM, DANGLING):
                        return PHANTOM
            if saw_member:
                return GROUP
            if saw_passthrough:
                return PASSTHROUGH
            return PHANTOM
        finally:
            resolving.discard(key)

    for crate in sorted(crates):
        info = crates[crate]
        for feat in sorted(info["features"]):
            entries = info["features"][feat]
            verdict = resolve(crate, feat)
            passthrough: list[dict] = []
            members: list[str] = []
            for entry in entries:
                if entry.startswith("dep:"):
                    continue
                if "/" in entry:
                    target, _, target_feat = entry.partition("/")
                    target = target.rstrip("?")
                    if target == crate:
                        continue
                    resolved = (
                        resolve(target, target_feat) if target in crates else UNCHECKED
                    )
                    passthrough.append(
                        {"entry": entry, "target": target, "resolved": resolved}
                    )
                elif entry != feat:
                    members.append(entry)

            verdicts.append(
                {
                    "crate": crate,
                    "feature": feat,
                    "verdict": verdict,
                    "cfg_sites": cfg_sites(info["sources"], feat),
                    "optional_dep": feat in info["optional_deps"],
                    "entries": entries,
                    "pass_through": passthrough,
                    "members": members,
                }
            )
    return verdicts


# ---------------------------------------------------------------------------
# Self-test: the classifier must stay honest about its own fixtures
# ---------------------------------------------------------------------------

MANIFEST_FIXTURE = """
[package]
name = "sut"

[features]
voice = ["dep:cpal"]
dev_full = [
    "voice",
    "shot_stats",
]

[dependencies]
cpal = { version = "0.15", optional = true }
"""


def self_test() -> int:
    """Validate the parser and the classifier against built-in fixtures."""
    failures = 0

    def check(label: str, expected, got) -> None:
        nonlocal failures
        if expected != got:
            print(f"  self-test FAIL: {label}: expected {expected!r}, got {got!r}")
            failures += 1

    # Parser: a multi-line array (the shape the real `dev_full` had) and an
    # optional dependency must survive parsing.
    manifest = tomllib.loads(MANIFEST_FIXTURE)
    check(
        "multi-line feature array",
        {"voice": ["dep:cpal"], "dev_full": ["voice", "shot_stats"]},
        features_of(manifest),
    )
    check("optional dependency", {"cpal"}, optional_deps(manifest))

    real_core = {
        "features": {"dev_full": ["voice"], "voice": ["dep:cpal"]},
        "sources": {},
        "optional_deps": {"cpal"},
    }
    fixtures: list[tuple[str, dict, dict, str]] = [
        (
            "gated by a cfg site",
            {"features": {"shot_stats": []}, "sources": {"a.rs": '#[cfg(feature = "shot_stats")]'}},
            {"optional_deps": set()},
            GATES,
        ),
        (
            "gated by an explicit optional dependency",
            {"features": {"voice": ["dep:cpal"]}, "sources": {}},
            {"optional_deps": {"cpal"}},
            GATES,
        ),
        (
            # An optional dependency with no `dep:` mention becomes an implicit
            # feature named after the dependency.
            "gated by an implicit optional-dependency feature",
            {"features": {"cpal": []}, "sources": {}},
            {"optional_deps": {"cpal"}},
            GATES,
        ),
        (
            "phantom: no cfg site, no dep, no members",
            {"features": {"teammem": []}, "sources": {"a.rs": "pub fn f() {}"}},
            {"optional_deps": set()},
            PHANTOM,
        ),
        (
            "group of real features",
            {
                "features": {
                    "dev_full": ["voice", "shot_stats"],
                    "voice": ["dep:cpal"],
                    "shot_stats": [],
                },
                "sources": {"a.rs": '#[cfg(feature = "shot_stats")]'},
            },
            {"optional_deps": {"cpal"}},
            GROUP,
        ),
        (
            "group is only as real as its members",
            {"features": {"dev_full": ["gone"], "gone": []}, "sources": {}},
            {"optional_deps": set()},
            PHANTOM,
        ),
        (
            "pass-through to a declared feature",
            {"features": {"dev_full": ["clawde-core/dev_full"]}, "sources": {}},
            {"optional_deps": set()},
            PASSTHROUGH,
        ),
        (
            # Regression: `clawde-cli`'s `default = ["voice"]`, where `voice` is
            # itself a pass-through. Resolving the member edge through the
            # pass-through table must not report the group as a phantom.
            "group whose member is a pass-through",
            {"features": {"default": ["voice"], "voice": ["clawde-core/voice"]}, "sources": {}},
            {"optional_deps": set()},
            GROUP,
        ),
        (
            "pass-through to a feature that does not exist",
            {"features": {"ultraplan": ["clawde-core/ultraplan"]}, "sources": {}},
            {"optional_deps": set()},
            DANGLING,
        ),
    ]
    for label, crate_info, extra, expected in fixtures:
        crates = {"clawde-core": dict(real_core), "sut": {**crate_info, **extra}}
        found = [v for v in classify(crates) if v["crate"] == "sut"]
        check(label, expected, found[0]["verdict"] if found else "no verdict")

    if failures:
        print(
            "feature-gate-scan: detector self-test failed — the classifier is "
            "not reporting what it claims, so its all-clear means nothing."
        )
        return 1
    print(f"feature-gate-scan: detector self-test passed ({len(fixtures) + 2} fixtures)")
    return 0


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

VERDICT_DETAIL = {
    PHANTOM: "no cfg site, no optional dep, no member features",
    DANGLING: "pass-through target declares no such feature",
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--root",
        default=str(pathlib.Path(__file__).resolve().parents[2]),
        help="repository root containing src-rust/ (default: the script's repo)",
    )
    parser.add_argument("--verbose", action="store_true", help="list every feature")
    parser.add_argument("--json", metavar="PATH", help="write the inventory as JSON")
    parser.add_argument("--warn-only", action="store_true", help="report but exit 0")
    parser.add_argument(
        "--self-test", action="store_true", help="run the detector fixtures only"
    )
    args = parser.parse_args(argv)

    if self_test() != 0:
        return 1
    if args.self_test:
        return 0

    root = pathlib.Path(args.root).resolve()
    crates = discover(root)
    if not crates:
        print(f"feature-gate-scan: no workspace crates found under {root}")
        return 2

    verdicts = classify(crates)
    by_crate: dict[str, int] = {}
    for verdict in verdicts:
        by_crate[verdict["crate"]] = by_crate.get(verdict["crate"], 0) + 1
    print(
        "feature-gate-scan: checked %d feature declaration(s) across %d crate(s)"
        % (len(verdicts), len(by_crate))
    )

    if args.verbose:
        for verdict in verdicts:
            evidence = []
            if verdict["cfg_sites"]:
                evidence.append(f"{verdict['cfg_sites']} cfg site(s)")
            if verdict["optional_dep"]:
                evidence.append("optional dep")
            if any(e.startswith("dep:") for e in verdict["entries"]):
                evidence.append("dep: entry")
            if verdict["members"]:
                evidence.append("members: " + ", ".join(verdict["members"]))
            if verdict["pass_through"]:
                evidence.append(
                    "pass-through: "
                    + ", ".join(
                        f"{p['entry']}->{p['resolved']}" for p in verdict["pass_through"]
                    )
                )
            print(
                "  %-12s %s:%s (%s)"
                % (
                    verdict["verdict"],
                    verdict["crate"],
                    verdict["feature"],
                    "; ".join(evidence) or "no evidence",
                )
            )

    if args.json:
        pathlib.Path(args.json).write_text(
            json.dumps(
                {
                    "crates_checked": len(by_crate),
                    "features_checked": len(verdicts),
                    "verdicts": verdicts,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n"
        )
        print(f"feature-gate-scan: inventory written to {args.json}")

    bad = [v for v in verdicts if v["verdict"] in VERDICT_DETAIL]
    if not bad:
        print("feature-gate-scan: every declared feature gates real code")
        return 0

    print(
        f"feature-gate-scan: {len(bad)} declaration(s) gate nothing or point at a "
        "feature that does not exist:"
    )
    for verdict in bad:
        print(
            f"  {verdict['verdict']:9s} {verdict['crate']}:{verdict['feature']} — "
            f"{VERDICT_DETAIL[verdict['verdict']]}"
        )
        print(f"            declared as {verdict['entries']}")
    print(
        "  Either gate real code with the name (add the cfg site), make it carry an"
    )
    print(
        "  optional dependency, or delete the declaration: an empty name makes"
    )
    print(
        "  `dev_full`'s 'every gated feature' promise false, and unbuilt"
        " capabilities"
    )
    print("  belong in docs/plans/.")
    return 0 if args.warn_only else 1


if __name__ == "__main__":
    sys.exit(main())
