//! The per-kind probe evaluators — deterministic Rust only, NO LLM anywhere.
//!
//! Every evaluator maps a probe config (a lenient `serde_json::Value` from ops.json) to a
//! [`ProbeOutcome`]: a green/yellow/red [`Status`] plus the observed value, the threshold it was
//! judged against, and a short human detail line. Evaluators are best-effort by contract: a
//! missing file, an unreadable/corrupt database, or a spawn failure is a YELLOW "unobservable"
//! verdict — never a panic, never a fabricated green (an unobservable product is not a healthy
//! product, and it is not a dead one either).
//!
//! Kinds: file_age, file_exists, json_field, jsonl_tail, log_grep, process, sqlite_query, cmd,
//! http_get, git_sha_match. Process matching is by NAME + EXECUTABLE PATH via PowerShell
//! Get-CimInstance Win32_Process (wmic does not exist on this machine) — NEVER by PID; the fleet's
//! heartbeat PIDs are known to recycle.

use super::registry;
use super::Status;
use crate::control::proc;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One evaluated probe: the verdict color plus the evidence it was derived from.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub status: Status,
    /// Machine-readable observation (age seconds, match count, bool, raw value...).
    pub value: Value,
    /// Compact human threshold string ("yellow>43200s red>86400s", "red_on>=3", "== true", ...).
    pub threshold: String,
    /// One human line of evidence ("age 47.2h (newest 2026-06-30T00:03:55Z)", "not running", ...).
    pub detail: String,
    /// Set by kill/breaker-class probes: a red here must NEVER be answered with a blind restart.
    pub restart_forbidden: bool,
}

impl ProbeOutcome {
    fn new(status: Status, value: Value, threshold: String, detail: String) -> Self {
        ProbeOutcome {
            status,
            value,
            threshold,
            detail,
            restart_forbidden: false,
        }
    }
}

/// The best-effort contract: whatever we cannot observe is YELLOW "unobservable", never a crash
/// and never a green. (A missing post_registry.json says nothing good about posting.)
fn unobservable(threshold: &str, why: impl std::fmt::Display) -> ProbeOutcome {
    ProbeOutcome::new(
        Status::Yellow,
        Value::Null,
        threshold.to_string(),
        format!("unobservable: {why}"),
    )
}

/// A misconfigured probe (unknown kind, missing required field). Yellow — loud in the verdict, but
/// one bad registry line must not red-flag the whole project or kill the sweep.
fn misconfigured(why: impl std::fmt::Display) -> ProbeOutcome {
    ProbeOutcome::new(
        Status::Yellow,
        Value::Null,
        String::new(),
        format!("misconfigured: {why}"),
    )
}

// --------------------------------------------------------------------------- //
// config getters (lenient, mirroring the control/registry.rs style)
// --------------------------------------------------------------------------- //

fn cfg_str<'a>(cfg: &'a Value, key: &str) -> Option<&'a str> {
    cfg.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn cfg_f64(cfg: &Value, key: &str) -> Option<f64> {
    cfg.get(key).and_then(Value::as_f64)
}

fn cfg_i64(cfg: &Value, key: &str) -> Option<i64> {
    cfg.get(key).and_then(Value::as_i64)
}

/// The consecutive-fail threshold (http_get-style probes): raw reds are held at yellow until this
/// many consecutive sweeps have failed. Read by outcomes::evolve, not by the evaluators.
pub fn red_after_consecutive(cfg: &Value) -> Option<i64> {
    cfg_i64(cfg, "red_after_consecutive").filter(|n| *n > 0)
}

// --------------------------------------------------------------------------- //
// timestamps + age thresholds (shared by file_age / json_field / jsonl_tail / sqlite age modes)
// --------------------------------------------------------------------------- //

/// Parse a timestamp Value: epoch seconds (number), RFC3339 ("...Z" / offset / fractional), or a
/// NAIVE "YYYY-MM-DDTHH:MM:SS[.f]" (interpreted as LOCAL time — Sover's post_registry published_at
/// is written naive-local). None when unparseable.
pub fn parse_ts(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => {
            let secs = n.as_f64()?;
            DateTime::<Utc>::from_timestamp(secs as i64, 0)
        }
        Value::String(s) => parse_ts_str(s.trim()),
        _ => None,
    }
}

fn parse_ts_str(s: &str) -> Option<DateTime<Utc>> {
    if s.is_empty() {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    // Naive local ("2026-06-13T13:20:32" / with fractional seconds): resolve via the local zone;
    // an ambiguous/nonexistent local instant (DST edge) falls back to interpreting as UTC.
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        use chrono::TimeZone;
        return match chrono::Local.from_local_datetime(&naive) {
            chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => {
                Some(t.with_timezone(&Utc))
            }
            chrono::LocalResult::None => Some(naive.and_utc()),
        };
    }
    // Epoch seconds serialized as a string.
    if let Ok(secs) = s.parse::<f64>() {
        return DateTime::<Utc>::from_timestamp(secs as i64, 0);
    }
    None
}

/// Age in seconds of `t` (clock skew / future timestamps clamp to 0 — "fresher than fresh").
fn age_s(t: DateTime<Utc>) -> f64 {
    ((Utc::now() - t).num_milliseconds() as f64 / 1000.0).max(0.0)
}

/// The age -> color mapping used by every recency probe: red when past red_after_s, else yellow
/// when past yellow_after_s, else green. Missing thresholds simply don't fire.
pub fn age_status(age_s: f64, yellow_after_s: Option<f64>, red_after_s: Option<f64>) -> Status {
    if let Some(r) = red_after_s {
        if age_s > r {
            return Status::Red;
        }
    }
    if let Some(y) = yellow_after_s {
        if age_s > y {
            return Status::Yellow;
        }
    }
    Status::Green
}

fn age_threshold_str(yellow: Option<f64>, red: Option<f64>) -> String {
    match (yellow, red) {
        (Some(y), Some(r)) => format!("yellow>{y}s red>{r}s"),
        (Some(y), None) => format!("yellow>{y}s"),
        (None, Some(r)) => format!("red>{r}s"),
        (None, None) => "(no age thresholds)".to_string(),
    }
}

