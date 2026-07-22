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
- `d4d73b3e69825c5621deb3b6142b9371ec0ce576` (`fleet: …`), `b85970f1f8f53abcef8d9eb186d77da90ee94b66`
  (`perf: …`), `754e9b05d6ded9f751e005160e727aa3c05b5761` (`publish: …`),
  `89aa5f6563d5bb166c7e45a91d12fb00c6966f3f` (`advisor(w4): …`) — 2026-07-13/15 RSI/advisor and
  fleet-work commits that mutated `repos.json`/`actions.json` with non-conforming subject prefixes.
  Already pushed to `origin/main`, so immutable; recorded here (not laundered) per the no-rewrite
  rule. These are a symptom of a real process gap — the RSI/advisor commit path does not tag
  watched-file commits — which must be fixed at the source so this list stops growing.

 - `574f065c31034c9936510e8331fd7e22e50ef765` (`fix: make live recovery fail closed`) — recovered
  2026-07-22 live-recovery hardening commit. It is recorded here rather than rewritten so the
  recovery branch preserves its original history; new commits remain subject to the tag gate.

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
