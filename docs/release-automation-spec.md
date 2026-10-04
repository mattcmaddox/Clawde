# Release Automation — Zero-Command Pipeline

Status: **Abandoned** — kept as the design record for the auto-release idea.

> **Historical — do not follow this document.** The zero-command pipeline it
> describes was tried and then turned off:
> - **Auto-release on push was disabled 2026-08-30** (`.github/workflows/release.yml`).
>   It force-bumped the patch version on every merge and clobbered deliberate
>   re-anchors. Releases are now deliberate only: `scripts/build.sh release
>   --version vX.Y.Z` (see `docs/build-release-refactor-spec.md`).
> - **The build no longer runs in Actions.** Building/publishing moved to the
>   local `scripts/build.sh`; Actions only *dispatches* npm publish. See
>   `docs/build-release-refactor-spec.md`.
> - **Windows and macOS are unsupported.** Only the two Linux legs are built, so
>   the three-platform matrix below is historical. See
>   `docs/decisions/drop-windows-support.md`.

## Problem

After pushing code to `main`, npm users running `npm update -g clawde` must
get the new binary. Currently this requires manual version bumps, manual
build commands, and manual npm publish. The user should never have to
remember or run release commands.

## Solution: Full Auto

Every merge to `main` automatically:

1. Bumps the patch version (0.2.0 → 0.2.1 → 0.2.2 ...)
2. Builds binaries for every platform
3. Creates a GitHub Release with the binaries
4. Publishes to npm
5. Users get the update on next `npm update -g clawde`

**Zero manual steps. Push to main = release.**

## How It Works

### Trigger

```yaml
on:
  push:
    branches: [main]
```

Every push to `main` triggers the release pipeline. No tags, no manual
dispatch, no version arguments.

### Step 1: Auto-Version

A GitHub Actions step computes the new version:

```
latest_tag=$(gh release list --limit 1 --json tagName --jq '.[0].tagName')
# e.g. v0.2.0
bump patch: v0.2.0 → v0.2.1
```

This uses the GitHub API (not git tags) so it's always the actual latest
published release. The version is computed once and passed to all subsequent
steps.

### Step 2: Build All Platforms (parallel jobs)

*(historical)* Three parallel build jobs, one per platform:

| Job | Runner | What it produces |
|-----|--------|------------------|
| `build-linux-x86_64` | `ubuntu-latest` | `clawde-linux-x86_64.tar.gz` |
| `build-linux-aarch64` | `ubuntu-latest` (cross + Docker) | `clawde-linux-aarch64.tar.gz` |
| `build-windows-x86_64` | `windows-latest` | `clawde-windows-x86_64.zip` |

Today the two Linux legs are built by the local `scripts/build.sh`, not by
Actions jobs; the Windows leg and its `windows-latest` runner are gone. The
`install.js` unsupported-platform error mentioned below still applies — it now
covers macOS and Windows alike.

### Step 3: Collect + Package

A downstream job:
- Downloads all platform artifacts
- Runs `scripts/build.sh package` to assemble `dist/`
- Generates `SHA256SUMS`

### Step 4: GitHub Release

```bash
gh release create "v$VERSION" \
    --title "Clawde v$VERSION" \
    --notes "..." \
    dist/*
```

### Step 5: npm Publish

Two options (TBD during implementation):

**Option A — OIDC Trusted Publisher (preferred)**
No secrets needed. npm verifies the GitHub Actions identity. Requires one-time
setup on npmjs.com linking the repo to the package.

**Option B — NPM_TOKEN secret**
Store the npm token as a GitHub Actions secret. Publish with:
```bash
echo "//registry.npmjs.org/:_authToken=$NPM_TOKEN" > ~/.npmrc
npm publish --access public --provenance
```

The existing `npm-publish.yml` already has the OIDC provenance flag. If OIDC
trusted publisher is configured on npm, Option A works out of the box. If not,
Option B is the fallback.

### Step 6: Summary

The workflow run summary shows:
- New version number
- Platforms built (and which were skipped)
- GitHub Release URL
- npm package URL

## User Experience

### As the developer (you):

```
# Normal workflow — just push code:
git push origin main
# ... that's it. The pipeline handles everything.

# To see what happened:
gh run list --limit 1     # check the release workflow
npm view clawde version   # confirm the new version
```

### As the npm user:

```bash
npm install -g clawde           # first install — gets latest
npm update -g clawde            # update — gets latest
npm install -g clawde@0.2.5     # specific version
```

## Edge Cases

### CI fails → no release
If tests/clippy/fmt fail on the push, the release workflow never starts.
Only green commits on `main` produce releases.

### Build fails for one platform → partial release
If a leg fails to build, the release still publishes the legs that succeeded
(pass `--allow-partial`); by default `release` refuses to publish with a
missing artifact. The `install.js` handles unsupported platforms gracefully
(shows "Unsupported platform" error) — that is now how macOS and Windows are
reported, since neither is built.

### Multiple pushes in quick succession
The workflow has `concurrency: cancel-in-progress: true` — if two pushes
land within seconds, only the latest triggers a release. No duplicate
versions.

### Version conflicts
The version is computed from the latest GitHub Release, not from the local
Cargo.toml. This means `bump-version.py` is only needed for the local
Cargo.toml update (so `clawde --version` reports correctly). The npm version
is set by the workflow, not by the committed package.json.

Actually — to keep things simple, the workflow should:
1. Bump Cargo.toml + Cargo.lock via `bump-version.py`
2. Commit the version bump to `main`
3. Build from that commit
4. Publish to npm

This keeps `Cargo.toml`, `Cargo.lock`, and `npm/package.json` in sync.

### The version-bump commit
The workflow pushes a version-bump commit back to `main`. This commit is
excluded from triggering another release via `[skip ci]` in the commit message
or by filtering on the committer (GitHub Actions bot).

## Files to Create/Modify

| File | Change |
|------|--------|
| `.github/workflows/release.yml` | **New** — the full auto-release pipeline |
| `.github/workflows/ci.yml` | Minor: ensure it gates the release workflow |
| `scripts/build.sh` | Minor: support `--ci` flag for non-interactive mode |
| `npm/install.js` | No change needed (already correct) |

*(historical)* The Windows/macOS build jobs named above were removed after this
spec was written; the pipeline it describes is not the one in use.

## What Does NOT Change

- `scripts/bump-version.py` — unchanged, used by the workflow
- `npm/package.json` — version field updated by the workflow (not committed
  by the workflow; the workflow sets it at publish time)
- `scripts/build.sh release` — still works for manual releases; the workflow
  uses the individual `build-one` and `package` commands directly
- User-facing install command — `npm install -g clawde` never changes

## Testing Plan

1. Dry-run the workflow on a branch (not main) to verify build steps
2. Create a test release on a fork or use `--dry-run` flags
3. Verify `npm update -g clawde` picks up the new version
4. Verify each built binary is downloadable from the release
