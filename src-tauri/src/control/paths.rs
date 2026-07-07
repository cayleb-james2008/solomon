//! Port of control.py path/identity resolution: HERE/_base_dir, repos.json + projects dirs,
//! per-repo path/name/runtime dir, the repo .venv python, and host-Python discovery.

use crate::control::proc;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// control.LOCK_LIVE_FLOOR_S — shared lock-liveness staleness floor (seconds). Used by locks.rs;
/// MUST stay equal to run_improver.acquire_lock and solomon._stale (a lower value anywhere lets a
/// still-live runner's lock be declared dead -> two runners on one repo).
pub const LOCK_LIVE_FLOOR_S: f64 = 4500.0;

static HERE: OnceLock<PathBuf> = OnceLock::new();

/// control._base_dir: the operator data dir (repos.json, improver/, runtime/, .env).
///
/// SOLOMON_HOME (when it contains improver/) wins; else walk UP from the executable to the first
/// ancestor that has improver/ (the dev binary lives under the repo, the frozen exe sits beside the
/// operator data — the same walk resolves both); else the exe's own dir.
pub fn here() -> &'static Path {
    HERE.get_or_init(|| {
        if let Ok(home) = std::env::var("SOLOMON_HOME") {
            let p = PathBuf::from(&home);
            if p.join("improver").is_dir() {
                return p;
            }
        }
        // audit A.6 root-cause (2026-07-07): under `cargo test`, with no explicit SOLOMON_HOME
        // override, pin HERE to a throwaway temp home. Every test that touches per-repo state
        // (supervisor/watchdog/contracts/heartbeat/locks/…) resolves its working dirs through
        // here() (HERE/runtime/<name>, HERE/improver/<name>), so redirecting here() is what
        // actually moves those scratch roots off the LIVE operator tree — the reason editing the
        // individual test helpers alone cannot fix it. Tests that need a real HERE (apptest_health)
        // set SOLOMON_HOME explicitly and hit the branch above, so their behavior is unchanged.
        #[cfg(test)]
        {
            let home = std::env::temp_dir().join(format!("solomon_test_home_{}", std::process::id()));
            let _ = std::fs::create_dir_all(home.join("improver"));
            home
        }
        #[cfg(not(test))]
        {
            if let Ok(exe) = std::env::current_exe() {
                let mut probe = exe.parent().map(Path::to_path_buf);
                // depth 6 (NOT 8): control._base_dir uses range(6) — the exe dir + up to 5 ancestors.
                // solomon_repo() (apptest_health.rs) uses 8, a deliberately different constant; do not unify.
                for _ in 0..6 {
                    match probe {
                        Some(ref dir) if dir.join("improver").is_dir() => return dir.clone(),
                        Some(ref dir) => probe = dir.parent().map(Path::to_path_buf),
                        None => break,
                    }
                }
                if let Some(dir) = exe.parent() {
                    return dir.to_path_buf();
                }
            }
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        }
    })
}

/// control.REPOS_JSON
pub fn repos_json() -> PathBuf {
    here().join("repos.json")
}

/// control.PROJECTS_DIR — the sibling …/projects folder (parent of HERE).
pub fn projects_dir() -> PathBuf {
    here().parent().unwrap_or_else(|| here()).join("projects")
}

/// control._ENV_FILE
pub fn env_file() -> PathBuf {
    here().join(".env")
}

