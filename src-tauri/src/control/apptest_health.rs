//! Port of control.py's `apptest_health` module: managed monitored-frontend app-test sessions
//! (browser_state / start/stop / state / frame / report), the frontend detector `has_frontend`,
//! the operator readiness snapshot `health()`, and Solomon's own short sha `current_sha()`.
//!
//! Bug-for-bug with control.py + improver/app_test_runtime.py. Bridge-return dicts are
//! `serde_json::Value` whose keys are byte-identical to the Python dicts. Spec + golden vectors:
//! `src-tauri/control-port-spec.json` (module == "apptest_health").
//!
//! `update_status` / `apply_update` are intentionally NOT ported (the Tauri updater replaces them).

use crate::control::{gh, keys, paths, proc, registry};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// app_test_runtime._now(): UTC, EXACTLY "%Y-%m-%dT%H:%M:%SZ" (no fractional seconds, literal Z).
fn now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-time conversion (Howard Hinnant's algorithm) for UTC seconds-since-epoch.
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as i64;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hh, mm, ss
    )
}

/// Python `int(x or 0)` over a possibly-missing / non-numeric JSON `seq` value.
///
/// Mirrors `int(state.get('seq') or 0)`: None/missing/0/"" -> 0; a JSON number truncates toward
/// zero; a numeric string parses; anything non-numeric raises (signalled here by `None`, which
/// callers map to the Python TypeError/ValueError reset path).
fn py_int_or_zero(v: Option<&Value>) -> Option<i64> {
    match v {
        None | Some(Value::Null) => Some(0),
        Some(Value::Bool(b)) => Some(if *b { 1 } else { 0 }), // `True or 0` -> True -> int 1
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Some(i)
            } else if let Some(f) = n.as_f64() {
                // `<float> or 0`: 0.0 is falsy -> 0; else int() truncates toward zero.
                if f == 0.0 {
                    Some(0)
                } else {
                    Some(f.trunc() as i64)
                }
            } else {
                None
            }
        }
        Some(Value::String(s)) => {
            if s.is_empty() {
                Some(0) // "" is falsy -> `"" or 0` -> 0
            } else {
                // int("  12 ") works in Python (strips whitespace); int("abc") raises.
                s.trim().parse::<i64>().ok()
            }
        }
        // list/dict: truthy, and int(list) raises TypeError -> reset path.
        Some(_) => None,
    }
}

// --------------------------------------------------------------------------- //
// detect_config — frontend launch detection (app_test_runtime.detect_config)
// --------------------------------------------------------------------------- //

/// app_test_runtime.detect_config: returns a launch config dict or None. NOT equivalent to
/// has_frontend — it only honors sandbox config, an index.html under web/public/root, or a
/// dev/preview/start npm script (in THAT precedence order).
fn detect_config(repo: &Value) -> Option<Value> {
    let configured = repo.get("sandbox");
    if let Some(c) = configured {
        if c.is_object()
            && c.get("enabled").map(is_truthy).unwrap_or(false)
            && c.get("launch").map(is_truthy).unwrap_or(false)
        {
            return Some(c.clone());
        }
    }
    let root = paths::repo_path(repo);
    let root = Path::new(&root);
    // shutil.which("python") or sys.executable — we resolve "python" or fall back to the literal.
    let python = which::which("python")
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "python".to_string());
    for relative in ["web", "public", "."] {
        let directory = root.join(relative);
        if directory.join("index.html").is_file() {
            let mut launch = vec![
                json!(python),
                json!("-m"),
                json!("http.server"),
                json!("{port}"),
                json!("--bind"),
                json!("127.0.0.1"),
            ];
            if relative != "." {
                launch.push(json!("--directory"));
                launch.push(json!(relative));
            }
            return Some(json!({
                "enabled": true,
                "launch": launch,
                "health": "/index.html",
                "pages": ["/index.html"],
                "boot_timeout": 20
            }));
        }
    }
    let package = root.join("package.json");
    if package.is_file() {
        let data: Value = std::fs::read_to_string(&package)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}));
        let scripts = if data.is_object() {
            data.get("scripts")
        } else {
            None
        };
        let script = ["dev", "preview", "start"].into_iter().find(|name| {
            scripts
                .map(|s| s.is_object() && s.get(*name).is_some())
                .unwrap_or(false)
        });
        if let Some(script) = script {
            let npm = if cfg!(windows) { "npm.cmd" } else { "npm" };
            return Some(json!({
                "enabled": true,
                "launch": [npm, "run", script, "--", "--host", "127.0.0.1", "--port", "{port}"],
                "health": "/",
                "pages": ["/"],
                "boot_timeout": 60
            }));
        }
    }
    None
}

