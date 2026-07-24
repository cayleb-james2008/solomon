//! Port of control.py's subprocess foundation: the one `run()` wrapper (control._run), the env
//! scrub (_clean_subenv), Windows window-hiding flags (winproc.hidden_subprocess_kwargs), the
//! atomic JSON write (tmp + os.replace), and which_git/which_gh.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

// winproc.py creation-flag constants.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
pub const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

// control._GH_FALLBACK
const GH_FALLBACK: &str = r"C:\Program Files\GitHub CLI\gh.exe";

/// Result of `run()` — mirrors subprocess.run(text=True, capture_output=True): decoded streams + code.
#[derive(Debug, Clone)]
pub struct RunOut {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl RunOut {
    /// returncode == 0
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// control._clean_subenv applied to a Command: strip GITHUB_TOKEN/GH_TOKEN/PYTHONPATH/PYTHONHOME
/// (forces gh keyring auth + prevents the cross-venv SRE-mismatch crash) and force UTF-8 stdio.
pub fn apply_clean_env(cmd: &mut Command) {
    for k in ["GITHUB_TOKEN", "GH_TOKEN", "PYTHONPATH", "PYTHONHOME"] {
        cmd.env_remove(k);
    }
    cmd.env("PYTHONUTF8", "1").env("PYTHONIOENCODING", "utf-8");
}

/// winproc.hidden_subprocess_kwargs() creation-flags bitmask. No-op (0) off Windows.
#[cfg(windows)]
pub fn hidden_flags(detached: bool, new_group: bool) -> u32 {
    let mut f = CREATE_NO_WINDOW;
    if detached {
        f |= DETACHED_PROCESS;
    }
    if new_group {
        f |= CREATE_NEW_PROCESS_GROUP;
    }
    f
}
#[cfg(not(windows))]
pub fn hidden_flags(_detached: bool, _new_group: bool) -> u32 {
    0
}

#[cfg(windows)]
fn apply_hidden(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}
#[cfg(not(windows))]
fn apply_hidden(_cmd: &mut Command) {}

fn build<S: AsRef<OsStr>>(args: &[S], cwd: Option<&Path>) -> Command {
    let mut cmd = Command::new(args[0].as_ref());
    cmd.args(&args[1..]);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    apply_clean_env(&mut cmd);
    apply_hidden(&mut cmd);
    cmd
}

/// control._run: capture output, hidden window, scrubbed env. `timeout` (seconds) bounds network ops;
/// on expiry the child is killed and Err(ErrorKind::TimedOut) is returned (mirrors TimeoutExpired).
/// A spawn failure (missing exe) returns Err(other) which callers treat like the Python OSError branch.
pub fn run<S: AsRef<OsStr>>(
    args: &[S],
    cwd: Option<&Path>,
    timeout: Option<Duration>,
) -> std::io::Result<RunOut> {
    run_prepared(build(args, cwd), timeout)
}

/// `run` with a small, caller-supplied environment overlay applied after the standard secret/path
/// scrub. This is for non-secret runtime selectors such as `SOVER_PROFILE`; credentials still stay
/// out of tracked config and are inherited through their existing provider-specific paths.
pub fn run_with_env<S: AsRef<OsStr>>(
    args: &[S],
    cwd: Option<&Path>,
    timeout: Option<Duration>,
    env: &[(&str, &str)],
) -> std::io::Result<RunOut> {
    let mut cmd = build(args, cwd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    run_prepared(cmd, timeout)
}

/// Windows-only: run the operator's custom base-gate command via `cmd /C <gate>` with the gate string
/// passed through `raw_arg` so cmd.exe receives it BYTE-FOR-BYTE. Rust's normal arg quoting is NOT
/// cmd.exe's parsing algorithm, so a gate command containing a quoted path-with-spaces (or other
/// cmd metachars) would be re-split/mangled by cmd.exe and spuriously fail the gate — a silent
/// false-RED that pins recovery. raw_arg bypasses Rust's quoting for the payload. Same scrubbed env
/// + hidden window + bounded timeout as `run`.
#[cfg(windows)]
pub fn run_win_shell(
    gate_cmd: &str,
    cwd: Option<&Path>,
    timeout: Option<Duration>,
) -> std::io::Result<RunOut> {
    let mut cmd = Command::new("cmd");
    // `/C` is a normal token; the gate payload is raw so cmd.exe parses it, not Rust.
    cmd.arg("/C").raw_arg(gate_cmd);
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    apply_clean_env(&mut cmd);
    apply_hidden(&mut cmd);
    run_prepared(cmd, timeout)
}

/// Shared spawn + (optional) bounded-timeout capture for a fully-prepared Command.
fn run_prepared(mut cmd: Command, timeout: Option<Duration>) -> std::io::Result<RunOut> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match timeout {
        None => {
            let o = cmd.output()?;
            Ok(RunOut {
                code: o.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            })
        }
        Some(d) => {
            use wait_timeout::ChildExt;
            // Drain stdout/stderr on dedicated threads so a child that fills the ~64KB OS pipe buffer
            // (e.g. `gh pr list --json …statusCheckRollup` on a busy repo) can't block-on-write and
            // never exit — the old post-exit drain would deadlock, spuriously time out, and silently
            // return empty output (an empty PR list). Mirrors CPython communicate(): drain concurrently
            // with the wait, then join the readers.
            let mut child = cmd.spawn()?;
            let out_h = child.stdout.take().map(|mut s| {
                std::thread::spawn(move || {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
            });
            let err_h = child.stderr.take().map(|mut s| {
                std::thread::spawn(move || {
                    let mut buf = String::new();
                    let _ = s.read_to_string(&mut buf);
                    buf
                })
            });
            let join = |h: Option<std::thread::JoinHandle<String>>| -> String {
                h.and_then(|h| h.join().ok()).unwrap_or_default()
            };
            match child.wait_timeout(d)? {
                Some(status) => Ok(RunOut {
                    code: status.code().unwrap_or(-1),
                    stdout: join(out_h),
                    stderr: join(err_h),
                }),
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Do NOT join the reader threads here. child.kill() is TerminateProcess on the
                    // DIRECT child only (we set just CREATE_NO_WINDOW — no job object / process group),
                    // so a surviving grandchild that inherited the write handle can hold the pipe open;
                    // joining would then block read_to_string forever and turn this prompt timeout into
                    // an INVISIBLE HANG — the exact failure the timeout exists to prevent. Detach the
                    // readers (drop the handles) and return promptly, as the pre-drain code did.
                    // ponytail: at most a couple parked reader threads per (rare) timeout; a prompt
                    // FAILED result beats a hung orchestrator. Upgrade to a job-object tree-kill only if
                    // leaked readers ever actually bite.
                    drop(out_h);
                    drop(err_h);
                    Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "subprocess timed out",
                    ))
                }
            }
        }
    }
}

// --------------------------------------------------------------------------- #
// app job object — bind spawned improver children to the GUI process lifetime
// --------------------------------------------------------------------------- #

/// Create THIS process's kill-on-close job object. Call ONCE at GUI startup (run_gui). Idempotent.
/// Children later passed to [`bind_to_app_job`] (and, via Windows nested jobs, all their descendants)
/// are terminated by the OS when this process's last handle to the job closes — i.e. when the GUI
/// exits, whether by normal close, panic, or TerminateProcess/taskkill. Headless subcommands
/// (`run-improver`, `watchdog`) never call this, so their spawns stay unbounded BY DESIGN: the
/// watchdog must be able to restart loops that outlive a single sweep.
#[cfg(windows)]
pub fn init_app_job() {
    app_job::init();
}
#[cfg(not(windows))]
pub fn init_app_job() {}

/// Assign a freshly-spawned child to the GUI job so it dies with the app. No-op when [`init_app_job`]
/// was never called (headless subcommands) or the job could not be created. Best-effort: an assign
/// failure leaves the child running unbounded rather than failing the spawn.
#[cfg(windows)]
pub fn bind_to_app_job(child: &std::process::Child) {
    app_job::assign(child);
}
#[cfg(not(windows))]
pub fn bind_to_app_job(_child: &std::process::Child) {}

#[cfg(windows)]
mod app_job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // The job handle stored as isize so the OnceLock is Send+Sync (a raw HANDLE is not). Set once in
    // the GUI process and never closed by us; the OS closes it at process exit, which (with
    // KILL_ON_JOB_CLOSE armed) terminates every assigned child. 0 means "no usable job".
    static JOB: OnceLock<isize> = OnceLock::new();

