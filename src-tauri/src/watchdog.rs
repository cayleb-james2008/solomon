//! Native Rust port of `monitor.py` — Solomon's overnight watchdog + data collector.
//!
//! Behavior is bug-for-bug with `monitor.py`. Run periodically (a Windows scheduled task — see
//! scripts/watchdog.cmd). Each sweep, for every registered repo, it:
//!
//!   1. **Restarts a CRASHED loop.** A loop that exits cleanly (operator Stop, or max-iterations)
//!      writes `status="stopped"` in its last heartbeat via the runner's `finally` block; a
//!      crash/kill leaves the last live status (`iterating`/`sleeping`/`error`). The watchdog
//!      restarts only the latter — an operator Stop is left alone, a crash is healed. The
//!      single-flight lock makes a redundant start a no-op, so this is race-safe.
//!   2. **Runs the supervisor's RUNG-0 recovery** (deterministic + reversible only: stale lock,
//!      lingering stop, dirty-tree reset when quiesced). Never a pi fix, push, merge, or force-kill.
//!   3. **Appends a JSON snapshot per repo** to `runtime/_monitor.jsonl` for overnight trend
//!      analysis (status, iteration, last outcome, diagnosis, whether it was restarted).
//!
//! Operator controls (honored, non-destructive):
//!   - `runtime/_watchdog.disabled`  — global kill-switch; the sweep does nothing.
//!   - `runtime/<name>/paused`       — never auto-restart this one repo.
//! A clean Stop from the GUI already sets `status="stopped"` and is respected automatically.
//!
//! The returns that back the JSONL/log are `serde_json::Value` whose keys are byte-identical to the
//! Python dicts; status/category/reason strings are quoted verbatim from `monitor.py`.
//!
//! Wired to the `solomon watchdog` subcommand (main.rs), which the SolomonWatchdog scheduled task
//! runs every 2 minutes. Some helpers are exercised only by tests, so allow dead-code for this module.
#![allow(dead_code)]

use crate::control::{self, paths};
use crate::supervisor;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::Path;

/// monitor.STALL_SWEEPS — consecutive error/preflight sweeps (incl. this one) that mark a lane STALLED.
const STALL_SWEEPS: usize = 3;

/// monitor.DISABLED — global kill-switch path: HERE/runtime/_watchdog.disabled.
fn disabled_path() -> std::path::PathBuf {
    paths::here().join("runtime").join("_watchdog.disabled")
}

/// monitor.MON_LOG — HERE/runtime/_monitor.jsonl.
fn mon_log() -> std::path::PathBuf {
    paths::here().join("runtime").join("_monitor.jsonl")
}

/// monitor._now: `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// monitor._recent_snapshots: the last `k` persisted `_monitor.jsonl` snapshots for repo `name`
/// (oldest→newest), excluding the current sweep (the caller appends it). `[]` on missing/corrupt
/// file.
///
/// `max_age_s`: when `Some`, drop records older than that (and any undated/unparseable record)
/// BEFORE taking the last `k`. A stall window must be temporally adjacent — otherwise stale
/// snapshots from a resolved wedge days ago (e.g. across a kill-switch pause) plus one fresh break
/// would be miscounted as "consecutive sweeps" and false-escalate.
fn recent_snapshots(name: &str, k: usize, max_age_s: Option<f64>) -> Vec<Value> {
    recent_snapshots_from(&mon_log(), name, k, max_age_s)
}

/// Path-parameterized core of `recent_snapshots` so tests can run against an isolated log file
/// (the production caller always passes `mon_log()`).
fn recent_snapshots_from(
    path: &std::path::Path,
    name: &str,
    k: usize,
    max_age_s: Option<f64>,
) -> Vec<Value> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(), // OSError -> []
    };
    let now_dt = Utc::now();
    let mut out: Vec<Value> = Vec::new();
    // Python f.read().splitlines() then `.strip()` each line.
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(_) => continue, // json.JSONDecodeError -> skip
        };
        // not (isinstance(rec, dict) and rec.get("repo") == name) -> skip
        if !(rec.is_object() && rec.get("repo").and_then(Value::as_str) == Some(name)) {
            continue;
        }
        if let Some(max_age) = max_age_s {
            // datetime.strptime(rec.get("ts"), "%Y-%m-%dT%H:%M:%SZ"); undated/unparseable -> skip.
            let ts = match rec.get("ts").and_then(Value::as_str) {
                Some(s) => s,
                None => continue, // TypeError (non-string / missing ts) -> skip
            };
            let t = match chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
                Ok(t) => t.and_utc(),
                Err(_) => continue, // ValueError -> skip (can't prove recency -> not part of a streak)
            };
            if (now_dt - t).num_milliseconds() as f64 / 1000.0 > max_age {
                continue;
            }
        }
        out.push(rec);
    }
    // out[-k:]
    if k == 0 {
        // Python out[-0:] == out[0:] == all; preserve the slice quirk for parity.
        out
    } else if k >= out.len() {
        out
    } else {
        out.split_off(out.len() - k)
    }
}

