//! Port of run_improver.py's revert_gates + the gate-side pieces of the iteration state machine.
//!
//! Bug-for-bug with improver/run_improver.py. The seven ordered gates (run AFTER the Pi agent makes
//! changes) live here together with their pure predicates/parsers:
//!   1. the test/command gate            — `run_gate` / `run_gate_once`
//!   2. the anti-gaming skip-marker scan — `anti_gaming_reason` (+ new_skip_markers / added_test_defs /
//!                                          removed_test_defs / item_demands_tests)
//!   3. the cross-repo gate              — `run_cross_repo_gates` (+ cross_repo_deps)
//!   4. the eval gate                    — `run_eval_gate` (+ parse_eval_score / eval_cmd /
//!                                          eval_gate_reason)
//!   5. the leak/secret guard            — `leak_in_diff`
//!   6/7. review/visual gate predicates  — `visual_gate_enabled` / `visual_gate_reason`
//!   plus the no-op model-quality signal — `narrated_without_writing`.
//!
//! Returned dicts are `serde_json::Value` with keys byte-identical to the Python dicts, and the
//! revert-reason / log strings are quoted verbatim from the source (em-dashes and arrows included).
//! State is threaded via `&mut Ctx` (logging mutates hb/log) or `&Ctx` (read-only config lookups).

use crate::control::proc;
use crate::improver::ctx::{self, Ctx};
use crate::improver::gitops;
use serde_json::{json, Map, Value};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use regex::Regex;
use std::sync::OnceLock;

// --------------------------------------------------------------------------- #
// regex sources (run_improver module-level patterns)
// --------------------------------------------------------------------------- #
//
// Each constant-pattern regex is compiled once and cached process-global in an `OnceLock` (matching
// gitops::global_artifact_patterns), so the per-iteration gates don't recompile them.
//
// DEVIATION (lookbehind): the Rust `regex` crate does NOT support lookbehind, so the source's two
// `(?<![\w.])` lookbehinds in _SKIP_MARKER_RE are emulated in `new_skip_markers` by checking the char
// preceding each candidate match (it must not be a word char or `.`) — behaviorally identical.

/// run_improver._EVAL_FLOAT_RE.
fn eval_float_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[-+]?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?").unwrap())
}

/// The lookbehind-FREE alternatives of run_improver._SKIP_MARKER_RE (decorator/in-body/Go/Rust forms).
/// These match anywhere; the two JS/TS lookbehind alternatives are handled separately.
fn skip_marker_plain_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(concat!(
        r"@\s*\w+\.(?:skip|skipif|xfail)\b",
        r"|@\s*(?:skip|skipif|xfail)\b",
        r"|\bmark\.(?:skip|skipif|xfail)\b",
        r"|pytest\.(?:skip|xfail)\s*\(",
        r"|unittest\.skip",
        r"|\.skipTest\s*\(",
        r"|raise\s+(?:unittest\.)?SkipTest",
        r"|\bt\.Skip(?:Now|f)?\s*\(",
        r"|#\s*\[\s*ignore\b",
    ))
    .unwrap())
}

/// The two `(?<![\w.])`-guarded JS/TS alternatives of _SKIP_MARKER_RE, WITHOUT the lookbehind (which
/// `new_skip_markers` enforces by inspecting the preceding char). `it.skip(`/`test.only(`/... and
/// `xit(`/`fdescribe(`/... at a call head.
fn skip_marker_js_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?:it|test|describe|context)\.(?:skip|only|fixme)\s*\(",
            r"|(?:xit|xdescribe|xtest|fit|fdescribe)\s*\(",
        ))
        .unwrap()
    })
}

/// run_improver._TEST_DEF_RE — a test DEFINITION line (matched against diff content with the leading
/// +/- stripped). Python / JS-TS / Go / Rust forms.
fn test_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^\s*(?:async\s+)?def\s+test\w*\s*\(",
            r"|^\s*class\s+Test\w*\b",
            r#"|^\s*(?:it|test|describe)\s*(?:\.\w+)?\s*\(\s*["'`]"#,
            r"|^\s*func\s+Test\w*\s*\(",
            r"|^\s*#\s*\[\s*test\b",
        ))
        .unwrap()
    })
}

/// run_improver._DEMANDS_TESTS_RE — ADD/RESTORE/PORT verb near "test(s)", OR RAISE-style verb near
/// "coverage". Case-insensitive (`re.I`).
fn demands_tests_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"(?i)",
            r"\b(?:add|adds|adding|write|writes|writing|create|creates|creating|backfill|backfills|",
            r"restore|restores|port|ports|porting)\b[^.\n]{0,80}\btests?\b",
            r"|\b(?:add|adds|adding|increase|increases|increasing|improve|improves|improving|raise|raises|",
            r"raising|bump|bumps|cover|covers|covering|extend|extends|extending|expand|expands)\b",
            r"[^.\n]{0,80}\bcoverage\b",
        ))
        .unwrap()
    })
}

/// run_improver._SECRET_TOKEN_PATTERNS — the four secret-shaped token patterns the leak guard scans
/// for in a public repo's added diff lines (also used by ctx.redact, compiled there separately).
/// Compiled once (process-global cache).
fn secret_token_patterns() -> &'static [Regex] {
    static PATS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            Regex::new(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b").unwrap(),
            Regex::new(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b").unwrap(),
            Regex::new(r"\bsk-[A-Za-z0-9_-]{20,}\b").unwrap(),
            Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._\-]{20,}").unwrap(),
        ]
    })
}

/// run_improver._SECRET_KEYVAL_PATTERN — NAME<sep>value where NAME looks like a credential and value
/// is secret-length. The leak guard MUST scan this too (it is what ctx.redact uses); without it a
/// `DB_PASSWORD=hunter2hunter2` / `OPENAI_API_KEY=sk_underscore_no_known_prefix...` added line matches
/// no token shape, passes Gate #5, and is pushed to a PUBLIC repo while still being redacted from logs
/// (so the leak is invisible to the operator). Mirrors ctx.rs's pattern. Compiled once.
fn secret_keyval_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b([A-Za-z0-9_]*(?:API_?KEY|ACCESS_TOKEN|AUTH_TOKEN|SECRET|PASSWORD|TOKEN))\b(\s*[=:]\s*)([A-Za-z0-9_\-\.]{8,})",
        )
        .unwrap()
    })
}

/// Collection-failure markers in gate output (empty run + one of these = unrunnable, not green).
fn missing_module_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"No module named|ModuleNotFoundError|ImportError|INTERNALERROR").unwrap()
    })
}

