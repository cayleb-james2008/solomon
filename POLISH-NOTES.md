# Solomon — polish notes (2026-09-18, Linux host, rustc 1.88.0 / clippy 0.1.88)

## What changed and why

- README: first screen now says what Solomon is, who it is for, and how to try it
  (clone → `cd src-tauri && cargo test` → `cargo run --release`).
- README: added a "What works today" section with the measured Linux numbers (including
  the 2 failing Rust tests and the clippy error count), so a reviewer on Linux sees them
  before running anything.
- README + AGENTS.md: fixed stale test-count claims ("1,100+" in README, "≈400" in
  AGENTS.md) to the reproducible number: 1,232 `#[test]` functions, with the exact grep
  command (`grep -rhoE '#\[(tokio::)?test\]' src-tauri/src | wc -l` from the repo root).
- README + AGENTS.md: fixed the false "no Python venv, no pytest suite" claim. The shipped
  exe is pure Rust, but the repo also contains a live Python `solomon/` package (the v2
  profit-engine experiments, see `CONTEXT.md`) with a 57-test pytest suite in `tests/`.
- `pyproject.toml`: added the missing `pyyaml>=6.0` dependency. `solomon/channels/content.py`
  does `import yaml` at runtime but PyYAML was never declared — 2 content tests failed with
  `ModuleNotFoundError` until PyYAML was installed. No code change needed.
- `src-tauri/src/ops/probe.rs`: compile-time `#[cfg]` gate around the `run_win_shell` /
  `/bin/sh` branch selection. As shipped, the Windows-only `proc::run_win_shell` (gated
  `#[cfg(windows)]` in `control/proc.rs`) was referenced behind a *runtime* `cfg!(windows)`
  check — hard error E0425, so the crate did not build on Linux at all. Behaviour is
  identical on both platforms (Windows keeps the byte-exact cmd.exe path). This is the only
  source change in this pass.
- Hygiene files: LICENSE, CODE_OF_CONDUCT.md, CONTRIBUTING.md, SECURITY.md all already
  exist and are sound (SECURITY.md uses a GitHub Security Advisory link, no invented
  email). `.gitignore` covers `target/`, `node_modules/`, `.env`, `runtime/`, `repos/`,
  `Solomon.exe` artifacts and test scratch dirs — verified with `git check-ignore`.
- Nothing was deleted. Every hygiene candidate is load-bearing or a genuine design doc:
  `repos.json`/`actions.json`/`ops.json` are read by `control/registry.rs`, `ceo/` and
  `deploy.rs`; `fleet_ledger.jsonl` is auto-provisioned at boot by `ops/fleet_ledger.rs`;
  `improver/*` lane contracts are consumed by the RSI loop and CEO planner;
  `SOLOMON_RSI.md` is referenced by `control/apptest_health.rs` as a repo marker;
  `CONTEXT.md`/`AGENTS.md` are the operator/agent guides; `.claude/` and `.ultra-code/`
  are internal planning docs (and `housekeeping.rs` knows about `.claude/worktrees/`).
  CHANGELOG.md was not added: the git history mixes two product lines (Rust harness +
  Python profit engine) and a factual changelog could not be attributed without guessing.
- CI (`.github/workflows/ci.yml`, `release.yml`): both are valid YAML (checked with
  `yaml.safe_load`) and every referenced path/script/command exists (`src-tauri/`,
  `cargo test`, `cargo clippy --all-targets -- -D warnings`, root-level `python pecrt.py`,
  `projectPath: src-tauri`). Both target `windows-latest`, so they are NOT RUN on this
  Linux host by design, not by defect. No workflow was changed.
- Claims removed or softened: "1,100+ unit tests" → exact 1,232 with command; "no Python
  venv, no pytest suite" → accurate two-Pythons description; "≈400 unit tests" (AGENTS.md)
  → 1,232 with command. No other superlatives found. GitHub badge URLs
  (`github.com/cayleb-james2008/solomon`) could not be verified — this checkout has no git
  remotes by design — but they are the author's declared location and were left as-is.

## Verified by running (exact command + real result)

All exit codes below are the command's own status from unpiped runs (`> log 2>&1`), except
where noted. (An earlier round used `cmd | tail`, whose `$?` was tail's status — those EXIT
lines were discarded and are not reported here.)

- `python3 pecrt.py` (repo root), exit 0 — `pecrt.py self-check: OK (all decision-mirror
  checks passed)`. VERIFIED.
- `/tmp/ossrecon/solomon-venv/bin/python -m pytest tests/ -q` (isolated venv with
  python-dotenv, pydantic, httpx, rich, openai, pytest-asyncio, pyyaml), 57 passed in
  0.58s. VERIFIED. (System python lacks `dotenv`, so the suite does not run without deps;
  with deps but without PyYAML, 55 passed / 2 failed on `ModuleNotFoundError: yaml` —
  the missing-declaration bug fixed above.)
- `cargo test` (in `src-tauri/`, unpiped, exit 101) —
  `test result: FAILED. 1189 passed; 2 failed; 1 ignored`. Both failures are
  Windows/toolchain assumptions, classified honestly, code left untouched:
  (a) `improver::gates::tests::lint_gate_auto_corrects_fmt_in_loop` (gates.rs:1622) —
  toolchain drift in a test fixture: the `linttest` fixture fails this host's clippy
  0.1.88 with `uninlined_format_args` (implied by `-D warnings`), so the auto-fix loop
  exhausts its budget and never converges;
  (b) `ops::registry::tests::resolve_path_expands_env_repo_and_relative` (registry.rs:212) —
  asserts `C:/ops_test_base/data/x.json` is absolute; on Linux it joins to the tmp home
  (`/tmp/solomon_test_home_<pid>/C:/...`), i.e. a Windows-assumption failure.
- `cargo clippy --all-targets -- -D warnings` (in `src-tauri/`, unpiped, exit 101) —
  `error: could not compile `solomon` (bin "solomon" test) due to 25 previous errors`.
  Breakdown from the full log: 24× `uninlined_format_args` (newer-clippy style lint),
  1× unused `super::*` (oneshot.rs:665), 1× unused `crate::control::proc` (visual.rs:38 —
  used inside Windows-only blocks, kept deliberately), 1× `assert!(true)`. Lints left
  untouched; the author's Windows CI toolchain does not emit them.
- `grep -rhoE '#\[(tokio::)?test\]' src-tauri/src | wc -l` → 1232 (zero `tokio::test`;
  all plain `#[test]`). `cargo test` executes 1192 in the binary target (1189+2+1);
  the remainder are in other targets / cfg-gated modules.
- `python3 -c "import yaml; yaml.safe_load(...)"` on both workflow files → valid YAML.
- `git remote -v` → empty (no remotes, as required). No push performed.

## Remains untested / NOT RUN and why

- `.github/workflows/ci.yml` + `release.yml` — require `windows-latest` runners; NOT RUN
  on this Linux host by design.
- Tauri GUI dashboard and NSIS/`latest.json` release pipeline — Windows-only (WebView2).
- Lane-agent LLM calls, ntfy topics, live lane probes — need API keys / external services.
- `ort` / `lancedb` stack entries — unpinned future extension points, not features.
- CHANGELOG.md — intentionally not added (see above).
