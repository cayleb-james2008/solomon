//! CEO autonomy, piece 2 — TOOLSET SELF-EXTENSION: propose -> lint -> register (inert) ->
//! operator-approved dry-run validation -> invoke, every rung fail-closed, ALL execution
//! human-gated.
//!
//! ============================ WHAT THIS ADDS =============================
//! The operator's end goal names "control and add to its own toolset on the fly" as a CEO
//! capability. This module gives it real teeth WITHOUT weakening a single gate: the CEO plane can
//! autonomously author a small PowerShell helper, statically LINT it against a deny-list (a cheap
//! PRE-FILTER — dangerous verbs are rejected BEFORE the script ever touches disk), and REGISTER
//! it (script + manifest entry) under gitignored `runtime\_tools\` WITHOUT EXECUTING A BYTE OF IT
//! — not even `-DryRun`, not even a parse check. Model-authored code never auto-executes: the
//! entry lands with `validation: "pending_operator"` + `dry_run: {passed:false, pending:true}`,
//! and the sandboxed mandatory `-DryRun` self-test runs only after an operator hand-sets
//! `approved_validation: true` on the entry (a later sweep's [`maybe_validate_approved_tools`]
//! performs it, records pass/fail, and clears `pending`). A validated tool is auto-invocable by
//! the tick ONLY in `-DryRun` (exactly what validation proved safe); a LIVE invocation requires
//! the additional operator-set `approved: true` in the manifest — absent that,
//! `invoke_tool(live=true)` DEGRADES to a dry-run and says so (never a silent live effect).
//! `unregister_tool` is the built-in per-tool rollback.
//! ========================================================================
//!
//! ## Location decision (why the provenance tripwire cannot break)
//! Tools + manifest + audit live under `<HERE>\runtime\_tools\` — gitignored (`runtime/` blanket
//! ignore), Solomon-owned, per-machine, janitor-bounded. `tools_manifest.json` is NOT a watched
//! file and is never git-committed: auto-committing agent-authored executables would be exactly
//! the "unattributed ungated write path" `provenance.rs` exists to stop. `provenance::TRACKED`
//! stays `repos.json`/`ops.json`/`actions.json` — a `#[test]` here pins that no `_tools` path
//! ever enters that set. Promoting a tool to permanent tracked `tools\` is an explicit MANUAL
//! operator step (copy + `operator:` commit), out of scope by design.
//!
//! ## The deny-list linter (static, fail-closed — a cheap PRE-FILTER, not the gate)
//! ANY match rejects, with the offending markers listed: file mutation outside `runtime\`/temp,
//! network sends/fetches (+ the `research::EXTERNAL_MUTATION_MARKERS` set, reused so the two deny
//! sources cannot diverge), process/service kills, background-process creation (schtasks /
//! scheduled tasks / Start-Job / services — the no-daemons doctrine), eval/policy/privilege
//! escalation, and the OBFUSCATION family a substring scan cannot see: `-EncodedCommand` /
//! `Invoke-Command`, the call operator `&` / dot-source `.` applied to anything other than a bare
//! literal command name or a quoted literal string (expression building — `& ('Stop-Proc'+'ess')`
//! assembles a denied verb out of innocent fragments), and backtick escapes outside strings
//! (`` Sto`p-Process `` spells a denied verb without ever containing it). A denied tool is never
//! written to disk, never registered; the rejection is audited. The linter is deliberately
//! over-broad (substring + word-boundary + shape heuristics): a false positive costs one refused
//! proposal, a false negative could cost a real side effect — fail-closed is the correct
//! direction. But a token linter over a full scripting language is BYPASSABLE BY CONSTRUCTION,
//! which is why it is only the pre-filter: the actual execution gate is the operator-approved
//! sandboxed dry-run — nothing model-authored runs until a human clears it.
//!
//! ## Tamper evidence
//! The manifest stores `sha256` of the exact registered bytes (the vendored `provenance` hash —
//! one hash source, never a second copy). `invoke_tool` recomputes the on-disk hash and REFUSES
//! on mismatch: a hand-edited tool body must be re-registered, never silently trusted.
#![allow(dead_code)]

use crate::control::{paths, proc};
use crate::notify::{self, Notice};
use chrono::Utc;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

// --------------------------------------------------------------------------- //
// constants + paths (all under runtime/_tools/ — gitignored, never tracked)
// --------------------------------------------------------------------------- //

/// Parse-check wall clock (a syntax check must be near-instant).
const PARSE_TIMEOUT_S: u64 = 20;
/// Mandatory sandboxed `-DryRun` self-test wall clock.
const DRYRUN_TIMEOUT_S: u64 = 30;
/// Registered-tool invocation wall clock (dry-run or live).
const INVOKE_TIMEOUT_S: u64 = 120;
/// Invocation-history ring per manifest entry (bounded state, janitor-free).
const MAX_INVOCATIONS_KEPT: usize = 50;

/// The tool author's role contract — constraints stated as a HINT; the linter is the enforcement.
const TOOL_AUTHOR_PROMPT: &str = "You author ONE small PowerShell helper tool for a Windows \
    desktop orchestrator. You are given the need (a planner directive). HARD CONTRACT: the script \
    MUST declare [CmdletBinding()] param([switch]$DryRun) and, when -DryRun is passed, perform NO \
    side effect and exit 0. It may only write under the relative 'runtime/' directory. FORBIDDEN \
    (statically rejected): deleting or writing files outside runtime/, any network access or \
    send, killing processes or services, creating scheduled tasks / services / background jobs, \
    Invoke-Expression, Invoke-Command, -EncodedCommand, execution-policy or registry or privilege \
    changes, backtick escapes, and the call operator '&' or dot-sourcing applied to anything but a \
    literal command name. Keep it under 60 lines. \
    Reply STRICT JSON only: {\"name\":\"<kebab-or-snake short name>\",\"purpose\":\"<one \
    line>\",\"script\":\"<the full PowerShell script>\"}";

/// `runtime/_tools/` — the self-authored toolbox root.
fn tools_dir() -> PathBuf {
    paths::here().join("runtime").join("_tools")
}

/// The manifest path — the SAME helper the approvals surface reads (single source, no drift).
pub(crate) fn manifest_path() -> PathBuf {
    crate::ceo::approvals::tools_manifest_path()
}

/// `runtime/_tools/<name>.ps1` — the registered script body (written only after lint + dry-run).
fn script_path(name: &str) -> PathBuf {
    tools_dir().join(format!("{name}.ps1"))
}

/// `runtime/_tools/_audit.jsonl` — the append-only propose/lint/dry-run/register/invoke ledger.
fn audit_path() -> PathBuf {
    tools_dir().join("_audit.jsonl")
}

/// `runtime/_tools/sandbox/` — throwaway cwd for dry-run invocations.
fn sandbox_dir() -> PathBuf {
    tools_dir().join("sandbox")
}

/// `runtime/_tools/_proposed_<date>` — the per-day propose stamp (atomic `create_new` claim).
fn proposed_stamp_path(date: &str) -> PathBuf {
    tools_dir().join(format!("_proposed_{date}"))
}

fn iso_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

// --------------------------------------------------------------------------- //
// the deny-list linter (pure, fail-closed, runs FIRST — before any subprocess)
// --------------------------------------------------------------------------- //

/// Network sends / fetches (case-insensitive substring).
const NETWORK_MARKERS: &[&str] = &[
    "invoke-webrequest",
    "invoke-restmethod",
    "net.webclient",
    "system.net",
    "start-bitstransfer",
    "send-mailmessage",
    "smtp",
    "http://",
    "https://",
];

/// Process / service kills (substring).
const PROCESS_MARKERS: &[&str] = &[
    "stop-process",
    "taskkill",
    "stop-service",
    "restart-service",
    "stop-computer",
    "restart-computer",
];

