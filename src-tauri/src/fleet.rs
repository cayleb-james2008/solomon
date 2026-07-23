//! Single-agent Autopilot runtime.
//!
//! This replaces "one long-lived improver process per project" with one file-backed scheduler that owns
//! provider quota, job priority, active leases, and proof records. Managed repo mutation still flows
//! through the existing gated `run-improver --once` executor.

use crate::control::{heartbeat, locks, paths, proc, registry};
use crate::improver::ctx::Ctx;
use crate::improver::{pi, progress};
use crate::notify;
use crate::ops;
use crate::supervisor;
use chrono::{DateTime, Duration as ChronoDuration, NaiveDateTime, Utc};
use serde_json::{json, Map, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

const STATE_FILE: &str = "autopilot_state.json";
const EVENTS_FILE: &str = "autopilot_events.jsonl";
const LOCK_FILE: &str = "autopilot.lock";
const LOCK_FILE_1: &str = "autopilot.lock.1";
const PROOF_FILE: &str = "autopilot_proof.json";
const LEGACY_STATE_FILE: &str = "fleet_state.json";
const LEGACY_PROOF_FILE: &str = "fleet_proof.json";
const DEFAULT_RUN_TIMEOUT_S: u64 = 3_600;
/// A completed proof owns the lane's retry slot for this long while its diagnosis/ops fingerprint
/// is unchanged. A new RED/YELLOW fingerprint or manual wake bypasses the hold immediately.
const PROOF_FRESH_HOURS: i64 = 12;

/// Consecutive autopilot sweeps a lane must be `proof_required` before the watchdog's
/// autopilot recover pass is allowed to force a stop→ideate→restart heal on it. This is the
/// time-decay the permanent-stuck audit found missing: `finish_non_ai_job`'s proof_required arm
/// mutates nothing, so a lane diagnosed `noop_streak`→`auto_safe=false` re-diagnoses the same
/// frozen corpse every ~5-min sweep forever. The watchdog only heals after N such sweeps (not
/// every sweep — that would token-thrash), which combined with `recover()`'s own `prior_heals<3`
/// exponential backoff caps the force-cycle. 3 sweeps ≈ 15 min at the 5-min sentinel cadence.
pub const STUCK_SWEEP_THRESHOLD: u64 = 3;

/// Consecutive `proof_required` outcomes after which a lane's autonomous RE-SPEC engages instead of
/// dispatching another inert proof_required job. This kills retry theater: a lane diagnosed
/// unhealthy with `auto_safe != true` re-fires the same non-mutating `proof_required` job every
/// ~5-min sweep forever (the dotz 53:1 proof_required:implement ratio was this). After
/// PROOF_COOLDOWN_THRESHOLD consecutive no-ops the cooldown arms; `plan_jobs` then routes the lane to
/// a BOUNDED autonomous diversification (a gated AI iteration at a FRESH, DIFFERENT approach via the
/// MoA brain) instead of parking for a human — see DIVERSIFY_DAILY_CAP. The cooldown clears the
/// moment the lane produces a non-proof_required outcome (it moved), same as `bump_stuck_counter`.
pub const PROOF_COOLDOWN_THRESHOLD: u64 = 3;
/// The proof_required cooldown window: 4h. Long enough to break the inert retry loop; short enough
/// that a genuinely-stuck lane re-surfaces the same day. It now paces the AUTONOMOUS diversification
/// (re-arm on the next threshold crossing) rather than a human-spec park.
pub const PROOF_COOLDOWN_S: i64 = 14_400;

/// Per-lane-per-day cap on AUTONOMOUS diversification (re-spec) implement iterations. A lane that
/// would otherwise be parked for a human spec instead gets up to this many gated AI attempts at a
/// FRESH, DIFFERENT approach per day (the MoA brain's own anti-thrash forces a different goal, never
/// the identical failing change); past it the lane backs off to low-frequency autonomous retry —
/// never a human park, never infinite spend. Small by design; `daily_call_budget` is the fleet-wide
/// spend ceiling on top of this.
pub const DIVERSIFY_DAILY_CAP: u64 = 3;

/// Bounded self-spec CATEGORIES the autonomous re-spec rotates through (operator-directed
/// full-autonomy directive 2026-07-18). Each self-generated spec targets exactly ONE category so
/// consecutive attempts are MATERIALLY different work, not the same failing change reworded. Order
/// matters: it is the rotation order, and `reliability` leads because a parked lane's most likely
/// real problem is a defect, not a missing feature.
pub const SELF_SPEC_CATEGORIES: [&str; 4] = ["reliability", "tests", "docs_hygiene", "feature"];

/// Consecutive trailing self-spec failures IN THE SAME CATEGORY before the next self-spec is
/// FORCED onto a different category. 2 = one same-category retry (with the failed approach
/// explicitly excluded from the goal text) is allowed, then the spec family is abandoned — the
/// anti-gaming reverts of 2026-07-17 were the same spec family retried until the rail caught it.
pub const SELF_SPEC_FAMILY_FAIL_LIMIT: usize = 2;

/// Last-N self-spec attempts kept per lane in `autopilot_state.json` (`st["self_specs"][name]`).
/// The differentiation guard consults this window; older attempts age out so a category is not
/// banned forever by ancient failures.
pub const SELF_SPEC_HISTORY_CAP: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Job {
    name: String,
    kind: String,
    state: String,
    priority: i64,
    requires_ai: bool,
    reason: String,
    next_action: String,
    /// True iff this implement job is an autonomous diversification (re-spec) attempt. `once()`
    /// charges the lane's per-day diversify budget when THIS job actually dispatches (job_started)
    /// — plan_jobs only routes on the count and never spends (see charge_diversify_dispatch).
    diversify: bool,
    /// The self-generated FRESH spec for a diversify attempt: the bounded goal text `run_repo_once`
    /// dispatches INSTEAD of the standing repos.json goal (operator directive 2026-07-18: park
    /// expiry re-arms autonomous re-spec with NEW goal text). None = run the standing goal.
    spec_goal: Option<String>,
    /// The self-spec's category (a member of [`SELF_SPEC_CATEGORIES`]); recorded in the per-lane
    /// spec ledger at dispatch so the differentiation guard can rotate families. None unless
    /// `spec_goal` is Some.
    spec_category: Option<String>,
    /// True iff this implement job is the one-shot HUMAN WAKE override dispatch (the operator
    /// ack'd the lane; run their standing spec even though the diagnosis is unhealthy). `once()`
    /// consumes the wake flag when THIS job actually dispatches — same charge-at-dispatch rule as
    /// `diversify` (a plan that loses the slot must not eat the wake).
    wake_override: bool,
    /// Stable snapshot of the diagnosis category + ops probe statuses that caused this job. A
    /// proof only suppresses another attempt while this fingerprint is unchanged.
    state_fingerprint: String,
}

struct AutopilotLease {
    path: PathBuf,
}

impl Drop for AutopilotLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn state() -> Value {
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    let mut changed = false;
    if st.get("active").map(json_truthy).unwrap_or(false) && !autopilot_lock_live() {
        st["active"] = Value::Null;
        changed = true;
    }
    let repos = registry::load_repos();
    let ops_payload = read_ops_payload();
    let jobs = plan_jobs(&repos, &cfg, &ops_payload, &mut st, None, false);
    st["ok"] = Value::Bool(true);
    st["queue"] = Value::Array(jobs.iter().map(job_value).collect());
    st["proofs"] = proof_records();
    st["config"] = public_config(&cfg);
    if changed {
        st["ts"] = json!(now());
        let _ = write_state(&st);
    }
    st
}

pub fn stop_name(repo: &Value) -> Value {
    let name = paths::repo_name(repo);
    if name.is_empty() {
        return json!({"ok": false, "error": "unknown repo"});
    }
    let mut out = if locks::is_running(repo) {
        crate::control::runner::stop(repo)
    } else {
        json!({"ok": true, "already": true})
    };
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    let q: Vec<Value> = st
        .get("manual_queue")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|v| v.as_str() != Some(name.as_str()))
        .collect();
    st["manual_queue"] = Value::Array(q);
    st["ts"] = json!(now());
    let _ = write_state(&st);
    append_event(&json!({"event": "autopilot_dequeue", "repo": name}));
    if let Value::Object(ref mut o) = out {
        o.insert("dequeued".to_string(), Value::Bool(true));
        o.insert("autopilot".to_string(), Value::Bool(true));
    }
    out
}

pub fn once(auto_push: bool, only_name: Option<&str>) -> Value {
    let cfg = registry::autopilot_config();
    if cfg.get("mode").and_then(Value::as_str) != Some("single_agent") {
        return json!({"ok": false, "error": "autopilot mode is not single_agent"});
    }
    if max_concurrent(&cfg) > 2 {
        return json!({"ok": false, "error": "single_agent requires max_concurrent_agent_calls <= 2"});
    }
    let _lease = match acquire_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            return json!({"ok": true, "leased": false, "summary": "Solomon Autopilot already active"})
        }
        Err(e) => return json!({"ok": false, "error": e}),
    };

    let repos = registry::load_repos();
    let ops_payload = read_ops_payload();
    let mut st = read_state(&cfg);
    if autopilot_paused(&st) {
        st["active"] = Value::Null;
        st["ts"] = json!(now());
        st["config"] = public_config(&cfg);
        let jobs = plan_jobs(&repos, &cfg, &ops_payload, &mut st, only_name, false);
        st["queue"] = Value::Array(jobs.iter().map(job_value).collect());
        let _ = write_state(&st);
        append_event(&json!({"event": "autopilot_paused"}));
        return json!({
            "ok": true,
            "paused": true,
            "actions": ["autopilot paused; no new AI work started"],
            "state": st,
        });
    }
    let jobs = plan_jobs(&repos, &cfg, &ops_payload, &mut st, only_name, true);
    let mut actions = Vec::<String>::new();
    st["ts"] = json!(now());
    st["queue"] = Value::Array(jobs.iter().map(job_value).collect());
    st["config"] = public_config(&cfg);
    if stale_non_ai_provider_cooldown(&st) {
        st["cooldown"] = Value::Null;
    }

    if let Some(until) = cooldown_until(&st) {
        if until > Utc::now() {
            let maintenance = jobs
                .iter()
                .find(|j| !j.requires_ai && j.kind != "cooldown")
                .cloned();
            if let Some(job) = maintenance {
                let out = finish_non_ai_job(&job, &repos, &ops_payload);
                actions.push(format!("{} {}", job.name, job.kind));
                st["last_result"] = out.clone();
                st["active"] = Value::Null;
                let _ = write_state(&st);
                return json!({"ok": true, "cooldown": cooldown_value(&st), "actions": actions, "result": out});
            }
            st["active"] = Value::Null;
            let _ = write_state(&st);
            return json!({
                "ok": true,
                "cooldown": cooldown_value(&st),
                "actions": ["provider cooldown active; no AI job started"],
                "queue": st["queue"].clone(),
            });
        }
    }

    if daily_used(&st) >= daily_budget(&cfg) {
        set_cooldown(&mut st, &cfg, "daily call budget reached");
        let _ = write_state(&st);
        append_event(&json!({"event": "provider_cooldown", "reason": "daily call budget reached"}));
        return json!({"ok": true, "actions": ["daily call budget reached; provider cooling down"], "cooldown": cooldown_value(&st)});
    }

    // Round-robin dispatch (2026-07-11): pick the FIRST AI job that is NOT the last-dispatched lane,
    // so a single high-priority lane doesn't hog the dispatch slot every sweep. Falls back to
    // jobs.first() when there's only one AI job or the rotation wraps around.
    if jobs.is_empty() {
        st["active"] = Value::Null;
        st["last_result"] =
            json!({"ts": now(), "outcome": "complete", "summary": "no queued autopilot work"});
        let _ = write_state(&st);
        return json!({"ok": true, "actions": [], "queue": []});
    }
    let last_dispatched = st
        .get("last_dispatched")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ai_jobs: Vec<&Job> = jobs.iter().filter(|j| j.requires_ai).collect();
    let pick: &Job = if ai_jobs.len() > 1 {
        ai_jobs
            .iter()
            .find(|j| j.name != last_dispatched)
            .copied()
            .unwrap_or(ai_jobs[0])
    } else {
        jobs.first().expect("jobs is non-empty (checked above)")
    };
    let job = pick.clone();

    st["active"] = job_value(&job);
    st["last_dispatched"] = json!(job.name.clone());
    // Diversify budget is charged HERE, at actual dispatch — a planned re-spec that lost the
    // single_agent slot costs nothing (plan-time bumping read 3/3 spent on a day with ZERO
    // job_started events for the starved lanes; see the Job.diversify doc). The dispatched
    // self-spec is recorded in the lane's spec ledger at the same moment so the differentiation
    // guard sees exactly what actually ran (never a planned-but-undispatched spec).
    if job.diversify {
        charge_diversify_dispatch(&mut st, &job.name);
        if let (Some(cat), Some(goal)) = (job.spec_category.as_deref(), job.spec_goal.as_deref()) {
            record_self_spec_dispatch(&mut st, &job.name, cat, goal);
        }
    }
    // The one-shot human wake override is consumed at ACTUAL dispatch (same rule as the diversify
    // budget): a wake whose job lost the slot stays pending for the next sweep. A pending wake is
    // also consumed when the lane dispatches ANY implement (a healthy lane's normal implement IS
    // the attempt the ack asked for — the flag must not linger and override a much later park).
    if job.wake_override || (job.kind == "implement" && human_wake_pending(&st, &job.name)) {
        consume_human_wake(&mut st, &job.name);
    }
    let _ = write_state(&st);
    append_event(
        &json!({"event": "job_started", "repo": job.name, "kind": job.kind, "requires_ai": job.requires_ai}),
    );

    let result = if job.requires_ai {
        run_ai_job(&job, &repos, &cfg, auto_push, &mut st)
    } else {
        finish_non_ai_job(&job, &repos, &ops_payload)
    };
    if job.kind == "cooldown" {
        for q in jobs
            .iter()
            .filter(|j| j.kind == "cooldown" && j.name != job.name)
        {
            let _ = finish_non_ai_job(q, &repos, &ops_payload);
        }
    }
    actions.push(format!(
        "{} {} -> {}",
        job.name,
        job.kind,
        result
            .get("outcome")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    ));
    st["active"] = Value::Null;
    st["last_result"] = result.clone();
    // Per-lane stuck counter (time-decay for the watchdog's autopilot recover pass). A lane whose
    // fleet outcome is `proof_required` OR `needs_human_spec` produced NO mutation this sweep (both
    // are inert non-AI arms — `proof_required` surfaces a blocker; `needs_human_spec` is the parked
    // cooldown state) — increment. Any other outcome (shipped/blocked/complete/cooldown/etc.) means
    // the lane moved, so clear the entry (and the proof_required cooldown via bump_stuck_counter).
    // The watchdog reads `stuck_sweeps` and only force-heals a lane stuck >= STUCK_SWEEP_THRESHOLD
    // sweeps. While a lane is parked in the proof_required cooldown, its stuck streak PERSISTS so
    // the watchdog's force-heal ladder still sees the full window. Purely additive state key;
    // read_state/write_state round-trip arbitrary JSON.
    let outcome_str = result.get("outcome").and_then(Value::as_str).unwrap_or("");
    let inert_outcome = outcome_str == "proof_required" || outcome_str == "needs_human_spec";
    bump_stuck_counter(&mut st, &job.name, inert_outcome);
    // Resolve the dispatched self-spec in the lane's spec ledger: an inert outcome (incl. an
    // anti-gaming REVERT, which lands as proof_required) marks the spec FAILED so the
    // differentiation guard rotates away from its family; any real outcome marks it MOVED.
    if job.diversify {
        resolve_self_spec_outcome(&mut st, &job.name, inert_outcome);
    }
    // SURFACE-ONCE bookkeeping: this dispatch just filed the lane's needs_human_spec need (the
    // proof record + job_finished event — the one operator notification). Record it so plan_jobs
    // parks identical re-dispatches (see nhs_already_surfaced) instead of starving real jobs.
    if job.kind == "needs_human_spec" && outcome_str == "needs_human_spec" {
        mark_nhs_surfaced(&mut st, &job.name, &job.reason);
    }
    let ops_payload2 = read_ops_payload();
    st["queue"] = Value::Array(
        plan_jobs(&repos, &cfg, &ops_payload2, &mut st, only_name, false)
            .iter()
            .map(job_value)
            .collect(),
    );
    st["ts"] = json!(now());
    let _ = write_state(&st);
    json!({"ok": true, "actions": actions, "result": result, "state": state()})
}

pub fn drain(auto_push: bool, limit: usize) -> Value {
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    if autopilot_paused(&st) {
        st["paused"] = Value::Bool(false);
        st["ts"] = json!(now());
        st["config"] = public_config(&cfg);
        let _ = write_state(&st);
    }
    let cap = if limit == 0 {
        registry::autopilot_targets().len().max(1)
    } else {
        limit
    };
    let mut runs = Vec::new();
    for _ in 0..cap {
        let out = once(auto_push, None);
        let active_elsewhere = out.get("leased").and_then(Value::as_bool) == Some(false);
        let cooled = out.get("cooldown").map(json_truthy).unwrap_or(false);
        let no_actions = out
            .get("actions")
            .and_then(Value::as_array)
            .map(|a| a.is_empty())
            .unwrap_or(false);
        runs.push(out);
        if active_elsewhere || cooled || no_actions {
            break;
        }
        if daily_used(&read_state(&cfg)) >= daily_budget(&cfg) {
            break;
        }
    }
    json!({"ok": true, "runs": runs, "state": state()})
}

pub fn wake(auto_push: bool, only_name: Option<&str>) -> Value {
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    st["paused"] = Value::Bool(false);
    // HUMAN ACK (an override, no longer the only exit — operator directive 2026-07-18): an
    // explicit per-lane wake means the operator reviewed the blocker and (re)spec'd the standing
    // goal. Reset the lane's park + stuck streak + surfaced-need marker (`bump_stuck_counter`'s
    // real-move arm, reused so the accounting stays identical) and arm the ONE-SHOT wake override
    // so the next dispatch runs a normal gated implement on the operator's standing spec — even
    // while the diagnosis is still unhealthy — instead of the autonomous self-respec. The
    // self-spec ledger is deliberately KEPT: past failures still inform differentiation if the
    // lane wedges again.
    if let Some(n) = only_name {
        bump_stuck_counter(&mut st, n, false);
        arm_human_wake(&mut st, n);
    }
    st["ts"] = json!(now());
    st["config"] = public_config(&cfg);
    let _ = write_state(&st);
    append_event(&json!({"event": "autopilot_wake", "repo": only_name.unwrap_or("")}));
    once(auto_push, only_name)
}

