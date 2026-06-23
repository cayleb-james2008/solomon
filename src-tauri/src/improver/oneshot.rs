//! Port of run_improver.py's oneshot_modes area: the one-shot ENTRY modes that exit BEFORE the
//! iteration loop (smoke / provision / ideate). Each is a distinct control flow with its own pi
//! invocation, file ownership, parsing, and exit codes.
//!
//! Bug-for-bug with improver/run_improver.py (functions: smoke, _strip_code_fence, _parse_provision,
//! provision, _STOPWORDS/_tokenize/_jaccard/_is_novel, _read_lessons, _recent_history_summaries,
//! _parse_ideas, _ideate_research_enabled, _ideate_task, ideate). NOTE: --beautify and --solomon are
//! NOT here — they run through one_iteration() with the BEAUTIFY/SOLOMON flags set (the
//! iteration_state_machine area owns them).
//!
//! Exit codes (from the spec): 0 success; 1 smoke recoverable fail; 5 provision/ideate timeout/parse.
//! None of these three touch git, write a heartbeat, or loop — they exit directly after success/fail.
//!
//! DEVIATION (timeout detection): Python wraps `run_pi(...)` in `try/except subprocess.TimeoutExpired`.
//! The ported `pi::run_pi` does NOT raise on timeout — it returns a `RunOut{code: 124, ...}` (the same
//! rc=124 convention git()/gh() use). So the provision/ideate timeout branches key on `out.code == 124`
//! instead of catching an exception. smoke() shells out DIRECTLY via `control::proc::run` (NOT run_pi,
//! matching the Python which uses a bare `subprocess.run`), so its timeout is a true `Err(TimedOut)`.

#[cfg(windows)]
use crate::control::proc;
use crate::improver::ctx::Ctx;
use crate::improver::pi;

use serde_json::{json, Value};
use std::process::Command;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// --------------------------------------------------------------------------- #
// smoke
// --------------------------------------------------------------------------- #

/// run_improver.smoke (~2952-2976): connectivity probe. Loads env, checks the active provider's
/// required API key, then shells out to pi with a no-tools READY system-prompt + 'READY?' task arg on
/// the configured provider/model with a 120s timeout, and looks for 'READY' (case-insensitive) in the
/// final assistant text. Makes NO repo changes; no git, no heartbeat.
pub fn smoke(ctx: &mut Ctx) -> i32 {
    ctx.load_env();
    let key = ctx.required_key();
    if std::env::var(&key).map(|v| v.is_empty()).unwrap_or(true) {
        // `not os.environ.get(key)` — unset OR empty-string -> fail.
        println!("SMOKE: FAIL — {key} not set (put it in Solomon/.env)");
        return 1;
    }
    // args = [pi, --print, --mode json, -ne, --provider PI_PROVIDER, --model PI_MODEL, -e PI_EXT,
    //         --no-tools, --system-prompt "Connectivity smoke test. ...", "READY?"]
    let pi_exe = ctx.pi_exe();
    let pi_ext = ctx.pi_ext.to_string_lossy().into_owned();
    let args = [
        pi_exe.as_str(),
        "--print",
        "--mode",
        "json",
        "-ne",
        "--provider",
        ctx.pi_provider.as_str(),
        "--model",
        ctx.pi_model.as_str(),
        "-e",
        pi_ext.as_str(),
        "--no-tools",
        "--system-prompt",
        "Connectivity smoke test. Output exactly the single word READY.",
        "READY?",
    ];

    // env = _clean_env(); env["RSI_PROVIDER"]=PI_PROVIDER; env["RSI_MODEL"]=PI_MODEL.
    // proc::run applies the clean env (strips token/pythonpath, forces UTF-8 stdio) and the hidden
    // window; we layer RSI_PROVIDER/RSI_MODEL on by building the Command ourselves so the two extra
    // vars reach the provider.ts extension. (proc::run can't pass extra env, so we replicate its
    // spawn+timeout shape here — same 120s bound, same Err(TimedOut) on expiry.)
    let mut cmd = Command::new(args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&ctx.repo);
    ctx.apply_clean_env(&mut cmd); // _clean_env(): strip GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME + UTF-8
    cmd.env("RSI_PROVIDER", &ctx.pi_provider); // unified provider.ts registers under this name
    cmd.env("RSI_MODEL", &ctx.pi_model); // the extension registers exactly this model id
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    apply_hidden(&mut cmd);

    let out = match run_with_timeout(cmd, Duration::from_secs(120)) {
        Ok(o) => o,
        Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {
            // except subprocess.TimeoutExpired -> "SMOKE: FAIL — timed out"
            println!("SMOKE: FAIL — timed out");
            return 1;
        }
        Err(_) => {
            // spawn failure (missing pi): the Python would raise FileNotFoundError before the
            // try-block can catch it; surface a fail with empty final_text + empty stderr.
            RunCapture {
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    };

    let t = pi::final_text(&out.stdout);
    if t.to_uppercase().contains("READY") {
        println!("SMOKE: PASS");
        return 0;
    }
    // f"SMOKE: FAIL — {t or (p.stderr or '').strip()[:300]}"
    let detail = if t.is_empty() {
        // p.stderr.strip()[:300] — strip, then first 300 chars (char-wise to stay UTF-8 safe).
        out.stderr.trim().chars().take(300).collect::<String>()
    } else {
        t
    };
    println!("SMOKE: FAIL — {detail}");
    1
}

/// Minimal capture of a child's exit + streams (smoke's local subprocess.run shape).
struct RunCapture {
    stdout: String,
    stderr: String,
}

/// subprocess.run(args, capture_output=True, text=True, timeout=120): spawn, wait up to `dur`, drain
/// both pipes (UTF-8 lossy == errors="replace"); on expiry kill + Err(TimedOut), matching proc::run.
fn run_with_timeout(mut cmd: Command, dur: Duration) -> std::io::Result<RunCapture> {
    use std::io::Read;
    use wait_timeout::ChildExt;
    let mut child = cmd.spawn()?;
    match child.wait_timeout(dur)? {
        Some(_status) => {
            let mut out = Vec::new();
            let mut err = Vec::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_end(&mut out);
            }
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_end(&mut err);
            }
            Ok(RunCapture {
                stdout: String::from_utf8_lossy(&out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
            })
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "subprocess timed out",
            ))
        }
    }
}

