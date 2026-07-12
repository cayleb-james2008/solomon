# Solomon — agent guide

Solomon is the **RSI orchestrator/supervisor**: it provisions, schedules, gates, ships, and
recovers improvements for the repos it manages (see `repos.json`), driving each via a per-repo
pi agent under a written contract (`improver/<name>/AGENT.md` + `backlog.md`). It does not write
managed-project code itself (see the keystone invariant below) — but it IS improved by its own
loop: `repos.json` carries a `solomon` entry pointed at this repo, run under the same pi-agent
pattern as the other lanes (`improver/solomon/AGENT.md` + `backlog.md`), shipping via gated PRs
to `main` like every other lane. The canonical loop spec and the 7 invariants live in
**`SOLOMON_RSI.md`** — read it before touching the harness.

## Distribution

**Operator preference: ship a single self-contained Windows exe** — `Solomon.exe`, one native
Rust/Tauri binary built from `src-tauri/` (`cargo build --release` → `src-tauri/target/release/solomon.exe`,
copied to the repo-root `Solomon.exe`; the Tauri bundler produces the NSIS installer). The exe is
both the GUI dashboard and every headless subcommand — there is no separate runtime, dev shell,
Python, or companion exe. In-app updates run through `tauri-plugin-updater` (GitHub Releases). Do
not reintroduce a Python interpreter, a second executable, or an external-runtime dependency
without operator sign-off.

## Run / test / build

All commands run in `src-tauri/`. The shipped app is pure Rust — no Python venv, no pytest suite, no
Python runtime dependency. One exception to "no Python in the repo": `pecrt.py` (repo root) is the
doctrine-mandated decision-identical Python MIRROR of `src-tauri/src/pecrt/`, held in lockstep with
the Rust side by a drift gate (`pecrt_golden.json` + `src-tauri/src/pecrt/drift.rs` in `cargo test`,
plus the `python pecrt.py` self-check). Never edit one side alone — update both + the golden together.

- **Gate / tests:** `cargo test` in `src-tauri/` (≈400 unit tests in `#[cfg(test)]` modules).
- **Dashboard:** `solomon.exe` with no args → the Tauri GUI (WebView2; frontend in `web/`).
- **Build exe:** `cargo build --release` in `src-tauri/` → `src-tauri/target/release/solomon.exe`
  (copy to repo-root `Solomon.exe`); or the Tauri bundler for the NSIS installer.
- **One RSI iteration (dry run):** `solomon run-improver --repo <path> --name <name> --once`.
- **Watchdog sweep:** `solomon watchdog` on demand. LIVENESS DOCTRINE (operator, SETTLED — supersedes the 2026-07-06 "sentinel" renegotiation): **no background processes and no scheduled tasks, ever** (no schtasks/cron/daemons). The watchdog lives INSIDE the visibly-open Solomon.exe — the run_gui tick thread sweeps every 2 min, and because the tick body runs before its first sleep, opening the app performs a catch-up sweep immediately. Solomon.exe is launched manually by the operator, never auto-started. `tools/install_sentinel.ps1` is DEPRECATED (its install path refuses and exits; `-Uninstall`/`-Status` remain for removing a historical task). Each sweep stamps `runtime/_sentinel_heartbeat.json` (dead-man visibility) and rides the janitor (6h) + config-provenance tripwire + controller-clean preflight.
- **Ops / CEO planes (v2):** `solomon probe [name]` (ground-truth outcome probes, ops.json), `solomon plan` / `solomon report` (morning plan / evening verified-outcome summary; the day-gated automatic runs ride the watchdog tick). Incidents and reports push to the operator via ntfy + Windows toast (`NTFY_TOPIC` in `.env`).
- **Other headless subcommands:** `solomon state | start <name> | stop <name> | supervise [name] | serve-health [port]` (see `src-tauri/src/main.rs`).

## Keystone invariant

Solomon NEVER hand-patches a managed repo. A managed project changes only via its pi agent on a
gated `rsi/*` branch shipped as a PR, or a supervisor-authorized PR-gated fix-session. The operator
curates the backlog/contract and toggles dials; the orchestrator provisions, schedules, and
supervises — but never edits a managed repo's working tree. See `SOLOMON_RSI.md` for all 7 invariants.

## Code style — ponytail

Follow `.claude/skills/ponytail` (vendored MIT skill): YAGNI, stdlib first, native platform
features before dependencies, one line over fifty, shortest working diff, deletion over addition.
Never simplify away input validation at trust boundaries, error/data-loss handling, security,
accessibility, or tests. Mark a deliberate shortcut with a `# ponytail:` comment naming its ceiling
and the upgrade path. Commands: `/ponytail-review` (flag over-engineering in a diff),
`/ponytail-audit` (scan the repo).
