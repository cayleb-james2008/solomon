//! Native Rust port of run_improver.py's **escalation_ladder** area — bug-for-bug.
//!
//! The escalation ladder turns three observed dead-ends (gamed-then-reverted forever; "made no
//! changes"; "deferred after repeated tries") into adaptive progress instead of an infinite loop:
//!   rung 0  feed the SPECIFIC failure reason back into the next task (`last_gate_feedback`),
//!   rung 1  at [`ctx::ESCALATE_TO_FALLBACK`] cumulative failures, switch to a stronger/different
//!           model for the next attempt ([`apply_fallback_model`]),
//!   rung 2  at `limit` failures, decompose the goal into sub-items (opt-in via
//!           [`ctx::DECOMPOSE_ENABLED`]) else defer it to the bottom of the backlog (prior behavior).
//!
//! Counts are cumulative ACROSS failure kinds (noop / deviation / revert) on the same goal, held in
//! the in-memory [`Ctx::fail_counts`] map + [`Ctx::escalated_goals`] set — never persisted to disk
//! (a runner restart replays the goal at rung 0; see the spec's port_risks). The corrective-note
//! strings and log formats are byte-for-byte identical to the source (the agent is tuned to those
//! exact phrases incl. the em-dash `—`).
//!
//! Also in this area: [`build_task`] (the per-iteration prompt, which CONSUMES + clears the one-time
//! gate/visual feedback), [`split_item_status`] (strip the ITEM-STATUS marker),
//! [`deviated_from_named_files`] / [`edit_mandated_files`] / [`norm_path`] (the edit-mandate deviation
//! guard), and the three persistent-bail self-stops
//! ([`note_dirty_base_bail`]/[`note_unpushed_base_bail`]/[`note_base_gate_red_bail`]).

use crate::improver::ctx::{self, Ctx};
use crate::improver::{backlog, pi};
use serde_json::json;

// --------------------------------------------------------------------------- #
// constants (run_improver.py module level, escalation-ladder area)
// --------------------------------------------------------------------------- #

/// run_improver._DIRTY_BASE_PERSISTENT_LIMIT — consecutive dirty-BASE preflight bails before self-stop.
const DIRTY_BASE_PERSISTENT_LIMIT: i64 = 3;
/// run_improver._UNPUSHED_BASE_PERSISTENT_LIMIT — consecutive un-pushed-base bails before self-stop.
const UNPUSHED_BASE_PERSISTENT_LIMIT: i64 = 3;
/// run_improver._BASE_GATE_RED_PERSISTENT_LIMIT — consecutive base-gate-RED bails before self-stop.
const BASE_GATE_RED_PERSISTENT_LIMIT: i64 = 3;

// --------------------------------------------------------------------------- #
// build_task (run_improver.build_task ~258-317)
// --------------------------------------------------------------------------- #

/// run_improver.build_task (~258-317): the per-iteration instruction for the Pi coder. Names the
/// chosen item, its ambition TIER (sizing the change), and the operator's north-star GOAL. CONSUMES
/// any one-time escalation feedback (`last_gate_feedback`) AND visual feedback (`last_visual_feedback`)
/// from the previous failed attempt, weaving each into the task and CLEARING it so the corrective note
/// is injected exactly once.
///
/// Mutates `last_gate_feedback`/`last_visual_feedback` -> `""`; reads `goal`, `gate_cmd`.
pub fn build_task(ctx: &mut Ctx, goal: &str, tier: &str) -> String {
    // north_star = (... if GOAL else "")
    let north_star = if !ctx.goal.is_empty() {
        format!(
            "NORTH-STAR GOAL (weigh above all): {}\nChoose the change with the most leverage toward \
that goal; if it needs a capability the project lacks, BUILD that capability as this one \
increment.\n\n",
            ctx.goal
        )
    } else {
        String::new()
    };

    // _custom_gate = bool((GATE_CMD or "").strip())
    let custom_gate = !ctx.gate_cmd.trim().is_empty();
    // _test_phrase = "a test for it" if _custom_gate else "a pytest test for it"
    let test_phrase = if custom_gate {
        "a test for it"
    } else {
        "a pytest test for it"
    };
    let gate_instr = if custom_gate {
        format!(
            "Then run the project's test gate (`{}`) yourself to confirm it is green, \
and add or adjust a test for your change in the repo's OWN test framework.",
            ctx.gate_cmd
        )
    } else {
        "Then run the test suite (`.venv/Scripts/python -m pytest`) yourself to confirm \
it is green."
            .to_string()
    };

    // sizing — chore vs tier
    let sizing = if tier == "chore" {
        format!(
            "Make the SMALLEST coherent change and add or update {test_phrase}; doing more \
than this one item is a regression."
        )
    } else {
        format!(
            "This is a {}-tier item — SIZE THE CHANGE TO THE OPPORTUNITY: a \
substantive, possibly multi-file change is expected and welcome; be ambitious and \
creative toward the goal, not minimal. It must still be ONE coherent, shippable \
improvement that passes the gate, with tests covering it. If it's genuinely too big \
for one iteration, implement the largest coherent first slice that's shippable now \
and note the rest in your summary.",
            tier.to_uppercase()
        )
    };

    // feedback_block — visual first (consumed), then gate (consumed); each cleared to "".
    let mut feedback_block = String::new();
    if !ctx.last_visual_feedback.is_empty() {
        feedback_block = format!(
            "\n\n{}\n\nAddress the most critical visual/functional issue above in this iteration if it falls \
within the current backlog item's scope. Otherwise, note it for a future item. The visual \
feedback is ONE-TIME — it does not repeat unless a new E2E sandbox review runs.\n",
            ctx.last_visual_feedback
        );
        ctx.last_visual_feedback = String::new(); // consumed — inject exactly once
    }
    if !ctx.last_gate_feedback.is_empty() {
        feedback_block.push_str(&format!(
            "\n\nTHE PREVIOUS ATTEMPT ON THIS ITEM FAILED — {}\nDo NOT repeat that approach; fix the underlying cause. NEVER make the gate pass by skipping, \
xfail-ing, deleting, or weakening tests — implement the real change so the existing tests \
stay green. This corrective note is ONE-TIME.\n",
            ctx.last_gate_feedback
        ));
        ctx.last_gate_feedback = String::new(); // consumed — inject exactly once
    }

    let placeholder_goal = goal.trim().eq_ignore_ascii_case("model-chosen improvement");
    let item_intro = if placeholder_goal {
        "No concrete backlog item is queued. Choose exactly ONE small, real bug, reliability gap, \
missing test, or cleanup that advances the north-star goal. Before editing, name the concrete \
target in your own notes, then implement only that target. If you cannot find a safe target \
after a short scan, end with ITEM-STATUS: deviated and do not invent a change."
            .to_string()
    } else {
        format!("Implement exactly ONE improvement in this repository: \"{goal}\".")
    };
    let status_instr = if placeholder_goal {
        "End with a 2-4 sentence summary of what you changed, then a FINAL line that is exactly \
`ITEM-STATUS: done` if you implemented a concrete change, or `ITEM-STATUS: deviated` if you \
found no safe change."
            .to_string()
    } else {
        "End with a 2-4 sentence summary of what you changed, then a FINAL line that is exactly \
`ITEM-STATUS: done` if you implemented (or it was already fully done) the named item above, or \
`ITEM-STATUS: deviated` if you instead changed something else."
            .to_string()
    };

    format!(
        "{north_star}{item_intro} {sizing} {gate_instr} \
Do NOT run git or gh — the runner commits and opens the pull request. If that item is already done or \
unclear, instead fix one clear small bug or cleanup you find. {status_instr}{feedback_block}"
    )
}

