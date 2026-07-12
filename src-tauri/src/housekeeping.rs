//! Storage housekeeping (Solomon v2) — the fleet must not eat the operator's disk.
//!
//! Measured 2026-07-02: ~41 GB of Rust `target/` accumulation across the managed repos (Rust
//! never garbage-collects build artifacts), plus stranded improver worktrees and merged-but-kept
//! `rsi/` branches. This module makes cleanup AUTOMATIC, riding the watchdog tick (no scheduled
//! task — operator rule), day-gated to the 04:00 quiet hour via the same gate machinery as the
//! CEO rhythm.
//!
//! Per managed repo (repos.json, deduped by path), in increasing aggressiveness:
//!   1. `git worktree prune` — drop stale worktree registrations (cheap, always safe).
//!   2. Delete local `<branch_prefix>*` branches FULLY MERGED into the base branch (`git branch
//!      -d` — the porcelain refuses unmerged work, so experiments are never lost).
//!   3. Delete STALE build dirs — `target*` / `build` / `src-tauri/target` whose newest shallow
//!      mtime is older than [`STALE_DAYS`] (an abandoned build tree is pure dead weight).
//!   4. Worktree hygiene — enforce the operator rule "<=1 worktree besides main/master": remove
//!      every NON-main, NON-locked worktree whose `git status --porcelain` is clean (an ignored
//!      `target/` does NOT show, so orphaned `.claude/worktrees/agent-*` copies — clean modulo
//!      build output — are reaped, freeing their nested `target/` that [`candidate_build_dirs`]
//!      never enumerates). A non-empty status (real in-flight work) KEEPS the worktree; git keeps
//!      the branch ref (no commit lost) and structurally refuses the main tree. Skipped for a lane
//!      mid-iteration (same [`lane_quiet`] gate as the debug sweep).
//!
//! Plus, independent of the 04:00 day gate: a rate-limited **debug sweep** (every
//! [`SWEEP_EVERY_S`]) that deletes FILES under each candidate's `debug/` subdir older than
//! [`RETAIN_DAYS`], then prunes emptied dirs — cargo-sweep style. Active lanes rebuild every few
//! minutes, so a whole-dir staleness rule never fires for them; the file-mtime sweep is what
//! bounds their growth. `release/` is NEVER enumerated (live exes — Asmodeus's trader — run from
//! there; the removed size-cap deleted one on 2026-07-02), and a lane mid-iteration is skipped.
//!
//! NEVER touched: `release/`, `dist/` (live packaged exes — Sover.exe runs from one), `.venv`,
//! `node_modules`, sources, or anything outside the explicit candidate names. Every run appends
//! one honest line to `runtime/_watchdog.out.log`; freeing more than [`NOTIFY_BYTES`] notifies
//! the operator.
#![allow(dead_code)]

use crate::control::{heartbeat, paths, proc, registry};
use crate::notify::{self, Notice};
use chrono::Timelike;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Quiet-hour gate: run once per day, first tick at/after 04:00 local.
const DUE_HOUR: u32 = 4;
/// A build dir untouched this long is abandoned — delete regardless of size.
const STALE_DAYS: i64 = 14;
/// Freeing more than this notifies the operator (low priority).
const NOTIFY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Debug files older than this are dead weight — delete. Hot artifacts (anything rebuilt within
/// the window) keep fresh mtimes and survive, so per-iteration gates stay incremental while
/// storage stays time-bounded (~one full-ish rebuild per repo per week is the whole cost).
const RETAIN_DAYS: i64 = 7;
/// Debug-sweep cadence — the 04:00 day gate alone leaves 24h accumulation windows.
const SWEEP_EVERY_S: u64 = 6 * 3600;
/// A debug dir STILL over this after a sweep is pathological churn — notify the operator, never
/// force-delete (the 2026-07-02 lesson: force-deleting a "quiet" build dir froze live capital).
const DEBUG_SOFT_CAP_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// A remote `<prefix>*` branch with NO open PR is only deleted once its tip commit is older than
/// this — so the sweep can't race a lane that just pushed a branch but hasn't opened its PR yet.
const REMOTE_ORPHAN_MIN_AGE_MIN: i64 = 60;

/// HERE/runtime/_housekeeping.json — {"done": date, "attempts_date": ..., "attempts": n}.
fn state_path() -> PathBuf {
    paths::here().join("runtime").join("_housekeeping.json")
}

/// The watchdog graft: day-gated storage sweep (reuses the CEO rhythm's pure gate helpers).
pub fn tick() {
    // Debug sweep first: every tick, self-rate-limited via its own stamp file — deliberately
    // independent of the 04:00 day gate below (whose early return would starve it for 24h).
    debug_sweep_tick();

    let now = chrono::Local::now();
    let today = now.format("%Y-%m-%d").to_string();
    let st: Value = std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({}));
    if !crate::ceo::should_attempt(&st, &today, now.hour(), DUE_HOUR) {
        return;
    }
    let out = run();
    let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
    let new_st = crate::ceo::record_attempt(&st, &today, ok);
    if let Some(parent) = state_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = proc::atomic_write_json(&state_path(), &new_st);
}

