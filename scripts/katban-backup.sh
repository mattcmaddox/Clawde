#!/usr/bin/env bash
# katban-backup.sh — standalone solo backup for clawde (NOT wired into Katban).
#
# Standalone by design: the Katban merge path never calls this. You run it on
# demand, or hook it to a systemd timer / cron later if you want it automatic.
# It backs up the CURRENT repo's current branch to two off-project destinations,
# fast-forward only, local-first. See docs/plans/katban-solo-backup-scope.md and
# .agents/skills/katban-solo-backup/SKILL.md for the full decision record.
#
# Usage:
#   ./scripts/katban-backup.sh check            # show switch + destination state
#   ./scripts/katban-backup.sh on|off           # set the on/off switch
#   ./scripts/katban-backup.sh push             # back up current branch to both
#   ./scripts/katban-backup.sh setup-local      # create the bare repo on the drive
#   ./scripts/katban-backup.sh setup-github     # create the private GH backup repo
#
# Hard rules (do not soften):
#   1. Never git push --force to any backup — fast-forward only.
#   2. Never push to origin or any public repo — private code, private targets.
#   3. Local-first: if the drive is unmounted, skip it with a warning, still push GH.
#   4. A failed push never fails anything else; it logs and moves on.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Destinations (single source of truth) ───────────────────────────────
# Personal paths are NOT baked in. Set CLAWDE_BACKUP_DRIVE (the mounted
# external-drive folder) in your shell env or in ~/.config/clawde/backup.conf
# via the `drive=` key. Defaults are neutral.
DRIVE_DIR="${CLAWDE_BACKUP_DRIVE:-$HOME/clawde-backup}"
LOCAL_REPO="$DRIVE_DIR/clawde.git"                 # bare repo, local-first
GH_REPO="${CLAWDE_BACKUP_GH_REPO:-yourname/clawde-backup}"   # private, off-site
GH_REMOTE="github-backup"
GH_URL="git@github.com:$GH_REPO.git"

# Switch + destination config live in the user's config area (not in git history).
SWITCH_FILE="${CLAWDE_BACKUP_CONF:-$HOME/.config/clawde/backup.conf}"
ENABLED_KEY="enabled"

# Load optional `key=value` overrides (drive=, gh_repo=) from backup.conf.
if [[ -f "$SWITCH_FILE" ]]; then
    while IFS='=' read -r k v; do
        case "$k" in
            drive)    [[ -n "$v" ]] && DRIVE_DIR="$v" ;;
            gh_repo)  [[ -n "$v" ]] && GH_REPO="$v" ;;
            enabled)  : ;;  # handled by is_enabled()
        esac
    done < "$SWITCH_FILE"
    LOCAL_REPO="$DRIVE_DIR/clawde.git"
    GH_URL="git@github.com:$GH_REPO.git"
fi

die() { echo "error: $*" >&2; exit 1; }

is_enabled() {
    [[ -f "$SWITCH_FILE" ]] || return 0              # default ON once file exists? no:
    local val
    val="$(awk -F= -v k="$ENABLED_KEY" '$1==k{gsub(/ +/,"",$2); print $2}' "$SWITCH_FILE" 2>/dev/null || true)"
    [[ -z "$val" || "$val" == "1" || "$val" == "true" || "$val" == "yes" || "$val" == "on" ]]
}

set_switch() {  # $1 = 1|0
    mkdir -p "$(dirname "$SWITCH_FILE")"
    local body=""
    if [[ -f "$SWITCH_FILE" ]]; then
        body="$(grep -v "^$ENABLED_KEY=" "$SWITCH_FILE" 2>/dev/null || true)"
    fi
    printf '%s\n%s=%s\n' "$body" "$ENABLED_KEY" "$1" > "$SWITCH_FILE"
}

git() { command git -C "$REPO_ROOT" "$@"; }   # always operate on the repo being backed up

branch_name() {
    git symbolic-ref --short HEAD 2>/dev/null \
        || die "cannot determine current branch (detached HEAD?)"
}

drive_ready() {
    mountpoint -q "$DRIVE_DIR" 2>/dev/null && return 0
    # Fall back: accept a dir that exists and is a mount point of the drive path.
    if [[ -d "$DRIVE_DIR" ]]; then
        # A plain dir (not a dedicated mount) is still usable for a manual backup.
        return 0
    fi
    return 1
}

