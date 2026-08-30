# Katban — Self-Hosted Clawde Web + Hosting (Public / LAN / Private) — Feature Spec

Status: Draft (post-structured interview: 5 discovery rounds + 4 audit rounds)
Scope: New crate(s) + CLI entry point for self-hosting Clawde: a kanban board
server ("Katban"), a dev-site hosting engine, and two access tiers (guest +
admin) across three network modes (private / LAN / public). No code changed
yet — this is the spec.

Author: user + agent (agent as design engineer)

**Build status — TUI control surface live (2026-08-29):** `/katban`
slash command (§19.1) with autocompletion and the Alt+G scrollable controls
menu (§19.2) are built and verified live in the TUI: menu opens with the
live store (per-link rotate-password/revoke rows, locked-IP unblock rows),
navigation skips headers and scrolls, Enter runs the seeded `/katban`
command through the registry (verified end to end: opened menu -> navigated
-> revoked a link from the menu -> store updated). The **Boards section**
(§19.3) adds per-card advance rows (`/katban board card set <id> <next>`)
plus list / ready / add-card rows; `CardStatus::next()` defines the advance
ladder. `/katban` gained the matching board subcommands with card-ID +
status autocompletion. `clawde katban link password <ID>` CLI subcommand
added for parity (rotates + prints once). An audit (§19.4) fixed multi-word
link names (slash command + CLI), removed menu rows for revoked/expired
links, and wired `--project NAME` through `/katban board ...` execute +
completions (dead `load_board_cards(_project)` param removed). A guest
server hardening audit (§19.5) closed XFF spoofing, port-blind origin
checks, missing `Secure` cookie, unbounded device/session growth, missing
chat rate limits, and a thin prompt-injection screen. A site-hosting audit
(§19.6) fixed caddy-dir drift (reloader watched the wrong file, bootstrap
imported the wrong file), the hardcoded guest port in `katban.service`,
caddy-config injection via unvalidated names/subdomains, missing `nosniff`
+ full-file reads, watcher reload storms over `node_modules`/`.git`/`target`,
and a possible root unit user under sudo. A board audit (§19.7) fixed
Review/Blocked cards auto-restarting, slug-colliding project names (lossless
encoding), clock-derived card ids, and added `/katban board link`/`unlink` +
Alt+G menu rows for dependencies; the per-project board write lock
(`board::BoardLock`, flock-backed) then landed so concurrent writers
serialize instead of last-writer-wins. Test counts: 86 katban + 10
katban-command + 5 tui dialog + 53 cli tests, clippy + workspace green.
§20 sketches the web board (Cline Kanban mapping) for the admin tier. Phase
1 of that web board is now built and verified (see §20.7): a thin read-only
board server (`clawde katban board serve`, port 8790) with inline HTML UI
+ `/api/projects` and `/api/board/{project}`, tested + clippy-clean
(+4 board_server tests -> 90 katban total).

**Build status — guest link feature complete (new crate `clawde-katban`):**
On top of the earlier slices (site lifecycle + `expose` + caddy include,
board backend, DuckDNS, `status`, LAN exposure flags), the guest tier
(§6–§9) is now built: `clawde katban link create/list/show/revoke` mint
password-protected guest links (salted-hashed passwords, per-device opaque
tokens stored only as hashes, expiry, per-link concurrency cap), and
`clawde katban guest serve` runs the guest chat server — login page, device
cookie (httpOnly + SameSite=Strict), chat page, `/api/chat` (chat +
WebSearch only, via a dedicated SearXNG endpoint, results screened as
untrusted data per §7b), and an on-demand downloadable AI session summary
(§6.1). Security: Origin checks on every request (Cline Kanban §3b lesson),
per-IP lockout after 5 wrong passwords, no routes to boards/projects/host
files, ephemeral in-memory sessions. Guest chat rides the host's free
providers (`free/auto`) and degrades to a friendly message when none are
configured. Config in `~/.clawde/katban/katban.json` + `links.json`. 68
tests + clippy + workspace check green; smoke-tested end to end (real model
reply, summary, lockout, revoke, cross-origin refusal).

Guest hardening updates: the lockout ladder is 5 wrong passwords -> 3-minute
lock, then 3 more -> 3-minute lock, then 3 more -> permanent block (per-IP,
persisted across restarts), with `clawde katban guest unblock <IP>` as the
admin escape hatch; `guest expose --subdomain chat.example.com` puts
the guest chat behind caddy (writes the managed `katban.conf` block alongside
exposed sites + best-effort DuckDNS update); the server re-reads `links.json`
when it changes on disk, so `link revoke`/`guest unblock` take effect
without a restart. Still to build per this spec: the board web UI + agent
spawning (§12 agent-execution slice) and the Katban container/caddy-reload
service (§10.1a units).

**Live bootstrap — done on TheDrone (2026-08-29):** `guest expose
--subdomain chat.example.com` wrote `/etc/caddy/katban.conf`, the
one-time `import katban.conf` was added to `/etc/caddy/Caddyfile` (backed up),
`caddy adapt` validated the route, and `caddy reload` applied it gracefully
via the admin API — no sudo needed (the reloader path unit is still optional;
`caddy reload` is the no-root fallback). Verified live through the public URL:
login page over TLS (wildcard cert), password login, chat page, and a real
model reply through `https://chat.example.com`. This surfaced and
fixed a real bug: the Origin allowlist only knew loopback, so browser
same-origin POSTs (browsers always send `Origin`) were refused — the check
now also allows the request Host and the configured public subdomain.

**Runtime decision flipped to systemd (2026-08-29, §7/§11/A3/B1):** Katban's
always-on runtime is now a **systemd service** (`katban.service`), not a
container — decided because Katban is a control plane for a bare-metal caddy
host and the auto-reloader is already a host-side systemd unit; the full
compose stack is the documented alternative for fresh installs, and SearXNG
is the one always-on container. Both expose commands now render
`katban.service` (User=user, `Restart=always`, `clawde katban guest serve`)
plus the reloader units into `~/.clawde/katban/caddy/` and print the
one-time `sudo systemctl enable --now` bootstrap. `systemd-analyze verify`
passes on all three units.

**Always-on service installed live (2026-08-29, §11 done):** the one-time
sudo step is complete on TheDrone. `katban.service` (User=user,
`Restart=always`, `clawde katban guest serve`) is installed at
`/etc/systemd/system/`, enabled, and active — the guest server now runs as a
managed service (verified: active (running), survives the ad-hoc process
being stopped). `katban-reload.path` + `katban-reload.service` are installed,
enabled, and verified live: appending to `/etc/caddy/katban.conf` fired the
path unit and `systemctl reload caddy` exited SUCCESS with the site still
serving 200. Full auth + chat round-trip verified through the public URL
under the systemd-managed server. To update the binary later: rebuild in
place and `sudo systemctl restart katban`.

The `/auth` endpoint now accepts **both** JSON (`{"password": ...}`) and
classic form-encoded (`password=...`) bodies via a dual `FromRequest`
extractor (content type decides; JSON wins when ambiguous) — the login page
(JSON), curl, and older clients all work; verified live through the public
URL with both formats (67 tests).

---

## 1. Origin of the request

The user wants Clawde to be reachable from the internet and self-hosted, in a
**safeguarded** way, split across three exposure modes:

1. **Public** — a URL they can share with friends so those friends can "try
   Clawde" with **zero access** to the user's files and system (strongly
   gated), **and** a live, public-facing dev site that updates as they code.
2. **Private** — admin/remote access with near-"777" power but still scoped
   safeguards and warnings.
3. **LAN** — local-network-only access, admin-password gated, permissions like
   present-day Clawde.

The self-hosted surface is named **Katban** (user's coinage). It is intended to
be functionally mostly like **Cline Kanban** — a web board that runs coding
agents per task — plus a **dev-site hosting engine** that automates
caddy/systemctl so an "online and working public page that updates with your
changes" never requires manual restart (that restart-is-the-pain is the single
biggest complaint driving this). The user explicitly asked the agent to act as
the engineer: reuse/repurpose whatever Clawde stack layers fit, and pick from
adjacent projects (Cline Kanban, OpenCode, Agent Kanban, Vibe Kanban) what is
engineering-worthy.

The three modes deliberately tie together (from the interview):
**PUBLIC = live site + guest clawde access (all public); LAN = local testing +
admin control (admin password, permissions like today); private = you only.**

**Architectural reframe (audit round, user):** Katban is the **always-on,
pre-secured web-facing server scoped to dev work**. The other surfaces — guest
chat and public live-viewing of a project — **piggyback on Katban's hardened
networking** rather than each opening their own port. A project that "goes
through this stack" is preconfigured with known, safe parameters, so the
sysadmin/owner never has to reason about dangerous exposure per project. Katban
owns the safe defaults for bind, TLS, caddy, and per-project subdomains, and
every exposed surface rides behind the same auth + jail boundary (§6–§8).

---

## 2. Grounding against Clawde today (verified against code)

Everything below is verified; treat as locked facts, not to be re-litigated.

### 2.1 Existing network/serving surfaces

- **OpenAI-compatible gateway** (`clawde serve`, crate `clawde-gateway`) —
  axum server with `POST /v1/chat/completions`, `POST /v1/responses`,
  `GET /v1/models[/{id}]`, `/healthz`, `/status`. Bearer auth (constant-time),
  per-key RPM/TPM token buckets, in-flight semaphore, timeouts, graceful
  drain, `permissionMode` (`allow-readonly` default / `allow` / `deny`),
  `workspacePaths` roots, curated `builtinTools` surface, loopback-only by
  default (`--allow-non-loopback` opt-in), optional TLS cert/key.
  `run_agent_loop` (`gateway/src/agent.rs:232`) is the server-side agent loop.
- **ACP server** (`clawde acp`, crate `clawde-acp`) — JSON-RPC over stdio or TCP
  (`--listen`, `--tls-*`, `--allow-non-loopback`).
- **Remote-control bridge** (crate `clawde-bridge`) — WebSocket/SSE bridge to a
  web UI session, poll loop, register/deregister.

### 2.2 Worktree / project machinery (native — the key enabler for Katban)

Clawde already isolates work per git worktree:

- `EnterWorktreeTool` / `ExitWorktreeTool` (`tools/src/worktree.rs`), one active
  worktree per session, default `.worktrees/<branch>`, optional
  post-create command.
- Agent-mode `isolation: "worktree"` (`query/src/agent_tool.rs`) runs a
  subagent in a dedicated worktree.
- Verify sandbox `git worktree` (`query/src/verify_sandbox.rs`,
  `run_checks_in_worktree`).
- `/move` re-homes a session between worktrees (`commands/src/new_move.rs`),
  mirroring OpenCode's many-worktrees-per-project model.
- `SnapshotRegistry` (`core/src/snapshot/registry.rs`) shares by worktree path
  so concurrent sessions never collide.
- Headless CLI: `clawde --print "..."`, `--resume`, `--output-format json|
  stream-json`, `--allowed-tools/--disallowed-tools`, `--add-dir`,
  `--permission-mode`, `/keys`, etc. — everything a board-spawned card needs.