/// One full housekeeping sweep over every managed repo. Total — per-repo failures are recorded
/// as actions, never a panic out of the watchdog thread.
pub fn run() -> Value {
    let mut freed: u64 = 0;
    let mut actions: Vec<String> = Vec::new();
    let mut seen_paths: Vec<String> = Vec::new();

    // ONLY explicitly-registered repos (repos.json), NOT load_repos()'s auto-discovered merge:
    // deleting branches/build dirs from a repo the operator never put under management is a
    // data-safety hazard (bug-bounty cycle 1, conf 78). A repo counts as managed only if it has an
    // explicit repos.json entry — the discovered-dir scan of workspace/projects is excluded.
    for r in registry::read_repos_json() {
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let path = paths::repo_path(&r);
        if path.is_empty() || !Path::new(&path).is_dir() || seen_paths.contains(&path) {
            continue;
        }
        seen_paths.push(path.clone());

        // 1. stale worktree registrations (always safe)
        let _ = proc::run(
            &["git", "-C", &path, "worktree", "prune"],
            None,
            Some(Duration::from_secs(60)),
        );

        // 1b. worktree hygiene: enforce <=1 non-main worktree, freeing orphaned copies + their
        //     nested target/. Skip a lane mid-iteration (a live improver/agent copy must not be
        //     yanked). Safe by construction: only a clean-modulo-ignored tree is removed.
        if lane_quiet(&r) {
            let (n, wfreed) = prune_orphan_worktrees(&r, &path);
            freed += wfreed;
            if n > 0 {
                actions.push(format!("{name}: removed {n} orphan worktree(s) ({})", human(wfreed)));
            }
        }

        // 2. merged improver branches — `git branch -d` refuses unmerged work by design.
        let base = registry::project_pr_target_branch(&r);
        let prefix = r
            .get("branch_prefix")
            .and_then(Value::as_str)
            .unwrap_or("rsi/")
            .to_string();
        let deleted = delete_merged_branches(&path, &base, &prefix);
        if deleted > 0 {
            actions.push(format!("{name}: {deleted} merged {prefix}* branches"));
        }

        // 2b. REMOTE branch cleanup: the auto-merge `--delete-branch` only removes cleanly-merged
        //     PR branches; a PR that didn't land (CI unverifiable / merge unverified / stopped / a
        //     crashed lane) leaves its `<prefix>*` branch on origin forever. Delete the stale ones
        //     (merged into base, or orphaned with no open PR) so they don't pile up on the
        //     (sometimes public) remote. Safe: keeps live-PR branches; content-free deletions.
        let rdeleted = delete_stale_remote_branches(&path, &base, &prefix);
        if rdeleted > 0 {
            actions.push(format!("{name}: {rdeleted} stale remote {prefix}* branch(es)"));
        }

        // 3. build dirs: ONLY delete a dir untouched for STALE_DAYS+ (abandoned = safe). The old
        //    size-cap-on-ACTIVE-dirs path was REMOVED after it deleted the LIVE Asmodeus trader's
        //    binary (2026-07-02): `lane_quiet` only checks the improver heartbeat, so it treated a
        //    15GB target/ as "quiet" and removable while a production Asmodeus.exe was running out
        //    of it — freezing capital. A recent (non-stale) build dir is presumed in use by a lane
        //    OR a production app and is NEVER force-deleted for size; the operator can clean big
        //    active targets manually. Disk stays bounded via stale-cleanup + merged-branch pruning.
        for dir in candidate_build_dirs(Path::new(&path)) {
            if !is_stale(&dir, STALE_DAYS) {
                continue;
            }
            let size = dir_size(&dir);
            if std::fs::remove_dir_all(&dir).is_ok() {
                freed += size;
                actions.push(format!(
                    "{name}: removed {} ({}, stale >{}d)",
                    dir.file_name().and_then(|s| s.to_str()).unwrap_or("?"),
                    human(size),
                    STALE_DAYS,
                ));
            } else {
                actions.push(format!(
                    "{name}: FAILED to remove {} (locked?)",
                    dir.display()
                ));
            }
        }
    }

    let summary = if actions.is_empty() {
        "nothing to clean".to_string()
    } else {
        actions.join("; ")
    };
    append_log(&format!(
        "{} housekeeping: freed {} | {}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        human(freed),
        summary
    ));
    if freed > NOTIFY_BYTES {
        let _ = notify::send(&Notice::plan(
            format!("Solomon housekeeping: freed {}", human(freed)),
            summary.clone(),
        ));
    }
    json!({"ok": true, "freed_bytes": freed, "actions": actions})
}

/// Stamp file whose MTIME is the last debug-sweep time. Deliberately NOT a key in
/// `_housekeeping.json`: `ceo::record_attempt` rewrites that file with only its own keys and
/// would silently wipe ours.
fn sweep_stamp_path() -> PathBuf {
    paths::here().join("runtime").join("_debug_sweep.stamp")
}

