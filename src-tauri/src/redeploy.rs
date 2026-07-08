//! Self-redeploy: Solomon updates its OWN production orchestrator binary without a forced
//! mid-iteration kill.
//!
//! On a periodic check (wired into the watchdog sweep — see `watchdog::main`), if the running
//! orchestrator's build sha is behind its remote default branch (`origin/HEAD`, resolved from git —
//! never a hardcoded `main`) AND a rebuild is warranted:
//!   (a) `cargo build --release` into a STAGING path (`target/release/<exe>.new.exe`, NOT the
//!       locked live exe),
//!   (b) wait for a genuine DRAIN WINDOW where NO lane is mid-ship AND no live-money lane has an
//!       open trade,
//!   (c) atomically swap the staged exe in and relaunch, resuming lanes.
//!
//! HARD INVARIANT: never interrupt a lane that is mid-ship or a live-money lane with an open
//! position. If no safe window appears within a bound, surface it LOUDLY in
//! `runtime/_watchdog.out.log` rather than force it.
//!
//! The drain-gate predicate and the lane-state detectors are PURE (no IO) so the safety contract is
//! unit-tested. The build/swap/relaunch are side-effectful and platform-specific; they are
//! defensive — any error aborts the attempt and logs loudly, NEVER forces a swap on an unsafe
//! window. This is the systemic fix for "production apps silently running old code" — the exact
//! reason ship.rs/gitops self-fixes strand in source for hours: iterations run back-to-back and a
//! human/forced mid-iteration kill was the only path (loses in-flight work; unsafe on the
//! live-money asmodeus lane).

#![allow(dead_code)]

use crate::control::{apptest_health, heartbeat, paths, proc, registry};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Heartbeat phases that mean a lane is MID-SHIP — actively pushing / opening a PR / merging.
/// Swapping the orchestrator binary or relaunching during one of these can lose in-flight ship work
/// (a half-pushed branch, an un-merged PR, a CI poll interrupted mid-check). Source: `ship.rs` sets
/// heartbeat phase `"ship"` (pre-push), `"pr"` (opening PR), `"merge"` (CI poll + squash-merge).
const MID_SHIP_PHASES: &[&str] = &["ship", "pr", "merge"];

/// Cooldown between redeploy attempts (minutes). A failed/no-window attempt is not retried more
/// often than this, so the periodic watchdog check does not spin `cargo build` every 2-min sweep.
const COOLDOWN_MINS: i64 = 30;

/// How long to wait for a safe drain window before giving up and surfacing loudly (minutes).
const DRAIN_WAIT_MINS: u64 = 10;

/// Poll interval while waiting for a drain window (seconds).
const DRAIN_POLL_S: u64 = 15;

// --------------------------------------------------------------------------- //
// DRAIN-GATE PREDICATE — pure, the load-bearing safety contract
// --------------------------------------------------------------------------- //

/// The drain-gate predicate: a redeploy swap is SAFE iff NO lane is mid-ship AND no live-money lane
/// has an open trade. Pure — pass the observed lane state in. This IS the hard invariant:
/// never interrupt a lane that is mid-ship or a live-money lane with an open position.
pub fn drain_safe(mid_ship: bool, open_live_trade: bool) -> bool {
    !mid_ship && !open_live_trade
}

/// Lane-collecting variant: safe iff BOTH lists are empty. `mid_ship_lanes` and
/// `open_live_trade_lanes` are the names of the lanes currently in each unsafe state. Pure — the
/// caller collects the state (IO) and passes it in; the predicate is unit-tested over the
/// combinatorial decision table.
pub fn drain_window_safe(mid_ship_lanes: &[String], open_live_trade_lanes: &[String]) -> bool {
    mid_ship_lanes.is_empty() && open_live_trade_lanes.is_empty()
}

// --------------------------------------------------------------------------- //
// LANE-STATE DETECTORS
// --------------------------------------------------------------------------- //

