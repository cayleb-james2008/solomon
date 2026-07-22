//! Metric-freshness ledger + no-new-data short-circuit + per-cycle budgets (RSI v3, requirement 4;
//! failure catalog #1 "value-blind objective").
//!
//! The core countermeasure: *no promote/rollback/mutate decision — and no token spend — unless the
//! tier-1 objective metric gained new samples since the last decision.* [`short_circuit`] is called
//! at the very top of `iteration::one_iteration`, BEFORE any git preflight, gate run, ideate, or pi
//! call, and a skipped cycle does NOT increment the iteration counter.
//!
//! Everything here is configured per repo via repos.json (read fresh each cycle). Non-live repos
//! with no `freshness` / `cycle_budget` keys keep byte-identical legacy behavior. A `live_app` is
//! fail-closed: omitting its objective probe is itself UNOBSERVABLE and halts before token spend or
//! mutation, so `no_objective` cannot mask a broken live data pipeline.
//!
//! CONFIG (repos.json per-repo row, all keys optional):
//!   "freshness":    {"cmd": "<shell cmd>", "min_new_samples": 1, "hard": true, "max_starve_cycles": 16}
//!   "cycle_budget": {"wall_s": 5400, "pi_calls": 6}
//!
//! FRESHNESS CMD CONTRACT: run with cwd=repo, the scrubbed improver env, and a 120s timeout; the
//! LAST non-empty stdout line must be JSON
//!   {"metric_id": str, "latest_ts": float, "n_samples": int, "observable": bool}
//! Nonzero exit / timeout / unparseable output all count as observable=false — and an unobservable
//! tier-1 metric is RED (halts meta-optimization), NEVER yellow. That asymmetry is the whole point:
//! every autopsied project kept optimizing a signal it could not see (asmodeus scored 0-row trades,
//! sover bred 45 unevaluable genomes, dotz graded empty test suites green).
//!
//! LEDGER: runtime/<name>/freshness.json — {"metric_id", "last_seen_ts", "last_n_samples",
//! "starve_count", "updated_at"} — written atomically (tmp+rename) so the dashboard/supervisor can
//! read it whole at any time.
//!
//! CYCLE BUDGET: runtime/<name>/cycle_budget.json — reset on every cycle that proceeds; pi.rs (WS2)
//! calls [`note_pi_call`] per agent invocation and [`budget_exceeded`] before starting another one.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::control::proc;
use crate::improver::ctx::{self, Ctx};
use crate::improver::gitops;

/// Wall-clock bound on the operator's freshness probe. The probe is supposed to be a cheap local
/// read (a sqlite count, a file mtime); 120s is generous headroom, and a hung probe must surface as
/// UNOBSERVABLE rather than wedge the loop.
const FRESHNESS_CMD_TIMEOUT_S: u64 = 120;

// --------------------------------------------------------------------------- #
// config (repos.json per-repo row, read fresh each cycle)
// --------------------------------------------------------------------------- #

/// Parsed `"freshness"` config. Present (Some) only when a non-empty `cmd` is configured — every
/// other key has the documented default, so a minimal row is just `{"cmd": "..."}`.
#[derive(Debug, Clone, PartialEq)]
struct FreshnessCfg {
    cmd: String,
    min_new_samples: i64,
    hard: bool,
    max_starve_cycles: i64,
}

fn freshness_cfg(row: &Value) -> Option<FreshnessCfg> {
    let f = row.get("freshness")?.as_object()?;
    let cmd = f
        .get("cmd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if cmd.is_empty() {
        return None; // no probe => feature off (legacy behavior)
    }
    Some(FreshnessCfg {
        cmd,
        min_new_samples: f.get("min_new_samples").and_then(Value::as_i64).unwrap_or(1),
        hard: f.get("hard").and_then(Value::as_bool).unwrap_or(true),
        // Floor 1: max_starve_cycles<=0 would make the escape valve fire every cycle, which is the
        // same observable behavior as hard=false — never a divide-into-nonsense state.
        max_starve_cycles: f
            .get("max_starve_cycles")
            .and_then(Value::as_i64)
            .unwrap_or(16)
            .max(1),
    })
}

/// Parsed `"cycle_budget"` config. Some only when at least one positive cap is set.
#[derive(Debug, Clone, PartialEq)]
struct BudgetCfg {
    wall_s: Option<f64>,
    max_pi_calls: Option<i64>,
}

fn budget_cfg(row: &Value) -> Option<BudgetCfg> {
    let b = row.get("cycle_budget")?.as_object()?;
    let wall_s = b.get("wall_s").and_then(Value::as_f64).filter(|w| *w > 0.0);
    let max_pi_calls = b.get("pi_calls").and_then(Value::as_i64).filter(|c| *c > 0);
    if wall_s.is_none() && max_pi_calls.is_none() {
        return None;
    }
    Some(BudgetCfg { wall_s, max_pi_calls })
}

// --------------------------------------------------------------------------- #
// the freshness report (probe stdout) + the ledger (runtime/<name>/freshness.json)
// --------------------------------------------------------------------------- #

/// One probe result: the LAST non-empty stdout line of the freshness cmd, parsed.
#[derive(Debug, Clone, PartialEq)]
struct Report {
    metric_id: String,
    latest_ts: f64,
    n_samples: i64,
    observable: bool,
}

/// Parse the probe's stdout per the cmd contract: the last NON-empty line must be the JSON report.
/// Noise lines before it (pip warnings, progress chatter) are ignored; anything else is an
/// Err(detail) the caller maps to UNOBSERVABLE.
fn parse_last_line_report(stdout: &str) -> Result<Report, String> {
    let line = stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .ok_or_else(|| "freshness cmd produced no stdout".to_string())?;
    let v: Value = serde_json::from_str(line)
        .map_err(|_| format!("last stdout line is not JSON: {}", head_chars(line, 160)))?;
    let field_err = |k: &str| format!("freshness report missing/mistyped field '{k}'");
    Ok(Report {
        metric_id: v
            .get("metric_id")
            .and_then(Value::as_str)
            .ok_or_else(|| field_err("metric_id"))?
            .to_string(),
        latest_ts: v
            .get("latest_ts")
            .and_then(Value::as_f64)
            .ok_or_else(|| field_err("latest_ts"))?,
        n_samples: v
            .get("n_samples")
            .and_then(Value::as_i64)
            .ok_or_else(|| field_err("n_samples"))?,
        observable: v
            .get("observable")
            .and_then(Value::as_bool)
            .ok_or_else(|| field_err("observable"))?,
    })
}

