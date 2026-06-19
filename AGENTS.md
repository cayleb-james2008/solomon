# Solomon — agent guide

Solomon is the **RSI orchestrator/supervisor**: it provisions, schedules, gates, ships, and
recovers improvements for the repos it manages (see `repos.json`), driving each via a per-repo
pi agent under a written contract (`improver/<name>/AGENT.md` + `backlog.md`). It does not write
managed-project code itself, and it is not improved by its own loop. The canonical loop spec and
the 7 invariants live in **`SOLOMON_RSI.md`** — read it before touching the harness.

## Distribution

**Operator preference: ship a single self-contained Windows exe** (`dist\Solomon\Solomon.exe`,
PyInstaller onedir via `build.ps1`, built with the maki venv python). The operator runs the exe
directly — assume no dev shell or separate runtime on the target machine. An updater companion exe
(`SolomonUpdater.exe`, the update+open entry point) is the one sanctioned companion; do not add
other executables or an external-runtime dependency without operator sign-off.

## Run / test / build

`<maki-venv-python>` = `C:\Users\Cayleb\Desktop\workspace\projects\maki\.venv\Scripts\python.exe`
(Solomon has no own venv — it borrows the maki interpreter, which has pywebview + pyinstaller).

- **Gate / tests:** `<maki-venv-python> -m pytest tests/` (collection scoped to `tests/` by `pytest.ini`).
- **Dashboard:** `<maki-venv-python> app.py` (pywebview / WebView2).
- **Build exe:** `./build.ps1` → `dist/Solomon/Solomon.exe`.
- **One RSI iteration (dry run):** `<maki-venv-python> improver/run_improver.py --repo <path> --name <name> --once`.
- **Watchdog sweep:** `<maki-venv-python> monitor.py`.

## Keystone invariant

Solomon NEVER hand-patches a managed repo. A managed project changes only via its pi agent on a
gated `rsi/*` branch shipped as a PR, or a supervisor-authorized PR-gated fix-session. The operator
curates the backlog/contract and toggles dials; the orchestrator provisions, schedules, and
supervises — but never edits a managed repo's working tree. See `SOLOMON_RSI.md` for all 7 invariants.
