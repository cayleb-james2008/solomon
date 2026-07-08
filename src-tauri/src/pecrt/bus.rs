//! The shared wake-bus — a source-agnostic generalization of the per-lane event-driven park.
//!
//! `improver::park` already implements event-driven wake FOR ONE LANE: it watches the freshness
//! ledger file, ends a bounded park on the first of {operator KILL, fresh objective data, floor
//! elapsed}, and taxonomizes the wake with [`WakeSource`]. This module LIFTS that mechanism into a
//! bus that a continuous reasoning thread can wait on across MANY sources at once — file-append
//! watchers (a lane wrote a new observation / heartbeat line), sqlite-row watchers (a row-count
//! advanced), and signal-file watchers (a sentinel appeared, e.g. STOP) — while keeping the EXACT
//! same [`WakeSource`] taxonomy and the EXACT same freshness OR-rule as `park`, so the two can never
//! disagree about what "fresh data" means or what ends a park.
//!
//! ## The standardized event schema
//!
//! The freshness PROBE contract (`improver::freshness` L18-19) is the standard event shape on the
//! bus: the last non-empty stdout line of a freshness probe is JSON
//!   `{"metric_id": str, "latest_ts": float, "n_samples": int, "observable": bool}`
//! [`FreshnessEvent`] models exactly that. Note this is the PROBE schema (`latest_ts`/`n_samples`),
//! distinct from the persisted LEDGER schema (`last_seen_ts`/`last_n_samples`) that `park`'s
//! `FreshnessMark` reads; both describe the same underlying metric, and [`FreshnessEvent::advanced`]
//! applies the SAME OR-rule as `park::has_fresh_data` (more samples OR newer ts). An UNOBSERVABLE
//! event never counts as an advance — an unobservable tier-1 metric is RED, not fresh (the whole
//! point of the freshness gate).
//!
//! ## Pure decision
//!
//! [`next_wake`] is pure: given the pre-wait baseline of each watched source and its current reading,
//! plus the `max_park` ceiling, it returns the [`WakeReason`] that would end the wait — WITHOUT
//! sleeping. The caller owns the 1s-granular poll loop (as `run.rs` already does), calling `next_wake`
//! each tick; the moment it returns anything but `FloorElapsed`, the wait ends. This keeps ALL timing
//! in the caller and ALL policy here, unit-testable with zero IO and zero threads.
//!
//! ## Scope (Layer 0) — what is and isn't ported
//!
//! This layer generalizes park's WAIT-END decision (`has_fresh_data` + the inner KILL/FreshData/floor
//! poll loop) into [`next_wake`]. It deliberately does NOT port park's `park_decision` — the
//! post-iteration `continue_or_park` PRODUCTIVITY self-grade (ship => continue 0s, noop => bounded
//! backoff). That self-grade is a per-lane loop concern that `improver::park::park_decision` still
//! owns unchanged; the thread-plane productivity/anti-compulsion policy (park is PREFERRED when
//! blocked on external truth) lives in the stable-prefix contract ([`crate::pecrt::warm::STABLE_PREFIX`])
//! for Layer 0 and is a candidate to lift into a bus-level `continue_or_park` in a later layer. Naming
//! this omission explicitly so the boundary is honest rather than implied-complete.
//!
//! ## Safety
//!
//! Nothing here weakens a gate. A wake is only a SCHEDULING signal — it decides WHEN the thread looks
//! at the world, never WHAT it may do. KILL is always the highest-priority wake and is never delayed
//! or suppressed; the bus can only ever make the thread MORE responsive to a stop, never less.

use serde_json::Value;

