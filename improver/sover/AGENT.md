# Sover self-improvement contract

Sover is an autonomous social-media brand supervisor: each profile gets a dedicated direct-LLM Chief Growth Officer, self-extending capability routes/lanes/subagents, and an operator dashboard. The RSI loop is intentionally scoped to agents + supervisor + brand; the frozen core (`dgm.py`, `scorer.py`, `brand_safety.py`, `master.py`, the money gate, and the hard invariants in `pi/AGENTS.md`) must never be edited by the loop. This contract frames every run as shipping one small, real, verified improvement toward that goal — always with a test and a green gate.

## Your job this run (exactly one improvement)

Populate `next_run` in `.runtime/<profile>/lane_state.json` and surface it in `GET /sover/lanes` so the dashboard Activity tab can show when each lane is scheduled to run next. Today `lane_runner._record()` only writes `last_run`, `last_status`, `detail`, and `enabled`, so the API's `_lanes()` returns `next_run: None` for every row. Add a per-lane interval lookup (core lane intervals in `lane_runner.py`, capability lane intervals from `capabilities.lane_specs()`), compute `next_run = last_run + interval`, and persist it in `_record()`. Update `tests/test_lane_runner.py` to assert `next_run` is populated and plausible, then run the gate and confirm green.

## Rules

- Do NOT run `git` or `gh` and never push/merge — the runner owns version control.
- Stay in the product source / tests / docs; do not touch `.github/`, secrets, or build files (e.g., `bin/sover_app.spec`).
- Keep tests portable: no GUI, no real network calls, no undeclared dependencies, and no live social-media profiles.
- The RSI scope is agents + supervisor + brand only; do not add dashboard panels or dashboard self-injection.
- Keep each change shippable: smallest coherent diff, deletion over addition, and honest commit subjects.
- Frozen-core files are off-limits; if a guard trips, flag a `need` rather than bypassing it.

## Cross-platform tests

The real gate is pytest, configured in `pyproject.toml`. Run it exactly as the repo's own `AGENTS.md` requires:

```bash
env -u PYTHONPATH -u PYTHONHOME SOVER_PROFILE=starter .venv/Scripts/python.exe -m pytest -q
```

The suite must pass before any run is considered complete. Tests exercise the API with FastAPI's `TestClient` and use disposable temp directories, so they stay offline and GUI-free.

## Map of the code

- `scripts/run_api.py` — FastAPI/uvicorn driver; binds `127.0.0.1` and serves the static dashboard.
- `scripts/api/app.py:create_app()` — app factory; wires core routes in `scripts/api/routes/`, auto-discovers active capability routes, and mounts `dashboard/dist`.
- `scripts/api/routes/` — API surfaces: `profiles`, `onboarding`, `chat`, `proposals`, `capabilities`, `autonomy`, `jobs`, `lanes`, `control`, `surface`, `browser`, `providers`, `update`.
- `scripts/run_pi.py` — per-profile CGO loop (`--once` or `--interval`); direct-LLM brief, frozen-core tripwires, bounded action execution, capability scaffold via `capability_plan.propose_next()`, and full-autonomy sweep.
- `scripts/lane_runner.py` — content-engine worker dispatcher; runs one lane and records status in `.runtime/<profile>/lane_state.json`.
- `scripts/supervisor.py` / `scripts/meta_improver.py` — live state gathering and Darwin-Gödel improvement step.
- `scripts/capabilities.py` + `scripts/capability_plan.py` + `scripts/capability_templates.py` — self-extension system: route/lane discovery, dependency-tree planner, and vetted template generation.
- `scripts/onboarding.py` — learns how an account posts, synthesizes a brand draft + `SOUL.md`, and gates content lanes until accepted.
- `scripts/proposals.py` / `scripts/needs.py` / `scripts/review_queue.py` / `scripts/monetization_gate.py` — operator approval gates for ideas, human actions, code/money hard gate, and money streams.
- `scripts/autonomy.py` — full-autonomy sweep (auto-approves only non-money gates).
- `scripts/config.py` + `scripts/sover_profile.py` — per-profile paths and runtime; active profile selected via `SOVER_PROFILE` env var.
- `bin/sover_app.py` / `scripts/standalone.py` / `bin/sover_app.spec` — desktop window, in-process API/CGO/lane scheduler, and PyInstaller spec.
- `scripts/gen_ecosystem.py` / `ecosystem.config.js` — PM2 dev process layout generated from the profile registry.
- `dashboard/dist/cockpit/` — static operator surface (chat + Browser/Activity/Approvals tabs). It is NOT an RSI target.
- `tests/` — pytest suite; `tests/conftest.py` and `tests/_bootstrap.py` set `sys.path` so `scripts/` modules resolve.
- `pi/SYSTEM.md`, `pi/CONTROL.md`, `pi/ONBOARD.md`, `pi/AGENTS.md` — operating procedures and hard invariants for the CGO, chat, onboarding, and agents.
- `AGENTS.md` (repo root) — top-level entry guide; read it first.