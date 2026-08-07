# Contributing to Solomon

Thank you for your interest in contributing to Solomon! This document describes how to set up
your environment and what the project expects from contributions.

## Getting started

1. **Fork and clone** the repository.
2. **Rust toolchain:** install Rust 1.77+ via [rustup](https://rustup.rs/).
3. **Build:**
   ```sh
   cd src-tauri && cargo build
   ```
4. **Run the test gate:**
   ```sh
   cargo test
   ```
   Every PR must pass `cargo test` — the pecrt drift gate rides inside it.
5. **Launch the dashboard:**
   ```sh
   cargo run --release
   ```

## What Solomon looks for in contributions

### Safety-first

Solomon is a **fail-closed** system. Contributions must not weaken any safety invariant:

- The **NO-MONEY-OUT** chokepoint (`src-tauri/src/money_guard.rs`) must never be loosened. Any
  unknown or ambiguous money-capable action must remain DENIED by default.
- The **honest-green ship gate** must remain honestly green — no test skipping, no empty or
  substanceless gates.
- The **keystone invariant** — Solomon never hand-patches a managed repo — must not be violated.
  Changes to managed repos land only via gated PRs from their own agent lanes.

### Code style: ponytail

Follow the ponytail style (see `AGENTS.md`):

- **YAGNI** — don't build what isn't needed yet.
- **Stdlib first** — native platform features before external dependencies.
- **One line over fifty** — if a function is getting complex, extract.
- **Shortest working diff** — keep changes surgical. Whole-repo `cargo fmt` is NOT a gate here.
- **Never simplify away** input validation, error handling, security, accessibility, or tests.
- Mark deliberate shortcuts with a `# ponytail:` comment naming the ceiling and upgrade path.

### Tests

- All new code must be accompanied by `#[cfg(test)]` module tests in the same file.
- Tests are the gate every PR must pass. If you change `pecrt.py` / `src-tauri/src/pecrt/` /
  `pecrt_golden.json`, you must update **all three** — the drift gate enforces this.

### Pull requests

- **Branch:** use a descriptive branch name (e.g., `feature/email-notifications`, `fix/lock-deadlock`).
- **Commit message:** follow [Conventional Commits](https://www.conventionalcommits.org/) format:
  `type(scope): description`
- **Scope:** keep PRs focused. One feature or fix per PR.
- **Description:** explain the *why*, not just the *what*. Reference issues if applicable.

## Reporting issues

Use the [GitHub issue tracker](https://github.com/cayleb-james2008/solomon/issues). Choose the
appropriate template (bug report or feature request) and fill in all fields.

## Code of conduct

Be respectful, constructive, and assume good intent. Safety-critical feedback is especially
welcome — if you see something that could be dangerous, say so.
