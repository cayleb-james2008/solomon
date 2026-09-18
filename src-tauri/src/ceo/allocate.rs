//! Deterministic fleet ROI ranking (pure — unit-tested). Given the outcomes-ledger snapshot and
//! the ops_status rollup, rank the lanes the growth planner should pour leverage into FIRST.
//!
//! The scaling contract (KEYSTONE-safe): scaling is OPT-IN and never touches real money. A lane is
//! only a scaling candidate when it is NOT a real-money lane (no equity_usd in its outcomes) AND
//! its ops rollup is fully green + healthy. Real-money lanes always score 0.0 and rank last — we
//! never "scale" capital velocity from here; asmodeus's money-out stays human-gated elsewhere.
//!
//! NO IO in the scored functions. `rank_lanes` reads only the two Values it is handed and reuses
//! `crate::ceo::velocity_context` for the trend label. north_star is not carried in the snapshot,
//! so trend degrades to "unknown"/"growing" (never a fabricated target) — safe by construction.

use crate::ceo::velocity_context;
use serde_json::Value;

/// True iff this lane's outcomes carry an `equity_usd` field — the marker that asmodeus's finance
/// collector ran (the ONLY collector that emits it). Real-money lanes are never scaled from here.
// ponytail: revisit if a 2nd finance lane is added (still safe: scaling is opt-in, so a new
// finance lane just correctly reads as never-scaled until it too grows an equity_usd field).
pub fn is_real_money(outcomes: &Value) -> bool {
    outcomes.get("equity_usd").is_some()
}

/// A lane is scalable iff it is NOT real-money AND its ops rollup is fully green + healthy. A
/// yellow/red lane must be fixed before it is scaled (grow a broken engine and you grow the break).
pub fn is_scalable(real_money: bool, rollup: &Value) -> bool {
    !real_money
        && rollup.get("healthy").and_then(Value::as_bool) == Some(true)
        && rollup.get("status").and_then(Value::as_str) == Some("green")
}

/// The leverage score: how much a marginal push on this lane moves the fleet forward. 0.0 for any
/// non-scalable lane. Otherwise priority_weight * gap_weight * ship_weight:
///   - priority_weight = 1/max(priority,1) — a priority-1 lane outweighs a priority-5 lane.
///   - gap_weight from the velocity trend: room-to-grow ("behind") beats already-growing beats
///     already-healthy; a stalled/unknown engine scores 0 (fix it, don't scale it).
///   - ship_weight = shipped/iterations clamped to [0.1, 1.0]; a lane that iterates but never
///     ships (0 shipped) still keeps a floor of 0.1, while iterations_24h==0 forces 0 (nothing
///     to scale — the lane never fired).
pub fn leverage_score(
    priority: i64,
    velocity: &Value,
    shipped_24h: i64,
    iterations_24h: i64,
    scalable: bool,
) -> f64 {
    if !scalable {
        return 0.0;
    }
    let priority_weight = 1.0 / (priority.max(1) as f64);
    let gap_weight = match velocity.get("trend").and_then(Value::as_str) {
        Some("behind") => 1.0,
        Some("growing") => 0.5,
        Some("healthy") => 0.25,
        _ => 0.0, // stalled / unknown / missing — fix the engine before scaling it
    };
    let ship_weight = if iterations_24h == 0 {
        0.0
    } else {
        (shipped_24h as f64 / iterations_24h as f64).clamp(0.1, 1.0)
    };
    priority_weight * gap_weight * ship_weight
}

/// Read an i64 outcomes/rollup field, defaulting to 0 when absent or non-integer.
fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

