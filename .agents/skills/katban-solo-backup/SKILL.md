# Katban solo backup (manual)

## Purpose

Katban is single-user. Card work could be lost on a dead laptop or failed disk,
so the repo is backed up to two destinations **manually on demand** — there is
no scheduled/automatic backup. This skill is the operating procedure.

The full decision/scope record lives in
`docs/plans/katban-solo-backup-scope.md`.

**Standalone by design:** the backup flow is **separate from Katban** — nothing
inside Katban calls it. The single entry point is `scripts/katban-backup.sh`.

**When to use:** whenever you want a fresh backup (e.g. after merging card work),
when setting up the backup destinations, or when extending this flow. This skill
is NOT invoked by Katban's merge path and runs NO timer — backups are intentional.

## The two backup destinations

1. **Local external drive (always-first, local-first):** a single stable folder
   on the mounted external `BumbleBee` drive (see `drive=` in
   `~/.config/clawde/backup.conf`). All backup lives *under* this one tree, never
   as scattered siblings. Create it if missing (`mkdir -p`). The drive may be
   unmounted; handle that. Any per-project/per-date folders go *inside* it.
2. **Private GitHub backup repo (off-site):** a **private** repo owned by the
   account name in `gh_repo=` (`gh repo create <owner>/clawde-backup --private`).
   `gh` is already authenticated.

## Do this whenever you want a backup (manual)

Run the standalone command (fast-forward only, local-first, refuses when OFF):

```bash
./scripts/katban-backup.sh push
```

It backs up the current branch to **both** targets. Check state / toggle with:

```bash
./scripts/katban-backup.sh check     # switch + branch + destination health
./scripts/katban-backup.sh off       # disable pushes (nothing is lost)
./scripts/katban-backup.sh on        # re-enable
```

Destination paths are personal and are configured in the gitignored file
`~/.config/clawde/backup.conf` (`drive=`, `gh_repo=`), or via env vars
`CLAWDE_BACKUP_DRIVE` / `CLAWDE_BACKUP_GH_REPO`. They are never baked into the
checked-in script.

One-time setup (done once on the owner's machine):

```bash
./scripts/katban-backup.sh setup-local    # bare repo under the configured drive
./scripts/katban-backup.sh setup-github   # gh repo create .../clawde-backup --private
```

**`setup-github` runs `gh repo create <owner>/clawde-backup --private` — never
`--public`.** Creating a backup destination is one-time setup, not a backup push.

## Public / private (hard constraint)

**Never push private code to a public remote or branch.** The primary `origin`
remote (a public repo) is **off-limits** as a backup destination. All card work
is private, so backups go **only** to the private GitHub backup repo and the
local drive folder. Never configure `origin` (or a public repo) as a backup push
target.

## Hard rules

1. **Never `git push --force` to any backup.** Fast-forward pushes only. Force
   can silently destroy history on the remote.
2. **Never hold the board lock across a remote push.** A hung network push
   would block the whole board. Push in a background task / after the lock
   releases.
3. **A failed backup must never fail the merge or wedge the scheduler.**
   Log a warning (journal) and move on.
4. **Local-first.** The local drive is the first/always target; GitHub is the
   up-to-second off-site copy. If the drive is unmounted, skip it with a warning
   and still push GitHub.
5. **Backup is idempotent/additive** — it only adds commits that aren't there;
   it never rewrites a checkpoint.
6. **Never hardcode personal paths in `scripts/katban-backup.sh`.** The drive
   folder, GH owner, and mount point must come from
   `~/.config/clawde/backup.conf` or `CLAWDE_BACKUP_*` env vars.

## Anti-patterns

- Pushing to `origin` or any **public** repo (leaks private work).
- Baking `/mnt/...`, the drive folder, or a personal username into the checked-in
  script or docs.
- Force-pushing to a backup (destroys history).
- Backing up inside the board `BoardLock` scope (blocks all board writers on a
  slow/hung network call).
- Treating a failed backup as a card failure (loses the primary job — merging).
- Re-adding a systemd timer / cron entry: backups are **manual by decision**.
- Building PRs / team review / `gh pr create` — explicitly out of scope for a
  solo setup.

## Checklist

- [x] Backup is manual-only (no systemd timer, no cron)
- [x] Destinations configured via gitignored `~/.config/clawde/backup.conf`
- [x] Private GitHub backup repo exists; `github-backup` remote named
- [x] Standing command (`katban-backup.sh push`) pushes fast-forward to both targets
- [x] Backup flow is standalone — never runs inside a Katban board lock
- [x] Backup failures log and don't block any merge (merge path touches no backup code)
- [x] On/off config switch (`on`/`off`, default ON when file absent)
- [x] No personal machine paths in checked-in files