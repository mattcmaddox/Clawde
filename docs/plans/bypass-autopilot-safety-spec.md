# Bypass Safety + Autopilot Architecture

Status: Phase 4A–4E done + audit pass — bypass hard boundary, autopilot state + non-blocking deferral, `/autopilot` review commands, TUI badge, transcript annotations, validated single-use replay, durable restart-recovery persistence, and audit fixes (expiry sweep on enqueue, retry-storm dedup, stricter approval matching); adapters (gateway/ACP/headless) remain out of scope
Scope: TUI/CLI first; gateway/ACP integration is a later adapter

## 1. Executive decision

Clawde should **reuse the industry-standard separation between capability boundaries and approval policy**, while keeping its own session/deferred workflow:

- **Sandbox/capability boundary:** what the process can technically touch (workspace roots, network, forbidden tools).
- **Risk/approval policy:** whether an action may execute without a user decision.
- **Session workflow:** how a user approves, rejects, or defers an action.

Do not implement autopilot as a more permissive bypass mode. Do not rely on model-generated risk labels, prompts, snapshots, or Git alone as the safety boundary.

This matches the useful patterns found in comparable systems:

- Claude Code separates permission modes from deny/ask/allow rules, preserves deny rules across modes, and has an `auto` mode with background classification. Its built-in user-interaction tools still require interaction.
- Codex separates OS-enforced sandboxing from approval policy. Its workspace-write preset can run ordinary local work while network and outside-workspace operations remain approval-gated.
- Cline provides per-category auto-approval and a separate YOLO escape hatch, but explicitly warns that YOLO removes safety checks. We should not copy YOLO semantics into autopilot.
- OpenHands separates confirmation policy from a security analyzer and exposes a resumable waiting state. This supports a typed policy boundary, but the analyzer must remain advisory; Clawde’s hard rules must be deterministic.
- SWE-agent-style systems reinforce that isolation belongs in the runtime/sandbox rather than in prompts or a UI-only classifier.

## 2. Revised scope

### Phase 4A: harden bypass

Make existing bypass honest and safer without changing its established activation contract:

- preserve Shift+Tab and explicit startup confirmation;
- preserve forbidden tools, network isolation, and explicit deny rules;
- add a deterministic hard-boundary classifier before bypass allows an action; (implemented in `crates/core/src/action_risk.rs`)
- show a persistent warning that snapshots cover tracked workspace changes only; (startup dialog copy updated)
- record bounded, redacted session audit events.

### Phase 4B: introduce autopilot policy

Implemented:

- typed `AutonomyMode` / `DeferredItem` / `DeferredState` / `AutonomyState` in `crates/core/src/autonomy.rs`;
- bounded in-memory session queue (capacity 64) with stable `AP-001` ids;
- `ToolContext.autonomy` handle wired in interactive sessions only (headless fails closed);
- `request_permission_inner` non-blocking path: `Safe` runs, `ReviewRequired` defers with a stable id, `Irreversible` denies;
- `AskUserQuestion` defers to an answerable queue item instead of creating/awaiting a oneshot;
- session binding: state is inert on session mismatch; CLI resets it on `/new`/resume/swap;
- deferred results are model-visible and instruct the model not to retry until reviewed;
- queue overflow denies rather than drops.

### Phase 4D: validated replay

Implemented — approve-then-retry, one approval = one execution:

- `/autopilot approve <id>` (`crates/commands/src/autopilot.rs`) validates before approving:
  - item exists, belongs to the current session, and is `Pending`;
  - it is a `ToolCall`, not a question;
  - it has not expired (24h TTL, `DEFAULT_ITEM_TTL_SECS`);
  - the tool still exists in the registry (`clawde_tools::find_tool`);
  - re-classifying the stored request is not `Irreversible`.
- On success the item is marked `Approved` and a `CommandResult::UserMessage` tells the model to retry the exact call (injected into the next turn).
- The permission backstop (`request_permission_inner`) checks `take_approved_match` before classifying: an exact request match (same tool name case-insensitively, same details, same path) consumes the approval and lets the call run through the normal typed executor — the tool's own schema validation and side effects apply unchanged.
- A changed request (different command/path) does NOT consume the approval; it is deferred again with a new id.
- Expired approvals are never consumed (`DeferredState::Expired`).
- `DeferredState::Approved` added; `expires_at_unix` stamped on every item; `expire_stale_items()` sweeps; `pending_count()` excludes expired items.
- No `approve-all`; no automatic replay; replay only through the central dispatcher.