/// Rank every lane present in BOTH the ledger snapshot and the ops_status rollup by leverage,
/// DESC (stable — ties keep snapshot order). Returns (name, score, rationale). north_star is not
/// carried in the snapshot, so velocity_context is fed "" — trend then degrades to
/// "growing"/"unknown" (never a fabricated numeric target).
pub fn rank_lanes(snapshot: &Value, status: &Value) -> Vec<(String, f64, String)> {
    let projects = match snapshot.get("projects").and_then(Value::as_object) {
        Some(p) => p,
        None => return Vec::new(),
    };
    let rollups = status.get("projects").and_then(Value::as_object);

    let mut ranked: Vec<(String, f64, String)> = Vec::new();
    for (name, outcomes) in projects {
        // only lanes observable in BOTH planes — a lane missing an ops rollup can't be judged healthy
        let rollup = match rollups.and_then(|r| r.get(name)) {
            Some(r) => r,
            None => continue,
        };

        let priority = i64_field(outcomes, "priority");
        let shipped = i64_field(outcomes, "shipped_24h");
        let iterations = i64_field(outcomes, "iterations_24h");
        let real_money = is_real_money(outcomes);
        let scalable = is_scalable(real_money, rollup);
        // north_star is not in the snapshot; "" degrades trend safely (never fabricates a target).
        let velocity = velocity_context(outcomes, "");
        let score = leverage_score(priority, &velocity, shipped, iterations, scalable);

        let rationale = if real_money {
            "real-money: never scaled -> 0.00".to_string()
        } else if !scalable {
            let why = rollup.get("status").and_then(Value::as_str).unwrap_or("?");
            format!("not scalable ({why}, needs green+healthy) -> 0.00")
        } else {
            let trend = velocity
                .get("trend")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            format!(
                "{trend} {shipped}/{iterations}, prio {priority}, ships {:.2} -> {score:.2}",
                if iterations == 0 {
                    0.0
                } else {
                    (shipped as f64 / iterations as f64).clamp(0.1, 1.0)
                }
            )
        };
        ranked.push((name.clone(), score, rationale));
    }

    // DESC by score, stable so equal scores keep snapshot iteration order.
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    ranked
}

