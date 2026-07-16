//! Single-agent Autopilot runtime.
//!
//! This replaces "one long-lived improver process per project" with one file-backed scheduler that owns
//! provider quota, job priority, active leases, and proof records. Managed repo mutation still flows
//! through the existing gated `run-improver --once` executor.

use crate::control::{heartbeat, locks, paths, proc, registry};
use crate::improver::pi;
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

/// Consecutive autopilot sweeps a lane must be `proof_required` before the watchdog's
/// autopilot recover pass is allowed to force a stop→ideate→restart heal on it. This is the
/// time-decay the permanent-stuck audit found missing: `finish_non_ai_job`'s proof_required arm
/// mutates nothing, so a lane diagnosed `noop_streak`→`auto_safe=false` re-diagnoses the same
/// frozen corpse every ~5-min sweep forever. The watchdog only heals after N such sweeps (not
/// every sweep — that would token-thrash), which combined with `recover()`'s own `prior_heals<3`
/// exponential backoff caps the force-cycle. 3 sweeps ≈ 15 min at the 5-min sentinel cadence.
pub const STUCK_SWEEP_THRESHOLD: u64 = 3;

/// Consecutive `proof_required` outcomes after which a lane is parked in a 4h cooldown and a
/// `needs_human_spec` need is filed instead of dispatching another inert proof_required job. This
/// kills retry theater: a lane diagnosed unhealthy with `auto_safe != true` re-fires the same
/// non-mutating `proof_required` job every ~5-min sweep forever (the dotz 53:1 proof_required:implement
/// ratio was this). After PROOF_COOLDOWN_THRESHOLD consecutive no-ops, the lane is parked for
/// PROOF_COOLDOWN_S and surfaced as a `needs_human_spec` need (kind "decision") — the operator specs a
/// real fix instead of the loop spinning on a corpse. The cooldown clears the moment the lane
/// produces a non-proof_required outcome (it moved), same as `bump_stuck_counter` clears `stuck`.
pub const PROOF_COOLDOWN_THRESHOLD: u64 = 3;
/// The proof_required cooldown window: 4h (lowered from 24h — a wedged lane should re-surface
/// the same day, not vanish for a day). Long enough to break the retry loop and force a human
/// spec; short enough that a genuinely-stuck lane re-surfaces rather than being silently parked
/// forever. Matched to the operator's "open Solomon, review the need, spec a fix" cadence.
pub const PROOF_COOLDOWN_S: i64 = 14_400;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Job {
    name: String,
    kind: String,
    state: String,
    priority: i64,
    requires_ai: bool,
    reason: String,
    next_action: String,
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
    bump_stuck_counter(
        &mut st,
        &job.name,
        outcome_str == "proof_required" || outcome_str == "needs_human_spec",
    );
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
    // HUMAN ACK: an explicit per-lane wake un-parks the lane's surfaced needs_human_spec need —
    // the operator reviewed/spec'd it, so the need re-checks the lane's current state and (if it
    // still holds) re-surfaces exactly once instead of staying silently parked.
    if let Some(n) = only_name {
        clear_nhs_surfaced(&mut st, n);
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
    let out = run_repo_once(repo, cfg, auto_push, key_env, &key_value);
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
        // re-checkable git-state class (dirty tree / out-of-band base / stranded branch) is
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
        let last_proof = read_proof(&name);
        let proof_fresh = last_proof
            .as_ref()
            .and_then(|p| p.get("ts").and_then(Value::as_str))
            .and_then(parse_ts)
            .map(|t| Utc::now() - t < ChronoDuration::hours(12))
            .unwrap_or(false);
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
        } else if ops_status == "red" || ops_status == "yellow" || manual_hit || !proof_fresh {
            (
                "implement",
                "queued",
                true,
                if ops_status == "red" || ops_status == "yellow" {
                    "ops outcome needs improvement"
                } else if manual_hit {
                    "manual autopilot wake request"
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
                "fresh proof exists and ops are not red/yellow",
                "hold",
            )
        };
        // TERMINAL PARK (kill retry theater at the SOURCE): a lane whose diagnosis is a
        // STRUCTURALLY stuck condition — a stranded/unmerged branch, a self-stopped or hung loop
        // process, an un-pushed / untracked-blocked base — is HUMAN-SPEC-REQUIRED, not retry-later.
        // Route it DIRECTLY to the terminal `needs_human_spec` state so `proof_required` is NEVER
        // chosen for it. This PRECEDES the proof_required cooldown below, which only rate-limits and
        // RE-FIRES the identical inert proof_required job every PROOF_COOLDOWN_S: for a condition no
        // loop iteration can clear — nothing the scheduler does merges the branch, kills the PID, or
        // pushes the base — re-surfacing the same corpse on cooldown expiry is pure theater. A
        // CONFIG/TRANSIENT blocker (missing key/goal, provider quota, a stale gate, an exhausted
        // backlog) is NOT structural and keeps the proof_required → cooldown → needs_human_spec
        // ladder, so a self-clearing condition re-enters the queue rather than being parked forever.
        // No inline stuck bump here: unlike the proof_required emit-bypass below (which is `continue`d
        // before finish_non_ai_job), a needs_human_spec job flows to the shared post-dispatch bump in
        // `once()`, exactly as the cooldown-parked needs_human_spec does — streak accounting is
        // identical, and the routing above is purely category-driven (it never reads the streak).
        if kind == "proof_required" && supervisor::is_structurally_stuck(diag_cat) {
            kind = "needs_human_spec";
            state = "needs_human_spec";
            requires_ai = false;
            // `reason` already holds diagnose()'s evidence (set in the proof_required arm) — keep it
            // so the operator sees the SPECIFIC structural cause (which branch / which PID / which
            // base), not a generic park message.
            next_action = "reconcile the structural blocker (merge/push the stranded branch, kill \
                           the hung improver PID, or push/clean the base), then wake the lane";
        }
        // proof_required COOLDOWN (kill retry theater): a lane parked for PROOF_COOLDOWN_S after
        // PROOF_COOLDOWN_THRESHOLD consecutive inert proof_required sweeps emits a
        // `needs_human_spec` need (kind "decision") INSTEAD of another inert proof_required job.
        // The operator reviews the need and specs a real fix; the loop stops spinning on a corpse.
        // An EXPIRED cooldown (until <= now) lets the proof_required job re-fire (and re-arm on the
        // next threshold crossing) — a genuinely-stuck lane re-surfaces rather than being parked
        // forever. The `stuck` counter is NOT cleared here (it keeps counting so the watchdog's
        // force-heal ladder still sees the full streak); only the job KIND changes.
        if kind == "proof_required" && proof_cooldown_active_at(st, &name, Utc::now()) {
            let cd = proof_cooldown_entry(st, &name).cloned().unwrap_or(Value::Null);
            kind = "needs_human_spec";
            state = "needs_human_spec";
            requires_ai = false;
            reason = "lane hit PROOF_COOLDOWN_THRESHOLD consecutive proof_required sweeps without a \
                      mutation; parked for a human spec — retry theater killed";
            next_action = "review the blocker, spec a real fix, then wake the lane";
            // Surface the cooldown in the job reason so the dashboard shows WHY it is parked.
            let _ = cd; // (the verdict's extra carries the diagnosis; the cooldown is in state)
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
        if kind == "proof_required" && emit_proofs {
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

fn run_repo_once(
    repo: &Value,
    cfg: &Value,
    auto_push: bool,
    key_env: &str,
    key_value: &str,
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
        registry::project_goal(repo),
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
/// PROOF_COOLDOWN_THRESHOLD, `arm_proof_cooldown` parks it for PROOF_COOLDOWN_S (kills retry
/// theater — `plan_jobs` then emits a `needs_human_spec` need instead of another inert
/// proof_required). On any non-proof_required outcome, both `stuck` and `proof_cooldowns` are
/// cleared (the lane moved, so it is no longer stuck NOR parked).
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
// row is parked for PROOF_COOLDOWN_S and surfaced as a `needs_human_spec` need. The state lives in
// `autopilot_state.json` under `st["proof_cooldowns"][name] = {"until": <iso>, "armed_at": <iso>,
// "consecutive": N}` so it round-trips with the existing read_state/write_state. The pure helpers
// below are unit-tested; `plan_jobs` gates the proof_required branch on `proof_cooldown_active`.

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
            "reason": "proof_required retry theater — parked for a human spec",
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
        "cooldown_s": cfg.get("cooldown_s").cloned().unwrap_or(json!(86400)),
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
        .unwrap_or(86400)
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
    // After PROOF_COOLDOWN_THRESHOLD (3) consecutive inert proof_required sweeps, a lane is parked
    // for PROOF_COOLDOWN_S (4h) and a `needs_human_spec` need is filed instead of dispatching
    // another inert proof_required job. The cooldown clears when the lane produces a real (non
    // proof_required / non needs_human_spec) outcome.

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
            json!("proof_required retry theater — parked for a human spec")
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

    // The core retry-theater kill: 3 consecutive proof_required sweeps arm the cooldown; the 4th
    // sweep emits a `needs_human_spec` job (kind changed) instead of another inert proof_required.
    #[test]
    fn bump_stuck_counter_arms_cooldown_at_threshold_and_needs_human_spec_blocks_4th() {
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

        // While parked, a needs_human_spec outcome (the parked lane's job kind) is TREATED AS STILL
        // STUCK — the streak persists and the cooldown stays armed (not cleared).
        bump_stuck_counter(&mut st, "dotz", true); // outcome == "needs_human_spec" → still stuck
        assert!(
            proof_cooldown_active_at(&st, "dotz", Utc::now()),
            "parked lane stays parked across needs_human_spec sweeps (streak persists)"
        );
        assert_eq!(st["stuck"]["dotz"]["sweeps"], 4);

        // The lane eventually MOVES (shipped/blocked/complete) — stuck + cooldown both clear.
        bump_stuck_counter(&mut st, "dotz", false);
        assert!(!proof_cooldown_active_at(&st, "dotz", Utc::now()), "cleared on a real move");
        assert!(st.get("stuck").unwrap().get("dotz").is_none());
    }

    // plan_jobs gates the proof_required branch on the cooldown: a lane with an ACTIVE cooldown gets
    // a `needs_human_spec` job (not another inert proof_required); an EXPIRED cooldown re-fires.
    #[test]
    fn plan_jobs_emits_needs_human_spec_when_proof_cooldown_active() {
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
            job.kind, "needs_human_spec",
            "an ACTIVE cooldown emits a needs_human_spec need, not another inert proof_required"
        );
        assert!(!job.requires_ai, "needs_human_spec is a non-AI inert need");
        assert!(
            job.reason.contains("PROOF_COOLDOWN_THRESHOLD"),
            "the need reason names the retry-theater kill: {reason}",
            reason = job.reason
        );
        assert!(
            job.next_action.contains("spec a real fix"),
            "the next action tells the operator to spec a real fix: {next}",
            next = job.next_action
        );

        // EXPIRED cooldown (until in the past): proof_required re-fires (the lane is no longer parked).
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
            job.kind, "proof_required",
            "an EXPIRED cooldown lets proof_required re-fire (lane re-surfaces, not parked forever)"
        );

        let _ = std::fs::remove_dir_all(rt);
    }

    // A STRUCTURALLY stuck lane (a stranded/unmerged branch — human reconcile required) must reach
    // the TERMINAL `needs_human_spec` state on the VERY FIRST sweep, with NO cooldown armed, instead
    // of the inert `proof_required` job — proving retry theater is killed at the source and never
    // re-fires. A retry-later lane (needs_goal) under the identical empty state stays proof_required,
    // so the two remediation ladders are provably distinct.
    #[test]
    fn plan_jobs_routes_structurally_stuck_lane_to_terminal_needs_human_spec() {
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
            stuck_job.kind, "needs_human_spec",
            "a structurally-stuck lane must terminally park, NOT re-fire proof_required"
        );
        assert_eq!(stuck_job.state, "needs_human_spec");
        assert!(!stuck_job.requires_ai, "the terminal park is a non-AI inert need");
        assert!(
            stuck_job.next_action.contains("reconcile the structural blocker"),
            "the next action tells the operator to reconcile the git/process state: {next}",
            next = stuck_job.next_action
        );
        // The specific diagnose() evidence is preserved (which branch), not a generic park message.
        assert!(
            stuck_job.reason.to_lowercase().contains("stranded"),
            "the reason keeps the specific structural cause: {reason}",
            reason = stuck_job.reason
        );
        assert!(
            proof_cooldown_entry(&st, &stuck).is_none(),
            "the terminal park must not depend on (or arm) the proof_required cooldown"
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

    // The starvation kill itself: on the DISPATCH path a surfaced needs_human_spec is skipped
    // (its stuck streak still bumps inline) so the queue's real jobs get the dispatch slot; the
    // DISPLAY path still shows it; a changed fingerprint re-surfaces it.
    #[test]
    fn plan_jobs_parks_surfaced_needs_human_spec_on_dispatch_path() {
        // Structural nhs lane (the asmodeus shape): stop sentinel + stranded reason, !running.
        let parked = format!("nhs_parked_lane_{}", std::process::id());
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
        // A healthy lane behind it whose implement job was starving.
        let real = format!("nhs_real_lane_{}", std::process::id());
        let real_repo = json!({"name": real.clone(), "path": format!("C:/p/{real}")});
        let real_rt = paths::runtime_dir(&real_repo).unwrap();
        let _ = std::fs::remove_dir_all(&real_rt);
        std::fs::create_dir_all(&real_rt).unwrap();

        let cfg = json!({"provider": "openrouter", "targets": [parked.clone(), real.clone()]});
        let repos = [parked_repo, real_repo];
        let ops = json!({"projects": {}});

        // Sweep 1 (dispatch path, nothing surfaced yet): the need IS planned — the one surfacing.
        let mut st = json!({"manual_queue": []});
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
        let need = jobs
            .iter()
            .find(|j| j.name == parked && j.kind == "needs_human_spec")
            .expect("first sweep surfaces the need once");
        // once() marks the surfacing after the dispatch — simulate exactly that.
        mark_nhs_surfaced(&mut st, &parked, &need.reason);

        // Sweep 2 (dispatch path): the identical need is PARKED; the real job gets the slot.
        let before = st["stuck"][&parked]["sweeps"].as_u64().unwrap_or(0);
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
        assert!(
            !jobs.iter().any(|j| j.name == parked),
            "a surfaced need must not re-enqueue on the dispatch path: {jobs:?}"
        );
        assert!(
            jobs.iter().any(|j| j.name == real && j.kind == "implement"),
            "the starved real job now heads the queue: {jobs:?}"
        );
        assert_eq!(
            st["stuck"][&parked]["sweeps"].as_u64().unwrap_or(0),
            before + 1,
            "the parked lane's stuck streak still bumps (watchdog window intact)"
        );

        // Display path (emit_proofs=false): the dashboard still shows the parked need.
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, false);
        assert!(
            jobs.iter().any(|j| j.name == parked && j.kind == "needs_human_spec"),
            "the display path keeps showing WHY the lane is parked: {jobs:?}"
        );

        // The lane's state changes (different stranding => different diagnose evidence/reason):
        // the need re-surfaces exactly once.
        std::fs::write(
            parked_rt.join("heartbeat.json"),
            serde_json::to_string(&json!({
                "status": "error",
                "phase": "preflight",
                "reason": "stranded_unmerged_branch_persistent",
                "last_summary": "Stranded finished work: rsi/iter-OTHER (+3 commit(s) not on main) — not an ancestor of the fork base.",
            }))
            .unwrap(),
        )
        .unwrap();
        let jobs = plan_jobs(&repos, &cfg, &ops, &mut st, None, true);
        assert!(
            jobs.iter().any(|j| j.name == parked && j.kind == "needs_human_spec"),
            "a changed fingerprint (lane state changed) re-surfaces the need once: {jobs:?}"
        );

        let _ = std::fs::remove_dir_all(parked_rt);
        let _ = std::fs::remove_dir_all(real_rt);
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