#[cfg(windows)]
fn apply_hidden(cmd: &mut Command) {
    cmd.creation_flags(proc::hidden_flags(false, false));
}
#[cfg(not(windows))]
fn apply_hidden(_cmd: &mut Command) {}

// --------------------------------------------------------------------------- #
// provision (one-shot contract generation)
// --------------------------------------------------------------------------- #

/// run_improver._strip_code_fence (~2980-2992): strip a SINGLE leading/trailing ``` code fence if
/// present — NOT all backtick chars (str.strip('`') would mangle content whose first/last line is an
/// inline-code span). Only a complete fence line (just ``` optionally + a language tag) is removed.
fn strip_code_fence(text: &str) -> String {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // leading fence: re.match(r"^```\w*\s*$", lines[0].strip())
    static LEAD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static TRAIL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let lead = LEAD.get_or_init(|| regex::Regex::new(r"^```\w*\s*$").unwrap());
    let trail = TRAIL.get_or_init(|| regex::Regex::new(r"^```\s*$").unwrap());
    if let Some(first) = lines.first() {
        if lead.is_match(first.trim()) {
            lines.remove(0);
        }
    }
    if let Some(last) = lines.last() {
        if trail.is_match(last.trim()) {
            lines.pop();
        }
    }
    lines.join("\n").trim().to_string()
}

