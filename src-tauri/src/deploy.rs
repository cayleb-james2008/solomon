//! Managed-app redeploy: Solomon rebuilds + relaunches a repo's LIVE app binary when a committed
//! fix has merged but never reached the running product — the "deploy gap" deadlock (asmodeus-class).
//!
//! `ship.rs` is pure git/PR: the improver merges code but NEVER rebuilds/redeploys the managed app,
//! so a committed crash-loop fix strands in source and the deployed binary stays behind HEAD
//! forever. Solomon already self-redeploys its OWN orchestrator (`redeploy::maybe_self_redeploy`);
//! this is the equivalent for the APPS it manages — generic, config-driven, safety-hardened.
//!
//! HARD SAFETY CONTRACT:
//!   - A repo is auto-deployed ONLY IFF it carries a `live_deploy` config (opt-in per repo). A repo
//!     WITHOUT `live_deploy` is NEVER auto-deployed — money-out / live-money stays human-gated
//!     (asmodeus explicitly: no `live_deploy` -> never touched here).
//!   - A deploy fires ONLY when the deployed binary is STALE (a probe detail says "deploy gap") AND
//!     the app is DOWN (a process probe says the app is NOT running) — rebuild+relaunch then loses
//!     nothing. Never when the app is UP and current (do not interrupt a healthy running app).
//!   - Reuses `redeploy::drain_safe` — never deploy while any lane is mid-ship or a live-money lane
//!     has an open trade.
//!   - Per-repo cooldown + at most ONE managed-app deploy per watchdog sweep across ALL repos (a
//!     cargo build is heavy — 6-at-once melted the disk on 2026-07-02). Together they prevent any
//!     rebuild storm.
//!
//! The trigger predicate `should_redeploy` is PURE (no IO) so the safety contract is unit-tested.
//! The rebuild/relaunch is side-effectful; any error aborts and the cooldown gates the next attempt
//! (never loops). Every deploy pages the operator LOUDLY — a real state change they must see.

#![allow(dead_code)]

use crate::control::{paths, proc, registry};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default min seconds between auto-deploys for a repo when `live_deploy.cooldown_s` is absent.
const DEFAULT_COOLDOWN_S: i64 = 1800;

/// Max managed-app deploys per watchdog sweep across ALL repos. Mirrors watchdog's
/// MAX_LANE_RESTARTS_PER_SWEEP intent — a cargo build is heavy; 6-at-once melted the disk on
/// 2026-07-02. ONE per sweep + the per-repo cooldown together bound the rebuild rate.
const MAX_DEPLOYS_PER_SWEEP: usize = 1;

/// How long to wait after relaunch before confirming the process came up (seconds).
const LAUNCH_CONFIRM_S: u64 = 5;

// --------------------------------------------------------------------------- //
// TRIGGER — pure, the load-bearing safety predicate
// --------------------------------------------------------------------------- //

/// The redeploy predicate: auto-deploy a repo IFF ALL of:
///   - it has a `live_deploy` config (`repo_cfg` object contains a truthy `live_deploy`), AND
///   - its ops probes show a DEPLOY GAP (the deployed binary is stale — a probe detail contains
///     "deploy gap") AND the PROCESS is red (the app's process probe says "NOT running") — so the
///     deployed binary is stale AND the app is down, so rebuild+relaunch loses nothing, AND
///   - the cooldown has elapsed (`last_deploy_age_s` is None or > the repo's cooldown), AND
///   - drain is safe (`drain_safe` — no lane mid-ship, no live-money lane with an open trade).
///
/// Never fires when the app is UP and current (deploy_gap && app_down both required). Pure — the
/// caller collects the ops probes + drain state + cooldown age (IO) and passes them in.
pub fn should_redeploy(
    repo_cfg: &Value,
    deploy_gap: bool,
    app_down: bool,
    last_deploy_age_s: Option<i64>,
    drain_ok: bool,
) -> bool {
    if !has_live_deploy(repo_cfg) {
        return false; // no live_deploy config -> NEVER auto-deploy (human-gated)
    }
    if !(deploy_gap && app_down) {
        return false; // both required: stale binary AND app down (never interrupt a healthy app)
    }
    if !drain_ok {
        return false; // never deploy while a lane is mid-ship / a live-money lane has an open trade
    }
    match last_deploy_age_s {
        Some(age) => age > cooldown_s(repo_cfg),
        None => true, // no prior deploy -> eligible
    }
}

