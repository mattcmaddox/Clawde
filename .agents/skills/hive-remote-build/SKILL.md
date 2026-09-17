# Hive Remote Build

Build Clawde release artifacts (or any heavy Rust build) on **TheHive**, the LAN build box (x86_64, 16 cores, 15 GB RAM). Use this whenever a local build would take longer than ~10 minutes (release legs, cross-compiles) or when the local machine is busy.

## Access

TheHive is reachable via the user's shell alias `hive`, which resolves to `ssh hive`. Interactive shell aliases are not visible to non-login shells, so ALWAYS invoke through an interactive login shell:

```bash
bash -ic 'ssh -o BatchMode=yes -o ConnectTimeout=10 hive "hostname"'
```

A working connection prints `TheHive` (the noisy bashrc output above it can be ignored — filter with `grep '='` or `tail`). If BatchMode fails, passwordless SSH (key auth) is broken — tell the user to run `ssh-copy-id hive` from their own terminal; never attempt password auth from a command (it hangs and leaks the password into shell history).

## Toolchain reality (verified 2026-09-16)

- TheHive is **WSL2 (Ubuntu 24.04, glibc 2.39)** with working Docker — its engine is the native WSL `dockerd` (default context, `unix:///var/run/docker.sock`), NOT Docker Desktop.
- The old Windows pairing was **cleaned up** (2026-09-16): the stale `credsStore: desktop.exe` in `~/.docker/config.json` (now `config.json.bak-wsl-cleanup`), the dead `desktop-linux` context, and `~/.docker/desktop` logs are gone. Plain `docker` commands work — no `DOCKER_CONFIG` override needed anymore.
- `docker buildx` 0.30.1 installed at `/usr/libexec/docker/cli-plugins/` (verified). Pull/run verified (local images persist from the Desktop era and are still usable).
- **Bare-metal Rust is NOT installed** (no cargo/rustc/rustup) — install it agent-side if needed for quick builds (see Bare metal below).
- Network works; git + HTTPS clone of the repo works; the user has installed the aarch64 cross packages (gcc/libc-dev/g++ cross + mold) via sudo.
- **sudo requires a password** (`sudo -n` fails) — the agent cannot apt-get anything unattended.

## Docker vs bare metal — which to use

| Use | Why |
|---|---|
| **Docker (`build-one` legs)** — for anything SHIPPED | The release script builds in `rust:1.98-bookworm`, pinning a glibc 2.36 floor so artifacts run on older distros. TheHive's native glibc is 2.39 — bare-metal binaries would break on Debian 12 / Ubuntu 22.04. The script also self-installs the container's cross packages. |
| **Bare metal cargo** — quick checks only | Fastest path (no container overhead, mold, native fs). Fine for compile smoke-tests or artifacts consumed only on this same machine. NEVER ship bare-metal artifacts to the release. |

## Docker path (release legs — primary)

```bash
# first time / updating:
bash -ic 'ssh hive "test -d ~/clawde/.git || git clone https://github.com/mattcmaddox/Clawde.git ~/clawde"'
bash -ic 'ssh hive "cd ~/clawde && git fetch origin && git checkout -B main origin/main && git log --oneline -1"'

# one leg (long: nohup + poll; plain docker works post-cleanup):
bash -ic 'ssh hive "cd ~/clawde && nohup scripts/build.sh build-one linux-aarch64 > /tmp/build.log 2>&1 & echo started"'
bash -ic 'ssh hive "tail -5 /tmp/build.log"'
```

The container already gets the aarch64 cross toolchain, libc headers (aws-lc-sys), and C++ cross-gcc (btls-sys) — those fixes are committed in `scripts/build.sh`.

## Bare metal path (quick checks)

Rust is not installed by default; bootstrap agent-side (no sudo, installs to ~/.cargo, ~/.rustup):

```bash
bash -ic 'ssh hive "curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal && ~/.cargo/bin/rustup target add aarch64-unknown-linux-gnu"'
```

The user already installed `gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`, `g++-aarch64-linux-gnu`, and `mold` via apt, so cross builds work:

```bash
bash -ic 'ssh hive "cd ~/clawde/src-rust && nohup env CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc RUSTFLAGS=-Clink-arg=-fuse-ld=mold cargo build --release --target aarch64-unknown-linux-gnu > /tmp/build.log 2>&1 & echo started"'
```

Before any bare-metal build, check `command -v cargo` and `command -v aarch64-linux-gnu-gcc`; install what is missing rather than failing mid-build. Remember: these binaries carry TheHive's glibc 2.39 requirement — quick checks only.

## Workspace setup

The repo is public; clone over HTTPS (TheHive has no GitHub SSH key). Clone or fast-forward an existing checkout:

```bash
bash -ic 'ssh hive "test -d ~/clawde/.git || git clone https://github.com/mattcmaddox/Clawde.git ~/clawde"'
bash -ic 'ssh hive "cd ~/clawde && git fetch origin && git checkout -B main origin/main && git log --oneline -1"'
```

Always confirm the checked-out commit matches the commit you intend to build.

## Bootstrap (see Setup status above)

Rust/rustup installs agent-side without sudo. The apt cross packages (`gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`, `g++-aarch64-linux-gnu`) need the USER to run the sudo block once. Verify before relying on either:

```bash
bash -ic 'ssh hive "command -v cargo; command -v aarch64-linux-gnu-gcc"'
```

## Running builds

Both paths run under `nohup` with a log file and get polled — never a foreground SSH for anything over ~8 minutes (tool timeouts). Poll with:

```bash
bash -ic 'ssh hive "tail -5 /tmp/build.log"'
```

## Retrieving artifacts

Copy the built binary back into the local checkout's expected target path before packaging/publishing locally (Docker-leg path shown; bare-metal lands in the same place):

```bash
mkdir -p /home/churl/clawde/src-rust/target/aarch64-unknown-linux-gnu/release
bash -ic 'scp hive:~/clawde/src-rust/target/aarch64-unknown-linux-gnu/release/clawde \
  /home/churl/clawde/src-rust/target/aarch64-unknown-linux-gnu/release/clawde'
```

Then run the publish step locally (`scripts/build.sh release --version vX.Y.Z --publish-only`, or `--allow-partial` when a platform leg is intentionally missing — e.g. Windows, which must be built on a Windows host).

## Division of labor (release flow)

1. Local machine: version stamp, commit/push, linux-x86_64 leg, GitHub Release creation, npm dispatch.
2. TheHive: heavy native legs only (e.g. linux-aarch64 cross build) on its 16 cores.
3. Copy artifacts back, publish locally with whatever legs are complete.
