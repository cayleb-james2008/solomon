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

// Deterministic growth sub-planes wired into `tick`/`morning_plan`: fleet ROI ranking (allocate),
// bounded reversible interval tightening (scale), and the sover produce/post profit boost.
pub mod allocate;
pub mod scale;
pub mod sover_boost;

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

    // DETERMINISTIC ops-RED graft (v2, close-the-loop): every sweep, ensure each project whose
    // OUTCOME probe is RED carries a targeted [ops-auto:<probe>] fix item atop its backlog —
    // idempotent, no-LLM, cheap. This is what closes the open loop the LLM morning plan left:
    // sover can be RED (no posts 37 h) yet, with only a once/day LLM plan, no fix is ever queued.
    ops_red_backlog_graft();

    // GROWTH GRAFTS (every sweep, deterministic + bounded): (1) hygiene — report-only off-base/dirty
    // managed trees grafted into the lane backlog + a loud page on a NEW stranded off-base pair;
    // (2) scale — one bounded reversible interval tightening across the fleet; (3) sover_boost — one
    // extra produce/post one-shot to close the posts/day gap. Each is wrapped in its OWN catch_unwind
    // (mirroring watchdog's per-graft isolation) so one graft's panic can't skip the next, and the
    // scale/boost decisions read the SAME fresh snapshot+rollup morning_plan/ops_red_backlog_graft use.
    let snapshot = ledger::snapshot();
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let _ = std::panic::catch_unwind(hygiene_backlog_graft);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scale::maybe_scale_lanes(&snapshot, &status)
    }));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sover_boost::maybe_boost(&snapshot, &status)
    }));

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

    // Fleet ALLOCATION context (deterministic): the ROI ranking + a DRY-RUN of the scale/boost
    // decisions (computed here, NEVER executed — execution stays in the tick grafts). Injected into
    // both each lane's ctx (its leverage score) and a fleet-level object the report renders.
    let ranking = allocate::rank_lanes(&snapshot, &status);
    let leverage: std::collections::HashMap<&str, f64> =
        ranking.iter().map(|(n, s, _)| (n.as_str(), *s)).collect();
    let (scale_actions, holds) = allocation_dry_run(&snapshot, &status);
    let top_lane = ranking.first().map(|(n, ..)| n.clone());

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
        let velocity = velocity_context(&outcomes, goal);
        ctx.push(json!({
            "lane": name,
            "priority": prio,
            "north_star": goal,
            "outcomes_24h": outcomes,
            "velocity": velocity,
            "leverage": leverage.get(name.as_str()).copied().unwrap_or(0.0),
            "probes": probes,
            "current_top_backlog_item": top_item,
        }));
    }
    // The fleet allocation object handed to the model alongside the per-lane ctx.
    let allocation = json!({
        "top_lane": top_lane,
        "ranking": ranking.iter().map(|(n, s, why)| json!([n, s, why])).collect::<Vec<_>>(),
        "scale_actions": scale_actions,
        "holds": holds,
    });

    let system = "You are the CEO planner of Solomon, a growth executive running autonomous \
        improvement lanes over the operator's projects — an AI that grows the company while the \
        operator sleeps. Given each lane's north star, measured 24h outcomes, its velocity \
        (current throughput vs target/trend), and probe status, choose ONE concrete, verifiable \
        goal per lane for today. GROWTH IS THE JOB: every lane's daily goal must measurably \
        ADVANCE its north-star velocity metric — shrink the days to its next follower / \
        engagement / trade / fill / user / monetization milestone — moving a real number in the \
        velocity object FORWARD. A healthy lane is NOT done: push it to its next milestone, never \
        give it cosmetic busywork. HARD RULES: asmodeus is priority 1 — give it the deepest, most \
        specific goal (its north star is capital velocity: fast, frequent profits across assets \
        and short timeframes; NEVER weaken kill-switch/breaker/capital_guard mechanisms, only \
        tunable thresholds). Growth must be organic/free only — no ad spend, no paid services; \
        any money-out step stays human-gated. Lanes whose north star says GROWTH IS IN SCOPE \
        (public projects) grow ORGANICALLY (README, docs, examples, release notes, showcase \
        content, throughput) — growth is how public projects scale; never paid channels. Fix a \
        measured RED outcome FIRST (zero posts, zero trades, lane never fired, red probe) — a \
        dead engine can't grow — but a lane with a healthy engine STILL gets pushed forward \
        toward its next growth milestone, not idled. Growth must never become reward-hacking, \
        off-brand, or unbounded. Goals must be implementable by a coding agent in one iteration \
        and verifiable from files/tests/logs. You are ALSO given a deterministic fleet `allocation` \
        (each lane's leverage score, the ROI ranking, and the dry-run scale/boost actions): write \
        ONE plain-English sentence naming where the fleet's marginal effort should go today (the \
        top-ranked lane, the scale/boost move, and what is held and why). Reply with STRICT JSON \
        only: {\"lanes\": {\"<lane>\": {\"tier\": \"chore|feature|refactor|architecture\", \
        \"goal\": \"<one sentence>\", \"why\": \"<one sentence>\"}}, \
        \"fleet\": {\"allocation\": \"<one sentence>\"}} — one lanes entry per lane given.";
    let user = serde_json::to_string_pretty(&json!({"date": today, "lanes": ctx, "allocation": allocation}))
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

    // Apply: prepend the day item to each UNPLANNED lane's backlog; report + notify. The report is
    // the founder's morning email (Polsia "while you slept"): (1) OVERNIGHT — verified, from the
    // ledger, never fabricated; (2) TODAY'S PLAN — the growth goals just chosen; (3) NEXT — the one
    // thing to watch. Section (1) is computed deterministically before the plan loop.
    let incidents = recent_incidents(Utc::now());
    let mut applied = Map::new();
    let mut report = format!("# Solomon morning plan — {today}\n\n");
    report.push_str(&overnight_section(&snapshot, &incidents));
    // (1.5) ALLOCATION — deterministic ROI ranking + dry-run scale/boost moves, then the model's
    // one-line fleet-allocation sentence (falling back to a deterministic top-lane line, never
    // fabricated, when the model omits it).
    report.push_str(&allocation_section(&ranking, &scale_actions, &holds));
    let fleet_line = fleet_allocation(&parsed)
        .unwrap_or_else(|| match ranking.iter().find(|(_, s, _)| *s > 0.0) {
            Some((n, ..)) => format!("Pour marginal effort into {n} (top ROI); hold real-money + non-green lanes."),
            None => "No scalable lane today — hold the fleet and fix red engines first.".to_string(),
        });
    report.push_str(&format!("_{fleet_line}_\n\n"));
    report.push_str("## TODAY'S PLAN\n\n");
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
    report.push_str(&format!("## NEXT\n\n{}\n", next_watch(&snapshot, &status)));
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

