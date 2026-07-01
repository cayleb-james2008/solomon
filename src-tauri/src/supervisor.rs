//! Native Rust port of improver/solomon.py — the per-repo RSI supervisor / watchdog.
//!
//! Bug-for-bug with solomon.py. A deterministic watchdog is the default and the only thing that ever
//! runs unattended:
//!   diagnose(repo)  — file-only health classification (safe to call from get_state every poll).
//!   recover(repo)   — a safe ladder:
//!       RUNG 0  deterministic, reversible recovery (clear stale lock/stop, reset base to origin) — auto.
//!       RUNG 1  an OPT-IN, PR-gated Solomon fix-session for a persistent gate-red streak.
//!       RUNG 2  escalate to the operator (write escalation.json with copy-paste steps; do nothing
//!               destructive).
//! Every action is appended to runtime/<name>/supervisor.jsonl. Nothing here force-kills a process,
//! discards un-pushed commits, pushes, or merges.
//!
//! All heartbeat/lock/branch/registry/gh/runner operations delegate to the already-ported, green
//! Phase-1 `control::*` modules. Bridge-return dicts are `serde_json::Value` with keys byte-identical
//! to the Python dicts; status/category/reason strings are quoted verbatim from the source.
//!
//! DEVIATIONS:
//!  * solomon_fix_session spawns the SELF executable as `<current_exe> run-improver --solomon …`
//!    detached/hidden (the native runner subcommand), NOT a python `run_improver.py --solomon` child
//!    as the Python source did — the native binary IS the runner. The arg vector mirrors
//!    control::runner::start's construction.
//!  * _provider_has_fallback reads `improver::ctx::fallback_model` directly (the native port of
//!    run_improver._FALLBACK_MODEL) instead of lazy-importing the runner module — there is no import
//!    to fail, but the same observable result (provider has a fallback -> True; else False) holds.
//!  * read_escalation returns `Option<Value>` (None == Python `null`) to match the existing
//!    `crate::watchdog` caller and the rest of this crate's API surface.
#![allow(dead_code)]

use crate::control::{branches, heartbeat, locks, paths, proc, registry, runner};
use chrono::{NaiveDateTime, Utc};
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

/// solomon._AUTO_SAFE — the categories whose RUNG-0 recovery is deterministic + reversible.
/// (Documentation of intent: the per-category branches in recover() carry the actual routing, and
/// diagnose() stamps `auto_safe` per-category.)
const AUTO_SAFE: &[&str] = &["stale_lock", "stop_lingering", "dirty_tree", "stuck"];

/// solomon._now: `datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")`.
fn now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// solomon._rt: control._runtime_dir(repo).
fn rt(repo: &Value) -> Option<std::path::PathBuf> {
    paths::runtime_dir(repo)
}

/// solomon._append_jsonl: append one JSON record line to runtime/<name>/<fname>. No-op when the repo
/// has no runtime dir; an OSError (makedirs/open/write) is swallowed (`except OSError: pass`).
fn append_jsonl(repo: &Value, fname: &str, rec: &Value) {
    let dir = match rt(repo) {
        Some(d) => d,
        None => return,
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(fname))
    {
        use std::io::Write;
        // json.dumps(rec) + "\n" — serde's default is compact, like json.dumps' default separators.
        let line = format!("{}\n", serde_json::to_string(rec).unwrap_or_default());
        let _ = f.write_all(line.as_bytes());
    }
}

/// solomon._stale: True when the heartbeat's `updated_at` is older than
/// `max(3 * project_interval(repo), LOCK_LIVE_FLOOR_S)` seconds. A falsy / unparseable timestamp
/// reads as NOT stale (False), matching `if not ts: return False` and the
/// `except (ValueError, TypeError): return False` guards.
fn stale(hb: &Value, repo: &Value) -> bool {
    // ts = hb.get("updated_at"); if not ts: return False.
    let ts = match hb.get("updated_at") {
        Some(Value::String(s)) if !s.is_empty() => s.as_str(),
        // missing key, JSON null, empty string -> falsy -> False; a non-string -> strptime TypeError -> False.
        _ => return false,
    };
    // datetime.strptime(ts, "%Y-%m-%dT%H:%M:%SZ") — strict (no fractional seconds / offset).
    let last = match NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%SZ") {
        Ok(dt) => dt.and_utc(),
        Err(_) => return false, // ValueError -> False
    };
    let age = (Utc::now() - last).num_milliseconds() as f64 / 1000.0;
    let threshold = (3.0 * registry::project_interval(repo) as f64).max(paths::LOCK_LIVE_FLOOR_S);
    age > threshold
}

/// solomon._provider_has_fallback: True if the runner has a configured FALLBACK model for this repo's
/// provider (used by the noop_streak auto-heal). See the module DEVIATION note.
fn provider_has_fallback(repo: &Value) -> bool {
    crate::improver::ctx::fallback_model(&registry::project_provider(repo)).is_some()
}

/// solomon._push_base_if_ahead: fast-forward PUBLISH a base merely AHEAD of origin (never --force,
/// never reset). Returns {pushed, ahead?, diverged, error?}. Diverged (also behind) or any git
/// failure -> pushed=False so recover() escalates.
fn push_base_if_ahead(repo: &Value) -> Value {
    let git = proc::which_git();
    let path = paths::repo_path(repo);
    let base = registry::project_pr_target_branch(repo);
    let git = match git {
        Some(g) if !path.is_empty() => g,
        _ => return json!({"pushed": false, "diverged": false, "error": "git/path unavailable"}),
    };
    let git_s = git.to_string_lossy().into_owned();
    let g = |args: &[&str]| -> std::io::Result<proc::RunOut> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 3);
        full.push(git_s.as_str());
        full.push("-C");
        full.push(&path);
        full.extend_from_slice(args);
        proc::run(&full, None, None)
    };

    // Whole try-block: any OSError/ValueError -> {pushed:false, diverged:false, error:str(e)}.
    let result = (|| -> std::io::Result<Value> {
        if g(&["remote", "get-url", "origin"])?.code != 0 {
            return Ok(json!({"pushed": false, "diverged": false, "error": "no origin remote"}));
        }
        let _ = g(&["fetch", "origin", "--quiet"])?; // current truth before counting
        let range = format!("origin/{base}...{base}");
        let lr = g(&["rev-list", "--left-right", "--count", &range])?;
        let (mut behind, mut ahead) = (0i64, 0i64);
        if lr.code == 0 {
            let parts: Vec<&str> = lr.stdout.split_whitespace().collect();
            if parts.len() == 2 {
                // int(parts[0]), int(parts[1]) — a parse failure is the Python ValueError branch
                // (caught by the outer except -> error:str(e)). Mirror it with a synthetic error.
                match (parts[0].parse::<i64>(), parts[1].parse::<i64>()) {
                    (Ok(b), Ok(a)) => {
                        behind = b;
                        ahead = a;
                    }
                    _ => {
                        return Ok(json!({
                            "pushed": false, "diverged": false,
                            "error": format!("invalid literal for int() with base 10: {:?}", lr.stdout.trim())
                        }));
                    }
                }
            }
        }
        if ahead <= 0 {
            return Ok(json!({"pushed": false, "ahead": ahead, "diverged": false}));
        }
        if behind > 0 {
            return Ok(json!({"pushed": false, "ahead": ahead, "diverged": true})); // not a fast-forward
        }
        let refspec = format!("{base}:{base}");
        let pu = g(&["push", "origin", &refspec])?; // FF-only; never --force
        if pu.code != 0 {
            let msg = if !pu.stderr.trim().is_empty() {
                pu.stderr.trim()
            } else if !pu.stdout.trim().is_empty() {
                pu.stdout.trim()
            } else {
                "push failed"
            };
            let msg: String = msg.chars().take(200).collect();
            return Ok(json!({"pushed": false, "ahead": ahead, "diverged": false, "error": msg}));
        }
        Ok(json!({"pushed": true, "ahead": ahead, "diverged": false}))
    })();

    match result {
        Ok(v) => v,
        Err(e) => json!({"pushed": false, "diverged": false, "error": e.to_string()}),
    }
}

