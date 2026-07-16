//! CEO autonomy, piece 2 — TOOLSET SELF-EXTENSION: propose -> lint -> dry-run -> register ->
//! invoke, every rung fail-closed, live effects human-gated.
//!
//! ============================ WHAT THIS ADDS =============================
//! The operator's end goal names "control and add to its own toolset on the fly" as a CEO
//! capability. This module gives it real teeth WITHOUT weakening a single gate: the CEO plane can
//! autonomously author a small PowerShell helper, statically LINT it against a deny-list (the
//! core safety gate — dangerous verbs are rejected BEFORE any subprocess touches the script),
//! validate it with a sandboxed mandatory `-DryRun` self-test, and REGISTER it (script + manifest
//! entry) under gitignored `runtime\_tools\`. A registered tool is auto-invocable by the tick
//! ONLY in `-DryRun` (exactly what validation proved safe); a LIVE invocation requires an
//! operator-set `approved: true` in the manifest — absent that, `invoke_tool(live=true)` DEGRADES
//! to a dry-run and says so (never a silent live effect). `unregister_tool` is the built-in
//! per-tool rollback.
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
//! ## The deny-list linter (static, fail-closed — the core safety gate)
//! ANY match rejects, with the offending markers listed: file mutation outside `runtime\`/temp,
//! network sends/fetches (+ the `research::EXTERNAL_MUTATION_MARKERS` set, reused so the two deny
//! sources cannot diverge), process/service kills, background-process creation (schtasks /
//! scheduled tasks / Start-Job / services — the no-daemons doctrine), and eval/policy/privilege
//! escalation. A denied tool is never written to disk, never dry-run, never registered; the
//! rejection is audited. The linter is deliberately over-broad (substring + word-boundary
//! heuristics): a false positive costs one refused proposal, a false negative could cost a real
//! side effect — fail-closed is the correct direction.
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
    Invoke-Expression, execution-policy or registry or privilege changes. Keep it under 60 lines. \
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
const DENY_TOKENS: &[&str] = &["rm", "del", "rmdir", "iwr", "irm", "curl", "wget", "kill", "iex"];

/// True iff `lower` contains `tok` as a standalone alphanumeric word (pure).
fn has_token(lower: &str, tok: &str) -> bool {
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == tok)
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

/// The static deny-list linter (pure — the core safety gate, run FIRST). `Ok(())` admits the
/// script to the dry-run rung; `Err` carries EVERY offending marker for the audit trail.
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

/// Register a proposal through the FULL gate: name check -> LINT (first, before any subprocess) ->
/// mandatory sandboxed dry-run -> `rsi:` provenance stamp + sha256 -> script + manifest write
/// (`approved: false` — nothing in Solomon ever sets it true). ANY failing rung registers NOTHING
/// and audits the named rejection. Returns `{ok, tool, reason?}`.
pub fn register_tool(p: &ToolProposal, model: &str, prompt_sha8: &str) -> Value {
    register_tool_with(p, model, prompt_sha8, &real_runner)
}

/// Core of [`register_tool`] with the subprocess RUNNER injected (hermetic tests need no real
/// PowerShell; a `#[ignore]`d integration test can exercise the real one).
pub(crate) fn register_tool_with<R>(
    p: &ToolProposal,
    model: &str,
    prompt_sha8: &str,
    runner: &R,
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
    // (2) mandatory dry-run (edge E13).
    let dry_run = match dry_run_with(&p.name, &p.script, runner) {
        Ok(rec) => rec,
        Err(e) => {
            audit("dryrun_fail", &p.name, &e);
            return json!({"ok": false, "tool": p.name, "reason": e});
        }
    };
    // (3) provenance stamp + tamper hash, then script + manifest (approved stays FALSE).
    let sha256 = crate::provenance::sha256_hex(p.script.as_bytes());
    let spath = script_path(&p.name);
    if let Some(parent) = spath.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if proc::atomic_write_bytes(&spath, p.script.as_bytes()).is_err() {
        audit("dryrun_fail", &p.name, "script write failed");
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
        "dry_run": dry_run,
        "lint": {"passed": true, "denied_markers": []},
        "approved": false,
        "invocations": [],
    });
    if !write_manifest(&manifest) {
        let _ = std::fs::remove_file(&spath); // no half-registration
        audit("dryrun_fail", &p.name, "manifest write failed");
        return json!({"ok": false, "tool": p.name, "reason": "manifest write failed"});
    }
    audit("register", &p.name, &format!("v{version} sha256={sha256} approved=false"));
    json!({"ok": true, "tool": p.name, "version": version})
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
/// dry-run -> register attempt per day, and only when today's planner output carries a tooling
/// directive (honest trigger) AND the fleet call budget has headroom. STAMP-FIRST (`create_new`)
/// before the LLM call — a hung/failed authoring consumes the day's attempt; two OS processes
/// cannot double-fire. Registration lands `approved:false` — live invocation stays human-gated.
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
                    "registered={ok} (lint+dry-run gated, approved=false — set approved:true in \
                     runtime\\_tools\\tools_manifest.json to allow LIVE invocation)"
                ),
            ));
        }
    }
}

// --------------------------------------------------------------------------- //
// tests — the self-tooling acceptance contracts
// --------------------------------------------------------------------------- //

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Serialize every test that touches the SHARED tools manifest / audit / sandbox under the
    /// per-process temp home (read-modify-write races + the shared atomic-write tmp path would
    /// flake otherwise). Mirrors NOTIFY_ENV_LOCK; poison-tolerant.
    static TOOLING_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    // -------- 20: a failing -DryRun blocks registration --------
    #[test]
    fn dryrun_failure_blocks_registration_and_audits() {
        let _l = lock();
        let name = uniq_name("dryfail");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        let calls = Cell::new(0u32);
        let out = register_tool_with(&p, "m", "ph", &fake_runner(3, &calls));
        assert_eq!(out["ok"], false, "{out}");
        assert!(out["reason"].as_str().unwrap().starts_with("dry_run: exit 3"), "{out}");
        assert!(calls.get() >= 2, "parse + dry-run both ran");
        assert!(registered(&name).is_null(), "no manifest entry on a dry-run failure");
        assert!(!script_path(&name).exists(), "no registered script on a dry-run failure");
        let audit_body = std::fs::read_to_string(audit_path()).unwrap_or_default();
        assert!(audit_body.contains("dryrun_fail"), "{audit_body}");
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
        let e = registered(&name);
        assert_eq!(e["provenance"], "rsi:", "the provenance stamp is mandatory");
        assert_eq!(e["approved"], false, "approved starts FALSE — only an operator flips it");
        assert_eq!(e["sha256"], crate::provenance::sha256_hex(script.as_bytes()));
        assert_eq!(e["dry_run"]["passed"], true);
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

    // -------- 22: sha256 tamper check refuses a hand-edited body --------
    #[test]
    fn tampered_script_body_is_refused_at_invoke() {
        let _l = lock();
        let name = uniq_name("tamper");
        let p = ToolProposal { name: name.clone(), purpose: "x".into(), script: benign_script() };
        let calls = Cell::new(0u32);
        assert_eq!(register_tool_with(&p, "m", "ph", &fake_runner(0, &calls))["ok"], true);
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
        let calls = Cell::new(0u32);
        assert_eq!(register_tool_with(&p, "m", "ph", &fake_runner(0, &calls))["ok"], true);

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
