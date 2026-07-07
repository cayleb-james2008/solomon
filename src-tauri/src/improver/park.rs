//! Event-driven wake + max-park liveness floor (RSI v3 WS5 #3; 2026-07-07 audit A.1).
//!
//! The per-lane improver loop historically slept a FIXED `interval` (default 120s, kairos 900s)
//! between iterations regardless of what just happened or whether the objective gained new data —
//! a value-blind pacing that both wastes wall clock (a shipped iteration that could immediately
//! pick up the next ready item still sleeps the full interval) and, at the fleet level, leaves the
//! only liveness signal a fixed cron with no upper bound tying park length to real events.
//!
//! This module demotes that blind sleep to a bounded, event-aware park:
//!
//!   * [`MAX_PARK_FLOOR_S`] — the hard ceiling on any single park (the "max-park liveness floor").
//!     A lane never sleeps longer than this even if its configured `interval` is larger, so the
//!     loop re-observes the world (stop flag, config edits, freshness) at least this often. 5 min
//!     is the safe default (matches the Sentinel cadence).
//!
//!   * [`park_decision`] — the pure `continue_or_park` self-grade run at the END of an iteration:
//!     given the terminal heartbeat, decide whether to CONTINUE immediately (a productive ship may
//!     have unblocked more ready work) or PARK for the bounded window (a noop/revert/error/idle
//!     should back off, not hot-loop). It never returns a park longer than the floor and never
//!     shortens a real KILL/stop response (the caller checks the stop flag every 1s inside the
//!     park regardless).
//!
//!   * [`WakeSource`] — the taxonomy of what ends a park, for logging + routing. A park ends on the
//!     first of: an operator KILL/stop (`Kill`), the objective gaining new data since the park
//!     began (`FreshData`), or the floor elapsing (`FloorElapsed`). `OpsChange` / `ProviderRecovery`
//!     are reserved for the fleet-plane wake (`fleet::once` on an ops-verdict flip or a
//!     budget-ledger provider un-park); the per-lane loop routes `Kill` / `FreshData` / `FloorElapsed`.
//!
//! Nothing here weakens a gate: the freshness short-circuit, KILL/DRAIN, blast-radius, skeptic, and
//! operator-pause guards all run inside `one_iteration` exactly as before. This only governs the
//! sleep BETWEEN iterations, and only ever makes it shorter or equal, never longer.

use serde_json::Value;
use std::path::Path;

/// The hard ceiling (seconds) on any single inter-iteration park — the "max-park liveness floor".
/// A lane whose configured `interval` exceeds this parks at most this long, so it re-observes the
/// stop flag / live config / freshness at least every 5 minutes (the Sentinel cadence). Safe
/// default per the audit; a lower value only makes the loop MORE responsive.
pub const MAX_PARK_FLOOR_S: i64 = 300;

/// What ended (or would end) a park. Logged for observability and used to route the fleet-plane
/// wake; the per-lane loop produces `Kill` / `FreshData` / `FloorElapsed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSource {
    /// An operator KILL / Stop sentinel appeared — end the park now and let the loop exit.
    Kill,
    /// The tier-1 objective gained new data (freshness ledger advanced) since the park began.
    FreshData,
    /// An ops-plane verdict changed color (fleet-plane wake source; not produced per-lane here).
    OpsChange,
    /// A parked provider endpoint recovered / un-parked (fleet-plane wake source; budget ledger).
    ProviderRecovery,
    /// The max-park floor elapsed with no earlier event — the ordinary bounded-park timeout.
    FloorElapsed,
}

impl WakeSource {
    /// Stable snake_case tag for logs / events.
    pub fn tag(self) -> &'static str {
        match self {
            WakeSource::Kill => "kill",
            WakeSource::FreshData => "fresh_data",
            WakeSource::OpsChange => "ops_change",
            WakeSource::ProviderRecovery => "provider_recovery",
            WakeSource::FloorElapsed => "floor_elapsed",
        }
    }
}

/// The `continue_or_park` self-grade output: how long to park (seconds; 0 == continue immediately)
/// and a short human reason for the log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkPlan {
    pub park_s: i64,
    pub why: &'static str,
}

/// A lightweight snapshot of the objective's freshness ledger, taken before a park so the loop can
/// detect new data arriving DURING the park (an early `FreshData` wake). Reads
/// `runtime/<name>/freshness.json` — the exact file `freshness::short_circuit` maintains. `None`
/// when the lane has no freshness ledger yet (feature off / first cycle): such a lane simply parks
/// the floor with no early freshness wake, identical to legacy pacing but bounded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FreshnessMark {
    pub last_seen_ts: f64,
    pub last_n_samples: i64,
}