/// Build the age-mode outcome from a resolved timestamp.
fn age_outcome(cfg: &Value, t: DateTime<Utc>, what: &str) -> ProbeOutcome {
    let yellow = cfg_f64(cfg, "yellow_after_s");
    let red = cfg_f64(cfg, "red_after_s");
    let a = age_s(t);
    ProbeOutcome::new(
        age_status(a, yellow, red),
        json!(a),
        age_threshold_str(yellow, red),
        format!(
            "age {:.1}h ({} {})",
            a / 3600.0,
            what,
            t.format("%Y-%m-%dT%H:%M:%SZ")
        ),
    )
}

// --------------------------------------------------------------------------- //
// dispatch
// --------------------------------------------------------------------------- //

/// Evaluate one probe config against the live filesystem. `repo_path` is the BY-NAME repos.json
/// join for this project ("" when unregistered) — used by ${REPO} paths and git_sha_match.
/// Total: any unexpected shape degrades to a yellow misconfigured/unobservable verdict.
pub fn evaluate(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let kind = match cfg_str(cfg, "kind") {
        Some(k) => k,
        None => return misconfigured("probe has no kind"),
    };
    match kind {
        "file_age" => eval_file_age(cfg, repo_path),
        "file_exists" => eval_file_exists(cfg, repo_path),
        "json_field" => eval_json_field(cfg, repo_path),
        "jsonl_tail" => eval_jsonl_tail(cfg, repo_path),
        "log_grep" => eval_log_grep(cfg, repo_path),
        "process" => eval_process(cfg),
        "sqlite_query" => eval_sqlite_query(cfg, repo_path),
        "cmd" => eval_cmd(cfg, repo_path),
        "http_get" => eval_http_get(cfg),
        "git_sha_match" => eval_git_sha_match(cfg, repo_path),
        other => misconfigured(format!("unknown probe kind '{other}'")),
    }
}

// result_large_err: the Err IS the probe verdict (a ProbeOutcome), constructed once per probe per
// sweep — not a hot path worth boxing.
#[allow(clippy::result_large_err)]
fn resolved_file(cfg: &Value, repo_path: &str) -> Result<PathBuf, ProbeOutcome> {
    match cfg_str(cfg, "file") {
        Some(f) => Ok(registry::resolve_path(f, repo_path)),
        None => Err(misconfigured("probe has no 'file'")),
    }
}

// --------------------------------------------------------------------------- //
// file_age / file_exists
// --------------------------------------------------------------------------- //

/// file_age: newest mtime among the glob matches (or the single path) vs age thresholds.
/// The glob is deliberately minimal — a single `*` in the FILENAME component (e.g. `final/*.mp4`),
/// which is all the seed registry needs. No matches / missing dir -> unobservable.
fn eval_file_age(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let path = match resolved_file(cfg, repo_path) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let threshold = age_threshold_str(cfg_f64(cfg, "yellow_after_s"), cfg_f64(cfg, "red_after_s"));
    let newest = match newest_mtime(&path) {
        Some(m) => m,
        None => return unobservable(&threshold, format!("no files match {}", path.display())),
    };
    let t: DateTime<Utc> = newest.into();
    age_outcome(cfg, t, "newest mtime")
}

/// Newest modification time among `path`'s matches. A `*` in the final component fans out over the
/// parent directory (prefix/suffix match); otherwise the literal file's mtime.
fn newest_mtime(path: &Path) -> Option<std::time::SystemTime> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if let Some(star) = name.find('*') {
        let (prefix, suffix) = (&name[..star], &name[star + 1..]);
        let dir = path.parent()?;
        let mut newest: Option<std::time::SystemTime> = None;
        for e in std::fs::read_dir(dir).ok()?.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with(prefix)
                && n.ends_with(suffix)
                && n.len() >= prefix.len() + suffix.len()
            {
                if let Ok(md) = e.metadata() {
                    if let Ok(m) = md.modified() {
                        if newest.map(|cur| m > cur).unwrap_or(true) {
                            newest = Some(m);
                        }
                    }
                }
            }
        }
        newest
    } else {
        std::fs::metadata(path).and_then(|m| m.modified()).ok()
    }
}

/// file_exists: present -> green, absent -> red (here absence IS the signal, not unobservability).
fn eval_file_exists(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let path = match resolved_file(cfg, repo_path) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let exists = path.exists();
    ProbeOutcome::new(
        if exists { Status::Green } else { Status::Red },
        json!(exists),
        "file present".to_string(),
        format!(
            "{} {}",
            path.display(),
            if exists { "present" } else { "MISSING" }
        ),
    )
}

// --------------------------------------------------------------------------- //
// json_field / jsonl_tail
// --------------------------------------------------------------------------- //

/// json_field: read a JSON file, walk a dotted path (a `*` segment fans out over object values /
/// array items — e.g. `*.published_at` over post_registry's "id:platform" dict), then judge:
///   - mode "age" (default): newest parsable timestamp among matches vs yellow/red_after_s
///   - mode "equals": the value must equal cfg.equals, else red
///   - mode "array_len": the array length vs red_below (len < red_below -> red)
fn eval_json_field(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let path = match resolved_file(cfg, repo_path) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let dotted = match cfg_str(cfg, "path") {
        Some(p) => p,
        None => return misconfigured("json_field has no 'path'"),
    };
    let mode = cfg_str(cfg, "mode").unwrap_or("age");
    let threshold = match mode {
        "equals" => format!("== {}", cfg.get("equals").unwrap_or(&Value::Null)),
        "array_len" => format!("len>={}", cfg_i64(cfg, "red_below").unwrap_or(0)),
        _ => age_threshold_str(cfg_f64(cfg, "yellow_after_s"), cfg_f64(cfg, "red_after_s")),
    };
    let root: Value = match std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(v) => v,
        None => return unobservable(&threshold, format!("missing/unparsable {}", path.display())),
    };
    let hits = walk_path(&root, dotted);
    if hits.is_empty() {
        return unobservable(
            &threshold,
            format!("path '{dotted}' not found in {}", path.display()),
        );
    }
    match mode {
        "equals" => {
            let expect = cfg.get("equals").cloned().unwrap_or(Value::Null);
            let got = hits[0].clone();
            let ok = got == expect;
            ProbeOutcome::new(
                if ok { Status::Green } else { Status::Red },
                got.clone(),
                threshold,
                format!("{dotted} = {got}{}", if ok { "" } else { " (MISMATCH)" }),
            )
        }
        "array_len" => {
            let len = hits[0].as_array().map(|a| a.len() as i64);
            match len {
                Some(n) => {
                    let red_below = cfg_i64(cfg, "red_below").unwrap_or(0);
                    ProbeOutcome::new(
                        if n < red_below {
                            Status::Red
                        } else {
                            Status::Green
                        },
                        json!(n),
                        threshold,
                        format!("{dotted} len={n}"),
                    )
                }
                None => unobservable(&threshold, format!("'{dotted}' is not an array")),
            }
        }
        // "age" (default): the NEWEST parsable timestamp among the matches — a registry of many
        // posts is fresh if ANY entry is fresh.
        _ => {
            let newest = hits.iter().filter_map(parse_ts).max();
            match newest {
                Some(t) => age_outcome(cfg, t, dotted),
                None => unobservable(&threshold, format!("no parsable timestamp at '{dotted}'")),
            }
        }
    }
}