pub fn pause() -> Value {
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    st["paused"] = Value::Bool(true);
    st["active"] = Value::Null;
    st["ts"] = json!(now());
    st["config"] = public_config(&cfg);
    let _ = write_state(&st);
    append_event(&json!({"event": "autopilot_pause"}));
    json!({"ok": true, "paused": true, "state": state()})
}

pub fn sweep_snapshots(repos: &[Value]) -> Vec<Value> {
    repos
        .iter()
        .filter(|r| r.is_object())
        .filter_map(|r| {
            let name = r.get("name").and_then(Value::as_str)?.to_string();
            let hb = heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));
            let hist = heartbeat::read_history(r, 1);
            let last = hist.last().cloned().unwrap_or_else(|| json!({}));
            let diag = supervisor::diagnose(r);
            Some(json!({
                "ts": now(),
                "repo": name,
                "running": locks::is_running(r),
                "restarted": false,
                "paused": paths::runtime_dir(r).map(|d| d.join("paused").exists()).unwrap_or(false),
                "status": hb.get("status").cloned().unwrap_or(Value::Null),
                "phase": hb.get("phase").cloned().unwrap_or(Value::Null),
                "iteration": hb.get("iteration").cloned().unwrap_or(Value::Null),
                "last_status": last.get("status").cloned().unwrap_or(Value::Null),
                "diagnosis": diag.get("category").cloned().unwrap_or(Value::Null),
                "autopilot": true,
            }))
        })
        .collect()
}

fn run_ai_job(job: &Job, repos: &[Value], cfg: &Value, auto_push: bool, st: &mut Value) -> Value {
    let Some(repo) = repos
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(job.name.as_str()))
    else {
        return proof(job, "blocked", "repo not found", None, None);
    };
    let repo_path = paths::repo_path(repo);
    if !Path::new(&repo_path).is_dir() {
        return proof(
            job,
            "blocked",
            &format!("configured repo path does not exist or is not a directory: {repo_path}"),
            Some(json!({"configured_path": repo_path})),
            Some(repo),
        );
    }
    let provider = cfg
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("openrouter");
    let key_env = provider_key_ref(provider);
    let key_ref = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or(key_env);
    let Some(key_value) = resolve_key(key_ref) else {
        return proof(
            job,
            "blocked",
            &format!("{key_ref} is not set for Solomon Autopilot"),
            None,
            None,
        );
    };
    increment_daily(st);
    append_event(
        &json!({"event": "agent_call_started", "repo": job.name, "provider": cfg["provider"], "model": cfg["model"]}),
    );
    let out = run_repo_once(repo, cfg, auto_push, key_env, &key_value, job.spec_goal.as_deref());
    let quota = pi::is_quota_error_output(&out.stdout, &out.stderr)
        || out.stdout.contains("\"reason\":\"quota_error\"")
        || out.stderr.contains("quota_error");
    if quota {
        // catalog #2: a quota error parks THAT endpoint (exponential, 6h cap) — never the
        // fleet for cfg.cooldown_s (the 86400s blanket that put Gen-2 to sleep for a day).
        set_quota_cooldown(st, cfg);
    }
    let diag = supervisor::diagnose(repo);
    let hist = heartbeat::read_history(repo, 1);
    let latest = hist.last().cloned().unwrap_or_else(|| json!({}));
    let outcome = if quota {
        "cooldown"
    } else if latest.get("status").and_then(Value::as_str) == Some("shipped") {
        "shipped"
    } else if latest.get("status").and_then(Value::as_str) == Some("blocked") {
        "blocked"
    } else if latest.get("status").and_then(Value::as_str) == Some("reverted") || out.code == 0 {
        "proof_required"
    } else {
        "blocked"
    };
    let summary = if quota {
        "provider quota hit; Solomon Autopilot cooled down instead of retrying".to_string()
    } else {
        latest
            .get("summary")
            .or_else(|| latest.get("last_summary"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                diag.get("evidence")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("run-improver exited {}", out.code))
    };
    let extra = json!({
        "command_code": out.code,
        "diagnosis": diag,
        "latest_history": latest,
        "stdout_tail": tail_chars(&out.stdout, 1200),
        "stderr_tail": tail_chars(&out.stderr, 1200),
        "cooldown": cooldown_value(st),
    });
    proof(job, outcome, &summary, Some(extra), Some(repo))
}

fn finish_non_ai_job(job: &Job, repos: &[Value], ops_payload: &Value) -> Value {
    let repo = repos
        .iter()
        .find(|r| r.get("name").and_then(Value::as_str) == Some(job.name.as_str()));
    match job.kind.as_str() {
        "maintenance" => {
            if let Some(r) = repo {
                let diag = supervisor::diagnose(r);
                if diag.get("category").and_then(Value::as_str) == Some("stale_lock")
                    && diag.get("auto_safe").and_then(Value::as_bool) == Some(true)
                {
                    let cleared = locks::clear_lock(r);
                    return proof(
                        job,
                        if cleared.get("ok").and_then(Value::as_bool) == Some(true) {
                            "complete"
                        } else {
                            "blocked"
                        },
                        "stale lock maintenance ran",
                        Some(json!({"diagnosis": diag, "clear_lock": cleared})),
                        Some(r),
                    );
                }
            }
            proof(
                job,
                "complete",
                "non-LLM maintenance checked; no unsafe action taken",
                None,
                repo,
            )
        }
        "cooldown" => proof(
            job,
            "cooldown",
            "last project heartbeat was quota_error; provider cooldown owns retry timing",
            repo.map(|r| json!({"diagnosis": supervisor::diagnose(r)})),
            repo,
        ),
        "proof_required" | "blocked" => proof(
            job,
            job.kind.as_str(),
            &job.reason,
            Some(
                json!({"ops": ops_payload.get("projects").and_then(|p| p.get(&job.name)).cloned().unwrap_or(Value::Null)}),
            ),
            repo,
        ),
        // needs_human_spec: a lane parked by the proof_required cooldown. Inert (like
        // proof_required) — it surfaces the need to the dashboard/operator without mutating the
        // repo. The outcome is "needs_human_spec" so `bump_stuck_counter` treats it as a
        // non-proof_required outcome and CLEARS the stuck counter + cooldown only when the lane
        // actually moves; while parked, the stuck counter keeps its streak (the watchdog's
        // force-heal ladder still sees the full window). Filing the need here = reporting it; the
        // operator specs the real fix and wakes the lane. The message uses the threshold constant
        // (not a second read_state call) so finish_non_ai_job stays IO-free in this arm — the exact
        // consecutive count is in the persisted autopilot_state.json the dashboard already reads.
        "needs_human_spec" => proof(
            job,
            "needs_human_spec",
            &job.reason,
            Some(json!({
                "ops": ops_payload.get("projects").and_then(|p| p.get(&job.name)).cloned().unwrap_or(Value::Null),
                "need_kind": "decision",
                "need": format!(
                    "lane '{0}' parked: {1} consecutive proof_required sweeps without a mutation. \
                     Review the blocker, spec a real fix, then wake the lane.",
                    job.name, PROOF_COOLDOWN_THRESHOLD
                ),
            })),
            repo,
        ),
        _ => proof(job, "complete", &job.reason, None, repo),
    }
}

fn plan_jobs(
    repos: &[Value],
    cfg: &Value,
    ops_payload: &Value,
    st: &mut Value,
    only_name: Option<&str>,
    emit_proofs: bool,
) -> Vec<Job> {
    let targets: Vec<String> = only_name
        .map(|n| vec![n.to_string()])
        .unwrap_or_else(|| cfg_targets(cfg));
    let manual = st
        .get("manual_queue")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut jobs = Vec::new();
    for name in targets {
        let Some(repo) = repos
            .iter()
            .find(|r| r.get("name").and_then(Value::as_str) == Some(name.as_str()))
        else {
            jobs.push(Job {
                name,
                kind: "blocked".into(),
                state: "blocked".into(),
                priority: 5,
                requires_ai: false,
                reason: "repo is listed in autopilot targets but missing from registry".into(),
                next_action: "fix repos.json target/path".into(),
                diversify: false,
                spec_goal: None,
                spec_category: None,
                wake_override: false,
                state_fingerprint: "registry_missing".into(),
            });
            continue;
        };
        // Skip paused lanes — a per-lane `paused` sentinel means "hands off this lane for the
        // automated sweep". Without this, a paused proof_required lane blocks the queue (once()
        // picks jobs.first() and the non-AI proof_required job completes instantly, cycling
        // through the paused lanes without ever reaching the implement jobs behind them).
        if paths::runtime_dir(repo).map(|d| d.join("paused").exists()).unwrap_or(false) {
            continue;
        }
        let mut diag = supervisor::diagnose(repo);
        // STALE-ERROR REVALIDATION (dispatch path only): a persisted heartbeat error of a
        // re-checkable class (dirty tree / out-of-band base / stranded branch / gh-auth) is
        // re-run against the repo before the scheduler routes on it — a condition that no longer
        // reproduces is cleared to idle (the 2026-07-15 solomon self-lane wedge: 'controller tree
        // dirty — 3 commit(s) not on origin/main' persisted a day past main==origin/main, parking
        // the lane on proof_required forever). Gated to emit_proofs so the frequent state()
        // display path stays file-only; the display self-corrects after the next dispatch sweep.
        if emit_proofs && supervisor::revalidate_persisted_error(repo, &diag).is_some() {
            diag = supervisor::diagnose(repo); // re-read the now-idle heartbeat (file-only, cheap)
        }
        let diag_cat = diag.get("category").and_then(Value::as_str).unwrap_or("ok");
        let ops_project = ops_payload
            .get("projects")
            .and_then(|p| p.get(&name))
            .cloned()
            .unwrap_or(Value::Null);
        let ops_status = ops_project
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("grey");
        let manual_hit = manual.iter().any(|v| v.as_str() == Some(name.as_str()));
        let state_fingerprint = retry_state_fingerprint(&ops_project, diag_cat);
        let same_state_proof_fresh = proof_matches_fresh_state(&name, &state_fingerprint);
        let normal_proof_fresh = same_state_proof_fresh
            || (ops_status != "red" && ops_status != "yellow" && explicit_proof_is_fresh(&name));
        let mut priority = match ops_status {
            "red" => 10,
            "yellow" => 30,
            _ => 70,
        };
        if manual_hit {
            priority = 1;
        }
        let (mut kind, mut state, mut requires_ai, mut reason, mut next_action) = if diag_cat
            == "quota_error"
            && quota_heartbeat_matches_config(repo, cfg)
        {
            (
                "cooldown",
                "cooldown",
                false,
                "provider quota/rate-limit heartbeat",
                "wait for cooldown; run non-LLM probes and cleanup",
            )
        } else if diag_cat == "quota_error" {
            (
                "implement",
                "queued",
                true,
                "stale quota heartbeat does not match current provider/model",
                "run one gated RSI iteration under the current provider",
            )
        } else if diag_cat == "stale_lock"
            && diag.get("auto_safe").and_then(Value::as_bool) == Some(true)
        {
            (
                "maintenance",
                "queued",
                false,
                "dead stale lock can be cleared without AI",
                "clear stale lock",
            )
        } else if diag_cat != "ok" && diag.get("auto_safe").and_then(Value::as_bool) != Some(true) {
            (
                "proof_required",
                "proof_required",
                false,
                diag.get("evidence")
                    .and_then(Value::as_str)
                    .unwrap_or("project is unhealthy"),
                "surface blocker and avoid retry theater",
            )
        } else if manual_hit || !normal_proof_fresh {
            (
                "implement",
                "queued",
                true,
                if manual_hit {
                    "manual autopilot wake request"
                } else if ops_status == "red" || ops_status == "yellow" {
                    "ops outcome needs improvement; prior proof is stale or missing"
                } else {
                    "proof record is stale or missing"
                },
                "run one gated RSI iteration",
            )
        } else {
            (
                "complete",
                "complete",
                false,
                "fresh proof owns this retry window; hold unchanged ops state",
                "hold",
            )
        };
        // HUMAN WAKE OVERRIDE (operator directive 2026-07-18: the wake STAYS as an exit, it is
        // just no longer the ONLY one). An explicit per-lane wake() ack'd this lane: the operator
        // reviewed the blocker and (re)spec'd the standing repos.json goal. Route the lane to a
        // NORMAL gated implement on THAT spec — bypassing both the inert proof_required arm and
        // the autonomous self-respec (no diversify budget charged, no generated goal). One-shot:
        // `once()` consumes the flag when this job actually dispatches (charge-at-dispatch, same
        // rule as the diversify budget — a plan that loses the slot must not eat the wake).
        let mut wake_job = false;
        if kind == "proof_required" && human_wake_pending(st, &name) {
            wake_job = true;
            kind = "implement";
            state = "queued";
            requires_ai = true;
            reason = "operator wake ack — running one gated RSI iteration on the operator's \
                      standing spec (human override of the autonomous re-spec loop)";
            next_action = "run one gated RSI iteration under the operator-reviewed standing goal";
        }
        // AUTONOMOUS RE-SPEC (operator directive: NO human requirement in the lane loop — a lane
        // must never park indefinitely waiting for a human spec). A lane that would otherwise sit
        // inert — either STRUCTURALLY stuck (stranded/unmerged branch, hung PID, un-pushed base)
        // OR carrying a proof_required park entry (PROOF_COOLDOWN_THRESHOLD consecutive inert
        // sweeps armed it), whether that park is ACTIVE **or EXPIRED** — instead gets a BOUNDED
        // autonomous self-respec: `generate_self_spec` produces a FRESH bounded spec (NEW goal
        // text, category-rotated and materially different from the last failed specs — see the
        // self-spec ledger) and the lane dispatches it as a real gated AI implement iteration.
        // PARK EXPIRY RE-ARMS THIS PATH: an expired-but-present park entry (nothing cleared it —
        // no human wake, no real outcome) routes straight back here instead of the old inert
        // proof_required re-fire + re-arm rotation. Capped at DIVERSIFY_DAILY_CAP per lane per
        // day; past the cap, BACK OFF to low-frequency autonomous retry (the inert proof_required
        // emit-bypass below — no LLM spend) and resume when the daily count resets. Never a human
        // park, never infinite spend. Anti-gaming stays intact: the gated implement can never
        // merge a bad change (the RSI gates + anti-gaming revert rails + the improver's
        // hypothesis/freshness/progress ledgers police fake progress); fleet only decides to keep
        // ATTEMPTING autonomously — with better-differentiated specs — rather than waiting on a
        // human. `needs_human_spec` is never chosen.
        // Holds the diversify/backoff reason string; assigned (and `reason` re-pointed at it) only on
        // the would_park path, so it must outlive the borrow until `jobs.push` below.
        let diversify_reason: String;
        // Marks the pushed job as a diversify attempt so `once()` charges the budget at ACTUAL
        // dispatch (job_started). Planning must NOT spend: plan_jobs runs 2x per sweep (dispatch +
        // display re-plan) and every state() poll, while only ONE planned job wins the single_agent
        // slot — plan-time bumping burned 3/3 with ZERO job_started events for the lane (the
        // 2026-07-17 asmodeus+solomon ~20h proof_required starvation).
        let mut diversify_job = false;
        // The generated fresh spec (goal text + category) for a diversify attempt; carried on the
        // Job so `once()` records the SAME spec it dispatches in the self-spec ledger.
        let mut spec_goal: Option<String> = None;
        let mut spec_category: Option<String> = None;
        let would_park = kind == "proof_required"
            && (supervisor::is_structurally_stuck(diag_cat)
                || proof_cooldown_entry(st, &name).is_some());
        if would_park {
            let today = today_local();
            if diversify_count(st, &name, &today) < DIVERSIFY_DAILY_CAP {
                // n = the attempt number IF this job dispatches; the count itself is only bumped
                // by charge_diversify_dispatch when once() actually starts the job.
                let n = diversify_count(st, &name, &today) + 1;
                let standing_goal = registry::project_goal(repo);
                let (category, goal) = generate_self_spec(st, &name, reason, &standing_goal);
                diversify_job = true;
                kind = "implement";
                state = "queued";
                requires_ai = true;
                // `reason` still carries diagnose()'s evidence (from the proof_required arm); frame it
                // as a diversification attempt so the dashboard shows WHY an AI iteration is running.
                diversify_reason = format!(
                    "autonomous diversification {n}/{DIVERSIFY_DAILY_CAP} (no human park): repeated \
                     proof_required — dispatching a FRESH, DIFFERENT self-generated spec \
                     (category: {category}), not the identical failing change. Blocker: {reason}"
                );
                reason = &diversify_reason;
                next_action = "run one gated RSI iteration taking a DIFFERENT approach from the prior \
                               failing change; the RSI gates still decide (no fake progress)";
                spec_goal = Some(goal);
                spec_category = Some(category.to_string());
            } else {
                // Daily diversification budget spent — BACK OFF to low-frequency autonomous retry:
                // keep proof_required (the inert emit-bypass below — no LLM spend, rate-limited) and
                // re-attempt diversification when the daily count resets. NEVER a human park.
                diversify_reason = format!(
                    "diversification budget spent for today ({DIVERSIFY_DAILY_CAP}/day) — backing off \
                     to low-frequency autonomous retry (no human park). Blocker: {reason}"
                );
                reason = &diversify_reason;
            }
        }
        // needs_human_spec SURFACE-ONCE PARK (dispatch path only, both routes above): a need that
        // was already surfaced with this exact fingerprint is NOT re-enqueued — re-dispatching the
        // identical inert job every sweep at red priority (10) starved every queued real job (30)
        // indefinitely (the asmodeus ~90-120s re-dispatch loop). The stuck streak still bumps
        // inline (identical accounting to a dispatched needs_human_spec, same as the
        // proof_required emit-bypass below) so the watchdog force-heal ladder keeps its window.
        // The display path (emit_proofs=false) still queues it so the dashboard shows the park;
        // a changed fingerprint (lane state changed), a real outcome (bump_stuck_counter clears),
        // or an explicit per-lane wake (human ack) re-surfaces the need once.
        if kind == "needs_human_spec" && emit_proofs && nhs_already_surfaced(st, &name, reason) {
            bump_stuck_counter(st, &name, true);
            continue;
        }
        if kind == "complete" {
            continue;
        }
        // DISPATCH-BYPASS for inert proof_required (Fix 4): a proof_required job does NO mutation
        // — it only surfaces a blocker. On the `once()` dispatch path (emit_proofs=true), emit its
        // proof record as a side effect here and `continue` so the dispatch slot is freed for an
        // actual implement/AI job behind it. The `needs_human_spec` escalation still queues (it
        // carries the operator-facing need). The `state()` display path (emit_proofs=false) still
        // queues proof_required so the dashboard shows the blocker.
        //
        // PROGRESS-LEDGER WIRING (the proof-required-ledger-fix cycle, 2026-07-18): a proof_required
        // emit-bypass does NO mutation, so it IS a zero-delta completion of the (proof_required,
        // goal, diagnosis) work — the exact retry-theater case the 3-strike quarantine (catalog #5)
        // exists to kill. Before this wiring, the emit-bypass called `proof()` + `bump_stuck_counter`
        // but NEVER `progress::record_outcome`, so the quarantine never fired and the lane re-fired
        // the same dead work forever (solomon self-lane: 39 consecutive, 0 shipped). Now: build the
        // lane Ctx, seed the ledger entry, snapshot the pre-state, emit the proof, then record the
        // outcome — 3 zero-delta proof_required completions quarantine the key and the selector
        // picks different work (the autonomous re-spec path above). The outcome word is "noop"
        // (proof_required is inert — never "shipped"; same discipline as orchestrator.rs:655-660).
        if kind == "proof_required" && emit_proofs {
            // A proof is the durable record of this inert diagnosis. Re-emitting the same blocker
            // every sweep creates duplicate pages/events and can race across watchdog processes;
            // keep the stuck streak moving, but let the existing proof own the retry window.
            if same_state_proof_fresh {
                bump_stuck_counter(st, &name, true);
                continue;
            }
            let ops_project = ops_payload
                .get("projects")
                .and_then(|p| p.get(&name))
                .cloned()
                .unwrap_or(Value::Null);
            let job = Job {
                name: name.clone(),
                kind: "proof_required".into(),
                state: "proof_required".into(),
                priority,
                requires_ai: false,
                reason: reason.to_string(),
                next_action: next_action.to_string(),
                diversify: false,
                spec_goal: None,
                spec_category: None,
                wake_override: false,
                state_fingerprint: state_fingerprint.clone(),
            };

            // Progress-ledger wiring: seed the entry + snapshot the pre-state BEFORE proof() (so
            // the post-state comparison in record_outcome measures the real delta, not our own
            // bookkeeping). The key uses the standing repos.json goal as the goal component (the
            // lane's north-star — the same goal a real implement iteration would select) and the
            // diagnosis component is read from `runtime/<name>/escalation.json` via
            // `progress::current_diagnosis(&ctx)` — the SAME reader the iteration path's
            // `filter_quarantined_selection` uses (progress.rs:55-61), so a future extension of
            // the iteration path to consult proof_required quarantines sees the same key. A lane
            // whose diagnosis changes (e.g. gh_not_ready -> ok) gets a fresh key, so a healed lane
            // is not blocked by the stale diagnosis's quarantine.
            //
            // NOTE (reviewer finding, 2026-07-18): today the iteration path's
            // `filter_quarantined_selection` queries `progress_key(ctx.phase="implement", goal,
            // current_diagnosis)` — a DIFFERENT key shape (ctx.phase="implement", not
            // "proof_required") — so the quarantine this call arms is NOT currently consulted by
            // the iteration path. The autonomous re-spec path above (the `would_park` branch)
            // consults `proof_cooldown_entry` (armed by `bump_stuck_counter`), not
            // `progress::quarantined`. So this `record_outcome` call is DEFENSE-IN-DEPTH
            // OBSERVABILITY: it records the zero-delta completion in the progress ledger so the
            // quarantine state is observable + so a future extension of the selector to consult
            // proof_required keys (e.g. routing proof_required through
            // `filter_quarantined_selection` with ctx.phase="proof_required") gets the strikes for
            // free. The PRIMARY retry-theater killer remains `bump_stuck_counter` → `proof_cooldown`
            // → the `would_park` autonomous re-spec path above. This call does no harm (it writes
            // to the ledger, which is a read-only file to the selector) and adds real
            // observability (the operator can read `runtime/<name>/progress.json` and see the
            // strike count even when the cooldown hasn't armed yet).
            let goal_text = registry::project_goal(repo);
            let provider = registry::project_provider(repo);
            let model = registry::project_model(repo);
            let model_opt: Option<&str> = if model.is_empty() { None } else { Some(&model) };
            let mut ctx = Ctx::configure(
                &paths::repo_path(repo),
                &name,
                &provider,
                model_opt,
            );
            let proof_diag = progress::current_diagnosis(&ctx);
            let proof_key = progress::progress_key("proof_required", &goal_text, &proof_diag);
            let proof_pre_hash = if proof_key.is_empty() {
                String::new()
            } else {
                progress::note_selected(&ctx, &proof_key, &goal_text);
                progress::state_hash(&ctx)
            };

            let _ = proof(
                &job,
                "proof_required",
                reason,
                Some(json!({"ops": ops_project})),
                Some(repo),
            );
            // Bump the stuck counter inline so the proof cooldown + watchdog force-heal still arm
            // even though the job was never dispatched to finish_non_ai_job.
            bump_stuck_counter(st, &name, true);

            // Record the zero-delta completion in the progress ledger (defense-in-depth
            // observability — see the NOTE above). The record_outcome call computes post_hash
            // internally and compares to proof_pre_hash; because proof_required is inert,
            // post_hash == pre_hash → no_delta → strike++. At QUARANTINE_STRIKES (3) the key is
            // quarantined for 24h in the ledger; the selector does not yet consult this key shape,
            // but the quarantine state is observable in `runtime/<name>/progress.json` for the
            // operator and for a future selector extension. calibration::resolve_pending is also
            // folded in by record_outcome (no-op here — no pending calibration attempt was stamped
            // for an inert proof_required).
            if !proof_key.is_empty() {
                progress::record_outcome(&mut ctx, &proof_key, &proof_pre_hash, "noop");
            }
            continue;
        }
        if kind == "cooldown" {
            priority = 3;
        }
        jobs.push(Job {
            name,
            kind: kind.into(),
            state: state.into(),
            priority,
            requires_ai,
            reason: reason.into(),
            next_action: next_action.into(),
            diversify: diversify_job,
            spec_goal,
            spec_category,
            wake_override: wake_job,
            state_fingerprint,
        });
    }
    jobs.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            // AI (true) before non-AI (false) at equal priority: a runnable AI improver
            // job must not be permanently starved behind a stuck non-AI proof_required
            // no-op (e.g. dotz). `b.cmp(&a)` puts `true` first. proof_required still runs
            // whenever it outranks (lower priority number) any AI job — the ladder is intact.
            .then_with(|| b.requires_ai.cmp(&a.requires_ai))
            .then_with(|| a.name.cmp(&b.name))
    });
    jobs
}