/// True iff the heartbeat's `phase` is a MID-SHIP phase (ship / pr / merge). Pure — takes the
/// heartbeat Value so the predicate is unit-tested without IO. A missing / non-string / empty phase
/// is NOT mid-ship (a lane between iterations or in a non-ship phase is safe to swap under).
pub fn lane_mid_ship(hb: &Value) -> bool {
    let phase = hb.get("phase").and_then(Value::as_str).unwrap_or("");
    MID_SHIP_PHASES.contains(&phase)
}

/// A repo is a LIVE-MONEY lane iff its `live_money` field is truthy (Python truthiness: true /
/// nonzero / non-empty string / non-empty array|object). Opt-in: absent / false / 0 / "" / null ->
/// not live-money. Pure.
pub fn is_live_money(repo: &Value) -> bool {
    json_truthy(repo.get("live_money").unwrap_or(&Value::Null))
}

/// True iff this repo is a LIVE-MONEY lane AND it has an open trade RIGHT NOW — the
/// `runtime/<name>/open_trade` sentinel exists. The live-money lane's own runtime writes that
/// sentinel while it holds an open position and removes it on flat-close, so a MISSING sentinel
/// means "no open position" (safe to swap). A live-money lane with no sentinel is NOT unsafe; a
/// non-live-money lane is never unsafe on this axis regardless of sentinel.
///
/// CONSERVATIVE: a live-money lane whose runtime dir can't be read / whose name can't be resolved
/// is treated as having an open trade (unsafe) — never force a swap on a live-money lane whose
/// state is unknown.
pub fn lane_open_live_trade(repo: &Value) -> bool {
    if !is_live_money(repo) {
        return false;
    }
    match paths::runtime_dir(repo) {
        Some(rt) => {
            // A live-money lane whose runtime dir is MISSING / unreadable is UNKNOWN -> unsafe
            // (never force a swap on a live-money lane whose state we can't observe). Only a
            // present runtime dir WITHOUT the open_trade sentinel is the all-clear.
            if !rt.is_dir() {
                return true;
            }
            match std::fs::metadata(rt.join("open_trade")) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => true, // unreadable sentinel -> unknown -> unsafe
            }
        }
        None => true, // no resolvable name -> unknown -> unsafe
    }
}

/// Collect the drain state over a set of repos: (mid-ship lane names, open-live-trade lane names).
/// Reads heartbeats + runtime sentinels. A repo with no heartbeat is not mid-ship.
pub fn collect_drain_state(repos: &[Value]) -> (Vec<String>, Vec<String>) {
    let mut mid_ship: Vec<String> = Vec::new();
    let mut open_live: Vec<String> = Vec::new();
    for r in repos {
        if !r.is_object() {
            continue;
        }
        let name = paths::repo_name(r);
        if name.is_empty() {
            continue;
        }
        let hb = heartbeat::read_heartbeat(r).unwrap_or_else(|| json!({}));
        if lane_mid_ship(&hb) {
            mid_ship.push(name.clone());
        }
        if lane_open_live_trade(r) {
            open_live.push(name);
        }
    }
    (mid_ship, open_live)
}

/// True iff the current drain window is safe across ALL registered repos (the live caller).
pub fn drain_window_now() -> bool {
    let (ms, olt) = collect_drain_state(&registry::load_repos());
    drain_window_safe(&ms, &olt)
}

// --------------------------------------------------------------------------- //
// BUILD-SHA-BEHIND DETECTION
// --------------------------------------------------------------------------- //