/// monitor.should_restart: restart only a CRASHED loop. Pure (no IO) so the decision is unit-tested.
///
/// True iff: not running, NOT explicitly paused, NO pending stop sentinel, and the repo has a
/// heartbeat whose status is anything except a deliberate stop/halt. Restarted: a live-phase crash
/// (iterating/sleeping/idle) AND a transient/retryable error (e.g. a flaky red base gate). Left alone:
///   - `"stopped"`                  — a clean exit (operator Stop / max-iterations);
///   - `"error"` + `phase=reverted` — a revert-failure HALT that needs operator cleanup (a blind
///                                     restart just re-hits the known-bad tree);
///   - no heartbeat                 — a repo that never ran (the watchdog keeps enabled loops alive,
///                                     it does not auto-enable new ones).
/// A persistent error (no key, dirty base) restarts, re-errors immediately, and the supervisor's
/// diagnose/anti-thrash escalates it — so it surfaces without the watchdog having to classify it.
pub fn should_restart(running: bool, hb: &Value, paused: bool, stop_pending: bool) -> bool {
    if running || paused || stop_pending {
        return false;
    }
    // status = (hb or {}).get("status")
    let status = hb.get("status").and_then(Value::as_str);
    // if not status or status == "stopped": (a JSON null / non-string / missing / "" status is falsy)
    match status {
        None | Some("") | Some("stopped") => return false,
        _ => {}
    }
    // status == "error" and (hb or {}).get("phase") == "reverted"
    if status == Some("error") && hb.get("phase").and_then(Value::as_str) == Some("reverted") {
        return false; // the revert-failure HALT — operator cleanup, not a restart
    }
    true
}

/// monitor._auto_push: read `.solomon.json`'s `auto_push` (default True; True on missing/corrupt).
fn auto_push() -> bool {
    let p = paths::here().join(".solomon.json");
    let data = match std::fs::read_to_string(&p) {
        Ok(d) => d,
        Err(_) => return true, // OSError -> True
    };
    let v: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return true, // json.JSONDecodeError -> True
    };
    // bool(json.load(f).get("auto_push", True)): missing key -> True; else Python truthiness of value.
    match v.get("auto_push") {
        None => true,
        Some(x) => json_truthy(x),
    }
}

/// Python `bool(x)` truthiness for a JSON value (as used by `_auto_push`).
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// monitor._base_is_clean: True iff the repo working tree is fully clean — no modified tracked files
/// AND no untracked non-ignored files (`git status --porcelain` empty). Used to auto-recover a
/// `dirty_base_persistent` self-stop ONLY once the operator has actually cleaned the tree, so
/// clearing the stop can never thrash (a still-dirty base stays stopped).
fn base_is_clean(path: &str) -> bool {
    if path.is_empty() || !Path::new(path).is_dir() {
        return false;
    }
    // control._run(["git", "-C", path, "status", "--porcelain"]) — windowless, guarded spawn.
    let r = match control::proc::run(&["git", "-C", path, "status", "--porcelain"], None, None) {
        Ok(r) => r,
        Err(_) => return false, // OSError -> False
    };
    r.code == 0 && r.stdout.trim().is_empty()
}

// --------------------------------------------------------------------------- //
// LIVE-EXE STALENESS: auto-rebuild a lane's LIVE production exe when it is behind
// origin/<default-branch>. Verified gap 2026-06-30: `sover.exe --live` (built from a commit now
// several PRs behind) keeps running stale render code because sover's self-updater
// (`POST /sover/update/apply`) is ON-DEMAND only — nothing auto-triggers it (asmodeus solved this
// with `ASMODEUS_AUTO_UPDATE=1` — rebuild+restart only when the book is FLAT; sover had no
// equivalent). The watchdog now fills that gap: per repos.json lane with a known live exe, it
// compares the running exe's embedded git_sha against `git rev-list origin/<default-branch>` and,
// when BEHIND, triggers a rebuild-relaunch ONLY through the app's OWN sanctioned updater path with
// hard safety gates. NEVER kill+`cargo build` a live exe directly from the watchdog (collides with
// the lane's own checkout; on Windows the running exe is file-locked).
//
// `live` config (optional repos.json field, per lane):
//   { "url": "http://127.0.0.1:8000",          // base URL of the running app (http only)
//     "health_path": "/health"                 // GET path returning JSON w/ `git_sha` (+ optional
//                                            //   `safe_to_rebuild`); default "/health"
//     "update_path": "/sover/update/apply",    // POST path of the app's OWN sanctioned updater;
//                                            //   default "/sover/update/apply"
//     "kind": "generic"|"trading"|"posting" } // default "generic"; trading/posting require an
//                                            //   explicit safe signal (book flat / outside posting
//                                            //   windows) before a rebuild is allowed.
// Absent / not an object / no truthy url -> this lane has no known live exe -> no-op.
// --------------------------------------------------------------------------- //

/// Pure behind/clean/safe predicate for the live-exe auto-rebuild gate. No IO — fully unit-tested.
/// ALL gates must hold (a single false short-circuits to "do not rebuild"):
///   - `behind`         — the live exe's embedded git_sha is NOT at origin/<default-branch> (stale).
///   - `tree_clean`     — the lane checkout has no uncommitted changes on the default branch.
///   - `on_default`     — HEAD is the default branch (never rebuild mid-RSI branch).
///   - `mid_iteration`   — a live RSI iteration is running on this checkout (MUST be false).
///   - `safe_window`    — trading: book is FLAT; posting: outside configured posting windows;
///                        generic: always true. Rebuilding a live trading exe mid-position or a
///                        poster mid-post-window risks real money / real posts.
pub fn should_auto_rebuild_live(
    behind: bool,
    tree_clean: bool,
    on_default: bool,
    mid_iteration: bool,
    safe_window: bool,
) -> bool {
    behind && tree_clean && on_default && !mid_iteration && safe_window
}

