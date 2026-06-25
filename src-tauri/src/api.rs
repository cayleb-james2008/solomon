//! Native Rust port of app.py's `Api` class (the pywebview js_api) + `_load_state`/`_save_state` +
//! `_health_payload`. This is the backend the JS bridge and the headless CLI call.
//!
//! Bug-for-bug with app.py. Returned dicts are `serde_json::Value` with keys byte-identical to the
//! Python dicts. JS calls these methods POSITIONALLY, so `dispatch` reads `args[0..]` in the same
//! order app.py's methods declare their parameters.
//!
//! All git/gh/heartbeat/branch/registry/contract/runner/supervisor work delegates to the already-
//! ported, green `control::*` and `supervisor::*` modules.
//!
//! DEVIATIONS:
//!  * The in-app updater's update_status / apply_update are handled in main.rs's `bridge` command via
//!    tauri-plugin-updater (they need the AppHandle, which dispatch does not have). dispatch still serves
//!    cached_update_status as a benign stub {ok:false, available:false} — there is no native cache to read.
//!    current_sha IS ported (it reads the checkout's git sha and is independent of the updater).
//!  * AppState loads fresh from disk per `dispatch` call rather than holding a long-lived `self._state`
//!    object. The disk file (.solomon.json) is the single source of truth, so a load-per-dispatch is
//!    observably identical to app.py's `self._state` (the setters' rollback-on-failed-persist is what
//!    app.py's rollback protects, and that is preserved below by re-loading, mutating, and persisting
//!    atomically — a failed persist leaves disk, and therefore the next read, unchanged).
#![allow(dead_code)]

use crate::control::{
    apptest_health, branches, contracts, gh, heartbeat, keys, paths, registry, runner,
};
use crate::supervisor;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

// --------------------------------------------------------------------------- #
// AppState — the .solomon.json store
// --------------------------------------------------------------------------- #

/// app._STATE_FILE — control.HERE/.solomon.json.
fn state_file() -> PathBuf {
    paths::here().join(".solomon.json")
}

/// app._LEGACY_STATE_FILE — the older builds' .rsi-control.json (read once, on migration only).
fn legacy_state_file() -> PathBuf {
    paths::here().join(".rsi-control.json")
}

/// The persisted operator state: theme + the global auto_push / auto_ai_fix dials + the saved layout.
/// Always an object Map (app.py's _state is a dict).
pub struct AppState {
    state: Map<String, Value>,
}

impl AppState {
    /// app._load_state: read .solomon.json; on missing/corrupt fall back ONCE to the legacy
    /// .rsi-control.json; on both failing, `{}`. The defaults (theme/auto_push/auto_ai_fix/layout) are
    /// NOT baked into the stored dict — they're applied by the getters, exactly like app.py.
    pub fn load() -> Self {
        let state = read_json_object(&state_file())
            .or_else(|| read_json_object(&legacy_state_file()))
            .unwrap_or_default();
        AppState { state }
    }

    /// app._save_state: atomic tmp + rename. Returns true on success, false on OSError (so a setter
    /// can tell the UI whether the dial reached disk — its rollback fires only on false). On failure
    /// the half-written tmp is removed.
    pub fn save(&self) -> bool {
        let target = state_file();
        let tmp = {
            let mut t = target.clone().into_os_string();
            t.push(".tmp");
            PathBuf::from(t)
        };
        // json.dump(state, f, indent=2)
        let body = match serde_json::to_vec_pretty(&Value::Object(self.state.clone())) {
            Ok(b) => b,
            Err(_) => return false,
        };
        if std::fs::write(&tmp, &body).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        if std::fs::rename(&tmp, &target).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return false;
        }
        true
    }

    /// app._set_state: set state[key]=value and persist. On a FAILED persist, roll the in-memory value
    /// back (so a fresh load() — i.e. the next dispatch — also reflects the unchanged disk). Returns
    /// whether the value reached disk.
    fn set_state(&mut self, key: &str, value: Value) -> bool {
        let prev = self.state.get(key).cloned();
        self.state.insert(key.to_string(), value);
        if self.save() {
            return true;
        }
        match prev {
            Some(p) => {
                self.state.insert(key.to_string(), p);
            }
            None => {
                self.state.remove(key);
            }
        }
        false
    }

    /// app.get_theme: state["theme"] or "dark".
    pub fn get_theme(&self) -> String {
        match self.state.get("theme") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => "dark".to_string(),
        }
    }

    /// app.set_theme: {ok, theme}.
    pub fn set_theme(&mut self, t: &str) -> Value {
        let ok = self.set_state("theme", Value::String(t.to_string()));
        json!({"ok": ok, "theme": t})
    }

    /// app.get_auto_push: bool(state.get("auto_push", True)) — default True.
    pub fn get_auto_push(&self) -> bool {
        match self.state.get("auto_push") {
            Some(v) => truthy(v),
            None => true,
        }
    }

    /// app.set_auto_push: {ok, auto_push}.
    pub fn set_auto_push(&mut self, v: bool) -> Value {
        let ok = self.set_state("auto_push", Value::Bool(v));
        json!({"ok": ok, "auto_push": v})
    }

    /// app.get_auto_ai_fix: bool(state.get("auto_ai_fix", False)) — default False.
    pub fn get_auto_ai_fix(&self) -> bool {
        match self.state.get("auto_ai_fix") {
            Some(v) => truthy(v),
            None => false,
        }
    }

    /// app.set_auto_ai_fix: {ok, auto_ai_fix}.
    pub fn set_auto_ai_fix(&mut self, v: bool) -> Value {
        let ok = self.set_state("auto_ai_fix", Value::Bool(v));
        json!({"ok": ok, "auto_ai_fix": v})
    }

    /// app.get_layout: state.get("layout") — the value or null (no default object).
    pub fn get_layout(&self) -> Value {
        self.state.get("layout").cloned().unwrap_or(Value::Null)
    }

    /// app.set_layout: {ok}.
    pub fn set_layout(&mut self, layout: Value) -> Value {
        let ok = self.set_state("layout", layout);
        json!({"ok": ok})
    }
}

