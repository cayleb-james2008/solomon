//! Native Rust port of improver/run_improver.py's `main()` — the runnable `solomon run-improver`
//! subcommand: argparse, configure, one-shot modes (smoke/provision/ideate), base-branch resolution,
//! preflight key/pi/GitHub gates, the single-flight runner lock, and the main loop.
//!
//! Behavior is bug-for-bug with run_improver.main() (source ~3367-end). The Python module-level
//! GLOBALS that main() mutates (SHIP/GATE_CMD/REASONING/GOAL/BEAUTIFY/SOLOMON/INTERVAL/PHASE/
//! BASE_BRANCH/GITHUB_TOOLS/_HALTED) live on [`Ctx`]; main here threads `&mut Ctx`. argparse is a
//! hand-rolled flag parser over `args` (the orchestrator passes `&argv[1..]`, i.e. without the
//! "run-improver" token). Status/phase/reason strings are quoted VERBATIM from the source.
//!
//! The one-shot phases (smoke/provision/ideate) live in [`crate::improver::oneshot`]; the loop body
//! is [`crate::improver::iteration::one_iteration`]; reflect lives in [`crate::improver::phases`].

use crate::control::{locks, paths};
use crate::improver::ctx::{now, Ctx};
use crate::improver::{iteration, oneshot, phases};
use serde_json::{json, Value};
use std::path::Path;

/// run_improver._FALLBACK / PROVIDERS choices for `--provider`. Mirrors `choices=list(PROVIDERS)`.
const PROVIDER_CHOICES: &[&str] = &["ollama-cloud", "openrouter"];
/// `--ship` choices.
const SHIP_CHOICES: &[&str] = &["local", "push", "pr", "auto-merge"];
/// `--reasoning` choices.
const REASONING_CHOICES: &[&str] = &["", "off", "minimal", "low", "medium", "high", "xhigh"];

/// Parsed command-line for run-improver — one field per argparse argument (defaults match main()).
struct Args {
    repo: Option<String>,
    name: Option<String>,
    provider: String,
    model: Option<String>,
    ship: String,
    gate: String,
    pr_target_branch: String,
    max_iterations: i64,
    reasoning: String,
    goal: String,
    once: bool,
    interval: i64,
    smoke: bool,
    beautify: bool,
    provision: bool,
    ideate: bool,
    solomon: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            repo: None,
            name: None,
            provider: "ollama-cloud".to_string(),
            model: None,
            ship: "pr".to_string(),
            gate: String::new(),
            pr_target_branch: String::new(),
            max_iterations: 0,
            reasoning: String::new(),
            goal: String::new(),
            once: false,
            interval: 120,
            smoke: false,
            beautify: false,
            provision: false,
            ideate: false,
            solomon: false,
        }
    }
}