// --------------------------------------------------------------------------- //
// deterministic ops-RED backlog graft (close-the-loop)
// --------------------------------------------------------------------------- //

/// The OUTCOME probes that count as a real failing product signal — a RED here means the actual
/// posting / trade / app path is broken, not merely the test gate. Internal/support probes
/// (heartbeat_fresh, binary_current, monitor_fresh, auth_health, cdp_alive, equity_fresh,
/// outcome_streak, ...) are DELIBERATELY excluded: they measure liveness/plumbing, not the outcome.
const OPS_OUTCOME_PROBES: [&str; 6] = [
    "publish_recency",
    "produce_recency",
    "fills_recency",
    "lane_freshness_instagram",
    "lane_freshness_tiktok",
    "lane_freshness_youtube",
];
// `process` (App.exe not running) is DELIBERATELY excluded: an app being down is a restart/redeploy
// job for the watchdog + deploy plane, not a code-fix the lane agent should chase — filing an
// "ops-auto:process" fix item would mislabel a down process as a code failure (and stack redundantly
// with publish/produce/lane_freshness, which all go RED together when the app is down).

/// The stable per-(project, probe) idempotence marker embedded in every ops-auto backlog line.
fn ops_marker(probe: &str) -> String {
    format!("[ops-auto:{probe}]")
}

/// Every ops sweep: for each project whose OUTCOME probe is RED (per `OPS_OUTCOME_PROBES`), ensure a
/// targeted `[ops-auto:<probe>]` fix item sits atop `improver/<name>/backlog.md`, IDEMPOTENTLY —
/// one OPEN item per (project, probe). Deterministic (no LLM), read fresh from runtime/ops_status.json.
///
/// This closes the open loop: the ops plane was OBSERVATION-ONLY — the improver reads backlog.md and
/// never sees ops, and the once/day LLM morning plan can leave a RED lane with no queued fix. Here a
/// RED outcome DETERMINISTICALLY forces a fix into the lane backlog every sweep, without flooding it.
fn ops_red_backlog_graft() {
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let projects = match status.get("projects").and_then(Value::as_object) {
        Some(p) => p,
        None => return,
    };
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    for (name, proj) in projects {
        // Which OUTCOME probes are RED for this project? (per-probe status map, whitelist-filtered)
        let probes = match proj.get("probes").and_then(Value::as_object) {
            Some(p) => p,
            None => continue,
        };
        for probe in OPS_OUTCOME_PROBES {
            if probes.get(probe).and_then(Value::as_str) != Some("red") {
                continue;
            }
            // The reason/detail for this probe from the rollup reasons list ("<probe>=red (<detail>)").
            let detail = red_probe_detail(proj, probe);
            ensure_ops_item(name, probe, &detail, &today);
        }
    }
}

/// Pull the human detail for a RED probe out of a project's `reasons` list (each entry is
/// `"<id>=<status> (<detail>)"`, produced by ops::outcomes::sweep_project). Falls back to the bare
/// probe name when no matching reason is present. Pure — unit-tested.
fn red_probe_detail(proj: &Value, probe: &str) -> String {
    let prefix = format!("{probe}=");
    proj.get("reasons")
        .and_then(Value::as_array)
        .and_then(|rs| {
            rs.iter()
                .filter_map(Value::as_str)
                .find(|r| r.starts_with(&prefix))
                .map(str::to_string)
        })
        .unwrap_or_else(|| probe.to_string())
}

/// Idempotence predicate (pure — unit-tested): true iff the backlog already carries an OPEN
/// (`- [ ]`) line with this (project, probe) `[ops-auto:<probe>]` marker. When true we prepend
/// NOTHING — one open item per (project, probe), so a persisting RED never floods the backlog every
/// 2-min sweep. A `- [x]` (done) line with the marker does NOT count as open — the lane goes RED
/// again -> a fresh item is queued.
fn has_open_ops_item(existing: &str, probe: &str) -> bool {
    has_open_marker(existing, &ops_marker(probe))
}

/// Shared idempotence predicate (pure — unit-tested): true iff any OPEN (`- [ ]`) backlog line
/// contains `marker`. A `- [x]` (done) line does NOT count as open — the condition recurs -> a
/// fresh item is queued. Both the ops-RED graft and the hygiene graft key off this.
fn has_open_marker(existing: &str, marker: &str) -> bool {
    existing
        .lines()
        .any(|l| l.trim().starts_with("- [ ]") && l.contains(marker))
}

/// The exact backlog line for a RED outcome probe (pure — unit-tested). Carries the stable
/// `[ops-auto:<probe>]` idempotence marker and a `[reliability]` intent tag (not a known improver
/// tier, so strip_tier leaves it in the text — deliberate; the item reads as reliability work).
fn ops_item_line(probe: &str, detail: &str, today: &str) -> String {
    format!(
        "- [ ] [reliability]{} {probe} has been RED ({detail}) — the real outcome is failing, \
         not the test gate; diagnose and fix the actual posting/trade/app path. (ops-auto {today})",
        ops_marker(probe)
    )
}

