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
   The backlog item is ticked when the iteration's change is **shipped**: for `pr` mode when the PR is
   *opened*, for `auto-merge` when it merges, for `local` when the branch is kept. (Ticking on PR-open
   in `pr` mode is deliberate — every iteration re-bases off the integration branch, which does **not**
   yet contain in-flight PRs, so leaving the item open would re-ship a duplicate PR each cycle; true
   merge/landed state is tracked via PR status, not the tick.) The item is **never** ticked on a
   push/auth failure (it didn't land) or when the agent deviated to a different change (that item didn't
   ship). Only a green, PR-merged change advances the compounding *base*, independent of the tick.
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

## RSI loop overhaul — branch hygiene, auto-condense-to-main, role pipeline, per-phase models

A 2026 hardening pass (grounded in the RSI video + current multi-agent-RSI practice) adds four
mechanically-enforced behaviors on top of the loop above:

- **Branch hygiene — at most ONE `rsi/*` branch, never a dirty worktree.** Preflight prunes any
  leftover `rsi/*` branches (dead-run / local-ship residue) while on the clean base, and the
  iteration branch is force-deleted after every ship outcome, with a clean-tree tripwire — the base
  is always pristine for the next iteration. (`_prune_stale_rsi_branches`.)
- **Auto-condense to main BEFORE the loop finishes.** With `ship: auto-merge` (now the default for
  managed repos), each iteration opens a PR, polls CI up to a bounded ceiling, then MERGES on green /
  CLOSES + reverts on red (auto-revert applied to CI) / hands off to GitHub native auto-merge if CI
  is slow. The iteration does not finish until the change lands on the integration branch or is
  cleanly reverted. (`_wait_for_ci_then_merge`.)
- **Role-agent pipeline** — each iteration runs an ordered set of bounded phases:
  `PLAN → IMPLEMENT → GATE → REVIEW(judge) → E2E(visual) → CLEANUP`. PLAN (read-only, `plan.md`)
  drafts a short plan injected into the implementer. REVIEW (adversarial judge, `review.md`) runs
  AFTER the objective gate + commit and BEFORE ship: it inspects the committed diff for
  reward-hacking / scope-creep / regressions / goal-miss and REVERTS on reject (fail-open if it can't
  run — the objective gate already passed). This is the Proposer/Solver/Judge anti-reward-hacking loop
  and the safety gate for fully-automatic merge-to-main. CLEANUP is the deterministic branch hygiene
  above; E2E is the existing visual review. PLAN/REVIEW are opt-in per repo via `pipeline: {plan,
  review}` in repos.json (default off = legacy).
- **Per-phase model / provider / reasoning.** repos.json may carry `phases.<phase>: {provider?,
  model?, reasoning?}` (plan, implement, review, e2e, beautify, ideate, recovery), overriding the
  repo-level config for that phase's process. Smart defaults route the light phases (beautify/e2e) to
  the provider's cheap worker model (minimax-m3 on Ollama Cloud) at low reasoning while the deep
  phases keep the repo's strong model — AlphaEvolve's breadth-vs-depth split applied to the loop.
  (`_apply_phase_config`.)

repos.json knobs (all default to prior behavior when absent): `ship` (local|push|pr|auto-merge),
`pipeline: {plan, review}`, `phases.<phase>.{provider,model,reasoning}`.

## Status & hardening

The loop and its safety ladder are implemented in `control.py`, `improver/run_improver.py`, and
`improver/solomon.py`. An ultra review (multi-agent, adversarially verified) produced a prioritized
hardening backlog to make the invariants *mechanically enforced* rather than prose-only — chiefly:
runner-side **baseline + anti-gaming** measurement (step 6, applied to supervisor fix-sessions too),
**CI-green-before-auto-merge** (step 7, polling CI before an auto-merge so an empty post-create rollup
isn't merged as "no CI"), backlog-advance on a **verified ship** (PR opened / merged / kept-local — not
on a push/auth failure or agent deviation; step 8), preflight **refuse-on-out-of-band-base-move**
(never-hand-patched keystone, step 1), and the **supervisor holding the single-flight lock** during
recovery (step 9). These are tracked and landed as gated changes to Solomon's own harness code.