/// Hand-rolled argparse over the run-improver flags. Accepts both `--flag value` and `--flag=value`
/// for value-taking options; the rest are store_true. Validates `choices` like argparse (exiting with
/// code 2 and an stderr message on a bad value / missing required `--repo`). Returns Err(exit_code)
/// to short-circuit main() with that code, matching argparse's `SystemExit(2)`.
fn parse_args(args: &[String]) -> Result<Args, i32> {
    let mut a = Args::default();
    let mut i = 0usize;
    // Pull the value for a `--flag value` / `--flag=value` option. `inline` is Some when "=" was used.
    let take = |i: &mut usize,
                    inline: Option<String>,
                    flag: &str|
     -> Result<String, i32> {
        if let Some(v) = inline {
            return Ok(v);
        }
        *i += 1;
        match args.get(*i) {
            Some(v) => Ok(v.clone()),
            None => {
                eprintln!("error: argument {flag}: expected one argument");
                Err(2)
            }
        }
    };
    fn check_choice(flag: &str, val: &str, choices: &[&str]) -> Result<(), i32> {
        if choices.contains(&val) {
            Ok(())
        } else {
            let q: Vec<String> = choices.iter().map(|c| format!("'{c}'")).collect();
            eprintln!(
                "error: argument {flag}: invalid choice: '{val}' (choose from {})",
                q.join(", ")
            );
            Err(2)
        }
    }
    while i < args.len() {
        let tok = args[i].clone();
        // split --flag=value
        let (flag, inline): (String, Option<String>) = match tok.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (tok.clone(), None),
        };
        match flag.as_str() {
            "--repo" => a.repo = Some(take(&mut i, inline, "--repo")?),
            "--name" => a.name = Some(take(&mut i, inline, "--name")?),
            "--provider" => {
                let v = take(&mut i, inline, "--provider")?;
                check_choice("--provider", &v, PROVIDER_CHOICES)?;
                a.provider = v;
            }
            "--model" => a.model = Some(take(&mut i, inline, "--model")?),
            "--ship" => {
                let v = take(&mut i, inline, "--ship")?;
                check_choice("--ship", &v, SHIP_CHOICES)?;
                a.ship = v;
            }
            "--gate" => a.gate = take(&mut i, inline, "--gate")?,
            "--pr-target-branch" => a.pr_target_branch = take(&mut i, inline, "--pr-target-branch")?,
            "--max-iterations" => {
                let v = take(&mut i, inline, "--max-iterations")?;
                a.max_iterations = match v.trim().parse::<i64>() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!("error: argument --max-iterations: invalid int value: '{v}'");
                        return Err(2);
                    }
                };
            }
            "--reasoning" => {
                let v = take(&mut i, inline, "--reasoning")?;
                check_choice("--reasoning", &v, REASONING_CHOICES)?;
                a.reasoning = v;
            }
            "--goal" => a.goal = take(&mut i, inline, "--goal")?,
            "--once" => a.once = true,
            "--interval" => {
                let v = take(&mut i, inline, "--interval")?;
                a.interval = match v.trim().parse::<i64>() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!("error: argument --interval: invalid int value: '{v}'");
                        return Err(2);
                    }
                };
            }
            "--smoke" => a.smoke = true,
            "--beautify" => a.beautify = true,
            "--provision" => a.provision = true,
            "--ideate" => a.ideate = true,
            "--solomon" => a.solomon = true,
            other => {
                eprintln!("error: unrecognized arguments: {other}");
                return Err(2);
            }
        }
        i += 1;
    }
    if a.repo.is_none() {
        eprintln!("error: the following arguments are required: --repo");
        return Err(2);
    }
    Ok(a)
}

/// `Path(repo).name` — the final path component (argparse default for `--name`). Trailing separators
/// are ignored (Path::file_name semantics match Python's PurePath.name closely enough for repo dirs).
fn path_name(repo: &str) -> String {
    Path::new(repo)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| repo.to_string())
}

