# Release Runbook — building the platform legs

Releases are driven by `scripts/build.sh` from the release machine (a Linux
box). The Linux legs build there; the Windows leg must be built on a Windows
machine using the **same script**, then its binary is copied back into
`target/` and the release is packaged + published from the Linux box. See
`docs/build-release-refactor-spec.md` for the overall design.

## The legs

| Leg (`build-one <id>`) | Build machine | Prerequisites | Produces |
|---|---|---|---|
| `linux-x86_64` | Linux x86_64 (the release box) | Rust, `libasound2-dev`, `pkg-config` | `target/x86_64-unknown-linux-gnu/release/clawde` |
| `linux-aarch64` | Linux (the release box) | Rust, `cross`, Docker | `target/aarch64-unknown-linux-gnu/release/clawde` |
| `windows-x86_64` | Windows 10/11 x86_64 | Rust (MSVC), VS Build Tools | `target\x86_64-pc-windows-msvc\release\clawde.exe` |

Windows is a **manual** leg: cross-compiling the GNU target from Linux fails
(`btls-sys` builds BoringSSL via CMake and needs NASM, absent from cross's
image). macOS legs are intentionally not built — no Apple hardware in the
release flow (decided 2026-08-22).

## Golden rule

- Build each leg with the **same `scripts/build.sh`**, checked out at the
  **same commit** as the version you are releasing (run `git pull` first —
  the binary embeds the version from `Cargo.toml`).
- Copy the finished **binary** (not an archive) back into the release box's
  `src-rust/target/<triple>/release/` path. `build.sh package` rebuilds
  `dist/` from those paths and regenerates `SHA256SUMS` — it deletes `dist/*`
  first, so never hand-copy archives into `dist/`.

## Setup per machine

### Windows (x86_64)

1. **Rust (MSVC toolchain)** — install via `winget install Rustlang.Rustup`
   or `rustup-init.exe` from <https://rustup.rs>. Keep the default MSVC
   toolchain. Verify: `rustup show` lists `x86_64-pc-windows-msvc`.
2. **Visual Studio Build Tools** — install the **"Desktop development with
   C++"** workload (provides the MSVC linker and Windows SDK; rustc locates
   them automatically). Verify `link.exe` is reachable via a Developer
   PowerShell, or just try the build below.
3. **Git for Windows** (provides Git Bash) — needed to run the bash script.
4. Build, from Git Bash:
   ```bash
   git clone https://github.com/mattcmaddox/Clawde.git && cd Clawde
   bash scripts/build.sh build-one windows-x86_64
   ```
   The script detects Git Bash via `uname -s` (`MINGW*`/`MSYS*`) and uses
   native `cargo` with the MSVC target. Output:
   `src-rust/target/x86_64-pc-windows-msvc/release/clawde.exe`.

No ALSA/OpenSSL step needed — cpal uses WASAPI on Windows.

### Linux release box (already set up)

```bash
cargo install cross --git https://github.com/cross-rs/cross   # one-time
sudo apt-get install -y libasound2-dev pkg-config             # one-time
scripts/build.sh build-all      # linux-x86_64 + linux-aarch64
```

## Collecting the legs

On each remote machine, copy the binary back to the release box:

```bash
# Windows leg (from the release box):
scp win-box:Clawde/src-rust/target/x86_64-pc-windows-msvc/release/clawde.exe \
    src-rust/target/x86_64-pc-windows-msvc/release/
```

Then verify the release box sees all three as ready:

```bash
scripts/build.sh package      # should report "3 packaged, 0 missing"
```

## Publishing

```bash
# Full release — refuses to publish until all 3 artifacts are present:
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
- [ ] All 3 binaries present under `src-rust/target/<triple>/release/`
- [ ] `scripts/build.sh package` reports `3 packaged, 0 missing`
- [ ] `scripts/build.sh release --version vX.Y.Z --dry-run` looks right
- [ ] Real release: `scripts/build.sh release --version vX.Y.Z`
- [ ] `gh release view vX.Y.Z` shows 3 archives + installers + SHA256SUMS