/// Rate-limited graft: run the debug sweep at most once per [`SWEEP_EVERY_S`]. The stamp is
/// touched AFTER a completed sweep, so a crash mid-sweep simply retries on the next tick.
fn debug_sweep_tick() {
    if let Ok(m) = std::fs::metadata(sweep_stamp_path()).and_then(|m| m.modified()) {
        // A future mtime (clock skew) makes elapsed() Err — sweep anyway and re-stamp.
        if m.elapsed()
            .map(|e| e.as_secs() < SWEEP_EVERY_S)
            .unwrap_or(false)
        {
            return;
        }
    }
    let _ = run_debug_sweep();
    if let Some(parent) = sweep_stamp_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(sweep_stamp_path(), b"");
}

/// One debug sweep over every managed repo whose lane is quiet: delete debug FILES older than
/// [`RETAIN_DAYS`], prune emptied dirs. `release/` is never enumerated — safety by construction,
/// not by liveness-guessing (the removed size-cap guessed and deleted a live trader binary).
pub fn run_debug_sweep() -> Value {
    let mut freed: u64 = 0;
    let mut skipped_locked = 0usize;
    let mut actions: Vec<String> = Vec::new();
    let mut seen_paths: Vec<String> = Vec::new();
    for r in registry::read_repos_json() {
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            continue;
        }
        let path = paths::repo_path(&r);
        if path.is_empty() || !Path::new(&path).is_dir() || seen_paths.contains(&path) {
            continue;
        }
        seen_paths.push(path.clone());
        // Never sweep under a live compile: a just-deleted rlib mid-build fails that gate run.
        if !lane_quiet(&r) {
            continue;
        }
        for root in debug_roots(Path::new(&path)) {
            let (f, skipped) = sweep_old_files(&root, RETAIN_DAYS);
            freed += f;
            skipped_locked += skipped;
            if f > 0 {
                actions.push(format!("{name}: {} from {}", human(f), root.display()));
            }
            let remaining = dir_size(&root);
            if remaining > DEBUG_SOFT_CAP_BYTES {
                let _ = notify::send(&Notice::plan(
                    format!("Solomon debug-sweep: {name} debug dir over soft cap"),
                    format!(
                        "{} is still {} after sweeping >{}d files — pathological churn; \
                         investigate manually (never force-deleted).",
                        root.display(),
                        human(remaining),
                        RETAIN_DAYS
                    ),
                ));
            }
        }
    }
    let summary = if actions.is_empty() {
        "nothing to sweep".to_string()
    } else {
        actions.join("; ")
    };
    append_log(&format!(
        "{} debug-sweep: freed {} ({} locked kept) | {}",
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"),
        human(freed),
        skipped_locked,
        summary
    ));
    if freed > NOTIFY_BYTES {
        let _ = notify::send(&Notice::plan(
            format!("Solomon debug-sweep: freed {}", human(freed)),
            summary.clone(),
        ));
    }
    json!({"ok": true, "freed_bytes": freed, "skipped_locked": skipped_locked, "actions": actions})
}

/// ONLY the `debug/` subdir of each build-dir candidate. By construction the sweep can never
/// see `release/` (live exes run from there) or `dist/` — the 2026-07-02 incident class.
pub fn debug_roots(repo: &Path) -> Vec<PathBuf> {
    candidate_build_dirs(repo)
        .into_iter()
        .map(|d| d.join("debug"))
        .filter(|d| d.is_dir())
        .collect()
}

/// cargo-sweep style: delete FILES whose mtime is older than `days`, then prune emptied dirs
/// bottom-up (the root itself is kept). Per-file `remove_file` is the in-use probe: Windows
/// refuses to delete an open file (sharing violation), so live artifacts are skipped and
/// counted — fail-safe. Returns (bytes freed, files skipped as locked/undeletable).
///
/// `build/` (a cargo build-script staging dir, `target/debug/build/<pkg>-<hash>/`) is NEVER
/// enumerated — safety by construction, the same stance already taken for `release/`. 2026-07-03
/// INCIDENT: a build script's generated output (e.g. bindgen's `out/bindgen.rs`) is written ONCE
/// and only regenerates if the build script's OWN inputs change — cargo does not re-run it just
/// because time has passed. Its mtime can legitimately sit well past RETAIN_DAYS in an actively
/// rebuilt project while sibling files in the same `out/` dir (the compiled .o/.a/.lib) get
/// refreshed on every build. A pure file-age sweep deleted the generated file but left the crate's
/// cargo fingerprint believing it still existed, breaking that crate's build until `cargo clean -p
/// <pkg>` forced a fresh regen — the fleet's own base gate went red from nothing but a housekeeping
/// sweep. `build/` outputs are cargo's own cache; only `cargo clean` may invalidate them.
pub fn sweep_old_files(root: &Path, days: i64) -> (u64, usize) {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs((days as u64) * 86_400);
    sweep_dir(root, cutoff, false)
}

fn sweep_dir(dir: &Path, cutoff: std::time::SystemTime, remove_self: bool) -> (u64, usize) {
    let mut freed = 0u64;
    let mut skipped = 0usize;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                if e.file_name() == "build" {
                    continue; // cargo build-script staging dir — never enumerated, see doc comment
                }
                let (f, s) = sweep_dir(&e.path(), cutoff, true);
                freed += f;
                skipped += s;
            } else if m.modified().map(|t| t < cutoff).unwrap_or(false) {
                // unreadable mtime -> keep (fail safe), same stance as is_stale()
                let len = m.len();
                if std::fs::remove_file(e.path()).is_ok() {
                    freed += len;
                } else {
                    skipped += 1;
                }
            }
        }
    }
    if remove_self {
        // remove_dir only succeeds on an empty dir — non-empty fails silently, which is right.
        let _ = std::fs::remove_dir(dir);
    }
    (freed, skipped)
}

