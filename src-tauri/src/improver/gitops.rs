//! Native Rust port of the git-operations layer of improver/run_improver.py — the preflight clean,
//! branch lifecycle, and per-repo config readers that gate what may reach a published repo.
//!
//! Bug-for-bug with run_improver.py. Every git/gh call goes through `ctx.git(...)` (cwd = the target
//! repo). The agent-artifact recovery heuristic, the dirty-tree predicates, and the public-repo leak
//! guards are ported with their CONSERVATIVE design intact (a false positive destroys operator work).
//!
//! Python `re.match` anchors at the START of the string only (not the end); the artifact patterns
//! supply their own END anchor (`$` / `\Z`). The Rust `regex` crate has no direct `re.match`, so
//! `pattern_match` compiles each pattern unanchored and checks that a match begins at offset 0,
//! reproducing Python's start-anchored semantics exactly.

use crate::improver::ctx::{self, Ctx};
use crate::improver::escalation;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

// --------------------------------------------------------------------------- #
// agent-artifact patterns (run_improver._AGENT_ARTIFACT_PATTERNS / _ARTIFACT_CANARY_PATHS)
// --------------------------------------------------------------------------- #

/// run_improver._AGENT_ARTIFACT_PATTERNS — the CONSERVATIVE global heuristic for a dead run's OWN
/// untracked debris (never operator work). Each is start-anchored via `^` AND end-anchored, matched
/// against the repo-relative POSIX path. Compiled once.
fn global_artifact_patterns() -> &'static [Regex] {
    static PATS: OnceLock<Vec<Regex>> = OnceLock::new();
    PATS.get_or_init(|| {
        vec![
            Regex::new(r"^AGENT_LOG\.md$").unwrap(), // the agent's per-run log (exact name)
            Regex::new(r"^capabilities/[^/]+(?:/.+)?$").unwrap(), // capabilities/<name>[/<anything>]
            Regex::new(r"^profiles/[^/]+(?:/.+)?$").unwrap(), // profiles/<id>[/<anything>]
            Regex::new(r"^start_[^/]+\.sh$").unwrap(), // start_<id>.sh launcher script
            Regex::new(r"^\.agent_artifacts/.+").unwrap(), // the explicit operator-opted-in sentinel dir
        ]
    })
    .as_slice()
}

/// run_improver._ARTIFACT_CANARY_PATHS — canonical operator files that must NEVER be classified as
/// agent artifacts; a per-repo `agent_artifacts` entry matching ANY of these is rejected as too broad.
const ARTIFACT_CANARY_PATHS: &[&str] = &[
    "README.md",
    "main.py",
    "app.py",
    "setup.py",
    "pyproject.toml",
    "src/app.py",
    "tests/test_x.py",
    "index.js",
    "package.json",
    "notes.txt",
];

/// Python `pat.match(s)` semantics: a match that begins at offset 0 (start-anchored, not end-anchored).
/// The patterns carry their own end anchors; `regex::find` is unanchored, so we require `start()==0`.
fn pattern_match(pat: &Regex, s: &str) -> bool {
    matches!(pat.find(s), Some(m) if m.start() == 0)
}

// --------------------------------------------------------------------------- #
// dirty-tree / sha primitives
// --------------------------------------------------------------------------- #

/// run_improver.tree_dirty (~732-736): True if there are uncommitted changes to TRACKED files
/// (work the loop must not clobber). Untracked files are NOT counted (untracked_non_ignored_files
/// guards those separately).
pub fn tree_dirty(ctx: &Ctx) -> bool {
    !ctx.git(&["status", "--porcelain", "--untracked-files=no"], 120)
        .stdout
        .trim()
        .is_empty()
}

/// run_improver._untracked_non_ignored_files (~739-748): non-ignored UNTRACKED files (`??` entries)
/// in the working tree, each stripped of the leading `"?? "` and trailing whitespace. Pure
/// (delegates to git()) so the guard rule is unit-testable.
pub fn untracked_non_ignored_files(ctx: &Ctx) -> Vec<String> {
    // `-z` gives NUL-terminated entries with the path VERBATIM (never git-quoted/octal-escaped).
    // Without it, git's default core.quotePath double-quotes + octal-escapes non-ASCII/space names
    // (e.g. `?? "w\303\251ird.txt"`), so the stored string would carry literal surrounding quotes
    // and \NNN escapes — mis-classifying a genuine agent artifact with such a name as operator work.
    let out = ctx
        .git(
            &["status", "--porcelain", "-z", "--untracked-files=normal"],
            120,
        )
        .stdout;
    let mut files = Vec::new();
    for entry in out.split('\0') {
        // Each `??` entry is exactly `?? <path>` (2 status chars + 1 space, then the literal path).
        if let Some(rest) = entry.strip_prefix("?? ") {
            if !rest.is_empty() {
                files.push(rest.to_string());
            }
        }
    }
    files
}

/// run_improver.head_sha (~919-920): `git rev-parse HEAD` stdout, stripped.
pub fn head_sha(ctx: &Ctx) -> String {
    ctx.git(&["rev-parse", "HEAD"], 120).stdout.trim().to_string()
}

// --------------------------------------------------------------------------- #
// agent-artifact classification (pure)
// --------------------------------------------------------------------------- #

/// run_improver._compile_artifact_pattern (~780-808): compile ONE operator-supplied `agent_artifacts`
/// entry to a fully-anchored regex matched the same way as the global patterns (`.match` vs the POSIX
/// path). A glob metachar (`*`/`?`) routes through an fnmatch translation; any other entry is treated
/// as a regex. Both forms are END-anchored (`\Z`/`$`). Returns None for an empty/uncompilable entry,
/// OR for a too-broad entry that would match a canonical operator file.
pub fn compile_artifact_pattern(spec: &str) -> Option<Regex> {
    let s = spec.trim();
    if s.is_empty() {
        return None;
    }
    let pat: Regex = if s.contains('*') || s.contains('?') {
        // glob: normalize separators to POSIX, drop a trailing slash, translate via fnmatch (end-anchored)
        let normalized = s.replace('\\', "/");
        let normalized = normalized.trim_end_matches('/');
        match Regex::new(&fnmatch_translate(normalized)) {
            Ok(p) => p,
            Err(_) => return None, // re.error -> None
        }
    } else {
        // regex: end-anchor for parity with the global patterns (strip a trailing '$', add \Z == \z)
        let body = s.strip_suffix('$').unwrap_or(s);
        match Regex::new(&format!(r"{body}\z")) {
            Ok(p) => p,
            Err(_) => return None,
        }
    };
    // breadth guard: reject a pattern that matches a canonical operator file
    if ARTIFACT_CANARY_PATHS.iter().any(|c| pattern_match(&pat, c)) {
        return None;
    }
    Some(pat)
}

