# Clawde Installation Guide

Clawde is a Rust reimplementation of the Claude Code CLI. The fastest way
to install it is via the one-liner installers below. They drop the binary
into `~/.clawde/bin` and add that directory to your `PATH` automatically.

---

## System Requirements

| Platform | Architecture | Minimum OS |
|----------|-------------|------------|
| Linux    | x86_64      | glibc 2.17+ (most distros from 2014 onward) |
| Linux    | aarch64     | glibc 2.17+ (Raspberry Pi 4, AWS Graviton, etc.) |
| macOS    | x86_64      | macOS 11 Big Sur |
| macOS    | aarch64     | macOS 11 Big Sur (Apple Silicon: M1/M2/M3) |

There are no other runtime dependencies. The binary is statically linked where
possible; on Linux it links against the system glibc.

---

## Quick install (recommended)

### Linux / macOS

```bash
curl -fsSL https://github.com/mattcmaddox/Clawde/releases/latest/download/install.sh | bash
```

The installer:

1. Detects your architecture.
2. Downloads the matching archive from the latest GitHub release.
3. Extracts `clawde` into `~/.clawde/bin/`.
4. Appends that directory to your shell config (`.bashrc`, `.zshrc`,
   `.config/fish/config.fish`).
5. On macOS, strips the quarantine attribute so Gatekeeper does not block the
   unsigned binary.

Open a new terminal afterwards (or `source` the modified shell config) so
the updated `PATH` takes effect, then run `clawde --version` to verify.

### Installer flags

| Flag | Effect |
|---|---|
| `--version 0.1.0` | Install a specific version |
| `--binary <path>` | Install from a local file (skip download) |
| `--install-dir <path>` | Override the install directory |
| `--no-modify-path` | Don't touch shell config |
| `--help` | Show usage |

Example: `curl -fsSL https://.../install.sh | bash -s -- --version 0.1.0`

---

## Via npm / bun

If you have Node.js or Bun installed, you can install Clawde as a global
package. The postinstall script automatically downloads the correct pre-built
native binary for your platform from GitHub Releases — no compilation needed.

```bash
# npm
npm install -g clawde

# bun
bun install -g clawde
```

After installation, run `clawde` directly from your terminal.

You can also run Clawde without a permanent install:

```bash
npx clawde          # via npm
bunx clawde         # via bun
```

**Supported platforms via npm:**

| Platform | Architecture |
|----------|-------------|
| Linux    | x86_64, aarch64 |
| macOS    | x86_64 (Intel), aarch64 (Apple Silicon) |

---

## Upgrading

Once installed, upgrade in place at any time:

```bash
clawde upgrade               # to the latest release
clawde upgrade --version 0.1.0   # pin to a specific version
clawde upgrade --force       # reinstall the same version
```

The upgrade command downloads the matching archive from GitHub, extracts the
new binary, and replaces the running executable atomically. Settings in
`~/.clawde/` are preserved.

---

## Manual install from GitHub Releases

If you'd rather not run an install script, grab archives directly from
[**GitHub Releases**](https://github.com/mattcmaddox/Clawde/releases):

| Archive | Platform |
|---------|----------|
| `clawde-linux-x86_64.tar.gz` | Linux x86_64 |
| `clawde-linux-aarch64.tar.gz` | Linux ARM64 |
| `clawde-macos-x86_64.tar.gz` | macOS Intel |
| `clawde-macos-aarch64.tar.gz` | macOS Apple Silicon |

Every archive contains a single binary named `clawde`.
Extract it and put it somewhere on your `PATH`. For example on Linux:

```bash
curl -L https://github.com/mattcmaddox/Clawde/releases/latest/download/clawde-linux-x86_64.tar.gz \
  | tar -xz
chmod +x clawde
sudo mv clawde /usr/local/bin/
```

On macOS, also strip the quarantine flag so Gatekeeper allows the unsigned
binary:

```bash
xattr -rd com.apple.quarantine /usr/local/bin/clawde
```

### User-local install without sudo

```bash
mkdir -p ~/.local/bin
mv clawde ~/.local/bin/clawde
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

For Zsh users, substitute `.zshrc` for `.bashrc`.

---

## Verifying the Installation

```bash
clawde --version
```

A successful installation prints the version string, for example:

```
clawde 0.3.3
```

To confirm the binary is the one you installed:

```bash
which clawde          # Linux / macOS
```

---

## Building from Source

Building from source requires the Rust toolchain (stable channel, 1.75 or
later). Install Rust via [rustup](https://rustup.rs/):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Option A: Install via Cargo

```bash
cargo install clawde --force
```

This downloads, compiles, and installs the binary to `~/.cargo/bin/clawde`.
That directory is added to `PATH` automatically by `rustup`.

### Option B: Clone and Build

```bash
git clone https://github.com/mattcmaddox/Clawde.git
cd clawde/src-rust

# Debug build (fast to compile, larger binary, extra runtime checks)
cargo build --package clawde-cli

# Release build (optimised, smaller, suitable for everyday use)
cargo build --release --package clawde-cli
```

The release binary is placed at:

```
src-rust/target/release/clawde
```

Copy it to a directory on your `PATH` as described above.

### Linux system dependencies

On Linux, the build requires ALSA development headers (for the optional voice
feature) and OpenSSL:

```bash
# Debian / Ubuntu
sudo apt-get install -y libasound2-dev libssl-dev pkg-config

# Fedora / RHEL
sudo dnf install -y alsa-lib-devel openssl-devel

# Arch
sudo pacman -S alsa-lib openssl
```

### Optional cargo features

| Feature | Description |
|---------|-------------|
| `voice` | Microphone input / voice prompting |
| `computer-use` | Screenshot capture and mouse/keyboard control |
| `dev_full` | All experimental features combined |

To enable a feature:

```bash
cargo build --release --package clawde-cli --features voice
cargo build --release --package clawde-cli --features dev_full
```

### Cross-compiling for Linux aarch64

Use `scripts/build.sh`, the single source of truth for building and
releasing clawde (see `docs/build-release-refactor-spec.md`). It builds
ARM64 Linux via [cross](https://github.com/cross-rs/cross) under Docker,
which manages the sysroot, OpenSSL, and ALSA headers automatically:

```bash
# one-time cross install
cargo install cross --git https://github.com/cross-rs/cross

# build the ARM64 Linux leg
scripts/build.sh build-one linux-aarch64

# build every leg this machine can, or package + publish a release
scripts/build.sh build-all
scripts/build.sh release --version vX.Y.Z --dry-run
```

---

## Shell Completions

Clawde does not currently ship a dedicated `completions` subcommand. All
flags can be discovered via `clawde --help`. If you want basic tab completion
in bash or zsh you can use the generic completion helper built into your shell:

```bash
# bash — add to ~/.bashrc
complete -C clawde clawde

# zsh — add to ~/.zshrc (requires compinit)
compdef _gnu_generic clawde
```

Richer completion scripts may be added in a future release.

---

## Upgrading a source install

```bash
cargo install clawde --force
```

For binary installs (the recommended path), use `clawde upgrade` — see
the [Upgrading](#upgrading) section above.

---

## Uninstalling

If you used the install script, remove the install directory:

```bash
rm -rf ~/.clawde/bin                    # Linux / macOS
```

For manual installs:

```bash
sudo rm /usr/local/bin/clawde           # if installed system-wide
rm ~/.local/bin/clawde                  # if installed user-local
```

To also remove all settings and session data:

```bash
rm -rf ~/.clawde
```

You may also want to remove the `# clawde` PATH line that the installer
appended to your shell config (`.bashrc`, `.zshrc`, etc.).
