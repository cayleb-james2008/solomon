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
//! Scheduling: the CEO rhythm itself has NO dedicated scheduled task — it rides the watchdog tick
//! (visibly-open Solomon.exe) or the `solomon plan` / `solomon report` CLI, and the first tick after
//! reopening notifies the operator how long ops was blind. What CHANGED (2026-07-06 liveness autopsy,
//! catalog #4): the "no scheduled tasks" rule was RENEGOTIATED by the operator, because
//! GUI-tick-only liveness was the proximate cause of the 8h/25.5h watchdog gaps and the 2+ day
//! outages. The Solomon Sentinel scheduled task (tools/install_sentinel.ps1 -> `solomon watchdog`,
//! every 5 min, out-of-band) now keeps the sweep alive, and the host-liveness floor (resurrector.rs)
//! relaunches a DEAD Solomon.exe from that out-of-band sweep — so a closed host is no longer an
//! unbounded blind window. The CEO rhythm still rides the host's tick; it just no longer depends on
//! a human to keep that host open.
#![allow(dead_code)]

// Deterministic growth sub-planes wired into `tick`/`morning_plan`: fleet ROI ranking (allocate),
// bounded reversible interval tightening (scale), and the sover produce/post profit boost.
pub mod allocate;
pub mod focus;
pub mod scale;
pub mod sover_boost;

// D4 Layer 1: the CEO orchestrator + constrained-specialist Task system. On each wake it maps a
// diagnosis to a typed Task and DISPATCHES it to a constrained Specialist (today: the Engineering
// specialist wrapping the pi coder); the EXISTING gates (money_guard, pecrt::safety, tiers
// blast-radius) DECIDE. It holds no new authority — DISPATCHES only. See orchestrator.rs.
pub mod orchestrator;

// D5 Layer 1: the FIRST non-engineering specialist behind the same `trait Specialist` — a RESEARCH
// worker that ONLY reads the world and DRAFTS a provenance-tagged observation into a lane's PECRT
// observation log. It publishes NOTHING and spends NOTHING: its whitelist names zero money-capable
// and zero external-mutation tools, its scope is the drafts/observation-log path only, and its gate
// reuses the SAME fail-closed money_guard + pecrt::safety composition — so any future publish/pay
// tool is denied by default. See research.rs.
pub mod research;

// D12 Phase D rung 1: the FIRST specialist that ACTS on public projects to GROW them — organic-only,
// gate-bounded, money-out human-gated. Behind the same `trait Specialist`, it DRAFTS organic growth
// content (README/docs/examples/release-notes/showcase copy) into a GATED per-lane drafts log exactly
// as Research drafts, and — behind a per-lane opt-in flag (`growth_publish`), dry-run-first — invokes
// a project's OWN sanctioned publish lane (never a raw external post). Its whitelist names ZERO
// money-capable and ZERO raw-external-mutation tools; every paid kind (buy_ads/pay_invoice/stripe/
// ad_spend/withdraw) is REFUSED at the SAME fail-closed money_guard gate Research enforces. See
// growth.rs.
pub mod growth;

// D8 Layer 3: the CROSS-PROJECT wins ledger reader — the minimal cross-project learning surfaced
// into the morning plan. It TAILS the existing append-only runtime/outcomes.jsonl and extracts
// ANONYMIZED prior wins (shipped iteration / published post / positive equity day / live trade)
// with zero lane identity, which the plan prompt consults. A write-only ledger (written by the
// evening summary but never read back into planning) fails its wiring #[test]. See wins.rs.
pub mod wins;

// D8 Layer 3: the plug-and-play ONBOARDING path — given (project path + API-key env) ALONE it
// auto-detects the stack, seeds a real freshness objective (emitter verified present per D1's rule,
// else the honest no_objective sentinel), provisions an ISOLATED runtime/<name>/ state dir, writes
// the repos.json row (ZERO hand-editing), and runs ONE gated cycle through the existing gates. See
// onboard.rs.
pub mod onboard;

