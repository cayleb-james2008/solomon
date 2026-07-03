//! Per-lane interval SCALE dials — deterministic, bounded, REVERSIBLE tightening of a lane's
//! iteration cadence when it is measurably BEHIND its north-star target and green enough to push.
//!
//! `deploy.rs` rebuilds a stale app; this is the cheaper lever: when a healthy lane is behind its
//! stated daily target, shorten its iteration `interval` one bounded step so it does more work per
//! day — and, symmetrically, walk that back toward the operator's baseline the moment the lane goes
//! unhealthy or real-money-risky. It rides the every-sweep CEO tick (`ceo::tick` calls
//! `maybe_scale_lanes`); the integrator owns that wiring.
//!
//! HARD SAFETY CONTRACT (mirrors deploy.rs's opt-in + cooldown + per-sweep cap):
//!   - A lane is scalable ONLY IFF its repos.json entry carries a `scale` object
//!     `{min_interval_s, step_s, cooldown_s}` (opt-in per lane, exactly like `live_deploy`). A lane
//!     WITHOUT `scale` is NEVER scaled.
//!   - We only ever TIGHTEN (shorten the interval) a lane that is BOTH green/stable AND not
//!     real-money, AND only when its velocity trend is "behind", AND only after the cooldown.
//!   - REVERSIBILITY is unconditional: the instant a lane is not scalable (RED / unstable /
//!     real-money) we walk its interval BACK toward the operator's baseline one `step_s` per sweep
//!     until it reaches baseline — never leaving a tightened cadence stuck on an unhealthy lane. The
//!     back-off is NOT cooldown-gated (safety must not wait) and is capped at the baseline (never
//!     loosens past what the operator configured).
//!   - `min_interval_s` floors how tight we go; the operator's ORIGINAL `interval` — captured into
//!     `runtime/<name>/_scale_baseline` on the first tighten (because tightening overwrites the live
//!     `interval` field in place) — is the BASELINE anchor the back-off restores to.
//!   - At most ONE interval write per sweep across ALL lanes (`MAX_SCALE_WRITES_PER_SWEEP`), so a
//!     sweep can never re-cadence the whole fleet at once. The cooldown + this cap bound the rate.
//!
//! The decision `next_interval` is PURE (no IO) so the whole safety table is unit-tested. The
//! orchestrator `maybe_scale_lanes` collects velocity + scalability + cooldown (IO) and applies the
//! one write, stamping the cooldown marker and paging the operator LOUDLY on every change.

#![allow(dead_code)]

use crate::control::{paths, registry};
use serde_json::Value;
use std::path::PathBuf;

/// Max interval-scale writes per sweep across ALL lanes. One re-cadence per sweep + the per-lane
/// cooldown together bound the rate — a sweep can never re-tune the whole fleet at once.
const MAX_SCALE_WRITES_PER_SWEEP: usize = 1;

/// Default min seconds between scale writes for a lane when `scale.cooldown_s` is absent/invalid.
const DEFAULT_SCALE_COOLDOWN_S: i64 = 3600;

// --------------------------------------------------------------------------- //
// DECISION — pure, the load-bearing safety predicate
// --------------------------------------------------------------------------- //

/// The next interval for a lane, or None to leave it untouched. PURE — the caller resolves
/// scalability, velocity, and cooldown (IO) and passes them in.
///
/// - `scale_cfg` None (no `scale` block) -> None. NEVER scaled (opt-in only).
/// - `!scalable` (RED / unstable / real-money) -> REVERSIBILITY: if we previously tightened
///   (`current_interval < baseline_interval`) step BACK toward the baseline by `step_s`, capped at
///   the baseline; else None. The back-off is not cooldown-gated — safety must not wait.
/// - `scalable` && trend=="behind" && `cooldown_elapsed` -> TIGHTEN by `step_s`, floored at
///   `min_interval_s`.
/// - otherwise -> None (healthy/not-behind, or within cooldown -> no change).
///
/// `baseline_interval` is the operator's configured `interval` (the anchor the back-off restores);
/// `current_interval` is the live interval right now (equal to baseline until we tighten it).
pub fn next_interval(
    scale_cfg: Option<&Value>,
    baseline_interval: i64,
    current_interval: i64,
    velocity: &Value,
    scalable: bool,
    cooldown_elapsed: bool,
) -> Option<i64> {
    let cfg = scale_cfg?; // no scale block -> never scaled

    if !scalable {
        // REVERSIBILITY: if we previously tightened below baseline, step back toward baseline.
        // Not cooldown-gated — an unhealthy / real-money lane must be de-tightened immediately.
        if current_interval < baseline_interval {
            let step = step_s(cfg);
            return Some((current_interval + step).min(baseline_interval));
        }
        return None; // already at/above baseline -> nothing to walk back
    }

    // Scalable: tighten only when measurably BEHIND the target, and only after the cooldown.
    let behind = velocity.get("trend").and_then(Value::as_str) == Some("behind");
    if behind && cooldown_elapsed {
        let step = step_s(cfg);
        let floor = min_interval_s(cfg);
        let next = (current_interval - step).max(floor);
        return Some(next);
    }
    None
}

