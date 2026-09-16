#!/usr/bin/env bash
# build.sh — the single source of truth for building, packaging, and
# releasing clawde. See docs/build-release-refactor-spec.md.
#
# The binary also has a built-in equivalent for the dev loop: `clawde build`
# (rebuild + replace itself) — prefer that when the source is on this machine.
# This script additionally covers cross-compiles, packaging, and publishing.
#
# Usage:
#   ./build.sh                     build release + install to ~/.local/bin
#   ./build.sh install             same as above
#   ./build.sh debug               build debug binary only (fast, for dev)
#   ./build.sh run                 build debug + run clawde (pass args after --)
#   ./build.sh build-one <id>      build a single platform leg
#   ./build.sh build-all           build every leg this machine can
#   ./build.sh package             assemble dist/: archives + SHA256SUMS
#   ./build.sh release --version vX.Y.Z [--publish-only] [--no-commit] [--dry-run]
#   ./build.sh targets             show which cross targets are ready
#   ./build.sh clean               remove build artifacts
#
# Platform legs:
#   linux-x86_64   container (rust:1.98-bullseye)  (this machine)
#   linux-aarch64  container + gcc-aarch64 cross   (this machine)
#   windows-x86_64 needs a Windows box (MSVC; BoringSSL build needs NASM, so
#                  cross-building the GNU target from Linux is not viable)
# (macOS legs intentionally not built — no Apple hardware in the release flow)
#
# Why the container: Linux builds must bind old GLIBC symbol versions. Host
# (Ubuntu 24.04, glibc 2.39) and cross's :main image (also 24.04) emit
# GLIBC_2.38/2.39 versioned refs (`__isoc23_*` from C deps, pidfd_spawnp from
# rustc 1.91's std) that make the binary refuse to load on anything older
# than 24.04 — e.g. Raspberry Pi OS Bookworm (2.36). Building inside Debian
# bookworm (glibc 2.36) binds only <=2.36 versions: runs on Pi OS Bookworm,
# Debian 12+, Ubuntu 22.04+. (bullseye would give a 2.31 floor but hit LTS
# end-of-life in 2026-08; its security pool 404s mid-migration to the
# archive, so it is no longer a reliable build base.) rustc must be >=1.98:
# older std unconditionally references pidfd_spawnp@GLIBC_2.39 (1.91 does).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_DIR="$REPO_ROOT/src-rust"

# ── Release constants — single source of truth ──────────────────────────
REPO="mattcmaddox/Clawde"
PKG="clawde-cli"          # cargo package (its only binary is named `clawde`)
BIN_NAME="clawde"         # binary name inside archives
DIST_DIR="$REPO_ROOT/dist"
INSTALL_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"
LINUX_BUILD_IMAGE="rust:1.98-bookworm"  # glibc 2.36 floor; see header note

# id -> (rust triple | builder-for-this-machine)
# builder: native | cross | manual
target_info() {  # $1 = id; echoes "triple"
    case "$1" in
        linux-x86_64)   echo "x86_64-unknown-linux-gnu" ;;
        linux-aarch64)  echo "aarch64-unknown-linux-gnu" ;;
        windows-x86_64) echo "x86_64-pc-windows-msvc" ;;
        *) return 1 ;;
    esac
}

target_ids() {
    echo "linux-x86_64 linux-aarch64 windows-x86_64"
}

native_here() {  # $1 = triple — can this machine natively build it?
    local triple="$1"
    case "$triple" in
        x86_64-unknown-linux-gnu)  [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "x86_64" ]] ;;
        aarch64-unknown-linux-gnu) [[ "$(uname -s)" == "Linux" && "$(uname -m)" == "aarch64" ]] ;;
        x86_64-pc-windows-msvc)    [[ "$(uname -s)" == MINGW* || "$(uname -s)" == MSYS* || "$(uname -s)" == CYGWIN* ]] ;;
        *) false ;;
    esac
}

