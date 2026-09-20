# Hive Remote Build

Build Clawde release artifacts (or any heavy Rust build) on **TheHive**, the LAN build box (x86_64, 16 cores, 15 GB RAM). Use this whenever a local build would take longer than ~10 minutes (release legs, cross-compiles) or when the local machine is busy.

## What TheHive is (and is not)

- It is a **CPU build box**. Rust compilation is pure CPU + disk work — rustc/LLVM have no GPU codepath, and nothing in Clawde's dependency tree can be offloaded to the RTX 3070. Speed comes from the 16 cores, not the GPU.
- The GPU's day job on this box is **Ollama inference** — but read the dated state note under [GPU and CUDA reality](#gpu-and-cuda-reality-verified-2026-09-19) before assuming it is reachable. Never describe Hive builds as "GPU builds"; never waste time probing `nvidia-smi` for build purposes. (`nvidia-smi` is also not on the SSH PATH — the WSL driver mount lives at `/usr/lib/wsl/lib/`.) GPU *compute* is a separate question and does work on this box — see [GPU and CUDA reality](#gpu-and-cuda-reality-verified-2026-09-19); if you use it, the card is shared with Ollama, so keep the job small and brief.

## Access

SSH works non-interactively with the plain `hive` alias from `~/.ssh/config`:

```bash
ssh -o BatchMode=yes -o ConnectTimeout=10 hive "hostname"
```

A working connection prints `TheHive`. (The interactive shell alias `bash -ic 'ssh ...'` variant is no longer needed and adds noisy bashrc output — and that bashrc output once leaked a live `GROQ_API_KEY` into a transcript. Never use `bash -ic` against Hive; the key has since been removed from `.bashrc`, but the rule stands.)

Identity details (fixed 2026-09-17): `~/.ssh/config` previously pinned `IdentityFile ~/.ssh/hivelab`, a key whose private half was gone and whose public half was not in TheHive's `authorized_keys` — so key auth silently failed and every attempt fell back to password prompts. The host block now uses `~/.ssh/id_ed25519` (`churl@TheDrone`), which TheHive trusts. Backup of the old config: `~/.ssh/config.bak-20260917`.

If BatchMode fails, passwordless SSH (key auth) is broken — tell the user to fix it from their own terminal; never attempt password auth from a command (it hangs and leaks the password into shell history).

## Toolchain reality (re-verified 2026-09-19)

- TheHive is **WSL2 (Ubuntu 24.04, glibc 2.39, kernel 6.18.40.1-microsoft-standard-WSL2)** with working Docker — its engine is the native WSL `dockerd` (default context, `unix:///var/run/docker.sock`), NOT Docker Desktop.
- The old Windows pairing was **cleaned up** (2026-09-16): the stale `credsStore: desktop.exe` in `~/.docker/config.json` (now `config.json.bak-wsl-cleanup`), the dead `desktop-linux` context, and `~/.docker/desktop` logs are gone. Plain `docker` commands work — no `DOCKER_CONFIG` override needed anymore.
- `docker buildx` 0.30.1 installed at `/usr/libexec/docker/cli-plugins/` (verified).
- **Bare-metal Rust IS installed** (corrected 2026-09-19 — the old "NOT installed" note was a PATH artifact). Toolchain `stable-x86_64-unknown-linux-gnu`, `cargo`/`rustc` **1.98.1**, in `~/.cargo/bin` and `~/.rustup`. In a non-interactive SSH session `~/.cargo/bin` is **not on `PATH`**, so `command -v cargo` returns nothing and every cargo one-liner dies with "cargo: not found"; that is what made this look missing. Always `export PATH=$HOME/.cargo/bin:$PATH` first — do not reinstall. The only installed target is `x86_64-unknown-linux-gnu`, so cross builds still need `rustup target add aarch64-unknown-linux-gnu`.
- Network works; git + HTTPS clone of the repo works; the user has installed the aarch64 cross packages (gcc/libc-dev/g++ cross + mold) via sudo.
- **sudo requires a password** (`sudo -n` fails) — the agent cannot apt-get anything unattended.

## GPU and CUDA reality (verified 2026-09-19)

The card is an **RTX 3070 (sm_86)**: driver 615.71.08, CUDA UMD 13.4, `/dev/dxg` present. `nvidia-smi` is not on `PATH`; the real binary is `/usr/lib/wsl/lib/nvidia-smi`. The toolkit is CUDA 12.9 at `/usr/local/cuda-12.9` (`nvcc` 12.9.86, with `ptxas`/`fatbinary`/`nvlink`).

What is **absent** — check this before designing anything GPU-side, since each gap forces a different approach:

- `/usr/lib/wsl/lib` (the driver mount / WSL shim) carries `libcuda.so.1` plus debug, encode and monitoring libs only: **no `libcudart`, `libcublas`, `libcurand`, `libnvrtc`, `libnvptxcompiler`, or `libnvidia-ptxjitcompiler.so.1`**.
- The toolkit's `lib64` (→ `targets/x86_64-linux/lib`) holds only `libcudart*`, `libcudadevrt.a`, `libculibos.a` and `libnvptxcompiler_static.a`: **no `libnvrtc`, no `libcublas`, and no `lib64/stubs/`** — so `-lcuda` and `-lcublas` cannot be linked at build time. Talk to the driver API through dynamic loading (`libloading`, or cudarc's `driver` feature) instead.
- **No NVRTC**, i.e. no runtime CUDA-C compilation. cudarc's default `nvrtc` feature is only needed for its `Ptx` wrapper type — never call its compile functions.
- **Vulkan is CPU-only.** `/usr/share/vulkan/icd.d/` carries only Mesa ICDs (`lvp` = lavapipe = software rasterizer, plus radeon/nouveau/intel/asahi); there is no NVIDIA ICD and no D3D12/Dozen ICD. A `wgpu` compute program here passes its own self-checks while executing on the CPU, so it must never be used as evidence of GPU work.