/// `re.search(r"`[^`]+\.[A-Za-z]{1,4}`", summary)` — a backtick-quoted filename mention.
fn mentions_file_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`]+\.[A-Za-z]{1,4}`").unwrap())
}

// --------------------------------------------------------------------------- #
// test-count parsing (shared by run_gate_once + run_cross_repo_gates)
// --------------------------------------------------------------------------- #

/// `_n(pat)` from the source: the first capture group of `pat` in `out`, parsed as int, else 0.
fn n_int(out: &str, pat: &str) -> i64 {
    let re = Regex::new(pat).unwrap();
    match re.captures(out) {
        Some(c) => c
            .get(1)
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .unwrap_or(0),
        None => 0,
    }
}

/// The shared pytest/unittest count-parsing block used identically by `_run_gate_once` and
/// `_run_cross_repo_gates`. Given the combined stdout+stderr and the process returncode, build the
/// `tests` dict {passed,failed,errors,skipped,collected,green}. Mirrors the source line-for-line:
/// pytest "N passed/failed/error/skipped" + "collected N item"; on an all-zero pytest read, fall back
/// to unittest "Ran N tests" with failures=/errors=; finally fall back collected to the sum.
fn parse_counts(out: &str, returncode: i32) -> Map<String, Value> {
    let mut passed = n_int(out, r"(\d+) passed");
    let mut failed = n_int(out, r"(\d+) failed");
    let mut errors = n_int(out, r"(\d+) error");
    let mut skipped = n_int(out, r"(\d+) skipped");
    let mut collected = n_int(out, r"collected (\d+) item");
    if passed == 0 && failed == 0 && errors == 0 {
        // unittest doesn't print pytest-style "N passed"; parse its own "Ran N tests" summary.
        if let Some(ran) = Regex::new(r"Ran (\d+) tests?").unwrap().captures(out) {
            failed = n_int(out, r"failures=(\d+)");
            errors = n_int(out, r"errors=(\d+)");
            if skipped == 0 {
                skipped = n_int(out, r"skipped=(\d+)");
            }
            let ran_n: i64 = ran
                .get(1)
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .unwrap_or(0);
            passed = (ran_n - failed - errors).max(0);
            if collected == 0 {
                collected = ran_n;
            }
        }
    }
    if collected == 0 {
        // fall back to the sum so the anti-gaming collected-rail works.
        collected = passed + failed + errors + skipped;
    }
    let mut m = Map::new();
    m.insert("passed".to_string(), json!(passed));
    m.insert("failed".to_string(), json!(failed));
    m.insert("errors".to_string(), json!(errors));
    m.insert("skipped".to_string(), json!(skipped));
    m.insert("collected".to_string(), json!(collected));
    m.insert("green".to_string(), json!(returncode == 0));
    m
}

// --------------------------------------------------------------------------- #
// Gate #1 — the test/command gate
// --------------------------------------------------------------------------- #

/// run_improver._run_gate_once (~1416-1464): run the gate command ONCE and parse it. Returns
/// (green, tests-object, tail). A custom GATE_CMD is a TRUSTED operator shell command (shell=True);
/// otherwise the built-in pytest gate (`<py> -m pytest -o addopts=`). GATE_TIMEOUT bounds a hung gate
/// and force-fails it as RED with {timeout:true}; tail is the last 1500 chars of stdout+stderr.
pub fn run_gate_once(c: &mut Ctx, effective_gate_cmd: &str) -> (bool, Value, String) {
    let dur = Some(Duration::from_secs(ctx::GATE_TIMEOUT.max(0) as u64));
    let res: Result<proc::RunOut, std::io::Error> = if !effective_gate_cmd.is_empty() {
        // GATE_CMD: operator-only, compound shell syntax — run via the shell, cwd=REPO.
        let mut cmd = shell_command(effective_gate_cmd);
        cmd.current_dir(&c.repo);
        c.apply_clean_env(&mut cmd);
        run_command_timed(cmd, dur)
    } else {
        // py = str(VENV_PY) if VENV_PY.exists() else sys.executable
        let py = if c.venv_py.exists() {
            c.venv_py.to_string_lossy().into_owned()
        } else {
            // sys.executable — the running interpreter. In the native port there is no embedded
            // interpreter; "python" lets the OS resolve the gate runner, matching the intent (run the
            // repo's pytest) when no .venv is present.
            "python".to_string()
        };
        let mut cmd = Command::new(&py);
        cmd.args(["-m", "pytest", "-o", "addopts="]);
        cmd.current_dir(&c.repo);
        c.apply_clean_env(&mut cmd);
        run_command_timed(cmd, dur)
    };

    let p = match res {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            // subprocess.TimeoutExpired -> RED with the timeout signature + a bounded partial tail.
            // (proc::run drains no partial output on timeout, so `partial` is empty, matching the
            //  common case where e.stdout/e.stderr are None.)
            c.log(&format!(
                "gate TIMED OUT after {}s — reporting RED so the iteration reverts",
                ctx::GATE_TIMEOUT
            ));
            let tests = json!({
                "passed": 0, "failed": 0, "errors": 1, "skipped": 0, "collected": 0,
                "green": false, "timeout": true
            });
            let partial = format!("\n[gate timed out after {}s]", ctx::GATE_TIMEOUT);
            return (false, tests, tail_1500(&partial));
        }
        Err(e) => {
            // A spawn failure (missing interpreter/gate). Python would raise; the closest non-raising
            // behavior is a RED gate carrying the OS error on the tail so the iteration reverts.
            let tail = e.to_string();
            let tests = parse_counts(&tail, -1);
            return (false, Value::Object(tests), tail_1500(&tail));
        }
    };

    let out = format!("{}{}", p.stdout, p.stderr);
    let green = p.code == 0;
    let tests = parse_counts(&out, p.code);
    (green, Value::Object(tests), tail_1500(&out))
}