/// Walk `dotted` ("a.b.c"; a `*` segment fans out over object values / array items). Returns every
/// matched leaf. Missing keys prune silently (empty result = path not found).
pub fn walk_path(root: &Value, dotted: &str) -> Vec<Value> {
    let mut current = vec![root.clone()];
    for seg in dotted.split('.') {
        let mut next = Vec::new();
        for v in current {
            if seg == "*" {
                match v {
                    Value::Object(o) => next.extend(o.into_iter().map(|(_, x)| x)),
                    Value::Array(a) => next.extend(a),
                    _ => {}
                }
            } else if let Some(x) = v.get(seg) {
                next.push(x.clone());
            }
        }
        current = next;
    }
    current
}

/// jsonl_tail: the last records of an append-only JSONL file (corrupt lines skipped, same lenient
/// parse as watchdog::recent_snapshots). Modes:
///   - "age" (default): the LAST record's `field` as a timestamp vs age thresholds
///   - "streak": the last `last_n` records; if there are at least last_n and NONE has
///     `field == want` -> yellow (the lane.outcome_streak rule: 5 non-shipped in a row).
///     Fewer records than last_n -> green (insufficient evidence, no false alarm on a fresh lane).
fn eval_jsonl_tail(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let path = match resolved_file(cfg, repo_path) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let field = match cfg_str(cfg, "field") {
        Some(f) => f,
        None => return misconfigured("jsonl_tail has no 'field'"),
    };
    let mode = cfg_str(cfg, "mode").unwrap_or("age");
    let threshold = match mode {
        "streak" => format!(
            "last {} all {} != {:?} -> yellow",
            cfg_i64(cfg, "last_n").unwrap_or(5),
            field,
            cfg_str(cfg, "want").unwrap_or("shipped")
        ),
        _ => age_threshold_str(cfg_f64(cfg, "yellow_after_s"), cfg_f64(cfg, "red_after_s")),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return unobservable(&threshold, format!("missing {}", path.display())),
    };
    let records: Vec<Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object)
        .collect();
    if records.is_empty() {
        return unobservable(&threshold, format!("no records in {}", path.display()));
    }
    match mode {
        "streak" => {
            let n = cfg_i64(cfg, "last_n").unwrap_or(5).max(1) as usize;
            let want = cfg_str(cfg, "want").unwrap_or("shipped");
            if records.len() < n {
                return ProbeOutcome::new(
                    Status::Green,
                    json!(records.len()),
                    threshold,
                    format!("only {} record(s) — streak not judged", records.len()),
                );
            }
            let last_n = &records[records.len() - n..];
            let hits = last_n
                .iter()
                .filter(|r| r.get(field).and_then(Value::as_str) == Some(want))
                .count();
            ProbeOutcome::new(
                if hits == 0 {
                    Status::Yellow
                } else {
                    Status::Green
                },
                json!({"window": n, "matching": hits}),
                threshold,
                format!("last {n}: {hits}x {field}={want}"),
            )
        }
        _ => {
            let last = &records[records.len() - 1];
            match last.get(field).and_then(parse_ts) {
                Some(t) => age_outcome(cfg, t, &format!("last .{field}")),
                None => unobservable(&threshold, format!("last record has no parsable '{field}'")),
            }
        }
    }
}

// --------------------------------------------------------------------------- //
// log_grep
// --------------------------------------------------------------------------- //

/// log_grep: regex match-count over the tail `tail_bytes` (default 64 KiB) of a log file.
///   - `pattern` + `red_on`:  count(pattern) >= red_on -> red
///   - `pattern` + `yellow_on` (no yellow_pattern): count(pattern) >= yellow_on -> yellow
///   - `yellow_pattern` + `yellow_on`: an INDEPENDENT yellow signal (auth_health: 401s red-scale,
///     429s yellow-scale in one probe)
///
/// Below every threshold -> green (transients tolerated by design). Missing file -> unobservable.
fn eval_log_grep(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let path = match resolved_file(cfg, repo_path) {
        Ok(p) => p,
        Err(o) => return o,
    };
    let pattern = match cfg_str(cfg, "pattern") {
        Some(p) => p,
        None => return misconfigured("log_grep has no 'pattern'"),
    };
    let red_on = cfg_i64(cfg, "red_on");
    let yellow_on = cfg_i64(cfg, "yellow_on");
    let yellow_pattern = cfg_str(cfg, "yellow_pattern");
    let threshold = format!(
        "red_on>={} yellow_on>={}",
        red_on.map(|n| n.to_string()).unwrap_or_else(|| "-".into()),
        yellow_on
            .map(|n| n.to_string())
            .unwrap_or_else(|| "-".into()),
    );
    let tail_bytes = cfg_i64(cfg, "tail_bytes").unwrap_or(65536).max(0) as usize;
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return unobservable(&threshold, format!("missing {}", path.display())),
    };
    let start = bytes.len().saturating_sub(tail_bytes);
    let tail = String::from_utf8_lossy(&bytes[start..]);
    grep_outcome(
        &tail,
        pattern,
        red_on,
        yellow_pattern,
        yellow_on,
        &threshold,
    )
}

/// Pure core of log_grep (unit-tested over fixture strings).
pub fn grep_outcome(
    tail: &str,
    pattern: &str,
    red_on: Option<i64>,
    yellow_pattern: Option<&str>,
    yellow_on: Option<i64>,
    threshold: &str,
) -> ProbeOutcome {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return misconfigured(format!("bad pattern: {e}")),
    };
    let count = re.find_iter(tail).count() as i64;
    let ycount = match yellow_pattern {
        Some(yp) => match regex::Regex::new(yp) {
            Ok(r) => r.find_iter(tail).count() as i64,
            Err(e) => return misconfigured(format!("bad yellow_pattern: {e}")),
        },
        None => count,
    };
    let status = if red_on.map(|n| count >= n).unwrap_or(false) {
        Status::Red
    } else if yellow_on.map(|n| ycount >= n).unwrap_or(false) {
        Status::Yellow
    } else {
        Status::Green
    };
    ProbeOutcome::new(
        status,
        json!({"matches": count, "yellow_matches": ycount}),
        threshold.to_string(),
        format!(
            "{count} match(es) in tail{}",
            if yellow_pattern.is_some() {
                format!(", {ycount} yellow-pattern")
            } else {
                String::new()
            }
        ),
    )
}