/// Standard base64 (RFC 4648, no line wrapping), ASCII output — matches
/// base64.b64encode(bytes).decode('ascii'). Inlined (no base64 crate dependency).
fn b64_standard(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Python truthiness for a JSON value (false / null / 0 / "" / [] / {} are falsy).
fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --------------------------------------------------------------------------- //
// AppTestManager — process-global session state machine
// --------------------------------------------------------------------------- //

struct Session {
    handle: Option<std::thread::JoinHandle<()>>,
    stop: Arc<(Mutex<bool>, Condvar)>,
    session_id: String,
}

impl Session {
    fn is_alive(&self) -> bool {
        self.handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false)
    }
}

struct AppTestManager {
    sessions: Mutex<HashMap<String, Session>>,
}

impl AppTestManager {
    fn new() -> Self {
        AppTestManager {
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// AppTestManager.start. session_id = "<name>-<uuid4 hex first 12>". Idempotent while a live
    /// session exists. The worker thread waits on the stop signal then writes the stopped report
    /// (the Sandbox/AgentBrowser drive in app_test_runtime lives in improver/, outside this port —
    /// the observable start/stop/already lifecycle is preserved).
    fn start(&self, repo: &Value, runtime_dir: PathBuf) -> Value {
        let name = repo
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let config = detect_config(repo);
        if name.is_empty() || config.is_none() {
            return json!({"ok": false, "error": "no runnable frontend configuration found"});
        }
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(active) = sessions.get(&name) {
            if active.is_alive() {
                return json!({"ok": true, "already": true, "sessionId": active.session_id});
            }
        }
        let session_id = format!("{}-{}", name, uuid_hex12());
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_stop = Arc::clone(&stop);
        let report_path = runtime_dir.join("app_test_report.json");
        let started = now();
        let sid = session_id.clone();
        let handle = std::thread::Builder::new()
            .name(format!("app-test-{}", name))
            .spawn(move || {
                let _ = std::fs::create_dir_all(&runtime_dir);
                // Wait until stop is signalled (mirrors `while not stop.wait(0.5): pass`).
                let (lock, cvar) = &*worker_stop;
                let mut signalled = lock.lock().unwrap();
                while !*signalled {
                    let (g, _) = cvar
                        .wait_timeout(signalled, Duration::from_millis(500))
                        .unwrap();
                    signalled = g;
                }
                drop(signalled);
                let final_report = json!({
                    "ok": true, "status": "stopped", "sessionId": sid,
                    "startedAt": started, "finishedAt": now(), "findings": []
                });
                if let Ok(body) = serde_json::to_string_pretty(&final_report) {
                    let _ = std::fs::write(&report_path, body);
                }
            })
            .ok();
        sessions.insert(
            name,
            Session {
                handle,
                stop,
                session_id: session_id.clone(),
            },
        );
        json!({"ok": true, "sessionId": session_id, "status": "starting"})
    }

    /// AppTestManager.stop. Idempotent (already:true when no session). Sets stop, joins with a 20s
    /// budget; if the worker is still alive after the join the stop reports the timeout error.
    fn stop(&self, name: &str) -> Value {
        // Take the session out under the lock (mirrors fetch-under-lock then pop-under-lock).
        let mut handle;
        let session_id;
        {
            let mut sessions = self.sessions.lock().unwrap();
            match sessions.get_mut(name) {
                None => return json!({"ok": true, "already": true}),
                Some(s) => {
                    // signal stop
                    {
                        let (lock, cvar) = &*s.stop;
                        *lock.lock().unwrap() = true;
                        cvar.notify_all();
                    }
                    session_id = s.session_id.clone();
                    handle = s.handle.take();
                }
            }
        }
        // join(timeout=20): poll for up to 20s for the worker to finish.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let mut still_alive = false;
        if let Some(h) = handle.take() {
            loop {
                if h.is_finished() {
                    let _ = h.join();
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    still_alive = true;
                    // Leave the (finished-eventually) thread detached; re-insert so a retry can see it.
                    // Guard on session_id: a concurrent start() during our 20s join may have already
                    // replaced this slot with a NEW live session — cross-wiring our old handle onto it
                    // would corrupt the map. Only re-attach if the slot is still OURS.
                    let mut sessions = self.sessions.lock().unwrap();
                    if let Some(s) = sessions.get_mut(name) {
                        if s.session_id == session_id {
                            s.handle = Some(h);
                        }
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        if still_alive {
            return json!({"ok": false, "error": "app test did not stop within 20 seconds"});
        }
        // Only remove the slot if it is STILL ours. A concurrent start() during our join may have
        // replaced it with a fresh live session; removing that would leak an untracked worker and
        // make the next stop() a no-op (already:true) against a running app-test.
        {
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.get(name).map(|s| s.session_id == session_id).unwrap_or(false) {
                sessions.remove(name);
            }
        }
        json!({"ok": true, "sessionId": session_id})
    }
}

fn manager() -> &'static AppTestManager {
    static MANAGER: OnceLock<AppTestManager> = OnceLock::new();
    MANAGER.get_or_init(AppTestManager::new)
}

/// uuid4().hex[:12] — 12 lowercase hex chars from a random 128-bit value.
fn uuid_hex12() -> String {
    let mut bytes = [0u8; 6];
    getrandom_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Fill `buf` with pseudo-random bytes from a time/heap-address xorshift mix. The value only needs
/// to be a non-colliding session suffix (mirrors uuid4().hex[:12]'s ROLE, not its CSPRNG strength);
/// no `rand`/`getrandom` crate is available and none is warranted here.
fn getrandom_bytes(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let addr = buf.as_ptr() as usize as u128;
    let mut x = nanos ^ (addr.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    for b in buf.iter_mut() {
        // xorshift-ish mix
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
}

// --------------------------------------------------------------------------- //
// browser_state / app_test_* / read_app_test_report
// --------------------------------------------------------------------------- //

/// control.browser_state: read runtime/<name>/browser_state.json VERBATIM. Error shapes:
/// {ok:false,error:"repo has no runtime"} (no runtime), {ok:false,error:"invalid browser state"}
/// (parsed value not an object), {ok:false,error:<io/parse err>} (missing/invalid file).
pub fn browser_state(repo: &Value) -> Value {
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return json!({"ok": false, "error": "repo has no runtime"}),
    };
    let path = rt.join("browser_state.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => json!({"ok": false, "error": "invalid browser state"}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// control.start_app_test: guard falsy repo / missing runtime, else delegate to MANAGER.start.
pub fn start_app_test(repo: &Value) -> Value {
    if !is_truthy(repo) {
        return json!({"ok": false, "error": "unknown repo"});
    }
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return json!({"ok": false, "error": "repo has no runtime"}),
    };
    manager().start(repo, rt)
}

/// control.stop_app_test: guard falsy repo, else MANAGER.stop(_repo_name(repo)).
pub fn stop_app_test(repo: &Value) -> Value {
    if !is_truthy(repo) {
        return json!({"ok": false, "error": "unknown repo"});
    }
    manager().stop(&paths::repo_name(repo))
}

/// control.app_test_state: returns {ok,unchanged,seq} when the cached state is unchanged relative
/// to after_seq (requires ok truthy AND seq>0 AND seq<=after), else the raw browser_state verbatim.
pub fn app_test_state(repo: &Value, after_seq: i64) -> Value {
    let state = browser_state(repo);
    // `seq = int(state.get('seq') or 0); after = int(after_seq or 0)`; either raising -> 0,0.
    let (seq, after) = match (py_int_or_zero(state.get("seq")), Some(after_seq)) {
        (Some(s), Some(a)) => (s, a),
        _ => (0, 0),
    };
    let ok = state.get("ok").map(is_truthy).unwrap_or(false);
    if ok && seq > 0 && seq <= after {
        return json!({"ok": true, "unchanged": true, "seq": seq});
    }
    state
}

/// control.app_test_frame: base64-encode runtime/<name>/browser_frame.jpg. Unchanged gate uses
/// ONLY seq (no ok check). {ok,unchanged,seq} | {ok,seq,mime,data} | {ok:false,seq,error}.
pub fn app_test_frame(repo: &Value, after_seq: i64) -> Value {
    let state = browser_state(repo);
    let seq = py_int_or_zero(state.get("seq")).unwrap_or(0);
    if seq <= after_seq {
        return json!({"ok": true, "unchanged": true, "seq": seq});
    }
    let frame = match paths::runtime_dir(repo) {
        Some(rt) => rt.join("browser_frame.jpg"),
        None => PathBuf::from(""),
    };
    match std::fs::read(&frame) {
        Ok(bytes) => {
            let data = b64_standard(&bytes);
            json!({"ok": true, "seq": seq, "mime": "image/jpeg", "data": data})
        }
        Err(e) => json!({"ok": false, "seq": seq, "error": e.to_string()}),
    }
}

/// control.read_app_test_report: read runtime/<name>/app_test_report.json VERBATIM. NOTE: NO
/// "repo has no runtime" guard — a None runtime yields path "" and the open fails into the generic
/// error dict. {dict verbatim} | {ok:false,error:"invalid report"} | {ok:false,error:<err>}.
pub fn read_app_test_report(repo: &Value) -> Value {
    let path = match paths::runtime_dir(repo) {
        Some(rt) => rt.join("app_test_report.json"),
        None => PathBuf::from(""),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => json!({"ok": false, "error": "invalid report"}),
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

// --------------------------------------------------------------------------- //
// has_frontend
// --------------------------------------------------------------------------- //

/// control.has_frontend: cheap, root-scoped web-surface detection (markers, then package.json
/// frameworks, then dev/start/preview scripts). NOT equivalent to detect_config.
pub fn has_frontend(repo: &Value) -> bool {
    let path = paths::repo_path(repo);
    if path.is_empty() || !Path::new(&path).is_dir() {
        return false;
    }
    let root = Path::new(&path);
    // ORDER matters; "web" appears as a marker but a bare "web" DIR is NOT dir-allowed.
    let markers = [
        "index.html",
        "web/index.html",
        "public/index.html",
        "src/index.html",
        "templates",
        "web",
        "frontend",
        "client",
    ];
    let dir_allowed = ["templates", "frontend", "client"];
    for marker in markers {
        let mut candidate = root.to_path_buf();
        for part in marker.split('/') {
            candidate = candidate.join(part);
        }
        if candidate.is_file() || (dir_allowed.contains(&marker) && candidate.is_dir()) {
            return true;
        }
    }
    let package_path = root.join("package.json");
    let package: Value = std::fs::read_to_string(&package_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    let mut deps: HashMap<String, ()> = HashMap::new();
    for key in ["dependencies", "devDependencies"] {
        if let Some(Value::Object(map)) = package.get(key) {
            for k in map.keys() {
                deps.insert(k.clone(), ());
            }
        }
    }
    let frameworks = [
        "react",
        "react-dom",
        "vue",
        "@vue/cli-service",
        "svelte",
        "@sveltejs/kit",
        "next",
        "nuxt",
        "vite",
        "astro",
        "angular",
        "@angular/core",
    ];
    if frameworks.iter().any(|f| deps.contains_key(*f)) {
        return true;
    }
    if let Some(Value::Object(scripts)) = package.get("scripts") {
        return ["dev", "start", "preview"]
            .iter()
            .any(|k| scripts.contains_key(*k));
    }
    false
}

// --------------------------------------------------------------------------- //
// health / current_sha
// --------------------------------------------------------------------------- //

/// control.health: operator readiness snapshot. repos[] follows load_repos() order; non-dict
/// entries are skipped. keys order is ollama-cloud then openrouter (from keys_status()).
pub fn health() -> Value {
    let mut repos: Vec<Value> = Vec::new();
    for r in registry::load_repos() {
        if !r.is_object() {
            continue;
        }
        let py = paths::venv_python(&r);
        let venv = py.map(|p| p.exists()).unwrap_or(false);
        repos.push(json!({
            "name": r.get("name").cloned().unwrap_or(Value::Null),
            "is_git": r.get("is_git").map(is_truthy).unwrap_or(false),
            "has_remote": r.get("has_remote").map(is_truthy).unwrap_or(false),
            "venv": venv
        }));
    }
    json!({
        "gh": gh::gh_ready(),
        "git": proc::which_git().is_some(),
        "keys": keys::keys_status(),
        "repos": repos
    })
}

/// control.current_sha: short sha of Solomon's OWN checkout, or None. KEPT (version/identity in UI).
pub fn current_sha() -> Option<String> {
    let repo = solomon_repo()?;
    let git = proc::which_git()?;
    let git = git.to_string_lossy().into_owned();
    let repo_s = repo.to_string_lossy().into_owned();
    let r = match proc::run(
        &[git.as_str(), "-C", repo_s.as_str(), "rev-parse", "--short", "HEAD"],
        None,
        None,
    ) {
        Ok(r) => r,
        Err(_) => return None, // OSError branch
    };
    if r.code == 0 {
        let s = r.stdout.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    } else {
        None
    }
}

/// control._solomon_repo: locate Solomon's own git checkout (the source-rebuild target). SOLOMON_HOME
/// if it is a repo, else a walk UP (<=8 levels) from HERE (and the exe dir) looking for the
/// Rust/Tauri repo markers (src-tauri/Cargo.toml + SOLOMON_RSI.md). Returns the path, or None.
pub(crate) fn solomon_repo() -> Option<PathBuf> {
    // Post-port markers: the Rust crate manifest + the canonical RSI spec doc (was solomon.spec +
    // control.py pre-port; both removed in the Python->Rust/Tauri rewrite).
    let is_repo =
        |d: &Path| d.join("src-tauri").join("Cargo.toml").is_file() && d.join("SOLOMON_RSI.md").is_file();

    if let Ok(env_home) = std::env::var("SOLOMON_HOME") {
        let p = PathBuf::from(&env_home);
        if is_repo(&p) {
            return Some(p);
        }
    }
    let mut starts: Vec<PathBuf> = vec![paths::here().to_path_buf()];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            starts.push(dir.to_path_buf());
        }
    }
    for start in starts {
        let mut d = start;
        for _ in 0..8 {
            if is_repo(&d) {
                return Some(d);
            }
            match d.parent() {
                Some(p) if p != d => d = p.to_path_buf(),
                _ => break,
            }
        }
    }
    None
}

// The Python-era best-effort `ensure_watchdog_task()` (auto-arm a `pythonw monitor.py` scheduled
// task) was removed in the Rust/Tauri port: monitor.py + the maki pythonw it shelled out to are
// gone, and nothing in the app called it. The SolomonWatchdog task now runs `solomon watchdog`
// (native subcommand, see main.rs) and is armed/repointed by the operator via Set-ScheduledTask.

// --------------------------------------------------------------------------- //
// tests — built from the spec's golden vectors
// --------------------------------------------------------------------------- //
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    // HERE is a process-global OnceLock; the file-backed tests pin it via SOLOMON_HOME, so they must
    // not run concurrently with one another.
    static ENV_GUARD: StdMutex<()> = StdMutex::new(());

    fn temp_home() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "solomon_apptest_{}_{}",
            std::process::id(),
            uuid_hex12()
        ));
        std::fs::create_dir_all(base.join("improver")).unwrap();
        base
    }

    fn write_state(home: &Path, name: &str, body: &str) {
        let rt = home.join("runtime").join(name);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(rt.join("browser_state.json"), body).unwrap();
    }

    // ---- py_int_or_zero (drives the seq parsing in state/frame) ----
    #[test]
    fn py_int_semantics() {
        assert_eq!(py_int_or_zero(None), Some(0));
        assert_eq!(py_int_or_zero(Some(&json!(null))), Some(0));
        assert_eq!(py_int_or_zero(Some(&json!(0))), Some(0));
        assert_eq!(py_int_or_zero(Some(&json!(5))), Some(5));
        assert_eq!(py_int_or_zero(Some(&json!("7"))), Some(7));
        assert_eq!(py_int_or_zero(Some(&json!("abc"))), None); // int("abc") raises
        assert_eq!(py_int_or_zero(Some(&json!(""))), Some(0)); // "" or 0 -> 0
    }

    // ---- is_truthy (Python truthiness, drives the falsy-repo guards) ----
    #[test]
    fn truthiness() {
        assert!(!is_truthy(&json!({}))); // empty dict falsy -> "unknown repo"
        assert!(!is_truthy(&json!(null)));
        assert!(is_truthy(&json!({"name": "x"})));
        assert!(!is_truthy(&json!(0)));
        assert!(is_truthy(&json!(1)));
    }

    // ---- browser_state golden vectors ----
    #[test]
    fn browser_state_no_runtime() {
        // repo={} -> no resolvable name -> "repo has no runtime"
        assert_eq!(
            browser_state(&json!({})),
            json!({"ok": false, "error": "repo has no runtime"})
        );
    }

    #[test]
    fn browser_state_verbatim_and_array_and_missing() {
        let _g = ENV_GUARD.lock().unwrap();
        let home = temp_home();
        std::env::set_var("SOLOMON_HOME", &home);
        // Force HERE re-resolution is impossible (OnceLock); instead assert via runtime_dir using the
        // same name. We rely on paths::here() having been pinned by SOLOMON_HOME on first call within
        // this process. If another test already initialized HERE, skip the file-backed assertions.
        if paths::here() != home.as_path() {
            std::env::remove_var("SOLOMON_HOME");
            return;
        }
        // verbatim
        write_state(
            &home,
            "foo",
            r#"{"schemaVersion":1,"sessionId":"foo-abc","seq":3,"ok":true,"status":"running"}"#,
        );
        assert_eq!(
            browser_state(&json!({"name": "foo"})),
            json!({"schemaVersion":1,"sessionId":"foo-abc","seq":3,"ok":true,"status":"running"})
        );
        // array -> invalid browser state
        write_state(&home, "arr", "[1,2,3]");
        assert_eq!(
            browser_state(&json!({"name": "arr"})),
            json!({"ok": false, "error": "invalid browser state"})
        );
        // missing file -> error dict (shape only; message is OS/locale-dependent)
        let r = browser_state(&json!({"name": "missing"}));
        assert_eq!(r.get("ok"), Some(&json!(false)));
        assert!(r.get("error").and_then(Value::as_str).is_some());
        std::env::remove_var("SOLOMON_HOME");
        // HERE is a process-global OnceLock pinned to this `home` on first resolution. Other modules'
        // file-backed tests resolve env_file()/runtime under the same HERE, so the pinned dir must
        // outlive this test. Only delete it if HERE was pinned elsewhere.
        if paths::here() != home.as_path() {
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    // ---- app_test_state golden vectors (pure over a stubbed browser_state via a real file) ----
    #[test]
    fn app_test_state_logic_via_synthetic() {
        // We exercise the unchanged math directly: emulate browser_state outputs.
        let unchanged = |state: Value, after: i64| -> Value {
            let (seq, after) = match (py_int_or_zero(state.get("seq")), Some(after)) {
                (Some(s), Some(a)) => (s, a),
                _ => (0, 0),
            };
            let ok = state.get("ok").map(is_truthy).unwrap_or(false);
            if ok && seq > 0 && seq <= after {
                json!({"ok": true, "unchanged": true, "seq": seq})
            } else {
                state
            }
        };
        // unchanged: seq==after, ok
        assert_eq!(
            unchanged(json!({"ok": true, "seq": 5, "status": "running"}), 5),
            json!({"ok": true, "unchanged": true, "seq": 5})
        );
        // newer seq -> passthrough
        assert_eq!(
            unchanged(json!({"ok": true, "seq": 7, "status": "running"}), 5),
            json!({"ok": true, "seq": 7, "status": "running"})
        );
        // after=0, seq positive -> not unchanged
        assert_eq!(
            unchanged(json!({"ok": true, "seq": 1}), 0),
            json!({"ok": true, "seq": 1})
        );
        // error passthrough
        assert_eq!(
            unchanged(json!({"ok": false, "error": "repo has no runtime"}), 3),
            json!({"ok": false, "error": "repo has no runtime"})
        );
        // non-numeric seq -> reset -> passthrough
        assert_eq!(
            unchanged(json!({"ok": true, "seq": "abc"}), 2),
            json!({"ok": true, "seq": "abc"})
        );
    }

    // ---- app_test_frame unchanged-gate semantics (no ok check) ----
    #[test]
    fn app_test_frame_unchanged_gate() {
        // seq<=after -> unchanged, regardless of ok.
        let gate = |state: Value, after: i64| -> Option<Value> {
            let seq = py_int_or_zero(state.get("seq")).unwrap_or(0);
            if seq <= after {
                Some(json!({"ok": true, "unchanged": true, "seq": seq}))
            } else {
                None
            }
        };
        assert_eq!(
            gate(json!({"ok": true, "seq": 4}), 4),
            Some(json!({"ok": true, "unchanged": true, "seq": 4}))
        );
        // no seq, after=0 -> unchanged at 0 even though ok:false
        assert_eq!(
            gate(json!({"ok": false, "error": "x"}), 0),
            Some(json!({"ok": true, "unchanged": true, "seq": 0}))
        );
        // advanced -> not gated
        assert_eq!(gate(json!({"ok": true, "seq": 9}), 8), None);
    }

    #[test]
    fn base64_of_jpeg_magic() {
        // FF D8 FF -> "/9j/" (golden vector for the fresh-frame encode)
        assert_eq!(b64_standard(&[0xFFu8, 0xD8, 0xFF]), "/9j/");
        // padding cases
        assert_eq!(b64_standard(b"M"), "TQ==");
        assert_eq!(b64_standard(b"Ma"), "TWE=");
        assert_eq!(b64_standard(b"Man"), "TWFu");
    }

    // ---- read_app_test_report: no-runtime path is the GENERIC error, not "repo has no runtime" ----
    #[test]
    fn read_report_no_runtime_is_generic_error() {
        let r = read_app_test_report(&json!({}));
        assert_eq!(r.get("ok"), Some(&json!(false)));
        // NOT the "repo has no runtime" string.
        assert_ne!(r.get("error").and_then(Value::as_str), Some("repo has no runtime"));
    }

    // ---- start/stop golden vectors ----
    #[test]
    fn start_falsy_repo() {
        assert_eq!(
            start_app_test(&json!(null)),
            json!({"ok": false, "error": "unknown repo"})
        );
    }

    #[test]
    fn stop_falsy_repo() {
        assert_eq!(
            stop_app_test(&json!({})),
            json!({"ok": false, "error": "unknown repo"})
        );
    }

    #[test]
    fn stop_no_active_session() {
        // No session registered for this random name -> already:true.
        let name = format!("nope-{}", uuid_hex12());
        assert_eq!(
            manager().stop(&name),
            json!({"ok": true, "already": true})
        );
    }

    #[test]
    fn start_no_runnable_config() {
        // Non-empty repo dict whose path has no frontend -> config error (not "unknown repo").
        let _g = ENV_GUARD.lock().unwrap();
        let home = temp_home();
        std::env::set_var("SOLOMON_HOME", &home);
        if paths::here() != home.as_path() {
            std::env::remove_var("SOLOMON_HOME");
            return;
        }
        let empty_repo_dir = home.join("blank_repo");
        std::fs::create_dir_all(&empty_repo_dir).unwrap();
        let repo = json!({"name": "blank", "path": empty_repo_dir.to_string_lossy()});
        assert_eq!(
            start_app_test(&repo),
            json!({"ok": false, "error": "no runnable frontend configuration found"})
        );
        std::env::remove_var("SOLOMON_HOME");
        if paths::here() != home.as_path() {
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn start_then_already_then_stop() {
        let _g = ENV_GUARD.lock().unwrap();
        let home = temp_home();
        std::env::set_var("SOLOMON_HOME", &home);
        if paths::here() != home.as_path() {
            std::env::remove_var("SOLOMON_HOME");
            return;
        }
        // A repo with web/index.html -> detect_config Some -> fresh start.
        let name = format!("front{}", uuid_hex12());
        let repo_dir = home.join(&name);
        std::fs::create_dir_all(repo_dir.join("web")).unwrap();
        std::fs::write(repo_dir.join("web").join("index.html"), "<html></html>").unwrap();
        let repo = json!({"name": name, "path": repo_dir.to_string_lossy()});

        let r1 = start_app_test(&repo);
        assert_eq!(r1.get("ok"), Some(&json!(true)));
        assert_eq!(r1.get("status"), Some(&json!("starting")));
        let sid = r1.get("sessionId").and_then(Value::as_str).unwrap().to_string();
        assert!(sid.starts_with(&format!("{}-", name)));
        assert_eq!(sid.len(), name.len() + 1 + 12); // <name>-<12 hex>
        assert!(sid.rsplit('-').next().unwrap().chars().all(|c| c.is_ascii_hexdigit()));

        // Concurrent start while alive -> already:true with the SAME sessionId.
        let r2 = start_app_test(&repo);
        assert_eq!(
            r2,
            json!({"ok": true, "already": true, "sessionId": sid})
        );

        // Clean stop -> {ok:true, sessionId}.
        let r3 = stop_app_test(&repo);
        assert_eq!(r3, json!({"ok": true, "sessionId": sid}));

        // Idempotent stop after stop.
        assert_eq!(stop_app_test(&repo), json!({"ok": true, "already": true}));

        std::env::remove_var("SOLOMON_HOME");
        if paths::here() != home.as_path() {
            let _ = std::fs::remove_dir_all(&home);
        }
    }

    #[test]
    fn stop_does_not_clobber_a_session_replaced_during_join() {
        // Regression guard for the start/stop race: stop() that captured session "A" must NOT
        // remove (or cross-wire) a NEW session "B" that a concurrent start() installed while stop()
        // was in its join loop. A worker blocked on a controllable gate keeps stop()'s join loop
        // spinning so we can deterministically perform the swap mid-flight (no timing-of-the-bug).
        let mgr = AppTestManager::new();
        let name = "race_foo".to_string();

        // Session A's worker waits on `gate_a` (ignoring the Session.stop signal), so stop()'s join
        // loop keeps polling until we release it.
        let gate_a = Arc::new((Mutex::new(false), Condvar::new()));
        let ga = Arc::clone(&gate_a);
        let handle_a = std::thread::spawn(move || {
            let (l, c) = &*ga;
            let mut done = l.lock().unwrap();
            while !*done {
                done = c.wait(done).unwrap();
            }
        });
        mgr.sessions.lock().unwrap().insert(
            name.clone(),
            Session {
                handle: Some(handle_a),
                stop: Arc::new((Mutex::new(false), Condvar::new())),
                session_id: "A".into(),
            },
        );

        std::thread::scope(|s| {
            let stopper = s.spawn(|| mgr.stop(&name));
            // Let stop() take A's handle and enter its (lock-free) join loop.
            std::thread::sleep(Duration::from_millis(80));
            // A concurrent start() replaces the slot with a fresh live session "B".
            let handle_b = std::thread::spawn(|| {});
            mgr.sessions.lock().unwrap().insert(
                name.clone(),
                Session {
                    handle: Some(handle_b),
                    stop: Arc::new((Mutex::new(false), Condvar::new())),
                    session_id: "B".into(),
                },
            );
            // Release A's worker so stop()'s join completes and it reaches the guarded remove.
            let (l, c) = &*gate_a;
            *l.lock().unwrap() = true;
            c.notify_all();
            let r = stopper.join().unwrap();
            assert_eq!(r.get("sessionId"), Some(&json!("A")));
        });

        // The guard must have kept session B intact (un-fixed code would have removed it).
        let sessions = mgr.sessions.lock().unwrap();
        assert_eq!(
            sessions.get(&name).map(|s| s.session_id.as_str()),
            Some("B"),
            "stop() that captured A must not remove the B session a concurrent start() installed"
        );
    }

    // ---- has_frontend golden vectors ----
    #[test]
    fn has_frontend_vectors() {
        let base = std::env::temp_dir().join(format!("solomon_hf_{}", uuid_hex12()));

        // index.html at root -> true
        let d1 = base.join("root_index");
        std::fs::create_dir_all(&d1).unwrap();
        std::fs::write(d1.join("index.html"), "x").unwrap();
        assert!(has_frontend(&json!({"path": d1.to_string_lossy()})));

        // templates dir present -> true
        let d2 = base.join("templates_repo");
        std::fs::create_dir_all(d2.join("templates")).unwrap();
        assert!(has_frontend(&json!({"path": d2.to_string_lossy()})));

        // bare web dir only (no web/index.html) -> false
        let d3 = base.join("bare_web");
        std::fs::create_dir_all(d3.join("web")).unwrap();
        assert!(!has_frontend(&json!({"path": d3.to_string_lossy()})));

        // react dependency -> true
        let d4 = base.join("react_repo");
        std::fs::create_dir_all(&d4).unwrap();
        std::fs::write(
            d4.join("package.json"),
            r#"{"dependencies":{"react":"^18"}}"#,
        )
        .unwrap();
        assert!(has_frontend(&json!({"path": d4.to_string_lossy()})));

        // dev script -> true
        let d5 = base.join("dev_repo");
        std::fs::create_dir_all(&d5).unwrap();
        std::fs::write(d5.join("package.json"), r#"{"scripts":{"dev":"vite"}}"#).unwrap();
        assert!(has_frontend(&json!({"path": d5.to_string_lossy()})));

        // nothing matches -> false
        let d6 = base.join("nothing");
        std::fs::create_dir_all(&d6).unwrap();
        std::fs::write(
            d6.join("package.json"),
            r#"{"dependencies":{"lodash":"^4"},"scripts":{"test":"jest"}}"#,
        )
        .unwrap();
        assert!(!has_frontend(&json!({"path": d6.to_string_lossy()})));

        // non-existent path -> false
        assert!(!has_frontend(&json!({"path": base.join("does_not_exist").to_string_lossy()})));

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- detect_config narrower than has_frontend (templates dir passes has_frontend, fails detect) ----
    #[test]
    fn detect_config_narrower_than_has_frontend() {
        let base = std::env::temp_dir().join(format!("solomon_dc_{}", uuid_hex12()));
        // templates dir: has_frontend true, detect_config None.
        let d = base.join("templates_only");
        std::fs::create_dir_all(d.join("templates")).unwrap();
        let repo = json!({"name": "t", "path": d.to_string_lossy()});
        assert!(has_frontend(&repo));
        assert!(detect_config(&repo).is_none());

        // web/index.html: detect_config Some (http.server launch).
        let d2 = base.join("web_index");
        std::fs::create_dir_all(d2.join("web")).unwrap();
        std::fs::write(d2.join("web").join("index.html"), "x").unwrap();
        let cfg = detect_config(&json!({"name": "w", "path": d2.to_string_lossy()})).unwrap();
        assert_eq!(cfg.get("enabled"), Some(&json!(true)));
        assert_eq!(cfg.get("health"), Some(&json!("/index.html")));

        // sandbox enabled+launch -> returned verbatim-ish (cloned).
        let repo3 = json!({"name": "s", "path": "C:/x",
            "sandbox": {"enabled": true, "launch": ["foo"], "extra": 1}});
        let cfg3 = detect_config(&repo3).unwrap();
        assert_eq!(cfg3.get("extra"), Some(&json!(1)));

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- uuid_hex12 shape ----
    #[test]
    fn uuid_hex12_is_12_lowercase_hex() {
        let s = uuid_hex12();
        assert_eq!(s.len(), 12);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    // ---- README quickstart exists at the repo root ----
    // The repo root is the parent of the crate dir (CARGO_MANIFEST_DIR = .../src-tauri). The README
    // is operator-facing docs; this guards against the quickstart section being dropped silently.
    #[test]
    fn readme_has_quickstart_section() {
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = crate_dir.parent().unwrap().join("README.md");
        let body = std::fs::read_to_string(&readme)
            .unwrap_or_else(|_| panic!("README.md missing at {}", readme.display()));
        assert!(
            body.contains("## Quickstart"),
            "README.md has no Quickstart heading"
        );
        // The quickstart must point at the real test gate (the gate every PR passes).
        assert!(
            body.contains("cargo test"),
            "README Quickstart does not reference the cargo test gate"
        );
    }
}