use crate::control::{paths, proc};
use crate::notify::{self, Notice};
use crate::ops::{self, ledger};
use crate::pecrt::warm::{LongTermAdapter, ObservationLog, ReconstructedContext, WarmContext};
use chrono::{Timelike, Utc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Local-time due hours (operator's wall clock, not UTC).
const PLAN_HOUR: u32 = 7;
const SUMMARY_HOUR: u32 = 20;
/// Attempts per day before giving up (a failing LLM endpoint must not be hammered every 2 min).
const MAX_ATTEMPTS: i64 = 3;
/// The CEO planner model over Ollama Cloud. Keep it distinct from the per-repo coder model so the
/// morning plan stays cheap and broad while Autopilot can spend GLM on implementation.
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
// warm three-tier memory (D11) — the CEO's continuous-thread carry-forward
// --------------------------------------------------------------------------- //

/// The fleet-level CEO "lane" name. The CEO already owns `runtime/_ceo_state.json` /
/// `runtime/_ceo_request.json`; its warm memory lives under the SAME `_ceo` prefix
/// (`runtime/_ceo/observations.jsonl`) so it can never collide with a product lane's runtime dir.
const CEO_WARM_LANE: &str = "_ceo";

/// How many recent dated observations / outcome-ledger rows seed the reconstructed working tier.
/// Both are TAILS (bounded reverse-seek), so a million-line log costs the same as a hundred-line one.
const CEO_WARM_OBS_TAIL: usize = 24;
const CEO_WARM_OUTCOMES_TAIL: usize = 12;

/// `HERE/runtime/_ceo/observations.jsonl` — the CEO's append-only DATED observation log. Sibling of
/// the per-lane observation logs the Research specialist writes (`runtime/<lane>/observations.jsonl`),
/// so it is auto-bounded by the SAME janitor rules (`*.jsonl` rotation + the per-runtime-dir cap) —
/// no new reaper is needed.
fn ceo_obs_log_path() -> PathBuf {
    paths::here()
        .join("runtime")
        .join(CEO_WARM_LANE)
        .join("observations.jsonl")
}

/// Build the CEO's three-tier warm context: the append-only observation log (SHORT-TERM) + a
/// READ-ONLY adapter over the ledgers Solomon already keeps (LONG-TERM: the shared
/// `runtime/outcomes.jsonl`). No migration, no new store. `here()` is test-redirected, so this is
/// hermetic under `cargo test`.
fn ceo_warm() -> WarmContext {
    ceo_warm_at(&ceo_obs_log_path(), paths::here())
}

/// Core of [`ceo_warm`], parameterized by the observation-log PATH and the HERE root so tests can
/// point it at a per-test unique dir (Solomon's convention for parallel test isolation is unique
/// paths, not serialization). Production always passes the `_ceo` path + the real HERE, so the wired
/// behavior is identical — this only lets a test avoid racing on the single shared `_ceo` log.
fn ceo_warm_at(obs_path: &std::path::Path, here: &std::path::Path) -> WarmContext {
    let obs = ObservationLog::at(obs_path.to_path_buf());
    let long_term = LongTermAdapter::new(here.to_path_buf(), CEO_WARM_LANE);
    WarmContext::new(obs, long_term)
}

/// Reconstruct the CEO's WARM working context at the top of a tick — the stable-prefix'd,
/// prior-observation-carrying prompt seed that REPLACES cold re-derivation as the thread's
/// context/identity. HARD INVARIANT (D11): this is context/identity ONLY. It is NEVER read by a
/// gate; the grafts below re-derive their decision inputs from a FRESH `ledger::snapshot()`. The
/// STABLE_PREFIX embedded here already pins "you are a SCHEDULER and MEMORY wrapper, not an
/// authority" — reconstruct_context can carry forward no authority and no gate-bypass. Emits the
/// prefix cache-hit rate so the ~$4/day prefix-cache economics stay observable in the log.
fn ceo_warm_reconstruct(warm: &mut WarmContext, log: &mut dyn FnMut(&str)) -> ReconstructedContext {
    let ctx = warm.reconstruct_context(CEO_WARM_OBS_TAIL, CEO_WARM_OUTCOMES_TAIL);
    log(&warm.cache.log_line());
    ctx
}

/// Append ONE dated observation of this tick's decisions to the CEO's observation log. This is the
/// SHORT-TERM write that the NEXT tick's `ceo_warm_reconstruct` reads back — the carry-forward that
/// makes the thread warm instead of cold. `append_fact` enforces the dated-first-order-fact contract
/// (rejects prose-of-prose / summary-of-summaries) and is append-only, so the log can never degrade
/// into drift and can never gain a summary line. A rejected/failed append is swallowed (logged) — a
/// disk hiccup or an accidentally-summary fact must never wedge the tick.
fn ceo_append_tick_observation(warm: &WarmContext, iso_date: &str, fact: &str) {
    if let Err(v) = warm.observations.append_fact(iso_date, fact) {
        // Non-fatal: the fact is lost, not the tick. (v is the rejection reason.)
        let _ = v;
    }
}

/// Compose the one-line dated fact recording THIS tick's decisions/outcomes — a first-order fact
/// (carries the epoch datum + the concrete gate states), never a rollup-of-rollups. Pure (caller
/// supplies the states + epoch) so the carry-forward `#[test]` can pin the shape without a clock.
fn ceo_tick_fact(epoch_s: u64, plan_state: &str, summary_state: &str) -> String {
    format!(
        "ceo tick t={epoch_s}: plan={plan_state} summary={summary_state}"
    )
}

// --------------------------------------------------------------------------- //
// tick — the watchdog graft
// --------------------------------------------------------------------------- //

/// One CEO tick, split into a FAST DETERMINISTIC CORE (always runs to completion) and a SLOW
/// BEST-EFFORT TAIL (offloaded). The core is blind-window notice + warm reconstruct + the D11 warm
/// append + the deterministic ops-RED/hygiene/scale grafts — all file-IO + bounded (git bounded at
/// 30 s, notify bounded at 15 s), so it can NEVER wedge the 2-min sweep and the graft flag resets
/// within milliseconds. The slow authority-bearing sub-grafts (the LLM morning plan, the LLM focus
/// decomposition, the sover produce/post subprocess) are OFFLOADED to `ceo_slow_tail` on its own
/// single-flighted thread, so a 250 s ollama call or a 600 s produce run can no longer (1) throttle
/// the CEO graft by pinning `CEO_GRAFT_RUNNING` (the observed GUI symptom: the append landed once in
/// ~25 min, 809 detached threads), nor (2) get killed mid-flight because the process exits before the
/// append lands (the observed Sentinel symptom: the append landed ~1 in 10). The append now sits at
/// the TOP of the core so it lands ASAP, and `watchdog::main` bounded-joins the core so the out-of-band
/// `solomon watchdog` one-shot lands it before exit. Every gate + catch_unwind isolation is preserved
/// — the tail re-runs the SAME gates against the SAME fresh snapshot; nothing here bypasses one.
pub fn tick() {
    // ---------- FAST DETERMINISTIC CORE (always completes; resets CEO_GRAFT_RUNNING within ms) ----------
    let _ = std::panic::catch_unwind(blind_window_notice_once);

    // WARM CARRY-FORWARD (D11): reconstruct the CEO thread's warm working context from its OWN prior
    // observations + the ledger tails — the stable-prefix'd, prior-observation-carrying context that
    // replaces cold re-derivation as the thread's identity. This is context/identity ONLY: it is NEVER
    // read by a graft or a gate (those re-derive from the FRESH `ledger::snapshot()` below), and the
    // STABLE_PREFIX inside it pins "scheduler, not authority" so it can carry forward no authority and
    // no gate-bypass. Isolated in its own catch_unwind so a warm-memory hiccup can never abort the sweep.
    let mut warm = ceo_warm();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut logf = |m: &str| eprintln!("[ceo] {m}");
        let _ctx = ceo_warm_reconstruct(&mut warm, &mut logf);
    }));

    // WARM CARRY-FORWARD (D11), the append half — the load-bearing per-sweep side effect, hoisted to
    // the TOP of the core (right after the reconstruct it pairs with) so it lands ASAP: it is the
    // SHORT-TERM write the NEXT tick's reconstruct reads back. It records THIS tick's day-gate state as
    // read from disk; the day-gate WRITES now live in the offloaded tail, so this is the gate state as
    // of the tick's start — still a valid dated first-order fact (the observation log is context/identity
    // only, never a gate input, so a one-tick lag before "plan=done" surfaces is immaterial). Isolated
    // so a warm-write hiccup can't skip the deterministic grafts below.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    ceo_warm_append_tick(&warm, &today);

    // DETERMINISTIC ops-RED graft (v2, close-the-loop): every sweep, ensure each project whose OUTCOME
    // probe is RED carries a targeted [ops-auto:<probe>] fix item atop its backlog — idempotent, no-LLM,
    // cheap. Now wrapped in its OWN catch_unwind (like the grafts below) so an ops-RED panic can't skip
    // hygiene/scale or the tail spawn — the append above already landed regardless.
    let _ = std::panic::catch_unwind(ops_red_backlog_graft);

    // GROWTH GRAFTS (every sweep, deterministic + bounded): (1) hygiene — report-only off-base/dirty
    // managed trees grafted into the lane backlog + a loud page on a NEW stranded off-base pair;
    // (2) scale — one bounded reversible interval tightening across the fleet. Each is wrapped in its
    // OWN catch_unwind (mirroring watchdog's per-graft isolation) so one graft's panic can't skip the
    // next, and the scale decision reads the SAME fresh snapshot+rollup the ops-RED graft / tail use.
    let snapshot = ledger::snapshot();
    let status: Value = std::fs::read(ops::outcomes::ops_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let _ = std::panic::catch_unwind(hygiene_backlog_graft);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        scale::maybe_scale_lanes(&snapshot, &status)
    }));

    // ---------- SLOW BEST-EFFORT TAIL (offloaded; single-flighted; never blocks the core) ----------
    // The slow authority-bearing sub-grafts run on their OWN thread so a 250 s ollama call or a 600 s
    // produce/post subprocess can NEVER wedge the 2-min sweep or pin the CEO graft flag. Single-flighted
    // (`CEO_SLOW_TAIL_RUNNING`): a still-running tail makes the next spawn a no-op, so slow tails can
    // never accumulate (the 809-thread GUI leak). The tail reads the SAME fresh snapshot+status the core
    // built. ALL GATES PRESERVED — see `ceo_slow_tail`.
    spawn_ceo_slow_tail(move || ceo_slow_tail(snapshot, status));
}

/// The D11 warm append (the fast-core deterministic tail): read the CEO day-gate state from disk and
/// append ONE dated first-order fact of this tick's plan/summary gate states to the observation log —
/// the SHORT-TERM write the NEXT tick's `ceo_warm_reconstruct` reads back, so the second consecutive
/// tick sees its own prior observation from the warm tier, not only a cold snapshot. `append_fact` is
/// append-only + rejects prose-of-prose, so the log stays factual and bounded (the janitor reaps the
/// `.jsonl` by the existing rules). Isolated in its own catch_unwind so a warm-write hiccup can never
/// skip the rest of the core.
fn ceo_warm_append_tick(warm: &WarmContext, today: &str) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let st = read_state();
        let plan_state = tick_gate_state(st.get("plan"));
        let summary_state = tick_gate_state(st.get("summary"));
        let epoch_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let fact = ceo_tick_fact(epoch_s, &plan_state, &summary_state);
        ceo_append_tick_observation(warm, today, &fact);
    }));
}

/// The CEO's SLOW best-effort sub-grafts, run OFF the fast-core thread by `spawn_ceo_slow_tail`: the
/// sover produce/post boost (a subprocess up to 600 s), the deep-work FOCUS decomposition (an LLM call
/// up to 250 s, at most once/day per focus lane), and the day-gated morning plan (LLM) + evening
/// summary. Each keeps its OWN catch_unwind isolation and its OWN gate — nothing here bypasses a gate or
/// the warm invariant; it only moves the SLOW work off the tick thread so the deterministic core (incl.
/// the D11 warm append) always lands and the CEO graft flag resets within milliseconds. Reads the SAME
/// fresh snapshot+status the core built this sweep.
fn ceo_slow_tail(snapshot: Value, status: Value) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sover_boost::maybe_boost(&snapshot, &status)
    }));
    // GROWTH SEAM (D12 / Phase B): the every-sweep growth-content COMPOSER — at most ONE gated,
    // unpublished, persona-safe draft per public lane per day (honest planner-directive trigger,
    // stamp-first day gate, draft-only; the publish ladder is never invoked from here).
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        crate::ceo::growth::maybe_draft_growth_content(&snapshot, &status)
    }));
    // DEEP-WORK FOCUS (Polsia): concentrate one top-leverage lane's next milestone into ordered
    // [campaign] steps; the other lanes keep their health-only baseline. Same fresh snapshot.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        focus::maybe_focus(&snapshot, &status)
    }));
    let _ = std::panic::catch_unwind(ceo_day_gates);
}