    pub fn init() {
        JOB.get_or_init(create_kill_on_close_job);
    }

    /// Create a job object with KILL_ON_JOB_CLOSE armed; returns the raw HANDLE as isize, or 0 on any
    /// failure (CreateJobObjectW / SetInformationJobObject). Factored out so the FFI is unit-testable.
    fn create_kill_on_close_job() -> isize {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let armed = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if armed == 0 {
                return 0; // could not arm kill-on-close — treat as no job
            }
            job as isize
        }
    }

    pub fn assign(child: &Child) {
        let Some(&raw) = JOB.get() else { return };
        if raw == 0 {
            return;
        }
        // Nested jobs (Win8+) let this succeed even if the child is already in a job; a failure here
        // (e.g. the child already exited) is non-fatal — there is nothing to clean up. ponytail:
        // microsecond race between spawn and assign — if the GUI is killed in that window one child
        // may orphan; CREATE_SUSPENDED+resume would close it but isn't worth the complexity.
        unsafe {
            AssignProcessToJobObject(raw as HANDLE, child.as_raw_handle() as HANDLE);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::{Command, Stdio};

        // Exercises the exact FFI the GUI relies on: create+arm a kill-on-close job and assign a real
        // spawned child to it. Catches the silent failure modes — wrong cargo features, bad struct
        // layout, or AssignProcessToJobObject returning ACCESS_DENIED. (Kill-on-PARENT-exit is an OS
        // guarantee once these three calls succeed; it can't be observed without exiting this process.)
        #[test]
        fn job_creates_arms_and_assigns_a_real_child() {
            let raw = create_kill_on_close_job();
            assert_ne!(raw, 0, "create+arm kill-on-close job failed");
            let mut child = Command::new("cmd")
                .args(["/c", "ping -n 30 127.0.0.1 >NUL"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn long-lived child");
            let assigned =
                unsafe { AssignProcessToJobObject(raw as HANDLE, child.as_raw_handle() as HANDLE) };
            let _ = child.kill();
            let _ = child.wait();
            assert_ne!(assigned, 0, "AssignProcessToJobObject failed (likely ACCESS_DENIED)");
        }
    }
}

/// Atomic JSON write: serialize like json.dump(indent=2), write `<path>.tmp`, rename over `path`.
///
/// ponytail: serde emits raw UTF-8 where Python's ensure_ascii=True emits `\uXXXX`. Both parse
/// identically, so repos.json round-trips correctly; the golden-diff parses-then-compares rather
/// than byte-compares. Swap in an ASCII-escaping formatter only if byte-identical output is required.
pub fn atomic_write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    atomic_write_bytes(path, &body)
}

/// Atomic byte write: write to `<path>.tmp.<pid>.<nanos>`, then replace `path`.
///
/// Tries std::fs::rename first (atomic on Unix; on Windows uses MoveFileExW with
/// MOVEFILE_REPLACE_EXISTING). If rename fails because the target is held open by a reader
/// (Windows ERROR_ACCESS_DENIED — the cause of false CONTROLLER DIRTY pages), falls back to
/// std::fs::copy + remove. The copy path is not strictly atomic but the reader always sees
/// either the old file or the full new file (CopyFileExW on NTFS uses copy-on-write at the
/// metadata level). A crash mid-write never leaves a half-written target in either path.
/// Used by atomic_write_json and the CEO backlog writer.
///
/// The `.tmp.<pid>.<nanos>` suffix (vs the legacy single `.tmp`) makes the tempfile UNIQUE per
/// concurrent caller — `cargo test` runs the parallel e2e `onboard_project` test against the SAME
/// `repos.json`, and two threads writing the SAME `<repos.json>.tmp` race (one's `write` overwrites
/// the other's body, the other thread's `rename` then finds the tmp file gone — Windows os error
/// 2 The system cannot find the file specified). Tagging the tmp with pid+nanos keeps each
/// thread safe; the rename target stays the same so atomicity against readers is unaffected.
pub fn atomic_write_bytes(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}.{}", std::process::id(), nanos));
    let tmp = PathBuf::from(tmp);
    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, body)?;
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(_) => {
                // Windows: target held open by reader — fall back to copy-overwrite.
                std::fs::copy(&tmp, path)?;
                let _ = std::fs::remove_file(&tmp);
                Ok(())
            }
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// control._which_git: shutil.which("git"). Memoized: the resolved path is stable for the process
/// lifetime, but get_state probes it several times per repo every 4s — a PATH scan per call is wasted
/// filesystem work. ponytail: cached for the process; a git installed AFTER launch isn't picked up
/// (restart to re-detect) — a non-issue for a desktop orchestrator.
pub fn which_git() -> Option<PathBuf> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| which::which("git").ok()).clone()
}

