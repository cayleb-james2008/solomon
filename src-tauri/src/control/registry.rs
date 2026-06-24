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
        None,
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
    match read_repos_json_strict() {
        Ok(v) => v,
        Err(_) => Vec::new(),
    }
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
            base.insert("branch_prefix".to_string(), Value::String("rsi/".to_string()));
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

/// control.project_goal: the north-star goal, stripped, or "" (unset).
pub fn project_goal(repo: &Value) -> String {
    let v = get(repo, "goal");
    let s = if truthy(v) { v.as_str().unwrap_or("") } else { "" };
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
// set_repo_config
// --------------------------------------------------------------------------- #

/// control.set_repo_config: upsert the repos.json entry for `name`, setting any passed (Some) keys.
/// Read-modify-write the whole list; preserve untouched keys; atomic write. Creates the entry
/// (carrying its discovered path) when absent. `phases` Some(obj) full-replaces the per-phase map,
/// pruning empty per-phase dicts; an empty result removes the `phases` key entirely.
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
) -> Value {
    if name.is_empty() {
        return json!({"ok": false, "error": "name required"});
    }
    let mut entries = match read_repos_for_write() {
        Ok(e) => e,
        Err(error) => return json!({"ok": false, "error": error}),
    };

    // Find the existing entry index (first dict whose name matches).
    let idx = entries.iter().position(|r| {
        r.is_object() && r.get("name").and_then(Value::as_str) == Some(name)
    });

    let pos = match idx {
        Some(i) => i,
        None => {
            // New entry: name, branch_prefix, then [path] if discovered.
            let mut obj = Map::new();
            obj.insert("name".to_string(), Value::String(name.to_string()));
            obj.insert("branch_prefix".to_string(), Value::String("rsi/".to_string()));
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
        let entry = entries[pos].as_object_mut().expect("matched entry is an object");
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
    }

    match write_repo_entries(&entries) {
        Ok(()) => json!({"ok": true}),
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
        s = s.splitn(2, "github.com/").nth(1).unwrap_or("").to_string();
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
    let r = match proc::run(
        &[gh_s.as_str(), "repo", "clone", spec.trim(), dest_s.as_str()],
        Some(&projects),
        None,
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
        let nm = cloned.get("name").and_then(Value::as_str).unwrap_or("").to_string();
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
    entry.insert("branch_prefix".to_string(), Value::String("rsi/".to_string()));
    entry.insert("is_git".to_string(), Value::Bool(is_git));
    entry.insert("has_remote".to_string(), Value::Bool(has_remote));
    entry.insert(
        "ship".to_string(),
        Value::String(if ship.is_empty() { "pr".to_string() } else { ship.to_string() }),
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
                return format!("{}{}", home, rest);
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
                        Err(_) => out.push_str(&format!("${{{}}}", name)),
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
        format!("{}/{}", prefix, body)
    } else if prefix.is_empty() {
        if body.is_empty() { ".".to_string() } else { body }
    } else {
        format!("{}{}", prefix, body)
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
mod tests {
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
        assert_eq!(project_provider(&json!({"provider": "openrouter"})), "openrouter");
    }

    // ---- project_model ----
    #[test]
    fn project_model_vectors() {
        assert_eq!(
            project_model(&json!({"model": "kimi-k2.7-code", "provider": "ollama-cloud"})),
            "kimi-k2.7-code"
        );
        assert_eq!(project_model(&json!({"provider": "ollama-cloud"})), "glm-5.2");
        assert_eq!(project_model(&json!({"provider": "openrouter"})), "qwen/qwen3-coder");
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
        assert_eq!(project_gate(&json!({"gate": "make test"})), Some("make test".to_string()));
    }

    // ---- project_pr_target_branch ----
    #[test]
    fn project_pr_target_branch_vectors() {
        assert_eq!(project_pr_target_branch(&json!({})), "main");
        assert_eq!(project_pr_target_branch(&json!({"pr_target_branch": "develop"})), "develop");
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

    // ---- project_goal ----
    #[test]
    fn project_goal_vectors() {
        assert_eq!(project_goal(&json!({})), "");
        assert_eq!(project_goal(&json!({"goal": "  improve maki \n"})), "improve maki");
        assert_eq!(project_goal(&json!({"goal": "line1\nline2"})), "line1\nline2");
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
            project_sandbox(&json!({"sandbox": {"enabled": true, "launch": "npm run dev", "pages": ["/"]}})),
            json!({"enabled": true, "launch": "npm run dev", "pages": ["/"]})
        );
    }

    // ---- _parse_repo_spec ----
    #[test]
    fn parse_repo_spec_vectors() {
        let name = |s: &str| parse_repo_spec(s).map(|(_, n)| n);
        assert_eq!(name("owner/repo"), Some("repo".to_string()));
        assert_eq!(name("https://github.com/owner/repo"), Some("repo".to_string()));
        assert_eq!(name("https://github.com/owner/repo.git"), Some("repo".to_string()));
        assert_eq!(name("https://github.com/owner/repo/"), Some("repo".to_string()));
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
                let name = obj.get("name").and_then(Value::as_str).unwrap_or("").to_string();
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
                base.insert("branch_prefix".to_string(), Value::String("rsi/".to_string()));
            }
        }
        order.into_iter().filter_map(|n| merged.remove(&n).map(Value::Object)).collect()
    }

    #[test]
    fn load_repos_discovered_only() {
        let out = merge(
            vec![json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": false})],
            vec![],
        );
        assert_eq!(
            out,
            vec![json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": false})]
        );
    }

    #[test]
    fn load_repos_config_overlays_discovered() {
        let out = merge(
            vec![json!({"name": "a", "path": "/a", "branch_prefix": "rsi/", "is_git": true, "has_remote": true})],
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
        assert_eq!(out[1], json!({"name": "z", "path": "/z", "branch_prefix": "rsi/"}));
    }

    #[test]
    fn load_repos_skips_nondict_and_nameless() {
        let out = merge(
            vec![],
            vec![json!(5), json!({"no": "name"}), json!({"name": ""}), json!({"name": "k"})],
        );
        assert_eq!(out, vec![json!({"name": "k", "branch_prefix": "rsi/"})]);
    }

    #[test]
    fn load_repos_path_override_wins() {
        let out = merge(
            vec![json!({"name": "a", "path": "/disc", "is_git": true, "has_remote": false, "branch_prefix": "rsi/"})],
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
        assert_eq!(parse_repo_spec("owner/repo"), Some(("owner".to_string(), "repo".to_string())));
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
        assert_eq!(normpath("\\\\server\\share\\proj"), unc(&["server", "share", "proj"]));
        assert_eq!(normpath("//server/share/proj"), unc(&["server", "share", "proj"]));
        // .. inside a UNC path resolves lexically while the \\server\share root is retained.
        assert_eq!(normpath("//server/share/a/../b"), unc(&["server", "share", "b"]));
        // split_drive recognizes the UNC root and a drive letter; other rooted/relative paths unaffected.
        assert_eq!(split_drive("//server/share/proj"), ("//server/share".to_string(), "/proj"));
        assert_eq!(split_drive("C:/x"), ("C:".to_string(), "/x"));
        assert_eq!(split_drive("/just/rooted"), (String::new(), "/just/rooted"));
        // a lone //server with no share component is NOT a drive (ntpath returns no drive).
        assert_eq!(split_drive("//server"), (String::new(), "//server"));
    }
}
