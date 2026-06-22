//! Backlog + no-op-streak helpers from improver/run_improver.py, ported bug-for-bug.
//!
//! The Python module keeps the backlog at module-global `BACKLOG = HERE / NAME / "backlog.md"`
//! (a markdown file of `- [ ]` / `- [x]` lines, optionally tier-tagged) and the iteration history
//! at `RUNTIME / "history.jsonl"`. Both are fields on [`Ctx`] now (`ctx.backlog`, `ctx.runtime`),
//! so these functions take `&Ctx`. `strip_tier` is pure (a free fn), matching `_strip_tier`.
//!
//! Scope (per the source's leading-`_` private fns):
//!   * [`top_backlog_item`]      — `_top_backlog_item`     (~1713)
//!   * [`mark_backlog_done`]     — `_mark_backlog_done`    (~1725)
//!   * [`unchecked_backlog_count`] — `_unchecked_backlog_count` (~1703)
//!   * [`needs_goal_skip`]       — `_needs_goal_skip`      (~1689)
//!   * [`recent_noop_streak`]    — `_recent_noop_streak`   (~1667)
//!   * [`strip_tier`]            — `_strip_tier`           (~1657)
//!
//! Quirks preserved: the `- [ ]`/`- [x]` prefix is matched on the STRIPPED line then sliced at
//! byte 5 (`s[5:]`), the tier tag is the case-insensitive `[chore|feature|refactor|architecture]`
//! prefix with a default of `"chore"`, history is scanned bottom-up and any non-noop terminal status
//! (or a JSON decode error) ends the streak, and `mark_backlog_done` rewrites only the FIRST matching
//! unchecked line via a single `replace("- [ ]","- [x]",1)`.

use crate::improver::ctx::Ctx;
use serde_json::Value;

/// run_improver._strip_tier (~1657-1664): split a leading
/// `[chore|feature|refactor|architecture]` ambition tag off a backlog item. Returns
/// `(text_without_tag, tier)`. Default `"chore"` = today's safe smallest-change behavior, so an
/// untagged (legacy) backlog behaves byte-identically; higher tiers lift the smallest-change ceiling.
///
/// Mirrors `re.match(r"\[(chore|feature|refactor|architecture)\]\s*", (text or "").strip(), re.I)`:
///   * `(text or "").strip()` — Python `str.strip()` removes leading/trailing ASCII+unicode
///     whitespace; we use `str::trim` (its closest equivalent).
///   * on match: `text_stripped[m.end():].strip()` (slice past the tag + matched `\s*`, then strip
///     again) and `m.group(1).lower()` (tier lowercased).
///   * no match: `(text_stripped, "chore")`.
pub fn strip_tier(text: &str) -> (String, String) {
    let stripped = text.trim();
    // Case-insensitive `[<tier>]` followed by zero+ whitespace, anchored at the start (re.match).
    let re = tier_re();
    if let Some(m) = re.find(stripped) {
        // re.match only matches at position 0; find() on an anchored pattern can only match there too.
        if m.start() == 0 {
            let tier = tier_re_capture(stripped); // group(1).lower()
            let rest = stripped[m.end()..].trim().to_string();
            return (rest, tier);
        }
    }
    (stripped.to_string(), "chore".to_string())
}

/// run_improver._recent_noop_streak (~1667-1686): count of TRAILING `"noop"` iterations in
/// `RUNTIME/history.jsonl` (the active-fabrication signal solomon.py's diagnose() keys on). 0 when
/// the loop is making real changes — the last terminal outcome wasn't a noop.
///
/// Bug-for-bug: read the file (OSError -> 0); iterate lines in REVERSE; skip blank lines; for each
/// non-blank line `json.loads(line)` and `.get("status")` — if `== "noop"` increment and continue,
/// any other status BREAKS, and a `JSONDecodeError` also BREAKS (a malformed trailing line ends the
/// streak). Note the asymmetry vs `_recent_history` elsewhere: a bad line here stops the count.
pub fn recent_noop_streak(ctx: &Ctx) -> i64 {
    let path = ctx.runtime.join("history.jsonl");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return 0, // except OSError -> 0
    };
    let mut streak: i64 = 0;
    // splitlines() then reversed(): split on line boundaries, drop the trailing empty from a final \n.
    for raw in content.lines().rev() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => {
                // .get("status") == "noop" — only a JSON string "noop" matches.
                if v.get("status").and_then(Value::as_str) == Some("noop") {
                    streak += 1;
                } else {
                    break;
                }
            }
            Err(_) => break, // except json.JSONDecodeError -> break
        }
    }
    streak
}

