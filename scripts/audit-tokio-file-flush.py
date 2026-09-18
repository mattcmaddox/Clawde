#!/usr/bin/env python3
"""Audit: every tokio async file handle that is written to must be flushed.

Why this exists (issue #3): `tokio::fs::File` returns `Ok` from `write_all`
once the bytes are queued to a blocking-pool task, and the type has **no
`Drop` impl** — dropping the handle detaches a still-queued write. Tokio's own
docs require calling `flush` before dropping a file. So a tokio file write
with no following `flush()` is fire-and-forget: it reports success and can
silently lose data. That is exactly how ~1-2% of transcript appends were
dropped while every call returned `Ok`.

Note this is specific to handle-based writes. `tokio::fs::write` is *safe*:
it runs the blocking `std::fs::write` on the thread pool and awaits it, so a
return implies durability. Only `OpenOptions`/`File::create` handles need
flushing, and std (`std::fs`) handles never do (synchronous writes, real
`Drop`-time close).

What it checks: for each function, each tokio write-capable file handle
binding (`tokio::fs::OpenOptions::new()` with `write(true)`/`append(true)`,
`tokio::fs::File::create`, `tokio::fs::File::options()`). If that handle is
written to inside its function, a `flush()` on the same handle must appear
after the last write.

Known limitations (accepted; the guard targets the regression class, not
exhaustive dataflow):
  - A handle wrapped before writing (e.g. `BufWriter::new(file)`) is not
    tracked, so writes through the wrapper are not attributed to the handle.
  - A handle moved into another owner that flushes later is not tracked.
  - Only the receiver is matched for macro forms (`write!(handle, ..)`);
    other wrapper shapes are missed.

Escape hatch: a comment containing `ALLOW_NO_FLUSH` anywhere inside a function
exempts that whole function from the check. Use it for a genuine exception
with a stated reason — never to silence a real fire-and-forget write.

Usage:
  python3 scripts/audit-tokio-file-flush.py            # scan the repo
  python3 scripts/audit-tokio-file-flush.py --root DIR # scan another tree
  python3 scripts/audit-tokio-file-flush.py --verbose  # list each handle
Exit 1 if any unflushed write is found.
"""

import argparse
import pathlib
import re
import sys

# Directories that never contain first-party sources worth auditing.
SKIP_DIRS = {"target", "node_modules", ".git", "__pycache__"}

# Tokio write-capable file constructors. `always_write` marks the ones that are
# unambiguously for writing; the others are confirmed by inspecting the builder
# chain for `write(true)` / `append(true)`.
FQ_CONSTRUCTORS = (
    (re.compile(r"tokio::fs::OpenOptions::new\(\)"), False),
    (re.compile(r"tokio::fs::File::create\("), True),
    (re.compile(r"tokio::fs::File::options\(\)"), False),
)
# Same constructors via the short `use tokio::fs;` / `use tokio::fs::File;`
# aliases, enabled per-file only when the corresponding import is present (so a
# `std::fs` alias is never mistaken for tokio). The lookbehind is essential:
# `\bfs::` also matches inside the fully-qualified `tokio::fs::` (`:` is a
# non-word character, so the boundary holds), which double-counted every site.
ALIAS_CONSTRUCTORS = (
    (re.compile(r"(?<![:\w])fs::OpenOptions::new\(\)"), False),
    (re.compile(r"(?<![:\w])fs::File::create\("), True),
    (re.compile(r"(?<![:\w])fs::File::options\(\)"), False),
)
ALIAS_FS_IMPORT = re.compile(r"^\s*use\s+tokio::fs\s*(?:as\s+\w+)?;", re.M)
ALIAS_FILE_IMPORT = re.compile(r"^\s*use\s+tokio::fs::File\s*;", re.M)

FN_RE = re.compile(r"\bfn\s+([A-Za-z_][A-Za-z0-9_]*)")
# `let mut name =` / `let name =`, plus the match-arm form `Ok(mut name) =>`.
LET_BINDING_RE = re.compile(r"\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=")
OK_ARM_BINDING_RE = re.compile(r"Ok\(\s*(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*\)\s*=>")
WRITE_CAPABLE_CHAIN_RE = re.compile(r"\.\s*(?:write|append)\s*\(\s*true\s*\)")

ALLOW_MARKER = "ALLOW_NO_FLUSH"


