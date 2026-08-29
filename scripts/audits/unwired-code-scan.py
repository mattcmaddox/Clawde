#!/usr/bin/env python3
"""Workspace unwired-code scanner.

Enumerates every item that the dead-code guard skips or that is clearly
unwired, so the audit has a complete inventory rather than a manual one.

For each `#[allow(dead_code)]`-annotated `pub fn`/`pub async fn` and each
`pub mod`, report the file + name, and count how many times the name appears
workspace-wide (1 = its own declaration = fully unwired).
"""
import os
import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parents[2] / "src-rust"

fn_re = re.compile(r"\bpub\s+(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)")
mod_re = re.compile(r"\bpub\s+mod\s+([A-Za-z_][A-Za-z0-9_]*)")

def collect_rs_files(root):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in ("target", ".git")]
        for f in filenames:
            if f.endswith(".rs"):
                yield pathlib.Path(dirpath) / f

files = list(collect_rs_files(ROOT))

# Build workspace-wide name->count with file offsets per item
names = {}
name_files = {}  # name -> set of files
for path in files:
    content = path.read_text(errors="replace")
    used = set()
    for m in fn_re.finditer(content):
        used.add(m.group(1))
    for m in re.finditer(r"\b([A-Za-z_][A-Za-z0-9_]*)\b", content):
        used.add(m.group(1))
    for name in used:
        names[name] = names.get(name, 0) + 1
        name_files.setdefault(name, set()).add(str(path))

def preceded_by_allow_dead_code(content, start, lookback=5):
    end = start
    for _ in range(lookback):
        nl = content[:end].rfind("\n")
        if nl == -1:
            break
        line = content[nl + 1 : end].strip()
        if line.startswith("#[allow(dead_code") or line.startswith("#[cfg_attr(not(feature"):
            return True
        end = nl
    return False

print("=" * 100)
print("A. ITEMS MARKED #[allow(dead_code)] that the guard skips")
print("   (declared in src but maybe never referenced elsewhere)")
print("=" * 100)
for path in files:
    rel = path.relative_to(ROOT)
    content = path.read_text(errors="replace")
    for m in fn_re.finditer(content):
        whole = m.group(0)
        if preceded_by_allow_dead_code(content, m.start()):
            n = m.group(1)
            count = names.get(n, 0)
            flag = "UNWIRED" if count < 2 else f"({count} refs)"
            print(f"{flag:10s} {rel}: {n}")

print()
print("=" * 100)
print("B. UNWIRED #[allow(dead_code)] pub fns (count<2) — grouped by module")
print("=" * 100)
module_order = {}
for path in files:
    rel = path.relative_to(ROOT)
    content = path.read_text(errors="replace")
    for m in fn_re.finditer(content):
        if preceded_by_allow_dead_code(content, m.start()):
            n = m.group(1)
            if names.get(n, 0) < 2:
                print(f"{str(rel):55s} {n}")

print()
print("=" * 100)
print("C. pub mod declarations (candidate whole-module scaffold; check export/wiring)")
print("=" * 100)
for path in files:
    rel = path.relative_to(ROOT)
    content = path.read_text(errors="replace")
    for m in mod_re.finditer(content):
        n = m.group(1)
        count = names.get(n, 0)
        # A module referenced via pub use / crate::module:: gives count>=2 normally;
        # flag modules whose name appears <2 times workspace-wide
        flag = "UNWIRED" if count < 2 else f"({count} refs)"
        print(f"{flag:10s} {str(rel):55s} {n}")