/// The explicit build-dir candidates for a repo root: top-level `target*` / `build`, plus the
/// Tauri workspace's `src-tauri/target`. Nothing else — dist/, .venv, node_modules are live.
pub fn candidate_build_dirs(repo: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(repo) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && (name.starts_with("target") || name == "build") {
                out.push(e.path());
            }
        }
    }
    let tauri_target = repo.join("src-tauri").join("target");
    if tauri_target.is_dir() {
        out.push(tauri_target);
    }
    out
}

/// Normalize a path for comparison (porcelain emits `/`, repos.json stores `\`; Windows is
/// case-insensitive): trim, `\`->`/`, drop a trailing `/`, lowercase.
fn norm_path(p: &str) -> String {
    p.trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_lowercase()
}

/// Enforce the operator rule "<=1 worktree besides main/master": remove every NON-main, NON-locked
/// worktree that carries NO real work, freeing the throwaway copy + its nested `target/` (which
/// [`candidate_build_dirs`] on the repo root never enumerates). Called only for a quiet lane.
///
/// SAFETY (why `--force` is safe here): the "real work" test is `git status --porcelain`, which
/// RESPECTS `.gitignore` — an ignored `target/` does NOT appear, but a modified tracked file or an
/// untracked SOURCE file DOES. A NON-EMPTY status ⇒ in-flight work ⇒ the worktree is KEPT. Only a
/// clean-modulo-ignored tree is removed, and `--force` there deletes nothing but the ignored build
/// output. git keeps the branch ref (no commit is ever lost) and structurally refuses the main tree.
/// Reuses [`crate::control::branches::list_worktrees`] rather than a second porcelain parser.
/// Returns (worktrees removed, bytes freed).
pub fn prune_orphan_worktrees(repo: &Value, main_path: &str) -> (usize, u64) {
    let root = norm_path(main_path);
    let mut removed = 0usize;
    let mut freed = 0u64;
    for wt in crate::control::branches::list_worktrees(repo) {
        let p = wt.get("path").and_then(Value::as_str).unwrap_or("");
        if p.is_empty() || norm_path(p) == root {
            continue; // never the main checkout
        }
        if wt.get("locked").and_then(Value::as_bool).unwrap_or(false) {
            continue; // git-locked (an active session marked it) — leave it
        }
        // Real-work guard. `status --porcelain` respects .gitignore, so an ignored target/ is
        // invisible; a non-empty result is modified-tracked or untracked-SOURCE work -> KEEP.
        // Unreadable -> treat as dirty -> keep (fail safe).
        let clean = match proc::run(
            &["git", "-C", p, "status", "--porcelain"],
            None,
            Some(Duration::from_secs(60)),
        ) {
            Ok(r) if r.ok() => r.stdout.trim().is_empty(),
            _ => false,
        };
        if !clean {
            continue;
        }
        // Size the nested build dirs BEFORE removal (they vanish with the working copy).
        let nested: u64 = candidate_build_dirs(Path::new(p))
            .iter()
            .map(|d| dir_size(d))
            .sum();
        let ok = proc::run(
            &["git", "-C", main_path, "worktree", "remove", "--force", p],
            None,
            Some(Duration::from_secs(120)),
        )
        .map(|r| r.ok())
        .unwrap_or(false);
        if ok {
            removed += 1;
            freed += nested;
        }
    }
    if removed > 0 {
        let _ = proc::run(
            &["git", "-C", main_path, "worktree", "prune"],
            None,
            Some(Duration::from_secs(60)),
        );
    }
    (removed, freed)
}

/// True when the dir's newest SHALLOW mtime (the dir itself + two levels of entries — cheap, no
/// full walk) is older than `days`. A fresh compile touches target/debug or target/release, so
/// two levels always see activity.
pub fn is_stale(dir: &Path, days: i64) -> bool {
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs((days as u64) * 86_400);
    newest_shallow_mtime(dir, 2)
        .map(|t| t < cutoff)
        .unwrap_or(false) // unreadable -> not stale (fail safe: keep)
}

fn newest_shallow_mtime(dir: &Path, depth: u32) -> Option<std::time::SystemTime> {
    let mut newest = std::fs::metadata(dir).and_then(|m| m.modified()).ok()?;
    if depth == 0 {
        return Some(newest);
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if let Ok(t) = m.modified() {
                    if t > newest {
                        newest = t;
                    }
                }
                if m.is_dir() {
                    if let Some(t) = newest_shallow_mtime(&e.path(), depth - 1) {
                        if t > newest {
                            newest = t;
                        }
                    }
                }
            }
        }
    }
    Some(newest)
}

/// Recursive size (full walk — run at most once daily, on the background tick thread).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_dir() {
                    total += dir_size(&e.path());
                } else {
                    total += m.len();
                }
            }
        }
    }
    total
}

