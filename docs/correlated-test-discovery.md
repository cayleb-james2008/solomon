# Correlated Test Discovery & Enhanced Anti-Gaming

## Overview

This document describes the enhancements made to Solomon's RSI loop to address the "misses correlated tests" issue and improve anti-gaming measures.

## Problem Statement

The original Solomon RSI loop had two key issues:

1. **Correlated Test Discovery**: When the agent changed files in module X, the gate would only run tests that directly test X, missing tests in other files that import or depend on X. This could lead to regressions in dependent code going undetected.

2. **Anti-Gaming Hardening**: While basic anti-gaming existed (pass count and collected count checks), it could be bypassed by:
   - Adding trivial tests to mask removal of real tests
   - Increasing skip counts significantly
   - Introducing new test failures

## Solution

### 1. Correlated Test Discovery

Added three new functions to `run_improver.py`:

#### `_find_correlated_tests(changed_files: list[str]) -> list[str]`

Given a list of changed `.py` files, finds test files that import or reference those modules.

**Strategy:**
1. Extract module names from changed files (e.g., `scripts/config.py` → `config`)
2. Search test files for imports/references to those modules using regex
3. Return the unique list of correlated test files

**Example:**
```python
# Changed file: asmodeus/execution/broker.py
# Test file: tests/test_broker.py contains "from asmodeus.execution.broker import paper"
# Result: tests/test_broker.py is included in the gate
```

#### `_expand_gate_with_correlated(gate_cmd: str, changed_files: list[str]) -> str`

Expands the gate command to include correlated tests alongside the default gate.

**Behavior:**
- Only expands the default pytest gate (no custom `GATE_CMD`)
- Appends correlated test files to ensure they're collected
- Returns custom gates unchanged (operator controls custom gates)

#### `_get_changed_files_for_correlation(base_sha: str) -> list[str]`

Gets the list of `.py` files changed between base and current HEAD.

**Used to:**
- Find correlated tests before the agent runs
- Pass to `run_gate()` for expanded test coverage

### 2. Enhanced Anti-Gaming Measures

Enhanced the `_anti_gaming_reason()` function with additional checks:

#### New Checks Added:

1. **Error Count Increase**: Detects when error count increased (new test failures introduced)
2. **Significant Skip Count Increase**: Detects when skip count increased significantly (tests being skipped instead of fixed)
3. **Collected Count Gaming Detection**: Logs warnings when collected count increases but pass count stays the same (possible gaming by adding trivial tests)

#### Existing Checks Retained:

1. **Pass Count Fell**: Tests removed/weakened/skipped
2. **Collected Count Fell**: Tests removed
3. **Skip/Xfail Markers Introduced**: Weakening that need not drop the count

## Integration

### Modified Functions:

1. **`run_gate(changed_files: list[str] | None = None)`**
   - Added optional `changed_files` parameter
   - Uses `_expand_gate_with_correlated()` to expand gate command
   - Maintains backward compatibility (optional parameter)

2. **`one_iteration()`**
   - Gets changed files after agent runs: `changed_files = _get_changed_files_for_correlation(base)`
   - Passes changed files to `run_gate(changed_files)`
   - Logs number of changed files found

## Testing

Created comprehensive test suite in `tests/test_correlated_tests.py`:

### Test Classes:

1. **`TestFindCorrelatedTests`** (7 tests)
   - Empty changed files
   - No modules in changed files
   - Private modules ignored
   - Simple module extraction
   - Package path extraction
   - Multiple correlated tests
   - No correlated tests

2. **`TestExpandGateWithCorrelated`** (4 tests)
   - Empty changed files
   - No correlated tests
   - Custom gate not expanded
   - Default pytest expanded

3. **`TestAntiGamingReason`** (9 tests)
   - No gaming
   - Pass count fell
   - Collected count fell
   - Skip markers introduced
   - Error count increased
   - Skip count increased significantly
   - Skip count increased slightly
   - No base tests
   - No current tests

4. **`TestNewSkipMarkers`** (12 tests)
   - No skip markers
   - pytest skip decorator
   - pytest skipif decorator
   - pytest xfail decorator
   - pytest skip call
   - pytest xfail call
   - unittest skip
   - unittest skipTest
   - raise SkipTest
   - Multiple skip markers
   - File header excluded
   - Non-add lines excluded

### Test Results:

```
======================= 201 passed, 1 skipped in 13.56s =======================
```

All existing tests pass, confirming no regressions.

## Usage

### Automatic Operation

The enhancements operate automatically:

1. After the agent runs, `one_iteration()` gets changed files
2. Changed files are passed to `run_gate(changed_files)`
3. `run_gate()` expands the gate to include correlated tests
4. Enhanced anti-gaming checks run on the results

### Manual Testing

To test the new functions:

```bash
cd solomon
uv run pytest tests/test_correlated_tests.py -v
```

## Benefits

1. **Better Regression Detection**: Changes to module X now trigger tests in files that import X
2. **Harder to Game**: More comprehensive anti-gaming measures prevent test weakening
3. **Backward Compatible**: Optional parameter in `run_gate()` maintains existing behavior
4. **Well Tested**: Comprehensive test suite ensures reliability
5. **Logging**: Clear visibility into what's happening

## Future Enhancements

Potential improvements:

1. **AST-based Import Analysis**: Replace regex with AST parsing for more accurate import detection
2. **Test Dependency Graph**: Build a graph of test dependencies for smarter test selection
3. **Performance Optimization**: Cache correlated tests for repeated runs
4. **Custom Gate Support**: Allow custom gates to opt-in to correlated test discovery

## Cross-Repo Support

The correlated-test-discovery mechanism now extends ACROSS repos that share a module. A repo may
declare `cross_repo_deps` in `repos.json` (a list of repo names whose gate should run when THIS repo
changes a shared module):

```json
{"name": "sover", "cross_repo_deps": ["maki"]}
```

After the primary (intra-repo) gate passes (green + anti-gaming clean), the runner runs each declared
dep repo's OWN gate (from the dep's `repos.json` row) in the dep repo's cwd. Any red dep reverts the
branch (same as a primary gate red). The per-dep results are recorded in `history.jsonl` under
`cross_repo_gates`. Self-references are excluded (the primary gate already covers this repo). If
`cross_repo_deps` is absent/empty, behavior is unchanged (backward compatible).

Key functions in `run_improver.py`:
- `_cross_repo_deps(name)`: resolves a repo's declared cross-repo deps to dep-repo dicts (with
  name/path/gate) read fresh from `repos.json`. Self-references + unknown names are skipped.
- `_run_cross_repo_gates(history_rec)`: runs each dep's gate in the dep's cwd, parses pytest/unittest
  counts, applies anti-gaming (the dep's pass/collected counts are recorded), and returns
  `{ok, results, failed_repo?}`. Any red dep -> `ok=False` + `failed_repo` so the caller reverts.

Tests: `tests/test_cross_repo_gate.py` (8 tests covering deps resolution, gate execution, the red
revert path, and the no-deps no-op).