// --------------------------------------------------------------------------- //
// process
// --------------------------------------------------------------------------- //

/// process: NAME + EXECUTABLE-PATH match via PowerShell Get-CimInstance Win32_Process (wmic is
/// gone from this Win11; PIDs recycle so they are NEVER matched). Running -> green, absent -> red,
/// PowerShell failure -> unobservable. Spawned via control::proc::run (hidden window — the
/// no-console-window convention).
fn eval_process(cfg: &Value) -> ProbeOutcome {
    let name = match cfg_str(cfg, "process_name") {
        Some(n) => n,
        None => return misconfigured("process probe has no 'process_name'"),
    };
    let path_contains = cfg_str(cfg, "path_contains").unwrap_or("");
    let threshold = format!("process {name} @ *{path_contains}*");
    // Single-quote the name inside the CIM filter; embedded quotes are doubled (CIM escape).
    let filter = format!("Name='{}'", name.replace('\'', "''"));
    let ps = format!(
        "Get-CimInstance Win32_Process -Filter \"{filter}\" | ForEach-Object {{ $_.ExecutablePath }}"
    );
    let r = match proc::run(
        &[
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            ps.as_str(),
        ],
        None,
        Some(Duration::from_secs(30)),
    ) {
        Ok(r) => r,
        Err(e) => return unobservable(&threshold, format!("powershell spawn failed: {e}")),
    };
    if r.code != 0 {
        return unobservable(&threshold, format!("Get-CimInstance exit {}", r.code));
    }
    let found = process_found(&r.stdout, path_contains);
    ProbeOutcome::new(
        if found { Status::Green } else { Status::Red },
        json!(found),
        threshold,
        if found {
            format!("{name} running")
        } else {
            format!("{name} NOT running")
        },
    )
}

/// Pure path-match core of the process probe: any non-empty ExecutablePath line containing
/// `path_contains` (case-insensitive, / and \ unified). Empty `path_contains` = any live instance.
pub fn process_found(ps_stdout: &str, path_contains: &str) -> bool {
    let needle = path_contains.to_lowercase().replace('/', "\\");
    ps_stdout.lines().any(|l| {
        let line = l.trim().to_lowercase().replace('/', "\\");
        !line.is_empty() && line.contains(&needle)
    })
}

// --------------------------------------------------------------------------- //
// sqlite_query
// --------------------------------------------------------------------------- //

/// sqlite_query: a read-only single-value SELECT (rusqlite, SQLITE_OPEN_READ_ONLY — the ops plane
/// can never write a managed product's database). Modes: "age" (result is a timestamp), "equals"
/// (result must equal cfg.equals else red), "number" (>= red_at -> red, >= yellow_at -> yellow).
/// A NULL result maps via `null_status` ("green" for "no open trades is fine", default yellow
/// unobservable). Query errors — including a CORRUPT database (asmodeus.db is partially malformed
/// today) — are unobservable, never a panic. `restart_forbidden_on_red: true` (kill/breaker class)
/// stamps the verdict so Phase 2 recovery can never blind-restart over an engaged kill-switch.
fn eval_sqlite_query(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let db = match cfg_str(cfg, "db") {
        Some(d) => registry::resolve_path(d, repo_path),
        None => return misconfigured("sqlite_query has no 'db'"),
    };
    let query = match cfg_str(cfg, "query") {
        Some(q) => q,
        None => return misconfigured("sqlite_query has no 'query'"),
    };
    let mode = cfg_str(cfg, "mode").unwrap_or("age");
    let threshold = match mode {
        "equals" => format!("== {}", cfg.get("equals").unwrap_or(&Value::Null)),
        "number" => format!(
            "yellow_at>={} red_at>={}",
            cfg_f64(cfg, "yellow_at")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            cfg_f64(cfg, "red_at")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
        _ => age_threshold_str(cfg_f64(cfg, "yellow_after_s"), cfg_f64(cfg, "red_after_s")),
    };
    if !db.exists() {
        return unobservable(&threshold, format!("missing db {}", db.display()));
    }
    let got = match sqlite_single_value(&db, query) {
        Ok(v) => v,
        Err(e) => return unobservable(&threshold, format!("query failed: {e}")),
    };
    if got.is_null() {
        return match cfg_str(cfg, "null_status") {
            Some("green") => ProbeOutcome::new(
                Status::Green,
                Value::Null,
                threshold,
                "NULL result (configured green)".to_string(),
            ),
            _ => unobservable(&threshold, "query returned NULL"),
        };
    }
    let mut out = match mode {
        "equals" => {
            let expect = cfg.get("equals").cloned().unwrap_or(Value::Null);
            let ok = sqlite_value_eq(&got, &expect);
            ProbeOutcome::new(
                if ok { Status::Green } else { Status::Red },
                got.clone(),
                threshold,
                format!("result = {got}{}", if ok { "" } else { " (MISMATCH)" }),
            )
        }
        "number" => match got.as_f64() {
            Some(n) => {
                let red_at = cfg_f64(cfg, "red_at");
                let yellow_at = cfg_f64(cfg, "yellow_at");
                let status = if red_at.map(|r| n >= r).unwrap_or(false) {
                    Status::Red
                } else if yellow_at.map(|y| n >= y).unwrap_or(false) {
                    Status::Yellow
                } else {
                    Status::Green
                };
                ProbeOutcome::new(status, got.clone(), threshold, format!("result = {n}"))
            }
            None => unobservable(&threshold, format!("non-numeric result {got}")),
        },
        _ => match parse_ts(&got) {
            Some(t) => age_outcome(cfg, t, "query result"),
            None => unobservable(&threshold, format!("unparsable timestamp {got}")),
        },
    };
    if out.status == Status::Red
        && cfg
            .get("restart_forbidden_on_red")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        out.restart_forbidden = true;
    }
    out
}

