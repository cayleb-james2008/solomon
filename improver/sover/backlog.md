# Sover backlog

- [x] Add a roundtrip test for `strategy.save_strategy()` that verifies atomic write, reload preserves values, and omitted `auto_post` stays `False`. (shipped: PR #1, merged into main)
- [ ] Add tests for `config.asset()` override resolution (per-profile asset wins over shared asset; missing shared path is returned gracefully).
- [ ] Add tests for `jsonstore.locked()` concurrent acquisition and stale-lock steal behavior using temporary paths.
- [ ] Add a characterization test for `frozen_guard.baseline()`/`verify()` that records and detects a single-bit change in a frozen file.
- [ ] Add a smoke test for the FastAPI app factory (`api.app:create_app()`) that confirms expected routers are wired and the root redirect resolves.
- [ ] Create a `pyproject.toml` or `requirements.txt` listing the runtime/test dependencies (FastAPI, uvicorn, python-dotenv, Pillow, etc.) discovered in the source.
- [ ] Harden `brand_safety.check_text()` tests for substring edge cases (e.g., allowed words containing denylisted substrings).
- [ ] Add a test for `dgm._static_check()` catching additional sandbox-escape patterns (import aliases, `exec`/`compile`, `__import__`).
- [ ] Add a test for `scorer.score()` behavior when the post registry or metrics directory is missing entirely.
- [ ] Extract hardcoded Windows font paths from `config.py` into profile-overridable strategy defaults and add a fallback test.