# Solomon

Solomon is the **RSI control plane**: a single self-contained Rust/Tauri binary that provisions,
schedules, gates, ships, and recovers autonomous improvement loops for the repos it manages. It
is the improver runner, the supervisor recovery ladder, the overnight watchdog, the git/`gh`
integration, and the web dashboard — one native exe, no Python, no companion runtime.

The canonical loop spec and its invariants live in [`SOLOMON_RSI.md`](./SOLOMON_RSI.md); the
operator/agent guide lives in [`AGENTS.md`](./AGENTS.md). Read both before touching the harness.

## Quickstart

All commands run from `src-tauri/`. The app is pure Rust — there is no Python venv or pytest suite.

```sh
# Run the test gate (the gate every PR must pass)
cd src-tauri && cargo test

# Launch the dashboard (Tauri GUI; no args -> GUI mode)
cargo run --release          # -> src-tauri/target/release/solomon.exe

# Build the single shipped exe (copy it to the repo-root Solomon.exe)
cargo build --release

# One RSI iteration on a repo (dry-run / headless)
solomon run-improver --repo <path> --name <name> --once

# One watchdog sweep on demand (the automatic every-2-min sweep runs inside the open Solomon.exe;
# no scheduled task exists and none may be created — operator rule)
solomon watchdog

# CEO rhythm on demand (the day-gated runs ride the in-app watchdog tick)
solomon plan | report

# Headless status / control
solomon state | start <name> | stop <name> | supervise [name] | serve-health [port]
```

Operator configuration lives in `repos.json` (the managed lanes), `improver/<name>/AGENT.md` +
`backlog.md` (each lane's contract), and `.env` (the Ollama Cloud API key for the improver). See
`.env.example` for the key name.

## Layout

- `src-tauri/src/` — the native backend: `main.rs` (CLI dispatch), `control/` (registry, git/`gh`,
  locks, runner, heartbeat, keys), `improver/` (the RSI loop), `supervisor.rs` (diagnose/recover),
  `watchdog.rs` (the sweep), `api.rs` (the `bridge` command + headless backend).
- `web/` — the dashboard frontend (`index.html`, `app.js`, `styles.css`), served in-process.
- `improver/` — per-lane agent contracts + backlogs (including `solomon/` for self-improvement).
- `runtime/` — per-lane runtime state (locks, heartbeats, browser state).
- `docs/` — schemas.