/// Numeric-tolerant equality for SQLite results (INTEGER 0 must equal a JSON 0 even when column
/// affinity hands back a float, and "0" stays distinct from 0 — no string coercion).
fn sqlite_value_eq(got: &Value, expect: &Value) -> bool {
    if got == expect {
        return true;
    }
    match (got.as_f64(), expect.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// First column of the first row, as a JSON value. Read-only open — no create, no write, ever.
fn sqlite_single_value(db: &Path, query: &str) -> Result<Value, String> {
    use rusqlite::types::ValueRef;
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    // Bound the wait on a writer-locked live db so a sweep can't wedge behind a transaction.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let mut stmt = conn.prepare(query).map_err(|e| e.to_string())?;
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    match rows.next().map_err(|e| e.to_string())? {
        Some(row) => {
            let v = row.get_ref(0).map_err(|e| e.to_string())?;
            Ok(match v {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => json!(i),
                ValueRef::Real(f) => json!(f),
                ValueRef::Text(t) => json!(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => json!(format!("<{} blob bytes>", b.len())),
            })
        }
        None => Ok(Value::Null),
    }
}

// --------------------------------------------------------------------------- //
// cmd
// --------------------------------------------------------------------------- //

/// cmd: run a shell command via control::proc::run (hidden window, scrubbed env, bounded by
/// `timeout_s`, default 60). Modes:
///   - "exit_code" (default): exit 0 -> green, non-zero -> red
///   - "number": stdout parsed as a number, >= red_at -> red, >= yellow_at -> yellow
///   - "json_array_len": stdout parsed as a JSON array (covers `gh pr list --json number`),
///     length judged like "number"
///
/// Spawn failure / non-zero exit in a parse mode -> unobservable.
fn eval_cmd(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let command = match cfg_str(cfg, "command") {
        Some(c) => c.replace("${REPO}", repo_path),
        None => return misconfigured("cmd probe has no 'command'"),
    };
    let mode = cfg_str(cfg, "mode").unwrap_or("exit_code");
    let threshold = match mode {
        "exit_code" => "exit 0".to_string(),
        _ => format!(
            "yellow_at>={} red_at>={}",
            cfg_f64(cfg, "yellow_at")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            cfg_f64(cfg, "red_at")
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
        ),
    };
    let timeout = Duration::from_secs(cfg_i64(cfg, "timeout_s").unwrap_or(60).max(1) as u64);
    let argv: Vec<&str> = if cfg!(windows) {
        vec!["cmd", "/C", command.as_str()]
    } else {
        vec!["/bin/sh", "-c", command.as_str()]
    };
    let r = match proc::run(&argv, None, Some(timeout)) {
        Ok(r) => r,
        Err(e) => return unobservable(&threshold, format!("spawn failed: {e}")),
    };
    match mode {
        "number" | "json_array_len" => {
            if r.code != 0 {
                return unobservable(&threshold, format!("command exit {}", r.code));
            }
            let n: Option<f64> = if mode == "number" {
                r.stdout.trim().parse::<f64>().ok()
            } else {
                serde_json::from_str::<Value>(r.stdout.trim())
                    .ok()
                    .and_then(|v| v.as_array().map(|a| a.len() as f64))
            };
            match n {
                Some(n) => {
                    let red_at = cfg_f64(cfg, "red_at");
                    let yellow_at = cfg_f64(cfg, "yellow_at");
                    let status = if red_at.map(|x| n >= x).unwrap_or(false) {
                        Status::Red
                    } else if yellow_at.map(|x| n >= x).unwrap_or(false) {
                        Status::Yellow
                    } else {
                        Status::Green
                    };
                    ProbeOutcome::new(status, json!(n), threshold, format!("value = {n}"))
                }
                None => unobservable(&threshold, "stdout not parsable"),
            }
        }
        _ => ProbeOutcome::new(
            if r.code == 0 {
                Status::Green
            } else {
                Status::Red
            },
            json!(r.code),
            threshold,
            format!("exit {}", r.code),
        ),
    }
}

// --------------------------------------------------------------------------- //
// http_get
// --------------------------------------------------------------------------- //

/// http_get: a bounded localhost-only GET over std::net::TcpStream (the serve-health pattern — no
/// reqwest, no async). HTTP 200 -> green; anything else (connect refused, timeout, non-200) is a
/// RAW red — outcomes::evolve holds it at yellow until `red_after_consecutive` sweeps have failed
/// (a single blip on CDP :9242 is not an incident; three straight is).
fn eval_http_get(cfg: &Value) -> ProbeOutcome {
    let url = match cfg_str(cfg, "url") {
        Some(u) => u,
        None => return misconfigured("http_get has no 'url'"),
    };
    let threshold = format!(
        "HTTP 200 (red after {} consecutive fails)",
        red_after_consecutive(cfg).unwrap_or(1)
    );
    let timeout = Duration::from_secs(cfg_i64(cfg, "timeout_s").unwrap_or(3).max(1) as u64);
    match http_get_status(url, timeout) {
        Ok(200) => ProbeOutcome::new(Status::Green, json!(200), threshold, "HTTP 200".to_string()),
        Ok(code) => ProbeOutcome::new(Status::Red, json!(code), threshold, format!("HTTP {code}")),
        Err(e) => ProbeOutcome::new(
            Status::Red,
            Value::Null,
            threshold,
            format!("no response: {e}"),
        ),
    }
}

/// Minimal loopback-only HTTP GET: parse `http://127.0.0.1:PORT/path`, connect with a timeout,
/// send the request, read the status line. Non-loopback hosts are refused (the ops plane probes
/// local daemons, it is not an HTTP client).
pub fn http_get_status(url: &str, timeout: Duration) -> Result<u16, String> {
    use std::io::{Read, Write};
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// URLs supported")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().map_err(|e| e.to_string())?),
        None => (hostport, 80),
    };
    if host != "127.0.0.1" && host != "localhost" {
        return Err("only localhost probes allowed".to_string());
    }
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e: std::net::AddrParseError| e.to_string())?;
    let mut stream =
        std::net::TcpStream::connect_timeout(&addr, timeout).map_err(|e| e.to_string())?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&buf[..n]);
    // "HTTP/1.1 200 OK" -> 200
    head.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| "malformed status line".to_string())
}

// --------------------------------------------------------------------------- //
// git_sha_match
// --------------------------------------------------------------------------- //

