# Asmodeus self-improvement contract

Asmodeus is a lean, Python 3.12 autonomous TradeLocker-futures trading system: a frozen harness (`asmodeus/`) that interprets mutable strategy specs, prompts, and policy living outside the binary under `paths.data_root()`. Your role each run is to ship exactly one small, real, verified improvement toward that goal — a test, a wiring fix, a hardening of a sacred floor — and confirm it against the project gate before finishing.

## Your job this run (exactly one improvement)

**Add unit tests for `asmodeus.cli` covering the `version`, `kill`, and `unkill` subcommands.** This module is the operator-facing entry point (`asmodeus --help`, `asmodeus kill`, `asmodeus unkill`) and currently has 0% test coverage. Keep the tests hermetic: mock or isolate the kill-switch file/db side effects, assert return codes and printed output, and ensure `asmodeus.cli:main` can be imported without dragging in heavy runtime dependencies. Add or update tests, run `uv run pytest`, confirm green, and end with a 2–4 sentence summary.

## Rules

- Do NOT run `git`, `gh`, or any push/merge. The runner owns version control.
- Stay in product source, tests, and docs. Do not touch `.github/`, secrets, build files, or the packaged `web/dist` SPA.
- Keep tests portable: no GUI, no network, no undeclared dependencies.
- One coherent change per run. Add or update a test for it. Keep it shippable.
- Never weaken the sacred floors in `ASMODEUS.md` (#3 and #4 in particular); execution-layer changes require a paired test + ADR note.

## Cross-platform tests

`uv run pytest` (pytest 8.x, configured in `pyproject.toml` with `testpaths = ["tests"]`, `asyncio_mode = "auto"`). The current suite passes in ~6s (358 tests). Target: still green after your change.

## Map of the code

- `asmodeus/cli.py` — console entry point (`asmodeus version|kill|unkill`). Kept import-light.
- `bin/app_main.py` — PyInstaller entry point; handles `--worker <name>` dispatch for the frozen exe.
- `asmodeus/shell.py` — app shell: singleton lock, worker supervisor loop, optional pywebview GUI.
- `asmodeus/workers/` — subprocess workers: `backend` (FastAPI/uvicorn), `fleet_ticker` (trading loop), `builder_lane`, `meta_loop`.
- `asmodeus/api/app.py` — FastAPI app (`/health`, `/state`, `/guardian-health`, `/kill`, `/agent_models`, per-aspect views).
- `asmodeus/db/` — SQLite: `conn.py` (WAL, busy timeout), `repo.py` (persistence), `schema.sql`.
- `asmodeus/execution/` — breaker, capital_guard, sizing, allocator, killswitch, mode, broker (paper + TradeLocker).
- `asmodeus/layer1_fleet/` — hardcoded deterministic cells (ORB, IBS) + indicators + regime + registry.
- `asmodeus/layer2_builder/` — AI strategy builder: spec DSL, interpreter, backtest, funnel, evaluate, store.
- `asmodeus/layer3_rsi/` — recursive self-improvement: meta loop, archive, policy, yield_score, external_ideas.
- `asmodeus/market/` — OHLCV bars and feed (synthetic + TradeLocker venue history).
- `asmodeus/runtime/` — supervisor, heartbeats, position monitor, breaker evaluation, fleet loop, subprocess utilities.
- `asmodeus/harness/` — LLM client (`llm.py`), model registry (`models.py`), pi launcher (`pi.py`).
- `asmodeus/paths.py` — single source of truth for mutable state root (`ASMODEUS_HOME` / `%LOCALAPPDATA%\Asmodeus` / `~/.asmodeus`).
- `tests/` — pytest suite. `conftest.py` isolates `$ASMODEUS_HOME` to a tmp path and stubs `.env` loading.
- `scripts/` — dev smoke/probe scripts (e.g., `smoke_backend.py`, `builder_smoke.py`).
- `packaging/` — PyInstaller spec, build script, icon.
- `docs/adr/` — architecture decision records.
- `ASMODEUS.md` — operating contract, sacred floors, freeze line.