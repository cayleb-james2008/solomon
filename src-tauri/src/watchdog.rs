//! Native Rust port of `monitor.py` — Solomon's overnight watchdog + data collector.
//!
//! Behavior is bug-for-bug with `monitor.py`. Run periodically (a Windows scheduled task — see
//! scripts/watchdog.cmd). Each sweep, for every registered repo, it:
//!
//!   1. **Restarts a CRASHED loop.** A loop that exits cleanly (operator Stop, or max-iterations)
//!      writes `status="stopped"` in its last heartbeat via the runner's `finally` block; a
//!      crash/kill leaves the last live status (`iterating`/`sleeping`/`error`). The watchdog
//!      restarts only the latter — an operator Stop is left alone, a crash is healed. The
//!      single-flight lock makes a redundant start a no-op, so this is race-safe.
//!   2. **Runs the supervisor's RUNG-0 recovery** (deterministic + reversible only: stale lock,
//!      lingering stop, dirty-tree reset when quiesced). Never a pi fix, push, merge, or force-kill.
//!   3. **Appends a JSON snapshot per repo** to `runtime/_monitor.jsonl` for overnight trend
//!      analysis (status, iteration, last outcome, diagnosis, whether it was restarted).
//!
//! Operator controls (honored, non-destructive):
//!   - `runtime/_watchdog.disabled`  — global kill-switch; the sweep does nothing.
//!   - `runtime/<name>/paused`       — never auto-restart this one repo.
//! A clean Stop from the GUI already sets `status="stopped"` and is respected automatically.
//!
//! The returns that back the JSONL/log are `serde_json::Value` whose keys are byte-identical to the
//! Python dicts; status/category/reason strings are quoted verbatim from `monitor.py`.
//!
//! Wired to the `solomon watchdog` subcommand (main.rs), which the SolomonWatchdog scheduled task
//! runs every 2 minutes. Some helpers are exercised only by tests, so allow dead-code for this module.
#![allow(dead_code)]

use crate::control::{self, paths};
use crate::supervisor;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::Path;

/// monitor.STALL_SWEEPS — consecutive error/preflight sweeps (incl. this one) that mark a lane STALLED.
const STALL_SWEEPS: usize = 3;

/// HEALTH_K — the iteration window the degraded-health classifier inspects. A lane whose last
/// `HEALTH_K` iterations all failed to ship AND were every one a timeout or a gate-RED is
/// `degraded:<reason>`, not `healthy` — even when its PID is alive. Pure observability: the
/// watchdog line surfaces the per-lane reason; it never auto-restarts or auto-merges on it.
const HEALTH_K: usize = 3;

/// monitor.DISABLED — global kill-switch path: HERE/runtime/_watchdog.disabled.
fn disabled_path() -> std::path::PathBuf {
    paths::here().join("runtime").join("_watchdog.disabled")
}

/// monitor.MON_LOG — HERE/runtime/_monitor.jsonl.
fn mon_log() -> std::path::PathBuf {
    paths::here().join("runtime").join("_monitor.jsonl")
}

/// monitor._now: `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// monitor._recent_snapshots: the last `k` persisted `_monitor.jsonl` snapshots for repo `name`
/// (oldest→newest), excluding the current sweep (the caller appends it). `[]` on missing/corrupt
/// file.
///
/// `max_age_s`: when `Some`, drop records older than that (and any undated/unparseable record)
/// BEFORE taking the last `k`. A stall window must be temporally adjacent — otherwise stale
/// snapshots from a resolved wedge days ago (e.g. across a kill-switch pause) plus one fresh break
/// would be miscounted as "consecutive sweeps" and false-escalate.
fn recent_snapshots(name: &str, k: usize, max_age_s: Option<f64>) -> Vec<Value> {
    recent_snapshots_from(&mon_log(), name, k, max_age_s)
}

/// Path-parameterized core of `recent_snapshots` so tests can run against an isolated log file
/// (the production caller always passes `mon_log()`).
fn recent_snapshots_from(
    path: &std::path::Path,
    name: &str,
    k: usize,
    max_age_s: Option<f64>,
) -> Vec<Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(), // OSError -> []
    };
    let now_dt = Utc::now();
    let mut out: Vec<Value> = Vec::new();
    // Python f.read().splitlines() then `.strip()` each line.
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue, // json.JSONDecodeError -> skip
        };
        // not (isinstance(rec, dict) and rec.get("repo") == name) -> skip
        if !(rec.is_object() && rec.get("repo").and_then(Value::as_str) == Some(name)) {
            continue;
        }
        if let Some(max_age) = max_age_s {
            // datetime.strptime(rec.get("ts"), "%Y-%m-%dT%H:%M:%SZ"); undated/unparseable -> skip.
            let ts = match rec.get("ts").and_then(Value::as_str) {
                Some(s) => s,
                None => continue, // TypeError (non-string / missing ts) -> skip
            };
            let t = match chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
                Ok(t) => t.and_utc(),
                Err(_) => continue, // ValueError -> skip (can't prove recency -> not part of a streak)
            };
            if (now_dt - t).num_milliseconds() as f64 / 1000.0 > max_age {
                continue;
            }
        }
        out.push(rec);
    }
    // out[-k:]
    if k == 0 {
        // Python out[-0:] == out[0:] == all; preserve the slice quirk for parity.
        out
    } else if k >= out.len() {
        out
    } else {
        out.split_off(out.len() - k)
    }
}