/// The lane's `live` config object, or None when absent / not an object / no truthy url.
fn live_config(r: &Value) -> Option<Value> {
    let lc = r.get("live")?;
    if !lc.is_object() {
        return None;
    }
    let url = lc.get("url").and_then(Value::as_str).unwrap_or("");
    if url.is_empty() {
        return None;
    }
    Some(lc.clone())
}

/// `git -C <path> rev-list --count <sha>..origin/<default-branch>`: how many commits origin is
/// ahead of the live exe's build sha (0 = up to date; N = behind). None on any error (spawn fails,
/// sha unknown, no origin ref) — the caller treats None as "cannot prove staleness" and does NOT
/// rebuild (conservative: never rebuild when uncertain), but still surfaces it as observable.
fn live_behind_count(path: &str, sha: &str, default_branch: &str) -> Option<u64> {
    if path.is_empty() || sha.is_empty() || default_branch.is_empty() {
        return None;
    }
    let range = format!("{sha}..origin/{default_branch}");
    let r = control::proc::run(
        &["git", "-C", path, "rev-list", "--count", &range],
        None,
        None,
    )
    .ok()?;
    if r.code != 0 {
        return None; // sha not in history / no origin ref -> cannot prove staleness
    }
    r.stdout.trim().parse::<u64>().ok()
}

/// True iff HEAD is on `default_branch` (`git rev-parse --abbrev-ref HEAD` == default_branch).
/// False on any error / empty path (the rebuild gate treats false as "not safe to rebuild").
fn on_default_branch(path: &str, default_branch: &str) -> bool {
    if path.is_empty() || default_branch.is_empty() {
        return false;
    }
    match control::proc::run(
        &["git", "-C", path, "rev-parse", "--abbrev-ref", "HEAD"],
        None,
        None,
    ) {
        Ok(o) => o.code == 0 && o.stdout.trim() == default_branch,
        Err(_) => false,
    }
}

/// `runtime/<name>/live_rebuilt.json` — the watchdog's record of the last successful sanctioned
/// rebuild (so `live_last_rebuilt` is observable across sweeps). {"ts": "..."}.
fn last_rebuilt_path(r: &Value) -> Option<std::path::PathBuf> {
    paths::runtime_dir(r).map(|d| d.join("live_rebuilt.json"))
}

fn read_last_rebuilt(r: &Value) -> Option<String> {
    let p = last_rebuilt_path(r)?;
    let data = std::fs::read_to_string(&p).ok()?;
    let v: Value = serde_json::from_str(&data).ok()?;
    v.get("ts").and_then(Value::as_str).map(str::to_string)
}

fn write_last_rebuilt(r: &Value, ts: &str) {
    if let Some(p) = last_rebuilt_path(r) {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, json!({"ts": ts}).to_string());
    }
}

