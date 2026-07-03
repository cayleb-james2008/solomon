//! The outcomes ledger (Solomon v2, Phase A) — BUSINESS outcomes per 24 h window, per project.
//!
//! The probe plane answers "is the product alive right now?"; this module answers "what did the
//! product actually DO in the last day?" — posts published (with URLs), equity moved, live trades
//! filled, lane iterations shipped. These are the numbers the June post-mortem showed nobody was
//! looking at: lanes shipped PRs while Sover published nothing for 5 days and Asmodeus's equity
//! sat at the same dollar figure for 10k straight snapshots.
//!
//! Deterministic Rust only — no LLM anywhere (same contract as the probe plane). Sources are
//! DISCOVERED from ops.json so paths live in exactly one place:
//!   - every project: `runtime/<name>/history.jsonl` (lane iterations / ships)
//!   - a project with a `publish_recency` probe: that probe's `file` is the post registry
//!     (a dict keyed `id:platform`, entries carrying `published_at` + `url`)
//!   - a project with a `sqlite_query` probe: that probe's `db` is the finance database
//!     (asmodeus schema: equity / trades / venue_fills), opened READ-ONLY like every ops probe
//!
//! `evening summary` appends one line per day to `runtime/outcomes.jsonl` (append-only) — the
//! honest daily record the CEO rhythm plans against and the Asmodeus profit-milestone reads from.
#![allow(dead_code)]

use super::{probe, registry};
use crate::control::paths;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Map, Value};
use std::path::Path;

/// The ledger window: one day.
const WINDOW_S: i64 = 86_400;

/// HERE/runtime/outcomes.jsonl — the append-only daily outcome ledger.
pub fn ledger_path() -> std::path::PathBuf {
    paths::here().join("runtime").join("outcomes.jsonl")
}