/// Background-process creation — the no-daemons doctrine (substring). `start-process` is denied
/// OUTRIGHT (stricter than the spec's `-WindowStyle`-only form): a helper tool spawning arbitrary
/// children is exactly the surface the doctrine exists to close.
const DAEMON_MARKERS: &[&str] = &[
    "schtasks",
    "new-scheduledtask",
    "register-scheduledtask",
    "start-job",
    "new-service",
    "sc.exe",
    "start-process",
];

/// Arbitrary eval / policy escalation / privilege / registry writes (substring).
const PRIVILEGE_MARKERS: &[&str] = &[
    "invoke-expression",
    "set-executionpolicy",
    "add-mppreference",
    "set-itemproperty",
    "new-itemproperty",
    "reg add",
    "reg.exe",
    "hklm:",
    "hkcu:",
    "runas",
];

/// Filesystem-mutating verbs (substring) — allowed ONLY when every path-like quoted literal in
/// the script is under an allowed runtime/temp prefix (see [`path_allowed`]); no path literal at
/// all + a mutating verb = reject (fail-closed).
const MUTATING_FS_MARKERS: &[&str] = &[
    "remove-item",
    "remove-itemproperty",
    "clear-content",
    "format-volume",
    "clear-disk",
    "set-content",
    "out-file",
    "add-content",
    "new-item",
    "move-item",
    "copy-item",
    "rename-item",
];

/// Short dangerous aliases, WORD-BOUNDARY matched (a substring scan on "rm"/"del"/"irm" would trip
/// on "form"/"deleted"/"confirm"). Denied OUTRIGHT — an alias is an obfuscation-shaped spelling of
/// an already-denied verb, so no path allowance applies (stricter than the spec table, deliberate).
const DENY_TOKENS: &[&str] =
    &["rm", "del", "rmdir", "iwr", "irm", "curl", "wget", "kill", "iex", "icm"];

/// Execution-obfuscation surfaces (case-insensitive substring): `-EncodedCommand` smuggles a
/// base64 payload past every text marker; `Invoke-Command` runs script blocks (locally or
/// remotely) out of band. Denied OUTRIGHT.
const OBFUSCATION_MARKERS: &[&str] = &["-encodedcommand", "invoke-command"];

/// True iff `lower` contains `tok` as a standalone alphanumeric word (pure).
fn has_token(lower: &str, tok: &str) -> bool {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == tok)
}

/// Push `h` once (the scanner can trip the same shape many times; one audit marker per family
/// keeps the hit list readable). Pure.
fn push_unique(hits: &mut Vec<String>, h: &str) {
    if !hits.iter().any(|x| x == h) {
        hits.push(h.to_string());
    }
}

/// True iff `prev` (the previous significant char) legitimately opens a COMMAND POSITION — start
/// of script/line, statement separator, block/group opener, or pipe. Pure.
fn starts_command_position(prev: Option<char>) -> bool {
    matches!(prev, None | Some('\n') | Some(';') | Some('{') | Some('(') | Some('|'))
}

/// True iff what follows index `j` (after `&` or dot-source `.`) is an admissible LITERAL
/// invocation target: a bare literal command name (letter-first, then letters/digits/`-`/`_`,
/// ending at a clean boundary) or a quoted LITERAL string (single-quoted, or double-quoted with
/// no `$`/backtick — an interpolated "$x" builds a name at runtime). Anything else — `(`, `$`,
/// `{`, `[`, concatenation — is expression building and is rejected. The admitted literal's TEXT
/// is still subject to every substring marker family. Pure.
fn literal_invocation_follows(cs: &[char], mut j: usize) -> bool {
    while j < cs.len() && (cs[j] == ' ' || cs[j] == '\t') {
        j += 1;
    }
    match cs.get(j).copied() {
        Some(q) if q == '\'' || q == '"' => {
            j += 1;
            let mut dynamic = false;
            while j < cs.len() && cs[j] != q {
                if q == '"' && (cs[j] == '$' || cs[j] == '`') {
                    dynamic = true; // interpolation / subexpression / escape builds the name
                }
                j += 1;
            }
            j < cs.len() && !dynamic // an unterminated quote is not a literal
        }
        Some(c) if c.is_ascii_alphabetic() => {
            j += 1;
            while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '-' || cs[j] == '_') {
                j += 1;
            }
            // the name must END at a clean boundary — a trailing `(`/`.`/`+`/quote keeps building.
            match cs.get(j).copied() {
                None => true,
                Some(c2) => c2.is_whitespace() || matches!(c2, ';' | ')' | '}' | '|'),
            }
        }
        _ => false,
    }
}

/// Scan `script` for the obfuscated-invocation shapes the substring families cannot see: the call
/// operator `&` (or the dot-source `.` in command position) applied to anything other than a bare
/// literal command name / quoted literal string, and backtick escapes outside strings that are
/// not line continuations. Comments are skipped; quoted strings are opaque here (their contents
/// are already substring-linted). `2>&1` stream merges and `&&` chains are not call operators.
/// Pure; over-broad by design (a pre-filter, not the execution gate).
fn call_operator_hits(script: &str) -> Vec<String> {
    let cs: Vec<char> = script.chars().collect();
    let mut hits: Vec<String> = Vec::new();
    let mut prev: Option<char> = None; // previous significant char (' '/'\t' skipped, '\r' -> '\n')
    let mut i = 0usize;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\'' | '"' => {
                i += 1;
                while i < cs.len() && cs[i] != c {
                    i += 1;
                }
                i += 1; // past the closing quote (or end)
                prev = Some(c);
                continue;
            }
            '#' => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '`' => {
                let continuation =
                    matches!(cs.get(i + 1).copied(), Some('\n') | Some('\r')) || i + 1 == cs.len();
                if !continuation {
                    push_unique(&mut hits, "obfuscation:backtick-escape");
                }
            }
            '&' => {
                if cs.get(i + 1) == Some(&'&') {
                    i += 2; // `&&` pipeline chain — the next command is plain text, marker-scanned
                    prev = Some('&');
                    continue;
                }
                // `>&`/`<&` stream merges are redirections, not invocations.
                let merge = matches!(prev, Some('>') | Some('<'));
                if !merge && !literal_invocation_follows(&cs, i + 1) {
                    push_unique(&mut hits, "obfuscation:call-operator-nonliteral");
                }
            }
            '.' => {
                // dot-source: `.` in command position followed by whitespace (`.5` / `.Trim()` /
                // `a.b` are not — no whitespace, or not command position).
                let dot_source = matches!(cs.get(i + 1).copied(), Some(' ') | Some('\t'));
                if starts_command_position(prev) && dot_source && !literal_invocation_follows(&cs, i + 1) {
                    push_unique(&mut hits, "obfuscation:dot-source-nonliteral");
                }
            }
            _ => {}
        }
        if c == '\n' || c == '\r' {
            prev = Some('\n');
        } else if c != ' ' && c != '\t' {
            prev = Some(c);
        }
        i += 1;
    }
    hits
}

/// Every '...'/"..." quoted literal in the script (naive scan — good enough for a deny linter:
/// an unquoted mutating path simply means "no allowed path literal", which rejects). Pure.
fn quoted_literals(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = script.chars();
    while let Some(c) = chars.next() {
        if c == '\'' || c == '"' {
            let quote = c;
            let mut lit = String::new();
            for c2 in chars.by_ref() {
                if c2 == quote {
                    break;
                }
                lit.push(c2);
            }
            out.push(lit);
        }
    }
    out
}

/// True iff a quoted literal LOOKS like a filesystem path (pure).
fn is_path_like(lit: &str) -> bool {
    lit.contains('/') || lit.contains('\\') || lit.to_ascii_lowercase().starts_with("$env:temp")
}

