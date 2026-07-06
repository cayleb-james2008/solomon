//! Native Rust port of the `ship_flow` area of improver/run_improver.py — shipping the gate-green
//! committed branch to origin, opening/managing PRs, polling CI checks, the auto-merge / wait-for-CI
//! logic, and the terminal ship-state determination.
//!
//! Bug-for-bug with run_improver.py. The Python module-level globals (SHIP, BASE_BRANCH, BEAUTIFY,
//! STOP, heartbeat/log) are threaded via `&mut Ctx` / `&Ctx`. Bridge-style dict returns are
//! `serde_json::Value` objects whose keys are byte-identical to the Python dicts the dashboard reads.
//! State strings (including the em-dash in `"open (CI red — not merged)"`) are reproduced verbatim.

use crate::control::proc;
use crate::improver::ctx::Ctx;
use crate::improver::gates;
use crate::improver::gitops;
use crate::improver::tiers;
use serde_json::{json, Value};
use std::time::Duration;

// run_improver._wait_for_ci_then_merge tuning constants (~2042-2043).
/// CI_WAIT_CEILING_S — poll CI up to this many seconds, then hand off to native auto-merge.
const CI_WAIT_CEILING_S: u64 = 1200;
/// CI_POLL_DELAY_S — seconds between CI polls in _wait_for_ci_then_merge.
const CI_POLL_DELAY_S: u64 = 20;

// --------------------------------------------------------------------------- #
// small helpers
// --------------------------------------------------------------------------- #

/// run_improver `STOP.exists()` — the operator halt switch is the presence of the stop file.
fn stop_exists(c: &Ctx) -> bool {
    c.stop_path.exists()
}

/// (pr.get("state") or "").lower(): the state field coalesced to "" then lowercased.
fn state_lower(pr: &Value) -> String {
    pr.get("state")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase()
}

/// Python `pr.get("number")` truthiness (used by _ship_succeeded / _pr_checks etc.): a positive
/// integer number is truthy; 0, null, or absent is falsy.
fn number_truthy(pr: &Value) -> bool {
    match pr.get("number") {
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or_else(|| {
            // float number: nonzero is truthy
            n.as_f64().map(|f| f != 0.0).unwrap_or(false)
        }),
        _ => false,
    }
}

/// `dict(pr)` shallow copy with a replaced "state" — Python `{**pr, "state": s}`.
fn with_state(pr: &Value, s: &str) -> Value {
    let mut out = pr.clone();
    if let Value::Object(m) = &mut out {
        m.insert("state".to_string(), json!(s));
    }
    out
}

// --------------------------------------------------------------------------- #
// _pr_title / _gh_ready
// --------------------------------------------------------------------------- #

/// run_improver._pr_title (~1881-1888): concise PR/commit title from the backlog goal. If the goal
/// is non-empty and (case-insensitively) NOT the generic "model-chosen improvement" placeholder,
/// return goal[:72]. Otherwise the first non-blank line of summary[:72], defaulting to "improvement".
/// Truncation is by Python chars (code points), exact at 72, no ellipsis.
pub fn pr_title(_c: &Ctx, goal: &str, summary: &str) -> String {
    let g = goal.trim();
    if !g.is_empty() && g.to_lowercase() != "model-chosen improvement" {
        return truncate_chars(g, 72);
    }
    let first = summary
        .lines()
        .map(str::trim)
        .find(|ln| !ln.is_empty())
        .unwrap_or("improvement");
    truncate_chars(first, 72)
}

/// run_improver._gh_ready (~1891-1892): `gh auth status` (30s timeout) returns 0.
pub fn gh_ready(c: &Ctx) -> bool {
    c.gh(&["auth", "status"], 30).code == 0
}

/// Python `s[:n]` slices by code points, not bytes.
fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- #
// _pr_checks / _await_pr_checks
// --------------------------------------------------------------------------- #

/// The three DISTINGUISHABLE outcomes of probing a PR's CI, so a merge decision can tell a repo with
/// NO CI configured (safe to merge) apart from a repo whose CI status could not be read (gh failed /
/// unparseable — must NOT be treated as merge-eligible). `pr_checks` collapses this to Option for the
/// display callers; the merge gate uses the richer form.
#[derive(Debug, Clone, PartialEq)]
pub enum ChecksProbe {
    /// gh succeeded and the rollup reduced to a state.
    State(String),
    /// gh succeeded and the statusCheckRollup is CONFIRMED empty/absent — no CI configured.
    NoChecks,
    /// gh failed, was unparseable, or the PR number was falsy — the CI status is UNKNOWN.
    Unavailable,
}

/// run_improver._pr_checks (~1895-1918): reduce a PR's statusCheckRollup to
/// `Some("success"|"pending"|"failure")` or `None` (no checks / gh failure / unparseable / empty
/// rollup). `number` is the PR number; a falsy number returns None. Thin wrapper over `pr_checks_probe`
/// that collapses NoChecks/Unavailable to None (unchanged behavior for the display callers).
pub fn pr_checks(c: &Ctx, number: Option<i64>) -> Option<String> {
    match pr_checks_probe(c, number) {
        ChecksProbe::State(s) => Some(s),
        ChecksProbe::NoChecks | ChecksProbe::Unavailable => None,
    }
}

/// Like `pr_checks` but distinguishes 'no CI configured' (`NoChecks`) from 'gh call failed / status
/// unknown' (`Unavailable`) — a gh outage during the poll window must NOT be folded into the
/// safe-to-merge 'no CI' branch (that would merge an un-CI'd PR).
pub fn pr_checks_probe(c: &Ctx, number: Option<i64>) -> ChecksProbe {
    // `if not number:` — None or 0 is falsy. A falsy number is not a confirmed 'no CI'; unknown.
    let num = match number {
        Some(n) if n != 0 => n,
        _ => return ChecksProbe::Unavailable,
    };
    let p = c.gh(&["pr", "view", &num.to_string(), "--json", "statusCheckRollup"], 120);
    if p.code != 0 {
        return ChecksProbe::Unavailable; // gh failed (nonzero exit, incl. 124 timeout) -> unknown
    }
    // rollup = (json.loads(p.stdout or "{}") or {}).get("statusCheckRollup") or []
    let raw = if p.stdout.is_empty() { "{}" } else { &p.stdout };
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return ChecksProbe::Unavailable, // JSONDecodeError -> unknown (NOT 'no CI')
    };
    let rollup = parsed
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rollup.is_empty() {
        return ChecksProbe::NoChecks; // gh succeeded + rollup CONFIRMED empty -> genuinely no CI
    }
    let mut bad = false;
    let mut pend = false;
    for chk in &rollup {
        let st = chk.get("state").and_then(Value::as_str).unwrap_or("").to_uppercase();
        let status = chk.get("status").and_then(Value::as_str).unwrap_or("").to_uppercase();
        let concl = chk.get("conclusion").and_then(Value::as_str).unwrap_or("").to_uppercase();
        if (!status.is_empty() && status != "COMPLETED") || st == "PENDING" {
            pend = true;
        }
        if matches!(st.as_str(), "FAILURE" | "ERROR")
            || matches!(
                concl.as_str(),
                "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            )
        {
            bad = true;
        }
    }
    ChecksProbe::State(if bad {
        "failure".to_string()
    } else if pend {
        "pending".to_string()
    } else {
        "success".to_string()
    })
}