/// solomon.diagnose: deterministic, file-only health classification (no git shell-out — cheap on
/// every poll). Returns {name, healthy, category, evidence, recommended:[...], auto_safe, running}.
///
/// The 14-category cascade ORDER is load-bearing:
///   ok / needs_goal / no_key / gh_not_ready / revert_failed / dirty_tree / base_out_of_band /
///   untracked_refusal / stale_lock / stop_lingering / stuck / gate_red_streak / ci_red_streak /
///   noop_streak / unknown_error.
pub fn diagnose(repo: &Value) -> Value {
    let name = paths::repo_name(repo);
    let hb = heartbeat::read_heartbeat(repo).unwrap_or_else(|| json!({}));
    let running = locks::is_running(repo);
    let rtd = rt(repo);
    let has_lock = rtd.as_ref().map(|d| d.join("lock").exists()).unwrap_or(false);
    let has_stop = rtd.as_ref().map(|d| d.join("stop").exists()).unwrap_or(false);

    let status = hb.get("status").and_then(Value::as_str);
    let phase = hb.get("phase").and_then(Value::as_str);
    let reason = hb.get("reason").and_then(Value::as_str);
    // summary = hb.get("last_summary") or ""  — a falsy (null/missing/"") value -> "".
    let summary = match hb.get("last_summary") {
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let hist = heartbeat::read_history(repo, 20);

    let summary_lower = summary.to_lowercase();
    // The first-200/first-160 char slices are by CHAR (Python slices by code point).
    let trunc = |s: &str, n: usize| -> String { s.chars().take(n).collect() };
    // `summary[:N] or <fallback>`: the truncated summary if non-empty else the fallback.
    let trunc_or = |s: &str, n: usize, fallback: &str| -> String {
        let t = trunc(s, n);
        if t.is_empty() { fallback.to_string() } else { t }
    };

    // cat, ev, rec, safe = "ok", (status or ("running" if running else "idle")), [], True
    let mut cat = "ok".to_string();
    let mut ev: String = match status {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => if running { "running".to_string() } else { "idle".to_string() },
    };
    let mut rec: Vec<String> = Vec::new();
    let mut safe = true;

    if status == Some("error") && reason == Some("needs_goal") {
        cat = "needs_goal".into();
        ev = trunc_or(summary, 200, "no north-star GOAL and no actionable backlog");
        rec = vec!["set this repo's GOAL in Config so the loop has an objective".into()];
        safe = false;
    } else if status == Some("error") && (summary.contains("not set") || summary.contains("API key")) {
        cat = "no_key".into();
        ev = trunc(summary, 160);
        rec = vec!["add the provider API key in Settings".into()];
        safe = false;
    } else if status == Some("error") && summary.contains("GitHub not ready") {
        cat = "gh_not_ready".into();
        ev = trunc(summary, 160);
        rec = vec!["connect GitHub (gh auth login)".into()];
        safe = false;
    } else if status == Some("error") && phase == Some("reverted") {
        cat = "revert_failed".into();
        ev = trunc_or(summary, 200, "revert failed");
        rec = vec![
            "attempt a reset of the base branch to origin".into(),
            "else manual cleanup".into(),
        ];
        safe = false;
    } else if status == Some("error")
        && phase == Some("preflight")
        && summary_lower.contains("dirty")
        && reason != Some("dirty_base_persistent")
    {
        cat = "dirty_tree".into();
        ev = trunc(summary, 160);
        rec = vec!["reset the base branch to origin".into()];
        safe = true;
    } else if status == Some("error")
        && phase == Some("preflight")
        && (summary_lower.contains("out-of-band") || summary_lower.contains("refusing to hard-reset"))
    {
        cat = "base_out_of_band".into();
        ev = trunc(summary, 200);
        rec = vec![
            "base has un-pushed / out-of-band commits — push or revert them \
             (the loop changes a repo only via gated PRs)"
                .into(),
        ];
        safe = false;
    } else if status == Some("error")
        && phase == Some("preflight")
        && summary_lower.contains("would be deleted by the preflight clean")
    {
        cat = "untracked_refusal".into();
        ev = trunc(summary, 200);
        rec = vec![
            "untracked files on the base block the preflight clean — review them, \
             then commit or remove them (the loop won't delete possible operator work)"
                .into(),
        ];
        safe = false;
    } else if has_lock && !running {
        cat = "stale_lock".into();
        ev = "lock file present but no live improver PID".into();
        rec = vec!["clear the stale lock".into()];
        safe = true;
    } else if has_stop && !running {
        let clean_exit = status == Some("stopped");
        cat = "stop_lingering".into();
        ev = if clean_exit {
            "stop sentinel present after a clean exit".into()
        } else {
            "stop sentinel present but the loop did not exit cleanly (crash/kill mid-stop) — \
             honoring the operator Stop, not auto-restarting"
                .into()
        };
        rec = if clean_exit {
            vec!["clear the vestigial stop sentinel".into()]
        } else {
            vec![
                "the loop was stopped but did not exit cleanly — press Start to resume, or investigate \
                 the crash; the watchdog will not auto-restart it"
                    .into(),
            ]
        };
        safe = clean_exit;
    } else if running && !matches!(phase, None | Some("sleep")) && stale(&hb, repo) {
        cat = "stuck".into();
        ev = format!(
            "no heartbeat update for a long time while in phase '{}'",
            phase.unwrap_or("")
        );
        rec = vec!["stop and restart the loop".into()];
        safe = true;
    } else if hist.len() >= 3
        && hist[hist.len() - 3..]
            .iter()
            .all(|r| matches!(r.get("status").and_then(Value::as_str), Some("reverted") | Some("error")))
    {
        cat = "gate_red_streak".into();
        ev = "last 3 iterations reverted/errored — the gate keeps failing".into();
        rec = vec!["run a Solomon fix-session (opt-in)".into()];
        safe = false;
    } else if registry::project_ship(repo) == "auto-merge"
        && hist.len() >= 3
        && hist[hist.len() - 3..]
            .iter()
            .all(|r| r.get("status").and_then(Value::as_str) == Some("blocked"))
    {
        cat = "ci_red_streak".into();
        ev = "last 3 auto-merge PRs did not land (CI-red / unmerged) — CI keeps failing".into();
        rec = vec!["review the failing CI on the open rsi/* PRs and fix the cause".into()];
        safe = false;
    } else if hist.len() >= 5
        && hist[hist.len() - 5..]
            .iter()
            .all(|r| r.get("status").and_then(Value::as_str) == Some("noop"))
    {
        cat = "noop_streak".into();
        ev = "5 iterations in a row made no change — the backlog looks exhausted or \
              too hard for the current model"
            .into();
        rec = vec![
            "refill the backlog (run Ideate) or simplify/replace the deferred items, \
             or raise the repo's model"
                .into(),
        ];
        safe = false;
    } else if status == Some("error") {
        cat = "unknown_error".into();
        ev = trunc_or(summary, 200, "unclassified loop error");
        rec = vec![
            "review the loop's last error (dashboard / runtime log) and address the cause".into(),
        ];
        safe = false;
    }

    // anti-thrash: same auto category fixed >= 3 times recently -> escalate instead of looping forever.
    if safe && cat != "ok" {
        let same = supervisor_log_count_same(repo, &cat);
        if same >= 3 {
            ev.push_str(" (auto-fixed repeatedly — escalating instead of looping)");
            safe = false;
        }
    }

    json!({
        "name": name,
        "healthy": cat == "ok",
        "category": cat,
        "evidence": ev,
        "recommended": rec,
        "auto_safe": safe,
        "running": running,
    })
}

/// Anti-thrash helper: count recent supervisor.jsonl records (last 4) that are a RUNG-0, non-escalate
/// auto-fix of `cat`. Mirrors the comprehension in diagnose().
fn supervisor_log_count_same(repo: &Value, cat: &str) -> usize {
    heartbeat::read_supervisor_log(repo, 4)
        .into_iter()
        .filter(|s| {
            s.get("category").and_then(Value::as_str) == Some(cat)
                && s.get("rung").and_then(Value::as_i64) == Some(0)
                && !s.get("escalate").and_then(Value::as_bool).unwrap_or(false)
        })
        .count()
}

/// solomon._suggested_steps: copy-paste manual recovery steps per category (for escalation.json).
fn suggested_steps(repo: &Value, cat: &str) -> Vec<String> {
    let path = {
        let p = paths::repo_path(repo);
        if p.is_empty() { "<repo>".to_string() } else { p }
    };
    let base = registry::project_pr_target_branch(repo);
    let cd = format!("cd \"{path}\"");
    match cat {
        "revert_failed" => vec![
            cd,
            format!("git checkout --force {base}"),
            "git reset --hard".into(),
            format!("git reset --hard origin/{base}"),
            "git status".into(),
        ],
        "no_key" => vec!["Open Solomon → Settings and add the provider's API key, then retry".into()],
        "needs_goal" => vec![
            "Open Solomon → this repo → Config and set a north-star GOAL (or add an actionable".into(),
            "backlog item in improver/<name>/backlog.md); the loop resumes once it has an objective".into(),
        ],
        "gh_not_ready" => vec!["gh auth login   # authenticate, then retry".into()],
        "stuck" => vec![
            cd,
            "# find the hung improver PID then stop it manually (Solomon will not force-kill):".into(),
            "taskkill /F /T /PID <pid>   # Windows".into(),
            "# or:  kill <pid>   # Unix".into(),
        ],
        "base_out_of_band" => vec![
            cd,
            format!("git log origin/{base}..{base} --oneline   # the un-pushed / out-of-band commits"),
            format!("git push origin {base}                  # if they're wanted, OR (destructive):"),
            format!("git reset --hard origin/{base}           # discard them — the loop ships only via gated PRs"),
        ],
        "ci_red_streak" => vec![
            cd,
            "gh pr list --state open            # the CI-red rsi/* PRs that won't merge".into(),
            "gh pr checks <number>                  # which check failed".into(),
            "# fix the failing-CI cause (or close the bad PRs); tick 'Allow AI fix' for a fix-session".into(),
        ],
        "untracked_refusal" => vec![
            cd,
            "git status --porcelain                          # the untracked files blocking preflight".into(),
            "git stash push --include-untracked -m solomon       # if they're disposable, OR".into(),
            "git add -A && git commit -m \"operator work\"         # if they are real work to keep".into(),
        ],
        "noop_streak" => vec![
            "Open Solomon → this repo → Ideate to refill the backlog with fresh items,".into(),
            "or edit improver/<name>/backlog.md to add/simplify items,".into(),
            "or raise the repo's model in Config (the current one keeps failing to implement)".into(),
        ],
        _ => vec![cd, "git status".into()],
    }
}

/// solomon._write_escalation: overwrite runtime/<name>/escalation.json with the current diagnosis +
/// suggested manual steps (category-deduped: a single overwrite). No-op without a runtime dir; an
/// OSError is swallowed. `d` must carry `category` + `evidence`.
pub fn write_escalation(repo: &Value, d: &Value) {
    let dir = match rt(repo) {
        Some(d) => d,
        None => return,
    };
    let category = d.get("category").and_then(Value::as_str).unwrap_or("");
    let rec = json!({
        "ts": now(),
        "category": d.get("category").cloned().unwrap_or(Value::Null),
        "evidence": d.get("evidence").cloned().unwrap_or(Value::Null),
        "suggested_manual_steps": suggested_steps(repo, category),
    });
    let _ = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        // json.dump(rec, f, indent=2).
        let body = serde_json::to_vec_pretty(&rec).map_err(std::io::Error::other)?;
        std::fs::write(dir.join("escalation.json"), body)
    })();
}

