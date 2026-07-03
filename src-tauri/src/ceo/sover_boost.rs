//! Extra sover produce/post one-shots — the real profit lever, ridden on the every-sweep CEO tick.
//!
//! `deploy.rs` rebuilds a stalled managed app; this is its GROWTH sibling for sover specifically:
//! when the fleet is HEALTHY, sover is GREEN, and sover's post throughput is BEHIND its daily
//! north-star target, Solomon fires ONE extra `sover.exe --live --lane produce` + `--lane post`
//! one-shot to close the gap. The keystone invariant holds: this NEVER hand-patches a managed
//! working tree — it invokes sover's own published produce/post lanes (the sanctioned one-shot),
//! nothing else.
//!
//! HARD SAFETY CONTRACT (mirrors deploy.rs):
//!   - A boost fires ONLY IFF sover carries a `produce_boost` config (opt-in). Absent => NEVER boost.
//!   - A boost fires ONLY when the fleet rollup for sover is HEALTHY + GREEN (never pile produce
//!     work onto a RED/degraded lane — fix first, grow second) AND the primary metric is posts_24h
//!     AND the trend is "behind" (a stated daily target the current output is under).
//!   - Per-day cap (`max_extra_per_day`) + per-sweep cooldown (`cooldown_s`) together bound the rate;
//!     STAMP-FIRST means a hung/failed run consumes the cooldown+count and cannot re-fire early.
//!   - At most ONE boost per tick (MAX_BOOSTS_PER_SWEEP) — produce+post is heavy (a browser session).
//!
//! `should_boost` is PURE (no IO) so the whole gate is unit-tested. `run_boost` is side-effectful;
//! it stamps BEFORE running, then pages the operator LOUDLY (report on success, red on a non-zero
//! exit) — a real profit action they must see.
#![allow(dead_code)]

use crate::control::{paths, proc};
use crate::notify::{self, Notice};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

/// Fallbacks used only when the `produce_boost` block omits a field (config is authoritative).
const DEFAULT_COOLDOWN_S: i64 = 3600;
const DEFAULT_MAX_PER_DAY: i64 = 3;
/// sover's daily post north star (the velocity target this boost steers toward). The operator's
/// stated cadence is "3 reels/day"; the number is the target `velocity_context` parses.
const SOVER_POST_TARGET: i64 = 3;
/// Per-tick cap: produce+post drives a real browser session — never stack more than one per sweep.
const MAX_BOOSTS_PER_SWEEP: usize = 1;
/// Each lane one-shot's wall-clock ceiling (produce, then post).
const BOOST_TIMEOUT_S: u64 = 600;

// --------------------------------------------------------------------------- //
// GATE — pure, the load-bearing safety predicate
// --------------------------------------------------------------------------- //

/// The boost predicate: fire ONE extra produce/post one-shot IFF ALL of:
///   - `boost_cfg` is present (`produce_boost` opt-in), AND
///   - the fleet rollup for sover is HEALTHY (`rollup.healthy == true`) and GREEN
///     (`rollup.status == "green"`) — never pile produce work onto a degraded lane, AND
///   - the primary velocity metric is `posts_24h` and its trend is `"behind"` (a stated daily
///     target the current output is under — room to grow toward the milestone), AND
///   - the per-sweep cooldown has elapsed (`cooldown_elapsed`), AND
///   - today's boost count is under the per-day cap (`boosts_today < max_extra_per_day`).
///
/// Pure — the caller collects the rollup / velocity / cooldown / count (IO) and passes them in.
pub fn should_boost(
    boost_cfg: Option<&Value>,
    rollup: &Value,
    velocity: &Value,
    cooldown_elapsed: bool,
    boosts_today: i64,
) -> bool {
    let cfg = match boost_cfg {
        Some(c) => c,
        None => return false, // no produce_boost config -> NEVER boost (opt-in only)
    };
    let healthy = rollup.get("healthy").and_then(Value::as_bool).unwrap_or(false);
    let green = rollup.get("status").and_then(Value::as_str) == Some("green");
    if !(healthy && green) {
        return false; // only grow a healthy, green lane
    }
    let metric_is_posts = velocity.get("metric").and_then(Value::as_str) == Some("posts_24h");
    let behind = velocity.get("trend").and_then(Value::as_str) == Some("behind");
    if !(metric_is_posts && behind) {
        return false; // only when posts are the primary metric AND under target
    }
    if !cooldown_elapsed {
        return false; // per-sweep cooldown gates the rate
    }
    boosts_today < max_extra_per_day(cfg)
}