/// The two day-gated CEO jobs — the morning plan (LLM, >= 07:00) and the evening summary
/// (deterministic, >= 20:00) — each attempt-capped + idempotent (see `should_attempt`). Runs in the
/// SLOW TAIL because `morning_plan` makes an ollama call.
///
/// CROSS-PROCESS SAFETY: the tail's single-flight guard (`CEO_SLOW_TAIL_RUNNING`) is a process-LOCAL
/// static, so it serializes tails only WITHIN one process. TWO OS processes run `watchdog::main()` and
/// can each reach this RMW at once — the in-app GUI tick (main.rs, every 120 s) and the out-of-band
/// `solomon watchdog` Sentinel one-shot (every 5 min) — and `proc::atomic_write_json` prevents a torn
/// file but NOT a lost update: reading the gate, holding it across `morning_plan`'s ~250 s ollama call,
/// then writing back lets a second process that read the SAME pending gate clobber the first's write
/// (at worst a double plan for the day, or a lost/undercounted attempt). The plan section is therefore
/// STAMP-FIRST — it persists the attempt BEFORE the slow call (mirroring `sover_boost::stamp_boost`),
/// shrinking that window to near-zero: a concurrent sweep (or this process's own next sweep) then reads
/// the bumped attempt and does not re-run. `should_attempt`/`record_attempt` semantics are UNCHANGED —
/// the pre-stamp is exactly `record_attempt`'s failed-attempt value, which success / a third strike
/// overwrite below, so every observable end state is byte-identical to the prior write-after order. The
/// deterministic evening summary is fast, so its RMW window is already negligible; it keeps the plain
/// record-after order.
fn ceo_day_gates() {
    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let hour = now.hour();
    let mut st = read_state();

    let plan_sec = st.get("plan").cloned().unwrap_or_else(|| json!({}));
    if should_attempt(&plan_sec, &today, hour, PLAN_HOUR) {
        // STAMP-FIRST: persist the attempt (attempts+1, `done` preserved) BEFORE the slow ollama call,
        // so a concurrent watchdog process — or a process killed mid-plan (the Sentinel one-shot exiting
        // before its detached tail finishes) — cannot re-run the plan or lose the attempt. `pending` is
        // exactly `record_attempt(&plan_sec, &today, false)` (the same bytes the old code wrote on a
        // failed attempt); success and the third-strike give-up overwrite it below.
        let pending = record_attempt(&plan_sec, &today, false);
        st["plan"] = pending.clone();
        write_state(&st);

        let ok = morning_plan()
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if ok {
            st["plan"] = json!({"done": today});
            write_state(&st);
        } else if attempts_today(&pending, &today) >= MAX_ATTEMPTS {
            // Third strike: give up for the day, loudly — an unplanned day must be a KNOWN unplanned day.
            let _ = notify::send(&Notice::red(
                "Solomon: morning plan FAILED".into(),
                format!("{MAX_ATTEMPTS} attempts failed — lanes continue on standing goals today"),
            ));
            // Distinguishable give-up sentinel (NOT a plain success): the dashboard renders this as
            // 'gave up', not a green 'done', so an unplanned day is never shown as planned.
            st["plan"] = json!({"done": today, "gave_up": true});
            write_state(&st);
        }
        // Plain failure (not the third strike): the STAMP-FIRST write already persisted attempts+1 —
        // nothing more to write, and the attempt is durable even if this process now dies.
    }

    let sum_sec = st.get("summary").cloned().unwrap_or_else(|| json!({}));
    if should_attempt(&sum_sec, &today, hour, SUMMARY_HOUR) {
        let ok = evening_summary()
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        st["summary"] = record_attempt(&sum_sec, &today, ok);
        write_state(&st);
    }
}

