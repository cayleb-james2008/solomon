//! The CEO rhythm (Solomon v2, Phase B) — the Polsia pattern: a morning plan that steers every
//! lane from MEASURED outcomes, and an evening summary that reports VERIFIED outcomes to the
//! operator. This is the accountability layer the June post-mortem demanded: "did we actually
//! post / trade / train today?" answered once a day, from disk, loudly.
//!
//!   - **Morning plan** (>= 07:00 local, once/day): one LLM call (minimax-m3 over Ollama Cloud —
//!     the ONLY LLM in the ops plane; everything else stays deterministic) reads the outcome
//!     ledger + probe verdicts and writes ONE concrete `- [ ] [tier] goal (ceo <date>)` item atop
//!     each lane's `improver/<name>/backlog.md` — the exact file the RSI loop's
//!     `top_backlog_item` consumes. Asmodeus (priority 1) gets the deepest, most specific goal.
//!     On LLM failure: retry next sweeps (3 attempts max), then give up for the day with a
//!     notified error — lanes simply continue their standing goals. An honest skipped plan beats
//!     a fabricated one.
//!   - **Evening summary** (>= 20:00 local, once/day): DETERMINISTIC — no LLM. Renders the
//!     outcome ledger + fleet probe status + 24 h incidents to `runtime/reports/<date>.md`,
//!     appends the daily line to `runtime/outcomes.jsonl`, and notifies the operator (urgent when
//!     any ZERO-outcome flag or red probe is present). A project with zero posts / zero live
//!     trades / a lane that never fired is flagged LOUDLY — silence is never success.
//!
//! Scheduling: rides the watchdog tick (visibly-open Solomon.exe) or the `solomon plan` /
//! `solomon report` CLI. NO SCHEDULED TASK exists and none may be created (operator rule) — a
//! closed Solomon.exe is an honest blind window, and the first tick after reopening notifies the
//! operator how long ops was blind.
#![allow(dead_code)]