/// The per-day boost cap from `produce_boost.max_extra_per_day`, default DEFAULT_MAX_PER_DAY.
fn max_extra_per_day(boost_cfg: &Value) -> i64 {
    boost_cfg
        .get("max_extra_per_day")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_PER_DAY)
}

/// The cooldown (seconds) from `produce_boost.cooldown_s`, default DEFAULT_COOLDOWN_S.
fn cooldown_s(boost_cfg: &Value) -> i64 {
    boost_cfg
        .get("cooldown_s")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_COOLDOWN_S)
}

/// Parse the daily counter file contents to an i64 (pure — unit-tested). Absent/garbage => 0, so a
/// missing or corrupt counter never blocks (fail-open toward "no boosts recorded yet").
pub fn parse_boost_count(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

// --------------------------------------------------------------------------- //
// COOLDOWN + COUNT MARKERS  (runtime/sover/_last_boost, _boost_count_<YYYY-MM-DD>)
// --------------------------------------------------------------------------- //

/// runtime/sover/ — sover's per-repo runtime dir (markers live UNDER Solomon, never in the product).
fn sover_runtime_dir() -> PathBuf {
    paths::here().join("runtime").join("sover")
}

fn last_boost_path() -> PathBuf {
    sover_runtime_dir().join("_last_boost")
}

fn boost_count_path(date: &str) -> PathBuf {
    sover_runtime_dir().join(format!("_boost_count_{date}"))
}

/// Age (seconds) since the last boost, or None when there was none / the marker is
/// unreadable/unparseable (treated as "no prior boost" -> cooldown elapsed).
fn last_boost_age_s() -> Option<i64> {
    let raw = std::fs::read_to_string(last_boost_path()).ok()?;
    let last = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%SZ")
        .ok()?
        .and_utc();
    Some((chrono::Utc::now() - last).num_seconds())
}

/// Today's boost count from the per-date counter file (absent/garbage => 0).
fn boosts_today(date: &str) -> i64 {
    std::fs::read_to_string(boost_count_path(date))
        .map(|s| parse_boost_count(&s))
        .unwrap_or(0)
}

/// STAMP FIRST: write `_last_boost` (now) and increment `_boost_count_<date>` BEFORE running the
/// boost, so a hung/failed run still consumes the cooldown + daily budget and cannot re-fire early.
/// Best-effort (OSError -> pass); the gate re-reads these on the next tick.
fn stamp_boost(date: &str) {
    let _ = (|| -> std::io::Result<()> {
        let dir = sover_runtime_dir();
        std::fs::create_dir_all(&dir)?;
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(last_boost_path(), ts)?;
        let next = boosts_today(date) + 1;
        std::fs::write(boost_count_path(date), next.to_string())?;
        Ok(())
    })();
}

// --------------------------------------------------------------------------- //
// EXECUTE — run the produce then post one-shots
// --------------------------------------------------------------------------- //

/// Read a lane argv from `produce_boost.<field>` (pure — unit-tested). None when absent/empty.
fn boost_argv(boost_cfg: &Value, field: &str) -> Option<Vec<String>> {
    let arr = boost_cfg.get(field).and_then(Value::as_array)?;
    let argv: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if argv.is_empty() { None } else { Some(argv) }
}

/// Run one lane one-shot with sover's cleaned env plus `SOVER_PROFILE=<profile>`, a fixed cwd, and a
/// BOOST_TIMEOUT_S ceiling. Mirrors `proc::run`'s scrubbed-env + hidden-window contract, adding the
/// one profile env sover's `--live` state root needs. Returns the RunOut (or Err on spawn/timeout).
fn run_lane(argv: &[String], cwd: &str, profile: &str) -> std::io::Result<proc::RunOut> {
    use std::io::Read;
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    if !cwd.is_empty() {
        cmd.current_dir(cwd);
    }
    proc::apply_clean_env(&mut cmd); // scrub GH/PYTHON* + force UTF-8 stdio (same as proc::run)
    cmd.env("SOVER_PROFILE", profile);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(proc::CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            buf
        })
    });
    let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    use wait_timeout::ChildExt;
    match child.wait_timeout(Duration::from_secs(BOOST_TIMEOUT_S))? {
        Some(status) => Ok(proc::RunOut {
            code: status.code().unwrap_or(-1),
            stdout: join(out_h),
            stderr: join(err_h),
        }),
        None => {
            let _ = child.kill();
            let _ = child.wait();
            // Detach readers (a grandchild may hold the pipe) — same rationale as proc::run's timeout.
            drop(out_h);
            drop(err_h);
            Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "boost lane timed out"))
        }
    }
}