/// run_improver.run_gate (~1467-1512): authoritative test gate. A CUSTOM GATE_CMD runs exactly once
/// (its returncode is authoritative; the empty-retry framing is pytest-only). The built-in pytest gate
/// retries up to 3 times (sleeping 3s) on an EMPTY result (0 collected/passed/failed/errors and not a
/// timeout) — a transient collection glitch — UNLESS the tail carries an import/collection error
/// marker, in which case it is surfaced immediately as `gate_unrunnable` (no retry).
pub fn run_gate(c: &mut Ctx) -> (bool, Value, String) {
    let effective_gate_cmd = c.gate_cmd.clone();
    if !effective_gate_cmd.is_empty() {
        return run_gate_once(c, &effective_gate_cmd);
    }
    let mut last: (bool, Value, String) = (false, Value::Null, String::new());
    for attempt in 0..3 {
        let (green, tests, tail) = run_gate_once(c, &effective_gate_cmd);
        last = (green, tests.clone(), tail.clone());
        let empty = !value_truthy(tests.get("timeout"))
            && tests.get("collected").and_then(Value::as_i64).unwrap_or(0) == 0
            && tests.get("passed").and_then(Value::as_i64).unwrap_or(0) == 0
            && tests.get("failed").and_then(Value::as_i64).unwrap_or(0) == 0
            && tests.get("errors").and_then(Value::as_i64).unwrap_or(0) == 0;
        if empty && missing_module_re().is_match(&tail)
        {
            // a HARD, non-transient failure with the same all-zeros signature: surface immediately.
            let mut tests = tests;
            if let Value::Object(m) = &mut tests {
                m.insert("gate_unrunnable".to_string(), json!(true));
            }
            c.log(
                "gate UNRUNNABLE (import/collection error — e.g. pytest not installed): not a transient \
glitch; surfacing as base gate RED",
            );
            return (green, tests, tail);
        }
        if !empty {
            return (green, tests, tail);
        }
        if attempt < 2 {
            c.log(&format!(
                "gate discovered 0 tests (attempt {}/3) — likely a transient collection glitch on a \
repo that has tests; retrying in 3s",
                attempt + 1
            ));
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    last
}

// --------------------------------------------------------------------------- #
// Gate #2 — anti-gaming skip-marker scan (pure predicates)
// --------------------------------------------------------------------------- #

/// run_improver._new_skip_markers (~1547-1553): added (+) diff lines that introduce a skip/xfail in
/// ANY form. The '+++ ' file-header line is excluded (not added content).
pub fn new_skip_markers(diff_text: &str) -> Vec<String> {
    let plain = skip_marker_plain_re();
    let js = skip_marker_js_re();
    diff_text
        .lines()
        .filter(|ln| ln.starts_with('+') && !ln.starts_with("+++") && skip_marker_matches(ln, &plain, &js))
        .map(|s| s.to_string())
        .collect()
}

/// True if `line` matches _SKIP_MARKER_RE: any lookbehind-free alternative, OR a JS/TS alternative
/// whose match is NOT immediately preceded by a word char or `.` (the source's `(?<![\w.])`). Mirrors
/// the Python regex's overall-match semantics: a single `.search` succeeds if ANY alternative matches.
fn skip_marker_matches(line: &str, plain: &Regex, js: &Regex) -> bool {
    if plain.is_match(line) {
        return true;
    }
    // emulate the (?<![\w.]) lookbehind for the JS/TS alternatives: the char just before the match
    // start must not be `[A-Za-z0-9_.]`. Scan all candidate matches (a later one may satisfy the guard).
    for m in js.find_iter(line) {
        let start = m.start();
        if start == 0 {
            return true; // beginning of line -> nothing before it
        }
        let prev = line[..start].chars().next_back();
        match prev {
            Some(ch) if ch.is_alphanumeric() || ch == '_' || ch == '.' => continue,
            _ => return true,
        }
    }
    false
}

/// run_improver._added_test_defs (~1568-1572): added (+) diff lines that DEFINE a test (matched with
/// the leading '+' stripped, i.e. `ln[1:]`). '+++' header excluded by the '+' + non-'+++' filter.
pub fn added_test_defs(diff_text: &str) -> Vec<String> {
    let re = test_def_re();
    diff_text
        .lines()
        .filter(|ln| ln.starts_with('+') && !ln.starts_with("+++") && re.is_match(&ln[1..]))
        .map(|s| s.to_string())
        .collect()
}

/// run_improver._removed_test_defs (~1575-1580): removed (-) diff lines that DELETE a test definition
/// (matched with the leading '-' stripped). The '--- ' file-header line is excluded.
pub fn removed_test_defs(diff_text: &str) -> Vec<String> {
    let re = test_def_re();
    diff_text
        .lines()
        .filter(|ln| ln.starts_with('-') && !ln.starts_with("---") && re.is_match(&ln[1..]))
        .map(|s| s.to_string())
        .collect()
}

/// run_improver._item_demands_tests (~1596-1602): True when the backlog item explicitly asks to
/// ADD/restore tests or RAISE coverage.
pub fn item_demands_tests(goal: &str) -> bool {
    demands_tests_re().is_match(goal)
}

/// run_improver._anti_gaming_reason (~1605-1650): why a GREEN gate should still be reverted as gamed,
/// or None. Pure (no git/IO). The numeric rails (1-5) run only when both baselines are present; the
/// count-less custom-gate rail (diff deletion) runs only when the baseline has no parseable
/// passed/collected; the skip-marker rail always runs last.
pub fn anti_gaming_reason(c: &Ctx, base_tests: &Value, tests: &Value, diff_text: &str) -> Option<String> {
    let _ = c; // pure over its args; &Ctx kept for signature parity with the source's module scope.
    // `if base_tests and tests:` — both must be truthy objects (a non-empty dict).
    let bt_truthy = is_truthy_obj(base_tests);
    let t_truthy = is_truthy_obj(tests);
    if bt_truthy && t_truthy {
        let b_passed = get_i64(base_tests, "passed", 0);
        let t_passed = get_i64(tests, "passed", 0);
        if t_passed < b_passed {
            return Some(format!(
                "pass count fell {b_passed}→{t_passed} (tests removed/weakened/skipped)"
            ));
        }
        // collected must not drop either (deleting real tests + adding one trivial one holds 'passed'
        // steady while coverage shrinks). Python: `base_c, c = ...get("collected")` then
        // `if base_c and c is not None and c < base_c`.
        // DEVIATION from the Python port (deliberate bug fix, not a simplification): the rail also
        // requires `passed` to have held STEADY. `collected` falls back to passed+failed+errors+skipped
        // when there's no explicit "collected N items" line, so RESOLVING failures/errors legitimately
        // shrinks the sum even though no test was removed — and the documented gaming pattern this rail
        // targets is "...holds 'passed' steady while coverage shrinks". If `passed` INCREASED the agent
        // made more tests pass (a real improvement), so a collected-sum drop is the fixed failures/errors
        // leaving the sum, not deleted tests. Without this guard, fixing a lint/error gate (e.g. ruff's
        // "Found N errors" parsed as errors=N on the red base) is falsely reverted as gamed. Rail 1
        // above already returns on `t_passed < b_passed`, so here `t_passed <= b_passed` ⇒ exactly steady.
        let base_c = base_tests.get("collected");
        let cur_c = tests.get("collected");
        let base_c_truthy = value_truthy(base_c);
        let cur_c_not_none = !matches!(cur_c, None | Some(Value::Null));
        if base_c_truthy && cur_c_not_none && t_passed <= b_passed {
            let base_cv = base_c.and_then(Value::as_i64).unwrap_or(0);
            let cur_cv = cur_c.and_then(Value::as_i64).unwrap_or(0);
            if cur_cv < base_cv {
                return Some(format!("collected count fell {base_cv}→{cur_cv} (tests removed)"));
            }
        }
        // error count increased — new test failures introduced
        let base_errors = get_i64(base_tests, "errors", 0);
        let current_errors = get_i64(tests, "errors", 0);
        if current_errors > base_errors {
            return Some(format!(
                "error count increased {base_errors}→{current_errors} (new test failures introduced)"
            ));
        }
        // skipped count increased significantly — tests skipped instead of fixed (allow +1/+2)
        let base_skipped = get_i64(base_tests, "skipped", 0);
        let current_skipped = get_i64(tests, "skipped", 0);
        if current_skipped > base_skipped + 2 {
            return Some(format!(
                "skipped count increased significantly {base_skipped}→{current_skipped} (tests being skipped instead of fixed)"
            ));
        }
    }
    // Count-less custom gate (gate-2): when the baseline has no parseable passed/collected the numeric
    // rails are inert. Python: `if not (base_tests and any(base_tests.get(k) for k in ("passed","collected"))):`
    let base_has_counts = bt_truthy
        && (value_truthy(base_tests.get("passed")) || value_truthy(base_tests.get("collected")));
    if !base_has_counts {
        let removed = removed_test_defs(diff_text);
        if !removed.is_empty() {
            return Some(format!(
                "removed {} test definition(s) on a gate with no parseable counts (numeric anti-gaming rail inactive)",
                removed.len()
            ));
        }
    }
    let skips = new_skip_markers(diff_text);
    if !skips.is_empty() {
        return Some(format!("introduced {} skip/xfail marker(s)", skips.len()));
    }
    None
}

// --------------------------------------------------------------------------- #
// Gate #3 — cross-repo correlated test gate
// --------------------------------------------------------------------------- #

/// run_improver._cross_repo_deps (~1129-1153): resolve THIS repo's declared `cross_repo_deps` to a
/// list of dep-repo rows (with name/path/gate) read fresh from repos.json. Self-references excluded;
/// unknown dep names silently skipped. [] when none/torn/non-list.
pub fn cross_repo_deps(c: &Ctx, name: &str) -> Vec<Value> {
    let rows = match read_repos_rows(c) {
        Some(r) => r,
        None => return Vec::new(),
    };
    let this = rows
        .iter()
        .find(|r| r.is_object() && r.get("name").and_then(Value::as_str) == Some(name));
    let this = match this {
        Some(r) => r,
        None => return Vec::new(),
    };
    let dep_names = match this.get("cross_repo_deps") {
        Some(Value::Array(a)) => a.clone(),
        // `this.get("cross_repo_deps") or []` then `if not isinstance(..., list): return []`
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for dn in dep_names {
        let dn = match dn {
            Value::String(s) => s,
            _ => continue, // not isinstance(dn, str)
        };
        if dn == name {
            continue; // skip self (primary gate covers it)
        }
        if let Some(dep) = rows
            .iter()
            .find(|r| r.is_object() && r.get("name").and_then(Value::as_str) == Some(dn.as_str()))
        {
            if value_truthy(dep.get("path")) {
                out.push(dep.clone());
            }
        }
    }
    out
}

/// run_improver._run_cross_repo_gates (~1156-1223): run each declared cross-repo dep's gate in the
/// dep's cwd, AFTER the primary gate passed. Returns {ok, results:{<dep>:{green,tests,tail}}, failed_repo?}.
/// `history_rec` is mutated in place (cross_repo_gates merged in) so it lands in history.jsonl.
/// Any red dep -> ok=false + failed_repo (the caller reverts). Absent/empty deps -> ok=true, results={}.
pub fn run_cross_repo_gates(c: &mut Ctx, history_rec: &mut Value) -> Value {
    let deps = cross_repo_deps(c, &c.name.clone());
    if deps.is_empty() {
        return json!({"ok": true, "results": {}});
    }
    let mut results = Map::new();
    for dep in deps {
        let dep_name = dep.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        let dep_path = dep.get("path").and_then(Value::as_str).unwrap_or("").to_string();
        if dep_name.is_empty() || dep_path.is_empty() || !Path::new(&dep_path).is_dir() {
            continue; // best-effort skip
        }
        // dep_gate = (dep.get("gate") or "").strip()
        let dep_gate = dep
            .get("gate")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if dep_gate.is_empty() {
            // no gate configured for the dep -> can't validate; record + skip (don't block).
            results.insert(
                dep_name.clone(),
                json!({"green": true, "tests": Value::Null, "tail": "(no gate configured for dep)"}),
            );
            continue;
        }
        // run the dep's gate in the dep's cwd (shell=True, scrubbed env, GATE_TIMEOUT-bounded).
        let mut cmd = shell_command(&dep_gate);
        cmd.current_dir(&dep_path);
        c.apply_clean_env(&mut cmd);
        let dur = Some(Duration::from_secs(ctx::GATE_TIMEOUT.max(0) as u64));
        let p = match run_command_timed(cmd, dur) {
            Ok(p) => p,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                let entry = json!({
                    "green": false,
                    "tests": {"passed": 0, "failed": 0, "errors": 1, "skipped": 0,
                              "collected": 0, "green": false, "timeout": true},
                    "tail": format!("[cross-repo gate {dep_name} timed out after {}s]", ctx::GATE_TIMEOUT)
                });
                results.insert(dep_name.clone(), entry.clone());
                hist_set_cross_repo(history_rec, &dep_name, &entry);
                return json!({"ok": false, "failed_repo": dep_name, "results": Value::Object(results)});
            }
            Err(e) => {
                // a spawn failure: treat the OS error as the dep gate's RED output.
                proc::RunOut { code: -1, stdout: String::new(), stderr: e.to_string() }
            }
        };
        let out = format!("{}{}", p.stdout, p.stderr);
        let tests = parse_counts(&out, p.code);
        let green = p.code == 0;
        // SEC-5: redact secret-shaped tokens from the dep gate tail before log/history.
        let entry = json!({
            "green": green,
            "tests": Value::Object(tests),
            "tail": c.redact(&tail_n(&out, 800))
        });
        results.insert(dep_name.clone(), entry.clone());
        hist_set_cross_repo(history_rec, &dep_name, &entry);
        if !green {
            return json!({"ok": false, "failed_repo": dep_name, "results": Value::Object(results)});
        }
    }
    json!({"ok": true, "results": Value::Object(results)})
}

// --------------------------------------------------------------------------- #
// Gate #4 — per-repo EVAL_CMD eval needle
// --------------------------------------------------------------------------- #

/// run_improver._parse_eval_score (~1235-1247): parse the FIRST float from a command's stdout (the
/// eval score). None when no float is parseable. Pure.
pub fn parse_eval_score(stdout: &str) -> Option<f64> {
    if stdout.is_empty() {
        return None;
    }
    let m = eval_float_re().find(stdout)?;
    m.as_str().parse::<f64>().ok()
}

/// run_improver._eval_cmd (~1250-1254): THIS repo's `EVAL_CMD` from repos.json, stripped. "" when
/// absent. Read fresh each call.
pub fn eval_cmd(c: &Ctx, name: &str) -> String {
    gitops::repo_row(c, name)
        .get("EVAL_CMD")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// run_improver._eval_gate_reason (~1332-1341): why a gate-green change should still be reverted
/// because the eval score dropped, or None. None baseline / None after -> None. Pure.
pub fn eval_gate_reason(base_score: Option<f64>, after_score: Option<f64>) -> Option<String> {
    let (b, a) = match (base_score, after_score) {
        (Some(b), Some(a)) => (b, a),
        _ => return None,
    };
    if a < b {
        return Some(format!(
            "eval score fell {}→{} (the change made the product WORSE on the richer needle, even though tests stayed green)",
            fmt_float(b),
            fmt_float(a)
        ));
    }
    None
}

/// run_improver._run_eval_gate (~1344-1367): run THIS repo's EVAL_CMD (if any) AFTER the test gate
/// passes, parse the score, and apply the anti-gaming drop check. Returns {ok, score, reason?}.
/// No EVAL_CMD -> {ok:true, score:None}. Timeout / unparseable -> {ok:true, score:None, reason:...}.
/// A drop -> {ok:false, score, reason}.
pub fn run_eval_gate(c: &mut Ctx, base_score: Option<f64>) -> Value {
    let cmd = eval_cmd(c, &c.name.clone());
    if cmd.is_empty() {
        return json!({"ok": true, "score": Value::Null});
    }
    let mut command = shell_command(&cmd);
    command.current_dir(&c.repo);
    c.apply_clean_env(&mut command);
    let dur = Some(Duration::from_secs(ctx::GATE_TIMEOUT.max(0) as u64));
    let p = match run_command_timed(command, dur) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            c.log(&format!(
                "EVAL_CMD timed out after {}s — reporting ok (can't measure a drop)",
                ctx::GATE_TIMEOUT
            ));
            return json!({"ok": true, "score": Value::Null, "reason": "eval timed out (no drop measured)"});
        }
        Err(e) => proc::RunOut { code: -1, stdout: String::new(), stderr: e.to_string() },
    };
    let out = format!("{}{}", p.stdout, p.stderr);
    let score = parse_eval_score(&out);
    let score = match score {
        Some(s) => s,
        None => {
            c.log(&format!(
                "EVAL_CMD emitted no parseable float — the eval needle is INACTIVE for this gate (stdout: {})",
                truncate_chars(out.trim(), 160)
            ));
            return json!({"ok": true, "score": Value::Null, "reason": "eval printed no float (needle inactive)"});
        }
    };
    let reason = eval_gate_reason(base_score, Some(score));
    match reason {
        Some(r) => json!({"ok": false, "score": float_val(score), "reason": r}),
        None => json!({"ok": true, "score": float_val(score)}),
    }
}

// --------------------------------------------------------------------------- #
// Gate #5 — leak / secret guard
// --------------------------------------------------------------------------- #

/// run_improver._leak_in_diff (~1313-1329): for a PUBLIC repo, a short reason if the committed diff's
/// ADDED ('+', not '+++') lines contain an operator deny-term (case-insensitive substring) or a
/// secret-shaped token; else "". Scans only added lines so deleting a deny-term never trips it.
pub fn leak_in_diff(c: &Ctx, diff: &str) -> String {
    if diff.is_empty() {
        return String::new();
    }
    let added: String = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .collect::<Vec<_>>()
        .join("\n");
    if added.is_empty() {
        return String::new();
    }
    let low = added.to_lowercase();
    for term in gitops::repo_deny_terms(c, &c.name) {
        if !term.is_empty() && low.contains(&term.to_lowercase()) {
            return format!("deny-term '{term}' present in the diff");
        }
    }
    for pat in secret_token_patterns() {
        if pat.is_match(&added) {
            return "a secret-shaped token is present in the diff".to_string();
        }
    }
    // Also scan the NAME=value credential shape (what ctx.redact scrubs) — a `PASSWORD=...` /
    // `API_KEY=...` add-line matches no token shape but is still a secret leaked to a public repo.
    if secret_keyval_pattern().is_match(&added) {
        return "a secret-shaped token is present in the diff".to_string();
    }
    String::new()
}

// --------------------------------------------------------------------------- #
// no-op model-quality signal
// --------------------------------------------------------------------------- #

/// run_improver._narrated_without_writing (~1109-1119): heuristic — did the agent CLAIM it
/// wrote/added code while the tree is actually clean? Pure over the summary text.
pub fn narrated_without_writing(summary: &str) -> bool {
    if summary.is_empty() {
        return false;
    }
    let s = summary.to_lowercase();
    let claims_work = ["added ", "created ", "i add", "implement", "wrote ", "new file"]
        .iter()
        .any(|w| s.contains(w));
    // mentions_file = bool(re.search(r"`[^`]+\.[A-Za-z]{1,4}`", summary)) or ".py" in s
    let mentions_file = mentions_file_re().is_match(summary) || s.contains(".py");
    claims_work && mentions_file
}

// --------------------------------------------------------------------------- #
// Gates #6/#7 — visual gate predicates
// --------------------------------------------------------------------------- #

/// run_improver._visual_gate_enabled (~1377-1390): True iff THIS repo should run the visual testing
/// phase. EXPLICIT OPT-IN ONLY: on only when repos.json sets a truthy `visual_gate`. Read fresh.
pub fn visual_gate_enabled(c: &Ctx, name: &str) -> bool {
    let row = gitops::repo_row(c, name);
    // `if not row: return False` — an empty {} row is falsy.
    if !is_truthy_obj(&row) {
        return false;
    }
    value_truthy(row.get("visual_gate"))
}

/// run_improver._visual_gate_reason (~1393-1412): why a gate-green change should still be reverted
/// because the visual review found a CRITICAL issue, or None. None when the review failed to run
/// (ok=false), there are no findings, or only warnings/info. Only a SUCCESSFUL review (ok truthy) with
/// >=1 critical finding blocks. A non-dict / no-result is itself an "unavailable" reason string.
pub fn visual_gate_reason(vr: &Value) -> Option<String> {
    // `if not isinstance(vr_result, dict): return "visual review unavailable: no review result"`
    if !vr.is_object() {
        return Some("visual review unavailable: no review result".to_string());
    }
    // `if not vr_result.get("ok"):`
    if !value_truthy(vr.get("ok")) {
        // detail = str(vr_result.get("error") or "review infrastructure failed")[:240]
        let err_v = vr.get("error");
        let detail = if value_truthy(err_v) {
            value_to_py_str(err_v.unwrap())
        } else {
            "review infrastructure failed".to_string()
        };
        return Some(format!("visual review unavailable: {}", truncate_chars(&detail, 240)));
    }
    // findings = vr_result.get("findings") or []
    let findings: Vec<Value> = match vr.get("findings") {
        Some(Value::Array(a)) if !a.is_empty() => a.clone(),
        _ => Vec::new(),
    };
    let is_critical = |f: &Value| -> bool {
        f.is_object()
            && f.get("severity")
                .map(|s| str_truthy_or_empty(Some(s)).to_lowercase() == "critical")
                .unwrap_or(false)
    };
    let n_crit = findings.iter().filter(|f| is_critical(f)).count();
    if n_crit == 0 {
        return None;
    }
    let descs: Vec<String> = findings
        .iter()
        .filter(|f| is_critical(f))
        // f.get("description", "?")
        .map(|f| match f.get("description") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => value_to_py_str(other),
            None => "?".to_string(),
        })
        .collect();
    let joined = descs.join("; ");
    Some(format!(
        "visual review found {n_crit} critical issue(s) — reverting (visual_gate is on): {}",
        truncate_chars(&joined, 300)
    ))
}