/// Parse a `http://host[:port][/path]` URL into (host, port, path). `https://` is NOT supported
/// (live production apps on localhost are plain http; stdlib TcpStream has no TLS — and we never
/// want a watchdog rebuild gated on a TLS dep). None for non-http / unparseable URLs.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rfind(':') {
        Some(i) => (&authority[..i], authority[i + 1..].parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

/// Split a raw HTTP response into its body (bytes after the blank `\r\n\r\n` separator). Empty
/// when no separator is found (malformed response) — the caller's JSON parse then yields None.
fn split_http_body(buf: &[u8]) -> Vec<u8> {
    for i in 0..buf.len().saturating_sub(3) {
        if buf[i..i + 4] == *b"\r\n\r\n" {
            return buf[i + 4..].to_vec();
        }
    }
    Vec::new()
}

/// GET `<url><health_path>`, parse the JSON body, return it. None on any connect/parse error (the
/// live exe is down or not instrumented). Bounded by a 5s read timeout so a silent app can't stall
/// the watchdog sweep.
fn http_get_json(url: &str, health_path: &str) -> Option<Value> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;
    use std::time::Duration;
    let (host, port, _base) = parse_http_url(url)?;
    let addr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let req = format!(
        "GET {health_path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).ok()?;
    let body = split_http_body(&buf);
    serde_json::from_slice(&body).ok()
}

/// POST `<url><update_path>` (the app's OWN sanctioned updater — Content-Length: 0). Returns true iff
/// the app answered with a 2xx status (it has accepted and will rebuild+relaunch itself). False on
/// any connect/IO error so the watchdog never silently assumes a rebuild happened.
fn http_post(url: &str, update_path: &str) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::net::ToSocketAddrs;
    use std::time::Duration;
    let (host, port, _base) = match parse_http_url(url) {
        Some(x) => x,
        None => return false,
    };
    let addr = match (host.as_str(), port).to_socket_addrs().ok().and_then(|mut i| i.next()) {
        Some(a) => a,
        None => return false,
    };
    let mut stream = match TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    let req = format!(
        "POST {update_path} HTTP/1.0\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .map(|c| (200..300).contains(&c))
        .unwrap_or(false)
}

/// Per-lane LIVE production-exe staleness check + sanctioned auto-rebuild trigger. Called from
/// sweep_repo AFTER the base snap is built; mutates `snap` to surface `live_behind` /
/// `live_last_rebuilt` / `live_url` (so staleness is OBSERVABLE instead of silent) and, when ALL
/// safety gates hold, POSTs to the app's own sanctioned updater (`live.update_path`).
fn live_exe_sweep(r: &Value, snap: &mut Value, actions: &mut Vec<String>) {
    let Some(lc) = live_config(r) else { return; };
    let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let url = lc.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let health_path = lc
        .get("health_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("/health")
        .to_string();
    let update_path = lc
        .get("update_path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("/sover/update/apply")
        .to_string();
    let kind = lc
        .get("kind")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("generic")
        .to_string();
    let path = paths::repo_path(r);
    let default_branch = control::registry::project_pr_target_branch(r);

    // Fetch the live exe's self-reported health (embedded git_sha + optional safe_to_rebuild).
    let health = http_get_json(&url, &health_path);
    let sha = health
        .as_ref()
        .and_then(|h| h.get("git_sha").and_then(Value::as_str))
        .map(str::to_string);
    let behind_count = sha.as_ref().and_then(|s| live_behind_count(&path, s, &default_branch));
    let last_rebuilt = read_last_rebuilt(r);

    // Surface FIRST — staleness must be OBSERVABLE even when we cannot/should not rebuild.
    if let Some(obj) = snap.as_object_mut() {
        obj.insert("live_url".to_string(), json!(url));
        obj.insert(
            "live_behind".to_string(),
            match &behind_count {
                Some(n) => json!(n),
                None => Value::Null,
            },
        );
        obj.insert(
            "live_last_rebuilt".to_string(),
            match &last_rebuilt {
                Some(s) => json!(s),
                None => Value::Null,
            },
        );
    }

    let behind = behind_count.map(|n| n > 0).unwrap_or(false);
    if !behind {
        return; // up to date (or unprovable) — nothing to rebuild
    }
    let n = behind_count.unwrap_or(0);
    let tree_clean = base_is_clean(&path);
    let on_default = on_default_branch(&path, &default_branch);
    let mid_iteration = control::locks::is_running(r);
    // safe_window: the app self-reports via health["safe_to_rebuild"] when it can; otherwise a
    // generic lane is always safe, while a trading/posting lane WITHOUT an explicit safe signal is
    // treated as NOT safe (never rebuild a live trading exe without a "book flat" signal).
    let safe_window = match health.as_ref().and_then(|h| h.get("safe_to_rebuild")) {
        Some(Value::Bool(b)) => *b,
        _ => kind == "generic",
    };

    if should_auto_rebuild_live(behind, tree_clean, on_default, mid_iteration, safe_window) {
        if http_post(&url, &update_path) {
            let ts = now();
            write_last_rebuilt(r, &ts);
            if let Some(obj) = snap.as_object_mut() {
                obj.insert("live_last_rebuilt".to_string(), json!(ts));
            }
            actions.push(format!(
                "{name} live-exe auto-rebuild: {n} commits behind origin/{default_branch}, triggered sanctioned updater {url}{update_path}"
            ));
        } else {
            actions.push(format!(
                "{name} live-exe auto-rebuild: {n} behind but sanctioned updater {url}{update_path} did not respond 2xx"
            ));
        }
    } else {
        // Stale but not safe to rebuild right now — surface WHY so the operator can act, instead
        // of letting the staleness stay silent (the exact failure mode this gate exists to fix).
        let reason = if mid_iteration {
            "mid RSI iteration"
        } else if !tree_clean {
            "checkout dirty"
        } else if !on_default {
            "not on default branch"
        } else {
            "unsafe window (trading book open / posting window active)"
        };
        actions.push(format!(
            "{name} live-exe STALE: {n} commits behind origin/{default_branch} — not rebuilt ({reason})"
        ));
    }
}

/// monitor._sweep_repo: process ONE repo for sweep(); returns (actions, snap). The caller runs this
/// inside a blanket try/except so one bad repo can never abort the whole sweep — the module's
/// documented 'a watchdog must never die on one bad repo' contract.
fn sweep_repo(r: &Value, auto_push_flag: bool) -> (Vec<String>, Value) {
    let mut actions: Vec<String> = Vec::new();
    // name = r["name"] — the caller guarantees a truthy name before calling.
    let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    let rt = paths::runtime_dir(r);
    let paused = rt
        .as_ref()
        .map(|d| d.join("paused").exists())
        .unwrap_or(false);
    let mut stop_pending = rt
        .as_ref()
        .map(|d| d.join("stop").exists())
        .unwrap_or(false);
    let running = control::locks::is_running(r);
    let hb = control::heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));

    // Anti-wedge: the runner self-stops on a PERSISTENTLY dirty base (STOP sentinel +
    // reason="dirty_base_persistent") so it doesn't spin forever — but that sentinel otherwise pins
    // the lane DEAD until a human clicks Start. If the base is now CLEAN again, clear the
    // self-written sentinel so should_restart heals the lane automatically. Safe: only fires on the
    // runner's own reason marker AND a verified-clean tree, so it never thrashes and never touches a
    // true operator Stop (status="stopped", no reason).
    if !running
        && !paused
        && stop_pending
        && hb.get("status").and_then(Value::as_str) == Some("error")
        && hb.get("reason").and_then(Value::as_str) == Some("dirty_base_persistent")
        && base_is_clean(&paths::repo_path(r))
    {
        if let Some(ref d) = rt {
            if std::fs::remove_file(d.join("stop")).is_ok() {
                stop_pending = false;
                actions.push(format!(
                    "{name} auto-recover: base clean again — cleared dirty_base_persistent stop"
                ));
            }
            // OSError -> pass (leave stop_pending as-is)
        }
    }

    let mut restarted = false;
    if should_restart(running, &hb, paused, stop_pending) {
        let res = control::runner::start(r, auto_push_flag, false);
        // restarted = bool(res.get("ok") and not res.get("already"))
        restarted = res.get("ok").and_then(Value::as_bool).unwrap_or(false)
            && !res.get("already").and_then(Value::as_bool).unwrap_or(false);
        if restarted {
            actions.push(format!(
                "restarted {name} (pid {})",
                py_repr(res.get("pid"))
            ));
        } else {
            actions.push(format!(
                "restart {name} FAILED: {}",
                py_repr(res.get("error"))
            ));
        }
    }

    // RUNG-0 deterministic recovery (never a pi fix here: allow_pi=False). Skip ALL auto-action on an
    // operator-PAUSED lane — `paused` means "hands off this lane for the automated sweep", so the
    // watchdog must not stop/restart/reset_to_base it. should_restart already honors paused; recover()
    // did NOT, so a paused lane could still be stomped.
    if !paused {
        // solomon.recover(r, allow_pi=False, allow_restart=auto_push, auto_push=auto_push).
        // catch Exception -> a watchdog must never die on one bad repo. recover() is total (no panics
        // expected); the catch is reproduced as a guard around the Value field reads.
        let rec = supervisor::recover(r, false, auto_push_flag, auto_push_flag);
        if let Some(taken) = rec.get("actions_taken").and_then(Value::as_array) {
            if !taken.is_empty() {
                let joined = taken
                    .iter()
                    .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| py_repr(Some(v))))
                    .collect::<Vec<_>>()
                    .join(",");
                actions.push(format!("{name} recover: {joined}"));
            }
        }
        if json_truthy(rec.get("escalate").unwrap_or(&Value::Null)) {
            actions.push(format!("{name} ESCALATED: {}", py_repr(rec.get("category"))));
        }
    }

    // hist = control.read_history(r, limit=1); last = hist[-1] if hist else {}
    let hist = control::heartbeat::read_history(r, 1);
    let last = hist.last().cloned().unwrap_or_else(|| json!({}));
    let hb2 = control::heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));
    // diag = solomon.diagnose(r).get("category"); on Exception -> "?"
    let diag = supervisor::diagnose(r)
        .get("category")
        .cloned()
        .unwrap_or(Value::Null);

    let running2 = control::locks::is_running(r);
    let mut snap = json!({
        "ts": now(),
        "repo": name,
        "running": running2,
        "restarted": restarted,
        "paused": paused,
        "status": hb2.get("status").cloned().unwrap_or(Value::Null),
        "phase": hb2.get("phase").cloned().unwrap_or(Value::Null),
        "iteration": hb2.get("iteration").cloned().unwrap_or(Value::Null),
        "last_status": last.get("status").cloned().unwrap_or(Value::Null),
        "diagnosis": diag,
    });

    // LIVE-EXE STALENESS: if this lane runs a LIVE production exe (e.g. `sover.exe --live`, an
    // `asmodeus.exe`), compare its embedded git_sha against origin/<default-branch> and, when behind
    // AND all safety gates hold, trigger the app's OWN sanctioned updater path (a POST to
    // `live.update_path`). NEVER kill+cargo-build a live exe directly — that collides with the
    // lane's own checkout and, on Windows, the running exe is file-locked. Surface `live_behind` /
    // `live_last_rebuilt` so the staleness is OBSERVABLE instead of silent (the systemic fix for
    // "production apps silently running stale code" — the self-updater is on-demand only, nothing
    // auto-triggers it, so a live exe built from a commit N PRs behind keeps running stale code).
    live_exe_sweep(r, &mut snap, &mut actions);

    // STALL DETECTOR: a lane that is STILL RUNNING but has repeated the SAME preflight refusal
    // (status=error, phase=preflight) every sweep is wedged. Compare across RECENT sweeps: if this
    // snap AND the prior STALL_SWEEPS-1 TEMPORALLY-ADJACENT snaps are all error/preflight, escalate
    // (do NOT auto-restart — a restart just re-hits the refusal). The max_age_s recency bound stops
    // stale snaps from a resolved wedge days ago from being miscounted as a fresh streak. Anti-thrash:
    // escalate once (skip if an escalation.json with this category already exists).
    if snap.get("running").and_then(Value::as_bool) == Some(true)
        && snap.get("status").and_then(Value::as_str) == Some("error")
        && snap.get("phase").and_then(Value::as_str) == Some("preflight")
    {
        // window = _recent_snapshots(name, STALL_SWEEPS-1, max_age_s=STALL_SWEEPS*600) + [snap]
        let mut window = recent_snapshots(
            &name,
            STALL_SWEEPS - 1,
            Some((STALL_SWEEPS as f64) * 600.0),
        );
        window.push(snap.clone());
        let all_preflight = window.iter().all(|s| {
            s.get("status").and_then(Value::as_str) == Some("error")
                && s.get("phase").and_then(Value::as_str) == Some("preflight")
        });
        if window.len() >= STALL_SWEEPS && all_preflight {
            let existing = supervisor::read_escalation(r).unwrap_or_else(|| json!({}));
            if existing.get("category").and_then(Value::as_str) != Some("running_stalled") {
                actions.push(format!(
                    "{name} STALLED: stuck in preflight for {STALL_SWEEPS} sweeps"
                ));
                // hb2.get('last_summary') or '' then [:200]
                let last_summary = hb2.get("last_summary").and_then(Value::as_str).unwrap_or("");
                let evidence = format!(
                    "running but stuck in preflight for {STALL_SWEEPS} consecutive sweeps — {}",
                    py_slice_200(last_summary)
                );
                // solomon._write_escalation(r, {...}); catch Exception -> pass.
                supervisor::write_escalation(
                    r,
                    &json!({"category": "running_stalled", "evidence": evidence}),
                );
            }
        }
    }

    (actions, snap)
}

