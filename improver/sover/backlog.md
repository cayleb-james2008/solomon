# Sover backlog

- [x] Add a roundtrip test for `strategy.save_strategy()` that verifies atomic write, reload preserves values, and omitted `auto_post` stays `False`. (shipped: PR #1, merged into main)
- [x] Add tests for `config.asset()` override resolution (per-profile asset wins over shared asset; missing shared path is returned gracefully). (shipped: PR #2)
- [x] Add tests for `jsonstore.locked()` concurrent acquisition and stale-lock steal behavior using temporary paths.
- [x] Add a characterization test for `frozen_guard.baseline()`/`verify()` that records and detects a single-bit change in a frozen file.
- [x] Add a smoke test for the FastAPI app factory (`api.app:create_app()`) that confirms expected routers are wired and the root redirect resolves.
- [x] Create a `pyproject.toml` or `requirements.txt` listing the runtime/test dependencies (FastAPI, uvicorn, python-dotenv, Pillow, etc.) discovered in the source.
- [ ] Fix the measurement-seam open loop in `analytics_collector.collect()` + `supervisor.health_audit()`. A post whose early metric reads (3h/24h) fail or never run gets NO `metrics/<id>__<platform>.json` doc, so the `measurement_seam` check (`stale == 0`) flags it as a PERMANENT open loop with no recovery (reproduced: a ~31h-old TikTok post with no metrics). Make `collect()` resilient — a down browser harness must not abort the whole lane — and have it write a terminal, honest `measurement_failed` doc (empty `snapshots`, which `scorer._latest_snapshot` already ignores — never a fabricated zero) for any registered post still missing metrics past a recoverability threshold (~30h), so one un-scrapeable post can't keep the seam red forever. Add a unit test redirecting `config.DATA`/`METRICS` to a temp dir.
- [ ] Harden `brand_safety.check_text()` tests for substring edge cases (e.g., allowed words containing denylisted substrings).
- [ ] Add a test for `dgm._static_check()` catching additional sandbox-escape patterns (import aliases, `exec`/`compile`, `__import__`).
- [ ] Add a test for `scorer.score()` behavior when the post registry or metrics directory is missing entirely.
- [ ] Extract hardcoded Windows font paths from `config.py` into profile-overridable strategy defaults and add a fallback test.