What **works**, exercised by hand on 2026-09-19 with direct driver calls (not inferred from the missing files):

- The **driver API end to end**: `cuInit`, `cuCtxCreate`, `cuModuleLoadData`, `cuModuleGetFunction`, `cuLaunchKernel`, `cuCtxSynchronize` and `cuMemcpyDtoH` all succeeded, and a 4-element saxpy launched from a PTX image *and* from a cubin image returned the expected values. Set `LD_LIBRARY_PATH=/usr/lib/wsl/lib` (`ldconfig` already maps `libcuda.so.1` there; the variable makes the mapping explicit).
- **PTX JIT works**: a 5206-byte `-arch=compute_86` PTX image loads, resolves and executes correctly, so the absent `libnvidia-ptxjitcompiler.so.1` is **not** a blocker — the WSL shim forwards JIT to the Windows driver. A `-cubin -arch=sm_86` image (9376 bytes) behaves identically and skips the per-process JIT.

So a GPU program here looks like: compile kernels with `nvcc` to PTX or cubin at build time (there is no runtime compiler), load the image through the driver API, ship one binary. One gotcha worth knowing up front: cudarc's build script panics unless you name an explicit CUDA version feature (`cuda-12090` for this toolkit). Anything you run shares the card with Ollama — keep GPU jobs small and short.

**Ollama target:** `192.168.1.45` is Hive's own `eth1` address and it is stable — that address at port 11434 is the one correct target, exactly as `AGENTS.md` says. Never substitute `127.0.0.1`: depending on where you stand it is the dev box's CPU instance or a Windows-side instance, neither of which is this service, and core's online-mode resolver rejects loopback anyway (`is_ollama_network_blocked`). If `192.168.1.45:11434` does not answer, the service is down (see the observation below) — report that instead of working around it; switching hosts, or installing/starting an Ollama, is the user's call, not an agent's.

Observed at the 2026-09-19 check, and the cause is local, not the address: nothing listens on 11434 inside WSL. `/usr/local/bin/ollama` is installed and `/etc/systemd/system/` holds `ollama.service`, `ollama-preload.service` and an `ollama.service.d/override.conf` drop-in — but `systemctl list-unit-files` reports both units **disabled**, so nothing is serving. The address itself is healthy: `192.168.1.45:22` still reaches this WSL's `sshd` (the `hive` SSH alias points at that host). Restoring it needs `sudo`, i.e. a password an agent does not have, so hand it back to the user: `sudo systemctl start ollama` (plus `enable` if it should survive a restart). The HTTP 200 on `127.0.0.1:11434` is a different Ollama served from outside this namespace — precisely the confusion to avoid.

**Scratch discipline:** `$HOME` on this box holds the user's own work (e.g. `~/cudaenv`, and `~/gpu_probe` — a CMake/CUDA C++ project that is *not* related to anything in this repo). Create your own directory, remove it when you are done, and never tidy or delete anything you did not create.

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

Rust is already installed (see Toolchain reality above). The only missing piece in a non-interactive SSH session is the `PATH`: cargo lives in `~/.cargo/bin` and is not exported for non-interactive shells.

```bash
ssh -o BatchMode=yes hive 'export PATH=$HOME/.cargo/bin:$PATH && cd ~/clawde/src-rust && cargo check'
```

On a freshly rebuilt box only — i.e. when `~/.cargo/bin` is genuinely absent — bootstrap agent-side (no sudo; installs under `~/.cargo` and `~/.rustup`):

```bash
ssh -o BatchMode=yes hive 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal'
```

The user already installed `gcc-aarch64-linux-gnu`, `libc6-dev-arm64-cross`, `g++-aarch64-linux-gnu`, and `mold` via apt — all four are on `PATH` (`/usr/bin`), and the cross libc is at `/usr/aarch64-linux-gnu/lib` — so the host side of a cross build works. The Rust side needs care: only the `x86_64-unknown-linux-gnu` target is installed, so add the aarch64 std once per toolchain:

```bash
ssh -o BatchMode=yes hive 'export PATH=$HOME/.cargo/bin:$PATH && rustup target add aarch64-unknown-linux-gnu'
```

Then cross builds work:

```bash
ssh -o BatchMode=yes hive 'export PATH=$HOME/.cargo/bin:$PATH && cd ~/clawde/src-rust && nohup env CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc RUSTFLAGS=-Clink-arg=-fuse-ld=mold cargo build --release --target aarch64-unknown-linux-gnu > /tmp/build.log 2>&1 & echo started'
```

Before any bare-metal build, export the cargo `PATH` (above) and check `command -v aarch64-linux-gnu-gcc`; the cross toolchain and mold are already present, so what is usually missing is the `PATH` itself, or the aarch64 std target on a fresh toolchain — fix those rather than failing mid-build. Remember: these binaries carry TheHive's glibc 2.39 requirement — quick checks only.

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
