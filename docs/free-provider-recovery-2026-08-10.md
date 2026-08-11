# Free-provider recovery evidence

Date: 2026-08-10.

## Problem

Clawde's free catalog contained multiple configured upstream definitions, but the
active authoritative auth store exposed only Groq. Older free-provider key-ring
snapshots contained credential-bearing key identities for Cline, Cloudflare,
NVIDIA, and OpenCode Zen, but those snapshots are runtime rotation state and
must not be treated as an implicit credential store by production code.

Consequently `free/auto` built a chain from too few authoritative credentials
and exhausted before semantic verification could produce a verdict.

## Safe recovery

A one-time, user-authorized migration copied the credential-bearing `key`
fields from the known free-provider key-ring snapshots into the supported
`auth.json.keys` rotation store. It:

- created a timestamped `auth.json.migration-backup-*` backup first;
- preserved the existing `credentials` map;
- deduplicated provider key slots;
- copied no cooldown or error metadata;
- wrote `auth.json` atomically;
- restored restrictive `0600` permissions; and
- left key-ring cooldown snapshots untouched for normal runtime restoration.

Migrated providers: Cline, Cloudflare, Groq, NVIDIA, and OpenCode Zen.

The migration is intentionally operational rather than an implicit fallback in
Clawde source. Runtime state must not resurrect deleted or revoked credentials.

## Validation

Local validation after the safe source revert:

- `cargo fmt --all`: passed
- `cargo test -p clawde-api`: 322 passed
- `cargo test -p clawde-query`: 255 passed
- `cargo check --workspace`: passed
- `git diff --check`: passed
- CLI build: passed

`clawde --check-keys` subsequently reported five stored providers:
Cline, Cloudflare, Groq, NVIDIA, and OpenCode Zen.

## Live free/auto smoke

The rebuilt production smoke used `free/auto` and the existing user-authorized
Clawde home. No localhost Ollama endpoint was used.

| Check | Result |
|---|---|
| Build | passed |
| Route | `free/auto` |
| Smoke exit | `0` |
| Overall live report | `ok=true` |
| Production AgentTool verifier | `ok=true` |
| Production attempts | `1` |
| Production verdict | `replan` |
| Direct error | none |
| Production error | none |
| Fixer path | not invoked (`replan` is not `fixable`) |

This proves that the production semantic verifier now reaches a working free
upstream. It does **not** claim G5 fixer acceptance, because the verifier did
not return a `fixable` verdict in this run. A separate smoke with a real
`fixable` result is required before evaluating the fresh executor.
