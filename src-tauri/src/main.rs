// Solomon — Rust/Tauri host (the SOLE executable: dashboard + `run-improver` + `watchdog`).
//
// The native Rust backend (control/improver/supervisor/watchdog/api) drives web/app.js UNCHANGED
// through one `bridge` command + a Proxy shim. Release builds are windowless (windows_subsystem),
// so neither the GUI nor the every-2-min watchdog sweep flashes a console; headless subcommands
// re-attach to the launching terminal (attach_parent_console) so their stdout stays visible.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod api; // native port of app.py's Api — the `bridge` command + headless backend (get_state, dispatch)
mod control; // native port of control.py — repos registry, git/gh, locks, runner (the `bridge` backend)
mod improver; // native port of improver/run_improver.py — the per-repo RSI loop (`run-improver` subcommand)
mod supervisor; // native port of improver/solomon.py — diagnose() + the 3-rung recover() ladder + escalation
mod watchdog; // native port of monitor.py — the `watchdog` subcommand (SolomonWatchdog scheduled sweep)

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
        Ok(None) => json!({"ok": true, "available": false, "currentSha": current_sha, "version": Value::Null}),
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
        Ok(Some(update)) => match update.download_and_install(|_chunk, _total| {}, || {}).await {
            Ok(()) => {
                app.restart();
            }
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        },
        Ok(None) => json!({"ok": true, "available": false}),
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// `solomon <sub> [arg]` -> the native headless control surface (replaces the app.py shell-out). Mirrors
/// app.py's `__main__` block: --state pretty-prints get_state; --start/--stop take a name; --supervise
/// runs the unattended sweep (one repo or all); --serve-health runs a 127.0.0.1 health endpoint.
fn run_headless(args: &[String]) -> i32 {
    let sub = args[0].trim_start_matches("--");
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
        // print(json.dumps(api.start(arg))) — app.py requires a name (`--start and arg`).
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
                0
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
            0
        }
    }
}

fn usage() {
    eprintln!(
        "usage: solomon state | start <name> | stop <name> | supervise [name] | serve-health [port]"
    );
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
    // `solomon watchdog` — the native overnight watchdog sweep (called by the SolomonWatchdog scheduled
    // task). Dispatch BEFORE run_gui so no window is created, and exit with watchdog::main's return code.
    if argv.first().map(String::as_str) == Some("watchdog") {
        std::process::exit(watchdog::main());
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
