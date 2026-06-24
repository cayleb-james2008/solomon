//! Native Rust port of run_improver.py's optional in-process pipeline phases — the
//! PLAN / REVIEW / REFLECT specialists that bracket the implement phase.
//!
//! Behavior is bug-for-bug with run_improver.py (`pipeline_phases` area of the port spec at
//! `src-tauri/run-improver-port-spec.json`). The Python module-level globals become fields on the
//! one [`Ctx`] threaded through every function; phase gating reads `ctx.review_enabled /
//! plan_enabled / reflect_enabled` (refreshed from repos.json `pipeline.*` each iteration by the
//! ctx/registry layer — not re-read here).
//!
//! Scope (this file owns exactly these three public entry points + their private task builders):
//!   * [`run_review_phase`] — adversarial JUDGE after the objective gate passes + commit, before
//!     ship. Returns `"approve" | "reject" | "skip"`; on `"reject"` it reverts the branch and the
//!     change is never shipped. Fail-OPEN: any unparseable verdict is `"skip"`.
//!   * [`run_plan_phase`]   — pre-implement read-only planner; returns advisory plan text (trimmed
//!     to 4000 chars) or `""` best-effort.
//!   * [`reflect`]          — post-iteration retrospective; distills ONE durable lesson from the
//!     LAST history.jsonl record and appends it (timestamped, deduped) to the repo's LESSONS.md.
//!
//! Phase-isolated pi calls go through [`crate::improver::pi::phase_run_pi`], which saves/restores
//! the loop's (implement) provider/model/reasoning around the call so e.g. review can run on a
//! different model. `ideate_phase` (the fourth pipeline phase) lives in the ideate/backlog module,
//! not here — `reflect`'s lesson-dedup helpers (`tokenize`/`is_novel`/`read_lessons`) are ported as
//! private copies here to keep this module self-contained (see module DEVIATIONS in the report).

use std::path::Path;

use regex::Regex;
use std::sync::OnceLock;

use crate::improver::ctx::{self, Ctx};
use crate::improver::{gitops, pi};

/// `(?i)REVIEW:\s*(approve|reject)\b([^\n]*)` — the review-verdict line. Compiled once.
fn review_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)REVIEW:\s*(approve|reject)\b([^\n]*)").unwrap())
}

/// `(?i)LESSON:\s*(.+)` — the reflect-lesson line. Compiled once.
fn lesson_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)LESSON:\s*(.+)").unwrap())
}

// ---- REVIEW phase ---------------------------------------------------------- #