/// run_improver._repo_artifact_patterns (~811-826): per-repo EXTRA artifact patterns from THIS repo's
/// `agent_artifacts` list in repos.json (glob/regex strings), read fresh each call. [] when
/// absent/empty/torn; uncompilable/too-broad entries are silently skipped.
pub fn repo_artifact_patterns(ctx: &Ctx, name: &str) -> Vec<Regex> {
    let raw = repo_row(ctx, name);
    let arr = match raw.get("agent_artifacts") {
        Some(Value::Array(a)) => a,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in arr {
        if let Value::String(spec) = entry {
            if let Some(pat) = compile_artifact_pattern(spec) {
                out.push(pat);
            }
        }
    }
    out
}

/// run_improver._is_agent_artifact (~829-848): True if `path` matches the CONSERVATIVE global
/// heuristic OR one of the per-repo `extra_patterns`. Matched against the repo-relative POSIX form;
/// a leading `./` is stripped (but NOT a bare `.`), and a trailing `/` (a git `??` dir entry) is
/// trimmed. Pure so it is unit-tested without a real repo.
pub fn is_agent_artifact(path: &str, extra_patterns: Option<&[Regex]>) -> bool {
    let mut p = path.trim().replace('\\', "/");
    if p.is_empty() {
        return false;
    }
    // strip a leading './' relative prefix (NOT a bare '.' — that would strip '.agent_artifacts/...')
    while p.starts_with("./") {
        p.drain(..2);
    }
    // a git status `??` dir entry has a trailing slash; normalize it (trim_end_matches strips ALL,
    // matching Python str.rstrip("/"))
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return false;
    }
    if global_artifact_patterns().iter().any(|pat| pattern_match(pat, p)) {
        return true;
    }
    if let Some(extra) = extra_patterns {
        if extra.iter().any(|pat| pattern_match(pat, p)) {
            return true;
        }
    }
    false
}

/// run_improver._all_agent_artifacts (~851-858): True ONLY when `files` is non-empty AND every entry
/// matches an agent-artifact heuristic. An empty list returns False.
pub fn all_agent_artifacts(files: &[String], extra_patterns: Option<&[Regex]>) -> bool {
    if files.is_empty() {
        return false;
    }
    files.iter().all(|f| is_agent_artifact(f, extra_patterns))
}

/// run_improver._untracked_recovery_action (~861-873): pure decision over untracked non-ignored files:
///   "recover" — ALL files are agent artifacts -> stage them on the rsi branch and gate as usual
///   "refuse"  — at least one file is operator work -> refuse the clean + escalate
///   "none"    — empty list -> the clean path handles it.
pub fn untracked_recovery_action(
    ctx: &Ctx,
    files: &[String],
    extra_patterns: Option<&[Regex]>,
) -> String {
    let _ = ctx; // pure over `files`/`extra_patterns`; ctx kept for signature parity with siblings
    if files.is_empty() {
        return "none".to_string();
    }
    if all_agent_artifacts(files, extra_patterns) {
        "recover".to_string()
    } else {
        "refuse".to_string()
    }
}

/// run_improver._dirty_blocks_iteration (~876-883): whether a dirty tree must SKIP the iteration.
/// ONLY a dirty BASE branch is protected operator work; a dirty rsi/* (or any non-base / detached)
/// branch is a dead run's leftover the forced preflight clears. Pure.
pub fn dirty_blocks_iteration(ctx: &Ctx, dirty: bool, cur: &str, base: &str) -> bool {
    let _ = ctx; // pure over the three args; ctx for signature parity
    dirty && cur == base
}

// --------------------------------------------------------------------------- #
// auto-stash / branch lifecycle
// --------------------------------------------------------------------------- #

/// run_improver._auto_stash_base (~886-916): non-destructively clear a dirty BASE tree by STASHING it
/// (never deleting), so the loop self-resumes instead of wedging on operator-action-required. Returns
/// True iff the tree is VERIFIABLY clean afterward (tracked-clean AND no untracked non-ignored files).
/// Best-effort + fail-safe: on any git error or a still-dirty tree returns False.
pub fn auto_stash_base(ctx: &mut Ctx, label: &str) -> bool {
    let msg = format!("solomon-auto-preflight {label} {}", ctx::now());
    let res = ctx.git(&["stash", "push", "--include-untracked", "-m", &msg], 120);
    if res.code != 0 {
        let err: String = res.stderr.trim().chars().take(160).collect();
        ctx.log(&format!(
            "auto-stash: git stash failed ({err}) — falling back to refuse/self-stop (no work destroyed)"
        ));
        return false;
    }
    if tree_dirty(ctx) || !untracked_non_ignored_files(ctx).is_empty() {
        ctx.log("auto-stash: tree still dirty after stash — falling back to refuse/self-stop");
        return false;
    }
    // record the stash label so recovery is one command (best-effort; OSError -> pass)
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = std::fs::write(
        ctx.runtime.join("last_auto_stash.txt"),
        format!("{msg}\n"),
    );
    ctx.log(&format!(
        "auto-stash: stashed dirty base into '{msg}' — base clean, resuming \
         (recover with: git -C <repo> stash list / stash pop)"
    ));
    true
}

/// Maximum empty (agent-artifact-only) preflight stashes tolerated before pruning oldest. Since
/// `reconcile_preflight_stashes` runs every preflight and drops ALL empty ones, the steady-state
/// count is 0 — this cap is a safety net for crash-recovery bursts.
const PREFLIGHT_STASH_CAP: usize = 20;

/// True iff a `git` process is currently running anywhere on the system. Used by
/// [`clear_stale_index_lock`] to avoid removing a `.git/index.lock` that a LIVE git operation
/// still holds — only an orphaned lock (from a killed-mid-git improver) is safe to clear. On
/// Windows, scans `tasklist` for `git.exe`; on Unix, scans `/proc/*/comm` for `git`. A spawn
/// failure or a missing `/proc` is CONSERVATIVE: returns true (assume git is running, leave the
/// lock alone) — never clobbers a lock that might be live.
fn any_git_running() -> bool {
    #[cfg(windows)]
    {
        match crate::control::proc::run(
            &["tasklist", "/FI", "IMAGENAME eq git.exe", "/NH", "/FO", "CSV"],
            None,
            None,
        ) {
            Ok(out) => out.stdout.lines().any(|l| l.contains("git.exe")),
            Err(_) => true, // tasklist spawn failure -> conservative (leave the lock)
        }
    }
    #[cfg(not(windows))]
    {
        let entries = match std::fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return true, // no /proc -> conservative
        };
        for entry in entries.flatten() {
            if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
                if comm.trim() == "git" {
                    return true;
                }
            }
        }
        false
    }
}

