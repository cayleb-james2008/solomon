# Solomon self-improvement contract

Solomon is the RSI orchestrator/supervisor itself: a Rust/Tauri app (`src-tauri/`) that provisions,
schedules, gates, ships, and recovers improvements for the OTHER repos it manages (asmodeus, dotz,
maki, sover) via a per-repo pi agent, plus a watchdog that restarts crashed lanes and a recovery
ladder (`diagnose()`/`recover()`) that classifies and fixes common failure states. This contract is
for improving SOLOMON'S OWN code — the orchestrator, not a managed project.

## Your job this run (exactly one improvement)

Pick the single highest-leverage real bug or hardening opportunity in Solomon's own codebase, with
priority on the classes of problem the operator has actually hit in production:

- **Silent config drift**: a per-repo `provider`/`model`/`api_key` in `repos.json` going out of
  sync with no loud error (see the `key_shape_mismatch()` guard already added in `improver/ctx.rs`
  as a precedent for this kind of fix — there may be other unvalidated cross-field config states).
- **Lane thrash without a real fix**: a lane restarting repeatedly (e.g. on `gh_not_ready` or
  `unknown_error`) without the watchdog distinguishing "this is transient, back off" from "this is
  structural, escalate now."
- **Stale heartbeat/escalation state**: a lane that self-stopped on a real failure that was later
  fixed, but whose `heartbeat.json`/`stop` sentinel never gets re-observed as green because nothing
  re-triggers a gate run after a manual fix lands.
- **Monitoring theater**: a green internal signal (heartbeat, lane iteration) standing in for a
  real outcome (a post published, a trade filled, a training run fired). The ops plane's probes
  are the honest signal; a green heartbeat with red ops probes is monitoring theater. Harden the
  supervisor + ops plane against it.

Otherwise: fix a genuine bug, harden gate execution / lock & heartbeat handling / subprocess
lifecycle, improve crash-recovery or the dashboard, or add missing test coverage. One coherent
change per run. Keep `cargo test` (run from `src-tauri/`) green.

## TOOL USE — you MUST write code with the tools, not narrate it

**You are a coding agent with file-editing tools.** Do NOT describe what you would change in
prose — actually USE the tools to edit files. A response that narrates a change without invoking
edit/write/bash tools is a no-op failure; the runner detects an unchanged git tree and counts the
iteration as wasted.

## Rules

- Do NOT run `git`, `gh`, or any push/merge — the runner owns version control and ships via PR
  (`pr_target_branch: main`, gated on `cargo test`).
- Keep ponytail/simplicity-first: stdlib over deps, native over custom, delete over add.
- Much of the codebase is a deliberately bug-for-bug faithful port of an earlier Python
  implementation (per-call-site truthiness mirrors, exact log strings, byte-identical argv) — do
  NOT "simplify" those away without understanding why they're there.
- Never touch a MANAGED repo's working tree directly (the keystone invariant in `AGENTS.md`) —
  this contract is for `src-tauri/`, `web/`, and Solomon's own docs only.
- A truthful "nothing worth changing this cycle" beats a fabricated or cosmetic change.

## Frozen-core oracle paths (READ-ONLY to this lane — operator-ratified 2026-07-18)

Solomon's self-improvement lane writes harness code that ENFORCES the oracle (the gate, the
anti-gaming check, the drift gate, the money-out guard, the quarantine decision). An improver
that grades its own grader always "succeeds" — so these paths are **READ-ONLY** to this lane
after operator ratification. A commit that touches one of them MUST carry an `operator:`
provenance prefix (explicit human sign-off); an autonomous `rsi:`-family tag is NOT sufficient
and the `frozen_core_commits_carry_operator_provenance_tags` build-gate test in
`src-tauri/src/provenance.rs` will fail the gate on it.

**File-level frozen core** (see `FROZEN_CORE` in `provenance.rs` — the build gate enforces these):
- `src-tauri/src/pecrt/{drift.rs,safety.rs,warm.rs,bus.rs,mod.rs}` + `pecrt.py` + `pecrt_golden.json`
  — the dual-implementation drift gate.
- `src-tauri/src/money_guard.rs` — the NO-MONEY-OUT guard.
- `src-tauri/src/improver/gates.rs` — the anti-gaming gate (file-level frozen; the function-level
  contract below is the real boundary — the file gate is the coarse backstop).