use crate::control::{paths, proc};
use crate::notify::{self, Notice};
use crate::ops::{self, ledger};
use chrono::{Timelike, Utc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Local-time due hours (operator's wall clock, not UTC).
const PLAN_HOUR: u32 = 7;
const SUMMARY_HOUR: u32 = 20;
/// Attempts per day before giving up (a failing LLM endpoint must not be hammered every 2 min).
const MAX_ATTEMPTS: i64 = 3;
/// The CEO planner model — minimax-m3 on Ollama Cloud (operator decision, 2026-07-01).
const CEO_MODEL: &str = "minimax-m3";
/// A reopened app that was blind longer than this notifies the gap (seconds).
const BLIND_NOTICE_S: f64 = 21_600.0;

/// HERE/runtime/_ceo_state.json — {"plan": {...}, "summary": {...}} day-gate state.
fn state_path() -> PathBuf {
    paths::here().join("runtime").join("_ceo_state.json")
}

/// HERE/runtime/reports/ — daily plan + summary markdown.
fn reports_dir() -> PathBuf {
    paths::here().join("runtime").join("reports")
}

fn read_state() -> Value {
    std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_state(st: &Value) {
    if let Some(parent) = state_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&state_path(), st);
}

// --------------------------------------------------------------------------- //
// day gate (pure — unit-tested)
// --------------------------------------------------------------------------- //

/// True iff `section` ({"done": date, "attempts_date": date, "attempts": n}) is due at `hour`
/// on `today`: past the due hour, not already done today, and under the attempt cap.
pub fn should_attempt(section: &Value, today: &str, hour: u32, due_hour: u32) -> bool {
    if hour < due_hour {
        return false;
    }
    if section.get("done").and_then(Value::as_str) == Some(today) {
        return false;
    }
    attempts_today(section, today) < MAX_ATTEMPTS
}

/// The attempt count IF it belongs to `today` (a stale attempts_date resets the count).
pub fn attempts_today(section: &Value, today: &str) -> i64 {
    if section.get("attempts_date").and_then(Value::as_str) == Some(today) {
        section.get("attempts").and_then(Value::as_i64).unwrap_or(0)
    } else {
        0
    }
}

/// Evolve the section after an attempt: success stamps `done`; failure increments the day's
/// attempt count. Pure — returns the new section.
pub fn record_attempt(section: &Value, today: &str, ok: bool) -> Value {
    if ok {
        json!({"done": today})
    } else {
        json!({
            "done": section.get("done").cloned().unwrap_or(Value::Null),
            "attempts_date": today,
            "attempts": attempts_today(section, today) + 1,
        })
    }
}

// --------------------------------------------------------------------------- //
// tick — the watchdog graft
// --------------------------------------------------------------------------- //

/// One CEO tick: blind-window notice (once per process), then the two day-gated jobs. Total —
/// every failure path is caught and recorded; a CEO failure can never abort the watchdog sweep
/// (the caller also wraps this in catch_unwind, matching the ops graft).
pub fn tick() {
    blind_window_notice_once();

    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let hour = now.hour();
    let mut st = read_state();

    let plan_sec = st.get("plan").cloned().unwrap_or_else(|| json!({}));
    if should_attempt(&plan_sec, &today, hour, PLAN_HOUR) {
        let ok = morning_plan().get("ok").and_then(Value::as_bool).unwrap_or(false);
        let new_sec = record_attempt(&plan_sec, &today, ok);
        // Third strike: give up for the day, loudly — an unplanned day must be a KNOWN unplanned day.
        if !ok && attempts_today(&new_sec, &today) >= MAX_ATTEMPTS {
            let _ = notify::send(&Notice::red(
                "Solomon: morning plan FAILED".into(),
                format!("{MAX_ATTEMPTS} attempts failed — lanes continue on standing goals today"),
            ));
            st["plan"] = json!({"done": today});
        } else {
            st["plan"] = new_sec;
        }
        write_state(&st);
    }

    let sum_sec = st.get("summary").cloned().unwrap_or_else(|| json!({}));
    if should_attempt(&sum_sec, &today, hour, SUMMARY_HOUR) {
        let ok = evening_summary().get("ok").and_then(Value::as_bool).unwrap_or(false);
        st["summary"] = record_attempt(&sum_sec, &today, ok);
        write_state(&st);
    }
}

/// Once per process: if the just-written ops_status carries a blind window over the threshold,
/// tell the operator how long ops was blind (the honest reopen signal).
fn blind_window_notice_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static DONE: AtomicBool = AtomicBool::new(false);
    if DONE.swap(true, Ordering::SeqCst) {
        return;
    }
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    if let Some(gap) = status.get("blind_window_s").and_then(Value::as_f64) {
        if gap > BLIND_NOTICE_S {
            let _ = notify::send(&Notice::report(
                "Solomon: ops was blind".into(),
                format!("no probe sweeps for {:.1} h (Solomon.exe closed) — fleet status is now live again", gap / 3600.0),
            ));
        }
    }
}

// --------------------------------------------------------------------------- //
// morning plan
// --------------------------------------------------------------------------- //

/// Compose + apply the morning plan. Returns {"ok": bool, ...} (also used by `solomon plan`).
pub fn morning_plan() -> Value {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let snapshot = ledger::snapshot();
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);

    // Lane inventory, priority-ordered by the ops registry's priority. ONLY lanes with a
    // non-empty north-star goal are planned: load_repos() merges auto-DISCOVERED repos over
    // repos.json, and a discovered-but-unconfigured lane (no goal) is dormant by definition —
    // planning it would invent work the operator never commissioned (observed 2026-07-01: two
    // goal-less discovered dirs got fabricated "dormant heartbeat" goals).
    let mut lanes: Vec<(i64, String, String)> = Vec::new(); // (priority, name, goal excerpt)
    for r in crate::control::registry::load_repos() {
        let name = match r.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        // The goal post the CEO plane steers by: the operator's committed one-line
        // improver/<name>/goal.md, falling back to the repos.json `goal` field when goal.md is
        // absent. A written-down target is the thing you optimize distance toward.
        let fallback = r.get("goal").and_then(Value::as_str).unwrap_or("");
        let goal_md = std::fs::read_to_string(goal_post_path(&name)).ok();
        let goal_full = pick_goal_post(goal_md.as_deref(), fallback);
        if goal_full.trim().is_empty() {
            continue; // no north star = not a planned lane
        }
        let prio = snapshot["projects"]
            .get(&name)
            .and_then(|p| p.get("priority"))
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX);
        lanes.push((prio, name, goal_full.chars().take(500).collect()));
    }
    lanes.sort();
    if lanes.is_empty() {
        return json!({"ok": false, "error": "no lanes in repos.json"});
    }

    // Idempotence: a lane whose backlog already carries today's (ceo <date>) marker is planned.
    let unplanned: Vec<&(i64, String, String)> = lanes
        .iter()
        .filter(|(_, name, _)| {
            std::fs::read_to_string(backlog_path(name))
                .map(|c| !c.contains(&ceo_marker(&today)))
                .unwrap_or(true)
        })
        .collect();
    if unplanned.is_empty() {
        return json!({"ok": true, "note": "already planned today"});
    }

    // Fleet context for the model: outcomes + probe reasons + current top backlog item per lane.
    let mut ctx = Vec::new();
    for (prio, name, goal) in &lanes {
        let outcomes = snapshot["projects"].get(name).cloned().unwrap_or(json!({}));
        let probes = status
            .get("projects")
            .and_then(|p| p.get(name))
            .map(|p| {
                json!({
                    "status": p.get("status").cloned().unwrap_or(Value::Null),
                    "reasons": p.get("reasons").cloned().unwrap_or(json!([])),
                })
            })
            .unwrap_or(json!({}));
        let top_item = std::fs::read_to_string(backlog_path(name))
            .ok()
            .and_then(|c| c.lines().find(|l| l.trim().starts_with("- [ ]")).map(str::to_string));
        ctx.push(json!({
            "lane": name,
            "priority": prio,
            "north_star": goal,
            "outcomes_24h": outcomes,
            "probes": probes,
            "current_top_backlog_item": top_item,
        }));
    }

    let system = "You are the CEO planner of Solomon, a control plane running autonomous \
        improvement lanes over the operator's projects. Given each lane's north star, measured \
        24h outcomes, and probe status, choose ONE concrete, verifiable goal per lane for today. \
        HARD RULES: asmodeus is priority 1 — give it the deepest, most specific goal (its north \
        star is capital velocity: fast, frequent profits across assets and short timeframes; \
        NEVER weaken kill-switch/breaker/capital_guard mechanisms, only tunable thresholds). \
        Growth must be organic/free only — no ad spend, no paid services; any money-out step \
        stays human-gated. Lanes whose north star says GROWTH IS IN SCOPE (public projects) \
        should get an ORGANIC growth goal (README, docs, examples, release notes, showcase \
        content) on days when their measured outcomes are healthy — growth is how public \
        projects scale; never paid channels. Prefer fixing a measured zero (zero posts, zero \
        trades, lane never fired, red probe) over cosmetic work. Goals must be implementable by \
        a coding agent in one iteration and verifiable from files/tests/logs. Reply with STRICT \
        JSON only: \
        {\"lanes\": {\"<lane>\": {\"tier\": \"chore|feature|refactor|architecture\", \
        \"goal\": \"<one sentence>\", \"why\": \"<one sentence>\"}}} — one entry per lane given.";
    let user = serde_json::to_string_pretty(&json!({"date": today, "lanes": ctx}))
        .unwrap_or_default();

    let reply = match ollama_chat(CEO_MODEL, system, &user) {
        Ok(r) => r,
        Err(e) => return json!({"ok": false, "error": format!("llm: {e}")}),
    };
    let parsed = match extract_json(&reply) {
        Some(v) => v,
        None => return json!({"ok": false, "error": "llm reply had no parseable JSON object"}),
    };
    let known: Vec<String> = lanes.iter().map(|(_, n, _)| n.clone()).collect();
    let items = plan_items(&parsed, &known);
    if items.is_empty() {
        return json!({"ok": false, "error": "llm JSON contained no usable lane goals"});
    }

    // Apply: prepend the day item to each UNPLANNED lane's backlog; report + notify.
    let mut applied = Map::new();
    let mut report = format!("# Solomon morning plan — {today}\n\n");
    for (lane, tier, goal, why) in &items {
        if !unplanned.iter().any(|(_, n, _)| n == lane) {
            continue; // already planned today — never double-stack
        }
        let line = format!("- [ ] [{tier}] {goal} {}", ceo_marker(&today));
        let path = backlog_path(lane);
        // DATA-SAFETY (bug-bounty cycle 1, conf 82): distinguish "file absent" (existing = "") from
        // "read failed" (the improver's own mark_backlog_done is mid-rewrite / the file is locked).
        // On a read ERROR we must NOT write — an unwrap_or_default() there truncates the entire
        // backlog to just today's one line, destroying every pending item. Skip the lane this cycle
        // instead; it gets planned next tick. The write is atomic (temp+rename) so a crash mid-write
        // can never leave a half-file, narrowing the RMW race with the improver.
        let existing = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(_) => continue, // read failed — do NOT risk truncating a live backlog
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if proc::atomic_write_bytes(&path, format!("{line}\n{existing}").as_bytes()).is_ok() {
            report.push_str(&format!("## {lane}\n- **goal** [{tier}]: {goal}\n- **why**: {why}\n\n"));
            applied.insert(lane.clone(), json!({"tier": tier, "goal": goal}));
        }
    }
    if applied.is_empty() {
        return json!({"ok": false, "error": "no backlog was writable"});
    }
    let report_path = reports_dir().join(format!("{today}-plan.md"));
    let _ = std::fs::create_dir_all(reports_dir());
    let _ = std::fs::write(&report_path, &report);

    let body: String = items
        .iter()
        .filter(|(l, ..)| applied.contains_key(l))
        .map(|(l, _, g, _)| format!("{l}: {g}"))
        .collect::<Vec<_>>()
        .join("\n");
    let _ = notify::send(&Notice::plan(format!("Solomon morning plan {today}"), body));
    json!({"ok": true, "applied": applied, "report": report_path.to_string_lossy()})
}

