//! Port of control.py's lock / running-state core (the HIGHEST-RISK module): _pid_alive, _read_lock,
//! _lock_is_live, is_running, clear_lock, acquire_supervisor_lock, release_supervisor_lock.
//!
//! Behavior is bug-for-bug with control.py. The lock file lives at `runtime/<name>/lock` and is
//! `<pid>` (legacy) or `<pid>\n<run_id>`. Liveness requires a live PID PLUS run-id/heartbeat
//! corroboration so a recycled OS PID can't pin a dead loop "running" forever. Functions backing JS
//! bridge calls return `serde_json::Value` with byte-identical keys to the Python dicts.

use crate::control::heartbeat;
use crate::control::paths;
#[cfg(windows)]
use crate::control::proc;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// control._pid_alive (win32 branch). False on falsy pid (0) without spawning. On Windows runs
/// `tasklist /FI "PID eq <pid>" /NH /FO CSV` and tests for the pid as a QUOTED CSV field
/// (`"<pid>"`) — an exact match, never a substring of another column/PID. A tasklist spawn failure
/// (OSError) reads as False (must not crash the watchdog sweep). Off-Windows: a libc `kill(pid, 0)`
/// probe (the `os.kill(pid, 0)` branch).
pub fn pid_alive(pid: i64) -> bool {
    if pid == 0 {
        return false; // falsy pid short-circuit — no subprocess call
    }
    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        match proc::run(
            &["tasklist", "/FI", filter.as_str(), "/NH", "/FO", "CSV"],
            None,
            None,
        ) {
            Ok(out) => out.stdout.contains(&format!("\"{pid}\"")),
            Err(_) => false, // tasklist spawn failure (WinError 6/8) -> False, never propagate
        }
    }
    #[cfg(not(windows))]
    {
        // os.kill(pid, 0): returns Ok if the process exists (or EPERM), Err(ESRCH) if not.
        if pid <= 0 {
            return false;
        }
        unsafe {
            // signal 0 = existence/permission probe
            let r = libc_kill(pid as i32, 0);
            if r == 0 {
                true
            } else {
                // EPERM (1) means the process exists but we can't signal it -> alive.
                std::io::Error::last_os_error().raw_os_error() == Some(1)
            }
        }
    }
}

#[cfg(not(windows))]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// control._read_lock: parse `runtime/<name>/lock` -> (pid, run_id). The lock is `<pid>` (legacy)
/// or `<pid>\n<run_id>`. Returns (0, None) on missing/empty/whitespace-only/corrupt (non-int first
/// line). A blank/whitespace-only run_id line -> None.
pub fn read_lock(rt: &Path) -> (i64, Option<String>) {
    let raw = match std::fs::read_to_string(rt.join("lock")) {
        Ok(s) => s,
        Err(_) => return (0, None),
    };
    let raw = raw.trim(); // Python: (f.read() or "").strip()
    if raw.is_empty() {
        return (0, None);
    }
    // Python splitlines() then index lines[0]/lines[1]; first line parsed as int.
    let lines: Vec<&str> = splitlines(raw);
    let pid = match lines.first().map(|l| l.trim()).and_then(|s| s.parse::<i64>().ok()) {
        Some(p) => p,
        None => return (0, None), // ValueError/IndexError -> (0, None)
    };
    let run_id = match lines.get(1).map(|l| l.trim()) {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    (pid, run_id)
}

/// control._lock_is_live: whether `runtime/<name>/lock` is held by a LIVE runner.
///
/// Short-circuit order (EXACT — see source): no rt -> false; no pid OR dead pid -> false; no
/// heartbeat dict -> true (just-spawned); run_id both present AND differ -> false (orphaned by a
/// newer runner); status == "stopped" -> false; age None -> true; else age <= max(3*interval, floor).
/// `rt` defaults to the repo's runtime dir.
pub fn lock_is_live(repo: &Value) -> bool {
    match paths::runtime_dir(repo) {
        Some(rt) => lock_is_live_rt(repo, &rt),
        None => false,
    }
}

/// `_lock_is_live` with an explicit runtime dir (matches the `rt=None` default-then-override param).
/// Private — only the same-module callers (clear_lock, acquire_supervisor_lock) need the rt override;
/// the public surface is `lock_is_live(&Value)` per the contract.
fn lock_is_live_rt(repo: &Value, rt: &Path) -> bool {
    let (pid, run_id) = read_lock(rt);
    if pid == 0 || !pid_alive(pid) {
        return false;
    }
    let hb = heartbeat::read_heartbeat(repo);
    let interval = crate::control::registry::project_interval(repo);
    lock_is_live_decide(&run_id, hb.as_ref(), interval)
}

/// Pure liveness decision GIVEN a live PID: the heartbeat-corroboration branches of _lock_is_live.
/// Factored out so the golden vectors test the exact short-circuit order without subprocess/disk.
/// (Caller has already established pid != 0 AND pid_alive == true.)
fn lock_is_live_decide(lock_run_id: &Option<String>, hb: Option<&Value>, interval: i64) -> bool {
    let hb = match hb {
        Some(h) if h.is_object() => h,
        // not isinstance(hb, dict) -> True (lock + live PID, no heartbeat yet -> just-started)
        _ => return true,
    };
    // `if run_id and hb.get("run_id") and hb.get("run_id") != run_id`: lock run_id truthy AND the
    // heartbeat run_id truthy AND they differ -> orphaned by a newer runner. Compare the RAW
    // heartbeat Value (not as_str): Python compares values, so a truthy NON-string run_id from a
    // corrupt/externally-written heartbeat (e.g. a number) is cross-type-unequal to the lock's
    // string and must also orphan — the old as_str() narrowing silently skipped that, keeping a
    // dead lock wrongly LIVE and blocking recovery.
    if let Some(lr) = lock_run_id.as_deref().filter(|s| !s.is_empty()) {
        let orphaned = match hb.get("run_id") {
            None => false,
            Some(Value::String(hr)) => !hr.is_empty() && hr != lr,
            Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Number(n)) => n.as_f64() != Some(0.0),
            Some(Value::Bool(true)) => true,
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
        };
        if orphaned {
            return false; // a newer runner owns the heartbeat; this lock is orphaned
        }
    }
    if hb.get("status").and_then(Value::as_str) == Some("stopped") {
        return false; // the lock's runner cleanly exited
    }
    let age = match heartbeat::heartbeat_age(hb) {
        Some(a) => a,
        None => return true, // no usable timestamp -> don't declare a live PID dead on that alone
    };
    let threshold = (3.0 * interval as f64).max(paths::LOCK_LIVE_FLOOR_S);
    age <= threshold
}