- `src-tauri/src/improver/progress.rs` — the quarantine DECISION logic (`progress_key`,
  `QUARANTINE_STRIKES`, the strike counter in `record_outcome`). Adding a NEW `record_outcome`
  CALL SITE (e.g. in `fleet.rs`) is allowed; editing the quarantine decision logic is not.
- `docs/rsi/PROVENANCE.md` + `src-tauri/src/provenance.rs` — the provenance convention + this
  tripwire itself.

**Function-level frozen core** (the file-level gate's coarse backstop; the pi-agent must not
edit these specific decision functions even within a non-frozen file):
- `gates.rs::anti_gaming_reason` + `gates.rs::new_skip_markers` — the reward-hacking verdict.
- `progress.rs::progress_key` + `progress.rs::record_outcome`'s strike-counter branch — the
  3-strike quarantine decision.
- `money_guard.rs::classify` — the money-out verdict.
- `pecrt/safety.rs::classify_schedule` — the pecrt safety verdict.

**Not frozen** (this lane MAY edit these — they are the dispatch path, not the oracle):
- `src-tauri/src/fleet.rs` — the autopilot dispatch path (adding a `progress::record_outcome`
  call site is allowed; the call target's logic is frozen, the call site is not).
- `src-tauri/src/improver/iteration.rs` — the iteration loop (same: call sites are editable,
  the called decision functions are not).
- `src-tauri/src/ceo/` — the CEO planes.
- `src-tauri/src/supervisor.rs` — the recovery ladder.
- `src-tauri/src/watchdog.rs` — the sweep loop.
- `src-tauri/src/control/` — registry, runner, proc, heartbeat, locks, branches, gh, keys, paths.
- `web/`, `runtime/`, `tools/` — the dashboard, runtime state, tooling scripts.

**Self-reference hazard**: this lane writes the harness code that ENFORCES the oracle. The
file-level build gate (`frozen_core_commits_carry_operator_provenance_tags`) is the mechanical
barrier; the function-level contract above is the prose reinforcement. BOTH apply. A change
that weakens the gate, the anti-gaming check, the pecrt drift gate, the money-out guard, or the
quarantine decision logic — even if it would make the lane "succeed" — is a self-dealing
violation and the gate reverts it.

## Map of the code

- `src-tauri/src/main.rs` — entrypoint, CLI subcommand dispatch (`run-improver`, `watchdog`,
  `state`, `start`, `stop`, `supervise`, `serve-health`).
- `src-tauri/src/control/` — `registry.rs` (repos.json read/write), `runner.rs`, `proc.rs`
  (subprocess spawning, env stripping), `heartbeat.rs`, `locks.rs`, `branches.rs`, `gh.rs`,
  `keys.rs`, `paths.rs`, `contracts.rs`, `apptest_health.rs`.
- `src-tauri/src/improver/` — the RSI loop itself: `run.rs` (main loop + preflight), `ctx.rs`
  (per-iteration context, provider/key resolution and validation), `gates.rs` (gate command
  execution), `gitops.rs`, `iteration.rs`, `phases.rs`, `ship.rs` (PR vs local-commit shipping),
  `oneshot.rs`, `backlog.rs`, `mod.rs`, `visual.rs`.
- `src-tauri/src/supervisor.rs` — the recovery ladder: `diagnose()` classifies lane health
  (ok/stale_lock/stop_lingering/dirty_tree/stuck/revert_failed/gate_red_streak/no_key/gh_not_ready),
  `recover()` walks RUNG0 (deterministic, reversible) -> RUNG1 (opt-in PR-gated pi fix-session) ->
  RUNG2 (escalate with copy-paste git steps, never destructive).
- `src-tauri/src/watchdog.rs` — the sweep loop that restarts crashed lanes and writes
  `runtime/_watchdog.out.log`.
- `src-tauri/src/api.rs` — Tauri command handlers bridging the Rust backend to `web/`.
- `web/` — the dashboard frontend (designed in claude.ai/design, ported here).
- `repos.json` — the live per-repo lane registry (path, provider, model, api_key, gate, ship,
  goal, pipeline). This file is operator-editable config, not source — treat edits to it as
  data changes, not code changes, and never weaken a `gate` command to make a lane pass.
- `improver/<name>/AGENT.md` + `backlog.md` — per-lane contract + task queue for each managed
  repo (and this file, for Solomon's own lane).
