//! HOST-INDEPENDENT LIVENESS FLOOR (failure catalog #4 — "liveness chained to a fragile host;
//! nothing resurrects the dead"). A dead-man tripwire whose ONLY job is: read the engine-host
//! heartbeat file → if it is STALE beyond a threshold AND the recorded host process is gone,
//! relaunch the engine host (`Solomon.exe`, no subcommand → `run_gui`) and page the operator ONCE
//! (marker-deduped, honoring the alert-fatigue countermeasure). It holds NO authority: it reads two
//! files + the process table and spawns the GUI host. It CANNOT trade, cannot touch a whitelist,
//! budget, KILL/blast-radius/breaker/skeptic/freshness gate, or any lane's git tree.
//!
//! # Why this is not already covered
//! The Solomon Sentinel scheduled task (`tools/install_sentinel.ps1` → `solomon watchdog`, every
//! 5 min, out-of-band; the "no scheduled tasks" rule was renegotiated by the operator 2026-07-06
//! after the liveness autopsy) keeps the SWEEP alive and restarts crashed per-lane improver LOOPS,
//! and `watchdog::standstill_alarm` PAGES once when the fleet is entirely down. But NOTHING
//! relaunches the GUI/CEO **host** (`Solomon.exe`) when it dies — and the CEO rhythm (morning plan /
//! allocate / scale) + the in-app 2-min tick only run inside that host. So a closed/crashed host is
//! a standing blind window that the sentinel keeps *paging* about but never *heals*. That is the
//! residual of catalog #4 (the asmodeus "shell closed Jul 4, everything dead 2+ days" class). This
//! module is the missing ACTUATION: detection → resurrection, not detection → page-forever.
//!
//! # The engine heartbeat this reads (NOT the sentinel dead-man record)
//! `_sentinel_heartbeat.json` is stamped by EVERY watchdog run — the GUI tick AND the out-of-band
//! sentinel — so it cannot distinguish "host alive" from "only the sentinel ran". A resurrector that
//! keyed off it would think the host was alive whenever the sentinel had just run. So the GUI host
//! writes its OWN marker, `runtime/_engine_heartbeat.json` = `{"ts","pid"}`, from a DEDICATED
//! lightweight heartbeat thread in `run_gui` — on a fixed short cadence (`HEARTBEAT_STAMP_INTERVAL_S`),
//! decoupled from the potentially-slow `watchdog::main()` sweep — so freshness tracks host-process
//! liveness, not sweep-completion time (see `stamp_engine_heartbeat`, called only on the GUI path).
//! The out-of-band sentinel process does NOT write it. Staleness of THIS file + a dead recorded PID
//! is a true "the host is gone" signal a bare sweep cannot forge.
//!
//! # Where it runs
//! Wired into `watchdog::main` behind `catch_unwind` (like the other grafts), so it runs on the ONE
//! path guaranteed to fire even when the host is dead: the out-of-band Solomon Sentinel sweep. The
//! OS-level scheduled task ALREADY EXISTS (installed + verified live 2026-07-06); this module adds
//! NO install step and modifies NO task. If the host is up, its 2-min tick's own resurrector call is
//! a cheap no-op (heartbeat fresh → Wait).
#![allow(dead_code)]

use crate::control::{paths, proc};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};
use std::path::PathBuf;

/// STALE_AFTER_S — the engine-host liveness threshold `T`. The GUI host stamps its heartbeat every
/// `HEARTBEAT_STAMP_INTERVAL_S` (30 s) from a DEDICATED thread decoupled from the sweep, so `T` at
/// 360 s means ~12 consecutive missed stamps — unambiguous host death, not a slow watchdog sweep
/// (the stamp thread never runs the sweep, so a sweep that blocks on fleet-wide git subprocesses can
/// no longer age a live host past `T`). It is still well inside the sentinel's 5-min out-of-band
/// cadence so the FIRST or SECOND sentinel sweep after a host death resurrects it — a bounded blind
/// window measured in minutes, replacing the multi-DAY outages of catalog #4. A floor, not the
/// pacing: do not lengthen it casually.
pub const STALE_AFTER_S: f64 = 360.0;

