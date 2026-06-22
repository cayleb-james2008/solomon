//! Native Rust port of `improver/visual_review.py` (+ its run_improver.py call site) — the visual
//! E2E sandbox review that runs AFTER the test gate passes and BEFORE the branch ships.
//!
//! Flow (bug-for-bug with visual_review.run):
//!   1. Boot the app in an ephemeral sandbox (`Sandbox`, per `ctx.sandbox_config`).
//!   2. Drive the persistent, policy-bounded `agent-browser` CLI over the configured pages
//!      (`_run_capture` → screenshots (base64) + a11y trees + console/network errors).
//!   3. Save the PNG screenshots to `runtime/<name>/visual_review/`.
//!   4. Build a `pi` task (screenshots + a11y + errors + iteration summary) and run the vision agent
//!      (`vision-cloud` provider, `visual_review.md` contract) with a 120s→180s retry escalation.
//!   5. Parse the `===FINDINGS=== … ===END===` block, build one-time feedback for the next RSI
//!      iteration, persist a slim `report.json`, and return the report dict.
//!
//! The whole module is best-effort: nothing here ever propagates a panic into the RSI loop; the
//! Python `try/except Exception` wrapper becomes a Rust outcome that always yields a `{ok:bool,…}`
//! object. The agent-browser binary is shelled to (an external tool) — CDP is NOT reimplemented.
//!
//! DEVIATIONS (see the report at the end of the porting task):
//!   - `run_visual_gate(&mut Ctx)` is the mandated entry signature; the Python `run()` took explicit
//!     `repo_path/runtime_dir/sandbox_config/vision_model/iteration_summary/provider_key` args. Those
//!     all come off `Ctx` except `iteration_summary`, which the call site stashes in the heartbeat as
//!     `last_summary` immediately before calling — so we read it from `ctx.hb["last_summary"]`.
//!   - `Sandbox` and `AgentBrowser` are ported THIN inline here (no separate Rust module exists and
//!     the task scopes edits to this one file). The sandbox boot path (copytree mirror, Job-Object
//!     process tree, scrubbed env, health-wait) and the agent-browser CLI driver are faithful to the
//!     observable contract `_run_capture` depends on (`navigate()` state dict + `frame_file`).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Map, Value};

use crate::control::proc;
use crate::improver::ctx::{self, Ctx};

// ---------------------------------------------------------------------------
// Constants mirrored from visual_review.py module scope.
// ---------------------------------------------------------------------------

// VISION_EXT = HERE / "provider.ts"; VISUAL_REVIEW_MD = HERE / "visual_review.md". `HERE` is the
// improver/ directory next to run_improver.py. ctx.here is the control root; the improver scripts
// live under <here>/improver/. We resolve relative to ctx.here so the bundled .ts/.md resolve the
// same way the Python module did (HERE = Path(visual_review.py).resolve().parent).
fn vision_ext(ctx: &Ctx) -> PathBuf {
    ctx.here.join("improver").join("provider.ts")
}
fn visual_review_md(ctx: &Ctx) -> PathBuf {
    ctx.here.join("improver").join("visual_review.md")
}

// ---------------------------------------------------------------------------
// Public entry — run_improver.py call site + visual_review.run() folded together.
// ---------------------------------------------------------------------------

/// `visual_review.run(...)` — main orchestrator. Boots the sandbox, captures pages, runs the vision
/// agent, parses findings, builds feedback, saves `report.json`, and returns the report dict. Never
/// raises (best-effort; the RSI loop must not break on a visual-review failure). Sets
/// `ctx.last_visual_feedback` to the report's feedback on a successful (`ok:true`) review and clears
/// it otherwise — mirroring the `LAST_VISUAL_FEEDBACK` assignment at the run_improver.py call site.
///
/// Return dict shapes (exact key sets per outcome path, matching visual_review.run):
///   - launch missing:        `{ok:false, error:"sandbox config has no 'launch' command"}`
///   - capture failed:        `{ok:false, error:"capture failed (agent-browser unavailable)"}`
///   - no vision_model:       `{ts, ok:true, findings:[], summary:<msg>, screenshots, capture, feedback:""}`
///   - no API key:            `{ts, ok:true, findings:[], summary:<msg>, screenshots, capture, feedback:""}`
///   - agent produced none:   `{ts, ok:false, error:"vision agent produced no output", findings:[], summary:<msg>, screenshots, capture, feedback:""}`
///   - success:               `{ts, ok:true, findings, summary, screenshots, capture:{pages,console_errors,network_errors}, feedback, agent_raw}`
///   - exception/boot fail:   `{ok:false, error:<str(e)[:300]>}`
pub fn run_visual_gate(ctx: &mut Ctx) -> Value {
    let report = run_inner(ctx);
    // Mirror the run_improver.py call site: on ok:true set LAST_VISUAL_FEEDBACK = feedback; else "".
    if report.get("ok").and_then(Value::as_bool) == Some(true) {
        ctx.last_visual_feedback = report
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    } else {
        ctx.last_visual_feedback = String::new();
    }
    report
}

fn run_inner(ctx: &mut Ctx) -> Value {
    let repo_path = ctx.repo.clone();
    let runtime_dir = ctx.runtime.clone();
    let sandbox_config = ctx.sandbox_config.clone();
    let vision_model = ctx.vision_model.clone();
    // iteration_summary — see module DEVIATION note: read from the heartbeat the call site just set.
    let iteration_summary = ctx
        .hb
        .get("last_summary")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let provider_key = "OLLAMA_API_KEY";

    let out_dir = runtime_dir.join("visual_review");
    let _ = std::fs::create_dir_all(&out_dir); // mkdir(parents=True, exist_ok=True)

    let pages = config_pages(&sandbox_config); // sandbox_config.get("pages") or ["/"]
    let launch = config_launch(&sandbox_config); // sandbox_config.get("launch") or ""
    if launch.is_empty() {
        return json!({"ok": false, "error": "sandbox config has no 'launch' command"});
    }

    // 1. Boot sandbox. The Python `with Sandbox(...) as sb:` block + the trailing `except Exception as e`
    // become a single fallible boot that, on any error, returns {ok:false, error:str(e)[:300]}.
    ctx.log("visual review: booting sandbox...");
    let sandbox = match Sandbox::boot(&sandbox_config, &repo_path) {
        Ok(sb) => sb,
        Err(e) => {
            let detail = truncate(&e, 200);
            ctx.log(&format!("visual review: failed — {detail}"));
            return json!({"ok": false, "error": truncate(&e, 300)});
        }
    };
    // The sandbox is RAII (Drop tears it down on every return below); from here on the only failure
    // mode is best-effort sub-steps, so the "except Exception" path is unreachable in practice
    // (capture/vision are themselves exception-free in Python).
    ctx.log(&format!(
        "visual review: sandbox healthy at {}",
        sandbox.base_url
    ));

    // 2. Capture.
    ctx.log("visual review: capturing pages...");
    let capture_dir = runtime_dir.join("app_test_capture");
    let capture_result = run_capture(&sandbox.base_url, &pages, &capture_dir, &repo_path);
    let capture_result = match capture_result {
        Some(c) => c,
        None => {
            return json!({"ok": false, "error": "capture failed (agent-browser unavailable)"});
        }
    };

    // 3. Save screenshots.
    let screenshots = save_screenshots(&capture_result, &out_dir);
    let n_screens = screenshots.as_array().map(|a| a.len()).unwrap_or(0);
    ctx.log(&format!("visual review: saved {n_screens} screenshots"));

    // 4. Build + run vision agent.
    if vision_model.is_empty() {
        // No vision model configured — save capture artifacts, skip agent.
        let report = json!({
            "ts": ctx::now(),
            "ok": true,
            "findings": [],
            "summary": "Visual review ran but no vision model configured — screenshots saved.",
            "screenshots": screenshots,
            "capture": capture_result,
            "feedback": "",
        });
        save_report(&report, &out_dir);
        return report;
    }

    if std::env::var_os(provider_key).is_none() {
        ctx.log("visual review: no API key — skipping vision agent");
        let report = json!({
            "ts": ctx::now(),
            "ok": true,
            "findings": [],
            "summary": "No API key for vision model — screenshots saved without agent review.",
            "screenshots": screenshots,
            "capture": capture_result,
            "feedback": "",
        });
        save_report(&report, &out_dir);
        return report;
    }

    let task = build_vision_task(&capture_result, &iteration_summary, &pages);
    ctx.log("visual review: running vision agent...");
    let agent_text = run_vision_agent(ctx, &task, &vision_model);
    let agent_text = match agent_text {
        Some(t) if !t.is_empty() => t,
        _ => {
            // The vision agent produced no output even after a retry — make it EXPLICIT (ok:false),
            // not a silent clean pass. (A bare `None` and an empty string both land here, matching
            // `if not agent_text:`.)
            ctx.log(
                "visual review: vision agent produced no output (timeout/unavailable) — \
                 review SKIPPED, not a clean pass",
            );
            let report = json!({
                "ts": ctx::now(),
                "ok": false,
                "error": "vision agent produced no output",
                "findings": [],
                "summary": "Visual review SKIPPED — vision agent unavailable (no output after retry).",
                "screenshots": screenshots,
                "capture": capture_result,
                "feedback": "",
            });
            save_report(&report, &out_dir);
            return report;
        }
    };

    let findings = parse_findings(&agent_text);
    // summary = _parse_summary(agent_text) or agent_text[:200]
    let parsed_summary = parse_summary(&agent_text);
    let summary = if parsed_summary.is_empty() {
        char_slice(&agent_text, 200)
    } else {
        parsed_summary
    };

    // 5. Build feedback for the RSI loop.
    let feedback = build_feedback(&findings, &summary);

    let (n_pages, n_console, n_network) = capture_counts(&capture_result);
    let report = json!({
        "ts": ctx::now(),
        "ok": true,
        "findings": findings,
        "summary": summary,
        "screenshots": screenshots,
        "capture": {
            "pages": n_pages,
            "console_errors": n_console,
            "network_errors": n_network,
        },
        "feedback": feedback,
        // agent_raw = agent_text[:2000] if agent_text else "" — agent_text is non-empty here.
        "agent_raw": char_slice(&agent_text, 2000),
    });
    save_report(&report, &out_dir);
    let fb = report.get("feedback").and_then(Value::as_str).unwrap_or("");
    ctx.log(&format!(
        "visual review: complete — {} findings, feedback: {}",
        report.get("findings").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0),
        char_slice(fb, 100)
    ));
    report
}

