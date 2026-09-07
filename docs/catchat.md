# Cat Chat

Cat Chat is Clawde's optional public-facing chat client. It is deliberately a
small sandbox for sharing a conversation with friends; it is not the Katban
development system.

## Scope and safety boundary

Cat Chat guests can use:

- chat with Clawde through the configured free/limited provider path;
- the dedicated web-search adapter; and
- their own ephemeral conversation plus an optional downloaded summary.

Cat Chat does **not** provide host-file access, shell commands, project boards,
git worktrees, agent scheduling, development tools, or access to another guest's
session. Guest sessions live in memory and are evicted after inactivity.

Cat Chat is intended for a self-hosted side project. Treat its shared password
as a door key for friends, not as an enterprise identity system. Use HTTPS
through Caddy before sharing a public URL.

## TUI: `/chat`

Bare `/chat` opens the Cat Chat link manager popup. The popup clearly separates
these two password actions:

- **Generate a new link + password** — creates a link and prints a random
  password once.
- **Set your own password** — seeds `/chat password <ID> --set ` so you type the
  exact memorable password your guests must remember.

The same management actions are available as slash commands:

```text
/chat
/chat status
/chat links
/chat create <NAME>
/chat show <ID>
/chat revoke <ID>
/chat password <ID>
/chat password <ID> --set "YOUR PASSWORD"
/chat unblock <IP>
```

`--set` is literal: the value is the password guests type. It is not a crypto
seed, encryption key, or value from which anything else is derived. The server
stores a salted hash, never the plaintext.

## Shell commands

The shell namespace is intentionally separate from Katban:

```bash
clawde catchat serve [--port 8789] [--host 127.0.0.1]
clawde catchat expose --subdomain chat.example.com
clawde catchat links create friends
clawde catchat links list
clawde catchat links show <ID>
clawde catchat links revoke <ID>
clawde catchat links password <ID>
clawde catchat links password <ID> --set "YOUR PASSWORD"
clawde catchat unblock <IP>
clawde catchat status
```

The server refuses non-loopback binding unless `--allow-non-loopback` is passed.
The usual public setup is to keep Cat Chat on loopback and let Caddy terminate
TLS and proxy the configured subdomain.

## Cat Chat password policy

Cat Chat is intentionally friendly to memorable passwords. There is no minimum
length requirement. The small sanity checks reject:

- empty values;
- all-digit values;
- a single repeated character; and
- common spray-list values such as `password1`.

Generated passwords are random and shown once. Rotating a password immediately
invalidates the old password and existing device credentials remain governed by
the link's device/session rules.

Wrong-password protection is Cat Chat-only: four failures cause a three-minute
lockout, then five more cause another three-minute lockout, then five more
(the ninth failure) cause a flat 24-hour block. During that block the response
is `l'épée de Damoclès — your lives are gone`; there is no countdown message.
After the block expires, the ladder starts over. The owner can clear an IP with
`/chat unblock <IP>` or `clawde catchat unblock <IP>`.

## State and deployment

Cat Chat stores its state separately from Katban:

```text
~/.clawde/catchat/links.json
```

`CLAWDE_HOME` changes the root for both products. Existing installations that
still have `~/.clawde/katban/links.json` are read as a one-time compatibility
fallback; new writes go to the Cat Chat directory.

`catchat expose` writes the Cat Chat route into the managed Caddy include and
renders `catchat.service`, a non-root systemd unit with restart-on-failure. The
Katban board has its own `katban-board.service`; the services and credentials
are not shared.

## What Cat Chat is not

If the task involves cards, dependencies, project repositories, agent
execution, review, verification, worktrees, or development-site hosting, use
[Katban](katban.md) and `/katban` instead.
