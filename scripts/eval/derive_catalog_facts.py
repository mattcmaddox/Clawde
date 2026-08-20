#!/usr/bin/env python3
"""Derive the FREE_CATALOG upstream facts from source.

Parses `FREE_CATALOG` in crates/api/src/providers/free/catalog.rs and emits the
upstream ids + default models as JSON. The catalog-order eval fixture consumes
this so its "name at least N upstreams" assertion never rots when the catalog
changes: regenerate the fixture facts with

    python3 scripts/eval/derive_catalog_facts.py --out scripts/eval/fixtures/catalog-order/catalog_facts.json
"""

import argparse
import hashlib
import json
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
CATALOG_RS = REPO_ROOT / "src-rust" / "crates" / "api" / "src" / "providers" / "free" / "catalog.rs"


def source_sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def parse_catalog(path: Path) -> list[dict]:
    src = path.read_text()
    constants = dict(
        re.findall(r'const\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"([^"]+)"', src)
    )
    # Entries are `FreeUpstream { id: "x", title: ..., default_model: "...", ... }`
    entries = []
    for m in re.finditer(r"FreeUpstream\s*\{", src):
        start = m.start()
        depth = 0
        i = src.index("{", start)
        j = i
        while j < len(src):
            c = src[j]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        block = src[i + 1 : j]
        id_m = re.search(r'id:\s*"([^"]+)"', block)
        model_m = re.search(r'default_model:\s*(?:"([^"]+)"|([A-Z][A-Z0-9_]*))', block)
        title_m = re.search(r'title:\s*"([^"]+)"', block)
        if id_m:
            model = (model_m.group(1) or constants.get(model_m.group(2), "")) if model_m else ""
            entries.append(
                {
                    "id": id_m.group(1),
                    "title": title_m.group(1) if title_m else "",
                    "default_model": model,
                }
            )
    return entries


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--out", type=Path, help="Write JSON to this path (default: stdout)")
    args = ap.parse_args()

    if not CATALOG_RS.exists():
        print(f"error: catalog source not found at {CATALOG_RS}", file=sys.stderr)
        return 1

    entries = parse_catalog(CATALOG_RS)
    if not entries:
        print("error: no FreeUpstream entries parsed", file=sys.stderr)
        return 1

    facts = {
        "source": str(CATALOG_RS.relative_to(REPO_ROOT)),
        "source_sha256": source_sha256(CATALOG_RS),
        "upstreams": entries,
        "ids": [e["id"] for e in entries],
    }
    payload = json.dumps(facts, indent=2) + "\n"
    if args.out:
        args.out.parent.mkdir(parents=True, exist_ok=True)
        args.out.write_text(payload)
        print(f"wrote {len(entries)} upstreams to {args.out}")
    else:
        sys.stdout.write(payload)
    return 0


if __name__ == "__main__":
    sys.exit(main())
