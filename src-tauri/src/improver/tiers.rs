//! Tiered code-autonomy policy (RSI v3) — per-repo `tiers` config in repos.json.
//!
//! Two tiers, both opt-in via a repos.json row key `"tiers"`; rows WITHOUT the key are
//! byte-identical legacy (every function here is a no-op for them):
//!
//!   "tiers": {
//!       "money_globs": ["trader.py", ...],   // money-path files: auto-land ONLY under an
//!                                            // ACTIVE non-regressing fitness needle
//!       "protected":   ["promote.py", ".state/", ...]  // grader/leash files: ANY diff touching
//!                                            // them is auto-reverted (write-protected grader)
//!   }
//!
//! Why: failure catalog #1 (value-blind objective) — the graders that measure fitness
//! (EVAL_CMD, the test gate, the leash authority) must not be editable by the loop they grade,
//! or the loop optimizes the measuring stick instead of the metric. And a money-path change may
//! only auto-merge when the fitness needle actually measured it (base AND after score); with the
//! needle inactive it ships as a PR for a human merge. Landing stays the default for everything
//! else; live exposure stays governed inside the target repo (kairos: promote.py + the backtest
//! verdict + KILL) — this module only routes SHIPPING, it grants nothing.
//!
//! Matching rules (shared by both tiers): an entry ending in '/' is a directory prefix
//! (".state/" matches ".state/kairos.db"); an entry containing '*'/'?' is a glob ('*' spans '/',
//! '?' is one char); anything else is an exact repo-relative path. Paths are normalized to
//! forward slashes on both sides (git emits forward slashes; config may not).

use crate::improver::ctx::Ctx;
use crate::improver::gates;
use serde_json::Value;

// --------------------------------------------------------------------------- #
// changed-file discovery
// --------------------------------------------------------------------------- #

/// The files changed on the iteration branch vs `base_branch`:
/// `git diff --name-only <base>...HEAD` (three-dot: merge-base..HEAD, so a stale local base
/// can't blame the branch for other people's files). Repo-relative, forward slashes, no blanks.
pub fn changed_files(c: &Ctx, base_branch: &str) -> Vec<String> {
    let range = format!("{base_branch}...HEAD");
    let out = c.git(&["diff", "--name-only", &range], 120).stdout;
    out.lines()
        .map(|l| l.trim().replace('\\', "/"))
        .filter(|l| !l.is_empty())
        .collect()
}

