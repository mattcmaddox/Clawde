#!/usr/bin/env python3
"""Validate the ACP registry manifest against the registry schema and the built legs.

Why this exists: `src-rust/crates/acp/registry-template/agent.json` is the file a
contributor copies into a PR to `agentclientprotocol/registry`, and the registry
CI validates it against `agent.schema.json`. Nothing in this repo checked it, so
the template silently drifted from reality: it advertised `darwin-*` and
`windows-x86_64` binary targets whose archives 404 on every release (only the two
Linux legs are built — see `target_ids()` in `scripts/build.sh`), and it was
missing the `license_url` field the schema requires for every id except `dimcode`.
Either defect fails registry CI.

What it checks:
  1. Structural (stdlib only, always runs): required top-level fields, the id and
     version patterns, `license_url` presence, a non-empty `distribution`, and
     every `binary` target using an allowed platform key with `archive` + `cmd`.
  2. Schema (only when `jsonschema` is importable): full validation against the
     vendored `scripts/registry-agent.schema.json` (a snapshot of the live
     registry schema, `$id` .../registry/v1/latest/agent.schema.json). CI installs
     jsonschema so this tier always runs there; local runs without it still get
     tier 1.
  3. Legs (best effort): the manifest's `binary` platform keys must equal the set
     of legs `scripts/build.sh` builds, parsed from `target_ids()`. Skipped with a
     note if that function can't be parsed — never a false failure.

Self-tests run on every invocation against built-in fixtures (valid + a few
broken shapes), so a rotted validator fails loudly instead of clearing the tree.

Usage:
  python3 scripts/validate-acp-template.py            # validate the template
  python3 scripts/validate-acp-template.py --quiet    # only report failures
Exit 1 on any failure.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TEMPLATE = ROOT / "src-rust" / "crates" / "acp" / "registry-template" / "agent.json"
SCHEMA = ROOT / "scripts" / "registry-agent.schema.json"
BUILD_SH = ROOT / "scripts" / "build.sh"

# Mirror of `#/definitions/binaryDistribution.propertyNames.enum` in the schema.
ALLOWED_PLATFORMS = {
    "darwin-aarch64",
    "darwin-x86_64",
    "linux-aarch64",
    "linux-x86_64",
    "windows-aarch64",
    "windows-x86_64",
}
REQUIRED_TOP = ("id", "name", "version", "description", "distribution")
ID_RE = re.compile(r"^[a-z][a-z0-9-]*$")
VERSION_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")


def structural_errors(doc: dict) -> list[str]:
    """Schema-independent checks that always run (no third-party dependency)."""
    errs: list[str] = []
    for key in REQUIRED_TOP:
        if key not in doc:
            errs.append(f"missing required field: {key}")
    if not isinstance(doc.get("id"), str) or not ID_RE.match(doc.get("id", "")):
        errs.append(f"id must match {ID_RE.pattern}: {doc.get('id')!r}")
    if not isinstance(doc.get("version"), str) or not VERSION_RE.match(doc.get("version", "")):
        errs.append(f"version must be plain X.Y.Z: {doc.get('version')!r}")
    # Required for every id except dimcode (schema `if`/`else`).
    if doc.get("id") != "dimcode" and "license_url" not in doc:
        errs.append("missing required field: license_url (required for every id except dimcode)")

    dist = doc.get("distribution")
    if not isinstance(dist, dict) or not dist:
        errs.append("distribution must be a non-empty object")
        return errs
    unknown = set(dist) - {"binary", "npx", "uvx"}
    if unknown:
        errs.append(f"distribution has unknown key(s): {sorted(unknown)}")

    binary = dist.get("binary")
    if binary is not None:
        if not isinstance(binary, dict) or not binary:
            errs.append("distribution.binary must be a non-empty object")
        else:
            for platform, target in binary.items():
                if platform not in ALLOWED_PLATFORMS:
                    errs.append(f"unknown platform key: {platform!r}")
                if not isinstance(target, dict):
                    errs.append(f"{platform}: target must be an object")
                    continue
                for field in ("archive", "cmd"):
                    if not isinstance(target.get(field), str) or not target.get(field):
                        errs.append(f"{platform}: missing required {field!r}")
    return errs


def schema_errors(doc: dict) -> list[str] | None:
    """Full schema validation, or None when jsonschema is unavailable."""
    try:
        import jsonschema  # noqa: PLC0415 — optional dependency
    except ImportError:
        return None
    schema = json.loads(SCHEMA.read_text(encoding="utf-8"))
    validator = jsonschema.Draft7Validator(schema)
    return [
        f"{list(e.path)}: {e.message}"
        for e in sorted(validator.iter_errors(doc), key=lambda e: list(e.path))
    ]


def built_legs() -> set[str] | None:
    """Leg ids from `target_ids()` in build.sh, or None if it can't be parsed."""
    try:
        text = BUILD_SH.read_text(encoding="utf-8")
    except OSError:
        return None
    body = re.search(r"target_ids\(\)\s*\{(.*?)\}", text, flags=re.DOTALL)
    if not body:
        return None
    echoed = re.search(r'echo\s+"([^"]+)"', body.group(1))
    if not echoed:
        return None
    legs = set(echoed.group(1).split())
    return legs or None


