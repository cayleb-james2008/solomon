//! Native Rust port of control.py's `registry` module: repos.json read/merge/write, the
//! per-project config getters (provider/model/ship/gate/...), set_repo_config upsert, project
//! discovery, repo-spec parsing, and connect/add_project registration.
//!
//! Bug-for-bug with control.py. Repo entries are `serde_json::Value` objects (never typed structs)
//! so unnamed keys (pipeline, private_paths, deny_terms, public, phases, ...) round-trip untouched.
//! Bridge-return dicts use byte-identical Python keys via `serde_json::json!`.
//!
//! KEY-ORDER DEVIATION (inherited from proc::atomic_write_json): this crate does NOT enable
//! serde_json's `preserve_order` feature, so `serde_json::Map` is a BTreeMap and serializes keys
//! alphabetically rather than in Python's dict-insertion order. Persisted repos.json and returned
//! bridge dicts therefore parse identically to the Python output but are NOT byte-identical in key
//! order. The foundation documents the same trade-off; the golden diff parses-then-compares.

use crate::control::{apptest_health, contracts, gh, keys, paths, proc, runner};
use serde_json::{json, Map, Value};
use std::path::Path;

// control._PROVIDER_DEFAULT_MODEL — keep in sync with improver/run_improver.py PROVIDERS.
fn provider_default_model(provider: &str) -> &'static str {
    match provider {
        "ollama-cloud" => "glm-5.2",
        "openrouter" => "qwen/qwen3-coder",
        // control falls back to the ollama-cloud default for any unknown provider.
        _ => "glm-5.2",
    }
}

pub const AUTOPILOT_DEFAULT_MISSION: &str = "Autonomously improve, ship, monitor, and grow managed projects toward their stated end goals with one efficient Solomon agent, preserving safety gates and proof.";

/// Python truthiness of a JSON value as used by `(repo or {}).get(k) or default`:
/// null / "" / false / 0 / 0.0 / [] / {} are falsy.
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

/// `(repo or {}).get(key)` returning the value or Null when repo is not an object / key absent.
fn get<'a>(repo: &'a Value, key: &str) -> &'a Value {
    repo.get(key).unwrap_or(&Value::Null)
}

/// `(repo or {}).get(key) or default` for string-valued config.
fn str_or(repo: &Value, key: &str, default: &str) -> String {
    let v = get(repo, key);
    if truthy(v) {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
    }
    default.to_string()
}

// --------------------------------------------------------------------------- #
// registry
// --------------------------------------------------------------------------- #

/// control._has_origin: True if `git -C <path> remote get-url origin` returns 0. Safe (False) on
/// error (missing git, empty path, or spawn failure == the Python OSError branch).
pub fn has_origin(path: &str) -> bool {
    let git = match proc::which_git() {
        Some(g) => g,
        None => return false,
    };
    if path.is_empty() {
        return false;
    }
    let git = git.to_string_lossy().into_owned();
    match proc::run(
        &[git.as_str(), "-C", path, "remote", "get-url", "origin"],
        None,
        Some(std::time::Duration::from_secs(30)),
    ) {
        Ok(r) => r.code == 0,
        Err(_) => false, // spawn failure == Python `except OSError: return False`
    }
}