/// The persisted decision baseline: what the objective looked like the last time this loop was
/// ALLOWED to make a meta-decision.
#[derive(Debug, Clone, PartialEq)]
struct Ledger {
    metric_id: String,
    last_seen_ts: f64,
    last_n_samples: i64,
    starve_count: i64,
}

fn ledger_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime.join("freshness.json")
}

fn read_ledger(ctx: &Ctx) -> Option<Ledger> {
    let text = std::fs::read_to_string(ledger_path(ctx)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(Ledger {
        metric_id: v
            .get("metric_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        last_seen_ts: v.get("last_seen_ts").and_then(Value::as_f64).unwrap_or(0.0),
        last_n_samples: v.get("last_n_samples").and_then(Value::as_i64).unwrap_or(0),
        starve_count: v.get("starve_count").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn write_ledger(ctx: &Ctx, l: &Ledger) {
    let body = json!({
        "metric_id": l.metric_id,
        "last_seen_ts": l.last_seen_ts,
        "last_n_samples": l.last_n_samples,
        "starve_count": l.starve_count,
        "updated_at": ctx::now(),
    });
    let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string());
    ctx.runtime_atomic_write(&ledger_path(ctx), &text);
}

/// The freshness predicate: the objective gained >= min_new_samples new samples OR its newest
/// datum's timestamp moved. Saturating so an absurd operator min can't wrap.
fn is_fresh(report: &Report, last_seen_ts: f64, last_n_samples: i64, min_new_samples: i64) -> bool {
    report.n_samples >= last_n_samples.saturating_add(min_new_samples)
        || report.latest_ts > last_seen_ts
}

// --------------------------------------------------------------------------- #
// running the probe (shell cmd, cwd=repo, scrubbed env, 120s bound)
// --------------------------------------------------------------------------- #

/// Build the platform shell invocation for the operator's probe script. Windows uses `raw_arg` so
/// cmd.exe receives the script BYTE-FOR-BYTE (Rust's arg quoting is not cmd.exe's parser — a quoted
/// JSON echo or a path-with-spaces would be re-split and spuriously fail, a false UNOBSERVABLE).
fn shell_command(script: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(script);
        c.creation_flags(proc::CREATE_NO_WINDOW);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(script);
        c
    }
}

/// Run the probe bounded. stdout/stderr are drained on reader threads BEFORE waiting so a chatty
/// probe can't fill the ~64KB OS pipe and deadlock into a false timeout (same fix as gates.rs /
/// proc::run). Timeout -> Err(TimedOut); spawn failure -> Err(other).
fn run_shell_timed(ctx: &Ctx, script: &str) -> std::io::Result<proc::RunOut> {
    use std::io::Read;
    use std::process::Stdio;
    use wait_timeout::ChildExt;

    let mut cmd = shell_command(script);
    cmd.current_dir(&ctx.repo);
    ctx.apply_clean_env(&mut cmd);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let join =
        |h: Option<std::thread::JoinHandle<String>>| h.and_then(|h| h.join().ok()).unwrap_or_default();
    match child.wait_timeout(Duration::from_secs(FRESHNESS_CMD_TIMEOUT_S))? {
        Some(status) => Ok(proc::RunOut {
            code: status.code().unwrap_or(-1),
            stdout: join(out_h),
            stderr: join(err_h),
        }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            // detach the readers (don't join): a surviving grandchild holding the write handle
            // could keep the pipe open and hang the join — return promptly.
            drop(out_h);
            drop(err_h);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("freshness cmd timed out after {FRESHNESS_CMD_TIMEOUT_S}s"),
            ))
        }
    }
}

/// Run + parse the probe. Err(detail) covers every UNOBSERVABLE flavor of the cmd contract:
/// nonzero exit, timeout, spawn failure, no stdout, non-JSON last line, missing fields.
fn run_freshness_probe(ctx: &Ctx, script: &str) -> Result<Report, String> {
    match run_shell_timed(ctx, script) {
        Ok(p) if p.code != 0 => {
            let tail: String = p.stderr.trim().chars().take(160).collect();
            Err(format!(
                "freshness cmd exited rc={}{}",
                p.code,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            ))
        }
        Ok(p) => parse_last_line_report(&p.stdout),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Err(format!("freshness cmd timed out after {FRESHNESS_CMD_TIMEOUT_S}s"))
        }
        Err(e) => Err(format!("freshness cmd failed to spawn: {e}")),
    }
}

// --------------------------------------------------------------------------- #
// crate-internal accessors for the outcome-critique layer (D3)
// --------------------------------------------------------------------------- #
//
// The outcome-critique module (`outcome_critique.rs`) samples the SAME tier-1 metric this file
// gates on, at ship time and again after the freshness window, to grade the SIGNED metric_delta of
// a shipped change. It re-uses this file's probe machinery (the bounded shell exec, the last-line
// JSON contract) rather than copying it — one probe implementation, one contract. These accessors
// are `pub(crate)` and additive; they change NO existing behavior of `short_circuit`.

/// The lane's configured freshness probe cmd, or None when the lane has no freshness config
/// (`no_objective` sentinel / absent key => the critique layer is inert for that lane, exactly as
/// the gate is). Read fresh from repos.json so a dashboard edit is honored, mirroring `short_circuit`.
pub(crate) fn probe_cmd(ctx: &Ctx) -> Option<String> {
    let row = gitops::repo_row(ctx, &ctx.name.clone());
    freshness_cfg(&row).map(|c| c.cmd)
}