def leg_errors(doc: dict, legs: set[str]) -> list[str]:
    binary = (doc.get("distribution") or {}).get("binary") or {}
    advertised = set(binary)
    errs: list[str] = []
    extra = advertised - legs
    if extra:
        errs.append(
            f"template advertises legs that build.sh does not build: {sorted(extra)} "
            f"(build.sh builds {sorted(legs)})"
        )
    return errs


def validate(doc: dict) -> tuple[list[str], str]:
    """Return (errors, note) for one document."""
    errs = structural_errors(doc)
    note = ""
    schema = schema_errors(doc)
    if schema is None:
        note = "jsonschema not installed — deep schema check skipped"
    else:
        errs += [f"schema: {e}" for e in schema]
    if not errs:
        legs = built_legs()
        if legs is None:
            note = (note + "; " if note else "") + "could not parse build.sh target_ids() — leg check skipped"
        else:
            errs += leg_errors(doc, legs)
    return errs, note


def self_test() -> list[str]:
    """Fixtures that prove the validator still catches the regressions it exists for."""
    good = {
        "id": "clawde",
        "name": "Clawde",
        "version": "0.3.6",
        "description": "x",
        "license_url": "https://example.com/LICENSE",
        "distribution": {
            "binary": {
                "linux-x86_64": {"archive": "https://example.com/a.tar.gz", "cmd": "./clawde"}
            }
        },
    }
    failures: list[str] = []
    errs, _ = validate(good)
    if errs:
        failures.append(f"valid fixture reported errors: {errs}")
    broken = {
        "missing license_url": {k: v for k, v in good.items() if k != "license_url"},
        "bad platform": {
            **good,
            "distribution": {"binary": {"solaris-x86_64": {"archive": "https://x", "cmd": "./c"}}},
        },
        "bad version": {**good, "version": "0.3"},
        "no distribution": {k: v for k, v in good.items() if k != "distribution"},
        "target missing cmd": {
            **good,
            "distribution": {"binary": {"linux-x86_64": {"archive": "https://x"}}},
        },
    }
    for name, doc in broken.items():
        errs, _ = validate(doc)
        if not errs:
            failures.append(f"broken fixture {name!r} was accepted")
    # The leg check must reject a manifest advertising a leg build.sh does not build.
    legs = built_legs()
    if legs:
        over = {**good, "distribution": {"binary": {**{k: good["distribution"]["binary"]["linux-x86_64"] for k in sorted(legs)}, "windows-x86_64": good["distribution"]["binary"]["linux-x86_64"]}}}
        if not leg_errors(over, legs):
            failures.append("leg check accepted an unpublished leg")
    return failures


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--quiet", action="store_true", help="only print failures")
    ap.add_argument("--self-test-only", action="store_true", help="run fixtures and exit")
    args = ap.parse_args()

    st = self_test()
    if st:
        print("validate-acp-template: SELF-TEST FAILED — validator is not trustworthy:", file=sys.stderr)
        for line in st:
            print(f"  {line}", file=sys.stderr)
        sys.exit(1)
    if args.self_test_only:
        print("validate-acp-template: self-test OK")
        return

    try:
        doc = json.loads(TEMPLATE.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"validate-acp-template: cannot read {TEMPLATE.relative_to(ROOT)}: {exc}", file=sys.stderr)
        sys.exit(1)

    errs, note = validate(doc)
    if errs:
        print(f"validate-acp-template: {TEMPLATE.relative_to(ROOT)} FAILED", file=sys.stderr)
        for line in errs:
            print(f"  - {line}", file=sys.stderr)
        sys.exit(1)
    if not args.quiet:
        print(f"validate-acp-template: {TEMPLATE.relative_to(ROOT)} OK"
              + (f" ({note})" if note else ""))


if __name__ == "__main__":
    main()
