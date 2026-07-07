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
- `rsi` family — engine-initiated. The RSI engine changed the file as part of an
  autonomous cycle. Plain `rsi:` (example:
  `rsi: park ollama-cloud/glm-5.2 until 2026-07-07T03:00Z`), or a version-suffixed
  release tag `rsi-vN:` / `rsi-vN.M:` (examples: `rsi-v3:`, `rsi-v3.1:`) for
  engine-version landing commits. The suffix must be `-v` + a digit; dots and
  lowercase alphanumerics may follow.

Rules:

1. One tag, first token of the subject, lowercase, followed by a space.
2. A commit mixing watched-config changes with unrelated code changes still needs the
   tag; prefer splitting the commit so config provenance stays legible.
3. No third category. A watched-file commit without a tag is a convention violation
   and should be treated by tooling the same as an uncommitted mutation.

Machine-readable form: `provenance::valid_provenance_subject`
(`src-tauri/src/provenance.rs`), enforced by the
`watched_file_commits_since_convention_carry_provenance_tags` test in the cargo-test
build gate, which runs the audit query below over every commit since this convention
landed (`4521559..HEAD`). Merge commits are excepted. The convention is therefore no
longer a write-only ledger: an untagged watched-file commit turns the gate red.

## Grandfathered exceptions

The enforcement test began actually running (rather than being aspirational) after some
watched-file commits had already landed untagged. History is **not** rewritten to fix them —
the convention's own rule is to surface a violation, not launder it into history. Each known
pre-enforcement violation is instead acknowledged here and excluded from the gate by full sha
(see `GRANDFATHERED` in the enforcement test):

- `16aed999a3be8074f22a88a53a95925709660d54` — `fleet fairness fix: AI improver before stuck
  non-AI proof_required at equal priority`. Touched `repos.json` without a provenance tag; landed
  before the gate was executed. New watched-file commits still require a tag — this list is
  append-only for genuinely historical commits, never a bypass for new ones.

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

Every line of that output must start with `operator:` or an `rsi`-family tag (merge
commits excepted; commits predating this convention are not retroactively judged).
That property is the check — and it is executed, not aspirational: see the
enforcement test named above.
