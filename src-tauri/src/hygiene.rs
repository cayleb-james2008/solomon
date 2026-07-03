//! Repo-hygiene detection — REPORT-ONLY, never destructive.
//!
//! This module observes managed-repo git state and classifies problems; it NEVER
//! mutates a working tree. It deliberately imports NO destructive helper
//! (reset_to_base / clean_branch / git reset|clean|stash) — per Solomon's keystone
//! invariant, growth/fixes flow only through the improver backlog or orchestrator dials.
//!
//! It exists to catch the gap `branches::branch_hygiene.off_base` misses: that field
//! only flags branches under the `rsi/` prefix, so a repo stranded on `codex/*`
//! (the asmodeus case) reads clean. `classify` here flags ANY non-base branch.
//!
//! Why `git status --porcelain --untracked-files=no` is the entire live-app safety
//! story: every false-flag risk is UNtracked/ignored and excluded by `-uno` —
//! asmodeus's %LOCALAPPDATA% writes, daedulus's untracked `runs/`, asmodeus's
//! untracked `.agents/`, and any live_app gitignored in-repo write. With `-uno`,
//! ONLY a modification to a committed, tracked file flags — which is a real problem
//! even for a live_app. No live_app special-case is needed.

use crate::control::{branches, paths, proc};
use serde_json::Value;
use std::time::Duration;

/// A single report-only hygiene problem found in a managed repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HygieneIssue {
    /// A committed, tracked file has uncommitted modifications.
    Dirty,
    /// HEAD is on a non-base branch (any prefix, not just `rsi/`).
    OffBase,
}

impl HygieneIssue {
    pub fn slug(&self) -> &'static str {
        match self {
            HygieneIssue::Dirty => "dirty",
            HygieneIssue::OffBase => "off_base",
        }
    }
}

/// PURE classifier. In-flight rsi loops are exempt (`running` => empty). Otherwise:
/// tracked_dirty => Dirty; a resolvable branch that differs from a resolvable base
/// (ANY prefix) => OffBase.
pub fn classify(current: &str, base: &str, tracked_dirty: bool, running: bool) -> Vec<HygieneIssue> {
    if running {
        return Vec::new();
    }
    let mut issues = Vec::new();
    if tracked_dirty {
        issues.push(HygieneIssue::Dirty);
    }
    if !current.is_empty() && !base.is_empty() && current != base {
        issues.push(HygieneIssue::OffBase);
    }
    issues
}

/// Timeout for the `git status` probe below. This runs once per managed repo on EVERY 2-min
/// watchdog tick on the tick's own thread — an unbounded call here (a held git index lock, disk
/// contention from a concurrent cargo build, a stalled credential-helper) freezes the entire fleet
/// (crash-restart, heartbeat collection, standstill alarm — everything) for as long as it hangs.
/// 30s is ample for a local `git status --porcelain` and fails safe (see `tracked_dirty_for`).
const GIT_STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// `git -C <path> status --porcelain --untracked-files=no` -> any non-empty stdout means
/// a tracked file is modified. Fail-safe: ANY error (no git, empty path, OSError/timeout)
/// returns false so hygiene never false-alarms on an unreadable repo.
fn tracked_dirty_for(repo: &Value) -> bool {
    let git = match proc::which_git() {
        Some(g) => g,
        None => return false,
    };
    let path = paths::repo_path(repo);
    if path.is_empty() {
        return false;
    }
    let git = git.to_string_lossy();
    let argv = [
        git.as_ref(),
        "-C",
        path.as_str(),
        "status",
        "--porcelain",
        "--untracked-files=no",
    ];
    match proc::run(&argv, None, Some(GIT_STATUS_TIMEOUT)) {
        Ok(r) => !r.stdout.trim().is_empty(),
        Err(_) => false,
    }
}

/// IO wrapper: read current/base/running from the read-only `branch_hygiene` snapshot,
/// compute tracked_dirty, and return (classify(...), the snapshot).
pub fn scan_repo(repo: &Value) -> (Vec<HygieneIssue>, Value) {
    let hyg = branches::branch_hygiene(repo);
    let current = hyg.get("current").and_then(Value::as_str).unwrap_or("");
    let base = hyg.get("base").and_then(Value::as_str).unwrap_or("");
    let running = hyg.get("running").and_then(Value::as_bool).unwrap_or(false);
    // In-flight rsi loops are exempt — short-circuit BEFORE the git-status subprocess so a running
    // lane costs no extra work on the hot 2-min tick (mirrors classify's own `running` early-exit).
    if running {
        return (Vec::new(), hyg);
    }
    let tracked_dirty = tracked_dirty_for(repo);
    let issues = classify(current, base, tracked_dirty, running);
    (issues, hyg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_strings_are_exact() {
        assert_eq!(HygieneIssue::Dirty.slug(), "dirty");
        assert_eq!(HygieneIssue::OffBase.slug(), "off_base");
    }

    #[test]
    fn classify_vectors() {
        use HygieneIssue::*;
        // clean on base
        assert_eq!(classify("main", "main", false, false), vec![]);
        // dirty tracked file, on base
        assert_eq!(classify("main", "main", true, false), vec![Dirty]);
        // stranded on codex/* — the asmodeus key case branch_hygiene.off_base misses
        assert_eq!(classify("codex/x", "main", false, false), vec![OffBase]);
        // both problems at once, order Dirty then OffBase
        assert_eq!(classify("codex/x", "main", true, false), vec![Dirty, OffBase]);
        // in-flight rsi loop is exempt even when dirty + off base
        assert_eq!(classify("rsi/iter-3", "main", true, true), vec![]);
        // unresolvable current branch: dirty only, no off_base
        assert_eq!(classify("", "", true, false), vec![Dirty]);
        // empty base fail-safe: no off_base without a known base
        assert_eq!(classify("main", "", false, false), vec![]);
    }
}