/// run_improver._await_pr_checks (~1921-1936): poll a just-opened PR's checks up to `attempts`
/// times (`delay` seconds between polls), returning the first non-None state. Breaks on a STOP file
/// or after the last attempt; returns the last (possibly None) value. Only used on ship=auto-merge.
pub fn await_pr_checks(c: &Ctx, number: Option<i64>, attempts: i64, delay: f64) -> Option<String> {
    let mut last: Option<String> = None;
    // for i in range(max(1, attempts)):
    let n = attempts.max(1);
    for i in 0..n {
        last = pr_checks(c, number);
        if last.is_some() {
            return last;
        }
        if stop_exists(c) || i >= n - 1 {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(delay));
    }
    last
}

/// Probe variant of `await_pr_checks`: poll up to `attempts` times, returning the first CONCLUSIVE
/// probe (a `State`, or a confirmed `NoChecks`). Only `Unavailable` (gh failed / unknown) is retried;
/// if every attempt is `Unavailable`, returns `Unavailable` — the caller must NOT merge on that.
fn await_pr_checks_probe(c: &Ctx, number: Option<i64>, attempts: i64, delay: f64) -> ChecksProbe {
    let mut last = ChecksProbe::Unavailable;
    let n = attempts.max(1);
    for i in 0..n {
        last = pr_checks_probe(c, number);
        if last != ChecksProbe::Unavailable {
            return last; // a conclusive State or a confirmed NoChecks — stop polling
        }
        if stop_exists(c) || i >= n - 1 {
            break;
        }
        std::thread::sleep(Duration::from_secs_f64(delay));
    }
    last
}

// --------------------------------------------------------------------------- #
// _existing_open_pr / _open_pr
// --------------------------------------------------------------------------- #

/// run_improver._existing_open_pr (~1939-1953): (number, url) of the OPEN PR whose head is `branch`,
/// or (None, None). Adopts an over-eager agent's self-opened PR. `gh pr list --head <branch>
/// --state open --json number,url`; first list element's number+url, else (None, None).
pub fn existing_open_pr(c: &Ctx, branch: &str) -> (Option<i64>, Option<String>) {
    let p = c.gh(
        &["pr", "list", "--head", branch, "--state", "open", "--json", "number,url"],
        120,
    );
    if p.code != 0 {
        return (None, None);
    }
    let raw = if p.stdout.is_empty() { "[]" } else { &p.stdout };
    let arr: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return (None, None), // (ValueError, TypeError) -> (None, None)
    };
    if let Some(list) = arr.as_array() {
        if let Some(Value::Object(first)) = list.first() {
            let num = first.get("number").and_then(Value::as_i64);
            let url = first.get("url").and_then(Value::as_str).map(str::to_string);
            return (num, url);
        }
    }
    (None, None)
}

/// run_improver._open_pr (~1956-1995): create the PR (or adopt an already-existing one). Body/title
/// differ in BEAUTIFY mode. On `gh pr create` failure: adopt an "already exists" PR (state "open"),
/// else best-effort delete this run's orphaned remote branch and return state "push-only". On
/// success, parse the PR number out of the last stdout line's `/pull/<n>`.
///
/// `tests` is the gate result dict (with int "passed"/"failed"); pass `None`-equivalent
/// `serde_json::Value::Null` to omit the gate line (only the BEAUTIFY path tolerates a missing tests
/// dict — the RSI path always passes a real one, matching the Python `tests: dict | None`).
pub fn open_pr(c: &mut Ctx, branch: &str, title: &str, summary: &str, tests: &Value) -> Value {
    let (body, pr_title_str) = if c.beautify {
        let body = format!(
            "Repo beautification (docs/presentation only — no code changes).\n\n{summary}\n\n\
             _Opened by the Solomon beautify pass on branch `{branch}` — review and merge or close._"
        );
        (body, format!("docs: {title}"))
    } else {
        // gate = "**Gate:** {passed} passed / {failed} failed on branch `{branch}`.\n\n" if tests else ""
        let gate = if tests.is_null() {
            String::new()
        } else {
            let passed = tests.get("passed").cloned().unwrap_or(Value::Null);
            let failed = tests.get("failed").cloned().unwrap_or(Value::Null);
            format!(
                "**Gate:** {} passed / {} failed on branch `{branch}`.\n\n",
                pyval(&passed),
                pyval(&failed)
            )
        };
        let body = format!(
            "Autonomous improvement (RSI loop).\n\n{summary}\n\n\
             {gate}_Opened by the Solomon RSI loop — review and merge or close._"
        );
        (body, format!("rsi: {title}"))
    };

    let base = c.base_branch.clone();
    let p = c.gh(
        &[
            "pr", "create", "--base", &base, "--head", branch, "--title", &pr_title_str, "--body",
            &body,
        ],
        120,
    );
    if p.code != 0 {
        let stderr = p.stderr.trim().to_string();
        // "already exists" -> adopt the agent-opened PR rather than falling to push-only.
        if stderr.to_lowercase().contains("already exists") {
            let (num, url) = existing_open_pr(c, branch);
            if let Some(n) = num {
                c.log(&format!(
                    "adopted existing PR #{n} for {branch} (gh pr create: already exists)"
                ));
                return json!({
                    "number": n,
                    "url": url,
                    "branch": branch,
                    "state": "open",
                });
            }
        }
        c.log(&format!("gh pr create failed: {}", slice_chars(&stderr, 200)));
        // Best-effort delete of exactly this run's orphaned remote branch.
        let d = c.git(&["push", "origin", "--delete", branch], 120);
        if d.code == 0 {
            c.log(&format!(
                "leak/hygiene: deleted orphaned remote branch {branch} after failed pr-create"
            ));
        }
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": "push-only",
        });
    }
    // url = last line of STRIPPED stdout if stdout.strip() else None. Strip the WHOLE stdout FIRST
    // (Python `(p.stdout or "").strip().splitlines()[-1]`): a trailing blank/whitespace line would
    // otherwise make `.lines().last()` return "" and null a successfully-created PR's url + number.
    let url: Option<String> = {
        let trimmed = p.stdout.trim();
        if trimmed.is_empty() {
            None
        } else {
            trimmed.lines().last().map(str::to_string)
        }
    };
    // num from url's trailing /pull/<n> segment (int() of the last "/"-split element)
    let mut num: Option<i64> = None;
    if let Some(u) = &url {
        if u.contains("/pull/") {
            if let Some(last_seg) = u.rsplit('/').next() {
                if let Ok(n) = last_seg.parse::<i64>() {
                    num = Some(n);
                }
            }
        }
    }
    json!({
        "number": num,
        "url": url,
        "branch": branch,
        "state": "open",
    })
}