/// monitor.should_restart: restart only a CRASHED loop. Pure (no IO) so the decision is unit-tested.
///
/// True iff: not running, NOT explicitly paused, NO pending stop sentinel, and the repo has a
/// heartbeat whose status is anything except a deliberate stop/halt. Restarted: a live-phase crash
/// (iterating/sleeping/idle) AND a transient/retryable error (e.g. a flaky red base gate). Left alone:
///   - `"stopped"`                  — a clean exit (operator Stop / max-iterations);
///   - `"error"` + `phase=reverted` — a revert-failure HALT that needs operator cleanup (a blind
///                                     restart just re-hits the known-bad tree);
///   - no heartbeat                 — a repo that never ran (the watchdog keeps enabled loops alive,
///                                     it does not auto-enable new ones).
/// A persistent error (no key, dirty base) restarts, re-errors immediately, and the supervisor's
/// diagnose/anti-thrash escalates it — so it surfaces without the watchdog having to classify it.
pub fn should_restart(running: bool, hb: &Value, paused: bool, stop_pending: bool) -> bool {
    if running || paused || stop_pending {
        return false;
    }
    // status = (hb or {}).get("status")
    let status = hb.get("status").and_then(Value::as_str);
    // if not status or status == "stopped": (a JSON null / non-string / missing / "" status is falsy)
    match status {
        None | Some("") | Some("stopped") => return false,
        _ => {}
    }
    // status == "error" and (hb or {}).get("phase") == "reverted"
    if status == Some("error") && hb.get("phase").and_then(Value::as_str) == Some("reverted") {
        return false; // the revert-failure HALT — operator cleanup, not a restart
    }
    true
}

/// DEGRADED HEALTH CLASSIFIER — deepens the watchdog's health signal beyond process-liveness.
///
/// A RUNNING lane (PID alive) is `degraded:<reason>` when its last `k` iterations all failed
/// to ship AND every one was a timeout or a gate-RED. `None` means healthy/unknown. Pure (no IO)
/// so the healthy-vs-degraded predicate is unit-tested over synthetic history slices.
///
/// Each history record (from `runtime/<lane>/history.jsonl`, oldest→newest) is classified:
///   - **TIMEOUT** — `status == "noop"` and `summary` contains "timed out" (the runner records a
///     pi timeout — `pi.status == "timed_out"`, `pi.exit_code == 124` — as a noop with summary
///     `"Pi session timed out."`).
///   - **GATE-RED** — `status == "reverted"` and `tests.green == false` (the test gate failed).
///   - **SHIP** — `status == "shipped"` (a successful merge/push).
/// Any other outcome (a ship, a non-timeout noop, a non-gate revert, an error, a blocked) or fewer
/// than `k` records breaks the streak → healthy (no false degradation from a single flake).
///
/// Pure observability — the caller MUST NOT auto-restart or auto-merge on this signal (ship=pr
/// stays manual). This is the systemic fix for "stale heartbeat/escalation state" + "lanes
/// thrashing without a real fix" being silently reported as `all healthy`.
pub fn lane_health(running: bool, history: &[Value], k: usize) -> Option<String> {
    if !running || k == 0 {
        return None;
    }
    // history is oldest→newest; take the last k. Fewer than k records → unknown, not degraded.
    if history.len() < k {
        return None;
    }
    let last_k = &history[history.len() - k..];
    let mut timeouts = 0usize;
    let mut gate_reds = 0usize;
    for rec in last_k {
        let status = rec.get("status").and_then(Value::as_str).unwrap_or("");
        match status {
            "noop" => {
                let summary = rec.get("summary").and_then(Value::as_str).unwrap_or("");
                if summary.to_lowercase().contains("timed out") {
                    timeouts += 1;
                } else {
                    return None; // a non-timeout noop (model no-op, beautify) breaks the streak
                }
            }
            "reverted" => {
                let green = rec
                    .get("tests")
                    .and_then(|t| t.get("green"))
                    .and_then(Value::as_bool);
                if green == Some(false) {
                    gate_reds += 1;
                } else {
                    return None; // a non-gate-RED revert (anti-gaming, eval, leak-guard) breaks the streak
                }
            }
            _ => return None, // shipped/error/blocked/stopped/unknown breaks the streak
        }
    }
    if timeouts == 0 && gate_reds == 0 {
        return None;
    }
    let reason = if gate_reds == 0 {
        format!("degraded:{k}x timed_out")
    } else if timeouts == 0 {
        format!("degraded:{k}x gate-RED")
    } else {
        format!("degraded:{k}x ({timeouts}x timed_out, {gate_reds}x gate-RED)")
    };
    Some(reason)
}