/// run_improver._run_review_phase (~2103-2135): adversarial REVIEW/JUDGE after the runner's
/// objective gate passes + the change is committed. An independent critic inspects the committed
/// diff for reward-hacking / scope creep / regressions / a change that doesn't accomplish the goal.
/// Returns `"approve" | "reject" | "skip"`. On `"reject"` the branch is reverted (never shipped).
/// Fail-OPEN: any unparseable verdict is `"skip"` (the objective gate already passed, so a flaky
/// reviewer must not wedge the loop).
pub fn run_review_phase(ctx: &mut Ctx, branch: &str, goal: &str, summary: &str) -> String {
    // stat = (git("diff", f"{BASE_BRANCH}..{branch}", "--stat").stdout or "")[:2000]
    let diff_range = format!("{}..{}", ctx.base_branch, branch);
    // git(...) uses run_improver.git's default timeout=120 (the call site passes none).
    let raw = ctx.git(&["diff", &diff_range, "--stat"], 120).stdout;
    let stat: String = raw.chars().take(2000).collect();

    let goal_line = if goal.is_empty() { "(no explicit goal)" } else { goal };
    let task = format!(
        "Adversarially review the committed change on this rsi/* branch — you are the JUDGE.\n\n\
Iteration goal:\n{goal_line}\n\nImplementer's summary:\n{summary}\n\n\
Changed files (stat):\n{stat}\n\nInspect the full diff with `git diff {base}..HEAD` \
and read the changed files. Judge per review.md, then end with EXACTLY one line: \
'REVIEW: approve - <reason>' or 'REVIEW: reject - <reason>'.",
        base = ctx.base_branch
    );

    // _phase_run_pi("review", task, system_md=REVIEW_MD, timeout=600). In Python a spawn/timeout
    // failure raises and the `except Exception` branch logs "review/judge: ... — fail-open" and returns
    // "skip". phase_run_pi here returns a RunOut (a timeout maps to rc=124, not a panic), so the
    // timeout fail-open must be done EXPLICITLY: run_pi drains the WHOLE partial stream before the kill,
    // so a reviewer that streamed a complete "REVIEW: reject" and THEN hung on teardown would otherwise
    // parse as a real reject and REVERT gate-green, verified work. Honor rc==124 as "skip" BEFORE
    // parsing (the same timeout marker iteration.rs trusts); a flaky/slow reviewer must never discard
    // shipped work. The no-verdict fail-open below covers the rest.
    let review_md = ctx.review_md.clone();
    let p = pi::phase_run_pi(ctx, "review", &task, Some(review_md.as_path()), 600);
    if p.code == 124 {
        ctx.log("review/judge: pi timed out — fail-open, not blocking ship");
        return "skip".to_string();
    }
    let text = pi::final_text(&p.stdout);

    let re = review_re();
    let m = match re.captures(&text) {
        None => {
            ctx.log("review/judge: no parseable verdict — fail-open, not blocking ship");
            return "skip".to_string();
        }
        Some(c) => c,
    };
    let verdict = m.get(1).map(|x| x.as_str()).unwrap_or("").to_lowercase();
    // reason = m.group(2).lstrip(" -—:").strip()[:200]
    let reason = trim200(lstrip_chars(
        m.get(2).map(|x| x.as_str()).unwrap_or(""),
        &[' ', '-', '\u{2014}', ':'],
    ));

    if verdict == "reject" {
        ctx.log(&format!(
            "review/judge: REJECT — {reason} — reverting (change not shipped)"
        ));
        // _drop_branch(branch, "reverted", "Reverted — ..."): phase="reverted", status defaults
        // to "sleeping" in Python — passed explicitly here (Rust drop_branch has no default arg).
        gitops::drop_branch(
            ctx,
            branch,
            "reverted",
            &format!("Reverted — review/judge rejected: {reason}. {summary}"),
            "sleeping",
        );
        return "reject".to_string();
    }
    ctx.log(&format!("review/judge: APPROVE — {reason}"));
    "approve".to_string()
}

// ---- PLAN phase ------------------------------------------------------------ #

/// run_improver._run_plan_phase (~2138-2151): pre-implement PLAN phase. A read-only planner drafts
/// a short implementation plan for the chosen backlog item, returned as advisory text injected into
/// the implement task (the planner never writes files). Best-effort: `""` on any error/timeout
/// (planning never blocks the iteration). Output trimmed to 4000 chars.
pub fn run_plan_phase(ctx: &mut Ctx, goal: &str) -> String {
    let task = format!(
        "Draft a SHORT implementation plan for this ONE backlog item — do NOT write or edit any \
files, just plan.\n\nItem:\n{goal}\n\nInspect the repo read-only as needed, then output a \
concise, ordered plan: the files to touch, the approach, and how to verify. Keep it tight."
    );
    // _phase_run_pi("plan", task, system_md=PLAN_MD, timeout=400). The Python `except Exception ->
    // log "plan phase: error (...) — skipping (no plan injected)"; return ""` branch is unreachable
    // here because phase_run_pi returns proc::RunOut (no fallible raise); same observable result
    // (an empty/garbage stdout still yields a trimmed-but-possibly-empty plan string).
    let plan_md = ctx.plan_md.clone();
    let p = pi::phase_run_pi(ctx, "plan", &task, Some(plan_md.as_path()), 400);
    // return final_text(p.stdout or "").strip()[:4000]
    trim_chars(pi::final_text(&p.stdout).trim(), 4000)
}

