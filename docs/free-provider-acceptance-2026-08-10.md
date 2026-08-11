# Free-provider acceptance evidence

Date: 2026-08-10.

## Scope

This run validates the production semantic verifier and fresh-executor fixer
through `free/auto`. It uses a disposable `CLAWDE_HOME` populated from the
existing authoritative multi-key backup. The real user home was not modified,
and no localhost Ollama endpoint was used.

The source tree also contains an AuthStore optimistic-concurrency safeguard:
long-lived or concurrent writers now compare the loaded auth-file fingerprint
under a fail-closed inter-process lock before replacing `auth.json`. Runtime
`key-ring-state` remains cooldown state only; it is not treated as a credential
source.

## Complete end-to-end acceptance

One isolated run reached the full production acceptance ladder:

| Check | Result |
|---|---|
| Current debug CLI build | passed |
| Authoritative isolated key providers | 12 provider IDs in backup; multiple key pools present |
| Route | `free/auto` |
| Production semantic verifier | reached |
| Production verdict | `fixable` |
| Production attempts | `1` |
| Fresh fixer attempts | `2` bounded attempts |
| Fixer result | accepted scoped patch |
| `file_changed` | `true` |
| `fix_verified` | `true` |
| `cargo_verified` | `true` |
| Semantic re-verification | passed |
| Overall live report | `ok=true` |

The direct reference probe reported `strict_parse_failure` in that run. This is
reference-only evidence; the production path is the object under test and
completed successfully.

## Bounded repetition and variance

Three additional isolated trials were run with fresh disposable homes and the
same production budgets. All reached the production semantic path:

| Trial | Production | Verdict | Fixer | Mutation | Result |
|---|---|---|---|---|---|
| 1 | `ok=true` | `fixable` | `strict_parse_failure` after 2 attempts | `false` | failed closed |
| 2 | `ok=false` | unavailable | `strict_parse_failure` | not reached | failed closed |
| 3 | `ok=true` | `fixable` | `strict_parse_failure` after 2 attempts | `false` | failed closed |

A final bounded trial had the same shape: verifier `fixable`, fixer
`strict_parse_failure`, no mutation, and exit status `1`.

This demonstrates model variance in tool/patch emission. It does **not** show a
Clawde acceptance bug: the fixer correctly refuses text-only or malformed
responses and never claims a repair without file mutation, deterministic
verification, Cargo verification, and semantic re-verification.

## Auth-store persistence diagnosis

The live user's current `auth.json` still contains only the legacy Groq
credential. The authoritative backup used for the isolated runs contains the
multi-key provider pools. The earlier one-time migration therefore did not
persist in the live runtime state; a stale or concurrent writer can explain the
reversion.

The source fix adds:

- a SHA-256 fingerprint captured on successful load;
- an inter-process `auth.json.lock` covering compare-through-rename;
- `0600` lock permissions and flushed owner contents;
- fail-closed behavior on changed files or lock contention;
- support for initial creation when no auth file exists; and
- regression coverage for missing-file creation, lock contention, stale
  writers, corrupt recovery, and multi-key round trips.

No implicit recovery from `key-ring-state` was added.

## Local validation

After the AuthStore change:

- `cargo fmt --all -- --check`: passed
- `cargo test --workspace`: 971 passed, 0 failed
- `cargo check --workspace`: passed
- `git diff --check`: passed

## Acceptance interpretation

- **Free-provider routing:** proven on the isolated authoritative backup.
- **Production semantic verifier:** proven and repeatedly reached.
- **Production fresh fixer:** proven end-to-end once; repeated runs correctly
  fail closed when the model does not produce an accepted patch.
- **Stable live fixer success:** not claimed. A model/provider with reliable
  structured tool-call or patch emission is still needed for repeatable G5
  acceptance.
- **Live user-home repair:** not yet performed because the current Clawde
  processes must not be allowed to race a credential-store repair. Restore the
  authoritative backup only after those old processes are stopped or restarted
  with the rebuilt binary.
