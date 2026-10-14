# Cat Chat

Cat Chat is Clawde's optional public-facing chat client. It is deliberately a
small sandbox for sharing a conversation with friends; it is not the Katban
development system.

## Scope and safety boundary

Cat Chat guests can use:

- chat with Clawde through the configured free/limited provider path — the
  answer text only, never the model's reasoning (upstreams that fold their
  chain of thought into the content field are stripped before a guest sees it);
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
/chat unblock <BUCKET>
```

`--set` is literal: the value is the password guests type. It is not a crypto
seed, encryption key, or value from which anything else is derived. The server
stores a salted hash, never the plaintext.

## Shell commands

The shell namespace is intentionally separate from Katban:

```bash
clawde catchat serve [--port 8789] [--host 127.0.0.1] [--search-endpoint URL]
clawde catchat expose --subdomain chat.example.com
clawde catchat links create friends
clawde catchat links list
clawde catchat links show <ID>
clawde catchat links revoke <ID>
clawde catchat links password <ID>
clawde catchat links password <ID> --set "YOUR PASSWORD"
clawde catchat unblock <BUCKET>
clawde catchat status
```

The server refuses non-loopback binding unless `--allow-non-loopback` is passed.
The usual public setup is to keep Cat Chat on loopback and let Caddy terminate
TLS and proxy the configured subdomain.

## Guest web search

Guests get exactly one tool, `web_search`, and it runs against a dedicated
SearXNG JSON API — never the host's own search client. The endpoint is
configurable, most specific first:

1. `--search-endpoint URL` on `catchat serve` (or `--search-endpoint=URL`);
2. `CLAWDE_CATCHAT_SEARCH_ENDPOINT`;
3. the default, `http://127.0.0.1:8080` (the conventional SearXNG sidecar port).

An empty or blank value is ignored rather than treated as "no search", so an
unset variable cannot silently disable the feature.

`catchat serve` **probes** the endpoint once at startup and prints the verdict.
A working endpoint logs
`guest search: <url> (SearXNG JSON API answered)`; anything else logs the
endpoint, then `WARNING: unavailable` and the reason — an unreachable host, a
non-JSON or non-2xx reply (another service squatting on the port), or JSON with
no `results` array. This matters because 8080 is the most contested port on a
home host: on 2026-10-13 it belonged to Nextcloud here, so the old
"local SearXNG at …" line advertised a feature that could only ever fail.

When search is unavailable the guest is not told a bare "search failed": the
model receives a tool result naming the failure and instructing it not to guess
and not to claim it searched, and the guest gets an answer from what the model
already knows. Results themselves are untrusted web content and are screened
before reaching the model (capped in number, snippets truncated, control
characters stripped, instruction-shaped results dropped).

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
After the block expires, the ladder starts over. The owner clears one with
`/chat unblock <BUCKET>` or `clawde catchat unblock <BUCKET>`.

**The argument is a bucket, not always an address.** Failures are filed under
the client the request came from, and there is one bucket that is not an
address: a request from a loopback peer with no `X-Forwarded-For` (a local
`curl`, or a proxy that sets no header) is filed under `loopback`, because
nothing in it identifies the caller. `clawde catchat status` and `/chat status`
name every live bucket with what is left of its window, and `unblock` resolves
the loopback spellings (`127.0.0.1`, `localhost`, `::1`, `[::1]:8789`, …) onto
that bucket, so `unblock 127.0.0.1` does clear a locked-out local guest. An
argument that matches nothing is an error naming the buckets that *do* hold a
lockout — never a reported success over a key nothing was stored under.

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

## Verifying the guest journey

The Cat Chat pages are one embedded HTML+JS document in
`crates/katban/src/catchat/pages.rs` with no frontend build step, so
`cargo test` never exercises them. `scripts/probes/catchat-ux-probe.py` drives a
real browser through the whole guest journey: the sign-in gate a stranger hits
first, a wrong password refused in the page, the shared password landing in the
chat, a reply rendering, the session surviving a reload, the downloadable
summary, and a locked-out local guest being let back in through
`catchat unblock`.

Guest web search is covered end to end as well. The probe starts a second local
mock that speaks the SearXNG JSON API and passes it with `--search-endpoint`,
and the mock upstream answers one marked guest message with a `web_search` tool
call. That lets the probe grade the whole path: the model's query reaching the
endpoint as `?q=…&format=json`, the screened result coming back in the model's
next request, the answer reaching the guest, and both verdicts of the startup
probe (a working endpoint reported as such, an unreachable one reported as
unavailable). The adapter itself also has Rust coverage over real HTTP in
`crates/katban/tests/catchat_guest_search.rs`.

```bash
cargo build
scripts/probes/browser-probes.sh catchat
```

It needs no configured free-tier keys: the probe points the guest chain at a
local OpenAI-compatible mock upstream with `CLAWDE_FREE_BASE_URL_GROQ` (debug
builds only, loopback only) and a scratch `auth.json`. That is also what makes
the reply deterministic — the mock answers with
`reasoning</think>VISIBLE-ANSWER`, the shape an upstream that leaves its chain of
thought in the content field produces, so the probe grades that the guest sees
only the answer.

It runs against a scratch `CLAWDE_HOME` and exits 2, not 0, when a prerequisite
is missing; CI (the Linux leg) passes `--require-browser`, which turns that skip
into a failure. `playwright` comes from the shared probe venv
(`scripts/probes/browser-probes.sh setup`), not the system python, which
refuses a bare `pip install` on an externally-managed distro (PEP 668).

## What Cat Chat is not

If the task involves cards, dependencies, project repositories, agent
execution, review, verification, worktrees, or development-site hosting, use
[Katban](katban.md) and `/katban` instead.