/// Read a JSON file as an object Map; None on missing / unreadable / non-object / corrupt (matching
/// app._load_state's `except (OSError, json.JSONDecodeError)` AND the fact that a non-dict top-level
/// JSON would make every `.get` raise — app.py only ever wrote a dict, so a non-object reads as "no
/// usable state" -> fall through to the next source).
fn read_json_object(path: &std::path::Path) -> Option<Map<String, Value>> {
    let bytes = std::fs::read(path).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    match v {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

/// Python truthiness for the auto_push / auto_ai_fix dials: bool(x). null/false/0/""/[]/{} are falsy.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --------------------------------------------------------------------------- #
// arg helpers (JS calls dispatch positionally)
// --------------------------------------------------------------------------- #

fn arg<'a>(args: &'a [Value], i: usize) -> &'a Value {
    args.get(i).unwrap_or(&Value::Null)
}

/// A required string arg (returns "" when absent / null / non-string — Python would have received
/// None and mostly passed it through to a getter that treats it as a missing name -> unknown repo).
fn arg_str(args: &[Value], i: usize) -> String {
    arg(args, i).as_str().unwrap_or("").to_string()
}

/// An optional string arg: Some(s) for a JSON string, None for null/absent. Non-strings -> None.
fn arg_opt_str(args: &[Value], i: usize) -> Option<String> {
    match args.get(i) {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A PR number arg: Python passes an int; accept a JSON int or a numeric string (the JS bridge can
/// stringify). Defaults to 0 when absent/invalid.
fn arg_i64(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// A bool arg with a default (JS `once`/`allow_pi`/`unattended`/`visual_gate` flags). Uses Python
/// truthiness so 0/""/null read false and 1/"x" read true.
fn arg_bool(args: &[Value], i: usize, default: bool) -> bool {
    match args.get(i) {
        Some(Value::Null) | None => default,
        Some(v) => truthy(v),
    }
}

/// `{"ok": false, "error": "unknown repo"}` — the app.py sentinel for a name with no matching repo.
fn unknown_repo() -> Value {
    json!({"ok": false, "error": "unknown repo"})
}

/// app.Api._repo: the first load_repos() entry whose `name` matches, or None.
fn find_repo(name: &str) -> Option<Value> {
    registry::load_repos()
        .into_iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(name))
}

// --------------------------------------------------------------------------- #
// get_state — the composite dashboard payload (per-field _safe resilience)
// --------------------------------------------------------------------------- #

/// app.Api.get_state. PER-FIELD resilience: each field is computed under a catch_unwind so one flaky
/// git/gh call degrades ONLY that field (to its documented default) and never blanks the card or lies
/// `running:false`. The control::* ports return Values rather than panicking, but the guard preserves
/// app.py's `_safe(fn, default)` contract exactly (and shields a future panic in any delegate).
fn get_state(st: &AppState) -> Value {
    let repos = safe(|| registry::load_repos(), Vec::new());
    let gh_ready = safe(|| gh::gh_ready(), false);

    // Each repo's payload is independent — its own git/gh/fs probes, including a network `gh pr list`
    // and ~5 git spawns. The old serial loop made one 4s dashboard refresh cost N×(those spawns).
    // Fan out one thread per repo and collect IN ORDER: O(N×per_repo) -> O(per_repo).
    // ponytail: one OS thread per repo (unbounded); cap with a small pool if the repo count grows large.
    let objs: Vec<&Value> = repos.iter().filter(|r| r.is_object()).collect();
    let out: Vec<Value> = std::thread::scope(|scope| {
        objs.iter()
            .map(|&r| scope.spawn(move || repo_state(r, gh_ready)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap_or(Value::Null))
            .filter(|v| !v.is_null())
            .collect()
    });

    json!({
        "repos": out,
        "gh_ready": gh_ready,
        "theme": st.get_theme(),
        "auto_push": st.get_auto_push(),
        "auto_ai_fix": st.get_auto_ai_fix(),
        "providers": ["ollama-cloud", "openrouter"],
        "keys": safe(|| keys::keys_status(), json!({})),
        "github": safe(|| gh::github_status(), json!({})),
    })
}

/// One repo's get_state payload. Independent git/gh/fs probes, each field guarded by `safe()` so a
/// flaky call degrades only that field (never blanks the card or lies `running:false`). Extracted from
/// get_state's loop so the loop can run one of these per thread (the fields share no state).
fn repo_state(r: &Value, gh_ready: bool) -> Value {
    json!({
        "name": r.get("name").cloned().unwrap_or(Value::Null),
        "path": r.get("path").cloned().unwrap_or(Value::Null),
        "provider": safe(|| Value::String(registry::project_provider(r)), json!("ollama-cloud")),
        "model": safe(|| Value::String(registry::project_model(r)), Value::Null),
        "ship": safe(|| Value::String(registry::project_ship(r)), json!("pr")),
        "gate": safe(|| registry::project_gate(r).map(Value::String).unwrap_or(Value::Null), Value::Null),
        "pr_target_branch": safe(|| Value::String(registry::project_pr_target_branch(r)), json!("main")),
        "reasoning": safe(|| Value::String(registry::project_reasoning(r)), json!("")),
        "goal": safe(|| Value::String(registry::project_goal(r)), json!("")),
        "interval": safe(|| json!(registry::project_interval(r)), json!(120)),
        "max_iterations": safe(|| json!(registry::project_max_iterations(r)), json!(0)),
        "api_key_set": safe(|| Value::Bool(!registry::project_api_key(r).is_empty()), Value::Bool(false)),
        "phases": phases_or_empty(r),
        "is_git": json_bool(r.get("is_git")),
        "has_remote": json_bool(r.get("has_remote")),
        "running": safe(|| Value::Bool(crate::control::locks::is_running(r)), Value::Bool(false)),
        "heartbeat": safe(|| heartbeat::read_heartbeat(r).unwrap_or(Value::Null), Value::Null),
        "prs": if gh_ready {
            safe(|| Value::Array(gh::list_prs(r)), json!([]))
        } else {
            json!([])
        },
        "local_branches": safe(|| Value::Array(branches::local_rsi_branches(r).into_iter().map(Value::String).collect()), json!([])),
        "worktrees": safe(|| Value::Array(branches::list_worktrees(r)), json!([])),
        "hygiene": safe(|| branches::branch_hygiene(r), json!({"dirty": false})),
        "frontend": safe(|| Value::Bool(apptest_health::has_frontend(r)), Value::Bool(false)),
        "browser": safe(|| apptest_health::browser_state(r), json!({"ok": false})),
        "contracts": safe(|| contracts::contracts_present(r), json!({"agent": false, "backlog": false})),
        "diagnosis": safe(|| supervisor::diagnose(r), json!({"category": "ok", "healthy": true})),
        "escalation": safe(|| supervisor::read_escalation(r).unwrap_or(Value::Null), Value::Null),
    })
}

/// `r.get("phases") or {}` — the phases dict verbatim, or an empty object when falsy.
fn phases_or_empty(r: &Value) -> Value {
    match r.get("phases") {
        Some(v) if truthy(v) => v.clone(),
        _ => json!({}),
    }
}

/// `bool(r.get(key))` — Python truthiness of a possibly-missing key, as a JSON bool.
fn json_bool(v: Option<&Value>) -> Value {
    Value::Bool(v.map(truthy).unwrap_or(false))
}

/// app.py's `_safe(fn, default)`: run `fn`, returning `default` if it panics. The control::* delegates
/// return Values (no panic), so this is a defensive shield matching the source's per-field contract.
fn safe<T>(fn_: impl FnOnce() -> T + std::panic::UnwindSafe, default: T) -> T {
    std::panic::catch_unwind(fn_).unwrap_or(default)
}

// --------------------------------------------------------------------------- #
// dispatch — mirror every Api method (JS calls positionally)
// --------------------------------------------------------------------------- #

/// The native equivalent of the pywebview js_api surface. `method` is the Api method name; `args` are
/// the positional arguments in the same order app.py's signature declares them. Returns the bridge
/// Value, or Err(message) for an unknown method (the JS bridge surfaces it as a rejected call).
pub fn dispatch(method: &str, args: &[Value]) -> Result<Value, String> {
    // A fresh load per dispatch: disk (.solomon.json) is the source of truth (see module note).
    let mut st = AppState::load();

    let v: Value = match method {
        // ---- in-app updater --------------------------------------------
        // update_status / apply_update are handled in the `bridge` command (they need the AppHandle for
        // tauri-plugin-updater). cached_update_status stays a benign stub: there is no native cache to read.
        "cached_update_status" => json!({"ok": false, "available": false}),
        "current_sha" => apptest_health::current_sha().map(Value::String).unwrap_or(Value::Null),

        // ---- combined dashboard state -----------------------------------
        "get_state" => get_state(&st),

        // ---- control ----------------------------------------------------
        "start" => match find_repo(&arg_str(args, 0)) {
            Some(r) => runner::start(&r, st.get_auto_push(), arg_bool(args, 1, false)),
            None => unknown_repo(),
        },
        "stop" => match find_repo(&arg_str(args, 0)) {
            Some(r) => runner::stop(&r),
            None => unknown_repo(),
        },
        "beautify" => match find_repo(&arg_str(args, 0)) {
            Some(r) => runner::beautify(&r, st.get_auto_push()),
            None => unknown_repo(),
        },
        "merge" => match find_repo(&arg_str(args, 0)) {
            Some(r) => gh::merge_pr(&r, arg_i64(args, 1)),
            None => unknown_repo(),
        },
        "close" => match find_repo(&arg_str(args, 0)) {
            Some(r) => gh::close_pr(&r, arg_i64(args, 1)),
            None => unknown_repo(),
        },

        // ---- config -----------------------------------------------------
        "set_repo_config" => set_repo_config(args),
        "set_key" => keys::set_key(&arg_str(args, 0), &arg_str(args, 1)),
        "add_project" => add_project(args),
        "connect_project" => connect_project(args),
        "github_login_start" => gh::github_login_start(),

        // ---- review / workspace / insights ------------------------------
        "pr_diff" => match find_repo(&arg_str(args, 0)) {
            Some(r) => gh::pr_diff(&r, arg_i64(args, 1)),
            None => unknown_repo(),
        },
        "read_log" => match find_repo(&arg_str(args, 0)) {
            Some(r) => heartbeat::read_log(&r),
            None => unknown_repo(),
        },
        // read_history returns a LIST (default [] on unknown repo, not the ok-dict sentinel).
        "read_history" => match find_repo(&arg_str(args, 0)) {
            Some(r) => Value::Array(heartbeat::read_history(&r, 20)),
            None => json!([]),
        },
        "read_contract" => match find_repo(&arg_str(args, 0)) {
            Some(r) => contracts::read_contract(&r, &arg_str(args, 1)),
            None => unknown_repo(),
        },
        "write_contract" => match find_repo(&arg_str(args, 0)) {
            Some(r) => contracts::write_contract(&r, &arg_str(args, 1), &arg_str(args, 2)),
            None => unknown_repo(),
        },
        // metrics returns a DICT (default {} on unknown repo).
        "metrics" => match find_repo(&arg_str(args, 0)) {
            Some(r) => heartbeat::metrics(&r),
            None => json!({}),
        },
        "cleanup_worktrees" => match find_repo(&arg_str(args, 0)) {
            Some(r) => branches::cleanup_worktrees(&r),
            None => unknown_repo(),
        },
        "clean_branch" => match find_repo(&arg_str(args, 0)) {
            Some(r) => branches::clean_branch(&r),
            None => unknown_repo(),
        },
        "start_app_test" => match find_repo(&arg_str(args, 0)) {
            Some(r) => apptest_health::start_app_test(&r),
            None => unknown_repo(),
        },
        "stop_app_test" => match find_repo(&arg_str(args, 0)) {
            Some(r) => apptest_health::stop_app_test(&r),
            None => unknown_repo(),
        },
        "app_test_state" => match find_repo(&arg_str(args, 0)) {
            Some(r) => apptest_health::app_test_state(&r, arg_i64_default(args, 1)),
            None => unknown_repo(),
        },
        "app_test_frame" => match find_repo(&arg_str(args, 0)) {
            Some(r) => apptest_health::app_test_frame(&r, arg_i64_default(args, 1)),
            None => unknown_repo(),
        },
        "read_app_test_report" => match find_repo(&arg_str(args, 0)) {
            Some(r) => apptest_health::read_app_test_report(&r),
            None => unknown_repo(),
        },

        // ---- provisioning + Solomon supervisor --------------------------
        "ensure_contracts" => match find_repo(&arg_str(args, 0)) {
            Some(r) => contracts::ensure_contracts(&r),
            None => unknown_repo(),
        },
        "enrich_contract" => match find_repo(&arg_str(args, 0)) {
            Some(r) => runner::enrich_contract(&r, false),
            None => unknown_repo(),
        },
        "ideate" => match find_repo(&arg_str(args, 0)) {
            Some(r) => runner::ideate(&r),
            None => unknown_repo(),
        },
        "supervise" => supervise(&st, args),
        // read_supervisor_log returns a LIST (default [] on unknown repo).
        "read_supervisor_log" => match find_repo(&arg_str(args, 0)) {
            Some(r) => Value::Array(heartbeat::read_supervisor_log(&r, 200)),
            None => json!([]),
        },
        // read_escalation returns the dict or null (null on unknown repo, matching `... if r else None`).
        "read_escalation" => match find_repo(&arg_str(args, 0)) {
            Some(r) => supervisor::read_escalation(&r).unwrap_or(Value::Null),
            None => Value::Null,
        },
        "clear_escalation" => match find_repo(&arg_str(args, 0)) {
            Some(r) => supervisor::clear_escalation(&r),
            None => unknown_repo(),
        },

        // ---- misc -------------------------------------------------------
        "open_url" => open_url(&arg_str(args, 0)),
        "get_theme" => Value::String(st.get_theme()),
        "set_theme" => st.set_theme(&arg_str(args, 0)),
        "get_auto_push" => Value::Bool(st.get_auto_push()),
        "set_auto_push" => st.set_auto_push(arg_bool(args, 0, false)),
        "get_auto_ai_fix" => Value::Bool(st.get_auto_ai_fix()),
        "set_auto_ai_fix" => st.set_auto_ai_fix(arg_bool(args, 0, false)),
        "get_layout" => st.get_layout(),
        "set_layout" => st.set_layout(arg(args, 0).clone()),

        _ => return Err(format!("unknown method: {method}")),
    };
    Ok(v)
}

/// app_test_state / app_test_frame take `after_seq=0` — a defaulted i64 (absent -> 0).
fn arg_i64_default(args: &[Value], i: usize) -> i64 {
    match args.get(i) {
        None | Some(Value::Null) => 0,
        _ => arg_i64(args, i),
    }
}

/// app.set_repo_config: pass through every Some(...) keyword. `phases` is a JSON value (object or null).
/// `api_key` (positional 11) is an optional per-repo OpenRouter/Ollama key overriding the global .env.
fn set_repo_config(args: &[Value]) -> Value {
    let name = arg_str(args, 0);
    // Positional order mirrors app.py: name, provider, model, ship, gate, pr_target_branch, interval,
    // max_iterations, reasoning, goal, phases, api_key.
    let provider = arg_opt_str(args, 1);
    let model = arg_opt_str(args, 2);
    let ship = arg_opt_str(args, 3);
    let gate = arg_opt_str(args, 4);
    let pr_target_branch = arg_opt_str(args, 5);
    let interval = arg_opt_i64(args, 6);
    let max_iterations = arg_opt_i64(args, 7);
    let reasoning = arg_opt_str(args, 8);
    let goal = arg_opt_str(args, 9);
    let phases = match args.get(10) {
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => None,
    };
    // api_key: null/absent -> None (unchanged); a string (incl. "" to clear) -> Some.
    let api_key = match args.get(11) {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    registry::set_repo_config(
        &name,
        provider.as_deref(),
        model.as_deref(),
        ship.as_deref(),
        gate.as_deref(),
        pr_target_branch.as_deref(),
        interval,
        max_iterations,
        reasoning.as_deref(),
        goal.as_deref(),
        phases.as_ref(),
        api_key,
    )
}

/// An optional i64 keyword arg (None for null/absent; numeric / numeric-string -> Some).
fn arg_opt_i64(args: &[Value], i: usize) -> Option<i64> {
    match args.get(i) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// app.add_project(spec, goal=None): clone, then (on ok) set the goal FIRST and kick a background
/// enrich; mark the result `enriching:true` when the enrich call accepted.
fn add_project(args: &[Value]) -> Value {
    let spec = arg_str(args, 0);
    let goal = arg_opt_str(args, 1);
    let r = registry::add_project(&spec);
    if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        return r;
    }
    let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    // set the north-star goal FIRST so the enrichment is steered by it.
    if let Some(g) = &goal {
        let trimmed = g.trim();
        if !trimmed.is_empty() {
            registry::set_repo_config(
                &name,
                None, None, None, None, None, None, None, None,
                Some(trimmed),
                None,
                None,
            );
        } else {
            // app.py passes goal.strip() unconditionally when `goal` is truthy; an all-whitespace
            // string is falsy in Python (`if goal:`), so it is NOT set — matched by the guard above.
        }
    }
    if let Some(repo) = find_repo(&name) {
        if runner::enrich_contract(&repo, true)
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // r = {**r, "enriching": True}
            if let Value::Object(mut o) = r {
                o.insert("enriching".to_string(), Value::Bool(true));
                return Value::Object(o);
            }
        }
    }
    r
}

/// app.connect_project(spec, goal=None, ship="pr", visual_gate=None, provider=None).
fn connect_project(args: &[Value]) -> Value {
    let spec = arg_str(args, 0);
    let goal = arg_opt_str(args, 1);
    // ship defaults to "pr" (app.py's signature default); an explicit null also means "pr".
    let ship = match args.get(2) {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => "pr".to_string(),
    };
    // visual_gate: None (auto-detect) unless an explicit bool is passed.
    let visual_gate = match args.get(3) {
        Some(Value::Bool(b)) => Some(*b),
        Some(Value::Null) | None => None,
        Some(v) => Some(truthy(v)),
    };
    let provider = arg_opt_str(args, 4);
    registry::connect_project(
        &spec,
        goal.as_deref(),
        &ship,
        visual_gate,
        provider.as_deref(),
    )
}

/// app.supervise(name=None, allow_pi=False, unattended=False). Diagnose + recover one repo (name) or
/// all (name=None/absent). allow = allow_pi OR (unattended AND auto_ai_fix); auto_push from state. A
/// per-repo guard keeps one repo's recover() failure from aborting the whole sweep.
fn supervise(st: &AppState, args: &[Value]) -> Value {
    // name: a string targets one repo; null/absent => all repos.
    let name: Option<String> = match args.first() {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let allow_pi = arg_bool(args, 1, false);
    let unattended = arg_bool(args, 2, false);

    let allow = allow_pi || (unattended && st.get_auto_ai_fix());
    let auto_push = st.get_auto_push();

    let targets: Vec<Value> = registry::load_repos()
        .into_iter()
        .filter(|r| {
            r.is_object()
                && match &name {
                    None => true,
                    Some(n) => r.get("name").and_then(Value::as_str) == Some(n.as_str()),
                }
        })
        .collect();

    if name.is_some() && targets.is_empty() {
        return unknown_repo();
    }

    let mut results: Vec<Value> = Vec::new();
    for r in &targets {
        let repo_name = r.get("name").cloned().unwrap_or(Value::Null);
        let r2 = r.clone();
        // Per-repo guard: a panicking recover() must not abort the sweep — surface it per-repo.
        let recovered = std::panic::catch_unwind(move || {
            supervisor::recover(&r2, allow, auto_push, auto_push)
        });
        match recovered {
            Ok(rec) => {
                // {"name": name, **recover(...)}
                let mut obj = Map::new();
                obj.insert("name".to_string(), repo_name);
                if let Value::Object(o) = rec {
                    for (k, val) in o {
                        obj.insert(k, val);
                    }
                }
                results.push(Value::Object(obj));
            }
            Err(e) => {
                let msg = panic_message(e);
                results.push(json!({
                    "name": repo_name,
                    "ok": false,
                    "error": msg,
                    "escalate": true,
                }));
            }
        }
    }
    json!({"ok": true, "results": results})
}

fn panic_message(e: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = e.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = e.downcast_ref::<String>() {
        s.clone()
    } else {
        "panicked".to_string()
    }
}

/// app.open_url: only http(s) links. Anything else -> {ok:false, error}. On a real link, open it in
/// the OS default browser (the Python `webbrowser.open` equivalent).
fn open_url(url: &str) -> Value {
    let lower = url.to_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return json!({"ok": false, "error": "only http(s) URLs are allowed"});
    }
    match open_in_browser(url) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e}),
    }
}

/// The argv (program first) that opens `url` in the OS default browser WITHOUT a shell. On Windows
/// this is `rundll32 url.dll,FileProtocolHandler <url>`: rundll32 hands the URL straight to the
/// registered protocol handler, so URL metacharacters (`&` `|` `^`) are passed LITERALLY and cannot
/// inject a command the way the old `cmd /C start "" <url>` did — which also corrupted legitimate
/// `?a=1&b=2` query strings, since cmd treats `&` as a command separator. Pure so the no-shell
/// guarantee is unit-tested without spawning.
#[cfg(windows)]
fn browser_argv(url: &str) -> Vec<String> {
    vec![
        "rundll32".to_string(),
        "url.dll,FileProtocolHandler".to_string(),
        url.to_string(),
    ]
}

/// webbrowser.open equivalent: hand the URL to the OS default handler. The http(s) gate in open_url
/// has already rejected non-web schemes (local exe / UNC / file://), and no shell is involved, so the
/// URL can neither change scheme nor inject a command.
fn open_in_browser(url: &str) -> Result<(), String> {
    use std::process::Command;
    #[cfg(windows)]
    {
        let argv = browser_argv(url);
        Command::new(&argv[0])
            .args(&argv[1..])
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        Command::new(opener)
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

// --------------------------------------------------------------------------- #
// health_payload — app.py _health_payload
// --------------------------------------------------------------------------- #

/// app._health_payload: overall readiness + a per-repo diagnose summary. Per-repo guard: a repo whose
/// diagnose() panics is reported with `category:"?"` and an error, never blanking the endpoint.
pub fn health_payload() -> Value {
    // Per-repo diagnose() is independent (and spawns git), so fan out one thread per repo and collect
    // in order — same serial-loop win as get_state.
    let all = registry::load_repos();
    let objs: Vec<&Value> = all.iter().filter(|r| r.is_object()).collect();
    let repos: Vec<Value> = std::thread::scope(|scope| {
        objs.iter()
            .map(|&r| scope.spawn(move || health_repo(r)))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap_or(Value::Null))
            .filter(|v| !v.is_null())
            .collect()
    });
    json!({"ok": true, "health": apptest_health::health(), "repos": repos})
}

/// One repo's health entry: diagnose() under a per-repo catch_unwind so a panicking repo is reported
/// (category "?") rather than blanking the endpoint. Extracted so health_payload can run one per thread.
fn health_repo(r: &Value) -> Value {
    let name = r.get("name").cloned().unwrap_or(Value::Null);
    let r2 = r.clone();
    match std::panic::catch_unwind(move || supervisor::diagnose(&r2)) {
        Ok(d) => json!({
            "name": name,
            "running": d.get("running").cloned().unwrap_or(Value::Null),
            "healthy": d.get("healthy").cloned().unwrap_or(Value::Null),
            "category": d.get("category").cloned().unwrap_or(Value::Null),
            "evidence": d.get("evidence").cloned().unwrap_or(Value::Null),
        }),
        Err(e) => json!({"name": name, "category": "?", "error": panic_message(e)}),
    }
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // AppState tests mutate the real .solomon.json under control::paths::here(); snapshot + restore it
    // so the suite is hermetic (the file is operator state, not a fixture). All AppState tests share
    // this one global file, so they MUST run serially — a process-wide mutex held for the whole test
    // body serializes them (cargo runs tests on multiple threads in one process by default).
    static STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct StateGuard {
        path: PathBuf,
        legacy: PathBuf,
        saved: Option<Vec<u8>>,
        saved_legacy: Option<Vec<u8>>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl StateGuard {
        fn capture() -> Self {
            // Hold the lock for the test's lifetime; recover from a poisoned lock (a prior panic).
            let lock = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let path = state_file();
            let legacy = legacy_state_file();
            let saved = std::fs::read(&path).ok();
            let saved_legacy = std::fs::read(&legacy).ok();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(&legacy);
            StateGuard { path, legacy, saved, saved_legacy, _lock: lock }
        }
    }
    impl Drop for StateGuard {
        fn drop(&mut self) {
            match &self.saved {
                Some(b) => {
                    let _ = std::fs::write(&self.path, b);
                }
                None => {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
            match &self.saved_legacy {
                Some(b) => {
                    let _ = std::fs::write(&self.legacy, b);
                }
                None => {
                    let _ = std::fs::remove_file(&self.legacy);
                }
            }
        }
    }

    #[test]
    fn appstate_defaults_when_absent() {
        let _g = StateGuard::capture();
        let st = AppState::load();
        assert_eq!(st.get_theme(), "dark");
        assert!(st.get_auto_push()); // default True
        assert!(!st.get_auto_ai_fix()); // default False
        assert_eq!(st.get_layout(), Value::Null);
    }

    #[test]
    fn appstate_atomic_save_roundtrip() {
        let _g = StateGuard::capture();
        let mut st = AppState::load();
        assert_eq!(st.set_theme("light"), json!({"ok": true, "theme": "light"}));
        assert_eq!(st.set_auto_push(false), json!({"ok": true, "auto_push": false}));
        assert_eq!(st.set_auto_ai_fix(true), json!({"ok": true, "auto_ai_fix": true}));
        assert_eq!(st.set_layout(json!([{"id": "p1", "type": "card", "repo": "x"}])), json!({"ok": true}));
        // a fresh load (next dispatch) sees the persisted values — disk is the source of truth.
        let st2 = AppState::load();
        assert_eq!(st2.get_theme(), "light");
        assert!(!st2.get_auto_push());
        assert!(st2.get_auto_ai_fix());
        assert_eq!(st2.get_layout(), json!([{"id": "p1", "type": "card", "repo": "x"}]));
    }

    #[test]
    fn appstate_legacy_migration() {
        let _g = StateGuard::capture();
        // No .solomon.json, but a legacy .rsi-control.json present -> values come from the legacy file.
        std::fs::write(
            legacy_state_file(),
            serde_json::to_vec(&json!({"theme": "light", "auto_push": false})).unwrap(),
        )
        .unwrap();
        let st = AppState::load();
        assert_eq!(st.get_theme(), "light");
        assert!(!st.get_auto_push());
        // .solomon.json takes precedence when BOTH exist (no legacy fallback then).
        std::fs::write(
            state_file(),
            serde_json::to_vec(&json!({"theme": "dark"})).unwrap(),
        )
        .unwrap();
        let st2 = AppState::load();
        assert_eq!(st2.get_theme(), "dark");
    }

    #[test]
    fn appstate_corrupt_state_reads_as_empty_defaults() {
        let _g = StateGuard::capture();
        std::fs::write(state_file(), b"{ not json").unwrap();
        // corrupt .solomon.json + no legacy -> {} -> defaults.
        let st = AppState::load();
        assert_eq!(st.get_theme(), "dark");
        assert!(st.get_auto_push());
    }

    #[test]
    fn dispatch_unknown_method_errors() {
        let r = dispatch("no_such_method", &[]);
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("unknown method"));
    }

    #[test]
    fn dispatch_updater_stub() {
        // update_status / apply_update now route through the `bridge` command (they need the AppHandle);
        // dispatch only still serves cached_update_status as a benign stub.
        let r = dispatch("cached_update_status", &[]).unwrap();
        assert_eq!(r, json!({"ok": false, "available": false}));
    }

    #[test]
    fn dispatch_unknown_repo_paths() {
        let none = json!("definitely-not-a-registered-repo-xyz");
        // ok-dict sentinel methods.
        for m in [
            "start", "stop", "beautify", "merge", "close", "pr_diff", "read_log", "read_contract",
            "write_contract", "cleanup_worktrees", "clean_branch", "start_app_test", "stop_app_test",
            "app_test_state", "app_test_frame", "read_app_test_report", "ensure_contracts",
            "enrich_contract", "ideate", "clear_escalation",
        ] {
            let r = dispatch(m, &[none.clone()]).unwrap();
            assert_eq!(r, unknown_repo(), "method {m} should return the unknown-repo sentinel");
        }
        // list-default methods.
        assert_eq!(dispatch("read_history", &[none.clone()]).unwrap(), json!([]));
        assert_eq!(dispatch("read_supervisor_log", &[none.clone()]).unwrap(), json!([]));
        // dict-default metrics.
        assert_eq!(dispatch("metrics", &[none.clone()]).unwrap(), json!({}));
        // null-default escalation read.
        assert_eq!(dispatch("read_escalation", &[none.clone()]).unwrap(), Value::Null);
        // supervise(name) with no matching repo -> unknown repo.
        assert_eq!(dispatch("supervise", &[none.clone()]).unwrap(), unknown_repo());
    }

    #[test]
    fn dispatch_open_url_gate() {
        // non-http(s) schemes are rejected before any spawn.
        assert_eq!(
            dispatch("open_url", &[json!("file:///etc/passwd")]).unwrap(),
            json!({"ok": false, "error": "only http(s) URLs are allowed"})
        );
        assert_eq!(
            dispatch("open_url", &[json!("C:/Windows/system32/calc.exe")]).unwrap(),
            json!({"ok": false, "error": "only http(s) URLs are allowed"})
        );
        assert_eq!(
            dispatch("open_url", &[json!("")]).unwrap(),
            json!({"ok": false, "error": "only http(s) URLs are allowed"})
        );
        // a non-string url arg is also rejected (arg_str -> "").
        assert_eq!(
            dispatch("open_url", &[json!(5)]).unwrap(),
            json!({"ok": false, "error": "only http(s) URLs are allowed"})
        );
    }

    #[cfg(windows)]
    #[test]
    fn open_url_uses_no_shell_and_keeps_query_string_verbatim() {
        // Security regression guard: the URL opener must NEVER route through a shell (cmd /C start was
        // a command-injection vector via `&`), and must pass a legitimate query string through verbatim.
        let url = "https://example.com/?a=1&b=2&q=x";
        let argv = browser_argv(url);
        assert_eq!(argv[0], "rundll32");
        assert!(
            !argv.iter().any(|a| {
                let l = a.to_lowercase();
                l == "cmd" || l == "cmd.exe" || l == "/c" || l == "start"
            }),
            "URL opener must not invoke a shell: {argv:?}"
        );
        // the full URL (including the `&` metacharacters) is one verbatim argv element
        assert!(argv.contains(&url.to_string()));
    }

    #[test]
    fn dispatch_theme_setters_roundtrip() {
        let _g = StateGuard::capture();
        assert_eq!(dispatch("get_theme", &[]).unwrap(), json!("dark"));
        assert_eq!(dispatch("set_theme", &[json!("light")]).unwrap(), json!({"ok": true, "theme": "light"}));
        assert_eq!(dispatch("get_theme", &[]).unwrap(), json!("light"));
        assert_eq!(dispatch("get_auto_push", &[]).unwrap(), json!(true));
        assert_eq!(dispatch("set_auto_push", &[json!(false)]).unwrap(), json!({"ok": true, "auto_push": false}));
        assert_eq!(dispatch("get_auto_push", &[]).unwrap(), json!(false));
        assert_eq!(dispatch("get_auto_ai_fix", &[]).unwrap(), json!(false));
        assert_eq!(dispatch("set_auto_ai_fix", &[json!(true)]).unwrap(), json!({"ok": true, "auto_ai_fix": true}));
        assert_eq!(dispatch("get_layout", &[]).unwrap(), Value::Null); // unset default
        // round-trip layout
        let layout = json!([{"id": "a", "type": "card", "repo": "r"}]);
        assert_eq!(dispatch("set_layout", &[layout.clone()]).unwrap(), json!({"ok": true}));
        assert_eq!(dispatch("get_layout", &[]).unwrap(), layout);
    }
}