### Phase 4E: durable persistence & restart recovery

Implemented — restart-recovery snapshot, never automatic execution:

- `AutonomyState` gains an optional `base_dir` (`set_persistence_dir`); the per-session file is derived from the session id (`<base>/autonomy-<session>.json`). `None` (tests/headless) disables persistence — fail closed by default.
- `persist()` runs after every mutation (enqueue, approve, reject, answer, consume, expiry sweep) using the atomic tmp+rename pattern with 0o600 perms. IO failures warn and are non-fatal (the in-memory queue stays authoritative for the current run).
- `load_persisted()` restores a session's queue on startup/resume with restart semantics:
  - `Approved` and `Pending` items are downgraded to `Stale` — an approval never survives a restart, autopilot never auto-reactivates, and nothing executes without a fresh `/autopilot approve`;
  - items past their 24h TTL are marked `Expired`;
  - `Rejected`/`Completed`/`Expired` history is preserved;
  - corrupt, version-mismatched, or other-session files are ignored;
  - `next_id` is bumped past restored ids.
- `approve_item` now accepts `Pending | Stale` — approving a restored item is the explicit revalidation step (re-checks tool existence + risk).
- CLI wires `set_persistence_dir(clawde_home()/autonomy)` + `load_persisted()` at interactive-session creation and inside `reset_autonomy_for_session` (so `/resume` restores the queue; `/new` gets a clean one).
- `reject_item`/`answer_question` moved into core so command mutations persist too.
- `list`/`status` surface `Stale` items with a `STALE — restored after restart; re-approve to revalidate` marker.

### Audit pass (end-to-end review of 4A–4E)

Verified safe / intentional (no change needed):

- approval consumption runs only inside the permission `Ask` branch, after explicit-deny and network-isolation checks — an approved item cannot bypass configured rules;
- an approved retry runs even when autopilot is OFF (one-time grant semantics — the user explicitly approved);
- Critical bash commands can never be approved (approve-time re-classification returns Irreversible) and are independently blocked by the Bash tool;
- `AskUserQuestion` in autopilot never creates or awaits a oneshot and never touches the TUI channel;
- deferral messages survive verbatim to the model for both `check_permission_for_tool_path` and the details/path wrapper;
- transcript annotations are wired into both ToolResult render paths;
- reset does not delete the old session's persisted file (keyed by session id — harmless, enables re-resume);
- restored items are review-only (`Stale`) until re-approved, and load applies the expiry sweep.

Fixed in the audit:

1. **Expiry sweep now runs on enqueue** — `expire_stale_items` removes (not just marks) expired items and is invoked at the head of both enqueue methods, so dead entries can no longer fill the 64-item capacity and cause false "queue full" denials. Capacity now counts only actionable/approved items.
2. **Retry-storm dedup** — `enqueue_tool_call` returns the existing actionable item (same tool, details, path) instead of enqueueing a duplicate, so a retry-storming model gets the same stable id back and cannot pollute the queue with copies of one action.
3. **Stricter approval matching** — `take_approved_match` now also requires the working dir to match, and when neither side carries a details/path fingerprint (stateful tools), the description is required to match as a tiebreaker so one invocation's approval cannot authorize a different invocation.

Adapters (gateway/ACP/headless, budgets, notifications) remain out of scope.

### Phase 4C: review queue

Implemented:

- `/autopilot` command family in `crates/commands/src/autopilot.rs`:
  - bare `/autopilot` toggles the session posture (no `on`/`off` subcommand, per user preference);
  - `/autopilot status` — posture, pending count, queue usage, safety note;
  - `/autopilot list` (alias `ls`) — inline text list of pending items (tool calls and questions) with stable ids, risk label, and age;
  - `/autopilot reject <id>` — marks a pending item rejected (agent will not run it);
  - `/autopilot answer <id> <text>` — completes a deferred question and injects the answer into the next model turn via `CommandResult::UserMessage` (user preference: inject into next turn);
  - fails closed with a clear message in headless/gateway/ACP sessions (no autonomy handle);
- registered in the command registry and in `PROMPT_COMMANDS` (`crates/core/src/slash_commands.rs`);
- `CommandContext.autonomy` field shared with `ToolContext`/TUI `App` so the command, the tool executor, and the status line see the same session state;
- TUI status badge `autopilot · N pending` (magenta, bold when pending) rendered after the permission-mode segment when active;
- dimmed transcript annotations for deferrals and denials (`[Autopilot deferred AP-001]`, `[Autopilot denied]`) in `crates/tui/src/messages/mod.rs`;
- answer injection path: interactive CLI already queues `CommandResult::UserMessage` as a user-visible turn (`submit_user_msg`).