/// control._which_gh: shutil.which("gh") or the GitHub CLI default install path if present. Memoized
/// for the same reason as which_git (see its note).
pub fn which_gh() -> Option<PathBuf> {
    static GH: OnceLock<Option<PathBuf>> = OnceLock::new();
    GH.get_or_init(|| {
        which::which("gh").ok().or_else(|| {
            let fb = Path::new(GH_FALLBACK);
            fb.exists().then(|| fb.to_path_buf())
        })
    })
    .clone()
}

// --------------------------------------------------------------------------- #
// Test support shared by the two pipe-flood drain tests (tests below + improver::oneshot::tests).
// --------------------------------------------------------------------------- #

/// Path to a ~224KB flood fixture (8000 x 28-byte lines), written atomically once per process.
/// `cmd /c type <fixture>` streams it I/O-bound: still 3.4x the ~64KB pipe buffer (the deadlock
/// trigger the drain tests exist to catch), but its wall time no longer scales with CPU load.
/// The old generator (`cmd /c for /L ... do @echo ...`) interpreted 8000 echo iterations inside
/// cmd.exe — measured 3s on a quiet box vs 58s under fleet load — and blew the 120s deadline plus
/// retry twice on 2026-07-02, false-REDding the base gate and stopping the RSI lane.
#[cfg(all(test, windows))]
pub(crate) fn flood_fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let p = std::env::temp_dir().join("solomon_flood_fixture_224k.txt");
        // Atomic write (temp + rename): concurrent test binaries share the fixed path safely —
        // a reader always sees a complete 224,000-byte file, never a truncation.
        atomic_write_bytes(&p, "XXXXXXXXXXXXXXXXXXXXXXXXXX\r\n".repeat(8000).as_bytes())
            .expect("write flood fixture to temp dir");
        p
    })
}