/// Delete local `<prefix>*` branches fully merged into `base`. Returns the count deleted.
/// `git branch -d` (lowercase) refuses unmerged branches — stranded experiments survive.
pub fn delete_merged_branches(path: &str, base: &str, prefix: &str) -> usize {
    let merged = match proc::run(
        &[
            "git",
            "-C",
            path,
            "branch",
            "--merged",
            base,
            "--format=%(refname:short)",
        ],
        None,
        Some(Duration::from_secs(60)),
    ) {
        Ok(r) if r.ok() => r.stdout,
        _ => return 0,
    };
    let mut n = 0;
    for b in merged.lines().map(str::trim) {
        if b.is_empty() || b == base || !b.starts_with(prefix) {
            continue;
        }
        if let Ok(r) = proc::run(
            &["git", "-C", path, "branch", "-d", b],
            None,
            Some(Duration::from_secs(30)),
        ) {
            if r.ok() {
                n += 1;
            }
        }
    }
    n
}

/// Pure keep/delete decision for one remote `<prefix>*` branch (factored out for testing without a
/// live git/gh). A branch is deleted iff:
///   * it is MERGED into the base (the work landed — always safe), OR
///   * PR state is KNOWN (`pr_ok`), it has NO open PR, AND its tip is at least `min_age_secs` old
///     (an orphaned/abandoned iteration branch; the age floor avoids racing a just-pushed branch).
/// A live open PR, or an unknown PR state (gh unreachable) on an unmerged branch, always KEEPS it.
fn remote_branch_should_delete(
    is_merged: bool,
    has_open_pr: bool,
    pr_ok: bool,
    age_secs: i64,
    min_age_secs: i64,
) -> bool {
    if is_merged {
        return true;
    }
    if !pr_ok || has_open_pr {
        return false;
    }
    age_secs >= min_age_secs
}

/// Head-ref branch names of this repo's OPEN PRs (via `gh pr list`), plus whether the query
/// SUCCEEDED. `(set, false)` on any gh failure/unauth — callers must then treat an unknown PR
/// state as "cannot safely delete an unmerged branch". `gh` resolves the repo from `path`'s remote.
fn open_pr_heads(path: &str) -> (std::collections::HashSet<String>, bool) {
    let r = proc::run(
        &["gh", "pr", "list", "--state", "open", "--json", "headRefName", "--limit", "500"],
        Some(Path::new(path)),
        Some(Duration::from_secs(120)),
    );
    match r {
        Ok(o) if o.ok() => {
            let raw = o.stdout.trim();
            let parsed: Value =
                serde_json::from_str(if raw.is_empty() { "[]" } else { raw }).unwrap_or_else(|_| json!([]));
            let set = parsed
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("headRefName").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            (set, true)
        }
        _ => (std::collections::HashSet::new(), false),
    }
}

/// Delete STALE REMOTE `<prefix>*` branches on origin so the fleet's iteration branches don't pile
/// up on the (sometimes public) remote. Companion to [`delete_merged_branches`], which only prunes
/// LOCAL branches — nothing previously cleaned the remote, so a PR that didn't cleanly auto-merge
/// (CI unverifiable / merge unverified / stopped / a crashed lane) left its `<prefix>*` branch on
/// origin forever. A remote branch is deleted per [`remote_branch_should_delete`]: merged into
/// `origin/<base>`, or an orphan with no open PR older than [`REMOTE_ORPHAN_MIN_AGE_MIN`].
///
/// The delete uses `git push --no-verify`: a ref DELETION carries no content, so it is safe even
/// under a brand-safety pre-push hook (e.g. projects/sover's public-repo guard that fail-closes on
/// EVERY push). `<base>` and any non-`<prefix>` branch are never touched. Returns branches deleted.
pub fn delete_stale_remote_branches(path: &str, base: &str, prefix: &str) -> usize {
    // origin required.
    let has_origin = proc::run(
        &["git", "-C", path, "remote", "get-url", "origin"],
        None,
        Some(Duration::from_secs(30)),
    )
    .map(|r| r.ok())
    .unwrap_or(false);
    if !has_origin {
        return 0;
    }
    // Sync remote-tracking refs + drop refs already deleted on origin.
    let _ = proc::run(
        &["git", "-C", path, "fetch", "origin", "--prune", "--quiet"],
        None,
        Some(Duration::from_secs(180)),
    );
    // Remote `<prefix>*` branches as remote-tracking short names (`origin/<prefix>...`).
    let listed = match proc::run(
        &[
            "git", "-C", path, "branch", "-r", "--list",
            &format!("origin/{prefix}*"), "--format=%(refname:short)",
        ],
        None,
        Some(Duration::from_secs(60)),
    ) {
        Ok(r) if r.ok() => r.stdout,
        _ => return 0,
    };
    let origin_prefix = format!("origin/{prefix}");
    let remote_branches: Vec<String> = listed
        .lines()
        .map(str::trim)
        .filter(|b| b.starts_with(&origin_prefix))
        .map(str::to_string)
        .collect();
    if remote_branches.is_empty() {
        return 0;
    }
    // Merged-into-base remote-tracking branches (safe to delete regardless of PR state).
    let merged: std::collections::HashSet<String> = match proc::run(
        &[
            "git", "-C", path, "branch", "-r", "--merged",
            &format!("origin/{base}"), "--format=%(refname:short)",
        ],
        None,
        Some(Duration::from_secs(60)),
    ) {
        Ok(r) if r.ok() => r.stdout.lines().map(|s| s.trim().to_string()).collect(),
        _ => std::collections::HashSet::new(),
    };
    // Open-PR head branches (kept). gh failure => pr_ok=false (only MERGED branches deleted).
    let (open_heads, pr_ok) = open_pr_heads(path);
    let now = chrono::Utc::now().timestamp();
    let min_age = REMOTE_ORPHAN_MIN_AGE_MIN * 60;
    let mut n = 0;
    for rb in &remote_branches {
        let short = match rb.strip_prefix("origin/") {
            Some(s) => s,
            None => continue,
        };
        if short == base {
            continue;
        }
        let is_merged = merged.contains(rb);
        let has_open_pr = open_heads.contains(short);
        // Tip age only matters for the orphan case; skip the git call otherwise. A parse/git
        // failure yields age 0 (< min_age) → the branch is KEPT (fail-safe).
        let age_secs = if is_merged || !pr_ok || has_open_pr {
            0
        } else {
            match proc::run(
                &["git", "-C", path, "log", "-1", "--format=%ct", rb],
                None,
                Some(Duration::from_secs(30)),
            ) {
                Ok(r) if r.ok() => r.stdout.trim().parse::<i64>().map(|ct| now - ct).unwrap_or(0),
                _ => 0,
            }
        };
        if !remote_branch_should_delete(is_merged, has_open_pr, pr_ok, age_secs, min_age) {
            continue;
        }
        // A ref deletion carries no content -> --no-verify is safe even under a brand-safety
        // pre-push hook (sover's public-repo guard). Never touches base or non-prefix branches.
        if let Ok(r) = proc::run(
            &["git", "-C", path, "push", "--no-verify", "origin", "--delete", short],
            None,
            Some(Duration::from_secs(60)),
        ) {
            if r.ok() {
                n += 1;
            }
        }
    }
    n
}