def blank_comments_and_strings(text):
    """Replace comments and string/char literals with spaces, keeping offsets.

    Guards against matching the audit's own vocabulary inside doc comments,
    prompt templates, or fixtures — the patterns below are code-shaped, but
    prose in a `///` comment or an embedded markdown string would otherwise
    read as live code. Newlines are preserved so line numbers stay accurate.
    """
    out = list(text)
    n = len(text)
    i = 0
    while i < n:
        c = text[i]

        if c == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] != "\n":
                out[i] = " "
                i += 1
            continue

        if c == "/" and i + 1 < n and text[i + 1] == "*":
            depth = 1
            while i < n and depth > 0:
                if text.startswith("/*", i):
                    depth += 1
                    out[i] = out[i + 1] = " "
                    i += 2
                elif text.startswith("*/", i):
                    depth -= 1
                    out[i] = out[i + 1] = " "
                    i += 2
                else:
                    if text[i] != "\n":
                        out[i] = " "
                    i += 1
            continue

        # Raw strings: r"..", r#".."#, br#".."# — may span lines and routinely
        # hold code samples (prompt templates), so they must be blanked.
        raw = None
        if c == "r" and i + 1 < n and text[i + 1] in '#"':
            raw = re.match(r'r(#*)"', text[i:])
        elif c == "b" and text.startswith('br', i):
            raw = re.match(r'br(#*)"', text[i:])
        if raw:
            terminator = '"' + raw.group(1)
            end = text.find(terminator, i + raw.end())
            end = n if end == -1 else end + len(terminator)
            for k in range(i, end):
                if out[k] != "\n":
                    out[k] = " "
            i = end
            continue

        # Normal and byte string literals.
        if c == '"' or (c == "b" and i + 1 < n and text[i + 1] == '"'):
            start = i + (1 if c == '"' else 2)
            j = start
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            end = min(j + 1, n)
            for k in range(i, end):
                if out[k] != "\n":
                    out[k] = " "
            i = end
            continue

        # Char literal, but not a lifetime (`'a`, `'_`, `'static`).
        if c == "'":
            m = re.match(r"'(?:\\.|[^\\'])'", text[i:])
            if m:
                for k in range(i, i + m.end()):
                    if out[k] != "\n":
                        out[k] = " "
                i += m.end()
            else:
                i += 1
            continue

        i += 1
    return "".join(out)


def line_of(text, offset):
    return text.count("\n", 0, offset) + 1


def find_fn_spans(text):
    """Return [(name, start_offset, end_offset)] for every `fn` body."""
    spans = []
    for m in FN_RE.finditer(text):
        brace = text.find("{", m.end())
        if brace == -1:
            continue
        depth = 0
        j = brace
        while j < len(text):
            if text[j] == "{":
                depth += 1
            elif text[j] == "}":
                depth -= 1
                if depth == 0:
                    break
            j += 1
        spans.append((m.group(1), m.start(), j))
    return spans


def innermost_fn(spans, offset):
    """Smallest span containing `offset` (handles nested fns), else None."""
    best = None
    for span in spans:
        if span[1] <= offset <= span[2]:
            if best is None or (span[2] - span[1]) < (best[2] - best[1]):
                best = span
    return best


def chain_is_write_capable(blank, ctor_end):
    """True if the builder chain after a constructor sets write/append(true)."""
    semi = blank.find(";", ctor_end)
    chain = blank[ctor_end : semi if semi != -1 else ctor_end + 500]
    return WRITE_CAPABLE_CHAIN_RE.search(chain) is not None


def binding_names(blank, ctor):
    """Names bound to the handle the constructor at `ctor` produces.

    Two shapes cover every site in this workspace:
      let mut file = ...OpenOptions::new()...      (binding precedes)
      match ...OpenOptions::new()... { Ok(f) =>   (binding follows)
    The backward search is bounded to the current statement so an unrelated
    earlier `let` cannot be mistaken for the handle.
    """
    names = set()

    boundary = max(
        blank.rfind(";", 0, ctor.start()),
        blank.rfind("{", 0, ctor.start()),
        blank.rfind("}", 0, ctor.start()),
    )
    stmt = blank[boundary + 1 : ctor.start()]
    for m in LET_BINDING_RE.finditer(stmt):
        names.add(m.group(1))

    for m in OK_ARM_BINDING_RE.finditer(blank, ctor.start(), ctor.start() + 300):
        names.add(m.group(1))
        break

    return names


