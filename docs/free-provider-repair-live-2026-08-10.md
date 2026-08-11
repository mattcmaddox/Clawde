# Live free-provider key repair evidence

Date: 2026-08-10.

## Repair performed

The live Clawde home had reverted to a legacy credentials-only store containing
only Groq, while an existing authoritative backup contained the supported
multi-key free-provider configuration. Three stale Clawde processes were
stopped before repair so old in-memory auth snapshots could not overwrite the
restored file.

The current live auth file was preserved at:

`/home/user/.clawde/auth.json.pre-free-repair-1786419986`

The authoritative multi-key backup was restored atomically to
`/home/user/.clawde/auth.json` with mode `0600`. Runtime key-ring cooldown
files were not copied, merged, or treated as credentials.

## Post-repair health

The rebuilt current CLI reported 16 stored providers:

- 12 rotation-key providers: Cerebras, Cline, Cloudflare, Cohere, Google,
  Groq, Hugging Face, Mistral, NVIDIA, OpenCode Zen, SambaNova, and Z.AI.
- 4 credential providers: GitHub, OpenAI, Vercel, and VoyageAI.

The auth JSON schema was `credentials,keys`, mode was `0600`, no auth lock
remained, and no old Clawde process was running during the check.

## Live production acceptance

A real bounded `clawde diagnostics --live --json` run used the repaired live
home and the rebuilt current binary:

| Check | Result |
|---|---|
| Exit status | `0` |
| Provider route | `free` composite |
| Effective model | `gemini-2.5-flash` |
| Routing strategy | `Auto` |
| Production verifier | `ok=true` |
| Production verdict | `fixable` |
| Production attempts | `1` |
| Fresh fixer | `ok=true` |
| Fresh fixer attempts | `1` |
| `file_changed` | `true` |
| `fix_verified` | `true` |
| `cargo_verified` | `true` |
| Auth provider count after run | `16` |

The separate single-shot direct reference probe reported
`strict_parse_failure`; that field is diagnostic-only. The production path is
the acceptance object under test and completed successfully without a provider
error or chain exhaustion.

## Source protection and validation

The source now protects `auth.json` with load fingerprints and a fail-closed
inter-process lock around compare-through-rename. Stale writers cannot silently
erase newer key pools, and `key-ring-state` remains cooldown state only.

Local validation:

- `cargo fmt --all -- --check`: passed
- `cargo test --workspace`: 971 passed, 0 failed
- `cargo check --workspace`: passed
- `git diff --check`: passed
