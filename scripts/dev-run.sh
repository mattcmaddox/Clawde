#!/usr/bin/env bash
# dev-run.sh — Build from source and run the latest code.
#
# Usage:
#   ./scripts/dev-run.sh            # build + launch interactive TUI
#   ./scripts/dev-run.sh -p "hello" # build + headless one-shot
#
# All arguments are forwarded to the clawde binary, so you can pass
# any flag the normal binary accepts.
#
# To use this as your everyday `clawde` command, add to ~/.bashrc:
#   alias clawde="~/clawde/scripts/dev-run.sh"
# or symlink into ~/.local/bin:
#   ln -sf ~/clawde/scripts/dev-run.sh ~/.local/bin/clawde

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BINARY="$REPO_ROOT/src-rust/target/debug/clawde"
SRC_DIR="$REPO_ROOT/src-rust"

# ---- Fast path: skip build if binary is already up to date ----
# Compares the binary's mtime against the newest source file.
needs_build() {
    [ ! -f "$BINARY" ] && return 0
    local newest_src
    newest_src=$(find "$SRC_DIR/crates" "$SRC_DIR/Cargo.toml" "$SRC_DIR/Cargo.lock" \
        -type f \( -name '*.rs' -o -name '*.toml' \) \
        -print0 2>/dev/null | xargs -0 ls -t 2>/dev/null | head -1)
    [ -z "$newest_src" ] && return 0
    [ "$newest_src" -nt "$BINARY" ]
}

if needs_build; then
    echo ":: Building clawde ..." >&2
    cd "$SRC_DIR" || { echo "Failed to enter $SRC_DIR" >&2; exit 1; }
    cargo build --package clawde-cli || {
        echo "Build failed — aborting." >&2
        exit 1
    }
    echo "" >&2
fi

# Run with all arguments passed through
exec "$BINARY" "$@"
