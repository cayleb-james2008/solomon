//! The ops sweep: run every probe for every ops.json project, persist the verdicts, roll up the
//! fleet status, and record incident TRANSITIONS — all file-based, all under runtime/ (gitignored).
//!
//! Outputs per sweep:
//!   - `runtime/<name>/probes_verdict.json` — per-probe {status, value, threshold, detail,
//!     checked_at, consecutive_red[, first_red_ts][, restart_forbidden]}
//!   - `runtime/ops_status.json` — the fleet rollup: per-project worst_probe + reasons +
//!     process_green/outcomes_green, plus the blind-window gap (see below)
//!   - `runtime/_incidents.jsonl` — APPEND-ONLY, one record per green→red and red→green
//!     transition, keyed probe_id + first_red_ts. A PERSISTING red logs exactly once — the
//!     previous sweep's verdict is the dedupe state, the same compare-against-last-record shape as
//!     supervisor::finish's log-once escalation dedupe.
//!
//! Rollup rule (the Phase 1 contract): worst probe wins per project, and a project is healthy
//! ONLY IF its process-type probes AND its outcome probes are all green — process-liveness alone
//! can never report "all healthy" again.
//!
//! NO SCHEDULED TASK exists and none may be created (operator rule). The sweep runs in exactly two
//! places: the watchdog tick inside the visibly-open Solomon.exe, and the `solomon probe` CLI. The
//! FIRST sweep after process start computes the gap since the previous ops_status.json checked_at
//! and writes it as `blind_window_s` — the honest "how long was ops blind" number the GUI shows.
#![allow(dead_code)]

use super::{exit_code, probe, registry, Status};
use crate::control::{paths, proc};
use chrono::Utc;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::OnceLock;

/// House timestamp format (matches watchdog/monitor/supervisor).
fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// HERE/runtime/ops_status.json — the fleet rollup.
pub fn ops_status_path() -> PathBuf {
    paths::here().join("runtime").join("ops_status.json")
}

/// HERE/runtime/_incidents.jsonl — append-only transition log.
pub fn incidents_path() -> PathBuf {
    paths::here().join("runtime").join("_incidents.jsonl")
}

/// HERE/runtime/<name>/probes_verdict.json.
pub fn verdict_path(name: &str) -> PathBuf {
    paths::here()
        .join("runtime")
        .join(name)
        .join("probes_verdict.json")
}

// --------------------------------------------------------------------------- //
// per-probe state evolution (pure — the unit-tested core)
// --------------------------------------------------------------------------- //

