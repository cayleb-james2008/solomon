# Asmodeus self-improvement contract

Asmodeus is a lean, Rust autonomous TradeLocker-futures trading system: a frozen harness
(`crates/`) that interprets mutable strategy specs, prompts, and policy living outside the binary
under `paths.data_root()`. Your role each run is to ship exactly one small, real, verified
improvement toward that goal — a test, a wiring fix, a hardening of a sacred floor — and confirm it
against the project gate before finishing.

## Your job this run (exactly one improvement)

**Verify and extend CLI test coverage for `asm-app`.** The CLI (`crates/asm-app/src/cli.rs`)
already has 25+ hermetic tests covering `version`, `kill`, `unkill`, `drain`, `undrain`, `help`,
`readcheck`, `pool-corr`, `fence-report`, and error paths. Review the existing coverage, identify
any gap in the kill-switch lifecycle (e.g. kill → unkill → kill round-trip, kill with stale PID,
unkill when no kill-switch exists), add a test for it, run `cargo test`, confirm green, and end
with a 2–4 sentence summary.

## TOOL USE — you MUST write code with the tools, not narrate it

**You are a coding agent with file-editing tools.** Do NOT describe what you would change in
prose — actually USE the tools to edit files. A response that says "I would add a test to..." or
"the fix is to change..." without invoking the edit/write/bash tools is a
**no-op failure**; the runner detects that you narrated without writing and counts the
iteration as wasted.

- **Read files** with the read tool before editing.
- **Edit files** with the edit/write tool to make your change. Every file you change MUST
  be modified via the tool, not described in text.
- **Run commands** with the bash tool (e.g. the test gate) to verify.
- **Do NOT summarize actions you did not take.** If you did not invoke the edit tool, the
  file was not changed — saying "I added a test" in your summary when you did not use the
  tool is a hallucination. The runner checks the git tree; a clean tree means you wrote
  nothing, regardless of what your text says.

## Rules

- Do NOT run `git`, `gh`, or any push/merge. The runner owns version control.
- Stay in product source, tests, and docs. Do not touch `.github/`, secrets, build files, or the packaged `web/dist` SPA.
- Keep tests portable: no GUI, no network, no undeclared dependencies.
- One coherent change per run. Add or update a test for it. Keep it shippable.
- Never weaken the sacred floors in `ASMODEUS.md` (#3 and #4 in particular); execution-layer changes require a paired test + ADR note.

## Cross-platform tests

`cargo test` (configured in `Cargo.toml`, 15 crates workspace). The current suite passes in ~6s.
Target: still green after your change.

## Map of the code

- `crates/asm-app/src/cli.rs` — console entry point (`asmodeus version|kill|unkill|drain|undrain|readcheck|pool-corr|fence-report`). Import-light, hermetic tests inline.
- `crates/asm-app/src/main.rs` — binary entry point; dispatches to `cli::run()`.
- `crates/asm-app/Cargo.toml` — workspace member manifest.
- `crates/asm-exec/src/` — execution layer: breaker, capital_guard, sizing, allocator, killswitch, mode, broker (paper + TradeLocker).
- `crates/asm-fleet/src/` — hardcoded deterministic cells (ORB, IBS) + indicators + regime + registry.
- `crates/asm-builder/src/` — AI strategy builder: spec DSL, interpreter, backtest, funnel, evaluate, store.
- `crates/asm-rsi/src/` — recursive self-improvement: meta loop, archive, policy, yield_score, external_ideas.
- `crates/asm-market/src/` — OHLCV bars and feed (synthetic + TradeLocker venue history).
- `crates/asm-runtime/src/` — supervisor, heartbeats, position monitor, breaker evaluation, fleet loop, subprocess utilities.
- `crates/asm-harness/src/` — LLM client, model registry, pi launcher.
- `crates/asm-paths/src/` — single source of truth for mutable state root (`ASMODEUS_HOME` / `%LOCALAPPDATA%\Asmodeus` / `~/.asmodeus`).
- `tests/` — integration test suite (if present).
- `docs/adr/` — architecture decision records.
- `ASMODEUS.md` — operating contract, sacred floors, freeze line.
