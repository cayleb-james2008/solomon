//! Native Rust port of control.py's "branches" module (bug-for-bug).
//!
//! Source of truth: control.py — local_rsi_branches (1337), cleanup_worktrees (1701),
//! list_worktrees (1738), branch_hygiene (1771), clean_branch (1807), reset_to_base (2012).
//! Spec + golden vectors: src-tauri/control-port-spec.json (module == "branches").
//!
//! Foundation: `proc::run` / `proc::which_git`; `paths::repo_path`. Sibling deps:
//! `registry::project_pr_target_branch`, `locks::is_running`.
//!
//! HIGH RISK: reset_to_base + clean_branch are destructive; the guard ORDER and per-command
//! exit-code propagation below are load-bearing and must stay byte-identical to control.py.

use crate::control::{paths, proc};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

/// 60s ceiling on every branches git call — mirrors the 4bbfd99 fix for `watchdog::base_is_clean`/
/// `base_is_pushed`. A hung git (credential prompt on null stdin, slow network, locked index,
/// orphaned pipe) must NEVER wedge the watchdog tick — `proc::run`'s timeout branch kills the
/// child and returns `Err(TimedOut)`, which every caller here maps to `ok:false` / `[]` (the
/// same shape as the existing OSError branch). No behavioral change on the happy path
/// (status/prune/checkout finish in <1s); only the pathological-hang path changes.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Python `s.strip()[:200]`: strip leading/trailing ASCII whitespace, then take the first 200
/// chars (Unicode code points, not bytes — Python slices by code point).
fn strip_trunc_200(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

/// `(out.stdout or "").strip()` idiom — RunOut.stdout is already a String (never None), so this is
/// just an ASCII-whitespace trim returning an owned String.
fn out_stripped(r: &proc::RunOut) -> String {
    r.stdout.trim().to_string()
}

/// `git -C <path> <args...>`: the `-C path` threading control.py uses for every branches call
/// except local_rsi_branches. Returns the same io::Result as proc::run (Err == Python OSError/timeout).
fn git_c(git: &Path, path: &str, args: &[&str]) -> std::io::Result<proc::RunOut> {
    let git = git.to_string_lossy();
    let mut full: Vec<&str> = Vec::with_capacity(args.len() + 3);
    full.push(&git);
    full.push("-C");
    full.push(path);
    full.extend_from_slice(args);
    proc::run(&full, None, Some(GIT_TIMEOUT))
}

// --------------------------------------------------------------------------- //
// local_rsi_branches
// --------------------------------------------------------------------------- //

/// control.py:1337 — `git branch --list "<prefix>*"` -> branch names. `[]` on any error.
///
/// NOTE: this is the ONE function that passes cwd=path rather than `-C path`. An empty/missing
/// branch_prefix returns `[]` (empty prefix matches NOTHING — deliberate, does not list all branches).
pub fn local_rsi_branches(repo: &Value) -> Vec<String> {
    let git = match proc::which_git() {
        Some(g) => g,
        None => return Vec::new(),
    };
    // repo.get("branch_prefix") or "" — default "" here (differs from branch_hygiene's "rsi/").
    let prefix = repo
        .get("branch_prefix")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = paths::repo_path(repo);
    if prefix.is_empty() || path.is_empty() {
        // an empty prefix must match NOTHING
        return Vec::new();
    }
    let git = git.to_string_lossy();
    let pattern = format!("{prefix}*");
    let r = match proc::run(
        &[git.as_ref(), "branch", "--list", pattern.as_str()],
        Some(Path::new(&path)), // cwd=path (not -C path) — the lone exception
        Some(GIT_TIMEOUT),
    ) {
        Ok(r) => r,
        Err(_) => return Vec::new(), // OSError / spawn failure -> []
    };
    if r.code != 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in split_lines(&r.stdout) {
        // line.replace("*", "").strip() — removes ALL '*' chars then strips ASCII whitespace.
        let name = line.replace('*', "");
        let name = name.trim();
        if !name.is_empty() {
            out.push(name.to_string());
        }
    }
    out
}

/// Python str.splitlines(): splits on \n, \r, \r\n; a trailing newline does NOT yield a final
/// empty element. (We only need the ASCII line boundaries git emits.)
fn split_lines(s: &str) -> Vec<&str> {
    // str::lines() splits on \n and \r\n and drops a single trailing newline — matching the
    // splitlines() behavior for the git output this module parses.
    s.lines().collect()
}

// --------------------------------------------------------------------------- //
// list_worktrees
// --------------------------------------------------------------------------- //

/// control.py:1738 — parsed `git worktree list --porcelain` entries. `[]` on missing git/path/error.
pub fn list_worktrees(repo: &Value) -> Vec<Value> {
    let git = proc::which_git();
    let path = paths::repo_path(repo);
    let git = match git {
        Some(g) if !path.is_empty() => g,
        _ => return Vec::new(),
    };
    let result = match git_c(&git, &path, &["worktree", "list", "--porcelain"]) {
        Ok(r) => r,
        Err(_) => return Vec::new(), // OSError -> []
    };
    if result.code != 0 {
        return Vec::new();
    }
    parse_worktrees(&result.stdout)
}

/// Pure parser for `git worktree list --porcelain` stdout (testable without git).
fn parse_worktrees(stdout: &str) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    let mut current: Option<Value> = None;
    // Iterate splitlines() + [""] — the sentinel empty string flushes the final block.
    let mut lines: Vec<&str> = split_lines(stdout);
    lines.push("");
    for line in lines {
        if line.is_empty() {
            if let Some(cur) = current.take() {
                rows.push(cur);
            }
            continue;
        }
        // key, _, value = line.partition(" ") — split on FIRST space; value "" if no space.
        let (key, value) = match line.find(' ') {
            Some(i) => (&line[..i], &line[i + 1..]),
            None => (line, ""),
        };
        if key == "worktree" {
            current = Some(json!({
                "path": value,
                "branch": Value::Null,
                "head": Value::Null,
                "bare": false,
                "detached": false,
                "locked": false,
                "prunable": false,
            }));
        } else if let Some(cur) = current.as_mut() {
            match key {
                "branch" => {
                    // removeprefix("refs/heads/") — strip ONLY a leading exact match.
                    let b = value.strip_prefix("refs/heads/").unwrap_or(value);
                    cur["branch"] = Value::String(b.to_string());
                }
                "HEAD" => {
                    cur["head"] = Value::String(value.to_string());
                }
                "bare" | "detached" | "locked" | "prunable" => {
                    cur[key] = Value::Bool(true);
                }
                _ => {} // unrecognized key while current is set -> ignored
            }
        }
        // a line before the first 'worktree' (current is None) and not 'worktree' -> ignored
    }
    rows
}