// ---------------------------------------------------------------------------
// _run_capture — drive the persistent agent-browser over the pages.
// ---------------------------------------------------------------------------

/// `_run_capture` — boot the persistent, policy-bounded agent-browser, navigate each page, and
/// capture screenshots (base64) + a11y trees + console/network errors. Returns `Some({ok, pages})`
/// or `None` on total failure (any error, or every page produced no screenshot — a silent ok with
/// zero screenshots would let the mandatory visual gate pass blind). Never raises.
fn run_capture(
    base_url: &str,
    pages: &[String],
    runtime_dir: &Path,
    repo_path: &Path,
) -> Option<Value> {
    let mut captured: Vec<Value> = Vec::new();
    // `with AgentBrowser(...) as browser:` — the whole block is wrapped in try/except → None.
    let mut browser = match AgentBrowser::open(repo_path, runtime_dir, &[base_url.to_string()]) {
        Ok(b) => b,
        Err(_) => return None,
    };

    // `for page in pages or ["/"]:` — pages already defaults to ["/"] from config_pages, but the
    // loop's own `or ["/"]` guard is preserved for the empty-list case.
    let iter_pages: Vec<String> = if pages.is_empty() {
        vec!["/".to_string()]
    } else {
        pages.to_vec()
    };

    for page in &iter_pages {
        // path = str(page or "/")  — an empty page string becomes "/".
        let path = if page.is_empty() { "/".to_string() } else { page.clone() };
        let url = format!(
            "{}/{}",
            base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let state = browser.navigate(&url);

        let mut item = Map::new();
        item.insert("path".into(), json!(path));
        item.insert(
            "console_errors".into(),
            state
                .get("consoleErrors")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([])),
        );
        item.insert(
            "network_errors".into(),
            state
                .get("networkErrors")
                .filter(|v| !v.is_null())
                .cloned()
                .unwrap_or_else(|| json!([])),
        );

        if state.get("ok").and_then(Value::as_bool) != Some(true) {
            // nav_error = state.get("error") or "navigation failed"
            let nav_err = non_empty_str(state.get("error")).unwrap_or("navigation failed");
            item.insert("nav_error".into(), json!(nav_err));
            captured.push(Value::Object(item));
            continue;
        }

        // Only embed the frame when THIS page's capture is fresh (observe() sets state["frame"] only
        // when the screenshot CLI succeeded). frame_file is a reused fixed path, so reading it
        // unconditionally would embed the PRIOR page's image on this page's failure.
        let has_frame = state.get("frame").map(|v| frame_truthy(v)).unwrap_or(false);
        if has_frame {
            match std::fs::read(&browser.frame_file) {
                Ok(bytes) => {
                    let b64 = b64_encode(&bytes);
                    item.insert("screenshot_b64".into(), json!(b64));
                }
                Err(_) => {
                    item.insert("screenshot_b64".into(), json!(""));
                }
            }
        } else {
            item.insert("screenshot_b64".into(), json!(""));
        }

        // a11y_yaml = "\n".join("- {role}: {name} [{ref}]" for element in elements if dict)
        let a11y = build_a11y_yaml(state.get("elements"));
        item.insert("a11y_yaml".into(), json!(a11y));
        captured.push(Value::Object(item));
    }
    drop(browser); // `with` block exit — close the session.

    // If NO page produced a screenshot, the capture is effectively unavailable → None.
    let any_shot = captured.iter().any(|p| {
        p.get("screenshot_b64")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    });
    if !captured.is_empty() && !any_shot {
        return None;
    }
    Some(json!({"ok": true, "pages": captured}))
}

/// `state["frame"]` is truthy when observe() set it to a dict (`{seq,mime,available}`); it is absent
/// otherwise. Python's `if state.get("frame"):` is truthy for any non-empty dict.
fn frame_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Object(o) => !o.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
    }
}

