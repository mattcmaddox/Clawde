# Build & Release Refactor Spec — one script, one source of truth

Status: **Implemented** (verified Aug 2026)

## 1. Motivation

The release pipeline is built on fragile, duplicated GitHub-specific machinery,
and it is currently **broken**:

1. **`--package clawde` does not exist.** The package is `clawde-cli` (its only
   binary is named `clawde`). Five build steps reference the dead package:
   `ci.yml:107`, `release.yml:127,145`, `patch-release.yml:150,168`. CI's Linux
   build step has been failing on every push since the rename, and **no release
   workflow can build anything** until fixed.
2. **The build matrix is copy-pasted** between `release.yml` and
   `patch-release.yml` (~150 lines each, with a comment admitting "keep these
   two job specs in sync"). They have already drifted once (SHA256SUMS was
   missing from patch-release.yml, breaking installer checksum verification).
3. **GitHub-specific mechanics** add moving parts with real failure modes:
   tag force-moving for patches, GITHUB_TOKEN workflow-dispatch chains (the
   workflow itself documents a workaround because `workflow_run` silently
   no-ops under GITHUB_TOKEN), commit-marker auto-release, and a GitHub-API
   release-notes composer that awk-splits a JSON body into three files.
4. **Version/package/repo constants are scattered** across workflows, install
   scripts, npm, and Rust — with no single authority.

Decisions (confirmed with user):

- Move away from GitHub Actions for building/publishing. A single local
  script is the **one source of truth**; GitHub Releases remains the download
  host (free, CDN-backed, installers already point at it).
- **Every fix cuts a new version.** No tag force-moves, no asset re-uploads.
- Installers (`install.sh`, `install.ps1`, `npm/install.js`, `clawde upgrade`)
  keep their own copies of constants and keep working unchanged against
  GitHub Releases.

## 2. Target architecture

`scripts/build.sh` becomes the authoritative build, package, and release tool.
All release constants live at the top of the script:

| Constant | Value |
|---|---|
| `REPO` | `mattcmaddox/Clawde` |
| `PKG` | `clawde-cli` |
| `BIN_NAME` | `clawde` |
| `DIST_DIR` | `dist/` (gitignored) |
| `TARGETS` | 5-leg table: id / rust target triple / builder (native \| cross \| manual) |

Target matrix (byte-compatible with today's archives — **archive names and
SHA256SUMS format must not change**, installers depend on them):

| id | triple | builder | artifact |
|---|---|---|---|
| `linux-x86_64` | `x86_64-unknown-linux-gnu` | native | `clawde-linux-x86_64.tar.gz` |
| `linux-aarch64` | `aarch64-unknown-linux-gnu` | cross (Docker) | `clawde-linux-aarch64.tar.gz` |
| `windows-x86_64` | `x86_64-pc-windows-msvc` | manual (Windows box) | `clawde-windows-x86_64.zip` |

Cross-compiling the Windows target from Linux is **not viable**: the
`btls-sys` dependency builds BoringSSL via CMake and needs NASM, which the
cross windows-gnu image lacks (verified empirically, Aug 2026). The Windows
leg must be built on a Windows machine with the MSVC target. **macOS legs are
intentionally excluded from the release flow** (no Apple hardware; decided
2026-08-22).

### Subcommands

```
./build.sh                    release build + install to ~/.local/bin (unchanged)
./build.sh install            same as above
./build.sh debug / run / clean / targets     (unchanged)
./build.sh build-one --target <id>   build one leg (native or cross); on a Mac
                                     or Windows box, builds that machine's leg
./build.sh build-all          build every leg this machine can; report the rest
./build.sh package            assemble dist/: archives + install.sh + install.ps1
                              + SHA256SUMS (byte-compatible with current layout)
./build.sh release --version vX.Y.Z [--publish-only] [--no-commit]
                              stamp → build → package → notes → gh release create
                              → dispatch npm-publish.yml
```

### `release` flow (one command, no Actions)