/// Serializes the two flood tests (control::proc + improver::oneshot share one test binary) so
/// their >64KB floods never run concurrently and compound drain-thread contention under load.
/// Poison-tolerant: a panic in one holder must not cascade-fail the sibling test.
#[cfg(all(test, windows))]
pub(crate) fn flood_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_captures_stdout_and_code() {
        // `cmd /c echo` on Windows; portable enough for the dev/CI host.
        #[cfg(windows)]
        let out = run(&["cmd", "/c", "echo", "hello"], None, None).unwrap();
        #[cfg(not(windows))]
        let out = run(&["echo", "hello"], None, None).unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("hello"));
    }

    #[test]
    fn run_with_env_applies_the_overlay() {
        #[cfg(windows)]
        let out = run_with_env(
            &["cmd", "/c", "echo", "%SOLOMON_PROC_TEST_VALUE%"],
            None,
            None,
            &[("SOLOMON_PROC_TEST_VALUE", "present")],
        )
        .unwrap();
        #[cfg(not(windows))]
        let out = run_with_env(
            &["sh", "-c", "printf '%s' \"$SOLOMON_PROC_TEST_VALUE\""],
            None,
            None,
            &[("SOLOMON_PROC_TEST_VALUE", "present")],
        )
        .unwrap();
        assert_eq!(out.code, 0);
        assert!(out.stdout.contains("present"));
    }

    // The timeout branch must drain pipes concurrently: a child emitting far more than the ~64KB OS
    // pipe buffer must NOT block-on-write and time out. The old post-exit drain returned Err(TimedOut)
    // here (and lost the output); the thread-drain captures it all and exits fast.
    //
    // LOAD TOLERANCE (2026-07-02 deflake, supersedes the 2026-07-01 deadline bump): the flood is
    // `type` of a pre-written ~224KB fixture — I/O-bound, so wall time is load-insensitive. The old
    // CPU-bound `for /L` echo loop's wall time scaled ~18x with fleet contention (3s quiet, 58s
    // loaded) and blew the 120s deadline + retry twice, false-REDding the base gate. Serialized
    // against the sibling oneshot flood test via flood_serial_lock; the retry-once-on-timeout stays
    // as belt-and-braces (a true deadlock still fails both attempts). Assertions unchanged.
    #[cfg(windows)]
    #[test]
    fn run_timeout_drains_large_output_without_deadlock() {
        use std::io::ErrorKind;
        let _serial = flood_serial_lock();
        // 224,000 bytes to stdout, well past one ~64KB pipe buffer.
        let fixture = flood_fixture().to_string_lossy().into_owned();
        let args = &["cmd", "/c", "type", fixture.as_str()];
        let dur = Duration::from_secs(120);
        let out = match run(args, None, Some(dur)) {
            Ok(o) => o,
            Err(e) if e.kind() == ErrorKind::TimedOut => {
                run(args, None, Some(dur))
                    .expect("timeout branch must not error on large output (after one retry on timeout)")
            }
            Err(e) => panic!("unexpected error: {e}"),
        };
        assert_eq!(out.code, 0);
        assert!(
            out.stdout.len() > 100_000,
            "expected the full large stdout, got {} bytes",
            out.stdout.len()
        );
    }

    #[test]
    fn atomic_write_then_read_roundtrips() {
        let dir = std::env::temp_dir().join("solomon_proc_test");
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("repos.json");
        let v = serde_json::json!([{"name": "x", "interval": 120}]);
        atomic_write_json(&p, &v).unwrap();
        let back: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(back, v);
        // tmp must not linger after a successful rename
        assert!(!dir.join("repos.json.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