/// The per-day idempotence marker appended to every CEO backlog item.
fn ceo_marker(date: &str) -> String {
    format!("(ceo {date})")
}

/// HERE/improver/<name>/backlog.md — the exact file improver::backlog::top_backlog_item reads.
fn backlog_path(name: &str) -> PathBuf {
    paths::here().join("improver").join(name).join("backlog.md")
}

/// HERE/improver/<name>/goal.md — the operator's committed, single-line goal post (the measurable
/// north star the CEO plane steers by). Tracked in git (unlike the gitignored backlog.md churn) so
/// retargeting a lane is a one-line edit the whole plane then optimizes distance toward.
fn goal_post_path(name: &str) -> PathBuf {
    paths::here().join("improver").join(name).join("goal.md")
}

/// Pick the authoritative goal post (pure — unit-tested): the first non-blank, non-`#` line of
/// goal.md (the operator's one-line target), or the repos.json `goal` fallback when goal.md is
/// absent or holds only headings. You cannot prioritize speed toward an unstated target.
pub fn pick_goal_post(goal_md: Option<&str>, fallback: &str) -> String {
    goal_md
        .and_then(|body| {
            body.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
        })
        .unwrap_or_else(|| fallback.trim().to_string())
}

/// Extract the first {...} JSON object from a model reply (pure — tolerates markdown fences and
/// prose around the object; None when nothing parses).
pub fn extract_json(reply: &str) -> Option<Value> {
    let start = reply.find('{')?;
    let end = reply.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str(&reply[start..=end]).ok()
}

