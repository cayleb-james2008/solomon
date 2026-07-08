//! OUTCOME-DRIVEN SELF-CRITIQUE (D3, Layer 2) — grade the PROFIT METRIC, not green tests.
//!
//! The failure this targets (failure catalog #1, "value-blind objective", second-order): the loop's
//! only after-the-fact grade of a shipped change was gates.rs's 1-5 quality score + tests-stayed-
//! green. A change that shipped GREEN but moved the tier-1 real-world metric zero-or-negative was
//! scored a WIN — the exact blind spot the ledger evidence shows (asmodeus equity_usd 168.97 flat
//! then null; kairos absent from the outcomes ledger entirely). Green tests are NOT profit.
//!
//! THE STEP: after a change SHIPS (iteration.rs post-ship, downstream of the freshness objective),
//! sample the lane's tier-1 freshness metric at ship time; then on a LATER cycle — once the metric
//! has gained real *settled* samples (the freshness window has turned) — sample it again and record
//! a SIGNED `metric_delta` with `n_new_samples`. The grade is keyed to each lane's freshness
//! `metric_id` and lands in a NEW per-lane ledger `runtime/<lane>/outcome_critique.jsonl`.
//!
//! HONESTY FLOOR (hard-coded, non-negotiable — the moat): a delta is CLAIMED only when the metric
//! gained real settled samples — `observable == true` AND `n_new_samples > 0`. A green test NEVER
//! imputes profit. Below [`MIN_NEW_SAMPLES`] the pending critique is HELD (not graded) so noise can
//! not thrash the backlog; a pending held longer than [`MAX_WAIT_SECS`] is dropped UNRECORDED (a
//! stalled metric grades nothing, honestly — it does not fabricate a zero).
//!
//! THE FEEDBACK (a write-only ledger is a bug — catalog #5): a shipped-GREEN change whose metric
//! moved zero-or-negative is graded a NON-WIN and its backlog FAMILY is down-weighted in a
//! fleet-wide `runtime/_outcome_family.json` table. [`family_demoted`] (consulted by backlog
//! prioritization) and [`family_penalty`] (folded into calibration's size-class ship-rate view)
//! read that table, so the next plan's item selection deprioritizes an approach that ships green but
//! does not move the metric.
//!
//! LEDGERS:
//!   * per-lane grade log  `runtime/<lane>/outcome_critique.jsonl` — one appended JSON line per
//!     graded change: {ts, change_id, metric_id, delta, n_settled_rows/n_new_samples, verdict,
//!     family, ship_value, after_value}.
//!   * per-lane pending    `runtime/<lane>/_outcome_critique_pending.json` — the single in-flight
//!     ship snapshot awaiting its window (atomic tmp+rename; a new ship overwrites a stalled one).
//!   * fleet-wide families `runtime/_outcome_family.json` — {"families": {"<family>": {"non_wins",
//!     "wins", "last_verdict", "demoted_until"}}} — the down-weight table (atomic writes).
//!
//! Attribution: the pending snapshot is stamped ONLY on a real LANDED ship of the NAMED item
//! (iteration.rs gates note_ship on `landed && !item_deviated && !beautify && !solomon`), so a
//! deviated/blocked/reverted terminal never opens a critique it cannot honestly grade.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::control::proc;
use crate::improver::ctx::{self, Ctx};
use crate::improver::freshness;

/// NOISE FLOOR: the metric must gain at least this many NEW settled samples between ship and the
/// after-measurement before ANY delta is graded. Below it the pending critique is held, never
/// recorded — a 1-fill blip must not down-weight a backlog family (catalog #5 thrash guard). The
/// floor is INCLUSIVE-pass: exactly MIN_NEW_SAMPLES new rows counts.
pub const MIN_NEW_SAMPLES: i64 = 3;

/// A pending critique older than this (no window turn in ~48h) is dropped UNRECORDED on the next
/// resolve pass — a metric that never gained settled rows grades nothing (honest), and the stale
/// snapshot is cleared so a fresh ship can open a new critique.
pub const MAX_WAIT_SECS: f64 = 172_800.0;

/// How long a NON-WIN demotes its backlog family (24h, matching the progress-ledger quarantine
/// cadence). After it expires the family is selectable at normal weight again — a demotion is a
/// down-weight, never a permanent ban (a metric regime can change).
pub const DEMOTE_SECS: i64 = 86_400;

const PENDING_NAME: &str = "_outcome_critique_pending.json";
const LEDGER_NAME: &str = "outcome_critique.jsonl";
const FAMILY_LEDGER_NAME: &str = "_outcome_family.json";

// --------------------------------------------------------------------------- #
// metric sample — the signed scalar we grade a delta on
// --------------------------------------------------------------------------- #

/// One observation of the lane's tier-1 metric: the freshness-contract fields plus the SIGNED
/// scalar we diff. `value` is the honest signed profit-shaped number the delta is computed on.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSample {
    pub metric_id: String,
    pub n_samples: i64,
    pub observable: bool,
    pub value: f64,
}