/// control.is_running: True if `runtime/<name>/lock` is held by a LIVE runner. Delegates to
/// _lock_is_live.
pub fn is_running(repo: &Value) -> bool {
    lock_is_live(repo)
}

/// control.clear_lock: remove a STALE lock (no live runner). REFUSES if a live runner holds it
/// (same liveness test as is_running). `{ok, removed}` / `{ok:false, error}`.
pub fn clear_lock(repo: &Value) -> Value {
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return json!({"ok": false, "error": "repo has no 'path'"}),
    };
    let lock = rt.join("lock");
    if !lock.exists() {
        return json!({"ok": true, "removed": false}); // nothing to clear
    }
    if lock_is_live_rt(repo, &rt) {
        let (pid, _) = read_lock(&rt);
        return json!({"ok": false, "error": format!("lock held by live runner (pid {pid})")});
    }
    match std::fs::remove_file(&lock) {
        Ok(()) => json!({"ok": true, "removed": true}),
        Err(e) => json!({"ok": false, "error": os_error_string(&e)}),
    }
}

/// control.acquire_supervisor_lock: take the repo's single-flight lock for a git-mutating recovery.
///
/// Returns (ok, token). ok==false means a LIVE runner holds the lock (caller escalates). Mirrors
/// run_improver.acquire_lock's atomic O_EXCL create + 0-byte-cleanup + never-steal-empty + tmp+rename
/// steal + sleep(100ms) + re-read confirm.
pub fn acquire_supervisor_lock(repo: &Value) -> (bool, Option<String>) {
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return (false, None),
    };
    if std::fs::create_dir_all(&rt).is_err() {
        return (false, None);
    }
    let lock = rt.join("lock");
    let token = format!("sup-{}", rand_hex32());
    let content = format!("{}\n{}", std::process::id(), token);

    // Atomic exclusive create — no lock present at all (O_CREAT | O_EXCL).
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(mut f) => {
            // O_EXCL succeeded. If the write fails, a 0-byte lock would be refused-as-empty forever
            // (never stolen) -> remove the file we just created and report not-acquired.
            if f.write_all(content.as_bytes()).is_ok() {
                return (true, Some(token));
            }
            drop(f); // close BEFORE remove (Windows can't unlink an open file)
            let _ = std::fs::remove_file(&lock);
            return (false, None);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // FileExistsError -> fall through to the takeover path.
        }
        Err(_) => return (false, None), // transient ACL/lock OSError -> escalate, never raise
    }

    // An empty/unparseable lock means a racer is mid-create or the file is corrupt — treat as HELD
    // and back off. Never steal an empty lock.
    if read_lock(&rt).0 == 0 {
        return (false, None);
    }
    if lock_is_live_rt(repo, &rt) {
        return (false, None); // a live runner holds it — do NOT mutate git under it
    }
    // Stale/recycled lock — take it over via tmp+rename, then verify we won.
    let tmp = rt.join(format!("lock.sup.{}.tmp", std::process::id()));
    if (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        drop(f);
        std::fs::rename(&tmp, &lock)
    })()
    .is_err()
    {
        return (false, None);
    }
    std::thread::sleep(Duration::from_millis(100));
    (read_lock(&rt).1.as_deref() == Some(token.as_str()), Some(token))
}

