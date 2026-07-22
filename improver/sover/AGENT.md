# Sover self-improvement contract

Sover is an autonomous social-media brand supervisor: each profile gets a dedicated direct-LLM
Chief Growth Officer, self-extending capability routes/lanes/subagents, and an operator dashboard.
The RSI loop is intentionally scoped to agents + supervisor + brand; the frozen core (`src/engine/catalog.rs`,
`src/engine/cgo.rs`, the money gate, and the hard invariants in `AGENTS.md`) must never be edited by
the loop. This contract frames every run as shipping one small, real, verified improvement toward
that goal — always with a test and a green gate.

## Your job this run (exactly one improvement)

**Audit and harden the lane freshness watchdog.** The `next_run` field is already populated in
`lane_state.json` by `src/engine/lanes.rs::record()` (computes `next_run = last_run + interval`
from a single UTC instant) and surfaced in `GET /sover/lanes` (`src/api/lanes.rs`). The freshness
watchdog in `src/capabilities/ops_watchdog.rs` checks that each enabled, schedulable lane whose
`last_run` exceeds its interval is flagged. Review the watchdog's staleness threshold logic,
identify an edge case (e.g. a lane that ran but wrote an empty `last_run`, or a disabled lane
whose `last_run` is stale but should not trigger a watchdog alert), add a test for it, run the
gate, and confirm green.

## Rules

- Do NOT run `git` or `gh` and never push/merge — the runner owns version control.
- Stay in the product source / tests / docs; do not touch `.github/`, secrets, or build files (e.g., `bin/sover_app.spec`).
- Keep tests portable: no GUI, no real network calls, no undeclared dependencies, and no live social-media profiles.
- The RSI scope is agents + supervisor + brand only; do not add dashboard panels or dashboard self-injection.
- Keep each change shippable: smallest coherent diff, deletion over addition, and honest commit subjects.
- Frozen-core files are off-limits; if a guard trips, flag a `need` rather than bypassing it.

## Cross-platform tests

The real gate is `cargo test --bin sover` (hermetic: no network, no browser, no live LLM). Run it
exactly as the repo's own `AGENTS.md` requires:

```bash
cargo test --bin sover
```

The suite must pass before any run is considered complete. Tests exercise the engine and API with
disposable temp directories, so they stay offline and GUI-free.

## Map of the code

- `src/main.rs` — binary entry point; CLI dispatch.
- `src/api/app.rs` — app factory; wires core routes in `src/api/routes/`.
- `src/api/routes/` — API surfaces: `profiles`, `onboarding`, `chat`, `proposals`, `capabilities`, `autonomy`, `jobs`, `lanes`, `control`, `surface`, `browser`, `providers`, `update`.
- `src/engine/supervisor.rs` / `src/engine/cgo.rs` — live state gathering and Darwin-Gödel improvement step.
- `src/engine/lanes.rs` — content-engine worker dispatcher; runs one lane and records status in `<runtime>/lane_state.json`. Writes `last_run`, `next_run`, `last_status`, `detail`, `enabled`.
- `src/engine/catalog.rs` — lane catalog and scheduling.
- `src/capabilities/ops_watchdog.rs` — lane freshness watchdog: flags enabled, schedulable lanes whose `last_run` exceeds their interval.
- `src/capabilities/` — self-extension system: route/lane discovery, dependency-tree planner, and vetted template generation.
- `src/onboarding.rs` — learns how an account posts, synthesizes a brand draft + `SOUL.md`, and gates content lanes until accepted.
- `src/proposals.rs` / `src/needs.rs` / `src/review_queue.rs` / `src/monetization_gate.rs` — operator approval gates for ideas, human actions, code/money hard gate, and money streams.
- `src/autonomy.rs` — full-autonomy sweep (auto-approves only non-money gates).
- `src/config.rs` + `src/sover_profile.rs` — per-profile paths and runtime; active profile selected via `SOVER_PROFILE` env var.
- `bin/sover_app.rs` / `src/standalone.rs` / `bin/sover_app.spec` — desktop window, in-process API/CGO/lane scheduler, and PyInstaller spec.
- `AGENTS.md` (repo root) — top-level entry guide; read it first.
