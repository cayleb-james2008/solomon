# The Solomon RSI Loop — canonical specification

> The single source of truth for the recursive-self-improvement (RSI) loop that **every** project
> managed by Solomon follows. Anchored to the thesis of *Recursive Self Improvement*
> ([youtu.be/t7_ZXgfJVG8](https://youtu.be/t7_ZXgfJVG8)): an AI can build and improve software, and
> already does — *the system always builds on its most recent verified-best version, so gains
> compound* (Anthropic, ["When AI builds itself"](https://www.anthropic.com/institute/recursive-self-improvement); the Karpathy loop).

Solomon is the **orchestrator/supervisor**. It does not write project code. Each managed repo has a
**pi agent** (the improver) that makes changes under a written contract; a **runner** that owns the
objective gate and version control; and Solomon, which provisions, schedules, and recovers. The
human operator sets goals and reviews PRs — and **never hand-patches a managed project**.

---

## The loop (every project, every iteration)

1. **Establish the compounding base.** The iteration starts from the integration branch at its most
   recent *verified-best committed* state, read fresh from git — never a working copy, stash, or
   un-merged experiment. Only green, PR-merged changes advance the base.
2. **Measure the baseline.** The runner captures an objective, hard-to-game needle on the base
   *before* any change: tests green/red **and the pass + collected counts**, build status, and the
   starting SHA. No baseline → no iteration.
3. **Select one highest-leverage improvement.** The pi agent, under `AGENT.md` + `backlog.md`, picks
   exactly **one** improvement. One iteration ships one improvement.
4. **Branch per iteration.** The runner cuts a fresh `rsi/<iteration>` branch off the integration
   tip. The integration branch is never edited in place.
5. **Implement under contract (agent-only).** The pi agent authors the change on its branch, scoped
   to the one improvement, test-first where tests exist. The operator/orchestrator never edits the
   working tree by hand (recovery excepted, step 9, and still gated).
6. **Gate — verify (runner-enforced, never model-judged).** The **runner** runs the test/build gate
   on the branch, parses the result itself, and runs **anti-gaming** checks (tests not deleted,
   weakened, `skip`/`xfail`-ed; pass + collected counts did not drop vs the baseline; gate not
   stubbed). The model's self-report is never the gate verdict.
7. **Ship via PR only, with auto-revert on red.** On green, push the branch and open a PR against the
   per-repo target branch (honoring `auto_push`/`effective_ship`); verify the branch actually landed
   on origin; capture the PR's CI rollup. On red — **local or CI** — revert/abandon the branch and
   never merge. Nothing reaches the integration branch except a green, PR-shipped change.
8. **Compound + anti-gaming check.** Once **merged**, the change becomes the new base; gains compound.
   The backlog item is ticked only when `origin/<base>` actually contains the change.
9. **Supervise & recover (supervisor-authorized, laddered).** Solomon diagnoses health from files
   only and walks a **safe** ladder: **RUNG-0** deterministic, reversible auto-recovery that never
   discards un-pushed commits, never force-pushes, never merges; **RUNG-1** an *opt-in*, PR-gated pi
   fix-session for a persistent gate-red streak; **RUNG-2** escalate to the operator (non-destructive,
   with copy-paste git steps). Anti-thrash escalates a repeatedly auto-fixed same-category failure.
   Recovery changes the repo only through the gated agent — never a hand-patch.
10. **Report & loop.** Record the iteration to `history.jsonl` (baseline → after, the verified needle,
    the commit/PR, any capability gap closed), then loop from step 1 on the new base. The goal does
    not shrink to fit budget; a truthful null beats a gamed success.

---

## Invariants (hard rules — a violation is a bug, not a tradeoff)

- **compounding-base** — every iteration begins from the integration branch's most recent
  verified-best *committed* state, read fresh from git. Only green, PR-merged changes advance it.
  *Without this it is random search, not RSI.*
- **gate-enforced-by-runner** — the pass/fail gate (tests + build + anti-gaming) is executed and
  adjudicated by the runner, never by the model. *An agent trusted to score itself learns to weaken
  the check.*
- **branch-per-iteration** — each iteration runs on a fresh `rsi/*` branch off the tip; the
  integration branch is never edited in place. *Makes every iteration reversible and reviewable.*
- **pr-only-shipping-with-auto-revert** — changes reach the integration branch only through a pushed
  PR that passed the gate; on red (local **or** CI) the iteration is reverted and never merged.
- **agent-implements-under-contract** — the per-repo pi agent is the sole author of changes, acting
  under its contract, and every change passes the gate.
- **supervisor-authorized-recovery** — recovery is driven by a *separate* supervisor along a safe
  ladder (reversible-first → opt-in PR-gated fix-session → non-destructive escalation), holding the
  runner's single-flight lock so it never mutates git under a live iteration.
- **never-hand-patched (keystone)** — a managed project is changed **only** by its pi agent (gated)
  or a supervisor-authorized, PR-gated fix-session. The human/orchestrator never hand-patches the
  managed repo — not to fix a bug, not to unstick the loop, not "just this once." *Hand-patches
  bypass the gate and the PR trail, don't compound into the verified base (the runner resets the
  tree to origin each iteration and would discard them), and corrupt the audit story that makes
  autonomy trustworthy.* The runner must **refuse** (not silently adopt) a base that moved without a
  gated iteration.

---

## Safety rails & the halt switch

- **Halt switch.** A `STOP` sentinel stops the loop immediately; a mid-iteration stop keeps the
  gate-green branch locally but does **not** ship it. `Start` must not silently revoke a live stop.
- **Token/secret hygiene.** Every subprocess runs with `GITHUB_TOKEN`/`GH_TOKEN` and
  `PYTHONPATH`/`PYTHONHOME` stripped (keyring auth + correct venv stdlib). Agent free-text is
  redacted of secret-shaped strings before it reaches a commit, PR, or log.
- **Reversible-first recovery.** RUNG-0 never discards un-pushed commits, never force-pushes, never
  merges. A revert failure halts the loop rather than letting the next preflight bulldoze a known-bad
  tree.

## Autonomy model — trust grows, oversight stays

Autonomy is a dial, not a switch. `ship` modes — `local` (commit only) → `push` → `pr` (default,
human merges) → `auto-merge` (only after CI is green). The global `auto_push` gate and the
`auto_ai_fix` setting keep unattended code changes opt-in. As the correct/redirect/takeover rate
falls, the dial can move up — but PR review and the halt switch never go away.

---

## Depth & creativity — explore, then exploit

A pure greedy "commit if better" ratchet reliably climbs to a **shallow local optimum** — it can only
accept changes that win immediately, so it favors trivial wins (add a test, dedup) and is structurally
blocked from deep, multi-step, creative improvements (Karpathy's AutoResearch observation). Solomon
counters this *upstream of the gate*, leaving the trustworthy ratchet untouched:

- **Ideate lane (divergent / explore)** — an operator-triggered one-shot (`run_improver --ideate`,
  `improver/ideate.md`) where the agent proposes **5–8 ambitious, non-obvious, leverage-ranked**
  improvements toward the north-star goal — trivial chores explicitly forbidden — which the runner
  sorts by leverage and **prepends to the backlog** (Python owns the write; the agent never edits its
  own menu). This refills the menu with deep work so the loop isn't starved into incrementalism.
- **Ambition tiers (exploit, sized to the opportunity)** — a backlog item may carry a leading
  `[chore|feature|refactor|architecture]` tag. `chore` (the default, so legacy backlogs are
  unchanged) keeps the smallest-coherent-change rule; higher tiers tell the agent to **size the
  change to the opportunity** — a substantive, multi-file change is welcome — while it still must be
  **one coherent, gate-green, PR-shipped** improvement. The gate and anti-gaming checks are unchanged:
  a bolder change must still pass the same green gate and never drop the test count.

The eval is the ceiling on depth ("evals are everything"): richer per-repo needles beyond unit tests
(a benchmark command, the visual gate, real product metrics, an LLM depth-judge for non-chore tiers)
are the planned next multiplier. Autonomy stays human-gated — ideation injects *reviewable* backlog
items; only the gate-enforced PR loop mutates a managed repo.

---

## Provisioning — how a project becomes loop-ready

Before a project's first loop, Solomon **auto-provisions** it (never hand-written per-repo):
`ensure_contracts(repo)` writes `improver/<name>/AGENT.md` + `backlog.md` (deterministic template,
optionally AI-enriched via a one-shot `run_improver --provision` pi session) and **auto-sets the
runner's gate** from stack detection (re-detected each start; never overriding an operator-set gate).
The gate must run green on the base before the loop can ship.

## Fix authorization (the keystone, operationalized)

| Who | May change a managed project? | How |
|---|---|---|
| **pi agent** | Yes | one improvement, on an `rsi/*` branch, gate-verified, PR-shipped |
| **Solomon supervisor** | Yes, for recovery | opt-in, PR-gated `run_improver --solomon` fix-session |
| **Human operator** | **No (never hand-patch)** | curate the backlog/contract; review & merge PRs; toggle dials |
| **Orchestrator (Claude/Solomon)** | **No** | provision, schedule, supervise — but never edit the repo's working tree |

Diagnoses and investigations are *input for the agent* (a backlog item / fix-session prompt), not a
license to hand-edit.

---

## Deployment — the two-instance pattern (source vs. live)

A managed product separates the **source Solomon improves** from the **live instance that runs**:

- **Source** (`workspace/projects/<name>`) — the brand-neutral code Solomon's RSI loop improves.
  Improvements compound here; releases are cut from its integration branch.
- **Live instance** (its own folder, e.g. a Desktop deployment) — runs the real workload on its own
  writable state (`SOVER_HOME`). It **updates its code** from the source's published **GitHub
  Releases** (in-app Update button) while keeping its private state local. Brand/secret data lives
  only in state and is never in the released, brand-neutral code.

This closes the recursive loop end-to-end: **Solomon improves the source → a release is cut → the
live instance auto-updates to its most recent verified-best version** — the video's thesis, shipped.

---

## Status & hardening

The loop and its safety ladder are implemented in `control.py`, `improver/run_improver.py`, and
`improver/solomon.py`. An ultra review (multi-agent, adversarially verified) produced a prioritized
hardening backlog to make the invariants *mechanically enforced* rather than prose-only — chiefly:
runner-side **baseline + anti-gaming** measurement (step 6), **CI-green-before-auto-merge** (step 7),
backlog-advance only on **verified merge** (step 8), preflight **refuse-on-out-of-band-base-move**
(never-hand-patched keystone, step 1), and the **supervisor holding the single-flight lock** during
recovery (step 9). These are tracked and landed as gated changes to Solomon's own harness code.