/// Pure core of [`clear_stale_index_lock`]: remove the lock file iff it exists AND no live git
/// process holds it. Returns true iff a stale lock was removed. Split out so the policy is
/// unit-tested without spawning git / tasklist.
fn remove_stale_index_lock(lock_path: &std::path::Path, git_running: bool) -> bool {
    if !lock_path.exists() || git_running {
        return false;
    }
    std::fs::remove_file(lock_path).is_ok()
}

/// Remove a stale `.git/index.lock` left behind by a KILLED-mid-git improver iteration. A killed
/// `git reset`/`checkout`/`commit` orphans the lock; without recovery, every subsequent iteration's
/// `git checkout --force` fails with "index.lock exists" → status=error → watchdog restarts → same
/// lock → an infinite "checkout main failed — skipping iteration" storm (5 concurrent maki
/// improvers observed 2026-07-01, all blocked by one orphaned `.git/index.lock`). SAFE: only clears
/// when NO live git process is running anywhere (the lock is provably orphaned) — a lock that might
/// be held by an in-flight git op is left untouched. Best-effort: a removal failure is logged but
/// never wedges the preflight. Returns true iff a stale lock was removed.
///
/// Called at the top of the preflight, BEFORE `git checkout --force`/`reset --hard`, so an orphaned
/// lock from a prior killed iteration doesn't block the next. The single-instance lane lock
/// (run.rs `acquire_lock`) already guarantees only one improver per repo, so an index.lock here is
/// almost certainly orphaned — the `any_git_running` guard is the belt-and-suspenders against an
/// external (non-Solomon) git op on the same repo.
pub fn clear_stale_index_lock(ctx: &mut Ctx) -> bool {
    let git_dir = ctx.git(&["rev-parse", "--git-dir"], 10).stdout.trim().to_string();
    let lock_path = if git_dir.is_empty() {
        // rev-parse failed (bad repo / corrupt) — fall back to the conventional .git/index.lock.
        ctx.repo.join(".git").join("index.lock")
    } else {
        // rev-parse returns a path relative to the repo root (or absolute for worktrees).
        let p = std::path::Path::new(&git_dir);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            ctx.repo.join(p)
        }
        .join("index.lock")
    };
    if !lock_path.exists() {
        return false;
    }
    let git_running = any_git_running();
    if remove_stale_index_lock(&lock_path, git_running) {
        ctx.log("preflight: cleared stale .git/index.lock (orphaned by a killed-mid-git iteration)");
        return true;
    }
    if git_running {
        ctx.log("preflight: .git/index.lock exists but a git process is running — leaving it (may be live)");
    } else {
        ctx.log("preflight: .git/index.lock exists but could not be removed — git checkout may fail");
    }
    false
}

/// Reconcile `solomon-auto-preflight` stashes accumulated in the stash list:
///   - EMPTY stashes (agent-artifact-only — AGENT_LOG.md, capabilities/, etc.) are DROPPED.
///   - NON-EMPTY stashes (real swept work — a non-agent-artifact tracked change or untracked file)
///     are preserved to a `solomon-recovered/<ts>-<idx>` branch before being dropped, so swept
///     work is recoverable, never orphaned.
///
/// Called at the start of each iteration's preflight so preflight stashes never accumulate.
/// HARD INVARIANT: a stash with real content is NEVER dropped without first preserving its content.
/// If the stash count exceeds `PREFLIGHT_STASH_CAP`, the oldest empty ones are pruned (a safety net
/// for crash-recovery bursts where reconciliation could not run).
pub fn reconcile_preflight_stashes(ctx: &mut Ctx) {
    let list = ctx.git(&["stash", "list"], 120).stdout;
    // Parse "stash@{N}: On branch: solomon-auto-preflight ..." (stash@{0} is newest).
    let mut preflight: Vec<usize> = Vec::new();
    for line in list.lines() {
        let Some(rest) = line.strip_prefix("stash@{") else { continue };
        let Some((idx_str, after)) = rest.split_once('}') else { continue };
        let Ok(idx) = idx_str.parse::<usize>() else { continue };
        if after.contains("solomon-auto-preflight") {
            preflight.push(idx);
        }
    }
    if preflight.is_empty() {
        return;
    }
    preflight.sort();
    // Collect artifact patterns while holding ctx immutably, then process stashes mutably.
    let extra = repo_artifact_patterns(ctx, &ctx.name.clone());
    let mut dropped: i64 = 0;
    let mut recovered: i64 = 0;
    // Process highest index first so drops don't shift lower indices.
    for idx in preflight.iter().rev() {
        let ref_str = format!("stash@{{{idx}}}");
        if stash_has_real_content(ctx, &ref_str, &extra) {
            if recover_stash_to_branch(ctx, &ref_str, *idx) {
                recovered += 1;
            }
        } else {
            ctx.git(&["stash", "drop", &ref_str], 120);
            dropped += 1;
        }
    }
    // Safety-net cap: if more than PREFLIGHT_STASH_CAP empty preflight stashes somehow remain
    // (e.g. recovery failures retained non-empty stashes that shifted indices), prune oldest empty.
    prune_excess_empty_preflight_stashes(ctx, &extra);
    if dropped > 0 || recovered > 0 {
        ctx.log(&format!(
            "stash-hygiene: dropped {dropped} empty preflight stash(es), recovered {recovered} \
             to solomon-recovered/* branch(es)"
        ));
    }
}

