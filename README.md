# Solomon

**Fleet autopilot for recursive self-improvement: one native exe that provisions, schedules, gates, ships, and recovers RSI loops across the repos it manages.**

[![ci](https://github.com/cayleb-james2008/solomon/actions/workflows/ci.yml/badge.svg)](https://github.com/cayleb-james2008/solomon/actions/workflows/ci.yml)
[![release](https://github.com/cayleb-james2008/solomon/actions/workflows/release.yml/badge.svg)](https://github.com/cayleb-james2008/solomon/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.77%2B-orange.svg)](https://www.rust-lang.org/)

Solomon is the **RSI control plane**: a single self-contained Rust/Tauri binary that provisions,
schedules, gates, ships, and recovers autonomous recursive-self-improvement (RSI) loops for the
repos it manages. It is the improver runner, the supervisor recovery ladder, the overnight
watchdog, the git/`gh` integration, the CEO planner, and the web dashboard — one native exe, no
Python, no companion runtime.

The canonical loop spec and its invariants live in [`SOLOMON_RSI.md`](./SOLOMON_RSI.md); the
agent guide lives in [`AGENTS.md`](./AGENTS.md). Read both before touching the harness.

## What it does

Each managed repo gets a lane. A lane runs an event-driven improvement loop: observe the repo's
real metric and objective data, pick the highest-value backlog item, have an agent implement it,
run the project-native test gate, and only ship (commit + push) if the gate is honestly green.
The supervisor diagnoses and recovers stuck lanes; the watchdog sweeps for stalls; a day-gated CEO
rhythm plans and reports across the fleet. The loop is designed to grow one real metric per repo,
cycle after cycle, without gambling the project, gaming its own metric, or reporting false progress.

## Features

- **One exe, every role.** `Solomon.exe` is the GUI dashboard *and* every headless subcommand
  (`run-improver`, `watchdog`, `probe`, `plan`/`report`, `state`/`start`/`stop`/`supervise`/`serve-health`).
  No Python runtime, no companion process, no dev shell.
- **Honest-green ship gate.** A lane ships only when the project-native test gate passes for real;
  empty or substanceless gates are forced RED.
- **Keystone invariant.** Solomon never hand-patches a managed repo — changes land only via a
  lane's own agent on a gated `rsi/*` branch shipped as a PR (see `SOLOMON_RSI.md`).
- **Self-managed.** Solomon is itself a lane (`improver/solomon/`), improved by the same gated
  PR loop it runs for every other repo.
- **Fail-closed money guard.** A mandatory chokepoint denies any money-out action by default
  (details below).
- **No background processes, ever.** Design doctrine: no schtasks/cron/daemons. The watchdog
  lives inside the visibly open `Solomon.exe` (2-minute tick with an immediate catch-up sweep on
  open); the exe is launched manually, never auto-started.

## Quickstart

All commands run from `src-tauri/`. The shipped app is pure Rust — no Python venv, no pytest suite,
no Python runtime dependency. One exception to "no Python in the repo": `pecrt.py` (repo root) is
the doctrine-mandated decision-identical Python MIRROR of `src-tauri/src/pecrt/` — not a runtime
component. Both implementations are pinned to `pecrt_golden.json` by a drift gate
(`src-tauri/src/pecrt/drift.rs` in `cargo test`, plus `python pecrt.py` self-check); change shared
constants only by updating both sides + the golden together.

```sh
# Run the test gate (the gate every PR must pass)
cd src-tauri && cargo test

# Launch the dashboard (Tauri GUI; no args -> GUI mode)
cargo run --release          # -> src-tauri/target/release/solomon.exe

# Build the single shipped exe (then deploy it safely — see Development & gates)
cargo build --release

# One RSI iteration on a repo (headless, single pass)
solomon run-improver --repo <path> --name <name> --once

# One watchdog sweep on demand (the automatic sweep runs inside the open Solomon.exe;
# no scheduled task exists and none may be created — operator rule)
solomon watchdog

# Ops / CEO rhythm on demand (the day-gated runs ride the in-app watchdog tick)
solomon probe [name]
solomon plan | report

# Headless status / control
solomon state | start <name> | stop <name> | supervise [name] | serve-health [port]
```

Configuration lives in `repos.json` (the managed lanes), `improver/<name>/AGENT.md` +
`backlog.md` (each lane's contract), and `.env` (provider API keys + the ntfy notification topic —
`.env.example` documents every key the code reads). Secrets (`.env`, `.solomon.json`) and runtime
state (`runtime/`, managed clones under `repos/`) are gitignored and never committed.

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

### Layout

- `src-tauri/src/` — the native backend: `main.rs` (CLI dispatch), `control/` (registry, git/`gh`,
  locks, runner, heartbeat, keys), `improver/` (the RSI loop + `park` pacing), `ceo/` (the CEO org),
  `money_guard.rs` (the NO-MONEY-OUT chokepoint), `supervisor.rs` (diagnose/recover),
  `watchdog.rs` (the sweep), `api.rs` (the `bridge` command + headless backend).
- `web/` — the dashboard frontend (`index.html`, `app.js`, `styles.css`), served in-process.
- `improver/` — per-lane agent contracts + backlogs (including `solomon/` for self-improvement).
- `runtime/` — per-lane runtime state (locks, heartbeats, browser state); gitignored.
- `pecrt.py` + `pecrt_golden.json` — the Python decision-mirror of `src-tauri/src/pecrt/` and the
  shared golden constants both implementations must match (drift gate; see Quickstart note).
- `tools/` — operator scripts, chiefly `build_safe.ps1` (the safe build-and-deploy path).
- `docs/` — schemas and the AI-CEO architecture plan (`docs/rsi/`).

## Safety patterns

### HARD gate: NO MONEY OUT (fail-closed)

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
  shut before it can ever be cut, and pages the user on any denied attempt.

### Additional safety guarantees

- **Honest-green ship gate:** a lane ships only when the project-native test gate passes for real.
- **Human-gated deploy:** a repo without explicit `live_deploy` is never auto-deployed.
- **Single-instance enforcement:** refuses a second `Solomon.exe` launch.
- **Recovery, not guesswork:** the supervisor walks a diagnosed recovery ladder; stuck lanes are
  isolated, not silently retried.

## Honest status

- **Solomon (the harness) is real and running.** The Rust/Tauri backend, improver loop, supervisor,
  watchdog, git/`gh` integration, and dashboard exist and are exercised by the `cargo test` gate.
- **The AI-CEO / autonomous-profit vision is in progress, not achieved.** The CEO org, profit
  objective, and multi-lane allocation are implemented as scaffolding and planning rhythm; Solomon
  does **not** claim autonomous profit. No verified real-money profit is asserted here.
- **Safety posture is fail-closed by design:** honest-green ship gate, human-gated deploy, and the
  NO-MONEY-OUT chokepoint. When in doubt, the harness refuses rather than guesses.

## Tech stack

| Layer | Technology |
|---|---|
| **Language** | Rust (2021 edition) |
| **GUI shell** | Tauri 2 (WebView2) |
| **Frontend** | Vanilla HTML/CSS/JS (no framework, no build step) |
| **Database** | `rusqlite` (bundled SQLite, read-only probes) |
| **LLM integration** | OpenAI-compatible endpoints via curl (Ollama, OpenRouter, local) |
| **VLM** | Vision-language model fallback for visual review |
| **Notifications** | ntfy + Windows toast |
| **Updater** | `tauri-plugin-updater` (GitHub Releases) |
| **Windows binary** | `winres`-aware build; single self-contained `.exe` |
| **Async runtime** | Sync-first design (tokio available for future async fan-out) |
| **AI inference** | `ort` (ONNX Runtime) — recommended; not pinned (no stable release yet) |
| **Vector storage** | `lancedb` — available for semantic search over backlogs/docs |

### Rust AI Desktop Tech Stack

Solomon's `Cargo.toml` includes the recommended Rust AI desktop stack for future extension:

- **`ort`** — ONNX Runtime for local, GPU-accelerated model inference (DirectML on Windows).
  Recommended but not yet pinned: the 1.x line is yanked and 2.0 is pre-release — add
  `ort = "2.0.0-rc.13"` when you're ready to pin an RC, or wait for a stable release.
- **`lancedb`** — embedded vector database for semantic search over backlogs and improvement
  history
- **`tokio`** — async runtime (reserved for future parallel LLM fan-out)
- **`tracing` / `tracing-subscriber`** — structured, level-filtered logging
- **`rusqlite`** — bundled SQLite for read-only state probes (no system sqlite3 required)
- **`winres`** — Windows resource compiler for embedding the app icon and manifest

## Configuration

Configuration is layered:

- **`repos.json`** — the managed repo lanes (paths, names, objectives).
- **`improver/<name>/AGENT.md`** — each lane's agent contract.
- **`improver/<name>/backlog.md`** — prioritized work items per lane.
- **`.env`** — provider API keys and notification settings (see `.env.example` for all variables).
- **`.solomon.json`** — runtime secrets (gitignored).

Key environment variables (documented in `.env.example`):

| Variable | Purpose |
|---|---|
| `SOLOMON_LLM_BASE_URL` | OpenAI-compatible LLM endpoint |
| `SOLOMON_LLM_API_KEY` | API key for the LLM provider (use `sk-no-key` for local) |
| `SOLOMON_LLM_MODEL` | Model slug (e.g. `meta-llama/llama-3.3-70b-instruct`) |
| `SOLOMON_VLM_BASE_URL` | Vision-language model endpoint |
| `OPENROUTER_API_KEY` | OpenRouter API key for lane agents |
| `SOLOMON_NTFY_TOPIC` | ntfy.sh topic for push notifications |
| `SOLOMON_MAX_CYCLES` | Max improvement cycles per CEO run (default 1) |
| `SOLOMON_CYCLE_SLEEP` | Seconds between cycles (default 300) |

## Development & gates

- **Test gate:** `cargo test` in `src-tauri/` (1,100+ unit tests in `#[cfg(test)]` modules) is the
  gate every PR must pass. The pecrt drift gate rides inside it — never change
  `pecrt.py` / `src-tauri/src/pecrt/` / `pecrt_golden.json` on one side alone.
- **Safe deploy:** never copy a freshly built exe over a running one by hand. Use
  `powershell -File tools\build_safe.ps1` — cargo writes only to `src-tauri\target\`, and the
  repo-root `Solomon.exe` is replaced only when no solomon process is alive (no `.bak` /
  rename-aside artifacts, ever). `-CopyOnly` and `-CheckOnly` variants exist.
- **Liveness doctrine:** no background processes and no scheduled tasks, ever. The watchdog and
  the day-gated CEO runs ride the tick thread inside the open GUI. `tools/install_sentinel.ps1`'s
  install path is deprecated and refuses; only its `-Uninstall`/`-Status` modes remain.
- **Releases:** pushing a `v*` tag runs `.github/workflows/release.yml` (Windows), which drafts a
  GitHub Release with the NSIS installer + `latest.json` for the in-app `tauri-plugin-updater`.
- **Code style:** ponytail — YAGNI, stdlib first, shortest working diff. Whole-repo `cargo fmt`
  is **not** a gate here; keep diffs surgical. See `AGENTS.md`.

## License

MIT — see [LICENSE](./LICENSE). Contributing guidelines in [CONTRIBUTING.md](./CONTRIBUTING.md);
code of conduct in [CODE_OF_CONDUCT.md](./CODE_OF_CONDUCT.md); security policy in
[SECURITY.md](./SECURITY.md).