/// Evolve one probe's persisted state given the previous verdict entry and this sweep's raw
/// outcome. Returns (new verdict entry, incident records to append). Pure — no IO — so the
/// consecutive-red gate, first_red_ts keying, and transition dedupe are unit-tested directly.
///
///   - `red_after_consecutive` (http_get-class): a raw red is HELD AT YELLOW until that many
///     consecutive sweeps have failed (one CDP blip is not an incident; three straight is).
///   - `consecutive_red` counts consecutive RAW-red sweeps (reset on any non-red observation).
///   - entering red (prev != red, new == red) appends a "red" incident stamped first_red_ts=now;
///     a red that PERSISTS keeps its original first_red_ts and appends NOTHING (log-once);
///     leaving red appends a "recovered" incident carrying the same first_red_ts key.
pub fn evolve(
    prev: Option<&Value>,
    raw: &probe::ProbeOutcome,
    red_after_consecutive: Option<i64>,
    project: &str,
    probe_id: &str,
    ts: &str,
) -> (Value, Vec<Value>) {
    let prev_status = prev
        .and_then(|p| p.get("status"))
        .and_then(Value::as_str)
        .and_then(Status::parse);
    let prev_consec = prev
        .and_then(|p| p.get("consecutive_red"))
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let raw_red = raw.status == Status::Red;
    let consec = if raw_red { prev_consec + 1 } else { 0 };
    let status = if raw_red {
        match red_after_consecutive {
            Some(n) if consec < n => Status::Yellow, // failing, but not long enough to call red
            _ => Status::Red,
        }
    } else {
        raw.status
    };

    // first_red_ts: the incident key. Set when ENTERING red; carried while red persists.
    let first_red_ts: Option<String> = if status == Status::Red {
        if prev_status == Some(Status::Red) {
            prev.and_then(|p| p.get("first_red_ts"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(ts.to_string()))
        } else {
            Some(ts.to_string())
        }
    } else {
        None
    };

    let mut entry = Map::new();
    entry.insert("status".into(), json!(status.as_str()));
    entry.insert("value".into(), raw.value.clone());
    entry.insert("threshold".into(), json!(raw.threshold));
    entry.insert("detail".into(), json!(raw.detail));
    entry.insert("checked_at".into(), json!(ts));
    entry.insert("consecutive_red".into(), json!(consec));
    if let Some(ref f) = first_red_ts {
        entry.insert("first_red_ts".into(), json!(f));
    }
    if raw.restart_forbidden {
        entry.insert("restart_forbidden".into(), json!(true));
    }

    let full_id = format!("{project}/{probe_id}");
    let mut incidents = Vec::new();
    if status == Status::Red && prev_status != Some(Status::Red) {
        // green/yellow/unknown -> red: open an incident, keyed probe_id + first_red_ts.
        incidents.push(json!({
            "ts": ts,
            "event": "red",
            "project": project,
            "probe": probe_id,
            "probe_id": full_id,
            "first_red_ts": first_red_ts,
            "value": raw.value,
            "threshold": raw.threshold,
            "detail": raw.detail,
        }));
    } else if status != Status::Red && prev_status == Some(Status::Red) {
        // red -> not-red: close the incident under the SAME first_red_ts key.
        let opened = prev
            .and_then(|p| p.get("first_red_ts"))
            .cloned()
            .unwrap_or(Value::Null);
        incidents.push(json!({
            "ts": ts,
            "event": "recovered",
            "project": project,
            "probe": probe_id,
            "probe_id": full_id,
            "first_red_ts": opened,
            "status": status.as_str(),
            "detail": raw.detail,
        }));
    }
    (Value::Object(entry), incidents)
}

// --------------------------------------------------------------------------- //
// blind window
// --------------------------------------------------------------------------- //

static BLIND_WINDOW: OnceLock<Value> = OnceLock::new();

/// The catch-up / blind-window report: on the FIRST sweep after process start, the gap (seconds)
/// since the previous ops_status.json checked_at — i.e. how long the ops plane was blind while
/// Solomon.exe was closed. Computed once per process (before the first sweep overwrites the file)
/// and carried on every subsequent status write so the GUI can show it later. Null when there is
/// no previous status to measure against.
fn blind_window() -> Value {
    BLIND_WINDOW
        .get_or_init(|| {
            let prev: Option<Value> = std::fs::read(ops_status_path())
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            compute_blind_window(prev.as_ref(), Utc::now())
        })
        .clone()
}

/// Pure core of `blind_window` (unit-tested): the seconds between the previous status'
/// checked_at and `now_dt`, clamped at 0; Null on missing/unparseable previous state.
pub fn compute_blind_window(prev_status: Option<&Value>, now_dt: chrono::DateTime<Utc>) -> Value {
    let ts = match prev_status
        .and_then(|p| p.get("checked_at"))
        .and_then(Value::as_str)
    {
        Some(t) => t,
        None => return Value::Null,
    };
    match chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
        Ok(t) => {
            let gap = (now_dt - t.and_utc()).num_milliseconds() as f64 / 1000.0;
            json!(gap.max(0.0))
        }
        Err(_) => Value::Null,
    }
}

// --------------------------------------------------------------------------- //
// sweep
// --------------------------------------------------------------------------- //

