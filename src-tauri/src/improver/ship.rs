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

/// run_improver._pr_checks (~1895-1918): reduce a PR's statusCheckRollup to
/// `Some("success"|"pending"|"failure")` or `None` (no checks / gh failure / unparseable / empty
/// rollup). `number` is the PR number; a falsy number returns None.
pub fn pr_checks(c: &Ctx, number: Option<i64>) -> Option<String> {
    // `if not number:` — None or 0 is falsy.
    let num = match number {
        Some(n) if n != 0 => n,
        _ => return None,
    };
    let p = c.gh(&["pr", "view", &num.to_string(), "--json", "statusCheckRollup"], 120);
    if p.code != 0 {
        return None;
    }
    // rollup = (json.loads(p.stdout or "{}") or {}).get("statusCheckRollup") or []
    let raw = if p.stdout.is_empty() { "{}" } else { &p.stdout };
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return None, // JSONDecodeError -> None
    };
    let rollup = parsed
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if rollup.is_empty() {
        return None;
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
    Some(if bad {
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
// _try_squash_merge / _auto_merge / _wait_for_ci_then_merge
// --------------------------------------------------------------------------- #

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
/// auto-merge (or "open (awaiting CI)" if --auto unavailable); "success"/None -> merge now. On a
/// successful squash-merge state="merged"; a failed merge leaves the PR unchanged (logged).
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
        c.log(&format!(
            "auto-merge: CI pending and native --auto unavailable on PR {num} — leaving open until CI resolves"
        ));
        return with_state(pr, "open (awaiting CI)");
    }
    let m = try_squash_merge(c, num);
    if m.code == 0 {
        return with_state(pr, "merged");
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
/// merge)"). No CI configured (None) is merge-eligible. If CI stays pending past the ceiling, hand
/// off to GitHub native auto-merge.
pub fn wait_for_ci_then_merge(c: &mut Ctx, pr: &Value) -> Value {
    let num = match pr.get("number").and_then(Value::as_i64) {
        Some(n) if n != 0 => n,
        _ => return pr.clone(),
    };
    c.heartbeat(json!({"phase": "merge"}));
    let deadline = std::time::Instant::now() + Duration::from_secs(CI_WAIT_CEILING_S);
    loop {
        let mut checks = pr_checks(c, Some(num));
        if checks.is_none() {
            // disambiguate 'no CI' from a transient gh blip across a short window.
            checks = await_pr_checks(c, Some(num), 6, 8.0);
        }
        if checks.as_deref() == Some("failure") {
            // CI RED: ALWAYS auto-revert, even with a pending STOP (checked before the STOP halt).
            c.gh(&["pr", "close", &num.to_string(), "--delete-branch"], 120);
            c.log(&format!("CI RED on PR {num} — closed PR + deleted branch (auto-revert)"));
            return with_state(pr, "reverted (CI red)");
        }
        if stop_exists(c) {
            // a live operator STOP leaves a green/pending PR OPEN (does not ship).
            return with_state(pr, "open (stopped before merge)");
        }
        // checks in ("success", None)
        if checks.is_none() || checks.as_deref() == Some("success") {
            let m = try_squash_merge(c, num);
            if m.code == 0 {
                return with_state(pr, "merged");
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
            return with_state(
                pr,
                if am.code == 0 {
                    "auto-merge queued (awaiting CI)"
                } else {
                    "open (awaiting CI)"
                },
            );
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
pub fn ship(c: &mut Ctx, branch: &str, title: &str, summary: &str, tests: &Value) -> Value {
    let ship_mode = c.ship.clone();
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

    fn test_ctx() -> Ctx {
        Ctx::configure(".", "maki", "ollama-cloud", None)
    }
}