/// True iff Solomon's OWN checkout's HEAD is behind its remote default branch (a rebuild is
/// warranted). The default branch is resolved from git itself (`origin/HEAD`) rather than hardcoded
/// `main`, so this is correct for a `master`-default checkout too (D0 self-honesty). `git fetch
/// origin <default>` (best-effort), then `git rev-list --count HEAD..origin/<default>` > 0.
/// Returns `None` when the repo / git / remote / default branch is unavailable so the caller no-ops
/// (never forces a rebuild on uncertain state).
pub fn build_behind_origin() -> Option<bool> {
    let repo = apptest_health::solomon_repo()?;
    let git = proc::which_git()?;
    let git = git.to_string_lossy().into_owned();
    let repo_s = repo.to_string_lossy().into_owned();
    // Resolve the true remote default (origin/HEAD) — never hardcode main/master. If git can't
    // resolve it (offline / no origin/HEAD), no-op rather than assume a branch that may not exist.
    let default = registry::resolve_default_branch(&repo_s)?;
    let origin_default = format!("origin/{default}");
    // Best-effort fetch — a stale ref makes "behind" a false negative (we no-op), which is safe.
    let _ = proc::run(
        &[git.as_str(), "-C", repo_s.as_str(), "fetch", "origin", default.as_str()],
        None,
        Some(Duration::from_secs(60)),
    );
    let r = proc::run(
        &[
            git.as_str(),
            "-C",
            repo_s.as_str(),
            "rev-list",
            "--count",
            &format!("HEAD..{origin_default}"),
        ],
        None,
        Some(Duration::from_secs(30)),
    )
    .ok()?;
    if r.code != 0 {
        return None;
    }
    let n: i64 = r.stdout.trim().parse::<i64>().ok()?;
    Some(n > 0)
}

// --------------------------------------------------------------------------- //
// STAGING BUILD
// --------------------------------------------------------------------------- //

/// `cargo build --release` in Solomon's own checkout, then copy the freshly-built exe to a STAGING
/// path (`target/release/<exe>.new.exe`) beside it — NOT the locked live exe. Returns the staged
/// path. The build is the expensive part (minutes); it does not interrupt any lane (a cargo build
/// touches neither the live exe nor any lane's runtime).
pub fn stage_build() -> Result<PathBuf, String> {
    let repo = apptest_health::solomon_repo()
        .ok_or_else(|| "solomon repo not located — cannot self-redeploy".to_string())?;
    let cargo = which_cargo().ok_or_else(|| "cargo not found on PATH".to_string())?;
    let cargo_s = cargo.to_string_lossy().into_owned();
    // The Cargo workspace is `src-tauri/`, NOT the repo root — running cargo in the root fails with
    // "could not find Cargo.toml" and the built artifact lands under src-tauri/target, not target.
    // (Bug-bounty cycle 1, conf 97: self-redeploy was permanently inoperative because both the cwd
    // and the artifact path pointed at the repo root.) cwd + paths are the workspace dir.
    let workspace = repo.join("src-tauri");
    // `cargo build --release` in the workspace. Long timeout — a clean release build can take many
    // minutes; an incremental one is fast. A timeout aborts the child and returns Err (no swap).
    let r = proc::run(
        &[cargo_s.as_str(), "build", "--release"],
        Some(&workspace),
        Some(Duration::from_secs(60 * 30)),
    )
    .map_err(|e| format!("cargo build spawn failed: {e}"))?;
    if r.code != 0 {
        let tail = r.stderr.trim();
        let tail: String = tail.chars().take(300).collect();
        return Err(format!("cargo build --release failed (code {}): {tail}", r.code));
    }
    // The built exe: src-tauri/target/release/<default-run>.exe (Cargo.toml default-run = "solomon").
    let exe_name = exe_file_name();
    let built = workspace.join("target").join("release").join(&exe_name);
    if !built.is_file() {
        return Err(format!(
            "build reported success but {} not found",
            built.display()
        ));
    }
    // Staging path: same dir, <exe>.new.exe — never the locked live exe.
    let staged_name = new_exe_name(&exe_name);
    let staged = workspace.join("target").join("release").join(&staged_name);
    std::fs::copy(&built, &staged)
        .map_err(|e| format!("copy {} -> {} failed: {e}", built.display(), staged.display()))?;
    Ok(staged)
}