/// `scale.step_s` — the bounded per-sweep interval change (seconds). Default 0-safe: a missing/
/// invalid step yields 0 (a no-op change, never negative), so a malformed config can't widen wildly.
fn step_s(scale_cfg: &Value) -> i64 {
    scale_cfg
        .get("step_s")
        .and_then(Value::as_i64)
        .filter(|s| *s > 0)
        .unwrap_or(0)
}

/// `scale.min_interval_s` — the tightening floor (seconds). Default 1 (never a zero/negative
/// interval, which would busy-loop the lane).
fn min_interval_s(scale_cfg: &Value) -> i64 {
    scale_cfg
        .get("min_interval_s")
        .and_then(Value::as_i64)
        .filter(|m| *m > 0)
        .unwrap_or(1)
}

/// `scale.cooldown_s` — min seconds between scale writes, default DEFAULT_SCALE_COOLDOWN_S.
pub fn scale_cooldown_s(scale_cfg: &Value) -> i64 {
    scale_cfg
        .get("cooldown_s")
        .and_then(Value::as_i64)
        .filter(|c| *c > 0)
        .unwrap_or(DEFAULT_SCALE_COOLDOWN_S)
}

/// True iff `repo_cfg` carries a truthy `scale` OBJECT — the opt-in for interval scaling. Returns
/// the block itself for the caller (None => never scaled, exactly like `has_live_deploy`).
pub fn scale_cfg(repo_cfg: &Value) -> Option<&Value> {
    repo_cfg
        .get("scale")
        .filter(|s| s.as_object().map(|o| !o.is_empty()).unwrap_or(false))
}

// --------------------------------------------------------------------------- //
// COOLDOWN MARKER — mirrors deploy.rs's _last_deploy idiom
// --------------------------------------------------------------------------- //

/// The per-lane last-scale marker path (`runtime/<name>/_last_scale`).
fn last_scale_path(name: &str) -> PathBuf {
    paths::here().join("runtime").join(name).join("_last_scale")
}

/// Age (seconds) since the last scale write for `name`, or None when there was none / the marker is
/// unreadable/unparseable (treated as "no prior scale" -> cooldown elapsed).
fn last_scale_age_s(name: &str) -> Option<i64> {
    let raw = std::fs::read_to_string(last_scale_path(name)).ok()?;
    let last = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%SZ")
        .ok()?
        .and_utc();
    Some((chrono::Utc::now() - last).num_seconds())
}

/// Stamp the per-lane last-scale marker (cooldown sentinel). Best-effort (OSError -> pass).
fn stamp_last_scale(name: &str) {
    let _ = (|| -> std::io::Result<()> {
        let p = last_scale_path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(&p, ts)
    })();
}

/// True iff the lane's scale cooldown has elapsed (no prior scale, or age > `scale.cooldown_s`).
fn cooldown_elapsed(name: &str, cfg: &Value) -> bool {
    match last_scale_age_s(name) {
        Some(age) => age > scale_cooldown_s(cfg),
        None => true, // no prior scale -> eligible
    }
}

// --------------------------------------------------------------------------- //
// BASELINE ANCHOR — the operator's ORIGINAL interval, persisted so back-off can restore it
// --------------------------------------------------------------------------- //
//
// Tightening writes the shrunk value into repos.json's ONE `interval` field in place, so reading
// repos.json alone loses the operator's original value after a single tighten (baseline would equal
// the already-shrunk current, and the reversibility branch could never fire). We therefore stamp the
// operator interval into `runtime/<name>/_scale_baseline` on the FIRST tighten and read it back as
// the baseline anchor thereafter; the marker is cleared once the lane is walked fully back to it.

