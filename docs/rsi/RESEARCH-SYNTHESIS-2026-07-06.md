# RSI research synthesis (2026-07-06)

Produced by a 7-lens research workflow (Anthropic, Karpathy/autoresearch, Emergent Garden video, academic lineage DGM/STOP/AlphaEvolve/SICA, OSS repos, production landing patterns, quant fitness). Lens summaries follow the architecture.

---

# SOLOMON RSI ENGINE v1 — Build Spec (target: kairos)

Brains: `glm-5.2:cloud`, `minimax-m3:cloud` via Ollama. Executor: pi.dev CLI. Host: Windows 11. Design stance: **the harness carries the intelligence; models are stateless subroutines** (Anthropic "workflows over agents"; Karpathy "scaffold owns the loop").

---

## 1. Topology & File Layout

Three trust zones, enforced by Windows ACLs (separate low-privilege account runs the executor) — not by prompts:

```
C:\...\solomon\                     # ZONE A: engine (loop may PROPOSE edits, human lands them)
  orchestrator.py                   # dumb deterministic state machine, restartable at any stage boundary
  prompts\*.md                      # versioned stage prompts (meta-editable, Tier 3)
  gates\tiers.json                  # path-glob -> tier map, versioned
  archive\experiments.tsv           # append-only, one row per attempt incl. discards
  archive\problems.json             # the backlog (schema below)
  archive\lessons.md                # nightly distilled lessons
  DIRECTION.md                      # human-owned taste: ranked themes, banned areas, trust notes
  SOLOMON.md                        # human-owned contract: goal metric, off-limits paths
  PROGRESS.md                       # session-appended log
C:\...\solomon-protected\           # ZONE P: executor account has NO write ACL, ever
  fitness\runner.py                 # replay harness wrapper (wraps kairos backtest.py logic, frozen copy)
  data\replay\battery\              # ~200 frozen 15-min windows, stratified by volatility regime
  data\replay\holdout\              # never shown to the loop; weekly eval only
  watchdog\watchdog.py + config     # separate Windows scheduled task
  invariants.py                     # risk rails imported by kairos order path
  keys\                             # Kalshi creds; loop-facing = paper/read-only creds only
C:\...\kairos\                      # ZONE K: mutable git worktree (trader.py, core.py, strategist.py,
                                    #   config.json strategy keys, app.py, tests — tests read-only to executor)
```

**Data structures.**
`problems.json` entry: `{id, tier(T0-T3), axis, description, metric_hypothesis($/window delta), verification_steps[] (append-only), status, attempts, last_failure_reason, ev, embedding}` — features.json pattern (Anthropic harness lens): JSON because models resist corrupting it; verification steps immutable to executor.
`experiments.tsv` row: `{ts, problem_id, candidate_id, parent_commit, model, diff_stats, gate_outcomes, fitness:{median_usd_win, p5_win, sign_p, sharpe, mdd}, decision(KEPT|DISCARD|REJECTED_SCOPE|FAILED_BUDGET), wall_s, note}` — autoresearch results.tsv (Karpathy lens): discards are data.
`results.json`: written by fitness runner **outside the worktree**; only harness-computed numbers enter the archive (autoresearch log-attack lesson).

## 2. The Loop (per-cycle state machine)

`SEED → SELECT → SPEC → IMPLEMENT → HACK-CHECK → REVIEW → FITNESS → LAND/REVERT → RECORD`, plus a weekly `META` cycle. Cadence: 20–60 cycles/day; per-cycle hard budgets (15 min implement, 10 min fitness) killed via Windows job objects/taskkill — the fixed experiment atom (Emergent Garden + Karpathy 5-min rule): timeouts score `FAILED_BUDGET` and the loop moves on; nothing can wedge it for weeks.