/// One ops sweep over every ops.json project (or just `only` for `solomon probe <name>`).
/// Runs all probes, persists per-project verdicts + incidents, writes runtime/ops_status.json,
/// and returns the full status payload. Per-project catch_unwind mirrors the watchdog's
/// 'never die on one bad repo' contract — one panicking probe set cannot abort the fleet sweep.
pub fn sweep_filtered(only: Option<&str>) -> Value {
    let ts = now();
    let blind = blind_window();
    let mut entries = registry::load_ops();
    entries.sort_by_key(registry::project_priority);

    let mut projects = Map::new();
    for entry in &entries {
        let name = registry::project_name(entry);
        if let Some(o) = only {
            if o != name {
                continue;
            }
        }
        let res =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep_project(entry, &ts)));
        match res {
            Ok(project_entry) => {
                projects.insert(name, project_entry);
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("panic");
                // an unobservable project is yellow, never a silent skip
                projects.insert(
                    name,
                    json!({
                        "priority": registry::project_priority(entry),
                        "status": "yellow",
                        "healthy": false,
                        "worst_probe": Value::Null,
                        "reasons": [format!("sweep panic: {msg}")],
                        "restart_forbidden": false,
                        "probes": {},
                    }),
                );
            }
        }
    }

    // A single-project run must not clobber the other projects' rollup: merge over the existing
    // payload so ops_status.json stays fleet-shaped.
    if only.is_some() {
        if let Some(existing) = std::fs::read(ops_status_path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        {
            if let Some(old) = existing.get("projects").and_then(Value::as_object) {
                for (k, v) in old {
                    projects.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
    }

    let payload = json!({
        "checked_at": ts,
        "blind_window_s": blind,
        "projects": projects,
    });
    let path = ops_status_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&path, &payload);
    payload
}

/// The watchdog-graft entry: all probes, all projects.
pub fn sweep() -> Value {
    sweep_filtered(None)
}

/// One project: evaluate every probe, evolve state against the previous verdict file, persist the
/// new verdict + incident transitions, and return the rollup entry for ops_status.json.
fn sweep_project(entry: &Value, ts: &str) -> Value {
    let name = registry::project_name(entry);
    let repo_path = registry::project_repo_path(&name);
    let prev: Value = std::fs::read(verdict_path(&name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let prev_probes = prev.get("probes").cloned().unwrap_or(Value::Null);

    let mut probes = Map::new();
    let mut incidents: Vec<Value> = Vec::new();
    let mut worst = Status::Green;
    let mut worst_probe: Option<String> = None;
    let mut reasons: Vec<String> = Vec::new();
    let mut restart_forbidden = false;
    let mut process_green = true;
    let mut outcomes_green = true;
    let mut status_map = Map::new();

    for cfg in registry::project_probes(entry) {
        // Lenient probe parse: a malformed entry (no id / no kind) is skipped with a logged
        // warning, never a panic — one bad registry line must not kill the sweep.
        let id = match cfg
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(i) => i.to_string(),
            None => {
                eprintln!("ops.json: {name}: skipping probe without an 'id'");
                continue;
            }
        };
        if cfg
            .get("kind")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            eprintln!("ops.json: {name}: skipping probe '{id}' without a 'kind'");
            continue;
        }
        let raw = probe::evaluate(&cfg, &repo_path);
        let (verdict, mut trans) = evolve(
            prev_probes.get(&id),
            &raw,
            probe::red_after_consecutive(&cfg),
            &name,
            &id,
            ts,
        );
        incidents.append(&mut trans);

        let status = verdict
            .get("status")
            .and_then(Value::as_str)
            .and_then(Status::parse)
            .unwrap_or(Status::Yellow);
        let is_process = cfg.get("kind").and_then(Value::as_str) == Some("process");
        if status != Status::Green {
            if is_process {
                process_green = false;
            } else {
                outcomes_green = false;
            }
            reasons.push(format!("{id}={} ({})", status.as_str(), raw.detail));
            // worst-wins; the FIRST probe (config order) at the worst severity names the rollup.
            if status > worst {
                worst = status;
                worst_probe = Some(id.clone());
            } else if worst_probe.is_none() {
                worst_probe = Some(id.clone());
            }
        }
        if raw.restart_forbidden && status == Status::Red {
            restart_forbidden = true;
        }
        status_map.insert(id.clone(), json!(status.as_str()));
        probes.insert(id, verdict);
    }

    // Persist the per-project verdict file (atomic, under runtime/<name>/).
    let vpath = verdict_path(&name);
    if let Some(parent) = vpath.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(
        &vpath,
        &json!({"checked_at": ts, "probes": Value::Object(probes)}),
    );
    append_incidents(&incidents);

    json!({
        "priority": registry::project_priority(entry),
        "status": worst.as_str(),
        // The Phase 1 health rule: process plane AND outcome plane both fully green.
        "healthy": process_green && outcomes_green,
        "process_green": process_green,
        "outcomes_green": outcomes_green,
        "worst_probe": worst_probe,
        "reasons": reasons,
        "restart_forbidden": restart_forbidden,
        "probes": Value::Object(status_map),
    })
}

/// Append transition records to runtime/_incidents.jsonl (append-only; OSError -> pass, matching
/// the watchdog's monitor-log append) — and push each one to the operator (Phase A: an incident
/// the operator never hears about is the June failure mode). The evolve() dedupe upstream means a
/// persisting red notifies exactly once; notify::send is best-effort and can never fail the sweep.
fn append_incidents(records: &[Value]) {
    if records.is_empty() {
        return;
    }
    crate::notify::notify_incidents(records);
    let _ = (|| -> std::io::Result<()> {
        let path = incidents_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        for r in records {
            writeln!(f, "{}", serde_json::to_string(r).unwrap_or_default())?;
        }
        Ok(())
    })();
}

// --------------------------------------------------------------------------- //
// rollup helpers (shared by the CLI and the watchdog line)
// --------------------------------------------------------------------------- //

/// Worst status across the payload's projects (green when there are none).
pub fn payload_worst(payload: &Value) -> Status {
    let mut worst = Status::Green;
    if let Some(projects) = payload.get("projects").and_then(Value::as_object) {
        for (_, p) in projects {
            if let Some(s) = p
                .get("status")
                .and_then(Value::as_str)
                .and_then(Status::parse)
            {
                worst = Status::worst(worst, s);
            }
        }
    }
    worst
}

/// The compact per-project fleet summary appended to the watchdog line, priority order:
/// `asmodeus RED(fills_recency) sover YELLOW(post_failures) dotz GREEN ...`.
pub fn payload_summary(payload: &Value) -> String {
    let projects = match payload.get("projects").and_then(Value::as_object) {
        Some(p) if !p.is_empty() => p,
        _ => return "no probes configured".to_string(),
    };
    let mut items: Vec<(&String, &Value)> = projects.iter().collect();
    items.sort_by_key(|(_, p)| {
        p.get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    items
        .iter()
        .map(|(name, p)| {
            let status = p.get("status").and_then(Value::as_str).unwrap_or("yellow");
            let up = status.to_uppercase();
            match p.get("worst_probe").and_then(Value::as_str) {
                Some(w) if status != "green" => format!("{name} {up}({w})"),
                _ => format!("{name} {up}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Watchdog graft entry: run the full sweep and return the one-line ops summary for the two-plane
/// watchdog line. Total — a panic anywhere inside is already isolated per-project.
pub fn sweep_and_summarize() -> String {
    let payload = sweep();
    payload_summary(&payload)
}

// --------------------------------------------------------------------------- //
// `solomon probe` CLI
// --------------------------------------------------------------------------- //

/// `solomon probe [name] [--json]` — run all probes (or one project's), print the verdict table
/// (or the ops_status.json payload with --json), and exit 0 green / 3 yellow / 4 red / 2 usage.
pub fn probe_main(args: &[String]) -> i32 {
    let mut name: Option<String> = None;
    let mut as_json = false;
    for a in args {
        match a.as_str() {
            "--json" => as_json = true,
            s if s.starts_with('-') => {
                eprintln!("usage: solomon probe [name] [--json]");
                return 2;
            }
            s => {
                if name.is_some() {
                    eprintln!("usage: solomon probe [name] [--json]");
                    return 2;
                }
                name = Some(s.to_string());
            }
        }
    }
    let entries = registry::load_ops();
    if entries.is_empty() {
        eprintln!(
            "no ops.json probe registry found at {}",
            registry::ops_json_path().display()
        );
        return 2;
    }
    if let Some(ref n) = name {
        if !entries.iter().any(|e| registry::project_name(e) == *n) {
            eprintln!("unknown project '{n}' (not in ops.json)");
            return 2;
        }
    }
    let payload = sweep_filtered(name.as_deref());
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
    } else {
        print_table(&payload, name.as_deref());
    }
    // Exit code judges only the requested scope: one project when named, the fleet otherwise.
    let scoped = match name {
        Some(ref n) => {
            let projects = payload.get("projects").cloned().unwrap_or(Value::Null);
            json!({"projects": {n.clone(): projects.get(n).cloned().unwrap_or(Value::Null)}})
        }
        None => payload.clone(),
    };
    exit_code(payload_worst(&scoped))
}

/// The compact verdict table: one row per probe, read back from the just-written verdict files so
/// the printed evidence is exactly what was persisted.
fn print_table(payload: &Value, only: Option<&str>) {
    let projects = match payload.get("projects").and_then(Value::as_object) {
        Some(p) => p,
        None => return,
    };
    let mut items: Vec<(&String, &Value)> = projects
        .iter()
        .filter(|(n, _)| only.map(|o| o == n.as_str()).unwrap_or(true))
        .collect();
    items.sort_by_key(|(_, p)| {
        p.get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    // (widths match the row format below: 10/20/7 + free-form detail)
    println!("PROJECT    PROBE                STATUS  DETAIL");
    for (name, proj) in &items {
        let verdict: Value = std::fs::read(verdict_path(name))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let probes = verdict.get("probes").and_then(Value::as_object);
        // Preserve ops.json probe order via the verdict map's insertion order (preserve_order on).
        if let Some(probes) = probes {
            for (id, v) in probes {
                let status = v.get("status").and_then(Value::as_str).unwrap_or("?");
                let detail = v.get("detail").and_then(Value::as_str).unwrap_or("");
                println!(
                    "{:<10} {:<20} {:<7} {}",
                    name,
                    id,
                    status.to_uppercase(),
                    detail
                );
            }
        }
        let rollup = proj.get("status").and_then(Value::as_str).unwrap_or("?");
        println!(
            "{:<10} {:<20} {:<7} healthy={} restart_forbidden={}",
            name,
            "== rollup ==",
            rollup.to_uppercase(),
            proj.get("healthy")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            proj.get("restart_forbidden")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        );
    }
    if let Some(b) = payload.get("blind_window_s").and_then(Value::as_f64) {
        println!("blind window since last sweep: {:.0}s", b);
    }
    println!("fleet: {}", payload_summary(payload));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::probe::ProbeOutcome;

    fn raw(status: Status) -> ProbeOutcome {
        ProbeOutcome {
            status,
            value: json!(1),
            threshold: "t".to_string(),
            detail: "d".to_string(),
            restart_forbidden: false,
        }
    }

    // -------- evolve: transition dedupe (a persisting red logs ONCE) --------
    #[test]
    fn evolve_red_transition_logs_once_and_recovery_closes() {
        // green -> red: one "red" incident, first_red_ts stamped now.
        let (v1, inc1) = evolve(None, &raw(Status::Red), None, "p", "x", "T1");
        assert_eq!(v1["status"], json!("red"));
        assert_eq!(v1["first_red_ts"], json!("T1"));
        assert_eq!(inc1.len(), 1);
        assert_eq!(inc1[0]["event"], json!("red"));
        assert_eq!(inc1[0]["probe_id"], json!("p/x"));

        // red persists -> NO new record (log-once), first_red_ts carried unchanged.
        let (v2, inc2) = evolve(Some(&v1), &raw(Status::Red), None, "p", "x", "T2");
        assert_eq!(v2["status"], json!("red"));
        assert_eq!(
            v2["first_red_ts"],
            json!("T1"),
            "first_red_ts must persist across sweeps"
        );
        assert_eq!(v2["consecutive_red"], json!(2));
        assert!(inc2.is_empty(), "a persisting red must log exactly once");

        // red -> green: one "recovered" record keyed by the SAME first_red_ts.
        let (v3, inc3) = evolve(Some(&v2), &raw(Status::Green), None, "p", "x", "T3");
        assert_eq!(v3["status"], json!("green"));
        assert!(v3.get("first_red_ts").is_none());
        assert_eq!(v3["consecutive_red"], json!(0));
        assert_eq!(inc3.len(), 1);
        assert_eq!(inc3[0]["event"], json!("recovered"));
        assert_eq!(inc3[0]["first_red_ts"], json!("T1"));
    }

    #[test]
    fn evolve_yellow_never_opens_incidents() {
        let (v, inc) = evolve(None, &raw(Status::Yellow), None, "p", "x", "T1");
        assert_eq!(v["status"], json!("yellow"));
        assert!(inc.is_empty());
        // yellow -> green: still nothing (incidents are red-scoped by contract)
        let (_, inc2) = evolve(Some(&v), &raw(Status::Green), None, "p", "x", "T2");
        assert!(inc2.is_empty());
    }

    // -------- evolve: the consecutive-red gate (http_get / cdp_alive rule) --------
    #[test]
    fn evolve_consecutive_gate_holds_yellow_until_n() {
        // fail #1 and #2 -> held at yellow, no incident
        let (v1, inc1) = evolve(None, &raw(Status::Red), Some(3), "p", "cdp", "T1");
        assert_eq!(v1["status"], json!("yellow"));
        assert_eq!(v1["consecutive_red"], json!(1));
        assert!(inc1.is_empty());
        let (v2, inc2) = evolve(Some(&v1), &raw(Status::Red), Some(3), "p", "cdp", "T2");
        assert_eq!(v2["status"], json!("yellow"));
        assert_eq!(v2["consecutive_red"], json!(2));
        assert!(inc2.is_empty());
        // fail #3 -> RED + the incident opens now
        let (v3, inc3) = evolve(Some(&v2), &raw(Status::Red), Some(3), "p", "cdp", "T3");
        assert_eq!(v3["status"], json!("red"));
        assert_eq!(v3["consecutive_red"], json!(3));
        assert_eq!(inc3.len(), 1);
        assert_eq!(inc3[0]["first_red_ts"], json!("T3"));
        // one success resets the streak entirely
        let (v4, _) = evolve(Some(&v3), &raw(Status::Green), Some(3), "p", "cdp", "T4");
        assert_eq!(v4["consecutive_red"], json!(0));
    }

    // -------- evolve: restart_forbidden stamped through --------
    #[test]
    fn evolve_restart_forbidden_carried_into_verdict() {
        let mut r = raw(Status::Red);
        r.restart_forbidden = true;
        let (v, _) = evolve(None, &r, None, "asmodeus", "kill_breaker", "T1");
        assert_eq!(v["restart_forbidden"], json!(true));
        // and NOT stamped when green
        let mut g = raw(Status::Green);
        g.restart_forbidden = false;
        let (v, _) = evolve(None, &g, None, "asmodeus", "kill_breaker", "T2");
        assert!(v.get("restart_forbidden").is_none());
    }

    // -------- blind window --------
    #[test]
    fn compute_blind_window_vectors() {
        let now_dt =
            chrono::NaiveDateTime::parse_from_str("2026-07-01T12:00:00Z", "%Y-%m-%dT%H:%M:%SZ")
                .unwrap()
                .and_utc();
        // 1h gap
        let prev = json!({"checked_at": "2026-07-01T11:00:00Z"});
        assert_eq!(compute_blind_window(Some(&prev), now_dt), json!(3600.0));
        // no previous state -> Null (nothing to measure against)
        assert_eq!(compute_blind_window(None, now_dt), Value::Null);
        // unparseable checked_at -> Null, never a panic
        let junk = json!({"checked_at": "yesterday-ish"});
        assert_eq!(compute_blind_window(Some(&junk), now_dt), Value::Null);
        // clock skew clamps at 0
        let future = json!({"checked_at": "2026-07-01T13:00:00Z"});
        assert_eq!(compute_blind_window(Some(&future), now_dt), json!(0.0));
    }

    // -------- rollup: worst wins + two-plane health rule --------
    #[test]
    fn payload_worst_and_summary() {
        let payload = json!({
            "checked_at": "T",
            "projects": {
                "asmodeus": {"priority": 1, "status": "red", "worst_probe": "fills_recency"},
                "sover": {"priority": 2, "status": "yellow", "worst_probe": "post_failures"},
                "dotz": {"priority": 4, "status": "green", "worst_probe": null},
            }
        });
        assert_eq!(payload_worst(&payload), Status::Red);
        // priority order, worst probe named for non-green only
        assert_eq!(
            payload_summary(&payload),
            "asmodeus RED(fills_recency) sover YELLOW(post_failures) dotz GREEN"
        );
        // all-green fleet
        let payload = json!({"projects": {"a": {"priority": 1, "status": "green"}}});
        assert_eq!(payload_worst(&payload), Status::Green);
        // empty registry
        assert_eq!(payload_worst(&json!({"projects": {}})), Status::Green);
        assert_eq!(
            payload_summary(&json!({"projects": {}})),
            "no probes configured"
        );
    }

    // -------- incidents file round-trip: green->red->red->green appends exactly 2 records ----
    #[test]
    fn incident_sequence_appends_two_records_for_one_outage() {
        let ts = ["T1", "T2", "T3"];
        let seq = [Status::Red, Status::Red, Status::Green];
        let mut prev: Option<Value> = None;
        let mut all: Vec<Value> = Vec::new();
        for (i, st) in seq.iter().enumerate() {
            let (v, mut inc) = evolve(prev.as_ref(), &raw(*st), None, "p", "x", ts[i]);
            all.append(&mut inc);
            prev = Some(v);
        }
        assert_eq!(
            all.len(),
            2,
            "one outage = one red + one recovered, never more"
        );
        assert_eq!(all[0]["event"], json!("red"));
        assert_eq!(all[1]["event"], json!("recovered"));
        // both records share the incident key (probe_id + first_red_ts)
        assert_eq!(all[0]["probe_id"], all[1]["probe_id"]);
        assert_eq!(all[0]["first_red_ts"], all[1]["first_red_ts"]);
    }

    // -------- CLI usage errors stay exit 2 --------
    #[test]
    fn probe_main_usage_errors() {
        // unknown flag
        assert_eq!(probe_main(&["--frobnicate".to_string()]), 2);
        // two positional names
        assert_eq!(probe_main(&["a".to_string(), "b".to_string()]), 2);
    }
}