/// Whether a stash carries REAL (non-agent-artifact) content — a tracked diff entry OR an
/// untracked file (the stash's third parent from `--include-untracked`) that is NOT an agent
/// artifact. A stash whose every file matches an agent-artifact heuristic is "empty" for hygiene
/// purposes and may be safely dropped. Pure over `ctx.git` results + `is_agent_artifact`.
fn stash_has_real_content(ctx: &Ctx, ref_str: &str, extra: &[Regex]) -> bool {
    // Tracked changes (git stash show --name-only lists modified tracked files only).
    // If the inspection itself FAILS (timeout -> code=124 under CPU contention, or any error),
    // stdout is empty and we cannot know the stash is empty. Treat a failed inspection as
    // "unknown -> assume real content" (retain), mirroring the conservative `code == 0` guard on
    // the untracked `^3` path below. Collapsing a failed inspection to "empty -> drop" would let a
    // tracked-only stash be permanently dropped, violating the HARD INVARIANT at line ~357.
    let tracked = ctx.git(&["stash", "show", "--name-only", ref_str], 120);
    if tracked.code != 0 {
        return true;
    }
    for line in tracked.stdout.lines() {
        let f = line.trim();
        if !f.is_empty() && !is_agent_artifact(f, Some(extra)) {
            return true;
        }
    }
    // Untracked files (stash's third parent — only exists when --include-untracked was used).
    // `git ls-tree --name-only` lists the files IN the untracked commit; `git diff ^1 ^3` would
    // also list base files absent from ^3 (all of them when ^3 is empty), producing false
    // positives. ls-tree gives exactly the untracked files swept into the stash.
    let untracked = ctx.git(
        &["ls-tree", "--name-only", &format!("{ref_str}^3")],
        120,
    );
    // A missing `^3` parent (no --include-untracked) legitimately fails with git's code 128 -> the
    // stash simply has no untracked content, fall through to droppable. But a TRANSIENT timeout
    // (code 124) means we could NOT inspect it, so we must NOT conclude "empty -> drop": retain,
    // mirroring the tracked path's fail-safe. (Only 124 -> unknown; 128 -> genuinely no untracked.)
    if untracked.code == 124 {
        return true;
    }
    if untracked.code == 0 {
        for line in untracked.stdout.lines() {
            let f = line.trim();
            if !f.is_empty() && !is_agent_artifact(f, Some(extra)) {
                return true;
            }
        }
    }
    false
}

/// Preserve a stash's content to a `solomon-recovered/<ts>-<idx>` branch via `git stash branch`,
/// then commit and return to the base branch. `git stash branch` applies the stash to a new branch
/// and DROPS the stash on success. Returns true on success; on failure the stash is RETAINED and
/// the error is surfaced loudly (heartbeat + log) for manual recovery.
fn recover_stash_to_branch(ctx: &mut Ctx, stash_ref: &str, idx: usize) -> bool {
    let ts = ctx::stamp();
    let branch = format!("solomon-recovered/{ts}-{idx}");
    let res = ctx.git(&["stash", "branch", &branch, stash_ref], 120);
    if res.code == 0 {
        // Stash applied + auto-dropped. Commit the changes so they survive a checkout.
        ctx.git(&["add", "-A"], 120);
        let msg = format!("solomon-recovered: swept preflight work from {stash_ref}");
        let _ = ctx.git(&["commit", "-m", &msg], 120);
        ctx.git(&["checkout", "--force", &ctx.base_branch], 120);
        ctx.log(&format!(
            "stash-hygiene: recovered real content from {stash_ref} to branch '{branch}'"
        ));
        true
    } else {
        let err: String = res.stderr.trim().chars().take(200).collect();
        // Clean up the partial branch (if created) and return to base.
        ctx.git(&["checkout", "--force", &ctx.base_branch], 120);
        let _ = ctx.git(&["branch", "-D", &branch], 120);
        ctx.log(&format!(
            "stash-hygiene: WARNING — could not auto-recover {stash_ref} to a branch ({err}); \
             stash RETAINED for manual recovery"
        ));
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "preflight",
            "last_summary": format!(
                "A preflight stash has real content but could not be auto-recovered to a branch ({err}). \
                 Manual recovery: git stash apply {stash_ref}"
            ),
        }));
        false
    }
}

/// Safety-net cap: when empty (agent-artifact-only) preflight stashes exceed
/// `PREFLIGHT_STASH_CAP`, prune the oldest ones. Oldest = highest stash index (stash@{0} is
/// newest). Non-empty stashes are NEVER touched here.
fn prune_excess_empty_preflight_stashes(ctx: &mut Ctx, extra: &[Regex]) {
    let list = ctx.git(&["stash", "list"], 120).stdout;
    let mut empty: Vec<usize> = Vec::new();
    for line in list.lines() {
        let Some(rest) = line.strip_prefix("stash@{") else { continue };
        let Some((idx_str, after)) = rest.split_once('}') else { continue };
        let Ok(idx) = idx_str.parse::<usize>() else { continue };
        if !after.contains("solomon-auto-preflight") {
            continue;
        }
        let ref_str = format!("stash@{{{idx}}}");
        if !stash_has_real_content(ctx, &ref_str, extra) {
            empty.push(idx);
        }
    }
    if empty.len() <= PREFLIGHT_STASH_CAP {
        return;
    }
    let to_prune = empty.len() - PREFLIGHT_STASH_CAP;
    // Oldest = highest index. Drop from highest to lowest to avoid index shifting.
    empty.sort_by(|a, b| b.cmp(a));
    let mut pruned = 0i64;
    for idx in empty.iter().take(to_prune) {
        let ref_str = format!("stash@{{{idx}}}");
        if ctx.git(&["stash", "drop", &ref_str], 120).code == 0 {
            pruned += 1;
        }
    }
    if pruned > 0 {
        ctx.log(&format!(
            "stash-hygiene: safety-net pruned {pruned} oldest empty preflight stash(es) (cap {PREFLIGHT_STASH_CAP})"
        ));
    }
}

/// run_improver._abort_branch (~923-938): revert the working tree and delete `branch`, fail-closed.
/// Returns True only when verifiably back on BASE_BRANCH with the branch removed; logs + returns False
/// otherwise so the caller surfaces an error.
pub fn abort_branch(ctx: &Ctx, branch: &str) -> bool {
    // DEVIATION: Python's _abort_branch calls log() (print + append to LOG + push onto hb["log_tail"]).
    // The entry-point signature for this port is `abort_branch(&Ctx, &str)` (read-only), so the
    // hb["log_tail"] mutation is dropped; the print + LOG-file append are preserved verbatim via
    // log_ro (the read-only slice of ctx.log). drop_branch's error-heartbeat path re-surfaces the same
    // diagnostic when a revert fails.
    ctx.git(&["reset", "--hard"], 120);
    let co = ctx.git(&["checkout", "--force", &ctx.base_branch], 120);
    if co.code != 0 {
        let err: String = co.stderr.trim().chars().take(200).collect();
        log_ro(ctx, &format!("CRITICAL: could not return to {}: {err}", ctx.base_branch));
        return false;
    }
    let head = ctx
        .git(&["rev-parse", "--abbrev-ref", "HEAD"], 120)
        .stdout
        .trim()
        .to_string();
    if head != ctx.base_branch {
        log_ro(
            ctx,
            &format!("CRITICAL: not on {} after checkout; refusing to delete {branch}", ctx.base_branch),
        );
        return false;
    }
    let d = ctx.git(&["branch", "-D", branch], 120);
    if d.code != 0 {
        let err: String = d.stderr.trim().chars().take(200).collect();
        log_ro(ctx, &format!("branch -D {branch} failed: {err}"));
    }
    true
}

