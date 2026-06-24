//! Port of control.py's heartbeat / runtime-state readers: read_heartbeat, read_log, read_history,
//! read_supervisor_log, metrics, _heartbeat_age.
//!
//! Behavior is bug-for-bug with control.py. All per-repo runtime files live under
//! `HERE/runtime/<name>/` (the source docstrings that say `<repo.path>/.rsi/...` are stale — the
//! code, and therefore this port, uses `_runtime_dir` = `HERE/runtime/<name>`). Functions that back
//! JS bridge calls return `serde_json::Value` with byte-identical keys to the Python dicts.

use crate::control::paths;
use chrono::{NaiveDateTime, Utc};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};

/// control.read_heartbeat: `json.load` of `runtime/<name>/heartbeat.json`, or None if
/// missing/corrupt. A parseable-but-non-object heartbeat (e.g. `42`, `"x"`, `[1,2]`) reads as None
/// so a non-dict never leaks to callers' `.get()`.
pub fn read_heartbeat(repo: &Value) -> Option<Value> {
    let rt = paths::runtime_dir(repo)?;
    let data = std::fs::read_to_string(rt.join("heartbeat.json")).ok()?;
    let v: Value = serde_json::from_str(&data).ok()?;
    if v.is_object() {
        Some(v)
    } else {
        None
    }
}

/// control.read_log: tail (last 16384 bytes) of `runtime/<name>/improver.log`.
/// `{ok, log}` — log is "" when the file is missing; `{ok:false, error}` when the repo has no path.
/// Bytes are decoded UTF-8 with U+FFFD replacement (Python's `decode("utf-8", "replace")`).
/// (control.read_log's `max_bytes` is a keyword default of 16384; the JS bridge never overrides it,
/// so the contract signature fixes it. The tail logic is in `read_log_n`.)
pub fn read_log(repo: &Value) -> Value {
    read_log_n(repo, 16384)
}

/// read_log with an explicit byte cap — the testable core (golden vectors exercise max_bytes 4 / 0).
fn read_log_n(repo: &Value, max_bytes: u64) -> Value {
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return json!({"ok": false, "error": "repo has no 'path'"}),
    };
    let data = (|| -> std::io::Result<Vec<u8>> {
        let mut f = std::fs::File::open(rt.join("improver.log"))?;
        let size = f.seek(SeekFrom::End(0))?;
        // Python: f.seek(max(0, size - max_bytes)). On unsigned ints, saturating_sub == max(0, ...).
        let start = size.saturating_sub(max_bytes);
        f.seek(SeekFrom::Start(start))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    })();
    match data {
        Ok(bytes) => json!({"ok": true, "log": String::from_utf8_lossy(&bytes)}),
        Err(_) => json!({"ok": true, "log": ""}),
    }
}

/// control.read_history: last `limit` records from `runtime/<name>/history.jsonl` (oldest→newest).
/// Blank lines and lines that don't parse as JSON, and non-object records, are dropped (but they
/// still consume a slot of the `limit` window — the window is the last `limit` raw lines). `[]` on
/// missing file / no runtime dir.
///
/// NB: Python's `lines[-limit:]` with `limit == 0` yields ALL lines (`[-0:]` == `[0:]`). This port
/// reproduces that quirk: `limit == 0` returns every record.
pub fn read_history(repo: &Value, limit: usize) -> Vec<Value> {
    read_jsonl_tail(repo, "history.jsonl", limit)
}

/// control.read_supervisor_log: last `limit` records from `runtime/<name>/supervisor.jsonl`
/// (oldest→newest). Same parsing/skip/limit semantics as read_history.
pub fn read_supervisor_log(repo: &Value, limit: usize) -> Vec<Value> {
    read_jsonl_tail(repo, "supervisor.jsonl", limit)
}

/// Shared body of read_history / read_supervisor_log: tail the last `limit` lines of a JSONL file,
/// keeping only the object records. `limit == 0` => all lines (Python `[-0:]` quirk).
fn read_jsonl_tail(repo: &Value, file: &str, limit: usize) -> Vec<Value> {
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return Vec::new(),
    };
    let content = match std::fs::read_to_string(rt.join(file)) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    // Python str.splitlines(): split on line boundaries, no trailing empty element.
    let lines = splitlines(&content);
    let window: &[&str] = if limit == 0 {
        &lines[..] // [-0:] == [0:] == all
    } else if limit >= lines.len() {
        &lines[..]
    } else {
        &lines[lines.len() - limit..]
    };
    let mut out = Vec::new();
    for line in window {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(rec) if rec.is_object() => out.push(rec),
            _ => continue,
        }
    }
    out
}