// --------------------------------------------------------------------------- #
// split_item_status (run_improver._split_item_status ~320-333)
// --------------------------------------------------------------------------- #

/// run_improver._split_item_status (~320-333): pull the trailing
/// `ITEM-STATUS: done|deviated|skipped` marker off the agent summary. Returns
/// `(clean_summary, deviated)` where `deviated` is true when the marker's value is NOT "done"
/// (i.e. "deviated"/"skipped"); used to tick the backlog item ONLY when the agent actually
/// implemented it.
///
/// DEVIATION FROM HINT SIGNATURE: the entry-point hint lists
/// `split_item_status(summary)->(Option<String>,Option<String>)`, but the SOURCE returns
/// `(clean_summary: str, deviated: bool)` — the SOURCE is authority, so this returns
/// `(String, bool)`.
pub fn split_item_status(summary: &str) -> (String, bool) {
    let mut deviated = false;
    let mut kept: Vec<&str> = Vec::new();
    // re.match(r"\s*ITEM-STATUS:\s*(done|deviated|skipped)\b", ln, re.I)
    // re.match anchors at the START of the line; \b after the keyword.
    for ln in summary.lines() {
        if let Some(val) = match_item_status(ln) {
            // m.group(1).lower() != "done"
            deviated = val.to_lowercase() != "done";
            continue; // strip the marker line from the PR/commit body
        }
        kept.push(ln);
    }
    // ("\n".join(kept).strip() or summary)
    let joined = kept.join("\n");
    let trimmed = joined.trim();
    let clean = if trimmed.is_empty() {
        summary.to_string()
    } else {
        trimmed.to_string()
    };
    (clean, deviated)
}

/// Implements `re.match(r"\s*ITEM-STATUS:\s*(done|deviated|skipped)\b", ln, re.I)`: anchored at the
/// start of the line, case-insensitive, returns the captured status word (group 1) on a match. `\s`
/// in Python's `re` (no re.UNICODE-only handling here) matches `[ \t\n\r\f\v]`; for a single line
/// `\n`/`\r` won't appear, so the ASCII whitespace set is faithful.
fn match_item_status(ln: &str) -> Option<String> {
    let bytes: Vec<char> = ln.chars().collect();
    let mut i = 0;
    // \s*  — leading ASCII whitespace
    while i < bytes.len() && is_py_space(bytes[i]) {
        i += 1;
    }
    // literal "ITEM-STATUS:" (re.I, but this segment is non-alpha except the word, which is upper —
    // match case-insensitively to honor re.I).
    let rest: String = bytes[i..].iter().collect();
    let lower = rest.to_lowercase();
    let prefix = "item-status:";
    if !lower.starts_with(prefix) {
        return None;
    }
    let after_colon = &rest[prefix.len()..];
    // \s*  after the colon
    let after_trim = after_colon.trim_start_matches(is_py_space);
    let after_lower = after_trim.to_lowercase();
    for kw in ["done", "deviated", "skipped"] {
        if after_lower.starts_with(kw) {
            // \b — next char must be a non-word char (or end of string)
            let next = after_trim[kw.len()..].chars().next();
            let boundary = match next {
                None => true,
                Some(c) => !(c.is_alphanumeric() || c == '_'),
            };
            if boundary {
                // return the ORIGINAL-cased captured word (group 1)
                return Some(after_trim[..kw.len()].to_string());
            }
        }
    }
    None
}

/// Python `re` `\s` for the `\s*` runs in `_split_item_status` — ASCII whitespace set.
fn is_py_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000B}' | '\u{000C}')
}

// --------------------------------------------------------------------------- #
// edit-mandate deviation guard (run_improver ~336-373)
// --------------------------------------------------------------------------- #

/// run_improver._norm_path (~349-350): `re.sub(r"^[./\\]+", "", p.strip().replace("\\","/")).lower()`
/// — strip leading `.`/`/`/`\` chars, backslashes->forward, lowercase.
pub fn norm_path(p: &str) -> String {
    let s = p.trim().replace('\\', "/");
    let trimmed = s.trim_start_matches(['.', '/', '\\']);
    trimmed.to_lowercase()
}

