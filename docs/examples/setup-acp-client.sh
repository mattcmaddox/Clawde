#!/usr/bin/env bash
# ===========================================================================
# setup-acp-client.sh — Provision a LAN machine with the ACP client
# ===========================================================================
# Run this FROM the ACP server machine to set up any client on your LAN.
#
# Usage:
#   ./setup-acp-client.sh <target-host> [ssh-user]
#
# Examples:
#   ./setup-acp-client.sh 192.168.1.40
#   ./setup-acp-client.sh theworker.local user
#
# What it does:
#   1. Copies the ACP client script to the target machine
#   2. Copies the server TLS certificate (for verified connections)
#   3. Installs the script to ~/.local/bin/acp
#   4. Adds a shell alias `acp-clawde` for quick access
#   5. Tests the connection (initialize + session/new)
# ===========================================================================

set -euo pipefail

# ---- Config ----------------------------------------------------------------
SCRIPT_SRC="$(dirname "$0")/acp-client.py"
CERT_SRC="$HOME/.clawde/certs/server.crt"
SERVER_HOST=""  # filled from the ACP server's own hostname detection

# ---- Argument parsing ------------------------------------------------------
TARGET="${1:?Usage: setup-acp-client.sh <target-host> [ssh-user]}"
SSH_USER="${2:-$USER}"

if [ ! -f "$SCRIPT_SRC" ]; then
    echo "Error: acp-client.py not found at $SCRIPT_SRC"
    echo "Expected to be in the same directory as this script (docs/examples/)."
    exit 1
fi

if [ ! -f "$CERT_SRC" ]; then
    echo "Warning: Server cert not found at $CERT_SRC"
    echo "TLS verification will not be available on the client."
    echo "Install the cert first: mkdir -p ~/.clawde/certs && ..."
    CERT_SRC=""
fi

# Detect our own IP (the ACP server address clients should connect to)
SERVER_HOST=$(hostname -I 2>/dev/null | awk '{print $1}')
if [ -z "$SERVER_HOST" ]; then
    SERVER_HOST=$(ip -4 addr show 2>/dev/null | grep 'inet ' | awk '{print $2}' | cut -d/ -f1 | grep -v '127.0.0.1' | head -1)
fi
if [ -z "$SERVER_HOST" ]; then
    echo "Warning: Could not detect server IP. Edit SERVER_HOST in the alias after install."
    SERVER_HOST="YOUR_SERVER_IP"
fi

echo "=== Setup ACP Client ==="
echo "  Target:    $SSH_USER@$TARGET"
echo "  Server:    $SERVER_HOST"
echo "  Script:    $SCRIPT_SRC"
echo "  Cert:      ${CERT_SRC:-"(none)"}"
echo ""

# ---- Step 1: Copy files to target -----------------------------------------
echo "[1/4] Copying ACP client script..."
scp "$SCRIPT_SRC" "$SSH_USER@$TARGET:/tmp/acp-client.py"

CERT_REMOTE=""
if [ -n "$CERT_SRC" ]; then
    echo "[2/4] Copying TLS certificate..."
    scp "$CERT_SRC" "$SSH_USER@$TARGET:/tmp/server.crt"
    CERT_REMOTE="/tmp/server.crt"
fi

# ---- Step 3: Install on target via SSH ------------------------------------
echo "[3/4] Installing on $TARGET..."

SSH_CMD=$(cat << SSHSCRIPT
set -e

# Install the client script
mkdir -p ~/.local/bin
cp /tmp/acp-client.py ~/.local/bin/acp
chmod +x ~/.local/bin/acp
echo "  Installed ~/.local/bin/acp"

# Install the TLS cert
if [ -f /tmp/server.crt ]; then
    mkdir -p ~/.local/share
    cp /tmp/server.crt ~/.local/share/clawde-cert.crt
    rm -f /tmp/server.crt
    echo "  Installed ~/.local/share/clawde-cert.crt"
fi

# Add shell alias (skip if already present)
ALIAS_LINE="alias acp-clawde='acp $SERVER_HOST --cert ~/.local/share/clawde-cert.crt'"
if grep -q "alias acp-clawde=" ~/.bash_aliases 2>/dev/null || grep -q "alias acp-clawde=" ~/.bashrc 2>/dev/null; then
    echo "  Shell alias 'acp-clawde' already exists, skipping"
else
    if [ -f ~/.bash_aliases ]; then
        echo "$ALIAS_LINE" >> ~/.bash_aliases
    else
        echo "$ALIAS_LINE" >> ~/.bashrc
    fi
    echo "  Added shell alias: acp-clawde"
fi

# Clean up temp files
rm -f /tmp/acp-client.py

echo ""
echo "=== Install complete on \$(hostname) ==="
SSHSCRIPT
)

ssh "$SSH_USER@$TARGET" "bash -s" <<< "$SSH_CMD"

# ---- Step 4: Test the connection -------------------------------------------
echo ""
echo "[4/4] Testing connection to ACP server $SERVER_HOST..."

if [ -n "$CERT_REMOTE" ]; then
    TEST_OUTPUT=$(ssh "$SSH_USER@$TARGET" "~/.local/bin/acp $SERVER_HOST --cert ~/.local/share/clawde-cert.crt" 2>&1) || true
else
    TEST_OUTPUT=$(ssh "$SSH_USER@$TARGET" "~/.local/bin/acp $SERVER_HOST --no-tls" 2>&1) || true
fi

if echo "$TEST_OUTPUT" | grep -qi "Connected"; then
    echo "  $TEST_OUTPUT"
    echo "  Connection: OK"
else
    echo "  Connection test output: $TEST_OUTPUT"
    echo "  Note: Test may have timed out (LLM response can be slow)."
    echo "  The install was successful even if the test timed out."
fi

echo ""
echo "=== Setup Complete ==="
echo ""
echo "On $TARGET, you can now run:"
echo ""
if [ -n "$CERT_REMOTE" ]; then
    echo "  acp $SERVER_HOST \"your prompt\" --cert ~/.local/share/clawde-cert.crt"
    echo "  acp-clawde \"your prompt\"                          (alias)"
    echo "  acp-clawde --interactive                           (REPL mode)"
else
    echo "  acp $SERVER_HOST \"your prompt\" --no-tls"
    echo "  acp $SERVER_HOST --interactive --no-tls"
fi
echo ""
echo "Or add 'export PATH=\$PATH:\$HOME/.local/bin' to ~/.bashrc if needed."
