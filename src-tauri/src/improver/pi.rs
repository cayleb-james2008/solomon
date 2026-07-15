//! Port of run_improver.py's pi_agent_contract area: the pi-CLI agent invocation surface.
//!
//! Bug-for-bug with improver/run_improver.py (functions: run_pi, _phase_run_pi, final_text,
//! _kill_tree, _agent_shim_dir, _write_shim). These drive the single Pi coding session per
//! iteration (and the per-phase one-off calls). run_pi spawns pi DIRECTLY via std::process::Command
//! (NOT control::proc::run) because it needs the Popen + communicate(timeout) + force-kill-the-tree
//! lifecycle: pi forks a Node grandchild that holds the stdout pipe, so a plain child-kill leaves the
//! pipe open and the read blocks forever (the observed 5-hour freeze). On timeout we kill the whole
//! process tree (Windows `taskkill /F /T`; POSIX `killpg(SIGKILL)`), wait a 20s grace, then surface a
//! timeout the caller treats as a failed iteration.
//!
//! The pi event stream is JSONL (`pi --print --mode json`); `final_text` extracts the LAST assistant
//! message's concatenated text-parts (the implementer's summary used for commit/PR body, deviation
//! detection, and feedback injection).

use crate::control::proc::{self, RunOut};
use crate::improver::budget;
use crate::improver::ctx::Ctx;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

// --------------------------------------------------------------------------- //
// is_quota_error — provider 429 / rate-limit / quota detection
// --------------------------------------------------------------------------- //

/// Detect a provider QUOTA / rate-limit / transport error in a text blob (stderr, or the
/// structured error text extracted from pi's stdout JSONL stream — see [`stream_error_messages`]).
///
/// When the shared Ollama account's usage cap is saturated (HTTP 429, body e.g. "you have reached
/// your session usage limit" or "you (cayleb_james) have reached your weekly usage limit, add extra
/// usage: https://ollama.com/settings"), this is a TRANSPORT/QUOTA error, NOT a reasoned model noop
/// — counting it as a noop caused a fleet-wide noop storm that escalated and RESET lanes (lost
/// iteration progress) whenever the shared account's usage cap was hit. Matching the "usage limit"
/// substring (a superset of "session usage limit") catches session/weekly/daily/any future
/// "<period> usage limit" wording without needing to enumerate each one.
///
/// Only ever called on stderr text or extracted `errorMessage` fields — NEVER on the agent's own
/// assistant-authored text content — so a task that legitimately narrates "429" or "rate limit" in
/// its summary cannot trigger a false positive.
pub fn is_quota_error(stderr: &str) -> bool {
    let l = stderr.to_ascii_lowercase();
    ["429", "usage limit", "rate limit", "rate_limit", "rate-limit",
     "too many requests", "quota exceeded"]
        .iter()
        .any(|pat| l.contains(pat))
}

/// Extract every `errorMessage` from a `stopReason:"error"` assistant message in pi's stdout JSONL
/// stream (same event-selection as [`final_text`]: an `agent_end`'s `messages[]`, or a streamed bare
/// `message` dict). 2026-07-03 INCIDENT: pi's `--print --mode json` does NOT put a provider transport
/// error in stderr (stderr is empty) — it emits an assistant message with empty `content` (so
/// `final_text` correctly extracts nothing) but `stopReason:"error"` and a human-readable
/// `errorMessage`, e.g. `errorMessage: "429 \"you (cayleb_james) have reached your weekly usage
/// limit...\""`. The original `is_quota_error(&stderr)`-only check therefore NEVER matched this real
/// 429 error (verified live: a direct pi invocation against the exhausted account reproduced this
/// exact JSONL shape with empty stderr), so the fleet-wide noop storm continued even after widening
/// the stderr pattern list. Concatenated space-separated so [`is_quota_error`] can pattern-match them
/// exactly like stderr text.
pub fn stream_error_messages(stdout: &str) -> String {
    let mut errs: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let msgs: Vec<Value> = if ev.get("type").and_then(Value::as_str) == Some("agent_end") {
            match ev.get("messages") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            }
        } else if matches!(ev.get("message"), Some(Value::Object(_))) {
            vec![ev.get("message").cloned().unwrap_or(Value::Null)]
        } else {
            continue;
        };
        for m in &msgs {
            if !m.is_object() {
                continue;
            }
            if m.get("stopReason").and_then(Value::as_str) == Some("error") {
                if let Some(em) = m.get("errorMessage").and_then(Value::as_str) {
                    errs.push(em.to_string());
                }
            }
        }
    }
    errs.join(" ")
}

/// True iff EITHER pi's stderr OR a `stopReason:"error"` message in its stdout JSONL stream carries a
/// quota/rate-limit signature (see [`is_quota_error`] and [`stream_error_messages`]). This is the
/// check callers should use — stderr alone misses the 2026-07-03 incident class where the provider
/// error lands in stdout's structured `errorMessage` field instead.
pub fn is_quota_error_output(stdout: &str, stderr: &str) -> bool {
    is_quota_error(stderr) || is_quota_error(&stream_error_messages(stdout))
}

#[cfg(windows)]
use std::os::windows::process::CommandExt;

// --------------------------------------------------------------------------- #
// _kill_tree
// --------------------------------------------------------------------------- #

/// run_improver._kill_tree (~981-992): force-kill a process and ALL its children (pi spawns a node
/// child that holds the stdout pipe). Windows: `taskkill /F /T /PID <pid>` (hidden window). POSIX:
/// `killpg(getpgid(pid), SIGKILL)`, swallowing OSError. Off Windows without a process group of its
/// own, getpgid resolves the child's group — faithful to the Python (which sets no preexec_fn either,
/// so `new_group` is a no-op on POSIX there too).
pub fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        // subprocess.run(["taskkill", "/F", "/T", "/PID", str(pid)], capture_output=True,
        //                **hidden_subprocess_kwargs())
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped()) // capture_output=True
            .stderr(Stdio::piped())
            .creation_flags(proc::CREATE_NO_WINDOW); // hidden_subprocess_kwargs() (no new_group)
        let _ = cmd.output();
    }
    #[cfg(not(windows))]
    {
        // os.killpg(os.getpgid(pid), signal.SIGKILL); except OSError: pass
        unsafe {
            extern "C" {
                fn getpgid(pid: i32) -> i32;
                fn killpg(pgrp: i32, sig: i32) -> i32;
            }
            const SIGKILL: i32 = 9;
            let pgid = getpgid(pid as i32);
            if pgid >= 0 {
                let _ = killpg(pgid, SIGKILL);
            }
            // getpgid<0 (errno set) mirrors the Python OSError -> pass (no kill)
        }
    }
}

// --------------------------------------------------------------------------- #
// _write_shim / _agent_shim_dir
// --------------------------------------------------------------------------- #

/// run_improver._write_shim (~995-1000): write a shim file (UTF-8) then chmod 0o755. Best-effort:
/// any OSError is swallowed. chmod is a no-op off POSIX (matches CPython, where os.chmod's mode bits
/// are largely ignored on Windows — there it only toggles the read-only attribute, which 0o755 leaves
/// writable, so the file stays as written).
fn write_shim(path: &Path, content: &str) {
    if std::fs::write(path, content.as_bytes()).is_err() {
        return; // except OSError: pass
    }
    set_executable(path);
}

#[cfg(unix)]
fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) {
    // os.chmod(path, 0o755) on Windows only clears the read-only bit; 0o755 keeps it writable, so the
    // freshly-written file is already in the right state. No-op.
}

