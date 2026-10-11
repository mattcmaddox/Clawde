## 10. User-preference questions still open (research could not answer)

Research resolves the *what does best practice say* dimension of five questions.
One remains a user-preference / UX-discoverability call:

- **Q6 (unresolved): past thinkingtrace discoverability.** `-back` and `back-play`
  exist today. Whether they're discoverable enough, and whether you want an easier
  way to revisit past reasoning, is a UX judgment that depends on how you actually
  work — not something the literature decides. The Claude Code design-space paper
  (arXiv:2604.14228) emphasizes real-time observation and after-the-fact audit as the
  human-control surfaces, but is silent on past-trace discoverability specifically.
  **Ask the user.**

### Q6 prompt for the user

Do you ever want to re-read a previous turn's thinking? If so:
- Is `-back` / `back-play` findable enough, or would you want a more visible way
  (e.g., a thinking history in the sidebar, a `/thinking-back` alias, or thinking
  traces shown inline with past messages)?
- Or is the final answer quality what matters, and you don't care about re-reading
  the raw thinking afterwards?

---

## 11. Research bibliography (cited in §9 resolutions)

Papers and sources consulted to resolve the §9 open questions. All are bleeding-edge
(2026) unless noted.

### Primary sources (directly resolve open questions)

- **Jali, Nayak & Joshi — "Not All Turns Are Equally Hard: Adaptive Thinking Budgets
  For Efficient Multi-Turn Reasoning" (arXiv:2604.05164v2, Apr 2026).** CMU. Formulates
  multi-turn reasoning as a sequential compute allocation MDP; TAB policy (GRPO-trained)
  allocates smaller budgets to easy turns, saves up to 35% tokens while maintaining
  accuracy. Key takeaways for this spec: (a) adapt budgets per turn, not uniformly;
  (b) easy turns waste budget that hard turns need; (c) the optimization signal is
  accuracy-vs-tokens, not brevity alone. Resolves Q1 (calibration) and reinforces Q2
  (ceiling rationale).

- **Liu, Zhao, Shang & Shen (VILA Lab / UCL) — "Dive into Claude Code: The Design Space
  of Today's and Future AI Agent Systems" (arXiv:2604.14228v2, Apr 2026, cited 51).**
  Source-level analysis of Claude Code's architecture. Core findings: the loop is a simple
  while-loop; most code is operational harness (5-layer compaction, 7-mode permissions,
  4 extensibility mechanisms, subagent delegation, append-only storage); guiding principle
  is "minimal scaffolding, maximal operational harness"; human decision authority is a
  first-class value. Resolves Q1 (guidance should be light on prescriptive structure),
  Q3 (reasoning in model, enforcement in harness — auto-raise sets request params, per-
  upstream shaping handles the model), and informs Q5 (observable-only heuristics).

- **Xu, Wen & Li (Peking U) — "Adapting the Interface, Not the Model: Runtime Harness
  Adaptation for Deterministic LLM Agents" (arXiv:2605.22166v2, May 2026).** Life-Harness:
  a lifecycle-aware harness with a Trajectory Regulation layer that "detects degenerate
  patterns such as repetition, stagnation, invalid retries, or budget exhaustion, and
  triggers recovery." 88.5% average relative improvement across 18 backbones by adapting
  the harness, not the model. Resolves Q5 (degeneration detection belongs in the harness/
  loop, not the model; specific degenerate patterns to key on).

- **Wang, Hou, Wang & Zhao (HUST) — "When Agents Do Not Stop: Uncovering Infinite Agentic
  Loops in LLM Agents" (arXiv:2607.01641v1, Jul 2026).** IAL-Scan static analyzer on
  6,549 repos: 68 confirmed IAL failures across 47 projects, 91.9% precision. Defines
  IAL as a feedback path repeatedly triggering costly/state-growing actions without an
  effective bound, where continuation is controlled by model outputs/tool observations/
  state/exceptions/delegation. Resolves Q5 (the danger is unbounded costly/state-growing
  repetition, not any repetition; heuristics must key on state change + cost, not mere
  repetition).