1. **Preflight** — `gh` authed, on `main`, version is `vMAJOR.MINOR.PATCH`.
2. **Stamp** — if `Cargo.toml` version != requested, run `scripts/bump-version.py`
   (remains the single version stamper), commit `chore(release): stamp vX.Y.Z`,
   push. `--no-commit` skips the git steps (tag then points at current HEAD).
3. **Build** — `build-all`; legs that need a Mac/Windows box must be present
   in `dist/` first (built via the same script on that machine and copied in,
   or produced by an earlier `build-one` run).
4. **Package** — assemble `dist/` exactly as the old workflow did.
5. **Notes** — generate from `git log` since the previous tag (simple,
   deterministic; replaces the GitHub-API/awk composer).
6. **Publish** — `gh release create vX.Y.Z dist/* --title "Clawde vX.Y.Z"
   --notes-file`. Then `gh workflow run npm-publish.yml -f version=vX.Y.Z`
   (npm keeps its OIDC/provenance publishing via Actions; dispatch is one line).
7. **`--publish-only`** — skip build/stamp; publish whatever is in `dist/`
   (used when Mac/Windows legs were built elsewhere).

### Workflow disposition

| Workflow | Fate | Reason |
|---|---|---|
| `release.yml` | **delete** | Build+publish machinery moves into `build.sh release` |
| `patch-release.yml` | **delete** | Tag force-moves/asset re-uploads contradict new-version-per-fix |
| `auto-release.yml` | **delete** | Commit-marker + GITHUB_TOKEN dispatch chain is the fragile part |
| `ci.yml` | **keep**, fix line 107 | `--package clawde` → `--package clawde-cli`; CI stays the safety net |
| `npm-publish.yml` | **keep**, drop `workflow_run` trigger | release.yml is gone; script dispatches it after publishing |
| `pages.yml` | keep | docs site, unrelated to releases |

## 3. Why this is the right shape

- **One authority.** Package name, binary name, targets, archive naming, and
  the release flow live in one executable file. The duplicated matrix and its
  drift risk disappear; the two workflows that referenced a dead package are
  deleted rather than patched.
- **No GitHub-specific fragility.** No tag force-moves, no token-dispatch
  chains, no API note composition, no commit-marker parsing. `gh release
  create` is a stable, well-trodden CLI. Every release is a deliberate local
  action, which is appropriate for a single-maintainer project.
- **Installers untouched.** Archive names, checksum format, and download URLs
  are byte-identical, so `install.sh`, `install.ps1`, `npm/install.js`, and
  `clawde upgrade` keep working with zero changes.
- **New-version-per-fix** removes the most dangerous operation in the old
  system (force-moving a published tag) entirely.
- **CI remains** for the thing Actions is genuinely good at: continuous
  check/clippy/test on every commit.

## 4. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Breaking archive naming / SHA256SUMS format (installers fail) | `package` reuses the exact names and `sha256sum *` format; verified by test in Phase 4 |
| Missing Windows leg at publish time | `release` refuses to publish unless all 3 artifacts are present in `dist/` (or `--allow-partial` is passed explicitly) |
| Windows-GNU cross-build from Linux fails (btls-sys needs NASM) | windows-x86_64 is a `manual` leg — build on a Windows box; documented in the script header |
| `cross` not installed (aarch64 leg) | `build-one`/`build-all` error with the install command; `targets` shows readiness |
| Version stamping without a commit leaves tag off the stamp commit | `release` commits + pushes the stamp by default; `--no-commit` is explicit opt-out |
| Accidentally publishing a broken release | `--dry-run` flag validates preflight, notes, and artifact list without creating anything |
| Losing the old release-notes richness (PR list, New Contributors) | Acceptable simplification: notes come from `git log` between tags; the in-app `/release-notes` command still reads the GitHub release page |

## 5. Open questions (resolved)

