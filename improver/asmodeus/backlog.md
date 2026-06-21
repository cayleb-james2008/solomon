# Asmodeus backlog

- [x] Extend `asmodeus.shell` tests to cover the singleton-lock fail-closed and orphan-survivor abort paths (currently 57% coverage).
- [x] Wire the auto-breaker into paper-mode `run_tick` so the paper shadow exercises the same −10/−15/−30 envelope as live (deferred in ADR-0001).
- [x] Record actual venue bracket-fill PnL in `_close_phantom` instead of booking realised PnL = 0 (deferred refinement in ADR-0002).
- [x] Add an `asmodeus status` CLI subcommand that prints current mode, breaker state, kill switch, and latest equity without launching the shell.
- [x] Add an ADR documenting the no-Guardian / no-Task Scheduler operator directive and the manual relaunch update flow.
- [x] Add a test that `asmodeus.workers.run_worker` rejects unknown worker names with a clear exit code.
- [x] Add tests for `asmodeus.env.load_env` path resolution and idempotency (currently 29% coverage).  (deferred: agent could not implement after repeated tries)
- [x] Add `--once` worker tests for `builder_lane` and `meta_loop` so their main loops are exercised without real sleep/LLM calls.  (deferred: agent could not implement after repeated tries)
- [x] Add tests for `asmodeus.runtime.subprocess_util` helpers, especially the Windows-only process helpers (currently 48% coverage).  (deferred: agent could not implement after repeated tries)
- [ ] Add unit tests for `asmodeus.cli` covering `version`, `kill`, and `unkill` (currently 0% coverage).  (deferred: agent could not implement after repeated tries)