/// Flatten to one line and cap at `max` chars, cutting at a WORD boundary (pure — unit-tested).
/// A mid-word chop turns a goal into gibberish the lane then implements literally (observed
/// 2026-07-01: "...records per-class c").
pub fn cap_line(s: &str, max: usize) -> String {
    // \r\n collapses to ONE space (a naive per-char replace would make two).
    let flat: String = s.replace("\r\n", " ").replace(['\r', '\n'], " ").trim().to_string();
    if flat.chars().count() <= max {
        return flat;
    }
    let hard: String = flat.chars().take(max).collect();
    match hard.rfind(' ') {
        Some(i) if i > 0 => hard[..i].trim_end().to_string(),
        _ => hard,
    }
}

/// Normalize the plan JSON into (lane, tier, goal, why) rows (pure — unit-tested). Unknown lanes
/// are dropped; a bad tier degrades to "chore" (the safe smallest-change tier); goals/whys are
/// flattened to one <=600-char line cut at a word boundary; empty goals are dropped.
pub fn plan_items(parsed: &Value, known_lanes: &[String]) -> Vec<(String, String, String, String)> {
    const TIERS: [&str; 4] = ["chore", "feature", "refactor", "architecture"];
    let lanes = parsed
        .get("lanes")
        .and_then(Value::as_object)
        .or_else(|| parsed.as_object()) // tolerate a reply missing the "lanes" wrapper
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for name in known_lanes {
        let entry = match lanes.get(name) {
            Some(e) => e,
            None => continue,
        };
        let goal = cap_line(entry.get("goal").and_then(Value::as_str).unwrap_or(""), 600);
        if goal.is_empty() {
            continue;
        }
        let tier = entry
            .get("tier")
            .and_then(Value::as_str)
            .map(str::to_lowercase)
            .filter(|t| TIERS.contains(&t.as_str()))
            .unwrap_or_else(|| "chore".to_string());
        let why = cap_line(entry.get("why").and_then(Value::as_str).unwrap_or(""), 600);
        out.push((name.clone(), tier, goal, why));
    }
    out
}

