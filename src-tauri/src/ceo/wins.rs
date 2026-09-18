//! D8 — Layer 3: the CROSS-PROJECT wins ledger reader.
//!
//! ============================ WHAT THIS ADDS =============================
//! The onboarding path (`ceo::onboard`) lets a brand-new project join the fleet. The MINIMAL
//! cross-project learning that pays off from day one is this: when the CEO plans a lane's day, it
//! should be able to see WHAT KINDS OF MOVES have actually WON across the fleet's history — a
//! shipped iteration, a published post, a positive equity day — so a new lane's plan is informed by
//! prior wins instead of starting cold. This module is the READER that surfaces those prior wins
//! into the morning-plan prompt.
//! ========================================================================
//!
//! ## Why this is a READER, not a new ledger (failure catalog #5)
//!
//! The substrate ALREADY exists: `ops::ledger::append_daily` writes one anonymizable daily snapshot
//! per day to `runtime/outcomes.jsonl` (append-only). D8 adds NO new ledger — it adds a bounded,
//! read-only tail + an ANONYMIZED win extractor. The `#[test]`
//! `wins_ledger_is_consulted_in_the_plan_prompt` proves the extractor's output is spliced into the
//! plan prompt's user JSON (a WRITE-ONLY ledger — one written by the evening summary but never read
//! back into planning — would make that test fail, which is exactly the "no write-only ledger" rule
//! the D7 contract enforces for the orchestrator ledgers).
//!
//! ## ANONYMIZED by construction (the moat: honest, not a leaderboard)
//!
//! A surfaced win NEVER names the lane it came from — cross-project learning is about the SHAPE of a
//! win (kind + magnitude), not "sover beat asmodeus". [`WinKind`] is the closed set of win shapes;
//! the summary the planner sees is counts + a representative magnitude per shape, with zero lane
//! identity. A day with no wins yields an EMPTY summary (never a fabricated encouragement) — silence
//! is honest, so the planner is never nudged by a win that did not happen.

use serde_json::{Value, json};

/// The default number of recent daily snapshots to tail from `outcomes.jsonl`. A bounded window (a
/// fortnight) keeps the read cheap (reverse-seek, never the whole file) and the wins recent enough
/// to be relevant to today's plan.
pub const DEFAULT_LOOKBACK_DAYS: usize = 14;

/// The CLOSED set of win SHAPES the reader surfaces. Each is a first-order, evidence-backed outcome
/// the ops ledger already records per project per day — a shipped code iteration, a published post
/// (with a URL — a claim without a URL is NOT a win, matching `ledger::posts_activity`), a positive
/// 24h equity day, or a live trade filled. Anonymized: the shape is fleet-learning signal; the lane
/// identity is deliberately dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinKind {
    /// A lane shipped at least one iteration in the window (`shipped_24h` > 0).
    ShippedIteration,
    /// A lane published at least one post WITH a URL (`posts_24h` > 0 and not all missing-url).
    PublishedPost,
    /// A lane's 24h equity delta was positive (`equity_delta_24h` > 0).
    PositiveEquityDay,
    /// A lane filled at least one live trade (`live_trades_24h` > 0).
    LiveTradeFilled,
}

impl WinKind {
    /// The stable, lane-anonymous identity of this win shape (for the plan prompt + tests).
    pub fn as_str(&self) -> &'static str {
        match self {
            WinKind::ShippedIteration => "shipped_iteration",
            WinKind::PublishedPost => "published_post",
            WinKind::PositiveEquityDay => "positive_equity_day",
            WinKind::LiveTradeFilled => "live_trade_filled",
        }
    }

    /// The full closed set, in the priority order the planner reads them.
    pub fn all() -> [WinKind; 4] {
        [
            WinKind::PositiveEquityDay,
            WinKind::LiveTradeFilled,
            WinKind::PublishedPost,
            WinKind::ShippedIteration,
        ]
    }
}