**Implication**: the Cline-Kanban model (one throwaway worktree per card, run an
agent in it, review the diff, commit or trash) is already Clawde-native. The
worktree piece is greenfield-ish; the agent-in-worktree piece mostly exists.

### 2.3 Permission & mode machinery

- `PermissionMode` (Default / Plan / AcceptEdits / BypassPermissions),
  `AutoApproveMode` (`core/src/auto_mode.rs`), `PermissionManager` w/
  `session_rules`/`add_session_allow[_path]`.
- Named presets (`core/src/lib.rs` ModeDef; `careful`/`fast`/`default`,
  custom from `~/.clawde/modes/` and `.clawde/modes/`); modes never silently
  select `BypassPermissions`.
- The **modes here are network/auth tiers, not the ModeDef presets** — but the
  preset + permission systems are available to express per-tier policy
  (e.g. guest tier = deny-write + tiny tool surface; admin tier = Plan/AcceptEdits
  cadence with confirmations).
- The spec-mode write gate, `plan_gate_error`, and verify sandbox are reusable
  for the admin-tier "confirm on destructive" behavior.

### 2.4 Room for new code

There is **no web UI**, no auth-tier layer, no board server, no caddy
automation, no file watcher / live-reload host, and no guest-sandbox runner
today. These are the genuinely new components Katban adds.

---

## 3. Research summary (bleeding-edge state)

### 3a. Cline Kanban (the core model)

`npx kanban` / `kanban` — a Node web app bound to `127.0.0.1:3484`, runs CLI
agents (Cline CLI, Claude Code, Codex, OpenCode) in parallel. Model:

- Four columns: **Backlog, In Progress, Review, Trash**. Add task via modal or
  sidebar "Kanban Agent" chat. Optionally "Start in plan mode".
- Per task: an isolated **git worktree** + headless agent; card shows the
  currently executing command in real time.
- **Dependencies**: drag card A onto B → B starts only when A completes; a
  complex build decomposes into a DAG (Kanban Agent links via
  `kanban task link --task-id … --linked-task-id …`).
- **Review**: completed cards move to Review; read the agent's conversation in a
  TUI and inspect the diff; **inline diff comments** steer the agent
  ("comment on a line → feedback to the agent"); then Commit or Open PR.
  Commit runs a commit pass, merges into main, and dependent cards auto-start.
- **Trash**: archive + cleanup the card's worktree.
- Persistence: `~/.cline/kanban/workspaces/<project>/boards.json` (plain JSON,
  columns/cards/dependencies), config ~ `~/.cline/kanban/config.json`
  (`selectedAgentId`, `agentAutonomousModeEnabled` default true — i.e. YOLO by
  default — which we deliberately reject for guest/admin-by-default).

### 3b. Known Cline Kanban security incident (design imperative for us)

Oasis Security (May 2026): CVSS 9.7 cross-origin **WebSocket hijack** in Cline
Kanban's kanban server. The local WS listener had **no origin check, no auth
token, no client verification**. Result — any webpage a developer visits can:
(1) open a WS to `localhost:3484` and exfiltrate a full workspace snapshot
(paths, tasks, branch, agent chat history); (2) write to the agent terminal input
→ inject arbitrary shell commands the agent treats as user instructions (shell on
the dev machine); (3) DoS active tasks. Fixed in v0.1.66.

**Hard requirements we adopt from this** (non-negotiable):
- Every WebSocket / HTTP-request surface must validate the **Origin** header.
- Every socket/channel requires a **capability-scoped token** (not none, not
  just a shared secret).
- Bind loopback-only by default; never bind `0.0.0.0` without explicit opt-in.
- No "YOLO by default" like Cline's `agentAutonomousModeEnabled: true`.
  Katban's default autonomy posture must be gated (see §7).

### 3c. Adjacent projects to lean on (user: "all, prioritized")

- **OpenCode** project/worktree model (many worktrees per project) — already
  mirrored in Clawde's `/move`; adopt for board-project scoping.
- **Vibe Kanban** — modern minimal kanban; ideas for UI density.
- **Agent Kanban (VS Code)** — PR-style review + structured `@kanban plan/todo/
  implement` commands; adopt the PR-style review flow.
- **FrugalGPT / free cascade** — already embodied by `free/auto`; guests ride it
  for free (§9).

---

## 4. The three modes (derived from bind/access)

Mode is **auto-derived from how a request reaches Katban + which auth is
presented** (interview decision). No mandatory `--mode` flag; the primary
control surface — **TUI-driven setup** plus a minimal `/katban` admin web route
(loopback only) — can pin/override the mode persistently, and an env/CLI
override exists for scripting. Requests are classified by (source network ×
presented credential).

| Mode | Source | Auth required | Guest shows | Admin can | Default permission posture |
|---|---|---|---|---|---|
| **Private** | `127.0.0.1` / local control | Admin (or trusted local) | n/a | Full 777 + safeguards | Bypass-ish but with destroy-confirm + audit (see §8) |
| **LAN** | local subnet | Admin password | Guest surface requires guest pw | Full (like today's `permissionMode`, e.g. Default/Plan/AcceptEdits) | Permission system of present-day Clawde |
| **Public** | internet | Guest shared password → guest tier; separate admin pw → admin tier | Chat + WebSearch only (§6) | 777 + safeguards | Guest: deny-write, tiny surface (§6), hardened jail (§7) |

Rules:
- The **admin tier always requires the admin credential**, whichever network.
- The **guest tier only activates on non-loopback/public exposure**; on loopback
  the guest surface is hidden or inert.
- The **hosting surface** follows the same mode: a public dev-site is only
  reachable in Public mode; LAN-only sites only on LAN; local previews on
  Private/LAN.
- A request whose source-network can't be classified (e.g. double NAT,
  IPv6) falls back to the most restrictive classification.

The gateway's existing `clawde serve` surfaces stay separate: Katban is its own
HTTP server/board, and may call the gateway agent loop as its on-demand agent
engine (§12), but they are distinct processes/routes by design.

---

## 5. Entry point & startup

- New CLI subcommand **`clawde katban`** (crate `clawde-katban`), sibling to
  `serve` / `acp`. `clawde katban --help` documents the control-menu os rich
  text.
- Controls:
  - `--listen ADDR` (overrides derived bind; loopback default).
  - `--allow-non-loopback` explicit opt-in to bind `0.0.0.0` (same gate as
    gateway/ACP).
  - `--mode private|lan|public` optional pin; otherwise auto-derived.
  - `--tls-cert/--tls-key`, `--data-dir`, `--no-caddy` (disables proxy autopilot,
    §10).
- **Control / config surface (audit decision): TUI-driven setup is primary**
  — plumbing (auth, sites, caddy wiring) is configured in the existing TUI /
  setup flow; Katban exposes only a **minimal `/katban` admin web route** for
  runtime board/site toggles, reachable only from the private/loopback path.
- Startup prints the reachable URLs per classified mode and (private/loopback
  path) surfaces the admin route. On public bind it prints a loud security
  banner summarizing what is exposed and how to tighten.

---

## 5a. First-run experience (what you actually do — plain-English)

The plan's real goal is that setting this up feels like a friendly tour, not a
server-admin exam. First run looks like:

1. **`clawde katban setup`** (once). A guided setup asks a few plain questions:
   where to keep Katban's files (default `~/.clawde/katban/`), an **admin
   password** (for you), a **guest password** (to share with friends), and
   whether Katban may add its one line to your caddy config (you approve it
   once; it's just one `import` line). It then starts Katban as a background
   service that stays on and restarts itself if needed.
2. **Put a project online.** `site add my-folder` asks what kind it is: a plain
   folder, a built website's output, or a live preview that refreshes in the
   browser as you save. You pick a subdomain (or Katban suggests one). Done —
   you get a live link like `myproject.example.com` that updates as you
   work. No caddy edits, no restarts.
3. **Give a friend a try.** Mint a link (it expires automatically after a set
   time). Share the link + guest password. Friends get a simple chat where
   Clawde can also look things up on the web — nothing more.
4. **Use the board (you only).** Log in as admin, create task cards, drag one
   onto another to say "this can't start until that one finishes", hit play,
   and watch Clawde work in its own private copy of the project. Review the
   changes, leave comments, and ship with Commit or Open PR.

Sensible defaults (decided in §16a E16, changeable later):
- Up to **3** tasks running at the same time (the rest wait in line).
- A failed task tries again **twice** (only when it was a temporary hiccup),
  then stops and tells you.
- A friend's chat has **no question limit** (user decision) — the link can
  expire after **30 days** unless set to never expire; at most **2** friends
  chat at once as a safety cap you can raise.

---

## 6. Access tiers

### 6.1 Guest tier ("friends try Clawde, zero access to your system")

Role of guest is to **try Clawde**: they prompt, they get model answers. They do
not touch the user's projects, files, terminal, environment, or secrets.

- **Tool surface: chat-only + WebSearch.** Guests will only ever use a **web
  search** tool. Everything else is rejected structurally (no bash, no file
  read/write, no env, no git, no AskUser, no plan tools). Answer to the
  user's question "are there any tools actually safe?" — **realistically
  almost none, and web search is the one defensible read-only add-on**; even
  file *reads* leak project content and WebFetch is an SSRF/open-proxy risk
  (see §6.3). WebSearch is **on by default for every link** (§16a E10) and is
  **backed by a dedicated free, sandboxed search endpoint** (see §9), not the
  host's own search tool/secrets; everything else stays structurally denied.
- **Hardened isolation** (§7) regardless, so even a compromised guest path can't
  reach the host.
- **Sessions are ephemeral and scoped** to the guest shell: in-memory
  **working memory** (chat context + notes) only, destroyed on end. No
  cross-guest transcripts, no host-filesystem access, **and no filesystem
  scratch dir in v1** (audit decision). On request the guest may **download an
  AI-generated summary** of their own session content — the  only artifact they can take away, built purely from the guest's session. **Timing (decision,
  §16a E9):** generated **on demand when the friend clicks download**, using the
  same free providers — no cost unless someone actually wants a summary.
- **Provisioning**: the shared guest password, plus per-link policy (per-link
  knobs = max turns + expiry, §16a E10; WebSearch is on by default), minted by
  an admin in the control menu / a `/katban` command.
- **No orchestration for guests** — multi-agent/board embedding (dependency DAG,
  parallel cards, worktree spawning) is dev/admin-only and used for Katban and
  other admin dev work (§12); guests only ever get chat + WebSearch.

### 6.2 Admin tier ("777" with scoped safeguards — no budget caps)