/// run_improver._parse_provision (~2995-3007): pull the two ===AGENT.md=== / ===backlog.md=== blocks
/// from the provisioner output. Returns (agent_md, backlog_md), each None when absent/empty. The
/// backlog block runs to end-of-text (NOT split at the next '===' substring — a markdown rule/table
/// inside the backlog would be wrongly truncated); _strip_code_fence removes a wrapping fence.
fn parse_provision(text: &str) -> (Option<String>, Option<String>) {
    if !text.contains("===AGENT.md===") || !text.contains("===backlog.md===") {
        return (None, None);
    }
    // after = text.split("===AGENT.md===", 1)[1]
    let after = text.split_once("===AGENT.md===").map_or("", |(_, a)| a);
    // agent_part, backlog_part = after.split("===backlog.md===", 1)
    let mut it = after.splitn(2, "===backlog.md===");
    let agent_part = it.next().unwrap_or("");
    let backlog_part = it.next().unwrap_or("");
    let agent_md = strip_code_fence(agent_part.trim());
    let backlog_md = strip_code_fence(backlog_part.trim());
    // (agent_md or None), (backlog_md or None)  — empty string -> None
    let agent = if agent_md.is_empty() { None } else { Some(agent_md) };
    let backlog = if backlog_md.is_empty() {
        None
    } else {
        Some(backlog_md)
    };
    (agent, backlog)
}

/// run_improver.provision (~3010-3039): one-shot — pi reads the repo and emits the two contract blocks;
/// the runner writes them (Python owns the filesystem — pi never writes contract files). Prints one
/// JSON line. No gate, no git, no loop. PHASE='provision' (set by the caller before invoking).
pub fn provision(ctx: &mut Ctx) -> i32 {
    let goal_line = if !ctx.goal.is_empty() {
        format!(
            "\n\nThe operator's NORTH-STAR GOAL for this project (weigh it heavily — the \
contract must lead with it and the backlog must be ordered to advance it, \
including building any capability the goal needs that the project lacks):\n{}\n",
            ctx.goal
        )
    } else {
        String::new()
    };
    let task = format!(
        "Read this repository and generate its Solomon improver contract and backlog per \
provision.md. Output ONLY the two fenced blocks.{goal_line}"
    );
    let provision_md = ctx.provision_md.clone();
    let p = pi::run_pi(ctx, &task, 600, Some(&provision_md));
    if p.code == 124 {
        // except subprocess.TimeoutExpired
        println!("{}", json!({"ok": false, "error": "provisioner timed out"}));
        return 5;
    }
    let final_t = pi::final_text(&p.stdout);
    let (agent_md, backlog_md) = parse_provision(&final_t);
    let (agent_md, backlog_md) = match (agent_md, backlog_md) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            println!(
                "{}",
                json!({"ok": false, "error": "provisioner did not emit both blocks"})
            );
            return 5;
        }
    };
    // AGENT_MD.parent.mkdir(parents=True, exist_ok=True); AGENT_MD.write_text; BACKLOG.write_text.
    if let Some(parent) = ctx.agent_md.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            println!("{}", json!({"ok": false, "error": e.to_string()}));
            return 5;
        }
    }
    if let Err(e) = std::fs::write(&ctx.agent_md, &agent_md) {
        println!("{}", json!({"ok": false, "error": e.to_string()}));
        return 5;
    }
    if let Err(e) = std::fs::write(&ctx.backlog, &backlog_md) {
        println!("{}", json!({"ok": false, "error": e.to_string()}));
        return 5;
    }
    // first = next((ln.strip() for ln in agent_md.splitlines() if ln.strip()), "")
    let first = agent_md
        .split('\n')
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("");
    println!(
        "{}",
        json!({
            "ok": true,
            "agent_written": char_len(&agent_md),
            "backlog_written": char_len(&backlog_md),
            "summary": first.chars().take(80).collect::<String>(),
        })
    );
    0
}

// --------------------------------------------------------------------------- #
// novelty / dedup (dependency-free token-Jaccard similarity)
// --------------------------------------------------------------------------- #

/// run_improver._STOPWORDS (~3046-3048): the stopword set dropped from similarity tokens.
fn stopwords() -> std::collections::HashSet<&'static str> {
    "a an the and or but for to of in on at by with from into as is are be it this that these those \
add adds added use uses using make makes made into via per its it's not no than then so we i"
        .split_whitespace()
        .collect()
}

/// run_improver._tokenize (~3051-3054): lowercase word tokens (r"[a-z0-9]+"), len>2, stopwords dropped.
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    // r"[a-z0-9]+" over a lowercased string == maximal runs of ascii-alphanumerics; stdlib split gives
    // the same tokens with no regex compile (this runs per corpus item inside is_novel's O(N) loop).
    let sw = stopwords();
    let lower = text.to_lowercase();
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(str::to_string)
        .filter(|w| w.chars().count() > 2 && !sw.contains(w.as_str()))
        .collect()
}