| Question | Resolution |
|---|---|
| Windows leg can't build on this Linux box | `build-one windows-x86_64` on a Windows box; `release --publish-only` assembles from `dist/`. Zero Actions dependency |
| macOS legs? | Dropped from the release flow entirely (2026-08-22) — no Apple hardware; installers would 404 until a Mac build is added back |
| ACP registry `agent.json` `website` field | Uses `https://github.com/mattcmaddox/Clawde` (changed from `clawde.example.com` to avoid exposing the home server; 2026-08-22 — see `.local/setup-notes.md`) |
| Should `release` auto-commit the version bump? | Yes by default (mirrors old auto-release); `--no-commit` opt-out |
| npm publish with provenance needs OIDC (Actions-only) | npm-publish.yml kept with `workflow_dispatch`; the script dispatches it via `gh workflow run` |
| Fix or delete the dead-package refs in the two release workflows? | Delete the workflows (Phase 3); fix only `ci.yml` which survives |

## 6. Implementation phases

### Phase 1 — Fix the dead package refs (unblocks CI today)
- `ci.yml:107` `cargo build --locked --package clawde` → `--package clawde-cli`
- `release.yml:127,145` + `patch-release.yml:150,168` → same one-word fix
  (keeps the repo green until Phase 3 deletes them)
- Verify: `cargo build --locked --package clawde-cli` succeeds locally

### Phase 2 — Extend `scripts/build.sh`
- Add constants table + `TARGETS` matrix
- `build-one`, `build-all`, `package`, `release` (with `--dry-run`,
  `--publish-only`, `--no-commit`), keep existing subcommands
- Cross.toml generation (currently inline in the workflows) moves into the script

### Phase 3 — Remove obsolete workflows
- Delete `auto-release.yml`, `patch-release.yml`, `release.yml`
- Trim `workflow_run` trigger from `npm-publish.yml`
- `ci.yml` already fixed in Phase 1

### Phase 4 — Docs & verification
- Update `docs/cheatsheet.md`, `docs/installation.md`, README if needed
- Verify: `build.sh package` layout matches old workflow output; SHA256SUMS
  parseable by install.sh's awk; `release --dry-run` validates cleanly;
  native + cross legs build; install.sh installs a locally packaged archive

### Phase 5+ — Wider Rust architecture (deferred, per "both, tooling first")
- Crate consolidation, query pipeline, path handling — candidates to be
  assessed in a separate spec after the tooling refactor lands

## 7. Change summary (to update as implemented)

| File | Change |
|---|---|
| `.github/workflows/ci.yml` | `--package clawde` → `--package clawde-cli`; comment updated |
| `.github/workflows/release.yml` | deleted (Phase 3) |
| `.github/workflows/patch-release.yml` | deleted (Phase 3) |
| `.github/workflows/auto-release.yml` | deleted (Phase 3) |
| `.github/release.yml` | deleted (label-category config, only used by deleted workflow) |
| `.github/workflows/npm-publish.yml` | drop `workflow_run` trigger + workflow_run checkout + job `if`; comment updated |
| `scripts/build.sh` | rewritten: constants table, `build-one`/`build-all`/`package`/`release` (`--dry-run`, `--publish-only`, `--no-commit`, `--allow-partial`), keeps `install`/`debug`/`run`/`targets`/`clean` |
| `src-rust/Cross.toml` | new: committed cross setup (aarch64 Linux + optional Windows-GNU) |
| `.gitignore` | add `dist/` |
| `AGENTS.md` | Releasing section rewritten; tag force-moves now forbidden outright |
| `README.md`, `docs/installation.md` | dead `--package clawde` refs → `--package clawde-cli`; cross-compile section points at `build.sh` |
| `docs/cheatsheet.md` | (no change needed — `clawde build` already documented) |

**Verification performed:** native + cross legs built (x86_64 and ARM64 ELF confirmed);
`package` output layout byte-compatible with the old workflow (top-level `clawde` /
`clawde.exe`, double-space `SHA256SUMS` parseable by install.sh's awk); `release
--dry-run` side-effect-free (no stamp/commit/push, git status clean); completeness
gate refuses partial publishes; Windows-GNU cross proved non-viable (btls-sys needs
NASM) and is documented as a `manual` leg; `cargo check --workspace` clean.
