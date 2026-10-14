# Katban

Katban is Clawde's real development feature: a Kanban-style control plane for
agent work. It is separate from Cat Chat, which is only the public-facing
sandbox chat.

## Scope

Katban owns:

- boards and cards, statuses, dependencies, and readiness;
- project-to-git-repository registration;
- isolated worktrees for card execution;
- headless Clawde agent runs and transient retries;
- review, verification, feedback, merge, and archive flows; and
- hosted development sites with loopback serving, live reload, and Caddy
  exposure.

Katban does **not** own guest links, guest sessions, Cat Chat passwords, or
public friend chat. Those belong to [Cat Chat](catchat.md).

## TUI: `/katban` and `Alt+G`

Use `/katban` for the development command surface:

```text
/katban
/katban status
/katban board list [--project NAME]
/katban board ready [--project NAME]
/katban board card add <PROMPT> [--project NAME]
/katban board card set <ID> <STATUS> [--project NAME]
/katban board card edit <ID> <PROMPT> [--project NAME]
/katban board card merge <ID> [--project NAME]
/katban board card remove <ID> [--project NAME]
/katban board link <A> <B> [--project NAME]
/katban board unlink <A> <B> [--project NAME]
/katban board auto-review on|off [--project NAME]
/katban board verify on|off [--project NAME]
/katban board attempts <N> [--project NAME]
/katban board attempts-upstreams [ID...] [--project NAME]
/katban project list
/katban site list
/katban board password <PASSWORD>
/katban board unblock <BUCKET>
```

`board unblock` takes the bucket `status`/Alt+G names: an address for a remote
admin, or `loopback` for one on the host itself (a request with no
`X-Forwarded-For`). The address spellings resolve onto it, so
`/katban board unblock 127.0.0.1` clears a local lockout.

`Alt+G` opens the Katban controls menu, built live from the board and admin
stores. It has six sections: **Status** (`/katban status`), **Boards** (list,
ready, add, link, and one advance / merge / review-feedback / inspect row per
open card), **Board settings** (the verification gate, auto-review, attempts and
its pinned upstreams, and the card runtime — each row states the value the board
holds now and runs the command that flips it), **Admin** (set or rotate the
dashboard password), **Locked buckets** (one unblock row per live lockout,
named as `status` names it), and **Projects & sites** (`project list`,
`site list`). Card editing that needs room to read what it will do — `card edit`,
`card remove`, `card comment`, `card scope` — stays on `/katban` and the web
board. The menu intentionally contains no Cat Chat link or guest-lockout rows;
`/chat` is the separate popup for that.

The menu's keys resolve through the `KatbanControls` keybinding context, so
they are listed in `/keybindings` and rebindable. Defaults: `↑`/`↓` (and `j`/`k`)
move, `PgUp`/`PgDn` scroll, `Home`/`End` jump to the first/last row, `Enter`
runs the highlighted row, `Esc`/`q` close. `Enter` writes the row's command
into the prompt and closes the menu, so it is visible before it runs: a complete
row (a toggle's target state, a card id, a bucket name) is ready as-is, while a
row that still needs input (a card prompt, an attempt count) is a prefix to
finish. The command itself runs on the next `Enter` — the input loop submits
only when no modal is open, and the menu is a modal.

## Board workflow

A practical development flow is:

```bash
clawde katban board init --project my-app
clawde katban project set my-app /path/to/my-app
clawde katban board card add "Add a health endpoint" --project my-app
clawde katban board card list --project my-app
clawde katban board run --project my-app
```

Cards are dependency-aware and respect the board's parallel cap. The runner
creates a project worktree, runs a headless Clawde process, captures the result
and diff, and moves successful work to review. Verification and review can
keep a card from being merged until the owner is satisfied.

### Attempts: N (best-of-N ladder)

By default every card runs once (`attempts 1`). For harder boards you can run
each card up to N times (1-5) on different free-catalog model families and let
the verification gate pick the winner — the first attempt whose tree passes
the project's checks is promoted to review, and the remaining rungs never run:

```bash
clawde katban board attempts 2 --project my-app
clawde katban board attempts-upstreams nvidia zai --project my-app   # empty = auto
```

- **Rotation**: with an explicit upstream list, rungs cycle through it.
  Empty (`auto`) derives rungs from the keyed free catalog, distinct model
  families first, then distinct hosts of the same family — serving stacks
  differ measurably even for the same model.
- **The lead rung must be able to finish a card**: a constrained free tier
  (`FreeUpstream::quick_task_lane` — poolside's preview, groq's token cap)
  fills a rung only after ordinary upstreams, because a card is a long
  multi-turn run and starting on a tier that throttles mid-run spends the
  first rung discovering that. Constrained lanes are demoted, never dropped:
  with only constrained keys the ladder still runs them, and an explicit
  `attempts-upstreams` list still wins verbatim. Upstreams the chain recently
  put into cooldown are ranked last of all.
- **Selection is never the agent's opinion**: the verify gate decides. A rung
  whose served-upstream attribution doesn't match its pin never wins (a pin
  that falls through is excluded even if its checks pass), and an empty
  completion (no text, no tool calls, no tokens — a thinking-only provider
  flake) never reaches the gate.
