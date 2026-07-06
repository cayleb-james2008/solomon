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
const PROOF_FILE: &str = "autopilot_proof.json";
const LEGACY_STATE_FILE: &str = "fleet_state.json";
const LEGACY_PROOF_FILE: &str = "fleet_proof.json";
const DEFAULT_RUN_TIMEOUT_S: u64 = 14_400;

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
    let jobs = plan_jobs(&repos, &cfg, &ops_payload, &st, None);
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
    if max_concurrent(&cfg) != 1 {
        return json!({"ok": false, "error": "single_agent requires max_concurrent_agent_calls=1"});
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
        let jobs = plan_jobs(&repos, &cfg, &ops_payload, &st, only_name);
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
    let jobs = plan_jobs(&repos, &cfg, &ops_payload, &st, only_name);
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

    let Some(job) = jobs.first().cloned() else {
        st["active"] = Value::Null;
        st["last_result"] =
            json!({"ts": now(), "outcome": "complete", "summary": "no queued autopilot work"});
        let _ = write_state(&st);
        return json!({"ok": true, "actions": [], "queue": []});
    };

    st["active"] = job_value(&job);
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
    st["queue"] = Value::Array(
        plan_jobs(&repos, &cfg, &read_ops_payload(), &st, only_name)
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
        set_cooldown(st, cfg, "provider quota/rate limit");
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
    } else if latest.get("status").and_then(Value::as_str) == Some("reverted") {
        "proof_required"
    } else if out.code == 0 {
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
        _ => proof(job, "complete", &job.reason, None, repo),
    }
}

fn plan_jobs(
    repos: &[Value],
    cfg: &Value,
    ops_payload: &Value,
    st: &Value,
    only_name: Option<&str>,
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
        let diag = supervisor::diagnose(repo);
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
        let (kind, state, requires_ai, reason, next_action) = if diag_cat == "quota_error"
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
        if kind == "complete" {
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
            .then_with(|| a.requires_ai.cmp(&b.requires_ai))
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
    acquire_lock_at(dir.join(LOCK_FILE))
}

fn autopilot_lock_live() -> bool {
    autopilot_lock_live_at(&paths::here().join("runtime").join(LOCK_FILE))
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

fn max_concurrent(cfg: &Value) -> i64 {
    cfg.get("max_concurrent_agent_calls")
        .and_then(Value::as_i64)
        .unwrap_or(1)
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
        let st = json!({"manual_queue": []});
        let jobs = plan_jobs(&repos, &cfg, &ops, &st, None);
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
            &[repo.clone()],
            &cfg,
            &json!({"projects": {}}),
            &json!({}),
            None,
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
            &[repo.clone()],
            &cfg,
            &json!({"projects": {"autopilot_stale": {"status": "green"}}}),
            &json!({}),
            None,
        );
        assert_eq!(jobs[0].kind, "implement");
        assert!(jobs[0].requires_ai);
        assert!(jobs[0].reason.contains("stale quota"));
        let _ = std::fs::remove_dir_all(rt);
    }

    #[test]
    fn daily_budget_resets_by_date() {
        let st = json!({"daily": {"date": "1999-01-01", "calls": 99}});
        assert_eq!(daily_used(&st), 0);
        let mut st2 = json!({});
        increment_daily(&mut st2);
        assert_eq!(daily_used(&st2), 1);
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
}