/// run_improver._needs_goal_skip (~1689-1700): True iff this iteration has NO real objective AND the
/// loop is already spinning on no-ops. The conjunction (all three) is required:
///   1. the north-star `GOAL.strip() == ""` (`ctx.goal` is already stored trimmed, but we re-trim to
///      match the source's `GOAL.strip()` exactly),
///   2. the chosen item `goal.lower()` contains the generic placeholder `"model-chosen improvement"`
///      OR an already-deferred marker `"(deferred"`, AND
///   3. `recent_noop_streak() > 0`.
/// A repo with a REAL backlog item OR one still shipping real changes with no GOAL still runs.
pub fn needs_goal_skip(ctx: &Ctx, goal: &str) -> bool {
    let g = goal.to_lowercase();
    ctx.goal.trim().is_empty()
        && (g.contains("model-chosen improvement") || g.contains("(deferred"))
        && recent_noop_streak(ctx) > 0
}

/// run_improver._unchecked_backlog_count (~1703-1710): count of ACTIONABLE backlog items — lines
/// whose stripped form starts `- [ ]` and that do NOT contain `"(deferred"`. OSError -> 0.
/// Note: `"(deferred" not in ln` is tested against the RAW (un-stripped) line, matching the source.
pub fn unchecked_backlog_count(ctx: &Ctx) -> i64 {
    let content = match std::fs::read_to_string(&ctx.backlog) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    content
        .lines()
        .filter(|ln| ln.trim().starts_with("- [ ]") && !ln.contains("(deferred"))
        .count() as i64
}

/// run_improver._top_backlog_item (~1713-1722): `(text, tier)` of the FIRST unchecked `- [ ]` item,
/// tier from its leading tag (default `"chore"`). When the file is unreadable (OSError) or has no
/// unchecked item, returns the placeholder `("model-chosen improvement", "chore")`.
///
/// Bug-for-bug: iterate lines top-down; first line whose STRIPPED form starts with `- [ ]` ->
/// `strip_tier(s[5:].strip())`. `s[5:]` slices the 5-byte `- [ ]` prefix off (the line was matched on
/// its stripped form, so byte 5 is always just past the prefix), then `.strip()` before tier parsing.
pub fn top_backlog_item(ctx: &Ctx) -> Option<(String, String)> {
    if let Ok(content) = std::fs::read_to_string(&ctx.backlog) {
        for line in content.lines() {
            let s = line.trim();
            if s.starts_with("- [ ]") {
                // s[5:].strip() — slice past "- [ ]" (5 bytes, all ASCII) then strip.
                let rest = s[5..].trim();
                return Some(strip_tier(rest));
            }
        }
    }
    // OSError pass OR loop fell through with no match -> the placeholder default.
    Some(("model-chosen improvement".to_string(), "chore".to_string()))
}

/// run_improver._mark_backlog_done (~1725-1743): after a successful ship, tick the backlog item we
/// just implemented (`- [ ]` -> `- [x]`) so a continuous loop advances to the NEXT item instead of
/// re-shipping the same one (every iteration bases off the integration branch, which doesn't yet have
/// the in-flight PRs).
///
/// Bug-for-bug:
///   * `if not goal: return` — empty `goal` is a no-op.
///   * read the file (OSError -> return, leaving it untouched).
///   * scan for the FIRST line whose STRIPPED form starts `- [ ]` AND whose tier-stripped text equals
///     `goal.strip()` (compare the goal trimmed; the item text is already stripped by `strip_tier`).
///   * rewrite that line with `ln.replace("- [ ]", "- [x]", 1)` (replaces the first `- [ ]` in the
///     RAW line, preserving indentation/trailing text), write the whole file back joined by `"\n"`
///     with a trailing `"\n"` (OSError on write -> swallowed), then RETURN (only the first match).
pub fn mark_backlog_done(ctx: &Ctx, goal: &str) {
    if goal.is_empty() {
        return;
    }
    let content = match std::fs::read_to_string(&ctx.backlog) {
        Ok(c) => c,
        Err(_) => return,
    };
    // Preserve the exact line set: splitlines() (no trailing empty for a final newline).
    let mut lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();
    let goal_stripped = goal.trim();
    for i in 0..lines.len() {
        let s = lines[i].trim();
        if s.starts_with("- [ ]") && strip_tier(s[5..].trim()).0 == goal_stripped {
            // ln.replace("- [ ]", "- [x]", 1) — first occurrence only.
            lines[i] = replace_first(&lines[i], "- [ ]", "- [x]");
            // "\n".join(lines) + "\n"
            let out = format!("{}\n", lines.join("\n"));
            let _ = std::fs::write(&ctx.backlog, out.as_bytes()); // OSError -> pass
            return;
        }
    }
}

// --------------------------------------------------------------------------- #
// free helpers
// --------------------------------------------------------------------------- #