/// run_improver.main (~3367-end), ported. `args` is the argv AFTER the "run-improver" token (i.e.
/// `&argv[1..]` from the dispatcher). Returns the process exit code.
pub fn main(args: &[String]) -> i32 {
    let a = match parse_args(args) {
        Ok(a) => a,
        Err(code) => return code,
    };

    // Windows: stdout/stderr default to cp1252; non-cp1252 agent output would UnicodeEncodeError and
    // crash the loop. Rust's println! writes UTF-8 bytes regardless of the console code page, so the
    // crash this guards against cannot occur here — the reconfigure() is a Python-only concern. (No-op.)

    let repo = a.repo.clone().expect("checked in parse_args");
    let name = a.name.clone().unwrap_or_else(|| path_name(&repo));
    let mut ctx = Ctx::configure(&repo, &name, &a.provider, a.model.as_deref());

    // global SHIP, GATE_CMD, REASONING, GOAL, BEAUTIFY, SOLOMON, INTERVAL, PHASE
    ctx.ship = a.ship.clone();
    ctx.gate_cmd = a.gate.clone(); // a.gate or "" (already "" by default)
    ctx.reasoning = a.reasoning.clone(); // a.reasoning or ""
    ctx.goal = a.goal.trim().to_string(); // (a.goal or "").strip()
    ctx.beautify = a.beautify;
    ctx.solomon = a.solomon;
    if ctx.reasoning.is_empty() {
        ctx.reasoning = "xhigh".to_string(); // REASONING = REASONING or "xhigh"
    }
    // PHASE = beautify -> recovery (solomon) -> ideate -> provision -> implement
    ctx.phase = if a.beautify {
        "beautify"
    } else if a.solomon {
        "recovery"
    } else if a.ideate {
        "ideate"
    } else if a.provision {
        "provision"
    } else {
        "implement"
    }
    .to_string();
    ctx.apply_phase_config(None); // per-phase model/provider/reasoning
    ctx.interval = a.interval.max(1); // INTERVAL = max(1, a.interval)

    // beautify + the supervisor fix-session are single-shot
    let mut once = a.once;
    if ctx.beautify || ctx.solomon {
        once = true;
    }

    if a.smoke {
        return oneshot::smoke(&mut ctx);
    }

    ctx.load_env();
    if a.provision {
        return oneshot::provision(&mut ctx);
    }
    if a.ideate {
        return oneshot::ideate(&mut ctx);
    }
    if ctx.git(&["rev-parse", "--is-inside-work-tree"], 120).code != 0 {
        println!("ERROR: not a git repository.");
        return 2;
    }

    // CONTROLLER-CLEAN PREFLIGHT (catalog #6: the control plane exempting itself from its own
    // gates — 601 uncommitted lines on an off-base branch hand-built into production). ONLY when
    // the target repo IS solomon itself: the control plane can never again iterate itself from an
    // uncommitted/off-base state. Managed repos keep their own preflight bails; the healing path
    // for a crash-left dirty tree is the supervisor's RUNG-0 reset, after which the next start
    // passes this gate.
    if ctx.name == "solomon" {
        if let Err(detail) = crate::provenance::controller_clean() {
            ctx.heartbeat(json!({
                "status": "error",
                "phase": "preflight",
                "last_summary": format!("controller tree dirty/off-base — {detail}"),
            }));
            println!("ERROR: controller tree dirty/off-base — {detail}");
            return 2;
        }
    }

    // global BASE_BRANCH
    let pr_target = a.pr_target_branch.trim().to_string();
    ctx.base_branch = if !pr_target.is_empty() {
        pr_target
    } else {
        let head = ctx
            .git(&["rev-parse", "--abbrev-ref", "HEAD"], 120)
            .stdout
            .trim()
            .to_string();
        if head.is_empty() {
            "main".to_string()
        } else {
            head
        }
    };

    // Ensure the base branch exists locally before the loop; bootstrap from origin/<base> only on a
    // SUCCESSFUL fetch (never from a stale tracking ref left by a failed/timed-out fetch).
    let base = ctx.base_branch.clone();
    if ctx
        .git(&["rev-parse", "--verify", "--quiet", &base], 120)
        .code
        != 0
    {
        let mut created = false;
        if ctx.has_remote()
            && ctx.git(&["fetch", "origin", "--quiet"], 120).code == 0
            && ctx
                .git(
                    &["rev-parse", "--verify", "--quiet", &format!("origin/{base}")],
                    120,
                )
                .code
                == 0
        {
            created = ctx
                .git(&["checkout", "-B", &base, &format!("origin/{base}")], 120)
                .code
                == 0;
        }
        if !created {
            ctx.heartbeat(json!({
                "status": "error",
                "last_summary": format!(
                    "Base branch '{base}' does not exist locally or on origin — \
set this repo's PR-target branch to a real branch in Config."
                ),
            }));
            println!("ERROR: base branch '{base}' not found (local or origin).");
            return 2;
        }
    }

    // Load the LIVE per-repo config (provider/model/api_key/gate/...) from repos.json BEFORE the
    // key guards so a repo keyed ONLY per-repo (no global .env key for its provider) is not silently
    // blocked at startup with a misleading "<KEY> not set" error — the same silent-config-drift
    // class already fixed for enrich_contract/ideate via provider_key_ready. refresh_config_from_registry
    // loads api_key and calls apply_api_key (setting the env var), and also lets key_shape_mismatch
    // see the REAL per-repo key at startup instead of only on the first mid-loop re-check. Safe: on
    // a missing/corrupt repos.json or absent repo row it silently keeps the argv-seeded config.
    ctx.refresh_config_from_registry();

    // required provider key
    let key = ctx.required_key();
    if std::env::var(&key).map(|v| v.is_empty()).unwrap_or(true) {
        ctx.heartbeat(json!({
            "status": "error",
            "last_summary": format!("{key} not set — add it to Solomon/.env"),
        }));
        println!("ERROR: {key} not set (Solomon/.env or environment).");
        return 2;
    }

    // provider/api_key shape mismatch (the 2026-06-27/28 owl-alpha-instead-of-glm-5.2 class of bug):
    // refuse to run rather than silently authenticating against a provider repos.json doesn't claim.
    if let Some(reason) = ctx.key_shape_mismatch() {
        ctx.heartbeat(json!({
            "status": "error",
            "last_summary": reason,
        }));
        println!("ERROR: {reason}");
        return 2;
    }

    // pi must be on PATH (else implement crash-loops every iteration)
    if which_pi_missing() {
        ctx.heartbeat(json!({
            "status": "error",
            "last_summary": "pi CLI not found on PATH — install pi / ensure it is on the detached process PATH",
        }));
        println!("ERROR: pi CLI not found on PATH.");
        return 2;
    }

    // GitHub repo: expose read-only github_* tools + verify the connection before iterating-to-ship.
    ctx.github_tools = ctx.has_remote();
    if ctx.github_tools && matches!(ctx.ship.as_str(), "push" | "pr" | "auto-merge") {
        let (ok, why) = ctx.github_ready();
        if !ok {
            ctx.heartbeat(json!({
                "status": "error",
                "last_summary": format!(
                    "GitHub not ready — {why}. Connect GitHub before Solomon iterates this remote repo."
                ),
            }));
            println!("ERROR: GitHub not ready — {why}");
            return 4;
        }
        ctx.log("GitHub connection verified — github_* tools enabled for the agent");
    }

    if !acquire_lock(&mut ctx) {
        println!(
            "Another improver is already running for {} (runtime lock held).",
            ctx.name
        );
        return 3;
    }
    // NOTE on lock-release-on-kill: Python registers atexit + SIGTERM/SIGBREAK handlers that call
    // release_lock(). Rust has no portable atexit, and installing OS signal handlers needs a non-
    // declared crate. The `finally` equivalent below ALWAYS runs release_lock on a normal/once/max-
    // iter/stop exit (the common paths); an external hard-kill (watchdog SIGTERM, console close) is
    // recovered by acquire_lock's dead-pid takeover on the next start, exactly as for a Python crash
    // that skips finally. Behavior on the recoverable paths is identical.

    if ctx.stop_path.exists() {
        let _ = std::fs::remove_file(&ctx.stop_path);
    }

    // _hb["started_at"] = _now()  (bare hb assignment — no heartbeat() freeze/updated_at)
    if let Value::Object(o) = &mut ctx.hb {
        o.insert("started_at".to_string(), json!(now()));
    }
    ctx.heartbeat(json!({"status": "idle", "phase": Value::Null}));
    ctx.log(&format!("Solomon RSI improver started for {}", ctx.name));

    let mut clean_exit = false;
    loop {
        if ctx.stop_path.exists() {
            ctx.log("stop flag set — exiting");
            clean_exit = true;
            break;
        }
        ctx.refresh_config_from_registry();
        // Re-check the provider/api_key shape AFTER the mid-loop refresh: a dashboard (or manual)
        // edit to repos.json can flip `provider` while leaving a stale `api_key` — or vice versa —
        // mid-run. The startup guard (above) only catches this at launch; without this re-check the
        // loop would silently keep iterating against a provider repos.json no longer claims, the
        // exact owl-alpha-class silent drift. Halt with the SAME error heartbeat the startup guard
        // writes (last_summary begins "repos.json api_key for ...") so supervisor.diagnose() surfaces
        // the `key_shape_mismatch` category and escalates, instead of serving the wrong model.
        if let Some(reason) = ctx.key_shape_mismatch() {
            ctx.heartbeat(json!({"status": "error", "last_summary": reason}));
            ctx.log(
                "provider/api_key drifted mid-loop (repos.json edit) — halting for the supervisor \
                 to escalate; fix repos.json (provider vs api_key) and restart",
            );
            clean_exit = true;
            break;
        }
        // run_improver.py wraps the loop body in try/…/finally: an unhandled exception in
        // one_iteration must fall through to the cleanup (release_lock + error/crashed heartbeat),
        // never kill the process with the runner lock still held. catch_unwind restores that
        // (panic=unwind is intentional — see Cargo.toml / the watchdog sweep).
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| iteration::one_iteration(&mut ctx)))
            .is_err()
        {
            ctx.log("one_iteration panicked — treating as a crashed iteration (will be restarted)");
            break; // clean_exit stays false -> finally writes error/crashed and releases the lock
        }
        if ctx.halted {
            ctx.log(
                "halted after an unrecoverable revert failure — operator action required \
(the repo is left at status=error/reverted for the supervisor to escalate)",
            );
            break;
        }
        // REFLECT after a NON-error iteration only (a terminal error heartbeat must keep its
        // diagnostic phase for solomon.diagnose()/monitor.should_restart()).
        if ctx.hb.get("status").and_then(Value::as_str) != Some("error") {
            phases::reflect(&mut ctx);
        }
        if once {
            clean_exit = true;
            break;
        }
        if a.max_iterations != 0
            && ctx.hb.get("iteration").and_then(Value::as_i64).unwrap_or(0) >= a.max_iterations
        {
            ctx.log(&format!(
                "reached max iterations ({}) — exiting",
                a.max_iterations
            ));
            clean_exit = true;
            break;
        }
        // interruptible 1s-granular cooldown
        for _ in 0..a.interval.max(1) {
            if ctx.stop_path.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    // finally: HALT or a PERSISTENT self-stop keep their error heartbeat + STOP sentinel; a deliberate
    // exit reports 'stopped'; a crash (clean_exit==false) reports error/crashed so the watchdog
    // restarts. All three persistent preflight self-stops (dirty/unpushed-base/base-gate-red) wrote an
    // operator-action error heartbeat + pinned a STOP; overwriting them with 'stopped' and deleting
    // the sentinel would erase the diagnostic supervisor.diagnose() surfaces and un-pin the loop —
    // previously only dirty_base_persistent was exempted, silently clobbering the other two.
    let persistent_self_stop = ctx.hb.get("status").and_then(Value::as_str) == Some("error")
        && matches!(
            ctx.hb.get("reason").and_then(Value::as_str),
            Some("dirty_base_persistent" | "unpushed_base_persistent" | "base_gate_red_persistent")
        );
    // A mid-loop key_shape_mismatch halt (above) wrote status=error with a last_summary beginning
    // "repos.json api_key for ..." — the same shape the startup guard writes and diagnose() keys
    // `key_shape_mismatch` off of. Preserve it through the finally so the supervisor surfaces the
    // drift instead of clobbering it with "stopped"/"crashed".
    let config_drift_halt = preserves_key_shape_mismatch_diagnostic(&ctx.hb);
    if ctx.halted || persistent_self_stop || config_drift_halt {
        // keep the error/reverted (or persistent self-stop / config-drift) heartbeat untouched
    } else if clean_exit {
        ctx.heartbeat(json!({"status": "stopped", "phase": Value::Null}));
    } else {
        ctx.heartbeat(json!({
            "status": "error",
            "phase": "crashed",
            "last_summary": "loop crashed (unhandled exception) — will be restarted",
        }));
    }
    release_lock(&ctx);
    if !persistent_self_stop {
        let _ = std::fs::remove_file(&ctx.stop_path);
    }
    0
}

