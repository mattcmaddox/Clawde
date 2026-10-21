#!/usr/bin/env bash
# End-to-end proof that a turn sent in IMAGE mode carries its image block.
#
# Runs the REAL clawde TUI in tmux against the local mock Gemini upstream in
# `image-mode-mock.py`, with one fabricated google key in a scratch auth store.
# The probe drives the actual key sequence a user does:
#
#   Tab, Tab  → BUILD → PLAN → IMAGE (the cycle picks the vision model)
#   @<png> …  → the `@path` auto-attach that works without a clipboard tool
#   Enter     → the turn
#
# and passes only when:
#
#   1. the status line reports IMAGE and the vision model, i.e. the mode really
#      switched and picked the synthetic `google/<model>` pin;
#   2. the turn completes and the mock's reply renders in the pane;
#   3. the mock received exactly one chat request, streaming, whose body carried
#      one `inlineData` part;
#   4. that part's bytes are byte-identical to the fixture on disk (SHA-256) and
#      its `mimeType` matches the sniffed file type — not a truncated payload, a
#      placeholder, or the wrong media type;
#   5. the same request still carries the typed text, so image and prompt are
#      one turn rather than two.
#
# Why a probe: the image path crosses four layers no Rust test drives together —
# the TUI's mode cycle (model swap), the CLI turn loop (attachment assembly), the
# free provider's vision gate, and Google's native request shaping. The Rust
# tests cover each in isolation; only the real binary proves the seam, and only a
# real socket proves the bytes on the wire.
#
# Google is the only vision-capable free-catalog upstream a mock can stand in
# for: `github-copilot` fixes its endpoint inside the provider, while Google's
# base URL comes from `GoogleProvider::with_base_url`, reached through the
# dev-only `CLAWDE_FREE_BASE_URL_GOOGLE` hook (debug builds only, loopback only).
#
# Everything runs against a scratch CLAWDE_HOME and HOME, so the developer's
# config, sessions, auth store and free-state caches are never read or written.
# The google free-state caches are pre-seeded inside that home, and the two
# startup calls that live outside the free chain (the GitHub update check and
# the models.dev registry refresh) are silenced, so the probe makes no network
# call at all — verified with `strace -f -e trace=connect`: the only socket it
# opens is the loopback mock.
#
# Usage: bash scripts/probes/image-mode-probe.sh [path-to-binary]

set -uo pipefail

BIN="${1:-src-rust/target/debug/clawde}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
MOCK="$REPO_ROOT/scripts/probes/image-mode-mock.py"

PORT="${PORT:-18120}"
SESSION="clawde-image-mode-probe"
MODEL="gemini-2.5-flash"
VISION_PIN="google/${MODEL}"
PRE_MODE_LABEL="auto · free"
PROMPT_MARKER="IMGPROBE"
REPLY_MARKER="IMAGEOK"

fail() { echo "FAIL: $*" >&2; exit 1; }
note() { echo "  $*"; }

[ -x "$BIN" ] || fail "no binary at $BIN (run: cargo build)"
[ -f "$MOCK" ] || fail "no mock at $MOCK"
command -v tmux >/dev/null || fail "tmux not installed"

# /tmp explicitly: TMPDIR on a dev box can point into a cache directory under
# $HOME, which the probe's HOME redirect is supposed to keep out of reach.
SCRATCH="$(mktemp -d /tmp/clawde-image-mode.XXXXXX)"
# A short path keeps the typed prompt well under the 150-char / 3-line paste
# threshold, so it is inserted inline instead of becoming a `[Pasted text #N]`
# chip (which would hide the `@` reference from a reader of the pane).
PNG="$SCRATCH/i.png"
LOG="$SCRATCH/requests.jsonl"

cleanup() {
  tmux kill-session -t "$SESSION" 2>/dev/null
  [ -n "${MOCK_PID:-}" ] && kill "$MOCK_PID" 2>/dev/null
  wait "${MOCK_PID:-}" 2>/dev/null
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Fixture + credentials + config
# ---------------------------------------------------------------------------

python3 - "$SCRATCH" "$PNG" "$MODEL" <<'PY'
import json, pathlib, struct, sys, time, zlib

scratch, png_path, model = sys.argv[1:4]
home = pathlib.Path(scratch)

# A real, decodable 1x1 RGB PNG (built here rather than checked in as a binary):
# the client sniffs the media type from the magic bytes and base64-encodes the
# bytes verbatim, so the probe can compare the wire payload against this file.
def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )

