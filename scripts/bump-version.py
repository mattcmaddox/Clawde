#!/usr/bin/env python3
"""Stamp a new Clawde version into every canonical source.

Usage:
  scripts/bump-version.py vMAJOR.MINOR.PATCH   stamp the version
  scripts/bump-version.py --list-paths         print the stamped files, one per line

Touches:
  - src-rust/Cargo.toml                 workspace.package.version
  - src-rust/Cargo.lock                 12 clawde* workspace package entries
  - npm/package.json                    version field
  - README.md                           shields.io badge (text + alt) + Beta callout
  - docs/index.md                       **Version:** line
  - docs/installation.md                "clawde X.Y.Z" sample output
  - src-rust/crates/acp/registry-template/agent.json
                                        version field + release download URLs (one per
                                        published platform; Linux-only today)

Fails loudly if any expected pattern is missing — that means the file shape
changed and the script needs updating, not silently producing a half-stamped
release.

SURFACE_PATHS is the single source of truth for *which* files this script
stamps. Callers that must stage exactly those files (scripts/build.sh,
.github/workflows/release.yml) read it through `--list-paths` rather than
repeating the list by hand, so adding a surface here is the only edit a new
stamped file needs.
"""
from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# Relative to ROOT and forward-slashed, so the same strings double as git pathspecs.
CARGO_TOML = "src-rust/Cargo.toml"
CARGO_LOCK = "src-rust/Cargo.lock"
NPM_PACKAGE = "npm/package.json"
README = "README.md"
DOCS_INDEX = "docs/index.md"
DOCS_INSTALLATION = "docs/installation.md"
ACP_AGENT = "src-rust/crates/acp/registry-template/agent.json"

# The canonical stamp surface, in the order the files are written below.
SURFACE_PATHS = (
    CARGO_TOML,
    CARGO_LOCK,
    NPM_PACKAGE,
    README,
    DOCS_INDEX,
    DOCS_INSTALLATION,
    ACP_AGENT,
)


def die(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def replace(path: Path, pattern: str, repl: str, *, count: int = 0, flags: int = re.MULTILINE) -> None:
    text = path.read_text(encoding="utf-8")
    new, n = re.subn(pattern, repl, text, count=count, flags=flags)
    if n == 0:
        die(f"no matches for {pattern!r} in {path.relative_to(ROOT)}")
    if new != text:
        path.write_text(new, encoding="utf-8")
    print(f"  {path.relative_to(ROOT)}: {n} replacement(s)")


def bump_cargo_lock(version: str) -> None:
    """Rewrite every workspace [[package]] block (those without a `source = ` line)."""
    path = ROOT / CARGO_LOCK
    text = path.read_text(encoding="utf-8")

    blocks = re.split(r"(?=^\[\[package\]\]$)", text, flags=re.MULTILINE)
    touched = 0
    for i, block in enumerate(blocks):
        if not block.startswith("[[package]]"):
            continue
        if re.search(r"^source = ", block, flags=re.MULTILINE):
            continue  # registry / git dep — leave alone
        name_match = re.search(r'^name = "([^"]+)"', block, flags=re.MULTILINE)
        if not name_match or not name_match.group(1).startswith("clawde"):
            continue  # any future non-clawde path dep — also leave alone
        new_block, n = re.subn(
            r'^version = "[^"]+"$',
            f'version = "{version}"',
            block,
            count=1,
            flags=re.MULTILINE,
        )
        if n != 1:
            die(f"Cargo.lock: workspace block for {name_match.group(1)} had no version line")
        blocks[i] = new_block
        touched += 1

    if touched == 0:
        die("Cargo.lock: found zero workspace package blocks — file shape changed?")
    path.write_text("".join(blocks), encoding="utf-8")
    print(f"  {CARGO_LOCK}: {touched} workspace package(s)")


def main() -> None:
    if len(sys.argv) == 2 and sys.argv[1] == "--list-paths":
        # Machine-readable: one repo-relative path per line, feed straight to `git add`.
        for rel in SURFACE_PATHS:
            print(rel)
        return

    if len(sys.argv) != 2:
        die("usage: bump-version.py vMAJOR.MINOR.PATCH | --list-paths")

    tag = sys.argv[1]
    m = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", tag)
    if not m:
        die(f"invalid tag {tag!r} — expected vMAJOR.MINOR.PATCH")
    version = f"{m.group(1)}.{m.group(2)}.{m.group(3)}"

    print(f"Stamping version {version} ({tag}):")

    # 1. Cargo.toml (workspace.package.version — first `version = "..."` line)
    replace(
        ROOT / CARGO_TOML,
        r'^version = "\d+\.\d+\.\d+"$',
        f'version = "{version}"',
        count=1,
    )

    # 2. Cargo.lock — every clawde* workspace package
    bump_cargo_lock(version)

    # 3. npm/package.json
    pkg_path = ROOT / NPM_PACKAGE
    pkg = json.loads(pkg_path.read_text(encoding="utf-8"))
    pkg["version"] = version
    pkg_path.write_text(json.dumps(pkg, indent=2) + "\n", encoding="utf-8")
    print(f"  {NPM_PACKAGE}")

    # 4. README.md badge + Beta callout
    readme = ROOT / README
    replace(readme, r"Version-\d+\.\d+\.\d+-2E8B57", f"Version-{version}-2E8B57", count=1)
    replace(readme, r'alt="Version \d+\.\d+\.\d+"', f'alt="Version {version}"', count=1)
    replace(readme, r"Beta \(v\d+\.\d+\.\d+\)", f"Beta (v{version})", count=1)

    # 5. docs/index.md
    replace(
        ROOT / DOCS_INDEX,
        r"\*\*Version:\*\* \d+\.\d+\.\d+",
        f"**Version:** {version}",
        count=1,
    )

    # 6. docs/installation.md — sample output line ("clawde X.Y.Z")
    replace(
        ROOT / DOCS_INSTALLATION,
        r"^clawde \d+\.\d+\.\d+$",
        f"clawde {version}",
        count=1,
    )

    # 7. ACP registry template — version field + release download URLs
    agent = ROOT / ACP_AGENT
    text = agent.read_text(encoding="utf-8")
    text, n_v = re.subn(r'"version": "\d+\.\d+\.\d+"', f'"version": "{version}"', text, count=1)
    text, n_u = re.subn(
        r"/releases/download/v\d+\.\d+\.\d+/",
        f"/releases/download/v{version}/",
        text,
    )
    if n_v != 1:
        die("agent.json: version field not found")
    if n_u == 0:
        die("agent.json: no /releases/download/vX.Y.Z/ URLs found")
    agent.write_text(text, encoding="utf-8")
    print(f"  {ACP_AGENT}: 1 version + {n_u} URL(s)")

    print(f"\nStamped {version}.")


if __name__ == "__main__":
    main()