/// HEARTBEAT_STAMP_INTERVAL_S — cadence of the DEDICATED engine-heartbeat thread in `run_gui`. The
/// heartbeat is stamped from its own lightweight thread on THIS fixed short interval, independent of
/// the potentially-slow `watchdog::main()` sweep, so heartbeat freshness tracks host-process liveness
/// rather than sweep-completion time. Must stay well below `STALE_AFTER_S` (30 s vs 360 s = 12×
/// headroom) so a healthy host is never mistaken for stale even if several stamps are missed; the
/// `heartbeat_interval_is_well_below_staleness_threshold` test pins that invariant.
pub const HEARTBEAT_STAMP_INTERVAL_S: u64 = 30;

/// SPAWN_COOLDOWN_S — after a relaunch, suppress further respawns for this long. A freshly-launched
/// host publishes its first heartbeat quickly now (its dedicated heartbeat thread stamps on startup,
/// within `HEARTBEAT_STAMP_INTERVAL_S`), but until that first stamp lands the OLD (dead) pid's stale
/// heartbeat is still on disk; without a cooldown a sentinel sweep that races that window would see
/// the stale record + the dead pid and spawn a REDUNDANT second host. The `tauri_plugin_single_instance`
/// guard already makes a duplicate launch harmless (it focuses the existing window and exits), so
/// this is belt-and-suspenders: it keeps the log clean and prevents a pathological respawn-every-sweep
/// if a relaunch keeps failing. 240 s is comfortably longer than a healthy new host needs to publish
/// its first fresh heartbeat and flip the decision to Wait naturally.
pub const SPAWN_COOLDOWN_S: f64 = 240.0;

/// The engine-host dead-man heartbeat: `runtime/_engine_heartbeat.json`. Written ONLY by the GUI
/// host (`run_gui`), never by the out-of-band sentinel process — that asymmetry is what makes a
/// stale value a true "the host is gone" signal.
fn engine_heartbeat_path() -> PathBuf {
    paths::here().join("runtime").join("_engine_heartbeat.json")
}

/// The last-relaunch-attempt stamp: `runtime/_resurrector.last`. Its mtime gates the spawn cooldown
/// (see `SPAWN_COOLDOWN_S`) so a just-relaunched host is given time to publish its first heartbeat
/// before another respawn is considered.
fn last_spawn_path() -> PathBuf {
    paths::here().join("runtime").join("_resurrector.last")
}

/// The paged-once dedupe marker: `runtime/_resurrector.marker`. Present == the operator has already
/// been paged for the CURRENT death episode; removed the moment a fresh heartbeat proves the host is
/// back, re-arming the alarm. Same log-once contract as `_standstill.marker`.
fn marker_path() -> PathBuf {
    paths::here().join("runtime").join("_resurrector.marker")
}