Replay is deliberately later and narrow:

- list, inspect, answer, approve, reject (approve = replay is 4D);
- validate session/project identity and current risk before any execution;
- execute only through the central dispatcher;
- stale or unvalidated items require a fresh request.

Do not begin with arbitrary replay or bulk approval.

## 3. Safety model

### 3.1 Three independent layers

1. **Capability boundary:** enforced by the runtime/tool executor. Workspace scope, network isolation, forbidden capabilities, and OS/container restrictions are technical limits.
2. **Policy decision:** deterministic typed classification plus configured allow/deny rules. Unknown actions are review-required.
3. **Interaction state:** normal approval, bypass, or autopilot. This layer cannot override layers 1 or 2.

The TUI status line and documentation must name all relevant layers. “Bypass active” must never imply “sandboxed” or “reversible.”

### 3.2 Typed risk result

Use a small core type:

```rust
enum ActionRisk {
    Safe,
    ReviewRequired,
    Irreversible,
}
```

Classification is conservative and deterministic for known tool metadata, paths, and shell syntax. Unknown or malformed input is `ReviewRequired`; inability to classify must never become `Safe`.

The classifier is a backstop, not a replacement for tool-specific policy. It must run in the central executor before bypass or autopilot can allow execution.

Initial hard boundaries:

- forbidden tools and network-isolated capabilities;
- explicit deny rules;
- operations outside configured workspace roots;
- credential/token/key exposure or exfiltration;
- destructive Git history operations and `git push`;
- broad or recursive deletion;
- database/schema destruction and bulk destructive migrations;
- publish, deploy, release, service, or system-wide mutations;
- arbitrary network mutation tools.

Use `ReviewRequired` rather than `Irreversible` when an action might be safe but requires a human decision. Reserve `Irreversible` for actions that should never be replayed by this mechanism.

## 4. Bypass permissions

Bypass remains trusted execution, not unattended autonomy. Keep its explicit activation and Shift+Tab behavior. The evaluation order must be:

1. malformed request or missing tool: deny;
2. forbidden/network-isolated capability: deny;
3. explicit deny rule: deny;
4. deterministic hard boundary / irreversible action: deny;
5. review-required ordinary action: bypass may allow, with audit and status warning;
6. ordinary action: allow.

Improve activation copy:

> BYPASS ACTIVE — ordinary permission prompts are skipped. Forbidden,
> isolated-network, explicitly denied, and hard-boundary actions remain blocked.
> Snapshots cover tracked workspace changes only.

Record only redacted metadata: session ID, timestamp, activation source, project-root fingerprint, and policy fingerprint. Do not persist raw command text or secrets.

Do not add a broad blast-radius failure mechanism to bypass initially. It creates a second authorization system and can give a false sense of protection. Add counters and warnings later if they are useful for observability.

## 5. Autopilot policy

```rust
enum AutonomyMode {
    Off,
    Autopilot,
}
```

Autopilot is session-scoped, default-off, and requires visible confirmation each session. A mode preset may describe or request autopilot, but applying a preset must not activate it. Do not overload `/config set mode` with activation side effects.

Decision matrix:

| Request | Normal | Bypass | Autopilot |
|---|---|---|---|
| Safe local read/edit | configured rules | allow | allow |
| Ordinary command | ask/configured rules | allow if not hard boundary | allow only if classified safe |
| Review-required action | ask/block | allow if not hard boundary | defer and continue |
| AskUserQuestion | interactive dialog | interactive dialog | create answerable question; continue |
| Irreversible/hard boundary | deny/review | deny | deny and record |
| Network-isolated capability | deny | deny | deny |

Autopilot must return a stable model-visible message:

> Deferred for user review as AP-004. Continue with safe work; do not retry
> this exact action until the user reviews it.

The central loop must recognize this as a terminal result for that tool call, preventing retry storms.

## 6. Deferred queue: minimal first version

Start with a typed in-memory session queue and transcript/system annotations. Add durable storage only after the lifecycle is proven. A restart must never execute pending work; if pending markers are restored later, they are review-only until explicitly revalidated.

```rust
enum DeferredKind {
    ToolCall,
    UserQuestion,
}

enum DeferredState {
    Pending,
    Rejected,
    Stale,
    Expired,
    Completed,
}
```