/// One chat completion over Ollama Cloud via curl.exe (no HTTP client dependency; TLS handled by
/// the OS curl, same guarded-spawn contract as every other subprocess). The API key rides a
/// curl `-H @file` headers file under runtime/ (gitignored) — never argv, never a log line.
fn ollama_chat(model: &str, system: &str, user: &str) -> Result<String, String> {
    let key = notify::env_value("OLLAMA_API_KEY").ok_or("no OLLAMA_API_KEY in .env")?;
    let rt = paths::here().join("runtime");
    let _ = std::fs::create_dir_all(&rt);
    let req_path = rt.join("_ceo_request.json");
    let hdr_path = rt.join("_ceo_headers.txt");
    let body = json!({
        "model": model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "stream": false,
    });
    std::fs::write(&req_path, serde_json::to_vec(&body).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::write(
        &hdr_path,
        format!("Authorization: Bearer {key}\nContent-Type: application/json\n"),
    )
    .map_err(|e| e.to_string())?;
    let hdr_arg = format!("@{}", hdr_path.display());
    let body_arg = format!("@{}", req_path.display());
    let args = [
        "curl",
        "-s",
        "-m",
        "240",
        "-H",
        hdr_arg.as_str(),
        "-d",
        body_arg.as_str(),
        "https://ollama.com/api/chat",
    ];
    let r = proc::run(&args, None, Some(Duration::from_secs(250))).map_err(|e| e.to_string())?;
    if !r.ok() {
        return Err(format!("curl exit {}: {}", r.code, r.stderr.trim()));
    }
    let v: Value = serde_json::from_str(r.stdout.trim())
        .map_err(|_| format!("non-JSON response: {}", r.stdout.chars().take(200).collect::<String>()))?;
    // Ollama native shape first, OpenAI-compatible shape second.
    if let Some(c) = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_str) {
        if !c.trim().is_empty() {
            return Ok(c.to_string());
        }
    }
    if let Some(c) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        if !c.trim().is_empty() {
            return Ok(c.to_string());
        }
    }
    Err(format!(
        "no message content in response: {}",
        r.stdout.chars().take(200).collect::<String>()
    ))
}

// --------------------------------------------------------------------------- //
// evening summary
// --------------------------------------------------------------------------- //

/// Compose + deliver the evening summary. DETERMINISTIC — verified outcomes only, never lane
/// claims. Returns {"ok": bool, "report": path, "urgent": bool} (also used by `solomon report`).
pub fn evening_summary() -> Value {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let snapshot = ledger::snapshot();
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let incidents = recent_incidents(Utc::now());

    let (markdown, flags, urgent) = render_report(&snapshot, &status, &incidents, &today);

    let _ = std::fs::create_dir_all(reports_dir());
    let report_path = reports_dir().join(format!("{today}.md"));
    if std::fs::write(&report_path, &markdown).is_err() {
        return json!({"ok": false, "error": format!("cannot write {}", report_path.display())});
    }
    ledger::append_daily(&snapshot);

    let fleet = ops::outcomes::payload_summary(&status);
    let mut body = format!("fleet: {fleet}");
    for f in flags.iter().take(10) {
        body.push('\n');
        body.push_str(f);
    }
    let notice = if urgent {
        Notice::red(format!("Solomon evening report {today} — ATTENTION"), body)
    } else {
        Notice::report(format!("Solomon evening report {today}"), body)
    };
    let _ = notify::send(&notice);
    json!({"ok": true, "report": report_path.to_string_lossy(), "urgent": urgent, "flags": flags})
}

/// CEO day-state for the dashboard: today's gate state, due hours, and today's plan/report
/// markdown (empty strings when not yet written). Read-only, cheap.
pub fn ceo_status() -> Value {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let st = read_state();
    let read_md = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    json!({
        "today": today,
        "plan_hour": PLAN_HOUR,
        "summary_hour": SUMMARY_HOUR,
        "plan": st.get("plan").cloned().unwrap_or(json!({})),
        "summary": st.get("summary").cloned().unwrap_or(json!({})),
        "plan_md": read_md(reports_dir().join(format!("{today}-plan.md"))),
        "report_md": read_md(reports_dir().join(format!("{today}.md"))),
    })
}