/// `shutil.which("pi") is None` — the startup pi-on-PATH probe. `ctx.pi_exe()` returns the resolved
/// path when found, or the bare name "pi" as a last resort (when which() failed). So "missing" ==
/// the resolved value is exactly "pi" (the un-resolved fallback) AND "pi" is not itself on PATH.
fn which_pi_missing() -> bool {
    which::which("pi").is_err()
}

/// True iff `hb` carries the key_shape_mismatch diagnostic the loop's startup guard and the
/// mid-loop re-check both write: status=="error" AND last_summary begins "repos.json api_key for".
/// Pure so the finally's keep-condition is unit-testable. Mirrors the predicate
/// supervisor::diagnose uses to classify the `key_shape_mismatch` category, so the two can never
/// drift apart (a heartbeat this preserves is exactly one diagnose surfaces as key_shape_mismatch).
fn preserves_key_shape_mismatch_diagnostic(hb: &Value) -> bool {
    hb.get("status").and_then(Value::as_str) == Some("error")
        && hb
            .get("last_summary")
            .and_then(Value::as_str)
            .map(|s| s.starts_with("repos.json api_key for"))
            .unwrap_or(false)
}

// --------------------------------------------------------------------------- //
// single-flight runner lock (run_improver.acquire_lock / release_lock)
// --------------------------------------------------------------------------- //