/// The single deep-work FOCUS lane: the highest-leverage lane worth concentrating sustained work on.
/// Returns the first lane in the (already leverage-DESC) ranking with a POSITIVE score — by
/// construction that lane is non-real-money, green + healthy, and behind/growing (`leverage_score`
/// zeroes every other lane). None when no lane is a scaling candidate (hold the fleet; a red engine is
/// fixed by the ops-RED graft, never "focused" into deep growth work). Pure — unit-tested.
pub fn pick_focus(ranking: &[(String, f64, String)]) -> Option<String> {
    ranking
        .iter()
        .find(|(_, score, _)| *score > 0.0)
        .map(|(name, _, _)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vel(trend: &str) -> Value {
        json!({ "trend": trend })
    }

    #[test]
    fn pick_focus_takes_top_positive_or_none() {
        // top positive leverage wins; a leading zero-score lane is skipped.
        let ranking = vec![
            ("sover".to_string(), 0.30, "behind".to_string()),
            ("dotz".to_string(), 0.10, "growing".to_string()),
        ];
        assert_eq!(pick_focus(&ranking).as_deref(), Some("sover"));
        // all zero (nothing scalable / real-money only) -> None (hold the fleet).
        let zeros = vec![
            ("asmodeus".to_string(), 0.0, "real-money".to_string()),
            ("maki".to_string(), 0.0, "not scalable".to_string()),
        ];
        assert_eq!(pick_focus(&zeros), None);
        // a zero-score lane ahead of a positive one (shouldn't happen after DESC sort, but be safe).
        let mixed = vec![
            ("a".to_string(), 0.0, "x".to_string()),
            ("b".to_string(), 0.20, "behind".to_string()),
        ];
        assert_eq!(pick_focus(&mixed).as_deref(), Some("b"));
        assert_eq!(pick_focus(&[]), None);
    }

    // -------- leverage_score: trend ordering behind > growing > healthy > stalled(=0) --------
    #[test]
    fn leverage_score_trend_ordering() {
        let behind = leverage_score(1, &vel("behind"), 1, 1, true);
        let growing = leverage_score(1, &vel("growing"), 1, 1, true);
        let healthy = leverage_score(1, &vel("healthy"), 1, 1, true);
        let stalled = leverage_score(1, &vel("stalled"), 1, 1, true);
        let unknown = leverage_score(1, &vel("unknown"), 1, 1, true);
        assert!(behind > growing && growing > healthy && healthy > stalled);
        assert_eq!(stalled, 0.0);
        assert_eq!(unknown, 0.0); // stalled/unknown both zero the gap weight
    }

    // -------- leverage_score: priority separates equal-trend lanes --------
    #[test]
    fn leverage_score_priority_1_beats_priority_5() {
        let p1 = leverage_score(1, &vel("behind"), 1, 1, true);
        let p5 = leverage_score(5, &vel("behind"), 1, 1, true);
        assert!(p1 > p5);
    }

    // -------- leverage_score: non-scalable and never-fired both zero --------
    #[test]
    fn leverage_score_not_scalable_is_zero() {
        assert_eq!(leverage_score(1, &vel("behind"), 1, 1, false), 0.0);
    }

    #[test]
    fn leverage_score_zero_iterations_zeros_ship_weight() {
        // iterations_24h == 0 -> ship_weight 0 -> whole score 0 even when scalable + behind
        assert_eq!(leverage_score(1, &vel("behind"), 0, 0, true), 0.0);
    }

    #[test]
    fn leverage_score_ship_weight_floor_and_value() {
        // 0 ships over 10 iters -> floored at 0.1; prio 1, behind -> 1.0 * 1.0 * 0.1
        assert_eq!(leverage_score(1, &vel("behind"), 0, 10, true), 0.1);
        // 6 ships / 10 iters -> 0.6; prio 2, behind -> 0.5 * 1.0 * 0.6 = 0.30
        assert!((leverage_score(2, &vel("behind"), 6, 10, true) - 0.30).abs() < 1e-9);
    }

    // -------- is_scalable --------
    #[test]
    fn is_scalable_rules() {
        let green = json!({"healthy": true, "status": "green"});
        let yellow = json!({"healthy": false, "status": "yellow"});
        let red = json!({"healthy": false, "status": "red"});
        // real money is never scalable regardless of a green rollup
        assert!(!is_scalable(true, &green));
        // scalable only on green + healthy
        assert!(is_scalable(false, &green));
        assert!(!is_scalable(false, &yellow));
        assert!(!is_scalable(false, &red));
        // green status but not healthy -> not scalable
        assert!(!is_scalable(
            false,
            &json!({"healthy": false, "status": "green"})
        ));
    }

    #[test]
    fn is_real_money_reads_equity_marker() {
        assert!(is_real_money(&json!({"equity_usd": 168.97})));
        assert!(is_real_money(&json!({"equity_usd": null}))); // field present == finance collector ran
        assert!(!is_real_money(&json!({"posts_24h": 3})));
    }

    // -------- rank_lanes: deterministic DESC, asmodeus (equity_usd) scores 0 and ranks last -----
    #[test]
    fn rank_lanes_deterministic_desc_real_money_last() {
        // sover: scalable, growing (posts, no target) 42/81 -> a real positive score
        // dotz:  scalable, growing 40/67, priority 4 -> a smaller positive score
        // asmodeus: real-money (equity_usd) -> 0.0, must rank LAST despite priority 1
        // daedulus: green+healthy but iterations 0 -> 0.0 (never fired)
        let snapshot = json!({
            "projects": {
                "asmodeus": {"priority": 1, "iterations_24h": 38, "shipped_24h": 18, "equity_usd": 168.97, "live_trades_24h": 2, "fills_24h": 4},
                "sover":    {"priority": 2, "iterations_24h": 81, "shipped_24h": 42, "posts_24h": 2},
                "daedulus": {"priority": 3, "iterations_24h": 0,  "shipped_24h": 0},
                "dotz":     {"priority": 4, "iterations_24h": 67, "shipped_24h": 40}
            }
        });
        let status = json!({
            "projects": {
                "asmodeus": {"status": "green", "healthy": true},
                "sover":    {"status": "green", "healthy": true},
                "daedulus": {"status": "green", "healthy": true},
                "dotz":     {"status": "green", "healthy": true}
            }
        });
        let ranked = rank_lanes(&snapshot, &status);
        let names: Vec<&str> = ranked.iter().map(|(n, _, _)| n.as_str()).collect();

        // sover (prio 2, growing) outranks dotz (prio 4, growing); both outrank the two zeros.
        assert_eq!(names[0], "sover");
        assert_eq!(names[1], "dotz");
        // the two zero-scored lanes rank last; asmodeus is one of them (real-money) and scores 0.0
        let asm = ranked.iter().find(|(n, _, _)| n == "asmodeus").unwrap();
        assert_eq!(asm.1, 0.0);
        assert!(asm.2.contains("real-money"));
        assert!(ranked[2].1 == 0.0 && ranked[3].1 == 0.0);

        // fully deterministic: a second call yields the identical order + scores
        let again = rank_lanes(&snapshot, &status);
        assert_eq!(ranked, again);
    }

    #[test]
    fn rank_lanes_requires_both_planes() {
        // a lane in the snapshot but absent from ops_status is unjudgeable -> dropped
        let snapshot = json!({"projects": {
            "sover": {"priority": 2, "iterations_24h": 10, "shipped_24h": 5},
            "ghost": {"priority": 9, "iterations_24h": 10, "shipped_24h": 5}
        }});
        let status = json!({"projects": {
            "sover": {"status": "green", "healthy": true}
        }});
        let ranked = rank_lanes(&snapshot, &status);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0, "sover");
    }
}