builder_for() {  # $1 = id → container | cross | manual
    local id="$1" triple
    triple="$(target_info "$id")" || return 1
    case "$id" in
        linux-x86_64|linux-aarch64) echo "container" ;;
        *)
            if native_here "$triple"; then
                echo "native"
            else
                echo "manual"
            fi
            ;;
    esac
}

# Build one Linux leg inside the pinned old-glibc image. x86_64 compiles
# natively in the container; aarch64 cross-compiles with the Debian cross
# toolchain (no QEMU — a full emulated build would take hours). Artifacts
# are copied into the conventional target/<triple>/release layout so
# package()/find_binary() need no special-casing.
container_build_leg() {  # $1 = leg id
    local id="$1" triple ctarget
    triple="$(target_info "$id")"
    ctarget="$SRC_DIR/target/container-$id"
    echo ":: Building $id ($triple) in $LINUX_BUILD_IMAGE (glibc 2.36 floor) ..."
    rm -f "$SRC_DIR/target/$triple/release/clawde"
    docker run --rm -i --platform linux/amd64 \
        -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
        -e PKG="$PKG" -e LEG_ID="$id" -e TRIPLE="$triple" \
        -v "$SRC_DIR:/clawde" -w /clawde \
        -v clawde-cargo-registry:/usr/local/cargo/registry \
        -v clawde-cargo-git:/usr/local/cargo/git \
        "$LINUX_BUILD_IMAGE" bash -se <<'EOS'
set -e
# bullseye is near LTS EOL and its security pool occasionally 404s mid-fetch;
# retry the install once after a fresh update before giving up.
apt_install() {
    apt-get install -y -qq --no-install-recommends \
        -o Acquire::Retries=5 "$@" >/dev/null \
        || { apt-get update -qq && apt-get install -y -qq --no-install-recommends \
                 -o Acquire::Retries=5 "$@" >/dev/null; }
}
if [[ "$LEG_ID" == "linux-aarch64" ]]; then
    dpkg --add-architecture arm64
    apt-get update -qq
    apt_install pkg-config gcc-aarch64-linux-gnu libasound2-dev:arm64 cmake golang-go ninja-build libclang-dev
    rustup target add "$TRIPLE"
    export CARGO_TARGET_DIR=/clawde/target/container-$LEG_ID
    export PKG_CONFIG_ALLOW_CROSS=1
    export PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
    export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc
    export CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++
    export AR_aarch64_unknown_linux_gnu=aarch64-linux-gnu-ar
    TARGET_FLAG="--target $TRIPLE"
else
    apt-get update -qq
    apt_install pkg-config libasound2-dev cmake golang-go ninja-build libclang-dev
    export CARGO_TARGET_DIR=/clawde/target/container-$LEG_ID
    TARGET_FLAG=""
fi
cargo build --release --locked --package "$PKG" $TARGET_FLAG
chown -R "$HOST_UID:$HOST_GID" "$CARGO_TARGET_DIR" || true
BIN_PATH="$CARGO_TARGET_DIR/$TRIPLE/release/clawde"
[[ "$LEG_ID" == "linux-x86_64" ]] && BIN_PATH="$CARGO_TARGET_DIR/release/clawde"
mkdir -p "/clawde/target/$TRIPLE/release"
cp "$BIN_PATH" "/clawde/target/$TRIPLE/release/clawde"
EOS
    [[ -f "$SRC_DIR/target/$triple/release/clawde" ]] \
        || die "container build produced no binary for $id"
}

# ── Helpers ─────────────────────────────────────────────────────────────

die() {
    echo "error: $*" >&2
    exit 1
}

require_cross() {
    command -v cross >/dev/null 2>&1 \
        || die "cross is not installed. Install it first:
  cargo install cross --git https://github.com/cross-rs/cross
  (requires Docker running)"
}

archive_name() {  # $1 = id → e.g. clawde-linux-x86_64.tar.gz
    local id="$1" ext="tar.gz"
    [[ "$id" == windows-* ]] && ext="zip"
    echo "$BIN_NAME-$id.$ext"
}