/// Extract the SIGNED scalar the critique grades from a probe's raw JSON payload. Priority:
///   1. an explicit top-level numeric `metric_value` (the forward-looking generic contract — any
///      lane can emit one signed number to be graded directly);
///   2. else kairos's settled-PnL shape `settled_usd_per_window_24h` (an object of per-mode signed
///      $/window values) summed — the settled-truth signal D3's acceptance is written against;
///   3. else 0.0 (no signed value in the payload — the delta is still honestly computable as the
///      change in `n_samples`-gated 0, i.e. "shipped, metric produced no signed movement").
///
/// Pure — unit-tested. NEVER guesses profit: absent a signed field the value is 0.0, and a 0-delta
/// on a shipped-green change is precisely the NON-WIN this layer must catch.
pub fn extract_metric_value(payload: &Value) -> f64 {
    if let Some(v) = payload.get("metric_value").and_then(Value::as_f64) {
        return v;
    }
    if let Some(obj) = payload
        .get("settled_usd_per_window_24h")
        .and_then(Value::as_object)
    {
        return obj.values().filter_map(Value::as_f64).sum();
    }
    0.0
}

/// Parse a probe payload into a [`MetricSample`]. Err when the contract's required fields are
/// missing/mistyped (same strictness as freshness's `Report`): an unparseable payload is
/// UNOBSERVABLE, never a fabricated zero-sample.
pub fn sample_from_payload(payload: &Value) -> Result<MetricSample, String> {
    let metric_id = payload
        .get("metric_id")
        .and_then(Value::as_str)
        .ok_or("probe payload missing 'metric_id'")?
        .to_string();
    let observable = payload
        .get("observable")
        .and_then(Value::as_bool)
        .ok_or("probe payload missing 'observable'")?;
    // n_samples is required only when observable (an unobservable payload legitimately omits it).
    let n_samples = payload
        .get("n_samples")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if observable && payload.get("n_samples").and_then(Value::as_i64).is_none() {
        return Err("observable probe payload missing 'n_samples'".to_string());
    }
    Ok(MetricSample {
        metric_id,
        n_samples,
        observable,
        value: extract_metric_value(payload),
    })
}

/// Run the lane's freshness probe once and parse a [`MetricSample`]. `None` when the lane has no
/// freshness config (no_objective/absent => the critique layer is inert). `Some(Err)` when the
/// probe ran but was unobservable/garbage (the caller HOLDS — never grades on a blind sample).
pub fn sample_metric(ctx: &Ctx) -> Option<Result<MetricSample, String>> {
    let cmd = freshness::probe_cmd(ctx)?;
    Some(freshness::probe_raw(ctx, &cmd).and_then(|payload| sample_from_payload(&payload)))
}

// --------------------------------------------------------------------------- #
// pending snapshot (runtime/<lane>/_outcome_critique_pending.json)
// --------------------------------------------------------------------------- #

fn pending_path(ctx: &Ctx) -> PathBuf {
    ctx.runtime.join(PENDING_NAME)
}

/// The ship-time snapshot awaiting its window turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub change_id: String,
    pub family: String,
    pub metric_id: String,
    pub ship_n_samples: i64,
    pub ship_value: f64,
    pub ship_ts: f64,
}