/// Extract the ANONYMIZED wins from ONE project's daily-outcome object (the value under
/// `projects.<name>` in an `outcomes.jsonl` line). Pure over the object — the unit-tested core, and
/// the SAME extractor the planner-facing summary composes from. Returns each `(WinKind, magnitude)`
/// the object evidences; the lane NAME is never taken (the caller passes only the value, not the
/// key), so anonymization holds by construction.
pub fn project_wins(outcomes: &Value) -> Vec<(WinKind, i64)> {
    let mut wins = Vec::new();
    let i = |k: &str| outcomes.get(k).and_then(Value::as_i64);

    // Positive equity day: equity_delta_24h is stored as a float (rounded to 2dp); a strictly
    // positive delta is a win, its magnitude the whole-dollar gain (floored, never fabricated).
    if let Some(d) = outcomes.get("equity_delta_24h").and_then(Value::as_f64) {
        if d > 0.0 {
            wins.push((WinKind::PositiveEquityDay, d.floor() as i64));
        }
    }
    if let Some(n) = i("live_trades_24h") {
        if n > 0 {
            wins.push((WinKind::LiveTradeFilled, n));
        }
    }
    // A published post is a win ONLY when at least one post has a URL: posts_24h counts claims, and
    // posts_missing_url_24h counts the claims WITHOUT evidence, so the evidenced count is the
    // difference (matching ledger::posts_activity's URL-evidence discipline — a claim without a URL
    // is not a win). A null posts_24h (unobservable registry) is not a win.
    if let Some(posts) = i("posts_24h") {
        let missing = i("posts_missing_url_24h").unwrap_or(0);
        let evidenced = posts - missing;
        if evidenced > 0 {
            wins.push((WinKind::PublishedPost, evidenced));
        }
    }
    if let Some(n) = i("shipped_24h") {
        if n > 0 {
            wins.push((WinKind::ShippedIteration, n));
        }
    }
    wins
}

/// Parse the `projects` map out of ONE `outcomes.jsonl` snapshot line and sum its anonymized wins.
/// Returns a per-`WinKind` `(days, total_magnitude)` accumulation folded into `acc`. A malformed
/// line contributes nothing (never an error that hides a real win — same leniency as the ledger's
/// own readers).
fn fold_snapshot_line(line: &str, acc: &mut std::collections::BTreeMap<&'static str, (i64, i64)>) {
    let snap: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(_) => return,
    };
    let projects = match snap.get("projects").and_then(Value::as_object) {
        Some(p) => p,
        None => return,
    };
    for (_name, outcomes) in projects {
        // _name is DROPPED — anonymization by construction (we only read the value).
        for (kind, mag) in project_wins(outcomes) {
            let e = acc.entry(kind.as_str()).or_insert((0, 0));
            e.0 += 1; // one more (lane, day) that evidenced this win shape
            e.1 += mag;
        }
    }
}

/// The planner-facing ANONYMIZED wins summary, computed from the last `lookback` daily snapshots of
/// `outcomes.jsonl`. Read-only + bounded (tails, never the whole file). The returned Value is the
/// object spliced into the morning-plan prompt's user JSON under `"prior_wins"`:
///
/// ```json
/// { "window_days": 14, "wins": [ {"kind": "shipped_iteration", "days": 9, "total": 21}, ... ] }
/// ```
///
/// with `wins` in [`WinKind::all`] priority order, EMPTY when the window evidenced no wins (an
/// honest empty — the planner is never nudged by a win that did not happen). `lines` is the raw
/// tail (dependency-injected so the `#[test]` drives it without touching the live ledger).
pub fn wins_summary_from_lines(lines: &[String], lookback: usize) -> Value {
    let mut acc: std::collections::BTreeMap<&'static str, (i64, i64)> =
        std::collections::BTreeMap::new();
    for line in lines {
        fold_snapshot_line(line, &mut acc);
    }
    let mut wins = Vec::new();
    for kind in WinKind::all() {
        if let Some((days, total)) = acc.get(kind.as_str()) {
            if *days > 0 {
                wins.push(json!({
                    "kind": kind.as_str(),
                    "days": days,
                    "total": total,
                }));
            }
        }
    }
    json!({ "window_days": lookback, "wins": wins })
}

/// The LIVE reader: tail the last [`DEFAULT_LOOKBACK_DAYS`] snapshots from the real
/// `runtime/outcomes.jsonl` and summarize the anonymized wins. This is the function the morning
/// plan calls to surface prior wins into the plan prompt. A missing/empty ledger yields the honest
/// empty summary (`wins: []`), never a fabricated one.
pub fn prior_wins() -> Value {
    let path = crate::ops::ledger::ledger_path();
    let lines = crate::pecrt::warm::read_last_lines(&path, DEFAULT_LOOKBACK_DAYS);
    wins_summary_from_lines(&lines, DEFAULT_LOOKBACK_DAYS)
}

