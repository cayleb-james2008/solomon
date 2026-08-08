# Security Policy

Solomon is a **fail-closed** system that orchestrates autonomous recursive
self-improvement loops and, in some lanes, touches live money. Security is a
first-class invariant, not an afterthought. This policy describes how to report
vulnerabilities and what the project guarantees.

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Please report it
privately so it can be addressed before disclosure.

- **Preferred:** email the maintainers at the address listed on the project's
  GitHub profile, or open a [private security advisory][advisory] on GitHub.
- Include as much of the following as possible:
  - The affected component and version (or commit hash).
  - A description of the vulnerability and its impact.
  - Steps to reproduce, or a minimal proof of concept.
  - Any suggested fix, if you have one.

You will receive an acknowledgment within a few business days. We ask that you
give us a reasonable window to fix and release before disclosing publicly.

## Supported versions

Security fixes are applied to the current `main` branch and the latest tagged
release. Older releases are not backported unless a fix is trivial and
low-risk.

## Safety invariants

The following are **hard invariants**. A contribution that weakens any of them
is rejected regardless of other benefits:

- **NO-MONEY-OUT chokepoint.** Solomon never moves money out. Any unknown or
  ambiguous money-capable action is **DENIED by default** by the single
  mandatory, preemptive chokepoint (`src-tauri/src/money_guard.rs`). The only
  permitted money action is a `place_trade` dispatched by a whitelisted
  live-money lane's own bot binary. This guard must never be loosened.
- **Honest-green ship gate.** A lane ships only when the project-native test
  gate passes for real. Empty or substanceless gates are forced RED. No test
  skipping, no fabricated green.
- **Keystone invariant.** Solomon never hand-patches a managed repo. Changes
  land only via a lane's own agent on a gated `rsi/*` branch shipped as a PR.
- **Fail-closed defaults.** When in doubt, the harness refuses rather than
  guesses.

## Security-relevant areas

- `src-tauri/src/money_guard.rs` — the NO-MONEY-OUT chokepoint.
- `src-tauri/src/improver/ship.rs` — the honest-green ship gate.
- `src-tauri/src/control/` — git/`gh` integration, locks, keys, runner.
- `src-tauri/src/api.rs` — the `bridge` command and headless backend surface.
- `src-tauri/src/notify.rs` — notification delivery (ntfy + toast).
- `.env` / `.solomon.json` — secrets. These are gitignored and must never be
  committed.

## Reporting process

1. Report privately (see above).
2. Maintainers triage and confirm the report.
3. A fix is developed on a private branch, tested against the `cargo test` gate
   and `cargo clippy`, and released.
4. The vulnerability is disclosed after the fix ships, with credit to the
   reporter unless they prefer anonymity.

[advisory]: https://github.com/cayleb-james2008/solomon/security/advisories/new