/// run_improver._edit_mandated_files (~353-357): normalized relative paths the item explicitly
/// mandates EDITING — a backticked code/config file directly following an edit verb. Empty when the
/// item gives no explicit edit mandate. Dedupes (Python returns a `set`).
pub fn edit_mandated_files(text: &str) -> std::collections::HashSet<String> {
    edit_mandate_re()
        .captures_iter(text)
        .map(|c| norm_path(&c[1]))
        .collect()
}

/// run_improver._EDIT_MANDATE_RE (~342-346): an edit verb, then within 0-15 non-`.`/non-newline/
/// non-backtick chars, a backticked path ending in a known code/config extension. Bounded
/// quantifiers (no catastrophic backtracking), case-insensitive.
fn edit_mandate_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"\b(?:edit|edits|editing|change|changes|changed|rewrite|rewrites|rewriting|modif\w+|replace|replaces|recreate|create|creates)\b[^.\n`]{0,15}?`([^`]{1,80}?\.(?:py|ts|tsx|js|jsx|json|toml|md|ya?ml|cfg|ini|txt|html|css|svg|rs|go|sh))`",
        )
        .case_insensitive(true)
        .build()
        .expect("edit-mandate regex is valid")
    })
}

/// run_improver._deviated_from_named_files (~360-373): True when the item explicitly mandates editing
/// one or more files but the committed diff touched none of them (matched by path SUFFIX so a named
/// `scripts/scheduler.py` isn't satisfied by a decoy `docs/scheduler.py`). Conservative: fires ONLY on
/// an explicit edit mandate — when no file is mandated, returns False (the agent's self-reported
/// ITEM-STATUS stands).
pub fn deviated_from_named_files(_ctx: &Ctx, goal: &str, changed_files: &str) -> bool {
    let named = edit_mandated_files(goal);
    if named.is_empty() {
        return false;
    }
    let touched: Vec<String> = changed_files
        .lines()
        .filter(|ln| !ln.trim().is_empty())
        .map(norm_path)
        .collect();
    for n in &named {
        // any(t == n or t.endswith("/" + n) for t in touched)
        let suffix = format!("/{n}");
        if touched.iter().any(|t| t == n || t.ends_with(&suffix)) {
            return false; // touched at least one mandated file -> not a deviation
        }
    }
    true // mandated files exist but the diff touched none of them
}

// --------------------------------------------------------------------------- #
// persistent-bail self-stops (run_improver ~400-488)
// --------------------------------------------------------------------------- #

/// run_improver._note_dirty_base_bail (~400-426): track consecutive dirty-BASE preflight bails.
/// Returns True (writing STOP + an error heartbeat) when [`DIRTY_BASE_PERSISTENT_LIMIT`] is reached —
/// the caller must NOT spin again. A dirty NON-base branch (rsi/* — cleared by the forced preflight)
/// does NOT count; a clean iteration RESETS the counter.
pub fn note_dirty_base_bail(
    ctx: &mut Ctx,
    dirty: bool,
    cur_branch: &str,
    base_branch: &str,
) -> bool {
    // only a dirty BASE branch counts
    if !(dirty && cur_branch == base_branch) {
        ctx.dirty_base_bail_count = 0;
        return false;
    }
    ctx.dirty_base_bail_count += 1;
    if ctx.dirty_base_bail_count < DIRTY_BASE_PERSISTENT_LIMIT {
        return false;
    }
    // persistent dirty-base: write STOP + an error heartbeat.
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = std::fs::write(&ctx.stop_path, "dirty_base_persistent\n");
    let last_summary = format!(
        "Base branch '{base_branch}' has been dirty for {} consecutive preflight bails — the loop self-stops \
so it doesn't spin forever. Commit, stash, or reset the base tree; then \
clear the stop sentinel (Solomon → Start) to resume.",
        ctx.dirty_base_bail_count
    );
    ctx.heartbeat(json!({
        "status": "error",
        "phase": "preflight",
        "reason": "dirty_base_persistent",
        "last_summary": last_summary,
    }));
    ctx.dirty_base_bail_count = 0; // reset after the stop so a later restart re-counts cleanly
    true
}

/// run_improver._note_unpushed_base_bail (~437-459): track consecutive un-pushed-base bails (the
/// fast-forward sync of the base to origin keeps failing). Returns True (writing STOP + an error
/// heartbeat) at [`UNPUSHED_BASE_PERSISTENT_LIMIT`]. `bail=false` resets the counter.
pub fn note_unpushed_base_bail(ctx: &mut Ctx, bail: bool, n_ahead: i64, shas: &str) -> bool {
    if !bail {
        ctx.unpushed_base_bail_count = 0;
        return false;
    }
    ctx.unpushed_base_bail_count += 1;
    if ctx.unpushed_base_bail_count < UNPUSHED_BASE_PERSISTENT_LIMIT {
        return false;
    }
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = std::fs::write(&ctx.stop_path, "unpushed_base_persistent\n");
    // shas[:240] — slice by Python code points.
    let shas_trunc: String = shas.chars().take(240).collect();
    let last_summary = format!(
        "Base has {n_ahead} un-pushed commit(s) and the fast-forward push to origin \
keeps failing — the loop self-stops so it doesn't spin forever. Reconcile \
the base with origin, then Start to resume. Commits: {shas_trunc}"
    );
    ctx.heartbeat(json!({
        "status": "error",
        "phase": "preflight",
        "reason": "unpushed_base_persistent",
        "last_summary": last_summary,
    }));
    ctx.unpushed_base_bail_count = 0;
    true
}

