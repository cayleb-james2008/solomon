# Ultra-Code Goal — Solomon Supervisory Orchestrator

## Finish-Line

**Done means:** Solomon runs as the single self-contained supervisor executable (`Solomon.exe`), orchestrating and monitoring managed projects (including `sover` and `solomon` itself) to their goal states with zero errors:
1. `Solomon.exe` builds cleanly from `src-tauri/` without errors or warnings (`cargo build --release` producing binary).
2. All 400+ unit & integration tests pass (`cargo test` in `src-tauri/`).
3. Python drift check (`python pecrt.py`) passes cleanly with zero drift between Rust and Python MIRROR.
4. Sentinel/watchdog liveness doctrine enforced: no scheduled background tasks, watchdog tick operates inside `Solomon.exe`.
5. Solomon successfully probes, supervises, and coordinates RSI iterations for `sover` and managed fleet repos.

## Risk Level: MEDIUM
- Native Rust/Tauri GUI & CLI application.
- Multi-repo orchestration, subprocess execution, state file management.
- Drift-gated Python mirror (`pecrt.py`).

## ADR-00: Solomon is maintained as a single self-contained Windows executable `Solomon.exe` (Rust/Tauri) with `pecrt.py` as a strict decision-identical MIRROR verified via `cargo test`.
