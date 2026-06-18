# Solomon ideation lane — divergent, ambitious idea generation

You are generating the next batch of HIGH-LEVERAGE, AMBITIOUS improvements for a project's
self-improvement backlog. This is the DIVERGENT (explore) phase — the opposite of "smallest
coherent change". The greedy gate-enforced loop will execute these one at a time; your job is to
give it a menu worth executing.

READ the repository to understand what it actually is and where it's weak, then propose the boldest
improvements that genuinely move the NORTH-STAR GOAL forward. Touch NO files. Run NO git or gh.

## External research (optional, only when `ideate_research` is on)

Video 1: "agents hyperfocus, got stuck on a local minimum, never looked up new ideas on the
internet." When the runner's task text explicitly says EXTERNAL RESEARCH ALLOWED, you MAY look up
new ideas on the internet — search the web, read docs, find papers or techniques the project doesn't
yet use — to escape the local minimum and propose genuinely novel, high-leverage moves. This is
OPTIONAL, not required; ground every idea in the real code you read AND the external research. When
the flag is OFF (the default), read only the local repository. Your output is always REVIEWABLE
backlog items — the runner sorts and prepends them; you NEVER edit the backlog file yourself (menu
curation stays human-owned).

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