/// run_improver._is_novel (~3066-3088): True if `idea` is NOT a near-duplicate of any corpus string.
/// Empty corpus / empty idea-tokens are novel. Two signals at/over threshold -> not novel:
/// symmetric Jaccard (inter/union >= threshold) and containment (inter/min(len) >= threshold + 0.15).
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
        let minlen = toks.len().min(ot.len());
        if (inter as f64) / (minlen as f64) >= threshold + 0.15 {
            return false; // containment (subsumed by a longer item)
        }
    }
    true
}

/// run_improver._read_lessons (~3091-3098): LESSONS.md text, '' when absent/unreadable.
fn read_lessons(ctx: &Ctx) -> String {
    std::fs::read_to_string(&ctx.lessons).unwrap_or_default()
}

/// run_improver._recent_history_summaries (~3101-3120): the last `limit` non-empty history.jsonl
/// iteration summaries (newest-biased). Absent/unparseable file -> empty list; malformed/empty
/// summaries skipped.
fn recent_history_summaries(ctx: &Ctx, limit: usize) -> Vec<String> {
    let mut out = Vec::new();
    let content = match std::fs::read_to_string(ctx.runtime.join("history.jsonl")) {
        Ok(c) => c,
        Err(_) => return out,
    };
    let lines: Vec<&str> = content.split('\n').collect();
    let start = lines.len().saturating_sub(limit);
    for line in &lines[start..] {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // json.JSONDecodeError -> skip
        };
        let s = rec.get("summary").and_then(Value::as_str).unwrap_or("").trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    }
    out
}

// --------------------------------------------------------------------------- #
// ideate (divergent backlog generation — the anti-shallowness lane)
// --------------------------------------------------------------------------- #

/// run_improver._parse_ideas (~3124-3154): parse the ideate lane's idea lines into (leverage, tier,
/// idea) tuples, highest-leverage first. TWO-PATH: strict tier-tagged lines (feature|refactor|
/// architecture only; chore excluded) when the model complies; else FALL BACK to plain idea lines
/// (tier='feature', leverage=3) with noise guards. Returns the chosen list sorted by -leverage.
fn parse_ideas(text: &str) -> Vec<(i64, String, String)> {
    // strict tier regex (re.I)
    static TIER_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BULLET_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static META_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let tier_re = TIER_RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"^[\s\-*\d.)#>]*\**\[?\s*(feature|refactor|architecture)\s*\]?\**\s*[|:\-–—]*\s*(\d+)?\s*[|:\-–—]*\s*(.+?)\s*$",
        )
        .case_insensitive(true)
        .build()
        .unwrap()
    });
    // fallback bullet-strip + meta-reject (re.I on the meta-prefix check)
    let bullet_re = BULLET_RE.get_or_init(|| regex::Regex::new(r"^[\s\-*•·\d.)>]+").unwrap());
    let meta_re = META_RE.get_or_init(|| {
        regex::RegexBuilder::new(
            r"^(idea lines|here|below|based on|i|no|note|first|second|third|next|the following|these|this (is|repo|project)|propose)\b",
        )
        .case_insensitive(true)
        .build()
        .unwrap()
    });

    let mut tiered: Vec<(i64, String, String)> = Vec::new();
    let mut plain: Vec<(i64, String, String)> = Vec::new();
    for ln in text.split('\n') {
        let s = ln.trim();
        if let Some(m) = tier_re.captures(s) {
            let body = m.get(3).map(|g| g.as_str().trim()).unwrap_or("");
            if body.chars().count() > 8 {
                // lev = min(5, max(1, int(group2))) if group2 else 3
                let lev = match m.get(2) {
                    Some(g) => {
                        // int(group2): \d+ -> a non-negative integer. Python ints are unbounded, so a
                        // huge value clamps to 5; an i64 parse-overflow on such input means "very large"
                        // -> i64::MAX, which then clamps to 5 (matching Python's min(5, max(1, n))).
                        let n: i64 = g.as_str().parse().unwrap_or(i64::MAX);
                        5.min(1.max(n))
                    }
                    None => 3,
                };
                let tier = m.get(1).unwrap().as_str().to_lowercase();
                // group(3).strip().rstrip("`").strip()
                let idea = body.trim_end_matches('`').trim().to_string();
                tiered.push((lev, tier, idea));
                continue;
            }
        }
        // fallback: re.sub(r"^[\s\-*•·\d.)>]+", "", s).strip().rstrip("`").strip()
        let body = bullet_re.replace(s, "");
        let body = body.trim().trim_end_matches('`').trim();
        let lower = body.to_lowercase();
        if body.chars().count() > 30
            && body.contains(' ')
            && !lower.starts_with('#')
            && !lower.starts_with("[chore]")
            && !body.ends_with(':')
            && !meta_re.is_match(body)
        {
            plain.push((3, "feature".to_string(), body.to_string()));
        }
    }
    // ideas = tiered or plain
    let mut ideas = if !tiered.is_empty() { tiered } else { plain };
    // ideas.sort(key=lambda t: -t[0])  — Python's sort is STABLE; sort_by_key with Reverse is stable.
    ideas.sort_by_key(|t| std::cmp::Reverse(t.0));
    ideas
}