/// run_improver._agent_shim_dir (~1003-1037): create (idempotently) a dir of PATH shims that REFUSE
/// the version-control verbs that escape the runner's branch-per-iteration sandbox — `gh` entirely
/// (the agent must use the read-only github_* tools), `git push|pull|merge|rebase`, AND the
/// branch-switching verbs (`git switch` any form, `git checkout -b`/`-B` or `git checkout <branch>`,
/// `git branch <name>`) — the 2026-06-24 escape was `git checkout -b chore/...` which committed
/// off the runner's rsi/* branch and wedged the gate/ship/revert. File-restore stays allowed
/// (`git checkout -- <file>`, `git checkout .`) and `git branch` (list). Read-only git and pi's own
/// internal git PASS THROUGH to the real binary. Prepended to the agent's PATH in run_pi. Returns the
/// dir, or None if it can't be created (best-effort). Both POSIX shell shims and Windows .cmd shims
/// are written into the same dir; only the matching platform's are on PATH-resolve.
///
/// The shim error strings differ POSIX vs Windows VERBATIM (debugging-load-bearing) and must match
/// run_improver byte-for-byte.
pub fn agent_shim_dir(ctx: &Ctx) -> Option<PathBuf> {
    // d = RUNTIME / "agent_shims"; d.mkdir(parents=True, exist_ok=True); except OSError: return None
    let d = ctx.runtime.join("agent_shims");
    if std::fs::create_dir_all(&d).is_err() {
        return None;
    }

    // gh (POSIX): verbose error WITH the "read-only ... gh CLI ... runner owns GitHub" phrasing.
    write_shim(
        &d.join("gh"),
        "#!/bin/sh\necho \"blocked by Solomon: use the read-only github_* tools, \
not the gh CLI (the runner owns GitHub)\" >&2\nexit 1\n",
    );
    // gh.cmd (Windows): SHORTER error, no "read-only"/"runner owns GitHub" phrasing.
    write_shim(
        &d.join("gh.cmd"),
        "@echo off\r\necho blocked by Solomon: use the github_* tools, not gh 1>&2\r\nexit /b 1\r\n",
    );

    // real_git = shutil.which("git"); only write the git shims when an ABSOLUTE git path is found.
    let real_git = proc::which_git().and_then(|p| {
        if p.is_absolute() {
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        }
    });
    if let Some(real_git) = real_git {
        // git (POSIX): case-statement. The VC verbs interpolate via `$1`; the branch verbs (switch,
        // checkout -b/<branch>, branch <name>) are refused too, but file-restore (`checkout -- <file>`,
        // `checkout .`) and `branch` (list) PASS THROUGH. Branch names can't start with `-`, so a `-*`
        // `$2` (incl. `--`) is always a flag/pathspec form and is safe to forward.
        let git_sh = format!(
            "#!/bin/sh\ncase \"$1\" in\n\
push|pull|merge|rebase) echo \"blocked by Solomon: the runner owns version control \
(no git $1 in the agent)\" >&2; exit 1;;\n\
switch) echo \"blocked by Solomon: the runner owns branches (no git switch in the agent)\" >&2; exit 1;;\n\
checkout) case \"$2\" in\n\
-b|-B) echo \"blocked by Solomon: the runner owns branches (no git checkout -b in the agent)\" >&2; exit 1;;\n\
\"\"|.|-*) exec \"{real_git}\" \"$@\";;\n\
*) echo \"blocked by Solomon: the runner owns branches \
(use git checkout -- <file> to discard, no branch switch in the agent)\" >&2; exit 1;;\n\
esac;;\n\
branch) case \"$2\" in\n\
\"\"|-*) exec \"{real_git}\" \"$@\";;\n\
*) echo \"blocked by Solomon: the runner owns branches (no git branch <name> in the agent)\" >&2; exit 1;;\n\
esac;;\n\
*) exec \"{real_git}\" \"$@\";;\nesac\n"
        );
        write_shim(&d.join("git"), &git_sh);
        // git.cmd (Windows): /I case-insensitive verb checks; the blocked messages do NOT name the verb.
        // VC verbs -> :blk; branch-switching verbs -> :blkbr. checkout/branch dispatch to sub-labels
        // that forward file-restore/list forms (empty $2, `.`, or a `-`-led flag incl. `--`) to real git
        // and refuse a bare branch token. (Branch names can't start with `-`, so a `-`-led $2 is safe.)
        let git_cmd = format!(
            "@echo off\r\n\
if /I \"%~1\"==\"push\" goto blk\r\n\
if /I \"%~1\"==\"pull\" goto blk\r\n\
if /I \"%~1\"==\"merge\" goto blk\r\n\
if /I \"%~1\"==\"rebase\" goto blk\r\n\
if /I \"%~1\"==\"switch\" goto blkbr\r\n\
if /I \"%~1\"==\"checkout\" goto chk\r\n\
if /I \"%~1\"==\"branch\" goto br\r\n\
\"{real_git}\" %*\r\n\
goto :eof\r\n\
:chk\r\n\
if /I \"%~2\"==\"-b\" goto blkbr\r\n\
if /I \"%~2\"==\"-B\" goto blkbr\r\n\
if \"%~2\"==\"\" goto run\r\n\
if \"%~2\"==\".\" goto run\r\n\
set \"a2=%~2\"\r\n\
if \"%a2:~0,1%\"==\"-\" goto run\r\n\
goto blkbr\r\n\
:br\r\n\
if \"%~2\"==\"\" goto run\r\n\
set \"b2=%~2\"\r\n\
if \"%b2:~0,1%\"==\"-\" goto run\r\n\
goto blkbr\r\n\
:run\r\n\
\"{real_git}\" %*\r\n\
goto :eof\r\n\
:blk\r\necho blocked by Solomon: the runner owns version control 1>&2\r\nexit /b 1\r\n\
:blkbr\r\necho blocked by Solomon: the runner owns branches 1>&2\r\nexit /b 1\r\n"
        );
        write_shim(&d.join("git.cmd"), &git_cmd);
    }
    Some(d)
}

// --------------------------------------------------------------------------- #
// resolve_pi_invocation — Windows batch-shim → node bypass (CVE-2024-24576 workaround)
// --------------------------------------------------------------------------- #

/// Resolve the `(program, leading_args)` to spawn for `pi`. On Windows, when `pi` resolves to an npm
/// `pi.CMD`/`pi.bat` shim, return `(node, [<cli.js>])` extracted from the shim so we spawn the real
/// `node.exe` (Command never sanitizes args for a real exe) instead of the batch file (whose multi-line
/// args Rust refuses — see run_pi's deviation note). Everywhere else: `(pi, [])` — unchanged.
///
/// The npm shim's launch line is `"%_prog%" "%dp0%\node_modules\…\cli.js" %*`; we read the shim, take
/// the `node_modules…*.js` token, resolve it against the shim's own directory, and verify it exists.
/// If anything is off (not a batch shim, can't read, no js token, js missing) we fall back to the
/// resolved `pi` path verbatim so behavior degrades to the prior (possibly-failing) path, never worse.
pub(crate) fn resolve_pi_invocation(pi: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        let lower = pi.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            if let Some(cli_js) = node_cli_from_shim(pi) {
                // node via which() (full path) or the bare "node" last-resort (same as _which).
                let node = match which::which("node") {
                    Ok(p) => p.to_string_lossy().into_owned(),
                    Err(_) => "node".to_string(),
                };
                return (node, vec![cli_js]);
            }
        }
    }
    let _ = pi; // (no-op read off Windows)
    (pi.to_string(), Vec::new())
}

