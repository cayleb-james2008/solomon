//! Janitor subsystem (operator requirement 5): temp-file deletion, log rotation, bounded runtime
//! dirs, history compaction — all STRICTLY inside `<HERE>/runtime/`. Ridden by the watchdog sweep
//! (both the GUI tick and the out-of-band Solomon Sentinel cadence) behind a 6h stamp, so storage
//! hygiene can never depend on a human remembering to clean up.
//!
//! Hard safety contract: never touches LOCK/heartbeat.json/freshness.json/cycle_budget.json/
//! progress.json/_provider_budget.json/_sentinel_heartbeat.json/_config_provenance.json (the
//! liveness + ledger files other subsystems own), and never a path outside runtime/ (every
//! candidate comes from a walk rooted there; symlinks are not followed so a junction cannot
//! smuggle the walk out).

use crate::control::paths;
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Files the janitor must NEVER delete, rotate, or rewrite (case-insensitive: Windows FS is, and
/// the contract names "LOCK" while the runner writes "lock").
const PROTECTED: &[&str] = &[
    "lock",
    "heartbeat.json",
    "freshness.json",
    "cycle_budget.json",
    "progress.json",
    "_provider_budget.json",
    "_sentinel_heartbeat.json",
    "_config_provenance.json",
    // Host-independent liveness floor (catalog #4): the engine-host dead-man heartbeat + the
    // paged-once dedupe marker the resurrector owns. Sweeping either would blind the liveness floor
    // (a deleted heartbeat reads as "missing" -> Wait, and a deleted marker re-arms the page storm).
    "_engine_heartbeat.json",
    "_resurrector.marker",
    "_resurrector.last",
];

/// The tunable thresholds, parameterized so the unit tests exercise every phase on tiny tmp trees
/// instead of writing 20MB fixtures. Production always uses [`Limits::default`].
struct Limits {
    /// stale-temp deletion age (contract: 7 days)
    stale_age_s: u64,
    /// rotate *.log / *.jsonl over this many bytes (contract: 20MB)
    rotate_bytes: u64,
    /// per-runtime/<repo> dir size cap (contract: 200MB)
    repo_cap_bytes: u64,
    /// compact history.jsonl over this many lines... (contract: 10000)
    compact_over: usize,
    /// ...down to its last this-many lines (contract: 5000)
    compact_keep: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            stale_age_s: 7 * 86400,
            rotate_bytes: 20 * 1024 * 1024,
            repo_cap_bytes: 200 * 1024 * 1024,
            compact_over: 10_000,
            compact_keep: 5_000,
        }
    }
}

fn iso_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// One janitor pass over `<HERE>/runtime/`. Returns
/// `{"deleted": n, "rotated": n, "freed_bytes": u64, "actions": [str]}`.
pub fn sweep() -> Value {
    sweep_dir(&paths::here().join("runtime"), &Limits::default())
}

fn protected(name: &str) -> bool {
    PROTECTED.iter().any(|p| p.eq_ignore_ascii_case(name))
}

/// Test-scratch dir name prefixes the janitor reaps (2026-07-07 audit A.6). `cargo test` writes
/// PID-suffixed working dirs directly into the LIVE `<HERE>/runtime/` (the supervisor/watchdog/
/// fleet tests exercise the real runtime dir, not `std::env::temp_dir()`), so they accumulate
/// unbounded — 19 observed on 2026-07-07. They are pure test detritus (no live ledger ever lands
/// under these names), so the janitor prunes them as stale temp. NOTE: `runtime/` is already
/// wholesale-gitignored, so these do NOT trigger the `git status --porcelain` controller-dirty
/// gate — this is storage hygiene (the janitor's bounded-runtime-dir contract), not a
/// controller-clean fix. Matched as a `starts_with` prefix so the PID/nanos suffix is irrelevant.
const TEST_SCRATCH_DIR_PREFIXES: &[&str] = &[
    "sup_test_",
    "testrepo_",
    "wd_ar_",
    "wd_heal_",
    "wd_stall_",
    "app_test_",
];

