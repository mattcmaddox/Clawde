# Release Runbook — building the platform legs

Releases are driven by `scripts/build.sh` from the release machine (a Linux
box). Both Linux legs build there, and the release is packaged + published from
the same box. See `docs/build-release-refactor-spec.md` for the overall design.

## The legs

| Leg (`build-one <id>`) | Build machine | Prerequisites | Produces |
|---|---|---|---|
| `linux-x86_64` | Linux x86_64 (the release box) | Rust, `libasound2-dev`, `pkg-config` | `target/x86_64-unknown-linux-gnu/release/clawde` |
| `linux-aarch64` | Linux (the release box) | Rust, `cross`, Docker | `target/aarch64-unknown-linux-gnu/release/clawde` |

Windows and macOS are not built or published at all (see
`docs/decisions/drop-windows-support.md`); the flow builds Linux only.

## Golden rule

- Build each leg with the **same `scripts/build.sh`**, checked out at the
  **same commit** as the version you are releasing (run `git pull` first —
  the binary embeds the version from `Cargo.toml`).
- `build.sh package` rebuilds `dist/` from
  `src-rust/target/<triple>/release/` and regenerates `SHA256SUMS` — it deletes
  `dist/*` first, so never hand-copy archives into `dist/`.

## Setup per machine

### Linux release box (already set up)

```bash
cargo install cross --git https://github.com/cross-rs/cross   # one-time
sudo apt-get install -y libasound2-dev pkg-config             # one-time
scripts/build.sh build-all      # linux-x86_64 + linux-aarch64
```

## Collecting the legs

Both legs build on the release box itself, so there is nothing to copy in. (The
`--publish-only` path packages whatever is already under `target/`, for the rare
time a leg was built elsewhere.)

Verify the release box sees both legs as ready:

```bash
scripts/build.sh package      # should report "2 packaged, 0 missing"
```

## Publishing

```bash
# Full release — refuses to publish until the expected artifacts (both
# Linux legs) are present:
scripts/build.sh release --version vX.Y.Z

# Preview first (side-effect-free):
scripts/build.sh release --version vX.Y.Z --dry-run
```

`release` stamps the version (`bump-version.py`), commits + pushes, builds
the Linux legs, packages, publishes via `gh release create`, and dispatches
the npm-publish workflow. Every fix cuts a new version — tags are never
force-moved.

## Checklist

- [ ] Release box on `main`, `git pull` done everywhere
- [ ] Both Linux binaries present under `src-rust/target/<triple>/release/`
- [ ] `scripts/build.sh package` reports `2 packaged, 0 missing`
- [ ] `scripts/build.sh release --version vX.Y.Z --dry-run` looks right
- [ ] Real release: `scripts/build.sh release --version vX.Y.Z`
- [ ] `gh release view vX.Y.Z` shows 2 archives + `install.sh` + SHA256SUMS
