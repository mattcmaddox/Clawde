# Katban — solo backup (scoped)

Decision record for the Katban commit-flow follow-on. This **narrows** the
earlier "commit / PR + auto-commit chains" scope down to what a single user
actually needs. Status: **implemented as a standalone, manual command** —
`scripts/katban-backup.sh`. The Katban merge path is intentionally **separate**
from the backup flow: nothing in Katban calls this script, and there is **no
scheduled/automatic backup** — the owner runs it on demand. Destination paths
are personal and live in the gitignored `~/.config/clawde/backup.conf`, never
in checked-in files.

## The decision

Katban is used by **one person** (the owner) and will likely stay that way.
Therefore:

- **Skip the Pull-Request machinery.** PRs exist so other people can review and
  merge work. For a solo user they are pure overhead — opening a PR to yourself
  is busywork. We do not build `gh pr create` flows, review threads, or
  push-to-remote-for-collaboration.
- **Backup instead (manual).** The real risk solo is *losing work*. The
  owner keeps two off-project copies and refreshes them **by hand** whenever
  they want — no automation, no timers. Nothing forces extra pushes.

The commit-flow core that already exists stays as-is:
- Runner pins a successful card's work to `katban/<id>` and records the commit
  in `card.commit` (Option B — "pin the commit").
- Review = merge-or-discard; merging marks the card Done.
- Dependents auto-start once a card is Done (the auto-chain already works).

## The two backup destinations (manual)

1. **Local external drive (always-first, local-first):** a single stable folder
   on the mounted external `BumbleBee` drive (`drive=` in
   `~/.config/clawde/backup.conf`). All backup lives *under* this one tree, never
   as scattered siblings. Purpose: a local, always-available second copy (works
   even when offline).
2. **Private GitHub backup repo (off-site):** a **private** repo under the
   account in `gh_repo=` → typically `gh repo create <owner>/clawde-backup
   --private`. Purpose: survives a dead laptop / lost machine — one full
   off-site copy. `gh` is already authenticated.

Mechanics: the project's checked-out branch is pushed to the backup remote as
a **fast-forward push (no force)**.

## Public / private (hard constraint)

**Private code must never be pushed to a public remote or branch.** The primary
`origin` remote (a public repo) is **off-limits** as a backup destination. All
card work is private to the owner, so backups go **only** to the private GitHub
backup repo and the local drive folder. Never configure `origin` (or any public
repo) as a backup push target.

## Safety rules (hard constraints)

1. **Never force-push to a backup.** `git push --force` can silently destroy
   history on the remote. Backups push fast-forward commits only.
2. **Backup is idempotent and additive.** Pushing a repo state never rewrites a
   checkpoint; it only adds what isn't there.
3. **No data loss on board writes.** The board's write path is already
   atomic + `BoardLock`-guarded; the backup flow must never sit inside the
   board lock while talking to a remote (a slow/hung network push would block
   the board). Push *after* releasing the lock / in a background task.
4. **If a backup fails, the board still works.** A failed push logs a warning
   (journal) but must never fail the merge or wedge the scheduler.
5. **Local-first.** The local drive is the first/always target; GitHub is the
   up-to-second off-site copy. If the drive is unmounted, skip it (warn) and
   still push GitHub.
6. **No personal paths in the repo.** Drive folder, GH owner, mount point are
   configured in the gitignored `~/.config/clawde/backup.conf` (`drive=`,
   `gh_repo=`) or `CLAWDE_BACKUP_DRIVE` / `CLAWDE_BACKUP_GH_REPO` env vars.

## Implemented (standalone, manual)

`scripts/katban-backup.sh` — **not wired into Katban**, **no timer**. Real
destination paths are read from `~/.config/clawde/backup.conf` (gitignored).
Subcommands:

- `check` — show the on/off switch, current branch, and destination state.
- `on` / `off` — set the switch (persisted in `~/.config/clawde/backup.conf`;
  default ON when the file is absent).
- `push` — back up the current branch to **both** targets, fast-forward only,
  local-first (drive unmounted → warn + still push GH). Refuses when OFF.
- `setup-local` — creates the bare repo under the configured drive folder.
- `setup-github` — creates the **private** backup repo and adds the
  `github-backup` remote (never pointed at origin/public).

The owner's machine has `drive=/mnt/.../clawde`, `gh_repo=<owner>/clawde-backup`
in `~/.config/clawde/backup.conf`, both targets live and `main` pushed to both.

## Non-goals (explicitly out)

- Automated/scheduled backup (systemd timer or cron) — **removed by decision**;
  the owner refreshes manually.
- Team PRs, review comments, `gh pr create`, CI on the board, multi-user auth.
- Real-time collaboration. Nothing here assumes anyone but the owner.