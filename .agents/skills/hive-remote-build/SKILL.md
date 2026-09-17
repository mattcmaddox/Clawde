# Hive Remote Build

Build Clawde release artifacts (or any heavy Rust build) on **TheHive**, the LAN build box (x86_64, 16 cores, 15 GB RAM). Use this whenever a local build would take longer than ~10 minutes (release legs, cross-compiles) or when the local machine is busy.

## What TheHive is (and is not)

- It is a **CPU build box**. Rust compilation is pure CPU + disk work — rustc/LLVM have no GPU codepath, and nothing in Clawde's dependency tree can be offloaded to the RTX 3070. Speed comes from the 16 cores, not the GPU.
- The GPU's only job on this box is **Ollama inference** (it serves the LAN at `192.168.1.45:11434`). Never describe Hive builds as "GPU builds"; never waste time probing `nvidia-smi` for build purposes. (`nvidia-smi` is also not on the SSH PATH — the WSL driver mount lives at `/usr/lib/wsl/lib/`.)

## Access

SSH works non-interactively with the plain `hive` alias from `~/.ssh/config`:

```bash
ssh -o BatchMode=yes -o ConnectTimeout=10 hive "hostname"
```

A working connection prints `TheHive`. (The interactive shell alias `bash -ic 'ssh ...'` variant is no longer needed and adds noisy bashrc output — and that bashrc output once leaked a live `GROQ_API_KEY` into a transcript. Never use `bash -ic` against Hive; the key has since been removed from `.bashrc`, but the rule stands.)

Identity details (fixed 2026-09-17): `~/.ssh/config` previously pinned `IdentityFile ~/.ssh/hivelab`, a key whose private half was gone and whose public half was not in TheHive's `authorized_keys` — so key auth silently failed and every attempt fell back to password prompts. The host block now uses `~/.ssh/id_ed25519` (`churl@TheDrone`), which TheHive trusts. Backup of the old config: `~/.ssh/config.bak-20260917`.

If BatchMode fails, passwordless SSH (key auth) is broken — tell the user to fix it from their own terminal; never attempt password auth from a command (it hangs and leaks the password into shell history).

## Toolchain reality (verified 2026-09-16)

- TheHive is **WSL2 (Ubuntu 24.04, glibc 2.39)** with working Docker — its engine is the native WSL `dockerd` (default context, `unix:///var/run/docker.sock`), NOT Docker Desktop.
- The old Windows pairing was **cleaned up** (2026-09-16): the stale `credsStore: desktop.exe` in `~/.docker/config.json` (now `config.json.bak-wsl-cleanup`), the dead `desktop-linux` context, and `~/.docker/desktop` logs are gone. Plain `docker` commands work — no `DOCKER_CONFIG` override needed anymore.
- `docker buildx` 0.30.1 installed at `/usr/libexec/docker/cli-plugins/` (verified).
- **Bare-metal Rust is NOT installed** (no cargo/rustc/rustup) — install it agent-side if needed for quick builds (see Bare metal below).
- Network works; git + HTTPS clone of the repo works; the user has installed the aarch64 cross packages (gcc/libc-dev/g++ cross + mold) via sudo.
- **sudo requires a password** (`sudo -n` fails) — the agent cannot apt-get anything unattended.

## Docker vs bare metal — which to use

| Use | Why |
|---|---|
| **Docker (`build-one` legs)** — for anything SHIPPED | The release script builds in `rust:1.98-bookworm`, pinning a glibc 2.36 floor so artifacts run on older distros. TheHive's native glibc is 2.39 — bare-metal binaries would break on Debian 12 / Ubuntu 22.04. The script also self-installs the container's cross packages. |
| **Bare metal cargo** — quick checks only | Fastest path (no container overhead, mold, native fs). Fine for compile smoke-tests or artifacts consumed only on this same machine. NEVER ship bare-metal artifacts to the release. |

## Prebaked image (skip apt on every leg)

Stock `rust:1.98-bookworm` re-installs `pkg-config libasound2-dev cmake golang-go ninja-build libclang-dev` via apt inside a `--rm` container on **every** leg — ~1–3 min wasted each run. A prebaked image with the apt layer baked in removes that; Docker then caches everything.