/// `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")` — the one timestamp format every
/// runtime file in this tree uses.
fn now_str(now: DateTime<Utc>) -> String {
    now.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Stamp the engine-host heartbeat: `{"ts": <now>, "pid": <this process>}`. Called ONLY from the GUI
/// host's dedicated heartbeat thread (`run_gui`) — the headless subcommands (`watchdog`,
/// `run-improver`, the sentinel) MUST NOT call it, or a bare sweep would forge host-liveness.
/// Best-effort (atomic
/// tmp+rename via `proc::write_json_atomic`); an IO error is swallowed exactly like every other
/// runtime stamp — a failed heartbeat write must never crash the GUI.
pub fn stamp_engine_heartbeat() {
    let rec = json!({ "ts": now_str(Utc::now()), "pid": std::process::id() });
    let p = engine_heartbeat_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Atomic write (tmp + os.replace) so a concurrent read by the sentinel never sees a torn file.
    // Fall back to a plain write if the atomic path fails for any reason; either way, swallow errors —
    // a failed heartbeat stamp must never crash the GUI.
    let _ = proc::atomic_write_json(&p, &rec)
        .or_else(|_| std::fs::write(&p, serde_json::to_string(&rec).unwrap_or_default()));
}

/// The parsed engine heartbeat, or `None` on missing / corrupt / non-object file. Mirrors
/// `heartbeat::read_heartbeat`'s "a non-object reads as None so a non-dict never leaks" contract.
fn read_engine_heartbeat() -> Option<Value> {
    let data = std::fs::read_to_string(engine_heartbeat_path()).ok()?;
    let v: Value = serde_json::from_str(&data).ok()?;
    if v.is_object() {
        Some(v)
    } else {
        None
    }
}

/// Age (seconds) of a heartbeat record's `ts`, relative to `now`. `None` when `ts` is absent /
/// empty / non-string / unparseable (the strict `%Y-%m-%dT%H:%M:%SZ` format — no fractional seconds,
/// no offset), matching `heartbeat::heartbeat_age`. A future ts yields a negative age (never
/// clamped) so a clock-skew anomaly is visible rather than silently read as "fresh".
fn heartbeat_age_s(hb: &Value, now: DateTime<Utc>) -> Option<f64> {
    let ts = match hb.get("ts") {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        _ => return None,
    };
    let last = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ").ok()?;
    Some((now - last.and_utc()).num_milliseconds() as f64 / 1000.0)
}

/// The recorded host PID from a heartbeat record, or 0 (a never-alive pid) when absent / non-integer.
fn heartbeat_pid(hb: &Value) -> i64 {
    hb.get("pid").and_then(Value::as_i64).unwrap_or(0)
}

/// What the resurrector should do this sweep. Pure — the whole decision is a function of the observed
/// facts, so it is exhaustively unit-tested from synthetic inputs with no IO.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// The host is alive (fresh heartbeat, or a stale heartbeat whose PID is still alive — a hung
    /// but not dead host, which we must NOT double-spawn) → do nothing this sweep.
    Wait,
    /// The host is confirmed dead (heartbeat stale beyond `T` AND its PID is gone) → relaunch it.
    /// `page` is true on the FIRST restart of an episode (send one deduped page) and false on a
    /// subsequent restart while the same episode's marker is still set (heal again, but stay silent —
    /// the alert-fatigue countermeasure: one page per death episode, not a page-storm).
    Restart { page: bool },
}

/// THE DECISION (pure, unit-tested). Inputs are the observed facts; there is no IO here so the
/// restart-vs-wait logic is driven directly from synthetic values (and, in the integration test,
/// from a synthetic heartbeat FILE via the reader above).
///
/// * `hb_present` — was `_engine_heartbeat.json` readable as an object at all? A MISSING heartbeat is
///   NOT treated as death: on a fresh install / wiped runtime the host may simply not have stamped
///   yet, and a resurrector that spawned a GUI on every empty-runtime sentinel sweep would fight a
///   headless server or a deliberately-closed host forever. Absent heartbeat → `Wait` (the standstill
///   alarm still pages on a genuinely dark fleet; resurrection requires a POSITIVE death signal).
/// * `age_s` — heartbeat age in seconds (`None` when unparseable → treated as NOT-stale → `Wait`,
///   the same fail-safe as `heartbeat::_stale`: an unreadable timestamp never triggers a destructive
///   action).
/// * `pid_alive` — is the recorded host PID currently a live process? A stale heartbeat whose PID is
///   ALIVE is a HUNG host, not a dead one; spawning a second GUI alongside it is the "two processes"
///   hazard the lock layer guards against, so a live PID → `Wait` regardless of age.
/// * `threshold_s` — `T` (`STALE_AFTER_S` in production; a test value in tests).
/// * `already_paged` — is the episode dedupe marker already set? Gates the page, never the restart:
///   a persisting death still gets re-healed every sweep (the whole point), but paged only once.
/// * `in_spawn_cooldown` — did a relaunch fire within `SPAWN_COOLDOWN_S`? If so, a just-launched host
///   has not yet had time to publish its first heartbeat, so the still-stale record is not yet proof
///   of a SECOND death — hold off (`Wait`) rather than respawn redundantly. The page is not re-sent
///   during cooldown either (the episode marker is still set from the first page).
pub fn decide(
    hb_present: bool,
    age_s: Option<f64>,
    pid_alive: bool,
    threshold_s: f64,
    already_paged: bool,
    in_spawn_cooldown: bool,
) -> Action {
    if !hb_present {
        return Action::Wait; // no positive death signal — never resurrect on a missing heartbeat
    }
    let stale = match age_s {
        Some(a) => a > threshold_s,
        None => return Action::Wait, // unparseable ts → fail safe, treat as fresh
    };
    if !stale {
        return Action::Wait; // fresh host tick — alive
    }
    if pid_alive {
        return Action::Wait; // stale heartbeat but PID alive → hung, not dead; do NOT double-spawn
    }
    if in_spawn_cooldown {
        // A relaunch just fired; the new host has not published its first heartbeat yet. The stale
        // record on disk is the OLD dead host's — not proof of a fresh death. Wait one cooldown out.
        return Action::Wait;
    }
    // Confirmed dead: stale beyond T AND the recorded host PID is gone AND not inside a post-relaunch
    // cooldown.
    Action::Restart {
        page: !already_paged,
    }
}

