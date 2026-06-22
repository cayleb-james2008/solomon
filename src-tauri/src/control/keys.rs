//! Native Rust port of control.py's "keys" module — global per-provider API keys
//! stored as env lines in `<HERE>/.env`. Bug-for-bug with control.py:
//! `set_key` (control.py:668) and `keys_status` (control.py:700).
//!
//! Bridge-return dicts are `serde_json::Value` with byte-identical keys to Python.
//! `keys_status` returns a map whose JSON key order is fixed: "ollama-cloud" then
//! "openrouter" (a serde_json::Map preserves insertion order with the `preserve_order`
//! feature; we instead build the object explicitly so ordering is guaranteed).

use serde_json::{json, Value};
use std::fs;

use crate::control::paths;

/// `_PROVIDER_ENV_KEY` (control.py:665) — ordered: ollama-cloud FIRST, openrouter SECOND.
/// Insertion order is load-bearing for `keys_status` JSON key ordering.
const PROVIDER_ENV_KEY: &[(&str, &str)] = &[
    ("ollama-cloud", "OLLAMA_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
];

fn provider_env_var(provider: &str) -> Option<&'static str> {
    PROVIDER_ENV_KEY
        .iter()
        .find(|(p, _)| *p == provider)
        .map(|(_, k)| *k)
}

// --------------------------------------------------------------------------- #
// Python string-semantics helpers (replicated for bug-for-bug fidelity)
// --------------------------------------------------------------------------- #

/// True if `c` is whitespace per Python `str.strip()` (no args). This is the exact
/// set CPython recognizes as whitespace (str.isspace boundary used by strip), which
/// is broader than Rust's `char::is_whitespace` in a couple of control chars
/// (0x1c-0x1f) and excludes nothing it needs. Enumerated against CPython 3.11.
fn is_py_strip_ws(c: char) -> bool {
    matches!(c,
        '\u{09}' | '\u{0a}' | '\u{0b}' | '\u{0c}' | '\u{0d}'
        | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{1f}' | '\u{20}'
        | '\u{85}' | '\u{a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}'
        | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
    )
}

/// Python `str.strip()` (no args): trim Python-whitespace from both ends.
fn py_strip_ws(s: &str) -> &str {
    s.trim_matches(is_py_strip_ws)
}

/// True if `c` is a line boundary per Python `str.splitlines()`. Enumerated against
/// CPython 3.11. Note `\r\n` is handled as a single break by the splitter below.
fn is_py_linebreak(c: char) -> bool {
    matches!(c,
        '\u{0a}' | '\u{0b}' | '\u{0c}' | '\u{0d}'
        | '\u{1c}' | '\u{1d}' | '\u{1e}'
        | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// Python `str.splitlines()` (keepends=False): split on the full Python line-boundary
/// set, treat `\r\n` as one break, and DROP a trailing empty element (no empty final
/// item when text ends in a line break). A naive `split('\n')` is NOT byte-identical.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes_len = s.len();
    let mut start = 0usize;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if is_py_linebreak(c) {
            out.push(&s[start..i]);
            // \r\n collapses into a single boundary
            if c == '\r' {
                if let Some(&(_, '\u{0a}')) = it.peek() {
                    it.next();
                }
            }
            // next segment starts after the (possibly two-char) boundary
            start = it.peek().map(|&(j, _)| j).unwrap_or(bytes_len);
        }
    }
    if start < bytes_len {
        out.push(&s[start..]);
    }
    out
}

/// Python `value.strip('"')` then `.strip("'")` — non-recursive, order-dependent.
/// `strip(ch)` removes ALL leading/trailing occurrences of the single char.
fn strip_quotes(s: &str) -> &str {
    s.trim_matches('"').trim_matches('\'')
}

// --------------------------------------------------------------------------- #
// public bridge fns
// --------------------------------------------------------------------------- #