/// run_improver._ideate_research_enabled (~3157-3165): True if THIS repo declared ideate_research:true
/// in repos.json (read fresh). False when absent — bool(row.get("ideate_research")).
fn ideate_research_enabled(ctx: &Ctx, name: &str) -> bool {
    py_bool(crate::improver::gitops::repo_row(ctx, name).get("ideate_research"))
}

/// Python bool() truthiness for a serde_json value (None/false/0/""/[]/{} -> false).
fn py_bool(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// run_improver._ideate_task (~3168-3188): the ideate lane's pi task text — base prompt + optional
/// research_line (when ideate_research on) + goal_line (north-star or the no-goal fallback).
fn ideate_task(ctx: &Ctx) -> String {
    let goal_line = if !ctx.goal.is_empty() {
        format!(
            "\n\nNORTH-STAR GOAL (rank every idea by how much it advances THIS):\n{}\n",
            ctx.goal
        )
    } else {
        "\n\n(No north-star goal set — propose the highest-leverage improvements \
toward making this project excellent at what it's for.)\n"
            .to_string()
    };
    let research_line = if ideate_research_enabled(ctx, &ctx.name) {
        "\n\nEXTERNAL RESEARCH ALLOWED (ideate_research is on): you MAY look up new ideas on the \
internet — search the web, read docs, find papers or techniques the project doesn't yet \
use — to escape the local minimum and propose genuinely novel, high-leverage moves. This \
is OPTIONAL, not required; ground every idea in the real code you read AND the external \
research. Your output is still REVIEWABLE backlog items — the runner sorts and prepends \
them; you NEVER edit the backlog file yourself (menu curation stays human-owned).\n"
            .to_string()
    } else {
        String::new()
    };
    format!(
        "Read this repository and propose its next batch of ambitious, high-leverage improvements \
per ideate.md. Output ONLY the idea lines.{research_line}{goal_line}"
    )
}

/// run_improver.ideate (~3191-3245): one-shot divergent pass — pi proposes ambitious, leverage-ranked,
/// tier-tagged improvements; the runner PREPENDS them (highest-leverage first) to the backlog as
/// `- [ ] [tier] ...` items, dropping near-duplicates via the novelty filter (against backlog +
/// last-30 history summaries + LESSONS lesson lines). No git, no gate, no loop. Prints one JSON line.
pub fn ideate(ctx: &mut Ctx) -> i32 {
    let task = ideate_task(ctx);
    let ideate_md = ctx.ideate_md.clone();
    let p = pi::run_pi(ctx, &task, 600, Some(&ideate_md));
    if p.code == 124 {
        // except subprocess.TimeoutExpired
        println!("{}", json!({"ok": false, "error": "ideate timed out"}));
        return 5;
    }
    let raw = pi::final_text(&p.stdout);
    let ideas = parse_ideas(&raw);
    if ideas.is_empty() {
        // log(f"ideate: no parseable ideas. Raw agent output (first 800 chars):\n{raw[:800]}")
        let head: String = raw.chars().take(800).collect();
        ctx.log(&format!(
            "ideate: no parseable ideas. Raw agent output (first 800 chars):\n{head}"
        ));
        println!(
            "{}",
            json!({"ok": false, "error": "ideate emitted no parseable ideas"})
        );
        return 5;
    }
    // existing = BACKLOG.read_text() if exists else "# backlog\n"
    let existing = match std::fs::read_to_string(&ctx.backlog) {
        Ok(c) => c,
        Err(_) => "# backlog\n".to_string(),
    };
    // corpus = backlog "- " lines + last-30 history summaries + LESSONS "- " lines
    let mut corpus: Vec<String> = existing
        .split('\n')
        .map(|l| l.trim())
        .filter(|l| l.starts_with("- "))
        .map(|l| l.to_string())
        .collect();
    corpus.extend(recent_history_summaries(ctx, 30));
    corpus.extend(
        read_lessons(ctx)
            .split('\n')
            .map(|l| l.trim())
            .filter(|l| l.starts_with("- "))
            .map(|l| l.to_string()),
    );

    let mut fresh: Vec<(i64, String, String)> = Vec::new();
    let mut dropped = 0i64;
    for (lev, tier, idea) in ideas {
        if is_novel(&idea, &corpus, 0.6) {
            corpus.push(idea.clone()); // so two near-identical candidates in one batch also dedupe
            fresh.push((lev, tier, idea));
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        ctx.log(&format!(
            "ideate: dropped {dropped} near-duplicate idea(s) (novelty filter); {} fresh",
            fresh.len()
        ));
    }
    if fresh.is_empty() {
        ctx.log("ideate: every candidate was a near-duplicate of the backlog/history/lessons — nothing fresh");
        println!(
            "{}",
            json!({"ok": false, "error": "ideate emitted only near-duplicate ideas"})
        );
        return 5;
    }
    // new_lines = [f"- [ ] [{tier}] {idea}" for ...]
    let new_lines: Vec<String> = fresh
        .iter()
        .map(|(_lev, tier, idea)| format!("- [ ] [{tier}] {idea}"))
        .collect();
    // prepend above the existing menu, after a header line if present
    let lines: Vec<&str> = existing.split('\n').collect();
    let head = if lines
        .first()
        .map(|l| l.trim_start().starts_with('#'))
        .unwrap_or(false)
    {
        1
    } else {
        0
    };
    // merged = lines[:head] + ([""] if head else []) + new_lines + lines[head:]
    let mut merged: Vec<String> = Vec::new();
    for l in &lines[..head] {
        merged.push(l.to_string());
    }
    if head > 0 {
        merged.push(String::new());
    }
    for l in &new_lines {
        merged.push(l.clone());
    }
    for l in &lines[head..] {
        merged.push(l.to_string());
    }
    // BACKLOG.parent.mkdir(...); write_text("\n".join(merged).rstrip() + "\n")
    if let Some(parent) = ctx.backlog.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            println!("{}", json!({"ok": false, "error": e.to_string()}));
            return 5;
        }
    }
    let body = format!("{}\n", py_rstrip(&merged.join("\n")));
    if let Err(e) = std::fs::write(&ctx.backlog, body) {
        println!("{}", json!({"ok": false, "error": e.to_string()}));
        return 5;
    }
    // {"ok": true, "added": len(new_lines), "top": new_lines[0][:90] if new_lines else ""}
    let top = new_lines
        .first()
        .map(|l| l.chars().take(90).collect::<String>())
        .unwrap_or_default();
    println!(
        "{}",
        json!({"ok": true, "added": new_lines.len(), "top": top})
    );
    0
}

// --------------------------------------------------------------------------- #
// small helpers (Python-string semantics)
// --------------------------------------------------------------------------- #

/// len(s) in Python is the count of Unicode code points (chars), NOT bytes — matters for the
/// agent_written/backlog_written byte-vs-char counts when the contract has non-ASCII glyphs.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// str.rstrip() with no args: strip trailing ASCII+Unicode whitespace.
fn py_rstrip(s: &str) -> String {
    s.trim_end().to_string()
}