fn read_pending(ctx: &Ctx) -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path(ctx)).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    Some(Pending {
        change_id: v
            .get("change_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        family: v
            .get("family")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        metric_id: v
            .get("metric_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ship_n_samples: v.get("ship_n_samples").and_then(Value::as_i64).unwrap_or(0),
        ship_value: v.get("ship_value").and_then(Value::as_f64).unwrap_or(0.0),
        ship_ts: v.get("ship_ts").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

fn write_pending(ctx: &Ctx, p: &Pending) {
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = proc::atomic_write_json(
        &pending_path(ctx),
        &json!({
            "change_id": p.change_id,
            "family": p.family,
            "metric_id": p.metric_id,
            "ship_n_samples": p.ship_n_samples,
            "ship_value": p.ship_value,
            "ship_ts": p.ship_ts,
            "stamped_at": ctx::now(),
        }),
    );
}

fn clear_pending(ctx: &Ctx) {
    let _ = std::fs::remove_file(pending_path(ctx));
}

// --------------------------------------------------------------------------- #
// note_ship — stamp the ship-time snapshot (iteration.rs post-ship hook)
// --------------------------------------------------------------------------- #

/// POST-SHIP HOOK: after a real landed ship of the NAMED item, sample the lane's tier-1 metric NOW
/// and stamp the pending critique. `change_id` is the landed commit sha; `family` is the backlog
/// family the down-weight keys on (the progress-ledger selection key). No-op (and no file written)
/// when the lane has no freshness config — the critique layer is inert for a no_objective lane,
/// exactly like the freshness gate. A sample that runs but is UNOBSERVABLE stamps the snapshot with
/// observable=false's zero value BUT records the metric_id so the after-pass can still detect a
/// window turn; the honesty floor in `resolve_due` refuses to grade until the metric is observable
/// AND gained samples, so a blind ship-time sample cannot manufacture a delta.
pub fn note_ship(ctx: &mut Ctx, change_id: &str, family: &str) {
    let sample = match sample_metric(ctx) {
        None => return, // no freshness config => inert
        Some(r) => r,
    };
    let (metric_id, n_samples, value, observable_note) = match sample {
        Ok(s) => (s.metric_id, s.n_samples, s.value, None),
        Err(detail) => {
            // A ship-time probe failure is logged but does NOT block the ship (the ship already
            // landed). Stamp a snapshot with an empty metric_id so the after-pass rebaselines on
            // the next observable probe rather than grading against a garbage ship sample.
            (String::new(), 0, 0.0, Some(ctx.redact(&detail)))
        }
    };
    write_pending(
        ctx,
        &Pending {
            change_id: change_id.to_string(),
            family: family.to_string(),
            metric_id: metric_id.clone(),
            ship_n_samples: n_samples,
            ship_value: value,
            ship_ts: unix_now(),
        },
    );
    match observable_note {
        Some(detail) => ctx.log(&format!(
            "outcome-critique: shipped '{}' but the ship-time metric probe was unobservable ({detail}) \
— pending stamped, will grade once the metric is observable and gains settled rows",
            head_chars(change_id, 16)
        )),
        None => ctx.log(&format!(
            "outcome-critique: shipped '{}' — pending stamped (metric '{metric_id}', ship n={n_samples}, \
ship value={value}); grade deferred to the next settled window",
            head_chars(change_id, 16)
        )),
    }
}

// --------------------------------------------------------------------------- #
// the grade — pure verdict cores
// --------------------------------------------------------------------------- #

/// The graded verdict for a resolved critique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The metric moved strictly positive on real settled samples — a real win.
    Win,
    /// The metric moved zero-or-negative on real settled samples — shipped-green but value-blind.
    NonWin,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Win => "win",
            Verdict::NonWin => "non_win",
        }
    }
}

/// The delta grade, honesty-floored. `Some((delta, verdict))` ONLY when the after-sample is
/// observable, shares the ship metric_id, and gained >= MIN_NEW_SAMPLES new settled rows. `None`
/// (HOLD — do not grade) on every honesty/noise-floor failure: unobservable after-sample, a
/// changed metric_id (the operator repointed the objective — the ship's baseline no longer
/// applies), or fewer than MIN_NEW_SAMPLES new rows. Pure — the acceptance tests (c) and (d) pin
/// this directly. A strictly-positive delta is a Win; zero-or-negative is a NonWin.
pub fn grade(ship: &Pending, after: &MetricSample) -> Option<(f64, Verdict)> {
    // HONESTY FLOOR: never grade a blind after-sample.
    if !after.observable {
        return None;
    }
    // The ship snapshot must share the metric identity we are grading (a repointed objective, or a
    // ship whose own probe was unobservable => empty metric_id, cannot be honestly graded).
    if ship.metric_id.is_empty() || ship.metric_id != after.metric_id {
        return None;
    }
    // NOISE FLOOR: the metric must have gained real settled samples.
    let n_new = after.n_samples - ship.ship_n_samples;
    if n_new < MIN_NEW_SAMPLES {
        return None;
    }
    let delta = after.value - ship.ship_value;
    let verdict = if delta > 0.0 {
        Verdict::Win
    } else {
        Verdict::NonWin
    };
    Some((delta, verdict))
}

/// The count of new settled rows between ship and after (>= 0 clamp — a probe whose sample count
/// went DOWN, e.g. a db compaction, reports 0 new, not a negative row count).
pub fn n_new_samples(ship: &Pending, after: &MetricSample) -> i64 {
    (after.n_samples - ship.ship_n_samples).max(0)
}

// --------------------------------------------------------------------------- #
// resolve_due — the after-window grade pass (called from freshness short-circuit's proceed path)
// --------------------------------------------------------------------------- #

/// AFTER-WINDOW PASS (ctx entry point, iteration.rs post-freshness hook): downstream of the
/// freshness objective, once the loop has PROCEEDED on a fresh cycle. No-op — and NO probe cost —
/// when no pending critique is in flight (the common case), so a lane with nothing to grade pays
/// nothing. When a pending exists it samples the lane's tier-1 metric once and defers to
/// [`resolve_due_with`] for the honesty/noise-floored grade. A probe that runs but is UNOBSERVABLE
/// HOLDS the pending (never grades on a blind sample).
pub fn resolve_due(ctx: &mut Ctx) {
    if read_pending(ctx).is_none() {
        return; // nothing in flight: no probe, no work
    }
    let after = match sample_metric(ctx) {
        None => return, // freshness config vanished mid-flight => leave the pending for later
        Some(Ok(s)) => s,
        Some(Err(detail)) => {
            // blind after-sample: HOLD (the stale-drop in resolve_due_with still applies via a
            // synthetic unobservable sample so an aged-out pending is still cleared honestly).
            let detail = ctx.redact(&detail);
            ctx.log(&format!(
                "outcome-critique: after-window metric probe unobservable ({detail}) — holding the \
pending grade (never graded on a blind sample)"
            ));
            resolve_due_with(
                ctx,
                &MetricSample {
                    metric_id: String::new(),
                    n_samples: 0,
                    observable: false,
                    value: 0.0,
                },
            );
            return;
        }
    };
    resolve_due_with(ctx, &after);
}

/// The pure-testable after-window core: grade a pending against an already-sampled `after`. If a
/// pending critique exists AND the metric has turned its window enough to grade honestly, record
/// the SIGNED delta to `runtime/<lane>/outcome_critique.jsonl`, resolve the pending, and feed a
/// NON-WIN back into the family down-weight table. When the noise floor is not yet met the pending
/// is HELD (returns without recording); a pending older than MAX_WAIT_SECS is dropped unrecorded.
/// No-op when no pending exists.
pub fn resolve_due_with(ctx: &mut Ctx, after: &MetricSample) {
    let pending = match read_pending(ctx) {
        Some(p) => p,
        None => return,
    };
    // Stale-drop: a pending that never saw its window turn is dropped UNRECORDED (honest silence).
    if unix_now() - pending.ship_ts > MAX_WAIT_SECS {
        clear_pending(ctx);
        ctx.log(&format!(
            "outcome-critique: pending for '{}' aged out (>{:.0}h, no settled window turn) — dropped \
UNRECORDED (a metric that gained no settled rows grades nothing)",
            head_chars(&pending.change_id, 16),
            MAX_WAIT_SECS / 3600.0
        ));
        return;
    }
    let (delta, verdict) = match grade(&pending, after) {
        Some(g) => g,
        None => return, // HOLD: honesty/noise floor not met — keep the pending for a later window
    };
    let n_new = n_new_samples(&pending, after);
    // record the grade line (the per-lane ledger).
    append_grade(ctx, &pending, delta, verdict, n_new, after.value);
    clear_pending(ctx);
    // feed the verdict into the family down-weight table.
    record_family_outcome(ctx, &pending.family, verdict);
    ctx.log(&format!(
        "outcome-critique: change '{}' graded {} — metric '{}' delta {:+} over {} new settled row(s) \
(ship value {} -> after {}); family '{}' {}",
        head_chars(&pending.change_id, 16),
        verdict.as_str(),
        pending.metric_id,
        delta,
        n_new,
        pending.ship_value,
        after.value,
        head_chars(&pending.family, 40),
        match verdict {
            Verdict::Win => "credited a win",
            Verdict::NonWin => "down-weighted (shipped green, metric did not move)",
        }
    ));
}

/// Append one graded line to the per-lane `outcome_critique.jsonl`. `n_settled_rows` is the count
/// of NEW settled samples the grade was computed over (acceptance (a) names this field).
fn append_grade(
    ctx: &Ctx,
    pending: &Pending,
    delta: f64,
    verdict: Verdict,
    n_settled_rows: i64,
    after_value: f64,
) {
    let line = json!({
        "ts": ctx::now(),
        "change_id": pending.change_id,
        "metric_id": pending.metric_id,
        "delta": delta,
        "n_settled_rows": n_settled_rows,
        "verdict": verdict.as_str(),
        "family": pending.family,
        "ship_value": pending.ship_value,
        "after_value": after_value,
    });
    let text = serde_json::to_string(&line).unwrap_or_else(|_| "{}".to_string());
    ctx.runtime_append(&ctx.runtime.join(LEDGER_NAME), &text);
}

// --------------------------------------------------------------------------- #
// family down-weight table (fleet-wide runtime/_outcome_family.json)
// --------------------------------------------------------------------------- #

/// The FLEET-wide runtime dir (Solomon/runtime): ctx.runtime is Solomon/runtime/<name>. Mirrors
/// calibration.rs's fleet_runtime_dir — a family's value-blindness is a property of the APPROACH,
/// worth sharing across lanes, not siloed per lane.
fn fleet_runtime_dir(ctx: &Ctx) -> PathBuf {
    ctx.runtime
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ctx.control.join("runtime"))
}