def write_offsets(body, base, name):
    """Absolute offsets of writes to `name` within `body`."""
    escaped = re.escape(name)
    patterns = (
        rf"\b{escaped}\s*\.\s*write_all\s*\(",
        rf"\b{escaped}\s*\.\s*write\s*\(",
        rf"\bwrite!\(\s*&?\s*mut\s+{escaped}\b",
        rf"\bwriteln!\(\s*&?\s*mut\s+{escaped}\b",
        rf"\bwrite!\(\s*{escaped}\b",
        rf"\bwriteln!\(\s*{escaped}\b",
    )
    found = []
    for pat in patterns:
        found.extend(base + m.start() for m in re.finditer(pat, body))
    return found


def flush_offsets(body, base, name):
    escaped = re.escape(name)
    return [
        base + m.start()
        for m in re.finditer(rf"\b{escaped}\s*\.\s*flush\s*\(", body)
    ]


def analyze(display, text):
    """Return ([checked handle descriptions], [violation strings]).

    `display` is a path string used only for messages.
    """
    blank = blank_comments_and_strings(text)

    constructors = list(FQ_CONSTRUCTORS)
    if ALIAS_FS_IMPORT.search(text):
        constructors.extend(ALIAS_CONSTRUCTORS)
    if ALIAS_FILE_IMPORT.search(text):
        constructors.append((re.compile(r"(?<![:\w])File::create\("), True))
        constructors.append((re.compile(r"(?<![:\w])File::options\(\)"), False))

    allow_lines = {
        i + 1 for i, line in enumerate(text.splitlines()) if ALLOW_MARKER in line
    }
    spans = find_fn_spans(blank)

    checked = []
    violations = []
    seen = set()
    for pat, always_write in constructors:
        for ctor in pat.finditer(blank):
            if not always_write and not chain_is_write_capable(blank, ctor.end()):
                continue
            span = innermost_fn(spans, ctor.start())
            lo, hi = (span[1], span[2]) if span else (0, len(blank))
            body = blank[lo:hi]
            for name in binding_names(blank, ctor):
                # Constructor patterns can overlap; report each handle once.
                if (ctor.start(), name) in seen:
                    continue
                seen.add((ctor.start(), name))
                writes = write_offsets(body, lo, name)
                if not writes:
                    continue  # opened but never written (e.g. ensure-exists)
                fn_name = span[0] if span else "<top level>"
                exempted = False
                if span:
                    fn_lines = (line_of(blank, span[1]), line_of(blank, span[2]))
                    exempted = any(
                        fn_lines[0] <= n <= fn_lines[1] for n in allow_lines
                    )
                # Exempted handles are still listed so an ALLOW_NO_FLUSH that
                # hides a real fire-and-forget write stays visible in --verbose.
                checked.append(
                    f"{display}:{line_of(blank, ctor.start())}: `{name}` in fn "
                    f"`{fn_name}`"
                    + (" [exempted: ALLOW_NO_FLUSH]" if exempted else "")
                )
                if exempted:
                    continue
                last_write = max(writes)
                if any(f > last_write for f in flush_offsets(body, lo, name)):
                    continue
                violations.append(
                    f"{display}:{line_of(blank, last_write)}: handle `{name}` is "
                    f"written (last write line {line_of(blank, last_write)}) with "
                    f"no `{name}.flush()` afterwards"
                )
    return checked, violations