/// monitor.sweep: one watchdog pass over all repos. Returns {ts, disabled, actions, snapshots}.
pub fn sweep() -> Value {
    if disabled_path().exists() {
        return json!({"ts": now(), "disabled": true, "actions": [], "snapshots": []});
    }
    let auto_push_flag = auto_push();
    let mut actions: Vec<String> = Vec::new();
    let mut snapshots: Vec<Value> = Vec::new();
    for r in control::registry::load_repos() {
        // if not isinstance(r, dict) or not r.get("name"): continue
        if !r.is_object() {
            continue;
        }
        let has_name = r
            .get("name")
            .map(|n| json_truthy(n))
            .unwrap_or(false);
        if !has_name {
            continue;
        }
        // Per-repo guard so one bad repo doesn't abort the whole sweep — matches monitor.py's
        // per-repo try/except (sweep(), lines 218-222): a failure in one repo is logged as a
        // "<name> sweep error: <e>" action and the sweep continues to the next repo. This is the
        // crash-recovery layer, so a panic in one repo's recover()/start() must NOT stop the others
        // from being restarted. (Requires unwinding panics — see [profile.release] in Cargo.toml.)
        let name = r.get("name").and_then(Value::as_str).unwrap_or("?").to_string();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| sweep_repo(&r, auto_push_flag))) {
            Ok((repo_actions, snap)) => {
                actions.extend(repo_actions);
                snapshots.push(snap);
            }
            Err(e) => {
                let msg = e
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("panic");
                actions.push(format!("{name} sweep error: {msg}"));
            }
        }
    }
    json!({"ts": now(), "disabled": false, "actions": actions, "snapshots": snapshots})
}