/// The files named by a unified diff's FILE HEADERS (`--- a/<p>` / `+++ b/<p>` / `rename from` /
/// `rename to`). Used where the caller already holds the committed diff text (the anti-gaming
/// gate) and must not shell out again. Content lines can't spoof these: inside a hunk a literal
/// `--- a/x` line is prefixed with ' ', '+' or '-' (so it starts `+---`/`----`/` ---`, never
/// `--- `). `/dev/null` (add/delete sides) is skipped; simple `"quoted"` paths are unwrapped.
pub fn files_from_diff(diff_text: &str) -> Vec<String> {
    // trim + unwrap simple `"quoted"` paths (embedded escapes are not un-escaped; tiers entries
    // never need them) + normalize separators. Prefix-stripping is done per header kind below —
    // only `---`/`+++` carry the a/ b/ mangle, rename lines are raw paths.
    fn norm(raw: &str) -> String {
        raw.trim().trim_matches('"').replace('\\', "/")
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        if p.is_empty() || p == "/dev/null" {
            return;
        }
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for ln in diff_text.lines() {
        if let Some(rest) = ln.strip_prefix("+++ ") {
            let p = norm(rest);
            push(p.strip_prefix("b/").unwrap_or(&p).to_string());
        } else if let Some(rest) = ln.strip_prefix("--- ") {
            let p = norm(rest);
            push(p.strip_prefix("a/").unwrap_or(&p).to_string());
        } else if let Some(rest) = ln.strip_prefix("rename from ") {
            push(norm(rest));
        } else if let Some(rest) = ln.strip_prefix("rename to ") {
            push(norm(rest));
        }
    }
    out
}

// --------------------------------------------------------------------------- #
// entry matching
// --------------------------------------------------------------------------- #

/// Does one tiers entry match one changed file? Trailing '/' = dir prefix; '*'/'?' = glob;
/// else exact path equality. Both sides normalized to forward slashes.
pub fn entry_matches(entry: &str, file: &str) -> bool {
    let e = entry.trim().replace('\\', "/");
    if e.is_empty() {
        return false;
    }
    let f = file.replace('\\', "/");
    if let Some(_dir) = e.strip_suffix('/') {
        // dir-prefix rule: ".state/" matches ".state/kairos.db" and ".state/a/b" — the entry
        // string itself (WITH the slash) must prefix the path, so ".state/" can't match ".statex".
        return f.starts_with(&e);
    }
    if e.contains('*') || e.contains('?') {
        return glob_match(&e, &f);
    }
    e == f
}

/// Minimal glob: '*' matches any (possibly empty) sequence INCLUDING '/', '?' matches exactly
/// one char, everything else is literal. Iterative backtracking (no regex dependency, no
/// pathological blowup: single star-resume pointer).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            // backtrack: let the last '*' swallow one more char.
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The string entries of `row_tiers[key]` (non-strings and blanks dropped). [] when the key is
/// absent / not a list / row_tiers is not an object — the legacy no-op shape.
fn str_list(row_tiers: &Value, key: &str) -> Vec<String> {
    match row_tiers.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

// --------------------------------------------------------------------------- #
// tier policies
// --------------------------------------------------------------------------- #

/// Grader write-protection: the first changed file matching a `protected` entry yields the
/// revert reason (routed through the EXISTING anti-gaming revert machinery by gates.rs). None
/// when the row has no `protected` list (legacy) or nothing matches.
pub fn protected_violation(row_tiers: &Value, files: &[String]) -> Option<String> {
    let entries = str_list(row_tiers, "protected");
    if entries.is_empty() {
        return None;
    }
    for f in files {
        if entries.iter().any(|e| entry_matches(e, f)) {
            return Some(format!(
                "diff touches protected grader/leash file {f} — auto-reverted (write-protected grader)"
            ));
        }
    }
    None
}

/// Money-path ship override: when any changed file matches `money_globs` AND the configured
/// ship mode is "auto-merge" AND the eval needle was NOT active for this iteration, the diff
/// ships as `Some("pr")` — a human merges it. A money-path diff may auto-land ONLY under an
/// ACTIVE non-regressing fitness needle. Everything else (non-money diff, needle active, or a
/// non-auto-merge configured mode) is None: use the configured mode unchanged.
pub fn money_ship_override(
    row_tiers: &Value,
    files: &[String],
    eval_active: bool,
    configured_ship: &str,
) -> Option<String> {
    if configured_ship != "auto-merge" || eval_active {
        return None;
    }
    let globs = str_list(row_tiers, "money_globs");
    if globs.is_empty() {
        return None;
    }
    let money = files
        .iter()
        .any(|f| globs.iter().any(|g| entry_matches(g, f)));
    if money {
        Some("pr".to_string())
    } else {
        None
    }
}

// --------------------------------------------------------------------------- #
// eval-needle activity
// --------------------------------------------------------------------------- #

/// Was the fitness needle ACTIVE for this iteration? Re-derived Ctx-side (iteration.rs is not
/// in this workstream's writable scope, so no new ship() parameter): the eval gate is the only
/// writer of the heartbeat's `eval_score` key, and it runs BEFORE ship() every iteration that
/// has an EVAL_CMD — so at ship time `c.hb` (the in-memory object heartbeat.json is written
/// from) carries this iteration's after-score, a Number only when a float actually parsed
/// (Null on timeout / no-float, absent when the gate never ran). Requiring EVAL_CMD to be
/// non-empty RIGHT NOW (read fresh from repos.json) closes the stale-key case where the
/// operator removes EVAL_CMD mid-run. Constraint: the BASE score is never persisted anywhere,
/// so "base AND after measured" degrades to the strongest derivable signal — needle configured
/// + an after-score parsed; if absent => inactive.
pub fn eval_needle_active(c: &Ctx) -> bool {
    let cmd = gates::eval_cmd(c, &c.name);
    eval_active_from(&cmd, c.hb.get("eval_score"))
}

/// Pure core of [`eval_needle_active`]: non-empty EVAL_CMD + a NUMERIC heartbeat eval_score.
pub fn eval_active_from(eval_cmd: &str, hb_eval_score: Option<&Value>) -> bool {
    !eval_cmd.trim().is_empty() && matches!(hb_eval_score, Some(Value::Number(_)))
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kairos_tiers() -> Value {
        json!({
            "money_globs": ["trader.py", "kalshi_client.py", "config.json"],
            "protected": ["promote.py", "test_tracking.py", "backtest.py", "report.py",
                          "tools/fitness.py", "AGENTS.md", "HARNESS.md", ".state/"]
        })
    }

    fn v(files: &[&str]) -> Vec<String> {
        files.iter().map(|s| s.to_string()).collect()
    }

    // ---- entry matching: exact / dir-prefix / glob ----

    #[test]
    fn exact_name_matches_only_exact_path() {
        assert!(entry_matches("trader.py", "trader.py"));
        assert!(!entry_matches("trader.py", "not_trader.py"));
        assert!(!entry_matches("trader.py", "sub/trader.py")); // exact means the full path
        assert!(entry_matches("tools/fitness.py", "tools/fitness.py"));
        assert!(!entry_matches("tools/fitness.py", "tools/fitness.pyc"));
    }

    #[test]
    fn dir_prefix_rule_state_dir() {
        // the '.state/' dir rule from the kairos row
        assert!(entry_matches(".state/", ".state/kairos.db"));
        assert!(entry_matches(".state/", ".state/sub/deep.json"));
        assert!(!entry_matches(".state/", ".statex/kairos.db")); // slash is part of the prefix
        assert!(!entry_matches(".state/", "state/kairos.db"));
        assert!(!entry_matches(".state/", "x/.state/kairos.db")); // prefix, not substring
    }

    #[test]
    fn glob_star_and_question() {
        assert!(entry_matches("*.json", "config.json"));
        assert!(entry_matches("*.json", "deep/dir/config.json")); // '*' spans '/'
        assert!(!entry_matches("*.json", "config.jsonc"));
        assert!(entry_matches("tools/*.py", "tools/fitness.py"));
        assert!(entry_matches("trader?.py", "trader2.py"));
        assert!(!entry_matches("trader?.py", "trader.py"));
    }

    #[test]
    fn backslash_paths_normalize() {
        // config written with Windows separators still matches git's forward-slash output.
        assert!(entry_matches("tools\\fitness.py", "tools/fitness.py"));
        assert!(entry_matches("tools/fitness.py", "tools\\fitness.py"));
    }

    // ---- protected_violation ----

    #[test]
    fn protected_violation_message_exact() {
        let t = kairos_tiers();
        let got = protected_violation(&t, &v(&["core.py", "promote.py"]));
        assert_eq!(
            got.as_deref(),
            Some("diff touches protected grader/leash file promote.py — auto-reverted (write-protected grader)")
        );
    }

    #[test]
    fn protected_violation_state_dir_and_fitness() {
        let t = kairos_tiers();
        assert_eq!(
            protected_violation(&t, &v(&[".state/kairos.db"])).as_deref(),
            Some("diff touches protected grader/leash file .state/kairos.db — auto-reverted (write-protected grader)")
        );
        assert!(protected_violation(&t, &v(&["tools/fitness.py"])).is_some());
        assert!(protected_violation(&t, &v(&["AGENTS.md"])).is_some());
    }

    #[test]
    fn protected_violation_none_for_clean_or_legacy() {
        let t = kairos_tiers();
        assert_eq!(protected_violation(&t, &v(&["core.py", "web/index.html"])), None);
        assert_eq!(protected_violation(&t, &[]), None);
        // legacy shapes: no tiers key / not an object / no protected list -> None
        assert_eq!(protected_violation(&json!({}), &v(&["promote.py"])), None);
        assert_eq!(protected_violation(&Value::Null, &v(&["promote.py"])), None);
        assert_eq!(
            protected_violation(&json!({"protected": "promote.py"}), &v(&["promote.py"])),
            None // wrong type (string, not list) is inert, not a crash
        );
    }

    // ---- money_ship_override truth table ----

    #[test]
    fn money_plus_active_eval_is_none() {
        // money-path diff under an ACTIVE needle: auto-land stands.
        let t = kairos_tiers();
        assert_eq!(money_ship_override(&t, &v(&["trader.py"]), true, "auto-merge"), None);
    }

    #[test]
    fn money_plus_inactive_eval_is_pr() {
        let t = kairos_tiers();
        assert_eq!(
            money_ship_override(&t, &v(&["trader.py"]), false, "auto-merge").as_deref(),
            Some("pr")
        );
        assert_eq!(
            money_ship_override(&t, &v(&["core.py", "config.json"]), false, "auto-merge").as_deref(),
            Some("pr")
        );
    }

    #[test]
    fn non_money_is_none() {
        let t = kairos_tiers();
        assert_eq!(money_ship_override(&t, &v(&["core.py", "app.py"]), false, "auto-merge"), None);
        assert_eq!(money_ship_override(&t, &[], false, "auto-merge"), None);
    }

    #[test]
    fn configured_pr_is_none() {
        // ship=pr configured: nothing to downgrade — the override only bites auto-merge.
        let t = kairos_tiers();
        assert_eq!(money_ship_override(&t, &v(&["trader.py"]), false, "pr"), None);
        assert_eq!(money_ship_override(&t, &v(&["trader.py"]), false, "local"), None);
        assert_eq!(money_ship_override(&t, &v(&["trader.py"]), false, "push"), None);
    }

    #[test]
    fn legacy_row_is_none() {
        assert_eq!(money_ship_override(&json!({}), &v(&["trader.py"]), false, "auto-merge"), None);
        assert_eq!(money_ship_override(&Value::Null, &v(&["trader.py"]), false, "auto-merge"), None);
    }

    // ---- files_from_diff ----

    #[test]
    fn diff_headers_parse_add_modify_delete_rename() {
        let diff = "\
diff --git a/trader.py b/trader.py
--- a/trader.py
+++ b/trader.py
@@ -1,2 +1,2 @@
-old
+new
diff --git a/new_file.py b/new_file.py
--- /dev/null
+++ b/new_file.py
diff --git a/gone.py b/gone.py
--- a/gone.py
+++ /dev/null
diff --git a/old_name.py b/new_name.py
rename from old_name.py
rename to new_name.py
";
        let files = files_from_diff(diff);
        assert_eq!(files, v(&["trader.py", "new_file.py", "gone.py", "old_name.py", "new_name.py"]));
    }

    #[test]
    fn diff_content_lines_cannot_spoof_headers() {
        // a hunk ADDING the literal text '--- a/promote.py' starts with '+', not '--- '.
        let diff = "\
diff --git a/notes.md b/notes.md
--- a/notes.md
+++ b/notes.md
@@ -1 +1,2 @@
 ctx
+--- a/promote.py
";
        assert_eq!(files_from_diff(diff), v(&["notes.md"]));
    }

    #[test]
    fn diff_deleting_protected_file_is_caught() {
        // a DELETION of promote.py has '+++ /dev/null' — the '--- a/' side must carry it.
        let diff = "--- a/promote.py\n+++ /dev/null\n";
        let t = kairos_tiers();
        assert!(protected_violation(&t, &files_from_diff(diff)).is_some());
    }

    // ---- eval_active_from ----

    #[test]
    fn eval_active_requires_cmd_and_numeric_score() {
        let score = json!(1.25);
        let null = Value::Null;
        assert!(eval_active_from("python tools/fitness.py", Some(&score)));
        // needle unconfigured -> inactive even with a lingering score
        assert!(!eval_active_from("", Some(&score)));
        assert!(!eval_active_from("   ", Some(&score)));
        // configured but no parsed score (Null / absent) -> inactive
        assert!(!eval_active_from("python tools/fitness.py", Some(&null)));
        assert!(!eval_active_from("python tools/fitness.py", None));
    }
}