/// monitor._auto_push: read `.solomon.json`'s `auto_push` (default True; True on missing/corrupt).
fn auto_push() -> bool {
    let p = paths::here().join(".solomon.json");
    let data = match std::fs::read_to_string(&p) {
        Ok(d) => d,
        Err(_) => return true, // OSError -> True
    };
    let v: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return true, // json.JSONDecodeError -> True
    };
    // bool(json.load(f).get("auto_push", True)): missing key -> True; else Python truthiness of value.
    match v.get("auto_push") {
        None => true,
        Some(x) => json_truthy(x),
    }
}

/// Python `bool(x)` truthiness for a JSON value (as used by `_auto_push`).
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// monitor._base_is_clean: True iff the repo working tree is fully clean — no modified tracked files
/// AND no untracked non-ignored files (`git status --porcelain` empty). Used to auto-recover a
/// `dirty_base_persistent` self-stop ONLY once the operator has actually cleaned the tree, so
/// clearing the stop can never thrash (a still-dirty base stays stopped).
fn base_is_clean(path: &str) -> bool {
    if path.is_empty() || !Path::new(path).is_dir() {
        return false;
    }
    // control._run(["git", "-C", path, "status", "--porcelain"]) — windowless, guarded spawn.
    let r = match control::proc::run(&["git", "-C", path, "status", "--porcelain"], None, None) {
        Ok(r) => r,
        Err(_) => return false, // OSError -> False
    };
    r.code == 0 && r.stdout.trim().is_empty()
}

/// True iff the local `base` branch is NOT ahead of `origin/<base>` — i.e. the un-pushed commits
/// that triggered an `unpushed_base_persistent` self-stop have since been pushed (or the base was
/// reset back to origin). Used to auto-recover that self-stop ONLY once the operator has actually
/// reconciled the base, so clearing the stop can never thrash (a still-ahead base stays stopped).
/// Safe on any error (missing remote ref, spawn failure): returns False, leaving the stop in place.
fn base_is_pushed(path: &str, base: &str) -> bool {
    if path.is_empty() || base.is_empty() || !Path::new(path).is_dir() {
        return false;
    }
    // git -C path rev-list --count origin/<base>..<base> -> "0" when base is not ahead of origin.
    let range = format!("origin/{base}..{base}");
    let r = match control::proc::run(
        &["git", "-C", path, "rev-list", "--count", range.as_str()],
        None,
        None,
    ) {
        Ok(r) => r,
        Err(_) => return false, // OSError -> False
    };
    r.code == 0 && r.stdout.trim() == "0"
}

/// The persistent-bail self-stop re-observation policy. The runner self-stops (STOP sentinel +
/// `status=error` + a `reason` marker) on a PERSISTENTLY dirty / un-pushed base so it doesn't spin
/// forever — but that sentinel otherwise pins the lane DEAD until a human clicks Start, leaving a
/// stale heartbeat that `diagnose` keeps re-escalating as `stop_lingering` even after the operator
/// has fixed the cause. The watchdog re-observes the underlying condition each sweep and clears the
/// sentinel once the cause is ACTUALLY healed, so the lane resumes on the next `should_restart`
/// instead of lingering in a stale escalation.
///
/// Returns the auto-recover action message (the suffix after `"{name} auto-recover: "`) when the
/// sentinel should be cleared for this `reason`, else `None`. Pure — the git condition checks are
/// passed in as bools — so the policy is unit-tested. Only fires on the runner's own reason markers
/// AND a verified-healed condition, so it never thrashes and never touches a true operator Stop
/// (status="stopped", no reason).
fn persistent_stop_cleared(reason: &str, base_clean: bool, base_pushed: bool) -> Option<&'static str> {
    match reason {
        "dirty_base_persistent" if base_clean => {
            Some("base clean again — cleared dirty_base_persistent stop")
        }
        "unpushed_base_persistent" if base_pushed => {
            Some("base pushed again — cleared unpushed_base_persistent stop")
        }
        _ => None,
    }
}