/// monitor.main: the scheduled-task entry. Appends snapshots to runtime/_monitor.jsonl and the
/// human-readable line to runtime/_watchdog.out.log. Returns the process exit code.
pub fn main() -> i32 {
    let out = sweep();
    if out.get("disabled").and_then(Value::as_bool) == Some(true) {
        println!(
            "{} watchdog disabled (kill-switch present) — no action",
            out.get("ts").and_then(Value::as_str).unwrap_or("")
        );
        return 0;
    }
    let snapshots = out
        .get("snapshots")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // os.makedirs(dirname(MON_LOG), exist_ok=True); append each snapshot as a JSON line. OSError -> pass.
    let log = mon_log();
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
        for snap in &snapshots {
            writeln!(f, "{}", serde_json::to_string(snap).unwrap_or_default())?;
        }
        Ok(())
    })();

    // summary = "; ".join(actions) if actions else "all healthy, no action"
    let actions: Vec<String> = out
        .get("actions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    let summary = if actions.is_empty() {
        "all healthy, no action".to_string()
    } else {
        actions.join("; ")
    };
    // running = sum(1 for s in snapshots if s["running"])
    let running = snapshots
        .iter()
        .filter(|s| s.get("running").and_then(Value::as_bool) == Some(true))
        .count();
    let line = format!(
        "{} watchdog: {}/{} running | {}",
        out.get("ts").and_then(Value::as_str).unwrap_or(""),
        running,
        snapshots.len(),
        summary
    );
    println!("{line}");
    // self-log so the task can run windowless (no stdout redirection needed). OSError -> pass.
    let _ = (|| -> std::io::Result<()> {
        use std::io::Write;
        let out_log = paths::here().join("runtime").join("_watchdog.out.log");
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&out_log)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
    0
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// Python f-string interpolation of a possibly-missing dict value (`res.get('pid')`, etc.). A JSON
/// string renders without quotes; a missing key / null renders as Python's `None`; numbers/bools
/// render with their canonical Python text.
fn py_repr(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True" } else { "False" }.to_string(),
        Some(other) => other.to_string(),
    }
}

