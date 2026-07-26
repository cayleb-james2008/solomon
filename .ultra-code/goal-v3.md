# Ultra-Code Goal — Solomon profit loop v3

## Finish line
Done means Solomon can run a CEO/content cycle with local Ornith at `http://localhost:13305/api/v1`, publish a real bottom-funnel Dev.to tool spotlight, and avoid repeating a successful spotlight when Dev.to authenticated reads intermittently fail. The code remains a single Python process with no new service or recurring LLM bill.

## 96-line
- Local Ornith is the production brain for think, topic selection, and article generation.
- Tool spotlights are selected before generic SEO topics because they link directly to live paid tools.
- The CEO deterministically routes a ready spotlight to content even if the LLM proposes speculative channel work.
- Dev.to reads use the required custom user-agent and `per_page=30` cap.
- A small normalized local title ledger prevents duplicate spotlights during remote API failures.
- Full pytest suite, clean-process local-brain smoke test, real publish smoke test, and post-publish rotation check are green.

## Deferred 4%
- Measured funnel conversion experiments and article refresh cadence (trigger: 10 spotlight posts with analytics).
- Multi-platform distribution and cross-posting (trigger: Dev.to cadence reaches one article per tool).
- Automatic local/cloud failover (trigger: local inference misses a complete production cycle).

## Risk
Medium-high: external publishing and public reputation are involved; money-out guard remains unchanged.

## ADRs
- ADR-06: local Ornith plus local publish ledger.
- ADR-07: bottom-funnel spotlights before generic SEO.