/// What ended (or would end) a wait on the bus. A strict SUPERSET of `improver::park::WakeSource`:
/// the first five variants are byte-tag-identical to park's taxonomy (so a park wake and a bus wake
/// log the same tag), and the last two name the GENERALIZED watcher origins the bus adds beyond the
/// single freshness-file watcher park had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSource {
    /// An operator KILL / Stop sentinel appeared — highest priority, ends the wait now.
    Kill,
    /// A watched objective gained new data (a freshness event advanced) since the wait began.
    FreshData,
    /// An ops-plane verdict changed color (fleet-plane wake source).
    OpsChange,
    /// A parked provider endpoint recovered / un-parked (budget ledger).
    ProviderRecovery,
    /// The max-park floor elapsed with no earlier event — the ordinary bounded timeout.
    FloorElapsed,
    /// A watched append-only file grew (a lane wrote a new observation-log / heartbeat / jsonl line).
    /// The generalized form of park's single freshness-file watch.
    FileAppend,
    /// A watched sqlite table's row count advanced (a new row landed).
    SqliteRow,
}

impl WakeSource {
    /// Stable snake_case tag for logs / events. The first five MUST match
    /// `improver::park::WakeSource::tag` byte-for-byte (a shared-vocabulary invariant, pinned by test
    /// `bus_tags_superset_park_tags`).
    pub fn tag(self) -> &'static str {
        match self {
            WakeSource::Kill => "kill",
            WakeSource::FreshData => "fresh_data",
            WakeSource::OpsChange => "ops_change",
            WakeSource::ProviderRecovery => "provider_recovery",
            WakeSource::FloorElapsed => "floor_elapsed",
            WakeSource::FileAppend => "file_append",
            WakeSource::SqliteRow => "sqlite_row",
        }
    }

    /// Priority rank — LOWER wins when two sources are simultaneously ready in a single tick. KILL is
    /// always 0 (never outranked); FloorElapsed is always last (only fires when nothing else did).
    /// This makes `next_wake` deterministic regardless of source ordering in the input.
    pub fn priority(self) -> u8 {
        match self {
            WakeSource::Kill => 0,
            WakeSource::FreshData => 1,
            WakeSource::OpsChange => 2,
            WakeSource::ProviderRecovery => 3,
            WakeSource::FileAppend => 4,
            WakeSource::SqliteRow => 5,
            WakeSource::FloorElapsed => 6,
        }
    }
}

/// The standardized freshness EVENT on the bus — the exact JSON the freshness probe emits on its last
/// stdout line (`improver::freshness` L18-19): `{metric_id, latest_ts, n_samples, observable}`.
#[derive(Debug, Clone, PartialEq)]
pub struct FreshnessEvent {
    pub metric_id: String,
    pub latest_ts: f64,
    pub n_samples: i64,
    pub observable: bool,
}

impl FreshnessEvent {
    /// Parse the standardized event JSON. `observable` defaults to FALSE when absent/non-bool — an
    /// event we cannot confirm is observable is treated as unobservable (RED, never fresh), matching
    /// the freshness gate's fail-closed asymmetry. `None` when neither an id nor the numeric fields
    /// are present (an absent / unrelated blob).
    pub fn from_value(v: &Value) -> Option<FreshnessEvent> {
        let metric_id = v.get("metric_id").and_then(Value::as_str);
        let latest_ts = v.get("latest_ts").and_then(Value::as_f64);
        let n_samples = v.get("n_samples").and_then(Value::as_i64);
        if metric_id.is_none() && latest_ts.is_none() && n_samples.is_none() {
            return None;
        }
        Some(FreshnessEvent {
            metric_id: metric_id.unwrap_or("").to_string(),
            latest_ts: latest_ts.unwrap_or(0.0),
            n_samples: n_samples.unwrap_or(0),
            observable: v.get("observable").and_then(Value::as_bool).unwrap_or(false),
        })
    }

    /// True iff `self` shows a strictly newer reading than the pre-wait `before` baseline AND is
    /// observable. SAME OR-rule as `improver::park::has_fresh_data` (more samples OR newer ts), plus
    /// the observability guard: an unobservable current reading is NEVER an advance, and a missing
    /// baseline never fires (no floor to advance past). This is the freshness wake predicate.
    pub fn advanced(&self, before: Option<&FreshnessEvent>) -> bool {
        if !self.observable {
            return false;
        }
        match before {
            Some(b) => self.n_samples > b.n_samples || self.latest_ts > b.latest_ts,
            None => false,
        }
    }
}

