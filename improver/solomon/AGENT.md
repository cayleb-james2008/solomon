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
- **The `paused` sentinel not actually pausing a running iteration**: `paused` only blocks the
  watchdog from *restarting* a crashed loop — it does not stop an already-running one. The
  documented clean-stop is `runtime/<lane>/stop`, but an operator (or future agent) reaching for
  `paused` expecting an immediate halt will be surprised. Consider whether `paused` should also
  write `stop` for a currently-running lane, or whether the docs/UI should make the distinction
  explicit.

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