fn family_ledger_path(dir: &Path) -> PathBuf {
    dir.join(FAMILY_LEDGER_NAME)
}

fn read_family_ledger(dir: &Path) -> Value {
    let text = std::fs::read_to_string(family_ledger_path(dir)).unwrap_or_default();
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if v.is_object() {
        v
    } else {
        json!({ "families": {} })
    }
}

fn write_family_ledger(dir: &Path, v: &Value) {
    let _ = std::fs::create_dir_all(dir);
    let _ = proc::atomic_write_json(&family_ledger_path(dir), v);
}

/// Fold one graded verdict into the fleet family table. A NON-WIN increments `non_wins` and (re)arms
/// a `demoted_until` DEMOTE_SECS out; a WIN increments `wins` and CLEARS any demotion (the approach
/// proved it can move the metric). No-op on an empty family (beautify/solomon lanes never carry one).
pub fn record_family_outcome(ctx: &Ctx, family: &str, verdict: Verdict) {
    if family.is_empty() {
        return;
    }
    let dir = fleet_runtime_dir(ctx);
    let mut led = read_family_ledger(&dir);
    if !led.get("families").map(Value::is_object).unwrap_or(false) {
        led["families"] = json!({});
    }
    let (mut wins, mut non_wins) = family_counts(&led, family);
    let until = match verdict {
        Verdict::Win => {
            wins += 1;
            0 // a win clears the demotion
        }
        Verdict::NonWin => {
            non_wins += 1;
            unix_now_i64() + DEMOTE_SECS
        }
    };
    led["families"][family] = json!({
        "wins": wins,
        "non_wins": non_wins,
        "last_verdict": verdict.as_str(),
        "demoted_until": until,
        "updated_at": ctx::now(),
    });
    write_family_ledger(&dir, &led);
}