/// monitor._sweep_repo: process ONE repo for sweep(); returns (actions, snap). The caller runs this
/// inside a blanket try/except so one bad repo can never abort the whole sweep — the module's
/// documented 'a watchdog must never die on one bad repo' contract.
fn sweep_repo(r: &Value, auto_push_flag: bool) -> (Vec<String>, Value) {
    let mut actions: Vec<String> = Vec::new();
    // name = r["name"] — the caller guarantees a truthy name before calling.
    let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let rt = paths::runtime_dir(r);
    let paused = rt
        .as_ref()
        .map(|d| d.join("paused").exists())
        .unwrap_or(false);
    let mut stop_pending = rt
        .as_ref()
        .map(|d| d.join("stop").exists())
        .unwrap_or(false);
    let running = control::locks::is_running(r);
    let hb = control::heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));

    // Anti-wedge: the runner self-stops on a PERSISTENTLY dirty / un-pushed base (STOP sentinel +
    // status=error + a reason marker) so it doesn't spin forever — but that sentinel otherwise pins
    // the lane DEAD until a human clicks Start, leaving a stale heartbeat that diagnose keeps
    // re-escalating as stop_lingering even AFTER the operator has fixed the cause. Re-observe the
    // underlying condition each sweep and clear the self-written sentinel once the cause is actually
    // healed, so should_restart heals the lane automatically. Safe: only fires on the runner's own
    // reason marker AND a verified-healed condition (see persistent_stop_cleared), so it never
    // thrashes and never touches a true operator Stop (status="stopped", no reason).
    if !running
        && !paused
        && stop_pending
        && hb.get("status").and_then(Value::as_str) == Some("error")
    {
        if let Some(reason) = hb.get("reason").and_then(Value::as_str) {
            let path = paths::repo_path(r);
            let base = control::registry::project_pr_target_branch(r);
            let msg = persistent_stop_cleared(reason, base_is_clean(&path), base_is_pushed(&path, &base));
            if let Some(m) = msg {
                if let Some(ref d) = rt {
                    if std::fs::remove_file(d.join("stop")).is_ok() {
                        stop_pending = false;
                        actions.push(format!("{name} auto-recover: {m}"));
                    }
                    // OSError -> pass (leave stop_pending as-is)
                }
            }
        }
    }

    let mut restarted = false;
    if should_restart(running, &hb, paused, stop_pending) {
        let res = control::runner::start(r, auto_push_flag, false);
        // restarted = bool(res.get("ok") and not res.get("already"))
        restarted = res.get("ok").and_then(Value::as_bool).unwrap_or(false)
            && !res.get("already").and_then(Value::as_bool).unwrap_or(false);
        if restarted {
            actions.push(format!(
                "restarted {name} (pid {})",
                py_repr(res.get("pid"))
            ));
        } else {
            actions.push(format!(
                "restart {name} FAILED: {}",
                py_repr(res.get("error"))
            ));
        }
    }

    // RUNG-0 deterministic recovery (never a pi fix here: allow_pi=False). Skip ALL auto-action on an
    // operator-PAUSED lane — `paused` means "hands off this lane for the automated sweep", so the
    // watchdog must not stop/restart/reset_to_base it. should_restart already honors paused; recover()
    // did NOT, so a paused lane could still be stomped.
    if !paused {
        // solomon.recover(r, allow_pi=False, allow_restart=auto_push, auto_push=auto_push).
        // catch Exception -> a watchdog must never die on one bad repo. recover() is total (no panics
        // expected); the catch is reproduced as a guard around the Value field reads.
        let rec = supervisor::recover(r, false, auto_push_flag, auto_push_flag);
        if let Some(taken) = rec.get("actions_taken").and_then(Value::as_array) {
            if !taken.is_empty() {
                let joined = taken
                    .iter()
                    .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| py_repr(Some(v))))
                    .collect::<Vec<_>>()
                    .join(",");
                actions.push(format!("{name} recover: {joined}"));
            }
        }
        if json_truthy(rec.get("escalate").unwrap_or(&Value::Null)) {
            actions.push(format!("{name} ESCALATED: {}", py_repr(rec.get("category"))));
        }
    }

    // hist = control.read_history(r, limit=1); last = hist[-1] if hist else {}
    let hist = control::heartbeat::read_history(r, 1);
    let last = hist.last().cloned().unwrap_or_else(|| json!({}));
    let hb2 = control::heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));
    // diag = solomon.diagnose(r).get("category"); on Exception -> "?"
    let diag = supervisor::diagnose(r)
        .get("category")
        .cloned()
        .unwrap_or(Value::Null);

    let running2 = control::locks::is_running(r);
    let snap = json!({
        "ts": now(),
        "repo": name,
        "running": running2,
        "restarted": restarted,
        "paused": paused,
        "status": hb2.get("status").cloned().unwrap_or(Value::Null),
        "phase": hb2.get("phase").cloned().unwrap_or(Value::Null),
        "iteration": hb2.get("iteration").cloned().unwrap_or(Value::Null),
        "last_status": last.get("status").cloned().unwrap_or(Value::Null),
        "diagnosis": diag,
    });

    // STALL DETECTOR: a lane that is STILL RUNNING but has repeated the SAME preflight refusal
    // (status=error, phase=preflight) every sweep is wedged. Compare across RECENT sweeps: if this
    // snap AND the prior STALL_SWEEPS-1 TEMPORALLY-ADJACENT snaps are all error/preflight, escalate
    // (do NOT auto-restart — a restart just re-hits the refusal). The max_age_s recency bound stops
    // stale snaps from a resolved wedge days ago from being miscounted as a fresh streak. Anti-thrash:
    // escalate once (skip if an escalation.json with this category already exists).
    if snap.get("running").and_then(Value::as_bool) == Some(true)
        && snap.get("status").and_then(Value::as_str) == Some("error")
        && snap.get("phase").and_then(Value::as_str) == Some("preflight")
    {
        // window = _recent_snapshots(name, STALL_SWEEPS-1, max_age_s=STALL_SWEEPS*600) + [snap]
        let mut window = recent_snapshots(
            &name,
            STALL_SWEEPS - 1,
            Some((STALL_SWEEPS as f64) * 600.0),
        );
        window.push(snap.clone());
        let all_preflight = window.iter().all(|s| {
            s.get("status").and_then(Value::as_str) == Some("error")
                && s.get("phase").and_then(Value::as_str) == Some("preflight")
        });
        if window.len() >= STALL_SWEEPS && all_preflight {
            let existing = supervisor::read_escalation(r).unwrap_or_else(|| json!({}));
            if existing.get("category").and_then(Value::as_str) != Some("running_stalled") {
                actions.push(format!(
                    "{name} STALLED: stuck in preflight for {STALL_SWEEPS} sweeps"
                ));
                // hb2.get('last_summary') or '' then [:200]
                let last_summary = hb2.get("last_summary").and_then(Value::as_str).unwrap_or("");
                let evidence = format!(
                    "running but stuck in preflight for {STALL_SWEEPS} consecutive sweeps — {}",
                    py_slice_200(last_summary)
                );
                // solomon._write_escalation(r, {...}); catch Exception -> pass.
                supervisor::write_escalation(
                    r,
                    &json!({"category": "running_stalled", "evidence": evidence}),
                );
            }
        }
    }

    // DEGRADED HEALTH CLASSIFIER: a lane whose PID is alive but whose last HEALTH_K iterations
    // all failed to ship AND were every one a timeout or a gate-RED is degraded, not healthy.
    // Pure observability — emits a per-lane reason in the watchdog line instead of the blanket
    // "all healthy"; does NOT auto-restart or auto-merge (ship=pr stays manual). This is the fix for
    // "stale heartbeat/escalation state" + "lanes thrashing without a real fix" being silently
    // reported as healthy.
    if running2 {
        let hist_k = control::heartbeat::read_history(r, HEALTH_K);
        if let Some(reason) = lane_health(true, &hist_k, HEALTH_K) {
            actions.push(format!("{name} {reason}"));
        }
    }

    (actions, snap)
}