/// Run the lane's freshness probe ONCE and return the raw last-line JSON object (the probe's full
/// contract payload — `metric_id`/`n_samples`/`observable`/`latest_ts` plus any lane-specific
/// signed-value fields like kairos's `settled_usd_per_window_24h`). Err(detail) on every
/// UNOBSERVABLE flavor (nonzero exit, timeout, spawn failure, no/garbage stdout). The critique
/// layer needs the WHOLE object (not just the four `Report` fields), so this returns `Value`.
pub(crate) fn probe_raw(ctx: &Ctx, script: &str) -> Result<Value, String> {
    match run_shell_timed(ctx, script) {
        Ok(p) if p.code != 0 => {
            let tail: String = p.stderr.trim().chars().take(160).collect();
            Err(format!(
                "freshness cmd exited rc={}{}",
                p.code,
                if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                }
            ))
        }
        Ok(p) => {
            let line = p
                .stdout
                .lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .ok_or_else(|| "freshness cmd produced no stdout".to_string())?;
            serde_json::from_str::<Value>(line)
                .map_err(|_| format!("last stdout line is not JSON: {}", head_chars(line, 160)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            Err(format!("freshness cmd timed out after {FRESHNESS_CMD_TIMEOUT_S}s"))
        }
        Err(e) => Err(format!("freshness cmd failed to spawn: {e}")),
    }
}

// --------------------------------------------------------------------------- #
// HOLD_META — the WS5 config-provenance tripwire hold
// --------------------------------------------------------------------------- #

/// Honor runtime/<name>/HOLD_META (written by the provenance tripwire when an unversioned mutation
/// of a watched config file is detected): while unexpired, meta-work is held. The hold carries a
/// TTL (requirement 7: escalations never dead-end at "wait for operator") — an expired or
/// unparseable-expiry HOLD_META is deleted and the loop resumes, so a malformed tripwire file can
/// never freeze a lane forever.
fn hold_meta_short_circuit(ctx: &mut Ctx) -> bool {
    let path = ctx.runtime.join("HOLD_META");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return false, // no hold
    };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("unversioned config mutation detected")
        .to_string();
    let expires_at = v.get("expires_at").and_then(Value::as_f64).unwrap_or(0.0);
    let now = unix_now();
    if now < expires_at {
        let summary = format!("meta-work held: {reason}");
        ctx.heartbeat(json!({
            "status": "idle",
            "phase": Value::Null,
            "reason": "config_drift_hold",
            "last_summary": summary,
        }));
        ctx.log(&format!(
            "SKIP iteration: HOLD_META active ({}s left) — {summary}",
            (expires_at - now) as i64
        ));
        return true;
    }
    let _ = std::fs::remove_file(&path);
    ctx.log("HOLD_META expired — removed; meta-work resumes");
    false
}

// --------------------------------------------------------------------------- #
// short_circuit — the per-cycle gate, called at the very top of one_iteration
// --------------------------------------------------------------------------- #

/// Returns true when this cycle must be SKIPPED (hold active, metric unobservable, or no new
/// objective data), having already written the matching heartbeat + log line. Returns false when
/// the cycle may proceed — and on EVERY proceeding path first resets the per-cycle budget so WS2's
/// pi wiring meters the cycle that is about to run.
pub fn short_circuit(ctx: &mut Ctx) -> bool {
    // (a) provenance-tripwire hold, TTL'd.
    if hold_meta_short_circuit(ctx) {
        return true;
    }

    // (b) no freshness config => feature off for legacy/code lanes. A live app cannot safely make
    // decisions against an absent objective: fail closed instead of allowing `no_objective` to
    // disguise a broken live telemetry pipeline.
    let name = ctx.name.clone();
    let row = gitops::repo_row(ctx, &name);
    let cfg = match freshness_cfg(&row) {
        Some(c) => c,
        None => {
            if row.get("live_app").and_then(Value::as_bool) == Some(true) {
                unobservable_halt(
                    ctx,
                    Some("live_app_objective"),
                    "live_app=true requires a non-empty freshness.cmd",
                );
                return true;
            }
            reset_cycle_budget(ctx);
            return false;
        }
    };

    // (c) probe the objective. Any failure flavor is TIER-1 RED — never yellow: deciding on a
    // metric nobody can observe is catalog failure #1, and it must halt loudly.
    let report = match run_freshness_probe(ctx, &cfg.cmd) {
        Ok(r) => r,
        Err(detail) => {
            let detail = ctx.redact(&detail);
            unobservable_halt(ctx, None, &detail);
            return true;
        }
    };
    if !report.observable {
        unobservable_halt(
            ctx,
            Some(&report.metric_id),
            "the freshness cmd reported observable=false",
        );
        return true;
    }

    // (d) fresh: new samples or a newer datum since the last decision -> baseline + proceed. A
    // missing ledger (first ever probe) or a CHANGED metric_id (the operator repointed the
    // objective) establishes a new baseline rather than starving on stale bookkeeping.
    let ledger = read_ledger(ctx);
    let fresh = match &ledger {
        None => true,
        Some(l) if l.metric_id != report.metric_id => true,
        Some(l) => is_fresh(&report, l.last_seen_ts, l.last_n_samples, cfg.min_new_samples),
    };
    if fresh {
        write_ledger(
            ctx,
            &Ledger {
                metric_id: report.metric_id.clone(),
                last_seen_ts: report.latest_ts,
                last_n_samples: report.n_samples,
                starve_count: 0,
            },
        );
        ctx.log(&format!(
            "freshness: objective '{}' has new data (n={}, latest_ts={}) — proceeding",
            report.metric_id, report.n_samples, report.latest_ts
        ));
        reset_cycle_budget(ctx);
        return false;
    }

    // (e) stale. The ledger is necessarily Some here (a missing ledger is `fresh` above).
    let l = ledger.expect("stale requires a ledger baseline");
    if !cfg.hard {
        // advisory-only mode: log the starvation but let the cycle run (some repos want the
        // freshness signal on the dashboard without gating on it).
        ctx.log(&format!(
            "freshness: no new data for '{}' (n={}) but hard=false — proceeding (advisory only)",
            report.metric_id, report.n_samples
        ));
        reset_cycle_budget(ctx);
        return false;
    }
    if l.starve_count + 1 >= cfg.max_starve_cycles {
        // Escape valve: after max_starve_cycles consecutive starved cycles, allow ONE non-metric
        // cycle so a starved lane can still do zero-risk upkeep (and so a broken data pipeline
        // eventually gets a cycle that could FIX the pipeline) instead of starving forever.
        ctx.log("starvation escape — allowing one non-metric cycle");
        write_ledger(
            ctx,
            &Ledger {
                starve_count: 0,
                ..l
            },
        );
        reset_cycle_budget(ctx);
        return false;
    }
    let k = l.starve_count + 1;
    write_ledger(
        ctx,
        &Ledger {
            starve_count: k,
            ..l
        },
    );
    let summary = format!(
        "objective '{}' gained 0 new samples since last decision (n={}) — cycle skipped, \
zero token spend (starve {k}/{max})",
        report.metric_id,
        report.n_samples,
        max = cfg.max_starve_cycles
    );
    ctx.heartbeat(json!({
        "status": "idle",
        "phase": Value::Null,
        "reason": "no_new_data",
        "last_summary": summary,
    }));
    ctx.log(&format!("SKIP iteration: {summary}"));
    true
}

