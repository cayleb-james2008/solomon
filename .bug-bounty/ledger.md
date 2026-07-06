# solomon — E2E Bug-Bounty Ledger

Tauri RSI self-improvement control plane (manages OTHER git repos). Gate: `cd src-tauri && cargo test`. Hunt as backend (Rust src-tauri) + static JS review of web/app.js. Keystone: never hand-patch a managed repo — this cycle only edits Solomon's OWN code.

## Cycle 1 — 2026-07-04
Matrix: 4 batched hunt clusters (git-worktree, subprocess-control, rsi-supervisor-watchdog, web-and-ceo) × {edge, correctness, absent} → 2 refuters (real && conf≥70) → guarded fixer → auditor. Independent code-reviewer verdict: **passed**. Gate `cargo test`: **727 passed, 0 failed**; clippy clean.

**19 candidates → 14 confirmed → 14 fixed (0 audit-fail, 0 skipped) + 1 reviewer-flagged completion.**

> ⚠️ **Also un-broke `main`:** clean `main` did NOT compile (4 pre-existing errors in ceo.rs — `ensure_ops_item`/`ops_item_line` called with 4 args to a 5-arg fn ×3, plus a `verdict_probes` type mismatch), likely a bad RSI self-commit. This cycle's ceo.rs fix restores the build.

### Fixed — git/worktree (data-loss fail-safes)
- prune_stale_rsi_branches force-deleted unmerged commits when `git rev-list` exited non-zero (ignored) → failed check now treated as "has unmerged, keep". (gitops.rs) [test]
- stash_has_real_content dropped a real tracked-only stash when `git stash show` failed → failed check ⇒ retain. (gitops.rs) [test]
- prune_excess_empty_preflight_stashes inherited the same drop risk → fixed via stash_has_real_content. (gitops.rs) [test]
- untracked_non_ignored_files stored git-quoted filenames, mis-classifying non-ASCII/space files → uses `-z`. (gitops.rs) [test]
- **[reviewer completion]** stash_has_real_content's *untracked* `ls-tree ^3` path still failed OPEN on a transient timeout → retain on code 124 (timeout) while still dropping legitimate no-`^3` (code 128). (gitops.rs)

### Fixed — subprocess / liveness
- `git fetch` with no timeout on the watchdog sweep thread could wedge the 2-min tick forever → bounded timeout. (supervisor.rs)
- `gh` network commands from Tauri UI handlers had no timeout → bounded (visibility 30s, pr_action 60s, diff 30s, clone 300s). (gh.rs, registry.rs)
- custom GATE_CMD passed to `cmd /C` as one mis-quoted arg → `run_win_shell` via `raw_arg`. (proc.rs, watchdog.rs)

### Fixed — RSI / supervisor / watchdog (auto-merge safety + budget)
- noop_streak recovery with auto_push OFF permanently killed a healthy lane (stopped, never restarted) → gates only the restart, preserves lane liveness (pages instead). (supervisor.rs)
- recover() restart paths bypassed MAX_LANE_RESTARTS_PER_SWEEP → thread-local restart budget shared across the sweep. (supervisor.rs, watchdog.rs, api.rs) [test]
- backlog item marked done on an auto-merge-QUEUED (not merged) PR → advance keyed on `ship_outcome=="shipped"` (queued ≠ merged). (iteration.rs, ship.rs) [test]
- wait_for_ci_then_merge merged an un-CI'd PR when gh polling transiently returned None → ChecksProbe fails CLOSED (Unavailable ⇒ "open, CI unverifiable"; merges only on NoChecks/success). (ship.rs) [test]

### Fixed — web / CEO (dead controls + absent states)
- Approvals Merge/Close buttons stayed permanently disabled after a failed action → try/finally re-enable. (app.js)
- Auto-push toggle silently no-op'd on backend failure → toasts the error. (app.js)
- CEO panel showed "done/done" for a day the morning plan gave up after 3 failed attempts → give-up sentinel `{done, gave_up:true}` rendered "failed / gave up". (ceo.rs, app.js)

### Process notes
- Fixes applied on branch `bounty-c1-clean` from `main`; merged --no-ff. An accidental whole-repo `cargo fmt` during the cycle was fully reverted; the maintainer's compact non-rustfmt style is preserved (fmt is NOT a gate here — see memory `no-whole-repo-cargo-fmt`).
- Reviewer's 2 low-severity notes: the untracked-timeout asymmetry (fixed above); the "3 vs 4 pre-existing compile errors" count (all 4 fixed).