/// The last 24 h of incident transitions from runtime/_incidents.jsonl (lenient parse, capped 20).
pub fn recent_incidents(now: chrono::DateTime<Utc>) -> Vec<Value> {
    let cutoff = now - chrono::Duration::seconds(86_400);
    let content = std::fs::read_to_string(ops::outcomes::incidents_path()).unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for line in content.lines() {
        let rec: Value = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let ts = rec.get("ts").cloned().unwrap_or(Value::Null);
        if let Some(t) = crate::ops::probe::parse_ts(&ts) {
            if t >= cutoff {
                out.push(rec);
            }
        }
    }
    let keep = out.len().saturating_sub(20);
    out.split_off(keep)
}

/// Render the report (pure — unit-tested). Returns (markdown, flag lines, urgent).
///
/// The flag rules are the post-mortem, encoded:
///   - posts_24h == 0                → "ZERO posts" (the 5-day Sover gap)
///   - posts_missing_url_24h > 0     → publish claims without URL evidence (the June 30 TikTok)
///   - live_trades_24h == 0          → zero live trades (finance-tracked projects)
///   - equity_delta_24h == 0.0       → equity flat (the $168.97 flatline)
///   - iterations_24h == 0           → lane never fired (the daedalus-trainer failure mode)
///   - any project status red        → urgent
pub fn render_report(
    snapshot: &Value,
    status: &Value,
    incidents: &[Value],
    date: &str,
) -> (String, Vec<String>, bool) {
    let mut md = format!("# Solomon evening report — {date}\n\n");
    md.push_str(&format!("fleet: {}\n\n", ops::outcomes::payload_summary(status)));
    let mut flags: Vec<String> = Vec::new();
    let mut urgent = false;

    let projects = snapshot
        .get("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<(&String, &Value)> = projects.iter().collect();
    items.sort_by_key(|(_, p)| p.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));

    for (name, p) in items {
        md.push_str(&format!(
            "## {name} (priority {})\n",
            p.get("priority").and_then(Value::as_i64).unwrap_or(0)
        ));
        // lane activity — every project has this
        let iters = p.get("iterations_24h").and_then(Value::as_i64).unwrap_or(0);
        let shipped = p.get("shipped_24h").and_then(Value::as_i64).unwrap_or(0);
        md.push_str(&format!("- lane: {iters} iterations / {shipped} shipped (24h)\n"));
        if iters == 0 {
            let f = format!("⚠ {name}: lane never fired in 24h");
            md.push_str(&format!("- {f}\n"));
            flags.push(f);
        }
        // posts — only when the collector ran
        if let Some(posts) = p.get("posts_24h") {
            if let Some(n) = posts.as_i64() {
                md.push_str(&format!(
                    "- posts: {n} published (24h), last at {}\n",
                    p.get("last_post_at").and_then(Value::as_str).unwrap_or("never")
                ));
                if n == 0 {
                    let f = format!("⚠ {name}: ZERO posts in 24h");
                    md.push_str(&format!("- {f}\n"));
                    flags.push(f);
                }
                let missing = p.get("posts_missing_url_24h").and_then(Value::as_i64).unwrap_or(0);
                if missing > 0 {
                    let f = format!("⚠ {name}: {missing} publish claim(s) without a URL");
                    md.push_str(&format!("- {f}\n"));
                    flags.push(f);
                }
            } else {
                let f = format!("⚠ {name}: post registry unobservable");
                md.push_str(&format!("- {f}\n"));
                flags.push(f);
            }
        }
        // finance — only when the collector ran
        if let Some(eq) = p.get("equity_usd") {
            let delta = p.get("equity_delta_24h").and_then(Value::as_f64);
            md.push_str(&format!(
                "- equity: ${} (Δ24h: {})\n",
                eq,
                delta.map(|d| format!("{d:+.2}")).unwrap_or_else(|| "?".into())
            ));
            let trades = p.get("live_trades_24h").and_then(Value::as_i64);
            let fills = p.get("fills_24h").and_then(Value::as_i64);
            md.push_str(&format!(
                "- live trades 24h: {} | venue fills 24h: {}\n",
                trades.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                fills.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            ));
            if trades == Some(0) {
                let f = format!("⚠ {name}: zero live trades in 24h");
                md.push_str(&format!("- {f}\n"));
                flags.push(f);
            }
            if delta == Some(0.0) {
                let f = format!("⚠ {name}: equity flat over 24h");
                md.push_str(&format!("- {f}\n"));
                flags.push(f);
            }
        }
        // probe reasons
        if let Some(proj) = status.get("projects").and_then(|s| s.get(name.as_str())) {
            let pstat = proj.get("status").and_then(Value::as_str).unwrap_or("?");
            if pstat == "red" {
                urgent = true;
            }
            if pstat != "green" {
                let reasons = proj
                    .get("reasons")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .unwrap_or_default();
                md.push_str(&format!("- probes: {} — {}\n", pstat.to_uppercase(), reasons));
            }
        }
        md.push('\n');
    }

    if !incidents.is_empty() {
        md.push_str("## incidents (24h)\n");
        for i in incidents {
            md.push_str(&format!(
                "- {} {} {}\n",
                i.get("ts").and_then(Value::as_str).unwrap_or("?"),
                i.get("probe_id").and_then(Value::as_str).unwrap_or("?"),
                i.get("event").and_then(Value::as_str).unwrap_or("?"),
            ));
        }
        md.push('\n');
    }

    if !flags.is_empty() {
        urgent = true;
    }
    (md, flags, urgent)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- day gate (pure) --------
    #[test]
    fn should_attempt_gates_on_hour_done_and_attempts() {
        let empty = json!({});
        // before the due hour: never
        assert!(!should_attempt(&empty, "2026-07-02", 6, 7));
        // at/after the due hour with a clean section: yes
        assert!(should_attempt(&empty, "2026-07-02", 7, 7));
        // already done today: no
        let done = json!({"done": "2026-07-02"});
        assert!(!should_attempt(&done, "2026-07-02", 9, 7));
        // done YESTERDAY: due again today
        let done_y = json!({"done": "2026-07-01"});
        assert!(should_attempt(&done_y, "2026-07-02", 9, 7));
        // attempt cap reached today: no
        let capped = json!({"attempts_date": "2026-07-02", "attempts": 3});
        assert!(!should_attempt(&capped, "2026-07-02", 9, 7));
        // stale attempts from yesterday reset
        let stale = json!({"attempts_date": "2026-07-01", "attempts": 3});
        assert!(should_attempt(&stale, "2026-07-02", 9, 7));
    }

    #[test]
    fn record_attempt_success_and_failure() {
        let sec = json!({"attempts_date": "2026-07-02", "attempts": 1});
        // success stamps done (attempts irrelevant afterwards)
        assert_eq!(record_attempt(&sec, "2026-07-02", true), json!({"done": "2026-07-02"}));
        // failure increments today's count
        let f = record_attempt(&sec, "2026-07-02", false);
        assert_eq!(f["attempts"], json!(2));
        assert_eq!(f["attempts_date"], json!("2026-07-02"));
        // failure on a NEW day starts from 1
        let f2 = record_attempt(&sec, "2026-07-03", false);
        assert_eq!(f2["attempts"], json!(1));
    }

    // -------- extract_json (pure) --------
    #[test]
    fn extract_json_tolerates_fences_and_prose() {
        let fenced = "Here is the plan:\n```json\n{\"lanes\": {\"a\": {\"goal\": \"x\"}}}\n```\nDone.";
        assert_eq!(
            extract_json(fenced).unwrap()["lanes"]["a"]["goal"],
            json!("x")
        );
        assert!(extract_json("no json here").is_none());
        assert!(extract_json("{broken").is_none());
    }

    // -------- cap_line word-boundary truncation (pure) --------
    #[test]
    fn cap_line_cuts_at_word_boundary() {
        assert_eq!(cap_line("short", 10), "short");
        // "alpha beta g" (12 chars) -> cut back to the last full word
        assert_eq!(cap_line("alpha beta gamma delta", 12), "alpha beta");
        // newlines flatten before capping
        assert_eq!(cap_line("a\r\nb\nc", 100), "a b c");
        // no space inside the window -> hard cut (never empty)
        assert_eq!(cap_line("abcdefghij", 5), "abcde");
    }

    // -------- plan_items normalization (pure) --------
    #[test]
    fn plan_items_normalizes_and_filters() {
        let known = vec!["asmodeus".to_string(), "sover".to_string()];
        let parsed = json!({"lanes": {
            "asmodeus": {"tier": "FEATURE", "goal": "line1\nline2", "why": "because"},
            "sover": {"tier": "not-a-tier", "goal": "  post more  "},
            "unknown_lane": {"tier": "chore", "goal": "dropped"},
            "empty": {"tier": "chore", "goal": ""}
        }});
        let items = plan_items(&parsed, &known);
        assert_eq!(items.len(), 2);
        // tier lowercased; newlines flattened
        assert_eq!(items[0], ("asmodeus".into(), "feature".into(), "line1 line2".into(), "because".into()));
        // bad tier degrades to chore; goal trimmed; missing why is ""
        assert_eq!(items[1].1, "chore");
        assert_eq!(items[1].2, "post more");
        // a reply missing the "lanes" wrapper still parses
        let bare = json!({"asmodeus": {"tier": "chore", "goal": "g"}});
        assert_eq!(plan_items(&bare, &known).len(), 1);
    }

    // -------- render_report flags (pure — the post-mortem, encoded) --------
    #[test]
    fn render_report_flags_zero_outcomes_loudly() {
        let snapshot = json!({"projects": {
            "asmodeus": {"priority": 1, "iterations_24h": 3, "shipped_24h": 1,
                          "equity_usd": 168.97, "equity_delta_24h": 0.0,
                          "live_trades_24h": 0, "fills_24h": 5},
            "sover": {"priority": 2, "iterations_24h": 2, "shipped_24h": 0,
                       "posts_24h": 0, "posts_missing_url_24h": 0, "last_post_at": "2026-06-24T12:24:07Z"},
            "daedulus": {"priority": 3, "iterations_24h": 0, "shipped_24h": 0},
        }});
        let status = json!({"projects": {
            "asmodeus": {"priority": 1, "status": "green", "reasons": []},
            "sover": {"priority": 2, "status": "red", "worst_probe": "publish_recency",
                       "reasons": ["publish_recency=red (age 100000s)"]},
            "daedulus": {"priority": 3, "status": "yellow", "reasons": ["heartbeat_fresh=yellow (unobservable)"]},
        }});
        let (md, flags, urgent) = render_report(&snapshot, &status, &[], "2026-07-02");
        assert!(urgent);
        // every post-mortem failure mode gets its loud flag:
        assert!(flags.iter().any(|f| f.contains("sover: ZERO posts")));
        assert!(flags.iter().any(|f| f.contains("asmodeus: zero live trades")));
        assert!(flags.iter().any(|f| f.contains("asmodeus: equity flat")));
        assert!(flags.iter().any(|f| f.contains("daedulus: lane never fired")));
        // sections render priority-ordered with the fleet line up top
        assert!(md.starts_with("# Solomon evening report — 2026-07-02"));
        let a = md.find("## asmodeus").unwrap();
        let s = md.find("## sover").unwrap();
        let d = md.find("## daedulus").unwrap();
        assert!(a < s && s < d);
        // red probe reasons surface in the section
        assert!(md.contains("publish_recency=red"));
    }

    #[test]
    fn render_report_all_green_is_calm() {
        let snapshot = json!({"projects": {
            "dotz": {"priority": 4, "iterations_24h": 5, "shipped_24h": 2},
        }});
        let status = json!({"projects": {"dotz": {"priority": 4, "status": "green"}}});
        let (md, flags, urgent) = render_report(&snapshot, &status, &[], "2026-07-02");
        assert!(!urgent);
        assert!(flags.is_empty());
        assert!(md.contains("5 iterations / 2 shipped"));
    }

    // -------- ceo marker / backlog idempotence key --------
    #[test]
    fn ceo_marker_is_dated() {
        assert_eq!(ceo_marker("2026-07-02"), "(ceo 2026-07-02)");
    }

    // -------- goal-post precedence (pure) --------
    #[test]
    fn pick_goal_post_prefers_goal_md_first_line_over_fallback() {
        // the first non-heading, non-blank line of goal.md wins over the repos.json fallback
        let md = "# asmodeus goal post (edit this line)\nGrow capital velocity: more live fills/day.\n";
        assert_eq!(
            pick_goal_post(Some(md), "old repos.json goal"),
            "Grow capital velocity: more live fills/day."
        );
        // a goal.md with only headings/blank lines falls back
        assert_eq!(pick_goal_post(Some("# heading only\n\n"), "fallback"), "fallback");
        // absent goal.md falls back (trimmed)
        assert_eq!(pick_goal_post(None, "  fallback  "), "fallback");
        // neither present -> empty (lane stays dormant; the plane never invents work)
        assert_eq!(pick_goal_post(None, ""), "");
    }
}