find_binary() {  # $1 = id → prints absolute path if a build exists, else nothing
    local id="$1" triple ext="" p
    triple="$(target_info "$id")" || return 1
    [[ "$id" == windows-* ]] && ext=".exe"
    p="$SRC_DIR/target/$triple/release/$BIN_NAME$ext"
    [[ -f "$p" ]] && { echo "$p"; return; }
    # A manually-built GNU-target exe is still a valid windows x86_64 artifact.
    if [[ "$id" == "windows-x86_64" ]]; then
        p="$SRC_DIR/target/x86_64-pc-windows-gnu/release/$BIN_NAME.exe"
        [[ -f "$p" ]] && echo "$p"
    fi
}

# ── Build subcommands ───────────────────────────────────────────────────

build_release() {
    echo ":: Building release clawde ..."
    (cd "$SRC_DIR" && cargo build --release --package "$PKG")
    echo ":: Binary: $SRC_DIR/target/release/clawde"
}

install_binary() {
    build_release
    mkdir -p "$INSTALL_DIR"
    cp "$SRC_DIR/target/release/clawde" "$INSTALL_DIR/clawde"
    chmod 755 "$INSTALL_DIR/clawde"
    echo ":: Installed to $INSTALL_DIR/clawde"
    echo "   Run 'clawde' from any directory. Version: $("$INSTALL_DIR/clawde" --version)"
}

build_debug() {
    echo ":: Building debug clawde ..."
    (cd "$SRC_DIR" && cargo build --package "$PKG")
    echo ":: Binary: $SRC_DIR/target/debug/clawde"
}

run_debug() {
    build_debug
    exec "$SRC_DIR/target/debug/clawde" "$@"
}

build_one() {  # $1 = id (or a raw rust triple)
    local id="$1" triple builder
    if triple="$(target_info "$id")"; then
        builder="$(builder_for "$id")"
    else
        # Treat the argument as a raw triple; attempt via cross.
        triple="$id"
        builder="cross"
    fi

    case "$builder" in
        container)
            container_build_leg "$id"
            ;;
        native)
            echo ":: Building $id ($triple, native) ..."
            (cd "$SRC_DIR" && cargo build --release --locked --package "$PKG" --target "$triple")
            ;;
        cross)
            require_cross
            docker info >/dev/null 2>&1 || die "Docker is not running — cross needs it."
            echo ":: Building $id ($triple, cross) ..."
            (cd "$SRC_DIR" && cross build --release --locked --package "$PKG" --target "$triple")
            ;;
        manual)
            echo ":: Skipping $id — cannot be built on this machine."
            echo "   On the right machine, run the same script:"
            echo "     build.sh build-one $id"
            echo "   then copy the binary into src-rust/target/$(target_info "$id")/release/ "
            echo "   (see docs/release-runbook.md) and re-run package/release."
            ;;
    esac
}

build_all() {
    local failed=0
    for id in $(target_ids); do
        if ! build_one "$id"; then
            failed=$((failed + 1))
        fi
    done
    if (( failed > 0 )); then
        echo ":: $failed leg(s) failed to build"
        return 1
    fi
    echo ":: All legs built that this machine can build."
}

# ── Package ─────────────────────────────────────────────────────────────

