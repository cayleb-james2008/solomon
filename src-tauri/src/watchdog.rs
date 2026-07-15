//! Native Rust port of `monitor.py` — Solomon's watchdog + data collector.
//!
//! Behavior is bug-for-bug with `monitor.py`. Runs every ~2 min INSIDE the visibly-open
//! Solomon.exe (the run_gui tick thread) AND every 5 min out-of-band via the Solomon Sentinel
//! scheduled task (`tools/install_sentinel.ps1` -> `solomon watchdog`; renegotiated by the
//! operator 2026-07-06 after the liveness autopsy — GUI-tick-only liveness caused the 8h/25.5h
//! watchdog gaps, so the GUI tick is now the SECONDARY layer), or on demand via `solomon
//! watchdog`. Each sweep, for every registered repo, it:
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
//!
//! A clean Stop from the GUI already sets `status="stopped"` and is respected automatically.
//!
//! The returns that back the JSONL/log are `serde_json::Value` whose keys are byte-identical to the
//! Python dicts; status/category/reason strings are quoted verbatim from `monitor.py`.
//!
//! Wired to the `solomon watchdog` subcommand (main.rs) and the in-app run_gui tick thread
//! (every 2 minutes while the app is open). Some helpers are exercised only by tests, so allow
//! dead-code for this module.
#![allow(dead_code)]

use crate::control::{self, paths};
use crate::supervisor;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// monitor.STALL_SWEEPS — consecutive error/preflight sweeps (incl. this one) that mark a lane STALLED.
const STALL_SWEEPS: usize = 3;

/// Max crash-restarts per sweep (the disk-meltdown guard — see sweep_repo). At most this many lanes
/// are (re)started in one 2-min sweep; the rest defer to later sweeps so their cargo build gates
/// never stack. 2 allows a little parallelism while staying far under the 6-at-once that melted the
/// disk on 2026-07-02.
pub const MAX_LANE_RESTARTS_PER_SWEEP: usize = 2;

/// HEALTH_K — the iteration window the degraded-health classifier inspects. A lane whose last
/// `HEALTH_K` iterations all failed to ship AND were every one a timeout or a gate-RED is
/// `degraded:<reason>`, not `healthy` — even when its PID is alive. Pure observability: the
/// watchdog line surfaces the per-lane reason; it never auto-restarts or auto-merges on it.
const HEALTH_K: usize = 3;

/// STANDSTILL_S — the fleet-standstill threshold (seconds). When the newest lane iteration across
/// ALL repos is older than this while lanes should be running, the fleet has silently frozen (the
/// June failure mode: loops "running" but shipping nothing for a day and a half). 3 h is long enough
/// that a slow pi session or a sleeping lane never trips it, short enough that a real wedge pages the
/// operator the same day instead of after a multi-day post-mortem.
const STANDSTILL_S: f64 = 3.0 * 3600.0;

/// The out-of-band `solomon watchdog` one-shot (the 5-min Sentinel scheduled task) exits the instant
/// `main()` returns — killing every detached graft thread. `main()` bounded-joins the CEO graft for at
/// most this long so the Sentinel path RELIABLY lands the CEO fast core's side effects (the D11 warm
/// append the carry-forward depends on) before exit. The core is deterministic + bounded (file IO +
/// git bounded at 30 s + notify at 15 s) and normally finishes in well under a second, so this is only
/// a ceiling for a pathological hang — on timeout `main()` proceeds and the thread dies with the
/// process (Sentinel) or lingers single-flighted (GUI). The GUI tick (long-lived) already lets the core
/// finish, so the join returns immediately there.
const CEO_CORE_JOIN_TIMEOUT_S: u64 = 60;

static CEO_GRAFT_RUNNING: AtomicBool = AtomicBool::new(false);
static HOUSEKEEPING_GRAFT_RUNNING: AtomicBool = AtomicBool::new(false);
/// Single-flight guard for the autopilot dispatch graft — one `fleet::once()` at a time, so a
/// slow AI job (the ~30-min run-improver subprocess) can never pile up detached threads. With
/// max_concurrent_agent_calls=2 the fleet's own AutopilotLease allows a 2nd concurrent job; this
/// guard just prevents the TICK itself from stacking a 3rd dispatch while the first two are still
/// running. Mirrors `spawn_ceo_slow_tail` / `spawn_watchdog_graft`.
static AUTOPILOT_DISPATCH_RUNNING: AtomicBool = AtomicBool::new(false);

/// monitor.DISABLED — global kill-switch path: HERE/runtime/_watchdog.disabled.
fn disabled_path() -> std::path::PathBuf {
    paths::here().join("runtime").join("_watchdog.disabled")
}

/// monitor.MON_LOG — HERE/runtime/_monitor.jsonl.
fn mon_log() -> std::path::PathBuf {
    paths::here().join("runtime").join("_monitor.jsonl")
}

/// HERE/runtime/_standstill.marker — the once-until-recovery dedup marker for the fleet-standstill
/// alarm. Present == the operator has already been paged for the current standstill; absent == armed.
fn standstill_marker() -> std::path::PathBuf {
    paths::here().join("runtime").join("_standstill.marker")
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
///     restart just re-hits the known-bad tree);
///   - no heartbeat                 — a repo that never ran (the watchdog keeps enabled loops alive,
///     it does not auto-enable new ones).
///
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
///
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

fn sweep_autopilot(auto_push_flag: bool) -> Value {
    let repos = control::registry::load_repos();
    let snapshots = crate::fleet::sweep_snapshots(&repos);
    // OFFLOAD the autopilot dispatch (fleet::once) to a detached, single-flighted thread so the
    // watchdog tick returns immediately — the AI job (a ~30-min run-improver subprocess) no longer
    // blocks the 60s tick. The fleet's own AutopilotLease (max_concurrent_agent_calls=2) governs how
    // many concurrent jobs actually run; this single-flight guard just prevents the TICK from
    // stacking a 3rd dispatch while the first two are still running. Mirrors `spawn_ceo_slow_tail`
    // (ceo.rs) and `spawn_watchdog_graft` below. If a prior dispatch is still running, the new tick
    // skips dispatch but STILL runs the watchdog's other duties (crash-restart, recover heal).
    let dispatch_actions = spawn_autopilot_dispatch(auto_push_flag);
    let mut actions: Vec<Value> = dispatch_actions;
    if actions.is_empty() {
        let state = crate::fleet::state();
        let queued = state
            .get("queue")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false);
        if queued {
            actions.push(json!("autopilot queue checked"));
        }
    }
    // AUTOPILOT PERMANENT-STUCK HEAL (2026-07-07 audit): `fleet::once` above routes a lane that
    // diagnoses `noop_streak`→`auto_safe=false` to an inert `proof_required` job that mutates
    // nothing — and the whole crash-restart + recover() machinery of the non-autopilot sweep is
    // bypassed by `sweep()`'s early `return out`. So on the autopilot path a dead/stuck lane was
    // NEVER restarted and NEVER healed: a permanent absorbing state the scheduler re-diagnosed
    // every sweep. Reach the EXISTING supervisor recover() heal here, after the fleet lease is
    // released, gated by a per-lane stuck-sweep counter (time-decay) and a `noop_streak`-only
    // diagnose filter (freshness discipline — evidence-starved categories keep parking). The heal
    // itself (stop→ideate→restart) and its `prior_heals<3` backoff are unchanged and reused.
    let heal_actions = autopilot_recover_pass(&repos, auto_push_flag);
    for a in heal_actions {
        actions.push(json!(a));
    }
    json!({"ts": now(), "disabled": false, "actions": actions, "snapshots": snapshots, "autopilot": crate::fleet::state(), "fleet": crate::fleet::state()})
}

/// Spawn the autopilot dispatch (`fleet::once`) on its OWN detached, single-flighted thread —
/// mirrors `spawn_ceo_slow_tail` (ceo.rs). If a prior dispatch is still running (a slow AI job),
/// the spawn is a NO-OP: the fleet's AutopilotLease governs concurrency (max 2); this guard just
/// prevents the 60s tick from stacking a 3rd dispatch. Returns immediately with a placeholder
/// action so the watchdog line records that a dispatch was attempted (or skipped as single-flighted).
fn spawn_autopilot_dispatch(auto_push_flag: bool) -> Vec<Value> {
    if AUTOPILOT_DISPATCH_RUNNING.swap(true, Ordering::SeqCst) {
        return vec![json!("autopilot dispatch skipped (prior dispatch still running)")];
    }
    std::thread::spawn(move || {
        let _guard = AutopilotDispatchGuard;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = crate::fleet::once(auto_push_flag, None);
        }));
    });
    vec![json!("autopilot dispatched")]
}