/// Render a JSON value the way Python's f-string `{x}` would for the gate line: ints/floats bare,
/// `None` as "None". (The RSI path passes ints; this keeps a stray None byte-identical.)
fn pyval(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Python `s[:n]` by code points.
fn slice_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// --------------------------------------------------------------------------- #
// post-merge ship-gate
// --------------------------------------------------------------------------- #

/// The ship-state string returned when a post-merge gate catches a RED merged main and reverts
/// the merge. Factored out as a constant for unit testing the state-classification predicates
/// (`ship_succeeded` / `ship_outcome`) without a live merge + gate.
///
/// Contains "reverted" so `ship_succeeded` returns false (not shipped) and `ship_outcome` returns
/// "blocked" — the merge was undone, so the backlog item does NOT advance.
pub const POST_MERGE_REVERT_STATE: &str = "reverted (main gate RED after merge)";

/// Post-merge ship-gate: after a squash-merge is confirmed, fetch origin/main, reset the local base
/// to the merged HEAD, and run the repo's gate against it. This catches the class of bug where two
/// independently-green PRs conflict semantically — each branch gated green in isolation, but the
/// merged main goes RED (verified 2026-07-01: solomon PRs #35+#36, both green in isolation, merged
/// ~1s apart, main went gate-RED and tripped `base_gate_red_persistent` for ~25min until the
/// supervisor fixed it).
///
/// On a GREEN post-merge gate: returns true (the merge is safe to keep).
/// On a RED post-merge gate: reverts the just-merged squash commit on the base, pushes the revert,
/// logs loudly, surfaces `main gate RED after merging #<n>` in the heartbeat, and returns false.
/// The caller MUST NOT report "merged" — the base has been healed.
fn post_merge_gate(c: &mut Ctx, num: i64) -> bool {
    let base = c.base_branch.clone();
    // Fetch origin so the local base reflects the just-merged commit.
    c.git(&["fetch", "origin", "--quiet"], 120);
    // Move to the base branch and reset to the merged HEAD.
    c.git(&["checkout", &base], 120);
    c.git(&["reset", "--hard", &format!("origin/{base}")], 120);

    // Run the gate against the merged result.
    let (green, tests, _tail) = gates::run_gate(c);
    if green {
        c.log(&format!(
            "post-merge gate GREEN on origin/{base} after merging #{num} — base is safe"
        ));
        return true;
    }

    // RED main after merge — revert the just-merged commit to heal the base.
    let failed = tests.get("failed").and_then(Value::as_i64).unwrap_or(0);
    c.log(&format!(
        "MAIN GATE RED after merging #{num} ({failed} failed) — reverting the merge to heal the base"
    ));
    c.heartbeat(json!({
        "status": "error",
        "phase": "ship",
        "reason": "main_gate_red_after_merge",
        "last_summary": format!(
            "main gate RED after merging #{num} ({failed} failed) — the merged result broke the base \
even though each PR was green in isolation. Reverting the merge commit on {base} to heal the base."
        ),
    }));
    // Revert the squash-merge commit (HEAD on the base) and push.
    let rev = c.git(&["revert", "--no-edit", "HEAD"], 120);
    if rev.code == 0 {
        c.git(&["push", "origin", &base], 120);
        c.log(&format!("reverted merge of #{num} on {base} and pushed — base healed"));
    } else {
        // revert failed — hard-reset as a last resort to heal the base.
        c.log(&format!(
            "git revert failed on merge of #{num} ({}), hard-resetting {base} to HEAD~1 and force-pushing",
            slice_chars(rev.stderr.trim(), 160)
        ));
        c.git(&["reset", "--hard", "HEAD~1"], 120);
        c.git(&["push", "origin", &base, "--force-with-lease"], 120);
        c.log(&format!(
            "hard-reset {base} to before merge of #{num} and force-pushed — base healed"
        ));
    }
    false
}

// --------------------------------------------------------------------------- #
// _try_squash_merge / _auto_merge / _wait_for_ci_then_merge
// --------------------------------------------------------------------------- #

/// Parse the stdout of `gh pr view <num> --json mergedAt`: true only when `mergedAt` is a
/// non-empty string (an ISO timestamp). null / missing / unparseable -> false (not merged).
/// Factored out of [`confirm_merged`] for unit testing without a live `gh`.
fn parse_merged_at(stdout: &str) -> bool {
    let raw = if stdout.is_empty() { "{}" } else { stdout };
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    match parsed.get("mergedAt") {
        Some(Value::String(s)) => !s.is_empty(),
        _ => false,
    }
}

/// Verify PR `<num>` is actually merged by polling `gh pr view <num> --json mergedAt`. Never trust
/// the `gh pr merge` exit code alone — a 0 return can mean the command queued but did not complete,
/// or a transient blip swallowed the API error. Returns true only when `mergedAt` is a real
/// timestamp.
fn confirm_merged(c: &Ctx, num: i64) -> bool {
    let p = c.gh(&["pr", "view", &num.to_string(), "--json", "mergedAt"], 120);
    parse_merged_at(&p.stdout)
}

/// run_improver._try_squash_merge (~1998-2011): `gh pr merge <num> --squash --delete-branch` with up
/// to 3 attempts (5s backoff between) to ride out a transient gh/network blip on already-green CI.
/// Returns the last RunOut (code 0 == merged).
pub fn try_squash_merge(c: &Ctx, num: i64) -> proc::RunOut {
    let mut m = proc::RunOut {
        code: -1,
        stdout: String::new(),
        stderr: String::new(),
    };
    for attempt in 0..3 {
        m = c.gh(&["pr", "merge", &num.to_string(), "--squash", "--delete-branch"], 120);
        if m.code == 0 {
            return m;
        }
        if attempt < 2 {
            std::thread::sleep(Duration::from_secs(5));
        }
    }
    m
}

/// run_improver._auto_merge (~2014-2036): squash-merge an open PR, but NEVER on CI-red. checks
/// "failure" -> leave open ("open (CI red — not merged)"); "pending" -> queue GitHub native
/// auto-merge, but if `--auto` is rejected (no required status checks on the repo) FALL THROUGH to
/// a direct squash-merge instead of stranding the PR open; "success"/None -> merge now. On a
/// confirmed merge state="merged"; a failed or unverified merge leaves the PR open (logged).
///
/// ship-bug fix: the original code used `--auto` even when the repo had NO required status checks.
/// GitHub rejects `--auto` on a clean-mergeable PR (auto-merge needs a pending gate), so the command
/// failed and the PR was left OPEN while the lane mis-logged 'merged'. Now `--auto` is only used when
/// checks are actually pending, a `--auto` rejection falls through to direct merge, and the merge is
/// VERIFIED by polling `gh pr view --json mergedAt` before ever returning "merged".
pub fn auto_merge(c: &mut Ctx, pr: &Value) -> Value {
    let num = match pr.get("number").and_then(Value::as_i64) {
        Some(n) if n != 0 => n,
        _ => return pr.clone(),
    };
    let checks = pr.get("checks").and_then(Value::as_str);
    if checks == Some("failure") {
        c.log(&format!("auto-merge: CI FAILING on PR {num} — leaving open, NOT merging"));
        return with_state(pr, "open (CI red — not merged)");
    }
    if checks == Some("pending") {
        let am = c.gh(
            &["pr", "merge", &num.to_string(), "--auto", "--squash", "--delete-branch"],
            120,
        );
        if am.code == 0 {
            return with_state(pr, "auto-merge queued (awaiting CI)");
        }
        // --auto was rejected (likely no required status checks on the repo) — fall through to a
        // direct merge instead of stranding the PR open for hours.
        c.log(&format!(
            "auto-merge: --auto unavailable on PR {num} (likely no required checks) — trying direct merge"
        ));
    }
    // checks is None, "success", or "pending" with --auto rejected -> direct merge
    let m = try_squash_merge(c, num);
    if m.code == 0 && confirm_merged(c, num) {
        // Post-merge ship-gate: verify the MERGED RESULT on origin/main, not just this PR's
        // branch. Two independently-green PRs can conflict semantically and break the merged
        // base. If the post-merge gate is RED, revert the merge to heal the base.
        if post_merge_gate(c, num) {
            return with_state(pr, "merged");
        }
        return with_state(pr, POST_MERGE_REVERT_STATE);
    }
    if m.code == 0 {
        // The command returned 0 but mergedAt is null — the merge did NOT actually land. Never
        // trust the exit code alone (the ship-bug: PRs piled up open while the lane logged 'merged').
        c.log(&format!(
            "gh pr merge {num} returned 0 but mergedAt is null — PR NOT actually merged"
        ));
        return with_state(pr, "open (merge unverified)");
    }
    c.log(&format!(
        "gh pr merge {num} failed: {} — PR left open",
        slice_chars(m.stderr.trim(), 200)
    ));
    pr.clone()
}

/// run_improver._wait_for_ci_then_merge (~2046-2085): auto-condense to main BEFORE the iteration
/// finishes — poll CI up to CI_WAIT_CEILING_S, then MERGE on green (squash+delete) or REVERT on red
/// (close PR + delete branch). The CI-red revert is checked BEFORE the STOP halt, so a mid-wait stop
/// can't strand a known-red PR; a STOP on green/pending leaves the PR open ("open (stopped before
/// merge)"). A CONFIRMED 'no CI configured' (empty rollup, gh succeeded) is merge-eligible, but a gh
/// outage that leaves CI status UNKNOWN is NOT — it leaves the PR open ("open (CI unverifiable)")
/// rather than merging without verification. If CI stays pending past the ceiling, hand off to
/// GitHub native auto-merge.
pub fn wait_for_ci_then_merge(c: &mut Ctx, pr: &Value) -> Value {
    let num = match pr.get("number").and_then(Value::as_i64) {
        Some(n) if n != 0 => n,
        _ => return pr.clone(),
    };
    c.heartbeat(json!({"phase": "merge"}));
    let deadline = std::time::Instant::now() + Duration::from_secs(CI_WAIT_CEILING_S);
    loop {
        let mut probe = pr_checks_probe(c, Some(num));
        if probe == ChecksProbe::Unavailable {
            // gh status unknown — re-poll a short window to disambiguate a transient gh blip from a
            // real 'no CI configured'. Only Unavailable is retried; a State/NoChecks is conclusive.
            probe = await_pr_checks_probe(c, Some(num), 6, 8.0);
        }
        if probe == ChecksProbe::State("failure".to_string()) {
            // CI RED: ALWAYS auto-revert, even with a pending STOP (checked before the STOP halt).
            c.gh(&["pr", "close", &num.to_string(), "--delete-branch"], 120);
            c.log(&format!("CI RED on PR {num} — closed PR + deleted branch (auto-revert)"));
            return with_state(pr, "reverted (CI red)");
        }
        if stop_exists(c) {
            // a live operator STOP leaves a green/pending PR OPEN (does not ship).
            return with_state(pr, "open (stopped before merge)");
        }
        // gh status could not be read across the whole poll window: DO NOT merge an un-CI'd PR (a gh
        // outage must not be folded into the safe-to-merge 'no CI' branch). Leave the PR open so a
        // real required check can't be bypassed; the operator/next iteration reconciles it.
        if probe == ChecksProbe::Unavailable {
            c.log(&format!(
                "CI status unverifiable for PR {num} (gh unreachable across the poll window) — \
                 leaving PR open rather than merging without CI"
            ));
            return with_state(pr, "open (CI unverifiable)");
        }
        // Merge-eligible ONLY on a CONFIRMED empty rollup (NoChecks) or a green State("success").
        if probe == ChecksProbe::NoChecks || probe == ChecksProbe::State("success".to_string()) {
            let m = try_squash_merge(c, num);
            if m.code == 0 && confirm_merged(c, num) {
                // Post-merge ship-gate: verify the MERGED RESULT on origin/main, not just this
                // PR's branch. Two independently-green PRs can conflict semantically and break
                // the merged base. If the post-merge gate is RED, revert the merge to heal the
                // base rather than proceeding to the next iteration on a broken base.
                if post_merge_gate(c, num) {
                    return with_state(pr, "merged");
                }
                return with_state(pr, POST_MERGE_REVERT_STATE);
            }
            if m.code == 0 {
                c.log(&format!(
                    "gh pr merge {num} returned 0 but mergedAt is null — PR NOT actually merged"
                ));
                return with_state(pr, "open (merge unverified)");
            }
            c.log(&format!(
                "gh pr merge {num} failed after retries: {} — PR left open",
                slice_chars(m.stderr.trim(), 200)
            ));
            return with_state(pr, "open (merge failed)");
        }
        if std::time::Instant::now() >= deadline {
            let am = c.gh(
                &["pr", "merge", &num.to_string(), "--auto", "--squash", "--delete-branch"],
                120,
            );
            if am.code == 0 {
                return with_state(pr, "auto-merge queued (awaiting CI)");
            }
            // --auto rejected (likely no required checks) — try direct merge with verification
            // instead of stranding the PR open for hours.
            let m = try_squash_merge(c, num);
            if m.code == 0 && confirm_merged(c, num) {
                // Post-merge ship-gate (same as the success/None path above).
                if post_merge_gate(c, num) {
                    return with_state(pr, "merged");
                }
                return with_state(pr, POST_MERGE_REVERT_STATE);
            }
            return with_state(pr, "open (awaiting CI)");
        }
        std::thread::sleep(Duration::from_secs(CI_POLL_DELAY_S));
    }
}

// --------------------------------------------------------------------------- #
// _ship_outcome / _ship_succeeded
// --------------------------------------------------------------------------- #

/// run_improver._ship_outcome (~2712-2721): the history status for a completed ship. In auto-merge
/// mode "shipped" only on a CONFIRMED merge (`"merged" in state` AND NOT `"not merged" in state`);
/// otherwise "blocked". pr/push/local defer to _ship_succeeded. `ship_mode` is matched exactly (not
/// lowercased); `state` is lowercased before substring checks.
pub fn ship_outcome(pr: &Value, ship_mode: &str) -> String {
    let state = state_lower(pr);
    if ship_mode == "auto-merge" {
        return if state.contains("merged") && !state.contains("not merged") {
            "shipped".to_string()
        } else {
            "blocked".to_string()
        };
    }
    if ship_succeeded(pr) {
        "shipped".to_string()
    } else {
        "blocked".to_string()
    }
}

/// run_improver._ship_succeeded (~2724-2744): a terminal ship that LANDED (advances the backlog
/// item). Substring logic on the lowercased state:
///   - any "fail"/"revert" -> False;
///   - a PR (truthy number) -> True unless the state contains the "open (" marker (the un-landed
///     auto-merge/stopped states); a plain pr-mode "open" lacks "(" so is True;
///   - a kept-local branch ("local" without "pending") -> True;
///   - else a verified push: state.startswith("pushed") AND pr["verified"] truthy.
pub fn ship_succeeded(pr: &Value) -> bool {
    let state = state_lower(pr);
    if state.contains("fail") || state.contains("revert") {
        return false;
    }
    if number_truthy(pr) {
        // un-landed states all contain the "open (" marker; substring-safe ("not merged" contains
        // "merged", so we must NOT test for "merged" directly).
        return !state.contains("open (");
    }
    if state.contains("local") && !state.contains("pending") {
        return true;
    }
    state.starts_with("pushed")
        && pr
            .get("verified")
            .map(value_truthy)
            .unwrap_or(false)
}

/// Python truthiness for the `bool(pr.get("verified"))` check: True only for `true`, a nonzero
/// number, or a non-empty string/array/object; null/false/0/""/empty are falsy.
fn value_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

// --------------------------------------------------------------------------- #
// _ship
// --------------------------------------------------------------------------- #

/// run_improver._ship (~2747-2816): ship the gate-green committed branch per SHIP mode.
///   local      -> keep the local branch; no push/PR.
///   push       -> push the branch; no PR.
///   pr         -> push + open PR (default).
///   auto-merge -> push + open PR, then poll CI + squash-merge.
/// push/pr/auto-merge without a remote degrade to local. Returns a dict with keys number, url,
/// branch, state (+ verified/checks where applicable), byte-identical to the Python dict.
///
/// RSI v3 tiered ship: when the repo row declares `tiers.money_globs` and this branch's diff
/// touches a money-path file, an `auto-merge` config is downgraded to `pr` for THIS ship only
/// unless the fitness needle was ACTIVE for the iteration (EVAL_CMD configured + an after-score
/// parsed — see tiers::eval_needle_active; iteration.rs is outside this workstream's writable
/// scope, so activity is re-derived from the heartbeat keys the eval gate already writes rather
/// than a new parameter). A money-path diff may auto-land ONLY under an active non-regressing
/// needle; with the needle inactive a human merges it. `c.ship` itself is never mutated, and
/// rows without `tiers` are byte-identical legacy.
pub fn ship(c: &mut Ctx, branch: &str, title: &str, summary: &str, tests: &Value) -> Value {
    let mut ship_mode = c.ship.clone();
    let row = gitops::repo_row(c, &c.name);
    if let Some(row_tiers) = row.get("tiers") {
        let base = c.base_branch.clone();
        let files = tiers::changed_files(c, &base);
        let eval_active = tiers::eval_needle_active(c);
        if let Some(mode) = tiers::money_ship_override(row_tiers, &files, eval_active, &ship_mode) {
            c.log(
                "tiered ship: money-path diff + inactive eval needle — shipping as PR (human merge), not auto-merge",
            );
            ship_mode = mode;
        }
    }
    let is_push_pr_am = matches!(ship_mode.as_str(), "push" | "pr" | "auto-merge");

    if is_push_pr_am && !c.has_remote() {
        c.log(&format!("ship={ship_mode} but no remote — kept local"));
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": "local (no remote)",
        });
    }

    if ship_mode == "local" {
        c.log(&format!("ship=local — kept committed branch {branch} locally (unshipped)"));
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": "local branch (unshipped)",
        });
    }

    // push / pr / auto-merge all need gh + a successful push first
    if !gh_ready(c) {
        c.log("gh not ready — branch committed locally; ship pending `gh auth login`");
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": "local (ship pending gh auth)",
        });
    }
    c.heartbeat(json!({"phase": "ship"}));
    // push timeout=300 (larger than git()'s 120s default) so a slow-but-working push isn't false-failed.
    let push = c.git(&["push", "-u", "origin", branch], 300);
    if push.code != 0 {
        c.log(&format!("git push failed: {}", slice_chars(push.stderr.trim(), 200)));
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": "push-failed",
            "verified": false,
        });
    }

    // verify the push actually landed on origin — not just a zero return code
    let verified = c.branch_on_remote(branch);
    if !verified {
        c.log(&format!(
            "WARNING: push reported success but '{branch}' is not visible on origin"
        ));
        if matches!(ship_mode.as_str(), "pr" | "auto-merge") {
            c.heartbeat(json!({"status": "error", "phase": "ship"}));
            return json!({
                "number": Value::Null,
                "url": Value::Null,
                "branch": branch,
                "state": "push-unverified (failed)",
                "verified": false,
            });
        }
    }

    if ship_mode == "push" {
        c.log(&format!(
            "ship=push — pushed {branch} (no PR){}",
            if verified { "" } else { " [UNVERIFIED]" }
        ));
        return json!({
            "number": Value::Null,
            "url": Value::Null,
            "branch": branch,
            "state": if verified { "pushed (no PR)" } else { "pushed (unverified)" },
            "verified": verified,
        });
    }

    c.heartbeat(json!({"phase": "pr"}));
    let mut pr = open_pr(c, branch, title, summary, tests);
    if let Value::Object(m) = &mut pr {
        m.insert("verified".to_string(), json!(verified));
    }
    // auto-merge polls so an empty rollup right after PR-create is 'CI not reported yet' (retry);
    // other ship modes only display checks, so a single read suffices.
    let num = pr.get("number").and_then(Value::as_i64);
    let checks = if ship_mode == "auto-merge" {
        await_pr_checks(c, num, 6, 8.0)
    } else {
        pr_checks(c, num)
    };
    if let Value::Object(m) = &mut pr {
        m.insert(
            "checks".to_string(),
            match &checks {
                Some(s) => json!(s),
                None => Value::Null,
            },
        );
    }
    c.log(&format!(
        "opened PR: {} (push {}, CI {})",
        pr.get("url").and_then(Value::as_str).unwrap_or("None"),
        if verified { "verified" } else { "UNVERIFIED" },
        checks.as_deref().unwrap_or("none"),
    ));
    if ship_mode == "auto-merge" {
        if stop_exists(c) {
            // a Stop arrived during _await_pr_checks polling: do NOT auto-merge (merging on a None
            // rollup would land an un-CI'd PR). Leave it open for review (honors the halt switch).
            c.log("stop requested — not auto-merging; PR left open for review");
            pr = with_state(&pr, "open (stopped before merge)");
        } else {
            pr = wait_for_ci_then_merge(c, &pr);
        }
        c.log(&format!(
            "ship=auto-merge — {}: {}",
            pr.get("state").and_then(Value::as_str).unwrap_or("None"),
            pr.get("url").and_then(Value::as_str).unwrap_or("None"),
        ));
    }
    pr
}