/// The per-lane baseline-anchor marker path (`runtime/<name>/_scale_baseline`).
fn scale_baseline_path(name: &str) -> PathBuf {
    paths::here()
        .join("runtime")
        .join(name)
        .join("_scale_baseline")
}

/// Read the persisted operator-interval anchor for `name`, or None when the lane was never tightened
/// (or the marker is unreadable/non-positive -> treat as "no anchor" so `current` becomes baseline).
fn read_scale_baseline(name: &str) -> Option<i64> {
    std::fs::read_to_string(scale_baseline_path(name))
        .ok()?
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
}

/// Stamp the operator-interval anchor (once, on the first tighten). Best-effort (OSError -> pass).
fn stamp_scale_baseline(name: &str, interval: i64) {
    let _ = (|| -> std::io::Result<()> {
        let p = scale_baseline_path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, interval.to_string())
    })();
}

/// Drop the anchor once the lane is fully restored to baseline (no longer tightened). Best-effort.
fn clear_scale_baseline(name: &str) {
    let _ = std::fs::remove_file(scale_baseline_path(name));
}

/// What to do with the baseline-anchor marker after a scale write (PURE — unit-tested):
/// - first tighten (tightening AND no marker yet) -> STAMP the operator anchor before the in-place
///   write erases it;
/// - full back-off (loosening AND the new interval reached/passed baseline) -> CLEAR (restored);
/// - otherwise -> LEAVE the marker as-is.
#[derive(Debug, PartialEq, Eq)]
enum BaselineAction {
    Stamp(i64),
    Clear,
    Leave,
}

fn baseline_action(
    tightening: bool,
    marker_exists: bool,
    new: i64,
    baseline: i64,
    anchor: i64,
) -> BaselineAction {
    if tightening && !marker_exists {
        BaselineAction::Stamp(anchor)
    } else if !tightening && new >= baseline {
        BaselineAction::Clear
    } else {
        BaselineAction::Leave
    }
}

// --------------------------------------------------------------------------- //
// ORCHESTRATOR — the every-sweep tick entry
// --------------------------------------------------------------------------- //