ihdr = struct.pack(">IIBBBBB", 1, 1, 8, 2, 0, 0, 0)
idat = zlib.compress(b"\x00\xff\x00\x00")
png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", ihdr)
    + chunk(b"IDAT", idat)
    + chunk(b"IEND", b"")
)
pathlib.Path(png_path).write_bytes(png)

# Multi-key store is the only credential source the free chain reads. One key,
# so the single-key path builds GoogleProvider (the arm `google_provider` wraps).
(home / "auth.json").write_text(
    json.dumps(
        {"credentials": {}, "keys": {"google": ["AIzaFAKEIMAGEPROBE000000000000000001"]}},
        indent=2,
    )
)

# A fresh home would otherwise stop on the first-launch flow, and importing the
# developer's real sessions would make the probe read state it must not touch.
(home / "settings.json").write_text(
    json.dumps(
        {
            "config": {"importExternalSessionsOnStart": False},
            "hasCompletedOnboarding": True,
        },
        indent=2,
    )
)

# Pre-seed both free-state caches so startup resolves the vision pin from disk
# instead of calling models.dev / the real /v1beta/models. Without this the probe
# would depend on the network and on whatever model models.dev currently picks.
state = home / "free-state"
state.mkdir(exist_ok=True)
now = int(time.time())
(state / "modelsdev-defaults.json").write_text(
    json.dumps({"saved_at_unix": now, "defaults": {"google": model}}, indent=2)
)
(state / "live-discovery.json").write_text(
    json.dumps({"saved_at_unix": now, "models": {"google": [model]}}, indent=2)
)

# Two startup calls run outside the free chain and would still reach the
# internet: the GitHub update check (24h disk cache under
# `$HOME/.cache/clawde/update_check.txt`) and the models.dev registry refresh
# (`CLAWDE_DISABLE_MODELS_FETCH`). Both are irrelevant to this seam, so the
# probe silences them and stays fully offline.
update_cache = home / ".cache" / "clawde"
update_cache.mkdir(parents=True, exist_ok=True)
# An empty cached version means "no newer release", served without a request.
(update_cache / "update_check.txt").write_text("")
PY

[ -s "$PNG" ] || fail "fixture PNG was not written"

# ---------------------------------------------------------------------------
# Mock upstream
# ---------------------------------------------------------------------------

python3 "$MOCK" \
  --port "$PORT" \
  --model "$MODEL" \
  --reply "$REPLY_MARKER-STREAM" \
  --log "$LOG" \
  --ready-file "$SCRATCH/mock.ready" >"$SCRATCH/mock.out" 2>&1 &
MOCK_PID=$!

for _ in $(seq 1 50); do
  [ -f "$SCRATCH/mock.ready" ] && break
  sleep 0.1
done
[ -f "$SCRATCH/mock.ready" ] || fail "mock never came up (see $SCRATCH/mock.out)"
note "mock vision upstream listening on 127.0.0.1:$PORT"

# ---------------------------------------------------------------------------
# Launcher: minimal env so no ambient provider key can join the chain, HOME
# redirected, and the google chat endpoint pointed at the mock.
# ---------------------------------------------------------------------------

cat >"$SCRATCH/launch.sh" <<EOF
#!/usr/bin/env bash
exec env -i \\
  PATH="$PATH" \\
  TERM="screen-256color" \\
  HOME="$SCRATCH" \\
  CLAWDE_HOME="$SCRATCH" \\
  "CLAWDE_FREE_BASE_URL_GOOGLE=http://127.0.0.1:$PORT" \\
  CLAWDE_DISABLE_MODELS_FETCH=1 \\
  "$BIN" -m free/auto "\$@"
EOF
chmod +x "$SCRATCH/launch.sh"

# ---------------------------------------------------------------------------
# Drive the TUI
# ---------------------------------------------------------------------------

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 110 -y 34
tmux send-keys -t "$SESSION" "$SCRATCH/launch.sh" C-m

pane() { tmux capture-pane -t "$SESSION" -p 2>/dev/null; }

# Wait for the input prompt box, not the banner: keys sent before it exists are
# dropped, and a dropped Tab looks exactly like a mode cycle that did not fire.
for _ in $(seq 1 80); do
  pane | grep -q '❯' && break
  sleep 0.5
done
sleep 2

# Tab is a chord-prefix key: the cycle fires when the chord window expires, so
# the two presses are separated by more than that window.
tmux send-keys -t "$SESSION" Tab
sleep 3
tmux send-keys -t "$SESSION" Tab
sleep 3