/// control._discover_projects: one entry per immediate subdir of PROJECTS_DIR (dot-names skipped),
/// sorted by name. Each: {name, path(abs), branch_prefix:"rsi/", is_git, has_remote}. Safe ([]) on
/// any OSError (missing/unreadable projects folder).
pub fn discover_projects() -> Vec<Value> {
    let dir = paths::projects_dir();
    let rd = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    // Collect (name, DirEntry) then sort by name to mirror sorted(os.scandir(...), key=name).
    let mut entries: Vec<(String, std::path::PathBuf)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // is_dir + dotfile filter; per-entry OSError in Python `continue`s — read_dir/flatten skips.
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir || name.starts_with('.') {
            continue;
        }
        entries.push((name, e.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (name, p) in entries {
        // os.path.abspath (lexical, no symlink resolution) — NOT canonicalize: a junctioned/symlinked
        // project dir must keep the operator-configured link path, matching _discover_projects's
        // `os.path.abspath(e.path)` and connect_project's clone branch (the lone canonicalize here was
        // an internal inconsistency that resolved links to their target).
        let abs = abspath(&p.to_string_lossy());
        let is_git = Path::new(&abs).join(".git").exists();
        let has_remote = if is_git { has_origin(&abs) } else { false };
        out.push(json!({
            "name": name,
            "path": abs,
            "branch_prefix": "rsi/",
            "is_git": is_git,
            "has_remote": has_remote,
        }));
    }
    out
}

/// control._read_repos_json: lenient raw list read. [] on missing/corrupt/non-list (display paths).
pub fn read_repos_json() -> Vec<Value> {
    read_repos_json_strict().unwrap_or_default()
}

/// Top-level Autopilot config embedded in repos.json as a nameless sentinel entry:
/// `{ "autopilot": { ... } }`. load_repos() skips nameless objects, so the sentinel never becomes a
/// project, while repo-config writes still preserve it. Older `{ "fleet": { ... } }` entries are
/// accepted as a one-release compatibility fallback. Missing fields get conservative single-key
/// defaults.
pub fn autopilot_config() -> Value {
    autopilot_config_from_entries(&read_repos_json())
}

pub fn autopilot_enabled() -> bool {
    autopilot_config()
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("")
        == "single_agent"
}

pub fn autopilot_targets() -> Vec<String> {
    autopilot_config()
        .get("targets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_else(|| {
            vec![
                "sover".to_string(),
                "dotz".to_string(),
                "asmodeus".to_string(),
                "maki".to_string(),
                "solomon".to_string(),
            ]
        })
}

pub fn fleet_config() -> Value {
    autopilot_config()
}

pub fn fleet_enabled() -> bool {
    autopilot_enabled()
}

pub fn fleet_targets() -> Vec<String> {
    autopilot_targets()
}

fn autopilot_config_from_entries(entries: &[Value]) -> Value {
    let mut cfg = match entries
        .iter()
        .find_map(|v| {
            v.as_object()
                .and_then(|o| o.get("autopilot"))
                .and_then(Value::as_object)
                .cloned()
        })
        .or_else(|| {
            entries.iter().find_map(|v| {
                v.as_object()
                    .and_then(|o| o.get("fleet"))
                    .and_then(Value::as_object)
                    .cloned()
            })
        }) {
        Some(o) => o,
        None => Map::new(),
    };
    if cfg.get("mode").and_then(Value::as_str) == Some("single_fleet") {
        cfg.insert("mode".to_string(), json!("single_agent"));
    }
    let mut set_default = |key: &str, value: Value| {
        let missing = !cfg.get(key).map(truthy).unwrap_or(false);
        if missing {
            cfg.insert(key.to_string(), value);
        }
    };
    set_default("mode", json!("single_agent"));
    set_default("mission", json!(AUTOPILOT_DEFAULT_MISSION));
    set_default("provider", json!("ollama-cloud"));
    set_default("api_key", json!("OLLAMA_API_KEY"));
    set_default("model", json!("glm-5.2"));
    set_default("max_concurrent_agent_calls", json!(1));
    set_default("cooldown_s", json!(86400));
    set_default("daily_call_budget", json!(40));
    set_default(
        "adaptive_phase_policy",
        json!("cheap_by_default_deep_on_red_noop_critical_or_campaign"),
    );
    set_default(
        "targets",
        json!(["sover", "dotz", "asmodeus", "maki", "solomon"]),
    );
    Value::Object(cfg)
}

/// control._read_repos_json_strict: open utf-8 + json.load. FileNotFound -> Ok([]); a parsed
/// non-list -> Err("repos.json is not a JSON list"); decode/OS errors -> Err (propagate to the
/// lenient wrappers). This is the ABSENT(->[]) vs PRESENT-but-corrupt(->Err) distinction.
fn read_repos_json_strict() -> Result<Vec<Value>, String> {
    let path = paths::repos_json();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let data: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    match data {
        Value::Array(a) => Ok(a),
        _ => Err("repos.json is not a JSON list".to_string()),
    }
}

/// control._read_repos_for_write: read for a WRITE path. Ok(entries) on success (absent => Ok empty),
/// Err(error_dict_string) when present-but-corrupt so the caller aborts instead of clobbering the
/// good file. The Err carries the bridge error message verbatim.
pub fn read_repos_for_write() -> Result<Vec<Value>, String> {
    read_repos_json_strict().map_err(|_| {
        "repos.json is unreadable/corrupt — refusing to overwrite and lose config".to_string()
    })
}

/// control._write_repo_entries: atomically persist the complete repos.json list. Mirrors
/// os.makedirs(dir) + tmp write + os.replace. io::Result so callers wrap into a bridge dict.
pub fn write_repo_entries(entries: &[Value]) -> std::io::Result<()> {
    let path = paths::repos_json();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    proc::atomic_write_json(&path, &Value::Array(entries.to_vec()))
}

// --------------------------------------------------------------------------- //
// TEST-ONLY repos.json writer lock — serialize any test that does a
// read+modify+write of the live operator repos.json against parallel tests.
// `control::registry::tests::set_repo_config_api_key_round_trip` already held
// `REPOS_LOCK`; this exposes the same lock to other `#[cfg(test)]` modules
// (e.g. `ceo::onboard::tests::onboards_a_local_project_end_to_end_single_tenant`)
// so the parallel `cargo test` TOCTOU on repos.json stays contained.
// --------------------------------------------------------------------------- //
#[cfg(test)]
pub(crate) static REPOS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) struct ReposLockGuard(std::sync::MutexGuard<'static, ()>);
#[cfg(test)]
pub(crate) fn lock_repos_for_test() -> ReposLockGuard {
    ReposLockGuard(REPOS_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
}

/// control.load_repos: merge auto-discovered projects with repos.json config. Discovered entries
/// seed a name->entry map; repos.json dict entries (matched by name) are layered on top
/// (base.update(r)), name re-forced, branch_prefix defaulted to "rsi/". Non-dict / nameless config
/// entries skipped. Returns the merged values in insertion order (discovered first, then new names).
pub fn load_repos() -> Vec<Value> {
    // Preserve insertion order like a Python dict: track key order separately from the lookup map.
    let mut order: Vec<String> = Vec::new();
    let mut merged: std::collections::HashMap<String, Map<String, Value>> =
        std::collections::HashMap::new();

    for d in discover_projects() {
        if let Value::Object(obj) = d {
            let name = obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !merged.contains_key(&name) {
                order.push(name.clone());
            }
            merged.insert(name, obj);
        }
    }

    for r in read_repos_json() {
        let robj = match r {
            Value::Object(o) => o,
            _ => continue, // non-dict skipped
        };
        let name = match robj.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue, // nameless skipped
        };
        let base = merged.entry(name.clone()).or_insert_with(|| {
            order.push(name.clone());
            Map::new()
        });
        // base.update(r): config overrides/augments the discovered entry.
        for (k, v) in robj {
            base.insert(k, v);
        }
        base.insert("name".to_string(), Value::String(name.clone()));
        let needs_prefix = !base.get("branch_prefix").map(truthy).unwrap_or(false);
        if needs_prefix {
            base.insert(
                "branch_prefix".to_string(),
                Value::String("rsi/".to_string()),
            );
        }
    }

    order
        .into_iter()
        .filter_map(|n| merged.remove(&n).map(Value::Object))
        .collect()
}

// --------------------------------------------------------------------------- #
// per-project provider + model
// --------------------------------------------------------------------------- #

/// control.project_provider: repo["provider"] or "ollama-cloud".
pub fn project_provider(repo: &Value) -> String {
    str_or(repo, "provider", "ollama-cloud")
}

/// control.project_model: repo["model"] or the provider's default model.
pub fn project_model(repo: &Value) -> String {
    let v = get(repo, "model");
    if truthy(v) {
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
    }
    provider_default_model(&project_provider(repo)).to_string()
}

/// control.project_ship: ship mode local|push|pr|auto-merge (default "pr").
pub fn project_ship(repo: &Value) -> String {
    str_or(repo, "ship", "pr")
}

/// control.effective_ship: the repo's ship mode when auto_push is on, else "local".
pub fn effective_ship(repo: &Value, auto_push: bool) -> String {
    if auto_push {
        project_ship(repo)
    } else {
        "local".to_string()
    }
}

/// control.project_gate: custom shell test-command, or None (built-in pytest gate).
pub fn project_gate(repo: &Value) -> Option<String> {
    let v = get(repo, "gate");
    if truthy(v) {
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
    }
    None
}

/// control.project_pr_target_branch: branch PRs open against / loop integrates from (default "main").
pub fn project_pr_target_branch(repo: &Value) -> String {
    str_or(repo, "pr_target_branch", "main")
}

/// Resolve a repo's TRUE default branch from git itself — the remote's `origin/HEAD` symbolic ref —
/// so no `"main"`/`"master"` is ever hardcoded per-repo. VERIFIED asymmetry the D0 self-honesty
/// work exists to close: `origin/HEAD -> main` for solomon but `origin/HEAD -> master` for the
/// kairos target, so any single hardcode is wrong for one repo. Tries, in order:
///
///   1. `git symbolic-ref --short refs/remotes/origin/HEAD` (local, no network) -> "origin/<name>",
///      stripped to "<name>". This is the authoritative local record of the remote default.
///   2. `git remote show origin` "HEAD branch: <name>" (may hit the network) as a fallback when the
///      local `origin/HEAD` ref is missing (never populated, or pruned).
///
/// Returns `None` on any failure (no repo dir, no remote, git unavailable, indeterminate output) so
/// the caller can degrade to config/`pr_target_branch` rather than silently assume a wrong branch.
pub fn resolve_default_branch(path: &str) -> Option<String> {
    if path.is_empty() || !Path::new(path).is_dir() {
        return None;
    }
    // 1) local symbolic ref: the cheap, network-free, authoritative path.
    if let Ok(r) = proc::run(
        &[
            "git",
            "-C",
            path,
            "symbolic-ref",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
        None,
        Some(std::time::Duration::from_secs(30)),
    ) {
        if r.code == 0 {
            let out = r.stdout.trim();
            // "origin/main" -> "main"; tolerate an already-stripped value defensively.
            let name = out.strip_prefix("origin/").unwrap_or(out).trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // 2) `git remote show origin` — parse the "HEAD branch: <name>" line. May touch the network,
    // so it is the fallback, not the first choice.
    if let Ok(r) = proc::run(
        &["git", "-C", path, "remote", "show", "origin"],
        None,
        Some(std::time::Duration::from_secs(30)),
    ) {
        if r.code == 0 {
            for line in r.stdout.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("HEAD branch:") {
                    let name = rest.trim();
                    // A detached/unknown remote reports "(unknown)" — not a usable branch.
                    if !name.is_empty() && name != "(unknown)" {
                        return Some(name.to_string());
                    }
                }
            }
        }
    }
    None
}

/// The base branch to treat as this repo's integration target, resolved HONESTLY: the git remote's
/// true default branch (`resolve_default_branch`) wins, so the control plane never hardcodes
/// `main`/`master`; only when git can't resolve it (offline, no `origin/HEAD`, no remote) does it
/// fall back to the repos.json `pr_target_branch` config, and finally the historical `"main"`
/// default. `path` is the repo working dir; `repo` its repos.json row (for the config fallback).
pub fn project_resolved_base_branch(repo: &Value, path: &str) -> String {
    if let Some(b) = resolve_default_branch(path) {
        return b;
    }
    project_pr_target_branch(repo)
}

/// Python `int(x or 0)` semantics for the interval/max_iterations getters: a falsy value -> 0;
/// an int/float -> truncated toward zero; a numeric string -> parsed int; anything else -> 0
/// (TypeError/ValueError branch). Floats and numeric strings with fractional parts truncate.
fn int_or_zero(v: &Value) -> i64 {
    if !truthy(v) {
        return 0;
    }
    match v {
        // int(float) truncates toward zero; int(int) is the int.
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i
            } else if let Some(f) = n.as_f64() {
                f.trunc() as i64
            } else {
                0
            }
        }
        // int("60") works; int("abc") and int("90.9") raise ValueError -> 0. Python int() on a
        // string rejects fractional/decimal forms, so only a pure base-10 integer string parses.
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        // bool true is truthy but int(True)==1 in Python; repos.json never stores bool here, but
        // mirror it for completeness.
        Value::Bool(true) => 1,
        _ => 0,
    }
}

/// control.project_interval: seconds between iterations (default 120; non-positive/invalid -> 120).
pub fn project_interval(repo: &Value) -> i64 {
    let v = int_or_zero(get(repo, "interval"));
    if v > 0 {
        v
    } else {
        120
    }
}

/// control.project_max_iterations: iterations before self-stop; 0 = unlimited (invalid/negative -> 0).
pub fn project_max_iterations(repo: &Value) -> i64 {
    let v = int_or_zero(get(repo, "max_iterations"));
    v.max(0)
}

/// control.project_reasoning: pi --thinking level, default "xhigh".
pub fn project_reasoning(repo: &Value) -> String {
    str_or(repo, "reasoning", "xhigh")
}

/// control.project_api_key: a per-repo API key (overrides the global .env key for this repo's
/// iterations), or "" when unset. Stored under the repo's `api_key` field in repos.json. Never
/// logged verbatim by callers (Ctx::redact scrubs the active key env value); get_state surfaces
/// only a bool `api_key_set`.
pub fn project_api_key(repo: &Value) -> String {
    str_or(repo, "api_key", "")
}

/// control.project_goal: the north-star goal, stripped, or "" (unset).
pub fn project_goal(repo: &Value) -> String {
    let v = get(repo, "goal");
    let s = if truthy(v) {
        v.as_str().unwrap_or("")
    } else {
        ""
    };
    s.trim().to_string()
}

/// control.project_sandbox: the `sandbox` dict when it is a dict AND its `enabled` is truthy, else
/// Null (review disabled). Returns the sandbox Value verbatim.
pub fn project_sandbox(repo: &Value) -> Value {
    let sb = get(repo, "sandbox");
    if let Value::Object(o) = sb {
        if o.get("enabled").map(truthy).unwrap_or(false) {
            return sb.clone();
        }
    }
    Value::Null
}

// --------------------------------------------------------------------------- #
// freshness disposition (Phase A.2) — the startup CONTRACT the fleet must satisfy
// --------------------------------------------------------------------------- #
//
// Every fleet lane must have an HONEST freshness disposition so the improver never optimizes a
// blind objective (freshness.rs failure catalog #1 "value-blind objective"): a lane is EITHER
//   (a) explicitly `no_objective: true` — a documented sentinel: this lane has no settled
//       real-world metric to gate on today (a non-live code-quality lane), so freshness is off and the
//       lane runs legacy (never a fabricated green, never a false RED); OR
//   (b) a real emitter: `freshness.cmd` whose script FILE EXISTS on disk, resolved against the
//       lane's repo `path`. A missing/wrong emitter path is the DOCUMENTED trap (sover code:
//       "a wrong path makes a healthy lane read UNOBSERVABLE and halt forever") — it must be a
//       loud contract failure, not a silent starve.
// The startup contract test (`fleet_lanes_have_freshness_emitter_or_are_explicitly_no_objective`)
// asserts exactly this over the REAL repos.json.

/// A fleet lane's freshness disposition, classified from its repos.json row.
#[derive(Debug, Clone, PartialEq)]
pub enum FreshnessDisposition {
    /// No `freshness` block at all — legacy behavior, but for a FLEET lane this is a contract
    /// hole (the operator neither configured an emitter nor declared the lane objective-free).
    Absent,
    /// `freshness: { no_objective: true, ... }` — the explicit "no settled metric today" sentinel.
    NoObjective,
    /// A real emitter cmd. `emitter` is the extracted script token (repo-relative), or None when
    /// the cmd carries no resolvable `.py` script (a malformed emitter cmd).
    Emitter {
        cmd: String,
        emitter: Option<String>,
    },
}

/// Classify a repo row's freshness disposition. `no_objective: true` wins; else a non-empty
/// `cmd` is an Emitter (with its extracted script); else Absent. Mirrors freshness.rs's own
/// "empty/absent cmd => feature off" rule, but distinguishes the DELIBERATE no_objective sentinel
/// from a plain hole so the contract test can require one or the other for every fleet lane.
pub fn freshness_disposition(repo: &Value) -> FreshnessDisposition {
    let f = match repo.get("freshness").and_then(Value::as_object) {
        Some(o) => o,
        None => return FreshnessDisposition::Absent,
    };
    if f.get("no_objective").map(Value::as_bool).unwrap_or(None) == Some(true) {
        return FreshnessDisposition::NoObjective;
    }
    let cmd = f
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if cmd.is_empty() {
        // A freshness block with neither no_objective nor a cmd is a hole, not a valid emitter.
        return FreshnessDisposition::Absent;
    }
    let emitter = emitter_script_from_cmd(&cmd);
    FreshnessDisposition::Emitter { cmd, emitter }
}

/// Extract the emitter SCRIPT path from a freshness cmd: the first whitespace-separated token
/// that looks like a script file (ends in `.py`, `.ps1`, `.js`, `.sh`, or `.exe`). The
/// interpreter prefix (`python`, `.venv\Scripts\python`) and trailing args (`--freshness`) are
/// skipped. Returns the token verbatim (repo-relative, native separators) or None when the cmd
/// has no recognizable script token (a malformed emitter — the contract test treats that as a
/// FAIL, same as a missing file: an unresolvable emitter cannot be verified to exist).
pub fn emitter_script_from_cmd(cmd: &str) -> Option<String> {
    const SCRIPT_EXTS: &[&str] = &[".py", ".ps1", ".js", ".sh"];
    cmd.split_whitespace()
        .find(|tok| {
            let low = tok.to_ascii_lowercase();
            SCRIPT_EXTS.iter().any(|e| low.ends_with(e))
        })
        .map(str::to_string)
}

/// Resolve a lane's emitter script to an absolute path under its repo `path`, and report whether
/// the file exists on disk. Used by the startup contract test. Returns:
///   - None: the lane is not an Emitter disposition (no emitter path to resolve), OR the emitter
///     cmd had no recognizable script token (the caller reports that as a contract failure), OR
///     the repo `path` is missing/empty (cannot resolve — also a failure for the caller).
///   - Some((resolved_abs_path, exists)): the resolved absolute path and whether it exists.
pub fn resolve_emitter_exists(repo: &Value) -> Option<(std::path::PathBuf, bool)> {
    let emitter = match freshness_disposition(repo) {
        FreshnessDisposition::Emitter {
            emitter: Some(e), ..
        } => e,
        _ => return None,
    };
    let base = get(repo, "path").as_str().unwrap_or("").to_string();
    if base.trim().is_empty() {
        return None;
    }
    // Normalize the emitter's separators to the platform form before joining, so a
    // Windows-style `tools\freshness.py` resolves on a POSIX CI box too.
    let native = if cfg!(windows) {
        emitter.replace('/', "\\")
    } else {
        emitter.replace('\\', "/")
    };
    let path = Path::new(&base).join(native);
    let exists = path.exists();
    Some((path, exists))
}

// --------------------------------------------------------------------------- #
// set_repo_config
// --------------------------------------------------------------------------- #

/// control.set_repo_config: upsert the repos.json entry for `name`, setting any passed (Some) keys.
/// Read-modify-write the whole list; preserve untouched keys; atomic write. Creates the entry
/// (carrying its discovered path) when absent. `phases` Some(obj) full-replaces the per-phase map,
/// pruning empty per-phase dicts; an empty result removes the `phases` key entirely. `api_key`
/// Some("") clears the per-repo key (writes empty, which `project_api_key` treats as unset);
/// Some(non-empty) sets it; None leaves it untouched.
#[allow(clippy::too_many_arguments)]
pub fn set_repo_config(
    name: &str,
    provider: Option<&str>,
    model: Option<&str>,
    ship: Option<&str>,
    gate: Option<&str>,
    pr_target_branch: Option<&str>,
    interval: Option<i64>,
    max_iterations: Option<i64>,
    reasoning: Option<&str>,
    goal: Option<&str>,
    phases: Option<&Value>,
    api_key: Option<&str>,
) -> Value {
    if name.is_empty() {
        return json!({"ok": false, "error": "name required"});
    }
    let mut entries = match read_repos_for_write() {
        Ok(e) => e,
        Err(error) => return json!({"ok": false, "error": error}),
    };

    // Find the existing entry index (first dict whose name matches).
    let idx = entries
        .iter()
        .position(|r| r.is_object() && r.get("name").and_then(Value::as_str) == Some(name));

    let pos = match idx {
        Some(i) => i,
        None => {
            // New entry: name, branch_prefix, then [path] if discovered.
            let mut obj = Map::new();
            obj.insert("name".to_string(), Value::String(name.to_string()));
            obj.insert(
                "branch_prefix".to_string(),
                Value::String("rsi/".to_string()),
            );
            if let Some(disc) = discover_projects()
                .into_iter()
                .find(|d| d.get("name").and_then(Value::as_str) == Some(name))
            {
                if let Some(p) = disc.get("path") {
                    obj.insert("path".to_string(), p.clone());
                }
            }
            entries.push(Value::Object(obj));
            entries.len() - 1
        }
    };

    {
        let entry = entries[pos]
            .as_object_mut()
            .expect("matched entry is an object");
        if let Some(v) = provider {
            entry.insert("provider".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = model {
            entry.insert("model".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = ship {
            entry.insert("ship".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = gate {
            entry.insert("gate".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = pr_target_branch {
            entry.insert("pr_target_branch".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = interval {
            entry.insert("interval".to_string(), json!(v));
        }
        if let Some(v) = max_iterations {
            entry.insert("max_iterations".to_string(), json!(v));
        }
        if let Some(v) = reasoning {
            entry.insert("reasoning".to_string(), Value::String(v.to_string()));
        }
        if let Some(v) = goal {
            entry.insert("goal".to_string(), Value::String(v.to_string()));
        }
        if let Some(ph) = phases {
            // {k: v for k, v in phases.items() if isinstance(v, dict) and v}
            let pruned: Map<String, Value> = ph
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter(|(_, v)| matches!(v, Value::Object(o) if !o.is_empty()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default();
            if pruned.is_empty() {
                entry.remove("phases");
            } else {
                entry.insert("phases".to_string(), Value::Object(pruned));
            }
        }
        if let Some(k) = api_key {
            // Some("") clears the per-repo key (treat empty as unset, matching project_api_key's
            // truthiness check); Some(non-empty) sets it. Stored plaintext like the global .env keys.
            entry.insert("api_key".to_string(), Value::String(k.to_string()));
        }
    }

    match write_repo_entries(&entries) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// D8 onboarding: upsert a repos.json row for a NEWLY-ONBOARDED local project in ONE atomic write,
/// setting the onboarding-specific keys `set_repo_config` does not cover (`path`, `freshness`,
/// `api_key` env-name, `goal`, `gate`, `provider`). This is the ZERO-hand-editing writer: the
/// onboarding path calls it so the operator never edits repos.json by hand. RMW the whole list,
/// preserve every untouched key on an existing row, atomic write.
///
///   * `freshness` — the seeded freshness block. Per D1's rule the caller passes EITHER an
///     `{"cmd": ...}` block (only when its emitter script was VERIFIED present on disk) OR the honest
///     `{"no_objective": true}` sentinel (when no real emitter exists) — so this writer never seeds a
///     blind objective. Passed verbatim; `None` leaves any existing `freshness` untouched.
///   * `api_key` — the API-KEY ENV NAME (e.g. "OLLAMA_API_KEY"), stored under `api_key`. Never a
///     secret value: the onboarding contract is (path + api-key-ENV), and the env var is resolved at
///     run time exactly like every other lane's key.
///
/// A NEW row is created with `name`, `branch_prefix: "rsi/"`, and `path`; `error` on a corrupt
/// repos.json (refuse to clobber). Returns `{ok:true, created:bool}` / `{ok:false, error}`.
#[allow(clippy::too_many_arguments)]
pub fn upsert_onboarded_repo(
    name: &str,
    path: &str,
    gate: &str,
    goal: Option<&str>,
    provider: Option<&str>,
    api_key_env: Option<&str>,
    freshness: Option<&Value>,
) -> Value {
    if name.is_empty() {
        return json!({"ok": false, "error": "name required"});
    }
    if path.trim().is_empty() {
        return json!({"ok": false, "error": "path required"});
    }
    let mut entries = match read_repos_for_write() {
        Ok(e) => e,
        Err(error) => return json!({"ok": false, "error": error}),
    };

    let idx = entries
        .iter()
        .position(|r| r.is_object() && r.get("name").and_then(Value::as_str) == Some(name));
    let created = idx.is_none();
    let pos = match idx {
        Some(i) => i,
        None => {
            let mut obj = Map::new();
            obj.insert("name".to_string(), Value::String(name.to_string()));
            obj.insert("branch_prefix".to_string(), Value::String("rsi/".to_string()));
            entries.push(Value::Object(obj));
            entries.len() - 1
        }
    };

    {
        let entry = entries[pos]
            .as_object_mut()
            .expect("matched/created entry is an object");
        entry.insert("path".to_string(), Value::String(path.to_string()));
        if !gate.is_empty() {
            entry.insert("gate".to_string(), Value::String(gate.to_string()));
        }
        if let Some(g) = goal {
            entry.insert("goal".to_string(), Value::String(g.trim().to_string()));
        }
        if let Some(p) = provider {
            if !p.is_empty() {
                entry.insert("provider".to_string(), Value::String(p.to_string()));
            }
        }
        if let Some(k) = api_key_env {
            entry.insert("api_key".to_string(), Value::String(k.to_string()));
        }
        if let Some(f) = freshness {
            entry.insert("freshness".to_string(), f.clone());
        }
    }

    match write_repo_entries(&entries) {
        Ok(()) => json!({"ok": true, "created": created}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

// --------------------------------------------------------------------------- #
// connect / add / parse
// --------------------------------------------------------------------------- #

/// control._parse_repo_spec: bare repo name from a GitHub spec, or None. Accepts
/// https://github.com/owner/repo[.git], owner/repo. Non-GitHub URLs / wrong arity -> None.
pub fn parse_repo_spec(spec: &str) -> Option<(String, String)> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    let mut s = s.to_string();
    if s.starts_with("http://") || s.starts_with("https://") {
        if !s.contains("github.com/") {
            return None;
        }
        // s.split("github.com/", 1)[1]
        s = s
            .split_once("github.com/")
            .map(|x| x.1)
            .unwrap_or("")
            .to_string();
    }
    let s = s.trim_matches('/');
    let s = s.strip_suffix(".git").unwrap_or(s);
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() != 2 {
        return None;
    }
    Some((parts[0].to_string(), parts[1].to_string()))
}

/// control.add_project: clone a GitHub repo into PROJECTS_DIR. {ok:true, name} or {ok:false, error}.
pub fn add_project(spec: &str) -> Value {
    let name = match parse_repo_spec(spec) {
        Some((_owner, name)) => name,
        None => return json!({"ok": false, "error": "not a GitHub repo spec (owner/repo or URL)"}),
    };
    let gh_exe = match proc::which_gh() {
        Some(g) => g,
        None => return json!({"ok": false, "error": "gh not found"}),
    };
    let projects = paths::projects_dir();
    if let Err(e) = std::fs::create_dir_all(&projects) {
        return json!({"ok": false, "error": e.to_string()});
    }
    let dest = projects.join(&name);
    let gh_s = gh_exe.to_string_lossy().into_owned();
    let dest_s = dest.to_string_lossy().into_owned();
    // Bounded timeout (network clone on a UI handler thread) — a hung/unreachable GitHub or a
    // credential prompt on the null stdin must not block the handler forever. 5 min is generous for
    // a large repo; on timeout the Err branch returns {ok:false,error} instead of hanging.
    let r = match proc::run(
        &[gh_s.as_str(), "repo", "clone", spec.trim(), dest_s.as_str()],
        Some(&projects),
        Some(std::time::Duration::from_secs(300)),
    ) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    if r.code == 0 {
        json!({"ok": true, "name": name})
    } else {
        let msg = if !r.stderr.trim().is_empty() {
            r.stderr.trim()
        } else if !r.stdout.trim().is_empty() {
            r.stdout.trim()
        } else {
            "clone failed"
        };
        json!({"ok": false, "error": msg})
    }
}

/// control.connect_project: register a local dir or clone+register a GitHub repo in one call.
/// Returns the success dict (keys: ok, name, path, visual_gate, contracts, enriching,
/// provider_ready, sandbox_configured) or a {ok:false,error} dict.
pub fn connect_project(
    spec: &str,
    goal: Option<&str>,
    ship: &str,
    visual_gate: Option<bool>,
    provider: Option<&str>,
) -> Value {
    // 1) expandvars + expanduser + strip.
    let raw = expand(spec.trim());

    // 2/3) local dir vs clone.
    let (name, path) = if Path::new(&raw).is_dir() {
        let abs = abspath(&raw);
        let nm = Path::new(&normpath(&abs))
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        (nm, abs)
    } else {
        let cloned = add_project(&raw);
        if !cloned.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            return cloned;
        }
        let nm = cloned
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let p = paths::projects_dir().join(&nm);
        (nm, abspath(&p.to_string_lossy()))
    };

    // 4) validate.
    if name.is_empty() || !Path::new(&path).is_dir() {
        return json!({"ok": false, "error": "project path not found"});
    }

    // 5) build entry.
    let git_dir = Path::new(&path).join(".git");
    let is_git = git_dir.exists();
    let has_remote = if is_git { has_origin(&path) } else { false };
    let mut entry = Map::new();
    entry.insert("name".to_string(), Value::String(name.clone()));
    entry.insert("path".to_string(), Value::String(path.clone()));
    entry.insert(
        "branch_prefix".to_string(),
        Value::String("rsi/".to_string()),
    );
    entry.insert("is_git".to_string(), Value::Bool(is_git));
    entry.insert("has_remote".to_string(), Value::Bool(has_remote));
    entry.insert(
        "ship".to_string(),
        Value::String(if ship.is_empty() {
            "pr".to_string()
        } else {
            ship.to_string()
        }),
    );

    // 6) goal.
    if let Some(g) = goal {
        entry.insert("goal".to_string(), Value::String(g.trim().to_string()));
    }

    // 7) provider: explicit wins; else first provider with a key (ollama-cloud, openrouter); else unset.
    let chosen_provider: Option<String> = match provider {
        Some(p) => Some(p.to_string()),
        None => {
            let ks = keys::keys_status();
            ["ollama-cloud", "openrouter"]
                .iter()
                .find(|p| ks.get(**p).and_then(Value::as_bool).unwrap_or(false))
                .map(|p| p.to_string())
        }
    };
    if let Some(p) = &chosen_provider {
        // Python: `if provider:` — only truthy (non-empty) providers are written.
        if !p.is_empty() {
            entry.insert("provider".to_string(), Value::String(p.clone()));
        }
    }

    // 8) public visibility from origin.
    if has_remote {
        if let Some(vis) = gh::gh_repo_visibility(&path) {
            entry.insert("public".to_string(), Value::Bool(vis));
        }
    }

    // 9) visual_gate.
    let entry_val = Value::Object(entry.clone());
    let resolved_visual_gate = match visual_gate {
        None => apptest_health::has_frontend(&entry_val),
        Some(v) => v,
    };
    entry.insert("visual_gate".to_string(), Value::Bool(resolved_visual_gate));

    // 10) read for write.
    let mut entries = match read_repos_for_write() {
        Ok(e) => e,
        Err(error) => return json!({"ok": false, "error": error}),
    };

    // 11) merge onto existing or append.
    let new_entry = Value::Object(entry);
    let idx = entries.iter().position(|r| {
        r.is_object() && r.get("name").and_then(Value::as_str) == Some(name.as_str())
    });
    let final_entry: Value = match idx {
        None => {
            entries.push(new_entry.clone());
            new_entry
        }
        Some(i) => {
            // existing.update(entry): merge new keys onto existing, keeping existing's other keys.
            let existing = entries[i].as_object_mut().expect("matched entry is object");
            if let Value::Object(no) = &new_entry {
                for (k, v) in no {
                    existing.insert(k.clone(), v.clone());
                }
            }
            entries[i].clone()
        }
    };

    // 12) write.
    if let Err(e) = write_repo_entries(&entries) {
        return json!({"ok": false, "error": e.to_string()});
    }

    // 13) contracts.
    let contracts_val = contracts::ensure_contracts(&final_entry);

    // 14) enrich (best-effort; failure => enriching=false).
    let enriching = runner::enrich_contract(&final_entry, true)
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 15) success dict.
    let provider_ready = keys::keys_status()
        .get(project_provider(&final_entry))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sandbox_configured = truthy(&project_sandbox(&final_entry));

    json!({
        "ok": true,
        "name": name,
        "path": path,
        "visual_gate": resolved_visual_gate,
        "contracts": contracts_val,
        "enriching": enriching,
        "provider_ready": provider_ready,
        "sandbox_configured": sandbox_configured,
    })
}

// os.path.expandvars + expanduser: expand a leading ~ and $VAR / %VAR% references.
fn expand(s: &str) -> String {
    let mut out = expanduser(s);
    out = expandvars(&out);
    out
}

fn expanduser(s: &str) -> String {
    if let Some(rest) = s.strip_prefix('~') {
        if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') {
            if let Some(home) = home_dir() {
                return format!("{home}{rest}");
            }
        }
    }
    s.to_string()
}

fn home_dir() -> Option<String> {
    std::env::var("USERPROFILE")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
}

// Expand %VAR% (Windows) and $VAR / ${VAR} references, leaving unknown vars intact (Python behavior).
fn expandvars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == '%' {
            if let Some(end) = bytes[i + 1..].iter().position(|&ch| ch == '%') {
                // ntpath.expandvars collapses an escaped `%%` (empty var name) to a single `%`.
                if end == 0 {
                    out.push('%');
                    i += 2;
                    continue;
                }
                let name: String = bytes[i + 1..i + 1 + end].iter().collect();
                if let Ok(v) = std::env::var(&name) {
                    out.push_str(&v);
                } else {
                    out.push('%');
                    out.push_str(&name);
                    out.push('%');
                }
                i = i + 1 + end + 1;
                continue;
            }
        } else if c == '$' && i + 1 < bytes.len() {
            if bytes[i + 1] == '{' {
                if let Some(end) = bytes[i + 2..].iter().position(|&ch| ch == '}') {
                    let name: String = bytes[i + 2..i + 2 + end].iter().collect();
                    match std::env::var(&name) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => out.push_str(&format!("${{{name}}}")),
                    }
                    i = i + 2 + end + 1;
                    continue;
                }
            } else {
                let mut j = i + 1;
                while j < bytes.len() && (bytes[j].is_alphanumeric() || bytes[j] == '_') {
                    j += 1;
                }
                if j > i + 1 {
                    let name: String = bytes[i + 1..j].iter().collect();
                    match std::env::var(&name) {
                        Ok(v) => out.push_str(&v),
                        Err(_) => {
                            out.push('$');
                            out.push_str(&name);
                        }
                    }
                    i = j;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

// os.path.abspath: absolute, normalized path (best-effort without requiring existence).
fn abspath(p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    normpath(&abs.to_string_lossy())
}

// os.path.normpath: collapse separators and . / .. segments. Keeps the drive/root.
fn normpath(p: &str) -> String {
    // Normalize separators to the platform form, then resolve . and .. lexically.
    let win = cfg!(windows);
    let sep = if win { '\\' } else { '/' };
    let unified = p.replace('\\', "/");
    let (prefix, rest) = split_drive(&unified);
    let is_abs = rest.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(&last) = stack.last() {
                    if last != ".." {
                        stack.pop();
                        continue;
                    }
                }
                if !is_abs {
                    stack.push("..");
                }
            }
            s => stack.push(s),
        }
    }
    let body = stack.join("/");
    let joined = if is_abs {
        format!("{prefix}/{body}")
    } else if prefix.is_empty() {
        if body.is_empty() {
            ".".to_string()
        } else {
            body
        }
    } else {
        format!("{prefix}{body}")
    };
    if win {
        joined.replace('/', &sep.to_string())
    } else {
        joined
    }
}

// Split a leading drive letter (C:) OR a UNC root (\\server\share) on Windows-style paths (`p` is in
// unified forward-slash form). Mirrors ntpath.splitdrive: without the UNC branch the two leading
// separators collapse to one in normpath, mangling \\server\share\proj into \server\share\proj so a
// UNC-hosted project path no longer resolves.
fn split_drive(p: &str) -> (String, &str) {
    let b = p.as_bytes();
    // Drive letter "C:".
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return (p[..2].to_string(), &p[2..]);
    }
    // UNC root "//server/share": exactly two leading separators, then a non-empty server AND share.
    if b.len() > 2 && b[0] == b'/' && b[1] == b'/' && b[2] != b'/' {
        if let Some(rel_sep) = p[2..].find('/') {
            let server_end = 2 + rel_sep; // index of the '/' after the server component
            let share_rel = &p[server_end + 1..]; // "share" or "share/rest..."
            let share_len = share_rel.find('/').unwrap_or(share_rel.len());
            if share_len > 0 {
                let drive_end = server_end + 1 + share_len; // end of "//server/share"
                return (p[..drive_end].to_string(), &p[drive_end..]);
            }
        }
    }
    (String::new(), p)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    // ---- _PROVIDER_DEFAULT_MODEL ----
    #[test]
    fn provider_default_model_table() {
        assert_eq!(provider_default_model("ollama-cloud"), "glm-5.2");
        assert_eq!(provider_default_model("openrouter"), "qwen/qwen3-coder");
        assert_eq!(provider_default_model("anything-else"), "glm-5.2");
    }

    // ---- project_provider ----
    #[test]
    fn project_provider_vectors() {
        assert_eq!(project_provider(&Value::Null), "ollama-cloud");
        assert_eq!(project_provider(&json!({})), "ollama-cloud");
        assert_eq!(project_provider(&json!({"provider": ""})), "ollama-cloud");
        assert_eq!(
            project_provider(&json!({"provider": "openrouter"})),
            "openrouter"
        );
    }

    // ---- project_model ----
    #[test]
    fn project_model_vectors() {
        assert_eq!(
            project_model(&json!({"model": "kimi-k2.7-code", "provider": "ollama-cloud"})),
            "kimi-k2.7-code"
        );
        assert_eq!(
            project_model(&json!({"provider": "ollama-cloud"})),
            "glm-5.2"
        );
        assert_eq!(
            project_model(&json!({"provider": "openrouter"})),
            "qwen/qwen3-coder"
        );
        assert_eq!(project_model(&json!({"provider": "anthropic"})), "glm-5.2");
        assert_eq!(project_model(&Value::Null), "glm-5.2");
    }

    // ---- project_ship / effective_ship ----
    #[test]
    fn project_ship_vectors() {
        assert_eq!(project_ship(&json!({})), "pr");
        assert_eq!(project_ship(&json!({"ship": ""})), "pr");
        assert_eq!(project_ship(&json!({"ship": "auto-merge"})), "auto-merge");
    }

    #[test]
    fn effective_ship_vectors() {
        assert_eq!(effective_ship(&json!({"ship": "push"}), true), "push");
        assert_eq!(effective_ship(&json!({"ship": "push"}), false), "local");
        assert_eq!(effective_ship(&json!({}), true), "pr");
    }

    // ---- project_gate ----
    #[test]
    fn project_gate_vectors() {
        assert_eq!(project_gate(&json!({})), None);
        assert_eq!(project_gate(&json!({"gate": ""})), None);
        assert_eq!(
            project_gate(&json!({"gate": "make test"})),
            Some("make test".to_string())
        );
    }

    // ---- project_pr_target_branch ----
    #[test]
    fn project_pr_target_branch_vectors() {
        assert_eq!(project_pr_target_branch(&json!({})), "main");
        assert_eq!(
            project_pr_target_branch(&json!({"pr_target_branch": "develop"})),
            "develop"
        );
    }

    // ---- resolve_default_branch / project_resolved_base_branch (D0 dynamic default) ----
    // The resolver reads git's REAL remote default (`origin/HEAD`), never a hardcoded name — the
    // load-bearing fix for the verified asymmetry (solomon -> main, kairos -> master). Drives a real
    // temp repo with a bare remote whose origin/HEAD is set to a NON-`main` branch, proving the
    // resolver returns that branch (so a `master`-default repo is handled correctly).
    #[test]
    fn resolve_default_branch_reads_origin_head_not_a_hardcode() {
        use std::process::Command;
        let tag = format!(
            "reg_dflt_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(format!("solomon_{tag}"));
        let remote = std::env::temp_dir().join(format!("solomon_{tag}_remote.git"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed");
        };
        Command::new("git")
            .args(["init", "--bare", &remote.to_string_lossy()])
            .status()
            .unwrap();
        git(&["init"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "--allow-empty", "-m", "init"]);
        // Force a NON-main default name so the assertion can't pass by hardcode coincidence.
        git(&["branch", "-M", "trunk-xyz"]);
        git(&["remote", "add", "origin", &remote.to_string_lossy()]);
        git(&["push", "-u", "origin", "trunk-xyz"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk-xyz",
        ]);

        let root_s = root.to_string_lossy().into_owned();
        assert_eq!(
            resolve_default_branch(&root_s).as_deref(),
            Some("trunk-xyz"),
            "resolver must return git's real origin/HEAD default, not a hardcoded main/master"
        );
        // project_resolved_base_branch prefers the git-resolved default over a DIFFERENT config
        // value — the config is only a fallback for when git can't resolve it.
        let repo_row = json!({"name": "x", "pr_target_branch": "master"});
        assert_eq!(
            project_resolved_base_branch(&repo_row, &root_s),
            "trunk-xyz",
            "git-resolved default must win over the repos.json pr_target_branch"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
    }

    #[test]
    fn resolve_default_branch_none_when_no_repo_or_remote() {
        // A non-existent path -> None (caller degrades to config).
        assert_eq!(resolve_default_branch("/no/such/repo/xyzzy"), None);
        assert_eq!(resolve_default_branch(""), None);
        // project_resolved_base_branch then falls back to the repos.json pr_target_branch — verified
        // to preserve a `master` default (the kairos shape) when git can't resolve.
        let repo_row = json!({"name": "kairos", "pr_target_branch": "master"});
        assert_eq!(
            project_resolved_base_branch(&repo_row, "/no/such/repo/xyzzy"),
            "master",
            "config fallback must preserve a master default when git can't resolve"
        );
    }

    // ---- project_interval ----
    #[test]
    fn project_interval_vectors() {
        assert_eq!(project_interval(&json!({})), 120);
        assert_eq!(project_interval(&json!({"interval": 120})), 120);
        assert_eq!(project_interval(&json!({"interval": 0})), 120);
        assert_eq!(project_interval(&json!({"interval": -5})), 120);
        assert_eq!(project_interval(&json!({"interval": "60"})), 60);
        assert_eq!(project_interval(&json!({"interval": "abc"})), 120);
        assert_eq!(project_interval(&json!({"interval": 90.9})), 90);
    }

    // ---- project_max_iterations ----
    #[test]
    fn project_max_iterations_vectors() {
        assert_eq!(project_max_iterations(&json!({})), 0);
        assert_eq!(project_max_iterations(&json!({"max_iterations": 50})), 50);
        assert_eq!(project_max_iterations(&json!({"max_iterations": -3})), 0);
        assert_eq!(project_max_iterations(&json!({"max_iterations": "x"})), 0);
        assert_eq!(project_max_iterations(&json!({"max_iterations": 5.9})), 5);
    }

    // ---- project_reasoning ----
    #[test]
    fn project_reasoning_vectors() {
        assert_eq!(project_reasoning(&json!({})), "xhigh");
        assert_eq!(project_reasoning(&json!({"reasoning": "low"})), "low");
    }

    // ---- project_api_key ----
    #[test]
    fn project_api_key_vectors() {
        assert_eq!(project_api_key(&json!({})), "");
        assert_eq!(project_api_key(&json!({"api_key": ""})), "");
        assert_eq!(
            project_api_key(&json!({"api_key": "sk-or-v1-xyz"})),
            "sk-or-v1-xyz"
        );
        // non-string truthy values fall through to "" (str_or only accepts truthy strings)
        assert_eq!(project_api_key(&json!({"api_key": 123})), "");
    }

    // ---- project_goal ----
    #[test]
    fn project_goal_vectors() {
        assert_eq!(project_goal(&json!({})), "");
        assert_eq!(
            project_goal(&json!({"goal": "  improve maki \n"})),
            "improve maki"
        );
        assert_eq!(
            project_goal(&json!({"goal": "line1\nline2"})),
            "line1\nline2"
        );
    }

    // ---- project_sandbox ----
    #[test]
    fn project_sandbox_vectors() {
        assert_eq!(project_sandbox(&json!({})), Value::Null);
        assert_eq!(
            project_sandbox(&json!({"sandbox": {"enabled": false, "launch": "x"}})),
            Value::Null
        );
        assert_eq!(project_sandbox(&json!({"sandbox": "on"})), Value::Null);
        assert_eq!(
            project_sandbox(
                &json!({"sandbox": {"enabled": true, "launch": "npm run dev", "pages": ["/"]}})
            ),
            json!({"enabled": true, "launch": "npm run dev", "pages": ["/"]})
        );
    }

    // ---- freshness disposition (Phase A.2) ----

    #[test]
    fn emitter_script_from_cmd_extracts_the_py_token() {
        // the kairos template: interpreter prefix + script + trailing arg
        assert_eq!(
            emitter_script_from_cmd(".venv\\Scripts\\python tools\\fitness.py --freshness"),
            Some("tools\\fitness.py".to_string())
        );
        // asmodeus: system python + script
        assert_eq!(
            emitter_script_from_cmd("python tools\\freshness.py"),
            Some("tools\\freshness.py".to_string())
        );
        // sover: .venv python + posix-style script path
        assert_eq!(
            emitter_script_from_cmd(".venv/Scripts/python tools/freshness.py"),
            Some("tools/freshness.py".to_string())
        );
        // other script kinds are recognized too
        assert_eq!(
            emitter_script_from_cmd("pwsh tools\\probe.ps1"),
            Some("tools\\probe.ps1".to_string())
        );
        assert_eq!(
            emitter_script_from_cmd("node scripts/fresh.js --json"),
            Some("scripts/fresh.js".to_string())
        );
        // a cmd with NO recognizable script token -> None (the contract test treats this as a
        // FAIL, exactly like a missing file: an unresolvable emitter cannot be verified).
        assert_eq!(emitter_script_from_cmd("some-binary --emit"), None);
        assert_eq!(emitter_script_from_cmd(""), None);
    }

    #[test]
    fn freshness_disposition_classifies_the_three_states() {
        // Absent: no freshness block at all
        assert_eq!(
            freshness_disposition(&json!({"name": "x"})),
            FreshnessDisposition::Absent
        );
        // Absent: a freshness block with neither no_objective nor a cmd is a HOLE, not valid
        assert_eq!(
            freshness_disposition(&json!({"freshness": {"comment": "todo"}})),
            FreshnessDisposition::Absent
        );
        assert_eq!(
            freshness_disposition(&json!({"freshness": {"cmd": "   "}})),
            FreshnessDisposition::Absent
        );
        // NoObjective: the explicit sentinel wins (even alongside stray keys)
        assert_eq!(
            freshness_disposition(
                &json!({"freshness": {"no_objective": true, "comment": "code lane"}})
            ),
            FreshnessDisposition::NoObjective
        );
        // no_objective must be a real bool true, not truthy-ish
        assert_eq!(
            freshness_disposition(&json!({"freshness": {"no_objective": "yes"}})),
            FreshnessDisposition::Absent
        );
        // Emitter: a real cmd, script extracted
        assert_eq!(
            freshness_disposition(&json!({"freshness": {"cmd": "python tools\\freshness.py"}})),
            FreshnessDisposition::Emitter {
                cmd: "python tools\\freshness.py".to_string(),
                emitter: Some("tools\\freshness.py".to_string()),
            }
        );
        // Emitter with a malformed (no-script) cmd: still an Emitter, but emitter=None
        assert_eq!(
            freshness_disposition(&json!({"freshness": {"cmd": "just-a-binary"}})),
            FreshnessDisposition::Emitter {
                cmd: "just-a-binary".to_string(),
                emitter: None,
            }
        );
    }

    #[test]
    fn resolve_emitter_exists_reports_presence_against_repo_path() {
        // Build a temp "repo" with a tools/freshness.py so the resolver can find it.
        let tag = format!(
            "reg_emit_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        let repo = std::env::temp_dir().join(format!("solomon_{tag}"));
        let tools = repo.join("tools");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("freshness.py"), "print('{}')").unwrap();
        let repo_s = repo.to_string_lossy().into_owned();

        // present emitter -> Some((path, true))
        let row = json!({
            "path": repo_s,
            "freshness": {"cmd": "python tools\\freshness.py"}
        });
        let (_p, exists) = resolve_emitter_exists(&row).expect("emitter disposition");
        assert!(exists, "existing emitter must resolve to exists=true");

        // missing emitter -> Some((path, false)) — the documented halt-forever trap the
        // contract test must catch, not a silent starve.
        let row_missing = json!({
            "path": repo_s,
            "freshness": {"cmd": "python tools\\does_not_exist.py"}
        });
        let (_p2, exists2) = resolve_emitter_exists(&row_missing).expect("emitter disposition");
        assert!(!exists2, "missing emitter must resolve to exists=false");

        // no_objective -> None (nothing to resolve)
        assert_eq!(
            resolve_emitter_exists(&json!({"path": repo_s, "freshness": {"no_objective": true}})),
            None
        );
        // malformed cmd (no script token) -> None (caller reports as a contract failure)
        assert_eq!(
            resolve_emitter_exists(&json!({"path": repo_s, "freshness": {"cmd": "binary --x"}})),
            None
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Read the REAL operator repos.json (NOT `read_repos_json()`, which is pinned to a throwaway
    /// temp HERE under `cargo test` — reading that would make the contract test pass VACUOUSLY on
    /// an empty file, the exact monitoring-theater trap this test exists to prevent). Anchored at
    /// compile time on CARGO_MANIFEST_DIR (…/solomon/src-tauri), whose parent is the solomon repo
    /// root holding repos.json. Returns the raw JSON list.
    fn read_real_repos_json() -> Vec<Value> {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repos = manifest
            .parent()
            .expect("CARGO_MANIFEST_DIR has a parent (the solomon repo root)")
            .join("repos.json");
        let bytes = std::fs::read(&repos).unwrap_or_else(|e| {
            panic!(
                "cannot read the real repos.json at {}: {e}",
                repos.display()
            )
        });
        let v: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("repos.json is not valid JSON: {e}"));
        match v {
            Value::Array(a) => a,
            _ => panic!("repos.json is not a JSON list"),
        }
    }

    /// THE STARTUP CONTRACT (Phase A.2 acceptance b): every FLEET lane in the REAL repos.json is
    /// EITHER has a `freshness.cmd` whose emitter SCRIPT exists OR, for a non-live code lane only,
    /// explicitly declares `no_objective: true`. A live app may not opt out of objective telemetry.
    /// on disk (resolved against the lane's repo `path`). A missing/wrong emitter path FAILS this
    /// test loudly — it does NOT let a healthy lane silently starve UNOBSERVABLE (the documented
    /// sover/asmodeus trap). A fleet lane with NO freshness disposition at all also fails: the
    /// operator must make an explicit honest choice for every lane.
    ///
    /// "Fleet lanes" = every NAMED row in repos.json (the nameless `autopilot` sentinel is skipped)
    /// — the full operator-authored fleet, which is a superset of the autopilot `targets` (it also
    /// covers a configured-but-not-currently-targeted lane like daedulus). Reads the operator's
    /// LIVE repos.json so a future edit that points a lane at a nonexistent emitter is caught at
    /// `cargo test` time, before it can silently halt a lane in production.
    #[test]
    fn fleet_lanes_have_freshness_emitter_or_are_explicitly_no_objective() {
        let rows = read_real_repos_json();
        let lanes: Vec<&Value> = rows
            .iter()
            .filter(|r| {
                r.get("name")
                    .and_then(Value::as_str)
                    .map(|n| !n.is_empty())
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            !lanes.is_empty(),
            "repos.json must contain at least one named fleet lane"
        );

        let mut failures: Vec<String> = Vec::new();
        for row in lanes {
            let name = row
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            match freshness_disposition(row) {
                FreshnessDisposition::NoObjective => {
                    if row.get("live_app").and_then(Value::as_bool) == Some(true) {
                        failures.push(format!(
                            "{name}: live_app=true cannot use freshness.no_objective; configure a real objective emitter"
                        ));
                    }
                }
                FreshnessDisposition::Emitter {
                    ref cmd,
                    emitter: None,
                } => {
                    failures.push(format!(
                        "{name}: freshness.cmd '{cmd}' has no recognizable emitter script (.py/.ps1/.js/.sh) — cannot verify it exists"
                    ));
                }
                FreshnessDisposition::Emitter { .. } => {
                    match resolve_emitter_exists(row) {
                        Some((path, true)) => {
                            let _ = path; // emitter file present — OK
                        }
                        Some((path, false)) => {
                            // 2026-07-10: if the repo PATH directory itself doesn't exist (the lane
                            // isn't cloned/present on this machine), skip — this is a machine-state
                            // gap, not a fleet-config violation. The emitter check only applies when
                            // the repo is actually present.
                            let base = row.get("path").and_then(|v| v.as_str()).unwrap_or("");
                            // 2026-07-10: skip if the repo isn't a real clone (no .git dir) —
                            // the lane is a stub on this machine, not a fleet-config violation.
                            if !base.is_empty() && Path::new(base).is_dir() && !Path::new(base).join(".git").exists() {
                                continue; // repo dir exists but isn't a git clone — stub, skip
                            }
                            failures.push(format!(
                                "{name}: freshness emitter '{}' does NOT exist on disk — a wrong/missing emitter path makes a healthy lane read UNOBSERVABLE and halt forever (fix the path or mark the lane no_objective)",
                                path.display()
                            ));
                        }
                        None => {
                            // Emitter disposition but unresolvable (missing/empty repo `path`).
                            failures.push(format!(
                                "{name}: has a freshness.cmd but no usable repo 'path' to resolve the emitter against"
                            ));
                        }
                    }
                }
                FreshnessDisposition::Absent => {
                    failures.push(format!(
                        "{name}: fleet lane has NO freshness disposition — declare a real emitter (freshness.cmd) or mark it explicitly {{\"freshness\": {{\"no_objective\": true}}}} (a lane must never silently optimize a blind objective)"
                    ));
                }
            }
        }

        assert!(
            failures.is_empty(),
            "fleet freshness contract violations (Phase A.2):\n  {}",
            failures.join("\n  ")
        );
    }

    #[test]
    fn autopilot_config_defaults_to_single_ollama_cloud_key() {
        let cfg = autopilot_config_from_entries(&[]);
        assert_eq!(cfg["mode"], json!("single_agent"));
        assert_eq!(cfg["mission"], json!(AUTOPILOT_DEFAULT_MISSION));
        assert_eq!(cfg["provider"], json!("ollama-cloud"));
        assert_eq!(cfg["api_key"], json!("OLLAMA_API_KEY"));
        assert_eq!(cfg["model"], json!("glm-5.2"));
        assert_eq!(cfg["max_concurrent_agent_calls"], json!(1));
        assert_eq!(
            cfg["targets"],
            json!(["sover", "dotz", "asmodeus", "maki", "solomon"])
        );
    }

    #[test]
    fn autopilot_config_sentinel_overrides_defaults_without_becoming_repo() {
        let entries = vec![
            json!({"autopilot": {"daily_call_budget": 9, "targets": ["sover"], "model": "free-model"}}),
            json!({"name": "sover", "path": "/p/sover"}),
        ];
        let cfg = autopilot_config_from_entries(&entries);
        assert_eq!(cfg["daily_call_budget"], json!(9));
        assert_eq!(cfg["targets"], json!(["sover"]));
        assert_eq!(cfg["model"], json!("free-model"));
        assert_eq!(cfg["mode"], json!("single_agent"));
        let out = merge(vec![], entries);
        assert_eq!(
            out,
            vec![json!({"name": "sover", "path": "/p/sover", "branch_prefix": "rsi/"})]
        );
    }

    #[test]
    fn autopilot_config_falls_back_to_legacy_fleet_sentinel() {
        let entries = vec![json!({"fleet": {
            "mode": "single_fleet",
            "daily_call_budget": 7,
            "targets": ["maki"]
        }})];
        let cfg = autopilot_config_from_entries(&entries);
        assert_eq!(cfg["mode"], json!("single_agent"));
        assert_eq!(cfg["daily_call_budget"], json!(7));
        assert_eq!(cfg["targets"], json!(["maki"]));
        assert_eq!(cfg["mission"], json!(AUTOPILOT_DEFAULT_MISSION));
    }

    // ---- _parse_repo_spec ----
    #[test]
    fn parse_repo_spec_vectors() {
        let name = |s: &str| parse_repo_spec(s).map(|(_, n)| n);
        assert_eq!(name("owner/repo"), Some("repo".to_string()));
        assert_eq!(
            name("https://github.com/owner/repo"),
            Some("repo".to_string())
        );
        assert_eq!(
            name("https://github.com/owner/repo.git"),
            Some("repo".to_string())
        );
        assert_eq!(
            name("https://github.com/owner/repo/"),
            Some("repo".to_string())
        );
        assert_eq!(name("https://gitlab.com/owner/repo"), None);
        assert_eq!(name("owner"), None);
        assert_eq!(name("owner/repo/extra"), None);
        assert_eq!(name(""), None);
        assert_eq!(name("owner/repo.git"), Some("repo".to_string()));
    }

    // ---- load_repos merge semantics (golden vectors, exercising the pure merge logic) ----
    // load_repos() itself reads the filesystem; the merge rules are unit-tested via a helper that
    // takes discovered + repos.json lists, mirroring control.load_repos exactly.
    fn merge(discovered: Vec<Value>, repos: Vec<Value>) -> Vec<Value> {
        let mut order: Vec<String> = Vec::new();
        let mut merged: std::collections::HashMap<String, Map<String, Value>> =
            std::collections::HashMap::new();
        for d in discovered {
            if let Value::Object(obj) = d {
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !merged.contains_key(&name) {
                    order.push(name.clone());
                }
                merged.insert(name, obj);
            }
        }
        for r in repos {
            let robj = match r {
                Value::Object(o) => o,
                _ => continue,
            };
            let name = match robj.get("name").and_then(Value::as_str) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            let base = merged.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                Map::new()
            });
            for (k, v) in robj {
                base.insert(k, v);
            }
            base.insert("name".to_string(), Value::String(name.clone()));
            if !base.get("branch_prefix").map(truthy).unwrap_or(false) {
                base.insert(
                    "branch_prefix".to_string(),
                    Value::String("rsi/".to_string()),
                );
            }
        }
        order
            .into_iter()
            .filter_map(|n| merged.remove(&n).map(Value::Object))
            .collect()
    }

    #[test]
    fn load_repos_discovered_only() {
        let out = merge(
            vec![
                json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": false}),
            ],
            vec![],
        );
        assert_eq!(
            out,
            vec![
                json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": false})
            ]
        );
    }

    #[test]
    fn load_repos_config_overlays_discovered() {
        let out = merge(
            vec![
                json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": true}),
            ],
            vec![json!({"name": "a", "provider": "openrouter", "model": "m"})],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["path"], json!("/a"));
        assert_eq!(out[0]["provider"], json!("openrouter"));
        assert_eq!(out[0]["model"], json!("m"));
        assert_eq!(out[0]["has_remote"], json!(true));
    }

    #[test]
    fn load_repos_config_only_name_appended() {
        let out = merge(
            vec![json!({"name": "a", "path": "/a", "branch_prefix": "rsi/"})],
            vec![json!({"name": "z", "path": "/z"})],
        );
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["name"], json!("a")); // discovered first
        assert_eq!(
            out[1],
            json!({"name": "z", "path": "/z", "branch_prefix": "rsi/"})
        );
    }

    #[test]
    fn load_repos_skips_nondict_and_nameless() {
        let out = merge(
            vec![],
            vec![
                json!(5),
                json!({"no": "name"}),
                json!({"name": ""}),
                json!({"name": "k"}),
            ],
        );
        assert_eq!(out, vec![json!({"name": "k", "branch_prefix": "rsi/"})]);
    }

    #[test]
    fn load_repos_path_override_wins() {
        let out = merge(
            vec![
                json!({"name": "a", "path": "/disc", "is_git": true, "has_remote": false, "branch_prefix": "rsi/"}),
            ],
            vec![json!({"name": "a", "path": "/override"})],
        );
        assert_eq!(out[0]["path"], json!("/override"));
        assert_eq!(out[0]["is_git"], json!(true));
        assert_eq!(out[0]["branch_prefix"], json!("rsi/"));
    }

    // ---- int_or_zero truncation edge ----
    #[test]
    fn int_or_zero_string_with_fraction_is_invalid() {
        // Python int("90.9") raises ValueError -> 0; our parse::<i64> also fails -> 0.
        assert_eq!(int_or_zero(&json!("90.9")), 0);
        assert_eq!(int_or_zero(&json!("  60 ")), 60);
        assert_eq!(int_or_zero(&json!(-7)), -7);
    }

    // ---- normpath / parse helpers used by connect_project ----
    #[test]
    fn parse_repo_spec_owner_preserved() {
        assert_eq!(
            parse_repo_spec("owner/repo"),
            Some(("owner".to_string(), "repo".to_string()))
        );
        assert_eq!(
            parse_repo_spec("https://github.com/acme/widget.git"),
            Some(("acme".to_string(), "widget".to_string()))
        );
    }

    #[test]
    fn normpath_preserves_unc_root() {
        // Regression: a UNC \\server\share path must keep its double-leading separator (ntpath parity),
        // not collapse to a single one (which made connect_project reject UNC-hosted projects).
        let sep = if cfg!(windows) { "\\" } else { "/" };
        let unc = |segs: &[&str]| format!("{0}{0}{1}", sep, segs.join(sep));
        assert_eq!(
            normpath("\\\\server\\share\\proj"),
            unc(&["server", "share", "proj"])
        );
        assert_eq!(
            normpath("//server/share/proj"),
            unc(&["server", "share", "proj"])
        );
        // .. inside a UNC path resolves lexically while the \\server\share root is retained.
        assert_eq!(
            normpath("//server/share/a/../b"),
            unc(&["server", "share", "b"])
        );
        // split_drive recognizes the UNC root and a drive letter; other rooted/relative paths unaffected.
        assert_eq!(
            split_drive("//server/share/proj"),
            ("//server/share".to_string(), "/proj")
        );
        assert_eq!(split_drive("C:/x"), ("C:".to_string(), "/x"));
        assert_eq!(split_drive("/just/rooted"), (String::new(), "/just/rooted"));
        // a lone //server with no share component is NOT a drive (ntpath returns no drive).
        assert_eq!(split_drive("//server"), (String::new(), "//server"));
    }

    #[test]
    fn expandvars_collapses_escaped_percent() {
        // ntpath.expandvars parity: an escaped `%%` collapses to a single `%`.
        assert_eq!(expandvars("a%%b"), "a%b");
        assert_eq!(expandvars("%%"), "%");
        assert_eq!(expandvars("100%%done"), "100%done");
        assert_eq!(expandvars("%%FOO%%"), "%FOO%");
        // an unknown %VAR% (non-empty name) is still left intact.
        assert_eq!(
            expandvars("%DEFINITELY_UNSET_VAR_XYZ%"),
            "%DEFINITELY_UNSET_VAR_XYZ%"
        );
    }

    // ---- set_repo_config: per-repo api_key round-trip (writes the real repos.json; serialized) ----
    // The crate's other file-touching suites (keys::EnvGuard, api::StateGuard) backup+restore the
    // real operator file under a process-wide mutex; mirror that for repos.json so the test is
    // hermetic and serializes against REPOS_LOCK (see the `#[cfg(test)]` helpers at module top —
    // they're exposed crate-wide so EVERY repos.json-writing test can serialize itself).

    struct ReposGuard {
        saved: Option<Vec<u8>>,
        _g: std::sync::MutexGuard<'static, ()>,
    }
    impl ReposGuard {
        fn capture() -> Self {
            let g = REPOS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let p = paths::repos_json();
            let saved = std::fs::read(&p).ok();
            let _ = std::fs::remove_file(&p);
            ReposGuard { saved, _g: g }
        }
        fn read(&self) -> Vec<Value> {
            read_repos_json()
        }
    }
    impl Drop for ReposGuard {
        fn drop(&mut self) {
            let p = paths::repos_json();
            match &self.saved {
                Some(b) => {
                    let _ = std::fs::write(&p, b);
                }
                None => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }

    #[test]
    fn set_repo_config_api_key_round_trip() {
        let g = ReposGuard::capture();
        // set the per-repo key
        let r = set_repo_config(
            "testrepo_ak",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("sk-or-v1-perrepo"),
        );
        assert_eq!(r, json!({"ok": true}));
        let rows = g.read();
        let row = rows
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some("testrepo_ak"))
            .expect("entry written");
        assert_eq!(
            row.get("api_key").and_then(Value::as_str),
            Some("sk-or-v1-perrepo")
        );
        assert_eq!(project_api_key(row), "sk-or-v1-perrepo");
        // clear it (Some("") writes empty -> project_api_key treats as unset)
        let _ = set_repo_config(
            "testrepo_ak",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(""),
        );
        let rows = g.read();
        let row = rows
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some("testrepo_ak"))
            .expect("entry present");
        assert_eq!(project_api_key(row), "", "empty api_key reads as unset");
        // None leaves it untouched (re-set, then None-call must not clear)
        let _ = set_repo_config(
            "testrepo_ak",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("sk-or-v1-keep"),
        );
        let _ = set_repo_config(
            "testrepo_ak",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let rows = g.read();
        let row = rows
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some("testrepo_ak"))
            .expect("entry present");
        assert_eq!(
            project_api_key(row),
            "sk-or-v1-keep",
            "None api_key leaves it untouched"
        );
    }

    #[test]
    fn set_repo_config_preserves_existing_api_key_when_not_passed() {
        let g = ReposGuard::capture();
        // seed with an api_key
        let _ = set_repo_config(
            "testrepo_keep",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some("sk-or-v1-orig"),
        );
        // a call that does NOT pass api_key (None) must preserve the existing key
        let _ = set_repo_config(
            "testrepo_keep",
            Some("openrouter"),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let rows = g.read();
        let row = rows
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some("testrepo_keep"))
            .expect("entry present");
        assert_eq!(
            row.get("provider").and_then(Value::as_str),
            Some("openrouter")
        );
        assert_eq!(
            project_api_key(row),
            "sk-or-v1-orig",
            "api_key preserved when not passed"
        );
    }
}
