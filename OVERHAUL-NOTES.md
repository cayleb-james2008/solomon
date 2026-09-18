# OVERHAUL-NOTES — solomon

Complete-overhaul run, 2026-09-18. Scope: modernize without changing what the app does — a
Rust/Tauri RSI fleet control plane plus a Python package. Verified on this Linux machine
(`cargo/rustc 1.88.0`, `node v26.7.0`, `python3 3.14.7`).

Every command below was run and its raw output captured under
`/home/cayleb/Work/projects/oss-showcase/_verify/` (scratch, gitignored).

## What changed

1. **Rust edition 2021 → 2024** (`src-tauri/Cargo.toml`). This is the substantive modernization:
   it brings the crate onto the current stable edition.
2. **Dependency refresh** — `Cargo.lock` updated via `cargo update` (see the `Cargo.lock` diff in
   commit `f398176`).
3. **`std::env::set_var` / `remove_var` migration.** Under edition 2024 those functions are
   `unsafe`. The phase-1 commits bumped the edition but did not migrate the call sites, which
   broke the test and clippy builds. Commit `199ef31` completes the migration using Rust's own
   edition-migration tool (`cargo fix --edition`), which wraps each call site in an
   `unsafe { ... }` block with a `// TODO: Audit that the environment access only happens in
   single-threaded code.` note. No behavior changed.
4. **Clippy `uninlined_format_args` fixes** across production and test code
   (e.g. `format!("{}", x)` → `format!("{x}")`).
5. **rustfmt applied** across the crate (`cargo fmt`), fixing a pre-existing formatting debt: the
   pre-run tree failed `cargo fmt --check` on 69 files under this toolchain (verified on a clean
   worktree at pre-run HEAD `2e01a63`).
6. **Removed a duplicated `#[test]` attribute** in `src-tauri/src/ops/registry.rs` that the phase-1
   run introduced (a duplicate attribute is an error under `-D warnings`).

## Verified by running (raw results)

Run from `solomon/src-tauri`:

| command | result |
|---|---|
| `cargo fmt --check` | **exit 0** |
| `cargo build` | **exit 0** |
| `cargo clippy --all-targets -- -D warnings` | **exit 0** (0 errors) |
| `cargo test` | **exit 101** — `1190 passed; 1 failed; 1 ignored` |

Logs: `_verify/lead/solomon-{fmt2,build2,clippy2,test2}.log`.

## The one honest failure (read this)

`provenance::tests::frozen_core_commits_carry_operator_provenance_tags` fails. This is **solomon's
own integrity gate working as designed**, not a broken test, and it is reported rather than
silenced.

- The test requires that any commit touching a `FROZEN_CORE` path (the anti-gaming gate, the pecrt
  drift gate, the money-out guard, and the provenance machinery itself) carries an explicit
  **`operator:`** provenance tag — the mechanical barrier against an autonomous agent editing its
  own grader ("self-dealing").
- The phase-1 modernization commits `f398176` and `d95694d` touch `FROZEN_CORE` paths (they must:
  those files needed the edition-2024 `unsafe`-env migration and rustfmt) and do **not** carry the
  `operator:` tag.
- The edits themselves were verified to be **purely mechanical** — rustfmt reflow,
  `uninlined_format_args`, and `unsafe { }` env wraps. No decision logic in the oracle changed
  (`git diff -w 2e01a63..HEAD -- <frozen paths>`).

**Why it was not "fixed":** the only ways to make it pass would be to rewrite the commits to claim
`operator:` sign-off, or to add the SHAs to `GRANDFATHERED_FROZEN`. The first would fabricate a
human approval that was not specifically given for these oracle edits; the second is explicitly
forbidden by the repo itself ("append-only for genuinely historical commits, never a bypass for new
ones"). Both would launder the exact self-dealing the gate exists to catch, so neither was done.

**Operator ratify path (one command, only if you approve the oracle edits):** after reviewing
`git -C solomon diff 2e01a63..HEAD -- src-tauri/src/pecrt src-tauri/src/money_guard.rs
src-tauri/src/improver/gates.rs src-tauri/src/improver/progress.rs src-tauri/src/provenance.rs`,
reword the two commits' subjects to start with `operator: ` and re-run `cargo test`.

## Python package

`pecrt.py` self-check passes: `python3 pecrt.py` → `pecrt.py self-check: OK (all
decision-mirror checks passed)` `EXIT=0`. The `solomon/` Python package's pytest suite (57 tests)
was green in the prior pass after declaring the undeclared `pyyaml` dependency in `pyproject.toml`.

## NOT RUN here (honest blockers)

- **Windows-only CI jobs** (`windows-latest`) — this is a Linux host.
- **GUI/dashboard** (`cargo run --release`) — needs a display; not launched.
- **Release packaging / installer** — platform-specific, not run.
- **Live probes** that need API keys or external services (ntfy, LLM endpoints) — not run.

## README truth pass

No claim was removed from the README in this pass; the phase-1 pass had already corrected the
test-count claim to a reproducible figure and added a "What works today" section. The counts in
this file are the ones measured today and name their exact commands.