/// Extract the `<dir>\node_modules\…\*.js` entry an npm batch shim wraps. Reads the shim file, finds
/// the first `node_modules` token through the next `.js`, and resolves it against the shim's directory
/// (`%dp0%`). Returns the absolute js path iff it exists on disk. The earlier `PATHEXT` `.JS` mention
/// in the shim sits before `node_modules`, so it never matches.
#[cfg(windows)]
fn node_cli_from_shim(cmd_path: &str) -> Option<String> {
    let text = std::fs::read_to_string(cmd_path).ok()?;
    let dir = Path::new(cmd_path).parent()?;
    let lower = text.to_ascii_lowercase();
    let start = lower.find("node_modules")?;
    let rel_end = lower[start..].find(".js")? + start + 3; // include ".js"
    let rel = &text[start..rel_end]; // original-case relative path (backslash-separated)
    let js = dir.join(rel);
    if js.exists() {
        Some(js.to_string_lossy().into_owned())
    } else {
        None
    }
}

// --------------------------------------------------------------------------- #
// run_pi
// --------------------------------------------------------------------------- #

/// run_improver.run_pi (~1040-1080): run ONE pi --mode json session against the agent contract.
///
/// argv EXACTLY:
///   [pi_exe, --print, --mode, json, -ne, --provider, PI_PROVIDER, --model, PI_MODEL,
///    (--thinking REASONING if REASONING truthy), -e, PI_EXT, (-e GITHUB_TOOLS_EXT if GITHUB_TOOLS),
///    --append-system-prompt, system_md|AGENT_MD, task]
/// `-ne` (--no-extensions) is ALWAYS set so a managed repo's own .pi/extensions can't crash pi at
/// startup (which solomon would miscount as a model no-op). The explicit `-e` paths still load.
///
/// env = clean env (GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME stripped, UTF-8 stdio) + the three
/// RSI_* vars the unified provider.ts reads, + the agent-shim dir PREPENDED to PATH (so the agent's
/// git/gh resolve to the refusing shims first). cwd=REPO. Spawned in a NEW PROCESS GROUP
/// (CREATE_NEW_PROCESS_GROUP on Windows) so taskkill /T can reach pi's node grandchild.
///
/// Lifecycle: Popen + communicate(timeout). On timeout -> kill_tree(pid), then a 20s grace
/// communicate, then surface a timeout. Returns RunOut{code, stdout, stderr} on normal exit; on
/// timeout returns a synthetic timeout RunOut (the Python re-raises TimeoutExpired; the Rust caller
/// surface is a failed RunOut carrying the partial streams + a timed-out stderr marker — see below).
///
/// DEVIATION (return shape on timeout): Python `run_pi` RE-RAISES subprocess.TimeoutExpired(output,
/// stderr) which one_iteration catches. With a non-panic RunOut return here, a timeout is surfaced as
/// RunOut{code: 124, stdout: <partial out>, stderr: <partial err> + "\n<argv0> timed out after <t>s"},
/// the same rc=124/"timed out after Ns" convention ctx.git()/gh() use for their bounded timeouts, so
/// the caller's existing timeout handling applies uniformly. Normal (non-timeout) returns are exact.
/// run_pi's exact pi argv (program + args) for the CURRENT ctx provider/model/reasoning. Extracted
/// so `budget::run_canary` exercises the IDENTICAL invocation surface (same batch-shim bypass,
/// extensions, system prompt) — a canary spawned any other way could pass while the real call path
/// stays broken (the asmodeus dead-fallback failure shape).
///
/// DEVIATION (Windows): the Python `run_pi` spawned `pi` (which resolves to the npm `pi.CMD`
/// batch shim) directly. Rust's std::process::Command REFUSES to spawn a `.cmd`/`.bat` when ANY
/// argument contains a character it cannot safely escape into a batch command line — newlines in
/// particular (the CVE-2024-24576 hardening, Rust ≥1.77.2): `spawn()` returns
/// io::ErrorKind::InvalidInput "batch file arguments are invalid" INSTANTLY. The implement task is
/// almost always multi-line (backlog item + gate/visual feedback), so every real iteration failed
/// with an instant rc=-1 that the caller miscounted as a model no-op. Fix: when `pi` resolves to a
/// batch shim, invoke the underlying `node <cli.js>` it wraps instead — node.exe is a real
/// executable, so Command passes the multi-line arg through verbatim (byte-identical argv to what
/// `pi.CMD` would have forwarded). Off Windows / non-batch pi: unchanged (program = the pi path).
pub(crate) fn build_pi_argv(ctx: &Ctx, task: &str, system_md: Option<&Path>) -> Vec<String> {
    let pi = ctx.pi_exe();
    let (program, lead) = resolve_pi_invocation(&pi);
    let mut args: Vec<String> = vec![program];
    args.extend(lead);
    args.extend([
        "--print".into(),
        "--mode".into(),
        "json".into(),
        "-ne".into(),
        "--provider".into(),
        ctx.pi_provider.clone(),
        "--model".into(),
        ctx.pi_model.clone(),
    ]);
    if !ctx.reasoning.is_empty() {
        args.push("--thinking".into());
        args.push(ctx.reasoning.clone());
    }
    args.push("-e".into());
    args.push(ctx.pi_ext.to_string_lossy().into_owned());
    if ctx.github_tools {
        args.push("-e".into());
        args.push(ctx.github_tools_ext.to_string_lossy().into_owned());
    }
    args.push("--append-system-prompt".into());
    // str(system_md or AGENT_MD)
    let sys_prompt: PathBuf = match system_md {
        Some(p) => p.to_path_buf(),
        None => ctx.agent_md.clone(),
    };
    args.push(sys_prompt.to_string_lossy().into_owned());
    args.push(task.to_string());
    args
}

/// run_pi's process env, shared with `budget::run_canary` for the same fidelity reason as
/// [`build_pi_argv`]: clean env (GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME stripped, UTF-8
/// stdio), the three RSI_* vars the unified provider.ts reads, and the agent-shim dir PREPENDED to
/// PATH (so the agent's git/gh resolve to the refusing shims first).
pub(crate) fn apply_pi_env(ctx: &Ctx, cmd: &mut Command) {
    // _clean_env(): strip GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME + force UTF-8 stdio.
    ctx.apply_clean_env(cmd);
    // The unified provider.ts registers the provider under RSI_PROVIDER; the extension reads the
    // model id (RSI_MODEL) and reasoning level (RSI_REASONING) from these.
    cmd.env("RSI_PROVIDER", &ctx.pi_provider);
    cmd.env("RSI_MODEL", &ctx.pi_model);
    cmd.env("RSI_REASONING", &ctx.reasoning);
    // shim = _agent_shim_dir(); if shim: env["PATH"] = shim + os.pathsep + env.get("PATH", "")
    if let Some(shim) = agent_shim_dir(ctx) {
        let cur = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ";" } else { ":" };
        let new_path = if cur.is_empty() {
            shim.to_string_lossy().into_owned()
        } else {
            format!("{}{}{}", shim.to_string_lossy(), sep, cur)
        };
        cmd.env("PATH", new_path);
    }
}