The Video-1-grounded enhancement pass (cross-repo gate, `EVAL_CMD` eval needles, visual hard-gate,
agent-artifact recovery, dirty-repo self-recovery, ideate external research) is documented in the
sections above. All 7 invariants + the safety rails are untouched — the new gates ADD to the gate
(`gate-enforced-by-runner`), they don't replace it; recovery stays reversible-first; the agent still
authors under contract; the supervisor still holds the lock; the base still compounds only via merged
PRs. Test suite: 256 passed, 1 skipped (`tests/test_e2e_loop.py` covers the full `one_iteration()`
orchestration against a mock pi).

---

## Correlated test discovery & enhanced anti-gaming

### Problem: Missed correlated tests

When the agent changes files in module X, the gate would only run tests that directly test X, missing
tests in other files that import or depend on X. This could lead to regressions in dependent code
going undetected.

### Solution: Correlated test discovery

The runner now automatically discovers and runs tests that are **correlated** with the changes:

1. **Extract module names** from changed files (e.g., `scripts/config.py` → `config`)
2. **Search test files** for imports/references to those modules using regex
3. **Expand the gate** to include correlated test files alongside the default gate

**Example:**
```
Changed file: asmodeus/execution/broker.py
Test file: tests/test_broker.py contains "from asmodeus.execution.broker import paper"
Result: tests/test_broker.py is included in the gate automatically
```

### Enhanced anti-gaming measures

The anti-gaming checks have been enhanced with additional safeguards:

1. **Error count increase**: Detects when error count increased (new test failures introduced)
2. **Significant skip count increase**: Detects when skip count increased significantly (tests being skipped instead of fixed)
3. **Collected count gaming detection**: Logs warnings when collected count increases but pass count stays the same (possible gaming by adding trivial tests)

### Implementation

Key functions added to `run_improver.py`:

- `_find_correlated_tests(changed_files)`: Finds test files that import changed modules
- `_expand_gate_with_correlated(gate_cmd, changed_files)`: Expands gate command to include correlated tests
- `_get_changed_files_for_correlation(base_sha)`: Gets list of changed `.py` files

### Integration

The `run_gate()` function now accepts an optional `changed_files` parameter:

```python
def run_gate(changed_files: list[str] | None = None) -> tuple:
    """Authoritative test gate. Returns (green, {passed,failed,errors,green}, tail)."""
    # Expand gate command to include correlated tests if changed_files provided
    effective_gate_cmd = GATE_CMD
    if changed_files:
        effective_gate_cmd = _expand_gate_with_correlated(GATE_CMD, changed_files)
    # ... rest of function
```

### Testing

Comprehensive test suite in `tests/test_correlated_tests.py` with 33 tests covering:
- Correlated test discovery
- Gate expansion
- Anti-gaming measures
- Skip marker detection

All 201 tests pass (including 28 existing tests, confirming no regressions).

---

## Cross-repo correlated test gate

When a managed repo shares a module with another repo, a change in one can break the other — and a
single-repo gate would miss it. A repo may declare `cross_repo_deps` in `repos.json`: a list of repo
names whose gate should run when THIS repo changes a shared module.

```json
{"name": "sover", "cross_repo_deps": ["maki"]}
```

After the primary gate passes (green + anti-gaming clean), the runner runs each declared dep repo's
OWN gate (from the dep's `repos.json` row) in the dep repo's cwd. Any red dep reverts the branch
(same as a primary gate red). The per-dep results are recorded in `history.jsonl` under
`cross_repo_gates`. Self-references are excluded (the primary gate already covers this repo). If
`cross_repo_deps` is absent/empty, behavior is unchanged (backward compatible).

---

## Per-repo eval needles (`EVAL_CMD`)

Video 1: "metrics can be misleading… amount of code merged = bloat/slop." A green test gate alone
optimizes for "tests pass", not for the real product metric. A repo may declare `EVAL_CMD` in
`repos.json`: a benchmark/visual/product-metric command run AFTER the test gate passes, parsed by
the RUNNER (never the model).

```json
{"name": "maki", "EVAL_CMD": ".venv\\Scripts\\python bench.py"}
```

The runner parses a single float from the command's stdout (the first float wins; e.g.
`score: 0.85 | p99: 210ms` yields `0.85`) and measures it on the clean base BEFORE any change, then
again after the gate passes. Anti-gaming: if the score DROPPED vs the baseline, the branch is
reverted — a green test gate that made the product WORSE on the richer needle still reverts. If the
command prints no float, or times out, the gate is INACTIVE (don't block on a missing needle; the
runner logs it). If `EVAL_CMD` is absent, behavior is unchanged. This is the spec's "evals are
everything" multiplier — richer per-repo needles beyond unit tests.