// --------------------------------------------------------------------------- //
// ATOMIC SWAP + RELAUNCH
// --------------------------------------------------------------------------- //

/// Atomically swap the staged exe in for the live orchestrator exe and relaunch the GUI.
///
/// Sequence (Windows-safe rename-while-running — NTFS allows renaming a file held open by a running
/// process, just not deleting/overwriting it):
///   1. copy staged -> `<live_dir>/<exe>.new.exe` (same volume as the live exe, so the renames are
///      atomic; a cross-volume rename would fail).
///   2. rename `<live>` -> `<live_dir>/<exe>.old.exe` (the old binary is preserved for rollback).
///   3. rename `<live_dir>/<exe>.new.exe` -> `<live>` (the new binary takes the live path).
///   4. spawn `<live>` detached (best-effort GUI relaunch; single-instance may focus an existing
///      window — the new code loads on the next natural GUI restart, and lanes spawned hereafter
///      by the watchdog use the new binary via `self_exe()`).
///
/// The caller MUST have already verified `drain_window_now()` IMMEDIATELY before calling — this
/// function does NOT re-check (the check is the caller's hard invariant). Any IO error aborts and
/// returns Err without leaving the live path empty (the rename of the old exe out happens LAST
/// before the new one is moved in, within a single same-volume dir).
pub fn swap_and_relaunch(staged: &Path) -> Result<(), String> {
    let live = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let live_dir = live
        .parent()
        .ok_or_else(|| "live exe has no parent dir".to_string())?;
    let exe_name = live
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| "live exe name unreadable".to_string())?
        .to_string();
    let staged_copy = live_dir.join(new_exe_name(&exe_name));
    let old = live_dir.join(old_exe_name(&exe_name));

    // 1. copy staged into the live exe's dir (same volume -> the renames are atomic).
    std::fs::copy(staged, &staged_copy)
        .map_err(|e| format!("copy staged into live dir failed: {e}"))?;

    // 2. rename the live exe out of the way (Windows: rename-while-running is allowed).
    //    If a previous .old exists, remove it first (best-effort; a lingering .old from a prior
    //    swap is stale). A failure here aborts BEFORE the live path is touched.
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&live, &old)
        .map_err(|e| format!("rename live -> .old failed: {e}"))?;

    // 3. move the new exe into the live path. If THIS fails, try to restore the old exe so the
    //    live path is never left empty (the operator can still launch Solomon).
    if let Err(e) = std::fs::rename(&staged_copy, &live) {
        let _ = std::fs::rename(&old, &live);
        return Err(format!("rename .new -> live failed (live restored): {e}"));
    }

    // 4. best-effort GUI relaunch — detached, hidden, no args (the GUI path). A failure here does
    //    NOT undo the swap (the new binary is live on disk); it just means the operator restarts.
    let _ = spawn_gui_relaunch(&live);
    Ok(())
}

/// Spawn the new live exe as a detached GUI process (no args). Best-effort — a failure is logged by
/// the caller, not propagated as a swap failure (the binary IS swapped on disk).
fn spawn_gui_relaunch(live: &Path) -> std::io::Result<()> {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(live);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    proc::apply_clean_env(&mut cmd);
    #[cfg(windows)]
    cmd.creation_flags(proc::DETACHED_PROCESS | proc::CREATE_NEW_PROCESS_GROUP | proc::CREATE_NO_WINDOW);
    cmd.spawn()?;
    Ok(())
}

// --------------------------------------------------------------------------- //
// ORCHESTRATOR — the periodic check (wired into watchdog::main)
// --------------------------------------------------------------------------- //

/// The single-flight lock path (`runtime/_redeploy.lock`).
fn lock_path() -> PathBuf {
    paths::here().join("runtime").join("_redeploy.lock")
}