/// Quiet = safe to delete this repo's build dir: heartbeat sleeping/idle/stopped/absent — never
/// while iterating (a delete under a live compile corrupts the iteration).
fn lane_quiet(r: &Value) -> bool {
    let hb = heartbeat::read_heartbeat(r).unwrap_or(Value::Null);
    matches!(
        hb.get("status").and_then(Value::as_str),
        None | Some("") | Some("sleeping") | Some("idle") | Some("stopped")
    )
}

fn human(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

/// Append one line to runtime/_watchdog.out.log (OSError -> pass, house contract).
fn append_log(line: &str) {
    let _ = (|| -> std::io::Result<()> {
        use std::io::Write;
        let p = paths::here().join("runtime").join("_watchdog.out.log");
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        writeln!(f, "{line}")?;
        Ok(())
    })();
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- remote-branch cleanup decision (delete_stale_remote_branches core logic) ----
    const MIN_AGE: i64 = REMOTE_ORPHAN_MIN_AGE_MIN * 60;

    #[test]
    fn remote_merged_branch_always_deleted() {
        // Merged into base -> deleted regardless of PR state / age (the work landed).
        assert!(remote_branch_should_delete(true, false, true, 0, MIN_AGE));
        assert!(remote_branch_should_delete(true, true, false, 0, MIN_AGE)); // even an open PR / gh-down
    }

    #[test]
    fn remote_orphan_old_no_pr_deleted() {
        // Not merged, PR state known, no open PR, tip older than the age floor -> orphan -> delete.
        assert!(remote_branch_should_delete(false, false, true, MIN_AGE, MIN_AGE));
        assert!(remote_branch_should_delete(false, false, true, MIN_AGE + 1, MIN_AGE));
    }

    #[test]
    fn remote_orphan_young_no_pr_kept() {
        // A just-pushed branch (younger than the floor) with no PR yet must NOT be deleted (race).
        assert!(!remote_branch_should_delete(false, false, true, MIN_AGE - 1, MIN_AGE));
        assert!(!remote_branch_should_delete(false, false, true, 0, MIN_AGE));
    }

    #[test]
    fn remote_open_pr_branch_kept() {
        // A live open PR may still land -> keep regardless of age.
        assert!(!remote_branch_should_delete(false, true, true, MIN_AGE * 10, MIN_AGE));
    }

    #[test]
    fn remote_unknown_pr_state_keeps_unmerged() {
        // gh unreachable (pr_ok=false): an unmerged branch's PR state is unknown -> never delete
        // (must not delete a possibly-in-flight branch); only merged branches go in that case.
        assert!(!remote_branch_should_delete(false, false, false, MIN_AGE * 10, MIN_AGE));
        assert!(remote_branch_should_delete(true, false, false, 0, MIN_AGE)); // merged still deleted
    }

    fn temp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "solomon_hk_{}_{}_{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() % 1_000_000)
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    // -------- staleness (fail-safe: fresh or unreadable keeps the dir) --------
    #[test]
    fn is_stale_vectors() {
        let d = temp("stale");
        std::fs::create_dir_all(d.join("debug")).unwrap();
        std::fs::write(d.join("debug").join("x.o"), b"x").unwrap();
        // just-written -> NOT stale
        assert!(!is_stale(&d, 14));
        // backdate everything shallow-visible -> stale
        let old =
            filetime::FileTime::from_unix_time((chrono::Utc::now().timestamp()) - 20 * 86_400, 0);
        for p in [d.clone(), d.join("debug"), d.join("debug").join("x.o")] {
            filetime::set_file_mtime(&p, old).unwrap();
        }
        assert!(is_stale(&d, 14));
        // missing dir -> not stale (never delete on uncertainty)
        assert!(!is_stale(Path::new("Z:/definitely/absent"), 14));
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- candidate selection: only explicit build names, never dist/.venv --------
    #[test]
    fn candidate_build_dirs_only_build_names() {
        let d = temp("cands");
        for n in ["target", "target-codex", "build", "dist", ".venv", "src"] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        std::fs::create_dir_all(d.join("src-tauri").join("target")).unwrap();
        let got: Vec<String> = candidate_build_dirs(&d)
            .into_iter()
            .map(|p| {
                p.strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(got.contains(&"target".to_string()));
        assert!(got.contains(&"target-codex".to_string()));
        assert!(got.contains(&"build".to_string()));
        assert!(got.contains(&"src-tauri/target".to_string()));
        assert!(!got
            .iter()
            .any(|g| g.contains("dist") || g.contains(".venv") || g == "src"));
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- merged-branch cleanup keeps unmerged work (git branch -d refusal) --------
    #[test]
    fn delete_merged_branches_keeps_unmerged() {
        use std::process::Command;
        let d = temp("git");
        let git = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(&d)
                .status()
                .unwrap();
            assert!(st.success(), "git {args:?}");
        };
        git(&["init"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        git(&["commit", "--allow-empty", "-m", "init"]);
        git(&["branch", "-M", "main"]);
        // merged branch: no extra commits -> already contained in main
        git(&["branch", "rsi/merged"]);
        // unmerged branch: one commit ahead
        git(&["checkout", "-b", "rsi/unmerged"]);
        git(&["commit", "--allow-empty", "-m", "ahead"]);
        git(&["checkout", "main"]);

        let n = delete_merged_branches(&d.to_string_lossy(), "main", "rsi/");
        assert_eq!(n, 1, "exactly the merged branch is deleted");
        let out = Command::new("git")
            .args(["branch", "--list"])
            .current_dir(&d)
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(!branches.contains("rsi/merged"));
        assert!(
            branches.contains("rsi/unmerged"),
            "unmerged experiment survives"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- debug sweep: release/ structurally unreachable (2026-07-02 incident class) -----
    #[test]
    fn debug_roots_never_include_release() {
        let d = temp("droots");
        for n in [
            "target/debug",
            "target/release",
            "target-codex/debug",
            "build/debug",
            "dist",
        ] {
            std::fs::create_dir_all(d.join(n)).unwrap();
        }
        std::fs::create_dir_all(d.join("src-tauri").join("target").join("debug")).unwrap();
        std::fs::create_dir_all(d.join("src-tauri").join("target").join("release")).unwrap();
        let got: Vec<String> = debug_roots(&d)
            .into_iter()
            .map(|p| {
                p.strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(got.contains(&"target/debug".to_string()));
        assert!(got.contains(&"target-codex/debug".to_string()));
        assert!(got.contains(&"build/debug".to_string()));
        assert!(got.contains(&"src-tauri/target/debug".to_string()));
        for g in &got {
            assert!(
                !g.contains("release"),
                "release/ must be unreachable, got {g}"
            );
            assert!(!g.contains("dist"), "dist/ must be unreachable, got {g}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- debug sweep: old files go, fresh survive, emptied dirs pruned, root kept --------
    #[test]
    fn sweep_old_files_deletes_old_keeps_fresh_prunes_empty() {
        let d = temp("sweep");
        let debug = d.join("debug");
        std::fs::create_dir_all(debug.join("deps")).unwrap();
        std::fs::create_dir_all(debug.join("incremental").join("sess")).unwrap();
        std::fs::write(debug.join("deps").join("old.rlib"), vec![0u8; 100]).unwrap();
        std::fs::write(
            debug.join("incremental").join("sess").join("old.o"),
            vec![0u8; 50],
        )
        .unwrap();
        std::fs::write(debug.join("deps").join("new.rlib"), vec![0u8; 10]).unwrap();
        let old =
            filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - 10 * 86_400, 0);
        for p in [
            debug.join("deps").join("old.rlib"),
            debug.join("incremental").join("sess").join("old.o"),
        ] {
            filetime::set_file_mtime(&p, old).unwrap();
        }

        let (freed, skipped) = sweep_old_files(&debug, 7);

        assert_eq!(freed, 150, "freed bytes = the two old files only");
        assert_eq!(skipped, 0);
        assert!(!debug.join("deps").join("old.rlib").exists());
        assert!(
            debug.join("deps").join("new.rlib").exists(),
            "fresh artifact survives"
        );
        assert!(
            !debug.join("incremental").exists(),
            "emptied dirs pruned bottom-up"
        );
        assert!(debug.join("deps").exists(), "non-empty dir survives");
        assert!(debug.exists(), "the sweep root itself is kept");
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- 2026-07-03 incident: build/ (cargo build-script staging) is never enumerated --------
    #[test]
    fn sweep_old_files_never_touches_build_dir_even_when_ancient() {
        // Regression guard: a build script's generated output (e.g. bindgen's out/bindgen.rs) can
        // legitimately sit past RETAIN_DAYS without being stale — cargo only regenerates it if the
        // build script's own inputs change. Deleting it out from under cargo's fingerprint cache
        // broke the crate's build with no code change at all.
        let d = temp("sweep_build_guard");
        let debug = d.join("debug");
        let out = debug.join("build").join("libsqlite3-sys-abc123").join("out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("bindgen.rs"), vec![0u8; 200]).unwrap();
        std::fs::write(out.join("sqlite3.o"), vec![0u8; 300]).unwrap();
        // Everything ancient — if build/ were swept like any other dir, all of this would go.
        let ancient =
            filetime::FileTime::from_unix_time(chrono::Utc::now().timestamp() - 365 * 86_400, 0);
        for p in [out.join("bindgen.rs"), out.join("sqlite3.o")] {
            filetime::set_file_mtime(&p, ancient).unwrap();
        }
        std::fs::create_dir_all(debug.join("deps")).unwrap();
        std::fs::write(debug.join("deps").join("old.rlib"), vec![0u8; 40]).unwrap();
        filetime::set_file_mtime(debug.join("deps").join("old.rlib"), ancient).unwrap();

        let (freed, _skipped) = sweep_old_files(&debug, 7);

        assert!(out.join("bindgen.rs").exists(), "build-script output must survive untouched");
        assert!(out.join("sqlite3.o").exists(), "build-script output must survive untouched");
        assert!(!debug.join("deps").join("old.rlib").exists(), "non-build/ ancient files still swept");
        assert_eq!(freed, 40, "freed count reflects only the swept non-build/ file");
        let _ = std::fs::remove_dir_all(&d);
    }

    // -------- worktree hygiene: removes a clean orphan (ignored target/ only), KEEPS a worktree
    //          with untracked source, KEEPS main, and the removed branch's ref survives --------
    #[test]
    fn prune_orphan_worktrees_removes_clean_keeps_dirty() {
        use std::process::Command;
        let base = temp("wt");
        let main = base.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(cwd)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        git(&main, &["init", "-q"]);
        git(&main, &["config", "user.email", "t@t"]);
        git(&main, &["config", "user.name", "t"]);
        std::fs::write(main.join(".gitignore"), "target/\n").unwrap();
        git(&main, &["add", "."]);
        git(&main, &["commit", "-q", "-m", "init"]);
        git(&main, &["branch", "-M", "main"]);
        // clean orphan: a committed worktree whose ONLY dirt is an ignored target/ (untracked build).
        let clean = base.join("wt-clean");
        git(
            &main,
            &["worktree", "add", "-q", clean.to_str().unwrap(), "-b", "rsi/iter-clean"],
        );
        std::fs::create_dir_all(clean.join("target").join("debug")).unwrap();
        std::fs::write(clean.join("target").join("debug").join("x.o"), vec![0u8; 2048]).unwrap();
        // dirty worktree: an UNTRACKED SOURCE file (not ignored) = real in-flight work -> must KEEP.
        let dirty = base.join("wt-dirty");
        git(
            &main,
            &["worktree", "add", "-q", dirty.to_str().unwrap(), "-b", "rsi/iter-dirty"],
        );
        std::fs::write(dirty.join("newwork.rs"), b"fn f() {}").unwrap();

        let repo = serde_json::json!({ "name": "wttest", "path": main.to_string_lossy() });
        let (removed, freed) = prune_orphan_worktrees(&repo, &main.to_string_lossy());

        assert_eq!(removed, 1, "only the clean orphan removed");
        assert!(freed >= 2048, "removed orphan's nested target/ counted, got {freed}");
        assert!(!clean.exists(), "clean orphan gone");
        assert!(dirty.exists(), "worktree with untracked SOURCE kept");
        assert!(main.exists(), "main checkout never removed");
        // the removed worktree's branch ref survives (no commit lost).
        assert!(
            Command::new("git")
                .args(["rev-parse", "--verify", "rsi/iter-clean"])
                .current_dir(&main)
                .status()
                .unwrap()
                .success(),
            "removed worktree's branch ref survives"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // -------- dir_size + human --------
    #[test]
    fn dir_size_and_human() {
        let d = temp("size");
        std::fs::write(d.join("a.bin"), vec![0u8; 2048]).unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.bin"), vec![0u8; 1024]).unwrap();
        assert_eq!(dir_size(&d), 3072);
        assert_eq!(human(3 * 1024 * 1024), "3 MB");
        assert_eq!(human(2 * 1024 * 1024 * 1024), "2.0 GB");
        let _ = std::fs::remove_dir_all(&d);
    }
}
