//! ops.json — the ops-plane probe registry (sibling of repos.json, joined to it BY NAME).
//!
//! Shape: a JSON list of per-project entries `{name, priority, probes: [...]}`. Parsed leniently
//! with `serde_json::Value` exactly like control/registry.rs parses repos.json — no serde derive
//! structs, no hard failures: a malformed entry or probe is skipped with a logged warning, never a
//! panic (a broken registry line must not take down the watchdog sweep). Unknown keys (e.g.
//! per-probe `comment` fields) round-trip untouched and are ignored by the evaluators.
//!
//! Path resolution: probe file/db paths may reference `%ENV%` variables (Windows style),
//! `${REPO}` (the project's repos.json path — the BY-NAME join), and relative paths (resolved
//! against Solomon HERE, e.g. `runtime/dotz/history.jsonl`).

use crate::control::paths;
use serde_json::Value;
use std::path::PathBuf;

/// HERE/ops.json — sibling of repos.json.
pub fn ops_json_path() -> PathBuf {
    paths::here().join("ops.json")
}

/// Load ops.json leniently: `[]` on missing/corrupt/non-list (with a logged warning for the
/// corrupt cases); non-dict or nameless entries are skipped with a warning. Never panics.
pub fn load_ops() -> Vec<Value> {
    load_ops_from(&ops_json_path())
}

/// Path-parameterized core of `load_ops` so tests run against isolated fixture files
/// (the production caller always passes `ops_json_path()`).
pub fn load_ops_from(path: &std::path::Path) -> Vec<Value> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return Vec::new(), // absent registry = no ops plane configured; not an error
    };
    let data: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("ops.json parse error (ops plane disabled this sweep): {e}");
            return Vec::new();
        }
    };
    let arr = match data {
        Value::Array(a) => a,
        _ => {
            eprintln!("ops.json is not a JSON list (ops plane disabled this sweep)");
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in arr {
        let named = entry.is_object()
            && entry
                .get("name")
                .and_then(Value::as_str)
                .map(|n| !n.is_empty())
                .unwrap_or(false);
        if !named {
            eprintln!("ops.json: skipping malformed project entry (not a dict with a name)");
            continue;
        }
        out.push(entry);
    }
    out
}

/// The ops entry's project name ("" never occurs post-load_ops filtering).
pub fn project_name(entry: &Value) -> String {
    entry
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// The ops entry's priority (asmodeus=1 highest ... solomon last). Missing/invalid -> i64::MAX so
/// unprioritized entries sort to the end rather than jumping the queue.
pub fn project_priority(entry: &Value) -> i64 {
    entry
        .get("priority")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX)
}

/// The entry's probe list; non-array/missing -> empty (a project with no probes rolls up green).
pub fn project_probes(entry: &Value) -> Vec<Value> {
    entry
        .get("probes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The BY-NAME join to repos.json: the project's repo path via control::registry::load_repos,
/// or "" when the name is not registered (probes that need ${REPO} then report unobservable).
pub fn project_repo_path(name: &str) -> String {
    for r in crate::control::registry::load_repos() {
        if r.get("name").and_then(Value::as_str) == Some(name) {
            return paths::repo_path(&r);
        }
    }
    String::new()
}

/// Resolve a probe path: expand `%ENV%` refs and `${REPO}`, then anchor relative paths at HERE.
/// Unknown %VARS% are left intact (ntpath.expandvars behavior, same as control/registry.rs).
pub fn resolve_path(raw: &str, repo_path: &str) -> PathBuf {
    let s = raw.replace("${REPO}", repo_path);
    let s = expand_env(&s);
    let p = PathBuf::from(&s);
    if p.is_absolute() {
        p
    } else {
        paths::here().join(p)
    }
}

/// Minimal %VAR% expansion (the only env-ref style the seed registry uses). Unknown vars are left
/// intact; `%%` collapses to a literal `%` (ntpath parity).
fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '%') {
                if end == 0 {
                    out.push('%');
                    i += 2;
                    continue;
                }
                let name: String = chars[i + 1..i + 1 + end].iter().collect();
                match std::env::var(&name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push('%');
                        out.push_str(&name);
                        out.push('%');
                    }
                }
                i = i + 1 + end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn write_fixture(name: &str, body: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("solomon_ops_reg_{}_{}", std::process::id(), name));
        std::fs::write(&p, body).unwrap();
        p
    }

    // -------- lenient parse: malformed entries skipped, never a panic --------
    #[test]
    fn load_ops_skips_malformed_entries() {
        let p = write_fixture(
            "mixed.json",
            r#"[
                {"name": "asmodeus", "priority": 1, "probes": []},
                "not a dict",
                {"priority": 9},
                {"name": ""},
                {"name": "sover", "probes": [{"id": "x", "kind": "file_exists"}]}
            ]"#,
        );
        let got = load_ops_from(&p);
        assert_eq!(got.len(), 2);
        assert_eq!(project_name(&got[0]), "asmodeus");
        assert_eq!(project_name(&got[1]), "sover");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_ops_missing_or_corrupt_is_empty() {
        // missing file -> [] (ops plane simply not configured)
        assert!(load_ops_from(std::path::Path::new("Z:/definitely/absent/ops.json")).is_empty());
        // corrupt JSON -> [] with a warning, never a panic
        let p = write_fixture("corrupt.json", "{not json");
        assert!(load_ops_from(&p).is_empty());
        let _ = std::fs::remove_file(&p);
        // a non-list top level -> []
        let p = write_fixture("nonlist.json", r#"{"name": "x"}"#);
        assert!(load_ops_from(&p).is_empty());
        let _ = std::fs::remove_file(&p);
    }

    // -------- accessors --------
    #[test]
    fn project_accessors_defaults() {
        let e = json!({"name": "n"});
        assert_eq!(project_priority(&e), i64::MAX); // unprioritized sorts last
        assert!(project_probes(&e).is_empty());
        let e = json!({"name": "n", "priority": 2, "probes": [{"id": "a"}]});
        assert_eq!(project_priority(&e), 2);
        assert_eq!(project_probes(&e).len(), 1);
    }

    // -------- path resolution --------
    #[test]
    // -------- path resolution --------
    #[test]
    fn resolve_path_expands_env_repo_and_relative() {
        unsafe {
            std::env::set_var("SOLOMON_OPS_TEST_VAR", "/tmp/ops_test_base");
        }
        let p = resolve_path("%SOLOMON_OPS_TEST_VAR%/data/x.json", "");
        assert_eq!(p, PathBuf::from("/tmp/ops_test_base/data/x.json"));
        unsafe {
            std::env::remove_var("SOLOMON_OPS_TEST_VAR");
        }

        // ${REPO} joins the repos.json path in.
        let p = resolve_path("${REPO}/logs/a.log", "/tmp/repo/root");
        assert_eq!(p, PathBuf::from("/tmp/repo/root/logs/a.log"));

        // relative paths anchor at HERE (Solomon's operator dir).
        let p = resolve_path("runtime/dotz/history.jsonl", "");
        assert!(p.is_absolute());
        assert!(p.ends_with(PathBuf::from("runtime/dotz/history.jsonl")));

        // unknown %VARS% stay intact (ntpath parity), %% collapses.
        assert_eq!(
            expand_env("%DEFINITELY_UNSET_OPS_VAR%"),
            "%DEFINITELY_UNSET_OPS_VAR%"
        );
        assert_eq!(expand_env("100%%"), "100%");
    }
}