/// The stale-temp deletion predicate (pure — unit-tested): `_bb_*.js` / `_bb_*.md` / `*.tmp` /
/// `_debug_*` files, and `__pycache__` + the test-scratch (`sup_test_*`/`testrepo_*`/`wd_ar_*`/
/// `wd_heal_*`/`wd_stall_*`/`app_test_*`) dirs. Age is checked by the caller (all classes: 7 days).
fn stale_temp(name: &str, is_dir: bool) -> bool {
    if is_dir {
        if name == "__pycache__" {
            return true;
        }
        return TEST_SCRATCH_DIR_PREFIXES
            .iter()
            .any(|p| name.starts_with(p));
    }
    let n = name.to_ascii_lowercase();
    (n.starts_with("_bb_") && (n.ends_with(".js") || n.ends_with(".md")))
        || n.ends_with(".tmp")
        || n.starts_with("_debug_")
}

fn file_name(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string()
}

/// mtime age in seconds; None on any metadata error (an unreadable entry is left alone).
fn age_s(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
        .map(|d| d.as_secs())
}

fn mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Recursive walk under `dir` collecting files and dirs. Symlinks/junctions are skipped entirely so
/// the walk can never escape runtime/ (the "never touch anything outside runtime/" contract).
fn walk(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let p = e.path();
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            dirs.push(p.clone());
            walk(&p, files, dirs);
        } else {
            files.push(p);
        }
    }
}

fn dir_size(dir: &Path) -> u64 {
    let mut files = Vec::new();
    let mut d = Vec::new();
    walk(dir, &mut files, &mut d);
    files.iter().map(|f| file_len(f)).sum()
}