/// control.release_supervisor_lock: release the lock only if WE still hold it (run-id == token).
/// No-op on empty token / no runtime dir / token mismatch. A failed remove is swallowed.
pub fn release_supervisor_lock(repo: &Value, token: &str) {
    if token.is_empty() {
        return;
    }
    let rt = match paths::runtime_dir(repo) {
        Some(rt) => rt,
        None => return,
    };
    if read_lock(&rt).1.as_deref() == Some(token) {
        let _ = std::fs::remove_file(rt.join("lock"));
    }
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// str(OSError) shape: Windows OS errors stringify like "Access is denied. (os error 5)" in Rust,
/// whereas Python yields just "Access is denied". The golden vectors compare on the message; we strip
/// Rust's " (os error N)" suffix and a trailing period to land on the Python-visible text.
fn os_error_string(e: &std::io::Error) -> String {
    let s = e.to_string();
    let s = match s.find(" (os error ") {
        Some(idx) => s[..idx].to_string(),
        None => s,
    };
    s.trim_end_matches('.').to_string()
}

/// A unique 32-hex-char token, matching the shape of Python's `uuid.uuid4().hex` (the supervisor
/// run-id). DEVIATION: this is NOT a cryptographic UUIDv4 — the `uuid` crate is not a declared
/// dependency and we may not edit Cargo.toml. Uniqueness comes from a splitmix64 stream seeded by
/// the high-resolution clock, PID, and a per-process atomic counter — collision-resistant enough for
/// single-flight lock arbitration (the only use), and the run-id is compared verbatim, never parsed
/// as a UUID.
fn rand_hex32() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = nanos
        ^ ((std::process::id() as u64) << 32)
        ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    // splitmix64: two draws give 128 bits -> 32 hex chars.
    let mut next = || {
        seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    format!("{:016x}{:016x}", next(), next())
}

/// Public shim so the build semaphore reuses the SAME token generator as the supervisor lock (no
/// second RNG, no `uuid`/`rand` dep). Not part of the control.py contract — internal reuse only.
pub fn rand_hex32_pub() -> String {
    rand_hex32()
}

/// Read a `<pid>\n<token>` lock-shaped file named `fname` inside `dir`, using `read_lock`'s exact
/// parse rules. The build semaphore's `slot-<i>` files share the lock format; this lets it reuse the
/// parser without re-implementing it. (`read_lock` itself is hardcoded to the filename `lock`, so
/// this points the identical rules at an arbitrary sibling filename.) Returns (pid, run_id).
pub fn read_lock_dir(dir: &Path, fname: &str) -> (i64, Option<String>) {
    let raw = match std::fs::read_to_string(dir.join(fname)) {
        Ok(s) => s,
        Err(_) => return (0, None),
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return (0, None);
    }
    let lines: Vec<&str> = splitlines(raw);
    let pid = match lines.first().map(|l| l.trim()).and_then(|s| s.parse::<i64>().ok()) {
        Some(p) => p,
        None => return (0, None),
    };
    let run_id = match lines.get(1).map(|l| l.trim()) {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => None,
    };
    (pid, run_id)
}

/// Python str.splitlines() for the lock parser (\n / \r\n / \r boundaries, no trailing empty).
fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let (mut start, mut i) = (0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                out.push(&s[start..i]);
                i += 1;
                start = i;
            }
            b'\r' => {
                out.push(&s[start..i]);
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    i += 2;
                } else {
                    i += 1;
                }
                start = i;
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        out.push(&s[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -------- _pid_alive golden vectors --------
    // The substring/quoting logic is the load-bearing part (a recycled or substring PID must not read
    // "alive"). We test it directly against the same CSV-field rule _pid_alive applies to tasklist's
    // stdout, plus the falsy-pid short-circuit which needs no subprocess.
    fn pid_in_csv(pid: i64, stdout: &str) -> bool {
        stdout.contains(&format!("\"{pid}\""))
    }

    #[test]
    fn pid_alive_csv_match_rule() {
        // quoted pid present -> alive
        assert!(pid_in_csv(1234, "\"python.exe\",\"1234\",\"Console\",\"1\",\"50,000 K\"\r\n"));
        // "INFO: No tasks" -> not alive
        assert!(!pid_in_csv(
            1234,
            "INFO: No tasks are running which match the specified criteria.\r\n"
        ));
        // pid only as unquoted substring of another field -> not alive
        assert!(!pid_in_csv(234, "\"a.exe\",\"91234\",\"Console\"\r\n"));
        // empty stdout (e.g. None coalesced) -> not alive
        assert!(!pid_in_csv(1234, ""));
    }

    #[test]
    fn pid_alive_falsy_short_circuits() {
        // pid 0 -> False with no subprocess. (None maps to 0/absent at the call sites.)
        assert!(!pid_alive(0));
    }

    // -------- _read_lock golden vectors --------
    fn write_lock(content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "solomon_lock_rl_{}_{}",
            std::process::id(),
            rand_hex32()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), content).unwrap();
        (dir.clone(), dir)
    }

    #[test]
    fn read_lock_vectors() {
        let cases: &[(&str, (i64, Option<&str>))] = &[
            ("12345\nsup-deadbeef", (12345, Some("sup-deadbeef"))),
            ("999", (999, None)),
            ("", (0, None)),
            ("   \n  ", (0, None)),
            ("notapid\ntok", (0, None)),
            ("7\n   ", (7, None)),
            ("500\ntoken123\n", (500, Some("token123"))),
        ];
        for (content, (epid, erun)) in cases {
            let (rt, _) = write_lock(content);
            let (pid, run) = read_lock(&rt);
            assert_eq!(pid, *epid, "pid for {content:?}");
            assert_eq!(run.as_deref(), *erun, "run_id for {content:?}");
            let _ = std::fs::remove_dir_all(&rt);
        }
        // missing file -> (0, None)
        let missing = std::env::temp_dir().join(format!(
            "solomon_lock_missing_{}",
            rand_hex32()
        ));
        assert_eq!(read_lock(&missing), (0, None));
    }

    // -------- _lock_is_live golden vectors (pure decision, live PID assumed) --------
    fn ts_ago(secs: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    #[test]
    fn lock_is_live_decide_vectors() {
        // live PID, no heartbeat -> just started -> True
        assert!(lock_is_live_decide(&Some("tokA".into()), None, 120));
        // run_id mismatch (orphan) -> False
        let hb = json!({"run_id":"tokB","status":"running","updated_at":ts_ago(1)});
        assert!(!lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // run_id match, fresh -> True
        let hb = json!({"run_id":"tokA","status":"running","updated_at":ts_ago(1)});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // status stopped overrides freshness -> False
        let hb = json!({"run_id":"tokA","status":"stopped","updated_at":ts_ago(1)});
        assert!(!lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // legacy lock run_id None, hb has different run_id -> mismatch SKIPPED -> True
        let hb = json!({"run_id":"tokB","status":"running","updated_at":ts_ago(1)});
        assert!(lock_is_live_decide(&None, Some(&hb), 120));
        // age just inside floor, default interval 120 -> True (<=4500 <= max(360,4500)).
        // ts_ago(4499) (not 4500) so the few-ms of wall-clock elapsed during evaluation cannot
        // push the computed age fractionally PAST 4500 and flip the inclusive-boundary check.
        let hb = json!({"run_id":"tokA","updated_at":ts_ago(4499)});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // age 1s past floor -> False
        let hb = json!({"run_id":"tokA","updated_at":ts_ago(4502)});
        assert!(!lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // large interval raises threshold: 5000 <= max(6000,4500)=6000 -> True
        let hb = json!({"run_id":"tokA","updated_at":ts_ago(5000)});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 2000));
        // unparseable updated_at -> age None -> True
        let hb = json!({"run_id":"tokA","updated_at":"garbage"});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // missing updated_at key -> age None -> True
        let hb = json!({"run_id":"tokA","status":"running"});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // empty/null hb (non-dict) treated as just-started -> True
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&Value::Null), 120));
        // a truthy NON-string run_id (corrupt heartbeat) differing from the lock's string -> orphaned -> False
        let hb = json!({"run_id": 42, "status": "running", "updated_at": ts_ago(1)});
        assert!(!lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
        // a FALSY non-string run_id (0) is `hb.get("run_id")`-falsy in Python -> not orphaned -> True
        let hb = json!({"run_id": 0, "status": "running", "updated_at": ts_ago(1)});
        assert!(lock_is_live_decide(&Some("tokA".into()), Some(&hb), 120));
    }

    #[test]
    fn lock_is_live_dead_pid_paths() {
        // pid 0 (empty lock) -> False without pid_alive; exercised via lock_is_live_rt with empty lock.
        let dir = std::env::temp_dir().join(format!(
            "solomon_lock_dead_{}",
            rand_hex32()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), "").unwrap();
        assert!(!lock_is_live_rt(&json!({"name":"x"}), &dir));
        // a PID that is essentially never alive (huge) -> dead pid -> False
        std::fs::write(dir.join("lock"), "2147483646\ntokA").unwrap();
        assert!(!lock_is_live_rt(&json!({"name":"x"}), &dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -------- is_running --------
    #[test]
    fn is_running_no_runtime_dir() {
        assert!(!is_running(&json!({}))); // no name -> runtime_dir None -> False
    }

    // -------- clear_lock golden vectors --------
    #[test]
    fn clear_lock_no_path() {
        assert_eq!(
            clear_lock(&json!({})),
            json!({"ok": false, "error": "repo has no 'path'"})
        );
    }

    #[test]
    fn clear_lock_no_lock_file() {
        let name = format!("clr_nolock_{}", rand_hex32());
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        let _ = std::fs::remove_file(rt.join("lock"));
        assert_eq!(clear_lock(&repo), json!({"ok": true, "removed": false}));
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn clear_lock_stale_removed() {
        // dead PID lock -> not live -> removed
        let name = format!("clr_stale_{}", rand_hex32());
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        std::fs::write(rt.join("lock"), "2147483646\ntokA").unwrap();
        // ensure no heartbeat so liveness depends purely on the dead pid
        let _ = std::fs::remove_file(rt.join("heartbeat.json"));
        assert_eq!(clear_lock(&repo), json!({"ok": true, "removed": true}));
        assert!(!rt.join("lock").exists());
        let _ = std::fs::remove_dir_all(&rt);
    }

    // -------- acquire / release supervisor lock golden vectors --------
    #[test]
    fn acquire_no_runtime_dir() {
        assert_eq!(acquire_supervisor_lock(&json!({})), (false, None));
    }

    #[test]
    fn acquire_fresh_then_release() {
        let name = format!("sup_fresh_{}", rand_hex32());
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&rt);

        // fresh acquire (no lock present) -> (true, token); file contains "<pid>\n<token>"
        let (ok, token) = acquire_supervisor_lock(&repo);
        assert!(ok);
        let token = token.unwrap();
        let content = std::fs::read_to_string(rt.join("lock")).unwrap();
        assert_eq!(content, format!("{}\n{}", std::process::id(), token));

        // a second acquire while we (a live PID == this process) hold a fresh lock: our own PID is
        // alive and there's no heartbeat -> lock_is_live true -> (false, None).
        let (ok2, tok2) = acquire_supervisor_lock(&repo);
        assert!(!ok2);
        assert_eq!(tok2, None);

        // release with the wrong token -> kept
        release_supervisor_lock(&repo, "sup-wrongtoken");
        assert!(rt.join("lock").exists());
        // release with the right token -> removed
        release_supervisor_lock(&repo, &token);
        assert!(!rt.join("lock").exists());

        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn acquire_steals_stale_lock() {
        // pre-existing lock owned by a DEAD pid -> takeover wins -> (true, token)
        let name = format!("sup_steal_{}", rand_hex32());
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        std::fs::write(rt.join("lock"), "2147483646\noldtok").unwrap();
        let _ = std::fs::remove_file(rt.join("heartbeat.json"));
        let (ok, token) = acquire_supervisor_lock(&repo);
        assert!(ok);
        let token = token.unwrap();
        assert_eq!(read_lock(&rt).1.as_deref(), Some(token.as_str()));
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn acquire_never_steals_empty_lock() {
        // pre-existing EMPTY lock (racer mid-create) -> treated as held -> (false, None)
        let name = format!("sup_empty_{}", rand_hex32());
        let repo = json!({ "name": name });
        let rt = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::create_dir_all(&rt);
        std::fs::write(rt.join("lock"), "").unwrap();
        assert_eq!(acquire_supervisor_lock(&repo), (false, None));
        let _ = std::fs::remove_dir_all(&rt);
    }

    #[test]
    fn release_guards() {
        // empty token -> no-op (no panic, no read)
        release_supervisor_lock(&json!({"name":"rel_x"}), "");
        // no runtime dir -> no-op
        release_supervisor_lock(&json!({}), "sup-x");
    }
}
