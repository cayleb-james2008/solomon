# Commit provenance for Solomon's watched config

Status: normative for RSI v3. Countermeasure to failure mode #6 in
`docs/rsi/FAILURE-CATALOG-2026-07-06.md` ("the control plane exempts itself from its
own gates"): Solomon's own tree ran 601 uncommitted lines on an off-base branch,
hand-built into production, while an unversioned config swap tripped a downstream
breaker on phantom data. Config surgery without provenance is how the controller
poisons itself.

## Watched files

The engine treats these as *watched config* — every change to them must land as a
committed, provenance-tagged change:

- `repos.json`
- `ops.json`
- `actions.json`

## The convention

Every commit that touches a watched file is prefixed with exactly one provenance tag
in its subject line:

- `operator:` — human-initiated. The operator (or a session acting on the operator's
  direct instruction) changed the file. Example:
  `operator: rsi-v3 baseline snapshot (tree normalization, docs/rsi design inputs)`
- `rsi:` — engine-initiated. The RSI engine changed the file as part of an
  autonomous cycle. Example: `rsi: park ollama-cloud/glm-5.2 until 2026-07-07T03:00Z`

Rules:

1. One tag, first token of the subject, lowercase, followed by a space.
2. A commit mixing watched-config changes with unrelated code changes still needs the
   tag; prefer splitting the commit so config provenance stays legible.
3. No third category. A watched-file commit without a tag is a convention violation
   and should be treated by tooling the same as an uncommitted mutation.

## Enforcement (WS5 tripwire)

WS5's config-provenance tripwire watches the files above and **pages on any
watched-file mutation that persists uncommitted** — i.e. a dirty watched file that is
not committed within the tripwire's grace window. The page identifies the file and
the diff; trading-adjacent lanes are halted until the mutation is either committed
with a provenance tag or reverted.

The tripwire deliberately does not auto-commit: an unattributed mutation is evidence
of an ungated write path, and the correct response is to stop and surface it, not to
launder it into history.

## Why prefixes and not trailers

The subject-line prefix is greppable from `git log --oneline` and visible in every
PR list and blame view without extra flags. Audit query:

```
git log --oneline -- repos.json ops.json actions.json
```

Every line of that output must start with `operator:` or `rsi:` (merge commits
excepted). That property is the check; anything else is a gap in the gate.