/// Idempotently prepend ONE `[ops-auto:<probe>]` fix item to a lane's backlog. If an OPEN line
/// already carries `[ops-auto:<probe>]`, do NOTHING — this MUST NOT flood the backlog every 2-min
/// sweep. Reuses the same atomic-prepend + read-error safety contract as morning_plan (a READ ERROR
/// — the improver mid-rewrite / a locked file — skips the lane rather than risk truncating a live
/// backlog; file-absent starts from empty).
fn ensure_ops_item(name: &str, probe: &str, detail: &str, today: &str) {
    let path = backlog_path(name);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return, // read failed — do NOT risk truncating a live backlog
    };
    if has_open_ops_item(&existing, probe) {
        return;
    }
    let line = ops_item_line(probe, detail, today);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_bytes(&path, format!("{line}\n{existing}").as_bytes());
}

// --------------------------------------------------------------------------- //
// deterministic repo-HYGIENE backlog graft (report-only; parallels ops-RED graft)
// --------------------------------------------------------------------------- //

/// The stable per-(repo, hygiene issue) idempotence marker embedded in every hygiene-auto line.
fn hygiene_marker(issue: crate::hygiene::HygieneIssue) -> String {
    format!("[hygiene-auto:{}]", issue.slug())
}

/// The exact backlog line for a hygiene issue (pure — unit-tested). Carries the stable
/// `[hygiene-auto:<slug>]` marker + the `[reliability]` intent tag (not a known improver tier, so
/// strip_tier leaves it in the text — deliberate, exactly like the ops-RED line). REPORT-ONLY
/// wording: the item asks the lane to LAND/return the branch or commit/revert — never to discard.
fn hygiene_item_line(issue: crate::hygiene::HygieneIssue, hyg: &Value, today: &str) -> String {
    use crate::hygiene::HygieneIssue;
    let cur = hyg.get("current").and_then(Value::as_str).unwrap_or("");
    let base = hyg.get("base").and_then(Value::as_str).unwrap_or("");
    match issue {
        HygieneIssue::OffBase => format!(
            "- [ ] [reliability]{} clean up: repo is off-base on '{cur}' (base '{base}') with no \
             live loop — land the branch into '{base}' or drop it and return to '{base}'; do NOT \
             discard uncommitted work without checking. (hygiene-auto {today})",
            hygiene_marker(issue)
        ),
        HygieneIssue::Dirty => format!(
            "- [ ] [reliability]{} clean up: the worktree has uncommitted changes to TRACKED files \
             — commit them on a branch or revert them; the tree must be clean between iterations. \
             (hygiene-auto {today})",
            hygiene_marker(issue)
        ),
    }
}

/// Idempotently prepend ONE `[hygiene-auto:<slug>]` item to a lane's backlog — one OPEN item per
/// (repo, issue). Clones ensure_ops_item's atomic-prepend + read-error-skip contract (a READ ERROR
/// skips the lane rather than risk truncating a live backlog; file-absent starts from empty).
fn ensure_hygiene_item(name: &str, issue: crate::hygiene::HygieneIssue, hyg: &Value, today: &str) {
    let path = backlog_path(name);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return, // read failed — do NOT risk truncating a live backlog
    };
    if has_open_marker(&existing, &hygiene_marker(issue)) {
        return;
    }
    let line = hygiene_item_line(issue, hyg, today);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_bytes(&path, format!("{line}\n{existing}").as_bytes());
}

/// HERE/runtime/hygiene_status.json — the small dedupe + dashboard map for the hygiene graft.
/// Shape: {"seen": {"<name>:<slug>": "<first_ts>"}, "repos": {"<name>": {"issues":[...], "detail": "..."}}}.
fn hygiene_status_path() -> PathBuf {
    paths::here().join("runtime").join("hygiene_status.json")
}