// --------------------------------------------------------------------------- #
// local helpers
// --------------------------------------------------------------------------- #

/// Build a `Command` that runs `script` through the platform shell (Python `subprocess.run(shell=True)`):
/// `cmd /C <script>` on Windows, `/bin/sh -c <script>` elsewhere.
fn shell_command(script: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(script);
        c
    } else {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }
}

/// Run a fully-built `Command` with a bounded timeout, capturing stdout/stderr (UTF-8 lossy). Mirrors
/// `proc::run`'s timeout semantics (kill on expiry -> Err(TimedOut)); used for the shell/pytest gates
/// that proc::run can't model directly (proc::run takes an argv, not a pre-built Command).
fn run_command_timed(mut cmd: Command, timeout: Option<Duration>) -> std::io::Result<proc::RunOut> {
    use std::io::Read;
    use std::process::Stdio;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(proc::CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    match timeout {
        None => {
            let o = cmd.output()?;
            Ok(proc::RunOut {
                code: o.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            })
        }
        Some(d) => {
            use wait_timeout::ChildExt;
            // Drain stdout/stderr on reader threads BEFORE waiting (same fix as proc::run): a verbose
            // gate (`cargo test`, a chatty pytest/`npm test`, or any GATE_CMD) that fills the ~64KB OS
            // pipe buffer would otherwise block-on-write, never exit, and burn the full GATE_TIMEOUT —
            // a GREEN suite misreported as a timed-out RED gate, auto-reverting a correct change.
            let mut child = cmd.spawn()?;
            let out_h = child.stdout.take().map(|mut s| {
                std::thread::spawn(move || {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
            });
            let err_h = child.stderr.take().map(|mut s| {
                std::thread::spawn(move || {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
            });
            let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
                h.and_then(|h| h.join().ok()).unwrap_or_default()
            };
            match child.wait_timeout(d)? {
                Some(status) => Ok(proc::RunOut {
                    code: status.code().unwrap_or(-1),
                    stdout: join(out_h),
                    stderr: join(err_h),
                }),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // detach the readers (don't join): a surviving grandchild holding the write handle
                    // could keep the pipe open and hang the join — return promptly. See proc::run.
                    drop(out_h);
                    drop(err_h);
                    Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "subprocess timed out"))
                }
            }
        }
    }
}

/// Read CONTROL/repos.json fresh and return its rows when it parses to a JSON array, else None
/// (mirrors the `try/except (OSError, ValueError)` + `if not isinstance(rows, list)` guards used by
/// _cross_repo_deps; a torn/non-list read yields None == the source's `[]`/early-return).
fn read_repos_rows(c: &Ctx) -> Option<Vec<Value>> {
    let path = c.control.join("repos.json");
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<Value>(&bytes).ok()? {
        Value::Array(a) => Some(a),
        _ => None,
    }
}

/// `history_rec.setdefault("cross_repo_gates", {})[dep_name] = entry` on a serde Value object.
fn hist_set_cross_repo(history_rec: &mut Value, dep_name: &str, entry: &Value) {
    if !history_rec.is_object() {
        *history_rec = Value::Object(Map::new());
    }
    if let Value::Object(m) = history_rec {
        let crg = m
            .entry("cross_repo_gates".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(crg_m) = crg {
            crg_m.insert(dep_name.to_string(), entry.clone());
        }
    }
}

/// Python `out[-1500:]` by code points (the gate tail). Empty stays empty; shorter-than-1500 unchanged.
fn tail_1500(s: &str) -> String {
    tail_n(s, 1500)
}

/// Python `s[-n:]` by code points (the trailing n chars).
fn tail_n(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    chars[chars.len() - n..].iter().collect()
}

/// Python `s[:n]` by code points.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python truthiness for an Optional JSON value (None/null/false/0/0.0/""/[]/{} -> false).
fn value_truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(num)) => num.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `if d:` truthiness for a dict-shaped value (non-empty object). A non-object is falsy here (the
/// source only reaches these checks with dicts or None).
fn is_truthy_obj(v: &Value) -> bool {
    matches!(v, Value::Object(o) if !o.is_empty())
}

/// `int(d.get(k, default))`-style fetch: the i64 of `obj[key]` (numbers floor toward zero like Python
/// int() on a float result is not reached here — these counts are ints), else `default`.
fn get_i64(obj: &Value, key: &str, default: i64) -> i64 {
    obj.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// Python str(x) of a JSON scalar (for error/description fields rendered into reasons).
fn value_to_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Like ctx's helper: a string value or "".
fn str_truthy_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// Format a float the way Python's f-string `{x}` renders it inside the eval-drop reason: an integral
/// value prints without a decimal point (Python repr of `0.85`→"0.85", `2.0`→"2.0"). serde's Number
/// is not involved here (these come from `float()`), so mirror Python `str(float)`: integral floats
/// keep a trailing `.0`. We emit Rust's default float formatting, which matches for the common decimal
/// scores; `{:?}`-style is avoided. DEVIATION: exotic reprs (1e-07) may differ textually from CPython.
fn fmt_float(x: f64) -> String {
    if x == x.trunc() && x.is_finite() {
        format!("{x:.1}")
    } else {
        // Rust's Display for f64 gives the shortest round-tripping decimal, matching CPython repr for
        // typical eval scores (0.85, 0.9123).
        format!("{x}")
    }
}

/// Wrap a parsed eval score as a serde Value, preserving integral floats as floats (Python keeps it a
/// float in the returned dict). serde_json::json!(f64) stores it as a Number.
fn float_val(x: f64) -> Value {
    serde_json::Number::from_f64(x)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

// --------------------------------------------------------------------------- #
// tests — load-bearing pure logic, against the spec's exact-string vectors
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn bt(passed: i64, collected: i64) -> Value {
        json!({"passed": passed, "failed": 0, "errors": 0, "skipped": 0, "collected": collected, "green": true})
    }

    fn full(passed: i64, failed: i64, errors: i64, skipped: i64, collected: i64) -> Value {
        json!({"passed": passed, "failed": failed, "errors": errors, "skipped": skipped, "collected": collected, "green": true})
    }

    fn ctx() -> Ctx {
        Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None)
    }

    // ---- parse_counts (Gate #1 count parsing) ----
    #[test]
    fn parse_counts_pytest_form() {
        let m = parse_counts("===== 5 passed, 1 skipped in 0.2s =====", 0);
        assert_eq!(m["passed"], json!(5));
        assert_eq!(m["skipped"], json!(1));
        // collected falls back to the sum (5 + 0 + 0 + 1)
        assert_eq!(m["collected"], json!(6));
        assert_eq!(m["green"], json!(true));
    }

    #[test]
    fn parse_counts_collected_explicit() {
        let m = parse_counts("collected 10 items\n8 passed, 2 failed", 1);
        assert_eq!(m["passed"], json!(8));
        assert_eq!(m["failed"], json!(2));
        assert_eq!(m["collected"], json!(10));
        assert_eq!(m["green"], json!(false));
    }

    #[test]
    fn parse_counts_unittest_form() {
        // Ran 7 tests ... failures=2, errors=1 -> passed = 7-2-1 = 4
        let m = parse_counts("Ran 7 tests in 0.01s\nFAILED (failures=2, errors=1)", 1);
        assert_eq!(m["passed"], json!(4));
        assert_eq!(m["failed"], json!(2));
        assert_eq!(m["errors"], json!(1));
        assert_eq!(m["collected"], json!(7));
    }

    // ---- anti_gaming_reason (Gate #2), exact spec strings ----
    #[test]
    fn anti_gaming_pass_count_fell() {
        let c = ctx();
        let r = anti_gaming_reason(&c, &bt(10, 10), &full(8, 0, 0, 0, 10), "");
        assert_eq!(
            r.as_deref(),
            Some("pass count fell 10→8 (tests removed/weakened/skipped)")
        );
    }

    #[test]
    fn anti_gaming_collected_fell() {
        let c = ctx();
        // passed held steady (10) but collected dropped 12->10
        let r = anti_gaming_reason(&c, &bt(10, 12), &full(10, 0, 0, 0, 10), "");
        assert_eq!(r.as_deref(), Some("collected count fell 12→10 (tests removed)"));
    }

    #[test]
    fn anti_gaming_collected_fell_not_gamed_when_passed_rose() {
        // Regression: fixing an error/lint gate makes `passed` RISE while the collected fallback sum
        // (passed+failed+errors+skipped) FALLS as the failures/errors leave it. That is a genuine fix,
        // not coverage shrink — the rail must NOT revert it. (maki: ruff "Found 2 errors" parsed as
        // errors=2 on the red base → collected 696; after the fix 694 passed/0 errors → collected 694.)
        let c = ctx();
        let base = json!({"passed": 693, "failed": 1, "errors": 2, "skipped": 0, "collected": 696});
        let after = json!({"passed": 694, "failed": 0, "errors": 0, "skipped": 0, "collected": 694});
        assert_eq!(anti_gaming_reason(&c, &base, &after, ""), None);
    }

    #[test]
    fn anti_gaming_error_increase() {
        let c = ctx();
        let base = json!({"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10});
        let after = json!({"passed": 10, "failed": 0, "errors": 2, "skipped": 0, "collected": 12});
        let r = anti_gaming_reason(&c, &base, &after, "");
        assert_eq!(
            r.as_deref(),
            Some("error count increased 0→2 (new test failures introduced)")
        );
    }

    #[test]
    fn anti_gaming_skipped_increase_significant() {
        let c = ctx();
        let base = json!({"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10});
        let after = json!({"passed": 10, "failed": 0, "errors": 0, "skipped": 3, "collected": 13});
        let r = anti_gaming_reason(&c, &base, &after, "");
        assert_eq!(
            r.as_deref(),
            Some("skipped count increased significantly 0→3 (tests being skipped instead of fixed)")
        );
    }

    #[test]
    fn anti_gaming_skipped_increase_allowed() {
        let c = ctx();
        // +2 is allowed (the guard is `> base+2`)
        let base = json!({"passed": 10, "failed": 0, "errors": 0, "skipped": 0, "collected": 10});
        let after = json!({"passed": 10, "failed": 0, "errors": 0, "skipped": 2, "collected": 12});
        assert_eq!(anti_gaming_reason(&c, &base, &after, ""), None);
    }

    #[test]
    fn anti_gaming_skip_marker_introduced() {
        let c = ctx();
        let diff = "+@pytest.mark.skip\n+def test_foo():\n+    pass";
        let r = anti_gaming_reason(&c, &bt(10, 10), &full(10, 0, 0, 0, 10), diff);
        assert_eq!(r.as_deref(), Some("introduced 1 skip/xfail marker(s)"));
    }

    #[test]
    fn anti_gaming_countless_removed_test_def() {
        let c = ctx();
        // base has no parseable counts -> diff deletion rail fires.
        let base = json!({"passed": 0, "failed": 0, "errors": 0, "skipped": 0, "collected": 0});
        let diff = "-def test_old():\n-    assert True";
        let r = anti_gaming_reason(&c, &base, &json!({}), diff);
        assert_eq!(
            r.as_deref(),
            Some("removed 1 test definition(s) on a gate with no parseable counts (numeric anti-gaming rail inactive)")
        );
    }

    #[test]
    fn anti_gaming_clean_returns_none() {
        let c = ctx();
        assert_eq!(anti_gaming_reason(&c, &bt(10, 10), &full(11, 0, 0, 0, 11), ""), None);
    }

    // ---- skip marker regex precision ----
    #[test]
    fn skip_marker_forms() {
        assert_eq!(new_skip_markers("+@pytest.mark.skip").len(), 1);
        assert_eq!(new_skip_markers("+@skipif(x)").len(), 1);
        assert_eq!(new_skip_markers("+    pytest.skip('reason')").len(), 1);
        assert_eq!(new_skip_markers("+    raise SkipTest").len(), 1);
        assert_eq!(new_skip_markers("+it.skip('x', () => {})").len(), 1);
        assert_eq!(new_skip_markers("+    t.Skip()").len(), 1);
        assert_eq!(new_skip_markers("+#[ignore]").len(), 1);
        // negative lookbehind: model.fit( must NOT match the fit-as-focus vector
        assert_eq!(new_skip_markers("+    model.fit(X, y)").len(), 0);
        assert_eq!(new_skip_markers("+    self.test.only(x)").len(), 0);
        // '+++' header excluded
        assert_eq!(new_skip_markers("+++ b/test_x.py @pytest.mark.skip").len(), 0);
    }

    // ---- test-def detection ----
    #[test]
    fn test_def_detection() {
        assert_eq!(added_test_defs("+def test_x():").len(), 1);
        assert_eq!(added_test_defs("+async def test_y():").len(), 1);
        assert_eq!(added_test_defs("+class TestZ:").len(), 1);
        assert_eq!(added_test_defs("+func TestGo(t *testing.T) {").len(), 1);
        assert_eq!(added_test_defs("+it('does a thing', () => {").len(), 1);
        assert_eq!(removed_test_defs("-def test_old():").len(), 1);
        // a non-test def is not counted
        assert_eq!(added_test_defs("+def helper():").len(), 0);
    }

    // ---- item_demands_tests ----
    #[test]
    fn item_demands_tests_cases() {
        assert!(item_demands_tests("Add unit tests for the parser"));
        assert!(item_demands_tests("improve coverage of the gate"));
        assert!(item_demands_tests("backfill tests"));
        // 'improve the test runner' — not an add-tests verb near tests, no coverage -> false
        assert!(!item_demands_tests("improve the test runner"));
    }

    // ---- parse_eval_score ----
    #[test]
    fn parse_eval_score_first_float_wins() {
        assert_eq!(parse_eval_score("score: 0.85 | p99: 210ms"), Some(0.85));
        assert_eq!(parse_eval_score("-3.5e2 then 1"), Some(-350.0));
        assert_eq!(parse_eval_score("no number here"), None);
        assert_eq!(parse_eval_score(""), None);
    }

    // ---- eval_gate_reason ----
    #[test]
    fn eval_gate_reason_cases() {
        assert_eq!(eval_gate_reason(None, Some(0.5)), None);
        assert_eq!(eval_gate_reason(Some(0.5), None), None);
        // equal -> no block (strict <)
        assert_eq!(eval_gate_reason(Some(0.5), Some(0.5)), None);
        let r = eval_gate_reason(Some(0.9), Some(0.8));
        assert_eq!(
            r.as_deref(),
            Some("eval score fell 0.9→0.8 (the change made the product WORSE on the richer needle, even though tests stayed green)")
        );
    }

    // ---- leak_in_diff (private repo no-op handled by caller; here the deny/secret detection) ----
    #[test]
    fn leak_in_diff_secret_token() {
        let c = ctx();
        let diff = "+ token = ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345\n+ ok";
        assert_eq!(leak_in_diff(&c, diff), "a secret-shaped token is present in the diff");
    }

    #[test]
    fn leak_in_diff_only_added_lines() {
        let c = ctx();
        // a secret on a REMOVED line is not scanned
        let diff = "- token = ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345";
        assert_eq!(leak_in_diff(&c, diff), "");
        assert_eq!(leak_in_diff(&c, ""), "");
    }

    #[test]
    fn leak_in_diff_secret_keyval() {
        // Regression guard: a NAME=value credential that matches NONE of the token shapes
        // (ghp_/github_pat_/sk-/Bearer) must still be caught by the keyval pattern, or it gets
        // pushed to a public repo (the port-spec check the leak guard had dropped).
        let c = ctx();
        assert_eq!(
            leak_in_diff(&c, "+DB_PASSWORD=hunter2hunter2\n+ok"),
            "a secret-shaped token is present in the diff"
        );
        assert_eq!(
            leak_in_diff(&c, "+OPENAI_API_KEY = sk_underscore_not_a_known_prefix_123\n+fine"),
            "a secret-shaped token is present in the diff"
        );
        // the same credential on a REMOVED line is not scanned (added-only)
        assert_eq!(leak_in_diff(&c, "-DB_PASSWORD=hunter2hunter2"), "");
    }

    // ---- narrated_without_writing ----
    #[test]
    fn narrated_without_writing_cases() {
        assert!(narrated_without_writing("Added tests/test_x.py covering the parser"));
        assert!(narrated_without_writing("I implemented the fix in app.py"));
        // claims work but mentions no file -> false
        assert!(!narrated_without_writing("Refactored the loop for clarity"));
        assert!(!narrated_without_writing(""));
    }

    // ---- visual_gate_reason ----
    #[test]
    fn visual_gate_reason_cases() {
        // non-dict -> unavailable
        assert_eq!(
            visual_gate_reason(&Value::Null).as_deref(),
            Some("visual review unavailable: no review result")
        );
        // ok=False -> unavailable (does not block, but reason is set)
        let r = visual_gate_reason(&json!({"ok": false, "error": "boom"}));
        assert_eq!(r.as_deref(), Some("visual review unavailable: boom"));
        // ok=True, no critical -> None
        assert_eq!(
            visual_gate_reason(&json!({"ok": true, "findings": [{"severity": "warning", "description": "x"}]})),
            None
        );
        // ok=True, one critical -> blocking reason
        let r = visual_gate_reason(&json!({
            "ok": true,
            "findings": [{"severity": "critical", "description": "login button missing"}]
        }));
        assert_eq!(
            r.as_deref(),
            Some("visual review found 1 critical issue(s) — reverting (visual_gate is on): login button missing")
        );
    }

    #[test]
    fn visual_gate_reason_ok_no_findings() {
        assert_eq!(visual_gate_reason(&json!({"ok": true})), None);
        assert_eq!(visual_gate_reason(&json!({"ok": true, "findings": []})), None);
    }
}
