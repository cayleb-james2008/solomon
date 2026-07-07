# Solomon

Solomon is the **RSI control plane**: a single self-contained Rust/Tauri binary that provisions,
schedules, gates, ships, and recovers autonomous recursive-self-improvement (RSI) loops for the
repos it manages. It is the improver runner, the supervisor recovery ladder, the overnight
watchdog, the git/`gh` integration, the CEO planner, and the web dashboard — one native exe, no
Python, no companion runtime.

The canonical loop spec and its invariants live in [`SOLOMON_RSI.md`](./SOLOMON_RSI.md); the
operator/agent guide lives in [`AGENTS.md`](./AGENTS.md). Read both before touching the harness.

## What it does

Each managed repo gets a lane. A lane runs an event-driven improvement loop: observe the repo's
real metric and objective data, pick the highest-value backlog item, have an agent implement it,
run the project-native test gate, and only ship (commit + push) if the gate is honestly green.
The supervisor diagnoses and recovers stuck lanes; the watchdog sweeps for stalls; a day-gated CEO
rhythm plans and reports across the fleet. The loop is designed to grow one real metric per repo,
cycle after cycle, without gambling the project, gaming its own metric, or reporting false progress.

## Architecture

- **Event-driven wake.** A lane does not sleep a fixed interval blind to outcomes. Each iteration
  ends in a `continue_or_park` self-grade with a hard `MAX_PARK_FLOOR` ceiling (5 min), so a lane
  re-observes its stop flag, live config, and freshness at least that often.
- **CEO org + profit objective.** A day-gated planner (`ceo/`) allocates attention across lanes
  (`allocate`, `focus`, `scale`) toward each repo's real objective. Lanes carrying live money score
  conservatively — an equity lane is never auto-scaled from its own metric.
- **Blast-radius discipline.** Ship is gated on an honest-green test gate; empty or substanceless
  gates are forced RED. Deploy is human-gated: a repo without explicit `live_deploy` is never
  auto-deployed.
- **Recovery ladder.** `supervisor.rs` diagnoses stuck lanes and walks a recovery ladder;
  `watchdog.rs` runs the periodic stall sweep inside the open exe (no OS scheduled task).

## HARD gate: NO MONEY OUT (fail-closed)

Solomon **never moves money out**. No withdrawal, transfer, deposit, funding, purchase, payment,
paid signup, or ad spend is ever performed autonomously. This is enforced by a single mandatory,
preemptive chokepoint (`src-tauri/src/money_guard.rs`), not merely stated in docs:

- It is a **pure, fail-closed predicate**: any unknown or ambiguous money-capable action is
  **DENIED** by default, so a future money tool is blocked until a human explicitly whitelists it.
- The **only** permitted money action is a `place_trade` dispatched by a whitelisted live-money
  lane's own bot binary (asmodeus / kairos — Kalshi + futures). Solomon itself never reaches for a
  money tool.
- Today the money surface is **NONE** — there is no stripe/paypal/withdraw/payout/checkout
  integration anywhere, and the frontend API is a closed, money-free set. The guard welds that hole
  shut before it can ever be cut, and pages the operator on any denied attempt.

## Honest status

- **Solomon (the harness) is real and running.** The Rust/Tauri backend, improver loop, supervisor,
  watchdog, git/`gh` integration, and dashboard exist and are exercised by the `cargo test` gate.
- **The AI-CEO / autonomous-profit vision is in progress, not achieved.** The CEO org, profit
  objective, and multi-lane allocation are implemented as scaffolding and planning rhythm; Solomon
  does **not** claim autonomous profit. No verified real-money profit is asserted here. Live-money
  lanes (asmodeus/kairos) run their own bots under the human-gated money-out doctrine above.
- **Safety posture is fail-closed by design:** honest-green ship gate, human-gated deploy, and the
  NO-MONEY-OUT chokepoint. When in doubt, the harness refuses rather than guesses.

## Quickstart

All commands run from `src-tauri/`. The app is pure Rust — there is no Python venv or pytest suite.

```sh
# Run the test gate (the gate every PR must pass)
cd src-tauri && cargo test

# Launch the dashboard (Tauri GUI; no args -> GUI mode)
cargo run --release          # -> src-tauri/target/release/solomon.exe

# Build the single shipped exe (copy it to the repo-root Solomon.exe)
cargo build --release

# One RSI iteration on a repo (headless, single pass)
solomon run-improver --repo <path> --name <name> --once

# One watchdog sweep on demand (the automatic sweep runs inside the open Solomon.exe;
# no scheduled task exists and none may be created — operator rule)
solomon watchdog

# CEO rhythm on demand (the day-gated runs ride the in-app watchdog tick)
solomon plan | report

# Headless status / control
solomon state | start <name> | stop <name> | supervise [name] | serve-health [port]
```

Operator configuration lives in `repos.json` (the managed lanes), `improver/<name>/AGENT.md` +
`backlog.md` (each lane's contract), and `.env` (the Ollama Cloud API key for the improver). See
`.env.example` for the key name. Secrets (`.env`, `.solomon.json`) and runtime state (`runtime/`,
managed clones under `repos/`) are gitignored and never committed.

## Layout

- `src-tauri/src/` — the native backend: `main.rs` (CLI dispatch), `control/` (registry, git/`gh`,
  locks, runner, heartbeat, keys), `improver/` (the RSI loop + `park` pacing), `ceo/` (the CEO org),
  `money_guard.rs` (the NO-MONEY-OUT chokepoint), `supervisor.rs` (diagnose/recover),
  `watchdog.rs` (the sweep), `api.rs` (the `bridge` command + headless backend).
- `web/` — the dashboard frontend (`index.html`, `app.js`, `styles.css`), served in-process.
- `improver/` — per-lane agent contracts + backlogs (including `solomon/` for self-improvement).
- `runtime/` — per-lane runtime state (locks, heartbeats, browser state); gitignored.
- `docs/` — schemas and the AI-CEO architecture plan.