// --------------------------------------------------------------------------- #
// tests — load-bearing pure logic (state parsers, reason builders, predicates)
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(state: &str) -> Value {
        json!({"number": Value::Null, "url": Value::Null, "branch": "b", "state": state})
    }

    // ---- _ship_succeeded ----

    #[test]
    fn ss_fail_and_revert() {
        assert!(!ship_succeeded(&json!({"state": "push-failed"})));
        assert!(!ship_succeeded(&json!({"state": "reverted (CI red)"})));
        // 'open (merge failed)' has both 'fail' and 'open (' — fail check wins first.
        assert!(!ship_succeeded(&json!({"number": 7, "state": "open (merge failed)"})));
    }

    #[test]
    fn ss_pr_open_marker() {
        // plain 'open' (pr-mode) has no '(' -> landed
        assert!(ship_succeeded(&json!({"number": 1, "state": "open"})));
        // un-landed auto-merge states contain 'open ('
        assert!(!ship_succeeded(&json!({"number": 1, "state": "open (CI red — not merged)"})));
        assert!(!ship_succeeded(&json!({"number": 1, "state": "open (awaiting CI)"})));
        assert!(!ship_succeeded(&json!({"number": 1, "state": "open (stopped before merge)"})));
        // 'merged' / 'auto-merge queued (...)' land (no 'open (' marker)
        assert!(ship_succeeded(&json!({"number": 1, "state": "merged"})));
        assert!(ship_succeeded(&json!({"number": 1, "state": "auto-merge queued (awaiting CI)"})));
        // number 0 is falsy -> not the PR branch
        assert!(!ship_succeeded(&json!({"number": 0, "state": "open"})));
    }

    #[test]
    fn ss_local_and_push() {
        assert!(ship_succeeded(&pr("local branch (unshipped)")));
        assert!(!ship_succeeded(&pr("local (something pending)")));
        // push needs startswith('pushed') AND verified truthy
        assert!(ship_succeeded(&json!({"state": "pushed (no PR)", "verified": true})));
        assert!(!ship_succeeded(&json!({"state": "pushed (no PR)", "verified": false})));
        assert!(!ship_succeeded(&json!({"state": "pushed (unverified)", "verified": false})));
    }

    // ---- _ship_outcome ----

    #[test]
    fn outcome_auto_merge() {
        assert_eq!(ship_outcome(&pr("merged"), "auto-merge"), "shipped");
        // 'not merged' contains 'merged' but must be blocked
        assert_eq!(ship_outcome(&pr("open (CI red — not merged)"), "auto-merge"), "blocked");
        assert_eq!(ship_outcome(&pr("auto-merge queued (awaiting CI)"), "auto-merge"), "blocked");
        assert_eq!(ship_outcome(&pr("open (awaiting CI)"), "auto-merge"), "blocked");
    }

    // rsi-supervisor-watchdog-2: the backlog-advance predicate (now ship_outcome=="shipped") and the
    // history predicate (ship_outcome) must AGREE on an auto-merge-QUEUED PR — it is NOT landed.
    // ship_succeeded alone (the old advance predicate) wrongly returns true for the queued state.
    #[test]
    fn auto_merge_queued_is_not_landed_but_ship_succeeded_disagrees() {
        let queued = json!({"number": 1, "state": "auto-merge queued (awaiting CI)"});
        // The OLD advance predicate is wrong here (this is the bug):
        assert!(
            ship_succeeded(&queued),
            "ship_succeeded wrongly treats a queued PR as landed"
        );
        // The predicate iteration.rs now uses agrees with history — NOT landed:
        assert_eq!(ship_outcome(&queued, "auto-merge"), "blocked");
        assert_ne!(ship_outcome(&queued, "auto-merge"), "shipped");
        // A confirmed merge IS landed under the same predicate.
        let merged = json!({"number": 1, "state": "merged"});
        assert_eq!(ship_outcome(&merged, "auto-merge"), "shipped");
    }

    // rsi-supervisor-watchdog-3: the ChecksProbe classifier must keep 'no CI configured' (confirmed
    // empty rollup) distinct from 'gh status unknown', so a gh outage is never merged as safe-no-CI.
    #[test]
    fn checks_probe_distinguishes_no_ci_from_unknown() {
        // An empty rollup is a CONFIRMED no-CI (merge-eligible).
        assert_eq!(probe_reduce_ok(&json!([])), ChecksProbe::NoChecks);
        // A green rollup is State("success").
        let success = json!([{"state": "SUCCESS", "status": "COMPLETED", "conclusion": "SUCCESS"}]);
        assert_eq!(
            probe_reduce_ok(&success),
            ChecksProbe::State("success".to_string())
        );
        // Only NoChecks or State("success") are merge-eligible; Unavailable (gh unknown) is NOT.
        for p in [
            ChecksProbe::NoChecks,
            ChecksProbe::State("success".to_string()),
        ] {
            assert!(is_merge_eligible(&p), "{p:?} should be merge-eligible");
        }
        assert!(
            !is_merge_eligible(&ChecksProbe::Unavailable),
            "gh-unknown must NOT be merge-eligible"
        );
        assert!(!is_merge_eligible(&ChecksProbe::State(
            "pending".to_string()
        )));
        assert!(!is_merge_eligible(&ChecksProbe::State(
            "failure".to_string()
        )));
    }

    #[test]
    fn outcome_pr_mode_defers_to_succeeded() {
        assert_eq!(ship_outcome(&json!({"number": 1, "state": "open"}), "pr"), "shipped");
        assert_eq!(
            ship_outcome(&json!({"number": 1, "state": "open (awaiting CI)"}), "pr"),
            "blocked"
        );
        // ship_mode is exact-match: 'Auto-Merge' is NOT the auto-merge path
        assert_eq!(ship_outcome(&pr("merged"), "Auto-Merge"), "blocked");
    }

    // ---- _pr_title ----

    #[test]
    fn pr_title_uses_goal() {
        let c = test_ctx();
        assert_eq!(pr_title(&c, "Add a retry to the fetcher", ""), "Add a retry to the fetcher");
    }

    #[test]
    fn pr_title_placeholder_falls_back_case_insensitive() {
        let c = test_ctx();
        // case-insensitive placeholder match -> fall back to summary
        assert_eq!(
            pr_title(&c, "Model-Chosen Improvement", "  \nFirst real line\nmore"),
            "First real line"
        );
        // empty goal -> fallback
        assert_eq!(pr_title(&c, "", "Only line"), "Only line");
        // all-blank summary -> 'improvement'
        assert_eq!(pr_title(&c, "model-chosen improvement", "  \n\t"), "improvement");
    }

    #[test]
    fn pr_title_truncates_to_72() {
        let c = test_ctx();
        let long = "x".repeat(100);
        assert_eq!(pr_title(&c, &long, "").chars().count(), 72);
    }

    // ---- _pr_checks reduction (pure rollup -> verdict) via a parse harness ----

    #[test]
    fn checks_failure_overrides_pending() {
        // a FAILURE conclusion sets bad; a non-completed status sets pending; bad wins.
        let rollup = json!([
            {"state": "COMPLETED", "status": "COMPLETED", "conclusion": "FAILURE"},
            {"state": "PENDING", "status": "IN_PROGRESS", "conclusion": ""}
        ]);
        assert_eq!(reduce_rollup(&rollup), Some("failure".to_string()));
    }

    #[test]
    fn checks_pending_then_success() {
        let pending = json!([{"state": "PENDING", "status": "QUEUED", "conclusion": ""}]);
        assert_eq!(reduce_rollup(&pending), Some("pending".to_string()));
        let success = json!([{"state": "SUCCESS", "status": "COMPLETED", "conclusion": "SUCCESS"}]);
        assert_eq!(reduce_rollup(&success), Some("success".to_string()));
    }

    #[test]
    fn checks_empty_rollup_is_none() {
        assert_eq!(reduce_rollup(&json!([])), None);
    }

    // Mirror of the _pr_checks reduction loop, factored out for unit testing without a live gh.
    fn reduce_rollup(rollup: &Value) -> Option<String> {
        let arr = rollup.as_array().cloned().unwrap_or_default();
        if arr.is_empty() {
            return None;
        }
        let mut bad = false;
        let mut pend = false;
        for chk in &arr {
            let st = chk.get("state").and_then(Value::as_str).unwrap_or("").to_uppercase();
            let status = chk.get("status").and_then(Value::as_str).unwrap_or("").to_uppercase();
            let concl = chk.get("conclusion").and_then(Value::as_str).unwrap_or("").to_uppercase();
            if (!status.is_empty() && status != "COMPLETED") || st == "PENDING" {
                pend = true;
            }
            if matches!(st.as_str(), "FAILURE" | "ERROR")
                || matches!(
                    concl.as_str(),
                    "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
                )
            {
                bad = true;
            }
        }
        Some(if bad {
            "failure".to_string()
        } else if pend {
            "pending".to_string()
        } else {
            "success".to_string()
        })
    }

    // Test mirror of pr_checks_probe's SUCCESS-path reduction (gh succeeded): empty rollup ->
    // NoChecks (confirmed no CI); non-empty -> State(reduced). Mirrors the real reduction so the
    // NoChecks-vs-State boundary is pinned without a live gh.
    fn probe_reduce_ok(rollup: &Value) -> ChecksProbe {
        match reduce_rollup(rollup) {
            None => ChecksProbe::NoChecks, // gh succeeded + empty rollup == confirmed no CI
            Some(s) => ChecksProbe::State(s),
        }
    }

    // The merge-eligibility rule used by wait_for_ci_then_merge: only a confirmed NoChecks or a green
    // State("success") may merge; Unavailable (gh unknown), pending, and failure may NOT.
    fn is_merge_eligible(p: &ChecksProbe) -> bool {
        *p == ChecksProbe::NoChecks || *p == ChecksProbe::State("success".to_string())
    }

    // ---- parse_merged_at: the merge-verification gate (ship-bug fix) ----
    //
    // `gh pr view <n> --json mergedAt` is the authoritative source of truth for whether a PR
    // actually landed. The `gh pr merge` exit code alone is NOT trusted — a 0 return can mean the
    // command queued but did not complete, or a transient blip swallowed the API error. This is the
    // fix for the ship-bug where PRs #13-22 piled up open for 8h while the lane mis-logged 'merged'.

    #[test]
    fn merged_at_real_timestamp_is_merged() {
        assert!(parse_merged_at(r#"{"mergedAt":"2026-07-01T12:00:00Z"}"#));
    }

    #[test]
    fn merged_at_null_is_not_merged() {
        assert!(!parse_merged_at(r#"{"mergedAt":null}"#));
    }

    #[test]
    fn merged_at_missing_is_not_merged() {
        assert!(!parse_merged_at(r#"{}"#));
        assert!(!parse_merged_at(""));
    }

    #[test]
    fn merged_at_empty_string_is_not_merged() {
        assert!(!parse_merged_at(r#"{"mergedAt":""}"#));
    }

    #[test]
    fn merged_at_garbage_json_is_not_merged() {
        assert!(!parse_merged_at("not json"));
        assert!(!parse_merged_at("{broken"));
    }

    // ---- ship_succeeded rejects the new unverified state ----
    //
    // 'open (merge unverified)' must NOT be treated as shipped — it contains 'open (' so the
    // existing predicate already rejects it, but pin the invariant explicitly.

    #[test]
    fn ss_merge_unverified_is_not_shipped() {
        assert!(!ship_succeeded(&json!({"number": 1, "state": "open (merge unverified)"})));
        assert_eq!(
            ship_outcome(&json!({"number": 1, "state": "open (merge unverified)"}), "auto-merge"),
            "blocked"
        );
    }

    // ---- no-CI PR is direct-merged, not left open ----
    //
    // When checks is None (no CI configured / no required checks), auto_merge must take the direct
    // merge path (try_squash_merge, no --auto). The --auto flag is only used when checks are
    // actually "pending". A confirmed merge (mergedAt is a real timestamp) returns "merged";
    // an unverified merge (code 0 but mergedAt null) returns "open (merge unverified)" — NOT
    // "merged". This is the regression test for the ship-bug where --auto was used on a no-CI
    // repo, GitHub rejected it, and the PR was left open while the lane logged 'merged'.

    #[test]
    fn no_ci_pr_confirmed_merge_is_merged() {
        // parse_merged_at with a real timestamp -> true -> state would be "merged"
        let gh_view_out = r#"{"mergedAt":"2026-07-01T12:00:00Z"}"#;
        assert!(parse_merged_at(gh_view_out));
        // and "merged" state is correctly classified as shipped
        assert!(ship_succeeded(&json!({"number": 1, "state": "merged"})));
        assert_eq!(ship_outcome(&json!({"number": 1, "state": "merged"}), "auto-merge"), "shipped");
    }

    #[test]
    fn no_ci_pr_unverified_merge_is_not_merged() {
        // parse_merged_at with null mergedAt -> false -> state would be "open (merge unverified)"
        let gh_view_out = r#"{"mergedAt":null}"#;
        assert!(!parse_merged_at(gh_view_out));
        // and "open (merge unverified)" is correctly classified as blocked, NOT shipped
        assert_eq!(
            ship_outcome(&json!({"number": 1, "state": "open (merge unverified)"}), "auto-merge"),
            "blocked"
        );
    }

    fn test_ctx() -> Ctx {
        Ctx::configure(".", "maki", "ollama-cloud", None)
    }

    // ---- post-merge ship-gate: a merge producing a RED main is caught and reverted ----
    //
    // The systemic fix for "solomon lane self-stops on base_gate_red_persistent from its own
    // back-to-back merges" (verified 2026-07-01: PRs #35+#36 both green in isolation, merged ~1s
    // apart, main went gate-RED). After a squash-merge, the ship path now fetches origin/main and
    // re-runs the gate against the MERGED HEAD. If RED, it reverts the merge and returns
    // POST_MERGE_REVERT_STATE instead of "merged" — so the backlog item does NOT advance and the
    // base is healed before the next iteration, rather than poisoning it and tripping
    // base_gate_red_persistent.

    #[test]
    fn post_merge_revert_state_is_not_shipped() {
        // Contains "reverted" -> ship_succeeded returns false (the merge was undone).
        assert!(!ship_succeeded(&json!({"number": 36, "state": POST_MERGE_REVERT_STATE})));
    }

    #[test]
    fn post_merge_revert_state_is_blocked() {
        // Does NOT contain "merged" -> ship_outcome returns "blocked", not "shipped".
        assert_eq!(
            ship_outcome(
                &json!({"number": 36, "state": POST_MERGE_REVERT_STATE}),
                "auto-merge",
            ),
            "blocked"
        );
    }

    #[test]
    fn post_merge_revert_state_contains_diagnostic() {
        // The state string must surface "main gate RED" so the lane log + dashboard show WHY the
        // merge was reverted, not just a bare "reverted".
        assert!(POST_MERGE_REVERT_STATE.contains("main gate RED"));
        assert!(POST_MERGE_REVERT_STATE.contains("reverted"));
        // Must NOT contain "merged" — otherwise ship_outcome's "merged" substring check would
        // wrongly classify it as shipped.
        assert!(!POST_MERGE_REVERT_STATE.contains("merged"));
    }
}
