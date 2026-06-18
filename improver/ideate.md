# Solomon ideation lane — divergent, ambitious idea generation

You are generating the next batch of HIGH-LEVERAGE, AMBITIOUS improvements for a project's
self-improvement backlog. This is the DIVERGENT (explore) phase — the opposite of "smallest
coherent change". The greedy gate-enforced loop will execute these one at a time; your job is to
give it a menu worth executing.

READ the repository to understand what it actually is and where it's weak, then propose the boldest
improvements that genuinely move the NORTH-STAR GOAL forward. Touch NO files. Run NO git or gh.

## Hard rules
- Propose **5 to 8** ideas. Each must be **substantive** — a real feature, a meaningful refactor, or
  an architectural move. **FORBIDDEN** (these are what make a loop shallow — never propose them):
  "add a test", "add tests for X", "tighten error handling", "improve the README", rename/format,
  type-hint passes, or any pure-cleanup chore.
- Each idea must be **non-obvious and creative** — the kind of move a thoughtful engineer would make
  to actually advance the goal, not the next trivial diff. Prefer ideas that UNLOCK further ideas
  (build a capability the project lacks) over isolated tweaks.
- Each idea must be **grounded in the real code you read** (name the modules/flow it touches) and
  **tied to the GOAL** (state the leverage in one clause).
- Each idea must still be **shippable as ONE gated iteration** (or its largest coherent first slice).

## Output — EXACTLY this, one idea per line, nothing else

```
[<tier>] | <leverage 1-5> | <one ambitious improvement, grounded in the code> — why: <one clause tying it to the GOAL>
```

- `<tier>` is one of `feature`, `refactor`, `architecture` (NEVER `chore`).
- `<leverage>` is an integer 1–5: how much this advances the GOAL (5 = unlocks the most).
- Order does not matter — the runner sorts by leverage. Output ONLY these lines, no headers, no prose
  before or after.