/// Reset the single-flight flag when the autopilot dispatch thread finishes (or panics) — mirrors
/// `GraftFlagGuard`, so a panicking dispatch can never wedge the flag true and starve every future
/// dispatch.
struct AutopilotDispatchGuard;
impl Drop for AutopilotDispatchGuard {
    fn drop(&mut self) {
        AUTOPILOT_DISPATCH_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// The autopilot force-heal pass (production wiring). Reads the real autopilot targets and the
/// per-lane stuck counter the fleet persists, then delegates the decision to
/// `autopilot_recover_pass_inner` with the live `fleet::stuck_sweeps` / `fleet::reset_stuck_sweeps`
/// dependencies injected. Kept thin so the decision logic below is unit-testable in isolation of
/// the real `runtime/autopilot_state.json` and `autopilot_config()`.
fn autopilot_recover_pass(repos: &[Value], auto_push_flag: bool) -> Vec<String> {
    let targets = control::registry::autopilot_targets();
    autopilot_recover_pass_inner(
        repos,
        &targets,
        auto_push_flag,
        crate::fleet::STUCK_SWEEP_THRESHOLD,
        &|name| crate::fleet::stuck_sweeps(name),
        &|name| crate::fleet::reset_stuck_sweeps(name),
    )
}

/// Decision core of the autopilot force-heal. For each autopilot target that the fleet has left
/// stuck as `proof_required` for at least `threshold` sweeps AND that currently diagnoses
/// `noop_streak`, run the existing `supervisor::recover()` heal (stop→ideate→restart) under the
/// shared per-sweep restart budget, then reset that lane's stuck counter via `reset_stuck`. Returns
/// the action strings to append to the sweep's `actions`. The `stuck_sweeps` / `reset_stuck`
/// closures are injected so tests can drive the gate deterministically without touching global
/// autopilot state.
///
/// Guardrails (all reused, none weakened):
///   - stuck-counter gate (`threshold` = `STUCK_SWEEP_THRESHOLD`): only every Nth sweep — the
///     time-decay the permanent-trap lacked. A lane must be stuck this long before ANY heal fires.
///   - `noop_streak`-only diagnose filter: `needs_goal`/`metric_unobservable`/`quota_error` and
///     every other evidence-gated category are NOT force-cycled — they keep parking/escalating.
///   - shared `MAX_LANE_RESTARTS_PER_SWEEP` budget via `with_restart_budget` (disk-meltdown guard).
///   - `recover()`'s own `prior_heals<3` exponential backoff → after 3 ideate-heals it pages
///     instead of a 4th heal, so no force-cycle-forever.
///   - operator-pause guard: an explicitly paused lane (`runtime/<name>/paused`) is skipped
///     BEFORE diagnose/recover — recover() does NOT honor the pause sentinel itself, so this
///     pass enforces it here, mirroring the crash-restart path's `if !paused` recover guard.
///   - `recover()` itself defers cleanly on an in-flight iteration.
fn autopilot_recover_pass_inner(
    repos: &[Value],
    targets: &[String],
    auto_push_flag: bool,
    threshold: u64,
    stuck_sweeps: &dyn Fn(&str) -> u64,
    reset_stuck: &dyn Fn(&str) -> std::io::Result<()>,
) -> Vec<String> {
    if targets.is_empty() {
        return Vec::new();
    }
    let mut actions: Vec<String> = Vec::new();
    // Arm the SAME shared per-sweep budget the crash-restart path uses so a sweep where several
    // lanes are stuck can't fire more than MAX_LANE_RESTARTS_PER_SWEEP heavy stop→ideate→restart
    // spawns total. recover() spends via spend_restart_budget() inside this armed scope.
    supervisor::with_restart_budget(MAX_LANE_RESTARTS_PER_SWEEP, || {
        for name in targets {
            // Resolve the target NAME to its registry repo dict (diagnose/recover need the Value).
            let Some(r) = repos
                .iter()
                .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
            else {
                continue;
            };
            // Operator-pause guard (mirrors the crash-restart path's `if !paused` at the recover()
            // call — see the `paused` guard where sweep_repo runs its RUNG-0 recover). `paused`
            // means "hands off this lane for the automated sweep": a human dropped
            // `runtime/<name>/paused` to hold it. recover() does NOT honor that sentinel itself
            // (it stops→ideates→restarts unconditionally), so WITHOUT this skip a lane the
            // operator explicitly paused would still be force-stopped→ideated→restarted — a
            // safety-gate weakening. Skip BEFORE diagnose/recover so a paused lane costs nothing
            // and is never touched.
            let paused = paths::runtime_dir(r)
                .map(|d| d.join("paused").exists())
                .unwrap_or(false);
            if paused {
                continue;
            }
            // Time-decay gate: only heal a lane that has been proof_required for >= threshold
            // sweeps. Below threshold, leave it to the fleet (anti-thrash — no heal every sweep).
            if stuck_sweeps(name) < threshold {
                continue;
            }
            // Freshness discipline: only the specific stuck cause (`noop_streak`) is force-healed.
            // recover() would itself route other categories, but gating HERE keeps the force-cycle
            // strictly scoped to the permanent-trap cause and avoids re-cycling a lane whose real
            // blocker is evidence-starved (needs_goal / metric_unobservable / quota_error).
            let cat = supervisor::diagnose(r)
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if cat != "noop_streak" {
                continue;
            }
            // Run the existing heal — identical arg shape to sweep_repo's call (allow_pi=false,
            // allow_restart=auto_push, auto_push=auto_push). It stops→ideates→restarts, backs off
            // via prior_heals<3, and spends the shared restart budget.
            let rec = supervisor::recover(r, false, auto_push_flag, auto_push_flag);
            let mut healed = false;
            if let Some(taken) = rec.get("actions_taken").and_then(Value::as_array) {
                if !taken.is_empty() {
                    let joined = taken
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| py_repr(Some(v)))
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    actions.push(format!("{name} recover: {joined}"));
                    // A real heal took place (stop/ideate/restart) — reset the time-decay window so
                    // the lane isn't re-force-cycled next sweep before its relaunched loop can prove
                    // itself. A pure `restart_deferred` (budget spent) is also non-empty but must NOT
                    // reset — the heal didn't actually run, so keep the counter to retry next sweep.
                    healed = taken
                        .iter()
                        .any(|v| v.as_str() == Some("restart") || v.as_str() == Some("ideate"));
                }
            }
            if json_truthy(rec.get("escalate").unwrap_or(&Value::Null)) {
                actions.push(format!("{name} ESCALATED: {}", py_repr(rec.get("category"))));
            }
            if healed {
                if let Err(e) = reset_stuck(name) {
                    actions.push(format!("{name} stuck-counter reset FAILED: {e}"));
                }
            }
        }
    });
    actions
}

fn ops_needs_attention(payload: &Value) -> bool {
    payload
        .get("projects")
        .and_then(Value::as_object)
        .map(|projects| {
            projects.values().any(|p| {
                matches!(
                    p.get("status").and_then(Value::as_str),
                    Some("red" | "yellow")
                )
            })
        })
        .unwrap_or(false)
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

/// True iff the repo's gate command passes on the current checkout. Used to auto-recover a
/// `base_gate_red_persistent` self-stop ONLY once the operator has actually fixed the gate, so
/// clearing the stop can never thrash (a still-RED gate stays stopped). Bounded to 120 s so a hung
/// gate does not stall the sweep. Safe on any error (spawn failure, timeout, missing interpreter):
/// returns False, leaving the stop in place.
fn base_gate_green(path: &str, gate_cmd: Option<&str>) -> bool {
    if path.is_empty() || !Path::new(path).is_dir() {
        return false;
    }
    let timeout = Duration::from_secs(120);
    // Custom GATE_CMD: run via shell (compound syntax), cwd=path. Default: python -m pytest.
    let result = if let Some(cmd) = gate_cmd.filter(|s| !s.is_empty()) {
        #[cfg(windows)]
        {
            // Pass the gate string to cmd.exe VERBATIM (raw_arg) — Rust's own arg quoting is not
            // cmd.exe's parser, so a gate with an embedded quoted path-with-spaces would be mangled
            // and spuriously fail (a silent false-RED that pins recovery).
            control::proc::run_win_shell(cmd, Some(Path::new(path)), Some(timeout))
        }
        #[cfg(not(windows))]
        {
            control::proc::run(
                &["/bin/sh", "-c", cmd],
                Some(Path::new(path)),
                Some(timeout),
            )
        }
    } else {
        control::proc::run(
            &["python", "-m", "pytest", "-o", "addopts="],
            Some(Path::new(path)),
            Some(timeout),
        )
    };
    match result {
        Ok(r) => r.code == 0,
        Err(_) => false, // spawn failure / timeout -> False, leave the stop in place
    }
}

/// The persistent-bail self-stop re-observation policy. The runner self-stops (STOP sentinel +
/// `status=error` + a `reason` marker) on a PERSISTENTLY dirty / un-pushed / gate-RED base so it
/// doesn't spin forever — but that sentinel otherwise pins the lane DEAD until a human clicks Start,
/// leaving a stale heartbeat that `diagnose` keeps re-escalating as `stop_lingering` even after the
/// operator has fixed the cause. The watchdog re-observes the underlying condition each sweep and
/// clears the sentinel once the cause is ACTUALLY healed, so the lane resumes on the next
/// `should_restart` instead of lingering in a stale escalation.
///
/// Returns the auto-recover action message (the suffix after `"{name} auto-recover: "`) when the
/// sentinel should be cleared for this `reason`, else `None`. Pure — the git/gate condition checks
/// are passed in as bools — so the policy is unit-tested. Only fires on the runner's own reason
/// markers AND a verified-healed condition, so it never thrashes and never touches a true operator
/// Stop (status="stopped", no reason).
fn persistent_stop_cleared(
    reason: &str,
    base_clean: bool,
    base_pushed: bool,
    base_gate_green: bool,
    controller_clean: bool,
) -> Option<&'static str> {
    match reason {
        "dirty_base_persistent" if base_clean => {
            Some("base clean again — cleared dirty_base_persistent stop")
        }
        "unpushed_base_persistent" if base_pushed => {
            Some("base pushed again — cleared unpushed_base_persistent stop")
        }
        "base_gate_red_persistent" if base_gate_green => {
            Some("base gate green again — cleared base_gate_red_persistent stop")
        }
        // CONTROLLER SELF-HONESTY (D0): only clears once the controller tree is GENUINELY reconciled
        // — on the resolved default branch AND clean AND pushed (exactly `controller_clean` == Ok).
        // A page-only or surface-only path can NEVER satisfy this, so the stop clears strictly after
        // a real merge-to-default + push.
        "controller_off_base_persistent" if controller_clean => {
            Some("controller tree reconciled (on-base + pushed + clean) — cleared controller_off_base_persistent stop")
        }
        _ => None,
    }
}

/// monitor._sweep_repo: process ONE repo for sweep(); returns (actions, snap). The caller runs this
/// inside a blanket try/except so one bad repo can never abort the whole sweep — the module's
/// documented 'a watchdog must never die on one bad repo' contract.
///
/// `restart_budget` rate-limits crash-restarts ACROSS the sweep: each restart decrements it, and a
/// lane that would restart while the budget is exhausted is DEFERRED to a later sweep (2 min apart)
/// instead of started now. This is the fix for the 2026-07-02 disk meltdown, where the watchdog
/// restarted all 6 crashed lanes in ONE sweep -> 6 simultaneous cargo builds saturated the disk and
/// froze the live trader. Spreading restarts across sweeps (with staggered lane intervals) keeps
/// heavy build gates from ever stacking. A deferred lane is not lost — the next sweep retries it.
fn sweep_repo(
    r: &Value,
    auto_push_flag: bool,
    restart_budget: &std::cell::Cell<usize>,
) -> (Vec<String>, Value) {
    let mut actions: Vec<String> = Vec::new();
    // name = r["name"] — the caller guarantees a truthy name before calling.
    let name = r
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
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
    let mut hb = control::heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));
    // Set true when THIS sweep healed a persistent-bail self-stop (persistent_stop_cleared removed
    // the sentinel + should_restart relaunched the loop). When healed, the RUNG-0 recover() pass
    // below is SKIPPED for this repo on this sweep — see the heal block + the recover guard.
    let mut healed = false;

    // Stale-heartbeat recovery (2026-07-10 MoA): when the improver process died silently (the PID
    // is dead, the lock is gone) but the heartbeat is frozen at "iterating"/"sleeping"/"idle",
    // the autopilot's plan_jobs sees a live-phase status and won't re-queue the lane as
    // `implement` — it thinks a job is already running. Clear the frozen heartbeat to "stopped"
    // so the next sweep's diagnose() sees a clean exit + plan_jobs queues `implement`. This is
    // the RUNG-0 reversible path: we only touch the heartbeat (a state file), never the repo.
    // Guard: only fire when the heartbeat is STALE (updated_at > 3x the lane interval) so a
    // slow-but-live iteration (the pi agent is still working, just hasn't written in a while)
    // is NOT mistaken for a dead process. The `is_running` check already confirmed the PID is
    // dead; the age guard adds a second safety margin.
    if !running {
        let status = hb.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "iterating" || status == "sleeping" || status == "idle" {
            let interval = crate::control::registry::project_interval(r);
            let stale_threshold = (3.0 * interval as f64).max(paths::LOCK_LIVE_FLOOR_S);
            let hb_age = control::heartbeat::heartbeat_age(&hb);
            let is_stale = hb_age.map(|a| a > stale_threshold).unwrap_or(false);
            if is_stale {
                if let Some(rt) = &rt {
                    let hb_path = rt.join("heartbeat.json");
                    if hb_path.exists() {
                        let mut fixed = hb.clone();
                        if let Value::Object(ref mut o) = fixed {
                            o.insert("status".into(), json!("stopped"));
                            o.insert("phase".into(), Value::Null);
                            if let Some(Value::Object(ref mut pio)) = o.get_mut("pi") {
                                pio.insert("status".into(), json!("exited"));
                                pio.insert("exit_code".into(), json!(-1));
                            }
                            let _ = std::fs::write(&hb_path, serde_json::to_vec(&fixed).unwrap_or_default());
                            actions.push(format!("{name} stale-heartbeat: cleared frozen '{status}' -> 'stopped' (dead PID, age {:.0}s)", hb_age.unwrap_or(0.0)));
                            hb = fixed;
                        }
                    }
                }
            }
        }
    }

    // Anti-wedge: the runner self-stops on a PERSISTENTLY dirty / un-pushed / gate-RED base (STOP
    // sentinel + status=error + a reason marker) so it doesn't spin forever — but that sentinel
    // otherwise pins the lane DEAD until a human clicks Start, leaving a stale heartbeat that
    // diagnose keeps re-escalating as stop_lingering even AFTER the operator has fixed the cause.
    // Re-observe the underlying condition each sweep and clear the self-written sentinel once the
    // cause is actually healed, so should_restart heals the lane automatically. Safe: only fires on
    // the runner's own reason marker AND a verified-healed condition (see persistent_stop_cleared),
    // so it never thrashes and never touches a true operator Stop (status="stopped", no reason).
    if !running
        && !paused
        && stop_pending
        && hb.get("status").and_then(Value::as_str) == Some("error")
    {
        if let Some(reason) = hb.get("reason").and_then(Value::as_str) {
            let path = paths::repo_path(r);
            // Resolve the base branch HONESTLY (git remote default), falling back to repos.json's
            // pr_target_branch — so the pushed check below is never asked against a wrong branch for
            // a `master`-default repo (D0).
            let base = control::registry::project_resolved_base_branch(r, &path);
            // For base_gate_red_persistent, run the gate to verify the operator's fix actually
            // landed; for dirty/unpushed, the cheap git checks suffice. Only run the gate when the
            // reason is base_gate_red_persistent (the gate is expensive, ~120 s worst case).
            let gate_green = if reason == "base_gate_red_persistent" {
                base_gate_green(&path, control::registry::project_gate(r).as_deref())
            } else {
                false
            };
            // For controller_off_base_persistent (solomon's OWN lane), re-observe the full
            // controller-clean preflight — on-base + pushed + clean — so the stop clears strictly
            // after a real reconcile. Only computed for that reason (git spawn) and only meaningful
            // for the solomon lane, which is the only lane that writes that reason.
            let controller_clean = reason == "controller_off_base_persistent"
                && name == "solomon"
                && crate::provenance::controller_clean().is_ok();
            let msg = persistent_stop_cleared(
                reason,
                base_is_clean(&path),
                base_is_pushed(&path, &base),
                gate_green,
                controller_clean,
            );
            if let Some(m) = msg {
                if let Some(ref d) = rt {
                    if std::fs::remove_file(d.join("stop")).is_ok() {
                        stop_pending = false;
                        healed = true;
                        actions.push(format!("{name} auto-recover: {m}"));
                    }
                    // OSError -> pass (leave stop_pending as-is)
                }
            }
        }
    }

    let mut restarted = false;
    if should_restart(running, &hb, paused, stop_pending) {
        if restart_budget.get() == 0 {
            // Budget exhausted this sweep — defer to avoid stacking heavy build gates (see the
            // meltdown note on the signature). The lane stays down; the next sweep retries it.
            actions.push(format!(
                "{name} restart DEFERRED (restart budget spent this sweep — retries next sweep)"
            ));
        } else {
            let res = control::runner::start(r, auto_push_flag, false);
            // restarted = bool(res.get("ok") and not res.get("already"))
            restarted = res.get("ok").and_then(Value::as_bool).unwrap_or(false)
                && !res.get("already").and_then(Value::as_bool).unwrap_or(false);
            if restarted {
                // Only a REAL start (spawned a new process) spends budget — an "already running"
                // no-op or a failed start must not consume a slot.
                restart_budget.set(restart_budget.get().saturating_sub(1));
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
    }

    // RUNG-0 deterministic recovery (never a pi fix here: allow_pi=False). Skip ALL auto-action on an
    // operator-PAUSED lane — `paused` means "hands off this lane for the automated sweep", so the
    // watchdog must not stop/restart/reset_to_base it. should_restart already honors paused; recover()
    // did NOT, so a paused lane could still be stomped.
    //
    // ALSO skip recover() on the sweep that JUST healed a persistent-bail self-stop (healed=true):
    // the heartbeat still carries the PRE-heal error (status=error, phase=preflight,
    // reason=<persistent>) because the relaunched loop hasn't written a fresh one yet, so recover()
    // -> diagnose() would re-observe that stale heartbeat and either escalate it as
    // unknown_error/stuck or stop+restart the lane we just restarted — the exact "stale heartbeat /
    // escalation state not re-observed after a fix lands" thrash. The next sweep re-observes the
    // fresh heartbeat the relaunched loop has since written and runs recover() normally.
    // Snapshot the escalation category BEFORE recover() runs. recover() overwrites
    // escalation.json with its own diagnosis (e.g. "unknown_error") on an error/preflight lane, so
    // reading it AFTER recover() would never see a prior "running_stalled" the stall detector wrote
    // on a previous sweep — defeating the stall detector's anti-thrash check (it would re-emit the
    // "STALLED" action and re-write the escalation every sweep instead of escalating once).
    let pre_recover_escalation_cat = supervisor::read_escalation(r)
        .and_then(|e| {
            e.get("category")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    if !paused && !healed {
        // solomon.recover(r, allow_pi=False, allow_restart=auto_push, auto_push=auto_push).
        // catch Exception -> a watchdog must never die on one bad repo. recover() is total (no panics
        // expected); the catch is reproduced as a guard around the Value field reads.
        //
        // recover()'s restart / fix-session paths spawn heavy cargo-gated processes. Run them under
        // the SAME per-sweep budget the crash-restart path uses (share the one Cell) so a sweep where
        // several lanes diagnose as stuck/noop can't fire more than MAX_LANE_RESTARTS_PER_SWEEP heavy
        // spawns total. Arm the thread-local from the shared budget, then write the remainder back.
        let rec = supervisor::with_restart_budget(restart_budget.get(), || {
            let out = supervisor::recover(r, false, auto_push_flag, auto_push_flag);
            restart_budget.set(supervisor::restart_budget_remaining());
            out
        });
        if let Some(taken) = rec.get("actions_taken").and_then(Value::as_array) {
            if !taken.is_empty() {
                let joined = taken
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| py_repr(Some(v)))
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                actions.push(format!("{name} recover: {joined}"));
            }
        }
        if json_truthy(rec.get("escalate").unwrap_or(&Value::Null)) {
            actions.push(format!(
                "{name} ESCALATED: {}",
                py_repr(rec.get("category"))
            ));
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
        let mut window =
            recent_snapshots(&name, STALL_SWEEPS - 1, Some((STALL_SWEEPS as f64) * 600.0));
        window.push(snap.clone());
        let all_preflight = window.iter().all(|s| {
            s.get("status").and_then(Value::as_str) == Some("error")
                && s.get("phase").and_then(Value::as_str) == Some("preflight")
        });
        if window.len() >= STALL_SWEEPS && all_preflight {
            // Anti-thrash: skip if an escalation with this category was ALREADY present before this
            // sweep's recover() overwrote it. Using the pre-recover snapshot (not a fresh read here)
            // is load-bearing: recover() runs before the stall detector and overwrites escalation.json
            // with its own diagnosis (e.g. "unknown_error"), so a fresh read would never see the
            // "running_stalled" from a prior sweep and the anti-thrash would never fire — the stall
            // action would be re-emitted every sweep.
            if pre_recover_escalation_cat != "running_stalled" {
                actions.push(format!(
                    "{name} STALLED: stuck in preflight for {STALL_SWEEPS} sweeps"
                ));
                // hb2.get('last_summary') or '' then [:200]
                let last_summary = hb2
                    .get("last_summary")
                    .and_then(Value::as_str)
                    .unwrap_or("");
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

// --------------------------------------------------------------------------- //
// RSI-v3 grafts (controller preflight + janitor + config provenance)
// --------------------------------------------------------------------------- //

/// Janitor cadence stamp: `<HERE>/runtime/_janitor.stamp`, refreshed after each completed pass.
const JANITOR_INTERVAL_S: u64 = 6 * 3600;

/// CONTROLLER-CLEAN PREFLIGHT (catalog #6): surface + page (marker-deduped, and confirmed across
/// TWO consecutive probes — see [`controller_dirty_page_decision`]) when Solomon's OWN
/// tree is dirty or off-base. Only surfaces — the sweep's restart-liveness duties always still run
/// (liveness is never hostage to hygiene); the hard refusal lives in improver::run for the solomon
/// lane. Skipped while the solomon lane's runner lock is LIVE: a gated rsi/* iteration legitimately
/// dirties the tree mid-flight — the failure mode is an ABANDONED dirty/off-base controller (the
/// 601-uncommitted-lines incident), i.e. dirt with no live gated run. Paging every legit iteration
/// would be catalog-#8 alert-flood.
fn controller_preflight_actions() -> Vec<String> {
    let solomon_running = control::registry::load_repos().iter().any(|r| {
        r.get("name").and_then(Value::as_str) == Some("solomon") && control::locks::is_running(r)
    });
    if solomon_running {
        return Vec::new();
    }
    let marker = paths::here().join("runtime").join("_controller_dirty_paged");
    let pending = paths::here().join("runtime").join("_controller_dirty_pending");
    match crate::provenance::controller_clean() {
        Err(detail) => {
            let (page_now, stamp_pending) =
                controller_dirty_page_decision(marker.exists(), pending.exists());
            let write_stamp = |p: &std::path::Path| {
                let _ = (|| -> std::io::Result<()> {
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(p, now())
                })();
            };
            if page_now {
                let _ = crate::notify::send(&crate::notify::Notice::red(
                    "Solomon: controller tree dirty/off-base".into(),
                    detail.clone(),
                ));
                write_stamp(&marker);
                let _ = std::fs::remove_file(&pending);
            } else if stamp_pending {
                write_stamp(&pending);
            }
            vec![format!(
                "CONTROLLER DIRTY: {detail} — engine refuses meta-work until the controller tree is committed/on-base"
            )]
        }
        Ok(()) => {
            // Recovered: clear the pending stamp AND the dedupe marker so the next dirty state
            // starts a fresh two-probe confirmation.
            let _ = std::fs::remove_file(&pending);
            let _ = std::fs::remove_file(&marker);
            Vec::new()
        }
    }
}

/// TWO-CONSECUTIVE-PROBE page confirmation for the controller-dirty page (skeptic finding 6,
/// 2026-07-06): the engine's own tmp+rename writes to watched config (proc.rs) race the sweep's
/// `git status` on Windows — a single-probe page turned every race into a false RED (observed
/// live 17:45Z: the sweep saw `[ D repos.json]` for a file that was present and clean seconds
/// later), catalog #8's alert-fatigue generator. Dirt is PAGED only when seen on two consecutive
/// sweeps: the first sighting stamps `_controller_dirty_pending` and pages nothing. The dirty
/// SURFACE line (and improver::run's hard refusal) still fires on every probe — only the page is
/// confirmation-gated. Returns `(page_now, stamp_pending)`. Pure — unit-tested.
fn controller_dirty_page_decision(already_paged: bool, pending: bool) -> (bool, bool) {
    if already_paged {
        (false, false) // page already sent for this dirty episode — dedupe
    } else if pending {
        (true, false) // second consecutive dirty probe — confirmed real, page now
    } else {
        (false, true) // first sighting — could be a tmp+rename race; wait for the next probe
    }
}

/// JANITOR (requirement 5): run at most every 6 h, riding whichever cadence fires first (GUI tick
/// or Sentinel). The stamp is written AFTER a completed pass so a crashed pass retries next sweep.
fn janitor_graft_actions() -> Vec<String> {
    let stamp = paths::here().join("runtime").join("_janitor.stamp");
    let fresh = std::fs::metadata(&stamp)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|d| d.as_secs() < JANITOR_INTERVAL_S)
        .unwrap_or(false);
    if fresh {
        return Vec::new();
    }
    let out = crate::janitor::sweep();
    let _ = (|| -> std::io::Result<()> {
        if let Some(p) = stamp.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&stamp, now())
    })();
    out.get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// CONFIG-PROVENANCE TRIPWIRE (catalog #6): one check per sweep; actions only on transitions.
fn provenance_graft_actions() -> Vec<String> {
    crate::provenance::check()
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// All three grafts, each catch_unwind-isolated: a hygiene/provenance failure only ever loses its
/// own action strings — it can NEVER abort the crash-restart sweep below it.
fn rsi_v3_grafts() -> Vec<String> {
    let mut actions: Vec<String> = Vec::new();
    for graft in [
        controller_preflight_actions as fn() -> Vec<String>,
        janitor_graft_actions,
        provenance_graft_actions,
    ] {
        if let Ok(mut a) = std::panic::catch_unwind(graft) {
            actions.append(&mut a);
        }
    }
    actions
}

/// monitor.sweep: one watchdog pass over all repos. Returns {ts, disabled, actions, snapshots}.
pub fn sweep() -> Value {
    if disabled_path().exists() {
        return json!({"ts": now(), "disabled": true, "actions": [], "snapshots": []});
    }
    // RSI-v3 grafts ride EVERY sweep (both cadences, both scheduler modes): controller-clean
    // preflight first (its action is prepended), then the 6h-stamped janitor, then the config
    // provenance tripwire. None of them can abort the crash-restart duties below.
    let graft_actions = rsi_v3_grafts();
    let auto_push_flag = auto_push();
    if control::registry::autopilot_enabled() {
        let mut out = sweep_autopilot(auto_push_flag);
        if !graft_actions.is_empty() {
            if let Some(arr) = out.get_mut("actions").and_then(Value::as_array_mut) {
                for (i, a) in graft_actions.iter().enumerate() {
                    arr.insert(i, json!(a));
                }
            }
        }
        return out;
    }
    let mut actions: Vec<String> = graft_actions;
    let mut snapshots: Vec<Value> = Vec::new();
    // Crash-restart budget for THIS sweep (disk-meltdown guard — see sweep_repo). Shared across
    // every repo; each real (re)start spends one, and lanes over budget defer to a later sweep.
    let restart_budget = std::cell::Cell::new(MAX_LANE_RESTARTS_PER_SWEEP);
    for r in control::registry::load_repos() {
        // if not isinstance(r, dict) or not r.get("name"): continue
        if !r.is_object() {
            continue;
        }
        let has_name = r.get("name").map(json_truthy).unwrap_or(false);
        if !has_name {
            continue;
        }
        // Per-repo guard so one bad repo doesn't abort the whole sweep — matches monitor.py's
        // per-repo try/except (sweep(), lines 218-222): a failure in one repo is logged as a
        // "<name> sweep error: <e>" action and the sweep continues to the next repo. This is the
        // crash-recovery layer, so a panic in one repo's recover()/start() must NOT stop the others
        // from being restarted. (Requires unwinding panics — see [profile.release] in Cargo.toml.)
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sweep_repo(&r, auto_push_flag, &restart_budget)
        })) {
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
    // SENTINEL HEARTBEAT (dead-man visibility): EVERY watchdog run — the GUI tick or the
    // out-of-band Solomon Sentinel scheduled task — stamps runtime/_sentinel_heartbeat.json, so
    // "when did a sweep last actually run?" is answerable from disk even when both stdout and the
    // GUI are gone (the 8h/25.5h-gap failure had no such record). Best-effort, OSError -> pass.
    let n_actions = out
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let _ = (|| -> std::io::Result<()> {
        let p = paths::here().join("runtime").join("_sentinel_heartbeat.json");
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &p,
            serde_json::to_string(&json!({
                "ts": now(),
                "pid": std::process::id(),
                "actions": n_actions,
            }))
            .unwrap_or_default(),
        )
    })();
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
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)?;
        for snap in &snapshots {
            writeln!(f, "{}", serde_json::to_string(snap).unwrap_or_default())?;
        }
        Ok(())
    })();

    // summary = "; ".join(actions) if actions else "all healthy, no action"
    let actions: Vec<String> = out
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut summary = if actions.is_empty() {
        "all healthy, no action".to_string()
    } else {
        actions.join("; ")
    };
    // running = sum(1 for s in snapshots if s["running"])
    let running = snapshots
        .iter()
        .filter(|s| s.get("running").and_then(Value::as_bool) == Some(true))
        .count();
    // OPS PLANE GRAFT (Phase 1): after the code-plane sweep, run the ground-truth outcome probes
    // over the live fleet (ops::outcomes::sweep — all probes, all projects) and append the ops
    // summary so the watchdog line is TWO-PLANE truth: "code 5/6 running | ops: asmodeus
    // RED(fills_recency) ...". A running loop with a dead product can never print "all healthy"
    // again. The Solomon Sentinel scheduled task (tools/install_sentinel.ps1) runs `solomon
    // watchdog` every 5 minutes out-of-band; the GUI tick sweep remains as a secondary layer
    // (renegotiated by the operator 2026-07-06 after the liveness autopsy). The first sweep after
    // process start writes the blind-window gap into runtime/ops_status.json. catch_unwind
    // mirrors the per-repo guard
    // above: an ops-plane failure must never abort the code-plane crash-recovery sweep.
    // Run the ops sweep once and KEEP the payload (not just the summary): the managed-app redeploy
    // graft below reads the same fresh per-project rollups (deploy-gap + process-down) this sweep
    // computed, so it never re-runs the probes. catch_unwind isolates an ops-plane panic exactly as
    // sweep_and_summarize did; on a panic we fall back to an empty payload -> the summary reads
    // "no probes configured" and the deploy graft no-ops.
    let ops_payload = std::panic::catch_unwind(crate::ops::outcomes::sweep)
        .unwrap_or_else(|_| json!({"projects": {}}));
    let ops_summary = crate::ops::outcomes::payload_summary(&ops_payload);
    if summary == "all healthy, no action" && ops_needs_attention(&ops_payload) {
        summary = "ops red/yellow; Solomon Autopilot proof required".to_string();
    }
    let line = format!(
        "{} watchdog: {}/{} running | {} | ops: {}",
        out.get("ts").and_then(Value::as_str).unwrap_or(""),
        running,
        snapshots.len(),
        summary,
        ops_summary
    );
    println!("{line}");
    // self-log so the task can run windowless (no stdout redirection needed). OSError -> pass.
    let _ = (|| -> std::io::Result<()> {
        use std::io::Write;
        let out_log = paths::here().join("runtime").join("_watchdog.out.log");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&out_log)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
    // FLEET-STANDSTILL ALARM: the code-plane sweep restarts CRASHED lanes and the ops plane pages on
    // a dead PRODUCT, but neither pages when every lane is "running" yet the whole fleet has silently
    // frozen — nothing shipping for hours (the June "running but posted nothing for 37 h" outage that
    // never reached the operator). One loud page when the fleet is entirely down OR its newest ship is
    // older than STANDSTILL_S, deduped by a persisted marker so a standing standstill never re-pages
    // every 2 min. catch_unwind + best-effort marker IO mirror the ops graft — a standstill check must
    // never abort crash-recovery.
    let _ = std::panic::catch_unwind(|| {
        let now = Utc::now();
        standstill_alarm(newest_lane_age_s(now), running, snapshots.len(), now);
    });
    // HOST-INDEPENDENT LIVENESS FLOOR (catalog #4): the standstill alarm above only PAGES when the
    // fleet is dark — it never relaunches the GUI/CEO host that went down. This is the missing
    // ACTUATION: read the engine-host heartbeat (stamped ONLY by the GUI tick, never by this
    // out-of-band sentinel), and if it is stale beyond T AND the recorded host PID is gone, relaunch
    // Solomon.exe and page ONCE (marker-deduped). A dead-man tripwire with no authority — it reads
    // two files + the process table and spawns the SELF binary; it cannot trade/whitelist/budget or
    // touch any gate. Runs on the out-of-band Solomon Sentinel sweep — the ONE path guaranteed to
    // fire when the host is dead — and is a cheap no-op (heartbeat fresh -> Wait) when the host is up.
    // catch_unwind mirrors the standstill alarm: a resurrector failure must never abort the sweep.
    let _ = std::panic::catch_unwind(crate::resurrector::run);
    // CEO RHYTHM GRAFT (v2 Phase B): after the two-plane sweep, the day-gated morning plan +
    // evening verified-outcome summary (see ceo::tick — cheap no-op on all but two sweeps a day).
    // catch_unwind mirrors the ops graft: a CEO failure must never abort crash-recovery.
    // Capture the CEO graft handle so we can bounded-join its FAST deterministic core before main()
    // returns (below). ceo::tick now runs a fast core (the D11 warm append + ops-RED/hygiene/scale) and
    // OFFLOADS its slow LLM/produce sub-grafts to a single-flighted tail, so it returns within ms — the
    // join lands the core on the out-of-band `solomon watchdog` one-shot (which exits the instant main()
    // returns), and is a near-instant no-op on the long-lived GUI tick.
    let ceo_graft = spawn_watchdog_graft(&CEO_GRAFT_RUNNING, crate::ceo::tick);
    // HOUSEKEEPING GRAFT (v2): day-gated (04:00) storage sweep — worktree prune, merged rsi/
    // branches, stale/oversized build dirs (see housekeeping.rs). Same isolation contract.
    let _ = spawn_watchdog_graft(&HOUSEKEEPING_GRAFT_RUNNING, crate::housekeeping::tick);
    // MANAGED-APP REDEPLOY GRAFT: close the "fix merged but never reaches the running app" deadlock
    // — rebuild+relaunch a managed repo's LIVE app binary when the deployed binary is stale (a
    // deploy-gap probe) AND the app is down (a process probe), but ONLY for a repo carrying a
    // live_deploy config (opt-in; asmodeus/live-money stays human-gated), in a safe drain window,
    // past its cooldown, and at most ONE per sweep across all repos (a cargo build is heavy). Reads
    // the fresh ops_payload this sweep already computed. Spawned on its own thread — see the
    // SELF-REDEPLOY comment below for why a cargo build must never run on the tick thread itself.
    // catch_unwind mirrors the ops graft: a deploy failure must never abort crash-recovery. See
    // deploy::maybe_redeploy_managed_apps.
    let ops_payload_for_deploy = ops_payload.clone();
    std::thread::spawn(move || {
        let _ = std::panic::catch_unwind(|| {
            crate::deploy::maybe_redeploy_managed_apps(&ops_payload_for_deploy)
        });
    });
    // SELF-REDEPLOY: the periodic check that swaps Solomon's OWN production binary when the checkout
    // is behind origin/main, but ONLY in a safe drain window (no lane mid-ship, no live-money lane
    // with an open trade). Cheap when there is nothing to do (cooldown + single-flight guards no-op
    // most sweeps); logs LOUDLY to _watchdog.out.log when a rebuild is staged but no safe window
    // appears, so production running old code is surfaced rather than silent. Never forces an unsafe
    // swap. See `redeploy::maybe_self_redeploy` for the hard invariant.
    //
    // Both this graft and the deploy graft above can run a `cargo build --release` (bounded at 30
    // min) and, for self-redeploy, an additional busy-wait for a safe drain window (up to
    // DRAIN_WAIT_MINS) — spawned on their own threads so a rebuild can never freeze the 2-min tick's
    // crash-recovery / heartbeat collection / standstill alarm for the rest of the fleet (the
    // 2026-07-03 standstill incidents). Each already single-flights via its own lock-file + cooldown
    // guard (redeploy_in_progress / deploy's cooldown marker), so spawning a checker thread every
    // tick is safe — the common case (nothing to do) returns almost immediately.
    std::thread::spawn(crate::redeploy::maybe_self_redeploy);
    // SENTINEL LANDING (Sentinel-path fix): wait (bounded) for the CEO fast core to land its side
    // effects — chiefly the D11 warm append the carry-forward reads back next wake — before we return.
    // On the out-of-band `solomon watchdog` one-shot the process exits the instant main() returns,
    // killing every detached graft thread; without this the CEO core (and its append) almost never
    // lands from the Sentinel path. The core is fast + bounded, so this returns in milliseconds; the
    // timeout only caps a pathological hang. Runs LAST so the deploy/self-redeploy checker threads above
    // spawn concurrently first.
    join_graft_bounded(ceo_graft, Duration::from_secs(CEO_CORE_JOIN_TIMEOUT_S));
    0
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

struct GraftFlagGuard(&'static AtomicBool);

impl Drop for GraftFlagGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn spawn_watchdog_graft<F>(running: &'static AtomicBool, f: F) -> Option<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    if running.swap(true, Ordering::SeqCst) {
        return None; // already running — single-flight, no new thread
    }
    Some(std::thread::spawn(move || {
        let _guard = GraftFlagGuard(running);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }))
}

/// Wait at most `timeout` for a spawned graft to finish. Rust threads can't be cancelled, so on
/// timeout we leave the thread running and return: on the out-of-band `solomon watchdog` one-shot the
/// thread dies with the process; on the long-lived GUI it is single-flighted, so it can't pile up.
/// Used for the CEO graft so the Sentinel path lands the fast core's side effects before exit. Cheap
/// in the common case (the fast core finishes in milliseconds); a `None` handle (graft was already
/// single-flighted out) is a no-op.
fn join_graft_bounded(handle: Option<std::thread::JoinHandle<()>>, timeout: Duration) {
    let handle = match handle {
        Some(h) => h,
        None => return,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = handle.join();
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
}

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

// --------------------------------------------------------------------------- //
// fleet-standstill alarm
// --------------------------------------------------------------------------- //

/// The fleet-standstill DECISION (pure — unit-tested). A standstill is either:
///   - the FLEET IS ENTIRELY DOWN: at least one lane is configured (`total > 0`) yet none are
///     running (`running == 0`) — nothing can ship at all; or
///   - the NEWEST lane iteration across the whole fleet is older than `threshold_s` while lanes
///     are meant to be running (`total > 0`) — loops alive but silently shipping nothing (the June
///     "running but frozen for 37 h" failure mode).
///
/// `newest_age_s` is the age (seconds) of the freshest lane iteration across all repos, or `None`
/// when NO repo has ever iterated (a fresh install / wiped runtime — treated as a stale fleet, not a
/// panic). Returns the human page body when a standstill is present, else `None`. An empty fleet
/// (`total == 0`) is never a standstill — there is nothing to be down.
fn standstill_reason(
    newest_age_s: Option<f64>,
    running: usize,
    total: usize,
    threshold_s: f64,
) -> Option<String> {
    if total == 0 {
        return None; // no lanes configured — nothing can stand still
    }
    if running == 0 {
        return Some(format!("fleet entirely DOWN — 0/{total} lanes running"));
    }
    match newest_age_s {
        Some(age) if age > threshold_s => Some(format!(
            "no lane has iterated in {:.1} h ({running}/{total} running but frozen)",
            age / 3600.0
        )),
        None => Some(format!(
            "no lane has EVER iterated ({running}/{total} running but no history)"
        )),
        _ => None,
    }
}

/// The age (seconds) of the freshest lane iteration across every repo, or `None` when no repo has a
/// parseable `last_iteration_ts`. Reuses the ledger's `lane_activity` reader so the "newest ship"
/// number is the same one the outcomes ledger reports (one source of truth for lane freshness).
fn newest_lane_age_s(now: DateTime<Utc>) -> Option<f64> {
    let mut newest: Option<DateTime<Utc>> = None;
    for r in control::registry::load_repos() {
        let name = match r.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let history = paths::here()
            .join("runtime")
            .join(name)
            .join("history.jsonl");
        let ts_raw = crate::ops::ledger::lane_activity(&history, now)
            .get("last_iteration_ts")
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(ts) = ts_raw.as_str() {
            if let Ok(t) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
                let t = t.and_utc();
                if newest.map(|p| t > p).unwrap_or(true) {
                    newest = Some(t);
                }
            }
        }
    }
    newest.map(|t| ((now - t).num_milliseconds() as f64 / 1000.0).max(0.0))
}

/// Fire ONE loud operator page when the fleet has stood still, deduped by a persisted marker so a
/// PERSISTING standstill never re-pages every 2-min sweep (the same log-once contract as the ops
/// incident dedupe). The marker is created on the paging sweep and REMOVED the moment the fleet
/// recovers, re-arming the alarm for the next standstill. Never panics (marker IO -> pass), never
/// fails a sweep — matches the notify::send best-effort contract.
///
/// `running`/`total`/`newest_age_s` come straight from the sweep; `newest_age_s` and `now` are
/// passed IN (not read from disk here) so the alarm is hermetically testable — recovery requires
/// running==total AND a fresh lane iteration, and the test controls the latter directly.
fn standstill_alarm(newest_age_s: Option<f64>, running: usize, total: usize, now: DateTime<Utc>) {
    let reason = standstill_reason(newest_age_s, running, total, STANDSTILL_S);
    let marker = standstill_marker();
    match reason {
        Some(body) => {
            // Already paged for this standstill? The marker is the dedupe state — page once.
            if marker.exists() {
                return;
            }
            let _ = crate::notify::send(&crate::notify::Notice::red(
                "Solomon: fleet STANDSTILL".into(),
                body,
            ));
            // Arm the dedupe marker (create-only; OSError -> pass, best-effort like every notify IO).
            let _ = (|| -> std::io::Result<()> {
                if let Some(parent) = marker.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&marker, now.format("%Y-%m-%dT%H:%M:%SZ").to_string())?;
                Ok(())
            })();
        }
        None => {
            // Recovered (or never stood still): clear the marker so the next standstill re-pages.
            let _ = std::fs::remove_file(&marker);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- controller-dirty page: two-consecutive-probe confirmation (pure logic) --------
    // A single tmp+rename race (the engine's own repos.json write vs the sweep's git status) must
    // NOT page; only dirt confirmed on two consecutive probes may, and once paged it stays deduped
    // for the episode. (skeptic finding 6, 2026-07-06 — the periodic false-RED generator.)
    #[test]
    fn controller_dirty_pages_only_on_the_second_consecutive_probe() {
        // first sighting: no page, stamp pending
        assert_eq!(controller_dirty_page_decision(false, false), (false, true));
        // second consecutive sighting: confirmed — page, consume pending
        assert_eq!(controller_dirty_page_decision(false, true), (true, false));
        // already paged this episode: dedupe regardless of pending state
        assert_eq!(controller_dirty_page_decision(true, false), (false, false));
        assert_eq!(controller_dirty_page_decision(true, true), (false, false));
    }

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
        assert!(!should_restart(
            false,
            &json!({"status": "stopped"}),
            false,
            false
        ));
        // no heartbeat -> {} -> no status -> left alone
        assert!(!should_restart(false, &json!({}), false, false));
        // status missing / null / empty-string -> falsy -> left alone
        assert!(!should_restart(
            false,
            &json!({"status": null}),
            false,
            false
        ));
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
            persistent_stop_cleared("dirty_base_persistent", true, false, false, false),
            Some("base clean again — cleared dirty_base_persistent stop")
        );
        // still dirty -> leave the stop (do not thrash).
        assert_eq!(
            persistent_stop_cleared("dirty_base_persistent", false, false, false, false),
            None
        );
    }

    #[test]
    fn persistent_stop_cleared_unpushed_when_pushed() {
        // unpushed_base_persistent + base no longer ahead of origin -> cleared.
        assert_eq!(
            persistent_stop_cleared("unpushed_base_persistent", false, true, false, false),
            Some("base pushed again — cleared unpushed_base_persistent stop")
        );
        // still ahead -> leave the stop.
        assert_eq!(
            persistent_stop_cleared("unpushed_base_persistent", false, false, false, false),
            None
        );
    }

    #[test]
    fn persistent_stop_cleared_gate_red_when_green() {
        // base_gate_red_persistent + base gate now green -> cleared.
        assert_eq!(
            persistent_stop_cleared("base_gate_red_persistent", false, false, true, false),
            Some("base gate green again — cleared base_gate_red_persistent stop")
        );
        // gate still red -> leave the stop (do not thrash).
        assert_eq!(
            persistent_stop_cleared("base_gate_red_persistent", false, false, false, false),
            None
        );
    }

    #[test]
    fn persistent_stop_cleared_controller_off_base_when_reconciled() {
        // controller_off_base_persistent + controller tree now reconciled -> cleared. This is the
        // D0 self-honesty stop: it clears STRICTLY when controller_clean() is Ok (on-base + pushed
        // + clean), never on a page-only path.
        assert_eq!(
            persistent_stop_cleared("controller_off_base_persistent", false, false, false, true),
            Some("controller tree reconciled (on-base + pushed + clean) — cleared controller_off_base_persistent stop")
        );
        // still off-base/dirty/un-pushed (controller_clean still Err) -> leave the stop. Crucially,
        // base_clean/base_pushed/base_gate_green being true is NOT enough — only the full
        // controller_clean re-observation clears this reason.
        assert_eq!(
            persistent_stop_cleared("controller_off_base_persistent", true, true, true, false),
            None
        );
    }

    #[test]
    fn persistent_stop_cleared_ignores_other_reasons() {
        // a true operator Stop carries no reason marker -> never auto-cleared.
        assert_eq!(persistent_stop_cleared("", true, true, true, true), None);
        assert_eq!(
            persistent_stop_cleared("anything_else", true, true, true, true),
            None
        );
    }

    // -------- D0: on-base + pushed is a START PRECONDITION for the controller's OWN lane --------
    // Acceptance (e): a controller left off its DEFAULT branch with an un-pushed commit must be
    // treated as NOT a valid start state, and only merging that work to the RESOLVED default branch
    // + pushing satisfies the precondition. The default branch is resolved DYNAMICALLY from git
    // (`registry::resolve_default_branch` -> origin/HEAD) — NOT a hardcoded "main"/"master" — so this
    // test is correct whether the host git names the initial branch `main` or `master`. Drives real
    // git over a bare remote; asserts the two building blocks the sweep's controller heal composes:
    // `resolve_default_branch` (the dynamic name) and `base_is_pushed` (against that resolved name).
    #[test]
    fn controller_on_base_and_pushed_is_a_start_precondition_dynamic_default() {
        use std::process::Command;
        let tag = format!(
            "wd_ctrl_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        let root = std::env::temp_dir().join(format!("solomon_{tag}"));
        let remote = std::env::temp_dir().join(format!("solomon_{tag}_remote.git"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str], dir: &std::path::Path| {
            let st = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?} failed in {dir:?}");
        };
        // bare remote + a work repo pushed to it, so origin/HEAD is populated.
        Command::new("git")
            .args(["init", "--bare", &remote.to_string_lossy()])
            .status()
            .unwrap();
        git(&["init"], &root);
        git(&["config", "user.email", "t@t"], &root);
        git(&["config", "user.name", "t"], &root);
        git(&["commit", "--allow-empty", "-m", "init"], &root);
        git(&["remote", "add", "origin", &remote.to_string_lossy()], &root);
        // Push the CURRENT branch (whatever git named it) and record it as origin's default HEAD.
        let cur = String::from_utf8(
            Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&root)
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        git(&["push", "-u", "origin", &cur], &root);
        git(
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{cur}"),
            ],
            &root,
        );

        let root_s = root.to_string_lossy().into_owned();
        // The resolver returns the ACTUAL default — no hardcoded name — and it matches the branch
        // git chose for us (main OR master, per host config). This is the load-bearing dynamic bit.
        let default = control::registry::resolve_default_branch(&root_s)
            .expect("origin/HEAD resolves the default branch");
        assert_eq!(default, cur, "resolver must return git's real default branch");

        // ON DEFAULT + PUSHED: the precondition is satisfied.
        assert!(base_is_clean(&root_s), "fresh checkout is clean");
        assert!(
            base_is_pushed(&root_s, &default),
            "on the resolved default with nothing ahead of origin -> pushed"
        );

        // OFF-BASE + UN-PUSHED: commit onto an rsi/* working branch (the live D0 failure shape).
        git(&["checkout", "-b", "rsi/off-base-work"], &root);
        git(&["commit", "--allow-empty", "-m", "operator: off-base work"], &root);
        // The default branch itself is still level with origin, but HEAD is NOT the default, and the
        // off-base commit is un-pushed — so this is NOT a valid controller start state. persistent
        // stop policy must NOT clear a controller stop while the tree is unreconciled.
        assert_ne!(cur, "rsi/off-base-work");
        assert_eq!(
            persistent_stop_cleared("controller_off_base_persistent", true, true, true, false),
            None,
            "an unreconciled controller tree must NOT clear its self-stop"
        );

        // RECONCILE: merge the off-base work to the resolved default and push it.
        git(&["checkout", &default], &root);
        git(&["merge", "--no-ff", "-m", "operator: merge off-base", "rsi/off-base-work"], &root);
        // BEFORE pushing the merge, the default is ahead of origin -> pushed precondition UNMET.
        assert!(
            !base_is_pushed(&root_s, &default),
            "an un-pushed merge leaves the default ahead of origin -> not pushed"
        );
        git(&["push", "origin", &default], &root);
        // AFTER the real push, on-base + pushed is satisfied and the controller stop would clear.
        assert!(
            base_is_pushed(&root_s, &default),
            "after the real push the default is level with origin -> pushed"
        );
        assert_eq!(
            persistent_stop_cleared("controller_off_base_persistent", true, true, true, true),
            Some("controller tree reconciled (on-base + pushed + clean) — cleared controller_off_base_persistent stop"),
            "only a real reconcile (on-base + pushed + clean) clears the controller self-stop"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&remote);
    }

    // -------- sweep_repo: recover() is SKIPPED on the sweep that heals a persistent stop --------
    // The watchdog heals a persistent-bail self-stop (persistent_stop_cleared removes the sentinel
    // + should_restart relaunches the loop), but the heartbeat still carries the PRE-heal error
    // (status=error, phase=preflight, reason=<persistent>) until the relaunched loop writes a fresh
    // one. Previously the RUNG-0 recover() pass ran on that SAME sweep, re-observed the stale error
    // heartbeat, and either escalated it (unknown_error/stuck) or stop+restarted the just-restarted
    // lane — the exact "stale heartbeat state not re-observed after a fix lands" thrash. Now
    // recover() is skipped on the heal sweep; the next sweep re-observes the fresh heartbeat.
    //
    // Hermetic end-to-end check of sweep_repo: a real temp git repo (clean base -> base_is_clean)
    // + a runtime dir carrying a dirty_base_persistent stop sentinel + error heartbeat. phase is
    // set to "reverted" ONLY so should_restart returns false (the revert-failure HALT) and no
    // improver is spawned by the test — the heal block gates on status==error, not phase, so the
    // heal still fires. Without the skip, recover() would classify this as revert_failed and append
    // a "<name> recover: ..." action (and do a real reset_to_base); with the skip, only the heal
    // action appears.
    #[test]
    fn sweep_repo_skips_recover_after_persistent_stop_heal() {
        use std::process::Command;
        let tag = format!(
            "wd_heal_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        // temp git repo with a clean base tree.
        let git_dir = std::env::temp_dir().join(format!("solomon_{tag}"));
        let _ = std::fs::remove_dir_all(&git_dir);
        std::fs::create_dir_all(&git_dir).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
            vec!["branch", "-M", "main"],
        ] {
            let st = Command::new("git")
                .args(&args)
                .current_dir(&git_dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed in {:?}", args, git_dir);
        }
        let repo = json!({"name": tag, "path": git_dir.to_string_lossy()});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(rt.join("stop"), "dirty_base_persistent\n").unwrap();
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "reverted",
                "reason": "dirty_base_persistent",
                "last_summary": "Base branch 'main' has been dirty for 3 consecutive preflight bails",
            })).unwrap(),
        ).unwrap();

        let (actions, _snap) = sweep_repo(
            &repo,
            false,
            &std::cell::Cell::new(MAX_LANE_RESTARTS_PER_SWEEP),
        );

        // The heal fired and removed the sentinel.
        assert!(
            actions
                .iter()
                .any(|a| a.contains("auto-recover: base clean again")),
            "heal action missing: {actions:?}"
        );
        assert!(!rt.join("stop").exists(), "stop sentinel not cleared");
        // recover() was SKIPPED on this sweep: no recover action, no escalation.json written, no
        // supervisor.jsonl record from recover (without the skip, a "<name> recover: ..." line and
        // a supervisor.jsonl entry would appear from the revert_failed classification).
        assert!(
            !actions.iter().any(|a| a.contains(" recover: ")),
            "recover ran on the heal sweep (re-observed stale heartbeat): {actions:?}"
        );
        assert!(
            !rt.join("escalation.json").exists(),
            "recover wrote a spurious escalation"
        );
        assert!(
            !rt.join("supervisor.jsonl").exists(),
            "recover wrote a spurious supervisor record"
        );

        let _ = std::fs::remove_dir_all(&rt);
        let _ = std::fs::remove_dir_all(&git_dir);
    }

    // -------- restart budget: an exhausted budget DEFERS a crash-restart (disk-meltdown guard) ----
    // The 2026-07-02 meltdown: the watchdog restarted all crashed lanes in ONE sweep -> simultaneous
    // cargo builds saturated the disk and froze the live trader. With the per-sweep restart budget
    // spent (0), a restartable (crashed) lane must be DEFERRED — NOT started — so its heavy build
    // gate can't stack this sweep; the next sweep retries it. Crucially, deferral must NOT spawn the
    // improver subprocess, so this test is spawn-free (a real start would launch `run-improver`).
    #[test]
    fn sweep_repo_defers_restart_when_budget_exhausted() {
        use std::process::Command;
        let tag = format!(
            "wd_budget_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        let git_dir = std::env::temp_dir().join(format!("solomon_{tag}"));
        let _ = std::fs::remove_dir_all(&git_dir);
        std::fs::create_dir_all(&git_dir).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
            vec!["branch", "-M", "main"],
        ] {
            let st = Command::new("git")
                .args(&args)
                .current_dir(&git_dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed in {:?}", args, git_dir);
        }
        let repo = json!({"name": tag, "path": git_dir.to_string_lossy()});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        // A crashed-but-restartable lane: not running (no lock), status a live phase (not stopped,
        // not error+reverted), no paused/stop sentinel -> should_restart() == true.
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({"status": "iterating", "phase": "implement"})).unwrap(),
        )
        .unwrap();

        // Budget already spent this sweep.
        let budget = std::cell::Cell::new(0usize);
        let (actions, _snap) = sweep_repo(&repo, false, &budget);

        assert!(
            actions.iter().any(|a| a.contains("restart DEFERRED")),
            "expected a DEFERRED action when budget is 0: {actions:?}"
        );
        // MUST NOT have started the lane (no spawn) — no "restarted <name> (pid ...)" action.
        assert!(
            !actions
                .iter()
                .any(|a| a.contains(&format!("restarted {tag}"))),
            "lane was restarted despite an exhausted budget: {actions:?}"
        );
        assert_eq!(
            budget.get(),
            0,
            "a deferred restart must not change the budget"
        );

        let _ = std::fs::remove_dir_all(&rt);
        let _ = std::fs::remove_dir_all(&git_dir);
    }

    // -------- fleet-standstill decision (pure) --------
    // The alarm must fire on an entirely-down fleet OR a running-but-frozen fleet, and stay silent
    // on a healthy fleet or an empty registry. Threshold-in-seconds is passed so the test is fast.
    #[test]
    fn standstill_reason_fires_on_down_and_frozen_only() {
        let thresh = 3.0 * 3600.0;
        // fleet entirely down: at least one lane configured, none running.
        let r = standstill_reason(Some(60.0), 0, 4, thresh).unwrap();
        assert!(r.contains("entirely DOWN"), "{r}");
        assert!(r.contains("0/4"), "{r}");
        // running but frozen: newest iteration older than the threshold.
        let r = standstill_reason(Some(4.0 * 3600.0), 4, 4, thresh).unwrap();
        assert!(r.contains("no lane has iterated"), "{r}");
        assert!(r.contains("4.0 h"), "{r}");
        // running but NO history at all -> frozen (fresh install / wiped runtime).
        let r = standstill_reason(None, 2, 4, thresh).unwrap();
        assert!(r.contains("no lane has EVER iterated"), "{r}");
        // healthy: running and a fresh ship inside the window -> no alarm.
        assert!(standstill_reason(Some(600.0), 4, 4, thresh).is_none());
        // empty registry: nothing can stand still -> never an alarm.
        assert!(standstill_reason(None, 0, 0, thresh).is_none());
        assert!(standstill_reason(Some(9_999_999.0), 0, 0, thresh).is_none());
    }

    // -------- standstill_alarm dedupe marker: page once, re-arm on recovery (IO round-trip) --------
    // A standing standstill must page EXACTLY once (marker present -> no re-page), and recovery must
    // remove the marker so the next standstill re-pages. Kill-switch on so send() is a silent no-op
    // (no real ntfy/toast) and, post Part-1, does not touch the live _notify.jsonl either.
    #[test]
    fn standstill_alarm_pages_once_and_rearms_on_recovery() {
        let _env = crate::notify::NOTIFY_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SOLOMON_NOTIFY_OFF", "1");
        let marker = standstill_marker();
        let _ = std::fs::remove_file(&marker);
        let now = Utc::now();

        // First standstill sweep (fleet down): arms the marker. age is irrelevant when running==0.
        standstill_alarm(None, 0, 4, now);
        assert!(
            marker.exists(),
            "first standstill must arm the dedupe marker"
        );
        let armed_at = std::fs::read_to_string(&marker).unwrap_or_default();

        // Still down next sweep: marker unchanged (paged once — not re-written, not re-paged).
        standstill_alarm(None, 0, 4, now + chrono::Duration::seconds(120));
        assert!(marker.exists());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap_or_default(),
            armed_at,
            "a persisting standstill must not re-page (marker must not be rewritten)"
        );

        // Recovery (all lanes running AND a fresh iteration): marker cleared, alarm re-armed.
        standstill_alarm(Some(60.0), 4, 4, now);
        assert!(
            !marker.exists(),
            "recovery must clear the marker to re-arm the alarm"
        );

        let _ = std::fs::remove_file(&marker);
        std::env::remove_var("SOLOMON_NOTIFY_OFF");
    }

    #[test]
    fn watchdog_graft_runs_single_flight_and_resets() {
        static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        spawn_watchdog_graft(&RUNNING, move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (skipped_tx, skipped_rx) = std::sync::mpsc::channel();
        spawn_watchdog_graft(&RUNNING, move || {
            skipped_tx.send(()).unwrap();
        });
        assert!(skipped_rx.recv_timeout(Duration::from_millis(100)).is_err());

        release_tx.send(()).unwrap();
        for _ in 0..50 {
            if !RUNNING.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!RUNNING.load(std::sync::atomic::Ordering::SeqCst));
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
        let log =
            std::env::temp_dir().join(format!("solomon_wd_snaps_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let fresh = now();
        let old = (Utc::now() - chrono::Duration::seconds(99999))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        // Append: two fresh for our name, one old for our name, one fresh for another repo, one junk.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writeln!(
            f,
            "{}",
            json!({"ts": old, "repo": name, "status": "error", "phase": "preflight"})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})
        )
        .unwrap();
        writeln!(
            f,
            "{}",
            json!({"ts": fresh, "repo": "someone_else", "status": "x"})
        )
        .unwrap();
        writeln!(f, "not json").unwrap();
        drop(f);

        // No age bound: all 3 of our records, last 2.
        assert_eq!(recent_snapshots_from(&log, name, 2, None).len(), 2);

        // Age bound 600s: the old record is dropped; only the 2 fresh remain; take last 2.
        let got = recent_snapshots_from(&log, name, 2, Some(600.0));
        assert_eq!(got.len(), 2);
        assert!(got
            .iter()
            .all(|r| r.get("repo").and_then(Value::as_str) == Some(name)));

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
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (2 failed).",
            ),
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (1 failed).",
            ),
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (3 failed).",
            ),
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
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (1 failed).",
            ),
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
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (1 failed).",
            ),
            hist_rec(
                "reverted",
                Some(true),
                "Reverted — leak guard: secret detected.",
            ),
            hist_rec(
                "reverted",
                Some(false),
                "Reverted — tests failed (2 failed).",
            ),
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
            assert_eq!(
                lane_health(true, &h, 3),
                None,
                "status={bad} should be healthy"
            );
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

    #[test]
    fn ops_needs_attention_on_red_or_yellow() {
        assert!(ops_needs_attention(
            &json!({"projects": {"sover": {"status": "red"}}})
        ));
        assert!(ops_needs_attention(
            &json!({"projects": {"dotz": {"status": "yellow"}}})
        ));
        assert!(!ops_needs_attention(
            &json!({"projects": {"maki": {"status": "green"}}})
        ));
        assert!(!ops_needs_attention(&json!({})));
    }

    // -------- stall detector anti-thrash: pre-recover escalation snapshot --------
    // The stall detector's anti-thrash checks the escalation category BEFORE recover() overwrites
    // it. Without the pre-recover snapshot, recover() runs first and overwrites escalation.json
    // with its own diagnosis (e.g. "unknown_error"), so the anti-thrash check would never see a
    // prior "running_stalled" and would re-emit the STALLED action every sweep.
    //
    // This test exercises the full sweep_repo path: a live lock + fresh error/preflight heartbeat
    // + 2 prior error/preflight snapshots in _monitor.jsonl (so the stall window is met) + a
    // pre-existing "running_stalled" escalation.json. The stall detector must NOT re-emit the
    // STALLED action (the anti-thrash suppresses it via the pre-recover snapshot).
    #[test]
    fn stall_detector_anti_thrash_uses_pre_recover_escalation() {
        use std::process::Command;
        let tag = format!(
            "wd_stall_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        );
        // temp git repo with a clean base tree (so diagnose doesn't classify it as dirty/etc).
        let git_dir = std::env::temp_dir().join(format!("solomon_{tag}"));
        let _ = std::fs::remove_dir_all(&git_dir);
        std::fs::create_dir_all(&git_dir).unwrap();
        for args in [
            vec!["init"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
            vec!["branch", "-M", "main"],
        ] {
            let st = Command::new("git")
                .args(&args)
                .current_dir(&git_dir)
                .status()
                .unwrap();
            assert!(st.success(), "git {:?} failed in {:?}", args, git_dir);
        }
        let repo = json!({"name": tag, "path": git_dir.to_string_lossy()});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();

        // Live lock (this process's PID) + fresh error/preflight heartbeat with a generic summary
        // that falls through to unknown_error in diagnose().
        let run_id = format!("tok-{tag}");
        std::fs::write(
            rt.join("lock"),
            format!("{}\n{}", std::process::id(), run_id),
        )
        .unwrap();
        let fresh = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "preflight",
                "run_id": run_id,
                "updated_at": fresh,
                "last_summary": "an unclassified preflight error occurred",
            }))
            .unwrap(),
        )
        .unwrap();

        // Pre-existing "running_stalled" escalation — the anti-thrash must see this BEFORE recover()
        // overwrites it, and suppress the re-escalation.
        std::fs::write(
            rt.join("escalation.json"),
            serde_json::to_string_pretty(&json!({
                "ts": fresh,
                "category": "running_stalled",
                "evidence": "running but stuck in preflight for 3 consecutive sweeps",
                "suggested_manual_steps": ["cd \"<repo>\"", "git status"],
            }))
            .unwrap(),
        )
        .unwrap();

        // 2 prior error/preflight snapshots in _monitor.jsonl (for STALL_SWEEPS=3, we need
        // STALL_SWEEPS-1=2 prior + the current snap from this sweep). Fresh timestamps so the
        // max_age_s recency bound doesn't drop them.
        let mon = mon_log();
        if let Some(parent) = mon.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ts = fresh.clone();
        let snap_line = |repo: &str| {
            serde_json::to_string(&json!({
                "ts": ts,
                "repo": repo,
                "running": true,
                "restarted": false,
                "paused": false,
                "status": "error",
                "phase": "preflight",
                "iteration": Value::Null,
                "last_status": Value::Null,
                "diagnosis": "unknown_error",
            }))
            .unwrap()
        };
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&mon)
                .unwrap();
            writeln!(f, "{}", snap_line(&tag)).unwrap();
            writeln!(f, "{}", snap_line(&tag)).unwrap();
        }

        let (actions, _snap) = sweep_repo(
            &repo,
            false,
            &std::cell::Cell::new(MAX_LANE_RESTARTS_PER_SWEEP),
        );

        // The stall detector must NOT re-emit the STALLED action — the pre-existing
        // "running_stalled" escalation (captured before recover() overwrote it) suppresses it.
        assert!(
            !actions.iter().any(|a| a.contains(" STALLED:")),
            "stall detector re-escalated despite a pre-existing running_stalled escalation: {actions:?}"
        );

        // Cleanup: remove this test's entries from _monitor.jsonl (filter out lines whose repo
        // matches our unique tag), and tear down the runtime + git dirs.
        if let Ok(content) = std::fs::read_to_string(&mon) {
            let kept: Vec<&str> = content
                .lines()
                .filter(|line| !line.contains(&format!("\"repo\":\"{}\"", tag)))
                .collect();
            let _ = std::fs::write(&mon, format!("{}\n", kept.join("\n")));
        }
        let _ = std::fs::remove_dir_all(&rt);
        let _ = std::fs::remove_dir_all(&git_dir);
    }

    // ================= AUTOPILOT PERMANENT-STUCK HEAL (autopilot_recover_pass) =================
    // The fleet's autopilot path routes a `noop_streak`→`auto_safe=false` lane to an inert
    // `proof_required` job and `sweep()`'s early `return out` bypasses the crash-restart + recover()
    // machinery — a permanent absorbing state (the 2026-07-07 audit). autopilot_recover_pass_inner
    // reaches the existing recover() heal, gated by a stuck-sweep time-decay counter and a
    // `noop_streak`-only diagnose filter. These tests drive the gate deterministically via injected
    // stuck-sweeps/reset closures (no global autopilot state touched).

    // Seed a runtime dir for a named lane with 5 consecutive noops + a sleeping heartbeat and NO
    // lock → diagnose() returns noop_streak (5-noop history; no lock so it doesn't divert to
    // stale_lock; not running so not in-flight) and the lane is heal-eligible. No lock is used so
    // runner::stop sees a not-running lane and confirms the stop immediately, letting the heal
    // proceed through ideate→restart deterministically in-test (a LIVE-lock fixture cannot stop the
    // test's own process, which is why the supervisor sleeping-lane test only asserts on `stop`).
    fn seed_noop_streak_lane(tag: &str) -> (std::path::PathBuf, Value) {
        let name = format!("wd_ar_{tag}_{}", std::process::id());
        let repo = json!({ "name": name });
        let dir = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (0..5)
            .map(|_| json!({"status": "noop"}).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("history.jsonl"), body).unwrap();
        let fresh = (Utc::now() - chrono::Duration::seconds(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(
            dir.join("heartbeat.json"),
            json!({"status": "sleeping", "phase": "sleep", "run_id": "tok-ar", "updated_at": fresh})
                .to_string(),
        )
        .unwrap();
        (dir, repo)
    }

    // (1a) A lane stuck N>=THRESHOLD sweeps as proof_required AND diagnosing noop_streak gets a REAL
    // recover cycle (a stop→ideate→restart heal, evidenced by the "recover:" action carrying "stop")
    // and its stuck counter is reset — proving the permanent trap is now escapable on autopilot.
    #[test]
    fn autopilot_recover_heals_stuck_noop_lane_at_threshold() {
        let (dir, repo) = seed_noop_streak_lane("heal");
        let name = repo["name"].as_str().unwrap().to_string();
        assert_eq!(supervisor::diagnose(&repo)["category"], "noop_streak");
        let repos = vec![repo.clone()];
        let targets = vec![name.clone()];
        let reset_hits = std::cell::Cell::new(0u32);
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,                       // auto_push_flag → allow_restart true
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| crate::fleet::STUCK_SWEEP_THRESHOLD, // stuck exactly at the threshold
            &|_n| {
                reset_hits.set(reset_hits.get() + 1);
                Ok(())
            },
        );
        assert!(
            actions.iter().any(|a| a.starts_with(&format!("{name} recover:")) && a.contains("stop")),
            "expected a real recover heal (stop,...) for the stuck noop lane: {actions:?}"
        );
        assert_eq!(
            reset_hits.get(),
            1,
            "a successful heal must reset the lane's stuck counter exactly once"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (1b) Anti-thrash gate: the SAME stuck noop lane below the threshold (stuck_sweeps=1) does NOT
    // fire a heal — no recover action, no reset, no stop sentinel. The time-decay the trap lacked.
    #[test]
    fn autopilot_recover_does_not_fire_below_threshold() {
        let (dir, repo) = seed_noop_streak_lane("below");
        let name = repo["name"].as_str().unwrap().to_string();
        assert_eq!(supervisor::diagnose(&repo)["category"], "noop_streak");
        let repos = vec![repo.clone()];
        let targets = vec![name.clone()];
        let reset_hits = std::cell::Cell::new(0u32);
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| 1, // only 1 sweep stuck — below THRESHOLD (3)
            &|_n| {
                reset_hits.set(reset_hits.get() + 1);
                Ok(())
            },
        );
        assert!(
            actions.is_empty(),
            "below-threshold lane must not be force-healed: {actions:?}"
        );
        assert_eq!(reset_hits.get(), 0, "no heal → no counter reset");
        assert!(
            !dir.join("stop").exists(),
            "below-threshold lane must not be stopped (no force-cycle every sweep)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (1c) BLOCKER regression (skeptic NO-GO fix): a lane that IS stuck at threshold AND diagnoses
    // noop_streak — i.e. identical to (1a), which DOES heal — is NOT healed when the operator has
    // explicitly paused it (`runtime/<name>/paused` present). recover() does not honor the pause
    // sentinel itself, so the pass must skip a paused lane BEFORE diagnose/recover. Asserts no
    // recover action, no counter reset, and (critically) no `stop` sentinel written — proving a
    // paused lane is never force-stopped→ideated→restarted (a safety-gate weakening otherwise).
    #[test]
    fn autopilot_recover_skips_paused_lane() {
        let (dir, repo) = seed_noop_streak_lane("paused");
        let name = repo["name"].as_str().unwrap().to_string();
        // Same eligible fixture as the (1a) heal case: threshold-stuck + noop_streak.
        assert_eq!(supervisor::diagnose(&repo)["category"], "noop_streak");
        // Operator hold: drop the pause sentinel the crash-restart path also honors.
        std::fs::write(dir.join("paused"), "").unwrap();
        let repos = vec![repo.clone()];
        let targets = vec![name.clone()];
        let reset_hits = std::cell::Cell::new(0u32);
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| crate::fleet::STUCK_SWEEP_THRESHOLD, // stuck long enough to heal if not paused
            &|_n| {
                reset_hits.set(reset_hits.get() + 1);
                Ok(())
            },
        );
        assert!(
            actions.is_empty(),
            "a paused lane must NOT be force-healed even when otherwise heal-eligible: {actions:?}"
        );
        assert_eq!(reset_hits.get(), 0, "no heal on a paused lane → no counter reset");
        assert!(
            !dir.join("stop").exists(),
            "a paused lane must never be force-stopped by the recover pass"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (2) Backoff: a lane at threshold with 3 prior noop_streak+ideate heals in supervisor.jsonl
    // ESCALATES (pages) instead of a 4th force-heal (recover()'s prior_heals<3). No infinite cycle.
    #[test]
    fn autopilot_recover_backs_off_after_three_prior_heals() {
        let (dir, repo) = seed_noop_streak_lane("backoff");
        let name = repo["name"].as_str().unwrap().to_string();
        // 3 prior RUNG-0.5 noop_streak heals that took an "ideate" action → prior_heals==3.
        let sup: String = (0..3)
            .map(|_| {
                json!({"category":"noop_streak","rung":0,"escalate":false,"actions":["stop","ideate","restart"]})
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("supervisor.jsonl"), sup).unwrap();
        assert_eq!(supervisor::diagnose(&repo)["category"], "noop_streak");
        let repos = vec![repo.clone()];
        let targets = vec![name.clone()];
        let reset_hits = std::cell::Cell::new(0u32);
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| {
                reset_hits.set(reset_hits.get() + 1);
                Ok(())
            },
        );
        assert!(
            actions.iter().any(|a| a.starts_with(&format!("{name} ESCALATED:"))),
            "after 3 prior heals the pass must escalate (page), not force a 4th heal: {actions:?}"
        );
        assert!(
            !actions.iter().any(|a| a.contains("recover:") && a.contains("restart")),
            "no 4th restart heal may fire once backed off: {actions:?}"
        );
        assert_eq!(reset_hits.get(), 0, "an escalation (no heal) must not reset the counter");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (3) Freshness discipline: a lane stuck at threshold but diagnosing a NON-noop_streak,
    // evidence-gated category (needs_goal) is NOT force-cycled — it keeps parking. Evidence-starved
    // repos are never force-restarted by this pass.
    #[test]
    fn autopilot_recover_skips_non_noop_streak_category() {
        let name = format!("wd_ar_needsgoal_{}", std::process::id());
        let repo = json!({ "name": name });
        let dir = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // needs_goal: an error heartbeat with reason=needs_goal (see diagnose_needs_goal_precedence).
        std::fs::write(
            dir.join("heartbeat.json"),
            json!({"status": "error", "reason": "needs_goal", "last_summary": ""}).to_string(),
        )
        .unwrap();
        assert_eq!(supervisor::diagnose(&repo)["category"], "needs_goal");
        let repos = vec![repo.clone()];
        let targets = vec![name.clone()];
        let reset_hits = std::cell::Cell::new(0u32);
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| crate::fleet::STUCK_SWEEP_THRESHOLD, // stuck long enough, but wrong category
            &|_n| {
                reset_hits.set(reset_hits.get() + 1);
                Ok(())
            },
        );
        assert!(
            actions.is_empty(),
            "an evidence-gated needs_goal lane must NOT be force-cycled: {actions:?}"
        );
        assert_eq!(reset_hits.get(), 0);
        assert!(!dir.join("stop").exists(), "needs_goal lane must not be stopped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // (4) Budget: THREE stuck noop lanes in one pass with MAX_LANE_RESTARTS_PER_SWEEP=2 → at most 2
    // heavy stop→ideate→restart heals actually run; the 3rd is deferred (restart_deferred), proving
    // the shared per-sweep restart budget caps the disk-meltdown blast radius. (MAX==2 asserted so
    // the test tracks the constant.)
    #[test]
    fn autopilot_recover_respects_shared_restart_budget() {
        assert_eq!(MAX_LANE_RESTARTS_PER_SWEEP, 2, "test assumes a budget of 2");
        let mut dirs = Vec::new();
        let mut repos = Vec::new();
        let mut targets = Vec::new();
        for i in 0..3 {
            let (dir, repo) = seed_noop_streak_lane(&format!("budget{i}"));
            targets.push(repo["name"].as_str().unwrap().to_string());
            repos.push(repo);
            dirs.push(dir);
        }
        let actions = autopilot_recover_pass_inner(
            &repos,
            &targets,
            true,
            crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| crate::fleet::STUCK_SWEEP_THRESHOLD,
            &|_n| Ok(()),
        );
        // Exactly one lane is deferred once the 2-slot budget is spent (restart_deferred surfaces in
        // the recover action string). recover() reserves the slot BEFORE stopping, so a deferred
        // lane is never left stopped-but-not-restarted.
        let deferred = actions
            .iter()
            .filter(|a| a.contains("restart_deferred"))
            .count();
        let real_heals = actions
            .iter()
            .filter(|a| a.contains("recover:") && a.contains("restart") && !a.contains("restart_deferred"))
            .count();
        assert!(
            real_heals <= MAX_LANE_RESTARTS_PER_SWEEP,
            "no more than the budget of heavy restarts may run: {actions:?}"
        );
        assert!(
            deferred >= 1,
            "with 3 stuck lanes and a budget of 2, at least one lane must defer: {actions:?}"
        );
        for dir in dirs {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
