//! Port of control.py "contracts" section: per-repo `improver/<name>/{AGENT.md,backlog.md}`
//! read/write, the read-only stack probe (`detect_stack`), the deterministic no-LLM contract
//! renderer (`render_default_contract`), presence check, and the idempotent provisioner
//! (`ensure_contracts`). Bug-for-bug with control.py:1360-1581; bridge-return dicts have
//! byte-identical keys to the Python dicts (use `serde_json::json!`).
//!
//! Spec + golden vectors: src-tauri/control-port-spec.json (module "contracts").

use crate::control::paths::{here, repo_name, repo_path};
use crate::control::registry;
use serde_json::{Value, json};
use std::path::PathBuf;

/// control._CONTRACT_FILES.get(which): 'backlog'->backlog.md, 'agent'->AGENT.md, else None.
fn contract_file(which: &str) -> Option<&'static str> {
    match which {
        "backlog" => Some("backlog.md"),
        "agent" => Some("AGENT.md"),
        _ => None,
    }
}

/// control._contract_path: HERE/improver/<name>/<fn>, iff BOTH name and fn are truthy; else None.
pub fn contract_path(repo: &Value, which: &str) -> Option<PathBuf> {
    let name = repo_name(repo);
    let fname = contract_file(which)?;
    if name.is_empty() {
        return None;
    }
    Some(here().join("improver").join(name).join(fname))
}

/// control.read_contract: {ok, text} (text '' when the file does not exist yet), or
/// {ok:false, error} for an unknown `which` or a non-not-found OSError.
pub fn read_contract(repo: &Value, which: &str) -> Value {
    let p = match contract_path(repo, which) {
        Some(p) => p,
        None => return json!({"ok": false, "error": "unknown contract (use 'backlog' or 'agent')"}),
    };
    match std::fs::read_to_string(&p) {
        Ok(text) => json!({"ok": true, "text": text}),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({"ok": true, "text": ""}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// control.write_contract: create the dir + write `text` ('' when None), {ok} / {ok:false, error}.
pub fn write_contract(repo: &Value, which: &str, text: &str) -> Value {
    let p = match contract_path(repo, which) {
        Some(p) => p,
        None => return json!({"ok": false, "error": "unknown contract (use 'backlog' or 'agent')"}),
    };
    // makedirs(dirname(p), exist_ok=True) then truncating write.
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({"ok": false, "error": e.to_string()});
        }
    }
    match std::fs::write(&p, text.as_bytes()) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// control.contracts_present: {agent:bool, backlog:bool} — each True iff the file exists AND its
/// content is non-empty after .strip(). Iteration order is agent then backlog (key order matters).
pub fn contracts_present(repo: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for which in ["agent", "backlog"] {
        let present = match contract_path(repo, which) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(text) => !py_strip(&text).is_empty(),
                Err(_) => false,
            },
            None => false,
        };
        out.insert(which.to_string(), Value::Bool(present));
    }
    Value::Object(out)
}