/// control.metrics: aggregate `history.jsonl` into headline counters + a test-pass series.
///
/// success_rate = round(shipped / decided, 3) where decided = shipped+reverted+noop+blocked, or
/// null when decided == 0. Uses banker's rounding (round-half-to-even) to match Python's round()
/// byte-for-byte (Rust's f64::round is half-away-from-zero and would diverge on exact-half cases).
pub fn metrics(repo: &Value) -> Value {
    let hist = read_history(repo, 1000);
    let mut iterations = 0i64;
    let (mut shipped, mut merged, mut reverted, mut noop, mut blocked, mut error, mut stopped) =
        (0i64, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
    let mut tests_series: Vec<Value> = Vec::new();
    for rec in &hist {
        iterations += 1;
        match rec.get("status").and_then(Value::as_str) {
            Some("shipped") => shipped += 1,
            Some("reverted") => reverted += 1,
            Some("noop") => noop += 1,
            Some("blocked") => blocked += 1,
            Some("error") => error += 1,
            Some("stopped") => stopped += 1,
            _ => {}
        }
        if let Some(pr) = rec.get("pr") {
            if pr.is_object() && pr.get("state").and_then(Value::as_str) == Some("merged") {
                merged += 1;
            }
        }
        if let Some(tests) = rec.get("tests") {
            if tests.is_object() {
                // Python: `tests.get("passed") is not None` — present AND not JSON null.
                let passed = tests.get("passed");
                if passed.is_some() && passed != Some(&Value::Null) {
                    // `tests.get("passed") or 0` / `tests.get("failed") or 0`: keep a TRUTHY value
                    // VERBATIM (incl. a float 7.5, a string "7", or a negative int — the spec forbids
                    // i64-coercion); only a falsy value (0/0.0/null/missing/""/[]/{}/false) becomes 0.
                    let p = or_zero(tests.get("passed"));
                    let f = or_zero(tests.get("failed"));
                    tests_series.push(json!({
                        "ts": rec.get("ts").cloned().unwrap_or(Value::Null),
                        "passed": p,
                        "failed": f,
                    }));
                }
            }
        }
    }
    let decided = shipped + reverted + noop + blocked;
    let success_rate: Value = if decided != 0 {
        Value::from(round_half_even(shipped as f64 / decided as f64, 3))
    } else {
        Value::Null
    };
    json!({
        "iterations": iterations,
        "shipped": shipped,
        "merged": merged,
        "reverted": reverted,
        "noop": noop,
        "blocked": blocked,
        "error": error,
        "stopped": stopped,
        "tests_series": tests_series,
        "success_rate": success_rate,
    })
}

/// control._heartbeat_age: seconds since the heartbeat's `updated_at`, or None if absent/unparseable.
///
/// Guard order mirrors the source exactly: `ts = (hb or {}).get("updated_at")`; if `ts` is falsy
/// (missing key, JSON null, or empty string) -> None BEFORE any parse; a non-string ts -> None
/// (Python TypeError); else parse strictly with `%Y-%m-%dT%H:%M:%SZ` (no fractional seconds, no
/// offset) and return `(now_utc - last).total_seconds()` (negative for a future timestamp).
pub fn heartbeat_age(hb: &Value) -> Option<f64> {
    // (hb or {}).get("updated_at"): a JSON null hb, or any non-object, has no key -> None.
    let ts_val = hb.get("updated_at");
    let ts = match ts_val {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        // Falsy string ("" ), missing, or JSON null -> `if not ts` short-circuits to None.
        Some(Value::String(_)) | None | Some(Value::Null) => return None,
        // Non-string, non-null updated_at (e.g. a number) -> Python strptime raises TypeError -> None.
        Some(_) => return None,
    };
    // chrono parse with the verbatim source format string. Python's strptime accepts single-digit
    // fields; chrono's %Y/%m/%d/%H/%M/%S also accept non-zero-padded values, so both agree there.
    let last = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    let last_utc = last.and_utc();
    Some((Utc::now() - last_utc).num_milliseconds() as f64 / 1000.0)
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// Python str.splitlines(): split on \n / \r\n / \r boundaries, with no trailing empty element when
/// the text ends in a newline. (serde/std `lines()` only splits \n and \r\n, not lone \r; the JSONL
/// files this reads use \n, but we match Python's broader splitter for fidelity.)
fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&s[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&s[start..i]);
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 2;
                } else {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        out.push(&s[start..]);
    }
    out
}