/// a11y join from `state["elements"]`: `"- {role or 'element'}: {name or ''} [{ref or ''}]"` per
/// dict element, skipping non-dict entries. None/missing elements → empty string.
fn build_a11y_yaml(elements: Option<&Value>) -> String {
    let arr = match elements.and_then(Value::as_array) {
        Some(a) => a,
        None => return String::new(),
    };
    let mut lines: Vec<String> = Vec::new();
    for el in arr {
        let obj = match el.as_object() {
            Some(o) => o,
            None => continue, // isinstance(element, dict) filter
        };
        let role = obj.get("role").and_then(Value::as_str).unwrap_or("element");
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("");
        let r = obj.get("ref").and_then(Value::as_str).unwrap_or("");
        lines.push(format!("- {role}: {name} [{r}]"));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// _save_screenshots
// ---------------------------------------------------------------------------

/// `_save_screenshots` — write each page's base64 screenshot to `screenshot_NN.png`. Returns a list
/// of `{path, page_path, filename}` dicts (skipping pages without a screenshot, and any that fail to
/// decode/write — `except Exception: pass`).
fn save_screenshots(capture_result: &Value, out_dir: &Path) -> Value {
    let mut screenshots: Vec<Value> = Vec::new();
    let pages = capture_result
        .get("pages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (i, page_data) in pages.iter().enumerate() {
        let b64 = page_data.get("screenshot_b64").and_then(Value::as_str).unwrap_or("");
        if b64.is_empty() {
            continue;
        }
        let fname = format!("screenshot_{i:02}.png"); // f"screenshot_{i:02d}.png"
        let fpath = out_dir.join(&fname);
        // try: write_bytes(b64decode(b64)) ... except Exception: pass
        let decoded = match b64_decode(b64) {
            Ok(d) => d,
            Err(_) => continue,
        };
        if std::fs::write(&fpath, &decoded).is_err() {
            continue;
        }
        let page_path = page_data.get("path").and_then(Value::as_str).unwrap_or("/");
        screenshots.push(json!({
            "path": fpath.to_string_lossy(),
            "page_path": page_path,
            "filename": fname,
        }));
    }
    Value::Array(screenshots)
}

// ---------------------------------------------------------------------------
// _build_vision_task
// ---------------------------------------------------------------------------

/// `_build_vision_task` — build the pi task text: iteration summary header, per-page console/network
/// errors (capped at 10 each), nav error, a11y tree (trimmed to ~2000 chars), and a screenshot
/// reference line. `pages_config` is accepted for parity but unused (matches the Python).
fn build_vision_task(capture_result: &Value, iteration_summary: &str, _pages_config: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "RSI ITERATION SUMMARY (what the coder changed):\n{iteration_summary}\n"
    ));
    let pages = capture_result.get("pages").and_then(Value::as_array);
    let n_pages = pages.map(|a| a.len()).unwrap_or(0);
    parts.push(format!("\nPAGES CAPTURED: {n_pages}\n"));

    let empty: Vec<Value> = Vec::new();
    for page_data in pages.unwrap_or(&empty) {
        let path = page_data.get("path").and_then(Value::as_str).unwrap_or("/");
        parts.push(format!("\n--- PAGE: {path} ---"));

        // console errors
        let console = page_data.get("console_errors").and_then(Value::as_array);
        match console {
            Some(c) if !c.is_empty() => {
                parts.push("CONSOLE ERRORS:".to_string());
                for e in c.iter().take(10) {
                    parts.push(format!("  ! {}", value_to_py_str(e)));
                }
            }
            _ => parts.push("CONSOLE ERRORS: none".to_string()),
        }

        // network errors
        let network = page_data.get("network_errors").and_then(Value::as_array);
        match network {
            Some(n) if !n.is_empty() => {
                parts.push("NETWORK ERRORS:".to_string());
                for e in n.iter().take(10) {
                    parts.push(format!("  ! {}", value_to_py_str(e)));
                }
            }
            _ => parts.push("NETWORK ERRORS: none".to_string()),
        }

        // nav error
        if let Some(nav_err) = non_empty_str(page_data.get("nav_error")) {
            parts.push(format!("NAVIGATION ERROR: {nav_err}"));
        }

        // a11y tree (trimmed to ~2000 chars)
        let a11y_raw = page_data.get("a11y_yaml").and_then(Value::as_str).unwrap_or("");
        let a11y = a11y_raw.trim().to_string();
        if !a11y.is_empty() {
            let a11y = if char_len(&a11y) > 2000 {
                format!("{}\n... (truncated)", char_slice(&a11y, 2000))
            } else {
                a11y
            };
            parts.push(format!("ACCESSIBILITY TREE:\n{a11y}"));
        }

        // screenshot reference
        let b64 = page_data.get("screenshot_b64").and_then(Value::as_str).unwrap_or("");
        if !b64.is_empty() {
            parts.push(format!(
                "SCREENSHOT: [base64 PNG, {} chars — attached as image input]",
                char_len(b64)
            ));
        } else {
            parts.push("SCREENSHOT: (capture failed for this page)".to_string());
        }
    }

    parts.push("\n\nReview this app per the visual_review.md contract. Output your findings.".to_string());
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// _run_vision_agent / _final_text
// ---------------------------------------------------------------------------

/// `_run_vision_agent` — spawn the pi vision agent with the `vision-cloud` provider. Returns the
/// agent's final assistant text, or `None` when pi is not on PATH / both attempts (120s, 180s)
/// timed out or produced empty output. A timeout/empty first attempt is retried ONCE with the longer
/// timeout so a slow cold-start is not misread as a clean pass. Never raises.
fn run_vision_agent(ctx: &Ctx, task: &str, vision_model: &str) -> Option<String> {
    let pi = which::which("pi").ok()?; // pi = shutil.which("pi"); if not pi: return None
    let pi = pi.to_string_lossy().to_string();
    let vision_ext = vision_ext(ctx);
    let visual_md = visual_review_md(ctx);

    let args: Vec<String> = vec![
        pi,
        "--print".into(),
        "--mode".into(),
        "json".into(),
        "--provider".into(),
        "vision-cloud".into(),
        "--model".into(),
        vision_model.into(),
        "-e".into(),
        vision_ext.to_string_lossy().into_owned(),
        "--append-system-prompt".into(),
        visual_md.to_string_lossy().into_owned(),
        "--no-tools".into(),
        task.into(),
    ];

    // env = dict(os.environ) + RSI_PROVIDER/RSI_VISION_MODEL; pop PYTHONPATH/PYTHONHOME.
    for timeout in [120u64, 180u64] {
        match spawn_pi(ctx, &args, vision_model, Duration::from_secs(timeout)) {
            Some(stdout) => {
                let text = final_text(&stdout);
                if !text.is_empty() {
                    return Some(text);
                }
                // empty → fall through to the longer-timeout retry (or exit the loop).
            }
            None => continue, // (subprocess.TimeoutExpired, OSError) → continue
        }
    }
    None
}

/// Spawn pi with the scrubbed/augmented env and a hard timeout. `Some(stdout)` on a (possibly
/// non-zero exit) completion; `None` on timeout or spawn failure (the `(TimeoutExpired, OSError)`
/// branch). Output is captured in full (pi's JSON stream can be large), drained on a thread so a
/// full stderr pipe never deadlocks the timeout wait.
fn spawn_pi(ctx: &Ctx, args: &[String], vision_model: &str, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    ctx.apply_clean_env(&mut cmd); // also strips PYTHONPATH/PYTHONHOME, forces UTF-8 stdio
    cmd.env("RSI_PROVIDER", "vision-cloud");
    cmd.env("RSI_VISION_MODEL", vision_model);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_hidden(&mut cmd);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return None, // OSError (pi not executable) → caller continues
    };

    // Drain stderr on a thread; read stdout after the timed wait. (pi can emit a large JSONL stream.)
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stderr_handle = stderr_pipe.take().map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
        })
    });

    use wait_timeout::ChildExt;
    match child.wait_timeout(timeout) {
        Ok(Some(_status)) => {}
        Ok(None) => {
            // TimeoutExpired
            let _ = child.kill();
            let _ = child.wait();
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            return None;
        }
        Err(_) => {
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            return None;
        }
    }

    let mut out = Vec::new();
    if let Some(mut s) = stdout_pipe.take() {
        let _ = s.read_to_end(&mut out);
    }
    if let Some(h) = stderr_handle {
        let _ = h.join();
    }
    // text=True, encoding="utf-8", errors="replace"
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// `_final_text` — extract the last assistant text from a pi `--mode json` JSONL event stream.
/// Sources: an `agent_end` event's `messages` array, or a top-level `message` object. Only
/// `role=="assistant"` messages contribute; text is the concatenation of `type=="text"` content
/// parts. The LAST non-empty assistant text wins. Returns the stripped result (or empty string).
fn final_text(stdout: &str) -> String {
    let mut final_text = String::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let ev: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // JSONDecodeError
        };
        let msgs: Vec<Value> = if ev.get("type").and_then(Value::as_str) == Some("agent_end") {
            ev.get("messages").and_then(Value::as_array).cloned().unwrap_or_default()
        } else if ev.get("message").map(Value::is_object).unwrap_or(false) {
            vec![ev.get("message").cloned().unwrap_or(Value::Null)]
        } else {
            continue;
        };
        for m in &msgs {
            let obj = match m.as_object() {
                Some(o) => o,
                None => continue,
            };
            if obj.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let t: String = obj
                .get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| {
                            let po = part.as_object()?;
                            if po.get("type").and_then(Value::as_str) == Some("text") {
                                Some(po.get("text").and_then(Value::as_str).unwrap_or(""))
                            } else {
                                None
                            }
                        })
                        .collect::<String>()
                })
                .unwrap_or_default();
            if !t.is_empty() {
                final_text = t;
            }
        }
    }
    final_text.trim().to_string()
}

// ---------------------------------------------------------------------------
// _parse_findings / _parse_summary
// ---------------------------------------------------------------------------

/// `_parse_findings` — parse the `===FINDINGS=== … ===END===` block. Each in-block line containing a
/// `|` is split on `|` (max 2 splits → 3 parts); when ≥3 parts, emit
/// `{severity:lower, category:lower, description}` (description kept as-is). Empty list if no block.
fn parse_findings(text: &str) -> Value {
    let mut findings: Vec<Value> = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let s = line.trim();
        if s == "===FINDINGS===" {
            in_block = true;
            continue;
        }
        if s == "===END===" {
            break;
        }
        if in_block && s.contains('|') {
            // [p.strip() for p in s.split("|", 2)] — Python maxsplit=2 → at most 3 parts.
            let raw: Vec<&str> = s.splitn(3, '|').collect();
            let parts: Vec<String> = raw.iter().map(|p| p.trim().to_string()).collect();
            if parts.len() >= 3 {
                findings.push(json!({
                    "severity": parts[0].to_lowercase(),
                    "category": parts[1].to_lowercase(),
                    "description": parts[2],
                }));
            }
        }
    }
    Value::Array(findings)
}