/// Single-flight guard for the CEO slow best-effort tail — one tail thread at a time, so a slow
/// ollama/produce run can never pile up detached threads (the observed 809-thread GUI leak).
static CEO_SLOW_TAIL_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Reset the single-flight flag when the tail thread finishes (or panics) — mirrors watchdog's
/// `GraftFlagGuard`, so a panicking tail can never wedge the flag true and starve every future tail.
struct CeoSlowTailGuard;
impl Drop for CeoSlowTailGuard {
    fn drop(&mut self) {
        CEO_SLOW_TAIL_RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Spawn the CEO slow best-effort tail on its OWN thread — single-flighted + panic-isolated (mirrors
/// `watchdog::spawn_watchdog_graft`). If a prior tail is still running (a slow ollama/produce call), the
/// spawn is a NO-OP: every sub-graft gate is idempotent + retries next sweep, so skipping a busy sweep's
/// tail is safe and slow tails can never accumulate. Returns immediately — the fast core never waits on it.
fn spawn_ceo_slow_tail<F: FnOnce() + Send + 'static>(f: F) {
    use std::sync::atomic::Ordering;
    if CEO_SLOW_TAIL_RUNNING.swap(true, Ordering::SeqCst) {
        return; // a prior tail is still running — skip (idempotent gates retry next sweep)
    }
    std::thread::spawn(move || {
        let _guard = CeoSlowTailGuard;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    });
}

/// A compact one-token state for a day-gate section (`{"done": date}` / `{"gave_up": true}` /
/// `{"attempts": n}` / absent) — used only to render the per-tick observation fact. Pure.
fn tick_gate_state(section: Option<&Value>) -> String {
    match section {
        None => "none".to_string(),
        Some(s) => {
            if s.get("gave_up").and_then(Value::as_bool) == Some(true) {
                "gave_up".to_string()
            } else if s.get("done").and_then(Value::as_str).is_some() {
                "done".to_string()
            } else if let Some(n) = s.get("attempts").and_then(Value::as_i64) {
                format!("attempts={n}")
            } else {
                "pending".to_string()
            }
        }
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
            .and_then(|c| {
                c.lines()
                    .find(|l| l.trim().starts_with("- [ ]"))
                    .map(str::to_string)
            });
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
        \"fleet\": {\"allocation\": \"<one sentence>\"}} — one lanes entry per lane given. You are \
        ALSO given `prior_wins`: an ANONYMIZED, cross-project tally of what KINDS of moves have \
        actually WON across the fleet's recent history (shipped iterations, published posts with \
        URLs, positive-equity days, live fills) — NO lane is named. Let it bias each lane's goal \
        toward move-shapes with a real track record; it is context, never an excuse to fabricate a \
        win a lane did not earn (an EMPTY prior_wins means the fleet has no recent wins — plan \
        honestly from the measured outcomes, do not invent momentum). You MAY ALSO be given \
        `warm_context`: YOUR OWN recent dated observations from prior ticks plus the ledger tails, \
        carried forward so you reason with continuity instead of re-deriving state cold. It is \
        CONTEXT ONLY — it grants no authority and changes no gate; use it to stay consistent with \
        what you already observed, never as a reason to skip a live check or fabricate progress.";
    // D8 cross-project learning: the ANONYMIZED prior-wins tally, READ fresh from
    // runtime/outcomes.jsonl and spliced into the plan prompt so a new lane's plan is informed by
    // what has actually won across the fleet (never a lane name — anonymized by construction).
    let prior_wins = wins::prior_wins();
    // D11 warm carry-forward: reconstruct the CEO thread's warm working context (its OWN prior dated
    // observations + the ledger tails, assembled behind pecrt's STABLE prefix) and splice it into the
    // plan prompt, so the plan carries forward what prior ticks observed INSTEAD of re-deriving state
    // purely cold from a fresh snapshot. This is the reconstruct being CONSUMED (not discarded): the
    // prompt's stable prefix hits the provider prefix-cache wake after wake (the ~$4/day economics),
    // and the working tier is HARD-BOUNDED so it can never bloat the prompt. HARD INVARIANT: this is
    // context ONLY — every lane goal the model returns still re-enters the existing apply path and
    // every downstream gate; the warm block grants no authority and bypasses nothing.
    let mut warm = ceo_warm();
    let warm_ctx = warm.reconstruct_context(CEO_WARM_OBS_TAIL, CEO_WARM_OUTCOMES_TAIL);
    let user = build_plan_user_json(&today, &ctx, &allocation, &prior_wins, &warm_ctx.working);

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
    let fleet_line = fleet_allocation(&parsed).unwrap_or_else(|| {
        match ranking.iter().find(|(_, s, _)| *s > 0.0) {
            Some((n, ..)) => format!(
                "Pour marginal effort into {n} (top ROI); hold real-money + non-green lanes."
            ),
            None => {
                "No scalable lane today — hold the fleet and fix red engines first.".to_string()
            }
        }
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
            report.push_str(&format!(
                "## {lane}\n- **goal** [{tier}]: {goal}\n- **why**: {why}\n\n"
            ));
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

/// Assemble the morning-plan prompt's USER JSON (pure — the unit-tested seam). The D8 wins-ledger
/// wiring lives HERE so it can be proven: `prior_wins` is spliced under the `"prior_wins"` key, so a
/// non-empty wins summary MUST appear in the serialized prompt. The wiring `#[test]`
/// `wins_ledger_is_consulted_in_the_plan_prompt` seeds the real outcomes.jsonl, calls
/// `wins::prior_wins()`, and asserts the win lands here — a WRITE-ONLY outcomes ledger (never read
/// back into planning) would leave `prior_wins` empty and fail that assertion (failure catalog #5).
///
/// D11: `warm_context` is the CEO thread's reconstructed WARM working tier (its own prior dated
/// observations + the ledger tails, hard-bounded) spliced under `"warm_context"` — the carry-forward
/// that makes the plan warm instead of cold. Empty string => the key is omitted (a cold first plan is
/// byte-identical to before). Proven by `warm_context_is_consulted_in_the_plan_prompt`.
fn build_plan_user_json(
    today: &str,
    ctx: &[Value],
    allocation: &Value,
    prior_wins: &Value,
    warm_context: &str,
) -> String {
    let mut obj = json!({
        "date": today,
        "lanes": ctx,
        "allocation": allocation,
        "prior_wins": prior_wins,
    });
    if !warm_context.trim().is_empty() {
        obj["warm_context"] = json!(warm_context);
    }
    serde_json::to_string_pretty(&obj).unwrap_or_default()
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
const OPS_OUTCOME_PROBES: [&str; 8] = [
    "publish_recency_instagram",
    "publish_recency_tiktok",
    "publish_recency_youtube",
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
///
/// YELLOW pre-red gate (v2): also fire on YELLOW outcome probes with `consecutive_red >= 2` — the
/// probe is failing but held at yellow by the consecutive-red gate (e.g. cdp_alive with
/// `red_after_consecutive: 3`). This catches degrading outcomes BEFORE they flip red.
///
/// Deploy-gap gate (v2): also fire on the `binary_current` (git_sha_match) probe when YELLOW with
/// a "deploy gap" detail — the running binary was built from an old commit. This is a process-level
/// drift signal that the watchdog/deploy plane should act on, but the lane agent must also see.
/// HERE/runtime/_dead_red.json — the dedupe map for the dead-lane-RED operator page.
/// Shape: {"seen": {"<name>:<probe>": "<first_ts>"}} — one page per (lane, probe) while it persists.
fn dead_red_status_path() -> PathBuf {
    paths::here().join("runtime").join("_dead_red.json")
}

/// Pure (unit-tested): page the operator about a RED outcome on a STOPPED lane iff the lane is NOT
/// running AND the probe is RED AND we have not already paged for this (lane, probe). A stopped lane
/// consumes NO backlog, so the ops-auto fix we still file is theater until the operator Starts it —
/// this turns that silent rot into ONE deduped, actionable page.
pub fn should_page_dead_red(running: bool, probe_red: bool, already_paged: bool) -> bool {
    !running && probe_red && !already_paged
}

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
    let now_ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // repos.json entries by name — needed to test lane liveness (is_running) for the dead-red page.
    let repo_by_name: std::collections::HashMap<String, Value> =
        crate::control::registry::read_repos_json()
            .into_iter()
            .filter_map(|r| {
                let n = paths::repo_name(&r);
                if n.is_empty() {
                    None
                } else {
                    Some((n, r))
                }
            })
            .collect();

    // Prior dead-red dedupe map ("<name>:<probe>" -> first-seen ts). Absent/garbage -> empty
    // (fail-open: re-page rather than ever silently drop a real stopped+RED lane).
    let prev: Value = std::fs::read(dead_red_status_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let prev_seen = prev
        .get("seen")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut seen = Map::new(); // this sweep's live "<name>:<probe>" -> first_ts (drops resolved keys)

    for (name, proj) in projects {
        // Load the full verdict file for this project to get consecutive_red and detail per probe.
        let verdict: Value = std::fs::read(ops::outcomes::verdict_path(name))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or(Value::Null);
        let verdict_probes = verdict.get("probes").cloned().unwrap_or(Value::Null);

        // Which OUTCOME probes are RED for this project? (per-probe status map, whitelist-filtered)
        let probes = match proj.get("probes").and_then(Value::as_object) {
            Some(p) => p,
            None => continue,
        };
        // Is this project's improver lane actually running? A stopped lane consumes NO backlog, so
        // an ops-auto item filed into it is never worked (theater) — that case must PAGE, loudly.
        // No repos.json entry (discovered-only) -> can't manage it -> treat as running -> no page.
        let running = repo_by_name
            .get(name)
            .map(crate::control::locks::is_running)
            .unwrap_or(true);

        // --- RED outcome probes (existing behavior) ---
        for probe in OPS_OUTCOME_PROBES {
            if probes.get(probe).and_then(Value::as_str) != Some("red") {
                continue;
            }
            // The reason/detail for this probe from the rollup reasons list ("<probe>=red (<detail>)").
            let detail = red_probe_detail(proj, probe);
            // Still file the queued fix item — it will be worked the moment the lane resumes.
            ensure_ops_item(name, probe, &detail, &today, "red");

            // DEAD-LANE-RED: a RED outcome on a STOPPED lane is silent rot. Page once per
            // (lane, probe) while it persists (deduped via _dead_red.json).
            let key = format!("{name}:{probe}");
            let already_paged = prev_seen.contains_key(&key);
            if should_page_dead_red(running, true, already_paged) {
                let held_note = match repo_by_name.get(name) {
                    Some(r) if r.get("live_app").and_then(Value::as_bool) == Some(true) => {
                        " (live app — held)"
                    }
                    _ => "",
                };
                let _ = notify::send(&Notice::red(
                    format!("Solomon: {name} RED but lane STOPPED"),
                    format!(
                        "{detail} — the {name} improver lane is STOPPED{held_note}, so this queued \
                         fix will NOT run. Start the lane (or fix it manually); Solomon does not \
                         auto-restart a Stopped lane."
                    ),
                ));
            }
            // Carry the dead-red key forward ONLY while still dead-red (stopped + red). A key that
            // drops out (lane resumed OR probe cleared) re-pages if the condition later recurs.
            if !running {
                let first_ts = prev_seen
                    .get(&key)
                    .and_then(Value::as_str)
                    .unwrap_or(now_ts.as_str())
                    .to_string();
                seen.insert(key, json!(first_ts));
            }
        }

        // --- YELLOW pre-red gate: outcome probes with consecutive_red >= 2 ---
        // These probes are failing but held at yellow by the consecutive-red gate (e.g. cdp_alive
        // with red_after_consecutive: 3). Fire an investigate item BEFORE they flip red.
        for probe in OPS_OUTCOME_PROBES {
            if probes.get(probe).and_then(Value::as_str) != Some("yellow") {
                continue;
            }
            let consec = verdict_probes
                .get(probe)
                .and_then(|v| v.get("consecutive_red"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            if consec < 2 {
                continue;
            }
            let detail = verdict_probes
                .get(probe)
                .and_then(|v| v.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("pre-red (consecutive_red >= 2)");
            ensure_ops_item(name, probe, detail, &today, "pre_red");
        }

        // --- Deploy-gap gate: binary_current (git_sha_match) probe YELLOW with "deploy gap" ---
        // The running binary was built from an old commit; this is a process-level drift signal.
        if probes.get("binary_current").and_then(Value::as_str) == Some("yellow") {
            let detail = verdict_probes
                .get("binary_current")
                .and_then(|v| v.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if detail.contains("deploy gap") {
                ensure_ops_item(name, "binary_current", detail, &today, "deploy_gap");
            }
        }
    }

    // Persist the pruned dedupe map (atomic; best-effort — a write failure just re-pages next sweep).
    if let Some(parent) = dead_red_status_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&dead_red_status_path(), &json!({"seen": seen}));
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

/// The exact backlog line for an ops-auto probe (pure — unit-tested). Carries the stable
/// `[ops-auto:<probe>]` idempotence marker and a `[reliability]` intent tag (not a known improver
/// tier, so strip_tier leaves it in the text — deliberate; the item reads as reliability work).
/// `kind` distinguishes: "red" = outcome probe is RED; "pre_red" = YELLOW with consecutive_red >= 2;
/// "deploy_gap" = binary_current YELLOW with "deploy gap" detail.
fn ops_item_line(probe: &str, detail: &str, today: &str, kind: &str) -> String {
    let (prefix, suffix) = match kind {
        "red" => (
            "has been RED",
            "— the real outcome is failing, not the test gate; diagnose and fix the actual posting/trade/app path.",
        ),
        "pre_red" => (
            "is YELLOW with consecutive_red >= 2 (pre-red)",
            "— the probe is failing but held at yellow by the consecutive-red gate; investigate before it flips red.",
        ),
        "deploy_gap" => (
            "is YELLOW with deploy gap",
            "— the running binary was built from an old commit; redeploy or investigate the deploy pipeline.",
        ),
        _ => ("is failing", "— investigate."),
    };
    format!(
        "- [ ] [reliability]{} {probe} {prefix} ({detail}) {suffix} (ops-auto {today})",
        ops_marker(probe)
    )
}

/// Idempotently prepend ONE `[ops-auto:<probe>]` fix item to a lane's backlog. If an OPEN line
/// already carries `[ops-auto:<probe>]`, do NOTHING — this MUST NOT flood the backlog every 2-min
/// sweep. Reuses the same atomic-prepend + read-error safety contract as morning_plan (a READ ERROR
/// — the improver mid-rewrite / a locked file — skips the lane rather than risk truncating a live
/// backlog; file-absent starts from empty).
fn ensure_ops_item(name: &str, probe: &str, detail: &str, today: &str, kind: &str) {
    let path = backlog_path(name);
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return, // read failed — do NOT risk truncating a live backlog
    };
    if has_open_ops_item(&existing, probe) {
        return;
    }
    let line = ops_item_line(probe, detail, today, kind);
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
    let prev_seen = prev
        .get("seen")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

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
    let flat: String = s
        .replace("\r\n", " ")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string();
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
        let outcomes = snapshot["projects"]
            .get(&name)
            .cloned()
            .unwrap_or(json!({}));
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
                    let m = velocity
                        .get("metric")
                        .and_then(Value::as_str)
                        .unwrap_or("throughput");
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
    let (metric, current) = ["posts_24h", "live_trades_24h", "fills_24h", "shipped_24h"]
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

/// RAII cleanup for a temp file that must not outlive the call that created it. Drop runs on EVERY
/// exit path (each `?`, early return, or panic), so the plaintext Bearer-key headers file cannot
/// linger under runtime/ after ollama_chat returns — closing the credential-persistence hole.
struct TempFileGuard(std::path::PathBuf);
impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Resolve the CEO/MoA chat transport from the AUTOPILOT provider config (repos.json sentinel),
/// mirroring how the improver lanes resolve theirs (ctx.rs required_key / fleet.rs
/// provider_key_ref). Audit finding #05 (2026-07-12): ollama_chat hardwired ollama.com +
/// OLLAMA_API_KEY, so every CEO/MoA call silently broke whenever the fleet switched provider
/// (e.g. the temporary OpenRouter switch). Returns (provider, endpoint, api-key FIELD — an
/// env-var NAME per the repos.json contract, or a literal key). Unknown/absent provider falls
/// back to the previous Ollama Cloud behavior so a missing/corrupt config can never brick the
/// CEO. Pure over `cfg` for testability; ollama_chat feeds it registry::autopilot_config().
fn chat_transport(cfg: &Value) -> (String, &'static str, String) {
    let provider = cfg
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("ollama-cloud")
        .to_string();
    let (endpoint, default_key) = match provider.as_str() {
        "openrouter" => (
            "https://openrouter.ai/api/v1/chat/completions",
            "OPENROUTER_API_KEY",
        ),
        _ => ("https://ollama.com/v1/chat/completions", "OLLAMA_API_KEY"),
    };
    let key_field = cfg
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(default_key);
    // autopilot_config() fills a missing api_key with the registry-wide default "OLLAMA_API_KEY"
    // regardless of provider — under openrouter that combination can only be the default-fill (an
    // Ollama key never authenticates against OpenRouter), so treat it as unset, not an override.
    let key_field = if provider == "openrouter" && key_field == "OLLAMA_API_KEY" {
        default_key
    } else {
        key_field
    };
    (provider, endpoint, key_field.to_string())
}

/// When the fleet provider is OpenRouter, an Ollama-native model id (no '/', e.g. the CEO_MODEL
/// const "minimax-m3") cannot exist there — substitute the autopilot-configured model so the
/// morning-plan/focus calls survive a provider switch. Provider-appropriate ids (OpenRouter ids
/// always carry a "vendor/model" slash) pass through untouched, so the brain-block worker models
/// stay exactly as configured. Under ollama-cloud the requested model is NEVER rewritten
/// (byte-identical previous behavior).
fn chat_model_for(cfg: &Value, provider: &str, requested: &str) -> String {
    if provider == "openrouter" && !requested.contains('/') {
        if let Some(m) = cfg
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| m.contains('/'))
        {
            return m.to_string();
        }
    }
    requested.to_string()
}

/// True when the api_key FIELD is an env-var NAME (uppercase ASCII letters/digits/underscore,
/// e.g. "OPENROUTER_API_KEY_2") rather than a literal secret — the same convention
/// improver::ctx::Ctx::resolved_api_key uses for the per-repo field, kept in lockstep so the
/// autopilot block and the repo blocks read identically.
fn looks_like_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_uppercase() || c == '_')
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// One chat completion over the CONFIGURED fleet provider (Ollama Cloud or OpenRouter — resolved
/// from the repos.json autopilot block by chat_transport, previous Ollama Cloud behavior as the
/// fallback) via curl.exe (no HTTP client dependency; TLS handled by the OS curl, same
/// guarded-spawn contract as every other subprocess). The name is historical — it predates the
/// provider switch support. The API key rides a curl `-H @file` headers file under runtime/
/// (gitignored) — never argv, never a log line. The headers file is wrapped in a TempFileGuard so
/// the Bearer key is removed on every return path.
pub(crate) fn ollama_chat(model: &str, system: &str, user: &str) -> Result<String, String> {
    let cfg = crate::control::registry::autopilot_config();
    let (provider, endpoint, key_field) = chat_transport(&cfg);
    let model = chat_model_for(&cfg, &provider, model);
    // Env-var NAME -> read it from Solomon/.env (the previous behavior), then the process env
    // (a per-repo Ctx::apply_api_key override lands there). A literal key is used as-is.
    let key = if looks_like_env_name(&key_field) {
        notify::env_value(&key_field)
            .or_else(|| std::env::var(&key_field).ok().filter(|v| !v.is_empty()))
            .ok_or_else(|| format!("no {key_field} in .env"))?
    } else {
        key_field
    };
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
    std::fs::write(
        &req_path,
        serde_json::to_vec(&body).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(
        &hdr_path,
        format!("Authorization: Bearer {key}\nContent-Type: application/json\n"),
    )
    .map_err(|e| e.to_string())?;
    // From here every exit path (curl failure, parse failure, success) drops this guard, deleting the
    // Bearer-key headers file — it must never persist under runtime/.
    let _hdr_guard = TempFileGuard(hdr_path.clone());
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
        endpoint,
    ];
    let r = proc::run(&args, None, Some(Duration::from_secs(250))).map_err(|e| e.to_string())?;
    if !r.ok() {
        return Err(format!("curl exit {}: {}", r.code, r.stderr.trim()));
    }
    let v: Value = serde_json::from_str(r.stdout.trim()).map_err(|_| {
        format!(
            "non-JSON response: {}",
            r.stdout.chars().take(200).collect::<String>()
        )
    })?;
    // Ollama native shape first, OpenAI-compatible shape second.
    if let Some(c) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
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
    // Reasoning-model fallback (2026-07-09 MoA brain): Ollama Cloud reasoning models (kimi-k2.7-code,
    // deepseek-v4-pro, glm-5.2 with reasoning_effort) return the answer in `reasoning_content` /
    // `reasoning` when `content` is empty. Without this fallback every MoA worker call on a
    // reasoning model returns Err("no message content") and the brain degrades to the pre-MoA
    // single-model baseline — the MoA brain becomes a no-op. Try the OpenAI-compatible
    // `reasoning_content` first, then the Ollama-native `reasoning`.
    if let Some(c) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
    {
        if !c.trim().is_empty() {
            return Ok(c.to_string());
        }
    }
    if let Some(c) = v
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("reasoning"))
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
    // Roll each app's OWN confirmed revenue into the fleet money-truth file so the CEO
    // grades against real dollars, not a green health signal (the June post-mortem root
    // cause: fleet_ledger::append had no caller). No-ops honestly while revenue is $0.
    let rolled = ops::fleet_ledger::rollup_apps();

    let fleet = ops::outcomes::payload_summary(&status);
    let mut body = format!("fleet: {fleet}");
    for f in flags.iter().take(10) {
        body.push('\n');
        body.push_str(f);
    }
    if rolled > 0 {
        body.push_str(&format!(
            "\n$ {rolled} new revenue row(s) recorded in fleet_ledger.jsonl"
        ));
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
        let marker = paths::here()
            .join("runtime")
            .join(&name)
            .join("_last_scale");
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
    md.push_str(&format!(
        "fleet: {}\n\n",
        ops::outcomes::payload_summary(status)
    ));
    let mut flags: Vec<String> = Vec::new();
    let mut urgent = false;

    let projects = snapshot
        .get("projects")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<(&String, &Value)> = projects.iter().collect();
    items.sort_by_key(|(_, p)| {
        p.get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });

    // Explicit repos.json entries by name, for the report-only HYGIENE scan below (a discovered dir
    // with no explicit entry is intentionally excluded — same rule as the ops/hygiene grafts).
    let repo_by_name: std::collections::HashMap<String, Value> =
        crate::control::registry::read_repos_json()
            .into_iter()
            .filter_map(|r| {
                let n = paths::repo_name(&r);
                if n.is_empty() {
                    None
                } else {
                    Some((n, r))
                }
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
        md.push_str(&format!(
            "- lane: {iters} iterations / {shipped} shipped (24h)\n"
        ));
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
                    p.get("last_post_at")
                        .and_then(Value::as_str)
                        .unwrap_or("never")
                ));
                if n == 0 {
                    let f = format!("⚠ {name}: ZERO posts in 24h");
                    md.push_str(&format!("- {f}\n"));
                    flags.push(f);
                }
                let missing = p
                    .get("posts_missing_url_24h")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
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
                delta
                    .map(|d| format!("{d:+.2}"))
                    .unwrap_or_else(|| "?".into())
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
                md.push_str(&format!(
                    "- probes: {} — {}\n",
                    pstat.to_uppercase(),
                    reasons
                ));
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
    md.push_str(
        "What VERIFIABLY happened since the last report (from the ledger — honest nulls):\n\n",
    );
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
    items.sort_by_key(|(_, p)| {
        p.get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });

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
                delta
                    .map(|d| format!("{d:+.2}"))
                    .unwrap_or_else(|| "?".into())
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
    items.sort_by_key(|(_, p)| {
        p.get("priority")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });

    // 1) highest-priority RED probe status
    for (name, _) in &items {
        let red = status
            .get("projects")
            .and_then(|s| s.get(name.as_str()))
            .and_then(|p| p.get("status"))
            .and_then(Value::as_str)
            == Some("red");
        if red {
            return format!(
                "{name} is RED — restore the engine before any growth work can compound."
            );
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

    // ===================================================================== #
    // Audit finding #05 (2026-07-12): the CEO/MoA chat transport must follow
    // the CONFIGURED autopilot provider, not a hardwired ollama.com +
    // OLLAMA_API_KEY — the hardwiring broke every CEO/MoA call whenever the
    // fleet switched provider (the temporary OpenRouter switch did exactly
    // that). Pure-fn coverage of the resolution table + fallback contract.
    // ===================================================================== #
    #[test]
    fn chat_transport_openrouter_resolves_endpoint_and_key() {
        let cfg = json!({"provider": "openrouter", "api_key": "OPENROUTER_API_KEY"});
        let (provider, endpoint, key_field) = chat_transport(&cfg);
        assert_eq!(provider, "openrouter");
        assert_eq!(endpoint, "https://openrouter.ai/api/v1/chat/completions");
        assert_eq!(key_field, "OPENROUTER_API_KEY");
    }

    #[test]
    fn chat_transport_default_is_previous_ollama_behavior() {
        // Missing/unknown provider -> byte-identical previous behavior (Ollama Cloud), so a
        // corrupt or absent autopilot block can never brick the CEO.
        for cfg in [json!({}), json!({"provider": "ollama-cloud"}), json!({"provider": "???"})] {
            let (_, endpoint, key_field) = chat_transport(&cfg);
            assert_eq!(endpoint, "https://ollama.com/v1/chat/completions");
            assert_eq!(key_field, "OLLAMA_API_KEY");
        }
    }

    #[test]
    fn chat_transport_honors_custom_key_env_name() {
        // A numbered per-account key name (the multi-account ladder convention) is honored.
        let cfg = json!({"provider": "openrouter", "api_key": "OPENROUTER_API_KEY_2"});
        let (_, _, key_field) = chat_transport(&cfg);
        assert_eq!(key_field, "OPENROUTER_API_KEY_2");
    }

    #[test]
    fn chat_transport_openrouter_ignores_registry_default_ollama_key() {
        // autopilot_config() default-fills api_key="OLLAMA_API_KEY" even when the operator only
        // set provider=openrouter — that combination is the default-fill, not an override (an
        // Ollama key never authenticates against OpenRouter).
        let cfg = json!({"provider": "openrouter", "api_key": "OLLAMA_API_KEY"});
        let (_, _, key_field) = chat_transport(&cfg);
        assert_eq!(key_field, "OPENROUTER_API_KEY");
    }

    #[test]
    fn chat_model_substitutes_ollama_native_id_under_openrouter() {
        // CEO_MODEL ("minimax-m3") does not exist on OpenRouter — the configured autopilot model
        // takes its place so the morning-plan/focus calls survive the provider switch.
        let cfg = json!({"provider": "openrouter", "model": "tencent/hy3:free"});
        assert_eq!(chat_model_for(&cfg, "openrouter", CEO_MODEL), "tencent/hy3:free");
        // A provider-appropriate (slashed) id passes through untouched — brain-block models stay
        // exactly as configured.
        assert_eq!(chat_model_for(&cfg, "openrouter", "vendor/x:free"), "vendor/x:free");
    }

    #[test]
    fn chat_model_never_rewritten_under_ollama_cloud() {
        // Previous behavior preserved: under ollama-cloud the requested model is NEVER rewritten,
        // even when the config carries an (irrelevant) slashed model.
        let cfg = json!({"provider": "ollama-cloud", "model": "tencent/hy3:free"});
        assert_eq!(chat_model_for(&cfg, "ollama-cloud", "minimax-m3"), "minimax-m3");
        // And a slashless configured model can never be substituted in (nothing to gain).
        let cfg2 = json!({"provider": "openrouter", "model": "glm-5.2"});
        assert_eq!(chat_model_for(&cfg2, "openrouter", "minimax-m3"), "minimax-m3");
    }

    #[test]
    fn looks_like_env_name_matches_ctx_convention() {
        // Kept in lockstep with improver::ctx::Ctx::resolved_api_key: uppercase/digits/underscore
        // = an env-var NAME; any lowercase/punctuation = a literal secret.
        assert!(looks_like_env_name("OPENROUTER_API_KEY"));
        assert!(looks_like_env_name("OLLAMA_API_KEY_2"));
        assert!(!looks_like_env_name("sk-or-v1-abc123"));
        assert!(!looks_like_env_name(""));
        assert!(!looks_like_env_name("2KEY")); // must not start with a digit
    }

    #[test]
    fn temp_file_guard_removes_file_on_drop() {
        // The headers file carrying the plaintext Bearer key must not outlive ollama_chat. Proves the
        // RAII guard deletes its file when dropped (every return path drops it).
        let p = std::env::temp_dir().join("solomon_ceo_hdr_guard_test.txt");
        std::fs::write(&p, "Authorization: Bearer secret\n").unwrap();
        assert!(p.exists(), "precondition: file written");
        {
            let _g = TempFileGuard(p.clone());
        } // guard drops here
        assert!(!p.exists(), "guard must delete the headers file on drop");
    }

    // ===================================================================== #
    // D8 ACCEPTANCE (b): the CROSS-PROJECT wins ledger is CONSULTED in the
    // plan prompt. This is the "no write-only ledger" contract (failure
    // catalog #5) for the outcomes ledger's planning-read side: the evening
    // summary WRITES runtime/outcomes.jsonl; the morning plan must READ it
    // back into the prompt. We seed a real win into the ledger, run the REAL
    // reader (wins::prior_wins), and assert the win lands in the assembled
    // plan-prompt user JSON. A write-only ledger (never read into planning)
    // would leave prior_wins EMPTY and this assertion would fail.
    // ===================================================================== #
    #[test]
    fn wins_ledger_is_consulted_in_the_plan_prompt() {
        // Write a snapshot line into the REAL outcomes.jsonl (under the per-process test HERE, so
        // this is hermetic and never touches the operator's live ledger). It records a lane that
        // shipped + moved equity — real, evidenced wins.
        let path = ledger::ledger_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let snapshot = json!({
            "date": "2026-07-07",
            "projects": {
                "seedlane": {"shipped_24h": 2, "equity_delta_24h": 12.5, "live_trades_24h": 1}
            }
        });
        // append (do not clobber a real ledger if one exists under the test home)
        use std::io::Write;
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .expect("open outcomes.jsonl for the seed write");
            writeln!(f, "{}", snapshot).unwrap();
        }

        // (1) The REAL reader consulted the ledger and surfaced the win (proving it is READ, not
        // write-only).
        let prior_wins = wins::prior_wins();
        assert!(
            wins::has_wins(&prior_wins),
            "wins::prior_wins must READ the seeded outcomes.jsonl and surface a win: {prior_wins}"
        );

        // (2) The reader's output is SPLICED into the plan prompt: the assembled user JSON carries
        // the anonymized win shapes. This is the wiring — a lane's plan is informed by prior wins.
        let user = build_plan_user_json("2026-07-08", &[], &json!({}), &prior_wins, "");
        assert!(user.contains("prior_wins"), "the prompt must carry the prior_wins block: {user}");
        assert!(
            user.contains("shipped_iteration") || user.contains("positive_equity_day"),
            "the anonymized win shape must appear in the plan prompt (reader consulted): {user}"
        );
        // ANONYMIZED: the lane name that produced the win is NEVER in the prompt's wins block.
        // (build_plan_user_json is given empty lanes, so any "seedlane" could only come from wins.)
        assert!(
            !user.contains("seedlane"),
            "the wins block must be anonymized — no lane name leaks into the plan prompt: {user}"
        );

        // (3) NEGATIVE control (the write-only failure mode): an EMPTY summary — what a NEVER-READ
        // ledger would yield — carries no win shape into the prompt. So the win in (2) is present
        // ONLY because the reader actually read the ledger.
        let empty_user = build_plan_user_json("2026-07-08", &[], &json!({}), &wins::wins_summary_from_lines(&[], 14), "");
        assert!(
            !empty_user.contains("shipped_iteration") && !empty_user.contains("positive_equity_day"),
            "an unread (write-only) ledger yields no win in the prompt — the presence of a win \
             proves the reader is wired: {empty_user}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ===================================================================== #
    // D11 ACCEPTANCE: the CEO's reconstructed WARM working context is CONSULTED
    // in the plan prompt (the reconstruct is CONSUMED, not discarded). This is
    // the "no cold re-derivation" contract for the plan prompt: a prior tick's
    // observation, carried forward through pecrt::warm, must appear in the
    // assembled plan user JSON under `warm_context`. An EMPTY warm context (a
    // cold first plan) must OMIT the key entirely, so the cold path is
    // byte-identical to the pre-D11 prompt (the negative control below).
    // ===================================================================== #
    #[test]
    fn warm_context_is_consulted_in_the_plan_prompt() {
        use crate::pecrt::warm::{LongTermAdapter, ObservationLog, WarmContext};
        // A prior tick's observation, reconstructed through the SAME warm API morning_plan uses.
        let obs_path = unique_ceo_obs_path("plan");
        let log = ObservationLog::at(obs_path.clone());
        log.append_fact("2026-07-08", "ceo tick t=1700009999: plan=done summary=pending")
            .unwrap();
        let mut warm = WarmContext::new(
            ObservationLog::at(obs_path.clone()),
            LongTermAdapter::new(paths::here().to_path_buf(), CEO_WARM_LANE),
        );
        let rc = warm.reconstruct_context(CEO_WARM_OBS_TAIL, CEO_WARM_OUTCOMES_TAIL);
        // (1) the reconstructed warm working context carries the prior observation forward.
        assert!(rc.working.contains("t=1700009999"), "warm working tier carried the prior tick obs");
        // (2) it is SPLICED into the plan prompt under `warm_context` — a lane's plan is informed by
        // what prior ticks observed (carry-forward), not a purely cold snapshot.
        let user = build_plan_user_json("2026-07-08", &[], &json!({}), &json!({}), &rc.working);
        assert!(user.contains("warm_context"), "the prompt must carry the warm_context block: {user}");
        assert!(user.contains("t=1700009999"), "the prior tick observation must reach the plan prompt");
        // (3) NEGATIVE control: an EMPTY warm context OMITS the key — a cold first plan is unchanged.
        let cold = build_plan_user_json("2026-07-08", &[], &json!({}), &json!({}), "");
        assert!(!cold.contains("warm_context"), "an empty warm context must omit the key: {cold}");

        let _ = std::fs::remove_dir_all(obs_path.parent().unwrap());
    }

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
        assert_eq!(
            record_attempt(&sec, "2026-07-02", true),
            json!({"done": "2026-07-02"})
        );
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
        let fenced =
            "Here is the plan:\n```json\n{\"lanes\": {\"a\": {\"goal\": \"x\"}}}\n```\nDone.";
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
        assert_eq!(
            items[0],
            (
                "asmodeus".into(),
                "feature".into(),
                "line1 line2".into(),
                "because".into()
            )
        );
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
        assert!(flags
            .iter()
            .any(|f| f.contains("asmodeus: zero live trades")));
        assert!(flags.iter().any(|f| f.contains("asmodeus: equity flat")));
        assert!(flags
            .iter()
            .any(|f| f.contains("daedulus: lane never fired")));
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
        let line = ops_item_line("publish_recency", "age 37.2h", "2026-07-03", "red");
        assert!(line.starts_with("- [ ] [reliability][ops-auto:publish_recency] "));
        assert!(line.contains("publish_recency has been RED (age 37.2h)"));
        assert!(line.contains("fix the actual posting/trade/app path"));
        assert!(line.ends_with("(ops-auto 2026-07-03)"));

        // idempotence: an OPEN item with the marker blocks a re-prepend...
        let open =
            "- [ ] [reliability][ops-auto:publish_recency] publish_recency has been RED (x)\n\
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
        assert_eq!(
            hygiene_marker(HygieneIssue::OffBase),
            "[hygiene-auto:off_base]"
        );
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
        assert!(has_open_marker(
            &existing,
            &hygiene_marker(HygieneIssue::OffBase)
        ));
        assert!(!has_open_marker(
            &existing,
            &hygiene_marker(HygieneIssue::Dirty)
        ));
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
    fn should_page_dead_red_only_when_stopped_red_and_unpaged() {
        // stopped + red + not-yet-paged -> page (the sover-stopped-with-RED case).
        assert!(should_page_dead_red(false, true, false));
        // a running lane is actually consuming its backlog -> never a dead-red page.
        assert!(!should_page_dead_red(true, true, false));
        // probe not red -> never.
        assert!(!should_page_dead_red(false, false, false));
        // already paged this (lane, probe) -> suppressed (one deduped page while it persists).
        assert!(!should_page_dead_red(false, true, true));
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
        assert_eq!(
            red_probe_detail(&proj, "process"),
            "process=red (Sover.exe NOT running)"
        );
        // no matching reason (or no reasons key) -> the bare probe name
        assert_eq!(red_probe_detail(&proj, "fills_recency"), "fills_recency");
        assert_eq!(red_probe_detail(&json!({}), "process"), "process");
    }

    // -------- velocity context (pure — growth anchor) --------
    #[test]
    fn velocity_context_anchors_the_growth_number() {
        // sover: posts throughput, a stated "3 reels/day" target -> behind, gap 1
        let sover = json!({"posts_24h": 2, "shipped_24h": 0, "iterations_24h": 2});
        let v = velocity_context(
            &sover,
            "Post 3 verified reels/day across IG/TikTok/YT; grow followers.",
        );
        assert_eq!(v["metric"], json!("posts_24h"));
        assert_eq!(v["current"], json!(2));
        assert_eq!(v["target"], json!(3));
        assert_eq!(v["gap"], json!(1));
        assert_eq!(v["trend"], json!("behind"));

        // healthy: current meets the target -> push to next milestone
        let healthy = json!({"posts_24h": 3});
        assert_eq!(
            velocity_context(&healthy, "Post 3 reels/day")["trend"],
            json!("healthy")
        );

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
        assert_eq!(
            parse_daily_target("Post 3 verified reels/day across IG"),
            Some(3)
        );
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
        let md =
            "# asmodeus goal post (edit this line)\nGrow capital velocity: more live fills/day.\n";
        assert_eq!(
            pick_goal_post(Some(md), "old repos.json goal"),
            "Grow capital velocity: more live fills/day."
        );
        // a goal.md with only headings/blank lines falls back
        assert_eq!(
            pick_goal_post(Some("# heading only\n\n"), "fallback"),
            "fallback"
        );
        // absent goal.md falls back (trimmed)
        assert_eq!(pick_goal_post(None, "  fallback  "), "fallback");
        // neither present -> empty (lane stays dormant; the plane never invents work)
        assert_eq!(pick_goal_post(None, ""), "");
    }

    // ===================================================================== #
    // D11 ACCEPTANCE: the CEO thread carries context forward through the warm
    // tier instead of cold-deriving state every wake. The load-bearing proof
    // is CARRY-FORWARD: a second consecutive tick, building a FRESH WarmContext
    // (exactly as `tick()` does — `let mut warm = ceo_warm()` each call), reads
    // back the observation the FIRST tick appended. `here()` is test-redirected
    // (per-process temp home) so this is hermetic — it never touches the live
    // CEO observation log.
    // ===================================================================== #
    /// A per-test unique CEO observation-log path (Solomon's parallel-isolation convention is unique
    /// paths, not serialization — several ceo tests would otherwise race on the single shared `_ceo`
    /// log). Points `ceo_warm_at` at an isolated dir while exercising the IDENTICAL construction seam.
    fn unique_ceo_obs_path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("solomon_ceo_warm_{}_{}_{}", tag, std::process::id(), uniq))
            .join("observations.jsonl")
    }

    #[test]
    fn ceo_tick_reads_its_own_prior_observation_from_the_warm_tier() {
        let obs_path = unique_ceo_obs_path("carry");
        let here = paths::here().to_path_buf();

        // ---- TICK 1: build a fresh warm context and append this tick's observation (the exact
        // seam `tick()` runs at its end). Use a DISTINCTIVE epoch so we can find it back verbatim.
        let warm1 = ceo_warm_at(&obs_path, &here);
        let fact1 = ceo_tick_fact(1_700_000_111, "done", "pending");
        ceo_append_tick_observation(&warm1, "2026-07-08", &fact1);

        // ---- TICK 2: a BRAND-NEW WarmContext (nothing carried in memory — it must read from disk),
        // reconstruct the working context (the seam `tick()` runs at its top). CARRY-FORWARD holds
        // iff the working context contains tick 1's observation — proving the second tick reads its
        // OWN prior observation from the warm tier, not only a cold snapshot.
        let mut warm2 = ceo_warm_at(&obs_path, &here);
        let mut sink = |_m: &str| {};
        let ctx2 = ceo_warm_reconstruct(&mut warm2, &mut sink);
        assert!(
            ctx2.working.contains("t=1700000111"),
            "second consecutive tick must read its OWN prior observation from the warm tier \
(carry-forward), not cold-derive — working was: {}",
            ctx2.working
        );
        // and it is assembled behind the STABLE prefix (prefix-cache economics) — the cold path never
        // produced a stable-prefixed prompt at all.
        assert!(
            ctx2.full_prompt().starts_with(crate::pecrt::warm::STABLE_PREFIX),
            "warm context must sit behind the stable prefix"
        );

        // ---- APPEND-ONLY: tick 2 appends its own fact; BOTH observations remain (append-only, the
        // first is never rewritten or summarized away).
        let fact2 = ceo_tick_fact(1_700_000_222, "done", "done");
        ceo_append_tick_observation(&warm2, "2026-07-08", &fact2);
        let tail = warm2.observations.tail(10);
        assert!(tail.iter().any(|l| l.contains("t=1700000111")), "tick-1 obs still present (append-only)");
        assert!(tail.iter().any(|l| l.contains("t=1700000222")), "tick-2 obs appended");
        assert!(tail.iter().all(|l| l.starts_with("2026-07-08\t")), "every line is DATED");

        let _ = std::fs::remove_dir_all(obs_path.parent().unwrap());
    }

    #[test]
    fn ceo_tick_observation_is_a_dated_first_order_fact_not_a_summary() {
        // the per-tick fact must PASS validate_fact (dated first-order fact) and must NOT be a
        // summary-of-summaries — this is what keeps the observation log from degrading into drift.
        let fact = ceo_tick_fact(1_700_000_333, "gave_up", "none");
        assert!(
            crate::pecrt::warm::ObservationLog::validate_fact(&fact).is_fact(),
            "the tick fact must be a legal first-order dated fact: {fact}"
        );
        // a real append round-trips; a summary-shaped fact would be refused by append_fact.
        let obs_path = unique_ceo_obs_path("fact");
        let warm = ceo_warm_at(&obs_path, paths::here());
        ceo_append_tick_observation(&warm, "2026-07-08", &fact);
        assert_eq!(warm.observations.tail(5).len(), 1, "the legal fact appended");
        // a summary is refused (append_fact returns Err; nothing persists) — the log can't drift.
        let refused = warm
            .observations
            .append_fact("2026-07-08", "a summary of the summaries across all ticks");
        assert!(refused.is_err(), "a summary-of-summaries must be refused");
        assert_eq!(warm.observations.tail(5).len(), 1, "the refused summary did NOT persist");
        let _ = std::fs::remove_dir_all(obs_path.parent().unwrap());
    }

    #[test]
    fn tick_gate_state_renders_each_day_gate_shape() {
        assert_eq!(tick_gate_state(None), "none");
        assert_eq!(tick_gate_state(Some(&json!({"done": "2026-07-08"}))), "done");
        assert_eq!(tick_gate_state(Some(&json!({"done": "2026-07-08", "gave_up": true}))), "gave_up");
        assert_eq!(tick_gate_state(Some(&json!({"attempts_date": "2026-07-08", "attempts": 2}))), "attempts=2");
        assert_eq!(tick_gate_state(Some(&json!({}))), "pending");
    }

    // ===================================================================== #
    // SCHEDULING FIX ACCEPTANCE: the D11 warm append (the fast CORE's tail)
    // lands on a completed tick EVEN WHEN a slow authority-bearing sub-graft
    // (focus ollama up to 250 s / sover produce up to 600 s) is still running.
    // Before the fix those sub-grafts ran on the SAME thread as the append, so
    // a slow one (a) starved the append and (b) pinned the CEO graft flag
    // (observed live: the GUI appended once in ~25 min with 809 detached
    // threads; the Sentinel landed ~1 in 10 before process teardown). The fix
    // runs the append in the fast core and OFFLOADS the slow sub-grafts to a
    // single-flighted tail. This proves all three load-bearing properties
    // hermetically: the offload is non-blocking, the append landed, and the
    // single-flight guard prevents a second tail from piling up.
    // ===================================================================== #
    #[test]
    fn warm_append_lands_even_when_a_sub_graft_is_slow() {
        use std::sync::atomic::{AtomicBool, Ordering as O};
        use std::sync::{mpsc, Arc};
        use std::time::{Duration, Instant};

        // Unique obs path so this never races the shared _ceo log (Solomon's parallel-isolation
        // convention is unique paths, not serialization).
        let obs_path = unique_ceo_obs_path("slowtail");
        let here = paths::here().to_path_buf();
        let warm = ceo_warm_at(&obs_path, &here);

        // (A) the fast core's append lands synchronously — the SAME primitive `ceo_warm_append_tick`
        // uses. A distinctive epoch lets us find it back verbatim.
        let fact = ceo_tick_fact(1_700_777_000, "pending", "pending");
        ceo_append_tick_observation(&warm, "2026-07-08", &fact);

        // Offload a DELIBERATELY-BLOCKING sub-graft (a slow focus/ollama stand-in) EXACTLY as `tick()`
        // offloads the real slow tail. The fast core must NOT block on it.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let t0 = Instant::now();
        spawn_ceo_slow_tail(move || {
            // blocks until released — models a 250 s focus decomposition still in flight.
            let _ = release_rx.recv();
        });
        let spawn_elapsed = t0.elapsed();

        // (1) NON-BLOCKING: the offload returned immediately — the slow sub-graft runs off-thread, so it
        // can never wedge the 2-min sweep or the fast core.
        assert!(
            spawn_elapsed < Duration::from_secs(2),
            "spawn_ceo_slow_tail must return immediately (slow sub-graft runs off-thread), took {spawn_elapsed:?}"
        );
        // The single-flight flag was set SYNCHRONOUSLY in the caller before the thread was spawned.
        assert!(
            CEO_SLOW_TAIL_RUNNING.load(O::SeqCst),
            "the single-flight flag is held while the tail runs"
        );

        // (2) APPEND LANDED: the D11 warm append is present on this completed core, even though the
        // sub-graft is STILL BLOCKED — proving the append no longer depends on the slow work finishing.
        let obs_tail = warm.observations.tail(10);
        assert!(
            obs_tail.iter().any(|l| l.contains("t=1700777000")),
            "the warm append must land on the completed core even while a sub-graft is slow: {obs_tail:?}"
        );

        // (3) SINGLE-FLIGHT: while the first tail is still blocked, a SECOND spawn is a NO-OP — its body
        // never runs, so slow tails can never pile up (the observed 809-thread GUI leak). The swap is
        // synchronous in the caller, so this is deterministic (no sleep/race): the second spawn's swap
        // sees the flag already true and returns before spawning any thread.
        let ran_second = Arc::new(AtomicBool::new(false));
        let ran_second_w = ran_second.clone();
        spawn_ceo_slow_tail(move || {
            ran_second_w.store(true, O::SeqCst);
        });
        assert!(
            !ran_second.load(O::SeqCst),
            "a second slow tail must NOT start while one is running (single-flight — no thread pileup)"
        );

        // Release the blocked tail so its guard resets the flag; prove the guard actually resets it (a
        // panicking/finished tail must never wedge the flag true and starve every future tail).
        let _ = release_tx.send(());
        for _ in 0..200 {
            if !CEO_SLOW_TAIL_RUNNING.load(O::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !CEO_SLOW_TAIL_RUNNING.load(O::SeqCst),
            "the tail guard must reset the single-flight flag when the tail finishes"
        );

        let _ = std::fs::remove_dir_all(obs_path.parent().unwrap());
    }
}
