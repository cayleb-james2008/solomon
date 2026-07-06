// Solomon — Rust/Tauri host (the SOLE executable: dashboard + `run-improver` + `watchdog`).
//
// The native Rust backend (control/improver/supervisor/watchdog/api) drives web/app.js UNCHANGED
// through one `bridge` command + a Proxy shim. Release builds are windowless (windows_subsystem),
// so neither the GUI nor the every-2-min watchdog sweep flashes a console; headless subcommands
// re-attach to the launching terminal (attach_parent_console) so their stdout stays visible.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod actions; // closed action registry + TTL escalation policies (actions.json) — every diagnosis maps to an executable remediation
mod api; // native port of app.py's Api — the `bridge` command + headless backend (get_state, dispatch)
mod ceo; // CEO rhythm (v2 Phase B): morning plan + evening verified-outcome summary (`plan`/`report` + watchdog graft)
mod control; // native port of control.py — repos registry, git/gh, locks, runner (the `bridge` backend)
mod deploy; // managed-app redeploy: rebuild+relaunch a repo's live app binary when a fix merged but never deployed (deploy gap)
mod fleet; // single-agent autopilot scheduler: one provider key, one active AI job, proof records
mod housekeeping; // storage housekeeping (v2): day-gated build-dir/branch/worktree cleanup on the watchdog tick
mod hygiene; // repo-hygiene detection (report-only): flags off-base / dirty managed trees the CEO grafts surface
mod improver; // native port of improver/run_improver.py — the per-repo RSI loop (`run-improver` subcommand)
mod janitor; // storage janitor (RSI v3, requirement 5): temp deletion, log rotation, bounded runtime dirs, history compaction — rides the watchdog sweep behind a 6h stamp
mod notify; // operator notifications (v2 Phase A): ntfy push + Windows toast, fed by ops incidents + CEO reports
mod ops; // ops plane (Phase 1): ground-truth probes + honest fleet status (`probe` subcommand + watchdog graft)
mod provenance; // config-provenance tripwire + controller-clean preflight (RSI v3, catalog #6): watched-config drift pages + holds trading lanes; Solomon refuses meta-work on itself from a dirty/off-base tree
mod redeploy; // native self-redeploy: swap Solomon's own production binary in a safe drain window
mod supervisor; // native port of improver/solomon.py — diagnose() + the 3-rung recover() ladder + escalation
mod watchdog; // native port of monitor.py — the `watchdog` subcommand + the in-app 2-min tick (run_gui)

use serde_json::{json, Value};
use tauri_plugin_updater::UpdaterExt;

// Define window.pywebview.api as a Proxy forwarding each positional method call to the single
// Tauri `bridge` command. __TAURI__ is read lazily (at call time) so init-script ordering can't
// race us, and the Proxy object itself is truthy immediately so app.js's realApi() boot path fires.
const SHIM: &str = r#"
(function () {
  window.pywebview = { api: new Proxy({}, {
    get: function (_t, m) {
      return function () {
        var args = Array.prototype.slice.call(arguments);
        return window.__TAURI__.core.invoke('bridge', { method: String(m), args: args });
      };
    }
  })};
})();
"#;

#[tauri::command]
async fn bridge(app: tauri::AppHandle, method: String, args: Vec<Value>) -> Result<Value, String> {
    // The in-app updater needs the AppHandle (which api::dispatch does not have), so update_status and
    // apply_update are special-cased here through tauri-plugin-updater. Every OTHER method forwards to
    // api::dispatch unchanged (cached_update_status stays a benign stub there — it has nothing to cache).
    match method.as_str() {
        "update_status" => Ok(update_status(&app).await),
        "apply_update" => Ok(apply_update(&app).await),
        // dispatch does blocking git/gh/fs work; run it off the async runtime's worker so the UI stays responsive.
        _ => tauri::async_runtime::spawn_blocking(move || api::dispatch(&method, &args))
            .await
            .map_err(|e| format!("bridge join: {e}"))?,
    }
}