/// Spawn the engine host as a detached, hidden GUI process (no args → `run_gui`). Byte-for-byte the
/// same detached/clean-env/no-window pattern as `redeploy::spawn_gui_relaunch` (the existing,
/// audited GUI-relaunch site), so the resurrected host is indistinguishable from an operator
/// double-click or a self-redeploy relaunch. Best-effort: a spawn error is returned for the caller
/// to log, never panicked. This is the ENTIRE mutating surface of the module — a process spawn of
/// the SELF binary with no subcommand; no trade/whitelist/budget/git capability is reachable from
/// here.
fn spawn_engine_host() -> std::io::Result<u32> {
    use std::process::{Command, Stdio};
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(&exe);
    // NO args — the bare exe is the GUI host path (main.rs: an empty argv → run_gui()).
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    proc::apply_clean_env(&mut cmd);
    #[cfg(windows)]
    cmd.creation_flags(proc::DETACHED_PROCESS | proc::CREATE_NEW_PROCESS_GROUP | proc::CREATE_NO_WINDOW);
    let child = cmd.spawn()?;
    Ok(child.id())
}

/// Append a LOUD line to `runtime/_watchdog.out.log` (the same log the sweep writes) so a
/// resurrection — or a failed one — is visible on disk even if the page never reaches the phone.
fn log_loud(msg: &str) {
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
        writeln!(f, "{msg}")
    })();
}

/// THE ACTUATOR (side-effectful, wired into `watchdog::main`). Reads the engine heartbeat, runs the
/// pure `decide`, and on `Restart` spawns the host + (on the first restart of an episode) pages once,
/// deduped by `_resurrector.marker`. On a healthy/hung host it is a cheap no-op. Never panics, never
/// fails a sweep — every IO is best-effort, matching the `notify::send` / standstill-alarm contract.
///
/// The `page` closure and `spawn`/`pid_alive`/`now` are the real production dependencies; `run_with`
/// below is the injectable core so the full restart-and-page-once flow is tested against a synthetic
/// heartbeat file with NO real process spawn and NO real page.
pub fn run() {
    let now = Utc::now();
    run_with(
        now,
        // pid_alive: the real process-table probe (tasklist / kill(0)).
        &|pid| crate::control::locks::pid_alive(pid),
        // spawn: the real detached GUI-host spawn; map to the {ok,pid?/error} the core records.
        &|| match spawn_engine_host() {
            Ok(pid) => (true, Some(pid), None),
            Err(e) => (false, None, Some(e.to_string())),
        },
        // page: the real operator page (urgent, red).
        &|title, body| {
            let _ = crate::notify::send(&crate::notify::Notice::red(title.to_string(), body.to_string()));
        },
    );
}