/// Read the current freshness ledger mark from a raw ledger JSON value (the parsed
/// `runtime/<name>/freshness.json`). Pure so it is unit-testable without touching disk. `None` when
/// the value carries neither field (an absent / empty / unrelated ledger).
pub fn freshness_mark_from(ledger: &Value) -> Option<FreshnessMark> {
    let ts = ledger.get("last_seen_ts").and_then(Value::as_f64);
    let n = ledger.get("last_n_samples").and_then(Value::as_i64);
    match (ts, n) {
        (None, None) => None,
        _ => Some(FreshnessMark {
            last_seen_ts: ts.unwrap_or(0.0),
            last_n_samples: n.unwrap_or(0),
        }),
    }
}

/// Read the freshness mark from a ledger file on disk (`runtime/<name>/freshness.json`). A missing
/// or unparseable file, or one without the ledger fields, is `None` — the lane simply gets no early
/// freshness wake (it still parks the bounded floor). Never panics.
pub fn read_freshness_mark(ledger_path: &Path) -> Option<FreshnessMark> {
    let text = std::fs::read_to_string(ledger_path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    freshness_mark_from(&v)
}

/// True iff `now` shows strictly newer objective data than the pre-park `before` mark — a newer
/// latest timestamp OR more samples. Either alone is sufficient (mirrors `freshness::is_fresh`'s
/// OR). Used to end a park early with [`WakeSource::FreshData`]. A missing `before` (the lane had
/// no ledger when the park began) never fires an early wake — there is no baseline to advance past.
pub fn has_fresh_data(before: Option<FreshnessMark>, now: Option<FreshnessMark>) -> bool {
    match (before, now) {
        (Some(b), Some(n)) => n.last_n_samples > b.last_n_samples || n.last_seen_ts > b.last_seen_ts,
        _ => false,
    }
}

/// The pure `continue_or_park` self-grade. Given the terminal heartbeat of the iteration that just
/// finished and the lane's configured `interval`, decide the inter-iteration park.
///
/// Rule:
///   * A PRODUCTIVE SHIP (`status == "sleeping"` with a real landed PR on `last_pr`) parks 0s —
///     continue immediately, because landing an item can unblock the next ready backlog item and
///     there is no reason to idle the full interval after real forward progress.
///   * Everything else (noop / reverted / blocked / error / idle / stopped, or a `sleeping` with no
///     landed PR) parks `min(interval, MAX_PARK_FLOOR_S)` — bounded backoff so a stuck/idle lane
///     neither hot-loops nor over-sleeps.
///
/// The result is ALWAYS `0 <= park_s <= MAX_PARK_FLOOR_S`, so this can only ever make the loop as
/// responsive as, or more responsive than, the legacy fixed-interval sleep — never less.
pub fn park_decision(hb: &Value, interval: i64) -> ParkPlan {
    let bounded = interval.clamp(0, MAX_PARK_FLOOR_S);
    let status = hb.get("status").and_then(Value::as_str).unwrap_or("");
    if status == "sleeping" && landed_a_pr(hb) {
        return ParkPlan {
            park_s: 0,
            why: "shipped — continue immediately (next item may be ready)",
        };
    }
    ParkPlan {
        park_s: bounded,
        why: "no forward progress this cycle — bounded park to the max-park floor",
    }
}

/// True iff the terminal heartbeat's `last_pr` describes a REAL landed PR (a numeric PR number or a
/// non-"local..." state) rather than a noop/local drop. A noop iteration writes `last_pr: null`; a
/// stopped/blocked/local iteration writes a `state` beginning "local". Only a genuine ship should
/// trigger the immediate-continue path.
fn landed_a_pr(hb: &Value) -> bool {
    let pr = match hb.get("last_pr") {
        Some(Value::Object(o)) => o,
        _ => return false,
    };
    // A real PR carries a numeric `number`. Auto-merge/push modes may carry only a non-local
    // `state` (e.g. "merged", "open (...)"), so accept either signal but reject explicit "local".
    if pr.get("number").and_then(Value::as_i64).is_some() {
        return true;
    }
    match pr.get("state").and_then(Value::as_str) {
        Some(s) => !s.to_lowercase().starts_with("local"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn floor_is_five_minutes() {
        assert_eq!(MAX_PARK_FLOOR_S, 300);
    }

    #[test]
    fn wake_source_tags_are_stable() {
        assert_eq!(WakeSource::Kill.tag(), "kill");
        assert_eq!(WakeSource::FreshData.tag(), "fresh_data");
        assert_eq!(WakeSource::OpsChange.tag(), "ops_change");
        assert_eq!(WakeSource::ProviderRecovery.tag(), "provider_recovery");
        assert_eq!(WakeSource::FloorElapsed.tag(), "floor_elapsed");
    }

    // ---- park_decision: the continue_or_park self-grade ----

    #[test]
    fn shipped_pr_continues_immediately() {
        // A real landed PR (numeric number) => park 0, continue.
        let hb = json!({
            "status": "sleeping",
            "last_pr": {"number": 42, "url": "https://x/pr/42", "branch": "rsi/iter-1"}
        });
        assert_eq!(park_decision(&hb, 900).park_s, 0);
    }

    #[test]
    fn shipped_via_merged_state_continues_immediately() {
        // auto-merge lane: no number yet, state "merged" (non-local) => continue.
        let hb = json!({"status": "sleeping", "last_pr": {"state": "merged", "branch": "rsi/iter-2"}});
        assert_eq!(park_decision(&hb, 120).park_s, 0);
    }

    #[test]
    fn noop_sleeping_parks_the_bounded_floor() {
        // A noop drop ends status=sleeping with last_pr: null => park the floor, never 0.
        let hb = json!({"status": "sleeping", "last_pr": Value::Null, "last_summary": "Pi made no changes"});
        let plan = park_decision(&hb, 900);
        assert_eq!(plan.park_s, MAX_PARK_FLOOR_S, "over-long interval clamps to the floor");
    }

    #[test]
    fn local_stopped_pr_does_not_count_as_shipped() {
        // A stop-before-PR ends with a "local (...)" state => bounded park, not continue.
        let hb = json!({
            "status": "stopped",
            "last_pr": {"number": Value::Null, "state": "local (stopped before PR)", "branch": "rsi/iter-3"}
        });
        assert_eq!(park_decision(&hb, 120).park_s, 120);
    }

    #[test]
    fn error_and_idle_park_the_bounded_window() {
        for status in ["error", "idle", "blocked"] {
            let hb = json!({"status": status});
            let plan = park_decision(&hb, 120);
            assert_eq!(plan.park_s, 120, "status={status} parks the bounded interval");
        }
    }

    #[test]
    fn park_is_never_longer_than_the_floor_and_never_negative() {
        for interval in [-5, 0, 1, 120, 900, 100_000] {
            let hb = json!({"status": "error"});
            let p = park_decision(&hb, interval).park_s;
            assert!((0..=MAX_PARK_FLOOR_S).contains(&p), "interval={interval} -> park {p} out of [0,{MAX_PARK_FLOOR_S}]");
        }
    }

    #[test]
    fn short_interval_is_respected_when_under_the_floor() {
        // A lane with a small interval keeps it (bounded only caps the top end).
        let hb = json!({"status": "error"});
        assert_eq!(park_decision(&hb, 30).park_s, 30);
    }

    // ---- freshness mark + fresh-data wake ----

    #[test]
    fn freshness_mark_absent_when_no_fields() {
        assert_eq!(freshness_mark_from(&json!({})), None);
        assert_eq!(freshness_mark_from(&json!({"metric_id": "x"})), None);
    }

    #[test]
    fn freshness_mark_reads_ledger_fields() {
        let m = freshness_mark_from(&json!({"last_seen_ts": 1783453420.0, "last_n_samples": 2788})).unwrap();
        assert_eq!(m.last_n_samples, 2788);
        assert_eq!(m.last_seen_ts, 1783453420.0);
    }

    #[test]
    fn has_fresh_data_fires_on_more_samples_or_newer_ts() {
        let before = Some(FreshnessMark { last_seen_ts: 100.0, last_n_samples: 10 });
        // more samples
        assert!(has_fresh_data(before, Some(FreshnessMark { last_seen_ts: 100.0, last_n_samples: 11 })));
        // newer ts
        assert!(has_fresh_data(before, Some(FreshnessMark { last_seen_ts: 101.0, last_n_samples: 10 })));
        // no change => no wake
        assert!(!has_fresh_data(before, Some(FreshnessMark { last_seen_ts: 100.0, last_n_samples: 10 })));
        // fewer samples (ledger reset / metric repoint) => not a "fresh" advance
        assert!(!has_fresh_data(before, Some(FreshnessMark { last_seen_ts: 100.0, last_n_samples: 3 })));
    }

    #[test]
    fn has_fresh_data_never_fires_without_a_baseline() {
        let now = Some(FreshnessMark { last_seen_ts: 999.0, last_n_samples: 999 });
        assert!(!has_fresh_data(None, now), "no pre-park baseline => no early wake");
        assert!(!has_fresh_data(now, None));
        assert!(!has_fresh_data(None, None));
    }
}
