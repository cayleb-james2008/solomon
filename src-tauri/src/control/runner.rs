//! Port of control.py's runner lifecycle: start/beautify/stop/enrich_contract/ideate.
//!
//! Bug-for-bug with control.py (lines 1066-1224, 1583-1659). These functions now spawn / drive the
//! SELF binary's `run-improver` subcommand (the run_improver.py logic is ported into this same
//! executable; main.rs dispatches argv[0]=="run-improver" -> improver::run::main). The argv shape is
//! byte-identical to the old [python, run_improver.py, ...flags] except the program is the self exe
//! and the leading "script" token is the literal "run-improver". Return values are
//! `serde_json::Value` objects whose keys are byte-identical to the Python dicts.
//!
//! Verified spec + golden vectors: `src-tauri/control-port-spec.json` (module == "runner").
//!
//! Load-bearing invariants (carried over verbatim from the source comments):
//!   * start(): ordering is (a) makedirs runtime, (b) ensure_contracts (abort if !ok), (c) if a gate
//!     was just auto-set, re-read live config from load_repos and reassign repo, (d) is_running gate
//!     (return already:true WITHOUT clearing the stop sentinel), (e) ONLY THEN clear the stop
//!     sentinel, (f) spawn. Reordering (d)/(e) silently revokes a pending Stop against a live loop.
//!   * start(): --gate carries the EMPTY STRING when project_gate is unset (not omitted).
//!   * start(): makedirs is NOT error-guarded in the source — an error there propagates (we panic),
//!     unlike every other start() failure which returns {ok:false}.
//!   * stop(): write empty sentinel, poll is_running up to 5x with a 1s sleep after each check; run
//!     cleanup_worktrees ONLY when stopped confirmed within the window; always return {ok:true}.
//!   * enrich_contract()/ideate(): scan stdout lines from the END; the FIRST line starting with '{'
//!     is decisive — if it fails to parse, BREAK to the error path (do not keep scanning).
//!   * ideate(): the is_running guard precedes the API-key check.

use crate::control::{branches, contracts, heartbeat, locks, paths, proc};
use serde_json::{json, Value};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// Python-truthiness of `prov.get("gate_set")`: in practice it is either absent/None or a non-empty
/// gate string. Treat null/missing/false/""/0/[]/{} as falsy (matching `if prov.get("gate_set"):`).
fn gate_set_truthy(prov: &Value) -> bool {
    match prov.get("gate_set") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// The program to spawn: this very executable, which re-enters via its `run-improver` subcommand.
/// On std::env::current_exe() error we fall back to "solomon.exe" (resolved off PATH/cwd by the OS),
/// mirroring how the old runner_python None branch degraded rather than hard-failing the spawn build.
fn self_exe() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "solomon.exe".to_string())
}