/// run_improver._note_base_gate_red_bail (~466-488): track consecutive base-gate-RED preflight bails
/// (a non-transient gate failure: pytest missing, a broken venv, a wrong GATE_CMD, a committed test
/// syntax error). Returns True (writing STOP + an error heartbeat) at
/// [`BASE_GATE_RED_PERSISTENT_LIMIT`]. `red=false` resets the counter.
pub fn note_base_gate_red_bail(ctx: &mut Ctx, red: bool, summary: &str) -> bool {
    if !red {
        ctx.base_gate_red_bail_count = 0;
        return false;
    }
    ctx.base_gate_red_bail_count += 1;
    if ctx.base_gate_red_bail_count < BASE_GATE_RED_PERSISTENT_LIMIT {
        return false;
    }
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let _ = std::fs::write(&ctx.stop_path, "base_gate_red_persistent\n");
    // (summary or "Base gate has been RED ...") + " — the loop self-stops ..."
    let head = if summary.is_empty() {
        "Base gate has been RED for several consecutive preflight bails"
    } else {
        summary
    };
    let last_summary = format!(
        "{head} — the loop self-stops so it doesn't spin forever. Fix the gate command or \
the base, then Start to resume."
    );
    ctx.heartbeat(json!({
        "status": "error",
        "phase": "preflight",
        "reason": "base_gate_red_persistent",
        "last_summary": last_summary,
    }));
    ctx.base_gate_red_bail_count = 0;
    true
}

// --------------------------------------------------------------------------- #
// the escalation ladder proper (run_improver ~1749-1878)
// --------------------------------------------------------------------------- #

/// run_improver._decompose_item (~1749-1785): rung 2 — ask pi (decompose.md) to split a
/// repeatedly-failing backlog item into 2-4 smaller, independently-shippable sub-items, then REPLACE
/// the original `- [ ]` item with them. Returns True iff 2-4 sub-items were generated, the item was
/// found, and the backlog was written. Opt-in ([`ctx::DECOMPOSE_ENABLED`]); ANY error/timeout -> False
/// (caller falls back to defer).
pub fn decompose_item(ctx: &mut Ctx, goal: &str, reason: &str) -> bool {
    // DECOMPOSE_MD = HERE / "decompose.md"
    let decompose_md = ctx.here.join("decompose.md");
    if goal.is_empty() || !decompose_md.exists() {
        return false;
    }
    // task = '... could not be implemented in one iteration' + (' (last failure: {reason})' if reason) + '...'
    let last_failure = if reason.is_empty() {
        String::new()
    } else {
        format!(" (last failure: {reason})")
    };
    let task = format!(
        "The backlog item \"{goal}\" could not be implemented in one iteration{last_failure}. \
Per decompose.md, split it into 2-4 SMALLER, independently-shippable sub-items that \
together accomplish it. Output ONLY the sub-item lines, one per line, each starting \"- \"."
    );
    // p = run_pi(task, system_md=DECOMPOSE_MD, timeout=600); Exception/timeout -> log + False.
    // run_pi never raises in the Rust port (it returns a RunOut whose code reflects spawn/timeout),
    // so the Python `except Exception` decompose-failed branch maps to: a timed-out/failed RunOut
    // still yields raw text we parse — which is exactly the source flow when pi exits cleanly. A hard
    // spawn failure surfaces as empty stdout -> raw "" -> subs empty -> returns False (defer), the
    // same observable outcome as the logged-exception branch.
    let p = pi::run_pi(ctx, &task, 600, Some(&decompose_md));
    // raw = final_text(p.stdout) or ""
    let raw = pi::final_text(&p.stdout);
    // subs = [s.strip()[2:].strip() for s in raw.splitlines()
    //         if s.strip().startswith("- ") and len(s.strip()) > 4]
    let mut subs: Vec<String> = Vec::new();
    for line in raw.lines() {
        let s = line.trim();
        // len(s.strip()) > 4 counts CODE POINTS (Python len on str).
        if s.starts_with("- ") && s.chars().count() > 4 {
            // s.strip()[2:].strip() — drop the leading "- " (2 chars) then strip again.
            let after: String = s.chars().skip(2).collect();
            subs.push(after.trim().to_string());
        }
    }
    // subs = [s for s in subs if s][:4]  — drop empties, cap at 4.
    let subs: Vec<String> = subs.into_iter().filter(|s| !s.is_empty()).take(4).collect();
    if subs.len() < 2 {
        return false;
    }
    // lines = BACKLOG.read_text().splitlines()  (OSError -> False)
    let content = match std::fs::read_to_string(&ctx.backlog) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut lines: Vec<String> = py_splitlines(&content);
    for i in 0..lines.len() {
        let s = lines[i].trim();
        // s.startswith("- [ ]") and _strip_tier(s[5:].strip())[0] == goal.strip()
        if s.starts_with("- [ ]") {
            let after: String = s.chars().skip(5).collect();
            let (stripped, _tier) = backlog::strip_tier(after.trim());
            if stripped == goal.trim() {
                // lines[i:i+1] = [f"- [ ] {sub}" for sub in subs]
                let repl: Vec<String> = subs.iter().map(|sub| format!("- [ ] {sub}")).collect();
                lines.splice(i..i + 1, repl);
                // BACKLOG.write_text("\n".join(lines) + "\n")  (OSError -> False)
                let out = format!("{}\n", lines.join("\n"));
                return std::fs::write(&ctx.backlog, out).is_ok();
            }
        }
    }
    false
}

