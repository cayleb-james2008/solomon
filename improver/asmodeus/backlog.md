# Asmodeus backlog

- [ ] Add unit tests for `asmodeus.cli` covering `version`, `kill`, and `unkill` (currently 0% coverage).
- [ ] Add tests for `asmodeus.env.load_env` path resolution and idempotency (currently 29% coverage).
- [ ] Add tests for `asmodeus.runtime.subprocess_util` helpers, especially the Windows-only process helpers (currently 48% coverage).
- [ ] Extend `asmodeus.shell` tests to cover the singleton-lock fail-closed and orphan-survivor abort paths (currently 57% coverage).
- [ ] Add `--once` worker tests for `builder_lane` and `meta_loop` so their main loops are exercised without real sleep/LLM calls.
- [ ] Wire the auto-breaker into paper-mode `run_tick` so the paper shadow exercises the same −10/−15/−30 envelope as live (deferred in ADR-0001).
- [ ] Record actual venue bracket-fill PnL in `_close_phantom` instead of booking realised PnL = 0 (deferred refinement in ADR-0002).
- [ ] Add an `asmodeus status` CLI subcommand that prints current mode, breaker state, kill switch, and latest equity without launching the shell.
- [ ] Add an ADR documenting the no-Guardian / no-Task Scheduler operator directive and the manual relaunch update flow.
- [ ] Add a test that `asmodeus.workers.run_worker` rejects unknown worker names with a clear exit code.