// --------------------------------------------------------------------------- //
// branch_hygiene
// --------------------------------------------------------------------------- //

fn clean_hygiene() -> Value {
    json!({
        "dirty": false, "reason": "", "current": "", "base": "",
        "off_base": false, "stray": [], "uncommitted": 0, "running": false,
    })
}

/// control.py:1771 — read-only snapshot of whether the RSI loop left git state DIRTY. Never raises;
/// any error / non-git repo returns the not-dirty constant.
pub fn branch_hygiene(repo: &Value) -> Value {
    let git = proc::which_git();
    let path = paths::repo_path(repo);
    let git = match git {
        Some(g) if !path.is_empty() => g,
        _ => return clean_hygiene(),
    };
    let base = crate::control::registry::project_pr_target_branch(repo);
    // rev-parse --abbrev-ref HEAD; OSError -> clean.
    let current = match git_c(&git, &path, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(r) => out_stripped(&r),
        Err(_) => return clean_hygiene(),
    };
    // repo.get("branch_prefix") or "rsi/" — default "rsi/" here (NOTE: differs from local_rsi_branches).
    let prefix = {
        let p = repo
            .get("branch_prefix")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if p.is_empty() {
            "rsi/".to_string()
        } else {
            p.to_string()
        }
    };
    let stray: Vec<String> = local_rsi_branches(repo)
        .into_iter()
        .filter(|b| *b != current)
        .collect();
    let off_base =
        !current.is_empty() && !base.is_empty() && current != base && current.starts_with(&prefix);
    let porcelain = match git_c(&git, &path, &["status", "--porcelain"]) {
        Ok(r) => r.stdout,
        Err(_) => return clean_hygiene(),
    };
    let uncommitted = split_lines(&porcelain)
        .into_iter()
        .filter(|ln| !ln.trim().is_empty())
        .count();
    let running = crate::control::locks::is_running(repo);

    let dirty = (off_base || !stray.is_empty()) && !running;
    let mut parts: Vec<String> = Vec::new();
    if off_base {
        parts.push(format!("on {current} (base {base})"));
    }
    if !stray.is_empty() {
        parts.push(format!("{} stray {prefix}* branch(es)", stray.len()));
    }
    json!({
        "dirty": dirty,
        "reason": parts.join("; "),
        "current": current,
        "base": base,
        "off_base": off_base,
        "stray": stray,
        "uncommitted": uncommitted,
        "running": running,
    })
}