/// The path-parameterized core so tests run against isolated tmp trees.
fn sweep_dir(root: &Path, lim: &Limits) -> Value {
    let mut deleted: u64 = 0;
    let mut rotated: u64 = 0;
    let mut freed: u64 = 0;
    let mut actions: Vec<String> = Vec::new();
    if !root.is_dir() {
        return json!({"deleted": 0, "rotated": 0, "freed_bytes": 0, "actions": []});
    }

    // (1) stale-temp deletion: pattern-matched files/dirs older than stale_age_s.
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, &mut files, &mut dirs);
    for p in &files {
        let name = file_name(p);
        if protected(&name) || !stale_temp(&name, false) {
            continue;
        }
        if age_s(p).map(|a| a >= lim.stale_age_s) != Some(true) {
            continue;
        }
        let len = file_len(p);
        if std::fs::remove_file(p).is_ok() {
            deleted += 1;
            freed += len;
            actions.push(format!("janitor: deleted stale temp {name}"));
        }
    }
    for d in &dirs {
        let name = file_name(d);
        if !stale_temp(&name, true) || age_s(d).map(|a| a >= lim.stale_age_s) != Some(true) {
            continue;
        }
        let size = dir_size(d);
        if std::fs::remove_dir_all(d).is_ok() {
            deleted += 1;
            freed += size;
            actions.push(format!("janitor: deleted stale {name} dir"));
        }
    }

    // (2) rotation: any *.log / *.jsonl over rotate_bytes -> <file>.1 (delete an existing .1 first —
    // max 2 generations: the live file + one .1), recreate empty. Best-effort: a file another
    // process holds open without share-delete just skips this pass.
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, &mut files, &mut dirs);
    for p in &files {
        let name = file_name(p);
        let n = name.to_ascii_lowercase();
        if protected(&name) || !(n.ends_with(".log") || n.ends_with(".jsonl")) {
            continue;
        }
        let len = file_len(p);
        if len <= lim.rotate_bytes {
            continue;
        }
        let gen1 = p.with_file_name(format!("{name}.1"));
        if gen1.exists() {
            let old = file_len(&gen1);
            if std::fs::remove_file(&gen1).is_ok() {
                freed += old;
            }
        }
        if std::fs::rename(p, &gen1).is_ok() && std::fs::File::create(p).is_ok() {
            rotated += 1;
            actions.push(format!("janitor: rotated {name} ({len} bytes -> {name}.1)"));
        }
    }

    // (3) bound each runtime/<repo> dir to repo_cap_bytes: delete oldest *.1 rotations first, then
    // oldest _canary* dirs — never anything else (a hard cap must not eat live ledgers).
    let subdirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir() && !t.is_symlink()).unwrap_or(false))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    for sub in &subdirs {
        let mut size = dir_size(sub);
        if size <= lim.repo_cap_bytes {
            continue;
        }
        let before = size;
        let mut files = Vec::new();
        let mut dirs = Vec::new();
        walk(sub, &mut files, &mut dirs);
        let mut rot1: Vec<&PathBuf> = files
            .iter()
            .filter(|p| file_name(p).ends_with(".1") && !protected(&file_name(p)))
            .collect();
        rot1.sort_by_key(|p| mtime(p));
        for p in rot1 {
            if size <= lim.repo_cap_bytes {
                break;
            }
            let len = file_len(p);
            if std::fs::remove_file(p).is_ok() {
                deleted += 1;
                freed += len;
                size = size.saturating_sub(len);
            }
        }
        if size > lim.repo_cap_bytes {
            let mut canaries: Vec<&PathBuf> = dirs
                .iter()
                .filter(|d| file_name(d).starts_with("_canary"))
                .collect();
            canaries.sort_by_key(|d| mtime(d));
            for d in canaries {
                if size <= lim.repo_cap_bytes {
                    break;
                }
                let s = dir_size(d);
                if std::fs::remove_dir_all(d).is_ok() {
                    deleted += 1;
                    freed += s;
                    size = size.saturating_sub(s);
                }
            }
        }
        if size < before {
            actions.push(format!(
                "janitor: bounded {} ({before} -> {size} bytes, cap {})",
                file_name(sub),
                lim.repo_cap_bytes
            ));
        }
    }

    // (4) compact any history.jsonl over compact_over lines to its last compact_keep, atomically
    // (tmp + rename), appending one honest marker line so the truncation is visible in the ledger.
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    walk(root, &mut files, &mut dirs);
    for p in &files {
        if file_name(p) != "history.jsonl" {
            continue;
        }
        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() <= lim.compact_over {
            continue;
        }
        let dropped = lines.len() - lim.compact_keep;
        let mut out = lines[lines.len() - lim.compact_keep..].join("\n");
        out.push('\n');
        out.push_str(
            &serde_json::to_string(&json!({"janitor_compacted": iso_now(), "dropped": dropped}))
                .unwrap_or_default(),
        );
        out.push('\n');
        let tmp = p.with_file_name("history.jsonl.compact_new");
        let old_len = file_len(p);
        let ok = std::fs::write(&tmp, &out).is_ok() && std::fs::rename(&tmp, p).is_ok();
        if ok {
            freed += old_len.saturating_sub(out.len() as u64);
            actions.push(format!(
                "janitor: compacted {} history.jsonl ({} -> {} lines, dropped {dropped})",
                p.parent().map(file_name).unwrap_or_default(),
                lines.len(),
                lim.compact_keep + 1
            ));
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    json!({"deleted": deleted, "rotated": rotated, "freed_bytes": freed, "actions": actions})
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_mtime, FileTime};

    fn tmp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "solomon_janitor_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn make_old(p: &Path) {
        // 8 days ago — past the 7-day stale window.
        let old = FileTime::from_unix_time(
            (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
                - 8 * 86400) as i64,
            0,
        );
        set_file_mtime(p, old).unwrap();
    }

    // -------- pattern + protected predicates (pure) --------
    #[test]
    fn stale_temp_pattern_table() {
        for (name, is_dir, want) in [
            ("_bb_probe.js", false, true),
            ("_bb_notes.md", false, true),
            ("_bb_data.json", false, false), // only .js/.md under _bb_
            ("scratch.tmp", false, true),
            ("lock.1234.tmp", false, true),
            ("_debug_dump.txt", false, true),
            ("__pycache__", true, true),
            ("__pycache__", false, false), // dir-only pattern
            // test-scratch dirs (audit A.6): reaped as dirs, never as files
            ("sup_test_stale_live_pid_154396", true, true),
            ("testrepo_keep", true, true),
            ("wd_ar_heal_15760", true, true),
            ("wd_heal_21792_293700", true, true),
            ("wd_stall_14520_83900", true, true),
            ("app_test_foo_123", true, true),
            ("sup_test_stale_live_pid_154396", false, false), // dir-only: a same-named FILE is left alone
            ("sup_testish", true, false),                     // needs the trailing underscore prefix
            ("history.jsonl", false, false),
            ("heartbeat.json", false, false),
            ("normal.log", false, false),
            ("runtime", true, false), // a real lane/runtime dir must never match
        ] {
            assert_eq!(stale_temp(name, is_dir), want, "{name} is_dir={is_dir}");
        }
    }

    #[test]
    fn protected_list_is_case_insensitive_and_complete() {
        for name in [
            "LOCK",
            "lock",
            "heartbeat.json",
            "freshness.json",
            "cycle_budget.json",
            "progress.json",
            "_provider_budget.json",
            "_sentinel_heartbeat.json",
            "_config_provenance.json",
        ] {
            assert!(protected(name), "{name} must be protected");
        }
        assert!(!protected("history.jsonl"));
        assert!(!protected("_monitor.jsonl"));
    }

    // -------- phase 1: stale-temp deletion honors age + protection --------
    #[test]
    fn deletes_old_temps_keeps_fresh_and_protected() {
        let root = tmp_root("del");
        let lane = root.join("lane");
        std::fs::create_dir_all(lane.join("__pycache__")).unwrap();
        std::fs::write(lane.join("__pycache__").join("m.pyc"), b"x").unwrap();
        std::fs::write(lane.join("old.tmp"), b"1234").unwrap();
        std::fs::write(lane.join("fresh.tmp"), b"1234").unwrap();
        std::fs::write(lane.join("_bb_probe.js"), b"js").unwrap();
        std::fs::write(lane.join("heartbeat.json"), b"{}").unwrap();
        std::fs::write(lane.join("lock"), b"123").unwrap();
        for p in ["old.tmp", "_bb_probe.js", "heartbeat.json", "lock"] {
            make_old(&lane.join(p));
        }
        make_old(&lane.join("__pycache__"));

        let out = sweep_dir(&root, &Limits::default());
        assert!(!lane.join("old.tmp").exists(), "old .tmp must be deleted");
        assert!(!lane.join("_bb_probe.js").exists(), "old _bb_*.js must be deleted");
        assert!(!lane.join("__pycache__").exists(), "old __pycache__ dir must be deleted");
        assert!(lane.join("fresh.tmp").exists(), "fresh .tmp must survive");
        assert!(lane.join("heartbeat.json").exists(), "protected must survive any age");
        assert!(lane.join("lock").exists(), "lock must survive any age");
        assert_eq!(out["deleted"], 3);
        assert!(out["freed_bytes"].as_u64().unwrap() > 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    // -------- phase 2: rotation over the byte threshold, max 2 generations --------
    #[test]
    fn rotates_oversized_logs_and_replaces_gen1() {
        let root = tmp_root("rot");
        let lane = root.join("lane");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::write(lane.join("big.log"), vec![b'x'; 100]).unwrap();
        std::fs::write(lane.join("big.log.1"), vec![b'y'; 50]).unwrap(); // stale gen-1 to replace
        std::fs::write(lane.join("small.jsonl"), b"tiny").unwrap();
        let lim = Limits { rotate_bytes: 64, ..Limits::default() };

        let out = sweep_dir(&root, &lim);
        assert_eq!(out["rotated"], 1);
        assert_eq!(file_len(&lane.join("big.log")), 0, "live file recreated empty");
        assert_eq!(file_len(&lane.join("big.log.1")), 100, "gen-1 is the old live file");
        assert!(!lane.join("big.log.1.1").exists(), "never a third generation");
        assert_eq!(file_len(&lane.join("small.jsonl")), 4, "under-threshold untouched");
        assert!(out["freed_bytes"].as_u64().unwrap() >= 50, "old .1 counted as freed");
        let _ = std::fs::remove_dir_all(&root);
    }

    // -------- phase 3: repo-dir cap deletes oldest .1 first, then oldest _canary dirs --------
    #[test]
    fn bounds_repo_dir_oldest_rotations_then_canaries() {
        let root = tmp_root("cap");
        let lane = root.join("lane");
        std::fs::create_dir_all(lane.join("_canary_a")).unwrap();
        std::fs::write(lane.join("keep.jsonl"), vec![b'k'; 40]).unwrap();
        std::fs::write(lane.join("old.log.1"), vec![b'o'; 40]).unwrap();
        std::fs::write(lane.join("new.log.1"), vec![b'n'; 40]).unwrap();
        std::fs::write(lane.join("_canary_a").join("f"), vec![b'c'; 40]).unwrap();
        make_old(&lane.join("old.log.1"));
        make_old(&lane.join("_canary_a"));
        // 160 bytes total; cap 100: oldest .1 (old.log.1, -40) -> 120, still over -> new.log.1
        // (-40) -> 80 <= cap. The canary dir survives because the .1 deletions sufficed.
        let lim = Limits { repo_cap_bytes: 100, ..Limits::default() };
        sweep_dir(&root, &lim);
        assert!(!lane.join("old.log.1").exists(), "oldest .1 deleted first");
        assert!(!lane.join("new.log.1").exists(), ".1 rotations deleted before canaries");
        assert!(lane.join("_canary_a").exists(), "canary spared once under cap");
        assert!(lane.join("keep.jsonl").exists(), "non-rotation files never deleted by the cap");

        // Still over cap with no .1 left -> the oldest canary dir goes.
        let lim2 = Limits { repo_cap_bytes: 50, ..Limits::default() };
        sweep_dir(&root, &lim2);
        assert!(!lane.join("_canary_a").exists(), "canary deleted when .1s are exhausted");
        assert!(lane.join("keep.jsonl").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    // -------- phase 4: history compaction is atomic, honest, and bounded --------
    #[test]
    fn compacts_oversized_history_and_appends_marker() {
        let root = tmp_root("compact");
        let lane = root.join("lane");
        std::fs::create_dir_all(&lane).unwrap();
        let lines: Vec<String> = (0..12).map(|i| format!("{{\"i\":{i}}}")).collect();
        std::fs::write(lane.join("history.jsonl"), format!("{}\n", lines.join("\n"))).unwrap();
        let lim = Limits { compact_over: 10, compact_keep: 5, ..Limits::default() };

        let out = sweep_dir(&root, &lim);
        let content = std::fs::read_to_string(lane.join("history.jsonl")).unwrap();
        let got: Vec<&str> = content.lines().collect();
        assert_eq!(got.len(), 6, "5 kept + 1 marker");
        assert_eq!(got[0], "{\"i\":7}", "keeps the LAST 5");
        let marker: Value = serde_json::from_str(got[5]).unwrap();
        assert_eq!(marker["dropped"], 7);
        assert!(marker.get("janitor_compacted").is_some());
        assert!(!lane.join("history.jsonl.compact_new").exists(), "tmp cleaned up");
        assert!(out["actions"].as_array().unwrap().iter().any(|a| a
            .as_str()
            .unwrap_or("")
            .contains("compacted")));

        // Idempotent: a second sweep leaves the already-small file alone.
        sweep_dir(&root, &lim);
        let again = std::fs::read_to_string(lane.join("history.jsonl")).unwrap();
        assert_eq!(again.lines().count(), 6);
        let _ = std::fs::remove_dir_all(&root);
    }

    // -------- missing root is a zero no-op, never a panic --------
    #[test]
    fn missing_root_returns_zeros() {
        let out = sweep_dir(Path::new("Z:\\definitely\\not\\here"), &Limits::default());
        assert_eq!(out["deleted"], 0);
        assert_eq!(out["rotated"], 0);
        assert_eq!(out["freed_bytes"], 0);
        assert!(out["actions"].as_array().unwrap().is_empty());
    }
}
