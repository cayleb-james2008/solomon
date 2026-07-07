# Solomon → Autonomous AI-CEO Platform — Architecture Plan (2026-07-07)

## North star (operator goal)
Solomon is an **autonomous AI CEO** that **scales, improves, and — mainly — makes PROFIT** from each project with **zero user intervention**. Product form: a user **plugs in their API key + their go-to project**, and the backend does everything else, as fast and highest-quality as possible. This redefines Solomon's scope from a *code-quality RSI engine* (its current form) to an *autonomous business operator whose objective is real profit*.

## Two sources, one architecture
1. **"True Agent Autonomy"** (https://youtu.be/GHsq0klC_4g) — the **runtime**: an event-driven, persistent reasoning thread + log-structured memory. Bind the *event-driven-persistent* form (parks between events, wakes on real signals), **not** the literal "never-stop" (which the video's own creator disavows as inefficient/hallucination-prone with zero validated wins).
2. **Polsia** (polsia.com, github.com/PolsiaAI) — the **org**: an AI-CEO orchestrator commanding **constrained** specialist agents (engineering/marketing/comms/ops/finance) with real tool execution + cross-company learning. Its **"constrained autonomy"** (limited tools + scope per agent) is the exact antidote to the never-stop runaway.

They converge: **event-driven wake + persistent memory + constrained specialist agents + real execution + a profit objective.**

## Target architecture (4 layers)

### Layer 0 — Runtime: PECRT (Persistent Event-Driven Continuous Reasoning Thread)
A long-running **orchestrator IDENTITY** (not a growing token buffer) that **parks between events at zero token cost** and **wakes on real signals**, carrying warm context forward, runs a bounded self-eval loop, then re-parks. Build **one shared `pecrt` toolkit** (Rust crate + decision-identical `pecrt.py`), bound into every harness:
- **`wake-bus`** — `next_wake(sources, max_park) -> WakeReason`. Pluggable watchers: file-append (outcomes.jsonl, verdicts), sqlite row/ts (settled trades, view metrics), signals (provider-recovery, KILL/drain), + a **max-park deadline** (the old 5-min cron demoted to a liveness *floor*). Solomon's `freshness::run_freshness_probe` JSON becomes the standard event schema.
- **`warm-context`** — three-tier memory: **WORKING** (bounded, rewritten each wake) + **SHORT-TERM** (append-only *dated log-structured observation log* — the anti-thrash/anti-drift tier, Mastra observer→consolidator→reflector recipe) + **LONG-TERM** (adapter over each harness's *existing* durable ledgers — no migration). `reconstruct_context()` with a stable prompt prefix for cache hits (~$4/day economics).
- **`eval-park`** — self-eval → continue-or-park, with the **anti-compulsion rule**: forbidden to invent busywork when blocked on external ground truth; **parking is the preferred outcome**.
- **Safety invariant (non-negotiable):** the thread is a *scheduler+memory wrapper, never a new authority*. Every action still flows through the existing gates; it **cannot edit whitelists, raise its own budget, or bypass the skeptic**. Persistence is of context/identity only; powers are re-checked against the gates every action.

### Layer 1 — Org: the AI-CEO (Polsia model)
On top of PECRT, an **AI-CEO Orchestrator** per project that, on wake: reads state → prioritizes → dispatches to **constrained specialist sub-agents** via a **Task System**, then reports.
- **Specialists** (each with limited tools + scope): **Engineering** (this is today's `pi` coding agent — already exists), **Growth/Marketing** (content, social — Sover's engine is a prototype), **Sales/Outreach**, **Finance** (revenue/costs/Stripe), **Research** (competitor/market/opportunity), **Ops** (deploy, monitoring, incident).
- **Constrained autonomy** = the safety gates already in place become each specialist's tool/scope limits.
- **Cross-project learning:** a shared, anonymized knowledge layer (a "wins ledger") so a pattern that worked on project A seeds project B — Solomon's fleet already has the substrate (outcomes.jsonl across repos).

### Layer 2 — Objective: real profit, not code quality
The single biggest reframe. Today Solomon's fitness ≈ tests/code-quality. The CEO's tier-1 objective must be **real business outcomes** (revenue, growth, engagement, settled PnL) with the **outcome-driven self-critique the video misses**: after acting, grade *did this move the target metric?* against ground truth, not "did it run green." This plugs directly into the freshness/evidence discipline (wake on new outcome samples; never claim progress a metric hasn't confirmed).

### Layer 3 — Product: plug-and-play, 0 intervention
- **Onboarding:** API key + project URL → auto-detect stack, provision the CEO + relevant specialists, seed the objective from the project's real metric source.
- **Multi-project / multi-tenant:** one runtime, N project-CEOs, isolated state + shared learning.
- **Re-engagement:** the "morning report" (what happened, what's next) — a durable-ledger summary, not a live thread.

## Current Solomon → the gap
| Dimension | Today | Target |
|-----------|-------|--------|
| Cadence | 5-min `schtasks` cron sweep, one job/sweep | Event-driven wake on real signals; park otherwise |
| Agent | one stateless `pi` ReAct session/cycle | Warm CEO identity + constrained specialists |
| Memory | cold re-derivation from ledgers each cycle | Warm working context + dated observation log + ledgers |
| Objective | code quality / tests green | Real **profit / business outcomes** + outcome self-critique |
| Scope | code improvement only | Full business ops (eng + growth + sales + finance) |
| Execution | commits/PRs | + real tool/MCP actions (deploy, post, email, pay) |
| Product | operator's own repos, hand-configured | Plug-in-key + any project, 0 intervention, multi-tenant |

## Build roadmap (sequenced by leverage÷effort)
**Phase A — Runtime foundation** (highest leverage; makes everything cheaper + honest)
1. Build the shared **`pecrt` toolkit** (wake-bus + warm-context + eval-park); extract dotz's context-bus/checkpoint as the reference; standardize Solomon's freshness-probe JSON as the event schema. `[L]`
2. **Bind Solomon:** flip `freshness` ON for **all 5 fleet lanes** + **repair the unobservable probes** (venue_fills/equity_fresh/publish_recency) → "spend only on new ground truth." `[S-M, high]`
3. Promote the freshness short-circuit into the **wake source** (demote the 5-min cron to a max-park floor) + add self-eval continue-or-park to `one_iteration`. `[M-L, high]`

**Phase B — CEO org layer**
4. Add the **CEO Orchestrator + Task System** above `plan_jobs`; register today's `pi` as the **Engineering specialist**; define the constrained specialist interface (tools + scope + gate).
5. Add the first **non-engineering specialists** where a project needs them (Growth for sover/landing-page; Research/Finance for the trading bots) — each constrained, each gated.

**Phase C — Profit objective**
6. Reorient each project-CEO's tier-1 objective to its **real business metric** (settled PnL for kairos/asmodeus; views/followers→revenue for sover; usage for dotz) + wire the **outcome-driven self-critique** (grade against the metric, not green tests).

**Phase D — Real execution**
7. **MCP tool integrations** so specialists *act* (deploy, post, email, payments) — under the same blast-radius gates + human-gated money-out.

**Phase E — Cross-project learning**
8. A shared anonymized **wins ledger**; the CEO consults it when planning.

**Phase F — Product**
9. **Plug-and-play onboarding** (API key + project) + multi-tenant isolation + the morning report.

## Safety + honest reality (kept front-and-center)
- Every layer operates **inside** the existing anti-gaming / blast-radius / skeptic / freshness gates — these *become* the constrained-autonomy guardrails. Money-out stays human-gated.
- **The engineering to solve** (not reasons to avoid): context rot → bounded working tier + dated observation log (never prose-of-prose); cost → event-driven parking + per-wake budget + stable-prefix cache; determinism/replay → checkpoint thread state.
- **Honest bound on "autonomous profit":** the CEO can *operate, scale, and optimize* a project superbly, but it **cannot manufacture edge where the economics don't allow it** — kairos/asmodeus are evidence-gated markets where an edge may or may not exist (that reality is unchanged by better agent architecture). For a general plug-and-play product, the same honesty applies: profit is bounded by each project's real economics, and the architecture's job is to *find and compound* real edges fast + honestly, and to *stop honestly* when there's none — never to fabricate progress. That honesty discipline is Solomon's existing moat; keep it.

## Immediate next step
Phase A.2 is the highest leverage-per-effort win and needs no new code: **turn freshness ON for all fleet lanes + fix the unobservable probes**, converting "tick regardless" into "spend only on new ground truth." That is also WS5 improvement #1 for the now-live Solomon.