/// TIER-1 RED: the objective metric is unobservable — write the error heartbeat. The supervisor's
/// diagnose() matches `reason == "metric_unobservable"` into its own category, which actions.json
/// maps to `page_operator_deduped` (a TTL'd page, never restart_lane — a lane restart cannot fix a
/// broken probe); the routing is asserted by supervisor's
/// `diagnose_metric_unobservable_routes_to_its_own_category_not_unknown_error`. `id` falls back to the ledger's
/// metric_id (the last identity we DID observe) so the page names the metric even when the probe
/// output was garbage.
fn unobservable_halt(ctx: &mut Ctx, id: Option<&str>, detail: &str) {
    let id = id
        .map(str::to_string)
        .or_else(|| read_ledger(ctx).map(|l| l.metric_id).filter(|m| !m.is_empty()))
        .unwrap_or_else(|| "unknown".to_string());
    let summary = format!(
        "tier-1 objective metric '{id}' is UNOBSERVABLE ({detail}) — meta-optimization halted; \
no promote/rollback/mutate decisions and no token spend on a blind objective"
    );
    ctx.heartbeat(json!({
        "status": "error",
        "phase": "preflight",
        "reason": "metric_unobservable",
        "last_summary": summary,
    }));
    ctx.log(&format!("TIER-1 RED: {summary}"));
}

// --------------------------------------------------------------------------- #
// per-cycle budgets — runtime/<name>/cycle_budget.json (consumed by WS2's pi.rs)
// --------------------------------------------------------------------------- #

fn budget_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime.join("cycle_budget.json")
}

/// Start a fresh budget window for the cycle that is about to run. Called by short_circuit on
/// every proceeding path. When the repo has no `cycle_budget` config a stale budget file is
/// REMOVED (a deleted config must turn the feature off, not leave a frozen cap in force) and
/// nothing is written — legacy repos never grow this file.
pub fn reset_cycle_budget(ctx: &Ctx) {
    let row = gitops::repo_row(ctx, &ctx.name);
    match budget_cfg(&row) {
        None => {
            let _ = std::fs::remove_file(budget_path(ctx));
        }
        Some(cfg) => {
            let body = json!({
                "started_at": unix_now(),
                "pi_calls": 0,
                "wall_s": cfg.wall_s,
                "max_pi_calls": cfg.max_pi_calls,
            });
            let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string());
            ctx.runtime_atomic_write(&budget_path(ctx), &text);
        }
    }
}

/// Count one pi/agent invocation against the current cycle's budget. No-op when no budget window
/// is in force (file absent => the repo has no cycle_budget config).
pub fn note_pi_call(ctx: &Ctx) {
    let path = budget_path(ctx);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return,
    };
    let mut v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return,
    };
    let n = v.get("pi_calls").and_then(Value::as_i64).unwrap_or(0);
    if let Some(o) = v.as_object_mut() {
        o.insert("pi_calls".to_string(), json!(n + 1));
    }
    let out = serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string());
    ctx.runtime_atomic_write(&path, &out);
}