fn family_counts(led: &Value, family: &str) -> (i64, i64) {
    let cell = led.get("families").and_then(|f| f.get(family));
    let n = |k: &str| {
        cell.and_then(|c| c.get(k))
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    (n("wins"), n("non_wins"))
}

// --------------------------------------------------------------------------- #
// readers — consumed by backlog prioritization + calibration (the feedback wiring)
// --------------------------------------------------------------------------- #

/// True iff `family` is currently demoted (a NON-WIN armed `demoted_until` in the future). Consulted
/// by backlog prioritization so a value-blind approach is deprioritized in the next plan's item
/// selection. An empty family, a never-seen family, and an expired demotion are all NOT demoted.
pub fn family_demoted(ctx: &Ctx, family: &str) -> bool {
    if family.is_empty() {
        return false;
    }
    let dir = fleet_runtime_dir(ctx);
    read_family_ledger(&dir)
        .get("families")
        .and_then(|f| f.get(family))
        .and_then(|e| e.get("demoted_until"))
        .and_then(Value::as_i64)
        .map(|t| t > unix_now_i64())
        .unwrap_or(false)
}

/// A backlog-selection penalty for `family`: 0 when not demoted, else 1 (a rank down-weight the
/// picker adds so a demoted family sorts AFTER an equal-rank non-demoted item). Kept a small
/// bounded integer so it re-orders WITHIN a priority bucket without ever leapfrogging a higher
/// bucket (a real ops-auto RED must still outrank a demoted chore).
pub fn family_penalty(ctx: &Ctx, family: &str) -> u8 {
    if family_demoted(ctx, family) {
        1
    } else {
        0
    }
}

/// The mandatory value-focus directive appended to a task when its backlog family is DEMOTED (a
/// prior shipped-green change of this family moved the tier-1 metric zero-or-negative). This is the
/// calibration/backlog feedback made actionable to the agent: the last time this approach shipped
/// green, the real metric did not move, so a green gate is NOT the objective — target the metric.
/// None when the family is not demoted. Reads the fleet ledger for the last recorded delta so the
/// directive names the concrete non-win. Pure-string; iteration.rs appends it at selection time.
pub fn value_focus_directive(ctx: &Ctx, family: &str) -> Option<String> {
    if !family_demoted(ctx, family) {
        return None;
    }
    let dir = fleet_runtime_dir(ctx);
    let (wins, non_wins) = family_counts(&read_family_ledger(&dir), family);
    Some(format!(
        "## Outcome-critique down-weight (mandatory — grade the metric, not the tests)\n\
         A PRIOR shipped-GREEN change of this backlog family moved the tier-1 real-world metric \
         zero-or-negative ({non_wins} non-win(s), {wins} win(s) recorded). A passing test gate is \
         NOT the objective here — the objective is the SETTLED metric this lane gates on. Do NOT \
         ship a change whose only evidence is green tests: make a change you can argue will move the \
         settled metric, and state in your summary the concrete mechanism by which it does. If you \
         cannot, prefer a different, higher-leverage item."
    ))
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

/// Whole-second unix time for the family down-weight table's `demoted_until` (an i64 wall-clock
/// deadline, matching the progress-ledger quarantine's i64 discipline). The sub-second `unix_now`
/// is used only for the ship_ts / MAX_WAIT_SECS aging math.
fn unix_now_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn uniq() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!("{:x}_{:x}", nanos, N.fetch_add(1, Ordering::Relaxed))
    }

    /// An isolated Ctx: its own control dir (for repos.json), runtime = <base>/runtime/<name> (so
    /// the fleet family ledger lands in <base>/runtime), and an EXISTING repo dir (a probe runs
    /// with cwd=repo). Mirrors the freshness.rs/calibration.rs test-ctx pattern.
    fn test_ctx(repos_rows: Option<Value>) -> Ctx {
        let base = std::env::temp_dir().join(format!("solomon_critique_test_{}", uniq()));
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
        let mut c = Ctx::configure(
            &repo.to_string_lossy(),
            "critiquetest",
            "ollama-cloud",
            None,
        );
        c.control = control;
        c.runtime = base.join("runtime").join("critiquetest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    fn ship_pending(metric_id: &str, n: i64, value: f64) -> Pending {
        Pending {
            change_id: "abcdef1234567890".into(),
            family: "fam-key-1".into(),
            metric_id: metric_id.into(),
            ship_n_samples: n,
            ship_value: value,
            ship_ts: unix_now(),
        }
    }

    fn after_sample(metric_id: &str, n: i64, value: f64, observable: bool) -> MetricSample {
        MetricSample {
            metric_id: metric_id.into(),
            n_samples: n,
            observable,
            value,
        }
    }

    // ---- signed-value extraction (the kairos settled-PnL shape + generic contract) ----

    #[test]
    fn extract_metric_value_prefers_explicit_then_settled_then_zero() {
        // explicit generic contract wins
        assert_eq!(
            extract_metric_value(
                &json!({"metric_value": 4.25, "settled_usd_per_window_24h": {"live": 9.0}})
            ),
            4.25
        );
        // kairos settled shape: sum the per-mode signed $/window values (paper + live)
        let payload = json!({
            "metric_id": "settled_usd_15m",
            "settled_usd_per_window_24h": {"paper": 1.5, "live": -0.5},
        });
        assert_eq!(extract_metric_value(&payload), 1.0);
        // neither present -> 0.0 (never a fabricated profit)
        assert_eq!(
            extract_metric_value(&json!({"metric_id": "m", "observable": true})),
            0.0
        );
    }

    #[test]
    fn sample_from_payload_requires_metric_id_and_observable_and_gated_n_samples() {
        let ok = json!({
            "metric_id": "settled_usd_15m", "observable": true, "n_samples": 3625,
            "settled_usd_per_window_24h": {"paper": 2.0, "live": 0.0},
        });
        let s = sample_from_payload(&ok).unwrap();
        assert_eq!(s.metric_id, "settled_usd_15m");
        assert_eq!(s.n_samples, 3625);
        assert!(s.observable);
        assert_eq!(s.value, 2.0);
        // missing metric_id / observable -> Err
        assert!(sample_from_payload(&json!({"observable": true, "n_samples": 1})).is_err());
        assert!(sample_from_payload(&json!({"metric_id": "m", "n_samples": 1})).is_err());
        // observable:true but missing n_samples -> Err (can't grade a delta without the row count)
        assert!(sample_from_payload(&json!({"metric_id": "m", "observable": true})).is_err());
        // observable:false may omit n_samples (a blind sample is allowed to be sparse)
        let blind = sample_from_payload(&json!({"metric_id": "m", "observable": false})).unwrap();
        assert!(!blind.observable);
        assert_eq!(blind.n_samples, 0);
    }

    // ---- ACCEPTANCE (c): the critique REFUSES to grade when observable:false or n_new==0 ----

    #[test]
    fn grade_refuses_when_after_sample_is_unobservable() {
        let ship = ship_pending("settled_usd_15m", 3600, 1.0);
        // plenty of new rows + a positive value move, but observable:false => NEVER graded
        let after = after_sample("settled_usd_15m", 3700, 5.0, false);
        assert_eq!(
            grade(&ship, &after),
            None,
            "an unobservable after-sample must not grade"
        );
    }

    #[test]
    fn grade_refuses_when_zero_new_samples_gained() {
        let ship = ship_pending("settled_usd_15m", 3625, 1.0);
        // observable, and the VALUE moved +4, but the metric gained ZERO new settled rows: a green
        // test cannot impute profit — the honesty floor refuses the grade.
        let after = after_sample("settled_usd_15m", 3625, 5.0, true);
        assert_eq!(
            grade(&ship, &after),
            None,
            "no new settled rows => no delta claimed even if the reported value moved"
        );
    }

    #[test]
    fn grade_refuses_when_metric_id_changed_or_ship_metric_empty() {
        // operator repointed the objective between ship and after => the ship baseline is invalid
        let ship = ship_pending("old_metric", 10, 1.0);
        let after = after_sample("new_metric", 100, 9.0, true);
        assert_eq!(
            grade(&ship, &after),
            None,
            "a changed metric_id must not grade"
        );
        // a ship whose own probe was unobservable (empty metric_id) can never be graded
        let blind_ship = ship_pending("", 0, 0.0);
        let after2 = after_sample("settled_usd_15m", 100, 9.0, true);
        assert_eq!(
            grade(&blind_ship, &after2),
            None,
            "empty ship metric_id must not grade"
        );
    }

    // ---- ACCEPTANCE (d): the min-samples floor guards noise ----

    #[test]
    fn grade_refuses_below_the_min_samples_floor() {
        let ship = ship_pending("settled_usd_15m", 3625, 1.0);
        // exactly MIN_NEW_SAMPLES-1 new rows: below the floor => HOLD (no grade, no thrash)
        let just_under = after_sample("settled_usd_15m", 3625 + MIN_NEW_SAMPLES - 1, 9.0, true);
        assert_eq!(
            grade(&ship, &just_under),
            None,
            "one below the min-samples floor must not grade (noise guard)"
        );
        // exactly MIN_NEW_SAMPLES new rows: at the inclusive floor => now it grades
        let at_floor = after_sample("settled_usd_15m", 3625 + MIN_NEW_SAMPLES, 9.0, true);
        assert!(
            grade(&ship, &at_floor).is_some(),
            "exactly MIN_NEW_SAMPLES new rows is the inclusive-pass floor"
        );
    }

    // ---- the WIN / NON-WIN verdict on real settled movement ----

    #[test]
    fn positive_delta_on_real_rows_is_a_win_negative_or_zero_is_a_non_win() {
        let ship = ship_pending("settled_usd_15m", 3600, 2.0);
        // +3.0 over 50 new rows -> WIN
        let (d, v) = grade(&ship, &after_sample("settled_usd_15m", 3650, 5.0, true)).unwrap();
        assert_eq!(v, Verdict::Win);
        assert!((d - 3.0).abs() < 1e-9);
        // exactly zero movement over real rows -> NON-WIN (shipped green, metric flat)
        let (d0, v0) = grade(&ship, &after_sample("settled_usd_15m", 3650, 2.0, true)).unwrap();
        assert_eq!(v0, Verdict::NonWin);
        assert!(d0.abs() < 1e-9);
        // a negative move -> NON-WIN
        let (dn, vn) = grade(&ship, &after_sample("settled_usd_15m", 3650, -1.0, true)).unwrap();
        assert_eq!(vn, Verdict::NonWin);
        assert!(dn < 0.0);
    }

    // ---- family down-weight table: NON-WIN demotes, WIN clears ----

    #[test]
    fn non_win_demotes_the_family_and_win_clears_it() {
        let c = test_ctx(None);
        assert!(!family_demoted(&c, "fam-a"), "cold family is not demoted");
        assert_eq!(family_penalty(&c, "fam-a"), 0);
        record_family_outcome(&c, "fam-a", Verdict::NonWin);
        assert!(family_demoted(&c, "fam-a"), "a non-win demotes the family");
        assert_eq!(family_penalty(&c, "fam-a"), 1);
        // a later WIN on the same family clears the demotion
        record_family_outcome(&c, "fam-a", Verdict::Win);
        assert!(!family_demoted(&c, "fam-a"), "a win clears the demotion");
        assert_eq!(family_penalty(&c, "fam-a"), 0);
        // counts accumulated
        let led = read_family_ledger(&fleet_runtime_dir(&c));
        assert_eq!(led["families"]["fam-a"]["non_wins"], json!(1));
        assert_eq!(led["families"]["fam-a"]["wins"], json!(1));
        // an empty family is inert (beautify/solomon lanes)
        record_family_outcome(&c, "", Verdict::NonWin);
        assert!(
            !family_ledger_path(&fleet_runtime_dir(&c)).exists() || {
                let l = read_family_ledger(&fleet_runtime_dir(&c));
                l["families"].get("").is_none()
            }
        );
    }

    #[test]
    fn expired_demotion_is_not_demoted() {
        let c = test_ctx(None);
        record_family_outcome(&c, "fam-b", Verdict::NonWin);
        // rewind demoted_until into the past
        let dir = fleet_runtime_dir(&c);
        let mut led = read_family_ledger(&dir);
        led["families"]["fam-b"]["demoted_until"] = json!(unix_now() as i64 - 10);
        write_family_ledger(&dir, &led);
        assert!(
            !family_demoted(&c, "fam-b"),
            "past demoted_until => selectable again"
        );
    }

    #[test]
    fn torn_family_ledger_fails_open() {
        let c = test_ctx(None);
        let dir = fleet_runtime_dir(&c);
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(family_ledger_path(&dir), "{ not json").unwrap();
        assert!(
            !family_demoted(&c, "fam-c"),
            "a torn ledger fails open (never demoted)"
        );
        record_family_outcome(&c, "fam-c", Verdict::NonWin);
        assert!(family_demoted(&c, "fam-c"), "and heals on the next write");
    }

    // ---- pending round-trip ----

    #[test]
    fn pending_round_trip_and_clear() {
        let c = test_ctx(None);
        assert!(read_pending(&c).is_none());
        let p = ship_pending("settled_usd_15m", 3625, 1.5);
        write_pending(&c, &p);
        assert_eq!(read_pending(&c), Some(p));
        clear_pending(&c);
        assert!(read_pending(&c).is_none());
    }

    // ---- resolve_due: HOLD below floor, RECORD at floor, drop stale ----

    #[test]
    fn resolve_due_holds_below_floor_then_records_and_feeds_back() {
        let mut c = test_ctx(None);
        write_pending(&c, &ship_pending("settled_usd_15m", 3600, 1.0));
        // below the noise floor: HOLD — no ledger line, pending preserved
        let under = after_sample("settled_usd_15m", 3600 + MIN_NEW_SAMPLES - 1, 9.0, true);
        resolve_due_with(&mut c, &under);
        assert!(
            read_pending(&c).is_some(),
            "held pending survives a below-floor pass"
        );
        assert!(
            !c.runtime.join(LEDGER_NAME).exists(),
            "no grade line recorded below the floor"
        );
        // window turns enough + a NEGATIVE move: NON-WIN recorded, pending cleared, family demoted
        let after = after_sample("settled_usd_15m", 3600 + MIN_NEW_SAMPLES + 40, -2.0, true);
        resolve_due_with(&mut c, &after);
        assert!(
            read_pending(&c).is_none(),
            "recorded grade clears the pending"
        );
        let ledger = std::fs::read_to_string(c.runtime.join(LEDGER_NAME)).unwrap();
        let rec: Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
        assert_eq!(rec["verdict"], json!("non_win"));
        assert_eq!(rec["metric_id"], json!("settled_usd_15m"));
        assert!(rec["delta"].as_f64().unwrap() < 0.0);
        assert_eq!(rec["n_settled_rows"], json!(MIN_NEW_SAMPLES + 40));
        assert!(
            family_demoted(&c, "fam-key-1"),
            "the non-win down-weighted its family"
        );
    }

    #[test]
    fn resolve_due_drops_a_stale_pending_unrecorded() {
        let mut c = test_ctx(None);
        let mut p = ship_pending("settled_usd_15m", 3600, 1.0);
        p.ship_ts = unix_now() - MAX_WAIT_SECS - 1.0; // aged out
        write_pending(&c, &p);
        // even with a gradeable after-sample, an aged-out pending is dropped UNRECORDED
        let after = after_sample("settled_usd_15m", 3700, 9.0, true);
        resolve_due_with(&mut c, &after);
        assert!(read_pending(&c).is_none(), "stale pending dropped");
        assert!(
            !c.runtime.join(LEDGER_NAME).exists(),
            "a stale pending grades NOTHING (no fabricated delta)"
        );
    }

    #[test]
    fn resolve_due_is_a_noop_without_a_pending() {
        let mut c = test_ctx(None);
        resolve_due_with(&mut c, &after_sample("settled_usd_15m", 3700, 9.0, true));
        assert!(!c.runtime.join(LEDGER_NAME).exists());
    }

    // ---- no freshness config => the whole layer is inert ----

    #[test]
    fn no_objective_lane_stamps_no_pending() {
        // a lane with no freshness config (no_objective sentinel) => sample_metric None => inert
        let mut c = test_ctx(Some(
            json!([{ "name": "critiquetest", "no_objective": true }]),
        ));
        note_ship(&mut c, "deadbeefcafe", "fam-x");
        assert!(
            read_pending(&c).is_none(),
            "no freshness cfg => no critique opened"
        );
    }

    // ---- note_ship: a real probe stamps a pending keyed to the metric_id ----

    #[cfg(windows)]
    const KAIROS_LIKE_PROBE: &str = r#"echo {"metric_id":"settled_usd_15m","latest_ts":1783486665.28,"n_samples":3625,"observable":true,"settled_usd_per_window_24h":{"paper":2.5,"live":-0.5}}"#;
    #[cfg(not(windows))]
    const KAIROS_LIKE_PROBE: &str = r#"printf '%s\n' '{"metric_id":"settled_usd_15m","latest_ts":1783486665.28,"n_samples":3625,"observable":true,"settled_usd_per_window_24h":{"paper":2.5,"live":-0.5}}'"#;

    #[test]
    fn note_ship_samples_a_real_probe_and_stamps_the_metric_snapshot() {
        let rows = json!([{ "name": "critiquetest", "freshness": {"cmd": KAIROS_LIKE_PROBE} }]);
        let mut c = test_ctx(Some(rows));
        note_ship(&mut c, "1122334455667788", "improve-sizing-truth");
        let p = read_pending(&c).expect("a freshness-configured lane stamps a pending on ship");
        assert_eq!(p.metric_id, "settled_usd_15m");
        assert_eq!(p.ship_n_samples, 3625);
        assert!(
            (p.ship_value - 2.0).abs() < 1e-9,
            "paper 2.5 + live -0.5 = 2.0"
        );
        assert_eq!(p.family, "improve-sizing-truth");
        assert_eq!(p.change_id, "1122334455667788");
    }

    /// END-TO-END ACCEPTANCE (a): a kairos-like ship then a settled-window turn produces a graded
    /// outcome_critique.jsonl line COMPUTED FROM the probe's settled-row count + signed value —
    /// never from test status. This drives note_ship (real probe) then resolve_due (after sample).
    #[test]
    fn end_to_end_ship_then_settled_window_records_a_delta_from_real_rows() {
        let rows = json!([{ "name": "critiquetest", "freshness": {"cmd": KAIROS_LIKE_PROBE} }]);
        let mut c = test_ctx(Some(rows));
        // SHIP: probe reads n=3625, value=2.0
        note_ship(&mut c, "shipsha00", "grow-live-pnl");
        // AFTER the window: the settled table gained real rows and live PnL improved (+3.0).
        let after = after_sample("settled_usd_15m", 3625 + 60, 5.0, true);
        resolve_due_with(&mut c, &after);
        let ledger = std::fs::read_to_string(c.runtime.join(LEDGER_NAME))
            .expect("a graded line was appended");
        let rec: Value = serde_json::from_str(ledger.lines().next().unwrap()).unwrap();
        assert_eq!(rec["change_id"], json!("shipsha00"));
        assert_eq!(rec["metric_id"], json!("settled_usd_15m"));
        assert_eq!(
            rec["n_settled_rows"],
            json!(60),
            "COMPUTED from the settled-row count"
        );
        assert!(
            (rec["delta"].as_f64().unwrap() - 3.0).abs() < 1e-9,
            "5.0 - 2.0 = 3.0"
        );
        assert_eq!(rec["verdict"], json!("win"));
        assert!(read_pending(&c).is_none());
    }
}