/// The case-insensitive anchored tier-tag regex, compiled per call (off the hot path; matches the
/// source's per-call `re.match`). Pattern: `\[(chore|feature|refactor|architecture)\]\s*` with re.I.
fn tier_re() -> regex::Regex {
    regex::RegexBuilder::new(r"^\[(chore|feature|refactor|architecture)\]\s*")
        .case_insensitive(true)
        .build()
        .expect("static tier regex compiles")
}

/// Extract `m.group(1).lower()` — the matched tier word, lowercased — from a string already known to
/// match `tier_re()` at position 0.
fn tier_re_capture(s: &str) -> String {
    tier_re()
        .captures(s)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_lowercase())
        .unwrap_or_else(|| "chore".to_string())
}

/// Python `str.replace(old, new, 1)` — replace the FIRST occurrence of `old` with `new`, leaving the
/// rest of the string untouched. (No occurrence -> the string unchanged.)
fn replace_first(s: &str, old: &str, new: &str) -> String {
    match s.find(old) {
        Some(idx) => {
            let mut out = String::with_capacity(s.len() - old.len() + new.len());
            out.push_str(&s[..idx]);
            out.push_str(new);
            out.push_str(&s[idx + old.len()..]);
            out
        }
        None => s.to_string(),
    }
}

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    /// A Ctx pointed at a fresh temp dir for backlog/history IO. Reuses configure() then redirects
    /// `backlog` + `runtime` to a unique temp subdir so tests never touch the real files.
    fn test_ctx(tag: &str) -> Ctx {
        let mut c = Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None);
        let base = std::env::temp_dir().join(format!(
            "solomon_backlog_test_{}_{}_{}",
            std::process::id(),
            tag,
            // a cheap per-call salt so parallel tests don't collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&base).unwrap();
        c.runtime = base.clone();
        c.backlog = base.join("backlog.md");
        c
    }

    fn write_backlog(ctx: &Ctx, text: &str) {
        std::fs::write(&ctx.backlog, text).unwrap();
    }
    fn write_history(ctx: &Ctx, text: &str) {
        std::fs::write(ctx.runtime.join("history.jsonl"), text).unwrap();
    }
    fn read_backlog(ctx: &Ctx) -> String {
        std::fs::read_to_string(&ctx.backlog).unwrap()
    }

    // ---- strip_tier ----
    #[test]
    fn strip_tier_default_is_chore() {
        assert_eq!(
            strip_tier("add a button"),
            ("add a button".to_string(), "chore".to_string())
        );
    }

    #[test]
    fn strip_tier_parses_each_tier_case_insensitively() {
        assert_eq!(
            strip_tier("[feature] add OAuth"),
            ("add OAuth".to_string(), "feature".to_string())
        );
        assert_eq!(
            strip_tier("[REFACTOR]   tidy module"),
            ("tidy module".to_string(), "refactor".to_string())
        );
        assert_eq!(
            strip_tier("[Architecture] split service"),
            ("split service".to_string(), "architecture".to_string())
        );
        assert_eq!(
            strip_tier("[chore] bump dep"),
            ("bump dep".to_string(), "chore".to_string())
        );
    }

    #[test]
    fn strip_tier_strips_surrounding_whitespace_and_unknown_tag_stays() {
        // leading/trailing whitespace removed before + after tag parsing
        assert_eq!(
            strip_tier("   [feature]   do X   "),
            ("do X".to_string(), "feature".to_string())
        );
        // an unknown bracket tag is NOT a tier -> whole text kept, default chore
        assert_eq!(
            strip_tier("[bugfix] do Y"),
            ("[bugfix] do Y".to_string(), "chore".to_string())
        );
        // empty -> ("", "chore")
        assert_eq!(strip_tier(""), (String::new(), "chore".to_string()));
    }

    // ---- recent_noop_streak ----
    #[test]
    fn noop_streak_counts_trailing_noops() {
        let c = test_ctx("streak_trailing");
        write_history(
            &c,
            "{\"status\":\"shipped\"}\n{\"status\":\"noop\"}\n{\"status\":\"noop\"}\n",
        );
        assert_eq!(recent_noop_streak(&c), 2);
    }

    #[test]
    fn noop_streak_breaks_on_non_noop_and_skips_blanks() {
        let c = test_ctx("streak_break");
        // trailing blank lines are skipped; a non-noop above the noops stops the count.
        write_history(
            &c,
            "{\"status\":\"noop\"}\n{\"status\":\"reverted\"}\n{\"status\":\"noop\"}\n\n\n",
        );
        assert_eq!(recent_noop_streak(&c), 1);
    }

    #[test]
    fn noop_streak_break_on_malformed_trailing_line_and_missing_file() {
        let c = test_ctx("streak_bad");
        // a malformed trailing line ends the streak immediately (before reaching the real noops).
        write_history(&c, "{\"status\":\"noop\"}\nnot json\n");
        assert_eq!(recent_noop_streak(&c), 0);
        // missing file -> 0
        let c2 = test_ctx("streak_missing");
        std::fs::remove_file(c2.runtime.join("history.jsonl")).ok();
        assert_eq!(recent_noop_streak(&c2), 0);
    }

    // ---- unchecked_backlog_count ----
    #[test]
    fn unchecked_count_ignores_done_and_deferred() {
        let c = test_ctx("count");
        write_backlog(
            &c,
            "- [ ] real item one\n- [x] already done\n- [ ] deferred thing (deferred 2026-01-01)\n  - [ ] indented still counts\n",
        );
        // two `- [ ]` without "(deferred": "real item one" and the indented one.
        assert_eq!(unchecked_backlog_count(&c), 2);
        // missing file -> 0
        let c2 = test_ctx("count_missing");
        assert_eq!(unchecked_backlog_count(&c2), 0);
    }

    // ---- top_backlog_item ----
    #[test]
    fn top_item_returns_first_unchecked_with_tier() {
        let c = test_ctx("top");
        // Only lines starting `- [ ]` are candidates (Python _top_backlog_item ~1718). A tier-tagged
        // but non-`- [ ]` line (`- [feature] ...`) is NOT an unchecked item and is skipped, so the
        // first real `- [ ]` line wins with its tier-stripped text (no leading tag => default chore).
        write_backlog(
            &c,
            "- [x] done first\n- [feature] build the thing\n- [ ] later item\n",
        );
        assert_eq!(
            top_backlog_item(&c),
            Some(("later item".to_string(), "chore".to_string()))
        );
    }

    #[test]
    fn top_item_placeholder_when_none_or_missing() {
        let c = test_ctx("top_none");
        write_backlog(&c, "- [x] all done\nplain text\n");
        assert_eq!(
            top_backlog_item(&c),
            Some(("model-chosen improvement".to_string(), "chore".to_string()))
        );
        // missing file -> same placeholder
        let c2 = test_ctx("top_missing");
        assert_eq!(
            top_backlog_item(&c2),
            Some(("model-chosen improvement".to_string(), "chore".to_string()))
        );
    }

    // ---- mark_backlog_done ----
    #[test]
    fn mark_done_ticks_first_matching_item_only() {
        let c = test_ctx("mark");
        write_backlog(
            &c,
            "- [ ] [feature] build the thing\n- [ ] other item\n- [ ] build the thing\n",
        );
        // goal matches the tier-stripped text of line 1.
        mark_backlog_done(&c, "build the thing");
        let out = read_backlog(&c);
        // only the FIRST match flips; the trailing duplicate stays unchecked.
        assert_eq!(
            out,
            "- [x] [feature] build the thing\n- [ ] other item\n- [ ] build the thing\n"
        );
    }

    #[test]
    fn mark_done_empty_goal_and_no_match_are_noops() {
        let c = test_ctx("mark_noop");
        let original = "- [ ] something\n- [x] done\n";
        write_backlog(&c, original);
        mark_backlog_done(&c, ""); // empty goal -> early return, file untouched
        assert_eq!(read_backlog(&c), original);
        mark_backlog_done(&c, "no such item"); // no match -> file untouched
        assert_eq!(read_backlog(&c), original);
    }

    // ---- needs_goal_skip ----
    #[test]
    fn needs_goal_skip_requires_all_three_conditions() {
        let mut c = test_ctx("skip");
        // empty GOAL + placeholder goal + a noop streak -> skip.
        c.goal = String::new();
        write_history(&c, "{\"status\":\"noop\"}\n");
        assert!(needs_goal_skip(&c, "model-chosen improvement"));
        assert!(needs_goal_skip(&c, "thing (deferred 2026-01-01)"));
        // a REAL backlog item (not placeholder/deferred) -> does NOT skip even with a noop streak.
        assert!(!needs_goal_skip(&c, "add a real feature"));
    }

    #[test]
    fn needs_goal_skip_false_when_goal_set_or_no_noops() {
        let mut c = test_ctx("noskip");
        write_history(&c, "{\"status\":\"noop\"}\n");
        // a non-empty north-star GOAL keeps the loop running.
        c.goal = "ship the moon".to_string();
        assert!(!needs_goal_skip(&c, "model-chosen improvement"));
        // empty GOAL + placeholder but NO noop streak (no history) -> does NOT skip.
        let mut c2 = test_ctx("noskip2");
        c2.goal = String::new();
        assert!(!needs_goal_skip(&c2, "model-chosen improvement"));
    }
}