/// Run one gated `run-improver --once` iteration for the repo. `goal_override` (a self-generated
/// FRESH spec from the autonomous re-spec path — see `generate_self_spec`) replaces the standing
/// repos.json goal for THIS dispatch only; None runs the standing goal. The override is goal text
/// only — provider/model/gate/ship stay exactly the configured ones, so every safety rail
/// (RSI gates, anti-gaming reverts, provenance) applies unchanged to self-spec runs.
fn run_repo_once(
    repo: &Value,
    cfg: &Value,
    auto_push: bool,
    key_env: &str,
    key_value: &str,
    goal_override: Option<&str>,
) -> proc::RunOut {
    let program = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "solomon.exe".to_string());
    let provider = cfg
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("ollama-cloud");
    let model = cfg
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.2");
    let argv = vec![
        program,
        "run-improver".into(),
        "--repo".into(),
        paths::repo_path(repo),
        "--name".into(),
        paths::repo_name(repo),
        "--provider".into(),
        provider.into(),
        "--model".into(),
        model.into(),
        "--ship".into(),
        registry::effective_ship(repo, auto_push),
        "--gate".into(),
        registry::project_gate(repo).unwrap_or_default(),
        "--pr-target-branch".into(),
        registry::project_pr_target_branch(repo),
        "--reasoning".into(),
        registry::project_reasoning(repo),
        "--interval".into(),
        "1".into(),
        "--max-iterations".into(),
        "1".into(),
        "--goal".into(),
        goal_override
            .map(str::to_string)
            .unwrap_or_else(|| registry::project_goal(repo)),
        "--once".into(),
    ];
    run_with_env(
        &argv,
        Path::new(&paths::repo_path(repo)),
        key_env,
        key_value,
        Duration::from_secs(DEFAULT_RUN_TIMEOUT_S),
    )
}

fn run_with_env(
    argv: &[String],
    cwd: &Path,
    key_env: &str,
    key_value: &str,
    timeout: Duration,
) -> proc::RunOut {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("SOLOMON_AUTOPILOT_MODE", "single_agent")
        .env(key_env, key_value);
    proc::apply_clean_env(&mut cmd);
    #[cfg(windows)]
    cmd.creation_flags(proc::hidden_flags(false, false));
    match run_prepared(cmd, timeout) {
        Ok(o) => o,
        Err(e) => proc::RunOut {
            code: if e.kind() == std::io::ErrorKind::TimedOut {
                124
            } else {
                -1
            },
            stdout: String::new(),
            stderr: e.to_string(),
        },
    }
}

fn run_prepared(mut cmd: Command, timeout: Duration) -> std::io::Result<proc::RunOut> {
    use wait_timeout::ChildExt;
    let mut child = cmd.spawn()?;
    let out_rx = read_pipe_async(child.stdout.take());
    let err_rx = read_pipe_async(child.stderr.take());
    match child.wait_timeout(timeout)? {
        Some(status) => Ok(proc::RunOut {
            code: status.code().unwrap_or(-1),
            stdout: collect_pipe(out_rx),
            stderr: collect_pipe(err_rx),
        }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "autopilot job timed out",
            ))
        }
    }
}

fn read_pipe_async<T: Read + Send + 'static>(pipe: Option<T>) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut s) = pipe {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            let _ = tx.send(buf);
        });
    }
    rx
}

fn collect_pipe(rx: std::sync::mpsc::Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
}

fn proof(
    job: &Job,
    outcome: &str,
    summary: &str,
    extra: Option<Value>,
    repo: Option<&Value>,
) -> Value {
    let mut v = json!({
        "ts": now(),
        "repo": job.name,
        "job": job.kind,
        "state": job.state,
        "outcome": outcome,
        "summary": summary,
        "requires_ai": job.requires_ai,
        "priority": job.priority,
        "next_action": job.next_action,
        "state_fingerprint": job.state_fingerprint,
        "extra": extra.unwrap_or(Value::Null),
    });
    if let Some(r) = repo {
        v["diagnosis"] = supervisor::diagnose(r);
        v["heartbeat"] = heartbeat::read_heartbeat(r).unwrap_or(Value::Null);
        v["latest_history"] = heartbeat::read_history(r, 1)
            .last()
            .cloned()
            .unwrap_or(Value::Null);
    } else {
        v["diagnosis"] = Value::Null;
    }
    write_proof_value(&job.name, &v);
    append_event(
        &json!({"event": "job_finished", "repo": job.name, "job": job.kind, "outcome": outcome}),
    );
    v
}

fn write_proof_value(name: &str, v: &Value) {
    if let Some(rt) = paths::runtime_dir(&json!({"name": name})) {
        write_proof_value_at(&rt, v);
    }
}

fn write_proof_value_at(rt: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(rt);
    let _ = proc::atomic_write_json(&rt.join(PROOF_FILE), v);
}

fn read_proof(name: &str) -> Option<Value> {
    let rt = paths::runtime_dir(&json!({"name": name}))?;
    let file_proof = std::fs::read(rt.join(PROOF_FILE))
        .or_else(|_| std::fs::read(rt.join(LEGACY_PROOF_FILE)))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let history_proof = heartbeat::read_history(&json!({"name": name}), 1)
        .last()
        .and_then(|h| proof_from_history(name, h));
    freshest_proof(file_proof, history_proof)
}

/// True iff the lane's most recent dispatch recorded a SUBPROCESS SPAWN FAILURE
/// (`command_code == -1`, e.g. the configured repo path does not exist → `os error 267`).
/// Used by `supervisor::diagnose` to distinguish a lane that was DISPATCHED and failed to
/// spawn (monitoring-theater: diagnose would otherwise read a stale/absent heartbeat and
/// report "ok/idle/healthy:true") from a lane that has simply never been dispatched. The
/// proof file is the dispatch's own record — a test fixture that never dispatched writes
/// no proof, so this never fires for unit-test lanes.
pub fn lane_spawn_failed(repo: &Value) -> bool {
    read_proof(&paths::repo_name(repo))
        .and_then(|p| p.get("extra").and_then(|e| e.get("command_code")).and_then(Value::as_i64))
        .map(|c| c == -1)
        .unwrap_or(false)
}

fn proof_matches_fresh_state(name: &str, state_fingerprint: &str) -> bool {
    read_explicit_proof(name)
        .filter(|proof| {
            proof.get("state_fingerprint").and_then(Value::as_str) == Some(state_fingerprint)
        })
        .as_ref()
        .map(proof_is_recent)
        .unwrap_or(false)
}

fn explicit_proof_is_fresh(name: &str) -> bool {
    read_explicit_proof(name)
        .as_ref()
        .map(proof_is_recent)
        .unwrap_or(false)
}