- **Honest slot accounting**: a card holds one `parallel_cap` slot for its
  whole ladder, so `attempts 3` never means 3 concurrent agents. Quota burn
  multiplies by N — the web board meta line shows the multiplier.
- **All rungs recorded**: every card carries the per-attempt matrix (upstream,
  verify result, elapsed, error) in `card show`, the web UI, and its
  `result` line. With `board verify off` the ladder degrades to
  first-non-empty-completion-wins (no selector exists).
- Rate-limited rungs get one bounded retry before the ladder moves on; a
  fully-failed ladder fails the card with the matrix in the result, and
  `auto_retry` applies as usual from there.

### Container tier (`board runtime docker`)

By default attempts execute on the host: the agent runs in the card's
worktree and safety comes from the worktree lane. Boards that want a hard
boundary can opt into the container tier:

    clawde katban board runtime docker --project my-app   # default: host

With `docker`, every attempt runs inside an ephemeral Docker container
(`ubuntu:24.04`, torn down no matter how the attempt ends): the worktree is
pushed in, the agent runs with `bypass-permissions` (the container IS the
safety boundary), the verify gate runs in-container, and a sha256 filesystem
manifest before/after the run captures everything the agent actually touched
— including untracked and gitignored files the git diff cannot see. The lane
was first built on Incus and moved to Docker on 2026-10-13 so its plumbing
could run in CI: GitHub runners ship Docker, not Incus.

- **Scope gate**: give a card an allowlist with
  `clawde katban board card scope <ID> src/ stats.py` (empty = no scope
  opinion). Any manifest-diff path outside the allowlist is a scope
  violation and the rung cannot win — an out-of-scope edit is exactly the
  blast-radius signal the host tier's git diff misses.
- **Dependency provisioning is setup, not card state**: every attempt
  installs a base set — `ca-certificates` and `git`, which Docker's stock
  `ubuntu:24.04` lacks (the Incus cloud image this lane was proven on had
  both) — plus whichever shared libs `ldd` reports missing for the pushed
  binary. A missing trust store is not a shared library, so it is asserted
  afterwards rather than left to `ldd`: without it the binary panics on its
  first HTTPS client, which is the first Docker card run's actual failure. A
  failed install or a missing CA bundle excludes the rung as an environment
  error instead of failing the card (mirrors the verify gate's
  install-failure skip semantics).
- **Artifacts**: the attempt's final tree is pulled as a tar under
  `~/.clawde/katban/container-artifacts/<card>/` for post-mortem review.
- **Requirements**: the `docker` CLI must be installed with a reachable
  daemon (`docker info`); `board runtime docker` refuses to switch otherwise.
  The per-attempt cost is real (container launch + push + apt), so the host
  tier remains the default.

Spec: [plans/katban-attempts-n-spec.md](plans/katban-attempts-n-spec.md)
(motivated by the measured best-of-N eval:
[plans/best-of-n-eval-spec.md](plans/best-of-n-eval-spec.md)).

The board web UI is loopback-first:

```bash
clawde katban board serve
clawde katban board expose --subdomain board.example.com --run my-app
```