/// True iff the summary evidenced at least one win (the `wins` array is non-empty). Used by the
/// plan-prompt wiring test to assert the reader is CONSULTED — a write-only ledger (never read back)
/// would leave the summary empty even with wins on disk, failing the wiring assertion.
pub fn has_wins(summary: &Value) -> bool {
    summary
        .get("wins")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn project_wins_extracts_each_shape_with_magnitude() {
        let outcomes = json!({
            "shipped_24h": 3,
            "posts_24h": 4,
            "posts_missing_url_24h": 1, // 3 evidenced
            "equity_delta_24h": 18.97,
            "live_trades_24h": 2,
        });
        let wins = project_wins(&outcomes);
        // all four shapes present; magnitudes are floored/evidenced, never fabricated.
        assert!(wins.contains(&(WinKind::ShippedIteration, 3)));
        assert!(wins.contains(&(WinKind::PublishedPost, 3))); // 4 claimed - 1 missing url
        assert!(wins.contains(&(WinKind::PositiveEquityDay, 18))); // floor(18.97)
        assert!(wins.contains(&(WinKind::LiveTradeFilled, 2)));
    }

    #[test]
    fn a_post_without_a_url_is_not_a_win() {
        // 2 posts CLAIMED, both missing a URL -> zero evidenced -> NOT a win (URL-evidence discipline).
        let outcomes = json!({"posts_24h": 2, "posts_missing_url_24h": 2});
        let wins = project_wins(&outcomes);
        assert!(!wins.iter().any(|(k, _)| *k == WinKind::PublishedPost));
    }

    #[test]
    fn a_flat_or_negative_equity_day_is_not_a_win() {
        assert!(
            !project_wins(&json!({"equity_delta_24h": 0.0}))
                .iter()
                .any(|(k, _)| *k == WinKind::PositiveEquityDay)
        );
        assert!(
            !project_wins(&json!({"equity_delta_24h": -5.0}))
                .iter()
                .any(|(k, _)| *k == WinKind::PositiveEquityDay)
        );
        // a zero-ship, zero-trade day evidences NO win at all.
        assert!(project_wins(&json!({"shipped_24h": 0, "live_trades_24h": 0})).is_empty());
    }

    #[test]
    fn wins_summary_folds_days_and_totals_and_is_anonymized() {
        // two daily snapshots, each with named lanes — the summary must carry NO lane name.
        let day1 = json!({
            "date": "2026-07-01",
            "projects": {
                "sover": {"shipped_24h": 2, "posts_24h": 1, "posts_missing_url_24h": 0},
                "asmodeus": {"shipped_24h": 1, "equity_delta_24h": 10.0}
            }
        });
        let day2 = json!({
            "date": "2026-07-02",
            "projects": {
                "sover": {"shipped_24h": 3},
                "asmodeus": {"equity_delta_24h": 4.0, "live_trades_24h": 1}
            }
        });
        let lines = vec![day1.to_string(), day2.to_string()];
        let summary = wins_summary_from_lines(&lines, 14);
        assert_eq!(summary["window_days"], json!(14));

        let wins = summary["wins"].as_array().unwrap();
        // shipped_iteration: sover(day1)+asmodeus(day1)+sover(day2) = 3 lane-days, total 2+1+3=6.
        let shipped = wins
            .iter()
            .find(|w| w["kind"] == "shipped_iteration")
            .unwrap();
        assert_eq!(shipped["days"], json!(3));
        assert_eq!(shipped["total"], json!(6));
        // positive_equity_day: asmodeus day1 + day2 = 2 lane-days, total 10+4=14.
        let eq = wins
            .iter()
            .find(|w| w["kind"] == "positive_equity_day")
            .unwrap();
        assert_eq!(eq["days"], json!(2));
        assert_eq!(eq["total"], json!(14));

        // ANONYMIZED: the serialized summary names no lane.
        let s = summary.to_string();
        assert!(
            !s.contains("sover") && !s.contains("asmodeus"),
            "wins summary must be anonymized: {s}"
        );
    }

    #[test]
    fn an_empty_ledger_yields_an_honest_empty_summary_not_a_fabricated_win() {
        let summary = wins_summary_from_lines(&[], 14);
        assert_eq!(summary["wins"], json!([]));
        assert!(
            !has_wins(&summary),
            "no lines => no wins (never fabricated)"
        );
        // a ledger of only zero-outcome days is also honestly empty.
        let zero_day = json!({"projects": {"maki": {"shipped_24h": 0, "live_trades_24h": 0}}});
        let summary2 = wins_summary_from_lines(&[zero_day.to_string()], 14);
        assert!(!has_wins(&summary2));
    }

    #[test]
    fn a_malformed_line_is_skipped_not_an_error() {
        let good = json!({"projects": {"x": {"shipped_24h": 1}}}).to_string();
        let lines = vec!["not json at all".to_string(), good, "{ broken".to_string()];
        let summary = wins_summary_from_lines(&lines, 14);
        assert!(
            has_wins(&summary),
            "one good line still yields its win despite malformed neighbors"
        );
    }
}