/// True iff a path-like literal is under an ALLOWED write root: the relative `runtime/` tree,
/// `$env:TEMP`, or the dry-run sandbox. Anything else (absolute C:\, UNC, ..) is disallowed. Pure.
fn path_allowed(lit: &str) -> bool {
    let l = lit.to_ascii_lowercase().replace('\\', "/");
    l == "runtime"
        || l.starts_with("runtime/")
        || l.starts_with("./runtime/")
        || l.starts_with("$env:temp")
        || l.contains("/_tools/sandbox")
}

/// The static deny-list linter (pure — a cheap fail-closed PRE-FILTER, run FIRST; the execution
/// gate is the operator-approved dry-run). `Ok(())` admits the script to registration; `Err`
/// carries EVERY offending marker for the audit trail.
pub(crate) fn lint_tool(script: &str) -> Result<(), Vec<String>> {
    let lower = script.to_ascii_lowercase();
    let mut hits: Vec<String> = Vec::new();
    for m in NETWORK_MARKERS {
        if lower.contains(m) {
            hits.push(format!("network:{m}"));
        }
    }
    // Reuse the research deny set so the two external-mutation sources can never diverge.
    for m in crate::ceo::research::EXTERNAL_MUTATION_MARKERS {
        if lower.contains(m) {
            hits.push(format!("external:{m}"));
        }
    }
    for m in PROCESS_MARKERS {
        if lower.contains(m) {
            hits.push(format!("process:{m}"));
        }
    }
    for m in DAEMON_MARKERS {
        if lower.contains(m) {
            hits.push(format!("daemon:{m}"));
        }
    }
    for m in PRIVILEGE_MARKERS {
        if lower.contains(m) {
            hits.push(format!("privilege:{m}"));
        }
    }
    for t in DENY_TOKENS {
        if has_token(&lower, t) {
            hits.push(format!("token:{t}"));
        }
    }
    for m in OBFUSCATION_MARKERS {
        if lower.contains(m) {
            hits.push(format!("obfuscation:{m}"));
        }
    }
    // Invocation-shape scan: `&`/`.` on non-literal targets + backtick escapes (obfuscation that
    // assembles denied verbs at runtime, invisible to every substring family above).
    hits.extend(call_operator_hits(script));
    // Filesystem mutation: the verbs (and any `>` redirection) are admissible ONLY when the
    // script's path-like literals exist and are ALL under the allowed runtime/temp roots.
    let mut fs_triggers: Vec<String> = MUTATING_FS_MARKERS
        .iter()
        .filter(|m| lower.contains(**m))
        .map(|m| m.to_string())
        .collect();
    if lower.contains('>') {
        fs_triggers.push("redirection:>".to_string());
    }
    if !fs_triggers.is_empty() {
        let path_lits: Vec<String> = quoted_literals(script)
            .into_iter()
            .filter(|l| is_path_like(l))
            .collect();
        let all_allowed = !path_lits.is_empty() && path_lits.iter().all(|l| path_allowed(l));
        if !all_allowed {
            for t in fs_triggers {
                hits.push(format!("fs-outside-runtime:{t}"));
            }
        }
    }
    if hits.is_empty() {
        Ok(())
    } else {
        Err(hits)
    }
}

/// Tool names become filenames + manifest keys: short, lowercase kebab/snake, letter-first — a
/// traversal-shaped or exotic name is refused before any path is built (pure).
pub(crate) fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// True iff a backlog line reads as a TOOLING directive (keyword heuristic — the outreach/growth
/// risk posture: a false positive is at worst one linted, dry-run-gated local registration). Pure.
pub(crate) fn is_tool_directive(line: &str) -> bool {
    let lower = line.to_lowercase();
    ["tool", "helper", "script", "automate", "automation", "capability"]
        .iter()
        .any(|k| lower.contains(k))
}

// --------------------------------------------------------------------------- //
// manifest + audit IO
// --------------------------------------------------------------------------- //

/// Read the manifest; absent/corrupt reads as EMPTY (fail-open toward "no tools registered" —
/// a malformed entry is skipped, never trusted; edge E16).
fn read_manifest() -> Value {
    std::fs::read(manifest_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|v: &Value| v.get("tools").map(|t| t.is_object()).unwrap_or(false))
        .unwrap_or_else(|| json!({"version": 1, "tools": {}}))
}

/// Atomic manifest write (temp + rename). Returns false on failure (callers refuse to claim
/// success on a non-durable write).
fn write_manifest(m: &Value) -> bool {
    if let Some(parent) = manifest_path().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    proc::atomic_write_json(&manifest_path(), m).is_ok()
}

/// Append one audit record (append-only, OSError -> pass — the notify-log contract).
fn audit(event: &str, tool: &str, detail: &str) {
    let _ = (|| -> std::io::Result<()> {
        let p = audit_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        writeln!(
            f,
            "{}",
            serde_json::to_string(&json!({
                "ts": iso_now(), "event": event, "tool": tool,
                "detail": detail, "provenance": "rsi:",
            }))
            .unwrap_or_default()
        )?;
        Ok(())
    })();
}

// --------------------------------------------------------------------------- //
// dry-run validation (mandatory, sandboxed, windowless, runner-injected)
// --------------------------------------------------------------------------- //

/// The production subprocess runner: windowless, scrubbed env, bounded (`proc::run`).
fn real_runner(argv: &[String], cwd: Option<&Path>, timeout: Duration) -> std::io::Result<proc::RunOut> {
    proc::run(argv, cwd, Some(timeout))
}

/// Validate a linted script: (1) a PowerShell PARSE check (via `Parser::ParseFile` on the sandbox
/// copy — the script rides as a FILE, never on a command line), then (2) the mandatory `-DryRun`
/// self-test with cwd = sandbox, bounded + windowless. CONTRACT: a valid tool declares
/// `param([switch]$DryRun)` and exits 0 under `-DryRun` with no side effect — a tool that ignores
/// the switch is caught by the linter (its dangerous verbs are already denied) and/or fails the
/// exit-0 contract here. Returns the manifest `dry_run` record on success.
pub(crate) fn dry_run_with<R>(name: &str, script: &str, runner: &R) -> Result<Value, String>
where
    R: Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut>,
{
    let sb = sandbox_dir();
    std::fs::create_dir_all(&sb).map_err(|e| format!("sandbox: {e}"))?;
    let sp = sb.join(format!("{name}.ps1"));
    std::fs::write(&sp, script.as_bytes()).map_err(|e| format!("sandbox write: {e}"))?;
    // (1) parse check — syntax errors exit 2 before anything runs.
    let sp_quoted = sp.to_string_lossy().replace('\'', "''");
    let parse_cmd = format!(
        "$e=$null;[void][System.Management.Automation.Language.Parser]::ParseFile('{sp_quoted}',\
         [ref]$null,[ref]$e); if($e -and $e.Count){{exit 2}} else {{exit 0}}"
    );
    let argv: Vec<String> = vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-Command".into(),
        parse_cmd,
    ];
    match runner(&argv, None, Duration::from_secs(PARSE_TIMEOUT_S)) {
        Ok(out) if out.code == 0 => {}
        Ok(out) => {
            return Err(format!("parse: exit {} {}", out.code, super::cap_line(&out.stderr, 160)))
        }
        Err(e) => return Err(format!("parse: {e}")),
    }
    // (2) the mandatory -DryRun self-test, sandbox cwd.
    let argv: Vec<String> = vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-File".into(),
        sp.to_string_lossy().into_owned(),
        "-DryRun".into(),
    ];
    match runner(&argv, Some(&sb), Duration::from_secs(DRYRUN_TIMEOUT_S)) {
        Ok(out) if out.code == 0 => Ok(json!({
            "passed": true,
            "exit_code": 0,
            "ts": iso_now(),
            "summary": super::cap_line(&out.stdout, 240),
        })),
        Ok(out) => Err(format!(
            "dry_run: exit {} {}",
            out.code,
            super::cap_line(&format!("{} {}", out.stderr, out.stdout), 200)
        )),
        Err(e) => Err(format!("dry_run: {e}")),
    }
}