Non-loopback binding requires `--allow-non-loopback`. Exposing the board should
be done behind HTTPS and only after configuring its admin password.

## Katban admin password

Katban uses a different policy from Cat Chat because it protects development
writes and agent execution. Set it from the shell or TUI:

```bash
clawde katban board password 'a long admin password!'
# or inside Clawde:
/katban board password a long admin password!
```

The admin credential is salted and hashed in the Katban store. The current
policy requires at least 16 characters, rejects all-digit and repeated-character
values, and requires punctuation or spaces. Password rotation clears the
Katban admin failed-attempt counters. This policy is deliberately not reused
by Cat Chat, and a Cat Chat link password cannot log into the board.

Admin sessions are separate, httpOnly, SameSite cookies with a rolling lifetime.
Writes on an exposed board require an admin session; loopback reads are useful
for local inspection but do not turn Cat Chat into an admin surface.

Wrong admin passwords use an independent policy: five failures make a
short-lived lockout, three lockout strikes make a 24-hour block, and
`/katban board unblock <BUCKET>` clears it — an address, or `loopback` for an
admin on the host itself (see the TUI command list above).

## Verifying the board UI

The board web app is one embedded HTML+JS document with no frontend build step,
so `cargo test` cannot exercise it. `scripts/probes/katban-board-ux-probe.py`
drives a real browser over the whole signed-out to signed-in to signed-out
journey: login gating, session restore across a reload, creating a board and
cards, editing, search/sort/hide-done, the dependency picker, and sign-out.

```bash
cargo build
scripts/probes/browser-probes.sh board
```

It also covers the two paths that only show up outside the happy flow: a
refused write (session dropped out from under a loaded page) must report in the
page's notice area and keep the unsaved draft, and an exposed board's reads are
replayed with the `X-Forwarded-*` headers caddy adds — refused without a
session, `Secure` on the session cookie, allowed with it — plus the sign-in gate
the page renders to a signed-out visitor. The proxy leg itself stays in the Rust
tests (`exposed_reads_require_auth_but_loopback_stays_open`).