/// Spawn a detached/hidden one-shot or loop runner (mirror of subprocess.Popen with
/// hidden_subprocess_kwargs(detached=True) + DEVNULL stdio + _clean_subenv). Returns the child pid
/// on success, or the OSError-equivalent string on spawn failure.
fn spawn_detached(argv: &[String], cwd: &str) -> Result<u32, String> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    proc::apply_clean_env(&mut cmd);
    #[cfg(windows)]
    cmd.creation_flags(proc::hidden_flags(true, false));
    match cmd.spawn() {
        Ok(child) => {
            proc::bind_to_app_job(&child); // die with the GUI app (no-op in headless subcommands)
            Ok(child.id())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Build start()'s argv (factored out for golden-vector tests). 11 value-bearing flag pairs +
/// optional --once. `py` is the self exe (program) and `runner` is the literal "run-improver"
/// subcommand token; the rest of the vector is byte-identical to the former python-run_improver argv.
fn start_argv(repo: &Value, auto_push: bool, once: bool, py: &str, runner: &str) -> Vec<String> {
    let mut args = vec![
        py.to_string(),
        runner.to_string(),
        "--repo".into(),
        paths::repo_path(repo),
        "--name".into(),
        paths::repo_name(repo),
        "--provider".into(),
        crate::control::registry::project_provider(repo),
        "--model".into(),
        crate::control::registry::project_model(repo),
        "--ship".into(),
        crate::control::registry::effective_ship(repo, auto_push),
        "--gate".into(),
        crate::control::registry::project_gate(repo).unwrap_or_default(),
        "--pr-target-branch".into(),
        crate::control::registry::project_pr_target_branch(repo),
        "--reasoning".into(),
        crate::control::registry::project_reasoning(repo),
        "--interval".into(),
        crate::control::registry::project_interval(repo).to_string(),
        "--max-iterations".into(),
        crate::control::registry::project_max_iterations(repo).to_string(),
        "--goal".into(),
        crate::control::registry::project_goal(repo),
    ];
    if once {
        args.push("--once".into());
    }
    args
}

/// Build beautify()'s argv (7 value-bearing pairs + the two valueless flags --beautify --once).
/// NOTE: no --gate/--interval/--max-iterations/--goal (differs from start()). `py`=self exe (program),
/// `runner`="run-improver" subcommand token.
fn beautify_argv(repo: &Value, auto_push: bool, py: &str, runner: &str, path: &str) -> Vec<String> {
    vec![
        py.to_string(),
        runner.to_string(),
        "--repo".into(),
        path.to_string(),
        "--name".into(),
        paths::repo_name(repo),
        "--provider".into(),
        crate::control::registry::project_provider(repo),
        "--model".into(),
        crate::control::registry::project_model(repo),
        "--ship".into(),
        crate::control::registry::effective_ship(repo, auto_push),
        "--pr-target-branch".into(),
        crate::control::registry::project_pr_target_branch(repo),
        "--reasoning".into(),
        crate::control::registry::project_reasoning(repo),
        "--beautify".into(),
        "--once".into(),
    ]
}

/// Build enrich/ideate argv (identical except the final flag: --provision vs --ideate). `py`=self exe
/// (program), `runner`="run-improver" subcommand token.
fn provision_argv(repo: &Value, py: &str, runner: &str, path: &str, name: &str, prov: &str, final_flag: &str) -> Vec<String> {
    vec![
        py.to_string(),
        runner.to_string(),
        "--repo".into(),
        path.to_string(),
        "--name".into(),
        name.to_string(),
        "--provider".into(),
        prov.to_string(),
        "--model".into(),
        crate::control::registry::project_model(repo),
        "--goal".into(),
        crate::control::registry::project_goal(repo),
        final_flag.to_string(),
    ]
}

/// Scan stdout lines from the END; the first `.trim()`ed line that starts with '{' is decisive —
/// parse it as JSON; if it fails to parse, return None (caller falls to the error path). Mirrors the
/// `for line in reversed(...): if startswith('{'): try json.loads ... except: break` quirk.
fn parse_last_brace_line(stdout: &str) -> Option<Value> {
    for line in stdout.trim().lines().rev() {
        let line = line.trim();
        if line.starts_with('{') {
            return serde_json::from_str::<Value>(line).ok();
        }
    }
    None
}

/// Build the `(stderr or stdout or <fallback>).strip()[:300]` error string. Python's `or` picks the
/// first non-empty stripped stream; truncation is by the first 300 chars AFTER strip.
fn provision_error(stderr: &str, stdout: &str, fallback: &str) -> String {
    let pick = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        fallback
    };
    let stripped = pick.trim();
    stripped.chars().take(300).collect()
}

/// control.start: spawn the improver detached if not already running. Returns {ok, pid|error}.
pub fn start(repo: &Value, auto_push: bool, once: bool) -> Value {
    // 1. runtime dir (None when the repo has no resolvable name).
    let rsi = match paths::runtime_dir(repo) {
        None => return json!({"ok": false, "error": "repo has no 'path'"}),
        Some(p) => p,
    };
    // 2. makedirs — NOT error-guarded in the source; an error here propagates (panic), not a result.
    std::fs::create_dir_all(&rsi).expect("start: failed to create runtime dir");

    // 3. ensure contracts (may auto-set the gate -> mutate repos.json).
    let prov = contracts::ensure_contracts(repo);
    if !prov.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return json!({
            "ok": false,
            "error": prov.get("error").cloned().unwrap_or(Value::Null),
        });
    }

    // 4. STALE RE-READ: if a gate was just auto-set, re-read live config so the freshly-detected gate
    //    is used on the very first launch (else project_gate(repo) is still '' and --gate carries '').
    //    The owned `live` binding is kept alive for the rest of the fn; `repo` may point at it.
    let live: Option<Value> = if gate_set_truthy(&prov) {
        let target_name = paths::repo_name(repo);
        crate::control::registry::load_repos()
            .into_iter()
            .find(|r| paths::repo_name(r) == target_name)
    } else {
        None
    };
    let repo: &Value = live.as_ref().unwrap_or(repo);

    // 5. GATE CHECK: must precede clearing the stop sentinel.
    if locks::is_running(repo) {
        let hb = heartbeat::read_heartbeat(repo).unwrap_or_else(|| json!({}));
        let pid = hb.get("pid").cloned().unwrap_or(Value::Null);
        return json!({"ok": true, "pid": pid, "already": true});
    }

    // 6. CLEAR STOP SENTINEL — only now (about to spawn). Swallow a missing-file error.
    let _ = std::fs::remove_file(rsi.join("stop"));

    // 7. the runner is now this same binary's `run-improver` subcommand (no external Python host).
    let program = self_exe();

    // 8/9. build argv (+ optional --once).
    let argv = start_argv(repo, auto_push, once, &program, "run-improver");

    // 10. spawn detached/hidden.
    match spawn_detached(&argv, &paths::repo_path(repo)) {
        Ok(pid) => json!({"ok": true, "pid": pid}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// control.beautify: spawn a one-shot docs-only "beautify" run detached. Returns {ok, pid|error}.
pub fn beautify(repo: &Value, auto_push: bool) -> Value {
    // 1. falsy repo (None / empty dict) -> unknown repo.
    if repo.is_null() || repo.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        return json!({"ok": false, "error": "unknown repo"});
    }
    // 2. path must be a real directory.
    let path = paths::repo_path(repo);
    if path.is_empty() || !Path::new(&path).is_dir() {
        return json!({"ok": false, "error": "repo has no valid 'path'"});
    }
    // 3. must be a git repo.
    if !repo.get("is_git").and_then(Value::as_bool).unwrap_or(false) {
        return json!({
            "ok": false,
            "error": format!("{} needs a git repo (publish it first)", paths::repo_name(repo))
        });
    }
    // 4. ensure contracts.
    let prov = contracts::ensure_contracts(repo);
    if !prov.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return json!({
            "ok": false,
            "error": prov.get("error").cloned().unwrap_or(Value::Null),
        });
    }
    // 5/6. venv python (NOT runner_python) must exist; message embeds the path or "None".
    let py = paths::venv_python(repo);
    let py_exists = py.as_ref().map(|p| p.exists()).unwrap_or(false);
    if !py_exists {
        let shown = match &py {
            Some(p) => p.to_string_lossy().into_owned(),
            None => "None".to_string(),
        };
        return json!({"ok": false, "error": format!("venv python not found: {}", shown)});
    }
    // venv_python existence is still a hard precondition for the TARGET repo (above); the SPAWN
    // program, however, is now this same binary's `run-improver` subcommand (not the venv python).
    let _ = py; // venv python was a precondition check only — it is no longer the spawn program.
    let program = self_exe();
    // 8. build argv + spawn.
    let argv = beautify_argv(repo, auto_push, &program, "run-improver", &path);
    match spawn_detached(&argv, &path) {
        Ok(pid) => json!({"ok": true, "pid": pid}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// control.stop: write the stop sentinel, wait briefly for the loop to confirm stopped, then run a
/// best-effort cleanup_worktrees. Always {ok:true} after a successful sentinel write.
pub fn stop(repo: &Value) -> Value {
    // 1. runtime dir.
    let rsi = match paths::runtime_dir(repo) {
        None => return json!({"ok": false, "error": "repo has no 'path'"}),
        Some(p) => p,
    };
    // 2. write empty sentinel; OSError (makedirs or write) -> error string.
    if let Err(e) = std::fs::create_dir_all(&rsi).and_then(|_| std::fs::write(rsi.join("stop"), "")) {
        return json!({"ok": false, "error": e.to_string()});
    }
    // 3. GRACE LOOP: check-then-sleep(1), up to 5 iterations. is_running never raises in this port
    //    (returns bool), so the Python `except Exception: stopped=False` guard is a no-op here.
    let mut stopped = false;
    for _ in 0..5 {
        if !locks::is_running(repo) {
            stopped = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    // 4. CLEANUP only when confirmed stopped (best-effort; result discarded).
    if stopped {
        let _ = branches::cleanup_worktrees(repo);
    }
    json!({"ok": true})
}

/// True iff an API key is available for this repo's provider — either the per-repo `api_key`
/// (which overrides the global .env key for this repo's iterations via Ctx::apply_api_key) or the
/// global .env key. Mirrors supervisor::keys_provider_ready so enrich_contract/ideate don't silently
/// block a repo keyed only per-repo (the same class of bug that stalled gate_red_streak fix-sessions
/// for per-repo-keyed repos before that fix).
fn provider_key_ready(repo: &Value) -> bool {
    !crate::control::registry::project_api_key(repo).is_empty()
        || crate::control::keys::keys_status()
            .get(crate::control::registry::project_provider(repo))
            .and_then(Value::as_bool)
            .unwrap_or(false)
}

/// control.enrich_contract: run a one-shot provisioner (run_improver.py --provision). background=true
/// spawns detached -> {ok, started}; otherwise blocks and passes through the runner's JSON line.
pub fn enrich_contract(repo: &Value, background: bool) -> Value {
    // 1. name + path.
    let name = paths::repo_name(repo);
    let path = paths::repo_path(repo);
    if name.is_empty() || path.is_empty() {
        return json!({"ok": false, "error": "repo has no name/path"});
    }
    // 2. provider key — per-repo api_key counts (see provider_key_ready).
    let prov = crate::control::registry::project_provider(repo);
    if !provider_key_ready(repo) {
        return json!({"ok": false, "error": format!("{} API key not set (add it in Settings)", prov)});
    }
    // 3/4. the runner is now this same binary's `run-improver` subcommand (no external Python host).
    let program = self_exe();
    // 5. argv.
    let argv = provision_argv(repo, &program, "run-improver", &path, &name, &prov, "--provision");
    // 6. background spawn.
    if background {
        return match spawn_detached(&argv, &path) {
            Ok(_) => json!({"ok": true, "started": true}),
            Err(e) => json!({"ok": false, "error": e}),
        };
    }
    // 7. blocking capture.
    // 660s ceiling — 10% over the `run-improver --provision` child's internal 600s wall
    // (`TIMEOUT_PHASE_600` in pi.rs), so the external kill only fires when the child has actually
    // wedged past its own self-imposed limit (orphaned pipe, stuck LLM HTTP read, dead grandchild),
    // never on a healthy run. Mirrors the 4bbfd99 watchdog tick discipline: a hung subprocess must
    // NEVER wedge the caller indefinitely. `Err(TimedOut)` maps to the existing `error` branch.
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let r = match proc::run(&argv_refs, Some(Path::new(&path)), Some(Duration::from_secs(660))) {
        Ok(o) => o,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    if let Some(v) = parse_last_brace_line(&r.stdout) {
        return v;
    }
    json!({"ok": false, "error": provision_error(&r.stderr, &r.stdout, "provision failed")})
}

/// control.ideate: run a one-shot divergent ideation pass (run_improver.py --ideate). Always blocking;
/// passes through the runner's JSON line. The is_running guard precedes the API-key check.
pub fn ideate(repo: &Value) -> Value {
    // 1. name + path.
    let name = paths::repo_name(repo);
    let path = paths::repo_path(repo);
    if name.is_empty() || path.is_empty() {
        return json!({"ok": false, "error": "repo has no name/path"});
    }
    // 2. IS_RUNNING GUARD — before the key check (single-flight: --ideate does a lock-free RMW of
    //    backlog.md, so it must refuse while a live loop holds the repo).
    if locks::is_running(repo) {
        return json!({"ok": false, "error": "loop is running — stop it first to ideate manually (ideation also runs in-loop)"});
    }
    // 3. provider key — per-repo api_key counts (see provider_key_ready).
    let prov = crate::control::registry::project_provider(repo);
    if !provider_key_ready(repo) {
        return json!({"ok": false, "error": format!("{} API key not set (add it in Settings)", prov)});
    }
    // 4/5. the runner is now this same binary's `run-improver` subcommand (no external Python host).
    let program = self_exe();
    // 6. argv (differs from enrich only by the final flag).
    let argv = provision_argv(repo, &program, "run-improver", &path, &name, &prov, "--ideate");
    // 7. blocking capture.
    // 660s ceiling — 10% over the `run-improver --ideate` child's internal 600s wall
    // (`TIMEOUT_PHASE_600` in pi.rs). `ideate` runs on the watchdog tick path
    // (`supervisor::recover` → `runner::ideate`), so an unbounded spawn here is the exact wedge
    // class 4bbfd99 bounded in `watchdog::base_is_clean`/`branches::git_c` — a stuck LLM HTTP read
    // or orphaned cargo grandchild would hang the whole sweep. `Err(TimedOut)` maps to the existing
    // `error` branch so the lane escalates instead of hanging the tick.
    let argv_refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    let r = match proc::run(&argv_refs, Some(Path::new(&path)), Some(Duration::from_secs(660))) {
        Ok(o) => o,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    // 8. parse identically (fallback string differs: "ideate failed").
    if let Some(v) = parse_last_brace_line(&r.stdout) {
        return v;
    }
    json!({"ok": false, "error": provision_error(&r.stderr, &r.stdout, "ideate failed")})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // PROG stands in for the self exe (program); RUNNER is now the literal "run-improver" subcommand
    // token. Production passes self_exe() for PROG and "run-improver" for RUNNER (see start()/etc).
    const PROG: &str = "PROG.exe";
    const RUNNER: &str = "run-improver";

    // ---- start_argv golden vectors ----

    #[test]
    fn start_argv_happy_path_gate_empty() {
        // GV1: gate unset -> --gate carries the empty string; once=False -> no --once.
        let repo = json!({
            "name": "sover", "path": "C:/p/sover", "is_git": true,
            "provider": "ollama-cloud", "model": "glm-5.2", "ship": "pr",
            "interval": 120, "max_iterations": 0, "reasoning": "xhigh", "goal": ""
        });
        let argv = start_argv(&repo, true, false, PROG, RUNNER);
        // args[1] is the run-improver subcommand token (formerly the run_improver.py path).
        assert_eq!(argv[1], "run-improver");
        assert_eq!(
            argv,
            vec![
                "PROG.exe", "run-improver", "--repo", "C:/p/sover", "--name", "sover",
                "--provider", "ollama-cloud", "--model", "glm-5.2", "--ship", "pr",
                "--gate", "", "--pr-target-branch", "main", "--reasoning", "xhigh",
                "--interval", "120", "--max-iterations", "0", "--goal", ""
            ]
        );
    }

    #[test]
    fn self_exe_ends_with_exe_name() {
        // args[0] is the self exe path; assert by suffix (not a hardcoded python path). On the test
        // host current_exe() is the test runner binary, so just assert it is non-empty and a path.
        let prog = self_exe();
        assert!(!prog.is_empty());
        // Production program token is whatever current_exe() resolves to (or the solomon.exe fallback).
        assert!(prog.ends_with(".exe") || prog.contains("solomon") || prog.contains(std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn start_argv_gate_set_and_once() {
        // GV3: detected gate flows through; --once appended.
        let gate = ".venv\\Scripts\\python -m unittest discover -s tests -t tests";
        let repo = json!({"name": "foo", "path": "C:/p/foo", "gate": gate});
        let argv = start_argv(&repo, true, true, PROG, RUNNER);
        // gate present, not empty
        let gpos = argv.iter().position(|a| a == "--gate").unwrap();
        assert_eq!(argv[gpos + 1], gate);
        assert_eq!(argv.last().unwrap(), "--once");
    }

    #[test]
    fn start_argv_auto_push_false_forces_local() {
        // GV4: ship -> local regardless of repo ship.
        let repo = json!({"name": "x", "path": "C:/p/x", "ship": "pr"});
        let argv = start_argv(&repo, false, false, PROG, RUNNER);
        let spos = argv.iter().position(|a| a == "--ship").unwrap();
        assert_eq!(argv[spos + 1], "local");
    }

    #[test]
    fn start_argv_has_eleven_value_pairs_no_once() {
        // verdict issue: 11 value-bearing flag pairs (22 tokens after py,runner), no --once.
        let repo = json!({"name": "x", "path": "C:/p/x"});
        let argv = start_argv(&repo, true, false, PROG, RUNNER);
        assert_eq!(argv.len(), 2 + 22);
        assert!(!argv.iter().any(|a| a == "--once"));
    }

    // ---- start() early-exit golden vectors ----

    #[test]
    fn start_empty_repo_no_path() {
        // GV6: empty repo -> "repo has no 'path'" before any spawn/contracts.
        let out = start(&json!({}), true, false);
        assert_eq!(out, json!({"ok": false, "error": "repo has no 'path'"}));
    }

    // ---- beautify_argv golden vectors ----

    #[test]
    fn beautify_argv_happy_path() {
        // GV: exact short argv (no --gate/--interval/--max-iterations/--goal; +--beautify --once).
        let repo = json!({
            "name": "sover", "path": "C:/p/sover", "is_git": true,
            "provider": "ollama-cloud", "model": "glm-5.2", "ship": "pr", "reasoning": "xhigh"
        });
        let argv = beautify_argv(&repo, true, PROG, RUNNER, "C:/p/sover");
        assert_eq!(argv[1], "run-improver");
        assert_eq!(
            argv,
            vec![
                "PROG.exe", "run-improver", "--repo", "C:/p/sover", "--name", "sover",
                "--provider", "ollama-cloud", "--model", "glm-5.2", "--ship", "pr",
                "--pr-target-branch", "main", "--reasoning", "xhigh", "--beautify", "--once"
            ]
        );
        assert!(!argv.iter().any(|a| a == "--gate"));
        assert!(!argv.iter().any(|a| a == "--goal"));
    }

    #[test]
    fn beautify_argv_auto_push_false_local() {
        let repo = json!({"name": "x", "path": "C:/p/x", "is_git": true, "ship": "auto-merge"});
        let argv = beautify_argv(&repo, false, PROG, RUNNER, "C:/p/x");
        let spos = argv.iter().position(|a| a == "--ship").unwrap();
        assert_eq!(argv[spos + 1], "local");
    }

    // ---- beautify() early-exit golden vectors ----

    #[test]
    fn beautify_empty_repo_unknown() {
        // GV: {} or null -> "unknown repo".
        assert_eq!(beautify(&json!({}), true), json!({"ok": false, "error": "unknown repo"}));
        assert_eq!(beautify(&Value::Null, true), json!({"ok": false, "error": "unknown repo"}));
    }

    #[test]
    fn beautify_non_git_refusal_name_interpolated() {
        // GV: path is a real dir (use temp dir), is_git false -> needs-git error.
        let dir = std::env::temp_dir().join("solomon_runner_beautify_git_test");
        let _ = std::fs::create_dir_all(&dir);
        let repo = json!({"name": "localfolder", "path": dir.to_string_lossy(), "is_git": false});
        let out = beautify(&repo, true);
        assert_eq!(
            out,
            json!({"ok": false, "error": "localfolder needs a git repo (publish it first)"})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn beautify_bad_path_not_dir() {
        // path missing / not a directory -> "repo has no valid 'path'".
        let repo = json!({"name": "x", "path": "C:/definitely/not/a/real/dir/xyz123"});
        assert_eq!(
            beautify(&repo, true),
            json!({"ok": false, "error": "repo has no valid 'path'"})
        );
    }

    // ---- provision_argv golden vectors (enrich/ideate) ----

    #[test]
    fn enrich_argv_shape() {
        // GV: enrich passthrough argv ends with --provision.
        let repo = json!({"name": "sover", "path": "C:/p/sover", "provider": "ollama-cloud", "model": "glm-5.2", "goal": ""});
        let argv = provision_argv(&repo, PROG, RUNNER, "C:/p/sover", "sover", "ollama-cloud", "--provision");
        assert_eq!(argv[1], "run-improver");
        assert_eq!(
            argv,
            vec![
                "PROG.exe", "run-improver", "--repo", "C:/p/sover", "--name", "sover",
                "--provider", "ollama-cloud", "--model", "glm-5.2", "--goal", "", "--provision"
            ]
        );
    }

    #[test]
    fn ideate_argv_differs_only_by_final_flag() {
        let repo = json!({"name": "sover", "path": "C:/p/sover", "provider": "ollama-cloud", "model": "glm-5.2", "goal": ""});
        let argv = provision_argv(&repo, PROG, RUNNER, "C:/p/sover", "sover", "ollama-cloud", "--ideate");
        assert_eq!(argv.last().unwrap(), "--ideate");
        assert!(!argv.iter().any(|a| a == "--provision"));
    }

    // ---- parse_last_brace_line quirk ----

    #[test]
    fn parse_passthrough_from_last_brace_line() {
        // GV: blocking success — last brace line parsed and returned verbatim.
        let stdout = "some log\n{\"ok\": true, \"agent_written\": true, \"backlog_written\": true, \"summary\": \"tailored\"}\n";
        let v = parse_last_brace_line(stdout).unwrap();
        assert_eq!(
            v,
            json!({"ok": true, "agent_written": true, "backlog_written": true, "summary": "tailored"})
        );
    }

    #[test]
    fn parse_malformed_last_brace_line_breaks() {
        // GV: a trailing malformed brace-line masks an earlier valid one -> None (error path).
        let stdout = "{\"ok\": true}\n{not valid json";
        assert!(parse_last_brace_line(stdout).is_none());
    }

    #[test]
    fn parse_no_brace_line() {
        assert!(parse_last_brace_line("just logs\nno json here").is_none());
        assert!(parse_last_brace_line("").is_none());
    }

    // ---- provision_error truncation + stream precedence ----

    #[test]
    fn provision_error_prefers_stderr() {
        // GV: no JSON, stderr present -> stderr stripped.
        let e = provision_error("Traceback ... provider auth failed", "", "provision failed");
        assert_eq!(e, "Traceback ... provider auth failed");
    }

    #[test]
    fn provision_error_falls_back_to_stdout() {
        // GV: stderr empty -> stdout used (the malformed-line case).
        let e = provision_error("", "{\"ok\": true}\n{not valid json", "provision failed");
        assert_eq!(e, "{\"ok\": true}\n{not valid json");
    }

    #[test]
    fn provision_error_default_fallback() {
        // GV: both empty -> fallback string.
        assert_eq!(provision_error("", "", "provision failed"), "provision failed");
        assert_eq!(provision_error("", "", "ideate failed"), "ideate failed");
    }

    #[test]
    fn provision_error_truncates_to_300_chars() {
        let long = "x".repeat(500);
        let e = provision_error(&long, "", "provision failed");
        assert_eq!(e.chars().count(), 300);
    }

    #[test]
    fn provision_error_strips_before_truncate() {
        let e = provision_error("   hello world   ", "", "provision failed");
        assert_eq!(e, "hello world");
    }

    // ---- provider_key_ready: per-repo api_key counts ----
    // enrich_contract/ideate pre-flight on the provider key. A repo keyed only per-repo (no global
    // .env key for its provider) must NOT be silently blocked — the per-repo api_key overrides the
    // global key in the spawned run-improver process, so it is a valid key. The short-circuit (||)
    // means a non-empty per-repo key never touches the real .env file, so this test is deterministic
    // and machine-state-independent. Mirrors supervisor::keys_provider_ready's fix for the same class
    // of silent-config-drift bug.
    #[test]
    fn provider_key_ready_per_repo_key_counts() {
        // per-repo key set -> ready regardless of global .env state
        let repo = json!({ "name": "pkr_1", "provider": "openrouter", "api_key": "sk-or-v1-xyz" });
        assert!(provider_key_ready(&repo), "per-repo api_key must count as ready");
        let repo2 = json!({ "name": "pkr_2", "provider": "ollama-cloud", "api_key": "oc-key-abc" });
        assert!(provider_key_ready(&repo2), "per-repo api_key counts for any provider");
        // no per-repo key, no global key -> not ready (the global check is env-dependent, but an
        // unknown provider with no per-repo key is deterministically not ready)
        let repo3 = json!({ "name": "pkr_3", "provider": "definitely-not-a-real-provider" });
        assert!(!provider_key_ready(&repo3), "no per-repo key + unknown provider -> not ready");
        // empty per-repo key falls through to the global check
        let repo4 = json!({ "name": "pkr_4", "provider": "definitely-not-a-real-provider", "api_key": "" });
        assert!(!provider_key_ready(&repo4), "empty per-repo key -> falls through to global (unknown provider -> not ready)");
    }
}