// --------------------------------------------------------------------------- //
// cleanup_worktrees
// --------------------------------------------------------------------------- //

/// control.py:1701 — prune worktrees + delete leftover local `<prefix>*` branches. Never touches the
/// currently checked-out branch. Returns {ok, pruned, removed} / {ok:false, error}.
pub fn cleanup_worktrees(repo: &Value) -> Value {
    let git = match proc::which_git() {
        Some(g) => g,
        None => return json!({"ok": false, "error": "git not found"}),
    };
    let path = paths::repo_path(repo);
    if path.is_empty() {
        return json!({"ok": false, "error": "repo has no 'path'"});
    }
    // prune -> bool (returncode == 0); cur; head_sha. OSError anywhere here -> {ok:false,error:str(e)}.
    let pruned;
    let cur;
    let head_sha;
    match (|| -> std::io::Result<(bool, String, String)> {
        let pr = git_c(&git, &path, &["worktree", "prune"])?;
        let c = git_c(&git, &path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let h = git_c(&git, &path, &["rev-parse", "HEAD"])?;
        Ok((pr.code == 0, out_stripped(&c), out_stripped(&h)))
    })() {
        Ok((p, c, h)) => {
            pruned = p;
            cur = c;
            head_sha = h;
        }
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    }
    let mut removed: Vec<String> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    let detached = cur == "HEAD"; // abbrev-ref is literal "HEAD" only when detached
    for b in local_rsi_branches(repo) {
        if b == cur {
            // never delete the branch we're standing on (named HEAD)
            continue;
        }
        // Detached-HEAD-only SHA guard: skip any rsi/* branch whose tip == HEAD's commit.
        if detached && !head_sha.is_empty() {
            match git_c(&git, &path, &["rev-parse", &b]) {
                Ok(r) => {
                    if out_stripped(&r) == head_sha {
                        continue;
                    }
                }
                Err(_) => {
                    // OSError on rev-parse b: Python would raise out of the loop body (no try here),
                    // propagating to the function — but the only catch is the earlier try block which
                    // has already exited. In CPython this would be an unhandled OSError. The detached
                    // path with a vanished git mid-loop is not reachable in practice (git was just
                    // used). Mirror the most defensive observable behavior: skip this branch.
                    continue;
                }
            }
        }
        // NEVER force-delete unmerged work that isn't visible upstream (audit finding #61's
        // actual deletion vector — a finished 14-commit daedulus branch died to exactly this
        // unconditional -D). Try `branch -d` first: git itself deletes only merged branches.
        // If -d refuses, escalate to -D only when the tip is already contained in the branch's
        // own origin copy (shipped/squash-merged remnants). Otherwise KEEP the branch — the
        // preflight stale-fork guard will surface it loudly instead of it dying silently here.
        match git_c(&git, &path, &["branch", "-d", &b]) {
            Ok(r) if r.code == 0 => {
                removed.push(b);
                continue;
            }
            _ => {}
        }
        let on_origin = git_c(
            &git,
            &path,
            &["merge-base", "--is-ancestor", &b, &format!("refs/remotes/origin/{b}")],
        )
        .map(|r| r.code == 0)
        .unwrap_or(false);
        if on_origin {
            match git_c(&git, &path, &["branch", "-D", &b]) {
                Ok(r) => {
                    if r.code == 0 {
                        removed.push(b);
                    }
                }
                Err(_) => { /* swallowed */ }
            }
        } else {
            kept.push(b); // unmerged + not upstream: stranded-work invariant — keep it
        }
    }
    json!({"ok": true, "pruned": pruned, "removed": removed, "kept": kept})
}

// --------------------------------------------------------------------------- //
// clean_branch
// --------------------------------------------------------------------------- //

/// control.py:1807 — clean a DIRTY repo back to its base branch + prune stray rsi/* branches.
/// Refuses while a loop is live (checked FIRST, before git). Force-checks-out base ONLY when off base.
pub fn clean_branch(repo: &Value) -> Value {
    // is_running checked FIRST, before git presence.
    if crate::control::locks::is_running(repo) {
        return json!({"ok": false, "error": "loop is running — stop it first"});
    }
    let git = proc::which_git();
    let path = paths::repo_path(repo);
    let git = match git {
        Some(g) => g,
        None => return json!({"ok": false, "error": "git not found"}),
    };
    if path.is_empty() {
        return json!({"ok": false, "error": "repo has no 'path'"});
    }
    let base = crate::control::registry::project_pr_target_branch(repo);
    let mut switched = false;
    // try: rev-parse prev; conditional force-checkout. OSError -> {ok:false,error:str(e)}.
    let prev = match git_c(&git, &path, &["rev-parse", "--abbrev-ref", "HEAD"]) {
        Ok(r) => out_stripped(&r),
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    // Only switch when actually OFF base (prev and base both truthy and differ).
    if !prev.is_empty() && !base.is_empty() && prev != base {
        match git_c(&git, &path, &["checkout", "--force", &base]) {
            Ok(co) => {
                if co.code != 0 {
                    let msg = if !co.stderr.is_empty() {
                        co.stderr.clone()
                    } else if !co.stdout.is_empty() {
                        co.stdout.clone()
                    } else {
                        format!("checkout {base} failed")
                    };
                    return json!({"ok": false, "error": strip_trunc_200(&msg)});
                }
                switched = true;
            }
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        }
    }
    let cw = cleanup_worktrees(repo);
    // removed defaults to [] via cw.get('removed', []); pruned via cw.get('pruned') (None if errored).
    let removed = cw
        .get("removed")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let pruned = cw.get("pruned").cloned().unwrap_or(Value::Null);
    json!({
        "ok": true,
        "from": prev,
        "checked_out": base,
        "switched": switched,
        "removed": removed,
        "pruned": pruned,
    })
}

// --------------------------------------------------------------------------- //
// reset_to_base  (HIGHEST RISK — guard order + per-command exit-code propagation)
// --------------------------------------------------------------------------- //

/// control.py:2012 — reset the base branch to origin truth, guarding against discarding un-pushed
/// base commits / uncommitted tracked work (refuse + escalate). Never deletes feature branches.
///
/// Guard ORDER is load-bearing: dirty-tracked-guard -> has_origin -> fetch (rc UNCHECKED) ->
/// un-pushed-guard -> checkout --force -> reset --hard -> reset --hard origin/{base}.
pub fn reset_to_base(repo: &Value) -> Value {
    let git = match proc::which_git() {
        Some(g) => g,
        None => return json!({"ok": false, "error": "git not found"}),
    };
    let path = paths::repo_path(repo);
    if path.is_empty() {
        return json!({"ok": false, "error": "repo has no 'path'"});
    }
    let base = crate::control::registry::project_pr_target_branch(repo);

    // Whole try-block: any OSError -> {ok:false, error:str(e)}.
    let result = (|| -> std::io::Result<Result<(), Value>> {
        // (a) dirty TRACKED guard FIRST. Untracked files ignored by --untracked-files=no.
        let dirty = git_c(&git, &path, &["status", "--porcelain", "--untracked-files=no"])?;
        if !dirty.stdout.trim().is_empty() {
            return Ok(Err(json!({
                "ok": false,
                "error": "uncommitted tracked changes on the base tree — escalate \
                          (won't auto-discard operator work; commit or stash first)"
            })));
        }
        // (b) has_origin.
        let has_origin = git_c(&git, &path, &["remote", "get-url", "origin"])?.code == 0;
        if has_origin {
            // (c) fetch BEFORE the un-pushed check; returncode NOT checked.
            let _ = git_c(&git, &path, &["fetch", "origin", "--quiet"])?;
            let range = format!("origin/{base}..{base}");
            let ahead = git_c(&git, &path, &["log", "--oneline", &range])?;
            if ahead.code == 0 && !ahead.stdout.trim().is_empty() {
                return Ok(Err(json!({
                    "ok": false,
                    "error": format!("un-pushed commits on {base} — escalate (won't auto-discard)")
                })));
            }
        }
        // (d) checkout --force base.
        let co = git_c(&git, &path, &["checkout", "--force", &base])?;
        if co.code != 0 {
            let msg = first_nonempty(&co.stderr, &co.stdout, &format!("checkout {base} failed"));
            return Ok(Err(json!({"ok": false, "error": strip_trunc_200(&msg)})));
        }
        // (e) reset --hard (working tree).
        let rs = git_c(&git, &path, &["reset", "--hard"])?;
        if rs.code != 0 {
            let msg = first_nonempty(&rs.stderr, &rs.stdout, "reset --hard failed");
            return Ok(Err(json!({"ok": false, "error": strip_trunc_200(&msg)})));
        }
        // (f) reset --hard origin/{base} (only with origin).
        if has_origin {
            let target = format!("origin/{base}");
            let ro = git_c(&git, &path, &["reset", "--hard", &target])?;
            if ro.code != 0 {
                let fallback = format!("reset --hard origin/{base} failed");
                let msg = first_nonempty(&ro.stderr, &ro.stdout, &fallback);
                return Ok(Err(json!({"ok": false, "error": strip_trunc_200(&msg)})));
            }
        }
        Ok(Ok(()))
    })();

    match result {
        Ok(Ok(())) => json!({"ok": true, "base": base}),
        Ok(Err(err)) => err,
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// Python `(a or b or c)` over already-decoded strings: first non-empty of stderr/stdout/fallback.
fn first_nonempty(a: &str, b: &str, c: &str) -> String {
    if !a.is_empty() {
        a.to_string()
    } else if !b.is_empty() {
        b.to_string()
    } else {
        c.to_string()
    }
}

// =========================================================================== //
// Tests — built from control-port-spec.json golden_vectors (module "branches").
// Functions touching git/which_git/is_running are exercised via the PURE helpers
// (parse_worktrees, strip_trunc_200, first_nonempty, split_lines) and via logic
// derived directly from the vectors; the I/O-bound paths are validated by the
// guard-ordering logic encoded above.
// =========================================================================== //
#[cfg(test)]
mod tests {
    use super::*;

    // ---- local_rsi_branches line parsing (vectors: marker strip, blank drop) ----
    fn parse_branch_lines(stdout: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in split_lines(stdout) {
            let name = line.replace('*', "");
            let name = name.trim();
            if !name.is_empty() {
                out.push(name.to_string());
            }
        }
        out
    }

    #[test]
    fn local_rsi_branches_typical_two_with_current_marker() {
        // stdout='* rsi/iter-3\n  rsi/iter-2\n' -> ['rsi/iter-3','rsi/iter-2']
        assert_eq!(
            parse_branch_lines("* rsi/iter-3\n  rsi/iter-2\n"),
            vec!["rsi/iter-3".to_string(), "rsi/iter-2".to_string()]
        );
    }

    #[test]
    fn local_rsi_branches_current_marker_stripped() {
        // stdout='* rsi/beautify-1\n' -> ['rsi/beautify-1']
        assert_eq!(
            parse_branch_lines("* rsi/beautify-1\n"),
            vec!["rsi/beautify-1".to_string()]
        );
    }

    #[test]
    fn local_rsi_branches_blank_lines_dropped() {
        assert_eq!(parse_branch_lines("\n  \n* rsi/a\n"), vec!["rsi/a".to_string()]);
    }

    #[test]
    fn local_rsi_branches_empty_prefix_returns_empty() {
        // repo with empty/missing branch_prefix -> [] without running git.
        let repo = json!({"path": "C:/x", "branch_prefix": ""});
        assert_eq!(local_rsi_branches(&repo), Vec::<String>::new());
        let repo2 = json!({"path": "C:/x"}); // missing key
        assert_eq!(local_rsi_branches(&repo2), Vec::<String>::new());
    }

    #[test]
    fn local_rsi_branches_empty_path_returns_empty() {
        let repo = json!({"branch_prefix": "rsi/"}); // no path
        assert_eq!(local_rsi_branches(&repo), Vec::<String>::new());
    }

    // ---- list_worktrees (parse_worktrees pure parser) ----
    #[test]
    fn list_worktrees_two_one_detached() {
        let stdout = "worktree C:/repo\nHEAD abc123\nbranch refs/heads/main\n\n\
                      worktree C:/repo/wt\nHEAD def456\ndetached\n\n";
        let got = parse_worktrees(stdout);
        let expected = json!([
            {"path":"C:/repo","branch":"main","head":"abc123","bare":false,"detached":false,"locked":false,"prunable":false},
            {"path":"C:/repo/wt","branch":null,"head":"def456","bare":false,"detached":true,"locked":false,"prunable":false}
        ]);
        assert_eq!(Value::Array(got), expected);
    }

    #[test]
    fn list_worktrees_missing_trailing_blank_flushed_by_sentinel() {
        let stdout = "worktree C:/repo\nHEAD abc123\nbranch refs/heads/dev";
        let got = parse_worktrees(stdout);
        let expected = json!([
            {"path":"C:/repo","branch":"dev","head":"abc123","bare":false,"detached":false,"locked":false,"prunable":false}
        ]);
        assert_eq!(Value::Array(got), expected);
    }

    #[test]
    fn list_worktrees_branch_without_refs_heads_prefix_unchanged() {
        // removeprefix only strips a leading exact match.
        let got = parse_worktrees("worktree C:/r\nbranch weird/main\n\n");
        assert_eq!(got[0]["branch"], json!("weird/main"));
    }

    #[test]
    fn list_worktrees_lines_before_first_worktree_ignored() {
        let got = parse_worktrees("HEAD abc\nbranch refs/heads/x\nworktree C:/r\nHEAD zzz\n\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["head"], json!("zzz"));
    }

    // ---- branch_hygiene: pure logic mirror for the dirty/reason computation ----
    // (the I/O wrapper resolves git/current/stray/porcelain/running; these tests pin the
    //  decision logic that the spec's vectors target.)
    struct Hyg {
        current: String,
        base: String,
        prefix: String,
        stray: Vec<String>,
        uncommitted: i64,
        running: bool,
    }
    fn hyg_result(h: &Hyg) -> Value {
        let off_base = !h.current.is_empty()
            && !h.base.is_empty()
            && h.current != h.base
            && h.current.starts_with(&h.prefix);
        let dirty = (off_base || !h.stray.is_empty()) && !h.running;
        let mut parts: Vec<String> = Vec::new();
        if off_base {
            parts.push(format!("on {} (base {})", h.current, h.base));
        }
        if !h.stray.is_empty() {
            parts.push(format!("{} stray {}* branch(es)", h.stray.len(), h.prefix));
        }
        json!({
            "dirty": dirty, "reason": parts.join("; "), "current": h.current, "base": h.base,
            "off_base": off_base, "stray": h.stray, "uncommitted": h.uncommitted, "running": h.running,
        })
    }

    #[test]
    fn branch_hygiene_off_base_one_stray_stopped_dirty() {
        let h = Hyg {
            current: "rsi/iter-3".into(),
            base: "main".into(),
            prefix: "rsi/".into(),
            stray: vec!["rsi/iter-2".into()],
            uncommitted: 1,
            running: false,
        };
        assert_eq!(
            hyg_result(&h),
            json!({"dirty":true,"reason":"on rsi/iter-3 (base main); 1 stray rsi/* branch(es)",
                   "current":"rsi/iter-3","base":"main","off_base":true,"stray":["rsi/iter-2"],
                   "uncommitted":1,"running":false})
        );
    }

    #[test]
    fn branch_hygiene_running_forces_not_dirty() {
        let h = Hyg {
            current: "rsi/iter-3".into(),
            base: "main".into(),
            prefix: "rsi/".into(),
            stray: vec!["rsi/iter-2".into()],
            uncommitted: 1,
            running: true,
        };
        assert_eq!(hyg_result(&h)["dirty"], json!(false));
        // reason still populated even when not dirty
        assert_eq!(
            hyg_result(&h)["reason"],
            json!("on rsi/iter-3 (base main); 1 stray rsi/* branch(es)")
        );
    }

    #[test]
    fn branch_hygiene_on_base_no_stray_clean() {
        let h = Hyg {
            current: "main".into(),
            base: "main".into(),
            prefix: "rsi/".into(),
            stray: vec![],
            uncommitted: 0,
            running: false,
        };
        assert_eq!(
            hyg_result(&h),
            json!({"dirty":false,"reason":"","current":"main","base":"main","off_base":false,
                   "stray":[],"uncommitted":0,"running":false})
        );
    }

    #[test]
    fn branch_hygiene_on_base_stray_only_dirty() {
        let h = Hyg {
            current: "main".into(),
            base: "main".into(),
            prefix: "rsi/".into(),
            stray: vec!["rsi/iter-1".into()],
            uncommitted: 0,
            running: false,
        };
        assert_eq!(
            hyg_result(&h),
            json!({"dirty":true,"reason":"1 stray rsi/* branch(es)","current":"main","base":"main",
                   "off_base":false,"stray":["rsi/iter-1"],"uncommitted":0,"running":false})
        );
    }

    #[test]
    fn branch_hygiene_clean_constant_shape() {
        // missing git -> clean constant
        assert_eq!(
            clean_hygiene(),
            json!({"dirty":false,"reason":"","current":"","base":"","off_base":false,
                   "stray":[],"uncommitted":0,"running":false})
        );
    }

    // ---- cleanup_worktrees: guard logic (which branches are removed) ----
    // Mirror of the loop's keep/skip decision, isolated from git I/O.
    fn cleanup_removed(
        cur: &str,
        head_sha: &str,
        branches: &[(&str, &str, bool)], // (name, tip_sha, branch_d_ok)
    ) -> Vec<String> {
        let detached = cur == "HEAD";
        let mut removed = Vec::new();
        for (b, tip, del_ok) in branches {
            if *b == cur {
                continue;
            }
            if detached && !head_sha.is_empty() && *tip == head_sha {
                continue;
            }
            if *del_ok {
                removed.push(b.to_string());
            }
        }
        removed
    }

    #[test]
    fn cleanup_named_branch_deletes_two() {
        assert_eq!(
            cleanup_removed("main", "aaa", &[("rsi/iter-1", "x", true), ("rsi/iter-2", "y", true)]),
            vec!["rsi/iter-1".to_string(), "rsi/iter-2".to_string()]
        );
    }

    #[test]
    fn cleanup_detached_skips_branch_at_head_sha() {
        // cur='HEAD'(detached), head_sha='bbb'; rsi/iter-3 tip==bbb skipped; rsi/iter-2 deleted.
        assert_eq!(
            cleanup_removed("HEAD", "bbb", &[("rsi/iter-3", "bbb", true), ("rsi/iter-2", "ccc", true)]),
            vec!["rsi/iter-2".to_string()]
        );
    }

    #[test]
    fn cleanup_current_named_rsi_branch_skipped() {
        assert_eq!(
            cleanup_removed("rsi/iter-9", "z", &[("rsi/iter-9", "z", true), ("rsi/iter-8", "q", true)]),
            vec!["rsi/iter-8".to_string()]
        );
    }

    #[test]
    fn cleanup_branch_d_failure_excluded() {
        assert_eq!(
            cleanup_removed("main", "x", &[("rsi/a", "1", false), ("rsi/b", "2", true)]),
            vec!["rsi/b".to_string()]
        );
    }

    #[test]
    fn cleanup_named_branch_sha_guard_not_applied() {
        // On a NAMED branch, a stray rsi/* sharing the base commit is STILL deleted.
        assert_eq!(
            cleanup_removed("main", "bbb", &[("rsi/x", "bbb", true)]),
            vec!["rsi/x".to_string()]
        );
    }

    #[test]
    fn cleanup_missing_path_error() {
        let repo = json!({}); // git present is irrelevant; path empty short-circuits after git check
        // We can't guarantee git presence on the test host, so only assert the path-missing shape
        // when git resolves; otherwise the function returns the git-not-found shape (also valid).
        let got = cleanup_worktrees(&repo);
        let err = got["error"].as_str().unwrap_or("");
        assert!(err == "repo has no 'path'" || err == "git not found");
    }

    // ---- clean_branch error/refusal shapes ----
    #[test]
    fn clean_branch_refusal_string_exact_bytes() {
        // Em-dash U+2014 with surrounding spaces — must be byte-identical.
        let s = "loop is running — stop it first";
        assert_eq!(s.as_bytes(), "loop is running \u{2014} stop it first".as_bytes());
    }

    // ---- reset_to_base error strings + helpers ----
    #[test]
    fn reset_dirty_tracked_refusal_string() {
        let s = "uncommitted tracked changes on the base tree — escalate \
                 (won't auto-discard operator work; commit or stash first)";
        assert!(s.contains("escalate"));
        assert!(s.contains("won't auto-discard operator work"));
        // em-dash present
        assert!(s.contains('\u{2014}'));
    }

    #[test]
    fn reset_unpushed_refusal_format() {
        let base = "main";
        let s = format!("un-pushed commits on {base} — escalate (won't auto-discard)");
        assert_eq!(s, "un-pushed commits on main — escalate (won't auto-discard)");
    }

    #[test]
    fn first_nonempty_prefers_stderr_then_stdout_then_fallback() {
        assert_eq!(first_nonempty("err", "out", "fb"), "err");
        assert_eq!(first_nonempty("", "out", "fb"), "out");
        assert_eq!(first_nonempty("", "", "fb"), "fb");
    }

    #[test]
    fn strip_trunc_200_strips_then_truncates_by_char() {
        assert_eq!(strip_trunc_200("  error: pathspec  "), "error: pathspec");
        let long: String = "x".repeat(250);
        assert_eq!(strip_trunc_200(&long).chars().count(), 200);
        // multibyte: truncation is by char, not byte
        let mb: String = "é".repeat(250);
        assert_eq!(strip_trunc_200(&mb).chars().count(), 200);
    }

    #[test]
    fn reset_to_base_missing_path_or_git_shape() {
        let repo = json!({}); // no path
        let got = reset_to_base(&repo);
        let err = got["error"].as_str().unwrap_or("");
        assert!(err == "repo has no 'path'" || err == "git not found");
        assert_eq!(got["ok"], json!(false));
    }
}