fn read_explicit_proof(name: &str) -> Option<Value> {
    let rt = paths::runtime_dir(&json!({"name": name}))?;
    std::fs::read(rt.join(PROOF_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
}

fn proof_is_recent(proof: &Value) -> bool {
    proof_time(proof)
        .map(|ts| Utc::now().signed_duration_since(ts))
        .map(|age| {
            age >= ChronoDuration::zero() && age < ChronoDuration::hours(PROOF_FRESH_HOURS)
        })
        .unwrap_or(false)
}

/// Stable retry identity: details such as "age 37.2h" intentionally do not participate because
/// they change every sweep. A diagnosis-category or per-probe status transition does participate,
/// so a genuinely new RED/YELLOW state bypasses an old proof immediately.
fn retry_state_fingerprint(ops_project: &Value, diagnosis_category: &str) -> String {
    let mut probes: Vec<String> = ops_project
        .get("probes")
        .and_then(Value::as_object)
        .map(|p| {
            p.iter()
                .map(|(id, status)| {
                    format!("{id}={}", status.as_str().unwrap_or("invalid"))
                })
                .collect()
        })
        .unwrap_or_default();
    probes.sort();
    format!(
        "diag={diagnosis_category}|ops={}|restart_forbidden={}|operator_gated={}|probes={}",
        ops_project
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("grey"),
        ops_project
            .get("restart_forbidden")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ops_project
            .get("red_operator_gated_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        probes.join(",")
    )
}

fn proof_from_history(name: &str, h: &Value) -> Option<Value> {
    let status = h.get("status").and_then(Value::as_str)?;
    let outcome = match status {
        "shipped" => "shipped",
        "blocked" => "blocked",
        "quota_error" => "cooldown",
        "reverted" => "proof_required",
        _ => "proof_required",
    };
    Some(json!({
        "ts": h.get("ts").cloned().unwrap_or_else(|| json!(now())),
        "repo": name,
        "job": "implement",
        "state": status,
        "outcome": outcome,
        "summary": h.get("summary").cloned().unwrap_or_else(|| json!("latest RSI iteration recorded")),
        "requires_ai": true,
        "priority": 10,
        "next_action": if outcome == "blocked" { "review blocker and choose the next gated action" } else { "collect evidence and choose the next gated improvement" },
        "extra": {"latest_history": h},
        "latest_history": h,
    }))
}

fn freshest_proof(a: Option<Value>, b: Option<Value>) -> Option<Value> {
    match (a, b) {
        (Some(left), Some(right)) => {
            if proof_time(&right) > proof_time(&left) {
                Some(right)
            } else {
                Some(left)
            }
        }
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

fn proof_time(v: &Value) -> Option<DateTime<Utc>> {
    v.get("ts").and_then(Value::as_str).and_then(parse_ts)
}

fn proof_records() -> Value {
    let mut out = Map::new();
    for name in registry::autopilot_targets() {
        if let Some(v) = read_proof(&name) {
            out.insert(name, v);
        }
    }
    Value::Object(out)
}

fn acquire_lock() -> Result<Option<AutopilotLease>, String> {
    let dir = paths::here().join("runtime");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // Multi-slot single-flight (max_concurrent_agent_calls=2): try the primary slot first, then
    // the secondary. Up to 2 concurrent `once()` calls may run; a 3rd is a single-flight no-op.
    let primary = dir.join(LOCK_FILE);
    let secondary = dir.join(LOCK_FILE_1);
    let max = max_concurrent(&registry::autopilot_config()).max(1) as usize;
    if max >= 2 {
        if let Some(l) = acquire_lock_at(primary.clone())? {
            return Ok(Some(l));
        }
        if let Some(l) = acquire_lock_at(secondary)? {
            return Ok(Some(l));
        }
        Ok(None)
    } else {
        acquire_lock_at(primary)
    }
}

fn autopilot_lock_live() -> bool {
    let dir = paths::here().join("runtime");
    autopilot_lock_live_at(&dir.join(LOCK_FILE)) || autopilot_lock_live_at(&dir.join(LOCK_FILE_1))
}

fn autopilot_lock_live_at(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let pid = raw.trim().parse::<i64>().unwrap_or(0);
    pid != 0 && locks::pid_alive(pid)
}

fn acquire_lock_at(path: PathBuf) -> Result<Option<AutopilotLease>, String> {
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let pid = raw.trim().parse::<i64>().unwrap_or(0);
        if pid != 0 && locks::pid_alive(pid) {
            return Ok(None);
        }
        let _ = std::fs::remove_file(&path);
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut f) => {
            f.write_all(std::process::id().to_string().as_bytes())
                .map_err(|e| e.to_string())?;
            Ok(Some(AutopilotLease { path }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn read_state(cfg: &Value) -> Value {
    std::fs::read(state_path())
        .or_else(|_| std::fs::read(legacy_state_path()))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .map(|mut st: Value| {
            st["mode"] = json!("single_agent");
            st["config"] = public_config(cfg);
            st
        })
        .unwrap_or_else(|| {
            json!({
                "ts": now(),
                "mode": "single_agent",
                "active": null,
                "queue": [],
                "manual_queue": [],
                "cooldown": null,
                "daily": {"date": today(), "calls": 0},
                "paused": false,
                "config": public_config(cfg),
            })
        })
}

fn write_state(st: &Value) -> std::io::Result<()> {
    let p = state_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    proc::atomic_write_json(&p, st)
}

/// Maintain `st["stuck"][name] = {"sweeps": N, "ts": <iso>}`. When `is_proof_required`, increment
/// the lane's sweep count (it took no mutating action this sweep); otherwise remove the entry (the
/// lane moved, so it is no longer stuck). The schema is owned here so the state stays consistent;
/// the watchdog reads it read-only via `stuck_sweeps`. Best-effort: never panics on a malformed map.
///
/// Also maintains the proof_required COOLDOWN: when a lane's consecutive count reaches
/// PROOF_COOLDOWN_THRESHOLD, `arm_proof_cooldown` arms it for PROOF_COOLDOWN_S (kills retry
/// theater — `plan_jobs` then routes the lane to a BOUNDED autonomous diversification instead of
/// another inert proof_required, never a human park). On any non-proof_required outcome, both
/// `stuck` and `proof_cooldowns` are cleared (the lane moved, so it is no longer stuck NOR parked).
fn bump_stuck_counter(st: &mut Value, name: &str, is_proof_required: bool) {
    if !st.get("stuck").map(Value::is_object).unwrap_or(false) {
        st["stuck"] = json!({});
    }
    let stuck = match st["stuck"].as_object_mut() {
        Some(m) => m,
        None => return,
    };
    if is_proof_required {
        let prev = stuck
            .get(name)
            .and_then(|e| e.get("sweeps"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let next = prev + 1;
        stuck.insert(name.to_string(), json!({"sweeps": next, "ts": now()}));
        // Arm the proof_required cooldown at the threshold — one shot per threshold crossing. A
        // lane already parked (until in the future) keeps its existing `until` (no double-arm);
        // a lane whose cooldown EXPIRED re-arms here on the next threshold crossing.
        if next >= PROOF_COOLDOWN_THRESHOLD {
            let already_active = proof_cooldown_active_at(st, name, Utc::now());
            if !already_active {
                arm_proof_cooldown(st, name, next, Utc::now());
            }
        }
    } else {
        stuck.remove(name);
        // The lane moved (shipped/blocked/complete/cooldown) — clear any proof_required park so
        // it is not held past its recovery. `bump_stuck_counter` is called AFTER the job runs, so a
        // lane that was parked and then produced a real outcome is un-parked immediately. The
        // surfaced needs_human_spec marker clears with it: a lane that moved gets a fresh
        // surfacing if it ever wedges again.
        clear_proof_cooldown(st, name);
        clear_nhs_surfaced(st, name);
    }
}

/// Read a lane's consecutive-`proof_required`-sweep count from the persisted autopilot state.
/// 0 when the lane is not stuck (entry absent) or the state is unreadable. Lets the watchdog's
/// autopilot recover pass gate its force-heal on a real time-decay window (STUCK_SWEEP_THRESHOLD)
/// without owning the state schema. Reads the same `runtime/autopilot_state.json` the fleet writes.
pub fn stuck_sweeps(name: &str) -> u64 {
    let cfg = registry::autopilot_config();
    read_state(&cfg)
        .get("stuck")
        .and_then(|s| s.get(name))
        .and_then(|e| e.get("sweeps"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Clear a lane's stuck counter in the persisted autopilot state (round-tripping the file). The
/// watchdog calls this after a successful force-heal so the lane starts a fresh time-decay window
/// rather than immediately re-qualifying next sweep. No-op (Ok) when the entry is already absent.
pub fn reset_stuck_sweeps(name: &str) -> std::io::Result<()> {
    let cfg = registry::autopilot_config();
    let mut st = read_state(&cfg);
    let present = st
        .get("stuck")
        .and_then(|s| s.get(name))
        .is_some();
    if !present {
        return Ok(());
    }
    if let Some(m) = st.get_mut("stuck").and_then(Value::as_object_mut) {
        m.remove(name);
    }
    st["ts"] = json!(now());
    write_state(&st)
}

// --------------------------------------------------------------------------- //
// proof_required COOLDOWN — kill retry theater
// --------------------------------------------------------------------------- //
//
// A lane that fires `proof_required` (the inert non-AI arm) PROOF_COOLDOWN_THRESHOLD sweeps in a
// row arms this cooldown; `plan_jobs` then routes the lane to a BOUNDED autonomous diversification
// (a gated AI attempt at a FRESH self-generated spec), never a human park. The state lives in
// `autopilot_state.json` under `st["proof_cooldowns"][name] = {"until": <iso>, "armed_at": <iso>,
// "consecutive": N}` so it round-trips with the existing read_state/write_state. The pure helpers
// below are unit-tested. ROUTING NOTE (operator directive 2026-07-18): `plan_jobs` routes on entry
// PRESENCE (`proof_cooldown_entry(..).is_some()`), not on `until` still being in the future — an
// EXPIRED-but-present entry is "park expiry with no human wake and no real outcome", and it
// re-arms the autonomous re-spec path directly instead of an inert proof_required re-fire. The
// entry is cleared only by a real outcome (`bump_stuck_counter`'s move arm) or a human wake ack.
// `until` still matters to `bump_stuck_counter`'s one-shot re-arm bookkeeping and the dashboard.

/// Read a lane's proof_required cooldown entry from autopilot state. None when the lane is not
/// parked (entry absent) or the state is unreadable. Pure (no IO): the caller passes the state it
/// already read. The persisted-state callers use `read_state`; tests pass a fixture.
fn proof_cooldown_entry<'a>(st: &'a Value, name: &str) -> Option<&'a Value> {
    st.get("proof_cooldowns")
        .and_then(Value::as_object)
        .and_then(|m| m.get(name))
}

/// True iff the lane is within an ACTIVE proof_required cooldown (the `until` stamp is in the
/// future). An expired cooldown (until <= now) returns false — the caller may re-arm it on the
/// next threshold crossing. Pure: the caller passes `now_utc` so tests are deterministic.
fn proof_cooldown_active_at(st: &Value, name: &str, now_utc: DateTime<Utc>) -> bool {
    let Some(entry) = proof_cooldown_entry(st, name) else {
        return false;
    };
    let Some(until) = entry
        .get("until")
        .and_then(Value::as_str)
        .and_then(parse_ts)
    else {
        return false;
    };
    until > now_utc
}

/// True iff the lane is within an ACTIVE proof_required cooldown (the `until` stamp is in the
/// future). Production helper — reads the persisted autopilot state and uses wall-clock now. The
/// `plan_jobs` gate uses the pure `proof_cooldown_active_at` (it already holds the state); this
/// public helper is the query seam for the dashboard / `solomon state` CLI to surface a parked lane
/// (analogous to `stuck_sweeps`). Best-effort: an unreadable state returns false (no cooldown).
#[allow(dead_code)]
pub fn proof_cooldown_active(name: &str) -> bool {
    let cfg = registry::autopilot_config();
    proof_cooldown_active_at(&read_state(&cfg), name, Utc::now())
}

/// Arm or refresh a lane's proof_required cooldown in the IN-MEMORY state: set
/// `proof_cooldowns[name] = {"until": now+PROOF_COOLDOWN_S, "armed_at": now, "consecutive": N}`.
/// Called by `bump_stuck_counter` when a lane's consecutive proof_required count reaches
/// PROOF_COOLDOWN_THRESHOLD. Pure (no IO): mutates the passed-in state; `write_state` persists it.
fn arm_proof_cooldown(st: &mut Value, name: &str, consecutive: u64, now_utc: DateTime<Utc>) {
    if !st.get("proof_cooldowns").map(Value::is_object).unwrap_or(false) {
        st["proof_cooldowns"] = json!({});
    }
    let Some(m) = st.get_mut("proof_cooldowns").and_then(Value::as_object_mut) else {
        return;
    };
    let until = now_utc + ChronoDuration::seconds(PROOF_COOLDOWN_S);
    m.insert(
        name.to_string(),
        json!({
            "until": until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "armed_at": now_utc.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "consecutive": consecutive,
            "reason": "proof_required retry theater — pacing autonomous re-spec (no human park)",
        }),
    );
}

/// Clear a lane's proof_required cooldown in the IN-MEMORY state (the lane moved — produced a
/// non-proof_required outcome). Pure (no IO): mutates the passed-in state. No-op when absent.
/// Called by `bump_stuck_counter` on any non-proof_required outcome alongside clearing `stuck`.
fn clear_proof_cooldown(st: &mut Value, name: &str) {
    if let Some(m) = st.get_mut("proof_cooldowns").and_then(Value::as_object_mut) {
        m.remove(name);
    }
}

// --------------------------------------------------------------------------- //
// autonomous diversification budget — the bounded no-human-park re-spec counter
// --------------------------------------------------------------------------- //
//
// `st["diversify"][name] = {"date": <YYYY-MM-DD>, "count": N}` — the per-lane-per-day count of
// autonomous diversification (re-spec) implement iterations. Bounds the no-human-park re-spec to
// DIVERSIFY_DAILY_CAP gated AI attempts per lane per day; a new day resets the budget. Round-trips
// with read_state/write_state like `stuck`/`proof_cooldowns`.

/// Local calendar date (`YYYY-MM-DD`) — the per-day diversification budget key.
fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// A lane's diversification count for `today` from the passed state (pure). 0 when absent or the
/// stored date is not `today` (a new day resets the budget).
fn diversify_count(st: &Value, name: &str, today: &str) -> u64 {
    st.get("diversify")
        .and_then(|d| d.get(name))
        .filter(|e| e.get("date").and_then(Value::as_str) == Some(today))
        .and_then(|e| e.get("count").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// Charge ONE unit of the lane's per-day diversification budget — called by `once()` when a
/// diversify job ACTUALLY dispatches (the job_started event), never at plan time. plan_jobs only
/// READS the count to route; a planned re-spec that loses the single_agent dispatch slot must not
/// consume budget (2026-07-17 starvation: asmodeus+solomon read 3/3 spent on a day with zero
/// job_started events because plan_jobs bumped on every dispatch/display/state() plan).
fn charge_diversify_dispatch(st: &mut Value, name: &str) {
    bump_diversify_count(st, name, &today_local());
}

/// Increment (or start, resetting on a new day) the lane's diversification count for `today`. Bounds
/// autonomous re-spec spend to DIVERSIFY_DAILY_CAP per lane per day.
fn bump_diversify_count(st: &mut Value, name: &str, today: &str) {
    if !st.get("diversify").map(Value::is_object).unwrap_or(false) {
        st["diversify"] = json!({});
    }
    let next = diversify_count(st, name, today) + 1;
    if let Some(m) = st.get_mut("diversify").and_then(Value::as_object_mut) {
        m.insert(name.to_string(), json!({"date": today, "count": next}));
    }
}

// --------------------------------------------------------------------------- //
// SELF-SPEC generation + differentiation — the fresh bounded spec a parked lane dispatches
// --------------------------------------------------------------------------- //
//
// Operator directive 2026-07-18 (full autonomy): the root cause of the solomon lane's
// consecutive=28 park was NOT the anti-gaming rail (that rail correctly reverted runs that
// introduced skip/xfail markers) — it was that every autonomous re-spec dispatched the SAME
// standing goal, so each fresh `run-improver --once` process replayed the same spec family at
// escalation rung 0 (the improver's anti-thrash memory is in-process only and dies with each
// --once run) and the rail kept catching the same bad idea. The fix is fleet-level, PERSISTED
// spec differentiation: each self-respec dispatches a FRESH bounded goal text whose CATEGORY
// rotates away from families that keep failing, with the failed approaches explicitly excluded
// in the goal text. Generation is deterministic templates (no LLM at plan time — plan_jobs runs
// on every state() poll); the creative work stays inside the gated improver run, BOUNDED by the
// generated spec. The ledger lives in `st["self_specs"][name] = [{"at", "category", "goal",
// "outcome": "dispatched"|"failed"|"moved"}, ...]` (newest LAST, capped at
// SELF_SPEC_HISTORY_CAP) and round-trips with read_state/write_state like `stuck`/`diversify`.

/// The lane's self-spec ledger entries (oldest→newest). Empty when absent/malformed.
fn self_spec_history(st: &Value, name: &str) -> Vec<Value> {
    st.get("self_specs")
        .and_then(|m| m.get(name))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Entry accessor: the spec's category ("" when malformed).
fn spec_entry_category(e: &Value) -> &str {
    e.get("category").and_then(Value::as_str).unwrap_or("")
}

/// True iff the ledger entry did NOT produce a real outcome: explicitly "failed", or still
/// "dispatched" (an attempt that never resolved — a crashed run — is treated as failed so the
/// rotation never repeats it on faith).
fn spec_entry_failed(e: &Value) -> bool {
    matches!(
        e.get("outcome").and_then(Value::as_str),
        Some("failed") | Some("dispatched") | None
    )
}

/// Normalize a goal text for material-difference comparison: lowercase, whitespace collapsed.
fn normalize_spec(goal: &str) -> String {
    goal.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True iff `candidate` differs materially from every FAILED spec in the lane's ledger window
/// (normalized full-text inequality — the generator guarantees more by construction: a different
/// category or an exclusion clause naming the prior failed approach).
fn self_spec_differs_from_failed(candidate: &str, history: &[Value]) -> bool {
    let cand = normalize_spec(candidate);
    !history.iter().filter(|e| spec_entry_failed(e)).any(|e| {
        e.get("goal")
            .and_then(Value::as_str)
            .map(|g| normalize_spec(g) == cand)
            .unwrap_or(false)
    })
}

/// Pick the next self-spec CATEGORY from the ledger (pure). Rule set (operator directive #2):
///   * no attempts yet → the first category in [`SELF_SPEC_CATEGORIES`];
///   * the trailing consecutive FAILED attempts end in category C with fewer than
///     [`SELF_SPEC_FAMILY_FAIL_LIMIT`] same-category failures → stay on C (one in-family retry,
///     with the failed approach excluded in the goal text);
///   * C has [`SELF_SPEC_FAMILY_FAIL_LIMIT`]+ trailing consecutive failures → FORCE a different
///     category: the next one in rotation order after C.
///   * the newest attempt MOVED (real outcome) → restart the rotation at the first category.
fn pick_self_spec_category(history: &[Value]) -> &'static str {
    let Some(newest) = history.last() else {
        return SELF_SPEC_CATEGORIES[0];
    };
    if !spec_entry_failed(newest) {
        return SELF_SPEC_CATEGORIES[0];
    }
    let newest_cat = spec_entry_category(newest);
    let trailing_same_family = history
        .iter()
        .rev()
        .take_while(|e| spec_entry_failed(e) && spec_entry_category(e) == newest_cat)
        .count();
    let idx = SELF_SPEC_CATEGORIES
        .iter()
        .position(|c| *c == newest_cat)
        .unwrap_or(0);
    if trailing_same_family >= SELF_SPEC_FAMILY_FAIL_LIMIT {
        SELF_SPEC_CATEGORIES[(idx + 1) % SELF_SPEC_CATEGORIES.len()]
    } else {
        SELF_SPEC_CATEGORIES[idx]
    }
}

/// The bounded per-category spec body. Every body demands ONE small, complete, honestly-gated
/// increment and explicitly forbids the exact anti-gaming failure modes that got prior runs
/// reverted (skip/xfail markers, weakened gates) — the spec steers WITH the rails, never around
/// them.
fn self_spec_body(category: &str) -> &'static str {
    match category {
        "reliability" => {
            "Fix ONE concrete reliability defect you can reproduce or demonstrate from the code \
             (a real bug, race, silent error swallow, resource leak, or crash path) and add a \
             regression test that fails without the fix."
        }
        "tests" => {
            "Add ONE meaningful test (or small test group) covering REAL currently-untested \
             behavior of an important code path, asserting on actual observable outcomes."
        }
        "docs_hygiene" => {
            "Fix ONE docs/hygiene debt item: a doc that contradicts actual behavior, a dead or \
             misleading comment/README section, a stale config example, or a small lint/dead-code \
             cleanup — and make the docs match VERIFIED behavior."
        }
        _ => {
            "Implement ONE small, complete, user-visible improvement end-to-end (smallest useful \
             increment of the north-star goal), with a test proving the new behavior."
        }
    }
}

/// Generate the FRESH bounded self-spec for a parked lane (pure, deterministic — no LLM, no IO):
/// (category, goal_text). The category comes from [`pick_self_spec_category`] (rotating away from
/// spec families that failed [`SELF_SPEC_FAMILY_FAIL_LIMIT`]x); the goal text embeds the blocker
/// evidence, the standing north-star for context, an explicit DO-NOT-REPEAT exclusion of the
/// most recent failed attempts, and the anti-gaming ground rules. A belt-and-braces guard walks
/// the rotation until the text differs materially from every failed spec in the ledger window
/// (guaranteed to terminate: category alone changes the text).
fn generate_self_spec(st: &Value, name: &str, blocker: &str, standing_goal: &str) -> (&'static str, String) {
    let history = self_spec_history(st, name);
    let mut category = pick_self_spec_category(&history);
    for _ in 0..SELF_SPEC_CATEGORIES.len() {
        let goal = compose_self_spec_goal(category, blocker, standing_goal, &history);
        if self_spec_differs_from_failed(&goal, &history) {
            return (category, goal);
        }
        let idx = SELF_SPEC_CATEGORIES.iter().position(|c| *c == category).unwrap_or(0);
        category = SELF_SPEC_CATEGORIES[(idx + 1) % SELF_SPEC_CATEGORIES.len()];
    }
    // Unreachable in practice (4 distinct category bodies vs a cap-8 window of failures whose
    // exclusion lists differ); fall through with the last candidate anyway — dispatching a
    // repeat-risk spec still beats parking forever, and the RSI gates police the result.
    (category, compose_self_spec_goal(category, blocker, standing_goal, &history))
}

/// Render the self-spec goal text for `category` (see [`generate_self_spec`]).
fn compose_self_spec_goal(
    category: &str,
    blocker: &str,
    standing_goal: &str,
    history: &[Value],
) -> String {
    let exclusions: Vec<String> = history
        .iter()
        .rev()
        .filter(|e| spec_entry_failed(e))
        .take(3)
        .filter_map(|e| e.get("goal").and_then(Value::as_str))
        .map(|g| format!("- {}", g.chars().take(140).collect::<String>()))
        .collect();
    let exclusion_block = if exclusions.is_empty() {
        String::new()
    } else {
        format!(
            "\nDO NOT REPEAT these previously-failed self-spec directions (pick clearly \
             different work):\n{}\n",
            exclusions.join("\n")
        )
    };
    let north_star = if standing_goal.trim().is_empty() {
        String::new()
    } else {
        format!("\nNorth-star context (do not chase it directly this iteration; stay on the bounded spec): {standing_goal}\n")
    };
    format!(
        "SELF-SPEC (autonomous re-spec, category: {category}). {body}\n\
         Ground rules: ONE bounded increment; leave the working tree gate-green; NEVER add \
         skip/xfail/ignore markers, never weaken, disable, or game any test or gate — reverted \
         attempts did exactly that and were correctly rolled back.\n\
         Known lane blocker (context, not necessarily the thing to fix): {blocker}\n\
         {exclusion_block}{north_star}",
        body = self_spec_body(category),
    )
}

/// Record a DISPATCHED self-spec in the lane's ledger (called by `once()` at actual dispatch,
/// beside the diversify-budget charge — never at plan time). Caps the ledger at
/// [`SELF_SPEC_HISTORY_CAP`] (oldest dropped).
fn record_self_spec_dispatch(st: &mut Value, name: &str, category: &str, goal: &str) {
    if !st.get("self_specs").map(Value::is_object).unwrap_or(false) {
        st["self_specs"] = json!({});
    }
    let entry = json!({"at": now(), "category": category, "goal": goal, "outcome": "dispatched"});
    if let Some(m) = st.get_mut("self_specs").and_then(Value::as_object_mut) {
        let list = m.entry(name.to_string()).or_insert_with(|| json!([]));
        if let Some(arr) = list.as_array_mut() {
            arr.push(entry);
            while arr.len() > SELF_SPEC_HISTORY_CAP {
                arr.remove(0);
            }
        }
    }
}

/// Resolve the newest DISPATCHED self-spec to "failed" (inert outcome — incl. an anti-gaming
/// revert) or "moved" (real outcome). Called by `once()` after the run. No-op when the ledger has
/// no pending dispatch (defensive; never panics on malformed state).
fn resolve_self_spec_outcome(st: &mut Value, name: &str, failed: bool) {
    let verdict = if failed { "failed" } else { "moved" };
    if let Some(arr) = st
        .get_mut("self_specs")
        .and_then(|m| m.get_mut(name))
        .and_then(Value::as_array_mut)
    {
        if let Some(e) = arr
            .iter_mut()
            .rev()
            .find(|e| e.get("outcome").and_then(Value::as_str) == Some("dispatched"))
        {
            e["outcome"] = json!(verdict);
        }
    }
}

// --------------------------------------------------------------------------- //
// HUMAN WAKE override flag — the one-shot operator ack (an exit, not the only one)
// --------------------------------------------------------------------------- //
//
// `st["human_wake"][name] = {"at": <iso>}` — armed by `wake(Some(name))` (the operator's explicit
// per-lane ack), consumed by `once()` when the override implement job ACTUALLY dispatches (same
// charge-at-dispatch rule as the diversify budget). While pending, `plan_jobs` routes the lane's
// proof_required verdict to a NORMAL gated implement on the operator's STANDING repos.json goal —
// bypassing the autonomous self-respec (no diversify budget, no generated goal). Round-trips with
// read_state/write_state like `stuck`/`diversify`/`self_specs`.

/// True iff the operator armed a wake override for this lane that has not yet dispatched.
fn human_wake_pending(st: &Value, name: &str) -> bool {
    st.get("human_wake")
        .and_then(Value::as_object)
        .map(|m| m.contains_key(name))
        .unwrap_or(false)
}

/// Arm the one-shot wake override (called by `wake(Some(name))` — the human ack).
fn arm_human_wake(st: &mut Value, name: &str) {
    if !st.get("human_wake").map(Value::is_object).unwrap_or(false) {
        st["human_wake"] = json!({});
    }
    if let Some(m) = st.get_mut("human_wake").and_then(Value::as_object_mut) {
        m.insert(name.to_string(), json!({"at": now()}));
    }
}

/// Consume the wake override (called by `once()` when the override job actually dispatches).
fn consume_human_wake(st: &mut Value, name: &str) {
    if let Some(m) = st.get_mut("human_wake").and_then(Value::as_object_mut) {
        m.remove(name);
    }
}

// --------------------------------------------------------------------------- //
// needs_human_spec SURFACE-ONCE park — kill dispatch starvation
// --------------------------------------------------------------------------- //
//
// A `needs_human_spec` job is inert (files the need, mutates nothing) but planned at the lane's
// ops priority — a red lane's need outranks (10) every queued real job (30/70). Because `once()`
// picks `jobs.first()` whenever there are <= 1 AI jobs, the SAME need re-dispatched every sweep
// starved the queue indefinitely (audit 2026-07-16: asmodeus needs_human_spec re-ran every
// ~90-120s for hours while dotz's no-AI maintenance and the solomon implement job never ran).
// The fix: surface each need ONCE (the first dispatch files the proof + event — the operator
// notification), then PARK it — `plan_jobs` skips re-enqueueing the identical need on the
// dispatch path until the lane's state changes (the need fingerprint — its reason — differs),
// the lane moves (bump_stuck_counter clears the entry), or a human acks via an explicit per-lane
// wake (wake() clears the entry). The state lives in `st["nhs_surfaced"][name] =
// {"at": <iso>, "fingerprint": <job reason>}` and round-trips with read_state/write_state. The
// `state()` display path still plans the need every poll so the dashboard keeps showing WHY the
// lane is parked.

/// True iff this lane's `needs_human_spec` need was ALREADY surfaced with the SAME fingerprint
/// (the job's reason string — diagnose evidence for the structural route, the fixed park message
/// for the cooldown route). A different fingerprint means the lane's state changed: re-surface
/// once. Pure (no IO): the caller passes the state it already read.
fn nhs_already_surfaced(st: &Value, name: &str, fingerprint: &str) -> bool {
    st.get("nhs_surfaced")
        .and_then(Value::as_object)
        .and_then(|m| m.get(name))
        .and_then(|e| e.get("fingerprint"))
        .and_then(Value::as_str)
        == Some(fingerprint)
}

/// Record that a lane's `needs_human_spec` need was surfaced (its job dispatched and filed the
/// proof/need). Called by `once()` after the dispatch. Pure (no IO): mutates the passed-in state.
fn mark_nhs_surfaced(st: &mut Value, name: &str, fingerprint: &str) {
    if !st.get("nhs_surfaced").map(Value::is_object).unwrap_or(false) {
        st["nhs_surfaced"] = json!({});
    }
    if let Some(m) = st.get_mut("nhs_surfaced").and_then(Value::as_object_mut) {
        m.insert(
            name.to_string(),
            json!({"at": now(), "fingerprint": fingerprint}),
        );
    }
}

/// Un-park a lane's surfaced `needs_human_spec` need so the next occurrence re-surfaces once.
/// Called when the lane moves (`bump_stuck_counter`'s clear arm) and on an explicit per-lane
/// `wake()` (the human ack). Pure (no IO). No-op when absent.
fn clear_nhs_surfaced(st: &mut Value, name: &str) {
    if let Some(m) = st.get_mut("nhs_surfaced").and_then(Value::as_object_mut) {
        m.remove(name);
    }
}

fn append_event(event: &Value) {
    let mut obj = match event {
        Value::Object(o) => o.clone(),
        _ => Map::new(),
    };
    obj.insert("ts".to_string(), json!(now()));
    let p = events_path();
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&Value::Object(obj)).unwrap_or_default()
        )?;
        Ok(())
    })();
}

fn read_ops_payload() -> Value {
    std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"projects": {}}))
}

fn state_path() -> PathBuf {
    paths::here().join("runtime").join(STATE_FILE)
}

fn legacy_state_path() -> PathBuf {
    paths::here().join("runtime").join(LEGACY_STATE_FILE)
}

fn events_path() -> PathBuf {
    paths::here().join("runtime").join(EVENTS_FILE)
}

fn public_config(cfg: &Value) -> Value {
    json!({
        "mode": cfg.get("mode").cloned().unwrap_or(json!("single_agent")),
        "mission": cfg.get("mission").cloned().unwrap_or(json!(registry::AUTOPILOT_DEFAULT_MISSION)),
        "provider": cfg.get("provider").cloned().unwrap_or(json!("ollama-cloud")),
        "api_key": cfg.get("api_key").cloned().unwrap_or(json!("OLLAMA_API_KEY")),
        "model": cfg.get("model").cloned().unwrap_or(json!("glm-5.2")),
        "max_concurrent_agent_calls": cfg.get("max_concurrent_agent_calls").cloned().unwrap_or(json!(1)),
        "cooldown_s": cfg.get("cooldown_s").cloned().unwrap_or(json!(300)),
        "daily_call_budget": cfg.get("daily_call_budget").cloned().unwrap_or(json!(40)),
        "adaptive_phase_policy": cfg.get("adaptive_phase_policy").cloned().unwrap_or(json!("cheap_by_default_deep_on_red_noop_critical_or_campaign")),
        "targets": cfg.get("targets").cloned().unwrap_or(json!(["sover", "dotz", "asmodeus", "maki", "solomon"])),
    })
}

fn job_value(j: &Job) -> Value {
    json!({
        "repo": j.name,
        "job": j.kind,
        "state": j.state,
        "priority": j.priority,
        "requires_ai": j.requires_ai,
        "reason": j.reason,
        "next_action": j.next_action,
        "spec_category": j.spec_category.clone().map(Value::from).unwrap_or(Value::Null),
        "spec_goal": j.spec_goal.clone().map(Value::from).unwrap_or(Value::Null),
    })
}

fn cooldown_until(st: &Value) -> Option<DateTime<Utc>> {
    st.get("cooldown")
        .and_then(|c| c.get("until"))
        .and_then(Value::as_str)
        .and_then(parse_ts)
}

fn autopilot_paused(st: &Value) -> bool {
    st.get("paused").and_then(Value::as_bool) == Some(true)
}

fn cooldown_value(st: &Value) -> Value {
    st.get("cooldown").cloned().unwrap_or(Value::Null)
}

fn stale_non_ai_provider_cooldown(st: &Value) -> bool {
    st.get("cooldown")
        .and_then(|c| c.get("reason"))
        .and_then(Value::as_str)
        == Some("provider quota/rate limit")
        && st
            .get("last_result")
            .and_then(|r| r.get("requires_ai"))
            .and_then(Value::as_bool)
            == Some(false)
}

/// QUOTA cooldown for the autopilot (catalog #2, ported from the improver lanes): the cooldown
/// window is the SHARED provider-budget ledger's per-endpoint exponential park
/// (min(900*2^(n-1), 21600)s), never `cfg.cooldown_s` — the condemned 86400s blanket meant one
/// quota error put all 5 autopilot targets to sleep for a day. The improver subprocess the
/// autopilot spawns shares the same ledger (Solomon/runtime/_provider_budget.json), so when it
/// already recorded the 429 this reuses that park stamp instead of double-bumping the backoff.
/// The reason string stays byte-identical: stale_non_ai_provider_cooldown keys on it.
fn set_quota_cooldown(st: &mut Value, cfg: &Value) {
    set_quota_cooldown_at(st, cfg, &paths::here().join("runtime"))
}

fn set_quota_cooldown_at(st: &mut Value, cfg: &Value, fleet_dir: &Path) {
    let provider = cfg
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("ollama-cloud")
        .to_string();
    let model = cfg
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.2")
        .to_string();
    let until_unix = crate::improver::budget::quota_park_until_at(fleet_dir, &provider, &model);
    let until = DateTime::<Utc>::from_timestamp(until_unix as i64, 0)
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    st["cooldown"] = json!({
        "since": now(),
        "until": until,
        "reason": "provider quota/rate limit",
        "provider": provider.clone(),
        "endpoint": format!("{provider}:{model}"),
        "per_endpoint": true,
    });
}

/// Blanket cooldown — DAILY-BUDGET use only (the window until the self-imposed call budget
/// resets). Quota errors must go through [`set_quota_cooldown`]; never route a provider 429 here.
fn set_cooldown(st: &mut Value, cfg: &Value, reason: &str) {
    let seconds = cfg
        .get("cooldown_s")
        .and_then(Value::as_i64)
        .unwrap_or(86_400)
        .max(60);
    let until = Utc::now() + ChronoDuration::seconds(seconds);
    st["cooldown"] = json!({
        "since": now(),
        "until": until.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "reason": reason,
        "provider": cfg.get("provider").cloned().unwrap_or(json!("ollama-cloud")),
    });
}

fn daily_budget(cfg: &Value) -> i64 {
    cfg.get("daily_call_budget")
        .and_then(Value::as_i64)
        .unwrap_or(40)
        .max(1)
}

fn daily_used(st: &Value) -> i64 {
    if st
        .get("daily")
        .and_then(|d| d.get("date"))
        .and_then(Value::as_str)
        == Some(today().as_str())
    {
        st.get("daily")
            .and_then(|d| d.get("calls"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    } else {
        0
    }
}

fn increment_daily(st: &mut Value) {
    let calls = daily_used(st) + 1;
    st["daily"] = json!({"date": today(), "calls": calls});
}

/// Daily-budget headroom for the CEO plane's composers (outreach / self-tooling): how many agent
/// calls remain under `daily_call_budget` today, clamped at zero. Reads the SAME autopilot state
/// ledger `plan_jobs`/`increment_daily` maintain, so a CEO compose can never spend past the fleet's
/// shared quota and never needs a second ledger. Purely additive — no state write, no behavior
/// change to the fleet loop; a caller seeing 0 simply skips composing this sweep and retries once
/// the date rolls the counter.
pub fn daily_calls_remaining() -> i64 {
    let cfg = registry::autopilot_config();
    calls_remaining(&cfg, &read_state(&cfg))
}

/// Pure core of [`daily_calls_remaining`] (unit-tested without disk): budget minus today's used
/// calls, never negative. A stale `daily.date` reads as zero used (the date reset `daily_used`
/// already enforces), so the remaining budget resets with the day.
pub(crate) fn calls_remaining(cfg: &Value, st: &Value) -> i64 {
    (daily_budget(cfg) - daily_used(st)).max(0)
}

fn max_concurrent(cfg: &Value) -> i64 {
    cfg.get("max_concurrent_agent_calls")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .clamp(1, 2)
}

fn cfg_targets(cfg: &Value) -> Vec<String> {
    cfg.get("targets")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|v| !v.is_empty())
        .unwrap_or_else(registry::autopilot_targets)
}

fn provider_key_ref(provider: &str) -> &'static str {
    match provider {
        "ollama-cloud" => "OLLAMA_API_KEY",
        _ => "OPENROUTER_API_KEY",
    }
}

fn provider_matches_config(pi_provider: &str, cfg_provider: &str) -> bool {
    match cfg_provider {
        "ollama-cloud" => pi_provider == "maki-cloud" || pi_provider == "ollama-cloud",
        "openrouter" => pi_provider == "openrouter",
        _ => false,
    }
}

fn quota_heartbeat_matches_config(repo: &Value, cfg: &Value) -> bool {
    let hb = match heartbeat::read_heartbeat(repo) {
        Some(h) => h,
        None => return false,
    };
    let cooldown_s = cfg
        .get("cooldown_s")
        .and_then(Value::as_i64)
        .unwrap_or(300)
        .max(0) as f64;
    match heartbeat::heartbeat_age(&hb) {
        Some(age) if age <= cooldown_s => {}
        _ => return false,
    }
    let cfg_provider = cfg
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("ollama-cloud");
    let pi_provider = hb
        .get("pi")
        .and_then(|p| p.get("provider"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if !provider_matches_config(pi_provider, cfg_provider) {
        return false;
    }
    let cfg_model = cfg
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("glm-5.2");
    let hb_model = hb.get("model").and_then(Value::as_str).unwrap_or("");
    let pi_model = hb
        .get("pi")
        .and_then(|p| p.get("model"))
        .and_then(Value::as_str)
        .unwrap_or("");
    hb_model == cfg_model || pi_model == cfg_model
}

fn resolve_key(key_ref: &str) -> Option<String> {
    if key_ref.starts_with("sk-or-v1-") {
        return Some(key_ref.to_string());
    }
    std::env::var(key_ref)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| notify::env_value(key_ref))
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|n| n.and_utc())
}

fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn today() -> String {
    Utc::now().format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo(name: &str) -> Value {
        json!({"name": name, "path": format!("C:/p/{name}")})
    }

    // Stuck-counter round-trip: a `proof_required` outcome (inert fleet action) increments the
    // per-lane counter; ANY other outcome clears the entry. This is the time-decay the watchdog's
    // autopilot recover pass gates on (via stuck_sweeps) to escape the permanent-stuck trap.
    #[test]
    fn bump_stuck_counter_increments_on_proof_required_and_clears_otherwise() {
        let mut st = json!({});
        bump_stuck_counter(&mut st, "dotz", true);
        bump_stuck_counter(&mut st, "dotz", true);
        bump_stuck_counter(&mut st, "dotz", true);
        assert_eq!(
            st["stuck"]["dotz"]["sweeps"], 3,
            "three consecutive proof_required sweeps → count 3: {st}"
        );
        // A different lane tracks independently.
        bump_stuck_counter(&mut st, "sover", true);
        assert_eq!(st["stuck"]["sover"]["sweeps"], 1);
        assert_eq!(st["stuck"]["dotz"]["sweeps"], 3);
        // A non-proof_required outcome (shipped/blocked/complete/cooldown) clears the lane.
        bump_stuck_counter(&mut st, "dotz", false);
        assert!(
            st["stuck"].get("dotz").is_none(),
            "a non-proof_required outcome must clear the stuck entry: {st}"
        );
        // Clearing an absent lane is a harmless no-op.
        bump_stuck_counter(&mut st, "never_seen", false);
        assert!(st["stuck"].get("never_seen").is_none());
    }

    #[test]
    fn plan_jobs_prioritizes_ops_red_before_stale_proof() {
        let a = format!("autopilot_red_{}", std::process::id());
        let b = format!("autopilot_green_{}", std::process::id());
        let repos = vec![repo(&a), repo(&b)];
        let cfg = json!({"provider": "openrouter", "targets": [a.clone(), b.clone()]});
        let mut projects = Map::new();
        projects.insert(b.clone(), json!({"status": "green"}));
        projects.insert(
            a.clone(),
            json!({"status": "red", "reasons": ["publish_recency=red"]}),
        );
        let ops = json!({"projects": Value::Object(projects)});
        let mut st = json!({"manual_queue": []});
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, false);
        assert_eq!(jobs[0].name, a);
        assert_eq!(jobs[0].kind, "implement");
        assert!(jobs[0].requires_ai);
    }

    #[test]
    fn plan_jobs_holds_unchanged_red_while_proof_is_fresh() {
        let name = format!(
            "autopilot_fresh_red_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let row = repo(&name);
        let rt = paths::runtime_dir(&row).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        let unchanged = json!({
            "status": "red",
            "probes": {"cdp_alive": "red", "process": "green"}
        });
        let fingerprint = retry_state_fingerprint(&unchanged, "ok");
        write_proof_value_at(
            &rt,
            &json!({
                "ts": now(),
                "repo": name.clone(),
                "outcome": "blocked",
                "state_fingerprint": fingerprint
            }),
        );
        let cfg = json!({"provider": "openrouter", "targets": [name.clone()]});
        let mut projects = Map::new();
        projects.insert(name.clone(), unchanged.clone());
        let ops = json!({"projects": Value::Object(projects)});
        let jobs = plan_jobs(
            std::slice::from_ref(&row),
            &cfg,
            &ops,
            &mut json!({"manual_queue": []}),
            None,
            false,
        );
        assert!(
            jobs.is_empty(),
            "a fresh proof must own the retry window even while ops remain red: {jobs:?}"
        );

        let changed = json!({
            "status": "red",
            "probes": {"cdp_alive": "green", "post_failures": "red", "process": "green"}
        });
        let mut changed_projects = Map::new();
        changed_projects.insert(name.clone(), changed);
        let changed_jobs = plan_jobs(
            std::slice::from_ref(&row),
            &cfg,
            &json!({"projects": Value::Object(changed_projects)}),
            &mut json!({"manual_queue": []}),
            None,
            false,
        );
        assert_eq!(changed_jobs.len(), 1, "a new RED fingerprint bypasses the old proof");
        assert_eq!(changed_jobs[0].kind, "implement");
        assert!(
            !proof_matches_fresh_state(
                &name,
                &retry_state_fingerprint(&unchanged, "needs_goal")
            ),
            "a changed diagnosis category must bypass the old proof"
        );
        let _ = std::fs::remove_dir_all(rt);
    }

    #[test]
    fn run_ai_job_blocks_a_missing_repo_before_key_or_budget_spend() {
        let name = format!(
            "autopilot_missing_repo_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let missing = std::env::temp_dir().join(&name);
        let _ = std::fs::remove_dir_all(&missing);
        let row = json!({"name": name, "path": missing});
        let job = Job {
            name: name.clone(),
            kind: "implement".into(),
            state: "queued".into(),
            priority: 10,
            requires_ai: true,
            reason: "test".into(),
            next_action: "test".into(),
            diversify: false,
            spec_goal: None,
            spec_category: None,
            wake_override: false,
            state_fingerprint: "missing_repo".into(),
        };
        let mut st = json!({});
        let result = run_ai_job(
            &job,
            &[row],
            &json!({"provider": "openrouter", "api_key": "UNSET_TEST_KEY"}),
            false,
            &mut st,
        );
        assert_eq!(result["outcome"], json!("blocked"));
        assert!(result["summary"]
            .as_str()
            .unwrap_or("")
            .contains("configured repo path does not exist"));
        assert_eq!(daily_used(&st), 0, "preflight failure must not spend the daily AI budget");
        if let Some(rt) = paths::runtime_dir(&json!({"name": name})) {
            let _ = std::fs::remove_dir_all(rt);
        }
    }

    #[test]
    fn plan_jobs_turns_quota_heartbeat_into_cooldown_job() {
        let name = format!("autopilot_quota_{}", std::process::id());
        let repo = json!({"name": name, "path": "C:/p/q"});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "sleeping",
                "phase": "quota_error",
                "reason": "quota_error",
                "last_summary": "429 Rate limit exceeded",
                "updated_at": now(),
                "model": "m",
                "pi": {"provider": "openrouter", "model": "m"}
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg = json!({"provider": "openrouter", "model": "m", "targets": [name.clone()]});
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut json!({}),
            None,
            false,
        );
        assert_eq!(jobs[0].kind, "cooldown");
        assert!(!jobs[0].requires_ai);
        let _ = std::fs::remove_dir_all(rt);
    }

    #[test]
    fn plan_jobs_reruns_stale_quota_heartbeat_under_current_provider() {
        let name = format!("autopilot_stale_quota_{}", std::process::id());
        let repo = json!({"name": name, "path": "C:/p/q"});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "sleeping",
                "phase": "quota_error",
                "reason": "quota_error",
                "last_summary": "old weekly usage limit",
                "updated_at": "1999-01-01T00:00:00Z",
                "model": "glm-5.2",
                "pi": {"provider": "maki-cloud", "model": "glm-5.2"}
            }))
            .unwrap(),
        )
        .unwrap();
        let cfg =
            json!({"provider": "ollama-cloud", "model": "glm-5.2", "targets": [name.clone()]});
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {"autopilot_stale": {"status": "green"}}}),
            &mut json!({}),
            None,
            false,
        );
        assert_eq!(jobs[0].kind, "implement");
        assert!(jobs[0].requires_ai);
        assert!(jobs[0].reason.contains("stale quota"));
        let _ = std::fs::remove_dir_all(rt);
    }

    // ---- deadlock regression: a permanently-noop non-AI proof_required job at the same
    // priority as runnable AI improver jobs must NOT permanently head the queue. Before the
    // fleet.rs sort tiebreak flip (requires_ai: true before false at equal priority), a stuck
    // priority-10 non-AI `proof_required` job (e.g. dotz, from needs_goal + auto_safe!=true)
    // was `jobs.first()` every sweep and `fleet::once` spun on it forever, starving the
    // priority-10 AI improvers (maki/sover/asmodeus/solomon). This proves the scheduler now
    // SELECTS a runnable AI job instead of the no-op head. ----
    #[test]
    fn plan_jobs_selects_ai_job_over_permanent_noop_proof_required() {
        // The proof_required repo is named to sort ALPHABETICALLY FIRST, so only the
        // requires_ai tiebreak (the fix) — not the name tiebreak — can move the AI job
        // ahead of it. If the fix regresses, jobs[0] falls back to this no-op head.
        let noop = format!("aaa_noop_proof_{}", std::process::id());
        let ai = format!("zzz_ai_improver_{}", std::process::id());
        let noop_repo = json!({"name": noop, "path": format!("C:/p/{noop}")});
        let ai_repo = json!({"name": ai, "path": format!("C:/p/{ai}")});

        // Drive the no-op repo into a non-AI proof_required job via diagnose():
        // status=error + reason=needs_goal => category "needs_goal", auto_safe=false =>
        // fleet plan_jobs proof_required branch (requires_ai=false).
        let noop_rt = paths::runtime_dir(&noop_repo).unwrap();
        let _ = std::fs::create_dir_all(&noop_rt);
        std::fs::write(
            noop_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "reason": "needs_goal",
                "last_summary": "no north-star GOAL and no actionable backlog",
                "updated_at": now(),
            }))
            .unwrap(),
        )
        .unwrap();

        // The AI repo has a clean/idle heartbeat => diagnose category "ok"; ops RED then
        // makes it an `implement`/requires_ai=true job. Both jobs land at priority 10 (ops RED),
        // so the ONLY thing separating them is the requires_ai tiebreak under test.
        let ai_rt = paths::runtime_dir(&ai_repo).unwrap();
        let _ = std::fs::create_dir_all(&ai_rt);
        std::fs::write(
            ai_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "idle",
                "updated_at": now(),
            }))
            .unwrap(),
        )
        .unwrap();

        let cfg = json!({"provider": "openrouter", "targets": [noop.clone(), ai.clone()]});
        let mut projects = Map::new();
        // Both RED => both priority 10, forcing the tie the deadlock lived in.
        projects.insert(noop.clone(), json!({"status": "red", "reasons": ["noop_streak=red"]}));
        projects.insert(ai.clone(), json!({"status": "red", "reasons": ["publish_recency=red"]}));
        let ops = json!({"projects": Value::Object(projects)});
        let mut st = json!({"manual_queue": []});

        let jobs = plan_jobs(&[noop_repo, ai_repo], &cfg, &ops, &mut st, None, false);

        // Precondition sanity: both jobs planned, both at priority 10, and the no-op really is
        // a non-AI proof_required job (the absorbing head the deadlock spun on).
        let noop_job = jobs.iter().find(|j| j.name == noop).expect("noop job planned");
        let ai_job = jobs.iter().find(|j| j.name == ai).expect("ai job planned");
        assert_eq!(noop_job.kind, "proof_required");
        assert!(!noop_job.requires_ai, "noop must be the non-AI proof_required head");
        assert_eq!(noop_job.priority, 10, "ops-RED noop must be priority 10");
        assert!(ai_job.requires_ai, "ai job must be a runnable AI improver");
        assert_eq!(ai_job.priority, 10, "ops-RED ai job must be priority 10");

        // THE FIX: at equal priority the scheduler picks the runnable AI improver, NOT the
        // permanently-noop proof_required job — even though the no-op sorts first by name.
        assert_eq!(
            jobs[0].name, ai,
            "scheduler must select the runnable AI job, not spin on the no-op proof_required head"
        );
        assert!(jobs[0].requires_ai, "jobs.first() must be an AI improver job");

        let _ = std::fs::remove_dir_all(noop_rt);
        let _ = std::fs::remove_dir_all(ai_rt);
    }

    // ---- quota cooldown: per-endpoint exponential park, never the 86400s blanket ----
    #[test]
    fn quota_cooldown_is_per_endpoint_park_not_a_day_blanket() {
        let dir = std::env::temp_dir().join(format!(
            "solomon_fleet_quota_cd_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // cfg carries the condemned 86400s blanket — the quota path must IGNORE it.
        let cfg = json!({"provider": "ollama-cloud", "model": "glm-5.2", "cooldown_s": 86_400});
        let mut st = json!({});
        set_quota_cooldown_at(&mut st, &cfg, &dir);
        let until = cooldown_until(&st).expect("cooldown until parses");
        let secs = (until - Utc::now()).num_seconds();
        assert!(
            (800..=21_700).contains(&secs),
            "first 429 must park ~900s (exponential, 6h cap), got {secs}s"
        );
        assert_eq!(st["cooldown"]["reason"], json!("provider quota/rate limit"));
        assert_eq!(st["cooldown"]["endpoint"], json!("ollama-cloud:glm-5.2"));
        assert_eq!(st["cooldown"]["per_endpoint"], json!(true));
        // second sighting of the SAME live park (e.g. the improver already recorded the 429):
        // the stamp is reused, not double-bumped.
        let mut st2 = json!({});
        set_quota_cooldown_at(&mut st2, &cfg, &dir);
        assert_eq!(st2["cooldown"]["until"], st["cooldown"]["until"]);
        // (per-endpoint isolation — parking A leaves B Proceed — is asserted by budget.rs's
        // parking_endpoint_a_leaves_endpoint_b_proceed on the same ledger primitive.)
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn daily_budget_resets_by_date() {
        let st = json!({"daily": {"date": "1999-01-01", "calls": 99}});
        assert_eq!(daily_used(&st), 0);
        let mut st2 = json!({});
        increment_daily(&mut st2);
        assert_eq!(daily_used(&st2), 1);
    }

    // The CEO-plane budget preflight primitive: remaining = budget - used, clamped >= 0, and a
    // stale daily date resets by day — so an outreach/tooling compose can never spend past the
    // fleet's shared `daily_call_budget` and never double-books a second ledger.
    #[test]
    fn daily_calls_remaining_is_budget_minus_used_clamped_and_date_reset() {
        let cfg = json!({"daily_call_budget": 10});
        // no state yet -> the full budget remains
        assert_eq!(calls_remaining(&cfg, &json!({})), 10);
        // today's burn counts down
        let st = json!({"daily": {"date": today(), "calls": 4}});
        assert_eq!(calls_remaining(&cfg, &st), 6);
        // at cap -> exactly 0; past cap -> still 0 (never negative)
        let st = json!({"daily": {"date": today(), "calls": 10}});
        assert_eq!(calls_remaining(&cfg, &st), 0);
        let st = json!({"daily": {"date": today(), "calls": 99}});
        assert_eq!(calls_remaining(&cfg, &st), 0);
        // yesterday's burn does NOT consume today's budget (resets by date, like daily_used)
        let st = json!({"daily": {"date": "1999-01-01", "calls": 99}});
        assert_eq!(calls_remaining(&cfg, &st), 10);
        // absent budget -> the conservative daily_budget default (40)
        assert_eq!(calls_remaining(&json!({}), &json!({})), 40);
    }

    #[test]
    fn public_config_never_exposes_resolved_secret() {
        let cfg = json!({"api_key": "OPENROUTER_API_KEY", "model": "m"});
        let pubc = public_config(&cfg);
        assert_eq!(pubc["api_key"], json!("OPENROUTER_API_KEY"));
        assert_eq!(pubc["model"], json!("m"));
        assert_eq!(pubc["mode"], json!("single_agent"));
        assert!(pubc["mission"]
            .as_str()
            .unwrap_or("")
            .contains("Autonomously improve"));
    }

    #[test]
    fn autopilot_provider_key_ref_matches_provider() {
        assert_eq!(provider_key_ref("ollama-cloud"), "OLLAMA_API_KEY");
        assert_eq!(provider_key_ref("openrouter"), "OPENROUTER_API_KEY");
        assert_eq!(provider_key_ref("unknown"), "OPENROUTER_API_KEY");
    }

    #[test]
    fn stale_non_ai_provider_cooldown_detects_bad_heartbeat_cooldown() {
        assert!(stale_non_ai_provider_cooldown(&json!({
            "cooldown": {"reason": "provider quota/rate limit"},
            "last_result": {"requires_ai": false}
        })));
        assert!(!stale_non_ai_provider_cooldown(&json!({
            "cooldown": {"reason": "provider quota/rate limit"},
            "last_result": {"requires_ai": true}
        })));
        assert!(!stale_non_ai_provider_cooldown(&json!({
            "cooldown": {"reason": "daily call budget reached"},
            "last_result": {"requires_ai": false}
        })));
    }

    #[test]
    fn collect_pipe_returns_when_reader_does_not_finish() {
        let (_tx, rx) = std::sync::mpsc::channel::<String>();
        let start = std::time::Instant::now();
        assert_eq!(collect_pipe(rx), "");
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn autopilot_lock_live_checks_pid_truth() {
        let dir = std::env::temp_dir().join(format!(
            "solomon_autopilot_live_lock_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autopilot.lock");

        assert!(!autopilot_lock_live_at(&path));
        std::fs::write(&path, "2147483646").unwrap();
        assert!(!autopilot_lock_live_at(&path));
        std::fs::write(&path, std::process::id().to_string()).unwrap();
        assert!(autopilot_lock_live_at(&path));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_proof_falls_back_to_legacy_fleet_file() {
        let name = format!("autopilot_legacy_proof_{}", std::process::id());
        let repo = json!({"name": name});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join(LEGACY_PROOF_FILE),
            serde_json::to_vec(&json!({"repo": name, "outcome": "blocked"})).unwrap(),
        )
        .unwrap();
        let proof = read_proof(repo["name"].as_str().unwrap()).unwrap();
        assert_eq!(proof["outcome"], json!("blocked"));
        let _ = std::fs::remove_dir_all(rt);
    }

    #[test]
    fn read_proof_prefers_fresher_history_over_stale_file() {
        let name = format!("autopilot_history_proof_{}", std::process::id());
        let repo = json!({"name": name});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join(PROOF_FILE),
            serde_json::to_vec(&json!({
                "ts": "1999-01-01T00:00:00Z",
                "repo": name,
                "outcome": "cooldown"
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            rt.join("history.jsonl"),
            format!(
                "{}\n",
                serde_json::to_string(&json!({
                    "ts": "2026-07-06T02:12:35Z",
                    "status": "blocked",
                    "summary": "judge timed out; kept local"
                }))
                .unwrap()
            ),
        )
        .unwrap();
        let proof = read_proof(&name).unwrap();
        assert_eq!(proof["outcome"], json!("blocked"));
        assert_eq!(proof["summary"], json!("judge timed out; kept local"));
        let _ = std::fs::remove_dir_all(rt);
    }

    #[test]
    fn paused_state_prevents_new_work_without_erasing_queue() {
        let cfg = json!({"mode": "single_agent", "targets": ["x"]});
        let mut st = json!({"paused": true, "queue": [{"repo": "x"}], "manual_queue": ["x"]});
        st["mode"] = json!("single_agent");
        st["config"] = public_config(&cfg);
        assert!(autopilot_paused(&st));
        assert_eq!(st["queue"][0]["repo"], json!("x"));
        assert_eq!(st["manual_queue"][0], json!("x"));
    }

    #[test]
    fn acquire_lock_allows_only_one_active_autopilot_agent() {
        let dir =
            std::env::temp_dir().join(format!("solomon_autopilot_lock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autopilot.lock");

        let first = acquire_lock_at(path.clone()).unwrap();
        assert!(first.is_some());
        let second = acquire_lock_at(path.clone()).unwrap();
        assert!(second.is_none());
        drop(first);
        let third = acquire_lock_at(path).unwrap();
        assert!(third.is_some());
        drop(third);

        let _ = std::fs::remove_dir_all(dir);
    }

    // ===================================================================== #
    // proof_required COOLDOWN — kill retry theater
    // ===================================================================== #
    //
    // After PROOF_COOLDOWN_THRESHOLD (3) consecutive inert proof_required sweeps, a lane's park
    // entry arms for PROOF_COOLDOWN_S (4h) and the scheduler routes the lane to the BOUNDED
    // autonomous self-respec instead of dispatching another inert proof_required job. The entry
    // clears when the lane produces a real (non proof_required / non needs_human_spec) outcome or
    // an operator wake ack.

    #[test]
    fn proof_cooldown_active_reads_until_stamp() {
        // No entry -> not active.
        let st = json!({});
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()));
        // until in the future -> active.
        let future = Utc::now() + ChronoDuration::seconds(3600);
        let st = json!({
            "proof_cooldowns": {
                "dotz": {"until": future.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "consecutive": 3}
            }
        });
        assert!(proof_cooldown_active_at(&st, "dotz", Utc::now()));
        // until in the past -> expired (not active) — the lane may re-fire proof_required.
        let past = Utc::now() - ChronoDuration::seconds(3600);
        let st = json!({
            "proof_cooldowns": {
                "dotz": {"until": past.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "consecutive": 3}
            }
        });
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()));
        // a different lane -> not active for the asked lane.
        let st = json!({
            "proof_cooldowns": {
                "sover": {"until": future.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "consecutive": 3}
            }
        });
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()));
        // malformed until (not a timestamp) -> not active.
        let st = json!({"proof_cooldowns": {"dotz": {"until": "not-a-ts"}}});
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()));
    }

    #[test]
    fn arm_proof_cooldown_sets_until_now_plus_window() {
        let mut st = json!({});
        let now = Utc::now();
        arm_proof_cooldown(&mut st, "dotz", 3, now);
        let until = proof_cooldown_entry(&st, "dotz")
            .and_then(|e| e.get("until"))
            .and_then(Value::as_str)
            .and_then(parse_ts)
            .expect("until stamp parses");
        let secs = (until - now).num_seconds();
        assert!(
            (PROOF_COOLDOWN_S - 5..=PROOF_COOLDOWN_S + 5).contains(&secs),
            "until is now+PROOF_COOLDOWN_S (±5s rounding), got {secs}s"
        );
        assert_eq!(st["proof_cooldowns"]["dotz"]["consecutive"], json!(3));
        assert_eq!(
            st["proof_cooldowns"]["dotz"]["reason"],
            json!("proof_required retry theater — pacing autonomous re-spec (no human park)")
        );
    }

    #[test]
    fn clear_proof_cooldown_removes_entry() {
        let mut st = json!({"proof_cooldowns": {"dotz": {"until": "2099-01-01T00:00:00Z"}}});
        clear_proof_cooldown(&mut st, "dotz");
        assert!(st.get("proof_cooldowns").unwrap().get("dotz").is_none());
        // clearing an absent lane is a no-op.
        clear_proof_cooldown(&mut st, "never_parked");
    }

    // The core retry-theater kill: 3 consecutive proof_required sweeps arm the cooldown (which then
    // paces the bounded autonomous diversification in plan_jobs); the streak persists while parked.
    #[test]
    fn bump_stuck_counter_arms_cooldown_at_threshold_and_persists_streak() {
        let mut st = json!({});
        // 3 consecutive proof_required sweeps — the 3rd arms the cooldown.
        bump_stuck_counter(&mut st, "dotz", true);
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()), "1st: not parked yet");
        bump_stuck_counter(&mut st, "dotz", true);
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()), "2nd: not parked yet");
        bump_stuck_counter(&mut st, "dotz", true);
        assert!(
            proof_cooldown_active_at(&st, "dotz", Utc::now()),
            "3rd: parked for the cooldown window (retry theater killed)"
        );
        assert_eq!(st["stuck"]["dotz"]["sweeps"], 3);

        // A further inert proof_required (backoff) sweep keeps the lane STUCK — the streak persists
        // and the cooldown stays armed (not cleared) so the diversification cadence holds.
        bump_stuck_counter(&mut st, "dotz", true); // another inert proof_required → still stuck
        assert!(
            proof_cooldown_active_at(&st, "dotz", Utc::now()),
            "parked lane stays parked across inert proof_required sweeps (streak persists)"
        );
        assert_eq!(st["stuck"]["dotz"]["sweeps"], 4);

        // The lane eventually MOVES (shipped/blocked/complete) — stuck + cooldown both clear.
        bump_stuck_counter(&mut st, "dotz", false);
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()), "cleared on a real move");
        assert!(st.get("stuck").unwrap().get("dotz").is_none());
    }

    // plan_jobs routes the proof_required branch on the PARK ENTRY: a lane with an ACTIVE cooldown
    // is DIVERSIFIED (a bounded gated AI attempt at a FRESH self-generated spec), never parked for
    // a human; past the daily cap it backs off to inert proof_required; an EXPIRED-but-present
    // cooldown (park expiry with no human wake) RE-ARMS the self-respec path directly (operator
    // directive 2026-07-18) instead of an inert proof_required re-fire.
    #[test]
    fn plan_jobs_diversifies_when_proof_cooldown_active_and_bounds_it() {
        // Drive a lane into a non-AI proof_required diagnosis (status=error, auto_safe!=true).
        let name = format!("cooldown_lane_{}", std::process::id());
        let repo = json!({"name": name.clone(), "path": format!("C:/p/{name}")});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "reason": "needs_goal",
                "last_summary": "no north-star GOAL and no actionable backlog",
                "updated_at": now(),
            }))
            .unwrap(),
        )
        .unwrap();

        // ACTIVE cooldown: until in the future.
        let future = Utc::now() + ChronoDuration::seconds(3600);
        let mut st = json!({
            "manual_queue": [],
            "proof_cooldowns": {
                name.clone(): {
                    "until": future.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                    "consecutive": 3
                }
            }
        });
        let cfg = json!({"provider": "openrouter", "targets": [name.clone()]});
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st,
            None,
            false,
        );
        let job = jobs.iter().find(|j| j.name == name).expect("job planned");
        assert_eq!(
            job.kind, "implement",
            "an ACTIVE cooldown DIVERSIFIES (a gated AI attempt), never a human park"
        );
        assert!(job.requires_ai, "the diversification is a gated AI implement iteration");
        assert!(
            job.reason.contains("diversification") && job.reason.contains("FRESH, DIFFERENT"),
            "the reason frames a fresh-approach attempt: {reason}",
            reason = job.reason
        );
        assert!(
            job.next_action.contains("DIFFERENT approach"),
            "the next action tells the brain to take a different approach: {next}",
            next = job.next_action
        );
        assert_eq!(
            diversify_count(&st, &name, &today_local()),
            0,
            "PLANNING consumes nothing — the budget is charged at actual dispatch (job_started)"
        );
        assert!(
            !jobs.iter().any(|j| j.kind == "needs_human_spec"),
            "no lane is ever parked for a human: {jobs:?}"
        );

        // BUDGET SPENT: with the daily diversification cap already used, an ACTIVE cooldown BACKS OFF
        // to low-frequency autonomous retry (inert proof_required) — still NEVER a human park.
        let mut st_capped = json!({
            "manual_queue": [],
            "proof_cooldowns": {name.clone(): {
                "until": future.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "consecutive": 3
            }},
            "diversify": {name.clone(): {"date": today_local(), "count": DIVERSIFY_DAILY_CAP}}
        });
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st_capped,
            None,
            false,
        );
        let job = jobs.iter().find(|j| j.name == name).expect("job planned");
        assert_eq!(
            job.kind, "proof_required",
            "past the daily diversification cap the lane backs off to inert proof_required, not a park"
        );
        assert!(job.reason.contains("backing off"), "the backoff is explicit: {}", job.reason);

        // EXPIRED-but-present cooldown (park expiry, no human wake, no real outcome): the
        // self-respec path RE-ARMS directly — a fresh self-generated spec is planned, never an
        // inert proof_required rotation, never a human park (operator directive 2026-07-18).
        let past = Utc::now() - ChronoDuration::seconds(3600);
        let mut st_expired = json!({
            "manual_queue": [],
            "proof_cooldowns": {name.clone(): {"until": past.format("%Y-%m-%dT%H:%M:%SZ").to_string()}}
        });
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st_expired,
            None,
            false,
        );
        let job = jobs.iter().find(|j| j.name == name).expect("job planned");
        assert_eq!(
            job.kind, "implement",
            "park EXPIRY re-arms the autonomous re-spec (a gated AI attempt), not an inert re-fire"
        );
        assert!(job.diversify, "the expiry re-spec is budget-charged at dispatch like any diversify");
        assert!(
            job.spec_goal.is_some(),
            "the expiry re-spec dispatches a FRESH self-generated goal text"
        );

        let _ = std::fs::remove_dir_all(rt);
    }

    // A STRUCTURALLY stuck lane (a stranded/unmerged branch) is DIVERSIFIED (a bounded gated AI
    // attempt at a fresh approach) on the very first sweep — NEVER parked for a human. A retry-later
    // lane (needs_goal) under the identical empty state stays proof_required, so the two ladders are
    // still distinct; neither is ever routed to needs_human_spec.
    #[test]
    fn plan_jobs_diversifies_a_structurally_stuck_lane_instead_of_parking() {
        // ---- structural lane: a persistent stranded rsi/* branch (has_stop + !running + reason). ----
        let stuck = format!("stranded_lane_{}", std::process::id());
        let stuck_repo = json!({"name": stuck.clone(), "path": format!("C:/p/{stuck}")});
        let stuck_rt = paths::runtime_dir(&stuck_repo).unwrap();
        let _ = std::fs::remove_dir_all(&stuck_rt);
        std::fs::create_dir_all(&stuck_rt).unwrap();
        // No `lock` file => !running. A `stop` sentinel + reason => diagnose "stranded_unmerged_branch".
        std::fs::write(stuck_rt.join("stop"), "stranded_unmerged_branch_persistent\n").unwrap();
        std::fs::write(
            stuck_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "preflight",
                "reason": "stranded_unmerged_branch_persistent",
                "last_summary": "Stranded finished work: rsi/iter-x (ahead 14) — not an ancestor of the fork base.",
            }))
            .unwrap(),
        )
        .unwrap();

        // ---- retry-later lane: needs_goal (config fix, self-clears — keeps the proof_required ladder). ----
        let retry = format!("needsgoal_lane_{}", std::process::id());
        let retry_repo = json!({"name": retry.clone(), "path": format!("C:/p/{retry}")});
        let retry_rt = paths::runtime_dir(&retry_repo).unwrap();
        let _ = std::fs::remove_dir_all(&retry_rt);
        std::fs::create_dir_all(&retry_rt).unwrap();
        std::fs::write(
            retry_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "reason": "needs_goal",
                "last_summary": "no north-star GOAL and no actionable backlog",
                "updated_at": now(),
            }))
            .unwrap(),
        )
        .unwrap();

        // EMPTY state: NO proof_cooldowns armed — so a needs_human_spec verdict can ONLY come from the
        // terminal structural park, never the cooldown path (which the cooldown test covers separately).
        let cfg = json!({"provider": "openrouter", "targets": [stuck.clone(), retry.clone()]});
        let mut st = json!({"manual_queue": []});
        let jobs = plan_jobs(
            &[stuck_repo, retry_repo],
            &cfg,
            &json!({"projects": {}}),
            &mut st,
            None,
            false,
        );

        let stuck_job = jobs.iter().find(|j| j.name == stuck).expect("structural job planned");
        assert_eq!(
            stuck_job.kind, "implement",
            "a structurally-stuck lane DIVERSIFIES (a gated AI attempt), NOT a human park"
        );
        assert_eq!(stuck_job.state, "queued");
        assert!(stuck_job.requires_ai, "the diversification is a gated AI implement iteration");
        assert!(
            stuck_job.next_action.contains("DIFFERENT approach"),
            "the next action tells the brain to take a different approach: {next}",
            next = stuck_job.next_action
        );
        // The specific diagnose() evidence is preserved (which branch) inside the diversify framing.
        assert!(
            stuck_job.reason.to_lowercase().contains("stranded")
                && stuck_job.reason.contains("diversification"),
            "the reason keeps the specific structural cause under the diversify framing: {reason}",
            reason = stuck_job.reason
        );
        assert_eq!(
            diversify_count(&st, &stuck, &today_local()),
            0,
            "PLANNING the structural diversification consumes nothing — charged at dispatch"
        );
        assert!(
            !jobs.iter().any(|j| j.kind == "needs_human_spec"),
            "no lane is ever routed to needs_human_spec: {jobs:?}"
        );

        // The retry-later lane, under the identical empty state, keeps the proof_required ladder.
        let retry_job = jobs.iter().find(|j| j.name == retry).expect("retry job planned");
        assert_eq!(
            retry_job.kind, "proof_required",
            "a retry-later (config-fixable) blocker keeps proof_required — the two ladders are distinct"
        );

        let _ = std::fs::remove_dir_all(stuck_rt);
        let _ = std::fs::remove_dir_all(retry_rt);
    }

    // ===================================================================== #
    // needs_human_spec SURFACE-ONCE park — kill dispatch starvation
    // ===================================================================== #
    //
    // Audit 2026-07-16: the asmodeus needs_human_spec job (requires_ai=false, priority 10)
    // re-dispatched every ~90-120s forever, each finishing instantly as a no-op — and priority 10
    // beat the queued priority-30 real jobs (dotz maintenance, solomon implement), which starved
    // indefinitely. Once surfaced, the identical need must NOT re-enqueue on the dispatch path.

    #[test]
    fn nhs_surface_once_helpers_roundtrip() {
        let mut st = json!({});
        assert!(!nhs_already_surfaced(&st, "asmodeus", "fp1"), "nothing surfaced yet");
        mark_nhs_surfaced(&mut st, "asmodeus", "fp1");
        assert!(nhs_already_surfaced(&st, "asmodeus", "fp1"), "identical need is parked");
        assert!(
            !nhs_already_surfaced(&st, "asmodeus", "fp2"),
            "a CHANGED fingerprint (lane state changed) re-surfaces"
        );
        assert!(!nhs_already_surfaced(&st, "dotz", "fp1"), "per-lane, not global");
        clear_nhs_surfaced(&mut st, "asmodeus");
        assert!(!nhs_already_surfaced(&st, "asmodeus", "fp1"), "cleared = re-surfaces once");
        clear_nhs_surfaced(&mut st, "never_marked"); // no-op, never panics
    }

    #[test]
    fn bump_stuck_counter_real_move_clears_nhs_surfaced() {
        let mut st = json!({});
        mark_nhs_surfaced(&mut st, "dotz", "fp");
        bump_stuck_counter(&mut st, "dotz", true); // inert sweep: park persists
        assert!(nhs_already_surfaced(&st, "dotz", "fp"));
        bump_stuck_counter(&mut st, "dotz", false); // the lane moved
        assert!(
            !nhs_already_surfaced(&st, "dotz", "fp"),
            "a real outcome un-parks the surfaced need"
        );
    }

    // THE BOUND HOLDS: a structurally-stuck lane DIVERSIFIES at most DIVERSIFY_DAILY_CAP times per
    // day (each a gated AI attempt at a fresh approach), then BACKS OFF to inert proof_required —
    // which the dispatch path emit-bypasses, freeing the slot for real jobs. `needs_human_spec` is
    // never produced, spend is bounded, and the dashboard still surfaces the blocker.
    #[test]
    fn plan_jobs_diversification_bound_holds_and_frees_the_slot() {
        let parked = format!("bound_lane_{}", std::process::id());
        let parked_repo = json!({"name": parked.clone(), "path": format!("C:/p/{parked}")});
        let parked_rt = paths::runtime_dir(&parked_repo).unwrap();
        let _ = std::fs::remove_dir_all(&parked_rt);
        std::fs::create_dir_all(&parked_rt).unwrap();
        std::fs::write(parked_rt.join("stop"), "stranded_unmerged_branch_persistent\n").unwrap();
        std::fs::write(
            parked_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "preflight",
                "reason": "stranded_unmerged_branch_persistent",
                "last_summary": "Stranded finished work: rsi/iter-x (+1 commit(s) not on main) — not an ancestor of the fork base.",
            }))
            .unwrap(),
        )
        .unwrap();

        let cfg = json!({"provider": "openrouter", "targets": [parked.clone()]});
        let repos = [parked_repo];
        let ops = json!({"projects": {}});
        let mut st = json!({"manual_queue": []});

        // The first DIVERSIFY_DAILY_CAP dispatch sweeps each DIVERSIFY (a gated AI implement
        // attempt). Each sweep plans the job, then the dispatch charge (`once()`'s job_started
        // path — same charge_diversify_dispatch seam) consumes one budget unit.
        for i in 1..=DIVERSIFY_DAILY_CAP {
            let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
            let job = jobs.iter().find(|j| j.name == parked).expect("diversify job planned");
            assert_eq!(job.kind, "implement", "sweep {i} diversifies (gated AI attempt): {job:?}");
            assert!(job.requires_ai);
            assert!(job.diversify, "the planned job is marked for the dispatch-time charge");
            charge_diversify_dispatch(&mut st, &parked); // the job WON the slot and dispatched
            assert_eq!(
                diversify_count(&st, &parked, &today_local()),
                i,
                "each ACTUAL dispatch consumes exactly one budget unit"
            );
            assert!(
                !jobs.iter().any(|j| j.kind == "needs_human_spec"),
                "never a human park: {jobs:?}"
            );
        }

        // Past the cap the lane BACKS OFF: proof_required is emit-bypassed on the dispatch path, so
        // the lane drops OUT of the queue (slot freed) — never a needs_human_spec, no more AI spend.
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
        assert!(
            !jobs.iter().any(|j| j.name == parked),
            "past the cap the lane backs off (emit-bypassed proof_required), freeing the slot: {jobs:?}"
        );
        assert_eq!(
            diversify_count(&st, &parked, &today_local()),
            DIVERSIFY_DAILY_CAP,
            "the daily diversification budget is capped — no runaway spend"
        );

        // The DISPLAY path still surfaces the blocker (inert proof_required, NOT a human park).
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, false);
        let job = jobs.iter().find(|j| j.name == parked).expect("display job planned");
        assert_eq!(
            job.kind, "proof_required",
            "the dashboard shows the inert blocker, not a human park: {job:?}"
        );
        assert!(job.reason.contains("backing off"), "the backoff is explicit: {}", job.reason);

        let _ = std::fs::remove_dir_all(parked_rt);
    }

    // PLANNED-BUT-UNDISPATCHED re-spec does NOT consume budget (2026-07-17 starvation root cause):
    // plan_jobs runs on the dispatch sweep, the display re-plan, AND every state() poll, but only
    // ONE planned job wins the single_agent slot. Plan-time bumping burned the whole 3/day budget
    // (asmodeus+solomon read 3/3 on a day with ZERO job_started events for those lanes), wedging
    // both lanes in the proof_required backoff for ~20h. The budget must move only when the job
    // actually dispatches.
    #[test]
    fn planned_but_undispatched_re_spec_does_not_consume_budget() {
        let lane = format!("undispatched_lane_{}", std::process::id());
        let repo = json!({"name": lane.clone(), "path": format!("C:/p/{lane}")});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(rt.join("stop"), "stranded_unmerged_branch_persistent\n").unwrap();
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "preflight",
                "reason": "stranded_unmerged_branch_persistent",
                "last_summary": "Stranded finished work: rsi/iter-x (+1 commit(s) not on main) — not an ancestor of the fork base.",
            }))
            .unwrap(),
        )
        .unwrap();

        let cfg = json!({"provider": "openrouter", "targets": [lane.clone()]});
        let repos = [repo];
        let ops = json!({"projects": {}});
        let mut st = json!({"manual_queue": []});

        // Many dispatch-path plans where the lane LOSES the single_agent slot every time (no
        // charge): the budget must stay untouched and the re-spec must keep re-planning as the
        // SAME first attempt — not silently exhaust to 3/3 with zero dispatches.
        for sweep in 1..=2 * DIVERSIFY_DAILY_CAP {
            let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
            let job = jobs.iter().find(|j| j.name == lane).expect("re-spec job planned");
            assert_eq!(job.kind, "implement", "sweep {sweep} still plans the re-spec: {job:?}");
            assert!(job.diversify);
            assert!(
                job.reason.contains(&format!("1/{DIVERSIFY_DAILY_CAP}")),
                "the attempt number does not advance while undispatched: {}",
                job.reason
            );
            assert_eq!(
                diversify_count(&st, &lane, &today_local()),
                0,
                "sweep {sweep}: a planned-but-undispatched re-spec consumes NO budget"
            );
        }
        // The lane finally wins the slot once — exactly one unit is charged.
        charge_diversify_dispatch(&mut st, &lane);
        assert_eq!(diversify_count(&st, &lane, &today_local()), 1);

        let _ = std::fs::remove_dir_all(rt);
    }

    // ===================================================================== #
    // FULL AUTONOMY — park expiry re-arms self-respec; human wake stays an override
    // (operator-directed full-autonomy directive 2026-07-18)
    // ===================================================================== #

    /// Writes the standard non-structural unhealthy lane fixture (needs_goal heartbeat) and
    /// returns (name, repo, runtime_dir). Callers must remove the runtime dir when done.
    fn unhealthy_lane_fixture(tag: &str) -> (String, Value, PathBuf) {
        let name = format!("{tag}_{}", std::process::id());
        let repo = json!({"name": name.clone(), "path": format!("C:/p/{name}")});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        std::fs::write(
            rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "reason": "needs_goal",
                "last_summary": "no north-star GOAL and no actionable backlog",
                "updated_at": now(),
            }))
            .unwrap(),
        )
        .unwrap();
        (name, repo, rt)
    }

    /// A park entry whose `until` is `secs_ago` seconds in the past (an EXPIRED park).
    fn expired_park(name: &str, secs_ago: i64) -> Value {
        let past = Utc::now() - ChronoDuration::seconds(secs_ago);
        json!({name: {
            "until": past.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "consecutive": 28,
        }})
    }

    // Directive test 1: park expiry with NO human wake dispatches a self-respec (budget
    // available) on the DISPATCH path — a fresh self-generated goal, not an inert rotation,
    // never a needs_human_spec park.
    #[test]
    fn park_expiry_with_no_wake_dispatches_a_self_respec() {
        let (name, repo, rt) = unhealthy_lane_fixture("expiry_respec_lane");
        let cfg = json!({"provider": "openrouter", "targets": [name.clone()]});
        let mut st = json!({
            "manual_queue": [],
            "proof_cooldowns": expired_park(&name, 3600),
        });
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st,
            None,
            true, // the DISPATCH path — this is what once() actually runs
        );
        let job = jobs.iter().find(|j| j.name == name).expect("self-respec planned");
        assert_eq!(
            job.kind, "implement",
            "park expiry with no wake dispatches a real gated implement attempt: {job:?}"
        );
        assert!(job.requires_ai && job.diversify);
        assert!(!job.wake_override, "no human was involved");
        let goal = job.spec_goal.as_deref().expect("a FRESH self-generated goal text");
        assert!(
            goal.contains("SELF-SPEC") && goal.contains("category:"),
            "the goal is a bounded self-spec, not the standing goal: {goal}"
        );
        assert!(
            goal.contains("NEVER add") && goal.contains("skip/xfail"),
            "the spec bakes the anti-gaming ground rules in (steer WITH the rails): {goal}"
        );
        assert_eq!(
            job.spec_category.as_deref(),
            Some(SELF_SPEC_CATEGORIES[0]),
            "no spec history -> the rotation starts at the first category"
        );
        assert_eq!(
            diversify_count(&st, &name, &today_local()),
            0,
            "planning charges nothing; the budget moves at dispatch"
        );
        assert!(
            !jobs.iter().any(|j| j.kind == "needs_human_spec"),
            "never a human park: {jobs:?}"
        );
        let _ = std::fs::remove_dir_all(rt);
    }

    // Directive test 2: with the daily budget exhausted, the expired-park lane stays parked
    // QUIETLY (inert emit-bypassed backoff — no AI spend, no needs_human_spec, slot freed) and
    // the self-respec resumes by itself when the per-day budget resets (yesterday's count is
    // dead) — no human wake required at any point.
    #[test]
    fn park_expiry_budget_exhausted_stays_parked_quietly_until_daily_reset() {
        let (name, repo, rt) = unhealthy_lane_fixture("expiry_budget_lane");
        let cfg = json!({"provider": "openrouter", "targets": [name.clone()]});
        let ops = json!({"projects": {}});

        // Budget spent TODAY: the dispatch path emit-bypasses the lane (quiet backoff).
        let mut st = json!({
            "manual_queue": [],
            "proof_cooldowns": expired_park(&name, 3600),
            "diversify": {name.clone(): {"date": today_local(), "count": DIVERSIFY_DAILY_CAP}},
        });
        let jobs = plan_jobs(std::slice::from_ref(&repo), &cfg, &ops, &mut st, None, true);
        assert!(
            !jobs.iter().any(|j| j.name == name),
            "budget exhausted -> the lane backs off quietly (emit-bypassed, slot freed): {jobs:?}"
        );
        assert!(
            !jobs.iter().any(|j| j.kind == "needs_human_spec"),
            "quiet does not mean parked for a human: {jobs:?}"
        );
        // The display path still shows WHY (inert proof_required backoff), so the dashboard is honest.
        let jobs = plan_jobs(std::slice::from_ref(&repo), &cfg, &ops, &mut st, None, false);
        let shown = jobs.iter().find(|j| j.name == name).expect("display job planned");
        assert_eq!(shown.kind, "proof_required");
        assert!(shown.reason.contains("backing off"), "backoff is explicit: {}", shown.reason);

        // A NEW DAY (the stored count is yesterday's): the budget reads 0 and the self-respec
        // re-arms with NO human wake — the cap resets naturally per-day, which is the rate limit.
        let mut st_new_day = json!({
            "manual_queue": [],
            "proof_cooldowns": expired_park(&name, 3600),
            "diversify": {name.clone(): {"date": "2001-01-01", "count": DIVERSIFY_DAILY_CAP}},
        });
        let jobs = plan_jobs(std::slice::from_ref(&repo), &cfg, &ops, &mut st_new_day, None, true);
        let job = jobs.iter().find(|j| j.name == name).expect("self-respec re-armed");
        assert_eq!(job.kind, "implement", "the daily reset re-arms the self-respec: {job:?}");
        assert!(job.diversify && job.spec_goal.is_some());

        let _ = std::fs::remove_dir_all(rt);
    }

    // Directive test 3: the human wake STAYS a working override. A pending wake ack routes the
    // parked lane to a NORMAL gated implement on the operator's STANDING spec (no self-generated
    // goal, no diversify budget), and the wake state helpers behave one-shot.
    #[test]
    fn human_wake_still_works_as_override() {
        let (name, repo, rt) = unhealthy_lane_fixture("wake_override_lane");
        let cfg = json!({"provider": "openrouter", "targets": [name.clone()]});

        // wake()'s state seam: the ack resets park + stuck + surfaced-need and arms the flag.
        let mut st = json!({
            "manual_queue": [],
            "proof_cooldowns": expired_park(&name, 3600),
            "stuck": {name.clone(): {"sweeps": 28, "ts": now()}},
        });
        bump_stuck_counter(&mut st, &name, false); // wake() reuses the real-move arm
        arm_human_wake(&mut st, &name);
        assert!(proof_cooldown_entry(&st, &name).is_none(), "the ack clears the park");
        assert!(human_wake_pending(&st, &name), "the ack arms the one-shot override");

        // Planning with the flag pending: a NORMAL implement on the standing spec — even though
        // the diagnosis is still unhealthy (proof_required verdict).
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st,
            None,
            true,
        );
        let job = jobs.iter().find(|j| j.name == name).expect("override job planned");
        assert_eq!(job.kind, "implement", "the wake override dispatches a real attempt: {job:?}");
        assert!(job.wake_override, "marked for the dispatch-time flag consumption");
        assert!(!job.diversify, "a human-ack'd run never charges the diversify budget");
        assert!(
            job.spec_goal.is_none(),
            "the override runs the operator's STANDING spec, not a generated one"
        );
        assert!(job.reason.contains("operator wake ack"), "reason names the ack: {}", job.reason);

        // One-shot: consuming at dispatch clears the flag; a second plan (still unhealthy, no
        // park entry) falls back to the ordinary proof_required ladder — not a repeat override.
        consume_human_wake(&mut st, &name);
        assert!(!human_wake_pending(&st, &name), "the override is one-shot");
        let jobs = plan_jobs(
            std::slice::from_ref(&repo),
            &cfg,
            &json!({"projects": {}}),
            &mut st,
            None,
            false,
        );
        let job = jobs.iter().find(|j| j.name == name).expect("job planned");
        assert_eq!(job.kind, "proof_required", "after the one shot the normal ladder resumes");
        consume_human_wake(&mut st, "never_armed"); // no-op, never panics

        let _ = std::fs::remove_dir_all(rt);
    }

    // Directive test 4: spec differentiation. A self-generated spec must differ materially from
    // the last failed specs, and a spec family that failed SELF_SPEC_FAMILY_FAIL_LIMIT times in a
    // row is forced onto a different category.
    #[test]
    fn self_spec_differentiation_forces_category_rotation() {
        // No history: rotation starts at the first category.
        assert_eq!(pick_self_spec_category(&[]), SELF_SPEC_CATEGORIES[0]);

        // One failure in a family: one in-family retry is allowed, but the goal text must differ
        // materially (the exclusion block quotes the failed attempt).
        let mut st = json!({});
        let (cat1, goal1) = generate_self_spec(&st, "lane", "blocker evidence", "standing goal");
        assert_eq!(cat1, SELF_SPEC_CATEGORIES[0]);
        record_self_spec_dispatch(&mut st, "lane", cat1, &goal1);
        resolve_self_spec_outcome(&mut st, "lane", true); // failed (e.g. anti-gaming revert)
        let (cat2, goal2) = generate_self_spec(&st, "lane", "blocker evidence", "standing goal");
        assert_eq!(cat2, cat1, "one failure allows one in-family retry");
        assert_ne!(
            normalize_spec(&goal2),
            normalize_spec(&goal1),
            "the retry differs materially from the failed spec"
        );
        assert!(
            goal2.contains("DO NOT REPEAT"),
            "the failed direction is explicitly excluded: {goal2}"
        );
        assert!(
            self_spec_differs_from_failed(&goal2, &self_spec_history(&st, "lane")),
            "the differentiation guard accepts the fresh spec"
        );
        assert!(
            !self_spec_differs_from_failed(&goal1, &self_spec_history(&st, "lane")),
            "re-dispatching the exact failed spec is rejected"
        );

        // Second consecutive failure in the SAME family: the category is FORCED to rotate.
        record_self_spec_dispatch(&mut st, "lane", cat2, &goal2);
        resolve_self_spec_outcome(&mut st, "lane", true);
        let (cat3, goal3) = generate_self_spec(&st, "lane", "blocker evidence", "standing goal");
        assert_ne!(
            cat3, cat1,
            "a spec family that failed {SELF_SPEC_FAMILY_FAIL_LIMIT}x is abandoned"
        );
        assert_eq!(cat3, SELF_SPEC_CATEGORIES[1], "rotation order is deterministic");
        assert!(goal3.contains(&format!("category: {cat3}")));

        // A real MOVE resets the rotation to the first category.
        record_self_spec_dispatch(&mut st, "lane", cat3, &goal3);
        resolve_self_spec_outcome(&mut st, "lane", false); // moved
        assert_eq!(
            pick_self_spec_category(&self_spec_history(&st, "lane")),
            SELF_SPEC_CATEGORIES[0],
            "a lane that moved restarts the rotation fresh"
        );

        // The ledger is capped: old attempts age out.
        for i in 0..(SELF_SPEC_HISTORY_CAP + 3) {
            record_self_spec_dispatch(&mut st, "lane", "tests", &format!("goal {i}"));
            resolve_self_spec_outcome(&mut st, "lane", true);
        }
        assert_eq!(
            self_spec_history(&st, "lane").len(),
            SELF_SPEC_HISTORY_CAP,
            "the ledger window is bounded"
        );

        // An unresolved "dispatched" entry counts as failed (a crashed run is never repeated on faith).
        let st2 = json!({"self_specs": {"lane": [
            {"category": "reliability", "goal": "g1", "outcome": "dispatched"},
        ]}});
        assert!(
            !self_spec_differs_from_failed("g1", &self_spec_history(&st2, "lane")),
            "an in-flight/crashed spec is treated as failed for differentiation"
        );
    }

    // ===================================================================== #
    // FLEET-HEALTH PROBE — proof_required/implement ratio over 24h
    // ===================================================================== #
    //
    // The retry-theater detector at the FLEET level: proof_required / implement over a 24h window
    // from the autopilot event log. >1.5 -> yellow, >2.5 -> red. Pure: the caller passes the event
    // lines + window boundary so the test is deterministic (no IO, no wall-clock coupling).

    /// Count `{"event":"job_finished","job":<job>,"outcome":<outcome>}` events in `lines` whose `ts`
    /// falls within `(since, now]` (inclusive of since, exclusive of now — the last second is the
    /// caller's `now`). Lines that fail to parse are skipped (lenient, same as the probe evaluators).
    /// Pure — unit-tested over fixture lines.
    pub fn count_job_finished(lines: &[&str], since: DateTime<Utc>, now: DateTime<Utc>, job: &str, outcome: &str) -> u64 {
        let mut n = 0u64;
        for line in lines {
            let rec: Value = match serde_json::from_str(line.trim()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if rec.get("event").and_then(Value::as_str) != Some("job_finished") {
                continue;
            }
            if rec.get("job").and_then(Value::as_str) != Some(job) {
                continue;
            }
            if rec.get("outcome").and_then(Value::as_str) != Some(outcome) {
                continue;
            }
            let Some(t) = rec.get("ts").and_then(Value::as_str).and_then(parse_ts) else {
                continue;
            };
            if t > since && t <= now {
                n += 1;
            }
        }
        n
    }

    /// The fleet-health probe: proof_required / implement ratio over a 24h window. Returns the
    /// (ratio, proof_count, implement_count). implement == 0 -> ratio is None (no denominator — the
    /// probe reports unobservable, not a fabricated green). Pure — the caller passes the event lines
    /// + the now boundary. This is the computation the `cmd`-kind ops.json probe runs via a thin
    ///   PowerShell wrapper (see ops.json solomon proof_ratio probe) AND the unit-tested pure core.
    pub fn proof_implement_ratio(lines: &[&str], now: DateTime<Utc>) -> Option<f64> {
        let since = now - ChronoDuration::hours(24);
        let proof = count_job_finished(lines, since, now, "proof_required", "proof_required") as f64;
        let implement_shipped =
            count_job_finished(lines, since, now, "implement", "shipped") as f64;
        // An implement run that reverted also lands as proof_required (fleet.rs:401). To measure the
        // "retry theater" ratio honestly we count ALL implement job_finished events (the AI jobs that
        // actually ran, whether they shipped or reverted) as the denominator — the theater is the loop
        // spinning on inert proof_required WITHOUT running implement jobs. The pure counter counts by
        // (job, outcome); here we sum the implement outcomes that mean "an AI run actually happened".
        let implement_reverted =
            count_job_finished(lines, since, now, "implement", "proof_required") as f64;
        let implement_blocked = count_job_finished(lines, since, now, "implement", "blocked") as f64;
        let implement = implement_shipped + implement_reverted + implement_blocked;
        if implement == 0.0 {
            return None;
        }
        Some(proof / implement)
    }

    #[test]
    fn proof_implement_ratio_red_at_3_5_to_1() {
        // A 24h window with 3.5:1 proof_required:implement (e.g. 7 proof_required, 2 implement shipped)
        // -> ratio 3.5 -> RED (exceeds 2.5).
        let now = Utc::now();
        let lines: Vec<String> = (0..7)
            .map(|i| {
                serde_json::to_string(&json!({
                    "event": "job_finished",
                    "repo": "dotz",
                    "job": "proof_required",
                    "outcome": "proof_required",
                    "ts": (now - ChronoDuration::seconds(600 + i * 60))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                }))
                .unwrap()
            })
            .chain((0..2).map(|i| {
                serde_json::to_string(&json!({
                    "event": "job_finished",
                    "repo": "maki",
                    "job": "implement",
                    "outcome": "shipped",
                    "ts": (now - ChronoDuration::seconds(300 + i * 60))
                        .format("%Y-%m-%dT%H:%M:%SZ")
                        .to_string(),
                }))
                .unwrap()
            }))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let ratio = proof_implement_ratio(&refs, now).expect("ratio computed");
        assert!((ratio - 3.5).abs() < 1e-9, "ratio is 3.5 (7 proof / 2 implement): {ratio}");
        assert!(ratio > 2.5, "3.5:1 is RED (>2.5)");
    }

    #[test]
    fn proof_implement_ratio_yellow_at_2_to_1() {
        let now = Utc::now();
        // 4 proof_required, 2 implement -> 2.0 -> YELLOW (>1.5, <=2.5).
        let lines: Vec<String> = (0..4)
            .map(|i| {
                serde_json::to_string(&json!({
                    "event": "job_finished", "repo": "dotz", "job": "proof_required",
                    "outcome": "proof_required",
                    "ts": (now - ChronoDuration::seconds(600 + i * 60))
                        .format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                })).unwrap()
            })
            .chain((0..2).map(|i| {
                serde_json::to_string(&json!({
                    "event": "job_finished", "repo": "maki", "job": "implement",
                    "outcome": "shipped",
                    "ts": (now - ChronoDuration::seconds(300 + i * 60))
                        .format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                })).unwrap()
            }))
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let ratio = proof_implement_ratio(&refs, now).expect("ratio computed");
        assert!((ratio - 2.0).abs() < 1e-9, "ratio is 2.0: {ratio}");
        assert!(ratio > 1.5 && ratio <= 2.5, "2.0:1 is YELLOW (>1.5, <=2.5)");
    }

    #[test]
    fn proof_implement_ratio_green_at_1_to_1() {
        let now = Utc::now();
        // 1 proof_required, 1 implement shipped -> 1.0 -> GREEN (<=1.5).
        let lines = [
            serde_json::to_string(&json!({
                "event": "job_finished", "repo": "dotz", "job": "proof_required",
                "outcome": "proof_required",
                "ts": (now - ChronoDuration::seconds(600)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })).unwrap(),
            serde_json::to_string(&json!({
                "event": "job_finished", "repo": "maki", "job": "implement",
                "outcome": "shipped",
                "ts": (now - ChronoDuration::seconds(300)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })).unwrap(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let ratio = proof_implement_ratio(&refs, now).expect("ratio computed");
        assert!((ratio - 1.0).abs() < 1e-9, "ratio is 1.0: {ratio}");
        assert!(ratio <= 1.5, "1.0:1 is GREEN (<=1.5)");
    }

    #[test]
    fn proof_implement_ratio_none_when_no_implement_in_window() {
        let now = Utc::now();
        // 5 proof_required, 0 implement -> None (no denominator — unobservable, not a fake green).
        let lines: Vec<String> = (0..5)
            .map(|i| {
                serde_json::to_string(&json!({
                    "event": "job_finished", "repo": "dotz", "job": "proof_required",
                    "outcome": "proof_required",
                    "ts": (now - ChronoDuration::seconds(600 + i * 60))
                        .format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                })).unwrap()
            })
            .collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        assert_eq!(
            proof_implement_ratio(&refs, now),
            None,
            "no implement jobs in window -> None (unobservable), not a fabricated 0 or green"
        );
    }

    #[test]
    fn proof_implement_ratio_excludes_events_outside_24h_window() {
        let now = Utc::now();
        // An old proof_required (30h ago) is OUTSIDE the 24h window; only the fresh ones count.
        let lines = [
            serde_json::to_string(&json!({
                "event": "job_finished", "repo": "dotz", "job": "proof_required",
                "outcome": "proof_required",
                "ts": (now - ChronoDuration::hours(30)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })).unwrap(),
            serde_json::to_string(&json!({
                "event": "job_finished", "repo": "dotz", "job": "proof_required",
                "outcome": "proof_required",
                "ts": (now - ChronoDuration::seconds(600)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })).unwrap(),
            serde_json::to_string(&json!({
                "event": "job_finished", "repo": "maki", "job": "implement",
                "outcome": "shipped",
                "ts": (now - ChronoDuration::seconds(300)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            })).unwrap(),
        ];
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let ratio = proof_implement_ratio(&refs, now).expect("ratio computed");
        // 1 proof (the 30h-old one excluded) / 1 implement = 1.0
        assert!((ratio - 1.0).abs() < 1e-9, "window excludes the 30h-old event: {ratio}");
    }
}