/// Python `s[:200]` over a `str` — slice by Unicode code points, not bytes (so multibyte chars in a
/// last_summary are never split mid-character).
fn py_slice_200(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- should_restart decision table (pure logic) --------
    #[test]
    fn should_restart_live_crash_restarts() {
        // iterating/sleeping/idle/error (not reverted) all restart when not running/paused/stopping.
        for status in ["iterating", "sleeping", "idle", "error"] {
            assert!(
                should_restart(false, &json!({"status": status}), false, false),
                "status={status} should restart"
            );
        }
    }

    #[test]
    fn should_restart_running_paused_or_stopping_never() {
        let hb = json!({"status": "iterating"});
        assert!(!should_restart(true, &hb, false, false)); // running
        assert!(!should_restart(false, &hb, true, false)); // paused
        assert!(!should_restart(false, &hb, false, true)); // stop pending
    }

    #[test]
    fn should_restart_stopped_or_no_heartbeat_left_alone() {
        // clean exit
        assert!(!should_restart(false, &json!({"status": "stopped"}), false, false));
        // no heartbeat -> {} -> no status -> left alone
        assert!(!should_restart(false, &json!({}), false, false));
        // status missing / null / empty-string -> falsy -> left alone
        assert!(!should_restart(false, &json!({"status": null}), false, false));
        assert!(!should_restart(false, &json!({"status": ""}), false, false));
    }

    #[test]
    fn should_restart_revert_halt_left_alone() {
        // error + phase=reverted -> revert-failure HALT, operator cleanup, not a restart
        assert!(!should_restart(
            false,
            &json!({"status": "error", "phase": "reverted"}),
            false,
            false
        ));
        // error with a non-reverted phase still restarts (transient/retryable)
        assert!(should_restart(
            false,
            &json!({"status": "error", "phase": "preflight"}),
            false,
            false
        ));
    }

    // -------- _auto_push truthiness --------
    #[test]
    fn json_truthy_matches_python_bool() {
        assert!(!json_truthy(&Value::Null));
        assert!(!json_truthy(&json!(false)));
        assert!(json_truthy(&json!(true)));
        assert!(!json_truthy(&json!(0)));
        assert!(json_truthy(&json!(1)));
        assert!(!json_truthy(&json!("")));
        assert!(json_truthy(&json!("x")));
        assert!(!json_truthy(&json!([])));
        assert!(json_truthy(&json!([1])));
        assert!(!json_truthy(&json!({})));
        assert!(json_truthy(&json!({"a": 1})));
    }

    // -------- py_repr f-string rendering --------
    #[test]
    fn py_repr_renders_like_python_fstring() {
        assert_eq!(py_repr(None), "None");
        assert_eq!(py_repr(Some(&Value::Null)), "None");
        assert_eq!(py_repr(Some(&json!("abc"))), "abc");
        assert_eq!(py_repr(Some(&json!(1234))), "1234");
        assert_eq!(py_repr(Some(&json!(true))), "True");
    }

    // -------- py_slice_200 unicode-safe truncation --------
    #[test]
    fn py_slice_200_counts_code_points() {
        let s: String = "é".repeat(250); // 250 chars, 500 bytes
        assert_eq!(py_slice_200(&s).chars().count(), 200);
        assert_eq!(py_slice_200("short").as_str(), "short");
    }

    // -------- _recent_snapshots filtering + windowing --------
    // Exercises the pure parse/filter/window logic against an ISOLATED temp _monitor.jsonl via
    // recent_snapshots_from — fully hermetic (no dependence on the real MON_LOG).
    #[test]
    fn recent_snapshots_filters_by_repo_and_age_and_window() {
        let name = "wd_test_recent_snaps_unique";
        // Hermetic: write to an ISOLATED temp log and read it via recent_snapshots_from — never the
        // real mon_log — so concurrent tests, repeated `cargo test` runs, and the live watchdog can
        // neither bleed records in nor accumulate residue.
        let log = std::env::temp_dir()
            .join(format!("solomon_wd_snaps_{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&log);

        let fresh = now();
        let old = (Utc::now() - chrono::Duration::seconds(99999))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        // Append: two fresh for our name, one old for our name, one fresh for another repo, one junk.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writeln!(f, "{}", json!({"ts": old, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": name, "status": "error", "phase": "preflight"})).unwrap();
        writeln!(f, "{}", json!({"ts": fresh, "repo": "someone_else", "status": "x"})).unwrap();
        writeln!(f, "not json").unwrap();
        drop(f);

        // No age bound: all 3 of our records, last 2.
        assert_eq!(recent_snapshots_from(&log, name, 2, None).len(), 2);

        // Age bound 600s: the old record is dropped; only the 2 fresh remain; take last 2.
        let got = recent_snapshots_from(&log, name, 2, Some(600.0));
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|r| r.get("repo").and_then(Value::as_str) == Some(name)));

        // k larger than available -> all (after age filter).
        assert_eq!(recent_snapshots_from(&log, name, 50, Some(600.0)).len(), 2);

        let _ = std::fs::remove_file(&log);
    }

    // -------- should_auto_rebuild_live: behind/clean/safe predicate (pure) --------
    // The systemic fix for "production apps silently running stale code": the watchdog may rebuild a
    // lane's LIVE production exe ONLY when EVERY safety gate holds. A single false -> no rebuild.
    #[test]
    fn should_auto_rebuild_live_all_gates_hold() {
        // behind + clean tree + on default branch + not mid-iteration + safe window -> rebuild.
        assert!(should_auto_rebuild_live(true, true, true, false, true));
    }

    #[test]
    fn should_auto_rebuild_live_any_single_gate_failing_blocks() {
        // Each gate alone must block the rebuild.
        assert!(!should_auto_rebuild_live(false, true, true, false, true)); // up to date
        assert!(!should_auto_rebuild_live(true, false, true, false, true)); // dirty checkout
        assert!(!should_auto_rebuild_live(true, true, false, false, true)); // not on default branch
        assert!(!should_auto_rebuild_live(true, true, true, true, true)); // mid RSI iteration
        assert!(!should_auto_rebuild_live(true, true, true, false, false)); // unsafe window (book/post)
    }

    #[test]
    fn should_auto_rebuild_live_trading_never_rebuilt_without_safe_signal() {
        // A trading/posting exe with no explicit safe_to_rebuild (safe_window=false) is NEVER rebuilt,
        // even when every other gate holds — real money / real posts are on the line.
        assert!(!should_auto_rebuild_live(true, true, true, false, false));
        // ... but with the safe signal (book flat / outside posting window) it rebuilds.
        assert!(should_auto_rebuild_live(true, true, true, false, true));
    }

    // -------- live_config: a lane is a live-exe lane iff repo["live"] is an object w/ url --------
    #[test]
    fn live_config_requires_object_with_url() {
        assert!(live_config(&json!({})).is_none());
        assert!(live_config(&json!({"live": {}})).is_none()); // object but no url
        assert!(live_config(&json!({"live": {"url": ""}})).is_none()); // empty url
        assert!(live_config(&json!({"live": "http://x"})).is_none()); // not an object
        assert!(live_config(&json!({"live": null})).is_none());
        assert!(live_config(&json!({"live": {"url": "http://127.0.0.1:8000"}})).is_some());
        assert!(live_config(
            &json!({"live": {"url": "http://127.0.0.1:8000", "kind": "trading", "update_path": "/sover/update/apply"}})
        )
        .is_some());
    }

    // -------- parse_http_url / split_http_body: stdlib HTTP plumbing (pure) --------
    #[test]
    fn parse_http_url_vectors() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:8000/health"),
            Some(("127.0.0.1".to_string(), 8000, "/health".to_string()))
        );
        assert_eq!(
            parse_http_url("http://127.0.0.1:8000"),
            Some(("127.0.0.1".to_string(), 8000, "/".to_string()))
        );
        assert_eq!(
            parse_http_url("http://localhost:8080/sover/update/apply"),
            Some(("localhost".to_string(), 8080, "/sover/update/apply".to_string()))
        );
        assert_eq!(
            parse_http_url("http://127.0.0.1/health"),
            Some(("127.0.0.1".to_string(), 80, "/health".to_string()))
        );
        assert_eq!(parse_http_url("https://127.0.0.1:8000"), None); // https unsupported (no TLS dep)
        assert_eq!(parse_http_url("not a url"), None);
        assert_eq!(parse_http_url("http://"), None); // empty authority
    }

    #[test]
    fn split_http_body_finds_header_body_boundary() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"git_sha\":\"abc\"}";
        assert_eq!(split_http_body(resp), br#"{"git_sha":"abc"}"#.to_vec());
        // no body separator -> empty (caller's JSON parse yields None).
        assert_eq!(split_http_body(b"no headers here"), Vec::<u8>::new());
    }

    // -------- read/write_last_rebuilt: the observable "last rebuilt" timestamp --------
    #[test]
    fn last_rebuilt_round_trip() {
        let repo = json!({"name": "wd_test_live_rebuilt_unique"});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        let p = rt.join("live_rebuilt.json");
        let _ = std::fs::remove_file(&p);
        // absent -> None
        assert_eq!(read_last_rebuilt(&repo), None);
        write_last_rebuilt(&repo, "2026-06-30T21:40:00Z");
        assert_eq!(read_last_rebuilt(&repo), Some("2026-06-30T21:40:00Z".to_string()));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- live_exe_sweep surfaces staleness WITHOUT rebuilding when unsafe --------
    // Drives the real live_exe_sweep against a live config whose URL is unreachable (http_get_json
    // returns None) — the lane's `live_behind` surfaces as null and no rebuild is attempted, but the
    // snap gains the observable live_* fields. Confirms the surfacing path never panics and never
    // rebuilds without a proven-behind + safe signal.
    #[test]
    fn live_exe_sweep_surfaces_nulls_when_exe_unreachable_and_does_not_rebuild() {
        let repo = json!({
            "name": "wd_test_live_sweep_unique",
            "path": "",
            "live": {"url": "http://127.0.0.1:1", "kind": "generic"}
        });
        let mut snap = json!({"ts": now(), "repo": "wd_test_live_sweep_unique", "running": false});
        let mut actions: Vec<String> = Vec::new();
        live_exe_sweep(&repo, &mut snap, &mut actions);
        // The live_* fields are surfaced regardless of reachability.
        assert_eq!(snap.get("live_url").and_then(Value::as_str), Some("http://127.0.0.1:1"));
        assert!(snap.get("live_behind").is_some()); // present (null — exe unreachable)
        assert!(snap.get("live_last_rebuilt").is_some()); // present (null)
        // Unreachable exe -> no proven-behind -> no rebuild action.
        assert!(actions.is_empty(), "no rebuild should be attempted when the exe is unreachable: {actions:?}");
    }
}