package() {
    mkdir -p "$DIST_DIR"
    rm -f "$DIST_DIR"/* 2>/dev/null || true

    local found=0 missing=0 id bin os arch out stage
    for id in $(target_ids); do
        bin="$(find_binary "$id")" || true
        if [[ -z "$bin" ]]; then
            echo ":: Missing $id binary — build it first (build-one/build-all)."
            missing=$((missing + 1))
            continue
        fi
        os="${id%-*}"
        arch="${id#*-}"
        out="$DIST_DIR/$(archive_name "$id")"
        stage="$DIST_DIR/.stage-$id"
        rm -rf "$stage"
        mkdir -p "$stage"
        if [[ "$os" == "windows" ]]; then
            cp "$bin" "$stage/$BIN_NAME.exe"
            if command -v zip >/dev/null 2>&1; then
                (cd "$stage" && zip -q "$out" "$BIN_NAME.exe")
            else
                (cd "$stage" && python3 -m zipfile -c "$out" "$BIN_NAME.exe")
            fi
        else
            cp "$bin" "$stage/$BIN_NAME"
            chmod +x "$stage/$BIN_NAME"
            tar -czf "$out" -C "$stage" "$BIN_NAME"
        fi
        rm -rf "$stage"
        echo ":: Packaged $(archive_name "$id")"
        found=$((found + 1))
    done

    if (( found > 0 )); then
        [[ -f "$REPO_ROOT/install.sh" ]] && cp "$REPO_ROOT/install.sh" "$DIST_DIR/install.sh"
        [[ -f "$REPO_ROOT/install.ps1" ]] && cp "$REPO_ROOT/install.ps1" "$DIST_DIR/install.ps1"
        # Hash into a temp name first so SHA256SUMS never hashes itself.
        (cd "$DIST_DIR" && sha256sum * > .SHA256SUMS.tmp && mv .SHA256SUMS.tmp SHA256SUMS)
        echo ":: SHA256SUMS written"
    fi

    echo ":: $found packaged, $missing missing"
    ls -lh "$DIST_DIR"
}

# ── Release ─────────────────────────────────────────────────────────────

generate_notes() {  # $1 = version tag
    local version="$1" prev
    prev="$(git -C "$REPO_ROOT" tag --sort=-v:refname | grep -vxF "$version" | head -1 || true)"
    {
        echo "## What's new in Clawde $version"
        echo
        if [[ -n "$prev" ]]; then
            echo "Commits since $prev:"
            echo
            git -C "$REPO_ROOT" log --no-merges --pretty=format:'- %s ([`%h`](https://github.com/'"$REPO"'/commit/%H))' "$prev..HEAD"
            echo
        else
            echo "Initial release."
        fi
    }
}

release() {
    local version="" publish_only=0 no_commit=0 dry_run=0 allow_partial=0

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --version) version="${2:-}"; shift 2 ;;
            --publish-only) publish_only=1; shift ;;
            --no-commit) no_commit=1; shift ;;
            --dry-run) dry_run=1; shift ;;
            --allow-partial) allow_partial=1; shift ;;
            *) die "Unknown release option: $1" ;;
        esac
    done

    # ── 1. Preflight ──────────────────────────────────────────────────
    [[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
        || die "release requires --version vMAJOR.MINOR.PATCH"
    if (( publish_only )); then
        echo ":: Publishing $version from existing dist/ artifacts."
    else
        command -v gh >/dev/null 2>&1 || die "gh CLI is required for releases."
        gh auth status >/dev/null 2>&1 || die "gh is not authenticated."
        if [[ "$(git -C "$REPO_ROOT" branch --show-current)" != "main" ]]; then
            echo "warning: not on main — releasing from a branch."
        fi
    fi

    local plain="${version#v}"

    # ── 2. Stamp version (if Cargo.toml differs) ──────────────────────
    local cargo_ver
    cargo_ver="$(grep '^version' "$SRC_DIR/Cargo.toml" | head -1 | sed 's/.*"\(.*\)".*/\1/')"
    if (( dry_run )); then
        if [[ "$cargo_ver" != "$plain" ]]; then
            echo ":: dry-run: would stamp $version (Cargo.toml is $cargo_ver)"
        fi
    elif [[ "$cargo_ver" != "$plain" ]]; then
        echo ":: Stamping $version across all sources (was $cargo_ver) ..."
        python3 "$REPO_ROOT/scripts/bump-version.py" "$version"
        if (( ! no_commit )); then
            git -C "$REPO_ROOT" add -A
            git -C "$REPO_ROOT" commit -m "chore(release): stamp $version"
            git -C "$REPO_ROOT" push origin main
            echo ":: Version bump committed and pushed."
        else
            echo ":: --no-commit: version stamped but not committed."
        fi
    fi

    # ── 3. Build ──────────────────────────────────────────────────────
    if (( ! publish_only )); then
        if (( dry_run )); then
            echo ":: dry-run: would build all legs this machine can"
        else
            echo ":: Building all legs this machine can ..."
            build_all || true   # manual legs are skipped, not fatal
        fi
    fi

    # ── 4. Package ────────────────────────────────────────────────────
    package

    # ── 5. Release notes ──────────────────────────────────────────────
    local notes="$DIST_DIR/RELEASE_NOTES.md"
    generate_notes "$version" > "$notes"

    # ── 6. Verify all artifacts present ───────────────────────────────
    local missing=() id f
    for id in $(target_ids); do
        f="$DIST_DIR/$(archive_name "$id")"
        [[ -f "$f" ]] || missing+=("$(archive_name "$id")")
    done
    if (( ${#missing[@]} > 0 )); then
        if (( allow_partial )); then
            echo "warning: publishing without: ${missing[*]}"
        else
            die "missing artifacts: ${missing[*]} — build them (build-one on the right machine) or pass --allow-partial"
        fi
    fi

    if (( dry_run )); then
        echo ""
        echo ":: DRY RUN — would publish $version to $REPO with:"
        ls -1 "$DIST_DIR"
        echo ""
        echo "Release notes preview:"
        cat "$notes"
        exit 0
    fi

    # ── 7. Publish ────────────────────────────────────────────────────
    command -v gh >/dev/null 2>&1 || die "gh CLI is required for releases."
    local files=() name
    for f in "$DIST_DIR"/*; do
        [[ -f "$f" ]] || continue
        name="$(basename "$f")"
        [[ "$name" == "RELEASE_NOTES.md" ]] && continue  # used as the body, not an asset
        files+=("$name")
    done
    echo ":: Creating GitHub release $version ..."
    (cd "$DIST_DIR" && gh release create "$version" --repo "$REPO" \
        --title "Clawde $version" --notes-file "$notes" \
        "${files[@]}")
    echo ":: Release $version published."

    # ── 8. Hand off npm publish (OIDC provenance needs Actions) ───────
    gh workflow run npm-publish.yml --repo "$REPO" --ref main \
        -f version="$version"
    echo ":: Dispatched npm-publish.yml for $version."
}

# ── Status / cleanup ────────────────────────────────────────────────────

show_targets() {
    echo "Installed rust targets:"
    rustup target list --installed 2>/dev/null | sed 's/^/  /'
    echo ""
    echo "Platform legs:"
    for id in $(target_ids); do
        printf "  %-16s %-35s %s\n" "$id" "$(target_info "$id")" "$(builder_for "$id")"
    done
    echo ""
    echo "Container builder: $LINUX_BUILD_IMAGE (glibc 2.36 floor for Linux legs)"
    echo "  docker: $(docker info >/dev/null 2>&1 && echo running || echo 'not running (required)')"
    echo "  zip:    $(command -v zip >/dev/null 2>&1 && echo ready || echo 'not installed (falls back to python3)')"
}

# ── Dispatch ────────────────────────────────────────────────────────────

cmd="${1:-install}"
shift || true

case "$cmd" in
    install|"")
        install_binary
        ;;
    debug)
        build_debug
        ;;
    run)
        run_debug "$@"
        ;;
    build-one)
        [[ $# -ge 1 ]] || die "build-one requires a target id (see 'targets')"
        build_one "$1"
        ;;
    build-all)
        build_all
        ;;
    package)
        package
        ;;
    release)
        release "$@"
        ;;
    windows|win)
        build_one windows-x86_64
        ;;
    linux-arm|arm|aarch64)
        build_one linux-aarch64
        ;;
    targets|status)
        show_targets
        ;;
    clean)
        (cd "$SRC_DIR" && cargo clean)
        rm -rf "$DIST_DIR"
        echo ":: Cleaned build artifacts"
        ;;
    *)
        echo "Unknown command: $cmd"
        echo "Usage: ./build.sh [install|debug|run|build-one|build-all|package|release|windows|linux-arm|targets|clean]"
        exit 1
        ;;
esac