---

## Visual review hard gate

Video 1: "agents can cheat… rewrite the evaluation function." The visual E2E review (boots the app
in a sandbox, captures screenshots, runs a vision agent) was advisory-only — a gap. A repo may
declare `visual_gate: true` in `repos.json` to upgrade a SUCCESSFUL review WITH ≥1 critical finding
to a BLOCKING gate: the branch is reverted (same as a test-gate red). Off by default for non-UI repos
(backward compatible). The existing advisory-only path (feedback for the next iteration) is unchanged
for warnings/info and for repos without the flag. A review that itself FAILED to run still does NOT
block (best-effort — the RSI loop must not break if the visual infra is down).

```json
{"name": "sover", "visual_gate": true, "sandbox": {"enabled": true, "launch": "...", "pages": ["/"]}}
```

---

## Agent-artifact recovery (untracked-file preflight)

Video 1: "agents go nuts in long-running sessions… return to a complete mess." A dead run's leftover
untracked files (the agent's own `AGENT_LOG.md`, `capabilities/*`, `profiles/*`, `start_*.sh`, or
anything under an explicit `.agent_artifacts/` sentinel) used to wedge the loop FOREVER — preflight
refused to `git clean -fd` them as operator-work protection. Now a CONSERVATIVE recovery path: if
EVERY untracked non-ignored file matches an agent-artifact heuristic (`_AGENT_ARTIFACT_PATTERNS`), the
runner STAGES them on the `rsi/*` branch (never the base), runs the gate, and ships or reverts —
exactly like a normal iteration. A single operator file among the set keeps the whole set protected
(refuse + escalate, unchanged). The heuristic list is deliberately narrow (a false positive would
destroy operator work); the bare `capabilities/` / `profiles/` dirs are NOT matched (they'd sweep any
operator dir of that name).

---

## Dirty-repo self-recovery

Review findings #2 (revert-failure wedge) + #3 (dirty-tree+live-loop deadlock):