// --------------------------------------------------------------------------- //
// register / invoke / unregister (the gated pipeline)
// --------------------------------------------------------------------------- //

/// One authored tool proposal (name + one-line purpose + the full script body).
pub struct ToolProposal {
    pub name: String,
    pub purpose: String,
    pub script: String,
}

/// Register a proposal through the PRE-FILTER gate: name check -> LINT (static, before the script
/// ever touches disk) -> `rsi:` provenance stamp + sha256 -> script + manifest write. REGISTRATION
/// NEVER EXECUTES THE SCRIPT — no `-DryRun`, no parse check, no subprocess of any kind:
/// model-authored code runs only after an operator hand-sets `approved_validation: true` on the
/// entry (see [`maybe_validate_approved_tools`]), and then only inside the sandboxed dry-run. The
/// entry lands with `validation: "pending_operator"`, `dry_run: {passed:false, pending:true}`,
/// `approved_validation: false`, and `approved: false` — nothing in Solomon ever sets either
/// approval flag true. ANY failing rung registers NOTHING and audits the named rejection.
/// Returns `{ok, tool, reason?}`.
pub fn register_tool(p: &ToolProposal, model: &str, prompt_sha8: &str) -> Value {
    register_tool_with(p, model, prompt_sha8, &real_runner)
}

/// Core of [`register_tool`] with the subprocess RUNNER injected. The runner is deliberately
/// UNUSED: keeping the seam lets the `#[test]`s inject a panicking runner and PROVE registration
/// executes nothing (the no-auto-execution contract, pinned forever).
pub(crate) fn register_tool_with<R>(
    p: &ToolProposal,
    model: &str,
    prompt_sha8: &str,
    _runner: &R,
) -> Value
where
    R: Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut>,
{
    if !valid_tool_name(&p.name) {
        audit("lint_reject", &p.name, "invalid tool name");
        return json!({"ok": false, "tool": p.name, "reason": "invalid tool name"});
    }
    // (1) LINT FIRST — a denied script never touches disk, never spawns anything (edge E12).
    if let Err(markers) = lint_tool(&p.script) {
        audit("lint_reject", &p.name, &markers.join(", "));
        return json!({"ok": false, "tool": p.name, "reason": "lint", "denied_markers": markers});
    }
    // (2) provenance stamp + tamper hash, then script + manifest — INERT: validation is
    //     pending_operator, dry_run is pending, both approval flags start FALSE.
    let sha256 = crate::provenance::sha256_hex(p.script.as_bytes());
    let spath = script_path(&p.name);
    if let Some(parent) = spath.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if proc::atomic_write_bytes(&spath, p.script.as_bytes()).is_err() {
        audit("register_fail", &p.name, "script write failed");
        return json!({"ok": false, "tool": p.name, "reason": "script write failed"});
    }
    let mut manifest = read_manifest();
    let version = manifest
        .pointer(&format!("/tools/{}/version", p.name))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        + 1;
    manifest["tools"][&p.name] = json!({
        "name": p.name,
        "kind": "ps1",
        "path": format!("runtime/_tools/{}.ps1", p.name),
        "purpose": p.purpose,
        "version": version,
        "sha256": sha256,
        "provenance": "rsi:",
        "authored_model": model,
        "prompt_sha8": prompt_sha8,
        "registered_ts": iso_now(),
        "validation": "pending_operator",
        "dry_run": {"passed": false, "pending": true},
        "lint": {"passed": true, "denied_markers": []},
        "approved_validation": false,
        "approved": false,
        "invocations": [],
    });
    if !write_manifest(&manifest) {
        let _ = std::fs::remove_file(&spath); // no half-registration
        audit("register_fail", &p.name, "manifest write failed");
        return json!({"ok": false, "tool": p.name, "reason": "manifest write failed"});
    }
    audit(
        "register",
        &p.name,
        &format!("v{version} sha256={sha256} validation=pending_operator approved=false (NOT executed)"),
    );
    json!({"ok": true, "tool": p.name, "version": version})
}

// --------------------------------------------------------------------------- //
// operator-approved validation — the ONLY seam that ever executes model-authored code
// --------------------------------------------------------------------------- //

/// The every-sweep VALIDATION seam, ridden on `ceo_autonomy_seams` right after the tool-propose
/// seam (its own catch_unwind + `_seam_tool_validate` marker): find manifest entries the OPERATOR
/// has cleared for validation (`approved_validation: true` — hand-set; nothing in Solomon writes
/// it) that still carry `dry_run.pending`, run the existing sandboxed dry-run on each, record
/// pass/fail + audit, and clear `pending`. No LLM, no day gate — cheap when nothing is approved
/// (one manifest read).
pub fn maybe_validate_approved_tools(_snapshot: &Value, _status: &Value) {
    super::seam_marker("tool_validate");
    let _ = validate_approved_tools_with(&real_runner);
}

/// Core of [`maybe_validate_approved_tools`] with the runner injected. Returns one
/// `{tool, passed}` record per entry validated THIS sweep (empty when nothing was approved —
/// unapproved entries are never touched, let alone executed).
pub(crate) fn validate_approved_tools_with<R>(runner: &R) -> Vec<Value>
where
    R: Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut>,
{
    let mut manifest = read_manifest();
    let names: Vec<String> = manifest
        .get("tools")
        .and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .filter(|(_, e)| {
                    e.get("approved_validation").and_then(Value::as_bool) == Some(true)
                        && e.pointer("/dry_run/pending").and_then(Value::as_bool) == Some(true)
                })
                .map(|(n, _)| n.clone())
                .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    for name in names {
        let entry = manifest.pointer(&format!("/tools/{name}")).cloned().unwrap_or(Value::Null);
        let (passed, dry_run, note) = match validate_one(&name, &entry, runner) {
            Ok(rec) => (true, rec, "sandboxed dry-run passed".to_string()),
            Err(e) => (
                false,
                json!({
                    "passed": false,
                    "pending": false,
                    "error": super::cap_line(&e, 200),
                    "ts": iso_now(),
                }),
                e,
            ),
        };
        // `pending` clears either way — a failed validation is a RECORDED verdict, not a retry
        // loop; the operator re-registers (or re-approves a fresh registration) to try again.
        manifest["tools"][&name]["dry_run"] = dry_run;
        manifest["tools"][&name]["validation"] = json!(if passed { "validated" } else { "failed" });
        audit(if passed { "validate_pass" } else { "validate_fail" }, &name, &note);
        out.push(json!({"tool": name, "passed": passed}));
    }
    if !out.is_empty() {
        let _ = write_manifest(&manifest);
    }
    out
}

/// One approved entry's validation: the on-disk bytes must still hash to the registered `sha256`
/// (the operator approved THOSE bytes — a body edited since registration is refused, re-register
/// it), then re-LINT (the linter may have grown teeth since registration), then the existing
/// sandboxed parse + `-DryRun` self-test. Returns the manifest `dry_run` record on success.
fn validate_one<R>(name: &str, entry: &Value, runner: &R) -> Result<Value, String>
where
    R: Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut>,
{
    let bytes = std::fs::read(script_path(name))
        .map_err(|_| "script missing on disk — re-register".to_string())?;
    let disk_sha = crate::provenance::sha256_hex(&bytes);
    if entry.get("sha256").and_then(Value::as_str) != Some(disk_sha.as_str()) {
        return Err("sha256 mismatch (body changed since registration — re-register)".to_string());
    }
    let script = String::from_utf8_lossy(&bytes).into_owned();
    if let Err(markers) = lint_tool(&script) {
        return Err(format!("lint: {}", markers.join(", ")));
    }
    dry_run_with(name, &script, runner)
}