/// The last-attempt timestamp path (`runtime/_redeploy.last`) — the cooldown sentinel.
fn last_path() -> PathBuf {
    paths::here().join("runtime").join("_redeploy.last")
}

/// Append a LOUD line to `runtime/_watchdog.out.log` (the same log the watchdog sweep writes). Used
/// when no safe drain window appears within the bound, or a build/swap fails — the operator must
/// see that production is running old code and why the self-redeploy did not fire.
pub fn log_loud(msg: &str) {
    use std::io::Write;
    let out_log = paths::here().join("runtime").join("_watchdog.out.log");
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = out_log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&out_log)?;
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        writeln!(f, "{ts} REDEPLOY: {msg}")?;
        Ok(())
    })();
}

/// True iff a live process holds the single-flight redeploy lock (so a concurrent watchdog sweep
/// no-ops rather than double-building). A stale lock (dead PID) is ignored.
fn redeploy_in_progress() -> bool {
    let p = lock_path();
    let raw = match std::fs::read_to_string(&p) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let pid = raw.trim().parse::<i64>().unwrap_or(0);
    if pid == 0 {
        return false;
    }
    crate::control::locks::pid_alive(pid)
}

/// Take the single-flight lock (overwrite a stale one). Best-effort — a failure to write does not
/// block the attempt (the worst case is two concurrent builds, and the swap is still gated by the
/// drain window).
fn take_lock() {
    let p = lock_path();
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, std::process::id().to_string())
    })();
}

/// Release the single-flight lock only if WE hold it.
fn release_lock() {
    let p = lock_path();
    let raw = std::fs::read_to_string(&p).unwrap_or_default();
    let pid = raw.trim().parse::<i64>().unwrap_or(0);
    if pid == std::process::id() as i64 {
        let _ = std::fs::remove_file(&p);
    }
}

/// True iff the cooldown has elapsed since the last attempt (or there was no prior attempt).
fn cooldown_elapsed() -> bool {
    let raw = match std::fs::read_to_string(last_path()) {
        Ok(s) => s,
        Err(_) => return true, // no prior attempt -> eligible
    };
    let ts = raw.trim();
    let last = match chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
        Ok(t) => t.and_utc(),
        Err(_) => return true, // unparseable -> eligible
    };
    let age = (chrono::Utc::now() - last).num_seconds();
    age >= COOLDOWN_MINS * 60
}

fn stamp_last() {
    let _ = (|| -> std::io::Result<()> {
        let p = last_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(&p, ts)
    })();
}