Cat Chat's pages have the same gap and their own probe — see
[catchat.md](catchat.md#verifying-the-guest-journey).

It needs the `playwright` Python package, which
`scripts/probes/browser-probes.sh setup` installs into a venv outside the repo
— a bare `pip install playwright` is refused where the distro python is
externally managed (PEP 668). The wrapper then runs the probe with a system
chromium when one is installed, so playwright's own browser download is only
needed where none exists. The probe exits 2, not 0, when a prerequisite is
missing, so a wiring that treats non-zero as failure cannot read a skip as a
pass. CI (the Linux leg of
`.github/workflows/ci.yml`) installs playwright into a throwaway venv and passes
`--require-browser`, which turns that skip into a failure — a browser that
failed to install breaks the run instead of reporting green. It runs against a
scratch `CLAWDE_HOME` and deliberately does not pass `board serve --run`, so it
never dispatches an agent run at the cards it creates.

## Verifying the container tier

`board runtime docker` moves a card's `attempts: N` ladder into ephemeral
Docker containers. The full card path cannot be a `cargo test` — it needs a
reachable dockerd, a base image, network, and live free-tier keys — so the happy
path is a dev-machine probe, manual-only, like the best-of-N eval harness. The
runtime plumbing is the part CI can run, because GitHub runners ship Docker;
that is why the tier was moved off Incus.

What the suite does cover: `restore_task_dir` against real `tar`, the rung
selection rules (`classify_container_rung`), and the write-back chain
(`finalize_pins_a_commit_from_a_restored_container_tree`). The rest is
`scripts/probes/katban-docker-card-probe.py`:

```bash
cargo build
python3 scripts/probes/katban-docker-card-probe.py              # all three phases
python3 scripts/probes/katban-docker-card-probe.py --plumbing   # no keys needed
```

The `plumbing` phase launches one container, pushes a tree in, mutates it
inside, pulls the task tar back, and extracts it the way `restore_task_dir`
does. It asserts the three facts that code rests on: `./.git` is a member of the
archive (so the restore must exclude it), the container's `.git` never lands on
the host worktree, and a deletion is invisible to a tar (the stale host copy
survives extraction, which is why the manifest diff's deleted list is applied on
top). No keys, no agent, about a minute — this is the phase CI runs on the Linux
leg, since GitHub runners ship Docker.

The `card` phase consumes free-tier quota: it seeds a scratch `CLAWDE_HOME` with
the real auth store, registers a scratch repo, and drives one trivial card
through the scheduler. It asserts the card reaches `review` with a pinned commit
whose tree holds the agent's file, that the attempt artifact tar was pulled, and
that no `clawde-katban` container is left behind. On failure it keeps the
scratch state and prints the rung records plus the scheduler log — a
40-character summary in the card result hides the difference between an agent
error, a failed gate and a skipped one.

The tier needs a ladder: `attempts` defaults to 1, and `runtime docker` with one
attempt has nothing to containerize. The CLI and the slash command refuse that
combination at the switch and name the fix (`board attempts 2`, then
`board attempts-upstreams <ID>` or free keys); the runner fails the card with the
same reason when the ladder is empty by run time, because keys can disappear
between the two.

Exit code 2 means a prerequisite was missing (dockerd unreachable, no debug
binary, no auth store), so a wiring that reads non-zero as failure cannot read a
skip as a pass. The card phase starts the scheduler in its own session and stops
it in a `finally`, so Ctrl-C is safe; a SIGKILL skips that, leaving the
scheduler and possibly a container behind:

```bash
pkill -f 'clawde katban board run --project probe'
docker ps -aq -f 'name=^clawde-katban' | xargs -r docker rm -f
```

## State and services

Katban state lives under:

```text
~/.clawde/katban/
├── katban.json       # hosted development-site configuration
├── boards/           # one board namespace per project
├── projects.json     # project -> repository registry
└── admin.json        # admin password hash, sessions, runner settings
```

`CLAWDE_HOME` changes the root. Cat Chat state is never written into this
namespace after the compatibility fallback is migrated.

For always-on operation, `clawde katban board expose --run NAME,...` renders
`katban-board.service`. It is a distinct non-root systemd unit from
`catchat.service`. Caddy configuration is generated into the managed include
file, while the shared reload watcher only reloads Caddy after that managed
file changes.

## Diagnosing drift: `clawde katban doctor`

`doctor` diffs the persisted desired state (exposed sites, Cat Chat links, the
admin board) against what the host actually has, and prints one line per check:

```bash
clawde katban doctor
clawde katban doctor --caddy-dir /etc/caddy   # non-default include dir
clawde katban doctor --fix                    # rewrite the managed include + reload caddy
clawde katban doctor --strict                 # exit non-zero when any error is found
```

It checks, one line per finding:

- the managed include against the freshly rendered text;
- that the Caddyfile actually loads it — a specific `.conf` import is flagged
  (deleting that file makes Caddy refuse every reload and takes unrelated sites
  down with it), and so is a Caddyfile that imports nothing at all, since then
  none of the routes exist;
- every exposed route's backend: each live site's proxy port, each locked
  site's folder, and the Cat Chat / admin-board ports. A live site whose server
  is down is a 502 and a locked site with a missing folder is a 404, neither of
  which caddy itself reports. A site's proxy target is always its **registered**
  port (the one `site list` prints): `site serve <NAME> --port N` re-points the
  server without touching that registration, so serving anywhere else answers
  locally and 502s through caddy — `site serve` warns when the two differ;
- each always-on unit's `ExecStart` binary — a unit pinned to a debug build
  (which `cargo clean` removes) warns rather than passing, and a relative path
  is resolved against the unit's `WorkingDirectory` rather than doctor's;
- that each installed role unit serves the same port its caddy block proxies to
  — the unit and the block are rendered at different times and installed by
  hand, so a stale port is a 502 nothing else reports;
- that `katban-reload.path` is installed and watches *this* caddy dir's include,
  since otherwise a new `expose` never reaches caddy;
- that the installed reload watcher **validates before it applies** — a watcher
  running a bare `systemctl reload caddy` applies whatever is on disk, and it
  also has to gate the *same* caddy dir doctor is looking at, or it is
  validating a different Caddyfile than the one being served;
- that caddy itself is running, when the host has a `caddy.service` unit (a
  *failed* unit is an error, a stopped one warns, and a host with no caddy unit
  — a container or a snap, say — is left alone rather than guessed at).

Units are only expected for roles that are configured, so a missing
`catchat.service` on a host that never ran `catchat expose` is not reported. A
store that will not parse (`katban.json`, Cat Chat links, the admin store) is
reported as an error finding instead of aborting the report. `--fix` rewrites
the managed include from desired state, reloads caddy, and re-runs the checks so
the summary reflects the repaired host; it refuses to write into a directory
that holds neither a Caddyfile nor the managed include, so a typo'd
`--caddy-dir` cannot leave a stray `katban.conf` behind.

Exit status is 0 while there are warnings; `--strict` makes any error fatal.

## Applying config: write, validate, reload

Every `expose` (and `doctor --fix`) applies the managed include in the same
three steps, in this order:

1. **write** the include atomically (temp file + `rename`);
2. **validate** the whole Caddyfile with `caddy adapt`, which parses and adapts
   without starting anything and without root (~50 ms). If the result no longer
   adapts, the include is **rolled back** to its previous contents and the
   command fails with caddy's own diagnostic;
3. **reload**, then report exactly which command ran.

The rollback is the part that matters. Caddy keeps the last-good config when a
reload fails, so a bad fragment cannot take the proxy down — but on its own that
means the bad fragment stays on disk, failing every later reload until someone
notices. Validating before reloading means an unadaptable state is never
persisted.

How the reload runs depends on the host:

- **default** — `systemctl reload caddy`. This is the trusted path: the
  Caddyfile is adapted by the host's own caddy, and the unauthenticated TCP
  admin API is never used.
- **socket-scoped admin endpoint** — if the Caddyfile sets
  `admin unix//run/caddy/admin.sock`, the reload runs root-free as
  `caddy reload --config <Caddyfile> --address <socket>`. Socket permissions do
  the access control, and the path belongs to the install that names it, so a
  scratch `--caddy-dir` can only fail to connect.
- `CLAWDE_CADDY_RELOAD=systemd` forces the service reload;
  `CLAWDE_CADDY_RELOAD=caddy` asks for the admin API and is **refused, with an
explanation**, unless the endpoint is a socket — posting a different
directory's Caddyfile to the default `localhost:2019` would replace the running
proxy's entire config.

The same choice is what the rendered `katban-reload.path` watcher uses, so a
socket-scoped host gets a watcher that needs no root either.

### The installed watcher runs the same gate

`katban-reload.service` does not run `systemctl reload caddy` directly. It runs

```bash
clawde katban caddy reload --caddy-dir /etc/caddy
```

which is the same write-free half of the sequence above: `caddy adapt` first,
reload only if the whole Caddyfile still adapts. That is what puts a *hand edit*
and any other writer of the include behind the gate, not just Clawde's own
`expose`:

- healthy → `validated <Caddyfile> with caddy adapt`, then the reload;
- unhealthy → `refused to reload: … no longer adapts` plus caddy's own syntax
  error, exit 1, and **no reload**. Caddy keeps serving its last-good config,
  and `journalctl -u katban-reload.service` names the file that needs fixing.

The watcher re-detects the reload flavour when it fires, so moving caddy's admin
endpoint to a socket later does not require re-rendering the unit. Because it
now runs the clawde binary, the unit only works while that file exists **and is
current**: a watcher rendered before this subcommand existed fails in the
journal as `unknown katban command: caddy` until the binary is rebuilt
(`cargo build --release`, or `clawded`) and the `expose` re-run. `doctor`
reports a watcher whose `ExecStart` binary is missing, and a watcher that
reloads unvalidated (a pre-gate unit) as `katban-reload gate`; a binary that
exists but is older than the subcommand is not something it can see.

## Separation rule

Use **Cat Chat** when the goal is “give friends a URL and a simple password so
they can chat.” Use **Katban** when the goal is “run and review development
work through agent-managed Kanban cards.” The two products may share low-level
HTTP/Caddy helpers, but they do not share product commands, stores, session
authorization, or password policy.