/// Detected stack: {lang, test_cmd, entrypoints, top_dirs}. Read-only, never raises.
pub fn detect_stack(path: &str) -> Value {
    // Default dict (key order lang, test_cmd, entrypoints, top_dirs).
    let mut entrypoints: Vec<Value> = Vec::new();
    let mut top_dirs: Vec<Value> = Vec::new();
    let mut lang = "unknown";
    let mut test_cmd = String::new();

    let base = std::path::Path::new(path);
    if path.is_empty() || !base.is_dir() {
        return json!({"lang": lang, "test_cmd": test_cmd, "entrypoints": entrypoints, "top_dirs": top_dirs});
    }
    let here_f = |f: &str| base.join(f).exists();
    let isdir_f = |f: &str| base.join(f).is_dir();
    let win = cfg!(windows);
    let venv_rel = if win {
        ".venv/Scripts/python.exe"
    } else {
        ".venv/bin/python"
    };
    // Windows literal: backslash, NO .exe (the emitted command runs through cmd.exe/shell=True, where
    // a forward-slash exe at line start fails). POSIX: forward-slash, no .exe.
    let py = if here_f(venv_rel) {
        if win {
            r".venv\Scripts\python"
        } else {
            ".venv/bin/python"
        }
    } else {
        "python"
    };
    let pytest_cfg = here_f("pytest.ini") || here_f("conftest.py") || here_f("tests/conftest.py");
    let py_test_cmd = || -> String {
        if pytest_cfg || !isdir_f("tests") {
            format!("{py} -m pytest")
        } else {
            format!("{py} -m unittest discover -s tests -t tests")
        }
    };
    let has_manifest = [
        "pyproject.toml",
        "requirements.txt",
        "requirements-dev.txt",
        "setup.py",
        "setup.cfg",
    ]
    .iter()
    .any(|f| here_f(f));

    if has_manifest {
        lang = "python";
        test_cmd = py_test_cmd();
    } else if here_f("package.json") {
        lang = "node";
        test_cmd = "npm test".to_string();
    } else if here_f("Cargo.toml") {
        lang = "rust";
        test_cmd = "cargo test".to_string();
    } else if here_f("go.mod") {
        lang = "go";
        test_cmd = "go test ./...".to_string();
    } else if here_f(venv_rel) && isdir_f("tests") {
        lang = "python";
        test_cmd = py_test_cmd();
    }

    for ep in [
        "app.py",
        "main.py",
        "__main__.py",
        "index.js",
        "src/main.py",
        "src/main.js",
        "src/main.ts",
    ] {
        if here_f(ep) {
            entrypoints.push(Value::String(ep.to_string()));
        }
    }

    // top_dirs: visible (non-dot) subdirs, sorted ascending by name, capped at 8. scandir OSError swallowed.
    if let Ok(rd) = std::fs::read_dir(base) {
        let mut names: Vec<String> = Vec::new();
        for entry in rd.flatten() {
            let n = entry.file_name().to_string_lossy().into_owned();
            // is_dir() follows symlinks like os.scandir entry.is_dir() (default follow_symlinks=True).
            let is_dir = entry.path().is_dir();
            if is_dir && !n.starts_with('.') {
                names.push(n);
            }
        }
        names.sort();
        for n in names.into_iter().take(8) {
            top_dirs.push(Value::String(n));
        }
    }

    json!({"lang": lang, "test_cmd": test_cmd, "entrypoints": entrypoints, "top_dirs": top_dirs})
}