/// House timestamp format.
fn fmt(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// --------------------------------------------------------------------------- //
// per-source collectors (path-parameterized — the unit-tested cores)
// --------------------------------------------------------------------------- //

/// Lane activity from `runtime/<name>/history.jsonl`: iterations + ships inside the window and the
/// last iteration timestamp. Missing/corrupt file -> zeros with `last_iteration_ts: null` (a lane
/// that never fired must read as ZERO, never as an error that hides the zero — the
/// daedalus-trainer failure mode was precisely a silent never-fired).
pub fn lane_activity(history: &Path, now: DateTime<Utc>) -> Value {
    let cutoff = now - Duration::seconds(WINDOW_S);
    let mut iterations = 0i64;
    let mut shipped = 0i64;
    let mut last_ts: Option<String> = None;
    if let Ok(content) = std::fs::read_to_string(history) {
        for line in content.lines() {
            let rec: Value = match serde_json::from_str(line.trim()) {
                Ok(r) => r,
                Err(_) => continue, // one malformed line must not zero the count
            };
            let ts_raw = rec.get("ts").cloned().unwrap_or(Value::Null);
            let ts = match probe::parse_ts(&ts_raw) {
                Some(t) => t,
                None => continue,
            };
            // history is append-ordered; track the max defensively anyway.
            if last_ts.as_deref().map(|p| fmt(ts).as_str() > p).unwrap_or(true) {
                last_ts = Some(fmt(ts));
            }
            if ts >= cutoff {
                iterations += 1;
                if rec.get("status").and_then(Value::as_str) == Some("shipped") {
                    shipped += 1;
                }
            }
        }
    }
    json!({
        "iterations_24h": iterations,
        "shipped_24h": shipped,
        "last_iteration_ts": last_ts,
    })
}

/// Post activity from a Sover-style post registry (dict keyed `id:platform`, entries carrying
/// `published_at` + `url`): publishes inside the window, publishes CLAIMED without a URL (counted
/// separately — a `published_at` with an empty `url` is a claim with no evidence, the June 30
/// TikTok case), and the newest publish timestamp overall.
pub fn posts_activity(registry_file: &Path, now: DateTime<Utc>) -> Value {
    let cutoff = now - Duration::seconds(WINDOW_S);
    let data: Option<Value> = std::fs::read(registry_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let entries = match data.as_ref().and_then(Value::as_object) {
        Some(o) => o,
        None => {
            return json!({
                "posts_24h": Value::Null,
                "posts_missing_url_24h": Value::Null,
                "last_post_at": Value::Null,
                "unobservable": format!("unreadable post registry {}", registry_file.display()),
            })
        }
    };
    let mut posts = 0i64;
    let mut missing_url = 0i64;
    let mut last: Option<DateTime<Utc>> = None;
    for (_, entry) in entries {
        let ts_raw = entry.get("published_at").cloned().unwrap_or(Value::Null);
        let ts = match probe::parse_ts(&ts_raw) {
            Some(t) => t,
            None => continue,
        };
        if last.map(|p| ts > p).unwrap_or(true) {
            last = Some(ts);
        }
        if ts >= cutoff {
            posts += 1;
            let url_empty = entry
                .get("url")
                .and_then(Value::as_str)
                .map(|u| u.trim().is_empty())
                .unwrap_or(true);
            if url_empty {
                missing_url += 1;
            }
        }
    }
    json!({
        "posts_24h": posts,
        "posts_missing_url_24h": missing_url,
        "last_post_at": last.map(fmt),
    })
}

/// Finance activity from the asmodeus-schema SQLite db (READ-ONLY, same open flags as the probe
/// plane): current equity, 24 h equity delta, live trade entries and venue fills inside the
/// window. Each field degrades INDEPENDENTLY to null on a query error (partial truth beats none);
/// a completely unreadable db reports `unobservable`.
pub fn finance_activity(db: &Path, now: DateTime<Utc>) -> Value {
    if !db.exists() {
        return json!({"unobservable": format!("missing db {}", db.display())});
    }
    let cutoff = fmt(now - Duration::seconds(WINDOW_S));
    // ISO-8601 strings compare lexicographically; stored values carry fractional seconds + Z after
    // the "YYYY-MM-DDTHH:MM:SS" prefix, which only sorts them later — >= / <= against the bare
    // prefix stays correct.
    let q = |sql: String| probe::sqlite_single_value(db, &sql).ok().unwrap_or(Value::Null);
    let equity_now = q("SELECT equity_usd FROM equity WHERE source='tradelocker' ORDER BY id DESC LIMIT 1".into());
    let equity_then = q(format!(
        "SELECT equity_usd FROM equity WHERE source='tradelocker' AND ts <= '{cutoff}' ORDER BY id DESC LIMIT 1"
    ));
    let delta = match (equity_now.as_f64(), equity_then.as_f64()) {
        (Some(a), Some(b)) => json!(((a - b) * 100.0).round() / 100.0),
        _ => Value::Null,
    };
    json!({
        "equity_usd": equity_now,
        "equity_delta_24h": delta,
        "live_trades_24h": q(format!(
            "SELECT COUNT(*) FROM trades WHERE mode='live' AND created_at >= '{cutoff}'"
        )),
        "fills_24h": q(format!(
            "SELECT COUNT(*) FROM venue_fills WHERE created_at >= '{cutoff}'"
        )),
    })
}

// --------------------------------------------------------------------------- //
// snapshot
// --------------------------------------------------------------------------- //

/// One project's outcome entry (pure over the ops.json entry + now — the unit-tested core).
/// Source discovery: lane history always; posts when a `publish_recency` probe names a file;
/// finance when a `sqlite_query` probe names a db.
pub fn project_outcomes(entry: &Value, now: DateTime<Utc>) -> Value {
    let name = registry::project_name(entry);
    let repo_path = registry::project_repo_path(&name);
    let mut out = Map::new();
    out.insert("priority".into(), json!(registry::project_priority(entry)));

    let history = paths::here().join("runtime").join(&name).join("history.jsonl");
    if let Value::Object(m) = lane_activity(&history, now) {
        out.extend(m);
    }

    for cfg in registry::project_probes(entry) {
        let id = cfg.get("id").and_then(Value::as_str).unwrap_or("");
        let kind = cfg.get("kind").and_then(Value::as_str).unwrap_or("");
        // 2026-07-03: publish_recency was split into one probe per platform
        // (publish_recency_instagram/tiktok/youtube, all pointing at the same post_registry.json) so
        // a dead platform can't hide behind a healthy one's fresh timestamp — see ops.json. Match the
        // family by prefix so posts_activity (aggregate counts, not platform-specific) still computes
        // from whichever one is present; the contains_key guard means it only runs once even though
        // all 3 probes point at the same file.
        if id.starts_with("publish_recency") && !out.contains_key("posts_24h") {
            if let Some(f) = cfg.get("file").and_then(Value::as_str) {
                if let Value::Object(m) = posts_activity(&registry::resolve_path(f, &repo_path), now) {
                    out.extend(m);
                }
            }
        } else if kind == "sqlite_query" && !out.contains_key("equity_usd") {
            if let Some(db) = cfg.get("db").and_then(Value::as_str) {
                if let Value::Object(m) = finance_activity(&registry::resolve_path(db, &repo_path), now) {
                    out.extend(m);
                }
            }
        }
    }
    Value::Object(out)
}

/// The fleet outcome snapshot: every ops.json project, priority order.
pub fn snapshot() -> Value {
    let now = Utc::now();
    let mut entries = registry::load_ops();
    entries.sort_by_key(registry::project_priority);
    let mut projects = Map::new();
    for entry in &entries {
        projects.insert(registry::project_name(entry), project_outcomes(entry, now));
    }
    json!({
        "ts": fmt(now),
        "window_s": WINDOW_S,
        "projects": projects,
    })
}

/// Append one daily record to runtime/outcomes.jsonl (append-only, OSError -> pass; called once
/// per day by the evening summary — the `date` field is the dedupe key for readers).
pub fn append_daily(snap: &Value) {
    let _ = (|| -> std::io::Result<()> {
        let path = ledger_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut rec = snap.clone();
        if let Some(o) = rec.as_object_mut() {
            o.insert(
                "date".into(),
                json!(chrono::Local::now().format("%Y-%m-%d").to_string()),
            );
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        writeln!(f, "{}", serde_json::to_string(&rec).unwrap_or_default())?;
        Ok(())
    })();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "solomon_ledger_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ))
    }

    // -------- lane_activity --------
    #[test]
    fn lane_activity_counts_window_and_ships() {
        let now = Utc::now();
        let fresh = fmt(now - Duration::seconds(600));
        let old = fmt(now - Duration::seconds(2 * WINDOW_S));
        let p = temp_file("hist.jsonl");
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\nnot json\n",
                json!({"ts": old, "status": "shipped"}),
                json!({"ts": fresh, "status": "shipped"}),
                json!({"ts": fresh, "status": "noop"}),
            ),
        )
        .unwrap();
        let got = lane_activity(&p, now);
        assert_eq!(got["iterations_24h"], json!(2)); // the old ship is outside the window
        assert_eq!(got["shipped_24h"], json!(1));
        assert_eq!(got["last_iteration_ts"], json!(fresh));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn lane_activity_missing_file_is_zero_not_error() {
        // A lane that never fired must read ZERO (the daedalus-trainer failure mode: silence
        // was indistinguishable from success because nothing rendered the zero).
        let got = lane_activity(Path::new("Z:/absent/history.jsonl"), Utc::now());
        assert_eq!(got["iterations_24h"], json!(0));
        assert_eq!(got["shipped_24h"], json!(0));
        assert_eq!(got["last_iteration_ts"], Value::Null);
    }

    // -------- posts_activity --------
    #[test]
    fn posts_activity_counts_fresh_and_missing_urls() {
        let now = Utc::now();
        let fresh = fmt(now - Duration::seconds(3600));
        let old = fmt(now - Duration::seconds(2 * WINDOW_S));
        let p = temp_file("registry.json");
        std::fs::write(
            &p,
            serde_json::to_string(&json!({
                "a:tiktok":   {"published_at": fresh, "url": "https://tiktok.com/x"},
                "a:youtube":  {"published_at": fresh, "url": ""},
                "b:tiktok":   {"published_at": old,   "url": "https://tiktok.com/y"},
                "junk":       {"note": "no published_at — skipped"},
            }))
            .unwrap(),
        )
        .unwrap();
        let got = posts_activity(&p, now);
        assert_eq!(got["posts_24h"], json!(2));
        assert_eq!(got["posts_missing_url_24h"], json!(1)); // claimed publish, no URL evidence
        assert_eq!(got["last_post_at"], json!(fresh));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn posts_activity_unreadable_registry_is_unobservable() {
        let got = posts_activity(Path::new("Z:/absent/registry.json"), Utc::now());
        assert_eq!(got["posts_24h"], Value::Null); // null, never a fake zero
        assert!(got["unobservable"].as_str().unwrap().contains("unreadable"));
    }

    // -------- finance_activity (over a real temp sqlite db) --------
    #[test]
    fn finance_activity_equity_delta_and_counts() {
        let now = Utc::now();
        let p = temp_file("asm.db");
        {
            let conn = rusqlite::Connection::open(&p).unwrap();
            conn.execute_batch(
                "CREATE TABLE equity (id INTEGER PRIMARY KEY, ts TEXT, equity_usd REAL, mode TEXT, source TEXT);
                 CREATE TABLE trades (id INTEGER PRIMARY KEY, mode TEXT, created_at TEXT);
                 CREATE TABLE venue_fills (id INTEGER PRIMARY KEY, created_at TEXT);",
            )
            .unwrap();
            let fresh = fmt(now - Duration::seconds(600));
            let old = fmt(now - Duration::seconds(2 * WINDOW_S));
            conn.execute(
                "INSERT INTO equity (ts, equity_usd, mode, source) VALUES (?1, 150.0, 'live', 'tradelocker')",
                [&old],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO equity (ts, equity_usd, mode, source) VALUES (?1, 168.97, 'live', 'tradelocker')",
                [&fresh],
            )
            .unwrap();
            // a paper-source row must NOT be picked up as current equity
            conn.execute(
                "INSERT INTO equity (ts, equity_usd, mode, source) VALUES (?1, 999.0, 'paper', 'fleet_ticker')",
                [&fresh],
            )
            .unwrap();
            conn.execute("INSERT INTO trades (mode, created_at) VALUES ('live', ?1)", [&fresh]).unwrap();
            conn.execute("INSERT INTO trades (mode, created_at) VALUES ('paper', ?1)", [&fresh]).unwrap();
            conn.execute("INSERT INTO trades (mode, created_at) VALUES ('live', ?1)", [&old]).unwrap();
            conn.execute("INSERT INTO venue_fills (created_at) VALUES (?1)", [&fresh]).unwrap();
        }
        let got = finance_activity(&p, now);
        assert_eq!(got["equity_usd"], json!(168.97));
        assert_eq!(got["equity_delta_24h"], json!(18.97)); // 168.97 - 150.0, 2dp
        assert_eq!(got["live_trades_24h"], json!(1)); // live+fresh only
        assert_eq!(got["fills_24h"], json!(1));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn finance_activity_missing_db_is_unobservable() {
        let got = finance_activity(Path::new("Z:/absent/asm.db"), Utc::now());
        assert!(got["unobservable"].as_str().unwrap().contains("missing db"));
    }

    // -------- project_outcomes source discovery --------
    #[test]
    fn project_outcomes_discovers_posts_source_from_publish_recency_probe() {
        let now = Utc::now();
        let reg = temp_file("disc_registry.json");
        std::fs::write(
            &reg,
            serde_json::to_string(&json!({
                "x:tiktok": {"published_at": fmt(now - Duration::seconds(60)), "url": "https://t/x"}
            }))
            .unwrap(),
        )
        .unwrap();
        let entry = json!({
            "name": "ledger_disc_test",
            "priority": 2,
            "probes": [
                {"id": "publish_recency", "kind": "json_field", "file": reg.to_string_lossy()},
                {"id": "other", "kind": "file_age", "file": "x"}
            ]
        });
        let got = project_outcomes(&entry, now);
        assert_eq!(got["priority"], json!(2));
        assert_eq!(got["posts_24h"], json!(1));
        // no sqlite probe -> no finance keys at all (absent, not null-noise)
        assert!(got.get("equity_usd").is_none());
        // lane history for an unknown lane -> zeros
        assert_eq!(got["iterations_24h"], json!(0));
        let _ = std::fs::remove_file(&reg);
    }
}
