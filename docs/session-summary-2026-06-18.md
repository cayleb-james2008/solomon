# Session Summary - June 18, 2026

## Goal
Enhance Solomon's RSI loop with correlated test detection and dirty-repo prevention to address the "misses correlated tests" issue.

## Accomplishments

### 1. Correlated Test Discovery ✅

**Problem:** When the agent changed files in module X, the gate would only run tests that directly test X, missing tests in other files that import or depend on X.

**Solution:** Implemented automatic discovery and execution of correlated tests:

#### New Functions Added:

1. **`_find_correlated_tests(changed_files: list[str]) -> list[str]`**
   - Extracts module names from changed files
   - Searches test files for imports/references to those modules
   - Returns list of correlated test files

2. **`_expand_gate_with_correlated(gate_cmd: str, changed_files: list[str]) -> str`**
   - Expands gate command to include correlated tests
   - Only expands default pytest gate (custom gates unchanged)
   - Maintains backward compatibility

3. **`_get_changed_files_for_correlation(base_sha: str) -> list[str]`**
   - Gets list of `.py` files changed between base and current HEAD
   - Used to find correlated tests before the agent runs

#### Integration:

Modified `run_gate()` to accept optional `changed_files` parameter:
```python
def run_gate(changed_files: list[str] | None = None) -> tuple:
    # Expand gate command to include correlated tests if changed_files provided
    effective_gate_cmd = GATE_CMD
    if changed_files:
        effective_gate_cmd = _expand_gate_with_correlated(GATE_CMD, changed_files)
    # ... rest of function
```

Modified `one_iteration()` to pass changed files to `run_gate()`:
```python
# Get changed files for correlated test discovery
changed_files = _get_changed_files_for_correlation(base)
if changed_files:
    log(f"found {len(changed_files)} changed files for correlated test discovery")
green, tests, tail = run_gate(changed_files)
```

### 2. Enhanced Anti-Gaming Measures ✅

**Problem:** Basic anti-gaming could be bypassed by adding trivial tests, increasing skip counts, or introducing new failures.

**Solution:** Enhanced `_anti_gaming_reason()` with additional checks:

1. **Error count increase**: Detects when error count increased (new test failures introduced)
2. **Significant skip count increase**: Detects when skip count increased significantly (tests being skipped instead of fixed)
3. **Collected count gaming detection**: Logs warnings when collected count increases but pass count stays the same (possible gaming by adding trivial tests)

### 3. Clean-Tree Preflight Enforcement ✅

**Status:** Already implemented and working correctly.

The existing implementation includes:
- `tree_dirty()` checks for uncommitted changes to TRACKED files
- `_dirty_blocks_iteration()` returns True only if dirty tree is on BASE branch
- Untracked non-ignored files are protected (refuses to clean them)
- Proper logging and heartbeat updates

### 4. Comprehensive Test Suite ✅

Created `tests/test_correlated_tests.py` with 33 tests covering:

- **TestFindCorrelatedTests** (7 tests): Module extraction, correlation detection
- **TestExpandGateWithCorrelated** (4 tests): Gate expansion logic
- **TestAntiGamingReason** (9 tests): Enhanced anti-gaming measures
- **TestNewSkipMarkers** (12 tests): Skip/xfail marker detection
- **TestGetChangedFilesForCorrelation** (1 test): Changed file detection

### 5. Documentation ✅

Created comprehensive documentation:
- `docs/correlated-test-discovery.md`: Detailed technical documentation
- Updated `SOLOMON_RSI.md`: Added section on correlated test discovery and enhanced anti-gaming

## Test Results

```
======================= 201 passed, 1 skipped in 13.56s =======================
```

All tests pass, including:
- 28 existing Solomon tests (no regressions)
- 33 new correlated test discovery tests
- 140 existing tests across other test files

## Key Benefits

1. **Better Regression Detection**: Changes to module X now trigger tests in files that import X
2. **Harder to Game**: More comprehensive anti-gaming measures prevent test weakening
3. **Backward Compatible**: Optional parameter in `run_gate()` maintains existing behavior
4. **Well Tested**: Comprehensive test suite ensures reliability
5. **Clear Logging**: Visibility into what's happening

## Files Modified

- `solomon/improver/run_improver.py`: Added correlated test discovery and enhanced anti-gaming
- `solomon/tests/test_correlated_tests.py`: New comprehensive test suite
- `solomon/docs/correlated-test-discovery.md`: Technical documentation
- `solomon/SOLOMON_RSI.md`: Updated with new features

## Next Steps

The Solomon RSI loop enhancements are complete and tested. The next steps in the triple-project overhaul are:

1. **Asmodeus**: Verify paper broker pipeline end-to-end, check worker liveness, verify TradeLocker connectivity
2. **Sover Dashboard**: Gut old HTML/CSS/JS, build new operator chat box UI, inject automation panels
3. **OpenCode Tasks**: Address the two pending OpenCode session tasks

## Verification

The implementation has been verified by:
1. Running all existing Solomon tests (28/28 passed)
2. Running new comprehensive test suite (33/33 passed)
3. Running full test suite (201/201 passed, 1 skipped)
4. No regressions detected in existing functionality