/// True iff `repo_cfg` carries a truthy `live_deploy` object — the opt-in for auto-deploy.
pub fn has_live_deploy(repo_cfg: &Value) -> bool {
    repo_cfg
        .get("live_deploy")
        .map(json_truthy)
        .unwrap_or(false)
}

/// The repo's per-deploy cooldown (seconds) — `live_deploy.cooldown_s`, default DEFAULT_COOLDOWN_S.
pub fn cooldown_s(repo_cfg: &Value) -> i64 {
    repo_cfg
        .get("live_deploy")
        .and_then(|d| d.get("cooldown_s"))
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_COOLDOWN_S)
}

// --------------------------------------------------------------------------- //
// OPS-STATUS READERS — extract the two conditions from the sweep's rollup
// --------------------------------------------------------------------------- //

/// True iff any of `project`'s rollup `reasons` reports a DEPLOY GAP — the binary_current probe's
/// detail `"binary <sha> != HEAD <head> (deploy gap)"`. Pure — takes the per-project rollup Value
/// (from `ops_status.json`'s `projects.<name>`).
pub fn project_deploy_gap(project: &Value) -> bool {
    reason_contains(project, "deploy gap")
}

/// True iff any of `project`'s rollup `reasons` reports the app process is DOWN — the process
/// probe's detail `"<App>.exe NOT running"`. Pure.
pub fn project_app_down(project: &Value) -> bool {
    reason_contains(project, "NOT running")
}

/// True iff any rollup `reasons` entry contains `needle`. The reasons array carries each non-green
/// probe as `"{id}={status} ({detail})"`, so the probe detail is embedded there.
fn reason_contains(project: &Value, needle: &str) -> bool {
    project
        .get("reasons")
        .and_then(Value::as_array)
        .map(|rs| {
            rs.iter()
                .filter_map(Value::as_str)
                .any(|r| r.contains(needle))
        })
        .unwrap_or(false)
}

// --------------------------------------------------------------------------- //
// COOLDOWN MARKER
// --------------------------------------------------------------------------- //

/// The per-repo last-deploy marker path (`runtime/<name>/_last_deploy`).
fn last_deploy_path(name: &str) -> PathBuf {
    paths::here().join("runtime").join(name).join("_last_deploy")
}

/// Age (seconds) since the last managed-app deploy for `name`, or None when there was none / the
/// marker is unreadable/unparseable (treated as "no prior deploy" -> eligible).
fn last_deploy_age_s(name: &str) -> Option<i64> {
    let raw = std::fs::read_to_string(last_deploy_path(name)).ok()?;
    let last = chrono::NaiveDateTime::parse_from_str(raw.trim(), "%Y-%m-%dT%H:%M:%SZ")
        .ok()?
        .and_utc();
    Some((chrono::Utc::now() - last).num_seconds())
}