Each item contains:

- opaque stable ID and session ID;
- creation time and bounded summary;
- kind and typed tool name/input where applicable;
- risk and reason;
- project-root fingerprint;
- relevant path/content stamp when available;
- state and expiry.

Do not use `Box<dyn Any>` or untyped values. Tool replay must retain the existing typed tool boundary and validate the input against the target tool schema before dispatch.

Recommended defaults:

- maximum 64 pending items per session;
- 24-hour expiry;
- queue overflow denies the new action and tells the model why;
- missing stamp or validation data means no replay;
- no `approve-all` in the first queue release.

## 7. Questions and permissions

### 7.1 AskUserQuestion

Keep normal interactive behavior unchanged. In autopilot, branch before creating or awaiting the oneshot:

1. validate and normalize the question;
2. enqueue an answerable `UserQuestion` item;
3. add a bounded transcript/system marker;
4. return a successful model-facing deferred result;
5. never await a UI/ACP response.

Answering a question later should feed the answer into the next model turn. It is not a replayable executable tool call.

### 7.2 Permission-gated tools

At the central permission backstop:

1. apply hard deny and explicit deny rules;
2. classify the request;
3. allow only `Safe` in autopilot;
4. enqueue `ReviewRequired` and return immediately;
5. deny `Irreversible`;
6. preserve blocking dialogs when autonomy is off.

The queue must be bounded and must not silently drop entries.

## 8. Review UX

First release commands:

- `/autopilot on|off|status|list`;
- `/autopilot reject <id>`;
- `/autopilot answer <id>` for questions.

Defer `/autopilot approve <id>` and executable replay until the validation and central-dispatch path are implemented and tested. This avoids creating a superficially safe but actually dangerous replay feature.

The TUI should show a compact badge (`AUTOPILOT OFF` or `AUTOPILOT ACTIVE · N pending`) and render queue events with existing semantic theme colors. Use warning for pending, error for denied/stale, and success for completed. Do not add hardcoded rank colors.

## 9. Audit and observability

Add typed, redacted session events:

- bypass activated/deactivated;
- action allowed under bypass;
- hard-boundary denied;
- autopilot started/stopped;
- action deferred/denied;
- question answered/rejected.

Persisting the full audit stream is optional for the first runtime increment, but every event must be bounded and safe to render. Raw inputs, environment values, credentials, and command secrets must be redacted.

## 10. Testing strategy

Safety tests must prove:

- bypass cannot override forbidden tools, network isolation, explicit deny, or irreversible classifications;
- unknown/malformed actions are not classified safe;
- autopilot allows only safe actions;
- review-required actions return immediately and enqueue exactly once;
- queue overflow denies rather than drops;
- AskUserQuestion does not create or await a oneshot in autopilot;
- normal dialogs and Shift+Tab behavior remain unchanged;
- session reset clears runtime state;
- no pending item executes after restart;
- any future replay rejects changed paths, changed project identity, stale state, malformed input, or missing tool/schema.

## 11. Decisions locked by this audit

1. Reuse the sandbox-vs-approval separation used by Codex and the confirmation-vs-risk separation used by OpenHands.
2. Reuse Cline’s per-category policy idea only as typed tool categories, not its model-controlled `requires_approval` flag.
3. Keep bypass and autopilot separate; do not make autopilot a hidden YOLO mode.
4. Put the deterministic classifier in the central executor, with tool-specific metadata as input.
5. Make autopilot conservative: only `Safe` runs automatically; unknown is review-required.
6. Implement queue visibility and answerable questions before executable replay.
7. Start with an in-memory session queue; add persistence after lifecycle tests establish the contract.
8. Omit blast-radius hard limits initially; add counters/warnings only when they can be explained and enforced accurately.
9. Keep headless autopilot off initially. A non-interactive invocation must fail closed rather than create an invisible queue.

## 12. Sources consulted

- Claude Code, “Choose a permission mode”: https://code.claude.com/docs/en/permission-modes
- OpenAI Codex, “Agent approvals & security”: https://learn.chatgpt.com/docs/agent-approvals-security
- Cline, “Auto Approve & YOLO Mode”: https://docs.cline.bot/features/auto-approve
- OpenHands, “Security & Action Confirmation”: https://docs.openhands.dev/sdk/guides/security
- SWE-agent/SWE-ReX project reference: https://github.com/SWE-agent/swe-rex