- **Wu — "When Errors Become Narratives: A Longitudinal Taxonomy of Silent Failures in a
  Production LLM Agent Runtime" (arXiv:2606.14589v1, Jun 2026).** 8-week study of a
  production agent runtime: 22 incidents, 5-class silent-failure taxonomy, fail-plausible
  class (LLM narrates errors into fluent false output). ~70% of silent failures caught by
  human user-view observation, not tests/health checks. Resolves Q5 (heuristics must be
  independent of model self-report; the user noticing is the most reliable detector; amplify
  human noticing rather than replace it).

- **NousResearch/hermes-agent #13663 — "Smart reasoning_effort routing based on task
  complexity" (GitHub, Apr–Jul 2026, closed/completed).** Identical problem statement to
  this spec's Q2/Q3: static reasoning_effort is too much for trivial turns, too little for
  complex ones. Community-converged mapping: low = simple Q&A/single-file/formatting/
  device control; medium = default/planning/summarization; high = coding/debugging/multi-
  step/architecture; xhigh = explicit "think hard" only; ultracode/max = user-intent only.
  Accepted solution: rule-based smart routing (Option B), with LLM classifier (Option C) as
  later upgrade. UX requirements: resolve actual provider-side effort per request, surface
  in footer, explicit override is hard override, don't escalate for simple action commands
  just because tools are available. Directly resolves Q2 (ceiling + mapping) and informs
  Q3 (routing + effort co-optimization).

- **Atlan — "ReAct vs. Plan-and-Execute: Agent Architecture Guide [2026]."**
  Compares ReAct (reason-act-observe loop) vs Plan-and-Execute (plan upfront, executor,
  re-plan on trigger). Key cautions: ReAct's repeated-reasoning failure mode wastes tokens
  reconsidering what a stable plan fixed once; goal drift accumulates locally-useful steps
  away from the objective; Plan-and-Execute's risk is plan staleness and poor re-planning
  triggers ("if triggers are too broad, the agent re-plans so often it loses the benefit").
  Resolves Q1 (encourage stable plan early, don't re-reason perpetually) and Q4 (Plan Mode
  interview should be gated, not always-on; too much planning friction destroys the benefit).

- **"LLM Agentic Failure Modes" (ceaksan, Apr 2026).** Eight-agent-failure-mode taxonomy:
  task drift, incorrect tool invocation, reward hacking, positional bias, mode collapse,
  degeneration loops, alignment faking, version drift. Degeneration loops: "repeated identical
  tool calls with no state change, or repeated error + retry without variation." Defense:
  goal anchoring (restate original goal every N steps), planning before acting (OpenSpec/ADR),
  multi-attempt + reflection, max step limits + tooling constraints, cross-model verification.
  Task drift: "every N steps, ask the agent to restate the original goal. If the definition
  shifts, drift is active." Resolves Q5 (specific detection signals and defenses; goal
  re-anchoring is the drift mechanism — already exists in Clawde as GOAL_REANCHOR_INTERVAL).

### Secondary sources (context, not direct resolution)

- Alenezi — "From Prompt–Response to Goal-Directed Systems: The Evolution of Agentic AI
  Software Architecture" (arXiv:2602.10479v1, Feb 2026). Reference architecture separating
  cognitive reasoning from execution via typed tool interfaces; BDI foundations; convergence
  toward standardized agent loops, registries, auditable control. Context for §2.4 and §6
  component map.

- Sebastian Raschka — "Controlling Reasoning Effort in LLMs" (2026) and "Language Models
  for Text Classification" (2026). Background on reasoning-effort parameters and the role
  of lightweight classifiers as pre-screeners — context for Q2/Q3 classifier discussion.

- LiteLLM auto-routing docs + MorphLLM LLM-router guide (2026). Production router practice:
  difficulty × ambiguity → model tier + reasoning effort; unmatched turns fall to defaults.
  Context for Q3 routing-hint discussion.

- OpenAI — "A practical guide to building agents" (Jan 2026). Three-component agent model
  (model, tools, orchestration). General context.

- Yao et al. (2022/2023) — ReAct original. Referenced by the above; Claude Code paper and
  Atlan guide both trace their reasoning-to-action lineage here.

---

*Spec ends here. No code changes yet.*