/// The per-lane interval-scale check, wired into `ceo::tick`. Iterates the explicit repos.json
/// entries; for each lane with a `scale` block, computes its velocity (from the fresh outcomes
/// snapshot), whether it is scalable (green AND not real-money), and whether its cooldown elapsed;
/// asks `next_interval`; and if that yields a NEW interval, writes it via `set_repo_config`, stamps
/// the cooldown marker, and pages the operator. At most ONE interval write per sweep across all
/// lanes (`MAX_SCALE_WRITES_PER_SWEEP`).
///
/// `snapshot` is the outcomes rollup (`{projects:{<name>:{...24h outcome fields...}}}`); `status` is
/// the ops rollup (`{projects:{<name>:{status,healthy,process_green,outcomes_green,...}}}`). A lane's
/// north star is its repos.json `goal`; scalability = the ops rollup is `healthy` AND the lane is
/// not `live_money`.
///
/// catch_unwind is the caller's responsibility (the tick wraps this) so a scale failure can never
/// abort the sweep. A back-off write is un-cooldown-gated but still consumes the per-sweep budget.
pub fn maybe_scale_lanes(snapshot: &Value, status: &Value) {
    let mut writes_left = MAX_SCALE_WRITES_PER_SWEEP;

    for repo in registry::read_repos_json() {
        if writes_left == 0 {
            break; // per-sweep cap reached — the rest defer to a later sweep
        }
        let cfg = match scale_cfg(&repo) {
            Some(c) => c.clone(), // opt-in only — a lane without `scale` is NEVER scaled
            None => continue,
        };
        let name = paths::repo_name(&repo);
        if name.is_empty() {
            continue;
        }

        // CURRENT = the live interval right now (repos.json holds the one `interval` field, which we
        // write in place when we tighten). BASELINE = the operator's ORIGINAL interval, persisted in
        // `_scale_baseline` on the first tighten so back-off can restore it even after in-place writes
        // overwrite the operator value. No marker yet => never tightened => current IS the baseline.
        let current = registry::project_interval(&repo);
        let marker = read_scale_baseline(&name);
        let baseline = marker.unwrap_or(current);

        // Velocity from the fresh outcomes snapshot + the lane's north star.
        let outcomes = snapshot
            .get("projects")
            .and_then(|p| p.get(&name))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let north_star = repo.get("goal").and_then(Value::as_str).unwrap_or("");
        let velocity = crate::ceo::velocity_context(&outcomes, north_star);

        // Scalable iff the ops rollup is HEALTHY (green process AND outcomes) AND the lane is not
        // real-money. Guard on BOTH signals: the repos.json `live_money` flag AND `is_real_money`
        // (equity_usd present in outcomes — the SAME signal the ranking/dry-run uses), so a finance
        // lane is excluded from tightening even if an operator ever added a `scale` block to it. A
        // missing rollup -> not healthy -> not scalable (back-off only, if it was tightened).
        let healthy = status
            .get("projects")
            .and_then(|p| p.get(&name))
            .and_then(|p| p.get("healthy"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let scalable = healthy
            && !crate::redeploy::is_live_money(&repo)
            && !crate::ceo::allocate::is_real_money(&outcomes);

        let cd_elapsed = cooldown_elapsed(&name, &cfg);

        let new = match next_interval(Some(&cfg), baseline, current, &velocity, scalable, cd_elapsed)
        {
            Some(n) => n,
            None => continue,
        };
        if new == current {
            continue; // no real change (already at floor / already at baseline)
        }

        // Decide the anchor bookkeeping from the SAME facts, before the write. `baseline` is the
        // operator anchor to persist on the first tighten (== current then, since no marker yet).
        let tightening = new < current;
        let action = baseline_action(tightening, marker.is_some(), new, baseline, baseline);

        let result = registry::set_repo_config(
            &name, None, None, None, None, None, Some(new), None, None, None, None, None,
        );
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            continue; // write refused (corrupt repos.json etc.) — don't stamp/page a non-write
        }
        match action {
            BaselineAction::Stamp(anchor) => stamp_scale_baseline(&name, anchor),
            BaselineAction::Clear => clear_scale_baseline(&name),
            BaselineAction::Leave => {}
        }
        stamp_last_scale(&name);
        writes_left -= 1;
        page(&name, current, new, scalable);
    }
}

/// Page the operator about an interval-scale change. A TIGHTEN (scalable, interval shrank) is a
/// growth report; a BACK-OFF (unscalable, interval grew toward baseline) is a recovered notice.
/// Best-effort (notify::send never fails a sweep).
fn page(name: &str, from: i64, to: i64, scalable: bool) {
    let notice = if scalable && to < from {
        crate::notify::Notice::report(
            format!("Solomon: {name} cadence tightened"),
            format!("{name}: interval {from}s -> {to}s (behind target — doing more per day)"),
        )
    } else {
        crate::notify::Notice::recovered(
            format!("Solomon: {name} cadence backed off"),
            format!("{name}: interval {from}s -> {to}s (unhealthy/real-money — restoring baseline)"),
        )
    };
    let _ = crate::notify::send(&notice);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg() -> Value {
        json!({"min_interval_s": 300, "step_s": 60, "cooldown_s": 3600})
    }
    fn behind() -> Value {
        json!({"trend": "behind"})
    }
    fn healthy() -> Value {
        json!({"trend": "healthy"})
    }

    // -------- next_interval: the full safety table --------

    #[test]
    fn tightens_when_behind_scalable_and_cooldown() {
        // behind + scalable + cooldown elapsed -> current - step, floored at min.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &behind(), true, true),
            Some(840) // 900 - 60
        );
    }

    #[test]
    fn tighten_floors_at_min_interval() {
        // already at the floor -> stays at the floor (never below min_interval_s).
        assert_eq!(
            next_interval(Some(&cfg()), 900, 300, &behind(), true, true),
            Some(300)
        );
        // one step above the floor but step overshoots -> clamped to the floor.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 340, &behind(), true, true),
            Some(300) // 340 - 60 = 280, floored to 300
        );
    }

    #[test]
    fn within_cooldown_no_change() {
        // behind + scalable but cooldown NOT elapsed -> None.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &behind(), true, false),
            None
        );
    }

    #[test]
    fn healthy_not_behind_no_change() {
        // scalable + cooldown elapsed but trend healthy (not behind) -> None.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &healthy(), true, true),
            None
        );
        // unknown/other trends likewise never tighten.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &json!({"trend": "growing"}), true, true),
            None
        );
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &json!({}), true, true),
            None
        );
    }

    #[test]
    fn no_scale_cfg_never_scales_even_when_behind() {
        // No scale block -> None even with trend forced "behind" and everything else screaming go.
        assert_eq!(
            next_interval(None, 900, 900, &behind(), true, true),
            None
        );
    }

    #[test]
    fn reversibility_unscalable_tightened_steps_toward_baseline() {
        // !scalable AND we previously tightened (current 780 < baseline 900) -> step back by step_s,
        // NOT cooldown-gated (cooldown_elapsed=false still walks back).
        assert_eq!(
            next_interval(Some(&cfg()), 900, 780, &behind(), false, false),
            Some(840) // 780 + 60
        );
        // the last step is capped AT the baseline (never loosens past the operator's interval).
        assert_eq!(
            next_interval(Some(&cfg()), 900, 870, &behind(), false, false),
            Some(900) // 870 + 60 = 930, capped to 900
        );
    }

    #[test]
    fn reversibility_unscalable_at_or_above_baseline_no_change() {
        // !scalable AND current already == baseline -> None (nothing to walk back).
        assert_eq!(
            next_interval(Some(&cfg()), 900, 900, &behind(), false, false),
            None
        );
        // !scalable AND current already > baseline (shouldn't happen, but defensive) -> None.
        assert_eq!(
            next_interval(Some(&cfg()), 900, 960, &behind(), false, false),
            None
        );
    }

    // -------- getters + cooldown helpers --------

    #[test]
    fn getters_defaults_and_values() {
        let c = cfg();
        assert_eq!(step_s(&c), 60);
        assert_eq!(min_interval_s(&c), 300);
        assert_eq!(scale_cooldown_s(&c), 3600);
        // defaults when absent/invalid
        assert_eq!(step_s(&json!({})), 0);
        assert_eq!(step_s(&json!({"step_s": 0})), 0);
        assert_eq!(step_s(&json!({"step_s": -5})), 0);
        assert_eq!(min_interval_s(&json!({})), 1);
        assert_eq!(min_interval_s(&json!({"min_interval_s": 0})), 1);
        assert_eq!(scale_cooldown_s(&json!({})), DEFAULT_SCALE_COOLDOWN_S);
        assert_eq!(scale_cooldown_s(&json!({"cooldown_s": -1})), DEFAULT_SCALE_COOLDOWN_S);
        assert_eq!(scale_cooldown_s(&json!({"cooldown_s": 600})), 600);
    }

    #[test]
    fn scale_cfg_is_opt_in_object_only() {
        // present, non-empty object -> Some
        assert!(scale_cfg(&json!({"scale": {"step_s": 60}})).is_some());
        // absent -> None
        assert!(scale_cfg(&json!({"name": "x"})).is_none());
        // empty object -> None (nothing configured)
        assert!(scale_cfg(&json!({"scale": {}})).is_none());
        // wrong type -> None
        assert!(scale_cfg(&json!({"scale": true})).is_none());
        assert!(scale_cfg(&json!({"scale": "yes"})).is_none());
    }

    #[test]
    fn per_sweep_cap_is_one() {
        // Documents the fleet-wide guard: at most one re-cadence per sweep.
        assert_eq!(MAX_SCALE_WRITES_PER_SWEEP, 1);
    }

    #[test]
    fn baseline_action_stamps_first_tighten_and_clears_on_full_backoff() {
        use BaselineAction::*;
        // First tighten, no marker yet -> STAMP the operator anchor (before the in-place write).
        assert_eq!(baseline_action(true, false, 840, 900, 900), Stamp(900));
        // Subsequent tighten, marker already exists -> LEAVE (anchor already captured).
        assert_eq!(baseline_action(true, true, 780, 900, 900), Leave);
        // Back-off that reached the baseline -> CLEAR (lane restored; no longer tightened).
        assert_eq!(baseline_action(false, true, 900, 900, 900), Clear);
        // Back-off still below baseline -> LEAVE (keep walking back next sweep).
        assert_eq!(baseline_action(false, true, 840, 900, 900), Leave);
        // Defensive: back-off past baseline -> CLEAR.
        assert_eq!(baseline_action(false, true, 930, 900, 900), Clear);
    }
}