/// Check GitHub Releases for a newer Solomon build. Returns the shape the dashboard's Update button
/// reads: {ok, available, currentSha, version}. `currentSha` stays the same value `current_sha` shows
/// (the checkout's short git sha) so the version label is unchanged. On any error: {ok:false, available:false}.
async fn update_status(app: &tauri::AppHandle) -> Value {
    let current_sha = api::dispatch("current_sha", &[]).unwrap_or(Value::Null);
    let updater = match app.updater() {
        Ok(u) => u,
        Err(_) => return json!({"ok": false, "available": false}),
    };
    match updater.check().await {
        Ok(Some(update)) => {
            json!({"ok": true, "available": true, "currentSha": current_sha, "version": update.version})
        }
        Ok(None) => {
            json!({"ok": true, "available": false, "currentSha": current_sha, "version": Value::Null})
        }
        Err(_) => json!({"ok": false, "available": false}),
    }
}

/// Download + install the latest release, then relaunch. download_and_install runs the NSIS installer
/// (Tauri swaps the running exe and relaunches); app.restart() is the explicit relaunch on success.
async fn apply_update(app: &tauri::AppHandle) -> Value {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    match updater.check().await {
        Ok(Some(update)) => match update
            .download_and_install(|_chunk, _total| {}, || {})
            .await
        {
            Ok(()) => {
                app.restart();
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        // Release vanished between the boot-time check and this click (yanked/re-tagged, or a transient
        // backend hiccup). Return ok:false so the dashboard's `if (r.ok === false)` handler shows
        // "update failed" instead of leaving the pill frozen on "updating…" with no feedback.
        Ok(None) => json!({"ok": false, "available": false, "error": "update no longer available"}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// `solomon <sub> [arg]` -> the native headless control surface (replaces the app.py shell-out). Mirrors
/// app.py's `__main__` block: --state pretty-prints get_state; --start/--stop take a name; --supervise
/// runs the unattended sweep (one repo or all); --serve-health runs a 127.0.0.1 health endpoint.
fn run_headless(args: &[String]) -> i32 {
    // A missing/unknown subcommand is a usage error, not silent success. Exit 2 (argparse's
    // SystemExit code for missing required args — the same improver::run::parse_args uses) so a
    // scripted `solomon start && ...` does not falsely report OK, and a short argv cannot panic on
    // `args[0]` (main() only routes the 5 known subcommands here, but the function must not lie /
    // crash if called with fewer).
    let sub = match args.first() {
        Some(a) => a.trim_start_matches("--"),
        None => {
            usage();
            return 2;
        }
    };
    let name_arg = args.get(1).cloned();
    match sub {
        // print(json.dumps(api.get_state(), indent=2))
        "state" => match api::dispatch("get_state", &[]) {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        },
        // print(json.dumps(api.start(arg))) — app.py requires a name (`--start and arg`). A missing
        // name is a usage error (exit 2), not exit-success — otherwise `solomon start` in a script
        // silently no-ops while reporting OK.
        "start" | "stop" => match name_arg {
            Some(n) => match api::dispatch(sub, &[Value::String(n)]) {
                Ok(v) => {
                    println!("{}", v);
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            },
            None => {
                usage();
                2
            }
        },
        // api.supervise(arg, unattended=True): name|null, allow_pi=false, unattended=true.
        "supervise" => {
            let name = name_arg.map(Value::String).unwrap_or(Value::Null);
            match api::dispatch("supervise", &[name, Value::Bool(false), Value::Bool(true)]) {
                Ok(v) => {
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                    0
                }
                Err(e) => {
                    eprintln!("{e}");
                    1
                }
            }
        }
        // serve_health(int(arg) if arg.isdigit() else 8787) — a blocking 127.0.0.1 health endpoint.
        "serve-health" => {
            let port: u16 = name_arg
                .as_deref()
                .and_then(|s| s.trim().parse::<u16>().ok())
                .unwrap_or(8787);
            serve_health(port)
        }
        _ => {
            usage();
            2
        }
    }
}

fn usage() {
    eprintln!(
        "usage: solomon state | autopilot-state | autopilot-wake [name] | autopilot-pause | supervise [name] | serve-health [port] | probe [name] | plan | report | watchdog"
    );
}

fn is_autopilot_subcommand(sub: Option<&str>) -> bool {
    matches!(
        sub,
        Some(
            "autopilot-state"
                | "autopilot-wake"
                | "autopilot-pause"
                | "fleet-state"
                | "fleet-once"
                | "fleet-drain"
        )
    )
}

/// app.serve_health: a tiny stdlib HTTP server on 127.0.0.1 serving the health payload as JSON on
/// GET /health (404 elsewhere). std::net only — no web framework. Blocks (serve_forever equivalent).
fn serve_health(port: u16) -> i32 {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind 127.0.0.1:{port}: {e}");
            return 1;
        }
    };
    let bound = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    println!("Solomon health endpoint: http://127.0.0.1:{bound}/health");
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(s) => s,
            Err(_) => continue,
        };
        // Bound the read so one silent/partial client can't pin this single-threaded accept loop
        // forever (which would wedge the whole health endpoint for every other poller). On timeout
        // read() returns Err -> unwrap_or(0) -> 0 bytes -> the 404 path -> connection dropped -> loop
        // continues. ponytail: a read timeout fixes the wedge; per-connection threads (full
        // ThreadingHTTPServer parity) only matter if concurrent health polls ever become a need.
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        // Read just the request line (the method + path); a single small read is enough for GET headers.
        let mut buf = [0u8; 1024];
        let n = stream.read(&mut buf).unwrap_or(0);
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let path = path.split('?').next().unwrap_or("");
        let resp: Vec<u8> = if path == "/health" {
            let body = serde_json::to_vec(&api::health_payload()).unwrap_or_default();
            let mut r = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend_from_slice(&body);
            r
        } else {
            let body = b"not found";
            let mut r = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend_from_slice(body);
            r
        };
        let _ = stream.write_all(&resp);
        let _ = stream.flush();
    }
    0
}

#[cfg(windows)]
fn attach_parent_console() {
    // Release builds are windows_subsystem="windows" (no console). For a headless subcommand invoked
    // from a terminal, re-attach to the parent's console so println!/eprintln! stay visible. Harmless
    // no-op when there is no parent console (the scheduled watchdog task / a detached spawn).
    extern "system" {
        fn AttachConsole(dw_process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = 0xFFFF_FFFF;
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // Windowless release: re-attach a console only for headless subcommands so their stdout shows;
    // the GUI path (no args) stays console-free.
    #[cfg(windows)]
    if !argv.is_empty() {
        attach_parent_console();
    }
    // `solomon run-improver ...` — the native per-repo RSI loop runner. Dispatch BEFORE run_gui so no
    // window is created (this is a headless long-running process), and exit with its return code.
    if argv.first().map(String::as_str) == Some("run-improver") {
        std::process::exit(improver::run::main(&argv[1..]));
    }
    // `solomon watchdog` — one watchdog sweep. The Solomon Sentinel scheduled task
    // (tools/install_sentinel.ps1) runs `solomon watchdog` every 5 minutes out-of-band; the GUI
    // tick sweep remains as a secondary layer. Renegotiated by the operator 2026-07-06 after the
    // liveness autopsy (GUI-tick-only liveness caused the 8h/25.5h watchdog gaps and 2+ day
    // outages). Dispatch BEFORE run_gui so no window is created; exit with watchdog::main's code.
    if argv.first().map(String::as_str) == Some("watchdog") {
        std::process::exit(watchdog::main());
    }
    // `solomon fleet-*` — the single-agent scheduler surface. Dispatch before the GUI so these remain
    // headless, and keep them in this one exe (no companion daemon/runtime).
    if is_autopilot_subcommand(argv.first().map(String::as_str)) {
        let st = api::AppState::load();
        let out = match argv[0].as_str() {
            "autopilot-state" | "fleet-state" => fleet::state(),
            "autopilot-wake" | "fleet-once" => {
                fleet::wake(st.get_auto_push(), argv.get(1).map(String::as_str))
            }
            "autopilot-pause" => fleet::pause(),
            "fleet-drain" => {
                let limit = argv
                    .get(1)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                fleet::drain(st.get_auto_push(), limit)
            }
            _ => unreachable!(),
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }
    // `solomon probe [name] [--json]` — the ops-plane ground-truth probe runner (Phase 1). Dispatch
    // BEFORE run_gui so no window is created, and exit with the verdict code: 0 = all green,
    // 3 = any yellow, 4 = any red (2 stays the usage-error code).
    if argv.first().map(String::as_str) == Some("probe") {
        std::process::exit(ops::outcomes::probe_main(&argv[1..]));
    }
    // `solomon plan` / `solomon report` — run the CEO morning plan / evening summary ON DEMAND
    // (the day-gated automatic runs ride the watchdog tick; the CLI bypasses the day gate WITHOUT
    // touching _ceo_state.json, so a manual run never suppresses the scheduled one). Exit 0 on
    // ok, 1 on failure — the JSON result is printed either way.
    if matches!(
        argv.first().map(String::as_str),
        Some("plan") | Some("report")
    ) {
        let out = if argv[0] == "plan" {
            ceo::morning_plan()
        } else {
            ceo::evening_summary()
        };
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        let ok = out.get("ok").and_then(Value::as_bool).unwrap_or(false);
        std::process::exit(if ok { 0 } else { 1 });
    }
    let is_sub = argv
        .first()
        .map(|a| {
            matches!(
                a.trim_start_matches("--"),
                "state" | "start" | "stop" | "supervise" | "serve-health"
            )
        })
        .unwrap_or(false);
    if is_sub {
        std::process::exit(run_headless(&argv));
    }
    run_gui();
}

fn run_gui() {
    // Arm the kill-on-close job BEFORE any improver child can be spawned (children are spawned via the
    // bridge once the window is up). Every backend loop the GUI starts is bound to this job and dies
    // when the GUI process exits — closing the app shuts down its backend processes.
    control::proc::init_app_job();
    // THE IN-APP WATCHDOG TICK (v2 Phase A): the every-2-min sweep lives INSIDE the visibly-open
    // Solomon.exe — crash-restart + RUNG-0 recovery + ops probes + incident notifications + the
    // CEO rhythm all ride it. The Solomon Sentinel scheduled task (tools/install_sentinel.ps1)
    // runs `solomon watchdog` every 5 minutes out-of-band; this GUI tick sweep remains as a
    // secondary layer. Renegotiated by the operator 2026-07-06 after the liveness autopsy
    // (GUI-tick-only liveness caused the 8h/25.5h watchdog gaps and 2+ day outages). The thread
    // dies with the process; catch_unwind keeps one bad sweep from killing the tick.
    std::thread::spawn(|| loop {
        let _ = std::panic::catch_unwind(|| {
            let _ = watchdog::main();
        });
        std::thread::sleep(std::time::Duration::from_secs(120));
    });
    tauri::Builder::default()
        // single-instance MUST be registered FIRST (Tauri 2 requirement) so it runs before other
        // plugins. Launching solomon.exe again focuses the running dashboard instead of opening a
        // duplicate: unminimize + focus the existing "main" window.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::Manager;
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![bridge])
        .setup(|app| {
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::App("index.html".into()),
            )
            .title("Solomon")
            .inner_size(1000.0, 760.0)
            .min_inner_size(820.0, 600.0)
            .initialization_script(SHIM)
            .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Solomon");
}

#[cfg(test)]
mod tests {
    use super::{is_autopilot_subcommand, run_headless};

    // Usage errors on the main entrypoint must report non-zero (exit 2, the argparse convention
    // improver::run::parse_args also uses) — not silent success. A scripted `solomon start && ...`
    // that no-ops because the repo name was forgotten must fail the chain, not falsely report OK.
    #[test]
    fn run_headless_missing_name_is_usage_error() {
        // `solomon start` (no repo name) -> usage error, exit 2 (was: exit 0 = silent success).
        assert_eq!(run_headless(&["start".to_string()]), 2);
        assert_eq!(run_headless(&["stop".to_string()]), 2);
        // `--start` strips the leading dashes the same way main()'s is_sub filter does.
        assert_eq!(run_headless(&["--start".to_string()]), 2);
    }

    // A short/empty argv must not panic on `args[0]`. main() only routes known subcommands here,
    // but the function is the entrypoint's sub-dispatcher and must degrade to a usage error (exit 2)
    // rather than index out of bounds if ever called with an empty slice.
    #[test]
    fn run_headless_empty_args_does_not_panic() {
        assert_eq!(run_headless(&[]), 2);
    }

    // An unknown subcommand is a usage error, not exit-success.
    #[test]
    fn run_headless_unknown_subcommand_is_usage_error() {
        assert_eq!(run_headless(&["frobnicate".to_string()]), 2);
    }

    #[test]
    fn autopilot_cli_commands_and_hidden_aliases_are_recognized() {
        for sub in [
            "autopilot-state",
            "autopilot-wake",
            "autopilot-pause",
            "fleet-state",
            "fleet-once",
            "fleet-drain",
        ] {
            assert!(is_autopilot_subcommand(Some(sub)), "{sub} not recognized");
        }
        assert!(!is_autopilot_subcommand(Some("loop-start")));
        assert!(!is_autopilot_subcommand(None));
    }
}