/// git_sha_match: the deploy-gap probe — compare a heartbeat's `git_sha` (the sha the RUNNING
/// binary was built from) against `git -C <repo> rev-parse --short HEAD`. Prefix-tolerant (short
/// shas vary in length). Different -> YELLOW (running old code is a drift warning, not an outage).
/// Missing heartbeat / git failure -> unobservable.
fn eval_git_sha_match(cfg: &Value, repo_path: &str) -> ProbeOutcome {
    let hb_path = match cfg_str(cfg, "heartbeat_file") {
        Some(f) => registry::resolve_path(f, repo_path),
        None => return misconfigured("git_sha_match has no 'heartbeat_file'"),
    };
    let field = cfg_str(cfg, "sha_field").unwrap_or("git_sha");
    let threshold = "heartbeat sha == repo HEAD".to_string();
    if repo_path.is_empty() || !Path::new(repo_path).is_dir() {
        return unobservable(&threshold, "project has no repos.json path");
    }
    let hb: Value = match std::fs::read(&hb_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(v) => v,
        None => {
            return unobservable(
                &threshold,
                format!("missing/unparsable {}", hb_path.display()),
            )
        }
    };
    let sha = match hb
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(s) => s.to_lowercase(),
        None => return unobservable(&threshold, format!("heartbeat has no '{field}'")),
    };
    let r = match proc::run(
        &["git", "-C", repo_path, "rev-parse", "--short", "HEAD"],
        None,
        Some(Duration::from_secs(30)),
    ) {
        Ok(r) => r,
        Err(e) => return unobservable(&threshold, format!("git spawn failed: {e}")),
    };
    if r.code != 0 {
        return unobservable(&threshold, format!("git rev-parse exit {}", r.code));
    }
    let head = r.stdout.trim().to_lowercase();
    let matched = !head.is_empty() && (sha.starts_with(&head) || head.starts_with(&sha));
    ProbeOutcome::new(
        if matched {
            Status::Green
        } else {
            Status::Yellow
        },
        json!({"heartbeat": sha, "head": head}),
        threshold,
        if matched {
            format!("binary current ({sha})")
        } else {
            format!("binary {sha} != HEAD {head} (deploy gap)")
        },
    )
}

