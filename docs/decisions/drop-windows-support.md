# Decision note: drop Windows support

**Recorded:** 2026-09-16
**Status:** Proposed — owner decision pending

Windows has been a recurring CI and support burden disproportionate to its use.

## Evidence (single day, 2026-09-16 CI cascade)

Nine fix commits in a row were required to get CI green; nearly every failure was Windows-only:

- `nix::fcntl` un-gated in katban's `BoardLock` — the crate did not even compile on Windows (latent since katban landed; masked because other legs failed first)
- 1 MiB main-thread stack default (Linux is 8 MiB) — the headless query loop overflowed (`STATUS_STACK_OVERFLOW`, exit `-1073741571`), killing every spawned-`clawde` integration test (fixed via linker `/STACK` reserve in `src-rust/.cargo/config.toml`)
- `tokio::spawn` without a reactor in the Ollama config persist path (panicked in sync tests)
- Test fixtures assuming Unix path shapes: `/workspace`, `/bin/server`, `sessions/2026-08-01.md`, `specs/example-feature.json`
- Tool output echoing drive-letter paths (`D:/nonexistent/...`) breaking the gateway golden-stream fixture
- Four katban systemd-exposure tests impossible on Windows (systemd is a Linux surface)
- Local/CI clippy version skew (0.1.91 vs 1.98) compounded the noise

## Scope of removal (when approved)

1. CI: drop `windows-latest` from the test matrix in `.github/workflows/ci.yml`
2. Release: stop building/publishing `clawde-windows-x86_64.zip` (see `scripts/build.sh` and the README "Supported Platforms" table, line ~104)
3. Release: drop the Windows leg from the cross-compile plan (local `x86_64-pc-windows-gnu` / `-msvc` targets, TheHive build docs in `.agents/skills/hive-remote-build/SKILL.md`)
4. Code: optionally remove the Windows-only workarounds that exist purely for this target (`.cargo/config.toml` stack reserve, `cfg(windows)` branches, kitty-keyboard push/pop note in `crates/tui`), or leave them as harmless dead paths
5. Docs: remove Windows install instructions from `README.md` and `docs/installation.md`

## Cost of keeping it

Every platform-agnostic change risks a surprise Windows-only failure that cannot be reproduced locally (dev box is Linux), so each one costs a push-and-wait CI cycle. The stack-overflow and compile-gating bugs above were real product bugs on Windows, not test noise — meaning the platform was already degraded for real users, not just CI.
