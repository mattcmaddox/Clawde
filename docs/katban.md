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
/katban board unblock <IP>
```

`Alt+G` opens the Katban controls menu. It contains board/project/site
operations and the Katban admin credential controls. It intentionally contains
no Cat Chat link or guest-lockout rows. `/chat` is the separate popup for that.

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

### Container tier (`board runtime incus`)

By default attempts execute on the host: the agent runs in the card's
worktree and safety comes from the worktree lane. Boards that want a hard
boundary can opt into the container tier:

    clawde katban board runtime incus --project my-app   # default: host

With `incus`, every attempt runs inside an ephemeral Incus container
(`images:ubuntu/24.04/cloud`, torn down no matter how the attempt ends): the
worktree is pushed in, the agent runs with `bypass-permissions` (the
container IS the safety boundary), the verify gate runs in-container, and a
sha256 filesystem manifest before/after the run captures everything the
agent actually touched — including untracked and gitignored files the git
diff cannot see.

- **Scope gate**: give a card an allowlist with
  `clawde katban board card scope <ID> src/ stats.py` (empty = no scope
  opinion). Any manifest-diff path outside the allowlist is a scope
  violation and the rung cannot win — an out-of-scope edit is exactly the
  blast-radius signal the host tier's git diff misses.
- **Dependency provisioning is setup, not card state**: missing shared libs
  for the binary are apt-installed into the container; a failed install
  excludes the rung as an environment error instead of failing the card
  (mirrors the verify gate's install-failure skip semantics).
- **Artifacts**: the attempt's final tree is pulled as a tar under
  `~/.clawde/katban/container-artifacts/<card>/` for post-mortem review.
- **Requirements**: the `incus` CLI must be installed and incusd reachable
  (`incus list`); `board runtime incus` refuses to switch otherwise. The
  per-attempt cost is real (container launch + push + apt), so host tier
  remains the default.

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
`/katban board unblock <IP>` or the corresponding board CLI command clears it.

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

## Separation rule

Use **Cat Chat** when the goal is “give friends a URL and a simple password so
they can chat.” Use **Katban** when the goal is “run and review development
work through agent-managed Kanban cards.” The two products may share low-level
HTTP/Caddy helpers, but they do not share product commands, stores, session
authorization, or password policy.