/// solomon.read_escalation: the parsed runtime/<name>/escalation.json, or None
/// (== Python `null`) on missing/corrupt file / no runtime dir.
pub fn read_escalation(repo: &Value) -> Option<Value> {
    let dir = rt(repo)?;
    let data = std::fs::read_to_string(dir.join("escalation.json")).ok()?;
    serde_json::from_str(&data).ok()
}

/// solomon.clear_escalation: remove runtime/<name>/escalation.json. {ok:true} (a missing file is
/// swallowed); {ok:false, error} only when the repo has no runtime dir.
pub fn clear_escalation(repo: &Value) -> Value {
    let dir = match rt(repo) {
        Some(d) => d,
        None => return json!({"ok": false, "error": "repo has no 'path'"}),
    };
    let _ = std::fs::remove_file(dir.join("escalation.json")); // OSError swallowed
    json!({"ok": true})
}

/// solomon._finish: append the supervisor.jsonl record, write escalation.json on escalation, and
/// return the recover() result dict. Implements the LOG-ONCE dedupe: a pure escalation (no actions)
/// whose category matches the last supervisor record is reported escalate=False and not re-logged.
fn finish(repo: &Value, d: &Value, actions: Vec<String>, escalate: bool, msg: &str) -> Value {
    let category = d.get("category").and_then(Value::as_str).unwrap_or("");
    // rung = 1 if gate_red_streak else (2 if escalate and not actions else 0)
    let rung: i64 = if category == "gate_red_streak" {
        1
    } else if escalate && actions.is_empty() {
        2
    } else {
        0
    };

    if escalate && actions.is_empty() {
        let prior = heartbeat::read_supervisor_log(repo, 1);
        if let Some(last) = prior.last() {
            if last.get("escalate").and_then(Value::as_bool).unwrap_or(false)
                && last.get("category").and_then(Value::as_str) == Some(category)
            {
                return json!({
                    "ok": false,
                    "category": category,
                    "actions_taken": actions,
                    "escalate": false,
                    "message": msg,
                    "escalate_deduped": true,
                });
            }
        }
    }

    append_jsonl(
        repo,
        "supervisor.jsonl",
        &json!({
            "ts": now(),
            "category": category,
            "rung": rung,
            "actions": actions.clone(),
            "escalate": escalate,
            "message": msg,
        }),
    );
    if escalate {
        write_escalation(repo, d);
    }
    json!({
        "ok": !escalate,
        "category": category,
        "actions_taken": actions,
        "escalate": escalate,
        "message": msg,
    })
}