/// monitor.sweep: one watchdog pass over all repos. Returns {ts, disabled, actions, snapshots}.
pub fn sweep() -> Value {
    if disabled_path().exists() {
        return json!({"ts": now(), "disabled": true, "actions": [], "snapshots": []});
    }
    let auto_push_flag = auto_push();
    let mut actions: Vec<String> = Vec::new();
    let mut snapshots: Vec<Value> = Vec::new();
    for r in control::registry::load_repos() {
        // if not isinstance(r, dict) or not r.get("name"): continue
        if !r.is_object() {
            continue;
        }
        let has_name = r
            .get("name")
            .map(|n| json_truthy(n))
            .unwrap_or(false);
        if !has_name {
            continue;
        }
        // Per-repo guard so one bad repo doesn't abort the whole sweep — matches monitor.py's
        // per-repo try/except (sweep(), lines 218-222): a failure in one repo is logged as a
        // "<name> sweep error: <e>" action and the sweep continues to the next repo. This is the
        // crash-recovery layer, so a panic in one repo's recover()/start() must NOT stop the others
        // from being restarted. (Requires unwinding panics — see [profile.release] in Cargo.toml.)
        let name = r.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep_repo(&r, auto_push_flag))) {
            Ok((repo_actions, snap)) => {
                actions.extend(repo_actions);
                snapshots.push(snap);
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("panic");
                actions.push(format!("{name} sweep error: {msg}"));
            }
        }
    }
    json!({"ts": now(), "disabled": false, "actions": actions, "snapshots": snapshots})
}