- **Runner self-stop on persistent dirty-BASE:** a dirty base tree blocks preflight every iteration
  but the runner never stopped — it spun forever on the same refusal while the watchdog kept
  restarting it. After N=3 (`_DIRTY_BASE_PERSISTENT_LIMIT`) consecutive dirty-BASE preflight bails,
  the runner writes the `STOP` sentinel + records `status=error/phase=preflight/reason=dirty_base_persistent`
  so `monitor.should_restart` leaves it alone and the operator is alerted. A dirty non-base branch
  (an `rsi/*` leftover cleared by the forced preflight) does NOT count; a clean iteration RESETS the
  counter (a transient dirty spell doesn't accumulate toward a false stop).
- **`solomon.recover()` auto-reset for `revert_failed` when the loop is NOT live:** a dead iteration
  left the repo on an un-revertable `rsi/*` branch and the loop halted; the runner never cleared it
  itself. If `diagnose()==revert_failed` AND `control.is_running(repo)` is False, the supervisor
  auto-runs `reset_to_base` (holding the supervisor lock so it never mutates git under a live
  iteration) + `cleanup_worktrees` to drop the lingering `rsi/*` branches. This is RUNG-0
  (reversible: `checkout --force` + `reset --hard origin/base` — never force-push, never merge). If
  the loop IS live or the reset fails, it escalates.
- **Auto-cleanup of `rsi/*` branches on a clean stop:** `control.stop()` now waits briefly for the
  loop to confirm stopped, then auto-runs `cleanup_worktrees` to delete lingering `rsi/*` branches
  left by local/push ship modes or dead runs (the observed `rsi/iter-...` leftover on sover).
  Cleanup is best-effort; the guard inside `cleanup_worktrees` (never delete the current branch)
  keeps it safe even if the loop is slow to exit.

---

## Ideate-lane external research

Video 1: "agents hyperfocus, got stuck on a local minimum, never looked up new ideas on the
internet." A repo may declare `ideate_research: true` in `repos.json` to PERMIT (but not require) the
ideate agent to do web/docs lookup (Context7/webfetch-style) for novel ideas — to escape the local
minimum and propose genuinely novel, high-leverage moves. Default false (the agent reads only the
local repo — the existing behavior, preserved). The output is always REVIEWABLE backlog items: the
runner sorts + prepends them, the agent NEVER edits its own menu (menu curation stays human-owned —
the `agent-implements-under-contract` invariant is untouched).

```json
{"name": "maki", "ideate_research": true}
```

---

## In-app agent browser panel + visible cursor (Feature 1)

A long-lived, **agent-controlled** browser whose live state (screenshot + cursor position +
URL) is rendered in a dashboard panel so the operator can watch the agent browse. The browser
is launched **headless** so it never steals the operator's cursor/focus — the operator sees
the agent's browser *only* through the panel's screenshot stream + the rendered visible cursor.
This is the "agent control only, visible to the user through a panel" requirement, satisfied
mechanically.

- **Bridge:** `improver/agent_browser.py` wraps pinned `agent-browser` 0.27.0 sessions.
  (node/Playwright). The bridge owns a persistent Chromium user-data-dir per repo (login
  state persists across actions within a session) and exposes `navigate / click / type /
  scroll / screenshot` actions for the agent's action loop.
- **Live state:** after each action the bridge atomically writes
  `runtime/<name>/browser_state.json` (`{ok, url, screenshot_b64, cursor:{x,y,click},
  status, phase, ts}`). `control.browser_state(repo)` reads it; the dashboard's Browser tab
  renders the screenshot + an animated cursor at the reported viewport-percentage position.
- **Image-native model required:** the agent is given the screenshot and must output the
  next action (click at x/y, type text, scroll) — this bridge executes it. The cursor
  position is the last action's target, so the operator sees where the agent "is."
- **Best-effort, never breaks the loop:** every failure path writes `{ok:false}` and
  returns an error dict — `agent_browser.py` never raises into the RSI loop. A missing
  node/playwright or a crashed driver shows the panel's empty state, not a dashboard crash.
- **Agent-only control:** the operator never drives the browser directly. The panel is
  observe-only; the visible cursor is the agent's, not the operator's.

---

## Mandatory visual testing phase for frontend repos (Feature 1a)

The visual hard-gate (`visual_gate: true`) was opt-in per repo. It is now **mandatory by
default for repos with a detected frontend**, closing the "green gate but broken UI" gap for
UI repos without requiring the operator to remember to set the flag.

- **Detection:** `control.has_frontend(repo)` probes for `index.html` (root/web/public/src),
  SPA frameworks in `package.json` (react/vue/next/vite/svelte/astro/solid/preact/lit/angular),
  `public/` / `dist/` / `web/dist/` / `static/` build dirs, or a `templates/` (Jinja) dir.
- **Resolution (`run_improver._visual_gate_enabled`):**
  1. `visual_gate: true` in repos.json → on (explicit opt-in, unchanged).
  2. `visual_gate: false` in repos.json → **OFF** even if a frontend is detected (explicit
     opt-out — e.g. a headless API repo with a stray `templates/` dir).
  3. `visual_gate` **absent** and the repo has a detected frontend → **ON** (mandatory).
  4. `visual_gate` absent and no frontend → off (byte-identical to legacy non-UI behavior).
- **Flow unchanged:** the existing post-gate visual review runs; a SUCCESSFUL review with
  ≥1 critical finding reverts the branch (visual gate red) and the feedback drives the next
  iteration via `LAST_VISUAL_FEEDBACK` — the agent gets one more iteration to address the
  findings and finish. The sandbox must still be configured (launch command) for the review
  to actually run; if it isn't, the review fails-to-run and best-effort doesn't block, but
  the gate is still *enabled* so the operator sees it's expected and configures the sandbox.
- **Connect flow:** `connect_project(visual_gate=None)` auto-detects and defaults the gate
  ON for frontend repos; an explicit `visual_gate=False` is always honored (the flag is
  written to repos.json so the runner sees the explicit opt-out, not a missing key).

---

## One-click connect + GitHub login (Features 4 & 5)

- **`connect_project(spec, goal, ship, provider, reasoning, interval, max_iterations,
  visual_gate)`** — a single backend call that clones (GitHub spec) or registers (local
  path) a repo, sets the north-star goal + config in repos.json, auto-detects the gate,
  triggers background contract enrichment, and sets the visual gate. The dashboard's
  Connect modal asks the handful of configuration questions up-front (goal, ship mode,
  reasoning level, cadence, visual testing) so the operator presses Connect and is ready
  to press Start — no round-trip through the workspace Config tab.
- **`github_login_start()`** — one-click GitHub login. Launches `gh auth login --web` in a
  new console window (device-code flow) so the operator can complete the interactive login
  without leaving Solomon. Idempotent: returns `{already:true, login}` if already authed.
  The dashboard polls `github_status` after launch so the Connect modal updates live.

---

## Worktree visualization (Feature 2)

- **`list_worktrees(repo)`** — structured view of the repo's git worktrees + local branches:
  `{kind: 'worktree'|'branch', name, path, branch, head_short, is_current, is_rsi, dirty}`.
  Rendered in the workspace's **Worktrees** tab with current/rsi/dirty/stale tags and
  springy row hover. `dirty` is a best-effort `git status --porcelain` check.
- **One-click cleanup:** the existing `cleanup_worktrees(repo)` (prune + delete leftover
  `rsi/*` branches, never the current branch) is surfaced as a button in the Worktrees tab
  and the Settings "Clean up all worktrees" action. The visualization refreshes after cleanup.

---

## Airy / modern / hyperinteractive UX (Feature 3)

Refined the visual language toward cleaner paneling and springy micro-interactions — **no
flashy glows**. Added spring easing tokens (`--ease-spring`, `--ease-out-soft`), springy
button/card/rail hover with `--lift`/`--tap`, cross-fade view transitions, a soft toast
float-in, a softer workspace drawer slide, a skeleton shimmer for async panels, and a
softer (non-flashy) pulse for live indicators. The existing 5 themes + tokens are
preserved; the polish is additive CSS over the same classes.

---

## Per-repo API key (`api_key`)

A repo may carry an `api_key` field in repos.json that overrides the global
provider key (in `Solomon/.env`) for THAT repo's iterations only. This lets each
repo use a different OpenRouter account/key (or a different Ollama key) without
sharing one global key.

```json
{"name": "maki", "provider": "openrouter", "model": "openrouter/owl-alpha",
 "api_key": "sk-or-v1-..."}
```

- **Loading:** `Ctx::load_env()` loads the global `.env` keys first (existing
  behavior), then `Ctx::apply_api_key()` overrides the active provider's env var
  (`OPENROUTER_API_KEY` / `OLLAMA_API_KEY`) with the per-repo value. Each repo's
  improver is a separate child process, so the override is isolated to that repo.
- **Mid-loop refresh:** `refresh_config_from_registry()` re-applies the per-repo
  key each iteration (after `apply_phase_config`, so a per-phase provider override
  lands the key in the right env var). A dashboard edit to the per-repo key takes
  effect without a stop+restart.
- **Preflight:** the existing `required_key()` env-var check passes when EITHER
  the global `.env` key OR the per-repo key is set.
- **Redaction:** `Ctx::redact()` already scrubs the literal `OPENROUTER_API_KEY` /
  `OLLAMA_API_KEY` env values from agent text; since `apply_api_key` sets the env
  var to the per-repo key, the per-repo key is scrubbed too.
- **Dashboard:** `get_state` surfaces `api_key_set: bool` per repo (never the
  value). The Loop Controls panel has a per-repo "API key (per-repo)" password
  input; the placeholder reflects set/unset state. `set_repo_config` accepts
  `api_key` as its 12th positional arg (null = unchanged, "" = clear, string = set).

---


`updater.py` + `updater.spec` build a standalone **`SolomonUpdater.exe`** (console) that
is the "update + open" entry point: double-click it and it (1) finds the solomon source
repo (`SOLOMON_HOME` or walks up from its own location), (2) `git pull --ff-only origin
<branch>` if the tree is clean and behind, (3) rebuilds `Solomon.exe` via PyInstaller if
the pull updated the tree or the exe is missing, then (4) launches `dist/Solomon/Solomon.exe`.

Safety: never force-pushes/resets/discards local commits (`--ff-only` fails loudly on
divergence); never pulls over a dirty tree (warns + rebuilds with the current tree); stops
a running `Solomon.exe` before rebuilding so the file isn't locked; on a build failure,
launches the previous exe if present so the operator isn't stranded. Build python is the
maki venv (same as `build.ps1`); override with `SOLOMON_BUILD_PY`. `build.ps1` builds
`Solomon.exe` (via `solomon.spec`); `SolomonUpdater.exe` is built separately via its own
`updater.spec` (`<maki-venv-python> -m PyInstaller updater.spec`).
