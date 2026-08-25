# Canonical Free-Key Storage Contract

## Overview

This document describes the canonical storage model for free API keys in Clawde.
Free provider keys are now stored in a single, well-defined location to eliminate
scattered credential storage and ensure proper key management.

## Storage Locations

### Canonical Store: `auth.json.keys`

All free API keys **must** be stored in the `keys` map of `auth.json`:

```json
{
  "keys": {
    "groq": ["gsk-abc123..."],
    "nvidia": ["nvapi-xyz789..."],
    "opencode-zen": ["zen-key-123...", "zen-key-456..."]
  },
  "credentials": {
    "github-copilot": { "OAuthToken": { ... } }
  }
}
```

### Legacy Store: `auth.json.credentials`

The `credentials` map is **deprecated** for free providers but still supports:
- **GitHub Copilot OAuth tokens** (OAuth, not API keys)
- **Migration/diagnostics display** (read-only for free providers)

## Key Rules

### 1. Free API Keys → `keys` Only

Free API keys must **only** be written to the `keys` map via:
- `AuthStore::set_free_key(provider, key)`
- `AuthStore::set_free_keys(provider, keys)`

These functions:
- Validate the provider is a free upstream
- Normalize keys (trim whitespace, enforce ≥8 char minimum)
- Remove duplicates
- Automatically migrate legacy credentials to `keys`

### 2. No Free API Keys in `credentials`

Free provider API keys must **never** be written to `credentials`. The resolver
`first_free_upstream_key()` only reads from `keys` (plus environment fallback).

### 3. OAuth Credentials Stay in `credentials`

GitHub Copilot uses OAuth tokens (not API keys). These remain in `credentials`
and are explicitly preserved during cleanup operations.

### 4. Environment Variable Fallback

If no stored key exists, the resolver falls back to environment variables:
- `GROQ_API_KEY` for Groq
- `NVIDIA_API_KEY` for NVIDIA
- etc.

Environment keys are auto-persisted to `keys` on first successful dispatch.

## Migration

### Automatic Migration

When `set_free_key()` or `set_free_keys()` is called:
1. Legacy `credentials[provider]` API keys are moved to `keys`
2. Invalid/placeholder slots (<8 chars) are removed
3. Duplicate keys are deduplicated

### Manual Migration

Use `/keys doctor` to diagnose and fix storage issues:
- Reports malformed slots
- Shows which keys are in `credentials` vs `keys`
- Recommends cleanup actions

## Provider Catalog

The canonical list of free providers is defined in two places:

1. **Core**: `AuthStore::is_free_upstream()` in `auth_store.rs`
2. **API**: `FREE_CATALOG` in `providers/free/mod.rs`

**Important**: These lists must stay in sync! The bidirectional catalog drift test
ensures no regressions.

### Known Providers

| Provider | Notes |
|----------|-------|
| github-copilot | OAuth only, no API key |
| poolside | API key |
| nvidia | API key |
| cerebras | API key |
| google | API key |
| cloudflare | Composite key (ACCOUNT_ID:TOKEN) |
| groq | API key |
| sambanova | API key |
| cline | API key |
| mistral | API key |
| opencode-zen | API key (shared with opencode-go) |
| opencode-go | Alias for opencode-zen, not a catalog entry |
| zai | API key |
| openrouter | API key |

## Testing

### Regression Tests

- `free_catalog_and_core_predicate_agree_bidirectionally` - Ensures catalog lists stay in sync
- `migration_tests` - Verifies legacy → canonical migration
- `oauth_preservation_tests` - Ensures GitHub Copilot OAuth is not deleted
- `resolver_tests` - Verifies keys-only dispatch
- `tui_dialog_tests` - Verifies TUI correctly uses canonical store

### Running Tests

```bash
cargo test -p clawde-core auth_store::tests
cargo test -p clawde-api providers::free
cargo test -p clawde-commands keys::tests
cargo test -p clawde-tui free_mode_dialog::tests
```

## Migration Guide

### For New Providers

1. Add to `FREE_CATALOG` in `providers/free/mod.rs`
2. Add to `is_free_upstream()` in `core/src/auth_store.rs`
3. Run the bidirectional drift test to verify

### For Existing Integrations

- Replace any direct `credentials` writes with `set_free_key()`/`set_free_keys()`
- Use `api_key_for()` for reads (it handles canonical resolution)
- Never read free keys from `credentials` directly

## Security Notes

- Keys are stored in plaintext in `auth.json` (same as before)
- File permissions should be 0600 (owner read/write only)
- Never log or expose key values
- Use environment variables for CI/CD instead of storing keys