/// Scan stdout lines from the END; the first trimmed line starting with '{' is the lane's JSON
/// status line (mirrors control::runner::parse_last_brace_line). None when there is none/unparseable.
fn last_json_line(stdout: &str) -> Option<Value> {
    for line in stdout.trim().lines().rev() {
        let line = line.trim();
        if line.starts_with('{') {
            return serde_json::from_str::<Value>(line).ok();
        }
    }
    None
}

/// A compact one-line summary of a lane's trailing JSON status for the operator page.
fn lane_summary(out: &proc::RunOut) -> String {
    match last_json_line(&out.stdout) {
        Some(v) => notify_one_line(&v.to_string()),
        None => {
            let tail: String = out.stderr.trim().chars().take(160).collect();
            if tail.is_empty() { "(no status line)".to_string() } else { tail }
        }
    }
}

fn notify_one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").chars().take(240).collect()
}

/// Run ONE boost for `repo_cfg` (sover): STAMP FIRST (cooldown + daily count), then run the produce
/// lane, then the post lane, each with sover's cleaned env + `SOVER_PROFILE` and a 600 s ceiling.
/// Pages the operator LOUDLY — `Notice::report` on success, `Notice::red` on any non-zero exit /
/// spawn / timeout. Returns Ok(()) when both lanes exit 0, Err(msg) otherwise (already paged).
pub fn run_boost(repo_cfg: &Value) -> Result<(), String> {
    let boost = repo_cfg
        .get("produce_boost")
        .ok_or_else(|| "sover: produce_boost missing".to_string())?;
    let cwd = boost.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
    let profile = boost.get("profile_env").and_then(Value::as_str).unwrap_or("ggg").to_string();
    let produce = boost_argv(boost, "produce")
        .ok_or_else(|| "sover: produce_boost.produce missing/empty".to_string())?;
    let post = boost_argv(boost, "bin")
        .ok_or_else(|| "sover: produce_boost.bin (post) missing/empty".to_string())?;

    // STAMP FIRST — a hung/failed run must still burn the cooldown + daily budget (never re-fire early).
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    stamp_boost(&date);

    // produce, then post. A non-zero exit / spawn error / timeout on EITHER is a real, paged failure.
    let pr = run_lane(&produce, &cwd, &profile).map_err(|e| {
        page_red(&format!("produce spawn/timeout: {e}"));
        format!("sover boost: produce failed: {e}")
    })?;
    if !pr.ok() {
        let s = lane_summary(&pr);
        page_red(&format!("produce exit {}: {s}", pr.code));
        return Err(format!("sover boost: produce exit {} ({s})", pr.code));
    }
    let po = run_lane(&post, &cwd, &profile).map_err(|e| {
        page_red(&format!("post spawn/timeout: {e}"));
        format!("sover boost: post failed: {e}")
    })?;
    if !po.ok() {
        let s = lane_summary(&po);
        page_red(&format!("post exit {}: {s}", po.code));
        return Err(format!("sover boost: post exit {} ({s})", po.code));
    }

    page_ok(&format!("produce: {} | post: {}", lane_summary(&pr), lane_summary(&po)));
    Ok(())
}

/// LOUD success page (a real extra post shipped — the operator should see the profit action).
fn page_ok(detail: &str) {
    let _ = notify::send(&Notice::report(
        "Solomon: sover produce/post boost".into(),
        format!("extra one-shot fired to close the posts/day gap — {detail}"),
    ));
}

/// LOUD failure page (the boost fired but a lane failed — a real, actionable state change).
fn page_red(detail: &str) {
    let _ = notify::send(&Notice::red(
        "Solomon: sover boost FAILED".into(),
        detail.to_string(),
    ));
}

// --------------------------------------------------------------------------- //
// ORCHESTRATOR — the every-sweep tick entry (called from ceo::tick by the integrator)
// --------------------------------------------------------------------------- //