/// run_improver._register_failure (~1788-1812): the escalation ladder for a failed iteration
/// (kind: noop | deviation | revert). Records a corrective note (`last_gate_feedback`, truncated to
/// 600 chars), increments [`Ctx::fail_counts`] for the goal, escalates to the fallback rung at
/// [`ctx::ESCALATE_TO_FALLBACK`], and at `limit` failures decomposes (if enabled + succeeds) else
/// defers — then resets both counters regardless. Counts are cumulative ACROSS kinds.
pub fn register_failure(ctx: &mut Ctx, goal: &str, kind: &str, reason: &str, limit: i64) {
    // if not goal or BEAUTIFY or SOLOMON or goal.lower() == "model-chosen improvement": return
    if goal.is_empty()
        || ctx.beautify
        || ctx.solomon
        || goal.to_lowercase() == "model-chosen improvement"
    {
        return;
    }
    // if reason: LAST_GATE_FEEDBACK = reason[:600]  (600 CODE POINTS)
    if !reason.is_empty() {
        ctx.last_gate_feedback = reason.chars().take(600).collect();
    }
    // n = _fail_counts.get(goal, 0) + 1 ; _fail_counts[goal] = n
    let n = ctx.fail_counts.get(goal).copied().unwrap_or(0) + 1;
    ctx.fail_counts.insert(goal.to_string(), n);
    // rung 1
    if n >= ctx::ESCALATE_TO_FALLBACK {
        ctx.escalated_goals.insert(goal.to_string());
        ctx.log(&format!(
            "escalation: '{}' failed {n}x ({kind}) — next attempt uses the fallback model",
            goal_head(goal)
        ));
    }
    // final rung
    if n >= limit {
        if ctx::DECOMPOSE_ENABLED && decompose_item(ctx, goal, reason) {
            ctx.log(&format!(
                "escalation: '{}' failed {n}x — decomposed into sub-items",
                goal_head(goal)
            ));
        } else if defer_backlog_item(ctx, goal) {
            ctx.log(&format!(
                "escalation: '{}' failed {limit}x ({kind}) — deferred to bottom of backlog",
                goal_head(goal)
            ));
        }
        ctx.fail_counts.insert(goal.to_string(), 0);
        ctx.escalated_goals.remove(goal);
    }
}

/// run_improver._clear_failure_state (~1815-1818): a successful ship resets the item's escalation
/// state (so a future re-add starts clean). `.pop(goal, None)` + `.discard(goal)` — both no-op-safe.
/// Does NOT log.
pub fn clear_failure_state(ctx: &mut Ctx, goal: &str) {
    ctx.fail_counts.remove(goal);
    ctx.escalated_goals.remove(goal);
}

/// run_improver._apply_fallback_model (~1821-1830): escalation rung 1 — if `goal` has hit the
/// fallback rung, override `pi_model` with the provider's fallback model for THIS attempt (and update
/// the heartbeat model), so one weak model can't dead-end an implementable item. Per-iteration only:
/// the next iteration's registry refresh resets `pi_model`.
pub fn apply_fallback_model(ctx: &mut Ctx, goal: &str) {
    if ctx.escalated_goals.contains(goal) {
        // fb = _FALLBACK_MODEL.get(PROVIDER_NAME)
        if let Some(fb) = ctx::fallback_model(&ctx.provider_name) {
            // if fb and fb != PI_MODEL
            if !fb.is_empty() && fb != ctx.pi_model {
                ctx.log(&format!(
                    "escalation: retrying '{}' with fallback model {fb} (was {})",
                    goal_head(goal),
                    ctx.pi_model
                ));
                ctx.pi_model = fb.to_string();
                hb_set_model(ctx, fb);
            }
        }
    }
}

/// run_improver._note_noop (~1833-1838): no-change iteration — escalate (feedback -> fallback model ->
/// decompose/defer) with the templated noop corrective note.
pub fn note_noop(ctx: &mut Ctx, goal: &str, limit: i64) {
    register_failure(
        ctx,
        goal,
        "noop",
        "the previous attempt produced NO changes to a clean tree — pick a different, \
concrete approach and actually edit files to implement THIS item",
        limit,
    );
}

/// The agent claimed it edited files but the tree stayed clean. Feed a sharper corrective note back
/// than the generic no-change case so the next attempt stops narrating possible edits and either
/// writes a real diff or honestly deviates.
pub fn note_narrated_noop(ctx: &mut Ctx, goal: &str, limit: i64) {
    register_failure(
        ctx,
        goal,
        "noop",
        "the previous attempt narrated file edits but left the tree clean -- do not describe \
planned edits; actually modify files for THIS item, or explicitly report ITEM-STATUS: deviated \
if there is no safe change",
        limit,
    );
}

/// Implement timeouts usually mean the item needs a smaller shippable slice. Count them in the same
/// escalation ladder as noops/reverts so the runner adapts instead of retrying forever.
pub fn note_timeout(ctx: &mut Ctx, goal: &str, limit: i64) {
    register_failure(
        ctx,
        goal,
        "timeout",
        "the previous attempt exceeded the implement timeout -- scope this item down to the \
largest coherent slice that can be edited, tested, and shipped in one cycle",
        limit,
    );
}

/// The agent shipped a real change to something OTHER than the named item. Escalate with the
/// templated deviation corrective note.
pub fn note_deviation(ctx: &mut Ctx, goal: &str, limit: i64) {
    register_failure(
        ctx,
        goal,
        "deviation",
        "the previous attempt changed something OTHER than this item — implement THIS \
specific backlog item, not an unrelated change",
        limit,
    );
}

/// run_improver._note_revert (~1849-1854): a green-but-REVERTED iteration (gamed gate / failed gate /
/// eval drop / review reject) — escalate exactly like noop/deviation, feeding the specific revert
/// `reason` back to the next attempt.
pub fn note_revert(ctx: &mut Ctx, goal: &str, reason: &str, limit: i64) {
    register_failure(ctx, goal, "revert", reason, limit);
}