/// Invoke a registered tool. FAIL-CLOSED ladder: the manifest entry must exist with
/// `lint.passed` + `dry_run.passed` + `provenance == "rsi:"`; the on-disk script must hash to the
/// registered `sha256` (edge E14 — a hand-edited body is refused, re-register it); and a LIVE run
/// additionally requires the operator-set `approved: true` — else the call DEGRADES to `-DryRun`
/// and reports it (edge E15, never a silent live effect). Windowless, scrubbed env, bounded.
pub fn invoke_tool(name: &str, live: bool, args: &[String]) -> Value {
    invoke_tool_with(name, live, args, &real_runner)
}

/// Core of [`invoke_tool`] with the runner injected.
pub(crate) fn invoke_tool_with<R>(name: &str, live: bool, args: &[String], runner: &R) -> Value
where
    R: Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut>,
{
    let mut manifest = read_manifest();
    let Some(entry) = manifest.pointer(&format!("/tools/{name}")).cloned() else {
        return json!({"ok": false, "tool": name, "reason": "not_found"});
    };
    let lint_ok = entry.pointer("/lint/passed").and_then(Value::as_bool) == Some(true);
    let dry_ok = entry.pointer("/dry_run/passed").and_then(Value::as_bool) == Some(true);
    let prov_ok = entry.get("provenance").and_then(Value::as_str) == Some("rsi:");
    if !(lint_ok && dry_ok && prov_ok) {
        audit("invoke", name, "refused: unvalidated manifest entry");
        return json!({"ok": false, "tool": name, "reason": "unvalidated manifest entry"});
    }
    // Tamper check: the CANONICAL script path (never a manifest-supplied path — no traversal).
    let spath = script_path(name);
    let bytes = match std::fs::read(&spath) {
        Ok(b) => b,
        Err(_) => {
            audit("invoke", name, "refused: script missing on disk");
            return json!({"ok": false, "tool": name, "reason": "script missing"});
        }
    };
    let disk_sha = crate::provenance::sha256_hex(&bytes);
    if entry.get("sha256").and_then(Value::as_str) != Some(disk_sha.as_str()) {
        audit("invoke", name, "refused: sha256 mismatch (hand-edited body — re-register it)");
        return json!({"ok": false, "tool": name, "reason": "sha256_mismatch"});
    }
    let approved = entry.get("approved").and_then(Value::as_bool) == Some(true);
    let (mode, degraded) = if live && approved {
        ("live", false)
    } else {
        ("dry_run", live) // a live request without approval DEGRADES, loudly flagged
    };
    let mut argv: Vec<String> = vec![
        "powershell".into(),
        "-NoProfile".into(),
        "-NonInteractive".into(),
        "-ExecutionPolicy".into(),
        "Bypass".into(),
        "-File".into(),
        spath.to_string_lossy().into_owned(),
    ];
    argv.extend(args.iter().cloned());
    if mode == "dry_run" {
        argv.push("-DryRun".into());
    }
    let cwd = if mode == "dry_run" { sandbox_dir() } else { tools_dir() };
    let _ = std::fs::create_dir_all(&cwd);
    let run = runner(&argv, Some(&cwd), Duration::from_secs(INVOKE_TIMEOUT_S));
    let (ok, exit_code, summary) = match &run {
        Ok(out) => (out.code == 0, out.code, super::cap_line(&out.stdout, 240)),
        Err(e) => (false, -1, format!("spawn: {e}")),
    };
    // Record the invocation (bounded ring) + audit.
    let mut invocations = entry
        .get("invocations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    invocations.push(json!({"ts": iso_now(), "mode": mode, "exit_code": exit_code}));
    if invocations.len() > MAX_INVOCATIONS_KEPT {
        let cut = invocations.len() - MAX_INVOCATIONS_KEPT;
        invocations.drain(..cut);
    }
    manifest["tools"][name]["invocations"] = json!(invocations);
    let _ = write_manifest(&manifest);
    audit("invoke", name, &format!("mode={mode} degraded={degraded} exit={exit_code}"));
    json!({
        "ok": ok, "tool": name, "mode": mode, "degraded": degraded,
        "exit_code": exit_code, "summary": summary,
    })
}

/// Rollback one registered tool: remove the manifest entry + the script body, audited. A
/// subsequent invoke reports `not_found`. `{ok:false}` when the tool was never registered.
pub fn unregister_tool(name: &str) -> Value {
    let mut manifest = read_manifest();
    let existed = manifest
        .get_mut("tools")
        .and_then(Value::as_object_mut)
        .map(|t| t.remove(name).is_some())
        .unwrap_or(false);
    if !existed {
        return json!({"ok": false, "tool": name, "reason": "not_found"});
    }
    let _ = write_manifest(&manifest);
    let _ = std::fs::remove_file(script_path(name));
    audit("unregister", name, "manifest entry + script removed (rollback)");
    json!({"ok": true, "tool": name})
}

// --------------------------------------------------------------------------- //
// the authoring seam — maybe_propose_tool (day-gated, honest-trigger, budget-aware)
// --------------------------------------------------------------------------- //

/// Find today's planner TOOLING directive across the lane backlogs (the outreach/growth honest
/// trigger shape: open, non-deferred, today's `(ceo <date>)` goal or an open `[campaign:` step,
/// reading as a tool need). None = a quiet day. A backlog read error skips that lane.
fn find_tool_need(today_marker: &str) -> Option<String> {
    for repo in crate::control::registry::read_repos_json() {
        let lane = paths::repo_name(&repo);
        if lane.is_empty() {
            continue;
        }
        let backlog = match std::fs::read_to_string(super::backlog_path(&lane)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let hit = backlog
            .lines()
            .map(str::trim)
            .find(|l| {
                l.starts_with("- [ ]")
                    && !l.contains("(deferred")
                    && (l.contains(today_marker) || l.contains("[campaign:"))
                    && is_tool_directive(l)
            })
            .map(|l| l.trim_start_matches("- [ ]").trim().to_string());
        if hit.is_some() {
            return hit;
        }
    }
    None
}

/// Atomically claim the day's propose attempt (`create_new` — exactly one OS process wins, the
/// growth stamp precedent, edge E18).
fn claim_proposed_stamp(date: &str) -> bool {
    use std::io::Write;
    (|| -> std::io::Result<()> {
        let p = proposed_stamp_path(date);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&p)?;
        f.write_all(format!("attempt {}", iso_now()).as_bytes())
    })()
    .is_ok()
}

/// Overwrite the day stamp with the named outcome (the stamp doubles as the skip log).
fn stamp_proposed(date: &str, note: &str) {
    let _ = std::fs::write(proposed_stamp_path(date), format!("{note} {}", iso_now()));
}

/// Author ONE tool proposal via the MoA brain (STRICT JSON `{name, purpose, script}`) — the same
/// model/skill resolution as the growth/outreach composers. Side-effectful (one LLM call);
/// deliberately not unit-tested (the `compose_and_dispatch` precedent) — everything around it is.
fn propose_via_brain(need: &str) -> Result<(ToolProposal, String, String), String> {
    let model = crate::ceo::growth::pick_growth_model(&crate::improver::brain::BrainConfig::from_autopilot());
    let skill = crate::improver::brain::load_skill("_tools", "author");
    let user = serde_json::to_string_pretty(&json!({"need": need})).unwrap_or_default();
    let reply = crate::improver::brain::spawn_worker(&model, TOOL_AUTHOR_PROMPT, &skill, &user, None)?;
    let parsed = super::extract_json(&reply).ok_or_else(|| "unparseable reply".to_string())?;
    let name = parsed
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    let purpose = super::cap_line(parsed.get("purpose").and_then(Value::as_str).unwrap_or(""), 160);
    let script = parsed.get("script").and_then(Value::as_str).unwrap_or("").to_string();
    if name.is_empty() || script.trim().is_empty() {
        return Err("empty name/script".to_string());
    }
    let prompt_sha8 = crate::provenance::sha256_hex(user.as_bytes())[..8].to_string();
    Ok((ToolProposal { name, purpose, script }, model, prompt_sha8))
}

/// The day-gated TOOL-PROPOSAL seam, ridden on `ceo_slow_tail`: at most ONE propose -> lint ->
/// register attempt per day, and only when today's planner output carries a tooling directive
/// (honest trigger) AND the fleet call budget has headroom. STAMP-FIRST (`create_new`) before the
/// LLM call — a hung/failed authoring consumes the day's attempt; two OS processes cannot
/// double-fire. Registration EXECUTES NOTHING and lands `validation: "pending_operator"` +
/// `approved: false` — both the sandboxed dry-run and live invocation stay human-gated.
pub fn maybe_propose_tool(_snapshot: &Value, _status: &Value) {
    super::seam_marker("tool_propose");
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    if proposed_stamp_path(&today).exists() {
        return; // day-gate: today's propose attempt is already consumed
    }
    let today_marker = super::ceo_marker(&today);
    let Some(need) = find_tool_need(&today_marker) else {
        return; // no tooling directive today — no fabricated busywork (no stamp burned)
    };
    if crate::fleet::daily_calls_remaining() <= 0 {
        return; // budget exhausted — retry when the date resets the counter (no stamp burned)
    }
    if !claim_proposed_stamp(&today) {
        return; // the other process claimed this attempt just now
    }
    audit("propose", "_pending", &super::cap_line(&need, 200));
    match propose_via_brain(&need) {
        Err(e) => stamp_proposed(&today, &format!("skip:llm_unavailable {}", super::cap_line(&e, 160))),
        Ok((proposal, model, prompt_sha8)) => {
            let out = register_tool(&proposal, &model, &prompt_sha8);
            let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
            stamp_proposed(&today, if ok { "ok" } else { "skip:register_refused" });
            let _ = notify::send(&Notice::report(
                format!("Solomon: tool proposal -> {}", proposal.name),
                format!(
                    "registered={ok} (linted, NOT executed — set \"approved_validation\": true on \
                     tools.{} in runtime\\_tools\\tools_manifest.json to allow the sandboxed \
                     dry-run; LIVE invocation additionally needs approved:true)",
                    proposal.name
                ),
            ));
        }
    }
}

// --------------------------------------------------------------------------- //
// tests — the self-tooling acceptance contracts
// --------------------------------------------------------------------------- //

/// Serialize every test that touches the SHARED tools manifest / audit / sandbox under the
/// per-process temp home (read-modify-write races + the shared atomic-write tmp path would
/// flake otherwise). Module-level so the `ceo.rs` seam-wiring test (which drives the validation
/// seam over the same manifest) can serialize against these too. Mirrors APPROVALS_TEST_LOCK;
/// poison-tolerant.
#[cfg(test)]
pub(crate) static TOOLING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TOOLING_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A runner that fakes PowerShell: parse checks pass, -DryRun runs exit with `dry_code`,
    /// everything else exits 0. Counts calls via Cells (Fn-compatible interior mutability).
    fn fake_runner(
        dry_code: i32,
        calls: &Cell<u32>,
    ) -> impl Fn(&[String], Option<&Path>, Duration) -> std::io::Result<proc::RunOut> + '_ {
        move |argv, _cwd, _t| {
            calls.set(calls.get() + 1);
            let code = if argv.iter().any(|a| a == "-DryRun") { dry_code } else { 0 };
            Ok(proc::RunOut { code, stdout: "fake ps".into(), stderr: String::new() })
        }
    }

    fn benign_script() -> String {
        "[CmdletBinding()]\n\
         param([switch]$DryRun)\n\
         if ($DryRun) { Write-Output 'dry run ok: 0 changes'; exit 0 }\n\
         New-Item -ItemType Directory -Force -Path 'runtime/_tools/scratch' | Out-Null\n\
         Set-Content -Path 'runtime/_tools/scratch/note.txt' -Value 'count 1'\n\
         exit 0\n"
            .to_string()
    }

    fn uniq_name(tag: &str) -> String {
        format!(
            "t{tag}{}{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|x| x.as_nanos() % 1_000_000)
                .unwrap_or(0)
        )
    }

    fn registered(name: &str) -> Value {
        read_manifest().pointer(&format!("/tools/{name}")).cloned().unwrap_or(Value::Null)
    }

    /// The full pipeline a usable tool walks: register (executes NOTHING) -> operator hand-sets
    /// `approved_validation: true` -> the validation seam runs the sandboxed dry-run. The fixture
    /// every invoke test needs.
    fn register_validated(p: &ToolProposal) {
        let calls = Cell::new(0u32);
        assert_eq!(register_tool_with(p, "m", "ph", &fake_runner(0, &calls))["ok"], true);
        assert_eq!(calls.get(), 0, "registration never executes");
        let mut m = read_manifest();
        m["tools"][&p.name]["approved_validation"] = json!(true);
        assert!(write_manifest(&m));
        let out = validate_approved_tools_with(&fake_runner(0, &calls));
        assert!(
            out.iter().any(|r| r["tool"] == json!(p.name.clone()) && r["passed"] == json!(true)),
            "validation passed for {}: {out:?}",
            p.name
        );
    }

    // -------- 17: the linter rejects EVERY denied category --------
    #[test]
    fn lint_rejects_each_denied_marker_family() {
        let cases: &[(&str, &str)] = &[
            // file deletion / mutation outside runtime
            ("Remove-Item 'C:\\Windows\\x.txt'", "fs delete outside runtime"),
            ("Set-Content -Path 'C:\\Users\\x\\a.txt' -Value 1", "fs write outside runtime"),
            ("Out-File -FilePath \"C:\\x.log\"", "out-file outside runtime"),
            ("Write-Output 1 > C:\\x.txt", "redirection with no allowed path literal"),
            ("rm sandbox.txt", "rm alias token"),
            ("del temp.txt", "del alias token"),
            // network sends / fetches
            ("Invoke-WebRequest 'https://example.com'", "web request"),
            ("$c = New-Object Net.WebClient", "webclient"),
            ("irm 'https://x'", "irm alias"),
            ("Send-MailMessage -To 'a@b.c'", "smtp send"),
            // process / service kills
            ("Stop-Process -Name sover", "stop-process"),
            ("taskkill /IM x.exe /F", "taskkill"),
            ("Stop-Service -Name spooler", "stop-service"),
            // background-process creation (no-daemons doctrine)
            ("schtasks /create /tn evil /tr x.exe", "schtasks"),
            ("Register-ScheduledTask -TaskName x", "scheduled task"),
            ("Start-Job -ScriptBlock { 1 }", "start-job"),
            ("New-Service -Name svc", "new-service"),
            ("Start-Process notepad.exe -WindowStyle Hidden", "start-process"),
            // eval / policy / privilege escalation
            ("Invoke-Expression $payload", "invoke-expression"),
            ("iex $x", "iex token"),
            ("Set-ExecutionPolicy Unrestricted", "execution policy"),
            ("Set-ItemProperty -Path 'HKLM:\\SOFTWARE\\x' -Name a -Value 1", "registry write"),
            ("Start-Process -Verb RunAs cmd", "runas"),
        ];
        for (script, why) in cases {
            let verdict = lint_tool(script);
            assert!(verdict.is_err(), "must reject ({why}): {script}");
        }
    }

    // -------- 17b: the OBFUSCATION family — expression-built invocations, encoded payloads --------
    #[test]
    fn lint_rejects_the_call_operator_obfuscation_family() {
        let cases: &[(&str, &str)] = &[
            // expression building inside the call operator (the classic linter bypass)
            ("& ('Stop-Proc'+'ess') -Name solomon", "concat inside call operator"),
            ("& (\"Sto\"+\"p-Process\") -Name x", "double-quote concat call"),
            ("& $cmd runtime/x", "call operator on a variable"),
            ("& \"Stop-Pro$suffix\" -Name x", "interpolated double-quoted command name"),
            ("& { Stop-Something }", "scriptblock invocation"),
            ("& ([char]83 + 'top-Process')", "char-cast assembly"),
            // dotted invocation of expressions
            ("$sb = [scriptblock]::Create($x); . $sb", "dot-sourcing a variable"),
            (". ($path)", "dot-sourcing a parenthesized expression"),
            // encoded / out-of-band execution
            ("powershell -EncodedCommand SQBFAFgAIABiAGEAZA==", "-EncodedCommand"),
            ("Invoke-Command -ScriptBlock { Get-Date }", "Invoke-Command"),
            ("icm { Get-Date }", "icm alias"),
            // backtick escapes inside command position spell denied verbs without containing them
            ("Sto`p-Process -Name solomon", "backtick-escape obfuscation"),
        ];
        for (script, why) in cases {
            assert!(lint_tool(script).is_err(), "must reject ({why}): {script}");
        }
        // and the LITERAL forms stay admissible (pre-filter, not a busywork gate): a bare literal
        // command name / quoted literal after `&` is fine (its text is still marker-scanned), and
        // a backtick line continuation is not an escape.
        assert!(lint_tool("& Write-Output hello").is_ok());
        assert!(lint_tool("& 'Write-Output' hello").is_ok());
        assert!(lint_tool("Write-Output one `\n  two").is_ok(), "line continuation is benign");
        // literal-but-denied names are caught by the SUBSTRING families, not missed via `&`
        assert!(lint_tool("& 'Stop-Process' -Name x").is_err());
    }

    // -------- 18: the linter ACCEPTS a benign runtime-scoped -DryRun tool --------
    #[test]
    fn lint_accepts_a_benign_runtime_scoped_dryrun_script() {
        let verdict = lint_tool(&benign_script());
        assert!(verdict.is_ok(), "the benign script must lint clean: {verdict:?}");
        // and the name gate: sane names pass, traversal/exotic names are refused
        assert!(valid_tool_name("count-runtime-files"));
        assert!(valid_tool_name("tool_v2"));
        for bad in ["", "..\\evil", "UPPER", "-lead", "a b", "1num", "x/..", "a.ps1"] {
            assert!(!valid_tool_name(bad), "{bad}");
        }
    }

    // -------- 19: a lint-failing proposal registers NOTHING --------
    #[test]
    fn lint_failing_proposal_registers_nothing_and_audits() {
        let _l = lock();
        let name = uniq_name("lintfail");
        let p = ToolProposal {
            name: name.clone(),
            purpose: "evil".into(),
            script: "param([switch]$DryRun)\nStop-Process -Name solomon".into(),
        };
        let calls = Cell::new(0u32);
        let out = register_tool_with(&p, "m", "ph", &fake_runner(0, &calls));
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["reason"], "lint");
        assert_eq!(calls.get(), 0, "the linter runs FIRST — no subprocess for a denied script");
        assert!(registered(&name).is_null(), "no manifest entry");
        assert!(!script_path(&name).exists(), "no script written");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(
            audit_body.contains(&name) && audit_body.contains("lint_reject"),
            "the rejection is audited: {audit_body}"
        );
    }

    // -------- 20: registration NEVER executes the proposed script --------
    #[test]
    fn registration_never_executes_and_lands_pending_operator_validation() {
        let _l = lock();
        let name = uniq_name("noexec");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        // A runner that PANICS on any invocation — registration must survive it untouched.
        let panicking = |_a: &[String], _c: Option<&Path>, _t: Duration| -> std::io::Result<proc::RunOut> {
            panic!("registration must NEVER execute the proposed script")
        };
        let out = register_tool_with(&p, "m", "ph", &panicking);
        assert_eq!(out["ok"], true, "{out}");
        let e = registered(&name);
        assert_eq!(e["validation"], "pending_operator");
        assert_eq!(e["dry_run"]["passed"], false);
        assert_eq!(e["dry_run"]["pending"], true);
        assert_eq!(e["approved_validation"], false, "only an operator flips it");
        assert_eq!(e["approved"], false);
        // and the unvalidated entry cannot be invoked AT ALL — not even dry-run
        let calls = Cell::new(0u32);
        let out = invoke_tool_with(&name, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["reason"], "unvalidated manifest entry");
        assert_eq!(calls.get(), 0, "no subprocess for an unvalidated tool");
        let _ = unregister_tool(&name);
    }

    // -------- 21: a clean register writes script + stamped manifest entry --------
    #[test]
    fn successful_register_writes_script_and_stamped_manifest_entry() {
        let _l = lock();
        let name = uniq_name("reg");
        let script = benign_script();
        let p = ToolProposal { name: name.clone(), purpose: "counts notes".into(), script: script.clone() };
        let calls = Cell::new(0u32);
        let out = register_tool_with(&p, "model-z", "ab12cd34", &fake_runner(0, &calls));
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(calls.get(), 0, "registration executes nothing");
        let e = registered(&name);
        assert_eq!(e["provenance"], "rsi:", "the provenance stamp is mandatory");
        assert_eq!(e["approved"], false, "approved starts FALSE — only an operator flips it");
        assert_eq!(e["sha256"], crate::provenance::sha256_hex(script.as_bytes()));
        assert_eq!(e["validation"], "pending_operator");
        assert_eq!(e["dry_run"]["passed"], false, "no dry-run ran — pending the operator");
        assert_eq!(e["dry_run"]["pending"], true);
        assert_eq!(e["lint"]["passed"], true);
        assert_eq!(e["authored_model"], "model-z");
        assert_eq!(e["prompt_sha8"], "ab12cd34");
        assert_eq!(e["version"], 1);
        let on_disk = std::fs::read_to_string(script_path(&name)).expect("script registered");
        assert_eq!(on_disk, script, "the registered bytes are the exact proposal bytes");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(audit_body.contains("\"event\":\"register\""), "{audit_body}");
        let _ = unregister_tool(&name);
    }

    // -------- the validation seam: operator-approved entries ONLY --------
    #[test]
    fn validation_seam_runs_only_operator_approved_entries() {
        let _l = lock();
        let approved = uniq_name("valyes");
        let untouched = uniq_name("valno");
        for n in [&approved, &untouched] {
            let p = ToolProposal { name: n.clone(), purpose: "x".into(), script: benign_script() };
            let calls = Cell::new(0u32);
            assert_eq!(register_tool_with(&p, "m", "ph", &fake_runner(0, &calls))["ok"], true);
        }
        // operator hand-sets approved_validation on ONE entry
        let mut m = read_manifest();
        m["tools"][&approved]["approved_validation"] = json!(true);
        assert!(write_manifest(&m));

        let calls = Cell::new(0u32);
        let out = validate_approved_tools_with(&fake_runner(0, &calls));
        assert!(
            out.iter().any(|r| r["tool"] == json!(approved.clone()) && r["passed"] == json!(true)),
            "{out:?}"
        );
        assert!(
            !out.iter().any(|r| r["tool"] == json!(untouched.clone())),
            "the unapproved entry is never validated: {out:?}"
        );
        let e = registered(&approved);
        assert_eq!(e["validation"], "validated");
        assert_eq!(e["dry_run"]["passed"], true);
        assert!(e["dry_run"].get("pending").is_none(), "pending cleared: {e}");
        let u = registered(&untouched);
        assert_eq!(u["validation"], "pending_operator", "untouched entry stays pending");
        assert_eq!(u["dry_run"]["pending"], true);
        // a validated tool is now dry-run invocable; the untouched one still refuses
        let out = invoke_tool_with(&approved, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["ok"], true, "{out}");
        assert_eq!(out["mode"], "dry_run");
        let out = invoke_tool_with(&untouched, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["reason"], "unvalidated manifest entry");
        // a second sweep finds nothing pending — validation is one-shot, not a retry loop
        let before = calls.get();
        assert!(validate_approved_tools_with(&fake_runner(0, &calls)).is_empty());
        assert_eq!(calls.get(), before, "no re-execution of an already-validated tool");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(audit_body.contains("validate_pass"), "{audit_body}");
        for n in [&approved, &untouched] {
            let _ = unregister_tool(n);
        }
    }

    // -------- a failing approved dry-run records the verdict + clears pending --------
    #[test]
    fn validation_failure_records_fail_and_still_blocks_invoke() {
        let _l = lock();
        let name = uniq_name("valfail");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        let calls = Cell::new(0u32);
        assert_eq!(register_tool_with(&p, "m", "ph", &fake_runner(0, &calls))["ok"], true);
        let mut m = read_manifest();
        m["tools"][&name]["approved_validation"] = json!(true);
        assert!(write_manifest(&m));

        let out = validate_approved_tools_with(&fake_runner(7, &calls)); // -DryRun exits 7
        assert!(
            out.iter().any(|r| r["tool"] == json!(name.clone()) && r["passed"] == json!(false)),
            "{out:?}"
        );
        let e = registered(&name);
        assert_eq!(e["validation"], "failed");
        assert_eq!(e["dry_run"]["passed"], false);
        assert_eq!(e["dry_run"]["pending"], false, "the verdict is recorded, not retried");
        assert!(e["dry_run"]["error"].as_str().unwrap().starts_with("dry_run: exit 7"), "{e}");
        // still not invocable
        let out = invoke_tool_with(&name, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["reason"], "unvalidated manifest entry");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(audit_body.contains("validate_fail"), "{audit_body}");
        let _ = unregister_tool(&name);
    }

    // -------- 22: sha256 tamper check refuses a hand-edited body --------
    #[test]
    fn tampered_script_body_is_refused_at_invoke() {
        let _l = lock();
        let name = uniq_name("tamper");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        let calls = Cell::new(0u32);
        register_validated(&p);
        // hand-edit the on-disk body (bypassing registration)
        std::fs::write(script_path(&name), "param([switch]$DryRun)\nexit 0\n# edited").unwrap();
        let out = invoke_tool_with(&name, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["ok"], false, "{out}");
        assert_eq!(out["reason"], "sha256_mismatch");
        let _ = unregister_tool(&name);
    }

    // -------- 23 + 24: live invocation is human-gated; approved unlocks it --------
    #[test]
    fn live_invoke_degrades_to_dryrun_until_operator_approves() {
        let _l = lock();
        let name = uniq_name("livegate");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        register_validated(&p);

        // (23) live requested, approved==false -> DEGRADES to -DryRun, loudly flagged.
        let seen_dryrun = Cell::new(false);
        let runner = |argv: &[String], _c: Option<&Path>, _t: Duration| {
            seen_dryrun.set(argv.iter().any(|a| a == "-DryRun"));
            Ok(proc::RunOut { code: 0, stdout: String::new(), stderr: String::new() })
        };
        let out = invoke_tool_with(&name, true, &[], &runner);
        assert_eq!(out["mode"], "dry_run", "unapproved live request degrades: {out}");
        assert_eq!(out["degraded"], true);
        assert!(seen_dryrun.get(), "the degraded run actually rides -DryRun");

        // (24) the OPERATOR sets approved:true (hand-edit of the manifest) -> live unlocks.
        let mut m = read_manifest();
        m["tools"][&name]["approved"] = json!(true);
        assert!(write_manifest(&m));
        let out = invoke_tool_with(&name, true, &[], &runner);
        assert_eq!(out["mode"], "live", "{out}");
        assert_eq!(out["degraded"], false);
        assert!(!seen_dryrun.get(), "a live run carries no -DryRun switch");
        // and the invocation history recorded both runs
        let inv = registered(&name)["invocations"].as_array().unwrap().clone();
        assert_eq!(inv.len(), 2);
        assert_eq!(inv[0]["mode"], "dry_run");
        assert_eq!(inv[1]["mode"], "live");
        let _ = unregister_tool(&name);
    }

    // -------- 25: unregister rolls back fully --------
    #[test]
    fn unregister_removes_entry_and_script_and_audits() {
        let _l = lock();
        let name = uniq_name("unreg");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        let calls = Cell::new(0u32);
        assert_eq!(register_tool_with(&p, "m", "ph", &fake_runner(0, &calls))["ok"], true);
        assert!(script_path(&name).exists());

        let out = unregister_tool(&name);
        assert_eq!(out["ok"], true, "{out}");
        assert!(registered(&name).is_null(), "manifest entry removed");
        assert!(!script_path(&name).exists(), "script removed");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(audit_body.contains("\"event\":\"unregister\""), "{audit_body}");
        // a subsequent invoke reports not_found
        let out = invoke_tool_with(&name, false, &[], &fake_runner(0, &calls));
        assert_eq!(out["reason"], "not_found");
        // and unregistering twice is an honest not_found, never a panic
        assert_eq!(unregister_tool(&name)["ok"], false);
    }

    // -------- 26: the provenance tripwire is UNTOUCHED --------
    #[test]
    fn provenance_tripwire_watches_no_tools_artifact() {
        for f in crate::provenance::TRACKED {
            assert!(
                !f.contains("_tools") && !f.contains("tools_manifest"),
                "provenance::TRACKED must never watch a _tools artifact: {f}"
            );
        }
        // the manifest resolves under gitignored runtime/, never beside the tracked configs
        let mp = manifest_path();
        assert!(
            mp.starts_with(paths::here().join("runtime")),
            "tools_manifest.json must live under runtime/: {mp:?}"
        );
    }

    // -------- 27: the honest tooling-directive trigger (pure) --------
    #[test]
    fn is_tool_directive_positive_and_negative_fixtures() {
        for pos in [
            "build a helper script to rotate runtime logs",
            "add a tool that summarizes outcomes",
            "automate the report collection",
            "new capability: local disk usage audit",
        ] {
            assert!(is_tool_directive(pos), "{pos}");
        }
        for neg in ["fix the trader bug", "improve README", "cold outreach to partners"] {
            assert!(!is_tool_directive(neg), "{neg}");
        }
    }

    // -------- integration (real PowerShell): the benign script honors the contract --------
    // #[ignore]d: run explicitly with `cargo test real_powershell -- --ignored` on a host where
    // spawning powershell.exe in tests is acceptable. Proves the parse + -DryRun contract against
    // the REAL interpreter (the seam-injected tests above stay hermetic).
    #[test]
    #[ignore]
    fn real_powershell_dry_run_contract_holds() {
        let _l = lock();
        let name = uniq_name("realps");
        let rec = dry_run_with(&name, &benign_script(), &real_runner).expect("dry run passes");
        assert_eq!(rec["passed"], true);
        assert!(rec["summary"].as_str().unwrap().contains("dry run ok"));
    }
}