/// Stamp the per-repo last-deploy marker (cooldown sentinel). Best-effort (OSError -> pass).
fn stamp_last_deploy(name: &str) {
    let _ = (|| -> std::io::Result<()> {
        let p = last_deploy_path(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        std::fs::write(&p, ts)
    })();
}

// --------------------------------------------------------------------------- //
// EXECUTE — rebuild + relaunch the managed app
// --------------------------------------------------------------------------- //

/// The rebuild/launch argv for a repo's `live_deploy`, or None when the field is absent/malformed.
fn live_deploy_argv(repo_cfg: &Value, field: &str) -> Option<Vec<String>> {
    let arr = repo_cfg
        .get("live_deploy")
        .and_then(|d| d.get(field))
        .and_then(Value::as_array)?;
    let argv: Vec<String> = arr
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Validate a launch argv BEFORE spending minutes on a rebuild. Fails loudly if any element carries
/// an ASCII control character (exactly how the asmodeus `scripts...keepalive` corruption
/// shipped — `\a` mangled into a JSON BEL escape) or names a script that does not exist on disk.
/// An element is treated as a script path only if it ends in a known launcher extension
/// (`.ps1`/`.bat`/`.cmd`/`.exe`, case-insensitive); such a path is resolved relative to `cwd` when
/// not absolute. Non-path argv (flags, `-Command`, interpreter names) is intentionally skipped so a
/// legitimate `pwsh -Command ...` launch is not rejected. Pure — safe to unit-test.
fn launch_argv_sane(argv: &[String], cwd: &Path) -> Result<(), String> {
    for a in argv {
        if a.chars().any(|c| c.is_ascii_control()) {
            return Err(format!("launch argv contains a control character: {a:?}"));
        }
        let lower = a.to_ascii_lowercase();
        let is_script = lower.ends_with(".ps1")
            || lower.ends_with(".bat")
            || lower.ends_with(".cmd")
            || lower.ends_with(".exe");
        if is_script {
            let p = Path::new(a);
            let resolved = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
            if !resolved.exists() {
                return Err(format!("launch script not found: {}", resolved.display()));
            }
        }
    }
    Ok(())
}

/// The repo's short HEAD sha (`git -C <path> rev-parse --short HEAD`), or "?" on any failure — used
/// only for the operator page (from-SHA -> HEAD-SHA), never a gate.
fn short_head(repo_path: &str) -> String {
    if repo_path.is_empty() || !Path::new(repo_path).is_dir() {
        return "?".to_string();
    }
    match proc::run(
        &["git", "-C", repo_path, "rev-parse", "--short", "HEAD"],
        None,
        Some(Duration::from_secs(30)),
    ) {
        Ok(r) if r.ok() => {
            let s = r.stdout.trim();
            if s.is_empty() { "?".to_string() } else { s.to_string() }
        }
        _ => "?".to_string(),
    }
}

/// Run one managed-app deploy for `repo_cfg`: rebuild the current HEAD in the repo cwd; on exit 0,
/// relaunch the app; briefly confirm the process came up. Stamps the cooldown marker regardless of
/// outcome (a failed deploy must not retry until the cooldown elapses — never loop). Pages the
/// operator LOUDLY (repo, from-SHA, success/failure). Returns Ok(()) on a successful rebuild+launch,
/// Err(msg) on any failure (already logged/paged).
fn maybe_redeploy_managed_app(repo_cfg: &Value) -> Result<(), String> {
    let name = paths::repo_name(repo_cfg);
    let path = paths::repo_path(repo_cfg);
    let cwd = PathBuf::from(&path);
    let head = short_head(&path);

    // Stamp the cooldown FIRST so a build that fails / hangs cannot re-fire before the cooldown.
    stamp_last_deploy(&name);

    let rebuild = live_deploy_argv(repo_cfg, "rebuild")
        .ok_or_else(|| format!("{name}: live_deploy.rebuild missing/empty"))?;
    let launch = live_deploy_argv(repo_cfg, "launch")
        .ok_or_else(|| format!("{name}: live_deploy.launch missing/empty"))?;

    // Validate the launch path BEFORE the ~30-minute rebuild: a mangled/missing launch script (the
    // asmodeus BEL-corruption bug) otherwise wastes a full build, fails the launch, pages, and cools
    // down for an hour — leaving the live app down. Fail loudly and page now instead.
    if let Err(e) = launch_argv_sane(&launch, &cwd) {
        let msg = format!("{name}: {e}");
        page(&name, &head, false, &msg);
        return Err(msg);
    }

    // Rebuild the current HEAD in the repo cwd. Long timeout — a clean release build is minutes; a
    // timeout aborts the child and returns Err (no relaunch).
    let r = proc::run(&rebuild, Some(&cwd), Some(Duration::from_secs(60 * 30)))
        .map_err(|e| format!("{name}: rebuild spawn failed: {e}"))?;
    if !r.ok() {
        let tail: String = r.stderr.trim().chars().take(300).collect();
        let msg = format!("{name}: rebuild FAILED (code {}) at {head}: {tail}", r.code);
        page(&name, &head, false, &format!("rebuild exit {}", r.code));
        return Err(msg);
    }

    // Relaunch the app. A launch failure is a real, paged failure (the fix built but did not deploy).
    let lr = proc::run(&launch, Some(&cwd), Some(Duration::from_secs(60)))
        .map_err(|e| format!("{name}: launch spawn failed: {e}"))?;
    if !lr.ok() {
        let msg = format!("{name}: relaunch FAILED (code {}) at {head}", lr.code);
        page(&name, &head, false, &format!("relaunch exit {}", lr.code));
        return Err(msg);
    }

    // Briefly confirm the process came up — the app_down condition should clear on the next probe.
    std::thread::sleep(Duration::from_secs(LAUNCH_CONFIRM_S));
    let up = app_process_up(repo_cfg);
    page(&name, &head, true, if up { "process confirmed up" } else { "launched (process not yet visible)" });
    Ok(())
}

/// Best-effort confirmation that the app's process is up after relaunch, reusing the ops process
/// probe. Reads the repo's `ops.json` process probe (if any) and evaluates it; a missing probe /
/// non-green result returns false (the page reflects "not yet visible", not a hard failure — the
/// next ops sweep re-observes the real state).
fn app_process_up(repo_cfg: &Value) -> bool {
    let name = paths::repo_name(repo_cfg);
    let path = paths::repo_path(repo_cfg);
    // Find the project's process probe in ops.json and evaluate it directly.
    for entry in crate::ops::registry::load_ops() {
        if crate::ops::registry::project_name(&entry) != name {
            continue;
        }
        for cfg in crate::ops::registry::project_probes(&entry) {
            if cfg.get("kind").and_then(Value::as_str) == Some("process") {
                let raw = crate::ops::probe::evaluate(&cfg, &path);
                return raw.status == crate::ops::Status::Green;
            }
        }
    }
    false
}

/// Page the operator LOUDLY about a managed-app deploy (a real state change). Success -> a recovered
/// notice; failure -> a red notice. Best-effort (notify::send never fails a sweep).
fn page(name: &str, from_sha: &str, success: bool, detail: &str) {
    let body = format!("{name}: {from_sha} -> HEAD ({detail})");
    let notice = if success {
        crate::notify::Notice::recovered(format!("Solomon: {name} redeployed"), body)
    } else {
        crate::notify::Notice::red(format!("Solomon: {name} redeploy FAILED"), body)
    };
    let _ = crate::notify::send(&notice);
}

// --------------------------------------------------------------------------- //
// ORCHESTRATOR — the watchdog-sweep entry
// --------------------------------------------------------------------------- //

/// The managed-app redeploy check, wired into the watchdog sweep after the ops graft. Reads the
/// fresh `ops_status` payload the sweep already computed (`projects.<name>` rollups), and for each
/// repo with a `live_deploy` config whose app is in a deploy-gap+down state (and the drain window is
/// safe + the cooldown has elapsed), rebuilds + relaunches — at most ONE deploy per sweep across ALL
/// repos.
///
/// catch_unwind is the caller's responsibility (watchdog wraps this) so a deploy failure can never
/// abort crash-recovery. A build/launch failure does NOT loop — the cooldown gates the next attempt.
pub fn maybe_redeploy_managed_apps(ops_status: &Value) {
    let projects = match ops_status.get("projects").and_then(Value::as_object) {
        Some(p) => p,
        None => return, // no ops rollup this sweep — nothing to do
    };
    // Shared drain window: computed once (the state can't change mid-sweep in a meaningful way, and
    // a single deploy per sweep re-checks it via should_redeploy's drain_ok).
    let drain_ok = crate::redeploy::drain_window_now();

    let mut deploys_left = MAX_DEPLOYS_PER_SWEEP;
    for repo_cfg in registry::load_repos() {
        if deploys_left == 0 {
            break; // per-sweep cap reached — the rest defer to a later sweep (cooldown-safe)
        }
        if !has_live_deploy(&repo_cfg) {
            continue; // opt-in only — a repo without live_deploy is NEVER auto-deployed
        }
        let name = paths::repo_name(&repo_cfg);
        let project = match projects.get(&name) {
            Some(p) => p,
            None => continue, // no ops rollup for this repo this sweep
        };
        let deploy_gap = project_deploy_gap(project);
        let app_down = project_app_down(project);
        let age = last_deploy_age_s(&name);
        if !should_redeploy(&repo_cfg, deploy_gap, app_down, age, drain_ok) {
            continue;
        }
        // Fire the one deploy this repo is eligible for; consume the per-sweep budget whether it
        // succeeds or fails (a heavy build ran either way — the cooldown gates the next attempt).
        deploys_left -= 1;
        let _ = maybe_redeploy_managed_app(&repo_cfg);
    }
}

// --------------------------------------------------------------------------- //
// helpers
// --------------------------------------------------------------------------- //

/// Python truthiness for a JSON value (mirrors redeploy::json_truthy / watchdog::json_truthy).
fn json_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // A repo cfg shaped like sover's live_deploy entry.
    fn live_repo() -> Value {
        json!({
            "name": "sover",
            "live_deploy": {
                "rebuild": ["cargo", "build", "--release"],
                "launch": ["cmd", "/c", "launch_ggg.bat"],
                "cooldown_s": 1800
            }
        })
    }

    // A repo cfg shaped like asmodeus (live-money, NO live_deploy) — must NEVER deploy.
    fn asmodeus_shaped() -> Value {
        json!({"name": "asmodeus", "live_money": true, "gate": "cargo test --workspace"})
    }

    // -------- should_redeploy: the full-condition decision table --------
    #[test]
    fn should_redeploy_true_only_on_full_condition() {
        let repo = live_repo();
        // all conditions met (no prior deploy) -> deploy
        assert!(should_redeploy(&repo, true, true, None, true), "full condition -> deploy");
        // cooldown elapsed (age > 1800) -> deploy
        assert!(should_redeploy(&repo, true, true, Some(1801), true));
    }

    #[test]
    fn should_redeploy_repo_without_live_deploy_never() {
        // asmodeus-shaped: live-money, NO live_deploy — NEVER auto-deploy even with every other
        // condition screaming deploy. This is the money-out human-gate invariant.
        let repo = asmodeus_shaped();
        assert!(
            !should_redeploy(&repo, true, true, None, true),
            "a repo without live_deploy must NEVER deploy"
        );
        // and an empty cfg is likewise never deployed
        assert!(!should_redeploy(&json!({}), true, true, None, true));
    }

    #[test]
    fn should_redeploy_cooldown_blocks() {
        let repo = live_repo();
        // within cooldown (age <= 1800) -> blocked
        assert!(!should_redeploy(&repo, true, true, Some(1800), true), "== cooldown -> blocked");
        assert!(!should_redeploy(&repo, true, true, Some(5), true), "recent deploy -> blocked");
        // default cooldown (no cooldown_s) is 1800s
        let repo_default = json!({
            "name": "x",
            "live_deploy": {"rebuild": ["a"], "launch": ["b"]}
        });
        assert!(!should_redeploy(&repo_default, true, true, Some(1800), true));
        assert!(should_redeploy(&repo_default, true, true, Some(1801), true));
    }

    #[test]
    fn should_redeploy_drain_unsafe_blocks() {
        let repo = live_repo();
        assert!(
            !should_redeploy(&repo, true, true, None, false),
            "drain unsafe (mid-ship / open live trade) -> never deploy"
        );
    }

    #[test]
    fn should_redeploy_app_up_or_current_false() {
        let repo = live_repo();
        // app UP (not down) but deploy gap -> do NOT interrupt a healthy running app
        assert!(!should_redeploy(&repo, true, false, None, true), "app up -> no deploy");
        // app down but binary CURRENT (no deploy gap) -> nothing to deploy
        assert!(!should_redeploy(&repo, false, true, None, true), "binary current -> no deploy");
        // neither
        assert!(!should_redeploy(&repo, false, false, None, true));
    }

    // -------- cooldown_s + has_live_deploy --------
    #[test]
    fn cooldown_and_has_live_deploy() {
        assert!(has_live_deploy(&live_repo()));
        assert!(!has_live_deploy(&asmodeus_shaped()));
        assert!(!has_live_deploy(&json!({})));
        // explicit cooldown honored; default when absent
        assert_eq!(cooldown_s(&live_repo()), 1800);
        assert_eq!(
            cooldown_s(&json!({"live_deploy": {"cooldown_s": 600}})),
            600
        );
        assert_eq!(cooldown_s(&json!({"live_deploy": {}})), DEFAULT_COOLDOWN_S);
        assert_eq!(cooldown_s(&json!({})), DEFAULT_COOLDOWN_S);
    }

    // -------- ops-status readers: deploy-gap + app-down detection from the rollup reasons --------
    #[test]
    fn project_readers_detect_gap_and_down() {
        // a rollup carrying both the binary_current deploy-gap and the process NOT-running reasons.
        let project = json!({
            "status": "red",
            "reasons": [
                "binary_current=yellow (binary abc123 != HEAD def456 (deploy gap))",
                "process=red (Sover.exe NOT running)"
            ]
        });
        assert!(project_deploy_gap(&project), "deploy gap detected from reasons");
        assert!(project_app_down(&project), "app-down detected from reasons");

        // a healthy rollup -> neither
        let healthy = json!({"status": "green", "reasons": []});
        assert!(!project_deploy_gap(&healthy));
        assert!(!project_app_down(&healthy));

        // deploy gap but app UP (no process reason) -> gap true, down false
        let gap_only = json!({
            "status": "yellow",
            "reasons": ["binary_current=yellow (binary abc != HEAD def (deploy gap))"]
        });
        assert!(project_deploy_gap(&gap_only));
        assert!(!project_app_down(&gap_only));

        // missing reasons array -> both false, never a panic
        assert!(!project_deploy_gap(&json!({"status": "red"})));
        assert!(!project_app_down(&json!({"status": "red"})));
    }

    // -------- per-sweep cap: at most ONE managed-app deploy across all repos --------
    // The cap is MAX_DEPLOYS_PER_SWEEP; asserting the constant here documents the disk-meltdown
    // guard (a cargo build is heavy — 6-at-once melted the disk on 2026-07-02). The orchestrator
    // loop decrements deploys_left and breaks at 0, so no more than this many rebuilds ever stack in
    // one sweep regardless of how many repos are gap+down.
    #[test]
    fn per_sweep_cap_is_one() {
        assert_eq!(MAX_DEPLOYS_PER_SWEEP, 1, "one heavy build per sweep — the meltdown guard");
    }

    // -------- live_deploy_argv: parse rebuild/launch argv --------
    #[test]
    fn live_deploy_argv_parses_and_rejects_empty() {
        let repo = live_repo();
        assert_eq!(
            live_deploy_argv(&repo, "rebuild"),
            Some(vec!["cargo".to_string(), "build".to_string(), "--release".to_string()])
        );
        assert_eq!(
            live_deploy_argv(&repo, "launch"),
            Some(vec!["cmd".to_string(), "/c".to_string(), "launch_ggg.bat".to_string()])
        );
        // missing field -> None
        assert_eq!(live_deploy_argv(&json!({"live_deploy": {}}), "rebuild"), None);
        // empty argv -> None (never spawn an empty command)
        assert_eq!(live_deploy_argv(&json!({"live_deploy": {"rebuild": []}}), "rebuild"), None);
        // no live_deploy at all -> None
        assert_eq!(live_deploy_argv(&json!({}), "rebuild"), None);
    }

    // -------- launch_argv_sane: reject the corruption class BEFORE a wasteful rebuild --------
    #[test]
    fn launch_argv_sane_rejects_control_chars() {
        // Exactly the asmodeus bug shape: a BEL byte smuggled into the script path element.
        let argv = vec![
            "powershell".to_string(),
            "-File".to_string(),
            "scripts\u{0007}smodeus_keepalive.ps1".to_string(),
        ];
        let err = launch_argv_sane(&argv, Path::new(".")).unwrap_err();
        assert!(err.contains("control character"), "got: {err}");
    }

    #[test]
    fn launch_argv_sane_rejects_missing_script() {
        let dir = std::env::temp_dir().join("deploy_launch_missing_test");
        let _ = std::fs::create_dir_all(&dir);
        let argv = vec![
            "powershell".to_string(),
            "-File".to_string(),
            "scripts\\does_not_exist.ps1".to_string(),
        ];
        let err = launch_argv_sane(&argv, &dir).unwrap_err();
        assert!(err.contains("launch script not found"), "got: {err}");
        assert!(err.contains("does_not_exist.ps1"), "err should name the path: {err}");
    }

    #[test]
    fn launch_argv_sane_ok_when_script_exists() {
        let dir = std::env::temp_dir().join("deploy_launch_ok_test");
        let _ = std::fs::create_dir_all(&dir);
        let script = dir.join("start.ps1");
        std::fs::write(&script, "# launcher").unwrap();
        // Flags/interpreter names are skipped; the existing .ps1 (relative to cwd) resolves -> Ok.
        let argv = vec![
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-File".to_string(),
            "start.ps1".to_string(),
        ];
        assert!(launch_argv_sane(&argv, &dir).is_ok());
        let _ = std::fs::remove_file(&script);
    }
}