/// run_improver._defer_backlog_item (~1857-1878): move a stuck `- [ ]` item to the BOTTOM of the
/// backlog (with a `  (deferred: ...)` note, appended only once) so `top_backlog_item` returns the
/// next item. Returns True iff it moved one. First match only; OSError on read/write -> False.
pub fn defer_backlog_item(ctx: &mut Ctx, goal: &str) -> bool {
    if goal.is_empty() {
        return false;
    }
    let content = match std::fs::read_to_string(&ctx.backlog) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut lines: Vec<String> = py_splitlines(&content);
    for i in 0..lines.len() {
        let s = lines[i].trim();
        if s.starts_with("- [ ]") {
            let after: String = s.chars().skip(5).collect();
            let (stripped, _tier) = backlog::strip_tier(after.trim());
            if stripped == goal.trim() {
                // item = lines.pop(i).rstrip()
                let mut item = lines.remove(i).trim_end().to_string();
                // if "(deferred" not in item: item += "  (deferred: ...)"
                if !item.contains("(deferred") {
                    item.push_str("  (deferred: agent could not implement after repeated tries)");
                }
                lines.push(item);
                let out = format!("{}\n", lines.join("\n"));
                return std::fs::write(&ctx.backlog, out).is_ok();
            }
        }
    }
    false
}

// --------------------------------------------------------------------------- #
// small internal helpers
// --------------------------------------------------------------------------- #

/// Python `goal[:50]` (the log-truncated goal head) by CODE POINTS.
fn goal_head(goal: &str) -> String {
    goal.chars().take(50).collect()
}

/// `_hb["model"] = v` — a bare heartbeat-dict field assignment (NOT a heartbeat() call), matching the
/// source. Ctx::hb_set is private to ctx.rs, so set the key directly here.
fn hb_set_model(ctx: &mut Ctx, model: &str) {
    if let serde_json::Value::Object(hb) = &mut ctx.hb {
        hb.insert("model".to_string(), json!(model));
    }
}