The image lives on TheHive as `clawde-build:latest` (built once, 2026-09-17; rebuild only when `scripts/build.sh`'s apt package list changes):

```bash
ssh -o BatchMode=yes hive 'docker images clawde-build --format "{{.Repository}}:{{.Tag}} {{.CreatedSince}}"'
# rebuild if needed (Dockerfile is committed in the repo):
ssh -o BatchMode=yes hive 'cd ~/clawde && docker build -t clawde-build:latest -f scripts/docker/clawde-build.Dockerfile scripts/docker/'
```

Point the build script at it via env var (no script edit needed):

```bash
ssh -o BatchMode=yes hive 'cd ~/clawde && LINUX_BUILD_IMAGE=clawde-build:latest nohup scripts/build.sh build-one linux-aarch64 > /tmp/build.log 2>&1 & echo started'
```

## Docker path (release legs — primary)

```bash
# first time / updating:
ssh -o BatchMode=yes hive 'test -d ~/clawde/.git || git clone https://github.com/mattcmaddox/Clawde.git ~/clawde'
ssh -o BatchMode=yes hive 'cd ~/clawde && git fetch origin && git checkout -B main origin/main && git log --oneline -1'

# one leg (long: nohup + poll):
ssh -o BatchMode=yes hive 'cd ~/clawde && LINUX_BUILD_IMAGE=clawde-build:latest nohup scripts/build.sh build-one linux-aarch64 > /tmp/build.log 2>&1 & echo started'
ssh -o BatchMode=yes hive 'tail -5 /tmp/build.log'
```

The container already gets the aarch64 cross toolchain, libc headers (aws-lc-sys), and C++ cross-gcc (btls-sys) — those fixes are committed in `scripts/build.sh`. The cargo registry/target volumes persist across legs (515 MB+ registry), so dependency recompiles only happen when the lockfile or checkout jumps.

## Bare metal path (quick checks)

Rust is not installed by default; bootstrap agent-side (no sudo, installs to ~/.cargo, ~/.rustup):

```bash
ssh -o BatchMode=yes hive 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal && ~/.cargo/bin/rustup target add aarch64-unknown-linux-gnu'
```

The user already installed `gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`, `g++-aarch64-linux-gnu`, and `mold` via apt, so cross builds work:

```bash
ssh -o BatchMode=yes hive 'cd ~/clawde/src-rust && nohup env CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc RUSTFLAGS=-Clink-arg=-fuse-ld=mold cargo build --release --target aarch64-unknown-linux-gnu > /tmp/build.log 2>&1 & echo started'
```

Before any bare-metal build, check `command -v cargo` and `command -v aarch64-linux-gnu-gcc`; install what is missing rather than failing mid-build. Remember: these binaries carry TheHive's glibc 2.39 requirement — quick checks only.

## Workspace setup

The repo is public; clone over HTTPS (TheHive has no GitHub SSH key). Clone or fast-forward an existing checkout:

```bash
ssh -o BatchMode=yes hive 'test -d ~/clawde/.git || git clone https://github.com/mattcmaddox/Clawde.git ~/clawde'
ssh -o BatchMode=yes hive 'cd ~/clawde && git fetch origin && git checkout -B main origin/main && git log --oneline -1'
```

Always confirm the checked-out commit matches the commit you intend to build.

## Running builds

Both paths run under `nohup` with a log file and get polled — never a foreground SSH for anything over ~8 minutes (tool timeouts). Poll with:

```bash
ssh -o BatchMode=yes hive 'tail -5 /tmp/build.log'
```

## Retrieving artifacts

Copy the built binary back into the local checkout's expected target path before packaging/publishing locally (Docker-leg path shown; bare-metal lands in the same place):

```bash
mkdir -p /home/churl/clawde/src-rust/target/aarch64-unknown-linux-gnu/release
scp hive:~/clawde/src-rust/target/aarch64-unknown-linux-gnu/release/clawde \
  /home/churl/clawde/src-rust/target/aarch64-unknown-linux-gnu/release/clawde
```

Then run the publish step locally (`scripts/build.sh release --version vX.Y.Z --publish-only`, or `--allow-partial` when a platform leg is intentionally missing).

## Division of labor (release flow)

1. Local machine: version stamp, commit/push, linux-x86_64 leg, GitHub Release creation, npm dispatch.
2. TheHive: heavy native legs only (e.g. linux-aarch64 cross build) on its 16 CPU cores.
3. Copy artifacts back, publish locally with whatever legs are complete.