/// Port of `set_key` (control.py:668). Upsert the provider's env line in `<HERE>/.env`,
/// preserving other lines. Returns `{"ok": true}` or `{"ok": false, "error": ...}`.
///
/// NON-ATOMIC by design (matches Python): direct truncate-write of the real file, no
/// temp+rename. CRLF endings are normalized to LF on rewrite (splitlines + join("\n")).
pub fn set_key(provider: &str, value: &str) -> Value {
    let key = match provider_env_var(provider) {
        Some(k) => k,
        None => return json!({"ok": false, "error": format!("unknown provider: {}", provider)}),
    };
    // value = (value or "").strip(); the caller already passes &str (empty == Python "").
    let value = py_strip_ws(value);
    if value.contains('\n') || value.contains('\r') {
        return json!({"ok": false, "error": "key must be a single line"});
    }

    // try-block equivalent: only OSError is caught (mapped to fs::Error here).
    let mut lines: Vec<String> = Vec::new();
    if paths::env_file().exists() {
        match fs::read_to_string(paths::env_file()) {
            Ok(content) => lines = py_splitlines(&content).into_iter().map(String::from).collect(),
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        }
    }
    let new_line = format!("{}={}", key, value);
    let mut found = false;
    for line in lines.iter_mut() {
        // line.split("=", 1)[0].strip() == key
        let before_eq = line.split_once('=').map(|(a, _)| a).unwrap_or(line.as_str());
        if py_strip_ws(before_eq) == key {
            *line = new_line.clone();
            found = true;
            break; // only the FIRST match is replaced
        }
    }
    if !found {
        lines.push(new_line);
    }
    let mut body = lines.join("\n");
    body.push('\n');
    match fs::write(paths::env_file(), body) {
        Ok(()) => json!({"ok": true}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// Port of `keys_status` (control.py:700). `{"ollama-cloud": bool, "openrouter": bool}`
/// in that exact order — whether each provider's key line exists and is non-empty in
/// `.env` (after whitespace + surrounding-quote stripping). NEVER returns key values.
/// On read OSError, returns all-False.
pub fn keys_status() -> Value {
    let content = match fs::read_to_string(paths::env_file()) {
        Ok(c) => c,
        Err(_) => {
            // OSError -> all present=False, preserving order.
            let mut obj = serde_json::Map::new();
            for (prov, _) in PROVIDER_ENV_KEY {
                obj.insert((*prov).to_string(), Value::Bool(false));
            }
            return Value::Object(obj);
        }
    };
    // by_key: last duplicate wins (dict assignment) — OPPOSITE of set_key's first-wins.
    let mut by_key: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for line in py_splitlines(&content) {
        let (k, v) = match line.split_once('=') {
            Some(kv) => kv,
            None => continue, // "=" not in line -> skip
        };
        let k = py_strip_ws(k);
        // v.strip().strip('"').strip("'") — three sequential passes, order matters.
        let v = strip_quotes(py_strip_ws(v));
        by_key.insert(k, v);
    }
    let mut obj = serde_json::Map::new();
    for (prov, env_key) in PROVIDER_ENV_KEY {
        let present = by_key.get(*env_key).map(|v| !v.is_empty()).unwrap_or(false);
        obj.insert((*prov).to_string(), Value::Bool(present));
    }
    Value::Object(obj)
}

// --------------------------------------------------------------------------- #
// tests — built from the spec golden vectors (control-port-spec.json, module=keys)
// --------------------------------------------------------------------------- #
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard};

    // env_file() resolves to HERE/.env where HERE = the binary base dir. Tests must
    // not write to/read a shared real .env, and they mutate process-global state
    // (the file), so they are serialized and the file is saved/restored around each.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard<'a> {
        _g: MutexGuard<'a, ()>,
        saved: Option<Vec<u8>>,
    }
    impl<'a> EnvGuard<'a> {
        fn new() -> Self {
            let g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = std::fs::read(paths::env_file()).ok();
            let _ = std::fs::remove_file(paths::env_file());
            EnvGuard { _g: g, saved }
        }
        fn write_env(&self, content: &str) {
            std::fs::write(paths::env_file(), content).unwrap();
        }
        fn read_env(&self) -> String {
            std::fs::read_to_string(paths::env_file()).unwrap()
        }
    }
    impl<'a> Drop for EnvGuard<'a> {
        fn drop(&mut self) {
            match &self.saved {
                Some(bytes) => {
                    let _ = std::fs::write(paths::env_file(), bytes);
                }
                None => {
                    let _ = std::fs::remove_file(paths::env_file());
                }
            }
        }
    }

    // ----- _PROVIDER_ENV_KEY mapping (case-sensitive, exact, ordered) -----
    #[test]
    fn provider_env_var_mapping_exact() {
        assert_eq!(provider_env_var("ollama-cloud"), Some("OLLAMA_API_KEY"));
        assert_eq!(provider_env_var("openrouter"), Some("OPENROUTER_API_KEY"));
        assert_eq!(provider_env_var("Ollama-Cloud"), None); // case-sensitive
        let order: Vec<&str> = PROVIDER_ENV_KEY.iter().map(|(p, _)| *p).collect();
        assert_eq!(order, vec!["ollama-cloud", "openrouter"]);
    }

    // ----- set_key golden vectors -----
    #[test]
    fn set_key_unknown_provider_verbatim() {
        let g = EnvGuard::new();
        assert_eq!(set_key("gpt4", "sk-abc"), json!({"ok": false, "error": "unknown provider: gpt4"}));
        // short-circuits before any file access
        assert!(!paths::env_file().exists());
        drop(g);
    }

    #[test]
    fn set_key_empty_provider() {
        let _g = EnvGuard::new();
        assert_eq!(set_key("", "x"), json!({"ok": false, "error": "unknown provider: "}));
    }

    #[test]
    fn set_key_interior_newline_rejected() {
        let _g = EnvGuard::new();
        assert_eq!(
            set_key("openrouter", "line1\nline2"),
            json!({"ok": false, "error": "key must be a single line"})
        );
    }

    #[test]
    fn set_key_interior_cr_rejected() {
        let _g = EnvGuard::new();
        assert_eq!(
            set_key("openrouter", "a\rb"),
            json!({"ok": false, "error": "key must be a single line"})
        );
    }

    #[test]
    fn set_key_leading_trailing_newline_stripped_not_rejected() {
        let g = EnvGuard::new();
        assert_eq!(set_key("ollama-cloud", "\n sk-xyz \n"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=sk-xyz\n");
    }

    #[test]
    fn set_key_creates_env_when_absent() {
        let g = EnvGuard::new();
        assert_eq!(set_key("ollama-cloud", "sk-abc"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=sk-abc\n");
    }

    #[test]
    fn set_key_appends_preserving_first() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=sk-abc\n");
        assert_eq!(set_key("openrouter", "or-key"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=sk-abc\nOPENROUTER_API_KEY=or-key\n");
    }

    #[test]
    fn set_key_replace_in_place_preserve_others() {
        let g = EnvGuard::new();
        g.write_env("# header\nOLLAMA_API_KEY=old\nFOO=bar\n");
        assert_eq!(set_key("ollama-cloud", "new"), json!({"ok": true}));
        assert_eq!(g.read_env(), "# header\nOLLAMA_API_KEY=new\nFOO=bar\n");
    }

    #[test]
    fn set_key_empty_value_writes_empty() {
        let g = EnvGuard::new();
        // Python None coerces to "" via (value or ""). Rust caller passes "".
        assert_eq!(set_key("ollama-cloud", ""), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=\n");
    }

    #[test]
    fn set_key_whitespace_only_value_becomes_empty() {
        let g = EnvGuard::new();
        assert_eq!(set_key("openrouter", "   "), json!({"ok": true}));
        assert_eq!(g.read_env(), "OPENROUTER_API_KEY=\n");
    }

    #[test]
    fn set_key_match_tolerates_spaces_around_keyname() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY = old\n");
        assert_eq!(set_key("ollama-cloud", "v"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=v\n");
    }

    #[test]
    fn set_key_only_first_duplicate_replaced() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=a\nOLLAMA_API_KEY=b\n");
        assert_eq!(set_key("ollama-cloud", "c"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OLLAMA_API_KEY=c\nOLLAMA_API_KEY=b\n");
    }

    #[test]
    fn set_key_value_with_equals_verbatim() {
        let g = EnvGuard::new();
        assert_eq!(set_key("openrouter", "a=b=c"), json!({"ok": true}));
        assert_eq!(g.read_env(), "OPENROUTER_API_KEY=a=b=c\n");
    }

    #[test]
    fn set_key_crlf_normalized_to_lf() {
        let g = EnvGuard::new();
        g.write_env("FOO=bar\r\nOLLAMA_API_KEY=old\r\n");
        assert_eq!(set_key("ollama-cloud", "new"), json!({"ok": true}));
        assert_eq!(g.read_env(), "FOO=bar\nOLLAMA_API_KEY=new\n");
    }

    // ----- keys_status golden vectors -----
    #[test]
    fn keys_status_missing_env_all_false() {
        let _g = EnvGuard::new();
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": false}));
    }

    #[test]
    fn keys_status_both_present() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=a\nOPENROUTER_API_KEY=b\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": true, "openrouter": true}));
    }

    #[test]
    fn keys_status_empty_value_false() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=\nOPENROUTER_API_KEY=b\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": true}));
    }

    #[test]
    fn keys_status_double_quotes_stripped() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=\"abc\"\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": true, "openrouter": false}));
    }

    #[test]
    fn keys_status_single_quotes_stripped() {
        let g = EnvGuard::new();
        g.write_env("OPENROUTER_API_KEY='abc'\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": true}));
    }

    #[test]
    fn keys_status_only_quotes_empty_false() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=\"\"\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": false}));
    }

    #[test]
    fn keys_status_whitespace_only_false() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=   \n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": false}));
    }

    #[test]
    fn keys_status_key_spaces_matched_value_stripped() {
        let g = EnvGuard::new();
        g.write_env("  OLLAMA_API_KEY = foo \n");
        assert_eq!(keys_status(), json!({"ollama-cloud": true, "openrouter": false}));
    }

    #[test]
    fn keys_status_duplicate_last_wins() {
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY=x\nOLLAMA_API_KEY=\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": false}));
    }

    #[test]
    fn keys_status_lines_without_eq_ignored_value_with_eq_present() {
        let g = EnvGuard::new();
        g.write_env("# comment\nrandomline\nOPENROUTER_API_KEY=a=b\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": false, "openrouter": true}));
    }

    #[test]
    fn keys_status_output_key_order_fixed() {
        let g = EnvGuard::new();
        g.write_env("OPENROUTER_API_KEY=b\nOLLAMA_API_KEY=a\n");
        // Order must be ollama-cloud first regardless of .env line order.
        let s = serde_json::to_string(&keys_status()).unwrap();
        assert_eq!(s, r#"{"ollama-cloud":true,"openrouter":true}"#);
    }

    // ----- nested-quote ordering edge case (spec edge_cases, not a numbered vector) -----
    #[test]
    fn keys_status_nested_quote_order_dependent() {
        // value '"key"' wrapped in single quotes: '\'"key"\'' -> strip('"') no-op
        // (ends with '), strip("'") removes outer singles -> '"key"' non-empty -> True.
        let g = EnvGuard::new();
        g.write_env("OLLAMA_API_KEY='\"key\"'\n");
        assert_eq!(keys_status(), json!({"ollama-cloud": true, "openrouter": false}));
    }

    // ----- py_splitlines fidelity -----
    #[test]
    fn py_splitlines_drops_trailing_empty_and_handles_crlf() {
        assert_eq!(py_splitlines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\rb"), vec!["a", "b"]);
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("x"), vec!["x"]);
    }
}