/// Python `str.splitlines()` for backlog rewriting: splits on `\n`/`\r`/`\r\n` (the common cases here)
/// and does NOT keep a trailing empty element when the text ends in a newline — matching how
/// `_decompose_item`/`_defer_backlog_item` rebuild the file with `"\n".join(lines) + "\n"`.
fn py_splitlines(s: &str) -> Vec<String> {
    // str.splitlines() also splits on \v \f \x1c-\x1e \x85    , but backlog.md only uses
    // \n (\r\n on Windows). Splitting on the universal-newline set used by file reads is faithful.
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\n' => {
                out.push(std::mem::take(&mut cur));
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// --------------------------------------------------------------------------- #
// tests — exact-string vectors for the pure logic (parsers / reason builders /
// gate predicates). The IO-bound paths (decompose/defer reading backlog.md,
// the bail self-stops writing STOP+heartbeat) are covered by the loop's
// integration tests; here we pin the byte-exact strings + branch logic.
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        let mut c = Ctx::configure("C:/nonexistent/repo", "testrepo", "ollama-cloud", None);
        c.runtime = std::env::temp_dir().join(format!("solomon_esc_test_{}", std::process::id()));
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        c
    }

    // ---- split_item_status ----
    #[test]
    fn split_item_status_strips_marker_and_flags_deviation() {
        let (clean, dev) = split_item_status("did the thing\nITEM-STATUS: done");
        assert_eq!(clean, "did the thing");
        assert!(!dev);

        let (clean2, dev2) = split_item_status("changed elsewhere\nITEM-STATUS: deviated");
        assert_eq!(clean2, "changed elsewhere");
        assert!(dev2);

        // skipped -> deviated true
        let (_c, dev3) = split_item_status("x\nITEM-STATUS: skipped");
        assert!(dev3);

        // case-insensitive + leading whitespace
        let (_c, dev4) = split_item_status("y\n   item-status:  DONE");
        assert!(!dev4);
    }

    #[test]
    fn split_item_status_empty_clean_falls_back_to_summary() {
        // only the marker line -> kept is empty -> ("\n".join([]).strip() or summary) == summary
        let summary = "ITEM-STATUS: done";
        let (clean, dev) = split_item_status(summary);
        assert_eq!(clean, summary);
        assert!(!dev);
    }

    #[test]
    fn split_item_status_no_marker_keeps_all() {
        let (clean, dev) = split_item_status("line one\nline two");
        assert_eq!(clean, "line one\nline two");
        assert!(!dev);
    }

    #[test]
    fn split_item_status_word_boundary() {
        // "ITEM-STATUS: doneish" — \b after "done" fails (next char is a word char) so it is NOT a
        // marker; the line is kept verbatim and deviated stays false.
        let (clean, dev) = split_item_status("ITEM-STATUS: doneish");
        assert_eq!(clean, "ITEM-STATUS: doneish");
        assert!(!dev);
    }

    // ---- norm_path ----
    #[test]
    fn norm_path_strips_and_lowercases() {
        assert_eq!(
            norm_path("  ./Scripts/Scheduler.PY  "),
            "scripts/scheduler.py"
        );
        assert_eq!(norm_path("..\\a\\B.rs"), "a/b.rs");
        assert_eq!(norm_path("/x/y.json"), "x/y.json");
        assert_eq!(norm_path(""), "");
    }

    // ---- edit_mandated_files / deviated_from_named_files ----
    #[test]
    fn edit_mandate_extracts_only_after_edit_verb() {
        // "edit `scripts/scheduler.py`" mandates that file.
        let m = edit_mandated_files("Please edit `scripts/scheduler.py` to fix the bug.");
        assert!(m.contains("scripts/scheduler.py"));
        // a file named only as context ("see `README.md`") is NOT a mandate.
        let m2 = edit_mandated_files("see `README.md` for details");
        assert!(m2.is_empty());
    }

    #[test]
    fn deviated_true_when_mandate_untouched() {
        let c = ctx();
        let goal = "edit `scripts/scheduler.py` to add a flag";
        // diff touched something else -> deviation
        assert!(deviated_from_named_files(
            &c,
            goal,
            "docs/other.md\nsrc/main.rs"
        ));
        // suffix match: touching scripts/scheduler.py -> NOT a deviation
        assert!(!deviated_from_named_files(&c, goal, "scripts/scheduler.py"));
        // decoy suffix: docs/scheduler.py must NOT satisfy scripts/scheduler.py
        assert!(deviated_from_named_files(&c, goal, "docs/scheduler.py"));
    }

    #[test]
    fn deviated_false_when_no_mandate() {
        let c = ctx();
        // no explicit edit-verb-then-backtick mandate -> never a deviation
        assert!(!deviated_from_named_files(
            &c,
            "improve the dashboard layout",
            "anything.py"
        ));
    }

    // ---- build_task: feedback consumption + exact strings ----
    #[test]
    fn build_task_consumes_gate_feedback_once() {
        let mut c = ctx();
        c.last_gate_feedback = "the gate was GAMED".to_string();
        let t = build_task(&mut c, "add a widget", "chore");
        assert!(t.contains("THE PREVIOUS ATTEMPT ON THIS ITEM FAILED — the gate was GAMED"));
        assert!(t.contains("This corrective note is ONE-TIME."));
        // consumed
        assert_eq!(c.last_gate_feedback, "");
        // a second build has no feedback block
        let t2 = build_task(&mut c, "add a widget", "chore");
        assert!(!t2.contains("THE PREVIOUS ATTEMPT"));
    }

    #[test]
    fn build_task_chore_vs_tier_and_pytest_gate() {
        let mut c = ctx();
        // default (no gate_cmd) -> pytest phrasing
        let chore = build_task(&mut c, "fix bug", "chore");
        assert!(chore.contains("a pytest test for it"));
        assert!(chore.contains(".venv/Scripts/python -m pytest"));
        assert!(chore.contains("doing more than this one item is a regression."));
        assert!(
            chore.contains("Implement exactly ONE improvement in this repository: \"fix bug\".")
        );
        assert!(chore.contains("ITEM-STATUS: done"));
        // a non-chore tier -> uppercased tier + ambition phrasing
        let feat = build_task(&mut c, "big feature", "feature");
        assert!(feat.contains("This is a FEATURE-tier item — SIZE THE CHANGE TO THE OPPORTUNITY"));
    }

    #[test]
    fn build_task_custom_gate_and_north_star() {
        let mut c = ctx();
        c.gate_cmd = "npm test".to_string();
        c.goal = "ship revenue".to_string();
        let t = build_task(&mut c, "x", "chore");
        assert!(t.starts_with("NORTH-STAR GOAL (weigh above all): ship revenue"));
        assert!(t.contains("Then run the project's test gate (`npm test`) yourself"));
        assert!(t.contains("a test for it")); // not "a pytest test for it"
        assert!(!t.contains("a pytest test for it"));
    }

    #[test]
    fn build_task_placeholder_goal_requires_concrete_target() {
        let mut c = ctx();
        c.goal = "make the app more reliable".to_string();
        let t = build_task(&mut c, "model-chosen improvement", "chore");
        assert!(t.contains("No concrete backlog item is queued."));
        assert!(t.contains("Choose exactly ONE small, real bug, reliability gap"));
        assert!(t.contains("found no safe change"));
        assert!(!t.contains(
            "Implement exactly ONE improvement in this repository: \"model-chosen improvement\"."
        ));
    }

    #[test]
    fn build_task_visual_then_gate_order() {
        let mut c = ctx();
        c.last_visual_feedback = "VISUAL: button overlaps".to_string();
        c.last_gate_feedback = "GATE: tests failed".to_string();
        let t = build_task(&mut c, "g", "chore");
        let vi = t.find("VISUAL: button overlaps").unwrap();
        let gi = t.find("GATE: tests failed").unwrap();
        assert!(vi < gi, "visual feedback precedes gate feedback");
        assert_eq!(c.last_visual_feedback, "");
        assert_eq!(c.last_gate_feedback, "");
    }

    // ---- register_failure ladder ----
    #[test]
    fn register_failure_skips_guarded_goals() {
        let mut c = ctx();
        register_failure(&mut c, "", "noop", "r", 3);
        assert!(c.fail_counts.is_empty());
        register_failure(&mut c, "model-chosen improvement", "noop", "r", 3);
        assert!(c.fail_counts.is_empty());
        c.beautify = true;
        register_failure(&mut c, "real goal", "noop", "r", 3);
        assert!(c.fail_counts.is_empty());
    }

    #[test]
    fn register_failure_sets_feedback_and_escalates_at_threshold() {
        let mut c = ctx();
        // first failure: rung 0 sets feedback, count=1, not yet escalated (ESCALATE_TO_FALLBACK=2)
        register_failure(&mut c, "g", "noop", "boom", 3);
        assert_eq!(c.last_gate_feedback, "boom");
        assert_eq!(c.fail_counts.get("g"), Some(&1));
        assert!(!c.escalated_goals.contains("g"));
        // second failure: count=2 >= ESCALATE_TO_FALLBACK -> escalated
        register_failure(&mut c, "g", "revert", "again", 3);
        assert_eq!(c.fail_counts.get("g"), Some(&2));
        assert!(c.escalated_goals.contains("g"));
    }

    #[test]
    fn register_failure_truncates_feedback_to_600() {
        let mut c = ctx();
        let long = "x".repeat(800);
        register_failure(&mut c, "g", "revert", &long, 3);
        assert_eq!(c.last_gate_feedback.chars().count(), 600);
    }

    #[test]
    fn register_failure_no_reason_keeps_prior_feedback() {
        let mut c = ctx();
        c.last_gate_feedback = "prior".to_string();
        // empty reason -> LAST_GATE_FEEDBACK untouched (the `if reason:` guard)
        register_failure(&mut c, "g", "noop", "", 3);
        assert_eq!(c.last_gate_feedback, "prior");
    }

    #[test]
    fn register_failure_final_rung_resets_counters() {
        let mut c = ctx();
        // No backlog file at the configured path -> defer returns False, but counters still reset.
        register_failure(&mut c, "g", "noop", "r", 3);
        register_failure(&mut c, "g", "noop", "r", 3);
        register_failure(&mut c, "g", "noop", "r", 3); // n==3==limit -> final rung
        assert_eq!(c.fail_counts.get("g"), Some(&0));
        assert!(!c.escalated_goals.contains("g"));
    }

    // ---- clear_failure_state ----
    #[test]
    fn clear_failure_state_removes_both() {
        let mut c = ctx();
        c.fail_counts.insert("g".to_string(), 2);
        c.escalated_goals.insert("g".to_string());
        clear_failure_state(&mut c, "g");
        assert!(c.fail_counts.get("g").is_none());
        assert!(!c.escalated_goals.contains("g"));
        // safe when absent
        clear_failure_state(&mut c, "missing");
    }

    // ---- apply_fallback_model ----
    #[test]
    fn apply_fallback_model_switches_only_when_escalated() {
        let mut c = ctx(); // ollama-cloud -> fallback kimi-k2.7-code, model glm-5.2
                           // not escalated -> no change
        apply_fallback_model(&mut c, "g");
        assert_eq!(c.pi_model, "glm-5.2");
        // escalated -> switch
        c.escalated_goals.insert("g".to_string());
        apply_fallback_model(&mut c, "g");
        assert_eq!(c.pi_model, "kimi-k2.7-code");
        assert_eq!(c.hb["model"], json!("kimi-k2.7-code"));
    }

    #[test]
    fn apply_fallback_model_noop_when_same_model() {
        let mut c = ctx();
        c.pi_model = "kimi-k2.7-code".to_string(); // already the fallback
        c.escalated_goals.insert("g".to_string());
        apply_fallback_model(&mut c, "g");
        assert_eq!(c.pi_model, "kimi-k2.7-code"); // unchanged, no double-apply
    }

    // ---- note_noop / note_deviation reason strings ----
    #[test]
    fn note_noop_uses_templated_reason() {
        let mut c = ctx();
        note_noop(&mut c, "g", 3);
        assert_eq!(
            c.last_gate_feedback,
            "the previous attempt produced NO changes to a clean tree — pick a different, \
concrete approach and actually edit files to implement THIS item"
        );
    }

    #[test]
    fn note_deviation_uses_templated_reason() {
        let mut c = ctx();
        note_deviation(&mut c, "g", 3);
        assert_eq!(
            c.last_gate_feedback,
            "the previous attempt changed something OTHER than this item — implement THIS \
specific backlog item, not an unrelated change"
        );
    }

    #[test]
    fn note_narrated_noop_uses_specific_feedback_and_counts_failure() {
        let mut c = ctx();
        note_narrated_noop(&mut c, "g", 3);
        assert_eq!(c.fail_counts.get("g"), Some(&1));
        assert_eq!(
            c.last_gate_feedback,
            "the previous attempt narrated file edits but left the tree clean -- do not describe \
planned edits; actually modify files for THIS item, or explicitly report ITEM-STATUS: deviated \
if there is no safe change"
        );
    }

    #[test]
    fn note_timeout_uses_scope_down_reason_and_counts_failure() {
        let mut c = ctx();
        note_timeout(&mut c, "g", 3);
        assert_eq!(c.fail_counts.get("g"), Some(&1));
        assert_eq!(
            c.last_gate_feedback,
            "the previous attempt exceeded the implement timeout -- scope this item down to the \
largest coherent slice that can be edited, tested, and shipped in one cycle"
        );
    }

    #[test]
    fn note_revert_passes_reason_through() {
        let mut c = ctx();
        note_revert(&mut c, "g", "the gate was GAMED and reverted", 3);
        assert_eq!(c.last_gate_feedback, "the gate was GAMED and reverted");
    }

    // ---- bail counters: reset semantics (no IO branch hit below the limit) ----
    #[test]
    fn note_dirty_base_bail_counts_only_dirty_base() {
        let mut c = ctx();
        // clean -> reset, returns false
        assert!(!note_dirty_base_bail(&mut c, false, "main", "main"));
        assert_eq!(c.dirty_base_bail_count, 0);
        // dirty but on a non-base branch -> does not count
        assert!(!note_dirty_base_bail(&mut c, true, "rsi/iter-x", "main"));
        assert_eq!(c.dirty_base_bail_count, 0);
        // dirty base -> counts; below limit (3) returns false
        assert!(!note_dirty_base_bail(&mut c, true, "main", "main"));
        assert_eq!(c.dirty_base_bail_count, 1);
        assert!(!note_dirty_base_bail(&mut c, true, "main", "main"));
        assert_eq!(c.dirty_base_bail_count, 2);
    }

    #[test]
    fn note_unpushed_base_bail_resets_on_false() {
        let mut c = ctx();
        assert!(!note_unpushed_base_bail(&mut c, true, 1, "abc"));
        assert_eq!(c.unpushed_base_bail_count, 1);
        assert!(!note_unpushed_base_bail(&mut c, false, 0, ""));
        assert_eq!(c.unpushed_base_bail_count, 0);
    }

    #[test]
    fn note_base_gate_red_bail_resets_on_green() {
        let mut c = ctx();
        assert!(!note_base_gate_red_bail(&mut c, true, "pytest missing"));
        assert_eq!(c.base_gate_red_bail_count, 1);
        assert!(!note_base_gate_red_bail(&mut c, false, ""));
        assert_eq!(c.base_gate_red_bail_count, 0);
    }

    // ---- py_splitlines ----
    #[test]
    fn py_splitlines_matches_python() {
        assert_eq!(py_splitlines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines(""), Vec::<String>::new());
    }
}