// ---- REFLECT phase --------------------------------------------------------- #

/// run_improver._reflect_task (~3266-3282): the REFLECT phase's pi task. Distills ONE durable,
/// concrete lesson from the just-finished iteration record (status + summary + tests). Names the
/// existing lessons so the agent doesn't restate one (the Rust port still dedupes as a backstop).
/// `rec` is the last parseable history.jsonl record (a JSON object).
fn reflect_task(ctx: &Ctx, rec: &serde_json::Value) -> String {
    // status = rec.get("status") or "?"   (Python truthiness: None/""/false -> "?")
    let status = json_str_or(rec.get("status"), "?");
    // summary = (rec.get("summary") or "")[:1200]
    let summary_full = json_str_or(rec.get("summary"), "");
    let summary: String = summary_full.chars().take(1200).collect();
    // tests = rec.get("tests") or {}   then json.dumps(tests)
    let tests_val = match rec.get("tests") {
        Some(v) if !is_falsy(v) => v.clone(),
        _ => serde_json::json!({}),
    };
    let tests_json = serde_json::to_string(&tests_val).unwrap_or_else(|_| "{}".to_string());

    let existing = read_lessons(ctx);
    let existing = existing.trim();
    let existing_block = if !existing.is_empty() {
        // existing[-2000:]  — last 2000 chars (Python code points)
        let tail = tail_chars(existing, 2000);
        format!(
            "\n\nLessons already recorded (do NOT restate any of these — only add a NEW, \
non-duplicate lesson):\n{tail}"
        )
    } else {
        String::new()
    };

    format!(
        "The RSI loop just finished one iteration on this repository.\n\
Outcome: {status}\nTest counts: {tests_json}\n\
What the implementer reported:\n{summary}\n\
Read the relevant code/diff/history as needed, then distill ONE durable, CONCRETE lesson per \
reflect.md — what was attempted, the outcome, the ROOT CAUSE if it failed, and what to try \
or avoid next time. End with EXACTLY one line beginning 'LESSON: '.{existing_block}"
    )
}

