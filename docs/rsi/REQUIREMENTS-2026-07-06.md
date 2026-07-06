# Solomon RSI engine v3 — operator requirements (Cayleb, 2026-07-06)

North star: Solomon is THE RSI engine (polsia-style) over the portfolio. First target: kairos,
goal metric "$100 profit every 15 minutes on Kalshi across all positions/markets". Operator is
the idea layer only — production/execution is fully autonomous. Runs unattended for weeks.

## Hard requirements (operator-stated)
1. Brains: Ollama cloud ONLY (glm-5.2:cloud, minimax-m3:cloud; local hardware later — provider
   layer must be swappable). Executor: pi CLI (proven in-house: sover run_pi.py + solomon pi.rs).
2. Tiered code autonomy on targets: non-money code auto-lands on green tests; money-path
   changes land but go LIVE only after honest replay + paper-shadow window; auto-rollback.
3. Solve the diagnosed failure modes (stagnation, restrictive gates, value measurement,
   problem generation) AND the full autopsy catalog (docs/rsi/FAILURE-CATALOG-2026-07-06.md)
   — the operator explicitly said his list is not exhaustive; the catalog is the floor.
4. Token + speed efficiency are first-class: no token spend without new objective data
   (metric-freshness ledger), short-circuit no-op cycles, per-cycle token/wall budgets,
   per-provider budget ledgers with canaried fallbacks.
5. Cleanliness/storage: a janitor subsystem — temp file deletion, log rotation, archive
   compaction, bounded runtime dirs — on the supervisor cadence.
6. Max 15 subagents per workflow when Solomon orchestrates fan-outs.
7. MASSIVELY reduce problem→solution latency: landing is the default (gates sized to blast
   radius), escalations carry TTLs with degraded-mode fallbacks, no "wait for operator" dead
   ends. Everything that keeps the loop alive lives inside the loop's writable, gated scope.

## Design inputs (read all three before building)
- docs/rsi/RESEARCH-SYNTHESIS-2026-07-06.md — the architecture from 7 research lenses
  (Anthropic, Karpathy autoresearch, Emergent Garden, DGM/STOP/AlphaEvolve/SICA, OSS, prod
  landing patterns, quant fitness).
- docs/rsi/FAILURE-CATALOG-2026-07-06.md — ranked, evidence-grounded failure modes from
  autopsies of solomon/asmodeus/dotz/sover/evohedge, each with its countermeasure.
- SOLOMON_RSI.md — the existing Gen-1 seven invariants (keep them; they are the part that
  worked: branch-per-iteration, runner-enforced gates, PR-only shipping, auto-revert,
  supervisor rungs, never-hand-patched).

## Kairos target contract
- Goal metric: settled $/15-min-window from .state/kairos.db (paper worst-case-priced + live).
  Metric-freshness gates every meta-decision on NEW settled rows, not iterations.
- Fitness: tests (test_tracking.py) → honest replay battery (frozen windows + holdout) →
  paper shadow → live evidence ladder (promote.py stays the leash authority inside kairos).
- Untouchable in kairos: the sacred floors in kairos AGENTS.md, promote.py's ladder,
  .state/KILL semantics.
