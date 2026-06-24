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
use crate::improver::ctx::Ctx;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
/// (the agent must use the read-only github_* tools) and `git push|pull|merge|rebase`. Read-only git
/// and pi's own internal git PASS THROUGH to the real binary. Prepended to the agent's PATH in
/// run_pi. Returns the dir, or None if it can't be created (best-effort). Both POSIX shell shims and
/// Windows .cmd shims are written into the same dir; only the matching platform's are on PATH-resolve.
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
        // git (POSIX): case-statement; the blocked branch interpolates the verb via `$1`.
        let git_sh = format!(
            "#!/bin/sh\ncase \"$1\" in\n  \
push|pull|merge|rebase) echo \"blocked by Solomon: the runner owns version control \
(no git $1 in the agent)\" >&2; exit 1;;\n  \
*) exec \"{real_git}\" \"$@\";;\nesac\n"
        );
        write_shim(&d.join("git"), &git_sh);
        // git.cmd (Windows): /I case-insensitive verb checks; the blocked message does NOT name the verb.
        let git_cmd = format!(
            "@echo off\r\n\
if /I \"%~1\"==\"push\" goto blk\r\n\
if /I \"%~1\"==\"pull\" goto blk\r\n\
if /I \"%~1\"==\"merge\" goto blk\r\n\
if /I \"%~1\"==\"rebase\" goto blk\r\n\
\"{real_git}\" %*\r\n\
goto :eof\r\n\
:blk\r\necho blocked by Solomon: the runner owns version control 1>&2\r\nexit /b 1\r\n"
        );
        write_shim(&d.join("git.cmd"), &git_cmd);
    }
    Some(d)
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
pub fn run_pi(ctx: &mut Ctx, task: &str, timeout: i64, system_md: Option<&Path>) -> RunOut {
    // ---- argv -------------------------------------------------------------- #
    let pi = ctx.pi_exe();
    let mut args: Vec<String> = vec![
        pi.clone(),
        "--print".into(),
        "--mode".into(),
        "json".into(),
        "-ne".into(),
        "--provider".into(),
        ctx.pi_provider.clone(),
        "--model".into(),
        ctx.pi_model.clone(),
    ];
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

    // ---- env --------------------------------------------------------------- #
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(&ctx.repo);
    // _clean_env(): strip GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME + force UTF-8 stdio.
    ctx.apply_clean_env(&mut cmd);
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

    // ---- spawn (Popen, new process group, piped, hidden) ------------------- #
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_spawn_flags(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // A spawn failure (missing pi) — Python would raise FileNotFoundError; surface a failed
            // RunOut so the caller treats it like a failed pi session (rc!=0, empty stdout).
            return RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };
    let pid = child.id();

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
    match child.wait_timeout(dur) {
        Ok(Some(status)) => {
            // Normal exit: the write ends are closed, so the readers finish; join for full output.
            RunOut {
                code: status.code().unwrap_or(-1),
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
            let argv0 = args
                .first()
                .cloned()
                .unwrap_or_else(|| "pi".to_string());
            let marker = format!("{argv0} timed out after {timeout}s");
            let stderr = if err.is_empty() {
                marker
            } else {
                format!("{err}\n{marker}")
            };
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
            RunOut {
                code: -1,
                stdout: String::new(),
                stderr: e.to_string(),
            }
        }
    }
}

/// hidden_subprocess_kwargs(new_group=True): CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP on Windows
/// (so taskkill /T can reach the node grandchild); a no-op off Windows (Python sets no preexec_fn /
/// start_new_session, so the child stays in the parent's group there).
#[cfg(windows)]
fn apply_spawn_flags(cmd: &mut Command) {
    cmd.creation_flags(proc::hidden_flags(false, true));
}
#[cfg(not(windows))]
fn apply_spawn_flags(_cmd: &mut Command) {}

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
/// run_pi default timeout (implement). run_improver: `def run_pi(task, timeout=1800, ...)`.
pub const TIMEOUT_IMPLEMENT: i64 = 1800;
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
        let stdout = format!(
            "\n   \nnot json at all\n{good}\n{{ truncated\n{user}\n{empty_assistant}\n"
        );
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

    // ---- timeout constant parity with the source ----
    #[test]
    fn timeout_constants_match_source() {
        assert_eq!(TIMEOUT_IMPLEMENT, 1800);
        assert_eq!(TIMEOUT_BEAUTIFY, 900);
        assert_eq!(TIMEOUT_PHASE_600, 600);
        assert_eq!(TIMEOUT_PHASE_400, 400);
    }

    // ---- agent_shim_dir: exact error strings + 4 verbs ----
    #[test]
    fn agent_shim_dir_writes_exact_block_strings() {
        let mut c = Ctx::configure("C:/nonexistent/repo", "shimtest", "ollama-cloud", None);
        c.runtime = std::env::temp_dir().join(format!("solomon_pi_shimtest_{}", std::process::id()));
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
                // POSIX git shim interpolates the verb via $1 and blocks exactly 4 verbs.
                assert!(gitsh.contains("push|pull|merge|rebase)"));
                assert!(gitsh.contains(
                    "blocked by Solomon: the runner owns version control (no git $1 in the agent)"
                ));
                let gitcmd = std::fs::read_to_string(d.join("git.cmd")).unwrap();
                // Windows git.cmd: /I checks for each of the 4 verbs; message does NOT name the verb.
                for v in ["push", "pull", "merge", "rebase"] {
                    assert!(gitcmd.contains(&format!("if /I \"%~1\"==\"{v}\" goto blk")));
                }
                // Source (_agent_shim_dir ~1036) emits the message with the cmd stderr redirect:
                // `echo blocked by Solomon: the runner owns version control 1>&2\r\n`.
                assert!(gitcmd.contains("blocked by Solomon: the runner owns version control 1>&2\r\n"));
                assert!(!gitcmd.contains("$1"));
            }
        }
        let _ = std::fs::remove_dir_all(&c.runtime);
    }

    // ---- phase_run_pi restores config even though the pi spawn fails (no real pi on the test host) ----
    #[test]
    fn phase_run_pi_restores_phase_config() {
        let mut c = Ctx::configure("C:/nonexistent/repo", "phasetest", "ollama-cloud", None);
        c.runtime = std::env::temp_dir().join(format!("solomon_pi_phasetest_{}", std::process::id()));
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
}