/// run_improver.reflect (~3285-3339): REFLECT phase (pipeline.reflect). After EACH iteration —
/// shipped OR failed/deferred — distill a durable, concrete lesson and APPEND it (timestamped,
/// deduplicated) to the target repo's LESSONS.md. Reads the LAST parseable history.jsonl record.
/// Best-effort: a no-op when disabled or when there is no history yet; never wedges the loop.
pub fn reflect(ctx: &mut Ctx) {
    if !ctx.reflect_enabled {
        return;
    }
    // lines = (RUNTIME / "history.jsonl").read_text(...).splitlines()  / except OSError: return
    let history_path = ctx.runtime.join("history.jsonl");
    let content = match std::fs::read_to_string(&history_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    // Python str.splitlines(): split on \n/\r/\r\n, NO trailing empty element.
    let lines: Vec<&str> = splitlines(&content);

    // rec = the last non-empty, parseable record (search reversed).
    let mut rec: Option<serde_json::Value> = None;
    for line in lines.iter().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                rec = Some(v);
                break;
            }
            Err(_) => continue, // json.JSONDecodeError -> keep scanning
        }
    }
    let rec = match rec {
        Some(r) => r,
        None => return, // nothing to reflect on yet
    };

    // try: heartbeat(phase="reflect"); p = _phase_run_pi("reflect", _reflect_task(rec), ..., 400).
    // The Python `except Exception -> log "reflect phase: error (...) — no lesson recorded"; return`
    // branch is unreachable here (phase_run_pi returns proc::RunOut, never raises).
    ctx.heartbeat(serde_json::json!({"phase": "reflect"}));
    let task = reflect_task(ctx, &rec);
    let reflect_md = ctx.reflect_md.clone();
    let p = pi::phase_run_pi(ctx, "reflect", &task, Some(reflect_md.as_path()), 400);

    let text = pi::final_text(&p.stdout);
    // m = re.search(r"LESSON:\s*(.+)", text, re.I)
    // lesson = _redact(m.group(1).strip().splitlines()[0])[:600].strip() if m else ""
    let re = lesson_re();
    let lesson = match re.captures(&text) {
        Some(c) => {
            let g1 = c.get(1).map(|x| x.as_str()).unwrap_or("");
            // .strip() then .splitlines()[0]  (first line of the stripped capture; "" if none)
            let stripped = g1.trim();
            let first = splitlines(stripped).into_iter().next().unwrap_or("");
            let redacted = ctx.redact(first);
            trim_chars(&redacted, 600).trim().to_string()
        }
        None => String::new(),
    };
    if lesson.is_empty() {
        ctx.log("reflect phase: agent produced no parseable lesson — nothing appended");
        return;
    }

    // DEDUP against existing lessons (token-Jaccard).
    let existing = read_lessons(ctx);
    let prior: Vec<String> = existing
        .lines()
        .map(|ln| ln.trim().to_string())
        .filter(|ln| ln.starts_with("- "))
        .collect();
    if !is_novel(&lesson, &prior, 0.6) {
        ctx.log("reflect phase: lesson near-duplicates an existing one — not appended (deduped)");
        return;
    }

    let entry = format!("- {} — {}", ctx::now(), lesson);
    // try: LESSONS.parent.mkdir(...); write/append; log  / except OSError: log
    if let Some(parent) = ctx.lessons.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let write_res = if existing.is_empty() {
        std::fs::write(&ctx.lessons, format!("# Lessons\n\n{entry}\n"))
    } else {
        // open(LESSONS, "a"): prepend "\n" only when the file doesn't already end in "\n".
        let sep = if existing.ends_with('\n') { "" } else { "\n" };
        append(&ctx.lessons, &format!("{sep}{entry}\n"))
    };
    match write_res {
        Ok(()) => ctx.log(&format!(
            "reflect phase: appended lesson — {}",
            trunc_chars(&lesson, 90)
        )),
        Err(e) => ctx.log(&format!("reflect phase: could not write lesson ({e})")),
    }
}

// ---- private helpers (free functions in the Python source) ----------------- #

/// run_improver._read_lessons (~3091-3098): the accumulated LESSONS.md text — best-effort, `""`
/// when absent or unreadable.
fn read_lessons(ctx: &Ctx) -> String {
    if ctx.lessons.exists() {
        std::fs::read_to_string(&ctx.lessons).unwrap_or_default()
    } else {
        String::new()
    }
}

// run_improver._STOPWORDS (~3046-3048).
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "for", "to", "of", "in", "on", "at", "by", "with",
    "from", "into", "as", "is", "are", "be", "it", "this", "that", "these", "those", "add",
    "adds", "added", "use", "uses", "using", "make", "makes", "made", "into", "via", "per", "its",
    "it's", "not", "no", "than", "then", "so", "we", "i",
];