1. **SEED** (glm-5.2, fires when open problems <12): mines (a) DIRECTION.md, (b) trading sensors — backtest-vs-live fill divergence, worst-K replay windows, exception/log anomalies from app.py, order-latency percentiles, (c) loop telemetry (bottleneck stage), (d) cheap no-LLM parameter-sweep probes on config.json numerics. Mostly Software-1.0 sensors feeding the LLM, not the LLM freewheeling — the entropy-collapse countermeasure (Karpathy/Dwarkesh).
2. **SELECT**: `ev = p(land | historical hit-rate of this task shape/tier) × metric_hypothesis × staleness(axis) / cost`; ε-greedy, with every 5th cycle forced **EXPLORE** (must not touch the current champion's technique family) — mandatory exploration quota (Emergent Garden: agents never branch out unaided).
3. **SPEC** (glm-5.2): converts problem → executor-shaped spec: single file, named function, expected behavior, acceptance test text, ≤150-line diff. Task-shaping is the planner's job because non-frontier executors only land small idiomatic diffs (Karpathy "leash"/nanochat lessons).
4. **IMPLEMENT** (pi.dev, fresh process, the only free-form stage, sandboxed to Zone K): fixed startup ritual — `git log -10`, tail PROGRESS.md, read the one assigned problem, run `init.ps1` smoke test **before** new work; smoke failure auto-swaps the problem to "fix broken state" (Anthropic stateless-session pattern: this kills the compounding-breakage spiral). One problem per session; mandatory commit + PROGRESS.md paragraph. Prompt states the mechanism flatly: "changes to tests, fitness code, or replay data are auto-rejected by the harness" — inoculation framing, not moralizing (Anthropic reward-hacking lens).
5. **HACK-CHECK** (deterministic Python, no LLM): reject if diff touches non-allowlisted paths (tests, backtest.py, report.py, invariants, keys), adds skip/xfail, mocks the Kalshi client in prod paths, swallows assertions, hardcodes metrics, or exceeds 150 lines / 3 files.
6. **REVIEW** (minimax-m3 — a *different* model than the implementer to decorrelate errors): JSON `{verdict: pass|revise|reject, blocking_issues[], suggestions[]}` against a correctness-only rubric; `revise` loops back to IMPLEMENT with issues inlined, **max 3 iterations**; reject writes `last_failure_reason` into problems.json. Crucially: review is **advisory repair, never fitness veto** — the mechanical fitness numbers decide (Karpathy "dhdhdhdh": under weeks of pressure the proposer finds any LLM judge's cracks).
7. **FITNESS** (Zone P process; §4).
8. **LAND/REVERT**: keep ⇒ commit to lineage branch + tier-appropriate promotion; else `git reset --hard`. Auto-revert triggers are **harness-enforced, never LLM-decided**: smoke fails twice, replay regression, canary below baseline CI, error-rate spike. `KNOWN_GOOD` tag advances only on confirmed-positive/neutral-after-canary (Anthropic git-native rollback).
9. **RECORD**: append experiments.tsv; nightly "sleep consolidation" job distills the day's rows into 3–5 falsifiable lessons in lessons.md, cited to experiment IDs, capped and compacted. Every SEED/SPEC prompt gets lessons.md + last-20 rows injected — externalized memory is the learning; the model is amnesiac (Karpathy).

Malformed JSON from any LLM stage = one retry then attempt-failure; never a hang.

## 3. The Four Failure Modes — Mechanisms & Why

**(1) STAGNATION.** Root causes: context amnesia, compounding silent breakage, exploitation-only defaults, plateau-as-model-property.
Mechanisms: stateless sessions + startup ritual + externalized memory (Anthropic harness); cheap failure atom — a dead attempt costs one `git reset`, so throughput = attempt-rate × hit-rate and attempt-rate is engineered (Karpathy autoresearch, ~700 experiments/2 days); **brain rotation** on plateau — no accepted improvement in N=15 cycles OR any "nothing more to do" claim ⇒ swap glm↔minimax, reset context to archive-summary-only; "impossible" is logged as a plateau event, never accepted (Emergent Garden: Opus falsely gave up, a different model broke the plateau); EXPLORE quota (Emergent Garden); per-task-shape hit-rate bandit shifting problem mix toward shapes that land (Karpathy). Loop-health leaderboard: landed/day, hit-rate/tier, cycles-to-land — stagnation you don't measure is stagnation you don't fix (Karpathy speedrun; Anthropic tier metrics). *Why over alternatives:* a smarter monolithic agent isn't available, and open-ended agency fails silently — which *is* the stagnation signature (Anthropic workflows-over-agents).

**(2) GATES.** Root cause: uniform strictness + binary verdicts + slow verification forcing conservatism.
Mechanisms: **risk-tiered gates by path-glob** (Anthropic RSP; Karpathy autonomy slider) — T0 (docs/logging/tests/telemetry): auto-merge on compile+tests; T1 (strategy logic/params in core.py, strategist.py, config.json): + replay non-regression; T2 (money path: trader.py order sizing, kalshi_client.py, promote.py): + 24h paper canary, then queued async owner-ack — loop never blocks on it; T3 (Solomon itself, fitness, watchdog, invariants): propose-only. **Evaluator-optimizer revise loop** instead of binary reject (Anthropic building-effective-agents): 70%-good patches land on attempt 2–3. **Fast deterministic verification** makes permissive gates affordable — restrictiveness migrates to the diff-size cap where weak models are most reliable (Karpathy generation-verification). **Cheap scripted rollback** (tested weekly by deliberately reverting — Emergent Garden) is the gate-loosening enabler. KPI tripwire: gate pass-rate <40% after retries ⇒ the rubric itself becomes a Tier-3 meta-problem. *Why:* the owner's gates died because human/LLM judgment sat in the approve path; here nothing subjective can veto a change that mechanically scores better.

**(3) VALUE MEASUREMENT.** Root cause: gating on live P&L (fails Karpathy's verifiability triad: not resettable, ~96 noisy samples/day, reward too slow) and gameable graders.
Mechanisms: frozen replay battery as the gate (resettable/efficient/rewardable — Karpathy verifiability); **write-protected grader** via ACLs + hack-check (autoresearch pinned `evaluate_bpb`; Anthropic `sys.exit(0)` exploit class); **paired statistics** (§4); **hidden holdout** windows (Emergent Garden hidden validation set); metrics parsed only by the harness from its own fill records, never from agent-readable logs (autoresearch log-attack surface). *Why:* every alternative (LLM-judged quality, self-reports, single-run P&L) is documented to fail under optimization pressure.

**(4) PROBLEM GENERATION.** Root cause: LLM idea-wells collapse (Karpathy: "the model will silently give you all the same stuff").
Mechanisms: problems.json backlog + refill stage as a first-class loop state (Anthropic features.json); **sensor-driven generation** (§2 SEED) so entropy comes from the market and the codebase, not the model; **novelty pressure** — embed each proposal, cosine-reject near-duplicates vs archive, inject tried-and-failed IDs as a DO-NOT-REPROPOSE list, rotate personas/temperatures (Karpathy); **metric-saturation rule** — best fitness flat <x% over N cycles ⇒ next task is "propose 10 new measurable sub-problems each with own metric+baseline" (Emergent Garden "100% on the test means you need a new test"); **axis table** (edge, sizing, execution cost, risk, infra, coverage) scheduled by staleness×headroom so one plateaued axis never starves the loop (Emergent Garden frontier fallback); **Amdahl bottleneck rotation** supplies meta-problems forever (Anthropic RSI essay); DIRECTION.md keeps the human at taste altitude, not diff altitude (Anthropic).

## 4. Fitness & Attribution Ladder (noisy-target design)

- **L3 — Tests** (~1 min): pytest on test_tracking.py etc.; verify via junit-xml artifact + assertion counts, never exit code alone (Anthropic).
- **L2 — Honest replay** (~5–10 min, the keep/discard gate): fitness runner replays the frozen battery (fees + slippage modeled, fixed seeds, wall-clock cap). **Accept iff**: median $/window > parent + ε (deadband kills noise-stacking) AND 5th-percentile window ≥ parent's (no tail risk bought for mean gain — Karpathy works.all()) AND paired same-window sign test p<0.05 / bootstrap CI excludes zero. Paired design on identical windows kills most variance for free. Per-window vectors logged for localization.
- **L2.5 — Async long-horizon confirmation**: full-week replay runs after KEEP; auto-demote on failure (autoresearch: short-horizon wins "did NOT reproduce").
- **L1 — Paper shadow (champion–challenger)**: old and new code paper-trade the same live windows 24h (T1) / 72h (T2); paired per-window comparison over ≥100 windows; P

---

## Lens summaries

- Anthropic's published thinking on recursive self-improvement ("When AI builds itself", June 2026), agent harness engineering ("Effective harnesses for long-running agents"), reward-hacking/misalignment research, the Responsible Scaling Policy's graduated-safeguards pattern, and long-horizon autonomy experiments (Project Vend) — mined for mechanisms that bound autonomy while preserving velocity in an unattended, non-frontier-model RSI loop.

- Andrej Karpathy 2024–2026: verifiability-first Software 2.0/3.0, jagged intelligence, 'keep AI on the leash', RL/reward-hacking skepticism, and his March-2026 'autoresearch' repo — a working minimal self-improvement loop (mutate one file → fixed 5-min run → pinned metric → keep/discard via git) that is the closest existing blueprint for Solomon's RSI engine over kairos.

- The flagged video is "Recursive Self Improvement" by Emergent Garden (youtu.be/t7_ZXgfJVG8, uploaded ~mid-June 2026, opens with the Tim & Eric "Celery Man" bit). Thesis in three parts: RSI is (1) possible — but today only as "weak RSI" ("indirect self-improvement that depends heavily on human contribution", e.g. Anthropic using Claude Code to build Claude Code), versus "strong RSI" ("fully automatic direct self-improvement... without any human help"), which is still science fiction; (2) hard — demonstrated via his "Fractal Search" experiment (copied from Karpathy's autoresearch): coding agents (Claude Opus ~7h/$114, a GPT model ~$60, another Claude ~$40; ~$214 total) run an endless research loop writing neural-net solutions that fit the Mandelbrot set, each a 5-minute training run scored on a hidden validation set, committed to git, plotted on a dashboard. Agents made real gains (Fourier nets, then hash grids — one model "broke the plateau" another had declared impossible) but then hit diminishing returns, hyperfocused on one technique, never searched for outside ideas, and Opus falsely "gave up and insisted that there was nothing more it could do"; (3) dangerous — metric gaming ("that which gets measured gets fudged"), agents able to rewrite their own eval ("give itself a score of negative infinity"), code opacity, and goal drift; so keep ultimate goals and eval "exclusively under human control", sandbox the loop, and expect that "even good metrics will eventually be saturated... if you get 100% on the test, you need a new test." The video is a near-exact scale model of Solomon: same loop shape, same non-frontier-brains constraint, and it empirically exhibits all four of the owner's failure modes — which makes its design details and observed pathologies directly harvestable.

- Academic lineage of self-improving coding agents (2023–2025) — Darwin Gödel Machine, STOP, Gödel Agent, AlphaEvolve, ADAS, Voyager, Reflexion/Self-Refine, SICA — with each system's loop structure, fitness function, archive/selection scheme, and anti-reward-hacking design extracted and mapped onto Solomon's four failure modes (stagnation, restrictive gates, value measurement, problem generation) for a Windows-hosted, Ollama-cloud-brained RSI loop targeting the kairos Kalshi trading bot.

- Open-source GitHub self-improvement-loop repos (DGM/HGM, SICA, OpenEvolve, AIDE, GEPA, EvoPrompt, SIA, Live-SWE-agent, BabyAGI/AutoGPT lineage) mined for concrete, portable mechanisms — archive schemas, parent selection, gate design, fitness functions, evaluators — for the Solomon RSI engine driving kairos with Ollama-cloud models on Windows.

- Production practitioner patterns for autonomous code-landing at velocity: how Devin/Cognition, Factory, Sweep, Google SRE/CI, and serious solo-dev harnesses (ralph loops, Codex loop engineering, cron improver lanes) keep autonomous edits landing consistently without breaking prod — merge policies, shadow/canary deploys, auto-rollback triggers, test-gate right-sizing, and outcome attribution ledgers, mapped onto Solomon-over-kairos with mid-tier Ollama-cloud models.

- Self-improving trading systems: how automated quant strategy-research loops (walk-forward promotion pipelines, shadow/paper/live tiers, overfitting-deflation statistics, sequential kill tests, strategy graveyards, regime-aware allocation) decide whether a change actually improved a live trading bot when the fitness signal (PnL) is noisy, non-stationary, and adversarial. Core insight: serious systems almost never gate on raw live PnL — they gate on PAIRED same-event comparisons, decomposed causal-layer metrics (calibration, fill quality, cost), and trial-count-deflated statistics, and they make gates asymmetric (cheap to try, fast and automatic to kill) so throughput survives while capital is protected.