pub fn run_pi(ctx: &mut Ctx, task: &str, timeout: i64, system_md: Option<&Path>) -> RunOut {
    // ---- RSI v3 WIRING 1/4: per-cycle budget (freshness ledger, WS1; requirement 4) ---------- #
    // Checked FIRST — an exhausted cycle budget must not even resolve/canary an endpoint. The
    // return is shaped like the rc=124 timeout RunOut so every caller lands in its EXISTING
    // note_timeout handling (no new wiring anywhere else), and the spawn is skipped entirely.
    if let Some(reason) = crate::improver::freshness::budget_exceeded(ctx) {
        ctx.log(&format!(
            "run_pi refused: cycle budget exhausted — {reason} (no token spent)"
        ));
        return RunOut {
            code: 124,
            stdout: String::new(),
            stderr: format!("cycle budget exhausted: {reason}"),
        };
    }

    // ---- RSI v3 WIRING 2/4: endpoint resolution + provider-budget preflight (catalog #2) ----- #
    // effective_endpoint returns the primary, or a canaried fallback when the primary is parked.
    // If even the resolved endpoint is Parked (i.e. the fallback path is unusable too), refuse
    // with a synthesized '429 ...' stderr: the literal "429" makes the EXISTING
    // is_quota_error/is_quota_error_output paths in iteration/oneshot/supervisor classify this as
    // quota_error — never a model no-op — with zero new wiring.
    let (prov, model, is_fallback) = budget::effective_endpoint(ctx);
    if let budget::Decision::Parked { until, why } = budget::preflight(ctx, &prov, &model) {
        ctx.log(&format!(
            "run_pi refused: {prov}:{model} parked by budget ledger until {until} — {why}"
        ));
        return RunOut {
            code: 1,
            stdout: String::new(),
            stderr: budget::parked_stderr(until, &why),
        };
    }

    // ---- RSI v3 WIRING 3/4: argv/env built against the RESOLVED endpoint --------------------- #
    // build_pi_argv/apply_pi_env read ctx.pi_provider/pi_model; when the fallback is in effect
    // they are swapped in for THIS call only and restored on every exit path below (the loop's
    // configured endpoint is a per-repo contract, not ours to keep).
    let saved_endpoint = (ctx.pi_provider.clone(), ctx.pi_model.clone());
    if is_fallback {
        let age = budget::last_canary_age_secs(ctx, &prov, &model)
            .map(|a| format!("{a}s"))
            .unwrap_or_else(|| "unknown".to_string());
        ctx.log(&format!(
            "provider fallback in effect: {}:{} -> {prov}:{model} (canary passed {age} ago)",
            saved_endpoint.0, saved_endpoint.1
        ));
        ctx.pi_provider = prov.clone();
        ctx.pi_model = model.clone();
    }

    // ---- argv + env ---------------------------------------------------------- #
    let args = build_pi_argv(ctx, task, system_md);
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&ctx.repo);
    apply_pi_env(ctx, &mut cmd);

    // ---- spawn (Popen, new process group, piped, hidden) ------------------- #
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_spawn_flags(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // A spawn failure (missing pi) — Python would raise FileNotFoundError; surface a failed
            // RunOut so the caller treats it like a failed pi session (rc!=0, empty stdout). pi
            // never started, so NO token was spent: nothing is recorded against any ledger.
            ctx.pi_provider = saved_endpoint.0;
            ctx.pi_model = saved_endpoint.1;
            return RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };
    // ---- RSI v3 WIRING 4/4 (spend side): a token is being spent NOW ---------- #
    // Recorded after the REAL spawn and before waiting, so a crash/timeout mid-session still
    // counted against both the fleet provider ledger and WS1's per-cycle call budget.
    budget::record_call(ctx, &prov, &model);
    crate::improver::freshness::note_pi_call(ctx);
    let pid = child.id();
    let started_at = crate::improver::ctx::now();
    let started_instant = std::time::Instant::now();
    ctx.heartbeat(json!({
        "pi": pi_heartbeat(PiHeartbeat {
            pid,
            started_at: &started_at,
            elapsed_s: 0,
            timeout_s: timeout,
            provider: &ctx.pi_provider,
            model: &ctx.pi_model,
            status: "running",
            exit_code: None,
        })
    }));
    let pump = start_pi_heartbeat_pump(
        ctx.heartbeat_path.clone(),
        pid,
        started_at.clone(),
        started_instant,
        timeout,
        ctx.pi_provider.clone(),
        ctx.pi_model.clone(),
    );

    // ---- communicate(timeout) ---------------------------------------------- #
    // Drain BOTH pipes on dedicated threads CONCURRENTLY with the wait. Reading stdout only AFTER
    // wait_timeout (the old post-exit drain()) deadlocked: pi --print --mode json streams a large
    // JSONL event stream, and once it fills the ~64KB OS pipe buffer pi blocks on write() and never
    // exits — so wait_timeout burned the whole timeout and returned a spurious rc=124 with truncated
    // output on essentially every real implement run. Mirrors control::proc::run + CPython
    // communicate() (both read the pipes concurrently). UTF-8 lossy == errors="replace".
    use std::io::Read;
    use wait_timeout::ChildExt;
    let out_h = child.stdout.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let err_h = child.stderr.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
        h.and_then(|h| h.join().ok()).unwrap_or_default()
    };
    let dur = std::time::Duration::from_secs(timeout.max(0) as u64);
    let out = match child.wait_timeout(dur) {
        Ok(Some(status)) => {
            // Normal exit: the write ends are closed, so the readers finish; join for full output.
            pump.stop();
            let code = status.code().unwrap_or(-1);
            let elapsed_s = elapsed_secs(started_instant);
            ctx.heartbeat(json!({
                "pi": pi_heartbeat(PiHeartbeat {
                    pid,
                    started_at: &started_at,
                    elapsed_s,
                    timeout_s: timeout,
                    provider: &ctx.pi_provider,
                    model: &ctx.pi_model,
                    status: "exited",
                    exit_code: Some(code),
                })
            }));
            RunOut {
                code,
                stdout: join(out_h),
                stderr: join(err_h),
            }
        }
        Ok(None) => {
            // TimeoutExpired: kill the whole tree (taskkill /T closes pi's AND the node grandchild's
            // write ends, so the reader threads unblock and the joins below cannot hang), a 20s grace,
            // then join the readers for the partial streams.
            kill_tree(pid);
            let dur20 = std::time::Duration::from_secs(20);
            let _ = child.wait_timeout(dur20); // proc.communicate(timeout=20)
            let _ = child.kill(); // ensure reaped even if the 20s grace also expired
            let out = join(out_h);
            let err = join(err_h);
            // raise subprocess.TimeoutExpired(args, timeout, output=out, stderr=err) — surfaced as a
            // rc=124 failed RunOut carrying the partial streams + the timed-out marker (see fn doc).
            let argv0 = args.first().cloned().unwrap_or_else(|| "pi".to_string());
            let marker = format!("{argv0} timed out after {timeout}s");
            let stderr = if err.is_empty() {
                marker
            } else {
                format!("{err}\n{marker}")
            };
            pump.stop();
            let elapsed_s = elapsed_secs(started_instant);
            ctx.heartbeat(json!({
                "pi": pi_heartbeat(PiHeartbeat {
                    pid,
                    started_at: &started_at,
                    elapsed_s,
                    timeout_s: timeout,
                    provider: &ctx.pi_provider,
                    model: &ctx.pi_model,
                    status: "timed_out",
                    exit_code: Some(124),
                })
            }));
            RunOut {
                code: 124,
                stdout: out,
                stderr,
            }
        }
        Err(e) => {
            // wait itself errored (unusual): kill and DETACH the readers (don't risk a join hang).
            let _ = child.kill();
            drop(out_h);
            drop(err_h);
            pump.stop();
            let elapsed_s = elapsed_secs(started_instant);
            ctx.heartbeat(json!({
                "pi": pi_heartbeat(PiHeartbeat {
                    pid,
                    started_at: &started_at,
                    elapsed_s,
                    timeout_s: timeout,
                    provider: &ctx.pi_provider,
                    model: &ctx.pi_model,
                    status: "wait_error",
                    exit_code: Some(-1),
                })
            }));
            RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            }
        }
    };
    // restore the loop's configured endpoint (a fallback was for THIS call only)
    ctx.pi_provider = saved_endpoint.0;
    ctx.pi_model = saved_endpoint.1;
    // ---- RSI v3 WIRING 4/4 (outcome side): quota parks THIS endpoint, never the fleet ------- #
    // is_quota_error_output inspects BOTH streams (the 2026-07-03 incident put the 429 in stdout's
    // structured errorMessage with empty stderr). A quota outcome escalates the endpoint's
    // exponential park; anything else resets its consecutive-429 streak.
    if is_quota_error_output(&out.stdout, &out.stderr) {
        let until = budget::record_quota(ctx, &prov, &model);
        ctx.log(&format!(
            "provider quota error on {prov}:{model} — parked until {until} (per-endpoint exponential backoff)"
        ));
    } else {
        budget::record_success(ctx, &prov, &model);
    }
    out
}