/// `_parse_summary` — return the text after the first line that (case-insensitively) starts with
/// `SUMMARY:`, stripped. Empty string if none.
fn parse_summary(text: &str) -> String {
    for line in text.lines() {
        let s = line.trim();
        if s.to_uppercase().starts_with("SUMMARY:") {
            // s[len("SUMMARY:"):].strip() — slice by BYTES; "SUMMARY:" is ASCII so byte==char here.
            return s[("SUMMARY:".len())..].trim().to_string();
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// _build_feedback
// ---------------------------------------------------------------------------

/// `_build_feedback` — one-time feedback for the next RSI iteration. Empty when there are no findings
/// or only info/pass severities. Otherwise lists CRITICAL then warning items as `  - [{category}]
/// {description}`, the review summary, and a standing instruction line.
fn build_feedback(findings: &Value, summary: &str) -> String {
    let arr = match findings.as_array() {
        Some(a) if !a.is_empty() => a,
        _ => return String::new(), // if not findings: return ""
    };
    let critical: Vec<&Value> = arr
        .iter()
        .filter(|f| f.get("severity").and_then(Value::as_str) == Some("critical"))
        .collect();
    let warnings: Vec<&Value> = arr
        .iter()
        .filter(|f| f.get("severity").and_then(Value::as_str) == Some("warning"))
        .collect();
    if critical.is_empty() && warnings.is_empty() {
        return String::new(); // only info/pass — no actionable feedback
    }

    let mut parts: Vec<String> =
        vec!["VISUAL REVIEW FEEDBACK (from the previous iteration's E2E sandbox review):".to_string()];
    if !critical.is_empty() {
        parts.push("CRITICAL issues found:".to_string());
        for f in &critical {
            parts.push(format!(
                "  - [{}] {}",
                f.get("category").and_then(Value::as_str).unwrap_or(""),
                f.get("description").and_then(Value::as_str).unwrap_or("")
            ));
        }
    }
    if !warnings.is_empty() {
        parts.push("Warnings:".to_string());
        for f in &warnings {
            parts.push(format!(
                "  - [{}] {}",
                f.get("category").and_then(Value::as_str).unwrap_or(""),
                f.get("description").and_then(Value::as_str).unwrap_or("")
            ));
        }
    }
    parts.push(format!("\nReview summary: {summary}"));
    parts.push(
        "Address the most critical visual/functional issue above in this iteration if it \
         falls within the current backlog item's scope. Otherwise, note it for a future item."
            .to_string(),
    );
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// _save_report
// ---------------------------------------------------------------------------

/// `_save_report` — write a slim `report.json` (dropping the heavy `capture`/`agent_raw` keys) with
/// `json.dumps(indent=2)`. Best-effort: an OSError on write is swallowed.
fn save_report(report: &Value, out_dir: &Path) {
    let slim: Value = match report.as_object() {
        Some(o) => {
            let mut m = Map::new();
            for (k, v) in o {
                if k != "capture" && k != "agent_raw" {
                    m.insert(k.clone(), v.clone());
                }
            }
            Value::Object(m)
        }
        None => report.clone(),
    };
    if let Ok(body) = serde_json::to_string_pretty(&slim) {
        let _ = std::fs::write(out_dir.join("report.json"), body);
    }
}

// ---------------------------------------------------------------------------
// capture helpers
// ---------------------------------------------------------------------------

/// `capture["pages"]` count + summed console/network error counts for the success-path `capture`
/// dict: `{pages:int, console_errors:int, network_errors:int}`.
fn capture_counts(capture_result: &Value) -> (i64, i64, i64) {
    let pages = capture_result.get("pages").and_then(Value::as_array);
    let n_pages = pages.map(|a| a.len() as i64).unwrap_or(0);
    let mut console = 0i64;
    let mut network = 0i64;
    if let Some(arr) = pages {
        for p in arr {
            console += p.get("console_errors").and_then(Value::as_array).map(|a| a.len() as i64).unwrap_or(0);
            network += p.get("network_errors").and_then(Value::as_array).map(|a| a.len() as i64).unwrap_or(0);
        }
    }
    (n_pages, console, network)
}

/// sandbox_config.get("pages") or ["/"] — coerce each element to its string form (str(page)).
fn config_pages(sandbox_config: &Value) -> Vec<String> {
    match sandbox_config.get("pages").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => a.iter().map(value_to_py_str).collect(),
        _ => vec!["/".to_string()],
    }
}

/// sandbox_config.get("launch") or "" — a list launch stays a list in Sandbox, but `run()` only
/// tests truthiness of the raw value. A list/str/number is truthy when non-empty/non-zero.
fn config_launch(sandbox_config: &Value) -> String {
    match sandbox_config.get("launch") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) if !a.is_empty() => "<list>".to_string(), // truthy sentinel
        Some(v) if frame_truthy(v) => value_to_py_str(v),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// AgentBrowser — thin Rust port of improver/agent_browser.py (shell to the agent-browser CLI).
// CDP is NOT reimplemented; we drive the external `agent-browser` exe exactly as the Python did.
// ---------------------------------------------------------------------------

struct AgentBrowser {
    repo_path: PathBuf,
    runtime_dir: PathBuf,
    state_file: PathBuf,
    frame_file: PathBuf,
    profile_dir: PathBuf,
    session_id: String,
    allowed_domains: Vec<String>,
    started: bool,
    closed: bool,
}

impl AgentBrowser {
    /// `AgentBrowser.__init__` + `__enter__` — set up the runtime/profile dirs and a unique session.
    fn open(repo_path: &Path, runtime_dir: &Path, allowed_origins: &[String]) -> std::io::Result<Self> {
        let runtime_dir = runtime_dir.to_path_buf();
        std::fs::create_dir_all(&runtime_dir)?;
        let profile_dir = runtime_dir.join("browser-profile");
        std::fs::create_dir_all(&profile_dir)?;
        // session_id = f"solomon-{uuid.uuid4().hex[:12]}" — shape only (see ctx run-id deviation).
        let session_id = format!("solomon-{}", short_hex(12));
        // allowed_domains = sorted unique hostnames of the origins.
        let mut domains: Vec<String> = allowed_origins
            .iter()
            .filter_map(|o| origin_host(o))
            .collect();
        domains.sort();
        domains.dedup();
        Ok(AgentBrowser {
            repo_path: std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf()),
            state_file: runtime_dir.join("browser_state.json"),
            frame_file: runtime_dir.join("browser_frame.jpg"),
            runtime_dir,
            profile_dir,
            session_id,
            allowed_domains: domains,
            started: false,
            closed: false,
        })
    }

    /// `_base_command` — resolve the agent-browser binary (SOLOMON_AGENT_BROWSER env override or PATH;
    /// the Win32 native node_modules path) and build the persistent-session argv prefix.
    fn base_command(&self) -> (Option<String>, Vec<String>) {
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut binary: Option<String> = std::env::var("SOLOMON_AGENT_BROWSER")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| which::which("agent-browser").ok().map(|p| p.to_string_lossy().into_owned()));
        #[cfg(windows)]
        if let Some(b) = &binary {
            let native = Path::new(b)
                .parent()
                .map(|p| p.join("node_modules/agent-browser/bin/agent-browser-win32-x64.exe"));
            if let Some(n) = native {
                if n.is_file() {
                    binary = Some(n.to_string_lossy().into_owned());
                }
            }
        }
        let mut command: Vec<String> = vec![
            binary.clone().unwrap_or_else(|| "agent-browser".to_string()),
            "--session".into(),
            self.session_id.clone(),
            "--profile".into(),
            self.profile_dir.to_string_lossy().into_owned(),
            "--json".into(),
            "--screenshot-format".into(),
            "jpeg".into(),
            "--screenshot-quality".into(),
            "70".into(),
        ];
        if !self.allowed_domains.is_empty() {
            command.push("--allowed-domains".into());
            command.push(self.allowed_domains.join(","));
        }
        (binary, command)
    }

    /// `_run_cli` — run one agent-browser subcommand, parse the last JSON line of stdout, and return
    /// `{ok:true, data}` on `success:true` (rc 0) or `{ok:false, error}` otherwise. Never raises.
    fn run_cli(&mut self, args: &[&str], timeout: u64) -> Value {
        let (binary, command) = self.base_command();
        if binary.is_none() {
            return json!({"ok": false, "error": "agent-browser 0.27.0 is not installed"});
        }
        let mut full: Vec<String> = command;
        full.extend(args.iter().map(|s| s.to_string()));
        let run = run_browser_cli(&full, &self.repo_path, Duration::from_secs(timeout));
        let (code, stdout, stderr) = match run {
            Some(r) => r,
            None => return json!({"ok": false, "error": "agent-browser command failed"}),
        };
        // payload = last parseable JSON line of stdout.
        let mut payload: Option<Value> = None;
        for line in stdout.lines().rev() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                payload = Some(v);
                break;
            }
        }
        let success = payload
            .as_ref()
            .and_then(|p| p.get("success"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if code != 0 || !payload.as_ref().map(Value::is_object).unwrap_or(false) || !success {
            let error = payload
                .as_ref()
                .and_then(|p| p.get("error"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .filter(|s| !s.is_empty())
                .or_else(|| (!stderr.is_empty()).then(|| stderr.clone()))
                .or_else(|| (!stdout.is_empty()).then(|| stdout.clone()))
                .unwrap_or_else(|| "agent-browser command failed".to_string());
            return json!({"ok": false, "error": truncate(&error, 300)});
        }
        self.started = true;
        let data = payload.and_then(|mut p| p.as_object_mut().and_then(|o| o.remove("data")));
        let data = match data {
            Some(Value::Object(o)) => Value::Object(o),
            Some(v) => json!({"value": v}),
            None => json!({"value": Value::Null}),
        };
        json!({"ok": true, "data": data})
    }

    /// `navigate` — open the URL (after the allowed-origin guard + liveness re-init) and `observe`.
    /// Returns a state dict; on failure a `{ok:false, error,…}` state (mirrors `_fail`).
    fn navigate(&mut self, url: &str) -> Value {
        if !self.url_allowed(url) {
            return self.fail("navigation is outside the allowed sandbox origins");
        }
        self.guard_alive();
        let result = self.run_cli(&["open", url], 30);
        if result.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = result.get("error").and_then(Value::as_str).unwrap_or("navigation failed");
            return self.fail(err);
        }
        self.observe()
    }

    /// `observe` — snapshot (refs → elements), screenshot (sets `frame` when it succeeds), console
    /// `errors`, and `network requests` (failures/4xx+ → network_errors). Returns the written state.
    fn observe(&mut self) -> Value {
        let snapshot = self.run_cli(&["snapshot", "-i", "-c"], 30);
        if snapshot.get("ok").and_then(Value::as_bool) != Some(true) {
            let err = snapshot.get("error").and_then(Value::as_str).unwrap_or("snapshot failed");
            return self.fail(err);
        }
        let data = snapshot.get("data").cloned().unwrap_or(Value::Null);
        let mut elements: Vec<Value> = Vec::new();
        if let Some(refs) = data.get("refs").and_then(Value::as_object) {
            for (r, value) in refs {
                let mut item = Map::new();
                item.insert("ref".into(), json!(r.strip_prefix('@').unwrap_or(r)));
                item.insert("role".into(), json!("element"));
                item.insert("name".into(), json!(""));
                if let Some(vo) = value.as_object() {
                    if let Some(role) = vo.get("role").filter(|v| !v.is_null()) {
                        item.insert("role".into(), role.clone());
                    }
                    if let Some(name) = vo.get("name").filter(|v| !v.is_null()) {
                        item.insert("name".into(), name.clone());
                    }
                } else {
                    item.insert("name".into(), json!(value_to_py_str(value)));
                }
                elements.push(Value::Object(item));
            }
        }
        let frame_path = self.frame_file.to_string_lossy().into_owned();
        let shot = self.run_cli(&["screenshot", &frame_path], 30);
        let console_result = self.run_cli(&["errors"], 30);
        let network_result = self.run_cli(&["network", "requests"], 30);
        let console_errors = if console_result.get("ok").and_then(Value::as_bool) == Some(true) {
            console_result
                .get("data")
                .and_then(|d| d.get("errors"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let requests = if network_result.get("ok").and_then(Value::as_bool) == Some(true) {
            network_result
                .get("data")
                .and_then(|d| d.get("requests"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let network_errors: Vec<Value> = requests
            .into_iter()
            .filter(|req| {
                let o = match req.as_object() {
                    Some(o) => o,
                    None => return false,
                };
                let has_failure = o.get("failure").map(|v| frame_truthy(v)).unwrap_or(false);
                let status = o.get("status").and_then(Value::as_i64).unwrap_or(0);
                has_failure || status >= 400
            })
            .collect();
        let frame_ok = shot.get("ok").and_then(Value::as_bool) == Some(true);
        self.write_state(true, Some(elements), frame_ok, console_errors, network_errors, None)
    }

    /// `_fail` — write + return an `{ok:false, status:"error", error}` state.
    fn fail(&mut self, error: &str) -> Value {
        self.write_state(false, None, false, Vec::new(), Vec::new(), Some(error))
    }

    /// `write_state` — persist the small `browser_state.json` (atomic) and RETURN the state dict.
    /// Only the keys `_run_capture` reads need to be faithful: `ok`, `frame`, `consoleErrors`,
    /// `networkErrors`, `elements`, `error`.
    fn write_state(
        &mut self,
        ok: bool,
        elements: Option<Vec<Value>>,
        frame_ok: bool,
        console_errors: Vec<Value>,
        network_errors: Vec<Value>,
        error: Option<&str>,
    ) -> Value {
        let els = elements.unwrap_or_default();
        let mut state = Map::new();
        state.insert("ok".into(), json!(ok));
        state.insert("elements".into(), Value::Array(els));
        state.insert("consoleErrors".into(), Value::Array(console_errors));
        state.insert("networkErrors".into(), Value::Array(network_errors));
        if frame_ok {
            // state["frame"] = {seq, mime, available} (a truthy dict).
            state.insert("frame".into(), json!({"mime": "image/jpeg", "available": true}));
        }
        if let Some(e) = error {
            state.insert("error".into(), json!(truncate(e, 300)));
        }
        // Best-effort atomic write (the dashboard reads this); failure is swallowed.
        if let Ok(body) = serde_json::to_string(&Value::Object(state.clone())) {
            let _ = std::fs::create_dir_all(&self.runtime_dir);
            let tmp = self.state_file.with_extension("json.tmp");
            if std::fs::write(&tmp, body).is_ok() {
                let _ = std::fs::rename(&tmp, &self.state_file);
            }
        }
        Value::Object(state)
    }

    /// `_origin`/`_url_allowed` — allow only http/https URLs whose origin is in the allowed set (by
    /// hostname here, matching how _base_command derives --allowed-domains).
    fn url_allowed(&self, url: &str) -> bool {
        let host = match origin_host(url) {
            Some(h) => h,
            None => return false,
        };
        self.allowed_domains.is_empty() || self.allowed_domains.contains(&host)
    }

    /// `_guard_alive`/`_probe_alive`/`_reinit_session` — before each action, if a started session is
    /// not responding, record a crashed state and re-init exactly once (next action re-creates it).
    fn guard_alive(&mut self) {
        if self.probe_alive() {
            return;
        }
        self.write_state(false, None, false, Vec::new(), Vec::new(),
                         Some("agent-browser session is not responding (process crashed)"));
        // _reinit_session: best-effort close + reset started.
        self.run_cli(&["close"], 10);
        self.started = false;
    }

    fn probe_alive(&mut self) -> bool {
        if !self.started {
            return true; // nothing to probe yet — the action itself starts it.
        }
        self.run_cli(&["get", "url"], 10).get("ok").and_then(Value::as_bool) == Some(true)
    }

    /// `close` — close the session (when a binary resolves) and remove the state/frame files +
    /// profile dir. Idempotent.
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        if self.base_command().0.is_some() {
            self.run_cli(&["close"], 10);
        }
        let _ = std::fs::remove_file(&self.state_file);
        let _ = std::fs::remove_file(&self.frame_file);
        let _ = std::fs::remove_dir_all(&self.profile_dir);
    }
}

impl Drop for AgentBrowser {
    fn drop(&mut self) {
        self.close();
    }
}

/// Run one agent-browser CLI invocation with a hard timeout, scrubbed/headed-off env, in `cwd`.
/// `Some((code, stdout, stderr))` on completion; `None` on timeout or spawn failure. Mirrors
/// agent_browser._clean_env (strip secret-shaped + PYTHONPATH/PYTHONHOME; force headless jpeg q70).
fn run_browser_cli(args: &[String], cwd: &Path, timeout: Duration) -> Option<(i32, String, String)> {
    let mut cmd = Command::new(&args[0]);
    cmd.args(&args[1..]);
    cmd.current_dir(cwd);
    // _clean_env: drop PYTHONPATH/PYTHONHOME/GITHUB_TOKEN/GH_TOKEN + any *PASSWORD*/*SECRET*/*CREDENTIAL*.
    cmd.env_remove("PYTHONPATH");
    cmd.env_remove("PYTHONHOME");
    cmd.env_remove("GITHUB_TOKEN");
    cmd.env_remove("GH_TOKEN");
    for (k, _) in std::env::vars() {
        let u = k.to_uppercase();
        if u.contains("PASSWORD") || u.contains("SECRET") || u.contains("CREDENTIAL") {
            cmd.env_remove(&k);
        }
    }
    cmd.env("AGENT_BROWSER_HEADED", "false");
    cmd.env("AGENT_BROWSER_SCREENSHOT_FORMAT", "jpeg");
    cmd.env("AGENT_BROWSER_SCREENSHOT_QUALITY", "70");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    apply_hidden(&mut cmd);

    let mut child = cmd.spawn().ok()?;
    let mut stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stderr_handle = stderr_pipe.map(|mut s| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf);
            buf
        })
    });

    use wait_timeout::ChildExt;
    let code = match child.wait_timeout(timeout) {
        Ok(Some(status)) => status.code().unwrap_or(-1),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            return None;
        }
        Err(_) => {
            if let Some(h) = stderr_handle {
                let _ = h.join();
            }
            return None;
        }
    };
    let mut out = Vec::new();
    if let Some(mut s) = stdout_pipe.take() {
        let _ = s.read_to_end(&mut out);
    }
    let err = stderr_handle
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Some((
        code,
        String::from_utf8_lossy(&out).into_owned(),
        String::from_utf8_lossy(&err).into_owned(),
    ))
}

// ---------------------------------------------------------------------------
// Sandbox — thin Rust port of improver/sandbox.py (boot an app on an ephemeral loopback port).
// ---------------------------------------------------------------------------

struct Sandbox {
    base_url: String,
    proc: Option<std::process::Child>,
    tmp_dir: Option<PathBuf>,
}

impl Sandbox {
    /// `Sandbox.__enter__` — pick a free loopback port, mirror the repo into a disposable worktree
    /// (skipping .git/.venv/node_modules/etc), launch the app with `{port}/{state}/{workdir}`
    /// placeholder substitution + port_env/state_env, and wait for the health endpoint to return 200.
    /// Returns `Err(message)` on any boot failure (the Python try/except in run() catches these).
    fn boot(config: &Value, repo_path: &Path) -> Result<Self, String> {
        let port = free_port().map_err(|e| format!("{e}"))?;
        let base_url = format!("http://127.0.0.1:{port}");

        // _create_isolation_root: copytree(repo -> tmp/worktree) skipping the ignore set.
        let tmp_dir = std::env::temp_dir().join(format!("solomon-sandbox-{}", short_hex(12)));
        let work_dir = tmp_dir.join("worktree");
        let state_dir = tmp_dir.join("state");
        std::fs::create_dir_all(&state_dir).map_err(|e| format!("{e}"))?;
        copy_tree_filtered(repo_path, &work_dir).map_err(|e| format!("{e}"))?;

        let mut sb = Sandbox {
            base_url: base_url.clone(),
            proc: None,
            tmp_dir: Some(tmp_dir),
        };

        // _command: launch list/str with {port}/{state}/{workdir} substitution.
        let argv = build_launch_argv(config, port, &state_dir, &work_dir, repo_path)?;
        if argv.is_empty() {
            return Err("sandbox config missing 'launch' command".to_string());
        }

        // _clean_sandbox_env + port_env/state_env + HOST=127.0.0.1.
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.current_dir(&work_dir);
        apply_sandbox_env(&mut cmd, config, port, &state_dir);
        cmd.env("HOST", "127.0.0.1");
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        apply_hidden_group(&mut cmd);

        let child = cmd.spawn().map_err(|e| format!("{e}"))?;
        sb.proc = Some(child);

        sb.wait_health(config)?;
        Ok(sb)
    }

    /// `_wait_health` — poll `{base_url}{health or "/"}` until a 200, the process exits early, or the
    /// boot_timeout (default 30s) elapses. Best-effort HTTP GET via the `ureq` client if available;
    /// see DEVIATION note — falls back to a TCP-connect probe when no HTTP client is present.
    fn wait_health(&mut self, config: &Value) -> Result<(), String> {
        let health = config.get("health").and_then(Value::as_str).unwrap_or("/");
        let timeout = config
            .get("boot_timeout")
            .and_then(Value::as_i64)
            .unwrap_or(30)
            .max(1) as u64;
        let url = format!("{}{}", self.base_url, health);
        let deadline = Instant::now() + Duration::from_secs(timeout);
        let mut last_error = String::new();
        while Instant::now() < deadline {
            if let Some(child) = self.proc.as_mut() {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!(
                        "sandbox process exited early (code {})",
                        status.code().unwrap_or(-1)
                    ));
                }
            }
            match http_get_200(&url) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => last_error = truncate(&e, 200),
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!(
            "sandbox did not become healthy within {timeout}s: {last_error}"
        ))
    }

    fn cleanup(&mut self) {
        if let Some(mut child) = self.proc.take() {
            // Kill the whole tree (taskkill /F /T on Windows; kill() elsewhere).
            #[cfg(windows)]
            {
                let _ = proc::run(
                    &["taskkill", "/F", "/T", "/PID", &child.id().to_string()],
                    None,
                    Some(Duration::from_secs(10)),
                );
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(dir) = self.tmp_dir.take() {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// `_command` placeholder substitution: replace only `{port}`/`{state}`/`{workdir}` literals so a
/// literal `{` in a launch arg passes through. Resolve a `.venv/`/`venv/` first arg against the repo.
fn build_launch_argv(
    config: &Value,
    port: u16,
    state_dir: &Path,
    work_dir: &Path,
    repo_path: &Path,
) -> Result<Vec<String>, String> {
    let launch = config.get("launch");
    let mut args: Vec<String> = match launch {
        Some(Value::Array(a)) => a.iter().map(value_to_py_str).collect(),
        Some(Value::String(s)) if !s.trim().is_empty() => shlex_split(s),
        _ => return Err("sandbox config missing 'launch' command".to_string()),
    };
    let port_s = port.to_string();
    let state_s = state_dir.to_string_lossy().into_owned();
    let work_s = work_dir.to_string_lossy().into_owned();
    for a in &mut args {
        *a = a
            .replace("{port}", &port_s)
            .replace("{state}", &state_s)
            .replace("{workdir}", &work_s);
    }
    if let Some(first) = args.first().cloned() {
        let sep = std::path::MAIN_SEPARATOR;
        let normalized = first.replace('/', &sep.to_string()).replace('\\', &sep.to_string());
        let venv_prefix = format!(".venv{sep}");
        let venv_prefix2 = format!("venv{sep}");
        if (normalized.starts_with(&venv_prefix) || normalized.starts_with(&venv_prefix2))
            && repo_path.join(&first).exists()
        {
            args[0] = repo_path
                .join(&first)
                .canonicalize()
                .unwrap_or_else(|_| repo_path.join(&first))
                .to_string_lossy()
                .into_owned();
        }
    }
    Ok(args)
}

/// `_clean_sandbox_env`: keep only the SAFE inherited env keys, re-root HOME/USERPROFILE/APPDATA/
/// LOCALAPPDATA/TEMP/TMP into the state dir, then layer extra_env + port_env/state_env. Reject
/// secret-shaped extra keys (best-effort: we just skip them rather than raising, since run() would
/// turn the ValueError into an ok:false error anyway).
fn apply_sandbox_env(cmd: &mut Command, config: &Value, port: u16, state_dir: &Path) {
    const SAFE: &[&str] = &[
        "PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "COMSPEC", "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE", "PROCESSOR_IDENTIFIER", "OS", "LANG", "TZ",
    ];
    // Start from an empty environment, then add back the safe keys.
    cmd.env_clear();
    let safe_upper: Vec<String> = SAFE.iter().map(|s| s.to_string()).collect();
    for (k, v) in std::env::vars() {
        if safe_upper.iter().any(|s| s.eq_ignore_ascii_case(&k)) {
            cmd.env(&k, &v);
        }
    }
    let root = state_dir; // _clean_sandbox_env roots at state_root (the sandbox state dir)
    let temp = root.join("temp");
    let appdata = root.join("appdata");
    let local = root.join("localappdata");
    let _ = std::fs::create_dir_all(&temp);
    let _ = std::fs::create_dir_all(&appdata);
    let _ = std::fs::create_dir_all(&local);
    cmd.env("HOME", root)
        .env("USERPROFILE", root)
        .env("APPDATA", &appdata)
        .env("LOCALAPPDATA", &local)
        .env("TEMP", &temp)
        .env("TMP", &temp)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("PYTHONNOUSERSITE", "1");
    // extra_env (skip secret-shaped), then port_env/state_env overlays.
    if let Some(extra) = config.get("extra_env").and_then(Value::as_object) {
        for (k, v) in extra {
            if !secret_shaped(k) {
                cmd.env(k, value_to_py_str(v));
            }
        }
    }
    if let Some(port_env) = config.get("port_env").and_then(Value::as_str) {
        if !port_env.is_empty() {
            cmd.env(port_env, port.to_string());
        }
    }
    if let Some(state_env) = config.get("state_env").and_then(Value::as_str) {
        if !state_env.is_empty() {
            cmd.env(state_env, state_dir.to_string_lossy().into_owned());
        }
    }
}

fn secret_shaped(name: &str) -> bool {
    let u = name.to_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "AUTH"]
        .iter()
        .any(|w| u.contains(w))
}

/// `_copy_ignore` mirror: copytree skipping the fixed ignore set + `.env`/`.env.*`/`*.pem`.
fn copy_tree_filtered(src: &Path, dst: &Path) -> std::io::Result<()> {
    const FIXED: &[&str] = &[
        ".git", ".venv", "venv", "node_modules", "__pycache__", ".pytest_cache",
        ".mypy_cache", ".ruff_cache", ".tox", "runtime", "build",
    ];
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_lowercase();
        if FIXED.contains(&name.as_str())
            || lower == ".env"
            || lower.starts_with(".env.")
            || lower.ends_with(".pem")
        {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_tree_filtered(&from, &to)?;
        } else {
            let _ = std::fs::copy(&from, &to);
        }
    }
    Ok(())
}

/// `_free_port`: bind a TCP socket to 127.0.0.1:0 and read back the OS-assigned port.
fn free_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// Health probe. DEVIATION: sandbox.py uses urllib (an HTTP 200 check). The Rust port has no HTTP
/// client dependency declared for this module, so this is a best-effort TCP-connect probe: a
/// successful connection to the loopback port is treated as healthy. Marked ponytail — swap in a
/// real `ureq`/`reqwest` 200 check (matching `health` path + status==200) when one is available.
fn http_get_200(url: &str) -> Result<bool, String> {
    // ponytail: TCP-connect liveness instead of an HTTP 200 on the health path.
    let host_port = url
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let authority = host_port.split('/').next().unwrap_or(host_port);
    match std::net::TcpStream::connect_timeout(
        &authority
            .parse()
            .map_err(|_| format!("bad addr {authority}"))?,
        Duration::from_secs(2),
    ) {
        Ok(_) => Ok(true),
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// small shared helpers
// ---------------------------------------------------------------------------

#[cfg(windows)]
fn apply_hidden(cmd: &mut Command) {
    cmd.creation_flags(proc::CREATE_NO_WINDOW);
}
#[cfg(not(windows))]
fn apply_hidden(_cmd: &mut Command) {}

#[cfg(windows)]
fn apply_hidden_group(cmd: &mut Command) {
    cmd.creation_flags(proc::CREATE_NO_WINDOW | proc::CREATE_NEW_PROCESS_GROUP);
}
#[cfg(not(windows))]
fn apply_hidden_group(_cmd: &mut Command) {}

// Standard base64 (RFC 4648) encode/decode — no external crate (base64 is not a declared dep, and
// Cargo.toml is off-limits). Matches Python's base64.b64encode(...).decode("ascii") / b64decode(...).
const B64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(B64_ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(B64_ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64_ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64_ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn b64_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        if chunk.is_empty() {
            break;
        }
        let mut n = 0u32;
        let mut pad = 0;
        for (i, &c) in chunk.iter().enumerate() {
            if c == b'=' {
                pad += 1;
                n <<= 6;
            } else {
                let v = val(c).ok_or(())?;
                let _ = i;
                n = (n << 6) | v;
            }
        }
        // pad remaining sextets if the final chunk is short (lenient like Python b64decode).
        for _ in 0..(4 - chunk.len()) {
            n <<= 6;
            pad += 1;
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// str(...) of a JSON scalar the way Python would render it in an f-string (used for error lines and
/// page coercion). Objects/arrays fall back to their JSON form (rare; the inputs are scalars).
fn value_to_py_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// `state.get(key) or default` truthiness for an optional string: returns Some(non-empty str) only.
fn non_empty_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Truncate to `n` BYTES the way `str(e)[:n]` / `.encode` callers expect — but slice on a char
/// boundary so we never split a UTF-8 codepoint (matches Python's char-indexed slicing for the
/// error/feedback strings, which are short ASCII in practice).
fn truncate(s: &str, n: usize) -> String {
    char_slice(s, n)
}

/// Python `s[:n]` — slice the first `n` CHARS (not bytes). Python str slicing is by code point.
fn char_slice(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// Python `len(s)` over a str — number of code points.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Origin hostname for the allowed-origins / url-allowed checks (http/https only).
fn origin_host(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let host = authority.split('@').last().unwrap_or(authority);
    // strip a :port if present (but keep IPv6 simple — inputs here are 127.0.0.1:NNNNN).
    let host = host.split(':').next().unwrap_or(host);
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

/// A short lowercase-hex token of `n` chars (shape of `uuid.uuid4().hex[:n]`; uniqueness from a
/// time/PID/counter splitmix64 stream — see the ctx run-id deviation; never parsed as a UUID).
fn short_hex(n: usize) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos
        ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_mul(0xD1B5_4A32_D192_ED03);
    let mut out = String::new();
    while out.len() < n {
        // splitmix64
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out.truncate(n);
    out
}

/// Minimal shlex split for the str-form launch command (POSIX-ish: whitespace-separated, honoring
/// single/double quotes). sandbox.py uses shlex.split(posix=non-win32); on Windows it uses
/// posix=False. We implement the common quote-aware split that covers the launch strings in use.
fn shlex_split(s: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            '\\' if in_double => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token || !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token || !cur.is_empty() {
        args.push(cur);
    }
    args
}

// ---------------------------------------------------------------------------
// Tests — exact-string vectors for the load-bearing pure logic.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- _parse_findings ------------------------------------------------
    #[test]
    fn parse_findings_basic_block() {
        let text = "preamble\n\
                    ===FINDINGS===\n\
                    CRITICAL | Layout | The header overlaps the nav\n\
                    Warning | a11y | low contrast button\n\
                    junk line without pipe\n\
                    ===END===\n\
                    trailing | ignored | because after END";
        let f = parse_findings(text);
        let arr = f.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["severity"], json!("critical"));
        assert_eq!(arr[0]["category"], json!("layout"));
        assert_eq!(arr[0]["description"], json!("The header overlaps the nav"));
        assert_eq!(arr[1]["severity"], json!("warning"));
        assert_eq!(arr[1]["category"], json!("a11y"));
    }

    #[test]
    fn parse_findings_maxsplit_two_keeps_pipes_in_description() {
        // split("|", 2) → description retains any further pipes.
        let text = "===FINDINGS===\nWARN | nav | a | b | c\n===END===";
        let f = parse_findings(text);
        let arr = f.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["description"], json!("a | b | c"));
    }

    #[test]
    fn parse_findings_no_block_is_empty() {
        assert_eq!(parse_findings("no markers | here | at all"), json!([]));
        assert_eq!(parse_findings(""), json!([]));
    }

    #[test]
    fn parse_findings_two_parts_skipped() {
        let text = "===FINDINGS===\nonly | two\n===END===";
        assert_eq!(parse_findings(text), json!([]));
    }

    // ---- _parse_summary -------------------------------------------------
    #[test]
    fn parse_summary_case_insensitive_prefix() {
        assert_eq!(parse_summary("noise\nsummary:  all good \nmore"), "all good");
        assert_eq!(parse_summary("SUMMARY: The page renders cleanly."), "The page renders cleanly.");
    }

    #[test]
    fn parse_summary_absent_is_empty() {
        assert_eq!(parse_summary("no summary line here"), "");
        assert_eq!(parse_summary(""), "");
    }

    // ---- _build_feedback ------------------------------------------------
    #[test]
    fn build_feedback_empty_when_no_findings() {
        assert_eq!(build_feedback(&json!([]), "sum"), "");
    }

    #[test]
    fn build_feedback_empty_when_only_info() {
        let findings = json!([{"severity": "info", "category": "x", "description": "d"}]);
        assert_eq!(build_feedback(&findings, "sum"), "");
    }

    #[test]
    fn build_feedback_critical_and_warning() {
        let findings = json!([
            {"severity": "critical", "category": "layout", "description": "overlap"},
            {"severity": "warning", "category": "a11y", "description": "contrast"},
            {"severity": "info", "category": "z", "description": "ignored"}
        ]);
        let fb = build_feedback(&findings, "looks rough");
        let expected = "VISUAL REVIEW FEEDBACK (from the previous iteration's E2E sandbox review):\n\
                        CRITICAL issues found:\n\
                        \u{20}\u{20}- [layout] overlap\n\
                        Warnings:\n\
                        \u{20}\u{20}- [a11y] contrast\n\
                        \nReview summary: looks rough\n\
                        Address the most critical visual/functional issue above in this iteration if it falls within the current backlog item's scope. Otherwise, note it for a future item.";
        assert_eq!(fb, expected);
    }

    // ---- _final_text ----------------------------------------------------
    #[test]
    fn final_text_agent_end_event() {
        let stream = r#"{"type":"start"}
{"type":"agent_end","messages":[{"role":"assistant","content":[{"type":"text","text":"hello "},{"type":"text","text":"world"}]}]}"#;
        assert_eq!(final_text(stream), "hello world");
    }

    #[test]
    fn final_text_message_object_and_last_wins() {
        let stream = "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"first\"}]}}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"second\"}]}}";
        assert_eq!(final_text(stream), "second");
    }

    #[test]
    fn final_text_skips_non_assistant_and_non_json() {
        let stream = "not json\n\
                      {\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ignore\"}]}}\n\
                      {\"type\":\"agent_end\",\"messages\":[{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"kept\"}]}]}";
        assert_eq!(final_text(stream), "kept");
    }

    #[test]
    fn final_text_empty_assistant_does_not_overwrite() {
        let stream = "{\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"keep\"}]}}\n\
                      {\"message\":{\"role\":\"assistant\",\"content\":[]}}";
        assert_eq!(final_text(stream), "keep");
    }

    #[test]
    fn final_text_none_or_empty() {
        assert_eq!(final_text(""), "");
    }

    // ---- _build_vision_task ---------------------------------------------
    #[test]
    fn build_vision_task_full_page() {
        let capture = json!({"pages": [{
            "path": "/dash",
            "console_errors": ["boom"],
            "network_errors": [],
            "a11y_yaml": "- button: Go [r1]",
            "screenshot_b64": "AAAA"
        }]});
        let task = build_vision_task(&capture, "added a button", &["/dash".to_string()]);
        assert!(task.starts_with("RSI ITERATION SUMMARY (what the coder changed):\nadded a button\n"));
        assert!(task.contains("\nPAGES CAPTURED: 1\n"));
        assert!(task.contains("\n--- PAGE: /dash ---"));
        assert!(task.contains("CONSOLE ERRORS:\n  ! boom"));
        assert!(task.contains("NETWORK ERRORS: none"));
        assert!(task.contains("ACCESSIBILITY TREE:\n- button: Go [r1]"));
        assert!(task.contains("SCREENSHOT: [base64 PNG, 4 chars — attached as image input]"));
        assert!(task.ends_with("Review this app per the visual_review.md contract. Output your findings."));
    }

    #[test]
    fn build_vision_task_nav_error_and_failed_shot() {
        let capture = json!({"pages": [{
            "path": "/x",
            "console_errors": [],
            "network_errors": ["500 GET /api"],
            "nav_error": "navigation failed",
            "screenshot_b64": ""
        }]});
        let task = build_vision_task(&capture, "s", &[]);
        assert!(task.contains("CONSOLE ERRORS: none"));
        assert!(task.contains("NETWORK ERRORS:\n  ! 500 GET /api"));
        assert!(task.contains("NAVIGATION ERROR: navigation failed"));
        assert!(task.contains("SCREENSHOT: (capture failed for this page)"));
    }

    // ---- a11y join ------------------------------------------------------
    #[test]
    fn a11y_yaml_join_and_filter() {
        let els = json!([
            {"role": "button", "name": "Go", "ref": "r1"},
            "not a dict",
            {"name": "anon"}
        ]);
        let out = build_a11y_yaml(Some(&els));
        assert_eq!(out, "- button: Go [r1]\n- element: anon []");
    }

    #[test]
    fn a11y_yaml_none_is_empty() {
        assert_eq!(build_a11y_yaml(None), "");
        assert_eq!(build_a11y_yaml(Some(&Value::Null)), "");
    }

    // ---- capture counts -------------------------------------------------
    #[test]
    fn capture_counts_sums() {
        let cap = json!({"pages": [
            {"console_errors": ["a", "b"], "network_errors": ["x"]},
            {"console_errors": [], "network_errors": ["y", "z"]}
        ]});
        assert_eq!(capture_counts(&cap), (2, 2, 3));
    }

    // ---- run_capture None-on-no-screenshots -----------------------------
    #[test]
    fn run_capture_all_failed_returns_none_semantics() {
        // Mirror the predicate directly: captured non-empty but no screenshot_b64 → None.
        let captured = vec![
            json!({"path": "/", "screenshot_b64": ""}),
            json!({"path": "/x", "nav_error": "navigation failed"}),
        ];
        let any_shot = captured.iter().any(|p| {
            p.get("screenshot_b64").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false)
        });
        assert!(!captured.is_empty() && !any_shot);
    }

    // ---- frame truthiness ----------------------------------------------
    #[test]
    fn frame_truthy_matches_python() {
        assert!(frame_truthy(&json!({"mime": "image/jpeg"})));
        assert!(!frame_truthy(&json!({})));
        assert!(!frame_truthy(&Value::Null));
        assert!(!frame_truthy(&json!(false)));
        assert!(frame_truthy(&json!(true)));
    }

    // ---- config helpers -------------------------------------------------
    #[test]
    fn config_pages_default_and_explicit() {
        assert_eq!(config_pages(&json!({})), vec!["/".to_string()]);
        assert_eq!(config_pages(&json!({"pages": []})), vec!["/".to_string()]);
        assert_eq!(
            config_pages(&json!({"pages": ["/", "/about"]})),
            vec!["/".to_string(), "/about".to_string()]
        );
    }

    #[test]
    fn config_launch_truthiness() {
        assert_eq!(config_launch(&json!({})), "");
        assert_eq!(config_launch(&json!({"launch": ""})), "");
        assert_eq!(config_launch(&json!({"launch": "python app.py"})), "python app.py");
        assert!(!config_launch(&json!({"launch": ["python", "app.py"]})).is_empty());
    }

    // ---- save_report drops heavy keys -----------------------------------
    #[test]
    fn save_report_drops_capture_and_agent_raw() {
        let dir = std::env::temp_dir().join(format!("solomon_vr_test_{}", short_hex(8)));
        std::fs::create_dir_all(&dir).unwrap();
        let report = json!({
            "ts": "2026-01-01T00:00:00Z", "ok": true, "findings": [],
            "summary": "s", "screenshots": [], "feedback": "",
            "capture": {"pages": 3}, "agent_raw": "lots of text"
        });
        save_report(&report, &dir);
        let body = std::fs::read_to_string(dir.join("report.json")).unwrap();
        let back: Value = serde_json::from_str(&body).unwrap();
        assert!(back.get("capture").is_none());
        assert!(back.get("agent_raw").is_none());
        assert_eq!(back["ok"], json!(true));
        assert_eq!(back["summary"], json!("s"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- shlex split ----------------------------------------------------
    #[test]
    fn shlex_split_quotes() {
        assert_eq!(shlex_split("python app.py"), vec!["python", "app.py"]);
        assert_eq!(
            shlex_split("python -m http.server \"{port}\""),
            vec!["python", "-m", "http.server", "{port}"]
        );
        assert_eq!(shlex_split("a 'b c' d"), vec!["a", "b c", "d"]);
    }

    // ---- char slicing (Python str[:n] semantics) ------------------------
    #[test]
    fn char_slice_respects_codepoints() {
        // em-dash is multibyte; slicing must not split it.
        let s = "ab—cd";
        assert_eq!(char_slice(s, 3), "ab—");
        assert_eq!(char_len(s), 5);
    }

    // ---- base64 ---------------------------------------------------------
    #[test]
    fn base64_roundtrip_and_known_vectors() {
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
        for v in [b"".as_ref(), b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar", &[0u8, 255, 1, 254]] {
            assert_eq!(b64_decode(&b64_encode(v)).unwrap(), v.to_vec());
        }
    }

    // ---- origin host ----------------------------------------------------
    #[test]
    fn origin_host_strips_port_and_scheme() {
        assert_eq!(origin_host("http://127.0.0.1:54321/path"), Some("127.0.0.1".to_string()));
        assert_eq!(origin_host("ftp://x"), None);
    }
}