ensure_local() {
    drive_ready || die "local drive not mounted — cannot setup. $DRIVE_DIR missing."
    mkdir -p "$DRIVE_DIR"
    if [[ ! -d "$LOCAL_REPO" ]]; then
        echo ":: Creating bare backup repo at $LOCAL_REPO ..."
        git clone --bare "$REPO_ROOT" "$LOCAL_REPO"
    fi
}

ensure_gh_remote() {
    if ! git remote get-url "$GH_REMOTE" >/dev/null 2>&1; then
        echo ":: Adding remote $GH_REMOTE -> $GH_URL"
        git remote add "$GH_REMOTE" "$GH_URL"
    fi
    # Safety: never allow the backup remote to point anywhere public or origin.
    local url
    url="$(git remote get-url "$GH_REMOTE")"
    [[ "$url" == "$GH_URL" ]] || die "backup remote $GH_REMOTE does not match expected URL"
    if [[ "$url" == *"mattcmaddox/Clawde"* ]]; then
        die "$GH_REMOTE must never point at the public Clawde repo"
    fi
}

safe_push() {  # $1 = remote, $2 = branch — fast-forward only
    if ! git push "$1" "refs/heads/$2:refs/heads/$2" 2>&1; then
        # A fast-forward-only push already refuses non-FF; just surface the real error.
        echo "warning: push to $1 ($2) failed — see above. Backup incomplete for that target." >&2
    fi
}

cmd_check() {
    if is_enabled; then
        echo "switch       : ON"
    else
        echo "switch       : OFF"
    fi
    echo "config file  : $SWITCH_FILE"
    echo "branch       : $(branch_name)"
    if drive_ready && [[ -d "$LOCAL_REPO" ]]; then
        echo "local drive  : OK ($LOCAL_REPO)"
    else
        echo "local drive  : MISSING/unmounted ($LOCAL_REPO)"
    fi
    if git remote get-url "$GH_REMOTE" >/dev/null 2>&1; then
        echo "github backup: $GH_REMOTE -> $(git remote get-url "$GH_REMOTE")"
    else
        echo "github backup: remote '$GH_REMOTE' not configured"
    fi
}

cmd_push() {
    is_enabled || die "backup switch is OFF — run 'katban-backup.sh on' first (no data lost; this only prevents pushes)."
    local branch br
    branch="$(branch_name)"
    br="refs/heads/$branch"
    local any=0

    # 1. Local-first.
    if drive_ready; then
        if [[ -d "$LOCAL_REPO" ]]; then
            echo ":: Backing up $branch -> local ($LOCAL_REPO) ..."
            git push "$LOCAL_REPO" "$br:refs/heads/$branch" && any=1 || echo "warning: local backup failed" >&2
        else
            echo "warning: $LOCAL_REPO missing — run 'katban-backup.sh setup-local'" >&2
        fi
    else
        echo "warning: local drive unmounted — skipping local backup (still pushing GitHub)" >&2
    fi

    # 2. Private GitHub off-site copy.
    ensure_gh_remote
    echo ":: Backing up $branch -> private GH ($GH_REPO) ..."
    git push "$GH_REMOTE" "$br:refs/heads/$branch" && any=1 || echo "warning: github backup failed" >&2

    if (( any == 0 )); then
        echo "warning: no backup target succeeded" >&2
        exit 1
    fi
}

cmd_setup_local() {
    ensure_local
    echo ":: Local bare backup repo ready at $LOCAL_REPO"
}

cmd_setup_github() {
    command -v gh >/dev/null 2>&1 || die "gh CLI is required to create the private backup repo."
    gh auth status >/dev/null 2>&1 || die "gh is not authenticated."
    if gh repo view "$GH_REPO" >/dev/null 2>&1; then
        echo ":: Backup repo already exists: $GH_REPO"
    else
        echo ":: Creating PRIVATE backup repo $GH_REPO ..."
        gh repo create "$GH_REPO" --private || { echo "warning: failed to create $GH_REPO" >&2; exit 1; }
    fi
    ensure_gh_remote
    echo ":: GitHub backup remote ready."
}

# ── Dispatch ────────────────────────────────────────────────────────────
cmd="${1:-check}"; shift || true

case "$cmd" in
    check)     cmd_check ;;
    on)        set_switch 1; echo "backup switch: ON" ;;
    off)       set_switch 0; echo "backup switch: OFF" ;;
    push)      cmd_push ;;
    setup-local)   cmd_setup_local ;;
    setup-github)  cmd_setup_github ;;
    *)
        echo "Usage: ./scripts/katban-backup.sh [check|on|off|push|setup-local|setup-github]"
        exit 1
        ;;
esac