/// One watched source's reading at a single tick: its kind and whether it FIRED (advanced past its
/// pre-wait baseline) this tick. `next_wake` consumes a slice of these each poll. Building a
/// `WatchSource` from raw IO (statting a file's len, counting sqlite rows, checking a sentinel path,
/// re-probing freshness) is the caller's job — keeping this decision pure and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSource {
    pub source: WakeSource,
    pub fired: bool,
}

impl WatchSource {
    pub fn new(source: WakeSource, fired: bool) -> Self {
        WatchSource { source, fired }
    }
}

/// The output of a bus wait tick: which source ends the wait, plus a short human reason for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeReason {
    pub source: WakeSource,
    pub why: &'static str,
}

/// The pure wake decision. Given the readings of every watched `source` at this poll tick, plus
/// `elapsed_s` (how long the wait has run) and `max_park` (the ceiling), return the `WakeReason` that
/// ends the wait THIS tick.
///
/// Rule (deterministic, priority-ordered):
///   * If any FIRED source exists, the wait ends on the one with the LOWEST `priority()` — so a
///     simultaneous KILL + FreshData tick always yields KILL, never FreshData. Ordering of `sources`
///     in the slice is irrelevant.
///   * Else if `elapsed_s >= max_park` (clamped to >= 0), the wait ends on `FloorElapsed`.
///   * Else no wake this tick (`None`) — the caller sleeps 1s and polls again.
///
/// `max_park` is clamped to `>= 0`; a zero or negative ceiling means "never park past this tick",
/// so with no fired source and `elapsed_s >= 0` it returns `FloorElapsed` immediately. This can only
/// ever make the wait as short as, or shorter than, the ceiling — never longer, and never past a
/// KILL. It exactly generalizes `park`'s inner 1s poll loop (KILL, then FreshData, then floor) to N
/// sources.
pub fn next_wake(sources: &[WatchSource], elapsed_s: i64, max_park: i64) -> Option<WakeReason> {
    let ceiling = max_park.max(0);

    // First: any fired source, lowest priority wins (KILL always outranks the rest).
    let winner = sources
        .iter()
        .filter(|w| w.fired)
        .min_by_key(|w| w.source.priority());
    if let Some(w) = winner {
        return Some(WakeReason {
            source: w.source,
            why: fired_why(w.source),
        });
    }

    // Else: the bounded floor.
    if elapsed_s >= ceiling {
        return Some(WakeReason {
            source: WakeSource::FloorElapsed,
            why: "max-park floor elapsed with no earlier event",
        });
    }

    // Else: keep waiting.
    None
}

