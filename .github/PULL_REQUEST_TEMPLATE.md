## Summary

A concise description of what this PR changes and why.

## Safety checklist

- [ ] The **NO-MONEY-OUT** chokepoint (`money_guard.rs`) is not weakened or loosened
- [ ] The **honest-green ship gate** is not bypassed or made substanceless
- [ ] The **keystone invariant** (Solomon never hand-patches a managed repo) is not violated
- [ ] No new money-capable action is introduced without an explicit whitelist entry
- [ ] All fail-closed defaults remain fail-closed

## Tests

- [ ] `cargo test` passes in `src-tauri/`
- [ ] New code is accompanied by `#[cfg(test)]` tests
- [ ] If `pecrt.py` / `pecrt.rs` / `pecrt_golden.json` were changed, all three were updated together

## Description of changes

-

## Breaking changes

- [ ] This PR introduces breaking changes
- [ ] If yes, they are documented below:

## Issues fixed

Fixes #