/// The read-only slice of `ctx.log` (print + append to the LOG file), for the `&Ctx` abort_branch path
/// where the hb["log_tail"] push (which needs `&mut`) cannot run. Same `{now} {msg}` line format.
fn log_ro(ctx: &Ctx, msg: &str) {
    let line = format!("{} {}", ctx::now(), msg);
    println!("{line}");
    ctx.runtime_append(&ctx.log_path, &line);
}

/// run_improver._drop_branch (~941-953): abort a branch and write the matching heartbeat — escalating
/// to status="error" (and HALTING the loop) if the revert could not complete, so the loop never
/// silently keeps branching off poisoned state. `status` defaults to "sleeping" in Python; callers
/// pass it explicitly here.
pub fn drop_branch(ctx: &mut Ctx, branch: &str, phase: &str, summary: &str, status: &str) {
    // Anti-thrash: track consecutive reverts of the same goal. When N consecutive reverts of the
    // SAME goal hit, force a different goal next iteration (defer + corrective note + surface
    // stuck_goal in the heartbeat) instead of silently retrying the reverted change. This covers
    // ALL revert paths (gate-red, anti-gaming, cross-repo, eval, leak guard, visual, review reject)
    // since they all go through drop_branch with phase="reverted".
    if phase == "reverted" {
        if let Some(goal) = ctx.hb.get("goal").and_then(Value::as_str).map(|s| s.to_string()) {
            escalation::note_consecutive_revert(ctx, &goal);
        }
    }
    if abort_branch(ctx, branch) {
        ctx.heartbeat(json!({"status": status, "phase": phase, "last_summary": summary}));
        ctx.record_history(phase, Some(branch), summary, None);
    } else {
        ctx.halted = true; // halt the loop — don't bulldoze a known-bad tree next preflight
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "reverted",
            "last_summary": format!(
                "REVERT FAILED — {branch} needs manual cleanup before the loop can safely continue. {summary}"
            ),
        }));
        ctx.record_history("error", Some(branch), summary, None);
    }
}

/// run_improver._prune_stale_rsi_branches (~957-978): branch hygiene — delete every local rsi/*
/// iteration branch except the current one, and prune stale worktrees. Called in preflight on the
/// CLEAN base so it never discards in-flight work. NEVER force-deletes a branch carrying commits
/// unreachable from the base (a kept gate-green local-ship branch); only fully-merged/empty residue.
/// Returns the count pruned.
pub fn prune_stale_rsi_branches(ctx: &Ctx) -> i64 {
    ctx.git(&["worktree", "prune"], 120);
    let cur = ctx
        .git(&["rev-parse", "--abbrev-ref", "HEAD"], 120)
        .stdout
        .trim()
        .to_string();
    let out = ctx.git(&["branch", "--list", "rsi/*"], 120).stdout;
    let mut pruned: i64 = 0;
    for line in out.lines() {
        let b = line.replace('*', "");
        let b = b.trim();
        if b.is_empty() || b == cur {
            continue;
        }
        // `git rev-list <base>..<b>` writes commits to STDOUT only on SUCCESS; on ANY failure
        // (misconfigured base, timeout -> code=124, etc.) it exits non-zero with EMPTY stdout.
        // A failed rev-list must NOT be trusted as "merged" — that would force-delete a branch
        // carrying unmerged commits, breaking this function's NEVER-force-delete invariant.
        let rl = ctx.git(&["rev-list", &format!("{}..{}", ctx.base_branch, b)], 120);
        if rl.code == 0
            && rl.stdout.trim().is_empty()
            && ctx.git(&["branch", "-D", b], 120).code == 0
        {
            pruned += 1;
        }
    }
    pruned
}

// --------------------------------------------------------------------------- #
// staging + per-repo config (the public-repo leak guards)
// --------------------------------------------------------------------------- #

/// run_improver._git_add_all (~1296-1310): stage everything with `git add -A`, then (PUBLIC repo)
/// UNSTAGE its `private_paths` — so an agent that un-ignored a private path STILL cannot land it in a
/// pushed commit. Bare `git add -A` (no pathspec) silently SKIPS ignored files. Non-destructive.
/// Returns the `git add -A` RunOut (Python returns `r`).
pub fn git_add_all(ctx: &Ctx) -> crate::control::proc::RunOut {
    let r = ctx.git(&["add", "-A"], 120);
    if repo_is_public(ctx, &ctx.name) {
        let paths = repo_private_paths(ctx, &ctx.name);
        if !paths.is_empty() {
            // git reset -q -- <p1> <p2> ...  (unstage any private path that slipped in; no-op if none)
            let mut argv: Vec<&str> = vec!["reset", "-q", "--"];
            for p in &paths {
                argv.push(p.as_str());
            }
            ctx.git(&argv, 120);
        }
    }
    r
}

/// run_improver._repo_row (~1260-1269): THIS repo's repos.json row (read fresh so a dashboard edit
/// takes effect mid-loop), or {} on absent/torn/non-list/missing.
pub fn repo_row(ctx: &Ctx, name: &str) -> Value {
    let bytes = match std::fs::read(ctx.control.join("repos.json")) {
        Ok(b) => b,
        Err(_) => return json!({}), // OSError -> {}
    };
    let rows: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return json!({}), // ValueError -> {}
    };
    let arr = match rows {
        Value::Array(a) => a,
        _ => return json!({}), // not a list -> {}
    };
    arr.into_iter()
        .find(|r| r.is_object() && r.get("name").and_then(Value::as_str) == Some(name))
        .filter(|r| r.is_object())
        .unwrap_or_else(|| json!({}))
}

/// run_improver._repo_is_public (~1272-1275): whether the repo is PUBLIC (repos.json `public: true`).
pub fn repo_is_public(ctx: &Ctx, name: &str) -> bool {
    py_bool(repo_row(ctx, name).get("public"))
}

/// run_improver._repo_private_paths (~1278-1284): repo-relative paths that must NEVER be staged into a
/// PUBLIC repo's commit (repos.json `private_paths`). POSIX, trailing slash trimmed; empty entries
/// dropped. [] when absent/non-list.
pub fn repo_private_paths(ctx: &Ctx, name: &str) -> Vec<String> {
    let row = repo_row(ctx, name);
    let arr = match row.get("private_paths") {
        Some(Value::Array(a)) => a,
        _ => return Vec::new(),
    };
    arr.iter()
        .filter_map(|p| {
            let s = value_to_py_str(p);
            if s.trim().is_empty() {
                None // `if str(p).strip()` — drop blank entries
            } else {
                Some(s.trim().replace('\\', "/").trim_end_matches('/').to_string())
            }
        })
        .collect()
}