/// Some(reason) when the current cycle has exhausted its wall clock or pi-call budget; None when
/// within budget OR when no budget is configured (file absent/torn — budgets fail OPEN: a broken
/// telemetry file must not starve a healthy lane).
pub fn budget_exceeded(ctx: &Ctx) -> Option<String> {
    let text = std::fs::read_to_string(budget_path(ctx)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let started = v.get("started_at").and_then(Value::as_f64)?;
    if let Some(wall) = v.get("wall_s").and_then(Value::as_f64) {
        let elapsed = unix_now() - started;
        if elapsed > wall {
            return Some(format!(
                "cycle wall budget exhausted ({elapsed:.0}s elapsed > {wall:.0}s cap)"
            ));
        }
    }
    if let Some(maxc) = v.get("max_pi_calls").and_then(Value::as_i64) {
        let calls = v.get("pi_calls").and_then(Value::as_i64).unwrap_or(0);
        if calls >= maxc {
            return Some(format!(
                "cycle pi-call budget exhausted ({calls} call(s) >= {maxc} cap)"
            ));
        }
    }
    None
}

// --------------------------------------------------------------------------- #
// small helpers
// --------------------------------------------------------------------------- #

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// First `n` chars (code points, not bytes — a mid-UTF-8 byte slice would panic).
fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    /// A per-test unique suffix (nanos + a process-global counter) so parallel tests never share a
    /// tmp dir. Mirrors the tmp-dir Ctx pattern of the ctx.rs/run.rs suites.
    fn uniq() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("{:x}_{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
    }

    /// Build an isolated Ctx: its own control dir (repos.json), runtime dir, and an EXISTING repo
    /// dir (the probe runs with cwd=repo; a missing cwd is a spawn error, not what's under test).
    fn test_ctx(repos_rows: Option<Value>) -> Ctx {
        let base = std::env::temp_dir().join(format!("solomon_freshness_test_{}", uniq()));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::create_dir_all(&repo);
        if let Some(rows) = repos_rows {
            std::fs::write(
                control.join("repos.json"),
                serde_json::to_string_pretty(&rows).unwrap(),
            )
            .unwrap();
        }
        let mut c = Ctx::configure(&repo.to_string_lossy(), "freshtest", "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join("freshtest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    /// A probe that always reports the SAME state (n=0, ts=1): fresh on the first-ever run (no
    /// ledger -> new baseline), stale on every run after.
    #[cfg(windows)]
    const CONST_PROBE: &str =
        r#"echo {"metric_id":"m","latest_ts":1.0,"n_samples":0,"observable":true}"#;
    #[cfg(not(windows))]
    const CONST_PROBE: &str =
        r#"printf '{"metric_id":"m","latest_ts":1.0,"n_samples":0,"observable":true}\n'"#;

    /// A probe that emits a NOISE line before the JSON report (the cmd contract: last non-empty
    /// stdout line wins).
    #[cfg(windows)]
    const NOISY_PROBE: &str = r#"echo warming up...&& echo {"metric_id":"m","latest_ts":2.0,"n_samples":7,"observable":true}"#;
    #[cfg(not(windows))]
    const NOISY_PROBE: &str = r#"printf '%s\n' 'warming up...' '{"metric_id":"m","latest_ts":2.0,"n_samples":7,"observable":true}'"#;

    fn rows_with_freshness(extra: Value) -> Value {
        let mut f = json!({"cmd": CONST_PROBE});
        if let (Some(dst), Some(src)) = (f.as_object_mut(), extra.as_object()) {
            for (k, v) in src {
                dst.insert(k.clone(), v.clone());
            }
        }
        json!([{ "name": "freshtest", "freshness": f }])
    }

    fn hb_str<'a>(c: &'a Ctx, key: &str) -> &'a str {
        c.hb.get(key).and_then(Value::as_str).unwrap_or("")
    }

    // ---- report parsing (the cmd contract) ----

    #[test]
    fn parse_report_takes_last_nonempty_line_ignoring_noise() {
        let out = "pip warning: blah\nprogress 50%\n\n{\"metric_id\":\"pnl_15m\",\"latest_ts\":1751.5,\"n_samples\":42,\"observable\":true}\n\n  \n";
        let r = parse_last_line_report(out).expect("must parse");
        assert_eq!(r.metric_id, "pnl_15m");
        assert_eq!(r.latest_ts, 1751.5);
        assert_eq!(r.n_samples, 42);
        assert!(r.observable);
    }

    #[test]
    fn parse_report_integer_latest_ts_accepted_as_float() {
        let out = r#"{"metric_id":"m","latest_ts":3,"n_samples":1,"observable":false}"#;
        let r = parse_last_line_report(out).expect("int ts coerces to f64");
        assert_eq!(r.latest_ts, 3.0);
        assert!(!r.observable);
    }

    #[test]
    fn parse_report_garbage_and_missing_fields_error() {
        assert!(parse_last_line_report("").is_err(), "no stdout");
        assert!(parse_last_line_report("   \n \n").is_err(), "blank stdout");
        assert!(parse_last_line_report("not json at all").is_err(), "non-JSON last line");
        // last line is JSON but a NOISE object -> missing fields is unparseable, not a guess
        assert!(
            parse_last_line_report(r#"{"metric_id":"m","latest_ts":1.0,"n_samples":2}"#).is_err(),
            "missing observable"
        );
        assert!(
            parse_last_line_report(r#"{"latest_ts":1.0,"n_samples":2,"observable":true}"#).is_err(),
            "missing metric_id"
        );
        // JSON before noise: the NOISE is the last line -> error (the contract is strict)
        let json_then_noise =
            "{\"metric_id\":\"m\",\"latest_ts\":1.0,\"n_samples\":2,\"observable\":true}\ndone.";
        assert!(parse_last_line_report(json_then_noise).is_err());
    }

    // ---- freshness arithmetic ----

    #[test]
    fn is_fresh_min_new_samples_arithmetic() {
        let rep = |n: i64, ts: f64| Report {
            metric_id: "m".into(),
            latest_ts: ts,
            n_samples: n,
            observable: true,
        };
        // n grew by exactly min -> fresh
        assert!(is_fresh(&rep(5, 1.0), 1.0, 3, 2));
        // n grew but less than min, ts unchanged -> stale
        assert!(!is_fresh(&rep(4, 1.0), 1.0, 3, 2));
        // n unchanged but a newer datum timestamp -> fresh
        assert!(is_fresh(&rep(3, 2.5), 1.0, 3, 2));
        // nothing moved -> stale
        assert!(!is_fresh(&rep(3, 1.0), 1.0, 3, 1));
        // saturating add: an absurd min can't wrap into "always fresh"
        assert!(!is_fresh(&rep(10, 1.0), 1.0, 3, i64::MAX));
    }

    // ---- ledger round-trip ----

    #[test]
    fn ledger_round_trip() {
        let c = test_ctx(None);
        assert!(read_ledger(&c).is_none(), "no ledger yet");
        let l = Ledger {
            metric_id: "pnl_15m".into(),
            last_seen_ts: 1751.25,
            last_n_samples: 42,
            starve_count: 3,
        };
        write_ledger(&c, &l);
        assert_eq!(read_ledger(&c), Some(l));
        // updated_at is persisted alongside (ISO Z stamp)
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(ledger_path(&c)).unwrap()).unwrap();
        assert_eq!(raw["updated_at"].as_str().unwrap().len(), 20);
    }

    // ---- absent config => byte-identical legacy behavior ----

    #[test]
    fn absent_config_is_a_no_op() {
        let mut c = test_ctx(Some(json!([{ "name": "freshtest" }])));
        assert!(!short_circuit(&mut c), "no freshness config -> proceed");
        assert!(!ledger_path(&c).exists(), "no ledger written");
        assert!(!budget_path(&c).exists(), "no budget file written");
        assert_eq!(hb_str(&c, "status"), "starting", "heartbeat untouched");
        // budget API is inert too
        note_pi_call(&c);
        assert!(!budget_path(&c).exists());
        assert_eq!(budget_exceeded(&c), None);
    }

    #[test]
    fn empty_cmd_is_feature_off() {
        let rows = json!([{ "name": "freshtest", "freshness": {"cmd": "   "} }]);
        let mut c = test_ctx(Some(rows));
        assert!(!short_circuit(&mut c));
        assert!(!ledger_path(&c).exists());
    }

    #[test]
    fn live_app_without_an_objective_probe_fails_closed() {
        let rows = json!([{
            "name": "freshtest",
            "live_app": true,
            "freshness": {"no_objective": true}
        }]);
        let mut c = test_ctx(Some(rows));
        assert!(short_circuit(&mut c), "a live app may not optimize a blind objective");
        assert_eq!(hb_str(&c, "status"), "error");
        assert_eq!(hb_str(&c, "reason"), "metric_unobservable");
        assert!(hb_str(&c, "last_summary").contains("requires a non-empty freshness.cmd"));
        assert!(!budget_path(&c).exists(), "no AI cycle budget may open on this halt");
    }

    // ---- the acceptance shape: first run proceeds, second skips with no_new_data ----

    #[test]
    fn first_run_proceeds_second_run_skips_no_new_data() {
        let mut c = test_ctx(Some(rows_with_freshness(json!({}))));
        // run 1: no ledger -> first observation is the baseline -> proceed
        assert!(!short_circuit(&mut c), "first probe establishes the baseline");
        let l1 = read_ledger(&c).expect("ledger written");
        assert_eq!((l1.last_n_samples, l1.starve_count), (0, 0));
        assert_eq!(l1.last_seen_ts, 1.0);
        // run 2: same n/ts -> stale -> hard skip
        assert!(short_circuit(&mut c), "no new data -> skip");
        assert_eq!(hb_str(&c, "status"), "idle");
        assert_eq!(hb_str(&c, "reason"), "no_new_data");
        assert!(
            hb_str(&c, "last_summary").contains("gained 0 new samples"),
            "got: {}",
            hb_str(&c, "last_summary")
        );
        assert!(hb_str(&c, "last_summary").contains("(starve 1/16)"));
        assert_eq!(read_ledger(&c).unwrap().starve_count, 1);
    }

    #[test]
    fn noisy_probe_with_new_data_proceeds_and_baselines() {
        // seed a stale baseline; the NOISY probe reports n=7/ts=2.0 -> fresh -> proceed + rebaseline
        let rows = json!([{ "name": "freshtest", "freshness": {"cmd": NOISY_PROBE} }]);
        let mut c = test_ctx(Some(rows));
        write_ledger(
            &c,
            &Ledger { metric_id: "m".into(), last_seen_ts: 1.0, last_n_samples: 3, starve_count: 5 },
        );
        assert!(!short_circuit(&mut c), "new samples -> proceed");
        let l = read_ledger(&c).unwrap();
        assert_eq!((l.last_n_samples, l.last_seen_ts, l.starve_count), (7, 2.0, 0));
    }

    #[test]
    fn changed_metric_id_rebaselines_instead_of_starving() {
        let mut c = test_ctx(Some(rows_with_freshness(json!({}))));
        write_ledger(
            &c,
            &Ledger { metric_id: "old_metric".into(), last_seen_ts: 9.0, last_n_samples: 99, starve_count: 4 },
        );
        assert!(!short_circuit(&mut c), "operator repointed the objective -> new baseline");
        assert_eq!(read_ledger(&c).unwrap().metric_id, "m");
    }

    // ---- unobservable => TIER-1 RED, never yellow ----

    #[test]
    fn nonzero_exit_is_unobservable_red() {
        let rows = json!([{ "name": "freshtest", "freshness": {"cmd": "exit 3"} }]);
        let mut c = test_ctx(Some(rows));
        // seed the ledger so the halt can NAME the metric even though the probe said nothing
        write_ledger(
            &c,
            &Ledger { metric_id: "pnl_15m".into(), last_seen_ts: 1.0, last_n_samples: 0, starve_count: 0 },
        );
        assert!(short_circuit(&mut c), "unobservable halts the cycle");
        assert_eq!(hb_str(&c, "status"), "error");
        assert_eq!(hb_str(&c, "phase"), "preflight");
        assert_eq!(hb_str(&c, "reason"), "metric_unobservable");
        let s = hb_str(&c, "last_summary");
        assert!(s.contains("'pnl_15m'") && s.contains("UNOBSERVABLE"), "got: {s}");
        assert!(s.contains("no promote/rollback/mutate"), "got: {s}");
    }

    #[test]
    fn garbage_stdout_is_unobservable_red() {
        // `echo` behaves the same under cmd /C and /bin/sh here
        let rows = json!([{ "name": "freshtest", "freshness": {"cmd": "echo definitely-not-json"} }]);
        let mut c = test_ctx(Some(rows));
        assert!(short_circuit(&mut c));
        assert_eq!(hb_str(&c, "reason"), "metric_unobservable");
        assert!(hb_str(&c, "last_summary").contains("'unknown'"), "no ledger -> id falls back");
    }

    #[test]
    fn observable_false_report_is_red() {
        #[cfg(windows)]
        let probe = r#"echo {"metric_id":"m","latest_ts":1.0,"n_samples":5,"observable":false}"#;
        #[cfg(not(windows))]
        let probe =
            r#"printf '{"metric_id":"m","latest_ts":1.0,"n_samples":5,"observable":false}\n'"#;
        let rows = json!([{ "name": "freshtest", "freshness": {"cmd": probe} }]);
        let mut c = test_ctx(Some(rows));
        assert!(short_circuit(&mut c));
        assert_eq!(hb_str(&c, "reason"), "metric_unobservable");
        assert!(hb_str(&c, "last_summary").contains("observable=false"));
    }

    // ---- starvation: hard=false, counting, and the escape valve ----

    #[test]
    fn soft_mode_stale_proceeds_without_skip() {
        let mut c = test_ctx(Some(rows_with_freshness(json!({"hard": false}))));
        write_ledger(
            &c,
            &Ledger { metric_id: "m".into(), last_seen_ts: 1.0, last_n_samples: 0, starve_count: 0 },
        );
        assert!(!short_circuit(&mut c), "hard=false -> advisory only");
        assert_eq!(hb_str(&c, "status"), "starting", "no skip heartbeat written");
        assert_eq!(read_ledger(&c).unwrap().starve_count, 0, "soft mode does not starve-count");
    }

    #[test]
    fn starve_escape_fires_at_exactly_max_starve_cycles() {
        let mut c = test_ctx(Some(rows_with_freshness(json!({"max_starve_cycles": 3}))));
        // starve_count=1: 1+1=2 < 3 -> still a skip
        write_ledger(
            &c,
            &Ledger { metric_id: "m".into(), last_seen_ts: 1.0, last_n_samples: 0, starve_count: 1 },
        );
        assert!(short_circuit(&mut c), "below the valve -> skip");
        assert_eq!(read_ledger(&c).unwrap().starve_count, 2);
        assert!(hb_str(&c, "last_summary").contains("(starve 2/3)"));
        // starve_count=2: 2+1 >= 3 -> the escape valve allows ONE non-metric cycle
        assert!(!short_circuit(&mut c), "at the valve -> one cycle allowed");
        assert_eq!(read_ledger(&c).unwrap().starve_count, 0, "valve resets the counter");
        let log = std::fs::read_to_string(&c.log_path).unwrap_or_default();
        assert!(
            log.contains("starvation escape — allowing one non-metric cycle"),
            "log: {log}"
        );
    }

    // ---- fleet-lane scenario: no-new-evidence lane PARKS while a fresh-evidence lane RUNS,
    //      and no lane is permanently starved (2026-07-07: the asmodeus+sover gating fix) ----

    /// A probe that emits the EXACT contract shape our real emitters produce
    /// (asmodeus tools/freshness.py "live_settled_fills", sover tools/freshness.py
    /// "published_reels"): {metric_id, latest_ts, n_samples, observable:true}. `n`/`ts` are
    /// baked into the script string so a lane can present "no new data" vs "a fresh sample".
    fn evidence_probe(metric: &str, n: i64, ts: f64) -> String {
        // Windows `echo` and POSIX single-quoted echo both pass the JSON through byte-for-byte
        // here (no shell metachars inside the object). Kept identical across platforms so the
        // scenario asserts the same on the CI box and the live Windows host.
        let obj = format!(
            r#"{{"metric_id":"{metric}","latest_ts":{ts},"n_samples":{n},"observable":true}}"#
        );
        #[cfg(windows)]
        {
            format!("echo {obj}")
        }
        #[cfg(not(windows))]
        {
            format!("printf '%s\\n' '{obj}'")
        }
    }

    fn lane_with_probe(probe: &str) -> Ctx {
        test_ctx(Some(
            json!([{ "name": "freshtest", "freshness": {"cmd": probe} }]),
        ))
    }

    /// The load-bearing property of the asmodeus+sover fix: given two fleet lanes probing a
    /// live objective, the lane whose objective gained NO new operator-written samples PARKS
    /// (skip, zero token spend), while the lane that DID gain a fresh sample proceeds — so AI
    /// cycles stop being wasted on an evidence-gated lane and flow to a lane with real new work.
    #[test]
    fn stale_lane_parks_while_fresh_lane_runs_and_no_lane_is_permanently_starved() {
        // --- Lane A ("sover-like"): a settled baseline, then the SAME probe result forever
        //     (no new publish) -> must PARK on every subsequent cycle. ---
        let stale_probe = evidence_probe("published_reels", 112, 1_000.0);
        let mut lane_a = lane_with_probe(&stale_probe);
        // cycle 1: no ledger -> first observation baselines -> proceed (the one allowed run)
        assert!(!short_circuit(&mut lane_a), "lane A cycle 1 baselines and runs");
        // cycles 2..=5: identical probe (no new sample) -> PARK every time, spending no tokens
        for cycle in 2..=5 {
            assert!(
                short_circuit(&mut lane_a),
                "lane A cycle {cycle}: no new evidence -> PARK (skip)"
            );
            assert_eq!(hb_str(&lane_a, "status"), "idle");
            assert_eq!(hb_str(&lane_a, "reason"), "no_new_data");
            assert!(
                hb_str(&lane_a, "last_summary").contains("zero token spend"),
                "park must be the zero-token-spend skip; got: {}",
                hb_str(&lane_a, "last_summary")
            );
        }
        // starvation is bounded and rising, never pinned/forgotten
        assert_eq!(read_ledger(&lane_a).unwrap().starve_count, 4);

        // --- Lane B ("asmodeus-like"): seed the SAME stale baseline, but this cycle a fresh
        //     settled fill has landed (n grew, ts advanced) -> the lane RUNS. ---
        let fresh_probe = evidence_probe("live_settled_fills", 8, 2_000.0);
        let mut lane_b = lane_with_probe(&fresh_probe);
        write_ledger(
            &lane_b,
            &Ledger {
                metric_id: "live_settled_fills".into(),
                last_seen_ts: 1_000.0,
                last_n_samples: 5,
                starve_count: 3,
            },
        );
        assert!(
            !short_circuit(&mut lane_b),
            "lane B: a fresh settled fill -> the cycle RUNS (not starved)"
        );
        let lb = read_ledger(&lane_b).unwrap();
        assert_eq!(
            (lb.last_n_samples, lb.last_seen_ts, lb.starve_count),
            (8, 2_000.0, 0),
            "running rebaselines the ledger and clears the starve counter"
        );

        // --- No lane is permanently starved: drive Lane A's stale probe up to the escape
        //     valve; at max_starve_cycles it MUST release ONE upkeep cycle and reset. ---
        // lane_a is currently at starve_count=4 with default max_starve_cycles=16. Skip until
        // one below the valve, then assert the valve releases exactly once.
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 100, "escape valve never fired — a lane is pinned forever");
            let before = read_ledger(&lane_a).unwrap().starve_count;
            let skipped = short_circuit(&mut lane_a);
            if !skipped {
                // the escape valve fired: one non-metric upkeep cycle was allowed and the
                // counter reset — the lane is NOT permanently starved.
                assert_eq!(
                    read_ledger(&lane_a).unwrap().starve_count,
                    0,
                    "escape valve resets the starve counter"
                );
                assert_eq!(
                    before + 1,
                    16,
                    "valve fires at exactly max_starve_cycles (default 16)"
                );
                break;
            }
        }
    }

    // ---- HOLD_META (WS5 provenance tripwire) ----

    #[test]
    fn hold_meta_active_skips_with_config_drift_hold() {
        // no freshness config needed: the hold check runs FIRST
        let mut c = test_ctx(Some(json!([{ "name": "freshtest" }])));
        let hold = json!({
            "reason": ".env mutated outside a gated commit",
            "file": ".env",
            "ts": unix_now(),
            "expires_at": unix_now() + 3600.0,
        });
        std::fs::write(c.runtime.join("HOLD_META"), hold.to_string()).unwrap();
        assert!(short_circuit(&mut c), "unexpired hold -> skip");
        assert_eq!(hb_str(&c, "status"), "idle");
        assert_eq!(hb_str(&c, "reason"), "config_drift_hold");
        assert!(
            hb_str(&c, "last_summary")
                .contains("meta-work held: .env mutated outside a gated commit")
        );
        assert!(c.runtime.join("HOLD_META").exists(), "an active hold is NOT deleted");
    }

    #[test]
    fn hold_meta_expired_is_deleted_and_flow_continues() {
        let mut c = test_ctx(Some(json!([{ "name": "freshtest" }])));
        let hold = json!({"reason": "r", "file": "f", "ts": 0.0, "expires_at": unix_now() - 5.0});
        std::fs::write(c.runtime.join("HOLD_META"), hold.to_string()).unwrap();
        assert!(!short_circuit(&mut c), "expired hold must not block");
        assert!(!c.runtime.join("HOLD_META").exists(), "expired hold deleted");
    }

    #[test]
    fn hold_meta_unparseable_is_treated_as_expired() {
        // a malformed tripwire file must never freeze a lane forever (TTL requirement)
        let mut c = test_ctx(Some(json!([{ "name": "freshtest" }])));
        std::fs::write(c.runtime.join("HOLD_META"), "{not json").unwrap();
        assert!(!short_circuit(&mut c));
        assert!(!c.runtime.join("HOLD_META").exists());
    }

    // ---- cycle budget ----

    fn rows_with_budget(wall_s: f64, pi_calls: i64) -> Value {
        json!([{ "name": "freshtest", "cycle_budget": {"wall_s": wall_s, "pi_calls": pi_calls} }])
    }

    #[test]
    fn budget_reset_note_and_exceeded_math() {
        let c = test_ctx(Some(rows_with_budget(3600.0, 2)));
        assert_eq!(budget_exceeded(&c), None, "no window yet -> None");
        reset_cycle_budget(&c);
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(budget_path(&c)).unwrap()).unwrap();
        assert_eq!(v["pi_calls"], json!(0));
        assert_eq!(v["wall_s"], json!(3600.0));
        assert_eq!(v["max_pi_calls"], json!(2));
        assert!(v["started_at"].as_f64().unwrap() > 0.0);
        assert_eq!(budget_exceeded(&c), None, "fresh window, 0 calls");
        note_pi_call(&c);
        assert_eq!(budget_exceeded(&c), None, "1 < 2");
        note_pi_call(&c);
        let why = budget_exceeded(&c).expect("2 >= 2 must exceed");
        assert!(why.contains("pi-call budget"), "got: {why}");
    }

    #[test]
    fn budget_wall_clock_exceeded() {
        let c = test_ctx(Some(rows_with_budget(10.0, 99)));
        reset_cycle_budget(&c);
        // rewind started_at instead of sleeping
        let mut v: Value =
            serde_json::from_str(&std::fs::read_to_string(budget_path(&c)).unwrap()).unwrap();
        v["started_at"] = json!(unix_now() - 60.0);
        std::fs::write(budget_path(&c), v.to_string()).unwrap();
        let why = budget_exceeded(&c).expect("60s elapsed > 10s cap");
        assert!(why.contains("wall budget"), "got: {why}");
    }

    #[test]
    fn budget_config_removed_turns_feature_off() {
        // window in force, then the operator deletes the cycle_budget key: the next reset must
        // REMOVE the stale file so a dead config can't keep tripping budget_exceeded.
        let c = test_ctx(Some(rows_with_budget(10.0, 1)));
        reset_cycle_budget(&c);
        note_pi_call(&c);
        assert!(budget_exceeded(&c).is_some());
        std::fs::write(c.control.join("repos.json"), r#"[{"name": "freshtest"}]"#).unwrap();
        reset_cycle_budget(&c);
        assert!(!budget_path(&c).exists(), "stale window removed");
        assert_eq!(budget_exceeded(&c), None);
    }

    #[test]
    fn budget_cfg_requires_a_positive_cap() {
        assert_eq!(budget_cfg(&json!({"cycle_budget": {}})), None);
        assert_eq!(budget_cfg(&json!({"cycle_budget": {"wall_s": 0, "pi_calls": 0}})), None);
        assert_eq!(
            budget_cfg(&json!({"cycle_budget": {"pi_calls": 6}})),
            Some(BudgetCfg { wall_s: None, max_pi_calls: Some(6) })
        );
    }

    // ---- config parsing defaults ----

    #[test]
    fn freshness_cfg_defaults() {
        let row = json!({"freshness": {"cmd": "probe.exe"}});
        let cfg = freshness_cfg(&row).unwrap();
        assert_eq!(cfg.min_new_samples, 1);
        assert!(cfg.hard);
        assert_eq!(cfg.max_starve_cycles, 16);
        assert_eq!(freshness_cfg(&json!({})), None, "no key -> off");
        assert_eq!(freshness_cfg(&json!({"freshness": {}})), None, "no cmd -> off");
        // a nonsense max_starve_cycles is floored to 1 (== escape every stale cycle), never <=0
        let row = json!({"freshness": {"cmd": "x", "max_starve_cycles": -5}});
        assert_eq!(freshness_cfg(&row).unwrap().max_starve_cycles, 1);
    }
}