fn fired_why(source: WakeSource) -> &'static str {
    match source {
        WakeSource::Kill => "operator KILL/Stop — end wait now",
        WakeSource::FreshData => "watched objective gained new data (freshness advanced)",
        WakeSource::OpsChange => "ops-plane verdict changed color",
        WakeSource::ProviderRecovery => "a parked provider recovered",
        WakeSource::FileAppend => "a watched append-only file grew (new line)",
        WakeSource::SqliteRow => "a watched sqlite table gained a row",
        WakeSource::FloorElapsed => "max-park floor elapsed with no earlier event",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- WakeSource taxonomy: shared vocabulary with park ----

    #[test]
    fn bus_tags_superset_park_tags() {
        // The first five tags MUST equal park::WakeSource::tag byte-for-byte (shared vocabulary).
        assert_eq!(WakeSource::Kill.tag(), "kill");
        assert_eq!(WakeSource::FreshData.tag(), "fresh_data");
        assert_eq!(WakeSource::OpsChange.tag(), "ops_change");
        assert_eq!(WakeSource::ProviderRecovery.tag(), "provider_recovery");
        assert_eq!(WakeSource::FloorElapsed.tag(), "floor_elapsed");
        // D9: `improver::park::WakeSource` is now a RE-EXPORT of this enum (the duplicate was
        // retired), so `P` resolves to this same type — the two can no longer drift by construction.
        // This pins that the shared path still names the identical variants + tags a park caller uses.
        use crate::improver::park::WakeSource as P;
        assert_eq!(WakeSource::Kill.tag(), P::Kill.tag());
        assert_eq!(WakeSource::FreshData.tag(), P::FreshData.tag());
        assert_eq!(WakeSource::OpsChange.tag(), P::OpsChange.tag());
        assert_eq!(WakeSource::ProviderRecovery.tag(), P::ProviderRecovery.tag());
        assert_eq!(WakeSource::FloorElapsed.tag(), P::FloorElapsed.tag());
        // The two generalized additions.
        assert_eq!(WakeSource::FileAppend.tag(), "file_append");
        assert_eq!(WakeSource::SqliteRow.tag(), "sqlite_row");
    }

    #[test]
    fn kill_has_top_priority_and_floor_is_last() {
        assert_eq!(WakeSource::Kill.priority(), 0);
        for s in [
            WakeSource::FreshData,
            WakeSource::OpsChange,
            WakeSource::ProviderRecovery,
            WakeSource::FileAppend,
            WakeSource::SqliteRow,
            WakeSource::FloorElapsed,
        ] {
            assert!(s.priority() > WakeSource::Kill.priority());
        }
        // Floor is strictly last so it only wins when nothing else fired.
        for s in [
            WakeSource::Kill,
            WakeSource::FreshData,
            WakeSource::OpsChange,
            WakeSource::ProviderRecovery,
            WakeSource::FileAppend,
            WakeSource::SqliteRow,
        ] {
            assert!(WakeSource::FloorElapsed.priority() > s.priority());
        }
    }

    // ---- FreshnessEvent: parse + advance (matches park::has_fresh_data OR-rule) ----

    #[test]
    fn freshness_event_parses_probe_schema() {
        let e = FreshnessEvent::from_value(&json!({
            "metric_id": "settled_usd_15m", "latest_ts": 1783455180.07, "n_samples": 2863, "observable": true
        }))
        .unwrap();
        assert_eq!(e.metric_id, "settled_usd_15m");
        assert_eq!(e.n_samples, 2863);
        assert!(e.observable);
    }

    #[test]
    fn freshness_event_absent_when_no_fields() {
        assert_eq!(FreshnessEvent::from_value(&json!({})), None);
        assert_eq!(FreshnessEvent::from_value(&json!({"observable": true})), None);
    }

    #[test]
    fn unobservable_defaults_and_never_advances() {
        // observable absent => false.
        let e = FreshnessEvent::from_value(&json!({"metric_id": "x", "n_samples": 5})).unwrap();
        assert!(!e.observable);
        let before = FreshnessEvent { metric_id: "x".into(), latest_ts: 0.0, n_samples: 1, observable: true };
        // even though 5 > 1, an unobservable current reading is NEVER an advance (RED not fresh).
        assert!(!e.advanced(Some(&before)));
    }

    #[test]
    fn advance_uses_same_or_rule_as_park() {
        let before = FreshnessEvent { metric_id: "x".into(), latest_ts: 100.0, n_samples: 10, observable: true };
        let more_samples = FreshnessEvent { metric_id: "x".into(), latest_ts: 100.0, n_samples: 11, observable: true };
        let newer_ts = FreshnessEvent { metric_id: "x".into(), latest_ts: 101.0, n_samples: 10, observable: true };
        let same = FreshnessEvent { metric_id: "x".into(), latest_ts: 100.0, n_samples: 10, observable: true };
        let fewer = FreshnessEvent { metric_id: "x".into(), latest_ts: 100.0, n_samples: 3, observable: true };
        assert!(more_samples.advanced(Some(&before)));
        assert!(newer_ts.advanced(Some(&before)));
        assert!(!same.advanced(Some(&before)), "no change => no advance");
        assert!(!fewer.advanced(Some(&before)), "ledger reset/repoint => not an advance");
        assert!(!more_samples.advanced(None), "no baseline => never advances");
    }

    // ---- next_wake: pure poll decision ----

    #[test]
    fn no_fired_source_before_floor_keeps_waiting() {
        let sources = [
            WatchSource::new(WakeSource::FreshData, false),
            WatchSource::new(WakeSource::FileAppend, false),
        ];
        assert_eq!(next_wake(&sources, 10, 300), None);
    }

    #[test]
    fn floor_elapses_when_no_source_fires() {
        let sources = [WatchSource::new(WakeSource::FreshData, false)];
        let r = next_wake(&sources, 300, 300).unwrap();
        assert_eq!(r.source, WakeSource::FloorElapsed);
        // and past the floor too
        assert_eq!(next_wake(&sources, 999, 300).unwrap().source, WakeSource::FloorElapsed);
    }

    #[test]
    fn kill_outranks_a_simultaneous_fresh_data() {
        // Both fired in one tick; KILL must win regardless of slice order.
        let a = [
            WatchSource::new(WakeSource::FreshData, true),
            WatchSource::new(WakeSource::Kill, true),
        ];
        let b = [
            WatchSource::new(WakeSource::Kill, true),
            WatchSource::new(WakeSource::FreshData, true),
        ];
        assert_eq!(next_wake(&a, 5, 300).unwrap().source, WakeSource::Kill);
        assert_eq!(next_wake(&b, 5, 300).unwrap().source, WakeSource::Kill);
    }

    #[test]
    fn a_fired_source_beats_the_floor_even_at_the_ceiling() {
        // FreshData fired exactly at the floor tick — the event wins, not FloorElapsed.
        let sources = [WatchSource::new(WakeSource::FreshData, true)];
        assert_eq!(next_wake(&sources, 300, 300).unwrap().source, WakeSource::FreshData);
    }

    #[test]
    fn zero_or_negative_ceiling_floors_immediately_but_still_yields_to_kill() {
        let none_fired = [WatchSource::new(WakeSource::FileAppend, false)];
        assert_eq!(next_wake(&none_fired, 0, 0).unwrap().source, WakeSource::FloorElapsed);
        assert_eq!(next_wake(&none_fired, 0, -5).unwrap().source, WakeSource::FloorElapsed);
        // a KILL in the same tick still outranks the immediate floor.
        let kill = [
            WatchSource::new(WakeSource::FileAppend, false),
            WatchSource::new(WakeSource::Kill, true),
        ];
        assert_eq!(next_wake(&kill, 0, 0).unwrap().source, WakeSource::Kill);
    }

    #[test]
    fn empty_sources_just_waits_out_the_floor() {
        assert_eq!(next_wake(&[], 10, 300), None);
        assert_eq!(next_wake(&[], 300, 300).unwrap().source, WakeSource::FloorElapsed);
    }

    // ---- live-loop drive model (D9): the exact tick loop run.rs runs on the bus ----

    /// A zero-IO stand-in for the live loop's inner park: it polls `next_wake` once per simulated
    /// tick, driving each registered watcher from a per-tick closure, and returns `(source, ticks)`
    /// — the winning WakeSource and how many ticks elapsed before it won. This mirrors run.rs's park
    /// block byte-for-byte in policy (build readings this tick -> next_wake -> stop or advance a
    /// tick), with the real 1s sleeps removed so the test is instant and deterministic.
    fn drive_park<F>(max_park: i64, mut readings_at: F) -> (WakeSource, i64)
    where
        F: FnMut(i64) -> Vec<WatchSource>,
    {
        let mut elapsed_s: i64 = 0;
        loop {
            let sources = readings_at(elapsed_s);
            if let Some(reason) = next_wake(&sources, elapsed_s, max_park) {
                return (reason.source, elapsed_s);
            }
            elapsed_s += 1;
            // Guard against a runaway test if the floor logic ever regressed.
            assert!(elapsed_s <= max_park + 5, "park never floored — next_wake floor regressed");
        }
    }

    #[test]
    fn synthetic_sqlite_row_event_wakes_the_loop_before_max_park() {
        // A settled-row landed at tick 3 of a 300s park (a new SqliteRow observation). The live loop
        // must wake on it EARLY — well before the max-park floor — not idle out the full ceiling.
        let (source, ticks) = drive_park(300, |t| {
            vec![
                WatchSource::new(WakeSource::Kill, false),
                WatchSource::new(WakeSource::FreshData, false),
                // the settled-row watcher fires from tick 3 onward
                WatchSource::new(WakeSource::SqliteRow, t >= 3),
            ]
        });
        assert_eq!(source, WakeSource::SqliteRow, "a settled row must end the park");
        assert_eq!(ticks, 3, "woke exactly when the row landed, not at the floor");
        assert!(ticks < 300, "early wake — did not wait out max-park");
    }

    #[test]
    fn synthetic_provider_recovery_event_wakes_the_loop_before_max_park() {
        // A parked provider un-parks at tick 12 of a 300s park. The bus wakes the loop on the
        // recovery instead of burning the whole floor.
        let (source, ticks) = drive_park(300, |t| {
            vec![
                WatchSource::new(WakeSource::Kill, false),
                WatchSource::new(WakeSource::ProviderRecovery, t >= 12),
            ]
        });
        assert_eq!(source, WakeSource::ProviderRecovery);
        assert_eq!(ticks, 12);
        assert!(ticks < 300);
    }

    #[test]
    fn kill_preempts_a_pending_event_at_priority_zero_mid_park() {
        // At tick 5 BOTH a settled-row event AND an operator KILL are present in the same tick.
        // KILL is priority 0 and MUST win — the loop responds to the stop, never to the lower-
        // priority data event. (Fail-closed: a set KILL can never be outranked or delayed.)
        let (source, ticks) = drive_park(300, |t| {
            vec![
                WatchSource::new(WakeSource::SqliteRow, t >= 5),
                WatchSource::new(WakeSource::ProviderRecovery, t >= 5),
                WatchSource::new(WakeSource::Kill, t >= 5),
            ]
        });
        assert_eq!(source, WakeSource::Kill, "KILL must preempt the data events at priority 0");
        assert_eq!(ticks, 5, "preempted on the first tick the KILL was present");
    }

    #[test]
    fn kill_present_from_the_start_wins_on_the_first_tick() {
        // A KILL already latched when the park begins ends the wait on tick 0, before any sleep —
        // the strongest fail-closed guarantee (an operator stop is never slept through).
        let (source, ticks) = drive_park(300, |_t| {
            vec![
                WatchSource::new(WakeSource::Kill, true),
                WatchSource::new(WakeSource::FreshData, true),
                WatchSource::new(WakeSource::SqliteRow, true),
            ]
        });
        assert_eq!(source, WakeSource::Kill);
        assert_eq!(ticks, 0, "KILL ends the park immediately, no ticks slept");
    }

    #[test]
    fn no_events_floors_at_exactly_max_park() {
        // With nothing firing, the drive model floors at exactly the ceiling — the byte-identical
        // sleep count run.rs preserves (max_park sleeps, then FloorElapsed).
        let (source, ticks) = drive_park(8, |_t| {
            vec![
                WatchSource::new(WakeSource::Kill, false),
                WatchSource::new(WakeSource::FreshData, false),
            ]
        });
        assert_eq!(source, WakeSource::FloorElapsed);
        assert_eq!(ticks, 8, "floored at exactly max_park ticks");
    }
}