- Separate admin password (+ optional TOTP 2FA; §8), device-trusted login.
- Near-unrestricted power **but** with the safeguards the user selected
  ("most of the above; gated for major destruction and deletions; leave off
  rate caps"):
  1. **Confirm-on-destructive** — deletes, git-history rewrites, large/irreversible
     refactors, and card worktree destruction require an explicit confirmation in
     the web UI before execution (wire the existing spec-mode write gate /
     `plan_gate_error` + verify machinery).
  2. **Write-root scoping warnings** — the board flags (warns, not blocks) when a
     task writes outside configured project roots.
  3. **Audit log** (§10.4) — every admin action recorded, tamper-evident,
     append-only.
  4. **No budget/spend/rate caps** (per interview) — admin work is not
     rate-limited/budget-capped.
  The confirm-on-destructive gate applies on **every** network, loopback
  included — "777" is a posture, not a perimeter (§16a E15).
- Per-mode permission posture (§4) applies; admin **on LAN** = present-day
  `permissionMode` (Default/Plan/AcceptEdits etc.); admin on the private/loopback
  path = the relaxed-by-default-but-confirm surface.

### 6.3 "Which tools are actually safe for guests?" (recorded reasoning)

| Tool | Guest-safe? | Why / risk |
|---|---|---|
| Chat (model I/O) | Yes (default) | Pure conversation; the whole point. |
| WebSearch | Marginal, opt-in only | Host-side search; hidden cost = token burn; must be concurrency-capped. Not SSRF (no internal secret used). |
| WebFetch | No | SSRF/open-proxy risk against the host's network/services. |
| Read / Glob / Grep | No | Leaks real project content even inside a scoped root; fingerprinting. |
| Bash / Pty | No | Arbitrary code execution. |
| Write / Edit / ApplyPatch | No | Mutates host files. |
| Env / Git / snapshot | No | Leaks secrets / history / infra. |
| AskUserQuestion | No | A guest blocking on the admin's approval dialog is a harassment/DoS vector. |

Conclusion: **WebSearch is the only guest tool and is on by default via the
dedicated sandboxed endpoint; zero *other* tools.** Never expose file/bash/web-
fetch/RAG-project-content to guests, period.

---

## 7. Isolation architecture (default = hardened lightweight jail)

Two distinct layers (audit decision, clarifies the earlier "default to jail"):

1. **Deployment**: Katban itself runs as an **always-on systemd service by
   default** (`katban.service`, rendered by `guest expose`/`site expose`,
   §11) — decided after the caddy mechanics were proven live: Katban's whole
   job is managing the host (caddy include file, sites, ports, host providers,
   and later git worktrees), caddy is bare-metal systemd here, and the
   auto-reloader is already a host-side path unit — so a container would only
   add a bridge, not isolation. A **full container stack** (katban + SearXNG +
   optional caddy sidecar, compose `~/Katban/compose.yaml`) remains the
   documented alternative for fresh/portable installs; **SearXNG is the one
   piece that is a container by default** (no host interaction, standard
   image, §9).
2. **Guest-session isolation**: a **hardened lightweight jail by default** —
   the lightweight hardening below — with full per-guest containerization as the
   heavier optional tier for public exposure. Both layers must be hardened.

Because guests get no filesystem/exec tools, the jail's job is defense-in-depth
so a compromise of the guest path still can't reach the host:

- **Process hardening**: Katban runs guest-agent work as a low-privilege,
  dedicated OS user if feasible; if running as root, refuse (mirror the
  existing root+sudol block on `--dangerously-skip-permissions`).
- **No guest filesystem access at all**: guests have no file tools and no real
  working directory; a guest "scratch" workspace, when enabled, is a
  **throwaway temp dir** created and destroyed per session (never a real
  project).
- **No network beyond the host's outbound**: guests get no inbound-triggered
  network tools; WebSearch goes through a **dedicated sandboxed search sidecar**
  (SearXNG by default, §9) — never the host's own search client or secrets. No
  WebFetch ever.
- **Scoped webserver**: guest requests are served by a dedicated surface that
  only renders chat + the guest's own ephemeral working memory / downloadable
  session summary — no routes to boards, projects, settings, config, or host
  files.
- **Outside-in**: per-link tokens, per-device tokens, Origin checks, per-link
  tool policy, max-turns/max-tokens, rate-limit+lockout are all *in front of*
  the agent loop (defense at the edge), not trusted by the agent.

### 7b. Defensive structures (research-backed; user requested these be added)

These are hard design requirements adopted from bleeding-edge AI-security work
so the "zero access" promise holds against a hostile stranger, not just a
friendly friend:

- **OWASP as the governing ruleset**: follow the OWASP Top 10 for LLM
  Applications (LLM01 prompt injection is #1) and the OWASP LLM Prompt Injection
  Prevention Cheat Sheet. Core rule — **separate data from instructions**: treat
  every untrusted artifact (guest prompts, WebSearch results, tool descriptors)
  as *data, never instructions*.
- **Classifier-gated untrusted content** (Anthropic, "Mitigating the risk of
  prompt injections", Nov 2025): all untrusted content entering the guest
  model's context — especially WebSearch results — is **screened by a
  classifier/guard before it is added to context**; flagged content is treated
  as untrusted data (or dropped/re-framed), never executed as instructions.
- **Default-deny egress + ephemeral lifecycles** (sandbox 7-patterns; Augment
  Code containment): guests have default-deny network egress; any guest
  sandbox/scratch (if ever reintroduced) is ephemeral and destroyed; no
  persistent guest state.
- **Multi-tenant isolation discipline** (hosted-agent isolation patterns):
  treat each guest as a discrete tenant with scoped routing, scoped storage,
  and per-tenant resource caps — no tenant can read another even if an agent is
  compromised.
- **AgentDojo-style red-team eval as a Phase-0 gate**: a bundled eval harness
  launches fixed hostile-guest scenarios — exfiltrate `~/.clawde`, read a real
  project, turn WebSearch into an open proxy, DoS via AskUser — and the guest
  path must pass before any public exposure is allowed.
- **Tool-poisoning guard** (arXiv 2603.21642): guests only ever see a curated
  **allowlisted** tool surface; host/MCP tools are never surfaced to guests, so
  poisoned tool descriptors can't reach them.

An **optional Docker compose** (`~/Katban/compose.yaml`, §11) packages the same
surfaces as containers (web UI, agent runner, optional per-guest sandbox
service) for maximal isolation of the public tier. Both paths are documented;
jail is the default posture.

---

## 8. Auth & session model

- **Guest**: shared password (session-scoped). First successful login **mints a
  strong per-device token** ("remember this device" from interview); subsequent
  visits use the token. Wrong-password attempts are **rate-limited then locked
  out** (per-IP/IP-range + per-account). Devices are **revokable individually**
  by admin. Per-link policies attach tool grants + turn/token budgets.
- **Admin**: separate password + optional TOTP. Same device-token +
  remember-this-device. Login events and all admin actions → audit log.
- **Cookies are httpOnly + SameSite=Strict + Secure-on-TLS**; tokens are
  capability-scoped and revocable; secrets (guest/admin passwords, device tokens)
  stored **encrypted at rest**.
- Brute-force: lockout window + noise/jitter; never reveal whether a username/
  link is valid vs. the password wrong.
- **Audit log scheme (engineer pick; user deferred — plain-English):** the
  admin action log is **append-only with a hash chain** — each line records the
  hash of the previous line, so nobody can silently edit or drop an early line
  without breaking the chain. Rotated by size; kept indefinitely unless the user
  sets a retention cap.

---

## 9. Guest LLM routing & cost posture

- Guests ride the **host's free-chain providers** (the `free/auto` cascade —
  all free/limited), per interview, because guests are expected to be rare and
  light. A **segregated guest routing path** (`free/auto` with tight per-request
  budget + concurrency cap) keeps guest traffic from competing with host
  requests or burning paid keys.
- **Guest WebSearch runs on a dedicated free, sandboxed search endpoint,
  included by default (audit decision) and available to ALL guest links** (no
  per-link opt-out of the search capability itself; per-link knobs are max
  turns + expiry, §16a E10). Default = a local **SearXNG** sidecar
  container in the same compose stack (no signup, standard JSON API; a thin
  typed Rust client with `reqwest`+`serde`, URL-encoded query, engine User-Agent,
  results are **capped/limited and classifier-screened as untrusted data** before
  they reach the model — §7b). Documented alternative:
  a public anonymous **Parallel Search MCP** server (`web_search` tool, no
  account). Both keep guest search fully separated from host credentials.
- Guests get **soft/structural** limits only: concurrency cap, per-request max
  turns/max tokens, per-link knobs (max turns + expiry, §16a E10), per-device
  lockout (§8). No budget caps for admin (interview).
- Any upstream/exhaustion surfaces to the guest as a friendly
  "temporarily unavailable," never raw key/url leakage (reuse gateway's
  no-leak guarantees).

---

## 10. Dev-site hosting engine ("when the TUI is not enough")

The single biggest pain driving this: "an online and working public page that
updates with my changes, without worrying about restarting caddy and systemctl."
Katban therefore **owns the reverse-proxy + hosting lifecycle** and automates it.

### 10.1 Proxy autopilot (Katban drives caddy)

- Katban detects caddy (binary present) and takes ownership of a **dedicated
  config block/routes** for Clawde sites. On the user's setup caddy is
  **bare-metal at `/etc/Caddy`**. **caddy mechanism (decision, §16a E8 — owned
  include file):** Katban manages a single file it owns (e.g. `katban.conf`),
  added to caddy via a **one-time `import`** line in the Caddyfile which the
  user approves once. Katban only ever adds/removes routes inside that file —
  tagged with comments — and tracks its own additions in memory
  (`katban sites show` for a diff-able view). On add/remove it runs
  **`caddy reload`** (instant, non-disruptive, no `systemctl`) to activate.
  Non-sweeping by construction: it never touches config it didn't write
  (`--no-caddy`, §17).
- Modes: **serve output of a build** (e.g. `vite`/static site output dir),
  **serve a plain static folder**, **live-reload a working dir** (file watch →
  push new content to browsers), and **preview Clawde's generated HTML
  artifacts** (dashboards, charts, mermaid diagrams, markdown-rendered docs, an
  in-browser visual/image editor) — "all of the above" from the interview,
  switchable per project.
- Katban writes its owned include file and reloads caddy (`caddy reload` — no
  `systemctl restart`), wires **Let's Encrypt** for real domains, and binds a
  **subdomain per project** (audit decision), e.g.
  `my_project.example.com`.
- **Subdomain creation (audit decision):** DuckDNS' free tier has no wildcard
  DNS, so Katban/Clawde **auto-create each per-project subdomain via the DuckDNS
  API** (pointing the record at your public IP) when a site is added; caddy then
  auto-certs it. A wildcard-capable DNS provider (pro domain) is an alternative
  that needs no per-subdomain calls.
- When caddy is absent (or `--no-caddy`), Katban falls back to its own axum
  static-serving + live-reload handler for LAN/local/one-off use (no domain). The
  caddy-managed path is the default for public/LAN web exposure.

### 10.1a caddy bootstrap & reload trigger path (concrete design)

**One-time bootstrap (`katban setup`, sudo, runs once — this is the only change
to the user's config):**

1. Create `/etc/caddy/katban.conf` (empty; Katban owns it).
2. Append a **top-level `import katban.conf`** to `/etc/caddy/Caddyfile` (in the
   site-block area, not inside the global options block). This is the *only*
   line added to the user's file.
3. Install a small **auto-reload watcher** (below) so future edits need no
   manual reload.
4. `systemctl reload caddy` once to pick up the import.

**Reload trigger — Katban runs as a host systemd service (the default, §11),
so it writes `/etc/caddy/katban.conf` directly and the host reloads caddy
itself.** Two mechanisms; the host-side watcher is the recommended default:

- **Primary (recommended) — host-side auto-reload watcher.** Katban only ever
  rewrites `/etc/caddy/katban.conf`; the host detects the change and reloads
  caddy itself. (For the container alternative, the same file is reached via
  a rw volume mount of that one file.)
  - A tiny **systemd path unit**: `katban-reload.path` watches
    `/etc/caddy/katban.conf` (`PathChanged`) and triggers
    `katban-reload.service`, which runs `systemctl reload caddy` (or
    `caddy reload --config /etc/caddy/Caddyfile` for non-service installs). It
    runs **on-demand only** when the file changes — not a daemon to forget —
    and is installed by the bootstrap. Equally valid: an `inotifywait`/
    `entr` watcher running the same reload.
  - This keeps the Caddyfile adaptation on the **host** (correct caddy binary
    and any custom modules), needs **no admin-API exposure**, and is the same
    `systemctl reload caddy` the user already trusts — but automated.
- **Fallback — admin API over a unix socket** (`admin unix//run/caddy/katban-admin.sock`,
  socket ACLs, no TCP). Katban adapts the Caddyfile to JSON and `POST`s it to
  the socket's `/load`. Caveat (documented): whoever adapts must use a
  compatible `caddy` binary, which breaks if the host uses custom caddy
  modules — so this stays a non-default alternative (mostly relevant to the
  container path). Note that `caddy reload --config /etc/caddy/Caddyfile` via
  the default localhost admin API was proven to work **without root** on
  TheDrone (2026-08-29).

**Safety semantics:**
- Katban writes `katban.conf` **atomically** (write temp file, then `rename()`)
  so a reload never sees a half-written file.
- `systemctl reload caddy` is graceful and atomic caddy-side: a bad template
  aborts the reload and caddy **keeps the last-good config** (no downtime);
  Katban surfaces any failed reload in `katban sites status`.

### 10.1b What one site looks like (per-site block shape)

The source of truth for a site is `~/.clawde/katban/sites/<site>.json` (name,
kind, folder, subdomain, publish/lock toggle). `katban.conf` is *generated*
from those, so the two never drift:

```caddyfile
# ---- KATBAN-MANAGED (do not edit by hand) ----
# live site (live-reload / preview): caddy proxies to Katban's handler
myproject.example.com {
    encode gzip
    reverse_proxy 127.0.0.1:8788   # Katban per-site live-reload handler
}
# static/published site: caddy serves the folder directly (no reload needed)
blog.example.com {
    encode gzip
    root * /home/user/projects/blog/dist
    file_server
}
# ---- END KATBAN-MANAGED ----
```

- **Live-reload / preview sites** proxy to Katban (it does the file watching,
  SSE push, and page auto-refresh, §16a E7).
- **Static / published sites** are served straight by caddy — no Katban in the
  path, less to go wrong.
- Katban adds the subdomain DNS record (DuckDNS API) and caddy auto-certs it on
  the next reload.

### 10.2 Lifecycle automation (the actual ask)

- `clawde katban site add <dir>` / `site remove` / `site list`: create/remove a
  hosted project, wire the route, reload caddy — no hand-editing configs, no
  `systemctl`.
- File watching + live reload on by default for a site; a "publish/lock" toggle
  stops auto-updating if the user wants a stable public snapshot mid-edit.
- Clean teardown on site remove (unroute + delete worktree/artifacts where
  appropriate), and orphan cleanup if Katban is stopped.

### 10.3 Preview surface

A `/preview/<project>` route renders whatever HTML artifact Clawde produced for
that project (or the live site itself), so "TUI not enough" is solved by a
browser tab. Same routing + auth bounds as everything else (a public preview is
only public in Public mode).

---

## 11. Storage, deployment & the `~/Katban` question (open item → recommendation)

The user raised: *if `~/Katban/compose.yaml` is optional, is storage per-project
or under `~/.clawde`?*

**Recommendation (engineering pick): treat deployment and state as separate
concerns and follow Clawde convention.**

- **Deployment (default): a host systemd service.** `guest expose` / `site
  expose` render `katban.service` (User=the admin OS user, never root;
  `Restart=always`; runs `clawde katban guest serve` on loopback) plus the
  `katban-reload.path`/`katban-reload.service` units into
  `~/.clawde/katban/caddy/`; the one-time bootstrap installs them with
  `sudo systemctl enable --now katban.service` and
  `katban-reload.path`. Rebuild the binary in place + `sudo systemctl
  restart katban` to update. Rationale (§7/§10.1a): Katban is a control plane
  for the host (caddy include file, sites, provider keys, git worktrees) and
  caddy is bare-metal systemd here; the container would only add a bridge.
- **Deployment (alternative): `~/Katban/compose.yaml`** — packaging for the
  all-container path: **katban** container (+ optional **caddy** sidecar for
  fresh installs) + **searxng** guest-search sidecar, `restart:
  unless-stopped`. Kept for fresh/portable installs; caddy stays bare-metal
  and is driven via Katban's **owned `katban.conf` include + `caddy reload`**
  (§10.1) either way.
  This file is **not where state lives.**
- **State lives under Clawde's config dir**, defaulting to
  `~/.clawde/katban/`:
  - `auth.json` — encrypted guest/admin/device secrets + per-link policies.
  - `boards/<project-hash>/board.json` — board state per project
    (mirrors Cline's `workspaces/<project>/boards.json`; per-git-root keyed
    like `session_storage` buckets).
  - `audit.log` — append-only tamper-evident admin/action log.
  - `sites/` — dev-site definitions + caddy blocks.
  - `scratch/` — throwaway guest scratch workspaces (destroyed after use).
- **Per-project override**: a board may live at the repo's `.clawde/katban/
  board.json` and travel with the code — same pattern as `.clawde/modes/` and
  `.clawde/output-styles/` (global default, project overrides).
- Note: the gateway and TUI already read `~/.clawde/settings.json`;
  `~/.clawde/katban/` is additive, not a replacement.

This resolves the interview open item by giving a single answer: **per-project
board state under `~/.clawde/katban/`, project-overridable to the repo; caddy/
compose are deployment not state.**

---

## 12. Katban agent execution (research-backed engineering decision)

Interview answer was "I have no idea — does one work better for Clawde? …investigate
hardware?" Hardware is *not* the deciding factor; Clawde's supported model is.

**Decision: per-card headless clawde subprocess in its own git worktree —
Cline Kanban's model, and Clawde-native.** Rationale:

- Clawde already has first-class worktree isolation (`EnterWorktree`/
  `ExitWorktree`, `isolation:"worktree"`, worktree-sandboxed verify, `/move`,
  worktree-keyed snapshots) — the Cline model maps directly.
- `clawde --print`/`--resume`, `--output-format stream-json`, `--allowed-tools`,
  `--permission-mode`, `/keys`, etc. make a spawnable card runner trivial.
- It gives automatic per-card isolation, git review, commit/PR, independent
  lifecycle — exactly Cline Kanban's value (parallel agents without conflicts).
- The **on-demand web requests** (guest chat, admin quick-ask from the UI) route
  through the existing **gateway agent loop** (`gateway/src/agent.rs`), reusing
  one agent engine. Board cards and on-demand requests therefore both exist but
  are intentionally distinct paths (§4).

A hybrid is what the spec pins: **worktree cards spawn headless clawde; web
on-demand chat runs the gateway loop.**

**Multi-agent orchestration is dev/admin-only (audit constraint).** Parallel
cards, the dependency DAG, worktree spawning, and the board's coordinator
machinery are used for Katban itself and other admin dev work — they are
**never exposed to guests** (guests only ever get chat + WebSearch, §6.1; see
the §7b allowlist + red-team guard for why).

---

## 12a. Board data model (what gets saved, `boards.json` per project)

One `boards.json` per project under `~/.clawde/katban/boards/<project>/`
(mirrors Cline's shape, extended with our rules from §16a E2–E6):

```jsonc
{
  "version": 1,
  "columns": [
    { "id": "backlog",     "title": "Backlog",     "cards": [/* see card */] },
    { "id": "in-progress", "title": "In Progress", "cards": [] },
    { "id": "review",      "title": "Review",      "cards": [] },
    { "id": "trash",       "title": "Trash",       "cards": [] }
  ],
  "dependencies": [ { "from": "<cardId>", "to": "<cardId>" } ],  // cycle-checked on add
  "settings": { "parallelCap": 3, "failPolicy": "block", "autoRetry": 2 }
}

// one card
{
  "id": "…", "prompt": "…",
  "status": "queued | running | blocked | failed | review | done",
  "badges": ["queued", "blocked", "failed"],   // §16a E2
  "branch": "katban/<slug>", "worktree": "…", "baseRef": "main",
  "createdAt": 1774672842329, "updatedAt": 1774673005471
}
```

- **Columns stay Cline's four**; the badge carries extra state (waiting for a
  slot, waiting on a dependency, failed) so the board reads at a glance.
- **Status is derived, not typed twice**: `blocked` = an unmet/failed
  dependency; `queued` = ready but no free parallel slot; `running`;
  `review` = finished or needs your attention; `done`/trashed.
- **Dependencies are cycle-checked at add-time** (§16a E4) — a cycle is refused
  with a warning naming the two cards.
- Card content = transcript + live diff recomputed on demand (§16a E5);
  worktree is destroyed when a card is trashed.

---

## 13. Tech stack & crate layout (proposal)

- New crate **`clawde-katban`** (axum, reusing the gateway's HTTP plumbing
  patterns: auth, error envelope, SSE, graceful shutdown); CLI wiring via
  `clawde katban` in `clawde-cli`.
- Web UI: a **bundled static web app** served by axum (Clawde ships no Node
  runtime today; Cline ships a React app). Choose the lightest approach that
  gives the board + chat + preview — e.g. a small compiled/bundled frontend
  (this is where "open to other projects" can add a Vibe Kanban / Agent Kanban
  look). WebSocket for live card/chat/proxy updates, with **Origin + token
  checks** (§3b).
- Reuse: `clawde-core` (config, PermissionManager, AutoApproveMode, snapshot/
  worktree), `clawde-query` (`run_agent_loop` seam or headless card runner),
  `clawde-gateway` (agent loop, error envelope, rate-limit patterns), free
  cascade (`free/auto`), `clawde-acp` TLS/bind guards as a copy-pattern.
- Add a **file watcher + live-reload** service (per dev-site) and a **caddy
  driver** (write + merge an owned `katban.conf` include, then `caddy reload`;
  bare-metal `/etc/Caddy` case).
- **Board runtime**: one multiplexed WebSocket with per-message capability
  checks (§16a E13); per-card agents stream events (status, tool calls, live
  diff); card artifacts = transcript + live diff recomputed on demand (E5).
- Guest **search client**: thin typed Rust client (`reqwest`+`serde`) against
  the SearXNG sidecar's standard JSON API (URL-encoded query, engine UA, capped
  results), or the public Parallel Search MCP server as documented alternative.
- **Runtime**: systemd-service-by-default (`katban.service`, §11); SearXNG
  sidecar is the one always-on container; compose at `~/Katban` is the
  documented alternative. TUI-driven setup is the primary control surface per
  §5.
- **Audit log**: append-only, hash-chained (§8); storage under
  `~/.clawde/katban/audit.log`.

---

## 14. Security hardening checklist (hard stops)

- [ ] Every WS/HTTP route validates `Origin`; sockets require capability tokens.
- [ ] Loopback bind by default; `0.0.0.0` needs explicit `--allow-non-loopback`.
- [ ] No default-YOLO posture anywhere (unlike Cline's
  `agentAutonomousModeEnabled: true`). Guest = zero tools; admin = confirm on
  destructive.
- [ ] No guest access to real projects/files/bash/web-fetch/WebFetch/env/git.
- [ ] Guest + admin tokens encrypted-at-rest; httpOnly/SameSite/secure cookies.
- [ ] Guest brute-force: rate-limit + lockout; noise on account-exists probes.
- [ ] Secrets/keys never leak in responses/logs/UI (reuse gateway guarantee).
- [ ] Refuse to run the guest/jail surface as root, and refuse
  `--dangerously-skip-permissions` semantics for any exposed tier.
- [ ] No budget/rate caps for admin by design; strict concurrency+turn/token
  caps for guests.
- [ ] Audit log append-only + **hash-chained**; board/site storage follows
  `~/.clawde/katban/` (§11).
- [ ] Tool-result sanitization + intermediate-system parsing at every agent
  boundary (prompt-injection defense from the gateway spec).
- [ ] Guest WebSearch runs on the **dedicated sandboxed endpoint** (SearXNG
  sidecar by default), never on host search/secrets.
- [ ] Guest sessions are ephemeral in-memory only + optionally a downloadable
  AI session summary; **no filesystem scratch dir** in v1.
- [ ] Public sites get **subdomain-per-project** + auto DNS creation (DuckDNS
  API) + per-subdomain Let's Encrypt via caddy.
- [ ] Katban is always-on by default (systemd `katban.service`; compose
  alternative for fresh installs); never a foreground-only process.
- [ ] Guest-path **red-team eval** (AgentDojo-style, §7b) passes before any
  public exposure: exfiltration, real-project read, WebSearch-as-open-proxy,
  and DoS all fail.
- [ ] Untrusted content (esp. WebSearch results) is **classifier-screened**
  before entering the guest model's context; data is never treated as
  instructions (OWASP LLM01 / Anthropic classifier approach).
- [ ] **Default-deny egress + ephemeral guest lifecycles**; no persistent
  guest state.
- [ ] Guest tool surface is an **allowlist only**; host/MCP tools are never
  surfaced to guests (tool-poisoning guard, arXiv 2603.21642).
- [ ] **Project and Site are distinct**; a Site needs no git repo (§16a E1).
- [ ] Board state machine = **4 columns + status badges**; dependency cycles
  are **detected and refused** with a warning naming the offending cards (E4).
- [ ] Dependency failure **blocks dependents (no auto-cancel)**; cards retry
  only on **transient** errors — never user/invalid/content-filtered (E6).
- [ ] **caddy** is changed only through Katban's owned include file +
  `caddy reload`; admin-API full-replace is avoided to keep other config safe
  (E8).
- [ ] Guest session summary is generated **on demand at download only** (E9).

---

## 15. Phased roadmap (sequenced by dependency; user will reorder)

1. **Phase 0 — Safety gate proven first**: the guest-safe shell (guest chat +
  WebSearch-only, hardened lightweight jail default, shared password + per-device
  tokens + lockout, Origin/token checks, deny-write). This is the trust boundary;
  everything else assumes it. (Smallest safe first slice, like the modes-spec
  did with undo-as-recovery-net.) **The AgentDojo-style guest red-team eval
  (§7b) passing is the exit gate for Phase 0 — no public exposure until then.**
2. **Phase 1 — Local/LAN dev-site hosting engine**: `clawde katban site …`,
  caddy autopilot + Let's Encrypt + live reload, static/build/serve/preview
  modes, axum fallback. Solves the "restart caddy/systemctl" pain for the user
  themselves.
3. **Phase 2 — Katban board**: board server + bundled web UI; per-card headless
  clawde in worktrees; 4-column + badge state machine; dependency DAG with
  **cycle detection (refuse to create)**, **transient-failure auto-retry**, and
  **block-dependents on failure**; diff review w/ inline comments; commit/PR;
  trash cleanup; control menu; project/worktree scoping (OpenCode model).
4. **Phase 3 — Public tiers**: expose guest tier (from Phase 0) publicly +
  live public site + admin public access behind correct auth; per-link policies;
  Docker compose path under `~/Katban` as an alternative to the jail default;
  provisioning docs.
5. **Phase 4 — Hardening + docs**: security review pass, audit log UX,
  dulnerability cases (WebSocket hijack test), README/docs, tests.

Actually the user "spec the whole thing; let me pick order" — this order is a
recommendation; any Phase can be reordered by the user at build time.

---

## 16a. Decisions locked in the audit rounds

| # | Question | Decision |
|---|---|---|
| A1 | Deployment target | Home server (the box you run on; here `TheDrone user@192.168.1.55`); base domain `example.com`, projects at `my_project.example.com`. |
| A2 | Public path | User-configured self-hosted side; caddy (bare-metal, `/etc/Caddy`) terminates TLS in front of Katban. |
| A3 | Katban runtime | **Always-on systemd service** (`katban.service`, rendered by expose; `Restart=always`, non-root user) by default — flipped from container after the caddy mechanics were proven live; SearXNG is the one always-on container; compose at `~/Katban` is the documented alternative for fresh installs. Not foreground. |
| A4 | Site addressing | **Subdomain per project** (`<project>.example.com`). |
| B1 | Runtime stack | systemd `katban.service` + SearXNG guest-search container sidecar; existing bare-metal caddy driven via owned include file (`katban.conf`) + `katban-reload.path` watcher; optional full compose stack (katban + searxng [+ caddy sidecar]) for fresh installs. |
| B2 | Board scoping | One board per git repo/project. |
| B3 | Ship flow | Review diff in board → **Commit + auto-Open PR** (GitHub/GitLab). |
| B4 | Parallel cap | Configurable cap (default ~3) with auto-queue when a slot frees. |
| C1 | Guest scratch | **Dropped for v1** — guests get ephemeral in-memory working memory (chat + notes) only, destroyed on end; optional downloadable AI **session summary**. No host-filesystem access. |
| C2 | Guest search | **Dedicated free sandboxed endpoint by default** — SearXNG sidecar (reqwest+serde typed client); Parallel Search MCP documented alternative. |
| C3 | Control surface | TUI-driven setup primary; minimal `/katban` admin web route for runtime toggles only. |
| C4 | Subdomain DNS | Auto-create per-project subdomains via **DuckDNS API** (free tier has no wildcard); wildcard DNS as alternative. |
| D1 | Guest memory | Ephemeral working memory + downloadable AI session summary; no scratch dir (§6.1). |
| D2 | Audit log | Append-only with **hash chain**; size rotation; retained by default, optional cap (§8). |
| E1 | Project model | Project and **Site are distinct**; a Site needs no git repo (sites are hosted targets; Projects only when you want a board/worktree). |
| E2 | Card states | Cline's 4 columns + **status badges** (queued/blocked/failed); queued cards wait on the parallel cap (engineering call). |
| E3 | Dependency failure | **Block dependents, no auto-cancel** (configurable per board); the failed card is reviewed before dependents proceed. |
| E4 | Dependency cycles | **Detect + refuse to create** a cycle, warning that names the offending cards; watchdog flags any cycle that appears via non-UI paths. |
| E5 | Card artifacts | **Cline-style**: transcript + live diff recomputed on demand; Clawde's snapshot layer available to build `/undo` later. |
| E6 | Retry policy | Auto-retry on **transient** failures (quota/429, timeout, upstream 5xx/empty) a few times w/ backoff; **never** retry user/invalid/content-filtered (reuse `should_fallback` semantics). |
| E7 | Live updates | **Full-page auto-refresh on save** (SSE push → browser tab reload); hot-swap later if needed. |
| E8 | caddy mechanism | **Owned include file** (`katban.conf`, one-time `import` you approve) + `caddy reload`; only add/remove Katban's own tagged routes; admin-API full-replace avoided (risk). |
| E9 | Session summary | Generated **on demand at download**, using the free providers; no cost unless a friend asks. |
| E10 | Link schema | All guest links get the **free sandboxed search by default**; per-link knobs = max turns + expiry (+ optional device cap). |
| E11 | Token format | **Opaque random** tokens, server-side store (instantly revocable). |
| E12 | Config home | Own **`~/.clawde/katban/katban.json`**, TUI-written. |
| E13 | WS topology | **One multiplexed socket** + capability tokens per message. |
| E14 | Guest scope (confirm) | Friends = **chat + anon/free web search only**; the Kanban/board is **admin/dev-only** (§6.1, §12). |
| E15 | Admin confirm gate | Applies on **every** network incl. loopback — "777" is a posture, not a perimeter (§6.2). |
| E16 | Default limits | Parallel tasks ~**3**; transient retries **2**; guest chat **no turn limit** (user decision); links expire **30 days** (or never); guest concurrency **2** (safety cap). All changeable later (§5a). |

## 16b. Still to finalize at build time (smaller items)

- Exact bundled-web-UI approach (small compiled frontend vs server-rendered
  controls) — **resolved 2026-08-29 (§20.7): thin hand-rolled frontend
  first, escalate to a bundled React app only if diff-review needs it.**
- Whether `clawde katban` shares a port/domain with `clawde serve` or uses its
  own (recommend: separate server + routes, own port).
- Board JSON schema is now designed (§12a: 4 columns + badges, derived status,
  cycle-checked dependencies). Remaining: the exact `/katban` route surface.
- caddy bootstrap + reload path is now designed (§10.1a: owned include file,
  host-side systemd-path reload watcher primary, unix-socket admin API
  fallback). Remaining there: auto-detect official `caddy.service`
  `ExecReload` vs bare `caddy reload` at bootstrap. Wildcard-capable DNS
  (DuckDNS pro) vs per-subdomain API automation fallback.
- SearXNG image/port; exact Parallel Search MCP alternative wiring. (Guest
  limits: no turn limit; link expiry default 30 days — §16a E16.)

---

## 17. Risks & guardrails

- **Do not erode the safety core**: the spec-mode write gate, verify sandbox,
  sanitization, cancellation, and permission classifiers stay authoritative;
  Katban layers on top and never bypasses them for an exposed tier.
- **Cline-Kanban WS lesson**: treat every socket/client as hostile until
  Origin+token validated; add a dedicated WebSocket-hijack regression test early.
- **Shared-secret cascade**: shared guest password is a single point of failure;
  per-device tokens + lockout + per-link revocation mitigate but do not remove
  it — document the tradeoff and default guest to zero tools.
- **Token burn on free cascade**: guest concurrency/turn caps required to keep
  free upstreams from draining; no budget caps for admin.
- **caddy ownership**: Katban only ever edits its owned `katban.conf` and writes
  it **atomically**; reloads are graceful and atomic (a bad template keeps the
  last-good config, no downtime); the host-side watcher runs `systemctl reload
  caddy`, not a restart, and `--no-caddy` disables caddy entirely.
- **State separation**: keep `~/Katban/compose.yaml` (deployment) distinct from
  `~/.clawde/katban/` (state) to avoid confusion when compose is optional.

---

## 18. Out of scope (v1)

- Replacing `clawde serve` / adding OpenAI gateway auth tiers inside the gateway
  crate (Katban is its own server; it may call the gateway loop).
- Machine-learning-based intent analysis / agentic IAM tooling (noted as future).
- Multi-user role graph beyond guest/admin.
- Replacing caddy with a from-scratch TLS server as the default for public sites
  (caddy-managed is default; axum fallback is LAN/local-only).
- Phone/mobile native apps (a responsive web UI covers it).

---

## 19. TUI control surface — the living Katban menu (added 2026-08-29)

Katban is controlled from inside Clawde in two always-in-sync ways, so admin
work never requires leaving the TUI and the surface grows as Katban grows:

### 19.1 `/katban` slash command

Registered in `clawde-commands` (`commands/src/katban.rs`), listed in
`PROMPT_COMMANDS`, and given argument-level autocompletion (subcommand names,
then live link IDs / locked-IP values read from the store — the `/keys`
convention: values carry the typed path prefix, `strip_typed_path` trims it
for display). Subcommands mirror `clawde katban` 1:1:

```
/katban                      status overview
/katban status               status overview
/katban link list            list guest links
/katban link create <name>   create a link (prints the password once)
/katban link show <id>       link details (devices, expiry)
/katban link revoke <id>     revoke a link
/katban link password <id>   rotate a link's password (prints the new one once)
/katban guest unblock <ip>   clear lockouts + permanent blocks
/katban site list            hosted sites
```

Everything mutates the same `~/.clawde/katban/links.json` / config the CLI and
guest server use, so the running server picks changes up via its on-disk store
reload (`maybe_reload_store`) — no restart needed.

### 19.2 Alt+G controls menu (hotkey)

`alt+g` (`openKatbanControls`, added to `default_bindings` as a Global
binding; configurable in `keybindings.json` like every other key) pops a
centered scrollable menu built live from the guest store each time it opens:

- **Status** — Katban overview row (`/katban status`)
- **Guest links** — list, create (seeds the prompt for a name), plus one
  *Rotate password* and one *Revoke* row per **live** link (state + expiry;
  revoked/expired links are skipped — dead links stay visible via
  `/katban link list`)
- **Locked IPs** — one *Unblock* row per locked/permanently-blocked IP

Complete rows (a specific link id or IP already in the row) submit the seeded
`/katban` command immediately on Enter; the create row just seeds the prompt
so the user types the link name. Esc closes; ↑/↓ (+ vim j/k) navigate, headers
are skipped, long lists scroll (PageUp/PageDown). Implementation:
`tui/src/katban_controls.rs`, wired into `App` (field, key handling,
`any_modal_open`, click-outside dismissal, render) and re-exported from
`tui/src/lib.rs`.

### 19.3 Boards section (added 2026-08-29)

The menu gained a **Boards** section (between Guest links and Locked IPs),
built live from the default board:

- **List cards** — `/katban board list`
- **Cards ready to run** — `/katban board ready` (respects the parallel cap)
- **Add a card** — seeds `/katban board card add ` for the user to type the
  prompt
- **Advance — <prompt preview>** — one row per non-done card; runs
  `/katban board card set <id> <next>` where `next` comes from
  `CardStatus::next()` (backlog→queued→running→review→done; blocked/failed
  retry to queued; done is terminal)

`/katban` gained the matching board subcommands (`board list`, `board ready`,
`board card add|set|remove`) with card-ID and status autocompletion, and the
CLI already had them — all three surfaces (TUI menu, slash command, CLI) stay
in sync. Verified live: opened the menu, advanced a card from the menu, and
the board file on disk flipped to the next status.

**Living rule:** when Katban gains a feature, add the corresponding `/katban`
subcommand and (optionally) a menu row that maps to it — the menu and the
command surface stay in sync by construction, and both stay reachable from
the TUI and headless (`clawde -p "/katban ..."` runs the same command path
via the registry).

### 19.4 Audit findings (2026-08-29)

A gap/bug audit of §19.1–19.3 found and fixed:

- **Multi-word link names broke.** `/katban link create summer crew friends`
  fell into the "unknown subcommand" error because the execute arm only
  matched a single-word name — the exact input the menu's *Create a guest
  link* row invites. The arm now joins the rest of the args; the CLI
  (`clawde katban link create`) mirrors this (name = everything before the
  first flag). Both get a helpful error when the name is empty.
- **Dead links got management rows.** The menu offered *Rotate password* /
  *Revoke* rows for revoked and expired links; rotating a dead link's
  password succeeds silently (footgun). The menu now filters to
  `guest::link_active` links; dead links remain visible via `/katban link
  list`.
- **`--project` was half-wired.** The board helpers and completion plumbing
  took a `project` parameter but every call site passed `None` and
  `load_board_cards(_project)` ignored its argument — `/katban board ...`
  could not target a non-default board while the CLI could. `--project
  NAME` / `--project=NAME` now parses anywhere in the args (same rules as
  the CLI), threads through execute *and* completions (including
  `--project <TAB>` project-name completion), and the dead parameter was
  removed.
- **Docs gaps.** `docs/keybindings.md` now lists `Alt+G`
  (`openKatbanControls`); `docs/commands.md` gained a Self-Hosting & Katban
  section documenting `/katban`.
- **Cosmetic.** `status_text` prints `boards: none` instead of a dangling
  blank line when no boards exist.

### 19.5 Guest server security audit (2026-08-29)

A hardening audit of the guest chat surface (auth, rate limiting, prompt
isolation) found and fixed:

- **`X-Forwarded-For` was trusted unconditionally.** A direct client (LAN
  mode, `--allow-non-loopback`) could set the header itself and rotate it
  per wrong password — defeating the entire lockout ladder. The server now
  serves with `connect-info` and only trusts `X-Forwarded-For` from a
  loopback peer (caddy on the same host); a non-loopback peer is identified
  by its TCP address and can't spoof the bucket.
- **Origin check ignored the port.** Same-host-different-port origins
  (`localhost:9999` → `localhost:8789`) passed as "same origin". The check
  now compares host AND port for loopback and same-host cases, closing
  local-page CSRF while keeping the public-subdomain + proxy paths working.
- **Cookie lacked `Secure`.** Behind caddy (HTTPS public URL) the device
  cookie was sent with `HttpOnly; SameSite=Strict` but no `Secure`; a guest
  hitting an http:// URL would send it in cleartext. The flag is now set
  when `X-Forwarded-Proto: https` (set by caddy), so loopback http:// dev
  still works.
- **Unbounded device-token list.** Every login appended a device row to
  `links.json` forever; a busy link grew without bound. `mint_device_token`
  now caps per link (`MAX_DEVICES_PER_LINK = 20`, oldest dropped).
- **Sessions never evicted.** `LiveSession`s (up to 40 messages each) leaked
  in memory for revoked links and abandoned tokens. Idle sessions are now
  swept (`SESSION_TTL_SECS = 24h`, rolling last-used) on each chat/summary
  access.
- **No throughput rate limit.** The concurrency cap (2 simultaneous) never
  stopped a script with the shared password from hammering `/api/chat` in a
  tight loop, burning the host's free-tier quota and SearXNG. Per-link
  fixed-window rate limits now cap chat (`CHAT_MAX_PER_MINUTE = 20`) and
  summary generation (`SUMMARY_MAX_PER_MINUTE = 2`), each an independent
  bucket.
- **Prompt-injection screen was thin.** The search-result blocklist missed
  common instruction-steering phrasings and never screened result URLs.
  Patterns expanded (ignore-your-instructions, override-your-system-prompt,
  reveal/show/forget instructions, act-as-if, jailbreak, …) and URLs are
  screened too.

Test counts: 78 katban (was 69) — new: XFF-spoof rejection, port-aware
origin, chat rate limit, session sweep, device cap, secure cookie flag,
proxy-secure auth. Clippy + workspace green.

### 19.6 Site-hosting audit (2026-08-29)

A hardening audit of the dev-site surface (host server, live reload, caddy
blocks) found and fixed:

- **Reloader + bootstrap hardcoded `/etc/caddy`, ignoring `--caddy-dir`.**
  `site expose --caddy-dir X` wrote the managed config to `X/katban.conf`,
  but `katban-reload.path` watched `/etc/caddy/katban.conf` (never changed →
  reloads silently never fired) and the bootstrap told the user to `import
  katban.conf` into `/etc/caddy/Caddyfile` (imports a file that was never
  written). The reloader path unit now watches the exact managed file, the
  import line uses the absolute path, and `bootstrap_instructions` takes the
  chosen caddy dir.
- **`katban.service` hardcoded the guest port.** `guest expose --port N`
  rendered `reverse_proxy 127.0.0.1:N`, but the always-on unit ran `guest
  serve` on the default 8789 → the public URL 502'd. The port is now
  persisted in the store (`guest_port`) and threaded into the unit
  (`guest serve --port N`), so every future `site expose`/`guest expose`
  regenerates a consistent unit.
- **Caddy config injection via unvalidated names/subdomains.** Site names
  (the caddy hostname fallback) and public subdomains flowed verbatim into
  `katban.conf`; one malformed value (`demo } reverse_proxy evil {`)
  injected directives or broke every exposed site at reload. New
  `caddy::valid_hostname` gate (letters/digits/`.`/`-`/`_`, no caddy syntax,
  no path separators) rejects bad values at `site add` and both expose
  commands.
- **No `nosniff` + full-file reads.** Unknown-extension files were served
  with no Content-Type (browsers could MIME-sniff a text file into HTML),
  and every file was read fully into memory (a multi-GB asset OOM'd the dev
  server). All responses now carry `X-Content-Type-Options: nosniff`, and
  non-HTML files stream via `tokio-util` `ReaderStream` with an explicit
  `Content-Length` (HTML keeps read+inject for live reload).
- **Watcher reload storms.** The 500 ms poll walked the whole tree and any
  write under `node_modules`/`.git`/`target` in a served project root
  reloaded every browser tab. Those subtrees are now skipped wholesale
  (`SKIP_DIRS`).
- **Unit user could be root.** `$USER` under sudo rendered `User=root`,
  defeating the spec's "never root" posture. `unit_user()` falls back to the
  owner of the katban data dir (from `/etc/passwd`) when invoked as root and
  refuses otherwise.
- **Cosmetic.** `site remove` now names a remaining exposed site (or points
  at `guest expose`) to regenerate the managed config, and `site expose`
  warns when a live site's port isn't listening (visitors would 502).

Also verified non-bugs: the watcher does NOT follow symlinks
(`DirEntry::metadata` never traverses), so no cycle crash or outside-root
walk; `host::resolve` already blocks symlink escapes via canonicalization.

Test counts: 81 katban (was 78; +1 nosniff, +1 skip-dirs, +1
valid_hostname) + 53 cli (+1 units test). Clippy + workspace green.

### 19.7 Board + dependency audit (2026-08-29)

A gap/bug audit of the board surface (cards, dependencies, cycles, parallel
cap) found and fixed:

- **Review and manually-blocked cards could auto-restart.** `ready_to_run`
  only excluded Running/Done, so `queued_ids` handed a card sitting in
  **Review** (work done, awaiting a human) or **Blocked** (admin said hold)
  right back to the scheduler — the future runner would have started it a
  second time. Ready now means Backlog/Queued/Failed only: failed auto-retries
  (the "retry automatically" decision; a retry counter belongs to the runner
  slice), review and blocked never restart on their own.
- **Project names collided under `slugify`.** `--project "My Repo"`,
  `--project "my-repo"`, and `--project "my_repo"` all mapped to
  `boards/my-repo/board.json` and silently shared one board;
  `existing_projects()` even returned the slug, not the real name. Board
  directories now use a **lossless encoding** (`project_dir_name`): safe
  characters verbatim, everything else `%XX` — injective, can never produce
  `..` or a separator — with `project_name_from_dir` decoding names back for
  display (deduped). Existing simple-name dirs (`default`, `my-repo`) are
  unchanged.
- **Card ids derived from the wall clock.** `now * 1000 + len` collided when
  the clock rewound (NTP) or after 1000 cards in one second, and ids were
  predictable. Cards now get random 8-byte hex ids (same scheme as guest
  links); every consumer treats ids as opaque strings.
- **Dependencies unreachable from inside Clawde.** The CLI had `board
  link`/`unlink` but `/katban` (and the Alt+G menu) didn't, so the
  cycle-checked dependency machinery was CLI-only. Added `/katban board link
  <A> <B>` / `board unlink <A> <B>` with card-id completions (first id, then
  second id), cycle errors surfaced with the "loop forever" explanation, and
  a **Link cards** row in the Alt+G menu that seeds the command.
- **Cosmetic.** The cycle error double-prefixed "cannot link:" (board error
  + CLI wrapper); the board error no longer carries the prefix.

**Concurrency (fixed 2026-08-29):** the board now takes an exclusive advisory
lock across every read-modify-write — a per-project `board.lock` via `flock`
(`board::BoardLock::acquire`) — so separate processes (CLI, TUI `/katban`,
and a future agent-runner) serialize instead of last-writer-wins overwriting
each other. `flock` auto-releases on process death, so a crash never leaves a
stale lock; on non-Unix the guard degrades to a no-op (the board is a
Linux-server surface). Verified live: a CLI board write blocks while another
process holds the lock, then lands on release. This was the "noted, not
fixed" parallel-writer risk flagged in the first pass of §19.7.

(Before this, the audit's other fixes: Review/Blocked cards no longer
auto-start, slug-colliding project names became lossless, clock-derived card
ids became random, and `/katban board link`/`unlink` + Alt+G rows added.)

Test counts: 86 katban (was 85; +1 board-lock exclusivity) + 10 katban-command
(was 8) + 53 cli + 1120 tui. Clippy (`-D warnings`, whole workspace) +
workspace green.

---

## 20. Web board design — Cline Kanban mapping (added 2026-08-29)

How Cline Kanban's three surfaces (cards, dependencies, diff review) would map
onto Katban's already-built backend — a design sketch, not a build plan. The
goal: a browser board for the **admin only** (§16a E14 — guests never see it),
served by the same katban server, with the guest chat staying a separate axum
app that has no routes to the board (the isolation that exists today is
preserved by construction).

### 20.1 What Cline Kanban actually is (ground truth)

`npx kanban` from the root of any git repo → a local web server + browser UI.
Each card gets its own ephemeral **worktree** and **terminal**; hooks stream
the latest message/tool call onto each card so you can monitor many agents at
a glance. Cards are made manually or by asking the sidebar agent to decompose
work (board-management instructions are injected into that session).
**⌘-click** a card to link it to another; when a card is completed and moved
to trash, linked tasks **auto-start**. Click a card → the agent's TUI + a
**diff of all changes in its worktree**; a checkpointing system gives a diff
from your last messages; click lines to **comment and send back to the
agent**. Then Commit / Open PR (dynamic prompt, merge-conflict handling), or
auto-commit / auto-PR for autonomous chains. A git navbar (history, branch
switch, fetch/pull/push) rounds it out. Gitignored dirs (node_modules) are
symlinked into each worktree to skip slow installs.

### 20.2 What already maps — zero new code

| Cline Kanban | Katban today |
|---|---|
| Card model | `board::Card` — and its `branch` field already exists, waiting for worktrees |
| ⌘-click linking | `add_dependency` (cycle-checked at add, error names both cards) |
| Completed → dependents auto-start | `ready_to_run` / `queued_ids`: dep `Done` → card ready; `trash_card` = Done |
| Run many in parallel | `parallel_cap` (default 3, §16a E16) + `queued_ids` |
| Per-repo boards | `--project` + lossless project dirs + the new `BoardLock` |
| Sidebar chat decomposes work | `/katban board ...` is already first-class in Clawde — the model can add/link/start cards by running the same commands |
| "Test your app" script shortcut | `site serve` + live reload (built, §10) |

### 20.3 What a web board adds (the real gaps)

1. **An admin board API + static app.** The board has **no HTTP surface today**
   (data + CLI/TUI only). This is the biggest gap: a `board` route set on the
   katban server, behind **admin session auth** (guest auth — password, opaque
   device tokens, origin checks, rate limits — is the template; the admin tier
   swaps the shared guest password for a per-admin session and keeps the §16a
   E15 confirm gate for destructive actions).
2. **Live updates.** §16a E13 pins one multiplexed WebSocket + per-message
   capability tokens. The existing site live-reload is one-way SSE
   (`host.rs`) — a valid fallback for v1 of the board (board → browser only).
3. **Diff review.** §16a E5: card artifacts = transcript + live diff
   recomputed on demand. Requires worktree + git plumbing (new, katban has no
   git dep yet) plus the runner slice. Cline's **checkpoint diff** maps to
   Clawde's snapshot layer (already cited in §12 for `/undo`) rather than git.
   Cline's **resume ID** maps to clawde's `--resume` — trashed cards stay
   resumable.
4. **The runner** (§12): per-card headless clawde subprocess in its own
   worktree. The play button = `card set running` + spawn; nothing executes
   cards today (`katban.service` runs only guest serve).
5. **Commit / PR + auto-commit chains** (§16a E5/E6): the dynamic-prompt
   commit flow and merge-conflict handling.

   **Implemented as Option B — "pin then merge-or-discard"** (§20.3 → commit
   slice landed with the runner): the runner commits a successful card's work to
   its branch (`katban/<id>`) at finalize and records the hash in `card.commit`, then
   tears the worktree down — so no long-lived checkouts (no GC). Review is now a
   merge-or-discard decision: **`card merge`** fast-forwards/merges the pinned
   branch into the project's current branch and deletes it (dependents unblock via
   readiness); **`card remove`/`archive`** deletes the branch. Merge conflicts
   abort with a clear error and the card stays in review for manual resolution.
   Surfaces: CLI `board card merge`, `/katban board card merge`, and a web
   **merge** button on review cards (`POST /api/board/{p}/cards/{id}/merge`). A
   genuine PR (push + `gh`) is still future work — Option B stops at landing the
   change locally.
6. **Status projection.** The live board renders `CardStatus` as Cline's
   columns (backlog/queued/running → In Progress; review; done/trash →
   Trash). `blocked` stays derived (unmet/failed dep), `queued` = ready but
   no free slot — the §12a column+badge view is a **projection** of the flat
   status enum, not new state.

### 20.4 Suggested phasing

1. Admin board API + read-only web board (cards, deps, status, ready list) —
   reuses `host.rs` static serving + guest auth patterns.
2. Live updates (WS per E13, or SSE as the v1 fallback).
3. Runner (worktree spawn) + live status/tool-call streaming onto cards
   (Cline's hooks).
4. Diff review + line comments + Commit/PR.
5. Auto-commit dependency chains (the "magical" end-to-end autonomy).

### 20.5 Still to confirm (carryover from §16b)

- Bundled web-UI approach: a small compiled frontend vs server-rendered
  controls — recommend the lightest viable (Clawde ships no Node runtime).
- Own port/routes for the board vs sharing the site server — recommend a
  separate axum app on its own loopback port, caddy-proxied only for the
  admin subdomain.

### 20.6 Existing code to reuse (research, 2026-08-29)

**Licensing lens (user decision, 2026-08-29): none — "don't care, pick the
best code."** The repo declares GPL-3.0, which would make AGPL incorporation a
legal problem *if the result is ever published*; the user has explicitly
chosen not to treat that as a constraint. So AGPL projects (Planka, Vikunja,
Focalboard, Taiga) are no longer excluded by license — they're judged on
fit like everything else, with the AGPL-vs-GPL publishing conflict noted as a
future concern only if the code is ever released. Surveyed:

| Project | Stars / forks | License | Verdict for Katban |
|---|---|---|---|
| **cline/kanban** | 1.3k / 306 | Apache-2.0 | **Primary reference.** Actively developed (pushed 2026-08). Its React frontend (columns, ⌘-click deps, diff review, checkpoints, worktree-per-card) is exactly the UX §12 already pins. No popular fork exists (top forks are 1–3 stars) — this is the canonical repo. |
| **BloopAI/vibe-kanban** | **27.9k** / 3.0k | Apache-2.0 | The "more popular" one — a *separate* project, not a fork. Rust backend + TS/React frontend + Postgres (sqlx). Sunsetting upstream (community-maintained). More evolved diff-review (inline comments → send to agent), workspaces (branch + terminal + dev server), PR creation, multi-agent. Heavy (Postgres, cloud/relay/tunnel machinery); not a drop-in, but the best **design reference** for review UX. |
| @asseinfo/react-kanban | 647 | MIT | Stale (no pushes since 2022). Skip. |
| SVAR React Kanban | — | MIT | Viable drag-drop component; not agent-aware. |
| dnd-kit / SortableJS | — | MIT | The modern drag-drop primitives (React / vanilla, zero-build). |
| Obsidian Kanban (mgmeyers) | — | MIT | Markdown-file board plugin; different context, not reusable directly. |
| Kanboard | — | MIT | PHP full app, server-side — not embeddable in the Rust stack. |
| Vikunja / Planka / Focalboard | — | AGPL-3.0 | Previously license-blocked; now in scope by user decision. Same coupling problem as the Apache apps (own server/DB/state), so still not drop-ins — but their board/UX is fair game to crib. |

**Recommendation** (three tiers):
1. **Lift Cline Kanban's React frontend** into a statically bundled web app
   served by the katban server (host.rs static serving), pointed at Katban's
   new admin board API — matches §12's Cline model and §13's "bundled static
   web app". Build-time Node only; no runtime Node. Apache-2.0 → clean under
   GPL-3.0. *Revisited 2026-08-29 (§20.7): its `web-ui/` is a separate React
   app coupled to a Node **state-hub WS protocol** — lifting it means
   reimplementing that protocol in Rust, which is more work than a thin UI.
   Demoted to reference, not lift.*
2. **Crib Vibe Kanban for review UX** (inline comments → send to agent, PR
   flow) as a design + component source — same license, more evolved.
3. **Zero-build fallback**: SortableJS + server-rendered HTML if we decide
   against a Node build toolchain at all (§16b's "lightest viable").

### 20.7 Approach decision — what to build, what to borrow (2026-08-29)

Evidence gathered: Cline Kanban's `web-ui/` is a standalone React app talking
WS to a Node `runtime-state-hub`; Vibe Kanban's TS app talks to a
Postgres-backed Rust API (~120 migrations, SaaS-shaped); both frontends are
coupled to their own state protocols. Katban's built backend is file-based,
flock'd, zero-dep, TUI-first. Decision set:

| Question | Decision | Why |
|---|---|---|
| Board storage | **Keep file-based `board.json` + `BoardLock`** | Fits single-admin + few runner processes; web board writes through the same lock; schema evolves via serde defaults; migration path to Postgres is 3 tables if the project ever goes multi-user (§21 audit, 2026-08-29). |
| Web UI | **Thin hand-rolled frontend over the admin board API** — start vanilla (plain HTML/CSS/JS served by host.rs, SortableJS for drag if wanted), escalate to a small bundled React app only if/when diff-review UI demands components | Zero Node runtime and (optionally) zero Node build; matches "lightest viable" (§16b) and the user's local-first instinct; Cline/Vibe frontends are protocol-coupled, so lifting costs more than writing thin. |
| Live updates | **WS per §16a E13, SSE as v1 fallback** | E13 pinned it; host.rs already has the SSE pattern to copy. |
| Runner | **Per-card headless clawde subprocess in its own git worktree (§12)** — git layer is a small validation-first wrapper over the git CLI, modeled on Vibe Kanban's `crates/git` (safety tests, path/command validation) rather than copied wholesale | §12 pinned the model; a ~100-line validated wrapper matches Katban's zero-dep ethos; the same reason Katban reimplemented origin checks instead of pulling Vibe's. |
| Admin web auth | **Session token + origin checks, guest auth as the template** | The guest server's password/device-token/origin/rate-limit patterns are proven in-repo; board API swaps the shared guest password for a per-admin session, keeps the E15 confirm gate. |
| Board app placement | **Separate axum app on its own loopback port, caddy-proxied only for the admin subdomain** | Third HTTP surface (guest server + site host + board); isolation by construction; matches §20.5. |

Phasing stays §20.4 (read-only board → live updates → runner + activity
streaming → diff review → auto-commit chains), with the frontend approach
resolved: thin first, escalate only on evidence.

Phase-1 web board landed (2026-08-29): `board::board_server` is a third axum
app (guest server + site host + board) serving an inline thin HTML board UI
at `/` plus a read-only admin API — `/api/projects` (existing project list)
and `/api/board/{project}` (cards + dependencies + `parallelCap` +
computed `ready` ids + per-card `blockedReason`), all read through the same
`board::load_board` on-disk data behind the flock. It binds loopback on
`DEFAULT_BOARD_PORT` (8790) by default, `--allow-non-loopback` to expose,
and is reachable from the alt-menu/CLI via `clawde katban board serve`. UI is
vanilla HTML/CSS/JS (dark board columns, project selector, ready/blocked
visuals) — no build toolchain, matching the §20.7 frontend call. Verified
live: seeded a board, served, curled `/` (page) + both API routes, missing
board 404s. Placement per the table above (own port, caddy-proxy later for
the admin subdomain). An audit of the board web surface closed the remaining
gaps: the frontend omitted the `blocked` column, so manually-held cards were
invisible on the board (it's a real `CardStatus`); added it (CSS + `COLS`).
Every board response (page, both APIs, error paths) now carries
`Cache-Control: no-store` + `X-Content-Type-Options: nosniff`, the same
hardening the §19.6 site-host audit applied to `host.rs` — board state is
live so never cacheable, and JSON must never be MIME-sniffed into HTML.
Refresh now preserves the admin's current project selection instead of
snapping back to the first. (Project-list ordering already sorted in
`existing_projects`.) The **write API + admin session auth landed**: the
board now has `POST /api/login` (admin password, JSON or form-encoded) plus
auth-gated write routes — add card, set status, advance (`CardStatus::next`
ladder), archive (mark done) — all origin-checked (Cline-Kanban lesson) and
all holding `board::BoardLock` around load -> change -> save so a browser
edit can never race the CLI / `/katban` / TUI. Credentials live in
`board_admin::AdminStore` (`katban/admin.json`) mirroring the guest store:
password is salted-hash only, sessions are random 256-bit values stored as
hashes, wrong passwords use the same 5->3->3->permanent per-IP lockout
ladder (shared via `apply_failed_attempt`). `clawde katban board password
<PW>` sets/rotates it. The frontend gains a Sign-in gate plus add-card /
advance / archive controls that POST with the session cookie. A follow-up
audit of the write surface closed two gaps: **login was not origin-gated**
(guest `auth` is; `POST /api/login` now runs the same `check_origin` as every
write, so a cross-origin page cannot drive a login), and the UI lost its
signed-in state on refresh because the cookie is HttpOnly — added `GET
/api/me` (200/401) so the page restores auth from the cookie on load. Both
are unit-tested (login-origin-gate, `/api/me` authed/unauth) and verified
live. Noted, not fixed: the admin cookie lacks a `Secure` flag (fine on the
loopback http board; add it when the board moves behind an https admin
subdomain).

**Build status — runner + web-board gap closure (2026-08-29):** the board
now executes cards and the web board closes its remaining editor gaps.
- **Dependencies in the web UI (feature gap):** previously link/unlink were
  CLI/TUI-only, so the web board could *show* dependencies but never create
  or remove them. Added origin-checked `POST /api/board/{project}/link` /
  `/unlink` (cycle-checked via `board::add_dependency`, holding `BoardLock`),
  and web controls: a per-card `link` button (prompts for the id to wait on)
  and an `x` unlink button beside each shown dependency. (2 new tests → 109)
- **New-board creation (feature gap):** the web UI could only *view* existing
  boards; a browser-only admin had no way to stand up a fresh project. Added
  auth-gated `POST /api/boards` (creates an empty board for a typed name,
  409 on duplicate) + a **New board** toolbar button. (→ 110)
- **SSE project-list refresh (feature gap):** the EventSource handler only
  re-fetched the current board, so a board created/removed elsewhere didn't
  appear until a manual Refresh. `events.onmessage` now calls `loadProjects()`
  (which preserves the selection and reloads the current board).
- **The runner (§12, the big one):** `clawde katban board run --project P`
  (module `runner::run_loop`) executes ready cards as headless `clawde
  --print "<prompt>"` subprocesses, each in its own git worktree. Scheduler:
  parallel-cap-aware (every `running` card, admin-set or runner-spawned,
  counts against `parallel_cap`); crash-recovery (a stale `running` card is
  reset to `queued` on start); holds `BoardLock` for every load->change->save;
  marks the card `running` (with a worktree dir) before spawning so slots are
  reserved atomically; succeeds -> `review`, fails -> `failed` with a retry
  counter up to the board's new `auto_retry` cap (§16a E6), then stays failed
  and blocks dependents.
- **Diff review (§20.3 #3, essential slice):** the runner captures each card's
  worktree diff at completion and stores it on the card (capped at `DIFF_CAP`
  16 KB so `board.json` can't balloon), then tears the worktree down; the web
  board renders a collapsible `diff (N ch)` `<details>` per card so review
  survives the checkout being gone.
- **retry-cap consistency (audit fix):** `board::ready_to_run` now treats a
  failed card past `auto_retry` as **not** ready (its dependents stay blocked),
  so the runner, `/katban board ready`, the CLI, and the web `ready` badge
  all agree — a failed card does not auto-restart forever. (→ 121 katban)
- **Retry/result surfacing:** the web card shows `retries N`, the last
  `result`, and `autoRetry` in the header; `clawde katban status` and
  `/katban` print `runnable:` — board projects with a registered repo.
- **Terminal review parity:** `board card show <ID>` (`/katban board card
  show <ID>`) prints a card's status, retries, last result, and the
  runner-captured diff — a browser-less admin can review card work after the
  worktree is torn down.

**Board execution pre-requisite (new — the `project` registry):** a board is
keyed by project *name*, but the runner can't make a worktree without knowing
which repository that name is. `clawde katban project set <NAME> <DIR>`
(`projects.rs`, `~/.clawde/katban/projects.json`) maps a board project to a
git repo (canonical absolute, validated as a dir); `project list` shows it.
A project with no repo still works for planning — only card *execution*
requires the mapping, and `board run` warns when it's missing. The web board
URL `/api/board/{project}` already surfaces which card ids are `ready`; the
runner's down-branch worktree-removal + diff capture are covered by unit
tests against a real git repo.

**Always-on runtime (built — §11/§20.7):** `board serve --run <PROJECT>` runs
the board web UI and the card scheduler in one process, and `board expose
--run <PROJECT>` (persisted in `admin.json` as `runnerProject`, re-exposes
keep it) renders `katban-board.service` — `board serve --port N --run
<PROJECT>`, `Restart=always`, non-root user, one unit per project until a
multi-project scheduler lands. The `Secure` cookie behind an https admin
subdomain is already implemented (`x-forwarded-proto: https` -> `; Secure`,
covered by `login_cookie_gets_secure_behind_https_proxy`).

Still to build per this spec (deferred, needs a design call): **diff-review
line comments + send-to-agent** (§16a E5, the React/escalation slice) and
**commit / PR + auto-commit chains** (§16a E5/E6). The audit's remaining
design call is the **multi-project scheduler** (N projects -> N units today).

---

## 21. Audit log

Discovery: 5-round structured interview (modes derivation, tool-safety,
isolation default-to-jail-hardened, auth, persistence, caddy-autopilot hosting,
lean-on adjacent projects, phasing). Grounded against code: worktree/isolation
machinery (§2.2), gateway agent loop + auth/perms (§2.1), modes/preset +
permission system (§2.3), and the Cline Kanban WebSocket-hijack advisory (§3b).

Audit rounds resolved the previously-open items and added: architectural
reframe (Katban = pre-secured web-facing server; guest chat + public live
viewing piggyback its networking), subdomain-per-project + DuckDNS API
automation, one-board-per-repo, commit+auto-PR, configurable parallel cap,
guest search via dedicated SearXNG endpoint (all guest links by default),
guest ephemeral working memory + on-demand downloadable AI session summary,
TUI-driven control surface, hash-chained audit log.

The abstraction round locked the board semantics + interfaces in §16a E1–E14:
Project/Site model (sites need no repo), 4-column + badge state machine, cycle
detection + block-dependents failure semantics + transient-only retry, Cline
-style card artifacts, **owned-include-file caddy mechanism** (replacing the
admin-API approach due to full-config-replace risk), on-demand session summary,
opaque server-side tokens, `~/.clawde/katban/katban.json`, multiplexed WS, and
the admin-only-orchestration confirmation. Smaller items remain in §16b.