/// Python `value or 0` for the metrics test counts: keep the JSON value VERBATIM when truthy, falling
/// back to 0 only on a falsy value. control-port-spec.json's metrics edge_cases mandates that non-int
/// counts round-trip unchanged — a string "7", a float 7.5, or a negative int -5 must NOT be
/// i64-coerced (it explicitly flags that as a Rust typing trap); only 0/0.0/null/missing/""/[]/{}/false
/// collapse to 0.
fn or_zero(v: Option<&Value>) -> Value {
    let truthy = match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    };
    if truthy {
        v.cloned().unwrap_or_else(|| json!(0))
    } else {
        json!(0)
    }
}

/// Python round(x, ndigits): round-half-to-even (banker's rounding) on the TRUE binary value of x.
/// Rust's `{:.N}` float formatter is correctly-rounded ties-to-even, so format-then-parse matches
/// CPython's round() byte-for-byte. The earlier `x*10^n` scale-then-round masked x's true value
/// whenever the product snapped to an exact .5 (e.g. 1/80 -> 0.012 instead of 0.013). Only ever
/// called with ndigits >= 0 (success_rate uses 3).
fn round_half_even(x: f64, ndigits: i32) -> f64 {
    format!("{:.*}", ndigits.max(0) as usize, x)
        .parse::<f64>()
        .unwrap_or(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_runtime(name: &str) -> (std::path::PathBuf, Value) {
        // runtime_dir == HERE/runtime/<name>; HERE resolves to a dir under the dev tree, but for
        // these tests we write/read via the real runtime_dir so the path logic is exercised end to
        // end. Use a unique name per test to avoid cross-test collisions.
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        (rt, repo)
    }

    // -------- read_heartbeat golden vectors --------
    #[test]
    fn read_heartbeat_vectors() {
        let (rt, repo) = tmp_runtime("hb_test_read_heartbeat");
        let hbf = rt.join("heartbeat.json");

        // Valid object heartbeat -> returned verbatim
        std::fs::write(
            &hbf,
            r#"{"status":"running","updated_at":"2026-06-22T10:00:00Z","run_id":"abc"}"#,
        )
        .unwrap();
        assert_eq!(
            read_heartbeat(&repo),
            Some(json!({"status":"running","updated_at":"2026-06-22T10:00:00Z","run_id":"abc"}))
        );

        // Non-object JSON (42) -> None
        std::fs::write(&hbf, "42").unwrap();
        assert_eq!(read_heartbeat(&repo), None);

        // Empty dict preserved
        std::fs::write(&hbf, "{}").unwrap();
        assert_eq!(read_heartbeat(&repo), Some(json!({})));

        // Corrupt JSON -> None
        std::fs::write(&hbf, r#"{"a":"#).unwrap();
        assert_eq!(read_heartbeat(&repo), None);

        // Missing file -> None
        let _ = std::fs::remove_file(&hbf);
        assert_eq!(read_heartbeat(&repo), None);

        // No runtime dir (empty name) -> None
        assert_eq!(read_heartbeat(&json!({})), None);

        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- read_log golden vectors --------
    #[test]
    fn read_log_vectors() {
        let (rt, repo) = tmp_runtime("hb_test_read_log");
        let logf = rt.join("improver.log");

        // Small log fully returned (default 16384 cap)
        std::fs::write(&logf, b"hello world\n").unwrap();
        assert_eq!(read_log(&repo), json!({"ok": true, "log": "hello world\n"}));

        // Tail truncation to last 4 bytes of "ABCDEFGHIJ" -> "GHIJ"
        std::fs::write(&logf, b"ABCDEFGHIJ").unwrap();
        assert_eq!(read_log_n(&repo, 4), json!({"ok": true, "log": "GHIJ"}));

        // max_bytes=0 -> ""
        std::fs::write(&logf, b"data").unwrap();
        assert_eq!(read_log_n(&repo, 0), json!({"ok": true, "log": ""}));

        // Invalid UTF-8 in tail -> two U+FFFD then " ok"
        std::fs::write(&logf, b"\xff\xfe ok").unwrap();
        assert_eq!(read_log(&repo), json!({"ok": true, "log": "\u{FFFD}\u{FFFD} ok"}));

        // Missing file -> {ok:true, log:""}
        let _ = std::fs::remove_file(&logf);
        assert_eq!(read_log(&repo), json!({"ok": true, "log": ""}));

        // No path/name -> error
        assert_eq!(
            read_log(&json!({})),
            json!({"ok": false, "error": "repo has no 'path'"})
        );

        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- read_history golden vectors --------
    #[test]
    fn read_history_vectors() {
        let (rt, repo) = tmp_runtime("hb_test_read_history");
        let hf = rt.join("history.jsonl");

        // ordering oldest->newest, last 2
        std::fs::write(&hf, "{\"i\":1}\n{\"i\":2}\n{\"i\":3}\n").unwrap();
        assert_eq!(read_history(&repo, 2), vec![json!({"i":2}), json!({"i":3})]);

        // blank/invalid lines in window consume slots
        std::fs::write(&hf, "{\"a\":1}\n\nnotjson\n{\"b\":2}\n").unwrap();
        assert_eq!(read_history(&repo, 3), vec![json!({"b":2})]);

        // limit=0 -> ALL lines
        std::fs::write(&hf, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        assert_eq!(read_history(&repo, 0), vec![json!({"a":1}), json!({"b":2})]);

        // non-dict records dropped
        std::fs::write(&hf, "42\n{\"x\":1}\n[1,2]\n").unwrap();
        assert_eq!(read_history(&repo, 50), vec![json!({"x":1})]);

        // missing file -> []
        let _ = std::fs::remove_file(&hf);
        assert_eq!(read_history(&repo, 50), Vec::<Value>::new());

        // no runtime dir -> []
        assert_eq!(read_history(&json!({}), 50), Vec::<Value>::new());

        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- read_supervisor_log golden vectors --------
    #[test]
    fn read_supervisor_log_vectors() {
        let (rt, repo) = tmp_runtime("hb_test_read_sup");
        let sf = rt.join("supervisor.jsonl");

        std::fs::write(&sf, "{\"e\":\"start\"}\n{\"e\":\"stale\"}\n{\"e\":\"clear\"}\n").unwrap();
        assert_eq!(
            read_supervisor_log(&repo, 2),
            vec![json!({"e":"stale"}), json!({"e":"clear"})]
        );

        std::fs::write(&sf, "{\"e\":1}\n").unwrap();
        assert_eq!(read_supervisor_log(&repo, 0), vec![json!({"e":1})]);

        let _ = std::fs::remove_file(&sf);
        assert_eq!(read_supervisor_log(&repo, 50), Vec::<Value>::new());

        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- metrics golden vectors --------
    #[test]
    fn metrics_vectors() {
        let (rt, repo) = tmp_runtime("hb_test_metrics");
        let hf = rt.join("history.jsonl");

        // mixed outcomes with merge and tests
        std::fs::write(
            &hf,
            "{\"status\":\"shipped\",\"pr\":{\"state\":\"merged\"},\"tests\":{\"passed\":10,\"failed\":0},\"ts\":\"t1\"}\n\
             {\"status\":\"reverted\",\"tests\":{\"passed\":5,\"failed\":2},\"ts\":\"t2\"}\n\
             {\"status\":\"noop\",\"ts\":\"t3\"}\n\
             {\"status\":\"error\",\"ts\":\"t4\"}\n",
        )
        .unwrap();
        let m = metrics(&repo);
        assert_eq!(m["iterations"], 4);
        assert_eq!(m["shipped"], 1);
        assert_eq!(m["merged"], 1);
        assert_eq!(m["reverted"], 1);
        assert_eq!(m["noop"], 1);
        assert_eq!(m["blocked"], 0);
        assert_eq!(m["error"], 1);
        assert_eq!(m["stopped"], 0);
        assert_eq!(
            m["tests_series"],
            json!([{"ts":"t1","passed":10,"failed":0},{"ts":"t2","passed":5,"failed":2}])
        );
        assert_eq!(m["success_rate"], json!(0.333)); // round(1/3,3)

        // success_rate None when no decided records
        std::fs::write(
            &hf,
            "{\"status\":\"error\"}\n{\"status\":\"stopped\"}\n{\"status\":\"running\"}\n",
        )
        .unwrap();
        let m = metrics(&repo);
        assert_eq!(m["iterations"], 3);
        assert_eq!(m["error"], 1);
        assert_eq!(m["stopped"], 1);
        assert_eq!(m["tests_series"], json!([]));
        assert_eq!(m["success_rate"], Value::Null);

        // blocked in denominator
        std::fs::write(
            &hf,
            "{\"status\":\"shipped\"}\n{\"status\":\"shipped\"}\n{\"status\":\"shipped\"}\n{\"status\":\"blocked\"}\n",
        )
        .unwrap();
        let m = metrics(&repo);
        assert_eq!(m["shipped"], 3);
        assert_eq!(m["blocked"], 1);
        assert_eq!(m["success_rate"], json!(0.75));

        // tests passed=0 appended; failed null -> 0; decided=1 -> 0.0
        std::fs::write(&hf, "{\"status\":\"noop\",\"tests\":{\"passed\":0,\"failed\":null},\"ts\":1}\n").unwrap();
        let m = metrics(&repo);
        assert_eq!(m["noop"], 1);
        assert_eq!(m["tests_series"], json!([{"ts":1,"passed":0,"failed":0}]));
        assert_eq!(m["success_rate"], json!(0.0));

        // spec fidelity: a TRUTHY non-int test count round-trips VERBATIM (NOT i64-coerced) — a string
        // "7" and a float 7.5 are kept as-is; only a falsy value collapses to 0 (per control-port-spec).
        std::fs::write(&hf, "{\"status\":\"noop\",\"tests\":{\"passed\":\"7\",\"failed\":7.5},\"ts\":1}\n").unwrap();
        let m = metrics(&repo);
        assert_eq!(m["tests_series"], json!([{"ts":1,"passed":"7","failed":7.5}]));

        // tests passed=null NOT appended
        std::fs::write(&hf, "{\"status\":\"noop\",\"tests\":{\"failed\":3}}\n").unwrap();
        let m = metrics(&repo);
        assert_eq!(m["tests_series"], json!([]));

        // empty history (no file)
        let _ = std::fs::remove_file(&hf);
        let m = metrics(&repo);
        assert_eq!(
            m,
            json!({"iterations":0,"shipped":0,"merged":0,"reverted":0,"noop":0,
                   "blocked":0,"error":0,"stopped":0,"tests_series":[],"success_rate":null})
        );

        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- _heartbeat_age golden vectors --------
    // The "now" cases are tested via a fixed-delta helper: we synthesize updated_at relative to the
    // real now and assert the age is within a small tolerance. The fixed-now exactness vectors
    // (3600 / -3600) are validated through the parse + arithmetic with a tolerance for the elapsed
    // test runtime.
    #[test]
    fn heartbeat_age_vectors() {
        // Missing updated_at -> None
        assert_eq!(heartbeat_age(&json!({"status":"running"})), None);
        // hb is JSON null -> None
        assert_eq!(heartbeat_age(&Value::Null), None);
        // Empty updated_at string -> None
        assert_eq!(heartbeat_age(&json!({"updated_at":""})), None);
        // Fractional seconds rejected by strict format -> None
        assert_eq!(heartbeat_age(&json!({"updated_at":"2026-06-22T10:00:00.500Z"})), None);
        // Offset form rejected -> None
        assert_eq!(heartbeat_age(&json!({"updated_at":"2026-06-22T10:00:00+00:00"})), None);
        // Non-string updated_at -> None (Python TypeError)
        assert_eq!(heartbeat_age(&json!({"updated_at":1234567890})), None);

        // A timestamp ~3600s in the past should yield age ~3600 (positive). Build it from real now.
        let past = (Utc::now() - chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let age = heartbeat_age(&json!({ "updated_at": past })).unwrap();
        assert!((age - 3600.0).abs() < 5.0, "age was {age}");

        // Future timestamp -> negative age
        let future = (Utc::now() + chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let age = heartbeat_age(&json!({ "updated_at": future })).unwrap();
        assert!(age < 0.0 && (age + 3600.0).abs() < 5.0, "age was {age}");
    }

    #[test]
    fn heartbeat_age_accepts_single_digit_fields() {
        // Python strptime accepts non-zero-padded fields; chrono's numeric specifiers do too.
        // Just assert it parses (Some), matching the fidelity note in the spec.
        let ts = (Utc::now() - chrono::Duration::seconds(10))
            .format("%Y-%-m-%-dT%-H:%-M:%-SZ")
            .to_string();
        assert!(heartbeat_age(&json!({ "updated_at": ts })).is_some());
    }

    #[test]
    fn round_half_even_matches_python() {
        // round(0.0625, 3) == 0.062 in CPython (half-to-even, 2 is even)
        assert_eq!(round_half_even(0.0625, 3), 0.062);
        assert_eq!(round_half_even(1.0 / 3.0, 3), 0.333);
        assert_eq!(round_half_even(0.75, 3), 0.75);
        // Divergent class the old x*1000 scale-then-round impl got WRONG: each ratio's *1000 product
        // snaps to an exact .5 that masks the true binary value, which CPython's round() rounds by.
        // python -c "print(round(1/80,3),round(3/80,3),round(7/80,3),round(9/80,3))" -> 0.013 0.037 0.087 0.113
        assert_eq!(round_half_even(1.0 / 80.0, 3), 0.013);
        assert_eq!(round_half_even(3.0 / 80.0, 3), 0.037);
        assert_eq!(round_half_even(7.0 / 80.0, 3), 0.087);
        assert_eq!(round_half_even(9.0 / 80.0, 3), 0.113);
    }
}