/// run_improver._repo_deny_terms (~1287-1293): operator brand/account identity strings to scrub/block
/// (repos.json `deny_terms`). [] when absent. Entries kept iff `str(t).strip()` is truthy, but the
/// VALUE stored is `str(t)` (NOT stripped) — preserved verbatim.
pub fn repo_deny_terms(ctx: &Ctx, name: &str) -> Vec<String> {
    let row = repo_row(ctx, name);
    let arr = match row.get("deny_terms") {
        Some(Value::Array(a)) => a,
        _ => return Vec::new(),
    };
    arr.iter()
        .map(value_to_py_str)
        .filter(|t| !t.trim().is_empty())
        .collect()
}

// --------------------------------------------------------------------------- #
// helpers
// --------------------------------------------------------------------------- #

/// Python `bool(x)` truthiness for a repos.json value (null/false/0/""/[]/{} -> false). Mirrors
/// ctx.rs's private `py_bool` (kept module-local since it is not pub-exported there).
fn py_bool(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `str(x)` of a repos.json scalar (private_paths/deny_terms do `str(p)`/`str(t)`). Strings
/// pass through verbatim; other scalars get their Python-ish text form.
fn value_to_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Translate an fnmatch glob into a regex string equivalent to Python `fnmatch.translate`, which
/// produces a `(?s:...)\Z` body where `*` -> `.*`, `?` -> `.`, `[...]` -> a char class, and every
/// other char is escaped. `\Z` (Python end-of-string) maps to `\z` in the `regex` crate. The leading
/// `(?s:` enables DOTALL so `.` matches `/` — matching Python's translate exactly.
fn fnmatch_translate(pat: &str) -> String {
    let chars: Vec<char> = pat.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let mut res = String::from("(?s:");
    while i < n {
        let c = chars[i];
        i += 1;
        match c {
            '*' => res.push_str(".*"),
            '?' => res.push('.'),
            '[' => {
                // find the matching ']'
                let mut j = i;
                if j < n && chars[j] == '!' {
                    j += 1;
                }
                if j < n && chars[j] == ']' {
                    j += 1;
                }
                while j < n && chars[j] != ']' {
                    j += 1;
                }
                if j >= n {
                    // no closing bracket: literal '['
                    res.push_str("\\[");
                } else {
                    let inner: String = chars[i..j].iter().collect();
                    // Python: stuff = inner.replace('\\', r'\\'); leading '!' -> '^'
                    let mut stuff = inner.replace('\\', "\\\\");
                    if let Some(rest) = stuff.strip_prefix('!') {
                        stuff = format!("^{rest}");
                    } else if stuff.starts_with('^') {
                        stuff = format!("\\{stuff}");
                    }
                    res.push('[');
                    res.push_str(&stuff);
                    res.push(']');
                    i = j + 1;
                }
            }
            other => {
                // re.escape(c)
                res.push_str(&regex::escape(&other.to_string()));
            }
        }
    }
    res.push_str(r")\z");
    res
}

// --------------------------------------------------------------------------- #
// tests — load-bearing pure logic (artifact classification + recovery decision)
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_agent_artifact: the CONSERVATIVE global heuristic ----
    #[test]
    fn global_artifact_matches() {
        assert!(is_agent_artifact("AGENT_LOG.md", None));
        assert!(is_agent_artifact("capabilities/foo", None));
        assert!(is_agent_artifact("capabilities/foo/bar.py", None));
        assert!(is_agent_artifact("profiles/ggg", None));
        assert!(is_agent_artifact("profiles/ggg/state.json", None));
        assert!(is_agent_artifact("start_ggg.sh", None));
        assert!(is_agent_artifact(".agent_artifacts/anything.txt", None));
    }

    #[test]
    fn global_artifact_non_matches() {
        // bare dir forms are NOT matched (they'd sweep any operator dir of that name)
        assert!(!is_agent_artifact("capabilities", None));
        assert!(!is_agent_artifact("profiles", None));
        // canonical operator files
        assert!(!is_agent_artifact("README.md", None));
        assert!(!is_agent_artifact("main.py", None));
        // a near-miss: AGENT_LOG must be the EXACT name (end-anchored)
        assert!(!is_agent_artifact("AGENT_LOG.md.bak", None));
        // start_ must end in .sh
        assert!(!is_agent_artifact("start_ggg.py", None));
    }

    #[test]
    fn trailing_slash_and_dot_prefix_normalized() {
        // a git `??` dir entry has a trailing slash -> capabilities/foo/ matches capabilities/foo[/...]
        assert!(is_agent_artifact("capabilities/foo/", None));
        // leading './' stripped (but NOT a bare '.')
        assert!(is_agent_artifact("./AGENT_LOG.md", None));
        assert!(is_agent_artifact("./.agent_artifacts/x", None));
        // backslashes normalized to POSIX
        assert!(is_agent_artifact("capabilities\\foo\\bar", None));
        // empty / whitespace -> false
        assert!(!is_agent_artifact("", None));
        assert!(!is_agent_artifact("   ", None));
    }

    // ---- all_agent_artifacts ----
    #[test]
    fn all_artifacts_requires_nonempty_and_every() {
        assert!(!all_agent_artifacts(&[], None)); // empty -> False
        assert!(all_agent_artifacts(
            &["AGENT_LOG.md".to_string(), "profiles/x".to_string()],
            None
        ));
        // one operator file among artifacts -> the whole set fails
        assert!(!all_agent_artifacts(
            &["AGENT_LOG.md".to_string(), "src/real_module.py".to_string()],
            None
        ));
    }

    // ---- untracked_recovery_action: the 3 exact decision strings ----
    #[test]
    fn recovery_action_strings() {
        let ctx = test_ctx();
        assert_eq!(untracked_recovery_action(&ctx, &[], None), "none");
        assert_eq!(
            untracked_recovery_action(&ctx, &["AGENT_LOG.md".to_string()], None),
            "recover"
        );
        assert_eq!(
            untracked_recovery_action(&ctx, &["operator_notes.md".to_string()], None),
            "refuse"
        );
        // mixed -> refuse (a single operator file keeps the set protected)
        assert_eq!(
            untracked_recovery_action(
                &ctx,
                &["AGENT_LOG.md".to_string(), "operator_notes.md".to_string()],
                None
            ),
            "refuse"
        );
    }

    // ---- compile_artifact_pattern: glob, regex, breadth guard, empties ----
    #[test]
    fn compile_pattern_glob_and_regex() {
        // glob entry
        let p = compile_artifact_pattern("*.log").unwrap();
        assert!(pattern_match(&p, "pi_runner.log"));
        assert!(!pattern_match(&p, "pi_runner.log.keep"));
        // glob with a trailing slash dropped + dir
        let p2 = compile_artifact_pattern("scaffold_*/").unwrap();
        assert!(pattern_match(&p2, "scaffold_x"));
        // regex entry (char class is NOT routed to glob since only */? signal a glob)
        let p3 = compile_artifact_pattern(r"lane_[0-9]+\.json").unwrap();
        assert!(pattern_match(&p3, "lane_42.json"));
        assert!(!pattern_match(&p3, "lane_42.json.bak")); // end-anchored
        // a trailing '$' is stripped then re-anchored (parity)
        let p4 = compile_artifact_pattern(r"foo\.txt$").unwrap();
        assert!(pattern_match(&p4, "foo.txt"));
    }

    #[test]
    fn compile_pattern_rejects_broad_and_empty() {
        // empty / whitespace -> None
        assert!(compile_artifact_pattern("").is_none());
        assert!(compile_artifact_pattern("   ").is_none());
        // too-broad: matches a canonical operator file -> None
        assert!(compile_artifact_pattern("*").is_none());
        assert!(compile_artifact_pattern("*.py").is_none()); // would match main.py
        assert!(compile_artifact_pattern(r".+").is_none());
        // a narrow operator pattern that hits NO canary survives
        assert!(compile_artifact_pattern("pi_runner_heartbeat.json").is_some());
    }

    // ---- extra patterns widen recovery without loosening the default ----
    #[test]
    fn extra_patterns_widen() {
        let extra = vec![compile_artifact_pattern("*.scratch").unwrap()];
        assert!(is_agent_artifact("tmp.scratch", Some(&extra)));
        assert!(!is_agent_artifact("tmp.scratch", None)); // not a global artifact
    }

    // ---- dirty_blocks_iteration: only a dirty BASE blocks ----
    #[test]
    fn dirty_blocks_only_on_base() {
        let ctx = test_ctx();
        assert!(dirty_blocks_iteration(&ctx, true, "main", "main"));
        assert!(!dirty_blocks_iteration(&ctx, false, "main", "main")); // clean base
        assert!(!dirty_blocks_iteration(&ctx, true, "rsi/iter-x", "main")); // dirty rsi branch
        assert!(!dirty_blocks_iteration(&ctx, true, "", "main")); // detached
    }

    // ---- fnmatch_translate sanity (the friendly default) ----
    #[test]
    fn fnmatch_translate_basic() {
        let re = Regex::new(&fnmatch_translate("*.log")).unwrap();
        assert!(pattern_match(&re, "a.log"));
        assert!(pattern_match(&re, "deep/path/a.log")); // DOTALL: . matches /
        let q = Regex::new(&fnmatch_translate("file?.txt")).unwrap();
        assert!(pattern_match(&q, "file1.txt"));
        assert!(!pattern_match(&q, "file12.txt"));
    }

    fn test_ctx() -> Ctx {
        Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None)
    }

    // ---- reconcile_preflight_stashes: real git repo integration tests ----

    /// Helper: create a real throwaway git repo + Ctx for stash-hygiene tests.
    fn real_repo_ctx() -> (Ctx, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let uniq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "solomon_stash_real_{}_{}",
            std::process::id(),
            uniq
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut c = Ctx::configure(&dir.to_string_lossy(), "testrepo", "ollama-cloud", None);
        c.runtime = std::env::temp_dir().join(format!(
            "solomon_stash_rt_{}_{}",
            std::process::id(),
            uniq
        ));
        std::fs::create_dir_all(&c.runtime).unwrap();

        // git init + minimal config (CI environments may lack global git config).
        assert_eq!(c.git(&["init", "--quiet"], 30).code, 0);
        c.git(&["config", "user.email", "test@test.test"], 10);
        c.git(&["config", "user.name", "Test"], 10);
        // Create an initial commit so HEAD resolves to a real branch.
        std::fs::write(dir.join("README.md"), "# test\n").unwrap();
        c.git(&["add", "-A"], 10);
        c.git(&["commit", "--quiet", "-m", "initial"], 10);
        // Detect the default branch name (main / master / etc.).
        let branch = c
            .git(&["rev-parse", "--abbrev-ref", "HEAD"], 10)
            .stdout
            .trim()
            .to_string();
        c.base_branch = if branch.is_empty() || branch == "HEAD" {
            "main".to_string()
        } else {
            branch
        };
        (c, dir)
    }

    /// HARD INVARIANT: a preflight stash with real dirty-base content (a non-agent-artifact
    /// tracked file) ends with that content committed to a solomon-recovered/* branch, NOT
    /// orphaned in the stash list.
    #[test]
    fn reconcile_recovers_real_dirty_base_content() {
        let (mut c, dir) = real_repo_ctx();

        // Dirty the base with REAL content (backend.py is not an agent artifact).
        std::fs::write(dir.join("backend.py"), "print('hello')\n").unwrap();
        c.git(&["add", "-A"], 10);
        c.git(&["commit", "--quiet", "-m", "add backend"], 10);
        std::fs::write(dir.join("backend.py"), "print('hello world')\n").unwrap();

        // Stash the dirty base (simulating preflight auto-stash).
        assert!(
            auto_stash_base(&mut c, "rsi/iter-test"),
            "auto_stash_base should succeed on a dirty tree"
        );
        let list = c.git(&["stash", "list"], 10).stdout;
        assert!(
            list.contains("solomon-auto-preflight"),
            "stash should exist after auto_stash_base"
        );

        // Reconcile — should recover the real content to a branch.
        reconcile_preflight_stashes(&mut c);

        // The stash should be GONE (recovered, not orphaned).
        let list2 = c.git(&["stash", "list"], 10).stdout;
        assert!(
            !list2.contains("solomon-auto-preflight"),
            "real-content stash should be recovered, not orphaned in the stash list"
        );

        // A solomon-recovered/* branch should exist.
        let branches = c
            .git(&["branch", "--list", "solomon-recovered/*"], 10)
            .stdout;
        assert!(
            !branches.trim().is_empty(),
            "a solomon-recovered/* branch should exist with the swept work"
        );

        // Verify the recovery branch actually has the real content.
        let recovered = branches
            .lines()
            .next()
            .unwrap()
            .replace('*', "")
            .trim()
            .to_string();
        c.git(&["checkout", "--force", &recovered], 10);
        let content = std::fs::read_to_string(dir.join("backend.py")).unwrap();
        assert!(
            content.contains("hello world"),
            "recovered branch should contain the real swept work, not the original"
        );

        // Cleanup.
        c.git(&["checkout", "--force", &c.base_branch], 10);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    /// A preflight stash containing ONLY an agent artifact (AGENT_LOG.md) is DROPPED — no
    /// recovery branch is created. This is the "empty" case: agent debris, not real work.
    #[test]
    fn reconcile_drops_agent_artifact_only_stash() {
        let (mut c, dir) = real_repo_ctx();

        // Track AGENT_LOG.md (an agent artifact) and dirty it.
        std::fs::write(dir.join("AGENT_LOG.md"), "original log\n").unwrap();
        c.git(&["add", "-A"], 10);
        c.git(&["commit", "--quiet", "-m", "add agent log"], 10);
        std::fs::write(dir.join("AGENT_LOG.md"), "modified log\n").unwrap();

        // Stash the dirty base (only an agent-artifact file changed).
        assert!(auto_stash_base(&mut c, "rsi/iter-test"));

        // Reconcile — should DROP it (agent artifact only, no real content).
        reconcile_preflight_stashes(&mut c);

        // No preflight stash should remain.
        let list = c.git(&["stash", "list"], 10).stdout;
        assert!(
            !list.contains("solomon-auto-preflight"),
            "agent-artifact-only stash should be dropped"
        );

        // No recovery branch should be created.
        let branches = c
            .git(&["branch", "--list", "solomon-recovered/*"], 10)
            .stdout;
        assert!(
            branches.trim().is_empty(),
            "no recovery branch should be created for agent-artifact-only stashes"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    // ---- prune_stale_rsi_branches: a FAILED rev-list must not force-delete an unmerged branch ----

    /// git-worktree-0: when `git rev-list <base>..<b>` FAILS (here: a misconfigured/renamed
    /// base_branch that does not exist), it exits non-zero with EMPTY stdout. The pruner must NOT
    /// treat that empty stdout as "merged" and force-delete the branch — the rsi/* branch carries an
    /// unmerged commit and must be RETAINED.
    #[test]
    fn prune_skips_branch_when_rev_list_fails() {
        let (mut c, dir) = real_repo_ctx();

        // Create an rsi/* branch carrying an UNMERGED commit (unreachable from base).
        c.git(&["checkout", "-b", "rsi/iter-keep"], 10);
        std::fs::write(dir.join("ship.py"), "print('gate-green ship')\n").unwrap();
        c.git(&["add", "-A"], 10);
        c.git(&["commit", "--quiet", "-m", "unmerged local-ship work"], 10);
        // Back to base so the pruner won't skip it as the current branch.
        c.git(&["checkout", "--force", &c.base_branch], 10);

        // Misconfigure the base so `git rev-list <BOGUS>..rsi/iter-keep` exits 128 with empty stdout.
        c.base_branch = "does-not-exist-base".to_string();

        let pruned = prune_stale_rsi_branches(&c);

        assert_eq!(
            pruned, 0,
            "a branch whose merged-ness could not be verified must NOT be pruned"
        );
        let branches = c.git(&["branch", "--list", "rsi/*"], 10).stdout;
        assert!(
            branches.contains("rsi/iter-keep"),
            "the unmerged rsi/* branch must be RETAINED when rev-list fails, not force-deleted"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    /// git-worktree-2/5 (conservative retention): a preflight stash carrying REAL tracked-only
    /// content (a modified tracked operator file, no untracked `^3` files) is recognized as having
    /// real content and is recovered to a branch, never silently dropped.
    #[test]
    fn stash_tracked_only_real_content_is_retained() {
        let (mut c, dir) = real_repo_ctx();

        // A tracked operator file (not an agent artifact), modified but NOT committed -> the stash
        // will have tracked changes and NO untracked `^3` parent.
        std::fs::write(dir.join("operator.py"), "print('v1')\n").unwrap();
        c.git(&["add", "-A"], 10);
        c.git(&["commit", "--quiet", "-m", "add operator file"], 10);
        std::fs::write(dir.join("operator.py"), "print('v2 real work')\n").unwrap();

        assert!(auto_stash_base(&mut c, "rsi/iter-test"));
        let ref0 = "stash@{0}";
        let extra = repo_artifact_patterns(&c, &c.name.clone());
        assert!(
            stash_has_real_content(&c, ref0, &extra),
            "a tracked-only real-content stash must be recognized as having real content"
        );

        reconcile_preflight_stashes(&mut c);

        let branches = c
            .git(&["branch", "--list", "solomon-recovered/*"], 10)
            .stdout;
        assert!(
            !branches.trim().is_empty(),
            "tracked-only real work must be recovered to a branch, never dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    /// git-worktree-3: an untracked file whose name contains a space is returned as the LITERAL
    /// path (no surrounding quotes / octal escapes), so start-anchored artifact patterns still match.
    #[test]
    fn untracked_space_name_is_unquoted() {
        let (c, dir) = real_repo_ctx();

        std::fs::write(dir.join("a file with spaces.txt"), "x\n").unwrap();
        let files = untracked_non_ignored_files(&c);

        assert!(
            files.iter().any(|f| f == "a file with spaces.txt"),
            "space-containing untracked path must be the literal name, not git-quoted; got {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.starts_with('"')),
            "no entry should carry a leading git quote character; got {files:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    // ---- clear_stale_index_lock: the orphaned-index.lock recovery ----
    //
    // A killed-mid-git iteration orphans a `.git/index.lock`; without recovery, every subsequent
    // `git checkout --force` fails → status=error → watchdog restart → same lock → infinite storm.
    // The pure policy core is tested directly (the `any_git_running` IO guard is belt-and-suspenders
    // and conservative by construction).

    #[test]
    fn remove_stale_index_lock_removes_when_no_git_running() {
        let dir = std::env::temp_dir().join(format!(
            "solomon_idxlock_clean_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        assert!(remove_stale_index_lock(&lock, false));
        assert!(!lock.exists(), "stale lock must be removed when no git is running");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_stale_index_lock_leaves_lock_when_git_running() {
        let dir = std::env::temp_dir().join(format!(
            "solomon_idxlock_live_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("index.lock");
        std::fs::write(&lock, "").unwrap();
        assert!(!remove_stale_index_lock(&lock, true));
        assert!(lock.exists(), "lock must NOT be removed while a git process might hold it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_stale_index_lock_noop_when_no_lock() {
        let dir = std::env::temp_dir().join(format!(
            "solomon_idxlock_none_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("index.lock");
        // no file created
        assert!(!remove_stale_index_lock(&lock, false));
        assert!(!lock.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
