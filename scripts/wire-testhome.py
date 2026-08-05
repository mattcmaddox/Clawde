#!/usr/bin/env python3
"""Add a `test_auth_store()` helper to the free/impls.rs tests module and
route every `clawde_core::AuthStore::default()` construction through it, so
each test holds a `TestHome` CLAWDE_HOME redirect for its whole body.

Any future accidental persistence (e.g. a `set_keys` call) then lands in a
temp dir instead of the user's real `~/.clawde/auth.json`.
"""

PATH = "src-rust/crates/api/src/providers/free/impls.rs"

with open(PATH) as f:
    lines = f.readlines()

# 1. Find the tests module header (first `#[cfg(test)]` + `mod tests {`).
start = None
for i in range(len(lines) - 1):
    if lines[i].strip() == "#[cfg(test)]" and lines[i + 1].strip() == "mod tests {":
        start = i + 1
        break
assert start is not None, "tests module not found"

# 2. Insert the helper right after the `use crate::provider_types::StopReason;`
#    import (the module's import block).
anchor_idx = None
for i in range(start, start + 20):
    if "use crate::provider_types::StopReason;" in lines[i]:
        anchor_idx = i
        break
assert anchor_idx is not None, "StopReason import anchor not found"

helper = [
    "\n",
    "    /// Build an in-memory auth store behind a [`TestHome`] CLAWDE_HOME\n",
    "    /// redirect so any accidental persistence (e.g. a future `set_keys`\n",
    "    /// call) lands in a temp dir instead of the user's real\n",
    "    /// `~/.clawde/auth.json`. The guard is returned so it stays alive for\n",
    "    /// the whole test body.\n",
    "    fn test_auth_store(\n",
    "    ) -> (clawde_core::AuthStore, crate::test_support::TestHome) {\n",
    "        let home = crate::test_support::TestHome::new();\n",
    "        (clawde_core::AuthStore::default(), home)\n",
    "    }\n",
]
lines[anchor_idx + 1 : anchor_idx + 1] = helper

# 3. Rewrite every `let [mut] store = clawde_core::AuthStore::default();` in
#    the tests module into `let [mut] store, _home = test_auth_store();`.
count = 0
in_tests = False
for i in range(len(lines)):
    if i == start - 1:
        in_tests = True
    if not in_tests:
        continue
    line = lines[i]
    stripped = line.strip()
    if stripped == "let mut store = clawde_core::AuthStore::default();":
        lines[i] = line.replace(
            "let mut store = clawde_core::AuthStore::default();",
            "let (mut store, _home) = test_auth_store();",
        )
        count += 1
    elif stripped == "let store = clawde_core::AuthStore::default();":
        lines[i] = line.replace(
            "let store = clawde_core::AuthStore::default();",
            "let (store, _home) = test_auth_store();",
        )
        count += 1

with open(PATH, "w") as f:
    f.writelines(lines)

print(f"rewrote {count} AuthStore constructions (expected 11)")
assert count == 11, f"expected 11, got {count}"