struct PiHeartbeat<'a> {
    pid: u32,
    started_at: &'a str,
    elapsed_s: i64,
    timeout_s: i64,
    provider: &'a str,
    model: &'a str,
    status: &'a str,
    exit_code: Option<i32>,
}

fn pi_heartbeat(hb: PiHeartbeat<'_>) -> Value {
    json!({
        "pid": hb.pid,
        "started_at": hb.started_at,
        "elapsed_s": hb.elapsed_s,
        "timeout_s": hb.timeout_s,
        "provider": hb.provider,
        "model": hb.model,
        "status": hb.status,
        "exit_code": hb.exit_code,
    })
}

struct PiHeartbeatPump {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PiHeartbeatPump {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn start_pi_heartbeat_pump(
    heartbeat_path: PathBuf,
    pid: u32,
    started_at: String,
    started_instant: std::time::Instant,
    timeout_s: i64,
    provider: String,
    model: String,
) -> PiHeartbeatPump {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let handle = std::thread::spawn(move || {
        while !stop_thread.load(Ordering::Relaxed) {
            for _ in 0..30 {
                if stop_thread.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if stop_thread.load(Ordering::Relaxed) {
                return;
            }
            update_pi_heartbeat_file(
                &heartbeat_path,
                pi_heartbeat(PiHeartbeat {
                    pid,
                    started_at: &started_at,
                    elapsed_s: elapsed_secs(started_instant),
                    timeout_s,
                    provider: &provider,
                    model: &model,
                    status: "running",
                    exit_code: None,
                }),
            );
        }
    });
    PiHeartbeatPump {
        stop,
        handle: Some(handle),
    }
}

fn update_pi_heartbeat_file(path: &Path, pi: Value) {
    let text = std::fs::read_to_string(path).unwrap_or_else(|_| "{}".to_string());
    let mut root = serde_json::from_str::<Value>(&text).unwrap_or_else(|_| json!({}));
    if let Value::Object(obj) = &mut root {
        obj.insert("pi".to_string(), pi);
        obj.insert("updated_at".to_string(), json!(crate::improver::ctx::now()));
    }
    let next = serde_json::to_string_pretty(&root).unwrap_or_else(|_| "{}".to_string());
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, next.as_bytes()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn elapsed_secs(started_at: std::time::Instant) -> i64 {
    started_at.elapsed().as_secs().min(i64::MAX as u64) as i64
}

/// hidden_subprocess_kwargs(new_group=True): CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP on Windows
/// (so taskkill /T can reach the node grandchild); a no-op off Windows (Python sets no preexec_fn /
/// start_new_session, so the child stays in the parent's group there).
#[cfg(windows)]
pub(crate) fn apply_spawn_flags(cmd: &mut Command) {
    cmd.creation_flags(proc::hidden_flags(false, true));
}
#[cfg(not(windows))]
pub(crate) fn apply_spawn_flags(_cmd: &mut Command) {}

// --------------------------------------------------------------------------- #
// final_text
// --------------------------------------------------------------------------- #

/// run_improver.final_text (~1083-1106): extract the LAST assistant text from a `pi --mode json`
/// event stream (JSONL). For each non-empty line: parse JSON (skip on decode error). If
/// `type == "agent_end"` use `messages` (or []); elif `message` is a dict use `[message]`; else skip.
/// For each msg with `role == "assistant"`, concatenate every `content[i].text` where
/// `content[i].type == "text"` (no separator). The LAST such non-empty text wins. Returns `.strip()`.
pub fn final_text(stdout: &str) -> String {
    let mut final_t = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // json.JSONDecodeError -> skip
        };
        // msgs selection
        let msgs: Vec<Value> = if ev.get("type").and_then(Value::as_str) == Some("agent_end") {
            // ev.get("messages") or []
            match ev.get("messages") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            }
        } else if matches!(ev.get("message"), Some(Value::Object(_))) {
            // isinstance(ev.get("message"), dict) -> [ev["message"]]
            vec![ev.get("message").cloned().unwrap_or(Value::Null)]
        } else {
            continue;
        };
        for m in &msgs {
            if !m.is_object() {
                continue; // isinstance(m, dict)
            }
            if m.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            // "".join(part["text"] for part in (m["content"] or []) if part.type=="text")
            let mut t = String::new();
            if let Some(Value::Array(content)) = m.get("content") {
                for part in content {
                    if !part.is_object() {
                        continue;
                    }
                    if part.get("type").and_then(Value::as_str) == Some("text") {
                        // part.get("text", "")
                        t.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""));
                    }
                }
            }
            if !t.is_empty() {
                final_t = t; // last text wins
            }
        }
    }
    final_t.trim().to_string()
}

// --------------------------------------------------------------------------- #
// _phase_run_pi
// --------------------------------------------------------------------------- #

/// run_improver._phase_run_pi (~2089-2100): run a one-off pi call for an in-process pipeline phase
/// (plan/review/ideate/reflect) on THAT phase's configured provider/model/reasoning, then RESTORE the
/// loop's (implement) config. Saves the five phase-config globals (PHASE, PI_PROVIDER, PI_MODEL,
/// PI_EXT, REASONING), sets PHASE=phase, applies the per-phase config, runs run_pi, and restores all
/// five in the equivalent of a `finally:` (here: an explicit save/restore around the run, since the
/// run_pi call cannot panic-unwind a meaningful result — a spawn/timeout becomes a RunOut, so the
/// restore always executes).
pub fn phase_run_pi(
    ctx: &mut Ctx,
    phase: &str,
    task: &str,
    system_md: Option<&Path>,
    timeout: i64,
) -> RunOut {
    // saved = (PHASE, PI_PROVIDER, PI_MODEL, PI_EXT, REASONING)
    let saved_phase = ctx.phase.clone();
    let saved_provider = ctx.pi_provider.clone();
    let saved_model = ctx.pi_model.clone();
    let saved_ext = ctx.pi_ext.clone();
    let saved_reasoning = ctx.reasoning.clone();

    // PHASE = phase; _apply_phase_config()
    ctx.phase = phase.to_string();
    ctx.apply_phase_config(None);
    let result = run_pi(ctx, task, timeout, system_md);

    // finally: PHASE, PI_PROVIDER, PI_MODEL, PI_EXT, REASONING = saved
    ctx.phase = saved_phase;
    ctx.pi_provider = saved_provider;
    ctx.pi_model = saved_model;
    ctx.pi_ext = saved_ext;
    ctx.reasoning = saved_reasoning;

    result
}