/// run_improver._tokenize (~3051-3054): lowercase `[a-z0-9]+` word tokens, dropping stopwords and
/// tokens of length <= 2 — the unit of similarity.
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    // r"[a-z0-9]+" over a lowercased string == maximal runs of ascii-alphanumerics; stdlib split gives
    // the same tokens with no regex compile (this runs per corpus item inside is_novel's O(N) loop).
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_string)
        .filter(|w| w.chars().count() > 2 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// run_improver._is_novel (~3066-3088): True if `idea` is NOT a near-duplicate of any string in
/// `corpus`. Empty token set -> always novel. Dual signals (either at/over its threshold -> not
/// novel): symmetric Jaccard (inter/union) >= `threshold`; containment (inter/min(|a|,|b|)) >=
/// `threshold + 0.15`.
fn is_novel(idea: &str, corpus: &[String], threshold: f64) -> bool {
    let toks = tokenize(idea);
    if toks.is_empty() {
        return true;
    }
    for other in corpus {
        let ot = tokenize(other);
        if ot.is_empty() {
            continue;
        }
        let inter = toks.intersection(&ot).count();
        if inter == 0 {
            continue;
        }
        let union = toks.union(&ot).count();
        if (inter as f64) / (union as f64) >= threshold {
            return false; // Jaccard (symmetric near-equal)
        }
        let min_len = toks.len().min(ot.len());
        if (inter as f64) / (min_len as f64) >= threshold + 0.15 {
            return false; // containment (subsumed by a longer item)
        }
    }
    true
}

// ---- small string-semantics shims (match Python str behavior on code points) #

/// Python `s.lstrip(chars)`: strip any leading char that is in `chars`.
fn lstrip_chars(s: &str, chars: &[char]) -> String {
    s.trim_start_matches(|c| chars.contains(&c)).to_string()
}

/// Python `s.strip()[:200]` over an already-lstrip'd string: `.strip()` then first 200 code points.
fn trim200(s: String) -> String {
    s.trim().chars().take(200).collect()
}

/// Python `s[:n]` — first `n` code points (NOT bytes).
fn trim_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Alias for `s[:n]` used in log lines (lesson[:90]).
fn trunc_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `s[-n:]` — last `n` code points.
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        s.to_string()
    } else {
        s.chars().skip(count - n).collect()
    }
}

/// Python `str.splitlines()`: split on universal newlines, with NO trailing empty element.
fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' || b == b'\r' {
            out.push(&s[start..i]);
            if b == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 1;
            }
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    if start < bytes.len() {
        out.push(&s[start..]);
    }
    out
}

/// JSON value -> Python `rec.get(key) or default` truthiness: a non-string or falsy value yields
/// the default; a non-empty string yields the string.
fn json_str_or(v: Option<&serde_json::Value>, default: &str) -> String {
    match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        _ => default.to_string(),
    }
}

/// Python truthiness of a JSON value (for `rec.get("tests") or {}`).
fn is_falsy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::Bool(b) => !*b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
        serde_json::Value::String(s) => s.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Object(o) => o.is_empty(),
    }
}

/// Append bytes to a file (Python `open(path, "a")`).
fn append(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(text.as_bytes())
}