/// Every ops sweep: for each explicit repos.json repo, scan its git hygiene (report-only) and, for
/// each issue, ensure a `[hygiene-auto:<slug>]` item sits atop its backlog — IDEMPOTENTLY, one OPEN
/// item per (repo, issue). Deterministic (no LLM). On a NEWLY-SEEN off_base pair (a stranded branch,
/// possibly a live-money one), page the operator LOUDLY immediately rather than waiting for the
/// evening report — deduped via runtime/hygiene_status.json so a standing off_base pages exactly once.
///
/// KEYSTONE-safe: hygiene::scan_repo is report-only (imports NO destructive helper); this graft only
/// WRITES a backlog goal + a status file + a page — it never touches a managed working tree.
fn hygiene_backlog_graft() {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let now_ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Prior dedupe map ("<name>:<slug>" -> first-seen ts). Absent/garbage -> empty (fail-open: a
    // first sweep after a wipe simply re-pages, never silently drops a real stranded branch).
    let prev: Value = std::fs::read(hygiene_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let prev_seen = prev.get("seen").and_then(Value::as_object).cloned().unwrap_or_default();

    let mut seen = Map::new(); // this sweep's live "<name>:<slug>" -> first_ts (drops resolved keys)
    let mut repos = Map::new(); // per-repo {issues:[slug...], detail} for the dashboard
    for r in crate::control::registry::read_repos_json() {
        let name = crate::control::paths::repo_name(&r);
        if name.is_empty() {
            continue;
        }
        let (issues, hyg) = crate::hygiene::scan_repo(&r);
        if issues.is_empty() {
            continue;
        }
        let cur = hyg.get("current").and_then(Value::as_str).unwrap_or("");
        let base = hyg.get("base").and_then(Value::as_str).unwrap_or("");
        let slugs: Vec<Value> = issues.iter().map(|i| json!(i.slug())).collect();
        repos.insert(
            name.clone(),
            json!({"issues": slugs, "detail": format!("on '{cur}' (base '{base}')")}),
        );
        for issue in &issues {
            ensure_hygiene_item(&name, *issue, &hyg, &today);
            // Carry the first-seen ts forward (persist the ORIGINAL timestamp for a standing issue).
            let key = format!("{name}:{}", issue.slug());
            let first_ts = prev_seen
                .get(&key)
                .and_then(Value::as_str)
                .unwrap_or(now_ts.as_str())
                .to_string();
            let newly_seen = !prev_seen.contains_key(&key);
            seen.insert(key, json!(first_ts));
            // LOUD page ONLY on a newly-seen OFF-BASE pair (a stranded, possibly live-money branch
            // shouldn't wait for the evening report). Dirty-only churn is left to the evening notice.
            if newly_seen && *issue == crate::hygiene::HygieneIssue::OffBase {
                let _ = notify::send(&Notice::red(
                    format!("Solomon: {name} repo off-base"),
                    format!(
                        "on '{cur}' (base '{base}') — no live loop; may be deliberate operator work. \
                         Reported, NOT auto-touched."
                    ),
                ));
            }
        }
    }

    // Persist the pruned dedupe + dashboard map (atomic; best-effort — a write failure just re-pages
    // next sweep, never a false silence).
    let out = json!({"seen": seen, "repos": repos});
    if let Some(parent) = hygiene_status_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&hygiene_status_path(), &out);
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

/// The model's one-line fleet-allocation sentence from `parsed["fleet"]["allocation"]` (pure —
/// unit-tested). Flattened + word-boundary capped at 300 (like the other cap helpers). None when
/// absent/blank so the caller can fall back to a deterministic top-lane line (never fabricated).
pub fn fleet_allocation(parsed: &Value) -> Option<String> {
    let s = parsed
        .get("fleet")
        .and_then(|f| f.get("allocation"))
        .and_then(Value::as_str)?;
    let capped = cap_line(s, 300);
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

/// Render the deterministic "## ALLOCATION" report block (pure — unit-tested): the ROI ranking as a
/// small table, then the dry-run SCALE/BOOST actions and HOLD lines. An EMPTY ranking renders an
/// honest "no scalable lanes" note (never a fabricated row). `scale_actions`/`holds` are already
/// rendered one-liners (e.g. "SCALE sover 120->90s (behind 1/3 posts, green)", "HOLD asmodeus: real-money").
pub fn allocation_section(
    ranking: &[(String, f64, String)],
    scale_actions: &[String],
    holds: &[String],
) -> String {
    let mut md = String::from("## ALLOCATION\n\n");
    if ranking.is_empty() {
        md.push_str("- no scalable lanes (nothing green + behind to pour marginal effort into)\n");
    } else {
        md.push_str("| lane | leverage | why |\n|---|---|---|\n");
        for (name, score, why) in ranking {
            md.push_str(&format!("| {name} | {score:.2} | {why} |\n"));
        }
    }
    if !scale_actions.is_empty() || !holds.is_empty() {
        md.push('\n');
        for a in scale_actions {
            md.push_str(&format!("- {a}\n"));
        }
        for h in holds {
            md.push_str(&format!("- {h}\n"));
        }
    }
    md.push('\n');
    md
}

/// DRY-RUN the scale + boost decisions across the fleet WITHOUT executing (execution stays in the
/// tick grafts): returns (scale_actions, holds) as rendered one-liners for the ALLOCATION block.
/// The dry-run assumes the cooldown has elapsed — it previews what health+velocity+config WOULD
/// permit; the real grafts still gate on the per-lane cooldown marker. A real-money lane (equity in
/// its outcomes) is always a HOLD; a scale-opt-in lane that is not green is a HOLD; a green+behind
/// scale-opt-in lane that would tighten is a SCALE; sover additionally previews a produce/post BOOST.
fn allocation_dry_run(snapshot: &Value, status: &Value) -> (Vec<String>, Vec<String>) {
    let mut scale_actions: Vec<String> = Vec::new();
    let mut holds: Vec<String> = Vec::new();
    for repo in crate::control::registry::read_repos_json() {
        let name = paths::repo_name(&repo);
        if name.is_empty() {
            continue;
        }
        let outcomes = snapshot["projects"].get(&name).cloned().unwrap_or(json!({}));
        let rollup = status
            .get("projects")
            .and_then(|p| p.get(&name))
            .cloned()
            .unwrap_or(json!({}));
        let real_money = allocate::is_real_money(&outcomes);
        if real_money {
            holds.push(format!("HOLD {name}: real-money"));
            continue; // never scaled from here — money-out stays human-gated
        }
        let north_star = repo.get("goal").and_then(Value::as_str).unwrap_or("");
        let velocity = velocity_context(&outcomes, north_star);
        let green = rollup.get("status").and_then(Value::as_str) == Some("green")
            && rollup.get("healthy").and_then(Value::as_bool) == Some(true);

        // scale dry-run (opt-in lanes only).
        if let Some(cfg) = scale::scale_cfg(&repo) {
            let baseline = crate::control::registry::project_interval(&repo);
            match scale::next_interval(Some(cfg), baseline, baseline, &velocity, green, true) {
                Some(next) if next < baseline => {
                    let m = velocity.get("metric").and_then(Value::as_str).unwrap_or("throughput");
                    let cur = velocity.get("current").and_then(Value::as_i64).unwrap_or(0);
                    let tgt = velocity.get("target").and_then(Value::as_i64).unwrap_or(0);
                    scale_actions.push(format!(
                        "SCALE {name} {baseline}->{next}s (behind {cur}/{tgt} {m}, green)"
                    ));
                }
                _ if !green => holds.push(format!("HOLD {name}: not green")),
                _ => {} // at floor / not behind / healthy — no move, no hold noise
            }
        }
        // sover produce/post boost dry-run (the profit lever; opt-in via produce_boost).
        if name == "sover" {
            if let Some(bcfg) = repo.get("produce_boost") {
                if sover_boost::should_boost(Some(bcfg), &rollup, &velocity, true, 0) {
                    let cur = velocity.get("current").and_then(Value::as_i64).unwrap_or(0);
                    let tgt = velocity.get("target").and_then(Value::as_i64).unwrap_or(0);
                    scale_actions.push(format!(
                        "BOOST sover produce/post (behind {cur}/{tgt} posts, green)"
                    ));
                }
            }
        }
    }
    (scale_actions, holds)
}

/// The compact per-lane VELOCITY object handed to the growth planner (pure — unit-tested). It
/// anchors the daily goal to a real number to move FORWARD: the lane's primary 24h throughput
/// metric (posts, else live trades / fills, else shipped iterations — the north-star signal that
/// exists in the measured outcomes), the numeric daily target parsed from the north star when one
/// is stated (e.g. sover's "3 reels/day"), the remaining gap to that target, and a trend label.
///
/// `trend` is DERIVED, never fabricated:
///   - "stalled"  — current is a measured zero (the engine is dead; fix before growth)
///   - "behind"   — a target is stated and current is under it (room to grow toward the milestone)
///   - "healthy"  — a target is stated and current meets/exceeds it (push to the NEXT milestone)
///   - "growing"  — no numeric target in the north star, but throughput is non-zero (keep pushing)
///   - "unknown"  — no throughput metric is observable (null outcomes; can't anchor a number)
pub fn velocity_context(outcomes: &Value, north_star: &str) -> Value {
    // The primary throughput metric, in north-star priority order: posts (public reach), then live
    // trades / venue fills (capital velocity), then shipped iterations (code lanes). First present
    // non-null wins — matches which collector actually ran for this lane.
    let (metric, current) = [
        "posts_24h",
        "live_trades_24h",
        "fills_24h",
        "shipped_24h",
    ]
    .iter()
    .find_map(|k| outcomes.get(*k).and_then(Value::as_i64).map(|n| (*k, n)))
    .map(|(k, n)| (Some(k), Some(n)))
    .unwrap_or((None, None));

    let target = parse_daily_target(north_star);
    let gap = match (current, target) {
        (Some(c), Some(t)) => Some((t - c).max(0)),
        _ => None,
    };
    let trend = match (current, target) {
        (Some(0), _) => "stalled",
        (Some(c), Some(t)) if c < t => "behind",
        (Some(_), Some(_)) => "healthy",
        (Some(_), None) => "growing",
        (None, _) => "unknown",
    };
    json!({
        "metric": metric,
        "current": current,
        "target": target,
        "gap": gap,
        "trend": trend,
    })
}

/// Parse a stated numeric DAILY target out of a north-star sentence (pure — unit-tested). Matches
/// the operator's convention "<N> ... /day" or "<N> ... per day" (e.g. "Post 3 verified reels/day"
/// -> 3). Returns None when no daily cadence number is stated (most lanes state a direction, not a
/// number — those grow on trend, not a fixed target).
fn parse_daily_target(north_star: &str) -> Option<i64> {
    let lower = north_star.to_lowercase();
    // Find "/day" or "per day", then read the nearest preceding integer.
    let anchor = lower.find("/day").or_else(|| lower.find("per day"))?;
    let before = &lower[..anchor];
    let mut digits = String::new();
    // Walk backwards over the words before the anchor to the first integer token.
    for tok in before.split(|c: char| !c.is_ascii_digit()).rev() {
        if !tok.is_empty() {
            digits = tok.to_string();
            break;
        }
    }
    digits.parse::<i64>().ok()
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

/// Count lanes whose `runtime/<name>/_last_scale` marker is dated today (each marker holds an
/// ISO ts; a today-dated marker == one interval tightening this sweep-day). Best-effort: an absent
/// or unreadable/unparseable marker counts as none. Iterates the explicit repos.json lanes.
fn scale_tightenings_today() -> i64 {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut n = 0;
    for repo in crate::control::registry::read_repos_json() {
        let name = paths::repo_name(&repo);
        if name.is_empty() {
            continue;
        }
        let marker = paths::here().join("runtime").join(&name).join("_last_scale");
        if let Ok(raw) = std::fs::read_to_string(&marker) {
            // marker ts is UTC "%Y-%m-%dT..."; compare its date prefix to local today is close enough
            // for a once-a-day report line (a boundary hour is not worth a tz-correct parse here).
            if raw.trim().starts_with(&today) {
                n += 1;
            }
        }
    }
    n
}

/// Today's sover produce/post boost count from `runtime/sover/_boost_count_<today>` (the same
/// per-date counter sover_boost stamps). Absent/garbage -> 0.
fn sover_boosts_today() -> i64 {
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let path = paths::here()
        .join("runtime")
        .join("sover")
        .join(format!("_boost_count_{today}"));
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| sover_boost::parse_boost_count(&s))
        .unwrap_or(0)
}

/// Render the report. Returns (markdown, flag lines, urgent). The core flag rules are pure and
/// unit-tested; it ALSO does light read-only IO for the repo-hygiene scan + the fleet-action
/// marker line (both fail-safe to "nothing found" on any read error).
///
/// The flag rules are the post-mortem, encoded:
///   - posts_24h == 0                → "ZERO posts" (the 5-day Sover gap)
///   - posts_missing_url_24h > 0     → publish claims without URL evidence (the June 30 TikTok)
///   - live_trades_24h == 0          → zero live trades (finance-tracked projects)
///   - equity_delta_24h == 0.0       → equity flat (the $168.97 flatline)
///   - iterations_24h == 0           → lane never fired (the daedalus-trainer failure mode)
///   - off-base / dirty tracked tree → repo hygiene flag (escalates urgent, like ops-RED)
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

    // Explicit repos.json entries by name, for the report-only HYGIENE scan below (a discovered dir
    // with no explicit entry is intentionally excluded — same rule as the ops/hygiene grafts).
    let repo_by_name: std::collections::HashMap<String, Value> =
        crate::control::registry::read_repos_json()
            .into_iter()
            .filter_map(|r| {
                let n = paths::repo_name(&r);
                if n.is_empty() { None } else { Some((n, r)) }
            })
            .collect();

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
        // report-only repo HYGIENE (off-base / dirty tracked tree). A non-empty flag escalates the
        // evening notice to urgent, exactly like an ops-RED — a stranded managed tree is a real issue.
        if let Some(repo) = repo_by_name.get(name.as_str()) {
            let (issues, hyg) = crate::hygiene::scan_repo(repo);
            for issue in issues {
                let f = match issue {
                    crate::hygiene::HygieneIssue::OffBase => format!(
                        "⚠ {name}: off-base on '{}' (base '{}') — no live loop",
                        hyg.get("current").and_then(Value::as_str).unwrap_or("?"),
                        hyg.get("base").and_then(Value::as_str).unwrap_or("?"),
                    ),
                    crate::hygiene::HygieneIssue::Dirty => {
                        format!("⚠ {name}: uncommitted tracked changes")
                    }
                };
                md.push_str(&format!("- {f}\n"));
                flags.push(f);
            }
        }
        md.push('\n');
    }

    // One deterministic line summarizing today's growth-graft actions from the persisted markers
    // (best-effort: absent markers read as 0). Never fabricated — a quiet day reads "0 tightenings,
    // 0 sover boosts".
    md.push_str(&format!(
        "## fleet actions today\n- scale actions today: {} tightenings, {} sover boosts\n\n",
        scale_tightenings_today(),
        sover_boosts_today()
    ));

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

// --------------------------------------------------------------------------- //
// "while you slept" overnight report (pure — deterministic, never fabricated)
// --------------------------------------------------------------------------- //

/// The OVERNIGHT section of the morning plan (pure — unit-tested): a Polsia "while you slept"
/// narrative built DETERMINISTICALLY from the outcome ledger — shipped counts, posts, live trades /
/// fills, equity delta, and lanes that never fired. Never fabricated: a missing/null metric is
/// stated plainly ("no post registry", "equity unobservable"), a measured zero is stated as a zero,
/// and a lane that never fired is called out by name. Priority-ordered, one line per lane.
pub fn overnight_section(snapshot: &Value, incidents: &[Value]) -> String {
    let mut md = String::from("## OVERNIGHT\n\n");
    md.push_str("What VERIFIABLY happened since the last report (from the ledger — honest nulls):\n\n");
    let projects = snapshot
        .get("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if projects.is_empty() {
        md.push_str("- no projects observed (empty ledger)\n\n");
        return md;
    }
    let mut items: Vec<(&String, &Value)> = projects.iter().collect();
    items.sort_by_key(|(_, p)| p.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));

    for (name, p) in items {
        let iters = p.get("iterations_24h").and_then(Value::as_i64).unwrap_or(0);
        let shipped = p.get("shipped_24h").and_then(Value::as_i64).unwrap_or(0);
        let mut parts: Vec<String> = Vec::new();
        if iters == 0 {
            parts.push("lane NEVER fired".into());
        } else {
            parts.push(format!("{iters} iterations / {shipped} shipped"));
        }
        // posts — only when the collector ran (Null = registry unobservable, an honest null)
        if let Some(posts) = p.get("posts_24h") {
            match posts.as_i64() {
                Some(n) => parts.push(format!("{n} posts")),
                None => parts.push("posts unobservable".into()),
            }
        }
        // finance — only when the collector ran
        if p.get("equity_usd").is_some() {
            let trades = p.get("live_trades_24h").and_then(Value::as_i64);
            let fills = p.get("fills_24h").and_then(Value::as_i64);
            parts.push(format!(
                "{} live trades / {} fills",
                trades.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
                fills.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
            ));
            let delta = p.get("equity_delta_24h").and_then(Value::as_f64);
            parts.push(format!(
                "equity Δ{}",
                delta.map(|d| format!("{d:+.2}")).unwrap_or_else(|| "?".into())
            ));
        }
        md.push_str(&format!("- **{name}**: {}\n", parts.join(", ")));
    }
    md.push_str(&format!("- incidents (24h): {}\n", incidents.len()));
    md.push('\n');
    md
}

/// The single most important thing to watch (pure — unit-tested): the NEXT section. Picks the
/// highest-priority lane whose OUTCOME probe is RED (a dead engine is the top risk to growth), and
/// falls back to the highest-priority lane with a measured zero-throughput outcome, else a calm
/// "all engines live — push growth" note. Deterministic; never fabricated.
pub fn next_watch(snapshot: &Value, status: &Value) -> String {
    let projects = snapshot
        .get("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<(&String, &Value)> = projects.iter().collect();
    items.sort_by_key(|(_, p)| p.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));

    // 1) highest-priority RED probe status
    for (name, _) in &items {
        let red = status
            .get("projects")
            .and_then(|s| s.get(name.as_str()))
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("red");
        if red {
            return format!("{name} is RED — restore the engine before any growth work can compound.");
        }
    }
    // 2) highest-priority measured zero-throughput lane
    for (name, p) in &items {
        let dead_lane = p.get("iterations_24h").and_then(Value::as_i64) == Some(0);
        let zero_posts = p.get("posts_24h").and_then(Value::as_i64) == Some(0);
        let zero_trades = p.get("live_trades_24h").and_then(Value::as_i64) == Some(0);
        if dead_lane || zero_posts || zero_trades {
            return format!("{name} has zero measured throughput in 24h — confirm the engine is producing before pushing the milestone.");
        }
    }
    "All engines live — watch that today's growth goals actually move each lane's velocity number forward.".into()
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
        // Synthetic lane name NOT in repos.json on purpose: render_report's report-only hygiene scan
        // (scan_repo) does live git IO for names that match a real repos.json entry, so using e.g.
        // "dotz" here would make `flags.is_empty()` depend on the live repo's branch/dirty state
        // (flaky). An unknown name => repo_by_name miss => no git IO => deterministic calm case.
        let snapshot = json!({"projects": {
            "greenlane": {"priority": 4, "iterations_24h": 5, "shipped_24h": 2},
        }});
        let status = json!({"projects": {"greenlane": {"priority": 4, "status": "green"}}});
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

    // -------- ops-RED backlog graft (deterministic close-the-loop) --------
    #[test]
    fn ops_red_graft_marker_line_and_idempotence() {
        // the stable per-(project, probe) marker + the exact prepended line shape
        assert_eq!(ops_marker("publish_recency"), "[ops-auto:publish_recency]");
        let line = ops_item_line("publish_recency", "age 37.2h", "2026-07-03");
        assert!(line.starts_with("- [ ] [reliability][ops-auto:publish_recency] "));
        assert!(line.contains("publish_recency has been RED (age 37.2h)"));
        assert!(line.contains("fix the actual posting/trade/app path"));
        assert!(line.ends_with("(ops-auto 2026-07-03)"));

        // idempotence: an OPEN item with the marker blocks a re-prepend...
        let open = "- [ ] [reliability][ops-auto:publish_recency] publish_recency has been RED (x)\n\
                    - [ ] something else\n";
        assert!(has_open_ops_item(open, "publish_recency"));
        // ...a DIFFERENT probe's marker is independent (one open item PER probe)...
        assert!(!has_open_ops_item(open, "process"));
        // ...a DONE (- [x]) marker line does NOT count as open (RED again -> re-queue)...
        let done = "- [x] [reliability][ops-auto:publish_recency] fixed last time\n";
        assert!(!has_open_ops_item(done, "publish_recency"));
        // ...and an empty backlog has no open item.
        assert!(!has_open_ops_item("", "publish_recency"));
    }

    // -------- shared open-marker predicate (the factored core of has_open_ops_item) --------
    #[test]
    fn has_open_marker_matches_open_lines_only() {
        let body = "- [ ] [reliability][hygiene-auto:off_base] clean up ...\n\
                    - [x] [reliability][hygiene-auto:dirty] done earlier\n\
                    - [ ] unrelated item\n";
        assert!(has_open_marker(body, "[hygiene-auto:off_base]"));
        // a DONE (- [x]) line with the marker does NOT count as open
        assert!(!has_open_marker(body, "[hygiene-auto:dirty]"));
        // a marker present nowhere is absent
        assert!(!has_open_marker(body, "[hygiene-auto:missing]"));
        assert!(!has_open_marker("", "[hygiene-auto:off_base]"));
    }

    // -------- hygiene backlog graft: marker + exact report-only line shape + idempotence --------
    #[test]
    fn hygiene_graft_marker_line_and_idempotence() {
        use crate::hygiene::HygieneIssue;
        assert_eq!(hygiene_marker(HygieneIssue::OffBase), "[hygiene-auto:off_base]");
        assert_eq!(hygiene_marker(HygieneIssue::Dirty), "[hygiene-auto:dirty]");

        let hyg = json!({"current": "codex/x", "base": "main"});
        let off = hygiene_item_line(HygieneIssue::OffBase, &hyg, "2026-07-03");
        assert!(off.starts_with("- [ ] [reliability][hygiene-auto:off_base] clean up: "));
        assert!(off.contains("repo is off-base on 'codex/x' (base 'main')"));
        assert!(off.contains("land the branch into 'main' or drop it"));
        // report-only: never instructs a blind discard
        assert!(off.contains("do NOT discard uncommitted work without checking"));
        assert!(off.ends_with("(hygiene-auto 2026-07-03)"));

        let dirty = hygiene_item_line(HygieneIssue::Dirty, &hyg, "2026-07-03");
        assert!(dirty.starts_with("- [ ] [reliability][hygiene-auto:dirty] clean up: "));
        assert!(dirty.contains("uncommitted changes to TRACKED files"));
        assert!(dirty.contains("commit them on a branch or revert them"));
        assert!(dirty.ends_with("(hygiene-auto 2026-07-03)"));

        // idempotence keys off the shared predicate: one OPEN item per (repo, issue)
        let existing = format!("{off}\n");
        assert!(has_open_marker(&existing, &hygiene_marker(HygieneIssue::OffBase)));
        assert!(!has_open_marker(&existing, &hygiene_marker(HygieneIssue::Dirty)));
    }

    // -------- fleet_allocation: the model's one-line sentence (or None to fall back) --------
    #[test]
    fn fleet_allocation_reads_sentence_or_none() {
        let with = json!({"fleet": {"allocation": "Pour effort into sover today."}});
        assert_eq!(
            fleet_allocation(&with).as_deref(),
            Some("Pour effort into sover today.")
        );
        // absent fleet / allocation -> None (caller uses the deterministic fallback)
        assert!(fleet_allocation(&json!({"lanes": {}})).is_none());
        assert!(fleet_allocation(&json!({"fleet": {}})).is_none());
        // blank string -> None
        assert!(fleet_allocation(&json!({"fleet": {"allocation": "   "}})).is_none());
    }

    // -------- allocation_section: deterministic block; empty ranking is honest --------
    #[test]
    fn allocation_section_renders_ranking_and_actions() {
        let ranking = vec![
            ("sover".to_string(), 0.30, "behind 1/3 posts".to_string()),
            ("dotz".to_string(), 0.10, "growing".to_string()),
        ];
        let scale = vec!["SCALE sover 120->90s (behind 1/3 posts, green)".to_string()];
        let holds = vec!["HOLD asmodeus: real-money".to_string()];
        let md = allocation_section(&ranking, &scale, &holds);
        assert!(md.starts_with("## ALLOCATION"));
        assert!(md.contains("| sover | 0.30 | behind 1/3 posts |"));
        assert!(md.contains("| dotz | 0.10 | growing |"));
        assert!(md.contains("- SCALE sover 120->90s (behind 1/3 posts, green)"));
        assert!(md.contains("- HOLD asmodeus: real-money"));

        // empty ranking -> honest "no scalable lanes", never a fabricated row
        let empty = allocation_section(&[], &[], &[]);
        assert!(empty.contains("no scalable lanes"));
        assert!(!empty.contains("|")); // no table when there's nothing to rank
    }

    #[test]
    fn red_probe_detail_pulls_reason_or_falls_back() {
        let proj = json!({
            "reasons": [
                "publish_recency=red (age 37.2h (*.published_at 2026-07-01T22:58:53Z))",
                "process=red (Sover.exe NOT running)",
            ]
        });
        // the matching reason line is returned verbatim (id=status (detail))
        assert_eq!(
            red_probe_detail(&proj, "publish_recency"),
            "publish_recency=red (age 37.2h (*.published_at 2026-07-01T22:58:53Z))"
        );
        assert_eq!(red_probe_detail(&proj, "process"), "process=red (Sover.exe NOT running)");
        // no matching reason (or no reasons key) -> the bare probe name
        assert_eq!(red_probe_detail(&proj, "fills_recency"), "fills_recency");
        assert_eq!(red_probe_detail(&json!({}), "process"), "process");
    }

    // -------- velocity context (pure — growth anchor) --------
    #[test]
    fn velocity_context_anchors_the_growth_number() {
        // sover: posts throughput, a stated "3 reels/day" target -> behind, gap 1
        let sover = json!({"posts_24h": 2, "shipped_24h": 0, "iterations_24h": 2});
        let v = velocity_context(&sover, "Post 3 verified reels/day across IG/TikTok/YT; grow followers.");
        assert_eq!(v["metric"], json!("posts_24h"));
        assert_eq!(v["current"], json!(2));
        assert_eq!(v["target"], json!(3));
        assert_eq!(v["gap"], json!(1));
        assert_eq!(v["trend"], json!("behind"));

        // healthy: current meets the target -> push to next milestone
        let healthy = json!({"posts_24h": 3});
        assert_eq!(velocity_context(&healthy, "Post 3 reels/day")["trend"], json!("healthy"));

        // measured zero -> stalled (a dead engine, regardless of target)
        let zero = json!({"posts_24h": 0});
        let vz = velocity_context(&zero, "Post 3 reels/day");
        assert_eq!(vz["trend"], json!("stalled"));
        assert_eq!(vz["gap"], json!(3));

        // finance lane, no numeric target: live trades throughput, non-zero -> growing
        let asmo = json!({"live_trades_24h": 4, "fills_24h": 9, "shipped_24h": 1});
        let va = velocity_context(&asmo, "Grow capital velocity — more live fills/day.");
        // "fills/day" states a per-day cadence but no NUMBER before it -> no target
        assert_eq!(va["metric"], json!("live_trades_24h"));
        assert_eq!(va["current"], json!(4));
        assert_eq!(va["target"], Value::Null);
        assert_eq!(va["gap"], Value::Null);
        assert_eq!(va["trend"], json!("growing"));

        // code lane with a shipped throughput signal, no numeric target -> growing on shipped_24h
        let dotz = json!({"iterations_24h": 5, "shipped_24h": 2});
        let vd = velocity_context(&dotz, "Harden dotz-core reliability.");
        assert_eq!(vd["metric"], json!("shipped_24h"));
        assert_eq!(vd["current"], json!(2));
        assert_eq!(vd["trend"], json!("growing"));
    }

    #[test]
    fn velocity_context_unknown_when_no_throughput_metric() {
        // none of posts/live_trades/fills/shipped present -> unknown, all nulls
        let out = json!({"iterations_24h": 5, "priority": 3});
        let v = velocity_context(&out, "Harden reliability.");
        assert_eq!(v["metric"], Value::Null);
        assert_eq!(v["current"], Value::Null);
        assert_eq!(v["target"], Value::Null);
        assert_eq!(v["trend"], json!("unknown"));
    }

    #[test]
    fn parse_daily_target_reads_stated_cadence_number() {
        assert_eq!(parse_daily_target("Post 3 verified reels/day across IG"), Some(3));
        assert_eq!(parse_daily_target("ship 10 things per day"), Some(10));
        // no number before the /day anchor -> None (a direction, not a target)
        assert_eq!(parse_daily_target("more live fills/day"), None);
        // no daily cadence stated at all -> None
        assert_eq!(parse_daily_target("Harden dotz-core reliability"), None);
    }

    // -------- overnight "while you slept" section (pure — deterministic, honest nulls) --------
    #[test]
    fn overnight_section_reports_verified_numbers_and_honest_nulls() {
        let snapshot = json!({"projects": {
            "asmodeus": {"priority": 1, "iterations_24h": 3, "shipped_24h": 1,
                          "equity_usd": 168.97, "equity_delta_24h": 12.50,
                          "live_trades_24h": 4, "fills_24h": 9},
            "sover": {"priority": 2, "iterations_24h": 2, "shipped_24h": 0, "posts_24h": 0},
            "maki": {"priority": 3, "iterations_24h": 0, "shipped_24h": 0, "posts_24h": Value::Null},
        }});
        let md = overnight_section(&snapshot, &[json!({"probe_id": "x"})]);
        assert!(md.starts_with("## OVERNIGHT"));
        // priority order: asmodeus before sover before maki
        let a = md.find("asmodeus").unwrap();
        let s = md.find("sover").unwrap();
        let m = md.find("maki").unwrap();
        assert!(a < s && s < m);
        // verified finance numbers surface
        assert!(md.contains("4 live trades / 9 fills"));
        assert!(md.contains("equity Δ+12.50"));
        // a measured zero is stated as a zero, not hidden
        assert!(md.contains("0 posts"));
        // a null registry is an honest null, never a fake zero
        assert!(md.contains("posts unobservable"));
        // a lane that never fired is called out
        assert!(md.contains("lane NEVER fired"));
        // incident count is reported
        assert!(md.contains("incidents (24h): 1"));
    }

    #[test]
    fn overnight_section_empty_ledger_is_honest() {
        let md = overnight_section(&json!({"projects": {}}), &[]);
        assert!(md.contains("no projects observed"));
    }

    // -------- next-watch (pure — the single most important thing) --------
    #[test]
    fn next_watch_prioritizes_red_then_zero_then_calm() {
        let snapshot = json!({"projects": {
            "asmodeus": {"priority": 1, "iterations_24h": 3, "live_trades_24h": 4},
            "sover": {"priority": 2, "iterations_24h": 2, "posts_24h": 0},
        }});
        // a RED lane wins (engine down is the top risk)
        let status_red = json!({"projects": {
            "asmodeus": {"status": "green"},
            "sover": {"status": "red"},
        }});
        assert!(next_watch(&snapshot, &status_red).starts_with("sover is RED"));
        // no red, but sover has zero posts -> zero-throughput watch
        let status_green = json!({"projects": {
            "asmodeus": {"status": "green"},
            "sover": {"status": "yellow"},
        }});
        assert!(next_watch(&snapshot, &status_green).contains("sover has zero measured throughput"));
        // all engines live -> calm growth note
        let healthy = json!({"projects": {
            "asmodeus": {"priority": 1, "iterations_24h": 3, "live_trades_24h": 4},
        }});
        let status_ok = json!({"projects": {"asmodeus": {"status": "green"}}});
        assert!(next_watch(&healthy, &status_ok).starts_with("All engines live"));
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