/// monitor.main: the scheduled-task entry. Appends snapshots to runtime/_monitor.jsonl and the
/// human-readable line to runtime/_watchdog.out.log. Returns the process exit code.
pub fn main() -> i32 {
    let out = sweep();
    if out.get("disabled").and_then(Value::as_bool) == Some(true) {
        println!(
            "{} watchdog disabled (kill-switch present) — no action",
            out.get("ts").and_then(Value::as_str).unwrap_or("")
        );
        return 0;
    }
    let snapshots = out
        .get("snapshots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // os.makedirs(dirname(MON_LOG), exist_ok=True); append each snapshot as a JSON line. OSError -> pass.
    let log = mon_log();
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        for snap in &snapshots {
            writeln!(f, "{}", serde_json::to_string(snap).unwrap_or_default())?;
        }
        Ok(())
    })();

    // summary = "; ".join(actions) if actions else "all healthy, no action"
    let actions: Vec<String> = out
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let summary = if actions.is_empty() {
        "all healthy, no action".to_string()
    } else {
        actions.join("; ")
    };
    // running = sum(1 for s in snapshots if s["running"])
    let running = snapshots
        .iter()
        .filter(|s| s.get("running").and_then(Value::as_bool) == Some(true))
        .count();
    let line = format!(
        "{} watchdog: {}/{} running | {}",
        out.get("ts").and_then(Value::as_str).unwrap_or(""),
        running,
        snapshots.len(),
        summary
    );
    println!("{line}");
    // self-log so the task can run windowless (no stdout redirection needed). OSError -> pass.
    let _ = (|| -> std::io::Result<()> {
        use std::io::Write;
        let out_log = paths::here().join("runtime").join("_watchdog.out.log");
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out_log)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
    // SELF-REDEPLOY: the periodic check that swaps Solomon's OWN production binary when the checkout
    // is behind origin/main, but ONLY in a safe drain window (no lane mid-ship, no live-money lane
    // with an open trade). Cheap when there is nothing to do (cooldown + single-flight guards no-op
    // most sweeps); logs LOUDLY to _watchdog.out.log when a rebuild is staged but no safe window
    // appears, so production running old code is surfaced rather than silent. Never forces an unsafe
    // swap. See `redeploy::maybe_self_redeploy` for the hard invariant.
    crate::redeploy::maybe_self_redeploy();
    0
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// Python f-string interpolation of a possibly-missing dict value (`res.get('pid')`, etc.). A JSON
/// string renders without quotes; a missing key / null renders as Python's `None`; numbers/bools
/// render with their canonical Python text.
fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python `s[:200]` over a `str` — slice by Unicode code points, not bytes (so multibyte chars in a
/// last_summary are never split mid-character).
fn py_slice_200(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- should_restart decision table (pure logic) --------
    #[test]
    fn should_restart_live_crash_restarts() {
        // iterating/sleeping/idle/error (not reverted) all restart when not running/paused/stopping.
        for status in ["iterating", "sleeping", "idle", "error"] {
            assert!(
                should_restart(false, &json!({"status": status}), false, false),
                "status={status} should restart"
            );
        }
    }

    #[test]
    fn should_restart_running_paused_or_stopping_never() {
        let hb = json!({"status": "iterating"});
        assert!(!should_restart(true, &hb, false, false)); // running
        assert!(!should_restart(false, &hb, true, false)); // paused
        assert!(!should_restart(false, &hb, false, true)); // stop pending
    }

    #[test]
    fn should_restart_stopped_or_no_heartbeat_left_alone() {
        // clean exit
        assert!(!should_restart(false, &json!({"status": "stopped"}), false, false));
        // no heartbeat -> {} -> no status -> left alone
        assert!(!should_restart(false, &json!({}), false, false));
        // status missing / null / empty-string -> falsy -> left alone
        assert!(!should_restart(false, &json!({"status": null}), false, false));
        assert!(!should_restart(false, &json!({"status": ""}), false, false));
    }

    #[test]
    fn should_restart_revert_halt_left_alone() {
        // error + phase=reverted -> revert-failure HALT, operator cleanup, not a restart
        assert!(!should_restart(
            false,
            &json!({"status": "error", "phase": "reverted"}),
            false,
            false
        ));
        // error with a non-reverted phase still restarts (transient/retryable)
        assert!(should_restart(
            false,
            &json!({"status": "error", "phase": "preflight"}),
            false,
            false
        ));
    }

    // -------- persistent_stop_cleared: re-observe-after-fix policy --------
    // The watchdog must clear a persistent-bail self-stop sentinel ONLY when the underlying cause
    // is actually healed, so a stale stop_lingering escalation does not outlive the fix. Pure
    // policy under test; the git condition checks are passed in as bools.
    #[test]
    fn persistent_stop_cleared_dirty_when_clean() {
        // dirty_base_persistent + base now clean -> cleared (byte-identical legacy message).
        assert_eq!(
            persistent_stop_cleared("dirty_base_persistent", true, false),
            Some("base clean again — cleared dirty_base_persistent stop")
        );
        // still dirty -> leave the stop (do not thrash).
        assert_eq!(persistent_stop_cleared("dirty_base_persistent", false, false), None);
    }

    #[test]
    fn persistent_stop_cleared_unpushed_when_pushed() {
        // unpushed_base_persistent + base no longer ahead of origin -> cleared.
        assert_eq!(
            persistent_stop_cleared("unpushed_base_persistent", false, true),
            Some("base pushed again — cleared unpushed_base_persistent stop")
        );
        // still ahead -> leave the stop.
        assert_eq!(persistent_stop_cleared("unpushed_base_persistent", false, false), None);
    }

    #[test]
    fn persistent_stop_cleared_ignores_other_reasons() {
        // base_gate_red_persistent is NOT re-observed here (running the gate is too expensive /
        // gate-cmd-specific for the watchdog sweep) -> stays stopped until a human clicks Start.
        assert_eq!(persistent_stop_cleared("base_gate_red_persistent", true, true), None);
        // a true operator Stop carries no reason marker -> never auto-cleared.
        assert_eq!(persistent_stop_cleared("", true, true), None);
        assert_eq!(persistent_stop_cleared("anything_else", true, true), None);
    }

    // -------- _auto_push truthiness --------
    #[test]
    fn json_truthy_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(!json_truthy(&json!(false)));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(0)));
        assert!(json_truthy(&json!(1)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
        assert!(!json_truthy(&json!([])));
        assert!(json_truthy(&json!([1])));
        assert!(!json_truthy(&json!({})));
        assert!(json_truthy(&json!({"a": 1})));
    }

    // -------- py_repr f-string rendering --------
    #[test]
    fn py_repr_renders_like_python_fstring() {
        assert_eq!(py_repr(None), "None");
        assert_eq!(py_repr(Some(&Value::Null)), "None");
        assert_eq!(py_repr(Some(&json!("abc"))), "abc");
        assert_eq!(py_repr(Some(&json!(1234))), "1234");
        assert_eq!(py_repr(Some(&json!(true))), "True");
    }

    // -------- py_slice_200 unicode-safe truncation --------
    #[test]
    fn py_slice_200_counts_code_points() {
        let s: String = "é".repeat(250); // 250 chars, 500 bytes
        assert_eq!(py_slice_200(&s).chars().count(), 200);
        assert_eq!(py_slice_200("short").as_str(), "short");
    }

    // -------- _recent_snapshots filtering + windowing --------
    // Exercises the pure parse/filter/window logic against an ISOLATED temp _monitor.jsonl via
    // recent_snapshots_from — fully hermetic (no dependence on the real MON_LOG).
    #[test]
    fn recent_snapshots_filters_by_repo_and_age_and_window() {
        let name = "wd_test_recent_snaps_unique";
        // Hermetic: write to an ISOLATED temp log and read it via recent_snapshots_from — never the
        // real mon_log — so concurrent tests, repeated `cargo test` runs, and the live watchdog can
        // neither bleed records in nor accumulate residue.
        let log = std::env::temp_dir()
            .join(format!("solomon_wd_snaps_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let fresh = now();
        let old = (Utc::now() - chrono::Duration::seconds(99999))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        // Append: two fresh for our name, one old for our name, one fresh for another repo, one junk.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writeln!(f, "{}", json!({"ts": old, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": "someone_else", "status": "x"})).unwrap();
        writeln!(f, "not json").unwrap();
        drop(f);

        // No age bound: all 3 of our records, last 2.
        assert_eq!(recent_snapshots_from(&log, name, 2, None).len(), 2);

        // Age bound 600s: the old record is dropped; only the 2 fresh remain; take last 2.
        let got = recent_snapshots_from(&log, name, 2, Some(600.0));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.get("repo").and_then(Value::as_str) == Some(name)));

        // k larger than available -> all (after age filter).
        assert_eq!(recent_snapshots_from(&log, name, 50, Some(600.0)).len(), 2);

        let _ = std::fs::remove_file(&log);
    }

    // -------- lane_health degraded classifier (pure predicate) --------
    // Exercises the healthy-vs-degraded decision over synthetic history slices — no IO.
    fn hist_rec(status: &str, green: Option<bool>, summary: &str) -> Value {
        let mut rec = json!({"status": status, "summary": summary});
        if let Some(g) = green {
            rec["tests"] = json!({"green": g});
        }
        rec
    }

    #[test]
    fn lane_health_all_timeouts_is_degraded() {
        // 3 consecutive pi timeouts (noop + "timed out" summary) → degraded:3x timed_out
        let h = vec![
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
        ];
        assert_eq!(
            lane_health(true, &h, 3),
            Some("degraded:3x timed_out".to_string())
        );
    }

    #[test]
    fn lane_health_all_gate_red_is_degraded() {
        // 3 consecutive gate-RED reverts → degraded:3x gate-RED
        let h = vec![
            hist_rec("reverted", Some(false), "Reverted — tests failed (2 failed)."),
            hist_rec("reverted", Some(false), "Reverted — tests failed (1 failed)."),
            hist_rec("reverted", Some(false), "Reverted — tests failed (3 failed)."),
        ];
        assert_eq!(
            lane_health(true, &h, 3),
            Some("degraded:3x gate-RED".to_string())
        );
    }

    #[test]
    fn lane_health_mixed_timeout_and_gate_red_is_degraded() {
        let h = vec![
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("reverted", Some(false), "Reverted — tests failed (1 failed)."),
            hist_rec("noop", None, "Pi session timed out."),
        ];
        assert_eq!(
            lane_health(true, &h, 3),
            Some("degraded:3x (2x timed_out, 1x gate-RED)".to_string())
        );
    }

    #[test]
    fn lane_health_ship_in_window_is_healthy() {
        // A ship in the last K breaks the streak — even with 2 timeouts before it.
        let h = vec![
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("shipped", Some(true), "Green. All tests pass."),
        ];
        assert_eq!(lane_health(true, &h, 3), None);
    }

    #[test]
    fn lane_health_non_timeout_noop_is_healthy() {
        // A noop that is NOT a timeout (model no-op, beautify) does not count as degradation.
        let h = vec![
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "(no summary returned)"),
            hist_rec("noop", None, "Pi session timed out."),
        ];
        assert_eq!(lane_health(true, &h, 3), None);
    }

    #[test]
    fn lane_health_non_gate_red_revert_is_healthy() {
        // A revert with tests.green == true (leak-guard, anti-gaming, eval gate) is not gate-RED.
        let h = vec![
            hist_rec("reverted", Some(false), "Reverted — tests failed (1 failed)."),
            hist_rec("reverted", Some(true), "Reverted — leak guard: secret detected."),
            hist_rec("reverted", Some(false), "Reverted — tests failed (2 failed)."),
        ];
        assert_eq!(lane_health(true, &h, 3), None);
    }

    #[test]
    fn lane_health_error_or_blocked_breaks_streak() {
        // error / blocked / stopped are not timeout/gate-RED — they break the streak.
        for bad in ["error", "blocked", "stopped"] {
            let h = vec![
                hist_rec("noop", None, "Pi session timed out."),
                hist_rec(bad, None, "something"),
                hist_rec("noop", None, "Pi session timed out."),
            ];
            assert_eq!(lane_health(true, &h, 3), None, "status={bad} should be healthy");
        }
    }

    #[test]
    fn lane_health_not_running_is_healthy() {
        // A dead lane is the should_restart/recover path, not a degraded-health signal.
        let h = vec![
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
        ];
        assert_eq!(lane_health(false, &h, 3), None);
    }

    #[test]
    fn lane_health_insufficient_history_is_healthy() {
        // Fewer than K records → unknown, not degraded (no false alarm on a fresh lane).
        let h = vec![hist_rec("noop", None, "Pi session timed out.")];
        assert_eq!(lane_health(true, &h, 3), None);
    }

    #[test]
    fn lane_health_k_zero_is_healthy() {
        let h = vec![hist_rec("noop", None, "Pi session timed out.")];
        assert_eq!(lane_health(true, &h, 0), None);
    }

    #[test]
    fn lane_health_takes_last_k() {
        // 5 records: 2 ships, then 3 timeouts. The last 3 are all timeouts → degraded.
        let h = vec![
            hist_rec("shipped", Some(true), "Green."),
            hist_rec("shipped", Some(true), "Green."),
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
            hist_rec("noop", None, "Pi session timed out."),
        ];
        assert_eq!(
            lane_health(true, &h, 3),
            Some("degraded:3x timed_out".to_string())
        );
    }
}