// ---------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    // ---- review verdict parsing (regex + reason lstrip/trim) ----

    fn parse_verdict(text: &str) -> Option<(String, String)> {
        let re = review_re();
        let c = re.captures(text)?;
        let verdict = c.get(1).unwrap().as_str().to_lowercase();
        let reason = trim200(lstrip_chars(
            c.get(2).map(|x| x.as_str()).unwrap_or(""),
            &[' ', '-', '\u{2014}', ':'],
        ));
        Some((verdict, reason))
    }

    #[test]
    fn review_approve_basic() {
        let (v, r) = parse_verdict("blah\nREVIEW: approve - looks good").unwrap();
        assert_eq!(v, "approve");
        assert_eq!(r, "looks good");
    }

    #[test]
    fn review_reject_emdash_reason() {
        // em-dash + colon + space all lstrip'd off the reason.
        let (v, r) = parse_verdict("REVIEW: reject —: scope creep").unwrap();
        assert_eq!(v, "reject");
        assert_eq!(r, "scope creep");
    }

    #[test]
    fn review_case_insensitive_keyword() {
        let (v, _) = parse_verdict("review: APPROVE").unwrap();
        assert_eq!(v, "approve");
    }

    #[test]
    fn review_word_boundary_blocks_substring() {
        // "disapprove" must not match approve (the keyword "REVIEW:" is absent anyway, but the \b
        // also guards "approves"/"rejected" prefixes when REVIEW: precedes).
        assert!(parse_verdict("the reviewer disapproves of nothing").is_none());
    }

    #[test]
    fn review_reason_trimmed_to_200() {
        let long = "x".repeat(500);
        let (_, r) = parse_verdict(&format!("REVIEW: approve - {long}")).unwrap();
        assert_eq!(r.chars().count(), 200);
    }

    #[test]
    fn review_no_marker_is_none() {
        assert!(parse_verdict("I approve this change wholeheartedly").is_none());
    }

    // ---- lesson parsing ----

    fn parse_lesson(text: &str) -> String {
        let re = lesson_re();
        match re.captures(text) {
            Some(c) => {
                let g1 = c.get(1).unwrap().as_str();
                let stripped = g1.trim();
                let first = splitlines(stripped).into_iter().next().unwrap_or("");
                trim_chars(first.trim(), 600).trim().to_string()
            }
            None => String::new(),
        }
    }

    #[test]
    fn lesson_first_line_only() {
        let out = parse_lesson("preamble\nLESSON: prefer stdlib\nsecond line ignored");
        assert_eq!(out, "prefer stdlib");
    }

    #[test]
    fn lesson_requires_marker() {
        assert_eq!(parse_lesson("no marker here, just prose"), "");
    }

    #[test]
    fn lesson_case_insensitive() {
        assert_eq!(parse_lesson("lesson: be lazy"), "be lazy");
    }

    #[test]
    fn lesson_trimmed_to_600() {
        let long = "y".repeat(900);
        let out = parse_lesson(&format!("LESSON: {long}"));
        assert_eq!(out.chars().count(), 600);
    }

    // ---- is_novel / tokenize ----

    #[test]
    fn empty_idea_is_novel() {
        // tokenize("the a an") -> empty (all stopwords/short) -> always novel
        assert!(is_novel("the a an", &["whatever long lesson text".to_string()], 0.6));
    }

    #[test]
    fn identical_is_not_novel() {
        let corpus = vec!["prefer the standard library over custom code".to_string()];
        assert!(!is_novel("prefer standard library over custom code", &corpus, 0.6));
    }

    #[test]
    fn disjoint_is_novel() {
        let corpus = vec!["network retries need exponential backoff".to_string()];
        assert!(is_novel("documentation should mention licensing terms", &corpus, 0.6));
    }

    #[test]
    fn containment_subsumed_is_not_novel() {
        // short idea fully subsumed by a longer corpus item -> containment signal (>= 0.75)
        let corpus = vec![
            "always pin dependency versions because floating versions break the build later"
                .to_string(),
        ];
        assert!(!is_novel("pin dependency versions", &corpus, 0.6));
    }

    #[test]
    fn tokenize_drops_short_and_stopwords() {
        let t = tokenize("The CI gate is RED on at");
        // "the","is","on","at" stopwords; "ci" too short (len 2). keeps "gate","red".
        assert!(t.contains("gate"));
        assert!(t.contains("red"));
        assert!(!t.contains("the"));
        assert!(!t.contains("ci"));
    }

    // ---- splitlines / slicing semantics ----

    #[test]
    fn splitlines_no_trailing_empty() {
        assert_eq!(splitlines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(splitlines(""), Vec::<&str>::new());
    }

    #[test]
    fn tail_chars_basic() {
        assert_eq!(tail_chars("abcdef", 3), "def");
        assert_eq!(tail_chars("ab", 5), "ab");
    }

    #[test]
    fn lstrip_only_listed_chars() {
        assert_eq!(lstrip_chars(" -—: hello", &[' ', '-', '\u{2014}', ':']), "hello");
        assert_eq!(lstrip_chars("xhello", &[' ', '-']), "xhello");
    }

    // ---- reflect_task falsy/truthiness for status/tests ----

    #[test]
    fn json_str_or_falsy_uses_default() {
        assert_eq!(json_str_or(Some(&serde_json::json!("")), "?"), "?");
        assert_eq!(json_str_or(Some(&serde_json::json!("shipped")), "?"), "shipped");
        assert_eq!(json_str_or(None, "?"), "?");
        assert_eq!(json_str_or(Some(&serde_json::Value::Null), "?"), "?");
    }

    #[test]
    fn tests_falsy_becomes_empty_object() {
        assert!(is_falsy(&serde_json::json!({})));
        assert!(is_falsy(&serde_json::Value::Null));
        assert!(!is_falsy(&serde_json::json!({"passed": 3})));
    }
}