// re-export the timeout constants verbatim from the source so callers don't re-derive them.
/// run_pi default timeout (implement). Lowered from 3600s (60 min) to 1800s (30 min) — a wedged
/// implement job should not hold an autopilot slot for an hour; the prior 60-min ceiling (raised
/// for slow free-tier models) starved the fleet on one stuck job. The deep-tier budget
/// (TIMEOUT_DEEP=5400s) still covers genuinely large architecture slices.
pub const TIMEOUT_IMPLEMENT: i64 = 1800;
/// deep-tier (architecture / ordered [campaign]) implement timeout — a genuinely large, multi-file
/// slice needs more than the standard wall before it is killed and reverted to a noop (the 3600s
/// guillotine). run_improver had no equivalent; this is Solomon's deep-work budget lever.
pub const TIMEOUT_DEEP: i64 = 5400;
/// beautify pass timeout. run_improver: `run_pi(..., timeout=900)` (skips the gate).
pub const TIMEOUT_BEAUTIFY: i64 = 900;
/// decompose/review/ideate/provision one-off timeout. run_improver: `timeout=600`.
pub const TIMEOUT_PHASE_600: i64 = 600;
/// plan/reflect one-off timeout. run_improver: `_phase_run_pi(..., timeout=400)`.
pub const TIMEOUT_PHASE_400: i64 = 400;