// --------------------------------------------------------------------------- //
// tests — golden vectors over temp-dir fixtures (hermetic; no live fleet dependence)
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("solomon_ops_probe_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().into_owned()
    }

    // -------- age_status mapping (the shared recency rule) --------
    #[test]
    fn age_status_vectors() {
        assert_eq!(age_status(10.0, Some(100.0), Some(200.0)), Status::Green);
        assert_eq!(age_status(150.0, Some(100.0), Some(200.0)), Status::Yellow);
        assert_eq!(age_status(250.0, Some(100.0), Some(200.0)), Status::Red);
        // missing thresholds simply don't fire
        assert_eq!(age_status(1e9, None, None), Status::Green);
        assert_eq!(age_status(250.0, None, Some(200.0)), Status::Red);
        assert_eq!(age_status(150.0, Some(100.0), None), Status::Yellow);
    }

    // -------- parse_ts: epoch / RFC3339-Z / fractional / naive-local / junk --------
    #[test]
    fn parse_ts_vectors() {
        // epoch seconds (the fleet_ticker heartbeat "t" field)
        assert!(parse_ts(&json!(1782942951)).is_some());
        // RFC3339 Z (lane_state last_run)
        assert!(parse_ts(&json!("2026-07-01T21:38:59Z")).is_some());
        // fractional + Z (venue_fills created_at)
        assert!(parse_ts(&json!("2026-06-30T00:03:55.500158Z")).is_some());
        // NAIVE local (post_registry published_at) — must parse, not None
        assert!(parse_ts(&json!("2026-06-13T13:20:32")).is_some());
        // junk
        assert!(parse_ts(&json!("not a date")).is_none());
        assert!(parse_ts(&json!(null)).is_none());
        assert!(parse_ts(&json!("")).is_none());
    }

    // -------- file_age: fresh green, old red (mtimes via filetime), glob newest-wins --------
    #[test]
    fn file_age_thresholds_and_glob() {
        let d = tdir("file_age");
        let old = d.join("old.mp4");
        let fresh = d.join("fresh.mp4");
        std::fs::write(&old, "x").unwrap();
        std::fs::write(&fresh, "x").unwrap();
        let two_days_ago = filetime::FileTime::from_unix_time(
            (Utc::now() - chrono::Duration::seconds(2 * 86400)).timestamp(),
            0,
        );
        filetime::set_file_mtime(&old, two_days_ago).unwrap();

        // single old file vs red>24h -> red
        let cfg = json!({"kind": "file_age", "file": s(&old), "red_after_s": 86400});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // glob: the NEWEST mtime wins -> fresh file -> green
        let cfg = json!({"kind": "file_age", "file": s(&d.join("*.mp4")), "red_after_s": 86400});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // no matches -> unobservable yellow, not a crash
        let cfg = json!({"kind": "file_age", "file": s(&d.join("*.avi")), "red_after_s": 86400});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("unobservable"), "{}", out.detail);
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- file_exists --------
    #[test]
    fn file_exists_present_green_absent_red() {
        let d = tdir("file_exists");
        let f = d.join("x.txt");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(
            evaluate(&json!({"kind": "file_exists", "file": s(&f)}), "").status,
            Status::Green
        );
        assert_eq!(
            evaluate(
                &json!({"kind": "file_exists", "file": s(&d.join("gone"))}),
                ""
            )
            .status,
            Status::Red
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- json_field: star-path age over a post_registry-shaped dict --------
    #[test]
    fn json_field_star_path_age_over_registry_dict() {
        let d = tdir("json_field_star");
        let f = d.join("post_registry.json");
        let fresh = (Utc::now() - chrono::Duration::seconds(3600))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(
            &f,
            serde_json::to_string(&json!({
                "a:tiktok": {"published_at": "2026-01-01T00:00:00Z"},
                "b:instagram": {"published_at": fresh},
                "junk": {"no_ts_here": true}
            }))
            .unwrap(),
        )
        .unwrap();
        // newest entry is 1h old -> green under yellow>12h
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "*.published_at",
                         "yellow_after_s": 43200, "red_after_s": 86400});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Green, "{}", out.detail);
        // a registry whose newest is ancient -> red
        std::fs::write(
            &f,
            serde_json::to_string(&json!({"a:tiktok": {"published_at": "2026-01-01T00:00:00Z"}}))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn json_field_equals_and_array_len_and_missing() {
        let d = tdir("json_field_eq");
        let f = d.join("strategy.json");
        std::fs::write(
            &f,
            r#"{"auto_post": true, "platforms": ["a", "b"], "nested": {"t": 1}}"#,
        )
        .unwrap();
        // equals match -> green
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "auto_post", "mode": "equals", "equals": true});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // equals mismatch -> red (the autopost_gate rule)
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "auto_post", "mode": "equals", "equals": false});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // array_len below red_below -> red; at/above -> green
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "platforms", "mode": "array_len", "red_below": 3});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "platforms", "mode": "array_len", "red_below": 2});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // missing file -> unobservable yellow
        let cfg = json!({"kind": "json_field", "file": s(&d.join("gone.json")), "path": "x"});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("unobservable"));
        // missing path -> unobservable yellow
        let cfg = json!({"kind": "json_field", "file": s(&f), "path": "absent.key"});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn walk_path_vectors() {
        let v = json!({"a": {"b": {"c": 1}}, "list": [{"x": 1}, {"x": 2}]});
        assert_eq!(walk_path(&v, "a.b.c"), vec![json!(1)]);
        assert_eq!(walk_path(&v, "list.*.x"), vec![json!(1), json!(2)]);
        assert!(walk_path(&v, "a.zzz").is_empty());
    }

    // -------- jsonl_tail: age + streak --------
    #[test]
    fn jsonl_tail_age_of_last_record() {
        let d = tdir("jsonl_age");
        let f = d.join("lane.jsonl");
        let fresh = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(
            &f,
            format!(
                "{}\nnot json\n{}\n",
                json!({"ts": "2026-01-01T00:00:00Z"}),
                json!({"ts": fresh})
            ),
        )
        .unwrap();
        // LAST record is fresh -> green (corrupt middle line skipped leniently)
        let cfg = json!({"kind": "jsonl_tail", "file": s(&f), "field": "ts", "yellow_after_s": 600, "red_after_s": 1800});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Green, "{}", out.detail);
        // missing file -> unobservable
        let cfg = json!({"kind": "jsonl_tail", "file": s(&d.join("gone.jsonl")), "field": "ts"});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn jsonl_tail_streak_rule() {
        let d = tdir("jsonl_streak");
        let f = d.join("history.jsonl");
        let rec = |st: &str| json!({"status": st}).to_string();
        // 5 straight non-shipped -> yellow (the lane.outcome_streak rule)
        std::fs::write(
            &f,
            [
                rec("noop"),
                rec("reverted"),
                rec("noop"),
                rec("noop"),
                rec("reverted"),
            ]
            .join("\n"),
        )
        .unwrap();
        let cfg = json!({"kind": "jsonl_tail", "file": s(&f), "field": "status", "mode": "streak",
                         "want": "shipped", "last_n": 5});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        // a ship inside the window breaks the streak -> green
        std::fs::write(
            &f,
            [
                rec("noop"),
                rec("shipped"),
                rec("noop"),
                rec("noop"),
                rec("reverted"),
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // fewer than last_n records -> green (insufficient evidence, no false alarm)
        std::fs::write(&f, [rec("noop"), rec("noop")].join("\n")).unwrap();
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- log_grep --------
    #[test]
    fn log_grep_thresholds() {
        let t = "ok\n-> 401 denied\nJWT token expired\n-> 401 again\n-> 429 slow\n-> 429\n";
        let thr = "red_on>=3 yellow_on>=5";
        // 3 auth-red matches >= red_on 3 -> red
        let out = grep_outcome(
            t,
            "-> 401|JWT token expired",
            Some(3),
            Some("-> 429"),
            Some(5),
            thr,
        );
        assert_eq!(out.status, Status::Red);
        // only 2 429s (< yellow_on 5) and 3 401-class needed 4 -> green
        let out = grep_outcome(
            t,
            "-> 401|JWT token expired",
            Some(4),
            Some("-> 429"),
            Some(5),
            thr,
        );
        assert_eq!(out.status, Status::Green);
        // yellow-on-any-match (post_failures rule): one hit -> yellow
        let out = grep_outcome(
            "chrome binary not found\n",
            "failed after 3 attempts|chrome binary not found",
            None,
            None,
            Some(1),
            thr,
        );
        assert_eq!(out.status, Status::Yellow);
        // no hits -> green
        let out = grep_outcome(
            "all fine\n",
            "failed after 3 attempts",
            None,
            None,
            Some(1),
            thr,
        );
        assert_eq!(out.status, Status::Green);
        // a bad regex is misconfigured-yellow, not a panic
        let out = grep_outcome("x", "(unclosed", Some(1), None, None, thr);
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("misconfigured"));
    }

    #[test]
    fn log_grep_reads_only_the_tail() {
        let d = tdir("log_tail");
        let f = d.join("big.log");
        // one match buried at the START of a file, outside the 64-byte tail -> not counted
        let mut body = String::from("ERROR match here\n");
        body.push_str(&"padding line\n".repeat(50));
        std::fs::write(&f, &body).unwrap();
        let cfg = json!({"kind": "log_grep", "file": s(&f), "pattern": "ERROR", "yellow_on": 1, "tail_bytes": 64});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // widen the tail to cover the whole file -> counted -> yellow
        let cfg = json!({"kind": "log_grep", "file": s(&f), "pattern": "ERROR", "yellow_on": 1, "tail_bytes": 100000});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        // missing file -> unobservable
        let cfg = json!({"kind": "log_grep", "file": s(&d.join("gone.log")), "pattern": "x", "yellow_on": 1});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- process: pure path-match core (never PID) --------
    #[test]
    fn process_found_matches_by_path_never_pid() {
        let out = "C:\\Users\\C\\Desktop\\workspace\\projects\\sover\\dist\\Sover.exe\r\n\r\n";
        assert!(process_found(out, "projects\\sover\\dist"));
        // forward slashes in the needle unify to backslashes
        assert!(process_found(out, "projects/sover/dist"));
        // a different install path does NOT match (name-only would false-green a stray copy)
        assert!(!process_found(out, "projects\\asmodeus"));
        // empty stdout (no such process) never matches
        assert!(!process_found("", "anything"));
        assert!(!process_found("\r\n\r\n", ""));
        // empty needle = any live instance
        assert!(process_found(out, ""));
    }

    // -------- sqlite_query: temp db, all modes + corrupt/missing degradation --------
    #[test]
    fn sqlite_query_modes_against_temp_db() {
        let d = tdir("sqlite");
        let db = d.join("t.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE venue_fills(created_at TEXT);
                 INSERT INTO venue_fills VALUES ('2026-01-01T00:00:00Z');
                 CREATE TABLE kill_switch(id INTEGER, engaged INTEGER);
                 INSERT INTO kill_switch VALUES (1, 0);
                 CREATE TABLE trades(status TEXT, mode TEXT, created_at TEXT);",
            )
            .unwrap();
        }
        let dbs = s(&db);
        // age mode: an ancient MAX(created_at) -> red
        let cfg = json!({"kind": "sqlite_query", "db": dbs, "query": "SELECT MAX(created_at) FROM venue_fills",
                         "yellow_after_s": 43200, "red_after_s": 86400});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // equals mode: engaged==0 expected -> green
        let cfg = json!({"kind": "sqlite_query", "db": dbs, "query": "SELECT engaged FROM kill_switch WHERE id=1",
                         "mode": "equals", "equals": 0});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // equals mismatch -> red + restart_forbidden stamped (the kill/breaker rule)
        let cfg = json!({"kind": "sqlite_query", "db": dbs, "query": "SELECT 1",
                         "mode": "equals", "equals": 0, "restart_forbidden_on_red": true});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Red);
        assert!(
            out.restart_forbidden,
            "kill/breaker red must forbid restart"
        );
        // NULL with null_status green (no open trades = nothing stuck)
        let cfg = json!({"kind": "sqlite_query", "db": dbs,
                         "query": "SELECT MIN(created_at) FROM trades WHERE status='open' AND mode='live'",
                         "null_status": "green", "yellow_after_s": 172800});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // NULL without the override -> unobservable yellow
        let cfg = json!({"kind": "sqlite_query", "db": dbs,
                         "query": "SELECT MIN(created_at) FROM trades", "yellow_after_s": 1});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        // number mode
        let cfg = json!({"kind": "sqlite_query", "db": dbs, "query": "SELECT 7", "mode": "number", "red_at": 5});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // a CORRUPT database file degrades to unobservable (asmodeus.db is partially malformed
        // today). NB: the query must touch the schema — SQLite prepares table-free expressions
        // like `SELECT 1` without ever reading the (garbage) file, which would false-green.
        let corrupt = d.join("corrupt.db");
        std::fs::write(&corrupt, "this is not a sqlite file at all").unwrap();
        let cfg = json!({"kind": "sqlite_query", "db": s(&corrupt),
                         "query": "SELECT MAX(created_at) FROM venue_fills", "mode": "number"});
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("unobservable"), "{}", out.detail);
        // missing db -> unobservable
        let cfg = json!({"kind": "sqlite_query", "db": s(&d.join("gone.db")), "query": "SELECT 1"});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- cmd --------
    #[cfg(windows)]
    #[test]
    fn cmd_modes() {
        // exit_code: 0 green, non-zero red
        let cfg = json!({"kind": "cmd", "command": "exit 0"});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        let cfg = json!({"kind": "cmd", "command": "exit 3"});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // number: stdout parsed, >= red_at -> red
        let cfg = json!({"kind": "cmd", "command": "echo 7", "mode": "number", "red_at": 5});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        let cfg = json!({"kind": "cmd", "command": "echo 2", "mode": "number", "red_at": 5, "yellow_at": 3});
        assert_eq!(evaluate(&cfg, "").status, Status::Green);
        // json_array_len (the gh pr list shape): 3 items >= yellow_at 2 -> yellow
        let cfg = json!({"kind": "cmd", "command": "echo [1,2,3]", "mode": "json_array_len", "yellow_at": 2, "red_at": 10});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
        // unparsable stdout in a parse mode -> unobservable
        let cfg = json!({"kind": "cmd", "command": "echo not-a-number", "mode": "number"});
        assert_eq!(evaluate(&cfg, "").status, Status::Yellow);
    }

    // -------- http_get: local listener 200 -> green; closed port -> raw red --------
    #[test]
    fn http_get_local_listener_and_closed_port() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = s.read(&mut buf);
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            }
        });
        let cfg = json!({"kind": "http_get", "url": format!("http://127.0.0.1:{port}/json/version"), "timeout_s": 5});
        let out = evaluate(&cfg, "");
        h.join().unwrap();
        assert_eq!(out.status, Status::Green, "{}", out.detail);

        // a port with no listener -> RAW red (evolve holds it yellow until the consecutive gate)
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
            // listener dropped here -> port closed
        };
        let cfg = json!({"kind": "http_get", "url": format!("http://127.0.0.1:{dead}/x"), "timeout_s": 2,
                         "red_after_consecutive": 3});
        assert_eq!(evaluate(&cfg, "").status, Status::Red);
        // non-localhost is refused (config error -> red raw with 'no response' detail is fine, but
        // the guard must fire rather than reach out to the network)
        let out = http_get_status("http://example.com:80/", Duration::from_secs(1));
        assert!(out.is_err());
    }

    // -------- git_sha_match: temp repo, match green / mismatch yellow --------
    #[test]
    fn git_sha_match_against_temp_repo() {
        use std::process::Command;
        let d = tdir("gitsha");
        let repo = d.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            let st = Command::new("git")
                .args(&args)
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(st.success());
        }
        let head = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(&repo)
            .output()
            .unwrap();
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        let hb = d.join("hb.json");
        std::fs::write(
            &hb,
            serde_json::to_string(&json!({"git_sha": head})).unwrap(),
        )
        .unwrap();
        let cfg = json!({"kind": "git_sha_match", "heartbeat_file": s(&hb)});
        let out = evaluate(&cfg, &s(&repo));
        assert_eq!(out.status, Status::Green, "{}", out.detail);
        // mismatched sha -> yellow (deploy gap is drift, not an outage)
        std::fs::write(&hb, r#"{"git_sha": "0000000"}"#).unwrap();
        let out = evaluate(&cfg, &s(&repo));
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.contains("deploy gap"));
        // no repo path -> unobservable
        let out = evaluate(&cfg, "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("unobservable"));
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- dispatch: unknown kind / missing kind degrade, never panic --------
    #[test]
    fn evaluate_unknown_or_missing_kind_is_misconfigured_yellow() {
        let out = evaluate(&json!({"kind": "quantum_entanglement"}), "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("misconfigured"));
        let out = evaluate(&json!({"id": "x"}), "");
        assert_eq!(out.status, Status::Yellow);
        assert!(out.detail.starts_with("misconfigured"));
    }
}