/// run_improver._read_lock_pid: the pid on the FIRST line of LOCK, or 0 when missing/empty/corrupt.
fn read_lock_pid(ctx: &Ctx) -> i64 {
    let raw = match std::fs::read_to_string(&ctx.lock_path) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    if raw.trim().is_empty() {
        return 0;
    }
    let first = raw.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return 0;
    }
    first.parse::<i64>().unwrap_or(0)
}

/// run_improver._heartbeat_stale(window): True only with POSITIVE evidence the holder is dead — its
/// heartbeat updated_at is older than `window` seconds. Missing/unparseable -> False.
fn heartbeat_stale(ctx: &Ctx, window: f64) -> bool {
    let hb: Value = match std::fs::read_to_string(&ctx.heartbeat_path) {
        Ok(s) => match serde_json::from_str(&s) {
            Ok(v) => v,
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    let ts = match hb.get("updated_at").and_then(Value::as_str) {
        Some(t) if !t.is_empty() => t,
        _ => return false,
    };
    let parsed = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ");
    let last = match parsed {
        Ok(n) => n.and_utc(),
        Err(_) => return false,
    };
    (chrono::Utc::now() - last).num_milliseconds() as f64 / 1000.0 > window
}

/// run_improver._heartbeat_is_stopped: True if the holder's heartbeat reports status=='stopped'.
fn heartbeat_is_stopped(ctx: &Ctx) -> bool {
    match std::fs::read_to_string(&ctx.heartbeat_path) {
        Ok(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => v.get("status").and_then(Value::as_str) == Some("stopped"),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// run_improver.acquire_lock (~2877-2935): single-flight, at most one improver per repo. Returns true
/// iff WE now hold the lock. Atomic O_EXCL create writing `<pid>\n<run_id>` (no empty window); an
/// EXISTING empty lock is treated as HELD (never stolen); only a confirmed-dead / frozen / stopped
/// holder is taken over (tmp+rename), verified by reading our pid back; on a win we stamp the
/// heartbeat immediately so the lock/heartbeat run_ids match (collapses the orphan window).
fn acquire_lock(ctx: &mut Ctx) -> bool {
    use std::io::Write;
    let _ = std::fs::create_dir_all(&ctx.runtime);
    let mypid = std::process::id() as i64;
    let content = format!("{}\n{}", mypid, ctx.run_id);

    // atomic exclusive create WITH the pid written before the handle closes (no empty window)
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ctx.lock_path)
    {
        Ok(mut f) => {
            let _ = f.write_all(content.as_bytes());
            return true;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => {} // Python only catches FileExistsError; any other open error would raise. Treat
                     // like "exists" and fall through to resolve-holder (best-effort, never panic).
    }

    // The lock exists. Resolve who holds it, tolerating a racer's just-created-but-empty file.
    let mut pid = 0i64;
    for _ in 0..10 {
        pid = read_lock_pid(ctx);
        if pid != 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if pid == mypid {
        return true; // already ours
    }
    if pid == 0 {
        return false; // still empty after the grace window — a racer holds it
    }
    let window = (3.0 * ctx.interval as f64).max(paths::LOCK_LIVE_FLOOR_S);
    if locks::pid_alive(pid) && !heartbeat_stale(ctx, window) && !heartbeat_is_stopped(ctx) {
        return false; // held by a live improver
    }
    // dead / frozen / cleanly-stopped holder -> take over, then VERIFY we won (last rename wins).
    let tmp = ctx.runtime.join(format!("lock.{mypid}.tmp"));
    let took_over = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, content.as_bytes())?;
        std::fs::rename(&tmp, &ctx.lock_path)
    })()
    .is_ok();
    if !took_over {
        return false;
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    let won = read_lock_pid(ctx) == mypid;
    if won {
        // stamp the heartbeat now so lock.run_id and heartbeat.run_id match the instant we win
        ctx.heartbeat(json!({"status": "idle"}));
    }
    won
}

/// run_improver.release_lock (~2938-2948): unlink the lock ONLY if it is still ours (first-line pid ==
/// our pid). A read error / unparseable content leaves it untouched (read_lock_pid -> 0 != our pid).
fn release_lock(ctx: &Ctx) {
    if read_lock_pid(ctx) == std::process::id() as i64 {
        let _ = std::fs::remove_file(&ctx.lock_path);
    }
}

// --------------------------------------------------------------------------- //
// tests
// --------------------------------------------------------------------------- //
#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build argv from string slices.
    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_defaults_when_only_repo_given() {
        let a = parse_args(&argv(&["--repo", "/srv/repos/foo"])).expect("ok");
        assert_eq!(a.repo.as_deref(), Some("/srv/repos/foo"));
        assert_eq!(a.provider, "ollama-cloud");
        assert_eq!(a.ship, "pr");
        assert_eq!(a.gate, "");
        assert_eq!(a.pr_target_branch, "");
        assert_eq!(a.max_iterations, 0);
        assert_eq!(a.reasoning, "");
        assert_eq!(a.goal, "");
        assert!(!a.once);
        assert_eq!(a.interval, 120);
        assert!(!a.smoke);
        assert!(!a.beautify);
        assert!(!a.provision);
        assert!(!a.ideate);
        assert!(!a.solomon);
        assert!(a.name.is_none());
        assert!(a.model.is_none());
    }

    #[test]
    fn parse_args_missing_repo_exits_2() {
        let code = parse_args(&argv(&[])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_value_flags_space_form() {
        let a = parse_args(&argv(&[
            "--repo", "r",
            "--name", "n",
            "--provider", "openrouter",
            "--model", "qwen",
            "--ship", "push",
            "--gate", "cargo test",
            "--pr-target-branch", "trunk",
            "--max-iterations", "7",
            "--reasoning", "high",
            "--goal", "fix bug",
            "--interval", "30",
        ]))
        .expect("ok");
        assert_eq!(a.repo.as_deref(), Some("r"));
        assert_eq!(a.name.as_deref(), Some("n"));
        assert_eq!(a.provider, "openrouter");
        assert_eq!(a.model.as_deref(), Some("qwen"));
        assert_eq!(a.ship, "push");
        assert_eq!(a.gate, "cargo test");
        assert_eq!(a.pr_target_branch, "trunk");
        assert_eq!(a.max_iterations, 7);
        assert_eq!(a.reasoning, "high");
        assert_eq!(a.goal, "fix bug");
        assert_eq!(a.interval, 30);
    }

    #[test]
    fn parse_args_inline_equals_form() {
        let a = parse_args(&argv(&[
            "--repo=r",
            "--provider=openrouter",
            "--ship=auto-merge",
            "--reasoning=off",
            "--max-iterations=3",
            "--interval=5",
        ]))
        .expect("ok");
        assert_eq!(a.repo.as_deref(), Some("r"));
        assert_eq!(a.provider, "openrouter");
        assert_eq!(a.ship, "auto-merge");
        assert_eq!(a.reasoning, "off");
        assert_eq!(a.max_iterations, 3);
        assert_eq!(a.interval, 5);
    }

    #[test]
    fn parse_args_store_true_flags() {
        let a = parse_args(&argv(&[
            "--repo", "r",
            "--once", "--smoke", "--beautify", "--provision", "--ideate", "--solomon",
        ]))
        .expect("ok");
        assert!(a.once);
        assert!(a.smoke);
        assert!(a.beautify);
        assert!(a.provision);
        assert!(a.ideate);
        assert!(a.solomon);
    }

    #[test]
    fn parse_args_bad_provider_choice_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--provider", "claude"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_bad_ship_choice_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--ship", "teleport"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_bad_reasoning_choice_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--reasoning", "ultra"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_empty_reasoning_is_valid_choice() {
        // "" is in REASONING_CHOICES (matches Python's default-of-""-is-allowed edge)
        let a = parse_args(&argv(&["--repo", "r", "--reasoning", ""])).expect("ok");
        assert_eq!(a.reasoning, "");
    }

    #[test]
    fn parse_args_non_int_max_iterations_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--max-iterations", "abc"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_non_int_interval_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--interval", "x"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_max_iterations_trims_whitespace() {
        let a = parse_args(&argv(&["--repo", "r", "--max-iterations", " 42 "])).expect("ok");
        assert_eq!(a.max_iterations, 42);
    }

    #[test]
    fn parse_args_unknown_flag_exits_2() {
        let code = parse_args(&argv(&["--repo", "r", "--bogus"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_missing_value_at_end_exits_2() {
        // --provider with no following value
        let code = parse_args(&argv(&["--repo", "r", "--provider"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn parse_args_negative_max_iterations_ok() {
        let a = parse_args(&argv(&["--repo", "r", "--max-iterations", "-1"])).expect("ok");
        assert_eq!(a.max_iterations, -1);
    }

    #[test]
    fn parse_args_repo_required_even_with_other_flags() {
        // other valid flags but no --repo
        let code = parse_args(&argv(&["--ship", "pr", "--interval", "10"])).err().unwrap();
        assert_eq!(code, 2);
    }

    #[test]
    fn path_name_returns_final_component() {
        assert_eq!(path_name("/srv/repos/foo"), "foo");
        assert_eq!(path_name("foo"), "foo");
        assert_eq!(path_name("/srv/repos/foo/"), "foo");
    }

    #[test]
    fn path_name_falls_back_to_input_on_root() {
        // Path::file_name is None for "/" — falls back to the input string
        assert_eq!(path_name("/"), "/");
    }

    // ---- preserves_key_shape_mismatch_diagnostic: the finally's keep-condition ----

    #[test]
    fn preserves_key_shape_mismatch_diagnostic_true_for_guard_summary() {
        // The exact heartbeat the startup guard AND the mid-loop re-check write.
        let hb = json!({
            "status": "error",
            "last_summary": "repos.json api_key for 'foo' looks like an OpenRouter key (sk-or-v1-...) but provider is 'ollama-cloud' ...",
        });
        assert!(preserves_key_shape_mismatch_diagnostic(&hb));
    }

    #[test]
    fn preserves_key_shape_mismatch_diagnostic_true_for_mirror_image_summary() {
        // The other direction the guard emits (provider=openrouter, key not sk-or-v1-).
        let hb = json!({
            "status": "error",
            "last_summary": "repos.json api_key for 'foo' is set but does not look like an OpenRouter key ...",
        });
        assert!(preserves_key_shape_mismatch_diagnostic(&hb));
    }

    #[test]
    fn preserves_key_shape_mismatch_diagnostic_false_for_other_errors() {
        // A generic error / crashed / no_key heartbeat must NOT be preserved by this predicate —
        // the finally should overwrite them per its normal branches.
        assert!(!preserves_key_shape_mismatch_diagnostic(&json!({"status": "error", "phase": "crashed"})));
        assert!(!preserves_key_shape_mismatch_diagnostic(&json!({"status": "error", "last_summary": "OLLAMA_API_KEY not set"})));
        assert!(!preserves_key_shape_mismatch_diagnostic(&json!({"status": "stopped"})));
        assert!(!preserves_key_shape_mismatch_diagnostic(&json!({})));
    }
}