/// solomon.solomon_fix_session: spawn a one-shot, detached Solomon fix-session. It runs through the
/// normal branch + gate + PR path, so a Solomon fix is itself a reviewable PR. Honors the global
/// auto_push gate (effective_ship): when auto_push is off the fix-session ships LOCAL only.
///
/// DEVIATION: spawns the SELF executable `<current_exe> run-improver --solomon …` (see module note).
pub fn solomon_fix_session(repo: &Value, auto_push: bool) -> Value {
    let path = paths::repo_path(repo);
    let name = paths::repo_name(repo);
    if path.is_empty() || name.is_empty() {
        return json!({"ok": false, "error": "repo has no name/path"});
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return json!({"ok": false, "error": e.to_string()}),
    };
    let exe_s = exe.to_string_lossy().into_owned();
    let argv: Vec<String> = vec![
        "run-improver".into(),
        "--repo".into(),
        path.clone(),
        "--name".into(),
        name,
        "--provider".into(),
        registry::project_provider(repo),
        "--model".into(),
        registry::project_model(repo),
        "--ship".into(),
        registry::effective_ship(repo, auto_push),
        "--pr-target-branch".into(),
        registry::project_pr_target_branch(repo),
        "--reasoning".into(),
        registry::project_reasoning(repo),
        "--solomon".into(),
    ];

    let mut cmd = Command::new(&exe_s);
    cmd.args(&argv);
    cmd.current_dir(&path);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Match the other spawn sites: strip the stale gh token + PYTHONPATH/PYTHONHOME at the boundary.
    proc::apply_clean_env(&mut cmd);
    #[cfg(windows)]
    cmd.creation_flags(proc::hidden_flags(true, false));
    match cmd.spawn() {
        Ok(child) => {
            proc::bind_to_app_job(&child); // die with the GUI app (no-op in headless subcommands)
            json!({"ok": true, "pid": child.id()})
        }
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// solomon.recover: walk the recovery ladder for one repo. Returns
/// {ok, category, actions_taken:[...], escalate:bool, message}. auto_push threads the global gate so a
/// restart / fix-session ships LOCAL-only when pushing is disabled.
///
/// RUNG-0 auto-safe: stale_lock->clear_lock, stop_lingering->clear_stop, dirty_tree->reset_to_base,
/// stuck->stop+restart, revert_failed->reset+cleanup+restart.
/// RUNG-0.5 conditional: base_out_of_band->push if FF-only, noop_streak->ideate+restart if fallback.
/// RUNG-1 opt-in: gate_red_streak->solomon_fix_session if allow_pi.
/// RUNG-2: escalate.
/// ANTI-THRASH: same category fixed >= 3 times (read_supervisor_log) -> escalate instead of loop.
pub fn recover(repo: &Value, allow_pi: bool, allow_restart: bool, auto_push: bool) -> Value {
    let d = diagnose(repo);
    let cat = d.get("category").and_then(Value::as_str).unwrap_or("").to_string();
    if cat == "ok" {
        // healthy — clear any STALE escalation.json a prior transient error left behind.
        clear_escalation(repo);
        // Stamp a healthy supervisor.jsonl record ONLY when transitioning from a non-healthy state.
        // Without this, the finish() log-once dedupe (which compares against the LAST supervisor
        // record) would still see the stale pre-fix escalation on a recurrence and suppress
        // re-escalation — the operator would never be re-notified that the same problem came back.
        // An "ok" record carries escalate=false, so the dedupe guard (which requires
        // escalate==true on the prior record) no longer fires. Only written when the last record is
        // not already "ok" to avoid flooding the log on every healthy poll.
        let last_was_ok = heartbeat::read_supervisor_log(repo, 1)
            .last()
            .map(|l| l.get("category").and_then(Value::as_str) == Some("ok"))
            .unwrap_or(false);
        if !last_was_ok {
            append_jsonl(
                repo,
                "supervisor.jsonl",
                &json!({
                    "ts": now(),
                    "category": "ok",
                    "rung": 0,
                    "actions": [],
                    "escalate": false,
                    "message": "healthy",
                }),
            );
        }
        return json!({"ok": true, "category": "ok", "actions_taken": [], "escalate": false, "message": "healthy"});
    }

    let mut actions: Vec<String> = Vec::new();

    // RUNG 2 / revert_failed special path.
    if cat == "revert_failed" {
        if locks::is_running(repo) {
            return finish(repo, &d, vec![], true,
                "loop is live — stop it before Solomon resets the un-reverted base");
        }
        // anti-thrash: cap recurring auto-resets.
        let prior_resets = heartbeat::read_supervisor_log(repo, 6)
            .into_iter()
            .filter(|s| {
                s.get("category").and_then(Value::as_str) == Some("revert_failed")
                    && s.get("actions")
                        .and_then(Value::as_array)
                        .map(|a| a.iter().any(|x| x.as_str() == Some("reset_to_base")))
                        .unwrap_or(false)
            })
            .count();
        if prior_resets >= 3 {
            return finish(repo, &d, vec![], true,
                "revert-failure recurs after repeated auto-resets — escalating \
                 (a deterministic cause keeps re-wedging the base)");
        }
        let (ok, token) = locks::acquire_supervisor_lock(repo);
        if !ok {
            return finish(repo, &d, vec![], true,
                "loop lock could not be acquired — stop it before Solomon resets the \
                 un-reverted base");
        }
        let r = branches::reset_to_base(repo);
        if let Some(tok) = token {
            locks::release_supervisor_lock(repo, &tok);
        }
        actions.push("reset_to_base".into());
        if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let m = r.get("error").and_then(Value::as_str).unwrap_or("reset_to_base failed — escalate").to_string();
            return finish(repo, &d, actions, true, &m);
        }
        // the reset succeeded: clean up the lingering rsi/* branches.
        let cw = branches::cleanup_worktrees(repo);
        actions.push("cleanup_worktrees".into());
        let mut msg = if !cw.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let err = cw.get("error").and_then(Value::as_str).unwrap_or("?");
            format!("recovered: reset_to_base (cleanup_worktrees: {err})")
        } else {
            let removed = cw.get("removed").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            format!("recovered: reset_to_base, cleanup_worktrees (removed {removed} rsi/* branch(es))")
        };
        if allow_restart {
            runner::start(repo, auto_push, false);
            actions.push("restart".into());
            msg.push_str(", restart");
        }
        return finish(repo, &d, actions, false, &msg);
    }

    // RUNG-0.5 base_out_of_band auto-heal.
    if cat == "base_out_of_band" {
        if auto_push {
            let pr = push_base_if_ahead(repo);
            if pr.get("pushed").and_then(Value::as_bool).unwrap_or(false) {
                actions.push("push_base".into());
                let ahead_s = py_repr_scalar(pr.get("ahead"));
                let m = format!("recovered: pushed {ahead_s} ahead base commit(s) to origin");
                return finish(repo, &d, actions, false, &m);
            }
            if pr.get("diverged").and_then(Value::as_bool).unwrap_or(false) {
                return finish(repo, &d, vec![], true,
                    "base diverged from origin (not a fast-forward) — operator must \
                     reconcile (push/rebase or revert)");
            }
        }
        return finish(repo, &d, vec![], true, "escalated — operator action required");
    }

    // RUNG-0.5 noop_streak auto-heal.
    if cat == "noop_streak" {
        if provider_has_fallback(repo) {
            let prior_heals = heartbeat::read_supervisor_log(repo, 8)
                .into_iter()
                .filter(|s| {
                    s.get("category").and_then(Value::as_str) == Some("noop_streak")
                        && s.get("actions")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().any(|x| x.as_str() == Some("ideate")))
                            .unwrap_or(false)
                })
                .count();
            if prior_heals < 3 {
                runner::stop(repo);
                actions.push("stop".into());
                for _ in 0..10 {
                    // grace window for the loop to exit cleanly
                    if !locks::is_running(repo) {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                if locks::is_running(repo) {
                    return finish(repo, &d, actions, true,
                        "loop would not stop for backlog refill — manual kill required");
                }
                let ir = runner::ideate(repo);
                actions.push("ideate".into());
                if allow_restart {
                    runner::start(repo, auto_push, false);
                    actions.push("restart".into());
                }
                if !ir.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                    let err = ir.get("error").and_then(Value::as_str).unwrap_or("?");
                    let m = format!("backlog refill failed ({err}) — escalating");
                    return finish(repo, &d, actions, true, &m);
                }
                let added_s = py_repr_scalar(ir.get("added"));
                let m = format!("recovered: refilled backlog (ideate added {added_s})");
                return finish(repo, &d, actions, false, &m);
            }
        }
        return finish(repo, &d, vec![], true, "escalated — operator action required");
    }

    let auto_safe = d.get("auto_safe").and_then(Value::as_bool).unwrap_or(false);
    if !auto_safe && cat != "gate_red_streak" {
        return finish(repo, &d, vec![], true, "escalated — operator action required");
    }

    if cat == "stale_lock" {
        let r = locks::clear_lock(repo);
        actions.push("clear_lock".into());
        if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let m = r.get("error").and_then(Value::as_str).unwrap_or("").to_string();
            return finish(repo, &d, actions, true, &m);
        }
    } else if cat == "stop_lingering" {
        if let Some(dir) = rt(repo) {
            let _ = std::fs::remove_file(dir.join("stop")); // OSError swallowed
        }
        actions.push("clear_stop".into());
    } else if cat == "dirty_tree" {
        let (ok, token) = locks::acquire_supervisor_lock(repo);
        if !ok {
            return finish(repo, &d, actions, true,
                "loop is live — stop it before Solomon resets the working tree");
        }
        let r = branches::reset_to_base(repo);
        if let Some(tok) = token {
            locks::release_supervisor_lock(repo, &tok);
        }
        actions.push("reset_to_base".into());
        if !r.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let m = r.get("error").and_then(Value::as_str).unwrap_or("").to_string();
            return finish(repo, &d, actions, true, &m);
        }
    } else if cat == "stuck" {
        runner::stop(repo);
        actions.push("stop".into());
        for _ in 0..10 {
            // grace window for the loop to exit cleanly
            if !locks::is_running(repo) {
                break;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        if locks::is_running(repo) {
            return finish(repo, &d, actions, true,
                "loop would not stop — manual kill required (Solomon will not force-kill)");
        }
        // Restart UNCONDITIONALLY (not gated by allow_restart) — see source comment.
        runner::start(repo, auto_push, false);
        actions.push("restart".into());
    } else if cat == "gate_red_streak" {
        let provider_keyed = keys_provider_ready(repo);
        if !(allow_pi && provider_keyed) {
            return finish(repo, &d, actions, true,
                "persistent gate failure — tick 'Allow AI fix' to run a Solomon fix-session");
        }
        if locks::is_running(repo) {
            return finish(repo, &d, actions, true,
                "loop is live — stop it before running a Solomon fix-session");
        }
        let r = solomon_fix_session(repo, auto_push);
        actions.push("solomon_fix_session".into());
        let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let msg = if ok {
            "launched Solomon fix-session".to_string()
        } else {
            r.get("error").and_then(Value::as_str).unwrap_or("").to_string()
        };
        return finish(repo, &d, actions, !ok, &msg);
    }

    let msg = format!("recovered: {}", actions.join(", "));
    finish(repo, &d, actions, false, &msg)
}

/// Render a JSON scalar the way Python's `str.format` interpolates `d.get('k', '?')`: an integer like
/// `3`, a string verbatim, and a MISSING / null value as the literal `?` fallback used in the source
/// f-strings (`pr.get('ahead')` / `ir.get('added', '?')`). A present `ahead` is always an int.
fn py_repr_scalar(v: Option<&Value>) -> String {
    match v {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => if *b { "True".into() } else { "False".into() },
        // None/null -> the f-string fallback. For `pr.get('ahead')` (no default) Python would print
        // "None"; for `ir.get('added', '?')` it prints "?". The push-base path always has an int
        // `ahead`, so this fallback is only reachable for `added`, whose source default is '?'.
        _ => "?".to_string(),
    }
}

/// `control.keys_status().get(control.project_provider(repo))` truthiness — the gate_red_streak
/// fix-session precondition.
fn keys_provider_ready(repo: &Value) -> bool {
    crate::control::keys::keys_status()
        .get(&registry::project_provider(repo))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::Path;

    // A unique runtime-backed repo for FS-touching diagnose/recover tests.
    fn tmp_repo(tag: &str) -> (std::path::PathBuf, Value) {
        let name = format!("sup_test_{tag}_{}", std::process::id());
        let repo = json!({ "name": name });
        let dir = paths::runtime_dir(&repo).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        (dir, repo)
    }

    fn write_hb(dir: &Path, hb: &Value) {
        std::fs::write(dir.join("heartbeat.json"), serde_json::to_string(hb).unwrap()).unwrap();
    }

    fn write_hist(dir: &Path, lines: &[Value]) {
        let body: String = lines
            .iter()
            .map(|l| serde_json::to_string(l).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("history.jsonl"), body).unwrap();
    }

    // ---------------- _stale ----------------
    #[test]
    fn stale_falsy_timestamp_is_false() {
        let repo = json!({"name": "x"});
        assert!(!stale(&json!({}), &repo)); // missing
        assert!(!stale(&json!({"updated_at": ""}), &repo)); // empty
        assert!(!stale(&json!({"updated_at": null}), &repo)); // null
        assert!(!stale(&json!({"updated_at": 12345}), &repo)); // non-string -> TypeError -> False
        assert!(!stale(&json!({"updated_at": "garbage"}), &repo)); // unparseable -> False
    }

    #[test]
    fn stale_fresh_vs_old() {
        let repo = json!({"name": "x"}); // default interval 120 -> threshold max(360, 4500)=4500
        let fresh = (Utc::now() - chrono::Duration::seconds(10))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        assert!(!stale(&json!({ "updated_at": fresh }), &repo));
        let old = (Utc::now() - chrono::Duration::seconds(5000))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        assert!(stale(&json!({ "updated_at": old }), &repo));
        // a large interval raises the threshold above 5000 -> NOT stale.
        let repo_big = json!({"name": "x", "interval": 2000}); // 3*2000=6000 > 5000
        assert!(!stale(&json!({ "updated_at": old }), &repo_big));
    }

    // ---------------- diagnose: category cascade ----------------
    #[test]
    fn diagnose_ok_when_idle() {
        let (dir, repo) = tmp_repo("ok");
        let d = diagnose(&repo);
        assert_eq!(d["category"], "ok");
        assert_eq!(d["healthy"], true);
        assert_eq!(d["evidence"], "idle");
        assert_eq!(d["auto_safe"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_needs_goal_precedence() {
        let (dir, repo) = tmp_repo("needsgoal");
        write_hb(&dir, &json!({"status": "error", "reason": "needs_goal", "last_summary": ""}));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "needs_goal");
        assert_eq!(d["evidence"], "no north-star GOAL and no actionable backlog");
        assert_eq!(d["auto_safe"], false);
        assert_eq!(
            d["recommended"],
            json!(["set this repo's GOAL in Config so the loop has an objective"])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_no_key() {
        let (dir, repo) = tmp_repo("nokey");
        write_hb(&dir, &json!({"status": "error", "last_summary": "OPENROUTER_API_KEY not set"}));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "no_key");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_gh_not_ready() {
        let (dir, repo) = tmp_repo("ghnr");
        write_hb(&dir, &json!({"status": "error", "last_summary": "GitHub not ready (gh auth)"}));
        assert_eq!(diagnose(&repo)["category"], "gh_not_ready");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_revert_failed() {
        let (dir, repo) = tmp_repo("revert");
        write_hb(&dir, &json!({"status": "error", "phase": "reverted", "last_summary": "boom"}));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "revert_failed");
        assert_eq!(d["evidence"], "boom");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_dirty_tree_vs_persistent() {
        let (dir, repo) = tmp_repo("dirty");
        write_hb(&dir, &json!({"status": "error", "phase": "preflight", "last_summary": "base tree is DIRTY"}));
        assert_eq!(diagnose(&repo)["category"], "dirty_tree");
        assert_eq!(diagnose(&repo)["auto_safe"], true);
        // dirty_base_persistent reason -> NOT dirty_tree; falls through to unknown_error (status=error).
        write_hb(&dir, &json!({"status": "error", "phase": "preflight",
                               "reason": "dirty_base_persistent", "last_summary": "base dirty"}));
        assert_eq!(diagnose(&repo)["category"], "unknown_error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_base_out_of_band() {
        let (dir, repo) = tmp_repo("oob");
        write_hb(&dir, &json!({"status": "error", "phase": "preflight",
                               "last_summary": "refusing to hard-reset a base ahead of origin"}));
        assert_eq!(diagnose(&repo)["category"], "base_out_of_band");
        write_hb(&dir, &json!({"status": "error", "phase": "preflight",
                               "last_summary": "base has out-of-band commits"}));
        assert_eq!(diagnose(&repo)["category"], "base_out_of_band");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_untracked_refusal() {
        let (dir, repo) = tmp_repo("untracked");
        write_hb(&dir, &json!({"status": "error", "phase": "preflight",
                               "last_summary": "files would be deleted by the preflight clean"}));
        assert_eq!(diagnose(&repo)["category"], "untracked_refusal");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_stale_lock_and_stop_lingering() {
        let (dir, repo) = tmp_repo("stalelock");
        // a lock owned by a dead pid + no live runner -> stale_lock
        std::fs::write(dir.join("lock"), "2147483646\ntok").unwrap();
        let d = diagnose(&repo);
        assert_eq!(d["category"], "stale_lock");
        assert_eq!(d["auto_safe"], true);
        let _ = std::fs::remove_file(dir.join("lock"));

        // stop sentinel after a clean exit -> stop_lingering, auto-safe true
        write_hb(&dir, &json!({"status": "stopped"}));
        std::fs::write(dir.join("stop"), "").unwrap();
        let d = diagnose(&repo);
        assert_eq!(d["category"], "stop_lingering");
        assert_eq!(d["auto_safe"], true);
        assert_eq!(d["evidence"], "stop sentinel present after a clean exit");

        // stop sentinel but NOT a clean exit -> stop_lingering, auto-safe false
        write_hb(&dir, &json!({"status": "error"}));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "stop_lingering");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_gate_red_streak() {
        let (dir, repo) = tmp_repo("gatered");
        write_hist(&dir, &[
            json!({"status": "reverted"}),
            json!({"status": "error"}),
            json!({"status": "reverted"}),
        ]);
        let d = diagnose(&repo);
        assert_eq!(d["category"], "gate_red_streak");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_ci_red_streak_requires_auto_merge() {
        let (dir, repo_plain) = tmp_repo("cired");
        write_hist(&dir, &[
            json!({"status": "blocked"}),
            json!({"status": "blocked"}),
            json!({"status": "blocked"}),
        ]);
        // without ship=auto-merge: 3 blocked is not reverted/error and not 5 noop -> ok
        assert_eq!(diagnose(&repo_plain)["category"], "ok");
        // with ship=auto-merge -> ci_red_streak
        let repo_am = json!({"name": repo_plain["name"].clone(), "ship": "auto-merge"});
        assert_eq!(diagnose(&repo_am)["category"], "ci_red_streak");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_noop_streak_needs_five() {
        let (dir, repo) = tmp_repo("noop");
        let noops = |n: usize| -> Vec<Value> { (0..n).map(|_| json!({"status": "noop"})).collect() };
        write_hist(&dir, &noops(4));
        assert_eq!(diagnose(&repo)["category"], "ok"); // only 4 -> ok
        write_hist(&dir, &noops(5));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "noop_streak");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_unknown_error_catchall() {
        let (dir, repo) = tmp_repo("unknown");
        write_hb(&dir, &json!({"status": "error", "last_summary": "weird thing"}));
        let d = diagnose(&repo);
        assert_eq!(d["category"], "unknown_error");
        assert_eq!(d["evidence"], "weird thing");
        assert_eq!(d["auto_safe"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnose_anti_thrash_escalates_safe_category() {
        let (dir, repo) = tmp_repo("antithrash");
        // 3 prior RUNG-0 non-escalate stale_lock fixes -> the 4th diagnose flips auto_safe false.
        let sup: Vec<Value> = (0..3)
            .map(|_| json!({"category": "stale_lock", "rung": 0, "escalate": false, "actions": ["clear_lock"]}))
            .collect();
        let body: String = sup.iter().map(|l| serde_json::to_string(l).unwrap()).collect::<Vec<_>>().join("\n");
        std::fs::write(dir.join("supervisor.jsonl"), body).unwrap();
        std::fs::write(dir.join("lock"), "2147483646\ntok").unwrap(); // stale lock present
        let d = diagnose(&repo);
        assert_eq!(d["category"], "stale_lock");
        assert_eq!(d["auto_safe"], false); // anti-thrash demoted
        assert!(d["evidence"].as_str().unwrap().contains("escalating instead of looping"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- _finish: rung + log-once dedupe ----------------
    #[test]
    fn finish_rung_assignment_and_dedupe() {
        let (dir, repo) = tmp_repo("finish");
        // gate_red_streak -> rung 1
        let d = json!({"category": "gate_red_streak", "evidence": "x"});
        let out = finish(&repo, &d, vec!["solomon_fix_session".into()], false, "launched");
        assert_eq!(out["category"], "gate_red_streak");
        assert_eq!(out["escalate"], false);

        // pure escalation (no actions) -> rung 2 + writes escalation.json + supervisor line
        let d2 = json!({"category": "unknown_error", "evidence": "boom"});
        let out2 = finish(&repo, &d2, vec![], true, "escalated — operator action required");
        assert_eq!(out2["escalate"], true);
        assert_eq!(out2["ok"], false);
        assert!(dir.join("escalation.json").exists());

        // a SECOND identical pure escalation is deduped -> escalate=false, escalate_deduped=true.
        let out3 = finish(&repo, &d2, vec![], true, "escalated — operator action required");
        assert_eq!(out3["escalate"], false);
        assert_eq!(out3["escalate_deduped"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- escalation read/write/clear ----------------
    #[test]
    fn escalation_roundtrip_and_clear() {
        let (dir, repo) = tmp_repo("esc");
        // missing -> None
        assert_eq!(read_escalation(&repo), None);
        let d = json!({"category": "no_key", "evidence": "OPENROUTER_API_KEY not set"});
        write_escalation(&repo, &d);
        let read = read_escalation(&repo).unwrap();
        assert_eq!(read["category"], "no_key");
        assert_eq!(read["evidence"], "OPENROUTER_API_KEY not set");
        assert!(read["suggested_manual_steps"].is_array());
        assert!(read["ts"].as_str().is_some());
        // clear removes it -> read returns None
        assert_eq!(clear_escalation(&repo), json!({"ok": true}));
        assert_eq!(read_escalation(&repo), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn escalation_no_runtime_dir() {
        assert_eq!(read_escalation(&json!({})), None);
        assert_eq!(clear_escalation(&json!({})), json!({"ok": false, "error": "repo has no 'path'"}));
    }

    // ---------------- recover: healthy clears escalation ----------------
    #[test]
    fn recover_ok_clears_escalation() {
        let (dir, repo) = tmp_repo("recok");
        write_escalation(&repo, &json!({"category": "no_key", "evidence": "x"}));
        assert!(dir.join("escalation.json").exists());
        let out = recover(&repo, false, true, true);
        assert_eq!(out["category"], "ok");
        assert_eq!(out["message"], "healthy");
        assert!(!dir.join("escalation.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: healthy transition breaks the log-once dedupe ----------------
    // After a fix lands (cat==ok), a supervisor.jsonl "ok" record is stamped so the finish()
    // dedupe (which compares against the last record) does NOT suppress re-escalation when the
    // SAME problem recurs. Without the "ok" record, the stale pre-fix escalation would still be
    // the last record and the recurrence would be silently deduped.
    #[test]
    fn recover_ok_breaks_dedupe_so_recurrence_re_escalates() {
        let (dir, repo) = tmp_repo("dedupe_recur");

        // 1. Escalate a no_key problem.
        write_hb(&dir, &json!({"status": "error", "last_summary": "OPENROUTER_API_KEY not set"}));
        let out1 = recover(&repo, false, true, true);
        assert_eq!(out1["category"], "no_key");
        assert_eq!(out1["escalate"], true);
        assert!(dir.join("escalation.json").exists());

        // 2. Fix lands: heartbeat clears -> healthy. An "ok" supervisor record is stamped.
        write_hb(&dir, &json!({"status": "idle"}));
        let out2 = recover(&repo, false, true, true);
        assert_eq!(out2["category"], "ok");
        assert!(!dir.join("escalation.json").exists());

        // 3. The same problem recurs (key removed again). MUST re-escalate, NOT be deduped.
        write_hb(&dir, &json!({"status": "error", "last_summary": "OPENROUTER_API_KEY not set"}));
        let out3 = recover(&repo, false, true, true);
        assert_eq!(out3["category"], "no_key");
        assert_eq!(out3["escalate"], true);
        assert!(!out3.get("escalate_deduped").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(false));
        assert!(dir.join("escalation.json").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: healthy does not flood supervisor.jsonl ----------------
    #[test]
    fn recover_ok_writes_one_ok_record_per_transition() {
        let (dir, repo) = tmp_repo("ok_no_flood");
        // Pre-seed a non-ok supervisor record so the first healthy recover writes an "ok" record.
        std::fs::write(
            dir.join("supervisor.jsonl"),
            "{\"category\":\"no_key\",\"escalate\":true,\"rung\":2}\n",
        )
        .unwrap();

        let _out1 = recover(&repo, false, true, true); // healthy -> stamps "ok"
        let _out2 = recover(&repo, false, true, true); // still healthy -> no new record

        let sup = std::fs::read_to_string(dir.join("supervisor.jsonl")).unwrap();
        let ok_count = sup.lines().filter(|l| l.contains("\"category\":\"ok\"")).count();
        assert_eq!(ok_count, 1, "only one ok record per transition, not one per poll");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: escalate-only category ----------------
    #[test]
    fn recover_no_key_escalates() {
        let (dir, repo) = tmp_repo("recnokey");
        write_hb(&dir, &json!({"status": "error", "last_summary": "OPENROUTER_API_KEY not set"}));
        let out = recover(&repo, false, true, true);
        assert_eq!(out["category"], "no_key");
        assert_eq!(out["escalate"], true);
        assert_eq!(out["ok"], false);
        assert_eq!(out["actions_taken"], json!([]));
        assert!(dir.join("escalation.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: stale_lock RUNG-0 clear ----------------
    #[test]
    fn recover_stale_lock_clears() {
        let (dir, repo) = tmp_repo("recstale");
        std::fs::write(dir.join("lock"), "2147483646\ntok").unwrap(); // dead pid
        let out = recover(&repo, false, true, true);
        assert_eq!(out["category"], "stale_lock");
        assert_eq!(out["escalate"], false);
        assert_eq!(out["actions_taken"], json!(["clear_lock"]));
        assert!(!dir.join("lock").exists()); // cleared
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: stop_lingering clean exit clears stop ----------------
    #[test]
    fn recover_stop_lingering_clears_stop() {
        let (dir, repo) = tmp_repo("recstop");
        write_hb(&dir, &json!({"status": "stopped"}));
        std::fs::write(dir.join("stop"), "").unwrap();
        let out = recover(&repo, false, true, true);
        assert_eq!(out["category"], "stop_lingering");
        assert_eq!(out["actions_taken"], json!(["clear_stop"]));
        assert!(!dir.join("stop").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- recover: gate_red_streak gated behind allow_pi ----------------
    #[test]
    fn recover_gate_red_streak_requires_allow_pi() {
        let (dir, repo) = tmp_repo("recgate");
        write_hist(&dir, &[
            json!({"status": "reverted"}),
            json!({"status": "reverted"}),
            json!({"status": "error"}),
        ]);
        // allow_pi=false -> escalate with the tick-Allow-AI-fix message.
        let out = recover(&repo, false, true, true);
        assert_eq!(out["category"], "gate_red_streak");
        assert_eq!(out["escalate"], true);
        assert_eq!(
            out["message"],
            "persistent gate failure — tick 'Allow AI fix' to run a Solomon fix-session"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---------------- suggested_steps ----------------
    #[test]
    fn suggested_steps_per_category() {
        let repo = json!({"name": "x", "path": "C:/p/x", "pr_target_branch": "main"});
        assert_eq!(suggested_steps(&repo, "no_key"),
                   vec!["Open Solomon → Settings and add the provider's API key, then retry"]);
        assert_eq!(suggested_steps(&repo, "gh_not_ready"),
                   vec!["gh auth login   # authenticate, then retry"]);
        let rv = suggested_steps(&repo, "revert_failed");
        assert_eq!(rv[0], "cd \"C:/p/x\"");
        assert_eq!(rv[1], "git checkout --force main");
        // default fall-through
        assert_eq!(suggested_steps(&repo, "stale_lock"), vec!["cd \"C:/p/x\"".to_string(), "git status".into()]);
    }

    // ---------------- _now format ----------------
    #[test]
    fn now_format_is_utc_z() {
        let s = now();
        assert_eq!(s.len(), 20); // YYYY-MM-DDTHH:MM:SSZ
        assert!(s.ends_with('Z'));
        assert!(NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%SZ").is_ok());
    }

    #[test]
    fn auto_safe_constant_matches_source() {
        assert_eq!(AUTO_SAFE, &["stale_lock", "stop_lingering", "dirty_tree", "stuck"]);
    }
}