MODE_PANE="$(pane)"
grep -q 'IMAGE' <<<"$MODE_PANE" \
  || { echo "$MODE_PANE"; fail "Tab did not reach IMAGE mode"; }
# The swap is deferred to the first image (or the submit), so merely entering
# the mode must leave the previous model in place — otherwise tab-cycling past
# IMAGE would strand the session on a model the user never chose.
if grep -q "$MODEL" <<<"$MODE_PANE"; then
  echo "$MODE_PANE"
  fail "entering IMAGE swapped the model before any image arrived"
fi
grep -q "$PRE_MODE_LABEL" <<<"$MODE_PANE" \
  || { echo "$MODE_PANE"; fail "the status row lost the pre-mode model label"; }
note "mode reached IMAGE on $PRE_MODE_LABEL; the swap waits for an image or submit"

TYPED="@$PNG $PROMPT_MARKER describe this image"
# Text and the submit key MUST be separate invocations: bracketed paste turns a
# CR sent alongside the text into a literal newline (see AGENTS.md).
tmux send-keys -t "$SESSION" "$TYPED"
sleep 4
tmux send-keys -t "$SESSION" C-m

# Poll for the mock's reply instead of sleeping a fixed amount. If it has not
# appeared after 10s, re-send the submit key: a C-m that lands while the app is
# still warming up is silently dropped, and re-sending is harmless once the turn
# has started. A slow CI runner is exactly where that happens.
for _ in $(seq 1 3); do
  for _ in $(seq 1 10); do
    sleep 1
    pane | grep -q "$REPLY_MARKER" && break 2
  done
  tmux send-keys -t "$SESSION" C-m
done
TURN_PANE="$(pane)"
echo
echo "=== pane ==="
printf '%s\n' "$TURN_PANE"
echo "=== request log ==="
cat "$LOG" 2>/dev/null || fail "no request log — the turn never reached the mock"
echo

grep -q "$REPLY_MARKER" <<<"$TURN_PANE" \
  || fail "the reply never reached the pane: the turn did not complete"

# The deferred swap lands at submit, so the status row now names the vision
# model even though entering the mode did not.
grep -q "$MODEL" <<<"$TURN_PANE" \
  || fail "the submit did not switch the session onto $VISION_PIN"

# `@image.png` is attached as an image block, never text-injected, so the
# file-injection dialog must not appear for it: its "inject anyways" option only
# relaxes the size limit, so a binary image left in the oversized list re-raises
# the same dialog and the turn never starts.
if grep -q 'File Injection Warning' <<<"$TURN_PANE"; then
  fail "the file-injection dialog fired for an image reference"
fi

# ---------------------------------------------------------------------------
# Assertions
# ---------------------------------------------------------------------------

python3 - "$LOG" "$PNG" "$MODEL" "$PROMPT_MARKER" <<'PY' || exit 1
import hashlib, json, pathlib, sys

log_path, png_path, model, prompt_marker = sys.argv[1:5]

rows = [json.loads(line) for line in open(log_path) if line.strip()]
posts = [r for r in rows if r.get("method") == "POST"]


def die(msg):
    print(f"FAIL: {msg}", file=sys.stderr)
    sys.exit(1)


if not posts:
    die("the mock never received a chat request")

if len(posts) != 1:
    for row in posts:
        print(json.dumps(row), file=sys.stderr)
    die(f"expected exactly one chat request, saw {len(posts)}")

req = posts[0]
if req.get("status") != 200 or not req.get("stream"):
    die(f"expected one streamed chat request, got {json.dumps(req)}")

if req.get("model") != model:
    die(f"request model {req.get('model')!r} is not the image-mode vision model {model!r}")

count = req.get("image_count")
if count != 1:
    die(f"request carried {count} image part(s), expected exactly 1")

fixture = pathlib.Path(png_path).read_bytes()
want = hashlib.sha256(fixture).hexdigest()
got = (req.get("image_sha256") or [None])[0]
if got != want:
    die(f"wire image {got} is not the attached image {want}")

mime = (req.get("image_mime") or [None])[0]
if mime != "image/png":
    die(f"wire media type {mime!r} is not the sniffed image/png")

if prompt_marker not in (req.get("text") or ""):
    die("the image request lost the typed prompt text — image and prompt split")

print(
    "  ok: one streamed request to "
    f"{model} carried inlineData {want[:12]}… ({mime}) with the prompt text"
)
PY

note "image mode end to end: the vision upstream received the image block"