/// The sover boost check, ridden on the every-sweep CEO tick. Locates the sover lane in
/// `read_repos_json()`; if it has no `produce_boost` block, returns (opt-in only). Otherwise computes
/// the velocity (posts_24h vs the SOVER_POST_TARGET daily north star) from the ledger `snapshot`, the
/// rollup from the ops `status`, the cooldown from `_last_boost`, and today's count from the counter
/// file; if `should_boost`, fires ONE boost. At most MAX_BOOSTS_PER_SWEEP per call.
///
/// `snapshot` is the ledger snapshot (`projects.<name>` outcomes); `status` is the ops rollup
/// (`projects.<name>` {healthy,status,...}). Both are the same payloads the CEO report already reads.
pub fn maybe_boost(snapshot: &Value, status: &Value) {
    // Locate the sover lane's explicit repos.json entry (produce_boost is an explicit opt-in field).
    let repo_cfg = match crate::control::registry::read_repos_json()
        .into_iter()
        .find(|r| paths::repo_name(r) == "sover")
    {
        Some(r) => r,
        None => return, // no sover lane configured
    };
    if repo_cfg.get("produce_boost").is_none() {
        return; // opt-in only — no produce_boost block => never boost
    }
    let boost_cfg = repo_cfg.get("produce_boost");

    // Velocity: sover's 24h outcomes vs the "3 posts/day" north star (target => posts_24h "behind").
    let outcomes = snapshot
        .get("projects")
        .and_then(|p| p.get("sover"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let north_star = format!("Post {SOVER_POST_TARGET} reels/day");
    let velocity = crate::ceo::velocity_context(&outcomes, &north_star);

    // Rollup: sover's ops_status project entry ({healthy,status,...}).
    let rollup = status
        .get("projects")
        .and_then(|p| p.get("sover"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let cd = cooldown_s(boost_cfg.unwrap()); // boost_cfg is Some here (checked above)
    let cooldown_elapsed = last_boost_age_s().map(|age| age > cd).unwrap_or(true);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let count = boosts_today(&date);

    let mut left = MAX_BOOSTS_PER_SWEEP;
    if left > 0 && should_boost(boost_cfg, &rollup, &velocity, cooldown_elapsed, count) {
        left -= 1;
        let _ = left; // one boost per call — the cap is documented + enforced by this single branch
        let _ = run_boost(&repo_cfg);
    }
}

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn boost_cfg() -> Value {
        json!({
            "bin": ["C:\\Users\\Cayleb\\sover-live\\target\\release\\sover.exe", "--live", "--lane", "post"],
            "produce": ["C:\\Users\\Cayleb\\sover-live\\target\\release\\sover.exe", "--live", "--lane", "produce"],
            "cwd": "C:\\Users\\Cayleb\\sover-live",
            "profile_env": "ggg",
            "cooldown_s": 3600,
            "max_extra_per_day": 3
        })
    }
    fn green_rollup() -> Value {
        json!({"healthy": true, "status": "green", "process_green": true, "outcomes_green": true})
    }
    // posts_24h behind the target of 3 (current 1 -> trend "behind").
    fn behind_velocity() -> Value {
        crate::ceo::velocity_context(&json!({"posts_24h": 1}), "Post 3 reels/day")
    }

    // -------- should_boost: the full-condition decision table --------
    #[test]
    fn should_boost_true_only_on_full_condition() {
        let cfg = boost_cfg();
        assert!(
            should_boost(Some(&cfg), &green_rollup(), &behind_velocity(), true, 0),
            "all conditions met -> boost"
        );
        // one under the cap still boosts
        assert!(should_boost(Some(&cfg), &green_rollup(), &behind_velocity(), true, 2));
    }

    #[test]
    fn should_boost_false_when_sover_not_green() {
        let cfg = boost_cfg();
        // RED rollup
        let red = json!({"healthy": false, "status": "red"});
        assert!(!should_boost(Some(&cfg), &red, &behind_velocity(), true, 0), "RED -> no boost");
        // yellow-ish: status not green even if some flag true
        let yellow = json!({"healthy": true, "status": "yellow"});
        assert!(!should_boost(Some(&cfg), &yellow, &behind_velocity(), true, 0), "yellow -> no boost");
        // healthy true but status missing -> not green -> no boost
        let no_status = json!({"healthy": true});
        assert!(!should_boost(Some(&cfg), &no_status, &behind_velocity(), true, 0));
    }

    #[test]
    fn should_boost_false_when_trend_not_behind() {
        let cfg = boost_cfg();
        // healthy trend (current 3 meets target 3) -> not "behind" -> no boost
        let healthy_v = crate::ceo::velocity_context(&json!({"posts_24h": 3}), "Post 3 reels/day");
        assert_eq!(healthy_v["trend"], json!("healthy"));
        assert!(!should_boost(Some(&cfg), &green_rollup(), &healthy_v, true, 0), "healthy trend -> no boost");
        // stalled (current 0) is not "behind" -> no boost (fix a dead engine, don't pile on produce)
        let stalled_v = crate::ceo::velocity_context(&json!({"posts_24h": 0}), "Post 3 reels/day");
        assert_eq!(stalled_v["trend"], json!("stalled"));
        assert!(!should_boost(Some(&cfg), &green_rollup(), &stalled_v, true, 0), "stalled -> no boost");
    }

    #[test]
    fn should_boost_false_when_metric_not_posts() {
        let cfg = boost_cfg();
        // an asmodeus-shaped lane: primary metric is live_trades_24h, not posts_24h -> no boost
        let trades_v = crate::ceo::velocity_context(
            &json!({"live_trades_24h": 1}),
            "Grow capital velocity — 5 live trades/day.",
        );
        assert_eq!(trades_v["metric"], json!("live_trades_24h"));
        assert!(!should_boost(Some(&cfg), &green_rollup(), &trades_v, true, 0), "non-posts metric -> no boost");
    }

    #[test]
    fn should_boost_false_when_cooldown_not_elapsed() {
        let cfg = boost_cfg();
        assert!(
            !should_boost(Some(&cfg), &green_rollup(), &behind_velocity(), false, 0),
            "cooldown not elapsed -> no boost"
        );
    }

    #[test]
    fn should_boost_false_when_at_or_over_cap() {
        let cfg = boost_cfg(); // max_extra_per_day = 3
        assert!(!should_boost(Some(&cfg), &green_rollup(), &behind_velocity(), true, 3), "== cap -> no boost");
        assert!(!should_boost(Some(&cfg), &green_rollup(), &behind_velocity(), true, 9), "over cap -> no boost");
        // default cap (no max_extra_per_day) is 3
        let cfg_default = json!({"produce": ["x"], "bin": ["y"]});
        assert!(should_boost(Some(&cfg_default), &green_rollup(), &behind_velocity(), true, 2));
        assert!(!should_boost(Some(&cfg_default), &green_rollup(), &behind_velocity(), true, 3));
    }

    #[test]
    fn should_boost_false_without_config() {
        assert!(
            !should_boost(None, &green_rollup(), &behind_velocity(), true, 0),
            "no produce_boost config -> NEVER boost (opt-in only)"
        );
    }

    // -------- parse_boost_count --------
    #[test]
    fn parse_boost_count_vectors() {
        assert_eq!(parse_boost_count(""), 0, "empty -> 0");
        assert_eq!(parse_boost_count("garbage"), 0, "garbage -> 0");
        assert_eq!(parse_boost_count("2"), 2, "\"2\" -> 2");
        // trims surrounding whitespace/newline (files are written with a bare integer)
        assert_eq!(parse_boost_count(" 2 \n"), 2);
    }

    // -------- cooldown_s + max_extra_per_day defaults --------
    #[test]
    fn dials_read_from_config_with_defaults() {
        let cfg = boost_cfg();
        assert_eq!(cooldown_s(&cfg), 3600);
        assert_eq!(max_extra_per_day(&cfg), 3);
        // defaults when absent
        assert_eq!(cooldown_s(&json!({})), DEFAULT_COOLDOWN_S);
        assert_eq!(max_extra_per_day(&json!({})), DEFAULT_MAX_PER_DAY);
    }

    // -------- boost_argv: the golden produce/post vector from a sample config --------
    #[test]
    fn boost_argv_golden_produce_and_post() {
        let cfg = boost_cfg();
        assert_eq!(
            boost_argv(&cfg, "produce"),
            Some(vec![
                "C:\\Users\\Cayleb\\sover-live\\target\\release\\sover.exe".to_string(),
                "--live".to_string(),
                "--lane".to_string(),
                "produce".to_string(),
            ]),
            "produce argv must be the --live --lane produce one-shot"
        );
        assert_eq!(
            boost_argv(&cfg, "bin"),
            Some(vec![
                "C:\\Users\\Cayleb\\sover-live\\target\\release\\sover.exe".to_string(),
                "--live".to_string(),
                "--lane".to_string(),
                "post".to_string(),
            ]),
            "post argv (bin) must be the --live --lane post one-shot"
        );
        // missing/empty -> None (never spawn an empty command)
        assert_eq!(boost_argv(&json!({}), "produce"), None);
        assert_eq!(boost_argv(&json!({"produce": []}), "produce"), None);
    }

    // -------- last_json_line: pick the trailing status line --------
    #[test]
    fn last_json_line_picks_trailing_brace_line() {
        let stdout = "warming up\n[info] launching\n{\"ok\":true,\"posted\":1}\n";
        let v = last_json_line(stdout).expect("a trailing JSON line");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["posted"], json!(1));
        // no brace line -> None
        assert!(last_json_line("just logs\nno json here").is_none());
        // unparseable brace line -> None (not a panic)
        assert!(last_json_line("{not valid json").is_none());
    }

    // -------- per-sweep cap documented --------
    #[test]
    fn per_sweep_cap_is_one() {
        assert_eq!(MAX_BOOSTS_PER_SWEEP, 1, "one produce/post boost per sweep — the heavy-session guard");
    }
}