// --------------------------------------------------------------------------- #
// tests
// --------------------------------------------------------------------------- #

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- final_text: agent_end.messages form, last-wins, text concat ----
    #[test]
    fn final_text_agent_end_last_assistant_wins() {
        // Two assistant texts across two agent_end events: the LAST one wins; text parts concat
        // with no separator; non-text parts skipped.
        let line1 = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "first "},
                    {"type": "tool_use", "text": "IGNORED"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        }))
        .unwrap();
        let line2 = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "  final summary —"}
                ]}
            ]
        }))
        .unwrap();
        let stdout = format!("{line1}\n{line2}\n");
        // last wins, then .strip() trims the leading/trailing whitespace; em-dash preserved.
        assert_eq!(final_text(&stdout), "final summary —");
    }

    #[test]
    fn final_text_message_dict_form() {
        // A streamed event with a bare `message` dict (not agent_end) is also harvested.
        let line = serde_json::to_string(&json!({
            "type": "message",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "hi"}]}
        }))
        .unwrap();
        assert_eq!(final_text(&line), "hi");
    }

    #[test]
    fn final_text_skips_noise_and_non_assistant() {
        // Blank lines, undecodable lines, non-assistant roles, and events without messages/message
        // are all skipped; empty assistant text does not overwrite a prior good one.
        let good = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [{"role": "assistant", "content": [{"type": "text", "text": "keep me"}]}]
        }))
        .unwrap();
        let user = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [{"role": "user", "content": [{"type": "text", "text": "user text"}]}]
        }))
        .unwrap();
        let empty_assistant = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [{"role": "assistant", "content": [{"type": "text", "text": ""}]}]
        }))
        .unwrap();
        let stdout =
            format!("\n   \nnot json at all\n{good}\n{{ truncated\n{user}\n{empty_assistant}\n");
        // good is kept; user role ignored; empty-text assistant does NOT clobber.
        assert_eq!(final_text(&stdout), "keep me");
    }

    #[test]
    fn final_text_empty_when_no_assistant_text() {
        assert_eq!(final_text(""), "");
        assert_eq!(final_text("{}\n[]\n\"x\"\n"), "");
        // agent_end with no messages key -> [] -> nothing.
        let no_msgs = serde_json::to_string(&json!({"type": "agent_end"})).unwrap();
        assert_eq!(final_text(&no_msgs), "");
    }

    #[test]
    fn final_text_content_missing_text_key_defaults_empty() {
        // part with type=text but no "text" key -> part.get("text","") == "" (no panic).
        let line = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [{"role": "assistant", "content": [
                {"type": "text"},
                {"type": "text", "text": "tail"}
            ]}]
        }))
        .unwrap();
        assert_eq!(final_text(&line), "tail");
    }

    #[test]
    fn pi_heartbeat_payload_exposes_live_child_state() {
        let payload = pi_heartbeat(PiHeartbeat {
            pid: 4242,
            started_at: "2026-06-28T04:00:00Z",
            elapsed_s: 31,
            timeout_s: 3600,
            provider: "openrouter",
            model: "openrouter/owl-alpha",
            status: "running",
            exit_code: None,
        });
        assert_eq!(payload["pid"], 4242);
        assert_eq!(payload["started_at"], "2026-06-28T04:00:00Z");
        assert_eq!(payload["elapsed_s"], 31);
        assert_eq!(payload["timeout_s"], 3600);
        assert_eq!(payload["provider"], "openrouter");
        assert_eq!(payload["model"], "openrouter/owl-alpha");
        assert_eq!(payload["status"], "running");
        assert!(payload["exit_code"].is_null());
    }

    // ---- is_quota_error: provider 429 / rate-limit / quota detection ----
    #[test]
    fn is_quota_error_detects_429_session_usage_limit() {
        // The exact Ollama 429 body that caused the fleet-wide noop storm.
        let stderr = "HTTP 429: you (cayleb_james) have reached your session usage limit";
        assert!(is_quota_error(stderr));
    }

    #[test]
    fn is_quota_error_detects_weekly_usage_limit() {
        // 2026-07-03: Ollama's actual weekly-cap body (verified live against
        // https://ollama.com/v1/chat/completions) — "session usage limit" alone missed this wording,
        // letting the account's WEEKLY quota exhaustion masquerade as a plain model noop across every
        // ollama-cloud lane (daedulus/dotz/maki/solomon) and thrash into noop_streak escalations.
        let stderr = "{\"error\":\"you (cayleb_james) have reached your weekly usage limit, \
add extra usage: https://ollama.com/settings (ref: c708135e-d4a9-484f-ac43-024780b99271)\"}";
        assert!(is_quota_error(stderr));
    }

    #[test]
    fn is_quota_error_detects_various_rate_limit_phrasings() {
        assert!(is_quota_error("Error: 429 Too Many Requests"));
        assert!(is_quota_error("rate limit exceeded — try again in 60s"));
        assert!(is_quota_error("RATE_LIMIT: quota exceeded"));
        assert!(is_quota_error("rate-limit: back off"));
        assert!(is_quota_error("rate_limit: back off"));
        assert!(is_quota_error("session usage limit reached"));
    }

    #[test]
    fn is_quota_error_case_insensitive() {
        assert!(is_quota_error("429 TOO MANY REQUESTS"));
        assert!(is_quota_error("Rate Limit Exceeded"));
        assert!(is_quota_error("QUOTA EXCEEDED"));
    }

    #[test]
    fn is_quota_error_false_on_normal_output() {
        assert!(!is_quota_error(""));
        assert!(!is_quota_error("Pi completed successfully"));
        assert!(!is_quota_error("some unrelated network error"));
        assert!(!is_quota_error("Error: connection refused"));
        // A model summary that happens to mention "429" in a coding context is in STDOUT, not
        // stderr — is_quota_error checks stderr only, so it won't false-positive.
        assert!(!is_quota_error("the agent wrote a retry handler"));
    }

    // ---- stream_error_messages / is_quota_error_output: 2026-07-03 incident ----
    // pi's --mode json puts a provider transport error in stdout's structured errorMessage field,
    // NOT stderr (verified live: a direct pi invocation against the exhausted account reproduced
    // this exact shape, empty stderr included).
    #[test]
    fn stream_error_messages_extracts_from_message_dict_stopreason_error() {
        // Byte-shape of the actual event pi emitted (trimmed to the fields that matter).
        let line = serde_json::to_string(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant", "content": [], "provider": "maki-cloud", "model": "glm-5.2",
                "stopReason": "error",
                "errorMessage": "429 \"you (cayleb_james) have reached your weekly usage limit, add extra usage: https://ollama.com/settings (ref: 33e5c065-97ae-4aea-b475-60fa926f5ef3)\""
            }
        })).unwrap();
        let extracted = stream_error_messages(&line);
        assert!(extracted.contains("weekly usage limit"), "{extracted}");
        assert!(is_quota_error(&extracted));
    }

    #[test]
    fn stream_error_messages_extracts_from_agent_end_messages() {
        let line = serde_json::to_string(&json!({
            "type": "agent_end",
            "messages": [{
                "role": "assistant", "content": [],
                "stopReason": "error", "errorMessage": "429 rate limit exceeded"
            }]
        })).unwrap();
        assert!(is_quota_error(&stream_error_messages(&line)));
    }

    #[test]
    fn stream_error_messages_ignores_non_error_and_missing_fields() {
        // No stopReason -> nothing extracted, even with content.
        let ok = serde_json::to_string(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "done"}]}
        })).unwrap();
        assert_eq!(stream_error_messages(&ok), "");
        // stopReason present but not "error" -> ignored.
        let stopped = serde_json::to_string(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [], "stopReason": "stop"}
        })).unwrap();
        assert_eq!(stream_error_messages(&stopped), "");
        // Blank/undecodable lines are skipped without panicking.
        assert_eq!(stream_error_messages("\n   \nnot json\n{ truncated\n"), "");
    }

    #[test]
    fn is_quota_error_output_catches_stdout_stream_error_when_stderr_is_empty() {
        // The full 2026-07-03 incident shape: empty stderr (as pi actually produced), quota error
        // only reachable via the stdout JSONL stream.
        let stdout = serde_json::to_string(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant", "content": [], "stopReason": "error",
                "errorMessage": "429 \"you (cayleb_james) have reached your weekly usage limit, add extra usage: https://ollama.com/settings\""
            }
        })).unwrap();
        assert!(is_quota_error_output(&stdout, ""));
    }

    #[test]
    fn is_quota_error_output_false_on_genuine_empty_noop() {
        // A real "no changes" noop: empty stdout, empty stderr — must NOT be misclassified as quota.
        assert!(!is_quota_error_output("", ""));
        // Normal assistant text content, no error field — still not a quota error.
        let stdout = serde_json::to_string(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [{"type": "text", "text": "no changes needed"}]}
        })).unwrap();
        assert!(!is_quota_error_output(&stdout, ""));
    }

    // ---- timeout constant parity with the source ----
    #[test]
    fn timeout_constants_match_source() {
        assert_eq!(TIMEOUT_IMPLEMENT, 1800);
        assert_eq!(TIMEOUT_BEAUTIFY, 900);
        assert_eq!(TIMEOUT_PHASE_600, 600);
        assert_eq!(TIMEOUT_PHASE_400, 400);
    }

    // ---- resolve_pi_invocation: Windows npm batch shim -> node <cli.js> bypass ----
    // Regression guard for the CVE-2024-24576 batch-spawn bug: a `.cmd` pi shim must resolve to
    // (node, [cli.js]) so multi-line tasks spawn; the PATHEXT `.JS` line must NOT be mistaken for the
    // entry. (Windows-only — node_cli_from_shim is cfg(windows).)
    #[cfg(windows)]
    #[test]
    fn node_cli_from_shim_extracts_js_entry() {
        let base = std::env::temp_dir().join(format!("solomon_pishim_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cli = base
            .join("node_modules")
            .join("@scope")
            .join("pkg")
            .join("dist")
            .join("cli.js");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        std::fs::write(&cli, "// entry").unwrap();
        let shim = base.join("pi.cmd");
        // npm-shim shape: the PATHEXT `.JS` mention precedes the node_modules launch line.
        std::fs::write(
            &shim,
            "@ECHO off\r\nSET PATHEXT=%PATHEXT:;.JS;=;%\r\n\"%_prog%\"  \"%dp0%\\node_modules\\@scope\\pkg\\dist\\cli.js\" %*\r\n",
        )
        .unwrap();
        let shim_s = shim.to_string_lossy().into_owned();
        let got = node_cli_from_shim(&shim_s).expect("js entry resolved");
        assert_eq!(
            PathBuf::from(&got),
            cli,
            "must extract the node_modules cli.js, not PATHEXT .JS"
        );
        let (prog, lead) = resolve_pi_invocation(&shim_s);
        assert_eq!(lead, vec![got], "lead arg is the resolved cli.js");
        assert!(
            prog.to_lowercase().contains("node"),
            "program is node, got {prog}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- agent_shim_dir: exact error strings + 4 verbs ----
    #[test]
    fn agent_shim_dir_writes_exact_block_strings() {
        let mut c = Ctx::configure("C:/nonexistent/repo", "shimtest", "ollama-cloud", None);
        c.runtime =
            std::env::temp_dir().join(format!("solomon_pi_shimtest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&c.runtime);

        let d = agent_shim_dir(&c).expect("shim dir created");
        // POSIX gh shim: the verbose "read-only ... gh CLI ... runner owns GitHub" string.
        let gh = std::fs::read_to_string(d.join("gh")).unwrap();
        assert!(gh.contains(
            "blocked by Solomon: use the read-only github_* tools, not the gh CLI (the runner owns GitHub)"
        ));
        assert!(gh.starts_with("#!/bin/sh\n"));
        // Windows gh.cmd: the SHORTER string (no "read-only", no "runner owns GitHub").
        let ghcmd = std::fs::read_to_string(d.join("gh.cmd")).unwrap();
        assert!(ghcmd.contains("blocked by Solomon: use the github_* tools, not gh"));
        assert!(!ghcmd.contains("read-only"));

        // git shims exist only if an absolute git is on PATH (the dev/CI host has one).
        if let Some(p) = proc::which_git() {
            if p.is_absolute() {
                let gitsh = std::fs::read_to_string(d.join("git")).unwrap();
                // POSIX git shim interpolates the verb via $1 and blocks the 4 VC verbs.
                assert!(gitsh.contains("push|pull|merge|rebase)"));
                assert!(gitsh.contains(
                    "blocked by Solomon: the runner owns version control (no git $1 in the agent)"
                ));
                // ...and the branch-switching verbs (the 2026-06-24 `checkout -b` escape vector).
                assert!(gitsh.contains(
                    "switch) echo \"blocked by Solomon: the runner owns branches (no git switch in the agent)\""
                ));
                assert!(gitsh.contains(
                    "-b|-B) echo \"blocked by Solomon: the runner owns branches (no git checkout -b in the agent)\""
                ));
                assert!(gitsh.contains(
                    "blocked by Solomon: the runner owns branches \
(use git checkout -- <file> to discard, no branch switch in the agent)"
                ));
                assert!(gitsh.contains(
                    "blocked by Solomon: the runner owns branches (no git branch <name> in the agent)"
                ));
                // File-restore / list forms PASS THROUGH: the empty/`.`/`-*` $2 arms exec real git.
                assert!(gitsh.contains("\"\"|.|-*) exec")); // checkout -- <file>, checkout .
                assert!(gitsh.contains("\"\"|-*) exec")); //   branch (list), branch -a/-d/...

                let gitcmd = std::fs::read_to_string(d.join("git.cmd")).unwrap();
                // Windows git.cmd: /I checks for each of the 4 VC verbs -> :blk (msg does NOT name verb).
                for v in ["push", "pull", "merge", "rebase"] {
                    assert!(gitcmd.contains(&format!("if /I \"%~1\"==\"{v}\" goto blk")));
                }
                // Source emits the VC message with the cmd stderr redirect.
                assert!(
                    gitcmd.contains("blocked by Solomon: the runner owns version control 1>&2\r\n")
                );
                // Branch-switching verbs dispatch to :blkbr (switch directly; checkout/branch via sub-labels).
                assert!(gitcmd.contains("if /I \"%~1\"==\"switch\" goto blkbr"));
                assert!(gitcmd.contains("if /I \"%~1\"==\"checkout\" goto chk"));
                assert!(gitcmd.contains("if /I \"%~1\"==\"branch\" goto br"));
                assert!(gitcmd.contains("if /I \"%~2\"==\"-b\" goto blkbr"));
                assert!(gitcmd.contains("if /I \"%~2\"==\"-B\" goto blkbr"));
                // File-restore / list forms forward to :run (real git): empty, `.`, or a `-`-led flag.
                assert!(gitcmd.contains("if \"%~2\"==\".\" goto run"));
                assert!(gitcmd.contains("if \"%a2:~0,1%\"==\"-\" goto run"));
                assert!(gitcmd.contains("if \"%b2:~0,1%\"==\"-\" goto run"));
                assert!(gitcmd.contains("blocked by Solomon: the runner owns branches 1>&2\r\n"));
                // Windows messages never interpolate a POSIX `$1`.
                assert!(!gitcmd.contains("$1"));
            }
        }
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    // ---- phase_run_pi restores config even though the pi spawn fails (no real pi on the test host) ----
    #[test]
    fn phase_run_pi_restores_phase_config() {
        let mut c = Ctx::configure("C:/nonexistent/repo", "phasetest", "ollama-cloud", None);
        c.runtime =
            std::env::temp_dir().join(format!("solomon_pi_phasetest_{}", std::process::id()));
        c.phase = "implement".to_string();
        c.pi_model = "glm-5.2".to_string();
        c.reasoning = "xhigh".to_string();
        let saved_phase = c.phase.clone();
        let saved_model = c.pi_model.clone();
        let saved_reasoning = c.reasoning.clone();
        let saved_provider = c.pi_provider.clone();
        let saved_ext = c.pi_ext.clone();

        // pi exe almost certainly absent on the host -> run_pi returns a failed RunOut; the point is
        // the phase config is restored afterwards regardless.
        let _ = phase_run_pi(&mut c, "review", "do a review", None, TIMEOUT_PHASE_600);

        assert_eq!(c.phase, saved_phase, "PHASE restored");
        assert_eq!(c.pi_model, saved_model, "PI_MODEL restored");
        assert_eq!(c.reasoning, saved_reasoning, "REASONING restored");
        assert_eq!(c.pi_provider, saved_provider, "PI_PROVIDER restored");
        assert_eq!(c.pi_ext, saved_ext, "PI_EXT restored");
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    // ---- RSI v3 budget wiring: both refusals return BEFORE any spawn (no pi, no token) ----

    fn unix_now_test() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Isolated wiring-test Ctx: own control dir (repos.json without a fallback) and an isolated
    /// runtime whose PARENT is the fleet dir the provider ledger lives in.
    fn wiring_ctx(tag: &str) -> Ctx {
        let base = std::env::temp_dir().join(format!(
            "solomon_pi_wire_{tag}_{}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        ));
        let control = base.join("control");
        let repo = base.join("repo");
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::create_dir_all(&repo);
        std::fs::write(control.join("repos.json"), r#"[{"name": "wiretest"}]"#).unwrap();
        let mut c = Ctx::configure(&repo.to_string_lossy(), "wiretest", "ollama-cloud", None);
        c.control = control;
        c.runtime = base.join("runtime").join("wiretest");
        c.heartbeat_path = c.runtime.join("heartbeat.json");
        c.log_path = c.runtime.join("improver.log");
        c.stop_path = c.runtime.join("stop");
        let _ = std::fs::create_dir_all(&c.runtime);
        c
    }

    #[test]
    fn run_pi_parked_endpoint_returns_synthesized_429_without_spawn() {
        let mut c = wiring_ctx("parked");
        let now = unix_now_test();
        // park the PRIMARY endpoint (ollama-cloud defaults: maki-cloud:glm-5.2) in the fleet ledger
        let fleet = c.runtime.parent().unwrap().to_path_buf();
        let _ = std::fs::create_dir_all(&fleet);
        let ledger = json!({"endpoints": {"maki-cloud:glm-5.2": {
            "window_cap_calls": 500, "window_started": now, "spent_calls": 1,
            "park_until": now + 3600, "consecutive_429": 2, "last_canary_pass": 0}}});
        std::fs::write(fleet.join("_provider_budget.json"), ledger.to_string()).unwrap();

        let out = run_pi(&mut c, "do work", 60, None);
        assert_eq!(out.code, 1);
        assert_eq!(out.stdout, "");
        assert!(
            out.stderr.starts_with("429 provider parked by budget ledger until"),
            "got: {}",
            out.stderr
        );
        assert!(out.stderr.contains("(no token spent)"));
        // the synthesized refusal MUST ride the existing quota classification (never a noop)
        assert!(is_quota_error(&out.stderr));
        assert!(is_quota_error_output(&out.stdout, &out.stderr));
        // no spawn happened => no call was recorded against the parked endpoint
        let after: Value = serde_json::from_str(
            &std::fs::read_to_string(fleet.join("_provider_budget.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(after["endpoints"]["maki-cloud:glm-5.2"]["spent_calls"], json!(1));
        // the loop's configured endpoint is untouched by the refusal
        assert_eq!(c.pi_provider, "maki-cloud");
        assert_eq!(c.pi_model, "glm-5.2");
        let _ = std::fs::remove_dir_all(c.runtime.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn run_pi_cycle_budget_exhausted_returns_timeout_shaped_124() {
        let mut c = wiring_ctx("cycle");
        // WS1's per-cycle budget window, already blown: started 100s ago with a 10s wall cap
        let now = unix_now_test();
        let budget_file = json!({"started_at": (now as f64) - 100.0, "wall_s": 10.0, "pi_calls": 0});
        std::fs::write(c.runtime.join("cycle_budget.json"), budget_file.to_string()).unwrap();

        let out = run_pi(&mut c, "do work", 60, None);
        // rc=124 is run_pi's timeout convention — callers land in their existing note_timeout path
        assert_eq!(out.code, 124);
        assert_eq!(out.stdout, "");
        assert!(
            out.stderr.starts_with("cycle budget exhausted:"),
            "got: {}",
            out.stderr
        );
        assert!(out.stderr.contains("wall budget exhausted"), "got: {}", out.stderr);
        // refused BEFORE endpoint resolution: no fleet ledger was even seeded
        assert!(
            !c.runtime.parent().unwrap().join("_provider_budget.json").exists(),
            "cycle-budget refusal must precede any provider-ledger IO"
        );
        let _ = std::fs::remove_dir_all(c.runtime.parent().unwrap().parent().unwrap());
    }
}
