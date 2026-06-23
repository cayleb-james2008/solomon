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
    let mut cmd = build(args, cwd);
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

/// Atomic JSON write: serialize like json.dump(indent=2), write `<path>.tmp`, rename over `path`.
///
/// ponytail: serde emits raw UTF-8 where Python's ensure_ascii=True emits `\uXXXX`. Both parse
/// identically, so repos.json round-trips correctly; the golden-diff parses-then-compares rather
/// than byte-compares. Swap in an ASCII-escaping formatter only if byte-identical output is required.
pub fn atomic_write_json(path: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp"); // control appends ".tmp" to the full name (repos.json.tmp), not an ext swap
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &body)?;
    std::fs::rename(&tmp, path)
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

    // The timeout branch must drain pipes concurrently: a child emitting far more than the ~64KB OS
    // pipe buffer must NOT block-on-write and time out. The old post-exit drain returned Err(TimedOut)
    // here (and lost the output); the thread-drain captures it all and exits fast.
    #[cfg(windows)]
    #[test]
    fn run_timeout_drains_large_output_without_deadlock() {
        // `for /L` emits ~8000 * 28-byte lines (~230KB) to stdout, well past one pipe buffer.
        let out = run(
            &["cmd", "/c", "for /L %i in (1,1,8000) do @echo XXXXXXXXXXXXXXXXXXXXXXXXXX"],
            None,
            Some(Duration::from_secs(30)),
        )
        .expect("timeout branch must not error on large output");
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