/// Injectable core of `run()`: all IO except the two runtime files (heartbeat read + marker) is
/// passed in, so a test drives the whole restart-and-page-once flow deterministically from a
/// synthetic `_engine_heartbeat.json` without spawning a process or sending a page.
///
/// `spawn` returns `(ok, pid, error)`; `page(title, body)` sends one operator notice; `pid_alive`
/// probes the process table. The heartbeat file and the dedupe marker ARE touched (they are the
/// module's own state) — tests point `paths::here()` at an isolated dir.
fn run_with(
    now: DateTime<Utc>,
    pid_alive: &dyn Fn(i64) -> bool,
    spawn: &dyn Fn() -> (bool, Option<u32>, Option<String>),
    page: &dyn Fn(&str, &str),
) {
    let hb = read_engine_heartbeat();
    let hb_present = hb.is_some();
    let hb = hb.unwrap_or_else(|| json!({}));
    let age = heartbeat_age_s(&hb, now);
    let recorded_pid = heartbeat_pid(&hb);
    let alive = recorded_pid != 0 && pid_alive(recorded_pid);
    let marker = marker_path();
    let already_paged = marker.exists();
    // Post-relaunch cooldown: the mtime-age of the last-spawn stamp, below SPAWN_COOLDOWN_S == still
    // cooling down. mtime (not a parsed ts) keeps this cheap and needs no clock in the file.
    let in_cooldown = std::fs::metadata(last_spawn_path())
        .and_then(|m| m.modified())
        .and_then(|t| t.elapsed().map_err(std::io::Error::other))
        .map(|d| d.as_secs_f64() < SPAWN_COOLDOWN_S)
        .unwrap_or(false);

    match decide(hb_present, age, alive, STALE_AFTER_S, already_paged, in_cooldown) {
        Action::Wait => {
            // Host alive (or no positive death signal): clear the dedupe marker the moment a fresh
            // heartbeat proves recovery, re-arming the page for the next death episode. Only clear on
            // a genuinely fresh host (present + not stale), NOT on the missing/unparseable Wait paths —
            // otherwise a transiently-unreadable heartbeat would silently disarm a standing episode.
            let fresh = hb_present && matches!(age, Some(a) if a <= STALE_AFTER_S);
            if fresh {
                let _ = std::fs::remove_file(&marker);
            }
        }
        Action::Restart { page: do_page } => {
            let age_h = age.map(|a| a / 3600.0).unwrap_or(0.0);
            let (ok, pid, err) = spawn();
            // Stamp the last-spawn time REGARDLESS of ok: a failed relaunch must also cool down so a
            // persistently-unlaunchable host isn't respawn-hammered every sweep (it stays surfaced via
            // the loud log + the one page). Best-effort; an IO error just shortens the cooldown.
            let _ = (|| -> std::io::Result<()> {
                let p = last_spawn_path();
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, now_str(now))
            })();
            if ok {
                log_loud(&format!(
                    "{} resurrector: engine host DEAD (heartbeat stale {:.2} h, pid {} gone) — relaunched Solomon.exe (pid {})",
                    now_str(now),
                    age_h,
                    recorded_pid,
                    pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into()),
                ));
            } else {
                log_loud(&format!(
                    "{} resurrector: engine host DEAD (heartbeat stale {:.2} h) — relaunch FAILED: {}",
                    now_str(now),
                    age_h,
                    err.as_deref().unwrap_or("unknown error"),
                ));
            }
            if do_page {
                // One page per death episode (marker-deduped) — the alert-fatigue countermeasure.
                let body = if ok {
                    format!(
                        "Engine host was DEAD (no heartbeat for {age_h:.1} h) — the liveness floor relaunched Solomon.exe automatically. No action needed unless it recurs."
                    )
                } else {
                    format!(
                        "Engine host is DEAD (no heartbeat for {age_h:.1} h) and the automatic relaunch FAILED: {}. Manual restart of Solomon.exe required.",
                        err.as_deref().unwrap_or("unknown error")
                    )
                };
                page("Solomon: engine host resurrected", &body);
                // Arm the dedupe marker (best-effort) so a persisting death re-heals but pages once.
                let _ = (|| -> std::io::Result<()> {
                    if let Some(parent) = marker.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&marker, now_str(now))
                })();
            }
        }
    }
}

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    // The core touches paths::here()/runtime for the heartbeat + marker; those integration tests
    // share process-global CWD state, so serialize them. The PURE decide() tests need no lock.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    // ---------------- pure decision table (no IO) ----------------
    // Signature: decide(hb_present, age_s, pid_alive, threshold_s, already_paged, in_spawn_cooldown).

    #[test]
    fn decide_fresh_host_waits() {
        // Fresh heartbeat (age below T), PID irrelevant → Wait.
        assert_eq!(decide(true, Some(10.0), false, 360.0, false, false), Action::Wait);
        assert_eq!(decide(true, Some(10.0), true, 360.0, false, false), Action::Wait);
        // Exactly AT the threshold is not yet stale (strict `>`).
        assert_eq!(decide(true, Some(360.0), false, 360.0, false, false), Action::Wait);
    }

    #[test]
    fn decide_stale_and_pid_gone_restarts_and_pages_once() {
        // Stale beyond T AND recorded PID gone AND not cooling down → confirmed-dead resurrection,
        // page on the first.
        assert_eq!(
            decide(true, Some(1000.0), false, 360.0, false, false),
            Action::Restart { page: true }
        );
        // Same death, marker already set → still restart, but SILENT (one page per episode).
        assert_eq!(
            decide(true, Some(1000.0), false, 360.0, true, false),
            Action::Restart { page: false }
        );
    }

    #[test]
    fn decide_stale_but_pid_alive_waits() {
        // A HUNG host (stale heartbeat, PID still alive) must NOT be double-spawned.
        assert_eq!(decide(true, Some(99999.0), true, 360.0, false, false), Action::Wait);
    }

    #[test]
    fn decide_stale_dead_but_in_cooldown_waits() {
        // Stale + dead pid, but a relaunch fired within SPAWN_COOLDOWN_S → the new host hasn't
        // published its first heartbeat yet; the stale record is the OLD host's, not a fresh death.
        // Hold off — no redundant respawn.
        assert_eq!(decide(true, Some(1000.0), false, 360.0, false, true), Action::Wait);
        // Even with the episode already paged, the cooldown still suppresses the respawn.
        assert_eq!(decide(true, Some(1000.0), false, 360.0, true, true), Action::Wait);
    }

    #[test]
    fn decide_missing_or_unparseable_heartbeat_waits() {
        // Missing heartbeat is NOT death (fresh install / deliberately-closed host).
        assert_eq!(decide(false, None, false, 360.0, false, false), Action::Wait);
        // Present but unparseable ts (age None) → fail safe → Wait.
        assert_eq!(decide(true, None, false, 360.0, false, false), Action::Wait);
    }

    #[test]
    fn decide_negative_age_future_ts_waits() {
        // A future ts (clock skew) yields a negative age → not stale → Wait (never resurrect on skew).
        assert_eq!(decide(true, Some(-500.0), false, 360.0, false, false), Action::Wait);
    }

    #[test]
    fn heartbeat_interval_is_well_below_staleness_threshold() {
        // The dedicated heartbeat thread stamps every HEARTBEAT_STAMP_INTERVAL_S, decoupled from the
        // slow watchdog sweep. That interval MUST leave several missed stamps of headroom under
        // STALE_AFTER_S, or a couple of skipped stamps on a busy host would forge a false-death.
        // Encode the invariant: at least 4 consecutive stamps must fit inside the staleness window.
        assert!(
            (HEARTBEAT_STAMP_INTERVAL_S as f64) * 4.0 <= STALE_AFTER_S,
            "interval {HEARTBEAT_STAMP_INTERVAL_S}s * 4 must be <= STALE_AFTER_S {STALE_AFTER_S}s so several missed stamps never look dead"
        );
        // And it must be a positive, sane short cadence (30–60 s per the design).
        assert!(
            (1..=60).contains(&HEARTBEAT_STAMP_INTERVAL_S),
            "cadence {HEARTBEAT_STAMP_INTERVAL_S}s must be a short 1–60 s interval"
        );
    }

    // ---------------- heartbeat file reader / age / pid helpers ----------------

    #[test]
    fn read_and_age_from_synthetic_file() {
        let _g = env_lock();
        let dir = paths::here().join("runtime");
        let _ = std::fs::create_dir_all(&dir);
        let p = engine_heartbeat_path();

        // A stale heartbeat 2 h in the past with a never-alive pid.
        let now = Utc::now();
        let past = (now - chrono::Duration::seconds(7200))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(&p, json!({ "ts": past, "pid": 2147483646i64 }).to_string()).unwrap();
        let hb = read_engine_heartbeat().expect("object heartbeat");
        let age = heartbeat_age_s(&hb, now).expect("parseable ts");
        assert!((age - 7200.0).abs() < 5.0, "age was {age}");
        assert_eq!(heartbeat_pid(&hb), 2147483646);

        // Non-object JSON → None (a non-dict never leaks to callers).
        std::fs::write(&p, "42").unwrap();
        assert!(read_engine_heartbeat().is_none());

        // Corrupt JSON → None.
        std::fs::write(&p, "{not json").unwrap();
        assert!(read_engine_heartbeat().is_none());

        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stamp_writes_fresh_this_pid_heartbeat_that_reads_non_stale() {
        // The dedicated heartbeat thread's whole job is one call to stamp_engine_heartbeat() per
        // interval. Prove a SINGLE stamp writes an object heartbeat that reads back as this process,
        // aged ~0, and comfortably inside the staleness window — i.e. one stamp keeps the host live
        // regardless of what the (uncalled here) watchdog sweep is doing.
        let _g = env_lock();
        let dir = paths::here().join("runtime");
        let _ = std::fs::create_dir_all(&dir);
        let p = engine_heartbeat_path();
        let _ = std::fs::remove_file(&p);

        stamp_engine_heartbeat();

        let hb = read_engine_heartbeat().expect("stamp wrote an object heartbeat");
        assert_eq!(
            heartbeat_pid(&hb),
            std::process::id() as i64,
            "heartbeat records THIS process pid"
        );
        let age = heartbeat_age_s(&hb, Utc::now()).expect("stamped ts is parseable");
        assert!((0.0..5.0).contains(&age), "a just-stamped heartbeat is fresh, age was {age}");
        assert!(
            age < STALE_AFTER_S,
            "a single fresh stamp is well inside the staleness window"
        );

        let _ = std::fs::remove_file(&p);
    }

    // ---------------- full actuator flow against a synthetic file (stale => restart) ----------------

    #[test]
    fn run_with_stale_dead_host_restarts_and_pages_exactly_once() {
        let _g = env_lock();
        let dir = paths::here().join("runtime");
        let _ = std::fs::create_dir_all(&dir);
        let hb_path = engine_heartbeat_path();
        let marker = marker_path();
        let last = last_spawn_path();
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&last);

        // Synthetic STALE heartbeat (2 h old) with a dead pid.
        let now = Utc::now();
        let past = (now - chrono::Duration::seconds(7200))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(&hb_path, json!({ "ts": past, "pid": 424242 }).to_string()).unwrap();

        let spawns = std::cell::Cell::new(0usize);
        let pages = std::cell::Cell::new(0usize);
        let spawn = || {
            spawns.set(spawns.get() + 1);
            (true, Some(9999u32), None)
        };
        let page = |_t: &str, _b: &str| pages.set(pages.get() + 1);
        let pid_dead = |_pid: i64| false; // recorded pid is gone → confirmed dead

        // Sweep 1: stale + dead, no cooldown → restart + page. Arms both the dedupe marker AND the
        // spawn-cooldown stamp.
        run_with(now, &pid_dead, &spawn, &page);
        assert_eq!(spawns.get(), 1, "should have spawned the host once");
        assert_eq!(pages.get(), 1, "should have paged once");
        assert!(marker.exists(), "dedupe marker armed after the first page");
        assert!(last.exists(), "spawn-cooldown stamp armed after the relaunch");

        // Sweep 2 immediately after: still stale + dead, but INSIDE the spawn cooldown → Wait (no
        // redundant respawn, no page). This is the belt-and-suspenders against a respawn storm while
        // the new host publishes its first heartbeat.
        run_with(now, &pid_dead, &spawn, &page);
        assert_eq!(spawns.get(), 1, "cooldown suppresses the redundant respawn");
        assert_eq!(pages.get(), 1, "and no second page");

        // Sweep 3 after the cooldown has passed (simulate by clearing the stamp): the death still
        // persists (host never came back) → re-heal (spawn) but STAY SILENT (marker still set — one
        // page per episode, no storm).
        let _ = std::fs::remove_file(&last);
        run_with(now, &pid_dead, &spawn, &page);
        assert_eq!(spawns.get(), 2, "past cooldown, re-heals the persisting death");
        assert_eq!(pages.get(), 1, "but pages only ONCE per episode — no storm");

        let _ = std::fs::remove_file(&hb_path);
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(&last);
    }

    #[test]
    fn run_with_fresh_host_waits_and_clears_marker() {
        let _g = env_lock();
        let dir = paths::here().join("runtime");
        let _ = std::fs::create_dir_all(&dir);
        let hb_path = engine_heartbeat_path();
        let marker = marker_path();

        // Leave a stale marker from a prior episode, then present a FRESH heartbeat.
        let _ = std::fs::remove_file(last_spawn_path());
        std::fs::write(&marker, "prior-episode").unwrap();
        let now = Utc::now();
        let fresh = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(&hb_path, json!({ "ts": fresh, "pid": 1 }).to_string()).unwrap();

        let spawns = std::cell::Cell::new(0usize);
        let pages = std::cell::Cell::new(0usize);
        let spawn = || {
            spawns.set(spawns.get() + 1);
            (true, Some(1u32), None)
        };
        let page = |_t: &str, _b: &str| pages.set(pages.get() + 1);
        let pid_alive = |_pid: i64| true;

        run_with(now, &pid_alive, &spawn, &page);
        assert_eq!(spawns.get(), 0, "a fresh host is never respawned");
        assert_eq!(pages.get(), 0, "a fresh host never pages");
        assert!(
            !marker.exists(),
            "recovery clears the dedupe marker, re-arming the next episode"
        );

        let _ = std::fs::remove_file(&hb_path);
    }

    #[test]
    fn run_with_stale_but_pid_alive_hung_host_waits() {
        let _g = env_lock();
        let dir = paths::here().join("runtime");
        let _ = std::fs::create_dir_all(&dir);
        let hb_path = engine_heartbeat_path();
        let marker = marker_path();
        let _ = std::fs::remove_file(&marker);
        let _ = std::fs::remove_file(last_spawn_path());

        // Stale heartbeat, but the recorded PID is ALIVE → hung host, must not double-spawn.
        let now = Utc::now();
        let past = (now - chrono::Duration::seconds(7200))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        std::fs::write(&hb_path, json!({ "ts": past, "pid": 555 }).to_string()).unwrap();

        let spawns = std::cell::Cell::new(0usize);
        let spawn = || {
            spawns.set(spawns.get() + 1);
            (true, Some(1u32), None)
        };
        let page = |_t: &str, _b: &str| {};
        let pid_alive = |_pid: i64| true; // recorded pid still alive

        run_with(now, &pid_alive, &spawn, &page);
        assert_eq!(spawns.get(), 0, "a hung (alive-pid) host is never double-spawned");
        assert!(!marker.exists(), "no page, no marker, for a hung host");

        let _ = std::fs::remove_file(&hb_path);
    }
}
