# Sover self-improvement contract

Sover is an autonomous social-media brand supervisor: a Python/FastAPI desktop app that connects to a logged-in browser, learns an account's posting style, and proposes/executes content while keeping money and core code human-gated. This agent's purpose is to ship one small, real, verified improvement each run — usually a test, a safety invariant, or a pure refactor — that moves the project toward reliable, multi-profile autonomous operation without touching secrets, CI, or build machinery.

## Your job this run (exactly one improvement)

Implement the first backlog item: **add a roundtrip test for `strategy.save_strategy()` that verifies atomic write + reload preserves values and never silently flips `auto_post`**. The test should live in `tests/test_strategy.py`, redirect `strategy.STRATEGY_PATH` to a temporary directory (as the existing tests already do), write a known dict via `save_strategy()`, reload via `load_strategy()`, assert the values roundtrip, and assert `auto_post` remains off when omitted. Keep it a pure unit test (no browser, no network, no real profile). Run the gate (`python -m unittest discover -s tests -t tests`) and confirm green. End with a 2–4 sentence summary of what changed and why it matters.

## Rules

- Do NOT run `git` or `gh`; never push, merge, or create branches. The runner owns version control.
- Stay in product source, tests, and docs. Do not touch `.github/`, secrets (`.env`), CI files, or build files (`bin/ggg_app.spec`, PyInstaller specs, `ecosystem.config.js`).
- Keep tests portable: no GUI, no network calls, no real browser, no undeclared dependencies, and no reliance on a particular profile being present.
- Keep changes shippable: one coherent improvement, minimal diff, all existing tests still pass.
- Do not edit the frozen core (`dgm.py`, `scorer.py`, `brand_safety.py`, `master.py`) unless the backlog item explicitly requires it; prefer tests and small pure helpers.

## Cross-platform tests

The real gate detected in the repo is:

```bash
.venv/Scripts/python.exe -m unittest discover -s tests -t tests
```

(Equivalent cross-platform form: `python -m unittest discover -s tests -t tests`.) The suite currently runs 69 tests and passes.

## Map of the code

- `scripts/` — the Python core. Bare imports (`import config`, `import dgm`, etc.) are resolved because `run_api.py` and `tests/_bootstrap.py` put this directory on `sys.path`.
  - `run_api.py` — FastAPI entry point; loads `.env`, fixes `sys.path`, calls `api.app:create_app()`, and runs uvicorn on `127.0.0.1`.
  - `api/app.py` + `api/routes/*.py` — FastAPI factory and route modules (health, profiles, cockpit, lanes, proposals, providers, onboarding, capabilities, chat, update, control, jobs, loop_health).
  - `config.py` — per-profile workspace/paths; reads active profile via `sover_profile.py`.
  - `sover_profile.py` — profile selection by `SOVER_PROFILE` env var; isolates state under `data/<id>/` and `.runtime/<id>/`.
  - `strategy.py` — single source of truth for strategy; `load_strategy()` merges `data/strategy.json` over safe defaults; `save_strategy()` is atomic.
  - `dgm.py`, `scorer.py`, `brand_safety.py`, `master.py` — the frozen core (self-improvement, scoring, safety guardrails, posting/rate/money).
  - `jsonstore.py` — atomic JSON/text writes and cross-process advisory locks.
  - `frozen_guard.py` — SHA256 manifest + read-only bit for the frozen core.
  - `ggg_supervisor.py`, `run_pi_ggg.py` — process supervisor and the per-profile Chief Growth Officer loop.
  - `mutable/pickers.py` — the only writable code surface the self-improvement loop may edit.
- `bin/ggg_app.py` — desktop app wrapper / PyInstaller entry point.
- `dashboard/dist/` — static dashboard files served by the API; `dashboard/prototype/` contains design prototypes.
- `profiles/` — profile registry and per-profile `profile.json` + `SOUL.md`.
- `capabilities/example_pulse/` — template for self-extending capability proposals.
- `tests/` — `unittest` suite. `_bootstrap.py` adds `scripts/` to `sys.path` so bare imports work under test.
- `data/`, `.runtime/`, `logs/`, `content/` — runtime state (gitignored / per-profile).