/// control._repo_path: repo["path"] or "".
pub fn repo_path(repo: &Value) -> String {
    repo.get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// control._repo_name: repo["name"] (if truthy) else basename(path) else "".
pub fn repo_name(repo: &Value) -> String {
    if let Some(n) = repo.get("name").and_then(Value::as_str) {
        if !n.is_empty() {
            return n.to_string();
        }
    }
    let p = repo_path(repo);
    if p.is_empty() {
        return String::new();
    }
    Path::new(&p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// control._runtime_dir: HERE/runtime/<name>, or None when the repo has no resolvable name.
/// (Per-repo runtime lives UNDER Solomon, never inside the target repo — keeps products RSI-free.)
pub fn runtime_dir(repo: &Value) -> Option<PathBuf> {
    let name = repo_name(repo);
    if name.is_empty() {
        None
    } else {
        Some(here().join("runtime").join(name))
    }
}

/// control._venv_python: <repo.path>/.venv/Scripts/python.exe, or None if no path.
pub fn venv_python(repo: &Value) -> Option<PathBuf> {
    let p = repo_path(repo);
    if p.is_empty() {
        None
    } else {
        Some(Path::new(&p).join(".venv").join("Scripts").join("python.exe"))
    }
}

/// control._runner_python: the interpreter to host run_improver.py — the repo's own .venv python
/// when present, else a discovered system Python >=3.11. (The native binary is not a Python host, so
/// unlike the Python original there is no "current interpreter" branch.) None when neither is found.
pub fn runner_python(repo: &Value) -> Option<PathBuf> {
    if let Some(py) = venv_python(repo) {
        if py.exists() {
            return Some(py);
        }
    }
    discover_host_python()
}

static HOST_PY: OnceLock<Option<PathBuf>> = OnceLock::new();

/// control._discover_host_python: a real Python >=3.11 to host run_improver.py. Tries the `py -3`
/// launcher, then common PATH names; verifies each is Python >=3.11 and not Solomon itself. Cached.
pub fn discover_host_python() -> Option<PathBuf> {
    HOST_PY
        .get_or_init(|| {
            let mut candidates: Vec<PathBuf> = Vec::new();
            if let Ok(launcher) = which::which("py") {
                let l = launcher.to_string_lossy().into_owned();
                if let Ok(o) = proc::run(
                    &[l.as_str(), "-3", "-c", "import sys;print(sys.executable)"],
                    None,
                    None,
                ) {
                    let s = o.stdout.trim();
                    if !s.is_empty() {
                        candidates.push(PathBuf::from(s));
                    }
                }
            }
            for name in ["python3.12", "python3.11", "python3", "python"] {
                if let Ok(w) = which::which(name) {
                    candidates.push(w);
                }
            }
            for c in candidates {
                let base = c
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if base.starts_with("solomon") || !c.exists() {
                    continue;
                }
                let cs = c.to_string_lossy().into_owned();
                if let Ok(o) = proc::run(
                    &[
                        cs.as_str(),
                        "-c",
                        "import sys;print('%d.%d' % sys.version_info[:2])",
                    ],
                    None,
                    None,
                ) {
                    let ver = o.stdout.trim();
                    if let Some((maj, minr)) = ver.split_once('.') {
                        if maj == "3" {
                            if let Ok(m) = minr.parse::<i32>() {
                                if m >= 11 {
                                    return Some(c);
                                }
                            }
                        }
                    }
                }
            }
            None
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repo_name_prefers_name_then_basename() {
        assert_eq!(repo_name(&json!({"name": "maki", "path": "C:/x/maki"})), "maki");
        assert_eq!(repo_name(&json!({"path": "C:/x/sover"})), "sover");
        assert_eq!(repo_name(&json!({"name": "", "path": "C:/x/dotz"})), "dotz");
        assert_eq!(repo_name(&json!({})), "");
    }

    #[test]
    fn repo_path_defaults_empty() {
        assert_eq!(repo_path(&json!({})), "");
        assert_eq!(repo_path(&json!({"path": "C:/a"})), "C:/a");
    }

    #[test]
    fn runtime_dir_none_without_name() {
        assert!(runtime_dir(&json!({})).is_none());
        assert!(runtime_dir(&json!({"name": "x"})).unwrap().ends_with("runtime/x")
            || runtime_dir(&json!({"name": "x"})).unwrap().ends_with("runtime\\x"));
    }
}