/// control.render_default_contract: deterministic (agent_md, backlog_md) tailored to the stack.
pub fn render_default_contract(repo: &Value) -> (String, String) {
    let name_owned = repo_name(repo);
    let name = if name_owned.is_empty() {
        "project".to_string()
    } else {
        name_owned
    };

    let stack = detect_stack(&repo_path(repo));
    let stack_lang = stack
        .get("lang")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let stack_test_cmd = stack.get("test_cmd").and_then(Value::as_str).unwrap_or("");
    let entrypoints: Vec<&str> = stack
        .get("entrypoints")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let top_dirs: Vec<&str> = stack
        .get("top_dirs")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    // gate fallback chain: custom gate > detected test_cmd > literal.
    let gate = match registry::project_gate(repo) {
        Some(g) if !g.is_empty() => g,
        _ => {
            if !stack_test_cmd.is_empty() {
                stack_test_cmd.to_string()
            } else {
                "(set a gate command in Config)".to_string()
            }
        }
    };

    let has_remote = repo.get("has_remote").map(value_truthy).unwrap_or(false);
    let goal = registry::project_goal(repo);

    let goal_block = if !goal.is_empty() {
        format!(
            "## North-star goal (weigh this above all else)\n\n> {goal}\n\nEvery iteration must move this goal forward — choose the single improvement with the most leverage\ntoward it. If achieving it needs a capability the project does not have yet, **build that capability**\n(still as one small, tested, shippable increment). The backlog serves the goal; when the backlog and\nthe goal disagree, the goal wins.\n\n"
        )
    } else {
        String::new()
    };

    let code_map = if !entrypoints.is_empty() {
        let eps = entrypoints
            .iter()
            .map(|e| format!("`{e}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "- Entry points: {eps}\n- Detected stack: {stack_lang}.\n- Read these first to learn the codebase before changing anything."
        )
    } else if !top_dirs.is_empty() {
        let dirs = top_dirs
            .iter()
            .map(|d| format!("`{d}`"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "- Top-level directories: {dirs}\n- Read these first to learn the codebase before changing anything."
        )
    } else {
        "- Read the README and the main entry point first to learn the codebase.".to_string()
    };

    let github_para = if has_remote {
        "\n- You MAY use the read-only `github_*` tools (`github_status`, `github_verify_push`, `github_pr_status`, `github_ci_status`, `github_list_prs`) to confirm the GitHub connection and check whether any open `rsi/*` PR is failing CI — if a recent one is red, prefer a change that fixes it. These tools only read; they never push, merge, or close."
    } else {
        ""
    };

    let agent_md = format!(
        "# {name} self-improvement contract\n\nYou are the **{name} improver** — an autonomous coding agent running one iteration of a\ncontinuous self-improvement loop on the {name} codebase. Each run, ship **one** small, real,\nverified improvement.\n\n{goal_block}## Your job this run (exactly one improvement)\n\n1. **The improvement is named in your task message.** Implement that one item. If it is already\n   done or unclear, instead fix one clear bug, missing test, rough edge, or simplification you\n   find while reading the code. Either way, do exactly *one* thing.\n2. **Implement it** with the smallest coherent change. Match the existing style; no new\n   dependencies or frameworks unless truly required; no speculative abstraction. Doing more than\n   the one item is a regression.\n3. **Add or update a test** that covers the change. Never delete, weaken, `xfail`, or skip an\n   existing test to \"make it pass.\"\n4. **Verify locally before you finish:** run the gate yourself — `{gate}` — it must be green. If\n   your change can't go green, revert your own edits and pick something smaller.\n5. **Summarize**: end with 2–4 sentences — what you changed, which file(s), and why. This becomes\n   the pull-request description.\n\n## Rules\n\n- **Do NOT run git or `gh` directly, and never push or merge.** The runner owns version control:\n  it created your branch, re-runs the gate authoritatively, and — only if green — commits and\n  opens a pull request for the operator to review.{github_para}\n- **Stay in the product.** Edit the application source and its tests/docs. Do NOT modify\n  `.github/`, `.env` / secrets, or build/packaging files unless the task explicitly says so.\n- **Keep tests portable.** The gate may run on Linux CI and installs only the repo's declared\n  dependencies — tests must not require a GUI, the network, or any package not in the project's\n  requirements. Guard OS-specific paths.\n- **Keep it shippable.** No half-finished features behind the gate; scope down to a complete,\n  tested slice and note the rest in your summary.\n\n## Map of the code\n\n{code_map}\n\n_Auto-generated by Solomon. Refine it, or use \u{201c}Enrich with AI\u{201d} to make it project-specific._\n"
    );

    let backlog_md = format!(
        "# {name} backlog\n\nImprovements the loop pulls from, top first. Edit freely.\n\n- [ ] add a test for the most-used module / entrypoint\n- [ ] tighten error handling on the main entrypoint\n- [ ] improve the README quickstart\n"
    );

    (agent_md, backlog_md)
}

/// control.ensure_contracts: idempotent — guarantee AGENT.md + backlog.md exist (never overwrite),
/// and auto-set the gate from stack detection when the operator hasn't set one.
pub fn ensure_contracts(repo: &Value) -> Value {
    if repo_name(repo).is_empty() {
        return json!({"ok": false, "error": "repo has no name"});
    }
    let pres = contracts_present(repo);
    let pres_agent = pres.get("agent").and_then(Value::as_bool).unwrap_or(false);
    let pres_backlog = pres
        .get("backlog")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut created: Vec<Value> = Vec::new();
    if !(pres_agent && pres_backlog) {
        let (agent_md, backlog_md) = render_default_contract(repo);
        for (which, text, label, already) in [
            ("agent", agent_md, "AGENT.md", pres_agent),
            ("backlog", backlog_md, "backlog.md", pres_backlog),
        ] {
            if !already {
                let r = write_contract(repo, which, &text);
                if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    // {'ok': False, 'error': r.get('error')} — propagate error (may be null).
                    return json!({"ok": false, "error": r.get("error").cloned().unwrap_or(Value::Null)});
                }
                created.push(Value::String(label.to_string()));
            }
        }
    }

    let mut out = serde_json::Map::new();
    out.insert("ok".to_string(), Value::Bool(true));
    out.insert("created".to_string(), Value::Array(created));

    // Auto-set the gate when the operator hasn't set one and a stack is detectable.
    if registry::project_gate(repo).is_none() {
        let det = detect_stack(&repo_path(repo))
            .get("test_cmd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if !det.is_empty() {
            let r = registry::set_repo_config(
                &repo_name(repo),
                None,
                None,
                None,
                Some(&det),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
            if r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                out.insert("gate_set".to_string(), Value::String(det));
            }
        }
    }
    Value::Object(out)
}

/// Python str.strip(): trims ASCII + Unicode whitespace from both ends. For the presence check we
/// only need "is the trimmed content empty?", so trimming Rust whitespace matches Python here.
fn py_strip(s: &str) -> &str {
    s.trim()
}

/// Python truthiness for a JSON value, as used by `bool((repo or {}).get('has_remote'))`:
/// false/null/0/0.0/""/[]/{} are falsy; everything else truthy.
fn value_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    // ----- contract_path -----

    #[test]
    fn contract_path_valid_and_mapping() {
        let p = contract_path(&json!({"name": "sover", "path": "C:/r/sover"}), "agent").unwrap();
        assert!(p.ends_with("improver/sover/AGENT.md") || p.ends_with("improver\\sover\\AGENT.md"));
        let b = contract_path(&json!({"name": "r"}), "backlog").unwrap();
        assert!(b.ends_with("improver/r/backlog.md") || b.ends_with("improver\\r\\backlog.md"));
    }

    #[test]
    fn contract_path_unknown_and_no_name() {
        assert!(contract_path(&json!({"name": "r"}), "foo").is_none());
        assert!(contract_path(&json!({}), "agent").is_none());
        // case-sensitive: 'AGENT'/'Backlog' are unknown
        assert!(contract_path(&json!({"name": "r"}), "AGENT").is_none());
    }

    // ----- read_contract / write_contract round-trip (uses a temp repo under improver/) -----

    fn unique_repo() -> (Value, PathBuf) {
        let name = format!(
            "__contracts_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let repo = json!({"name": name, "path": "C:/nonexistent_test_path"});
        let dir = here().join("improver").join(&name);
        (repo, dir)
    }

    #[test]
    fn read_unknown_which_exact_error() {
        let r = read_contract(&json!({"name": "r"}), "AGENT");
        assert_eq!(
            r,
            json!({"ok": false, "error": "unknown contract (use 'backlog' or 'agent')"})
        );
    }

    #[test]
    fn read_missing_file_is_empty_ok() {
        let (repo, dir) = unique_repo();
        // ensure the file does not exist
        let _ = fs::remove_dir_all(&dir);
        let r = read_contract(&repo, "agent");
        assert_eq!(r, json!({"ok": true, "text": ""}));
    }

    #[test]
    fn write_then_read_roundtrip_and_none_text() {
        let (repo, dir) = unique_repo();
        let w = write_contract(&repo, "agent", "# contract");
        assert_eq!(w, json!({"ok": true}));
        let r = read_contract(&repo, "agent");
        assert_eq!(r, json!({"ok": true, "text": "# contract"}));

        // None-text equivalent: empty string writes empty file
        let w2 = write_contract(&repo, "backlog", "");
        assert_eq!(w2, json!({"ok": true}));
        let r2 = read_contract(&repo, "backlog");
        assert_eq!(r2, json!({"ok": true, "text": ""}));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_unknown_which() {
        let r = write_contract(&json!({"name": "r"}), "x", "y");
        assert_eq!(
            r,
            json!({"ok": false, "error": "unknown contract (use 'backlog' or 'agent')"})
        );
    }

    // ----- contracts_present -----

    #[test]
    fn contracts_present_both_and_whitespace() {
        let (repo, dir) = unique_repo();
        let _ = fs::remove_dir_all(&dir);
        // both absent
        assert_eq!(
            contracts_present(&repo),
            json!({"agent": false, "backlog": false})
        );
        // agent whitespace-only -> false; backlog non-empty -> true
        write_contract(&repo, "agent", "   \n");
        write_contract(&repo, "backlog", "y");
        assert_eq!(
            contracts_present(&repo),
            json!({"agent": false, "backlog": true})
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn contracts_present_no_name() {
        assert_eq!(
            contracts_present(&json!({})),
            json!({"agent": false, "backlog": false})
        );
    }

    #[test]
    fn contracts_present_key_order() {
        // serde_json::Map preserves insertion order with the "preserve_order" feature; regardless,
        // equality holds. Assert the keys are agent then backlog when serialized.
        let (repo, dir) = unique_repo();
        let _ = fs::remove_dir_all(&dir);
        let s = serde_json::to_string(&contracts_present(&repo)).unwrap();
        assert!(s.starts_with("{\"agent\""), "got {s}");
    }

    // ----- detect_stack -----

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "solomon_ds_{tag}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detect_stack_nonexistent_path() {
        assert_eq!(
            detect_stack("C:/nope_does_not_exist_zzz"),
            json!({"lang": "unknown", "test_cmd": "", "entrypoints": [], "top_dirs": []})
        );
        assert_eq!(
            detect_stack(""),
            json!({"lang": "unknown", "test_cmd": "", "entrypoints": [], "top_dirs": []})
        );
    }

    #[test]
    fn detect_stack_python_pytest_manifest() {
        let d = tmp_dir("pytest");
        fs::write(d.join("pyproject.toml"), "[project]").unwrap();
        fs::write(d.join("pytest.ini"), "[pytest]").unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::create_dir_all(d.join("tests")).unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        // Golden vector input has .venv/Scripts/python.exe present, so `py` is the venv form
        // ('.venv\Scripts\python' on Windows). Create it so the detection matches the spec vector.
        let venv_exe = if cfg!(windows) {
            d.join(".venv").join("Scripts").join("python.exe")
        } else {
            d.join(".venv").join("bin").join("python")
        };
        fs::create_dir_all(venv_exe.parent().unwrap()).unwrap();
        fs::write(&venv_exe, "").unwrap();
        let r = detect_stack(d.to_str().unwrap());
        assert_eq!(r["lang"], "python");
        let expected = if cfg!(windows) {
            r".venv\Scripts\python -m pytest"
        } else {
            ".venv/bin/python -m pytest"
        };
        assert_eq!(r["test_cmd"], expected);
        assert_eq!(r["top_dirs"], json!(["src", "tests"])); // .git excluded, sorted
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_stack_manifest_tests_no_pytest_is_unittest() {
        let d = tmp_dir("unittest");
        fs::write(d.join("requirements.txt"), "").unwrap();
        fs::create_dir_all(d.join("tests")).unwrap();
        let r = detect_stack(d.to_str().unwrap());
        assert_eq!(r["lang"], "python");
        // no venv exe -> py == "python"
        assert_eq!(
            r["test_cmd"],
            "python -m unittest discover -s tests -t tests"
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_stack_node_rust_go() {
        let dn = tmp_dir("node");
        fs::write(dn.join("package.json"), "{}").unwrap();
        assert_eq!(detect_stack(dn.to_str().unwrap())["test_cmd"], "npm test");
        assert_eq!(detect_stack(dn.to_str().unwrap())["lang"], "node");
        let _ = fs::remove_dir_all(&dn);

        let dr = tmp_dir("rust");
        fs::write(dr.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_stack(dr.to_str().unwrap())["test_cmd"], "cargo test");
        let _ = fs::remove_dir_all(&dr);

        let dg = tmp_dir("go");
        fs::write(dg.join("go.mod"), "module x").unwrap();
        assert_eq!(
            detect_stack(dg.to_str().unwrap())["test_cmd"],
            "go test ./..."
        );
        let _ = fs::remove_dir_all(&dg);
    }

    #[test]
    fn detect_stack_manifest_beats_others() {
        let d = tmp_dir("prio");
        fs::write(d.join("pyproject.toml"), "[project]").unwrap();
        fs::write(d.join("package.json"), "{}").unwrap();
        fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_stack(d.to_str().unwrap())["lang"], "python");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_stack_entrypoint_ordering() {
        let d = tmp_dir("eps");
        fs::write(d.join("main.py"), "").unwrap();
        fs::write(d.join("app.py"), "").unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src").join("main.ts"), "").unwrap();
        let r = detect_stack(d.to_str().unwrap());
        assert_eq!(
            r["entrypoints"],
            json!(["app.py", "main.py", "src/main.ts"])
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn detect_stack_unknown_lists_dirs() {
        let d = tmp_dir("unk");
        fs::create_dir_all(d.join("alpha")).unwrap();
        fs::create_dir_all(d.join("beta")).unwrap();
        fs::create_dir_all(d.join(".hidden")).unwrap();
        let r = detect_stack(d.to_str().unwrap());
        assert_eq!(r["lang"], "unknown");
        assert_eq!(r["test_cmd"], "");
        assert_eq!(r["top_dirs"], json!(["alpha", "beta"]));
        let _ = fs::remove_dir_all(&d);
    }

    // ----- render_default_contract -----

    #[test]
    fn render_backlog_fixed_format() {
        let (agent, backlog) = render_default_contract(&json!({"name": "sover"}));
        assert_eq!(
            backlog,
            "# sover backlog\n\nImprovements the loop pulls from, top first. Edit freely.\n\n- [ ] add a test for the most-used module / entrypoint\n- [ ] tighten error handling on the main entrypoint\n- [ ] improve the README quickstart\n"
        );
        assert!(agent.starts_with("# sover self-improvement contract"));
    }

    #[test]
    fn render_no_name_defaults_to_project() {
        let (agent, backlog) = render_default_contract(&json!({}));
        assert!(backlog.starts_with("# project backlog\n"));
        assert!(agent.starts_with("# project self-improvement contract"));
    }

    #[test]
    fn render_gate_fallback_literal_when_nothing_detected() {
        // unknown stack (path missing) + no gate -> literal in the Verify-locally line
        let (agent, _) = render_default_contract(&json!({"name": "r", "path": "C:/nope_zzz"}));
        assert!(agent.contains(
            "run the gate yourself — `(set a gate command in Config)` — it must be green."
        ));
    }

    #[test]
    fn render_goal_present_injects_block() {
        let (agent, _) = render_default_contract(&json!({"name": "r", "goal": "maximize profit"}));
        assert!(agent.contains("## North-star goal (weigh this above all else)\n\n> maximize profit\n\nEvery iteration must move this goal forward"));
        // block sits before "## Your job this run"
        let nstar = agent.find("## North-star").unwrap();
        let job = agent.find("## Your job this run").unwrap();
        assert!(nstar < job);
    }

    #[test]
    fn render_goal_absent_omits_block() {
        let (agent, _) = render_default_contract(&json!({"name": "r"}));
        assert!(!agent.contains("## North-star"));
        assert!(
            agent.contains(
                "verified improvement.\n\n## Your job this run (exactly one improvement)"
            )
        );
    }

    #[test]
    fn render_has_remote_toggles_github_para() {
        let (with, _) = render_default_contract(&json!({"name": "r", "has_remote": true}));
        assert!(with.contains("You MAY use the read-only `github_*` tools"));
        assert!(with.contains("they never push, merge, or close."));
        let (without, _) = render_default_contract(&json!({"name": "r"}));
        assert!(!without.contains("github_*"));
        // pull-request line immediately followed by the Stay-in-the-product bullet
        assert!(without.contains(
            "opens a pull request for the operator to review.\n- **Stay in the product.**"
        ));
    }

    #[test]
    fn render_code_map_entrypoints_includes_stack_line() {
        let d = tmp_dir("cm_ep");
        fs::write(d.join("app.py"), "").unwrap();
        fs::write(d.join("requirements.txt"), "").unwrap();
        let (agent, _) =
            render_default_contract(&json!({"name": "r", "path": d.to_str().unwrap()}));
        assert!(agent.contains("- Entry points: `app.py`\n- Detected stack: python.\n- Read these first to learn the codebase before changing anything."));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn render_code_map_topdirs_no_stack_line() {
        let d = tmp_dir("cm_td");
        fs::create_dir_all(d.join("lib")).unwrap();
        fs::create_dir_all(d.join("src")).unwrap();
        let (agent, _) =
            render_default_contract(&json!({"name": "r", "path": d.to_str().unwrap()}));
        // top_dirs sorted: lib, src
        assert!(agent.contains("- Top-level directories: `lib`, `src`\n- Read these first to learn the codebase before changing anything."));
        assert!(!agent.contains("Detected stack:"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn render_code_map_fallback() {
        let (agent, _) = render_default_contract(&json!({"name": "r", "path": "C:/nope_zzz2"}));
        assert!(
            agent.contains(
                "- Read the README and the main entry point first to learn the codebase."
            )
        );
    }

    #[test]
    fn render_curly_quotes_in_footer() {
        let (agent, _) = render_default_contract(&json!({"name": "r"}));
        assert!(agent.contains("use \u{201c}Enrich with AI\u{201d} to make it project-specific."));
    }

    // ----- ensure_contracts -----

    #[test]
    fn ensure_no_name() {
        assert_eq!(
            ensure_contracts(&json!({})),
            json!({"ok": false, "error": "repo has no name"})
        );
    }

    #[test]
    fn ensure_only_backlog_missing_no_gate_unknown_stack() {
        let (repo, dir) = unique_repo();
        let _ = fs::remove_dir_all(&dir);
        // pre-create a non-empty AGENT.md only
        write_contract(&repo, "agent", "# present");
        let r = ensure_contracts(&repo);
        assert_eq!(r["ok"], json!(true));
        assert_eq!(r["created"], json!(["backlog.md"]));
        // unknown stack (path missing) -> no gate_set
        assert!(r.get("gate_set").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    // ===================================================================== #
    // D7 STARTUP CONTRACT: every orchestrator ledger MUST have a reader wired
    // into the CEO orchestrator's dispatch/planning path — a WRITE-ONLY ledger
    // is a bug (failure catalog #5: asmodeus's refutation blocklist was written
    // but never read -> 166 re-litigations of the same 3 dead families). This
    // asserts each ledger the orchestrator's Task-dispatch decision depends on
    // is READ BACK to change the decision; a ledger whose value is never
    // consulted fails this test loudly at `cargo test` time.
    // ===================================================================== #

    /// An isolated lane Ctx over injected tmp paths (mirrors progress.rs::test_ctx): its own runtime
    /// dir + a NON-git repo dir so the state-hash components are STABLE and test-controlled. Used to
    /// drive the orchestrator's real readers over a synthetic ledger.
    fn d7_iso_ctx(name: &str) -> crate::improver::ctx::Ctx {
        use crate::improver::ctx::Ctx;
        let base = std::env::temp_dir().join(format!(
            "solomon_d7contract_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = fs::create_dir_all(&control);
        let _ = fs::create_dir_all(&repo);
        let mut c = Ctx::configure(&repo.to_string_lossy(), name, "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join(name);
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        c.backlog = base.join("improver").join(name).join("backlog.md");
        c.lessons = base.join("improver").join(name).join("LESSONS.md");
        let _ = fs::create_dir_all(c.backlog.parent().unwrap());
        let _ = fs::create_dir_all(&c.runtime);
        c
    }

    /// The CLOSED registry of ledgers the D4 orchestrator's dispatch decision reads. Each entry
    /// names the ledger and a `reader` closure that PROVES the ledger's value is consulted by the
    /// orchestrator's real selection path (not a mock): the closure seeds the ledger to a value that
    /// MUST change the dispatch decision, runs the real reader, and returns whether the decision
    /// changed. A write-only ledger (value never read) can produce no such closure and fails the
    /// completeness assertion below.
    ///
    /// Extending the orchestrator with a new ledger REQUIRES adding it here with a passing reader
    /// proof — otherwise this contract test fails, enforcing the "no write-only ledger" rule.
    #[test]
    fn every_orchestrator_ledger_is_read_back_into_the_dispatch_decision() {
        use crate::ceo::orchestrator::{ORCHESTRATOR_LEDGERS, selection_readers_contract};
        use crate::improver::{calibration, progress};

        // (0) The registry is non-empty and names the two substrates D7 re-asserts.
        assert!(
            ORCHESTRATOR_LEDGERS.contains(&"progress.json"),
            "the quarantine ledger must be a registered orchestrator ledger"
        );
        assert!(
            ORCHESTRATOR_LEDGERS.contains(&"_task_calibration.json"),
            "the calibration ledger must be a registered orchestrator ledger"
        );

        // (1) progress.json READER PROOF: a quarantined key must make the orchestrator's selection
        // reader SKIP that task. Seed a quarantine, then assert the reader (quarantined()) reads it
        // back as true — a write-only quarantine ledger would read false and re-run the theater task.
        let mut ctx = d7_iso_ctx("d7prog");
        let key = progress::selection_key(&ctx, "a persistently no-delta task");
        progress::note_selected(&ctx, &key, "a persistently no-delta task");
        let pre = progress::state_hash(&ctx);
        for _ in 0..progress::QUARANTINE_STRIKES {
            progress::record_outcome(&mut ctx, &key, &pre, "noop");
        }
        assert!(
            progress::quarantined(&ctx, &key),
            "progress.json is READ BACK: a 3x no-delta key reads as quarantined (not write-only)"
        );

        // (2) _task_calibration.json READER PROOF: a proven-low (model, class) cell must make the
        // orchestrator's size reader emit a decompose directive. Seed the low cell, then assert the
        // reader (decompose_directive()) reads it back as Some — a write-only calibration ledger
        // would read None and dispatch the oversized item whole.
        let mut ctx2 = d7_iso_ctx("d7calib");
        let fleet_dir = ctx2.runtime.parent().unwrap().to_path_buf();
        for _ in 0..calibration::MIN_ATTEMPTS {
            calibration::record_outcome_at(&fleet_dir, &ctx2.pi_model, "architecture", false);
        }
        assert!(
            calibration::decompose_directive(&ctx2, "architecture").is_some(),
            "_task_calibration.json is READ BACK: a proven-low cell emits a decompose directive \
             (not write-only)"
        );
        // a healthy/cold cell reads back as None (the reader is value-sensitive, not always-on).
        assert!(
            calibration::decompose_directive(&ctx2, "chore").is_none(),
            "the calibration reader is value-sensitive — a cold cell yields no directive"
        );

        // (3) COMPLETENESS: every registered ledger has a wired reader proof (the function panics if
        // any ledger in the registry lacks one). This is the "no write-only ledger" gate — adding a
        // ledger without a reader trips it.
        let unread = selection_readers_contract(&mut ctx2);
        assert!(
            unread.is_empty(),
            "these orchestrator ledgers are WRITE-ONLY (written but never read back into the \
             dispatch decision): {unread:?} — wire a reader into the planning path or remove the \
             ledger (failure catalog #5)"
        );

        let _ = &mut ctx;
    }
}