/// The periodic self-redeploy check. Wired into `watchdog::main` (every sweep). Cheap when there is
/// nothing to do: the cooldown + single-flight guards mean most sweeps no-op immediately. When the
/// checkout IS behind origin/main, it stages a release build, then waits up to `DRAIN_WAIT_MINS`
/// for a safe drain window; on a safe window it atomically swaps + relaunches; otherwise it surfaces
/// LOUDLY in `_watchdog.out.log` and leaves the staged exe in place for the next eligible attempt.
///
/// NEVER forces a swap on an unsafe window. A build/swap error is logged loudly and the attempt
/// aborts (the live binary is untouched).
pub fn maybe_self_redeploy() {
    if redeploy_in_progress() {
        return; // another sweep is mid-build/swap — do not double-build
    }
    if !cooldown_elapsed() {
        return; // recent attempt — let the next eligible sweep try
    }
    // Not behind origin (or can't tell) -> nothing to do. Do NOT stamp the cooldown for a no-op
    // fetch so a behind-check that flips true right after is acted on promptly.
    match build_behind_origin() {
        Some(false) | None => return,
        Some(true) => {}
    }
    take_lock();
    // Guard: release the lock no matter how we exit this attempt.
    struct LockGuard;
    impl Drop for LockGuard {
        fn drop(&mut self) {
            release_lock();
        }
    }
    let _guard = LockGuard;
    stamp_last();

    // (a) stage a release build into a STAGING path (not the locked live exe).
    let staged = match stage_build() {
        Ok(p) => p,
        Err(e) => {
            log_loud(&format!("staging build FAILED — production remains on the old binary: {e}"));
            return;
        }
    };

    // (b) wait for a genuine DRAIN WINDOW where no lane is mid-ship AND no live-money lane has an
    //     open trade. Re-check right before the swap (the hard invariant).
    let deadline = Instant::now() + Duration::from_secs(DRAIN_WAIT_MINS * 60);
    let mut last_state: Option<(Vec<String>, Vec<String>)> = None;
    while Instant::now() < deadline {
        let (ms, olt) = collect_drain_state(&registry::load_repos());
        if drain_window_safe(&ms, &olt) {
            // (c) atomically swap + relaunch.
            if let Err(e) = swap_and_relaunch(&staged) {
                log_loud(&format!(
                    "drain window was safe but swap FAILED — production remains on the old binary: {e}"
                ));
            } else {
                log_loud("self-redeploy complete: new orchestrator binary swapped in and GUI relaunched");
            }
            return;
        }
        last_state = Some((ms, olt));
        std::thread::sleep(Duration::from_secs(DRAIN_POLL_S));
    }

    // No safe window within the bound — surface LOUDLY, do NOT force.
    let (ms, olt) = last_state.unwrap_or_default();
    let ms_s = if ms.is_empty() { "none".to_string() } else { ms.join(",") };
    let olt_s = if olt.is_empty() { "none".to_string() } else { olt.join(",") };
    log_loud(&format!(
        "NO safe drain window within {DRAIN_WAIT_MINS} min — production remains on the old binary. \
         mid-ship lanes: {ms_s}; live-money lanes with open trades: {olt_s}. \
         A staged build is at {} — swap deferred to the next idle window.",
        staged.display()
    ));
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// Python truthiness for a JSON value (as used by `is_live_money`'s `repo.get("live_money")` check).
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

/// `shutil.which("cargo")` — memoized for the process lifetime (a cargo installed after launch is
/// not picked up until restart — a non-issue for a desktop orchestrator).
fn which_cargo() -> Option<PathBuf> {
    use std::sync::OnceLock;
    static CARGO: OnceLock<Option<PathBuf>> = OnceLock::new();
    CARGO
        .get_or_init(|| which::which("cargo").ok())
        .clone()
}

/// The built release exe file name. Cargo.toml `default-run = "solomon"`; on Windows the release
/// artifact is `solomon.exe`, elsewhere `solomon`.
fn exe_file_name() -> String {
    if cfg!(windows) {
        "solomon.exe".to_string()
    } else {
        "solomon".to_string()
    }
}

/// `<exe>.new.<ext>` — the staging name, e.g. `solomon.new.exe`. Keeps the extension so the OS
/// treats it as the same kind of binary on launch.
fn new_exe_name(exe: &str) -> String {
    insert_suffix(exe, ".new")
}

/// `<exe>.old.<ext>` — the rollback name, e.g. `solomon.old.exe`.
fn old_exe_name(exe: &str) -> String {
    insert_suffix(exe, ".old")
}

/// Insert `suffix` before the final extension of `exe` (`solomon.exe` + `.new` -> `solomon.new.exe`).
/// An exe with no extension just appends the suffix (`solomon` + `.new` -> `solomon.new`).
fn insert_suffix(exe: &str, suffix: &str) -> String {
    match exe.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}{suffix}.{ext}"),
        None => format!("{exe}{suffix}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- drain_safe: the load-bearing hard invariant (combinatorial decision table) --------
    // no-mid-ship AND no-open-live-trade => safe; else unsafe.
    #[test]
    fn drain_safe_decision_table() {
        assert!(drain_safe(false, false), "no mid-ship, no open trade -> SAFE");
        assert!(!drain_safe(true, false), "mid-ship -> unsafe");
        assert!(!drain_safe(false, true), "open live trade -> unsafe");
        assert!(!drain_safe(true, true), "both unsafe -> unsafe");
    }

    #[test]
    fn drain_window_safe_only_when_both_empty() {
        assert!(drain_window_safe(&[], &[]));
        // a mid-ship lane makes it unsafe
        assert!(!drain_window_safe(&["asmodeus".to_string()], &[]));
        // an open live trade makes it unsafe
        assert!(!drain_window_safe(&[], ["asmodeus".to_string()].as_ref()));
        // both
        assert!(!drain_window_safe(
            ["a".to_string(), "b".to_string()].as_ref(),
            ["c".to_string()].as_ref()
        ));
    }

    // -------- lane_mid_ship: phase classifier --------
    #[test]
    fn lane_mid_ship_phases() {
        for phase in ["ship", "pr", "merge"] {
            assert!(lane_mid_ship(&json!({"phase": phase})), "phase={phase} is mid-ship");
        }
        // non-ship phases are NOT mid-ship
        for phase in ["preflight", "plan", "test", "commit", "review", "ideate", "reflect", "implement"] {
            assert!(!lane_mid_ship(&json!({"phase": phase})), "phase={phase} is NOT mid-ship");
        }
        // missing / null / empty / non-string phase -> not mid-ship
        assert!(!lane_mid_ship(&json!({})));
        assert!(!lane_mid_ship(&json!({"phase": null})));
        assert!(!lane_mid_ship(&json!({"phase": ""})));
        assert!(!lane_mid_ship(&json!({"phase": 42})));
    }

    // -------- is_live_money: opt-in truthiness --------
    #[test]
    fn is_live_money_truthiness() {
        // opt-in: truthy values
        assert!(is_live_money(&json!({"live_money": true})));
        assert!(is_live_money(&json!({"live_money": 1})));
        assert!(is_live_money(&json!({"live_money": "yes"})));
        assert!(is_live_money(&json!({"live_money": ["x"]})));
        assert!(is_live_money(&json!({"live_money": {"k": 1}})));
        // not live-money: falsy / absent
        assert!(!is_live_money(&json!({})));
        assert!(!is_live_money(&json!({"live_money": false})));
        assert!(!is_live_money(&json!({"live_money": 0})));
        assert!(!is_live_money(&json!({"live_money": ""})));
        assert!(!is_live_money(&json!({"live_money": null})));
        assert!(!is_live_money(&json!({"live_money": []})));
        assert!(!is_live_money(&json!({"live_money": {}})));
    }

    // -------- lane_open_live_trade: sentinel + conservative-unknown --------
    // A live-money lane with the open_trade sentinel -> open. Without it -> safe. A non-live-money
    // lane is never unsafe on this axis. A live-money lane with an unreadable runtime dir -> unsafe.
    fn tmp_repo(name: &str, live_money: bool) -> (std::path::PathBuf, Value) {
        let repo = json!({ "name": name, "live_money": live_money });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        std::fs::create_dir_all(&rt).unwrap();
        (rt, repo)
    }

    #[test]
    fn lane_open_live_trade_sentinel_logic() {
        // live-money + sentinel present -> open trade
        let (rt, repo) = tmp_repo("rdp_open_live_unique", true);
        std::fs::write(rt.join("open_trade"), "").unwrap();
        assert!(lane_open_live_trade(&repo), "live-money + open_trade sentinel -> open");
        // remove sentinel -> no open trade (safe)
        std::fs::remove_file(rt.join("open_trade")).unwrap();
        assert!(!lane_open_live_trade(&repo), "live-money + NO sentinel -> no open trade");
        let _ = std::fs::remove_dir_all(&rt);

        // non-live-money lane with the sentinel -> NOT unsafe (sentinel is meaningless off a live-money lane)
        let (rt, repo) = tmp_repo("rdp_nonlive_unique", false);
        std::fs::write(rt.join("open_trade"), "").unwrap();
        assert!(!lane_open_live_trade(&repo), "non-live-money + sentinel -> not unsafe on this axis");
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn lane_open_live_trade_unknown_is_unsafe() {
        // A live-money lane with no runtime dir resolvable (empty name) -> unknown -> unsafe.
        // {live_money: true} with no name -> runtime_dir None -> unsafe.
        let repo = json!({"live_money": true});
        assert!(lane_open_live_trade(&repo), "live-money + unresolvable name -> unsafe (do not force)");
        // a live-money lane whose runtime dir was deleted out from under it -> unreadable -> unsafe
        let repo = json!({"name": "rdp_gone_dir_unique", "live_money": true});
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);
        assert!(lane_open_live_trade(&repo), "live-money + missing runtime dir -> unsafe (do not force)");
    }

    // -------- collect_drain_state: end-to-end over synthetic repos with temp runtime --------
    #[test]
    fn collect_drain_state_classifies_lanes() {
        // lane A: mid-ship (phase "merge") + live-money WITH open trade
        let (rt_a, repo_a) = tmp_repo("rdp_collect_a_unique", true);
        std::fs::write(
            rt_a.join("heartbeat.json"),
            r#"{"status":"iterating","phase":"merge"}"#,
        )
        .unwrap();
        std::fs::write(rt_a.join("open_trade"), "").unwrap();
        // lane B: mid-ship (phase "pr"), NOT live-money
        let (rt_b, repo_b) = tmp_repo("rdp_collect_b_unique", false);
        std::fs::write(
            rt_b.join("heartbeat.json"),
            r#"{"status":"iterating","phase":"pr"}"#,
        )
        .unwrap();
        // lane C: safe (phase "preflight"), live-money WITHOUT open trade
        let (rt_c, repo_c) = tmp_repo("rdp_collect_c_unique", true);
        std::fs::write(
            rt_c.join("heartbeat.json"),
            r#"{"status":"iterating","phase":"preflight"}"#,
        )
        .unwrap();
        // lane D: safe (no heartbeat), not live-money
        let (_rt_d, repo_d) = tmp_repo("rdp_collect_d_unique", false);

        let repos = vec![repo_a, repo_b, repo_c, repo_d];
        let (ms, olt) = collect_drain_state(&repos);
        // mid-ship: A (merge) and B (pr)
        assert!(ms.contains(&"rdp_collect_a_unique".to_string()), "A is mid-ship");
        assert!(ms.contains(&"rdp_collect_b_unique".to_string()), "B is mid-ship");
        assert!(!ms.contains(&"rdp_collect_c_unique".to_string()), "C is NOT mid-ship");
        // open live trade: A only (C is live-money but no sentinel; B/D are not live-money)
        assert!(olt.contains(&"rdp_collect_a_unique".to_string()), "A has open live trade");
        assert!(!olt.contains(&"rdp_collect_c_unique".to_string()), "C is live-money but flat");
        // the window is unsafe (A is both mid-ship and open-live-trade)
        assert!(!drain_window_safe(&ms, &olt));

        // clean up
        for name in ["rdp_collect_a_unique", "rdp_collect_b_unique", "rdp_collect_c_unique", "rdp_collect_d_unique"] {
            let repo = json!({ "name": name });
            if let Some(rt) = paths::runtime_dir(&repo) {
                let _ = std::fs::remove_dir_all(&rt);
            }
        }
    }

    // -------- exe-name suffix helpers --------
    #[test]
    fn new_and_old_exe_names_keep_extension() {
        assert_eq!(new_exe_name("solomon.exe"), "solomon.new.exe");
        assert_eq!(old_exe_name("solomon.exe"), "solomon.old.exe");
        // no extension -> suffix appended
        assert_eq!(new_exe_name("solomon"), "solomon.new");
        assert_eq!(old_exe_name("solomon"), "solomon.old");
    }
}