# Detector fixtures, run on every invocation. A guard that silently stops
# detecting is worse than no guard: if these expectations ever drift, the audit
# fails loudly instead of reporting a false all-clear.
# Each entry: (name, expected_violation_count, rust_source).
SELF_TEST_FIXTURES = (
    # --- must flag: the exact shapes of the original bug ---
    (
        "openoptions-let-no-flush",
        1,
        """fn f(path: &Path) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
    (
        "openoptions-match-arm-no-flush",
        1,
        """fn f(path: &Path) {
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(path)
    {
        Ok(mut f) => {
            f.write_all(b"x");
        }
        Err(e) => {}
    }
}
""",
    ),
    (
        "file-create-no-flush",
        1,
        """fn f(path: &Path) -> std::io::Result<()> {
    let mut file = tokio::fs::File::create(path)?;
    file.write(b"x")?;
    Ok(())
}
""",
    ),
    (
        "flush-before-write-is-not-enough",
        1,
        """fn f(path: &Path) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new().append(true).open(path)?;
    file.flush()?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
    (
        "tokio-fs-alias-no-flush",
        1,
        """use tokio::fs;
fn f(path: &Path) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
    # --- must stay quiet: safe or out-of-scope shapes ---
    (
        "flush-after-write",
        0,
        """fn f(path: &Path) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(b"x")?;
    file.flush()?;
    Ok(())
}
""",
    ),
    (
        "constructor-without-any-write",
        0,
        """fn f(path: &Path) -> std::io::Result<()> {
    tokio::fs::OpenOptions::new().write(true).create(true).open(path)?;
    Ok(())
}
""",
    ),
    (
        "tokio-fs-write-is-synchronous",
        0,
        """async fn f(path: &Path) -> std::io::Result<()> {
    tokio::fs::write(path, b"x").await?;
    Ok(())
}
""",
    ),
    (
        "read-only-openoptions",
        0,
        """fn f(path: &Path) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new().read(true).open(path)?;
    file.write(b"x")?;
    Ok(())
}
""",
    ),
    (
        "std-fs-handle-needs-no-flush",
        0,
        """use std::fs;
fn f(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
    (
        "tokio-alias-import-does-not-capture-std-fs",
        0,
        """use tokio::fs;
fn f(path: &Path) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
    (
        "patterns-in-comments-and-strings",
        0,
        """// let mut file = tokio::fs::OpenOptions::new().append(true).open(path)?;
// file.write_all(b\"x\")?;
fn f() {
    let _s = "tokio::fs::OpenOptions::new() file.write_all(";
}
""",
    ),
    (
        "allow-marker-exempts-function",
        0,
        """fn f(path: &Path) -> std::io::Result<()> {
    // ALLOW_NO_FLUSH: deliberate, handle closed by the caller.
    let mut file = tokio::fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(b"x")?;
    Ok(())
}
""",
    ),
)


def self_test():
    """Return [(name, expected, actual_violations)] for drifted fixtures."""
    failures = []
    for name, expected, src in SELF_TEST_FIXTURES:
        _, violations = analyze(f"<self-test:{name}>", src)
        if len(violations) != expected:
            failures.append((name, expected, violations))
    return failures


def iter_rust_files(root):
    for p in sorted(root.rglob("*.rs")):
        if any(part in SKIP_DIRS or part.startswith(".") for part in p.parts):
            continue
        yield p


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    default_root = pathlib.Path(__file__).resolve().parent.parent
    ap.add_argument(
        "--root",
        default=str(default_root),
        help="directory to scan recursively for .rs files",
    )
    ap.add_argument(
        "--verbose",
        action="store_true",
        help="list every written handle that was checked",
    )
    args = ap.parse_args()

    # Validate the detector before trusting it to clear the tree.
    failures = self_test()
    if failures:
        print(
            f"audit-tokio-file-flush: SELF-TEST FAILED "
            f"({len(failures)} of {len(SELF_TEST_FIXTURES)} fixtures)"
        )
        for name, expected, violations in failures:
            print(f"  {name}: expected {expected}, got {len(violations)}")
            for v in violations:
                print("      " + v)
        print("  The detector is broken — a pass below would be meaningless.")
        return 1
    print(
        f"audit-tokio-file-flush: self-test passed "
        f"({len(SELF_TEST_FIXTURES)} fixtures)"
    )

    root = pathlib.Path(args.root)
    if not root.is_dir():
        print(f"audit-tokio-file-flush: not a directory: {root}")
        return 1

    total_checked = 0
    all_violations = []
    all_checked = []
    files = 0
    for path in iter_rust_files(root):
        files += 1
        text = path.read_text(errors="replace")
        try:
            display = str(path.relative_to(root))
        except ValueError:
            display = str(path)
        checked, violations = analyze(display, text)
        total_checked += len(checked)
        all_checked.extend(checked)
        all_violations.extend(violations)

    print(
        f"audit-tokio-file-flush: checked {total_checked} written tokio file "
        f"handle(s) across {files} Rust file(s)"
    )
    if args.verbose:
        for c in all_checked:
            print("  ok: " + c)
    if all_violations:
        print(f"audit-tokio-file-flush: {len(all_violations)} unflushed write(s)")
        for v in all_violations:
            print("  " + v)
        print(
            "  tokio::fs::File returns Ok from write_all once the bytes are "
            "queued to a"
        )
        print(
            "  blocking task and has no Drop impl, so dropping the handle "
            "detaches the write."
        )
        print(
            "  Add `handle.flush().await?` after the write (see issue #3), or "
            "document a"
        )
        print("  real exception with an `ALLOW_NO_FLUSH` comment in the function.")
        return 1
    print("audit-tokio-file-flush: all tokio async file writes are flushed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
